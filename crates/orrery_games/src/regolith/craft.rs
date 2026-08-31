//! The `regolith.craft` module's canonical systems.
//!
//! This module owns the `Craft` section of `RegolithState`: player control,
//! weapon requests, and consumption of target-owned resolutions. Each rule is
//! a named function over `&mut Craft`; the ordered tables at the bottom are
//! what the executor actually runs, and what the composition manifest
//! declares.
//!
//! **Why the tick is split here and not elsewhere.** Every cut is at a point
//! where the original single body had already finished with a value: the
//! sealed-order loop is one system because D46(d) makes the authority's input
//! order physical and nothing may reorder inside it, while everything before
//! and after it was already a sequence of independent phases held together
//! only by local variables. The values that genuinely have to survive a cut
//! *unrounded* — position and velocity in metres — live in
//! [`super::CraftScratch`], which is why the VC-7 boundary is now two named
//! systems (`craft-load-kinematics`, `craft-store-kinematics`) instead of
//! whichever statement happened to run last.

use orrery_core::{QPos, QVel, System};
use rand_core::RngCore;

use super::state::{Craft, LockClass, PITCH_LIMIT_URAD, TAU_URAD};
use super::{
    order::{LockBreakReason, Order, Outcome, ShotResult},
    projectile_resolution, spawn_pose, velocity_within_limit, weapon, Cx, ProjectileResolution,
    Regolith, RegolithLocals, COVER_CLAIM_INTERVAL_TICKS, DRAG_PER_SEC, DT, LOCK_ACQUISITION_TICKS,
    LOCK_DECAY_PER_TICK, RESPAWN_TICKS,
};

/// Age both per-tick cooldowns by one tick.
pub(crate) fn tick_cooldowns(craft: &mut Craft, _cx: &mut Cx<'_>) {
    craft.cooldown = craft.cooldown.saturating_sub(1);
    craft.cover_claim_cooldown = craft.cover_claim_cooldown.saturating_sub(1);
}

/// Advance an in-progress lock decay, dropping the lock when it completes.
pub(crate) fn decay_lock(craft: &mut Craft, _cx: &mut Cx<'_>) {
    if craft.lock_decay_progress == 0 {
        return;
    }
    craft.lock_decay_progress = craft
        .lock_decay_progress
        .saturating_add(LOCK_DECAY_PER_TICK);
    if craft.lock_decay_progress >= LOCK_ACQUISITION_TICKS {
        craft.lock_target = None;
        craft.lock_class = None;
        craft.lock_progress = 0;
        craft.lock_decay_progress = 0;
    }
}

/// Open the tick's unquantized kinematic window (VC-7).
///
/// Position and velocity leave the lattice here and rejoin it in
/// [`store_kinematics`]. Every system between the two works in metres, so a
/// rounding step cannot be introduced by accident between two rules.
pub(crate) fn load_kinematics(craft: &mut Craft, cx: &mut Cx<'_>) {
    let scratch = &mut cx.locals.craft;
    let (px, py, pz) = craft.pos.to_metres();
    let (vx, vy, vz) = craft.vel.to_metres_per_sec();
    scratch.pos_m = [px, py, pz];
    scratch.vel_mps = [vx, vy, vz];
    scratch.was_alive = craft.alive();
}

/// Apply this tick's sealed inputs, in the authority's order.
///
/// This is one system on purpose. D46(d) makes the sealed order physical — a
/// prior-tick `CollisionResolved` applies before an authored `Collide`, and a
/// `Fire` consumes the lock that existed before a later `Lock` switches it —
/// so a decomposition that ran one input class before another would be a
/// different simulation, not a tidier one.
#[allow(clippy::too_many_lines)]
pub(crate) fn apply_orders(craft: &mut Craft, cx: &mut Cx<'_>) {
    let me = cx.entity;
    let limits = craft.archetype.limits();
    let origin = craft.pos;
    let firing_vel = craft.vel;
    let was_alive = cx.locals.craft.was_alive;
    let collision = cx.locals.claims.collision;
    let mut disabled = !was_alive;
    for order in cx.inputs.iter() {
        match order {
            Order::Thrust { .. } | Order::Lock { .. } | Order::Fire | Order::Grab { .. }
                if disabled => {}
            Order::Thrust {
                accel_mmss,
                yaw_urad,
                pitch_urad,
            } => {
                let accel = i64::from(*accel_mmss)
                    .clamp(0, cx.rules.movement_cap(limits.max_accel_mmss))
                    as f64
                    / 1_000.0;
                let theta = f64::from(craft.yaw_urad) / 1_000_000.0;
                let phi = f64::from(craft.pitch_urad) / 1_000_000.0;
                let horizontal = libm::cos(phi);
                // Thrust contributes Δv for this tick. It does not replace
                // velocity: repeated thrust therefore builds speed over
                // time until drag and the chassis ceiling balance it.
                let delta_vx = accel * horizontal * libm::cos(theta) * DT;
                let delta_vy = accel * libm::sin(phi) * DT;
                let delta_vz = accel * horizontal * libm::sin(theta) * DT;
                let vel = &mut cx.locals.craft.vel_mps;
                vel[0] += delta_vx;
                vel[1] += delta_vy;
                vel[2] += delta_vz;
                craft.yaw_urad = craft.yaw_urad.wrapping_add(*yaw_urad).rem_euclid(TAU_URAD);
                craft.pitch_urad = craft
                    .pitch_urad
                    .saturating_add(*pitch_urad)
                    .clamp(-PITCH_LIMIT_URAD, PITCH_LIMIT_URAD);
            }
            Order::Lock { target } => match craft.lock_target {
                None => {
                    craft.lock_target = Some(*target);
                    craft.lock_class = None;
                    craft.lock_progress = 1;
                    craft.lock_decay_progress = 0;
                    cx.emit(Outcome::LockRequested {
                        locker: me,
                        target: *target,
                    });
                }
                Some(current) if current == *target => {
                    if craft.lock_progress < LOCK_ACQUISITION_TICKS {
                        craft.lock_progress = craft.lock_progress.saturating_add(1);
                        if craft.lock_progress == LOCK_ACQUISITION_TICKS
                            && craft.lock_class.is_some()
                        {
                            craft.locks_acquired = craft.locks_acquired.saturating_add(1);
                        }
                    }
                }
                // A Lock naming a different target switches the lock,
                // paying acquisition again from scratch: the switch is
                // free to make but never cheaper than a fresh lock.
                Some(_) => {
                    craft.lock_target = Some(*target);
                    craft.lock_class = None;
                    craft.lock_progress = 1;
                    craft.lock_decay_progress = 0;
                    cx.emit(Outcome::LockRequested {
                        locker: me,
                        target: *target,
                    });
                }
            },
            Order::LockConfirmed { target, class } => {
                cx.locals.craft.lock_reply = Some((*target, Some(*class)));
            }
            Order::LockRefused { target } => {
                cx.locals.craft.lock_reply = Some((*target, None));
            }
            Order::Fire => {
                // Orders are applied in their sealed order. A preceding Lock
                // therefore switches first, while a preceding Fire consumes
                // the lock that existed before a later switch.
                let Some(target) = craft.lock_target.filter(|_| {
                    craft.lock_progress >= LOCK_ACQUISITION_TICKS && craft.lock_class.is_some()
                }) else {
                    cx.emit(Outcome::ShotRefused {
                        attacker: me,
                        result: ShotResult::NoLock,
                    });
                    continue;
                };
                let equipped = craft.weapon;
                let weapon = equipped.weapon();
                if craft.cooldown > 0 && cx.rules.honours_cooldown() {
                    continue;
                }
                for _ in 0..weapon.rolls {
                    let roll = cx.rng.next_u32() % weapon.damage_spread.max(1);
                    let amount = cx.rules.damage(
                        i32::try_from(weapon.damage_base.saturating_add(roll)).unwrap_or(i32::MAX),
                    );
                    craft.damage_dealt = craft
                        .damage_dealt
                        .saturating_add(u64::from(amount.unsigned_abs()));
                    cx.emit(Outcome::DamageDealt {
                        attacker: me,
                        target,
                        amount,
                        attacker_pos: origin,
                        attacker_vel: firing_vel,
                        attacker_yaw_urad: craft.yaw_urad,
                        attacker_archetype: craft.archetype,
                        attacker_weapon: equipped,
                        flight_ticks: None,
                    });
                }
                craft.shots = craft.shots.saturating_add(1);
                craft.cooldown = weapon.cooldown_ticks;
            }
            Order::LockRequested { locker } => {
                if craft.hull > 0 {
                    cx.emit(Outcome::LockConfirmed {
                        locker: *locker,
                        target: me,
                        class: LockClass::Ship,
                    });
                } else {
                    cx.emit(Outcome::LockRefused {
                        locker: *locker,
                        target: me,
                    });
                }
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
            } => {
                match projectile_resolution(
                    origin,
                    craft.vel,
                    limits.radius_mm,
                    was_alive && !disabled,
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
                        continue;
                    }
                    ProjectileResolution::Miss => {
                        cx.emit(Outcome::ShotResolved {
                            attacker: *from,
                            target: me,
                            result: ShotResult::Miss,
                        });
                        continue;
                    }
                    ProjectileResolution::OutOfArc => {
                        cx.emit(Outcome::ShotResolved {
                            attacker: *from,
                            target: me,
                            result: ShotResult::OutOfArc,
                        });
                        continue;
                    }
                    ProjectileResolution::Break(reason) => {
                        cx.emit(Outcome::LockBroken {
                            locker: *from,
                            target: me,
                            reason,
                        });
                        continue;
                    }
                    ProjectileResolution::Hit => {
                        cx.emit(Outcome::ShotResolved {
                            attacker: *from,
                            target: me,
                            result: ShotResult::Hit,
                        });
                    }
                }
                let incoming = (*amount).max(0);
                let absorbed = incoming.min(craft.shield.max(0));
                craft.shield -= absorbed;
                let through = incoming - absorbed;
                if through > 0 && craft.hull > 0 {
                    craft.hull = (craft.hull - through).max(0);
                    if craft.hull == 0 {
                        disabled = true;
                        craft.respawn_in = RESPAWN_TICKS;
                        cx.emit(Outcome::Destroyed { by: *from });
                        cx.emit(Outcome::LockBroken {
                            locker: *from,
                            target: me,
                            reason: LockBreakReason::TargetDestroyed,
                        });
                    }
                }
            }
            Order::Grab { pickup } => {
                craft.grabs_attempted = craft.grabs_attempted.saturating_add(1);
                cx.emit(Outcome::GrabAttempted {
                    pickup: *pickup,
                    ship: me,
                    ship_pos: origin,
                });
            }
            Order::PickupGranted { kind } => {
                // This write is the durable inventory trace: the pickup
                // decided the outcome, then delivery brought it home.
                craft.weapon = *kind;
                craft.pickups_won = craft.pickups_won.saturating_add(1);
            }
            Order::PickupDenied => {
                craft.grabs_lost = craft.grabs_lost.saturating_add(1);
            }
            Order::KillCredit => craft.kills = craft.kills.saturating_add(1),
            Order::RockCredit { points } => {
                craft.score_rock_points =
                    craft.score_rock_points.saturating_add(u64::from(*points));
            }
            Order::LockBroken { target, reason: _ } => {
                if craft.lock_target == Some(*target) {
                    craft.lock_target = None;
                    craft.lock_class = None;
                    craft.lock_progress = 0;
                    craft.lock_decay_progress = 0;
                }
            }
            Order::LockVisibility { target, occluded } => {
                if craft.lock_target == Some(*target)
                    && craft.lock_progress == LOCK_ACQUISITION_TICKS
                {
                    if *occluded {
                        if craft.lock_decay_progress == 0 {
                            craft.lock_decay_progress = LOCK_DECAY_PER_TICK;
                        }
                    } else {
                        craft.lock_decay_progress = 0;
                        craft.lock_progress = LOCK_ACQUISITION_TICKS;
                    }
                }
            }
            Order::CollisionResolved { from: _, velocity } => {
                if velocity_within_limit(*velocity, limits.max_speed_mms) {
                    let vel = &mut cx.locals.craft.vel_mps;
                    vel[0] = velocity.x as f64 / 1_000.0;
                    vel[1] = velocity.y as f64 / 1_000.0;
                    vel[2] = velocity.z as f64 / 1_000.0;
                    craft.collisions = craft.collisions.saturating_add(1);
                }
            }
            Order::Collide { other }
                if collision.is_some_and(|resolution| resolution.other == *other) =>
            {
                let resolution = collision.expect("guarded by the matching resolution");
                // One exchange is adjudicated twice, but its force is computed
                // once: this step applies the resolver's own velocity and the
                // event carries its counterparty's. Keeping this arm in the
                // sealed-order loop is the physical meaning of D46(d). A
                // prior-tick CollisionResolved is delivered first and applies
                // before this authored contact; reversing host composition
                // reverses which mutually applied force is observed last.
                let vel = &mut cx.locals.craft.vel_mps;
                vel[0] = resolution.own_velocity.x as f64 / 1_000.0;
                vel[1] = resolution.own_velocity.y as f64 / 1_000.0;
                vel[2] = resolution.own_velocity.z as f64 / 1_000.0;
                craft.collisions = craft.collisions.saturating_add(1);
                cx.emit(Outcome::Collision {
                    collider: me,
                    target: resolution.other,
                    target_velocity: resolution.target_velocity,
                });
            }
            Order::ClaimCover { .. } => {
                if craft.cover_claim_cooldown == 0 {
                    craft.cover_claim_cooldown = COVER_CLAIM_INTERVAL_TICKS;
                }
            }
            Order::GrabAttempt { .. }
            | Order::BloomPopulationChanged { .. }
            | Order::ShotResolved { .. }
            | Order::Collide { .. } => {}
        }
    }
}

/// Settle the target's answer to a lock request, once the sealed orders are in.
pub(crate) fn resolve_lock_reply(craft: &mut Craft, cx: &mut Cx<'_>) {
    let Some((target, class)) = cx.locals.craft.lock_reply else {
        return;
    };
    if craft.lock_target != Some(target) {
        return;
    }
    match class {
        Some(class) if craft.lock_class.is_none() => {
            craft.lock_class = Some(class);
            if craft.lock_progress == LOCK_ACQUISITION_TICKS {
                craft.locks_acquired = craft.locks_acquired.saturating_add(1);
            }
        }
        Some(_) => {}
        None => {
            craft.lock_target = None;
            craft.lock_class = None;
            craft.lock_progress = 0;
            craft.lock_decay_progress = 0;
        }
    }
}

/// Hold speed at the chassis ceiling.
pub(crate) fn clamp_speed(craft: &mut Craft, cx: &mut Cx<'_>) {
    let ceiling = cx
        .rules
        .movement_cap(craft.archetype.limits().max_speed_mms) as f64
        / 1_000.0;
    let vel = &mut cx.locals.craft.vel_mps;
    let speed = libm::sqrt(vel[0] * vel[0] + vel[1] * vel[1] + vel[2] * vel[2]);
    if speed > ceiling && speed > 0.0 {
        let scale = ceiling / speed;
        vel[0] *= scale;
        vel[1] *= scale;
        vel[2] *= scale;
    }
}

/// Apply one tick of drag.
pub(crate) fn apply_drag(_craft: &mut Craft, cx: &mut Cx<'_>) {
    let retained = 1.0 - DRAG_PER_SEC * DT;
    for axis in &mut cx.locals.craft.vel_mps {
        *axis *= retained;
    }
}

/// Integrate position from velocity.
pub(crate) fn integrate(_craft: &mut Craft, cx: &mut Cx<'_>) {
    let scratch = &mut cx.locals.craft;
    for axis in 0..3 {
        scratch.pos_m[axis] += scratch.vel_mps[axis] * DT;
    }
}

/// Count down a wreck and reset it when the countdown expires.
pub(crate) fn respawn(craft: &mut Craft, cx: &mut Cx<'_>) {
    if cx.locals.craft.was_alive || craft.hull != 0 || craft.respawn_in == 0 {
        return;
    }
    craft.respawn_in -= 1;
    if craft.respawn_in != 0 {
        return;
    }
    let limits = craft.archetype.limits();
    let (spawn_pos, spawn_yaw) = spawn_pose(cx.entity.0.saturating_sub(1));
    let scratch = &mut cx.locals.craft;
    scratch.pos_m = [
        spawn_pos.x as f64 / 1_000.0,
        spawn_pos.y as f64 / 1_000.0,
        spawn_pos.z as f64 / 1_000.0,
    ];
    scratch.vel_mps = [0.0; 3];
    scratch.respawned = true;
    craft.yaw_urad = spawn_yaw;
    craft.pitch_urad = 0;
    craft.hull = limits.max_hull;
    craft.shield = limits.max_shield;
    craft.cooldown = 0;
    craft.weapon = weapon::WeaponKind::Stock;
    craft.lock_target = None;
    craft.lock_class = None;
    craft.lock_progress = 0;
    craft.lock_decay_progress = 0;
    craft.cover_claim_cooldown = 0;
}

/// Close the unquantized window: snap position and velocity back to the
/// lattice (VC-7). Nothing after this may work in metres.
pub(crate) fn store_kinematics(craft: &mut Craft, cx: &mut Cx<'_>) {
    let scratch = &cx.locals.craft;
    craft.pos = QPos::from_metres(scratch.pos_m[0], scratch.pos_m[1], scratch.pos_m[2]);
    craft.vel =
        QVel::from_metres_per_sec(scratch.vel_mps[0], scratch.vel_mps[1], scratch.vel_mps[2]);
}

/// Sample the engine trail from the stored position.
pub(crate) fn advance_trail(craft: &mut Craft, cx: &mut Cx<'_>) {
    if craft.hull == 0 || cx.locals.craft.respawned {
        // A wreck has no engine trail, and a respawn must not draw one
        // enormous segment from the wreck to the new spawn position.
        craft.trail.clear();
    } else {
        let pos = craft.pos;
        craft.trail.advance(pos, &mut craft.arithmetic_overflowed);
    }
}

/// `regolith.craft`'s control stage: cooldowns, locks and the sealed inputs.
pub(crate) const CONTROL: &[System<Regolith, RegolithLocals>] = &[
    craft_system!("craft-tick-cooldowns", tick_cooldowns),
    craft_system!("craft-decay-lock", decay_lock),
    craft_system!("craft-load-kinematics", load_kinematics),
    craft_system!("craft-apply-orders", apply_orders),
    craft_system!("craft-resolve-lock-reply", resolve_lock_reply),
];

/// `regolith.craft`'s motion stage: the whole unquantized window and its close.
pub(crate) const MOTION: &[System<Regolith, RegolithLocals>] = &[
    craft_system!("craft-clamp-speed", clamp_speed),
    craft_system!("craft-apply-drag", apply_drag),
    craft_system!("craft-integrate", integrate),
    craft_system!("craft-respawn", respawn),
    craft_system!("craft-store-kinematics", store_kinematics),
    craft_system!("craft-advance-trail", advance_trail),
];
