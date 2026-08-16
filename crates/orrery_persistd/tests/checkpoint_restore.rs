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
#[cfg(feature = "fdb")]
use orrery_persistd::{FenceRow, FenceStatus, FenceStore};

/// Install the active ownership row required by FDB checkpoint writes. Tests
/// share a cluster, so retain an already-correct row for restart coverage.
#[cfg(feature = "fdb")]
async fn activate_fdb_checkpoint_fence(cluster: &str, grid: GridId) {
    activate_fdb_fence(cluster, grid, CellId::ROOT, 0, Epoch::new(0)).await;
}

#[cfg(feature = "fdb")]
async fn activate_fdb_fence(cluster: &str, grid: GridId, shard: CellId, owner: u64, epoch: Epoch) {
    let store = orrery_persistd::fence::FdbFenceStore::connect(cluster).unwrap();
    let expected = FenceRow {
        owner,
        epoch,
        status: FenceStatus::Active,
    };
    match store.read(grid, shard).await.unwrap() {
        Some(row) => assert_eq!(row, expected, "test grid has unexpected fence"),
        None => {
            assert!(matches!(
                store.fence(grid, shard, None, &expected).await.unwrap(),
                orrery_persistd::FenceOutcome::Fenced
            ));
        }
    }
}

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record_in(
    grid: GridId,
    cell: CellId,
    entity: u64,
    kind: RecordKind,
    payload: &[u8],
) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0), // assigned by the journal
        cell,
        grid,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

fn mk_record(cell: CellId, entity: u64, kind: RecordKind, payload: &[u8]) -> JournalRecord {
    mk_record_in(GridId::ROOT, cell, entity, kind, payload)
}

/// A fresh in-memory checkpoint store as the trait object `CellRuntime::open`
/// takes.
fn store() -> Arc<dyn CheckpointStore> {
    Arc::new(MemCheckpointStore::new())
}

/// Coerce a shared store into the trait object `CellRuntime::open` takes.
fn store_dyn<S: CheckpointStore + 'static>(store: &Arc<S>) -> Arc<dyn CheckpointStore> {
    store.clone()
}

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    runtime_config_in(dir, GridId::ROOT)
}

/// A runtime pinned to `grid`.
///
/// The fdb-gated tests below share one real database, and after P-3 a
/// `read_cold`/`delete` on `CellId::ROOT` covers that grid's whole subtree —
/// so tests keyed to the root cell in the root grid see each other's rows and
/// fail depending on order and `--test-threads`. P-7 made `GridId` a keyspace
/// discriminator, so each fdb test gets its own grid and they are disjoint by
/// construction.
fn runtime_config_in(dir: &std::path::Path, grid: GridId) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid,
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
async fn actor_serves_rows_present_only_in_the_checkpoint() {
    // The durable tier is the system of record (D11): a restart must serve at
    // least what the checkpoint holds, even when the journal never saw those
    // entities. `open` seeds each actor from the checkpoint store before it
    // serves a single read.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemCheckpointStore::new());

    // Seed the checkpoint directly: two entities the journal never saw.
    let mut entities = std::collections::HashMap::new();
    let mut by_cell = std::collections::HashMap::new();
    for i in 0..3u64 {
        entities.insert(
            PersistId::new(i),
            orrery_persistd::EntityRecord {
                components: bytes::Bytes::copy_from_slice(&i.to_le_bytes()),
                dirty: false,
            },
        );
        by_cell.insert(PersistId::new(i), CellId::ROOT);
    }
    store
        .checkpoint(&orrery_persistd::CheckpointData {
            shard: CellId::ROOT,
            grid: GridId::ROOT,
            node_id: 0,
            epoch: Epoch::new(0),
            watermark: Lsn::new(0, 0),
            entities,
            by_cell,
            tombstones: std::collections::HashMap::new(),
            superseded: std::collections::HashSet::new(),
            taken_at_ms: 1_700_000_000_000,
        })
        .await
        .unwrap();

    // Open with an EMPTY journal but the seeded checkpoint: the actor serves
    // the checkpoint's rows immediately (§3.4: checkpoint is the base).
    let rt = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities.len(),
        3,
        "the actor serves checkpoint-seeded rows the journal never saw"
    );
    for i in 0..3u64 {
        let e = &page.entities[&PersistId::new(i)];
        assert_eq!(e.components.as_ref(), &i.to_le_bytes());
    }
    rt.close().await.unwrap();
}

#[tokio::test]
async fn open_scans_the_journal_once_for_many_shards() {
    // The recovery pass is one journal scan, not `shards × journal`: opening
    // an 8-shard runtime over the same journal must complete in the same
    // order of time as a 1-shard open (the old nested loop scanned the whole
    // journal per shard).
    let dir = tempfile::tempdir().unwrap();

    // 512 records across the whole cell space, submitted concurrently so the
    // adaptive committer batches them instead of paying one batch window per
    // record.
    {
        let journal =
            Arc::new(orrery_persistd::Journal::open(&runtime_config(dir.path()).journal).unwrap());
        let mut handles = Vec::new();
        for i in 0..512u64 {
            let cell = CellId::from_coords(
                glam::IVec3::new((i % 64) as i32 - 32, (i % 8) as i32 - 4, 0),
                18,
            )
            .unwrap();
            let rec = mk_record(cell, i, RecordKind::Spawn, &i.to_le_bytes());
            handles.push(journal.append(rec).unwrap());
        }
        for h in handles {
            h.committed().await.unwrap();
        }
        journal.close().await.unwrap();
        drop(journal);
    }

    let one_shard = |shards: Vec<CellId>| async {
        let mut cfg = runtime_config(dir.path());
        cfg.shards = shards;
        let t0 = std::time::Instant::now();
        let rt = CellRuntime::open(&cfg, &store()).await.unwrap();
        let opened = t0.elapsed();
        (rt, opened)
    };

    let (rt1, t1) = one_shard(vec![CellId::ROOT]).await;
    let page = rt1.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 512, "1-shard open replays all");
    rt1.close().await.unwrap();

    let (rt8, t8) = one_shard(CellId::ROOT.children().to_vec()).await;
    let mut total = 0usize;
    for child in CellId::ROOT.children() {
        let page = rt8.read(GridId::ROOT, child).await.unwrap();
        total += page.entities.len();
    }
    assert_eq!(total, 512, "8-shard open replays all, partitioned");
    rt8.close().await.unwrap();

    assert!(
        t8 <= t1 * 2 + Duration::from_millis(500),
        "8-shard open ({t8:?}) stays within 2x the 1-shard open ({t1:?})"
    );
}

#[tokio::test]
async fn checkpoint_then_replay_zero_loss() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemCheckpointStore::new());

    // Phase 1: write 50 entities, checkpoint (watermark W).
    let rt = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    for i in 0..50u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(store.as_ref()).await.unwrap();
    let ckpt = store
        .load(CellId::ROOT, GridId::ROOT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ckpt.entities.len(), 50);

    // Phase 2: write 50 more (these live only in the journal tail).
    for i in 50..100u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.close().await.unwrap();

    // Simulate node loss: fresh runtime, restore from checkpoint + journal tail.
    let rt2 = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    let replayed = rt2.restore(CellId::ROOT, store.as_ref()).await.unwrap();
    assert_eq!(
        replayed, 50,
        "restore replays exactly the tail past the watermark"
    );

    let page = rt2.read(GridId::ROOT, CellId::ROOT).await.unwrap();
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
    let store = Arc::new(MemCheckpointStore::new());

    let rt = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    // Track the last acked LSN independently of the checkpoint path: the
    // apply ack carries it (the journal stamps it into the stored record).
    let mut last_lsn = Lsn::new(0, 0);
    for i in 0..10u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        last_lsn = rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(store.as_ref()).await.unwrap();
    let ckpt = store
        .load(CellId::ROOT, GridId::ROOT)
        .await
        .unwrap()
        .unwrap();
    // The watermark is exactly the LSN covered by the checkpoint: the last
    // applied record's real LSN (D11 §6 `ckpt/` is watermark-only).
    assert_eq!(ckpt.watermark, last_lsn, "watermark == last applied LSN");
    rt.close().await.unwrap();
}

#[tokio::test]
async fn despawn_tombstone_survives_checkpoint_restore_and_gc() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemCheckpointStore::new());

    let rt = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    for i in 0..10u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    // Despawn half; after the checkpoint the actor must hold a tombstone for
    // each (P-6) — the rows the checkpoint previously wrote are overwritten by
    // the marker, and a cold/area read must not resurrect them.
    for i in 0..5u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Despawn, b"");
        rt.apply(rec).await.unwrap();
    }
    let snap = rt.actor_snapshot(CellId::ROOT).await.unwrap();
    assert_eq!(snap.entities.len(), 5, "only the live half remains");
    assert_eq!(snap.tombstones.len(), 5, "one tombstone per despawn (P-6)");
    rt.checkpoint(store.as_ref()).await.unwrap();

    // Kill -9: restore from the checkpoint + journal tail. The despawned ids
    // must stay dead and their markers must be carried by the checkpoint.
    rt.close().await.unwrap();
    let rt2 = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    rt2.restore(CellId::ROOT, store.as_ref()).await.unwrap();
    let page = rt2.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities.len(),
        5,
        "despawned entities are not resurrected"
    );
    for i in 5..10u64 {
        assert!(page.entities.contains_key(&PersistId::new(i)));
    }
    for i in 0..5u64 {
        assert!(!page.entities.contains_key(&PersistId::new(i)));
    }
    let snap2 = rt2.actor_snapshot(CellId::ROOT).await.unwrap();
    assert_eq!(snap2.tombstones.len(), 5, "tombstones survive restore");

    // A later re-checkpoint on the live actor keeps writing fresh markers.
    rt2.checkpoint(store.as_ref()).await.unwrap();
    let ckpt = store
        .load(CellId::ROOT, GridId::ROOT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ckpt.entities.len(), 5);
    assert_eq!(ckpt.tombstones.len(), 5);
    rt2.close().await.unwrap();
}

#[tokio::test]
async fn restore_with_no_checkpoint_replays_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemCheckpointStore::new());

    let rt = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    for i in 0..20u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }
    rt.close().await.unwrap();

    // No checkpoint written: restore must replay the whole journal.
    let rt2 = CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
        .await
        .unwrap();
    let replayed = rt2.restore(CellId::ROOT, store.as_ref()).await.unwrap();
    assert_eq!(replayed, 20);
    let page = rt2.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 20);
    rt2.close().await.unwrap();
}

#[tokio::test]
async fn scheduler_checkpoints_on_cadence_and_quiesce() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemCheckpointStore::new());
    let runtime = Arc::new(tokio::sync::Mutex::new(
        CellRuntime::open(&runtime_config(dir.path()), &store_dyn(&store))
            .await
            .unwrap(),
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
        store_dyn(&store),
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
        if store
            .load(CellId::ROOT, GridId::ROOT)
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheduler never checkpointed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The checkpoint reflects the entity.
    let ckpt = store
        .load(CellId::ROOT, GridId::ROOT)
        .await
        .unwrap()
        .unwrap();
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
    activate_fdb_checkpoint_fence(&cluster, GridId::new(9001)).await;
    let store = std::sync::Arc::new(
        orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap(),
    );

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(
        &runtime_config_in(dir.path(), GridId::new(9001)),
        &store_dyn(&store),
    )
    .await
    .unwrap();
    for i in 0..10u64 {
        let rec = mk_record_in(
            GridId::new(9001),
            CellId::ROOT,
            i,
            RecordKind::Spawn,
            &i.to_le_bytes(),
        );
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(store.as_ref()).await.unwrap();
    let ckpt = store
        .load(CellId::ROOT, GridId::new(9001))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ckpt.entities.len(), 10);
    store.delete(CellId::ROOT, GridId::new(9001)).await.unwrap();
    assert!(store
        .load(CellId::ROOT, GridId::new(9001))
        .await
        .unwrap()
        .is_none());
    rt.close().await.unwrap();
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_checkpoint_then_restore() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    activate_fdb_checkpoint_fence(&cluster, GridId::new(9002)).await;
    let store = std::sync::Arc::new(
        orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap(),
    );

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(
        &runtime_config_in(dir.path(), GridId::new(9002)),
        &store_dyn(&store),
    )
    .await
    .unwrap();
    for i in 0..30u64 {
        let rec = mk_record_in(
            GridId::new(9002),
            CellId::ROOT,
            i,
            RecordKind::Spawn,
            &i.to_le_bytes(),
        );
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint(store.as_ref()).await.unwrap();
    for i in 30..40u64 {
        let rec = mk_record_in(
            GridId::new(9002),
            CellId::ROOT,
            i,
            RecordKind::Spawn,
            &i.to_le_bytes(),
        );
        rt.apply(rec).await.unwrap();
    }
    rt.close().await.unwrap();

    let rt2 = CellRuntime::open(
        &runtime_config_in(dir.path(), GridId::new(9002)),
        &store_dyn(&store),
    )
    .await
    .unwrap();
    let replayed = rt2.restore(CellId::ROOT, store.as_ref()).await.unwrap();
    assert!(replayed >= 10, "replayed {replayed} tail records");
    let page = rt2.read(GridId::new(9002), CellId::ROOT).await.unwrap();
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
    activate_fdb_checkpoint_fence(&cluster, GridId::new(9003)).await;
    let store = std::sync::Arc::new(
        orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap(),
    );

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(
        &runtime_config_in(dir.path(), GridId::new(9003)),
        &store_dyn(&store),
    )
    .await
    .unwrap();
    for i in 0..20u64 {
        let rec = mk_record_in(
            GridId::new(9003),
            CellId::ROOT,
            i,
            RecordKind::Spawn,
            &i.to_le_bytes(),
        );
        rt.apply(rec).await.unwrap();
    }
    // Checkpoint writes `world/{cell_id}/{entity_id}` rows for the entities.
    rt.checkpoint(store.as_ref()).await.unwrap();
    rt.close().await.unwrap();

    // Read the cold cell back from FDB (no live actor involved).
    let cold = store
        .read_cold(GridId::new(9003), CellId::ROOT)
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

    store.delete(CellId::ROOT, GridId::new(9003)).await.unwrap();
    assert!(store
        .read_cold(GridId::new(9003), CellId::ROOT)
        .await
        .unwrap()
        .is_none());
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_subtree_keying_and_watermark_only_checkpoint() {
    // P-2/P-3/P-8 regression, unmasked — the tests above read CellId::ROOT,
    // which *is* its own span, so they cannot catch the defects that the
    // seeder's acceptance gates depend on (docs/12-world-seeding.md §2).
    //
    //   * P-2: rows are keyed by the entity's own cell (`by_cell`), so a cold
    //     read of one interest cell returns exactly that cell's entities.
    //   * P-3: subtree spans, so `read_cold(shard)` serves the whole subtree
    //     and `delete(shard)` clears it (and only it).
    //   * P-8: the `ckpt/` value is the watermark only — `load` rebuilds the
    //     entity bag from the `world/` rows, and the recovery fields round-trip.
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    use orrery_persistd::{CheckpointData, EntityRecord};
    use std::collections::{HashMap, HashSet};

    // Level-18 shard from docs/01-spatial-model §3.3 (subtree
    // `0x...4C01..=0x...4FFF`) plus two level-21 cells under it and one outside.
    let shard = CellId::from_bits(0xA924_9249_2492_4E00).unwrap();
    let c1 = CellId::from_bits(0xA924_9249_2492_4D65).unwrap();
    let c2 = CellId::from_bits(0xA924_9249_2492_4D66).unwrap();
    let foreign = CellId::from_bits(0xA924_9249_2492_5200).unwrap();
    assert!(shard.is_prefix_of(c1) && shard.is_prefix_of(c2));
    assert!(
        !shard.is_prefix_of(foreign),
        "foreign cell is outside the shard"
    );

    let rec = |bytes: u64| EntityRecord {
        components: bytes::Bytes::copy_from_slice(&bytes.to_le_bytes()),
        dirty: false,
    };
    let entities = HashMap::from([
        (PersistId::new(1), rec(1)),
        (PersistId::new(2), rec(2)),
        (PersistId::new(3), rec(3)),
    ]);
    let by_cell = HashMap::from([
        (PersistId::new(1), c1),
        (PersistId::new(2), c2),
        (PersistId::new(3), foreign),
    ]);
    let data = CheckpointData {
        shard,
        grid: GridId::new(9004),
        node_id: 11,
        epoch: Epoch::new(5),
        watermark: Lsn::new(2, 4096),
        entities,
        by_cell,
        tombstones: HashMap::new(),
        superseded: HashSet::new(),
        taken_at_ms: 1_700_000_000_000,
    };
    activate_fdb_fence(&cluster, data.grid, data.shard, data.node_id, data.epoch).await;
    let store = std::sync::Arc::new(
        orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap(),
    );
    store.checkpoint(&data).await.unwrap();

    // P-8: load rebuilds the bag from rows; the watermark fields round-trip.
    let loaded = store
        .load(shard, GridId::new(9004))
        .await
        .unwrap()
        .expect("checkpoint present");
    assert_eq!(loaded.node_id, 11);
    assert_eq!(loaded.epoch, Epoch::new(5));
    assert_eq!(loaded.watermark, Lsn::new(2, 4096));
    assert_eq!(loaded.taken_at_ms, 1_700_000_000_000);
    // The foreign entity's row lives outside the shard subtree, so loading the
    // shard serves exactly its own entities.
    assert_eq!(loaded.entities.len(), 2, "shard load excludes foreign rows");
    assert!(loaded.entities.contains_key(&PersistId::new(1)));
    assert!(loaded.entities.contains_key(&PersistId::new(2)));
    assert!(loaded.by_cell.get(&PersistId::new(1)) == Some(&c1));
    assert!(loaded.by_cell.get(&PersistId::new(2)) == Some(&c2));

    // P-2: a cold read of one interest cell returns exactly that cell's rows.
    let page_c1 = store
        .read_cold(GridId::new(9004), c1)
        .await
        .unwrap()
        .expect("c1 has rows");
    assert_eq!(page_c1.entities.len(), 1);
    assert_eq!(
        page_c1.entities[&PersistId::new(1)].components.as_ref(),
        &1u64.to_le_bytes()
    );

    // P-3: reading the shard serves the whole subtree, spanning both cells.
    let page_shard = store
        .read_cold(GridId::new(9004), shard)
        .await
        .unwrap()
        .expect("shard subtree has rows");
    assert_eq!(page_shard.entities.len(), 2);

    // P-3: delete clears the shard's subtree but not the foreign row.
    store.delete(shard, GridId::new(9004)).await.unwrap();
    assert!(store
        .load(shard, GridId::new(9004))
        .await
        .unwrap()
        .is_none());
    assert!(store
        .read_cold(GridId::new(9004), shard)
        .await
        .unwrap()
        .is_none());
    let foreign_page = store
        .read_cold(GridId::new(9004), foreign)
        .await
        .unwrap()
        .expect("foreign row survives shard delete");
    assert_eq!(foreign_page.entities.len(), 1);
    assert!(foreign_page.entities.contains_key(&PersistId::new(3)));

    store.delete(foreign, GridId::new(9004)).await.unwrap();
    assert!(store
        .read_cold(GridId::new(9004), foreign)
        .await
        .unwrap()
        .is_none());
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_tombstones_write_gc_and_isolate_grids() {
    // P-6 + P-7 end-to-end on the durable tier:
    //   * a checkpoint writes tombstone rows for despawned entities and clears
    //     rows whose GC deadline passed (D11 §6 GC pass),
    //   * `read_cold` never surfaces a tombstone,
    //   * `load` rebuilds the tombstone set so recovery keeps the countdown,
    //   * the same (cell, entity) under two grids lives in disjoint rows, and
    //     `delete(grid, shard)` clears only its own grid's rows.
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    use orrery_persistd::{CheckpointData, EntityRecord, Tombstone};
    use std::collections::{HashMap, HashSet};

    let shard = CellId::from_bits(0xA924_9249_2492_4E00).unwrap();
    let cell = CellId::from_bits(0xA924_9249_2492_4D65).unwrap();
    let store = std::sync::Arc::new(
        orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap(),
    );
    let rec = |bytes: u64| EntityRecord {
        components: bytes::Bytes::copy_from_slice(&bytes.to_le_bytes()),
        dirty: false,
    };
    let tomb = |deadline: u64| Tombstone {
        cell,
        tick: Tick::new(9),
        gc_deadline_ms: deadline,
    };

    // P-6: a live entity plus an unexpired and an expired tombstone.
    let data = CheckpointData {
        shard,
        grid: GridId::new(9005),
        node_id: 1,
        epoch: Epoch::new(1),
        watermark: Lsn::new(2, 4096),
        entities: HashMap::from([(PersistId::new(1), rec(1))]),
        by_cell: HashMap::from([(PersistId::new(1), cell)]),
        tombstones: HashMap::from([
            (PersistId::new(2), tomb(1_900_000_000_000)),
            (PersistId::new(3), tomb(100)),
        ]),
        superseded: HashSet::new(),
        taken_at_ms: 1_700_000_000_000,
    };
    activate_fdb_fence(&cluster, data.grid, data.shard, data.node_id, data.epoch).await;
    store.checkpoint(&data).await.unwrap();

    // The cold scan serves only the live entity; neither tombstone leaks.
    let page = store
        .read_cold(GridId::new(9005), cell)
        .await
        .unwrap()
        .expect("live row present");
    assert!(page.entities.contains_key(&PersistId::new(1)));
    assert!(
        page.entities.len() == 1,
        "tombstones are never entities (P-6)"
    );

    // load rebuilds the tombstone set (the expired row was GC'd by the pass).
    let loaded = store
        .load(shard, GridId::new(9005))
        .await
        .unwrap()
        .expect("checkpoint present");
    assert_eq!(
        loaded.entities.len(),
        1,
        "GC'd tombstone rows are not entities either"
    );
    assert!(
        loaded.tombstones.contains_key(&PersistId::new(2)),
        "unexpired tombstone survives the pass"
    );
    assert!(
        !loaded.tombstones.contains_key(&PersistId::new(3)),
        "expired tombstone was cleared (GC pass)"
    );

    // P-7: the identical (cell, entity) in another grid is a separate row, and
    // deleting grid 0's shard leaves grid 2's row untouched.
    let g2 = GridId::new(9006);
    let data2 = CheckpointData {
        shard,
        grid: g2,
        node_id: 1,
        epoch: Epoch::new(1),
        watermark: Lsn::new(2, 4096),
        entities: HashMap::from([(PersistId::new(3), rec(3))]),
        by_cell: HashMap::from([(PersistId::new(3), cell)]),
        tombstones: HashMap::new(),
        superseded: HashSet::new(),
        taken_at_ms: 1_700_000_000_000,
    };
    activate_fdb_fence(
        &cluster,
        data2.grid,
        data2.shard,
        data2.node_id,
        data2.epoch,
    )
    .await;
    store.checkpoint(&data2).await.unwrap();
    let page2 = store
        .read_cold(g2, cell)
        .await
        .unwrap()
        .expect("grid 2 row");
    assert!(page2.entities.contains_key(&PersistId::new(3)));

    store.delete(shard, GridId::new(9005)).await.unwrap();
    assert!(store
        .load(shard, GridId::new(9005))
        .await
        .unwrap()
        .is_none());
    let page2_after = store
        .read_cold(g2, cell)
        .await
        .unwrap()
        .expect("grid 2 row survives grid 0 delete");
    assert!(page2_after.entities.contains_key(&PersistId::new(3)));

    store.delete(shard, g2).await.unwrap();
    assert!(store.read_cold(g2, cell).await.unwrap().is_none());
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_tombstone_end_to_end_lifecycle() {
    // P-6 through the actor path: spawn → checkpoint → despawn → checkpoint
    // (marker written) → restore, with the grid carried end to end.
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    activate_fdb_checkpoint_fence(&cluster, GridId::new(9007)).await;
    let store = std::sync::Arc::new(
        orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap(),
    );

    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(
        &runtime_config_in(dir.path(), GridId::new(9007)),
        &store_dyn(&store),
    )
    .await
    .unwrap();
    for i in 0..6u64 {
        let rec = mk_record_in(
            GridId::new(9007),
            CellId::ROOT,
            i,
            RecordKind::Spawn,
            &i.to_le_bytes(),
        );
        rt.apply(rec).await.unwrap();
    }
    store.delete(CellId::ROOT, GridId::new(9007)).await.unwrap();
    rt.checkpoint_shard(CellId::ROOT, store.as_ref())
        .await
        .unwrap();
    for i in 0..3u64 {
        let rec = mk_record_in(GridId::new(9007), CellId::ROOT, i, RecordKind::Despawn, b"");
        rt.apply(rec).await.unwrap();
    }
    rt.checkpoint_shard(CellId::ROOT, store.as_ref())
        .await
        .unwrap();

    let ckpt = store
        .load(CellId::ROOT, GridId::new(9007))
        .await
        .unwrap()
        .expect("checkpoint present");
    assert_eq!(ckpt.entities.len(), 3);
    assert_eq!(
        ckpt.tombstones.len(),
        3,
        "markers persisted by the checkpoint"
    );
    rt.close().await.unwrap();

    // Restore: the actor comes back with three live entities and three
    // markers, and a cold read serves only the live ones (no resurrection).
    let rt2 = CellRuntime::open(
        &runtime_config_in(dir.path(), GridId::new(9007)),
        &store_dyn(&store),
    )
    .await
    .unwrap();
    rt2.restore(CellId::ROOT, store.as_ref()).await.unwrap();
    let page = rt2.read(GridId::new(9007), CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 3);
    for i in 0..3u64 {
        assert!(
            !page.entities.contains_key(&PersistId::new(i)),
            "{i} despawned"
        );
    }
    let snap = rt2.actor_snapshot(CellId::ROOT).await.unwrap();
    assert_eq!(snap.tombstones.len(), 3);
    let cold = store
        .read_cold(GridId::new(9007), CellId::ROOT)
        .await
        .unwrap()
        .expect("live rows present");
    assert_eq!(cold.entities.len(), 3, "cold scan skips tombstone rows");
    rt2.close().await.unwrap();

    store.delete(CellId::ROOT, GridId::new(9007)).await.unwrap();
}
