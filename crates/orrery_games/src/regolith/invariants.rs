//! Integer stage-1 checks for Regolith's craft and rock tables.
use super::{
    state::{RegolithState, TAU_URAD},
    DRAG_PER_SEC_PER_MILLE,
};
use orrery_core::invariants::checks;
use orrery_core::{Invariant, InvariantKind, InvariantSample, InvariantViolation, QVel, TICK_HZ};

const VEL_MARGIN_MMS: i64 = 100;
const POS_MARGIN_MM: i64 = 100;
const TICKS_PER_SEC: i64 = TICK_HZ as i64;
/// Every published Regolith stage-1 validator.
pub const INVARIANTS: &[Invariant<RegolithState>] = &[
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
fn speed_cap(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let limit = match sample.current {
        RegolithState::Craft(craft) => craft.archetype.limits().max_speed_mms,
        RegolithState::Rock(rock) => rock.tier.limits().max_speed_mms,
    } + VEL_MARGIN_MMS;
    let vel = match sample.current {
        RegolithState::Craft(craft) => craft.vel,
        RegolithState::Rock(rock) => rock.vel,
    };
    if vel.difference_squared(QVel::default()) > i128::from(limit) * i128::from(limit) {
        Err(InvariantViolation::new(
            InvariantKind::SpeedCap,
            "regolith/speed-cap",
        ))
    } else {
        Ok(())
    }
}
fn acceleration_cap(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let (Some(RegolithState::Craft(previous)), RegolithState::Craft(current)) =
        (sample.previous, sample.current)
    else {
        return Ok(());
    };
    let limits = current.archetype.limits();
    let per_tick = limits.max_accel_mmss / TICKS_PER_SEC
        + limits.max_speed_mms * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC)
        + VEL_MARGIN_MMS;
    if checks::exceeds_acceleration(previous.vel, current.vel, sample.elapsed_ticks, per_tick) {
        Err(InvariantViolation::new(
            InvariantKind::AccelerationCap,
            "regolith/acceleration-cap",
        ))
    } else {
        Ok(())
    }
}
fn teleport(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let (previous_pos, current_pos, cap) = match (previous, sample.current) {
        (RegolithState::Craft(previous), RegolithState::Craft(current)) => (
            previous.pos,
            current.pos,
            current.archetype.limits().max_speed_mms,
        ),
        (RegolithState::Rock(previous), RegolithState::Rock(current)) => (
            previous.pos,
            current.pos,
            current.tier.limits().max_speed_mms,
        ),
        _ => {
            return Err(InvariantViolation::new(
                InvariantKind::ValueRange,
                "regolith/value-range",
            ))
        }
    };
    if checks::exceeds_speed(
        previous_pos,
        current_pos,
        sample.elapsed_ticks,
        cap / TICKS_PER_SEC + POS_MARGIN_MM,
    ) {
        Err(InvariantViolation::new(
            InvariantKind::Teleport,
            "regolith/teleport",
        ))
    } else {
        Ok(())
    }
}
fn fire_rate(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let (Some(RegolithState::Craft(previous)), RegolithState::Craft(current)) =
        (sample.previous, sample.current)
    else {
        return Ok(());
    };
    let fired = current.shots.saturating_sub(previous.shots);
    let cooldown = u32::from(current.weapon.weapon().cooldown_ticks).max(1);
    if fired > sample.elapsed_ticks / cooldown + 1 {
        Err(InvariantViolation::new(
            InvariantKind::RateLimit,
            "regolith/fire-rate",
        ))
    } else {
        Ok(())
    }
}
fn value_range(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    const NAME: &str = "regolith/value-range";
    match sample.current {
        RegolithState::Craft(craft) => {
            let limits = craft.archetype.limits();
            let weapon = craft.weapon.weapon();
            if craft.hull < 0
                || craft.hull > limits.max_hull
                || craft.shield < 0
                || craft.shield > limits.max_shield
                || craft.cooldown > weapon.cooldown_ticks
                || craft.yaw_urad < 0
                || craft.yaw_urad >= TAU_URAD
                || craft.pitch_urad != 0
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
            }
        }
        RegolithState::Rock(rock) => {
            if rock.hull < 0 || rock.hull > rock.tier.limits().max_hull {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
            }
        }
    }
    if let Some(previous) = sample.previous {
        match (previous, sample.current) {
            (RegolithState::Craft(previous), RegolithState::Craft(current))
                if current.archetype != previous.archetype
                    || current.weapon != previous.weapon
                    || current.shots < previous.shots
                    || current.damage_dealt < previous.damage_dealt =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            (RegolithState::Rock(previous), RegolithState::Rock(current))
                if current.tier != previous.tier
                    || current.generation != previous.generation
                    || current.splits_done < previous.splits_done =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            (RegolithState::Craft(_), RegolithState::Rock(_))
            | (RegolithState::Rock(_), RegolithState::Craft(_)) => {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            _ => {}
        }
    }
    Ok(())
}
