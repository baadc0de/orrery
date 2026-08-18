//! A misrouted claim must not manufacture an invariant-J violation.
//!
//! docs/08-persistence.md §2.1.2 names `LeaseStore::put` answering
//! `LocationConflict` as the reason the `claim_lease` row-install site cannot
//! break J: "the claim is routed to `actor(locate().unwrap_or(cell))` and
//! stores `location = cell`; `LeaseStore::put` answers `LocationConflict`
//! rather than overwrite a different location, so even a misrouted claim
//! cannot manufacture a violation".
//!
//! That guard was untested. Replacing the whole
//! `LeasePut::LocationConflict(_) => return Denied(NotEligible)` arm with
//! `{}` survived the full suite — the actor would fall through, install
//! `lease_cells[e]` at a cell whose durable location belongs to another
//! shard, and hand back a `Granted` row for an entity it does not own. That
//! is precisely the J-false state `apply_fenced`'s routing argument forbids.
//!
//! The misroute is produced the only way it is reachable: a durable location
//! index that answers with a cell in the wrong shard. Everything else —
//! `put`, `load_cell` — stays truthful, so the conflict the actor meets is a
//! real one.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, JournalConfig, LeaseMigrate, LeasePut, LeaseStore,
    LeaseStoreError, MemFenceStore, MemLeaseStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, DenyReason, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId,
    PersistId, RecordKind, Tick,
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

/// A truthful `MemLeaseStore` whose `locate` can be pointed at a chosen cell,
/// so a claim can be routed to an actor that does not own the entity.
struct MisroutingLeaseStore {
    inner: MemLeaseStore,
    forged: Mutex<Option<CellId>>,
}

#[async_trait::async_trait]
impl LeaseStore for MisroutingLeaseStore {
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
        if let Some(cell) = *self.forged.lock().expect("forge lock") {
            return Ok(Some(cell));
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
async fn a_misrouted_claim_is_denied_rather_than_installing_a_row_in_the_wrong_shard() {
    let roots = CellId::ROOT.children();
    let (shard_a, shard_b) = (roots[0], roots[1]);
    let cell_a = shard_a.children()[0];
    let cell_b = shard_b.children()[0];
    let owner = test_node(41);
    let intruder = test_node(42);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MisroutingLeaseStore {
        inner: MemLeaseStore::new(),
        forged: Mutex::new(None),
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

    // The entity lives in shard B, and its durable location says so.
    let entity = PersistId::new(8_401);
    rt.apply(mk_record(cell_b, entity, b"seed")).await.unwrap();
    let ClaimResult::Granted(held) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cell_b,
        entity,
        owner,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the shard-B entity must be granted a lease");
    };

    // Now the location index misroutes: the claim below is sent to shard A's
    // actor, which has never seen this entity and so has nothing local to
    // refuse it with. Only `LeaseStore::put` knows the entity is committed to
    // shard B.
    *store.forged.lock().unwrap() = Some(cell_a);
    let denied = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cell_a,
        entity,
        intruder,
        ClaimKind::Strong,
        1_100,
    )
    .await
    .unwrap();
    assert!(
        matches!(denied, ClaimResult::Denied(DenyReason::NotEligible)),
        "a claim whose cell conflicts with the durable location must be denied, got {denied:?}"
    );
    *store.forged.lock().unwrap() = None;

    // Shard A must hold no row at all: a row there is exactly the J-false
    // state `apply_fenced` routing by `record.cell` is not allowed to meet.
    let (row_a, _, _) = rt
        .actor(GridId::ROOT, cell_a)
        .expect("shard A actor")
        .inspect_lease(entity)
        .await
        .unwrap();
    assert!(
        row_a.is_none(),
        "the denied claim must not install a registrar row in shard A: {row_a:?}"
    );

    // And the real owner is untouched, row and durable location alike.
    let (row_b, _, _) = rt
        .actor(GridId::ROOT, cell_b)
        .expect("shard B actor")
        .inspect_lease(entity)
        .await
        .unwrap();
    assert_eq!(
        row_b,
        Some(held),
        "the shard-B holder keeps the lease it was granted"
    );
    assert_eq!(
        rt.lease_location(entity).await.unwrap(),
        Some(cell_b),
        "the durable location must not have moved"
    );

    rt.close().await.unwrap();
}
