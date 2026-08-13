//! Fencing CAS + hotspot split integration tests (P2 continuation).
//!
//! These exercise the `actor/{shard}` fencing protocol (§3.4) and the hotspot
//! split (§3.5) end-to-end with the in-memory fence store — no FoundationDB.

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::fence::{FenceError, FenceOutcome, FenceRow, FenceStatus, MemFenceStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{payload_crc, CellRuntime, FenceStore, JournalConfig, RuntimeConfig};

use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

/// A fresh in-memory checkpoint store as the trait object `CellRuntime::open`
/// and `fence_shard` take. These tests never checkpoint; the store only has
/// to exist so an actor can be seeded from it at open.
fn mem_store() -> Arc<dyn CheckpointStore> {
    Arc::new(MemCheckpointStore::new())
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

fn runtime_config(dir: &std::path::Path, node_id: u64) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

fn runtime_config_with_fence(
    dir: &std::path::Path,
    node_id: u64,
    fence: Arc<MemFenceStore>,
) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id,
        epoch: Epoch::new(0),
        fence,
    }
}

fn runtime_config_shards(
    dir: &std::path::Path,
    node_id: u64,
    shards: Vec<CellId>,
) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// A shard cell at the shard level (one level coarser than interest), so its
/// children are real shard cells with distinct subtrees.
fn shard_cell(x: i32, y: i32, z: i32) -> CellId {
    // Shard level = interest − 3 = 18 (docs/01-spatial-model.md §3.4).
    CellId::from_coords(glam::IVec3::new(x, y, z), 18).unwrap()
}

/// A runtime config whose fence store is the given [`FenceStore`] (dyn).
#[cfg(feature = "fdb")]
fn runtime_config_dyn_fence(
    dir: &std::path::Path,
    node_id: u64,
    shards: Vec<CellId>,
    fence: Arc<dyn orrery_persistd::FenceStore>,
) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id,
        epoch: Epoch::new(0),
        fence,
    }
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

#[tokio::test]
async fn fence_shard_from_absent_fences_and_spawns_actor() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = CellRuntime::open(&runtime_config(dir.path(), 7), &mem_store()).unwrap();

    let shard = shard_cell(1, 0, 0);
    let epoch = rt
        .fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();
    assert_eq!(epoch, Epoch::new(1));

    // The actor exists and can serve writes at the new epoch.
    let rec = mk_record(shard, 1, RecordKind::Spawn, b"hp=100");
    let lsn = rt.apply(rec).await.unwrap();
    assert!(lsn >= Lsn::new(0, 0));

    // The fence row records this node + epoch.
    let row = rt.fence().read(GridId::ROOT, shard).await.unwrap().unwrap();
    assert_eq!(row.owner, 7);
    assert_eq!(row.epoch, Epoch::new(1));
    assert_eq!(row.status, FenceStatus::Active);

    rt.close().await.unwrap();
}

#[tokio::test]
async fn fence_shard_conflicts_on_stale_expected() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = CellRuntime::open(&runtime_config(dir.path(), 7), &mem_store()).unwrap();

    let shard = shard_cell(1, 0, 0);
    rt.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();

    // A second fence with a stale expected row (epoch 0) must conflict.
    let stale = FenceRow {
        owner: 7,
        epoch: Epoch::new(0),
        status: FenceStatus::Active,
    };
    let err = rt
        .fence_shard(shard, Some(&stale), mem_store().as_ref())
        .await
        .unwrap_err();
    match err {
        FenceError::Conflict { current } => {
            let cur = current.unwrap();
            assert_eq!(cur.owner, 7);
            assert_eq!(cur.epoch, Epoch::new(1));
        }
        other => panic!("expected conflict, got {other:?}"),
    }

    rt.close().await.unwrap();
}

#[tokio::test]
async fn fence_shard_second_node_conflicts() {
    // Two nodes fencing the same shard: the second must conflict — the
    // single-writer invariant (D2) holds at the fence layer. They share one
    // fence store (the durable `actor/` rows) but have separate journals.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let fence = Arc::new(MemFenceStore::new());
    let mut rt_a = CellRuntime::open(
        &runtime_config_with_fence(dir_a.path(), 1, Arc::clone(&fence)),
        &mem_store(),
    )
    .unwrap();
    let mut rt_b = CellRuntime::open(
        &runtime_config_with_fence(dir_b.path(), 2, Arc::clone(&fence)),
        &mem_store(),
    )
    .unwrap();

    let shard = shard_cell(2, 0, 0);
    rt_a.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();
    let err = rt_b
        .fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap_err();
    match err {
        FenceError::Conflict { current } => {
            assert_eq!(current.unwrap().owner, 1);
        }
        other => panic!("expected conflict, got {other:?}"),
    }

    rt_a.close().await.unwrap();
    rt_b.close().await.unwrap();
}

#[tokio::test]
async fn split_partitions_entities_and_retires_parent() {
    let dir = tempfile::tempdir().unwrap();
    let shard = shard_cell(0, 0, 0);
    let mut rt = CellRuntime::open(
        &runtime_config_shards(dir.path(), 3, vec![shard]),
        &mem_store(),
    )
    .unwrap();
    rt.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();
    let parent_row = rt.fence().read(GridId::ROOT, shard).await.unwrap().unwrap();

    // Seed entities across the shard's children. Each child is a shard cell at
    // level 19; pick one interest cell inside each child.
    let children = shard.children();
    for (i, child) in children.iter().enumerate() {
        let rec = mk_record(*child, i as u64, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }

    // Split.
    let child_rows = rt.split(shard, &parent_row).await.unwrap();
    assert_eq!(child_rows.len(), 8);
    for (_, row) in &child_rows {
        assert_eq!(row.epoch, Epoch::new(2));
        assert_eq!(row.owner, 3);
        assert_eq!(row.status, FenceStatus::Active);
    }

    // Parent row retired; children active.
    assert!(rt
        .fence()
        .read(GridId::ROOT, shard)
        .await
        .unwrap()
        .is_none());
    for (child, _) in &child_rows {
        let row = rt
            .fence()
            .read(GridId::ROOT, *child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.epoch, Epoch::new(2));
    }

    // Each child actor holds exactly its own entity.
    for (i, child) in children.iter().enumerate() {
        let page = rt.read(GridId::ROOT, *child).await.unwrap();
        assert_eq!(page.entities.len(), 1, "child {child:?}");
        assert!(page.entities.contains_key(&PersistId::new(i as u64)));
    }

    // The parent actor is gone.
    assert!(rt.actor(GridId::ROOT, shard).is_none());

    rt.close().await.unwrap();
}

#[tokio::test]
async fn split_after_checkpoint_restore_partitions_correctly() {
    // A checkpoint carries the per-entity cell map, so a split after a
    // checkpoint+restore still partitions entities into the right children
    // (§3.5, §8).
    let dir = tempfile::tempdir().unwrap();
    let store = orrery_persistd::checkpoint::MemCheckpointStore::new();
    let shard = shard_cell(0, 0, 0);

    let child = shard.children()[3];
    let child2 = shard.children()[6];

    // Phase 1: seed entities, checkpoint, close.
    {
        let mut rt = CellRuntime::open(
            &runtime_config_shards(dir.path(), 3, vec![shard]),
            &mem_store(),
        )
        .unwrap();
        rt.fence_shard(shard, None, mem_store().as_ref())
            .await
            .unwrap();
        rt.apply(mk_record(child, 1, RecordKind::Spawn, b"a"))
            .await
            .unwrap();
        rt.apply(mk_record(child2, 2, RecordKind::Spawn, b"b"))
            .await
            .unwrap();
        rt.checkpoint(&store).await.unwrap();
        rt.close().await.unwrap();
    }

    // Phase 2: reopen, restore from checkpoint + journal, then split.
    let mut rt = CellRuntime::open(
        &runtime_config_shards(dir.path(), 3, vec![shard]),
        &mem_store(),
    )
    .unwrap();
    rt.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();
    rt.restore(shard, &store).await.unwrap();
    let parent_row = rt.fence().read(GridId::ROOT, shard).await.unwrap().unwrap();

    let child_rows = rt.split(shard, &parent_row).await.unwrap();
    assert_eq!(child_rows.len(), 8);

    // Entity 1 lives in child[3], entity 2 in child[6].
    let page1 = rt.read(GridId::ROOT, child).await.unwrap();
    assert!(page1.entities.contains_key(&PersistId::new(1)));
    assert!(!page1.entities.contains_key(&PersistId::new(2)));
    let page2 = rt.read(GridId::ROOT, child2).await.unwrap();
    assert!(page2.entities.contains_key(&PersistId::new(2)));
    assert!(!page2.entities.contains_key(&PersistId::new(1)));

    rt.close().await.unwrap();
}

#[tokio::test]
async fn second_level_split_partitions_correctly() {
    // A child actor spawned by a split carries its per-entity cell map, so a
    // second-level split of that child still partitions into the right
    // grandchildren (§3.5).
    let dir = tempfile::tempdir().unwrap();
    let shard = shard_cell(0, 0, 0);
    let mut rt = CellRuntime::open(
        &runtime_config_shards(dir.path(), 3, vec![shard]),
        &mem_store(),
    )
    .unwrap();
    rt.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();
    let parent_row = rt.fence().read(GridId::ROOT, shard).await.unwrap().unwrap();

    // Seed entities in two grandchildren of the same child.
    let child = shard.children()[2];
    let grand_a = child.children()[1];
    let grand_b = child.children()[5];
    rt.apply(mk_record(grand_a, 1, RecordKind::Spawn, b"a"))
        .await
        .unwrap();
    rt.apply(mk_record(grand_b, 2, RecordKind::Spawn, b"b"))
        .await
        .unwrap();

    // First split: shard -> children.
    let child_rows = rt.split(shard, &parent_row).await.unwrap();
    assert_eq!(child_rows.len(), 8);

    // Second split: the child that holds both entities -> grandchildren.
    let child_row = rt.fence().read(GridId::ROOT, child).await.unwrap().unwrap();
    let grand_rows = rt.split(child, &child_row).await.unwrap();
    assert_eq!(grand_rows.len(), 8);

    // Each grandchild holds exactly its own entity.
    let page_a = rt.read(GridId::ROOT, grand_a).await.unwrap();
    assert!(page_a.entities.contains_key(&PersistId::new(1)));
    assert!(!page_a.entities.contains_key(&PersistId::new(2)));
    let page_b = rt.read(GridId::ROOT, grand_b).await.unwrap();
    assert!(page_b.entities.contains_key(&PersistId::new(2)));
    assert!(!page_b.entities.contains_key(&PersistId::new(1)));

    rt.close().await.unwrap();
}

#[tokio::test]
async fn split_conflicts_on_stale_parent_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = CellRuntime::open(&runtime_config(dir.path(), 3), &mem_store()).unwrap();

    let shard = shard_cell(0, 0, 0);
    rt.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();

    // A stale parent row (wrong epoch) must not split.
    let stale = FenceRow {
        owner: 3,
        epoch: Epoch::new(0),
        status: FenceStatus::Active,
    };
    let err = rt.split(shard, &stale).await.unwrap_err();
    match err {
        FenceError::Conflict { current } => {
            assert_eq!(current.unwrap().epoch, Epoch::new(1));
        }
        other => panic!("expected conflict, got {other:?}"),
    }

    // Parent still active, no children spawned as their own shards.
    assert!(rt.actor(GridId::ROOT, shard).is_some());
    for child in shard.children() {
        // No child fence row was written.
        assert!(rt
            .fence()
            .read(GridId::ROOT, child)
            .await
            .unwrap()
            .is_none());
    }

    rt.close().await.unwrap();
}

#[tokio::test]
async fn fence_outcome_is_exposed() {
    // The FenceStore trait returns FenceOutcome directly; assert the enum shape
    // is usable by callers (e.g. gateways repairing on epoch-mismatch NACKs).
    let store = MemFenceStore::new();
    let shard = shard_cell(0, 0, 0);
    let row = FenceRow {
        owner: 1,
        epoch: Epoch::new(1),
        status: FenceStatus::Active,
    };
    assert_eq!(
        store.fence(GridId::ROOT, shard, None, &row).await.unwrap(),
        FenceOutcome::Fenced
    );
    let conflict = store.fence(GridId::ROOT, shard, None, &row).await.unwrap();
    assert!(matches!(conflict, FenceOutcome::Conflict { current } if current == Some(row)));
}

#[cfg(feature = "fdb")]
fn fdb_fence_store() -> Option<orrery_persistd::fence::FdbFenceStore> {
    let cluster = fdb_cluster_file()?;
    orrery_persistd::fence::FdbFenceStore::connect(&cluster).ok()
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_fence_cas_roundtrip() {
    let Some(store) = fdb_fence_store() else {
        eprintln!("skipping: no reachable FDB cluster");
        return;
    };
    // A distinct shard per test avoids parallel collisions on the shared FDB
    // keyspace; retire first so "fence from absent" is idempotent across runs.
    let shard = shard_cell(1, 0, 0);
    store.retire(GridId::ROOT, shard).await.unwrap();
    let row = FenceRow {
        owner: 1,
        epoch: Epoch::new(3),
        status: FenceStatus::Active,
    };

    // Fence from absent, read back.
    assert_eq!(
        store.fence(GridId::ROOT, shard, None, &row).await.unwrap(),
        FenceOutcome::Fenced
    );
    assert_eq!(store.read(GridId::ROOT, shard).await.unwrap(), Some(row));

    // Stale expected (wrong epoch) -> conflict, row unchanged.
    let stale = FenceRow {
        epoch: Epoch::new(2),
        ..row
    };
    let conflict = store
        .fence(GridId::ROOT, shard, Some(&stale), &row)
        .await
        .unwrap();
    assert!(matches!(conflict, FenceOutcome::Conflict { current } if current == Some(row)));
    assert_eq!(store.read(GridId::ROOT, shard).await.unwrap(), Some(row));

    // Correct expected -> advances to a new epoch.
    let new = FenceRow {
        owner: 2,
        epoch: Epoch::new(4),
        status: FenceStatus::Active,
    };
    assert_eq!(
        store
            .fence(GridId::ROOT, shard, Some(&row), &new)
            .await
            .unwrap(),
        FenceOutcome::Fenced
    );
    assert_eq!(store.read(GridId::ROOT, shard).await.unwrap(), Some(new));

    store.retire(GridId::ROOT, shard).await.unwrap();
    assert_eq!(store.read(GridId::ROOT, shard).await.unwrap(), None);
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_begin_split_atomic() {
    let Some(store) = fdb_fence_store() else {
        eprintln!("skipping: no reachable FDB cluster");
        return;
    };
    let parent = shard_cell(2, 0, 0);
    let parent_row = FenceRow {
        owner: 1,
        epoch: Epoch::new(1),
        status: FenceStatus::Active,
    };
    // Retire any prior state so the "from absent" fence is idempotent.
    store.retire(GridId::ROOT, parent).await.unwrap();
    for c in parent.children() {
        store.retire(GridId::ROOT, c).await.unwrap();
    }
    store
        .fence(GridId::ROOT, parent, None, &parent_row)
        .await
        .unwrap();

    let children: Vec<(CellId, FenceRow)> = parent
        .children()
        .iter()
        .map(|&c| {
            (
                c,
                FenceRow {
                    owner: 2,
                    epoch: Epoch::new(2),
                    status: FenceStatus::Active,
                },
            )
        })
        .collect();

    // Split writes parent -> Splitting and all 8 child rows in one txn.
    assert_eq!(
        store
            .begin_split(GridId::ROOT, parent, &parent_row, &children)
            .await
            .unwrap(),
        FenceOutcome::Fenced
    );
    let parent_after = store.read(GridId::ROOT, parent).await.unwrap().unwrap();
    assert_eq!(parent_after.status, FenceStatus::Splitting);
    assert_eq!(parent_after.epoch, Epoch::new(1));
    for (child, row) in &children {
        assert_eq!(store.read(GridId::ROOT, *child).await.unwrap(), Some(*row));
    }

    // Stale parent -> conflict, nothing changes.
    let conflict = store
        .begin_split(GridId::ROOT, parent, &parent_row, &[])
        .await
        .unwrap();
    assert!(
        matches!(conflict, FenceOutcome::Conflict { current } if current == Some(parent_after))
    );

    store.retire(GridId::ROOT, parent).await.unwrap();
    for (child, _) in &children {
        store.retire(GridId::ROOT, *child).await.unwrap();
    }
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_runtime_split_end_to_end() {
    // Drive the runtime split through the real FDB fence store: the parent row
    // CAS and the 8 child rows commit to FDB, and the runtime still retires the
    // parent and serves reads from the children.
    let Some(store) = fdb_fence_store() else {
        eprintln!("skipping: no reachable FDB cluster");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let shard = shard_cell(3, 0, 0);
    // Idempotent across runs: clear any prior row for this shard and its kids.
    store.retire(GridId::ROOT, shard).await.unwrap();
    for c in shard.children() {
        store.retire(GridId::ROOT, c).await.unwrap();
    }
    let mut rt = CellRuntime::open(
        &runtime_config_dyn_fence(dir.path(), 3, vec![shard], Arc::new(store)),
        &mem_store(),
    )
    .unwrap();

    rt.fence_shard(shard, None, mem_store().as_ref())
        .await
        .unwrap();
    let parent_row = rt.fence().read(GridId::ROOT, shard).await.unwrap().unwrap();

    let children = shard.children();
    for (i, child) in children.iter().enumerate() {
        let rec = mk_record(*child, i as u64, RecordKind::Spawn, &i.to_le_bytes());
        rt.apply(rec).await.unwrap();
    }

    let child_rows = rt.split(shard, &parent_row).await.unwrap();
    assert_eq!(child_rows.len(), 8);
    assert!(rt
        .fence()
        .read(GridId::ROOT, shard)
        .await
        .unwrap()
        .is_none());
    for (i, child) in children.iter().enumerate() {
        let page = rt.read(GridId::ROOT, *child).await.unwrap();
        assert_eq!(page.entities.len(), 1, "child {child:?}");
        assert!(page.entities.contains_key(&PersistId::new(i as u64)));
    }

    rt.close().await.unwrap();
}
