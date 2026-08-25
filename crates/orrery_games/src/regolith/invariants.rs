//! Integer stage-1 checks for Regolith's craft and rock tables.
use super::{
    state::{RegolithState, TAU_URAD},
    weapon::WeaponKind,
    BLOOM_CADENCE_TICKS, BLOOM_CENTRAL_RADIUS_MM, BLOOM_MAX_LIVE_ROCKS, DRAG_PER_SEC_PER_MILLE,
    ISLAND_CRAFT_BUDGET, ISLAND_PICKUP_BUDGET, ISLAND_ROCK_BUDGET, LOCK_ACQUISITION_TICKS,
    RESPAWN_TICKS,
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
        name: "regolith/score-rate",
        check: score_rate,
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
        RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => 0,
    } + VEL_MARGIN_MMS;
    let vel = match sample.current {
        RegolithState::Craft(craft) => craft.vel,
        RegolithState::Rock(rock) => rock.vel,
        RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => QVel::default(),
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
    if previous.hull == 0 && current.hull > 0 {
        return Ok(());
    }
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
        (RegolithState::Craft(previous), RegolithState::Craft(current)) => {
            if previous.hull == 0 && current.hull > 0 {
                return Ok(());
            }
            (
                previous.pos,
                current.pos,
                current.archetype.limits().max_speed_mms,
            )
        }
        (RegolithState::Rock(previous), RegolithState::Rock(current)) => (
            previous.pos,
            current.pos,
            current.tier.limits().max_speed_mms,
        ),
        (RegolithState::Pickup(previous), RegolithState::Pickup(current)) => {
            (previous.pos, current.pos, 0)
        }
        (RegolithState::BloomDirector(_), RegolithState::BloomDirector(_)) => {
            return Ok(());
        }
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
fn score_rate(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let (Some(RegolithState::Craft(previous)), RegolithState::Craft(current)) =
        (sample.previous, sample.current)
    else {
        return Ok(());
    };
    let ticks = u64::from(sample.elapsed_ticks);
    let kills = u64::from(current.kills.saturating_sub(previous.kills));
    let pickups = u64::from(current.pickups_won.saturating_sub(previous.pickups_won));
    let rock_points = current
        .score_rock_points
        .saturating_sub(previous.score_rock_points);
    let max_rock_points = ticks
        .saturating_mul(u64::from(ISLAND_ROCK_BUDGET))
        .saturating_mul(4);
    if kills > ticks.saturating_mul(u64::from(ISLAND_CRAFT_BUDGET))
        || pickups > ticks.saturating_mul(u64::from(ISLAND_PICKUP_BUDGET))
        || rock_points > max_rock_points
    {
        Err(InvariantViolation::new(
            InvariantKind::RateLimit,
            "regolith/score-rate",
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
                || craft.respawn_in > RESPAWN_TICKS
                || craft.lock_progress > LOCK_ACQUISITION_TICKS
                || craft.lock_decay_progress >= LOCK_ACQUISITION_TICKS
                || craft.cover_claim_cooldown > super::COVER_CLAIM_INTERVAL_TICKS
                || (craft.lock_decay_progress > 0
                    && (craft.lock_target.is_none()
                        || craft.lock_progress != LOCK_ACQUISITION_TICKS))
                || (craft.lock_target.is_none() != (craft.lock_progress == 0))
                || (craft.lock_target.is_none() && craft.lock_class.is_some())
                || (craft.hull > 0 && craft.respawn_in != 0)
                || (craft.hull == 0 && craft.respawn_in == 0)
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
            }
        }
        RegolithState::Rock(rock) => {
            if rock.hull < 0
                || rock.hull > rock.tier.limits().max_hull
                || rock.born_in_bloom != rock.bloom.is_some()
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
            }
        }
        RegolithState::Pickup(pickup) => {
            if pickup.ttl_remaining > pickup.expires_at
                || pickup.claimed_by.is_some() != pickup.claimed_at.is_some()
                || (pickup.expired && pickup.claimed_by.is_some())
                || (pickup.expired && pickup.ttl_remaining != 0)
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
            }
        }
        RegolithState::BloomDirector(director) => {
            let site_options_agree =
                director.site_pos.is_some() == director.site_active_until.is_some();
            let site_count_agrees = director.site_pos.is_some() == (director.site_rocks_alive > 0);
            let site_is_central = director.site_pos.is_none_or(|pos| {
                pos.y == 0
                    && pos.x.abs() <= BLOOM_CENTRAL_RADIUS_MM
                    && pos.z.abs() <= BLOOM_CENTRAL_RADIUS_MM
            });
            if director.next_bloom_tick <= director.clock_tick
                || director.next_bloom_tick % BLOOM_CADENCE_TICKS != 0
                || director.site_rocks_alive > BLOOM_MAX_LIVE_ROCKS
                || !site_options_agree
                || !site_count_agrees
                || !site_is_central
                || director
                    .site_active_until
                    .is_some_and(|until| until <= director.clock_tick)
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME));
            }
        }
    }
    if let Some(previous) = sample.previous {
        match (previous, sample.current) {
            (RegolithState::Craft(previous), RegolithState::Craft(current))
                if current.archetype != previous.archetype
                    || (current.weapon != previous.weapon
                        && !(current.pickups_won > previous.pickups_won
                            || (current.weapon == WeaponKind::Stock
                                && previous.hull == 0
                                && current.hull > 0)))
                    || current.shots < previous.shots
                    || current.damage_dealt < previous.damage_dealt
                    || current.grabs_attempted < previous.grabs_attempted
                    || current.pickups_won < previous.pickups_won
                    || current.grabs_lost < previous.grabs_lost
                    || current.score_rock_points < previous.score_rock_points
                    || current.kills < previous.kills =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            (RegolithState::Rock(previous), RegolithState::Rock(current))
                if current.tier != previous.tier
                    || current.generation != previous.generation
                    || current.splits_done < previous.splits_done
                    || current.born_in_bloom != previous.born_in_bloom
                    || current.pickups_dropped < previous.pickups_dropped
                    || current.bloom != previous.bloom =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            (RegolithState::Pickup(previous), RegolithState::Pickup(current))
                if current.pos != previous.pos
                    || current.kind != previous.kind
                    || current.expires_at != previous.expires_at
                    || current.ttl_remaining > previous.ttl_remaining
                    || previous.claimed_by.is_some()
                        && current.claimed_by != previous.claimed_by
                    || previous.claimed_at.is_some()
                        && current.claimed_at != previous.claimed_at
                    || previous.expired && !current.expired =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            (RegolithState::BloomDirector(previous), RegolithState::BloomDirector(current))
                if current.clock_tick < previous.clock_tick
                    || current.blooms_seeded < previous.blooms_seeded
                    || current.next_bloom_tick < previous.next_bloom_tick =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            (previous, current)
                if core::mem::discriminant(previous) != core::mem::discriminant(current) =>
            {
                return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
            }
            _ => {}
        }
    }
    Ok(())
}
