//! Mapping `big_space` grid coordinates to `orrery_protocol::CellId`.
//!
//! The engine-independent `CellId` encoding lives in `orrery_protocol`; this
//! module bridges it to `big_space`'s [`Grid`]/[`CellCoord`] so an entity's
//! interest cell (the replication group) is derived from its grid position.
//!
//! Constants and engine-free helpers are re-exported from `orrery_protocol`
//! so existing callers keep compiling unchanged.

pub use orrery_protocol::{
    cell_id_from_metres, metres_from_cell_id, shard_of, CellId, DEFAULT_CELL_EDGE_M,
    INTEREST_LEVEL, SHARD_LEVEL,
};

#[cfg(feature = "big_space")]
use crate::SpatialConfig;

/// Convert a `big_space` [`CellCoord`] (the integer grid cell index) into an
/// interest-level [`CellId`].
///
/// `big_space`'s `GridPrecision` is `i64` by default; the `CellId` encoding
/// addresses ±2²⁰ cells per axis at level 21. Coordinates outside that range
/// (a grid larger than the addressable volume) are clamped to the nearest
/// representable cell — the design's `u128` feature extends the range for grids
/// that need it (docs/01-spatial-model.md §4).
#[cfg(feature = "big_space")]
pub fn cell_of(coord: &big_space::prelude::CellCoord, _cfg: &SpatialConfig) -> CellId {
    let x = clamp_coord(coord.x);
    let y = clamp_coord(coord.y);
    let z = clamp_coord(coord.z);
    CellId::from_coords(glam::IVec3::new(x, y, z), INTEREST_LEVEL)
        .expect("clamped coordinates are always in range")
}

/// Clamp a `GridPrecision` (i64) coordinate into the `CellId` level-21 range.
#[cfg(feature = "big_space")]
fn clamp_coord(c: big_space::prelude::GridPrecision) -> i32 {
    let half = 1i64 << (INTEREST_LEVEL - 1);
    c.clamp(-half, half - 1) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_level_is_interest_minus_three() {
        assert_eq!(SHARD_LEVEL, 18);
        // One shard cell = 8×8×8 interest cells.
        assert_eq!(INTEREST_LEVEL - SHARD_LEVEL, 3);
    }

    #[test]
    fn shard_of_is_ancestor() {
        let cell = CellId::from_coords(glam::IVec3::new(2, -1, 8), INTEREST_LEVEL).unwrap();
        let shard = shard_of(cell);
        assert_eq!(shard.level(), SHARD_LEVEL);
        assert!(shard.is_prefix_of(cell));
    }

    #[cfg(feature = "big_space")]
    #[test]
    fn cell_of_maps_grid_coords() {
        use big_space::prelude::CellCoord;
        let cfg = SpatialConfig::default();
        let coord = CellCoord::new(2, -1, 8);
        let cell = cell_of(&coord, &cfg);
        assert_eq!(cell.level(), INTEREST_LEVEL);
        assert_eq!(cell.coords(), (glam::IVec3::new(2, -1, 8), INTEREST_LEVEL));
    }
}
