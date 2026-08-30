//! F-7 scenario: authority handoff adjacent to rollback (A10 §6.2, D47).
//!
//! Two peers, two real apps, no transport. Both run the shipped
//! `OrreryPredictPlugin` stack with the topology settled at a declared P2P
//! session, so lightyear's real prediction pipeline runs headless; both
//! register `Pose` for local rollback, so the real ring, the real
//! `prepare_rollback` restore, the real replay and the real history rewrite
//! all execute. The handoff itself travels the real ingress: an
//! `AuthorityCorrectionClaimsV1` through the real `AuthorityCorrectionInbox`,
//! drained by the real `reconcile_authority_corrections`, decided by the real
//! `authority_correction_plan`, consumed by a stand-in for the game's
//! ordinary path (the same adapter shape `correction.rs`'s own tests use).
//!
//! The scenario: peer A holds the pen over entity P and steps it (+7 mm per
//! tick). Peer B predicts P under A's authority with an interaction guess A
//! has not applied (+20 mm per tick) — docs/05 §2 case 2, a divergent
//! predicted view of a contested entity, which docs/05 §11 permits. At tick 6
//! A divests. The handoff correction (A's final state, stamped
//! `authoritative_tick = 6`) reaches B at tick 9 — B is still holding its
//! guesses for ticks 7-9, and the pass is inside the 9-tick window. D8's
//! in-window branch must therefore
//!
//! 1. decide `Rollback { tick: 6 }` — the re-anchor decision;
//! 2. re-anchor B's ring at the handoff tick: the delivered state replaces
//!    the guess at tick 6, the stale guesses above it are discarded, and the
//!    real forced rollback replays 7-9 under the successor's rule (+13 mm per
//!    tick), rewriting the ring;
//! 3. pass the pen exactly once: A named through tick 6, B named from tick 7
//!    on, no tick with both and no tick with neither.
//!
//! The "feeder" here is the prediction-tier sibling of `feed_uplink`'s uplink
//! guard (A10 §6.2): the peer whose live `PredictedBy` component names it as
//! holder of entity P is the peer feeding the entity's authoritative state —
//! the same single-writer invariant the uplink guard enforces, read where
//! prediction actually runs. Two holders at one tick is split brain; zero is
//! an orphaned entity; the assertion distinguishes the two.
//!
//! Why the pen passes only on the `Rollback` branch: a snap reconciles
//! against a *remote* authority's state — installing someone's state is not
//! holding their pen (docs/05 §3, "beyond the window: snap + reconcile").
//! The successor's authority begins at the re-anchor, which is why the
//! guarded stage is rollback-window anchoring itself.
//!
//! #417 clause analysis. The only production clause standing between this
//! fixture and a silently wrong handoff is the in-window branch of
//! `authority_correction_plan` (`crates/orrery_predict/src/correction.rs`).
//! Every other clause on the path is exercised in its passing direction by
//! construction: the correction is well-formed, the inbox drains, the
//! reconciler accepts, the entity is predicted and carries history. Deleting
//! that branch alone (every age snaps) must fail this scenario by name on the
//! exactly-one-feeder assertion, because a snap never re-anchors the ring and
//! never carries the pen: B's ring keeps its stale guesses above the handoff
//! tick and no peer holds the pen afterwards.
//!
//! Tick numerics: `TickBridge` is anchored at universe tick 0 by the plugin,
//! so lightyear's session tick and the universe tick coincide in this harness
//! and the correction's universe stamp can drive the session-tick rollback
//! directly.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_time::TimeUpdateStrategy;
use lightyear::core::history_buffer::HistoryState;
use lightyear::core::tick::Tick as SessionTick;
use lightyear::prelude::{
    LocalTimeline, LocalTimelineSync, NetworkingMetadata, Predicted, PredictionAppRegistrationExt,
    PredictionHistory, PredictionMetrics, StateRollbackMetadata, P2P,
};
use orrery_predict::correction::AuthorityCorrectionPlan;
use orrery_predict::{
    AuthorityCorrectionInbox, AuthorityCorrectionReconciler, OrreryPredictPlugin, PredictConfig,
    PredictedBy, SharedAuthorityCorrectionReconciler,
};
use orrery_protocol::{
    AuthorityCorrectionClaimsV1, NodeId, PersistId, RulesetId, Tick as UniverseTick,
};

/// The game's predicted pose component. Integer millimetres: every trajectory
/// below is exact, so any restore that is not bit-identical shows in the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
struct Pose {
    pos_mm: i64,
}

/// The outgoing authority's rule: its simulation advances the entity 7 mm per
/// tick.
const HOLDER_STEP_MM: i64 = 7;
/// The successor's rule: once the pen is its, the entity advances 13 mm per
/// tick — a different trajectory, so a ring sample's value identifies whose
/// rule produced it.
const SUCCESSOR_STEP_MM: i64 = 13;
/// The interactor's guess: B predicts A's entity under an interaction input A
/// has not applied, advancing it 20 mm per tick.
const GUESS_STEP_MM: i64 = 20;

const HOLDER: u8 = 1;
const SUCCESSOR: u8 = 2;
const ENTITY: u64 = 77;
/// The pen-pass tick: A's last authoritative state, stamped as the
/// authoritative tick of the handoff correction.
const HANDOFF_TICK: u64 = 6;
/// The tick on which the correction is processed: three ticks of real ingress
/// latency after the pass, still inside the 9-tick window.
const PROCESSED_TICK: u64 = 9;
/// The final tick of the scenario: one walk-off frame after the rollback.
const LAST_TICK: u64 = 11;

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

/// The correction as handed to the ordinary-path adapter.
struct PendingCorrection {
    correction: AuthorityCorrectionClaimsV1,
    plan: AuthorityCorrectionPlan,
}

/// The decision the real drain recorded. `observed` keeps the plan for the
/// scenario's assertions; `pending` is the work item the adapter consumes
/// once.
#[derive(Default)]
struct Decision {
    observed: Mutex<Option<AuthorityCorrectionPlan>>,
    pending: Mutex<Option<PendingCorrection>>,
}

#[derive(Resource, Clone)]
struct DecisionRes(Arc<Decision>);

struct HandoffReconciler(Arc<Decision>);

impl AuthorityCorrectionReconciler for HandoffReconciler {
    fn reconcile(
        &self,
        correction: &AuthorityCorrectionClaimsV1,
        plan: AuthorityCorrectionPlan,
    ) -> Result<(), String> {
        *self.0.observed.lock().expect("decision") = Some(plan);
        *self.0.pending.lock().expect("decision") = Some(PendingCorrection {
            correction: correction.clone(),
            plan,
        });
        Ok(())
    }
}

/// Which authority this app saw named as holder of entity P, per tick. Last
/// write wins, so a replayed tick records the rewrite's holder — the final
/// accounting of who owns each tick of the entity's history.
#[derive(Resource, Default)]
struct PenLedger(Mutex<BTreeMap<u64, Option<NodeId>>>);

fn peer_app(reconciler: Option<SharedAuthorityCorrectionReconciler>) -> App {
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
    // the handoff are part of the scenario, so wall-clock drift must not be
    // able to shift them.
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    // Declare a P2P session with no links: enough for the shipped stack's
    // topology inference to settle on `NetworkTopology::P2P`, which is what
    // turns lightyear's real prediction pipeline on (`should_run`).
    app.world_mut().spawn(P2P);
    if let Some(reconciler) = reconciler {
        app.insert_resource(reconciler);
    }
    app.finish();
    app.local_rollback::<Pose>();
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

/// Exactly one fixed step per update, and the ledger entry that proves it.
fn step(app: &mut App) -> u64 {
    app.update();
    let tick = u64::from(app.world().resource::<LocalTimeline>().tick().0);
    let recorded = app
        .world()
        .resource::<PenLedger>()
        .0
        .lock()
        .expect("ledger")
        .len();
    assert_eq!(
        tick as usize, recorded,
        "the harness tick discipline broke: every tick of this app must have \
         exactly one pen observation"
    );
    tick
}

/// The pen-holder's simulation: the authority steps its own entity.
fn step_authority(mut poses: Query<&mut Pose, With<Predicted>>) {
    for mut pose in &mut poses {
        pose.pos_mm += HOLDER_STEP_MM;
    }
}

/// The predictor's simulation: the interactor guesses the remote entity with
/// an input the holder has not applied; after the handoff it steps the entity
/// it now holds under the successor's rule.
fn step_predictor(mut poses: Query<(&mut Pose, &PredictedBy)>) {
    for (mut pose, predicted_by) in &mut poses {
        let step_mm = if predicted_by.authority == node(SUCCESSOR) {
            SUCCESSOR_STEP_MM
        } else {
            GUESS_STEP_MM
        };
        pose.pos_mm += step_mm;
    }
}

/// Record which authority this app's live `PredictedBy` names for the entity
/// at this tick. This is the feeder observation: the holder named by the real
/// component is the peer feeding the entity's authoritative state. Last write
/// wins, so a replayed tick records the rewrite's holder.
fn record_pen(
    entity: Entity,
    timeline: Res<LocalTimeline>,
    held: Query<&PredictedBy>,
    ledger: Res<PenLedger>,
) {
    let holder = held.get(entity).ok().map(|by| by.authority);
    ledger
        .0
        .lock()
        .expect("ledger")
        .insert(u64::from(timeline.tick().0), holder);
}

/// The game's ordinary-path adapter, acting on the real plan. On the in-window
/// branch it takes the pen and re-bases the ring at the authoritative tick; on
/// the snap branch it installs the state as a remote correction and leaves pen
/// and ring alone.
fn consummate_handoff(
    decision: Res<DecisionRes>,
    mut poses: Query<(Entity, &mut Pose, &mut PredictionHistory<Pose>)>,
    mut rollback_metadata: ResMut<StateRollbackMetadata>,
    mut fired: Local<bool>,
    mut commands: Commands,
) {
    if *fired {
        return;
    }
    let Some(pending) = decision.0.pending.lock().expect("decision").take() else {
        return;
    };
    *fired = true;

    let correction = pending.correction;
    let plan = pending.plan;
    *fired = true;

    let anchor = SessionTick(
        u32::try_from(correction.authoritative_tick.0)
            .expect("the session tick fits u32 in this harness"),
    );
    let delivered = Pose {
        pos_mm: i64::from_le_bytes(
            correction
                .authoritative_state
                .as_slice()
                .try_into()
                .expect("the harness encodes the delivered pose as one little-endian i64"),
        ),
    };

    match plan {
        AuthorityCorrectionPlan::Rollback { tick } => {
            assert_eq!(
                tick, correction.authoritative_tick,
                "the plan must anchor at the correction's authoritative tick"
            );
            let (entity, mut pose, mut history) = poses
                .single_mut()
                .expect("the successor app predicts exactly one entity");
            // The pen passes here: the successor's authority begins at the
            // re-anchor.
            commands.entity(entity).insert(PredictedBy {
                authority: node(SUCCESSOR),
                persist_id: correction.entity,
            });
            // Re-base the ring at the handoff tick: the delivered state takes
            // over from the tick it is stamped with; everything above it is
            // the old authority's future and is discarded. The real rollback
            // requested below then restores from this entry and replays.
            history.clear_after_tick(anchor);
            history.add_update(anchor, delivered);
            *pose = delivered;
            rollback_metadata.request_forced_rollback(anchor);
        }
        AuthorityCorrectionPlan::Snap { .. } => {
            // A snap installs remote state at present; the ring is not
            // re-anchored and the pen does not pass.
            let (_, mut pose, _) = poses
                .single_mut()
                .expect("the successor app predicts exactly one entity");
            *pose = delivered;
        }
    }
}

fn delivered_claims() -> AuthorityCorrectionClaimsV1 {
    AuthorityCorrectionClaimsV1 {
        issuer: node(HOLDER),
        subject: node(SUCCESSOR),
        entity: PersistId::new(ENTITY),
        reconcile_from: UniverseTick(HANDOFF_TICK),
        authoritative_tick: UniverseTick(HANDOFF_TICK),
        // A's final state: 7 mm per tick for six ticks.
        authoritative_state: (HOLDER_STEP_MM * HANDOFF_TICK as i64)
            .to_le_bytes()
            .to_vec(),
        ruleset: RulesetId {
            version: 1,
            digest: [3; 32],
        },
        adjudication: [4; 32],
    }
}

/// The vacuity guard: the harness is only a scenario if the shipped pipeline
/// actually runs — the topology inference settles on P2P from the declared
/// session, and a predicted entity's ring is really being fed by the shipped
/// `FixedPostUpdate` history system.
#[test]
fn the_handoff_harness_pipeline_actually_runs() {
    let mut app = peer_app(None);
    warm_up(&mut app);

    app.insert_resource(PenLedger::default());
    let entity = app.world_mut().spawn((Predicted, Pose { pos_mm: 0 })).id();
    app.add_systems(FixedUpdate, step_authority);
    app.add_systems(
        FixedUpdate,
        move |timeline: Res<LocalTimeline>, held: Query<&PredictedBy>, ledger: Res<PenLedger>| {
            record_pen(entity, timeline, held, ledger);
        },
    );
    assert_eq!(step(&mut app), 1);
    assert_eq!(step(&mut app), 2);

    let ring = app
        .world()
        .get::<PredictionHistory<Pose>>(entity)
        .expect("ring")
        .buffer();
    assert_eq!(
        ring.len(),
        2,
        "the shipped history system must feed the ring"
    );
}

/// The scenario. See the module docs for the choreography and the clause
/// analysis.
#[test]
fn authority_handoff_inside_the_window_reanchors_the_ring_and_the_pen_flips_exactly_once() {
    let decision = Arc::new(Decision::default());
    let mut holder = peer_app(None);
    let mut successor = peer_app(Some(SharedAuthorityCorrectionReconciler(Arc::new(
        HandoffReconciler(decision.clone()),
    ))));
    successor.insert_resource(DecisionRes(decision.clone()));

    warm_up(&mut holder);
    warm_up(&mut successor);
    holder.insert_resource(PenLedger::default());
    successor.insert_resource(PenLedger::default());

    let holder_entity = holder
        .world_mut()
        .spawn((
            Predicted,
            Pose { pos_mm: 0 },
            PredictedBy {
                authority: node(HOLDER),
                persist_id: PersistId::new(ENTITY),
            },
        ))
        .id();
    // The successor predicts the contested entity under the holder's
    // authority — a divergent view, no pen.
    let successor_entity = successor
        .world_mut()
        .spawn((
            Predicted,
            Pose { pos_mm: 0 },
            PredictedBy {
                authority: node(HOLDER),
                persist_id: PersistId::new(ENTITY),
            },
        ))
        .id();

    holder.add_systems(FixedUpdate, step_authority);
    successor.add_systems(FixedUpdate, step_predictor);
    let held_holder = holder_entity;
    holder.add_systems(
        FixedUpdate,
        move |timeline: Res<LocalTimeline>, held: Query<&PredictedBy>, ledger: Res<PenLedger>| {
            record_pen(held_holder, timeline, held, ledger);
        },
    );
    let held_successor = successor_entity;
    successor.add_systems(
        FixedUpdate,
        move |timeline: Res<LocalTimeline>, held: Query<&PredictedBy>, ledger: Res<PenLedger>| {
            record_pen(held_successor, timeline, held, ledger);
        },
    );
    successor.add_systems(
        PostUpdate,
        consummate_handoff.after(orrery_predict::reconcile_authority_corrections),
    );

    // Ticks 1-6: A feeds its ring with its truth; B feeds its ring with
    // guesses of A's entity.
    for _ in 0..HANDOFF_TICK {
        step(&mut holder);
        step(&mut successor);
    }

    // A divests at the pen-pass tick: it stops writing the entity.
    holder
        .world_mut()
        .entity_mut(holder_entity)
        .remove::<Predicted>()
        .remove::<PredictedBy>();

    // Ticks 7-8: B does not know yet and keeps guessing under A's authority.
    for _ in HANDOFF_TICK..(PROCESSED_TICK - 1) {
        step(&mut holder);
        step(&mut successor);
    }

    // The handoff correction arrives through the real ingress.
    successor
        .world_mut()
        .resource_mut::<AuthorityCorrectionInbox>()
        .push(delivered_claims());

    // Tick 9: the real drain decides; the adapter acts on the plan.
    step(&mut successor);
    step(&mut holder);

    // Tick 10: the real forced rollback — restore at the handoff tick,
    // discard the stale guesses above it, replay 7-9 under the successor's
    // rule — and then the frame's own step at tick 10.
    step(&mut successor);
    step(&mut holder);

    // Tick 11: one walk-off frame on an ordinary timeline.
    step(&mut successor);
    step(&mut holder);

    assert_eq!(
        u64::from(holder.world().resource::<LocalTimeline>().tick().0),
        LAST_TICK
    );
    assert_eq!(
        u64::from(successor.world().resource::<LocalTimeline>().tick().0),
        LAST_TICK
    );

    // ── The exactly-one-feeder assertion ────────────────────────────────────
    //
    // Walk every tick of the scenario and collect the peers whose live
    // components named them holder of entity P.
    let holder_ledger = holder
        .world_mut()
        .remove_resource::<PenLedger>()
        .expect("holder ledger")
        .0
        .into_inner()
        .expect("holder ledger");
    let successor_ledger = successor
        .world_mut()
        .remove_resource::<PenLedger>()
        .expect("successor ledger")
        .0
        .into_inner()
        .expect("successor ledger");

    let mut flips = 0;
    let mut previous: Option<NodeId> = None;
    for tick in 1..=LAST_TICK {
        let mut holders: Vec<NodeId> = Vec::new();
        for (ledger, peer) in [(&holder_ledger, "holder"), (&successor_ledger, "successor")] {
            match ledger.get(&tick) {
                Some(Some(authority)) => holders.push(*authority),
                Some(None) => {}
                None => panic!(
                    "tick {tick}: the {peer} app recorded nothing — the harness missed a tick"
                ),
            }
        }
        holders.sort();
        holders.dedup();
        match holders.len() {
            1 => {}
            0 => panic!(
                "tick {tick}: zero feeders — nobody holds the pen over entity {ENTITY}; \
                 the entity's authoritative state is orphaned"
            ),
            _ => panic!(
                "tick {tick}: two feeders — {holders:?} both hold the pen over entity \
                 {ENTITY}: split brain, the single-writer invariant is broken"
            ),
        }
        let current = holders[0];
        if previous.is_some_and(|previous| previous != current) {
            flips += 1;
        }
        previous = Some(current);
    }
    assert_eq!(
        previous,
        Some(node(SUCCESSOR)),
        "the handoff never completed: zero feeders from the successor — the pen \
         never reached the peer the correction named"
    );
    assert_eq!(
        flips, 1,
        "the pen must pass exactly once across the whole scenario"
    );

    // The plan was decided by the real window math. It is asserted after the
    // pen tile deliberately: the feeder ledger is the scenario's headline
    // claim, and the mutation transcript must name it.
    {
        let observed = decision.observed.lock().expect("decision");
        assert_eq!(
            *observed,
            Some(AuthorityCorrectionPlan::Rollback {
                tick: UniverseTick(HANDOFF_TICK)
            }),
            "a handoff inside the 9-tick window must take the in-window branch"
        );
    }

    // ── The ring re-anchored at the handoff tick ────────────────────────────
    let successor_ring: Vec<(u64, i64)> = successor
        .world()
        .get::<PredictionHistory<Pose>>(successor_entity)
        .expect("the successor's ring")
        .buffer()
        .iter()
        .map(|(tick, state)| match state {
            HistoryState::Updated(pose) => (u64::from(tick.0), pose.pos_mm),
            other => panic!("tick {}: unexpected ring state {other:?}", tick.0),
        })
        .collect();

    // The ring retains the rollback window's depth: at tick {LAST_TICK} the
    // shipped prune keeps ticks LAST_TICK-9..=LAST_TICK, so B's canonical
    // past survives from tick 2 on.
    let ring_floor = LAST_TICK.saturating_sub(9).max(1);
    let expected: Vec<(u64, i64)> = (ring_floor..=LAST_TICK)
        .map(|tick| {
            let pos = if tick < HANDOFF_TICK {
                // B's canonical past: its guesses of A's entity.
                GUESS_STEP_MM * tick as i64
            } else {
                // The anchored handoff state, replayed forward under the
                // successor's rule.
                HOLDER_STEP_MM * HANDOFF_TICK as i64
                    + SUCCESSOR_STEP_MM * (tick - HANDOFF_TICK) as i64
            };
            (tick, pos)
        })
        .collect();
    assert_eq!(
        successor_ring, expected,
        "the ring must be contiguous and re-anchored: the handoff state at tick \
         {HANDOFF_TICK}, the successor's rewrite above it, B's canonical past below it"
    );

    let live = successor
        .world()
        .get::<Pose>(successor_entity)
        .expect("live pose");
    assert_eq!(
        live.pos_mm,
        HOLDER_STEP_MM * HANDOFF_TICK as i64
            + SUCCESSOR_STEP_MM * (LAST_TICK - HANDOFF_TICK) as i64,
        "the live state follows the re-anchored ring"
    );

    // The outgoing authority stopped writing at the pen-pass: its ring ends
    // there, so no tick above the pass has two writers.
    let holder_ring = holder
        .world()
        .get::<PredictionHistory<Pose>>(holder_entity)
        .expect("the holder's ring")
        .buffer();
    assert_eq!(
        holder_ring.len(),
        (HANDOFF_TICK - ring_floor + 1) as usize,
        "the holder's ring must cover exactly its pen ticks down to the window floor"
    );
    assert!(
        holder_ring
            .iter()
            .all(|(tick, _)| u64::from(tick.0) <= HANDOFF_TICK),
        "the holder kept feeding above the pen-pass tick — two feeders"
    );

    // The rollback was real: lightyear counted exactly one.
    assert_eq!(
        successor.world().resource::<PredictionMetrics>().rollbacks,
        1,
        "the handoff rollback must have executed on the real path"
    );
}
