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

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::net::EndpointHandle;
use crate::session::{HostHandle, PathState, SessionEvent, SessionHandle, SessionOptions};

/// A running test session: either a host accepting connections or a peer
/// dialing one. Both expose the same `shutdown`.
enum Session {
    Host(HostHandle),
    Peer(SessionHandle),
}

impl Session {
    async fn shutdown(self) -> Result<()> {
        match self {
            Session::Host(h) => h.shutdown().await,
            Session::Peer(p) => p.shutdown().await,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,p0_nat_test=info")),
        )
        .init();

    tracing::info!(relay = %cli.relay, "starting p0-nat-test");

    // Build the iroh endpoint. The relay URL is the self-hosted rendezvous
    // (docs/02-networking.md §8); it doubles as the punch rendezvous and the
    // fallback path for pairs that cannot punch.
    let endpoint = EndpointHandle::new(cli.relay.clone()).await?;
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
    };

    let (tx, mut rx) = mpsc::channel(256);
    let host_mode = cli.peer.is_none();
    let peers = if host_mode { cli.peers } else { 1 };

    // In host mode we accept `peers` connections; in peer mode we dial one.
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

    let mut last_path: Vec<Option<PathState>> = vec![None; peers as usize];

    let deadline = tokio::time::Instant::now() + cli.duration();
    while tokio::time::Instant::now() < deadline {
        let recv = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
        match recv {
            Ok(Some(SessionEvent::Connected { peer, remote })) => {
                tracing::info!(peer, %remote, "session connected");
            }
            Ok(Some(SessionEvent::Path { peer, path })) => {
                if last_path.get(peer) != Some(&Some(path.clone())) {
                    tracing::info!(peer, %path, "path state changed");
                    last_path[peer] = Some(path);
                }
            }
            Ok(Some(SessionEvent::Stats {
                peer,
                sent,
                received,
                dropped,
            })) => {
                tracing::info!(peer, sent, received, dropped, "datagram stats (10s window)");
            }
            Ok(Some(SessionEvent::Error { peer, error })) => {
                tracing::warn!(peer, %error, "session error");
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
    endpoint.shutdown().await?;

    Ok(())
}
