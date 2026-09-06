//! Wasmtime host implementation of the `polymorph:webrtc-datachannels`
//! interfaces, backed by the pure-Rust
//! [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) stack.
//!
//! This crate factors the host-agnostic part of the Wasmtime WebRTC host out of
//! the demo binaries so any host can satisfy the `polymorph:webrtc-datachannels`
//! imports with one call to [`add_to_linker`]. It is a wasip3 (component-model
//! async) implementation modeled after [`wasmtime_wasi_http::p3`]: a host embeds
//! a [`WebrtcCtx`] in its store state, implements [`WebrtcView`] to
//! expose it alongside the store's [`ResourceTable`], and calls
//! [`add_to_linker`] to satisfy the `types` and `connections` imports with a
//! real WebRTC/SCTP data channel.
//!
//! A host generating its own bindings with `wasmtime::component::bindgen!`
//! must map the `connections` resources onto this crate's host types:
//!
//! ```text
//! with: {
//!     "polymorph:webrtc-datachannels/connections.data-channel-options":
//!         wasmtime_webrtc_datachannels::DataChannelOptions,
//!     "polymorph:webrtc-datachannels/connections.data-channel":
//!         wasmtime_webrtc_datachannels::DataChannel,
//!     "polymorph:webrtc-datachannels/connections.peer-connection":
//!         wasmtime_webrtc_datachannels::PeerConnection,
//! },
//! ```
//!
//! The crate has no tests of its own: its behavior is asserted end to end by
//! the conformance suite (`conformance/`) and the demo-host integration tests
//! (`examples/wasmtime-demo/tests`).
//!
//! [`wasmtime_wasi_http::p3`]: https://docs.rs/wasmtime-wasi-http

pub mod bindings;
mod data_channel;
mod error;
mod host;
mod peer_connection;
mod state_watch;

pub use data_channel::{DataChannel, DEFAULT_MAX_INBOUND_BUFFER_BYTES};
pub use error::{WebrtcError, WebrtcResult};
pub use peer_connection::PeerConnection;

use std::sync::Arc;

use wasmtime::component::{HasData, Linker, ResourceTable};
use webrtc::peer_connection::SettingEngineBuilder;

/// A hook run against a fresh [`SettingEngineBuilder`] before each peer
/// connection is created. See [`WebrtcCtx::set_setting_engine_hook`].
pub type SettingEngineHook =
    Arc<dyn Fn(SettingEngineBuilder) -> SettingEngineBuilder + Send + Sync>;

/// A STUN/TURN server a peer connection may gather server-reflexive and relay
/// candidates from. Mirrors `webrtc-rs`'s `RTCIceServer`; `username`/`credential`
/// are ignored for STUN-only URLs.
#[derive(Clone, Debug, Default)]
pub struct WebrtcIceServer {
    /// STUN/TURN URLs, e.g. `stun:host:3478` or `turn:host:3478?transport=udp`.
    pub urls: Vec<String>,
    /// TURN long-term-credential username (empty for STUN-only servers).
    pub username: String,
    /// TURN long-term-credential secret (empty for STUN-only servers).
    pub credential: String,
}

/// Network/ICE configuration applied when a peer connection is built.
///
/// The default value reproduces the crate's built-in behavior: bind a single
/// ephemeral UDP socket on IPv4 loopback, no STUN/TURN servers, and the `all`
/// ICE transport policy. The conformance netns lab (see `conformance/README.md`)
/// overrides these to bind a scenario-specific interface address and to
/// point at a STUN/TURN server, forcing server-reflexive or relay candidate
/// paths.
#[derive(Clone, Debug, Default)]
pub struct WebrtcIceConfig {
    /// UDP socket addresses to bind and gather host candidates from. When empty
    /// the crate binds its default (`127.0.0.1:0`). Use a `:0` port to let the
    /// OS choose an ephemeral port.
    pub udp_addrs: Vec<String>,
    /// STUN/TURN servers to gather server-reflexive and relay candidates from.
    pub ice_servers: Vec<WebrtcIceServer>,
    /// When `true`, only TURN relay candidates are used (the `relay` ICE
    /// transport policy); requires at least one TURN server in `ice_servers`.
    pub relay_only: bool,
}

impl WebrtcIceConfig {
    /// True when this configuration leaves every field at its default, in which
    /// case the crate's built-in loopback behavior is used unchanged.
    pub fn is_default(&self) -> bool {
        self.udp_addrs.is_empty() && self.ice_servers.is_empty() && !self.relay_only
    }
}

/// Configuration and per-store state for the WebRTC data-channel host.
///
/// This is intentionally minimal (mirroring `wasmtime_wasi_http`'s
/// `WasiHttpCtx`); it exists so hosts have a stable place to grow configuration
/// without changing the [`WebrtcView`] shape.
///
/// The knobs so far: the [`SettingEngineBuilder`] hook (see
/// [`set_setting_engine_hook`](Self::set_setting_engine_hook)), the analogue
/// of wasmtime-wasi-http's `WasiHttpHooks`; the [`WebrtcIceConfig`] ICE
/// server configuration (see [`set_ice_config`](Self::set_ice_config)); the
/// `wait-connected` timeout (see
/// [`set_connect_timeout`](Self::set_connect_timeout)); and the per-channel
/// inbound buffer bound (see
/// [`set_max_inbound_buffer_bytes`](Self::set_max_inbound_buffer_bytes)). The
/// crate reads no ambient environment: every knob is set through this context
/// by the embedding host, which owns any env-driven configuration.
#[derive(Clone)]
#[non_exhaustive]
pub struct WebrtcCtx {
    setting_engine_hook: Option<SettingEngineHook>,
    ice_config: WebrtcIceConfig,
    connect_timeout: std::time::Duration,
    max_inbound_buffer_bytes: usize,
}

impl Default for WebrtcCtx {
    fn default() -> Self {
        Self {
            setting_engine_hook: None,
            ice_config: WebrtcIceConfig::default(),
            connect_timeout: peer_connection::DEFAULT_CONNECT_TIMEOUT,
            max_inbound_buffer_bytes: data_channel::DEFAULT_MAX_INBOUND_BUFFER_BYTES,
        }
    }
}

impl std::fmt::Debug for WebrtcCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebrtcCtx")
            .field(
                "setting_engine_hook",
                &self.setting_engine_hook.as_ref().map(|_| "<hook>"),
            )
            .field("ice_config", &self.ice_config)
            .finish()
    }
}

impl WebrtcCtx {
    /// Create a new, default context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook run against a fresh [`SettingEngineBuilder`] before
    /// each peer connection this context creates.
    ///
    /// This is the customization point for `webrtc-rs` behavior the crate does
    /// not opt into itself (mirroring wasmtime-wasi-http's `WasiHttpHooks`). For
    /// example, two peers sharing one host may only reach each other over
    /// loopback, so a demo/test host can enable loopback ICE candidates:
    ///
    /// ```
    /// # use wasmtime_webrtc_datachannels::WebrtcCtx;
    /// let mut ctx = WebrtcCtx::new();
    /// ctx.set_setting_engine_hook(|engine| engine.with_include_loopback_candidate(true));
    /// ```
    pub fn set_setting_engine_hook(
        &mut self,
        hook: impl Fn(SettingEngineBuilder) -> SettingEngineBuilder + Send + Sync + 'static,
    ) {
        self.setting_engine_hook = Some(Arc::new(hook));
    }

    /// The registered [`SettingEngineBuilder`] hook, if any, cheaply cloned so callers
    /// can apply it without holding a borrow of the context.
    pub fn setting_engine_hook(&self) -> Option<SettingEngineHook> {
        self.setting_engine_hook.clone()
    }

    /// Set the network/ICE configuration applied to every peer connection this
    /// context creates (bind addresses, STUN/TURN servers, relay-only policy).
    ///
    /// The default leaves the crate's built-in loopback behavior unchanged; the
    /// conformance netns lab overrides it per scenario (see
    /// `conformance/README.md`).
    pub fn set_ice_config(&mut self, config: WebrtcIceConfig) {
        self.ice_config = config;
    }

    /// The configured network/ICE configuration, cheaply cloned so callers can
    /// apply it without holding a borrow of the context.
    pub fn ice_config(&self) -> WebrtcIceConfig {
        self.ice_config.clone()
    }

    /// Set how long `peer-connection.wait-connected` waits before failing with
    /// `error.timed-out` (the WIT leaves the bound implementation-defined).
    /// Default: 30 seconds.
    pub fn set_connect_timeout(&mut self, timeout: std::time::Duration) {
        self.connect_timeout = timeout;
    }

    /// The configured `wait-connected` timeout.
    pub fn connect_timeout(&self) -> std::time::Duration {
        self.connect_timeout
    }

    /// Set the per-channel inbound buffer bound, in payload bytes (see the
    /// `data-channel` WIT docs for the overflow contract). Default:
    /// [`DEFAULT_MAX_INBOUND_BUFFER_BYTES`]. The crate itself never reads the
    /// environment; a host offering the bound as an env knob reads and
    /// validates the value itself and applies it here.
    pub fn set_max_inbound_buffer_bytes(&mut self, bytes: usize) {
        self.max_inbound_buffer_bytes = bytes;
    }

    /// The configured per-channel inbound buffer bound.
    pub fn max_inbound_buffer_bytes(&self) -> usize {
        self.max_inbound_buffer_bytes
    }
}

/// A borrowed view into a host's [`WebrtcCtx`] and its [`ResourceTable`].
///
/// Returned by [`WebrtcView::webrtc`], this is the [`HasData::Data`] the
/// generated host bindings operate on.
pub struct WebrtcCtxView<'a> {
    /// Mutable reference to the WebRTC host context.
    pub ctx: &'a mut WebrtcCtx,
    /// Mutable reference to the table used to manage host resources.
    pub table: &'a mut ResourceTable,
}

/// A trait that provides access to the [`WebrtcCtx`] host state.
///
/// Implement this for your store's data type so [`add_to_linker`] can wire the
/// `polymorph:webrtc-datachannels` imports onto your linker.
pub trait WebrtcView: Send {
    /// Return a [`WebrtcCtxView`] from a mutable reference to `self`.
    fn webrtc(&mut self) -> WebrtcCtxView<'_>;
}

/// The type for which this crate implements the `polymorph:webrtc-datachannels`
/// interfaces. Used as the [`HasData`] marker for the generated bindings.
pub struct Webrtc;

impl HasData for Webrtc {
    type Data<'a> = WebrtcCtxView<'a>;
}

/// Backing type for the `connections.data-channel-options` resource.
///
/// A plain configuration builder (mirroring `wasi:http`'s `request-options`):
/// the guest constructs a default value through the imported constructor,
/// adjusts the fields through the setters, then hands the resource to a
/// data-channel-creating function such as
/// `peer-connection.create-data-channel`. The host that receives the resource
/// reads these fields back to configure the `webrtc-rs` channel.
#[derive(Clone, Debug)]
pub struct DataChannelOptions {
    /// The channel label. Both peers observe the same label.
    pub label: String,
    /// Whether messages are delivered in order.
    pub ordered: bool,
    /// The maximum number of retransmissions before a message is dropped, or
    /// `None` for fully reliable delivery.
    pub max_retransmits: Option<u16>,
}

impl Default for DataChannelOptions {
    fn default() -> Self {
        Self {
            label: String::new(),
            ordered: true,
            max_retransmits: None,
        }
    }
}

/// Host state behind a `peer-connection-config` resource.
///
/// A configuration builder like [`DataChannelOptions`], but with fallible
/// setters per the WIT contract: capability-gated options are rejected
/// eagerly, so a config a connection is constructed with was accepted in
/// full. This host supports STUN/TURN servers and the `relay` policy (both
/// map onto [`WebrtcIceConfig`]), so its setters validate rather than
/// reject: a malformed server entry fails `invalid`.
#[derive(Clone, Debug, Default)]
pub struct PeerConnectionConfig {
    /// The accepted STUN/TURN servers.
    pub ice_servers: Vec<WebrtcIceServer>,
    /// Whether only relay (TURN) candidates may be used.
    pub relay_only: bool,
}

/// Add the `polymorph:webrtc-datachannels` interfaces implemented by this crate
/// (`types` and `connections`) to the provided [`Linker`].
///
/// The store's data type `T` must implement [`WebrtcView`]. The engine's
/// [`Config`](wasmtime::Config) must have `wasm_component_model_async` enabled,
/// since the `send`/`receive` methods use the component-model async ABI.
///
/// # Example
///
/// ```no_run
/// use wasmtime::component::{Linker, ResourceTable};
/// use wasmtime::{Engine, Result};
/// use wasmtime_webrtc_datachannels::{
///     add_to_linker, WebrtcCtx, WebrtcCtxView, WebrtcView,
/// };
///
/// struct MyState {
///     webrtc: WebrtcCtx,
///     table: ResourceTable,
/// }
///
/// impl WebrtcView for MyState {
///     fn webrtc(&mut self) -> WebrtcCtxView<'_> {
///         WebrtcCtxView {
///             ctx: &mut self.webrtc,
///             table: &mut self.table,
///         }
///     }
/// }
///
/// fn wire(linker: &mut Linker<MyState>) -> Result<()> {
///     add_to_linker(linker)
/// }
/// ```
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: WebrtcView + 'static,
{
    bindings::webrtc_datachannels::types::add_to_linker::<_, Webrtc>(linker, T::webrtc)?;
    bindings::webrtc_datachannels::connections::add_to_linker::<_, Webrtc>(linker, T::webrtc)?;
    Ok(())
}
