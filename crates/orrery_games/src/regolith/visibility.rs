//! Audited recorded-neighbour predicates for Regolith claims.

use orrery_core::{geometry::segment_intersects_sphere, OrderedInputs, QPos, QVel, StateView};
use orrery_protocol::PersistId;

use super::{order::Order, state::RegolithState, Outcome, OCCLUSION_MARGIN_MM};

/// Collision state produced by the audited predicate and applied to own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CollisionResolution {
    pub(crate) other: PersistId,
    pub(crate) own_velocity: QVel,
    pub(crate) target_velocity: QVel,
}

/// All cross-entity claims admitted through the single audited read site.
#[derive(Debug, Default)]
pub(crate) struct VerifiedClaims {
    pub(crate) visibility: Option<Outcome>,
    pub(crate) collision: Option<CollisionResolution>,
    pub(crate) arithmetic_overflowed: bool,
}

#[derive(Clone, Copy)]
struct Body {
    pos: QPos,
    vel: QVel,
    radius_mm: i64,
    mass_units: i64,
    max_speed_mms: i64,
    craft: bool,
    alive: bool,
}

impl Body {
    fn from_state(state: &RegolithState) -> Option<Self> {
        match state {
            RegolithState::Craft(craft) => {
                let limits = craft.archetype.limits();
                Some(Self {
                    pos: craft.pos,
                    vel: craft.vel,
                    radius_mm: limits.radius_mm,
                    mass_units: limits.mass_units,
                    max_speed_mms: limits.max_speed_mms,
                    craft: true,
                    alive: craft.alive(),
                })
            }
            RegolithState::Rock(rock) => {
                let limits = rock.tier.limits();
                Some(Self {
                    pos: rock.pos,
                    vel: rock.vel,
                    radius_mm: limits.radius_mm,
                    mass_units: limits.mass_units,
                    max_speed_mms: limits.max_speed_mms,
                    craft: false,
                    alive: rock.hull > 0,
                })
            }
            RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => None,
        }
    }
}

/// Verify rate-eligible visibility and collision claims against recorded frames.
///
/// The expensive broad phase stays outside the core. This function performs
/// the O(1), integer-exact predicates through the one read expression admitted
/// by D43(d); replay receives the same canonical frames instead of consulting a
/// live world.
pub(crate) fn verify_claims(
    view: &mut StateView<'_, RegolithState>,
    inputs: &OrderedInputs<'_, Order>,
) -> VerifiedClaims {
    let cover = matches!(
        view.own(),
        RegolithState::Craft(craft) if craft.cover_claim_cooldown == 0
    )
    .then(|| {
        inputs.iter().find_map(|order| match order {
            Order::ClaimCover { locker, rock } => Some((*locker, *rock)),
            _ => None,
        })
    })
    .flatten();
    let collision_id = inputs.iter().find_map(|order| match order {
        Order::Collide { other } => Some(*other),
        _ => None,
    });
    let [locker, rock, collision] = [
        cover.map(|pair| pair.0),
        cover.map(|pair| pair.1),
        collision_id,
    ]
    .map(|id| id.and_then(|id| view.neighbor(id).cloned()));

    let visibility = cover.and_then(|(locker_id, rock_id)| {
        verify_visibility(view, locker_id, rock_id, locker.as_ref(), rock.as_ref())
    });
    let mut arithmetic_overflowed = false;
    let collision = collision_id.and_then(|other_id| {
        verify_collision(
            view.entity(),
            view.own(),
            other_id,
            collision.as_ref()?,
            &mut arithmetic_overflowed,
        )
    });

    VerifiedClaims {
        visibility,
        collision,
        arithmetic_overflowed,
    }
}

fn verify_visibility(
    view: &StateView<'_, RegolithState>,
    locker_id: PersistId,
    rock_id: PersistId,
    locker: Option<&RegolithState>,
    rock: Option<&RegolithState>,
) -> Option<Outcome> {
    let RegolithState::Craft(target) = view.own() else {
        return None;
    };
    if target.cover_claim_cooldown > 0 {
        return None;
    }
    let target_id = view.entity();
    if locker_id == rock_id || locker_id == target_id || rock_id == target_id {
        return None;
    }
    let (Some(RegolithState::Craft(locker)), Some(RegolithState::Rock(rock))) = (locker, rock)
    else {
        return None;
    };
    if locker.lock_target != Some(target_id)
        || locker.lock_progress != super::LOCK_ACQUISITION_TICKS
    {
        return None;
    }
    let radius = rock
        .tier
        .limits()
        .radius_mm
        .saturating_sub(OCCLUSION_MARGIN_MM);
    Some(Outcome::LockVisibility {
        locker: locker_id,
        target: target_id,
        occluded: segment_intersects_sphere(locker.pos, target.pos, rock.pos, radius),
    })
}

fn verify_collision(
    me: PersistId,
    own_state: &RegolithState,
    other_id: PersistId,
    other_state: &RegolithState,
    overflowed: &mut bool,
) -> Option<CollisionResolution> {
    let own = Body::from_state(own_state)?;
    let other = Body::from_state(other_state)?;
    // Ships submit broad-phase candidates. Rock-rock contact is outside #441,
    // and the lower stable id is the sole ship-ship resolver.
    if !own.craft || !own.alive || !other.alive || (other.craft && me >= other_id) {
        return None;
    }

    let normal = checked_sub_components(
        [other.pos.x, other.pos.y, other.pos.z],
        [own.pos.x, own.pos.y, own.pos.z],
    );
    let distance_sq = checked_sum_squares(normal, overflowed)?;
    let radius = own.radius_mm.checked_add(other.radius_mm).or_else(|| {
        *overflowed = true;
        None
    })?;
    let radius_sq = i128::from(radius)
        .checked_mul(i128::from(radius))
        .or_else(|| {
            *overflowed = true;
            None
        })?;
    if distance_sq == 0 || distance_sq > radius_sq {
        return None;
    }

    let relative_velocity = checked_sub_components(
        [other.vel.x, other.vel.y, other.vel.z],
        [own.vel.x, own.vel.y, own.vel.z],
    );
    let approach = checked_dot(relative_velocity, normal, overflowed)?;
    if approach >= 0 {
        return None;
    }

    let mass_sum = own.mass_units.checked_add(other.mass_units).or_else(|| {
        *overflowed = true;
        None
    })?;
    let denominator = i128::from(mass_sum).checked_mul(distance_sq).or_else(|| {
        *overflowed = true;
        None
    })?;
    let own_delta = collision_delta(normal, approach, other.mass_units, denominator, overflowed)?;
    let target_delta = collision_delta(
        normal,
        approach.checked_neg().or_else(|| {
            *overflowed = true;
            None
        })?,
        own.mass_units,
        denominator,
        overflowed,
    )?;
    Some(CollisionResolution {
        other: other_id,
        own_velocity: bounded_add(own.vel, own_delta, own.max_speed_mms, overflowed)?,
        target_velocity: bounded_add(other.vel, target_delta, other.max_speed_mms, overflowed)?,
    })
}

fn collision_delta(
    normal: [i128; 3],
    approach: i128,
    other_mass: i64,
    denominator: i128,
    overflowed: &mut bool,
) -> Option<QVel> {
    let scale = 2_i128
        .checked_mul(i128::from(other_mass))
        .and_then(|value| value.checked_mul(approach))
        .or_else(|| {
            *overflowed = true;
            None
        })?;
    let mut component = |axis: i128| {
        scale
            .checked_mul(axis)
            .and_then(|value| value.checked_div(denominator))
            .and_then(|value| i64::try_from(value).ok())
            .or_else(|| {
                *overflowed = true;
                None
            })
    };
    Some(QVel {
        x: component(normal[0])?,
        y: component(normal[1])?,
        z: component(normal[2])?,
    })
}

fn bounded_add(
    velocity: QVel,
    delta: QVel,
    max_speed_mms: i64,
    overflowed: &mut bool,
) -> Option<QVel> {
    let mut add = |left: i64, right: i64| {
        left.checked_add(right).or_else(|| {
            *overflowed = true;
            None
        })
    };
    let velocity = QVel {
        x: add(velocity.x, delta.x)?,
        y: add(velocity.y, delta.y)?,
        z: add(velocity.z, delta.z)?,
    };
    Some(clamp_velocity(velocity, max_speed_mms))
}

fn clamp_velocity(velocity: QVel, max_speed_mms: i64) -> QVel {
    let speed_sq = [velocity.x, velocity.y, velocity.z]
        .into_iter()
        .map(|value| i128::from(value).unsigned_abs().pow(2))
        .sum::<u128>();
    let speed = super::integer_sqrt(speed_sq);
    let ceiling = max_speed_mms.max(0) as u128;
    if speed <= ceiling || speed == 0 {
        return velocity;
    }
    let scaled = |value: i64| {
        i64::try_from(i128::from(value) * i128::from(max_speed_mms) / speed as i128)
            .unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
    };
    QVel {
        x: scaled(velocity.x),
        y: scaled(velocity.y),
        z: scaled(velocity.z),
    }
}

fn checked_sub_components(a: [i64; 3], b: [i64; 3]) -> [i128; 3] {
    core::array::from_fn(|axis| i128::from(a[axis]) - i128::from(b[axis]))
}

fn checked_sum_squares(values: [i128; 3], overflowed: &mut bool) -> Option<i128> {
    values.into_iter().try_fold(0_i128, |sum, value| {
        value
            .checked_mul(value)
            .and_then(|square| sum.checked_add(square))
            .or_else(|| {
                *overflowed = true;
                None
            })
    })
}

fn checked_dot(a: [i128; 3], b: [i128; 3], overflowed: &mut bool) -> Option<i128> {
    a.into_iter().zip(b).try_fold(0_i128, |sum, (left, right)| {
        left.checked_mul(right)
            .and_then(|product| sum.checked_add(product))
            .or_else(|| {
                *overflowed = true;
                None
            })
    })
}
