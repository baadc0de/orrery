//! A 0.1 % diagnostic sample must never drop a bulk write.
//!
//! The defect this file pins shipped in #86 and was found by re-reading
//! docs/14-capacity.md §11's own counters. `begin_location_audit` fires on
//! `Ok(FencedApply::Accepted(_))`, and `finish_location_audit` used to be
//! *awaited* inside `apply_fenced` — which the gateway runs inside
//! `within_route_budget(received_at, MAX_ROUTE_ADMISSION_WAIT_US, …)`, a
//! 25 ms `tokio::time::timeout` measured from the diff's arrival. A sampled
//! diff whose audit read overran the remaining budget therefore had its whole
//! route future *cancelled*: the diff was counted `shed_slow_route`, the
//! client got no ack, and the audit landed in neither `location_audits` nor
//! `location_audit_errors`.
//!
//! It was not theoretical and it was not rare. Over the 73 point directories
//! of the ssd-versus-memory study, both engines, three orders of magnitude of
//! shed rate, the identity
//!
//! ```text
//! shed_slow_route == (decided audits) - (completed audits)
//! ```
//!
//! held **exactly** at all 73 points — memory-r20k-r1 17/17, ssd-r200k-r1
//! 353/353, rigprobe-free 7244/7244 — and `location_audit_us_max` sat at
//! 20 771-26 526 us in every one of them, clamped on the 25 ms budget. Bulk
//! shed attributable to actual route slowness was zero in the entire study.
//!
//! Two claims, end to end through a real gateway, with the audit read made
//! slower than the whole route budget:
//!
//! 1. **The diff is routed and acknowledged anyway**, and `shed_slow_route`
//!    stays 0. This is the defect. With the audit awaited on the request path
//!    it fails at the first diff.
//! 2. **The audit is still accounted for.** Detaching a diagnostic is only an
//!    improvement if it cannot then vanish, so `location_audits_decided` must
//!    equal `location_audits + location_audit_errors +
//!    location_audits_dropped` once the samples have landed. A fix that made
//!    the audit invisible instead of slow would pass claim 1 and fail this.
//!
//! Its own test binary because `RouteStageMetrics` is process-global and the
//! sampler is pinned to one-in-one here.

mod lanes;
mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::{route_stage_metrics, settle_location_audits};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, LeaseMigrate, LeasePut, LeaseStore,
    LeaseStoreError, MemFenceStore, MemLeaseStore, Router, RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::{
    CellId, ClaimBasis, ClaimId, ClaimKind, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId,
    Lease, LeaseId, LeaseMsg, PersistId, RecordKind, Tick,
};

/// The route-admission budget for this gateway. Well under the shipped 25 ms
/// so the test does not sleep through it, and the assertion below is about
/// *which side of the budget the audit runs on*, not about how long either
/// number is.
/// The downstream route-admission budget this test runs the gateway under.
///
/// Deliberately generous — 200 ms, not the 10 ms it used to be. The value is
/// not a fidelity claim about production; it is headroom. Claim 1 below
/// asserts that a *healthy* router sheds nothing, which is only true while the
/// router actually beats this budget. At 10 ms a contended CI runner made a
/// healthy gateway shed for real, and the assertion then fired with
/// `shed_slow_route: 6` and a message accusing the code of a correctness bug —
/// the #370 species again, one line further down than where #368 found it.
///
/// Raising it costs wall clock (the two observation sleeps below are four
/// budgets each) and buys an assertion that means what it says. `SLOW_LOCATE`
/// is derived from it, so the audit stays six budgets slow at any value.
const ROUTE_BUDGET_US: u64 = 200_000;

/// A liveness ceiling. The served-versus-shed assertion is the reply ordering
/// below; this merely prevents a permanently stalled test from holding a
/// worker forever.
///
/// It used to be written as `ROUTE_BUDGET_US * 3_000` with a doc comment
/// saying that came to 30 seconds. It did, at the 10 ms budget in force when
/// it was written; the budget is now 200 ms and the same expression is **600
/// seconds**, so a hang burned ten minutes of a runner and the comment beside
/// it was false. A ceiling that measures nothing the test measures should not
/// be derived from a budget the test does tune — the derivation only invited
/// that drift. It is the shared ceiling now, and the one premise that really
/// is load-bearing is pinned below instead.
const ACK_LIVENESS_TIMEOUT: Duration = lanes::LIVENESS_CEILING;

/// The longest the *served* path can legitimately take: `within_route_budget`
/// cancels any route that outruns its own budget, so `DIFFS` routes that are
/// each served cannot together exceed this.
const SERVED_PATH_CEILING_US: u64 = ROUTE_BUDGET_US * DIFFS;

// A ceiling inside the served path's own bound would cut off a correct run and
// report it as exactly the shed this test exists to catch — the inverted form
// of the same misdiagnosis. Make that a compile error rather than a comment.
const _: () = assert!(
    ACK_LIVENESS_TIMEOUT.as_micros() > SERVED_PATH_CEILING_US as u128,
    "the ack ceiling must outlast every route this test serves, or a healthy \
     run times out and reads as the shed the test is looking for"
);

/// How long the audit's `LeaseStore::locate` takes once armed: six budgets.
/// Any single sampled audit on the request path therefore overruns, with no
/// dependence on scheduling luck.
///
/// Derived from the budget rather than written as a literal. It was
/// `from_millis(60)` — six times the then-10 ms budget by arithmetic the
/// compiler could not see, so raising the budget would have silently made the
/// "slow" audit fast and the test would have proven nothing while staying
/// green.
const SLOW_LOCATE_US: u64 = ROUTE_BUDGET_US * 6;
const SLOW_LOCATE: Duration = Duration::from_micros(SLOW_LOCATE_US);

// The test's whole premise is that this audit *overruns* the route budget. If
// it ever stopped doing so the test would keep passing and prove nothing: a
// fast audit sheds nothing, which is exactly the outcome claim 1 asserts. That
// is not hypothetical — pinning SLOW_LOCATE back to its old `from_millis(60)`
// literal while the budget is 200 ms leaves the suite green. Make the premise
// a compile error instead of a comment.
const _: () = assert!(
    SLOW_LOCATE_US > ROUTE_BUDGET_US,
    "the armed audit must outlast the route budget or this test asserts nothing"
);

/// Diffs written under the fence. At one-in-one sampling every one of them is
/// audited, so with the audit on the request path every one of them is shed.
const DIFFS: u64 = 6;

const ENTITY: PersistId = PersistId::new(4_242);

/// A `MemLeaseStore` whose `locate` can be made slow, on demand.
///
/// Armed only after the lease has been claimed: `claim_lease` reads and writes
/// the same store, and a store that was slow from the start would be testing
/// the claim path instead of the audit.
struct SlowLocateStore {
    inner: MemLeaseStore,
    slow: AtomicBool,
    locates: AtomicUsize,
}

impl SlowLocateStore {
    fn new() -> Self {
        Self {
            inner: MemLeaseStore::new(),
            slow: AtomicBool::new(false),
            locates: AtomicUsize::new(0),
        }
    }
    fn go_slow(&self) {
        self.slow.store(true, Ordering::SeqCst);
    }
    fn locates(&self) -> usize {
        self.locates.load(Ordering::SeqCst)
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
        self.locates.fetch_add(1, Ordering::SeqCst);
        if self.slow.load(Ordering::SeqCst) {
            tokio::time::sleep(SLOW_LOCATE).await;
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

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_audit_slower_than_the_route_budget_neither_sheds_the_diff_nor_vanishes() {
    // Audit every accept, so the claim under test is exercised by every diff
    // rather than by one in a thousand of them.
    std::env::set_var("ORRERY_FENCED_LOCATION_AUDIT_N", "1");

    let peer = support::secret(1);
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(SlowLocateStore::new());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let runtime = Arc::new(
        CellRuntime::open_with_lease_store(
            &runtime_config(dir.path()),
            &checkpoints,
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .expect("open runtime"),
    );

    // A player-basis claim is only plausible for an entity the cluster has
    // already committed (D7 §4.2), so the fence needs this first.
    runtime
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("actor for the hosted root cell")
        .start_diff(orrery_protocol::JournalRecord {
            lsn: orrery_protocol::Lsn::new(0, 0),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: ENTITY,
            tick: Tick::new(0),
            epoch: Epoch::new(0),
            author: support::node(9),
            kind: RecordKind::Spawn,
            crc: orrery_persistd::payload_crc(b"seeded"),
            payload: Bytes::from_static(b"seeded"),
        })
        .await
        .expect("seed append")
        .committed()
        .await
        .expect("seed commit");

    let config = GatewayConfig {
        route_admission_wait_us: ROUTE_BUDGET_US,
        ..support::authority_config(peer.public(), GridId::ROOT, vec![CellId::ROOT])
    };
    let router: Arc<dyn Router> = Arc::clone(&runtime) as Arc<dyn Router>;
    let server = GatewayServer::spawn(config, router)
        .await
        .expect("spawn gateway");
    let metrics = Arc::clone(server.metrics());

    let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(peer.clone())
        .bind()
        .await
        .expect("bind client endpoint");
    let conn = client
        .connect(server.addr(), GATEWAY_ALPN)
        .await
        .expect("connect to gateway");
    let mut admission = conn.accept_uni().await.expect("admission stream");
    assert_eq!(
        admission.read_to_end(16).await.expect("admission"),
        vec![0u8]
    );
    let conn = lanes::GatewayLanes::attach(conn);
    conn.send_control(&GatewayMsg::VersionedHello {
        token: support::valid_session_token(peer.public()),
        node: peer.public(),
        version: orrery_protocol::PROTOCOL_VERSION,
    })
    .await;
    lanes::expect_hello_ack(&conn).await;

    conn.send_control(&GatewayMsg::Lease {
        message: LeaseMsg::Claim {
            claim_id: ClaimId(1),
            entity: ENTITY,
            grid: GridId::ROOT,
            cell: CellId::ROOT,
            kind: ClaimKind::Weak,
            basis: ClaimBasis::Explicit,
            observed: Default::default(),
            tick: Tick::new(1),
        },
    })
    .await;
    let (lease_id, seq) = loop {
        match conn.next_reply(ACK_LIVENESS_TIMEOUT).await {
            Some(GatewayReply::Lease {
                message: LeaseMsg::Grant { lease_id, seq, .. },
            }) => break (lease_id, seq),
            Some(GatewayReply::Lease { message }) => panic!("claim was not granted: {message:?}"),
            Some(_) => continue,
            None => panic!(
                "timed out after {} s with no answer to the lease claim. A \
                 denial would have arrived as a Lease reply, so this is not \
                 evidence the claim was denied; a dropped claim and a loaded \
                 runner are both silence and this cannot separate them",
                ACK_LIVENESS_TIMEOUT.as_secs(),
            ),
        }
    };

    // Only now is `locate` expensive: everything above uses it too.
    store.go_slow();
    let locates_before = store.locates();
    let before = route_stage_metrics().snapshot();

    for tick in 2..2 + DIFFS {
        conn.send_state(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: ENTITY,
                tick: Tick::new(tick),
                kind: RecordKind::ComponentDiff,
                payload: Bytes::from_static(b"state"),
                seq: tick,
                lease_id: Some(lease_id),
                authority_seq: Some(seq),
            },
        });
    }

    // -- claim 1: the write is served, not shed ---------------------------
    // Let the route budget expire before looking at the valve. This is the
    // ordering evidence: an inline audit sheds these writes before it can
    // produce any reply, so fail on that invariant rather than later calling
    // its silence a diagnostic failure.
    tokio::time::sleep(Duration::from_micros(ROUTE_BUDGET_US * 4)).await;
    let ingress = metrics.ingress.snapshot();
    assert_eq!(
        ingress.shed_slow_route, 0,
        "nothing may be shed: the only slow thing here is a diagnostic that the \
         route does not wait for: {ingress:?}"
    );
    assert_eq!(
        (ingress.shed_stale, ingress.shed_saturated),
        (0, 0),
        "and neither ingress refusal is what this test is about: {ingress:?}"
    );

    // The audit read is six route budgets long and one fires per accept. With
    // it awaited inside `apply_fenced` the gateway's `within_route_budget`
    // timeout cancels every one of these routes, so no acknowledgement ever
    // arrives and this loop times out on the first diff.
    let mut acked = 0;
    while acked < DIFFS {
        match conn.next_reply(ACK_LIVENESS_TIMEOUT).await {
            Some(GatewayReply::BulkAck { .. }) => acked += 1,
            Some(GatewayReply::BulkNack { entity, tick, .. }) => {
                panic!("a live fence at its own cell must be admitted: {entity:?} {tick:?}")
            }
            Some(other) => panic!("unexpected reply while awaiting acks: {other:?}"),
            None => panic!(
                "timed out after {} s waiting for reply {}/{}. The no-shed \
                 invariant is the `shed_slow_route == 0` assertion above, not \
                 this wait — a shed would already have failed there — so this \
                 arm reports silence, which a loaded runner produces too",
                ACK_LIVENESS_TIMEOUT.as_secs(),
                acked + 1,
                DIFFS,
            ),
        }
    }

    // A second, post-ack observation catches a late shed rather than one that
    // happened before an acknowledgement reached this client.
    tokio::time::sleep(Duration::from_micros(ROUTE_BUDGET_US * 4)).await;
    let ingress = metrics.ingress.snapshot();
    assert_eq!(
        ingress.shed_slow_route, 0,
        "nothing may be shed: the only slow thing here is a diagnostic that the \
         route does not wait for: {ingress:?}"
    );
    assert_eq!(
        (ingress.shed_stale, ingress.shed_saturated),
        (0, 0),
        "and neither ingress refusal is what this test is about: {ingress:?}"
    );

    // -- claim 2: every sample the sampler decided on is still visible -----
    assert!(
        settle_location_audits(Duration::from_secs(30)).await,
        "the decided audits never landed in a counter: {:?}",
        route_stage_metrics().snapshot().delta(before)
    );
    let delta = route_stage_metrics().snapshot().delta(before);
    assert_eq!(
        delta.applies, DIFFS,
        "every diff took the fenced route: {delta:?}"
    );
    assert_eq!(
        delta.location_audits_decided, DIFFS,
        "at one-in-one the sampler decides on every accept: {delta:?}"
    );
    assert_eq!(
        delta.location_audits + delta.location_audit_errors + delta.location_audits_dropped,
        delta.location_audits_decided,
        "a decided audit that reaches no counter is exactly the failure this \
         change removes; detaching it must not make it invisible: {delta:?}"
    );
    assert_eq!(
        delta.location_audits, DIFFS,
        "the store answers, so every sample reaches a verdict: {delta:?}"
    );
    assert_eq!(delta.location_mismatches, 0, "J holds here: {delta:?}");
    assert_eq!(
        store.locates() - locates_before,
        DIFFS as usize,
        "one audit read per accept, and nothing else reads the store"
    );
    assert!(
        delta.location_audit_us_sum
            >= DIFFS * u64::try_from(SLOW_LOCATE.as_micros()).unwrap() * 8 / 10,
        "the audits' own stage still carries their cost: {delta:?}"
    );

    server.shutdown().await;
}
