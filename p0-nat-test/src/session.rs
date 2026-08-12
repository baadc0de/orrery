//! The test session: one iroh connection to one peer, running the per-tick
//! state datagram loop and reporting path + delivery telemetry.
//!
//! Both the host and the peer dial the same target NodeId (the host dials
//! itself). That keeps the session lifecycle, the channel policy, and the
//! datagram loop identical on both sides — matching how P0 treats the transport
//! as a drop-in symmetric layer (docs/02-networking.md §4).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::net::EndpointHandle;

/// Configuration for a test session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// State datagram send rate, in ticks per second.
    pub tick_hz: u32,
    /// Payload size of each state datagram, in bytes.
    pub payload_bytes: usize,
    /// How long to run the datagram loop.
    pub duration: Duration,
}

/// Events a session reports to the main loop for logging. In host mode a
/// session is one of several simultaneous connections, so events carry the
/// peer index they belong to.
#[derive(Debug)]
pub enum SessionEvent {
    /// The QUIC connection to the remote peer is up.
    Connected { peer: usize, remote: EndpointId },
    /// The active path (relay vs direct) changed.
    Path { peer: usize, path: PathState },
    /// Aggregated datagram delivery for the last reporting window.
    Stats {
        peer: usize,
        sent: u64,
        received: u64,
        dropped: u64,
    },
    /// A recoverable error inside the session.
    Error { peer: usize, error: String },
}

/// Current path of a session, mirroring the design's `PathState`
/// (docs/02-networking.md §4).
#[derive(Debug, Clone, PartialEq)]
pub enum PathState {
    /// Traffic is (still) riding the relay path.
    Relay,
    /// A direct, punched path is primary.
    Direct,
    /// Multipath transition window — relay standby, direct in use.
    Mixed,
}

impl std::fmt::Display for PathState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathState::Relay => write!(f, "relay"),
            PathState::Direct => write!(f, "direct"),
            PathState::Mixed => write!(f, "mixed"),
        }
    }
}

/// A handle to a running session task. Kept alive for the session's lifetime.
pub struct SessionHandle {
    shutdown_tx: oneshot::Sender<()>,
}

impl SessionHandle {
    /// Spawn a session task that dials `target`, then runs the datagram loop.
    pub fn spawn(
        endpoint: EndpointHandle,
        target: EndpointId,
        options: SessionOptions,
        events: mpsc::Sender<SessionEvent>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(session_task(endpoint, target, options, events, shutdown_rx));
        Self { shutdown_tx }
    }

    /// Ask the session to stop and wait for it to finish.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        Ok(())
    }
}

/// A handle to a host that accepts several simultaneous connections (local
/// mesh test). One datagram loop runs per connection.
pub struct HostHandle {
    shutdown_tx: oneshot::Sender<()>,
}

impl HostHandle {
    /// Spawn a host task that accepts `count` connections and runs a datagram
    /// loop per connection, tagging events with the peer index.
    pub fn spawn(
        endpoint: EndpointHandle,
        count: u32,
        options: SessionOptions,
        events: mpsc::Sender<SessionEvent>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(host_task(endpoint, count, options, events, shutdown_rx));
        Self { shutdown_tx }
    }

    /// Ask the host to stop and wait for it to finish.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        Ok(())
    }
}

async fn session_task(
    endpoint: EndpointHandle,
    target: EndpointId,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let run = run_session(endpoint, target, 0, options, events.clone(), shutdown_rx).await;
    if let Err(e) = run {
        // If the channel is closed the consumer is gone; the error is moot.
        let _ = events
            .send(SessionEvent::Error {
                peer: 0,
                error: e.to_string(),
            })
            .await;
    }
}

/// Accept `count` connections, spawning a datagram loop per connection.
async fn host_task(
    endpoint: EndpointHandle,
    count: u32,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let mut shutdown_rx = shutdown_rx;
    let mut handles = Vec::new();

    for peer in 0..count {
        // Accept one connection, then hand it to its own datagram loop task.
        let accepted = tokio::select! {
            _ = &mut shutdown_rx => break,
            res = accept(endpoint.inner()) => res,
        };
        let conn = match accepted {
            Ok(conn) => conn,
            Err(e) => {
                let _ = events
                    .send(SessionEvent::Error {
                        peer: peer as usize,
                        error: format!("accept failed: {e}"),
                    })
                    .await;
                continue;
            }
        };
        let remote = conn.remote_id();
        let _ = events
            .send(SessionEvent::Connected {
                peer: peer as usize,
                remote,
            })
            .await;

        let (tx, rx) = oneshot::channel();
        handles.push(tx);
        let events = events.clone();
        let options = options.clone();
        let peer = peer as usize;
        tokio::spawn(async move {
            let _ = run_datagram_loop_for(conn, peer, options, events, rx).await;
        });
    }

    // Wait for shutdown or until all per-peer loops finish.
    let _ = shutdown_rx.await;
    for tx in handles {
        let _ = tx.send(());
    }
}

async fn run_session(
    endpoint: EndpointHandle,
    target: EndpointId,
    peer: usize,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    // Dial by key, not by address (docs/02-networking.md §1). The peer dials
    // the host by NodeId; the host (no target) accepts an incoming connection.
    // Both then run the same datagram loop.
    let conn = dial(endpoint.inner(), endpoint.relay(), target).await?;
    let _ = events
        .send(SessionEvent::Connected { peer, remote: target })
        .await;

    run_datagram_loop(conn, options, events, shutdown_rx).await
}

/// The per-tick state datagram loop shared by host and peer.
async fn run_datagram_loop(
    conn: Connection,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    run_datagram_loop_for(conn, 0, options, events, shutdown_rx).await
}

/// The per-tick state datagram loop, attributed to peer index `peer`.
async fn run_datagram_loop_for(
    conn: Connection,
    peer: usize,
    options: SessionOptions,
    events: mpsc::Sender<SessionEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    // Channel policy (docs/02-networking.md §7): state rides unreliable
    // datagrams; nothing reliable is sent in this spike.
    let payload = build_payload(options.payload_bytes);

    let tick_interval = Duration::from_secs_f64(1.0 / options.tick_hz as f64);

    // Receiver task: count datagrams until the connection closes.
    let (recv_tx, mut recv_rx) = mpsc::channel(1024);
    let recv_task = tokio::spawn(receiver_task(conn.clone(), recv_tx, options.payload_bytes));

    // Path monitor: report relay -> direct migration (the P0 punch signal).
    let (path_tx, mut path_rx) = mpsc::channel(64);
    let path_task = tokio::spawn(path_monitor_task(conn.clone(), path_tx));

    // Windowed statistics.
    let mut window = DeliveryWindow::default();
    let stats_every = Duration::from_secs(10);
    let mut last_stats = Instant::now();

    let mut shutdown_rx = shutdown_rx;
    let mut ticker = tokio::time::interval(tick_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let started = Instant::now();
    let mut seq: u64 = 0;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                seq = seq.wrapping_add(1);
                if let Err(e) = conn.send_datagram(payload.clone()) {
                    tracing::warn!(%e, "datagram send failed");
                    break;
                }
                window.sent += 1;
            }
            maybe = recv_rx.recv() => {
                match maybe {
                    Some(RecvEvent::Datagram) => {
                        window.received += 1;
                    }
                    Some(RecvEvent::Drop) => {
                        window.dropped += 1;
                    }
                    None => break,
                }
            }
            path = path_rx.recv() => {
                match path {
                    Some(path) => {
                        let _ = events.send(SessionEvent::Path { peer, path }).await;
                    }
                    None => break,
                }
            }
            _ = &mut shutdown_rx => {
                tracing::info!("shutdown requested");
                break;
            }
        }

        // Periodic stats and path probing.
        if last_stats.elapsed() >= stats_every {
            let _ = events
                .send(SessionEvent::Stats {
                    peer,
                    sent: window.sent,
                    received: window.received,
                    dropped: window.dropped,
                })
                .await;
            window = DeliveryWindow::default();
            last_stats = Instant::now();
        }

        if started.elapsed() >= options.duration {
            break;
        }
    }

    recv_task.abort();
    path_task.abort();

    // Final stats flush.
    let _ = events
        .send(SessionEvent::Stats {
            peer,
            sent: window.sent,
            received: window.received,
            dropped: window.dropped,
        })
        .await;

    Ok(())
}

/// Dial a peer by NodeId, connecting through the home relay first so the
/// session is up immediately; hole punching then migrates it to a direct path
/// inside the same connection (docs/02-networking.md §1). The relay URL is the
/// dial hint (the design's `relay_hint` in `PeerEntry`): without it the peer
/// has no addressing information for the host.
async fn dial(endpoint: &Endpoint, relay: &RelayUrl, target: EndpointId) -> Result<Connection> {
    let addr = EndpointAddr::new(target).with_relay_url(relay.clone());
    endpoint
        .connect(addr, b"p0-nat-test")
        .await
        .with_context(|| format!("failed to dial {target}"))
}

/// Accept one incoming connection (host mode). The host is the rendezvous:
/// peers dial it by NodeId, and it runs the same datagram loop in reply.
async fn accept(endpoint: &Endpoint) -> Result<Connection> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| anyhow::anyhow!("no incoming connection"))?;
    incoming
        .accept()
        .map_err(|e| anyhow::anyhow!("failed to accept connection: {e}"))?
        .await
        .map_err(|e| anyhow::anyhow!("connection handshake failed: {e}"))
}

fn build_payload(size: usize) -> Bytes {
    Bytes::from(vec![b'p'; size.max(1)])
}

#[derive(Default)]
struct DeliveryWindow {
    sent: u64,
    received: u64,
    dropped: u64,
}

enum RecvEvent {
    Datagram,
    Drop,
}

/// Receive loop: reads datagrams until the connection is closed, counting
/// delivery. In a real transport test we'd verify sequence continuity against
/// the rolling counter; the count alone already yields the P0 drop/delivery
/// signal.
async fn receiver_task(conn: Connection, tx: mpsc::Sender<RecvEvent>, payload_size: usize) {
    loop {
        match conn.read_datagram().await {
            Ok(buf) => {
                let ok = buf.len() >= payload_size;
                let _ = tx
                    .send(if ok {
                        RecvEvent::Datagram
                    } else {
                        RecvEvent::Drop
                    })
                    .await;
            }
            Err(e) => {
                tracing::info!(%e, "datagram stream closed");
                break;
            }
        }
    }
}

/// Watch the connection's path events and report the active path. The P0
/// punch signal is the migration from a relay path to a direct (IP) path
/// (docs/02-networking.md §1); a session that never leaves the relay path is
/// the expected ~10% tail (docs/02-networking.md §8).
async fn path_monitor_task(conn: Connection, tx: mpsc::Sender<PathState>) {
    use tokio_stream::StreamExt;

    let mut events = conn.path_events();
    while let Some(event) = events.next().await {
        let state = match event {
            iroh::endpoint::PathEvent::Selected { remote_addr, .. } => {
                if remote_addr.is_ip() {
                    PathState::Direct
                } else if remote_addr.is_relay() {
                    PathState::Relay
                } else {
                    PathState::Mixed
                }
            }
            _ => continue,
        };
        if tx.send(state).await.is_err() {
            break;
        }
    }
}
