//! The multi-node `persistd` reference binary (docs/10-crates.md §11, P2 gaps
//! #2/#7).
//!
//! A durable persistence node connecting to FoundationDB, checkpointing every
//! shard on the 20 s jittered cadence, serving never-loaded cells from the cold
//! store, keeping the same gateway [`NodeId`] across a kill -9 (via
//! `--secret-key`), and hosting shard cells at the D16 8×8×8-interest-cell
//! granularity (`--shard-level`) instead of [`CellId::ROOT`].
//!
//! When `--fdb-cluster-file` is set, the binary connects to FoundationDB for
//! checkpoint storage and fencing (D11 §6). Without it, it runs with in-memory
//! stores — suitable for development and tests that do not need the durable
//! tier.
//!
//! On startup the binary prints the gateway's [`EndpointAddr`] as a single-line
//! JSON object on stdout (tracing stays on stderr) so a harness can find the
//! address. The two signals that trigger graceful shutdown are SIGTERM and
//! Ctrl-C.
//!
//! The demo path this enables: start the cluster with `--secret-key`, load it,
//! `kill -9` every node, restart from the same FDB cluster and secret key, and
//! the world resumes (RPO 0 intents, bulk bounded by the journal/replication
//! window).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::Mutex;

use orrery_persistd::cluster::{ColdFallbackRouter, Router};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, CheckpointScheduler, Cluster, FenceStore, GatewayConfig, GatewayServer,
    JournalConfig, MemCheckpointStore, RuntimeConfig,
};
use orrery_protocol::{CellId, Epoch, GridId};

/// Command-line configuration for the `persistd` binary.
#[derive(Debug, Parser)]
#[command(name = "persistd", about = "Orrery persistence cluster (P2)")]
struct Cli {
    /// Number of nodes in the cluster.
    #[arg(long, default_value_t = 1)]
    nodes: usize,

    /// Base directory for node journals. Node `i` uses `{dir}/node-{i}`.
    #[arg(long, default_value = "persistd-data")]
    dir: PathBuf,

    /// Disable chain replication between nodes (default on).
    #[arg(long)]
    no_chain: bool,

    /// The gateway bind address.
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,

    /// FoundationDB cluster file path. When set, the binary connects to FDB for
    /// checkpoint storage and fencing; without it the in-memory stores are used.
    #[arg(long)]
    fdb_cluster_file: Option<PathBuf>,

    /// Hex-encoded iroh secret key, pinning the gateway's NodeId across runs.
    /// When absent a fresh identity is generated per boot.
    #[arg(long)]
    secret_key: Option<String>,

    /// The shard-cell tree level for initial shard placement. Default 0
    /// (CellId::ROOT). Level 18 gives 8×8×8 interest-cell granularity per
    /// shard (D16). The binary hosts one cell at this level per node.
    #[arg(long, default_value_t = 0)]
    shard_level: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // tracing to stderr (D12) so stdout is reserved for the JSON address line.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    // ── FoundationDB stores (optional) ──────────────────────────────────
    // The fence store is shared across every node in this process so they
    // all fence against the same durable rows. When no FDB cluster file is
    // given we fall back to in-memory stores (MemFenceStore, MemCheckpointStore)
    // which are not durable across process death.
    let fence_store: Arc<dyn FenceStore> = if let Some(ref cluster_file) = cli.fdb_cluster_file {
        #[cfg(feature = "fdb")]
        {
            let path = cluster_file.display().to_string();
            Arc::new(orrery_persistd::fence::FdbFenceStore::connect(&path)?)
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
            let path = cluster_file.display().to_string();
            let store = orrery_persistd::checkpoint::FdbCheckpointStore::connect(&path)?;
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
    let shard = if cli.shard_level == 0 {
        CellId::ROOT
    } else {
        CellId::from_coords(glam::IVec3::ZERO, cli.shard_level)
            .expect("shard_level produces a valid cell")
    };

    let mut runtimes: HashMap<u64, Arc<Mutex<CellRuntime>>> = HashMap::new();
    let mut schedulers: Vec<CheckpointScheduler> = Vec::new();

    for i in 0..cli.nodes {
        let node_dir = cli.dir.join(format!("node-{i}"));
        std::fs::create_dir_all(&node_dir)?;

        let config = RuntimeConfig {
            shards: vec![shard],
            grid: GridId::ROOT,
            journal: JournalConfig {
                dir: node_dir,
                commit: GroupCommitConfig {
                    mode: AdaptiveCommitMode::Adaptive,
                    ..GroupCommitConfig::default()
                },
            },
            node_id: i as u64,
            epoch: Epoch::new(0),
            fence: Arc::clone(&fence_store),
        };
        // Recovery seeds actors from the same durable tier the checkpoint
        // scheduler writes: the checkpoint is the base, the journal the
        // delta (§3.4).
        let rt = CellRuntime::open(&config, &checkpoint_store)?;
        let rt_arc = Arc::new(Mutex::new(rt));

        // Spawn one checkpoint scheduler per runtime, using the default 20 s
        // jittered cadence (D16).
        let scheduler = orrery_persistd::spawn_checkpoint_scheduler(
            Arc::clone(&rt_arc),
            Arc::clone(&checkpoint_store),
            &orrery_persistd::checkpoint::CheckpointConfig::default(),
        );
        schedulers.push(scheduler);

        runtimes.insert(i as u64, rt_arc);
    }

    // Keep a separate Arc for shutdown so we can close each runtime's journal
    // after the cluster is dropped.
    let runtimes_for_shutdown = runtimes.clone();

    // ── Cluster and routing ─────────────────────────────────────────────
    let chain = (!cli.no_chain).then(orrery_persistd::journal::ChainConfig::default);
    let cluster = Cluster::new(runtimes, chain.as_ref());

    // Wrap the cluster in a cold-fallback router when FDB is available.
    let router: Arc<dyn Router> = if let Some(cold) = cold_store {
        Arc::new(ColdFallbackRouter::new(cluster, cold))
    } else {
        // Without FDB there is no cold fallback; Cluster itself is the Router.
        Arc::new(cluster)
    };

    // ── Gateway ─────────────────────────────────────────────────────────
    let secret_key = cli
        .secret_key
        .as_deref()
        .map(|hex| {
            hex.parse::<iroh::SecretKey>()
                .map_err(|e| anyhow::anyhow!("invalid --secret-key (expected hex iroh key): {e}"))
        })
        .transpose()?;

    let gateway = GatewayServer::spawn(
        GatewayConfig {
            bind: cli.bind.parse()?,
            secret_key,
            ..GatewayConfig::default()
        },
        router,
    )
    .await?;

    // Print the gateway address as a single-line JSON object on stdout, so a
    // demo harness can parse it. Everything else goes to stderr via tracing.
    {
        let addr = gateway.addr();
        let node_id = gateway.id();
        let json = serde_json::json!({
            "endpoint_addr": format!("{addr:?}"),
            "node_id": format!("{node_id}"),
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
        nodes = cli.nodes,
        gateway = %gateway.id(),
        shard_level = cli.shard_level,
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

    // Graceful shutdown in reverse order: gateway → schedulers → cluster.
    tracing::info!("stopping gateway");
    gateway.shutdown().await;

    for scheduler in schedulers {
        scheduler.shutdown().await;
    }

    // The router (moved into GatewayServer::spawn) is released when the
    // gateway's accept loop stops, which releases the cluster's chain
    // replicators and runtime Arcs.

    // Close each runtime's journal explicitly.
    for (_, rt_arc) in runtimes_for_shutdown {
        let Ok(mutex) = Arc::try_unwrap(rt_arc) else {
            tracing::warn!("runtime Arc still referenced during shutdown");
            continue;
        };
        if let Err(e) = mutex.into_inner().close().await {
            tracing::warn!(error = %e, "journal close error during shutdown");
        }
    }

    tracing::info!("persistd shutdown complete");
    Ok(())
}
