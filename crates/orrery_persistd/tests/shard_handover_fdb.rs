//! What a live shard handover does to the **durable** tier (issue #119, D26
//! rule 3).
//!
//! `tests/shard_handover.rs` proves the sequence against in-memory stores,
//! which is the right place for the fence-transition logic and the wrong place
//! for this question: the whole point of a sibling handover is that the row
//! and its cell index are one durable object two processes take turns owning,
//! and an in-process `HashMap` shared by two `CellRuntime`s cannot tell you
//! whether the FoundationDB keyspace survived the move.
//!
//! **The property, stated exactly.** docs/04-authority.md §9 says a lease is
//! entity-keyed and that `lease_id` and `seq` are preserved when an entity
//! moves — but §9 is about a *rekey*, an entity crossing a cell boundary while
//! its holder keeps writing. A shard handover is not that. D26 rule 3 step 3
//! divests: every held row **parks**, and `park` bumps `lease_id` by one by
//! construction (`lease.rs`), because the token the old holder has installed
//! must stop working. So the honest durable property, and the one asserted
//! here, is:
//!
//! * the row survives, at the same entity, at the same cell, with its
//!   `(own_seq, auth_seq)` **unchanged** — that is what the successor adopts
//!   and what D26 rule 3 step 3 means by "parks with `own_seq` intact";
//! * its `lease_id` advances **exactly once**, by the park, and no more;
//! * a row that was already parked before the drain is byte-for-byte
//!   identical afterwards, `lease_id` included;
//! * the cell index (`LeaseStore::locate`) still resolves, and resolves to the
//!   same cell, so the successor can route a re-claim.
//!
//! Nothing tested this before, in either form.

mod support;

#[cfg(feature = "fdb")]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(feature = "fdb")]
use std::sync::Arc;
#[cfg(feature = "fdb")]
use std::time::Duration;

#[cfg(feature = "fdb")]
use orrery_persistd::checkpoint::CheckpointStore;
#[cfg(feature = "fdb")]
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
#[cfg(feature = "fdb")]
use orrery_persistd::{
    CellRuntime, CheckpointCause, ClaimResult, DivestOutcome, FdbContext, FdbLeaseStore, FenceRow,
    FenceStatus, FenceStore, JournalConfig, LeaseStore, Router, RuntimeConfig,
};
#[cfg(feature = "fdb")]
use orrery_protocol::{CellId, ClaimKind, Epoch, GridId, LeaseFlags, PersistId};

#[cfg(feature = "fdb")]
const NODE_A: u64 = 4_119_001;
#[cfg(feature = "fdb")]
const NODE_B: u64 = 4_119_002;

#[cfg(feature = "fdb")]
fn node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

/// A grid nothing else in this process or this cluster is using.
///
/// The same device `tests/lease_fdb.rs` uses, and for the same C-8 reason: the
/// suites write into whatever cluster they are pointed at, so every test owns
/// a keyspace no other run can collide with.
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
fn runtime_config(
    dir: &std::path::Path,
    grid: GridId,
    node_id: u64,
    shards: Vec<CellId>,
    fence: Arc<dyn FenceStore>,
) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        },
        node_id,
        epoch: Epoch::new(1),
        fence,
    }
}

/// The durable half of D26 rule 3, over one real FoundationDB cluster and two
/// separate `CellRuntime`s that share nothing but it.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn durable_lease_rows_and_their_cell_index_survive_a_live_shard_handover() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let leases: Arc<dyn LeaseStore> = Arc::new(FdbLeaseStore::from_context(&context));
    let fence: Arc<dyn FenceStore> = Arc::new(orrery_persistd::fence::FdbFenceStore::from_context(
        &context,
    ));
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(orrery_persistd::checkpoint::FdbCheckpointStore::from_context(&context));
    let grid = unique_grid_id();
    // A read first, so an unreachable cluster fails here rather than halfway
    // through a handover.
    leases
        .load_cell(grid, CellId::ROOT)
        .await
        .expect("configured FDB cluster must be reachable");

    let shard = CellId::ROOT.children()[0];
    let other = CellId::ROOT.children()[1];
    let cell = shard.children()[0];
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Two owners, two disjoint shards, one durable fence — the sibling
    // topology D26 rule 1 describes, with the row as the only ownership rule.
    for (owner, shard) in [(NODE_A, shard), (NODE_B, other)] {
        fence
            .fence(
                grid,
                shard,
                None,
                &FenceRow {
                    owner,
                    epoch: Epoch::new(1),
                    status: FenceStatus::Active,
                },
            )
            .await
            .expect("bootstrap fence");
    }

    let a = CellRuntime::open_with_lease_store(
        &runtime_config(dir_a.path(), grid, NODE_A, vec![shard], Arc::clone(&fence)),
        &checkpoints,
        Arc::clone(&leases),
    )
    .await
    .expect("open A");
    let b = CellRuntime::open_with_lease_store(
        &runtime_config(dir_b.path(), grid, NODE_B, vec![other], Arc::clone(&fence)),
        &checkpoints,
        Arc::clone(&leases),
    )
    .await
    .expect("open B");

    // Given: three live rows on A's shard, and one that is already parked.
    let live: Vec<PersistId> = (1..=3).map(|n| PersistId::new(41_190_000 + n)).collect();
    let already_parked = PersistId::new(41_190_009);
    for (index, entity) in live.iter().enumerate() {
        let ClaimResult::Granted(_) = a
            .claim_lease(
                grid,
                cell,
                *entity,
                node(u8::try_from(index + 1).unwrap()),
                ClaimKind::Weak,
                0,
            )
            .await
            .expect("claim routed")
        else {
            panic!("claim must be granted");
        };
    }
    let ClaimResult::Granted(parked_row) = a
        .claim_lease(grid, cell, already_parked, node(9), ClaimKind::Weak, 0)
        .await
        .expect("claim routed")
    else {
        panic!("claim must be granted");
    };
    a.park_lease(grid, cell, already_parked, node(9), parked_row.lease_id)
        .await
        .expect("park the control row");

    // The durable state as the cluster holds it, before anything moves.
    let mut before = Vec::new();
    for entity in live.iter().copied().chain([already_parked]) {
        let located = leases
            .locate(grid, entity)
            .await
            .expect("locate")
            .expect("the row is indexed before the move");
        assert_eq!(located, cell);
        let row = leases
            .load_cell(grid, shard)
            .await
            .expect("load")
            .into_iter()
            .find(|(_, row)| row.entity == entity)
            .expect("the durable row exists before the move");
        before.push((entity, row.1));
    }

    // When: the shard is handed to B, through the whole of rule 3.
    a.begin_handover(shard, NODE_B, None)
        .await
        .expect("step 1: mark");
    let DivestOutcome::Divested(divested) = a
        .divest_shard(grid, shard, 0)
        .await
        .expect("step 3: drain routed")
    else {
        panic!("no rekey is in flight");
    };
    assert_eq!(divested.len(), 3, "the three live rows are divested");
    a.quiesce_handover(shard);
    a.checkpoint_shard_because(shard, checkpoints.as_ref(), CheckpointCause::PreHandover)
        .await
        .expect("step 5: pre-handover checkpoint");
    let handed = a.complete_handover(shard).await.expect("step 6: CAS");
    assert_eq!(handed.owner, NODE_B);
    assert_eq!(handed.epoch, Epoch::new(2));
    assert_eq!(handed.status, FenceStatus::Active);
    b.adopt_shard(shard, checkpoints.as_ref())
        .await
        .expect("step 7: B opens the shard");

    // Then: every durable row is still there, at the same cell, with its
    // sequences untouched — read back through the *successor*, which is the
    // only reader that matters after the move.
    for (entity, row_before) in &before {
        let located = leases
            .locate(grid, *entity)
            .await
            .expect("locate after the move")
            .expect("the cell index survived the move");
        assert_eq!(
            located, cell,
            "the entity is committed to the same cell; a handover moves the \
             shard's owner, never the entity's location"
        );

        let (row_cell, row) = leases
            .load_cell(grid, shard)
            .await
            .expect("load after the move")
            .into_iter()
            .find(|(_, row)| row.entity == *entity)
            .expect("the durable row survived the move");
        assert_eq!(row_cell, cell);
        assert_eq!(
            row.seq, row_before.seq,
            "(own_seq, auth_seq) is unchanged across the move — this is what \
             D26 rule 3 step 3 means by parking with own_seq intact, and it is \
             the ordering the successor's next grant continues from"
        );
        assert!(
            row.holder.is_none() && row.flags.contains(LeaseFlags::PARKED),
            "no row is left held: that is the precondition that makes the \
             successor's `with_fresh_recovery_ttl` correct"
        );
        if *entity == already_parked {
            assert_eq!(
                row.lease_id, row_before.lease_id,
                "a row that was already parked before the drain is untouched, \
                 token included"
            );
            assert_eq!(row, *row_before, "…byte for byte");
        } else {
            assert_eq!(
                row.lease_id.0,
                row_before.lease_id.0 + 1,
                "a divested row's token advances exactly once, by its own park \
                 — the token the old holder installed must stop working, and \
                 nothing else may touch it"
            );
        }

        // The successor sees the same row through its own actor, which is the
        // path a re-claim takes.
        let (restored, restored_cell, _) = b
            .inspect_lease(grid, *entity)
            .await
            .expect("the successor can inspect the adopted row");
        let restored = restored.expect("the successor restored the row");
        assert_eq!(restored_cell, Some(cell));
        assert_eq!(restored.seq, row_before.seq);
        assert!(restored.holder.is_none());
        assert_eq!(
            restored.expires_at, 0,
            "a parked weak row carries no TTL on the successor's clock either"
        );
    }

    // And the shard is genuinely B's now: A refuses it, B serves it.
    assert!(a.actor(grid, cell).is_none());
    assert!(b.actor(grid, cell).is_some());
    let row = fence
        .read(grid, shard)
        .await
        .expect("read")
        .expect("the actor/ row is never retired by a handover");
    assert_eq!((row.owner, row.epoch), (NODE_B, Epoch::new(2)));

    tokio::time::sleep(Duration::from_millis(2)).await;
    a.close().await.expect("close A");
    b.close().await.expect("close B");
}
