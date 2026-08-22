//! The batched heartbeat path must not hold entity gates across I/O.
//!
//! The locate phase these tests are about is now the route's **fallback**:
//! renewals are answered by the actor owning the presented cell, and only
//! entries it has no row for read the lease store (docs/08-persistence.md
//! §2.2.4). So both batches below are built to miss — the leases live in one
//! shard and the renewals present a cell in another — because a batch that
//! hits takes no locate at all and would pass these tests without entering
//! the code they are about. The sampled invariant-J audit is turned off for
//! the same reason: it is a real `locate`, and it would otherwise be the
//! thing that trips the park.
//!
//! `heartbeat_leases` used to lock every renewed entity's gate up front and
//! then issue one `LeaseStore::locate` per entry underneath the whole set, so
//! a peer renewing 77 leases blocked every diff touching any of those 77
//! entities for the length of 77 FDB reads. These two tests pin the two
//! halves of the fix that can silently regress: the gates really are free
//! while the batch locates, and a location that goes stale in that window is
//! detected rather than applied to the wrong actor.

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
    RecordKind, Tick, ENTITY_REKEY_VERSION,
};
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

fn checkpoints() -> Arc<dyn CheckpointStore> {
    Arc::new(MemCheckpointStore::new())
}

/// A registrar tier whose `locate` for one chosen entity answers from the
/// pre-call state and then parks until it is released.
///
/// It reads through *first* and blocks afterwards deliberately: that is the
/// shape of the race the second test is about — a location that was correct
/// when it was read and is not by the time it is used.
struct ParkingLocateStore {
    inner: MemLeaseStore,
    park: PersistId,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    armed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LeaseStore for ParkingLocateStore {
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
        if entity == self.park && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
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

/// While a renewal batch is resolving locations, every gate it will take is
/// still free.
///
/// The proof is a second, single-entity lease call for an entity *inside* the
/// batch: it takes that entity's gate, so it can only complete if the batch is
/// not already holding it. With the gates taken up front this call cannot
/// return until the batch's last `locate` does, and the timeout below fires.
#[tokio::test]
async fn a_renewal_batch_holds_no_gate_while_it_resolves_locations() {
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "0");
    let dir = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let parked = PersistId::new(8_040);
    let store = Arc::new(ParkingLocateStore {
        inner: MemLeaseStore::new(),
        park: parked,
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    // The leases live in `home`; the renewals present `away`, whose actor has
    // no row for any of them, so the whole batch takes the locate fallback.
    let roots = CellId::ROOT.children();
    let (home, away) = (roots[0], roots[1]);
    let rt = Arc::new(
        CellRuntime::open_with_lease_store(
            &runtime_config(dir.path(), vec![home, away]),
            &checkpoints(),
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .unwrap(),
    );
    let holder = test_node(31);

    let mut batch = Vec::new();
    for id in 8_040..8_080u64 {
        let entity = PersistId::new(id);
        let ClaimResult::Granted(row) = Router::claim_lease(
            rt.as_ref(),
            GridId::ROOT,
            home,
            entity,
            holder,
            ClaimKind::Weak,
            0,
        )
        .await
        .unwrap() else {
            panic!("claim {id} should be granted");
        };
        batch.push(LeaseRenewal {
            cell: away,
            entity,
            lease_id: row.lease_id,
        });
    }
    // The witness is the *last* entry, so under the old shape the batch is
    // holding every gate — including the witness's — by the time it parks.
    let witness = *batch.last().unwrap();
    assert_ne!(witness.entity, parked);

    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let renewals = batch.clone();
    let batch_rt = Arc::clone(&rt);
    let batched = tokio::spawn(async move {
        Router::heartbeat_leases(batch_rt.as_ref(), GridId::ROOT, holder, &renewals, 1).await
    });
    entered.notified().await;

    // The batch is parked inside `locate`. A single-entity renewal of an
    // entity in the same batch must still get its gate.
    let single = tokio::time::timeout(
        Duration::from_secs(5),
        Router::heartbeat_lease(
            rt.as_ref(),
            GridId::ROOT,
            witness.cell,
            witness.entity,
            holder,
            witness.lease_id,
            2,
        ),
    )
    .await
    .expect("a renewal batch must not hold its gates across `locate`")
    .unwrap();
    assert_eq!(single.expect("witness row").lease_id, witness.lease_id);

    release.notify_one();
    let rows = batched.await.unwrap().unwrap();
    assert_eq!(rows.len(), batch.len());
    for (entry, row) in batch.iter().zip(&rows) {
        let row = row.as_ref().expect("every held pair renews");
        assert_eq!(row.entity, entry.entity, "the reply stays positional");
        assert_eq!(row.lease_id, entry.lease_id);
    }

    Arc::try_unwrap(rt).ok().unwrap().close().await.unwrap();
}

/// A location that migrates between the batch's `locate` and the batch's gate
/// is re-resolved, not applied to the actor that no longer owns the entity.
///
/// Two shards, so a stale location means a *different actor*: routing the
/// renewal to the source shard after the entity has moved answers `None` and
/// costs the peer a lease it still holds.
#[tokio::test]
async fn a_renewal_whose_entity_migrates_under_it_is_re_resolved() {
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "0");
    let dir = tempfile::tempdir().unwrap();
    let cells = CellId::ROOT.children();
    let (source, destination) = (cells[0], cells[1]);
    let entity = PersistId::new(9_101);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let store = Arc::new(ParkingLocateStore {
        inner: MemLeaseStore::new(),
        park: entity,
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let rt = Arc::new(
        CellRuntime::open_with_lease_store(
            &runtime_config(dir.path(), vec![source, destination]),
            &checkpoints(),
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .unwrap(),
    );
    let holder = test_node(13);
    // A rekey moves a *durable* entity, so give it a world row in `source`
    // first; without one `prepare_rekey` answers `SourceEntityMissing`.
    let image = b"pre-migration".as_slice();
    rt.apply(JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: source,
        grid: GridId::ROOT,
        entity,
        tick: Tick::new(1),
        epoch: Epoch::new(1),
        author: test_node(1),
        kind: RecordKind::Spawn,
        crc: payload_crc(image),
        payload: bytes::Bytes::copy_from_slice(image),
    })
    .await
    .unwrap();
    let actor = rt.actor(GridId::ROOT, source).expect("source actor");
    let ClaimResult::Granted(grant) = actor
        .claim_lease(entity, source, holder, ClaimKind::Strong, 20)
        .await
        .unwrap()
    else {
        panic!("source lease must be granted");
    };
    // Presenting the destination, which holds no row yet: that is what sends
    // this renewal down the locate fallback, where the stale-location
    // re-resolve lives.
    let renewals = vec![LeaseRenewal {
        cell: destination,
        entity,
        lease_id: grant.lease_id,
    }];
    assert_eq!(
        rt.lease_location(entity).await.unwrap(),
        Some(source),
        "the batch is about to read this, and it is about to stop being true"
    );

    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let batch_rt = Arc::clone(&rt);
    let batched = tokio::spawn(async move {
        Router::heartbeat_leases(batch_rt.as_ref(), GridId::ROOT, holder, &renewals, 30).await
    });
    // The batch has read `source` and is parked holding nothing.
    entered.notified().await;

    let rekey = orrery_protocol::EntityRekey {
        source_schema_floor: 0,
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: source,
        destination_grid: GridId::ROOT,
        destination_cell: destination,
        expected_lease_id: grant.lease_id,
        source_record: bytes::Bytes::copy_from_slice(image),
    };
    let payload = bytes::Bytes::from(postcard::to_allocvec(&rekey).unwrap());
    Router::commit_rekey(
        rt.as_ref(),
        JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: source,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(7),
            epoch: Epoch::new(1),
            author: test_node(1),
            kind: RecordKind::Rekey,
            crc: payload_crc(&payload),
            payload,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rt.lease_location(entity).await.unwrap(),
        Some(destination),
        "the migration committed while the batch was parked"
    );

    release.notify_one();
    let rows = tokio::time::timeout(Duration::from_secs(10), batched)
        .await
        .expect("the re-resolved entry must not deadlock on its own gate")
        .unwrap()
        .unwrap();
    let row = rows[0]
        .as_ref()
        .expect("the renewal must follow the entity to its new actor");
    assert_eq!(row.entity, entity);
    assert_eq!(row.lease_id, grant.lease_id);
    assert_eq!(row.holder, Some(holder));

    Arc::try_unwrap(rt).ok().unwrap().close().await.unwrap();
}
