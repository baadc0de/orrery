//! The fenced route's fallback `LeaseStore::locate` is bounded in flight.
//!
//! The fallback is the expensive branch — an FDB read plus a second mailbox
//! turn — and `libfdb_c` serves every read in a process on **one** network
//! thread, which docs/14-capacity.md §5.1 measured as the binding constraint
//! on a whole box. Nothing bounded how many of those reads could be in flight
//! at once, so a peer that could steer diffs onto the branch could steer the
//! whole box's throughput with them.
//!
//! The gateway now meters the diff shapes that steered it — both a wrong
//! cell and an entity the session holds no lease for
//! (`AuthoritySnapshot::misrouted_diffs` / `unindexed_diffs`) — but "no
//! vector we know of" is not a bound, so this is the bound: a permit pool,
//! and this file is what says the pool is real. Note what it bounds:
//! **concurrency, not rate.** The rate bound is the gateway's per-connection
//! bucket, and this pool is what still holds when a vector slips past it.
//!
//! Its own test binary because the permit count resolves once per process
//! from the environment.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, JournalConfig, LeaseMigrate, LeasePut, LeaseStore, LeaseStoreError,
    MemFenceStore, MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId, PersistId, RecordKind, Tick,
};

const PERMITS: usize = 2;
const CONCURRENT: usize = 8;
const LOCATE_DELAY: Duration = Duration::from_millis(20);

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(cell: CellId, entity: PersistId, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid: GridId::ROOT,
        entity,
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::ComponentDiff,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

/// A lease store that reports the deepest concurrency its `locate` ever saw.
#[derive(Default)]
struct WatchingLeaseStore {
    inner: MemLeaseStore,
    inflight: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait::async_trait]
impl LeaseStore for WatchingLeaseStore {
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
        let now = self.inflight.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(now, Ordering::AcqRel);
        tokio::time::sleep(LOCATE_DELAY).await;
        let answer = self.inner.locate(grid, entity).await;
        self.inflight.fetch_sub(1, Ordering::AcqRel);
        answer
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_fallback_locate_never_exceeds_its_permit_pool() {
    std::env::set_var("ORRERY_FENCED_LOCATE_FALLBACK_PERMITS", PERMITS.to_string());
    // The audit is the other reader of the lease store on this path, and it
    // deliberately runs *outside* the pool. Off, so the peak below is the
    // fallback's alone.
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "0");

    let shard = CellId::ROOT.children()[0];
    let hosted = shard.children()[0];
    // No actor hosts this cell, which is one of the three documented fallback
    // triggers and the one that needs no fixture to reach.
    let unhosted = CellId::ROOT.children()[1].children()[0];
    let holder = test_node(51);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(WatchingLeaseStore::default());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let config = RuntimeConfig {
        shards: vec![shard],
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
        fence: Arc::new(MemFenceStore::new()),
    };
    let rt = Arc::new(
        CellRuntime::open_with_lease_store(
            &config,
            &checkpoints,
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .unwrap(),
    );
    assert!(rt.actor(GridId::ROOT, hosted).is_some());
    assert!(rt.actor(GridId::ROOT, unhosted).is_none());

    let mut running = Vec::new();
    for id in 0..CONCURRENT {
        let rt = Arc::clone(&rt);
        running.push(tokio::spawn(async move {
            // Distinct entities, so these do not queue on one entity gate and
            // the only thing that can serialize them is the permit pool.
            Router::apply_fenced(
                &*rt,
                mk_record(unhosted, PersistId::new(9_500 + id as u64), b"diff"),
                holder,
                LeaseId(1),
                Default::default(),
                1_001,
            )
            .await
        }));
    }
    for task in running {
        // Every one of these falls back, locates, finds no actor for the
        // unhosted cell and reports it. The answer is not the point; the
        // concurrency of the read that produced it is.
        let _ = task.await.unwrap();
    }

    let peak = store.peak.load(Ordering::Acquire);
    assert!(
        peak >= 2,
        "the fixture must actually contend: peak in-flight was {peak}"
    );
    assert!(
        peak <= PERMITS,
        "the fallback locate must not exceed its permit pool: peak {peak} > {PERMITS}"
    );

    Arc::try_unwrap(rt).ok().unwrap().close().await.unwrap();
}
