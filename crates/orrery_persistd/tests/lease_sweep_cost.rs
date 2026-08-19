//! The 1 Hz lease sweep does not pay for the world to answer "nothing expired".
//!
//! `sweep_leases` copies the actor's whole registrar so a durable write that
//! fails partway can be abandoned. That copy is load-bearing when something
//! expires and pure waste when nothing does — which is the steady state, since
//! holders renew every 3 s against a 10 s TTL and the gateway sweeps every
//! second. Both maps are copied whole, so the unguarded version cost the
//! *shard's* population once per actor per tick.
//!
//! And the sweep driver asked each actor in turn. One mailbox round trip per
//! hosted shard, serially, for work that shares nothing and takes no gate.
//!
//! Neither is expensive at the P2 operating point — together about 0.9 ms per
//! second across 128 shards. They are pinned because both grow with the
//! deployment while the 1 Hz tick that pays for them does not.
//!
//! What is testable here is stated exactly, because the first version of this
//! file was not: **that a restored copy is invisible from outside the actor**.
//! Skipping it changes no return value, no durable write and no observable
//! state, so no test can catch its return — a put-count assertion passes
//! either way, as a mutation check showed. What the guard can actually break
//! is the predicate: if `has_expired` ever under-reports, the sweep silently
//! stops parking rows it should park. So that equivalence is what is pinned,
//! exhaustively, and the copy's cost is left to `benches/lease_renewal.rs`.
//!
//! The concurrency half is pinned with a barrier rather than a stuck actor and
//! a witness, for the same reason: the witness version passed under a serial
//! driver whenever `self.actors` happened to iterate the free shard first.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, ClaimResult, JournalConfig, LeaseMigrate, LeasePut, LeaseStore, LeaseStoreError,
    MemLeaseStore, Router, RuntimeConfig, LEASE_TTL_MS,
};
use orrery_protocol::{CellId, ClaimKind, Epoch, GridId, Lease, LeaseId, NodeId, PersistId};

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn runtime_config(dir: &std::path::Path, shards: Vec<CellId>) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        },
        node_id: 1,
        epoch: Epoch::new(1),
        ..RuntimeConfig::default()
    }
}

/// Counts durable writes, and — once armed — holds every write at a barrier
/// until as many actors as the barrier expects have reached one.
///
/// A barrier rather than one parked actor plus a witness: the witness version
/// of this test passed under a serial driver whenever the runtime's actor map
/// happened to iterate the un-stuck shard first, which is not a property any
/// test should depend on. A barrier cannot be satisfied by a serial driver in
/// any order.
///
/// Armed after setup, never during: `claim_lease` writes durably too, so a
/// store that blocked from construction would block the fixture rather than
/// the sweep under test.
#[derive(Default)]
struct SweepStore {
    inner: MemLeaseStore,
    puts: AtomicUsize,
    barrier: Option<tokio::sync::Barrier>,
    armed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LeaseStore for SweepStore {
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
        self.puts.fetch_add(1, Ordering::SeqCst);
        if let Some(barrier) = &self.barrier {
            if self.armed.load(Ordering::SeqCst) {
                barrier.wait().await;
            }
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

/// A quiet sweep writes nothing, and says so for every tick, not just the first.
#[tokio::test]
async fn a_sweep_with_nothing_expired_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let shards: Vec<CellId> = CellId::ROOT.children().into_iter().take(4).collect();
    let store = Arc::new(SweepStore::default());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), shards.clone()),
        &checkpoints,
        Arc::clone(&store) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();
    let holder = test_node(41);

    for (index, shard) in shards.iter().enumerate() {
        for n in 0..8u64 {
            let cell = shard.children()[n as usize];
            let entity = PersistId::new(30_000 + (index as u64) * 100 + n);
            let ClaimResult::Granted(_) =
                Router::claim_lease(&rt, GridId::ROOT, cell, entity, holder, ClaimKind::Weak, 0)
                    .await
                    .unwrap()
            else {
                panic!("claim should be granted");
            };
        }
    }

    // Every lease is live until `LEASE_TTL_MS`. Sweep repeatedly inside that
    // window: each tick must park nothing and write nothing.
    let before = store.puts.load(Ordering::SeqCst);
    for tick in 0..16u64 {
        let parked = rt.sweep_expired_leases(tick * 100).await;
        assert!(parked.is_empty(), "tick {tick} parked a live lease");
    }
    assert_eq!(
        store.puts.load(Ordering::SeqCst) - before,
        0,
        "a sweep that parks nothing must not write",
    );

    // And the sweep still works: past the TTL every held row parks, once.
    let parked = rt.sweep_expired_leases(LEASE_TTL_MS + 1).await;
    assert_eq!(
        parked.len(),
        32,
        "every held lease parks when its TTL passes"
    );
    assert_eq!(
        store.puts.load(Ordering::SeqCst) - before,
        32,
        "one durable write per parked row",
    );
    let again = rt.sweep_expired_leases(LEASE_TTL_MS + 2).await;
    assert!(again.is_empty(), "a parked row is not parked twice");

    rt.close().await.unwrap();
}

/// Every actor's sweep runs at once, in any actor-map order.
///
/// The store refuses to answer a durable write until `SHARDS` of them are in
/// flight together. A serial driver has one write outstanding at a time and
/// can never release the barrier, whatever order it visits actors in, so the
/// timeout is the assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_actors_sweep_runs_at_once() {
    const SHARDS: usize = 4;
    let dir = tempfile::tempdir().unwrap();
    let shards: Vec<CellId> = CellId::ROOT.children().into_iter().take(SHARDS).collect();
    let store = Arc::new(SweepStore {
        inner: MemLeaseStore::new(),
        puts: AtomicUsize::new(0),
        barrier: Some(tokio::sync::Barrier::new(SHARDS)),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &runtime_config(dir.path(), shards.clone()),
        &checkpoints,
        Arc::clone(&store) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();
    let holder = test_node(42);

    // Exactly one expiring lease per shard, so the barrier's count is the
    // number of actors and not the number of rows.
    for (index, shard) in shards.iter().enumerate() {
        let entity = PersistId::new(31_000 + index as u64);
        let ClaimResult::Granted(_) = Router::claim_lease(
            &rt,
            GridId::ROOT,
            shard.children()[0],
            entity,
            holder,
            ClaimKind::Weak,
            0,
        )
        .await
        .unwrap() else {
            panic!("claim should be granted");
        };
    }

    store.armed.store(true, Ordering::SeqCst);
    let parked = tokio::time::timeout(
        Duration::from_secs(10),
        rt.sweep_expired_leases(LEASE_TTL_MS + 1),
    )
    .await
    .expect(
        "the sweep must ask its actors concurrently; a serial driver has one durable write in \
         flight at a time and never releases the barrier",
    );
    store.armed.store(false, Ordering::SeqCst);
    assert_eq!(parked.len(), SHARDS, "every shard's row parks");

    rt.close().await.unwrap();
}

/// `has_expired` is true exactly when `sweep_expired` would park something.
///
/// This is the guard's whole risk surface. Skipping the registrar copy is
/// invisible from outside the actor — it changes no return value and no
/// durable write — so nothing can test that it was skipped. What a wrong
/// predicate does is visible and bad: a sweep that silently stops parking
/// rows whose TTL passed, which is a lease nobody can ever reclaim.
#[test]
fn has_expired_agrees_with_the_sweep_it_guards() {
    use orrery_persistd::LeaseRegistrar;
    use orrery_protocol::ClaimKind as CK;

    // A matrix over the states a registrar row can be in when a sweep looks
    // at it, crossed with clocks either side of each one's deadline.
    let holder = test_node(43);
    let other = test_node(44);
    let mut checked = 0;
    let mut saw_true = false;
    let mut saw_false = false;
    for held in [0u64, 1, 3] {
        for kind in [CK::Weak, CK::Strong] {
            for parked_already in [false, true] {
                for now in [
                    0u64,
                    1,
                    LEASE_TTL_MS - 1,
                    LEASE_TTL_MS,
                    LEASE_TTL_MS + 1,
                    u64::MAX / 2,
                ] {
                    let mut registrar = LeaseRegistrar::default();
                    for n in 0..held {
                        let entity = PersistId::new(90_000 + n);
                        registrar.claim(entity, holder, kind, 0);
                    }
                    if parked_already {
                        registrar.disconnect(holder, 0);
                        // And a second holder whose rows are still live, so a
                        // parked row and a held row can coexist.
                        registrar.claim(PersistId::new(95_000), other, kind, 0);
                    }
                    let predicted = registrar.has_expired(now);
                    let mut swept = registrar.clone();
                    let actually = !swept.sweep_expired(now).is_empty();
                    assert_eq!(
                        predicted, actually,
                        "has_expired disagreed with sweep_expired: held={held} kind={kind:?} \
                         parked={parked_already} now={now}",
                    );
                    checked += 1;
                    saw_true |= actually;
                    saw_false |= !actually;
                }
            }
        }
    }
    assert!(checked >= 72, "the matrix shrank: {checked} cases");
    assert!(
        saw_true && saw_false,
        "the matrix must contain states that sweep and states that do not",
    );
}
