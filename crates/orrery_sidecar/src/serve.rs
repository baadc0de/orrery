//! Putting the extracted batches on a link an engine can read — and keeping
//! the simulation entirely unaware of whether anyone is reading (#898 step 3,
//! A9 P-4).
//!
//! `orrery::ipc::export_ipc_frames` writes [`IpcOutbound`] messages into the
//! world every run and stops there: the facade must not name a socket. This
//! module is the consumer the facade's docs point at, and it lives in the
//! game composition because that is where every other engine-facing choice of
//! this sidecar already lives.
//!
//! # The one property this file exists to guarantee
//!
//! **A9 P-4: kill the observer, and the sidecar's state is unchanged.** The
//! renderer is a spectator of a simulation that is authoritative without it.
//! Three separate things could break that, and each is designed out here
//! rather than hoped about:
//!
//! 1. **Blocking.** A write to a TCP socket whose peer has stopped reading
//!    blocks once the send buffer fills. If the simulation thread ever calls
//!    `write`, a wedged observer wedges the ruleset. So the socket is owned
//!    by a dedicated thread and the simulation only ever `try_send`s to a
//!    bounded channel, which never blocks.
//! 2. **Unbounded growth.** An observer that reads slowly rather than not at
//!    all would grow an unbounded queue until the sidecar died of memory.
//!    The channel is bounded; when it fills, the *observer* is dropped, not
//!    the frames — see [`SERVE_QUEUE`] and the overrun path below.
//! 3. **Failure propagating inward.** A broken pipe, an `ECONNRESET` from a
//!    `SIGKILL`ed renderer, a frame the peer refused: every one of them is
//!    handled by dropping the connection and returning to `accept`. None of
//!    them is reported into the world, and no system reads the outcome. The
//!    sidecar cannot condition on the observer because it is given nothing to
//!    condition on.
//!
//! The complement matters too: with no observer connected the publisher does
//! not even encode. [`IpcServer::streaming`] is one relaxed atomic load, so
//! an unobserved sidecar pays for the extraction it would run anyway and
//! nothing else.
//!
//! # Why the connection is exclusive, and why it is not queued
//!
//! One observer at a time. A second dial replaces the first, which is the
//! behaviour a developer attaching a fresh editor to a long-running sidecar
//! actually wants; the displaced one sees a closed stream and is expected to
//! reconnect. Fan-out to several renderers is a real feature and deliberately
//! not this one: it needs a per-observer queue and a policy for the slowest,
//! and #898 step 3 renders from one observer.
//!
//! On overrun the connection is dropped rather than the frames coalesced. The
//! stream is push-only and has no resync point, so an observer that missed a
//! frames batch has no way to be told which one — and a `SpawnBatch` silently
//! skipped is an entity that never appears. Disconnecting is the honest
//! answer: the observer sees the stream end and reconnects, and its first
//! frames batch is a complete extraction, so it is correct again immediately.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bevy::prelude::*;

use orrery::ipc::{export_ipc_frames, IpcOutbound, PresentationFrame};
use orrery_ipc_transport::{set_nodelay, FrameWriter};

/// How many encoded batches may be in flight to the writer thread.
///
/// Two seconds of a 60 Hz stream at four batches a tick. Large enough that a
/// scheduler hiccup on the writer thread is invisible; small enough that an
/// observer which has genuinely stopped reading is disconnected within a
/// couple of seconds rather than after the sidecar's memory has grown.
pub const SERVE_QUEUE: usize = 480;

/// How long the writer thread waits for a batch before re-checking whether the
/// simulation has declared the current observer overrun.
const WRITER_POLL: Duration = Duration::from_millis(50);

/// Counters about the link, for a report. Nothing in the world reads them.
///
/// They are deliberately not a [`Resource`]: A9 P-4 is the claim that the
/// simulation cannot observe its observer, and a resource carrying the
/// observer's fate into the world is precisely the wire that would make the
/// claim false. A caller that wants them holds the [`IpcServer`] handle.
#[derive(Debug, Default)]
pub struct ServeStats {
    /// Observers accepted since the listener opened.
    pub accepted: AtomicU64,
    /// Encoded batches handed to the writer thread.
    pub queued: AtomicU64,
    /// Batches written to a socket.
    pub written: AtomicU64,
    /// Connections dropped because the peer failed or vanished.
    pub link_failures: AtomicU64,
    /// Connections dropped because the queue filled.
    pub overruns: AtomicU64,
}

/// The serving side of the sidecar's IPC link.
///
/// Held as a Bevy [`Resource`] so the publishing system can reach it, but it
/// carries no state the simulation reads: the only thing a system does with
/// it is push bytes at a channel and never look back.
#[derive(Resource)]
pub struct IpcServer {
    bound: SocketAddr,
    outbound: SyncSender<Vec<u8>>,
    streaming: Arc<AtomicBool>,
    overrun: Arc<AtomicBool>,
    stats: Arc<ServeStats>,
}

impl IpcServer {
    /// Bind a listener and start serving whatever is published to it.
    ///
    /// Pass port `0` for an OS-chosen port; [`bound`](Self::bound) reports
    /// what was actually taken, which is what lets a test or a launcher run
    /// without a port lease.
    ///
    /// # Errors
    ///
    /// Returns the bind error, or the error from reading back the bound
    /// address.
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        let (outbound, inbound) = sync_channel(SERVE_QUEUE);
        let streaming = Arc::new(AtomicBool::new(false));
        let overrun = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(ServeStats::default());

        let server = Self {
            bound,
            outbound,
            streaming: Arc::clone(&streaming),
            overrun: Arc::clone(&overrun),
            stats: Arc::clone(&stats),
        };
        let (arrivals, accepted) = channel();
        let accept_stats = Arc::clone(&stats);
        thread::Builder::new()
            .name("orrery-ipc-accept".to_owned())
            .spawn(move || accept_observers(&listener, &arrivals, &accept_stats))?;
        thread::Builder::new()
            .name("orrery-ipc-serve".to_owned())
            .spawn(move || serve(&accepted, &inbound, &streaming, &overrun, &stats))?;
        Ok(server)
    }

    /// The address the listener actually took.
    #[must_use]
    pub const fn bound(&self) -> SocketAddr {
        self.bound
    }

    /// Whether an observer is connected right now.
    ///
    /// A hint, and only a hint: the answer can be stale by the time it is
    /// read, because the observer may be killed between the load and the
    /// send. Nothing here is a synchronisation point, which is the reason the
    /// publisher below is correct whichever way the race falls.
    #[must_use]
    pub fn streaming(&self) -> bool {
        self.streaming.load(Ordering::Relaxed)
    }

    /// The link counters.
    #[must_use]
    pub fn stats(&self) -> &ServeStats {
        &self.stats
    }

    /// Offer one encoded batch to the current observer, if there is one.
    ///
    /// Never blocks and never fails upward. A full queue disconnects the
    /// observer; a dead writer thread is a permanently unstreaming server.
    fn offer(&self, body: Vec<u8>) {
        match self.outbound.try_send(body) {
            Ok(()) => {
                self.stats.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                // The observer is not keeping up. Stop encoding for it
                // immediately, and let the writer thread notice and drop the
                // socket. The batch that did not fit is simply gone: the
                // reconnecting observer's first frames batch is a complete
                // extraction, so there is nothing to resend.
                self.overrun.store(true, Ordering::Relaxed);
                self.streaming.store(false, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.streaming.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// The acceptor: the only thread that ever calls `accept`.
///
/// It is separate from the writer, and that separation is load-bearing. With
/// one thread doing both, a dial while an observer was connected would sit in
/// the listen backlog until that observer died — so an observer that hangs
/// rather than dies would hold the listener for as long as it lived, and a
/// developer attaching a fresh renderer would appear to connect and then
/// receive nothing forever. Accepting eagerly is what makes "a second dial
/// replaces the first" true rather than merely intended.
fn accept_observers(listener: &TcpListener, arrivals: &Sender<TcpStream>, stats: &ServeStats) {
    loop {
        let Ok((stream, _peer)) = listener.accept() else {
            // The listener itself is gone; there is nothing left to serve and
            // nothing to tell the simulation about it.
            return;
        };
        let _ = set_nodelay(&stream);
        stats.accepted.fetch_add(1, Ordering::Relaxed);
        if arrivals.send(stream).is_err() {
            return;
        }
    }
}

/// The writer: the only thread that ever touches a connected socket.
fn serve(
    arrivals: &Receiver<TcpStream>,
    inbound: &Receiver<Vec<u8>>,
    streaming: &AtomicBool,
    overrun: &AtomicBool,
    stats: &ServeStats,
) {
    let mut current: Option<FrameWriter<TcpStream>> = None;
    loop {
        // A newer observer supersedes the one in hand. The displaced one sees
        // a closed stream and is expected to reconnect; nothing is resent to
        // the new one, because its first frames batch is a complete
        // extraction one tick from now.
        match arrivals.try_recv() {
            Ok(stream) => {
                while inbound.try_recv().is_ok() {}
                overrun.store(false, Ordering::Relaxed);
                current = Some(FrameWriter::new(stream));
                streaming.store(true, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }

        if current.is_some() && overrun.load(Ordering::Relaxed) {
            stats.overruns.fetch_add(1, Ordering::Relaxed);
            current = None;
            streaming.store(false, Ordering::Relaxed);
            overrun.store(false, Ordering::Relaxed);
        }

        match inbound.recv_timeout(WRITER_POLL) {
            Ok(body) => {
                let Some(writer) = current.as_mut() else {
                    continue;
                };
                if writer.write_frame(&body).is_err() || writer.flush().is_err() {
                    // A killed observer arrives here, as `EPIPE` or
                    // `ECONNRESET`. It is the end of the connection and
                    // nothing else.
                    stats.link_failures.fetch_add(1, Ordering::Relaxed);
                    current = None;
                    streaming.store(false, Ordering::Relaxed);
                    continue;
                }
                stats.written.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Encode every batch the extractor produced and offer it to the observer.
///
/// Runs after `export_ipc_frames` for the same markers, so the batches this
/// system reads are the ones that run just wrote. When nobody is connected it
/// drains the reader and does no work — the messages still expire on Bevy's
/// own double-buffer schedule, exactly as they do in a sidecar with no server
/// at all.
pub fn publish_ipc_frames(server: Res<IpcServer>, mut inbound: MessageReader<IpcOutbound>) {
    if !server.streaming() {
        inbound.clear();
        return;
    }
    for outbound in inbound.read() {
        // An encode failure here is a batch too large for the schema's `u32`
        // lengths, which at D6's 1024-entity ceiling cannot happen. It is
        // still not a reason to disturb the simulation: the batch is dropped
        // and the next extraction supersedes it.
        if let Ok(body) = outbound.0.clone().encode() {
            server.offer(body);
        }
    }
}

/// Serve one presented component's extracted batches on a bound listener.
///
/// Add it after the extraction plugin, instantiated with the same markers and
/// presented component, so the ordering constraint can be stated against the
/// actual system rather than against a label.
pub struct OrreryIpcServePlugin<P, I, C> {
    server: PendingServer,
    marker: ServeMarkers<P, I, C>,
}

/// The bound server, waiting for `build` to take it out.
///
/// A `Mutex` because `Plugin::build` takes `&self`: the server is moved into
/// the app exactly once, and the plugin cannot hand out a second one.
type PendingServer = std::sync::Mutex<Option<IpcServer>>;

/// The plugin's unused generic parameters. `fn() -> (P, I, C)` keeps it
/// `Send + Sync` and invariant-free regardless of the parameters — the same
/// device `OrreryIpcExportPlugin` uses for the same three types.
type ServeMarkers<P, I, C> = std::marker::PhantomData<fn() -> (P, I, C)>;

impl<P, I, C> OrreryIpcServePlugin<P, I, C> {
    /// The plugin for an already-bound server.
    ///
    /// The server is built by the caller so the bound address is available
    /// before the app runs — a launcher has to print the port, and a test has
    /// to dial it.
    #[must_use]
    pub const fn new(server: IpcServer) -> Self {
        Self {
            server: std::sync::Mutex::new(Some(server)),
            marker: std::marker::PhantomData,
        }
    }
}

impl<P, I, C> Plugin for OrreryIpcServePlugin<P, I, C>
where
    P: Component,
    I: Component,
    C: PresentationFrame,
{
    fn build(&self, app: &mut App) {
        let server = self
            .server
            .lock()
            .expect("the serve plugin's server is never held across a panic")
            .take()
            .expect("OrreryIpcServePlugin is added to exactly one app");
        app.insert_resource(server);
        app.add_systems(
            Update,
            publish_ipc_frames.after(export_ipc_frames::<P, I, C>),
        );
    }
}
