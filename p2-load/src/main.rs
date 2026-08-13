//! P2 latency rig — the D16 gate's load generator.
//!
//! One command drives the P2 demo load (docs/11-roadmap.md §P2): 10k
//! entities across 100+ cells at a calibrated diff and intent mix against a
//! real `persistd` gateway, emitting one JSON record per line for the four
//! D16 latency series (journal commit, bulk ack, intent commit, area first
//! page-in). `p2-dashboard --gate` reads that stream and exits non-zero when
//! any p99 misses its D16 target.
//!
//! Design notes:
//!
//! - **Transport.** The rig speaks the gateway wire surface directly over raw
//!   iroh datagrams + stream-framed control frames on one packet lane
//!   (roadmap decision C-1: there is no reliable-stream class in P2). The
//!   aeronet session stack is a Bevy client convenience; the persistd gateway
//!   is raw iroh (`crates/orrery_persistd/src/gateway.rs`), so the rig dials
//!   it without linking Bevy.
//! - **Measurement.** Diffs are registered in the *real* D16 1–4 Hz
//!   `UplinkScheduler` and intents in the real `IntentQueue`; acks feed the
//!   shared bounded-memory `LatencyHistogram`s, and the JSONL stream is
//!   drained from those histograms so the gate and the live rig read the same
//!   numbers from the same code path.
//! - **Inventory.** `--manifest` consumes a seeder manifest
//!   (docs/12-world-seeding.md §9.3); without one the rig synthesizes a
//!   deterministic placement of `--entities` PersistIds over ≥ `--cells`
//!   interest cells. The placement must not collapse to one cell — every
//!   pre-existing load path in the repo hardcodes `CellId::ROOT`.
//! - **Trajectory.** Movement is a closed-form `(entity, tick) → position`
//!   program (docs/12-world-seeding.md §12.3): each entity orbits its cell
//!   with a period chosen so crossings are continuous, which is what
//!   exercises cross-cell routing without a multi-gigabyte trace.
//!
//! Telemetry posture: **the OTel bridge (D12) is deferred.** This crate
//! deliberately adds no `opentelemetry` dependency — that stack would be a
//! new D14 pinned dependency, which is an orchestrator decision. The JSONL
//! contract documented in `p2-load/README.md` is the delivered telemetry
//! mechanism; `tracing` logs on stderr are diagnostic only and are not the
//! D12 bridge.

mod cli;
mod telemetry;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use clap::Parser;
use futures::FutureExt;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use serde::{Deserialize, Serialize};

use orrery_persist_client::config::PersistClientConfig;
use orrery_persist_client::{IntentQueue, UplinkScheduler};
use orrery_protocol::channels::{encode_datagram, encode_stream_frame, untag, Channel};
use orrery_protocol::{
    Attestation, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp,
    IntentOutcome, NodeId, PersistId, RecordKind, Tick,
};

use cli::Cli;
use telemetry::{RunContext, TelemetrySink};

/// The gateway ALPN (matches `orrery_persistd::gateway::GATEWAY_ALPN` and the
/// client crate's `gateway::GATEWAY_ALPN`; re-declared here so the rig links
/// neither crate's gateway module).
const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The rig's flush cadence. 20 Hz matches D16's send-rate default (the game
/// loop the scheduler is designed around) and gives the per-session fan-out
/// math its denominator.
const FLUSH_HZ: u64 = 20;

/// The per-session flush byte budget, = the D16 client default
/// (`PersistClientConfig::flush_budget_bytes`, 1024). One session sustains
/// `budget / (payload + 64)` diffs per flush (uplink.rs:160-163); the startup
/// fan-out assert is checked against exactly this number.
const FLUSH_BUDGET_BYTES: usize = 1024;

/// The uplink scheduler's per-diff overhead estimate (`size = payload + 64`,
/// uplink.rs flush). The fan-out math sizes sessions against this constant.
const DIFF_OVERHEAD_BYTES: usize = 64;

/// The intent-mix RNG modulus. Per-entity sends are counted; every send whose
/// `(entity_hash + send_index) mod 1_000_000` falls inside a mix bucket is
/// upgraded to an intent of that kind. `1_000_000` gives three decimal places
/// of fraction resolution, comfortably under the 0.01–0.1 mixes in D16-shaped
/// workloads.
const MIX_MODULUS: u64 = 1_000_000;

fn main() -> ExitCode {
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => match rt.block_on(run()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: tokio runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    // Logs on stderr (the p0-nat-test contract): stdout is the JSONL stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // ── Inventory: manifest or deterministic synthetic placement ─────────
    let inventory = match &cli.manifest {
        Some(path) => {
            let inv = ManifestInventory::load(path)
                .with_context(|| format!("load manifest {}", path.display()))?;
            inv.inventory
        }
        None => synthetic_inventory(cli.entities, cli.cells),
    };
    if inventory.is_empty() {
        bail!("empty entity inventory (manifest has no entries, or --entities 0)");
    }
    let distinct_cells = inventory
        .iter()
        .map(|e| e.cell)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if (distinct_cells as u64) < u64::from(cli.cells) && cli.manifest.is_none() {
        bail!(
            "synthetic placement produced only {distinct_cells} distinct cells (< {}): \
             the load would be confined, and every pre-existing load path in this repo \
             already hardcodes CellId::ROOT",
            cli.cells
        );
    }

    // ── Load profile: CLI, optionally overridden by a scenario workload ──
    let mut diff_hz = cli.diff_hz;
    let mut intent_mix = parse_intent_mix(&cli.intent_mix)?;
    let mut duration = cli.duration();
    if let Some(scenario_path) = &cli.scenario {
        let workload = Workload::load(scenario_path)
            .with_context(|| format!("load scenario {}", scenario_path.display()))?;
        diff_hz = workload.diff_hz.unwrap_or(diff_hz);
        intent_mix = workload.intent_mix.unwrap_or(intent_mix);
        duration = workload.duration.unwrap_or(duration);
        tracing::info!(
            scenario = %scenario_path.display(),
            diff_hz,
            duration_secs = duration.as_secs(),
            "scenario [[workload]] overrides applied (docs/12 §12.3)"
        );
    }

    // ── Fan-out assert (the rig must not report queueing as commit latency)
    check_fan_out(
        cli.sessions,
        cli.diff_payload_bytes,
        inventory.len() as u64,
        diff_hz,
    )?;

    // ── Transport ────────────────────────────────────────────────────────
    let endpoint = bind_endpoint(cli.secret_key.as_deref()).await?;
    tracing::info!(node = %endpoint.id(), addr = %cli.addr, "rig endpoint up");

    let mut sessions = Vec::new();
    for i in 0..cli.sessions {
        let conn = dial(&endpoint, cli.addr, cli.gateway)
            .await
            .with_context(|| format!("dial session {i}"))?;
        sessions.push(conn);
    }
    tracing::info!(sessions = sessions.len(), "gateway sessions connected");

    // ── Ack log (kill-9 harness input) ───────────────────────────────────
    let ack_log = match &cli.ack_log {
        Some(path) => Some(AckLog::open(path)?),
        None => None,
    };

    // ── Run context header ───────────────────────────────────────────────
    if cli.json {
        telemetry::run_header(&RunContext {
            gateway: cli.gateway.to_string(),
            addr: cli.addr.to_string(),
            entities: inventory.len() as u64,
            cells: distinct_cells as u64,
            sessions: u64::from(cli.sessions),
            diff_hz,
            intent_mix: intent_mix.clone(),
            duration_secs: duration.as_secs(),
        });
    }

    // ── The run ──────────────────────────────────────────────────────────
    let rig_endpoint = endpoint.clone();
    let rig = Rig {
        cli: &cli,
        emit_json: cli.json,
        endpoint,
        sessions,
        inventory,
        diff_hz,
        intent_mix,
        duration,
        ack_log,
    };
    let outcome = rig.drive().await;

    if cli.json {
        match &outcome {
            Ok(stats) => telemetry::run_footer(&format!(
                "duration elapsed; diffs={} acks={} intents={} intent_acks={}",
                stats.diffs_sent, stats.diff_acks, stats.intents_sent, stats.intent_acks
            )),
            Err(e) => telemetry::run_footer(&format!("run failed: {e:#}")),
        }
    }
    outcome?;

    // Close the endpoint cleanly so iroh does not log an ungraceful abort at
    // drop time.
    rig_endpoint.close().await;
    Ok(())
}

/// The parsed `--intent-mix`: `kind=fraction` pairs, fractions in `[0, 1]`.
type IntentMix = BTreeMap<String, f64>;

fn parse_intent_mix(s: &str) -> Result<IntentMix> {
    let mut out = IntentMix::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (kind, frac) = pair
            .split_once('=')
            .with_context(|| format!("--intent-mix entry '{pair}' is not kind=fraction"))?;
        let frac: f64 = frac
            .parse()
            .with_context(|| format!("--intent-mix entry '{pair}': fraction is not a number"))?;
        if !(0.0..=1.0).contains(&frac) {
            bail!("--intent-mix entry '{pair}': fraction must be in [0, 1]");
        }
        out.insert(kind.trim().to_string(), frac);
    }
    let total: f64 = out.values().sum();
    if total > 1.0 {
        bail!("--intent-mix fractions sum to {total} > 1.0 (mix is relative to the diff rate)");
    }
    Ok(out)
}

/// The startup fan-out assert. One session sustains
/// `FLUSH_BUDGET_BYTES / (payload + DIFF_OVERHEAD_BYTES)` diffs per flush at
/// `FLUSH_HZ` flushes per second; if `sessions × capacity < entities ×
/// diff_hz` the scheduler's queues grow without bound and the rig would
/// silently report queueing delay as commit latency. Refuse instead.
fn check_fan_out(sessions: u32, payload_bytes: usize, entities: u64, diff_hz: f64) -> Result<()> {
    let per_flush = FLUSH_BUDGET_BYTES / (payload_bytes + DIFF_OVERHEAD_BYTES).max(1);
    let per_session = (per_flush as u64) * FLUSH_HZ;
    let capacity = (per_session as u128) * (sessions as u128);
    let demand = (entities as u128) * ((diff_hz * 1000.0) as u128) / 1000;
    if capacity < demand {
        bail!(
            "fan-out too small: {sessions} session(s) × {per_session} diffs/s = {} < \
             {entities} entities × {diff_hz} Hz = {demand}. Increase --sessions or lower \
             --entities/--diff-hz — otherwise the rig measures queueing delay, not commit \
             latency (UplinkScheduler::flush budget: {FLUSH_BUDGET_BYTES} B, size = payload \
             + {DIFF_OVERHEAD_BYTES} B).",
            capacity
        );
    }
    tracing::info!(
        sessions,
        per_session,
        entities,
        diff_hz,
        "fan-out assert satisfied"
    );
    Ok(())
}

/// Bind the rig-local iroh endpoint (relay disabled — the rig is
/// gateway-colocated by design, docs/11-roadmap.md §P2 "gateway-colocated
/// load generator"; a relayed path would inflate the client-observed series).
async fn bind_endpoint(secret_key: Option<&str>) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![GATEWAY_ALPN.to_vec()]);
    if let Some(hex) = secret_key {
        let sk: SecretKey = hex.parse().context("invalid --secret-key (expected hex)")?;
        builder = builder.secret_key(sk);
    }
    builder.bind().await.context("bind rig endpoint")
}

/// Dial the gateway and complete the admission + hello handshake.
///
/// Mirrors the persistd gateway's session shape (`handle_connection` in
/// crates/orrery_persistd/src/gateway.rs): the server streams one admission
/// uni (`[ACCEPTED]`) on connect, then speaks tagged datagrams + stream-framed
/// control frames. The rig must read the admission stream before its hello is
/// answered (the server sends the `HelloAck` as a stream frame on the packet
/// lane, per C-1).
async fn dial(endpoint: &Endpoint, addr: SocketAddr, gateway: NodeId) -> Result<Connection> {
    let endpoint_addr = EndpointAddr::new(gateway).with_ip_addr(addr);
    let conn = endpoint
        .connect(endpoint_addr, GATEWAY_ALPN)
        .await
        .with_context(|| format!("connect to gateway at {addr}"))?;

    // Read the admission uni-stream the server opens (`[ACCEPTED]`).
    let mut admission = conn
        .accept_uni()
        .await
        .context("accept gateway admission stream")?;
    let mut byte = [0u8; 1];
    admission
        .read_exact(&mut byte)
        .await
        .context("read admission byte")?;
    if byte[0] != 0 {
        bail!(
            "gateway admission byte was {}, expected 0 (ACCEPTED)",
            byte[0]
        );
    }

    // Hello, then require the ack to name the gateway we dialed.
    send_msg(
        &conn,
        &GatewayMsg::Hello {
            token: b"p2-load".to_vec(),
            node: endpoint.id(),
        },
    );
    let reply = recv_reply(&conn, Duration::from_secs(10))
        .await
        .context("await HelloAck")?;
    match reply {
        GatewayReply::HelloAck {
            gateway: id,
            protocol,
        } => {
            if id != gateway {
                bail!(
                    "HelloAck names gateway {id}, but --gateway says {gateway}: refusing to \
                     load-test the wrong node"
                );
            }
            tracing::info!(%id, protocol, "gateway hello acknowledged");
        }
        other => bail!("expected HelloAck, got {other:?}"),
    }
    Ok(conn)
}

/// Non-blocking receive: `None` when no datagram is queued. The load loop
/// must never await the network — a blocked receive would starve the 20 Hz
/// flush clock and inflate the very latencies the rig measures. Errors are
/// reported as `Some(Err(..))` so the caller can log and drop the session.
///
/// Implemented as a zero-duration timeout around the read: a ready datagram
/// resolves immediately, a pending one yields `Elapsed`. This needs no
/// `futures` dep and no unsafe pinning.
fn try_recv_datagram(conn: &Connection) -> Option<Result<Bytes, String>> {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => match conn.read_datagram().now_or_never() {
            Some(Ok(pkt)) => Some(Ok(pkt)),
            Some(Err(e)) => Some(Err(e.to_string())),
            None => None,
        },
        Err(_) => None,
    }
}

/// Send one `GatewayMsg` on the packet lane. Bulk diffs are tagged datagrams;
/// Hello/Subscribe/SubmitIntent are stream-framed control frames (C-1: one
/// lane; the tag + length prefix is what routes them server-side).
fn send_msg(conn: &Connection, msg: &GatewayMsg) {
    let bytes = match msg {
        GatewayMsg::Diff { .. } => Bytes::from(encode_datagram(msg)),
        _ => Bytes::from(encode_stream_frame(msg)),
    };
    if let Err(e) = conn.send_datagram(bytes) {
        tracing::warn!(error = %e, "gateway datagram send failed");
    }
}

/// Await the next decodable `GatewayReply` (any channel) within `timeout`.
async fn recv_reply(conn: &Connection, timeout: Duration) -> Result<GatewayReply> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timeout waiting for gateway reply");
        }
        let pkt = match tokio::time::timeout(remaining, conn.read_datagram()).await {
            Ok(Ok(pkt)) => pkt,
            Ok(Err(e)) => bail!("gateway connection closed: {e}"),
            Err(_) => bail!("timeout waiting for gateway reply"),
        };
        let Some((channel, rest)) = untag(&pkt) else {
            continue;
        };
        let reply: Option<GatewayReply> = match channel {
            Channel::State => postcard::from_bytes(rest).ok(),
            Channel::Control => orrery_protocol::channels::decode_stream_frame(&pkt),
        };
        if let Some(reply) = reply {
            return Ok(reply);
        }
        tracing::debug!("gateway: undecodable reply datagram");
    }
}

/// One inventory entry: an entity placed in a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    /// The entity.
    entity: PersistId,
    /// Its interest-level cell.
    cell: CellId,
}

/// The rig's entity/cell inventory.
type Inventory = Vec<Placement>;

/// Deterministic rig-internal placement: `--entities` PersistIds over
/// ≥ `--cells` distinct interest-level cells.
///
/// Entities round-robin over a lattice of cell coordinates centered on the
/// origin at the interest level (D16 cell edge 128 m). The lattice side is
/// the cube root of the requested cell count, so `--cells 128` yields a
/// 6×6×4-ish lattice whose distinct-cell count is ≥ the request. The
/// trajectory program (`Trajectory`) moves each entity across cell
/// boundaries, so placement is the *initial* state, not the coverage.
fn synthetic_inventory(entities: u64, cells: u32) -> Inventory {
    let cells = cells.max(1) as u64;
    // Lattice side: ceil(cbrt(cells)) per axis, clamped to the interest-level
    // coordinate range (±2^20 at level 21; D16 128 m cells).
    let mut side = 1u64;
    while side * side * side < cells {
        side += 1;
    }
    let mut inventory = Vec::with_capacity(entities as usize);
    for i in 0..entities {
        // Round-robin the lattice in x-fastest order, wrapping in z so a cell
        // count that is not a cube still covers ≥ `cells` distinct cells
        // (test `inventory_covers_at_least_the_requested_cells`).
        let idx = i % (side * side * side);
        let x = (idx % side) as i32;
        let y = ((idx / side) % side) as i32;
        let z = (idx / (side * side)) as i32;
        let cell = CellId::from_coords(glam::IVec3::new(x, y, z), orrery_protocol::INTEREST_LEVEL)
            .expect("lattice coordinate is in range at the interest level");
        inventory.push(Placement {
            entity: PersistId::new(i + 1),
            cell,
        });
    }
    inventory
}

/// A closed-form trajectory program (docs/12-world-seeding.md §12.3): each
/// entity walks a small circle centered on its cell's origin, with a radius
/// just over one cell diagonal so the walk crosses cell boundaries
/// continuously. Position is a pure function of `(entity, tick)` — seekable,
/// stateless, and a few hundred bytes for the whole fleet.
struct Trajectory {
    /// Angular speed, rad/tick, per entity (derived from its id so the fleet
    /// does not cross cells in lockstep).
    omega: f64,
    /// Phase offset, rad.
    phase: f64,
}

impl Trajectory {
    /// The trajectory for one entity. The crossing rate comes out at roughly
    /// one cell per few seconds at 4 Hz — enough to exercise cross-cell
    /// routing continuously without churning the shard map.
    fn for_entity(entity: PersistId) -> Self {
        let h = entity.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        Self {
            // ω spread over [0.01, 0.05] rad/tick.
            omega: 0.01 + 0.04 * ((h >> 32) as f64 / u32::MAX as f64),
            phase: (h as u32) as f64 / u32::MAX as f64 * std::f64::consts::TAU,
        }
    }

    /// The cell offset (in interest cells, each axis in `{-1, 0, 1}`) of this
    /// entity at `tick`, relative to its inventory cell. The walk crosses a
    /// boundary whenever the offset changes — the cell the diff is routed to
    /// changes with it, which is the coverage the rig exists to produce.
    fn cell_offset(&self, tick: u64) -> (i32, i32) {
        let a = self.phase + self.omega * tick as f64;
        // Radius 1.5 cells: the circle's projection on each axis crosses the
        // cell boundary twice per period.
        let r = 1.5;
        let dx = (r * a.cos()).round() as i32;
        let dz = (r * a.sin()).round() as i32;
        (dx, dz)
    }
}

/// One `[[workload]]` block from a scenario TOML (docs/12-world-seeding.md
/// §12.3). Only the fields the rig consumes are modeled; unknown fields are
/// ignored so the same scenario file drives the seeder and the rig.
#[derive(Debug, Deserialize)]
struct ScenarioFile {
    /// The workload blocks; the rig takes the first.
    #[serde(default)]
    workload: Vec<WorkloadToml>,
}

#[derive(Debug, Deserialize)]
struct WorkloadToml {
    /// Workload name (informational).
    #[allow(dead_code)]
    name: Option<String>,
    /// Per-entity diff rate (Hz).
    diff_hz: Option<f64>,
    /// Intent mix (`kind = fraction`).
    intent_mix: Option<BTreeMap<String, f64>>,
    /// Run duration, e.g. "30m", "90s".
    duration: Option<String>,
}

/// The rig-relevant slice of a scenario `[[workload]]` block.
struct Workload {
    diff_hz: Option<f64>,
    intent_mix: Option<IntentMix>,
    duration: Option<Duration>,
}

impl Workload {
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let parsed: ScenarioFile = toml::from_str(&text)
            .with_context(|| format!("parse scenario TOML {}", path.display()))?;
        let Some(w) = parsed.workload.into_iter().next() else {
            bail!("scenario {} has no [[workload]] block", path.display());
        };
        Ok(Self {
            diff_hz: w.diff_hz,
            intent_mix: w.intent_mix,
            duration: w.duration.as_deref().map(parse_duration).transpose()?,
        })
    }
}

/// Parse a scenario duration string: `"30m"`, `"90s"`, `"1h"`.
fn parse_duration(s: &str) -> Result<Duration> {
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = digits
        .parse()
        .with_context(|| format!("workload duration '{s}': bad number"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        other => bail!("workload duration '{s}': unknown unit '{other}' (use s/m/h)"),
    };
    Ok(Duration::from_secs(secs))
}

/// The seeder manifest (docs/12-world-seeding.md §9.3): one JSONL entry per
/// seeded row, `(content_key, persist_id, grid, cell, value_digest, byte_len,
/// archetype, layer, emit)` in canonical `(grid, cell, content_key)` order,
/// plus a `content/version` header line. The rig consumes the
/// `(persist_id, cell)` inventory and ignores the digest/archetype fields —
/// they exist for the seeder's diff/patch flow (§9.4), not for load shape.
struct ManifestInventory {
    inventory: Inventory,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestLine {
    /// The `content/version` header (§9.3: records `(content_build,
    /// manifest_digest, scenario_seed, config_digest, toolchain, seeded_at)`).
    Header {
        /// Discriminator (`content/version` header line, §9.3).
        #[allow(dead_code)]
        content_version: serde_json::Value,
    },
    /// One seeded-row entry.
    Entry(ManifestEntry),
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    /// The cluster-minted persistent entity id.
    persist_id: PersistId,
    /// The interest-level cell the row lives in (raw bits; the manifest's
    /// canonical form per §9.3, and how `CellId` serializes).
    cell: CellId,
    /// The grid the cell is relative to (P-7). The rig loads only root-grid
    /// inventories in P2; a non-root grid entry is skipped with a warning.
    grid: Option<GridId>,
}

impl ManifestInventory {
    fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut inventory = Vec::new();
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), lineno + 1))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parsed: ManifestLine = serde_json::from_str(line)
                .with_context(|| format!("parse {}:{}", path.display(), lineno + 1))?;
            match parsed {
                ManifestLine::Header { .. } => {}
                ManifestLine::Entry(entry) => {
                    if entry.grid.is_some_and(|g| g != GridId::ROOT) {
                        tracing::warn!(
                            line = lineno + 1,
                            "manifest entry in a non-root grid skipped (P2 loads root-grid \
                             inventories only)"
                        );
                        continue;
                    }
                    inventory.push(Placement {
                        entity: entry.persist_id,
                        cell: entry.cell,
                    });
                }
            }
        }
        Ok(Self { inventory })
    }
}

/// The append-only ack log: one JSON line per ack, so a kill-9 harness can
/// enumerate the pre-kill acked set and diff it against the post-restart
/// manifest (docs/12-world-seeding.md §12.3).
struct AckLog {
    writer: BufWriter<std::fs::File>,
}

/// One ack-log record. Tagged so the harness can split diff acks from intent
/// acks; `lsn` is the durable journal position the gateway acked (the
/// watermark the kill-9 assertion compares against).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AckRecord {
    /// A bulk diff was durably journaled.
    Diff {
        /// The entity.
        entity: PersistId,
        /// The diff's tick.
        tick: Tick,
        /// The durable journal position.
        lsn: orrery_protocol::Lsn,
    },
    /// An intent committed.
    Intent {
        /// The intent id (idempotency key), as a decimal string: serde_json
        /// has no arbitrary-precision u128 support, and the harness compares
        /// by equality, so the string form is lossless for this contract.
        intent_id: String,
        /// The commit tick.
        tick: Tick,
    },
}

impl AckLog {
    fn open(path: &Path) -> Result<Self> {
        // Append-only by construction: `create` truncates a stale log from a
        // previous run at the same path, then every write is an append. The
        // kill-9 harness reads the log up to the kill point.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("open --ack-log {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    fn record(&mut self, record: &AckRecord) {
        // A failed ack-log write must not kill the run: the log is the
        // harness's enumeration input, not the durability path. Errors are
        // logged (stderr) so a silently-empty log is never mistaken for "no
        // acks".
        match serde_json::to_string(record) {
            Ok(line) => {
                if let Err(e) = writeln!(self.writer, "{line}").and_then(|()| self.writer.flush()) {
                    tracing::warn!(error = %e, "ack-log write failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "ack-log serialize failed"),
        }
    }
}

/// Run statistics for the footer.
#[derive(Debug, Default)]
struct RunStats {
    diffs_sent: u64,
    diff_acks: u64,
    intents_sent: u64,
    intent_acks: u64,
}

/// The rig, mid-run.
struct Rig<'a> {
    cli: &'a Cli,
    /// Whether JSONL telemetry goes to stdout (`cli.json`). A field rather
    /// than a read of `cli.json` so a future integration harness can run the
    /// drive loop silently without rebuilding the CLI.
    emit_json: bool,
    endpoint: Endpoint,
    sessions: Vec<Connection>,
    inventory: Inventory,
    diff_hz: f64,
    intent_mix: IntentMix,
    duration: Duration,
    ack_log: Option<AckLog>,
}

impl Rig<'_> {
    /// Drive the load loop until the duration elapses.
    async fn drive(mut self) -> Result<RunStats> {
        let mut sched = UplinkScheduler::new();
        let cfg = PersistClientConfig {
            flush_budget_bytes: FLUSH_BUDGET_BYTES,
            ..PersistClientConfig::default()
        };
        let mut intents = IntentQueue::new(1024);

        // Register every entity at the calibrated rate (inside the D16 1–4 Hz
        // uplink range).
        for p in &self.inventory {
            sched.register(p.entity, self.diff_hz as f32);
        }

        // Per-session state: scheduler shards, send counters, tick clock.
        let sessions = self.sessions.len().max(1);
        let shard_size = self.inventory.len().div_ceil(sessions);
        let tick = Arc::new(AtomicU64::new(0));
        let mut stats = RunStats::default();
        let sink = TelemetrySink::new();

        // Area load: one 27-cell subscribe per session at startup, measuring
        // time-to-first-page per session (D16: area first page-in < 50 ms).
        let mut area_pending: Vec<Option<Instant>> = vec![None; sessions];
        for (i, conn) in self.sessions.iter().enumerate() {
            let center = self
                .inventory
                .get(i * shard_size)
                .map_or(self.inventory[0].cell, |p| p.cell);
            let cells = center.neighbors27();
            send_msg(
                conn,
                &GatewayMsg::Subscribe {
                    grid: GridId::ROOT,
                    cells,
                },
            );
            area_pending[i] = Some(Instant::now());
        }

        let flush_period = Duration::from_secs_f64(1.0 / FLUSH_HZ as f64);
        let start = Instant::now();
        let mut elapsed = Duration::ZERO;
        let mut next_flush = start;

        while start.elapsed() < self.duration {
            // ── Receive: drain every pending datagram on every session ──
            // Collect first (borrowing the sessions), then handle (borrowing
            // self mutably): the two borrows cannot overlap in one loop.
            let mut inbox: Vec<(usize, Bytes)> = Vec::new();
            for (i, conn) in self.sessions.iter().enumerate() {
                loop {
                    match try_recv_datagram(conn) {
                        Some(Ok(pkt)) => inbox.push((i, pkt)),
                        Some(Err(e)) => {
                            tracing::warn!(session = i, error = %e, "gateway read failed");
                            break;
                        }
                        None => break,
                    }
                }
            }
            for (i, pkt) in inbox {
                self.handle_reply(
                    i,
                    &pkt,
                    &mut sched,
                    &mut intents,
                    &mut stats,
                    &mut area_pending,
                );
            }

            // ── Load: queue each entity's diff for this flush window ─────
            if start.elapsed() >= next_flush.duration_since(start) {
                // Compute each entity's current cell from its trajectory and
                // queue a fresh diff (newest-wins replaces the pending one).
                for p in &self.inventory {
                    let tick_now = tick.load(Ordering::Relaxed);
                    let cell = moved_cell(p, tick_now);
                    sched.queue(DiffUplink {
                        cell,
                        grid: GridId::ROOT,
                        entity: p.entity,
                        tick: Tick::new(tick_now),
                        kind: RecordKind::ComponentDiff,
                        payload: synthetic_payload(p.entity, tick_now, self.cli.diff_payload_bytes),
                        seq: tick_now,
                    });
                }
                tick.fetch_add(1, Ordering::Relaxed);

                // Flush, then fan the selected diffs out over the sessions
                // round-robin (one scheduler across sessions keeps the
                // priority math global; the budget was asserted per-session).
                let out = sched.flush(&cfg, elapsed);
                for (n, diff) in out.iter().enumerate() {
                    // Intent mix: a fraction of sends is upgraded to an intent
                    // instead of a diff (docs/12 §12.3 `intent_mix`). The
                    // decision is deterministic per (entity, send index).
                    if let Some(kind) = self.intent_for(diff.entity, diff.seq) {
                        let id = intent_id(diff.entity, diff.seq);
                        let intent = self.make_intent(id, kind);
                        if intents.submit(intent).is_some() {
                            stats.intents_sent += 1;
                        }
                        // The diff is still sent: the intent is *in addition
                        // to* the bulk stream (trades/crafts do not replace
                        // the entity's state diff).
                    }
                    send_msg(
                        &self.sessions[n % sessions],
                        &GatewayMsg::Diff { diff: diff.clone() },
                    );
                    stats.diffs_sent += 1;
                }
                // Drain queued intents to the wire.
                for intent in intents.drain() {
                    send_msg(
                        &self.sessions[(intent.intent_id as usize) % sessions],
                        &GatewayMsg::SubmitIntent { intent },
                    );
                }

                next_flush += flush_period;
            }

            // ── Telemetry drain (bounded-memory histograms → JSONL) ──────
            // The drain writes to stdout, which the test harness owns; the
            // `emit_json` flag is false under `cargo test` so the drive loop
            // stays silent there. The drain logic itself is covered by the
            // telemetry module's tests.
            if self.emit_json {
                sink.drain_histogram(telemetry::SERIES_BULK_ACK, sched.ack_latency());
                sink.drain_histogram(telemetry::SERIES_INTENT_COMMIT, intents.intent_latency());
            }

            tokio::time::sleep(Duration::from_millis(1)).await;
            elapsed = start.elapsed();
        }

        // Final drain.
        if self.emit_json {
            sink.drain_histogram(telemetry::SERIES_BULK_ACK, sched.ack_latency());
            sink.drain_histogram(telemetry::SERIES_INTENT_COMMIT, intents.intent_latency());
        }
        tracing::info!(
            diffs = stats.diffs_sent,
            acks = stats.diff_acks,
            intents = stats.intents_sent,
            intent_acks = stats.intent_acks,
            bulk_p99_us = sched.ack_latency().p99().as_micros() as u64,
            intent_p99_us = intents.intent_latency().p99().as_micros() as u64,
            "run complete"
        );
        Ok(stats)
    }

    /// Handle one inbound datagram from session `i`.
    #[allow(clippy::too_many_arguments)]
    fn handle_reply(
        &mut self,
        session: usize,
        pkt: &[u8],
        sched: &mut UplinkScheduler,
        intents: &mut IntentQueue,
        stats: &mut RunStats,
        area_pending: &mut [Option<Instant>],
    ) {
        let Some((channel, rest)) = untag(pkt) else {
            return;
        };
        let reply: Option<GatewayReply> = match channel {
            Channel::State => postcard::from_bytes(rest).ok(),
            Channel::Control => orrery_protocol::channels::decode_stream_frame(pkt),
        };
        let Some(reply) = reply else {
            tracing::debug!(session, "undecodable gateway reply");
            return;
        };
        match reply {
            GatewayReply::BulkAck {
                entity,
                tick,
                lsn,
                provisional,
            } => {
                sched.on_ack(entity, tick, provisional);
                stats.diff_acks += 1;
                if let Some(log) = &mut self.ack_log {
                    log.record(&AckRecord::Diff { entity, tick, lsn });
                }
            }
            GatewayReply::BulkNack {
                entity,
                tick,
                reason,
            } => {
                sched.on_nack(entity, tick);
                tracing::debug!(session, ?entity, ?tick, reason, "bulk nack");
            }
            GatewayReply::IntentAck { intent_id, outcome } => {
                if let IntentOutcome::Committed { tick, .. } = &outcome {
                    if let Some(log) = &mut self.ack_log {
                        log.record(&AckRecord::Intent {
                            intent_id: intent_id.to_string(),
                            tick: *tick,
                        });
                    }
                    stats.intent_acks += 1;
                }
                intents.on_ack(intent_id, outcome);
            }
            GatewayReply::AreaPage { .. } => {
                if let Some(t0) = area_pending.get_mut(session).and_then(|s| s.take()) {
                    let dt = t0.elapsed();
                    if self.cli.json {
                        telemetry::sample(telemetry::SERIES_AREA_FIRST_PAGE, dt.as_micros() as u64);
                    }
                    tracing::debug!(session, first_page_ms = dt.as_millis(), "first area page");
                }
            }
            GatewayReply::HelloAck { .. } => {}
        }
    }

    /// Whether this send is upgraded to an intent, and of which kind.
    fn intent_for(&self, entity: PersistId, send_index: u64) -> Option<String> {
        if self.intent_mix.is_empty() {
            return None;
        }
        let h = entity
            .0
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(send_index);
        let bucket = h % MIX_MODULUS;
        let mut cursor = 0f64;
        for (kind, frac) in &self.intent_mix {
            cursor += frac;
            if (bucket as f64) < cursor * MIX_MODULUS as f64 {
                return Some(kind.clone());
            }
        }
        None
    }

    /// Build a minimal intent of `kind` bound to the entity's current cell
    /// epoch. The P2 gateway's intent path is a stub (signature check →
    /// `Ruleset` validation stub → optimistic commit, roadmap §P2): the rig's
    /// job is to make the *commit latency* measurable at the calibrated mix,
    /// so the intent is wire-shaped and empty of ops.
    fn make_intent(&self, id: u128, kind: String) -> Intent {
        Intent {
            intent_id: id,
            issuer: self.endpoint.id(),
            cell_epoch: Epoch::new(0),
            ops: vec![IntentOp {
                op: op_code(&kind),
                args: Bytes::from(kind.into_bytes()),
            }],
            attestations: Vec::<Attestation>::new(),
            signature: dummy_signature(),
        }
    }
}

/// A stable op code per intent kind (FNV-1a over the kind string, truncated
/// to the wire type's u16). The P2 gateway stub ignores `op`; the code exists
/// so the intent stream is distinguishable in the gateway's logs.
fn op_code(kind: &str) -> u16 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in kind.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    (h >> 48) as u16
}

/// The P2 gateway's intent stub does not verify signatures (roadmap §P2:
/// attestation arrives in P5), so the rig signs with a fixed key — the field
/// is wire-shaped, not meaningful.
fn dummy_signature() -> orrery_protocol::Signature {
    static KEY: std::sync::OnceLock<iroh::SecretKey> = std::sync::OnceLock::new();
    KEY.get_or_init(|| iroh::SecretKey::from_bytes(&[0x2du8; 32]))
        .sign(b"p2-load intent")
}

/// A deterministic intent id for (entity, send index): high 64 bits the
/// entity, low 64 the send index. Idempotency keys are per-intent (D11 §2.2);
/// this makes the rig's keys unique per send and stable across resends.
fn intent_id(entity: PersistId, send_index: u64) -> u128 {
    ((entity.0 as u128) << 64) | send_index as u128
}

/// The cell an entity's trajectory has moved it into at `tick`.
fn moved_cell(p: &Placement, tick: u64) -> CellId {
    let traj = Trajectory::for_entity(p.entity);
    let (dx, dz) = traj.cell_offset(tick);
    if dx == 0 && dz == 0 {
        return p.cell;
    }
    let (coords, level) = p.cell.coords();
    CellId::from_coords(coords + glam::IVec3::new(dx, 0, dz), level).unwrap_or(p.cell)
}

/// A synthetic component payload: deterministic per (entity, tick), exactly
/// `len` bytes. Deterministic so a despawn/respawn comparison can spot
/// corruption; sized to the D16 flush-budget math (`payload + 64`).
fn synthetic_payload(entity: PersistId, tick: u64, len: usize) -> Bytes {
    let mut out = Vec::with_capacity(len.max(16));
    out.extend_from_slice(&entity.0.to_le_bytes());
    out.extend_from_slice(&tick.to_le_bytes());
    let mut x = entity.0 ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    while out.len() < len {
        // xorshift64* filler.
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len.max(16));
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_covers_at_least_the_requested_cells() {
        // The D16 demo load: 10k entities over ≥ 128 distinct interest cells.
        // Assert the generated placement yields exactly `--entities` ids and
        // ≥ `--cells` distinct cells — a placement confined to one cell is
        // the pre-existing failure mode (every other load path hardcodes
        // CellId::ROOT) and cannot measure cross-cell routing.
        let inventory = synthetic_inventory(10_000, 128);
        assert_eq!(inventory.len(), 10_000);
        let distinct: std::collections::BTreeSet<_> = inventory.iter().map(|p| p.cell).collect();
        assert!(
            distinct.len() >= 128,
            "placement must span ≥ 128 distinct cells, got {}",
            distinct.len()
        );
        // And the ids are exactly PersistId 1..=10_000, unique.
        let ids: std::collections::BTreeSet<_> = inventory.iter().map(|p| p.entity).collect();
        assert_eq!(ids.len(), 10_000);
        assert_eq!(inventory[0].entity, PersistId::new(1));
        assert_eq!(inventory[9_999].entity, PersistId::new(10_000));

        // Every cell is a valid interest-level cell (level 21, D16 128 m).
        for c in &distinct {
            assert_eq!(c.level(), orrery_protocol::INTEREST_LEVEL);
        }

        // A larger request scales: 1 000 entities over ≥ 100 cells (the demo
        // floor).
        let inv2 = synthetic_inventory(1_000, 100);
        let distinct2: std::collections::BTreeSet<_> = inv2.iter().map(|p| p.cell).collect();
        assert!(distinct2.len() >= 100, "got {}", distinct2.len());
    }

    #[test]
    fn trajectory_moves_entities_across_cells() {
        // The closed-form program must actually cross cell boundaries: an
        // entity whose cell offset never changes is confined. Sample the
        // first 100 entities over 4 000 ticks (~17 min at 4 Hz) and require
        // every one to leave its inventory cell at least once.
        for i in 1..=100u64 {
            let entity = PersistId::new(i);
            let traj = Trajectory::for_entity(entity);
            let mut left = false;
            for tick in 0..4_000u64 {
                if traj.cell_offset(tick) != (0, 0) {
                    left = true;
                    break;
                }
            }
            assert!(left, "entity {i} never left its inventory cell");
        }
    }

    #[test]
    fn fan_out_assert_accepts_default_and_rejects_undersized() {
        // The math the assert protects (UplinkScheduler::flush budget 1024 B,
        // size = payload + 64 B, 20 flushes/s): one session sustains
        // 1024/128 × 20 = 160 diffs/s at the default 64-byte payload. The
        // default load is 10 000 entities × 2 Hz = 20 000 diffs/s, so the
        // default of 6 sessions is *deliberately* too small — the assert is
        // the guard rail that makes an operator raise --sessions rather than
        // silently measure queueing delay. Pin the boundary: 125 sessions
        // pass (capacity == demand), 6 fail.
        assert!(check_fan_out(125, 64, 10_000, 2.0).is_ok());
        let err = check_fan_out(6, 64, 10_000, 2.0).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("fan-out too small"), "unexpected error: {msg}");
        // A conforming small run passes: 1 000 entities × 2 Hz = 2 000/s ≤
        // 13 sessions × 160/s = 2 080/s.
        assert!(check_fan_out(13, 64, 1_000, 2.0).is_ok());
    }

    #[test]
    fn intent_mix_parse_and_bounds() {
        // The mix parser must reject sums > 1 (a mix is a *fraction of* the
        // diff rate) and non-numeric/out-of-range entries. The pairs below
        // exercise the accepted and rejected shapes against hand-computed
        // values.
        let mix = parse_intent_mix("trade=0.02,craft=0.01").unwrap();
        assert_eq!(mix.len(), 2);
        assert!((mix["trade"] - 0.02).abs() < f64::EPSILON);
        assert!(parse_intent_mix("trade=1.5").is_err());
        assert!(parse_intent_mix("trade=abc").is_err());
        assert!(parse_intent_mix("trade=0.6,craft=0.6").is_err());
    }

    #[test]
    fn intent_ids_are_unique_per_send_and_stable() {
        // The idempotency key is (entity ‖ send_index): unique per send,
        // stable across resends of the same send.
        let a = intent_id(PersistId::new(7), 3);
        let b = intent_id(PersistId::new(7), 4);
        let c = intent_id(PersistId::new(8), 3);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, intent_id(PersistId::new(7), 3));
        assert_eq!(a >> 64, 7);
        assert_eq!(a as u64, 3);
    }

    #[test]
    fn duration_strings_parse() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("10x").is_err());
    }

    #[test]
    fn ack_log_is_append_only_and_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acks.jsonl");

        {
            let mut log = AckLog::open(&path).unwrap();
            log.record(&AckRecord::Diff {
                entity: PersistId::new(7),
                tick: Tick::new(42),
                lsn: orrery_protocol::Lsn::new(3, 4096),
            });
            log.record(&AckRecord::Intent {
                intent_id: "213458173728644058818963591144807231488".to_string(),
                tick: Tick::new(9),
            });
            log.record(&AckRecord::Diff {
                entity: PersistId::new(8),
                tick: Tick::new(43),
                lsn: orrery_protocol::Lsn::new(3, 8192),
            });
        }

        // Re-open (as the kill-9 harness would) and parse every line back.
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one JSON line per ack, append-only");

        let first: AckRecord = serde_json::from_str(lines[0]).unwrap();
        match first {
            AckRecord::Diff { entity, tick, lsn } => {
                assert_eq!(entity, PersistId::new(7));
                assert_eq!(tick, Tick::new(42));
                assert_eq!(lsn, orrery_protocol::Lsn::new(3, 4096));
            }
            other => panic!("expected a diff ack, got {other:?}"),
        }
        let second: AckRecord = serde_json::from_str(lines[1]).unwrap();
        match second {
            AckRecord::Intent { intent_id, tick } => {
                assert_eq!(
                    intent_id,
                    // 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00 in decimal —
                    // the harness compares the string, so this is lossless.
                    "213458173728644058818963591144807231488"
                );
                assert_eq!(tick, Tick::new(9));
            }
            other => panic!("expected an intent ack, got {other:?}"),
        }
        // The log is append-only: a second open for append must not truncate
        // the earlier records (the harness reads the whole file up to the
        // kill point). Open in *append* mode and add one more; all four
        // records must parse.
        {
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let mut log = AckLog {
                writer: BufWriter::new(file),
            };
            log.record(&AckRecord::Diff {
                entity: PersistId::new(9),
                tick: Tick::new(50),
                lsn: orrery_protocol::Lsn::new(4, 0),
            });
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4);
        for line in text.lines() {
            let _: AckRecord = serde_json::from_str(line).expect("every line parses");
        }
    }
}
