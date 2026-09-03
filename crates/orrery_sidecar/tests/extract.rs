//! The IPC extractor produces frames from the live world a ruleset stepped
//! (#898 step 3).
//!
//! The distinction this file exists to protect: a test that constructs the
//! frames it then checks proves only the codec, which #898 step 2 already
//! proved. Here nothing about a frame is written by the test. The sidecar's
//! real app runs, `Synthetic::step` is the only producer of the position,
//! Lightyear's real prediction pipeline and rollback loop run, and the
//! extractor's `IpcOutbound` messages are read back exactly as the shipped
//! sidecar emits them. The assertions compare each batch against what the
//! rules produced — the coupling a synthesized batch cannot have.
//!
//! The interpolated class is driven through the same shipped app: an
//! `Interpolated` entity with a real `ConfirmedHistory` is presented by
//! `orrery_predict`'s basis-exporting pipeline, and the extractor must frame
//! exactly the value-and-basis pair that pipeline co-produced.

mod common;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use lightyear::core::history_buffer::HistoryState;
use lightyear::core::tick::Tick as SessionTick;
use lightyear::core::time::{Overstep, TickInstant};
use lightyear::prelude::{
    ConfirmedHistory, Interpolated, InterpolationTimeline, LocalTimeline, NetworkTimeline,
    PredictionHistory, StateRollbackMetadata, P2P,
};

use common::{session_tick, warm_up, ENTITY};
use orrery::ipc::IpcOutbound;
use orrery_ipc::SidecarToEngine;
use orrery_predict::{InterpolateWithBasis as _, ReconciliationMonitor, TickBridge, TrackKey};
use orrery_protocol::{InterpBasis, PersistId, QuantizedDir, Tick, UNorm16};
use orrery_sidecar::{secret, sidecar, spawn_predicted, PredictedPosition, StepTrace};

/// A remote entity only the interpolated class knows about.
const REMOTE: PersistId = PersistId::new(42);

/// Everything the extractor emitted in the update that just ran.
///
/// Messages live two updates, so draining right after the update that wrote
/// them yields exactly that update's batches and nothing else.
fn batches(app: &mut App) -> Vec<SidecarToEngine> {
    app.world_mut()
        .resource_mut::<Messages<IpcOutbound>>()
        .drain()
        .map(|outbound| outbound.0)
        .collect()
}

fn frames_of(batches: &[SidecarToEngine]) -> &orrery_ipc::FrameBatch {
    batches
        .iter()
        .find_map(|batch| match batch {
            SidecarToEngine::Frames(batch) => Some(batch),
            _ => None,
        })
        .expect("every extraction run emits one frames batch")
}

fn spawned(batches: &[SidecarToEngine]) -> &[PersistId] {
    batches
        .iter()
        .find_map(|batch| match batch {
            SidecarToEngine::Spawns(batch) => Some(batch.entities.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

fn despawned(batches: &[SidecarToEngine]) -> &[PersistId] {
    batches
        .iter()
        .find_map(|batch| match batch {
            SidecarToEngine::Despawns(batch) => Some(batch.entities.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

fn corrections(batches: &[SidecarToEngine]) -> Vec<orrery_ipc::CorrectionNotice> {
    batches
        .iter()
        .find_map(|batch| match batch {
            SidecarToEngine::Corrections(batch) => Some(batch.corrections.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The universe tick the sidecar presents on. The bridge is anchored at the
/// universe origin, so it is also the session tick.
fn presented_tick(app: &App) -> orrery_protocol::Tick {
    app.world()
        .resource::<TickBridge>()
        .resolve(app.world().resource::<LocalTimeline>().tick().0)
}

/// The predicted half of the contract, against the world `Synthetic::step`
/// stepped: the spawn batch names the entity once, every frame carries the
/// rules-produced position on an exact basis at the tick it was produced, a
/// despawned entity leaves presentation by name — and nothing here is
/// synthesized beside the check.
#[test]
fn frames_track_the_world_a_ruleset_stepped() {
    let key = secret(41);
    let authority = key.public();
    let mut app = sidecar(key, true);
    // A declared P2P session turns on Lightyear's real prediction pipeline,
    // exactly as the rollback test does.
    app.world_mut().spawn(P2P);
    warm_up(&mut app);
    let _ = batches(&mut app);
    let predicted = spawn_predicted(&mut app, authority, ENTITY);

    // The entity enters presentation: one spawn batch, by stable id.
    app.update();
    let entering = batches(&mut app);
    assert_eq!(
        spawned(&entering),
        &[ENTITY],
        "the extracted spawn batch must name the entity the world gained"
    );

    let first = frames_of(&entering);
    assert!(
        first.interpolated.is_empty(),
        "a locally predicted entity is presented by the predicted class alone"
    );
    assert_eq!(first.extracted_at, presented_tick(&app));
    let [frame] = first.predicted.as_slice() else {
        panic!(
            "exactly one predicted entity is presented: {:?}",
            first.predicted
        );
    };
    assert_eq!(
        frame.persist_id, ENTITY,
        "keyed by stable id, never a Bevy Entity"
    );
    assert_eq!(
        frame.basis,
        InterpBasis::exact(first.extracted_at),
        "a predicted sample names the one tick it was produced on"
    );

    // The frame is the value `Synthetic::step` produced, not a value the
    // schema could have been handed by hand. The step trace is the rules' own
    // record of what they asserted, and the component is the presented state.
    let stepped = app
        .world()
        .resource::<StepTrace>()
        .0
        .last()
        .copied()
        .expect("the ruleset stepped inside the update");
    let presented = app
        .world()
        .get::<PredictedPosition>(predicted)
        .expect("presented component")
        .0;
    assert_eq!(
        frame.transform.translation.x, stepped.position_mm,
        "the frame must carry what the ruleset produced"
    );
    assert_eq!(presented, stepped.position_mm, "fixture sanity");
    assert_eq!(frame.transform.forward, QuantizedDir::new(1, 0, 0));
    assert_eq!(frame.transform.up, QuantizedDir::new(0, 1, 0));

    // Two more canonical ticks: the frames follow the rules, tick for tick,
    // and the presentation set does not respawn.
    for _ in 0..2 {
        app.update();
        let batch = batches(&mut app);
        let frames = frames_of(&batch);
        assert_eq!(frames.extracted_at, presented_tick(&app));
        let [frame] = frames.predicted.as_slice() else {
            panic!("exactly one predicted entity is presented");
        };
        let stepped = app
            .world()
            .resource::<StepTrace>()
            .0
            .last()
            .copied()
            .expect("the ruleset stepped");
        assert_eq!(
            frame.transform.translation.x, stepped.position_mm,
            "each frame carries the position that tick's step produced"
        );
        assert_eq!(
            frame.basis,
            InterpBasis::exact(frames.extracted_at),
            "and the basis is that tick, exactly"
        );
        assert!(
            spawned(&batch).is_empty() && despawned(&batch).is_empty(),
            "a stable presentation set emits no membership batches: {batch:?}"
        );
    }

    // The entity leaves presentation: the despawn batch names it, and the
    // frames stop.
    app.world_mut().despawn(predicted);
    app.update();
    let leaving = batches(&mut app);
    assert_eq!(
        despawned(&leaving),
        &[ENTITY],
        "the extracted despawn batch must name the entity the world lost"
    );
    assert!(
        frames_of(&leaving).predicted.is_empty(),
        "a despawned entity is presented no more"
    );
}

/// An interpolated entity is framed with the value-and-basis pair
/// `orrery_predict`'s pipeline co-produced — the pair a shooter-side claim is
/// built from — and never with a basis recomputed beside the frame.
#[test]
fn an_interpolated_entity_is_framed_with_the_basis_its_value_was_presented_on() {
    let key = secret(42);
    let authority = key.public();
    let mut app = sidecar(key, true);
    app.world_mut().spawn(P2P);
    warm_up(&mut app);
    let _ = batches(&mut app);
    spawn_predicted(&mut app, authority, ENTITY);

    // The snapshots replication would have delivered for a remote entity,
    // bracketing the presentation clock. Their values are the inputs to the
    // real interpolation pipeline; nothing downstream of them is written by
    // this test. The interpolation timeline is advanced in PreUpdate by the
    // frame delta from wherever it was set, so the brackets are absolute and
    // the timeline is parked mid-bracket with a half-tick overstep.
    let history_from = PredictedPosition(10_000);
    let history_to = PredictedPosition(10_006);
    let interpolated = app
        .world_mut()
        .spawn((
            Interpolated,
            PredictedPosition(0),
            orrery_authority::PersistIdentity(REMOTE),
            {
                let mut history = ConfirmedHistory::default();
                history.insert_explicit(SessionTick(6), HistoryState::Updated(history_from));
                history.insert_explicit(SessionTick(12), HistoryState::Updated(history_to));
                history
            },
        ))
        .id();

    // Present one frame: the pipeline samples the history at the
    // interpolation timeline's clock, co-producing the value and its basis.
    app.world_mut()
        .resource_mut::<InterpolationTimeline>()
        .set_now(TickInstant::from_tick_and_overstep(
            SessionTick(9),
            Overstep::from_f32(0.5),
        ));
    app.update();

    let batch = batches(&mut app);
    let frames = frames_of(&batch);
    let [frame] = frames.interpolated.as_slice() else {
        panic!(
            "exactly one interpolated entity is presented: {:?}",
            frames.interpolated
        );
    };
    assert_eq!(frame.persist_id, REMOTE);
    assert!(
        frames
            .predicted
            .iter()
            .all(|frame| frame.persist_id != REMOTE),
        "an interpolated entity is presented by the interpolated class alone"
    );

    // The basis is the one the pipeline exported with the value — read from
    // the component, not rederived here — and the transform is that value.
    let exported = app
        .world()
        .get::<orrery_predict::RenderedInterpBasis>(interpolated)
        .expect("the basis-exporting pipeline co-produced a basis")
        .0;
    assert_eq!(
        frame.basis, exported,
        "the frame carries the exported basis"
    );
    assert_ne!(
        frame.basis.from, frame.basis.to,
        "the history brackets a real interval; an exact basis here would be a synthesized one"
    );
    assert_eq!(
        frame.transform.translation.x,
        app.world()
            .get::<PredictedPosition>(interpolated)
            .expect("presented component")
            .0,
        "the frame carries the presented value"
    );

    // The presented value is the blend the basis describes — strictly between
    // the two snapshots, so a frame copied from either snapshot would fail.
    let alpha = UNorm16::from_f64(4.5 / 6.0);
    let expected =
        PredictedPosition::interpolate(history_from, history_to, alpha.to_f64() as f32, None);
    assert_eq!(frame.transform.translation.x, expected.0);
    assert_ne!(frame.transform.translation.x, history_from.0);
    assert_ne!(frame.transform.translation.x, history_to.0);
}

/// A real Lightyear rollback: the monitor counts the correction it produces,
/// the extractor turns the increment into a notice for the observer, and the
/// frames that follow carry the replayed, rules-produced value — presentation
/// regenerated by overwrite, never un-wound.
#[test]
fn a_real_rollback_is_reported_and_its_frames_regenerated() {
    const ANCHOR_TICK: u32 = 6;
    const PRESENT_TICK: u32 = 9;

    let key = secret(43);
    let authority = key.public();
    let mut app = sidecar(key, true);
    app.world_mut().spawn(P2P);
    warm_up(&mut app);
    let predicted = spawn_predicted(&mut app, authority, ENTITY);

    let start = session_tick(&app);
    for _offset in 1..=ANCHOR_TICK {
        app.update();
    }
    let anchor = *app
        .world()
        .get::<PredictedPosition>(predicted)
        .expect("predicted position at the anchor");

    // This is the only position the test writes; the wrong future stored for
    // the next three ticks is what only `Synthetic::step` produces from it.
    app.world_mut()
        .entity_mut(predicted)
        .insert(PredictedPosition(9_000));
    for _offset in (ANCHOR_TICK + 1)..=PRESENT_TICK {
        app.update();
    }
    // Everything up to here is prologue; the read-back below must be exact.
    let _ = batches(&mut app);

    // Deposit the captured rules-produced anchor as the correction, drop the
    // wrong future from Lightyear's ring, and ask the real rollback manager
    // to replay to the present — the same mechanics the rollback test uses.
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

    // Presentation is regenerated by overwrite: the frames this very run
    // produced already carry the replayed, rules-produced value.
    let replayed = anchor.0 + i64::from(PRESENT_TICK - ANCHOR_TICK);
    let regenerated = batches(&mut app);
    {
        let frames = frames_of(&regenerated);
        let [frame] = frames.predicted.as_slice() else {
            panic!("exactly one predicted entity is presented");
        };
        assert_eq!(
            frame.transform.translation.x, replayed,
            "the frame must carry the replayed rules-produced value"
        );
    }

    // The correction is recorded by `feed_residuals` in `PostUpdate`, after
    // this frame's extraction ran — so the notice rides the next run. That
    // run also advances the clock one tick, which is what lets the assertion
    // below tell the residual's tick from the extraction's: a notice stamped
    // `extracted_at` would carry the *later* tick.
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    app.update();

    let drained = batches(&mut app);

    let rollbacks = app
        .world()
        .resource::<ReconciliationMonitor>()
        .track(&TrackKey {
            authority,
            entity: ENTITY,
        })
        .map(|track| track.rollbacks)
        .unwrap_or(0);
    assert!(
        rollbacks > 0,
        "precondition: the forced rollback must produce a counted correction"
    );

    let notices = corrections(&drained);
    assert_eq!(
        notices.len(),
        1,
        "one correction since the last extraction, not one per replayed tick"
    );
    assert_eq!(notices[0].persist_id, ENTITY);
    assert_eq!(
        notices[0].observed_at,
        Tick::new(u64::from(start + PRESENT_TICK)),
        "stamped with the tick the residual was recorded on"
    );

    // And the frames keep following the rules after the correction: the next
    // tick's step, not the replayed past.
    let frames = frames_of(&drained);
    let [frame] = frames.predicted.as_slice() else {
        panic!("exactly one predicted entity is presented");
    };
    let stepped = app
        .world()
        .resource::<StepTrace>()
        .0
        .last()
        .copied()
        .expect("the ruleset stepped on the follow-up tick");
    assert_eq!(
        frame.transform.translation.x, stepped.position_mm,
        "extraction resumes following the ruleset after the correction"
    );
}
