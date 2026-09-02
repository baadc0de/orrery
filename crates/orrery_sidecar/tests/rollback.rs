//! The canonical rules execute inside Lightyear's predicted tick, and the
//! pose the authority retains follows the rollback (#898 step 1, #871).
//!
//! Carried over from the `synthetic_sidecar` example this crate replaced, and
//! extended: it is no longer enough that the replayed tick rewrites a
//! game-local pose component. The replay must reach the *authority's ring*,
//! because that is the value a `HitClaim` is adjudicated against.

mod common;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use lightyear::core::history_buffer::HistoryState;
use lightyear::core::tick::Tick as SessionTick;
use lightyear::prelude::{
    LocalTimeline, PredictionHistory, PredictionMetrics, StateRollbackMetadata, P2P,
};

use common::{grant, session_tick, warm_up, ENTITY};
use orrery_authority::PoseHistory;
use orrery_predict::TickBridge;
use orrery_sidecar::{secret, sidecar, spawn_predicted, PredictedPosition, StepTrace};

fn step(app: &mut App) -> u32 {
    app.update();
    session_tick(app)
}

/// The step-1 proof, plus the ring: the rules-produced position is restored
/// and produced again by the real Lightyear rollback loop, which reruns
/// `FixedMain` — and because the publisher runs inside `FixedMain` too, the
/// authority ends up retaining the *replayed* pose rather than the
/// mispredicted one.
///
/// Mutation check: move `publish_canonical_poses` from `FixedPostUpdate` to
/// `Update` and the final ring assertion fails — the ring keeps the
/// mispredicted 9_00x pose, which is exactly the class of bug that would let a
/// hit be adjudicated against a pose the ruleset never asserted.
#[test]
fn rollback_reexecutes_synthetic_step_and_the_ring_keeps_the_replayed_pose() {
    const ANCHOR_TICK: u32 = 6;
    const PRESENT_TICK: u32 = 9;

    let key = secret(1);
    let authority = key.public();
    let mut app = sidecar(key, true);
    // A declared P2P session is sufficient to turn on Lightyear's real
    // prediction pipeline; #896 already proves the facade's iroh bridge.
    app.world_mut().spawn(P2P);
    warm_up(&mut app);
    let predicted = spawn_predicted(&mut app, authority, ENTITY);
    grant(&mut app, ENTITY);

    let start = session_tick(&app);
    for offset in 1..=ANCHOR_TICK {
        assert_eq!(step(&mut app), start + offset);
    }
    let anchor = *app
        .world()
        .get::<PredictedPosition>(predicted)
        .expect("predicted position at the anchor");

    // This is the only position the test writes. The wrong future stored for
    // the next three ticks is 9_001..9_003, which only `Synthetic::step`
    // produces from it.
    app.world_mut()
        .entity_mut(predicted)
        .insert(PredictedPosition(9_000));
    for offset in (ANCHOR_TICK + 1)..=PRESENT_TICK {
        assert_eq!(step(&mut app), start + offset);
    }
    let before_rollback = app.world().resource::<StepTrace>().0.clone();
    assert!(
        before_rollback
            .iter()
            .any(|entry| entry.position_mm > 9_000),
        "the mispredicted future must have been produced by `Synthetic::step`"
    );

    // The mispredicted pose really did reach the authority: without this the
    // final assertion could pass on a ring that was never wrong.
    let bridge_anchor = app
        .world()
        .resource::<TickBridge>()
        .resolve(start + ANCHOR_TICK);
    let mispredicted_tick = app
        .world()
        .resource::<TickBridge>()
        .resolve(start + PRESENT_TICK);
    assert!(
        app.world()
            .resource::<PoseHistory>()
            .pose_at(ENTITY, mispredicted_tick)
            .is_some_and(|pose| pose.position.x > 9_000),
        "the ring must be holding the mispredicted pose before the rollback"
    );

    // Deposit the captured rules-produced anchor as the correction, drop the
    // wrong future from Lightyear's ring, and ask the real rollback manager to
    // replay to the present. No ordinary tick runs in this update, so every
    // new trace entry is from rollback re-execution.
    {
        let mut entity = app.world_mut().entity_mut(predicted);
        *entity
            .get_mut::<PredictedPosition>()
            .expect("predicted position") = anchor;
        let mut history = entity
            .get_mut::<PredictionHistory<PredictedPosition>>()
            .expect("Lightyear installed prediction history");
        let anchor_tick = SessionTick(start + ANCHOR_TICK);
        history.clear_after_tick(anchor_tick);
        history.add_update(anchor_tick, anchor);
    }
    app.world_mut()
        .resource_mut::<StateRollbackMetadata>()
        .request_forced_rollback(SessionTick(start + ANCHOR_TICK));
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(0));
    app.update();

    assert_eq!(
        app.world().resource::<PredictionMetrics>().rollbacks,
        1,
        "Lightyear did not execute the requested misprediction rollback"
    );
    assert_eq!(
        app.world().resource::<LocalTimeline>().tick().0,
        start + PRESENT_TICK,
        "the rollback must replay to the present without advancing it"
    );

    let trace = &app.world().resource::<StepTrace>().0;
    let replay = &trace[before_rollback.len()..];
    assert_eq!(
        replay.len(),
        usize::try_from(PRESENT_TICK - ANCHOR_TICK).unwrap(),
        "each rewound tick must execute `Synthetic::step` exactly once"
    );
    for (entry, offset) in replay.iter().zip((ANCHOR_TICK + 1)..=PRESENT_TICK) {
        assert_eq!(entry.tick, start + offset);
        assert_eq!(
            entry.position_mm,
            anchor.0 + i64::from(offset - ANCHOR_TICK),
            "the replayed value must come from `Synthetic::step` over the captured anchor"
        );
        assert_eq!(
            entry.pose.position.x, entry.position_mm,
            "the replayed tick must also rewrite its canonical pose"
        );
    }

    let retained_present = app
        .world()
        .get::<PredictionHistory<PredictedPosition>>(predicted)
        .expect("prediction history after replay")
        .buffer()
        .iter()
        .find(|(tick, _)| tick.0 == start + PRESENT_TICK)
        .map(|(_, state)| state)
        .expect("the replay must rewrite the present-tick history entry");
    assert_eq!(
        retained_present,
        &HistoryState::Updated(PredictedPosition(
            anchor.0 + i64::from(PRESENT_TICK - ANCHOR_TICK)
        )),
        "the retained post-rollback value must be the rules-produced replay result"
    );

    // The point of carrying this test into the sidecar: the authority's ring
    // was holding the 9_00x misprediction a moment ago and now holds the
    // replayed, rules-produced pose for the same universe tick. A claim
    // arriving after the rollback is adjudicated against the corrected pose.
    let history = app.world().resource::<PoseHistory>();
    assert_eq!(
        history
            .pose_at(ENTITY, mispredicted_tick)
            .expect("the present tick is still retained")
            .position
            .x,
        anchor.0 + i64::from(PRESENT_TICK - ANCHOR_TICK),
        "the ring must follow the rollback, not the misprediction"
    );
    assert_eq!(
        history
            .pose_at(ENTITY, bridge_anchor)
            .expect("the anchor tick is still retained")
            .position
            .x,
        anchor.0,
        "the anchor the replay started from is untouched by it"
    );

    // And not only the present tick: every tick the replay re-executed must
    // have had its retained pose rewritten. A ring that corrected only its
    // newest entry would still hand a claim with an older basis the pose from
    // the future that never happened.
    for offset in (ANCHOR_TICK + 1)..=PRESENT_TICK {
        let universe = app.world().resource::<TickBridge>().resolve(start + offset);
        assert_eq!(
            app.world()
                .resource::<PoseHistory>()
                .pose_at(ENTITY, universe)
                .unwrap_or_else(|| panic!("{universe:?} was replayed and must be retained"))
                .position
                .x,
            anchor.0 + i64::from(offset - ANCHOR_TICK),
            "the replayed pose for {universe:?} must have replaced the mispredicted one"
        );
    }
}
