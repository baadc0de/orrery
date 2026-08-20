//! Live FoundationDB coverage for durable lease rows.

mod support;

#[cfg(feature = "fdb")]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(feature = "fdb")]
use std::sync::Arc;
#[cfg(feature = "fdb")]
use std::time::Duration;

#[cfg(feature = "fdb")]
use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
#[cfg(feature = "fdb")]
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
#[cfg(feature = "fdb")]
use orrery_persistd::keyspace;
#[cfg(feature = "fdb")]
use orrery_persistd::{CellRuntime, ClaimResult, FdbContext, FdbLeaseStore, JournalConfig};
use orrery_persistd::{LeaseMigrate, LeasePut, LeaseStore, MemLeaseStore};
#[cfg(feature = "fdb")]
use orrery_persistd::{MemFenceStore, RuntimeConfig};
use orrery_protocol::{CellId, GridId, Lease, LeaseFlags, LeaseId, PersistId, SeqPair};
#[cfg(feature = "fdb")]
use orrery_protocol::{ClaimKind, Epoch};

fn node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

#[cfg(feature = "fdb")]
fn unique_persist_id() -> PersistId {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    PersistId::new(elapsed.as_secs().rotate_left(32) ^ u64::from(elapsed.subsec_nanos()))
}

#[cfg(feature = "fdb")]
fn unique_grid_id() -> GridId {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    GridId::new(
        elapsed.subsec_nanos()
            ^ std::process::id().rotate_left(16)
            ^ NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

#[cfg(feature = "fdb")]
fn runtime_config(dir: &std::path::Path, grid: GridId) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

fn lease(entity: PersistId, lease_id: u64) -> Lease {
    Lease {
        entity,
        holder: Some(node(80)),
        seq: SeqPair {
            own_seq: 7,
            auth_seq: 11,
        },
        lease_id: LeaseId(lease_id),
        expires_at: 42_000,
        flags: LeaseFlags(LeaseFlags::PLAYER_BOUND.0 | LeaseFlags::STRONG_HELD.0),
        bound_to: None,
    }
}

#[tokio::test]
async fn mem_migration_preserves_row_when_source_and_fence_match() {
    // Given: a durable row at one exact source cell.
    let store = MemLeaseStore::new();
    let grid = GridId::new(9_800);
    let entity = PersistId::new(8_001);
    let cells = CellId::ROOT.children();
    let row = lease(entity, 17);
    assert_eq!(
        store.put(grid, cells[0], &row).await.unwrap(),
        LeasePut::Stored
    );

    // When: the source and fencing token both match.
    let outcome = store
        .migrate(grid, entity, cells[0], cells[1], row.lease_id)
        .await
        .unwrap();

    // Then: only the destination indexes the bit-identical lease row.
    assert_eq!(outcome, LeaseMigrate::Migrated);
    assert!(store.load_cell(grid, cells[0]).await.unwrap().is_empty());
    assert_eq!(
        store.load_cell(grid, cells[1]).await.unwrap(),
        vec![(cells[1], row)]
    );
    assert_eq!(store.locate(grid, entity).await.unwrap(), Some(cells[1]));
}

#[tokio::test]
async fn mem_migration_has_no_side_effect_when_source_is_wrong() {
    // Given: a durable row at a different cell than the caller expects.
    let store = MemLeaseStore::new();
    let grid = GridId::new(9_800);
    let entity = PersistId::new(8_002);
    let cells = CellId::ROOT.children();
    let row = lease(entity, 23);
    assert_eq!(
        store.put(grid, cells[0], &row).await.unwrap(),
        LeasePut::Stored
    );

    // When: the caller presents a different source cell.
    let wrong_source = store
        .migrate(grid, entity, cells[2], cells[1], row.lease_id)
        .await
        .unwrap();

    // Then: the mismatch is typed and the original location/row is intact.
    assert_eq!(
        wrong_source,
        LeaseMigrate::SourceMismatch {
            actual: Some(cells[0])
        }
    );
    assert_eq!(
        store.load_cell(grid, cells[0]).await.unwrap(),
        vec![(cells[0], row)]
    );
    assert!(store.load_cell(grid, cells[1]).await.unwrap().is_empty());
    assert_eq!(store.locate(grid, entity).await.unwrap(), Some(cells[0]));
}

#[tokio::test]
async fn mem_migration_has_no_side_effect_when_fence_is_stale() {
    // Given: a durable row with a newer fencing token than the caller.
    let store = MemLeaseStore::new();
    let grid = GridId::new(9_800);
    let entity = PersistId::new(8_003);
    let cells = CellId::ROOT.children();
    let row = lease(entity, 29);
    assert_eq!(
        store.put(grid, cells[0], &row).await.unwrap(),
        LeasePut::Stored
    );

    // When: the caller presents a stale fencing token.
    let stale_fence = store
        .migrate(grid, entity, cells[0], cells[1], LeaseId(28))
        .await
        .unwrap();

    // Then: the mismatch is typed and the original location/row is intact.
    assert_eq!(
        stale_fence,
        LeaseMigrate::LeaseIdMismatch {
            actual: row.lease_id
        }
    );
    assert_eq!(
        store.load_cell(grid, cells[0]).await.unwrap(),
        vec![(cells[0], row)]
    );
    assert!(store.load_cell(grid, cells[1]).await.unwrap().is_empty());
    assert_eq!(store.locate(grid, entity).await.unwrap(), Some(cells[0]));
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_lease_actor_restore_and_expiry_are_durable() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let store = Arc::new(FdbLeaseStore::from_context(&context));
    // A read verifies reachability before creating a journal or rows.
    let grid = unique_grid_id();
    store
        .load_cell(grid, CellId::ROOT)
        .await
        .expect("configured FDB cluster must be reachable");

    let entity = unique_persist_id();
    let holder = node(81);
    let dir = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let config = runtime_config(dir.path(), grid);

    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    let actor = rt.actor(grid, CellId::ROOT).unwrap();
    let ClaimResult::Granted(before_restart) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };
    assert!(store
        .load_cell(grid, CellId::ROOT)
        .await
        .unwrap()
        .iter()
        .any(|(cell, row)| *cell == CellId::ROOT && row == &before_restart));
    assert_eq!(
        store.locate(grid, entity).await.unwrap(),
        Some(CellId::ROOT)
    );
    rt.close().await.unwrap();

    tokio::time::sleep(Duration::from_millis(2)).await;
    let restored_rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    let restored_actor = restored_rt.actor(grid, CellId::ROOT).unwrap();
    let restored = restored_actor
        .validate_lease(entity, holder, before_restart.lease_id, 0)
        .await
        .unwrap()
        .expect("FDB row restored by actor");
    assert_eq!(restored.holder, Some(holder));
    assert_eq!(restored.lease_id, before_restart.lease_id);
    assert_eq!(restored.seq, before_restart.seq);
    assert!(restored.expires_at > before_restart.expires_at);

    let parked = restored_actor
        .sweep_leases(restored.expires_at)
        .await
        .unwrap()
        .pop()
        .expect("expired row is parked");
    assert_eq!(parked.lease.holder, None);
    assert!(parked.lease.flags.contains(LeaseFlags::PARKED));
    assert!(parked.lease.lease_id > restored.lease_id);
    // The sweep also reports the identity the row lost, so a successor policy
    // can act on a restored-then-expired lease without re-reading the tier.
    assert_eq!(parked.previous_holder, holder);
    assert_eq!(parked.previous_lease_id, restored.lease_id);
    assert_eq!(parked.grid, grid);
    assert_eq!(parked.cell, CellId::ROOT);
    assert!(store
        .load_cell(grid, CellId::ROOT)
        .await
        .unwrap()
        .iter()
        .any(|(cell, row)| *cell == CellId::ROOT && row == &parked.lease));

    restored_rt.close().await.unwrap();
    store.remove(grid, CellId::ROOT, entity).await.unwrap();
    assert_eq!(store.locate(grid, entity).await.unwrap(), None);

    // Concurrent first writes for the same entity but different cells must
    // conflict in FDB rather than minting two actor-visible locations.
    let racing_entity = PersistId::new(entity.0.wrapping_add(1));
    let children = CellId::ROOT.children();
    let left = Lease {
        entity: racing_entity,
        holder: Some(node(82)),
        seq: SeqPair::default(),
        lease_id: LeaseId(1),
        expires_at: 10_000,
        flags: LeaseFlags(0),
        bound_to: None,
    };
    let right = Lease {
        holder: Some(node(83)),
        ..left.clone()
    };
    let (left_result, right_result) = tokio::join!(
        store.put(grid, children[0], &left),
        store.put(grid, children[1], &right),
    );
    let outcomes = [left_result.unwrap(), right_result.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, LeasePut::Stored))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, LeasePut::LocationConflict(_)))
            .count(),
        1
    );
    let committed = store
        .locate(grid, racing_entity)
        .await
        .unwrap()
        .expect("one FDB location won");
    assert!(committed == children[0] || committed == children[1]);
    store.remove(grid, committed, racing_entity).await.unwrap();
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_migration_is_atomic_for_exact_source_and_fence() {
    // Given: one reachable FDB-backed lease row at an exact source cell.
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let store = FdbLeaseStore::from_context(&context);
    let grid = unique_grid_id();
    store
        .load_cell(grid, CellId::ROOT)
        .await
        .expect("configured FDB cluster must be reachable");
    let entity = unique_persist_id();
    let cells = CellId::ROOT.children();
    let row = lease(entity, 31);
    assert_eq!(
        store.put(grid, cells[0], &row).await.unwrap(),
        LeasePut::Stored
    );

    // When: stale source and fence attempts precede a destination-index conflict.
    assert_eq!(
        store
            .migrate(grid, entity, cells[2], cells[1], row.lease_id)
            .await
            .unwrap(),
        LeaseMigrate::SourceMismatch {
            actual: Some(cells[0])
        }
    );
    assert_eq!(
        store.load_cell(grid, cells[0]).await.unwrap(),
        vec![(cells[0], row.clone())]
    );
    assert!(store.load_cell(grid, cells[1]).await.unwrap().is_empty());
    assert_eq!(store.locate(grid, entity).await.unwrap(), Some(cells[0]));
    assert_eq!(
        store
            .migrate(grid, entity, cells[0], cells[1], LeaseId(30))
            .await
            .unwrap(),
        LeaseMigrate::LeaseIdMismatch {
            actual: row.lease_id
        }
    );
    assert_eq!(
        store.load_cell(grid, cells[0]).await.unwrap(),
        vec![(cells[0], row.clone())]
    );
    assert!(store.load_cell(grid, cells[1]).await.unwrap().is_empty());
    assert_eq!(store.locate(grid, entity).await.unwrap(), Some(cells[0]));
    let destination_key = keyspace::lease_cell_key(grid, cells[1], entity);
    context
        .database()
        .run(move |trx, _| async move {
            trx.set(&destination_key, &[]);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .migrate(grid, entity, cells[0], cells[1], row.lease_id)
            .await
            .unwrap(),
        LeaseMigrate::IndexConflict
    );
    context
        .database()
        .run(move |trx, _| async move {
            trx.clear(&destination_key);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .migrate(grid, entity, cells[0], cells[1], row.lease_id)
            .await
            .unwrap(),
        LeaseMigrate::Migrated
    );

    // Then: FDB exposes no old index and one destination/reverse location.
    assert!(store
        .load_cell(grid, cells[0])
        .await
        .unwrap()
        .iter()
        .all(|(_, loaded)| loaded.entity != entity));
    let destination_rows = store.load_cell(grid, cells[1]).await.unwrap();
    assert_eq!(
        destination_rows
            .iter()
            .filter(|(_, loaded)| loaded.entity == entity)
            .count(),
        1
    );
    assert!(destination_rows
        .iter()
        .any(|(cell, loaded)| *cell == cells[1] && loaded == &row));
    assert_eq!(store.locate(grid, entity).await.unwrap(), Some(cells[1]));

    let source_key = keyspace::lease_cell_key(grid, cells[0], entity);
    let destination_key = keyspace::lease_cell_key(grid, cells[1], entity);
    let location_key = keyspace::lease_location_key(grid, entity);
    let row_key = keyspace::lease_key(grid, entity);
    let (source_value, destination_value, location_value, row_value) = context
        .database()
        .run(move |trx, _| {
            let source_key = source_key;
            let destination_key = destination_key;
            let location_key = location_key;
            let row_key = row_key;
            async move {
                Ok((
                    trx.get(&source_key, false)
                        .await?
                        .map(|value| value.to_vec()),
                    trx.get(&destination_key, false)
                        .await?
                        .map(|value| value.to_vec()),
                    trx.get(&location_key, false)
                        .await?
                        .map(|value| value.to_vec()),
                    trx.get(&row_key, false).await?.map(|value| value.to_vec()),
                ))
            }
        })
        .await
        .unwrap();
    assert_eq!(source_value, None);
    assert_eq!(destination_value, Some(Vec::new()));
    assert_eq!(
        location_value,
        Some(cells[1].to_bits().to_be_bytes().to_vec())
    );
    let durable_row: Lease = postcard::from_bytes(&row_value.expect("durable lease row")).unwrap();
    assert_eq!(durable_row.holder, row.holder);
    assert_eq!(durable_row.lease_id, row.lease_id);
    assert_eq!(durable_row.seq, row.seq);
    assert_eq!(durable_row, row);
    store.remove(grid, cells[1], entity).await.unwrap();
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_migration_transaction_error_preserves_source_indexes() {
    // Given: a valid row whose reverse index is then made malformed.
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let store = FdbLeaseStore::from_context(&context);
    let grid = unique_grid_id();
    store
        .load_cell(grid, CellId::ROOT)
        .await
        .expect("configured FDB cluster must be reachable");
    let entity = unique_persist_id();
    let cells = CellId::ROOT.children();
    let row = lease(entity, 37);
    assert_eq!(
        store.put(grid, cells[0], &row).await.unwrap(),
        LeasePut::Stored
    );
    let location_key = keyspace::lease_location_key(grid, entity);
    context
        .database()
        .run(move |trx, _| async move {
            trx.set(&location_key, &[0xff]);
            Ok(())
        })
        .await
        .unwrap();

    // When: migration fails while validating the transaction's durable input.
    let result = store
        .migrate(grid, entity, cells[0], cells[1], row.lease_id)
        .await;

    // Then: the row/old index remain exact and no destination index appears.
    assert!(result.is_err());
    assert!(store
        .load_cell(grid, cells[0])
        .await
        .unwrap()
        .iter()
        .any(|(cell, loaded)| *cell == cells[0] && loaded == &row));
    assert!(store
        .load_cell(grid, cells[1])
        .await
        .unwrap()
        .iter()
        .all(|(_, loaded)| loaded.entity != entity));
    store.remove(grid, cells[0], entity).await.unwrap();
}
