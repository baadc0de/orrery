//! Reviewed, permanent component-type allocations for statically linked games.
//!
//! Values in a game's table are monotone allocations: do not reuse a retired
//! value or renumber an existing one. [`crate::validate`] rejects duplicate
//! values at composition time.
//!
//! # Which side is canonical (#750)
//!
//! D45 clause (a) makes the schema id of record the pair
//! `(ComponentTypeId, SchemaVersion)`, so this table records **both halves of
//! that pair** and is the canonical reviewed ledger for them: an allocation or
//! a schema bump is an edit here, under review, and it is the one place a
//! reviewer has to look to see what a game has ever permanently claimed.
//!
//! A game's [`crate::CompatibilityManifest::component_schemas`] is the
//! **derived** statement of the same rows. It is not redundant with this
//! table: it adds the two things a ledger deliberately does not carry — the
//! owning [`crate::ModuleId`] and D45's five capability dimensions — and it is
//! readable by a consumer that has the manifest and not the game. The
//! duplication of the pair itself is accepted, and it is guarded rather than
//! trusted: a game's manifest table must agree with its row here, asserted by
//! a named test beside the manifest (for Regolith,
//! `regolith::composition_tests::the_manifest_schema_table_agrees_with_the_reviewed_registry`).

use orrery_core::ComponentTypeId;
use orrery_protocol::SchemaVersion;

/// One reviewed entry in a game's permanent component-type registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentTypeIdRegistryEntry {
    /// The permanent game-assigned identifier.
    pub id: ComponentTypeId,
    /// The reviewed current schema version for that identifier — the other
    /// half of D45 clause (a)'s schema id of record. Monotone, never reused
    /// or gapped; a bump here is the reviewed act, and the manifest follows.
    pub schema_version: SchemaVersion,
    /// The source-level name allocated this identifier.
    pub name: &'static str,
}

/// Regolith's reviewed component-type allocations.
///
/// This is the ledger `crates/orrery_games/src/regolith/mod.rs` consumes:
/// `regolith::components::STATE` aliases [`STATE`], and
/// `REGOLITH_COMPOSITION.component_schemas` states this row with its owner and
/// capabilities attached.
pub mod regolith {
    use super::{ComponentTypeId, ComponentTypeIdRegistryEntry};
    use orrery_protocol::atrest::SCHEMA_V0;

    /// Verifiable Regolith state for every entity-window variant.
    pub const STATE: ComponentTypeId = ComponentTypeId(1);

    /// The permanent reviewed allocation table for Regolith.
    ///
    /// `STATE` is still at the at-rest bootstrap version: no Regolith schema
    /// has ever been migrated, so nothing has earned a bump.
    pub const COMPONENT_TYPE_IDS: &[ComponentTypeIdRegistryEntry] =
        &[ComponentTypeIdRegistryEntry {
            id: STATE,
            schema_version: SCHEMA_V0,
            name: "STATE",
        }];
}
