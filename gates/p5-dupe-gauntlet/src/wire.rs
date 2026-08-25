//! Minimal raw-iroh client for the gateway's two reply transports.

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_stream_frame, untag, Channel,
    MAX_RELIABLE_MESSAGE_BYTES,
};
use orrery_protocol::{GatewayMsg, GatewayReply};
use tokio::sync::{mpsc, Mutex};

const LENGTH_PREFIX_LEN: usize = 4;

/// One admitted gateway connection.
pub struct Session {
    _endpoint: iroh::Endpoint,
    connection: iroh::endpoint::Connection,
    inbound: Mutex<mpsc::UnboundedReceiver<Bytes>>,
}

impl Session {
    /// Dial a gateway and consume its admission stream.
    pub async fn connect(secret: iroh::SecretKey, address: iroh::EndpointAddr) -> Result<Self> {
        let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![orrery_persistd::GATEWAY_ALPN.to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(secret)
            .bind()
            .await
            .context("bind gauntlet endpoint")?;
        let connection = endpoint
            .connect(address, orrery_persistd::GATEWAY_ALPN)
            .await
            .context("dial gauntlet gateway")?;
        let mut admission = connection
            .accept_uni()
            .await
            .context("accept gateway admission stream")?;
        let accepted = admission
            .read_to_end(16)
            .await
            .context("read gateway admission")?;
        anyhow::ensure!(accepted == vec![0u8], "gateway refused admission");

        let (tx, rx) = mpsc::unbounded_channel();
        spawn_datagram_reader(connection.clone(), tx.clone());
        spawn_stream_reader(connection.clone(), tx);
        Ok(Self {
            _endpoint: endpoint,
            connection,
            inbound: Mutex::new(rx),
        })
    }

    /// Send a control-lane frame.
    pub fn send(&self, message: &GatewayMsg) -> Result<()> {
        self.connection
            .send_datagram(Bytes::from(encode_stream_frame(message)))
            .context("send gateway control frame")
    }

    /// Read the next decodable reply from either gateway transport.
    pub async fn recv(&self, within: Duration) -> Option<GatewayReply> {
        let deadline = tokio::time::Instant::now() + within;
        let mut inbound = self.inbound.lock().await;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let packet = match tokio::time::timeout(remaining, inbound.recv()).await {
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

fn spawn_stream_reader(connection: iroh::endpoint::Connection, sink: mpsc::UnboundedSender<Bytes>) {
    tokio::spawn(async move {
        while let Ok(stream) = connection.accept_uni().await {
            let sink = sink.clone();
            tokio::spawn(async move { read_reliable_stream(stream, &sink).await });
        }
    });
}

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
