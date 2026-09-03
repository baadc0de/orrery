//! The remaining half of #898 step 4: a `HitVerdict` produced by a real
//! authority **over a real link**.
//!
//! #938 proved a claim written straight into the world is decoded and
//! adjudicated by a real authority. It could not prove the link, because the
//! claim was injected as a `PeerPacket` message and the reply was drained out
//! of `Messages<SendPacket>` — so neither `receive_peer_packets` nor
//! `send_peer_packets` ever ran.
//!
//! Here nothing is injected. Two shipped sidecars open two real iroh
//! endpoints, connect, and:
//!
//! 1. the shooter's game writes a [`FireIntent`] naming an `orrery_ipc`
//!    [`EntityFrame`] — the value #898 step 2's schema exports;
//! 2. `orrery::hit::send_hit_claims` builds the [`HitClaim`] from that frame's
//!    basis and puts it on the state lane;
//! 3. `send_peer_packets` writes it to an iroh datagram, and the holder's
//!    `receive_peer_packets` reads it back off one;
//! 4. `orrery::hit::answer_hit_claims` looks the basis up in the ring its own
//!    canonical step filled, and sends the verdict back the same way;
//! 5. `orrery::hit::receive_hit_verdicts` matches it to the claim the shooter
//!    sent.
//!
//! Every step is production code. The test supplies the fire intent and the
//! lease grant, which are the game's and the registrar's, and nothing else.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use aeronet_iroh::endpoint::IrohEndpoint;
use bevy::ecs::system::EntityCommand;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bytes::Bytes;
use lightyear::prelude::{LocalTimelineSync, NetworkingMetadata};

use common::{grant, ENTITY};
use orrery::hit::{AdjudicatedHit, CanonicalPose, FireIntent, HitClaimLog};
use orrery_authority::PoseHistory;
use orrery_ipc::{EntityFrame, QuantizedTransform};
use orrery_net::channels::encode_hit;
use orrery_net::plugin::{PeerRegistry, ALPN};
use orrery_net::PeerPacket;
use orrery_protocol::{
    HitMsg, HitOutcome, HitSurface, HitVerdict, InterpBasis, LatticePoint, NodeId, QuantizedDir,
    Tick, UNorm16,
};
use orrery_sidecar::{secret, sidecar, spawn_predicted, SYNTHETIC_WEAPON};

/// Real endpoints, a real handshake and a real datagram round trip: generous
/// enough that a slow CI box is not a failure, short enough to be a test.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How far back the shooter's rendered basis sits, in its own ticks.
///
/// `from` is 10 ticks back, inside the 12-tick rewind cap and well inside the
/// 32-tick ring; `to` is 6 back, so the basis is a genuine four-tick *blend*
/// rather than an exact snapshot. That gap is what makes the accepted pose a
/// value distinct from the pose at `from`, at `to`, and at `fire_tick` — which
/// is how this test can tell a claim built from the exported basis apart from
/// one that synthesised `InterpBasis::exact(fire_tick)` beside the shot.
const BASIS_FROM_BACK: u64 = 10;
const BASIS_TO_BACK: u64 = 6;

fn wait_until(
    left: &mut App,
    right: &mut App,
    context: &str,
    mut condition: impl FnMut(&mut App, &mut App) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        left.update();
        right.update();
        if condition(left, right) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out {context}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn endpoint_addr(app: &mut App) -> Option<iroh::EndpointAddr> {
    app.world_mut()
        .query::<&IrohEndpoint>()
        .iter(app.world())
        .next()
        .map(IrohEndpoint::addr)
}

/// Open two sidecars on real iroh endpoints and connect them.
fn connect_pair(left: &mut App, right: &mut App) {
    wait_until(
        left,
        right,
        "for both iroh endpoints to open",
        |left, right| endpoint_addr(left).is_some() && endpoint_addr(right).is_some(),
    );

    let target = endpoint_addr(right).expect("right endpoint is open");
    let connect = {
        let mut endpoints = left.world_mut().query::<&IrohEndpoint>();
        endpoints
            .iter(left.world())
            .next()
            .expect("left endpoint is open")
            .connect(target, ALPN)
    };
    let outgoing = left.world_mut().spawn_empty().id();
    connect.apply(left.world_mut().entity_mut(outgoing));

    wait_until(
        left,
        right,
        "for both peer registries to hold the other side",
        |left, right| {
            left.world().resource::<PeerRegistry>().len() == 1
                && right.world().resource::<PeerRegistry>().len() == 1
        },
    );
}

/// Settle the prediction pipeline on a linked pair and pin one canonical tick
/// per `update`, so both sides advance together for the rest of the test.
fn settle(left: &mut App, right: &mut App) {
    wait_until(
        left,
        right,
        "for both topologies to settle on P2P",
        |l, r| {
            l.world().resource::<NetworkingMetadata>().mode.is_p2p()
                && r.world().resource::<NetworkingMetadata>().mode.is_p2p()
        },
    );
    for app in [left, right] {
        app.world_mut()
            .resource_mut::<LocalTimelineSync>()
            .set_synced(true);
        app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
    }
}

/// The tick the shooter's own canonical step last stamped for `ENTITY`.
///
/// The shooter predicts the remote entity like any peer does, so this is the
/// clock its presentation is on — and therefore the clock its basis is stated
/// in. It is *not* read from the holder: a shooter has no access to the
/// target authority's ring, and a test that peeked at it would be arranging
/// the very agreement it claims to prove.
fn shooter_tick(app: &mut App) -> Tick {
    app.world_mut()
        .query::<&CanonicalPose>()
        .iter(app.world())
        .map(|pose| pose.tick)
        .max()
        .expect("the shooter simulates its predicted copy of the target")
}

/// Everything the shooter's presentation knows about the target this frame,
/// in the exact shape `orrery_ipc` exports it.
fn rendered_frame(app: &mut App, basis: InterpBasis) -> EntityFrame {
    let translation = app
        .world_mut()
        .query::<&CanonicalPose>()
        .iter(app.world())
        .next()
        .expect("the shooter's predicted copy has a pose")
        .sample
        .position;
    EntityFrame {
        persist_id: ENTITY,
        transform: QuantizedTransform {
            translation,
            forward: QuantizedDir::new(1, 0, 0),
            up: QuantizedDir::new(0, 1, 0),
        },
        basis,
    }
}

/// Fire one shot at the frame the shooter is presenting right now.
fn fire(app: &mut App) {
    let now = shooter_tick(app);
    let basis = InterpBasis {
        from: Tick::new(now.0 - BASIS_FROM_BACK),
        to: Tick::new(now.0 - BASIS_TO_BACK),
        alpha: UNorm16::from_f64(0.5),
    };
    let target_frame = rendered_frame(app, basis);
    app.world_mut().write_message(FireIntent {
        shooter: orrery_protocol::PersistId::new(7),
        weapon: SYNTHETIC_WEAPON,
        fire_tick: now,
        target_frame,
        // The shot starts at the lattice origin and the synthetic entity
        // travels out along +x, so the ray runs straight down the axis the
        // target is on and the miss distance is the target's own offset.
        origin: LatticePoint::new(0, 0, 0),
        claimed: HitSurface(0),
    });
}

/// Collect and remove every verdict the shooter has matched to a claim.
fn adjudicated(app: &mut App) -> Vec<AdjudicatedHit> {
    app.world_mut()
        .resource_mut::<Messages<AdjudicatedHit>>()
        .drain()
        .collect()
}

/// **The proof clause.** A verdict produced by an authority that received the
/// claim over a real link.
///
/// Mutation checks, both of which this test is the only thing that catches:
///
/// - drop `send_hit_claims` from `OrreryHitRegistrationPlugin` and nothing is
///   ever put on the wire: the test times out with no verdict;
/// - build the claim with `InterpBasis::exact(intent.fire_tick)` instead of
///   `intent.target_frame.basis` and the accepted pose is the one at the fire
///   tick rather than the blend the shooter rendered — the pose assertion
///   fails by ten millimetres.
#[test]
fn a_verdict_crosses_two_real_iroh_endpoints() {
    let holder_key = secret(21);
    let shooter_key = secret(22);
    let holder_node = holder_key.public();
    let shooter_node = shooter_key.public();

    let mut holder = sidecar(holder_key.clone(), true);
    let mut shooter = sidecar(shooter_key.clone(), true);
    connect_pair(&mut holder, &mut shooter);
    settle(&mut holder, &mut shooter);

    // The holder simulates and holds the entity; the shooter predicts the very
    // same one, addressed at the holder. `PredictedBy` is what `send_hit_claims`
    // resolves the claim's destination through.
    spawn_predicted(&mut holder, holder_node, ENTITY);
    spawn_predicted(&mut shooter, holder_node, ENTITY);
    grant(&mut holder, ENTITY);

    // Far enough that the basis is well inside both the ring and the cap.
    for _ in 0..(BASIS_FROM_BACK + 4) {
        holder.update();
        shooter.update();
    }

    let mut verdicts: Vec<AdjudicatedHit> = Vec::new();
    wait_until(
        &mut holder,
        &mut shooter,
        "for a verdict to come back across the link",
        |_, shooter| {
            fire(shooter);
            verdicts.extend(adjudicated(shooter));
            verdicts
                .iter()
                .any(|hit| matches!(hit.verdict.outcome, HitOutcome::Accepted { .. }))
        },
    );

    let hit = verdicts
        .iter()
        .find(|hit| matches!(hit.verdict.outcome, HitOutcome::Accepted { .. }))
        .expect("the loop exited on an accepted verdict");

    // The verdict came from the peer the claim was addressed to, as the
    // transport vouched for it — not from the `shooter` field of any message.
    assert_eq!(
        hit.authority,
        NodeId::from_bytes(holder_node.as_bytes()).expect("the holder's node id"),
        "the verdict must be attributed to the authority the claim was sent to"
    );
    assert_ne!(
        hit.authority,
        NodeId::from_bytes(shooter_node.as_bytes()).expect("the shooter's node id"),
        "and that is not this node"
    );
    assert_eq!(hit.verdict.claim, hit.claim.key());
    assert_eq!(hit.verdict.target, ENTITY);

    // Nothing here was arranged: the holder's ring was filled by its own
    // canonical step, and the claim's basis was stated by the shooter from its
    // own clock. The verdict's pose must be the *blend* those two ticks imply.
    let HitOutcome::Accepted { pose, applied_at } = hit.verdict.outcome else {
        unreachable!("selected on Accepted")
    };
    let basis = hit.claim.basis;
    let history = holder.world().resource::<PoseHistory>();
    let from = history
        .pose_at(ENTITY, basis.from)
        .expect("the authority answered, so it retained the basis' older tick");
    let to = history
        .pose_at(ENTITY, basis.to)
        .expect("and its newer one");
    // Checked before the blend it feeds: a basis whose two ticks coincide is
    // an *exact* basis, and every assertion below would then hold for a claim
    // that never carried the shooter's rendered interval at all.
    assert_ne!(
        from.position.x, to.position.x,
        "the claim's basis must span a real interval — the shooter rendered a blend, so a \
         claim carrying `InterpBasis::exact` is not the claim it made"
    );
    let expected =
        from.position.x + (((to.position.x - from.position.x) as f64) * 0.5).round() as i64;
    assert_eq!(
        pose.x, expected,
        "the authority must re-derive the pose the shooter rendered — \
         lerp(pose({:?}), pose({:?}), 0.5) — from the basis the claim carried",
        basis.from, basis.to
    );
    if let Some(at_fire) = history.pose_at(ENTITY, hit.claim.fire_tick) {
        assert_ne!(
            pose.x, at_fire.position.x,
            "and it must not be the pose at the fire tick: that is the value a claim \
             that synthesised its own basis would have been adjudicated against"
        );
    }
    assert!(
        applied_at > basis.to,
        "an accepted effect lands on the authority's own next tick, never in the \
         shooter's rendered past"
    );
}

/// A verdict from a peer the claim was never addressed to is discarded.
///
/// The source is the [`NodeId`] the transport vouches for. Without this check
/// any connected peer could tell this node its shots landed — and the shooter
/// applies feedback from an `AdjudicatedHit`.
///
/// Mutation check: delete the `authority != packet.from` guard in
/// `receive_hit_verdicts` and this test reports one adjudicated hit instead of
/// none.
#[test]
fn a_verdict_from_the_wrong_peer_is_not_adjudicated() {
    let holder_key = secret(23);
    let shooter_key = secret(24);
    let holder_node = holder_key.public();

    let mut holder = sidecar(holder_key.clone(), true);
    let mut shooter = sidecar(shooter_key.clone(), true);
    connect_pair(&mut holder, &mut shooter);
    settle(&mut holder, &mut shooter);

    spawn_predicted(&mut shooter, holder_node, ENTITY);
    for _ in 0..(BASIS_FROM_BACK + 4) {
        holder.update();
        shooter.update();
    }

    fire(&mut shooter);
    shooter.update();
    let claim = {
        let log = shooter.world().resource::<HitClaimLog>();
        assert_eq!(log.pending_len(), 1, "one claim is outstanding");
        *log.pending(orrery_protocol::HitClaimKey {
            shooter: orrery_protocol::PersistId::new(7),
            input_seq: 0,
        })
        .expect("the claim this node sent")
    };

    // A third peer answers for an authority it is not, with a verdict whose
    // key is otherwise perfectly valid.
    let impostor = NodeId::from_bytes(secret(99).public().as_bytes()).expect("an impostor node id");
    shooter.world_mut().write_message(PeerPacket {
        from: impostor,
        channel: orrery_net::channels::Channel::State,
        payload: Bytes::from(encode_hit(&HitMsg::Verdict(HitVerdict {
            claim: claim.key(),
            target: ENTITY,
            claimed: HitSurface(0),
            outcome: HitOutcome::Accepted {
                applied_at: Tick::new(claim.fire_tick.0 + 1),
                pose: LatticePoint::new(0, 0, 0),
            },
        }))),
    });
    shooter.update();

    assert!(
        adjudicated(&mut shooter).is_empty(),
        "a verdict from a peer this node did not claim to must not be adjudicated"
    );
    assert_eq!(
        shooter.world().resource::<HitClaimLog>().unsolicited,
        1,
        "and it must be counted, so 'refused' is distinguishable from 'never arrived'"
    );
    assert_eq!(
        shooter.world().resource::<HitClaimLog>().pending_len(),
        1,
        "the real claim is still outstanding"
    );
}
