//! The performance claim of the fenced route, expressed as correctness
//! assertions, and the bound on its fallback.
//!
//! "FoundationDB is off the bulk write path" is a statement about a call
//! count, so it is assertable: a counting `LeaseStore` decorator sees zero
//! `locate` calls across accepted fenced diffs. And the fallback that remains
//! is bounded by construction — one locate, at most two mailbox turns, no
//! loop — which is what keeps the reject path from becoming the old cost plus
//! overhead.
//!
//! One test function, on purpose: `RouteStageMetrics` is process-global, so
//! two tests taking deltas around their own calls in parallel would read each
//! other's work.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::{route_stage_metrics, RouteStageSnapshot};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, FencedApply, JournalConfig, LeaseMigrate, LeasePut,
    LeaseStore, LeaseStoreError, MemFenceStore, MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId, PersistId,
    RecordKind, Tick,
};

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

/// Every `LeaseStore` call the route makes, counted.
#[derive(Default)]
struct CountingLeaseStore {
    inner: MemLeaseStore,
    locates: AtomicUsize,
}

impl CountingLeaseStore {
    fn locates(&self) -> usize {
        self.locates.load(Ordering::Acquire)
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
        self.locates.fetch_add(1, Ordering::AcqRel);
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

struct Delta {
    locates: usize,
    stage: RouteStageSnapshot,
}

fn since(store: &CountingLeaseStore, mark: (usize, RouteStageSnapshot)) -> Delta {
    Delta {
        locates: store.locates() - mark.0,
        stage: route_stage_metrics().snapshot().delta(mark.1),
    }
}

fn mark(store: &CountingLeaseStore) -> (usize, RouteStageSnapshot) {
    (store.locates(), route_stage_metrics().snapshot())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_route_reads_no_lease_store_and_bounds_its_fallback() {
    // The sampled J audit is a *deliberate* FDB read on the accept path, at
    // one in a thousand in release and one in one under `debug_assertions`.
    // It is not the route, so it is turned off here: the claim under test is
    // "routing reads nothing", and mixing the audit into the count would
    // measure the audit instead.
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "0");

    let roots = CellId::ROOT.children();
    let (shard_a, shard_b) = (roots[0], roots[1]);
    let presented = shard_a.children()[0];
    let in_shard_b = shard_b.children()[0];
    let holder = test_node(31);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CountingLeaseStore::default());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let config = RuntimeConfig {
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
        fence: Arc::new(MemFenceStore::new()),
    };
    let rt = CellRuntime::open_with_lease_store(
        &config,
        &checkpoints,
        Arc::clone(&store) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();

    // -- zero FDB on the accept path -------------------------------------
    let hot = PersistId::new(7_001);
    rt.apply(mk_record(presented, hot, b"seed")).await.unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        presented,
        hot,
        holder,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the hot entity must be granted a lease");
    };
    let before = mark(&store);
    const ACCEPTS: usize = 64;
    for tick in 0..ACCEPTS {
        let applied = Router::apply_fenced(
            &rt,
            mk_record(presented, hot, format!("diff-{tick}").as_bytes()),
            holder,
            grant.lease_id,
            grant.seq,
            1_001,
        )
        .await
        .unwrap();
        let FencedApply::Accepted(handle) = applied else {
            panic!("a live fence at its own cell must be admitted (tick {tick})");
        };
        handle.committed().await.unwrap();
    }
    let delta = since(&store, before);
    assert_eq!(
        delta.locates, 0,
        "an accepted fenced diff must not read the lease store"
    );
    assert_eq!(delta.stage.applies, ACCEPTS as u64);
    assert_eq!(
        delta.stage.mailbox_turns, ACCEPTS as u64,
        "the fast path is exactly one mailbox turn per diff"
    );
    assert_eq!(delta.stage.locate_fallbacks, 0);
    assert_eq!(delta.stage.locate_us_sum, 0);

    // -- cross-shard stale cell: one locate, two turns --------------------
    let elsewhere = PersistId::new(7_002);
    rt.apply(mk_record(in_shard_b, elsewhere, b"seed"))
        .await
        .unwrap();
    let ClaimResult::Granted(other_grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        in_shard_b,
        elsewhere,
        holder,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the shard-B entity must be granted a lease");
    };
    let before = mark(&store);
    let stale = Router::apply_fenced(
        &rt,
        mk_record(presented, elsewhere, b"stale-shard"),
        holder,
        other_grant.lease_id,
        other_grant.seq,
        1_001,
    )
    .await
    .unwrap();
    let FencedApply::Rejected(Some(current)) = stale else {
        panic!("a diff presenting another shard's cell must be rejected with the live row");
    };
    assert_eq!(
        current, other_grant,
        "the NACK must still carry the row the true owner holds"
    );
    let delta = since(&store, before);
    assert_eq!(delta.locates, 1, "the fallback locates exactly once");
    assert_eq!(delta.stage.locate_fallbacks, 1);
    assert_eq!(
        delta.stage.mailbox_turns, 2,
        "ask the presented cell's owner, then the true owner: two turns, never three"
    );

    // -- the fallback that resolves to the actor already asked ------------
    // An entity with no row anywhere: the presented cell's owner rejects
    // without a row, the locate answers `None`, and `unwrap_or(record.cell)`
    // names the actor that already answered. It must not be asked twice.
    let unknown = PersistId::new(7_003);
    let before = mark(&store);
    let missing = Router::apply_fenced(
        &rt,
        mk_record(presented, unknown, b"unknown"),
        holder,
        LeaseId(1),
        grant.seq,
        1_001,
    )
    .await
    .unwrap();
    assert!(
        matches!(missing, FencedApply::Rejected(None)),
        "an unleased entity is rejected with no row"
    );
    let delta = since(&store, before);
    assert_eq!(delta.locates, 1, "still exactly one locate");
    assert_eq!(delta.stage.locate_fallbacks, 1);
    assert_eq!(
        delta.stage.mailbox_turns, 1,
        "the fallback must not re-send to the actor that already answered"
    );

    rt.close().await.unwrap();
}
