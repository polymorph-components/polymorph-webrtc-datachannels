# examples

Guest components (`echo-demo`, `cli-signaling`, `webrtc-consumer`) and the native
demo/manual-signaling driver (`wasmtime-demo`) that exercise the
`polymorph:webrtc-datachannels` interfaces.

- **`echo-demo`** / **`cli-signaling`** — guest components whose WebRTC work is
  performed by a **host** (`wasmtime-impl`, or `polyengine-impl` under a JS engine).
- **`webrtc-consumer`** — a minimal consumer component that **imports**
  `polymorph:webrtc-datachannels/connections`. It is composed (`wac plug`) with the
  [`wasip3-impl`](../wasip3-impl) provider component — whose in-guest sans-I/O
  stack satisfies that import — into one self-contained component, then run under
  `wasmtime`, standing up an offerer and an answerer that connect over loopback
  entirely in-guest and exchange a message each way. This is the basic in-guest
  integration test. Build, compose, and run it with `just examples::test-webrtc-composed`
  (needs `wasmtime` v46+ and `wac` on `PATH`), or directly:

  ```sh
  cargo build --release -p wasip3-webrtc-datachannels -p webrtc-consumer \
      --target wasm32-wasip2
  wac plug target/wasm32-wasip2/release/webrtc_consumer.wasm \
      --plug target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm \
      -o target/webrtc-composed.wasm
  wasmtime run -W component-model-async=y -S cli -S p3 -S inherit-network \
      target/webrtc-composed.wasm
  ```
- **`wasmtime-demo`** — the native Rust host binaries built on `wasmtime-impl`.

## Shared guest helpers are deliberately duplicated

Small helpers (draining a `local-ice-candidates` stream, adopting the first
incoming data channel, the `wasi:cli` stdout `print`) are copied between the
example guests rather than shared through a crate: each example stays
self-contained, and the guests intentionally span two `wit-bindgen`
generations (0.59, and 0.57 matching the `wasip3` crate's runtime), whose
stream types do not unify across a shared library. Treat the copies as
per-example code, not a library to keep in sync.
