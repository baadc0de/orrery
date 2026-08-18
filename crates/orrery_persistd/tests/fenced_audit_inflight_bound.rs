//! Detaching the invariant-J audit bounds it, and a sample the bound refuses
//! is still a number.
//!
//! "Just spawn it" is a fix with a well-known failure mode: nothing upstream
//! throttles a detached task, so a burst of accepts becomes a burst of
//! concurrent `LeaseStore::locate` reads and the diagnostic starts costing
//! more than the thing it is diagnosing. `fenced_location_audit_inflight` is
//! the bound. Two things have to be true of it, and the second is what stops
//! the bound from re-introducing the defect it was added beside:
//!
//! 1. **The bound never reaches the write.** Refusing a sample must not
//!    refuse, delay or nack the accept it was sampled from — the audit is
//!    still a diagnostic when it declines to run.
//! 2. **A refused sample is counted.** `location_audits_dropped` exists so
//!    that `decided == audits + errors + dropped` stays an identity. Without
//!    it a saturated audit pool would silently shrink the sample and
//!    `location_audits` would read as "the audit is running fine" while it
//!    was mostly not running at all — the same class of invisibility as the
//!    shed this whole change is about.
//!
//! The bound is pinned to **one** here so that concurrent accepts contend for
//! it deterministically. Its own test binary for two reasons:
//! `ORRERY_FENCED_LOCATION_AUDIT_INFLIGHT` resolves once per process, and
//! `RouteStageMetrics` is process-global.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::{
    fenced_location_audit_inflight, route_stage_metrics, settle_location_audits,
};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, FencedApply, JournalConfig, LeaseMigrate, LeasePut,
    LeaseStore, LeaseStoreError, MemFenceStore, MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId, PersistId,
    RecordKind, Tick,
};

/// Long enough that the single permit is still held when the next accept
/// arrives, without the test depending on how fast this box is.
const SLOW_LOCATE: Duration = Duration::from_millis(50);
const ACCEPTS: u64 = 12;

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(cell: CellId, entity: PersistId, tick: u64, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid: GridId::ROOT,
        entity,
        tick: Tick::new(tick),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::ComponentDiff,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

struct SlowLocateStore {
    inner: MemLeaseStore,
    slow: AtomicBool,
    concurrent: AtomicUsize,
    peak: AtomicUsize,
}

impl SlowLocateStore {
    fn new() -> Self {
        Self {
            inner: MemLeaseStore::new(),
            slow: AtomicBool::new(false),
            concurrent: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LeaseStore for SlowLocateStore {
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
        if self.slow.load(Ordering::SeqCst) {
            let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(SLOW_LOCATE).await;
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
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
async fn a_refused_audit_sample_costs_a_counter_not_a_write() {
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "1");
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_INFLIGHT", "1");
    assert_eq!(
        fenced_location_audit_inflight(),
        1,
        "the bound under test must be the one this binary set"
    );

    let cell = CellId::ROOT.children()[0];
    let holder = test_node(41);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SlowLocateStore::new());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let config = RuntimeConfig {
        shards: vec![CellId::ROOT],
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

    let entity = PersistId::new(7_711);
    rt.apply(mk_record(cell, entity, 1, b"seed")).await.unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &*rt,
        GridId::ROOT,
        cell,
        entity,
        holder,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the entity must be granted a lease");
    };

    // Only now is `locate` expensive: the setup above uses it too.
    store.slow.store(true, Ordering::SeqCst);
    let before = route_stage_metrics().snapshot();

    let mut running = Vec::new();
    for tick in 0..ACCEPTS {
        let rt = Arc::clone(&rt);
        running.push(tokio::spawn(async move {
            Router::apply_fenced(
                &*rt,
                mk_record(cell, entity, 100 + tick, b"diff"),
                holder,
                grant.lease_id,
                grant.seq,
                1_001,
            )
            .await
        }));
    }
    // Claim 1: every accept is served. A permit the audit could not get is
    // not the write's problem.
    for task in running {
        let applied = task.await.unwrap().unwrap();
        let FencedApply::Accepted(handle) = applied else {
            panic!("a live fence at its own cell must be admitted");
        };
        handle.committed().await.unwrap();
    }

    assert!(
        settle_location_audits(Duration::from_secs(30)).await,
        "the decided audits never landed in a counter: {:?}",
        route_stage_metrics().snapshot().delta(before)
    );
    let delta = route_stage_metrics().snapshot().delta(before);

    assert_eq!(delta.applies, ACCEPTS, "{delta:?}");
    assert_eq!(
        delta.location_audits_decided, ACCEPTS,
        "at one-in-one the sampler decides on every accept: {delta:?}"
    );
    // Claim 2: the identity is closed, which is the only thing that makes
    // `location_audits` readable as a sample rate at all.
    assert_eq!(
        delta.location_audits + delta.location_audit_errors + delta.location_audits_dropped,
        delta.location_audits_decided,
        "decided == audits + errors + dropped, always: {delta:?}"
    );
    assert!(
        delta.location_audits_dropped > 0,
        "a one-permit pool against {ACCEPTS} concurrent 50 ms audits must refuse \
         some of them; if it refused none the bound is not being applied: {delta:?}"
    );
    assert!(
        delta.location_audits > 0,
        "and it must still take some, or the diagnostic has stopped: {delta:?}"
    );
    assert_eq!(
        store.peak.load(Ordering::SeqCst),
        1,
        "the bound is a bound: never more than one audit read in flight"
    );

    Arc::into_inner(rt)
        .expect("the only handle")
        .close()
        .await
        .unwrap();
}
