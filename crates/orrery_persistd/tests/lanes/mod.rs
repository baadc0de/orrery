//! The two-lane transport a raw-iroh test client needs to reach the gateway.
//!
//! Lives beside `support` rather than inside it because `support` is also
//! `#[path]`-included by `orrery_persist_client`'s live tests, which link Bevy
//! and `aeronet_iroh` but not raw `iroh` or `tokio`. Splitting the two keeps
//! the shared fixture linkable from both sides.

#![allow(
    dead_code,
    reason = "each integration-test binary uses a different subset of this helper"
)]

use std::time::Duration;

use bytes::Bytes;

/// A test client's view of a gateway connection's two lanes.
///
/// The gateway answers control traffic on reliable uni-streams and bulk state
/// on datagrams (roadmap decision C-1), so a test that reads only
/// `conn.read_datagram()` sees half the conversation and times out on the
/// other half. This drains both into one queue — the same shape the real
/// client's `process_replies` uses — so a test asserts on *what* arrived
/// rather than on which lane carried it.
///
/// The send side mirrors the Bevy client: one long-lived uni-stream, opened on
/// the first control message, carrying `[u32 LE length][payload]` frames.
pub struct GatewayLanes {
    conn: iroh::endpoint::Connection,
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Bytes>>,
    control: tokio::sync::Mutex<Option<iroh::endpoint::SendStream>>,
}

impl GatewayLanes {
    /// Start draining both lanes of an already-admitted connection.
    ///
    /// Must be called *after* the admission uni-stream has been read: the
    /// stream reader accepts every inbound stream from here on and would
    /// otherwise consume it.
    pub fn attach(conn: iroh::endpoint::Connection) -> Self {
        let (tx, inbound) = tokio::sync::mpsc::unbounded_channel();
        let datagrams = conn.clone();
        let datagram_tx = tx.clone();
        tokio::spawn(async move {
            while let Ok(pkt) = datagrams.read_datagram().await {
                if datagram_tx.send(pkt).is_err() {
                    return;
                }
            }
        });
        let streams = conn.clone();
        tokio::spawn(async move {
            while let Ok(mut recv) = streams.accept_uni().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        let mut prefix = [0u8; 4];
                        if recv.read_exact(&mut prefix).await.is_err() {
                            return;
                        }
                        let len = u32::from_le_bytes(prefix) as usize;
                        if len > orrery_protocol::channels::MAX_RELIABLE_MESSAGE_BYTES {
                            return;
                        }
                        let mut payload = vec![0u8; len];
                        if recv.read_exact(&mut payload).await.is_err() {
                            return;
                        }
                        if tx.send(Bytes::from(payload)).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            conn,
            inbound: tokio::sync::Mutex::new(inbound),
            control: tokio::sync::Mutex::new(None),
        }
    }

    /// The underlying connection, for connection-level operations (close,
    /// stats) that are not lane traffic.
    pub fn conn(&self) -> &iroh::endpoint::Connection {
        &self.conn
    }

    /// Write a control message on the reliable lane.
    pub async fn send_control(&self, msg: &orrery_protocol::GatewayMsg) {
        let payload = orrery_protocol::channels::encode_stream_frame(msg);
        let mut framed = Vec::with_capacity(payload.len() + 4);
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        let mut control = self.control.lock().await;
        if control.is_none() {
            *control = Some(self.conn.open_uni().await.expect("open control stream"));
        }
        control
            .as_mut()
            .expect("control stream opened")
            .write_chunk(Bytes::from(framed))
            .await
            .expect("write control frame");
    }

    /// Send a bulk-state message on the datagram lane.
    pub fn send_state(&self, msg: &orrery_protocol::GatewayMsg) {
        self.conn
            .send_datagram(Bytes::from(orrery_protocol::channels::encode_datagram(msg)))
            .expect("send state datagram");
    }

    /// The next raw inbound payload from either lane, within `timeout`.
    pub async fn next_payload(&self, timeout: Duration) -> Option<Bytes> {
        let mut inbound = self.inbound.lock().await;
        tokio::time::timeout(timeout, inbound.recv()).await.ok()?
    }

    /// The next decodable reply from either lane, within `timeout`.
    pub async fn next_reply(&self, timeout: Duration) -> Option<orrery_protocol::GatewayReply> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let payload = self.next_payload(remaining).await?;
            if let Some(reply) = decode_reply(&payload) {
                return Some(reply);
            }
        }
    }
}

/// Decode a reply on its channel tag, not on the lane that carried it.
pub fn decode_reply(payload: &[u8]) -> Option<orrery_protocol::GatewayReply> {
    use orrery_protocol::channels::{untag, Channel};
    match untag(payload)?.0 {
        Channel::State => orrery_protocol::channels::decode_datagram(payload),
        Channel::Control => orrery_protocol::channels::decode_stream_frame(payload),
    }
}
