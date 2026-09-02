//! The served mint path: `IdentityClient` ↔ `IdentityServer`, end to end.
//!
//! `tests/issuance.rs` checks [`orrery_identity::IdentityService`] against the
//! protocol verifier; this file checks the *process* around it — the iroh
//! endpoint, the wire messages, and the bootstrap — using that same verifier,
//! taken unmodified, on whatever the service puts on the wire. That is #861's
//! acceptance shape: a running process answers login and half-TTL refresh, a
//! login for an unbound node is refused `NotBound`, an unreadable ledger
//! refuses rather than minting `Good`, an issuer rotation is performed against
//! the running service, and the standing stamped into a token is the strike
//! ledger's answer rather than a constant.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use orrery_identity::mem::MemAccountStore;
use orrery_identity::standing::StrikeRowSource;
use orrery_identity::{
    mint_invite, AccountStore, BindOutcome, ComputedStanding, CooldownStanding, IdentityClient,
    IdentityError, IdentityServer, IdentityServerConfig, IdentityService, InviteLedger,
    IssuerKeyring, IssuerSigningKey, OsInviteCodeGenerator, StandingInvalidationSource,
    StandingSource, StaticStanding, UnavailableStanding, DEFAULT_SESSION_TOKEN_TTL_MS,
    DEFAULT_STANDING_THRESHOLDS,
};
use orrery_persistd::adjudication::{
    StrikeEvidenceRef, StrikeKind, StrikeMode, StrikeRow, MAJOR_STRIKE_WEIGHT_MILLI,
    STRIKE_RETENTION_MS,
};
use orrery_protocol::{
    AccountId, AccountInvalidation, IdentityRefusal, IdentityReply, IssuerKey, IssuerKeyId, NodeId,
    PersistId, RulesetId, SessionStanding, SessionTokenClaimsV1, SessionTokenVerificationError,
    SessionTokenVerifier, Tick, TokenClock, UnixMillis,
};

const ACCOUNT: AccountId = AccountId(0x0861_0000_0000_0001);
const T0: u64 = 1_700_000_000_000;
const PATIENCE: Duration = Duration::from_secs(10);

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[31] = 0x11;
    iroh::SecretKey::from_bytes(&bytes)
}

fn node(seed: u8) -> NodeId {
    secret(seed).public()
}

fn signing_key(id: u32, seed: u8) -> IssuerSigningKey {
    IssuerSigningKey::new(IssuerKeyId::new(id), secret(seed))
}

/// The one clock both the service mints with and the server verifies with, so
/// a test moves time once and both halves of the contract see the same
/// instant. This is the property a real deployment has by construction — one
/// host clock — and what [`orrery_identity::SystemClock`] gives the binary.
#[derive(Debug, Clone)]
struct MutableClock(Arc<AtomicU64>);

impl TokenClock for MutableClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

/// One account, one bound node: everything a legal mint needs.
async fn account_with_bound_node(store: &Arc<MemAccountStore>, bound: &NodeId) {
    store
        .create_account(ACCOUNT, T0)
        .await
        .expect("create account");
    assert_eq!(
        store.bind(ACCOUNT, bound, T0).await.expect("bind"),
        BindOutcome::Bound
    );
}

/// A running identity server over [`MemAccountStore`], generic over the
/// standing source so each test supplies exactly the posture its subject
/// needs.
async fn spawn_server<T: StandingSource + 'static>(
    store: &Arc<MemAccountStore>,
    standing: T,
) -> (
    IdentityServer<Arc<MemAccountStore>, T, MutableClock>,
    MutableClock,
) {
    let clock = MutableClock(Arc::new(AtomicU64::new(T0)));
    let service = IdentityService::new(
        Arc::clone(store),
        standing,
        clock.clone(),
        IssuerKeyring::new(signing_key(1, 0xA1)),
    );
    let server = IdentityServer::spawn(IdentityServerConfig::for_tests(
        Arc::new(service),
        clock.clone(),
    ))
    .await
    .expect("spawn identity server");
    (server, clock)
}

/// Verify what the wire carried, with the verifier the coordinator and gateway
/// run — never a test-local reading of the claims.
fn verify(
    clock: &MutableClock,
    keys: Vec<IssuerKey>,
    token: &[u8],
    client_node: &NodeId,
) -> Result<SessionTokenClaimsV1, SessionTokenVerificationError> {
    SessionTokenVerifier::new(clock.clone(), keys).verify(token, client_node)
}

#[tokio::test]
async fn a_running_service_mints_a_login_token_the_protocol_verifier_accepts() {
    let store = Arc::new(MemAccountStore::new());
    let bound = node(1);
    account_with_bound_node(&store, &bound).await;
    let (server, clock) = spawn_server(&store, StaticStanding::all_good()).await;
    let client = IdentityClient::connect(secret(1), server.addr(), PATIENCE)
        .await
        .expect("connect");

    let reply = client
        .login(ACCOUNT, None, PATIENCE)
        .await
        .expect("login answered");
    let IdentityReply::Issued {
        token,
        refresh_at_ms,
    } = reply
    else {
        panic!("a bound node's login mints: {reply:?}");
    };
    let claims = verify(
        &clock,
        server.service().published_issuer_keys(),
        &token,
        &client.node(),
    )
    .expect("the token verifies through the protocol verifier, unmodified");
    assert_eq!(claims.account, ACCOUNT);
    assert_eq!(claims.node, bound);
    assert_eq!(claims.ttl_ms.0, DEFAULT_SESSION_TOKEN_TTL_MS);
    assert_eq!(
        refresh_at_ms.0,
        T0 + DEFAULT_SESSION_TOKEN_TTL_MS / 2,
        "docs/09 §8: the issuer, not the client, computes the half-TTL instant"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_half_ttl_refresh_over_the_wire_reissues_a_verifying_token() {
    let store = Arc::new(MemAccountStore::new());
    let bound = node(1);
    account_with_bound_node(&store, &bound).await;
    let (server, clock) = spawn_server(&store, StaticStanding::all_good()).await;
    let client = IdentityClient::connect(secret(1), server.addr(), PATIENCE)
        .await
        .expect("connect");

    let ttl_ms = 60_000u64;
    let reply = client
        .login(ACCOUNT, Some(ttl_ms), PATIENCE)
        .await
        .expect("login answered");
    let IdentityReply::Issued {
        token: first,
        refresh_at_ms,
    } = reply
    else {
        panic!("login mints: {reply:?}");
    };
    assert_eq!(
        refresh_at_ms.0,
        T0 + ttl_ms / 2,
        "docs/09 §8: refresh at half-TTL"
    );

    // The client comes back at the instructed instant. The server verifies the
    // presented token with its own clock before re-running the four questions,
    // so a refresh is a mint that had to prove it was speaking for a session
    // this service signed.
    clock.0.store(T0 + ttl_ms / 2, Ordering::SeqCst);
    let reply = client
        .refresh(first.clone(), Some(ttl_ms), PATIENCE)
        .await
        .expect("refresh answered");
    let IdentityReply::Issued {
        token: second,
        refresh_at_ms: second_refresh_at,
    } = reply
    else {
        panic!("refresh reissues: {reply:?}");
    };
    assert_eq!(
        second_refresh_at.0,
        T0 + ttl_ms,
        "the next instructed instant is a half-TTL later"
    );
    assert_ne!(
        first, second,
        "a refresh reissues, it does not echo the presented bytes"
    );

    // At the first token's expiry the old one is refused, by name, and the
    // refreshed one still verifies — the point of refreshing early.
    let keys = server.service().published_issuer_keys();
    clock.0.store(T0 + ttl_ms, Ordering::SeqCst);
    assert_eq!(
        verify(&clock, keys.clone(), &first, &client.node()),
        Err(SessionTokenVerificationError::Expired)
    );
    assert!(verify(&clock, keys, &second, &client.node()).is_ok());
    server.shutdown().await;
}

#[tokio::test]
async fn a_login_for_an_unbound_node_is_refused_not_bound_over_the_wire() {
    let store = Arc::new(MemAccountStore::new());
    account_with_bound_node(&store, &node(1)).await;
    let (server, _clock) = spawn_server(&store, StaticStanding::all_good()).await;

    // A different transport identity asks for the account. The body carries no
    // node field to forge — the service takes the asker's identity from the
    // connection — so the claim simply does not resolve.
    let stranger = IdentityClient::connect(secret(30), server.addr(), PATIENCE)
        .await
        .expect("connect");
    let reply = stranger
        .login(ACCOUNT, None, PATIENCE)
        .await
        .expect("the request is answered");
    assert_eq!(
        reply,
        IdentityReply::Refused(IdentityRefusal::NotBound {
            node: stranger.node(),
            account: ACCOUNT,
        }),
        "the four checks are exercised by the served path, not only by the service unit tests"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_unreadable_ledger_refuses_the_mint_over_the_wire_instead_of_minting_good() {
    let store = Arc::new(MemAccountStore::new());
    account_with_bound_node(&store, &node(1)).await;
    // `UnavailableStanding` is the deployment whose ledger cannot be read; on
    // the served path it must produce a *refusal*, because a client that
    // receives a token has no way to know the `Good` inside it was never read.
    let (server, _clock) = spawn_server(&store, UnavailableStanding).await;
    let client = IdentityClient::connect(secret(1), server.addr(), PATIENCE)
        .await
        .expect("connect");

    assert_eq!(
        client.login(ACCOUNT, None, PATIENCE).await.expect("the request is answered"),
        IdentityReply::Refused(IdentityRefusal::StandingUnavailable(ACCOUNT)),
        "D33 clause (f) on the served path: the unanswered question is a refusal, never a minted Good"
    );
    server.shutdown().await;
}

/// A mutable stand-in for the executor-owned `ya` strike family, so a test can
/// file findings and move decay without ever writing the account store.
#[derive(Clone, Default)]
struct MutableStrikeRows(Arc<Mutex<Vec<StrikeRow>>>);

impl MutableStrikeRows {
    fn file(&self, row: StrikeRow) {
        Self::lock(&self.0).push(row);
    }

    fn lock(rows: &Mutex<Vec<StrikeRow>>) -> MutexGuard<'_, Vec<StrikeRow>> {
        rows.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl StrikeRowSource for MutableStrikeRows {
    async fn rows(&self, _account: AccountId) -> Result<Vec<StrikeRow>, IdentityError> {
        Ok(Self::lock(&self.0).clone())
    }
}

fn major_strike(issued_at_ms: u64) -> StrikeRow {
    StrikeRow {
        issued_at_ms,
        weight_milli: MAJOR_STRIKE_WEIGHT_MILLI,
        kind: StrikeKind::Deviation,
        evidence_ref: StrikeEvidenceRef {
            entity: PersistId::new(1),
            window_start: Tick::new(1),
            window_end: Tick::new(2),
            digest: [issued_at_ms as u8; 32],
        },
        ruleset: RulesetId {
            version: 1,
            digest: [1; 32],
        },
        mode: StrikeMode::Live,
        expires_at_ms: issued_at_ms + STRIKE_RETENTION_MS,
    }
}

/// The test the issue exists for: the standing a served mint stamps — and the
/// decision to mint at all — is read from the strike ledger at mint time.
///
/// One connection, three phases of the same account's ledger:
///
/// 1. no rows: the mint answers, and the token says `Good` because the scorer
///    *read* an empty ledger;
/// 2. two live major findings: the served mint refuses `Cooldown`, by name —
///    against a mint that stamps a constant `Good` (the defect #861 closes)
///    this phase returns `Issued` and this test fails here;
/// 3. decay plus the dwell floor: the same account, same connection, mints
///    again, and the token's standing claim has moved to what the ledger now
///    holds.
///
/// Phase 2 also asserts the #934 half: the refused mint's durable dwell write
/// is exactly what `StandingInvalidationSource` publishes, so the running
/// service is what makes the coordinator's standing feed real.
#[tokio::test]
async fn the_served_mint_decision_follows_the_real_strike_ledger_not_a_constant() {
    let store = Arc::new(MemAccountStore::new());
    let bound = node(1);
    account_with_bound_node(&store, &bound).await;

    let rows = MutableStrikeRows::default();
    let scorer_now = Arc::new(AtomicU64::new(0));
    let clock_handle = Arc::clone(&scorer_now);
    let scorer = ComputedStanding::new(
        rows.clone(),
        move || clock_handle.load(Ordering::SeqCst),
        DEFAULT_STANDING_THRESHOLDS,
    )
    .expect("the default policy package is coherent");
    let (server, clock) =
        spawn_server(&store, CooldownStanding::new(Arc::clone(&store), scorer)).await;
    let client = IdentityClient::connect(secret(1), server.addr(), PATIENCE)
        .await
        .expect("connect");

    // Phase 1: an empty ledger is a *read* `Good`, stamped because the scorer
    // found nothing, not because the mint defaults to it.
    let reply = client
        .login(ACCOUNT, None, PATIENCE)
        .await
        .expect("clean-ledger login answered");
    let IdentityReply::Issued { token: good, .. } = reply else {
        panic!("a clean ledger mints: {reply:?}");
    };
    let claims = verify(
        &clock,
        server.service().published_issuer_keys(),
        &good,
        &client.node(),
    )
    .expect("verify");
    assert_eq!(claims.standing, SessionStanding::Good);

    // Phase 2: findings arrive on the ledger; the next served mint reads them
    // and refuses. No token exists for the account after this phase.
    rows.file(major_strike(scorer_now.load(Ordering::SeqCst)));
    rows.file(major_strike(scorer_now.load(Ordering::SeqCst)));
    assert_eq!(
        client
            .login(ACCOUNT, None, PATIENCE)
            .await
            .expect("the request is answered"),
        IdentityReply::Refused(IdentityRefusal::Cooldown(ACCOUNT)),
        "the mint read the strike ledger and refused the account it found there"
    );
    let refusal_instant = scorer_now.load(Ordering::SeqCst);
    assert_eq!(
        StandingInvalidationSource::new(Arc::clone(&store))
            .current()
            .await
            .expect("read the dc family"),
        vec![AccountInvalidation {
            account: ACCOUNT,
            effective_from_ms: UnixMillis(refusal_instant),
        }],
        "the refused mint's dwell write is the #934 feed's data: the running service is its consumer half"
    );

    // Phase 3: decay and the dwell floor pass; the ledger's answer changes and
    // the stamped claim follows it.
    let day_ms = 24 * 60 * 60 * 1_000;
    scorer_now.store(14 * day_ms, Ordering::SeqCst);
    let reply = client
        .login(ACCOUNT, None, PATIENCE)
        .await
        .expect("post-dwell login answered");
    let IdentityReply::Issued {
        token: quarantined, ..
    } = reply
    else {
        panic!("dwell passed and the score decayed; the mint must answer: {reply:?}");
    };
    let claims = verify(
        &clock,
        server.service().published_issuer_keys(),
        &quarantined,
        &client.node(),
    )
    .expect("verify");
    assert_eq!(
        claims.standing,
        SessionStanding::Quarantined,
        "the standing claim moved because the ledger moved — a constant could not have done that"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_issuer_rotation_is_performed_against_the_running_service() {
    let store = Arc::new(MemAccountStore::new());
    let bound = node(1);
    account_with_bound_node(&store, &bound).await;
    let (server, clock) = spawn_server(&store, StaticStanding::all_good()).await;
    let client = IdentityClient::connect(secret(1), server.addr(), PATIENCE)
        .await
        .expect("connect");

    let reply = client
        .login(ACCOUNT, None, PATIENCE)
        .await
        .expect("login answered");
    let IdentityReply::Issued { token: before, .. } = reply else {
        panic!("login mints: {reply:?}");
    };
    let old_key_id = server.service().active_issuer_key_id();

    // The rotation is an operator action against the running process — the
    // same accessor `bin/orrery-identity.rs` would expose to its admin surface.
    server
        .service()
        .rotate(signing_key(2, 0xB2))
        .expect("rotate");
    let new_key_id = server.service().active_issuer_key_id();
    assert_ne!(new_key_id, old_key_id);
    let reply = client
        .login(ACCOUNT, None, PATIENCE)
        .await
        .expect("post-rotate login answered");
    let IdentityReply::Issued { token: after, .. } = reply else {
        panic!("post-rotate login mints: {reply:?}");
    };
    assert_eq!(
        verify(
            &clock,
            server.service().published_issuer_keys(),
            &after,
            &client.node()
        )
        .expect("verify")
        .issuer_key_id,
        new_key_id,
        "the new token is signed under the new key"
    );

    // Dual accept: one verifier holding the published set takes both.
    let dual = server.service().published_issuer_keys();
    assert_eq!(dual.len(), 2);
    assert!(verify(&clock, dual.clone(), &before, &client.node()).is_ok());
    assert!(verify(&clock, dual, &after, &client.node()).is_ok());

    // Retire closes the window, and the old key's tokens fail by name.
    server
        .service()
        .retire_issuer_key(old_key_id)
        .expect("retire");
    let closed = server.service().published_issuer_keys();
    assert_eq!(closed.len(), 1);
    assert_eq!(
        verify(&clock, closed.clone(), &before, &client.node()),
        Err(SessionTokenVerificationError::UnknownIssuer(old_key_id)),
        "a retired key's tokens stop verifying, by name, against the running service's published set"
    );
    assert!(verify(&clock, closed, &after, &client.node()).is_ok());
    server.shutdown().await;
}

#[tokio::test]
async fn a_deployment_creates_and_binds_an_account_through_the_served_redeem() {
    let store = Arc::new(MemAccountStore::new());
    let clock = MutableClock(Arc::new(AtomicU64::new(T0)));
    let service = IdentityService::new(
        Arc::clone(&store),
        StaticStanding::all_good(),
        clock.clone(),
        IssuerKeyring::new(signing_key(1, 0xA1)),
    );
    let mut config = IdentityServerConfig::for_tests(Arc::new(service), clock.clone());
    // The deployment half of the redeem surface: a real ledger path, exactly
    // what `--ledger` hands the binary.
    let ledger = tempfile::tempdir().expect("ledger directory");
    let ledger_path = ledger.path().join("invites.tsv");
    config.ledger = Some(ledger_path.clone());
    let server = IdentityServer::spawn(config)
        .await
        .expect("spawn identity server");

    // The operator half: one code minted into that ledger, by the same library
    // call `orrery-invite mint` runs.
    let minted = InviteLedger::update_locked(&ledger_path, |book| {
        mint_invite(
            book,
            "shakedown".into(),
            &mut OsInviteCodeGenerator,
            UnixMillis::new(T0),
        )
    })
    .expect("mint invite");

    // The deployment half again: a fresh node redeems, and is minted to.
    let newcomer = IdentityClient::connect(secret(40), server.addr(), PATIENCE)
        .await
        .expect("connect");
    let reply = newcomer
        .redeem_invite(minted.code, None, PATIENCE)
        .await
        .expect("redeem answered");
    let IdentityReply::Issued { token, .. } = reply else {
        panic!("redemption mints: {reply:?}");
    };
    let claims = verify(
        &clock,
        server.service().published_issuer_keys(),
        &token,
        &newcomer.node(),
    )
    .expect("verify");
    assert_eq!(claims.account, minted.account);

    // The account now exists *and* the redeeming node is bound to it, so the
    // same node mints again by plain login — account creation without a test
    // harness, which is the acceptance line this file exists for.
    let reply = newcomer
        .login(minted.account, None, PATIENCE)
        .await
        .expect("login answered");
    assert!(
        matches!(reply, IdentityReply::Issued { .. }),
        "the redeemed account logs in on the node that redeemed it: {reply:?}"
    );

    // And a different node is still a stranger to it — the binding is real.
    let stranger = IdentityClient::connect(secret(41), server.addr(), PATIENCE)
        .await
        .expect("connect");
    assert_eq!(
        stranger
            .login(minted.account, None, PATIENCE)
            .await
            .expect("the request is answered"),
        IdentityReply::Refused(IdentityRefusal::NotBound {
            node: stranger.node(),
            account: minted.account,
        })
    );
    server.shutdown().await;
}
