//! Nested-grid identity (docs/01-spatial-model.md §13).
//!
//! Each moving reference frame (ship, planet, station) is its own `CellId`
//! space. A [`GridId`] is carried alongside a [`CellId`] wherever a cell
//! reference can cross frames — wire messages, journal records, storage keys,
//! log records. The root universe grid is 0.

use serde::{Deserialize, Serialize};

/// Identifies one nested grid (one `CellId` space). The root universe grid is
/// `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GridId(pub u32);

impl GridId {
    /// The root universe grid.
    pub const ROOT: Self = Self(0);

    /// A grid id for a nested reference frame.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

impl core::fmt::Display for GridId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "grid:{}", self.0)
    }
}
