//! Headless prediction sidecar proving that canonical rules run in Lightyear's tick.
//!
//! This is deliberately a small game-owned composition, not another facade
//! plugin.  The facade cannot infer a game's predicted component, turn that
//! component into `CoreState`, choose a hit radius, or decide which entity is
//! predicted by which authority.

#[path = "../tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;
use lightyear::prelude::{
    AppComponentExt, Diffable, InterpolationRegistrationExt, LocalTimeline, Predicted,
    PredictionBuilderExt,
};
use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_authority::PoseSample;
use orrery_core::{tick_rng, OrderedInputs, Ruleset, StateView};
use orrery_net::plugin::NetConfig;
use orrery_predict::{AppReconciliationExt, PredictedBy, ReconciliationResidual, TickBridge};
use orrery_protocol::{LatticePoint, PersistId, UniverseSeed};
use serde::{Deserialize, Serialize};

use support::{Synthetic, SyntheticState};

const SYNTHETIC_SEED: UniverseSeed = UniverseSeed([0x51; 32]);
const HIT_RADIUS_MM: u32 = 450;
const STEP_TRACE_CAP: usize = 64;

/// The one game component registered for prediction and rollback.
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

/// Game-owned identity used to construct the ruleset's per-entity view.
#[derive(Debug, Clone, Copy, Component)]
struct SyntheticEntity(PersistId);

/// The most recent end-of-tick pose, ready for a `PoseHistory::record` writer.
#[derive(Debug, Clone, Copy, Component)]
struct LatestPose(PoseSample);

#[derive(Debug, Resource)]
struct SimulationEnabled(bool);

/// An append-only observation lives outside rollback state, so repeated ticks
/// expose re-execution rather than being restored with the predicted entity.
#[derive(Debug, Default, Resource)]
struct StepTrace(Vec<StepObservation>);

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
struct StepObservation {
    tick: u32,
    position_mm: i64,
    pose: PoseSample,
}

fn interpolate_position(
    start: PredictedPosition,
    end: PredictedPosition,
    t: f32,
) -> PredictedPosition {
    let delta = (end.0 - start.0) as f32;
    PredictedPosition(start.0 + (delta * t).round() as i64)
}

/// The game adapter executed in `FixedUpdate`, including rollback replays.
///
/// `Synthetic::step` is the sole producer of the next position. The system
/// mirrors that canonical result back into the registered predicted component
/// and writes the corresponding authority-facing `PoseSample` in the same
/// tick.
fn step_synthetic_rules(
    enabled: Res<SimulationEnabled>,
    timeline: Res<LocalTimeline>,
    bridge: Res<TickBridge>,
    mut trace: ResMut<StepTrace>,
    mut entities: Query<(&SyntheticEntity, &mut PredictedPosition, &mut LatestPose)>,
) {
    if !enabled.0 {
        return;
    }

    let universe_tick = bridge.resolve(timeline.tick().0);
    for (entity, mut position, mut latest_pose) in &mut entities {
        let mut state = SyntheticState {
            position_mm: position.0,
        };
        let neighbors = BTreeMap::new();
        let observation_ticks = BTreeMap::new();
        let mut view = StateView::new(
            entity.0,
            &mut state,
            &neighbors,
            &observation_ticks,
            universe_tick,
            0,
        );
        let inputs = OrderedInputs::new(&[]);
        let mut rng = tick_rng(SYNTHETIC_SEED, entity.0, universe_tick);

        Synthetic.step(&mut view, &inputs, &mut rng);

        position.0 = state.position_mm;
        let pose = PoseSample {
            position: LatticePoint::new(state.position_mm, 0, 0),
            hit_radius: HIT_RADIUS_MM,
        };
        latest_pose.0 = pose;
        trace.0.push(StepObservation {
            tick: timeline.tick().0,
            position_mm: state.position_mm,
            pose,
        });
        if trace.0.len() > STEP_TRACE_CAP {
            trace.0.remove(0);
        }
    }
}

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0_u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes)
}

fn sidecar(secret_key: iroh::SecretKey, simulation_enabled: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    // `MinimalPlugins` does not install the state-transition schedule that
    // Lightyear initializes. `DefaultPlugins` would already include this.
    app.add_plugins(StatesPlugin);
    app.add_plugins(OrreryClientPlugins::<Synthetic>::new(
        OrreryConfig::default().with_net(NetConfig {
            relay_mode: iroh::RelayMode::Disabled,
            secret_key: Some(secret_key),
        }),
    ));
    app.component::<PredictedPosition>()
        .replicate()
        .add_interpolation_with(interpolate_position)
        .predict()
        .add_correction_fn::<PredictedPosition>(interpolate_position);
    app.track_reconciliation::<PredictedPosition>();
    app.insert_resource(SimulationEnabled(simulation_enabled));
    app.init_resource::<StepTrace>();
    app.add_systems(FixedUpdate, step_synthetic_rules);
    app.finish();
    app
}

fn spawn_predicted(app: &mut App, authority: iroh::PublicKey, persist_id: PersistId) -> Entity {
    app.world_mut()
        .spawn((
            Predicted,
            PredictedPosition::default(),
            SyntheticEntity(persist_id),
            PredictedBy {
                authority,
                persist_id,
            },
            LatestPose(PoseSample {
                position: LatticePoint::default(),
                hit_radius: HIT_RADIUS_MM,
            }),
        ))
        .id()
}

fn main() -> AppExit {
    let key = secret(9);
    let authority = key.public();
    let mut app = sidecar(key, true);
    spawn_predicted(&mut app, authority, PersistId::new(1));
    app.run()
}

#[cfg(test)]
mod tests {
    use bevy::time::TimeUpdateStrategy;
    use lightyear::core::history_buffer::HistoryState;
    use lightyear::core::tick::Tick as SessionTick;
    use lightyear::prelude::{
        LocalTimelineSync, NetworkingMetadata, PredictionHistory, PredictionMetrics,
        StateRollbackMetadata, P2P,
    };

    use super::*;

    fn warm_up(app: &mut App) {
        app.insert_resource(TimeUpdateStrategy::FixedTimesteps(0));
        app.update();
        app.update();
        assert!(
            app.world().resource::<NetworkingMetadata>().mode.is_p2p(),
            "the prediction pipeline is off: topology did not settle on P2P"
        );
        app.world_mut()
            .resource_mut::<LocalTimelineSync>()
            .set_synced(true);
        app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    }

    fn step(app: &mut App) -> u32 {
        app.update();
        app.world().resource::<LocalTimeline>().tick().0
    }

    /// The step-1 proof: the rules-produced position is restored and produced
    /// again by the real Lightyear rollback loop, which reruns `FixedMain`.
    #[test]
    fn rollback_reexecutes_synthetic_step_and_keeps_its_rules_produced_value() {
        const ANCHOR_TICK: u32 = 6;
        const PRESENT_TICK: u32 = 9;

        let key = secret(1);
        let authority = key.public();
        let mut app = sidecar(key, true);
        // A declared P2P session is sufficient to turn on Lightyear's real
        // prediction pipeline; #896 already proves the facade's iroh bridge.
        app.world_mut().spawn(P2P);
        warm_up(&mut app);
        let persist_id = PersistId::new(898);
        let predicted = spawn_predicted(&mut app, authority, persist_id);

        for expected_tick in 1..=ANCHOR_TICK {
            assert_eq!(step(&mut app), expected_tick);
        }
        let anchor = *app
            .world()
            .get::<PredictedPosition>(predicted)
            .expect("predicted position at the anchor");
        assert_eq!(
            anchor,
            PredictedPosition(i64::from(ANCHOR_TICK)),
            "the rollback anchor must itself be rules-produced"
        );

        // This is the only position the test writes. The wrong future stored
        // for ticks 7-9 is 9_001..9_003, which only Synthetic::step produces.
        app.world_mut()
            .entity_mut(predicted)
            .insert(PredictedPosition(9_000));
        for expected_tick in (ANCHOR_TICK + 1)..=PRESENT_TICK {
            assert_eq!(step(&mut app), expected_tick);
        }
        let before_rollback = app.world().resource::<StepTrace>().0.clone();
        for tick in (ANCHOR_TICK + 1)..=PRESENT_TICK {
            assert!(
                before_rollback
                    .iter()
                    .any(|entry| entry.tick == tick && entry.position_mm > 9_000),
                "tick {tick} is not a recorded misprediction produced by Synthetic::step"
            );
        }

        // Deposit the captured rules-produced anchor as the correction, drop
        // the wrong future from Lightyear's ring, and ask the real rollback
        // manager to replay to the present. No ordinary tick runs in this
        // update, so every new trace entry is from rollback re-execution.
        {
            let mut entity = app.world_mut().entity_mut(predicted);
            *entity
                .get_mut::<PredictedPosition>()
                .expect("predicted position") = anchor;
            let mut history = entity
                .get_mut::<PredictionHistory<PredictedPosition>>()
                .expect("Lightyear installed prediction history");
            let anchor_tick = SessionTick(ANCHOR_TICK);
            history.clear_after_tick(anchor_tick);
            history.add_update(anchor_tick, anchor);
        }
        app.world_mut()
            .resource_mut::<StateRollbackMetadata>()
            .request_forced_rollback(SessionTick(ANCHOR_TICK));
        app.insert_resource(TimeUpdateStrategy::FixedTimesteps(0));
        app.update();

        assert_eq!(
            app.world().resource::<PredictionMetrics>().rollbacks,
            1,
            "Lightyear did not execute the requested misprediction rollback"
        );
        assert_eq!(
            app.world().resource::<LocalTimeline>().tick().0,
            PRESENT_TICK,
            "the rollback must replay to the present without advancing it"
        );

        let trace = &app.world().resource::<StepTrace>().0;
        let replay = &trace[before_rollback.len()..];
        assert_eq!(
            replay.len(),
            usize::try_from(PRESENT_TICK - ANCHOR_TICK).unwrap(),
            "each rewound tick must execute Synthetic::step exactly once"
        );
        for (entry, expected_tick) in replay.iter().zip((ANCHOR_TICK + 1)..=PRESENT_TICK) {
            assert_eq!(entry.tick, expected_tick);
            assert_eq!(
                entry.position_mm,
                i64::from(expected_tick),
                "the replayed value must come from Synthetic::step over the captured anchor"
            );
            assert_eq!(
                entry.pose.position.x, entry.position_mm,
                "the replayed tick must also rewrite its PoseSample"
            );
        }

        let live_position = app
            .world()
            .get::<PredictedPosition>(predicted)
            .expect("predicted position");
        let live_pose = app
            .world()
            .get::<LatestPose>(predicted)
            .expect("latest pose");
        let retained_present = app
            .world()
            .get::<PredictionHistory<PredictedPosition>>(predicted)
            .expect("prediction history after replay")
            .buffer()
            .iter()
            .find(|(tick, _)| tick.0 == PRESENT_TICK)
            .map(|(_, state)| state)
            .expect("the replay must rewrite the present-tick history entry");
        assert_eq!(
            retained_present,
            &HistoryState::Updated(PredictedPosition(i64::from(PRESENT_TICK))),
            "the retained post-rollback value must be the rules-produced replay result"
        );
        // After `App::update`, Lightyear intentionally leaves the live
        // component at its frame-interpolated presentation value. The fixed
        // simulation value restored before the next tick is the history entry
        // asserted above; both the live sample and the fixed sample must have
        // come from the rules, never from the test-written 9_000 future.
        assert!(
            trace
                .iter()
                .any(|entry| entry.position_mm == live_position.0),
            "the live presentation value was not produced by Synthetic::step"
        );
        assert_eq!(
            live_pose.0.position.x,
            i64::from(PRESENT_TICK),
            "the latest PoseSample must retain the final rules-produced replay value"
        );
    }
}
