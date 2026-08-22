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

async fn hello(client: &Client, token: Vec<u8>) -> Option<GatewayReply> {
    client
        .conn
        .send_control(&GatewayMsg::VersionedHello {
            token,
            node: client.node,
            version: orrery_protocol::PROTOCOL_VERSION,
        })
        .await;
    client.conn.next_reply(Duration::from_secs(5)).await
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
    assert!(matches!(
        hello(&client, token).await,
        Some(GatewayReply::HelloRefused {
            reason: GatewayReply::HELLO_REFUSED_STANDING,
            ..
        })
    ));
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
    assert!(matches!(
        hello(&client, token.clone()).await,
        Some(GatewayReply::HelloAck { .. })
    ));

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
    assert!(matches!(
        hello(&client, token).await,
        Some(GatewayReply::HelloRefused {
            reason: GatewayReply::HELLO_REFUSED_STANDING,
            ..
        })
    ));
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
    assert!(
        matches!(
            hello(&client, token.clone()).await,
            Some(GatewayReply::HelloAck { .. })
        ),
        "established normally before any of this started"
    );

    feed.publish(invalidated(AccountId::new(7), INVALIDATED_AT_MS))
        .await;
    // The refusal rides the consumer state, which the sweep refreshes —
    // wait for a poll that has seen the entry, then expire into the outage.
    let polled = feed.polls.load(Ordering::SeqCst);
    feed.wait_for_poll(polled + 1).await;
    health.0.store(false, Ordering::SeqCst);
    // The token expired (900 + 60_000 < 61_500) while identity is down.
    clock.0.store(61_500, Ordering::SeqCst);
    assert!(matches!(
        hello(&client, token).await,
        Some(GatewayReply::HelloRefused {
            reason: GatewayReply::HELLO_REFUSED_STANDING,
            ..
        })
    ));
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
    assert!(
        matches!(
            hello(&client, support::valid_session_token(client.node)).await,
            Some(GatewayReply::HelloAck { .. })
        ),
        "shadow suppresses the refusal"
    );
    let snapshot = server.standing_metrics().snapshot();
    assert_eq!(snapshot.shadow_hello_would_refuse, 1);
    assert_eq!(snapshot.hello_refused_standing, 0);
}
