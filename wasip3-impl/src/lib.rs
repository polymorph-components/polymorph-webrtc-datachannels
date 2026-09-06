//! `wasip3-webrtc-datachannels`: a wasm **component** that runs the sans-I/O
//! `rtc` WebRTC stack *in-guest* and exports the shared
//! `polymorph:webrtc-datachannels` `connections` resources.
//!
//! This is the third implementation alongside the `wasmtime-impl` (webrtc-rs)
//! and `polyengine-impl` (JS engines/browser) hosts. Unlike those two — which run the fully async
//! `webrtc-rs` engine host-side — this one is itself a component: it drives the
//! sans-I/O `rtc` stack over WASIp3 `wasi:sockets` UDP and `wasi:clocks` timers,
//! entirely inside wasm.
//!
//! Because it imports only WASIp3 interfaces and exports the package surface, it
//! can be composed (`wac plug`) with any consumer component that imports
//! `connections`, producing a single self-contained component.
//!
//! Layers:
//!
//! - [`SansIoPeer`] (`peer.rs`) — the runtime-agnostic core wrapping an `rtc`
//!   `RTCPeerConnection`: signaling primitives, the six sans-I/O stepping calls,
//!   and message sends. It performs no I/O.
//! - The `runtime` module — a WASIp3 `wasi:sockets`/`wasi:clocks` pump that runs
//!   the core in-guest.
//! - The `provider` module — the exported `connections` resources
//!   (`data-channel-options`, `data-channel`, `peer-connection`) implemented on
//!   top of the driver.

mod peer;

wit_bindgen::generate!({
    path: "wit",
    world: "provider",
    generate_all,
});

mod provider;
mod runtime;

use provider::Component;
export!(Component);
