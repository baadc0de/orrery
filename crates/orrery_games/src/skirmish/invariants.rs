//! The stage-1 checks (D10 stage 1, docs/06 §3, docs/11-roadmap.md §P4).
//!
//! P4's deliverable list calls for "continuous cheap checks: speed/acceleration
//! caps, teleport detection, rate limits, impossible values". This is that
//! list, and it is the first place in the tree where
//! [`Ruleset::invariants`](orrery_core::Ruleset::invariants) returns anything —
//! the core defines the seam and the witness runs it on every received sample,
//! but until a game supplied validators, every peer was evaluating an empty
//! slice.
//!
//! # Every check is integer, and every check divides by `elapsed_ticks`
//!
//! Two disciplines, both there to keep honest players out of the strike
//! pipeline (D17 risk 3):
//!
//! - **No floats.** Squared magnitudes on the millimetre lattice, compared as
//!   `i128` — mostly by way of [`orrery_core::invariants::checks`], which
//!   exists so games do not each re-derive them. A check that reached for
//!   `sqrt` would be asking the tolerance question at the wrong stage, on a
//!   machine that need not agree with the one that produced the sample.
//! - **No assumed adjacency.** Samples arrive at the replication rate (20 Hz,
//!   so three ticks apart) and under loss the gap widens without bound. Every
//!   rate-derived limit is computed from
//!   [`InvariantSample::elapsed_ticks`], and a check with no previous sample
//!   passes rather than accuses — "I only just met this entity" is not
//!   evidence.
//!
//! # The limits are derived from the rules, not guessed at
//!
//! The acceleration check is the one that punishes a guess. "Velocity may
//! change by `a_max · dt` per tick" is the obvious limit and it is **wrong for
//! these rules**: a craft at its ceiling also sheds drag every tick, and the
//! speed clamp rewrites the velocity vector after the thrust goes in. Written
//! the obvious way, this check fires on honest play within ten seconds — which
//! is how it was found, by `honest_play_raises_no_stage_one_flag` on the first
//! run of the battery, rather than by a player being accused of cheating.
//! [`max_delta_v_per_tick`] therefore reads the same drag constant the rules
//! use, and a change to one that is not made to the other shows up as a
//! failing test rather than as a report.
//!
//! On top of the derived limit each check carries a fixed margin of ten D16
//! bands (ε_pos 1 cm, ε_vel 1 cm/s — so 10 cm and 10 cm/s), absorbing the
//! quantization of two samples and the residue of the clamp/drag interaction.
//! That is generous by design: these are cheap filters whose false-positive
//! rate must be zero, precision is replay's job, and the 1.5× the demo
//! criterion names still clears every margin here several times over.
//!
//! # Why the hard-jump check is not used
//!
//! [`checks::is_teleport`] takes no time term, which is what makes it immune
//! to a cheat riding a long sample gap. It is still not here: this game's
//! samples arrive over a lossy link with an unbounded gap, so any fixed jump
//! cap is either smaller than a legitimate 2-second gap's travel — a false
//! positive on every packet-loss burst — or larger, and inert. The
//! displacement check below carries the time term instead, and replay carries
//! the precision.

use orrery_core::invariants::checks;
use orrery_core::{Invariant, InvariantKind, InvariantSample, InvariantViolation, QVel, TICK_HZ};

use super::archetype::Limits;
use super::state::{Craft, PITCH_LIMIT_URAD, TAU_URAD};
use super::DRAG_PER_SEC_PER_MILLE;

/// Slack on velocity-derived limits: ten D16 velocity bands, per tick.
const VEL_MARGIN_MMS: i64 = 100;

/// Slack on position-derived limits: ten D16 position bands, per tick.
const POS_MARGIN_MM: i64 = 100;

/// The fixed tick rate, as the divisor turning per-second limits into
/// per-tick ones.
const TICKS_PER_SEC: i64 = TICK_HZ as i64;

/// Every stage-1 check this game publishes.
pub const INVARIANTS: &[Invariant<Craft>] = &[
    Invariant {
        name: "skirmish/speed-cap",
        check: speed_cap,
    },
    Invariant {
        name: "skirmish/acceleration-cap",
        check: acceleration_cap,
    },
    Invariant {
        name: "skirmish/teleport",
        check: teleport,
    },
    Invariant {
        name: "skirmish/fire-rate",
        check: fire_rate,
    },
    Invariant {
        name: "skirmish/value-range",
        check: value_range,
    },
];

/// The velocity field never exceeds the archetype's ceiling.
///
/// Needs no history at all: the rules clamp speed every tick and drag only
/// ever takes it further under the ceiling, so a single honest sample is
/// already checkable. That makes this the one check that fires on the first
/// sample of an entity a peer has just met.
fn speed_cap(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let limit = sample.current.archetype.limits().max_speed_mms + VEL_MARGIN_MMS;
    if sample.current.vel.difference_squared(QVel::default())
        > i128::from(limit) * i128::from(limit)
    {
        return Err(InvariantViolation::new(
            InvariantKind::SpeedCap,
            "skirmish/speed-cap",
        ));
    }
    Ok(())
}

/// The most one tick of these rules can change a velocity.
///
/// Two terms, because two things move a velocity: a full tick of thrust, and
/// the drag a craft at the ceiling sheds in the same tick. The speed clamp
/// adds nothing — it can only ever pull a velocity back toward the one before
/// it — but it is why the thrust term is a bound rather than an equality.
#[must_use]
fn max_delta_v_per_tick(limits: &Limits) -> i64 {
    let thrust = limits.max_accel_mmss / TICKS_PER_SEC;
    let drag = limits.max_speed_mms * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC);
    thrust + drag + VEL_MARGIN_MMS
}

/// Velocity did not change faster than the archetype can push it.
fn acceleration_cap(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let per_tick = max_delta_v_per_tick(&sample.current.archetype.limits());
    if checks::exceeds_acceleration(
        previous.vel,
        sample.current.vel,
        sample.elapsed_ticks,
        per_tick,
    ) {
        return Err(InvariantViolation::new(
            InvariantKind::AccelerationCap,
            "skirmish/acceleration-cap",
        ));
    }
    Ok(())
}

/// Displacement is reachable at the archetype's top speed.
///
/// Distinct from the speed cap rather than redundant with it: this reads the
/// *position* field, so a craft that jumps without ever holding an illegal
/// velocity — the classic teleport — is caught here and nowhere else.
fn teleport(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let per_tick = sample.current.archetype.limits().max_speed_mms / TICKS_PER_SEC + POS_MARGIN_MM;
    if checks::exceeds_speed(
        previous.pos,
        sample.current.pos,
        sample.elapsed_ticks,
        per_tick,
    ) {
        return Err(InvariantViolation::new(
            InvariantKind::Teleport,
            "skirmish/teleport",
        ));
    }
    Ok(())
}

/// No more shots than the cooldown allows over the elapsed time.
///
/// The `+ 1` is the boundary shot: a craft whose weapon came ready in the tick
/// before the previous sample fires immediately after it, which is legal and
/// would otherwise read as one shot too many on samples that happen to
/// straddle a cooldown.
fn fire_rate(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let fired = sample.current.shots.saturating_sub(previous.shots);
    let cooldown = u32::from(sample.current.archetype.limits().cooldown_ticks).max(1);
    if fired > sample.elapsed_ticks / cooldown + 1 {
        return Err(InvariantViolation::new(
            InvariantKind::RateLimit,
            "skirmish/fire-rate",
        ));
    }
    Ok(())
}

/// Fields inside their legal ranges, and the monotone ones still monotone.
///
/// The archetype comparison belongs here for a reason worth stating: a craft
/// that re-declared itself a cruiser would be handing every *other* check a
/// limit table its authority never ran, so that cheat would arrive as four
/// checks quietly passing rather than as one failing.
fn value_range(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    const NAME: &str = "skirmish/value-range";
    let craft = sample.current;
    let limits = craft.archetype.limits();

    if craft.hull < 0
        || craft.hull > limits.max_hull
        || craft.shield < 0
        || craft.shield > limits.max_shield
        || craft.cooldown > limits.cooldown_ticks
        || craft.yaw_urad < 0
        || craft.yaw_urad >= TAU_URAD
        || craft.pitch_urad.abs() > PITCH_LIMIT_URAD
    {
        return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
    }

    if let Some(previous) = sample.previous {
        if craft.archetype != previous.archetype
            || craft.shots < previous.shots
            || craft.damage_dealt < previous.damage_dealt
        {
            return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
        }
    }
    Ok(())
}
