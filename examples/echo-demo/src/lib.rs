//! `echo-demo`: an example WebAssembly component that exercises a WebRTC data
//! channel one message at a time.
//!
//! The component is host-agnostic and drives the standard
//! `polymorph:webrtc-datachannels/connections` interface end to end: it stands up
//! **two** peer connections inside the component (an offerer and an answerer),
//! exchanges their SDP offer/answer and trickled ICE candidates directly, and
//! then:
//!
//!   1. spawns a producer loop that sends `message-count` messages through
//!      `data-channel.send` on the offerer's channel,
//!   2. echoes every message back from the answerer's end of the channel,
//!   3. concurrently reads the echoed messages back through
//!      `data-channel.receive`, verifying each payload byte-for-byte and in
//!      order while counting the messages/bytes,
//!
//! all within a single cooperative async task (the loops run under
//! `futures::join!`). The same component binary runs unchanged under the
//! polyengine (runtime-linked JS) host and the Wasmtime (`webrtc-rs`) host, which is
//! what demonstrates cross-implementation compatibility.

wit_bindgen::generate!({
    path: "wit",
    world: "webrtc-echo-demo",
    generate_all,
});

use exports::demo::webrtc_echo::demo::{DemoConfig, DemoStats, Guest};
use polymorph::webrtc_datachannels::connections::{
    DataChannel, DataChannelOptions, PeerConnection,
};
use polymorph::webrtc_datachannels::types::{Error, IceCandidate, Message};

struct Component;

impl Guest for Component {
    async fn run(config: DemoConfig) -> Result<DemoStats, Error> {
        let count = config.message_count;
        let size = config.message_size as usize;

        let (offerer, answerer, near, far) = connect_pair().await?;

        // Drive the three loops concurrently on this single task. Each call
        // carries exactly one message, preserving WebRTC message boundaries.
        let send_fut = async {
            for i in 0..count {
                near.send(Message::Binary(make_message(size, i))).await?;
            }
            Ok::<(), Error>(())
        };
        let echo_fut = async {
            for _ in 0..count {
                match far.receive().await {
                    Ok(message) => far.send(message).await?,
                    // The channel closed before every message arrived.
                    Err(_) => break,
                }
            }
            Ok::<(), Error>(())
        };
        let recv_fut = async {
            let mut messages_received: u32 = 0;
            let mut bytes_echoed: u64 = 0;
            while messages_received < count {
                match near.receive().await {
                    Ok(message) => {
                        // Every echoed message must come back byte-for-byte and
                        // in order: the channel is ordered and the answerer
                        // echoes verbatim, so message `i` is exactly
                        // `make_message(size, i)`.
                        let expected = make_message(size, messages_received);
                        match &message {
                            Message::Binary(bytes) if *bytes == expected => {}
                            other => {
                                return Err(Error::Other(format!(
                                    "message {messages_received} corrupted or out of order \
                                     (got {} bytes, kind {})",
                                    message_len(other),
                                    match other {
                                        Message::Binary(_) => "binary",
                                        Message::String(_) => "string",
                                    }
                                )));
                            }
                        }
                        messages_received += 1;
                        bytes_echoed += message_len(&message) as u64;
                    }
                    // The channel closed before every message was echoed back.
                    Err(_) => break,
                }
            }
            Ok((messages_received, bytes_echoed))
        };

        let (send_result, echo_result, recv_result) = futures::join!(send_fut, echo_fut, recv_fut);
        send_result?;
        echo_result?;
        let (messages_received, bytes_echoed) = recv_result?;

        offerer.close();
        answerer.close();

        Ok(DemoStats {
            messages_sent: count,
            messages_received,
            bytes_echoed,
        })
    }
}

/// Stand up the offerer and the in-component echo answerer over the standard
/// `connections` interface — a real SDP offer/answer exchange plus trickled
/// ICE — and return both peers with the two ends of the negotiated channel.
async fn connect_pair() -> Result<(PeerConnection, PeerConnection, DataChannel, DataChannel), Error>
{
    let offerer = PeerConnection::new(None);
    let answerer = PeerConnection::new(None);

    let options = DataChannelOptions::new();
    options.set_label("echo");
    options.set_ordered(true);
    let near = offerer.create_data_channel(options)?;

    let offer = offerer.create_offer().await?;
    offerer.set_local_description(offer.clone()).await?;
    answerer.set_remote_description(offer).await?;
    let answer = answerer.create_answer().await?;
    answerer.set_local_description(answer.clone()).await?;
    offerer.set_remote_description(answer).await?;

    // Relay each side's candidates to the other. This batches rather than
    // trickles: `collect_candidates` waits for the stream to end (the
    // end-of-candidates signal) before any are added — fine here, where both
    // peers live in one instance and gathering needs no remote input.
    for candidate in collect_candidates(offerer.local_ice_candidates()).await {
        answerer.add_ice_candidate(candidate).await?;
    }
    for candidate in collect_candidates(answerer.local_ice_candidates()).await {
        offerer.add_ice_candidate(candidate).await?;
    }

    let (offerer_connected, answerer_connected) =
        futures::join!(offerer.wait_connected(), answerer.wait_connected());
    offerer_connected?;
    answerer_connected?;

    // Adopt the channel the offerer opened.
    let mut incoming = answerer.incoming_data_channels();
    let (_status, batch) = incoming.read(Vec::with_capacity(1)).await;
    let far = batch
        .into_iter()
        .next()
        .ok_or_else(|| Error::Other("no incoming data channel".to_string()))?;

    Ok((offerer, answerer, near, far))
}

/// Drain a `local-ice-candidates` stream to its end.
async fn collect_candidates(stream: wit_bindgen::StreamReader<IceCandidate>) -> Vec<IceCandidate> {
    let mut stream = stream;
    let mut out = Vec::new();
    loop {
        let (status, batch) = stream.read(Vec::with_capacity(4)).await;
        out.extend(batch);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            break;
        }
    }
    out
}

/// The byte length of a received message, regardless of kind.
fn message_len(message: &Message) -> usize {
    match message {
        Message::Binary(bytes) => bytes.len(),
        Message::String(text) => text.len(),
    }
}

/// Build a deterministic `size`-byte message tagged with its index; the
/// receive loop verifies each echoed payload against it byte-for-byte.
fn make_message(size: usize, index: u32) -> Vec<u8> {
    let mut message = vec![0u8; size];
    let tag = index.to_le_bytes();
    for (slot, byte) in message.iter_mut().zip(tag.iter().cycle()) {
        *slot = *byte;
    }
    message
}

export!(Component);
