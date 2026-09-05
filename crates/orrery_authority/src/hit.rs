//! The authority's pose ring and the hit-claim validator (docs/05 §7).
//!
//! An authority validates a [`HitClaim`] against **its own retained pose
//! history, never by resimulating the past**. D47 (a)(1) forbids an authority
//! rewinding its own entity, and docs/05 §2 case 3 says why: its signed input
//! log is straight-line by construction, so a claim about tick `T − k` is
//! answered by looking up what the entity's pose *was* at `T − k`, and the
//! effect — if any — lands at the authority's current tick, logged at arrival.
//!
//! The ring is the whole mechanism. Each authoritative entity keeps
//! `pose_history_ticks` (32 on D16's defaults, ≈ 533 ms) of per-tick poses,
//! written once per tick after the step that produced them. A claim names an
//! interpolation basis; the validator reads the two basis ticks out of the
//! ring, blends them exactly as the shooter did, and tests the shooter's ray
//! against that pose. Nothing here can reach a ruleset's `step`, and the type
//! signature says so: [`PoseHistory::validate`] takes `&self`, a claim, and
//! the ruleset's *static* facts ([`HitRules`]) — no state, no inputs, no RNG.
//!
//! What the ring does not do: it is not a prediction ring (that is
//! `orrery_predict`'s 16-tick history, on the *predicting* side), and it
//! records nothing about entities this peer does not hold. An authority that
//! loses an entity calls [`PoseHistory::forget`], and a claim against it is
//! refused as [`HitRefusal::NotMyEntity`] from then on.

use std::collections::{BTreeMap, BTreeSet};

use bevy_ecs::prelude::*;
use orrery_protocol::{
    Admission, HitClaim, HitClaimCap, HitClaimGate, HitClaimKey, HitOutcome, HitRefusal,
    HitVerdict, HitWindow, LatticePoint, NodeId, PersistId, Tick, WeaponRef,
};

/// How many source peers [`PoseHistory`] keeps an admission gate for.
///
/// D6's per-cell player ceiling, the same figure D25 caps the `Expire`
/// fan-out at: no honest authority is shot at by more peers than share its
/// cell. Past it the least-recently-seen gate is evicted, which only ever
/// makes an evicted source *more* admissible (it returns to a full bucket),
/// so a flood of fresh node ids cannot lock an honest shooter out.
pub const MAX_CLAIM_SOURCES: usize = 128;

/// What [`PoseHistory::answer`] did with one claim datagram.
///
/// Every arm is named because a claim the authority does not answer is
/// indistinguishable, to the shooter, from a lost datagram (docs/05 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimAnswer {
    /// Send this verdict back to the source. Accepted, or refused by name —
    /// including [`HitRefusal::OverClaimRate`] when the source is over its
    /// [`HitClaimCap`].
    Verdict(HitVerdict),
    /// A copy of a claim (or of an over-cap flood) this authority answered
    /// earlier in the same tick; that answer is in flight. Nothing to send.
    AlreadyAnswered {
        /// The key of the coalesced copy.
        key: HitClaimKey,
        /// The tick the standing answer was given at.
        at: Tick,
    },
}

/// One retained pose: where the entity was at the end of a tick, and how big
/// it is for the purpose of being hit.
///
/// The hit volume is a sphere. That is a synthetic simplification and it is
/// deliberate: the shape of a target is the game's, and the platform's job is
/// to prove the *lookup*, which a sphere does as well as a skeleton. A game
/// with body parts derives its surface from this pose on the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoseSample {
    /// Position on the lattice at the end of the tick.
    pub position: LatticePoint,
    /// Hit radius, in lattice units (millimetres).
    pub hit_radius: u32,
}

/// Canonical end-of-tick poses published by the game for hit validation.
///
/// This is a host-supplied, post-canonical-step observation queue. After each
/// canonical step, game code calls [`Self::publish`] once for every persistent
/// entity this node held during that step. The authority plugin drains the
/// queue in `Update`, records only entities for which it still holds a live
/// fence, and discards publications for unheld entities by name.
///
/// The sample must come from the game's canonical simulation state. A Bevy
/// `Transform`, rendered mirror, interpolated pose, or predicted pose is not a
/// substitute: validating against one would make the authority judge a claim
/// against state it did not compute. This type deliberately contains no Bevy
/// transform conversion that could make that mistake look sanctioned.
///
/// Each publication carries its own [`Tick`] instead of borrowing
/// [`crate::ContactTick::tick`]. `ContactTick` names the physics step that
/// produced a contact report, while a game's canonical step need not be its
/// physics step. Both ticks must come from the same universe-global tick source
/// when they describe the same step; publishing different values is a host
/// contract violation. Keeping the tick on the queued item also preserves
/// every canonical tick when several fixed steps run before one `Update`.
///
/// A second publication for the same `(tick, entity)` replaces the pending
/// sample. The consumer therefore calls [`PoseHistory::record`] exactly once
/// per published tick per held entity.
#[derive(Debug, Default, Resource)]
pub struct CanonicalPosePublications {
    pending: BTreeMap<(Tick, PersistId), PoseSample>,
}

impl CanonicalPosePublications {
    /// Publish `entity`'s canonical pose at the end of `tick`.
    ///
    /// Call this from the game-owned system that has just completed the
    /// canonical step, never from presentation extraction or interpolation.
    pub fn publish(&mut self, entity: PersistId, tick: Tick, sample: PoseSample) {
        self.pending.insert((tick, entity), sample);
    }

    /// Whether no canonical poses are waiting for the authority consumer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The number of distinct `(tick, entity)` publications waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }
}

/// The ruleset's static facts a hit validator needs.
///
/// Static, not simulated: a reach per weapon, and a tolerance. Both are
/// numbers the ruleset knows without stepping anything, which is what keeps
/// validation a lookup. Rate and damage are not here — rate is a per-shooter
/// history the witness validators audit against the shooter's log (D10), and
/// damage is weapon design, which this module has no opinion on.
pub trait HitRules {
    /// How far `weapon` reaches, in lattice units, or `None` for a weapon the
    /// ruleset does not know.
    fn weapon_reach(&self, weapon: WeaponRef) -> Option<u32>;

    /// How far outside the hit radius a ray may pass and still count, in
    /// lattice units. Absorbs the shooter's quantization of the direction and
    /// the blend; zero means the sphere is the sphere.
    fn hit_tolerance(&self) -> u32 {
        0
    }
}

/// A fixed-depth ring of per-tick poses for one entity.
///
/// Slot `tick % depth` holds the sample for `tick`, stamped with the tick so
/// a lookup can tell a retained sample from the one it overwrote. Depth zero
/// retains nothing.
#[derive(Debug, Clone)]
pub struct PoseRing {
    slots: Vec<Option<(Tick, PoseSample)>>,
}

impl PoseRing {
    /// An empty ring retaining `depth` ticks.
    #[must_use]
    pub fn new(depth: u16) -> Self {
        Self {
            slots: vec![None; usize::from(depth)],
        }
    }

    /// How many ticks this ring retains.
    #[must_use]
    pub fn depth(&self) -> u16 {
        u16::try_from(self.slots.len()).expect("ring depth was a u16")
    }

    /// Record the pose at the end of `tick`, evicting whatever `tick − depth`
    /// held.
    pub fn record(&mut self, tick: Tick, sample: PoseSample) {
        let Some(slot) = self.slot(tick) else {
            return;
        };
        self.slots[slot] = Some((tick, sample));
    }

    /// The pose retained for exactly `tick`, if it is still in the ring.
    #[must_use]
    pub fn get(&self, tick: Tick) -> Option<PoseSample> {
        let slot = self.slot(tick)?;
        match self.slots[slot] {
            Some((stamped, sample)) if stamped == tick => Some(sample),
            _ => None,
        }
    }

    /// The oldest and newest ticks retained, or `None` for an empty ring.
    #[must_use]
    pub fn retained(&self) -> Option<(Tick, Tick)> {
        let mut bounds: Option<(Tick, Tick)> = None;
        for (tick, _) in self.slots.iter().flatten() {
            bounds = Some(match bounds {
                None => (*tick, *tick),
                Some((oldest, newest)) => (oldest.min(*tick), newest.max(*tick)),
            });
        }
        bounds
    }

    fn slot(&self, tick: Tick) -> Option<usize> {
        let depth = self.slots.len();
        if depth == 0 {
            return None;
        }
        let depth = u64::try_from(depth).expect("ring depth fits in u64");
        Some(usize::try_from(tick.0 % depth).expect("slot index is below depth"))
    }
}

/// The authority's pose history: one [`PoseRing`] per entity it holds, and
/// the [`HitWindow`] it validates claims within.
///
/// Sized by `PredictConfig::hit_window()` in `orrery_predict` and handed to
/// [`OrreryAuthorityPlugin`](crate::OrreryAuthorityPlugin) by the facade;
/// the default is [`HitWindow::CLOSED`], which refuses every claim, so a
/// plugin that was never told its window fails closed rather than with a
/// figure copied from a document.
///
/// Also owns one [`HitClaimGate`] per source peer, keyed by the [`NodeId`] the
/// transport vouches for rather than the `shooter` a claim names — a peer
/// can mint shooter ids, not node ids — so [`PoseHistory::answer`] refuses an
/// over-cap source by name before any pose is looked up (#923). The cap is
/// derived from the window ([`HitClaimCap::for_window`]): the closed default
/// admits nothing, by the same fail-closed rule.
#[derive(Debug, Resource)]
pub struct PoseHistory {
    window: HitWindow,
    cap: HitClaimCap,
    rings: BTreeMap<PersistId, PoseRing>,
    gates: BTreeMap<NodeId, HitClaimGate>,
    now: Option<Tick>,
}

impl Default for PoseHistory {
    fn default() -> Self {
        Self::new(HitWindow::CLOSED)
    }
}

impl PoseHistory {
    /// An empty history validating within `window`.
    #[must_use]
    pub fn new(window: HitWindow) -> Self {
        Self {
            window,
            cap: HitClaimCap::for_window(window),
            rings: BTreeMap::new(),
            gates: BTreeMap::new(),
            now: None,
        }
    }

    /// The window claims are validated within.
    #[must_use]
    pub fn window(&self) -> HitWindow {
        self.window
    }

    /// The per-source admission cap claims are gated by.
    #[must_use]
    pub fn claim_cap(&self) -> HitClaimCap {
        self.cap
    }

    /// The admission gate for `source`, if it has sent a claim.
    #[must_use]
    pub fn claim_gate(&self, source: NodeId) -> Option<&HitClaimGate> {
        self.gates.get(&source)
    }

    /// The newest tick any ring has recorded — the authority's present, as
    /// far as its history knows.
    #[must_use]
    pub fn current_tick(&self) -> Option<Tick> {
        self.now
    }

    /// Whether this authority retains history for `entity`.
    #[must_use]
    pub fn holds(&self, entity: PersistId) -> bool {
        self.rings.contains_key(&entity)
    }

    /// The oldest and newest ticks retained for `entity`.
    #[must_use]
    pub fn retained(&self, entity: PersistId) -> Option<(Tick, Tick)> {
        self.rings.get(&entity)?.retained()
    }

    /// The pose retained for `entity` at exactly `tick`.
    #[must_use]
    pub fn pose_at(&self, entity: PersistId, tick: Tick) -> Option<PoseSample> {
        self.rings.get(&entity)?.get(tick)
    }

    /// Record `entity`'s pose at the end of `tick`.
    ///
    /// Called once per tick per held entity, after the step that produced the
    /// pose. The first record for an entity opens its ring at the window's
    /// depth.
    pub fn record(&mut self, entity: PersistId, tick: Tick, sample: PoseSample) {
        let depth = self.window.history_ticks;
        self.rings
            .entry(entity)
            .or_insert_with(|| PoseRing::new(depth))
            .record(tick, sample);
        self.now = Some(self.now.map_or(tick, |now| now.max(tick)));
    }

    /// Drop `entity`'s history — on authority loss, handoff, or despawn.
    pub fn forget(&mut self, entity: PersistId) {
        self.rings.remove(&entity);
    }

    /// Answer one claim datagram from `source`: admit it through the source's
    /// [`HitClaimGate`], then [`validate`](Self::validate) it.
    ///
    /// This is the entry point for a claim that arrived on the wire.
    /// [`validate`](Self::validate) alone is the pure lookup and spends
    /// nothing; calling it directly for wire traffic would leave the hit lane
    /// uncapped on this side. The gate runs *first* — before the ring is
    /// consulted, before `NotMyEntity` — because what it bounds is the
    /// verdicts this authority uploads, and every arm of the verdict costs
    /// the same bytes.
    ///
    /// `now` is the ring's newest recorded tick, which is the only clock a
    /// pose history has; an authority that has recorded nothing gates at
    /// tick zero, and its closed window refuses everything anyway.
    pub fn answer(
        &mut self,
        source: NodeId,
        claim: &HitClaim,
        rules: &impl HitRules,
    ) -> ClaimAnswer {
        let now = self.now.unwrap_or(Tick::new(0));
        let key = claim.key();
        let admission = self.gate_for(source).admit(key, now);
        match admission {
            Admission::Admit => ClaimAnswer::Verdict(self.validate(claim, rules)),
            Admission::Refuse(refusal) => ClaimAnswer::Verdict(HitVerdict {
                claim: key,
                target: claim.target,
                claimed: claim.claimed,
                outcome: HitOutcome::Rejected(refusal),
            }),
            Admission::AlreadyAnswered { at } => ClaimAnswer::AlreadyAnswered { key, at },
        }
    }

    fn gate_for(&mut self, source: NodeId) -> &mut HitClaimGate {
        if !self.gates.contains_key(&source) && self.gates.len() >= MAX_CLAIM_SOURCES {
            let stalest = self
                .gates
                .iter()
                .min_by_key(|(_, gate)| gate.last_seen())
                .map(|(node, _)| *node)
                .expect("the gate table is non-empty when full");
            self.gates.remove(&stalest);
        }
        let cap = self.cap;
        self.gates
            .entry(source)
            .or_insert_with(|| HitClaimGate::new(cap))
    }

    /// Validate `claim` against retained history: a lookup, never a
    /// resimulation.
    ///
    /// Pure: spends no admission token. Wire traffic goes through
    /// [`answer`](Self::answer), which gates first. The checks, in the order
    /// a refusal names them:
    ///
    /// 1. the target is an entity this authority holds a ring for;
    /// 2. the ray has a direction;
    /// 3. the basis is ordered, `from <= to <= fire_tick`;
    /// 4. `fire_tick − basis.from` is within the rewind cap —
    ///    [`HitRefusal::OutsideRewindWindow`], by name;
    /// 5. both basis ticks are still in the ring;
    /// 6. the ruleset knows the weapon;
    /// 7. the re-derived pose lies along the ray, ahead of its origin and
    ///    within reach;
    /// 8. the ray passes within the hit radius plus tolerance.
    ///
    /// An accepted verdict names the tick the effect lands on — the
    /// authority's next tick, never the fire tick — and the pose it tested.
    #[must_use]
    pub fn validate(&self, claim: &HitClaim, rules: &impl HitRules) -> HitVerdict {
        let outcome = match self.check(claim, rules) {
            Ok(pose) => HitOutcome::Accepted {
                applied_at: Tick::new(self.now.map_or(0, |now| now.0).saturating_add(1)),
                pose,
            },
            Err(refusal) => HitOutcome::Rejected(refusal),
        };
        HitVerdict {
            claim: claim.key(),
            target: claim.target,
            claimed: claim.claimed,
            outcome,
        }
    }

    fn check(&self, claim: &HitClaim, rules: &impl HitRules) -> Result<LatticePoint, HitRefusal> {
        let ring = self
            .rings
            .get(&claim.target)
            .ok_or(HitRefusal::NotMyEntity {
                target: claim.target,
            })?;

        if claim.ray.direction.is_zero() {
            return Err(HitRefusal::MalformedRay);
        }

        let basis = claim.basis;
        if !basis.is_ordered() || basis.to > claim.fire_tick {
            return Err(HitRefusal::MalformedBasis {
                basis,
                fire_tick: claim.fire_tick,
            });
        }

        let rewind_ticks = claim.rewind_ticks().ok_or(HitRefusal::MalformedBasis {
            basis,
            fire_tick: claim.fire_tick,
        })?;
        let cap_ticks = self.window.rewind_ticks;
        if rewind_ticks > u64::from(cap_ticks) {
            return Err(HitRefusal::OutsideRewindWindow {
                rewind_ticks,
                cap_ticks,
            });
        }

        let lookup = |tick: Tick| {
            ring.get(tick).ok_or_else(|| {
                let bounds = ring.retained();
                HitRefusal::BasisNotRetained {
                    tick,
                    oldest_retained: bounds.map(|(oldest, _)| oldest),
                    newest_retained: bounds.map(|(_, newest)| newest),
                }
            })
        };
        let from = lookup(basis.from)?;
        let to = lookup(basis.to)?;

        let reach = rules
            .weapon_reach(claim.weapon)
            .ok_or(HitRefusal::UnknownWeapon {
                weapon: claim.weapon,
            })?;

        let pose = blend(from.position, to.position, basis.alpha.to_f64());
        let radius = from.hit_radius.max(to.hit_radius);
        let allowed = radius.saturating_add(rules.hit_tolerance());

        let (along_ray, miss_distance) = approach(claim.ray.origin, claim.ray.direction, pose);
        if along_ray < 0.0 || along_ray > f64::from(reach) {
            return Err(HitRefusal::OutOfReach {
                along_ray: along_ray.round() as i64,
                reach,
            });
        }
        if miss_distance > f64::from(allowed) {
            return Err(HitRefusal::Miss {
                miss_distance: miss_distance.round() as u32,
                allowed,
            });
        }
        Ok(pose)
    }
}

/// Drain game-published canonical poses into the rings of entities held here.
///
/// The live fence set is the filter, not an ECS marker or a status supplied by
/// the game. Publications for any other [`PersistId`] are ignored and never
/// open a ring. Conversely, every held entity gets an empty ring even when the
/// host publishes nothing, so a claim against a held-but-unpublished entity is
/// refused as [`HitRefusal::BasisNotRetained`] rather than mistaken for an
/// entity held elsewhere. Rings are dropped as soon as their fence is lost.
pub fn record_published_held_poses(
    mut publications: ResMut<CanonicalPosePublications>,
    state: Res<crate::AuthorityState>,
    mut history: ResMut<PoseHistory>,
) {
    let held: BTreeSet<_> = state.held_leases().map(|(persist, _)| persist).collect();

    history.rings.retain(|persist, _| held.contains(persist));
    let depth = history.window.history_ticks;
    for persist in &held {
        history
            .rings
            .entry(*persist)
            .or_insert_with(|| PoseRing::new(depth));
    }

    for ((tick, persist), sample) in std::mem::take(&mut publications.pending) {
        if held.contains(&persist) {
            history.record(persist, tick, sample);
        }
    }
}

/// `lerp(a, b, alpha)` per axis, rounded back onto the lattice.
fn blend(a: LatticePoint, b: LatticePoint, alpha: f64) -> LatticePoint {
    let lerp = |from: i64, to: i64| -> i64 {
        // Exact at both ends: `from + (to - from) * alpha` in f64 is `from`
        // at 0 and `to` at 1 up to rounding, and the round puts it back.
        let delta = (to as f64 - from as f64) * alpha;
        from + delta.round() as i64
    };
    LatticePoint::new(lerp(a.x, b.x), lerp(a.y, b.y), lerp(a.z, b.z))
}

/// Distance along the normalized ray to the closest approach to `point`, and
/// the distance from the ray to `point` at that approach.
///
/// Only `+ − × ÷ √` — IEEE-754 pins every one of those bit for bit, which is
/// what lets two authorities on different platforms agree on a verdict without
/// this being verifiable-core code.
fn approach(
    origin: LatticePoint,
    direction: orrery_protocol::QuantizedDir,
    point: LatticePoint,
) -> (f64, f64) {
    let d = [
        f64::from(direction.x),
        f64::from(direction.y),
        f64::from(direction.z),
    ];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let d = [d[0] / len, d[1] / len, d[2] / len];
    let v = [
        point.x as f64 - origin.x as f64,
        point.y as f64 - origin.y as f64,
        point.z as f64 - origin.z as f64,
    ];
    let along = v[0] * d[0] + v[1] * d[1] + v[2] * d[2];
    let v_sq = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    let miss = (v_sq - along * along).max(0.0).sqrt();
    (along, miss)
}

#[cfg(test)]
mod tests {
    //! The synthetic ruleset here follows `orrery_sim_host`'s
    //! `OffLatticeRuleset` precedent: the smallest `Ruleset` that produces a
    //! pose per tick, plus a step counter so a test can assert how many times
    //! the rules ran. It is not a game and not Regolith.

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use orrery_core::{
        CodecError, CoreCodec, OrderedInputs, Quantized, Ruleset, StateView, StepOutput, TickRng,
    };
    use orrery_protocol::{
        HitSurface, InterpBasis, QuantizedDir, QuantizedRay, RulesetId, UNorm16, UniverseSeed,
    };
    use orrery_sim_host::{NoEventRouting, SimulationHost, SimulationHostConfig, TickCount};

    use super::*;

    /// Millimetres the walker moves along +x every tick.
    const STRIDE_MM: i64 = 100;
    const RADIUS_MM: u32 = 500;
    const RIFLE: WeaponRef = WeaponRef(1);
    const RIFLE_REACH_MM: u32 = 100_000;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct WalkerState {
        x_mm: i64,
    }

    impl Quantized for WalkerState {
        fn quantize(&mut self) {}
    }

    impl CoreCodec for WalkerState {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.x_mm.to_le_bytes());
        }

        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            let raw: [u8; 8] = bytes
                .try_into()
                .map_err(|_| CodecError("walker state is 8 bytes"))?;
            Ok(Self {
                x_mm: i64::from_le_bytes(raw),
            })
        }
    }

    #[derive(Clone)]
    enum Never {}

    impl CoreCodec for Never {
        fn encode(&self, _out: &mut Vec<u8>) {
            match *self {}
        }

        fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
            Err(CodecError("walker has no input or event"))
        }
    }

    /// Walks +x at a fixed stride and counts every step it is asked for.
    struct Walker {
        steps: Arc<AtomicU64>,
    }

    impl Ruleset for Walker {
        const OVERFLOW_IS_CANONICAL: bool = false;
        type CoreState = WalkerState;
        type CoreInput = Never;
        type CoreEvent = Never;

        fn id(&self) -> RulesetId {
            RulesetId {
                version: 1,
                digest: [0x5A; 32],
            }
        }

        fn step(
            &self,
            view: &mut StateView<'_, Self::CoreState>,
            _inputs: &OrderedInputs<'_, Self::CoreInput>,
            _rng: &mut TickRng,
        ) -> StepOutput<Self::CoreEvent> {
            self.steps.fetch_add(1, Ordering::SeqCst);
            view.own_mut().x_mm += STRIDE_MM;
            StepOutput::default()
        }
    }

    impl HitRules for Walker {
        fn weapon_reach(&self, weapon: WeaponRef) -> Option<u32> {
            (weapon == RIFLE).then_some(RIFLE_REACH_MM)
        }
    }

    const WINDOW: HitWindow = HitWindow::new(12, 32);
    const TARGET: PersistId = PersistId::new(9);
    const SHOOTER: PersistId = PersistId::new(7);

    /// An authority peer: the host running the rules, and the ring it fills
    /// from them once per tick.
    struct AuthorityPeer {
        host: SimulationHost<Walker, NoEventRouting>,
        history: PoseHistory,
        steps: Arc<AtomicU64>,
    }

    impl AuthorityPeer {
        fn new() -> Self {
            let steps = Arc::new(AtomicU64::new(0));
            let mut host = SimulationHost::new(
                SimulationHostConfig::new(UniverseSeed([0x42; 32])),
                Walker {
                    steps: Arc::clone(&steps),
                },
                NoEventRouting,
            );
            host.install_state(TARGET, WalkerState { x_mm: 0 });
            Self {
                host,
                history: PoseHistory::new(WINDOW),
                steps,
            }
        }

        fn rules(&self) -> Walker {
            Walker {
                steps: Arc::clone(&self.steps),
            }
        }

        fn state(&self) -> WalkerState {
            WalkerState::decode(&self.host.state_bytes(TARGET).expect("target installed"))
                .expect("walker state decodes")
        }

        /// Run `ticks` ticks, recording the pose after each.
        fn run(&mut self, ticks: u64) {
            for _ in 0..ticks {
                let report = self.host.step(TickCount::new(1));
                let state = self.state();
                self.history.record(
                    TARGET,
                    report.first_tick,
                    PoseSample {
                        position: LatticePoint::new(state.x_mm, 0, 0),
                        hit_radius: RADIUS_MM,
                    },
                );
            }
        }

        fn now(&self) -> Tick {
            self.history.current_tick().expect("ran at least one tick")
        }
    }

    /// A ray fired straight down +y at the x the walker had at `tick`.
    fn ray_through_pose_at(peer: &AuthorityPeer, tick: Tick) -> QuantizedRay {
        let pose = peer
            .history
            .pose_at(TARGET, tick)
            .expect("tick is retained");
        QuantizedRay {
            origin: LatticePoint::new(pose.position.x, -10_000, 0),
            direction: QuantizedDir::new(0, 1, 0),
        }
    }

    fn claim(fire_tick: Tick, basis: InterpBasis, ray: QuantizedRay) -> HitClaim {
        HitClaim {
            shooter: SHOOTER,
            target: TARGET,
            weapon: RIFLE,
            fire_tick,
            basis,
            ray,
            claimed: HitSurface(0),
            input_seq: 1,
        }
    }

    /// The property D47 (a)(1) states and docs/05 §7 relies on: validating a
    /// claim against a pose inside the window is a **lookup**. The rules are
    /// not stepped, the entity is not rewound, and the pose the verdict names
    /// is the *retained* one, not the present one.
    #[test]
    fn a_claim_inside_the_window_validates_by_lookup_and_never_resimulates() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);

        let now = peer.now();
        let basis_tick = Tick::new(now.0 - 8);
        let retained = peer
            .history
            .pose_at(TARGET, basis_tick)
            .expect("8 ticks back is inside a 32-tick ring");
        let present = peer.state();
        assert_ne!(
            retained.position.x, present.x_mm,
            "the walker moved on since the basis tick, or the test proves nothing"
        );

        let steps_before = peer.steps.load(Ordering::SeqCst);
        let bytes_before = peer.host.state_bytes(TARGET);

        let claim = claim(
            now,
            InterpBasis::exact(basis_tick),
            ray_through_pose_at(&peer, basis_tick),
        );
        let verdict = peer.history.validate(&claim, &peer.rules());

        // No resimulation: the rules were not asked to step, and the entity's
        // canonical state — the thing a rewind would have to touch — is byte
        // for byte what it was.
        assert_eq!(
            peer.steps.load(Ordering::SeqCst),
            steps_before,
            "validation must not step the ruleset"
        );
        assert_eq!(
            peer.host.state_bytes(TARGET),
            bytes_before,
            "validation must not touch the entity's state"
        );

        assert_eq!(
            verdict.outcome,
            HitOutcome::Accepted {
                applied_at: Tick::new(now.0 + 1),
                pose: retained.position,
            },
            "the verdict names the retained pose and the next tick"
        );
        assert_eq!(verdict.claim, claim.key());
        assert_eq!(verdict.target, TARGET);
    }

    /// A blended basis re-derives the pose the shooter drew, not either
    /// endpoint, and the ray that hits it is the ray through the blend.
    #[test]
    fn a_blended_basis_is_re_derived_from_both_retained_ticks() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let now = peer.now();
        let from = Tick::new(now.0 - 6);
        let to = Tick::new(now.0 - 3);
        let from_x = peer.history.pose_at(TARGET, from).unwrap().position.x;
        let to_x = peer.history.pose_at(TARGET, to).unwrap().position.x;
        assert_eq!(to_x - from_x, 3 * STRIDE_MM);

        let basis = InterpBasis {
            from,
            to,
            alpha: UNorm16::from_f64(0.5),
        };
        let expected_x = from_x + (3 * STRIDE_MM) / 2;
        // Fire past the endpoint poses but through the blend: the sphere
        // (500 mm) is wider than the 150 mm offset, so pin it with a miss
        // check against a ray placed just outside the radius from the blend.
        let hit = claim(
            now,
            basis,
            QuantizedRay {
                origin: LatticePoint::new(expected_x, -10_000, 0),
                direction: QuantizedDir::new(0, 1, 0),
            },
        );
        match peer.history.validate(&hit, &peer.rules()).outcome {
            HitOutcome::Accepted { pose, .. } => assert_eq!(pose.x, expected_x),
            other => panic!("expected acceptance at the blend, got {other:?}"),
        }

        let miss = claim(
            now,
            basis,
            QuantizedRay {
                origin: LatticePoint::new(expected_x + i64::from(RADIUS_MM) + 1, -10_000, 0),
                direction: QuantizedDir::new(0, 1, 0),
            },
        );
        assert_eq!(
            peer.history.validate(&miss, &peer.rules()).outcome,
            HitOutcome::Rejected(HitRefusal::Miss {
                miss_distance: RADIUS_MM + 1,
                allowed: RADIUS_MM,
            })
        );
    }

    /// The refusal docs/05 §7 puts on the victim's authority by name: a
    /// basis further behind the fire tick than the cap is refused as
    /// `OutsideRewindWindow`, carrying both the rewind and the cap — not
    /// dropped, and not confused with a ring miss (the tick *is* retained).
    #[test]
    fn a_claim_outside_the_rewind_cap_is_refused_by_name() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let now = peer.now();
        let cap = u64::from(WINDOW.rewind_ticks);

        // One past the cap: retained (32 > 13), but too far behind the fire
        // tick.
        let too_far = Tick::new(now.0 - cap - 1);
        assert!(
            peer.history.pose_at(TARGET, too_far).is_some(),
            "the basis is inside the ring, so this refusal is the cap's and nobody else's"
        );
        let claim_too_far = claim(
            now,
            InterpBasis::exact(too_far),
            ray_through_pose_at(&peer, too_far),
        );
        assert_eq!(
            peer.history.validate(&claim_too_far, &peer.rules()).outcome,
            HitOutcome::Rejected(HitRefusal::OutsideRewindWindow {
                rewind_ticks: cap + 1,
                cap_ticks: WINDOW.rewind_ticks,
            })
        );

        // Exactly the cap is inside it.
        let at_cap = Tick::new(now.0 - cap);
        let claim_at_cap = claim(
            now,
            InterpBasis::exact(at_cap),
            ray_through_pose_at(&peer, at_cap),
        );
        assert!(
            matches!(
                peer.history.validate(&claim_at_cap, &peer.rules()).outcome,
                HitOutcome::Accepted { .. }
            ),
            "a rewind of exactly the cap is legal"
        );
    }

    /// A basis the ring no longer holds is a different refusal from the cap,
    /// and it names what the ring does hold.
    #[test]
    fn a_basis_the_ring_evicted_is_refused_with_the_retained_bounds() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let now = peer.now();
        let depth = u64::from(WINDOW.history_ticks);
        let evicted = Tick::new(now.0 - depth);
        assert!(peer.history.pose_at(TARGET, evicted).is_none());

        // Declare a fire tick that keeps the rewind inside the cap, so the
        // only thing standing between this claim and acceptance is the ring.
        let declared_fire = Tick::new(evicted.0 + 5);
        let claim = claim(
            declared_fire,
            InterpBasis::exact(evicted),
            QuantizedRay {
                origin: LatticePoint::new(0, -10_000, 0),
                direction: QuantizedDir::new(0, 1, 0),
            },
        );
        assert_eq!(
            peer.history.validate(&claim, &peer.rules()).outcome,
            HitOutcome::Rejected(HitRefusal::BasisNotRetained {
                tick: evicted,
                oldest_retained: Some(Tick::new(now.0 - depth + 1)),
                newest_retained: Some(now),
            })
        );
    }

    #[test]
    fn a_claim_against_an_entity_this_authority_does_not_hold_is_refused() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let now = peer.now();
        let stranger = PersistId::new(1234);
        let claim = HitClaim {
            target: stranger,
            ..claim(
                now,
                InterpBasis::exact(now),
                ray_through_pose_at(&peer, now),
            )
        };
        assert_eq!(
            peer.history.validate(&claim, &peer.rules()).outcome,
            HitOutcome::Rejected(HitRefusal::NotMyEntity { target: stranger })
        );

        // And one this authority *stopped* holding.
        peer.history.forget(TARGET);
        let claim = HitClaim {
            target: TARGET,
            ..claim
        };
        assert_eq!(
            peer.history.validate(&claim, &peer.rules()).outcome,
            HitOutcome::Rejected(HitRefusal::NotMyEntity { target: TARGET })
        );
    }

    #[test]
    fn malformed_claims_are_refused_before_any_lookup() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let now = peer.now();
        let ray = ray_through_pose_at(&peer, now);

        let backwards = InterpBasis {
            from: now,
            to: Tick::new(now.0 - 1),
            alpha: UNorm16::ZERO,
        };
        assert_eq!(
            peer.history
                .validate(&claim(now, backwards, ray), &peer.rules())
                .outcome,
            HitOutcome::Rejected(HitRefusal::MalformedBasis {
                basis: backwards,
                fire_tick: now
            })
        );

        let after_fire = InterpBasis::exact(now);
        let fired_earlier = Tick::new(now.0 - 1);
        assert_eq!(
            peer.history
                .validate(&claim(fired_earlier, after_fire, ray), &peer.rules())
                .outcome,
            HitOutcome::Rejected(HitRefusal::MalformedBasis {
                basis: after_fire,
                fire_tick: fired_earlier
            })
        );

        let no_direction = QuantizedRay {
            direction: QuantizedDir::new(0, 0, 0),
            ..ray
        };
        assert_eq!(
            peer.history
                .validate(
                    &claim(now, InterpBasis::exact(now), no_direction),
                    &peer.rules()
                )
                .outcome,
            HitOutcome::Rejected(HitRefusal::MalformedRay)
        );
    }

    #[test]
    fn weapon_and_reach_are_the_rulesets_facts_not_the_claimants() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let now = peer.now();
        let ray = ray_through_pose_at(&peer, now);

        let unknown = HitClaim {
            weapon: WeaponRef(77),
            ..claim(now, InterpBasis::exact(now), ray)
        };
        assert_eq!(
            peer.history.validate(&unknown, &peer.rules()).outcome,
            HitOutcome::Rejected(HitRefusal::UnknownWeapon {
                weapon: WeaponRef(77)
            })
        );

        // Behind the origin: the ray points away from the pose.
        let away = QuantizedRay {
            direction: QuantizedDir::new(0, -1, 0),
            ..ray
        };
        assert_eq!(
            peer.history
                .validate(&claim(now, InterpBasis::exact(now), away), &peer.rules())
                .outcome,
            HitOutcome::Rejected(HitRefusal::OutOfReach {
                along_ray: -10_000,
                reach: RIFLE_REACH_MM
            })
        );

        // Beyond reach: same ray, origin pulled back past the rifle's reach.
        let far = QuantizedRay {
            origin: LatticePoint::new(ray.origin.x, -i64::from(RIFLE_REACH_MM) - 1, 0),
            ..ray
        };
        assert_eq!(
            peer.history
                .validate(&claim(now, InterpBasis::exact(now), far), &peer.rules())
                .outcome,
            HitOutcome::Rejected(HitRefusal::OutOfReach {
                along_ray: i64::from(RIFLE_REACH_MM) + 1,
                reach: RIFLE_REACH_MM
            })
        );
    }

    /// The closed window is the default and it refuses everything: a ring of
    /// depth zero retains nothing, so the first lookup fails.
    #[test]
    fn the_default_history_is_closed_and_retains_nothing() {
        let mut history = PoseHistory::default();
        assert_eq!(history.window(), HitWindow::CLOSED);
        history.record(
            TARGET,
            Tick::new(5),
            PoseSample {
                position: LatticePoint::default(),
                hit_radius: 1,
            },
        );
        assert!(history.holds(TARGET));
        assert_eq!(history.retained(TARGET), None);
        assert_eq!(history.pose_at(TARGET, Tick::new(5)), None);
    }

    #[test]
    fn the_ring_evicts_by_depth_and_stamps_its_slots() {
        let mut ring = PoseRing::new(4);
        let sample = |x: i64| PoseSample {
            position: LatticePoint::new(x, 0, 0),
            hit_radius: 1,
        };
        for tick in 0..6u64 {
            ring.record(Tick::new(tick), sample(tick as i64));
        }
        assert_eq!(ring.retained(), Some((Tick::new(2), Tick::new(5))));
        assert_eq!(ring.get(Tick::new(1)), None, "evicted by tick 5");
        assert_eq!(ring.get(Tick::new(5)), Some(sample(5)));
        assert_eq!(
            ring.get(Tick::new(9)),
            None,
            "slot 1 holds tick 5, and the stamp says so"
        );
    }

    fn source(seed: u8) -> NodeId {
        iroh_base::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn claim_seq(peer: &AuthorityPeer, seq: u16) -> HitClaim {
        let now = peer.now();
        let basis = Tick::new(now.0 - 6);
        HitClaim {
            input_seq: seq,
            ..claim(
                now,
                InterpBasis::exact(basis),
                ray_through_pose_at(peer, basis),
            )
        }
    }

    fn refusal(answer: ClaimAnswer) -> Option<HitRefusal> {
        match answer {
            ClaimAnswer::Verdict(HitVerdict {
                outcome: HitOutcome::Rejected(refusal),
                ..
            }) => Some(refusal),
            _ => None,
        }
    }

    /// #923: the hit lane is unsheddable, so it is capped at admission, and
    /// an over-cap claim is refused **by name** in a verdict that echoes the
    /// claim's key — the shooter learns which claim to stop resending. A
    /// second over-cap copy in the same tick is coalesced, never silently
    /// dropped: the return value says so.
    #[test]
    fn an_over_cap_claim_is_refused_by_name_and_the_refusal_names_the_claim() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let rules = peer.rules();
        let shooter_peer = source(1);
        let cap = peer.history.claim_cap();
        assert_eq!(cap, HitClaimCap::for_window(WINDOW));
        assert_eq!(cap.burst, 32, "the burst is the ring's depth");

        for seq in 0..cap.burst {
            let claim = claim_seq(&peer, seq);
            let answer = peer.history.answer(shooter_peer, &claim, &rules);
            assert!(
                matches!(
                    answer,
                    ClaimAnswer::Verdict(HitVerdict {
                        outcome: HitOutcome::Accepted { .. },
                        ..
                    })
                ),
                "claim {seq} inside the burst validates normally: {answer:?}"
            );
        }

        let over = claim_seq(&peer, cap.burst);
        let answer = peer.history.answer(shooter_peer, &over, &rules);
        let ClaimAnswer::Verdict(verdict) = answer else {
            panic!("the over-cap claim must be *answered*, got {answer:?}");
        };
        assert_eq!(verdict.claim, over.key(), "the refusal names the claim");
        assert_eq!(
            verdict.outcome,
            HitOutcome::Rejected(HitRefusal::OverClaimRate {
                burst: 32,
                refill_ticks: 1,
            })
        );

        let copy = claim_seq(&peer, cap.burst + 1);
        assert_eq!(
            peer.history.answer(shooter_peer, &copy, &rules),
            ClaimAnswer::AlreadyAnswered {
                key: copy.key(),
                at: peer.now(),
            },
            "a same-tick flood is told once, and the coalescing is named"
        );
        let gate = peer.history.claim_gate(shooter_peer).expect("gate opened");
        assert_eq!((gate.refused(), gate.coalesced()), (1, 1));

        // One tick on, one token has refilled: the next new claim goes through.
        peer.run(1);
        let fresh = claim_seq(&peer, cap.burst + 2);
        assert_eq!(
            refusal(peer.history.answer(shooter_peer, &fresh, &rules)),
            None
        );
    }

    /// The leg that matters as much as the refusal: a cap that refuses
    /// legitimate play is worse than no cap. Regolith's fastest weapon has a
    /// 20-tick cooldown (`crates/orrery_games/src/regolith/weapon.rs`), and
    /// docs/05 §7 has the shooter resend until a verdict names the key. So:
    /// a shot every 20 ticks for a minute, each shot a spread that hits
    /// *eight* targets, each claim resent twice as if the verdict were lost —
    /// and, separately, the fire-rate invariant's own floor of one shot per
    /// tick, single-target, with the same resends. Neither ever meets
    /// `OverClaimRate`.
    ///
    /// The floor leg is single-target on purpose. The cap is in claims and
    /// the invariant is in shots; they coincide at the floor only for one
    /// claim per shot, and a weapon that hits two targets every tick for half
    /// a second is not a designed rate any validator in the tree admits. At
    /// any shipped cooldown the spread has `cooldown` tokens of headroom per
    /// shot, which the eight-target leg demonstrates.
    #[test]
    fn an_honest_weapon_at_its_designed_rate_is_never_refused() {
        const REGOLITH_FASTEST_COOLDOWN_TICKS: u64 = 20;
        const RESEND_AFTER_TICKS: [u64; 2] = [6, 12];

        for (cooldown, targets_per_shot) in [(REGOLITH_FASTEST_COOLDOWN_TICKS, 8), (1, 1)] {
            let mut peer = AuthorityPeer::new();
            peer.run(40);
            let rules = peer.rules();
            let shooter_peer = source(2);
            let mut pending: Vec<(Tick, HitClaim)> = Vec::new();
            let mut seq: u16 = 0;
            let mut accepted = 0u32;

            for _ in 0..3_600u64 {
                peer.run(1);
                let now = peer.now();
                if now.0.is_multiple_of(cooldown) {
                    // One shot; every target it hits is a claim in this tick.
                    for _ in 0..targets_per_shot {
                        let claim = claim_seq(&peer, seq);
                        seq = seq.wrapping_add(1);
                        let answer = peer.history.answer(shooter_peer, &claim, &rules);
                        assert_eq!(refusal(answer), None, "cooldown {cooldown}: {answer:?}");
                        if matches!(
                            answer,
                            ClaimAnswer::Verdict(HitVerdict {
                                outcome: HitOutcome::Accepted { .. },
                                ..
                            })
                        ) {
                            accepted += 1;
                        }
                        for after in RESEND_AFTER_TICKS {
                            pending.push((Tick::new(now.0 + after), claim));
                        }
                    }
                }
                for (_, claim) in pending.iter().filter(|(due, _)| *due == now) {
                    let answer = peer.history.answer(shooter_peer, claim, &rules);
                    assert_eq!(
                        refusal(answer),
                        None,
                        "cooldown {cooldown}: a resend is never over cap: {answer:?}"
                    );
                }
                pending.retain(|(due, _)| *due > now);
            }
            assert!(
                accepted > 0,
                "the honest claims were real: some validated against the ring"
            );
            let gate = peer.history.claim_gate(shooter_peer).expect("gate opened");
            assert_eq!(gate.refused(), 0, "cooldown {cooldown}");
            assert_eq!(gate.coalesced(), 0, "cooldown {cooldown}");
        }
    }

    /// The gate is keyed by the peer the transport vouches for. A source
    /// cannot buy a second bucket by naming a second shooter, and one source
    /// running dry does not touch another's bucket.
    #[test]
    fn the_cap_is_per_source_peer_not_per_named_shooter() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let rules = peer.rules();
        let flooder = source(3);
        let honest = source(4);

        for seq in 0..32u16 {
            let claim = HitClaim {
                // A fresh shooter id per claim buys nothing.
                shooter: PersistId::new(1_000 + u64::from(seq)),
                ..claim_seq(&peer, seq)
            };
            assert_eq!(refusal(peer.history.answer(flooder, &claim, &rules)), None);
        }
        let over = HitClaim {
            shooter: PersistId::new(2_000),
            ..claim_seq(&peer, 99)
        };
        assert!(matches!(
            refusal(peer.history.answer(flooder, &over, &rules)),
            Some(HitRefusal::OverClaimRate { .. })
        ));

        let theirs = claim_seq(&peer, 0);
        assert_eq!(
            refusal(peer.history.answer(honest, &theirs, &rules)),
            None,
            "another peer's bucket is its own"
        );
    }

    /// The gate table is bounded at D6's per-cell ceiling and evicts the
    /// stalest source, which can only make that source *more* admissible.
    #[test]
    fn the_gate_table_is_bounded_and_evicts_the_stalest_source() {
        let mut peer = AuthorityPeer::new();
        peer.run(40);
        let rules = peer.rules();

        let first = source(0);
        let claim = claim_seq(&peer, 0);
        peer.history.answer(first, &claim, &rules);
        peer.run(1);
        for seed in 1..=MAX_CLAIM_SOURCES {
            let claim = claim_seq(&peer, 0);
            peer.history.answer(
                iroh_base::SecretKey::from_bytes(&[
                    seed as u8,
                    (seed >> 8) as u8,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    1,
                ])
                .public(),
                &claim,
                &rules,
            );
        }
        assert!(
            peer.history.claim_gate(first).is_none(),
            "the stalest source was evicted"
        );
        assert!(peer.history.gates.len() <= MAX_CLAIM_SOURCES);
    }
}
