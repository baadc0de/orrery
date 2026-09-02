//! The synthetic-rules prediction sidecar (#898 step 1, #871).
//!
//! A deliberately small game: one entity, one integer of state, one weapon.
//! It exists to make platform mechanisms *reachable*, and it is a shipped
//! binary rather than an example because the recurring defect this crate was
//! written against is a seam that exists and that nothing outside a test ever
//! executes.
//!
//! What it proves that no test crate could:
//!
//! - the canonical rules run inside Lightyear's predicted tick, and survive
//!   rollback (`Synthetic::step` is the only producer of a position);
//! - a held entity's canonical pose reaches the authority's 32-tick ring
//!   through [`orrery::hit::publish_canonical_poses`], every tick, at the tick
//!   the rules stamped;
//! - a [`HitClaim`](orrery_protocol::HitClaim) arriving on a real peer link is
//!   answered with a [`HitVerdict`](orrery_protocol::HitVerdict) by
//!   [`orrery::hit::answer_hit_claims`].
//!
//! The rules themselves are not here. They are `orrery_synthetic`, which is
//! Bevy-free: D42 (a) / D43 (e)(1) keep a `Ruleset` out of any crate with Bevy
//! in its graph, and `scripts/core-gates.sh` discovers and enforces that by
//! name. This crate is the composition — the app, the predicted component, the
//! weapon table — and it is where every Bevy dependency lives.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;
use lightyear::prelude::{
    AppComponentExt, Diffable, InterpolationRegistrationExt, LocalTimeline, Predicted,
    PredictionBuilderExt,
};
use serde::{Deserialize, Serialize};

use orrery::hit::{CanonicalPose, OrreryHitRegistrationPlugin};
use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_authority::{AuthorityPhase, HitRules, PersistIdentity, PoseSample};
use orrery_core::{tick_rng, OrderedInputs, Ruleset, StateView};
use orrery_net::plugin::NetConfig;
use orrery_predict::{AppReconciliationExt, PredictedBy, ReconciliationResidual, TickBridge};
use orrery_protocol::{LatticePoint, PersistId, UniverseSeed, WeaponRef};
use orrery_synthetic::{Synthetic, SyntheticState};

/// The universe seed the sidecar's per-tick RNG is derived from.
pub const SYNTHETIC_SEED: UniverseSeed = UniverseSeed([0x51; 32]);
/// How big the one entity is to be hit, in lattice units (millimetres).
pub const HIT_RADIUS_MM: u32 = 450;
/// The one weapon this ruleset knows.
pub const SYNTHETIC_WEAPON: WeaponRef = WeaponRef(9);
/// How far [`SYNTHETIC_WEAPON`] reaches, in lattice units.
pub const SYNTHETIC_REACH_MM: u32 = 20_000;
/// How many step observations the trace retains.
const STEP_TRACE_CAP: usize = 64;

/// The static hit facts [`Synthetic`] knows: one weapon's reach, and the
/// tolerance that absorbs the shooter's quantization of its ray.
///
/// A [`Resource`] rather than a method on [`Synthetic`] because it is what the
/// game hands the platform:
/// [`OrreryHitRegistrationPlugin`] is generic over exactly this.
#[derive(Debug, Clone, Copy, Resource)]
pub struct SyntheticHitRules {
    /// Reach of [`SYNTHETIC_WEAPON`], in lattice units.
    pub reach_mm: u32,
    /// How far outside the hit radius a ray may still count.
    pub tolerance_mm: u32,
}

impl Default for SyntheticHitRules {
    fn default() -> Self {
        Self {
            reach_mm: SYNTHETIC_REACH_MM,
            // The shooter quantizes its direction, so an exactly-grazing ray
            // may miss by a fraction of a lattice unit. Zero would make the
            // sphere the sphere and refuse those honestly-aimed shots.
            tolerance_mm: 50,
        }
    }
}

impl HitRules for SyntheticHitRules {
    fn weapon_reach(&self, weapon: WeaponRef) -> Option<u32> {
        (weapon == SYNTHETIC_WEAPON).then_some(self.reach_mm)
    }

    fn hit_tolerance(&self) -> u32 {
        self.tolerance_mm
    }
}

/// The one game component registered for prediction and rollback.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Component)]
pub struct PredictedPosition(pub i64);

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

/// Whether the canonical step runs. A sidecar started with it off is a peer
/// that holds nothing and asserts nothing.
#[derive(Debug, Resource)]
pub struct SimulationEnabled(pub bool);

/// An append-only record of every executed canonical step.
///
/// It lives outside rollback state deliberately: a replayed tick appends a
/// second entry rather than restoring over the first, which is what lets a
/// test see re-execution instead of inferring it.
#[derive(Debug, Default, Resource)]
pub struct StepTrace(pub Vec<StepObservation>);

/// One executed canonical step.
#[derive(Debug, Clone, Copy)]
pub struct StepObservation {
    /// The session tick the step ran on.
    pub tick: u32,
    /// The position `Synthetic::step` produced.
    pub position_mm: i64,
    /// The pose written for the authority in the same step.
    pub pose: PoseSample,
}

/// Lightyear's presentation blend between two predicted positions.
#[must_use]
pub fn interpolate_position(
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
/// and writes the corresponding [`CanonicalPose`] **in the same tick, from the
/// same value**.
///
/// That last property is the whole contract. The pose is taken from
/// `state.position_mm` — what the rules just asserted — and never from the
/// live component, which Lightyear leaves at a frame-interpolated
/// presentation value once the frame ends. Publishing the presentation value
/// would have the target's authority adjudicate a hit against a pose no
/// ruleset ever produced.
pub fn step_synthetic_rules(
    enabled: Res<SimulationEnabled>,
    timeline: Res<LocalTimeline>,
    bridge: Res<TickBridge>,
    mut trace: ResMut<StepTrace>,
    mut entities: Query<(&PersistIdentity, &mut PredictedPosition, &mut CanonicalPose)>,
) {
    if !enabled.0 {
        return;
    }

    let universe_tick = bridge.resolve(timeline.tick().0);
    for (identity, mut position, mut canonical) in &mut entities {
        let mut state = SyntheticState {
            position_mm: position.0,
        };
        let neighbors = BTreeMap::new();
        let observation_ticks = BTreeMap::new();
        let mut view = StateView::new(
            identity.0,
            &mut state,
            &neighbors,
            &observation_ticks,
            universe_tick,
            0,
        );
        let inputs = OrderedInputs::new(&[]);
        let mut rng = tick_rng(SYNTHETIC_SEED, identity.0, universe_tick);

        Synthetic.step(&mut view, &inputs, &mut rng);

        position.0 = state.position_mm;
        let pose = PoseSample {
            position: LatticePoint::new(state.position_mm, 0, 0),
            hit_radius: HIT_RADIUS_MM,
        };
        *canonical = CanonicalPose::new(universe_tick, pose);
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

/// Build the sidecar app: the facade group, hit registration, and the game's
/// one predicted component.
///
/// `simulation_enabled` exists so an observer sidecar can be composed
/// identically and assert nothing.
pub fn sidecar(secret_key: iroh::SecretKey, simulation_enabled: bool) -> App {
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
    // The two wires #871 was filed about, and the rules table only the game
    // can supply.
    app.insert_resource(SyntheticHitRules::default());
    app.add_plugins(OrreryHitRegistrationPlugin::<SyntheticHitRules>::new());
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

/// Spawn the one predicted entity this sidecar simulates.
///
/// [`PersistIdentity`] is what the publisher and the authority's live-fence
/// filter both key on, so it is the entity's identity here rather than a
/// game-local wrapper.
///
/// It starts [`AuthorityPhase::Remote`]: this peer simulates it optimistically
/// and holds nothing until the registrar grants a fence. Until that grant the
/// authority retains no poses for it and refuses every claim against it, which
/// is the correct answer for an entity this node is not authoritative for.
pub fn spawn_predicted(app: &mut App, authority: iroh::PublicKey, persist_id: PersistId) -> Entity {
    app.world_mut()
        .spawn((
            Predicted,
            PredictedPosition::default(),
            PersistIdentity(persist_id),
            AuthorityPhase::Remote,
            PredictedBy {
                authority,
                persist_id,
            },
            CanonicalPose::new(
                orrery_protocol::Tick::new(0),
                PoseSample {
                    position: LatticePoint::default(),
                    hit_radius: HIT_RADIUS_MM,
                },
            ),
        ))
        .id()
}

/// A deterministic secret key, so a sidecar's node id is reproducible across
/// runs of the same scenario.
#[must_use]
pub fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0_u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes)
}
