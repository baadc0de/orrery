//! The first running consumer of `OrreryClientPlugins` (#873).
//!
//! The first test is the positive proof the current facade can make: two full
//! `MinimalPlugins` apps start without panicking, open real iroh endpoints,
//! connect, and discover the other peer. The second is a characterization of
//! the rot that stops the requested proof there. It attempts to replicate one
//! registered payload, then names the missing bridge instead of replacing it
//! with a test-only transport and claiming convergence.

mod support;

use std::thread;
use std::time::{Duration, Instant};

use aeronet_iroh::endpoint::IrohEndpoint;
use bevy::ecs::system::EntityCommand;
use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;
use lightyear::prelude::{NetworkTarget, Replicate};
use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_net::plugin::{NetConfig, PeerRegistry, ALPN};
use orrery_replicon::{OrreryRepliconAppExt, ReplicatedPayload};

use support::Synthetic;

const TIMEOUT: Duration = Duration::from_secs(10);

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0_u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes)
}

fn client(seed: u8) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    // Missing consumer contract found by the pre-existing composition tests:
    // lightyear calls `init_state`, while MinimalPlugins has no state schedule.
    app.add_plugins(StatesPlugin);
    app.add_plugins(OrreryClientPlugins::<Synthetic>::new(
        OrreryConfig::default().with_net(NetConfig {
            relay_mode: iroh::RelayMode::Disabled,
            secret_key: Some(secret(seed)),
        }),
    ));
    app.replicate::<ReplicatedPayload<i64>>();
    app.finish();
    app
}

fn wait_until(
    left: &mut App,
    right: &mut App,
    context: &str,
    mut condition: impl FnMut(&mut World, &mut World) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        left.update();
        right.update();
        if condition(left.world_mut(), right.world_mut()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out {context}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn endpoint_addr(app: &mut App) -> Option<iroh::EndpointAddr> {
    app.world_mut()
        .query::<&IrohEndpoint>()
        .iter(app.world())
        .next()
        .map(IrohEndpoint::addr)
}

fn connect_pair(left: &mut App, right: &mut App) {
    wait_until(
        left,
        right,
        "for both iroh endpoints to open",
        |left, right| {
            let mut left_endpoints = left.query::<&IrohEndpoint>();
            let mut right_endpoints = right.query::<&IrohEndpoint>();
            left_endpoints.iter(left).next().is_some()
                && right_endpoints.iter(right).next().is_some()
        },
    );

    let target = endpoint_addr(right).expect("right endpoint is open");
    let connect = {
        let mut endpoints = left.world_mut().query::<&IrohEndpoint>();
        endpoints
            .iter(left.world())
            .next()
            .expect("left endpoint is open")
            .connect(target, ALPN)
    };
    let outgoing = left.world_mut().spawn_empty().id();
    connect.apply(left.world_mut().entity_mut(outgoing));

    wait_until(
        left,
        right,
        "for both facade peer registries",
        |left, right| {
            left.resource::<PeerRegistry>().len() == 1
                && right.resource::<PeerRegistry>().len() == 1
        },
    );
}

#[test]
fn two_facades_start_and_discover_over_real_iroh_endpoints() {
    let mut left = client(1);
    let mut right = client(2);

    connect_pair(&mut left, &mut right);

    let left_peer = left.world().resource::<PeerRegistry>().peers[0].1.id;
    let right_peer = right.world().resource::<PeerRegistry>().peers[0].1.id;
    assert_eq!(left_peer, secret(2).public());
    assert_eq!(right_peer, secret(1).public());
}

#[test]
fn connected_iroh_sessions_do_not_become_replication_capable_lightyear_links() {
    let mut left = client(3);
    let mut right = client(4);
    connect_pair(&mut left, &mut right);

    let left_session = left.world().resource::<PeerRegistry>().peers[0].0;
    let right_session = right.world().resource::<PeerRegistry>().peers[0].0;
    assert!(
        left.world()
            .get::<lightyear::prelude::Link>(left_session)
            .is_none()
            && right
                .world()
                .get::<lightyear::prelude::Link>(right_session)
                .is_none(),
        "orrery_net sessions unexpectedly became lightyear links; update the #873 finding and turn this into a convergence test"
    );

    left.world_mut().spawn((
        Replicate::to_clients(NetworkTarget::All),
        ReplicatedPayload(41_i64),
    ));

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        left.update();
        right.update();
        thread::sleep(Duration::from_millis(5));
    }

    let received = right
        .world_mut()
        .query::<&ReplicatedPayload<i64>>()
        .iter(right.world())
        .any(|payload| payload.0 == 41);
    assert!(
        !received,
        "the known missing transport bridge unexpectedly replicated; replace this characterization with state convergence and a genuine rollback"
    );
}
