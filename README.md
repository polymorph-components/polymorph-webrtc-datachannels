# polymorph:webrtc-datachannels

A WIT interface and multiple implementations showing that high-performance
WebRTC data-channel communication can be expressed with the **WebAssembly
Component Model's async features** (`stream`, `future`, async imports/exports),
with a *single* guest component binary running unchanged against very
different stacks:

- a **native Rust** host ([Wasmtime] + [`webrtc-rs`]),
- a **runtime-linked JS** host ([`polyengine`] on stock Deno — and inside a real
  browser — backed by [`node-datachannel`] or the native `RTCPeerConnection`;
  no transpile step, no engine flag), and
- an **in-guest** component ([`wasip3-impl`](wasip3-impl)) that runs the whole
  sans-I/O WebRTC stack inside wasm over `wasi:sockets`.

All of them run the identical component and round-trip every message through a
genuine WebRTC/SCTP data channel; the [conformance suite](conformance) asserts
they behave compatibly.

[`node-datachannel`]: https://github.com/murat-dogan/node-datachannel
[Wasmtime]: https://github.com/bytecodealliance/wasmtime
[`webrtc-rs`]: https://github.com/webrtc-rs/webrtc
[`polyengine`]: https://github.com/polymorph-components/polyengine

## Releases

Everything here is **unstable** (0.x), but [releases](../../releases) are
**caret-honest**: within a minor line they stay backward-compatible, and
anything breaking bumps the minor. Consumption is pinned at a release's
commit — cargo git dependencies, vendored WIT, the release-pinned
polyengine/JSR graph — and bumped deliberately.

## What's here

| Path | Deliverable |
| --- | --- |
| [`wit/`](wit) | The streaming **WIT interface**, the `polymorph:webrtc-datachannels@0.1.0` package. Each demo component keeps its own demo-only WIT and symlinks this package in as a dependency. |
| [`examples/echo-demo`](examples/echo-demo) | A **Rust example component** exercising a data channel one message at a time. |
| [`wasmtime-impl`](wasmtime-impl) | The **Wasmtime host crate** (webrtc-rs), modeled after `wasmtime_wasi_http::p3`. Provides `add_to_linker` + `WebrtcView` for the `types` interface and the `data-channel` resource of `connections` (the `peer-connection` resource is unimplemented). Crate name: `wasmtime-webrtc-datachannels`. |
| [`polyengine-impl`](polyengine-impl) | The **runtime-linked JS host module** ([polyengine](https://github.com/polymorph-components/polyengine) on stock Deno or in the browser, node-datachannel/RTCPeerConnection-backed): the browser-first reference host (`jco-impl/webrtc.js`, retired with the jco legs — see git history) ported to polyengine's embedder conventions, upstreamed from polyengine's `ports/webrtc`. |
| [`examples/wasmtime-demo`](examples/wasmtime-demo) | The **native Rust host** (Wasmtime + webrtc-rs): demo binaries built on `wasmtime-impl`. |
| [`examples/cli-signaling`](examples/cli-signaling) | The **manual-signaling CLI guest component** (Rust), driving `connections.peer-connection` with guest-side vanilla ICE. |
| [`examples/webrtc-consumer`](examples/webrtc-consumer) | A **minimal consumer component** that imports `connections`. Composed (`wac plug`) with `wasip3-impl` for the in-guest round-trip integration test (`just examples::test-webrtc-composed`). |
| [`wasip3-impl`](wasip3-impl) | The **third implementation**: a wasm **component** (built for `wasm32-wasip2`) that runs the sans-I/O `rtc` 0.21 WebRTC stack *in-guest* — importing only `wasi:sockets`/`wasi:clocks` — and **exports** `polymorph:webrtc-datachannels/connections`. Its `SansIoPeer` core is driven over `wasi:sockets` UDP and WASI timers by an in-guest runtime pump. Composable via `wac plug`. Crate name: `wasip3-webrtc-datachannels`. |
| [`conformance/`](conformance) | The **cross-implementation conformance suite**, on the [`polymorph:test`](https://github.com/polymorph-components/polymorph-test) harness: two suite components (full and pair-only) run against every target (wasmtime, the composed wasip3 stack, polyengine under stock Deno and inside headless Chromium), an interop matrix pairing every implementation with a non-wasm reference peer (Google's libwebrtc via LiveKit's Rust bindings), a netns lab, and a Shadow lab. `just conformance`; see [`conformance/README.md`](conformance/README.md). |
| [`AGENTS.md`](AGENTS.md) | Orientation for agents/contributors, linking the `lann/wasm-component-starter` knowledge base. |

## The interface

The interface lives at the root [`wit/`](wit) as the
`polymorph:webrtc-datachannels` package. Each demo component keeps its own demo-only
WIT alongside it and pulls the package in as a `deps` symlink, so there is still
a single copy of the shared surface to edit:

**`polymorph:webrtc-datachannels`** — the shared interfaces:

- **`types`** — every structural (non-resource) type in the package: the
  `error` variant, the `message`/`message-kind`/
  `stream-message`/`send-via-stream-error` data-channel types, and the
  `sdp-type`/`session-description`/`ice-candidate` signaling types. Structural
  types carry no host identity, so a single composition can share them across
  components.
- **`connections`** — the stateful WebRTC resources, which (unlike the
  structural `types`) are each owned by the one component that implements them:
  - **`data-channel-options`** — a configuration builder for a data channel (a
    subset of `RTCDataChannelInit`: `label`, `ordered`, `max-retransmits`),
    shaped after `wasi:http`'s `request-options`: construct a default value,
    adjust fields through the setters, then hand it to a data-channel-creating
    function such as `peer-connection.create-data-channel`.
  - **`data-channel`** — the high-throughput surface. A `data-channel` is
    bidirectional and message-oriented; each call carries exactly **one**
    data-channel message, preserving WebRTC message boundaries:
    - `send: async func(message: message) -> result<_, error>`
    - `receive: async func() -> result<message, error>`

    A `message` is a variant — `binary(list<u8>)` or `%string(string)` (text,
    valid UTF-8). Concurrent calls are supported so the host and guest can
    pipeline messages and let the async ABI apply backpressure. To bound
    in-memory buffering, a message may instead flow through a byte `stream` as a
    `stream-message` (`kind`, `length`, `data: stream<u8>`):
    - `send-via-stream: async func(messages: stream<stream-message>) -> result<_, send-via-stream-error>`
    - `receive-via-stream: func() -> result<stream<stream-message>, error>`

    `send-via-stream-error` carries the underlying `error` plus `sent`, the
    number of messages handed to the transport before the failure.
    `receive-via-stream` takes over the channel's inbound messages: it may be
    called only once, after which it (and `receive`) fail with
    `error.receiving-via-stream`.
  - **`peer-connection`** — a fuller `RTCPeerConnection`-style surface (SDP
    offer/answer + trickle ICE) that documents where a *guest-driven* connection
    API is headed. It is the design target and is **not** required by the
    runnable demo.

**`demo:webrtc-echo`** — the demo-only interfaces, which live with the echo
demo that uses them ([`examples/echo-demo/wit`](examples/echo-demo/wit); the
manual-signaling CLI demo in
[`examples/cli-signaling`](examples/cli-signaling) imports only the standard
`connections` interface and handles vanilla ICE guest-side):

- **`rendezvous`** — a proposed, deliberately *unstandardized* HTTP signaling
  mailbox for carrying SDP/ICE between two *separate* peers via an existing
  server over `wasi:http@0.3`, so remote connections can be developed locally.
  Like the `connections.peer-connection` resource, it is designed but not yet wired into the runnable demo (see
  [`AGENTS.md`](AGENTS.md#real-signaling-rendezvous--wasihttp03--direction)).
- **`demo`** — the exported entry point (`run`) the hosts call.

The demo world is intentionally tiny:

```wit
world webrtc-echo-demo {
    import polymorph:webrtc-datachannels/connections@0.1.0;
    export demo;
}
```

## The example component

[`examples/echo-demo`](examples/echo-demo/src/lib.rs) is host-agnostic Rust
(`wit-bindgen`, `wasm32-unknown-unknown` + `wasm-tools component new`). Its
`run`:

1. stands up **two** peer connections in-component through the standard
   `connections` interface (a real SDP offer/answer exchange plus trickled
   ICE) and adopts the negotiated channel on both ends,
2. sends `message-count` messages one at a time through `data-channel.send`
   on one end, echoes each back from the other, and **concurrently** reads
   them back one at a time from `data-channel.receive` (all loops under
   `futures::join!`),
3. returns counts so the host can assert a complete round trip.

## Running it

Prerequisites: Rust (with the `wasm32-unknown-unknown` target),
[`wasm-tools`], Deno 2.x (the polyengine host), and Node 24+ (the polyengine
browser drivers — plain `node`, no engine flag).

[`wasm-tools`]: https://github.com/bytecodealliance/wasm-tools

### polyengine (runtime-linked JS) host

The conformance legs are the maintained entry points — the component is
runtime-linked at load time, so there is nothing to transpile or stage:

```sh
just conformance::run-polyengine           # the full suite under stock Deno
just conformance::run-polyengine-browser   # the same suite inside headless Chromium
```

The host module (`polyengine-impl/src/webrtc.ts`) is environment-portable: it
resolves `RTCPeerConnection` from the browser global when present and falls
back to `node-datachannel` under Deno.

### Wasmtime (native Rust) host

```sh
cd examples/wasmtime-demo
cargo run --release --bin wasmtime-webrtc-host -- ../echo-demo/build/echo-demo.component.wasm 1000 4096
#                                                                                       ^msg count ^msg size
```

(Run `just examples::build-component` once first, or build the component
manually, to produce the `.wasm`.)
