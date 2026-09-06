//! The `webrtc-rs`-backed [`PeerConnection`] host resource.
//!
//! [`PeerConnection`] is the concrete host type mapped onto the
//! `polymorph:webrtc-datachannels/connections.peer-connection` resource: the
//! guest-driven connection surface (`create-offer`/`create-answer`,
//! `set-local-description`/`set-remote-description`, trickled ICE candidates)
//! that lets a guest connect two separate peers.
//!
//! ## Deferred wiring
//!
//! The WIT `constructor` and `create-data-channel` are **synchronous**, but a
//! `webrtc-rs` peer connection can only be built on a running Tokio
//! runtime (`webrtc-rs` panics if constructed without one). The constructor
//! therefore spawns a build task and hands back a resource immediately; every
//! async method awaits the shared "built" future before touching the peer
//! connection. `create-data-channel` likewise spawns a task that opens and wires
//! the channel once the peer connection exists, returning a
//! [`DataChannel::deferred`] whose transport is filled in when the channel
//! opens.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{FutureExt, Shared};
use futures::StreamExt as _;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use webrtc::data_channel::RTCDataChannelInit;
use webrtc::peer_connection::{
    PeerConnection as WebrtcPeerConnection, RTCIceCandidateInit, RTCPeerConnectionState,
    RTCSessionDescription,
};

use anyhow::{anyhow, Result};
use webrtc::data_channel::DataChannel as WebrtcDataChannel;
use webrtc::peer_connection::{
    PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceServer,
    RTCIceTransportPolicy, SettingEngineBuilder,
};
use webrtc::runtime::default_runtime;

use crate::data_channel::{
    close_signal, spawn_channel_wiring, wire_open_channel, wiring_channel, CloseSignal,
    CloseTrigger,
};
use crate::error::{WebrtcError, WebrtcResult};
use crate::{DataChannel, SettingEngineHook};

/// How long [`PeerConnection::wait_connected`] waits before reporting a
/// timeout, unless overridden through `WebrtcCtx::set_connect_timeout`.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`PeerConnection::close`] keeps the underlying connection alive
/// after the close is observed locally, so messages already handed to the
/// transport flush to the wire before teardown discards the SCTP send queue.
/// Long enough for queued sends on any sane path, short enough that an
/// unresponsive peer cannot hold resources meaningfully longer.
const CLOSE_DRAIN: Duration = Duration::from_secs(1);

/// The connection lifecycle as observed by the host, backing
/// `peer-connection.state-changes` (mapped onto the WIT `connection-state` at
/// the binding layer). `Failed` and `Closed` are terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionPhase {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// The kind of SDP description passed to `set-local-description` /
/// `set-remote-description`, mirroring the applicable `session-description`
/// variants (`rollback` is rejected before reaching the host).
#[derive(Clone, Copy, Debug)]
pub enum SdpKind {
    /// An SDP offer.
    Offer,
    /// An SDP answer.
    Answer,
    /// A provisional SDP answer.
    Pranswer,
}

/// A locally gathered ICE candidate to trickle to the remote peer.
#[derive(Clone, Debug)]
pub struct LocalCandidate {
    /// The `candidate` attribute value.
    pub candidate: String,
    /// The media stream identification tag, if any.
    pub sdp_mid: Option<String>,
    /// The index of the media description this candidate is associated with.
    pub sdp_mline_index: Option<u16>,
}

/// A future resolving to the built peer connection, or its build error.
/// Shared so every async method observes the same outcome.
type BuiltFuture =
    Shared<Pin<Box<dyn Future<Output = WebrtcResult<Arc<dyn WebrtcPeerConnection>>> + Send>>>;

/// Connection-state signalling shared with the `webrtc-rs` state-change handler.
#[derive(Default)]
struct ConnState {
    /// Set once the connection reaches `connected`.
    connected: AtomicBool,
    /// Set once the connection reaches `failed` or `closed`.
    failed: AtomicBool,
    /// Woken on every state transition so `wait_connected` can re-check.
    notify: Notify,
}

/// Shared inner state behind a `peer-connection` resource.
struct Inner {
    /// Resolves to the built peer connection once the spawned build task runs.
    built: BuiltFuture,
    /// The locally gathered ICE candidates, taken by `local-ice-candidates`.
    candidates: Mutex<Option<UnboundedReceiver<LocalCandidate>>>,
    /// Channels opened by the remote peer, taken by `incoming-data-channels`.
    incoming: Mutex<Option<UnboundedReceiver<DataChannel>>>,
    /// Connection-state signalling for `wait_connected`.
    state: Arc<ConnState>,
    /// The number of `create-data-channel` calls whose spawned registration
    /// task has not yet reached `webrtc-rs`. `create-offer` / `create-answer`
    /// wait for this to reach zero so the produced SDP covers every channel
    /// the guest created before asking for it.
    pending_channels: Arc<PendingOps>,
    /// The built peer connection, retained so `close` (and `Drop`) can tear down
    /// its `webrtc-rs` background tasks. Taken on the first close.
    pc: Arc<Mutex<Option<Arc<dyn WebrtcPeerConnection>>>>,
    /// Fires the connection-close signal every owned data channel observes.
    close_trigger: CloseTrigger,
    /// The signal handed to each data channel this connection creates/adopts.
    close_signal: CloseSignal,
    /// How long `wait-connected` waits before reporting a timeout.
    connect_timeout: Duration,
    /// The per-channel inbound buffer bound, in payload bytes.
    max_inbound_buffer_bytes: usize,
    /// The connection lifecycle, backing `state-changes`.
    phase: Arc<crate::state_watch::StateWatch<ConnectionPhase>>,
    /// Take-once claim for `state-changes` (the WIT contract).
    state_taken: AtomicBool,
}

/// A counter of in-flight spawned operations, awaitable at zero.
#[derive(Default)]
struct PendingOps {
    count: std::sync::atomic::AtomicUsize,
    notify: Notify,
}

impl PendingOps {
    /// Record one newly spawned operation.
    fn begin(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    /// Record one finished operation, waking any waiters.
    fn end(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Resolve once no operations are in flight.
    async fn settled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Arm the notification before checking, so an `end` between the
            // check and the wait is not missed.
            notified.as_mut().enable();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Mirror `close()`: fire the close signal (any surviving `data-channel`
        // resources observe `error.closed`) and defer the network teardown by
        // the same bounded drain, so a message handed to the transport just
        // before the resource was dropped still flushes to the wire rather
        // than being discarded with the SCTP send queue.
        self.phase.set(ConnectionPhase::Closed);
        self.close_trigger.fire();
        let pc = self.pc.lock().unwrap().take();
        if pc.is_none() {
            return;
        }
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(CLOSE_DRAIN).await;
                close_peer_connections(pc.into_iter().collect());
            });
        } else {
            close_peer_connections(pc.into_iter().collect());
        }
    }
}

/// Host state behind a `peer-connection` resource.
///
/// Cheaply cloneable (an `Arc` around the shared state) so host methods can hold
/// a handle without borrowing the resource table across `.await`.
#[derive(Clone)]
pub struct PeerConnection {
    inner: Arc<Inner>,
}

impl PeerConnection {
    /// Construct a peer connection, spawning the `webrtc-rs` build task.
    ///
    /// `hook` customizes the [`SettingEngineBuilder`](webrtc::peer_connection::SettingEngineBuilder)
    /// before the connection is built, and `ice` (bind addresses, STUN/TURN
    /// servers, relay-only policy) is applied when it is built.
    /// `connect_timeout` bounds `wait-connected` and
    /// `max_inbound_buffer_bytes` bounds each channel's inbound buffering
    /// (both configured through `WebrtcCtx`). Requires a
    /// running Tokio runtime; without one every subsequent operation fails.
    pub fn new_with(
        hook: Option<SettingEngineHook>,
        ice: crate::WebrtcIceConfig,
        connect_timeout: Duration,
        max_inbound_buffer_bytes: usize,
    ) -> Self {
        let (built_tx, built_rx) =
            oneshot::channel::<WebrtcResult<Arc<dyn WebrtcPeerConnection>>>();
        let (cand_tx, cand_rx) = mpsc::unbounded::<LocalCandidate>();
        let (inc_tx, inc_rx) = mpsc::unbounded::<DataChannel>();
        let state = Arc::new(ConnState::default());
        let pc_slot: Arc<Mutex<Option<Arc<dyn WebrtcPeerConnection>>>> = Arc::new(Mutex::new(None));
        let (close_trigger, close_sig) = close_signal();
        let phase = Arc::new(crate::state_watch::StateWatch::new(
            ConnectionPhase::New,
            |p| matches!(p, ConnectionPhase::Failed | ConnectionPhase::Closed),
        ));

        if let Ok(handle) = Handle::try_current() {
            let state = state.clone();
            let pc_slot = pc_slot.clone();
            let trigger = close_trigger.clone();
            let signal = close_sig.clone();
            let phase = phase.clone();
            handle.spawn(async move {
                let handler = connection_handler(
                    cand_tx,
                    inc_tx,
                    state,
                    trigger,
                    signal.clone(),
                    max_inbound_buffer_bytes,
                    phase.clone(),
                );
                match new_peer_connection_with(
                    |engine| match &hook {
                        Some(hook) => hook(engine),
                        None => engine,
                    },
                    ice,
                    handler,
                )
                .await
                {
                    Ok(pc) => {
                        *pc_slot.lock().unwrap() = Some(pc.clone());
                        // `close()` may have raced the build: it found the slot
                        // empty, so tear the connection down now that it exists.
                        if signal.is_closed() {
                            let taken = pc_slot.lock().unwrap().take();
                            close_peer_connections(taken.into_iter().collect());
                        }
                        let _ = built_tx.send(Ok(pc));
                    }
                    Err(err) => {
                        // The connection can never connect; terminally over.
                        phase.set(ConnectionPhase::Failed);
                        let _ = built_tx.send(Err(WebrtcError::other(err)));
                    }
                }
            });
        } else {
            phase.set(ConnectionPhase::Failed);
            let _ = built_tx.send(Err(WebrtcError::msg(
                "peer connection requires a running tokio runtime",
            )));
        }

        let built = async move {
            built_rx
                .await
                .unwrap_or_else(|_| Err(WebrtcError::msg("peer connection build was cancelled")))
        }
        .boxed()
        .shared();

        Self {
            inner: Arc::new(Inner {
                built,
                candidates: Mutex::new(Some(cand_rx)),
                incoming: Mutex::new(Some(inc_rx)),
                state,
                pending_channels: Arc::new(PendingOps::default()),
                pc: pc_slot,
                close_trigger,
                close_signal: close_sig,
                connect_timeout,
                max_inbound_buffer_bytes,
                phase,
                state_taken: AtomicBool::new(false),
            }),
        }
    }

    /// The connection's lifecycle watch, backing `state-changes`.
    pub(crate) fn state_watch(&self) -> Arc<crate::state_watch::StateWatch<ConnectionPhase>> {
        self.inner.phase.clone()
    }

    /// Claim `state-changes` (take-once): `true` for the first caller only.
    pub(crate) fn take_state_stream(&self) -> bool {
        !self.inner.state_taken.swap(true, Ordering::SeqCst)
    }

    /// Await the built peer connection (or its build error).
    async fn pc(&self) -> WebrtcResult<Arc<dyn WebrtcPeerConnection>> {
        self.inner.built.clone().await
    }

    /// Whether the connection is terminally over: closed by [`close`] or
    /// failed. Per the WIT contract, methods called after that point fail
    /// with `error.closed`.
    pub fn is_closed(&self) -> bool {
        self.inner.close_signal.is_closed()
    }

    /// Gate a method on the connection being open (see [`is_closed`]).
    fn ensure_open(&self) -> WebrtcResult<()> {
        if self.is_closed() {
            return Err(WebrtcError::Closed);
        }
        Ok(())
    }

    /// Create a data channel to negotiate in-band with the peer.
    ///
    /// Returns immediately with a [`DataChannel`] whose transport is wired once
    /// the peer connection is built and the channel opens.
    pub fn create_data_channel(
        &self,
        label: String,
        ordered: bool,
        max_retransmits: Option<u16>,
    ) -> DataChannel {
        let (wire_tx, wired) = wiring_channel();
        let built = self.inner.built.clone();
        let channel_label = label.clone();
        let max_inbound_buffer_bytes = self.inner.max_inbound_buffer_bytes;
        if let Ok(handle) = Handle::try_current() {
            let pending = self.inner.pending_channels.clone();
            pending.begin();
            handle.spawn(async move {
                let pc = match built.await {
                    Ok(pc) => pc,
                    Err(err) => {
                        pending.end();
                        let _ = wire_tx.send(Err(err));
                        return;
                    }
                };
                let init = RTCDataChannelInit {
                    ordered,
                    max_retransmits,
                    ..Default::default()
                };
                let created = pc.create_data_channel(&channel_label, Some(init)).await;
                // The channel is registered with the peer connection (or has
                // failed) as soon as `create_data_channel` returns, so an offer
                // produced from here on covers it.
                pending.end();
                match created {
                    Ok(channel) => spawn_channel_wiring(channel, wire_tx, max_inbound_buffer_bytes),
                    Err(err) => {
                        let _ = wire_tx.send(Err(WebrtcError::other(err)));
                    }
                }
            });
        } else {
            let _ = wire_tx.send(Err(WebrtcError::msg(
                "peer connection requires a running tokio runtime",
            )));
        }
        DataChannel::deferred(label, wired, self.inner.close_signal.clone())
    }

    /// Take the locally gathered ICE candidate stream. Returns `None` if it has
    /// already been taken (`local-ice-candidates` is meant to be called once).
    pub fn take_local_candidates(&self) -> Option<UnboundedReceiver<LocalCandidate>> {
        self.inner.candidates.lock().unwrap().take()
    }

    /// Take the remote-opened data-channel stream. Returns `None` if it has
    /// already been taken (`incoming-data-channels` is meant to be called once).
    pub fn take_incoming_channels(&self) -> Option<UnboundedReceiver<DataChannel>> {
        self.inner.incoming.lock().unwrap().take()
    }

    /// Produce an SDP offer. The caller applies it via `set-local-description`.
    pub async fn create_offer(&self) -> WebrtcResult<String> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        // Wait for any spawned `create-data-channel` registrations, so the
        // offer's SDP covers every channel created before this call.
        self.inner.pending_channels.settled().await;
        pc.create_offer(None)
            .await
            .map(|desc| desc.sdp)
            .map_err(WebrtcError::other)
    }

    /// Produce an SDP answer to a previously set remote offer.
    pub async fn create_answer(&self) -> WebrtcResult<String> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        // Wait for any spawned `create-data-channel` registrations, so the
        // answer's SDP covers every channel created before this call.
        self.inner.pending_channels.settled().await;
        pc.create_answer(None)
            .await
            .map(|desc| desc.sdp)
            .map_err(WebrtcError::other)
    }

    /// Apply a local description, starting ICE gathering (and, in turn, the
    /// trickled `local-ice-candidates`).
    pub async fn set_local_description(&self, kind: SdpKind, sdp: String) -> WebrtcResult<()> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        let desc = to_rtc_description(kind, sdp)?;
        pc.set_local_description(desc)
            .await
            .map_err(WebrtcError::other)
    }

    /// Apply the remote peer's description.
    pub async fn set_remote_description(&self, kind: SdpKind, sdp: String) -> WebrtcResult<()> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        let desc = to_rtc_description(kind, sdp)?;
        pc.set_remote_description(desc)
            .await
            .map_err(WebrtcError::other)
    }

    /// Add an ICE candidate received from the remote peer.
    pub async fn add_ice_candidate(
        &self,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> WebrtcResult<()> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment: None,
            url: None,
        };
        pc.add_ice_candidate(init)
            .await
            .map_err(WebrtcError::invalid_signaling)
    }

    /// Resolve once the connection reaches `connected`, or report a timeout /
    /// failure.
    pub async fn wait_connected(&self) -> WebrtcResult<()> {
        self.pc().await?;
        let state = self.inner.state.clone();
        let deadline = tokio::time::sleep(self.inner.connect_timeout);
        tokio::pin!(deadline);
        loop {
            let notified = state.notify.notified();
            tokio::pin!(notified);
            // Arm the notification before checking, so a transition between the
            // check and the wait is not missed.
            notified.as_mut().enable();
            if state.connected.load(Ordering::SeqCst) {
                return Ok(());
            }
            if state.failed.load(Ordering::SeqCst) {
                return Err(WebrtcError::Closed);
            }
            tokio::select! {
                _ = &mut notified => continue,
                _ = &mut deadline => return Err(WebrtcError::TimedOut),
            }
        }
    }

    /// Close the peer connection, tearing down its `webrtc-rs` background tasks.
    /// Idempotent.
    ///
    /// The close is observed **locally** at once — the close signal fires
    /// first, so pending and subsequent operations on this side resolve with
    /// `error.closed` immediately — but the network teardown is deferred by a
    /// bounded [`CLOSE_DRAIN`] grace: `webrtc-rs`'s `close()` discards the
    /// SCTP send queue, so a message accepted by `send` just before `close`
    /// (for example a rendezvous sentinel the remote peer still needs) would
    /// otherwise be lost before it reaches the wire.
    pub fn close(&self) {
        // Fire the close signal first so pending channel operations resolve
        // with `error.closed` (the `webrtc` 0.21 wrapper reports nothing to the
        // channels itself), then tear down the connection after the drain.
        // `close` wins over a later `failed` callback: the phase watch treats
        // `closed` as terminal.
        self.inner.phase.set(ConnectionPhase::Closed);
        self.inner.close_trigger.fire();
        let pc = self.inner.pc.lock().unwrap().take();
        if pc.is_none() {
            return;
        }
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(CLOSE_DRAIN).await;
                close_peer_connections(pc.into_iter().collect());
            });
        } else {
            // No runtime to defer on: tear down immediately (the connection's
            // own runtime is gone, so nothing could flush anyway).
            close_peer_connections(pc.into_iter().collect());
        }
    }
}

/// Build the [`PeerConnectionEventHandler`](webrtc::peer_connection::PeerConnectionEventHandler)
/// that feeds the guest-facing streams and connection-state signalling.
///
/// The `webrtc` 0.21 builder takes one handler at build time, so all callbacks
/// are assembled here into a single [`CallbackHandler`]:
///
/// - each locally gathered ICE candidate is trickled onto `cand_tx`, and the
///   stream is ended (the sender dropped) once ICE gathering completes;
/// - each remote-opened data channel is wired and pushed onto `inc_tx`;
/// - connection-state transitions drive `wait_connected` via `state`.
fn connection_handler(
    cand_tx: UnboundedSender<LocalCandidate>,
    inc_tx: UnboundedSender<DataChannel>,
    state: Arc<ConnState>,
    close_trigger: CloseTrigger,
    close_sig: CloseSignal,
    max_inbound_buffer_bytes: usize,
    phase: Arc<crate::state_watch::StateWatch<ConnectionPhase>>,
) -> Arc<CallbackHandler> {
    let cand_tx = Arc::new(Mutex::new(Some(cand_tx)));
    let gather_cand_tx = cand_tx.clone();
    Arc::new(
        CallbackHandler::new()
            .on_ice_candidate(move |event| {
                if let Ok(init) = event.candidate.to_json() {
                    if let Some(tx) = cand_tx.lock().unwrap().as_ref() {
                        let _ = tx.unbounded_send(LocalCandidate {
                            candidate: init.candidate,
                            sdp_mid: init.sdp_mid,
                            sdp_mline_index: init.sdp_mline_index,
                        });
                    }
                }
            })
            .on_gathering_complete(move || {
                gather_cand_tx.lock().unwrap().take();
            })
            .on_data_channel({
                // Deliver incoming channels in the order they open (the WIT
                // contract): the callback forwards each channel synchronously
                // onto an ordered queue, and one dispatcher task per
                // connection awaits the async `label()` and wires them
                // sequentially — a per-channel task here could reorder two
                // channels opened in quick succession.
                let (raw_tx, mut raw_rx) = mpsc::unbounded::<Arc<dyn WebrtcDataChannel>>();
                if Handle::try_current().is_ok() {
                    tokio::spawn(async move {
                        while let Some(channel) = raw_rx.next().await {
                            let label = channel.label().await.unwrap_or_default();
                            let wired = wire_open_channel(channel, max_inbound_buffer_bytes);
                            let _ = inc_tx.unbounded_send(DataChannel::deferred(
                                label,
                                wired,
                                close_sig.clone(),
                            ));
                        }
                    });
                }
                move |channel| {
                    let _ = raw_tx.unbounded_send(channel);
                }
            })
            .on_connection_state(move |s| {
                // Feed the lifecycle watch (`state-changes`) from every
                // transition; terminal states latch there.
                if let Some(p) = match s {
                    RTCPeerConnectionState::New => Some(ConnectionPhase::New),
                    RTCPeerConnectionState::Connecting => Some(ConnectionPhase::Connecting),
                    RTCPeerConnectionState::Connected => Some(ConnectionPhase::Connected),
                    RTCPeerConnectionState::Disconnected => Some(ConnectionPhase::Disconnected),
                    RTCPeerConnectionState::Failed => Some(ConnectionPhase::Failed),
                    RTCPeerConnectionState::Closed => Some(ConnectionPhase::Closed),
                    _ => None,
                } {
                    phase.set(p);
                }
                match s {
                    RTCPeerConnectionState::Connected => {
                        state.connected.store(true, Ordering::SeqCst);
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        state.failed.store(true, Ordering::SeqCst);
                        // A failed/closed connection closes its channels too.
                        close_trigger.fire();
                    }
                    _ => {}
                }
                state.notify.notify_waiters();
            }),
    )
}

/// Build a `webrtc-rs` session description from a [`SdpKind`] and SDP string.
/// A description that fails to parse is invalid signaling.
fn to_rtc_description(kind: SdpKind, sdp: String) -> WebrtcResult<RTCSessionDescription> {
    let result = match kind {
        SdpKind::Offer => RTCSessionDescription::offer(sdp),
        SdpKind::Answer => RTCSessionDescription::answer(sdp),
        SdpKind::Pranswer => RTCSessionDescription::pranswer(sdp),
    };
    result.map_err(WebrtcError::invalid_signaling)
}

/// Close each peer connection so `webrtc-rs` tears down its ICE/DTLS/SCTP
/// background tasks.
///
/// [`WebrtcPeerConnection::close`] is async, so the closes are spawned onto the
/// current Tokio runtime when one is running; dropping the `Arc`s alone would
/// leak those tasks for the process lifetime. Called from `Drop` impls, where
/// awaiting is not possible. When no runtime is running (a resource dropped
/// after the host's runtime has shut down), the closes run to completion on a
/// dedicated thread with its own small runtime, so cleanup does not silently
/// depend on the caller's runtime still being alive.
fn close_peer_connections(connections: Vec<Arc<dyn WebrtcPeerConnection>>) {
    if connections.is_empty() {
        return;
    }
    let close_all = async move {
        for connection in connections {
            let _ = connection.close().await;
        }
    };
    if let Ok(handle) = Handle::try_current() {
        handle.spawn(close_all);
    } else {
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(close_all);
            }
        });
    }
}

/// Create a peer connection with an explicit [`WebrtcIceConfig`](crate::WebrtcIceConfig)
/// controlling the UDP bind addresses, STUN/TURN servers, and ICE transport
/// policy, giving the caller a chance to customize the `webrtc-rs`
/// [`SettingEngineBuilder`] first and supplying the event `handler` that receives its
/// callbacks (the `webrtc` 0.21 builder takes a single
/// [`PeerConnectionEventHandler`] at build time). A default config binds IPv4
/// loopback; the conformance netns lab (see `conformance/README.md`) overrides
/// it per scenario to exercise host, server-reflexive, and relay candidate
/// paths.
/// Size of the dedicated reactor pool every peer-connection driver runs
/// on (`webrtc`'s `spawn_reactor`: a bounded set of single-threaded
/// runtimes on their own OS threads; drivers are pinned round-robin and
/// their I/O binds to the pool thread). Drivers never share the
/// embedder's runtime, so a driver that stalls — or spins, which a
/// sans-IO core reporting a timer its handler cannot advance made one
/// do, at millions of loop passes per second — costs a pool thread,
/// not the host. Keeping the host responsive is also what lets such a
/// wedge self-heal: the resource's owner still runs, observes the
/// failure, and drops the connection, whose `closing` check ends the
/// driver loop. Two threads bound the process cost while keeping one
/// wedged driver from pausing every other connection's driver.
const DRIVER_REACTOR_POOL_SIZE: usize = 2;

async fn new_peer_connection_with(
    configure: impl FnOnce(SettingEngineBuilder) -> SettingEngineBuilder,
    ice: crate::WebrtcIceConfig,
    handler: Arc<dyn PeerConnectionEventHandler>,
) -> Result<Arc<dyn WebrtcPeerConnection>> {
    let setting = configure(SettingEngineBuilder::new()).build();
    let runtime = default_runtime().ok_or_else(|| anyhow!("no async runtime found"))?;

    // Bind the scenario-specified interface addresses, or the crate default.
    let udp_addrs: Vec<String> = if ice.udp_addrs.is_empty() {
        vec!["127.0.0.1:0".to_string()]
    } else {
        ice.udp_addrs.clone()
    };

    // Assemble the RTCConfiguration from the scenario's STUN/TURN servers and
    // transport policy. An all-default config yields an empty builder, matching
    // the previous `RTCConfigurationBuilder::new().build()`.
    let mut config = RTCConfigurationBuilder::new();
    if !ice.ice_servers.is_empty() {
        config = config.with_ice_servers(
            ice.ice_servers
                .iter()
                .map(|server| RTCIceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
        );
    }
    if ice.relay_only {
        config = config.with_ice_transport_policy(RTCIceTransportPolicy::Relay);
    }

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config.build())
        .with_setting_engine(setting)
        .with_handler(handler)
        .with_runtime(runtime)
        .with_udp_addrs(udp_addrs)
        .with_dedicated_reactor_pool_size(DRIVER_REACTOR_POOL_SIZE)
        .build()
        .await?;
    Ok(Arc::new(pc))
}

/// A [`PeerConnectionEventHandler`] built from optional callback senders.
///
/// The `webrtc` 0.21 builder takes one handler at build time; this type
/// assembles a handler from just the callbacks the connection needs without a
/// bespoke trait impl per call site.
#[allow(clippy::type_complexity)]
#[derive(Default)]
struct CallbackHandler {
    on_ice_candidate:
        Option<Box<dyn Fn(webrtc::peer_connection::RTCPeerConnectionIceEvent) + Send + Sync>>,
    on_gathering_complete: Option<Box<dyn Fn() + Send + Sync>>,
    on_data_channel: Option<Box<dyn Fn(Arc<dyn WebrtcDataChannel>) + Send + Sync>>,
    on_connection_state:
        Option<Box<dyn Fn(webrtc::peer_connection::RTCPeerConnectionState) + Send + Sync>>,
}

impl CallbackHandler {
    /// A handler with no callbacks registered.
    fn new() -> Self {
        Self::default()
    }

    /// Register a callback for each locally gathered ICE candidate.
    fn on_ice_candidate(
        mut self,
        f: impl Fn(webrtc::peer_connection::RTCPeerConnectionIceEvent) + Send + Sync + 'static,
    ) -> Self {
        self.on_ice_candidate = Some(Box::new(f));
        self
    }

    /// Register a callback fired once ICE gathering reaches `complete`.
    fn on_gathering_complete(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_gathering_complete = Some(Box::new(f));
        self
    }

    /// Register a callback for each data channel opened by the remote peer.
    fn on_data_channel(
        mut self,
        f: impl Fn(Arc<dyn WebrtcDataChannel>) + Send + Sync + 'static,
    ) -> Self {
        self.on_data_channel = Some(Box::new(f));
        self
    }

    /// Register a callback for peer-connection state transitions.
    fn on_connection_state(
        mut self,
        f: impl Fn(webrtc::peer_connection::RTCPeerConnectionState) + Send + Sync + 'static,
    ) -> Self {
        self.on_connection_state = Some(Box::new(f));
        self
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for CallbackHandler {
    async fn on_ice_candidate(&self, event: webrtc::peer_connection::RTCPeerConnectionIceEvent) {
        if let Some(f) = &self.on_ice_candidate {
            f(event);
        }
    }

    async fn on_ice_gathering_state_change(
        &self,
        state: webrtc::peer_connection::RTCIceGatheringState,
    ) {
        if state == webrtc::peer_connection::RTCIceGatheringState::Complete {
            if let Some(f) = &self.on_gathering_complete {
                f();
            }
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn WebrtcDataChannel>) {
        if let Some(f) = &self.on_data_channel {
            f(data_channel);
        }
    }

    async fn on_connection_state_change(
        &self,
        state: webrtc::peer_connection::RTCPeerConnectionState,
    ) {
        if let Some(f) = &self.on_connection_state {
            f(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The peer-connection driver must run on the dedicated reactor pool
    /// (`webrtc-rx*` threads), never on the embedder's runtime: a stalled
    /// or spinning driver then costs a pool thread, not the host — and
    /// the host staying responsive is what lets the owner drop the
    /// connection and end the wedge. The pool engagement is one easily
    /// lost builder call, and nothing else observes it; this does.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn driver_runs_on_the_dedicated_reactor_pool() {
        let _pc = PeerConnection::new_with(
            None,
            crate::WebrtcIceConfig::default(),
            Duration::from_secs(5),
            usize::MAX,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let found = std::fs::read_dir("/proc/self/task")
                .expect("/proc is available on linux")
                .filter_map(|entry| entry.ok())
                .any(|entry| {
                    std::fs::read_to_string(entry.path().join("comm"))
                        .map(|comm| comm.trim().starts_with("webrtc-rx"))
                        .unwrap_or(false)
                });
            if found {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no webrtc-rx reactor-pool thread appeared: \
                 the driver is running on the embedder's runtime"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
