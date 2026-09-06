// Host module for `polymorph:webrtc-datachannels/connections@0.1.0` under
// the polyengine embedder conventions (contracts/embedder-api.md in the polyengine
// repository — every `contracts/embedder-api.md` citation below names that
// document).
//
// PORT PROVENANCE. This is a faithful translation of this repository's
// browser-first reference host, `jco-impl/webrtc.js` **at commit 65bc15b**
// — the file was retired with the jco legs, so the `jco-impl/webrtc.js:LINE`
// citations below read against that revision
// (`git show 65bc15b:jco-impl/webrtc.js`). It was written against the
// standard W3C `RTCPeerConnection` / `RTCDataChannel` API; the conformance
// suite under `conformance/` asserts the behavioral parity, and this module
// now carries every JS-host row of the matrix. The LOGIC — backend
// resolution, timeout and buffer bounds, the trickle-ICE surface, the
// overflow-close behavior, the receive-via-stream single-use rule — is
// preserved; only the boundary conventions are translated:
//
//   the retired jco host                 | this port
//   -------------------------------------+------------------------------------
//   `throw { tag, val }` (bare payload)   | `throw new ComponentException({ kind, value })`
//   jco `Stream` (`read({count})`)        | `Stream<T>` / `ReadableStream`
//   `jco --map` module wiring             | `webrtcImports()` record fragment
//   module-level setters                  | same setters (see below)
//
//   - thrown bare `{ kind, value }` payloads become `throw new ComponentException(payload)`
//     (contracts/embedder-api.md §"Error model" — "Host import with
//     result<T, E>": throw new ComponentException(payload) for err).
//   - jco `Stream`/`ReadableStream` params/results become the runtime's real
//     `Stream<T>` (consumed, e.g. `send-via-stream`'s guest-provided
//     messages) / `ReadableStream` (produced, e.g. `receive-via-stream`'s
//     result — one of the natural JS producers the conventions accept where
//     a `stream<T>` is expected). Imported from
//     `@polyengine/protocol` (this package's only polyengine dependency, per
//     A22 — host modules must not import `@polyengine/runtime`; pinned in
//     this package's `deno.json` to a caret range), NOT reimplemented
//     locally: `ComponentException` is a plain branded
//     class with no `Store` involvement, so a local clone would produce a
//     second class identity and every `throw` from this port would fail
//     the `isComponentException` brand check at a real component boundary —
//     silently becoming an unbranded-throw trap instead of a guest-visible
//     err. Brand predicates (`isComponentException`, `isStream`), not
//     `instanceof`, are used against these classes throughout this module —
//     `instanceof` is not contract behavior across separately-loaded
//     protocol module copies.
//   - the inbound buffer bound stays a module-level setter
//     (`setMaxInboundBufferBytes`), exactly as in the reference: the WIT
//     does not expose the bound as guest-configurable (it is host policy
//     per the `data-channel` resource's doc comment), so there is no
//     guest-facing shape to convert.

import {
  ComponentException,
  isComponentException,
  isStream,
  type Stream,
  type StreamSource,
} from "@polyengine/protocol";
import type {
  ConfigError,
  ConnectionState,
  DataChannelState,
  IceCandidate,
  IceServer,
  IceTransportPolicy,
  LiftedStreamMessage,
  Message,
  SendViaStreamError,
  SessionDescription,
  StreamMessage,
  WebrtcError,
} from "./types.ts";

// --- isomorphic RTCPeerConnection resolution --------------------------------

/**
 * Whether the backend resolves `createOffer` only once the connection has
 * negotiation material (true of libdatachannel, which derives descriptions
 * from the media/data sections that exist); see `PeerConnection.createOffer`.
 * Ported verbatim from the reference (jco-impl/webrtc.js:28).
 */
let offerNeedsChannel = false;

// deno-lint-ignore no-explicit-any
type RTCPeerConnectionCtor = new (config?: unknown) => any;

let cachedRTCPeerConnection: RTCPeerConnectionCtor | undefined;

/**
 * Resolve `RTCPeerConnection` isomorphically: a browser (including headless
 * Chromium) exposes the W3C class as a global; under Deno/Node it is
 * provided by `node-datachannel`'s polyfill, imported lazily so the bare
 * specifier never has to resolve in the browser. Ported from
 * jco-impl/webrtc.js:30-43.
 */
async function resolveRTCPeerConnection(): Promise<RTCPeerConnectionCtor> {
  if (cachedRTCPeerConnection) return cachedRTCPeerConnection;
  // deno-lint-ignore no-explicit-any
  const g = globalThis as any;
  if (g.RTCPeerConnection) {
    cachedRTCPeerConnection = g.RTCPeerConnection;
    return cachedRTCPeerConnection!;
  }
  try {
    const { RTCPeerConnection } = await import("node-datachannel/polyfill");
    offerNeedsChannel = true;
    cachedRTCPeerConnection = RTCPeerConnection as RTCPeerConnectionCtor;
    return cachedRTCPeerConnection;
  } catch (cause) {
    throw new Error(
      "no RTCPeerConnection available: not running in a browser and " +
        "node-datachannel could not be loaded (run `deno install " +
        "--allow-scripts=npm:node-datachannel` in polyengine-impl)",
      { cause },
    );
  }
}

/**
 * Test/embedder hook: force the pure-TS werift fallback instead of
 * node-datachannel. werift's `RTCPeerConnection` is also W3C-shaped, so the
 * same resolver slot works; `offerNeedsChannel` is werift-specific behavior
 * (untested — see report) and is left at its default (`false`) for this
 * path since werift's `createOffer` does not require pre-existing
 * negotiation material.
 */
export async function useWerift(): Promise<void> {
  const { RTCPeerConnection } = await import("werift");
  cachedRTCPeerConnection = RTCPeerConnection as unknown as RTCPeerConnectionCtor;
  offerNeedsChannel = false;
}

/** Test hook: reset resolution so the next construction re-resolves. */
export function resetResolvedBackend(): void {
  cachedRTCPeerConnection = undefined;
  offerNeedsChannel = false;
}

// --- tunables (module-level, matching the reference) ------------------------

/** Keep the SCTP send buffer bounded; pause the producer when it fills. */
const MAX_BUFFERED_AMOUNT = 8 * 1024 * 1024;

/** How long `waitConnected` waits before failing with `error.timed-out`. */
const CONNECT_TIMEOUT_MS = 20_000;

/**
 * How long `close()` keeps the underlying connection alive after the close
 * is observed locally, so messages already handed to the transport flush to
 * the wire before teardown discards the SCTP send queue.
 */
const CLOSE_DRAIN_MS = 1_000;

/**
 * How long a REMOTE close/error may wait for already-delivered messages to
 * dispatch before readers observe the end, re-armed by each late arrival
 * (a quiesce window, the receive-side sibling of CLOSE_DRAIN_MS). Chromium
 * can dispatch an RTCDataChannel `close` event ahead of `message` events
 * for data that arrived on the wire first (dispatch inversion, observed
 * under CPU starvation in the conformance browser leg — issue #154):
 * rejecting waiters inside the close task then fails `receive()` with
 * `closed` while the payload sits one task behind, which breaks the WIT
 * contract's drop-implies-close ordering (messages sent before the implied
 * close must still arrive). Under the starvation that triggers the
 * inversion the late message task can itself lag far behind the close
 * task, so the window is generous; a quiet channel just reports `closed`
 * this much later. Local paths are NOT deferred: `close()` latches
 * `#localClosed` synchronously (the WIT contract's "observed locally at
 * once") and overflow signals through `overflowed` immediately.
 */
const REMOTE_END_DRAIN_MS = 1_000;

/**
 * How long a channel `close()` (or drop) waits for payload already handed
 * to the transport to flush before the SCTP reset is issued — the
 * channel-level sibling of CLOSE_DRAIN_MS. The close is still observed
 * locally at once (the WIT contract): sends after `close()` fail `closed`
 * synchronously; only the wire-level teardown waits for the send queue.
 */
const CHANNEL_CLOSE_DRAIN_MS = 1_000;

/** The default bound on buffered inbound payload bytes awaiting `receive`. */
const DEFAULT_MAX_INBOUND_BUFFERED = 8 * 1024 * 1024;

/** The configured inbound buffer bound; channels capture it at creation. */
let maxInboundBuffered = DEFAULT_MAX_INBOUND_BUFFERED;

/**
 * Set the per-channel inbound buffer bound, in payload bytes. Ported from
 * jco-impl/webrtc.js:78-83; not part of the WIT surface (host policy).
 */
export function setMaxInboundBufferBytes(bytes: number): void {
  if (!(Number.isFinite(bytes) && bytes > 0)) {
    throw new Error(`invalid inbound buffer bound ${bytes}: expected a positive byte count`);
  }
  maxInboundBuffered = bytes;
}

/** Reset the inbound buffer bound to its default (test hook). */
export function resetMaxInboundBufferBytes(): void {
  maxInboundBuffered = DEFAULT_MAX_INBOUND_BUFFERED;
}

const utf8 = new TextEncoder();
function utf8ByteLength(text: string): number {
  return utf8.encode(text).byteLength;
}

// --- data-channel-options ----------------------------------------------------

/**
 * The `data-channel-options` resource: a configuration builder for a data
 * channel, mirroring `wasi:http`'s `request-options`.
 */
export class DataChannelOptions {
  #label = "";
  #ordered = true;
  #maxRetransmits: number | undefined = undefined;

  label(): string {
    return this.#label;
  }
  setLabel(label: string): void {
    this.#label = label;
  }

  ordered(): boolean {
    return this.#ordered;
  }
  setOrdered(ordered: boolean): void {
    this.#ordered = ordered;
  }

  maxRetransmits(): number | undefined {
    return this.#maxRetransmits;
  }
  setMaxRetransmits(maxRetransmits: number | undefined): void {
    this.#maxRetransmits = maxRetransmits;
  }

  /** The `RTCDataChannelInit` these options describe. */
  toInit(): { ordered: boolean; maxRetransmits?: number } {
    const init: { ordered: boolean; maxRetransmits?: number } = { ordered: this.#ordered };
    if (this.#maxRetransmits != null) {
      init.maxRetransmits = this.#maxRetransmits;
    }
    return init;
  }
}

// --- peer-connection-config ---------------------------------------------------

/**
 * The `peer-connection-config` resource: a configuration builder with
 * fallible setters (`config-error` per contracts/embedder-api.md's
 * error model), following `wasi:http`'s `request-options` precedent.
 */
export class PeerConnectionConfig {
  #iceServers: IceServer[] = [];
  #policy: IceTransportPolicy = "all";

  iceServers(): IceServer[] {
    return this.#iceServers;
  }

  setIceServers(servers: IceServer[]): void {
    for (const server of servers) {
      if (!server.urls.length) {
        throw new ComponentException<ConfigError>({ kind: "invalid", value: "ice-server has no urls" });
      }
      for (const url of server.urls) {
        if (!/^(stun|stuns|turn|turns):/.test(url)) {
          throw new ComponentException<ConfigError>({
            kind: "invalid",
            value: `ice-server url ${JSON.stringify(url)} has no stun:/stuns:/turn:/turns: scheme`,
          });
        }
      }
    }
    this.#iceServers = servers;
  }

  iceTransportPolicy(): IceTransportPolicy {
    return this.#policy;
  }

  setIceTransportPolicy(policy: IceTransportPolicy): void {
    this.#policy = policy;
  }

  /** The `RTCConfiguration` these options describe. */
  toConfiguration(): Record<string, unknown> {
    const configuration: Record<string, unknown> = { iceTransportPolicy: this.#policy };
    if (this.#iceServers.length) {
      configuration.iceServers = this.#iceServers.map((server) => {
        const entry: Record<string, unknown> = { urls: server.urls };
        if (server.username) entry.username = server.username;
        if (server.credential) entry.credential = server.credential;
        return entry;
      });
    }
    return configuration;
  }
}

// --- inbound message queue (per data-channel) --------------------------------

interface Waiter {
  resolve: (m: Message) => void;
  reject: (e: WebrtcError) => void;
}

/**
 * Per-message inbound queue over a native `RTCDataChannel`, bounded by the
 * configured inbound buffer (payload bytes). Ported from
 * jco-impl/webrtc.js's `incomingQueue` (1013-1091).
 */
function incomingQueue(channel: {
  addEventListener: (type: string, listener: (e: unknown) => void) => void;
}) {
  const limit = maxInboundBuffered;
  const messages: { message: Message; size: number }[] = [];
  const waiters: Waiter[] = [];
  let buffered = 0;
  let overflowed = false;
  let closed = false;

  const push = (message: Message, size: number) => {
    if (endTimer !== undefined && !closed) armEnd(); // late arrival: re-arm the quiesce window
    const waiter = waiters.shift();
    if (waiter) {
      waiter.resolve(message);
    } else {
      buffered += size;
      messages.push({ message, size });
    }
  };

  // deno-lint-ignore no-explicit-any
  channel.addEventListener("message", ({ data }: any) => {
    if (overflowed) return;
    const size = typeof data === "string" ? utf8ByteLength(data) : data.byteLength;
    if (buffered + size > limit && !waiters.length) {
      overflowed = true;
      // deno-lint-ignore no-explicit-any
      (channel as any).close();
      return;
    }
    const message: Message = typeof data === "string"
      ? { kind: "string", value: data }
      : { kind: "binary", value: new Uint8Array(data) };
    push(message, size);
  });

  const endError = (): WebrtcError =>
    overflowed ? { kind: "receive-buffer-overflow" } : { kind: "closed" };
  let endTimer: ReturnType<typeof setTimeout> | undefined;
  const armEnd = () => {
    if (endTimer !== undefined) clearTimeout(endTimer);
    endTimer = setTimeout(() => {
      if (closed) return;
      closed = true;
      while (waiters.length) waiters.shift()!.reject(endError());
    }, REMOTE_END_DRAIN_MS);
  };
  const end = () => {
    // Deferred, not immediate: the `close` event can be dispatched ahead of
    // `message` events whose data arrived first (see REMOTE_END_DRAIN_MS).
    // Within the window, late messages resolve parked waiters in arrival
    // order via `push` (each one re-arming the window); `next()` keeps
    // draining the backlog after `closed` flips, so nothing already
    // delivered is ever dropped.
    if (closed || endTimer !== undefined) return;
    armEnd();
  };
  channel.addEventListener("close", end);
  channel.addEventListener("error", end);

  return {
    next(): Promise<Message> {
      if (messages.length) {
        const { message, size } = messages.shift()!;
        buffered -= size;
        return Promise.resolve(message);
      }
      if (overflowed) return Promise.reject(new ComponentException<WebrtcError>({ kind: "receive-buffer-overflow" }));
      if (closed) return Promise.reject(new ComponentException<WebrtcError>({ kind: "closed" }));
      return new Promise<Message>((resolve, reject) => {
        waiters.push({
          resolve,
          reject: (e) => reject(new ComponentException<WebrtcError>(e)),
        });
      });
    },
    /** Reject every pending waiter with a raw `error` payload (not wrapped). */
    rejectWaiters(error: WebrtcError): void {
      while (waiters.length) waiters.shift()!.reject(error);
    },
    /** Discard the unread backlog; fail pending and future reads `closed`. */
    discard(): void {
      if (endTimer !== undefined) clearTimeout(endTimer);
      messages.length = 0;
      buffered = 0;
      closed = true;
      while (waiters.length) waiters.shift()!.reject({ kind: "closed" });
    },
  };
}

// --- data-channel --------------------------------------------------------------

/**
 * The `data-channel` resource, implemented over a native `RTCDataChannel`.
 * Ported from jco-impl/webrtc.js's `DataChannel` (206-401).
 */
export class DataChannel {
  // deno-lint-ignore no-explicit-any
  #channel: any;
  #incoming: ReturnType<typeof incomingQueue>;
  #streamClaimed = false;
  #localClosed = false;
  #stateTaken = false;
  #statePokes = new Set<() => void>();

  // deno-lint-ignore no-explicit-any
  constructor(channel: any) {
    this.#channel = channel;
    channel.binaryType = "arraybuffer";
    this.#incoming = incomingQueue(channel);
  }

  label(): string {
    return this.#channel.label;
  }

  async send(message: Message): Promise<void> {
    // CONTRACT: the WIT `close` doc requires the close to be "observed
    // locally at once" — calls made after `close()` fail `closed` — but
    // node-datachannel's `RTCDataChannel.close()` transitions `readyState`
    // asynchronously (a `send()` racing right after `close()` can still see
    // `"open"`), unlike a synchronous local latch. Gate on the local flag
    // first so this port's `close()` is observed synchronously regardless of
    // backend timing.
    if (this.#localClosed) throw new ComponentException<WebrtcError>({ kind: "closed" });
    await this.#waitOpen();
    await this.#waitForDrain();
    try {
      this.#channel.send(message.value);
    } catch {
      throw new ComponentException<WebrtcError>({ kind: "closed" });
    }
  }

  async receive(): Promise<Message> {
    if (this.#localClosed) throw new ComponentException<WebrtcError>({ kind: "closed" });
    if (this.#streamClaimed) {
      throw new ComponentException<WebrtcError>({ kind: "receiving-via-stream" });
    }
    return this.#incoming.next();
  }

  /**
   * Send a stream of messages whose payloads are each streamed as bytes.
   * The conventions hand the host a `Stream<T>` handle for a guest-provided
   * `stream<T>` parameter; a plain `ReadableStream`/`AsyncIterable` is also
   * tolerated (contracts/embedder-api.md §"Streams and futures").
   */
  async sendViaStream(
    messages:
      | Stream<LiftedStreamMessage>
      | ReadableStream<LiftedStreamMessage>
      | AsyncIterable<LiftedStreamMessage>,
  ): Promise<void> {
    let sent = 0n;
    try {
      for await (const item of streamItems(messages)) {
        const bytes = await collectByteStream(item.data);
        if (bytes.length !== item.length) {
          throw {
            kind: "other",
            value: `stream-message payload was ${bytes.length} bytes but length declared ${item.length}`,
          } satisfies WebrtcError;
        }
        const message: Message = item.kind === "string"
          ? { kind: "string", value: new TextDecoder().decode(bytes) }
          : { kind: "binary", value: bytes };
        await this.send(message);
        sent += 1n;
      }
    } catch (error) {
      const payload: WebrtcError = isComponentException(error)
        ? (error.payload as WebrtcError)
        : (isWebrtcError(error) ? error : { kind: "closed" });
      throw new ComponentException<SendViaStreamError>({ error: payload, sent });
    }
  }

  /**
   * Take over the channel's inbound messages, delivering each as a
   * `StreamMessage` whose payload is a `StreamSource<u8>`. Once-only per
   * the WIT contract. Returns a plain `ReadableStream`, one of the natural
   * JS producers the conventions accept where a `stream<T>` result is
   * expected — the runtime lowers it; this port never drives a `Store`.
   */
  receiveViaStream(): ReadableStream<StreamMessage> {
    if (this.#localClosed) throw new ComponentException<WebrtcError>({ kind: "closed" });
    if (this.#streamClaimed) {
      throw new ComponentException<WebrtcError>({ kind: "receiving-via-stream" });
    }
    this.#streamClaimed = true;
    const incoming = this.#incoming;
    incoming.rejectWaiters({ kind: "receiving-via-stream" });
    return new ReadableStream<StreamMessage>({
      async pull(controller) {
        let message: Message;
        try {
          message = await incoming.next();
        } catch {
          // The channel closed (or its inbound buffer overflowed): the
          // stream simply ends, per the WIT contract.
          controller.close();
          return;
        }
        const bytes = message.kind === "string"
          ? new TextEncoder().encode(message.value)
          : message.value;
        controller.enqueue({
          kind: message.kind,
          length: bytes.length,
          data: bytesToReadable(bytes) as StreamSource<number>,
        });
      },
    });
  }

  /** Resolve once the channel is open, or reject `closed` if it closes. */
  #waitOpen(): Promise<void> {
    const channel = this.#channel;
    if (channel.readyState === "open") return Promise.resolve();
    if (channel.readyState === "closing" || channel.readyState === "closed") {
      return Promise.reject(new ComponentException<WebrtcError>({ kind: "closed" }));
    }
    return new Promise<void>((resolve, reject) => {
      channel.addEventListener("open", () => resolve(), { once: true });
      channel.addEventListener(
        "close",
        () => reject(new ComponentException<WebrtcError>({ kind: "closed" })),
        { once: true },
      );
      channel.addEventListener(
        "error",
        () => reject(new ComponentException<WebrtcError>({ kind: "closed" })),
        { once: true },
      );
    });
  }

  close(): void {
    if (this.#localClosed) return;
    this.#localClosed = true;
    this.#incoming.discard();
    const channel = this.#channel;
    const finish = () => {
      try {
        channel.close();
      } catch {
        // Already closed.
      }
    };
    // Flush before the reset: Chromium can DISCARD payload still in the
    // SCTP send queue when `RTCDataChannel.close()` is called (observed as
    // issue #154 — an 8-byte probe buffered at close never reached the
    // peer), even though the WHATWG closing procedure says queued data is
    // sent first. The close stays observed locally at once (`#localClosed`
    // above, per the WIT contract); only the transport-level reset waits,
    // bounded, for the queue to drain — the channel-level sibling of
    // CLOSE_DRAIN_MS. `bufferedamountlow` with threshold 0 fires when the
    // queue empties; the timer covers backends without the event (and
    // queues that never drain).
    if (channel.bufferedAmount > 0 && channel.readyState === "open") {
      let done = false;
      const timer = setTimeout(() => {
        if (done) return;
        done = true;
        finish();
      }, CHANNEL_CLOSE_DRAIN_MS);
      const onDrain = () => {
        if (channel.bufferedAmount > 0 || done) return;
        done = true;
        clearTimeout(timer);
        finish();
      };
      try {
        channel.bufferedAmountLowThreshold = 0;
        channel.addEventListener("bufferedamountlow", onDrain);
      } catch {
        // No bufferedamountlow on this backend: the timer bound covers it.
      }
    } else {
      finish();
    }
    for (const poke of this.#statePokes) poke();
  }

  stateChanges(): ReadableStream<DataChannelState> {
    if (this.#stateTaken) {
      return new ReadableStream<DataChannelState>({
        start(c) {
          c.close();
        },
      });
    }
    this.#stateTaken = true;
    return stateStream<DataChannelState>(
      () => this.#channel.readyState,
      (wake) => {
        for (const event of ["open", "closing", "close", "error"]) {
          this.#channel.addEventListener(event, wake);
        }
        this.#statePokes.add(wake);
      },
      (state) => state === "closed",
    );
  }

  [Symbol.dispose](): void {
    try {
      this.close();
    } catch {
      // Already closed.
    }
  }

  /** Apply backpressure so a fast producer cannot overrun the SCTP buffer. */
  #waitForDrain(): Promise<void> {
    const channel = this.#channel;
    if (channel.bufferedAmount <= MAX_BUFFERED_AMOUNT) return Promise.resolve();
    return new Promise<void>((resolve) => {
      channel.bufferedAmountLowThreshold = MAX_BUFFERED_AMOUNT / 2;
      const onLow = () => {
        channel.removeEventListener("bufferedamountlow", onLow);
        resolve();
      };
      channel.addEventListener("bufferedamountlow", onLow);
    });
  }
}

function isWebrtcError(v: unknown): v is WebrtcError {
  return typeof v === "object" && v !== null && typeof (v as { kind?: unknown }).kind === "string";
}

// --- peer-connection ------------------------------------------------------------

/**
 * A single WebRTC peer connection driving the full `RTCPeerConnection`-style
 * signaling surface: offer/answer, trickle ICE, and in-band data channels.
 * Ported from jco-impl/webrtc.js's `PeerConnection` (407-781).
 */
export class PeerConnection {
  // deno-lint-ignore no-explicit-any
  #pc: any;
  #candidates: { stream: ReadableStream<IceCandidate>; end: () => void };
  #channels: { stream: ReadableStream<DataChannel>; end: () => void };
  #everConnected = false;
  #closed = false;
  #failed = false;
  #candidatesTaken = false;
  #channelsTaken = false;
  #closeHooks = new Set<() => void>();
  // deno-lint-ignore no-explicit-any
  #ownedChannels = new Set<any>();
  /**
   * The `DataChannel` wrappers over `#ownedChannels`, latched closed by
   * `close()`: the wrapper's local-close flag is the synchronous gate the
   * WIT contract's "observed locally at once" requires, because a backend
   * may transition the native `readyState` asynchronously (node-datachannel
   * does — see `DataChannel.send`'s gate comment).
   */
  #ownedWrappers = new Set<DataChannel>();
  #stateTaken = false;
  #statePokes = new Set<() => void>();

  /**
   * Construct a peer connection. `config` is taken by ownership, matching
   * the WIT constructor `constructor(config: option<peer-connection-config>)`
   * (contracts/embedder-api.md: "the WIT constructor as the JS constructor",
   * and — "Constructors are synchronous" — this cannot await).
   *
   * CONTRACT: resolving `RTCPeerConnection` isomorphically is necessarily
   * async (the node-datachannel polyfill is a dynamic `import`). This module
   * resolves the backend once via a **top-level await**
   * (`resolveRTCPeerConnection()` at the bottom of this file, mirroring the
   * reference's own top-level await, jco-impl/webrtc.js:45): ES module
   * evaluation does not complete — so no importer's code can run — until a
   * module's own top-level await settles, which means every consumer that
   * imports this file only ever observes it after the backend is already
   * cached. `new PeerConnection(config)` therefore stays synchronous, as the
   * WIT constructor requires. `create()` remains available as an async
   * convenience for test code that deliberately re-resolves the backend
   * mid-run (`resetResolvedBackend()`/`useWerift()`).
   */
  constructor(config?: PeerConnectionConfig) {
    if (!cachedRTCPeerConnection) {
      throw new Error(
        "PeerConnection constructed before the RTCPeerConnection backend " +
          "resolved — this should be unreachable via normal module import " +
          "(top-level await); if resolution failed, the actionable error " +
          "was already thrown/logged at module load. Use `await " +
          "PeerConnection.create(config)` after `resetResolvedBackend()`.",
      );
    }
    const ctor = cachedRTCPeerConnection;
    this.#pc = new ctor(config ? config.toConfiguration() : undefined);

    const latch = () => {
      if (this.#isConnectedNow()) this.#everConnected = true;
      if (!this.#failed && this.#isFailedNow()) {
        this.#failed = true;
        for (const hook of this.#closeHooks) hook();
        this.#closeHooks.clear();
        this.#candidates.end();
        this.#channels.end();
        for (const poke of this.#statePokes) poke();
      }
    };
    this.#pc.addEventListener("connectionstatechange", latch);
    this.#pc.addEventListener("iceconnectionstatechange", latch);

    this.#candidates = eventStream<IceCandidate>((push, end) => {
      const seen = new Set<string>();
      // deno-lint-ignore no-explicit-any
      const pushCandidate = (candidate: string, sdpMid: any, sdpMlineIndex: any) => {
        const normalized = candidate.trim().replace(/^a=/, "");
        if (seen.has(normalized)) return;
        seen.add(normalized);
        push({ candidate: normalized, sdpMid, sdpMlineIndex });
      };
      // deno-lint-ignore no-explicit-any
      this.#pc.addEventListener("icecandidate", ({ candidate }: any) => {
        if (candidate == null || candidate.candidate === "") {
          end();
          return;
        }
        pushCandidate(
          candidate.candidate,
          candidate.sdpMid ?? undefined,
          candidate.sdpMLineIndex ?? undefined,
        );
      });
      this.#pc.addEventListener("icegatheringstatechange", () => {
        if (this.#pc.iceGatheringState !== "complete") return;
        for (const c of sdpCandidates(this.#pc.localDescription?.sdp)) {
          pushCandidate(c.candidate, c.sdpMid, c.sdpMlineIndex);
        }
        end();
      });
    });

    this.#channels = eventStream<DataChannel>((push) => {
      // deno-lint-ignore no-explicit-any
      this.#pc.addEventListener("datachannel", ({ channel }: any) => {
        this.#ownedChannels.add(channel);
        const wrapper = new DataChannel(channel);
        this.#ownedWrappers.add(wrapper);
        push(wrapper);
      });
    });
  }

  /**
   * Async convenience factory: resolve the backend (if not already cached)
   * then construct. Equivalent to `new PeerConnection(config)` once the
   * top-level await above has settled; useful for test code that calls
   * `resetResolvedBackend()`/`useWerift()` mid-run.
   */
  static async create(config?: PeerConnectionConfig): Promise<PeerConnection> {
    await resolveRTCPeerConnection();
    return new PeerConnection(config);
  }

  #requireOpen(): void {
    if (
      this.#closed || this.#failed || this.#isFailedNow() ||
      this.#pc.connectionState === "closed"
    ) {
      throw new ComponentException<WebrtcError>({ kind: "closed" });
    }
  }

  #isConnectedNow(): boolean {
    return (
      this.#pc.connectionState === "connected" ||
      this.#pc.iceConnectionState === "connected" ||
      this.#pc.iceConnectionState === "completed"
    );
  }

  #isFailedNow(): boolean {
    return this.#pc.connectionState === "failed" || this.#pc.iceConnectionState === "failed";
  }

  createDataChannel(options: DataChannelOptions): DataChannel {
    this.#requireOpen();
    try {
      const channel = this.#pc.createDataChannel(options.label(), options.toInit());
      this.#ownedChannels.add(channel);
      const wrapper = new DataChannel(channel);
      this.#ownedWrappers.add(wrapper);
      return wrapper;
    } catch (err) {
      throw new ComponentException<WebrtcError>({ kind: "other", value: String(err) });
    }
  }

  incomingDataChannels(): ReadableStream<DataChannel> {
    if (this.#channelsTaken) {
      return new ReadableStream<DataChannel>({
        start(c) {
          c.close();
        },
      });
    }
    this.#channelsTaken = true;
    return this.#channels.stream;
  }

  async createOffer(): Promise<SessionDescription> {
    this.#requireOpen();
    try {
      if (offerNeedsChannel && this.#ownedChannels.size === 0) {
        this.#pc.createDataChannel("", { negotiated: true, id: 1023 });
      }
      const offer = await this.#pc.createOffer();
      return { kind: "offer", sdp: offer.sdp };
    } catch (err) {
      throw new ComponentException<WebrtcError>({ kind: "other", value: String(err) });
    }
  }

  async createAnswer(): Promise<SessionDescription> {
    this.#requireOpen();
    try {
      const answer = await this.#pc.createAnswer();
      return { kind: "answer", sdp: answer.sdp };
    } catch (err) {
      throw new ComponentException<WebrtcError>({ kind: "other", value: String(err) });
    }
  }

  async setLocalDescription(description: SessionDescription): Promise<void> {
    this.#requireOpen();
    try {
      await this.#pc.setLocalDescription({ type: description.kind, sdp: description.sdp });
    } catch (err) {
      throw new ComponentException<WebrtcError>({ kind: "invalid-signaling", value: String(err) });
    }
  }

  async setRemoteDescription(description: SessionDescription): Promise<void> {
    this.#requireOpen();
    try {
      await this.#pc.setRemoteDescription({ type: description.kind, sdp: description.sdp });
    } catch (err) {
      throw new ComponentException<WebrtcError>({ kind: "invalid-signaling", value: String(err) });
    }
  }

  localIceCandidates(): ReadableStream<IceCandidate> {
    if (this.#candidatesTaken) {
      return new ReadableStream<IceCandidate>({
        start(c) {
          c.close();
        },
      });
    }
    this.#candidatesTaken = true;
    return this.#candidates.stream;
  }

  async addIceCandidate(candidate: IceCandidate): Promise<void> {
    this.#requireOpen();
    try {
      await this.#pc.addIceCandidate({
        candidate: candidate.candidate,
        sdpMid: candidate.sdpMid ?? null,
        sdpMLineIndex: candidate.sdpMlineIndex ?? null,
      });
    } catch (err) {
      throw new ComponentException<WebrtcError>({ kind: "invalid-signaling", value: String(err) });
    }
  }

  stateChanges(): ReadableStream<ConnectionState> {
    if (this.#stateTaken) {
      return new ReadableStream<ConnectionState>({
        start(c) {
          c.close();
        },
      });
    }
    this.#stateTaken = true;
    return stateStream<ConnectionState>(
      () => {
        if (this.#closed) return "closed";
        if (this.#failed) return "failed";
        return this.#pc.connectionState;
      },
      (wake) => {
        this.#pc.addEventListener("connectionstatechange", wake);
        this.#pc.addEventListener("iceconnectionstatechange", wake);
        this.#statePokes.add(wake);
      },
      (state) => state === "failed" || state === "closed",
    );
  }

  async waitConnected(): Promise<void> {
    const pc = this.#pc;
    const isFailed = () => this.#isFailedNow() || pc.connectionState === "closed";

    if (this.#isConnectedNow()) this.#everConnected = true;
    if (this.#everConnected) return;
    if (this.#closed || isFailed()) throw new ComponentException<WebrtcError>({ kind: "closed" });
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        cleanup();
        reject(new ComponentException<WebrtcError>({ kind: "timed-out" }));
      }, CONNECT_TIMEOUT_MS);
      const check = () => {
        if (this.#isConnectedNow()) {
          this.#everConnected = true;
          cleanup();
          resolve();
        } else if (isFailed()) {
          cleanup();
          reject(new ComponentException<WebrtcError>({ kind: "closed" }));
        }
      };
      const onClose = () => {
        cleanup();
        reject(new ComponentException<WebrtcError>({ kind: "closed" }));
      };
      const cleanup = () => {
        clearTimeout(timer);
        this.#closeHooks.delete(onClose);
        pc.removeEventListener("connectionstatechange", check);
        pc.removeEventListener("iceconnectionstatechange", check);
      };
      this.#closeHooks.add(onClose);
      pc.addEventListener("connectionstatechange", check);
      pc.addEventListener("iceconnectionstatechange", check);
    });
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const hook of this.#closeHooks) hook();
    this.#closeHooks.clear();
    this.#candidates.end();
    this.#channels.end();
    for (const poke of this.#statePokes) poke();
    // Close the owned channels through their wrappers, so the close is
    // observed locally at once (the wrapper latches its local-close flag
    // and closes the native channel; `DataChannel.close` is idempotent).
    // Gating on the native `readyState` alone is not enough: a backend may
    // transition it asynchronously, leaving a post-close `send` a window
    // in which it still sees `"open"`.
    for (const wrapper of this.#ownedWrappers) {
      try {
        wrapper.close();
      } catch {
        // Already closed.
      }
    }
    const deadline = Date.now() + CLOSE_DRAIN_MS;
    const drained = () =>
      // deno-lint-ignore no-explicit-any
      [...this.#ownedChannels].every((channel: any) => channel.bufferedAmount === 0);
    const tick = setInterval(() => {
      if (drained() || Date.now() >= deadline) {
        clearInterval(tick);
        this.#pc.close();
      }
    }, 10);
    // deno-lint-ignore no-explicit-any
    (tick as any).unref?.();
  }

  [Symbol.dispose](): void {
    try {
      this.close();
    } catch {
      // Already closed.
    }
  }
}

// --- helpers -------------------------------------------------------------------

/**
 * Extract the ICE candidates from an SDP description as
 * `{ candidate, sdpMid, sdpMlineIndex }` records in the W3C trickle shape.
 * Ported from jco-impl/webrtc.js:834-854.
 */
function sdpCandidates(
  sdp: string | undefined,
): { candidate: string; sdpMid: string | undefined; sdpMlineIndex: number | undefined }[] {
  if (!sdp) return [];
  const out: { candidate: string; sdpMid: string | undefined; sdpMlineIndex: number | undefined }[] = [];
  let sdpMid: string | undefined;
  let sdpMlineIndex = -1;
  for (const line of sdp.split(/\r?\n/)) {
    if (line.startsWith("m=")) {
      sdpMlineIndex += 1;
      sdpMid = undefined;
    } else if (line.startsWith("a=mid:")) {
      sdpMid = line.slice("a=mid:".length).trim();
    } else if (line.startsWith("a=candidate:")) {
      out.push({
        candidate: line.slice("a=".length).trim(),
        sdpMid,
        sdpMlineIndex: sdpMlineIndex >= 0 ? sdpMlineIndex : undefined,
      });
    }
  }
  return out;
}

/**
 * Iterate a guest-provided WIT stream. The conventions hand the host a
 * `Stream<T>` handle whose async iterator yields `Chunk<T>` — an *array* of
 * elements for a non-`u8` element type — so a batched read is flattened
 * here. A web `ReadableStream`/plain `AsyncIterable` is also tolerated.
 */
async function* streamItems(
  input:
    | Stream<LiftedStreamMessage>
    | ReadableStream<LiftedStreamMessage>
    | AsyncIterable<LiftedStreamMessage>,
): AsyncGenerator<LiftedStreamMessage> {
  if (input instanceof ReadableStream) {
    const reader = input.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        yield value;
      }
    } finally {
      reader.releaseLock();
    }
    return;
  }
  for await (const value of input as AsyncIterable<unknown>) {
    // A batched read yields an array of elements.
    if (Array.isArray(value)) {
      yield* value as LiftedStreamMessage[];
    } else {
      yield value as LiftedStreamMessage;
    }
  }
}

/**
 * Collect every byte of a `stream<u8>` into one `Uint8Array`. Accepts the
 * runtime's `Stream<number>` handle (`read(max)`, empty chunk = end — see
 * contracts/embedder-api.md §"Streams and futures"), a `ReadableStream`, or
 * a plain `AsyncIterable`.
 */
async function collectByteStream(
  stream: Stream<number> | ReadableStream<unknown> | AsyncIterable<unknown>,
): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  const push = (value: unknown) => {
    if (value === undefined || value === null) return;
    const chunk = toByteChunk(value);
    if (chunk.length) {
      chunks.push(chunk);
      total += chunk.length;
    }
  };
  if (typeof ReadableStream !== "undefined" && stream instanceof ReadableStream) {
    const reader = stream.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        push(value);
      }
    } finally {
      reader.releaseLock();
    }
  } else if (isStream(stream)) {
    const READ_BATCH = 65536;
    for (;;) {
      const chunk = await stream.read(READ_BATCH);
      if ((chunk as Uint8Array).length === 0) break;
      push(chunk);
    }
  } else {
    for await (const value of stream as AsyncIterable<unknown>) {
      push(value);
    }
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}

/**
 * Coerce one chunk of a WIT byte stream (a number, an array of numbers, or
 * a typed array, depending on how the runtime batched the read) to a
 * `Uint8Array`.
 */
function toByteChunk(value: unknown): Uint8Array {
  if (typeof value === "number") return Uint8Array.of(value);
  if (value instanceof Uint8Array) return value;
  return Uint8Array.from(value as ArrayLike<number>);
}

/** A single-chunk byte `ReadableStream` over `bytes`. */
function bytesToReadable(bytes: Uint8Array): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      if (bytes.length) controller.enqueue(bytes);
      controller.close();
    },
  });
}

/**
 * A `ReadableStream` fed by an event source. `setup(push, end)` wires the
 * source to `push` each value and `end` to close the stream; values pushed
 * before the stream starts pulling are buffered. Ported verbatim from the
 * jco-impl reference's `eventStream` (jco-impl/webrtc.js:788-818).
 */
function eventStream<T>(
  setup: (push: (item: T) => void, end: () => void) => void,
): { stream: ReadableStream<T>; end: () => void } {
  let controller: ReadableStreamDefaultController<T> | undefined;
  let ended = false;
  const buffer: T[] = [];
  const stream = new ReadableStream<T>({
    start(c) {
      controller = c;
      for (const item of buffer) c.enqueue(item);
      buffer.length = 0;
      if (ended) c.close();
    },
  });
  const push = (item: T) => {
    if (ended) return;
    if (controller) controller.enqueue(item);
    else buffer.push(item);
  };
  const end = () => {
    if (ended) return;
    ended = true;
    if (controller) {
      try {
        controller.close();
      } catch {
        // Already closed.
      }
    }
  };
  setup(push, end);
  return { stream, end };
}

/**
 * A pull-based coalescing state watch backing the `state-changes` streams:
 * each element is `current()` at the time it is produced (the first
 * element reflects the state at the first read), consecutive elements are
 * distinct, and the stream closes after a terminal state. Ported from the
 * jco-impl reference's `stateStream` (jco-impl/webrtc.js:978-1011).
 */
function stateStream<S>(
  current: () => S,
  subscribe: (wake: () => void) => void,
  isTerminal: (state: S) => boolean,
): ReadableStream<S> {
  let delivered: S | undefined;
  let hasDelivered = false;
  let notify: (() => void) | null = null;
  subscribe(() => {
    if (notify) {
      const wake = notify;
      notify = null;
      wake();
    }
  });
  return new ReadableStream<S>({
    async pull(controller) {
      for (;;) {
        const woken = new Promise<void>((resolve) => {
          notify = resolve;
        });
        const state = current();
        if (!hasDelivered || state !== delivered) {
          hasDelivered = true;
          delivered = state;
          controller.enqueue(state);
          if (isTerminal(state)) controller.close();
          return;
        }
        if (isTerminal(state)) {
          controller.close();
          return;
        }
        await woken;
      }
    },
  });
}

// Eagerly resolve the backend at module load via a genuine **top-level
// await**, mirroring the reference's own top-level await
// (jco-impl/webrtc.js:45): ES module evaluation does not complete for any
// importer of this file until this await settles, so `new PeerConnection`
// stays synchronous once anyone can actually reach it — see the doc comment
// on the constructor. Failures are swallowed here rather than left to
// propagate out of module evaluation (which would break every consumer of
// this module in environments without WebRTC at all, including ones that
// only want e.g. the pure structural types); `PeerConnection.create()`
// re-attempts resolution and surfaces the actionable error to its caller.
await resolveRTCPeerConnection().catch(() => {
  cachedRTCPeerConnection = undefined;
});

// --- module wiring (contracts/embedder-api.md §"Module wiring and instantiation") --

/** The exact WIT interface id this port implements. */
export const CONNECTIONS_INTERFACE = "polymorph:webrtc-datachannels/connections@0.1.0";

/**
 * The interface-shaped aggregate export: "a module's named export, camelCase
 * of the interface short-name, provides that interface."
 */
export const connections = { DataChannelOptions, PeerConnectionConfig, DataChannel, PeerConnection };

/**
 * The imports-record fragment for `instantiate`:
 * `{ "polymorph:webrtc-datachannels/connections@0.1.0": { ... } }`.
 * Registered at the interface's exact version (only one exists) — the
 * resolver derives the `@0.1` track alternate automatically.
 */
export function webrtcImports(): Record<string, unknown> {
  return { [CONNECTIONS_INTERFACE]: connections };
}
