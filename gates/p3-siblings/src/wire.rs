//! The gateway wire surface, spoken directly over raw iroh.
//!
//! A near-copy of `gates/p3-island/src/wire.rs`. The two tools are separate
//! workspaces and neither is a library, so there is nowhere shared to put it
//! short of publishing a crate for eighty lines of framing; the duplication is
//! deliberate and the two must be kept in step by hand. What it must stay in
//! step *with* is the gateway's own framing, and the header below is the whole
//! reason that is not obvious.
//!
//! This mirrors what `orrery_persist_client` does through Bevy, minus Bevy:
//! the property under test is what the *registrar* does when a peer dies, so
//! the harness wants the smallest possible client that is still the real
//! protocol.
//!
//! # Replies arrive on two transports, not one
//!
//! The gateway answers on whichever transport the channel policy (D3) picks
//! for the payload, and since `8fd8c22` that is not one transport: state
//! replies stay on datagrams, and **every control reply — `HelloAck`,
//! `InterestAck`, every `LeaseMsg` — rides a unidirectional QUIC stream**,
//! length-prefixed exactly as `orrery_persistd::reliable` frames it. A reader
//! that polls only `read_datagram` therefore never sees a single control
//! reply: it observes silence and reports it as a refusal. That is precisely
//! how this harness read the gateway between `8fd8c22` and this comment, and
//! it made the P3 island gate fail its first nightly run with `0/50 claims
//! answered` and eight peers each dying on `gateway did not accept the peer's
//! hello: None`.
//!
//! So [`Session::connect`] starts one reader per transport and merges them
//! into a single queue, and [`Session::recv`] reads that queue. The merge is
//! the honest model of the wire: a caller waiting for a reply does not know,
//! and must not need to know, which lane it will come back on.

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
    MAX_RELIABLE_MESSAGE_BYTES,
};
use orrery_protocol::{GatewayMsg, GatewayReply};
use tokio::sync::{mpsc, Mutex};

/// The gateway ALPN, matching `orrery_persistd::GATEWAY_ALPN`.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The length prefix every reliable-lane message carries, little-endian.
///
/// `orrery_persistd::reliable::frame` writes it and `aeronet_iroh`'s stream
/// lane parses it; this is the third reader of the same four bytes.
const LENGTH_PREFIX_LEN: usize = 4;

/// An open gateway session: a raw iroh connection past the admission stream.
pub struct Session {
    /// Held for its lifetime, not read: dropping the endpoint closes every
    /// connection made from it, which would tear the session down mid-run.
    #[allow(dead_code)]
    endpoint: iroh::Endpoint,
    pub connection: iroh::endpoint::Connection,
    /// Replies from both transports, in arrival order. Behind a mutex so
    /// [`Session::recv`] keeps taking `&self` — callers hold the session in an
    /// `Arc` and read it from more than one place.
    inbound: Mutex<mpsc::UnboundedReceiver<Bytes>>,
}

impl Session {
    /// Dial `address` with `secret` and complete the admission handshake.
    pub async fn connect(secret: iroh::SecretKey, address: iroh::EndpointAddr) -> Result<Self> {
        let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![GATEWAY_ALPN.to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(secret)
            .bind()
            .await
            .context("bind harness endpoint")?;
        let connection = endpoint
            .connect(address, GATEWAY_ALPN)
            .await
            .context("dial gateway")?;
        // The gateway streams `[ACCEPTED]` on a uni stream before any
        // datagrams flow; not reading it would race the first send.
        let mut admission = connection
            .accept_uni()
            .await
            .context("accept gateway admission stream")?;
        let accepted = admission
            .read_to_end(16)
            .await
            .context("read gateway admission")?;
        anyhow::ensure!(accepted == vec![0u8], "gateway refused admission");

        // Both readers start only now, for the same reason the gateway starts
        // its own after `send_admission`: the stream reader accepts every
        // inbound stream from the moment it runs, and would consume the
        // admission stream the handshake above reads by hand.
        let (tx, rx) = mpsc::unbounded_channel();
        spawn_datagram_reader(connection.clone(), tx.clone());
        spawn_stream_reader(connection.clone(), tx);

        Ok(Self {
            endpoint,
            connection,
            inbound: Mutex::new(rx),
        })
    }

    /// Send a control-lane message.
    pub fn send_control(&self, message: &GatewayMsg) -> Result<()> {
        self.connection
            .send_datagram(Bytes::from(encode_stream_frame(message)))
            .context("send control frame")
    }

    /// Send a state-lane message.
    pub fn send_state(&self, message: &GatewayMsg) -> Result<()> {
        self.connection
            .send_datagram(Bytes::from(encode_datagram(message)))
            .context("send state datagram")
    }

    /// Read the next decodable reply from either transport, or `None` on
    /// timeout.
    ///
    /// Undecodable packets are skipped rather than failing the read: the lane
    /// carries both channels, and a harness that died on an unexpected frame
    /// would be reporting its own brittleness as a registrar defect.
    pub async fn recv(&self, within: Duration) -> Option<GatewayReply> {
        let deadline = tokio::time::Instant::now() + within;
        let mut inbound = self.inbound.lock().await;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let packet = match tokio::time::timeout(remaining, inbound.recv()).await {
                // Both readers gone means a closed connection, which is
                // indistinguishable from silence for the caller's purposes.
                Ok(Some(packet)) => packet,
                Ok(None) | Err(_) => return None,
            };
            let Some((channel, _)) = untag(&packet) else {
                continue;
            };
            let decoded = match channel {
                Channel::State => decode_datagram(&packet),
                Channel::Control => decode_stream_frame(&packet),
            };
            if let Some(reply) = decoded {
                return Some(reply);
            }
        }
    }
}

/// Forward every inbound datagram to `sink` until the connection ends.
fn spawn_datagram_reader(
    connection: iroh::endpoint::Connection,
    sink: mpsc::UnboundedSender<Bytes>,
) {
    tokio::spawn(async move {
        while let Ok(packet) = connection.read_datagram().await {
            if sink.send(packet).is_err() {
                return;
            }
        }
    });
}

/// Forward every message on every inbound reliable stream to `sink`.
///
/// One reader task per stream, matching the gateway's two-lane split: a stalled
/// area page must not hold up the control reply queued behind it on the other
/// stream.
fn spawn_stream_reader(connection: iroh::endpoint::Connection, sink: mpsc::UnboundedSender<Bytes>) {
    tokio::spawn(async move {
        while let Ok(stream) = connection.accept_uni().await {
            let sink = sink.clone();
            tokio::spawn(async move {
                // A peer may reset one stream without the connection being in
                // trouble, so this ends the reader and nothing else.
                read_reliable_stream(stream, &sink).await;
            });
        }
    });
}

/// Read `[u32 LE length][payload]` messages off one stream until it ends.
async fn read_reliable_stream(
    mut stream: iroh::endpoint::RecvStream,
    sink: &mpsc::UnboundedSender<Bytes>,
) {
    loop {
        let mut prefix = [0u8; LENGTH_PREFIX_LEN];
        if stream.read_exact(&mut prefix).await.is_err() {
            return;
        }
        let len = u32::from_le_bytes(prefix) as usize;
        // The length is gateway-chosen, and this harness is not the place to
        // discover a heap exhaustion: refuse it before reserving for it.
        if len > MAX_RELIABLE_MESSAGE_BYTES {
            return;
        }
        let mut payload = vec![0u8; len];
        if stream.read_exact(&mut payload).await.is_err() {
            return;
        }
        if sink.send(Bytes::from(payload)).is_err() {
            return;
        }
    }
}
