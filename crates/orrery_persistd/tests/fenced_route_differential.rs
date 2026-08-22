//! The differential matrix for the fenced-diff route.
//!
//! `CellRuntime::apply_fenced` no longer reads FoundationDB to choose which
//! actor evaluates the fence; it asks the owner of `record.cell` first and
//! consults `LeaseStore::locate` only when that owner rejects without a row.
//! The whole safety argument for that is one invariant —
//!
//! > **(J)** if an actor's registrar holds a row for entity `e`, then
//! > `locate(e)` names a cell inside that actor's shard subtree (or is
//! > `None`)
//!
//! — plus the claim that the accept set is therefore unchanged. This file
//! turns that claim into a checked fact: over an enumerated state matrix, the
//! shipped implementation and the pre-change one
//! ([`CellRuntime::apply_fenced_via_locate`], retained verbatim as the
//! oracle) must return the same `Result<FencedApply, Reject>` discriminant
//! **and** the same `Option<Lease>` payload.
//!
//! The matrix runs twice: under `MemLeaseStore` on default features, and
//! under `FdbLeaseStore` with `--features fdb` and a reachable cluster.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, FencedApply, JournalConfig, LeaseMigrate, LeasePut,
    LeaseStore, LeaseStoreError, MemFenceStore, MemLeaseStore, Reject, Router, RuntimeConfig,
    LEASE_TTL_MS,
};
use orrery_protocol::{
    CellId, ClaimKind, EntityRekey, Epoch, GridId, JournalRecord, Lease, LeaseFlags, LeaseId, Lsn,
    NodeId, PersistId, RecordKind, SeqPair, Tick, ENTITY_REKEY_VERSION,
};

// -- fixtures ---------------------------------------------------------------

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(
    grid: GridId,
    cell: CellId,
    entity: PersistId,
    kind: RecordKind,
    payload: &[u8],
) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid,
        entity,
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

fn rekey_record(rekey: &EntityRekey) -> JournalRecord {
    let payload = bytes::Bytes::from(postcard::to_allocvec(rekey).unwrap());
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: rekey.source_cell,
        grid: rekey.source_grid,
        entity: rekey.entity,
        tick: Tick::new(7),
        epoch: Epoch::new(2),
        author: test_node(1),
        kind: RecordKind::Rekey,
        crc: payload_crc(&payload),
        payload,
    }
}

/// Two shards under one grid, plus a cell nobody hosts.
///
/// `presented_unhosted` is a leaf under a third root child that is not in
/// `shards`, so `CellRuntime::actor` answers `None` for it — the state that
/// makes the fast path fall through without ever asking an actor.
pub struct Cells {
    shard_a: CellId,
    shard_b: CellId,
    presented: CellId,
    sibling_in_shard_a: CellId,
    in_shard_b: CellId,
    presented_unhosted: CellId,
}

fn cells() -> Cells {
    let roots = CellId::ROOT.children();
    Cells {
        shard_a: roots[0],
        shard_b: roots[1],
        presented: roots[0].children()[0],
        sibling_in_shard_a: roots[0].children()[1],
        in_shard_b: roots[1].children()[0],
        presented_unhosted: roots[2].children()[0],
    }
}

fn runtime_config(dir: &std::path::Path, grid: GridId, cells: &Cells) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![cells.shard_a, cells.shard_b],
        grid,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                batch_window: Duration::from_millis(1),
                ..GroupCommitConfig::default()
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// A lease store whose `migrate` commits and *then* reports failure.
///
/// This is the exact shape of `CommittedRekeyPlan::execute` returning
/// `Err(RekeyError::LeaseStore)` with the location already moved: FDB's
/// `commit_unknown_result` on a transaction that did land. The source actor
/// keeps its registrar row **and** its `pending_rekeys` reservation while the
/// durable location already names the destination, which is the one reachable
/// state in which invariant J is persistently false.
struct MigrateCommitsThenFails {
    inner: Arc<dyn LeaseStore>,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl LeaseStore for MigrateCommitsThenFails {
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
        let committed = self
            .inner
            .migrate(grid, entity, from, to, expected_lease_id)
            .await?;
        if self.armed.load(Ordering::Acquire) {
            return Err(LeaseStoreError("commit_unknown_result".into()));
        }
        Ok(committed)
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

// -- the matrix ------------------------------------------------------------

/// Where the entity's `by_cell` entry and its registrar row live.
///
/// The two travel together on purpose: an entity is leased at the cell it
/// occupies, and the axis the design cares about is which *actor* holds the
/// row relative to the actor that owns the presented cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    /// Never spawned at any cell (the row, if any, is granted at the
    /// presented cell, so the registrar holds it and `by_cell` does not).
    Nowhere,
    /// The cell the diff presents, in shard A.
    Presented,
    /// A different leaf of shard A — same actor, different `by_cell` value.
    SiblingInShardA,
    /// A leaf of shard B — a *different* actor from the presented cell's.
    InShardB,
}

/// Which conjunct of the fence the presented token fails, if any.
///
/// Presenting a wrong token and mutating the row are the same thing to the
/// admission predicate, which compares the two for equality; presenting is
/// the constructible half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    None,
    WrongHolder,
    WrongLeaseId,
    WrongSeq,
    Expired,
    Matching,
}

/// An in-flight rekey reservation left behind by a failed migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Unset,
    /// Reservation on the source, durable location already in shard B.
    ToOtherShard,
    /// Reservation on the source, durable location already at a sibling leaf
    /// of the same shard.
    ToSameShard,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    home: Home,
    row: Row,
    pending: Pending,
    /// `false` presents a cell in a shard this runtime does not host, so
    /// `actor(record.cell)` is `None` and the fast path cannot even ask.
    presented_hosted: bool,
}

fn matrix() -> Vec<Scenario> {
    let mut out = Vec::new();
    for home in [
        Home::Nowhere,
        Home::Presented,
        Home::SiblingInShardA,
        Home::InShardB,
    ] {
        for row in [
            Row::None,
            Row::WrongHolder,
            Row::WrongLeaseId,
            Row::WrongSeq,
            Row::Expired,
            Row::Matching,
        ] {
            for presented_hosted in [true, false] {
                out.push(Scenario {
                    home,
                    row,
                    pending: Pending::Unset,
                    presented_hosted,
                });
            }
        }
    }
    // `pending_rekeys` is not freely crossable with the axes above: the only
    // way to leave a reservation standing is a rekey whose migration
    // committed and then failed, which needs a live row at a real home.
    for pending in [Pending::ToOtherShard, Pending::ToSameShard] {
        for presented_hosted in [true, false] {
            out.push(Scenario {
                home: Home::Presented,
                row: Row::Matching,
                pending,
                presented_hosted,
            });
        }
    }
    out
}

/// What a fenced apply answered, in a form two runs can be compared by.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Rejected(Option<Lease>),
    Failed(Reject),
}

/// The presented token for a scenario, plus the row the grant produced.
struct Presented {
    holder: NodeId,
    lease_id: LeaseId,
    seq: SeqPair,
    now_ms: u64,
}

const CLAIM_AT_MS: u64 = 1_000;
const HOLDER: u8 = 21;

/// Build one scenario's state on `rt`, returning the token the diff presents.
async fn seed(
    rt: &CellRuntime,
    store: &MigrateCommitsThenFails,
    grid: GridId,
    cells: &Cells,
    entity: PersistId,
    scenario: Scenario,
) -> Presented {
    let holder = test_node(HOLDER);
    let home = match scenario.home {
        Home::Nowhere => None,
        Home::Presented => Some(cells.presented),
        Home::SiblingInShardA => Some(cells.sibling_in_shard_a),
        Home::InShardB => Some(cells.in_shard_b),
    };
    if let Some(home) = home {
        rt.apply(mk_record(grid, home, entity, RecordKind::Spawn, b"seed"))
            .await
            .expect("seeding an unfenced spawn at a hosted cell");
    }
    let claim_at = home.unwrap_or(cells.presented);
    let granted = if scenario.row == Row::None {
        None
    } else {
        match Router::claim_lease(
            rt,
            grid,
            claim_at,
            entity,
            holder,
            ClaimKind::Strong,
            CLAIM_AT_MS,
        )
        .await
        .expect("claim must reach an actor")
        {
            ClaimResult::Granted(row) => Some(row),
            other => panic!("scenario {scenario:?}: claim denied: {other:?}"),
        }
    };
    let destination = match scenario.pending {
        Pending::Unset => None,
        Pending::ToOtherShard => Some(cells.in_shard_b),
        Pending::ToSameShard => Some(cells.sibling_in_shard_a),
    };
    if let (Some(row), Some(destination)) = (granted.as_ref(), destination) {
        store.armed.store(true, Ordering::Release);
        let outcome = Router::commit_rekey(
            rt,
            rekey_record(&EntityRekey {
                source_schema_floor: 0,
                version: ENTITY_REKEY_VERSION,
                entity,
                source_grid: grid,
                source_cell: claim_at,
                destination_grid: grid,
                destination_cell: destination,
                expected_lease_id: row.lease_id,
                source_record: bytes::Bytes::from_static(b"seed"),
            }),
        )
        .await;
        store.armed.store(false, Ordering::Release);
        assert!(
            outcome.is_err(),
            "scenario {scenario:?}: the armed migrate must report failure"
        );
        // The state this scenario exists to build: the location already
        // names the destination while the source still holds the row.
        assert_eq!(
            store.locate(grid, entity).await.unwrap(),
            Some(destination),
            "scenario {scenario:?}: the migrate must have committed before it failed"
        );
    }
    let base = granted.unwrap_or(Lease {
        entity,
        holder: Some(holder),
        seq: SeqPair::default(),
        lease_id: LeaseId(1),
        expires_at: CLAIM_AT_MS + LEASE_TTL_MS,
        flags: LeaseFlags(0),
        bound_to: None,
    });
    let (holder, lease_id, seq, now_ms) = match scenario.row {
        Row::None | Row::Matching => (holder, base.lease_id, base.seq, CLAIM_AT_MS + 1),
        Row::WrongHolder => (
            test_node(HOLDER + 1),
            base.lease_id,
            base.seq,
            CLAIM_AT_MS + 1,
        ),
        Row::WrongLeaseId => (
            holder,
            LeaseId(base.lease_id.0.wrapping_add(1)),
            base.seq,
            CLAIM_AT_MS + 1,
        ),
        Row::WrongSeq => (
            holder,
            base.lease_id,
            SeqPair {
                own_seq: base.seq.own_seq.wrapping_add(7),
                auth_seq: base.seq.auth_seq,
            },
            CLAIM_AT_MS + 1,
        ),
        Row::Expired => (
            holder,
            base.lease_id,
            base.seq,
            CLAIM_AT_MS + LEASE_TTL_MS + 1,
        ),
    };
    Presented {
        holder,
        lease_id,
        seq,
        now_ms,
    }
}

async fn settle(result: Result<FencedApply, Reject>) -> Outcome {
    match result {
        Ok(FencedApply::Accepted(handle)) => {
            handle.committed().await.expect("accepted append commits");
            Outcome::Accepted
        }
        Ok(FencedApply::Rejected(row)) => Outcome::Rejected(row),
        Err(reject) => Outcome::Failed(reject),
    }
}

/// The two implementations, over the whole matrix, on two identically-built
/// runtimes so the payloads are comparable field for field.
async fn run_matrix(
    grid_new: GridId,
    grid_old: GridId,
    make_store: impl Fn() -> Arc<dyn LeaseStore>,
) {
    let cells = cells();
    let scenarios = matrix();
    let dir_new = tempfile::tempdir().unwrap();
    let dir_old = tempfile::tempdir().unwrap();
    let store_new = Arc::new(MigrateCommitsThenFails {
        inner: make_store(),
        armed: AtomicBool::new(false),
    });
    let store_old = Arc::new(MigrateCommitsThenFails {
        inner: make_store(),
        armed: AtomicBool::new(false),
    });
    let checkpoints_new: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let rt_new = CellRuntime::open_with_lease_store(
        &runtime_config(dir_new.path(), grid_new, &cells),
        &checkpoints_new,
        Arc::clone(&store_new) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();
    let checkpoints_old: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let rt_old = CellRuntime::open_with_lease_store(
        &runtime_config(dir_old.path(), grid_old, &cells),
        &checkpoints_old,
        Arc::clone(&store_old) as Arc<dyn LeaseStore>,
    )
    .await
    .unwrap();

    let mut presented = Vec::with_capacity(scenarios.len());
    for (index, scenario) in scenarios.iter().copied().enumerate() {
        let entity = PersistId::new(9_000 + index as u64);
        let a = seed(&rt_new, &store_new, grid_new, &cells, entity, scenario).await;
        let b = seed(&rt_old, &store_old, grid_old, &cells, entity, scenario).await;
        assert_eq!(
            (a.lease_id, a.seq, a.now_ms),
            (b.lease_id, b.seq, b.now_ms),
            "scenario {scenario:?}: the two runtimes must be seeded identically"
        );
        presented.push((entity, a));
    }

    let mut divergences = Vec::new();
    let (mut accepted, mut rejected_row, mut rejected_none, mut failed) = (0, 0, 0, 0);
    for (index, scenario) in scenarios.iter().copied().enumerate() {
        let (entity, token) = &presented[index];
        let cell = if scenario.presented_hosted {
            cells.presented
        } else {
            cells.presented_unhosted
        };
        let new = settle(
            Router::apply_fenced(
                &rt_new,
                mk_record(
                    grid_new,
                    cell,
                    *entity,
                    RecordKind::ComponentDiff,
                    b"fenced",
                ),
                token.holder,
                token.lease_id,
                token.seq,
                token.now_ms,
            )
            .await,
        )
        .await;
        let old = settle(
            rt_old
                .apply_fenced_via_locate(
                    mk_record(
                        grid_old,
                        cell,
                        *entity,
                        RecordKind::ComponentDiff,
                        b"fenced",
                    ),
                    token.holder,
                    token.lease_id,
                    token.seq,
                    token.now_ms,
                )
                .await,
        )
        .await;
        match &new {
            Outcome::Accepted => accepted += 1,
            Outcome::Rejected(Some(_)) => rejected_row += 1,
            Outcome::Rejected(None) => rejected_none += 1,
            Outcome::Failed(_) => failed += 1,
        }
        if !same_outcome(&new, &old) {
            divergences.push((scenario, new, old));
        }
    }
    // An oracle that only ever compares two identical rejections proves
    // nothing. Every arm of `FencedApply`/`Reject` the route can produce has
    // to be somewhere in the matrix, or the matrix is decorative.
    assert!(
        accepted > 0 && rejected_row > 0 && rejected_none > 0 && failed > 0,
        "matrix is not exercising every outcome: accepted={accepted} \
         rejected_with_row={rejected_row} rejected_without_row={rejected_none} failed={failed}"
    );

    // Discriminants must match everywhere. An accept or a reject that moved
    // is the change being wrong, full stop.
    for (scenario, new, old) in &divergences {
        assert_eq!(
            std::mem::discriminant(new),
            std::mem::discriminant(old),
            "scenario {scenario:?}: accept/reject moved ({new:?} vs {old:?})"
        );
    }
    // The one payload divergence this change accepts in writing: a rekey
    // whose migration committed and then failed leaves the source holding
    // both the reservation and the row while the location already names the
    // destination in another shard. The oracle routes to the destination and
    // NACKs with `None`; the fast path asks the source, which rejects on
    // conjunct 1 and hands back its live row. Both reject, so admission is
    // identical; the new payload is the more useful of the two, and it is the
    // row the client still holds. See `docs/08-persistence.md` §2.
    let unexpected: Vec<_> = divergences
        .iter()
        .filter(|(scenario, new, old)| {
            !(scenario.pending == Pending::ToOtherShard
                && matches!(new, Outcome::Rejected(Some(_)))
                && matches!(old, Outcome::Rejected(None)))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "fenced route diverged from the locate oracle: {unexpected:#?}"
    );
    // And it must actually happen, or the exemption above is a hole waiting
    // for a real divergence to fall through it. Exactly one scenario in the
    // matrix reaches it: the cross-shard failed migration whose presented
    // cell *is* hosted, so the fast path can reach the source actor at all.
    let accepted_divergences = divergences
        .iter()
        .filter(|(scenario, _, _)| {
            scenario.pending == Pending::ToOtherShard && scenario.presented_hosted
        })
        .count();
    assert_eq!(
        accepted_divergences, 1,
        "the documented NACK-payload divergence must be the one that fires: {divergences:#?}"
    );

    rt_new.close().await.unwrap();
    rt_old.close().await.unwrap();
}

/// Equal outcomes, ignoring the `entity`-independent grid the two runtimes
/// were built under.
///
/// This was a plain `new == old` on the stated grounds that "nothing in
/// `Lease` carries a grid". That stopped being true when `Reject::WrongOwner`
/// gained one (#125): the `fdb` leg deliberately runs the two runtimes under
/// **two different grids** — "two grids, not two stores", so the oracle's
/// runtime cannot see the subject's rows — so every scenario whose outcome is
/// a `WrongOwner` now diverges on a field that is different by construction.
/// Measured 2026-08-20 against a live cluster: eight of the matrix's scenarios
/// diverge, all of them on the grid alone, with identical shard and epoch.
///
/// So the grid is normalised out here rather than the two runtimes being put
/// on one grid, which would defeat the isolation the fdb leg exists for. The
/// shard and the epoch — the two fields that say anything about *routing* —
/// are still compared exactly.
fn same_outcome(new: &Outcome, old: &Outcome) -> bool {
    fn without_grid(outcome: &Outcome) -> Outcome {
        match outcome {
            Outcome::Failed(Reject::WrongOwner { shard, epoch, .. }) => {
                Outcome::Failed(Reject::WrongOwner {
                    grid: GridId::ROOT,
                    shard: *shard,
                    epoch: *epoch,
                })
            }
            other => other.clone(),
        }
    }
    without_grid(new) == without_grid(old)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_route_matches_the_locate_oracle_over_the_state_matrix() {
    run_matrix(
        GridId::ROOT,
        GridId::ROOT,
        || Arc::new(MemLeaseStore::new()),
    )
    .await;
}

#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fdb_fenced_route_matches_the_locate_oracle_over_the_state_matrix() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let context = Arc::new(
        orrery_persistd::FdbContext::connect(&cluster)
            .expect("configured FDB cluster file must open"),
    );
    // Two grids, not two stores: the oracle's runtime must not see the
    // shipped implementation's rows, and one FDB keyspace is shared.
    run_matrix(unique_grid_id(), unique_grid_id(), move || {
        Arc::new(orrery_persistd::FdbLeaseStore::from_context(&context))
    })
    .await;
}

#[cfg(feature = "fdb")]
fn unique_grid_id() -> GridId {
    use std::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    GridId::new(
        elapsed.subsec_nanos()
            ^ std::process::id().rotate_left(16)
            ^ NEXT.fetch_add(1, Ordering::Relaxed),
    )
}
