//! D33 clause (e)'s cooldown and ban enforcement at the gateway wire surface
//! (issue #219).
//!
//! Identity refuses to mint tokens for cooled-down and banned accounts, but a
//! token minted *before* the standing crossed stays cryptographically valid
//! for up to an hour. What closes that gap here is the account-generation
//! invalidation identity publishes and this gateway consumes: open sessions
//! are terminated within one posture poll plus apply, reconnecting peers with
//! stale tokens get [`GatewayReply::HELLO_REFUSED_STANDING`] — a different
//! answer from a malformed token — and none of it happens until C5's posture
//! leaves `off`.
//!
//! The unit tests beside [`crate::gateway`] prove the watermark rule and the
//! teardown against the registry directly; these run the real accept loop,
//! its 1 s sweep arm, and a raw-iroh client speaking the aeronet wire shape.

mod lanes;
mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use iroh::RelayMode;
use orrery_persistd::gateway::{
    FeedFailure, GatewayClock, IdentityHealth, StandingInvalidationFeed, StrikesEnforcement,
    StrikesPosture,
};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router, RuntimeConfig,
    GATEWAY_ALPN,
};
use orrery_protocol::{
    AccountId, AccountInvalidation, CellId, GatewayMsg, GatewayReply, GridId, UnixMillis,
};

const INVALIDATED_AT_MS: u64 = 5_000;

/// An in-memory stand-in for identity's publication, whose entries a test can
/// change while sessions are open — which is exactly the moment this
/// machinery exists for.
///
/// The gateway consults this only on its 1 s sweep, never on admission (D32
/// clause (c): no hot-path reads), so a test synchronizes on
/// [`MutableFeed::wait_for_poll`] rather than on raw sleeps.
struct MutableFeed {
    entries: std::sync::Mutex<Vec<AccountInvalidation>>,
    polls: AtomicU64,
}

impl MutableFeed {
    fn serving(entries: Vec<AccountInvalidation>) -> Arc<Self> {
        Arc::new(Self {
            entries: std::sync::Mutex::new(entries),
            polls: AtomicU64::new(0),
        })
    }

    async fn publish(&self, entry: AccountInvalidation) {
        self.entries.lock().expect("feed lock").push(entry);
    }

    /// Retract everything, as a publisher does when a cooldown decays away
    /// or an appeal is upheld.
    async fn retract_all(&self) {
        self.entries.lock().expect("feed lock").clear();
    }

    /// Block until the gateway's sweep has polled at least `minimum` times,
    /// which is when anything published so far is in its consumer state.
    async fn wait_for_poll(&self, minimum: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.polls.load(Ordering::SeqCst) < minimum {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the maintenance sweep never polled the standing feed"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[async_trait::async_trait]
impl StandingInvalidationFeed for MutableFeed {
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self.entries.lock().expect("feed lock").clone())
    }
}

/// A Unix clock a test can move between phases.
struct AtomicClock(AtomicU64);

impl GatewayClock for AtomicClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

/// Identity reachability, switchable so a test can start the outage mid-run.
struct HealthSwitch(AtomicBool);

impl IdentityHealth for HealthSwitch {
    fn is_available(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn invalidated(account: AccountId, effective_from_ms: u64) -> AccountInvalidation {
    AccountInvalidation {
        account,
        effective_from_ms: UnixMillis::new(effective_from_ms),
    }
}

fn node(seed_byte: u8) -> orrery_protocol::NodeId {
    support::node(seed_byte)
}

async fn runtime(dir: &std::path::Path) -> Arc<tokio::sync::Mutex<CellRuntime>> {
    let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
        Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
    Arc::new(tokio::sync::Mutex::new(
        CellRuntime::open(
            &RuntimeConfig {
                shards: vec![CellId::ROOT],
                grid: GridId::ROOT,
                journal: JournalConfig {
                    dir: dir.to_path_buf(),
                    commit: GroupCommitConfig {
                        mode: AdaptiveCommitMode::AlwaysBatch,
                        batch_window: Duration::from_millis(100),
                        batch_max_records: 100_000,
                        batch_max_bytes: 1 << 20,
                    },
                },
                node_id: 0,
                epoch: orrery_protocol::Epoch::new(0),
                fence: Arc::new(MemFenceStore::new()),
            },
            &store,
        )
        .await
        .unwrap(),
    ))
}

fn standing_gateway_config(
    issuer: &iroh_base::SecretKey,
    clock: Arc<dyn GatewayClock>,
    health: Arc<dyn IdentityHealth>,
    feed: Arc<MutableFeed>,
    posture: StrikesEnforcement,
) -> GatewayConfig {
    GatewayConfig {
        authorizer: support::authorizer(issuer),
        identity_clock: clock,
        identity_health: health,
        standing_feed: Some(feed),
        strikes_posture: StrikesPosture::new(posture),
        ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
    }
}

/// One dialled-and-handshaken client, plus the NodeId it authenticated as.
struct Client {
    _endpoint: iroh::Endpoint,
    conn: lanes::GatewayLanes,
    node: orrery_protocol::NodeId,
}

/// Dial `server` and read the admission byte, but do not send a `Hello`.
async fn connect(server: &GatewayServer, seed_byte: u8) -> Client {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(iroh_base::SecretKey::from_bytes(&[seed_byte; 32]))
        .bind()
        .await
        .unwrap();
    let node = endpoint.id();
    let conn = endpoint.connect(server.addr(), GATEWAY_ALPN).await.unwrap();
    // Admission: the gateway streams [ACCEPTED] (byte 0) on a uni stream.
    // Read it before attaching, or the lane reader consumes it.
    let mut admission_stream = conn.accept_uni().await.unwrap();
    let accepted = admission_stream.read_to_end(16).await.unwrap();
    assert_eq!(accepted, vec![0u8]);
    Client {
        _endpoint: endpoint,
        conn: lanes::GatewayLanes::attach(conn),
        node,
    }
}

/// The liveness ceiling every `hello()` wait runs under. Named so an arm
/// that reports a timeout can state how long it actually waited, and shared
/// with the rest of the suite so it cannot drift on its own. No call site
/// below expects `hello()` to return `None`, so raising it costs a passing
/// run nothing.
const HELLO_LIVENESS_TIMEOUT: Duration = lanes::LIVENESS_CEILING;

async fn hello(client: &Client, token: Vec<u8>) -> Option<GatewayReply> {
    client
        .conn
        .send_control(&GatewayMsg::VersionedHello {
            token,
            node: client.node,
            version: orrery_protocol::PROTOCOL_VERSION,
        })
        .await;
    client.conn.next_reply(HELLO_LIVENESS_TIMEOUT).await
}

/// A gateway running the real accept loop over an empty journal runtime.
async fn spawn_gateway(config: GatewayConfig) -> GatewayServer {
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(dir.path()).await;
    let router: Arc<dyn Router> = Arc::clone(&runtime) as Arc<dyn Router>;
    let server = GatewayServer::spawn(config, router).await.unwrap();
    // The journal directory and the runtime behind the router stay alive for
    // as long as the caller holds the server.
    tokio::spawn(async move {
        let (_runtime, _dir) = (runtime, dir);
        std::future::pending::<()>().await;
    });
    server
}

/// The refusal #219 asks to be able to tell apart from a malformed token.
#[tokio::test]
async fn a_cooldown_or_ban_refusal_arrives_as_hello_refused_standing_not_silence() {
    let issuer = support::issuer();
    let feed = MutableFeed::serving(vec![invalidated(AccountId::new(7), INVALIDATED_AT_MS)]);
    let server = spawn_gateway(standing_gateway_config(
        &issuer,
        Arc::new(AtomicClock(AtomicU64::new(support::TOKEN_NOW_MS))),
        Arc::new(HealthSwitch(AtomicBool::new(true))),
        Arc::clone(&feed),
        StrikesEnforcement::Live,
    ))
    .await;

    let client = connect(&server, 21).await;
    feed.wait_for_poll(1).await;
    let token = support::valid_session_token(client.node);
    match hello(&client, token.clone()).await {
        Some(GatewayReply::HelloRefused { reason, .. }) => {
            assert_eq!(
                reason,
                GatewayReply::HELLO_REFUSED_STANDING,
                "the client learns its account may not hold a session, not that its bytes were bad"
            );
        }
        other => panic!("expected a standing refusal, got {other:?}"),
    }

    // Retraction is not a pardon. A poll that comes back without the entry
    // — or empty, or from a compromised publisher — never un-applies what
    // was applied: recovery runs through minting, and this token predates
    // the watermark until its own signed TTL kills it. (The fresh-mint path
    // is `a_token_minted_after_the_watermark_is_admitted_again`.)
    feed.retract_all().await;
    feed.wait_for_poll(feed.polls.load(Ordering::SeqCst) + 1)
        .await;
    match hello(&client, token).await {
        Some(GatewayReply::HelloRefused {
            reason: GatewayReply::HELLO_REFUSED_STANDING,
            ..
        }) => {}
        Some(other) => panic!("expected a standing refusal, got {other:?}"),
        None => panic!(
            "timed out after {} s with no reply to the hello at all. An \
             admission would have arrived as a HelloAck, so this is not \
             evidence that the gateway admitted an account it should have \
             refused; it is silence, which a loaded runner also produces",
            HELLO_LIVENESS_TIMEOUT.as_secs(),
        ),
    }
}

/// The termination half, through the real maintenance loop: a session opened
/// before the crossing dies within one poll plus apply, and its dead token
/// cannot walk back in.
#[tokio::test]
async fn an_invalidation_published_mid_session_terminates_the_open_session() {
    let issuer = support::issuer();
    let feed = MutableFeed::serving(vec![]);
    let server = spawn_gateway(standing_gateway_config(
        &issuer,
        Arc::new(AtomicClock(AtomicU64::new(support::TOKEN_NOW_MS))),
        Arc::new(HealthSwitch(AtomicBool::new(true))),
        Arc::clone(&feed),
        StrikesEnforcement::Live,
    ))
    .await;

    let client = connect(&server, 22).await;
    let token = support::valid_session_token(client.node);
    match hello(&client, token.clone()).await {
        Some(GatewayReply::HelloAck { .. }) => {}
        Some(other) => panic!("the session must establish before it can be terminated: {other:?}"),
        None => panic!(
            "timed out after {} s with no reply to the hello at all. A refusal \
             would have arrived as a HelloRefused, so this is not evidence \
             that the gateway refused a session it had no reason to refuse; \
             it is silence, which a loaded runner also produces",
            HELLO_LIVENESS_TIMEOUT.as_secs(),
        ),
    }

    feed.publish(invalidated(AccountId::new(7), INVALIDATED_AT_MS))
        .await;
    // D32 clause (c)'s bound: one 1 s poll interval plus apply, under 2 s.
    tokio::time::sleep(Duration::from_millis(1_700)).await;
    assert_eq!(
        server.standing_metrics().snapshot().sessions_terminated,
        1,
        "the open session was torn down by the sweep"
    );

    // And the token that predates the watermark cannot re-establish.
    match hello(&client, token).await {
        Some(GatewayReply::HelloRefused {
            reason: GatewayReply::HELLO_REFUSED_STANDING,
            ..
        }) => {}
        Some(other) => panic!("expected a standing refusal, got {other:?}"),
        None => panic!(
            "timed out after {} s with no reply to the hello at all. An \
             admission would have arrived as a HelloAck, so this is not \
             evidence that the gateway admitted an account it should have \
             refused; it is silence, which a loaded runner also produces",
            HELLO_LIVENESS_TIMEOUT.as_secs(),
        ),
    }
}

/// docs/09 §8's grace rule must not become a ban escape hatch: an
/// invalidation published before the outage outlives it, and the expired
/// token that would have been graced in (#227) is refused with the standing
/// reason instead.
#[tokio::test]
async fn grace_does_not_admit_an_account_identity_has_invalidated() {
    let issuer = support::issuer();
    let feed = MutableFeed::serving(vec![]);
    let health = Arc::new(HealthSwitch(AtomicBool::new(true)));
    let clock = Arc::new(AtomicClock(AtomicU64::new(support::TOKEN_NOW_MS)));
    let server = spawn_gateway(standing_gateway_config(
        &issuer,
        Arc::clone(&clock) as Arc<dyn GatewayClock>,
        Arc::clone(&health) as Arc<dyn IdentityHealth>,
        Arc::clone(&feed),
        StrikesEnforcement::Live,
    ))
    .await;

    let client = connect(&server, 23).await;
    let token = support::valid_session_token(client.node);
    match hello(&client, token.clone()).await {
        Some(GatewayReply::HelloAck { .. }) => {}
        Some(other) => {
            panic!("established normally before any of this started, got {other:?}")
        }
        None => panic!(
            "timed out after {} s with no reply to the hello at all. A refusal \
             would have arrived as a HelloRefused, so this is not evidence \
             that the gateway refused a session it had no reason to refuse; \
             it is silence, which a loaded runner also produces",
            HELLO_LIVENESS_TIMEOUT.as_secs(),
        ),
    }

    feed.publish(invalidated(AccountId::new(7), INVALIDATED_AT_MS))
        .await;
    // The refusal rides the consumer state, which the sweep refreshes —
    // wait for a poll that has seen the entry, then expire into the outage.
    let polled = feed.polls.load(Ordering::SeqCst);
    feed.wait_for_poll(polled + 1).await;
    health.0.store(false, Ordering::SeqCst);
    // The token expired (900 + 60_000 < 61_500) while identity is down.
    clock.0.store(61_500, Ordering::SeqCst);
    match hello(&client, token).await {
        Some(GatewayReply::HelloRefused {
            reason: GatewayReply::HELLO_REFUSED_STANDING,
            ..
        }) => {}
        Some(other) => panic!("expected a standing refusal, got {other:?}"),
        None => panic!(
            "timed out after {} s with no reply to the hello at all. An \
             admission would have arrived as a HelloAck, so this is not \
             evidence that the gateway admitted an account it should have \
             refused; it is silence, which a loaded runner also produces",
            HELLO_LIVENESS_TIMEOUT.as_secs(),
        ),
    }
    assert_eq!(
        server.standing_metrics().snapshot().hello_refused_standing,
        1
    );
}

/// D17.3's requirement, at the wire: with C5 still observing, the invalidated
/// account is admitted and the counter moves — a control that cannot be
/// observed before it acts cannot be promoted.
#[tokio::test]
async fn in_shadow_the_invalidated_account_is_still_admitted_and_counted() {
    let issuer = support::issuer();
    let feed = MutableFeed::serving(vec![invalidated(AccountId::new(7), INVALIDATED_AT_MS)]);
    let server = spawn_gateway(standing_gateway_config(
        &issuer,
        Arc::new(AtomicClock(AtomicU64::new(support::TOKEN_NOW_MS))),
        Arc::new(HealthSwitch(AtomicBool::new(true))),
        Arc::clone(&feed),
        StrikesEnforcement::Shadow,
    ))
    .await;

    let client = connect(&server, 24).await;
    feed.wait_for_poll(1).await;
    match hello(&client, support::valid_session_token(client.node)).await {
        Some(GatewayReply::HelloAck { .. }) => {}
        Some(other) => panic!("shadow mode did not admit the invalidated account: {other:?}"),
        None => panic!(
            "timed out after {} s with no reply to the hello at all. A refusal \
             would have arrived as a HelloRefused, so this is not evidence \
             that shadow enforcement refused the account; it is silence, which \
             a loaded runner also produces",
            HELLO_LIVENESS_TIMEOUT.as_secs(),
        ),
    }
    let snapshot = server.standing_metrics().snapshot();
    assert_eq!(snapshot.shadow_hello_would_refuse, 1);
    assert_eq!(snapshot.hello_refused_standing, 0);
}

/// The deployed feed, against the real `dc` family (#862).
///
/// Everything above injects [`MutableFeed`], which proves the gateway's half of
/// D33 clause (e) and nothing about where the entries come from. These run
/// [`orrery_persistd::standing_feed::DcCooldownFeed`] — the type the `persistd`
/// binary now installs — over a live FoundationDB cluster, against the same
/// accept loop, sweep and raw-iroh client as the rest of the suite.
///
/// # Why the row is written as raw bytes and not through `orrery_identity`
///
/// It cannot be: `orrery_identity` depends on `orrery_persistd`, so no target
/// in this crate — test targets included — may name it
/// (`docs/spikes/862-gateway-consumer-dependency-cycle.md` carries the cargo
/// error). What keeps this honest is that the writer below builds its key with
/// [`orrery_persistd::keyspace::cooldown_entry_key`], which since #862 is also
/// the function `orrery_identity::fdb`'s real `observe_cooldown` calls. There is
/// one definition of these bytes, so a test writing them writes identity's
/// layout by construction rather than by a comment promising it does.
#[cfg(feature = "fdb")]
mod dc_feed {
    use super::{
        connect, hello, node, spawn_gateway, AtomicClock, HealthSwitch, HELLO_LIVENESS_TIMEOUT,
        INVALIDATED_AT_MS,
    };
    use orrery_persistd::gateway::{
        FeedFailure, SharedStandingInvalidationFeed, StandingInvalidationFeed, StrikesEnforcement,
        StrikesPosture,
    };
    use orrery_persistd::keyspace;
    use orrery_persistd::standing_feed::DcCooldownFeed;
    use orrery_persistd::{GatewayConfig, GatewayServer};
    use orrery_protocol::{AccountId, AccountInvalidation, CellId, GatewayReply, GridId};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// The gateway's maintenance arm ticks once a second; two ticks is the
    /// window in which a poll must have happened, and the window in which an
    /// `Off` gateway must still not have polled.
    const TWO_SWEEPS: Duration = Duration::from_millis(2_500);

    /// An account id no other lane uses.
    ///
    /// The dev cluster at 127.0.0.1:4500 is shared, and the `dc` family is
    /// process-global: a fixed id would let this suite and a sibling's identity
    /// suite write each other's rows. `0x0862_0005` is this issue's band, and
    /// the low half is the pid and a counter so two runs of this binary — or
    /// two arms of it in parallel — never meet either.
    fn unique_account() -> AccountId {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        AccountId::new(
            0x0862_0005_0000_0000
                | (u64::from(std::process::id()) << 16)
                | NEXT.fetch_add(1, Ordering::Relaxed),
        )
    }

    /// The real feed, plus a poll counter.
    ///
    /// The counter is the only thing this adds: every `invalidations` call is
    /// delegated to [`DcCooldownFeed`], so what the gateway reads is what the
    /// binary reads. It exists because "`Off` reads nothing" is a claim about
    /// the *absence* of a read, which no metric on the gateway can witness —
    /// D32 clause (b) says an `Off` control does not even poll, and only the
    /// feed itself can say whether it was asked.
    struct CountingDcFeed {
        inner: DcCooldownFeed,
        polls: AtomicU64,
    }

    #[async_trait::async_trait]
    impl StandingInvalidationFeed for CountingDcFeed {
        async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.inner.invalidations().await
        }
    }

    impl CountingDcFeed {
        fn polls(&self) -> u64 {
            self.polls.load(Ordering::SeqCst)
        }

        async fn wait_for_poll(&self, minimum: u64) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while self.polls() < minimum {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the maintenance sweep never polled the dc feed"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    /// A cluster handle, or `None` when this box has no dev cluster — the same
    /// self-skip every other FDB-gated suite here uses.
    fn context() -> Option<orrery_persistd::FdbContext> {
        let cluster_file = super::support::fdb_cluster_file()?;
        Some(orrery_persistd::FdbContext::connect(&cluster_file).expect("connect to dev cluster"))
    }

    /// Write one `dc` row exactly as identity's `observe_cooldown` does:
    /// `keyspace::cooldown_entry_key(account)` -> `entered_at_ms:u64-be`.
    async fn write_cooldown_entry(
        db: &foundationdb::Database,
        account: AccountId,
        entered_at_ms: u64,
    ) {
        db.run(|trx, _| async move {
            trx.set(
                &keyspace::cooldown_entry_key(account),
                &entered_at_ms.to_be_bytes(),
            );
            Ok(())
        })
        .await
        .expect("write the dc cooldown entry");
    }

    /// Leave the shared cluster as it was found.
    async fn clear_cooldown_entry(db: &foundationdb::Database, account: AccountId) {
        db.run(|trx, _| async move {
            trx.clear(&keyspace::cooldown_entry_key(account));
            Ok(())
        })
        .await
        .expect("clear the dc cooldown entry");
    }

    /// A gateway whose standing feed is the deployed one, at `posture`.
    fn config(
        issuer: &iroh_base::SecretKey,
        feed: Arc<CountingDcFeed>,
        posture: StrikesEnforcement,
    ) -> GatewayConfig {
        GatewayConfig {
            authorizer: super::support::authorizer(issuer),
            identity_clock: Arc::new(AtomicClock(AtomicU64::new(super::support::TOKEN_NOW_MS))),
            identity_health: Arc::new(HealthSwitch(AtomicBool::new(true))),
            standing_feed: Some(feed as SharedStandingInvalidationFeed),
            strikes_posture: StrikesPosture::new(posture),
            ..super::support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
        }
    }

    /// One arm: write the row, run a gateway at `posture`, dial and `Hello`.
    async fn run_arm(
        posture: StrikesEnforcement,
        seed_byte: u8,
    ) -> Option<(Arc<CountingDcFeed>, GatewayServer, Option<GatewayReply>)> {
        let context = context()?;
        let db = context.database();
        let account = unique_account();
        write_cooldown_entry(&db, account, INVALIDATED_AT_MS).await;

        let issuer = super::support::issuer();
        let feed = Arc::new(CountingDcFeed {
            inner: DcCooldownFeed::from_context(&context),
            polls: AtomicU64::new(0),
        });
        let server = spawn_gateway(config(&issuer, Arc::clone(&feed), posture)).await;
        let client = connect(&server, seed_byte).await;

        match posture {
            // `Off` must be given at least as long as an acting posture to do
            // the thing it must not do; waiting for a poll that must never
            // arrive is the one wait this suite cannot express as a condition.
            StrikesEnforcement::Off => tokio::time::sleep(TWO_SWEEPS).await,
            _ => feed.wait_for_poll(1).await,
        }

        let reply = hello(
            &client,
            super::support::session_token_for_account(
                &issuer,
                account,
                client.node,
                super::support::TOKEN_ISSUED_AT_MS,
                super::support::TOKEN_TTL_MS,
            ),
        )
        .await;
        clear_cooldown_entry(&db, account).await;
        Some((feed, server, reply))
    }

    fn admitted(reply: Option<GatewayReply>, posture: &str) {
        match reply {
            Some(GatewayReply::HelloAck { .. }) => {}
            Some(other) => panic!("{posture} did not admit the invalidated account: {other:?}"),
            None => panic!(
                "timed out after {} s with no reply to the hello at all. A refusal \
                 would have arrived as a HelloRefused, so this is silence rather \
                 than evidence that {posture} refused the account",
                HELLO_LIVENESS_TIMEOUT.as_secs(),
            ),
        }
    }

    /// D32 clause (b): "Off observes nothing." Not even the poll.
    #[tokio::test]
    async fn at_off_the_dc_family_is_never_read_and_the_account_is_admitted() {
        let Some((feed, server, reply)) = run_arm(StrikesEnforcement::Off, 60).await else {
            eprintln!("skipped: no FoundationDB dev cluster is configured");
            return;
        };
        assert_eq!(
            feed.polls(),
            0,
            "an Off gateway read identity's dc family; D32 clause (b) says an \
             Off control does not even poll"
        );
        admitted(reply, "off");
        assert_eq!(
            server.standing_metrics().snapshot(),
            orrery_persistd::gateway::GatewayStandingSnapshot::default(),
            "an Off gateway recorded a standing observation"
        );
    }

    /// Shadow runs the whole predicate over the real rows and acts on none of
    /// it — the half #934's bug did not have.
    #[tokio::test]
    async fn at_shadow_the_real_dc_row_is_evaluated_and_the_account_still_admitted() {
        let Some((feed, server, reply)) = run_arm(StrikesEnforcement::Shadow, 61).await else {
            eprintln!("skipped: no FoundationDB dev cluster is configured");
            return;
        };
        assert!(feed.polls() > 0, "a Shadow gateway never polled the feed");
        admitted(reply, "shadow");
        let snapshot = server.standing_metrics().snapshot();
        assert_eq!(
            snapshot.shadow_hello_would_refuse, 1,
            "the shadow arm did not evaluate the dc row it read"
        );
        assert_eq!(
            snapshot.hello_refused_standing, 0,
            "a shadow posture refused a Hello"
        );
    }

    /// Only `Live` acts.
    #[tokio::test]
    async fn at_live_the_real_dc_row_refuses_the_hello() {
        let Some((_feed, server, reply)) = run_arm(StrikesEnforcement::Live, 62).await else {
            eprintln!("skipped: no FoundationDB dev cluster is configured");
            return;
        };
        match reply {
            Some(GatewayReply::HelloRefused { reason, .. })
                if reason == GatewayReply::HELLO_REFUSED_STANDING => {}
            other => panic!(
                "a live gateway did not refuse a Hello for an account identity \
                 had cooled down; the dc row was written and read but nothing \
                 acted on it: {other:?}"
            ),
        }
        assert_eq!(
            server.standing_metrics().snapshot().hello_refused_standing,
            1
        );
    }
}
