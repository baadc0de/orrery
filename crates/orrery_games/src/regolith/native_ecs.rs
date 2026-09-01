//! Native ECS dispatch across Regolith's migrated module frontier.
//!
//! The host registers one ruleset-owned callback at the seam. This dispatcher
//! keeps the two declared modules separate while selecting the module that owns
//! the active entity's state section.

use bevy_ecs::prelude::{Entity, World};
use orrery_core::{OrderedInputs, StateView, StepOutput, TickRng};

use super::order::{Order, Outcome};
use super::state::RegolithState;
use super::{craft_ecs, world_ecs, Regolith};

/// Synchronize every migrated module's concrete components with the seam cache.
pub fn sync_migrated(world: &mut World, entity: Entity, state: Option<&RegolithState>) {
    world_ecs::sync_migrated(world, entity, state);
    craft_ecs::sync_migrated(world, entity, state);
}

/// Run the native schedule owned by the active entity's declared module.
pub fn step_migrated(
    world: &mut World,
    entity: Entity,
    rules: &Regolith,
    view: &mut StateView<'_, RegolithState>,
    inputs: &OrderedInputs<'_, Order>,
    rng: &mut TickRng,
) -> StepOutput<Outcome> {
    if matches!(view.own(), RegolithState::Craft(_)) {
        craft_ecs::step_migrated(world, entity, rules, view, inputs, rng)
    } else {
        world_ecs::step_migrated(world, entity, rules, view, inputs, rng)
    }
}
