//! End-to-end tests for the cell-actor runtime + segmented journal.
//!
//! These exercise the actor → journal → group-commit → replay path with no
//! FoundationDB: durability, idempotent replay, and crash-and-recover.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use orrery_persistd::checkpoint::{
    CheckpointData, CheckpointError, CheckpointStore, MemCheckpointStore,
};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, FencedApply, JournalConfig, LeaseMigrate, LeasePut,
    LeaseStore, LeaseStoreError, MemLeaseStore, Reject, RekeyError, Router, RuntimeConfig,
    LEASE_TTL_MS,
};
// Not re-exported at the crate root: `lib.rs` belongs to another lane, and the
// module path is public and does the job.
use orrery_persistd::lease::CLAIM_HERD_DAMPER_MS;

use orrery_protocol::{
    CellId, ClaimKind, DenyReason, EntityRekey, Epoch, GridId, JournalRecord, Lease, LeaseFlags,
    LeaseId, Lsn, PersistId, RecordKind, Tick, ENTITY_REKEY_VERSION,
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

fn runtime_config(dir: &std::path::Path, batch: bool) -> RuntimeConfig {
    let mode = if batch {
        AdaptiveCommitMode::AlwaysBatch
    } else {
        AdaptiveCommitMode::Adaptive
    };
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode,
                // Generous window so concurrent appends land in one batch.
                batch_window: if batch {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(1)
                },
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: std::sync::Arc::new(orrery_persistd::MemFenceStore::new()),
    }
}

fn rekey_record(rekey: &EntityRekey) -> JournalRecord {
    let payload = bytes::Bytes::from(postcard::to_allocvec(rekey).unwrap());
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: rekey.source_cell,
        grid: rekey.source_grid,
        entity: rekey.entity,
        tick: Tick::new(7),
        epoch: Epoch::new(2),
        author: test_node(1),
        kind: RecordKind::Rekey,
        crc: payload_crc(&payload),
        payload,
    }
}

fn valid_rekey() -> EntityRekey {
    let cells = CellId::ROOT.children();
    EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity: PersistId::new(5_501),
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::new(4),
        destination_cell: cells[1],
        expected_lease_id: LeaseId(19),
        source_record: bytes::Bytes::from_static(b"deterministic-source-image"),
    }
}

#[tokio::test]
async fn source_actor_snapshot_and_journal_characterization_before_rekey() {
    // Given: two real shard actors and one source entity with a durable lease.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let store = Arc::new(MemLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(&config, &mem_store(), store.clone())
        .await
        .unwrap();
    let entity = PersistId::new(5_500);
    let holder = test_node(12);

    // When: the source actor journals the entity and grants its lease.
    let spawn = mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"characterized-components",
    );
    let committed_lsn = rt.apply(spawn).await.unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(entity, cells[0], holder, ClaimKind::Weak, 10)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };

    // Then: the actor snapshot, lease store, and real journal agree on source ownership.
    let page = rt.read(GridId::ROOT, cells[0]).await.unwrap();
    assert_eq!(
        page.entities[&entity].components.as_ref(),
        b"characterized-components"
    );
    assert!(rt
        .read(GridId::ROOT, cells[1])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        store.load_cell(GridId::ROOT, cells[0]).await.unwrap(),
        vec![(cells[0], grant)]
    );
    let journaled: Vec<_> = rt
        .journal()
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .collect();
    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0].lsn, committed_lsn);
    assert_eq!(journaled[0].cell, cells[0]);
    assert_eq!(journaled[0].payload.as_ref(), b"characterized-components");

    rt.close().await.unwrap();
}

#[tokio::test]
async fn persistence_rekey_decoder_rejects_untrusted_or_stale_shapes() {
    // Given: one valid committed-rekey envelope and invalid boundary variants.
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let valid = valid_rekey();
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&valid)).await,
        Err(RekeyError::ActorUnavailable)
    );

    let mut wrong_version = valid.clone();
    wrong_version.version = ENTITY_REKEY_VERSION + 1;
    let mut self_move = valid.clone();
    self_move.destination_grid = self_move.source_grid;
    self_move.destination_cell = self_move.source_cell;
    let mut missing_fence = valid.clone();
    missing_fence.expected_lease_id = LeaseId(0);
    let mut missing_source = valid.clone();
    missing_source.source_record = bytes::Bytes::new();
    let mut wrong_source_envelope = rekey_record(&valid);
    wrong_source_envelope.cell = CellId::ROOT.children()[2];
    let malformed = JournalRecord {
        payload: bytes::Bytes::from_static(b"not-postcard"),
        crc: payload_crc(b"not-postcard"),
        ..rekey_record(&valid)
    };
    let mut wrong_crc = rekey_record(&valid);
    wrong_crc.crc = payload_crc(&wrong_crc.payload).wrapping_add(1);
    assert_ne!(wrong_crc.crc, payload_crc(&wrong_crc.payload));

    // When/Then: persistence rejects each malformed, stale, or ambiguous input.
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&wrong_version)).await,
        Err(RekeyError::VersionMismatch)
    );
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&self_move)).await,
        Err(RekeyError::SelfMove)
    );
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&missing_fence)).await,
        Err(RekeyError::MissingExpectedFence)
    );
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&missing_source)).await,
        Err(RekeyError::MissingSourceRecord)
    );
    assert_eq!(
        Router::commit_rekey(&rt, wrong_source_envelope).await,
        Err(RekeyError::SourceMismatch)
    );
    assert_eq!(
        Router::commit_rekey(&rt, malformed).await,
        Err(RekeyError::MalformedPayload)
    );
    assert_eq!(
        Router::commit_rekey(&rt, wrong_crc).await,
        Err(RekeyError::MalformedPayload)
    );
    rt.close().await.unwrap();
}

#[tokio::test]
async fn router_commit_rekey_rejects_an_unavailable_actor_topology() {
    // Given: a persistence-validated rekey whose destination grid is not hosted.
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let record = rekey_record(&valid_rekey());

    // When: server code calls the dedicated router entrypoint.
    let outcome = Router::commit_rekey(&rt, record).await;

    // Then: the server entrypoint rejects the unavailable actor topology.
    assert_eq!(outcome, Err(RekeyError::ActorUnavailable));
    rt.close().await.unwrap();
}

#[tokio::test]
async fn committed_rekey_moves_entity_then_recovers_destination() {
    // Given: two real shard actors, a journaled source image, and its exact lease fence.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let checkpoints = mem_store();
    let store = Arc::new(MemLeaseStore::new());
    let entity = PersistId::new(5_601);
    let holder = test_node(13);
    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"rekeyed-component-bytes",
    ))
    .await
    .unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(entity, cells[0], holder, ClaimKind::Strong, 20)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    let rekey = EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::ROOT,
        destination_cell: cells[1],
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::from_static(b"rekeyed-component-bytes"),
    };

    // When: the server commits the rekey and the runtime restarts before any checkpoint.
    Router::commit_rekey(&rt, rekey_record(&rekey))
        .await
        .unwrap();
    assert!(rt
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    let moved = rt.read(GridId::ROOT, cells[1]).await.unwrap();
    assert_eq!(
        moved.entities[&entity].components.as_ref(),
        b"rekeyed-component-bytes"
    );
    let destination = rt.actor(GridId::ROOT, cells[1]).unwrap().clone();
    let moved_lease = destination
        .validate_lease(entity, holder, grant.lease_id, 21)
        .await
        .unwrap()
        .expect("destination fence");
    assert_eq!(moved_lease.holder, grant.holder);
    assert_eq!(moved_lease.lease_id, grant.lease_id);
    assert_eq!(moved_lease.seq, grant.seq);
    rt.close().await.unwrap();
    let recovered = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();

    // Then: replay reconstructs only the destination with identical bytes and fencing identity.
    assert!(recovered
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    let recovered_page = recovered.read(GridId::ROOT, cells[1]).await.unwrap();
    assert_eq!(
        recovered_page.entities[&entity].components.as_ref(),
        b"rekeyed-component-bytes"
    );
    let recovered_destination = recovered.actor(GridId::ROOT, cells[1]).unwrap().clone();
    let recovered_lease = recovered_destination
        .validate_lease(entity, holder, grant.lease_id, 0)
        .await
        .unwrap()
        .expect("recovered destination fence");
    assert_eq!(recovered_lease.holder, grant.holder);
    assert_eq!(recovered_lease.lease_id, grant.lease_id);
    assert_eq!(recovered_lease.seq, grant.seq);
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[1])
    );
    let rekeys: Vec<_> = recovered
        .journal()
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .filter(|record| record.kind == RecordKind::Rekey)
        .collect();
    assert_eq!(rekeys.len(), 1);
    assert_eq!(rekeys[0].entity, entity);

    recovered.close().await.unwrap();
}

#[tokio::test]
async fn a_rekey_for_unhosted_shards_does_not_brick_open() {
    // Given: a journal holding a committed rekey between two shards that the
    // *next* process does not host — exactly what a chain follower's journal
    // accumulates, since it mirrors every record of the node it replicates and
    // a cross-node entity move is a rekey naming two foreign cells.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1], cells[2]];
    let checkpoints = mem_store();
    let store = Arc::new(MemLeaseStore::new());
    let moved = PersistId::new(5_701);
    let local = PersistId::new(5_702);
    let holder = test_node(23);
    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    rt.apply(mk_record(
        cells[0],
        moved.0,
        RecordKind::Spawn,
        b"foreign-image",
    ))
    .await
    .unwrap();
    rt.apply(mk_record(
        cells[2],
        local.0,
        RecordKind::Spawn,
        b"hosted-image",
    ))
    .await
    .unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(moved, cells[0], holder, ClaimKind::Strong, 30)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    Router::commit_rekey(
        &rt,
        rekey_record(&EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity: moved,
            source_grid: GridId::ROOT,
            source_cell: cells[0],
            destination_grid: GridId::ROOT,
            destination_cell: cells[1],
            expected_lease_id: grant.lease_id,
            source_record: bytes::Bytes::from_static(b"foreign-image"),
        }),
    )
    .await
    .unwrap();
    rt.close().await.unwrap();

    // When: the node restarts hosting only the shard that rekey never named.
    // `open` used to fail here — the source `ok_or_else` in the replay branch
    // and the destination one inside `recover_rekey` both raised on a shard
    // this node has no actor for, so one mirrored move crash-looped the node.
    config.shards = vec![cells[2]];
    let recovered = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .expect("a rekey for unhosted shards must not fail open");

    // Then: it serves the shard it does host, and claims neither of the others.
    let page = recovered.read(GridId::ROOT, cells[2]).await.unwrap();
    assert_eq!(page.entities[&local].components.as_ref(), b"hosted-image");
    assert!(recovered.actor(GridId::ROOT, cells[0]).is_none());
    assert!(recovered.actor(GridId::ROOT, cells[1]).is_none());
    // The mirrored record is still on disk; it was skipped, not dropped.
    let rekeys = recovered
        .journal()
        .scan_from(Lsn::new(0, 0))
        .filter(|item| item.as_ref().unwrap().record.kind == RecordKind::Rekey)
        .count();
    assert_eq!(rekeys, 1);

    recovered.close().await.unwrap();
}

#[tokio::test]
async fn stalled_lease_recovery_is_cancellable_and_reopen_remains_destination_only() {
    // Given: a committed rekey whose successful recovery is destination-only.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let checkpoints = mem_store();
    let durable_store = Arc::new(MemLeaseStore::new());
    let entity = PersistId::new(5_605);
    let holder = test_node(17);
    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, durable_store.clone())
        .await
        .unwrap();
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"cancellable-recovery",
    ))
    .await
    .unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(entity, cells[0], holder, ClaimKind::Strong, 70)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    let rekey = EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::ROOT,
        destination_cell: cells[1],
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::from_static(b"cancellable-recovery"),
    };
    Router::commit_rekey(&rt, rekey_record(&rekey))
        .await
        .unwrap();
    rt.close().await.unwrap();
    let stalled_store = Arc::new(StallingRecoveryLeaseStore::new());

    // When: the durable provider stalls while startup reconciles the committed rekey.
    {
        let stalled_open =
            CellRuntime::open_with_lease_store(&config, &checkpoints, stalled_store.clone());
        tokio::pin!(stalled_open);
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                () = stalled_store.migrate_entered.notified() => {}
                _ = &mut stalled_open => panic!("stalled provider unexpectedly completed"),
            }
        })
        .await
        .expect("stalled recovery remains cancellable");
        assert_eq!(stalled_store.active_migrations.load(Ordering::Acquire), 1);
    }

    // Then: cancellation drops the in-flight provider future, and a clean retry
    // recovers exactly one destination image without replaying the source.
    assert_eq!(stalled_store.active_migrations.load(Ordering::Acquire), 0);
    let recovered = tokio::time::timeout(
        Duration::from_secs(1),
        CellRuntime::open_with_lease_store(&config, &checkpoints, durable_store.clone()),
    )
    .await
    .expect("recovery retry remains bounded")
    .unwrap();
    assert!(recovered
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        recovered
            .read(GridId::ROOT, cells[1])
            .await
            .unwrap()
            .entities[&entity]
            .components
            .as_ref(),
        b"cancellable-recovery"
    );
    recovered.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_checkpoint_recovery_is_cancellable_and_retry_succeeds() {
    // Given: a checkpointed entity followed by a newer journal-only image.
    let dir = tempfile::tempdir().unwrap();
    let config = runtime_config(dir.path(), false);
    let store = Arc::new(StallingCheckpointStore::new());
    let checkpoints: Arc<dyn CheckpointStore> = store.clone();
    let entity = PersistId::new(5_606);
    let rt =
        CellRuntime::open_with_lease_store(&config, &checkpoints, Arc::new(MemLeaseStore::new()))
            .await
            .unwrap();
    rt.apply(mk_record(
        CellId::ROOT,
        entity.0,
        RecordKind::Spawn,
        b"checkpoint-image",
    ))
    .await
    .unwrap();
    rt.checkpoint(store.as_ref()).await.unwrap();
    rt.apply(mk_record(
        CellId::ROOT,
        entity.0,
        RecordKind::ComponentDiff,
        b"journal-tail-image",
    ))
    .await
    .unwrap();
    rt.close().await.unwrap();

    // When: checkpoint loading stalls and the supervising task cancels startup.
    store.stall_load.store(true, Ordering::Release);
    let init_config = config.clone();
    let init_checkpoints = Arc::clone(&checkpoints);
    let mut init_task =
        tokio::spawn(async move { CellRuntime::open(&init_config, &init_checkpoints).await });
    tokio::time::timeout(Duration::from_secs(1), store.load_entered.notified())
        .await
        .expect("checkpoint provider is entered before the supervisor deadline");
    assert_eq!(store.active_loads.load(Ordering::Acquire), 1);
    init_task.abort();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), &mut init_task).await;
    let active_after_cancel = store.active_loads.load(Ordering::Acquire);

    // Then: cancellation drops the provider future, and the same journal can
    // be reopened to recover the newer tail over the checkpoint base.
    assert!(
        matches!(cancelled, Ok(Err(error)) if error.is_cancelled()),
        "aborting initialization must not wait for the checkpoint provider"
    );
    assert_eq!(active_after_cancel, 0, "checkpoint load future was dropped");
    store.stall_load.store(false, Ordering::Release);
    let recovered = tokio::time::timeout(
        Duration::from_secs(1),
        CellRuntime::open_with_lease_store(&config, &checkpoints, Arc::new(MemLeaseStore::new())),
    )
    .await
    .expect("checkpoint recovery retry remains bounded")
    .unwrap();
    let page = recovered.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(page.entities.len(), 1);
    assert_eq!(
        page.entities[&entity].components.as_ref(),
        b"journal-tail-image"
    );
    recovered.close().await.unwrap();
}

#[tokio::test]
async fn committed_rekey_mem_recovery_has_one_destination_row() {
    // Given: a real Mem-backed source entity with a durable strong lease.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let checkpoints = mem_store();
    let store = Arc::new(MemLeaseStore::new());
    let entity = PersistId::new(5_614);
    let holder = test_node(25);
    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"durable-mem-rekey",
    ))
    .await
    .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cells[0],
        entity,
        holder,
        ClaimKind::Strong,
        0,
    )
    .await
    .unwrap() else {
        panic!("source lease must be granted");
    };

    // When: the server commits the exact source image to its sibling cell.
    Router::commit_rekey(
        &rt,
        rekey_record(&EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity,
            source_grid: GridId::ROOT,
            source_cell: cells[0],
            destination_grid: GridId::ROOT,
            destination_cell: cells[1],
            expected_lease_id: grant.lease_id,
            source_record: bytes::Bytes::from_static(b"durable-mem-rekey"),
        }),
    )
    .await
    .unwrap();

    // Then: direct durable reads expose one destination row and no source row.
    assert!(store
        .load_cell(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.load_cell(GridId::ROOT, cells[1]).await.unwrap(),
        vec![(cells[1], grant.clone())]
    );
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[1])
    );

    rt.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let recovered = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    let destination = recovered.actor(GridId::ROOT, cells[1]).unwrap().clone();
    let restored = destination
        .validate_lease(entity, holder, grant.lease_id, 0)
        .await
        .unwrap()
        .expect("recreated destination actor restores its fence");

    assert_eq!(restored.holder, grant.holder);
    assert_eq!(restored.lease_id, grant.lease_id);
    assert_eq!(restored.seq, grant.seq);
    assert!(restored.expires_at > grant.expires_at);
    assert_eq!(
        store.load_cell(GridId::ROOT, cells[1]).await.unwrap(),
        vec![(cells[1], restored)]
    );
    assert!(store
        .load_cell(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[1])
    );

    recovered.close().await.unwrap();
}

#[tokio::test]
async fn mem_rekey_stale_expected_source_preserves_source_durable_row() {
    // Given: a real actor claim has established the source durable row.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1], cells[2]];
    let store = Arc::new(MemLeaseStore::new());
    let entity = PersistId::new(5_615);
    let holder = test_node(26);
    let rt = CellRuntime::open_with_lease_store(&config, &mem_store(), store.clone())
        .await
        .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cells[0],
        entity,
        holder,
        ClaimKind::Weak,
        0,
    )
    .await
    .unwrap() else {
        panic!("source lease must be granted");
    };

    // When: a migration presents a sibling cell as its stale expected source.
    let outcome = store
        .migrate(GridId::ROOT, entity, cells[1], cells[2], grant.lease_id)
        .await
        .unwrap();

    // Then: direct durable reads still expose exactly the original source row.
    assert_eq!(
        outcome,
        LeaseMigrate::SourceMismatch {
            actual: Some(cells[0])
        }
    );
    assert_eq!(
        store.load_cell(GridId::ROOT, cells[0]).await.unwrap(),
        vec![(cells[0], grant)]
    );
    assert!(store
        .load_cell(GridId::ROOT, cells[1])
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .load_cell(GridId::ROOT, cells[2])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[0])
    );

    rt.close().await.unwrap();
}

#[tokio::test]
async fn mem_rekey_stale_expected_token_preserves_source_durable_row() {
    // Given: a real actor claim has established the source durable row.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let store = Arc::new(MemLeaseStore::new());
    let entity = PersistId::new(5_616);
    let holder = test_node(27);
    let rt = CellRuntime::open_with_lease_store(&config, &mem_store(), store.clone())
        .await
        .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cells[0],
        entity,
        holder,
        ClaimKind::Strong,
        0,
    )
    .await
    .unwrap() else {
        panic!("source lease must be granted");
    };

    // When: a migration presents the current source with a stale fencing token.
    let outcome = store
        .migrate(
            GridId::ROOT,
            entity,
            cells[0],
            cells[1],
            LeaseId(grant.lease_id.0 + 1),
        )
        .await
        .unwrap();

    // Then: direct durable reads still expose exactly the original source row.
    assert_eq!(
        outcome,
        LeaseMigrate::LeaseIdMismatch {
            actual: grant.lease_id
        }
    );
    assert_eq!(
        store.load_cell(GridId::ROOT, cells[0]).await.unwrap(),
        vec![(cells[0], grant)]
    );
    assert!(store
        .load_cell(GridId::ROOT, cells[1])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[0])
    );

    rt.close().await.unwrap();
}

#[tokio::test]
async fn rekeyed_entity_routes_current_fence_to_destination_and_rejects_stale_cell() {
    // Given: a real source entity and lease that have been committed into a sibling cell.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let store = Arc::new(MemLeaseStore::new());
    let entity = PersistId::new(5_611);
    let holder = test_node(21);
    let rt = CellRuntime::open_with_lease_store(&config, &mem_store(), store)
        .await
        .unwrap();
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"before-rekey",
    ))
    .await
    .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cells[0],
        entity,
        holder,
        ClaimKind::Strong,
        10,
    )
    .await
    .unwrap() else {
        panic!("source lease must be granted");
    };
    Router::commit_rekey(
        &rt,
        rekey_record(&EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity,
            source_grid: GridId::ROOT,
            source_cell: cells[0],
            destination_grid: GridId::ROOT,
            destination_cell: cells[1],
            expected_lease_id: grant.lease_id,
            source_record: bytes::Bytes::from_static(b"before-rekey"),
        }),
    )
    .await
    .unwrap();

    // When: the holder first presents the stale source cell, then the committed destination.
    let journal_before_stale = rt.journal().scan_from(Lsn::new(0, 0)).count();
    let stale = Router::apply_fenced(
        &rt,
        mk_record(cells[0], entity.0, RecordKind::ComponentDiff, b"stale-cell"),
        holder,
        grant.lease_id,
        grant.seq,
        11,
    )
    .await
    .unwrap();
    let FencedApply::Rejected(Some(current)) = stale else {
        panic!("stale presented cell must be rejected with the current lease");
    };
    let accepted = Router::apply_fenced(
        &rt,
        mk_record(
            cells[1],
            entity.0,
            RecordKind::ComponentDiff,
            b"destination-diff",
        ),
        holder,
        grant.lease_id,
        grant.seq,
        11,
    )
    .await
    .unwrap();
    let FencedApply::Accepted(handle) = accepted else {
        panic!("current fence at the committed destination must be admitted");
    };
    handle.committed().await.unwrap();

    // Then: rejection identifies the live fence, journals nothing, and the destination applies.
    assert_eq!(current, grant);
    assert_eq!(
        rt.journal().scan_from(Lsn::new(0, 0)).count(),
        journal_before_stale + 1
    );
    assert!(rt
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        rt.read(GridId::ROOT, cells[1]).await.unwrap().entities[&entity]
            .components
            .as_ref(),
        b"destination-diff"
    );

    rt.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_entity_rekey_racing_old_cell_fence_admits_exactly_one_path() {
    // Given: a source image whose current holder can either write it or move it.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let entity = PersistId::new(5_612);
    let holder = test_node(24);
    let rt =
        CellRuntime::open_with_lease_store(&config, &mem_store(), Arc::new(MemLeaseStore::new()))
            .await
            .unwrap();
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"race-source",
    ))
    .await
    .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cells[0],
        entity,
        holder,
        ClaimKind::Strong,
        30,
    )
    .await
    .unwrap() else {
        panic!("source lease must be granted");
    };
    let rekey = rekey_record(&EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::ROOT,
        destination_cell: cells[1],
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::from_static(b"race-source"),
    });
    let old_cell_diff = mk_record(
        cells[0],
        entity.0,
        RecordKind::ComponentDiff,
        b"old-cell-winner",
    );

    // When: the trusted rekey and the old-cell fenced write start concurrently.
    let (rekey_result, diff_result) = tokio::join!(
        Router::commit_rekey(&rt, rekey),
        Router::apply_fenced(&rt, old_cell_diff, holder, grant.lease_id, grant.seq, 31,)
    );
    let diff_result = diff_result.unwrap();
    if let FencedApply::Accepted(handle) = &diff_result {
        handle.committed().await.unwrap();
    }

    // Then: serialization admits one mutation, and a moved-path rejection carries the live row.
    let rekey_accepted = rekey_result.is_ok();
    let diff_accepted = matches!(diff_result, FencedApply::Accepted(_));
    assert_ne!(rekey_accepted, diff_accepted);
    if let FencedApply::Rejected(Some(current)) = diff_result {
        assert_eq!(current, grant);
        assert!(rekey_accepted);
    }
    let mutation_records = rt
        .journal()
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .filter(|record| {
            record.tick == Tick::new(7) || record.payload.as_ref() == b"old-cell-winner"
        })
        .count();
    assert_eq!(mutation_records, 1);

    rt.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrelated_entities_continue_while_another_entity_claim_is_blocked() {
    // Given: sibling actors and a real in-memory lease tier that pauses one entity's put.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let blocked = PersistId::new(5_620);
    let unrelated = PersistId::new(5_621);
    let store = Arc::new(BlockingPutLeaseStore::new(blocked));
    let rt = Arc::new(
        CellRuntime::open_with_lease_store(&config, &mem_store(), store.clone())
            .await
            .unwrap(),
    );

    // When: one claim is paused inside its actor-owned durable transition.
    let blocked_rt = Arc::clone(&rt);
    let blocked_claim = tokio::spawn(async move {
        Router::claim_lease(
            blocked_rt.as_ref(),
            GridId::ROOT,
            cells[0],
            blocked,
            test_node(22),
            ClaimKind::Weak,
            20,
        )
        .await
    });
    store.entered.notified().await;
    let unrelated_result = tokio::time::timeout(
        Duration::from_millis(250),
        Router::claim_lease(
            rt.as_ref(),
            GridId::ROOT,
            cells[1],
            unrelated,
            test_node(23),
            ClaimKind::Weak,
            20,
        ),
    )
    .await;

    // Then: the unrelated stripe completes without waiting for the paused entity.
    assert!(matches!(unrelated_result, Ok(Ok(ClaimResult::Granted(_)))));
    store.release.notify_one();
    assert!(matches!(
        blocked_claim.await.unwrap().unwrap(),
        ClaimResult::Granted(_)
    ));

    Arc::try_unwrap(rt).ok().unwrap().close().await.unwrap();
}

#[tokio::test]
async fn committed_rekey_migration_failure_preserves_exact_source_only() {
    // Given: two actors and a durable source whose lease store rejects migration.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let store = Arc::new(FailMigrateLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(&config, &mem_store(), store.clone())
        .await
        .unwrap();
    let entity = PersistId::new(5_602);
    let holder = test_node(14);
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"source-only",
    ))
    .await
    .unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(entity, cells[0], holder, ClaimKind::Weak, 30)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    let rekey = EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::ROOT,
        destination_cell: cells[1],
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::from_static(b"source-only"),
    };

    // When: the committed handoff reaches the injected durable migration failure.
    let outcome = Router::commit_rekey(&rt, rekey_record(&rekey)).await;

    // Then: hot state and the durable index still expose exactly the source copy and fence.
    assert_eq!(outcome, Err(RekeyError::LeaseStore));
    let source_page = rt.read(GridId::ROOT, cells[0]).await.unwrap();
    assert_eq!(
        source_page.entities[&entity].components.as_ref(),
        b"source-only"
    );
    assert!(rt
        .read(GridId::ROOT, cells[1])
        .await
        .unwrap()
        .entities
        .is_empty());
    let source_lease = source
        .validate_lease(entity, holder, grant.lease_id, 31)
        .await
        .unwrap()
        .expect("source fence remains");
    assert_eq!(source_lease, grant);
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[0])
    );

    rt.close().await.unwrap();
    store.fail_migrate.store(false, Ordering::Release);
    let recovered = CellRuntime::open_with_lease_store(&config, &mem_store(), store.clone())
        .await
        .unwrap();
    assert!(recovered
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        recovered
            .read(GridId::ROOT, cells[1])
            .await
            .unwrap()
            .entities[&entity]
            .components
            .as_ref(),
        b"source-only"
    );
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[1])
    );
    recovered.close().await.unwrap();
}

#[tokio::test]
async fn committed_rekey_migration_failure_fences_source_until_restart_recovery() {
    // Given: a real source actor whose committed rekey cannot migrate its lease store.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let checkpoints = mem_store();
    let store = Arc::new(FailMigrateLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    let entity = PersistId::new(5_604);
    let holder = test_node(16);
    rt.apply(mk_record(
        cells[0],
        entity.0,
        RecordKind::Spawn,
        b"committed-source-image",
    ))
    .await
    .unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(entity, cells[0], holder, ClaimKind::Strong, 50)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    let rekey = EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::ROOT,
        destination_cell: cells[1],
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::from_static(b"committed-source-image"),
    };

    // When: durable append succeeds but migration fails, then a source mutation is attempted.
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&rekey)).await,
        Err(RekeyError::LeaseStore)
    );
    assert_eq!(
        rt.apply(mk_record(
            cells[0],
            entity.0,
            RecordKind::ComponentDiff,
            b"must-not-replay-at-source",
        ))
        .await,
        Err(Reject::JournalClosed)
    );

    // Then: the source remains the sole immediate copy, its durable rekey has an LSN, and
    // restarting reconciles the same fence and bytes to the destination only.
    assert_eq!(
        rt.read(GridId::ROOT, cells[0]).await.unwrap().entities[&entity]
            .components
            .as_ref(),
        b"committed-source-image"
    );
    assert!(rt
        .read(GridId::ROOT, cells[1])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        source
            .validate_lease(entity, holder, grant.lease_id, 51)
            .await
            .unwrap(),
        Some(grant.clone())
    );
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[0])
    );
    let records: Vec<_> = rt
        .journal()
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .collect();
    let rekey_record = records
        .iter()
        .find(|record| record.kind == RecordKind::Rekey)
        .expect("durable committed rekey");
    assert!(rekey_record.lsn >= Lsn::new(0, 0));
    assert_eq!(rekey_record.entity, entity);
    assert_eq!(
        records
            .iter()
            .filter(|record| {
                record.entity == entity && record.payload.as_ref() == b"must-not-replay-at-source"
            })
            .count(),
        0
    );

    rt.close().await.unwrap();
    store.fail_migrate.store(false, Ordering::Release);
    let recovered = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    assert!(recovered
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        recovered
            .read(GridId::ROOT, cells[1])
            .await
            .unwrap()
            .entities[&entity]
            .components
            .as_ref(),
        b"committed-source-image"
    );
    let destination = recovered.actor(GridId::ROOT, cells[1]).unwrap().clone();
    let recovered_lease = destination
        .validate_lease(entity, holder, grant.lease_id, 0)
        .await
        .unwrap()
        .expect("recovered destination fence");
    assert_eq!(recovered_lease.holder, grant.holder);
    assert_eq!(recovered_lease.lease_id, grant.lease_id);
    assert_eq!(recovered_lease.seq, grant.seq);
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[1])
    );
    recovered.close().await.unwrap();
    let recovered_again = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    assert!(recovered_again
        .read(GridId::ROOT, cells[0])
        .await
        .unwrap()
        .entities
        .is_empty());
    assert_eq!(
        recovered_again
            .read(GridId::ROOT, cells[1])
            .await
            .unwrap()
            .entities[&entity]
            .components
            .as_ref(),
        b"committed-source-image"
    );
    let recovered_again_lease = recovered_again
        .actor(GridId::ROOT, cells[1])
        .unwrap()
        .validate_lease(entity, holder, grant.lease_id, 0)
        .await
        .unwrap()
        .expect("idempotent recovered destination fence");
    assert_eq!(recovered_again_lease.holder, grant.holder);
    assert_eq!(recovered_again_lease.lease_id, grant.lease_id);
    assert_eq!(recovered_again_lease.seq, grant.seq);
    assert_eq!(
        store.locate(GridId::ROOT, entity).await.unwrap(),
        Some(cells[1])
    );
    recovered_again.close().await.unwrap();
}

#[tokio::test]
async fn committed_rekey_rejects_stale_source_image_and_fence_before_append() {
    // Given: a live source whose actor image and lease fence are authoritative.
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let mut config = runtime_config(dir.path(), false);
    config.shards = vec![cells[0], cells[1]];
    let store = Arc::new(MemLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(&config, &mem_store(), store)
        .await
        .unwrap();
    let entity = PersistId::new(5_603);
    let holder = test_node(15);
    rt.apply(mk_record(cells[0], entity.0, RecordKind::Spawn, b"current"))
        .await
        .unwrap();
    let source = rt.actor(GridId::ROOT, cells[0]).unwrap().clone();
    let ClaimResult::Granted(grant) = source
        .claim_lease(entity, cells[0], holder, ClaimKind::Weak, 40)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    let base = EntityRekey {
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: cells[0],
        destination_grid: GridId::ROOT,
        destination_cell: cells[1],
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::from_static(b"current"),
    };

    // When: stale actor bytes and then a stale fence are presented to the trusted entrypoint.
    let mut stale_image = base.clone();
    stale_image.source_record = bytes::Bytes::from_static(b"stale");
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&stale_image)).await,
        Err(RekeyError::SourceRecordMismatch)
    );
    let mut stale_fence = base;
    stale_fence.expected_lease_id = LeaseId(grant.lease_id.0 + 1);
    assert_eq!(
        Router::commit_rekey(&rt, rekey_record(&stale_fence)).await,
        Err(RekeyError::FenceMismatch)
    );

    // Then: neither rejected request produced a durable committed-rekey record.
    let rekey_count = rt
        .journal()
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .filter(|record| record.kind == RecordKind::Rekey)
        .count();
    assert_eq!(rekey_count, 0);
    assert_eq!(
        rt.read(GridId::ROOT, cells[0]).await.unwrap().entities[&entity]
            .components
            .as_ref(),
        b"current"
    );
    assert!(rt
        .read(GridId::ROOT, cells[1])
        .await
        .unwrap()
        .entities
        .is_empty());

    rt.close().await.unwrap();
}

/// A fresh in-memory checkpoint store as the trait object `CellRuntime::open`
/// takes.
fn mem_store() -> Arc<dyn CheckpointStore> {
    Arc::new(MemCheckpointStore::new())
}

/// A registrar tier which can fail writes on demand, used to prove that a
/// rejected durable transition never leaks into actor-owned hot state.
struct ToggleLeaseStore {
    fail_put: AtomicBool,
}

#[derive(Default)]
struct CountingLeaseStore {
    puts: AtomicUsize,
}

struct BlockingLeaseStore {
    block_put: AtomicBool,
    release_put: Notify,
}

struct BlockingPutLeaseStore {
    inner: MemLeaseStore,
    blocked_entity: PersistId,
    should_block: AtomicBool,
    entered: Notify,
    release: Notify,
}

struct FailMigrateLeaseStore {
    inner: MemLeaseStore,
    fail_migrate: AtomicBool,
}

struct StallingRecoveryLeaseStore {
    inner: MemLeaseStore,
    migrate_entered: Notify,
    active_migrations: AtomicUsize,
}

struct StallingCheckpointStore {
    inner: MemCheckpointStore,
    stall_load: AtomicBool,
    load_entered: Notify,
    release_load: Notify,
    active_loads: AtomicUsize,
}

struct ActiveCheckpointLoad<'a>(&'a AtomicUsize);

impl Drop for ActiveCheckpointLoad<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ActiveMigration<'a>(&'a AtomicUsize);

impl Drop for ActiveMigration<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl FailMigrateLeaseStore {
    fn new() -> Self {
        Self {
            inner: MemLeaseStore::new(),
            fail_migrate: AtomicBool::new(true),
        }
    }
}

impl StallingRecoveryLeaseStore {
    fn new() -> Self {
        Self {
            inner: MemLeaseStore::new(),
            migrate_entered: Notify::new(),
            active_migrations: AtomicUsize::new(0),
        }
    }
}

impl StallingCheckpointStore {
    fn new() -> Self {
        Self {
            inner: MemCheckpointStore::new(),
            stall_load: AtomicBool::new(false),
            load_entered: Notify::new(),
            release_load: Notify::new(),
            active_loads: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl CheckpointStore for StallingCheckpointStore {
    async fn checkpoint(&self, data: &CheckpointData) -> Result<(), CheckpointError> {
        self.inner.checkpoint(data).await
    }

    async fn load(
        &self,
        shard: CellId,
        grid: GridId,
    ) -> Result<Option<CheckpointData>, CheckpointError> {
        if self.stall_load.load(Ordering::Acquire) {
            self.active_loads.fetch_add(1, Ordering::AcqRel);
            let _active = ActiveCheckpointLoad(&self.active_loads);
            self.load_entered.notify_one();
            self.release_load.notified().await;
        }
        self.inner.load(shard, grid).await
    }

    async fn delete(&self, shard: CellId, grid: GridId) -> Result<(), CheckpointError> {
        self.inner.delete(shard, grid).await
    }
}

#[async_trait::async_trait]
impl LeaseStore for StallingRecoveryLeaseStore {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        self.inner.load_cell(grid, shard).await
    }

    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        self.inner.put(grid, cell, lease).await
    }

    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        self.inner.locate(grid, entity).await
    }

    async fn migrate(
        &self,
        _grid: GridId,
        _entity: PersistId,
        _from: CellId,
        _to: CellId,
        _expected_lease_id: LeaseId,
    ) -> Result<LeaseMigrate, LeaseStoreError> {
        self.active_migrations.fetch_add(1, Ordering::AcqRel);
        let _active = ActiveMigration(&self.active_migrations);
        self.migrate_entered.notify_one();
        std::future::pending().await
    }

    async fn remove(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        self.inner.remove(grid, cell, entity).await
    }
}

#[async_trait::async_trait]
impl LeaseStore for FailMigrateLeaseStore {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        self.inner.load_cell(grid, shard).await
    }

    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        self.inner.put(grid, cell, lease).await
    }

    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        self.inner.locate(grid, entity).await
    }

    async fn migrate(
        &self,
        grid: GridId,
        entity: PersistId,
        from: CellId,
        to: CellId,
        expected_lease_id: LeaseId,
    ) -> Result<LeaseMigrate, LeaseStoreError> {
        if self.fail_migrate.load(Ordering::Acquire) {
            return Err(LeaseStoreError("injected migrate failure".into()));
        }
        self.inner
            .migrate(grid, entity, from, to, expected_lease_id)
            .await
    }

    async fn remove(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        self.inner.remove(grid, cell, entity).await
    }
}

impl BlockingLeaseStore {
    fn new() -> Self {
        Self {
            block_put: AtomicBool::new(false),
            release_put: Notify::new(),
        }
    }
}

impl BlockingPutLeaseStore {
    fn new(blocked_entity: PersistId) -> Self {
        Self {
            inner: MemLeaseStore::new(),
            blocked_entity,
            should_block: AtomicBool::new(true),
            entered: Notify::new(),
            release: Notify::new(),
        }
    }
}

impl CountingLeaseStore {
    fn put_count(&self) -> usize {
        self.puts.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl LeaseStore for CountingLeaseStore {
    async fn load_cell(
        &self,
        _grid: GridId,
        _shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        Ok(Vec::new())
    }

    async fn put(
        &self,
        _grid: GridId,
        _cell: CellId,
        _lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        self.puts.fetch_add(1, Ordering::AcqRel);
        Ok(LeasePut::Stored)
    }

    async fn locate(
        &self,
        _grid: GridId,
        _entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        Ok(None)
    }

    async fn remove(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl LeaseStore for BlockingLeaseStore {
    async fn load_cell(
        &self,
        _grid: GridId,
        _shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        Ok(Vec::new())
    }

    async fn put(
        &self,
        _grid: GridId,
        _cell: CellId,
        _lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        if self.block_put.load(Ordering::Acquire) {
            self.release_put.notified().await;
        }
        Ok(LeasePut::Stored)
    }

    async fn locate(
        &self,
        _grid: GridId,
        _entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        Ok(None)
    }

    async fn remove(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl LeaseStore for BlockingPutLeaseStore {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        self.inner.load_cell(grid, shard).await
    }

    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        if lease.entity == self.blocked_entity && self.should_block.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.put(grid, cell, lease).await
    }

    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        self.inner.locate(grid, entity).await
    }

    async fn migrate(
        &self,
        grid: GridId,
        entity: PersistId,
        from: CellId,
        to: CellId,
        expected_lease_id: LeaseId,
    ) -> Result<LeaseMigrate, LeaseStoreError> {
        self.inner
            .migrate(grid, entity, from, to, expected_lease_id)
            .await
    }

    async fn remove(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        self.inner.remove(grid, cell, entity).await
    }
}

impl ToggleLeaseStore {
    fn new() -> Self {
        Self {
            fail_put: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl LeaseStore for ToggleLeaseStore {
    async fn load_cell(
        &self,
        _grid: GridId,
        _shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        Ok(Vec::new())
    }

    async fn put(
        &self,
        _grid: GridId,
        _cell: CellId,
        _lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        if self.fail_put.load(Ordering::Acquire) {
            Err(LeaseStoreError("injected put failure".into()))
        } else {
            Ok(LeasePut::Stored)
        }
    }

    async fn locate(
        &self,
        _grid: GridId,
        _entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        Ok(None)
    }

    async fn remove(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn actor_applies_and_snapshot_reflects() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
        .await
        .unwrap();

    let rec = mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"hp=100");
    let lsn = rt.apply(rec.clone()).await.unwrap();
    assert!(lsn >= Lsn::new(0, 0), "lsn is valid: {lsn:?}");

    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    let rec2 = mk_record(CellId::ROOT, 7, RecordKind::ComponentDiff, b"hp=50");
    rt.apply(rec2).await.unwrap();
    let page2 = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    let e = &page2.entities[&PersistId::new(7)];
    assert_eq!(e.components.as_ref(), b"hp=50");
    let _ = page;

    rt.close().await.unwrap();
}

#[tokio::test]
async fn failed_lease_store_transition_leaves_registrar_hot_state_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ToggleLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(991);
    let holder = test_node(9);

    assert_eq!(
        actor
            .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 10)
            .await,
        Err(Reject::LeaseStore)
    );

    store.fail_put.store(false, Ordering::Release);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 10)
        .await
        .expect("retry after durable tier recovery")
    else {
        panic!("fresh claim should be granted");
    };
    assert_eq!(grant.lease_id.0, 1, "failed claim must not consume token");
    assert_eq!(
        grant.seq.auth_seq, 1,
        "failed claim must not advance sequence"
    );

    rt.close().await.unwrap();
}

#[tokio::test]
async fn heartbeat_renews_hot_expiry_without_persisting_a_new_lease_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingLeaseStore::default());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(990);
    let holder = test_node(8);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    let renewed = actor
        .heartbeat_lease(entity, holder, grant.lease_id, 500)
        .await
        .unwrap()
        .expect("valid heartbeat returns its renewed row");

    assert_eq!(renewed.expires_at, 500 + LEASE_TTL_MS);
    assert_eq!(renewed.lease_id, grant.lease_id);
    assert_eq!(renewed.seq, grant.seq);
    assert_eq!(store.put_count(), 1, "only the initial claim is durable");

    rt.close().await.unwrap();
}

#[tokio::test]
async fn heartbeat_does_not_wait_for_a_blocked_durable_store_or_block_the_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(BlockingLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(988);
    let holder = test_node(6);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    store.block_put.store(true, Ordering::Release);
    let renewed = tokio::time::timeout(
        Duration::from_millis(50),
        actor.heartbeat_lease(entity, holder, grant.lease_id, 500),
    )
    .await;
    let current = tokio::time::timeout(
        Duration::from_millis(50),
        actor.validate_lease(entity, holder, grant.lease_id, 501),
    )
    .await;
    store.release_put.notify_waiters();

    let renewed = renewed
        .expect("heartbeat must not wait for durable storage")
        .unwrap()
        .expect("valid heartbeat returns its renewed row");
    let current = current
        .expect("following mailbox request must not wait for durable storage")
        .unwrap()
        .expect("current lease row");
    assert_eq!(renewed.expires_at, 500 + LEASE_TTL_MS);
    assert_eq!(current, renewed);

    rt.close().await.unwrap();
}

#[tokio::test]
async fn heartbeat_with_stale_token_returns_current_row_without_persisting() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingLeaseStore::default());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(989);
    let holder = test_node(7);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    let current = actor
        .heartbeat_lease(
            entity,
            holder,
            orrery_protocol::LeaseId(grant.lease_id.0 + 1),
            500,
        )
        .await
        .unwrap()
        .expect("stale heartbeat returns the current row");

    assert_eq!(current, grant);
    assert_eq!(store.put_count(), 1, "stale heartbeat is not durable");

    rt.close().await.unwrap();
}

#[tokio::test]
async fn heartbeat_with_wrong_holder_returns_current_row_without_persisting() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingLeaseStore::default());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(987);
    let holder = test_node(5);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    let current = actor
        .heartbeat_lease(entity, test_node(4), grant.lease_id, 500)
        .await
        .unwrap()
        .expect("wrong-holder heartbeat returns the current row");

    assert_eq!(current, grant);
    assert_eq!(
        store.put_count(),
        1,
        "wrong-holder heartbeat is not durable"
    );

    rt.close().await.unwrap();
}

#[tokio::test]
async fn heartbeat_after_expiry_returns_current_row_without_persisting() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingLeaseStore::default());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(986);
    let holder = test_node(3);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    let current = actor
        .heartbeat_lease(entity, holder, grant.lease_id, grant.expires_at)
        .await
        .unwrap()
        .expect("expired heartbeat returns the current row");

    assert_eq!(current, grant);
    assert_eq!(store.put_count(), 1, "expired heartbeat is not durable");

    rt.close().await.unwrap();
}

#[tokio::test]
async fn heartbeat_survives_store_failure_while_durable_park_and_expiry_stay_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ToggleLeaseStore::new());
    store.fail_put.store(false, Ordering::Release);
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(985);
    let holder = test_node(2);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    store.fail_put.store(true, Ordering::Release);
    let renewed = actor
        .heartbeat_lease(entity, holder, grant.lease_id, 500)
        .await
        .unwrap()
        .expect("hot heartbeat succeeds despite durable-store failure");
    assert_eq!(renewed.expires_at, 500 + LEASE_TTL_MS);
    assert_eq!(
        actor.park_lease(entity, holder, grant.lease_id).await,
        Err(Reject::LeaseStore)
    );
    assert_eq!(
        actor.sweep_leases(renewed.expires_at).await,
        Err(Reject::LeaseStore)
    );
    let current = actor
        .validate_lease(entity, holder, grant.lease_id, 501)
        .await
        .unwrap()
        .expect("failed durable transitions retain their hot row");

    assert_eq!(current, renewed);

    rt.close().await.unwrap();
}

#[tokio::test]
async fn disconnect_parks_the_durable_row_and_exposes_it_for_stale_token_nacks() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(992);
    let holder = test_node(10);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 10)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    let parked = actor
        .park_lease(entity, holder, grant.lease_id)
        .await
        .unwrap()
        .expect("current lease is parked");
    assert_eq!(parked.holder, None);
    assert!(parked.flags.contains(LeaseFlags::PARKED));
    assert!(parked.lease_id > grant.lease_id);
    // Validation intentionally returns the current row even for a stale
    // pair, so the gateway can attach it to a lease-specific `BulkNack`.
    let current = actor
        .validate_lease(entity, holder, grant.lease_id, 11)
        .await
        .unwrap()
        .expect("parked current row");
    assert_eq!(current, parked);

    let persisted = store.load_cell(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(persisted, vec![(CellId::ROOT, parked)]);

    rt.close().await.unwrap();
}

#[tokio::test]
async fn actor_recovery_preserves_fencing_identity_and_refreshes_lease_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemLeaseStore::new());
    let checkpoints = mem_store();
    let config = runtime_config(dir.path(), false);
    let entity = PersistId::new(993);
    let holder = test_node(11);

    let rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let ClaimResult::Granted(before_restart) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };
    rt.close().await.unwrap();

    // Ensure the registrar's process-monotonic clock has advanced before the
    // recreated actor writes its recovery TTL.
    tokio::time::sleep(Duration::from_millis(2)).await;
    let restored_rt = CellRuntime::open_with_lease_store(&config, &checkpoints, store.clone())
        .await
        .unwrap();
    let restored_actor = restored_rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("restored root actor")
        .clone();
    let after_restart = restored_actor
        .validate_lease(entity, holder, before_restart.lease_id, 0)
        .await
        .unwrap()
        .expect("recovered lease row");

    assert_eq!(after_restart.holder, before_restart.holder);
    assert_eq!(after_restart.lease_id, before_restart.lease_id);
    assert_eq!(after_restart.seq, before_restart.seq);
    assert!(
        after_restart.expires_at > before_restart.expires_at,
        "recovery refreshes, rather than reuses, the old TTL"
    );
    assert_eq!(
        store.load_cell(GridId::ROOT, CellId::ROOT).await.unwrap(),
        vec![(CellId::ROOT, after_restart)]
    );

    restored_rt.close().await.unwrap();
}

#[tokio::test]
async fn expiry_sweep_parks_the_durable_row_and_rotates_its_token() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemLeaseStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), false),
        &mem_store(),
        store.clone(),
    )
    .await
    .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(994);
    let holder = test_node(12);
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("claim should be granted");
    };

    let swept = actor.sweep_leases(grant.expires_at).await.unwrap();
    assert_eq!(swept.len(), 1);
    let parked = &swept[0];
    assert_eq!(parked.lease.entity, entity);
    assert_eq!(parked.lease.holder, None);
    assert!(parked.lease.flags.contains(LeaseFlags::PARKED));
    assert!(parked.lease.lease_id > grant.lease_id);
    // The sweep also reports who lost the lease and under which token, so a
    // successor policy can act without re-reading the registrar.
    assert_eq!(parked.previous_holder, holder);
    assert_eq!(parked.previous_lease_id, grant.lease_id);
    assert_eq!(parked.cell, CellId::ROOT);
    assert_eq!(parked.reason, orrery_protocol::ExpireReason::Timeout);
    assert_eq!(
        store.load_cell(GridId::ROOT, CellId::ROOT).await.unwrap(),
        vec![(CellId::ROOT, parked.lease.clone())]
    );

    rt.close().await.unwrap();
}

#[tokio::test]
async fn committed_entity_location_tracks_actor_hot_state_without_creating_a_lease() {
    // Given: an entity committed in one child cell of the root actor.
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(9940);
    let cells = CellId::ROOT.children();
    actor
        .start_diff(mk_record(cells[0], 9940, RecordKind::Spawn, b"spawn"))
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();

    // When: the committed location is read and then a later state record moves it.
    assert_eq!(
        Router::committed_entity_cell(&rt, GridId::ROOT, entity)
            .await
            .unwrap(),
        Some(cells[0])
    );
    actor
        .start_diff(mk_record(
            cells[1],
            9940,
            RecordKind::ComponentDiff,
            b"move",
        ))
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();

    // Then: lookup returns the new cell, rejects the wrong grid, and leaves leases absent.
    assert_eq!(
        Router::committed_entity_cell(&rt, GridId::ROOT, entity)
            .await
            .unwrap(),
        Some(cells[1])
    );
    assert_eq!(
        Router::committed_entity_cell(&rt, GridId::new(9), entity)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        Router::committed_entity_cell(&rt, GridId::ROOT, PersistId::new(9941))
            .await
            .unwrap(),
        None
    );
    assert!(actor
        .validate_lease(entity, test_node(1), orrery_protocol::LeaseId(0), 0)
        .await
        .unwrap()
        .is_none());

    rt.close().await.unwrap();
}

#[tokio::test]
async fn committed_entity_cell_rejects_a_cross_cell_claim() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(995);
    let cells = CellId::ROOT.children();
    let ClaimResult::Granted(first) = actor
        .claim_lease(entity, cells[0], test_node(13), ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("initial cell claim should be granted");
    };

    assert_eq!(
        actor
            .claim_lease(entity, cells[1], test_node(14), ClaimKind::Weak, 1)
            .await
            .unwrap(),
        ClaimResult::Denied(DenyReason::NotEligible)
    );
    let current = actor
        .validate_lease(entity, test_node(13), first.lease_id, 1)
        .await
        .unwrap()
        .expect("original row remains current");
    assert_eq!(current.lease_id, first.lease_id);
    assert_eq!(current.holder, Some(test_node(13)));

    rt.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_weak_claims_are_serialized_with_monotonic_fencing() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(996);
    let first = actor.claim_lease(entity, CellId::ROOT, test_node(15), ClaimKind::Weak, 0);
    let second = actor.claim_lease(entity, CellId::ROOT, test_node(16), ClaimKind::Weak, 0);
    let (first, second) = tokio::join!(first, second);
    let outcomes = [first.unwrap(), second.unwrap()];
    let granted: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ClaimResult::Granted(row) => Some(row.clone()),
            ClaimResult::Denied(_) => None,
        })
        .collect();
    let denied: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ClaimResult::Denied(reason) => Some(reason.clone()),
            ClaimResult::Granted(_) => None,
        })
        .collect();
    // The registrar arbitrates the race rather than serving it. Granting both
    // in turn is not "serialized": the second grant invalidates the token the
    // first claimant just installed, and the first claimant is never told —
    // it keeps simulating an entity somebody else now owns. Exactly one wins;
    // the loser is refused, and the refusal names the winner and its pair so
    // the loser rolls its optimistic claim back onto the right stream
    // (docs/04-authority.md §4.1).
    assert_eq!(granted.len(), 1, "one winner: {outcomes:?}");
    let winner = &granted[0];
    assert_eq!(winner.lease_id.0, 1);
    assert_eq!(winner.seq.auth_seq, 1);
    assert_eq!(
        denied,
        vec![DenyReason::Held {
            holder: winner.holder.unwrap(),
            seq: winner.seq,
        }],
        "the loser is told who won"
    );

    let current = actor
        .validate_lease(entity, winner.holder.unwrap(), winner.lease_id, 0)
        .await
        .unwrap()
        .expect("single durable winner row");
    assert_eq!(current, *winner);

    // The damper bounds the herd; it does not make weak authority sticky. A
    // claim arriving after the window is a real interaction and still takes
    // the lease, with the next fencing token and the next authority sequence.
    let steal_at = CLAIM_HERD_DAMPER_MS + 1;
    let ClaimResult::Granted(stolen) = actor
        .claim_lease(
            entity,
            CellId::ROOT,
            test_node(17),
            ClaimKind::Weak,
            steal_at,
        )
        .await
        .unwrap()
    else {
        panic!("a weak steal past the herd window is still granted");
    };
    assert_eq!(stolen.lease_id.0, 2);
    assert_eq!(stolen.seq.auth_seq, 2);

    rt.close().await.unwrap();
}

/// A whole heartbeat batch renews through one actor, pair by pair.
///
/// The gateway-side unit test measures the turn count; this one measures the
/// answer, against a real runtime: every valid pair renews, every invalid pair
/// is still named on its own, and the batch's outcome is positional so an ack
/// cannot be attributed to the wrong entity.
#[tokio::test]
async fn a_renewal_batch_renews_each_pair_against_its_own_row() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let holder = test_node(21);
    let stranger = test_node(22);
    let mut held = Vec::new();
    for id in 900..905u64 {
        let entity = PersistId::new(id);
        let ClaimResult::Granted(row) = rt
            .claim_lease(
                GridId::ROOT,
                CellId::ROOT,
                entity,
                holder,
                ClaimKind::Weak,
                0,
            )
            .await
            .unwrap()
        else {
            panic!("claim {id} should be granted");
        };
        held.push((entity, row.lease_id));
    }
    // A pair the holder no longer owns, and a pair for an entity with no row.
    let stale = (held[2].0, LeaseId(held[2].1 .0 + 1));
    let absent = (PersistId::new(999), LeaseId(1));
    let batch = vec![held[0], held[1], stale, held[3], absent, held[4]];

    let rows = rt
        .heartbeat_leases(GridId::ROOT, CellId::ROOT, holder, &batch, 1)
        .await
        .unwrap();

    assert_eq!(rows.len(), batch.len(), "one answer per requested pair");
    let renewed: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(index, row)| {
            row.as_ref()
                .is_some_and(|row| row.holder == Some(holder) && row.lease_id == batch[*index].1)
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        renewed,
        vec![0, 1, 3, 5],
        "exactly the pairs the peer holds"
    );
    // The stale pair still returns the row, so the holder learns the truth
    // rather than merely that something failed; the absent one has no row.
    assert_eq!(rows[2].as_ref().unwrap().lease_id, held[2].1);
    assert!(rows[4].is_none());

    // The renewal moved the TTL for the pairs it accepted, and only those.
    let extended = rt
        .inspect_lease(GridId::ROOT, held[0].0)
        .await
        .unwrap()
        .0
        .expect("renewed row");
    assert_eq!(extended.expires_at, 1 + LEASE_TTL_MS);

    // A batch from someone who holds none of it renews none of it.
    let rows = rt
        .heartbeat_leases(GridId::ROOT, CellId::ROOT, stranger, &batch, 2)
        .await
        .unwrap();
    assert!(rows
        .iter()
        .all(|row| row.as_ref().is_none_or(|row| row.holder != Some(stranger))));

    rt.close().await.unwrap();
}

#[tokio::test]
async fn strong_actor_lease_cannot_be_stolen_by_a_weak_claim() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(997);
    let holder = test_node(17);
    let ClaimResult::Granted(strong) = actor
        .claim_lease(entity, CellId::ROOT, holder, ClaimKind::Strong, 0)
        .await
        .unwrap()
    else {
        panic!("strong claim should be granted");
    };

    assert_eq!(
        actor
            .claim_lease(entity, CellId::ROOT, test_node(18), ClaimKind::Weak, 1)
            .await
            .unwrap(),
        ClaimResult::Denied(DenyReason::StrongHeld)
    );
    let current = actor
        .validate_lease(entity, holder, strong.lease_id, 1)
        .await
        .unwrap()
        .expect("strong row remains current");
    assert_eq!(current, strong);

    rt.close().await.unwrap();
}

#[tokio::test]
async fn fenced_append_rechecks_the_lease_inside_the_actor_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), false), &mem_store())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();
    let entity = PersistId::new(998);
    let original_holder = test_node(19);
    let ClaimResult::Granted(original) = actor
        .claim_lease(entity, CellId::ROOT, original_holder, ClaimKind::Weak, 0)
        .await
        .unwrap()
    else {
        panic!("initial claim should be granted");
    };
    // Past the claim-herd window, so this is an ordinary weak steal and not
    // the tail of a herd: the point under test is the fence recheck, which
    // needs a successor to exist at all.
    let ClaimResult::Granted(successor) = actor
        .claim_lease(
            entity,
            CellId::ROOT,
            test_node(20),
            ClaimKind::Weak,
            CLAIM_HERD_DAMPER_MS + 1,
        )
        .await
        .unwrap()
    else {
        panic!("weak successor should be granted");
    };

    let result = actor
        .start_fenced_diff(
            mk_record(CellId::ROOT, entity.0, RecordKind::Spawn, b"stale"),
            original_holder,
            original.lease_id,
            original.seq,
            CLAIM_HERD_DAMPER_MS + 1,
        )
        .await
        .unwrap();
    let FencedApply::Rejected(Some(current)) = result else {
        panic!("superseded token must be rejected before journal admission");
    };
    assert_eq!(current, successor);
    assert!(
        !rt.read(GridId::ROOT, CellId::ROOT)
            .await
            .unwrap()
            .entities
            .contains_key(&entity),
        "a rejected fenced append never reaches hot state"
    );

    rt.close().await.unwrap();
}

#[tokio::test]
async fn actor_returns_pending_handle_after_fold_without_resolver_task() {
    let dir = tempfile::tempdir().unwrap();
    // `runtime_config(..., true)` holds a one-record group for 100 ms, making
    // the boundary between actor work and durability deterministic.
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
        .await
        .unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();

    let handle = tokio::time::timeout(
        Duration::from_millis(20),
        actor.start_diff(mk_record(CellId::ROOT, 88, RecordKind::Spawn, b"pending")),
    )
    .await
    .expect("mailbox returns before the group fsync")
    .expect("append accepted");

    assert!(
        tokio::time::timeout(Duration::from_millis(20), handle.committed())
            .await
            .is_err(),
        "returned handle must still represent the pending durability wait"
    );
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(88)].components.as_ref(),
        b"pending",
        "fold precedes returning the pending handle"
    );

    let committed = handle.committed().await.unwrap();
    assert_eq!(committed, handle.lsn());
    rt.close().await.unwrap();
}

#[tokio::test]
async fn read_snapshot_filters_to_requested_cells() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
        .await
        .unwrap();

    let cell_a = CellId::from_coords(glam::IVec3::new(2, -1, 8), 21).unwrap();
    let cell_b = cell_a
        .neighbor(glam::IVec3::new(1, 0, 0))
        .expect("within the volume");

    // Two entities in neighbouring interest cells, both under the ROOT shard.
    rt.apply(mk_record(cell_a, 1, RecordKind::Spawn, b"a"))
        .await
        .unwrap();
    rt.apply(mk_record(cell_b, 2, RecordKind::Spawn, b"b"))
        .await
        .unwrap();

    // Reading one interest cell returns exactly that cell's entity (P-4).
    let page = rt.read(GridId::ROOT, cell_a).await.unwrap();
    assert_eq!(page.entities.len(), 1, "one interest cell reads one entity");
    assert!(page.entities.contains_key(&PersistId::new(1)));
    assert!(!page.entities.contains_key(&PersistId::new(2)));

    // Reading the covering shard serves the whole subtree, mirroring read_cold.
    let shard = cell_a
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("interest cell has a level-18 shard ancestor");
    let subtree = rt.read(GridId::ROOT, shard).await.unwrap();
    assert_eq!(subtree.entities.len(), 2, "shard read serves its subtree");

    rt.close().await.unwrap();
}

#[test]
fn crash_and_recover_zero_loss() {
    // Simulate `kill -9` by running the write phase in its own tokio runtime and
    // dropping it: dropping a runtime aborts all spawned tasks (actors + the
    // committer), releasing the journal file lock — exactly what process death
    // does. Acked writes survive because each batch was group-fsynced.
    let dir = tempfile::tempdir().unwrap();
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
                .await
                .unwrap();
            for i in 0..100u64 {
                let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
                rt.apply(rec).await.unwrap();
            }
        });
        // rt dropped here => tasks aborted; but the journal's file lock is
        // released only when the Last Arc<Journal> drops, so the phase-2 open
        // below must take it by closing the runtime, not by aborting.
    }

    // Restart from the same journal dir — no FDB, so the journal IS the truth.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
            .await
            .unwrap();
        let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert_eq!(page.entities.len(), 100, "all entities recovered");
        for i in 0..100u64 {
            let e = &page.entities[&PersistId::new(i)];
            assert_eq!(e.components.as_ref(), &i.to_le_bytes());
        }
        rt.close().await.unwrap();
    });
}

#[tokio::test]
async fn concurrent_diffs_batch_into_fewer_fsyncs() {
    // The mailbox must not serialize the node on one fsync: 64 concurrent
    // applies through the `Mutex<CellRuntime>` router pipeline into the
    // journal's commit queue and share fsyncs (§4 adaptive group commit).
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
        .await
        .unwrap();
    let router = Arc::new(tokio::sync::Mutex::new(rt));

    let mut waiters = Vec::new();
    for i in 0..64u64 {
        let router = Arc::clone(&router);
        waiters.push(tokio::spawn(async move {
            let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
            let append = orrery_persistd::Router::apply(router.as_ref(), rec)
                .await
                .unwrap();
            append.committed().await.unwrap()
        }));
    }
    for w in waiters {
        w.await.unwrap();
    }

    let fsyncs = router.lock().await.flush_count();
    assert!(
        fsyncs < 64,
        "64 concurrent applies must share fsyncs (got {fsyncs})"
    );
    // All 64 landed (one record each, last-writer-wins across distinct ids).
    let page = router
        .lock()
        .await
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .unwrap();
    assert_eq!(page.entities.len(), 64);
    let rt = Arc::try_unwrap(router)
        .unwrap_or_else(|_| panic!("router sole owner"))
        .into_inner();
    rt.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_diffs_stay_last_writer_wins() {
    // Same entity, sequential diffs through the concurrent path: the last
    // writer wins regardless of resolver interleaving (mailbox order is the
    // single-writer serial order, §3.1).
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
        .await
        .unwrap();

    rt.apply(mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"first"))
        .await
        .unwrap();
    rt.apply(mk_record(
        CellId::ROOT,
        7,
        RecordKind::ComponentDiff,
        b"second",
    ))
    .await
    .unwrap();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(7)].components.as_ref(),
        b"second",
        "the last acked writer wins"
    );

    // A concurrent burst of 16 writers of the SAME entity: the mailbox
    // serializes them, so the surviving value must be exactly one of the 16
    // acked payloads (never a torn mix).
    let router = Arc::new(tokio::sync::Mutex::new(rt));
    let mut waiters = Vec::new();
    for i in 0..16u64 {
        let router = Arc::clone(&router);
        let payload = format!("burst-{i}").into_bytes();
        waiters.push(tokio::spawn(async move {
            let rec = mk_record(CellId::ROOT, 9, RecordKind::ComponentDiff, &payload);
            let append = orrery_persistd::Router::apply(router.as_ref(), rec)
                .await
                .unwrap();
            append.committed().await.unwrap();
            payload
        }));
    }
    let mut acked = Vec::new();
    for w in waiters {
        acked.push(w.await.unwrap());
    }
    let rt = Arc::try_unwrap(router)
        .unwrap_or_else(|_| panic!("router sole owner"))
        .into_inner();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    let winner = page.entities[&PersistId::new(9)].components.clone();
    assert!(
        acked.iter().any(|p| p.as_slice() == winner.as_ref()),
        "the surviving value is one of the acked writers, got {winner:?}"
    );

    rt.close().await.unwrap();
}

#[test]
fn records_from_prior_epoch_survive_a_fence_bump() {
    // The C-2 regression (docs/11-roadmap.md §P2): 100 records acked at epoch
    // 0, then the shard is fenced to epoch 1 and the node restarts. The
    // naive predicate (`rec.epoch < config.epoch`) would discard all 100 —
    // the whole world, read as success. The running-maximum predicate keeps
    // them: a node's own journal has non-decreasing epochs, so only a
    // genuine zombie interleaving is dropped.
    let dir = tempfile::tempdir().unwrap();
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store())
                .await
                .unwrap();
            for i in 0..100u64 {
                let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
                rt.apply(rec).await.unwrap();
            }
            rt.close().await.unwrap();
        });
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Phase 2: restart at epoch 1 and fence the shard — this is what
        // startup fencing does once persistd-wiring is live. The 100 epoch-0
        // records were journaled before the fence, so they sit *below* the
        // new epoch; the running-maximum predicate must still replay them.
        let mut cfg = runtime_config(dir.path(), true);
        cfg.epoch = Epoch::new(1);
        let mut rt = CellRuntime::open(&cfg, &mem_store()).await.unwrap();
        let assumed = rt
            .fence_shard(CellId::ROOT, None, mem_store().as_ref())
            .await
            .unwrap();
        assert_eq!(assumed, Epoch::new(1), "the shard is fenced to epoch 1");

        let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert_eq!(
            page.entities.len(),
            100,
            "all 100 pre-fence records survive the epoch-1 restart (C-2)"
        );
        for i in 0..100u64 {
            let e = &page.entities[&PersistId::new(i)];
            assert_eq!(e.components.as_ref(), &i.to_le_bytes());
        }
        rt.close().await.unwrap();
    });
}

#[test]
fn zombie_writes_from_a_superseded_epoch_are_dropped() {
    // The other half of C-2: an epoch-0 record arriving AFTER an epoch-1
    // record at a lower LSN (a genuine zombie interleaving) is the only thing
    // replay drops.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let journal = std::sync::Arc::new(
            orrery_persistd::Journal::open(&runtime_config(dir.path(), true).journal).unwrap(),
        );
        // Epoch 1 lands first (lower LSN)…
        let mut rec_new = mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"new-epoch");
        rec_new.epoch = Epoch::new(1);
        journal.append(rec_new).unwrap().committed().await.unwrap();
        // …then a zombie epoch-0 write arrives (higher LSN, older epoch).
        let rec_old = mk_record(CellId::ROOT, 2, RecordKind::Spawn, b"zombie");
        journal.append(rec_old).unwrap().committed().await.unwrap();
        journal.close().await.unwrap();
        // The scan borrows the journal's keyspace; drop every handle before
        // reopening the same dir in `CellRuntime::open`.
        drop(journal);

        let mut cfg = runtime_config(dir.path(), true);
        cfg.epoch = Epoch::new(1);
        let rt = CellRuntime::open(&cfg, &mem_store()).await.unwrap();
        let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert!(
            page.entities.contains_key(&PersistId::new(1)),
            "the epoch-1 record replays"
        );
        assert!(
            !page.entities.contains_key(&PersistId::new(2)),
            "the zombie epoch-0 record is dropped (C-2)"
        );
        rt.close().await.unwrap();
    });
}

#[tokio::test]
async fn apply_stamps_the_actor_epoch_into_the_journal() {
    // The actor is the epoch authority (D11 §2.1: the server assigns epoch):
    // the gateway's placeholder `Epoch::new(0)` is overwritten with the
    // actor's ownership epoch before the append, so the journaled bytes carry
    // the real epoch.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = runtime_config(dir.path(), true);
    cfg.epoch = Epoch::new(7);
    let rt = CellRuntime::open(&cfg, &mem_store()).await.unwrap();
    rt.apply(mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"x"))
        .await
        .unwrap();

    let stored: Vec<_> = rt
        .journal()
        .scan_from(Lsn::new(0, 0))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].record.epoch,
        Epoch::new(7),
        "the stored record carries the actor's epoch, not the placeholder"
    );
    rt.close().await.unwrap();
}

#[tokio::test]
async fn two_runtimes_on_two_grids_do_not_serve_each_others_cells() {
    // P-7 at the router layer: the same raw cell id under two grids names two
    // different entity universes. Each runtime's `actor()` rejects the other
    // grid, so a read routed by (grid, cell) returns only its own grid's
    // entities.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut cfg_a = runtime_config(dir_a.path(), true);
    cfg_a.grid = GridId::new(9501);
    let mut cfg_b = runtime_config(dir_b.path(), true);
    cfg_b.grid = GridId::new(9502);
    let rt_a = CellRuntime::open(&cfg_a, &mem_store()).await.unwrap();
    let rt_b = CellRuntime::open(&cfg_b, &mem_store()).await.unwrap();

    let cell = CellId::ROOT;
    let mut rec_a = mk_record(cell, 1, RecordKind::Spawn, b"grid-9501");
    rec_a.grid = GridId::new(9501);
    rt_a.apply(rec_a).await.unwrap();
    let mut rec_b = mk_record(cell, 2, RecordKind::Spawn, b"grid-9502");
    rec_b.grid = GridId::new(9502);
    rt_b.apply(rec_b).await.unwrap();

    // Each runtime serves only its own grid's view of the same raw cell.
    let page_a = rt_a.read(GridId::new(9501), cell).await.unwrap();
    assert_eq!(page_a.entities.len(), 1);
    assert_eq!(
        page_a.entities[&PersistId::new(1)].components.as_ref(),
        b"grid-9501"
    );
    let page_b = rt_b.read(GridId::new(9502), cell).await.unwrap();
    assert_eq!(page_b.entities.len(), 1);
    assert_eq!(
        page_b.entities[&PersistId::new(2)].components.as_ref(),
        b"grid-9502"
    );

    // And a cross-grid read is refused, not silently served.
    assert!(rt_a.read(GridId::new(9502), cell).await.is_err());
    assert!(rt_b.read(GridId::new(9501), cell).await.is_err());
    assert!(rt_a.actor(GridId::new(9502), cell).is_none());

    rt_a.close().await.unwrap();
    rt_b.close().await.unwrap();
}

#[test]
fn group_commit_batches_into_one_fsync() {
    // Deterministic batching proof: N concurrent appends resolve on a single
    // persist. We can't observe the fsync count without a test seam; instead we
    // assert committed() advances past all and that journal replay is whole.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = runtime_config(dir.path(), true);
        let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();

        let mut handles = Vec::new();
        for i in 0..50u64 {
            let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
            handles.push(journal.append(rec).unwrap());
        }
        for h in &handles {
            h.committed().await.unwrap();
        }
        // All durable and ordered.
        let last = handles.iter().map(|h| h.lsn()).max().unwrap();
        assert!(journal.committed() >= last);
        assert_eq!(journal.scan_from(Lsn::new(0, 0)).count(), 50);
    });
}

#[test]
fn adaptive_lone_append_commits() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = runtime_config(dir.path(), false);
        let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();
        let rec = mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"x");
        let h = journal.append(rec).unwrap();
        h.committed().await.unwrap();
        assert!(journal.committed() >= h.lsn());
    });
}

#[test]
fn reopen_preserves_lsn_monotonicity() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = runtime_config(dir.path(), true);
        let last_lsn;
        {
            let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();
            let h = journal
                .append(mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"a"))
                .unwrap();
            h.committed().await.unwrap();
            last_lsn = h.lsn();
            journal.close().await.unwrap();
        }
        let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();
        let h = journal
            .append(mk_record(CellId::ROOT, 2, RecordKind::Spawn, b"b"))
            .unwrap();
        h.committed().await.unwrap();
        assert!(h.lsn() > last_lsn, "LSN continues after reopen");
    });
}

#[test]
fn corruption_is_detected_on_scan() {
    // Simulated: reopen and scan; crc is verified in runtime.recover. This test
    // checks the crc primitive agrees with what recovery expects.
    assert_eq!(payload_crc(b""), 0);
    assert_eq!(payload_crc(b"123456789"), 0xE306_9283);
}

/// One synthetic journal, two recovery paths, one answer.
///
/// `CellRuntime::restore`'s doc claimed its C-2 predicate was identical to
/// `open`'s and it was not: `open` advanced the running maximum epoch before
/// the watermark filter while `restore` skipped at the watermark first, and
/// `open` dispatched to the deepest matching shard while `restore` matched any
/// prefix. Under a nested shard set both differences bite at once — the parent
/// is a prefix of every cell in the child, and the pre-watermark records that
/// establish the epoch are exactly the ones `restore` cannot see.
///
/// The journal below is built by hand because the actor stamps its own epoch
/// onto anything routed through `apply`: two records at epoch 5 covered by the
/// checkpoint, then two zombies at epoch 3 in the tail, one pair per shard.
#[tokio::test]
async fn open_and_restore_fold_a_nested_journal_identically() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime_config(dir.path(), false);
    let nested = CellId::ROOT.children()[0];
    config.shards = vec![CellId::ROOT, nested];
    // Owned by the nested shard, and a prefix match for the root shard.
    let nested_cell = nested.children()[2];
    // Owned by the root shard: no deeper shard covers it.
    let root_cell = CellId::ROOT.children()[3];

    let at_epoch = |cell: CellId, entity: u64, epoch: u64| JournalRecord {
        epoch: Epoch::new(epoch),
        ..mk_record(cell, entity, RecordKind::Spawn, b"synthetic")
    };
    let journal = orrery_persistd::Journal::open(&config.journal).unwrap();
    let mut positions = Vec::new();
    for record in [
        at_epoch(nested_cell, 1, 5),
        at_epoch(root_cell, 2, 5),
        // Superseded-at-write-time zombies: below the epoch already seen at a
        // lower LSN in their own shard.
        at_epoch(nested_cell, 3, 3),
        at_epoch(root_cell, 4, 3),
    ] {
        let handle = journal.append(record).unwrap();
        handle.committed().await.unwrap();
        positions.push(handle.lsn());
    }
    journal.close().await.unwrap();
    // The fjall lock is released on drop, not on close.
    drop(journal);

    // A checkpoint per shard at epoch 5, covering the first two records.
    let checkpoints = Arc::new(MemCheckpointStore::new());
    let watermark = positions[1];
    for (shard, entity, cell) in [(nested, 1u64, nested_cell), (CellId::ROOT, 2, root_cell)] {
        checkpoints
            .checkpoint(&CheckpointData {
                shard,
                grid: GridId::ROOT,
                node_id: 0,
                epoch: Epoch::new(5),
                watermark,
                entities: std::collections::HashMap::from([(
                    PersistId::new(entity),
                    orrery_persistd::EntityRecord {
                        components: bytes::Bytes::from_static(b"synthetic"),
                        dirty: false,
                    },
                )]),
                by_cell: std::collections::HashMap::from([(PersistId::new(entity), cell)]),
                tombstones: std::collections::HashMap::new(),
                superseded: std::collections::HashSet::new(),
                taken_at_ms: 1_700_000_000_000,
            })
            .await
            .unwrap();
    }

    let rt = CellRuntime::open(&config, &(checkpoints.clone() as Arc<dyn CheckpointStore>))
        .await
        .unwrap();
    let opened: Vec<_> = vec![
        rt.read(GridId::ROOT, nested).await.unwrap().entities,
        rt.read(GridId::ROOT, CellId::ROOT).await.unwrap().entities,
    ];
    assert_eq!(
        opened[0].keys().copied().collect::<Vec<_>>(),
        vec![PersistId::new(1)],
        "the nested shard keeps its checkpointed entity and drops its zombie"
    );
    assert_eq!(
        opened[1].keys().copied().collect::<Vec<_>>(),
        vec![PersistId::new(2)],
        "the root shard serves only what it owns — the nested cell is not its \
         entity, prefix match or not"
    );

    // The same journal through the other path, onto the same actors.
    for shard in [nested, CellId::ROOT] {
        rt.restore(shard, checkpoints.as_ref()).await.unwrap();
    }
    assert_eq!(
        rt.read(GridId::ROOT, nested).await.unwrap().entities.len(),
        opened[0].len(),
        "restore folds the nested shard exactly as open did"
    );
    assert_eq!(
        rt.read(GridId::ROOT, CellId::ROOT)
            .await
            .unwrap()
            .entities
            .len(),
        opened[1].len(),
        "restore folds the root shard exactly as open did"
    );

    rt.close().await.unwrap();
}

/// A corrupt record for a shard this node does not host must not fail `open`.
///
/// The CRC check used to run before the record was dispatched, so one bad
/// payload anywhere in a shared journal bricked startup for every node that
/// read it — including the ones with no actor for that cell, which can neither
/// observe the damage nor repair it.
#[tokio::test]
async fn open_tolerates_a_corrupt_record_for_a_foreign_shard() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime_config(dir.path(), false);
    let hosted = CellId::ROOT.children()[0];
    let foreign = CellId::ROOT.children()[4];
    config.shards = vec![hosted];

    let journal = orrery_persistd::Journal::open(&config.journal).unwrap();
    let mut corrupt = mk_record(foreign, 9, RecordKind::Spawn, b"payload");
    corrupt.crc ^= 0xffff_ffff;
    for record in [mk_record(hosted, 8, RecordKind::Spawn, b"payload"), corrupt] {
        let handle = journal.append(record).unwrap();
        handle.committed().await.unwrap();
    }
    journal.close().await.unwrap();
    // The fjall lock is released on drop, not on close.
    drop(journal);

    let rt = CellRuntime::open(&config, &mem_store())
        .await
        .expect("a corrupt foreign-shard record is not this node's to fail on");
    let page = rt.read(GridId::ROOT, hosted).await.unwrap();
    assert_eq!(page.entities.len(), 1);
    rt.close().await.unwrap();
}

/// A record from another grid is not this runtime's to fold. The live write
/// path has always refused it (`CellRuntime::actor`'s grid guard) and so has
/// the rekey branch of recovery; the plain diff path did not, so recovery
/// admitted rows the running node would have rejected.
#[tokio::test]
async fn open_skips_records_from_another_grid() {
    let dir = tempfile::tempdir().unwrap();
    let config = runtime_config(dir.path(), false);

    let journal = orrery_persistd::Journal::open(&config.journal).unwrap();
    let foreign = JournalRecord {
        grid: GridId::new(77),
        ..mk_record(CellId::ROOT, 6, RecordKind::Spawn, b"other-grid")
    };
    for record in [
        mk_record(CellId::ROOT, 5, RecordKind::Spawn, b"this-grid"),
        foreign,
    ] {
        let handle = journal.append(record).unwrap();
        handle.committed().await.unwrap();
    }
    journal.close().await.unwrap();
    // The fjall lock is released on drop, not on close.
    drop(journal);

    let rt = CellRuntime::open(&config, &mem_store()).await.unwrap();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities.len(),
        1,
        "the same cell id under another grid is another universe (P-7)"
    );
    rt.close().await.unwrap();
}
