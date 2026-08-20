//! Live shard handover between two sibling gateways (D26 rule 3, issue #119,
//! docs/08-persistence.md §3.4.1).
//!
//! Two `CellRuntime`s in one process, sharing **one** fence store, **one**
//! lease store and **one** checkpoint store — which is the whole topology a
//! handover is about, because the durable row is the only channel between the
//! two nodes. What each test drives is the ADR's own numbered sequence:
//!
//! ```text
//! 1 mark      CAS (A, e, Active) -> (A, e, Draining{B})
//! 2 close     claims under S refused                     [gateway; see tests/gateway.rs]
//! 3 divest    every live row parked, Expire on the holder's own session on A
//! 4 bound     one handoff_deadline_ms for the whole shard, not one per row
//! 5 quiesce   PreHandover checkpoint, stop accepting diffs
//! 6 hand over CAS (A, e, Draining{B}) -> (B, e+1, Active)
//! 7 open      B loads the checkpoint; every restored row is parked
//! 8 redirect  WrongOwner{grid, shard, owner: B}
//! ```
//!
//! The journal is deliberately **not** shared: a sibling is not a chain
//! follower (D26's last consequence), so B has no copy of A's journal and the
//! `PreHandover` checkpoint of step 5 is the only base it gets. A test that
//! pointed both runtimes at one journal directory would prove a topology
//! nobody deploys — and would not even open, since the journal takes a file
//! lock.

mod support;

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::fence::{FenceRow, FenceStatus, MemFenceStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    overlapping_active_ownership, CellRuntime, ClaimResult, DivestOutcome, FenceStore,
    JournalConfig, LeaseStore, MemLeaseStore, Router, RuntimeConfig, TransferPhase,
};
use orrery_protocol::{CellId, ClaimKind, Epoch, GridId, LeaseFlags, PersistId};

const NODE_A: u64 = 1;
const NODE_B: u64 = 2;

fn peer(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

/// The two shards A and B start out owning: siblings at level 1, disjoint, so
/// the deployment satisfies D26's "`--shard` sets across siblings must not
/// overlap" before anything moves.
fn shards() -> (CellId, CellId) {
    let children = CellId::ROOT.children();
    (children[0], children[1])
}

fn config(
    dir: &std::path::Path,
    node_id: u64,
    shards: Vec<CellId>,
    fence: Arc<MemFenceStore>,
) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(10),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id,
        epoch: Epoch::new(0),
        fence,
    }
}

/// A and B, both activated against one durable fence, over disjoint shards.
struct Siblings {
    a: CellRuntime,
    b: CellRuntime,
    fence: Arc<MemFenceStore>,
    leases: Arc<dyn LeaseStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    shard_a: CellId,
    shard_b: CellId,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Siblings {
    async fn open() -> Self {
        let (shard_a, shard_b) = shards();
        let fence = Arc::new(MemFenceStore::new());
        let leases: Arc<dyn LeaseStore> = Arc::new(MemLeaseStore::new());
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
        let dir_a = tempfile::tempdir().expect("temp dir a");
        let dir_b = tempfile::tempdir().expect("temp dir b");

        // Both nodes bootstrap their own row: the fence CAS is what makes each
        // one the owner, not the flag list (D26 rule 1).
        for (node, shard) in [(NODE_A, shard_a), (NODE_B, shard_b)] {
            fence
                .fence(
                    GridId::ROOT,
                    shard,
                    None,
                    &FenceRow {
                        owner: node,
                        epoch: Epoch::new(1),
                        status: FenceStatus::Active,
                    },
                )
                .await
                .expect("bootstrap fence");
        }

        let mut config_a = config(dir_a.path(), NODE_A, vec![shard_a], Arc::clone(&fence));
        config_a.epoch = Epoch::new(1);
        let mut config_b = config(dir_b.path(), NODE_B, vec![shard_b], Arc::clone(&fence));
        config_b.epoch = Epoch::new(1);
        let a = CellRuntime::open_with_lease_store(&config_a, &checkpoints, Arc::clone(&leases))
            .await
            .expect("open A");
        let b = CellRuntime::open_with_lease_store(&config_b, &checkpoints, Arc::clone(&leases))
            .await
            .expect("open B");
        Self {
            a,
            b,
            fence,
            leases,
            checkpoints,
            shard_a,
            shard_b,
            _dirs: (dir_a, dir_b),
        }
    }

    /// Every `actor/` row in the grid, in the shape D26's I1 is stated over.
    async fn rows(&self) -> Vec<(GridId, CellId, FenceRow)> {
        let mut rows = Vec::new();
        for shard in [self.shard_a, self.shard_b] {
            if let Some(row) = self
                .fence
                .read(GridId::ROOT, shard)
                .await
                .expect("fence read")
            {
                rows.push((GridId::ROOT, shard, row));
            }
        }
        rows
    }

    /// I1: no two `Active` rows in one grid cover overlapping subtrees.
    async fn assert_i1(&self, at: &str) {
        let rows = self.rows().await;
        assert!(
            overlapping_active_ownership(&rows).is_none(),
            "I1 violated {at}: two nodes hold Active rows over overlapping subtrees: {rows:?}"
        );
        // The prefix test is only interesting if two nodes really do hold rows
        // over the same subtree at some point, so also state the stronger
        // version the harness reads: no two *distinct owners* are Active over
        // the same shard.
        let mut active: Vec<(CellId, u64)> = rows
            .iter()
            .filter(|(_, _, row)| row.status == FenceStatus::Active)
            .map(|(_, shard, row)| (*shard, row.owner))
            .collect();
        active.sort_by_key(|(shard, _)| shard.to_bits());
        for pair in active.windows(2) {
            assert!(
                pair[0].0 != pair[1].0,
                "I1 violated {at}: shard {:?} is Active for both {} and {}",
                pair[0].0,
                pair[0].1,
                pair[1].1
            );
        }
    }

    async fn claim(&self, runtime: &CellRuntime, cell: CellId, entity: u64, holder: u8) {
        let outcome = runtime
            .claim_lease(
                GridId::ROOT,
                cell,
                PersistId::new(entity),
                peer(holder),
                ClaimKind::Weak,
                0,
            )
            .await
            .expect("claim routed");
        assert!(
            matches!(outcome, ClaimResult::Granted(_)),
            "claim must be granted to set the test up: {outcome:?}"
        );
    }
}

/// The whole of D26 rule 3, with I1 sampled between every step.
///
/// This is the test the issue asks for: *no window in which two nodes hold
/// `Active` rows for overlapping shard subtrees*. It is checkable without a
/// global pause because every transition is a CAS on one row and `Draining` is
/// not `Active` — so sampling the rows between the steps is sampling every
/// state the cluster is ever in.
#[tokio::test]
async fn no_window_exists_in_which_two_nodes_serve_overlapping_subtrees() {
    let world = Siblings::open().await;
    let shard = world.shard_a;
    world.assert_i1("at rest").await;

    // Step 1: mark.
    let marked = world
        .a
        .begin_handover(shard, NODE_B, Some(peer(9)))
        .await
        .expect("mark draining");
    assert_eq!(marked.owner, NODE_A, "the mark moves status, never owner");
    assert_eq!(marked.epoch, Epoch::new(1), "the mark does not bump epoch");
    assert_eq!(marked.status, FenceStatus::Draining { successor: NODE_B });
    world.assert_i1("after the mark").await;

    // A is still the owner and still serving: `Draining` closes admission,
    // not the write path.
    assert!(
        world.a.actor(GridId::ROOT, shard).is_some(),
        "the drain is invisible to the write path (D26 rule 3 step 2)"
    );
    assert_eq!(
        world
            .a
            .shard_transfer(GridId::ROOT, shard)
            .map(|transfer| transfer.phase),
        Some(TransferPhase::Draining)
    );

    // B must not be able to take the row while it is draining: only the
    // owner's own CAS may move it, and only from `Draining{B}`.
    assert!(
        world.b.adopt_shard(shard, world.checkpoints.as_ref()).await.is_err(),
        "a draining row is not adoptable; (B, e+1, Active) is reachable only from (A, e, Draining{{B}})"
    );
    world.assert_i1("while B tried to adopt").await;

    // Steps 3–5.
    let DivestOutcome::Divested(_) = world
        .a
        .divest_shard(GridId::ROOT, shard, 0)
        .await
        .expect("drain routed")
    else {
        panic!("no rekey is in flight");
    };
    world.a.quiesce_handover(shard);
    assert!(
        world.a.actor(GridId::ROOT, shard).is_none(),
        "a quiesced shard accepts no more diffs (step 5)"
    );
    world.assert_i1("after the quiesce").await;

    // Step 6.
    let handed = world
        .a
        .complete_handover(shard)
        .await
        .expect("handover CAS");
    assert_eq!(handed.owner, NODE_B);
    assert_eq!(handed.epoch, Epoch::new(2), "only the epoch and owner move");
    assert_eq!(handed.status, FenceStatus::Active);
    world.assert_i1("after the CAS, before B opens").await;

    // Step 7. Between the CAS and this line the shard is `Active` for B and
    // hosted by nobody, which is a liveness gap and not an I1 violation: I1 is
    // about two nodes serving, and A is not one of them any more.
    assert!(
        world.a.actor(GridId::ROOT, shard).is_none(),
        "A stopped serving the shard at the CAS"
    );
    world
        .b
        .adopt_shard(shard, world.checkpoints.as_ref())
        .await
        .expect("B adopts the row that names it");
    assert!(world.b.actor(GridId::ROOT, shard).is_some());
    world.assert_i1("after B opened").await;

    // Step 8: A now answers a request under the shard as a redirect naming B.
    let transfer = world
        .a
        .shard_transfer(GridId::ROOT, shard)
        .expect("A remembers where the shard went");
    assert_eq!(transfer.phase, TransferPhase::HandedOver);
    assert_eq!(transfer.successor, NODE_B);
    assert_eq!(
        transfer.successor_gateway,
        Some(peer(9)),
        "the redirect names the successor's gateway when the caller supplied one"
    );
    assert_eq!(transfer.epoch, Epoch::new(2));

    world.a.close().await.expect("close A");
    world.b.close().await.expect("close B");
}

/// I2, at the registrar: every row live when the drain started is parked, and
/// parked in the way the successor's recovery needs.
///
/// The `Expire` *delivery* half of I2 is a gateway property and is asserted
/// over a real session in `tests/gateway.rs`; what this pins is the registrar
/// side, which is what makes delivery possible at all — a drain that left a
/// row held would have nothing to deliver about.
#[tokio::test]
async fn the_drain_parks_every_live_row_and_keeps_its_sequences() {
    let world = Siblings::open().await;
    let shard = world.shard_a;
    let cell = shard.children()[0];

    for entity in 1..=4u64 {
        world.claim(&world.a, cell, entity, entity as u8).await;
    }
    // A fifth row that is already parked before the drain: it must come
    // through completely untouched, token included.
    world.claim(&world.a, cell, 5, 5).await;
    world
        .a
        .park_lease(
            GridId::ROOT,
            cell,
            PersistId::new(5),
            peer(5),
            orrery_protocol::LeaseId(1),
        )
        .await
        .expect("park the fifth row");

    let mut before = Vec::new();
    for entity in 1..=5u64 {
        let (row, _, _) = world
            .a
            .inspect_lease(GridId::ROOT, PersistId::new(entity))
            .await
            .expect("inspect");
        before.push(row.expect("row exists"));
    }
    assert_eq!(
        before[..4]
            .iter()
            .filter(|row| row.holder.is_some())
            .count(),
        4,
        "four rows are live when the drain begins"
    );

    world
        .a
        .begin_handover(shard, NODE_B, None)
        .await
        .expect("mark");
    let DivestOutcome::Divested(parked) = world
        .a
        .divest_shard(GridId::ROOT, shard, 1_000)
        .await
        .expect("drain routed")
    else {
        panic!("no rekey is in flight");
    };

    assert_eq!(
        parked.len(),
        4,
        "I2's left-hand term: every row that was live is divested, and the \
         already-parked one is not divested twice"
    );
    for row in &parked {
        assert!(
            row.previous_lease_id.0 > 0,
            "the Expire is addressed by the token the holder still believes it has"
        );
        assert_eq!(
            row.reason,
            orrery_protocol::ExpireReason::Parked,
            "a handover park is not a Timeout: the holder was alive and renewing, \
             and this is the counter that tells the two apart"
        );
    }

    // Every row survives the move: the sequences are what the successor
    // adopts, and `park` moves holder/lease_id/flags/expires_at and nothing
    // else.
    for (index, entity) in (1..=5u64).enumerate() {
        let (row, cell_after, _) = world
            .a
            .inspect_lease(GridId::ROOT, PersistId::new(entity))
            .await
            .expect("inspect");
        let row = row.expect("row survived the drain");
        assert_eq!(
            row.seq, before[index].seq,
            "own_seq/auth_seq are preserved across the drain (D26 rule 3 step 3)"
        );
        assert_eq!(cell_after, Some(cell), "the cell index is unchanged");
        assert!(row.holder.is_none(), "no row is left held");
        assert!(row.flags.contains(LeaseFlags::PARKED));
        if entity == 5 {
            assert_eq!(
                row.lease_id, before[index].lease_id,
                "a row that was already parked is untouched, token included"
            );
        } else {
            assert_eq!(
                row.lease_id.0,
                before[index].lease_id.0 + 1,
                "a divested row's token is bumped exactly once, by its park"
            );
        }
    }

    world.a.quiesce_handover(shard);
    world.a.complete_handover(shard).await.expect("CAS");
    world.a.close().await.expect("close A");
    world.b.close().await.expect("close B");
}

/// Step 7's precondition, stated as the property it exists for: after a drain,
/// the successor restores **no held row**, so `with_fresh_recovery_ttl` never
/// re-arms a lease whose holder cannot reach it.
///
/// This is the defect D26's Context describes, tested from the successor's
/// side. Without the drain, B would restore four rows with a full fresh 10 s
/// TTL naming holders whose sessions are on A.
#[tokio::test]
async fn the_successor_restores_no_held_row() {
    let world = Siblings::open().await;
    let shard = world.shard_a;
    let cell = shard.children()[0];
    for entity in 1..=4u64 {
        world.claim(&world.a, cell, entity, entity as u8).await;
    }

    world
        .a
        .begin_handover(shard, NODE_B, None)
        .await
        .expect("mark");
    let DivestOutcome::Divested(_) = world
        .a
        .divest_shard(GridId::ROOT, shard, 0)
        .await
        .expect("drain")
    else {
        panic!("no rekey is in flight");
    };
    world.a.quiesce_handover(shard);
    world
        .a
        .checkpoint_shard_because(
            shard,
            world.checkpoints.as_ref(),
            orrery_persistd::CheckpointCause::PreHandover,
        )
        .await
        .expect("pre-handover checkpoint");
    world.a.complete_handover(shard).await.expect("CAS");

    world
        .b
        .adopt_shard(shard, world.checkpoints.as_ref())
        .await
        .expect("adopt");

    for entity in 1..=4u64 {
        let (row, _, _) = world
            .b
            .inspect_lease(GridId::ROOT, PersistId::new(entity))
            .await
            .expect("inspect on the successor");
        let row = row.expect("the successor restored the row");
        assert!(
            row.holder.is_none(),
            "every restored row is parked; there is no held row to re-arm \
             (D26 rule 3 step 7)"
        );
        assert_eq!(
            row.expires_at, 0,
            "a parked weak row carries no TTL, so nothing counts down toward a \
             park with zero connected peers"
        );
    }

    // And the successor can hand it straight back: the row is a row like any
    // other, and only its owner and epoch moved.
    let row = world
        .fence
        .read(GridId::ROOT, shard)
        .await
        .expect("read")
        .expect("the row is never retired");
    assert_eq!(row.owner, NODE_B);
    assert_eq!(row.epoch, Epoch::new(2));

    world.a.close().await.expect("close A");
    world.b.close().await.expect("close B");
}

/// A drain that cannot finish must not hand the shard over.
///
/// The abort leaves the row exactly where it was — `Active`, naming A, at the
/// same epoch — so a failed handover is a no-op on ownership rather than a
/// half-move. The rows the drain already parked stay parked, which is safe
/// because their holders were told.
#[tokio::test]
async fn an_aborted_handover_leaves_ownership_exactly_where_it_was() {
    let world = Siblings::open().await;
    let shard = world.shard_a;

    let before = world
        .fence
        .read(GridId::ROOT, shard)
        .await
        .expect("read")
        .expect("row");
    world
        .a
        .begin_handover(shard, NODE_B, None)
        .await
        .expect("mark");
    let restored = world.a.abort_handover(shard).await.expect("abort");
    assert_eq!(restored, before, "an abort restores the row byte for byte");
    assert!(
        world.a.shard_transfer(GridId::ROOT, shard).is_none(),
        "and forgets the handover, so admission reopens"
    );
    assert!(
        world.a.actor(GridId::ROOT, shard).is_some(),
        "A is serving the shard again"
    );
    world.assert_i1("after the abort").await;

    // A committed handover is not abortable: the row belongs to B now, and a
    // CAS back would be A claiming a shard it does not own.
    world
        .a
        .begin_handover(shard, NODE_B, None)
        .await
        .expect("mark again");
    world.a.quiesce_handover(shard);
    world.a.complete_handover(shard).await.expect("CAS");
    assert!(
        world.a.abort_handover(shard).await.is_err(),
        "a committed handover cannot be rolled back by its previous owner"
    );

    world.a.close().await.expect("close A");
    world.b.close().await.expect("close B");
}

/// The three CASes are compare-and-set, and every precondition is checked.
#[tokio::test]
async fn a_handover_refuses_every_precondition_it_does_not_hold() {
    let world = Siblings::open().await;
    let (shard_a, shard_b) = (world.shard_a, world.shard_b);

    assert!(
        world.a.begin_handover(shard_a, NODE_A, None).await.is_err(),
        "a shard cannot be handed to its current owner"
    );
    assert!(
        world.a.begin_handover(shard_b, NODE_B, None).await.is_err(),
        "a node cannot hand away a shard it does not host"
    );
    assert!(
        world.a.complete_handover(shard_a).await.is_err(),
        "the handover CAS has no meaning without a preceding mark"
    );
    assert!(
        world
            .b
            .adopt_shard(shard_a, world.checkpoints.as_ref())
            .await
            .is_err(),
        "a shard whose row names someone else is not adoptable"
    );
    assert!(
        world
            .b
            .adopt_shard(shard_b, world.checkpoints.as_ref())
            .await
            .is_err(),
        "a shard already hosted here is not adoptable twice"
    );

    // Two marks in a row: the second sees `Draining`, not `Active`.
    world
        .a
        .begin_handover(shard_a, NODE_B, None)
        .await
        .expect("mark");
    assert!(
        world.a.begin_handover(shard_a, NODE_B, None).await.is_err(),
        "a second mark cannot re-enter a drain that is already running"
    );

    world.assert_i1("after the refusals").await;
    let _ = world.leases.locate(GridId::ROOT, PersistId::new(1)).await;
    world.a.close().await.expect("close A");
    world.b.close().await.expect("close B");
}
