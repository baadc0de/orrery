//! Integer stage-1 checks for Regolith's published chassis and weapon tables.
use super::{
    state::{Craft, PITCH_LIMIT_URAD, TAU_URAD},
    DRAG_PER_SEC_PER_MILLE,
};
use orrery_core::invariants::checks;
use orrery_core::{Invariant, InvariantKind, InvariantSample, InvariantViolation, QVel, TICK_HZ};

const VEL_MARGIN_MMS: i64 = 100;
const POS_MARGIN_MM: i64 = 100;
const TICKS_PER_SEC: i64 = TICK_HZ as i64;
/// Every published Regolith stage-1 validator.
pub const INVARIANTS: &[Invariant<Craft>] = &[
    Invariant {
        name: "regolith/speed-cap",
        check: speed_cap,
    },
    Invariant {
        name: "regolith/acceleration-cap",
        check: acceleration_cap,
    },
    Invariant {
        name: "regolith/teleport",
        check: teleport,
    },
    Invariant {
        name: "regolith/fire-rate",
        check: fire_rate,
    },
    Invariant {
        name: "regolith/value-range",
        check: value_range,
    },
];
fn speed_cap(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let limit = sample.current.archetype.limits().max_speed_mms + VEL_MARGIN_MMS;
    if sample.current.vel.difference_squared(QVel::default())
        > i128::from(limit) * i128::from(limit)
    {
        Err(InvariantViolation::new(
            InvariantKind::SpeedCap,
            "regolith/speed-cap",
        ))
    } else {
        Ok(())
    }
}
fn acceleration_cap(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let limits = sample.current.archetype.limits();
    let per_tick = limits.max_accel_mmss / TICKS_PER_SEC
        + limits.max_speed_mms * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC)
        + VEL_MARGIN_MMS;
    if checks::exceeds_acceleration(
        previous.vel,
        sample.current.vel,
        sample.elapsed_ticks,
        per_tick,
    ) {
        Err(InvariantViolation::new(
            InvariantKind::AccelerationCap,
            "regolith/acceleration-cap",
        ))
    } else {
        Ok(())
    }
}
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
        Err(InvariantViolation::new(
            InvariantKind::Teleport,
            "regolith/teleport",
        ))
    } else {
        Ok(())
    }
}
fn fire_rate(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let fired = sample.current.shots.saturating_sub(previous.shots);
    let cooldown = u32::from(sample.current.weapon.weapon().cooldown_ticks).max(1);
    if fired > sample.elapsed_ticks / cooldown + 1 {
        Err(InvariantViolation::new(
            InvariantKind::RateLimit,
            "regolith/fire-rate",
        ))
    } else {
        Ok(())
    }
}
fn value_range(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    const NAME: &str = "regolith/value-range";
    let craft = sample.current;
    let limits = craft.archetype.limits();
    let weapon = craft.weapon.weapon();
    if craft.hull < 0
        || craft.hull > limits.max_hull
        || craft.shield < 0
        || craft.shield > limits.max_shield
        || craft.cooldown > weapon.cooldown_ticks
        || craft.yaw_urad < 0
        || craft.yaw_urad >= TAU_URAD
        || craft.pitch_urad.abs() > PITCH_LIMIT_URAD
    {
        return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
    }
    if let Some(previous) = sample.previous {
        if craft.archetype != previous.archetype
            || craft.weapon != previous.weapon
            || craft.shots < previous.shots
            || craft.damage_dealt < previous.damage_dealt
        {
            return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
        }
    }
    Ok(())
}
