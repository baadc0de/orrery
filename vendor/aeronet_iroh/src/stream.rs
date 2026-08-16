//! Reliable message lane over QUIC unidirectional streams.
//!
//! [`IrohIo`](crate::session::IrohIo) carries RFC 9221 datagrams: unreliable,
//! unordered, and hard-capped at the path MTU. That is the right primitive for
//! state that is worthless once stale, and the wrong one for anything that must
//! *arrive* — a control message, a bulk transfer, a repair. QUIC already offers
//! the other primitive on the same connection, with no head-of-line blocking
//! between the two. This module exposes it.
//!
//! # The frontend
//!
//! [`IrohStreamIo`] mirrors [`Session`](aeronet_io::Session)'s shape on purpose:
//! push to [`IrohStreamIo::send`], drain [`IrohStreamIo::recv`], and let the
//! plugin's poll/flush systems move bytes across the async boundary. A message
//! is delivered whole or not at all; there is no MTU to size against and no
//! partial read to reassemble.
//!
//! # One stream, or one per message
//!
//! Reliability is not the only axis. A QUIC stream is ordered *within itself*
//! and independent of every other stream, so how messages are assigned to
//! streams decides what blocks what:
//!
//! - [`StreamMode::Shared`] puts every message on one long-lived stream. Cheap
//!   — no per-message stream setup, and one send window to keep full — but a
//!   lost segment stalls every *later* message until it is retransmitted, even
//!   messages that have already arrived. Ordering across messages is total.
//! - [`StreamMode::Bulk`] opens a fresh stream per message and finishes it.
//!   Messages cannot block each other, at the cost of a stream per message and
//!   no ordering between them.
//!
//! Neither is universally right, which is why the mode rides on the *message*
//! rather than on the session: a peer typically wants its sparse ordered
//! control traffic on one stream and its bulk transfers out of that stream's
//! way. Making it a session-wide setting would force one choice on both.
//!
//! # Framing, and why both modes share it
//!
//! Every message is `[u32 LE length][payload]`, in both modes. `Bulk` could use
//! the stream's own FIN as the delimiter and skip the prefix, but then a
//! receiver would have to know which mode the *sender* chose. Mode is a local
//! send-side policy — a peer may mix both on one connection, and change its
//! mind between messages — so the reader stays uniform: read frames until the
//! stream ends, however many that turns out to be.

use {
    crate::{
        IrohRuntime,
        session::{MAX_STREAM_MESSAGE_LEN, SessionError},
    },
    alloc::{sync::Arc, vec, vec::Vec},
    bevy_ecs::prelude::*,
    bevy_platform::time::Instant,
    bytes::{Bytes, BytesMut},
    core::num::Saturating,
    futures::{
        FutureExt, StreamExt,
        channel::{mpsc, oneshot},
        never::Never,
    },
    iroh::endpoint::Connection,
    tracing::{debug, trace},
};

/// Which stream a message is written to.
///
/// See the [module docs](self#one-stream-or-one-per-message) for the trade this
/// picks between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StreamMode {
    /// Write to the session's single long-lived stream.
    ///
    /// Totally ordered with every other `Shared` message, and free of
    /// per-message stream setup. A lost segment holds up everything queued
    /// behind it.
    #[default]
    Shared,
    /// Open a stream for this message alone, and finish it.
    ///
    /// Independent of every other message: loss on one stream delays only that
    /// message. Costs one stream per message, and imposes no ordering.
    Bulk,
}

/// A message to write to the peer.
#[derive(Debug, Clone)]
pub struct SendMessage {
    /// The payload, delivered whole or not at all.
    pub payload: Bytes,
    /// Which stream to write it to.
    pub mode: StreamMode,
}

impl SendMessage {
    /// A message on the shared stream.
    #[must_use]
    pub const fn shared(payload: Bytes) -> Self {
        Self {
            payload,
            mode: StreamMode::Shared,
        }
    }

    /// A message on a stream of its own.
    #[must_use]
    pub const fn bulk(payload: Bytes) -> Self {
        Self {
            payload,
            mode: StreamMode::Bulk,
        }
    }
}

/// A message that arrived from the peer.
#[derive(Debug, Clone)]
pub struct RecvMessage {
    /// When the frontend was handed this message.
    pub recv_at: Instant,
    /// The payload.
    pub payload: Bytes,
}

/// What the stream lane has moved.
#[derive(Debug, Default, Clone, Copy)]
pub struct StreamStats {
    /// Messages handed to the backend.
    pub messages_sent: u64,
    /// Messages delivered from the backend.
    pub messages_recv: u64,
    /// Payload bytes handed to the backend.
    pub bytes_sent: u64,
    /// Payload bytes delivered from the backend.
    pub bytes_recv: u64,
    /// Inbound streams abandoned mid-message.
    ///
    /// A peer may reset one stream without the connection being in trouble, so
    /// this is counted rather than escalated. A climbing value against a stable
    /// [`Self::messages_recv`] is a peer that is opening streams and not
    /// finishing them.
    pub streams_aborted: u64,
}

/// The reliable message lane for one Iroh session.
///
/// Inserted alongside [`IrohIo`](crate::session::IrohIo) when the session
/// connects, and removed with it. Shaped like
/// [`Session`](aeronet_io::Session) — push to [`Self::send`], drain
/// [`Self::recv`] — so a caller that already drives the datagram lane drives
/// this one the same way.
///
/// You should not add or remove this component directly; the session
/// implementation manages it.
#[derive(Debug, Component)]
pub struct IrohStreamIo {
    /// Messages that have arrived, oldest first. Drain this.
    ///
    /// Ordering across [`StreamMode::Bulk`] messages is not meaningful — they
    /// travel on independent streams and arrive in whatever order the network
    /// delivers them.
    pub recv: Vec<RecvMessage>,
    /// Messages to write. Push here; the flush system drains it.
    pub send: Vec<SendMessage>,
    /// What this lane has moved.
    pub stats: StreamStats,
    rx_message_from_backend: mpsc::UnboundedReceiver<FromBackend>,
    tx_message_to_backend: mpsc::UnboundedSender<SendMessage>,
}

/// What the backend hands the frontend.
///
/// One channel rather than two so a message and the abort that follows it stay
/// in order, and so the frontend has a single thing to drain.
#[derive(Debug)]
pub(crate) enum FromBackend {
    /// A whole message, read off some stream.
    Message(RecvMessage),
    /// An inbound stream ended mid-message.
    StreamAborted,
}

impl IrohStreamIo {
    pub(crate) const fn new(
        rx_message_from_backend: mpsc::UnboundedReceiver<FromBackend>,
        tx_message_to_backend: mpsc::UnboundedSender<SendMessage>,
    ) -> Self {
        Self {
            recv: Vec::new(),
            send: Vec::new(),
            stats: StreamStats {
                messages_sent: 0,
                messages_recv: 0,
                bytes_sent: 0,
                bytes_recv: 0,
                streams_aborted: 0,
            },
            rx_message_from_backend,
            tx_message_to_backend,
        }
    }

    /// A lane with no backend behind it.
    ///
    /// [`Self::send`] and [`Self::recv`] behave exactly as they do on a live
    /// session — queued messages simply go nowhere, and nothing ever arrives
    /// unless a caller pushes to `recv` itself. This is what an in-process
    /// harness needs: `aeronet_io::Session::new` is public for the same reason,
    /// so a test can drive the frontend buffers without a socket underneath.
    #[must_use]
    pub fn detached() -> Self {
        let (_, rx_message_from_backend) = mpsc::unbounded();
        let (tx_message_to_backend, _) = mpsc::unbounded();
        Self::new(rx_message_from_backend, tx_message_to_backend)
    }
}

/// Drains each session's inbound stream messages into [`IrohStreamIo::recv`].
pub(crate) fn poll_streams(mut sessions: Query<&mut IrohStreamIo>) {
    for mut io in &mut sessions {
        let io = &mut *io;
        let mut num_messages = Saturating(0u64);
        let mut num_bytes = Saturating(0u64);
        while let Ok(from_backend) = io.rx_message_from_backend.try_recv() {
            match from_backend {
                FromBackend::Message(message) => {
                    num_messages += 1;
                    num_bytes += message.payload.len() as u64;
                    io.recv.push(message);
                }
                FromBackend::StreamAborted => io.stats.streams_aborted += 1,
            }
        }
        if num_messages.0 > 0 {
            io.stats.messages_recv += num_messages.0;
            io.stats.bytes_recv += num_bytes.0;
            trace!(
                num_messages = num_messages.0,
                num_bytes = num_bytes.0,
                "Received stream messages"
            );
        }
    }
}

/// Hands each session's queued messages to its backend.
pub(crate) fn flush_streams(mut sessions: Query<&mut IrohStreamIo>) {
    for mut io in &mut sessions {
        let io = &mut *io;
        let mut num_messages = Saturating(0u64);
        let mut num_bytes = Saturating(0u64);
        for message in io.send.drain(..) {
            num_messages += 1;
            num_bytes += message.payload.len() as u64;
            _ = io.tx_message_to_backend.unbounded_send(message);
        }
        if num_messages.0 > 0 {
            io.stats.messages_sent += num_messages.0;
            io.stats.bytes_sent += num_bytes.0;
            trace!(
                num_messages = num_messages.0,
                num_bytes = num_bytes.0,
                "Flushed stream messages"
            );
        }
    }
}

/// The length prefix every message carries.
const LENGTH_PREFIX_LEN: usize = 4;

/// Writes queued messages to the peer, opening streams as the mode requires.
///
/// The shared stream is opened lazily on the first [`StreamMode::Shared`]
/// message: a session that only ever sends bulk — or never sends at all — costs
/// the peer no stream.
pub(crate) async fn stream_send_loop(
    conn: Arc<Connection>,
    mut rx_closed: oneshot::Receiver<()>,
    mut rx_message_from_frontend: mpsc::UnboundedReceiver<SendMessage>,
) -> Result<Never, SessionError> {
    let mut shared: Option<iroh::endpoint::SendStream> = None;
    loop {
        let message = futures::select! {
            message = rx_message_from_frontend.next() => message,
            _ = rx_closed => return Err(SessionError::FrontendClosed),
        }
        .ok_or(SessionError::FrontendClosed)?;

        let framed = frame(&message.payload)?;
        match message.mode {
            StreamMode::Shared => {
                if shared.is_none() {
                    shared = Some(conn.open_uni().await.map_err(SessionError::OpenStream)?);
                }
                let Some(stream) = shared.as_mut() else {
                    return Err(SessionError::OpenStream(
                        iroh::endpoint::ConnectionError::LocallyClosed,
                    ));
                };
                stream
                    .write_chunk(framed)
                    .await
                    .map_err(SessionError::WriteStream)?;
            }
            StreamMode::Bulk => {
                // Awaiting the open applies the peer's concurrent-stream limit
                // as backpressure, which is the honest place to feel it. The
                // *write* is spawned, so a message that is slow to drain does
                // not hold up the next one — which is the entire reason this
                // mode exists.
                let mut stream = conn.open_uni().await.map_err(SessionError::OpenStream)?;
                IrohRuntime::spawn(async move {
                    if let Err(err) = stream.write_chunk(framed).await {
                        debug!("Failed to write bulk message: {err:?}");
                        return;
                    }
                    if let Err(err) = stream.finish() {
                        debug!("Failed to finish bulk stream: {err:?}");
                    }
                });
            }
        }
    }
}

/// Length-prefix one payload, refusing anything the reader would reject.
fn frame(payload: &Bytes) -> Result<Bytes, SessionError> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| u64::from(*len) <= MAX_STREAM_MESSAGE_LEN);
    let Some(len) = len else {
        return Err(SessionError::StreamMessageTooLarge {
            len: payload.len(),
            max: MAX_STREAM_MESSAGE_LEN,
        });
    };
    let mut framed = BytesMut::with_capacity(LENGTH_PREFIX_LEN + payload.len());
    framed.extend_from_slice(&len.to_le_bytes());
    framed.extend_from_slice(payload);
    Ok(framed.freeze())
}

/// Accepts inbound streams and reads whole messages off them.
pub(crate) async fn stream_recv_loop(
    conn: Arc<Connection>,
    mut rx_closed: oneshot::Receiver<()>,
    tx_to_frontend: mpsc::UnboundedSender<FromBackend>,
) -> Result<Never, SessionError> {
    loop {
        let stream = futures::select! {
            stream = conn.accept_uni().fuse() => stream,
            _ = rx_closed => return Err(SessionError::FrontendClosed),
        }
        .map_err(SessionError::Connection)?;

        // One reader per stream, so a stalled stream blocks only itself. That
        // is the property `StreamMode::Bulk` is bought for; reading serially
        // here would hand it straight back.
        let tx_to_frontend = tx_to_frontend.clone();
        IrohRuntime::spawn(async move {
            if let Err(err) = read_stream(stream, tx_to_frontend.clone()).await {
                // A peer may reset one stream without the connection being in
                // trouble, so this does not disconnect the session.
                debug!("Inbound stream ended early: {err:?}");
                _ = tx_to_frontend.unbounded_send(FromBackend::StreamAborted);
            }
        });
    }
}

/// Read length-prefixed messages off one stream until it ends.
async fn read_stream(
    mut stream: iroh::endpoint::RecvStream,
    tx_to_frontend: mpsc::UnboundedSender<FromBackend>,
) -> Result<(), SessionError> {
    loop {
        let mut prefix = [0u8; LENGTH_PREFIX_LEN];
        match stream.read_exact(&mut prefix).await {
            Ok(()) => {}
            // A clean end between messages: the sender finished the stream,
            // which is exactly what `StreamMode::Bulk` does after every
            // message and what `Shared` does at disconnect.
            Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(()),
            Err(err) => return Err(SessionError::ReadStream(err)),
        }

        let len = u64::from(u32::from_le_bytes(prefix));
        if len > MAX_STREAM_MESSAGE_LEN {
            // The length is attacker-chosen, so this is refused before a buffer
            // is reserved for it rather than after.
            return Err(SessionError::StreamMessageTooLarge {
                len: usize::try_from(len).unwrap_or(usize::MAX),
                max: MAX_STREAM_MESSAGE_LEN,
            });
        }
        let len = usize::try_from(len).map_err(|_| SessionError::StreamMessageTooLarge {
            len: usize::MAX,
            max: MAX_STREAM_MESSAGE_LEN,
        })?;

        let mut payload = vec![0u8; len];
        stream
            .read_exact(&mut payload)
            .await
            .map_err(SessionError::ReadStream)?;

        tx_to_frontend
            .unbounded_send(FromBackend::Message(RecvMessage {
                recv_at: Instant::now(),
                payload: Bytes::from(payload),
            }))
            .map_err(|_| SessionError::FrontendClosed)?;
    }
}
