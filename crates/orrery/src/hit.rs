//! The hit-registration crossing: canonical poses in, verdicts out.
//!
//! docs/05 §7 specifies hit registration as a protocol between two
//! authorities. Every piece of it existed before this module and none of it
//! was connected: [`HitClaim`](orrery_protocol::HitClaim) and
//! [`HitVerdict`](orrery_protocol::HitVerdict) round-trip on the wire
//! (`orrery_protocol`), the 32-tick ring and the lookup validator run
//! (`orrery_authority`), the admission cap refuses a flooding source by name
//! (#923) — but nothing wrote a pose into a ring outside a test, and nothing
//! read a claim off a link outside a test. This module is the wires that were
//! missing, and #871's actual gap.
//!
//! Both ends of the exchange live here, because a node is routinely both:
//! [`publish_canonical_poses`] and [`answer_hit_claims`] are the target
//! authority's half (#938), and [`send_hit_claims`] and
//! [`receive_hit_verdicts`] are the shooter's (#898 step 4). Until the shooter
//! half existed, every [`HitClaim`](orrery_protocol::HitClaim) in the tree was
//! built by a test, so the protocol `answer_hit_claims` implements had no
//! counterparty outside one.
//!
//! # Why both wires are here and not one crate lower
//!
//! The facade's module doc states the rule: a system belongs at the
//! composition root when it *crosses* two subsystems and neither may name the
//! other without inverting D15's spine. Both of these do.
//!
//! - [`publish_canonical_poses`] moves a pose from a game-written component
//!   into `orrery_authority`'s host-supplied queue. `orrery_authority` cannot
//!   define the component, because the pose's units, its hit radius, and the
//!   tick it belongs to are the *game's* and the authority layer has no
//!   ruleset. A game crate cannot name `CanonicalPosePublications`, because
//!   `orrery_games` is deliberately Bevy-free and sits below authority.
//! - [`send_hit_claims`] turns the game's fire intent, aimed at an
//!   `orrery_ipc` [`EntityFrame`] the extractor exported, into a claim on
//!   `orrery_net`'s state lane addressed to the peer `orrery_predict`'s
//!   [`PredictedBy`] names. `orrery_ipc` is Bevy-free by mechanical gate and
//!   may not name an ECS message; `orrery_predict` must not learn what a hit
//!   claim is; `orrery_net` must not learn what an interpolation basis is.
//! - [`answer_hit_claims`] takes a claim off `orrery_net`'s peer link, asks
//!   `orrery_authority`'s ring, and writes the verdict back to the link.
//!   `orrery_authority` may not depend on `orrery_net` — that is the same
//!   direction [`bind_island_membership`](crate::bind_island_membership) and
//!   [`divest_on_drain`](crate::divest_on_drain) already refuse — and
//!   `orrery_net` must not learn what a pose ring is. Only this crate depends
//!   on both.
//!
//! Naming either component from below would be the inversion #933 declined to
//! make for `PredictedBy`, for the same reason and with the same remedy.
//!
//! # Why the publisher runs in `FixedPostUpdate`
//!
//! Two properties depend on it, and neither survives an `Update`-rate sample.
//!
//! 1. **Every canonical tick is preserved.** Several fixed steps may run
//!    before one `Update`. A component read once per frame would keep only the
//!    last, and the ring's tick stamps would silently become "the most recent
//!    tick that happened to survive to a frame boundary" — at which point
//!    `basis.from`'s 32-tick bound stops meaning what docs/05 §7 says it
//!    means. Publishing per fixed step, keyed by the tick the game stamped,
//!    keeps all of them; the queue is already keyed `(tick, entity)` for
//!    exactly this.
//! 2. **The pose is what the ruleset asserted, not what the skin drew.**
//!    After `App::update`, Lightyear intentionally leaves a predicted
//!    component at its frame-interpolated *presentation* value. Sampling
//!    there would feed the authority a number no `Ruleset::step` ever
//!    produced, and validating a claim against it would make the authority
//!    judge state it did not compute — the precise failure the project's
//!    visual/simulation separation exists to prevent. Inside `FixedMain` the
//!    only value available is the one the step just wrote.
//!
//! Running inside `FixedMain` also means rollback replays it: when Lightyear
//! rewinds and re-executes ticks 7..9, the game's step rewrites
//! [`CanonicalPose`] for each and this system republishes each, so the pending
//! entry for `(tick, entity)` is *replaced* by the corrected, rules-produced
//! pose before the authority ever records it. A hit is therefore adjudicated
//! against the post-rollback canonical pose, which is the only one the
//! ruleset ever asserted.

use std::collections::BTreeMap;

use bevy_app::{App, FixedPostUpdate, Plugin, Update};
use bevy_ecs::prelude::*;

use orrery_authority::{
    record_published_held_poses, CanonicalPosePublications, ClaimAnswer, HitRules, PersistIdentity,
    PoseHistory, PoseSample,
};
use orrery_ipc::EntityFrame;
use orrery_net::channels::{decode_hit, encode_hit};
use orrery_net::{PeerPacket, SendPacket};
use orrery_predict::PredictedBy;
use orrery_protocol::{
    HitClaim, HitClaimKey, HitMsg, HitSurface, HitVerdict, LatticePoint, NodeId, PersistId,
    QuantizedDir, QuantizedRay, Tick, WeaponRef,
};

/// One entity's canonical end-of-step pose, written by the game.
///
/// The game writes this in the same system that ran its canonical step, from
/// the state that step produced. That is a contract and not merely a
/// convention: a `Transform`, a rendered mirror, an interpolated sample or a
/// predicted-but-unreconciled value are all *not* substitutes, because
/// validating a claim against one would have the target's authority judge a
/// pose it never computed.
///
/// `tick` is the universe tick the step ran on — the same clock
/// [`ContactTick::tick`](orrery_authority::ContactTick) carries, resolved
/// through [`TickBridge`](orrery_predict::TickBridge) — not a frame counter
/// and not the session tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct CanonicalPose {
    /// The universe tick this pose is the end of.
    pub tick: Tick,
    /// Where the entity was, and how big it is to be hit.
    pub sample: PoseSample,
}

impl CanonicalPose {
    /// The pose `sample` at the end of `tick`.
    #[must_use]
    pub const fn new(tick: Tick, sample: PoseSample) -> Self {
        Self { tick, sample }
    }
}

/// Publish every game-written [`CanonicalPose`] into the authority's queue.
///
/// The **production writer** `CanonicalPosePublications` did not have: before
/// this, the only caller of `publish` in the tree was a test, and
/// `crates/orrery/tests/client_group.rs` asserted the queue stayed empty after
/// a full facade run.
///
/// It decides nothing beyond whether the move applies, which is what keeps it
/// admissible as a composition-root system. Whether a published pose is
/// *retained* is not this system's call and must not become one:
/// [`record_published_held_poses`] filters by the settled live-fence set, so a
/// pose for an entity this node does not hold is discarded there, by name.
pub fn publish_canonical_poses(
    poses: Query<(&PersistIdentity, &CanonicalPose)>,
    mut publications: ResMut<CanonicalPosePublications>,
) {
    for (identity, pose) in &poses {
        publications.publish(identity.0, pose.tick, pose.sample);
    }
}

/// Answer hit claims arriving on the state lane, and send the verdicts back.
///
/// The **production consumer** the ring did not have. `decode_hit` had no
/// caller outside a test either: `orrery_predict`'s replication bridge drops
/// every non-replication sub-tag, hit traffic included, and
/// `orrery_net`'s budget meter only ever *counted* `TAG_HIT` bytes into
/// [`Lane::Hit`](orrery_net::budget::Lane) without anyone deserializing them.
///
/// The source is the [`NodeId`](orrery_protocol::NodeId) the transport
/// vouches for, never the `shooter` the claim names — a peer can mint shooter
/// ids, it cannot mint node ids — which is what makes
/// [`PoseHistory::answer`]'s per-source admission cap (#923) mean anything.
///
/// A [`HitMsg::Verdict`] arriving here is ignored: it is the *shooter's* half
/// of the exchange, and this system is the target's authority. Answering one
/// would let two peers volley verdicts at each other forever.
///
/// [`ClaimAnswer::AlreadyAnswered`] sends nothing, deliberately: the standing
/// answer for that key is already in flight, and docs/05 §7 has the shooter
/// resending until a verdict names its key.
pub fn answer_hit_claims<R: HitRules + Resource>(
    mut packets: MessageReader<PeerPacket>,
    mut history: ResMut<PoseHistory>,
    rules: Res<R>,
    mut outbound: MessageWriter<SendPacket>,
) {
    for packet in packets.read() {
        let Some(HitMsg::Claim(claim)) = decode_hit(&packet.payload) else {
            continue;
        };
        match history.answer(packet.from, &claim, &*rules) {
            ClaimAnswer::Verdict(verdict) => {
                outbound.write(SendPacket::state(
                    packet.from,
                    encode_hit(&HitMsg::Verdict(verdict)).into(),
                ));
            }
            ClaimAnswer::AlreadyAnswered { .. } => {}
        }
    }
}

/// Wires hit registration end to end for a game whose static hit facts are
/// `R`.
///
/// `R` is a [`Resource`] the game inserts and that answers [`HitRules`] — a
/// reach per weapon and a tolerance. The facade cannot invent either: they are
/// weapon design, which is the game's, and the plugin is generic rather than
/// boxed so that a game's rules table stays an ordinary value the rest of its
/// code can read.
///
/// Not a member of [`OrreryClientPlugins`](crate::OrreryClientPlugins),
/// because that group is generic over
/// [`Ruleset`](orrery_core::Ruleset) alone and a `Ruleset` does not imply
/// `HitRules`; a game adds this alongside the group once it has a rules
/// resource. Without it the seam behaves exactly as the facade's host-contract
/// table already documents: every claim returns `BasisNotRetained`.
#[derive(Debug)]
pub struct OrreryHitRegistrationPlugin<R: HitRules + Resource> {
    marker: core::marker::PhantomData<fn() -> R>,
}

impl<R: HitRules + Resource> Default for OrreryHitRegistrationPlugin<R> {
    fn default() -> Self {
        Self {
            marker: core::marker::PhantomData,
        }
    }
}

impl<R: HitRules + Resource> OrreryHitRegistrationPlugin<R> {
    /// The plugin for a game whose static hit facts live in `R`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<R: HitRules + Resource> Plugin for OrreryHitRegistrationPlugin<R> {
    fn build(&self, app: &mut App) {
        // In `FixedMain`, so rollback replays it and every canonical tick is
        // published — see this module's docs.
        app.add_systems(FixedPostUpdate, publish_canonical_poses);
        // After the drain, so a claim answered this frame sees the poses this
        // frame published rather than last frame's ring.
        app.add_systems(
            Update,
            answer_hit_claims::<R>.after(record_published_held_poses),
        );
        // The shooter's half. `send_hit_claims` runs before the link drains
        // `SendPacket`, and `receive_hit_verdicts` reads the same inbound
        // `PeerPacket` stream `answer_hit_claims` does — the two sort inbound
        // traffic by which half of the exchange it belongs to, and a node is
        // routinely both halves at once for different entities.
        app.init_resource::<HitClaimLog>();
        app.add_message::<FireIntent>();
        app.add_message::<AdjudicatedHit>();
        app.add_systems(Update, (send_hit_claims, receive_hit_verdicts));
    }
}

// ---------------------------------------------------------------------------
// The shooter's half: a claim built from the frame the engine was handed.
// ---------------------------------------------------------------------------

/// The game's decision to fire, addressed at a frame it actually rendered.
///
/// This is the *only* input to [`send_hit_claims`], and it is deliberately not
/// a claim: everything a claimant must not be trusted to state for itself is
/// derived here rather than accepted from the game. What the game supplies is
/// what only the game knows — that it fired, with which weapon, from where, at
/// which presented frame, on which of its own ticks.
///
/// # Why the target arrives as an [`EntityFrame`] and not a [`PersistId`]
///
/// docs/05 §7 has the authority re-derive the target's pose at *the basis the
/// shooter rendered from*, and refuse anything else. A claim whose basis was
/// synthesised beside the shot — `InterpBasis::exact(fire_tick)` is the
/// tempting one — is a claim about a pose the shooter never presented, and the
/// authority would then adjudicate a shot nobody took. [`EntityFrame`] is the
/// value `orrery_ipc` (#898 step 2) hands the engine for exactly this reason;
/// its own docs say "consumers use this exact value when constructing a later
/// hit claim". Taking the whole frame — basis *and* the translation the ray is
/// aimed at — is what makes that true rather than aspirational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Message)]
pub struct FireIntent {
    /// The entity that fired.
    pub shooter: PersistId,
    /// The weapon fired, resolved by the *target's* ruleset.
    pub weapon: WeaponRef,
    /// The shooter's universe tick when the fire input was sampled.
    ///
    /// The game's, because only the game's canonical step knows which tick its
    /// input belonged to; the facade sees frames, not ticks.
    pub fire_tick: Tick,
    /// The frame the shooter presented the target from, as exported.
    pub target_frame: EntityFrame,
    /// Where the shot started, on the position lattice.
    pub origin: LatticePoint,
    /// The surface the shooter's presentation predicted. Echoed, never checked.
    pub claimed: HitSurface,
}

/// One adjudicated shot: the claim this node sent, and the verdict its
/// target's authority returned over the link.
///
/// Emitted only for a verdict that answers a claim this node actually made, to
/// the peer it made it to — see [`receive_hit_verdicts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Message)]
pub struct AdjudicatedHit {
    /// The peer whose authority answered, as the transport vouched for it.
    pub authority: NodeId,
    /// The claim as sent.
    pub claim: HitClaim,
    /// The verdict as received.
    pub verdict: HitVerdict,
}

/// The claims this node has sent and not yet had answered, and the sequence
/// they are numbered from.
///
/// `input_seq` is minted here rather than by the game because it is half of
/// the ack key: `(shooter, input_seq)` is what a verdict echoes and what the
/// target's [`HitClaimGate`](orrery_protocol::HitClaimGate) dedupes resends
/// by. A game that numbered its own would eventually reuse a key across two
/// different shots, and the authority would coalesce the second into the
/// first's standing answer.
#[derive(Debug, Default, Resource)]
pub struct HitClaimLog {
    next_seq: u16,
    pending: BTreeMap<HitClaimKey, (NodeId, HitClaim)>,
    /// Intents dropped because the frame named an entity with no known
    /// authority — nobody to send the claim to.
    pub unaddressed: u64,
    /// Intents dropped because the ray had no direction: the origin and the
    /// rendered translation coincide.
    pub degenerate: u64,
    /// Verdicts discarded because they answered no claim this node made, or
    /// came from a peer this node did not claim to.
    pub unsolicited: u64,
}

impl HitClaimLog {
    /// The claim awaiting a verdict under `key`, if any.
    #[must_use]
    pub fn pending(&self, key: HitClaimKey) -> Option<&HitClaim> {
        self.pending.get(&key).map(|(_, claim)| claim)
    }

    /// How many claims are outstanding.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// The largest component a [`QuantizedDir`] may carry, as an `i64`.
const DIR_SCALE: i64 = i16::MAX as i64;

/// The direction from `origin` towards `target`, scaled into `i16`.
///
/// Magnitude is irrelevant to the validator — [`QuantizedRay`] carries no
/// reach, and the weapon's reach is the target ruleset's fact — so this scales
/// the largest component to `i16::MAX` and keeps the others in proportion.
/// Scaling *up* rather than truncating matters: a target 40 m away on a
/// millimetre lattice has components in the tens of thousands, and a naive
/// `as i16` would wrap a straight-ahead shot into a sideways one.
///
/// `None` when the two points coincide, which is the one direction that means
/// nothing and which the authority refuses as
/// [`HitRefusal::MalformedRay`](orrery_protocol::HitRefusal::MalformedRay).
fn direction_towards(origin: LatticePoint, target: LatticePoint) -> Option<QuantizedDir> {
    let delta = [
        target.x - origin.x,
        target.y - origin.y,
        target.z - origin.z,
    ];
    let largest = delta.iter().map(|component| component.abs()).max()?;
    if largest == 0 {
        return None;
    }
    let scale = |component: i64| -> i16 {
        // Widened before the multiply: the lattice is `i64` millimetres and
        // `component * DIR_SCALE` overflows an `i64` at ~281 km.
        let scaled = i128::from(component) * i128::from(DIR_SCALE) / i128::from(largest);
        // In range by construction — `|component| <= largest` — but clamped
        // rather than cast, so an arithmetic mistake here can never silently
        // become a different direction.
        scaled.clamp(i128::from(i16::MIN), i128::from(i16::MAX)) as i16
    };
    Some(QuantizedDir::new(
        scale(delta[0]),
        scale(delta[1]),
        scale(delta[2]),
    ))
}

/// Turn each [`FireIntent`] into a [`HitClaim`] on the state lane, addressed
/// to the peer holding the target.
///
/// The **production shooter-side claim path** the tree did not have: before
/// this, every `HitClaim` in the repository was built by a test, so the
/// exchange `answer_hit_claims` implements had no counterparty outside one.
///
/// # Why here and not in a lower crate
///
/// Same reason as this module's other two systems, from the other direction.
/// The claim needs `orrery_ipc`'s exported frame (the schema crate, which is
/// Bevy-free and may not name an ECS resource), `orrery_predict`'s
/// [`PredictedBy`] to learn *which peer* holds the target, `orrery_protocol`'s
/// claim, and `orrery_net`'s lane to put it on. No one of those crates may
/// name the other three without inverting D15's spine, and #933 declined
/// exactly this inversion for `PredictedBy` itself.
///
/// # The address is the authority, never the claim
///
/// A claim goes to the [`NodeId`] on the target's [`PredictedBy`] — the peer
/// the prediction layer already believes holds it. A target with no
/// `PredictedBy` is unaddressable and the intent is dropped and counted, not
/// broadcast: a claim sent to every peer would be answered `NotMyEntity` by
/// all but one, and each of those answers spends a token in *that* peer's
/// admission gate for no reason.
pub fn send_hit_claims(
    mut intents: MessageReader<FireIntent>,
    targets: Query<&PredictedBy>,
    mut log: ResMut<HitClaimLog>,
    mut outbound: MessageWriter<SendPacket>,
) {
    for intent in intents.read() {
        let target = intent.target_frame.persist_id;
        let Some(authority) = targets
            .iter()
            .find(|predicted| predicted.persist_id == target)
            .map(|predicted| predicted.authority)
        else {
            log.unaddressed += 1;
            continue;
        };
        let Some(direction) =
            direction_towards(intent.origin, intent.target_frame.transform.translation)
        else {
            log.degenerate += 1;
            continue;
        };

        let input_seq = log.next_seq;
        log.next_seq = log.next_seq.wrapping_add(1);
        let claim = HitClaim {
            shooter: intent.shooter,
            target,
            weapon: intent.weapon,
            fire_tick: intent.fire_tick,
            // The rendered basis, verbatim. Synthesising one here would make
            // the authority adjudicate a pose the shooter never presented.
            basis: intent.target_frame.basis,
            ray: QuantizedRay {
                origin: intent.origin,
                direction,
            },
            claimed: intent.claimed,
            input_seq,
        };
        log.pending.insert(claim.key(), (authority, claim));
        outbound.write(SendPacket::state(
            authority,
            encode_hit(&HitMsg::Claim(claim)).into(),
        ));
    }
}

/// Match inbound [`HitMsg::Verdict`]s to the claims this node sent.
///
/// The shooter's half of `answer_hit_claims`. A [`HitMsg::Claim`] arriving
/// here is ignored: that is the target authority's half, and this system is
/// the shooter.
///
/// Two things are checked before a verdict becomes an [`AdjudicatedHit`], and
/// both are about a peer that is not the one we asked:
///
/// 1. the key must name a claim this node has outstanding — a verdict for a
///    shot nobody fired is not evidence of anything;
/// 2. the source must be the peer that claim was *addressed to*. The source is
///    the [`NodeId`] the transport vouches for, so a third peer cannot answer
///    for an authority it is not. Without this check any connected peer could
///    tell this node its shots landed.
///
/// Both discards are counted rather than logged, so a test can tell "no
/// verdict arrived" from "a verdict arrived and was refused".
pub fn receive_hit_verdicts(
    mut packets: MessageReader<PeerPacket>,
    mut log: ResMut<HitClaimLog>,
    mut adjudicated: MessageWriter<AdjudicatedHit>,
) {
    for packet in packets.read() {
        let Some(HitMsg::Verdict(verdict)) = decode_hit(&packet.payload) else {
            continue;
        };
        let Some((authority, claim)) = log.pending.get(&verdict.claim).copied() else {
            log.unsolicited += 1;
            continue;
        };
        if authority != packet.from {
            log.unsolicited += 1;
            continue;
        }
        log.pending.remove(&verdict.claim);
        adjudicated.write(AdjudicatedHit {
            authority,
            claim,
            verdict,
        });
    }
}
