//! The `persistd` reference binary (docs/10-crates.md §11, P2 gaps #2/#7).
//!
//! The current binary is a **single-node** harness: it connects to
//! FoundationDB when asked, checkpoints on the 20 s jittered cadence, serves
//! never-loaded cells from the cold store, keeps the same gateway [`NodeId`]
//! across a kill -9 (via `--secret-key`), and can host one or more explicit
//! local shard cells via `--shard`.
//!
//! When `--fdb-cluster-file` is set, the binary connects to FoundationDB for
//! checkpoint storage and fencing (D11 §6). A volatile in-memory authority
//! path requires an explicit development switch; production authority never
//! starts without FDB.
//!
//! A static two-process topology is also supported: a primary owns the shard
//! actors and gateway, while its one follower owns only a mirrored journal and
//! the chain gRPC listener. The follower is deliberately outside the client
//! write path.
//!
//! On startup the binary prints the gateway's [`EndpointAddr`] as a single-line
//! JSON object on stdout (tracing stays on stderr) so a harness can find the
//! address. Log verbosity on that stderr stream is `RUST_LOG`; with the
//! variable unset the filter is `info`, which is what this binary has always
//! emitted. Nothing tracing writes ever reaches stdout, so raising the filter
//! cannot disturb the readiness contract. The two signals that trigger graceful shutdown are SIGTERM and
//! Ctrl-C. `--metrics-jsonl` appends `sample_batch` records and gateway
//! counter records to a separate P2 artifact file; it never alters the stdout
//! readiness contract. The gateway counters themselves are collected
//! unconditionally — the flag opens a sink, it does not start the
//! measurement — but `persistd` has no scrape or admin surface, so on a node
//! started without it those counters stay in-process until the D12 OTel
//! bridge lands.
//!
//! The demo path this enables: start the binary with `--secret-key`, load it,
//! `kill -9` the process, restart from the same FDB cluster and secret key, and
//! the world resumes (RPO 0 intents, bulk bounded by the journal window).

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use glam::IVec3;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

use orrery_persistd::cluster::{ColdFallbackRouter, Router};
use orrery_persistd::gateway::SessionTokenV1Authorizer;
use orrery_persistd::intent::{AttestationEnforcement, BaselineIntentValidator};
use orrery_persistd::journal::{
    spawn_chain, spawn_chain_grpc, AdaptiveCommitMode, ChainConfig, GroupCommitConfig, Journal,
};
use orrery_persistd::{
    ActivationOutcome, AuthorityMetrics, CellRuntime, CheckpointScheduler,
    CoordinatorHandoutAuthority, DurableChainId, FenceFreshnessConfig, FenceFreshnessMonitor,
    FenceRow, FenceStore, GatewayConfig, GatewayMetrics, GatewayServer, GatewayServerLatency,
    GatewayServerLatencySnapshot, GrpcChainTransport, IntentExecutor, JournalConfig,
    MemCheckpointStore, RuntimeConfig, ShardActivation,
};
#[cfg(feature = "fdb")]
use orrery_persistd::{FdbContext, FdbIntentExecutor};
use orrery_protocol::metrics::{
    SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER, SERIES_GATEWAY_BULK_SERVER,
    SERIES_GATEWAY_INTENT_SERVER, SERIES_JOURNAL_COMMIT,
};
use orrery_protocol::{CellId, Epoch, GridId, IssuerKey, IssuerKeyId, NodeId};

type SharedExecutor = Arc<dyn IntentExecutor>;

/// Command-line configuration for the `persistd` binary.
#[derive(Debug, Parser)]
#[command(name = "persistd", about = "Orrery persistence cluster (P2)")]
struct Cli {
    /// Stable process/node identity. Accepted in single-node mode so a process
    /// can keep the same runtime identity across restarts.
    #[arg(long)]
    node_id: Option<u64>,

    /// Base directory for node journals. Node `i` uses `{dir}/node-{i}`.
    #[arg(long, default_value = "persistd-data")]
    dir: PathBuf,

    /// Local chain listen address for clustered topology.
    #[arg(long)]
    chain_listen: Option<SocketAddr>,

    /// Stable primary process identity for a follower. Used with
    /// `--chain-listen`; the follower never accepts gateway writes.
    #[arg(long)]
    chain_primary: Option<u64>,

    /// Fencing epoch for this static chain assignment. Required in clustered
    /// mode so a chain never silently resumes under a different ownership
    /// epoch.
    #[arg(long)]
    chain_epoch: Option<u64>,

    /// Promote this passive follower after the named primary has failed.  The
    /// FDB fence is advanced from that owner before recovery or gateway
    /// readiness; this flag is deliberately not a lease override.
    #[arg(long, value_name = "NODE_ID")]
    promote_from: Option<u64>,

    /// A follower node and its listen address in `<node-id>@<addr>` form.
    /// Repeated to describe the follower chain order.
    #[arg(long, value_name = "NODE_ID@ADDR")]
    chain_follower: Vec<ChainFollower>,

    /// The gateway bind address.
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,

    /// FoundationDB cluster file path. When set, the binary connects to FDB for
    /// checkpoint storage, fencing, and intent execution; without it the
    /// in-memory stores are used.
    #[arg(long)]
    fdb_cluster_file: Option<PathBuf>,

    /// Permit the volatile in-memory lease store for local development and
    /// tests. Production authority requires `--fdb-cluster-file` instead.
    #[arg(long)]
    allow_volatile_leases: bool,

    /// Trusted identity issuer key in `<key-id>@<public-key>` form.
    #[arg(long, value_name = "KEY_ID@PUBLIC_KEY")]
    issuer_key: Vec<IssuerKeySpec>,

    /// Development affordance: spawn `<count>@<cell>` placeholder entities at
    /// startup so a harness has something to claim.
    ///
    /// Authority requires an entity to already have a committed cell before it
    /// can be claimed (a claim names a location the registrar can check), and
    /// spawning is a server-side or intent concern — a client cannot bootstrap
    /// one. Offline seeding via `orrery-seed` is the real path; this exists so
    /// the authority harness can run without a seeded world, and it refuses
    /// to run on a node holding durable leases.
    #[arg(long, value_name = "COUNT@CELL")]
    dev_seed: Option<DevSeedSpec>,

    /// Trusted coordinator interest-grant key in `<key-id>@<public-key>` form.
    ///
    /// Repeatable, so a key rotation can be deployed with an overlap. Without
    /// at least one, no peer has coordinator-confirmed interest: weak claims
    /// are refused and a lost lease parks instead of moving to a successor.
    #[arg(long, value_name = "KEY_ID@PUBLIC_KEY")]
    coordinator_key: Vec<IssuerKeySpec>,

    /// D27's K-of-N attestation quorum: `off`, `shadow` or `required`.
    ///
    /// [D32](../../../docs/adr/0032-enforcement-ramp.md) clause (c)'s control
    /// C1, and the startup half of its lever. `off` is the default and
    /// preserves this binary's landed behaviour exactly: the quorum is not
    /// evaluated, no witness-epoch cache is configured, and no `epoch/` or
    /// `attest/` row is written. `shadow` evaluates the full predicate,
    /// records the verdict it would have returned on the
    /// `orrery::ramp::shadow` tracing target, and refuses nothing on it.
    /// `required` refuses.
    ///
    /// **The default is `off` because D32's inventory sets it there**, and the
    /// reason is the same one #147 gave its switch: landing a flag must change
    /// nothing until an operator acts. Promotion to `shadow` is an operator
    /// decision with clause (e)'s evidence behind it, not a build's.
    ///
    /// Anything but `off` needs `--coordinator-key`: the quorum judges an
    /// intent against a coordinator-signed announcement, and a gateway that
    /// trusts no coordinator key caches none. Startup refuses the combination
    /// rather than running a mode that can never evaluate anything.
    #[arg(long, value_name = "off|shadow|required", default_value = "off")]
    attestation_enforcement: AttestationEnforcement,

    /// Hex-encoded iroh secret key, pinning the gateway's NodeId across runs.
    /// When absent a fresh identity is generated per boot.
    #[arg(long)]
    secret_key: Option<String>,

    /// Local shard specification. Accepts raw `CellId` bits (`0x...` or
    /// decimal) or coordinate form `x,y,z@level`.
    #[arg(long, value_name = "RAW|X,Y,Z@LEVEL")]
    shard: Vec<ShardSpec>,

    /// A shard this node may **adopt** from a live sibling, but does not
    /// activate at startup (D26 rule 3 step 7).
    ///
    /// The successor half of a live shard handover. The outgoing owner drains
    /// the shard and CASes `actor/{grid}/{shard}` to name this node; this node
    /// polls the rows named here and, on seeing one that names it `Active`,
    /// loads the `PreHandover` checkpoint and opens the actor. It never CASes
    /// the row itself, which is what keeps a handover a single-writer
    /// transition with no gateway-to-gateway transport in it.
    ///
    /// Deliberately a *separate* flag from `--shard` rather than a mode of it:
    /// `--shard` is what this process activates and refuses to start without,
    /// and a standby shard is neither. Standby shards are validated against
    /// `--shard` for overlap exactly as `--shard` values are against each
    /// other, because "`--shard` sets across siblings must not overlap" (D26)
    /// is worth catching within one process too.
    #[arg(long, value_name = "RAW|X,Y,Z@LEVEL")]
    standby_shard: Vec<ShardSpec>,

    /// Path to a JSON file this node watches for a shard-handover request.
    ///
    /// **This is the invocation surface, not a policy.** Who decides to move a
    /// shard, and when, is placement/ops policy and explicitly outside D26 and
    /// issue #119; what this provides is a way for an operator (or a harness)
    /// to say "hand shard S to node N" to a running gateway that has no admin
    /// wire surface of its own. The file is read, renamed aside, and the
    /// outcome written to `<path>.result`:
    ///
    /// ```json
    /// {"grid": 0, "shard": 4611686018427387905,
    ///  "successor_node": 2, "successor_gateway": "<hex node id>"}
    /// ```
    ///
    /// `successor_gateway` is optional and is what lets the refusals this node
    /// then issues be *redirects* — see `DenyReason::WrongOwner`.
    #[arg(long, value_name = "PATH")]
    handover_request: Option<PathBuf>,

    /// Retain every journal segment: never release the ones the checkpoints
    /// have made redundant (D20).
    ///
    /// The D16 `journal_retention` parameter, as a switch. Retention is on by
    /// default and this turns it off, which is what a bisect wants — a run
    /// with and without it on one binary, one host and one build — and what an
    /// operator wants when a journal has to be preserved for forensics. It is
    /// not a tuning knob: with retention off, journal disk and the index
    /// rebuilt from it at every open grow with the node's uptime.
    #[arg(long)]
    no_journal_retention: bool,

    /// Checkpoint cadence in milliseconds, overriding D16's 20 s (jitter is
    /// held at a quarter of it, as the default pair is).
    ///
    /// A harness lever, not a tuning knob. Whether a 30-second gate run
    /// releases anything at all is otherwise an accident of where 128 shards'
    /// jittered 20 s timers happen to fall — which is exactly why D20 could
    /// only report that retention was harmless where it happened to fire. A
    /// gate that has to *prove* retention sets a cadence it can outlast.
    #[arg(long, value_name = "MS")]
    checkpoint_interval_ms: Option<u64>,

    /// Append server-internal latency batches and gateway counter records to
    /// this JSONL file.
    ///
    /// A chain follower runs this reporter too, and the record set is the
    /// difference between the two roles rather than the reporter's presence: a
    /// follower has a journal, so it emits `journal_commit_ms` batches for its
    /// own mirrored appends, and has no gateway, so its gateway counters never
    /// move. Its `journal_commit_ms` is the follower's local commit latency,
    /// not the primary's — do not merge the two files and read one p99 off
    /// the result.
    #[arg(long, value_name = "PATH")]
    metrics_jsonl: Option<PathBuf>,
}

/// The P2 artifact needs a compact, server-internal view of group-commit
/// latency. A one-second export interval keeps file traffic negligible while
/// preserving the fixed-memory histogram's buckets and counts.
const METRICS_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Owns the primary-only JSONL reporter and guarantees its final delta is
/// flushed before the journal closes during a graceful shutdown.
struct MetricsReporter {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl MetricsReporter {
    async fn shutdown(self) {
        // A completed reporter has dropped its receiver; that is already
        // logged by the task and there is nothing more to signal.
        let _ = self.shutdown.send(());
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "journal metrics reporter task failed");
        }
    }
}

/// Start a compact JSONL reporter for one node's local journal and gateway.
///
/// Both roles run it. A follower passes a gateway metrics handle nothing ever
/// increments, so its file carries journal batches and no gateway records —
/// which is the honest shape, because a follower does have a journal whose
/// commit latency is worth seeing and does not have a gateway.
fn spawn_metrics_reporter(
    path: PathBuf,
    journal: Arc<Journal>,
    gateway_metrics: Arc<GatewayMetrics>,
    authority_metrics: Arc<AuthorityMetrics>,
) -> anyhow::Result<MetricsReporter> {
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let (shutdown, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let metrics = journal.commit_metrics();
        let retention_journal = Arc::clone(&journal);
        let mut retention_cursor = orrery_persistd::journal::JournalRetention::default();
        let mut cursor = metrics.snapshot();
        let mut stage_cursor = metrics.stage_snapshot();
        let mut bulk_cursor = gateway_metrics.bulk.snapshot();
        let mut bulk_latency_cursor = gateway_metrics.bulk.latency_snapshot();
        let mut intent_latency_cursor = gateway_metrics.intent.latency().snapshot();
        let mut area_latency_cursor = gateway_metrics.area.latency().snapshot();
        let mut intent_cursor = orrery_persistd::GatewayIntentSnapshot::default();
        let mut lease_stage_cursor = orrery_persistd::LeaseStageSnapshot::default();
        let mut area_cursor = orrery_persistd::GatewayAreaSnapshot::default();
        let mut report_cursor = orrery_persistd::GatewayReportSnapshot::default();
        let mut authority_cursor = orrery_persistd::AuthoritySnapshot::default();
        let mut interval = tokio::time::interval(METRICS_REPORT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut writer = BufWriter::new(file);

        // One drain path, called from both the interval tick and the final
        // flush. Two copies of this chain is how a record ends up emitted
        // every second and lost on shutdown, or the reverse.
        let mut drain = move |writer: &mut BufWriter<std::fs::File>| -> std::io::Result<()> {
            write_journal_metric_batches(&mut *writer, &metrics.delta(&mut cursor))?;
            write_journal_stage_delta(&mut *writer, metrics.stage_delta(&mut stage_cursor))?;
            write_gateway_bulk_latency(&mut *writer, &gateway_metrics, &mut bulk_latency_cursor)?;
            write_gateway_server_latency(
                &mut *writer,
                SERIES_GATEWAY_INTENT_SERVER,
                gateway_metrics.intent.latency(),
                &mut intent_latency_cursor,
            )?;
            write_gateway_server_latency(
                &mut *writer,
                SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER,
                gateway_metrics.area.latency(),
                &mut area_latency_cursor,
            )?;
            write_gateway_bulk_delta(&mut *writer, &gateway_metrics, &mut bulk_cursor)?;
            write_gateway_intent(&mut *writer, &gateway_metrics, &mut intent_cursor)?;
            write_gateway_lease_stage(&mut *writer, &mut lease_stage_cursor)?;
            write_gateway_area(&mut *writer, &gateway_metrics, &mut area_cursor)?;
            write_gateway_report(&mut *writer, &gateway_metrics, &mut report_cursor)?;
            write_gateway_authority(&mut *writer, &authority_metrics, &mut authority_cursor)?;
            write_journal_retention(&mut *writer, &retention_journal, &mut retention_cursor)?;
            writer.flush()
        };

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = &mut stopped => {
                    if let Err(error) = drain(&mut writer) {
                        tracing::warn!(path = %path.display(), %error, "failed to flush final journal metrics");
                    }
                    break;
                }
            }

            if let Err(error) = drain(&mut writer) {
                tracing::warn!(path = %path.display(), %error, "failed to write journal metrics; stopping reporter");
                break;
            }
        }
    });
    Ok(MetricsReporter { shutdown, task })
}

/// Append the journal's retention state whenever it changes (D20, D23).
///
/// Absolute totals rather than interval deltas, and the floor rather than a
/// count of bytes: what a reader needs to know is whether this journal is
/// bounded *now*, and — when it is not — which precondition is the binding
/// one. A gauge that reset every second would hide a floor that stopped
/// moving an interval ago, which is the shape the failure actually has.
///
/// Both roles emit it. A follower's mirror is bounded by the floor its primary
/// reports and a primary's journal by its own checkpoints, so the record means
/// something different on each side and is the same three numbers on both.
fn write_journal_retention(
    mut writer: impl Write,
    journal: &Journal,
    cursor: &mut orrery_persistd::journal::JournalRetention,
) -> std::io::Result<()> {
    let snapshot = journal.retention();
    if snapshot == *cursor {
        return Ok(());
    }
    *cursor = snapshot;
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "journal_retention",
            "floor": snapshot.floor.to_string(),
            "releases": snapshot.releases,
            "records_dropped": snapshot.records_dropped,
            "blocked": snapshot.blocked.map(|reason| reason.to_string()),
        }),
    )?;
    writeln!(writer)
}

/// Append the authority counters whenever any of them moved.
///
/// All of them, and the count has grown three times for the same reason. First
/// `divest_requested` and `handoff_timed_out`: the second is the zombie-host
/// symptom docs/09-services-and-ops.md §10 builds a runbook around — a holder
/// that answers no divest request — and a runbook whose symptom has no counter
/// is a paragraph, not a procedure. Then `misrouted_diffs`, `unindexed_diffs`
/// and `misroute_throttled`, which docs/08-persistence.md §2.1.2 names as the
/// alarm on the fenced route's expensive branch while they lived only in
/// process memory — the same failure one layer out. Then D26's handover
/// counters, which are an *invariant* rather than an alarm: I2 is stated as a
/// subtraction between two of them, so exporting one without the other would
/// export half a statement.
///
/// These are absolute totals, not interval deltas: `duplicate_authority` is an
/// invariant that must read zero for the life of the process, and a gauge that
/// resets every interval would hide a violation that happened one tick ago.
fn write_gateway_authority(
    mut writer: impl Write,
    metrics: &AuthorityMetrics,
    cursor: &mut orrery_persistd::AuthoritySnapshot,
) -> std::io::Result<()> {
    let snapshot = metrics.snapshot();
    if snapshot == *cursor {
        return Ok(());
    }
    *cursor = snapshot;
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "gateway_authority",
            "duplicate_authority": snapshot.duplicate_authority,
            // docs/08-persistence.md §2.1.2 names these three as the alarm for
            // a peer steering diffs onto the fenced route's expensive branch.
            // They were counted in-process and never scraped, so the alarm
            // existed only in a `snapshot()` nobody read; a documented alarm
            // that is not exported is a paragraph, like the two below it.
            "misrouted_diffs": snapshot.misrouted_diffs,
            "unindexed_diffs": snapshot.unindexed_diffs,
            "misroute_throttled": snapshot.misroute_throttled,
            "reassigned": snapshot.reassigned,
            "parked_without_successor": snapshot.parked_without_successor,
            "divested": snapshot.divested,
            "divest_rejected": snapshot.divest_rejected,
            "divest_requested": snapshot.divest_requested,
            "handoff_timed_out": snapshot.handoff_timed_out,
            // D25's `Expire` fan-out, exported for the same reason the three
            // route counters above are: a bound nobody can read is a
            // paragraph. `sent` against `parked_without_successor` gives the
            // realised `|A|` per parked lease — the distribution D25's open
            // question asks to be measured — and `dropped` distinguishes a
            // cell past D6's population ceiling from one peer's egress bucket
            // doing its job.
            "expire_fanout_sent": snapshot.expire_fanout_sent,
            "expire_fanout_skipped": snapshot.expire_fanout_skipped,
            "expire_fanout_dropped": snapshot.expire_fanout_dropped,
            // D26 invariant I2, as two zero-valued counters an operator (and
            // the sibling harness) can subtract without joining anything:
            // `leases_live_at_drain_start - expires_delivered_before_cas == 0`
            // and `heartbeats_rejected_wrong_owner == 0` across a handover
            // window. The second is the one that fails on the exact defect
            // D26's Context describes — a holder still renewing at a process
            // that no longer hosts its shard — which is why it is exported
            // from every node whether or not that node ever hands a shard
            // anywhere.
            "handover_leases_live_at_drain_start": snapshot.handover_leases_live_at_drain_start,
            "handover_expires_delivered_before_cas": snapshot.handover_expires_delivered_before_cas,
            "heartbeats_rejected_wrong_owner": snapshot.heartbeats_rejected_wrong_owner,
            // A handover park is the design; a crash park with no successor is
            // a capability loss. They are the same durable transition, so
            // without this counter `parked_without_successor` would alarm on
            // every planned shard move.
            "handover_parks": snapshot.handover_parks,
            "claims_denied_draining": snapshot.claims_denied_draining,
            "handovers_completed": snapshot.handovers_completed,
            "handovers_aborted": snapshot.handovers_aborted,
            // A session the registrar gave up on because it went silent, not
            // because its connection ended. It is the counter that says a peer
            // left without saying so — and, since #157, the one that says the
            // registrar stopped offering that peer other peers' leases.
            "stale_sessions_reaped": snapshot.stale_sessions_reaped,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Export the server receipt-through-send-call bulk histogram in dashboard
/// form.
fn write_gateway_bulk_latency(
    mut writer: impl Write,
    metrics: &GatewayMetrics,
    cursor: &mut orrery_persistd::GatewayBulkLatencySnapshot,
) -> std::io::Result<()> {
    for sample in metrics.bulk.latency_delta(cursor) {
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "type": "sample_batch",
                "series": SERIES_GATEWAY_BULK_SERVER,
                "value_us": sample.value_us,
                "count": sample.count,
            }),
        )
        .map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

/// Export one server-side span histogram under its own ungated series name.
///
/// The name is a parameter and the two callers pass
/// `gateway_intent_server_ms` and `gateway_area_first_page_server_ms` — never
/// `intent_commit_ms` or `area_first_page_ms`. Those are client round trips
/// gated by D16, and `p2-dashboard` folds by series name into one histogram
/// with no source field: a server span written under a gated name would lower
/// that p99 and pass a gate it never measured.
fn write_gateway_server_latency(
    mut writer: impl Write,
    series: &str,
    latency: &GatewayServerLatency,
    cursor: &mut GatewayServerLatencySnapshot,
) -> std::io::Result<()> {
    for sample in latency.delta(cursor) {
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "type": "sample_batch",
                "series": series,
                "value_us": sample.value_us,
                "count": sample.count,
            }),
        )
        .map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

/// Append the intent-path counters whenever any of them moved.
fn write_gateway_intent(
    mut writer: impl Write,
    metrics: &GatewayMetrics,
    cursor: &mut orrery_persistd::GatewayIntentSnapshot,
) -> std::io::Result<()> {
    let snapshot = metrics.intent.snapshot();
    if snapshot == *cursor {
        return Ok(());
    }
    *cursor = snapshot;
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "gateway_intent",
            "replies": snapshot.replies,
            "committed": snapshot.committed,
            "rejected": snapshot.rejected,
            "rejected_no_executor": snapshot.rejected_no_executor,
            "lane_saturated": snapshot.lane_saturated,
            // D29's low-population path. Every name carries the
            // `intent_provisional_` sense the record requires, and none of
            // them may be summed with the bulk-ack `provisional` counter in
            // the `gateway_bulk` record: the two mechanisms have contrary
            // remediations, and an averaged panel would say nothing about
            // either.
            "intent_provisional_committed": snapshot.provisional_committed,
            "intent_provisional_refused_cap": snapshot.provisional_refused_cap,
            "intent_provisional_refused_input": snapshot.provisional_refused_input,
            "intent_provisional_finalized": snapshot.provisional_finalized,
            "intent_provisional_annulled": snapshot.provisional_annulled,
            "intent_provisional_expired": snapshot.provisional_expired,
            "intent_annulled_replays": snapshot.annulled_replays,
            "server_us_sum": snapshot.server_us_sum,
            "server_us_max": snapshot.server_us_max,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Append the renewal stage decomposition whenever it moved.
///
/// Its own record kind rather than a field on `gateway_lease`: these are stage
/// *times*, and folding them in beside counters is how a reader ends up
/// dividing a per-message sum by a per-lease count. Both denominators travel
/// with the record for exactly that reason — see `lease::stages`.
fn write_gateway_lease_stage(
    mut writer: impl Write,
    cursor: &mut orrery_persistd::LeaseStageSnapshot,
) -> std::io::Result<()> {
    let snapshot = orrery_persistd::lease_stage_metrics().snapshot();
    if snapshot == *cursor {
        return Ok(());
    }
    let d = snapshot.delta(cursor);
    *cursor = snapshot;
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "gateway_lease_stage",
            "heartbeats": d.heartbeats,
            "renewals": d.renewals,
            "entries_max": d.entries_max,
            "session_us_sum": d.session_us_sum,
            "session_us_max": d.session_us_max,
            "resolve_us_sum": d.resolve_us_sum,
            "resolve_us_max": d.resolve_us_max,
            "route_us_sum": d.route_us_sum,
            "route_us_max": d.route_us_max,
            "recheck_us_sum": d.recheck_us_sum,
            "recheck_us_max": d.recheck_us_max,
            "encode_us_sum": d.encode_us_sum,
            "encode_us_max": d.encode_us_max,
            "heartbeat_us_sum": d.heartbeat_us_sum,
            "heartbeat_us_max": d.heartbeat_us_max,
            "gap_us_sum": d.gap_us_sum,
            "gap_us_max": d.gap_us_max,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Append the area-load counters whenever any of them moved.
fn write_gateway_area(
    mut writer: impl Write,
    metrics: &GatewayMetrics,
    cursor: &mut orrery_persistd::GatewayAreaSnapshot,
) -> std::io::Result<()> {
    let snapshot = metrics.area.snapshot();
    if snapshot == *cursor {
        return Ok(());
    }
    *cursor = snapshot;
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "gateway_area",
            "subscribes": snapshot.subscribes,
            "first_pages": snapshot.first_pages,
            "frames": snapshot.frames,
            "cell_read_errors": snapshot.cell_read_errors,
            "first_page_us_sum": snapshot.first_page_us_sum,
            "first_page_us_max": snapshot.first_page_us_max,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Append the discrepancy-report outcome and refusal split whenever it moved.
///
/// Absolute totals, like the authority record and for the same reason: a
/// refusal that happened one interval ago is the thing an operator is looking
/// for, and an interval gauge that resets would hide it.
fn write_gateway_report(
    mut writer: impl Write,
    metrics: &GatewayMetrics,
    cursor: &mut orrery_persistd::GatewayReportSnapshot,
) -> std::io::Result<()> {
    let snapshot = metrics.report.snapshot();
    if snapshot == *cursor {
        return Ok(());
    }
    *cursor = snapshot;
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "gateway_report",
            "verdicts": snapshot.verdicts,
            "adjudicated": snapshot.adjudicated,
            "confirms": snapshot.confirms,
            "exonerates": snapshot.exonerates,
            "evidence_forged": snapshot.evidence_forged,
            "unadjudicable": snapshot.unadjudicable,
            "refused_no_adjudicator": snapshot.refused_no_adjudicator,
            "refused_rate_limited": snapshot.refused_rate_limited,
            "refused_reporter_mismatch": snapshot.refused_reporter_mismatch,
            "refused_no_session": snapshot.refused_no_session,
            "refused_other": snapshot.refused_other,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Append one compact interval aggregate for gateway stages outside Fjall.
fn write_gateway_bulk_delta(
    mut writer: impl Write,
    metrics: &GatewayMetrics,
    cursor: &mut orrery_persistd::GatewayBulkSnapshot,
) -> std::io::Result<()> {
    let delta = metrics.bulk.delta(cursor);
    if delta.acknowledgements == 0 {
        return Ok(());
    }
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "gateway_bulk_stage_delta",
            "acknowledgements": delta.acknowledgements,
            "route_queue_us_sum": delta.route_queue_us_sum,
            "route_queue_us_max": delta.route_queue_us_max,
            "router_apply_us_sum": delta.router_apply_us_sum,
            "router_apply_us_max": delta.router_apply_us_max,
            "journal_wait_us_sum": delta.journal_wait_us_sum,
            "journal_wait_us_max": delta.journal_wait_us_max,
            "reply_us_sum": delta.reply_us_sum,
            "reply_us_max": delta.reply_us_max,
            "total_us_sum": delta.total_us_sum,
            "total_us_max": delta.total_us_max,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Append one compact interval aggregate for group-commit stage diagnostics.
fn write_journal_stage_delta(
    mut writer: impl Write,
    delta: orrery_persistd::JournalStageSnapshot,
) -> std::io::Result<()> {
    if delta.flushes == 0 {
        return Ok(());
    }
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type": "journal_stage_delta",
            "flushes": delta.flushes,
            "records": delta.records,
            "bytes": delta.bytes,
            "queue_wait_us_sum": delta.queue_wait_us_sum,
            "queue_wait_us_max": delta.queue_wait_us_max,
            "blocking_dispatch_us_sum": delta.blocking_dispatch_us_sum,
            "blocking_dispatch_us_max": delta.blocking_dispatch_us_max,
            "fjall_batch_commit_us_sum": delta.fjall_batch_commit_us_sum,
            "fjall_batch_commit_us_max": delta.fjall_batch_commit_us_max,
            "sync_data_us_sum": delta.sync_data_us_sum,
            "sync_data_us_max": delta.sync_data_us_max,
            "resolve_us_sum": delta.resolve_us_sum,
            "resolve_us_max": delta.resolve_us_max,
            "sync_data_us_max_bytes": delta.sync_data_us_max_bytes,
            "sync_data_us_max_records": delta.sync_data_us_max_records,
            "slow_syncs": delta.slow_syncs,
            "slow_sync_bytes_sum": delta.slow_sync_bytes_sum,
            "slow_sync_records_sum": delta.slow_sync_records_sum,
        }),
    )
    .map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

/// Append P2-dashboard-compatible records for one telemetry delta.
fn write_journal_metric_batches(
    mut writer: impl Write,
    samples: &[orrery_persistd::JournalCommitSample],
) -> std::io::Result<()> {
    for sample in samples {
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "type": "sample_batch",
                "series": SERIES_JOURNAL_COMMIT,
                "value_us": sample.value_us,
                "count": sample.count,
            }),
        )
        .map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

/// Tracing filter used when `RUST_LOG` is unset or unparseable.
///
/// `tracing_subscriber::fmt::Subscriber::DEFAULT_MAX_LEVEL` is INFO, which is
/// what this binary emitted for its whole life before it had a filter at all,
/// so an operator who sets nothing sees exactly the same stderr as before.
const DEFAULT_LOG_FILTER: &str = "info";

/// Build the stderr tracing filter from a `RUST_LOG` value.
///
/// The builder form `tracing_subscriber::fmt()` installs no `EnvFilter` on
/// `init()` — only the free function `fmt::init()` does — so before this
/// existed `persistd` ignored `RUST_LOG` outright and no `debug!` in the
/// gateway could be turned on in a harness run. A malformed value falls back
/// to [`DEFAULT_LOG_FILTER`] rather than aborting startup: a mistyped
/// environment variable must not take a persistence authority down.
fn log_filter(rust_log: Option<&str>) -> EnvFilter {
    rust_log
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_LOG_FILTER))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // tracing to stderr (D12) so stdout is reserved for the JSON address line.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(log_filter(
            std::env::var(EnvFilter::DEFAULT_ENV).ok().as_deref(),
        ))
        .init();

    let topology = resolve_topology(&cli)?;
    // Standby shards are validated against the served set for overlap, which
    // is the same property `validate_shards` holds within `--shard` and the
    // one D26 makes an operator-checkable deployment property across
    // siblings. A standby shard that overlapped a served one would make
    // `adopt_shard` ambiguous at exactly the moment it mattered.
    let standby_shards = resolve_standby_shards(&cli, &topology)?;
    let startup_started = Instant::now();

    if matches!(topology.role, TopologyRole::Follower { .. }) {
        return run_follower(&cli, &topology).await;
    }
    if matches!(topology.role, TopologyRole::Promotion { .. }) && cli.fdb_cluster_file.is_none() {
        anyhow::bail!("--promote-from requires --fdb-cluster-file for durable fencing");
    }
    if cli.fdb_cluster_file.is_none() && !cli.allow_volatile_leases {
        anyhow::bail!(
            "authority requires --fdb-cluster-file; use --allow-volatile-leases only for local development or tests"
        );
    }
    if cli.issuer_key.is_empty() {
        anyhow::bail!("authority requires at least one --issuer-key <key-id>@<public-key>");
    }

    // ── FoundationDB stores (optional) ──────────────────────────────────
    // The FDB client network can boot only once per process. Keep one context
    // and pass its database handle to fence, checkpoint, and intent adapters.
    #[cfg(feature = "fdb")]
    let fdb_context = cli
        .fdb_cluster_file
        .as_ref()
        .map(|path| FdbContext::connect(&path.display().to_string()))
        .transpose()?;

    // The fence store gates the single runtime against the same durable rows.
    // When no FDB cluster file is given we fall back to in-memory stores
    // (MemFenceStore, MemCheckpointStore) which are not durable across process
    // death.
    let fence_store: Arc<dyn FenceStore> = if let Some(ref cluster_file) = cli.fdb_cluster_file {
        #[cfg(feature = "fdb")]
        {
            let _ = cluster_file;
            let context = fdb_context
                .as_ref()
                .expect("FDB context exists when --fdb-cluster-file is set");
            Arc::new(orrery_persistd::fence::FdbFenceStore::from_context(context))
        }
        #[cfg(not(feature = "fdb"))]
        {
            let _ = cluster_file;
            anyhow::bail!(
                "persistd was compiled without the `fdb` feature; \
                 --fdb-cluster-file requires libfdb_c"
            );
        }
    } else {
        Arc::new(orrery_persistd::fence::MemFenceStore::new())
    };

    // Checkpoint store and cold-cell reader. With FDB both are the same
    // FdbCheckpointStore; without FDB we use MemCheckpointStore for checkpoints
    // and no cold fallback.
    let checkpoint_store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore>;
    // cold_store is only constructed under the fdb feature.
    #[allow(unused_assignments, unused_mut)]
    let mut cold_store: Option<Arc<dyn orrery_persistd::checkpoint::ColdCellReader>> = None;

    if let Some(ref cluster_file) = cli.fdb_cluster_file {
        #[cfg(feature = "fdb")]
        {
            let _ = cluster_file;
            let context = fdb_context
                .as_ref()
                .expect("FDB context exists when --fdb-cluster-file is set");
            let store = orrery_persistd::checkpoint::FdbCheckpointStore::from_context(context);
            let store = Arc::new(store);
            let cold: Arc<dyn orrery_persistd::checkpoint::ColdCellReader> =
                Arc::clone(&store) as Arc<dyn orrery_persistd::checkpoint::ColdCellReader>;
            checkpoint_store = store as Arc<dyn orrery_persistd::checkpoint::CheckpointStore>;
            cold_store = Some(cold);
        }
        #[cfg(not(feature = "fdb"))]
        {
            let _ = cluster_file;
            anyhow::bail!(
                "persistd was compiled without the `fdb` feature; \
                 --fdb-cluster-file requires libfdb_c"
            );
        }
    } else {
        checkpoint_store = Arc::new(MemCheckpointStore::new());
    }

    // ── Journal / runtime config ────────────────────────────────────────
    let mut schedulers: Vec<CheckpointScheduler> = Vec::new();
    let node_dir = cli.dir.join(format!("node-{}", topology.node_id));
    std::fs::create_dir_all(&node_dir)?;

    // Fencing is the startup admission control.  No actor recovery, scheduler,
    // or gateway is created until the durable ownership transition commits.
    // In particular a former primary cannot reclaim a shard after its follower
    // has promoted: ordinary startup accepts only an absent row or our own row.
    tracing::info!(
        role = topology.role.name(),
        "persistd startup: acquiring durable activation"
    );
    let activation = activate_topology(
        &topology,
        fence_store.as_ref(),
        cli.fdb_cluster_file.is_some(),
    )
    .await?;
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis(),
        ownership_epoch = activation.epoch.0,
        "persistd startup: durable activation acquired"
    );

    // A promoted follower first makes its mirrored, provenance-checked prefix
    // locally replayable.  This happens after its fence CAS and before the
    // runtime opens, so readiness means recovery includes the promoted tail.
    let recovery_cutoff = if let Some(source_epoch) = activation.promoted_source_epoch {
        let adoption_started = Instant::now();
        tracing::info!(
            source_epoch,
            elapsed_ms = startup_started.elapsed().as_millis(),
            "persistd promotion recovery: opening mirrored journal"
        );
        let journal = Journal::open(&JournalConfig {
            dir: node_dir.clone(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                batch_window: Duration::from_micros(200),
                ..GroupCommitConfig::default()
            },
        })?;
        tracing::info!(
            source_epoch,
            elapsed_ms = startup_started.elapsed().as_millis(),
            "persistd promotion recovery: validating and indexing mirrored chain history"
        );
        let history = journal
            .adopt_chain_history(
                topology
                    .chain_id_at(source_epoch)
                    .expect("promotion has a source chain identity"),
            )
            .map_err(|error| anyhow::anyhow!("promoted chain-history adoption failed: {error}"))?;
        let cutoff = history
            .watermark()
            .map(|lsn| format!("{}:{}", lsn.segment, lsn.offset));
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis(),
            adoption_elapsed_ms = adoption_started.elapsed().as_millis(),
            recovery_cutoff = ?cutoff,
            "persistd promotion recovery: mirrored chain history adopted"
        );
        journal.close().await?;
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis(),
            "persistd promotion recovery: mirrored journal closed; opening runtime"
        );
        cutoff
    } else {
        None
    };

    let config = RuntimeConfig {
        shards: topology.shards.clone(),
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: node_dir,
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                batch_window: Duration::from_micros(200),
                ..GroupCommitConfig::default()
            },
        },
        node_id: topology.node_id,
        epoch: activation.epoch,
        fence: Arc::clone(&fence_store),
    };
    // Recovery seeds actors from the same durable tier the checkpoint
    // scheduler writes: the checkpoint is the base, the journal the
    // delta (§3.4).
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis(),
        "persistd startup: recovering runtime"
    );
    let lease_store: Arc<dyn orrery_persistd::LeaseStore> = if cli.fdb_cluster_file.is_some() {
        #[cfg(feature = "fdb")]
        {
            Arc::new(orrery_persistd::FdbLeaseStore::from_context(
                fdb_context.as_ref().expect("FDB context exists"),
            ))
        }
        #[cfg(not(feature = "fdb"))]
        unreachable!("FDB flag was rejected before startup")
    } else {
        Arc::new(orrery_persistd::MemLeaseStore::new())
    };
    let runtime = Arc::new(
        CellRuntime::open_with_lease_store(&config, &checkpoint_store, lease_store).await?,
    );
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis(),
        "persistd startup: runtime recovered"
    );
    let runtime_for_shutdown = Arc::clone(&runtime);

    // FDB-backed active nodes must continuously revalidate the rows acquired
    // above.  Start this after recovery but before exposing a gateway, so a
    // failed first poll can only yield provisional bulk acknowledgements.
    let fence_freshness_monitor = if cli.fdb_cluster_file.is_some() {
        Some(
            runtime
                .start_fence_freshness_monitor(FenceFreshnessConfig::default())
                .await
                .map_err(|error| anyhow::anyhow!("start fence freshness monitor: {error}"))?,
        )
    } else {
        None
    };

    // This executes only on an active node: passive followers returned above
    // before opening a runtime. Capture the journal before handing the runtime
    // to the gateway/router so reporter lifetime is independent of routing.
    // Server-side telemetry is always collected, on the authority-metrics
    // precedent: the flag opens a sink, it does not start the measurement.
    // A counter that only exists when someone remembered a flag is a
    // debugging session scheduled in advance, not telemetry. What this does
    // not do is make them reachable — there is no scrape or admin surface on
    // this binary, so without `--metrics-jsonl` they stay in-process until
    // the D12 OTel bridge reads them.
    let gateway_metrics = Arc::new(GatewayMetrics::default());
    let authority_metrics = Arc::new(AuthorityMetrics::default());
    let journal_open_ms = runtime.journal().open_ms();
    let metrics_reporter = if let Some(path) = cli.metrics_jsonl.clone() {
        let journal = Arc::clone(runtime.journal());
        Some(spawn_metrics_reporter(
            path,
            journal,
            Arc::clone(&gateway_metrics),
            Arc::clone(&authority_metrics),
        )?)
    } else {
        None
    };

    // Chain replication is intentionally downstream of the journal ack path.
    // The transport is lazy: an unavailable follower marks the chain degraded
    // but never prevents the primary from serving local durable writes.
    let chain_replicator = if matches!(topology.role, TopologyRole::Primary { .. }) {
        let chain = topology
            .chain_id()
            .expect("primary topology always has a chain identity");
        let follower = topology
            .follower()
            .expect("only primary topologies have a chain id")
            .node_id;
        let journal = Arc::clone(runtime.journal());
        let transport = Arc::new(GrpcChainTransport::new(
            topology
                .follower()
                .expect("only primary topologies have a follower")
                .addr,
            chain,
        ));
        Some(spawn_chain(
            journal,
            transport,
            &ChainConfig {
                follower,
                ..ChainConfig::default()
            },
        ))
    } else {
        None
    };

    // Spawn one checkpoint scheduler for the single runtime, using the
    // default 20 s jittered cadence (D16). The same scheduler drives journal
    // retention (D20): its checkpoints are what establish the release floor.
    let scheduler = orrery_persistd::spawn_checkpoint_scheduler_direct(
        Arc::clone(&runtime),
        Arc::clone(&checkpoint_store),
        &{
            let mut config = orrery_persistd::checkpoint::CheckpointConfig {
                retention: !cli.no_journal_retention,
                ..orrery_persistd::checkpoint::CheckpointConfig::default()
            };
            if let Some(ms) = cli.checkpoint_interval_ms {
                config.interval = Duration::from_millis(ms);
                config.jitter = config.interval / 4;
            }
            config
        },
    );
    schedulers.push(scheduler);

    // Wrap the runtime in a cold-fallback router when FDB is available.
    let router: Arc<dyn Router> = if let Some(cold) = cold_store {
        Arc::new(ColdFallbackRouter::new(Arc::clone(&runtime), cold))
    } else {
        // Without FDB there is no cold fallback; the runtime itself is the Router.
        runtime.clone()
    };
    // Retained only for the dev-seed affordance below, which needs the actor
    // handles directly rather than the routing surface.
    let seed_runtime = cli.dev_seed.is_some().then(|| Arc::clone(&runtime));
    // Retained for the handover orchestrator, which needs the runtime's fence
    // CASes and its actor table, not the routing surface. Released before the
    // `Arc::try_unwrap` at shutdown by stopping that task first.
    let handover_runtime = Arc::clone(&runtime);
    drop(runtime);

    // ── Gateway ─────────────────────────────────────────────────────────
    let secret_key = cli
        .secret_key
        .as_deref()
        .map(|hex| {
            hex.parse::<iroh::SecretKey>()
                .map_err(|e| anyhow::anyhow!("invalid --secret-key (expected hex iroh key): {e}"))
        })
        .transpose()?;

    // The durable ownership this process activated, as the intent executor's
    // fence. Uniform across the shard set: `activate_topology` refuses a mixed
    // epoch, so one epoch names the whole activation.
    #[cfg(feature = "fdb")]
    let activation_shards = topology.shards.clone();
    #[cfg(feature = "fdb")]
    let activation_owner = topology.node_id;
    #[cfg(feature = "fdb")]
    let activation_epoch = activation.epoch;

    // The intent ledger's fence names the whole activated shard set, so a
    // handover has to restate it. Captured out of the constructor closure
    // because `SharedExecutor` is a `dyn` handle with no way back to the
    // concrete type.
    #[cfg(feature = "fdb")]
    let intent_executor: Arc<std::sync::Mutex<Option<Arc<FdbIntentExecutor>>>> =
        Arc::new(std::sync::Mutex::new(None));
    #[cfg(feature = "fdb")]
    let captured_intent_executor = Arc::clone(&intent_executor);
    let mut gateway_config =
        gateway_config(&cli, secret_key, |cluster_file, grid, attestation| {
            #[cfg(feature = "fdb")]
            {
                let _ = cluster_file;
                let context = fdb_context
                    .as_ref()
                    .expect("FDB context exists when --fdb-cluster-file is set");
                // The ledger is fenced by the same activation the bulk path is:
                // every intent transaction re-reads the ownership rows this
                // startup committed, so a superseded process that still holds a
                // live FDB handle cannot mint durable effects. The whole shard set
                // is named because an `IntentOp` carries no cell — see
                // `IntentFence`.
                let exec = FdbIntentExecutor::fenced_from_context(
                    context,
                    grid,
                    orrery_persistd::IntentFence {
                        shards: activation_shards.clone(),
                        owner: activation_owner,
                        epoch: activation_epoch,
                    },
                );
                // D32 clause (d)'s corollary: the executor's re-proof and the
                // `attest/` marker have to agree with the mode admission ran
                // under, so the same cache and the same resolver are wired into
                // both sides here rather than configured twice. Under `off` the
                // executor holds no authority and writes no row, which is the
                // state this binary shipped in.
                let exec = match attestation {
                    None => exec,
                    Some(wiring) => match wiring.mode {
                        AttestationEnforcement::Off => exec,
                        AttestationEnforcement::Shadow => exec.shadowing_epochs(
                            Arc::clone(&wiring.epochs),
                            Arc::clone(&wiring.bindings),
                        ),
                        AttestationEnforcement::Required => exec.recording_epochs(
                            Arc::clone(&wiring.epochs),
                            Arc::clone(&wiring.bindings),
                        ),
                    },
                };
                let exec = Arc::new(exec);
                *captured_intent_executor
                    .lock()
                    .expect("intent executor capture lock poisoned") = Some(Arc::clone(&exec));
                Ok(Some(exec as SharedExecutor))
            }
            #[cfg(not(feature = "fdb"))]
            {
                let _ = (cluster_file, grid, attestation);
                anyhow::bail!(
                    "persistd was compiled without the `fdb` feature; \
                     --fdb-cluster-file requires libfdb_c"
                );
            }
        })?;
    if let Some(monitor) = &fence_freshness_monitor {
        gateway_config.bulk_ack_admission = monitor.clone();
    }
    gateway_config.metrics = Arc::clone(&gateway_metrics);
    gateway_config.authority_metrics = Arc::clone(&authority_metrics);
    // Read before the config is consumed by `spawn`. Reported on the readiness
    // line because "can this cluster adjudicate a discrepancy report" is not
    // otherwise observable from outside the process: without an adjudicator
    // every report comes back `REPORT_REFUSED_NO_ADJUDICATOR`, which a witness
    // sees and an operator does not. It is `false` in a stock build by design
    // — registering a build means linking a `Ruleset`
    // (docs/09-services-and-ops.md §1).
    let adjudicator_configured = gateway_config.adjudicator.is_some();
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis(),
        "persistd startup: starting gateway"
    );
    let gateway = GatewayServer::spawn(gateway_config, router).await?;

    // ── Live shard handover (D26 rule 3) ────────────────────────────────
    // Started after the gateway, because the drain delivers each `Expire` on
    // a holder's own session and there are no sessions before the gateway
    // exists. A node with neither a request path nor a standby shard starts
    // no task at all.
    let handover_control = if cli.handover_request.is_some() || !standby_shards.is_empty() {
        let context = HandoverContext {
            runtime: Arc::clone(&handover_runtime),
            checkpoints: Arc::clone(&checkpoint_store),
            drain: gateway.drain_handle(),
            authority_metrics: Arc::clone(&authority_metrics),
            freshness: fence_freshness_monitor.clone(),
            #[cfg(feature = "fdb")]
            intents: intent_executor
                .lock()
                .expect("intent executor capture lock poisoned")
                .clone(),
            node_id: topology.node_id,
        };
        Some(spawn_handover_control(
            context,
            cli.handover_request.clone(),
            standby_shards.clone(),
        ))
    } else {
        None
    };

    // Development seeding, after activation and before the readiness line, so
    // a harness that sees the line can immediately claim what it names.
    let dev_seeded = if let Some(spec) = cli.dev_seed {
        if cli.fdb_cluster_file.is_some() {
            anyhow::bail!(
                "--dev-seed is a local harness affordance; seed a durable world with orrery-seed instead"
            );
        }
        let seed_runtime = seed_runtime
            .as_ref()
            .expect("dev seed runtime is retained whenever --dev-seed is set");
        let entities = dev_seed_entities(seed_runtime, spec).await?;
        tracing::warn!(
            count = entities.len(),
            cell = ?spec.cell,
            "seeded placeholder entities: --dev-seed is not a production path"
        );
        entities
    } else {
        Vec::new()
    };

    // Print the gateway address as a single-line JSON object on stdout, so a
    // demo harness can parse it. Everything else goes to stderr via tracing.
    {
        let addr = gateway.addr();
        let node_id = gateway.id();
        // `EndpointAddr` is the full client dial document; expose one direct
        // socket separately so local multi-process harnesses can construct an
        // endpoint without parsing its debug representation.
        let bind_addr = addr
            .ip_addrs()
            .next()
            .expect("gateway endpoint has its configured direct bind address");
        let json = serde_json::json!({
            "endpoint_addr": format!("{addr:?}"),
            "bind_addr": bind_addr.to_string(),
            "node_id": format!("{node_id}"),
            "cluster_node_id": topology.node_id,
            "role": topology.role.name(),
            "ownership_epoch": activation.epoch.0,
            // Which shards this node answers for, and at what fence epoch.
            // Without it, "you are talking to the wrong node" is a claim a
            // harness can receive (`DenyReason::WrongOwner`) and never check:
            // nothing outside the process said which shards were owned here.
            // Raw `CellId` bits, the same form `--shard` accepts, so the flag
            // list and this line are comparable without a decoder.
            "shards": activation
                .rows
                .iter()
                .map(|(shard, row)| {
                    serde_json::json!({
                        "cell": shard.to_bits(),
                        "epoch": row.epoch.0,
                    })
                })
                .collect::<Vec<_>>(),
            // Shards this node may adopt from a live sibling but does not own
            // (D26 rule 3 step 7). Reported for the same reason `shards` is:
            // "which shards is this process prepared to take" is otherwise
            // invisible from outside it, and a harness that has to drive a
            // handover needs both halves.
            "standby_shards": standby_shards
                .iter()
                .map(|shard| shard.to_bits())
                .collect::<Vec<_>>(),
            "recovery_cutoff": recovery_cutoff,
            // The D16 `journal_open_ms` budget's measurand, reported by the
            // node that paid it: a restart's index rebuild is linear in the
            // journal it opens, which is what retention exists to bound, and
            // an operator reading it out of a log timestamp is reading the
            // whole startup instead.
            "journal_open_ms": journal_open_ms,
            "bulk_ack_fence_monitor": fence_freshness_monitor.is_some(),
            "adjudicator": adjudicator_configured,
            "dev_seeded_entities": dev_seeded,
        });
        // Write manually so a BrokenPipe (harness closed stdout) does not
        // panic the process.
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = writeln!(handle, "{json}");
        let _ = handle.flush();
    }

    tracing::info!(
        gateway = %gateway.id(),
        node_id = topology.node_id,
        shards = topology.shards.len(),
        role = topology.role.name(),
        startup_elapsed_ms = startup_started.elapsed().as_millis(),
        "persistd cluster up"
    );

    // ── Signal handling ─────────────────────────────────────────────────
    // Wait for either Ctrl-C or SIGTERM, then shut down cleanly.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, shutting down");
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, shutting down");
        }
    }

    // Graceful shutdown in reverse order: gateway - schedulers - cluster.
    // The handover orchestrator goes first and is *awaited*, because it holds
    // an `Arc<CellRuntime>` and the journal close below needs the last one.
    if let Some((shutdown, task)) = handover_control {
        let _ = shutdown.send(());
        let _ = task.await;
    }
    drop(handover_runtime);
    tracing::info!("stopping gateway");
    gateway.shutdown().await;

    if let Some(monitor) = fence_freshness_monitor {
        monitor.shutdown();
    }

    if let Some(replicator) = chain_replicator {
        replicator.shutdown().await;
    }

    for scheduler in schedulers {
        scheduler.shutdown().await;
    }

    if let Some(reporter) = metrics_reporter {
        reporter.shutdown().await;
    }

    // Close each runtime's journal explicitly.
    let Ok(runtime) = Arc::try_unwrap(runtime_for_shutdown) else {
        tracing::warn!("runtime Arc still referenced during shutdown");
        return Ok(());
    };
    if let Err(e) = runtime.close().await {
        tracing::warn!(error = %e, "journal close error during shutdown");
    }

    tracing::info!("persistd shutdown complete");
    Ok(())
}

// ── Live shard handover (D26 rule 3, docs/08-persistence.md §3.4.1) ───────

/// One operator-invoked shard handover, as the request file spells it.
///
/// Deserialized rather than parsed out of flags because a handover is issued
/// against a *running* process. It is deliberately the smallest thing that can
/// name a move: which shard, and to whom.
#[derive(Debug, serde::Deserialize)]
struct HandoverRequest {
    /// The grid the shard's `CellId` lives in (P-7: cell ids are
    /// grid-relative). Defaults to the root grid, which is what
    /// `--single-grid` deployments use.
    #[serde(default)]
    grid: u32,
    /// The shard cell, as raw `CellId` bits — the same form `--shard` accepts.
    shard: u64,
    /// The cluster node id the `actor/{grid}/{shard}` row moves to.
    successor_node: u64,
    /// The successor's iroh gateway identity, if the caller knows it.
    ///
    /// Optional, and what it buys is the difference between a refusal and a
    /// redirect: with it, every `WrongOwner` this node then issues for the
    /// shard names where to go instead (D26 rule 3 step 8).
    #[serde(default)]
    successor_gateway: Option<String>,
}

/// What one handover did, written beside the request as `<path>.result`.
#[derive(Debug, serde::Serialize)]
struct HandoverOutcome {
    /// Whether the ownership CAS committed.
    handed_over: bool,
    /// Unix milliseconds at which step 1 was attempted, and at which the last
    /// step finished.
    ///
    /// Reported so a harness can measure the **player-facing** window without
    /// charging this file-watching invocation surface's poll interval to the
    /// protocol: the window that matters starts when admission closes, not
    /// when an operator's request file appeared on disk.
    started_at_unix_ms: u64,
    finished_at_unix_ms: u64,
    /// Why not, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    shard: u64,
    grid: u32,
    successor_node: u64,
    /// The epoch the row carried before the handover, and after it.
    epoch_before: u64,
    epoch_after: u64,
    /// D26 I2's left-hand term: rows live under the shard when the drain
    /// began.
    leases_live_at_drain_start: u64,
    /// D26 I2's right-hand term: `Expire` frames their holders' own sessions
    /// accepted, before the CAS.
    expires_delivered_before_cas: u64,
    /// Holders with no live session here to deliver to. Broken out so the I2
    /// subtraction never silently absorbs them.
    expires_undeliverable: u64,
    /// Whether the drain finished inside `handoff_deadline_ms`.
    drain_complete: bool,
    /// The drain's own bound, for reading the measurement against.
    handoff_deadline_ms: u32,
    /// Stage timings, in milliseconds, as the process that ran them measured.
    mark_ms: u64,
    drain_ms: u64,
    checkpoint_ms: u64,
    cas_ms: u64,
    /// Mark through CAS: the window in which this node stopped admitting
    /// claims under the shard and the successor did not yet own it.
    total_ms: u64,
}

/// Everything a handover needs that outlives a borrow of the gateway.
#[derive(Clone)]
struct HandoverContext {
    runtime: Arc<CellRuntime>,
    checkpoints: Arc<dyn orrery_persistd::checkpoint::CheckpointStore>,
    drain: orrery_persistd::ShardDrainHandle,
    authority_metrics: Arc<AuthorityMetrics>,
    freshness: Option<Arc<FenceFreshnessMonitor>>,
    #[cfg(feature = "fdb")]
    intents: Option<Arc<FdbIntentExecutor>>,
    #[cfg_attr(not(feature = "fdb"), allow(dead_code))]
    node_id: u64,
}

impl HandoverContext {
    /// Re-state the shard set this node's intent ledger is fenced by.
    ///
    /// A no-op without an intent executor. With one it is not optional: the
    /// fence verifies *every* shard the node activated, so a shard that has
    /// correctly moved to a sibling would fail it and refuse every subsequent
    /// intent on this node (`IntentFence`).
    fn refence_intents(&self) {
        #[cfg(feature = "fdb")]
        if let Some(intents) = &self.intents {
            let mut shards = self.runtime.shard_cells();
            shards.sort_unstable_by_key(|shard| shard.to_bits());
            intents.refence(shards, self.node_id, self.runtime.epoch());
        }
    }

    /// Run D26 rule 3's sequence for one shard, start to finish.
    ///
    /// The step numbering below is the ADR's own. Two orderings differ from
    /// the letter of it, both deliberately and both stated in the PR:
    ///
    /// * **The quiesce precedes the `PreHandover` checkpoint.** The ADR reads
    ///   "Checkpoint(PreHandover); stop accepting diffs", which leaves a
    ///   window in which a diff is accepted *after* the checkpoint that is
    ///   about to become the successor's only base — and a sibling has no copy
    ///   of this node's journal to replay it from, so that diff is simply
    ///   lost. Quiescing first closes it. The cost is that refusals start one
    ///   checkpoint earlier, which is inside the same window either way.
    /// * **No row is reassigned during the drain.** See
    ///   `Redistributor::drain_shard`.
    async fn run(&self, request: &HandoverRequest) -> HandoverOutcome {
        let grid = GridId::new(request.grid);
        let shard = match CellId::from_bits(request.shard) {
            Some(shard) => shard,
            None => {
                return HandoverOutcome {
                    handed_over: false,
                    started_at_unix_ms: unix_now_ms(),
                    finished_at_unix_ms: unix_now_ms(),
                    error: Some(format!("invalid shard cell bits {}", request.shard)),
                    shard: request.shard,
                    grid: request.grid,
                    successor_node: request.successor_node,
                    epoch_before: 0,
                    epoch_after: 0,
                    leases_live_at_drain_start: 0,
                    expires_delivered_before_cas: 0,
                    expires_undeliverable: 0,
                    drain_complete: false,
                    handoff_deadline_ms: self.drain.handoff_deadline_ms(),
                    mark_ms: 0,
                    drain_ms: 0,
                    checkpoint_ms: 0,
                    cas_ms: 0,
                    total_ms: 0,
                };
            }
        };
        let successor_gateway = request
            .successor_gateway
            .as_deref()
            .and_then(|hex| hex.parse::<orrery_protocol::NodeId>().ok());
        let mut outcome = HandoverOutcome {
            handed_over: false,
            started_at_unix_ms: unix_now_ms(),
            finished_at_unix_ms: 0,
            error: None,
            shard: request.shard,
            grid: request.grid,
            successor_node: request.successor_node,
            epoch_before: 0,
            epoch_after: 0,
            leases_live_at_drain_start: 0,
            expires_delivered_before_cas: 0,
            expires_undeliverable: 0,
            drain_complete: false,
            handoff_deadline_ms: self.drain.handoff_deadline_ms(),
            mark_ms: 0,
            drain_ms: 0,
            checkpoint_ms: 0,
            cas_ms: 0,
            total_ms: 0,
        };
        if grid != self.runtime.grid() {
            outcome.error = Some(format!(
                "this node serves grid {}, not {}",
                self.runtime.grid().0,
                request.grid
            ));
            return outcome;
        }
        let started = Instant::now();

        // ── Step 1: mark. Status only; still the owner, still the writer. ──
        let marked = match self
            .runtime
            .begin_handover(shard, request.successor_node, successor_gateway)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                outcome.error = Some(format!("mark: {error}"));
                return outcome;
            }
        };
        outcome.epoch_before = marked.epoch.0;
        // The mark is this node's own write to a row it is watching for
        // bulk-ack freshness. Told about it, the monitor stays fresh; not told,
        // it reads its own deliberate status change as a split-brain signal and
        // makes every acknowledgement on every shard provisional for the length
        // of the drain.
        if let Some(freshness) = &self.freshness {
            freshness.rewatch(shard, marked);
        }
        outcome.mark_ms = started.elapsed().as_millis() as u64;
        tracing::info!(
            shard = %shard,
            successor = request.successor_node,
            epoch = marked.epoch.0,
            "handover: shard marked draining"
        );

        // ── Steps 2–4: admission closed by the mark; drain, bounded. ───────
        let drain_started = Instant::now();
        let drain = self.drain.drain_shard(grid, shard).await;
        outcome.drain_ms = drain_started.elapsed().as_millis() as u64;
        outcome.leases_live_at_drain_start = drain.live_at_start;
        outcome.expires_delivered_before_cas = drain.expires_delivered;
        outcome.expires_undeliverable = drain.expires_undeliverable;
        outcome.drain_complete = drain.complete;
        if !drain.complete {
            // The rows this drain did not reach still have live holders here.
            // Moving the row out from under them is the hazard D26 exists to
            // prevent, so the handover does not happen.
            self.authority_metrics.record_handover_aborted();
            match self.runtime.abort_handover(shard).await {
                Ok(restored) => {
                    if let Some(freshness) = &self.freshness {
                        freshness.rewatch(shard, restored);
                    }
                    outcome.error = Some("drain did not complete inside the deadline; handover aborted and the shard is unchanged".into());
                }
                Err(error) => outcome.error = Some(format!(
                    "drain did not complete inside the deadline, and the abort CAS also failed: {error}"
                )),
            }
            outcome.total_ms = started.elapsed().as_millis() as u64;
            outcome.finished_at_unix_ms = unix_now_ms();
            return outcome;
        }

        // ── Step 5: quiesce, then flush. See the note above on the order. ──
        self.runtime.quiesce_handover(shard);
        let checkpoint_started = Instant::now();
        if let Err(error) = self
            .runtime
            .checkpoint_shard_because(
                shard,
                self.checkpoints.as_ref(),
                orrery_persistd::CheckpointCause::PreHandover,
            )
            .await
        {
            self.authority_metrics.record_handover_aborted();
            let abort = self.runtime.abort_handover(shard).await;
            outcome.checkpoint_ms = checkpoint_started.elapsed().as_millis() as u64;
            outcome.total_ms = started.elapsed().as_millis() as u64;
            if let Ok(restored) = abort {
                if let Some(freshness) = &self.freshness {
                    freshness.rewatch(shard, restored);
                }
            }
            outcome.error = Some(match abort {
                Ok(_) => format!("pre-handover checkpoint failed ({error}); handover aborted"),
                Err(abort) => format!(
                    "pre-handover checkpoint failed ({error}), and the abort CAS also failed: {abort}"
                ),
            });
            return outcome;
        }
        outcome.checkpoint_ms = checkpoint_started.elapsed().as_millis() as u64;

        // ── Step 6: hand over. ────────────────────────────────────────────
        let cas_started = Instant::now();
        match self.runtime.complete_handover(shard).await {
            Ok(row) => {
                outcome.epoch_after = row.epoch.0;
                outcome.handed_over = true;
            }
            Err(error) => {
                self.authority_metrics.record_handover_aborted();
                let abort = self.runtime.abort_handover(shard).await;
                outcome.cas_ms = cas_started.elapsed().as_millis() as u64;
                outcome.total_ms = started.elapsed().as_millis() as u64;
                if let Ok(restored) = abort {
                    if let Some(freshness) = &self.freshness {
                        freshness.rewatch(shard, restored);
                    }
                }
                outcome.error = Some(match abort {
                    Ok(_) => format!("handover CAS failed ({error}); aborted"),
                    Err(abort) => {
                        format!("handover CAS failed ({error}), and the abort also failed: {abort}")
                    }
                });
                return outcome;
            }
        }
        outcome.cas_ms = cas_started.elapsed().as_millis() as u64;
        outcome.total_ms = started.elapsed().as_millis() as u64;
        outcome.finished_at_unix_ms = unix_now_ms();

        // The row is gone from this node: stop watching it for bulk-ack
        // freshness, and take it out of the intent ledger's fence. Both would
        // otherwise fail closed on a shard this node handed away on purpose.
        if let Some(freshness) = &self.freshness {
            freshness.forget(shard);
        }
        self.refence_intents();
        self.authority_metrics.record_handover_completed();
        tracing::info!(
            shard = %shard,
            successor = request.successor_node,
            epoch = outcome.epoch_after,
            leases_divested = outcome.leases_live_at_drain_start,
            expires_delivered = outcome.expires_delivered_before_cas,
            total_ms = outcome.total_ms,
            "handover: shard handed to sibling"
        );
        outcome
    }

    /// Adopt any standby shard whose durable row now names this node.
    ///
    /// The successor's whole part in a handover: poll, notice, load, open. It
    /// performs no CAS — the outgoing owner's did — which is why two live
    /// gateways need no transport between them for this.
    async fn poll_standby(&self, standby: &[CellId]) {
        for &shard in standby {
            if self.runtime.actor(self.runtime.grid(), shard).is_some() {
                continue;
            }
            match self
                .runtime
                .adopt_shard(shard, self.checkpoints.as_ref())
                .await
            {
                Ok(row) => {
                    if let Some(freshness) = &self.freshness {
                        if let Err(error) = freshness.adopt(shard, row) {
                            tracing::warn!(%shard, %error, "handover: adopted shard is not watched for fence freshness");
                        }
                    }
                    self.refence_intents();
                    tracing::info!(
                        shard = %shard,
                        epoch = row.epoch.0,
                        "handover: adopted shard from sibling"
                    );
                }
                // The overwhelmingly common answer: the row still names the
                // sibling, because no handover has happened. Logged at trace
                // so a poll loop does not become a log flood.
                Err(error) => {
                    tracing::trace!(%shard, %error, "handover: standby shard not adoptable yet")
                }
            }
        }
    }
}

/// Unix milliseconds, for stamping a handover's own window.
///
/// Wall clock deliberately, and it is the one place in this feature that uses
/// one: every *decision* here is made against the registrar's monotonic clock
/// or against FoundationDB's own ordering, and this is not a decision — it is
/// a timestamp a harness on another machine subtracts from its own reading to
/// report a window. A monotonic instant would be meaningless across processes.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// How often a standby node re-reads its standby shards' rows, and how often a
/// node with `--handover-request` looks for a request.
///
/// Short relative to the < 1 s handover budget docs/08 §3.5 names, because the
/// successor's poll is *inside* the player-facing window: a peer redirected to
/// the successor cannot claim until the successor has opened the shard.
const HANDOVER_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Watch `path` for handover requests and run them, and adopt standby shards
/// whose rows have moved to this node.
///
/// One task for both because they are one clock: a node may be the outgoing
/// owner of one shard and the successor of another at the same time, and the
/// two halves share nothing but the interval.
fn spawn_handover_control(
    context: HandoverContext,
    request_path: Option<PathBuf>,
    standby: Vec<CellId>,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (shutdown, mut stop) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop => break,
                () = tokio::time::sleep(HANDOVER_POLL_INTERVAL) => {}
            }
            if !standby.is_empty() {
                context.poll_standby(&standby).await;
            }
            let Some(path) = request_path.as_ref() else {
                continue;
            };
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            // Renamed aside *before* the handover runs, so a request cannot be
            // executed twice if the process is slow or restarts mid-move.
            let taken = path.with_extension("taken");
            if let Err(error) = std::fs::rename(path, &taken) {
                tracing::warn!(%error, "handover: could not claim the request file");
                continue;
            }
            let request: HandoverRequest = match serde_json::from_slice(&bytes) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(%error, "handover: malformed request");
                    continue;
                }
            };
            let outcome = context.run(&request).await;
            let result_path = path.with_extension("result");
            match serde_json::to_vec(&outcome) {
                Ok(json) => {
                    if let Err(error) = std::fs::write(&result_path, json) {
                        tracing::warn!(%error, "handover: could not write the result file");
                    }
                }
                Err(error) => tracing::warn!(%error, "handover: could not encode the result"),
            }
        }
    });
    (shutdown, task)
}

/// Run the passive half of a static chain topology. It opens no actor runtime,
/// scheduler, fence store, or gateway: mirrored records are its only writes.
async fn run_follower(cli: &Cli, topology: &Topology) -> anyhow::Result<()> {
    if cli.fdb_cluster_file.is_some() {
        anyhow::bail!(
            "--fdb-cluster-file is not valid for a chain follower; a follower hosts only its mirrored journal"
        );
    }
    if cli.secret_key.is_some() {
        anyhow::bail!(
            "--secret-key is not valid for a chain follower; --node-id is its stable chain identity"
        );
    }

    let TopologyRole::Follower { listen, .. } = topology.role else {
        unreachable!("run_follower is called only for follower topology");
    };
    let node_dir = cli.dir.join(format!("node-{}", topology.node_id));
    std::fs::create_dir_all(&node_dir)?;
    let journal = Arc::new(Journal::open(&JournalConfig {
        dir: node_dir,
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::Adaptive,
            batch_window: Duration::from_micros(200),
            ..GroupCommitConfig::default()
        },
    })?);
    let server = spawn_chain_grpc(
        listen,
        Arc::clone(&journal),
        topology
            .chain_id()
            .expect("follower topology always has a durable chain identity"),
    )
    .await?;
    let metrics_reporter = cli
        .metrics_jsonl
        .clone()
        .map(|path| {
            spawn_metrics_reporter(
                path,
                Arc::clone(&journal),
                Arc::new(GatewayMetrics::default()),
                Arc::new(AuthorityMetrics::default()),
            )
        })
        .transpose()?;
    // A follower has no checkpoint cadence to hang retention off — it opens no
    // runtime and no scheduler — so its mirror is released on a timer of its
    // own, bounded by the floor its primary reports (D23).
    let retention = (!cli.no_journal_retention).then(|| {
        let interval = cli.checkpoint_interval_ms.map_or_else(
            || orrery_persistd::checkpoint::CheckpointConfig::default().interval,
            Duration::from_millis,
        );
        orrery_persistd::checkpoint::spawn_mirror_retention(Arc::clone(&journal), interval)
    });

    // stdout is a one-line machine-readable readiness contract. Unlike a
    // primary, the follower has no client endpoint to advertise.
    {
        use std::io::Write;
        let json = serde_json::json!({
            "node_id": topology.node_id,
            "chain_addr": server.addr().to_string(),
            "role": topology.role.name(),
            "journal_open_ms": journal.open_ms(),
        });
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = writeln!(handle, "{json}");
        let _ = handle.flush();
    }
    tracing::info!(
        node_id = topology.node_id,
        chain_addr = %server.addr(),
        shards = topology.shards.len(),
        epoch = topology.epoch.0,
        "persistd chain follower up"
    );

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C, shutting down"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
    }
    server.shutdown().await;
    if let Some(retention) = retention {
        retention.shutdown().await;
    }
    if let Some(reporter) = metrics_reporter {
        reporter.shutdown().await;
    }
    journal.close().await?;
    tracing::info!("persistd chain follower shutdown complete");
    Ok(())
}

/// How many `actor/{shard}` rows the startup pre-read has in flight at once.
///
/// Bounded rather than unbounded: a node owning thousands of shards must not
/// open thousands of simultaneous FDB futures at the one moment it is also
/// replaying a journal. 32 is deep enough that the round-trip latency of a
/// local cluster (~0.1–1 ms, docs/08-persistence.md §5) stops dominating
/// startup and shallow enough to stay a polite client.
const STARTUP_FENCE_READ_CONCURRENCY: usize = 32;

#[derive(Debug)]
struct StartupActivation {
    epoch: Epoch,
    /// The old primary epoch whose chain identity must be adopted.
    promoted_source_epoch: Option<u64>,
    /// The durable `actor/{shard}` rows this activation committed, in the
    /// canonical shard order `activate_shards` returns them in.
    ///
    /// Kept per shard rather than collapsed to the one epoch above, even
    /// though activation refuses a mixed set today: the readiness line is read
    /// by harnesses and operators, and "every shard is at epoch e" is a fact
    /// worth *showing* rather than one they have to know to assume.
    rows: Vec<(CellId, FenceRow)>,
}

/// Perform the only ownership transition allowed by the static topology.
/// Reading first is safe because `activate_shards` compares every row again in
/// one transaction.  We intentionally refuse an ordinary boot over another
/// node: a stale process must be restarted as a follower, never steal back a
/// promoted writer merely because it can reach FDB.
async fn activate_topology(
    topology: &Topology,
    fence: &dyn FenceStore,
    durable: bool,
) -> anyhow::Result<StartupActivation> {
    // The pre-read of every shard's current row, concurrently. It was one
    // sequential round trip per shard, which at the 128-shard deployment of
    // docs/11-roadmap.md §P2 is 128 serialized FDB reads on the startup
    // critical path and grows linearly from there.
    //
    // Nothing about the ownership transition moves here. These reads are
    // side-effect-free, and the transition itself is still the single
    // all-or-nothing `activate_shards` transaction below (fence/fdb.rs
    // re-reads the whole compare set inside it, so a row that changed between
    // this pre-read and the commit is caught there, exactly as before). A
    // partial failure therefore cannot leave the node half-activated: it
    // fails here, before any row is written.
    //
    // `buffered` preserves input order, so `try_collect` surfaces the first
    // failing shard in *shard order* rather than whichever read lost a race,
    // and the role checks below still run in a deterministic order.
    let rows: Vec<Option<FenceRow>> = futures::stream::iter(topology.shards.iter().copied())
        .map(|shard| async move { fence.read(GridId::ROOT, shard).await })
        .buffered(STARTUP_FENCE_READ_CONCURRENCY)
        .try_collect()
        .await?;

    let mut requests = Vec::with_capacity(topology.shards.len());
    let mut source_epoch = None;
    for (&shard, current) in topology.shards.iter().zip(rows) {
        match topology.role {
            TopologyRole::Promotion { primary, .. } => {
                let Some(row) = current else {
                    anyhow::bail!("cannot promote shard {shard:?}: no primary fence row");
                };
                if row.owner != primary {
                    anyhow::bail!("cannot promote shard {shard:?}: expected failed primary {primary}, found owner {}", row.owner);
                }
                if let Some(epoch) = source_epoch {
                    if epoch != row.epoch.0 {
                        anyhow::bail!("cannot promote a shard set with mixed fence epochs");
                    }
                } else {
                    source_epoch = Some(row.epoch.0);
                }
            }
            TopologyRole::Single | TopologyRole::Primary { .. } => {
                if let Some(row) = current {
                    if row.owner != topology.node_id {
                        anyhow::bail!(
                            "startup rejected for shard {shard:?}: owned by node {} at epoch {}",
                            row.owner,
                            row.epoch.0
                        );
                    }
                }
            }
            TopologyRole::Follower { .. } => unreachable!("passive follower does not activate"),
        }
        requests.push(ShardActivation {
            shard,
            expected: current,
        });
    }
    if durable && topology.chain_id().is_some() {
        let expected_epoch = requests
            .first()
            .and_then(|request| request.expected)
            .map_or(1, |row| row.epoch.0 + 1);
        if requests
            .iter()
            .any(|request| request.expected.map_or(1, |row| row.epoch.0 + 1) != expected_epoch)
        {
            anyhow::bail!("cannot activate a shard set with mixed resulting fence epochs");
        }
        if topology.epoch.0 != expected_epoch {
            anyhow::bail!(
                "--chain-epoch {} is an assertion; FDB fence activation would produce epoch {expected_epoch}",
                topology.epoch.0
            );
        }
    }
    let ActivationOutcome::Activated { rows } = fence
        .activate_shards(GridId::ROOT, topology.node_id, &requests)
        .await?
    else {
        anyhow::bail!("startup fence activation conflicted; retry after refreshing topology");
    };
    let epoch = rows
        .first()
        .map(|(_, row)| row.epoch)
        .unwrap_or(Epoch::new(0));
    if rows.iter().any(|(_, row)| row.epoch != epoch) {
        anyhow::bail!("startup fence activation returned mixed epochs");
    }
    Ok(StartupActivation {
        epoch,
        promoted_source_epoch: source_epoch,
        rows,
    })
}

/// Spawn placeholder entities so a harness has claimable subjects.
///
/// These go in through the ordinary actor append path — the same records a
/// real spawn produces — so the entities have a committed cell, appear in area
/// loads, and are claimable exactly like seeded content.
async fn dev_seed_entities(
    runtime: &Arc<CellRuntime>,
    spec: DevSeedSpec,
) -> anyhow::Result<Vec<u64>> {
    let actor = runtime
        .actor(GridId::ROOT, spec.cell)
        .ok_or_else(|| anyhow::anyhow!("no actor owns dev-seed cell {:?}", spec.cell))?
        .clone();
    let author = orrery_protocol::NodeId::from_bytes(&[0; 32])
        .map_err(|error| anyhow::anyhow!("zero author node id: {error}"))?;

    let mut seeded = Vec::with_capacity(spec.count as usize);
    for index in 1..=spec.count {
        let entity = orrery_protocol::PersistId::new(u64::from(index));
        let payload = bytes::Bytes::from_static(b"dev-seed");
        actor
            .start_diff(orrery_protocol::JournalRecord {
                lsn: orrery_protocol::Lsn::new(0, 0),
                cell: spec.cell,
                grid: GridId::ROOT,
                entity,
                tick: orrery_protocol::Tick::new(0),
                epoch: Epoch::new(0),
                author,
                kind: orrery_protocol::RecordKind::Spawn,
                crc: orrery_persistd::payload_crc(&payload),
                payload,
            })
            .await
            .map_err(|error| anyhow::anyhow!("dev seed append failed: {error:?}"))?
            .committed()
            .await
            .map_err(|error| anyhow::anyhow!("dev seed commit failed: {error:?}"))?;
        seeded.push(entity.0);
    }
    Ok(seeded)
}

/// The witness-epoch cache, resolver and mode an FDB executor records
/// `attest/` rows through — D32 clause (d)'s "a deployment running C1 in
/// anything but `off` must wire the epoch authority into **both** the
/// validator and the executor".
///
/// Handed to the executor factory rather than applied after it, because
/// [`FdbIntentExecutor::recording_epochs`] consumes `self` and the binary
/// wraps the executor in an `Arc` the moment it is built. `None` is `off`,
/// where the executor writes no row and the two sides have nothing to agree
/// about.
struct AttestationWiring {
    mode: AttestationEnforcement,
    epochs: Arc<orrery_persistd::witness_epoch::WitnessEpochAuthority>,
    bindings: orrery_persistd::gateway::SharedBindingAuthority,
}

fn gateway_config<F>(
    cli: &Cli,
    secret_key: Option<iroh::SecretKey>,
    mut make_executor: F,
) -> anyhow::Result<GatewayConfig>
where
    F: FnMut(
        &std::path::Path,
        GridId,
        Option<&AttestationWiring>,
    ) -> anyhow::Result<Option<SharedExecutor>>,
{
    // With no coordinator key configured the default deny-all authority
    // stands: interest is unproven, so weak claims are refused and lost leases
    // park. That is the conservative posture, not a silent downgrade.
    let interest_authority: Arc<dyn orrery_persistd::gateway::InterestAuthority> =
        if cli.coordinator_key.is_empty() {
            Arc::new(orrery_persistd::gateway::DenyAllInterestAuthority)
        } else {
            Arc::new(CoordinatorHandoutAuthority::new(
                cli.coordinator_key.iter().map(|key| key.0.clone()),
            ))
        };

    // D32 clause (c)'s control C1, from CLI default to a validator that can
    // actually see it. Everything below this comment is the second half of
    // #217: the arm existed in the library and no deployment could reach it,
    // because `gateway_config` hardcoded `permissive()` and nothing ever
    // called `enforcing()` outside a test.
    let attestation = attestation_wiring(cli)?;

    let executor = if let Some(cluster_file) = cli.fdb_cluster_file.as_deref() {
        make_executor(cluster_file, GridId::ROOT, attestation.as_ref())?
    } else {
        None
    };

    // The admission filter, in whichever of the three postures the operator
    // asked for. `off` keeps the landed constructor untouched — including its
    // absent resolver, which is honest rather than degraded (a validator with
    // no `id/` rows has no account to compare and says so by having none).
    let validator: Arc<dyn orrery_persistd::intent::IntentValidator> = match &attestation {
        None => Arc::new(BaselineIntentValidator::permissive()),
        Some(wiring) => match wiring.mode {
            AttestationEnforcement::Off => Arc::new(BaselineIntentValidator::permissive()),
            AttestationEnforcement::Shadow => Arc::new(BaselineIntentValidator::shadow(
                Arc::clone(&wiring.epochs),
                Arc::clone(&interest_authority),
                Arc::clone(&wiring.bindings),
            )),
            AttestationEnforcement::Required => Arc::new(BaselineIntentValidator::enforcing(
                Arc::clone(&wiring.epochs),
                Arc::clone(&interest_authority),
                Arc::clone(&wiring.bindings),
            )),
        },
    };

    Ok(GatewayConfig {
        witness_epochs: attestation
            .as_ref()
            .map(|wiring| Arc::clone(&wiring.epochs)),
        bind: cli.bind.parse::<SocketAddr>()?,
        secret_key,
        executor,
        authorizer: Arc::new(SessionTokenV1Authorizer::new(
            cli.issuer_key.iter().map(|key| key.0.clone()),
        )),
        interest_authority,
        // The deployed admission filter. Not the library's
        // `PermissiveValidator` (which admits everything, and is a bring-up
        // default), and no longer a blanket refusal: it enforces the intent
        // envelope's shape, the one op this cluster's executor interprets,
        // and the account binding of a ledger credit. Its doc comment states
        // exactly what it does not check — every durable invariant, which a
        // linked `Ruleset` and the FDB transaction still owe.
        validator,
        ..GatewayConfig::default()
    })
}

/// Build C1's wiring from the CLI, or refuse a mode that could never evaluate.
///
/// `None` is `off`: no cache, no resolver, and the landed permissive validator
/// — which is why `off` is byte-for-byte the behaviour this binary had before
/// the flag existed.
///
/// # The resolver is deliberately the one that resolves nothing
///
/// [`UnboundBindingAuthority`] answers `None` to every `owner(n)`, and under
/// D31 clause (f) a miss *excludes*: `E(I)` collapses below
/// `WITNESS_SET_FLOOR_N` and the recorded verdict is
/// `LowPopulationEpoch`'s provisional branch rather than an admission. That is
/// not a degraded shadow, it is an accurate one — nothing in this cluster
/// writes the `id/` rows yet, so a gateway that pretended to resolve bindings
/// would be measuring a control it is not running. When `orrery_identity` is
/// deployed, this is the one line that changes.
///
/// # Errors
///
/// A mode other than `off` with no `--coordinator-key`: the cache would trust
/// no announcement, every intent would answer `UnknownEpoch`, and `shadow`
/// would spend a whole observation period recording
/// [`ShadowVerdict::Unevaluated`]. Refusing at startup is the difference
/// between a misconfiguration and a month of blank telemetry.
///
/// [`UnboundBindingAuthority`]: orrery_persistd::gateway::UnboundBindingAuthority
/// [`ShadowVerdict::Unevaluated`]: orrery_persistd::intent::ShadowVerdict::Unevaluated
fn attestation_wiring(cli: &Cli) -> anyhow::Result<Option<AttestationWiring>> {
    if cli.attestation_enforcement == AttestationEnforcement::Off {
        return Ok(None);
    }
    if cli.coordinator_key.is_empty() {
        anyhow::bail!(
            "--attestation-enforcement {} requires at least one --coordinator-key: \
             the K-of-N quorum judges an intent against a coordinator-signed witness-set \
             announcement, and a gateway that trusts no coordinator key caches none",
            cli.attestation_enforcement.as_str()
        );
    }
    Ok(Some(AttestationWiring {
        mode: cli.attestation_enforcement,
        epochs: Arc::new(orrery_persistd::witness_epoch::WitnessEpochAuthority::new(
            cli.coordinator_key.iter().map(|key| key.0.clone()),
        )),
        bindings: Arc::new(orrery_persistd::gateway::UnboundBindingAuthority),
    }))
}

/// `<count>@<cell>` for [`Cli::dev_seed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DevSeedSpec {
    count: u32,
    cell: CellId,
}

impl FromStr for DevSeedSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (count, cell) = value
            .split_once('@')
            .ok_or_else(|| "expected dev seed as <count>@<cell>".to_string())?;
        let count = count
            .parse::<u32>()
            .map_err(|error| format!("invalid dev seed count `{count}`: {error}"))?;
        if count == 0 {
            return Err("dev seed count must be positive".to_string());
        }
        Ok(Self {
            count,
            cell: cell.parse::<ShardSpec>()?.0,
        })
    }
}

#[derive(Debug, Clone)]
struct IssuerKeySpec(IssuerKey);

impl FromStr for IssuerKeySpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (key_id, public_key) = value
            .split_once('@')
            .ok_or_else(|| "expected issuer key as <key-id>@<public-key>".to_string())?;
        let key_id = key_id
            .parse::<u32>()
            .map_err(|error| format!("invalid issuer key id `{key_id}`: {error}"))?;
        let public_key = public_key
            .parse::<NodeId>()
            .map_err(|error| format!("invalid issuer public key `{public_key}`: {error}"))?;
        Ok(Self(IssuerKey::new(IssuerKeyId::new(key_id), public_key)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ShardSpec(CellId);

impl FromStr for ShardSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_shard_spec(s).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ChainFollower {
    node_id: u64,
    addr: SocketAddr,
}

impl FromStr for ChainFollower {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (node_id, addr) = s
            .split_once('@')
            .ok_or_else(|| "expected follower as <node-id>@<addr>".to_string())?;
        let node_id = node_id
            .parse::<u64>()
            .map_err(|e| format!("invalid follower node id `{node_id}`: {e}"))?;
        let addr = addr
            .parse::<SocketAddr>()
            .map_err(|e| format!("invalid follower addr `{addr}`: {e}"))?;
        Ok(Self { node_id, addr })
    }
}

#[derive(Debug, Clone)]
struct Topology {
    node_id: u64,
    shards: Vec<CellId>,
    epoch: Epoch,
    role: TopologyRole,
}

impl Topology {
    /// The durable chain identity is stable across reconnects and derives only
    /// from explicit CLI topology. `node_id` is a persistd placement identity;
    /// it is deterministically embedded in a `NodeId` because the gRPC chain
    /// protocol shares that typed identity with the iroh-facing architecture.
    fn chain_id(&self) -> Option<DurableChainId> {
        self.chain_id_at(self.epoch.0)
    }

    fn chain_id_at(&self, epoch: u64) -> Option<DurableChainId> {
        match self.role {
            TopologyRole::Single => None,
            TopologyRole::Primary { follower } => Some(DurableChainId {
                primary_node: chain_node_id(self.node_id),
                follower_node: chain_node_id(follower.node_id),
                shard_set: canonical_shard_set(GridId::ROOT, &self.shards),
                epoch,
            }),
            TopologyRole::Follower { primary, .. } => Some(DurableChainId {
                primary_node: chain_node_id(primary),
                follower_node: chain_node_id(self.node_id),
                shard_set: canonical_shard_set(GridId::ROOT, &self.shards),
                epoch,
            }),
            TopologyRole::Promotion { primary, .. } => Some(DurableChainId {
                primary_node: chain_node_id(primary),
                follower_node: chain_node_id(self.node_id),
                shard_set: canonical_shard_set(GridId::ROOT, &self.shards),
                epoch,
            }),
        }
    }

    fn follower(&self) -> Option<ChainFollower> {
        match self.role {
            TopologyRole::Primary { follower } => Some(follower),
            TopologyRole::Single
            | TopologyRole::Follower { .. }
            | TopologyRole::Promotion { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopologyRole {
    /// The legacy one-process harness: actor runtime and gateway, no mirror.
    Single,
    /// The only process allowed to own shards and serve gateway writes.
    Primary { follower: ChainFollower },
    /// A passive journal mirror and gRPC listener, never a gateway.
    Follower { primary: u64, listen: SocketAddr },
    /// A former follower that has atomically fenced `primary` and may now
    /// recover its mirrored history and expose a gateway.
    Promotion { primary: u64, listen: SocketAddr },
}

impl TopologyRole {
    const fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Primary { .. } => "primary",
            Self::Follower { .. } => "follower",
            Self::Promotion { .. } => "promoted-primary",
        }
    }
}

fn resolve_topology(cli: &Cli) -> anyhow::Result<Topology> {
    let shards = resolve_shards(&cli.shard)?;
    let clustered = cli.chain_listen.is_some()
        || !cli.chain_follower.is_empty()
        || cli.chain_primary.is_some()
        || cli.chain_epoch.is_some();
    if !clustered {
        return Ok(Topology {
            node_id: cli.node_id.unwrap_or(0),
            shards,
            epoch: Epoch::new(0),
            role: TopologyRole::Single,
        });
    }

    let node_id = cli
        .node_id
        .ok_or_else(|| anyhow::anyhow!("--node-id is required when any --chain-* flags are set"))?;
    let epoch = Epoch::new(
        cli.chain_epoch
            .ok_or_else(|| anyhow::anyhow!("--chain-epoch is required in clustered topology"))?,
    );
    validate_followers(&cli.chain_follower, node_id)?;

    let role = match (cli.chain_listen, cli.chain_follower.as_slice(), cli.chain_primary, cli.promote_from) {
        (None, [follower], None, None) => TopologyRole::Primary {
            follower: *follower,
        },
        (None, [..], None, Some(_)) => anyhow::bail!(
            "--promote-from is valid only for a passive follower (--chain-listen --chain-primary)"
        ),
        (Some(listen), [], Some(primary), None) => {
            if primary == node_id {
                anyhow::bail!("--chain-primary cannot name the local --node-id {node_id}");
            }
            TopologyRole::Follower { primary, listen }
        }
        (Some(listen), [], Some(primary), Some(promote_from)) => {
            if promote_from != primary { anyhow::bail!("--promote-from must equal --chain-primary for the follower being promoted"); }
            if primary == node_id { anyhow::bail!("--chain-primary cannot name the local --node-id {node_id}"); }
            TopologyRole::Promotion { primary, listen }
        }
        (None, [], _, _) => anyhow::bail!(
            "clustered primary requires exactly one --chain-follower <node-id>@<addr>"
        ),
        (None, [_, _, ..], None, _) => anyhow::bail!(
            "clustered primary requires exactly one --chain-follower <node-id>@<addr>"
        ),
        (Some(_), [], None, _) => anyhow::bail!(
            "clustered follower requires --chain-primary <node-id> with --chain-listen"
        ),
        (Some(_), [..], _, _) => anyhow::bail!(
            "a clustered process is either primary (--chain-follower) or follower (--chain-listen --chain-primary), not both"
        ),
        (None, [..], Some(_), _) => anyhow::bail!(
            "a clustered primary uses --chain-follower; --chain-primary is follower-only"
        ),
    };

    Ok(Topology {
        node_id,
        shards,
        epoch,
        role,
    })
}

/// Canonical durable shard-set encoding, deliberately independent of CLI flag
/// ordering: `version(1) | grid(u32 BE) | count(u32 BE) | cell_bits(u64 BE)*`.
/// It includes the grid because the same `CellId` bit pattern is meaningful in
/// each nested grid (D5); the fixed-width network-order format is stable for
/// durable chain keys and straightforward to inspect in recovery tooling.
fn canonical_shard_set(grid: GridId, shards: &[CellId]) -> Vec<u8> {
    let mut bits: Vec<u64> = shards.iter().map(|cell| cell.to_bits()).collect();
    bits.sort_unstable();
    let mut encoded = Vec::with_capacity(1 + 4 + 4 + bits.len() * 8);
    encoded.push(1);
    encoded.extend_from_slice(&grid.0.to_be_bytes());
    encoded.extend_from_slice(
        &u32::try_from(bits.len())
            .expect("shard set length fits u32")
            .to_be_bytes(),
    );
    for bits in bits {
        encoded.extend_from_slice(&bits.to_be_bytes());
    }
    encoded
}

/// Derive the typed durable-chain identity from the explicit stable process
/// id. This is not an iroh authentication key: chain RPC currently authenticates
/// the durable tuple, and this mapping only avoids representing the same node
/// with two unrelated ID types in that tuple.
fn chain_node_id(node_id: u64) -> orrery_protocol::NodeId {
    let mut seed = [0_u8; 32];
    seed[..20].copy_from_slice(b"orrery-chain-node-v1");
    seed[24..].copy_from_slice(&node_id.to_be_bytes());
    iroh::SecretKey::from_bytes(&seed).public()
}

/// The shard set this process owns, in the canonical order the rest of
/// startup requires.
///
/// The sort is not cosmetic: `fence::validate_activation_set` refuses an
/// activation whose shards are not *strictly sorted by `CellId` bits*, so a
/// `--shard` list in any other order dies at durable activation with
/// "shards must be strictly sorted by CellId bits" — a message about an
/// internal invariant, produced by a flag order the operator had no reason to
/// think was significant. Nothing downstream depends on flag order
/// (`canonical_shard_set` already sorts for the durable chain identity), so
/// the canonical order is established once, here.
fn resolve_shards(shards: &[ShardSpec]) -> anyhow::Result<Vec<CellId>> {
    let mut shards: Vec<CellId> = if shards.is_empty() {
        vec![CellId::ROOT]
    } else {
        shards.iter().map(|spec| spec.0).collect()
    };
    shards.sort_unstable_by_key(|shard| shard.to_bits());

    validate_shards(&shards)?;
    Ok(shards)
}

/// The shards this node may adopt later, validated against the ones it serves.
///
/// D26's "`--shard` sets across siblings must not overlap" is an operator
/// property the CAS only enforces after the fact, at the cost of a failed
/// activation. Within one process it is checkable up front, and a standby
/// shard is exactly a shard this process intends to serve — so it is held to
/// the same rule as `--shard`, against `--shard` and against itself.
fn resolve_standby_shards(cli: &Cli, topology: &Topology) -> anyhow::Result<Vec<CellId>> {
    if cli.standby_shard.is_empty() {
        return Ok(Vec::new());
    }
    if matches!(topology.role, TopologyRole::Follower { .. }) {
        anyhow::bail!("--standby-shard is not valid for a chain follower: a follower is never a gateway and adopts no shard");
    }
    if cli.fdb_cluster_file.is_none() {
        anyhow::bail!(
            "--standby-shard requires --fdb-cluster-file: a handover between two processes is a \
             transition on one durable `actor/{{grid}}/{{shard}}` row, and an in-memory fence store \
             is not shared with anything"
        );
    }
    let standby = resolve_shards(&cli.standby_shard)?;
    let mut combined = topology.shards.clone();
    combined.extend(standby.iter().copied());
    validate_shards(&combined).map_err(|error| {
        anyhow::anyhow!("--standby-shard overlaps the served shard set: {error}")
    })?;
    Ok(standby)
}

fn validate_followers(followers: &[ChainFollower], node_id: u64) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for follower in followers {
        if follower.node_id == node_id {
            anyhow::bail!("--chain-follower cannot target the local --node-id {node_id}");
        }
        if !seen.insert(follower.node_id) {
            anyhow::bail!(
                "duplicate --chain-follower entry for node {}",
                follower.node_id
            );
        }
    }
    Ok(())
}

/// Reject a shard set that is not a partition: no duplicates, and no cell
/// inside another's subtree.
///
/// # Why sorting makes this a neighbour test
///
/// The obvious implementation compares every pair, which is 8 128 comparisons
/// at the 128-shard deployment P2 describes and 8 million at 4 096 — a wall
/// well before the shard counts a real deployment implies. Sorting by raw
/// `CellId` bits removes the quadratic factor, and the reason it is *safe* to
/// only compare neighbours is a property of the encoding, not an assumption:
///
/// - A cell's subtree is exactly the contiguous id range
///   `[id − lsb + 1, id + lsb − 1]`, where `lsb` is the level sentinel bit
///   (`CellId::subtree_range`). Every such range **contains its own cell's
///   id** — the sentinel sits in the middle of the range.
/// - Those ranges form a laminar family: two cells' subtrees are either
///   nested or disjoint, because the Morton prefix of one is a prefix of the
///   other or it is not.
/// - So if `A` contains `B`, then `B`'s id lies in `A`'s range, and *every id
///   numerically between them* lies in `A`'s range too — every cell sorted
///   between `A` and `B` therefore also overlaps `A`. An overlapping pair at
///   distance `k > 1` in sorted order thus implies an overlapping pair at
///   distance `k − 1`, and by induction one at distance 1.
///
/// Comparing adjacent pairs is therefore complete: it cannot miss an overlap.
/// (Duplicates sort adjacent too, so the same pass names them, with the
/// message they had before.) `a_full_interest_lattice_of_shards_is_accepted_and_canonical`
/// and `sorted_overlap_detection_matches_the_all_pairs_definition` hold this
/// against the all-pairs definition it replaces.
fn validate_shards(shards: &[CellId]) -> anyhow::Result<()> {
    let mut sorted: Vec<CellId> = shards.to_vec();
    sorted.sort_unstable_by_key(|shard| shard.to_bits());

    for pair in sorted.windows(2) {
        let (left, right) = (pair[0], pair[1]);
        if left == right {
            anyhow::bail!("duplicate --shard {left}");
        }
        if left.is_prefix_of(right) || right.is_prefix_of(left) {
            anyhow::bail!("overlapping --shard values: {left} and {right}");
        }
    }

    Ok(())
}

fn parse_shard_spec(s: &str) -> Result<CellId, String> {
    if s.contains('@') || s.contains(',') || s.starts_with("coord:") || s.starts_with("coords:") {
        return parse_shard_coords(s);
    }
    parse_shard_raw(s)
}

fn parse_shard_raw(s: &str) -> Result<CellId, String> {
    let raw = s
        .strip_prefix("raw:")
        .or_else(|| s.strip_prefix("raw="))
        .unwrap_or(s);
    let cleaned = raw.replace('_', "");
    let value = if let Some(hex) = cleaned.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|e| format!("invalid raw shard `{raw}`: {e}"))?
    } else if let Some(hex) = cleaned.strip_prefix("0X") {
        u64::from_str_radix(hex, 16).map_err(|e| format!("invalid raw shard `{raw}`: {e}"))?
    } else {
        cleaned
            .parse::<u64>()
            .map_err(|e| format!("invalid raw shard `{raw}`: {e}"))?
    };
    CellId::from_bits(value).ok_or_else(|| "raw shard `0` is not valid".to_string())
}

fn parse_shard_coords(s: &str) -> Result<CellId, String> {
    let coords = s
        .strip_prefix("coord:")
        .or_else(|| s.strip_prefix("coords:"))
        .unwrap_or(s);
    let (xyz, level) = coords
        .split_once('@')
        .ok_or_else(|| "coordinate shard must use x,y,z@level".to_string())?;
    let mut parts = xyz.split(',');
    let parse_coord = |label: &str, value: Option<&str>| -> Result<i32, String> {
        let value = value.ok_or_else(|| format!("missing {label} coordinate in `{s}`"))?;
        value
            .parse::<i32>()
            .map_err(|e| format!("invalid {label} coordinate `{value}`: {e}"))
    };
    let x = parse_coord("x", parts.next())?;
    let y = parse_coord("y", parts.next())?;
    let z = parse_coord("z", parts.next())?;
    if parts.next().is_some() {
        return Err(format!("coordinate shard has too many coordinates: `{s}`"));
    }
    let level = level
        .parse::<u8>()
        .map_err(|e| format!("invalid shard level `{level}`: {e}"))?;
    CellId::from_coords(IVec3::new(x, y, z), level).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter an unset `RUST_LOG` produces must admit exactly what the
    /// unfiltered `fmt()` builder admitted, or every harness that reads this
    /// binary's stderr changes shape at once.
    #[test]
    fn absent_rust_log_keeps_the_previous_info_default() {
        use tracing_subscriber::layer::Layer;
        let hint = Layer::<tracing_subscriber::Registry>::max_level_hint(&log_filter(None));
        assert_eq!(
            hint,
            Some(tracing_subscriber::fmt::Subscriber::DEFAULT_MAX_LEVEL)
        );
    }

    /// The defect this replaced: `RUST_LOG` reached nothing, so a `debug!` in
    /// the gateway could not be turned on during an incident.
    #[test]
    fn rust_log_raises_the_filter_for_a_targeted_module() {
        use tracing_subscriber::layer::Layer;
        let filter = log_filter(Some("orrery_persistd::journal=debug"));
        let hint = Layer::<tracing_subscriber::Registry>::max_level_hint(&filter);
        assert_eq!(hint, Some(tracing_subscriber::filter::LevelFilter::DEBUG));
    }

    /// A mistyped `RUST_LOG` must not abort a persistence authority at boot.
    #[test]
    fn malformed_rust_log_falls_back_to_the_default() {
        use tracing_subscriber::layer::Layer;
        assert!(
            EnvFilter::try_new("orrery_persistd=notalevel").is_err(),
            "the fixture must actually be a parse error"
        );
        let hint = Layer::<tracing_subscriber::Registry>::max_level_hint(&log_filter(Some(
            "orrery_persistd=notalevel",
        )));
        assert_eq!(
            hint,
            Some(tracing_subscriber::fmt::Subscriber::DEFAULT_MAX_LEVEL)
        );
    }

    fn cli(fdb_cluster_file: Option<PathBuf>) -> Cli {
        Cli {
            node_id: None,
            dir: PathBuf::from("persistd-data"),
            chain_listen: None,
            chain_primary: None,
            chain_epoch: None,
            promote_from: None,
            chain_follower: Vec::new(),
            bind: "127.0.0.1:0".parse().expect("valid loopback bind"),
            fdb_cluster_file,
            allow_volatile_leases: false,
            issuer_key: vec![IssuerKeySpec(IssuerKey::new(
                IssuerKeyId::new(1),
                iroh::SecretKey::from_bytes(&[9; 32]).public(),
            ))],
            coordinator_key: Vec::new(),
            attestation_enforcement: AttestationEnforcement::Off,
            no_journal_retention: false,
            checkpoint_interval_ms: None,
            dev_seed: None,
            secret_key: None,
            shard: Vec::new(),
            standby_shard: Vec::new(),
            handover_request: None,
            metrics_jsonl: None,
        }
    }

    #[test]
    fn journal_metrics_jsonl_uses_dashboard_sample_batch_contract() {
        let mut output = Vec::new();
        write_journal_metric_batches(
            &mut output,
            &[
                orrery_persistd::JournalCommitSample {
                    value_us: 2_000,
                    count: 3,
                },
                orrery_persistd::JournalCommitSample {
                    value_us: 5_000,
                    count: 1,
                },
            ],
        )
        .expect("JSONL serialization succeeds");

        let records: Vec<serde_json::Value> = std::str::from_utf8(&output)
            .expect("JSONL is UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL record"))
            .collect();
        assert_eq!(
            records,
            vec![
                serde_json::json!({
                    "type": "sample_batch",
                    "series": "journal_commit_ms",
                    "value_us": 2_000,
                    "count": 3,
                }),
                serde_json::json!({
                    "type": "sample_batch",
                    "series": "journal_commit_ms",
                    "value_us": 5_000,
                    "count": 1,
                }),
            ]
        );
    }

    #[test]
    fn gateway_authority_record_carries_every_counter() {
        // Five of the ten were emitted for a while. Of the five that were
        // not, `handoff_timed_out` is the zombie-host symptom
        // docs/09-services-and-ops.md §10's runbook turns on — a holder that
        // answers no divest request — and the three misroute counters are the
        // alarm docs/08-persistence.md §2.1.2 names for a peer steering diffs
        // onto the fenced route's expensive branch. The snapshot is the
        // schema's only source, so the assertion is on the key set.
        let metrics = AuthorityMetrics::default();
        // A cursor that differs from the (all-zero) snapshot, so the record is
        // written: the writer is deliberately change-triggered.
        let mut cursor = orrery_persistd::AuthoritySnapshot {
            divest_requested: 1,
            ..orrery_persistd::AuthoritySnapshot::default()
        };
        let mut output = Vec::new();
        write_gateway_authority(&mut output, &metrics, &mut cursor)
            .expect("JSONL serialization succeeds");

        let record: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&output).expect("JSONL is UTF-8").trim())
                .expect("valid JSONL record");
        assert_eq!(record["type"], "gateway_authority");
        for field in [
            "duplicate_authority",
            "misrouted_diffs",
            "unindexed_diffs",
            "misroute_throttled",
            "reassigned",
            "parked_without_successor",
            "divested",
            "divest_rejected",
            "divest_requested",
            "handoff_timed_out",
            "expire_fanout_sent",
            "expire_fanout_skipped",
            "expire_fanout_dropped",
            // D26's live-handover tier. The first two are invariant I2's two
            // terms and are listed adjacently on purpose: I2 is their
            // difference, so a record carrying one without the other exports
            // half a statement.
            "handover_leases_live_at_drain_start",
            "handover_expires_delivered_before_cas",
            "heartbeats_rejected_wrong_owner",
            "handover_parks",
            "claims_denied_draining",
            "handovers_completed",
            "handovers_aborted",
            "stale_sessions_reaped",
        ] {
            assert!(
                record.get(field).is_some(),
                "gateway_authority dropped {field}"
            );
        }
        let object = record.as_object().expect("record is an object");
        assert_eq!(
            object.len(),
            22,
            "twenty-one counters plus the type tag: {object:?}"
        );
        // p3-island's reader takes `duplicate_authority` and ignores every
        // other key, so widening the record is safe for the one parser there
        // is (`p3-island/src/main.rs`).
        assert_eq!(record["duplicate_authority"], 0);
    }

    #[test]
    fn gateway_config_wires_executor_from_root_grid() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(PathBuf, GridId)>));
        let seen_capture = Arc::clone(&seen);
        let cfg = gateway_config(
            &cli(Some(PathBuf::from("/tmp/fdb.cluster"))),
            None,
            move |cluster_file, grid, _attestation| {
                let mut slot = seen_capture.lock().expect("capture lock");
                *slot = Some((cluster_file.to_path_buf(), grid));
                Ok(Some(
                    Arc::new(orrery_persistd::MemIntentExecutor::new()) as SharedExecutor
                ))
            },
        )
        .expect("gateway config");

        let slot = seen.lock().expect("capture lock");
        let (path, grid) = slot.as_ref().expect("executor factory called");
        assert_eq!(path, &PathBuf::from("/tmp/fdb.cluster"));
        assert_eq!(*grid, GridId::ROOT);
        assert!(cfg.executor.is_some(), "FDB cluster file wires an executor");
    }

    #[test]
    fn gateway_config_leaves_executor_empty_without_fdb() {
        let cfg = gateway_config(&cli(None), None, |_cluster_file, _grid, _attestation| {
            panic!("executor factory should not be called without --fdb-cluster-file")
        })
        .expect("gateway config");

        assert!(cfg.executor.is_none(), "no FDB file leaves executor unset");
    }

    #[test]
    fn gateway_config_accepts_only_tokens_signed_by_the_configured_issuer() {
        use orrery_persistd::gateway::GatewayAuthorization;
        use orrery_protocol::{
            AccountId, SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1,
            SessionTokenVerificationError, UnixMillis,
        };

        // Given: production startup has one configured identity issuer.
        let config = gateway_config(&cli(None), None, |_cluster_file, _grid, _attestation| {
            Ok(None)
        })
        .expect("gateway config");
        let client = iroh::SecretKey::from_bytes(&[8; 32]);
        let signed = SessionTokenV1::sign(
            SessionTokenClaimsV1::new(
                AccountId::new(7),
                client.public(),
                UnixMillis::new(900),
                SessionTokenTtlMs::new(1_000),
                SessionStanding::Good,
                IssuerKeyId::new(1),
                false,
            ),
            &iroh::SecretKey::from_bytes(&[9; 32]),
        )
        .expect("sign test session token")
        .encode()
        .expect("encode test session token");

        // When: the gateway admits a valid credential and rejects malformed input.
        let admitted =
            config
                .authorizer
                .authorize(&signed, &client.public(), UnixMillis::new(1_000));
        let rejected = config
            .authorizer
            .authorize(&[], &client.public(), UnixMillis::new(1_000));

        // Then: only the configured issuer's signed token establishes authority.
        assert!(matches!(admitted, Ok(GatewayAuthorization::Valid(_))));
        assert_eq!(rejected, Err(SessionTokenVerificationError::Malformed));
    }

    // ── D32 clause (c): control C1's deployment lever ────────────────────
    //
    // The point of these three tests is not that the flag parses — clap can be
    // trusted with that — but that each position *reaches the objects it is
    // supposed to reach*: the validator, the gateway's epoch cache, and the
    // executor's `attest/` marker. Before #217 the enforcing constructor had
    // no caller in this file at all, so a flag that merely parsed would have
    // reproduced exactly the defect it was written to fix.

    /// A `Cli` selecting `mode`, with the coordinator key every mode but `off`
    /// requires.
    fn cli_enforcing(mode: AttestationEnforcement) -> Cli {
        Cli {
            coordinator_key: vec![IssuerKeySpec(IssuerKey::new(
                IssuerKeyId::new(2),
                iroh::SecretKey::from_bytes(&[11; 32]).public(),
            ))],
            attestation_enforcement: mode,
            // A cluster file, so the executor factory actually runs: the
            // executor half of the wiring is only observable on the FDB path.
            ..cli(Some(PathBuf::from("/tmp/fdb.cluster")))
        }
    }

    /// A signed intent naming a cell-epoch no gateway in this test has an
    /// announcement for.
    ///
    /// The discriminating fixture: `off` and `shadow` both admit it and
    /// `required` refuses it, so one intent separates all three modes without
    /// standing up a coordinator.
    fn unannounced_intent() -> (orrery_protocol::Intent, iroh::SecretKey) {
        let key = iroh::SecretKey::from_bytes(&[12; 32]);
        let mut intent = orrery_protocol::Intent {
            evidence: None,
            intent_id: 4242,
            issuer: key.public(),
            cell_epoch: orrery_protocol::CellEpoch::new(0x0007_0000_0000_0007),
            ops: vec![orrery_protocol::IntentOp {
                // `Ruleset`-opaque: the mode decides this intent, not the op.
                op: 100,
                args: bytes::Bytes::new(),
            }],
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        (intent, key)
    }

    /// Build a config in `mode` and report what the flag actually reached:
    /// the validator's verdict on an unannounced intent, whether the gateway
    /// got an epoch cache, and the mode the executor factory was handed.
    fn wiring_reached_by(
        mode: AttestationEnforcement,
    ) -> (
        orrery_persistd::intent::IntentVerdict,
        bool,
        Option<AttestationEnforcement>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(None::<AttestationEnforcement>));
        let capture = Arc::clone(&seen);
        let config = gateway_config(
            &cli_enforcing(mode),
            None,
            move |_cluster_file, _grid, attestation| {
                *capture.lock().expect("capture lock") = attestation.map(|wiring| wiring.mode);
                Ok(None)
            },
        )
        .expect("gateway config");

        let (intent, key) = unannounced_intent();
        let verdict = config.validator.validate(
            &intent,
            &orrery_persistd::intent::IntentContext {
                issuer: key.public(),
                account: Some(orrery_protocol::AccountId::new(7)),
                standing: orrery_protocol::SessionStanding::Good,
            },
        );
        let executor_mode = *seen.lock().expect("capture lock");
        (verdict, config.witness_epochs.is_some(), executor_mode)
    }

    /// The flag's default, pinned so that changing it is a test change and
    /// therefore visible in review.
    ///
    /// D32 clause (c)'s inventory sets C1's CLI default to `off`, for the
    /// reason #147 gave its switch: landing a flag must change nothing until
    /// an operator acts.
    #[test]
    fn the_attestation_flag_defaults_to_off_and_that_default_changes_nothing() {
        use clap::Parser;

        let parsed = Cli::try_parse_from(["persistd"]).expect("bare invocation parses");
        assert_eq!(parsed.attestation_enforcement, AttestationEnforcement::Off);

        // And `off` is the landed behaviour byte for byte: no epoch cache on
        // the gateway, no wiring handed to the executor, and the permissive
        // validator admitting the zero-attestation path every issuer in this
        // tree sends.
        let config = gateway_config(&cli(None), None, |_cluster_file, _grid, attestation| {
            assert!(
                attestation.is_none(),
                "`off` hands the executor no epoch authority, so it writes no `attest/` row"
            );
            Ok(None)
        })
        .expect("gateway config");
        assert!(config.witness_epochs.is_none());

        let (intent, key) = unannounced_intent();
        assert!(matches!(
            config.validator.validate(
                &intent,
                &orrery_persistd::intent::IntentContext {
                    issuer: key.public(),
                    account: Some(orrery_protocol::AccountId::new(7)),
                    standing: orrery_protocol::SessionStanding::Good,
                },
            ),
            orrery_persistd::intent::IntentVerdict::Admit(_)
        ));
    }

    /// Each of the three positions reaches the validator, the epoch cache and
    /// the executor — not merely the argument parser.
    #[test]
    fn the_attestation_flag_selects_all_three_modes_at_the_validator() {
        use orrery_persistd::intent::IntentVerdict;

        // `off`: nothing is evaluated, nothing is wired, the intent commits.
        let (verdict, cached, executor) = wiring_reached_by(AttestationEnforcement::Off);
        assert!(matches!(verdict, IntentVerdict::Admit(_)));
        assert!(!cached, "`off` configures no witness-epoch cache");
        assert_eq!(executor, None);

        // `shadow`: the predicate is evaluated against a cache that resolves
        // nothing, the verdict is recorded, and the intent commits anyway.
        // That last clause is the one #222 gates on.
        let (verdict, cached, executor) = wiring_reached_by(AttestationEnforcement::Shadow);
        assert!(
            matches!(verdict, IntentVerdict::Admit(_)),
            "shadow suppresses the refusal `required` returns on this exact intent"
        );
        assert!(cached, "shadow needs the cache the predicate resolves in");
        assert_eq!(
            executor,
            Some(AttestationEnforcement::Shadow),
            "D32 clause (d): the executor's marker and re-proof follow the same mode"
        );

        // `required`: the same intent is refused. The pair is the whole
        // assertion — a flag proven to reach the validator is a flag whose two
        // enforcing positions answer differently on one input.
        let (verdict, cached, executor) = wiring_reached_by(AttestationEnforcement::Required);
        assert!(
            matches!(verdict, IntentVerdict::Reject { .. }),
            "an unannounced cell-epoch is `UnknownEpoch`, which `required` refuses"
        );
        assert!(cached);
        assert_eq!(executor, Some(AttestationEnforcement::Required));
    }

    /// A mode that could never evaluate anything is refused at startup rather
    /// than run.
    ///
    /// Without a coordinator key the cache trusts no announcement, so `shadow`
    /// would spend its whole observation period recording
    /// `Unevaluated(NoEpochAuthority)`-shaped nothing and `required` would
    /// refuse every intent on the cluster. Both are misconfigurations, and the
    /// difference between catching one at startup and finding it a month later
    /// is the whole value of the check.
    #[test]
    fn an_enforcing_mode_without_a_coordinator_key_refuses_to_start() {
        for mode in [
            AttestationEnforcement::Shadow,
            AttestationEnforcement::Required,
        ] {
            let cli = Cli {
                attestation_enforcement: mode,
                ..cli(None)
            };
            assert!(
                cli.coordinator_key.is_empty(),
                "the fixture must be keyless"
            );
            let Err(error) =
                gateway_config(&cli, None, |_cluster_file, _grid, _attestation| Ok(None))
            else {
                panic!("a keyless {} mode must not start", mode.as_str());
            };
            assert!(
                error.to_string().contains("--coordinator-key"),
                "the refusal must name the missing argument: {error}"
            );
        }
    }

    #[test]
    fn shard_parser_accepts_raw_and_coordinate_input() {
        let raw = parse_shard_spec("0xA924_9249_2492_4E00").expect("raw shard parses");
        let coords = parse_shard_spec("2,-1,8@21").expect("coordinate shard parses");
        assert_eq!(raw, CellId::from_bits(0xA924_9249_2492_4E00).unwrap());
        assert_eq!(
            coords,
            CellId::from_coords(IVec3::new(2, -1, 8), 21).unwrap()
        );
    }

    #[test]
    fn overlapping_local_shards_are_rejected() {
        let shards = vec![
            CellId::ROOT,
            CellId::from_coords(IVec3::new(0, 0, 0), 1).unwrap(),
        ];
        let err = validate_shards(&shards).expect_err("overlap must be rejected");
        assert!(
            err.to_string().contains("overlapping --shard values"),
            "unexpected error: {err}"
        );
    }

    /// The deployment the P2 criterion describes is one actor per level-18
    /// shard, and the kill-9 gate's seeded world spans 128 of them. That is a
    /// shard set two orders of magnitude larger than any flag list this binary
    /// had been given, so the two O(n²) admission checks it goes through are
    /// exercised here at the real size: `validate_shards` compares every pair
    /// for prefix containment, and `canonical_shard_set` must produce one
    /// stable encoding regardless of the flag order the harness emitted.
    #[test]
    fn a_full_interest_lattice_of_shards_is_accepted_and_canonical() {
        // 8×4×4 = 128 distinct level-18 cells: siblings at one level, so no
        // cell is a prefix of another and the whole set is a legal deployment.
        let shards: Vec<CellId> = (0..128)
            .map(|i| {
                let x = i % 8;
                let y = (i / 8) % 4;
                let z = i / 32;
                CellId::from_coords(IVec3::new(x, y, z), orrery_protocol::SHARD_LEVEL)
                    .expect("in range")
            })
            .collect();
        assert_eq!(shards.len(), 128);
        validate_shards(&shards).expect("a disjoint level-18 shard set is a legal deployment");

        // Order independence, at a size where an accidental order-sensitive
        // encoding would make the primary's and follower's `DurableChainId`
        // disagree and the mirror handshake fail for a reason no log names.
        let mut reversed = shards.clone();
        reversed.reverse();
        assert_eq!(
            canonical_shard_set(GridId::ROOT, &shards),
            canonical_shard_set(GridId::ROOT, &reversed),
        );

        // And a shard set is not silently truncated: the encoding carries all
        // 128 cells (version + grid + count + 8 bytes each).
        assert_eq!(
            canonical_shard_set(GridId::ROOT, &shards).len(),
            1 + 4 + 4 + 128 * 8
        );
    }

    /// The all-pairs definition `validate_shards` used to be, kept as the
    /// oracle the sorted implementation is held against (and as the slow half
    /// of the scaling measurement below).
    fn overlaps_all_pairs(shards: &[CellId]) -> bool {
        for (idx, &left) in shards.iter().enumerate() {
            for &right in &shards[idx + 1..] {
                if left == right || left.is_prefix_of(right) || right.is_prefix_of(left) {
                    return true;
                }
            }
        }
        false
    }

    /// A deterministic pseudo-random cell at `level`, spread over the whole
    /// coordinate range that level admits.
    fn scattered_cell(seed: u64, level: u8) -> CellId {
        let mut h = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        h ^= h >> 33;
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 29;
        let half = 1i64 << (level - 1);
        let axis = |shift: u32| -> i32 {
            let v = ((h >> shift) & ((1u64 << level) - 1)) as i64;
            (v - half) as i32
        };
        CellId::from_coords(IVec3::new(axis(0), axis(21), axis(42)), level).expect("in range")
    }

    /// The sorted neighbour test must accept exactly what the all-pairs
    /// comparison accepted — no overlap missed, none invented.
    ///
    /// The interesting cases are the ones the Morton encoding makes
    /// non-obvious: a parent whose id sorts *below* its child, a parent whose
    /// id sorts *above* it, and disjoint cells that sort between an
    /// overlapping pair. The scattered mixed-level sets below produce all
    /// three; the two hand-built cases pin the ancestor relation explicitly.
    #[test]
    fn sorted_overlap_detection_matches_the_all_pairs_definition() {
        let mut checked_overlapping = 0_u32;
        let mut checked_disjoint = 0_u32;
        for trial in 0..2_000_u64 {
            // Mixed levels, so ancestors and descendants really do occur.
            let shards: Vec<CellId> = (0..6)
                .map(|i| {
                    let level = 1 + ((trial + i) % 4) as u8;
                    scattered_cell(trial * 7 + i, level)
                })
                .collect();
            let expected = overlaps_all_pairs(&shards);
            let actual = validate_shards(&shards).is_err();
            assert_eq!(
                expected, actual,
                "trial {trial} disagreed on {shards:?} (all-pairs says overlapping={expected})"
            );
            if expected {
                checked_overlapping += 1;
            } else {
                checked_disjoint += 1;
            }
        }
        // The differential is only evidence if both answers occurred.
        assert!(checked_overlapping > 0, "no overlapping set was generated");
        assert!(checked_disjoint > 0, "no disjoint set was generated");

        // A parent and a descendant, with unrelated disjoint cells sorted
        // between them — the case the neighbour test has to survive.
        let parent = CellId::from_coords(IVec3::new(0, 0, 0), 2).expect("in range");
        let child = parent.children()[7];
        assert!(parent.is_prefix_of(child));
        let mut nested = vec![child, parent];
        nested.push(CellId::from_coords(IVec3::new(-2, -2, -2), 2).expect("in range"));
        nested.push(CellId::from_coords(IVec3::new(1, 1, 1), 2).expect("in range"));
        assert!(overlaps_all_pairs(&nested));
        assert!(
            validate_shards(&nested).is_err(),
            "a parent and its descendant must be refused however the set is ordered"
        );

        // The root contains everything, in either sort position.
        assert!(validate_shards(&[CellId::ROOT, child]).is_err());
        assert!(validate_shards(&[child, CellId::ROOT]).is_err());

        // And a duplicate is still named as a duplicate, not as an overlap.
        let err = validate_shards(&[child, child]).expect_err("duplicate must be rejected");
        assert!(
            err.to_string().contains("duplicate --shard"),
            "unexpected error: {err}"
        );
    }

    /// The complexity change, measured rather than asserted.
    ///
    /// `#[ignore]` because a wall-clock ratio is not a sound gate on a shared,
    /// concurrently-loaded box — the ratio it prints is evidence for a human
    /// (and for the PR), while
    /// `sorted_overlap_detection_matches_the_all_pairs_definition` is what
    /// actually guards the behaviour. Run with:
    /// `cargo test -p orrery_persistd --bin persistd -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not a gate: prints a wall-clock comparison"]
    fn validate_shards_scaling_measurement() {
        for count in [128_u64, 1024, 4096] {
            // A disjoint set (the legal deployment, and the worst case for
            // both implementations: neither can exit early).
            let shards: Vec<CellId> = (0..count)
                .map(|i| {
                    let x = (i % 16) as i32;
                    let y = ((i / 16) % 16) as i32;
                    let z = (i / 256) as i32;
                    CellId::from_coords(IVec3::new(x, y, z), orrery_protocol::SHARD_LEVEL)
                        .expect("in range")
                })
                .collect();
            assert_eq!(shards.len() as u64, count);
            validate_shards(&shards).expect("disjoint set is legal");

            let started = Instant::now();
            for _ in 0..20 {
                assert!(!overlaps_all_pairs(&shards));
            }
            let all_pairs = started.elapsed() / 20;

            let started = Instant::now();
            for _ in 0..20 {
                validate_shards(&shards).expect("disjoint set is legal");
            }
            let sorted = started.elapsed() / 20;

            println!(
                "validate_shards n={count}: all-pairs {:?} vs sorted {:?}",
                all_pairs, sorted
            );
        }
    }

    #[test]
    fn duplicate_chain_followers_are_rejected() {
        let followers = vec![
            ChainFollower {
                node_id: 2,
                addr: "127.0.0.1:3001".parse().unwrap(),
            },
            ChainFollower {
                node_id: 2,
                addr: "127.0.0.1:3002".parse().unwrap(),
            },
        ];
        let err = validate_followers(&followers, 1).expect_err("duplicate follower must fail");
        assert!(
            err.to_string().contains("duplicate --chain-follower"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn clustered_topology_requires_node_id() {
        let mut cli = cli(None);
        cli.chain_listen = Some("127.0.0.1:3000".parse().unwrap());
        let err = resolve_topology(&cli).expect_err("clustered topology must be rejected");
        assert!(
            err.to_string().contains("--node-id is required"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn primary_topology_is_explicit_and_uses_its_chain_epoch() {
        let mut cli = cli(None);
        cli.node_id = Some(7);
        cli.chain_epoch = Some(42);
        cli.chain_follower = vec![ChainFollower {
            node_id: 8,
            addr: "127.0.0.1:3001".parse().unwrap(),
        }];
        cli.shard = vec![ShardSpec(CellId::from_bits(9).unwrap())];

        let topology = resolve_topology(&cli).expect("primary topology");
        assert_eq!(topology.role.name(), "primary");
        assert_eq!(topology.epoch, Epoch::new(42));
        let chain = topology.chain_id().expect("clustered chain id");
        assert_eq!(chain.primary_node, chain_node_id(7));
        assert_eq!(chain.follower_node, chain_node_id(8));
        assert_eq!(chain.epoch, 42);
    }

    #[test]
    fn follower_topology_has_no_gateway_role() {
        let mut cli = cli(None);
        cli.node_id = Some(8);
        cli.chain_epoch = Some(42);
        cli.chain_primary = Some(7);
        cli.chain_listen = Some("127.0.0.1:3001".parse().unwrap());

        let topology = resolve_topology(&cli).expect("follower topology");
        assert!(matches!(topology.role, TopologyRole::Follower { .. }));
        assert!(topology.follower().is_none());
        let chain = topology.chain_id().expect("clustered chain id");
        assert_eq!(chain.primary_node, chain_node_id(7));
        assert_eq!(chain.follower_node, chain_node_id(8));
    }

    #[test]
    fn promotion_is_explicitly_tied_to_its_failed_primary() {
        let mut cli = cli(Some(PathBuf::from("/tmp/fdb.cluster")));
        cli.node_id = Some(8);
        cli.chain_epoch = Some(2);
        cli.chain_primary = Some(7);
        cli.chain_listen = Some("127.0.0.1:3001".parse().unwrap());
        cli.promote_from = Some(7);

        let topology = resolve_topology(&cli).expect("promotion topology");
        assert!(matches!(
            topology.role,
            TopologyRole::Promotion { primary: 7, .. }
        ));
        assert_eq!(topology.chain_id_at(1).unwrap().epoch, 1);
    }

    #[tokio::test]
    async fn ordinary_startup_refuses_a_foreign_fence_owner() {
        let mut cli = cli(None);
        cli.node_id = Some(2);
        cli.chain_epoch = Some(2);
        cli.chain_follower = vec![ChainFollower {
            node_id: 3,
            addr: "127.0.0.1:3001".parse().unwrap(),
        }];
        let topology = resolve_topology(&cli).unwrap();
        let store = orrery_persistd::MemFenceStore::new();
        let first = store
            .activate_shards(
                GridId::ROOT,
                1,
                &[ShardActivation {
                    shard: CellId::ROOT,
                    expected: None,
                }],
            )
            .await
            .unwrap();
        assert!(matches!(first, ActivationOutcome::Activated { .. }));
        let err = activate_topology(&topology, &store, false)
            .await
            .expect_err("stale former primary must be rejected");
        assert!(err.to_string().contains("owned by node 1"));
    }

    /// A [`FenceStore`] whose `read` takes measurable time and records how
    /// many reads were ever in flight at once. Everything else delegates to
    /// the in-memory store, so the activation itself behaves normally.
    struct ObservedFenceStore {
        inner: orrery_persistd::MemFenceStore,
        read_delay: Duration,
        in_flight: std::sync::atomic::AtomicUsize,
        max_in_flight: std::sync::atomic::AtomicUsize,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl ObservedFenceStore {
        fn new(read_delay: Duration) -> Self {
            Self {
                inner: orrery_persistd::MemFenceStore::new(),
                read_delay,
                in_flight: std::sync::atomic::AtomicUsize::new(0),
                max_in_flight: std::sync::atomic::AtomicUsize::new(0),
                reads: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl FenceStore for ObservedFenceStore {
        async fn read(
            &self,
            grid: GridId,
            shard: CellId,
        ) -> Result<Option<FenceRow>, orrery_persistd::FenceError> {
            use std::sync::atomic::Ordering;
            self.reads.fetch_add(1, Ordering::Relaxed);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.read_delay).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.inner.read(grid, shard).await
        }

        async fn fence(
            &self,
            grid: GridId,
            shard: CellId,
            expected: Option<&orrery_persistd::FenceRow>,
            new: &orrery_persistd::FenceRow,
        ) -> Result<orrery_persistd::FenceOutcome, orrery_persistd::FenceError> {
            self.inner.fence(grid, shard, expected, new).await
        }

        async fn activate_shards(
            &self,
            grid: GridId,
            owner: u64,
            shards: &[ShardActivation],
        ) -> Result<ActivationOutcome, orrery_persistd::FenceError> {
            self.inner.activate_shards(grid, owner, shards).await
        }

        async fn begin_split(
            &self,
            grid: GridId,
            parent: CellId,
            parent_expected: &orrery_persistd::FenceRow,
            children: &[(CellId, orrery_persistd::FenceRow)],
        ) -> Result<orrery_persistd::FenceOutcome, orrery_persistd::FenceError> {
            self.inner
                .begin_split(grid, parent, parent_expected, children)
                .await
        }

        async fn retire(
            &self,
            grid: GridId,
            shard: CellId,
        ) -> Result<(), orrery_persistd::FenceError> {
            self.inner.retire(grid, shard).await
        }
    }

    fn lattice(count: usize) -> Vec<ShardSpec> {
        (0..count)
            .map(|i| {
                let x = (i % 16) as i32;
                let y = ((i / 16) % 16) as i32;
                let z = (i / 256) as i32;
                ShardSpec(
                    CellId::from_coords(IVec3::new(x, y, z), orrery_protocol::SHARD_LEVEL)
                        .expect("in range"),
                )
            })
            .collect()
    }

    /// Startup must read the shard set's fence rows concurrently.
    ///
    /// It read them one at a time, so durable activation cost one full store
    /// round trip per shard — 128 of them in the deployment P2 describes, and
    /// linearly more beyond it. The guarded property is *overlap*: with a
    /// 20 ms read, a sequential pre-read of 64 shards cannot finish in under
    /// 1.28 s and can never have two reads in flight.
    ///
    /// The concurrency observation is the primary assertion because it is not
    /// a timing threshold; the elapsed bound is a generous backstop (a
    /// concurrent pre-read is ~40 ms of sleep here) that stays true on a
    /// loaded box.
    #[tokio::test]
    async fn startup_reads_the_shard_set_concurrently() {
        use std::sync::atomic::Ordering;

        let mut cli = cli(None);
        cli.node_id = Some(1);
        cli.shard = lattice(64);
        let topology = resolve_topology(&cli).expect("single-node topology");
        assert_eq!(topology.shards.len(), 64);

        let store = ObservedFenceStore::new(Duration::from_millis(20));
        let started = Instant::now();
        let activation = activate_topology(&topology, &store, false)
            .await
            .expect("cold activation");
        let elapsed = started.elapsed();

        assert_eq!(activation.epoch.0, 1);
        assert_eq!(
            store.reads.load(Ordering::SeqCst),
            64,
            "every shard is still pre-read exactly once"
        );
        assert!(
            store.max_in_flight.load(Ordering::SeqCst) > 1,
            "the pre-read is sequential: never more than one read in flight"
        );
        assert!(
            store.max_in_flight.load(Ordering::SeqCst) <= STARTUP_FENCE_READ_CONCURRENCY,
            "the pre-read must stay bounded, got {}",
            store.max_in_flight.load(Ordering::SeqCst)
        );
        assert!(
            elapsed < Duration::from_millis(1_000),
            "64 sequential 20 ms reads would take 1.28 s; took {elapsed:?}"
        );
    }

    /// Concurrency must not cost the deterministic refusal: the shard the
    /// error names is the first one *in shard order* that is owned elsewhere,
    /// not whichever read happened to land first.
    #[tokio::test]
    async fn a_foreign_owner_is_reported_for_the_first_shard_in_order() {
        let mut cli = cli(None);
        cli.node_id = Some(2);
        cli.shard = lattice(8);
        let topology = resolve_topology(&cli).expect("single-node topology");

        let store = orrery_persistd::MemFenceStore::new();
        // Two foreign owners, on the third and sixth shards in order.
        for (index, owner) in [(2_usize, 7_u64), (5, 9)] {
            let outcome = store
                .activate_shards(
                    GridId::ROOT,
                    owner,
                    &[ShardActivation {
                        shard: topology.shards[index],
                        expected: None,
                    }],
                )
                .await
                .expect("seed a foreign row");
            assert!(matches!(outcome, ActivationOutcome::Activated { .. }));
        }

        let err = activate_topology(&topology, &store, false)
            .await
            .expect_err("a foreign owner must refuse startup");
        assert!(
            err.to_string().contains("owned by node 7"),
            "the first foreign shard in order must be the one reported: {err}"
        );
    }

    #[test]
    fn canonical_shard_set_is_order_independent_and_grid_scoped() {
        let one = CellId::from_bits(9).unwrap();
        let two = CellId::from_bits(3).unwrap();
        let root = canonical_shard_set(GridId::ROOT, &[one, two]);
        assert_eq!(root, canonical_shard_set(GridId::ROOT, &[two, one]));
        assert_ne!(root, canonical_shard_set(GridId::new(1), &[one, two]));
        assert_eq!(
            root,
            [
                1, 0, 0, 0, 0, // encoding version + root grid
                0, 0, 0, 2, // two cells
                0, 0, 0, 0, 0, 0, 0, 3, // sorted CellId bits
                0, 0, 0, 0, 0, 0, 0, 9,
            ]
        );
    }

    #[test]
    fn clustered_topology_requires_explicit_epoch() {
        let mut cli = cli(None);
        cli.node_id = Some(7);
        cli.chain_follower = vec![ChainFollower {
            node_id: 8,
            addr: "127.0.0.1:3001".parse().unwrap(),
        }];
        let err = resolve_topology(&cli).expect_err("epoch is a chain fence");
        assert!(err.to_string().contains("--chain-epoch is required"));
    }
}
