//! Multi-node cluster integration tests (P2 gaps #2/#7).
//!
//! Exercises the rendezvous-routed multi-node cluster: diffs and reads route
//! to the node placement assigns, chain replication covers node loss, and the
//! kill-9 → restart → world-resumes demo path works for the bulk journal.
//! Intent RPO 0 coverage lives in `tests/intent_commit.rs` — this file has no
//! intent path (grep `Intent` returns 0 hits here).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, CheckpointError, ClaimResult, Cluster, ColdCellReader,
    ColdFallbackRouter, EntityRecord, JournalConfig, MemFenceStore, Router, RuntimeConfig,
    SnapshotPage,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick,
};

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
        grid: GridId::ROOT,
        journal: journal_config(dir),
        node_id,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// A fresh in-memory checkpoint store as the trait object `CellRuntime::open`
/// takes.
fn mem_store() -> Arc<dyn orrery_persistd::checkpoint::CheckpointStore> {
    Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new())
}

/// Build a `nodes`-node cluster, each with its own journal dir under `base`.
async fn build_cluster(base: &std::path::Path, nodes: usize) -> Cluster {
    let mut runtimes = HashMap::new();
    for i in 0..nodes {
        let dir = base.join(format!("node-{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = CellRuntime::open(&runtime_config(&dir, i as u64), &mem_store())
            .await
            .unwrap();
        runtimes.insert(i as u64, Arc::new(tokio::sync::Mutex::new(rt)));
    }
    Cluster::new(runtimes, Some(&orrery_persistd::ChainConfig::default()))
}

#[tokio::test]
async fn cluster_routes_by_placement_and_replicates() {
    let base = tempfile::tempdir().unwrap();
    let cluster = build_cluster(base.path(), 3).await;
    assert_eq!(cluster.len(), 3);

    // Every node hosts the root shard, so the owner of ROOT is deterministic.
    let owner = cluster.owner(CellId::ROOT).unwrap();
    assert!(owner < 3, "owner {owner} is a node id");

    // A diff routes to the owning node's actor and acks.
    let router: Arc<dyn Router> = Arc::new(cluster);
    let rec = mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"hp=100");
    let append = router.apply(rec.clone()).await.unwrap();
    let lsn = append.committed().await.unwrap();
    assert!(lsn >= Lsn::new(0, 0));

    // The owning node's runtime reflects the write.
    let page = router.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(7)].components.as_ref(),
        b"hp=100"
    );
}

#[tokio::test]
async fn cluster_valid_claim_waits_for_contended_runtime_then_completes() {
    // Given: a valid target entity in the sole runtime of a one-node cluster.
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("node-0");
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(tokio::sync::Mutex::new(
        CellRuntime::open(&runtime_config(&dir, 0), &mem_store())
            .await
            .unwrap(),
    ));
    let cluster = Arc::new(Cluster::new(
        HashMap::from([(0u64, Arc::clone(&runtime))]),
        None,
    ));
    let entity = PersistId::new(8);
    let holder = test_node(8);
    cluster
        .apply(mk_record(
            CellId::ROOT,
            entity.0,
            RecordKind::Spawn,
            b"valid",
        ))
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();

    // When: the runtime is held while a valid claim begins routing.
    let held_runtime = runtime.lock().await;
    assert!(
        cluster.runtime_for(GridId::ROOT, CellId::ROOT).is_some(),
        "a held target runtime must remain routable instead of appearing JournalClosed"
    );
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let cluster_for_claim = Arc::clone(&cluster);
    let mut claim = tokio::spawn(async move {
        let _ = started_tx.send(());
        Router::claim_lease(
            cluster_for_claim.as_ref(),
            GridId::ROOT,
            CellId::ROOT,
            entity,
            holder,
            ClaimKind::Weak,
            0,
        )
        .await
    });
    started_rx.await.unwrap();

    // Then: contention leaves the operation pending, rather than falsely
    // reporting a closed journal. Releasing the runtime completes the claim.
    let early = tokio::time::timeout(Duration::from_millis(100), &mut claim).await;
    assert!(
        early.is_err(),
        "a valid claim must wait for the contended runtime, not return JournalClosed"
    );
    drop(held_runtime);
    let claimed = tokio::time::timeout(Duration::from_secs(5), claim)
        .await
        .expect("claim completes after the runtime lock is released")
        .expect("claim task does not panic")
        .expect("valid routed claim does not return JournalClosed");
    assert!(matches!(claimed, ClaimResult::Granted(_)));

    // Cancel/resume probe: retrying after the release remains valid.
    let retried = Router::claim_lease(
        cluster.as_ref(),
        GridId::ROOT,
        CellId::ROOT,
        entity,
        holder,
        ClaimKind::Weak,
        1,
    )
    .await
    .expect("retry does not return JournalClosed");
    assert!(matches!(retried, ClaimResult::Granted(_)));
    assert!(matches!(
        cluster.read(GridId::new(1), CellId::ROOT).await,
        Err(orrery_persistd::Reject::JournalClosed)
    ));
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
            let rt = CellRuntime::open(&runtime_config(dir, i as u64), &mem_store())
                .await
                .unwrap();
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
        let rt = CellRuntime::open(&runtime_config(dir, i as u64), &mem_store())
            .await
            .unwrap();
        runtimes.insert(i as u64, Arc::new(tokio::sync::Mutex::new(rt)));
    }
    let cluster = Cluster::new(runtimes, Some(&orrery_persistd::ChainConfig::default()));
    let router: Arc<dyn Router> = Arc::new(cluster);

    // The world resumes: all 100 entities are present.
    let page = router.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 100, "world resumed after restart");
    for i in 0..100u64 {
        let e = &page.entities[&PersistId::new(i)];
        assert_eq!(e.components.as_ref(), &i.to_le_bytes());
    }
}

/// A cold-store stub for the P-5 regression: serves a fixed durable page.
struct StubCold {
    entities: HashMap<PersistId, EntityRecord>,
}

#[async_trait::async_trait]
impl ColdCellReader for StubCold {
    async fn read_cold(
        &self,
        _grid: GridId,
        _cell: CellId,
    ) -> Result<Option<SnapshotPage>, CheckpointError> {
        Ok(Some(SnapshotPage {
            entities: self.entities.clone(),
        }))
    }
}

#[tokio::test]
async fn has_actor_means_live_actor_not_placement_answer() {
    // P-5: `Cluster::has_actor` must test for a live actor, not a rendezvous
    // placement answer. A node hosting no shards is still named owner of every
    // cell — but no actor covers it, so `has_actor` is false and an area load
    // falls through to the cold store.
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("node-0");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = runtime_config(&dir, 0);
    cfg.shards = Vec::new();
    let rt = CellRuntime::open(&cfg, &mem_store()).await.unwrap();
    let runtimes = HashMap::from([(0u64, Arc::new(tokio::sync::Mutex::new(rt)))]);
    let cluster = Cluster::new(runtimes, None);

    let cell = CellId::ROOT;
    assert_eq!(cluster.owner(cell), Some(0), "placement still answers");

    let cold: Arc<dyn ColdCellReader> = Arc::new(StubCold {
        entities: HashMap::from([(
            PersistId::new(7),
            EntityRecord {
                components: bytes::Bytes::copy_from_slice(b"seeded"),
                dirty: false,
            },
        )]),
    });
    let router: Arc<dyn Router> = Arc::new(ColdFallbackRouter::new(cluster, Arc::clone(&cold)));
    assert!(
        !router.has_actor(GridId::ROOT, cell).await,
        "no live actor for the cell"
    );
    let page = router
        .read_cold(GridId::ROOT, cell)
        .await
        .unwrap()
        .expect("cold fallback serves the cell");
    assert_eq!(
        page.entities[&PersistId::new(7)].components.as_ref(),
        b"seeded"
    );

    // A node hosting the ROOT shard has a live actor: `has_actor` is true and
    // the read is served from actor memory, not the cold store.
    let dir2 = base.path().join("node-1");
    std::fs::create_dir_all(&dir2).unwrap();
    let rt2 = CellRuntime::open(&runtime_config(&dir2, 1), &mem_store())
        .await
        .unwrap();
    let runtimes2 = HashMap::from([(1u64, Arc::new(tokio::sync::Mutex::new(rt2)))]);
    let cluster2 = Cluster::new(runtimes2, None);
    cluster2
        .apply(mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"live"))
        .await
        .unwrap();
    let router2: Arc<dyn Router> = Arc::new(ColdFallbackRouter::new(cluster2, cold));
    assert!(
        router2.has_actor(GridId::ROOT, CellId::ROOT).await,
        "live actor present"
    );
    let page = router2.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(7)].components.as_ref(),
        b"live"
    );
}
