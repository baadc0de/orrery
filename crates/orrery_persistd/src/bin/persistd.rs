//! The `persistd` reference binary (docs/10-crates.md §11, P2 gaps #2/#7).
//!
//! The current binary is a **single-node** harness: it connects to
//! FoundationDB when asked, checkpoints on the 20 s jittered cadence, serves
//! never-loaded cells from the cold store, keeps the same gateway [`NodeId`]
//! across a kill -9 (via `--secret-key`), and can host one or more explicit
//! local shard cells via `--shard`.
//!
//! When `--fdb-cluster-file` is set, the binary connects to FoundationDB for
//! checkpoint storage and fencing (D11 §6). Without it, it runs with in-memory
//! stores - suitable for development and tests that do not need the durable
//! tier.
//!
//! A static two-process topology is also supported: a primary owns the shard
//! actors and gateway, while its one follower owns only a mirrored journal and
//! the chain gRPC listener. The follower is deliberately outside the client
//! write path.
//!
//! On startup the binary prints the gateway's [`EndpointAddr`] as a single-line
//! JSON object on stdout (tracing stays on stderr) so a harness can find the
//! address. The two signals that trigger graceful shutdown are SIGTERM and
//! Ctrl-C. `--metrics-jsonl` appends primary-side `journal_commit_ms`
//! `sample_batch` records to a separate P2 artifact file; it never alters the
//! stdout readiness contract.
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
use std::time::Duration;

use clap::Parser;
use glam::IVec3;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use orrery_persistd::cluster::{ColdFallbackRouter, Router};
use orrery_persistd::journal::{
    spawn_chain, spawn_chain_grpc, AdaptiveCommitMode, ChainConfig, GroupCommitConfig, Journal,
};
use orrery_persistd::{
    ActivationOutcome, CellRuntime, CheckpointScheduler, DurableChainId, FenceFreshnessConfig,
    FenceStore, GatewayConfig, GatewayServer, GrpcChainTransport, IntentExecutor, JournalConfig,
    MemCheckpointStore, RuntimeConfig, ShardActivation,
};
#[cfg(feature = "fdb")]
use orrery_persistd::{FdbContext, FdbIntentExecutor};
use orrery_protocol::{CellId, Epoch, GridId};

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

    /// Hex-encoded iroh secret key, pinning the gateway's NodeId across runs.
    /// When absent a fresh identity is generated per boot.
    #[arg(long)]
    secret_key: Option<String>,

    /// Local shard specification. Accepts raw `CellId` bits (`0x...` or
    /// decimal) or coordinate form `x,y,z@level`.
    #[arg(long, value_name = "RAW|X,Y,Z@LEVEL")]
    shard: Vec<ShardSpec>,

    /// Append server-internal journal commit latency batches to this JSONL
    /// file. Only an active gateway node emits these records; chain followers
    /// remain passive and do not produce a D16 measurement stream.
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

/// Start a compact JSONL reporter for the active node's local journal.
///
/// The caller deliberately invokes this only after the follower early-return:
/// a passive follower mirrors records but must not be treated as the primary's
/// server-internal commit-latency source.
fn spawn_metrics_reporter(path: PathBuf, journal: Arc<Journal>) -> anyhow::Result<MetricsReporter> {
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let (shutdown, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let metrics = journal.commit_metrics();
        let mut cursor = metrics.snapshot();
        let mut interval = tokio::time::interval(METRICS_REPORT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut writer = BufWriter::new(file);

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = &mut stopped => {
                    if let Err(error) = write_journal_metric_batches(&mut writer, &metrics.delta(&mut cursor))
                        .and_then(|()| writer.flush())
                    {
                        tracing::warn!(path = %path.display(), %error, "failed to flush final journal metrics");
                    }
                    break;
                }
            }

            if let Err(error) =
                write_journal_metric_batches(&mut writer, &metrics.delta(&mut cursor))
                    .and_then(|()| writer.flush())
            {
                tracing::warn!(path = %path.display(), %error, "failed to write journal metrics; stopping reporter");
                break;
            }
        }
    });
    Ok(MetricsReporter { shutdown, task })
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
                "series": "journal_commit_ms",
                "value_us": sample.value_us,
                "count": sample.count,
            }),
        )
        .map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // tracing to stderr (D12) so stdout is reserved for the JSON address line.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let topology = resolve_topology(&cli)?;

    if matches!(topology.role, TopologyRole::Follower { .. }) {
        return run_follower(&cli, &topology).await;
    }
    if matches!(topology.role, TopologyRole::Promotion { .. }) && cli.fdb_cluster_file.is_none() {
        anyhow::bail!("--promote-from requires --fdb-cluster-file for durable fencing");
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
    let activation = activate_topology(
        &topology,
        fence_store.as_ref(),
        cli.fdb_cluster_file.is_some(),
    )
    .await?;

    // A promoted follower first makes its mirrored, provenance-checked prefix
    // locally replayable.  This happens after its fence CAS and before the
    // runtime opens, so readiness means recovery includes the promoted tail.
    let recovery_cutoff = if let Some(source_epoch) = activation.promoted_source_epoch {
        let journal = Journal::open(&JournalConfig {
            dir: node_dir.clone(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        })?;
        let history = journal.adopt_chain_history(
            topology
                .chain_id_at(source_epoch)
                .expect("promotion has a source chain identity"),
        )?;
        let cutoff = history.watermark().map(|lsn| format!("{lsn:?}"));
        journal.close().await?;
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
    let runtime = Arc::new(Mutex::new(CellRuntime::open(&config, &checkpoint_store)?));
    let runtime_for_shutdown = Arc::clone(&runtime);

    // FDB-backed active nodes must continuously revalidate the rows acquired
    // above.  Start this after recovery but before exposing a gateway, so a
    // failed first poll can only yield provisional bulk acknowledgements.
    let fence_freshness_monitor = if cli.fdb_cluster_file.is_some() {
        let guard = runtime.lock().await;
        Some(
            guard
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
    let metrics_reporter = if let Some(path) = cli.metrics_jsonl.clone() {
        let journal = {
            let guard = runtime.lock().await;
            Arc::clone(guard.journal())
        };
        Some(spawn_metrics_reporter(path, journal)?)
    } else {
        None
    };

    // Chain replication is intentionally downstream of the journal ack path.
    // The transport is lazy: an unavailable follower marks the chain degraded
    // but never prevents the primary from serving local durable writes.
    let chain_replicator = if let Some(chain) = topology.chain_id() {
        let follower = topology
            .follower()
            .expect("only primary topologies have a chain id")
            .node_id;
        let journal = {
            let guard = runtime.lock().await;
            Arc::clone(guard.journal())
        };
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
    // default 20 s jittered cadence (D16).
    let scheduler = orrery_persistd::spawn_checkpoint_scheduler(
        Arc::clone(&runtime),
        Arc::clone(&checkpoint_store),
        &orrery_persistd::checkpoint::CheckpointConfig::default(),
    );
    schedulers.push(scheduler);

    // Wrap the runtime in a cold-fallback router when FDB is available.
    let router: Arc<dyn Router> = if let Some(cold) = cold_store {
        Arc::new(ColdFallbackRouter::new(Arc::clone(&runtime), cold))
    } else {
        // Without FDB there is no cold fallback; the runtime itself is the Router.
        runtime.clone()
    };
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

    let mut gateway_config = gateway_config(&cli, secret_key, |cluster_file, grid| {
        #[cfg(feature = "fdb")]
        {
            let _ = cluster_file;
            let context = fdb_context
                .as_ref()
                .expect("FDB context exists when --fdb-cluster-file is set");
            let exec = FdbIntentExecutor::from_context(context, grid);
            Ok(Some(Arc::new(exec) as SharedExecutor))
        }
        #[cfg(not(feature = "fdb"))]
        {
            let _ = (cluster_file, grid);
            anyhow::bail!(
                "persistd was compiled without the `fdb` feature; \
                     --fdb-cluster-file requires libfdb_c"
            );
        }
    })?;
    if let Some(monitor) = &fence_freshness_monitor {
        gateway_config.bulk_ack_admission = monitor.clone();
    }
    let gateway = GatewayServer::spawn(gateway_config, router).await?;

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
            "recovery_cutoff": recovery_cutoff,
            "bulk_ack_fence_monitor": fence_freshness_monitor.is_some(),
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
    let Ok(mutex) = Arc::try_unwrap(runtime_for_shutdown) else {
        tracing::warn!("runtime Arc still referenced during shutdown");
        return Ok(());
    };
    if let Err(e) = mutex.into_inner().close().await {
        tracing::warn!(error = %e, "journal close error during shutdown");
    }

    tracing::info!("persistd shutdown complete");
    Ok(())
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

    // stdout is a one-line machine-readable readiness contract. Unlike a
    // primary, the follower has no client endpoint to advertise.
    {
        use std::io::Write;
        let json = serde_json::json!({
            "node_id": topology.node_id,
            "chain_addr": server.addr().to_string(),
            "role": topology.role.name(),
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
    journal.close().await?;
    tracing::info!("persistd chain follower shutdown complete");
    Ok(())
}

#[derive(Debug)]
struct StartupActivation {
    epoch: Epoch,
    /// The old primary epoch whose chain identity must be adopted.
    promoted_source_epoch: Option<u64>,
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
    let mut requests = Vec::with_capacity(topology.shards.len());
    let mut source_epoch = None;
    for &shard in &topology.shards {
        let current = fence.read(GridId::ROOT, shard).await?;
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
    })
}

fn gateway_config<F>(
    cli: &Cli,
    secret_key: Option<iroh::SecretKey>,
    mut make_executor: F,
) -> anyhow::Result<GatewayConfig>
where
    F: FnMut(&std::path::Path, GridId) -> anyhow::Result<Option<SharedExecutor>>,
{
    let executor = if let Some(cluster_file) = cli.fdb_cluster_file.as_deref() {
        make_executor(cluster_file, GridId::ROOT)?
    } else {
        None
    };

    Ok(GatewayConfig {
        bind: cli.bind.parse::<SocketAddr>()?,
        secret_key,
        executor,
        ..GatewayConfig::default()
    })
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

fn resolve_shards(shards: &[ShardSpec]) -> anyhow::Result<Vec<CellId>> {
    let shards: Vec<CellId> = if shards.is_empty() {
        vec![CellId::ROOT]
    } else {
        shards.iter().map(|spec| spec.0).collect()
    };

    validate_shards(&shards)?;
    Ok(shards)
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

fn validate_shards(shards: &[CellId]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for &shard in shards {
        if !seen.insert(shard) {
            anyhow::bail!("duplicate --shard {shard}");
        }
    }

    for (idx, &left) in shards.iter().enumerate() {
        for &right in &shards[idx + 1..] {
            if left.is_prefix_of(right) || right.is_prefix_of(left) {
                anyhow::bail!("overlapping --shard values: {left} and {right}");
            }
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
            secret_key: None,
            shard: Vec::new(),
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
    fn gateway_config_wires_executor_from_root_grid() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(PathBuf, GridId)>));
        let seen_capture = Arc::clone(&seen);
        let cfg = gateway_config(
            &cli(Some(PathBuf::from("/tmp/fdb.cluster"))),
            None,
            move |cluster_file, grid| {
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
        let cfg = gateway_config(&cli(None), None, |_cluster_file, _grid| {
            panic!("executor factory should not be called without --fdb-cluster-file")
        })
        .expect("gateway config");

        assert!(cfg.executor.is_none(), "no FDB file leaves executor unset");
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
