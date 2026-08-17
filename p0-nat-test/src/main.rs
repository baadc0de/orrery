//! P0 NAT test tool — Orrery transport spike.
//!
//! A standalone, friend-distributable binary that exercises the single biggest
//! P0 bet (docs/11-roadmap.md): iroh 1.0.x as the universal transport — QUIC
//! with NAT hole punching and relay fallback (docs/02-networking.md §1, D3).
//!
//! Two roles, one binary:
//!   * `host` — the rendezvous. Prints its `NodeId`, which friends paste into
//!     `--peer`. Also runs the per-tick state datagram loop.
//!   * `peer` — dials the host by `NodeId`, then runs the same loop.
//!
//! Every pair reports the same telemetry the P0 demo criterion cares about
//! (docs/11-roadmap.md §P0): direct-vs-relay path, time-to-direct-path, and
//! per-tick datagram delivery. There is no game and no replication — raw
//! sessions, datagrams, streams, and NAT telemetry, exactly as P0 specifies.

mod cli;
mod net;
mod session;
mod telemetry;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use iroh::EndpointId;
use std::path::Path;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::net::EndpointHandle;
use crate::session::{
    HostHandle, MeshHandle, PathState, SessionEvent, SessionHandle, SessionOptions,
};
use crate::telemetry::{emit, TelemetryContext};

/// A running test session: a host accepting connections, a peer dialing one,
/// or a full mesh. All expose the same `shutdown`.
enum Session {
    Host(HostHandle),
    Peer(SessionHandle),
    Mesh(MeshHandle),
}

impl Session {
    async fn shutdown(self) -> Result<()> {
        match self {
            Session::Host(h) => h.shutdown().await,
            Session::Peer(p) => p.shutdown().await,
            Session::Mesh(m) => m.shutdown().await,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // In JSON mode, tracing logs go to stderr so stdout stays machine-parseable
    // (one JSON record per line).
    let writer = if cli.json {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr)
    } else {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stdout)
    };
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,p0_nat_test=info")),
        )
        .init();

    tracing::info!(relay = %cli.relay, "starting p0-nat-test");

    // Build the iroh endpoint. The relay URL is the self-hosted rendezvous
    // (docs/02-networking.md §8); it doubles as the punch rendezvous and the
    // fallback path for pairs that cannot punch.
    let endpoint = EndpointHandle::new(cli.relay.clone(), cli.secret_key.clone()).await?;
    tracing::info!(node_id = %endpoint.node_id(), "endpoint ready");

    // `--print-id` is the host helper: print the NodeId friends paste into
    // `--peer`, then exit without dialing or sending.
    if cli.print_id {
        println!("{}", endpoint.node_id());
        endpoint.shutdown().await?;
        return Ok(());
    }

    // A session is one iroh connection to one peer. The peer dials the host by
    // NodeId; the host (no `--peer`) accepts incoming connections. With
    // `--peers N` the host accepts N simultaneous connections (local mesh test);
    // otherwise it accepts one. Both sides run the same datagram loop per
    // connection.
    let options = SessionOptions {
        tick_hz: cli.tick_hz,
        payload_bytes: cli.payload_bytes,
        duration: cli.duration(),
        ping_interval: Duration::from_secs_f64(1.0 / cli.ping_hz as f64),
    };

    let (tx, mut rx) = mpsc::channel(256);

    // Mesh mode: each node dials every roster node after it and accepts every
    // roster node before it, forming a true full mesh. The roster is a file of
    // NodeIds (one per line); `--mesh-index` (or self-matching) picks this
    // node's position.
    if let Some(roster_path) = &cli.mesh {
        let roster = read_roster(roster_path)?;
        let self_index = match cli.mesh_index {
            Some(i) => i,
            None => roster
                .iter()
                .position(|id| *id == endpoint.node_id())
                .ok_or_else(|| {
                    anyhow::anyhow!("local NodeId not found in roster; pass --mesh-index")
                })?,
        };
        let n = roster.len();
        let mesh = MeshHandle::spawn(endpoint.clone(), roster, self_index, options, tx.clone());
        let ctx = TelemetryContext {
            node: endpoint.node_id(),
            role: "mesh",
        };
        let session = Session::Mesh(mesh);
        run_event_loop(&ctx, &mut rx, n as u32, cli.duration(), session).await?;
        endpoint.shutdown().await?;
        return Ok(());
    }

    // Star mode: a host accepting N connections, or a peer dialing one.
    let host_mode = cli.peer.is_none();
    let peers = if host_mode { cli.peers } else { 1 };

    let session = if host_mode {
        Session::Host(HostHandle::spawn(endpoint.clone(), peers, options, tx))
    } else {
        Session::Peer(SessionHandle::spawn(
            endpoint.clone(),
            cli.peer.unwrap(),
            options,
            tx,
        ))
    };

    // Telemetry: in JSON mode we emit records; in human mode we log. We track
    // per-peer connect time so we can report time-to-direct-path (the P0
    // criterion's direct-path metric).
    let ctx = TelemetryContext {
        node: endpoint.node_id(),
        role: if host_mode { "host" } else { "peer" },
    };

    run_event_loop(&ctx, &mut rx, peers, cli.duration(), session).await?;
    endpoint.shutdown().await?;

    Ok(())
}

/// Drive the event loop for a run: read session events, emit telemetry, and
/// keep running until the test window elapses or the session ends. `n_peers`
/// is the number of connections this run expects (the size of the per-peer
/// tracking vectors).
async fn run_event_loop(
    ctx: &TelemetryContext,
    rx: &mut mpsc::Receiver<SessionEvent>,
    n_peers: u32,
    duration: Duration,
    session: Session,
) -> Result<()> {
    let n = n_peers as usize;
    let mut last_path: Vec<Option<PathState>> = vec![None; n];
    let mut connected_at: Vec<Option<Instant>> = vec![None; n];

    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        let recv = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
        match recv {
            Ok(Some(SessionEvent::Connected { peer, remote })) => {
                connected_at[peer] = Some(Instant::now());
                emit(ctx, peer, &SessionEvent::Connected { peer, remote }, None);
            }
            Ok(Some(SessionEvent::Path { peer, path })) => {
                let ttd_ms = if path == PathState::Direct {
                    connected_at[peer].map(|t| t.elapsed().as_millis() as u64)
                } else {
                    None
                };
                if last_path.get(peer) != Some(&Some(path.clone())) {
                    emit(
                        ctx,
                        peer,
                        &SessionEvent::Path {
                            peer,
                            path: path.clone(),
                        },
                        ttd_ms,
                    );
                    last_path[peer] = Some(path);
                }
            }
            Ok(Some(SessionEvent::Stats {
                peer,
                sent,
                received,
                dropped,
                rtt_p50_us,
                rtt_p95_us,
            })) => {
                emit(
                    ctx,
                    peer,
                    &SessionEvent::Stats {
                        peer,
                        sent,
                        received,
                        dropped,
                        rtt_p50_us,
                        rtt_p95_us,
                    },
                    None,
                );
            }
            Ok(Some(SessionEvent::Error { peer, error })) => {
                emit(ctx, peer, &SessionEvent::Error { peer, error }, None);
            }
            Ok(None) => {
                // The session task finished (normal shutdown); nothing more to do.
                break;
            }
            Err(_) => {
                // Timeout waiting for the next event; loop and check the deadline.
            }
        }
    }

    tracing::info!("test window elapsed; shutting down");
    session.shutdown().await?;
    Ok(())
}

/// Read a mesh roster file: one iroh NodeId (hex) per line, blank/comment
/// lines ignored. Returns the ordered list of NodeIds.
fn read_roster(path: &Path) -> Result<Vec<EndpointId>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read roster {}", path.display()))?;
    let mut ids = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let id: EndpointId = line
            .parse()
            .with_context(|| format!("roster line {}: invalid NodeId '{line}'", i + 1))?;
        ids.push(id);
    }
    if ids.is_empty() {
        anyhow::bail!("roster {} is empty", path.display());
    }
    Ok(ids)
}
