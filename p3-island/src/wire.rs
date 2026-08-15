//! The gateway wire surface, spoken directly over raw iroh.
//!
//! This mirrors what `orrery_persist_client` does through Bevy, minus Bevy:
//! the property under test is what the *registrar* does when a peer dies, so
//! the harness wants the smallest possible client that is still the real
//! protocol.

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
};
use orrery_protocol::{GatewayMsg, GatewayReply};

/// The gateway ALPN, matching `orrery_persistd::GATEWAY_ALPN`.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// An open gateway session: a raw iroh connection past the admission stream.
pub struct Session {
    /// Held for its lifetime, not read: dropping the endpoint closes every
    /// connection made from it, which would tear the session down mid-run.
    #[allow(dead_code)]
    endpoint: iroh::Endpoint,
    pub connection: iroh::endpoint::Connection,
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
        Ok(Self {
            endpoint,
            connection,
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

    /// Read the next decodable reply, or `None` on timeout.
    ///
    /// Undecodable packets are skipped rather than failing the read: the lane
    /// carries both channels, and a harness that died on an unexpected frame
    /// would be reporting its own brittleness as a registrar defect.
    pub async fn recv(&self, within: Duration) -> Option<GatewayReply> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let packet =
                match tokio::time::timeout(remaining, self.connection.read_datagram()).await {
                    Ok(Ok(packet)) => packet,
                    // A closed connection is indistinguishable from silence for
                    // the caller's purposes: both mean no more replies.
                    Ok(Err(_)) | Err(_) => return None,
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
