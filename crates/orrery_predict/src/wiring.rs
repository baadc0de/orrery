//! The lightyear 0.29 configuration layer (D8, D15, docs/10-crates.md §7).
//!
//! Everything in this module names a lightyear type, and nothing outside this
//! crate does — layering rule 3, the plan-B blast radius. Read it as the answer
//! to one question: *which lightyear knob is each D16 number?* That mapping is
//! not obvious, it moved in 0.29, and writing it down is most of the value here.
//!
//! | D16 | lightyear 0.29 |
//! |---|---|
//! | 60 Hz sim tick | `ClientPlugins { tick_duration }`, which sets `Time<Fixed>` and the `TickDuration` resource |
//! | 20 Hz send | `ReplicationMetadata::new(50 ms)` — app-global in 0.29; `ReplicationSender` became a unit marker with no interval of its own |
//! | 9-tick rollback window | `RollbackPolicy::max_rollback_ticks`, on the `PredictionManager` resource |
//! | 100 ms interpolation buffer | `InterpolationConfig::min_delay`, with `send_interval_ratio` zeroed so the delay is the fixed figure D8 specifies rather than a multiple of the observed send rate |
//! | 20-tick input redundancy | `InputConfig::packet_redundancy` |
//!
//! Two of those need justifying, because the lightyear default is *not* wrong,
//! it is answering a different question.
//!
//! `InterpolationConfig` defaults to `send_interval × 1.7`, which adapts to the
//! peer's actual send rate — sensible for a client-server game with one server.
//! Orrery interpolates entities from many authorities at different rates, and
//! D8 fixes the buffer at two send intervals of the *configured* rate so the
//! view delay is a property of the game, not of whichever peer is currently
//! slowest. `orrery_net`'s per-link jitter estimator is what widens it
//! (docs/05 §5), and it cannot do that against a moving baseline.
//!
//! `RollbackPolicy::max_rollback_ticks` defaults to 20. D8 says 9, and the
//! difference is not caution: 20 ticks at the ≈ 1 ms step target is 20 ms of
//! replay, which does not fit two render frames, so the guard in [`budget`]
//! would be in its eviction rung on every ordinary correction. The effective
//! bound is `min(max_rollback_ticks, InputTimelineConfig::maximum_predicted_ticks)`,
//! so this crate sets both.
//!
//! ## What lightyear 0.29 does not provide
//!
//! **Per-entity authority.** `lightyear_replication`'s own documentation says
//! so: *"Authority is currently not working since replicon only supports server
//! to client replication"* (`lightyear_replication-0.29.0/src/lib.rs:67`).
//! `HasAuthority`, `AuthorityBroker`, `GiveAuthority` and `RequestAuthority`
//! exist as types; the machinery behind them does not run. D7's leases and D8's
//! per-entity reconciliation are therefore `orrery_authority`'s to implement in
//! full, with lightyear supplying prediction *mechanics* only. This is R-1/R-2
//! arriving in the form the risk register anticipated — not a build failure, a
//! capability gap — and it is why the plan-B seam is drawn where it is.
//!
//! **A rollback signal.** `lightyear_prediction` fires no event, trigger or
//! observer on rollback, and `PredictionMetrics` counts rollbacks globally with
//! no entity attribution. The per-entity residual reaches this crate through
//! `VisualCorrection<D>`, which lightyear adds to a mispredicted entity after
//! `RollbackSystems::EndRollback` and which carries the error itself. That is
//! what [`AppReconciliationExt::track_reconciliation`] hooks, and it is why a
//! game must register correction for a component before its residuals become
//! witness evidence.
//!
//! [`budget`]: crate::budget

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_platform::time::Instant;
use lightyear::core::tick::TickDuration;
use lightyear::prelude::client::ClientPlugins;
use lightyear::prelude::client::InputDelayConfig;
use lightyear::prelude::input::InputConfig;
use lightyear::prelude::{InputTimelineConfig, SyncConfig};
use lightyear::prelude::{
    InterpolationConfig, LocalTimeline, Predicted, PredictionManager, PredictionMetrics,
    ReplicationMetadata, RollbackMode, RollbackPolicy, RollbackSystems, VisualCorrection,
};
use orrery_protocol::{NodeId, PersistId};
use tracing::warn;

use crate::budget::{ResimPlan, RollbackBudget};
use crate::config::PredictConfig;
use crate::monitor::{DegradedReason, ReconciliationMonitor, TrackKey, WitnessConfidence};
use crate::tick::TickBridge;

/// Ties a locally predicted entity to the authority whose claims it is
/// reconciled against.
///
/// `orrery_authority` owns the claim state machine and populates this;
/// `orrery_predict` only reads it, because a residual with no authority
/// attached is a number with nobody to attribute it to — and attribution is the
/// entire point of the monitor (D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct PredictedBy {
    /// The peer holding authority over this entity.
    pub authority: NodeId,
    /// The entity's cluster-minted persistent id, which is what evidence and
    /// journal records reference — never a Bevy `Entity`.
    pub persist_id: PersistId,
}

impl PredictedBy {
    /// The monitor key for this entity.
    #[must_use]
    pub const fn track_key(&self) -> TrackKey {
        TrackKey {
            authority: self.authority,
            entity: self.persist_id,
        }
    }
}

/// A lightyear correction value this crate can read as a reconciliation
/// residual.
///
/// lightyear's `VisualCorrection<D>` carries the mispredict error in the
/// component's own type — a `Transform`'s error is a `Transform`, an
/// `avian3d::LinearVelocity`'s error is a velocity. Only the game knows which
/// of its components carries position and which carries velocity, and in what
/// units, so the projection onto the quantization lattice is its to define.
///
/// Both methods default to zero so a component that carries only one of the two
/// implements only that one.
pub trait ReconciliationResidual: Send + Sync + 'static {
    /// Positional error magnitude, in millimetres on the quantization lattice
    /// (docs/05 §9: the same bits the authority sent).
    fn pos_error_mm(&self) -> i64 {
        0
    }

    /// Velocity error magnitude, in millimetres per second.
    fn vel_error_mms(&self) -> i64 {
        0
    }
}

/// Registers a predicted component's corrections as witness evidence.
pub trait AppReconciliationExt {
    /// Feed [`ReconciliationMonitor`] from `D`'s post-rollback corrections.
    ///
    /// `D` is the type lightyear's `VisualCorrection<D>` carries for the
    /// component, which for the common `add_correction()` registration is the
    /// component itself. Registering a component here without also registering
    /// correction for it on lightyear's side is silent: the query simply never
    /// matches, because the marker is never added.
    fn track_reconciliation<D>(&mut self) -> &mut Self
    where
        D: ReconciliationResidual + Component;
}

impl AppReconciliationExt for App {
    fn track_reconciliation<D>(&mut self) -> &mut Self
    where
        D: ReconciliationResidual + Component,
    {
        self.add_systems(
            PostUpdate,
            feed_residuals::<D>.after(RollbackSystems::VisualCorrection),
        )
    }
}

/// System sets this crate adds, so a game can order against them by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SystemSet)]
pub enum PredictSystems {
    /// Samples the cost of one predicted-subset fixed step.
    MeasureStep,
    /// Applies the D8 degradation ladder to lightyear's rollback bound.
    EnforceBudget,
    /// Turns post-rollback corrections into monitor residuals.
    FeedMonitor,
}

/// Wall-clock start of the current fixed step, for the cost EWMA.
#[derive(Debug, Clone, Copy, Resource)]
struct StepStart(Instant);

/// Rollback count observed on the previous frame, so the guard can tell a
/// frame that rolled back from one that did not — lightyear exposes the count
/// and nothing else.
#[derive(Debug, Default, Clone, Copy, Resource)]
struct LastRollbackCount(u32);

/// Install lightyear's client stack, configured from `cfg`.
///
/// Only the client half: `ClientPlugins` is what carries `PredictionPlugin`,
/// and prediction is the half of the stack D8 assigns to this crate. The
/// authority half — a peer serving the entities it owns — is `orrery_authority`
/// and `orrery_net`'s, and cannot be lightyear's while lightyear's own
/// authority machinery is documented as not working (see the module docs).
pub(crate) fn install(app: &mut App, cfg: &PredictConfig) {
    app.add_plugins(ClientPlugins {
        tick_duration: cfg.tick_duration(),
    });

    // `ClientPlugins` seeds each of these; overriding afterwards is the
    // documented order, and the reverse would be silently ignored.
    app.insert_resource(TickDuration(cfg.tick_duration()));
    app.insert_resource(ReplicationMetadata::new(cfg.send_interval()));
    app.insert_resource(
        InterpolationConfig::default()
            .with_min_delay(cfg.interp_buffer)
            // Zeroed deliberately: see the module docs. With the default 1.7
            // the buffer would track whichever authority is sending slowest.
            .with_send_interval_ratio(0.0),
    );

    // The effective rollback bound is the minimum of these two, so D8's window
    // has to be written to both or the smaller default silently wins.
    app.insert_resource(InputTimelineConfig::new(
        SyncConfig::default(),
        InputDelayConfig {
            // No input delay, ever. lightyear's default trades up to 3 ticks of
            // it away to avoid rollbacks, which is the right call for a
            // client-server game where every entity is remote. In Orrery the
            // own player is locally authoritative and RTT-free by construction
            // (docs/05 §2, case 1); buying fewer rollbacks with input latency
            // would spend the one thing the architecture gives away for free.
            minimum_input_delay_ticks: 0,
            maximum_input_delay_before_prediction: 0,
            maximum_predicted_ticks: cfg.rollback_ticks,
        },
    ));
    app.insert_resource(PredictionManager {
        rollback_policy: RollbackPolicy {
            state: RollbackMode::Check,
            input: RollbackMode::Check,
            max_rollback_ticks: cfg.rollback_ticks,
        },
        ..PredictionManager::default()
    });

    app.insert_resource(StepStart(Instant::now()))
        .init_resource::<LastRollbackCount>();

    app.add_systems(
        FixedFirst,
        mark_step_start.in_set(PredictSystems::MeasureStep),
    );
    app.add_systems(FixedLast, measure_step.in_set(PredictSystems::MeasureStep));
    app.add_systems(
        PostUpdate,
        enforce_rollback_budget
            .in_set(PredictSystems::EnforceBudget)
            .after(RollbackSystems::VisualCorrection),
    );
    app.configure_sets(
        PostUpdate,
        PredictSystems::FeedMonitor.after(PredictSystems::EnforceBudget),
    );
}

/// The input configuration D8 §4 asks for, for a game's action type `A`.
///
/// `orrery_predict` cannot install this itself: lightyear's `InputPlugin<A>` is
/// generic over the game's action type, and inserting `InputConfig<A>` requires
/// naming it. So the crate supplies the *values* and the game supplies the type:
///
/// ```ignore
/// app.add_plugins(lightyear::prelude::input::native::InputPlugin::<MyAction> {
///     config: orrery_predict::wiring::input_config(&cfg),
/// });
/// ```
///
/// The redundancy conversion is the part worth reading. D16 states the
/// redundancy cap in **ticks** (20, ~333 ms at 60 Hz); lightyear's
/// `packet_redundancy` counts **packets** — "a value of 3 means each input
/// packet will contain the inputs for the 3 last packets". At 60 Hz sim over a
/// 20 Hz send there are three ticks per packet, so D16's 20 ticks is seven
/// packets, not twenty. Writing 20 here would carry ~1 s of input history in
/// every datagram for no benefit.
#[must_use]
pub fn input_config<A>(cfg: &PredictConfig) -> InputConfig<A> {
    let ticks_per_packet = (cfg.tick_hz.max(1) / cfg.send_hz.max(1)).max(1);
    let packets = cfg
        .redundant_input_ticks
        .div_ceil(u16::try_from(ticks_per_packet).unwrap_or(1).max(1));
    InputConfig {
        // The shooter's interpolation delay has to reach the entity's authority
        // for docs/05 §7's rewind-cap check to mean anything: the authority
        // re-derives the pose the shooter actually saw, and it cannot do that
        // without knowing how far behind that view was.
        lag_compensation: true,
        packet_redundancy: packets.max(1),
        send_interval: cfg.send_interval(),
        ignore_rollbacks: false,
        rebroadcast_inputs: false,
        ..InputConfig::default()
    }
}

fn mark_step_start(mut start: ResMut<StepStart>) {
    start.0 = Instant::now();
}

fn measure_step(start: Res<StepStart>, mut budget: ResMut<RollbackBudget>) {
    budget.observe_step(start.0.elapsed());
}

/// Apply the D8 degradation ladder to lightyear's rollback bound.
///
/// lightyear owns the replay loop and will not ask permission before running
/// it, so the guard cannot gate a rollback directly. What it *can* do is set
/// the bound: lightyear ignores rollback requests beyond
/// `max_rollback_ticks`, and an ignored request becomes exactly what D8 asks
/// for beyond the window — a snap, reconciled with presentation-side error
/// smoothing. Shrinking the bound when the measured step cost says the full
/// window will not fit is therefore the ladder, enforced through the only
/// lever lightyear exposes.
/// The read-mostly half of [`enforce_rollback_budget`]'s inputs, bundled so the
/// system stays under clippy's argument ceiling.
#[derive(SystemParam)]
struct BudgetSignal<'w> {
    cfg: Res<'w, PredictConfig>,
    metrics: Res<'w, PredictionMetrics>,
    last: ResMut<'w, LastRollbackCount>,
    time: Res<'w, bevy_time::Time>,
}

fn enforce_rollback_budget(
    mut signal: BudgetSignal,
    mut budget: ResMut<RollbackBudget>,
    mut monitor: ResMut<ReconciliationMonitor>,
    mut manager: ResMut<PredictionManager>,
    predicted: Query<(), With<Predicted>>,
) {
    let cfg = &signal.cfg;
    let rolled_back = signal.metrics.rollbacks > signal.last.0;
    signal.last.0 = signal.metrics.rollbacks;

    if !rolled_back {
        // A frame with no rollback is what lets the hysteresis cap expire; a
        // guard that only ever measured bad frames would never give the
        // predicted set back.
        budget.observe_clean_frame(signal.time.delta());
        manager.rollback_policy.max_rollback_ticks = cfg.rollback_ticks;
        return;
    }

    let predicted_len = u16::try_from(predicted.iter().count()).unwrap_or(u16::MAX);
    let plan = budget.plan(cfg.rollback_ticks, predicted_len);
    match plan {
        ResimPlan::Immediate { .. } | ResimPlan::Amortize { .. } => {
            manager.rollback_policy.max_rollback_ticks = cfg.rollback_ticks;
        }
        ResimPlan::Evict { demote, ticks_now } => {
            // Only on the transition, not every frame of a sustained overrun:
            // a log line per frame under load is the one thing guaranteed to
            // make the load worse.
            if monitor.confidence() == WitnessConfidence::Full {
                warn!(
                    demote,
                    window = ticks_now,
                    step_cost_us = budget.step_cost.as_micros() as u64,
                    "rollback budget exceeded; narrowing the window and shedding predicted entities"
                );
            }
            // Narrowing the window turns the unaffordable tail of the replay
            // into a snap, which is bounded work, instead of a frame that
            // arrives late and makes the next replay longer.
            manager.rollback_policy.max_rollback_ticks = ticks_now;
            monitor.degrade(DegradedReason::BudgetEviction);
        }
        ResimPlan::SnapOwnPlayer => {
            if monitor.confidence() == WitnessConfidence::Full {
                warn!(
                    step_cost_us = budget.step_cost.as_micros() as u64,
                    "rollback budget floor reached; every correction now snaps the own player"
                );
            }
            manager.rollback_policy.max_rollback_ticks = 0;
            monitor.degrade(DegradedReason::BudgetEviction);
        }
    }
}

/// Turn `D`'s post-rollback corrections into monitor residuals.
///
/// `Added` rather than a plain query: `VisualCorrection` decays over several
/// frames after the rollback, and sampling it every frame would count one
/// mispredict as a sustained run — manufacturing the violation the monitor
/// exists to detect honestly.
fn feed_residuals<D>(
    corrections: Query<(&PredictedBy, &VisualCorrection<D>), Added<VisualCorrection<D>>>,
    timeline: Res<LocalTimeline>,
    bridge: Res<TickBridge>,
    mut monitor: ResMut<ReconciliationMonitor>,
) where
    D: ReconciliationResidual + Component,
{
    let tick = bridge.resolve(timeline.tick().0);
    for (predicted_by, correction) in &corrections {
        let key = predicted_by.track_key();
        monitor.record_rollback(key);
        monitor.record_residual(
            key,
            tick,
            correction.error.pos_error_mm(),
            correction.error.vel_error_mms(),
        );
    }
}
