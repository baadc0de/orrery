//! The `persistd` reference binary (docs/10-crates.md §11, P2 gaps #2/#7).
//!
//! The current binary is a **single-node** harness: it connects to
//! FoundationDB when asked, checkpoints on the 20 s jittered cadence, serves
//! never-loaded cells from the cold store, keeps the same gateway [`NodeId`]
//! across a kill -9 (via `--secret-key`), and can host shard cells at the D16
//! 8×8×8-interest-cell granularity (`--shard-level`) instead of
//! [`CellId::ROOT`].
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
//! The demo path this enables: start the binary with `--secret-key`, load it,
//! `kill -9` the process, restart from the same FDB cluster and secret key, and
//! the world resumes (RPO 0 intents, bulk bounded by the journal window).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::Mutex;

use orrery_persistd::cluster::{ColdFallbackRouter, Router};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
#[cfg(feature = "fdb")]
use orrery_persistd::FdbIntentExecutor;
use orrery_persistd::{
    CellRuntime, CheckpointScheduler, FenceStore, GatewayConfig, GatewayServer, IntentExecutor,
    JournalConfig, MemCheckpointStore, RuntimeConfig,
};
use orrery_protocol::{CellId, Epoch, GridId};

type SharedExecutor = Arc<dyn IntentExecutor>;

/// Command-line configuration for the `persistd` binary.
#[derive(Debug, Parser)]
#[command(name = "persistd", about = "Orrery persistence cluster (P2)")]
struct Cli {
    /// Number of nodes requested.
    ///
    /// The current binary only supports `1`; `> 1` is rejected until a real
    /// node-to-node chain transport exists.
    #[arg(long, default_value_t = 1)]
    nodes: usize,

    /// Base directory for node journals. Node `i` uses `{dir}/node-{i}`.
    #[arg(long, default_value = "persistd-data")]
    dir: PathBuf,

    /// Retained for compatibility; the current binary is single-node only, so
    /// there is no distributed chain to disable yet.
    #[arg(long)]
    no_chain: bool,

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

    /// The shard-cell tree level for initial shard placement. Default 0
    /// (CellId::ROOT). Level 18 gives 8×8×8 interest-cell granularity per
    /// shard (D16). The binary hosts one cell at this level.
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

    if cli.nodes > 1 {
        anyhow::bail!(
            "--nodes {nodes} is not supported by the current persistd reference \
             binary. It uses an in-process MemChainTransport, not a real \
             node-to-node chain transport. Run with --nodes 1 until that \
             transport exists.",
            nodes = cli.nodes,
        );
    }

    // ── FoundationDB stores (optional) ──────────────────────────────────
    // The fence store gates the single runtime against the same durable rows.
    // When no FDB cluster file is given we fall back to in-memory stores
    // (MemFenceStore, MemCheckpointStore) which are not durable across process
    // death.
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

    let mut schedulers: Vec<CheckpointScheduler> = Vec::new();
    let node_dir = cli.dir.join("node-0");
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
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::clone(&fence_store),
    };
    // Recovery seeds actors from the same durable tier the checkpoint
    // scheduler writes: the checkpoint is the base, the journal the
    // delta (§3.4).
    let runtime = Arc::new(Mutex::new(CellRuntime::open(&config, &checkpoint_store)?));
    let runtime_for_shutdown = Arc::clone(&runtime);

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

    let gateway = GatewayServer::spawn(
        gateway_config(&cli, secret_key, |cluster_file, grid| {
            #[cfg(feature = "fdb")]
            {
                let path = cluster_file.display().to_string();
                let exec = FdbIntentExecutor::connect(&path, grid)?;
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
        })?,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(nodes: usize, fdb_cluster_file: Option<PathBuf>) -> Cli {
        Cli {
            nodes,
            dir: PathBuf::from("persistd-data"),
            no_chain: false,
            bind: "127.0.0.1:0".parse().expect("valid loopback bind"),
            fdb_cluster_file,
            secret_key: None,
            shard_level: 0,
        }
    }

    #[test]
    fn gateway_config_wires_executor_from_root_grid() {
        let seen = Arc::new(std::sync::Mutex::new(None::<(PathBuf, GridId)>));
        let seen_capture = Arc::clone(&seen);
        let cfg = gateway_config(
            &cli(1, Some(PathBuf::from("/tmp/fdb.cluster"))),
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
        let cfg = gateway_config(&cli(1, None), None, |_cluster_file, _grid| {
            panic!("executor factory should not be called without --fdb-cluster-file")
        })
        .expect("gateway config");

        assert!(cfg.executor.is_none(), "no FDB file leaves executor unset");
    }
}
