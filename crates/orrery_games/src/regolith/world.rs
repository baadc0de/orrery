//! World-domain delegation for the assembled Regolith ruleset.
//!
//! This domain owns the `Rock`, `Pickup`, and `BloomDirector` sections of
//! `RegolithState`. It receives craft-originated requests only through the
//! ordered next-tick `Order` channel and emits typed `Outcome`s for the same
//! channel to compose on the following tick.

use orrery_core::{OrderedInputs, TickRng};
use orrery_protocol::PersistId;

use super::{
    order::Outcome,
    state::{BloomDirector, Pickup, Rock},
    Order, Regolith,
};

/// Execute rock resolution and lifecycle behaviour.
pub(crate) fn step_rock(
    rules: &Regolith,
    entity: PersistId,
    rock: Rock,
    inputs: &OrderedInputs<'_, Order>,
    rng: &mut TickRng,
) -> (Rock, Vec<Outcome>) {
    rules.step_rock(entity, rock, inputs, rng)
}

/// Execute pickup expiry and contest behaviour.
pub(crate) fn step_pickup(
    entity: PersistId,
    pickup: Pickup,
    inputs: &OrderedInputs<'_, Order>,
) -> (Pickup, Vec<Outcome>) {
    Regolith::step_pickup(entity, pickup, inputs)
}

/// Execute bloom lifecycle behaviour.
pub(crate) fn step_director(
    entity: PersistId,
    director: BloomDirector,
    inputs: &OrderedInputs<'_, Order>,
    rng: &mut TickRng,
) -> (BloomDirector, Vec<Outcome>) {
    Regolith::step_director(entity, director, inputs, rng)
}
