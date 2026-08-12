//! Command-line interface for the P0 NAT test tool.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use iroh::PublicKey;

/// Default relay URL: the self-hosted iroh-relay on the Hetzner box
/// (see .agents/memory/hetzner-relay.md). Friends can override with --relay.
const DEFAULT_RELAY: &str = "https://iroh-relay.distopik.com";

/// Default test window: the P0 demo criterion runs 30 minutes; the default
/// here is a quick smoke test that still exercises punch + a few seconds of
/// 60 Hz datagrams.
const DEFAULT_DURATION_SECS: u64 = 30;

/// P0 NAT test — Orrery transport spike.
///
/// Exercises iroh 1.0.x QUIC with NAT hole punching and relay fallback
/// (docs/02-networking.md §1, docs/11-roadmap.md §P0). Run `host` on one
/// machine, pass its NodeId to friends as `--peer`, and compare the path and
/// datagram telemetry each side prints.
#[derive(Debug, Parser)]
#[command(name = "p0-nat-test", version, about)]
pub struct Cli {
    /// The iroh relay URL used as the punch rendezvous and fallback path.
    #[arg(long, global = true, default_value = DEFAULT_RELAY)]
    pub relay: String,

    /// The remote peer's NodeId to dial. Omit to act as the host (rendezvous).
    #[arg(long, global = true)]
    pub peer: Option<PublicKey>,

    /// Host mode: accept this many simultaneous connections (local mesh test).
    /// Defaults to 1; only meaningful when `--peer` is absent.
    #[arg(long, global = true, default_value_t = 1)]
    pub peers: u32,

    /// State datagram send rate, in ticks per second (P0 stress = 60).
    #[arg(long, global = true, default_value_t = 60)]
    pub tick_hz: u32,

    /// Payload size of each state datagram, in bytes.
    #[arg(long, global = true, default_value_t = 64)]
    pub payload_bytes: usize,

    /// Roundtrip ping rate, in pings per second (for P50/P95 latency).
    #[arg(long, global = true, default_value_t = 1)]
    pub ping_hz: u32,

    /// Total test window, in seconds.
    #[arg(long, global = true, default_value_t = DEFAULT_DURATION_SECS)]
    pub duration_secs: u64,

    /// Print the NodeId and exit without dialing or sending (host helper).
    #[arg(long, global = true)]
    pub print_id: bool,

    /// Full-mesh mode: path to a roster file (one NodeId per line, this node
    /// included). Dials every node after us in the list, accepts every node
    /// before us, so each pair connects exactly once. Combine with
    /// `--mesh-index` to identify this node's position.
    #[arg(long, global = true)]
    pub mesh: Option<PathBuf>,

    /// This node's index in the `--mesh` roster (0-based). Required with
    /// `--mesh` if the roster cannot be matched to the local NodeId.
    #[arg(long, global = true)]
    pub mesh_index: Option<usize>,

    /// A stable secret key (hex) so this node keeps the same NodeId across
    /// runs. Without it, every invocation generates a fresh NodeId, which
    /// breaks `--mesh` rosters (NodeIds are ephemeral).
    #[arg(long, global = true)]
    pub secret_key: Option<String>,

    /// Emit telemetry as one JSON object per line on stdout (machine-parseable
    /// for the punch-rate dashboard). Tracing logs go to stderr.
    #[arg(long, global = true)]
    pub json: bool,
}

impl Cli {
    /// The test window as a `Duration`.
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration_secs)
    }
}
