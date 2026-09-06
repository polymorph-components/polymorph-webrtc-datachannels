//! The runtime-agnostic sans-I/O peer: [`SansIoPeer`].
//!
//! This wraps an [`rtc`] `RTCPeerConnection` and exposes only what a driver
//! needs — signaling primitives, the sans-I/O stepping calls
//! (`poll_transmit`/`handle_input`/`handle_timeout` and the
//! drained events), and message sends. It performs **no** I/O and awaits
//! nothing, so the same core can be fed by the in-guest `wasi:sockets` driver
//! (see [`crate::runtime`]).
//!
//! The sans-I/O model has no OS interface enumeration (`rtc` stubs `ifaces()`
//! out on wasm), so candidates are supplied explicitly by the driver
//! via [`SansIoPeer::add_local_host_candidate`] rather than gathered.

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{anyhow, Result};
use bytes::BytesMut;

use rtc::data_channel::{RTCDataChannelId, RTCDataChannelInit};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCIceCandidate, RTCIceCandidateInit,
};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};

/// One outbound UDP datagram the peer wants sent.
#[derive(Debug, Clone)]
pub struct Transmit {
    /// The datagram payload.
    pub payload: Vec<u8>,
    /// Where to send it.
    pub destination: SocketAddr,
}

/// An observable change drained from the peer after stepping it.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// The connection reached the `connected` state.
    Connected,
    /// The connection failed.
    Failed,
    /// The connection closed or disconnected.
    Closed,
    /// A data channel opened; `id` addresses it for [`SansIoPeer::send_text`] /
    /// [`SansIoPeer::send_binary`].
    ChannelOpen {
        /// The channel's id.
        id: RTCDataChannelId,
        /// The negotiated channel label.
        label: String,
    },
    /// A data channel closed (locally or by the remote peer).
    ChannelClosed {
        /// The channel's id.
        id: RTCDataChannelId,
    },
    /// An inbound data-channel message.
    Message {
        /// The channel the message arrived on.
        id: RTCDataChannelId,
        /// Whether the payload was sent as text (UTF-8) rather than binary.
        text: bool,
        /// The message payload.
        data: Vec<u8>,
    },
}

/// A sans-I/O WebRTC peer wrapping a single `rtc` `RTCPeerConnection`.
pub struct SansIoPeer {
    pc: RTCPeerConnection,
}

impl SansIoPeer {
    /// Create a peer with no data channels. Either side of the exchange starts
    /// here: an offerer adds a channel via
    /// [`create_data_channel`](Self::create_data_channel) and produces an
    /// offer; an answerer receives the offer, adopts the remote-created
    /// channel via negotiation, and produces an answer.
    pub fn new() -> Result<Self> {
        Ok(Self { pc: build_pc()? })
    }

    /// Create a data channel that will be negotiated in-band with the peer.
    /// Returns the channel's id, which addresses it for
    /// [`send_text`](Self::send_text) / [`send_binary`](Self::send_binary).
    pub fn create_data_channel(
        &mut self,
        label: &str,
        ordered: bool,
        max_retransmits: Option<u16>,
    ) -> Result<RTCDataChannelId> {
        let init = RTCDataChannelInit {
            ordered,
            max_retransmits,
            ..Default::default()
        };
        Ok(self.pc.create_data_channel(label, Some(init))?.id())
    }

    /// Produce an SDP offer (already set as the local description) describing
    /// the local peer's current channels.
    pub fn create_offer(&mut self) -> Result<String> {
        let offer = self.pc.create_offer(None)?;
        self.pc.set_local_description(Instant::now(), offer)?;
        local_sdp(&self.pc)
    }

    /// Apply the remote peer's SDP offer. Any ICE candidates embedded in the
    /// offer are picked up here, so a non-trickle remote (like the demo
    /// `webrtc-rs` hosts) needs no separate candidate exchange.
    pub fn set_remote_offer(&mut self, sdp: String) -> Result<()> {
        self.pc
            .set_remote_description(Instant::now(), RTCSessionDescription::offer(sdp)?)?;
        Ok(())
    }

    /// Apply the remote peer's SDP answer.
    pub fn set_remote_answer(&mut self, sdp: String) -> Result<()> {
        self.pc
            .set_remote_description(Instant::now(), RTCSessionDescription::answer(sdp)?)?;
        Ok(())
    }

    /// Produce an SDP answer (already set as the local description) in response
    /// to a previously applied remote offer.
    pub fn create_answer(&mut self) -> Result<String> {
        let answer = self.pc.create_answer(None)?;
        self.pc.set_local_description(Instant::now(), answer)?;
        local_sdp(&self.pc)
    }

    /// Supply one local `host` candidate for the UDP socket bound at `addr`.
    ///
    /// The sans-I/O stack does not gather candidates itself, so the driver
    /// feeds the address of the socket it owns. Returns the candidate line, so
    /// the driver can trickle it to a remote peer that expects candidates out
    /// of band.
    pub fn add_local_host_candidate(&mut self, addr: SocketAddr) -> Result<String> {
        let host = CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: addr.ip().to_string(),
                port: addr.port(),
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?;
        let init = RTCIceCandidate::from(&host).to_json()?;
        self.pc.add_local_candidate(RTCIceCandidateInit {
            candidate: init.candidate.clone(),
            ..Default::default()
        })?;
        Ok(init.candidate)
    }

    /// Add an ICE candidate received from the remote peer.
    pub fn add_remote_candidate(&mut self, candidate: String) -> Result<()> {
        self.pc.add_remote_candidate(RTCIceCandidateInit {
            candidate,
            ..Default::default()
        })?;
        Ok(())
    }

    /// Pull the next outbound datagram, if any. Call in a loop until it returns
    /// `None` after every step.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.pc.poll_write().map(|msg| Transmit {
            payload: msg.message.to_vec(),
            destination: msg.transport.peer_addr,
        })
    }

    /// Feed an inbound datagram received from `from` on the socket bound at
    /// `local`.
    pub fn handle_input(
        &mut self,
        payload: &[u8],
        from: SocketAddr,
        local: SocketAddr,
        now: Instant,
    ) {
        let _ = self.pc.handle_read(TaggedBytesMut {
            now,
            transport: TransportContext {
                local_addr: local,
                peer_addr: from,
                ecn: None,
                transport_protocol: TransportProtocol::UDP,
            },
            message: BytesMut::from(payload),
        });
    }

    /// Notify the peer that time has advanced so its internal timers
    /// (retransmits, keep-alives) can fire.
    pub fn handle_timeout(&mut self, now: Instant) {
        let _ = self.pc.handle_timeout(now);
    }

    /// Drain all currently available events (connection-state changes, opened
    /// data channels, and inbound messages) into a single ordered list.
    pub fn drain_events(&mut self) -> Vec<PeerEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.pc.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => match state {
                    RTCPeerConnectionState::Connected => events.push(PeerEvent::Connected),
                    RTCPeerConnectionState::Failed => events.push(PeerEvent::Failed),
                    RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed => {
                        events.push(PeerEvent::Closed)
                    }
                    _ => {}
                },
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(id)) => {
                    let label = self
                        .pc
                        .data_channel(id)
                        .map(|dc| dc.label().to_owned())
                        .unwrap_or_default();
                    events.push(PeerEvent::ChannelOpen { id, label });
                }
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(id)) => {
                    events.push(PeerEvent::ChannelClosed { id });
                }
                _ => {}
            }
        }
        while let Some(message) = self.pc.poll_read() {
            if let RTCMessage::DataChannelMessage(id, msg) = message.message {
                events.push(PeerEvent::Message {
                    id,
                    text: msg.is_string,
                    data: msg.data.to_vec(),
                });
            }
        }
        events
    }

    /// Send a text (UTF-8) message on the given channel.
    pub fn send_text(&mut self, id: RTCDataChannelId, text: &str) -> Result<()> {
        let mut dc = self
            .pc
            .data_channel(id)
            .ok_or_else(|| anyhow!("no data channel with id {id:?}"))?;
        dc.send_text(Instant::now(), text)?;
        Ok(())
    }

    /// Send a binary message on the given channel.
    pub fn send_binary(&mut self, id: RTCDataChannelId, data: &[u8]) -> Result<()> {
        let mut dc = self
            .pc
            .data_channel(id)
            .ok_or_else(|| anyhow!("no data channel with id {id:?}"))?;
        dc.send(Instant::now(), BytesMut::from(data))?;
        Ok(())
    }

    /// Close a single data channel, sending its SCTP stream reset to the peer.
    pub fn close_data_channel(&mut self, id: RTCDataChannelId) {
        if let Some(mut dc) = self.pc.data_channel(id) {
            let _ = dc.close();
        }
    }

    /// Bytes handed to the channel's sends that SCTP has not yet released —
    /// zero once everything queued has reached the wire (or been abandoned).
    pub fn channel_outstanding_bytes(&mut self, id: RTCDataChannelId) -> usize {
        self.pc
            .data_channel(id)
            .map(|dc| dc.outstanding_bytes())
            .unwrap_or(0)
    }

    /// Close the peer connection.
    pub fn close(&mut self) {
        let _ = self.pc.close();
    }
}

/// Build a peer connection with the default (STUN-less) configuration.
fn build_pc() -> Result<RTCPeerConnection> {
    let config = RTCConfigurationBuilder::new().build();
    Ok(RTCPeerConnectionBuilder::new()
        .with_configuration(config)
        .build(Instant::now())?)
}

/// Read back the complete local description SDP.
fn local_sdp(pc: &RTCPeerConnection) -> Result<String> {
    Ok(pc
        .local_description()
        .ok_or_else(|| anyhow!("no local description available"))?
        .sdp)
}
