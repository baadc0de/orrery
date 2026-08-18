//! The sampled invariant-J audit must not put FoundationDB back under the
//! entity gate, and it must be billed to a stage of its own.
//!
//! The audit is a real `LeaseStore::locate`. It used to run inside
//! `apply_fenced`'s entity-gate critical section, *after*
//! `RouteStageMetrics::record` had already been called — so its cost was
//! excluded from all three stage timers and reappeared, misattributed, as the
//! next diff's `gate_wait_us`. With a 5 ms locate and eight concurrent
//! accepts on one entity that read as `applies=8 locate_us_sum=0
//! gate_wait_us_sum=171433` for a 49.5 ms wall.
//!
//! This pins the fix from both sides: the concurrent accepts must not
//! serialize behind each other's audits (`gate_wait_us_sum` stays small), and
//! the time the audits do cost must appear in `location_audit_us_sum` rather
//! than nowhere.
//!
//! Its own test binary because `RouteStageMetrics` is process-global: a
//! second test taking deltas in parallel would read this one's work.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::route_stage_metrics;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, FencedApply, JournalConfig, LeaseMigrate, LeasePut,
    LeaseStore, LeaseStoreError, MemFenceStore, MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId, PersistId,
    RecordKind, Tick,
};

/// Deliberately far above any plausible scheduling jitter on a loaded box, so
/// the assertions below are about where the cost is charged, not about how
/// fast the box is.
const LOCATE_DELAY: Duration = Duration::from_millis(25);
const ACCEPTS: usize = 8;

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

/// A lease store whose `locate` costs real wall time, like the FDB one.
#[derive(Default)]
struct SlowLocateLeaseStore {
    inner: MemLeaseStore,
    locates: AtomicUsize,
    slow: std::sync::atomic::AtomicBool,
}

impl SlowLocateLeaseStore {
    fn locates(&self) -> usize {
        self.locates.load(Ordering::Acquire)
    }
    fn go_slow(&self) {
        self.slow.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl LeaseStore for SlowLocateLeaseStore {
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
        self.locates.fetch_add(1, Ordering::AcqRel);
        if self.slow.load(Ordering::Acquire) {
            tokio::time::sleep(LOCATE_DELAY).await;
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
async fn the_sampled_audit_runs_with_the_entity_gate_released_and_is_billed_to_itself() {
    // Audit every accept, so eight accepts buy eight audits and the effect is
    // at its largest. In release this is one in a thousand.
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "1");

    let shard = CellId::ROOT.children()[0];
    let cell = shard.children()[0];
    let holder = test_node(31);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SlowLocateLeaseStore::default());
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

    let entity = PersistId::new(9_101);
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

    // Only now does `locate` become expensive: the setup above uses it too.
    store.go_slow();
    let locates_before = store.locates();
    let before = route_stage_metrics().snapshot();

    let mut running = Vec::new();
    for tick in 0..ACCEPTS {
        let rt = Arc::clone(&rt);
        running.push(tokio::spawn(async move {
            Router::apply_fenced(
                &*rt,
                mk_record(cell, entity, 100 + tick as u64, b"diff"),
                holder,
                grant.lease_id,
                grant.seq,
                1_001,
            )
            .await
        }));
    }
    for task in running {
        let applied = task.await.unwrap().unwrap();
        let FencedApply::Accepted(handle) = applied else {
            panic!("a live fence at its own cell must be admitted");
        };
        handle.committed().await.unwrap();
    }

    let delta = route_stage_metrics().snapshot().delta(before);
    let audit_locates = store.locates() - locates_before;

    assert_eq!(delta.applies, ACCEPTS as u64);
    assert_eq!(
        delta.locate_fallbacks, 0,
        "every accept took the fast path, so no locate belongs to routing"
    );
    assert_eq!(
        delta.locate_us_sum, 0,
        "the route's own locate stage must stay empty; the audit has its own"
    );
    assert_eq!(
        audit_locates, ACCEPTS,
        "one sampled audit per accept at N=1, and nothing else reads the store"
    );
    assert_eq!(delta.location_audits, ACCEPTS as u64);
    assert_eq!(delta.location_mismatches, 0);
    assert_eq!(delta.location_audit_errors, 0);

    // The audits cost real time, and that time is now attributable.
    let floor = (ACCEPTS as u64) * u64::try_from(LOCATE_DELAY.as_micros()).unwrap() * 8 / 10;
    assert!(
        delta.location_audit_us_sum >= floor,
        "the audits' own stage must carry their cost: {} us < {floor} us",
        delta.location_audit_us_sum
    );

    // The point of the fix. With the audit inside the critical section each
    // accept waits for every earlier accept's audit, and the sum grows as
    // n(n-1)/2 * delay -- 700 ms at these settings. Off the gate, the accepts
    // queue only behind one another's mailbox turns.
    let ceiling = u64::try_from(LOCATE_DELAY.as_micros()).unwrap() * 4;
    assert!(
        delta.gate_wait_us_sum < ceiling,
        "the audit must not be held under the entity gate: gate_wait_us_sum={} >= {ceiling}",
        delta.gate_wait_us_sum
    );

    Arc::try_unwrap(rt).ok().unwrap().close().await.unwrap();
}
