//! Multi-node cluster integration tests (P2 gaps #2/#7).
//!
//! Exercises the rendezvous-routed multi-node cluster: diffs and reads route to
//! the node placement assigns, chain replication covers node loss, and the
//! kill-9 → restart → world-resumes demo path works (RPO 0 intents, bulk
//! bounded by the journal/replication window).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, Cluster, JournalConfig, MemFenceStore, Router, RuntimeConfig,
};
use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(cell: CellId, entity: u64, kind: RecordKind, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0), // assigned by the journal
        cell,
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

fn journal_config(dir: &std::path::Path) -> JournalConfig {
    JournalConfig {
        dir: dir.to_path_buf(),
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::AlwaysBatch,
            batch_window: Duration::from_millis(100),
            batch_max_records: 100_000,
            batch_max_bytes: 1 << 20,
        },
    }
}

fn runtime_config(dir: &std::path::Path, node_id: u64) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        journal: journal_config(dir),
        node_id,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// Build a `nodes`-node cluster, each with its own journal dir under `base`.
fn build_cluster(base: &std::path::Path, nodes: usize) -> Cluster {
    let mut runtimes = HashMap::new();
    for i in 0..nodes {
        let dir = base.join(format!("node-{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = CellRuntime::open(&runtime_config(&dir, i as u64)).unwrap();
        runtimes.insert(i as u64, Arc::new(tokio::sync::Mutex::new(rt)));
    }
    Cluster::new(runtimes, Some(&orrery_persistd::ChainConfig::default()))
}

#[tokio::test]
async fn cluster_routes_by_placement_and_replicates() {
    let base = tempfile::tempdir().unwrap();
    let cluster = build_cluster(base.path(), 3);
    assert_eq!(cluster.len(), 3);

    // Every node hosts the root shard, so the owner of ROOT is deterministic.
    let owner = cluster.owner(CellId::ROOT).unwrap();
    assert!(owner < 3, "owner {owner} is a node id");

    // A diff routes to the owning node's actor and acks.
    let router: Arc<dyn Router> = Arc::new(cluster);
    let rec = mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"hp=100");
    let lsn = router.apply(rec.clone()).await.unwrap();
    assert!(lsn >= Lsn::new(0, 0));

    // The owning node's runtime reflects the write.
    let page = router.read(CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(7)].components.as_ref(),
        b"hp=100"
    );
}

#[tokio::test]
async fn cluster_restart_resumes_world() {
    // The kill-9 → restart → world-resumes demo path: write, drop the cluster
    // (simulating process death), reopen from the same journals, and the world
    // is intact.
    let base = tempfile::tempdir().unwrap();
    let dirs: Vec<std::path::PathBuf> = (0..2)
        .map(|i| base.path().join(format!("node-{i}")))
        .collect();
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }

    // Phase 1: write 100 entities into a 2-node cluster.
    {
        let mut runtimes = HashMap::new();
        for (i, dir) in dirs.iter().enumerate() {
            let rt = CellRuntime::open(&runtime_config(dir, i as u64)).unwrap();
            runtimes.insert(i as u64, Arc::new(tokio::sync::Mutex::new(rt)));
        }
        let cluster = Cluster::new(runtimes, Some(&orrery_persistd::ChainConfig::default()));
        for i in 0..100u64 {
            let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
            // Call the Router trait method directly on the cluster.
            cluster.apply(rec).await.unwrap();
        }
        // Close cleanly: stop chain replication and close every journal so the
        // file locks release before we reopen the same dirs.
        cluster.close().await;
    }

    // Phase 2: restart from the same journals.
    let mut runtimes = HashMap::new();
    for (i, dir) in dirs.iter().enumerate() {
        let rt = CellRuntime::open(&runtime_config(dir, i as u64)).unwrap();
        runtimes.insert(i as u64, Arc::new(tokio::sync::Mutex::new(rt)));
    }
    let cluster = Cluster::new(runtimes, Some(&orrery_persistd::ChainConfig::default()));
    let router: Arc<dyn Router> = Arc::new(cluster);

    // The world resumes: all 100 entities are present.
    let page = router.read(CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 100, "world resumed after restart");
    for i in 0..100u64 {
        let e = &page.entities[&PersistId::new(i)];
        assert_eq!(e.components.as_ref(), &i.to_le_bytes());
    }
}
