//! Integer stage-1 checks for Regolith's craft and rock tables.
use super::{
    state::{Craft, CraftSection, RegolithState, Rock, RockSection, PITCH_LIMIT_URAD, TAU_URAD},
    weapon::WeaponKind,
    BLOOM_CADENCE_TICKS, BLOOM_CENTRAL_RADIUS_MM, BLOOM_MAX_LIVE_ROCKS, DRAG_PER_SEC_PER_MILLE,
    ISLAND_BOUNDARY_MM, ISLAND_CRAFT_BUDGET, ISLAND_PICKUP_BUDGET, ISLAND_ROCK_BUDGET,
    LOCK_ACQUISITION_TICKS, RESPAWN_TICKS, TETHER_DRAG_PER_SEC_PER_MILLE,
};
use orrery_core::invariants::checks;
use orrery_core::{
    section_invariant, Invariant, InvariantKind, InvariantSample, InvariantViolation, QVel, TICK_HZ,
};

const VEL_MARGIN_MMS: i64 = 100;
const POS_MARGIN_MM: i64 = 100;
const TICKS_PER_SEC: i64 = TICK_HZ as i64;
/// Every published Regolith stage-1 validator.
pub const INVARIANTS: &[Invariant<RegolithState>] = &[
    // Four of the six checks name the section they are about in their own
    // signature (#791). `section_invariant!` lifts each to the whole-state
    // `Invariant` this slice publishes, and an entity in another section
    // passes without the check running — which is what the discarding arms of
    // the hand-written matches did, written once in `orrery_core` instead of
    // once per check.
    //
    // The speed cap is registered twice because it is genuinely about two
    // sections: crafts and rocks both move under a published ceiling. It is
    // one body, generic over `SpeedLimited`, and neither instantiation can see
    // a pickup or a director at all.
    section_invariant!("regolith/speed-cap", CraftSection, speed_cap::<Craft>),
    section_invariant!("regolith/speed-cap", RockSection, speed_cap::<Rock>),
    section_invariant!("regolith/acceleration-cap", CraftSection, acceleration_cap),
    // Teleport and value-range stay whole-state, and the reason is the same
    // for both: they ask about a *pair* of samples spanning two sections. A
    // craft that arrives where a rock was is the discriminant mismatch each of
    // them reports, and no per-section signature can hold a question about the
    // section changing. See `InvariantSample::project`.
    Invariant {
        name: "regolith/teleport",
        check: teleport,
    },
    section_invariant!("regolith/fire-rate", CraftSection, fire_rate),
    section_invariant!("regolith/score-rate", CraftSection, score_rate),
    Invariant {
        name: "regolith/value-range",
        check: value_range,
    },
];
/// A section whose values move under a published velocity ceiling.
///
/// Implemented by craft and rock and by nothing else, which is the whole
/// content of the two arms `speed_cap` used to carry only to yield a zero the
/// comparison then discarded: pickups and bloom directors do not move, so they
/// have no ceiling to name, and now there is nowhere to write one.
trait SpeedLimited {
    /// Lattice velocity.
    fn vel(&self) -> QVel;
    /// The resolver-owned velocity ceiling, in millimetres per second.
    fn max_speed_mms(&self) -> i64;
}

impl SpeedLimited for Craft {
    fn vel(&self) -> QVel {
        self.vel
    }

    fn max_speed_mms(&self) -> i64 {
        self.archetype.limits().max_speed_mms
    }
}

impl SpeedLimited for Rock {
    fn vel(&self) -> QVel {
        self.vel
    }

    fn max_speed_mms(&self) -> i64 {
        self.tier.limits().max_speed_mms
    }
}

fn speed_cap<S: SpeedLimited>(sample: &InvariantSample<'_, S>) -> Result<(), InvariantViolation> {
    let limit = sample.current.max_speed_mms() + VEL_MARGIN_MMS;
    if sample.current.vel().difference_squared(QVel::default())
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
    // The one remaining `else` is the documented no-history case, not a
    // section discard: `InvariantSample::previous` is `None` for the first
    // sample of an entity this peer has just met, and a check that cannot
    // decide without history must pass rather than accuse.
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let current = sample.current;
    if previous.hull == 0 && current.hull > 0 {
        return Ok(());
    }
    let limits = current.archetype.limits();
    let mut per_tick = limits.max_accel_mmss / TICKS_PER_SEC
        + limits.max_speed_mms * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC)
        + VEL_MARGIN_MMS;
    // The tether (#955) is a second drag, and a craft outside the island can
    // shed velocity faster than thrust and ordinary drag together explain. The
    // allowance is granted *only* where the tether can act, because this bound
    // is a cheat-detection surface: widening it everywhere would buy the
    // anchor at the price of every impossible acceleration inside the island,
    // which is where all the play is. `previous` is the position the tether
    // read on the tick that produced `current`, so it is the honest witness to
    // whether the tether could have been running.
    if outside_the_island(previous.pos) {
        per_tick += limits.max_speed_mms * TETHER_DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC);
    }
    if checks::exceeds_acceleration(previous.vel, current.vel, sample.elapsed_ticks, per_tick) {
        Err(InvariantViolation::new(
            InvariantKind::AccelerationCap,
            "regolith/acceleration-cap",
        ))
    } else {
        Ok(())
    }
}
/// Whether a craft is outside the square island edge on any axis.
///
/// The same per-axis box test [`super::craft::apply_tether`] applies, and
/// deliberately the same shape as the rock reflection at
/// [`ISLAND_BOUNDARY_MM`]: a radius here would be a second, disagreeing
/// definition of one boundary.
fn outside_the_island(pos: orrery_core::QPos) -> bool {
    pos.x.unsigned_abs() > ISLAND_BOUNDARY_MM as u64
        || pos.y.unsigned_abs() > ISLAND_BOUNDARY_MM as u64
        || pos.z.unsigned_abs() > ISLAND_BOUNDARY_MM as u64
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
fn fire_rate(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let current = sample.current;
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
fn score_rate(sample: &InvariantSample<'_, Craft>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    let current = sample.current;
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
                || craft.pitch_urad.abs() > PITCH_LIMIT_URAD
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
