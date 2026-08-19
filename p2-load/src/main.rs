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
//!   iroh, on both lanes: unreliable datagrams for bulk diffs and reliable
//!   unidirectional streams for control — subscribes, intents, hello (roadmap
//!   decision C-1). It must ride the same lanes the shipped client does, or it
//!   would be measuring a path nobody runs. The aeronet session stack is a
//!   Bevy client convenience; the persistd gateway is raw iroh
//!   (`crates/orrery_persistd/src/gateway.rs`), so the rig dials it without
//!   linking Bevy — at the cost of owning the ~60 lines of stream framing that
//!   `aeronet_iroh::stream` gives the client for free.
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
//! - **Authority.** Every bulk diff the rig sends is *fenced*: the gateway
//!   sets `strict_authority: true` unconditionally
//!   (`crates/orrery_persistd/src/gateway.rs`, `route_session_diff`), so a
//!   diff without a granted `(lease_id, authority_seq)` is rejected before the
//!   journal. The rig therefore claims a strong lease per entity before it
//!   drives any load, renews them on a heartbeat, and refuses to fall back to
//!   unleased writes — the fallback is exactly how a run of 541 408 rejections
//!   passed for a durability measurement.
//! - **One NodeId per session.** The gateway's peer registry is keyed by
//!   `NodeId` and only the *newest* session of a peer is current
//!   (`PeerRegistry::activate` sets `peer.current`); every older session's
//!   `lock_current()` returns `None` and its diffs are nacked before routing.
//!   A rig fanning 125 connections out of one endpoint therefore had 124 dead
//!   sessions. Each session gets its own endpoint, which also gives it its own
//!   claim-rate bucket (that bucket is `NodeId`-scoped: 64 burst, 20/s).
//! - **Cells are pinned.** A leased writer cannot move an entity between
//!   cells: `apply_fenced` admits a diff only when
//!   `by_cell[entity] == record.cell` (`orrery_persistd/src/actor.rs`), and the
//!   gateway answers a client-sent `LeaseMsg::Rekey` with an unconditional
//!   `Deny{NotEligible}`. Cross-cell coverage therefore comes from the
//!   *placement* (≥ `--cells` distinct cells), not from motion. See
//!   docs/08-persistence.md §2.1.
//!
//! Telemetry posture: **the OTel bridge (D12) is deferred.** This crate
//! deliberately adds no `opentelemetry` dependency — that stack would be a
//! new D14 pinned dependency, which is an orchestrator decision. The JSONL
//! contract documented in `p2-load/README.md` is the delivered telemetry
//! mechanism; `tracing` logs on stderr are diagnostic only and are not the
//! D12 bridge.

mod cli;
// Recovery-reader wiring arrives with the promotion slice; meanwhile this
// module is compiled and unit-tested as the stable pure verifier contract.
#[allow(dead_code)]
mod evidence;
mod telemetry;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use clap::Parser;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use serde::{Deserialize, Serialize};

use orrery_persist_client::config::PersistClientConfig;
use orrery_persist_client::latency::LatencyHistogram;
use orrery_persist_client::{IntentQueue, UplinkScheduler};
use orrery_protocol::channels::{encode_datagram, encode_stream_frame, untag, Channel};
use orrery_protocol::{
    Attestation, CellEpoch, CellId, ClaimBasis, ClaimId, ClaimKind, DiffUplink, GatewayMsg,
    GatewayReply, GridId, Intent, IntentOp, IntentOutcome, LeaseId, LeaseMsg, NodeId, PersistId,
    RecordKind, SeqPair, Tick,
};

use cli::Cli;
use evidence::{
    compare_recovery, AckRecord, DiffEvidence, IntentOutcomeEvidence, RecoveredDiff,
    RecoveredEvidence,
};
use telemetry::{RunContext, TelemetrySink};

/// The gateway ALPN (matches `orrery_persistd::gateway::GATEWAY_ALPN` and the
/// client crate's `gateway::GATEWAY_ALPN`; re-declared here so the rig links
/// neither crate's gateway module).
const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The rig's flush cadence. 20 Hz matches D16's send-rate default (the game
/// loop the scheduler is designed around) and gives the per-session fan-out
/// math its denominator.
const FLUSH_HZ: u64 = 20;

/// Connection flushes are spread through one 20 Hz frame.  The P2 profile
/// needs 125 independent scheduler budgets; flushing all of them at one frame
/// boundary creates a 1,000-datagram burst (125 sessions × 8 diffs) even
/// though each session independently satisfies its 160 diff/s budget.  Keep
/// every individual scheduler at 20 Hz, but distribute their frames over
/// these slots to make the load generator measure append latency rather than
/// an artificial client-side burst.
const SESSION_FLUSH_PHASE_SLOTS: usize = 20;

/// After the send window closes, keep receiving for a short bounded interval
/// so replies already in flight are represented in the latency gate. Ten
/// times the D16 bulk target is enough to expose a backlog without turning a
/// failed gateway into an unbounded test hang.
const FINAL_REPLY_DRAIN: Duration = Duration::from_millis(50);

/// How long the final drain sleeps when nothing has arrived yet.
///
/// Short enough that a reply landing mid-drain is still credited well inside
/// [`FINAL_REPLY_DRAIN`], long enough that the drain is not a spin loop
/// competing with the reader tasks it is waiting on.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// The per-session flush byte budget, = the D16 client default
/// (`PersistClientConfig::flush_budget_bytes`, 1024). One session sustains
/// `budget / (payload + 64)` diffs per flush (uplink.rs:160-163); the startup
/// fan-out assert is checked against exactly this number.
const FLUSH_BUDGET_BYTES: usize = 1024;

/// How often each session's QUIC RTT estimate is sampled into
/// `client_quic_rtt_ms`. It is a smoothed gauge, not a per-operation
/// measurement, so sampling it faster than the transport updates it would
/// only reweight the same value.
const RTT_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// The uplink scheduler's per-diff overhead estimate (`size = payload + 64`,
/// uplink.rs flush). The fan-out math sizes sessions against this constant.
const DIFF_OVERHEAD_BYTES: usize = 64;

/// The intent-mix RNG modulus. Per-entity sends are counted; every send whose
/// `(entity_hash + send_index) mod 1_000_000` falls inside a mix bucket is
/// upgraded to an intent of that kind. `1_000_000` gives three decimal places
/// of fraction resolution, comfortably under the 0.01–0.1 mixes in D16-shaped
/// workloads.
const MIX_MODULUS: u64 = 1_000_000;

/// The registrar's per-`NodeId` claim burst
/// (`ClaimBucket::BURST_CLAIMS` in `crates/orrery_persistd/src/gateway.rs`,
/// and `orrery_authority::contact::CLAIM_BURST`). Re-declared rather than
/// imported: this crate is a separate workspace and links neither.
const CLAIM_BURST: usize = 64;

/// The registrar's per-`NodeId` claim refill rate
/// (`ClaimBucket::CLAIMS_PER_SECOND`, `CLAIM_RATE_PER_SEC`).
const CLAIM_RATE_PER_SEC: usize = 20;

/// How long one claim round waits for its replies before pacing the next one.
/// One second is the bucket's refill quantum, so a round that emitted a full
/// `CLAIM_RATE_PER_SEC` batch has exactly earned the next one when it ends.
const CLAIM_ROUND: Duration = Duration::from_secs(1);

/// Hard ceiling on the whole claim phase. The phase is `entities / sessions`
/// claims per NodeId, paced by the bucket above; this bounds a gateway that
/// simply stops answering instead of denying.
const CLAIM_PHASE_TIMEOUT: Duration = Duration::from_secs(120);

/// Lease renewal cadence. `LEASE_TTL_MS` is 10 s
/// (`crates/orrery_persistd/src/lease.rs`); renewing at 3 s survives two lost
/// heartbeats. Heartbeats are batched per session and are *not* charged to the
/// claim bucket.
const LEASE_HEARTBEAT: Duration = Duration::from_secs(3);

/// Override for [`LEASE_HEARTBEAT`], in milliseconds
/// (`P2_LOAD_LEASE_HEARTBEAT_MS`).
///
/// A study knob, not a deployment one. The renewal cadence is a *cause* of
/// intent latency on this rig, not just a background chore: every session's
/// whole entity set is renewed in one pass of the drive loop, so at 250
/// sessions x 40 entities the gateway receives ~10 000 lease renewals inside a
/// few milliseconds and turns each into a `LeaseStore::locate` -- an FDB read.
/// Being able to move the period is what turns "the intent tail is periodic at
/// exactly this cadence" from a coincidence into a causal claim: halve the
/// frequency and the fraction of intents caught in a burst must halve with it.
fn lease_heartbeat_period() -> Duration {
    std::env::var("P2_LOAD_LEASE_HEARTBEAT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(LEASE_HEARTBEAT, Duration::from_millis)
}

/// Whether to spread each session's renewal across the period instead of
/// renewing every session in one pass (`P2_LOAD_HEARTBEAT_PHASED=1`).
///
/// Bulk flushes are already phased per session (`session_flush_phase`); the
/// heartbeat is not, and that asymmetry is the whole of the burst. A real
/// deployment's clients are not phase-aligned to each other, so a synchronized
/// renewal is a property of this load generator rather than of the workload it
/// stands for -- which is exactly why it has to be switchable rather than
/// argued about.
fn heartbeat_phased() -> bool {
    std::env::var("P2_LOAD_HEARTBEAT_PHASED").is_ok_and(|v| v == "1")
}

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

    if cli.verify_recovery {
        return verify_recovery(&cli).await;
    }

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
    //
    // One endpoint — one NodeId — per session. The gateway's peer registry is
    // keyed by NodeId and `PeerRegistry::activate` makes only the newest
    // session of a peer current, so N connections from one endpoint leave
    // N−1 sessions whose `lock_current()` is `None`: every diff on them is
    // nacked at `route_session_diff` before authority is even consulted. The
    // claim-rate bucket is NodeId-scoped too, so distinct identities are also
    // what makes a 10 000-entity claim phase finish in about a second instead
    // of eight minutes.
    let mut endpoints = Vec::new();
    let mut signing_keys = Vec::new();
    let mut sessions = Vec::new();
    for i in 0..cli.sessions {
        let (endpoint, signing_key) =
            bind_endpoint(session_secret_key(cli.secret_key.as_deref(), i)?.as_deref()).await?;
        // One token per session: it is bound to this endpoint's NodeId and
        // carries its own issue time, so it is minted rather than shared.
        let token = session_token(&cli, endpoint.id())?;
        let conn = dial(&endpoint, cli.addr, cli.gateway, token)
            .await
            .with_context(|| format!("dial session {i}"))?;
        endpoints.push(endpoint);
        signing_keys.push(signing_key);
        sessions.push(conn);
    }
    tracing::info!(
        sessions = sessions.len(),
        node = %endpoints[0].id(),
        "gateway sessions connected (one NodeId each; first shown)"
    );

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
    let rig_endpoints = endpoints.clone();
    let mut rig = Rig {
        cli: &cli,
        emit_json: cli.json,
        endpoints,
        signing_keys,
        sessions,
        inventory,
        diff_hz,
        intent_mix,
        duration,
        ack_log,
        pending_diffs: HashMap::new(),
        leases: HashMap::new(),
        intent_sessions: HashMap::new(),
    };

    // ── Authority first. No lease, no load. ──────────────────────────────
    let claim = rig.claim_leases().await;
    if let Err(e) = &claim {
        if cli.json {
            telemetry::run_footer(&format!("claim phase failed: {e:#}"));
        }
    }
    let claim_elapsed = claim?;

    let outcome = rig.drive().await;

    if cli.json {
        match &outcome {
            Ok(stats) => telemetry::run_footer(&format!(
                "duration elapsed; claim_secs={:.2} diffs={} acks={} durable_acks={} nacks={} \
                 intents={} intent_acks={}",
                claim_elapsed.as_secs_f64(),
                stats.diffs_sent,
                stats.diff_acks,
                stats.durable_diff_acks,
                stats.diff_nacks,
                stats.intents_sent,
                stats.intent_acks
            )),
            Err(e) => telemetry::run_footer(&format!("run failed: {e:#}")),
        }
    }
    outcome?;

    // Close the endpoints cleanly so iroh does not log an ungraceful abort at
    // drop time.
    for endpoint in rig_endpoints {
        endpoint.close().await;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct RecoveryVerificationReport {
    pass: bool,
    recovery_cutoff: String,
    ack_records: usize,
    eligible_bulk_acks: usize,
    bulk_checked: usize,
    intents_checked: usize,
    rejected_intents_audited: usize,
    mismatches: Vec<String>,
}

async fn verify_recovery(cli: &Cli) -> Result<()> {
    let ack_path = cli
        .ack_log
        .as_ref()
        .context("--verify-recovery requires --ack-log")?;
    let cluster = cli
        .fdb_cluster_file
        .as_ref()
        .context("--verify-recovery requires --fdb-cluster-file")?;
    let cutoff_text = cli
        .recovery_cutoff
        .as_ref()
        .context("--verify-recovery requires --recovery-cutoff")?;
    let output = cli
        .output
        .as_ref()
        .context("--verify-recovery requires --output")?;
    let cutoff = parse_lsn(cutoff_text)?;
    let records = read_ack_records(ack_path)?;
    let eligible: Vec<AckRecord> = records
        .iter()
        .filter(|r| match r {
            AckRecord::Diff(d) => d.lsn <= cutoff,
            AckRecord::Intent { .. } => true,
        })
        .cloned()
        .collect();
    let expected: Vec<DiffEvidence> = eligible
        .iter()
        .filter_map(|r| match r {
            AckRecord::Diff(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    let diffs = read_gateway_state(cli, &expected).await?;
    let intents = read_intent_rows(cluster, &eligible).await?;
    let compared = compare_recovery(&eligible, &RecoveredEvidence { diffs, intents });
    let report = RecoveryVerificationReport {
        pass: compared.passes(),
        recovery_cutoff: cutoff.to_string(),
        ack_records: records.len(),
        eligible_bulk_acks: expected.len(),
        bulk_checked: compared.bulk_checked,
        intents_checked: compared.intents_checked,
        rejected_intents_audited: compared.rejected_intents_audited,
        mismatches: compared
            .mismatches
            .iter()
            .map(|m| format!("{m:?}"))
            .collect(),
    };
    let tmp = output.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&report)?)?;
    std::fs::rename(tmp, output)?;
    if !report.pass {
        bail!("recovery verification failed; see {}", output.display());
    }
    Ok(())
}

fn read_ack_records(path: &Path) -> Result<Vec<AckRecord>> {
    BufReader::new(std::fs::File::open(path)?)
        .lines()
        .enumerate()
        .filter_map(|(n, line)| match line {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => Some(
                serde_json::from_str(&s)
                    .with_context(|| format!("parse {}:{}", path.display(), n + 1)),
            ),
            Err(e) => Some(Err(e.into())),
        })
        .collect()
}

fn parse_lsn(raw: &str) -> Result<orrery_protocol::Lsn> {
    if let Some((s, o)) = raw.split_once(':') {
        return Ok(orrery_protocol::Lsn::new(
            s.trim().parse()?,
            o.trim().parse()?,
        ));
    }
    let values: Vec<u64> = raw
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    if values.len() == 2 {
        Ok(orrery_protocol::Lsn::new(values[0], values[1]))
    } else {
        bail!("invalid --recovery-cutoff {raw:?}; expected segment:offset")
    }
}

async fn read_gateway_state(cli: &Cli, expected: &[DiffEvidence]) -> Result<Vec<RecoveredDiff>> {
    let (endpoint, _) = bind_endpoint(None).await?;
    let token = session_token(cli, endpoint.id())?;
    let conn = dial(&endpoint, cli.addr, cli.gateway, token).await?;
    let wanted_ids: BTreeSet<_> = expected.iter().map(|diff| diff.entity).collect();

    // ROOT is only a covering discovery scan; its reply cell is not evidence
    // of the storage cell for every returned row. It is kept for the
    // diagnostic below, not for the proof.
    let root = read_snapshot_pages(&conn, &[CellId::ROOT]).await?;
    let discovered: BTreeSet<_> = recovered_snapshot_diffs(wanted_ids.clone(), root)?
        .into_iter()
        .map(|diff| diff.entity)
        .collect();

    let cells = recovery_leaf_cells(expected);
    let leaves = read_snapshot_pages(&conn, &cells).await?;
    endpoint.close().await;
    let recovered = recovered_snapshot_diffs(wanted_ids, leaves)?;

    // Both "the promoted node does not hold it at all" and "it holds it
    // somewhere other than the leaf it was acknowledged at" reach the
    // comparator as `MissingBulk`, and the two point at different subsystems —
    // durability versus placement. The covering scan above separates them, so
    // the next reader of a failed gate does not have to guess.
    let at_leaf: BTreeSet<_> = recovered.iter().map(|diff| diff.entity).collect();
    let misplaced: Vec<_> = discovered.difference(&at_leaf).copied().collect();
    if !misplaced.is_empty() {
        tracing::warn!(
            count = misplaced.len(),
            first = ?misplaced.first(),
            "recovery: entities the covering ROOT scan returned are absent from the leaf they were acknowledged at"
        );
    }
    Ok(recovered)
}

/// The physical leaves to re-read, taken from the acknowledgements themselves.
///
/// `DiffEvidence::cell` is the cell the gateway acknowledged the write at, and
/// a leased writer cannot move an entity between cells (the gateway denies a
/// client `LeaseMsg::Rekey`, and `apply_fenced` pins `record.cell` to the
/// committed one) — so the acknowledged cell *is* the claim under proof, and
/// finding the entity in that leaf's page is a physical identity assertion
/// rather than the enclosing ROOT page.
///
/// This deliberately consults neither `--manifest` nor `--entities`/`--cells`.
/// It used to reload the rig's inventory and, with no manifest, fall back to
/// `synthetic_inventory` — and the kill-9 gate never passed `--manifest` to
/// its verify step. The fallback synthesised a 128-cell lattice at
/// `INTEREST_LEVEL`, and `read_snapshot` matches a requested cell against
/// stored cells by prefix, so a level-21 request matches only itself: every
/// leaf read landed on a cell nothing was stored in. Measured 2026-08-17: 99
/// of 100 seeded entities reported `MissingBulk` while the promoted node
/// demonstrably held all 100, the hundredth surviving on a single coincidental
/// collision between the two lattices. Deriving the cells from the ack log
/// removes the second source of the same fact, and with it the drift.
fn recovery_leaf_cells(expected: &[DiffEvidence]) -> Vec<CellId> {
    expected
        .iter()
        .map(|diff| diff.cell)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Read complete area pages for the requested physical cells.
async fn read_snapshot_pages(
    conn: &GatewayLink,
    cells: &[CellId],
) -> Result<BTreeMap<CellId, BTreeMap<u32, (Vec<PersistId>, Vec<Bytes>)>>> {
    // Subscribe travels on the reliable lane, which frames whole messages and
    // no longer bounds a request to one MTU — so this batch size is now about
    // the *reply*, not the request: one subscribe of hundreds of leaves is one
    // burst of hundreds of pages the recovery reader must hold at once. 64
    // keeps that working set bounded, and is independent of page payload
    // chunking, which the gateway handles separately.
    const CELLS_PER_SUBSCRIBE: usize = 64;
    let mut pages = BTreeMap::new();
    for batch in cells.chunks(CELLS_PER_SUBSCRIBE) {
        pages.extend(read_snapshot_batch(conn, batch).await?);
    }
    Ok(pages)
}

/// Read one datagram-safe batch of area pages.
async fn read_snapshot_batch(
    conn: &GatewayLink,
    cells: &[CellId],
) -> Result<BTreeMap<CellId, BTreeMap<u32, (Vec<PersistId>, Vec<Bytes>)>>> {
    let wanted: BTreeSet<_> = cells.iter().copied().collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    send_msg(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: cells.to_vec(),
        },
    )
    .await;
    let mut chunks = BTreeMap::new();
    let mut totals = BTreeMap::new();
    while wanted.iter().any(|cell| {
        totals
            .get(cell)
            .is_none_or(|total| chunks.get(cell).map_or(0, BTreeMap::len) < *total as usize)
    }) {
        match recv_reply(conn, Duration::from_secs(10)).await? {
            GatewayReply::AreaPage { cell, page } if wanted.contains(&cell) => {
                totals.insert(cell, page.total_chunks);
                chunks
                    .entry(cell)
                    .or_insert_with(BTreeMap::new)
                    .insert(page.chunk_index, (page.entities, page.payloads));
            }
            GatewayReply::AreaLoadError { cell, .. } if wanted.contains(&cell) => {
                bail!("gateway recovery read failed for {cell}")
            }
            _ => {}
        }
    }
    Ok(chunks)
}

type RecoveredPageMap = BTreeMap<CellId, BTreeMap<u32, (Vec<PersistId>, Vec<Bytes>)>>;

/// Decode and filter physical-cell pages, retaining each response cell.
///
/// Callers must use leaf pages for a storage-cell identity proof; ROOT pages
/// are suitable only for discovery.
fn recovered_snapshot_diffs(
    wanted_ids: BTreeSet<PersistId>,
    pages: RecoveredPageMap,
) -> Result<Vec<RecoveredDiff>> {
    let mut actual = BTreeMap::new();
    for (cell, chunks) in pages {
        for (_, (ids, payloads)) in chunks {
            for (entity, payload) in ids.into_iter().zip(payloads) {
                if !wanted_ids.contains(&entity) {
                    continue;
                }
                if payload.len() < 16 {
                    bail!("recovered payload for {entity:?} lacks synthetic tick");
                }
                let tick = Tick::new(u64::from_le_bytes(
                    payload[8..16].try_into().expect("length checked"),
                ));
                actual.insert(
                    entity,
                    RecoveredDiff {
                        grid: GridId::ROOT,
                        cell,
                        entity,
                        tick,
                        payload_digest: blake3::hash(&payload).to_hex().to_string(),
                    },
                );
            }
        }
    }
    Ok(actual.into_values().collect())
}

async fn read_intent_rows(
    cluster: &Path,
    records: &[AckRecord],
) -> Result<BTreeMap<String, IntentOutcomeEvidence>> {
    let ctx = orrery_persistd::FdbContext::connect(&cluster.display().to_string())
        .map_err(|e| anyhow::anyhow!("open FDB: {e}"))?;
    let db = ctx.database();
    let mut result = BTreeMap::new();
    for id_text in records.iter().filter_map(|r| match r {
        AckRecord::Intent { intent_id, .. } => Some(intent_id),
        _ => None,
    }) {
        let key = orrery_persistd::keyspace::intent_key(id_text.parse()?);
        let raw = db
            .run(|trx, _| async move { Ok(trx.get(&key, false).await?) })
            .await
            .map_err(|e: foundationdb::FdbBindingError| {
                anyhow::anyhow!("read intent {id_text}: {e}")
            })?;
        let Some(raw) = raw else {
            continue;
        };
        let row: orrery_persistd::keyspace::IntentRow = postcard::from_bytes(&raw)?;
        result.insert(
            id_text.clone(),
            match row.outcome {
                IntentOutcome::Committed { tick, minted } => {
                    IntentOutcomeEvidence::Committed { tick, minted }
                }
                IntentOutcome::Rejected { reason } => IntentOutcomeEvidence::Rejected { reason },
            },
        );
    }
    Ok(result)
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

/// The connection-local scheduler which owns an inventory entry. Keeping this
/// mapping pure makes the driver and its capacity proof use the same sharding
/// rule.
fn scheduler_shard(index: usize, sessions: usize) -> usize {
    index % sessions
}

/// Number of flush windows in which to spread the initial registrations.
///
/// `UplinkScheduler` deliberately starts every registration with zero credit.
/// If the rig queues all of them on its first flush, equal rates make every
/// entity become ready in the same later flush.  The resulting 10k burst
/// exhausts every connection's byte budget despite the aggregate fan-out
/// proof.  Starting one evenly-sized cohort per window lets the scheduler
/// retain its normal rate accumulator while assigning a stable phase to each
/// entity.
fn registration_phase_slots(diff_hz: f64) -> usize {
    ((FLUSH_HZ as f64 / diff_hz).ceil() as usize).max(1)
}

/// The deterministic initial-registration phase for one inventory entry.
///
/// Entries are sharded round-robin, so their per-session ordinal is
/// `index / sessions`.  Phasing that ordinal (rather than the global index)
/// keeps each connection balanced as well as the global workload.
fn registration_phase(index: usize, sessions: usize, phase_slots: usize) -> u64 {
    debug_assert!(sessions > 0);
    debug_assert!(phase_slots > 0);
    ((index / sessions) % phase_slots) as u64
}

/// Whether this flush owns an entity's next open-loop generation cohort.
///
/// Generation is deliberately independent of acknowledgement timing. At the
/// default 10k @ 2 Hz profile, ten phase slots select exactly 1,000 entities
/// per 50 ms frame. Queueing retains the scheduler's newest-wins behavior if
/// an older tick is still pending.
fn generation_cohort_is_due(
    index: usize,
    sessions: usize,
    phase_slots: usize,
    flush_index: u64,
) -> bool {
    let phase = registration_phase(index, sessions, phase_slots);
    flush_index >= phase && (flush_index - phase).is_multiple_of(phase_slots as u64)
}

/// The intra-frame flush phase for a connection-local scheduler.
///
/// Round-robin assignment keeps the number of sessions (and therefore the
/// number of independently-budgeted datagrams) within one in every phase.
fn session_flush_phase(session: usize) -> usize {
    session % SESSION_FLUSH_PHASE_SLOTS
}

/// Build one D16 scheduler per live connection, registering each entity with
/// exactly one owner. The owner is also the connection used for its datagrams,
/// so acknowledgements return to the scheduler that recorded the send time.
fn scheduler_shards(
    inventory: &[Placement],
    sessions: usize,
    diff_hz: f64,
) -> Vec<UplinkScheduler> {
    let mut schedulers = (0..sessions)
        .map(|_| UplinkScheduler::new())
        .collect::<Vec<_>>();
    for (index, p) in inventory.iter().enumerate() {
        schedulers[scheduler_shard(index, sessions)].register(p.entity, diff_hz as f32);
    }
    schedulers
}

/// Combine the connection-local scheduler histograms into the single client
/// population reported to the P2 dashboard. `LatencyHistogram::merge` keeps
/// the same bounded-memory bucket semantics as a single scheduler.
fn aggregate_bulk_latency(schedulers: &[UplinkScheduler]) -> LatencyHistogram {
    aggregate(schedulers, UplinkScheduler::ack_latency)
}

/// Combine one per-scheduler histogram across every connection shard.
fn aggregate(
    schedulers: &[UplinkScheduler],
    pick: fn(&UplinkScheduler) -> &LatencyHistogram,
) -> LatencyHistogram {
    let mut combined = LatencyHistogram::new();
    for scheduler in schedulers {
        combined.merge(pick(scheduler));
    }
    combined
}

/// Drain every client-side bulk series — the gated round trip and the four
/// stages that decompose it — into the JSONL stream in one place.
///
/// One function because they must be drained together: a report that has
/// `bulk_ack_ms` from the end of the run and its attribution from the middle
/// of it decomposes two different populations.
fn drain_bulk_series(sink: &telemetry::TelemetrySink, schedulers: &[UplinkScheduler]) {
    sink.drain_histogram(
        telemetry::SERIES_BULK_ACK,
        &aggregate_bulk_latency(schedulers),
    );
    sink.drain_histogram(
        telemetry::SERIES_CLIENT_BULK_QUEUE,
        &aggregate(schedulers, UplinkScheduler::queue_latency),
    );
    sink.drain_histogram(
        telemetry::SERIES_CLIENT_BULK_SEND,
        &aggregate(schedulers, UplinkScheduler::send_latency),
    );
    sink.drain_histogram(
        telemetry::SERIES_CLIENT_BULK_WIRE,
        &aggregate(schedulers, UplinkScheduler::wire_latency),
    );
    sink.drain_histogram(
        telemetry::SERIES_CLIENT_BULK_DISPATCH,
        &aggregate(schedulers, UplinkScheduler::dispatch_latency),
    );
}

/// Combine every link's send-buffer occupancy into one population.
///
/// Drained beside the bulk series rather than with the RTT gauge: it is the
/// far half of the same `client_bulk_send_ms` boundary those series bracket,
/// and reading it against a different slice of the run would compare a queue
/// depth to a latency that is not the one it caused.
fn aggregate_send_buffer(links: &[GatewayLink]) -> LatencyHistogram {
    let mut combined = LatencyHistogram::new();
    for link in links {
        combined.merge(&link.send_buffer_histogram());
    }
    combined
}

/// Every *distinct* durable ack must have produced a `bulk_ack_ms` sample.
///
/// Two exclusions, both deliberate:
///
/// - **Nacks.** `UplinkScheduler::on_nack_at` no longer records a
///   reply-latency sample: a refusal round trip is not a write's
///   acknowledgement latency, and folding it into the gated series let a run
///   with zero successful writes report a p99 of 1.25–1.75 ms.
/// - **Duplicate acks.** The bulk lane is unreliable and unordered and an
///   unacked diff is resent, so the gateway can answer the same
///   `(entity, tick)` twice. The scheduler samples the first reply and
///   discards the send instant with it, so a duplicate can never be a sample
///   and must not be demanded of the histogram.
fn check_bulk_reply_coverage(sampled: u64, stats: &RunStats) -> Result<()> {
    let expected = stats.first_durable_diff_acks;
    if sampled != expected {
        bail!(
            "bulk latency coverage incomplete after {} ms drain: sampled {} of {} durable acks",
            FINAL_REPLY_DRAIN.as_millis(),
            sampled,
            expected
        );
    }
    Ok(())
}

/// Bind the rig-local iroh endpoint (relay disabled — the rig is
/// gateway-colocated by design, docs/11-roadmap.md §P2 "gateway-colocated
/// load generator"; a relayed path would inflate the client-observed series).
/// Derive session `index`'s iroh secret from the rig's `--secret-key`.
///
/// Each session needs its own NodeId (see `run`), so a pinned rig identity has
/// to fan out into a pinned *family* of identities rather than one. Derivation
/// is `blake3(secret_key_hex || index)`, so `--secret-key` still reproduces the
/// same NodeIds across runs. Without `--secret-key` each session generates its
/// own ephemeral key.
fn session_secret_key(secret_key: Option<&str>, index: u32) -> Result<Option<String>> {
    let Some(root) = secret_key else {
        return Ok(None);
    };
    // Reject a malformed root here rather than at the Nth derivation.
    root.parse::<SecretKey>()
        .context("invalid --secret-key (expected hex)")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(root.as_bytes());
    hasher.update(&index.to_le_bytes());
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

async fn bind_endpoint(secret_key: Option<&str>) -> Result<(Endpoint, SecretKey)> {
    // The endpoint identity is also the intent issuer. Keep its private key
    // alongside the endpoint so load-generated intents are signed by the
    // NodeId that the gateway binds to the connection.
    let signing_key = match secret_key {
        Some(encoded) => encoded
            .parse()
            .context("invalid --secret-key (expected hex)")?,
        None => SecretKey::generate(),
    };
    let builder = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .secret_key(signing_key.clone());
    let endpoint = builder.bind().await.context("bind rig endpoint")?;
    Ok((endpoint, signing_key))
}

/// One gateway session's two lanes.
///
/// The rig speaks the gateway wire surface directly over raw iroh, so it owns
/// the plumbing the Bevy client gets from `aeronet_iroh`: a datagram lane for
/// bulk diffs, and a reliable unidirectional stream for control — subscribes,
/// intents, hello — framed `[u32 LE length][payload]` exactly as the gateway's
/// reader expects. Inbound traffic from both lanes lands in one queue, stamped
/// on arrival, because the D16 series are measured from the moment a reply
/// reaches this process and not from when the loop got around to it.
struct GatewayLink {
    conn: Connection,
    /// Datagrams the reader task below has taken off the connection.
    ///
    /// The transport's own `frame_rx.datagram` counts what the endpoint driver
    /// has decoded. The difference between the two is how many replies are
    /// sitting in the connection's inbound queue *after* the driver and
    /// *before* this process looks at them — the one gap in the round trip
    /// that neither `client_bulk_wire_ms` (which ends when the reader task
    /// stamps) nor the QUIC RTT (computed in the driver) can see.
    datagrams_read: Arc<std::sync::atomic::AtomicU64>,
    /// The outbound control stream, opened on the first control message.
    control: tokio::sync::Mutex<Option<iroh::endpoint::SendStream>>,
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(Bytes, Instant)>>,
    /// Bytes already queued in the endpoint driver's outbound datagram buffer
    /// at the moment a diff was handed to it (`client_send_buffer_bytes`).
    ///
    /// `send_datagram` returns once the driver has *buffered* the payload, so
    /// `client_bulk_send_ms` ends before the packet exists on the wire, and
    /// the QUIC RTT — computed from ACKs on packets that already went out —
    /// cannot see the wait either. Anything queued here lands in
    /// `client_bulk_wire_ms` and in no other series, which is precisely the
    /// signature of an unattributed round-trip gap. Measuring it is what
    /// turns "the endpoint driver might be queueing" into a number.
    send_buffer: std::sync::Mutex<LatencyHistogram>,
    /// The buffer's configured size, read once while it is still empty:
    /// `datagram_send_buffer_space` reports the space *left* and quinn does
    /// not expose the capacity. Sampling this after a send would build the
    /// baseline from an already-occupied buffer and understate every
    /// occupancy that follows.
    send_buffer_capacity: u64,
}

impl GatewayLink {
    /// Start draining both lanes of an admitted connection.
    ///
    /// Must run *after* the admission uni-stream has been read: the stream
    /// reader accepts every inbound stream from here on.
    fn attach(conn: Connection) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let datagrams = conn.clone();
        let datagram_tx = tx.clone();
        let datagrams_read = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let read_counter = Arc::clone(&datagrams_read);
        tokio::spawn(async move {
            while let Ok(pkt) = datagrams.read_datagram().await {
                read_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if datagram_tx.send((pkt, Instant::now())).is_err() {
                    return;
                }
            }
        });
        let streams = conn.clone();
        tokio::spawn(async move {
            while let Ok(mut recv) = streams.accept_uni().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        let mut prefix = [0u8; 4];
                        if recv.read_exact(&mut prefix).await.is_err() {
                            return;
                        }
                        let len = u32::from_le_bytes(prefix) as usize;
                        if len > orrery_protocol::channels::MAX_RELIABLE_MESSAGE_BYTES {
                            return;
                        }
                        let mut payload = vec![0u8; len];
                        if recv.read_exact(&mut payload).await.is_err() {
                            return;
                        }
                        if tx.send((Bytes::from(payload), Instant::now())).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let send_buffer_capacity =
            u64::try_from(conn.datagram_send_buffer_space()).unwrap_or(u64::MAX);
        Self {
            conn,
            datagrams_read,
            control: tokio::sync::Mutex::new(None),
            inbound: tokio::sync::Mutex::new(rx),
            send_buffer: std::sync::Mutex::new(LatencyHistogram::new()),
            send_buffer_capacity,
        }
    }

    /// Replies the endpoint driver has decoded but this process has not yet
    /// taken off the connection.
    ///
    /// Zero means the reader task is keeping up and the whole of
    /// `client_bulk_wire_ms` is on the far side of this socket. A depth that
    /// grows with the run is this process falling behind, and the delay it
    /// implies is `depth / (replies per second on this session)`.
    fn rx_backlog(&self) -> u64 {
        let decoded = self.conn.stats().frame_rx.datagram;
        let read = self
            .datagrams_read
            .load(std::sync::atomic::Ordering::Relaxed);
        decoded.saturating_sub(read)
    }

    /// Write a control payload on the reliable lane.
    ///
    /// A message the gateway's reader would refuse is dropped here rather than
    /// written: writing it would tear the stream and take every message queued
    /// behind it. The cap is checked before the frame is built, so an oversize
    /// message does not get an allocation on its way to being refused.
    async fn send_control(&self, msg: &GatewayMsg) {
        let payload = encode_stream_frame(msg);
        if payload.len() > orrery_protocol::channels::MAX_RELIABLE_MESSAGE_BYTES {
            tracing::warn!(len = payload.len(), "control message too large to send");
            return;
        }
        let mut framed = Vec::with_capacity(payload.len() + 4);
        #[allow(clippy::cast_possible_truncation)] // Bounded by the cap just checked.
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        let mut control = self.control.lock().await;
        if control.is_none() {
            match self.conn.open_uni().await {
                Ok(stream) => *control = Some(stream),
                Err(e) => {
                    tracing::warn!(error = %e, "opening the control stream failed");
                    return;
                }
            }
        }
        let Some(stream) = control.as_mut() else {
            return;
        };
        if let Err(e) = stream.write_chunk(Bytes::from(framed)).await {
            tracing::warn!(error = %e, "control stream write failed");
        }
    }

    /// Send a bulk-state message on the datagram lane.
    fn send_state(&self, msg: &GatewayMsg) {
        let outcome = self.conn.send_datagram(Bytes::from(encode_datagram(msg)));
        // Sampled immediately after the hand-off, so it includes this
        // datagram: the question is how much the driver is holding when a
        // diff joins the queue, not how much it holds between sends.
        let occupancy = self
            .send_buffer_capacity
            .saturating_sub(u64::try_from(self.conn.datagram_send_buffer_space()).unwrap_or(0));
        if let Ok(mut hist) = self.send_buffer.lock() {
            hist.record_units(occupancy);
        }
        if let Err(e) = outcome {
            tracing::warn!(error = %e, "gateway datagram send failed");
        }
    }

    /// A copy of this link's send-buffer occupancy histogram, for the drain.
    fn send_buffer_histogram(&self) -> LatencyHistogram {
        self.send_buffer
            .lock()
            .map(|hist| hist.clone())
            .unwrap_or_default()
    }

    /// QUIC's current smoothed round-trip estimate for this connection.
    ///
    /// Computed inside the endpoint driver from ACK timing, so unlike
    /// `client_bulk_wire_ms` it does not carry the application's own read
    /// loop. Reading the two side by side is what separates a slow path from
    /// a queue at one end of it.
    /// `None` while no path is open (the connection is establishing or gone).
    /// The selected path is the one application data is actually riding.
    fn quic_rtt(&self) -> Option<Duration> {
        let paths = self.conn.paths();
        let selected = paths.iter().find(iroh::endpoint::Path::is_selected);
        selected.or_else(|| paths.iter().next()).map(|p| p.rtt())
    }

    /// The next inbound payload from either lane, with its arrival instant.
    async fn next_inbound(&self, timeout: Duration) -> Option<(Bytes, Instant)> {
        let mut inbound = self.inbound.lock().await;
        tokio::time::timeout(timeout, inbound.recv()).await.ok()?
    }

    /// An inbound payload if one is already queued, without awaiting.
    ///
    /// The run loop is a fixed-cadence pump, not a select over sockets, so it
    /// drains what has arrived and moves on. The arrival instant travels with
    /// the payload because it was taken in the reader task — a busy frame
    /// must not show up in the D16 series as gateway latency.
    fn try_next_inbound(&self) -> Option<(Bytes, Instant)> {
        self.inbound.try_lock().ok()?.try_recv().ok()
    }
}

/// Dial the gateway and complete the admission + hello handshake.
///
/// Mirrors the persistd gateway's session shape (`handle_connection` in
/// crates/orrery_persistd/src/gateway.rs): the server streams one admission
/// uni (`[ACCEPTED]`) on connect, then speaks tagged datagrams and reliable
/// control streams. The admission stream must be read before the lane readers
/// start, or they consume it.
async fn dial(
    endpoint: &Endpoint,
    addr: SocketAddr,
    gateway: NodeId,
    token: Vec<u8>,
) -> Result<GatewayLink> {
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
    let link = GatewayLink::attach(conn);

    // Hello, then require the ack to name the gateway we dialed.
    send_msg(
        &link,
        &GatewayMsg::Hello {
            token,
            node: endpoint.id(),
        },
    )
    .await;
    let reply = recv_reply(&link, Duration::from_secs(10))
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
    Ok(link)
}

/// One payload that arrived on one session, from either lane.
///
/// There is no longer a `Closed` variant: with a reader task per lane, a torn
/// connection is not an event that races the payloads ahead of it in the queue
/// — it is a state, read off the connection itself, and reading it that way
/// avoids reporting a close while replies from that session are still queued.
#[derive(Debug)]
struct InboundEvent {
    session: usize,
    packet: Bytes,
    received_at: Instant,
}

/// Send one `GatewayMsg` on whichever lane the channel policy assigns it.
///
/// Bulk diffs are tagged datagrams; Hello/Subscribe/SubmitIntent are control
/// payloads on the reliable stream lane (C-1). The rig must send on the same
/// lanes the shipped client does — measuring the datagram path while the game
/// uses streams would measure something nobody runs.
async fn send_msg(link: &GatewayLink, msg: &GatewayMsg) {
    match msg {
        GatewayMsg::Diff { .. } => link.send_state(msg),
        _ => link.send_control(msg).await,
    }
}

/// Await the next decodable `GatewayReply` from either lane within `timeout`.
async fn recv_reply(link: &GatewayLink, timeout: Duration) -> Result<GatewayReply> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timeout waiting for gateway reply");
        }
        let Some((pkt, _)) = link.next_inbound(remaining).await else {
            bail!("timeout waiting for gateway reply");
        };
        if let Some(reply) = decode_reply(&pkt) {
            return Ok(reply);
        }
        tracing::debug!("gateway: undecodable reply");
    }
}

/// Decode a reply on its channel tag, not on the lane that carried it.
fn decode_reply(pkt: &[u8]) -> Option<GatewayReply> {
    let (channel, rest) = untag(pkt)?;
    match channel {
        Channel::State => postcard::from_bytes(rest).ok(),
        Channel::Control => orrery_protocol::channels::decode_stream_frame(pkt),
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
/// 6×6×4-ish lattice whose distinct-cell count is ≥ the request. Placement is
/// the *whole* cross-cell coverage: a leased writer cannot move an entity
/// between cells, because the gateway denies a client `LeaseMsg::Rekey` and
/// `apply_fenced` admits a diff only at the entity's committed cell.
///
/// The synthetic placement is only usable against a world seeded to match it —
/// a claim names a committed cell, and an entity that was never written has
/// none. Prefer `--manifest` from `orrery-seed`.
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
        // `orrery-seed verify --emit-manifest` writes the JSONL docs/12 §9.3
        // describes: one entry per line, with a `content_version` trailer as
        // the last line. The pretty-printed JSON *array* branch below is a
        // compatibility path for manifests emitted before that was fixed —
        // the seeder no longer produces one.
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if text.trim_start().starts_with('[') {
            let entries: Vec<ManifestEntry> = serde_json::from_str(&text)
                .with_context(|| format!("parse {} as a JSON manifest array", path.display()))?;
            return Ok(Self {
                inventory: Self::placements(entries),
            });
        }
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

    /// Root-grid placements from decoded manifest entries. Non-root-grid rows
    /// are skipped with a warning (P2 loads root-grid inventories only, P-7).
    fn placements(entries: Vec<ManifestEntry>) -> Inventory {
        entries
            .into_iter()
            .filter_map(|entry| {
                if entry.grid.is_some_and(|g| g != GridId::ROOT) {
                    tracing::warn!(
                        entity = ?entry.persist_id,
                        "manifest entry in a non-root grid skipped (P2 loads root-grid \
                         inventories only)"
                    );
                    return None;
                }
                Some(Placement {
                    entity: entry.persist_id,
                    cell: entry.cell,
                })
            })
            .collect()
    }
}

/// The append-only ack log: one JSON line per ack, so a kill-9 harness can
/// enumerate the pre-kill acked set and diff it against the post-restart
/// manifest (docs/12-world-seeding.md §12.3).
struct AckLog {
    writer: BufWriter<std::fs::File>,
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
                // The load loop is also the gateway receive loop. Flushing
                // every acknowledgement makes the evidence log a synchronous
                // disk operation per packet, manufacturing tail latency at
                // the 20k/s P2 rate. `BufWriter` preserves order and flushes
                // when this cleanly-completing load process is dropped before
                // the separate recovery verifier reads the file.
                if let Err(e) = writeln!(self.writer, "{line}") {
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
    /// All bulk replies, including provisional acknowledgements that do not
    /// qualify as durable recovery evidence.
    diff_acks: u64,
    durable_diff_acks: u64,
    /// Durable acks that were the first for their `(entity, tick)` — one per
    /// distinct send, which is exactly what the scheduler samples.
    first_durable_diff_acks: u64,
    /// Durable acks for an `(entity, tick)` already credited. The bulk lane is
    /// unreliable and unordered and unacked diffs are resent, so these exist;
    /// they are not evidence and not latency samples.
    duplicate_durable_diff_acks: u64,
    provisional_diff_acks: u64,
    diff_nacks: u64,
    intents_sent: u64,
    intent_acks: u64,
    /// Leases the registrar took back mid-run (a `HeartbeatAck` `invalid` row
    /// or an unsolicited `Expire`). Non-zero fails the run: a rig that keeps
    /// writing after losing authority is measuring rejections again.
    leases_lost: u64,
}

/// One entity's granted authority: the fencing token and the exact sequence
/// pair `apply_fenced` compares against (`row.seq == authority_seq`).
#[derive(Debug, Clone, Copy)]
struct GrantedLease {
    lease_id: LeaseId,
    seq: SeqPair,
    /// The committed cell the lease was granted at. Every diff for this entity
    /// must name it: `apply_fenced` requires `by_cell[entity] == record.cell`,
    /// and a client cannot rekey (the gateway denies `LeaseMsg::Rekey`).
    cell: CellId,
}

/// The rig, mid-run.
struct Rig<'a> {
    cli: &'a Cli,
    /// Whether JSONL telemetry goes to stdout (`cli.json`). A field rather
    /// than a read of `cli.json` so a future integration harness can run the
    /// drive loop silently without rebuilding the CLI.
    emit_json: bool,
    /// One endpoint per session, index-aligned with `sessions`.
    endpoints: Vec<Endpoint>,
    /// Private halves of `endpoints[i].id()`, used for canonical issuer
    /// signatures. An intent must be signed by the identity that authenticated
    /// the connection carrying it, so this is index-aligned too.
    signing_keys: Vec<SecretKey>,
    sessions: Vec<GatewayLink>,
    inventory: Inventory,
    diff_hz: f64,
    intent_mix: IntentMix,
    duration: Duration,
    ack_log: Option<AckLog>,
    /// Sent bulk diffs awaiting an acknowledgement. An ack has no payload or
    /// cell fields, so retaining the exact outbound evidence is required for
    /// the post-crash state proof.
    pending_diffs: HashMap<(PersistId, Tick), DiffEvidence>,
    /// Granted authority, per entity. An entity absent here is never written:
    /// there is no unleased path out of this rig.
    leases: HashMap<PersistId, GrantedLease>,
    /// Which session (and therefore which issuer identity) minted each
    /// in-flight intent, so it is submitted on the connection whose
    /// authenticated NodeId the gateway binds it to.
    intent_sessions: HashMap<u128, usize>,
}

impl Rig<'_> {
    /// Claim a strong lease on every inventory entity before any load runs.
    ///
    /// Returns the phase's wall time. The rig refuses to proceed with a
    /// partial grant set: a diff without a lease is rejected at the gateway
    /// before it reaches the journal (`strict_authority: true`), so a silent
    /// fallback would once again measure refusals and call them durability.
    ///
    /// Shape of the phase:
    ///
    /// - An entity is claimed on the session that will send its diffs
    ///   (`scheduler_shard`), because the lease is bound to *that* session's
    ///   NodeId and `apply_fenced` compares the record's author against the
    ///   lease holder.
    /// - `ClaimKind::Strong` with `ClaimBasis::Explicit`. A weak claim would
    ///   additionally require coordinator-signed interest
    ///   (`interest_authority.allows`), which the P2 gate has no coordinator
    ///   to mint; a strong claim skips that check and, once held, cannot be
    ///   stolen out from under the run.
    /// - The claimed cell is the inventory cell, which must equal the entity's
    ///   *committed* cell or the registrar denies the claim as `NotEligible`
    ///   (`gateway.rs`: `plausible` requires
    ///   `committed_entity_cell(grid, entity) == cell`). Entities must already
    ///   exist durably — seed them (`orrery-seed`, or `persistd --dev-seed`)
    ///   before running the rig.
    /// - Pacing is the registrar's own bucket: `CLAIM_BURST` in the first
    ///   round and `CLAIM_RATE_PER_SEC` per second thereafter, *per session*.
    async fn claim_leases(&mut self) -> Result<Duration> {
        let started = Instant::now();
        let sessions = self.sessions.len().max(1);

        // Outstanding claims per session, in inventory order.
        let mut outstanding: Vec<Vec<Placement>> = vec![Vec::new(); sessions];
        for (index, placement) in self.inventory.iter().enumerate() {
            outstanding[scheduler_shard(index, sessions)].push(*placement);
        }
        let total = self.inventory.len();
        let per_session_max = outstanding.iter().map(Vec::len).max().unwrap_or(0);
        tracing::info!(
            entities = total,
            sessions,
            per_session = per_session_max,
            claim_burst = CLAIM_BURST,
            claim_rate_per_sec = CLAIM_RATE_PER_SEC,
            "claiming leases (strong/explicit) before load"
        );

        // Correlation ids are per-session and start at 1 (`ClaimId::0` is
        // reserved for registrar-initiated grants).
        let mut next_claim_id: Vec<u64> = vec![1; sessions];
        let mut in_flight: Vec<HashMap<ClaimId, Placement>> = vec![HashMap::new(); sessions];
        let mut denials: BTreeMap<String, u64> = BTreeMap::new();
        let deadline = tokio::time::Instant::now() + CLAIM_PHASE_TIMEOUT;
        let mut round = 0usize;

        while self.leases.len() < total {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "claim phase timed out after {:?}: granted {} of {} leases ({} still in \
                     flight). Denials so far: {:?}",
                    CLAIM_PHASE_TIMEOUT,
                    self.leases.len(),
                    total,
                    in_flight.iter().map(HashMap::len).sum::<usize>(),
                    denials
                );
            }
            let budget = if round == 0 {
                CLAIM_BURST
            } else {
                CLAIM_RATE_PER_SEC
            };
            round += 1;

            let mut emitted = 0usize;
            for session in 0..sessions {
                for _ in 0..budget {
                    let Some(placement) = outstanding[session].pop() else {
                        break;
                    };
                    let claim_id = ClaimId(next_claim_id[session]);
                    next_claim_id[session] += 1;
                    in_flight[session].insert(claim_id, placement);
                    send_msg(
                        &self.sessions[session],
                        &GatewayMsg::Lease {
                            message: LeaseMsg::Claim {
                                claim_id,
                                entity: placement.entity,
                                grid: GridId::ROOT,
                                cell: placement.cell,
                                kind: ClaimKind::Strong,
                                basis: ClaimBasis::Explicit,
                                observed: SeqPair::default(),
                                tick: Tick::new(0),
                            },
                        },
                    )
                    .await;
                    emitted += 1;
                }
            }

            // Collect for one refill quantum, whether or not this round had
            // anything left to send: replies to earlier rounds are still
            // arriving, and a `RateLimited` denial is requeued below.
            let round_end = tokio::time::Instant::now() + CLAIM_ROUND;
            loop {
                let remaining = round_end.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut got_any = false;
                for session in 0..sessions {
                    while let Some((packet, _)) = self.sessions[session].try_next_inbound() {
                        got_any = true;
                        let Some(reply) = decode_reply(&packet) else {
                            continue;
                        };
                        let GatewayReply::Lease { message } = reply else {
                            // Bulk/area traffic cannot exist yet; anything
                            // else here is the gateway talking about a session
                            // shape the rig did not ask for.
                            tracing::debug!(session, "non-lease reply during claim phase");
                            continue;
                        };
                        match message {
                            LeaseMsg::Grant {
                                claim_id,
                                entity,
                                lease_id,
                                seq,
                                ..
                            } => {
                                let cell = in_flight[session]
                                    .remove(&claim_id)
                                    .map(|placement| placement.cell);
                                let Some(cell) = cell else {
                                    tracing::warn!(
                                        session,
                                        ?entity,
                                        "grant for a claim this session never made"
                                    );
                                    continue;
                                };
                                self.leases.insert(
                                    entity,
                                    GrantedLease {
                                        lease_id,
                                        seq,
                                        cell,
                                    },
                                );
                            }
                            LeaseMsg::Deny {
                                claim_id,
                                entity,
                                reason,
                                retry_after_ms,
                            } => {
                                let placement = claim_id
                                    .and_then(|claim_id| in_flight[session].remove(&claim_id));
                                *denials.entry(format!("{reason:?}")).or_default() += 1;
                                match (reason, placement) {
                                    // The registrar's own back-pressure: requeue
                                    // and let the pacing above absorb it.
                                    (orrery_protocol::DenyReason::RateLimited, Some(p)) => {
                                        tracing::debug!(
                                            session,
                                            ?entity,
                                            retry_after_ms,
                                            "claim rate limited; requeued"
                                        );
                                        outstanding[session].push(p);
                                    }
                                    (reason, _) => {
                                        // Everything else is definitive. Fail
                                        // now, with the reason, rather than
                                        // degrading to unleased writes.
                                        bail!(
                                            "gateway denied the lease claim for {entity:?} on \
                                             session {session}: {reason:?}. The rig has no \
                                             unleased write path — seed the entity durably (its \
                                             committed cell is what a claim names) and rerun."
                                        );
                                    }
                                }
                            }
                            other => {
                                tracing::debug!(
                                    session,
                                    ?other,
                                    "lease control during claim phase"
                                );
                            }
                        }
                    }
                }
                if !got_any {
                    tokio::time::sleep(remaining.min(DRAIN_POLL_INTERVAL)).await;
                }
            }
            tracing::debug!(
                round,
                emitted,
                granted = self.leases.len(),
                total,
                "claim round complete"
            );

            // Nothing left to send and nothing left in flight, yet short of
            // the inventory: replies were lost, not denied. Requeue them.
            if outstanding.iter().all(Vec::is_empty)
                && in_flight.iter().all(HashMap::is_empty)
                && self.leases.len() < total
            {
                let mut requeued = 0usize;
                for (index, placement) in self.inventory.iter().enumerate() {
                    if !self.leases.contains_key(&placement.entity) {
                        outstanding[scheduler_shard(index, sessions)].push(*placement);
                        requeued += 1;
                    }
                }
                tracing::warn!(requeued, "claims with no reply; retrying");
            }
        }

        let elapsed = started.elapsed();
        tracing::info!(
            leases = self.leases.len(),
            claim_secs = elapsed.as_secs_f64(),
            rate_limited = denials.get("RateLimited").copied().unwrap_or_default(),
            "claim phase complete"
        );
        if self.emit_json {
            telemetry::run_footer(&format!(
                "claim phase: {} leases in {:.2}s",
                self.leases.len(),
                elapsed.as_secs_f64()
            ));
        }
        Ok(elapsed)
    }

    /// Renew every held lease on its owning session.
    ///
    /// One batched `Heartbeat` per session. Heartbeats do not change the
    /// durable sequence or the token (`lease.rs::heartbeat`), so the
    /// `(lease_id, seq)` the rig carries on its diffs stays valid across them.
    ///
    /// Renews the leases held by exactly the named sessions, rather than every
    /// session at once as it used to. Which of the two it does is the
    /// difference between the gateway receiving ~10 000 lease renewals inside
    /// a few milliseconds and receiving them spread over three seconds -- and
    /// each renewal is a `LeaseStore::locate`, i.e. an FDB read, so an
    /// all-sessions pass is an FDB burst.
    async fn heartbeat_sessions(&self, due: &[usize]) {
        let sessions = self.sessions.len().max(1);
        let mut per_session: Vec<Vec<(PersistId, LeaseId)>> = vec![Vec::new(); sessions];
        for (index, placement) in self.inventory.iter().enumerate() {
            if let Some(lease) = self.leases.get(&placement.entity) {
                per_session[scheduler_shard(index, sessions)]
                    .push((placement.entity, lease.lease_id));
            }
        }
        for (session, renew) in per_session.into_iter().enumerate() {
            if renew.is_empty() || !due.contains(&session) {
                continue;
            }
            send_msg(
                &self.sessions[session],
                &GatewayMsg::Lease {
                    message: LeaseMsg::Heartbeat {
                        renew,
                        tick: Tick::new(0),
                    },
                },
            )
            .await;
        }
    }

    /// Drive the load loop until the duration elapses.
    async fn drive(mut self) -> Result<RunStats> {
        let cfg = PersistClientConfig {
            flush_budget_bytes: FLUSH_BUDGET_BYTES,
            ..PersistClientConfig::default()
        };
        let mut intents = IntentQueue::new(1024);

        // Each connection owns an independent D16 scheduler and its byte
        // budget.  The fan-out check is expressed per session, so sharing one
        // scheduler and merely round-robining its selected output would cap a
        // 125-session demo at one session's 160 diffs/s.
        let sessions = self.sessions.len().max(1);
        let shard_size = self.inventory.len().div_ceil(sessions);
        let mut schedulers = scheduler_shards(&self.inventory, sessions, self.diff_hz);

        // Per-session state: scheduler shards, send counters, tick clock.
        let mut flush_index = 0_u64;
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
            )
            .await;
            area_pending[i] = Some(Instant::now());
        }

        let flush_period = Duration::from_secs_f64(1.0 / FLUSH_HZ as f64);
        let phase_slots = registration_phase_slots(self.diff_hz);
        let start = Instant::now();
        let mut elapsed = Duration::ZERO;
        let mut next_queue = start;
        let session_phase_period =
            Duration::from_secs_f64(1.0 / (FLUSH_HZ * SESSION_FLUSH_PHASE_SLOTS as u64) as f64);
        let mut next_session_flush = (0..sessions)
            .map(|session| start + session_phase_period * session_flush_phase(session) as u32)
            .collect::<Vec<_>>();
        let heartbeat_period = lease_heartbeat_period();
        let phased = heartbeat_phased();
        // Phased: session `i` renews at `i/sessions` of the way through the
        // period, so the same renewals reach the gateway spread evenly instead
        // of all at once. Unphased (the default, and what every published
        // number was measured on): one pass over every session.
        let mut next_session_heartbeat: Vec<Instant> = (0..sessions)
            .map(|session| {
                if phased {
                    start + heartbeat_period.mul_f64(session as f64 / sessions as f64)
                } else {
                    start + heartbeat_period
                }
            })
            .collect();
        let mut quic_rtt = LatencyHistogram::new();
        let mut next_rtt_sample = start;
        let mut max_rx_backlog = 0u64;

        while start.elapsed() < self.duration {
            // ── Receive: drain both lanes of every session ────────────────
            //
            // Each `GatewayLink` runs its own reader tasks and stamps arrival
            // there; this loop only takes what has already landed, so the D16
            // series measure the gateway rather than this loop's cadence.
            let mut inbound = Vec::new();
            for (session, link) in self.sessions.iter().enumerate() {
                while let Some((packet, received_at)) = link.try_next_inbound() {
                    inbound.push(InboundEvent {
                        session,
                        packet,
                        received_at,
                    });
                }
            }
            for event in inbound {
                self.handle_inbound(
                    event,
                    &mut schedulers,
                    &mut intents,
                    &mut stats,
                    &mut area_pending,
                );
            }

            let now = Instant::now();

            // ── Renew authority ──────────────────────────────────────────
            // Leases expire 10 s after their last renewal, so a 30 s run that
            // did not heartbeat would start being fenced out a third of the
            // way in and report the rejections as latency.
            let due: Vec<usize> = next_session_heartbeat
                .iter()
                .enumerate()
                .filter_map(|(session, at)| (now >= *at).then_some(session))
                .collect();
            if !due.is_empty() {
                self.heartbeat_sessions(&due).await;
                for session in due {
                    next_session_heartbeat[session] = now + heartbeat_period;
                }
            }

            // ── Path time ────────────────────────────────────────────────
            // One RTT gauge per session per sampling tick. Cheap (a stats
            // read), and the only number in the run that measures the wire
            // without the application on top of it.
            if now >= next_rtt_sample {
                for link in &self.sessions {
                    if let Some(rtt) = link.quic_rtt() {
                        quic_rtt.record(rtt);
                    }
                    max_rx_backlog = max_rx_backlog.max(link.rx_backlog());
                }
                next_rtt_sample = now + RTT_SAMPLE_INTERVAL;
            }

            // ── Load: queue each entity's diff for this global frame ─────
            //
            // Queueing is global so a state change is available to its owner
            // on that owner's next 20 Hz flush. The actual scheduler flushes
            // below are connection-phased: a frame is not a 125-session burst.
            if now >= next_queue {
                // Allocate payloads only for this frame's rate cohort. A
                // cohort is refreshed once per configured send interval,
                // immediately before the scheduler can spend its next credit.
                // This preserves trajectory ticks and session budgets while
                // avoiding 10x throwaway payload construction at 2 Hz.
                for (index, p) in self.inventory.iter().enumerate() {
                    let shard = scheduler_shard(index, sessions);
                    if !generation_cohort_is_due(index, sessions, phase_slots, flush_index) {
                        continue;
                    }
                    // No lease, no write. An entity whose lease the registrar
                    // withdrew mid-run is dropped from the load rather than
                    // written unfenced; `stats.leases_lost` fails the run.
                    let Some(lease) = self.leases.get(&p.entity).copied() else {
                        continue;
                    };
                    let tick_now = flush_index;
                    schedulers[shard].queue(DiffUplink {
                        // The committed cell the lease was granted at, not a
                        // trajectory position: `apply_fenced` admits a diff
                        // only where `by_cell[entity] == record.cell`, and a
                        // client cannot rekey (the gateway denies
                        // `LeaseMsg::Rekey` unconditionally).
                        cell: lease.cell,
                        grid: GridId::ROOT,
                        entity: p.entity,
                        tick: Tick::new(tick_now),
                        kind: RecordKind::ComponentDiff,
                        payload: synthetic_payload(p.entity, tick_now, self.cli.diff_payload_bytes),
                        seq: tick_now,
                        lease_id: Some(lease.lease_id),
                        authority_seq: Some(lease.seq),
                    });
                }
                flush_index += 1;

                // Resume the 20 Hz open-loop clock maintaining exact phase cadence.
                next_queue += flush_period;
                while next_queue <= now {
                    next_queue += flush_period;
                }
            }

            // Flush each connection at 20 Hz, offset within the global frame.
            // Each shard retains its own byte budget; only *when* its packets
            // are released changes. This removes a synthetic 1,000-packet
            // burst from the rig without reducing its 20,000 diff/s profile.
            for (session, sched) in schedulers.iter_mut().enumerate() {
                if now < next_session_flush[session] {
                    continue;
                }
                let out = sched.flush(&cfg, elapsed);
                for diff in out {
                    // Intent mix: a fraction of sends is upgraded to an intent
                    // instead of a diff (docs/12 §12.3 `intent_mix`). The
                    // decision is deterministic per (entity, send index).
                    if let Some(kind) = self.intent_for(diff.entity, diff.seq) {
                        let id = intent_id(diff.entity, diff.seq);
                        let intent = self.make_intent(session, id, kind);
                        if intents.submit(intent).is_some() {
                            stats.intents_sent += 1;
                            // The gateway binds `intent.issuer` to the
                            // connection's authenticated NodeId, so the intent
                            // must leave on the session whose key signed it.
                            self.intent_sessions.insert(id, session);
                        }
                        // The diff is still sent: the intent is *in addition
                        // to* the bulk stream (trades/crafts do not replace
                        // the entity's state diff).
                    }
                    send_msg(
                        &self.sessions[session],
                        &GatewayMsg::Diff { diff: diff.clone() },
                    )
                    .await;
                    // The datagram is now on the socket. Reporting the
                    // instant splits `bulk_ack_ms` into the rig's own send
                    // path and the wire; without it the flush-selection
                    // stamp is all the scheduler has, and everything this
                    // loop does between selecting a diff and writing it is
                    // reported as gateway latency.
                    sched.on_sent_at(diff.entity, diff.tick, Instant::now());
                    self.pending_diffs.insert(
                        (diff.entity, diff.tick),
                        DiffEvidence {
                            grid: diff.grid,
                            cell: diff.cell,
                            entity: diff.entity,
                            tick: diff.tick,
                            // The LSN is supplied by the durable ack.
                            lsn: orrery_protocol::Lsn::new(0, 0),
                            payload_digest: blake3::hash(&diff.payload).to_hex().to_string(),
                        },
                    );
                    stats.diffs_sent += 1;
                }
                // Advance to next period maintaining exact slot alignment.
                next_session_flush[session] += flush_period;
                while next_session_flush[session] <= now {
                    next_session_flush[session] += flush_period;
                }
            }

            // Drain queued intents to the wire after the phased bulk sends.
            // A load phase produces only its matching intent subset, so this
            // control traffic is phased with the bulk work as well.
            for intent in intents.drain() {
                let session = self
                    .intent_sessions
                    .get(&intent.intent_id)
                    .copied()
                    .unwrap_or((intent.intent_id as usize) % sessions);
                send_msg(
                    &self.sessions[session],
                    &GatewayMsg::SubmitIntent { intent },
                )
                .await;
            }

            // ── Telemetry drain (bounded-memory histograms → JSONL) ──────
            // The drain writes to stdout, which the test harness owns; the
            // `emit_json` flag is false under `cargo test` so the drive loop
            // stays silent there. The drain logic itself is covered by the
            // telemetry module's tests.
            if self.emit_json {
                drain_bulk_series(&sink, &schedulers);
                sink.drain_histogram(
                    telemetry::SERIES_CLIENT_SEND_BUFFER,
                    &aggregate_send_buffer(&self.sessions),
                );
                sink.drain_histogram(telemetry::SERIES_CLIENT_QUIC_RTT, &quic_rtt);
                sink.drain_histogram(telemetry::SERIES_INTENT_COMMIT, intents.intent_latency());
            }

            let now = Instant::now();
            let mut next_wake = next_queue;
            for &flush_at in &next_session_flush {
                if flush_at < next_wake {
                    next_wake = flush_at;
                }
            }
            if next_wake > now {
                let sleep_dur = next_wake.saturating_duration_since(now);
                tokio::time::sleep(sleep_dur.min(Duration::from_millis(1))).await;
            } else {
                tokio::task::yield_now().await;
            }
            elapsed = start.elapsed();
        }

        // Stop sending, then collect replies already in flight for a bounded
        // interval. This makes latency coverage explicit without allowing a
        // dead gateway to hang the gate.
        let drain_deadline = tokio::time::Instant::now() + FINAL_REPLY_DRAIN;
        loop {
            let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let mut inbound = Vec::new();
            for (session, link) in self.sessions.iter().enumerate() {
                while let Some((packet, received_at)) = link.try_next_inbound() {
                    inbound.push(InboundEvent {
                        session,
                        packet,
                        received_at,
                    });
                }
            }
            if inbound.is_empty() {
                // Nothing landed this pass: yield the drain interval in small
                // slices rather than busy-spinning the whole 500 ms.
                tokio::time::sleep(remaining.min(DRAIN_POLL_INTERVAL)).await;
                continue;
            }
            for event in inbound {
                self.handle_inbound(
                    event,
                    &mut schedulers,
                    &mut intents,
                    &mut stats,
                    &mut area_pending,
                );
            }
        }

        if self.emit_json {
            drain_bulk_series(&sink, &schedulers);
            sink.drain_histogram(
                telemetry::SERIES_CLIENT_SEND_BUFFER,
                &aggregate_send_buffer(&self.sessions),
            );
            sink.drain_histogram(telemetry::SERIES_CLIENT_QUIC_RTT, &quic_rtt);
            sink.drain_histogram(telemetry::SERIES_INTENT_COMMIT, intents.intent_latency());
        }
        check_bulk_reply_coverage(aggregate_bulk_latency(&schedulers).total(), &stats)?;
        tracing::info!(
            diffs = stats.diffs_sent,
            acks = stats.diff_acks,
            durable_acks = stats.durable_diff_acks,
            duplicate_durable_acks = stats.duplicate_durable_diff_acks,
            provisional_acks = stats.provisional_diff_acks,
            // Rejections are reported, always. 541 408 of them were invisible
            // in this line once, and the summary said the run went fine.
            diff_nacks = stats.diff_nacks,
            leases = self.leases.len(),
            leases_lost = stats.leases_lost,
            intents = stats.intents_sent,
            intent_acks = stats.intent_acks,
            bulk_p99_us = aggregate_bulk_latency(&schedulers).p99().as_micros() as u64,
            // The deepest the transport's inbound queue got on any one
            // session. A bulk-ack tail with this at zero did not accrue on
            // this side of the socket.
            max_rx_backlog,
            intent_p99_us = intents.intent_latency().p99().as_micros() as u64,
            // Same population, stopped at the ack's arrival instead of at the
            // rig's handling of it. The difference between the two is this
            // rig's dispatch delay, and it is the only way to tell a
            // server-side tail from a client-side one without a new series.
            intent_arrival_p50_us = intents.arrival_latency().p50().as_micros() as u64,
            intent_arrival_p90_us = intents.arrival_latency().p90().as_micros() as u64,
            intent_arrival_p99_us = intents.arrival_latency().p99().as_micros() as u64,
            intent_arrival_max_us = intents.arrival_latency().max().unwrap_or_default().as_micros() as u64,
            "run complete"
        );
        if stats.leases_lost > 0 {
            bail!(
                "the registrar withdrew {} lease(s) mid-run: the rig stopped writing those \
                 entities rather than falling back to unfenced writes, but the run's durability \
                 evidence is incomplete",
                stats.leases_lost
            );
        }
        if stats.durable_diff_acks == 0 {
            bail!(
                "no durable bulk acknowledgement in the whole run ({} diffs sent, {} nacked): \
                 nothing was journaled, so there is no durability evidence to gate on",
                stats.diffs_sent,
                stats.diff_nacks
            );
        }
        Ok(stats)
    }

    fn handle_inbound(
        &mut self,
        event: InboundEvent,
        schedulers: &mut [UplinkScheduler],
        intents: &mut IntentQueue,
        stats: &mut RunStats,
        area_pending: &mut [Option<Instant>],
    ) {
        self.handle_reply(
            event.session,
            &event.packet,
            event.received_at,
            schedulers,
            intents,
            stats,
            area_pending,
        );
    }

    /// Handle one inbound reply from session `i`, from either lane.
    #[allow(clippy::too_many_arguments)]
    fn handle_reply(
        &mut self,
        session: usize,
        pkt: &[u8],
        received_at: Instant,
        schedulers: &mut [UplinkScheduler],
        intents: &mut IntentQueue,
        stats: &mut RunStats,
        area_pending: &mut [Option<Instant>],
    ) {
        let Some(reply) = decode_reply(pkt) else {
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
                if let Some(sched) = schedulers.get_mut(session) {
                    sched.on_ack_at(entity, tick, provisional, received_at);
                }
                stats.diff_acks += 1;
                if !provisional {
                    stats.durable_diff_acks += 1;
                    if let Some(mut evidence) = self.pending_diffs.remove(&(entity, tick)) {
                        // First durable ack for this exact send. The
                        // scheduler samples latency on exactly these, so this
                        // is the count the coverage check compares against.
                        stats.first_durable_diff_acks += 1;
                        evidence.lsn = lsn;
                        if let Some(log) = &mut self.ack_log {
                            log.record(&AckRecord::Diff(evidence));
                        }
                    } else {
                        // A duplicate of an already-credited ack: the bulk
                        // lane is unreliable *and* unordered, and an unacked
                        // diff is resent, so the gateway can answer the same
                        // `(entity, tick)` more than once. Counted in
                        // `diff_acks`, deliberately not in the coverage
                        // denominator.
                        stats.duplicate_durable_diff_acks += 1;
                        tracing::debug!(?entity, ?tick, "duplicate durable bulk ack");
                    }
                } else {
                    // Deliberately retain the outbound record: a provisional
                    // reply is scheduler feedback only, never a durable
                    // acknowledgement for the kill-9 recovery comparator.
                    stats.provisional_diff_acks += 1;
                }
            }
            GatewayReply::BulkNack {
                entity,
                tick,
                reason,
                lease,
            } => {
                if let Some(sched) = schedulers.get_mut(session) {
                    sched.on_nack_at(entity, tick, received_at);
                }
                stats.diff_nacks += 1;
                // The rig holds a lease for every entity it writes, so the
                // current-holder row in a nack is actionable: it says the
                // fencing token the rig is carrying is no longer the live one.
                let held = self.leases.get(&entity).map(|held| held.lease_id);
                let fenced_out = lease
                    .as_ref()
                    .is_some_and(|row| held.is_some_and(|held| row.lease_id != held));
                if fenced_out {
                    self.leases.remove(&entity);
                    stats.leases_lost += 1;
                    tracing::warn!(
                        session,
                        ?entity,
                        ?tick,
                        ?lease,
                        "fenced out: the rig's lease is no longer the live row"
                    );
                } else {
                    tracing::debug!(session, ?entity, ?tick, reason, ?lease, "bulk nack");
                }
            }
            GatewayReply::IntentAck { intent_id, outcome } => {
                let evidence_outcome = match &outcome {
                    IntentOutcome::Committed { tick, minted } => IntentOutcomeEvidence::Committed {
                        tick: *tick,
                        minted: minted.clone(),
                    },
                    IntentOutcome::Rejected { reason } => {
                        IntentOutcomeEvidence::Rejected { reason: *reason }
                    }
                };
                if let Some(log) = &mut self.ack_log {
                    log.record(&AckRecord::Intent {
                        intent_id: intent_id.to_string(),
                        outcome: evidence_outcome,
                    });
                }
                if matches!(outcome, IntentOutcome::Committed { .. }) {
                    stats.intent_acks += 1;
                }
                // The arrival stamp the bulk arms already use. It was
                // dropped here, which put this rig's own poll cadence inside
                // `intent_commit_ms` and inside no other D16 series.
                intents.on_ack_at(intent_id, outcome, received_at);
                // Retire the settled intent, which is what `IntentQueue`'s
                // contract asks of a client that has observed the terminal
                // status — and what the rig was not doing.
                //
                // Without this the queue never gives a slot back, so it fills
                // to its 1024 capacity and `submit` returns `None` for the rest
                // of the run. At a 3 % mix and 18 000 diffs/s that takes under
                // two seconds: every `intent_commit_ms` sample in a 30 s point
                // came from the opening burst, while sessions were still
                // connecting, and `--intent-mix` had no effect on anything past
                // it. The intent rate is now the mix, for the whole run.
                intents.retire(intent_id);
                self.intent_sessions.remove(&intent_id);
            }
            GatewayReply::AreaPage { .. } => {
                if let Some(t0) = area_pending.get_mut(session).and_then(|s| s.take()) {
                    let dt = received_at.checked_duration_since(t0).unwrap_or_default();
                    if self.cli.json {
                        telemetry::sample(telemetry::SERIES_AREA_FIRST_PAGE, dt.as_micros() as u64);
                    }
                    tracing::debug!(session, first_page_ms = dt.as_millis(), "first area page");
                }
            }
            GatewayReply::AreaLoadError { cell, kind } => {
                tracing::warn!(session, ?cell, kind, "area load failed");
            }
            GatewayReply::HelloAck { .. } => {}
            GatewayReply::HelloRefused {
                protocol, reason, ..
            } => {
                tracing::warn!(session, protocol, reason, "gateway refused the session");
            }
            // The rig holds leases, so lease control mid-run is meaningful:
            // it is the registrar telling the rig it has lost authority. A
            // withdrawn lease drops the entity from the load and is counted;
            // it never silently degrades into an unfenced write.
            GatewayReply::Lease { message } => match message {
                LeaseMsg::HeartbeatAck { invalid, .. } => {
                    if invalid.is_empty() {
                        return;
                    }
                    // Matched on the pair, not the bare token: `LeaseId` is a
                    // per-row counter, so an id-only match would drop every
                    // entity this rig holds at the same counter value and
                    // wildly overstate `leases_lost`.
                    let dropped: Vec<PersistId> = self
                        .leases
                        .iter()
                        .filter(|(entity, held)| invalid.contains(&(**entity, held.lease_id)))
                        .map(|(entity, _)| *entity)
                        .collect();
                    for entity in &dropped {
                        self.leases.remove(entity);
                    }
                    stats.leases_lost += dropped.len() as u64;
                    tracing::warn!(
                        session,
                        invalid = invalid.len(),
                        dropped = dropped.len(),
                        "registrar refused to renew leases"
                    );
                }
                LeaseMsg::Expire {
                    entity,
                    lease_id,
                    reason,
                    ..
                } => {
                    if self
                        .leases
                        .get(&entity)
                        .is_some_and(|held| held.lease_id == lease_id)
                    {
                        self.leases.remove(&entity);
                        stats.leases_lost += 1;
                    }
                    tracing::warn!(
                        session,
                        ?entity,
                        ?lease_id,
                        ?reason,
                        "lease expired mid-run"
                    );
                }
                other => {
                    tracing::warn!(session, ?other, "unexpected lease control on a rig session");
                }
            },
            GatewayReply::InterestAck { epoch, reason } => {
                tracing::warn!(
                    session,
                    ?epoch,
                    reason,
                    "unexpected interest ack on a rig session"
                );
            }
            // Same reasoning as the two above: the rig files no discrepancy
            // reports, so a verdict addressed to it means the gateway has this
            // connection confused with a witnessing peer.
            GatewayReply::ReportVerdict {
                subject,
                entity,
                reason,
                ..
            } => {
                tracing::warn!(
                    session,
                    ?subject,
                    ?entity,
                    reason,
                    "unexpected report verdict on a rig session"
                );
            }
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
    /// epoch. The P2 gateway verifies the issuer signature before durable
    /// execution, so the rig signs the canonical preimage with the same key
    /// that established this endpoint's authenticated NodeId.
    fn make_intent(&self, session: usize, id: u128, kind: String) -> Intent {
        signed_intent(
            id,
            self.endpoints[session].id(),
            &self.signing_keys[session],
            kind,
        )
    }
}

/// Mint the session token the gateway's `Hello` check requires.
///
/// The token is bound to `node` — the rig's own iroh identity — because the
/// gateway refuses a token whose claimed node is not the connection's
/// authenticated remote. Without `--issuer-secret` this falls back to a
/// placeholder, which only a gateway configured with no verifier will admit.
/// A real `persistd` refuses it, and the refusal is quieter than it looks: the
/// connection stays up and the hello simply goes unanswered. Hence the warning.
fn session_token(cli: &Cli, node: NodeId) -> Result<Vec<u8>> {
    let Some(secret) = cli.issuer_secret.as_deref() else {
        tracing::warn!(
            "no --issuer-secret: sending a placeholder session token, which a gateway \
             started with --issuer-key will refuse"
        );
        return Ok(b"p2-load".to_vec());
    };
    let issuer: SecretKey = secret
        .parse()
        .context("invalid --issuer-secret (expected hex)")?;
    let issued_at_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_millis(),
    )
    .context("system clock is beyond the u64 millisecond range")?;
    let claims = orrery_protocol::SessionTokenClaimsV1::new(
        orrery_protocol::AccountId::new(cli.account_id),
        node,
        orrery_protocol::UnixMillis::new(issued_at_ms),
        orrery_protocol::SessionTokenTtlMs::new(orrery_protocol::MAX_SESSION_TOKEN_TTL_MS),
        orrery_protocol::SessionStanding::Good,
        orrery_protocol::IssuerKeyId::new(cli.issuer_key_id),
    );
    orrery_protocol::SessionTokenV1::sign(claims, &issuer)
        .map_err(|e| anyhow::anyhow!("sign session token: {e:?}"))?
        .encode()
        .map_err(|e| anyhow::anyhow!("encode session token: {e:?}"))
}

/// Build and canonically sign one load-generated intent. Kept separate from
/// [`Rig`] so this critical wire invariant has a direct unit regression.
fn signed_intent(id: u128, issuer: NodeId, signing_key: &SecretKey, kind: String) -> Intent {
    debug_assert_eq!(issuer, signing_key.public());
    let mut intent = Intent {
        intent_id: id,
        issuer,
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op: op_code(&kind),
            args: Bytes::from(kind.into_bytes()),
        }],
        attestations: Vec::<Attestation>::new(),
        // `Intent::sign` replaces this placeholder immediately.
        signature: signing_key.sign(&[]),
    };
    intent.sign(signing_key);
    intent
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

/// A deterministic intent id for (entity, send index): high 64 bits the
/// entity, low 64 the send index. Idempotency keys are per-intent (D11 §2.2);
/// this makes the rig's keys unique per send and stable across resends.
fn intent_id(entity: PersistId, send_index: u64) -> u128 {
    ((entity.0 as u128) << 64) | send_index as u128
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
    fn recovery_rereads_the_leaves_the_acks_name_not_a_synthesised_lattice() {
        // Regression for the P2 kill-9 gate's `MissingBulk` wall. The recovery
        // reader used to re-read leaves from the *rig's inventory*, falling
        // back to `synthetic_inventory` when no `--manifest` was given — and
        // the gate's verify invocation gives none. Both lattices sit at
        // `INTEREST_LEVEL`, and `read_snapshot` matches a requested cell
        // against a stored cell by prefix, so a level-21 request matches only
        // itself: the reader asked for 100 cells that held nothing and
        // reported 99 of 100 durable entities missing (the hundredth was a
        // lone coincidental collision). Measured 2026-08-17 against a promoted
        // node that provably held all 100.
        //
        // The two lattices below are deliberately disjoint, which is what
        // makes this check non-vacuous: a reader sourcing cells from anywhere
        // but the acknowledgements cannot produce this set.
        let seeded: Vec<CellId> = [(3i32, 5, 7), (11, 13, 17), (-4, 9, -2)]
            .into_iter()
            .map(|(x, y, z)| {
                CellId::from_coords(glam::IVec3::new(x, y, z), orrery_protocol::INTEREST_LEVEL)
                    .expect("interest-level coordinate is in range")
            })
            .collect();
        // Two entities share a leaf, so the plan must also de-duplicate: the
        // gateway answers one page per requested cell and a repeated cell is a
        // repeated page, not a second proof.
        let acked: Vec<DiffEvidence> = [
            (1u64, seeded[0]),
            (2, seeded[1]),
            (3, seeded[1]),
            (4, seeded[2]),
        ]
        .into_iter()
        .map(|(id, cell)| DiffEvidence {
            grid: GridId::ROOT,
            cell,
            entity: PersistId::new(id),
            tick: Tick::new(9),
            lsn: orrery_protocol::Lsn::new(0, id),
            payload_digest: String::new(),
        })
        .collect();

        let planned = recovery_leaf_cells(&acked);
        let expected: Vec<CellId> = seeded
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(
            planned, expected,
            "the plan must be exactly the acknowledged leaves, de-duplicated"
        );

        // The failure this guards: the synthesised lattice the old fallback
        // produced for these same entity ids shares no cell with the acked
        // ones, so every read landed on an empty leaf.
        let synthesised: BTreeSet<_> =
            synthetic_inventory(crate::cli::DEFAULT_ENTITIES, crate::cli::DEFAULT_CELLS)
                .into_iter()
                .filter(|placement| placement.entity <= PersistId::new(4))
                .map(|placement| placement.cell)
                .collect();
        assert!(
            synthesised.is_disjoint(&planned.iter().copied().collect()),
            "the fixture no longer separates the two sources; pick different seeded coordinates"
        );
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
    fn scheduler_shards_give_every_session_its_own_budget() {
        // Regression for the P2 rig throughput bug: the fan-out calculation
        // counted 125 independent session budgets, while the driver had only
        // one scheduler and round-robined its output. Every scheduler must
        // own a balanced disjoint entity set.
        let inventory = synthetic_inventory(10_000, 128);
        let schedulers = scheduler_shards(&inventory, 125, 2.0);
        assert_eq!(schedulers.len(), 125);
        assert!(schedulers.iter().all(|scheduler| scheduler.len() == 80));
        assert_eq!(
            schedulers.iter().map(UplinkScheduler::len).sum::<usize>(),
            10_000
        );
    }

    #[test]
    fn phased_registrations_spread_default_fan_out_per_session() {
        // The P2 profile is 10k entities at 2 Hz over a 20 Hz flush clock.
        // Its first rate interval must feed exactly 1k registrations into
        // each 50 ms window, or the schedulers will later emit a 10k burst.
        // Because the phase uses each connection's local ordinal, the same
        // proof holds per session: 80 entities / 10 phases = 8 per window.
        let entities = 10_000;
        let sessions = 125;
        let slots = registration_phase_slots(2.0);
        assert_eq!(slots, 10);

        let mut global = vec![0usize; slots];
        let mut per_session = vec![vec![0usize; slots]; sessions];
        for index in 0..entities {
            let session = scheduler_shard(index, sessions);
            let phase = registration_phase(index, sessions, slots) as usize;
            global[phase] += 1;
            per_session[session][phase] += 1;
        }

        assert_eq!(global, vec![1_000; slots]);
        assert!(per_session.iter().all(|counts| counts == &vec![8; slots]));
    }

    #[test]
    fn session_flush_phases_spread_default_fan_out_within_a_frame() {
        // The default P2 fan-out has 125 independent connections. They must
        // not all flush on the same 50 ms boundary: with eight diffs per
        // session that would make an artificial 1,000-packet burst. Twenty
        // intra-frame phases leave six or seven sessions in each 2.5 ms slot.
        let mut counts = [0usize; SESSION_FLUSH_PHASE_SLOTS];
        for session in 0..125 {
            counts[session_flush_phase(session)] += 1;
        }
        assert_eq!(counts.iter().sum::<usize>(), 125);
        assert_eq!(*counts.iter().min().unwrap(), 6);
        assert_eq!(*counts.iter().max().unwrap(), 7);
    }

    #[test]
    fn cold_profile_is_rate_limited_and_ack_independent() {
        // Reproduce the load driver's registration, queue, flush, and ack
        // flow with a deterministic clock. A fresh UplinkScheduler starts
        // with zero credit, so an entity's first 2 Hz send is one 500 ms
        // interval after its phased registration. That makes a cold 30 s run
        // contain 59 sends/entity (590k for the P2 population), not 600k.
        // Thereafter every entity sends exactly twice in every one-second
        // window; in particular this catches a future accidental 20 Hz
        // queue/flush coupling.
        let entities = 1_000_usize;
        let sessions = 13;
        let inventory = synthetic_inventory(entities as u64, 100);
        let phase_slots = registration_phase_slots(2.0);
        let simulate = |ack_delay_frames: Option<u64>| {
            let mut schedulers = scheduler_shards(&inventory, sessions, 2.0);
            let cfg = PersistClientConfig {
                flush_budget_bytes: FLUSH_BUDGET_BYTES,
                ..PersistClientConfig::default()
            };
            let frame = Duration::from_secs_f64(1.0 / FLUSH_HZ as f64);
            let mut sent_per_second = [0_usize; 30];
            let mut pending_acks: Vec<(u64, usize, PersistId, Tick)> = Vec::new();
            let mut payloads_allocated = 0_usize;

            for frame_index in 0..(30 * FLUSH_HZ) {
                let mut retained = Vec::new();
                for (due, shard, entity, tick) in pending_acks.drain(..) {
                    if due <= frame_index {
                        schedulers[shard].on_ack(entity, tick, false);
                    } else {
                        retained.push((due, shard, entity, tick));
                    }
                }
                pending_acks = retained;

                for (index, placement) in inventory.iter().enumerate() {
                    if !generation_cohort_is_due(index, sessions, phase_slots, frame_index) {
                        continue;
                    }
                    payloads_allocated += 1;
                    schedulers[scheduler_shard(index, sessions)].queue(DiffUplink {
                        cell: placement.cell,
                        grid: GridId::ROOT,
                        entity: placement.entity,
                        tick: Tick::new(frame_index),
                        kind: RecordKind::ComponentDiff,
                        payload: synthetic_payload(placement.entity, frame_index, 64),
                        seq: frame_index,
                        lease_id: None,
                        authority_seq: None,
                    });
                }

                let elapsed = frame * frame_index as u32;
                for (shard, scheduler) in schedulers.iter_mut().enumerate() {
                    for diff in scheduler.flush(&cfg, elapsed) {
                        sent_per_second[(frame_index / FLUSH_HZ) as usize] += 1;
                        if let Some(delay) = ack_delay_frames {
                            pending_acks.push((frame_index + delay, shard, diff.entity, diff.tick));
                        }
                    }
                }
            }
            (sent_per_second, payloads_allocated)
        };

        let immediate = simulate(Some(0));
        let delayed = simulate(Some(7));
        let lost = simulate(None);
        assert_eq!(immediate.0, delayed.0, "delayed acks changed offered rate");
        assert_eq!(immediate.0, lost.0, "lost acks changed offered rate");
        assert_eq!(immediate.1, entities * 30 * 2);
        assert_eq!(immediate.1, delayed.1);
        assert_eq!(immediate.1, lost.1);
        assert_eq!(immediate.0.iter().sum::<usize>(), entities * (30 * 2 - 1));
        assert_eq!(immediate.0[0], entities);
        assert!(immediate.0[1..].iter().all(|&count| count == entities * 2));
    }

    #[test]
    fn default_open_loop_cohorts_are_exactly_one_thousand_per_frame() {
        let entities = 10_000;
        let sessions = 125;
        let slots = registration_phase_slots(2.0);
        let counts = (0..slots as u64)
            .map(|flush_index| {
                (0..entities)
                    .filter(|&index| generation_cohort_is_due(index, sessions, slots, flush_index))
                    .count()
            })
            .collect::<Vec<_>>();
        assert_eq!(counts, vec![1_000; slots]);
    }

    #[test]
    fn registration_phases_remain_balanced_for_non_divisible_profiles() {
        // A small arbitrary workload exercises the ceiling phase count used
        // for rates such as 3 Hz, where a rate interval is not an integral
        // number of 50 ms windows. Every session's cohorts differ by at most
        // one entity, so no connection receives an avoidable startup burst.
        let entities = 1_003;
        let sessions = 13;
        let slots = registration_phase_slots(3.0);
        assert_eq!(slots, 7);

        let mut per_session = vec![vec![0usize; slots]; sessions];
        for index in 0..entities {
            let session = scheduler_shard(index, sessions);
            let phase = registration_phase(index, sessions, slots) as usize;
            per_session[session][phase] += 1;
        }

        for counts in per_session {
            let min = *counts.iter().min().unwrap();
            let max = *counts.iter().max().unwrap();
            assert!(max - min <= 1, "unbalanced phase cohorts: {counts:?}");
        }
    }

    #[test]
    fn final_drain_requires_complete_durable_reply_coverage() {
        // Coverage is durable acks only. A nack is not a sample any more
        // (`UplinkScheduler::on_nack_at`), so the three nacks below must not
        // be demanded of the histogram — and must not be able to *supply* the
        // count either, which is what let a zero-write run gate green.
        let stats = RunStats {
            durable_diff_acks: 2,
            first_durable_diff_acks: 2,
            diff_nacks: 3,
            ..RunStats::default()
        };
        assert!(check_bulk_reply_coverage(2, &stats).is_ok());
        let error = check_bulk_reply_coverage(5, &stats).unwrap_err();
        assert!(format!("{error:#}").contains("sampled 5 of 2"));

        // The pathological shape this whole lane exists to catch: every diff
        // rejected, nothing durable. It cannot produce a covered run.
        let all_rejected = RunStats {
            diffs_sent: 541_408,
            diff_nacks: 541_408,
            ..RunStats::default()
        };
        assert!(check_bulk_reply_coverage(541_408, &all_rejected).is_err());
        assert!(check_bulk_reply_coverage(0, &all_rejected).is_ok());

        // A duplicate durable ack is not a sample and must not be demanded of
        // the histogram: the gateway can answer one resent `(entity, tick)`
        // twice, and the second reply finds no send instant to measure from.
        let with_duplicates = RunStats {
            durable_diff_acks: 15_601,
            first_durable_diff_acks: 15_542,
            duplicate_durable_diff_acks: 59,
            ..RunStats::default()
        };
        assert!(check_bulk_reply_coverage(15_542, &with_duplicates).is_ok());
        assert!(check_bulk_reply_coverage(15_541, &with_duplicates).is_err());
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
    fn generated_intent_verifies_as_its_connected_issuer() {
        let key = SecretKey::from_bytes(&[0x5au8; 32]);
        let intent = signed_intent(42, key.public(), &key, "trade".to_owned());

        assert!(
            intent.verify_issuer(),
            "p2-load must sign the canonical preimage with the endpoint identity"
        );
        assert_eq!(intent.issuer, key.public());
        assert_eq!(intent.ops[0].op, op_code("trade"));
    }

    #[test]
    fn recovery_snapshot_finds_successor_moved_outside_ack_cell() {
        let entity = PersistId::new(42);
        let acknowledged_cell = CellId::from_coords(glam::IVec3::new(2, 0, 2), 3).unwrap();
        let successor_cell = CellId::from_coords(glam::IVec3::new(3, 0, 2), 3).unwrap();
        let expected = DiffEvidence {
            grid: GridId::ROOT,
            cell: acknowledged_cell,
            entity,
            tick: Tick::new(255),
            lsn: orrery_protocol::Lsn::new(1, 7),
            payload_digest: blake3::hash(&synthetic_payload(entity, 255, 64))
                .to_hex()
                .to_string(),
        };

        // The leaf re-read is deliberately not limited to the historical ack
        // cell: a valid successor may have moved, but its returned page cell
        // is still retained as strict storage-cell evidence.
        assert_ne!(acknowledged_cell, successor_cell);
        let recovered = recovered_snapshot_diffs(
            [expected.entity].into_iter().collect(),
            BTreeMap::from([(
                successor_cell,
                BTreeMap::from([(
                    0,
                    (
                        vec![entity, PersistId::new(999)],
                        vec![
                            synthetic_payload(entity, 266, 64),
                            synthetic_payload(PersistId::new(999), 266, 64),
                        ],
                    ),
                )]),
            )]),
        )
        .unwrap();

        assert_eq!(recovered.len(), 1, "unwanted rows are filtered");
        assert_eq!(recovered[0].entity, entity);
        assert_eq!(recovered[0].tick, Tick::new(266));
        assert_eq!(recovered[0].cell, successor_cell);
        assert_ne!(recovered[0].payload_digest, expected.payload_digest);
    }

    #[test]
    fn duration_strings_parse() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("10x").is_err());
    }

    #[tokio::test]
    async fn area_load_error_is_ignored_for_latency_tracking() {
        let (endpoint, signing_key) = bind_endpoint(None).await.expect("bind test endpoint");
        let cli = Cli {
            gateway: node(1),
            addr: SocketAddr::from(([127, 0, 0, 1], 1)),
            entities: 1,
            cells: 1,
            diff_hz: 2.0,
            intent_mix: String::new(),
            sessions: 1,
            duration_secs: 1,
            manifest: None,
            scenario: None,
            json: false,
            ack_log: None,
            verify_recovery: false,
            fdb_cluster_file: None,
            recovery_cutoff: None,
            output: None,
            diff_payload_bytes: 64,
            secret_key: None,
            issuer_secret: None,
            issuer_key_id: 1,
            account_id: 1,
        };
        let mut rig = Rig {
            cli: &cli,
            emit_json: false,
            endpoints: vec![endpoint],
            signing_keys: vec![signing_key],
            sessions: Vec::new(),
            inventory: Vec::new(),
            diff_hz: 2.0,
            intent_mix: BTreeMap::new(),
            duration: Duration::from_secs(1),
            ack_log: None,
            pending_diffs: HashMap::new(),
            leases: HashMap::new(),
            intent_sessions: HashMap::new(),
        };
        let mut sched = vec![UplinkScheduler::new()];
        let mut intents = IntentQueue::new(1024);
        let mut stats = RunStats::default();
        let mut area_pending = vec![Some(Instant::now())];
        let reply = GatewayReply::AreaLoadError {
            cell: orrery_protocol::CellId::ROOT,
            kind: orrery_protocol::AREA_LOAD_ERR_LIVE,
        };

        rig.handle_reply(
            0,
            &encode_stream_frame(&reply),
            Instant::now(),
            &mut sched,
            &mut intents,
            &mut stats,
            &mut area_pending,
        );

        assert!(
            area_pending[0].is_some(),
            "area-load timer must remain pending"
        );
        assert_eq!(stats.diffs_sent, 0);
        assert_eq!(stats.diff_acks, 0);
        assert_eq!(stats.durable_diff_acks, 0);
        assert_eq!(stats.provisional_diff_acks, 0);
        assert_eq!(stats.intents_sent, 0);
        assert_eq!(stats.intent_acks, 0);
    }

    #[tokio::test]
    async fn provisional_bulk_ack_is_not_written_as_durable_evidence() {
        let (endpoint, signing_key) = bind_endpoint(None).await.expect("bind test endpoint");
        let cli = Cli {
            gateway: node(1),
            addr: SocketAddr::from(([127, 0, 0, 1], 1)),
            entities: 1,
            cells: 1,
            diff_hz: 2.0,
            intent_mix: String::new(),
            sessions: 1,
            duration_secs: 1,
            manifest: None,
            scenario: None,
            json: false,
            ack_log: None,
            verify_recovery: false,
            fdb_cluster_file: None,
            recovery_cutoff: None,
            output: None,
            diff_payload_bytes: 64,
            secret_key: None,
            issuer_secret: None,
            issuer_key_id: 1,
            account_id: 1,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acks.jsonl");
        let evidence = DiffEvidence {
            grid: GridId::ROOT,
            cell: CellId::ROOT,
            entity: PersistId::new(99),
            tick: Tick::new(4),
            lsn: orrery_protocol::Lsn::new(0, 0),
            payload_digest: "outbound".to_owned(),
        };
        let mut pending_diffs = HashMap::new();
        pending_diffs.insert((evidence.entity, evidence.tick), evidence);
        let mut rig = Rig {
            cli: &cli,
            emit_json: false,
            endpoints: vec![endpoint],
            signing_keys: vec![signing_key],
            sessions: Vec::new(),
            inventory: Vec::new(),
            diff_hz: 2.0,
            intent_mix: BTreeMap::new(),
            duration: Duration::from_secs(1),
            ack_log: Some(AckLog::open(&path).unwrap()),
            pending_diffs,
            leases: HashMap::new(),
            intent_sessions: HashMap::new(),
        };
        let mut sched = vec![UplinkScheduler::new()];
        let mut intents = IntentQueue::new(1024);
        let mut stats = RunStats::default();
        let mut area_pending = vec![None];
        let reply = GatewayReply::BulkAck {
            entity: PersistId::new(99),
            tick: Tick::new(4),
            lsn: orrery_protocol::Lsn::new(8, 1),
            provisional: true,
        };

        rig.handle_reply(
            0,
            &encode_datagram(&reply),
            Instant::now(),
            &mut sched,
            &mut intents,
            &mut stats,
            &mut area_pending,
        );
        drop(rig);

        assert_eq!(stats.diff_acks, 1);
        assert_eq!(stats.durable_diff_acks, 0);
        assert_eq!(stats.provisional_diff_acks, 1);
        assert!(
            std::fs::read_to_string(path).unwrap().is_empty(),
            "a provisional bulk reply must never enter acks.jsonl"
        );
    }

    fn node(n: u8) -> orrery_protocol::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn ack_log_is_append_only_and_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acks.jsonl");
        let evidence = |entity, tick, segment, offset| DiffEvidence {
            grid: GridId::ROOT,
            cell: CellId::ROOT,
            entity: PersistId::new(entity),
            tick: Tick::new(tick),
            lsn: orrery_protocol::Lsn::new(segment, offset),
            payload_digest: format!("digest-{entity}-{tick}"),
        };

        {
            let mut log = AckLog::open(&path).unwrap();
            log.record(&AckRecord::Diff(evidence(7, 42, 3, 4096)));
            log.record(&AckRecord::Intent {
                intent_id: "213458173728644058818963591144807231488".to_string(),
                outcome: IntentOutcomeEvidence::Committed {
                    tick: Tick::new(9),
                    minted: vec![PersistId::new(77)],
                },
            });
            log.record(&AckRecord::Diff(evidence(8, 43, 3, 8192)));
        }

        // Re-open (as the kill-9 harness would) and parse every line back.
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one JSON line per ack, append-only");

        let first: AckRecord = serde_json::from_str(lines[0]).unwrap();
        match first {
            AckRecord::Diff(DiffEvidence {
                grid,
                cell,
                entity,
                tick,
                lsn,
                payload_digest,
            }) => {
                assert_eq!(grid, GridId::ROOT);
                assert_eq!(cell, CellId::ROOT);
                assert_eq!(entity, PersistId::new(7));
                assert_eq!(tick, Tick::new(42));
                assert_eq!(lsn, orrery_protocol::Lsn::new(3, 4096));
                assert_eq!(payload_digest, "digest-7-42");
            }
            other => panic!("expected a diff ack, got {other:?}"),
        }
        let second: AckRecord = serde_json::from_str(lines[1]).unwrap();
        match second {
            AckRecord::Intent { intent_id, outcome } => {
                assert_eq!(
                    intent_id,
                    // 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00 in decimal —
                    // the harness compares the string, so this is lossless.
                    "213458173728644058818963591144807231488"
                );
                assert_eq!(
                    outcome,
                    IntentOutcomeEvidence::Committed {
                        tick: Tick::new(9),
                        minted: vec![PersistId::new(77)],
                    }
                );
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
            log.record(&AckRecord::Diff(evidence(9, 50, 4, 0)));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4);
        for line in text.lines() {
            let _: AckRecord = serde_json::from_str(line).expect("every line parses");
        }
    }
}
