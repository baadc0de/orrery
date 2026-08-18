//! A dead mailbox at the presented cell is not an answer about the fence.
//!
//! `CellRuntime::apply_fenced` enters its locate fallback on a rowless
//! reject, on there being no actor for the presented cell — and on
//! `Reject::JournalClosed` from an actor that exists but cannot answer.
//! Without that third trigger the route would report `JournalClosed` where
//! the pre-change route located the true owner and returned its live row: a
//! `BulkNack` reason 1 where there should be a reason 2 carrying the row the
//! D7 §5 duplicate-authority detector reads.
//!
//! Its own test binary because `RouteStageMetrics` is process-global and
//! `fenced_route_bounds.rs` takes deltas around its own calls.

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
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

/// A store that refuses to load one shard's registrar rows at actor spawn.
///
/// That is how an actor ends up **registered with a dead mailbox**: startup
/// recovery is inside the spawned task, and a failed `load_cell` makes it
/// `return` — failing closed, deliberately, rather than serving an empty
/// registrar and minting duplicate leases — while `CellRuntime::open`
/// succeeds and keeps the handle.
struct RefuseLoadCellFor {
    inner: MemLeaseStore,
    shard: CellId,
}

#[async_trait::async_trait]
impl LeaseStore for RefuseLoadCellFor {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        if shard == self.shard {
            return Err(LeaseStoreError("refusing this shard".into()));
        }
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

/// The reachable case: an actor whose startup recovery failed keeps its
/// handle and loses its mailbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_mailbox_at_the_presented_cell_falls_back_like_no_actor_at_all() {
    let roots = CellId::ROOT.children();
    let (shard_a, shard_b) = (roots[0], roots[1]);
    let presented = shard_a.children()[0];
    let in_shard_b = shard_b.children()[0];
    let holder = test_node(33);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RefuseLoadCellFor {
        inner: MemLeaseStore::new(),
        shard: shard_a,
    });
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

    let entity = PersistId::new(7_101);
    rt.apply(mk_record(in_shard_b, entity, b"seed"))
        .await
        .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        in_shard_b,
        entity,
        holder,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the shard-B entity must be granted a lease");
    };

    // Shard A's actor is registered but cannot answer.
    assert!(rt.actor(GridId::ROOT, presented).is_some());
    let stale = Router::apply_fenced(
        &rt,
        mk_record(presented, entity, b"dead-mailbox"),
        holder,
        grant.lease_id,
        grant.seq,
        1_001,
    )
    .await
    .unwrap();
    let FencedApply::Rejected(Some(current)) = stale else {
        panic!("a dead mailbox must not swallow the fence answer: {stale:?}");
    };
    assert_eq!(current, grant);

    // And the pre-change route agrees, which is the point.
    let oracle = rt
        .apply_fenced_via_locate(
            mk_record(presented, entity, b"dead-mailbox"),
            holder,
            grant.lease_id,
            grant.seq,
            1_001,
        )
        .await
        .unwrap();
    assert!(matches!(oracle, FencedApply::Rejected(Some(row)) if row == grant));

    rt.close().await.unwrap();
}
