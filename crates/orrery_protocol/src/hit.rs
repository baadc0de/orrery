//! Hit registration wire types (D8, docs/05-prediction-rollback.md §7).
//!
//! A hit is the one interaction where the shooter's *view* and the target's
//! *state* live on different peers and at different times: the shooter aims at
//! an interpolated pose, 100 ms in the past plus transit, and the target's
//! authority is the only party that can say whether that pose was real. Two
//! messages carry the whole exchange — [`HitClaim`] from the shooter, and
//! [`HitVerdict`] back — and everything here is designed around one rule:
//!
//! **The target's authority verifies the claim from its own history and trusts
//! nothing the shooter says about the target.** A claim therefore carries the
//! shooter's *observations* (which snapshot ticks it blended, and the ray it
//! cast) and never the pose it thinks it hit; the authority re-derives that
//! pose from its own retained ring and intersects the ray itself. A claim the
//! victim's authority cannot check independently is worth exactly the sparks
//! the shooter painted locally (docs/05 §7: "a cheating shooter can at most
//! paint local sparks; it cannot mint value").
//!
//! The validation lives in `orrery_authority`; this module is the shape of the
//! wire and the vocabulary of the verdict.

use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::{PersistId, Tick};

/// A blend factor in `[0, 1]`, carried as `u16` so the wire has no float in it.
///
/// `0` is `0.0`, `u16::MAX` is `1.0`. The quantization step is ~1.5e-5, far
/// below what a 1 mm position lattice can resolve over a 16 ms send interval,
/// so nothing is lost by refusing to put an `f32` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UNorm16(pub u16);

impl UNorm16 {
    /// Exactly `0.0`.
    pub const ZERO: Self = Self(0);
    /// Exactly `1.0`.
    pub const ONE: Self = Self(u16::MAX);

    /// The blend factor as a float in `[0, 1]`.
    #[must_use]
    pub fn to_f64(self) -> f64 {
        f64::from(self.0) / f64::from(u16::MAX)
    }

    /// A blend factor from a float, clamped into `[0, 1]`.
    #[must_use]
    pub fn from_f64(value: f64) -> Self {
        let clamped = if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, 1.0)
        };
        // Exact at both ends; rounds to nearest in between.
        Self((clamped * f64::from(u16::MAX) + 0.5) as u16)
    }
}

/// The interpolation basis a shooter rendered a target from (docs/05 §1, §7).
///
/// An interpolated entity is drawn between two received snapshots, at
/// `lerp(pose(from), pose(to), alpha)`. The claim carries exactly those three
/// numbers, and nothing derived from them: the authority owns the poses at
/// `from` and `to`, so from this basis it can rebuild the very pose the shooter
/// saw without taking the shooter's word for where the target was.
///
/// Invariants the authority enforces (a claim breaking them is refused as
/// [`HitRefusal::MalformedBasis`]): `from <= to`, and `to <= fire_tick` — the
/// shooter cannot have rendered a snapshot it had not received when it fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InterpBasis {
    /// The older snapshot tick of the pair.
    pub from: Tick,
    /// The newer snapshot tick of the pair. Equal to `from` when the shooter
    /// rendered a snapshot exactly rather than a blend.
    pub to: Tick,
    /// Blend factor from `from` towards `to`.
    pub alpha: UNorm16,
}

impl InterpBasis {
    /// A basis that names one snapshot exactly.
    #[must_use]
    pub const fn exact(tick: Tick) -> Self {
        Self {
            from: tick,
            to: tick,
            alpha: UNorm16::ZERO,
        }
    }

    /// Whether `from <= to`, the one ordering the type itself can state.
    #[must_use]
    pub fn is_ordered(&self) -> bool {
        self.from <= self.to
    }
}

/// A point on the position lattice: millimetres, grid-relative, as `i64`.
///
/// The same lattice `orrery_core::QPos` quantizes core state to (docs/05 §9:
/// quantize both sides). Defined here rather than imported because the wire
/// crate sits below the core, and a ray needs an origin in the units the
/// authority's ring already stores.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct LatticePoint {
    /// Millimetres along x.
    pub x: i64,
    /// Millimetres along y.
    pub y: i64,
    /// Millimetres along z.
    pub z: i64,
}

impl LatticePoint {
    /// A point from its three lattice coordinates.
    #[must_use]
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }
}

/// A direction quantized per axis to `i16`, not necessarily unit length.
///
/// The validator normalizes; the wire carries whatever the shooter's
/// projection produced. Zero on every axis is a malformed ray and is refused
/// rather than normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QuantizedDir {
    /// Direction component along x.
    pub x: i16,
    /// Direction component along y.
    pub y: i16,
    /// Direction component along z.
    pub z: i16,
}

impl QuantizedDir {
    /// A direction from its three components.
    #[must_use]
    pub const fn new(x: i16, y: i16, z: i16) -> Self {
        Self { x, y, z }
    }

    /// Whether every component is zero — the one direction that means nothing.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.x == 0 && self.y == 0 && self.z == 0
    }
}

/// The ray a shooter cast, in the authority's units.
///
/// Origin and direction only. The reach is deliberately *not* a field: the
/// weapon's reach is the ruleset's fact about [`WeaponRef`], and a claimant
/// that could name its own reach could name any reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QuantizedRay {
    /// Where the shot started, on the position lattice.
    pub origin: LatticePoint,
    /// Which way it went.
    pub direction: QuantizedDir,
}

/// A ruleset-defined weapon identifier.
///
/// Opaque here. The target's authority resolves it through its ruleset to a
/// reach (and, later, rate and damage); an identifier the ruleset does not know
/// is refused as [`HitRefusal::UnknownWeapon`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WeaponRef(pub u32);

/// A ruleset-defined surface the shooter *claims* to have hit: a body part, a
/// voxel face.
///
/// Presentation only. The authority never validates it and never derives
/// anything from it — it is echoed so the shooter's feedback can name the
/// surface it predicted. A game that grades damage by surface derives the
/// surface on the authority from the re-derived pose, not from this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HitSurface(pub u16);

/// What a shooter sends the target's authority (docs/05 §7).
///
/// # What the validator can check without trusting the claimant
///
/// Every field is either an identity the transport already vouches for, an
/// observation the authority can reproduce from its own history, or a
/// presentation-only echo:
///
/// | Field | Trusted? | How the authority checks it |
/// |---|---|---|
/// | `target` | no | must be an entity this authority holds a ring for |
/// | `fire_tick`, `basis` | no | `fire_tick − basis.from` against the rewind cap; both basis ticks against the ring |
/// | `ray` | partly | the re-derived pose must lie on it within the ruleset's tolerance and reach |
/// | `weapon` | no | must resolve through the ruleset |
/// | `shooter`, `input_seq` | ack key | echoed on the verdict; dedupes resends |
/// | `claimed` | ignored | echoed for presentation |
///
/// Two things are left partly on trust here, and both are named so nobody
/// mistakes the validator for more than it is. The ray's *origin*: the
/// target's authority holds only an interpolated copy of the shooter, so it
/// cannot pin the origin to the shooter's authoritative pose. And
/// `fire_tick` itself: the authority cannot see when the shooter's input was
/// really sampled, so a shooter may declare a fire tick up to the ring's
/// depth behind the present and pass the cap check on paper. What the
/// victim's authority *does* bound unconditionally is `now − basis.from`,
/// through the ring: a basis older than `pose_history_ticks` is not retained
/// and is refused whatever `fire_tick` says. Range, rate and the honesty of
/// `fire_tick` against the shooter's own signed input log are the invariants
/// docs/05 §7 assigns to the witness validators (D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HitClaim {
    /// The entity that fired.
    pub shooter: PersistId,
    /// The entity claimed hit; its authority is the validator.
    pub target: PersistId,
    /// The weapon fired, resolved by the target's ruleset.
    pub weapon: WeaponRef,
    /// The shooter's tick when the fire input was sampled.
    pub fire_tick: Tick,
    /// The interpolation basis the shooter rendered the target from.
    pub basis: InterpBasis,
    /// The ray the shooter cast.
    pub ray: QuantizedRay,
    /// The surface the shooter's presentation predicted. Echoed, never checked.
    pub claimed: HitSurface,
    /// Shooter-local sequence, the resend/ack key together with `shooter`.
    pub input_seq: u16,
}

impl HitClaim {
    /// The key a verdict echoes so the shooter can match it to this claim.
    #[must_use]
    pub const fn key(&self) -> HitClaimKey {
        HitClaimKey {
            shooter: self.shooter,
            input_seq: self.input_seq,
        }
    }

    /// The rewind this claim asks the authority to look back by, in ticks:
    /// `fire_tick − basis.from`, or `None` when the basis is *after* the fire
    /// tick and the question has no answer.
    ///
    /// `from` rather than `to` or the blend point, deliberately: the cap bounds
    /// how far into the past a shooter may retro-date, and the older snapshot
    /// is the furthest the claim reaches.
    #[must_use]
    pub fn rewind_ticks(&self) -> Option<u64> {
        self.fire_tick.0.checked_sub(self.basis.from.0)
    }
}

/// The `(shooter, input_seq)` pair a verdict echoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HitClaimKey {
    /// The entity that fired.
    pub shooter: PersistId,
    /// The shooter-local sequence of the claim.
    pub input_seq: u16,
}

/// Why the target's authority refused a [`HitClaim`].
///
/// Every refusal names what was checked and what the bound was, because a
/// silently dropped claim is indistinguishable from a lost datagram, and the
/// shooter would resend it forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HitRefusal {
    /// The claimed target is not an entity this authority holds pose history
    /// for — not its entity, or one it stopped holding.
    NotMyEntity {
        /// The entity the claim named.
        target: PersistId,
    },
    /// The basis breaks its own ordering: `from > to`, or `to > fire_tick`.
    MalformedBasis {
        /// The basis as claimed.
        basis: InterpBasis,
        /// The fire tick as claimed.
        fire_tick: Tick,
    },
    /// The ray has no direction.
    MalformedRay,
    /// `fire_tick − basis.from` exceeds the hit-rewind cap (D8: 200 ms, 12
    /// ticks at 60 Hz). The one refusal docs/05 §7 puts on the *victim's*
    /// authority by name: no peer can retro-date further than the cap.
    OutsideRewindWindow {
        /// How far back the claim reached.
        rewind_ticks: u64,
        /// The cap it exceeded.
        cap_ticks: u16,
    },
    /// A basis tick is inside the cap but no longer (or not yet) in the
    /// authority's pose ring. Inside the cap this should not happen on a ring
    /// sized per docs/05 §7 — which is why the retained bounds are named, so a
    /// ring that is too short shows up as a number rather than a feeling.
    BasisNotRetained {
        /// The basis tick that was not found.
        tick: Tick,
        /// The oldest tick the ring still holds, if it holds any.
        oldest_retained: Option<Tick>,
        /// The newest tick the ring holds, if it holds any.
        newest_retained: Option<Tick>,
    },
    /// The ruleset does not know this weapon.
    UnknownWeapon {
        /// The weapon as claimed.
        weapon: WeaponRef,
    },
    /// The re-derived pose is further along the ray than the weapon reaches,
    /// or behind its origin.
    OutOfReach {
        /// Distance along the ray to the closest approach, in lattice units;
        /// negative means behind the origin.
        along_ray: i64,
        /// The weapon's reach, in lattice units.
        reach: u32,
    },
    /// The ray passes the re-derived pose by more than its hit radius plus the
    /// ruleset's tolerance.
    Miss {
        /// Closest approach of the ray to the pose, in lattice units.
        miss_distance: u32,
        /// The radius (plus tolerance) that would have counted, in lattice units.
        allowed: u32,
    },
    /// The source has put more *new* claims on the wire than its admission
    /// cap allows ([`HitClaimCap`], #923). Named so the shooter stops
    /// resending this key: a resend cannot earn a token, and the claim will
    /// be refused the same way until the bucket refills. Not a verdict on
    /// the shot — the pose was never looked up.
    OverClaimRate {
        /// New claims the bucket holds when full.
        burst: u16,
        /// Ticks per token refilled.
        refill_ticks: u16,
    },
}

/// The authority's answer to a [`HitClaim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HitOutcome {
    /// The claim validated against retained pose history; the effect is applied
    /// at the authority's current tick, never back-dated (D47 (a)(1), D9).
    Accepted {
        /// The tick the effect lands on: the authority's *next* tick, per the
        /// next-tick event rule (D46 (a)(1)).
        applied_at: Tick,
        /// The pose the authority re-derived and tested — what the shooter
        /// reconciles its presentation against.
        pose: LatticePoint,
    },
    /// Refused, by name.
    Rejected(HitRefusal),
}

/// What the target's authority sends back to the shooter (docs/05 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HitVerdict {
    /// The claim this answers.
    pub claim: HitClaimKey,
    /// The entity the claim named.
    pub target: PersistId,
    /// The surface the shooter claimed, echoed unexamined.
    pub claimed: HitSurface,
    /// Accepted with the applied tick, or refused with a reason.
    pub outcome: HitOutcome,
}

/// The two hit-registration messages, as one datagram family.
///
/// Claims are resent until a verdict names their key (docs/05 §7), so both
/// directions ride the unreliable state channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HitMsg {
    /// Shooter to target's authority.
    Claim(HitClaim),
    /// Target's authority to shooter.
    Verdict(HitVerdict),
}

/// The two numbers a hit validator is configured by, in ticks.
///
/// Both are derived from `orrery_predict`'s `PredictConfig` — the rewind cap
/// (D8, `hit_rewind_ticks`) and the retained ring depth docs/05 §7 sizes from
/// it (`pose_history_ticks`). The ring lives in `orrery_authority`, the numbers
/// in `orrery_predict`, and neither crate may depend on the other, so the pair
/// is spelled once here where both can name it.
///
/// [`HitWindow::CLOSED`] is the default: a zero-depth ring that refuses every
/// claim. Fail-closed rather than a written-down copy of the D16 figures, so
/// that the only place `32` exists is the derivation that produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct HitWindow {
    /// The furthest back a claim may reach, in ticks.
    pub rewind_ticks: u16,
    /// How many ticks of pose history the authority retains.
    pub history_ticks: u16,
}

impl HitWindow {
    /// Refuses everything: no history, no rewind.
    pub const CLOSED: Self = Self {
        rewind_ticks: 0,
        history_ticks: 0,
    };

    /// A window from its two figures.
    #[must_use]
    pub const fn new(rewind_ticks: u16, history_ticks: u16) -> Self {
        Self {
            rewind_ticks,
            history_ticks,
        }
    }
}

/// The admission cap on hit claims one source may put on the wire (#923).
///
/// Hit traffic is unsheddable (`orrery_net::budget::is_sheddable`): the
/// backstop cannot drop a claim without losing a shot the player has already
/// taken. The rule that makes an unsheddable lane coherent is the one the
/// witness lane already follows — *bounded at the source*, so the lane cannot
/// be the thing that exhausts the budget. This is the hit lane's bound, and
/// the same figures are enforced twice: by the shooter's peer over the claims
/// it emits, and by the target's authority over the claims it answers, keyed
/// by the peer the transport vouches for. A source that skips its own gate
/// meets the same numbers on the other side, and gets the refusal by name.
///
/// # Derivation
///
/// Both figures come from things already in the tree, so that a change to
/// either is a change to a stated reason and not to a number.
///
/// - **Refill: one token per tick.** docs/05 §7 assigns the fire-rate
///   invariant to the witness validators (D10), and every shipped validator
///   checks `fired ≤ elapsed / cooldown + 1` with `cooldown.max(1)` — one
///   shot per tick is the invariant's own floor, because a fire input is
///   sampled once per tick. The platform cannot know a game's real cooldown
///   (Regolith's fastest is 20 ticks), so it caps at the floor and leaves the
///   tighter per-weapon rate where docs/05 §7 puts it. At D16's 60 Hz that is
///   60 new claims per second per source. The cap counts *claims* and the
///   invariant counts *shots*; a shot that hits several targets is several
///   claims, so the two coincide at the floor only for single-target hits.
///   At any real cooldown the spread has `cooldown` tokens of headroom per
///   shot — Regolith's fastest weapon could hit twenty targets a shot,
///   sustained, before the cap noticed.
/// - **Burst: the pose ring's depth** (`HitWindow::history_ticks`, 32 on
///   D16's defaults). A claim's basis older than the ring is refused as
///   [`HitRefusal::BasisNotRetained`] whatever else is true of it, so a burst
///   deeper than the ring cannot all be valid. A shooter that emits 33 new
///   claims in one tick has, by construction, emitted at least one the ring
///   cannot answer.
///
/// # What is metered
///
/// A token is spent per *new* claim key — `(shooter, input_seq)`. A resend
/// of a key this gate has already admitted is free while the key is among the
/// last `burst` admissions, because docs/05 §7 has the shooter resend until a
/// verdict names the key, and charging the resend would turn verdict loss
/// into a refusal of a legitimate shot. Duplicate copies of one key inside one
/// tick are coalesced rather than answered twice: the answer to the first copy
/// has not had time to arrive, so a second answer carries nothing.
///
/// Stated worst case for a source that replays admitted keys deliberately:
/// `burst + 1` answers per tick. The gate bounds a malicious source, but not
/// as tightly as it bounds an honest one; the honest bound is the one the
/// budget arithmetic in `orrery_authority`'s source-cap check counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HitClaimCap {
    /// New claims the bucket holds when full.
    pub burst: u16,
    /// Ticks per token refilled. Zero is treated as one.
    pub refill_ticks: u16,
}

impl HitClaimCap {
    /// Admits nothing: the cap of a closed [`HitWindow`].
    pub const CLOSED: Self = Self {
        burst: 0,
        refill_ticks: 1,
    };

    /// The cap docs/05 §7's figures imply for `window`: burst = the ring's
    /// depth, refill = one per tick (the fire-rate invariant's floor).
    #[must_use]
    pub const fn for_window(window: HitWindow) -> Self {
        Self {
            burst: window.history_ticks,
            refill_ticks: 1,
        }
    }

    /// The sustained rate this cap admits, in new claims per second at
    /// `tick_hz`. This is the figure the sum-of-caps check charges the hit
    /// lane with; the burst is transient and refills from the same rate.
    #[must_use]
    pub const fn sustained_claims_per_second(&self, tick_hz: u32) -> u64 {
        let refill = if self.refill_ticks == 0 {
            1
        } else {
            self.refill_ticks as u64
        };
        tick_hz as u64 / refill
    }
}

impl Default for HitClaimCap {
    fn default() -> Self {
        Self::CLOSED
    }
}

/// How a [`HitClaimGate`] answered one claim datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Validate and answer it.
    Admit,
    /// Answer it with this refusal — by name, so the producer stops.
    Refuse(HitRefusal),
    /// This key, or this source's over-cap refusal, was already answered at
    /// `at` (the current tick). The earlier answer is in flight and a second
    /// one carries nothing; the copy is coalesced, not dropped, and the count
    /// is kept in [`HitClaimGate::coalesced`].
    AlreadyAnswered {
        /// The tick the standing answer was given at.
        at: Tick,
    },
}

/// The per-source admission gate for [`HitClaim`]s: a token bucket over new
/// claim keys, with resends free and same-tick duplicates coalesced. See
/// [`HitClaimCap`] for the figures and their derivation.
///
/// Engine-free and clock-free: the caller passes its own tick, so the same
/// type serves the shooter's peer (its simulation tick) and the target's
/// authority (its pose ring's newest tick).
#[derive(Debug, Clone)]
pub struct HitClaimGate {
    cap: HitClaimCap,
    tokens: u16,
    /// The tick the bucket was last brought up to date at.
    refilled_at: Option<Tick>,
    /// The last `burst` keys admitted, oldest first; a resend of one is free.
    admitted: VecDeque<HitClaimKey>,
    /// Keys answered at `answered_at`, so a same-tick copy is coalesced.
    answered_at: Option<Tick>,
    answered: BTreeSet<HitClaimKey>,
    /// The tick this source was last refused for rate, so a flood is told
    /// once per tick rather than once per datagram.
    refused_at: Option<Tick>,
    coalesced: u64,
    refused: u64,
}

impl HitClaimGate {
    /// A full bucket under `cap`.
    #[must_use]
    pub fn new(cap: HitClaimCap) -> Self {
        Self {
            cap,
            tokens: cap.burst,
            refilled_at: None,
            admitted: VecDeque::with_capacity(usize::from(cap.burst)),
            answered_at: None,
            answered: BTreeSet::new(),
            refused_at: None,
            coalesced: 0,
            refused: 0,
        }
    }

    /// The cap this gate enforces.
    #[must_use]
    pub const fn cap(&self) -> HitClaimCap {
        self.cap
    }

    /// New claims this gate would admit right now, before any refill.
    #[must_use]
    pub const fn tokens(&self) -> u16 {
        self.tokens
    }

    /// The tick this gate last saw a claim at.
    #[must_use]
    pub const fn last_seen(&self) -> Option<Tick> {
        self.refilled_at
    }

    /// Same-tick duplicate copies coalesced so far.
    #[must_use]
    pub const fn coalesced(&self) -> u64 {
        self.coalesced
    }

    /// Over-cap refusals issued so far (one per tick at most).
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// Decide one claim datagram carrying `key`, at the caller's tick `now`.
    pub fn admit(&mut self, key: HitClaimKey, now: Tick) -> Admission {
        self.refill(now);
        if self.answered_at != Some(now) {
            self.answered_at = Some(now);
            self.answered.clear();
        }

        if self.answered.contains(&key) {
            self.coalesced = self.coalesced.saturating_add(1);
            return Admission::AlreadyAnswered { at: now };
        }
        if self.admitted.contains(&key) {
            self.answered.insert(key);
            return Admission::Admit;
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            if self.admitted.len() >= usize::from(self.cap.burst) {
                self.admitted.pop_front();
            }
            if self.cap.burst > 0 {
                self.admitted.push_back(key);
            }
            self.answered.insert(key);
            return Admission::Admit;
        }
        if self.refused_at == Some(now) {
            self.coalesced = self.coalesced.saturating_add(1);
            return Admission::AlreadyAnswered { at: now };
        }
        self.refused_at = Some(now);
        self.refused = self.refused.saturating_add(1);
        Admission::Refuse(HitRefusal::OverClaimRate {
            burst: self.cap.burst,
            refill_ticks: self.cap.refill_ticks,
        })
    }

    /// Bring the bucket up to `now`. A tick behind the last one refills
    /// nothing and moves nothing: the bucket never runs backwards.
    fn refill(&mut self, now: Tick) {
        let Some(last) = self.refilled_at else {
            self.refilled_at = Some(now);
            return;
        };
        if now <= last {
            return;
        }
        let refill = u64::from(self.cap.refill_ticks.max(1));
        let elapsed = now.0 - last.0;
        let earned = elapsed / refill;
        self.tokens = u16::try_from(u64::from(self.tokens) + earned)
            .unwrap_or(u16::MAX)
            .min(self.cap.burst);
        // Keep the fractional progress towards the next token.
        self.refilled_at = Some(Tick::new(last.0 + earned * refill));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim() -> HitClaim {
        HitClaim {
            shooter: PersistId::new(7),
            target: PersistId::new(9),
            weapon: WeaponRef(3),
            fire_tick: Tick::new(1_000),
            basis: InterpBasis {
                from: Tick::new(990),
                to: Tick::new(993),
                alpha: UNorm16::from_f64(0.25),
            },
            ray: QuantizedRay {
                origin: LatticePoint::new(1, -2, 3),
                direction: QuantizedDir::new(1_000, 0, -7),
            },
            claimed: HitSurface(2),
            input_seq: 0xBEEF,
        }
    }

    #[test]
    fn hit_claim_roundtrips() {
        let claim = claim();
        let bytes = postcard::to_stdvec(&claim).unwrap();
        assert_eq!(postcard::from_bytes::<HitClaim>(&bytes).unwrap(), claim);

        let msg = HitMsg::Claim(claim);
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(postcard::from_bytes::<HitMsg>(&bytes).unwrap(), msg);
    }

    #[test]
    fn hit_verdict_roundtrips_every_outcome() {
        let key = claim().key();
        let outcomes = [
            HitOutcome::Accepted {
                applied_at: Tick::new(1_004),
                pose: LatticePoint::new(5, 6, 7),
            },
            HitOutcome::Rejected(HitRefusal::NotMyEntity {
                target: PersistId::new(9),
            }),
            HitOutcome::Rejected(HitRefusal::MalformedBasis {
                basis: InterpBasis::exact(Tick::new(1)),
                fire_tick: Tick::new(0),
            }),
            HitOutcome::Rejected(HitRefusal::MalformedRay),
            HitOutcome::Rejected(HitRefusal::OutsideRewindWindow {
                rewind_ticks: 13,
                cap_ticks: 12,
            }),
            HitOutcome::Rejected(HitRefusal::BasisNotRetained {
                tick: Tick::new(3),
                oldest_retained: Some(Tick::new(4)),
                newest_retained: None,
            }),
            HitOutcome::Rejected(HitRefusal::UnknownWeapon {
                weapon: WeaponRef(99),
            }),
            HitOutcome::Rejected(HitRefusal::OutOfReach {
                along_ray: -3,
                reach: 50_000,
            }),
            HitOutcome::Rejected(HitRefusal::Miss {
                miss_distance: 900,
                allowed: 500,
            }),
            HitOutcome::Rejected(HitRefusal::OverClaimRate {
                burst: 32,
                refill_ticks: 1,
            }),
        ];
        for outcome in outcomes {
            let verdict = HitVerdict {
                claim: key,
                target: PersistId::new(9),
                claimed: HitSurface(2),
                outcome,
            };
            let bytes = postcard::to_stdvec(&verdict).unwrap();
            assert_eq!(
                postcard::from_bytes::<HitVerdict>(&bytes).unwrap(),
                verdict,
                "{outcome:?}"
            );
            let msg = HitMsg::Verdict(verdict);
            let bytes = postcard::to_stdvec(&msg).unwrap();
            assert_eq!(postcard::from_bytes::<HitMsg>(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn unorm16_is_exact_at_both_ends_and_clamps() {
        assert_eq!(UNorm16::from_f64(0.0), UNorm16::ZERO);
        assert_eq!(UNorm16::from_f64(1.0), UNorm16::ONE);
        assert_eq!(UNorm16::from_f64(-4.0), UNorm16::ZERO);
        assert_eq!(UNorm16::from_f64(4.0), UNorm16::ONE);
        assert_eq!(UNorm16::from_f64(f64::NAN), UNorm16::ZERO);
        let half = UNorm16::from_f64(0.5).to_f64();
        assert!((half - 0.5).abs() < 1e-4, "{half}");
    }

    #[test]
    fn rewind_is_measured_from_the_older_basis_tick() {
        assert_eq!(claim().rewind_ticks(), Some(10));
        let ahead = HitClaim {
            basis: InterpBasis::exact(Tick::new(1_001)),
            ..claim()
        };
        assert_eq!(ahead.rewind_ticks(), None);
    }

    #[test]
    fn the_closed_window_is_the_default() {
        assert_eq!(HitWindow::default(), HitWindow::CLOSED);
        assert_eq!(HitWindow::CLOSED.history_ticks, 0);
    }

    fn key(seq: u16) -> HitClaimKey {
        HitClaimKey {
            shooter: PersistId::new(7),
            input_seq: seq,
        }
    }

    #[test]
    fn the_cap_is_derived_from_the_window_not_written_down() {
        let cap = HitClaimCap::for_window(HitWindow::new(12, 32));
        assert_eq!(cap.burst, 32, "burst is the ring's depth");
        assert_eq!(cap.refill_ticks, 1, "the fire-rate invariant's floor");
        assert_eq!(cap.sustained_claims_per_second(60), 60);
        assert_eq!(
            HitClaimCap::for_window(HitWindow::CLOSED),
            HitClaimCap::CLOSED
        );
        assert_eq!(HitClaimCap::default(), HitClaimCap::CLOSED);
    }

    #[test]
    fn an_over_cap_claim_is_refused_by_name_once_per_tick() {
        let mut gate = HitClaimGate::new(HitClaimCap::for_window(HitWindow::new(12, 32)));
        let now = Tick::new(100);
        for seq in 0..32 {
            assert_eq!(gate.admit(key(seq), now), Admission::Admit, "seq {seq}");
        }
        assert_eq!(
            gate.admit(key(32), now),
            Admission::Refuse(HitRefusal::OverClaimRate {
                burst: 32,
                refill_ticks: 1,
            })
        );
        // The flood is told once this tick; further copies are coalesced.
        assert_eq!(
            gate.admit(key(33), now),
            Admission::AlreadyAnswered { at: now }
        );
        assert_eq!(gate.refused(), 1);
        assert_eq!(gate.coalesced(), 1);
        // One tick later one token has refilled, and the refusal is fresh.
        let next = Tick::new(101);
        assert_eq!(gate.admit(key(32), next), Admission::Admit);
        assert_eq!(
            gate.admit(key(33), next),
            Admission::Refuse(HitRefusal::OverClaimRate {
                burst: 32,
                refill_ticks: 1,
            })
        );
    }

    #[test]
    fn a_resend_of_an_admitted_key_is_free_and_a_same_tick_copy_is_coalesced() {
        let mut gate = HitClaimGate::new(HitClaimCap::for_window(HitWindow::new(12, 32)));
        assert_eq!(gate.admit(key(1), Tick::new(10)), Admission::Admit);
        assert_eq!(gate.tokens(), 31);
        assert_eq!(
            gate.admit(key(1), Tick::new(10)),
            Admission::AlreadyAnswered { at: Tick::new(10) }
        );
        // Resent six ticks later, the verdict having been lost: free.
        assert_eq!(gate.admit(key(1), Tick::new(16)), Admission::Admit);
        assert_eq!(
            gate.tokens(),
            32,
            "the resend spent nothing and six ticks refilled"
        );
    }

    #[test]
    fn the_bucket_refills_at_the_stated_rate_and_never_runs_backwards() {
        let mut gate = HitClaimGate::new(HitClaimCap {
            burst: 4,
            refill_ticks: 3,
        });
        let t = Tick::new(30);
        for seq in 0..4 {
            assert_eq!(gate.admit(key(seq), t), Admission::Admit);
        }
        assert!(matches!(gate.admit(key(4), t), Admission::Refuse(_)));
        // Two ticks is not a token; three is.
        assert!(matches!(
            gate.admit(key(4), Tick::new(32)),
            Admission::Refuse(_)
        ));
        assert_eq!(gate.admit(key(4), Tick::new(33)), Admission::Admit);
        // A tick behind the last one refills nothing.
        assert!(matches!(
            gate.admit(key(5), Tick::new(20)),
            Admission::Refuse(_)
        ));
        assert_eq!(gate.last_seen(), Some(Tick::new(33)));
    }

    /// The refusal was appended, never inserted: every arm before it keeps
    /// the positional byte a version-8 build wrote (see `PROTOCOL_VERSION`).
    #[test]
    fn over_claim_rate_is_appended_after_every_older_refusal() {
        let bytes = postcard::to_stdvec(&HitRefusal::OverClaimRate {
            burst: 32,
            refill_ticks: 1,
        })
        .unwrap();
        assert_eq!(bytes[0], 8, "OverClaimRate is the ninth arm, after Miss");
        let miss = postcard::to_stdvec(&HitRefusal::Miss {
            miss_distance: 1,
            allowed: 1,
        })
        .unwrap();
        assert_eq!(miss[0], 7, "Miss stays where a version-8 build put it");
    }

    #[test]
    fn a_closed_cap_refuses_every_new_claim_by_name() {
        let mut gate = HitClaimGate::new(HitClaimCap::CLOSED);
        assert_eq!(
            gate.admit(key(0), Tick::new(0)),
            Admission::Refuse(HitRefusal::OverClaimRate {
                burst: 0,
                refill_ticks: 1,
            })
        );
    }
}
