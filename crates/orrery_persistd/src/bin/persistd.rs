//! The multi-node `persistd` reference binary (docs/10-crates.md §11, P2 gaps
//! #2/#7).
//!
//! A thin wrapper over the [`orrery_persistd`] library harness: it reads a
//! cluster config, opens one [`CellRuntime`] per node, wires them into a
//! [`Cluster`] with rendezvous routing and chain replication, and serves the
//! gateway. This is deliberately thin — all logic lives in the library (D12).
//!
//! The demo path this enables: start the cluster, load it, `kill -9` every
//! node, restart from the same journals, and the world resumes (RPO 0 intents,
//! bulk bounded by the journal/replication window).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::Mutex;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, Cluster, GatewayConfig, GatewayServer, JournalConfig, Router, RuntimeConfig,
};
use orrery_protocol::{CellId, Epoch};

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

    /// Enable chain replication between nodes (default on).
    #[arg(long, default_value_t = true)]
    chain: bool,

    /// The gateway bind address.
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();

    // The demo hosts a single root shard on every node; splits add finer
    // shards at runtime. Each node gets its own journal directory.
    let shard = CellId::ROOT;
    let mut runtimes = HashMap::new();

    for i in 0..cli.nodes {
        let node_dir = cli.dir.join(format!("node-{i}"));
        std::fs::create_dir_all(&node_dir)?;
        let config = RuntimeConfig {
            shards: vec![shard],
            journal: JournalConfig {
                dir: node_dir,
                commit: GroupCommitConfig {
                    mode: AdaptiveCommitMode::Adaptive,
                    ..GroupCommitConfig::default()
                },
            },
            node_id: i as u64,
            epoch: Epoch::new(0),
            fence: Arc::new(orrery_persistd::MemFenceStore::new()),
        };
        let rt = CellRuntime::open(&config)?;
        runtimes.insert(i as u64, Arc::new(Mutex::new(rt)));
    }

    let chain = cli.chain.then(orrery_persistd::ChainConfig::default);
    let cluster = Cluster::new(runtimes, chain.as_ref());

    // The gateway routes by placement across the cluster.
    let router: Arc<dyn Router> = Arc::new(cluster);
    let gateway = GatewayServer::spawn(
        GatewayConfig {
            bind: cli.bind.parse()?,
            ..GatewayConfig::default()
        },
        router,
    )
    .await?;

    tracing::info!(
        nodes = cli.nodes,
        gateway = %gateway.id(),
        "persistd cluster up"
    );

    // Keep the process alive until interrupted.
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    gateway.shutdown().await;
    Ok(())
}
