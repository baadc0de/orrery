//! A heartbeat batch resolves its locations concurrently.
//!
//! The locate phase holds no gate while it reads, which was the fix that
//! `heartbeat_gate_hold.rs` pins. It still issued those reads one after
//! another: a peer renewing 77 leases made 77 serial FoundationDB round trips
//! — ~38 ms at the P2 operating point — for reads that share nothing and are
//! each validated by their own stripe mark afterwards.
//!
//! That phase is now the **fallback**: the route asks the actor owning each
//! entry's presented cell first and locates only for entries it has no row
//! for (docs/08-persistence.md §2.2.4). So this batch is built to miss —
//! every lease is claimed in one shard and renewed against a cell in the
//! other — because a batch that hits would take no locate at all and this
//! file would pass without testing anything.
//!
//! It pins the concurrency, because it is the kind of thing a later edit
//! reverts by accident: the store's `locate` refuses to answer until every
//! entry in the batch has entered it. A serial locate phase can never satisfy
//! that, so it hangs, and the timeout is the assertion.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::LeaseRenewal;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, JournalConfig, LeaseMigrate, LeasePut, LeaseStore,
    LeaseStoreError, MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId, PersistId,
    RecordKind, Tick,
};

const BATCH: usize = 8;

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(cell: CellId, entity: PersistId) -> JournalRecord {
    let payload = b"seed";
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid: GridId::ROOT,
        entity,
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::Spawn,
        payload: bytes::Bytes::from_static(payload),
        crc: payload_crc(payload),
    }
}

/// A registrar tier whose `locate` waits at a barrier for the whole batch.
///
/// Armed only around the heartbeat itself: claims locate too, and they are
/// genuinely sequential.
struct BarrierLocateStore {
    inner: MemLeaseStore,
    armed: AtomicBool,
    barrier: tokio::sync::Barrier,
}

#[async_trait::async_trait]
impl LeaseStore for BarrierLocateStore {
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
        if self.armed.load(Ordering::Acquire) {
            self.barrier.wait().await;
        }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_batch_resolves_its_locations_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    // Two shards: the leases live in `home`, the renewals present `away`. The
    // presented cell's actor holds no row for any of them, so every entry
    // takes the fallback and every entry locates.
    let roots = CellId::ROOT.children();
    let (home, away) = (roots[0], roots[1]);
    let holder = test_node(52);
    let store = Arc::new(BarrierLocateStore {
        inner: MemLeaseStore::new(),
        armed: AtomicBool::new(false),
        barrier: tokio::sync::Barrier::new(BATCH),
    });
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let config = RuntimeConfig {
        shards: vec![home, away],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.path().to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                batch_window: Duration::from_millis(1),
                ..GroupCommitConfig::default()
            },
        },
        node_id: 1,
        epoch: Epoch::new(1),
        ..RuntimeConfig::default()
    };
    let rt = CellRuntime::open_with_lease_store(
        &config,
        &checkpoints,
        Arc::clone(&store) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();

    let mut renew = Vec::with_capacity(BATCH);
    for index in 0..BATCH {
        let entity = PersistId::new(6_200 + index as u64);
        rt.apply(mk_record(home, entity)).await.unwrap();
        let ClaimResult::Granted(grant) = Router::claim_lease(
            &rt,
            GridId::ROOT,
            home,
            entity,
            holder,
            ClaimKind::Weak,
            1_000,
        )
        .await
        .unwrap() else {
            panic!("entity {entity:?} must be granted a lease");
        };
        renew.push(LeaseRenewal {
            entity,
            cell: away,
            lease_id: grant.lease_id,
        });
    }

    store.armed.store(true, Ordering::Release);
    let renewed = tokio::time::timeout(
        Duration::from_secs(10),
        Router::heartbeat_leases(&rt, GridId::ROOT, holder, &renew, 1_100),
    )
    .await
    .expect(
        "the locate phase must have all its reads in flight at once; a serial loop never \
         releases the barrier",
    )
    .unwrap();
    store.armed.store(false, Ordering::Release);

    assert_eq!(renewed.len(), BATCH);
    for (index, row) in renewed.iter().enumerate() {
        let row = row
            .as_ref()
            .unwrap_or_else(|| panic!("entry {index} renewed"));
        assert_eq!(row.entity, renew[index].entity);
        assert_eq!(row.lease_id, renew[index].lease_id);
        assert_eq!(row.holder, Some(holder));
    }

    rt.close().await.unwrap();
}
