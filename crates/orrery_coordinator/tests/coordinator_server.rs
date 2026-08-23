//! End-to-end coordinator tests over real iroh loopback endpoints.
//!
//! These exercise the thing that was missing rather than the registry logic
//! underneath it, which has its own unit tests: does a peer that connects,
//! authenticates and reports presence actually receive a usable interest grant
//! and an island manifest, and does the coordinator refuse the things it must?

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_coordinator::server::{
    FixedUnixClock, IdentityHealth, ServerConfig, SystemPresenceClock,
};
use orrery_coordinator::{
    CoordinatorClient, CoordinatorServer, FeedFailure, InterestIssuer, StandingInvalidationFeed,
    StrikesMode, StrikesPosture, WitnessEpochIssuer, WitnessSeedConfig,
};
use orrery_protocol::{
    verify_interest_grant, AccountId, AccountInvalidation, CellId, CoordMsg, GridId, IssuerKey,
    IssuerKeyId, NodeId, SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1,
    TokenClock, UnixMillis, MAX_SESSION_TOKEN_TTL_MS,
};

const NOW_MS: u64 = 1_000_000;
const PATIENCE: Duration = Duration::from_secs(10);

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[1] = 0x5C;
    iroh::SecretKey::from_bytes(&bytes)
}

fn node(seed: u8) -> NodeId {
    secret(seed).public()
}

fn cell(x: i32) -> CellId {
    CellId::from_coords(glam::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).unwrap()
}

/// A token binding `bound` to its own account, so a fixture of several peers
/// is several *accounts* — witness eligibility dedups on the account, so
/// peers sharing one would collapse to a single pool slot.
fn account_token(issuer: &iroh::SecretKey, bound: NodeId, account: u64) -> Vec<u8> {
    SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            AccountId::new(account),
            bound,
            UnixMillis::new(NOW_MS - 1_000),
            SessionTokenTtlMs::new(60_000),
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

fn token(issuer: &iroh::SecretKey, bound: NodeId, ttl_ms: u64) -> Vec<u8> {
    SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            AccountId::new(7),
            bound,
            UnixMillis::new(NOW_MS - 1_000),
            SessionTokenTtlMs::new(ttl_ms),
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

async fn coordinator(issuer: &iroh::SecretKey, interest: &iroh::SecretKey) -> CoordinatorServer {
    CoordinatorServer::spawn(ServerConfig {
        token_clock: Arc::new(FixedUnixClock(NOW_MS)),
        presence_clock: Arc::new(SystemPresenceClock::default()),
        ..ServerConfig::new(
            [IssuerKey::new(IssuerKeyId::new(1), issuer.public())],
            InterestIssuer::new(interest.clone(), IssuerKeyId::new(1)),
        )
    })
    .await
    .expect("spawn coordinator")
}

/// A wall clock the test moves, so one token can be signed while it is valid
/// and presented again after it has expired — which is the whole shape of the
/// grace rule and is unreachable with a fixed clock.
#[derive(Debug, Clone)]
struct StepClock(Arc<AtomicU64>);

impl TokenClock for StepClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::Relaxed))
    }
}

/// An identity service a test can take down.
#[derive(Debug, Clone)]
struct SwitchedIdentityHealth(Arc<AtomicBool>);

impl IdentityHealth for SwitchedIdentityHealth {
    fn is_available(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A coordinator whose clock and identity health the test drives.
async fn coordinator_in_an_outage(
    issuer: &iroh::SecretKey,
    interest: &iroh::SecretKey,
) -> (CoordinatorServer, Arc<AtomicU64>, Arc<AtomicBool>) {
    let clock = Arc::new(AtomicU64::new(NOW_MS));
    let identity_up = Arc::new(AtomicBool::new(true));
    let server = CoordinatorServer::spawn(ServerConfig {
        token_clock: Arc::new(StepClock(Arc::clone(&clock))),
        presence_clock: Arc::new(SystemPresenceClock::default()),
        identity_health: Arc::new(SwitchedIdentityHealth(Arc::clone(&identity_up))),
        ..ServerConfig::new(
            [IssuerKey::new(IssuerKeyId::new(1), issuer.public())],
            InterestIssuer::new(interest.clone(), IssuerKeyId::new(1)),
        )
    })
    .await
    .expect("spawn coordinator");
    (server, clock, identity_up)
}

#[tokio::test]
async fn an_identity_outage_does_not_end_an_established_session() {
    // docs/09 §8: "coordinator and gateway accept expired-but-otherwise-valid
    // tokens for *established* sessions while identity is unreachable". The
    // coordinator half of that sentence is what this pins. Without it a relay
    // flap during an identity outage costs the peer its presence, its island
    // membership and its interest grant.
    let issuer = secret(200);
    let interest = secret(201);
    let (server, clock, identity_up) = coordinator_in_an_outage(&issuer, &interest).await;

    // Given: a peer that authenticated while identity was up.
    let session = token(&issuer, node(1), 60_000);
    let first = CoordinatorClient::connect(secret(1), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("peer admitted");
    first.report_presence(vec![cell(0)]).expect("presence");
    first.next_grant(PATIENCE).await.expect("first grant");
    first.leave().await;

    // When: identity goes down and the token ages out under it. The peer
    // cannot refresh — refresh is the first thing an identity outage takes.
    identity_up.store(false, Ordering::Relaxed);
    clock.store(NOW_MS + 120_000, Ordering::Relaxed);

    // Then: the same token, now 61 s expired, still re-establishes the
    // session it established the first time.
    let again = CoordinatorClient::connect(secret(1), server.addr(), session, PATIENCE)
        .await
        .expect("an established peer rides out the outage");
    again.report_presence(vec![cell(0)]).expect("presence");
    let grant = again.next_grant(PATIENCE).await.expect("grant");
    let trusted = [IssuerKey::new(IssuerKeyId::new(1), interest.public())];
    assert_eq!(
        verify_interest_grant(&grant, &node(1), &trusted)
            .expect("valid")
            .covered_cells,
        vec![cell(0)]
    );
    assert_eq!(server.stats().await.connected_peers, 1);

    server.shutdown().await;
}

#[tokio::test]
async fn an_expired_token_is_refused_while_identity_is_reachable() {
    // The asymmetry is the rule. Grace is a concession to an unreachable
    // identity service, not a second lifetime for every token: with identity
    // up, the peer can refresh, so an expired token is simply expired.
    let issuer = secret(200);
    let interest = secret(201);
    let (server, clock, _identity_up) = coordinator_in_an_outage(&issuer, &interest).await;

    let session = token(&issuer, node(1), 60_000);
    let first = CoordinatorClient::connect(secret(1), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("peer admitted");
    first.leave().await;

    clock.store(NOW_MS + 120_000, Ordering::Relaxed);
    assert!(
        CoordinatorClient::connect(
            secret(1),
            server.addr(),
            session,
            Duration::from_millis(750)
        )
        .await
        .is_err(),
        "an expired token was accepted with identity reachable"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_identity_outage_still_locks_out_a_peer_that_never_logged_in() {
    // "An identity outage locks out new logins, never in-flight play" — the
    // first half. A stranger's expired token has no established session
    // behind it, and an outage must not turn one into a way in.
    let issuer = secret(200);
    let interest = secret(201);
    let (server, clock, identity_up) = coordinator_in_an_outage(&issuer, &interest).await;

    identity_up.store(false, Ordering::Relaxed);
    clock.store(NOW_MS + 120_000, Ordering::Relaxed);
    assert!(
        CoordinatorClient::connect(
            secret(1),
            server.addr(),
            token(&issuer, node(1), 60_000),
            Duration::from_millis(750)
        )
        .await
        .is_err(),
        "an outage admitted a peer this coordinator had never seen"
    );
    assert_eq!(server.stats().await.connected_peers, 0);

    server.shutdown().await;
}

#[tokio::test]
async fn grace_admits_a_late_token_and_never_a_weaker_check() {
    // Everything except the clock still has to hold. Otherwise "identity is
    // down" would be a window in which signatures, node bindings and issuers
    // stopped meaning anything — which is the failure mode that would make
    // the grace rule worse than the churn it prevents.
    let issuer = secret(200);
    let interest = secret(201);
    let (server, clock, identity_up) = coordinator_in_an_outage(&issuer, &interest).await;

    let session = token(&issuer, node(1), 60_000);
    let established = CoordinatorClient::connect(secret(1), server.addr(), session, PATIENCE)
        .await
        .expect("peer admitted");
    established.leave().await;

    identity_up.store(false, Ordering::Relaxed);
    clock.store(NOW_MS + 120_000, Ordering::Relaxed);

    // An expired token from an issuer this coordinator does not trust.
    let forged = secret(199);
    assert!(CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&forged, node(1), 60_000),
        Duration::from_millis(750)
    )
    .await
    .is_err());

    // An expired token bound to a different node than the one presenting it.
    assert!(CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(2), 60_000),
        Duration::from_millis(750)
    )
    .await
    .is_err());

    // A genuine expired token for this peer — but not the one it established
    // with. Same account, later issue time: grace re-admits a session, it
    // does not accept fresh claims nobody's live signature vouches for.
    assert!(CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 30_000),
        Duration::from_millis(750)
    )
    .await
    .is_err());

    assert_eq!(server.stats().await.connected_peers, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn grace_does_not_outlast_its_window() {
    // The bound the gateway does not pin. An outage long enough to age a
    // token an hour past its expiry has stopped being something to ride out,
    // and a coordinator running on standing that old is worse than a login.
    let issuer = secret(200);
    let interest = secret(201);
    let (server, clock, identity_up) = coordinator_in_an_outage(&issuer, &interest).await;

    // The peer holds its session open across the whole outage — a live
    // connection is never re-verified — and only then blips. So its session
    // ended a moment ago and it is established by every other measure; the
    // one thing wrong with the token it presents is its age, which is
    // precisely what this window is.
    let session = token(&issuer, node(1), 60_000);
    let first = CoordinatorClient::connect(secret(1), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("peer admitted");
    identity_up.store(false, Ordering::Relaxed);
    // The token expired at NOW_MS + 59_000; one millisecond past the window.
    clock.store(
        NOW_MS + 59_000 + MAX_SESSION_TOKEN_TTL_MS + 1,
        Ordering::Relaxed,
    );
    first.leave().await;
    assert!(
        CoordinatorClient::connect(
            secret(1),
            server.addr(),
            session,
            Duration::from_millis(750)
        )
        .await
        .is_err(),
        "grace outlasted its window"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_graced_peer_keeps_its_place_in_the_island_manifest() {
    // The consequence that makes this a topology problem rather than a login
    // problem: a coordinator session ends by forgetting the peer entirely, so
    // a peer that cannot re-establish drops out of every manifest its island
    // pushes and out of the pool D28's reseed triggers count.
    let issuer = secret(200);
    let interest = secret(201);
    let (server, clock, identity_up) = coordinator_in_an_outage(&issuer, &interest).await;

    let staying = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("first peer admitted");
    staying.report_presence(vec![cell(0)]).expect("presence");
    staying.next_manifest(PATIENCE).await.expect("own island");

    let session = token(&issuer, node(2), 60_000);
    let blipping = CoordinatorClient::connect(secret(2), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("second peer admitted");
    blipping.report_presence(vec![cell(0)]).expect("presence");
    let merged = staying.next_manifest(PATIENCE).await.expect("merged");
    assert_eq!(merged.peers.len(), 2);

    // Its connection drops mid-outage — a relay flap, a NAT rebind.
    blipping.leave().await;
    staying.next_manifest(PATIENCE).await.expect("departure");
    identity_up.store(false, Ordering::Relaxed);
    clock.store(NOW_MS + 120_000, Ordering::Relaxed);

    // It comes back on the only token it has, and the island is whole again.
    let back = CoordinatorClient::connect(secret(2), server.addr(), session, PATIENCE)
        .await
        .expect("the peer is re-admitted on grace");
    back.report_presence(vec![cell(0)]).expect("presence");
    let rejoined = staying
        .next_manifest(PATIENCE)
        .await
        .expect("the survivor learns the peer is back");
    assert_eq!(rejoined.peers.len(), 2);
    assert!(rejoined.peers.iter().any(|entry| entry.node == node(2)));
    assert_eq!(server.stats().await.connected_peers, 2);

    server.shutdown().await;
}

#[tokio::test]
async fn a_graced_peer_is_left_out_of_the_witness_pool_it_would_have_joined() {
    // The standing decision, end to end. D28 clause (e) filters on the
    // token's signed `standing`, and a graced session's is only as fresh as
    // the outage is long — a quarantine applied while identity was down is
    // invisible. So the peer plays and does not witness, and the coordinator
    // says so in the pool it announces rather than in a comment.
    let issuer = secret(200);
    let interest = secret(201);
    let clock = Arc::new(AtomicU64::new(NOW_MS));
    let identity_up = Arc::new(AtomicBool::new(true));
    let server = CoordinatorServer::spawn(ServerConfig {
        token_clock: Arc::new(StepClock(Arc::clone(&clock))),
        presence_clock: Arc::new(SystemPresenceClock::default()),
        identity_health: Arc::new(SwitchedIdentityHealth(Arc::clone(&identity_up))),
        witness_issuer: Some(WitnessEpochIssuer::new(
            interest.clone(),
            IssuerKeyId::new(1),
            [0x2Au8; 32],
            1,
        )),
        // Every window driven to its floor, for one reason each: a test
        // cannot wait out a 10 s probation or a 30 s epoch, and the 60 s
        // departure cooldown would exclude the returning peer on its own —
        // exactly the confound that would make this test pass while proving
        // nothing about grace. The epoch is 1 ms rather than 0 because a
        // verifier refuses a zero-length epoch, and a QUIC reconnect is not
        // finished inside one millisecond.
        witness_seed: WitnessSeedConfig {
            min_presence_ms: 0,
            reseed_min_ms: 0,
            epoch_ms: 1,
            reseed_cooldown_ms: 0,
            ..WitnessSeedConfig::default()
        },
        ..ServerConfig::new(
            [IssuerKey::new(IssuerKeyId::new(1), issuer.public())],
            InterestIssuer::new(interest.clone(), IssuerKeyId::new(1)),
        )
    })
    .await
    .expect("spawn coordinator");

    // Five peers that stay, on five accounts — the floor exactly, so the
    // graced sixth is the difference between a pool of five and one of six.
    let mut staying = Vec::new();
    for index in 1..=5u8 {
        let client = CoordinatorClient::connect(
            secret(index),
            server.addr(),
            account_token(&issuer, node(index), u64::from(index)),
            PATIENCE,
        )
        .await
        .expect("peer admitted");
        client.report_presence(vec![cell(0)]).expect("presence");
        staying.push(client);
    }

    let session = account_token(&issuer, node(6), 6);
    let blipping = CoordinatorClient::connect(secret(6), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("sixth peer admitted");
    blipping.report_presence(vec![cell(0)]).expect("presence");
    blipping
        .next_witness_epoch(PATIENCE)
        .await
        .expect("a verified session is witness-eligible");
    blipping.leave().await;

    // It comes back mid-outage on the token it already had.
    identity_up.store(false, Ordering::Relaxed);
    clock.store(NOW_MS + 120_000, Ordering::Relaxed);
    let back = CoordinatorClient::connect(secret(6), server.addr(), session, PATIENCE)
        .await
        .expect("the peer is re-admitted on grace");
    back.report_presence(vec![cell(0)]).expect("presence");

    // Its own connection is new, so the first announcement it is handed is
    // the one its return seeded — no backlog to read past.
    let bytes = back
        .next_witness_epoch(PATIENCE)
        .await
        .expect("the reseed its return triggered");
    let trusted = [IssuerKey::new(IssuerKeyId::new(1), interest.public())];
    let claims =
        orrery_protocol::verify_witness_epoch(&bytes, &trusted).expect("a signed announcement");
    assert!(
        !claims.candidates.contains(&node(6)),
        "a session running on an expired token's standing must not witness"
    );
    assert_eq!(
        claims.candidates.len(),
        5,
        "the five verified peers, and only those"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn presence_yields_a_grant_a_gateway_will_verify() {
    // Given: a coordinator trusting one identity issuer.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    // When: a peer authenticates and reports what it covers.
    let client = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("coordinator admits an authenticated peer");
    client
        .report_presence(vec![cell(0), cell(1)])
        .expect("send presence");

    // Then: it gets back bytes a gateway will accept — verified here with the
    // gateway's own verifier, against only the coordinator's public half.
    let grant = client.next_grant(PATIENCE).await.expect("grant arrives");
    let trusted = [IssuerKey::new(IssuerKeyId::new(1), interest.public())];
    let claims = verify_interest_grant(&grant, &node(1), &trusted)
        .expect("a gateway accepts the coordinator's signature");
    assert_eq!(claims.peer, node(1));
    assert_eq!(claims.grid, GridId::ROOT);
    assert_eq!(claims.covered_cells, {
        let mut expected = vec![cell(0), cell(1)];
        expected.sort();
        expected
    });

    // And: an island formed around that presence, named back to the peer.
    let manifest = client
        .next_manifest(PATIENCE)
        .await
        .expect("manifest arrives");
    assert!(manifest.peers.iter().any(|entry| entry.node == node(1)));
    assert!(manifest.cells.contains(&cell(0)));

    let stats = server.stats().await;
    assert_eq!(stats.presence_reports, 1);
    assert_eq!(stats.islands, 1);
    assert_eq!(stats.connected_peers, 1);
    server.shutdown().await;
}

#[tokio::test]
async fn a_merge_tells_every_peer_in_the_island_not_just_the_reporter() {
    // Given: two peers on disjoint cells, so two islands.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    let first = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("first peer admitted");
    first.report_presence(vec![cell(0)]).expect("presence");
    first.next_manifest(PATIENCE).await.expect("own island");

    let second = CoordinatorClient::connect(
        secret(2),
        server.addr(),
        token(&issuer, node(2), 60_000),
        PATIENCE,
    )
    .await
    .expect("second peer admitted");

    // When: the second peer's presence overlaps the first's cell, merging them.
    second
        .report_presence(vec![cell(0), cell(1)])
        .expect("presence");

    // Then: the peer that reported *nothing* is told too. Without this it would
    // keep acting on a roster that no longer describes its island.
    let merged = first
        .next_manifest(PATIENCE)
        .await
        .expect("the quiet peer learns about the merge");
    assert_eq!(merged.peers.len(), 2);
    assert!(merged.peers.iter().any(|entry| entry.node == node(1)));
    assert!(merged.peers.iter().any(|entry| entry.node == node(2)));

    server.shutdown().await;
}

#[tokio::test]
async fn a_departing_peer_is_removed_from_the_roster_it_leaves_behind() {
    // Given: two peers sharing an island.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    let staying = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("first peer admitted");
    staying.report_presence(vec![cell(0)]).expect("presence");
    staying.next_manifest(PATIENCE).await.expect("own island");

    let leaving = CoordinatorClient::connect(
        secret(2),
        server.addr(),
        token(&issuer, node(2), 60_000),
        PATIENCE,
    )
    .await
    .expect("second peer admitted");
    leaving.report_presence(vec![cell(0)]).expect("presence");
    let merged = staying.next_manifest(PATIENCE).await.expect("merged");
    assert_eq!(merged.peers.len(), 2);

    // When: it leaves gracefully. (A crash is the slower shape — QUIC cannot
    // tell a departed peer from a silent one until its idle timeout — and is
    // covered by the island gate, not here.)
    leaving.leave().await;

    // Then: the survivor is told, so it does not keep reaching for a ghost.
    let after = staying
        .next_manifest(PATIENCE)
        .await
        .expect("the survivor learns about the departure");
    assert_eq!(after.peers.len(), 1);
    assert_eq!(after.peers[0].node, node(1));
    assert!(
        after.epoch > merged.epoch,
        "membership changes must advance the epoch so a peer can order them"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_sole_peer_leaving_its_island_is_ordered_to_drain_it() {
    // Given: one peer, alone in the island its presence formed.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    let peer = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer admitted");
    peer.report_presence(vec![cell(0)]).expect("presence");
    let formed = peer.next_manifest(PATIENCE).await.expect("own island");
    assert_eq!(formed.peers.len(), 1);
    assert!(formed.cells.contains(&cell(0)));

    // When: it departs that island — by moving out of range of it, which is
    // the departure a still-connected peer can be told about. (Closing the
    // connection is the other shape, covered by the roster test above; a peer
    // that has hung up cannot read an order addressed to it.)
    peer.report_presence(vec![cell(500)]).expect("presence");

    // Then: the island it emptied is named in a drain order, with a deadline
    // in the future on the coordinator's clock. Without it the peer would get
    // a new assignment and no word at all about the island it just retired.
    let mut drain = None;
    let mut assignment = None;
    let until = tokio::time::Instant::now() + PATIENCE;
    while drain.is_none() || assignment.is_none() {
        let remaining = until.saturating_duration_since(tokio::time::Instant::now());
        match peer.recv(remaining).await {
            Some(CoordMsg::Drain { island, deadline }) => drain = Some((island, deadline)),
            Some(CoordMsg::IslandAssignment { manifest }) => assignment = Some(manifest),
            Some(_) => continue,
            None => break,
        }
    }
    let (island, deadline) = drain.expect("the peer is told which island it drained");
    assert_eq!(island, formed.island);
    assert!(
        deadline > NOW_MS,
        "a drain deadline already past is an order with no grace in it"
    );
    assert_eq!(
        deadline,
        NOW_MS + 10_000,
        "the default grace is D7's 10 s lease TTL"
    );

    // And: it is not left islandless — the move formed a new one.
    let joined = assignment.expect("the peer is assigned the island it moved into");
    assert_ne!(joined.island, formed.island);
    assert!(joined.cells.contains(&cell(500)));

    let stats = server.stats().await;
    assert_eq!(stats.drains_issued, 1);
    assert_eq!(stats.islands, 1);
    server.shutdown().await;
}

#[tokio::test]
async fn presence_is_refused_without_a_valid_token() {
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    // A token from the wrong issuer: presence would otherwise let an
    // unauthenticated peer mint itself interest, and interest authorizes
    // authority.
    let forged = secret(199);
    assert!(CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&forged, node(1), 60_000),
        Duration::from_millis(750),
    )
    .await
    .is_err());

    // A genuine token bound to a *different* peer must not admit this one.
    assert!(CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(2), 60_000),
        Duration::from_millis(750),
    )
    .await
    .is_err());

    assert_eq!(server.stats().await.connected_peers, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn unusable_presence_is_dropped_rather_than_signed() {
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;
    let client = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer admitted");

    // An empty report authorizes nothing and a verifier would refuse the
    // resulting grant, so it never becomes one.
    client.report_presence(Vec::new()).expect("send");
    assert!(client.next_grant(Duration::from_millis(500)).await.is_err());

    // An oversized one would turn a signed handout into an unbounded
    // membership test on the gateway's claim hot path.
    let oversized = (0..(orrery_protocol::MAX_PRESENCE_CELLS as i32 + 1))
        .map(cell)
        .collect();
    client.report_presence(oversized).expect("send");
    assert!(client.next_grant(Duration::from_millis(500)).await.is_err());

    assert_eq!(server.stats().await.presence_reports, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn a_presence_flood_is_rate_limited_rather_than_served() {
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;
    let client = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer admitted");

    // Each report costs an island re-evaluation and a fresh signature, so an
    // unmetered one is a cheap way to make a coordinator expensive.
    for index in 0..200 {
        client
            .report_presence(vec![cell(index % 8)])
            .expect("send presence");
    }
    // Drain whatever the coordinator chose to answer with.
    while client.recv(Duration::from_millis(300)).await.is_some() {}

    let accepted = server.stats().await.presence_reports;
    assert!(accepted > 0, "the budget must not refuse everything");
    assert!(
        accepted < 200,
        "a 200-report flood must not be served in full, got {accepted}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_reconnecting_peer_is_handed_its_interest_again() {
    // A gateway holds no interest for a session that has just started, and the
    // coordinator's own record of presence outlives the connection — so a
    // returning peer must not have to move before it can claim again.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    let first = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer admitted");
    first.report_presence(vec![cell(0)]).expect("presence");
    first.next_grant(PATIENCE).await.expect("first grant");
    drop(first);

    let again = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer re-admitted");
    let grant = again
        .next_grant(PATIENCE)
        .await
        .expect("a returning peer is handed its interest without reporting again");
    let trusted = [IssuerKey::new(IssuerKeyId::new(1), interest.public())];
    assert_eq!(
        verify_interest_grant(&grant, &node(1), &trusted)
            .expect("valid")
            .covered_cells,
        vec![cell(0)]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn coordinator_directed_messages_from_a_peer_are_ignored() {
    // `CoordMsg` is one enum for both directions. A peer sending a manifest or
    // a grant is confused rather than hostile, but it must not be able to
    // inject either into the coordinator's state.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;
    let client = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer admitted");

    let forged_manifest = CoordMsg::IslandAssignment {
        manifest: orrery_protocol::IslandManifest {
            island: orrery_protocol::IslandId::new(99),
            epoch: 99,
            cells: vec![cell(0)],
            regime: orrery_protocol::TopologyRegime::Mesh,
            peers: vec![orrery_protocol::PeerEntry {
                node: node(2),
                cells: vec![cell(0)],
            }],
        },
    };
    client
        .connection()
        .send_datagram(bytes::Bytes::from(
            orrery_protocol::channels::encode_stream_frame(&forged_manifest),
        ))
        .expect("send");

    // Nothing changed: no island was invented, and the session survives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stats = server.stats().await;
    assert_eq!(stats.islands, 0);
    assert_eq!(stats.connected_peers, 1);

    // The session is still usable afterwards.
    client.report_presence(vec![cell(0)]).expect("presence");
    assert!(client.next_grant(PATIENCE).await.is_ok());

    server.shutdown().await;
}

#[tokio::test]
async fn a_populated_cell_gets_a_witness_set_every_peer_can_courier() {
    // D28 clause (a) end to end: the coordinator alone chooses, and the peers
    // covering the cell are the only delivery mechanism. There is no
    // coordinator→gateway connection anywhere in this test because there is
    // none in the design — a gateway needs the public key and the bytes.
    let issuer = secret(200);
    let interest = secret(201);
    let witness_master = [0x2Au8; 32];
    let server = CoordinatorServer::spawn(ServerConfig {
        token_clock: Arc::new(FixedUnixClock(NOW_MS)),
        presence_clock: Arc::new(SystemPresenceClock::default()),
        witness_issuer: Some(WitnessEpochIssuer::new(
            interest.clone(),
            IssuerKeyId::new(1),
            witness_master,
            1,
        )),
        // The eligibility windows are real clocks; a test cannot wait out a
        // 10 s probation, and the filters themselves are unit-tested against
        // an injected clock in `witness.rs`.
        witness_seed: WitnessSeedConfig {
            min_presence_ms: 0,
            reseed_min_ms: 0,
            ..WitnessSeedConfig::default()
        },
        ..ServerConfig::new(
            [IssuerKey::new(IssuerKeyId::new(1), issuer.public())],
            InterestIssuer::new(interest.clone(), IssuerKeyId::new(1)),
        )
    })
    .await
    .expect("spawn coordinator");

    // Five peers on five accounts, all covering one cell — the floor exactly.
    let mut clients = Vec::new();
    for index in 1..=5u8 {
        let client = CoordinatorClient::connect(
            secret(index),
            server.addr(),
            account_token(&issuer, node(index), u64::from(index)),
            PATIENCE,
        )
        .await
        .expect("peer admitted");
        client.report_presence(vec![cell(0)]).expect("presence");
        clients.push(client);
    }

    // Every one of them receives the same signed announcement, and every one
    // of them can hand it to a gateway that holds only the public half.
    let trusted = [IssuerKey::new(IssuerKeyId::new(1), interest.public())];
    let mut announcements = Vec::new();
    for client in &clients {
        let bytes = client
            .next_witness_epoch(PATIENCE)
            .await
            .expect("a covering peer is handed the cell's witness set");
        let claims = orrery_protocol::verify_witness_epoch(&bytes, &trusted)
            .expect("a gateway accepts the coordinator's signature");
        announcements.push(claims);
    }
    let first = &announcements[0];
    assert_eq!(first.cell, cell(0));
    assert_eq!(
        first.candidates.len(),
        5,
        "five eligible peers, five accounts"
    );
    assert_eq!(
        first.selected.len(),
        5,
        "a pool under the target is taken whole"
    );
    assert!(
        first.prev_seed_key.is_none(),
        "the cell's first epoch opens nothing"
    );
    for claims in &announcements {
        assert_eq!(claims, first, "one epoch, one set, one set of bytes");
    }
    // The set is drawn from the pool and from nothing else: nobody who was
    // not in the cell can appear in it.
    for selected in &first.selected {
        assert!(first.candidates.contains(selected));
        assert!((1..=5u8).any(|index| node(index) == *selected));
    }

    let stats = server.stats().await;
    assert_eq!(stats.witness_epochs_seeded, 1);
    assert_eq!(stats.witness_epochs_delivered, 5);

    for client in clients {
        client.leave().await;
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_coordinator_with_no_witness_issuer_announces_nothing() {
    // The default posture, and it must stay a real default rather than a set
    // of empty announcements: P4 files nothing, so `orrery_witness`'s
    // self-chosen fallback is still what runs, and a coordinator that shipped
    // an epoch without being configured to seed one would be asserting an
    // authority it was never given a key for.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;
    let client = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("peer admitted");
    client.report_presence(vec![cell(0)]).expect("presence");
    assert!(client.next_grant(PATIENCE).await.is_ok());

    assert!(
        client
            .next_witness_epoch(Duration::from_millis(300))
            .await
            .is_err(),
        "an unconfigured coordinator announced a witness epoch"
    );
    assert_eq!(server.stats().await.witness_epochs_seeded, 0);
    server.shutdown().await;
}

// ── Standing enforcement (D33 clause (e), issue #219) ────────────────────

/// An in-memory stand-in for identity's publication, whose entries a test can
/// change while sessions are open — which is exactly the moment this
/// machinery exists for.
///
/// The coordinator consults this only on its 1 s sweep, never on admission
/// (D32 clause (c): no hot-path reads), so tests synchronize on
/// [`MutableStandingFeed::wait_for_poll`] rather than on raw sleeps.
struct MutableStandingFeed {
    entries: std::sync::Mutex<Vec<AccountInvalidation>>,
    polls: AtomicU64,
}

impl MutableStandingFeed {
    fn serving(entries: Vec<AccountInvalidation>) -> Arc<Self> {
        Arc::new(Self {
            entries: std::sync::Mutex::new(entries),
            polls: AtomicU64::new(0),
        })
    }

    async fn publish(&self, entry: AccountInvalidation) {
        self.entries.lock().expect("feed lock").push(entry);
    }

    /// Block until the coordinator's sweep has polled at least `minimum`
    /// times, which is when anything published so far is in its consumer
    /// state and every open session has been re-checked against it.
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
impl StandingInvalidationFeed for MutableStandingFeed {
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self.entries.lock().expect("feed lock").clone())
    }
}

fn invalidated(account: AccountId, effective_from_ms: u64) -> AccountInvalidation {
    AccountInvalidation {
        account,
        effective_from_ms: UnixMillis::new(effective_from_ms),
    }
}

/// A coordinator whose standing feed, posture, clock and identity health the
/// test drives.
async fn coordinator_with_standing(
    issuer: &iroh::SecretKey,
    interest: &iroh::SecretKey,
    feed: Arc<MutableStandingFeed>,
    posture: StrikesMode,
) -> (CoordinatorServer, Arc<AtomicU64>, Arc<AtomicBool>) {
    let clock = Arc::new(AtomicU64::new(NOW_MS));
    let identity_up = Arc::new(AtomicBool::new(true));
    let server = CoordinatorServer::spawn(ServerConfig {
        token_clock: Arc::new(StepClock(Arc::clone(&clock))),
        presence_clock: Arc::new(SystemPresenceClock::default()),
        identity_health: Arc::new(SwitchedIdentityHealth(Arc::clone(&identity_up))),
        standing_feed: Some(feed),
        strikes_posture: StrikesPosture::new(posture),
        ..ServerConfig::new(
            [IssuerKey::new(IssuerKeyId::new(1), issuer.public())],
            InterestIssuer::new(interest.clone(), IssuerKeyId::new(1)),
        )
    })
    .await
    .expect("spawn coordinator");
    (server, clock, identity_up)
}

/// The termination half of clause (e): a session opened before the crossing
/// loses presence, island membership and its witness facts within one poll
/// interval, and its still-cryptographically-valid token cannot walk back in.
#[tokio::test]
async fn an_open_session_is_terminated_and_the_dead_token_cannot_re_establish() {
    let issuer = secret(210);
    let interest = secret(211);
    let feed = MutableStandingFeed::serving(vec![]);
    let (server, _clock, _identity_up) =
        coordinator_with_standing(&issuer, &interest, Arc::clone(&feed), StrikesMode::Live).await;

    // The token was minted at `NOW_MS - 1_000`; an invalidation effective at
    // `NOW_MS` kills it and nothing minted after it.
    let session = token(&issuer, node(1), 60_000);
    let peer = CoordinatorClient::connect(secret(1), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("peer admitted");
    peer.report_presence(vec![cell(0)]).expect("presence");
    peer.next_grant(PATIENCE).await.expect("grant");

    feed.publish(invalidated(AccountId::new(7), NOW_MS)).await;
    let polled = feed.polls.load(Ordering::SeqCst);
    feed.wait_for_poll(polled + 1).await;

    // The connection task ran the ordinary disconnect path: presence
    // forgotten, roster rebroadcast, witness facts dropped.
    let stats = server.stats().await;
    assert_eq!(stats.connected_peers, 0, "the invalidated session is gone");
    assert_eq!(stats.standing_sessions_terminated, 1);

    assert!(
        CoordinatorClient::connect(
            secret(1),
            server.addr(),
            session,
            Duration::from_millis(750)
        )
        .await
        .is_err(),
        "a pre-watermark token cannot re-establish presence"
    );
    server.shutdown().await;
}

/// Grace exists so an identity *outage* does not end in-flight play. An
/// invalidation published before the outage is a standing decision that
/// outlives it; one published during it cannot exist, because identity is
/// what publishes. Either way the graced re-admission of #227 must not
/// resurrect the account here.
#[tokio::test]
async fn grace_does_not_admit_an_account_identity_has_invalidated() {
    let issuer = secret(210);
    let interest = secret(211);
    let feed = MutableStandingFeed::serving(vec![]);
    let (server, clock, identity_up) =
        coordinator_with_standing(&issuer, &interest, Arc::clone(&feed), StrikesMode::Live).await;

    // Established while identity answered for the account.
    let session = token(&issuer, node(1), 60_000);
    let first = CoordinatorClient::connect(secret(1), server.addr(), session.clone(), PATIENCE)
        .await
        .expect("peer admitted");
    first.report_presence(vec![cell(0)]).expect("presence");
    first.leave().await;

    feed.publish(invalidated(AccountId::new(7), NOW_MS)).await;
    let polled = feed.polls.load(Ordering::SeqCst);
    feed.wait_for_poll(polled + 1).await;

    // Then the token expired under an outage. Without the entry this exact
    // sequence is the graced re-admission `an_identity_outage_does_not_end_
    // an_established_session` pins.
    identity_up.store(false, Ordering::Relaxed);
    clock.store(NOW_MS + 120_000, Ordering::Relaxed);
    assert!(
        CoordinatorClient::connect(
            secret(1),
            server.addr(),
            session,
            Duration::from_millis(750)
        )
        .await
        .is_err(),
        "the invalidation overrides grace"
    );
    assert_eq!(server.stats().await.standing_hellos_refused, 1);
    server.shutdown().await;
}

/// D17.3's requirement on this process too: with C5 still observing, the
/// invalidated account is admitted, plays normally, and the counter moves —
/// a control that cannot be observed before it acts cannot be promoted.
#[tokio::test]
async fn in_shadow_the_invalidated_account_is_still_admitted_and_counted() {
    let issuer = secret(210);
    let interest = secret(211);
    let feed = MutableStandingFeed::serving(vec![invalidated(AccountId::new(7), NOW_MS + 10)]);
    let (server, _clock, _identity_up) =
        coordinator_with_standing(&issuer, &interest, Arc::clone(&feed), StrikesMode::Shadow).await;
    feed.wait_for_poll(1).await;

    let peer = CoordinatorClient::connect(
        secret(1),
        server.addr(),
        token(&issuer, node(1), 60_000),
        PATIENCE,
    )
    .await
    .expect("shadow suppresses the refusal");
    peer.report_presence(vec![cell(0)]).expect("presence");
    assert!(peer.next_grant(PATIENCE).await.is_ok());

    let stats = server.stats().await;
    assert_eq!(stats.shadow_hellos_would_refuse, 1);
    assert_eq!(stats.standing_hellos_refused, 0);
    assert_eq!(stats.connected_peers, 1);
    server.shutdown().await;
}
