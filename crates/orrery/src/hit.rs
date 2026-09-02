//! The hit-registration crossing: canonical poses in, verdicts out.
//!
//! docs/05 §7 specifies hit registration as a protocol between two
//! authorities. Every piece of it existed before this module and none of it
//! was connected: [`HitClaim`](orrery_protocol::HitClaim) and
//! [`HitVerdict`](orrery_protocol::HitVerdict) round-trip on the wire
//! (`orrery_protocol`), the 32-tick ring and the lookup validator run
//! (`orrery_authority`), the admission cap refuses a flooding source by name
//! (#923) — but nothing wrote a pose into a ring outside a test, and nothing
//! read a claim off a link outside a test. This module is the two wires that
//! were missing, and #871's actual gap.
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

use bevy_app::{App, FixedPostUpdate, Plugin, Update};
use bevy_ecs::prelude::*;

use orrery_authority::{
    record_published_held_poses, CanonicalPosePublications, ClaimAnswer, HitRules, PersistIdentity,
    PoseHistory, PoseSample,
};
use orrery_net::channels::{decode_hit, encode_hit};
use orrery_net::{PeerPacket, SendPacket};
use orrery_protocol::{HitMsg, Tick};

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
    }
}
