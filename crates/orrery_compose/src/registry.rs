//! Reviewed, permanent component-type allocations for statically linked games.
//!
//! Values in a game's table are monotone allocations: do not reuse a retired
//! value or renumber an existing one. [`crate::validate`] rejects duplicate
//! values at composition time.

use orrery_core::ComponentTypeId;

/// One reviewed entry in a game's permanent component-type registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentTypeIdRegistryEntry {
    /// The permanent game-assigned identifier.
    pub id: ComponentTypeId,
    /// The source-level name allocated this identifier.
    pub name: &'static str,
}

/// Regolith's reviewed component-type allocations.
///
/// This transcribes `crates/orrery_games/src/regolith/mod.rs:328-331` exactly.
/// S4.2 may make the game consume this table; S4.1 deliberately does not
/// modify the shipped game source.
pub mod regolith {
    use super::{ComponentTypeId, ComponentTypeIdRegistryEntry};

    /// Verifiable Regolith state for every entity-window variant.
    pub const STATE: ComponentTypeId = ComponentTypeId(1);

    /// The permanent reviewed allocation table for Regolith.
    pub const COMPONENT_TYPE_IDS: &[ComponentTypeIdRegistryEntry] =
        &[ComponentTypeIdRegistryEntry {
            id: STATE,
            name: "STATE",
        }];
}
