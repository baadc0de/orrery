//! Craft-domain delegation for the assembled Regolith ruleset.
//!
//! This domain owns the `Craft` section of `RegolithState`, including player
//! control, weapon requests, and consumption of target-owned resolutions.

use orrery_core::{OrderedInputs, TickRng};
use orrery_protocol::PersistId;

use super::{order::Outcome, state::Craft, visibility, Order, Regolith};

/// Execute the craft-owned behaviour in the manifest's declared order.
pub(crate) fn step(
    rules: &Regolith,
    entity: PersistId,
    craft: Craft,
    inputs: &OrderedInputs<'_, Order>,
    collision: Option<visibility::CollisionResolution>,
    rng: &mut TickRng,
) -> (Craft, Vec<Outcome>) {
    rules.step_craft(entity, craft, inputs, collision, rng)
}
