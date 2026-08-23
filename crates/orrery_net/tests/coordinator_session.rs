//! The Bevy coordinator client against a real coordinator.
//!
//! `orrery_net` and `orrery_coordinator::client` implement the same session
//! independently — the Bevy path and the headless path (docs/10-crates.md §4).
//! Nothing shared enforces that they agree, so this is what keeps them from
//! drifting: a real [`CoordinatorServer`] over loopback iroh, a real Bevy app,
//! and no hand-written fixture standing in for either side of the wire.
//!
//! A fixture would only prove the client accepts what the test author believed
//! the coordinator emits. That is exactly the assumption that was wrong — the
//! manifest roster includes its own recipient, and only the real server says so.

use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;

use orrery_coordinator::server::{FixedUnixClock, ServerConfig, SystemPresenceClock};
use orrery_coordinator::{CoordinatorServer, InterestIssuer};
use orrery_net::coordinator::{ActiveInterest, CoordinatorLink, LinkStatus};
use orrery_net::plugin::NetConfig;
use orrery_net::{CoordinatorConfig, IrohRuntime, IslandMembership, IslandSource, OrreryNetPlugin};
use orrery_protocol::{
    AccountId, CellId, GridId, IssuerKey, IssuerKeyId, NodeId, SessionStanding,
    SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, UnixMillis,
};

const NOW_MS: u64 = 1_000_000;
/// Generous: a loopback handshake is milliseconds, but CI hosts stall.
const PATIENCE: Duration = Duration::from_secs(20);

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[1] = 0x5C;
    iroh::SecretKey::from_bytes(&bytes)
}

fn cell(x: i32) -> CellId {
    CellId::from_coords(bevy::math::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).expect("in range")
}

fn token(issuer: &iroh::SecretKey, bound: NodeId) -> Vec<u8> {
    SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            AccountId::new(7),
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

/// A peer app wired to `server`, with `interest`'s public half trusted.
fn peer(
    server: &CoordinatorServer,
    identity: iroh::SecretKey,
    token: Vec<u8>,
    interest: &iroh::SecretKey,
) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        // The test owns the runtime. Left to itself the plugin builds one and
        // the app cannot be dropped from inside an async test.
        .insert_resource(IrohRuntime::from(tokio::runtime::Handle::current()))
        .add_plugins(OrreryNetPlugin {
            config: NetConfig {
                // Loopback: no relay, so nothing reaches for n0's production map.
                relay_mode: iroh::RelayMode::Disabled,
                secret_key: Some(identity),
            },
            coordinator: CoordinatorConfig {
                address: Some(server.addr()),
                token,
                issuer_keys: vec![IssuerKey::new(IssuerKeyId::new(1), interest.public())],
                handshake_timeout: PATIENCE,
                reconnect_delay: Duration::from_millis(200),
            },
        });
    app
}

/// Pump the app until `done`, or fail with what the link was doing.
///
/// Bevy's `update` is synchronous and the session is a tokio task, so the sleep
/// is not padding — it is what yields the thread for the task to make progress.
async fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        app.update();
        if done(app) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; link status is {:?}",
            app.world().resource::<CoordinatorLink>().status
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_joins_an_island_and_receives_its_interest_grant() {
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;
    let identity = secret(1);
    let me = identity.public();

    let mut app = peer(&server, identity, token(&issuer, me), &interest);

    // The handshake runs over the game endpoint, so the peer the coordinator
    // sees is the peer its island-mates would dial.
    pump_until(&mut app, "the coordinator handshake", |app| {
        app.world().resource::<CoordinatorLink>().is_connected()
    })
    .await;

    assert!(app
        .world()
        .resource::<CoordinatorLink>()
        .report_presence(vec![cell(0), cell(1)]));

    pump_until(&mut app, "an interest grant", |app| {
        app.world().resource::<ActiveInterest>().claims.is_some()
    })
    .await;

    let claims = app
        .world()
        .resource::<ActiveInterest>()
        .claims
        .clone()
        .expect("claims");
    assert_eq!(
        claims.peer, me,
        "the grant is bound to the endpoint the peer actually runs"
    );
    assert_eq!(claims.grid, GridId::ROOT);
    assert_eq!(claims.covered_cells, {
        let mut expected = vec![cell(0), cell(1)];
        expected.sort();
        expected
    });

    // The opaque bytes are kept verbatim: the gateway verifies them itself, so
    // a peer that re-encoded them would break a signature it cannot make.
    let forwarded = app.world().resource::<ActiveInterest>().grant.clone();
    orrery_protocol::verify_interest_grant(
        &forwarded,
        &me,
        &[IssuerKey::new(IssuerKeyId::new(1), interest.public())],
    )
    .expect("a gateway accepts what the peer forwards");

    pump_until(&mut app, "an island manifest", |app| {
        app.world().resource::<IslandMembership>().is_member()
    })
    .await;

    let membership = app.world().resource::<IslandMembership>();
    assert_eq!(membership.source, IslandSource::Coordinator);
    assert_eq!(
        membership.peer_count(),
        0,
        "alone in the island: the roster names only this peer, and a peer is \
         not its own island-mate"
    );
    assert!(!membership.contains(me));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_peers_see_each_other_in_the_island_the_coordinator_formed() {
    // The manifest — not the session set — is what makes a peer a member. Both
    // apps here have exactly one connection each, to the coordinator, and
    // neither has ever seen the other on the wire.
    let issuer = secret(200);
    let interest = secret(201);
    let server = coordinator(&issuer, &interest).await;

    let first = secret(1);
    let second = secret(2);
    let (a, b) = (first.public(), second.public());

    let mut app_a = peer(&server, first, token(&issuer, a), &interest);
    let mut app_b = peer(&server, second, token(&issuer, b), &interest);

    for (app, what) in [(&mut app_a, "peer a"), (&mut app_b, "peer b")] {
        pump_until(app, what, |app| {
            app.world().resource::<CoordinatorLink>().is_connected()
        })
        .await;
    }

    // Overlapping presence is what merges them into one island.
    assert!(app_a
        .world()
        .resource::<CoordinatorLink>()
        .report_presence(vec![cell(0)]));
    assert!(app_b
        .world()
        .resource::<CoordinatorLink>()
        .report_presence(vec![cell(0)]));

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        app_a.update();
        app_b.update();
        let seen_a = app_a.world().resource::<IslandMembership>().contains(b);
        let seen_b = app_b.world().resource::<IslandMembership>().contains(a);
        if seen_a && seen_b {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "peers never saw each other: a sees b = {seen_a}, b sees a = {seen_b}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let island_a = app_a.world().resource::<IslandMembership>().island;
    let island_b = app_b.world().resource::<IslandMembership>().island;
    assert_eq!(island_a, island_b, "one island, agreed on by both");
    assert_eq!(app_a.world().resource::<IslandMembership>().peer_count(), 1);
    assert_eq!(app_b.world().resource::<IslandMembership>().peer_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_coordinator_the_link_stays_disabled() {
    // The coordinator-less path must not spin a session task or leave the app
    // reporting `Connecting` forever — P0's transport tests run in this shape.
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(IrohRuntime::from(tokio::runtime::Handle::current()))
        .add_plugins(OrreryNetPlugin {
            config: NetConfig {
                relay_mode: iroh::RelayMode::Disabled,
                secret_key: Some(secret(9)),
            },
            coordinator: CoordinatorConfig::default(),
        });
    for _ in 0..8 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<CoordinatorLink>().status,
        LinkStatus::Disabled
    );
    assert_eq!(
        app.world().resource::<IslandMembership>().source,
        IslandSource::ConnectedPeers,
        "membership falls back to the session set"
    );
}
