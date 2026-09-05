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
//!   [`orrery::hit::answer_hit_claims`];
//! - a live world the rules stepped is extracted into `orrery_ipc` frames
//!   every tick (#898 step 3): the shipped sidecar is a frame producer, its
//!   interpolation runs through the basis-exporting path, and
//!   [`orrery::ipc::export_ipc_frames`] reads the presented value-and-basis
//!   pairs out as `SidecarToEngine` batches.
//!
//! The rules themselves are not here. They are `orrery_synthetic`, which is
//! Bevy-free: D42 (a) / D43 (e)(1) keep a `Ruleset` out of any crate with Bevy
//! in its graph, and `scripts/core-gates.sh` discovers and enforces that by
//! name. This crate is the composition — the app, the predicted component, the
//! weapon table — and it is where every Bevy dependency lives.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod serve;

use std::collections::BTreeMap;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;
use lightyear::prelude::{
    AppComponentExt, Diffable, Interpolated, LocalTimeline, Predicted, PredictionBuilderExt,
};
use serde::{Deserialize, Serialize};

use orrery::hit::{CanonicalPose, OrreryHitRegistrationPlugin};
use orrery::ipc::{OrreryIpcExportPlugin, PresentationFrame};
use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_authority::{AuthorityPhase, HitRules, PersistIdentity, PoseSample};
use orrery_core::{tick_rng, OrderedInputs, Ruleset, StateView};
use orrery_ipc::QuantizedTransform;
use orrery_net::plugin::NetConfig;
use orrery_predict::{
    AppInterpolationBasisExt as _, AppReconciliationExt, InterpolateWithBasis, PredictedBy,
    ReconciliationResidual, TickBridge,
};
use orrery_protocol::{LatticePoint, PersistId, QuantizedDir, UniverseSeed, WeaponRef};
use orrery_synthetic::{Synthetic, SyntheticState};

pub use serve::{IpcServer, OrreryIpcServePlugin, ServeStats};

/// How often the sidecar presents, when it is driven by its own runner.
///
/// Not the canonical rate: the ruleset steps on Bevy's fixed timestep and is
/// untouched by this. This is how often `Update` runs, and therefore how
/// often the extractor produces a frames batch — presentation, which A9 §2.4
/// and D53 clause (f) item 2 both keep firmly on the skin's side of the line.
///
/// Comfortably above the fixed rate so every canonical tick is presented at
/// least once, and low enough that the link carries frames rather than a busy
/// loop's output. A game that drives the app itself sets its own rate and
/// this constant does not apply to it.
pub const PRESENTATION_HZ: f64 = 120.0;

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

impl InterpolateWithBasis for PredictedPosition {
    fn interpolate(
        from: Self,
        to: Self,
        alpha: f32,
        _sample_delta: Option<std::time::Duration>,
    ) -> Self {
        interpolate_position(from, to, alpha)
    }
}

/// The projection the IPC extractor frames this game's presented state with.
///
/// The synthetic entity moves along the lattice's x axis and faces along it;
/// its one integer of state *is* the x translation in millimetres, so the
/// projection is exact. A real game projects whichever component it actually
/// presents.
impl PresentationFrame for PredictedPosition {
    fn frame(&self) -> QuantizedTransform {
        QuantizedTransform {
            translation: LatticePoint::new(self.0, 0, 0),
            forward: QuantizedDir::new(1, 0, 0),
            up: QuantizedDir::new(0, 1, 0),
        }
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
    build_sidecar(secret_key, simulation_enabled, None)
}

/// The same sidecar, additionally serving its extracted batches to one
/// observer on an already-bound listener (#898 step 3).
///
/// The server is bound by the caller rather than here so its address is known
/// before the app runs: a launcher has to print the port it took, and a test
/// has to dial it. Serving changes nothing about the simulation — that is
/// A9 P-4, and `tests/observer_kill.rs` is the proof rather than this
/// sentence.
pub fn sidecar_serving(
    secret_key: iroh::SecretKey,
    simulation_enabled: bool,
    server: IpcServer,
) -> App {
    build_sidecar(secret_key, simulation_enabled, Some(server))
}

fn build_sidecar(
    secret_key: iroh::SecretKey,
    simulation_enabled: bool,
    server: Option<IpcServer>,
) -> App {
    let mut app = App::new();
    // `MinimalPlugins`' schedule runner spins as fast as the machine allows.
    // For an unobserved sidecar that is merely wasteful; for a serving one it
    // is a defect the observer made visible. Extraction runs in `Update`, so
    // the *presentation* rate is the frame rate, not the tick rate: measured
    // free-running on this box, one sidecar emitted 1,725 batches/s at ~248 %
    // CPU against a 64 Hz canonical tick — roughly 27 batches per tick, every
    // one of them a complete extraction on the link.
    //
    // Capping the frame rate is the right lever rather than de-duplicating by
    // tick, because consecutive batches at one tick are *not* redundant: an
    // interpolated entity's alpha advances between them, and that motion is
    // the whole reason the interpolated class is presented at frame rate at
    // all. [`PRESENTATION_HZ`] says what the cap is and why.
    app.add_plugins(
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / PRESENTATION_HZ,
        ))),
    );
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
        .predict()
        .add_correction_fn::<PredictedPosition>(interpolate_position);
    // Interpolation runs through the facade's basis-exporting path, so every
    // presented value is paired with the `RenderedInterpBasis` that produced
    // it — which is what the extractor below frames and what a shooter-side
    // claim is built from (#898 step 4).
    app.interpolate_with_basis::<PredictedPosition>();
    // #898 step 3: the shipped sidecar is a frame producer. Its predicted
    // entities are lightyear's `Predicted` copies, its interpolated ones
    // lightyear's `Interpolated` copies — the markers the game already names.
    app.add_plugins(OrreryIpcExportPlugin::<
        Predicted,
        Interpolated,
        PredictedPosition,
    >::new());
    // …and, when a listener was bound, the batches leave the process. The
    // publisher is instantiated with the same three types as the extractor so
    // it can be ordered after the exact system that writes what it reads.
    if let Some(server) = server {
        app.add_plugins(OrreryIpcServePlugin::<
            Predicted,
            Interpolated,
            PredictedPosition,
        >::new(server));
    }
    app.track_reconciliation::<PredictedPosition>();
    app.insert_resource(SimulationEnabled(simulation_enabled));
    app.init_resource::<StepTrace>();
    app.add_systems(FixedUpdate, step_synthetic_rules);
    app.finish();
    // …and `cleanup()`, which is not decoration. `ScheduleRunnerPlugin`'s
    // runner re-runs `finish()` on any app whose plugin state is not
    // `Cleaned`, and `RepliconSharedPlugin::finish` *removes* the
    // `ProtocolHasher` it consumes (`vendor/bevy_replicon/src/shared.rs:124-127`),
    // so a second pass panics on the `expect` there. Every test drives the app
    // with `update()` and never met it; `orrery-sidecar` calls `run()` and met
    // it on its first tick, which is the shipped binary failing at startup for
    // as long as it has existed. Advancing to `Cleaned` here is what makes the
    // runner skip the second pass.
    app.cleanup();
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

/// Present a second entity through the **interpolated** class, so an observer
/// has one capsule of each timeline to render (#898 step 3).
///
/// # What is real here and what is not — read this before citing it
///
/// Real: everything downstream of the snapshots. The entity carries
/// lightyear's own `Interpolated` marker and a real `ConfirmedHistory`;
/// `orrery_predict`'s basis-exporting pipeline samples that history at the
/// interpolation timeline's clock and co-produces the presented value with
/// the `RenderedInterpBasis` that produced it; the extractor frames that pair
/// and nothing else. The bracket an observer receives is a genuine bracket
/// and the alpha a genuine alpha.
///
/// **Not real: the peer.** The snapshots are written by [`feed_stand_in`]
/// below, on this process's own clock, rather than delivered by replication
/// from another node. So this proves the *presentation and extraction* path
/// for the interpolated class end to end; it does **not** prove that a
/// replicated peer produces an `Interpolated` copy over the facade's link.
/// Nothing in the tree proves that yet, and a demo built on this must not be
/// described as two peers.
pub fn spawn_stand_in_remote(app: &mut App, persist_id: PersistId) -> Entity {
    app.add_systems(FixedUpdate, feed_stand_in);
    app.world_mut()
        .spawn((
            Interpolated,
            PredictedPosition::default(),
            PersistIdentity(persist_id),
            lightyear::prelude::ConfirmedHistory::<PredictedPosition>::default(),
        ))
        .id()
}

/// How far ahead of the interpolation clock the stand-in keeps a snapshot.
///
/// The interpolated timeline runs behind the local one; a bracket must exist
/// on both sides of it or the pipeline has nothing to interpolate between and
/// exports no basis. Six ticks is a tenth of a second at 60 Hz.
const STAND_IN_LEAD: u32 = 6;

/// Keep a bracket ahead of the interpolation clock for every stand-in entity.
///
/// The value is a plain ramp on the lattice's x axis. It is not produced by
/// `Synthetic::step`, and deliberately so: this is a stand-in for snapshots
/// that would have arrived over a link, and dressing it as canonical output
/// would be the lie [`spawn_stand_in_remote`]'s docs are written to prevent.
fn feed_stand_in(
    timeline: Res<LocalTimeline>,
    mut entities: Query<&mut lightyear::prelude::ConfirmedHistory<PredictedPosition>>,
) {
    let now = timeline.tick();
    let ahead = lightyear::core::tick::Tick(now.0.wrapping_add(STAND_IN_LEAD));
    for mut history in &mut entities {
        history.insert_explicit(
            ahead,
            lightyear::core::history_buffer::HistoryState::Updated(PredictedPosition(i64::from(
                ahead.0,
            ))),
        );
    }
}

/// A deterministic secret key, so a sidecar's node id is reproducible across
/// runs of the same scenario.
#[must_use]
pub fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0_u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes)
}
