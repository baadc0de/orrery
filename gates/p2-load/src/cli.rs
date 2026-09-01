//! Command-line interface for the P2 latency rig.
//!
//! Mirrors the P0 precedent (`gates/p0-nat-test/src/cli.rs`): one flat `Parser`
//! struct, tracing on stderr, JSON telemetry on stdout. The load profile is
//! either taken from a seeder manifest (`--manifest`, docs/12-world-seeding.md
//! §9.3/§12.3) or synthesized deterministically from `--entities`/`--cells`.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

/// The P2 demo load (docs/11-roadmap.md §P2): 10 000 entities across 100+
/// cells. 10k is the demo's entity count.
pub const DEFAULT_ENTITIES: u64 = 10_000;

/// 128 cells ≥ the demo's "100+", and matches the p2demo scenario's
/// 128×16×128-cell extent (docs/12-world-seeding.md §18.2) at a coarse
/// occupancy.
pub const DEFAULT_CELLS: u32 = 128;

/// The rig session budget must clear `entities × diff_hz` (see
/// `check_fan_out` in main.rs): with the default 1024-byte flush budget
/// (orrery_persist_client/src/config.rs, D16) one session sustains
/// `1024 / (diff payload + 64)` diffs per flush at 20 flushes/s — 160 diffs/s
/// at the default 64-byte payload. The demo's 10 000 × 2 Hz therefore needs
/// ≥ 125 sessions; the assert refuses smaller values instead of reporting
/// queueing delay as commit latency. Six is a sane default for small smoke
/// runs (≤ 1 000 entities × 2 Hz).
pub const DEFAULT_SESSIONS: u32 = 6;

/// The default per-entity diff rate (Hz), inside the D16 1–4 Hz uplink range.
pub const DEFAULT_DIFF_HZ: f64 = 2.0;

/// The default intent mix (docs/12-world-seeding.md §12.3's `intent_mix`):
/// 3% of diff sends upgraded to an intent, split 2:1 trade:craft. Mix
/// fractions are relative to the diff rate, not additive.
pub const DEFAULT_INTENT_MIX: &str = "trade=0.02,craft=0.01";

/// The default diff payload (bytes). Sized so `payload + 64` (the uplink
/// scheduler's per-diff overhead estimate, uplink.rs flush) fits the D16
/// 1024-byte flush budget about eight times per flush — the fan-out the
/// startup assert checks.
pub const DEFAULT_DIFF_PAYLOAD_BYTES: usize = 64;

/// P2 latency rig — the D16 gate's load generator.
///
/// Dials a real `persistd` gateway (iroh, ALPN `orrery/gateway/0`), registers
/// `--entities` synthetic entities in the D16 1–4 Hz uplink scheduler at
/// `--diff-hz`, drives them across `--cells` distinct interest cells from a
/// closed-form trajectory (docs/12-world-seeding.md §12.3: a trajectory
/// *program*, not a trace), and interleaves the `--intent-mix`. All four D16
/// latency series are emitted as JSONL on stdout for `gates/p2-dashboard --gate`.
#[derive(Debug, Parser)]
#[command(name = "p2-load", version, about)]
pub struct Cli {
    /// The gateway's NodeId (transport identity, D3). This is the node the rig
    /// dials; the rig refuses to run against a gateway whose `HelloAck` names
    /// a different id.
    #[arg(long, env = "ORRERY_GATEWAY")]
    pub gateway: orrery_protocol::NodeId,

    /// The gateway's socket address to dial (`ip:port`). The persistd binary
    /// prints its full `EndpointAddr` as one JSON line on startup; the direct
    /// socket address from it goes here.
    #[arg(long, env = "ORRERY_ADDR")]
    pub addr: std::net::SocketAddr,

    /// Number of synthetic entities to register in the uplink scheduler.
    /// Overridden by the manifest's inventory when `--manifest` is given.
    #[arg(long, default_value_t = DEFAULT_ENTITIES, env = "ORRERY_ENTITIES")]
    pub entities: u64,

    /// Minimum number of distinct interest cells the entity inventory must
    /// span. The rig refuses to run a placement confined to fewer cells —
    /// every pre-existing load path in the repo hardcodes `CellId::ROOT`
    /// (benches/journal_latency.rs, tests/cluster.rs, bin/persistd.rs), which
    /// cannot measure cross-cell handoff or the 27-cell area load.
    #[arg(long, default_value_t = DEFAULT_CELLS, env = "ORRERY_CELLS")]
    pub cells: u32,

    /// Per-entity diff rate, Hz, within the D16 1–4 Hz uplink range.
    #[arg(long, default_value_t = DEFAULT_DIFF_HZ, env = "ORRERY_DIFF_HZ")]
    pub diff_hz: f64,

    /// Intent mix as `kind=fraction` pairs relative to the diff rate, e.g.
    /// `trade=0.02,craft=0.01` means 2% of diff sends are replaced by a
    /// `trade` intent and 1% by a `craft` intent.
    #[arg(long, default_value = DEFAULT_INTENT_MIX, env = "ORRERY_INTENT_MIX")]
    pub intent_mix: String,

    /// Number of concurrent client sessions (iroh connections) to fan the
    /// load out over. Must satisfy the startup fan-out assert: sessions ×
    /// per-session flush capacity ≥ entities × diff_hz, or the rig would
    /// silently report queueing delay as commit latency.
    #[arg(long, default_value_t = DEFAULT_SESSIONS, env = "ORRERY_SESSIONS")]
    pub sessions: u32,

    /// Run duration, seconds.
    #[arg(long, default_value_t = 30, env = "ORRERY_P2_LOAD_DURATION_SECS")]
    pub duration_secs: u64,

    /// A seeder manifest (JSONL, docs/12-world-seeding.md §9.3: one entry per
    /// line — `(content_key, persist_id, grid, cell, value_digest, byte_len,
    /// archetype, layer, emit)` — plus a `content/version` header) to take the
    /// entity/cell inventory from. Without it the rig synthesizes a
    /// deterministic placement of `--entities` PersistIds over ≥ `--cells`
    /// cells at the interest level.
    #[arg(long, env = "ORRERY_MANIFEST")]
    pub manifest: Option<PathBuf>,

    /// Optional scenario file (TOML, docs/12-world-seeding.md §12.3) whose
    /// `[[workload]]` block supplies `diff_hz`/`intent_mix`/`duration` and the
    /// trajectory program (`motion`). Only meaningful with `--manifest`.
    #[arg(long, env = "ORRERY_SCENARIO")]
    pub scenario: Option<PathBuf>,

    /// Emit telemetry as one JSON object per line on stdout (the
    /// machine-parseable contract `gates/p2-dashboard` consumes). Tracing logs go to
    /// stderr.
    #[arg(long, env = "ORRERY_JSON")]
    pub json: bool,

    /// Append-only ack log: one JSON line per ack received (`diff` with
    /// `(entity, tick, lsn)`, `intent` with `(intent_id, tick)`), so a
    /// kill-9 harness can enumerate the pre-kill acked set and diff it
    /// against the post-restart manifest (docs/12-world-seeding.md §12.3).
    #[arg(long, env = "ORRERY_ACK_LOG")]
    pub ack_log: Option<PathBuf>,

    /// Verify an ack log against the promoted gateway and durable intent rows.
    #[arg(long)]
    pub verify_recovery: bool,

    /// FoundationDB cluster file for `--verify-recovery`.
    #[arg(long, env = "ORRERY_FDB_CLUSTER_FILE")]
    pub fdb_cluster_file: Option<PathBuf>,

    /// Adopted journal watermark reported by the promoted follower.
    #[arg(long, env = "ORRERY_RECOVERY_CUTOFF")]
    pub recovery_cutoff: Option<String>,

    /// Machine-readable report path for `--verify-recovery`.
    #[arg(long, env = "ORRERY_P2_LOAD_OUTPUT")]
    pub output: Option<PathBuf>,

    /// Diff payload size in bytes. 64 matches the D16 flush-budget math in
    /// `UplinkScheduler::flush` (`size = payload + 64`).
    #[arg(long, default_value_t = DEFAULT_DIFF_PAYLOAD_BYTES, env = "ORRERY_DIFF_PAYLOAD_BYTES")]
    pub diff_payload_bytes: usize,

    /// Rig-local iroh secret key (hex), pinning the rig's NodeId across runs.
    #[arg(long)]
    pub secret_key: Option<String>,

    /// Identity issuer secret key (hex) the rig signs its own session token
    /// with, paired with `--issuer-key-id`.
    ///
    /// `persistd` refuses to start without at least one `--issuer-key`, and it
    /// binds the token to the connection's authenticated NodeId — so a rig
    /// that cannot mint a token cannot get past `Hello`, whatever else it can
    /// measure. Give the gateway `--issuer-key <id>@<public key of this
    /// secret>` and the two agree. Without it the rig sends a placeholder
    /// token, which only a gateway configured with no verifier will admit.
    #[arg(long)]
    pub issuer_secret: Option<String>,

    /// The issuer key id carried in the rig's session token. Must match the
    /// id half of the gateway's `--issuer-key <id>@<key>`.
    #[arg(long, default_value_t = 1, env = "ORRERY_ISSUER_KEY_ID")]
    pub issuer_key_id: u32,

    /// The account id the rig's session token claims.
    #[arg(long, default_value_t = 1, env = "ORRERY_ACCOUNT_ID")]
    pub account_id: u64,
}

impl Cli {
    /// The run duration as a `Duration`.
    #[must_use]
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration_secs)
    }
}
