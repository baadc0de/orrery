//! The `regolith.world` module's canonical systems.
//!
//! This module owns the `Rock`, `Pickup` and `BloomDirector` sections of
//! `RegolithState`. It receives craft-originated requests only through the
//! ordered next-tick `Order` channel and emits typed `Outcome`s for the same
//! channel to compose on the following tick.
//!
//! Each rule is a named function over the one component it touches. A rock's
//! systems never see a pickup and a pickup's never see a director: the
//! projection in `craft_system!`'s sibling macros is the selection an ECS
//! query would do, done per entity, with no population scanned and therefore
//! no unrecorded read possible.

use orrery_core::System;
use rand_core::RngCore;

use super::state::{BloomDirector, LockClass, Pickup, Rock};
use super::{
    bloom_spec, draw_bloom_site, flagged_add, flagged_neg, order::LockBreakReason, pickup_id,
    projectile_resolution, split_children, uniform_percent, velocity_within_limit, weapon,
    within_grab_reach, Cx, Order, Outcome, ProjectileResolution, Regolith, RegolithLocals,
    ShotResult, BLOOM_CADENCE_TICKS, BLOOM_LIFETIME_TICKS, BLOOM_MAX_LIVE_ROCKS, BLOOM_ROCK_COUNT,
    ISLAND_BOUNDARY_MM, PICKUP_TTL_TICKS,
};
use orrery_core::TICK_HZ;

// ── rock ────────────────────────────────────────────────────────────────

/// Record whether the rock entered this tick intact.
///
/// Two later systems key off the tick-start hull rather than the live one — a
/// rock destroyed *this* tick still runs its destruction rules, and a rock
/// that was already a husk runs the refusal rules instead. Keeping the
/// distinction in a named local makes it a stated premise rather than an
/// accident of which `if` a block happened to sit inside.
pub(crate) fn load_rock(rock: &mut Rock, cx: &mut Cx<'_>) {
    cx.locals.rock.was_alive = rock.hull > 0;
}

/// Apply this tick's sealed inputs to a live rock.
pub(crate) fn resolve_rock_orders(rock: &mut Rock, cx: &mut Cx<'_>) {
    if !cx.locals.rock.was_alive {
        return;
    }
    let me = cx.entity;
    let origin = rock.pos;
    let inputs = cx.inputs;
    for order in inputs.iter() {
        match order {
            Order::LockRequested { locker } => {
                cx.emit(Outcome::LockConfirmed {
                    locker: *locker,
                    target: me,
                    class: LockClass::Rock,
                });
            }
            Order::Damage {
                amount,
                from,
                from_pos,
                from_vel,
                from_yaw_urad,
                from_archetype,
                from_weapon,
                flight_ticks,
            } => match projectile_resolution(
                origin,
                rock.vel,
                rock.tier.limits().radius_mm,
                rock.hull > 0,
                *from_pos,
                *from_vel,
                *from_yaw_urad,
                *from_archetype,
                *from_weapon,
                *flight_ticks,
                cx.rng,
            ) {
                ProjectileResolution::InFlight(ticks) => {
                    cx.emit(Outcome::DamageDealt {
                        attacker: *from,
                        target: me,
                        amount: *amount,
                        attacker_pos: *from_pos,
                        attacker_vel: *from_vel,
                        attacker_yaw_urad: *from_yaw_urad,
                        attacker_archetype: *from_archetype,
                        attacker_weapon: *from_weapon,
                        flight_ticks: Some(ticks),
                    });
                }
                ProjectileResolution::Hit => {
                    cx.emit(Outcome::ShotResolved {
                        attacker: *from,
                        target: me,
                        result: ShotResult::Hit,
                    });
                    rock.hull = (rock.hull - (*amount).max(0)).max(0);
                    if rock.hull == 0 {
                        cx.locals.rock.killer = Some(*from);
                    }
                }
                ProjectileResolution::OutOfArc => {
                    cx.emit(Outcome::ShotResolved {
                        attacker: *from,
                        target: me,
                        result: ShotResult::OutOfArc,
                    });
                }
                ProjectileResolution::Break(reason) => {
                    cx.emit(Outcome::LockBroken {
                        locker: *from,
                        target: me,
                        reason,
                    });
                }
                ProjectileResolution::Miss => {
                    cx.emit(Outcome::ShotResolved {
                        attacker: *from,
                        target: me,
                        result: ShotResult::Miss,
                    });
                }
            },
            Order::CollisionResolved { from: _, velocity }
                if velocity_within_limit(*velocity, rock.tier.limits().max_speed_mms) =>
            {
                rock.vel = *velocity;
                rock.collisions = rock.collisions.saturating_add(1);
            }
            _ => {}
        }
    }
}

/// Split, drop and credit a rock that died this tick.
pub(crate) fn resolve_rock_destruction(rock: &mut Rock, cx: &mut Cx<'_>) {
    if !cx.locals.rock.was_alive || rock.hull != 0 {
        return;
    }
    let me = cx.entity;
    if let Some(child_tier) = rock.tier.child() {
        let children = split_children(me, rock, child_tier, cx.rng);
        cx.emit(Outcome::Split {
            parent: me,
            generation: rock.generation,
            children,
        });
        rock.splits_done = rock.splits_done.saturating_add(1);
        if let Some(bloom) = rock.bloom {
            cx.emit(Outcome::BloomPopulationChanged {
                director: bloom.director,
                bloom_index: bloom.bloom_index,
                delta: 1,
            });
        }
    } else {
        let threshold = if rock.born_in_bloom { 50 } else { 25 };
        if uniform_percent(cx.rng) < threshold {
            let kind = if cx.rng.next_u32() & 1 == 0 {
                weapon::WeaponKind::Volley
            } else {
                weapon::WeaponKind::Heavy
            };
            cx.emit(Outcome::SpawnPickup {
                id: pickup_id(me),
                pos: rock.pos,
                kind,
                expires_at: PICKUP_TTL_TICKS,
            });
            rock.pickups_dropped = rock.pickups_dropped.saturating_add(1);
        }
        if let Some(bloom) = rock.bloom {
            cx.emit(Outcome::BloomPopulationChanged {
                director: bloom.director,
                bloom_index: bloom.bloom_index,
                delta: -1,
            });
        }
    }
    if let Some(by) = cx.locals.rock.killer {
        cx.emit(Outcome::RockDestroyed {
            by,
            points: rock.tier.limits().points,
        });
        cx.emit(Outcome::LockBroken {
            locker: by,
            target: me,
            reason: LockBreakReason::TargetDestroyed,
        });
    }
}

/// Refuse everything addressed to a rock that was already a husk.
pub(crate) fn refuse_dead_rock_orders(_rock: &mut Rock, cx: &mut Cx<'_>) {
    if cx.locals.rock.was_alive {
        return;
    }
    let me = cx.entity;
    let inputs = cx.inputs;
    for order in inputs.iter() {
        match order {
            Order::Damage { from, .. } => cx.emit(Outcome::LockBroken {
                locker: *from,
                target: me,
                reason: LockBreakReason::TargetDestroyed,
            }),
            Order::LockRequested { locker } => cx.emit(Outcome::LockRefused {
                locker: *locker,
                target: me,
            }),
            _ => {}
        }
    }
}

/// Drift a live rock and reflect it off the island edge.
pub(crate) fn drift_rock(rock: &mut Rock, _cx: &mut Cx<'_>) {
    if rock.hull <= 0 {
        return;
    }
    rock.pos.x = flagged_add(
        rock.pos.x,
        rock.vel.x / i64::from(TICK_HZ),
        &mut rock.arithmetic_overflowed,
    );
    rock.pos.y = flagged_add(
        rock.pos.y,
        rock.vel.y / i64::from(TICK_HZ),
        &mut rock.arithmetic_overflowed,
    );
    rock.pos.z = flagged_add(
        rock.pos.z,
        rock.vel.z / i64::from(TICK_HZ),
        &mut rock.arithmetic_overflowed,
    );
    if rock.pos.x.unsigned_abs() > ISLAND_BOUNDARY_MM as u64 {
        rock.vel.x = flagged_neg(rock.vel.x, &mut rock.arithmetic_overflowed);
    }
    if rock.pos.y.unsigned_abs() > ISLAND_BOUNDARY_MM as u64 {
        rock.vel.y = flagged_neg(rock.vel.y, &mut rock.arithmetic_overflowed);
    }
    if rock.pos.z.unsigned_abs() > ISLAND_BOUNDARY_MM as u64 {
        rock.vel.z = flagged_neg(rock.vel.z, &mut rock.arithmetic_overflowed);
    }
}

// ── pickup ──────────────────────────────────────────────────────────────

/// Age an unclaimed pickup and expire it.
pub(crate) fn expire_pickup(pickup: &mut Pickup, cx: &mut Cx<'_>) {
    if pickup.claimed_by.is_some() || pickup.expired {
        return;
    }
    pickup.ttl_remaining = pickup.ttl_remaining.saturating_sub(1);
    if pickup.ttl_remaining == 0 {
        pickup.expired = true;
        cx.emit(Outcome::Expired { id: cx.entity });
    }
}

/// Adjudicate grab attempts and refuse locks.
pub(crate) fn contest_pickup(pickup: &mut Pickup, cx: &mut Cx<'_>) {
    let me = cx.entity;
    let inputs = cx.inputs;
    for order in inputs.iter() {
        match order {
            Order::GrabAttempt { ship, ship_pos } => {
                let eligible = pickup.claimed_by.is_none()
                    && !pickup.expired
                    && within_grab_reach(pickup.pos, *ship_pos);
                if eligible {
                    pickup.claimed_by = Some(*ship);
                    pickup.claimed_at =
                        Some(pickup.expires_at.saturating_sub(pickup.ttl_remaining));
                    cx.emit(Outcome::Granted {
                        ship: *ship,
                        kind: pickup.kind,
                    });
                } else {
                    cx.emit(Outcome::Denied { ship: *ship });
                }
            }
            Order::LockRequested { locker } => cx.emit(Outcome::LockRefused {
                locker: *locker,
                target: me,
            }),
            _ => {}
        }
    }
}

// ── bloom director ──────────────────────────────────────────────────────

/// Fold this tick's bloom population reports into the live site.
pub(crate) fn apply_bloom_population(director: &mut BloomDirector, cx: &mut Cx<'_>) {
    let me = cx.entity;
    let inputs = cx.inputs;
    for input in inputs {
        if let Order::LockRequested { locker } = input {
            cx.emit(Outcome::LockRefused {
                locker: *locker,
                target: me,
            });
            continue;
        }
        let Order::BloomPopulationChanged { bloom_index, delta } = input else {
            continue;
        };
        let current_index = director.blooms_seeded.checked_sub(1);
        if current_index != Some(*bloom_index) || director.site_pos.is_none() {
            continue;
        }
        director.site_rocks_alive = if *delta < 0 {
            director
                .site_rocks_alive
                .saturating_sub(delta.unsigned_abs().into())
        } else {
            director
                .site_rocks_alive
                .saturating_add(u16::try_from(*delta).unwrap_or(0))
                .min(BLOOM_MAX_LIVE_ROCKS)
        };
        if director.site_rocks_alive == 0 {
            director.site_pos = None;
            director.site_active_until = None;
        }
    }
}

/// Advance the director's own clock.
pub(crate) fn advance_bloom_clock(director: &mut BloomDirector, _cx: &mut Cx<'_>) {
    director.clock_tick = director.clock_tick.saturating_add(1);
}

/// Retire a bloom site that has outlived its window.
pub(crate) fn expire_bloom_site(director: &mut BloomDirector, _cx: &mut Cx<'_>) {
    if director
        .site_active_until
        .is_some_and(|until| director.clock_tick >= until)
    {
        director.site_pos = None;
        director.site_active_until = None;
        director.site_rocks_alive = 0;
    }
}

/// Seed the next bloom when the cadence comes due.
pub(crate) fn seed_bloom(director: &mut BloomDirector, cx: &mut Cx<'_>) {
    if director.clock_tick < director.next_bloom_tick {
        return;
    }
    let me = cx.entity;
    let bloom_index = director.blooms_seeded;
    let site_pos = draw_bloom_site(cx.rng);
    let active_until = director.clock_tick.saturating_add(BLOOM_LIFETIME_TICKS);
    let rocks = Box::new(core::array::from_fn(|slot| {
        bloom_spec(me, bloom_index, slot, site_pos, cx.rng)
    }));
    cx.emit(Outcome::BloomSeeded {
        director: me,
        bloom_index,
        site_pos,
        active_until,
        rocks,
    });
    director.blooms_seeded = director.blooms_seeded.saturating_add(1);
    director.next_bloom_tick = director.next_bloom_tick.saturating_add(BLOOM_CADENCE_TICKS);
    director.site_pos = Some(site_pos);
    director.site_active_until = Some(active_until);
    director.site_rocks_alive = BLOOM_ROCK_COUNT;
}

/// `regolith.world`'s resolution stage: everything driven by sealed inputs.
pub(crate) const RESOLUTION: &[System<Regolith, RegolithLocals>] = &[
    rock_system!("rock-load", load_rock),
    rock_system!("rock-resolve-orders", resolve_rock_orders),
    rock_system!("rock-resolve-destruction", resolve_rock_destruction),
    rock_system!("rock-refuse-when-dead", refuse_dead_rock_orders),
    pickup_system!("pickup-expire", expire_pickup),
    pickup_system!("pickup-contest", contest_pickup),
    director_system!("bloom-apply-population", apply_bloom_population),
];

/// `regolith.world`'s lifecycle stage: motion and the bloom cadence.
pub(crate) const LIFECYCLE: &[System<Regolith, RegolithLocals>] = &[
    rock_system!("rock-drift", drift_rock),
    director_system!("bloom-advance-clock", advance_bloom_clock),
    director_system!("bloom-expire-site", expire_bloom_site),
    director_system!("bloom-seed", seed_bloom),
];
