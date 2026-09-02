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
}
