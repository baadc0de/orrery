//! The sampled invariant-J audit is a safety net, so this is the test that it
//! can actually catch something.
//!
//! `location_mismatches` "must be zero" is the whole published merge argument
//! for taking the per-diff locate off the fenced bulk path, and a sweep
//! observing zero at five load points does not distinguish a healthy system
//! from a counter that cannot fire. Two source mutations survived the entire
//! 342-test suite before this file existed:
//!
//! * `let mismatched = owner != Some(accepting_shard);` -> `let mismatched = false;`
//! * an early `return` at the top of the audit
//!
//! Both are killed below, and so are the two structural blind spots the same
//! review named: a `locate` that answers `None` or errors used to increment
//! neither counter, and a *fallback-forwarded* accept used not to be audited
//! at all — the one branch where the presented cell was demonstrably not the
//! owner, and therefore the branch best placed to see a false J.
//!
//! The sampler is pinned to one-in-one here so the assertions hold in either
//! build profile; the *default* it would otherwise resolve to is pinned
//! separately, in `fenced_audit_sampling_default.rs`, which must set no
//! environment variable at all.
//!
//! Its own test binary because `RouteStageMetrics` is process-global.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::{route_stage_metrics, settle_location_audits, RouteStageSnapshot};
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

/// What the durable location index answers, independently of what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locates {
    /// Tell the truth.
    Truthfully,
    /// The location key moved behind the accepting actor's back: exactly the
    /// state invariant J forbids, and the only one the audit exists to catch.
    At(CellId),
    /// The key is gone.
    Nowhere,
    /// The read failed.
    Erroring,
}

/// A `MemLeaseStore` whose `locate` can be made to lie, in each of the three
/// ways the audit has to tell apart.
///
/// `put` / `load_cell` / `migrate` stay truthful throughout, so the *actor*
/// keeps the state a real grant produced and only the durable location index
/// diverges — which is what makes the J-false state under test a J-false
/// state rather than a broken fixture.
struct ScriptedLeaseStore {
    inner: MemLeaseStore,
    mode: Mutex<Locates>,
}

impl ScriptedLeaseStore {
    fn new() -> Self {
        Self {
            inner: MemLeaseStore::new(),
            mode: Mutex::new(Locates::Truthfully),
        }
    }
    fn set(&self, mode: Locates) {
        *self.mode.lock().expect("mode lock") = mode;
    }
}

#[async_trait::async_trait]
impl LeaseStore for ScriptedLeaseStore {
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
        let mode = *self.mode.lock().expect("mode lock");
        match mode {
            Locates::Truthfully => self.inner.locate(grid, entity).await,
            Locates::At(cell) => Ok(Some(cell)),
            Locates::Nowhere => Ok(None),
            Locates::Erroring => Err(LeaseStoreError("scripted audit failure".into())),
        }
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

/// Route one fenced diff that must be admitted, and wait for it to commit.
#[allow(clippy::too_many_arguments)]
async fn accept(
    rt: &CellRuntime,
    tick: u64,
    entity: PersistId,
    cell: CellId,
    holder: NodeId,
    lease_id: LeaseId,
    seq: orrery_protocol::SeqPair,
) {
    let applied = Router::apply_fenced(
        rt,
        mk_record(cell, entity, tick, b"diff"),
        holder,
        lease_id,
        seq,
        1_001,
    )
    .await
    .unwrap();
    let FencedApply::Accepted(handle) = applied else {
        panic!("expected an accept at tick {tick}, got {applied:?}");
    };
    handle.committed().await.unwrap();
}

fn mark() -> RouteStageSnapshot {
    route_stage_metrics().snapshot()
}

/// The delta since `previous`, **after** every decided audit has landed.
///
/// The audit runs on a detached task since 2026-08-19 — it used to be awaited
/// inside `apply_fenced`, where the gateway's route-admission timeout could
/// cancel it and shed the diff with it (see `fenced_audit_never_sheds.rs`).
/// So "the accept returned" no longer implies "its sample is counted", and
/// every assertion below would otherwise be a race. The settle is asserted
/// rather than best-effort: an audit that never lands is itself the defect
/// this counter set exists to make impossible.
async fn since(previous: RouteStageSnapshot) -> RouteStageSnapshot {
    assert!(
        settle_location_audits(Duration::from_secs(30)).await,
        "a decided audit never landed in a counter: {:?}",
        route_stage_metrics().snapshot().delta(previous)
    );
    route_stage_metrics().snapshot().delta(previous)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sampled_location_audit_can_fire_and_counts_every_outcome() {
    // Audit every accept. The default is this under `debug_assertions` and
    // one in a thousand in release; setting it explicitly is what lets the
    // counter assertions below be exact in both.
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "1");

    let roots = CellId::ROOT.children();
    let (shard_a, shard_b) = (roots[0], roots[1]);
    let cell_a = shard_a.children()[0];
    let cell_b = shard_b.children()[0];
    let holder = test_node(37);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ScriptedLeaseStore::new());
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

    // A perfectly ordinary leased entity in shard A.
    let healthy = PersistId::new(8_201);
    rt.apply(mk_record(cell_a, healthy, 1, b"seed"))
        .await
        .unwrap();
    let ClaimResult::Granted(grant) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cell_a,
        healthy,
        holder,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the shard-A entity must be granted a lease");
    };

    // -- the audit runs at all, at the debug default of one in one ---------
    // Kills the `return` at the top of the audit: with it, both counters stay
    // flat forever and every "must be zero" reading is vacuous.
    let before = mark();
    accept(&rt, 10, healthy, cell_a, holder, grant.lease_id, grant.seq).await;
    let delta = since(before).await;
    assert_eq!(delta.applies, 1);
    assert_eq!(
        delta.location_audits, 1,
        "at one-in-one every accept is audited"
    );
    assert_eq!(delta.location_mismatches, 0, "J holds for an honest grant");
    assert_eq!(delta.location_audit_errors, 0);

    // -- a J-false state is caught -----------------------------------------
    // The durable location now names a cell in shard B while shard A's actor
    // still holds the row and still admits the write. That is the one silent
    // failure the whole "the accept set is unchanged" argument rests on.
    // Kills `let mismatched = false;`.
    store.set(Locates::At(cell_b));
    let before = mark();
    accept(&rt, 11, healthy, cell_a, holder, grant.lease_id, grant.seq).await;
    let delta = since(before).await;
    assert_eq!(delta.location_audits, 1);
    assert_eq!(
        delta.location_mismatches, 1,
        "an accept whose durable location sits in another shard must be counted"
    );
    assert_eq!(delta.location_audit_errors, 0);
    store.set(Locates::Truthfully);

    // -- a locate that answers `None` is not a clean bill of health --------
    store.set(Locates::Nowhere);
    let before = mark();
    accept(&rt, 12, healthy, cell_a, holder, grant.lease_id, grant.seq).await;
    let delta = since(before).await;
    assert_eq!(
        delta.location_audits, 0,
        "a missing location key is not a verdict"
    );
    assert_eq!(delta.location_mismatches, 0);
    assert_eq!(
        delta.location_audit_errors, 1,
        "a sample that read nothing must still be visible"
    );

    // -- and neither is a failing one --------------------------------------
    store.set(Locates::Erroring);
    let before = mark();
    accept(&rt, 13, healthy, cell_a, holder, grant.lease_id, grant.seq).await;
    let delta = since(before).await;
    assert_eq!(delta.location_audits, 0);
    assert_eq!(delta.location_mismatches, 0);
    assert_eq!(
        delta.location_audit_errors, 1,
        "a lease store that cannot answer must not read as agreement"
    );
    store.set(Locates::Truthfully);

    // -- a fallback-forwarded accept is audited too ------------------------
    // Built from the exact state docs/08 §2.1.2 names as reachable: a
    // cross-shard duplicate `by_cell` entry. Shard B holds the entity's row
    // and its durable location, and a replayed record at a shard-A cell moves
    // B's `by_cell` there without giving shard A a row. So the presented
    // cell's owner (A) rejects with no row, the locate names B, and B admits
    // the forwarded diff.
    let forwarded = PersistId::new(8_202);
    rt.apply(mk_record(cell_b, forwarded, 1, b"seed"))
        .await
        .unwrap();
    let ClaimResult::Granted(other) = Router::claim_lease(
        &rt,
        GridId::ROOT,
        cell_b,
        forwarded,
        holder,
        ClaimKind::Strong,
        1_000,
    )
    .await
    .unwrap() else {
        panic!("the shard-B entity must be granted a lease");
    };
    rt.actor(GridId::ROOT, cell_b)
        .expect("shard B actor")
        .restore_apply(mk_record(cell_a, forwarded, 2, b"replayed-elsewhere"))
        .await
        .unwrap();

    let before = mark();
    accept(
        &rt,
        14,
        forwarded,
        cell_a,
        holder,
        other.lease_id,
        other.seq,
    )
    .await;
    let delta = since(before).await;
    assert_eq!(
        delta.locate_fallbacks, 1,
        "shard A rejects without a row, so the route falls back exactly once"
    );
    assert_eq!(
        delta.mailbox_turns, 2,
        "ask the presented cell's owner, then the true owner"
    );
    assert_eq!(
        delta.location_audits, 1,
        "a forwarded accept is audited exactly like a fast-path one"
    );
    assert_eq!(delta.location_mismatches, 0);
    assert_eq!(delta.location_audit_errors, 0);

    rt.close().await.unwrap();
}
