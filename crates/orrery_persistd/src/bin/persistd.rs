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
//! The process-topology flags (`--node-id`, `--chain-listen`,
//! `--chain-follower`) are parsed now so the eventual cross-process chain RPC
//! can land without changing the binary surface, but startup still rejects any
//! chain request until that transport exists.
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
use std::str::FromStr;
use std::sync::Arc;

use clap::Parser;
use glam::IVec3;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // tracing to stderr (D12) so stdout is reserved for the JSON address line.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let topology = resolve_topology(&cli)?;

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
    let mut schedulers: Vec<CheckpointScheduler> = Vec::new();
    let node_dir = cli.dir.join(format!("node-{}", topology.node_id));
    std::fs::create_dir_all(&node_dir)?;

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
        node_id = topology.node_id,
        shards = topology.shards.len(),
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
}

fn resolve_topology(cli: &Cli) -> anyhow::Result<Topology> {
    let shards = resolve_shards(&cli.shard)?;
    let node_id = cli.node_id.unwrap_or(0);

    if cli.chain_listen.is_some() || !cli.chain_follower.is_empty() {
        if cli.node_id.is_none() {
            anyhow::bail!(
                "--node-id is required when any --chain-* flags are set; chain RPC is not wired into persistd yet"
            );
        }
        validate_followers(&cli.chain_follower, node_id)?;
        anyhow::bail!(
            "chain topology is not wired into persistd yet; remove --chain-listen/--chain-follower and run single-node"
        );
    }

    validate_followers(&cli.chain_follower, node_id)?;

    Ok(Topology { node_id, shards })
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
            chain_follower: Vec::new(),
            bind: "127.0.0.1:0".parse().expect("valid loopback bind"),
            fdb_cluster_file,
            secret_key: None,
            shard: Vec::new(),
        }
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
}
