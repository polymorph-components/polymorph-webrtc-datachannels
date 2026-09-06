//! Demo-only host glue for the Wasmtime WebRTC host.
//!
//! The `polymorph:webrtc-datachannels` host implementation (`types`,
//! `connections`, and the stream/pipe plumbing) lives in the
//! [`wasmtime_webrtc_datachannels`] crate. The binaries in this crate
//! (`wasmtime-webrtc-host`, `cli-signaling`, `echo-remote`) are thin hosts
//! over its `add_to_linker`; this library holds only the engine/context
//! setup they share.

use wasmtime::{Config, Engine, Result};
use wasmtime_webrtc_datachannels::WebrtcCtx;

/// Build the Wasmtime engine every demo binary uses: the component model with
/// component-model async enabled (the `send`/`receive` methods use the async
/// ABI).
pub fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

/// Build the WebRTC context from the demo hosts' environment surface: the
/// `WEBRTC_INCLUDE_LOOPBACK` variable opts into loopback ICE candidates
/// (peers running on the same host without another mutually reachable address
/// need it to pair), and the conventional `WEBRTC_MAX_INBOUND_BUFFER_BYTES`
/// variable overrides the inbound buffer bound. These env-driven tweaks are
/// host glue: the host crate itself reads no environment, so a malformed
/// value fails loud here.
pub fn webrtc_ctx() -> WebrtcCtx {
    let mut ctx = WebrtcCtx::new();
    if std::env::var_os("WEBRTC_INCLUDE_LOOPBACK").is_some() {
        ctx.set_setting_engine_hook(|engine| engine.with_include_loopback_candidate(true));
    }
    if let Some(bytes) = max_inbound_buffer_bytes_from_env() {
        ctx.set_max_inbound_buffer_bytes(bytes);
    }
    ctx
}

/// The `WEBRTC_MAX_INBOUND_BUFFER_BYTES` override when set: `None` when the
/// variable is unset or empty, the byte count for a positive integer, and a
/// panic otherwise — the variable is primarily a test knob, and a typo that
/// silently restored the default bound would invalidate exactly the test
/// that set it.
fn max_inbound_buffer_bytes_from_env() -> Option<usize> {
    const ENV: &str = "WEBRTC_MAX_INBOUND_BUFFER_BYTES";
    match std::env::var(ENV) {
        Ok(value) if !value.is_empty() => Some(
            value
                .parse::<usize>()
                .ok()
                .filter(|&bytes| bytes > 0)
                .unwrap_or_else(|| {
                    panic!("invalid {ENV} {value:?}: expected a positive byte count")
                }),
        ),
        _ => None,
    }
}
