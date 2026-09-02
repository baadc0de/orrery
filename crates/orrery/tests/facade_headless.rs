//! The first running consumer of `OrreryClientPlugins` (#873).
//!
//! Two full `MinimalPlugins` apps start, open real iroh endpoints, connect,
//! discover the other peer, and carry one registered entity through the
//! facade's production P2P replication bridge.

use std::thread;
use std::time::{Duration, Instant};

use aeronet_iroh::endpoint::IrohEndpoint;
use bevy::ecs::system::EntityCommand;
use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;
use lightyear::prelude::{
    AppComponentExt, Diffable, InterpolationRegistrationExt, NetworkTarget, Predicted,
    PredictionBuilderExt, PredictionTarget, Replicate,
};
use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_net::plugin::{NetConfig, PeerRegistry, ALPN};
use orrery_predict::{
    AppReconciliationExt, PredictedBy, ReconciliationMonitor, ReconciliationResidual, TrackKey,
};
use orrery_protocol::PersistId;
use orrery_replicon::{OrreryRepliconAppExt, ReplicatedPayload};
use serde::{Deserialize, Serialize};

use orrery_games::Skirmish;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Component)]
struct PredictedPosition(i64);

impl Diffable for PredictedPosition {
    fn base_value() -> Self {
        Self::default()
    }

    fn diff(&self, new: &Self) -> Self {
        Self(new.0 - self.0)
    }

    fn apply_diff(&mut self, delta: &Self) {
        self.0 += delta.0;
    }
}

impl ReconciliationResidual for PredictedPosition {
    fn pos_error_mm(&self) -> i64 {
        self.0.abs()
    }
}

fn interpolate_position(
    start: PredictedPosition,
    end: PredictedPosition,
    t: f32,
) -> PredictedPosition {
    let delta = (end.0 - start.0) as f32;
    PredictedPosition(start.0 + (delta * t).round() as i64)
}

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
    app.add_plugins(OrreryClientPlugins::<Skirmish>::new(
        OrreryConfig::default().with_net(NetConfig {
            relay_mode: iroh::RelayMode::Disabled,
            secret_key: Some(secret(seed)),
        }),
    ));
    app.replicate::<ReplicatedPayload<i64>>();
    app.component::<PredictedPosition>()
        .replicate()
        .add_interpolation_with(interpolate_position)
        .predict()
        .add_correction_fn::<PredictedPosition>(interpolate_position);
    app.track_reconciliation::<PredictedPosition>();
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

    let authoritative = left
        .world_mut()
        .spawn((
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(NetworkTarget::All),
            ReplicatedPayload(41_i64),
            PredictedPosition(41),
        ))
        .id();

    wait_until(
        &mut left,
        &mut right,
        "for the registered entity state to converge through the facade bridge",
        |_, right| {
            right
                .query::<&ReplicatedPayload<i64>>()
                .iter(right)
                .any(|payload| payload.0 == 41)
        },
    );

    let predicted = right
        .world_mut()
        .query_filtered::<Entity, (With<PredictedPosition>, With<Predicted>)>()
        .iter(right.world())
        .next()
        .expect("the prediction target should produce a predicted receiver entity");
    let persist_id = PersistId(889);
    right.world_mut().entity_mut(predicted).insert(PredictedBy {
        authority: secret(1).public(),
        persist_id,
    });

    // Record a deliberately wrong local prediction in Lightyear's history.
    right
        .world_mut()
        .entity_mut(predicted)
        .insert(PredictedPosition(9_000));
    thread::sleep(Duration::from_millis(25));
    left.update();
    right.update();

    // The next authoritative update must force Lightyear's rollback path. A
    // manually inserted VisualCorrection would only test the monitor wiring;
    // this value travels over the real iroh session and causes the correction.
    left.world_mut()
        .entity_mut(authoritative)
        .insert((ReplicatedPayload(77_i64), PredictedPosition(77)));

    let key = TrackKey {
        authority: secret(1).public(),
        entity: persist_id,
    };
    wait_until(
        &mut left,
        &mut right,
        "for authoritative reconciliation to produce an observed residual",
        |_, right| {
            let state_converged = right
                .query::<&ReplicatedPayload<i64>>()
                .iter(right)
                .any(|payload| payload.0 == 77);
            state_converged
                && right
                    .resource::<lightyear::prelude::PredictionMetrics>()
                    .rollbacks
                    > 0
                && right
                    .resource::<ReconciliationMonitor>()
                    .track(&key)
                    .is_some_and(|track| track.rollbacks > 0 && track.pos_ewma_mm > 0)
        },
    );
}
