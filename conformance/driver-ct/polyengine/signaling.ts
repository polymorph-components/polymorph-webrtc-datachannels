// The `conformance:signaling/mailbox` host for the polyengine conformance leg: a
// `fetch`-based client for the suite-owned HTTP mailbox served by
// `conformance-signalingd` (see `conformance/signaling/PROTOCOL.md`). The
// polyengine analogue of the retired jco `signaling.js` (see git history) — same endpoints, same long-poll
// discipline; only the boundary conventions differ (`throw new
// ComponentException({ kind, value })` rather than a bare `{ tag, val }` payload, per
// polyengine's contracts/embedder-api.md §"Error model").
//
// Blob payloads are opaque here; the conformance guest owns the encoding.
// Failures are thrown as the WIT `error` variant's `other` case, which the
// runtime lifts into the `result<_, error>` the mailbox interface declares.

import { ComponentException } from "@polyengine/protocol";

/** The mailbox interface's WIT id (conformance/wit/deps/conformance-signaling). */
export const MAILBOX_INTERFACE = "conformance:signaling/mailbox@0.1.0";

/** `mailbox.role`, lifted per the conventions: a kebab-case string literal. */
export type Role = "offerer" | "answerer";

/**
 * A joined mailbox session for one `{room}` and `{role}` on one server. It
 * publishes to its own role's mailbox and consumes the peer's mailbox in
 * publish order, tracking the next sequence number to fetch. Reads are
 * sequence-numbered and idempotent, so a fetch may be retried after a
 * timeout.
 */
export class Session {
  #base: string;
  #room: string;
  #role: Role;
  #recvSeq = 0;

  constructor(base: string, room: string, role: Role) {
    this.#base = base.replace(/\/+$/, "");
    this.#room = room;
    this.#role = role;
  }

  /**
   * Join (creating implicitly) `room` on the signaling server at `server` as
   * `asRole`.
   */
  static open(server: string, room: string, asRole: Role): Promise<Session> {
    return Promise.resolve(new Session(server, room, asRole));
  }

  /** The peer's role path segment (the mailbox this session consumes). */
  #peerRole(): Role {
    return this.#role === "offerer" ? "answerer" : "offerer";
  }

  /** Publish the next opaque blob to this session's own mailbox. */
  async send(blob: Uint8Array): Promise<void> {
    const url = `${this.#base}/rooms/${this.#room}/${this.#role}`;
    let resp: Response;
    try {
      resp = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/octet-stream" },
        body: blob as Uint8Array<ArrayBuffer>,
      });
    } catch (err) {
      throw mailboxError(`send: ${err}`);
    }
    if (!resp.ok) {
      throw mailboxError(`send status ${resp.status}`);
    }
  }

  /**
   * Fetch the next opaque blob from the peer's mailbox, long-polling and
   * retrying `304` until a blob arrives (returned as a `Uint8Array`) or the
   * peer marks its mailbox done (`undefined` — the lifted `option` none).
   */
  async recv(): Promise<Uint8Array | undefined> {
    for (;;) {
      const url = `${this.#base}/rooms/${this.#room}/${this.#peerRole()}?seq=${this.#recvSeq}&wait=10000`;
      let resp: Response;
      try {
        resp = await fetch(url);
      } catch (err) {
        throw mailboxError(`recv: ${err}`);
      }
      switch (resp.status) {
        // A blob is available: advance our read cursor and return it.
        case 200: {
          const bytes = new Uint8Array(await resp.arrayBuffer());
          this.#recvSeq += 1;
          return bytes;
        }
        // The peer marked its mailbox done at or before this seq.
        case 204:
          return undefined;
        // Not yet available; retry the same seq.
        case 304:
          await resp.body?.cancel();
          continue;
        default:
          throw mailboxError(`recv status ${resp.status}`);
      }
    }
  }

  /** Mark this session's own mailbox as done. */
  async done(): Promise<void> {
    const url = `${this.#base}/rooms/${this.#room}/${this.#role}/done`;
    let resp: Response;
    try {
      resp = await fetch(url, { method: "POST" });
    } catch (err) {
      throw mailboxError(`done: ${err}`);
    }
    if (!resp.ok) {
      throw mailboxError(`done status ${resp.status}`);
    }
  }
}

/** Map a host-side mailbox failure to the guest-visible `error.other`. */
function mailboxError(detail: string): ComponentException {
  return new ComponentException({ kind: "other", value: `mailbox: ${detail}` });
}

/**
 * The imports-record fragment for the mailbox interface:
 * `{ "conformance:signaling/mailbox@0.1.0": { Session } }`.
 */
export function mailboxImports(): Record<string, unknown> {
  return { [MAILBOX_INTERFACE]: { Session } };
}
