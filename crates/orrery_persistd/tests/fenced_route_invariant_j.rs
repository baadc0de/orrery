//! Invariant J, asserted directly after every event that can install or move
//! a registrar row.
//!
//! > **(J)** if an actor's registrar holds a row for entity `e`, then
//! > `LeaseStore::locate(e)` names a cell inside that actor's shard subtree
//! > (or is `None`).
//!
//! J is the whole safety argument for routing a fenced diff by `record.cell`:
//! an actor that *accepts* holds a row, so by J it is the actor the locate
//! would have named, so the accept set is unchanged. Everything else about a
//! wrong route is loud — a `Rejected` the client sees as a `BulkNack`. A
//! violation of J is the one way the change could admit a write the old code
//! rejected, silently. So it gets a test that walks every actor rather than a
//! comment.
//!
//! The walk runs after grant, park, sweep, cross-shard rekey, intra-shard
//! rekey, `split`, `activate_shards`, and actor recovery from `load_cell`,
//! under `MemLeaseStore` on default features and `FdbLeaseStore` with
//! `--features fdb`.

mod support;

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::fence::{MemFenceStore, ShardActivation};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, JournalConfig, LeaseStore, MemLeaseStore, Router,
    RuntimeConfig, LEASE_TTL_MS,
};
use orrery_protocol::{
    CellId, ClaimKind, EntityRekey, Epoch, GridId, JournalRecord, Lsn, NodeId, PersistId,
    RecordKind, Tick, ENTITY_REKEY_VERSION,
};

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(grid: GridId, cell: CellId, entity: PersistId, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid,
        entity,
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::Spawn,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
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

fn config(
    dir: &std::path::Path,
    grid: GridId,
    shards: Vec<CellId>,
    fence: Arc<MemFenceStore>,
) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                batch_window: Duration::from_millis(1),
                ..GroupCommitConfig::default()
            },
        },
        node_id: 3,
        epoch: Epoch::new(0),
        fence,
    }
}

/// Walk every actor and check J for every registrar row it holds.
async fn assert_invariant_j(
    rt: &CellRuntime,
    store: &dyn LeaseStore,
    grid: GridId,
    entities: &[PersistId],
    after: &str,
) {
    let mut rows_seen = 0usize;
    for handle in rt.actor_handles() {
        for &entity in entities {
            let (row, mirror, _) = handle
                .inspect_lease(entity)
                .await
                .expect("an actor must answer its own registrar");
            let Some(_) = row else {
                continue;
            };
            rows_seen += 1;
            // The actor's own mirror of the durable location key. This is the
            // observable form of the four `debug_assert_row_in_shard` call
            // sites: if the mirror names a cell outside the shard, a row was
            // installed somewhere it could never be routed to.
            if let Some(mirror) = mirror {
                assert!(
                    handle.shard().is_prefix_of(mirror),
                    "after {after}: actor {:?} mirrors entity {entity:?} at {mirror:?}, \
                     outside its own shard",
                    handle.shard()
                );
            }
            let located = store
                .locate(grid, entity)
                .await
                .expect("the lease store must answer a locate");
            // `None` is J's own escape hatch: with no location key, the route
            // falls back to `record.cell`, which names this actor anyway.
            let Some(located) = located else {
                continue;
            };
            assert!(
                handle.shard().is_prefix_of(located),
                "after {after}: actor {:?} holds a row for entity {entity:?} whose durable \
                 location is {located:?} — invariant J is false, and the fenced route's \
                 accept-set equivalence with it",
                handle.shard()
            );
        }
    }
    assert!(
        rows_seen > 0,
        "after {after}: no actor held any row, so the walk checked nothing"
    );
}

async fn run(grid: GridId, store: Arc<dyn LeaseStore>) {
    let roots = CellId::ROOT.children();
    let (shard_a, shard_b) = (roots[0], roots[1]);
    let a_home = shard_a.children()[0];
    let a_sibling = shard_a.children()[1];
    let b_home = shard_b.children()[0];
    let dir = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let fence = Arc::new(MemFenceStore::new());
    let cfg = config(dir.path(), grid, vec![shard_a, shard_b], Arc::clone(&fence));
    let mut rt = CellRuntime::open_with_lease_store(&cfg, &checkpoints, Arc::clone(&store))
        .await
        .unwrap();

    // grant
    let granted = PersistId::new(4_101);
    let parked = PersistId::new(4_102);
    let swept = PersistId::new(4_103);
    let moved_across = PersistId::new(4_104);
    let moved_within = PersistId::new(4_105);
    let entities = [granted, parked, swept, moved_across, moved_within];
    // One holder per entity. `LeaseRegistrar::disconnect` parks *every* row a
    // holder holds, so a shared holder would make the park step below park the
    // whole fixture and leave the rekeys nothing live to move.
    let holder_of = |entity: PersistId| test_node(41 + (entity.0 - 4_101) as u8);
    for (entity, cell) in [
        (granted, a_home),
        (parked, a_home),
        (swept, b_home),
        (moved_across, a_home),
        (moved_within, a_home),
    ] {
        rt.apply(mk_record(grid, cell, entity, b"seed"))
            .await
            .unwrap();
        let ClaimResult::Granted(_) = Router::claim_lease(
            &rt,
            grid,
            cell,
            entity,
            holder_of(entity),
            ClaimKind::Strong,
            1_000,
        )
        .await
        .unwrap() else {
            panic!("entity {entity:?} must be granted a lease");
        };
    }
    assert_invariant_j(&rt, store.as_ref(), grid, &entities, "grant").await;

    // park
    Router::park_lease(
        &rt,
        grid,
        a_home,
        parked,
        holder_of(parked),
        live_lease_id(&rt, parked).await,
    )
    .await
    .unwrap();
    assert_invariant_j(&rt, store.as_ref(), grid, &entities, "park").await;

    // cross-shard rekey, then intra-shard rekey
    for (entity, destination, label) in [
        (moved_across, b_home, "cross-shard rekey"),
        (moved_within, a_sibling, "intra-shard rekey"),
    ] {
        let lease_id = live_lease_id(&rt, entity).await;
        Router::commit_rekey(
            &rt,
            rekey_record(&EntityRekey {
                source_schema_floor: 0,
                version: ENTITY_REKEY_VERSION,
                entity,
                source_grid: grid,
                source_cell: a_home,
                destination_grid: grid,
                destination_cell: destination,
                expected_lease_id: lease_id,
                source_record: bytes::Bytes::from_static(b"seed"),
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("{label} must commit: {e:?}"));
        assert_invariant_j(&rt, store.as_ref(), grid, &entities, label).await;
    }

    // sweep (TTL expiry, no entity gate held by the sweeper)
    rt.sweep_expired_leases(1_000 + LEASE_TTL_MS + 1).await;
    assert_invariant_j(&rt, store.as_ref(), grid, &entities, "sweep").await;

    // split: shard A's actor becomes eight child actors, each re-seeded from
    // `load_cell` over its own subtree.
    rt.fence_shard(shard_a, None, checkpoints.as_ref())
        .await
        .unwrap();
    let parent_row = rt.fence().read(grid, shard_a).await.unwrap().unwrap();
    rt.split(shard_a, &parent_row).await.unwrap();
    assert_invariant_j(&rt, store.as_ref(), grid, &entities, "split").await;

    // activate_shards: shard B re-activated at a fresh epoch, which respawns
    // its actor and reloads its rows.
    let b_row = rt.fence().read(grid, shard_b).await.unwrap();
    rt.activate_shards(
        &[ShardActivation {
            shard: shard_b,
            expected: b_row,
        }],
        checkpoints.as_ref(),
    )
    .await
    .unwrap();
    assert_invariant_j(&rt, store.as_ref(), grid, &entities, "activate_shards").await;

    // actor recovery from `load_cell`: close and reopen over the same store.
    rt.close().await.unwrap();
    let reopened = CellRuntime::open_with_lease_store(&cfg, &checkpoints, Arc::clone(&store))
        .await
        .unwrap();
    assert_invariant_j(
        &reopened,
        store.as_ref(),
        grid,
        &entities,
        "actor recovery from load_cell",
    )
    .await;
    reopened.close().await.unwrap();
}

/// The live `lease_id` an actor holds for `entity`, from whichever actor has
/// the row.
async fn live_lease_id(rt: &CellRuntime, entity: PersistId) -> orrery_protocol::LeaseId {
    for handle in rt.actor_handles() {
        if let Ok((Some(row), _, _)) = handle.inspect_lease(entity).await {
            return row.lease_id;
        }
    }
    panic!("no actor holds a row for {entity:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invariant_j_holds_after_every_row_install() {
    run(GridId::ROOT, Arc::new(MemLeaseStore::new())).await;
}

#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fdb_invariant_j_holds_after_every_row_install() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let context = orrery_persistd::FdbContext::connect(&cluster)
        .expect("configured FDB cluster file must open");
    let grid = {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        GridId::new(
            elapsed.subsec_nanos()
                ^ std::process::id().rotate_left(16)
                ^ NEXT.fetch_add(1, Ordering::Relaxed),
        )
    };
    run(
        grid,
        Arc::new(orrery_persistd::FdbLeaseStore::from_context(&context)),
    )
    .await;
}
