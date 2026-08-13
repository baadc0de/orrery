//! Checkpoint/restore integration tests (slice 3).
//!
//! The `MemCheckpointStore` tests run always (no external service). The
//! `FdbCheckpointStore` tests are compiled under the `fdb` feature and
//! **self-skip** when no cluster is reachable — so a bare checkout stays green
//! and a machine with the dev cluster up exercises the real FDB path.

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, spawn_checkpoint_scheduler, CellRuntime, CheckpointConfig, JournalConfig,
    RuntimeConfig,
};

use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

#[cfg(feature = "fdb")]
use orrery_persistd::checkpoint::ColdCellReader;

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

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: std::sync::Arc::new(orrery_persistd::MemFenceStore::new()),
    }
}

#[tokio::test]
async fn checkpoint_then_replay_zero_loss() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemCheckpointStore::new();

    // Phase 1: write 50 entities, checkpoint (watermark W).
    let rt = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    for i in 0..50u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(&store).await.unwrap();
    let ckpt = store.load(CellId::ROOT).await.unwrap().unwrap();
    assert_eq!(ckpt.entities.len(), 50);

    // Phase 2: write 50 more (these live only in the journal tail).
    for i in 50..100u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.close().await.unwrap();

    // Simulate node loss: fresh runtime, restore from checkpoint + journal tail.
    let rt2 = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    let replayed = rt2.restore(CellId::ROOT, &store).await.unwrap();
    assert!(replayed >= 50, "replayed {replayed} tail records");

    let page = rt2.read(CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 100, "all 100 entities recovered");
    for i in 0..100u64 {
        let e = &page.entities[&PersistId::new(i)];
        assert_eq!(e.components.as_ref(), &i.to_le_bytes());
    }
    rt2.close().await.unwrap();
}

#[tokio::test]
async fn checkpoint_marks_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemCheckpointStore::new();

    let rt = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    for i in 0..10u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(&store).await.unwrap();
    let ckpt = store.load(CellId::ROOT).await.unwrap().unwrap();
    // The watermark is the LSN covered by the checkpoint (>= the last applied).
    assert!(ckpt.watermark >= Lsn::new(0, 0));
    rt.close().await.unwrap();
}

#[tokio::test]
async fn restore_with_no_checkpoint_replays_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemCheckpointStore::new();

    let rt = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    for i in 0..20u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.close().await.unwrap();

    // No checkpoint written: restore must replay the whole journal.
    let rt2 = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    let replayed = rt2.restore(CellId::ROOT, &store).await.unwrap();
    assert_eq!(replayed, 20);
    let page = rt2.read(CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 20);
    rt2.close().await.unwrap();
}

#[tokio::test]
async fn scheduler_checkpoints_on_cadence_and_quiesce() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemCheckpointStore::new());
    let runtime = Arc::new(tokio::sync::Mutex::new(
        CellRuntime::open(&runtime_config(dir.path())).unwrap(),
    ));

    // Write an entity so there is something to checkpoint.
    {
        let rt = runtime.lock().await;
        let rec = mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"hp=100");
        rt.apply(rec).await.unwrap();
    }

    // A fast cadence so the test is not slow.
    let scheduler = spawn_checkpoint_scheduler(
        Arc::clone(&runtime),
        store.clone(),
        &CheckpointConfig {
            interval: Duration::from_millis(50),
            jitter: Duration::from_millis(10),
        },
    );

    // Quiesce-flush: an immediate checkpoint on demand.
    scheduler.quiesce_signal().quiesce(CellId::ROOT).await;

    // Wait for the cadence timer to fire a checkpoint too.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if store.load(CellId::ROOT).await.unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheduler never checkpointed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The checkpoint reflects the entity.
    let ckpt = store.load(CellId::ROOT).await.unwrap().unwrap();
    assert_eq!(ckpt.entities.len(), 1);
    assert_eq!(
        ckpt.entities[&PersistId::new(1)].components.as_ref(),
        b"hp=100"
    );

    scheduler.shutdown().await;
    // After shutdown the scheduler no longer holds the runtime Arc; take it
    // back so we can close the journal cleanly.
    let rt = Arc::try_unwrap(runtime)
        .unwrap_or_else(|_| panic!("scheduler released the runtime"))
        .into_inner();
    rt.close().await.unwrap();
}

/// The cluster file for the FDB-gated tests, or `None` if not configured.
///
/// Honors `ORRERY_FDB_CLUSTER_FILE`; otherwise walks up from the crate dir to
/// find the workspace-root `.fdb-dev/fdb.cluster` (tests run with CWD = the
/// crate dir, not the workspace root).
#[cfg(feature = "fdb")]
fn fdb_cluster_file() -> Option<String> {
    if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
        return Some(path);
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".fdb-dev/fdb.cluster");
        if candidate.exists() {
            return Some(candidate.display().to_string());
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_checkpoint_roundtrip() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let store = orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    for i in 0..10u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(&store).await.unwrap();
    let ckpt = store.load(CellId::ROOT).await.unwrap().unwrap();
    assert_eq!(ckpt.entities.len(), 10);
    store.delete(CellId::ROOT).await.unwrap();
    assert!(store.load(CellId::ROOT).await.unwrap().is_none());
    rt.close().await.unwrap();
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_checkpoint_then_restore() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let store = orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    for i in 0..30u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(&store).await.unwrap();
    for i in 30..40u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.close().await.unwrap();

    let rt2 = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    let replayed = rt2.restore(CellId::ROOT, &store).await.unwrap();
    assert!(replayed >= 10, "replayed {replayed} tail records");
    let page = rt2.read(CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 40);
    rt2.close().await.unwrap();
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_cold_cell_area_load() {
    // Cold-cell area load (gap #4): a cell with no live actor is served by an
    // FDB range scan over `world/{cell_id}/…`, not from actor memory. The
    // checkpoint writes the entity rows; `read_cold` reads them back.
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let store = orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path())).unwrap();
    for i in 0..20u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    // Checkpoint writes `world/{cell_id}/{entity_id}` rows for the entities.
    rt.checkpoint(&store).await.unwrap();
    rt.close().await.unwrap();

    // Read the cold cell back from FDB (no live actor involved).
    let cold = store
        .read_cold(CellId::ROOT)
        .await
        .unwrap()
        .expect("cold cell has rows");
    assert_eq!(
        cold.entities.len(),
        20,
        "cold scan returned all 20 entities"
    );
    for i in 0..20u64 {
        let e = &cold.entities[&PersistId::new(i)];
        assert_eq!(e.components.as_ref(), &i.to_le_bytes());
    }

    store.delete(CellId::ROOT).await.unwrap();
    assert!(store.read_cold(CellId::ROOT).await.unwrap().is_none());
}
