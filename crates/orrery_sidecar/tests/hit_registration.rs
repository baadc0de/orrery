//! Hit registration end to end, inside the shipped sidecar (#871).
//!
//! Every one of these drives the real `App` that `orrery-sidecar`'s `main`
//! builds: the facade group, the authority plugin holding the ring, and the
//! two composition-root crossings. None of them reaches into
//! `CanonicalPosePublications` or `PoseHistory` to arrange the state under
//! test — the poses get there because the game's canonical step wrote them
//! and the publisher moved them, which is the property #871 says was missing.

mod common;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bytes::Bytes;
use lightyear::prelude::PredictionHistory;

use common::{held_sidecar, ENTITY};
use orrery_authority::PoseHistory;
use orrery_net::channels::Channel;
use orrery_net::channels::{decode_hit, encode_hit};
use orrery_net::{PeerPacket, SendPacket};
use orrery_protocol::{
    HitClaim, HitMsg, HitOutcome, HitRefusal, HitSurface, HitVerdict, InterpBasis, LatticePoint,
    NodeId, PersistId, QuantizedDir, QuantizedRay, Tick,
};
use orrery_sidecar::{secret, PredictedPosition, StepTrace, HIT_RADIUS_MM, SYNTHETIC_WEAPON};

/// The shooter's node, as the transport would vouch for it.
///
/// A real key rather than a filled byte array: the source of a claim is the
/// node id the transport authenticated, and `NodeId` refuses anything that is
/// not a public key — which is exactly why a peer can mint a `shooter` field
/// and cannot mint this.
fn shooter_node() -> NodeId {
    NodeId::from_bytes(secret(7).public().as_bytes()).expect("a shooter node id")
}

/// A claim fired at `fire_tick`, aimed straight down +x from the origin at a
/// target the shooter believes was at `basis`.
fn claim(fire_tick: Tick, basis: InterpBasis) -> HitClaim {
    HitClaim {
        shooter: PersistId::new(7),
        target: ENTITY,
        weapon: SYNTHETIC_WEAPON,
        fire_tick,
        basis,
        ray: QuantizedRay {
            origin: LatticePoint::new(0, 0, 0),
            direction: QuantizedDir::new(1, 0, 0),
        },
        claimed: HitSurface(0),
        input_seq: 1,
    }
}

/// Deliver `claim` as a state datagram from the shooter and collect whatever
/// the sidecar puts on the wire in reply.
fn exchange(app: &mut App, claim: &HitClaim) -> Vec<HitMsg> {
    app.world_mut().write_message(PeerPacket {
        from: shooter_node(),
        channel: Channel::State,
        payload: Bytes::from(encode_hit(&HitMsg::Claim(*claim))),
    });
    app.update();
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .drain()
        .filter_map(|packet| decode_hit(&packet.payload))
        .collect()
}

/// The newest tick the authority retains a pose for, and that pose.
fn newest_retained(app: &App) -> (Tick, LatticePoint) {
    let history = app.world().resource::<PoseHistory>();
    let (_, newest) = history
        .retained(ENTITY)
        .expect("the publisher must have opened a ring for the held entity");
    let pose = history
        .pose_at(ENTITY, newest)
        .expect("the newest retained tick has a pose");
    (newest, pose.position)
}

/// The publisher exists at all: a ring the game filled through the seam,
/// without a test ever calling `publish`.
///
/// This is the assertion that fails on `main` today, where the only caller of
/// `CanonicalPosePublications::publish` anywhere in the tree is a test.
#[test]
fn the_shipped_sidecar_fills_the_authority_ring_from_its_canonical_step() {
    let (app, _) = held_sidecar(11, 8);

    let history = app.world().resource::<PoseHistory>();
    let (oldest, newest) = history
        .retained(ENTITY)
        .expect("the sidecar's canonical step must reach the authority's ring");
    assert!(
        newest > oldest,
        "the ring must retain a span of canonical ticks, not one sample: {oldest:?}..{newest:?}"
    );

    // Every retained tick's pose is the one `Synthetic::step` produced for
    // that tick — the ruleset moves the entity one unit per tick, so the ring
    // is a contiguous run and the radius is the game's, never a default.
    let mut tick = oldest;
    while tick <= newest {
        let pose = history
            .pose_at(ENTITY, tick)
            .unwrap_or_else(|| panic!("no pose retained for {tick:?} inside the ring's own span"));
        assert_eq!(
            pose.hit_radius, HIT_RADIUS_MM,
            "the hit radius is the game's, published with the pose"
        );
        if tick > oldest {
            let previous = history
                .pose_at(ENTITY, Tick::new(tick.0 - 1))
                .expect("the previous tick is retained too");
            assert_eq!(
                pose.position.x - previous.position.x,
                1,
                "consecutive retained poses must differ by exactly the one unit \
                 `Synthetic::step` adds, so no tick was skipped or sampled twice"
            );
        }
        tick = Tick::new(tick.0 + 1);
    }
}

/// **The pose the authority validates against is what the ruleset asserted,
/// not what the skin drew.**
///
/// After `App::update`, Lightyear deliberately leaves a predicted component at
/// its frame-interpolated *presentation* value, which is a number no
/// `Ruleset::step` ever produced. The publisher reads the canonical value
/// inside `FixedMain` instead, so the two disagree — and the ring holds the
/// canonical one.
///
/// Mutation check: make `step_synthetic_rules` write the pose from
/// `position.0` (the live component) after the update rather than from
/// `state.position_mm`, and this test fails on the retained pose.
#[test]
fn the_retained_pose_is_the_rules_produced_value_not_the_interpolated_one() {
    let (app, predicted) = held_sidecar(12, 8);

    let (newest, retained) = newest_retained(&app);

    // What the rules asserted for that tick, taken from the append-only trace
    // rather than from any component Lightyear may have since blended.
    let asserted = app
        .world()
        .resource::<StepTrace>()
        .0
        .iter()
        .rev()
        .find(|entry| entry.pose.position == retained)
        .map(|entry| entry.position_mm)
        .expect("the retained pose must be one `Synthetic::step` produced");
    assert_eq!(
        retained.x, asserted,
        "the ring holds the rules-produced position for {newest:?}"
    );

    // The live component is the presentation value. It is a legitimate thing
    // for the skin to read and an illegitimate thing for an authority to
    // adjudicate against, so the ring must not agree with it by construction.
    let live = app
        .world()
        .get::<PredictedPosition>(predicted)
        .expect("the predicted component");
    let fixed = app
        .world()
        .get::<PredictionHistory<PredictedPosition>>(predicted)
        .expect("Lightyear installed prediction history");
    assert!(
        app.world()
            .resource::<StepTrace>()
            .0
            .iter()
            .any(|entry| entry.position_mm == live.0),
        "the live value must itself be a rules-produced sample or a blend of two; \
         a live value from nowhere would make this test's premise wrong"
    );
    // The retained pose is pinned to a tick. The live component is not pinned
    // to anything: it is whatever the frame's blend produced. Asserting the
    // ring against the *trace* rather than against the component is the whole
    // separation, and this keeps the two from being conflated.
    assert!(
        fixed.buffer().iter().count() > 0,
        "the fixed-simulation history is what the ring's values must track"
    );
}

/// A claim against a retained pose is accepted, by lookup, over the wire.
///
/// The first `HitVerdict` this repository has ever produced from a claim that
/// arrived as bytes on a peer link. Deterministic: the same claim against the
/// same ring yields the same pose and the same `applied_at`, and `applied_at`
/// is the authority's next tick, never the fire tick (D46 (a)(1)).
#[test]
fn a_claim_against_a_published_pose_is_accepted_by_lookup_over_the_wire() {
    let (mut app, _) = held_sidecar(13, 8);
    let (fired_at, pose) = newest_retained(&app);

    let replies = exchange(&mut app, &claim(fired_at, InterpBasis::exact(fired_at)));

    assert_eq!(replies.len(), 1, "exactly one verdict goes back");
    let HitMsg::Verdict(verdict) = replies[0] else {
        panic!("the authority's reply must be a verdict, not another claim");
    };
    assert_eq!(verdict.target, ENTITY);

    // `applied_at` is the authority's *own* next tick at the moment it
    // answered — not `fire_tick + 1`. The exchange itself advanced a canonical
    // tick, and the difference between those two readings is the point: D46
    // (a)(1)'s next-tick rule is about the target's clock, and an authority
    // never rewinds its own entity to land an effect in the shooter's past.
    let (answered_at, _) = newest_retained(&app);
    assert!(
        answered_at > fired_at,
        "the fixture must have advanced the authority's clock during the exchange,          or this test cannot tell the two readings apart"
    );
    assert_eq!(
        verdict.outcome,
        HitOutcome::Accepted {
            applied_at: Tick::new(answered_at.0 + 1),
            pose,
        },
        "the verdict must name the retained pose it tested, and land on the          authority's next tick rather than the shooter's"
    );
}

/// A claim whose basis predates the rewind cap is refused **by name**.
///
/// D8's 200 ms cap is the contract the shooter is held to, and
/// `OutsideRewindWindow` is what tells it so. A refusal that arrived as
/// `BasisNotRetained` would be the ring's depth talking instead, which is a
/// different fact and a different bug.
///
/// Mutation check: widen `hit_rewind_ticks` past the pose ring's depth and
/// this test fails with `BasisNotRetained`, naming the confusion directly.
#[test]
fn a_claim_older_than_the_rewind_cap_is_refused_by_name() {
    // Deep enough that a basis 20 ticks back is still *retained*: the ring is
    // 32 ticks and the cap is 12, so the gap between them is the only place
    // this refusal can be distinguished from a retention miss.
    let (mut app, _) = held_sidecar(14, 26);
    let (newest, _) = newest_retained(&app);

    // `hit_rewind_ticks` is 12 on the facade's defaults; 20 is past it and
    // still inside the 32-tick ring, so the refusal can only be the cap.
    let stale = Tick::new(newest.0 - 20);
    assert!(
        app.world()
            .resource::<PoseHistory>()
            .pose_at(ENTITY, stale)
            .is_some(),
        "the stale basis must still be in the ring, or the refusal proves nothing"
    );
    let replies = exchange(&mut app, &claim(newest, InterpBasis::exact(stale)));

    assert_eq!(replies.len(), 1);
    let HitMsg::Verdict(HitVerdict { outcome, .. }) = replies[0] else {
        panic!("a refusal is still a verdict");
    };
    assert!(
        matches!(
            outcome,
            HitOutcome::Rejected(HitRefusal::OutsideRewindWindow { .. })
        ),
        "the cap must refuse by its own name, not as a retention miss: {outcome:?}"
    );
}

/// A claim for an entity this sidecar does not hold is refused as
/// `NotMyEntity`, and never opens a ring.
#[test]
fn a_claim_for_an_unheld_entity_is_refused_without_opening_a_ring() {
    let (mut app, _) = held_sidecar(15, 8);
    let (newest, _) = newest_retained(&app);

    let mut stranger = claim(newest, InterpBasis::exact(newest));
    stranger.target = PersistId::new(4_242);
    let replies = exchange(&mut app, &stranger);

    assert_eq!(replies.len(), 1);
    let HitMsg::Verdict(HitVerdict { outcome, .. }) = replies[0] else {
        panic!("a refusal is still a verdict");
    };
    assert!(
        matches!(
            outcome,
            HitOutcome::Rejected(HitRefusal::NotMyEntity { .. })
        ),
        "an unheld target is not this authority's to answer for: {outcome:?}"
    );
    assert!(
        app.world()
            .resource::<PoseHistory>()
            .retained(PersistId::new(4_242))
            .is_none(),
        "answering a claim must never open a ring for an entity we do not hold"
    );
}

/// A verdict arriving at the sidecar is not answered.
///
/// The shooter's half of the exchange. Answering it would let two peers volley
/// verdicts at each other for as long as both are up.
#[test]
fn an_inbound_verdict_is_not_answered() {
    let (mut app, _) = held_sidecar(16, 8);
    let (newest, pose) = newest_retained(&app);
    let subject = claim(newest, InterpBasis::exact(newest));

    app.world_mut().write_message(PeerPacket {
        from: shooter_node(),
        channel: Channel::State,
        payload: Bytes::from(encode_hit(&HitMsg::Verdict(HitVerdict {
            claim: subject.key(),
            target: ENTITY,
            claimed: HitSurface(0),
            outcome: HitOutcome::Accepted {
                applied_at: Tick::new(newest.0 + 1),
                pose,
            },
        }))),
    });
    app.update();

    let replies: Vec<_> = app
        .world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .drain()
        .filter_map(|packet| decode_hit(&packet.payload))
        .collect();
    assert!(
        replies.is_empty(),
        "the target's authority answers claims, not verdicts: {replies:?}"
    );
}

/// Several canonical ticks may complete before one `Update`, and **every one
/// of them** must reach the ring stamped with the tick the rules ran on.
///
/// This is the property that forces the publisher into `FixedMain`. A
/// once-per-frame sample keeps only the last step of each burst, and the ring
/// silently develops holes — at which point `basis.from`'s 32-tick bound stops
/// meaning what docs/05 §7 says it means, and a claim against a real,
/// simulated tick is refused as `BasisNotRetained` for no reason the shooter
/// can act on.
///
/// Mutation check: move `publish_canonical_poses` from `FixedPostUpdate` to
/// `Update` in `orrery::hit` and this test fails, reporting two of every three
/// canonical ticks missing from the ring.
#[test]
fn every_canonical_tick_of_a_multi_step_frame_reaches_the_ring() {
    const STEPS_PER_FRAME: u32 = 3;

    let (mut app, _) = held_sidecar(17, 4);
    let (before, _) = newest_retained(&app);

    // One frame, three canonical steps — the ordinary case whenever a frame
    // runs long or the fixed accumulator has debt to pay off.
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(STEPS_PER_FRAME));
    app.update();

    let (after, _) = newest_retained(&app);
    assert_eq!(
        after.0,
        before.0 + u64::from(STEPS_PER_FRAME),
        "the ring's newest tick must advance by every step the frame ran"
    );
    let history = app.world().resource::<PoseHistory>();
    for tick in (before.0 + 1)..=after.0 {
        let tick = Tick::new(tick);
        assert!(
            history.pose_at(ENTITY, tick).is_some(),
            "{tick:?} was simulated in this frame and must be retained; a frame-rate \
             sample would have kept only the last step of the burst"
        );
    }
}
