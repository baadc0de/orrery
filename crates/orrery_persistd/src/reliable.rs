//! The reliable lane: control traffic over raw iroh unidirectional streams
//! (D3, roadmap decision C-1).
//!
//! `orrery_persistd` is Bevy-free (D15), so it cannot link `aeronet_iroh`'s
//! stream lane the way the client does. It speaks the same wire instead: every
//! message is `[u32 LE length][payload]`, read until the stream ends. That is
//! `aeronet_iroh::stream`'s framing verbatim, which is what lets the Bevy
//! client and this server talk without either knowing about the other's
//! plumbing.
//!
//! # Why the gateway opens two streams, not one
//!
//! A QUIC stream is ordered within itself and independent of every other
//! stream. Putting all control traffic on one stream would be simplest and
//! would reintroduce, at the transport layer, exactly the coupling
//! [`handle_connection`](crate::gateway) already spends per-message tasks to
//! avoid: a 27-cell area load — megabytes, cold FDB scans — sitting in front of
//! an intent ack that the D16 table budgets at p99 < 10 ms. So the lane is
//! split by what the traffic is for:
//!
//! - [`Lane::Control`] carries hello acks, intent acks, lease control and
//!   interest acks. Sparse, small, and ordered with each other.
//! - [`Lane::Area`] carries area pages and area-load errors. Also ordered —
//!   nearest-first page-in (D16 < 50 ms to first page) is an *ordering*
//!   property, so pages must not race each other — but ordered on a stream of
//!   its own, where a slow page delays only later pages.
//!
//! Two long-lived streams, not a stream per message: per-page streams would
//! also decouple pages from each other, which nearest-first does not want, and
//! would spend a stream on every one of the 27.
//!
//! Both are opened lazily, on first use, so a connection that never subscribes
//! costs the peer no area stream.
//!
//! # Failures are counted, never swallowed
//!
//! The C-1 posture on the datagram lane was that a failed send is logged and
//! counted rather than dropped silently, because a reply that never arrives is
//! indistinguishable from one that was never generated. That posture survives
//! the move: a write error, a refused oversize message, or a backend that has
//! gone away all increment the shared failure counter and log at `warn`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use iroh::endpoint::Connection;
use orrery_protocol::channels::MAX_RELIABLE_MESSAGE_BYTES;
use orrery_protocol::NodeId;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// The length prefix every reliable message carries.
const LENGTH_PREFIX_LEN: usize = 4;

/// Which of a connection's two reliable streams a message is written to.
///
/// See the [module docs](self#why-the-gateway-opens-two-streams-not-one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// Sparse ordered control: hello acks, intent acks, lease and interest
    /// control.
    Control,
    /// Area-load pages and per-cell load errors, ordered nearest-first among
    /// themselves and out of [`Lane::Control`]'s way.
    Area,
}

/// The writing half of one connection's reliable lanes.
///
/// Cloneable and cheap: sending is a channel push, and the stream writes happen
/// on the per-lane tasks [`spawn`] started. Dropping every clone ends those
/// tasks, which finishes the streams — the peer's reader sees a clean end
/// between messages and stops, rather than reporting a torn stream.
#[derive(Debug, Clone)]
pub struct ReliableSender {
    control: mpsc::UnboundedSender<Bytes>,
    area: mpsc::UnboundedSender<Bytes>,
    failures: Arc<AtomicU64>,
    remote: NodeId,
}

impl ReliableSender {
    /// Queue `payload` on `lane`.
    ///
    /// Returns immediately; the write is the lane task's job. A message the
    /// peer's reader would refuse is dropped here rather than written, because
    /// writing it would tear the whole stream and take every message queued
    /// behind it with it.
    pub fn send(&self, lane: Lane, payload: Bytes) {
        if payload.len() > MAX_RELIABLE_MESSAGE_BYTES {
            self.failures.fetch_add(1, Ordering::Relaxed);
            warn!(
                remote = %self.remote,
                len = payload.len(),
                max = MAX_RELIABLE_MESSAGE_BYTES,
                ?lane,
                "gateway: reliable message too large to send"
            );
            return;
        }
        let tx = match lane {
            Lane::Control => &self.control,
            Lane::Area => &self.area,
        };
        if tx.send(payload).is_err() {
            self.failures.fetch_add(1, Ordering::Relaxed);
            debug!(remote = %self.remote, ?lane, "gateway: reliable lane closed before send");
        }
    }
}

/// Start the two per-connection writer tasks and return their sender.
///
/// `failures` is the gateway's shared send-failure counter, so a stream write
/// that fails is as visible in the telemetry as a datagram send that failed.
#[must_use]
pub fn spawn(conn: Arc<Connection>, remote: NodeId, failures: Arc<AtomicU64>) -> ReliableSender {
    let control = spawn_lane(
        Arc::clone(&conn),
        remote,
        Arc::clone(&failures),
        Lane::Control,
    );
    let area = spawn_lane(conn, remote, Arc::clone(&failures), Lane::Area);
    ReliableSender {
        control,
        area,
        failures,
        remote,
    }
}

/// One lane's writer task: open the stream on the first message, then write
/// framed messages until the sender half is dropped or the stream breaks.
fn spawn_lane(
    conn: Arc<Connection>,
    remote: NodeId,
    failures: Arc<AtomicU64>,
    lane: Lane,
) -> mpsc::UnboundedSender<Bytes> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(async move {
        let mut stream: Option<iroh::endpoint::SendStream> = None;
        while let Some(payload) = rx.recv().await {
            if stream.is_none() {
                match conn.open_uni().await {
                    Ok(opened) => stream = Some(opened),
                    Err(e) => {
                        failures.fetch_add(1, Ordering::Relaxed);
                        warn!(?e, %remote, ?lane, "gateway: failed to open reliable stream");
                        break;
                    }
                }
            }
            let Some(send) = stream.as_mut() else { break };
            let len = payload.len();
            if let Err(e) = send.write_chunk(frame(&payload)).await {
                failures.fetch_add(1, Ordering::Relaxed);
                warn!(?e, %remote, len, ?lane, "gateway: reliable stream write failed");
                break;
            }
        }
        // A clean finish is what tells the peer's reader "no more messages"
        // rather than "this stream was reset".
        if let Some(mut send) = stream {
            let _ = send.finish();
        }
    });
    tx
}

/// Length-prefix one payload.
fn frame(payload: &Bytes) -> Bytes {
    let mut framed = BytesMut::with_capacity(LENGTH_PREFIX_LEN + payload.len());
    #[allow(clippy::cast_possible_truncation)] // `send` refuses anything past the cap.
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(payload);
    framed.freeze()
}

/// Accept inbound unidirectional streams and forward whole messages to `sink`.
///
/// Each stream gets its own reader task, so a peer that stalls one stream
/// stalls only that one — the same property the send side buys by splitting
/// control from area traffic, applied in the other direction.
///
/// This must not be started before any handshake stream the protocol reads by
/// hand: it accepts every inbound stream from the moment it runs, and would
/// consume one that a caller was about to read itself.
pub fn spawn_receiver(conn: Arc<Connection>, remote: NodeId, sink: mpsc::UnboundedSender<Bytes>) {
    tokio::spawn(async move {
        loop {
            let stream = match conn.accept_uni().await {
                Ok(stream) => stream,
                Err(e) => {
                    debug!(?e, %remote, "gateway: inbound reliable lane closed");
                    return;
                }
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                // A peer may reset one stream without the connection being in
                // trouble, so this ends the reader and nothing else.
                if let Err(e) = read_stream(stream, &sink).await {
                    debug!(?e, %remote, "gateway: inbound reliable stream ended early");
                }
            });
        }
    });
}

/// Read length-prefixed messages off one stream until it ends cleanly.
async fn read_stream(
    mut stream: iroh::endpoint::RecvStream,
    sink: &mpsc::UnboundedSender<Bytes>,
) -> Result<(), String> {
    loop {
        let mut prefix = [0u8; LENGTH_PREFIX_LEN];
        match stream.read_exact(&mut prefix).await {
            Ok(()) => {}
            // A clean end *between* messages: the peer finished the stream.
            Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(()),
            Err(e) => return Err(format!("read length prefix: {e}")),
        }
        let len = u32::from_le_bytes(prefix) as usize;
        if len > MAX_RELIABLE_MESSAGE_BYTES {
            // The length is peer-chosen. Refuse it before a buffer is reserved
            // for it, not after.
            return Err(format!(
                "message of {len} B exceeds the {MAX_RELIABLE_MESSAGE_BYTES} B cap"
            ));
        }
        let mut payload = vec![0u8; len];
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("read payload: {e}"))?;
        if sink.send(Bytes::from(payload)).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_prefixes_the_payload_length_little_endian() {
        // The prefix is what `aeronet_iroh`'s reader parses; a big-endian or
        // off-by-one prefix would desynchronize the whole stream rather than
        // corrupt one message, so this is checked byte for byte.
        let framed = frame(&Bytes::from_static(b"abcd"));
        assert_eq!(&framed[..4], &4u32.to_le_bytes());
        assert_eq!(&framed[4..], b"abcd");
    }

    #[tokio::test]
    async fn oversize_send_is_counted_rather_than_written() {
        // Writing a message past the cap would tear the stream and take every
        // message queued behind it, so it is refused at the door — and the
        // refusal must be visible, because a dropped reply is otherwise
        // indistinguishable from a reply that was never generated.
        let (control, _control_rx) = mpsc::unbounded_channel();
        let (area, mut area_rx) = mpsc::unbounded_channel();
        let failures = Arc::new(AtomicU64::new(0));
        let sender = ReliableSender {
            control,
            area,
            failures: Arc::clone(&failures),
            remote: iroh::SecretKey::from_bytes(&[1u8; 32]).public(),
        };

        sender.send(
            Lane::Area,
            Bytes::from(vec![0u8; MAX_RELIABLE_MESSAGE_BYTES + 1]),
        );
        assert_eq!(failures.load(Ordering::Relaxed), 1);
        assert!(
            area_rx.try_recv().is_err(),
            "an oversize message must not reach the stream"
        );

        sender.send(Lane::Area, Bytes::from_static(b"small"));
        assert_eq!(failures.load(Ordering::Relaxed), 1);
        assert_eq!(area_rx.try_recv().expect("queued"), b"small".as_slice());
    }

    #[test]
    fn send_to_a_closed_lane_is_counted() {
        // The lane task ends when the connection dies. A caller that keeps
        // replying afterwards must show up in the failure counter, not vanish.
        let (control, control_rx) = mpsc::unbounded_channel();
        let (area, _area_rx) = mpsc::unbounded_channel();
        drop(control_rx);
        let failures = Arc::new(AtomicU64::new(0));
        let sender = ReliableSender {
            control,
            area,
            failures: Arc::clone(&failures),
            remote: iroh::SecretKey::from_bytes(&[2u8; 32]).public(),
        };

        sender.send(Lane::Control, Bytes::from_static(b"reply"));
        assert_eq!(failures.load(Ordering::Relaxed), 1);
    }
}
