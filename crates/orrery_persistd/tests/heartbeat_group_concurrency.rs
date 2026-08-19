//! Phase 2 of a heartbeat batch dispatches its actor groups concurrently.
//!
//! Phase 1's locates were made concurrent when the batched path stopped
//! reading FoundationDB serially (`heartbeat_locate_concurrency.rs`). Phase 2
//! was left as a `for` loop, and it is the more expensive half: a group is one
//! mailbox round trip, the fold only collapses leases that share an *actor*,
//! and at the P2 operating point a session's 40 leases sit in 40 different
//! shards. So the batch was 40 mailbox round trips end to end, each queued
//! behind a different actor's journal work, for renewals that share nothing.
//! `benches/lease_renewal.rs` measures that shape: 0.295 ms per batch serial,
//! 0.064 ms concurrent. See docs/08-persistence.md §2.2.3.
//!
//! Pinning it needs no timing threshold. One group is held on an entity gate
//! that something else owns; a second group, on a different actor, must still
//! land while the first is stuck. A serial loop cannot ever satisfy that — it
//! has not started the second group — so the timeout is the assertion, and it
//! is a timeout on something that otherwise takes microseconds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::LeaseRenewal;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, ClaimResult, JournalConfig, LeaseMigrate, LeasePut, LeaseStore, LeaseStoreError,
    MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{CellId, ClaimKind, Epoch, GridId, Lease, LeaseId, NodeId, PersistId};
use tokio::sync::Notify;

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

/// A registrar tier that parks the *first* `locate` of one chosen entity.
///
/// One-shot, so it holds the single-entity renewal that is used to occupy an
/// entity gate and then gets out of the way of the batch under test.
struct ParkFirstLocateStore {
    inner: MemLeaseStore,
    park: PersistId,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl LeaseStore for ParkFirstLocateStore {
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
        let found = self.inner.locate(grid, entity).await?;
        if entity == self.park && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(found)
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

const CLAIM_MS: u64 = 0;
const RENEW_MS: u64 = 5;

#[tokio::test]
async fn a_stuck_actor_group_does_not_hold_up_the_rest_of_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let (first, second) = (cells[0], cells[1]);
    let stuck = PersistId::new(4_201);
    let free = PersistId::new(4_202);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let store = Arc::new(ParkFirstLocateStore {
        inner: MemLeaseStore::new(),
        park: stuck,
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        armed: AtomicBool::new(false),
    });
    let rt = Arc::new(
        CellRuntime::open_with_lease_store(
            &runtime_config(dir.path(), vec![first, second]),
            &(Arc::new(MemCheckpointStore::new()) as Arc<dyn CheckpointStore>),
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .unwrap(),
    );
    let holder = test_node(23);

    // One lease per shard, so the batch is two groups on two different
    // actors — the P2 shape in miniature.
    let mut batch = Vec::new();
    for (cell, entity) in [(first, stuck), (second, free)] {
        let ClaimResult::Granted(row) = Router::claim_lease(
            rt.as_ref(),
            GridId::ROOT,
            cell,
            entity,
            holder,
            ClaimKind::Weak,
            CLAIM_MS,
        )
        .await
        .unwrap() else {
            panic!("claim of {entity:?} should be granted");
        };
        batch.push(LeaseRenewal {
            cell,
            entity,
            lease_id: row.lease_id,
        });
    }
    let claimed_until = rt
        .inspect_lease(GridId::ROOT, free)
        .await
        .unwrap()
        .0
        .expect("the free entity holds a row")
        .expires_at;

    // Occupy the first group's entity gate with something that is not the
    // batch: a single-entity renewal takes the gate and *then* locates, and
    // this store parks that locate. It is one-shot, so the batch's own phase
    // 1 runs unobstructed.
    store.armed.store(true, Ordering::SeqCst);
    let stuck_lease_id = batch[0].lease_id;
    let gate_rt = Arc::clone(&rt);
    let occupier = tokio::spawn(async move {
        Router::heartbeat_lease(
            gate_rt.as_ref(),
            GridId::ROOT,
            first,
            stuck,
            holder,
            stuck_lease_id,
            RENEW_MS,
        )
        .await
    });
    entered.notified().await;

    let renewals = batch.clone();
    let batch_rt = Arc::clone(&rt);
    let batched = tokio::spawn(async move {
        Router::heartbeat_leases(batch_rt.as_ref(), GridId::ROOT, holder, &renewals, RENEW_MS).await
    });

    // The batch cannot *finish* — its first group is behind a gate nobody has
    // released. What it must still do is renew the second group, on the other
    // actor, without waiting for the first.
    let observer = Arc::clone(&rt);
    tokio::time::timeout(Duration::from_secs(10), async move {
        loop {
            let row = observer
                .inspect_lease(GridId::ROOT, free)
                .await
                .unwrap()
                .0
                .expect("the free entity holds a row");
            if row.expires_at != claimed_until {
                return row.expires_at;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect(
        "a heartbeat batch must dispatch its actor groups concurrently; a serial loop never \
         reaches the second group while the first is held",
    );

    release.notify_one();
    let rows = batched.await.unwrap().unwrap();
    assert_eq!(rows.len(), batch.len());
    for (entry, row) in batch.iter().zip(&rows) {
        let row = row.as_ref().expect("every held pair renews");
        assert_eq!(row.entity, entry.entity, "the reply stays positional");
        assert_eq!(row.lease_id, entry.lease_id);
        assert_eq!(row.expires_at, RENEW_MS + orrery_persistd::LEASE_TTL_MS);
    }
    occupier.await.unwrap().unwrap();

    Arc::try_unwrap(rt).ok().unwrap().close().await.unwrap();
}
