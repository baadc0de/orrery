//! End-to-end checks that the two guards are wired to lightyear rather than
//! sitting beside it (docs/05-prediction-rollback.md §3, §10).
//!
//! Both of these were resources nothing fed before this crate had a lightyear
//! dependency, and "the plugin inserts them" is not evidence that anything
//! reaches them. These tests drive the real systems in a real `App`, through
//! the exact lightyear types the running stack uses: a `VisualCorrection` is
//! what lightyear adds to a mispredicted entity, and `PredictionMetrics` is the
//! only rollback signal it exposes.
//!
//! What they deliberately do *not* do is stand up a connection. lightyear's
//! `check_rollback` is gated on `NetworkingMetadata` reporting a connected
//! client or P2P topology, so a genuine rollback needs two peers and a link —
//! that is the P3 island harness's job, not a unit test's. What is testable
//! here is everything between lightyear's signal and Orrery's evidence, which
//! is the part this crate owns.

use std::time::Duration;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use lightyear::prelude::{PredictionManager, PredictionMetrics, VisualCorrection};
use orrery_predict::monitor::{DegradedReason, WitnessConfidence};
use orrery_predict::wiring::{AppReconciliationExt, PredictedBy, ReconciliationResidual};
use orrery_predict::{
    MonitorSignal, OrreryPredictPlugin, PredictConfig, ReconciliationMonitor, RollbackBudget,
    TickBridge, TrackKey,
};
use orrery_protocol::{NodeId, PersistId, Tick};

/// A stand-in for a game's predicted pose component. Real games implement
/// [`ReconciliationResidual`] on whatever carries position and velocity; the
/// crate cannot know which component that is, or in what units.
#[derive(Debug, Clone, Copy, Component)]
struct Pose {
    pos_mm: i64,
    vel_mms: i64,
}

impl ReconciliationResidual for Pose {
    fn pos_error_mm(&self) -> i64 {
        self.pos_mm
    }

    fn vel_error_mms(&self) -> i64 {
        self.vel_mms
    }
}

fn node(discriminant: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = 1;
    bytes[31] = discriminant;
    for candidate in 0..=u8::MAX {
        bytes[30] = candidate;
        if let Ok(key) = NodeId::from_bytes(&bytes) {
            return key;
        }
    }
    panic!("no valid key for discriminant {discriminant}");
}

fn app(config: PredictConfig) -> App {
    let mut app = App::new();
    app.add_plugins(bevy_app::PanicHandlerPlugin);
    app.add_plugins(bevy_time::TimePlugin);
    app.add_plugins(bevy_state::app::StatesPlugin);
    app.add_plugins(OrreryPredictPlugin { config });
    app.track_reconciliation::<Pose>();
    app.finish();
    app
}

/// The headline claim: a lightyear mispredict becomes a monitor residual
/// attributed to the authority that caused it, with no game code in between.
#[test]
fn a_lightyear_correction_becomes_attributed_witness_evidence() {
    let mut app = app(PredictConfig::default());
    let authority = node(1);
    let persist_id = PersistId(77);

    app.world_mut().spawn((
        PredictedBy {
            authority,
            persist_id,
        },
        // What lightyear adds to a mispredicted entity after
        // `RollbackSystems::EndRollback`: the error, in the component's units.
        VisualCorrection {
            error: Pose {
                pos_mm: 42,
                vel_mms: 7,
            },
        },
    ));
    app.update();

    let monitor = app.world().resource::<ReconciliationMonitor>();
    let key = TrackKey {
        authority,
        entity: persist_id,
    };
    let track = monitor
        .track(&key)
        .copied()
        .expect("the correction should have opened a track for this authority");
    assert_eq!(track.rollbacks, 1, "one correction is one rollback");
    assert_eq!(track.pos_ewma_mm, 42);
    assert_eq!(track.vel_ewma_mms, 7);
    assert_eq!(
        track.violation_start,
        Some(Tick(0)),
        "42 mm is outside the 10 mm band, so a run opened"
    );
}

/// `VisualCorrection` decays over several frames after a rollback. Sampling it
/// every frame would turn one mispredict into a sustained run and manufacture
/// the violation the monitor exists to detect honestly — the R-6 failure mode.
#[test]
fn a_decaying_correction_is_counted_once_not_every_frame() {
    let mut app = app(PredictConfig::default());
    let authority = node(2);
    let persist_id = PersistId(9);

    app.world_mut().spawn((
        PredictedBy {
            authority,
            persist_id,
        },
        VisualCorrection {
            error: Pose {
                pos_mm: 50,
                vel_mms: 0,
            },
        },
    ));
    for _ in 0..30 {
        app.update();
    }

    let monitor = app.world().resource::<ReconciliationMonitor>();
    let track = monitor
        .track(&TrackKey {
            authority,
            entity: persist_id,
        })
        .copied()
        .expect("track");
    assert_eq!(track.rollbacks, 1, "thirty frames, one mispredict");
    assert_eq!(
        track.violation_ticks, 1,
        "one sample cannot be a sustained violation"
    );
}

/// Attribution is not optional: a correction on an entity with no authority
/// recorded is a number with nobody to attribute it to, and inventing an
/// attribution is how honest peers get accused.
#[test]
fn a_correction_without_an_authority_is_not_evidence() {
    let mut app = app(PredictConfig::default());
    app.world_mut().spawn(VisualCorrection {
        error: Pose {
            pos_mm: 5_000,
            vel_mms: 0,
        },
    });
    app.update();
    assert!(app.world().resource::<ReconciliationMonitor>().is_empty());
}

/// The budget guard has to reach lightyear, and there is exactly one lever:
/// `max_rollback_ticks`. lightyear ignores rollback requests beyond it, which
/// is D8's "beyond the window, snap + reconcile". A machine whose measured step
/// cost cannot pay for the full window must therefore see the window narrow.
#[test]
fn an_unaffordable_step_cost_narrows_lightyears_rollback_window() {
    let mut app = app(PredictConfig::default());

    // A frame with no rollback leaves the configured window in place.
    app.update();
    assert_eq!(
        app.world()
            .resource::<PredictionManager>()
            .rollback_policy
            .max_rollback_ticks,
        9,
        "a clean frame keeps D8's window"
    );

    // Now: one predicted entity, a step that costs four times its target, and
    // a rollback this frame.
    app.world_mut().spawn(lightyear::prelude::Predicted);
    app.world_mut().resource_mut::<RollbackBudget>().step_cost = Duration::from_millis(4);
    app.world_mut()
        .resource_mut::<PredictionMetrics>()
        .rollbacks += 1;
    app.update();

    let narrowed = app
        .world()
        .resource::<PredictionManager>()
        .rollback_policy
        .max_rollback_ticks;
    assert!(
        narrowed < 9,
        "a 36 ms replay does not fit two render frames; window stayed at {narrowed}"
    );

    // And the peer must say so: a machine that cannot afford prediction is not
    // a high-confidence witness (docs/05 §10).
    assert_eq!(
        app.world().resource::<ReconciliationMonitor>().confidence(),
        WitnessConfidence::Reduced(DegradedReason::BudgetEviction),
    );
}

/// The window must come back. A guard that narrowed permanently would leave a
/// machine that had one bad second predicting nine ticks less for the rest of
/// the session.
#[test]
fn the_window_is_restored_once_frames_are_clean_again() {
    let mut app = app(PredictConfig::default());
    app.world_mut().spawn(lightyear::prelude::Predicted);
    app.world_mut().resource_mut::<RollbackBudget>().step_cost = Duration::from_millis(4);
    app.world_mut()
        .resource_mut::<PredictionMetrics>()
        .rollbacks += 1;
    app.update();
    assert!(
        app.world()
            .resource::<PredictionManager>()
            .rollback_policy
            .max_rollback_ticks
            < 9
    );

    app.update();
    assert_eq!(
        app.world()
            .resource::<PredictionManager>()
            .rollback_policy
            .max_rollback_ticks,
        9,
        "no rollback this frame, so the full window is available again"
    );
}

/// Residuals are stamped in universe ticks, not lightyear's. Everything
/// downstream — signed logs, adjudication windows, journal records — indexes on
/// the universe timeline, and a report carrying a session-relative tick would
/// point an adjudicator at the wrong moment in history.
#[test]
fn residuals_are_stamped_on_the_universe_timeline() {
    let mut app = app(PredictConfig::default());
    // What `orrery_net` does after the sync phase converges.
    app.world_mut()
        .insert_resource(TickBridge::anchor(Tick(9_000_000_000), 0));

    let authority = node(3);
    let persist_id = PersistId(5);
    app.world_mut().spawn((
        PredictedBy {
            authority,
            persist_id,
        },
        VisualCorrection {
            error: Pose {
                pos_mm: 1_000,
                vel_mms: 0,
            },
        },
    ));
    app.update();

    let monitor = app.world().resource::<ReconciliationMonitor>();
    let track = monitor
        .track(&TrackKey {
            authority,
            entity: persist_id,
        })
        .copied()
        .expect("track");
    assert_eq!(
        track.last_tick,
        Some(Tick(9_000_000_000)),
        "stamped against the universe epoch, not lightyear's session tick"
    );
}

/// A hard snap is evidence on its own tick, and the signal has to be the one
/// `orrery_witness` consumes — including the disputed window, which is what a
/// log-segment request is scoped by.
#[test]
fn a_hard_snap_produces_a_witness_signal_with_a_bounded_window() {
    let mut monitor = ReconciliationMonitor::default();
    let key = TrackKey {
        authority: node(4),
        entity: PersistId(1),
    };
    let signal = monitor
        .record_residual(key, Tick(1_000), 100_000, 0)
        .expect("100 m of error in one tick is not float drift");
    match signal {
        MonitorSignal::SustainedToleranceViolation { window, .. } => {
            assert_eq!(window.start, Tick(1_000));
            assert_eq!(window.end, Tick(1_001));
        }
        other => panic!("expected a tolerance violation, got {other:?}"),
    }
}
