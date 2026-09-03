//! #862 acceptance box 2, joined end to end over one FoundationDB cluster.
//!
//! > *An account crossing `C` produces an invalidation a coordinator consumes,
//! > and the account's open session ends or is refused at the next `Hello`.*
//!
//! Every link of that chain shipped separately, and every link was covered
//! separately — which is the shape the box's sibling (box 1) explicitly
//! refuses to accept, and it was still what the tree had:
//!
//! | link | deployed at | covered by |
//! |---|---|---|
//! | a verdict files `ya` + a `yd` notice | `bin/persistd.rs` (`FdbStrikeLedger`) | `orrery_identity::fdb` tests |
//! | the notice becomes a `dc` entry | `bin/orrery-identity.rs` (`StandingFilingReactor`) | `filing.rs` tests, **all in memory** |
//! | `dc` becomes an `AccountInvalidation` | `orrery-coordinator.rs` (`IdentityStandingFeed`) | `standing_feed.rs` tests, **`MemAccountStore`** |
//! | the invalidation ends a session | `server.rs:1585` / refuses at `:1386` | `coordinator_server.rs`, **`MutableStandingFeed`** |
//!
//! Nothing read what the previous link wrote. A drift in the `yd` key layout,
//! in the `dc` value encoding, or in the instant the reactor stamps would have
//! surfaced in a deployment as a session that simply never ended — the exact
//! failure D33 clause (e) exists to prevent, and a silent one.
//!
//! This file is the join. The real executor-side ledger files against the real
//! identity store's bindings; the real reactor drains the real durable queue;
//! the real adapter polls the real `dc` family; and a real `CoordinatorServer`
//! over real iroh loopback endpoints ends a real session and refuses the token
//! that held it. The only fixture is a counting decorator around the shipped
//! feed, and it is there to *synchronize* on the coordinator's 1 s sweep, not
//! to stand in for anything: `PollCountingFeed` delegates every call to
//! `IdentityStandingFeed`, which is the type `orrery-coordinator.rs:154`
//! builds.
//!
//! # Why this file is in `orrery_coordinator`
//!
//! It is the only crate that may name all three. `orrery_identity` depends on
//! `orrery_persistd`, and the coordinator depends on both — so the chain can
//! only be assembled at its downstream end. That is the same asymmetry
//! `docs/spikes/862-gateway-consumer-dependency-cycle.md` records for the
//! gateway consumer, seen from the side where it costs nothing.
//!
//! # Running it
//!
//! Behind `standing-feed` + `fdb-state`, and self-skipping with a `skipping:`
//! line when no cluster is reachable — `scripts/check.sh`'s test lane runs
//! neither feature, so what guards this file per commit is the clippy lane's
//! `-p orrery_coordinator --features fdb-state,standing-feed --all-targets`.
//!
//! ```text
//! ORRERY_FDB_CLUSTER_FILE=.fdb-dev/fdb.cluster \
//!   cargo test -p orrery_coordinator --features standing-feed,fdb-state \
//!   --test standing_invalidation_chain -- --nocapture
//! ```

#![cfg(all(feature = "standing-feed", feature = "fdb-state"))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_coordinator::server::{FixedUnixClock, ServerConfig, SystemPresenceClock};
use orrery_coordinator::standing_feed::IdentityStandingFeed;
use orrery_coordinator::{
    CoordinatorClient, CoordinatorServer, FeedFailure, InterestIssuer, StandingInvalidationFeed,
    StrikesMode, StrikesPosture,
};
use orrery_identity::fdb::{FdbAccountStore, FdbFilingNoticeQueue, FdbStrikeRowSource};
use orrery_identity::filing::StandingFilingReactor;
use orrery_identity::standing::DEFAULT_STANDING_THRESHOLDS;
use orrery_identity::{AccountStore, BindOutcome, ComputedStanding};
use orrery_persistd::adjudication::{
    FdbStrikeLedger, OffenceTime, StrikeEvidenceRef, StrikeFileOutcome, StrikeKind, StrikeLedger,
    StrikeMode, StrikeRow, MAJOR_STRIKE_WEIGHT_MILLI, STRIKE_RETENTION_MS,
};
use orrery_persistd::gateway::{StrikesEnforcement, StrikesPosture as LedgerPosture};
use orrery_persistd::keyspace;
use orrery_protocol::{
    AccountId, AccountInvalidation, CellId, IssuerKey, IssuerKeyId, NodeId, PersistId, RulesetId,
    SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, Tick, UnixMillis,
};

/// The coordinator's wall clock, and the instant the reactor stamps the `dc`
/// entry with. The session token below is minted a second earlier, so the
/// watermark kills it.
const NOW_MS: u64 = 1_000_000;
const PATIENCE: Duration = Duration::from_secs(10);

/// Account ids are namespaced to this file (`0x0862_2000_…`), the discipline
/// `orrery_identity::fdb`'s suite states: these run against a shared
/// development cluster, and a collided id turns an unrelated agent's run into
/// a failure that reads like a bug in the mechanism.
fn account(slot: u64) -> AccountId {
    AccountId(0x0862_2000_0000_0000 | slot)
}

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[1] = 0x86;
    bytes[2] = 0x22;
    iroh::SecretKey::from_bytes(&bytes)
}

fn cell(x: i32) -> CellId {
    CellId::from_coords(glam::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).expect("cell in range")
}

/// A session token for `account`, bound to `node`, minted *before* the
/// crossing this test causes.
fn token_for(issuer: &iroh::SecretKey, account: AccountId, node: NodeId) -> Vec<u8> {
    SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            account,
            node,
            UnixMillis::new(NOW_MS - 1_000),
            SessionTokenTtlMs::new(60_000),
            // Stamped `Good`, and truthfully so: at mint time the account had
            // not crossed. That is precisely why clause (e) needs a feed —
            // the token cannot be re-judged, only invalidated.
            SessionStanding::Good,
            IssuerKeyId::new(1),
            false,
        ),
        issuer,
    )
    .expect("sign token")
    .encode()
    .expect("encode token")
}

/// One D33 clause (a) major finding, distinguished by `seed` so the ledger's
/// evidence-digest dedup treats two calls as two facts.
fn major_row(issued_at_ms: u64, seed: u8, mode: StrikeMode) -> StrikeRow {
    StrikeRow {
        issued_at_ms,
        weight_milli: MAJOR_STRIKE_WEIGHT_MILLI,
        kind: StrikeKind::Deviation,
        evidence_ref: StrikeEvidenceRef {
            entity: PersistId::new(862),
            window_start: Tick::new(1),
            window_end: Tick::new(2),
            digest: [seed; 32],
        },
        ruleset: RulesetId {
            version: 1,
            digest: [1; 32],
        },
        mode,
        expires_at_ms: issued_at_ms + STRIKE_RETENTION_MS,
    }
}

/// The shipped feed, plus a poll counter.
///
/// The coordinator consults its feed only on the 1 s maintenance sweep, never
/// on admission (D32 clause (c): no hot-path reads), so a test that publishes
/// and then asserts has to wait for a sweep. Counting polls is how
/// `coordinator_server.rs` waits, and this decorator is the same trick applied
/// to the *real* feed rather than to a stand-in for it — every
/// `invalidations()` below is `IdentityStandingFeed`'s, reading the `dc`
/// family out of FoundationDB.
struct PollCountingFeed {
    inner: IdentityStandingFeed<Arc<FdbAccountStore>>,
    polls: AtomicU64,
}

impl PollCountingFeed {
    fn over(store: Arc<FdbAccountStore>) -> Arc<Self> {
        Arc::new(Self {
            inner: IdentityStandingFeed::new(store),
            polls: AtomicU64::new(0),
        })
    }

    /// Block until the sweep has polled at least `minimum` times, which is
    /// when everything durable at the time of the previous poll is in the
    /// coordinator's consumer state and every open session has been re-checked
    /// against it.
    async fn wait_for_poll(&self, minimum: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while self.polls.load(Ordering::SeqCst) < minimum {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the maintenance sweep never polled the durable standing feed"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[async_trait::async_trait]
impl StandingInvalidationFeed for PollCountingFeed {
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
        let seen = self.inner.invalidations().await;
        // Counted *after* the read, so `wait_for_poll(n + 1)` guarantees a
        // completed poll rather than one merely started. Counting first would
        // let the assertion race the read it is waiting for.
        self.polls.fetch_add(1, Ordering::SeqCst);
        seen
    }
}

/// Leave the cluster as it was found. The `d` rows are identity's, the `ya`
/// range and the `yd` notice are the executor's, and this suite writes through
/// both — so it clears both. A leftover `dc` row is the dangerous one: it
/// would publish an invalidation for an account the next run believes it just
/// created clean.
async fn wipe(db: &Arc<foundationdb::Database>, account: AccountId, node: &NodeId) {
    let node = *node;
    let strikes_start = keyspace::strike_account_range_start(account);
    let strikes_end = keyspace::strike_account_range_end(account);
    let history_start = keyspace::binding_history_node_range_start(&node);
    let history_end = keyspace::binding_history_node_range_end(&node);
    let _ = db
        .run(|trx, _| {
            let strikes_start = strikes_start.clone();
            let strikes_end = strikes_end.clone();
            let history_start = history_start.clone();
            let history_end = history_end.clone();
            async move {
                trx.clear(&keyspace::account_key(account));
                trx.clear(&keyspace::binding_window_key(account));
                trx.clear(&keyspace::cooldown_entry_key(account));
                trx.clear(&keyspace::filing_notice_key(account));
                trx.clear(&keyspace::binding_key(&node));
                trx.clear_range(&history_start, &history_end);
                trx.clear_range(&strikes_start, &strikes_end);
                Ok(())
            }
        })
        .await;
}

/// The three durable halves the deployed binaries build, over one handle.
struct Chain {
    db: Arc<foundationdb::Database>,
    store: Arc<FdbAccountStore>,
}

impl Chain {
    /// `None` with a `skipping:` line when no cluster is reachable — right for
    /// a developer's `cargo test`, and a trap for CI, which is why
    /// `scripts/fdb-tests.sh` fails on that line rather than on the exit status.
    fn open() -> Option<Self> {
        let cluster = orrery_persistd::fdb::discover_cluster_file()?;
        let context = orrery_persistd::fdb::FdbContext::connect(&cluster)
            .expect("connect to the development cluster");
        let db = context.database();
        Some(Self {
            store: Arc::new(FdbAccountStore::from_database(Arc::clone(&db))),
            db,
        })
    }

    /// Bind `node` to `account` through identity's own writer, then file
    /// `rows` through the executor's own writer — the two production paths,
    /// in the order a deployment runs them.
    async fn file(&self, account: AccountId, node: &NodeId, at_ms: u64, rows: &[StrikeRow]) {
        self.store
            .create_account(account, at_ms)
            .await
            .expect("create the account");
        assert_eq!(
            self.store.bind(account, node, at_ms).await.expect("bind"),
            BindOutcome::Bound
        );

        let ledger = FdbStrikeLedger::from_database(Arc::clone(&self.db));
        for row in rows {
            assert_eq!(
                ledger
                    .file(*node, OffenceTime::KnownMs(at_ms + 1), row, None)
                    .expect("the executor files against the resolved binding"),
                StrikeFileOutcome::Filed { account },
                "attribution must resolve through the `db`/`dh` rows identity wrote"
            );
        }
    }

    /// The reactor `orrery-identity.rs:192` builds, at the posture the caller
    /// names, swept once.
    async fn sweep(&self, mode: StrikesEnforcement) -> orrery_identity::filing::FilingSweep {
        let scorer = ComputedStanding::new(
            FdbStrikeRowSource::from_database(Arc::clone(&self.db)),
            || NOW_MS,
            DEFAULT_STANDING_THRESHOLDS,
        )
        .expect("the default policy package is coherent");
        StandingFilingReactor::new(
            Arc::clone(&self.store),
            FdbFilingNoticeQueue::from_database(Arc::clone(&self.db)),
            scorer,
            LedgerPosture::new(mode),
        )
        .sweep()
        .await
        .expect("read the filing queue")
    }

    /// A coordinator whose feed is identity's durable `dc` family.
    async fn coordinator(
        &self,
        issuer: &iroh::SecretKey,
        interest: &iroh::SecretKey,
        feed: Arc<PollCountingFeed>,
        posture: StrikesMode,
    ) -> CoordinatorServer {
        CoordinatorServer::spawn(ServerConfig {
            token_clock: Arc::new(FixedUnixClock(NOW_MS)),
            presence_clock: Arc::new(SystemPresenceClock::default()),
            standing_feed: Some(feed),
            strikes_posture: StrikesPosture::new(posture),
            ..ServerConfig::new(
                [IssuerKey::new(IssuerKeyId::new(1), issuer.public())],
                InterestIssuer::new(interest.clone(), IssuerKeyId::new(1)),
            )
        })
        .await
        .expect("spawn coordinator")
    }
}

/// These cases run one at a time, and that is a property of what they drive
/// rather than a convenience.
///
/// Both halves of the chain read *fleet-wide* families by design:
/// `FilingNoticeQueue::pending` is one range read over the whole `yd` family —
/// keyed by account precisely so its cardinality is "accounts awaiting
/// evaluation" — and `AccountStore::cooldown_entries` is the same shape over
/// `dc`, because the invalidation feed's contract is "every account currently
/// refused", in full, each poll. Neither can be narrowed to one account
/// without changing what the production code does.
///
/// So a per-account fixture is not isolation here: two concurrent cases see
/// each other's notices in their own sweep counts. Namespaced account ids stop
/// them *colliding*; this stops them *observing* each other. It does not cover
/// a second process against the same development cluster, which is the same
/// exposure `orrery_identity::fdb`'s suite carries and the reason both
/// namespace their ids.
static ONE_FLEET_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

macro_rules! chain_test {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some(chain) = Chain::open() else {
                eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
                return;
            };
            let _fleet = ONE_FLEET_AT_A_TIME.lock().await;
            let body: fn(Chain) -> _ = $body;
            body(chain).await;
        }
    };
}

// -------------------------------------------------------------------------
// The chain, whole.

chain_test!(
    a_verdict_ends_the_open_session_it_finds_and_the_token_cannot_return,
    |chain| async move {
        let account = account(0x01);
        let peer = secret(1);
        let node: NodeId = peer.public();
        let issuer = secret(210);
        let interest = secret(211);
        wipe(&chain.db, account, &node).await;

        let feed = PollCountingFeed::over(Arc::clone(&chain.store));
        let server = chain
            .coordinator(&issuer, &interest, Arc::clone(&feed), StrikesMode::Live)
            .await;

        // The session exists first, and is healthy: nothing has been filed, so
        // the feed reads an empty `dc` family and the peer plays.
        let session = token_for(&issuer, account, node);
        let connected =
            CoordinatorClient::connect(peer.clone(), server.addr(), session.clone(), PATIENCE)
                .await
                .expect("an unblemished account is admitted");
        connected.report_presence(vec![cell(0)]).expect("presence");
        connected.next_grant(PATIENCE).await.expect("grant");
        assert_eq!(
            server.stats().await.connected_peers,
            1,
            "the session is open before the verdict"
        );

        // The verdict. Two majors is 6.0 — over `C` (5.0), under the ban band
        // (7.0) — so this is a *cooldown* crossing, which is the one clause (e)
        // publishes.
        let filed_at = NOW_MS - 60_000;
        chain
            .file(
                account,
                &node,
                filed_at,
                &[
                    major_row(filed_at + 1, 1, StrikeMode::Live),
                    major_row(filed_at + 1, 2, StrikeMode::Live),
                ],
            )
            .await;

        // The account has crossed and has *not* logged in again — which is the
        // whole gap #958 closed. The mint path would never run for it.
        let sweep = chain.sweep(StrikesEnforcement::Live).await;
        assert_eq!(
            (sweep.seen, sweep.published, sweep.cleared),
            (1, 1, 1),
            "the executor's `yd` notice is drained into a `dc` entry: {sweep:?}"
        );
        let entry = chain
            .store
            .cooldown_entry(account)
            .await
            .expect("read the entry")
            .expect("the crossing left a durable `dc` row");
        assert_eq!(
            entry.entered_at_ms, NOW_MS,
            "the watermark is the refusal instant, not the filing instant"
        );

        // One completed poll later the coordinator has consumed it, and the
        // session opened before the crossing is gone through the ordinary
        // disconnect path.
        let polled = feed.polls.load(Ordering::SeqCst);
        feed.wait_for_poll(polled + 2).await;
        let stats = server.stats().await;
        assert_eq!(
            stats.connected_peers, 0,
            "the invalidated session is gone: {stats:?}"
        );
        assert_eq!(
            stats.standing_sessions_terminated, 1,
            "and it was standing that ended it, not a transport fault"
        );

        // The second half of the box: the same still-cryptographically-valid
        // token is refused at the next `Hello`.
        assert!(
            CoordinatorClient::connect(peer, server.addr(), session, Duration::from_millis(750))
                .await
                .is_err(),
            "a pre-watermark token cannot walk back in"
        );
        assert!(
            server.stats().await.standing_hellos_refused >= 1,
            "the refusal is attributed to standing"
        );

        server.shutdown().await;
        wipe(&chain.db, account, &node).await;
    }
);

chain_test!(
    a_crossing_that_precedes_the_session_refuses_the_hello_outright,
    |chain| async move {
        let account = account(0x02);
        let peer = secret(2);
        let node: NodeId = peer.public();
        let issuer = secret(212);
        let interest = secret(213);
        wipe(&chain.db, account, &node).await;

        let filed_at = NOW_MS - 60_000;
        chain
            .file(
                account,
                &node,
                filed_at,
                &[
                    major_row(filed_at + 1, 3, StrikeMode::Live),
                    major_row(filed_at + 1, 4, StrikeMode::Live),
                ],
            )
            .await;
        assert_eq!(chain.sweep(StrikesEnforcement::Live).await.published, 1);

        let feed = PollCountingFeed::over(Arc::clone(&chain.store));
        let server = chain
            .coordinator(&issuer, &interest, Arc::clone(&feed), StrikesMode::Live)
            .await;
        // The consumer state is populated by the sweep, not by admission, so
        // wait for a completed poll before presenting the token.
        feed.wait_for_poll(1).await;

        assert!(
            CoordinatorClient::connect(
                peer,
                server.addr(),
                token_for(&issuer, account, node),
                Duration::from_millis(750)
            )
            .await
            .is_err(),
            "an account already over `C` is refused at `Hello`"
        );
        // `>= 1`, not `== 1`: `CoordinatorClient::connect` may make more than
        // one attempt inside its patience window, and each refused `Hello` is
        // counted. What is being asserted is *why* the connection failed, and
        // a second refusal is more of the same answer, not a different one.
        assert!(
            server.stats().await.standing_hellos_refused >= 1,
            "the refusal is attributed to standing"
        );

        server.shutdown().await;
        wipe(&chain.db, account, &node).await;
    }
);

// -------------------------------------------------------------------------
// The two postures that must change nothing, on the same durable chain.

chain_test!(
    a_shadow_stamped_verdict_reaches_no_coordinator,
    |chain| async move {
        let account = account(0x03);
        let peer = secret(3);
        let node: NodeId = peer.public();
        let issuer = secret(214);
        let interest = secret(215);
        wipe(&chain.db, account, &node).await;

        // Box 4 on the wired path, one link further than #956 took it: a
        // shadow-stamped row is filed durably and queues a real notice — the
        // ledger has no mode branch — and it is the *scorer* that finds
        // nothing. So a `Live` reactor evaluates and publishes nothing, and
        // the coordinator's feed stays empty.
        let filed_at = NOW_MS - 60_000;
        chain
            .file(
                account,
                &node,
                filed_at,
                &[
                    major_row(filed_at + 1, 5, StrikeMode::Shadow),
                    major_row(filed_at + 1, 6, StrikeMode::Shadow),
                ],
            )
            .await;

        let sweep = chain.sweep(StrikesEnforcement::Live).await;
        assert_eq!(
            (sweep.seen, sweep.evaluated, sweep.published),
            (1, 1, 0),
            "a shadow filing is evaluated and publishes nothing: {sweep:?}"
        );
        assert!(
            chain
                .store
                .cooldown_entry(account)
                .await
                .expect("read the entry")
                .is_none(),
            "a shadow verdict manufactures no `dc` row"
        );

        let feed = PollCountingFeed::over(Arc::clone(&chain.store));
        let server = chain
            .coordinator(&issuer, &interest, Arc::clone(&feed), StrikesMode::Live)
            .await;
        feed.wait_for_poll(1).await;
        let session = CoordinatorClient::connect(
            peer,
            server.addr(),
            token_for(&issuer, account, node),
            PATIENCE,
        )
        .await
        .expect("a shadow-stamped account is admitted");
        session.report_presence(vec![cell(1)]).expect("presence");
        session.next_grant(PATIENCE).await.expect("grant");

        let polled = feed.polls.load(Ordering::SeqCst);
        feed.wait_for_poll(polled + 2).await;
        let stats = server.stats().await;
        assert_eq!(stats.connected_peers, 1, "the session survives: {stats:?}");
        assert_eq!(stats.standing_sessions_terminated, 0);

        server.shutdown().await;
        wipe(&chain.db, account, &node).await;
    }
);

chain_test!(
    an_off_reactor_publishes_nothing_and_keeps_the_notice,
    |chain| {
        async move {
            let account = account(0x04);
            let peer = secret(4);
            let node: NodeId = peer.public();
            wipe(&chain.db, account, &node).await;

            let filed_at = NOW_MS - 60_000;
            chain
                .file(
                    account,
                    &node,
                    filed_at,
                    &[
                        major_row(filed_at + 1, 7, StrikeMode::Live),
                        major_row(filed_at + 1, 8, StrikeMode::Live),
                    ],
                )
                .await;

            // D32 clause (b): `Off` observes nothing. The notice is *kept*, so an
            // operator promoting C5 later still acts on everything filed while it
            // was off — which is what makes the dial safe to leave at its default.
            let off = chain.sweep(StrikesEnforcement::Off).await;
            assert_eq!(
                (off.seen, off.published, off.cleared),
                (0, 0, 0),
                "an off reactor does not even read the queue: {off:?}"
            );
            assert!(
                chain
                    .store
                    .cooldown_entry(account)
                    .await
                    .expect("read the entry")
                    .is_none(),
                "and writes no `dc` row for a coordinator to consume"
            );

            let promoted = chain.sweep(StrikesEnforcement::Live).await;
            assert_eq!(
                (promoted.seen, promoted.published),
                (1, 1),
                "the notice survived the off period: {promoted:?}"
            );

            wipe(&chain.db, account, &node).await;
        }
    }
);
