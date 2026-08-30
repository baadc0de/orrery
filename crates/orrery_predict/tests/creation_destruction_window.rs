//! F-7 scenario: entity creation and destruction inside the rollback window
//! (A10 §6.2, D47 clause (e)).
//!
//! One real app, no transport: the shipped `OrreryPredictPlugin` stack with
//! the topology settled at a declared P2P session, two components registered
//! for local rollback, so the real ring, the real `prepare_rollback` restore,
//! the real replay and the real history rewrite all execute. The spawn and
//! the despawn are not test gestures: they are outcomes of the simulated
//! rules themselves (an entity's vitality crosses zero and the ordinary
//! `prediction_despawn` path runs; a new entity materializes at a fixed
//! tick), so the forced rollback to a tick before both events re-executes
//! them deterministically during the replay.
//!
//! What D47 promises, and what this scenario asserts:
//!
//! - **Restore is all-or-nothing at the witnessed entity.** `Pose` and
//!   `Vitality` are one predicted set: at the first replayed tick both must
//!   stand at their anchor-tick ring values — never one restored and one left
//!   at the present. (D47 clause (e): a partially restored state "encodes to
//!   canonical bytes whose hash matches no claim any authority ever made";
//!   partial restore is a named future door, so it must be a failure here,
//!   not a silent pass.) The two components are also causally coupled —
//!   vitality drives velocity — so a mixed-tick state diverges the replayed
//!   trajectory and the ring stops matching the frozen straight-line record.
//! - **No predicted set survives its entity.** The despawned entity stays
//!   despawned after the rollback of its neighbour (the replay re-executes
//!   the death), nothing feeds its ring after it dies, and the materialized
//!   entity's ring contains exactly its one spawn-tick sample — a predicted
//!   set neither outlives nor predates its entity.
//! - **A rollback of a neighbour neither resurrects the despawned entity nor
//!   loses the materialized one** (A10 §6.2's own phrasing).
//!
//! #417 clause analysis. The only production clause standing between this
//! fixture and a silently partial restore is `prepare_rollback`'s
//! per-component restore in lightyear's rollback machinery
//! (`lightyear_prediction-0.29.0/src/rollback.rs`): for each registered
//! component of each predicted entity it either restores the anchor-tick
//! value from the ring or leaves the present value in place when the ring has
//! no anchor for the entity. No orrery clause stands at that seam — the
//! restore mechanics are exactly what the plan-B seam crate delegates to
//! lightyear (docs/10-crates.md layering rule 3) — so the fixture pins the
//! observable consequence: both components of a witnessed entity move
//! together or the scenario fails. Making the restore partial for one
//! component kind must fail this scenario by name on the all-or-nothing
//! capture.

use std::collections::BTreeMap;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_time::TimeUpdateStrategy;
use lightyear::core::history_buffer::HistoryState;
use lightyear::core::tick::Tick as SessionTick;
use lightyear::prelude::{
    LocalTimeline, LocalTimelineSync, NetworkingMetadata, Predicted, PredictionAppRegistrationExt,
    PredictionDespawnCommandsExt, PredictionDisable, PredictionHistory, PredictionMetrics,
    StateRollbackMetadata, P2P,
};
use orrery_predict::{OrreryPredictPlugin, PredictConfig};

/// The game's predicted pose component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
struct Pose {
    pos_mm: i64,
}

/// The game's predicted vitality component — causally coupled to `Pose` so a
/// mixed-tick restore cannot reproduce the straight-line trajectory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
struct Vitality {
    hp: i64,
}

#[derive(Component)]
struct Neighbour;

#[derive(Component)]
struct Doomed;

/// Marks the entity materialized inside the window. It has no `Vitality` and
/// is stepped by nothing: its whole predicted set is the one spawn-tick pose
/// sample.
#[derive(Component)]
struct Sprouted;

const STEP_MM: i64 = 3;
/// The tick the rollback anchors at: before both the spawn and the death.
const ANCHOR_TICK: u64 = 4;
/// The tick the new entity materializes on.
const SPAWN_TICK: u64 = 6;
/// The tick the doomed entity's vitality crosses zero.
const DEATH_TICK: u64 = 8;
/// The present tick when the rollback is forced.
const PRESENT_TICK: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Doomed,
    Neighbour,
}

/// Which entity kind, at which tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SampleKey {
    tick: u64,
    kind: Kind,
}

/// One witnessed entity's straight-line state at one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    pos_mm: i64,
    hp: i64,
}

/// The straight-line record: per entity kind and tick, the pose and vitality
/// the deterministic rules produced.
#[derive(Resource, Default)]
struct Trajectory(BTreeMap<SampleKey, Sample>);

/// Armed before the rollback update; the first replay tick's restored state
/// is captured into [`Captured`] and the arm drops.
#[derive(Resource, Default)]
struct CaptureArmed(bool);

#[derive(Resource, Default)]
struct Captured {
    neighbour: Option<Sample>,
    doomed: Option<Sample>,
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(bevy_app::PanicHandlerPlugin);
    app.add_plugins(bevy_time::TimePlugin);
    // lightyear's replication backend calls `init_state`, which needs the
    // `StateTransition` schedule that `StatesPlugin` installs (see
    // `reconciliation.rs`'s harness).
    app.add_plugins(bevy_state::app::StatesPlugin);
    app.add_plugins(OrreryPredictPlugin {
        config: PredictConfig::default(),
    });
    // One fixed step per update, from synthetic time: the tick positions of
    // the spawn, the death and the rollback anchor are part of the scenario,
    // so wall-clock drift must not be able to shift them.
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    // Declare a P2P session with no links: enough for the shipped stack's
    // topology inference to settle on `NetworkTopology::P2P`, which is what
    // turns lightyear's real prediction pipeline on (`should_run`).
    app.world_mut().spawn(P2P);
    app.finish();
    app.local_rollback::<Pose>();
    app.local_rollback::<Vitality>();
    app
}

/// Settle the shipped topology inference without advancing the timeline, so
/// the pipeline is running before any scenario tick is accounted. The
/// session's clock is declared synced: with no remote to sample, the
/// in-process driver is the documented `set_synced` caller, and without it
/// lightyear's sync-gated `check_rollback` would never run — the harness
/// would silently test nothing.
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

/// The simulated rules: vitality drains one per tick; the entity moves while
/// it is alive; a doomed entity whose vitality has crossed zero goes through
/// the ordinary prediction-despawn path; one new entity materializes at its
/// scheduled tick. Deterministic, so the replay must reproduce the record.
fn step_world(
    mut commands: Commands,
    mut q: Query<(Entity, &mut Pose, &mut Vitality, Option<&Doomed>), Without<Sprouted>>,
    timeline: Res<LocalTimeline>,
    mut materialized: Local<bool>,
) {
    let tick = u64::from(timeline.tick().0);
    for (entity, mut pose, mut vitality, doomed) in &mut q {
        vitality.hp -= 1;
        let vel = if vitality.hp > 0 { STEP_MM } else { 0 };
        pose.pos_mm += vel;
        if doomed.is_some() && vitality.hp <= 0 {
            commands.entity(entity).prediction_despawn();
        }
    }
    if tick == SPAWN_TICK && !*materialized {
        *materialized = true;
        commands.spawn((Predicted, Sprouted, Pose { pos_mm: 1_000 }));
    }
}

/// The straight-line record.
fn record_trajectory(
    q: Query<(&Pose, &Vitality, Option<&Neighbour>), Without<Sprouted>>,
    timeline: Res<LocalTimeline>,
    mut trajectory: ResMut<Trajectory>,
) {
    let tick = u64::from(timeline.tick().0);
    for (pose, vitality, neighbour) in &q {
        let kind = if neighbour.is_some() {
            Kind::Neighbour
        } else {
            Kind::Doomed
        };
        trajectory.0.insert(
            SampleKey { tick, kind },
            Sample {
                pos_mm: pose.pos_mm,
                hp: vitality.hp,
            },
        );
    }
}

/// Capture the restored state at the first replayed tick — before the
/// replay's own step can overwrite it. This is where all-or-nothing is
/// observable: every component of a witnessed entity must stand at its
/// anchor-tick ring value at once.
fn capture_restore(
    neighbour: Query<(&Pose, &Vitality), With<Neighbour>>,
    doomed: Query<(&Pose, &Vitality), With<Doomed>>,
    mut armed: ResMut<CaptureArmed>,
    mut captured: ResMut<Captured>,
) {
    if !armed.0 {
        return;
    }
    armed.0 = false;
    let (pose, vitality) = neighbour.single().expect("the neighbour is witnessed");
    captured.neighbour = Some(Sample {
        pos_mm: pose.pos_mm,
        hp: vitality.hp,
    });
    let (pose, vitality) = doomed.single().expect("the doomed entity is witnessed");
    captured.doomed = Some(Sample {
        pos_mm: pose.pos_mm,
        hp: vitality.hp,
    });
}

/// The vacuity guard: the harness is only a scenario if the shipped pipeline
/// actually runs — the topology inference settles on P2P, both components'
/// rings are really being fed by the shipped history system, and the ordinary
/// despawn path really engages when vitality crosses zero.
#[test]
fn the_window_harness_pipeline_actually_runs() {
    let mut app = app();
    warm_up(&mut app);
    app.init_resource::<Trajectory>();
    let neighbour = app
        .world_mut()
        .spawn((
            Neighbour,
            Predicted,
            Pose { pos_mm: 0 },
            Vitality { hp: 20 },
        ))
        .id();
    let doomed = app
        .world_mut()
        .spawn((Doomed, Predicted, Pose { pos_mm: 0 }, Vitality { hp: 8 }))
        .id();
    app.add_systems(FixedUpdate, (step_world, record_trajectory));

    for _ in 0..DEATH_TICK {
        app.update();
    }

    for (entity, depth, name) in [
        (neighbour, DEATH_TICK, "neighbour"),
        (doomed, DEATH_TICK - 1, "doomed"),
    ] {
        let pose_ring = app
            .world()
            .get::<PredictionHistory<Pose>>(entity)
            .unwrap_or_else(|| panic!("{name} pose ring missing"));
        let vitality_ring = app
            .world()
            .get::<PredictionHistory<Vitality>>(entity)
            .unwrap_or_else(|| panic!("{name} vitality ring missing"));
        assert_eq!(
            pose_ring.len(),
            depth as usize,
            "the shipped history system must feed the {name} pose ring"
        );
        assert_eq!(
            vitality_ring.len(),
            depth as usize,
            "the shipped history system must feed the {name} vitality ring"
        );
    }
    assert!(
        app.world().get::<PredictionDisable>(doomed).is_some(),
        "the ordinary prediction-despawn path must engage at zero vitality"
    );
}

/// The scenario. See the module docs for the choreography and the clause
/// analysis.
#[test]
fn an_entity_spawned_and_despawned_inside_the_window_restores_all_or_nothing_and_no_predicted_set_survives_its_entity(
) {
    let mut app = app();
    warm_up(&mut app);
    app.init_resource::<Trajectory>();
    app.init_resource::<CaptureArmed>();
    app.init_resource::<Captured>();

    let neighbour = app
        .world_mut()
        .spawn((
            Neighbour,
            Predicted,
            Pose { pos_mm: 0 },
            Vitality { hp: 20 },
        ))
        .id();
    let doomed = app
        .world_mut()
        .spawn((Doomed, Predicted, Pose { pos_mm: 0 }, Vitality { hp: 8 }))
        .id();

    app.add_systems(FixedUpdate, capture_restore.before(step_world));
    app.add_systems(FixedUpdate, step_world);
    app.add_systems(FixedUpdate, record_trajectory.after(step_world));

    // Ticks 1-10: the straight line. The doomed entity dies at tick 8 on the
    // ordinary despawn path; the new entity materializes at tick 6.
    for _ in 0..PRESENT_TICK {
        app.update();
    }
    assert_eq!(
        u64::from(app.world().resource::<LocalTimeline>().tick().0),
        PRESENT_TICK
    );

    // The death really happened on the ordinary path before the rollback, so
    // "stays despawned" below cannot pass vacuously.
    assert!(app.world().get::<PredictionDisable>(doomed).is_some());
    assert!(app.world().get::<PredictionDisable>(neighbour).is_none());

    // Freeze the straight-line record before the rollback rewinds anything.
    // The live record stays in the world — the replayed ticks keep recording
    // — but every comparison below is against this frozen copy.
    let reference = app.world().resource::<Trajectory>().0.clone();

    let ring_at = |world: &bevy_ecs::world::World, entity: Entity, tick: u64| -> Option<Sample> {
        let pose = world
            .get::<PredictionHistory<Pose>>(entity)
            .expect("pose ring")
            .buffer()
            .iter()
            .find(|(ring_tick, _)| u64::from(ring_tick.0) == tick)
            .map(|(_, state)| match state {
                HistoryState::Updated(pose) => pose.pos_mm,
                other => panic!("unexpected ring state {other:?}"),
            });
        let vitality = world
            .get::<PredictionHistory<Vitality>>(entity)
            .expect("vitality ring")
            .buffer()
            .iter()
            .find(|(ring_tick, _)| u64::from(ring_tick.0) == tick)
            .map(|(_, state)| match state {
                HistoryState::Updated(vitality) => vitality.hp,
                other => panic!("unexpected ring state {other:?}"),
            });
        pose.zip(vitality).map(|(pos_mm, hp)| Sample { pos_mm, hp })
    };

    let neighbour_anchor = ring_at(app.world(), neighbour, ANCHOR_TICK)
        .expect("the neighbour is witnessed: its ring must hold the anchor tick");
    let doomed_anchor = ring_at(app.world(), doomed, ANCHOR_TICK)
        .expect("the doomed entity lived at the anchor: its ring must hold it");

    // Force the real rollback to the anchor tick, six ticks back — inside
    // D8's 9-tick window — on an update that runs nothing else.
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(0));
    app.insert_resource(CaptureArmed(true));
    app.world_mut()
        .resource_mut::<StateRollbackMetadata>()
        .request_forced_rollback(SessionTick(u32::try_from(ANCHOR_TICK).expect("fits u32")));
    app.update();

    assert_eq!(
        u64::from(app.world().resource::<LocalTimeline>().tick().0),
        PRESENT_TICK,
        "the rollback replays to the present and nothing beyond it"
    );

    // ── Restore is all-or-nothing at the witnessed entities ─────────────────
    let captured = app
        .world_mut()
        .remove_resource::<Captured>()
        .expect("captured");
    assert_eq!(
        captured.neighbour,
        Some(neighbour_anchor),
        "the neighbour's pose and vitality must both stand at their anchor-tick \
         ring values at the first replayed tick — a mixed-tick state encodes to \
         bytes no authority ever claimed (D47 clause (e))"
    );
    assert_eq!(
        captured.doomed,
        Some(doomed_anchor),
        "the doomed entity was alive at the anchor: its pose and vitality must \
         both be restored together"
    );

    // ── The replay reproduced the straight line, ring for ring ─────────────
    for tick in (ANCHOR_TICK + 1)..=PRESENT_TICK {
        let actual = ring_at(app.world(), neighbour, tick)
            .unwrap_or_else(|| panic!("tick {tick}: the neighbour's ring lost a replayed tick"));
        assert_eq!(
            actual,
            reference[&SampleKey {
                tick,
                kind: Kind::Neighbour
            }],
            "tick {tick}: the replayed ring diverged from the straight line"
        );
    }

    // ── No predicted set survives its entity ────────────────────────────────
    //
    // The despawned entity: still despawned after the neighbour's rollback —
    // the replay re-executed the death rather than resurrecting it.
    assert!(
        app.world().get::<PredictionDisable>(doomed).is_some(),
        "the rollback resurrected the despawned entity"
    );
    let doomed_ring_ticks: Vec<u64> = app
        .world()
        .get::<PredictionHistory<Pose>>(doomed)
        .expect("doomed pose ring")
        .buffer()
        .iter()
        .map(|(tick, _)| u64::from(tick.0))
        .collect();
    assert!(
        doomed_ring_ticks.iter().all(|tick| *tick <= DEATH_TICK),
        "nothing may feed a despawned entity's ring: entries past the death tick \
         would be a predicted set surviving its entity"
    );
    for tick in &doomed_ring_ticks {
        if *tick > ANCHOR_TICK {
            let actual = ring_at(app.world(), doomed, *tick)
                .unwrap_or_else(|| panic!("tick {tick}: the doomed entity's ring lost its pair"));
            assert_eq!(
                actual,
                reference[&SampleKey {
                    tick: *tick,
                    kind: Kind::Doomed
                }],
                "tick {tick}: the doomed entity's replayed ring diverged from the \
                 straight line before its death"
            );
        }
    }

    // The materialized entity: not lost by the neighbour's rollback, and its
    // predicted set contains exactly its spawn-tick sample — the set neither
    // predates the entity nor outlives it.
    let mut sprouted_found: Option<(Entity, i64)> = None;
    {
        let mut q = app
            .world_mut()
            .query_filtered::<(Entity, &Pose), With<Sprouted>>();
        for (entity, pose) in q.iter_mut(app.world_mut()) {
            sprouted_found = Some((entity, pose.pos_mm));
        }
    }
    let (sprouted, sprouted_pose) =
        sprouted_found.expect("the materialized entity must survive its neighbour's rollback");
    assert!(app.world().get::<PredictionDisable>(sprouted).is_none());
    assert_eq!(sprouted_pose, 1_000);
    let sprouted_ring_ticks: Vec<u64> = app
        .world()
        .get::<PredictionHistory<Pose>>(sprouted)
        .expect("materialized ring")
        .buffer()
        .iter()
        .map(|(tick, _)| u64::from(tick.0))
        .collect();
    assert_eq!(
        sprouted_ring_ticks,
        Vec::<u64>::new(),
        "the materialized entity's predicted set must hold nothing at or before \
         the rollback anchor: its spawn-tick sample lies above the anchor, so the \
         re-anchor discards it, and a static entity's ring is only re-fed by a \
         replayed change — no restored sample may claim the entity existed at \
         the anchor"
    );

    // The neighbour itself: alive, at the straight-line present.
    let live = app
        .world()
        .get::<Pose>(neighbour)
        .zip(app.world().get::<Vitality>(neighbour))
        .map(|(pose, vitality)| Sample {
            pos_mm: pose.pos_mm,
            hp: vitality.hp,
        })
        .expect("the neighbour survives");
    assert_eq!(
        live,
        reference[&SampleKey {
            tick: PRESENT_TICK,
            kind: Kind::Neighbour
        }],
        "the neighbour's present state is the straight-line present"
    );

    // The rollback was real: lightyear counted exactly one.
    assert_eq!(
        app.world().resource::<PredictionMetrics>().rollbacks,
        1,
        "the window rollback must have executed on the real path"
    );
}
