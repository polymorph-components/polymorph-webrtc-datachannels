# wasip3-impl (`wasip3-webrtc-datachannels`)

A **wasm component** that runs the sans-I/O `rtc` WebRTC stack *in-guest* and
**exports** the shared `polymorph:webrtc-datachannels` `connections` resources. It
imports only WASIp3 capabilities — `wasi:sockets` UDP and `wasi:clocks` timers —
so it can be composed (`wac plug`) with any consumer component that imports
`connections`.

This is the third implementation alongside the [`wasmtime-impl`](../wasmtime-impl)
(webrtc-rs) and [`polyengine-impl`](../polyengine-impl) (JS engines/browser) hosts. Unlike those two —
which run the fully async `webrtc-rs` engine *host-side* — this one is itself a
component: it drives the *sans-I/O* `rtc` stack, where protocol logic is
separated from I/O, over `wasi:sockets` UDP and `wasi:clocks` timers, entirely
inside wasm.

## Layers

- **`SansIoPeer`** (`src/peer.rs`) — the runtime-agnostic core. It wraps an
  `rtc` `RTCPeerConnection` and exposes only signaling primitives, the six
  sans-I/O stepping calls (`poll_transmit` / `handle_input` / `poll_timeout` /
  `handle_timeout` plus drained events), and message sends. It performs **no**
  I/O and awaits nothing.
- **`runtime`** (`src/runtime.rs`) — the in-guest driver. A `Runtime` owns one
  peer connection's UDP socket and sans-I/O core and runs the event loop as a
  detached task (`wit_bindgen::spawn`): it flushes queued datagrams, drains the
  core's events into shared queues, and parks on the earliest of a timer or an
  inbound datagram.
- **`provider`** (`src/provider.rs`) — the exported `connections` resources
  (`data-channel-options`, `data-channel`, `peer-connection`), implemented on
  top of the driver.

Because the sans-I/O model has no OS interface enumeration on wasm (`ifaces()`
returns `Unsupported`), each `peer-connection` supplies its own host candidate
**explicitly** from the socket it binds, rather than gathering from mDNS.

## Composition & integration test

`peer-connection` binds its socket on IPv4 loopback, so this component is the
composable, self-contained provider used for **same-host** integration testing:
two peers run in one process and reach each other over `127.0.0.1`.
[`examples/webrtc-consumer`](../examples/webrtc-consumer) is a minimal consumer
that imports `connections`, stands up an offerer and an answerer, and round-trips
a message each way. Compose and run it:

```sh
just examples::test-webrtc-composed
```

which builds both components, composes them with `wac plug` (the consumer's
`connections` import satisfied by this component's export), and runs the single
composed component under `wasmtime`.

Real (non-loopback) networking is a later step: it needs a way for the consumer
to choose the bind address and a routable host candidate.

## Build

```sh
just examples::build-wasip3-provider
```

produces `target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm`, a
component that imports `wasi:sockets`/`wasi:clocks` and exports
`polymorph:webrtc-datachannels/connections`.

## Dependency pin

`rtc` is pinned once, at the workspace level in the root `Cargo.toml`, to an
upstream [`webrtc-rs/rtc`](https://github.com/webrtc-rs/rtc) `master` commit:
the srflx source-address fix
([`webrtc-rs/rtc#136`](https://github.com/webrtc-rs/rtc/pull/136)) is merged
upstream but not yet in any published release
([#120](https://github.com/polymorph-components/polymorph-webrtc-datachannels/issues/120)
tracks unwinding the pin).
