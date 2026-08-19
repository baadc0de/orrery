//! A renewal that finds its row reads no lease store, and the fallback is
//! bounded.
//!
//! The batched renewal route asks the actor that owns each entry's presented
//! cell and consults `LeaseStore::locate` only for entries that actor has no
//! row for. Under `--fdb-cluster-file` a locate is a FoundationDB read on the
//! single `libfdb_c` network thread that docs/14-capacity.md §5.1 measured as
//! the whole capacity of one box, and the renewal path issues ~3 333 of them a
//! second at the P2 operating point. So "the steady-state count is zero" is
//! the claim, and this is where it is asserted rather than described.
//!
//! Its own test binary: `RouteStageMetrics` is process-global, so a file that
//! also drove fenced applies would read this file's deltas wrong.

use std::sync::atomic::{AtomicUsize, Ordering};
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

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

#[derive(Default)]
struct CountingLeaseStore {
    inner: MemLeaseStore,
    locates: AtomicUsize,
}

impl CountingLeaseStore {
    fn locates(&self) -> usize {
        self.locates.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LeaseStore for CountingLeaseStore {
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
        self.locates.fetch_add(1, Ordering::SeqCst);
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

const BATCH: u64 = 64;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_batch_reads_no_lease_store_and_bounds_its_fallback() {
    // The sampled J audit is a *deliberate* store read on the accept path, at
    // one in a thousand in release and one in one under `debug_assertions`.
    // It is not the route, so it is turned off here: the claim under test is
    // "routing reads nothing", and mixing the audit into the count would
    // measure the audit instead.
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "0");

    let roots = CellId::ROOT.children();
    let (shard_a, shard_b) = (roots[0], roots[1]);
    let holder = test_node(31);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingLeaseStore::default());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let rt = CellRuntime::open_with_lease_store(
        &RuntimeConfig {
            shards: vec![shard_a, shard_b],
            grid: GridId::ROOT,
            journal: JournalConfig {
                dir: dir.path().to_path_buf(),
                commit: GroupCommitConfig {
                    mode: AdaptiveCommitMode::Adaptive,
                    batch_window: Duration::from_millis(1),
                    ..GroupCommitConfig::default()
                },
            },
            node_id: 0,
            epoch: Epoch::new(0),
            ..RuntimeConfig::default()
        },
        &checkpoints,
        Arc::clone(&store) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();

    // A batch spread over both shards and many leaf cells — the P2 shape, in
    // which the fold is one group per actor and nothing folds by cell.
    let mut batch = Vec::new();
    for i in 0..BATCH {
        let shard = if i % 2 == 0 { shard_a } else { shard_b };
        let cell = shard.children()[(i / 2 % 8) as usize].children()[(i / 16 % 8) as usize];
        let entity = PersistId::new(50_000 + i);
        let ClaimResult::Granted(row) =
            Router::claim_lease(&rt, GridId::ROOT, cell, entity, holder, ClaimKind::Weak, 0)
                .await
                .unwrap()
        else {
            panic!("claim {i} should be granted");
        };
        batch.push(LeaseRenewal {
            cell,
            entity,
            lease_id: row.lease_id,
        });
    }

    // -- zero store reads on the path that renews ------------------------
    let before = store.locates();
    let rows = Router::heartbeat_leases(&rt, GridId::ROOT, holder, &batch, 5)
        .await
        .unwrap();
    assert!(
        rows.iter().all(Option::is_some),
        "every held pair must renew",
    );
    assert_eq!(
        store.locates() - before,
        0,
        "a renewal batch whose rows are where the holder presented them must not read the lease \
         store",
    );

    // Repeated renewals stay at zero: nothing here is a one-shot cache warm.
    let before = store.locates();
    for round in 0..8 {
        let rows = Router::heartbeat_leases(&rt, GridId::ROOT, holder, &batch, 6 + round)
            .await
            .unwrap();
        assert!(rows.iter().all(Option::is_some));
    }
    assert_eq!(store.locates() - before, 0, "and it stays at zero");

    // -- the fallback is bounded, and only the misses pay for it ---------
    // One entry the owner has no row for, in a batch of rows it does. It must
    // cost exactly one locate, and its neighbours must still cost none.
    let mut mixed = batch.clone();
    mixed.push(LeaseRenewal {
        cell: shard_a.children()[7].children()[7],
        entity: PersistId::new(60_001),
        lease_id: LeaseId(1),
    });
    let before = store.locates();
    let rows = Router::heartbeat_leases(&rt, GridId::ROOT, holder, &mixed, 20)
        .await
        .unwrap();
    assert_eq!(rows.len(), mixed.len(), "the reply stays positional");
    assert!(
        rows[..batch.len()].iter().all(Option::is_some),
        "a miss must not disturb the entries around it",
    );
    assert_eq!(rows[batch.len()], None, "and the miss is answered `None`");
    assert_eq!(
        store.locates() - before,
        1,
        "one unrouted entry is one locate — the fallback is per miss, not per batch",
    );

    rt.close().await.unwrap();
}
