//! Audited claim-and-verify policy for Regolith visibility transitions.

use orrery_core::{geometry::segment_intersects_sphere, OrderedInputs, StateView};

use super::{order::Order, state::RegolithState, Outcome, OCCLUSION_MARGIN_MM};

/// Verify the first rate-eligible cover claim against two recorded neighbours.
///
/// The expensive occluder search stays outside the core. The target supplies
/// exactly the locker and rock it found; this function performs only the
/// integer-exact predicate. `StateView` records both reads, and replay receives
/// their canonical frames rather than consulting live world state.
pub(crate) fn verify_claim(
    view: &mut StateView<'_, RegolithState>,
    inputs: &OrderedInputs<'_, Order>,
) -> Option<Outcome> {
    let RegolithState::Craft(target) = view.own() else {
        return None;
    };
    if target.cover_claim_cooldown > 0 {
        return None;
    }
    let target_pos = target.pos;
    let (locker_id, rock_id) = inputs.iter().find_map(|order| match order {
        Order::ClaimCover { locker, rock } => Some((*locker, *rock)),
        _ => None,
    })?;
    let target_id = view.entity();
    if locker_id == rock_id || locker_id == target_id || rock_id == target_id {
        return None;
    }
    let [locker, rock] = [locker_id, rock_id].map(|id| view.neighbor(id).cloned());
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
        occluded: segment_intersects_sphere(locker.pos, target_pos, rock.pos, radius),
    })
}
