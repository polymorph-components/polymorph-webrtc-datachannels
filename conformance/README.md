# Cross-implementation conformance

The conformance suite runs one corpus of behavioral cases — the same
assertions, the same stimulus — against every implementation of
`polymorph:webrtc-datachannels`, over real WebRTC data channels. It is
built on the [`polymorph:test`](https://github.com/polymorph-components/polymorph-test)
harness: suites are wasm components exporting the frozen tests
contract, runners emit one canonical results JSONL stream per target,
and `component-test aggregate` validates every stream against the
committed lockfile and target manifest before rendering the matrix.

Run everything with `just conformance` (Node 24+, Deno 2.x, a Chrome 137+
binary, and `wac` required; see [`AGENTS.md`](../AGENTS.md) for setup).

## How it works

| Piece | Role |
| --- | --- |
| [`suite-body/`](suite-body) | The case bodies and SUT bindings, shared by both suite components: the incumbent guest's assertions re-plumbed for the harness (config via the store environment, per-case rooms). |
| [`guest-ct/`](guest-ct) | The full suite component. The name hierarchy is the execution topology: `solo/*` cases run in one instance (two in-process peer connections, or a lone peer for the error probes); `pair/*` cases run as two role-paired instances of the same binary sharing a signaling room. The committed `tests.lock` is the corpus inventory. |
| [`guest-pair-ct/`](guest-pair-ct) | The pair-only sibling suite: the `pair/*` subset as its own artifact with its own `tests.lock` — the corpus the interop directions and the network labs run (their targets never execute `solo/*` cases). |
| [`driver-ct/`](driver-ct) | The legs. `rtc-ct-driver`'s child mode (`exec`) runs one suite instance's stream — a role, a selection, a linker profile — on the component-test runner (fresh instance per case, budgets, the wire format). Its orchestrator modes provision the signaling server, spawn one child stream per instance, fold the two sides of each pair case (worst status wins, details role-labelled), and emit one stream per target. `polyengine/` holds the [polyengine](https://github.com/polymorph-components/polyengine) children (stock Deno and headless Chromium, runtime-linked — no transpile step or engine flag). `targets*.toml` are the per-matrix manifests; `matrix.md` and `matrix-interop.md` are the committed review surfaces. |
| [`reference/`](reference) | The non-wasm reference peer: a native binary driving Google's libwebrtc (via LiveKit's Rust bindings), the suite's wire-level anchor. One process per case; the orchestrator synthesizes its stream and pairs it against any suite target. |
| [`signaling/`](signaling) | `conformance-signalingd`, the suite-owned HTTP mailbox (see `signaling/PROTOCOL.md`). The guest never speaks HTTP: it imports `conformance:signaling/mailbox`, served natively by the driver, by `fetch` in the polyengine legs (`driver-ct/polyengine/signaling.ts`), and by the in-guest `wasi:http` client ([`wasip3-mailbox/`](wasip3-mailbox)) in the composed artifacts. |
| [`wit/`](wit) | The suite world (`sut-imports`): only the surface under test and the mailbox — the export surface comes from the component-test SDK. The `polymorph:webrtc-datachannels` package arrives through the `deps` symlink, never a copy. |

### Pairing

A `pair/*` case runs as two instances of the same suite binary — an
offerer and an answerer — in lockstep: role, signaling URL, and a
run id arrive through the store environment (`RTC_CT_*`), each case
derives its room as `<run-id>-<case id>`, and the two sides rendezvous
per case over the mailbox (long-poll holds whichever side arrives
first). Both sides are first-class verdict producers: the driver folds
the two streams case-wise, so a red cell names the side that failed —
asymmetric assertions like `channel-close-flush` (offerer asserts the
flush signal, answerer asserts payload completeness and the observed
close) keep both halves.

## The matrices

- **Loopback** (`targets.toml`, committed `matrix.md`): the full suite
  per implementation — `wasmtime` (native webrtc-rs host), `composed`
  (the suite `wac plug`ged with the `wasip3-impl` provider and the
  in-guest mailbox client: the whole WebRTC stack in wasm over
  `wasi:sockets`), `polyengine-deno` (the polyengine-native host,
  `polyengine-impl/`, runtime-linked under stock Deno — no transpile step,
  no engine flag), and `polyengine-browser` (the same polyengine host module
  over the browser's native `RTCPeerConnection` in headless Chromium,
  served as one deno-bundled page module).
- **Interop** (`targets-interop.toml`, committed `matrix-interop.md`):
  the pair-only suite per `<offerer>-x-<answerer>` direction — every
  implementation against the reference peer in both orders (a red cell
  implicates the implementation, not the pair), the reference
  self-pair, and the direct implementation pairs. The one expected-fail
  is `wasip3-guest-x-reference` / `pair/channel-close-flush`
  ([#123](https://github.com/polymorph-components/polymorph-webrtc-datachannels/issues/123):
  `rtc` emits no SCTP stream reset on close). An expected-fail that
  passes fails the aggregate, keeping the declaration honest.
- **Shadow** (`targets-shadow.toml`; gated by exit code, matrix
  uploaded as a CI artifact): the pair corpus over a routed,
  non-loopback path inside the Shadow discrete-event simulator —
  deterministic, no root, CI's non-loopback coverage. Rows: `wasmtime`,
  `wasip3-guest`, `reference`, and the `wasmtime`↔`wasip3-guest`
  interop pair in both orders.
- **netns** (`targets-netns.toml`; workstation-only, root): real
  routed candidate paths per ICE scenario — `lan`, `stun-srflx`
  (full-cone NAT), `turn-relay`, `nat-symmetric` (ICE falls back to
  relay through coturn). `just conformance::netns <scenario>`, then
  `just conformance::aggregate-netns` once all four streams exist.

## Running

```sh
just conformance                    # loopback + interop + matrix gates
just conformance::run-wasmtime      # one loopback leg
just conformance::run-interop       # the 13 interop directions
just conformance::shadow            # the Shadow lab (needs `shadow`)
just conformance::netns lan         # one netns scenario (sudo)
just conformance::lock-update       # after an intentional suite change
just conformance::matrix-update     # after an intentional behavior change
```

The lockfiles and matrices are generated artifacts: regenerate them
through the recipes and commit the diff — the diff is the review
surface. The `component-test` CLI is cargo-installed at the rev
`Cargo.lock` pins for the harness crates (one rev everywhere; the
`_ct-tools` pins gate fails on skew).

## Growing the suite

Add a case body to `suite-body`, a delegating `#[case]` to `guest-ct`
(and to `guest-pair-ct` if it is a pair case), then
`just conformance::lock-update` and a full run with
`just conformance::matrix-update`; commit the diffs. Assertions target
interoperable behavior only — never SDP contents, candidate ordering,
timing, or exact error strings. When an implementation cannot serve a
capability, declare a feature in the manifests, tag the affected cases,
and add a `!feature` decline probe — the polymorph:test way — rather
than skipping.
