//! Command-line interface for the P0 NAT test tool.

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

    /// State datagram send rate, in ticks per second (P0 stress = 60).
    #[arg(long, global = true, default_value_t = 60)]
    pub tick_hz: u32,

    /// Payload size of each state datagram, in bytes.
    #[arg(long, global = true, default_value_t = 64)]
    pub payload_bytes: usize,

    /// Total test window, in seconds.
    #[arg(long, global = true, default_value_t = DEFAULT_DURATION_SECS)]
    pub duration_secs: u64,

    /// Print the NodeId and exit without dialing or sending (host helper).
    #[arg(long, global = true)]
    pub print_id: bool,
}

impl Cli {
    /// The test window as a `Duration`.
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration_secs)
    }
}
