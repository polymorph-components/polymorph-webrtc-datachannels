// In-process loopback tests for the `polyengine-impl` host module: two
// instantiations of the port's `PeerConnection`/`DataChannel` classes wired
// directly to each other (no signaling server), buffering trickled
// candidates until the peer has its remote description.
//
// Sanitizer note: `node-datachannel` keeps background native threads alive
// across the whole process. A `deno test` process cannot exit
// mid-suite, so every test below disables `sanitizeResources`/`sanitizeOps` —
// the leaked timers/ops belong to node-datachannel's native worker pool, not
// to unclosed resources this test forgot to close (every test does close its
// peer connections). `node-datachannel`'s own `cleanup()` is called once at
// the end of the whole suite (see the final `Deno.test` below);
// per-test cleanup would tear down the shared native
// context out from under any test that runs after it.

import { assert, assertEquals, assertRejects } from "jsr:@std/assert@1";
import {
  DataChannel,
  DataChannelOptions,
  PeerConnection,
  resetMaxInboundBufferBytes,
  setMaxInboundBufferBytes,
} from "../src/webrtc.ts";
import { ComponentException } from "@polyengine/protocol";
import type { IceCandidate, Message, WebrtcError } from "../src/types.ts";

const NO_SANITIZE = { sanitizeResources: false, sanitizeOps: false };

/**
 * Wire two freshly constructed `PeerConnection`s through a full offer/answer
 * + trickle-ICE exchange, in-process (no network signaling server),
 * buffering each side's candidates until the peer has its remote
 * description.
 */
async function connectPair(): Promise<{ a: PeerConnection; b: PeerConnection }> {
  const a = await PeerConnection.create();
  const b = await PeerConnection.create();

  // Candidates start flowing as soon as a local description is set — on a
  // slow machine that is BEFORE the counterpart has its remote description,
  // and an addIceCandidate delivered that early is rejected
  // (invalid-signaling) and lost (the stream delivers each candidate once).
  // Hold each pump until its receiving peer is ready.
  let aHasRemote!: () => void;
  let bHasRemote!: () => void;
  const ready = {
    a: new Promise<void>((r) => (aHasRemote = r)),
    b: new Promise<void>((r) => (bHasRemote = r)),
  };
  const pumpCandidates = (
    from: PeerConnection,
    to: PeerConnection,
    toReady: Promise<void>,
  ) => {
    (async () => {
      // `localIceCandidates()` returns a plain `ReadableStream<IceCandidate>`
      // (one candidate per element — see src/webrtc.ts's
      // "Module wiring"/streams note), not a batched `Stream<T>` handle.
      for await (const candidate of from.localIceCandidates()) {
        await toReady;
        try {
          await to.addIceCandidate(candidate);
        } catch {
          // Connection may have moved on/closed; ignore stray trickles.
        }
      }
    })();
  };
  pumpCandidates(a, b, ready.b);
  pumpCandidates(b, a, ready.a);

  const offer = await a.createOffer();
  await a.setLocalDescription(offer);
  await b.setRemoteDescription(offer);
  bHasRemote();
  const answer = await b.createAnswer();
  await b.setLocalDescription(answer);
  await a.setRemoteDescription(answer);
  aHasRemote();

  await Promise.all([a.waitConnected(), b.waitConnected()]);
  return { a, b };
}

/** The first data channel `pc` receives via `incomingDataChannels`. */
async function firstIncoming(pc: PeerConnection): Promise<DataChannel> {
  for await (const ch of pc.incomingDataChannels()) return ch;
  throw new Error("incomingDataChannels ended with no channel");
}

Deno.test("loopback: text echo both directions", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const options = new DataChannelOptions();
    options.setLabel("chat");
    const chA = a.createDataChannel(options);
    const chB = await firstIncoming(b);

    await chA.send({ kind: "string", value: "hello from a" });
    const gotAtB = await chB.receive();
    assertEquals(gotAtB, { kind: "string", value: "hello from a" });

    await chB.send({ kind: "string", value: "hello from b" });
    const gotAtA = await chA.receive();
    assertEquals(gotAtA, { kind: "string", value: "hello from b" });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: binary echo + message-boundary preservation", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);

    const msg1 = new Uint8Array([1, 2, 3]);
    const msg2 = new Uint8Array([4, 5]);
    await chA.send({ kind: "binary", value: msg1 });
    await chA.send({ kind: "binary", value: msg2 });

    const got1 = await chB.receive();
    const got2 = await chB.receive();
    assertEquals(got1, { kind: "binary", value: msg1 });
    assertEquals(got2, { kind: "binary", value: msg2 });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: unordered/maxRetransmits options accepted", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const options = new DataChannelOptions();
    options.setOrdered(false);
    options.setMaxRetransmits(3);
    assertEquals(options.ordered(), false);
    assertEquals(options.maxRetransmits(), 3);

    const chA = a.createDataChannel(options);
    const chB = await firstIncoming(b);
    await chA.send({ kind: "string", value: "unordered ok" });
    assertEquals(await chB.receive(), { kind: "string", value: "unordered ok" });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: receive-via-stream consumes a burst", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);

    const received: Message[] = [];
    const streamDone = (async () => {
      const stream = chB.receiveViaStream();
      for await (const sm of stream) {
        const bytes = await collectU8(sm.data as unknown as AsyncIterable<Uint8Array>);
        received.push(
          sm.kind === "string"
            ? { kind: "string", value: new TextDecoder().decode(bytes) }
            : { kind: "binary", value: bytes },
        );
        if (received.length === 3) return;
      }
    })();

    for (let i = 0; i < 3; i++) {
      await chA.send({ kind: "string", value: `msg-${i}` });
    }
    await streamDone;
    assertEquals(received, [
      { kind: "string", value: "msg-0" },
      { kind: "string", value: "msg-1" },
      { kind: "string", value: "msg-2" },
    ]);
  } finally {
    a.close();
    b.close();
  }
});

async function collectU8(stream: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  for await (const chunk of stream) {
    chunks.push(chunk);
    total += chunk.length;
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

Deno.test("loopback: single-use violation -> receiving-via-stream error", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);
    void chA; // keep `a`'s channel referenced for symmetry/documentation

    chB.receiveViaStream();
    const err = await assertRejects(() => chB.receive(), ComponentException);
    assertEquals((err as ComponentException<WebrtcError>).payload, { kind: "receiving-via-stream" });

    // A second `receiveViaStream` call after the first also violates the
    // once-only rule (thrown synchronously, per the WIT contract).
    let threw: unknown;
    try {
      chB.receiveViaStream();
    } catch (e) {
      threw = e;
    }
    assert(threw instanceof ComponentException);
    assertEquals((threw as ComponentException<WebrtcError>).payload, { kind: "receiving-via-stream" });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: inbound-buffer overflow -> overflow-close semantics", NO_SANITIZE, async () => {
  setMaxInboundBufferBytes(16); // small bound: a handful of short messages overflow it
  try {
    const { a, b } = await connectPair();
    try {
      const chA = a.createDataChannel(new DataChannelOptions());
      const chB = await firstIncoming(b);

      // Send enough payload bytes to exceed the 16-byte bound; the receiver
      // never calls `receive()` while these arrive, so they all buffer up
      // and the overflow-close fires on the sender or receiver's channel.
      for (let i = 0; i < 20; i++) {
        try {
          await chA.send({ kind: "string", value: `0123456789-${i}` });
        } catch {
          break; // sender side observed the close once b's channel closed.
        }
      }

      // Drain whatever buffered before the overflow, then expect the
      // terminal `receive-buffer-overflow` error (per the WIT `data-channel`
      // resource doc: "messages buffered before the overflow remain
      // receivable, after which `receive` fails with
      // `error.receive-buffer-overflow`").
      let overflowed = false;
      for (let i = 0; i < 20; i++) {
        try {
          await chB.receive();
        } catch (e) {
          assert(e instanceof ComponentException);
          assertEquals((e as ComponentException<WebrtcError>).payload, {
            kind: "receive-buffer-overflow",
          });
          overflowed = true;
          break;
        }
      }
      assert(overflowed, "expected receive-buffer-overflow after draining the backlog");
    } finally {
      a.close();
      b.close();
    }
  } finally {
    resetMaxInboundBufferBytes();
  }
});

Deno.test("loopback: close propagation + post-close error cases", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  const chA = a.createDataChannel(new DataChannelOptions());
  const chB = await firstIncoming(b);

  chA.close();
  const err = await assertRejects(() => chA.send({ kind: "string", value: "x" }), ComponentException);
  assertEquals((err as ComponentException<WebrtcError>).payload, { kind: "closed" });

  // The peer observes the remote close too (eventually `receive` fails).
  let sawClosed = false;
  for (let i = 0; i < 50; i++) {
    try {
      await chB.receive();
    } catch (e) {
      assert(e instanceof ComponentException);
      sawClosed = true;
      break;
    }
  }
  assert(sawClosed, "expected the peer's receive() to observe the remote close");

  a.close();
  b.close();
  const connErr = await assertRejects(() => a.createOffer(), ComponentException);
  assertEquals((connErr as ComponentException<WebrtcError>).payload, { kind: "closed" });
});

Deno.test(
  "loopback: peer-connection close latches its channels synchronously",
  NO_SANITIZE,
  async () => {
    const { a, b } = await connectPair();
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);

    // Closing the PEER CONNECTION closes its owned channels with the close
    // observed locally at once (the WIT contract): the very first send()
    // after close() fails `closed`, on the locally created channel and on
    // the incoming (remote-created) one alike — regardless of how lazily
    // the backend transitions the native readyState.
    a.close();
    const errA = await assertRejects(
      () => chA.send({ kind: "string", value: "after-close" }),
      ComponentException,
    );
    assertEquals((errA as ComponentException<WebrtcError>).payload, { kind: "closed" });

    b.close();
    const errB = await assertRejects(
      () => chB.send({ kind: "string", value: "after-close" }),
      ComponentException,
    );
    assertEquals((errB as ComponentException<WebrtcError>).payload, { kind: "closed" });
  },
);

Deno.test("loopback: wait-connected resolves and is latched", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    // Already connected; awaiting again resolves immediately (latched).
    await a.waitConnected();
    await b.waitConnected();
  } finally {
    a.close();
    b.close();
    // Latch survives close, per the WIT contract.
    await a.waitConnected();
    await b.waitConnected();
  }
});

// The browser leg's dispatch-inversion regression (issue #154): Chromium
// can dispatch an RTCDataChannel `close` event AHEAD of `message` events
// for data that arrived on the wire first. The WIT contract's
// drop-implies-close ordering requires the payload to win: a parked
// `receive()` must still resolve with the late-dispatched message, and
// only the NEXT receive reports `closed`. Deterministic here: a fake
// channel dispatches the events in the inverted order on purpose (no
// timing, no browser).
Deno.test("remote close dispatched before a delivered message: payload still wins", NO_SANITIZE, async () => {
  const listeners = new Map<string, ((e: unknown) => void)[]>();
  const fake = {
    label: "inversion-probe",
    binaryType: "",
    readyState: "open",
    bufferedAmount: 0,
    addEventListener(type: string, fn: (e: unknown) => void) {
      let fns = listeners.get(type);
      if (!fns) listeners.set(type, fns = []);
      fns.push(fn);
    },
    send(_: unknown) {},
    close() {},
  };
  const fire = (type: string, event: unknown = {}) => {
    for (const fn of listeners.get(type) ?? []) fn(event);
  };

  const dc = new DataChannel(fake);
  const parked = dc.receive(); // waiter parked before any event
  fire("close"); // the inversion: close dispatched first...
  fire("message", { data: new Uint8Array([7, 7, 7, 7, 7, 7, 7, 7]).buffer }); // ...payload one task behind

  const got = await parked;
  assertEquals(got, {
    kind: "binary",
    value: new Uint8Array([7, 7, 7, 7, 7, 7, 7, 7]),
  } as Message);

  // After the drain window the implied close reaches readers as `closed`.
  const err = await assertRejects(() => dc.receive(), ComponentException);
  assertEquals((err.payload as WebrtcError).kind, "closed");
});

// The sender half of issue #154: Chromium can DISCARD payload still in the
// SCTP send queue when `RTCDataChannel.close()` is called, so the port's
// `close()` (and drop) must hold the wire-level reset until the queue
// drains, while the close is still observed locally at once. Deterministic
// against a fake channel that reports a non-empty send buffer.
Deno.test("close with buffered payload defers the reset until the queue drains", NO_SANITIZE, async () => {
  const listeners = new Map<string, ((e: unknown) => void)[]>();
  let nativeCloseCalls = 0;
  const fake = {
    label: "flush-probe",
    binaryType: "",
    readyState: "open",
    bufferedAmount: 8,
    bufferedAmountLowThreshold: -1,
    addEventListener(type: string, fn: (e: unknown) => void) {
      let fns = listeners.get(type);
      if (!fns) listeners.set(type, fns = []);
      fns.push(fn);
    },
    send(_: unknown) {},
    close() {
      nativeCloseCalls++;
    },
  };
  const fire = (type: string, event: unknown = {}) => {
    for (const fn of listeners.get(type) ?? []) fn(event);
  };

  const dc = new DataChannel(fake);
  dc.close();
  // Locally the close is immediate (WIT contract)...
  await assertRejects(
    () => dc.send({ kind: "binary", value: new Uint8Array([1]) }),
    ComponentException,
  );
  // ...but the wire-level reset waits for the transport queue.
  assertEquals(nativeCloseCalls, 0);
  assertEquals(fake.bufferedAmountLowThreshold, 0);

  fake.bufferedAmount = 0;
  fire("bufferedamountlow");
  assertEquals(nativeCloseCalls, 1);

  // Idempotent: a second close (e.g. the drop after an explicit close)
  // does not re-arm anything.
  dc.close();
  assertEquals(nativeCloseCalls, 1);
});

Deno.test("close with a stuck send queue still resets after the drain bound", NO_SANITIZE, async () => {
  const listeners = new Map<string, ((e: unknown) => void)[]>();
  let nativeCloseCalls = 0;
  const fake = {
    label: "stuck-probe",
    binaryType: "",
    readyState: "open",
    bufferedAmount: 8, // never drains
    bufferedAmountLowThreshold: -1,
    addEventListener(type: string, fn: (e: unknown) => void) {
      let fns = listeners.get(type);
      if (!fns) listeners.set(type, fns = []);
      fns.push(fn);
    },
    send(_: unknown) {},
    close() {
      nativeCloseCalls++;
    },
  };

  const dc = new DataChannel(fake);
  dc.close();
  assertEquals(nativeCloseCalls, 0);
  // The 1s drain bound (CHANNEL_CLOSE_DRAIN_MS) fires even though the
  // queue never empties.
  await new Promise((resolve) => setTimeout(resolve, 1_200));
  assertEquals(nativeCloseCalls, 1);
});

// Run node-datachannel's cleanup once, after every test has finished, per
// the probe's discipline (probe.mjs:74/89-90) — it tears down the shared
// native ICE/DTLS/SCTP worker context so the process can exit. This must be
// the LAST registered test so it runs after all the loopback tests above.
Deno.test({
  name: "cleanup: node-datachannel native workers",
  ...NO_SANITIZE,
  fn: async () => {
    try {
      const nodeDatachannel = await import("node-datachannel");
      nodeDatachannel.cleanup?.();
      // Give libdatachannel's poll/worker threads time to join before the
      // test runner tears the isolate down: an exit that races the joins
      // can die in native teardown (SIGSEGV) after every test has already
      // passed. (A hard process exit right after cleanup() is fine — this
      // settle exists for harnesses that keep the runtime alive.)
      await new Promise((resolve) => setTimeout(resolve, 1000));
    } catch {
      // Not resolved to node-datachannel in this run (e.g. werift-forced
      // test environment, or a browser-like global) — nothing to clean up.
    }
  },
});
