//! A read-only view of the ruleset's combat state, and the tracer tracks the
//! ruleset's own events describe.
//!
//! Nothing in this module decides anything. `lock_progress`, `lock_target`,
//! `hull`, `shield`, `cooldown` and a projectile's remaining `flight_ticks`
//! are all owned by `orrery_games::regolith` and are copied here verbatim for
//! drawing. `docs/15` §7 and #320 constraint 3 both land on the same rule: the
//! skin displays, it never computes, and it emits no orders.

use bevy::prelude::Resource;
use orrery_core::Executor;
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::order::{LockBreakReason, Outcome, ShotResult};
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_games::regolith::weapon::{Weapon, WeaponKind};
use orrery_games::regolith::LOCK_ACQUISITION_TICKS;
use orrery_games::Regolith;
use orrery_protocol::PersistId;

/// Tick marks on the acquisition ring: one per held-trigger tick the ruleset
/// requires, so a full ring is exactly `LOCK_ACQUISITION_TICKS`.
pub const LOCK_RING_SEGMENTS: usize = LOCK_ACQUISITION_TICKS as usize;

/// How many tracers the skin can draw at once.
pub const TRACER_POOL: usize = 24;

/// Ticks a lock-break banner stays up after the ruleset reports the break.
pub const BREAK_BANNER_TICKS: u16 = 90;

/// Ticks the shot-result cue stays up after the event that raised it.
pub const SHOT_CUE_TICKS: u16 = 45;

/// The three states the design draws for a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPhase {
    /// No `lock_target`: the ring is gone.
    Idle,
    /// `lock_target` is set and `lock_progress < LOCK_ACQUISITION_TICKS`.
    Acquiring,
    /// `lock_progress` reached the threshold; the trigger is live.
    Locked,
}

impl LockPhase {
    /// The design's label above the reticle.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "NO LOCK",
            Self::Acquiring => "ACQUIRING",
            Self::Locked => "LOCKED",
        }
    }
}

/// Everything the lock readout draws, copied out of one [`Craft`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockView {
    /// `Craft::lock_target`.
    pub target: Option<PersistId>,
    /// `Craft::lock_progress`.
    pub progress: u16,
    /// `Craft::locks_acquired`.
    pub acquired: u32,
}

impl LockView {
    /// Copies the lock fields out of a craft. This is the only place the skin
    /// reads them, and it copies — it never recomputes.
    #[must_use]
    pub const fn of(craft: &Craft) -> Self {
        Self {
            target: craft.lock_target,
            progress: craft.lock_progress,
            acquired: craft.locks_acquired,
        }
    }

    /// Which of the design's three reticle states this is.
    #[must_use]
    pub const fn phase(self) -> LockPhase {
        match self.target {
            None => LockPhase::Idle,
            Some(_) if self.progress >= LOCK_ACQUISITION_TICKS => LockPhase::Locked,
            Some(_) => LockPhase::Acquiring,
        }
    }

    /// Tick marks lit on the acquisition ring.
    ///
    /// One per tick of `lock_progress`, saturating at a full ring. With no
    /// target there is nothing to acquire, so the ring is empty regardless of
    /// whatever stale progress a previous target left behind.
    #[must_use]
    pub fn segments_lit(self) -> usize {
        if self.target.is_none() {
            return 0;
        }
        (self.progress as usize).min(LOCK_RING_SEGMENTS)
    }

    /// The reticle's caption line, matching the design's two rows.
    #[must_use]
    pub fn caption(self) -> String {
        match self.phase() {
            LockPhase::Idle => "no target".to_owned(),
            LockPhase::Acquiring => format!(
                "{} / {} t · {:.2} s",
                self.progress,
                LOCK_ACQUISITION_TICKS,
                f64::from(self.progress) / f64::from(orrery_core::TICK_HZ)
            ),
            LockPhase::Locked => format!(
                "{LOCK_ACQUISITION_TICKS} / {LOCK_ACQUISITION_TICKS} t · lock #{}",
                self.acquired
            ),
        }
    }
}

/// One shot the ruleset says is still in the air.
///
/// Every field is lifted straight off an [`Outcome::DamageDealt`]. The skin
/// runs no ballistics of its own: if the ruleset stops emitting the event, the
/// tracer stops existing on the same tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    /// Who fired.
    pub attacker: PersistId,
    /// Who it is aimed at; also whose step re-emits the event each tick.
    pub target: PersistId,
    /// `attacker_pos` from the event — the muzzle, in the ruleset's lattice.
    pub origin: orrery_core::QPos,
    /// `attacker_weapon` from the event.
    pub weapon: WeaponKind,
    /// `flight_ticks` from the event: ticks of flight still to run.
    pub remaining: u16,
    /// Total flight, recovered from the first event of this shot.
    pub total: u16,
}

impl Track {
    /// Fraction of the flight already run, in `0.0..=1.0`.
    #[must_use]
    pub fn travelled(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        let flown = f32::from(self.total.saturating_sub(self.remaining));
        (flown / f32::from(self.total)).clamp(0.0, 1.0)
    }
}

/// The shots currently in the air, rebuilt from scratch every ruleset tick.
#[derive(Debug, Default, Resource)]
pub struct ProjectileTracks {
    tracks: Vec<Track>,
}

impl ProjectileTracks {
    /// Replaces the track set with the one this tick's events describe.
    ///
    /// The ruleset re-emits a `DamageDealt` with a decremented `flight_ticks`
    /// on every tick a shot is still travelling, so one tick of events *is*
    /// the complete set of shots in the air. Rebuilding rather than advancing
    /// is deliberate: there is no skin-side clock that could drift from the
    /// ruleset's, and a shot that resolved this tick simply has no event and
    /// so has no tracer.
    ///
    /// A muzzle event carries `flight_ticks: None` and is skipped, because the
    /// same shot reappears one tick later with a real remaining count and the
    /// identical `attacker_pos`. Matching a continuation to last tick's track
    /// by `remaining + 1` recovers `total` for the flown fraction.
    pub fn observe(&mut self, events: &[Outcome]) {
        let mut carried = Vec::with_capacity(events.len());
        let mut consumed = vec![false; self.tracks.len()];
        for event in events {
            let Outcome::DamageDealt {
                attacker,
                target,
                attacker_pos,
                attacker_weapon,
                flight_ticks: Some(remaining),
                ..
            } = event
            else {
                continue;
            };
            let prior = self.tracks.iter().enumerate().find_map(|(index, track)| {
                (!consumed[index]
                    && track.attacker == *attacker
                    && track.target == *target
                    && track.remaining == remaining.saturating_add(1))
                .then_some(index)
            });
            let total = match prior {
                Some(index) => {
                    consumed[index] = true;
                    self.tracks[index].total
                }
                None => remaining.saturating_add(1),
            };
            carried.push(Track {
                attacker: *attacker,
                target: *target,
                origin: *attacker_pos,
                weapon: *attacker_weapon,
                remaining: *remaining,
                total,
            });
        }
        self.tracks = carried;
    }

    /// The shots in the air, in event order.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The nearest-to-landing shot this entity fired, if any.
    #[must_use]
    pub fn own_shot(&self, attacker: PersistId) -> Option<Track> {
        self.tracks
            .iter()
            .filter(|track| track.attacker == attacker)
            .min_by_key(|track| track.remaining)
            .copied()
    }
}

/// A lock break the ruleset reported, held on screen long enough to read.
#[derive(Debug, Default, Resource)]
pub struct LockBreak {
    /// Why the ruleset dropped the lock, while the banner is up.
    pub reason: Option<LockBreakReason>,
    /// Ticks the banner still has to run.
    pub ticks_left: u16,
}

impl LockBreak {
    /// Raises the banner for every break the ruleset reported for `locker`.
    pub fn observe(&mut self, events: &[Outcome], locker: PersistId) {
        for event in events {
            if let Outcome::LockBroken {
                locker: who,
                reason,
                ..
            } = event
            {
                if *who == locker {
                    self.reason = Some(*reason);
                    self.ticks_left = BREAK_BANNER_TICKS;
                }
            }
        }
    }

    /// Ages the banner by one tick.
    pub fn age(&mut self) {
        self.ticks_left = self.ticks_left.saturating_sub(1);
        if self.ticks_left == 0 {
            self.reason = None;
        }
    }

    /// The banner line, empty when nothing broke recently.
    #[must_use]
    pub fn banner(&self) -> String {
        match self.reason {
            None => String::new(),
            Some(LockBreakReason::RangeExceeded) => "LOCK BROKEN · RANGE EXCEEDED".to_owned(),
            Some(LockBreakReason::TargetDestroyed) => "LOCK BROKEN · TARGET DESTROYED".to_owned(),
        }
    }
}

/// How far the skin has got with one of the player's own shots.
///
/// Two layers, per the #383 owner decision: an immediate provisional cue the
/// skin raises on the shot's last in-flight tick, and the target's
/// authoritative verdict which arrives one delivery later and corrects it.
/// The skin never decides whether damage landed — `ShotResult` is copied out
/// of [`Outcome::ShotResolved`] verbatim, and a provisional cue that a later
/// authoritative event contradicts is explicitly accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotCue {
    /// The final in-flight tick was observed; the target's step adjudicates
    /// the shot next tick. Provisional only.
    Arrival {
        /// Target the shot was flying towards.
        target: PersistId,
    },
    /// The target's authoritative verdict.
    Resolved {
        /// Target that adjudicated the shot.
        target: PersistId,
        /// Hit or miss, as the ruleset rolled it.
        result: ShotResult,
    },
}

/// The player's most recent shot feedback, held on screen long enough to read.
#[derive(Debug, Default, Resource)]
pub struct ShotFeedback {
    /// The live cue, if any.
    pub cue: Option<ShotCue>,
    /// Ticks the cue still has to run.
    pub ticks_left: u16,
}

impl ShotFeedback {
    /// Arms the provisional arrival cue off the player's final in-flight tick.
    ///
    /// The last `DamageDealt` a shot ever carries has `flight_ticks == 1`;
    /// the target resolves on the following tick. Seeing that event is a fact
    /// about timing, not about the outcome — this claims nothing more than
    /// "an adjudication is due", which is all the skin may claim.
    pub fn arm_provisional(&mut self, tracks: &ProjectileTracks, shooter: PersistId) {
        if let Some(track) = tracks.own_shot(shooter) {
            if track.remaining == 1 {
                self.cue = Some(ShotCue::Arrival {
                    target: track.target,
                });
                self.ticks_left = SHOT_CUE_TICKS;
            }
        }
    }

    /// Reads this tick's events for the shooter's resolution — or for a lock
    /// break, which cancels a still-unconfirmed provisional cue.
    ///
    /// A shot can fail to be adjudicated at all: the range check runs before
    /// flight every tick, so `LockBroken` can retire a shot that never rolls.
    /// An authoritative verdict always wins over a break seen the same tick,
    /// because a hit that kills its target emits both.
    pub fn observe(&mut self, events: &[Outcome], shooter: PersistId) {
        let mut resolved = None;
        let mut broke = false;
        for event in events {
            match event {
                Outcome::ShotResolved {
                    attacker,
                    target,
                    result,
                } if *attacker == shooter => {
                    resolved = Some(ShotCue::Resolved {
                        target: *target,
                        result: *result,
                    });
                }
                Outcome::LockBroken { locker, .. } if *locker == shooter => broke = true,
                _ => {}
            }
        }
        match resolved {
            Some(cue) => {
                self.cue = Some(cue);
                self.ticks_left = SHOT_CUE_TICKS;
            }
            None if broke && matches!(self.cue, Some(ShotCue::Arrival { .. })) => {
                self.cue = None;
                self.ticks_left = 0;
            }
            None => {}
        }
    }

    /// Ages the cue by one tick.
    pub fn age(&mut self) {
        self.ticks_left = self.ticks_left.saturating_sub(1);
        if self.ticks_left == 0 {
            self.cue = None;
        }
    }

    /// The target the world-space impact flash anchors on, while one should
    /// draw: a provisional arrival, or an authoritative hit. A miss draws no
    /// flash — that is exactly the correction the verdict makes.
    #[must_use]
    pub fn flash_target(&self) -> Option<PersistId> {
        match self.cue {
            Some(ShotCue::Arrival { target }) => Some(target),
            Some(ShotCue::Resolved {
                target,
                result: ShotResult::Hit,
            }) => Some(target),
            _ => None,
        }
    }

    /// The HUD line, empty when nothing is live.
    #[must_use]
    pub fn banner(&self) -> String {
        match self.cue {
            None => String::new(),
            Some(ShotCue::Arrival { .. }) => "IMPACT…".to_owned(),
            Some(ShotCue::Resolved {
                result: ShotResult::Hit,
                ..
            }) => "HIT CONFIRMED".to_owned(),
            Some(ShotCue::Resolved {
                result: ShotResult::Miss,
                ..
            }) => "MISS".to_owned(),
        }
    }
}

/// One craft's drawable numbers, all copied from its hashed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CraftView {
    /// Who this is.
    pub entity: PersistId,
    /// `Craft::archetype`.
    pub archetype: Archetype,
    /// `Craft::weapon`.
    pub weapon: WeaponKind,
    /// `Craft::hull`.
    pub hull: i32,
    /// `Craft::shield`.
    pub shield: i32,
    /// `Craft::cooldown`.
    pub cooldown: u16,
    /// `Craft::respawn_in`.
    pub respawn_in: u16,
    /// `Craft::shots`.
    pub shots: u32,
    /// Lattice position, for the range readout.
    pub pos: orrery_core::QPos,
    /// Lattice velocity, for the speed readout.
    pub vel: orrery_core::QVel,
    /// `Craft::score`, already derived by the ruleset.
    pub score: u64,
}

impl CraftView {
    /// Copies one craft.
    #[must_use]
    pub fn of(entity: PersistId, craft: &Craft) -> Self {
        Self {
            entity,
            archetype: craft.archetype,
            weapon: craft.weapon,
            hull: craft.hull,
            shield: craft.shield,
            cooldown: craft.cooldown,
            respawn_in: craft.respawn_in,
            shots: craft.shots,
            pos: craft.pos,
            vel: craft.vel,
            score: craft.score(),
        }
    }

    /// Hull ceiling published by the chassis.
    #[must_use]
    pub fn max_hull(&self) -> i32 {
        self.archetype.limits().max_hull
    }

    /// Shield ceiling published by the chassis.
    #[must_use]
    pub fn max_shield(&self) -> i32 {
        self.archetype.limits().max_shield
    }

    /// Speed ceiling published by the chassis, in metres per second.
    #[must_use]
    pub fn max_speed_ms(&self) -> f64 {
        self.archetype.limits().max_speed_mms as f64 / 1_000.0
    }

    /// Current speed, in metres per second.
    #[must_use]
    pub fn speed_ms(&self) -> f64 {
        let (x, y, z) = self.vel.to_metres_per_sec();
        (x * x + y * y + z * z).sqrt()
    }

    /// The equipped weapon's published table row.
    #[must_use]
    pub fn weapon_table(&self) -> Weapon {
        self.weapon.weapon()
    }

    /// The chassis name the HUD prints.
    #[must_use]
    pub const fn chassis_name(&self) -> &'static str {
        match self.archetype {
            Archetype::Interceptor => "INTERCEPTOR",
            Archetype::Cruiser => "CRUISER",
        }
    }
}

/// Where a target sits relative to the shooter's weapon envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeBand {
    /// Inside `optimal_mm`: no range penalty.
    Optimal,
    /// Between `optimal_mm` and `optimal_mm + falloff_mm`.
    Falloff,
    /// Past the falloff edge, where the ruleset raises `RangeExceeded`.
    Beyond,
}

impl RangeBand {
    /// The HUD's phrase for this band.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Optimal => "inside optimal",
            Self::Falloff => "in falloff",
            Self::Beyond => "beyond reach",
        }
    }

    /// Classifies a separation against a weapon's own published limits.
    ///
    /// This reads the weapon table and compares two numbers: it is the
    /// *weapon envelope*, not the resolver's reach. The resolver raises
    /// `RangeExceeded` at `optimal + falloff + target_radius_mm`
    /// (`regolith/mod.rs`, `projectile_resolution`), which is strictly wider
    /// than this by the target's own signature radius. [`reach_mm`] is the
    /// resolver's number and is what [`CombatView::hit_forecast`] uses;
    /// this stays the envelope the weapon panel prints.
    #[must_use]
    pub fn of(range_mm: i64, weapon: Weapon) -> Self {
        if range_mm <= weapon.optimal_mm {
            Self::Optimal
        } else if range_mm <= weapon.optimal_mm.saturating_add(weapon.falloff_mm) {
            Self::Falloff
        } else {
            Self::Beyond
        }
    }
}

/// The ruleset's fixed-point scale for a hit chance: `hit_chance_ppm` returns
/// parts per million of this, and the roll is `uniform_below(rng, SCALE)`.
///
/// Mirrors `CHANCE_SCALE` in `orrery_games::regolith`, which is private.
pub const CHANCE_SCALE: u32 = 1_000_000;

/// The signature radius the ruleset's tracking term is normalised against.
///
/// Mirrors the private `REFERENCE_SIGNATURE_RADIUS_MM`. It is the
/// interceptor's `radius_mm`, so an interceptor-sized target neither helps
/// nor hurts the tracking term.
const REFERENCE_SIGNATURE_RADIUS_MM: u128 = 3_000;

/// The separation past which the resolver stops rolling and breaks the lock.
///
/// This is the resolver's own `reach`: `optimal + falloff + target_radius`.
#[must_use]
pub fn reach_mm(weapon: Weapon, target_radius_mm: i64) -> i64 {
    weapon
        .optimal_mm
        .saturating_add(weapon.falloff_mm)
        .saturating_add(target_radius_mm)
}

/// `sum_squares`, refusing rather than saturating.
fn checked_sum_squares(values: [i128; 3]) -> Option<u128> {
    let mut sum = 0u128;
    for value in values {
        let magnitude = value.unsigned_abs();
        sum = sum.checked_add(magnitude.checked_mul(magnitude)?)?;
    }
    Some(sum)
}

/// The ruleset's own hit chance, in parts per million, for the geometry it
/// would resolve against.
///
/// This is a transcription of `hit_chance_ppm` in
/// `orrery_games::regolith`, which is a private function of a crate this
/// client may read but must not change. Every step below is the same
/// expression in the same order and the same integer types, so where both
/// answer they answer identically.
///
/// The one deliberate divergence is **saturation**. The ruleset uses
/// `saturating_mul` / `saturating_add`, so an input large enough to overflow
/// `i128` or `u128` silently pins the intermediate at its type maximum and
/// the ruleset goes on to produce a number that looks like a chance and is
/// not one. D43 clause (f) has no witnessed saturation flag yet (#447 F2),
/// so nothing downstream can tell that happened. This transcription uses
/// checked arithmetic and returns [`None`] instead: where the ruleset would
/// have saturated, the skin declines to name a band rather than echoing a
/// confidently wrong one.
///
/// `saturating_sub` on `range_mm - optimal` is *not* one of those cases — on
/// `u128` it is a deliberate floor at zero ("no range penalty inside
/// optimal"), not an overflow guard, so it is transcribed verbatim.
///
/// One honest caveat about the guards. A cross-product term can only
/// overflow `i128` when the separation itself is around `9.2e18` mm, and at
/// that separation `range_ratio²` has already overflowed `u128`, so the
/// range term refuses first. The checked arithmetic on the cross product is
/// therefore defence in depth: replacing it with `saturating_mul` changes
/// no answer this function can be asked for. It is kept because "the
/// dominating guard happens to fire first" is a property of the current
/// weapon table, not of the expression.
#[must_use]
pub fn hit_chance_ppm(
    target_pos: orrery_core::QPos,
    target_vel: orrery_core::QVel,
    target_radius_mm: i64,
    attacker_pos: orrery_core::QPos,
    attacker_vel: orrery_core::QVel,
    weapon: Weapon,
) -> Option<u32> {
    let rx = i128::from(target_pos.x).checked_sub(i128::from(attacker_pos.x))?;
    let ry = i128::from(target_pos.y).checked_sub(i128::from(attacker_pos.y))?;
    let rz = i128::from(target_pos.z).checked_sub(i128::from(attacker_pos.z))?;
    let vx = i128::from(target_vel.x).checked_sub(i128::from(attacker_vel.x))?;
    let vy = i128::from(target_vel.y).checked_sub(i128::from(attacker_vel.y))?;
    let vz = i128::from(target_vel.z).checked_sub(i128::from(attacker_vel.z))?;
    let range_sq = checked_sum_squares([rx, ry, rz])?;
    let range_mm = integer_sqrt(range_sq);

    let cross = [
        ry.checked_mul(vz)?.checked_sub(rz.checked_mul(vy)?)?,
        rz.checked_mul(vx)?.checked_sub(rx.checked_mul(vz)?)?,
        rx.checked_mul(vy)?.checked_sub(ry.checked_mul(vx)?)?,
    ];
    let cross_magnitude = integer_sqrt(checked_sum_squares(cross)?);
    let angular_urad_per_sec = cross_magnitude
        .checked_mul(1_000_000)?
        .checked_div(range_sq)
        .unwrap_or(0);
    let scale = u128::from(CHANCE_SCALE);
    let tracking_denominator =
        u128::from(weapon.tracking_urad_per_sec).checked_mul(target_radius_mm.max(1) as u128)?;
    let tracking_ratio = angular_urad_per_sec
        .checked_mul(REFERENCE_SIGNATURE_RADIUS_MM)?
        .checked_mul(scale)?
        / tracking_denominator.max(1);

    let optimal = weapon.optimal_mm.max(0) as u128;
    let range_ratio =
        range_mm.saturating_sub(optimal).checked_mul(scale)? / (weapon.falloff_mm.max(1) as u128);
    let penalty = tracking_ratio
        .checked_mul(tracking_ratio)?
        .checked_add(range_ratio.checked_mul(range_ratio)?)?;
    let denominator = scale.checked_mul(scale)?.checked_add(penalty)?;
    let chance = scale.checked_mul(scale)?.checked_mul(scale)? / denominator.max(1);
    Some(u32::try_from(chance.min(scale)).unwrap_or(CHANCE_SCALE))
}

/// A qualitative reading of the ruleset's hit chance.
///
/// The owner's decision (#445): a band, not a percentage. A number implies a
/// resolution the adjudicator does not have — `est. 100%` followed by a miss
/// reads as a lie even when the estimate was honest — while a band carries
/// the two things a pilot can act on: *is my tracking holding* and *am I in
/// reach*.
///
/// The boundaries are chosen where a false model would show:
///
/// * **`Perfect` at exactly `CHANCE_SCALE`.** The roll is
///   `uniform_below(rng, CHANCE_SCALE) < chance`, which draws from
///   `0..CHANCE_SCALE`, so `chance == CHANCE_SCALE` is the *only* value at
///   which "cannot miss" is a true statement. One ppm below it there is a
///   real miss branch, and a band claiming perfection there would be a lie
///   the player could catch.
/// * **`NoChance` at exactly zero, and out of reach.** `draw < 0` is never
///   true, so zero is the only value at which "cannot hit" is true. Out past
///   the resolver's reach the projectile is not rolled at all — it raises
///   `RangeExceeded` and breaks the lock — so that is `NoChance` too, even
///   though the chance *expression* evaluated there still yields a small
///   positive number. Printing that number would be the false model.
/// * **`Good` at half.** `chance = SCALE / (1 + u²)` where `u` is the total
///   penalty `sqrt(tracking² + range²) / SCALE` the ruleset forms. `u = 1` —
///   penalty exactly equal to the scale — is `chance = SCALE/2`: the point
///   where a shot is more likely to land than not.
/// * **`Fair` at a tenth.** `u = 3`, three times the penalty budget.
///
/// The two interior boundaries are round numbers *in the penalty the ruleset
/// forms*, not in the output; the two exterior ones are exact properties of
/// the ruleset's own comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HitBand {
    /// The resolver cannot land this shot: zero chance, or out of reach.
    NoChance,
    /// Under a tenth. Tracking is losing, or the target is deep in falloff.
    Poor,
    /// Between a tenth and a half.
    Fair,
    /// Better than even.
    Good,
    /// Exactly `CHANCE_SCALE`: the ruleset has no miss branch here.
    Perfect,
    /// The inputs would have overflowed the ruleset's saturating arithmetic,
    /// so no honest band exists. See [`hit_chance_ppm`].
    Unreadable,
}

impl HitBand {
    /// Classifies a chance the ruleset actually rolls.
    #[must_use]
    pub const fn of_chance_ppm(chance: u32) -> Self {
        if chance == 0 {
            Self::NoChance
        } else if chance < CHANCE_SCALE / 10 {
            Self::Poor
        } else if chance < CHANCE_SCALE / 2 {
            Self::Fair
        } else if chance < CHANCE_SCALE {
            Self::Good
        } else {
            Self::Perfect
        }
    }

    /// The word the HUD prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoChance => "NO CHANCE",
            Self::Poor => "POOR",
            Self::Fair => "FAIR",
            Self::Good => "GOOD",
            Self::Perfect => "PERFECT",
            Self::Unreadable => "NO READ",
        }
    }

    /// The one-line reason, so the band explains itself rather than being a
    /// second unexplained signal.
    #[must_use]
    pub const fn note(self) -> &'static str {
        match self {
            Self::NoChance => "out of reach or no tracking",
            Self::Poor => "tracking is losing",
            Self::Fair => "tracking is slipping",
            Self::Good => "tracking is holding",
            Self::Perfect => "no miss branch",
            Self::Unreadable => "inputs out of range",
        }
    }
}

/// The complete drawable picture of one tick, from one craft's seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Resource)]
pub struct CombatView {
    /// The craft this client flies, when the executor still holds one.
    pub own: Option<CraftView>,
    /// The lock fields of that craft.
    pub lock: LockView,
    /// The locked target's own state, when it is a craft this client can see.
    pub target: Option<CraftView>,
}

impl CombatView {
    /// Reads one tick out of the shared headless executor.
    ///
    /// Pure: it copies hashed fields into plain numbers and returns. It writes
    /// nothing back, so it cannot change which orders the pipeline emits.
    #[must_use]
    pub fn read(executor: &Executor<Regolith>, me: PersistId) -> Self {
        let own = craft_of(executor, me).map(|craft| CraftView::of(me, craft));
        let lock = craft_of(executor, me).map(LockView::of).unwrap_or_default();
        let target = lock.target.and_then(|target| {
            craft_of(executor, target).map(|craft| CraftView::of(target, craft))
        });
        Self { own, lock, target }
    }

    /// Straight-line separation to the locked target, in millimetres.
    #[must_use]
    pub fn range_mm(&self) -> Option<i64> {
        let own = self.own?;
        let target = self.target?;
        let (dx, dy, dz) = (
            i128::from(target.pos.x) - i128::from(own.pos.x),
            i128::from(target.pos.y) - i128::from(own.pos.y),
            i128::from(target.pos.z) - i128::from(own.pos.z),
        );
        let square = dx * dx + dy * dy + dz * dz;
        i64::try_from(integer_sqrt(square.unsigned_abs())).ok()
    }

    /// Which band the locked target sits in for the equipped weapon.
    #[must_use]
    pub fn band(&self) -> Option<RangeBand> {
        Some(RangeBand::of(self.range_mm()?, self.own?.weapon_table()))
    }

    /// The ruleset's own hit chance for a shot fired **now**, in ppm.
    ///
    /// It is a forecast, not a replay of an adjudicated shot. The ruleset
    /// rolls against the attacker snapshot carried on the `Damage` order —
    /// the shooter's position and velocity at the tick the trigger was
    /// pulled — and the target's state at the tick the projectile arrives.
    /// This reads both craft as they are on the current tick, which is the
    /// question a pilot is actually asking: *if I fire now, from here.*
    ///
    /// [`None`] means there is nothing to read: no own craft, no visible
    /// target craft, or inputs the ruleset's arithmetic would have saturated
    /// on.
    #[must_use]
    pub fn hit_chance_ppm(&self) -> Option<u32> {
        let own = self.own?;
        let target = self.target?;
        hit_chance_ppm(
            target.pos,
            target.vel,
            target.archetype.limits().radius_mm,
            own.pos,
            own.vel,
            own.weapon_table(),
        )
    }

    /// The qualitative band beside the locked target.
    ///
    /// [`None`] only when there is no locked target craft to read. A target
    /// past the resolver's reach is [`HitBand::NoChance`] — the resolver
    /// breaks the lock there instead of rolling — and inputs that would have
    /// saturated are [`HitBand::Unreadable`].
    #[must_use]
    pub fn hit_forecast(&self) -> Option<HitBand> {
        let own = self.own?;
        let target = self.target?;
        let weapon = own.weapon_table();
        let radius = target.archetype.limits().radius_mm;
        let separation = separation_mm(own.pos, target.pos);
        if separation > reach_mm(weapon, radius).max(0) as u128 {
            return Some(HitBand::NoChance);
        }
        Some(
            self.hit_chance_ppm()
                .map_or(HitBand::Unreadable, HitBand::of_chance_ppm),
        )
    }
}

/// Floor of the straight-line separation between two lattice points, in
/// millimetres, without an intermediate `i64` that a replicated coordinate
/// pair could overflow.
fn separation_mm(a: orrery_core::QPos, b: orrery_core::QPos) -> u128 {
    let delta = |p: i64, q: i64| i128::from(p) - i128::from(q);
    let (dx, dy, dz) = (
        delta(a.x, b.x).unsigned_abs(),
        delta(a.y, b.y).unsigned_abs(),
        delta(a.z, b.z).unsigned_abs(),
    );
    integer_sqrt(
        dx.saturating_mul(dx)
            .saturating_add(dy.saturating_mul(dy))
            .saturating_add(dz.saturating_mul(dz)),
    )
}

fn craft_of(executor: &Executor<Regolith>, entity: PersistId) -> Option<&Craft> {
    match executor.state(entity)? {
        RegolithState::Craft(craft) => Some(craft),
        _ => None,
    }
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut guess = value;
    let mut next = (guess + value / guess) / 2;
    while next < guess {
        guess = next;
        next = (guess + value / guess) / 2;
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::{QPos, QVel};

    fn craft() -> Craft {
        Craft::spawned(Archetype::Interceptor, QPos::from_metres(0.0, 0.0, 0.0), 0)
    }

    fn shot(attacker: u64, target: u64, remaining: Option<u16>) -> Outcome {
        Outcome::DamageDealt {
            attacker: PersistId::new(attacker),
            target: PersistId::new(target),
            amount: 11,
            attacker_pos: QPos::from_metres(10.0, 0.0, 20.0),
            attacker_vel: QVel::default(),
            attacker_weapon: WeaponKind::Stock,
            flight_ticks: remaining,
        }
    }

    #[test]
    fn lock_view_copies_every_acquisition_tick() {
        let mut craft = craft();
        craft.lock_target = Some(PersistId::new(2));
        for progress in 0..=LOCK_ACQUISITION_TICKS + 5 {
            craft.lock_progress = progress;
            let view = LockView::of(&craft);
            assert_eq!(view.progress, progress, "the view must copy lock_progress");
            assert_eq!(
                view.segments_lit(),
                (progress as usize).min(LOCK_RING_SEGMENTS),
                "the ring must light one mark per acquisition tick"
            );
            assert_eq!(
                view.phase(),
                if progress >= LOCK_ACQUISITION_TICKS {
                    LockPhase::Locked
                } else {
                    LockPhase::Acquiring
                }
            );
        }
    }

    #[test]
    fn no_target_means_no_ring() {
        let mut craft = craft();
        craft.lock_target = None;
        craft.lock_progress = 17;
        let view = LockView::of(&craft);
        assert_eq!(view.phase(), LockPhase::Idle);
        assert_eq!(view.segments_lit(), 0);
    }

    #[test]
    fn a_muzzle_event_alone_draws_nothing() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(1, 2, None)]);
        assert!(
            tracks.tracks().is_empty(),
            "the muzzle event carries no flight time; the continuation does"
        );
    }

    #[test]
    fn a_tracer_walks_the_rulesets_own_flight_ticks() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(1, 2, None)]);
        tracks.observe(&[shot(1, 2, Some(4))]);
        assert_eq!(tracks.tracks().len(), 1);
        assert_eq!(tracks.tracks()[0].total, 5);
        assert_eq!(tracks.tracks()[0].remaining, 4);
        assert!((tracks.tracks()[0].travelled() - 0.2).abs() < 1e-6);
        for remaining in [3u16, 2, 1] {
            tracks.observe(&[shot(1, 2, Some(remaining))]);
            let track = tracks.tracks()[0];
            assert_eq!(track.total, 5, "total must survive the whole flight");
            assert_eq!(track.remaining, remaining);
            let expected = (5.0 - f32::from(remaining)) / 5.0;
            assert!((track.travelled() - expected).abs() < 1e-6);
        }
        // Resolution emits no event, so the tracer must vanish with it.
        tracks.observe(&[]);
        assert!(tracks.tracks().is_empty());
    }

    #[test]
    fn concurrent_shots_keep_separate_tracks() {
        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(1, 2, Some(6)), shot(1, 2, Some(3))]);
        assert_eq!(tracks.tracks().len(), 2);
        tracks.observe(&[shot(1, 2, Some(5)), shot(1, 2, Some(2))]);
        let remaining: Vec<_> = tracks.tracks().iter().map(|t| t.remaining).collect();
        assert_eq!(remaining, vec![5, 2]);
        assert!(tracks.tracks().iter().all(|t| t.total == 7 || t.total == 4));
        assert_eq!(
            tracks.own_shot(PersistId::new(1)).map(|t| t.remaining),
            Some(2)
        );
    }

    #[test]
    fn the_break_banner_names_the_rulesets_reason() {
        let mut banner = LockBreak::default();
        assert!(banner.banner().is_empty());
        banner.observe(
            &[Outcome::LockBroken {
                locker: PersistId::new(1),
                target: PersistId::new(2),
                reason: LockBreakReason::RangeExceeded,
            }],
            PersistId::new(1),
        );
        assert!(banner.banner().contains("RANGE"));
        for _ in 0..BREAK_BANNER_TICKS {
            banner.age();
        }
        assert!(banner.banner().is_empty(), "the banner must expire");
    }

    #[test]
    fn a_break_reported_to_someone_else_is_not_mine() {
        let mut banner = LockBreak::default();
        banner.observe(
            &[Outcome::LockBroken {
                locker: PersistId::new(9),
                target: PersistId::new(2),
                reason: LockBreakReason::TargetDestroyed,
            }],
            PersistId::new(1),
        );
        assert!(banner.banner().is_empty());
    }

    fn resolved(attacker: u64, result: ShotResult) -> Outcome {
        Outcome::ShotResolved {
            attacker: PersistId::new(attacker),
            target: PersistId::new(2),
            result,
        }
    }

    /// The two layers of #383's owner decision, in order: a provisional cue
    /// armed off the shot's own final flight tick, then the authoritative
    /// verdict that replaces it.
    #[test]
    fn provisional_arrival_is_armed_then_corrected_by_the_verdict() {
        let me = PersistId::new(1);
        let mut tracks = ProjectileTracks::default();
        let mut feedback = ShotFeedback::default();

        // Mid-flight: nothing armed yet — the skin claims no arrival early.
        tracks.observe(&[shot(1, 2, Some(3))]);
        feedback.arm_provisional(&tracks, me);
        assert!(feedback.cue.is_none());

        // The last leg: an arrival is due next tick. That is all it says.
        tracks.observe(&[shot(1, 2, Some(2))]);
        tracks.observe(&[shot(1, 2, Some(1))]);
        feedback.arm_provisional(&tracks, me);
        assert_eq!(
            feedback.cue,
            Some(ShotCue::Arrival {
                target: PersistId::new(2)
            })
        );
        assert_eq!(
            feedback.flash_target(),
            Some(PersistId::new(2)),
            "the provisional flash draws on the target"
        );
        assert_eq!(feedback.banner(), "IMPACT…");

        // The target's verdict corrects it; a miss withdraws the flash.
        feedback.observe(&[resolved(1, ShotResult::Miss)], me);
        assert_eq!(
            feedback.cue,
            Some(ShotCue::Resolved {
                target: PersistId::new(2),
                result: ShotResult::Miss,
            })
        );
        assert_eq!(feedback.banner(), "MISS");
        assert_eq!(
            feedback.flash_target(),
            None,
            "a miss must retract the provisional flash"
        );

        // And expiry takes the whole cue with it.
        for _ in 0..SHOT_CUE_TICKS {
            feedback.age();
        }
        assert!(feedback.cue.is_none());
        assert!(feedback.banner().is_empty());
    }

    #[test]
    fn a_hit_verdict_confirms_the_flash_and_expires() {
        let mut feedback = ShotFeedback::default();
        feedback.observe(&[resolved(1, ShotResult::Hit)], PersistId::new(1));
        assert_eq!(feedback.banner(), "HIT CONFIRMED");
        assert_eq!(
            feedback.flash_target(),
            Some(PersistId::new(2)),
            "an authoritative hit keeps the flash up"
        );
        feedback.age();
        assert!(feedback.cue.is_some(), "one tick is not SHOT_CUE_TICKS");
    }

    #[test]
    fn someone_elses_shot_never_fires_my_cue() {
        let mut feedback = ShotFeedback::default();
        feedback.observe(&[resolved(9, ShotResult::Hit)], PersistId::new(1));
        assert!(feedback.cue.is_none(), "resolution names its shooter");

        let mut tracks = ProjectileTracks::default();
        tracks.observe(&[shot(9, 2, Some(1))]);
        feedback.arm_provisional(&tracks, PersistId::new(1));
        assert!(
            feedback.cue.is_none(),
            "another shooter's final leg arms nobody"
        );
    }

    /// A shot can die unadjudicated — the range check runs before flight —
    /// so a break must cancel a pending provisional cue. A verdict already in
    /// hand survives: a killing hit emits both events in one tick.
    #[test]
    fn a_lock_break_cancels_a_provisional_but_never_a_verdict() {
        let me = PersistId::new(1);
        let break_event = |reason| Outcome::LockBroken {
            locker: me,
            target: PersistId::new(2),
            reason,
        };

        let mut feedback = ShotFeedback {
            cue: Some(ShotCue::Arrival {
                target: PersistId::new(2),
            }),
            ticks_left: SHOT_CUE_TICKS,
        };
        feedback.observe(&[break_event(LockBreakReason::RangeExceeded)], me);
        assert!(
            feedback.cue.is_none(),
            "an unadjudicated shot must take its provisional flash back"
        );
        assert!(feedback.banner().is_empty());

        let mut feedback = ShotFeedback::default();
        feedback.observe(&[resolved(1, ShotResult::Hit)], me);
        feedback.observe(&[break_event(LockBreakReason::TargetDestroyed)], me);
        assert_eq!(
            feedback.cue,
            Some(ShotCue::Resolved {
                target: PersistId::new(2),
                result: ShotResult::Hit,
            }),
            "the verdict outlives the kill-shot's lock break"
        );
    }

    /// End to end against the real rules: hold the trigger, watch the
    /// provisional cue arm on a genuine final leg, then confirm every
    /// verdict the skin shows came verbatim from a `ShotResolved` the
    /// target's step emitted. This is #383's owner decision made executable:
    /// feedback always arrives, the skin never invents its outcome.
    #[test]
    fn live_fire_arms_a_provisional_and_confirms_it_from_the_ruleset() {
        use orrery_games::{Game, Regolith as Ruleset};
        use orrery_protocol::{Tick, UniverseSeed};

        let seed = UniverseSeed([0x61; 32]);
        let game = Ruleset::honest();
        let mut executor = Executor::new(game, seed);
        let me = PersistId::new(1);
        let them = PersistId::new(2);
        executor.insert(me, game.spawn(me, 0));
        executor.insert(them, game.spawn(them, 1));
        let my_pipeline = crate::intent::IntentPipeline::new(seed, me, 0, vec![them]);
        let their_pipeline = crate::intent::IntentPipeline::new(seed, them, 1, vec![me]);

        let held = crate::intent::Controls {
            fire: true,
            thrust: true,
            ..crate::intent::Controls::default()
        };
        use orrery_games::regolith::order::Order;
        let mut pending = std::collections::BTreeMap::<PersistId, Vec<Order>>::new();
        let mut tracks = ProjectileTracks::default();
        let mut feedback = ShotFeedback::default();
        let mut arrivals_armed = 0usize;
        let mut verdicts = Vec::new();

        for raw in 0..150u64 {
            let tick = Tick::new(raw);
            let mut delivered = std::collections::BTreeMap::<PersistId, Vec<Order>>::new();
            let mut emitted = Vec::new();
            for (entity, mut orders) in [
                (me, my_pipeline.human_orders(tick, held)),
                (them, their_pipeline.bot_orders(tick)),
            ] {
                let mut inbox = pending.remove(&entity).unwrap_or_default();
                inbox.append(&mut orders);
                let outcome = executor
                    .step_entity(entity, tick, &inbox)
                    .expect("both craft installed");
                for event in &outcome.events {
                    if let Some((target, input)) = executor.ruleset().deliver(event) {
                        delivered.entry(target).or_default().push(input);
                    }
                }
                emitted.extend(outcome.events.iter().cloned());
            }
            pending = delivered;

            tracks.observe(&emitted);
            feedback.age();
            feedback.arm_provisional(&tracks, me);
            if matches!(feedback.cue, Some(ShotCue::Arrival { .. })) {
                arrivals_armed += 1;
            }
            let before = feedback.cue;
            feedback.observe(&emitted, me);

            // The cue stays up for SHOT_CUE_TICKS after its event, so the
            // cross-check belongs to the tick the verdict was raised.
            if let (Some(ShotCue::Resolved { target, result }), before_cue) = (feedback.cue, before)
            {
                if before_cue != feedback.cue {
                    let source = emitted.iter().any(|event| {
                        matches!(
                            event,
                            Outcome::ShotResolved {
                                attacker,
                                target: who,
                                result: rolled,
                            } if *attacker == me && *who == target && *rolled == result
                        )
                    });
                    assert!(
                        source,
                        "tick {raw}: the skin showed {result:?} with no authoritative event behind it"
                    );
                    verdicts.push(result);
                }
            }
        }

        assert!(
            arrivals_armed >= 1,
            "held trigger over 150 ticks must arm at least one provisional arrival"
        );
        assert!(
            !verdicts.is_empty(),
            "and at least one authoritative verdict must land"
        );
    }

    #[test]
    fn range_bands_follow_the_published_weapon_table() {
        let stock = WeaponKind::Stock.weapon();
        assert_eq!(RangeBand::of(0, stock), RangeBand::Optimal);
        assert_eq!(RangeBand::of(stock.optimal_mm, stock), RangeBand::Optimal);
        assert_eq!(
            RangeBand::of(stock.optimal_mm + 1, stock),
            RangeBand::Falloff
        );
        assert_eq!(
            RangeBand::of(stock.optimal_mm + stock.falloff_mm, stock),
            RangeBand::Falloff
        );
        assert_eq!(
            RangeBand::of(stock.optimal_mm + stock.falloff_mm + 1, stock),
            RangeBand::Beyond
        );
    }

    #[test]
    fn integer_sqrt_is_exact_on_squares() {
        for value in [0u128, 1, 4, 9, 144, 1_000_000, 250_000 * 250_000] {
            let root = integer_sqrt(value);
            assert!(root * root <= value && (root + 1) * (root + 1) > value);
        }
    }
}

#[cfg(test)]
mod band_boundaries {
    use super::*;
    use orrery_core::{QPos, QVel};

    fn view(own_x: i64, target_x: i64, target_vz: i64) -> CombatView {
        let mut me = Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: own_x,
                y: 0,
                z: 0,
            },
            0,
        );
        me.lock_target = Some(PersistId::new(2));
        me.lock_progress = LOCK_ACQUISITION_TICKS;
        let mut them = Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: target_x,
                y: 0,
                z: 0,
            },
            0,
        );
        them.vel = QVel {
            x: 0,
            y: 0,
            z: target_vz,
        };
        CombatView {
            own: Some(CraftView::of(PersistId::new(1), &me)),
            lock: LockView::of(&me),
            target: Some(CraftView::of(PersistId::new(2), &them)),
        }
    }

    /// Every boundary, at the ppm it claims, in both directions.
    #[test]
    fn the_bands_break_exactly_where_they_say_they_do() {
        assert_eq!(HitBand::of_chance_ppm(0), HitBand::NoChance);
        assert_eq!(HitBand::of_chance_ppm(1), HitBand::Poor);
        assert_eq!(HitBand::of_chance_ppm(99_999), HitBand::Poor);
        assert_eq!(HitBand::of_chance_ppm(100_000), HitBand::Fair);
        assert_eq!(HitBand::of_chance_ppm(499_999), HitBand::Fair);
        assert_eq!(HitBand::of_chance_ppm(500_000), HitBand::Good);
        assert_eq!(HitBand::of_chance_ppm(999_999), HitBand::Good);
        assert_eq!(HitBand::of_chance_ppm(1_000_000), HitBand::Perfect);
    }

    /// The interior boundaries are round numbers in the *penalty* the ruleset
    /// forms, not in the output, so they are checked that way: `u = 1` is half
    /// and `u = 3` is a tenth, where `chance = SCALE / (1 + u²)`.
    #[test]
    fn the_interior_boundaries_are_the_rulesets_penalty_landmarks() {
        let scale = u128::from(CHANCE_SCALE);
        let chance_at = |penalty: u128| {
            let denominator = scale * scale + penalty;
            u32::try_from((scale * scale * scale / denominator).min(scale)).expect("in range")
        };
        // u = 1: penalty == SCALE².
        assert_eq!(chance_at(scale * scale), CHANCE_SCALE / 2);
        assert_eq!(
            HitBand::of_chance_ppm(chance_at(scale * scale)),
            HitBand::Good,
            "an even-money shot is the bottom of GOOD"
        );
        // u = 3: penalty == 9·SCALE².
        assert_eq!(chance_at(9 * scale * scale), CHANCE_SCALE / 10);
        assert_eq!(
            HitBand::of_chance_ppm(chance_at(9 * scale * scale)),
            HitBand::Fair,
            "a one-in-ten shot is the bottom of FAIR"
        );
    }

    /// Past the resolver's reach the projectile is never rolled, so the band
    /// is NO CHANCE even though the chance expression still evaluates to a
    /// small positive number there. Printing that number is the false model
    /// this boundary exists to stop.
    #[test]
    fn out_of_reach_is_no_chance_not_a_small_number() {
        let weapon = WeaponKind::Stock.weapon();
        let radius = Archetype::Interceptor.limits().radius_mm;
        let reach = reach_mm(weapon, radius);

        // At reach the range term alone has eaten most of the chance, but
        // the resolver still rolls, so the band still names a real chance.
        let inside = view(0, reach, 0);
        assert_eq!(inside.hit_forecast(), Some(HitBand::Fair));

        let outside = view(0, reach + 1, 0);
        assert_eq!(outside.hit_forecast(), Some(HitBand::NoChance));
        // And the raw expression, unguarded, would have said otherwise.
        assert!(
            outside.hit_chance_ppm().expect("no saturation here") > 0,
            "the guard is doing real work: the bare formula is still positive here"
        );
    }

    /// Tracking, the term the band exists to make visible: hold range fixed,
    /// wind up the transverse speed, and the band walks down its own ladder.
    #[test]
    fn the_band_walks_down_as_tracking_falls_behind() {
        let optimal = WeaponKind::Stock.weapon().optimal_mm;
        let mut seen = Vec::new();
        for transverse in [0i64, 20_000, 60_000, 200_000, 1_000_000_000] {
            seen.push(view(0, optimal, transverse).hit_forecast().expect("locked"));
        }
        assert_eq!(
            seen,
            vec![
                HitBand::Perfect,
                HitBand::Good,
                HitBand::Fair,
                HitBand::Poor,
                HitBand::NoChance
            ],
            "the ladder is not monotone in the ruleset's own tracking term"
        );
    }

    /// The arcs are livery; the band is arithmetic. Neither may leak into the
    /// other.
    ///
    /// `projectile_resolution` and `hit_chance_ppm` never read the shooter's
    /// `yaw_urad`, and `Order::Fire` is accepted at any bearing — the ruleset
    /// does not gate a shot on facing at all. So the band must be identical
    /// for a target dead ahead and the same target dead astern. A band that
    /// dimmed to NO CHANCE outside the drawn arc would look like the most
    /// natural thing in the world and would disagree with every shot the
    /// adjudicator lands.
    #[test]
    fn the_band_does_not_read_the_firing_arc() {
        let optimal = WeaponKind::Stock.weapon().optimal_mm;
        let ahead = view(0, optimal, 30_000);
        let mut astern = ahead;
        // Same geometry, shooter spun to face the other way.
        let mut me = Craft::spawned(Archetype::Interceptor, QPos { x: 0, y: 0, z: 0 }, 3_141_592);
        me.lock_target = Some(PersistId::new(2));
        me.lock_progress = LOCK_ACQUISITION_TICKS;
        astern.own = Some(CraftView::of(PersistId::new(1), &me));
        assert_eq!(
            ahead.hit_forecast(),
            astern.hit_forecast(),
            "the band moved when only the shooter's facing changed"
        );
        assert_eq!(ahead.hit_chance_ppm(), astern.hit_chance_ppm());
    }

    /// D43 clause (f) has no witnessed saturation flag yet (#447 F2). The
    /// ruleset's `saturating_mul` would pin an overflowing intermediate at its
    /// type maximum and go on to produce a number that looks like a chance;
    /// nothing downstream could tell. The skin refuses instead.
    #[test]
    fn saturating_inputs_get_no_band_rather_than_a_wrong_one() {
        let weapon = WeaponKind::Stock.weapon();
        // Positions and velocities that stay inside `i64` but whose cross
        // product does not stay inside `i128`.
        let far = QPos {
            x: i64::MAX,
            y: i64::MAX,
            z: 0,
        };
        let fast = QVel {
            x: i64::MAX,
            y: 0,
            z: i64::MAX,
        };
        assert_eq!(
            hit_chance_ppm(
                far,
                fast,
                3_000,
                QPos { x: 0, y: 0, z: 0 },
                QVel { x: 0, y: 0, z: 0 },
                weapon,
            ),
            None,
            "the ruleset would have saturated here and still returned a number"
        );

        // A separation small enough for `range_sq` to be computable, with
        // both terms of one cross component overflowing `i128` and then
        // cancelling to zero — the quietest possible way to be wrong, since
        // it reports a target tearing across the sky as having no angular
        // rate at all. The refusal here comes from the *range* term, which
        // overflows first at any separation big enough to overflow a cross
        // product; see `hit_chance_ppm`'s note on why the cross guard is
        // defence in depth rather than an independently reachable one.
        const NEAR: i64 = 13_000_000_000_000_000_000_u64 as i64;
        assert_eq!(
            hit_chance_ppm(
                QPos {
                    x: 0,
                    y: i64::MIN.wrapping_add(NEAR),
                    z: i64::MIN.wrapping_add(NEAR),
                },
                QVel {
                    x: 0,
                    y: i64::MAX - 1,
                    z: i64::MAX,
                },
                3_000,
                QPos {
                    x: 0,
                    y: i64::MIN,
                    z: i64::MIN,
                },
                QVel {
                    x: 0,
                    y: i64::MIN,
                    z: i64::MIN,
                },
                weapon,
            ),
            None,
            "a cross product that only saturates after cancellation is the \
             quietest way to be wrong here"
        );

        let mut me = Craft::spawned(Archetype::Interceptor, QPos { x: 0, y: 0, z: 0 }, 0);
        me.lock_target = Some(PersistId::new(2));
        me.lock_progress = LOCK_ACQUISITION_TICKS;
        let mut them = Craft::spawned(Archetype::Interceptor, far, 0);
        them.vel = fast;
        let saturating = CombatView {
            own: Some(CraftView::of(PersistId::new(1), &me)),
            lock: LockView::of(&me),
            target: Some(CraftView::of(PersistId::new(2), &them)),
        };
        // That target is also far out of reach, and the reach guard is the
        // honest answer there: it is a fact about the geometry that needs no
        // tracking arithmetic at all.
        assert_eq!(saturating.hit_forecast(), Some(HitBand::NoChance));

        // Inside reach, with only the *velocities* extreme, there is no
        // geometric fact to fall back on and the band must decline.
        let mut close = Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: weapon.optimal_mm,
                y: 0,
                z: 0,
            },
            0,
        );
        close.vel = fast;
        let unreadable = CombatView {
            own: Some(CraftView::of(PersistId::new(1), &me)),
            lock: LockView::of(&me),
            target: Some(CraftView::of(PersistId::new(2), &close)),
        };
        assert_eq!(unreadable.hit_forecast(), Some(HitBand::Unreadable));
        assert_eq!(HitBand::Unreadable.label(), "NO READ");
    }
}

/// The hit band, checked against the rules that adjudicate the shot.
///
/// Nothing in here asserts that the transcription in [`hit_chance_ppm`] is
/// faithful. It *drives the real `Regolith` ruleset* and reads back the
/// `ShotResolved` verdicts it emits, so a transcription that drifted from
/// `orrery_games`'s private `hit_chance_ppm` fails here rather than shipping
/// a HUD that teaches a false model.
#[cfg(test)]
mod ruleset_agreement {
    use super::*;
    use orrery_core::{Executor, QPos, QVel};
    use orrery_games::regolith::archetype::Archetype;
    use orrery_games::regolith::order::{LockBreakReason, Order, Outcome, ShotResult};
    use orrery_games::regolith::state::{Craft, RegolithState};
    use orrery_games::regolith::weapon::WeaponKind;
    use orrery_games::Regolith;
    use orrery_protocol::{Tick, UniverseSeed};

    const SHOOTER: PersistId = PersistId::new(1);
    const TARGET: PersistId = PersistId::new(2);
    /// Every scenario shoots from the origin, standing still, at optimal
    /// range, so the range term is zero and the tracking term is the only
    /// thing moving.
    const RANGE_MM: i64 = 300_000;
    const WEAPON: WeaponKind = WeaponKind::Stock;

    /// A target at `RANGE_MM` along `+X`, closing sideways at `transverse_mms`.
    fn target_at(transverse_mms: i64) -> Craft {
        let mut craft = Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: RANGE_MM,
                y: 0,
                z: 0,
            },
            0,
        );
        craft.vel = QVel {
            x: 0,
            y: 0,
            z: transverse_mms,
        };
        craft
    }

    fn target_radius_mm() -> i64 {
        Archetype::Interceptor.limits().radius_mm
    }

    /// The skin's forecast for exactly the geometry the ruleset will resolve.
    fn client_chance(target: &Craft) -> u32 {
        hit_chance_ppm(
            target.pos,
            target.vel,
            target_radius_mm(),
            QPos { x: 0, y: 0, z: 0 },
            QVel { x: 0, y: 0, z: 0 },
            WEAPON.weapon(),
        )
        .expect("this geometry is far inside the ruleset's saturation limits")
    }

    /// One real resolution. The target is installed with the exact state we
    /// chose and handed a `Damage` order with one flight tick left, which is
    /// the branch `projectile_resolution` rolls on.
    ///
    /// `tick_rng(seed, entity, tick)` is a function of those three alone, so
    /// holding `(seed, tick)` fixed holds the *draw* fixed while the geometry
    /// moves — which is what makes the sweep below a threshold measurement
    /// rather than a sampling exercise.
    fn resolve(seed_byte: u8, tick: u64, target: Craft) -> Vec<Outcome> {
        let game = Regolith::honest();
        let mut executor = Executor::new(game, UniverseSeed([seed_byte; 32]));
        executor.insert(TARGET, RegolithState::Craft(target));
        let order = Order::Damage {
            amount: 1,
            from: SHOOTER,
            from_pos: QPos { x: 0, y: 0, z: 0 },
            from_vel: QVel { x: 0, y: 0, z: 0 },
            from_weapon: WEAPON,
            flight_ticks: Some(1),
        };
        executor
            .step_entity(TARGET, Tick::new(tick), &[order])
            .expect("the target is installed")
            .events
    }

    fn verdict(seed_byte: u8, tick: u64, target: Craft) -> Option<ShotResult> {
        resolve(seed_byte, tick, target)
            .iter()
            .find_map(|event| match event {
                Outcome::ShotResolved { result, .. } => Some(*result),
                _ => None,
            })
    }

    /// The exterior boundary, upper end: `PERFECT` claims the ruleset cannot
    /// miss, and the ruleset must not miss.
    ///
    /// `uniform_below(rng, CHANCE_SCALE)` draws from `0..CHANCE_SCALE` and
    /// the test is `draw < chance`, so `chance == CHANCE_SCALE` is the only
    /// value where that is a theorem. Every draw here is a different rng
    /// stream.
    #[test]
    fn perfect_never_misses_in_the_ruleset() {
        let target = target_at(0);
        assert_eq!(
            client_chance(&target),
            CHANCE_SCALE,
            "a target with no transverse motion at optimal range is the ruleset's ceiling"
        );
        assert_eq!(
            HitBand::of_chance_ppm(client_chance(&target)),
            HitBand::Perfect
        );
        for seed in 0..64u8 {
            for tick in [1u64, 7, 5_000, 1_000_000] {
                assert_eq!(
                    verdict(seed, tick, target_at(0)),
                    Some(ShotResult::Hit),
                    "seed {seed} tick {tick}: PERFECT missed"
                );
            }
        }
    }

    /// The exterior boundary, lower end: `NO CHANCE` claims the ruleset
    /// cannot hit, and it must not.
    #[test]
    fn no_chance_never_hits_in_the_ruleset() {
        // Tracking ratio past 1e9 drives the penalty past CHANCE_SCALE^3.
        let target = target_at(1_000_000_000);
        assert_eq!(client_chance(&target), 0);
        assert_eq!(
            HitBand::of_chance_ppm(client_chance(&target)),
            HitBand::NoChance
        );
        for seed in 0..64u8 {
            for tick in [1u64, 7, 5_000, 1_000_000] {
                assert_eq!(
                    verdict(seed, tick, target_at(1_000_000_000)),
                    Some(ShotResult::Miss),
                    "seed {seed} tick {tick}: NO CHANCE hit"
                );
            }
        }
    }

    /// The ruleset's own draw for one `(seed, tick)` stream.
    ///
    /// `Executor::step_entity` seeds with `tick_rng(seed, entity, tick)` and
    /// nothing else (`orrery_core/src/executor.rs`), and `tick_rng` is
    /// public, so the stream is reproducible from outside the ruleset. The
    /// rejection loop is `uniform_below`'s, which is private — but a wrong
    /// reproduction cannot pass quietly: the biconditional below would break
    /// on the first stream, not silently agree.
    fn draw_for(seed_byte: u8, tick: u64) -> u32 {
        use rand_core::RngCore;
        let mut rng = orrery_core::tick_rng(UniverseSeed([seed_byte; 32]), TARGET, Tick::new(tick));
        let limit = u32::MAX - u32::MAX % CHANCE_SCALE;
        loop {
            let draw = rng.next_u32();
            if draw < limit {
                return draw % CHANCE_SCALE;
            }
        }
    }

    /// Transverse speeds chosen to put the skin's chance across the whole
    /// range, from just under the ceiling to nothing at all.
    const SWEEP_MMS: [i64; 8] = [
        3_828, 18_000, 35_351, 54_000, 82_486, 162_000, 378_000, 1_706_800,
    ];

    /// The measurement this whole issue turns on: **the ruleset's own hit
    /// chance, read off the ruleset, is the number the band is built from.**
    ///
    /// The ruleset hits exactly when `draw < chance`, so across many streams
    /// at one fixed geometry its chance is pinned to the interval between the
    /// largest draw that hit and the smallest draw that missed. That is a
    /// *measurement of `orrery_games`'s private `hit_chance_ppm`* through its
    /// verdicts, not a restatement of the transcription. With 16_384 streams
    /// the interval is around a hundred ppm wide, and the skin's number has
    /// to sit inside it at every geometry.
    #[test]
    fn the_rulesets_own_chance_is_measured_and_it_is_the_skins_number() {
        const STREAMS: u32 = 16_384;
        let streams: Vec<(u8, u64)> = (0..STREAMS)
            .map(|index| ((index % 256) as u8, 5 + u64::from(index / 256) * 1_000_003))
            .collect();

        let mut widest = 0u32;
        let mut chances = Vec::new();
        let mut near_certain_miss = None;
        for transverse in SWEEP_MMS {
            let chance = client_chance(&target_at(transverse));
            chances.push(chance);
            let mut highest_hit: Option<u32> = None;
            let mut lowest_miss: Option<u32> = None;
            for &(seed, tick) in &streams {
                let draw = draw_for(seed, tick);
                match verdict(seed, tick, target_at(transverse)) {
                    Some(ShotResult::Hit) => {
                        highest_hit = Some(highest_hit.map_or(draw, |best| best.max(draw)));
                    }
                    Some(ShotResult::Miss) => {
                        lowest_miss = Some(lowest_miss.map_or(draw, |best| best.min(draw)));
                        if chance >= 990_000 {
                            near_certain_miss = Some((chance, draw));
                        }
                    }
                    other => panic!("no verdict at {transverse} mm/s: {other:?}"),
                }
            }
            // Hit means `draw < ruleset_chance`; miss means `draw >= it`.
            let low = highest_hit.map_or(0, |draw| draw + 1);
            let high = lowest_miss.unwrap_or(CHANCE_SCALE);
            assert!(
                low <= high,
                "{transverse} mm/s: the verdicts are not a threshold at all"
            );
            assert!(
                (low..=high).contains(&chance),
                "{transverse} mm/s: the ruleset's own chance is in {low}..={high} ppm, \
                 the skin says {chance} ppm"
            );
            widest = widest.max(high - low);
        }

        assert!(
            widest <= 5_000,
            "the measurement is only good to {widest} ppm; raise STREAMS"
        );
        assert!(
            chances.iter().any(|chance| *chance > 990_000)
                && chances.iter().any(|chance| *chance < 10_000),
            "the sweep did not span the range: {chances:?}"
        );

        // The upper boundary, earning its place. `PERFECT` is reserved for
        // exactly `CHANCE_SCALE` because that is the only value at which
        // "cannot miss" is a theorem — the roll is `draw < chance` over
        // `draw ∈ 0..CHANCE_SCALE`. Here is the ruleset missing a shot the
        // skin rates at better than 99%: a band that rounded that up to
        // PERFECT would have been caught by this miss.
        let (chance, draw) = near_certain_miss
            .expect("no stream drew high enough to show a near-certain shot missing");
        assert!(
            draw >= chance && chance >= 990_000,
            "draw {draw} against chance {chance}"
        );
    }

    /// The same verdicts, one at a time rather than in aggregate: every
    /// single resolution the ruleset produced is exactly the one
    /// `draw < skin_chance` predicts. A transcription that agreed on average
    /// and disagreed in detail fails here.
    #[test]
    fn every_single_verdict_is_the_one_the_skins_number_predicts() {
        let mut hits = 0u32;
        let mut misses = 0u32;
        for seed in 0..=255u8 {
            let tick = 11 + u64::from(seed) * 4_099;
            let draw = draw_for(seed, tick);
            for transverse in SWEEP_MMS {
                let chance = client_chance(&target_at(transverse));
                let expected = if draw < chance {
                    hits += 1;
                    ShotResult::Hit
                } else {
                    misses += 1;
                    ShotResult::Miss
                };
                assert_eq!(
                    verdict(seed, tick, target_at(transverse)),
                    Some(expected),
                    "seed {seed} tick {tick}: draw {draw} against {chance} ppm \
                     at {transverse} mm/s"
                );
            }
        }
        assert!(
            hits > 100 && misses > 100,
            "the sweep must exercise both verdicts: {hits} hits, {misses} misses"
        );
    }

    /// And in aggregate: over many rng streams at a fixed geometry, the
    /// ruleset lands the shot about as often as the skin's number says.
    #[test]
    fn the_observed_hit_rate_matches_the_skins_number() {
        // A geometry near the middle of the range, so the check has room to
        // fail in both directions.
        let mut transverse = 0i64;
        while client_chance(&target_at(transverse)) > CHANCE_SCALE / 2 {
            transverse += 250;
            assert!(transverse < 10_000_000, "never crossed half");
        }
        let chance = client_chance(&target_at(transverse));
        assert!(
            (CHANCE_SCALE / 4..=CHANCE_SCALE / 2).contains(&chance),
            "wanted a mid-range geometry, got {chance} ppm"
        );
        let mut hits = 0u32;
        let mut total = 0u32;
        for seed in 0..64u8 {
            for tick in 0..16u64 {
                total += 1;
                if verdict(seed, tick, target_at(transverse)) == Some(ShotResult::Hit) {
                    hits += 1;
                }
            }
        }
        let observed = f64::from(hits) / f64::from(total);
        let predicted = f64::from(chance) / f64::from(CHANCE_SCALE);
        assert!(
            (observed - predicted).abs() < 0.08,
            "{total} shots landed {observed:.3} of the time against a predicted {predicted:.3}"
        );
    }

    /// The reach boundary, which the *weapon envelope* alone gets wrong.
    ///
    /// `projectile_resolution` breaks the lock past
    /// `optimal + falloff + target_radius_mm`. [`RangeBand::of`] stops at
    /// `optimal + falloff`, so between the two the envelope says "beyond
    /// reach" while the ruleset is still rolling. [`reach_mm`] is the
    /// resolver's number and is the one the band uses.
    #[test]
    fn the_band_breaks_where_the_resolver_breaks_and_not_before() {
        let weapon = WEAPON.weapon();
        let radius = target_radius_mm();
        let envelope_edge = weapon.optimal_mm + weapon.falloff_mm;
        let reach = reach_mm(weapon, radius);
        assert_eq!(reach, envelope_edge + radius, "reach carries the signature");

        let at = |range_mm: i64| {
            let mut craft = Craft::spawned(
                Archetype::Interceptor,
                QPos {
                    x: range_mm,
                    y: 0,
                    z: 0,
                },
                0,
            );
            craft.vel = QVel { x: 0, y: 0, z: 0 };
            craft
        };

        // Inside the resolver's reach but outside the weapon envelope: the
        // ruleset still rolls, so the band must not say NO CHANCE here.
        let events = resolve(9, 11, at(envelope_edge + 1));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Outcome::ShotResolved { .. })),
            "the resolver still rolls one radius past the envelope"
        );
        assert_eq!(
            RangeBand::of(envelope_edge + 1, weapon),
            RangeBand::Beyond,
            "the envelope, on its own, would have called this unreachable"
        );

        // Exactly at reach: still rolled.
        assert!(resolve(9, 11, at(reach))
            .iter()
            .any(|event| matches!(event, Outcome::ShotResolved { .. })));

        // One millimetre past: no roll at all, the lock breaks.
        let past = resolve(9, 11, at(reach + 1));
        assert!(
            !past
                .iter()
                .any(|event| matches!(event, Outcome::ShotResolved { .. })),
            "past reach the ruleset never rolls"
        );
        assert!(past.iter().any(|event| matches!(
            event,
            Outcome::LockBroken {
                reason: LockBreakReason::RangeExceeded,
                ..
            }
        )));
    }
}
