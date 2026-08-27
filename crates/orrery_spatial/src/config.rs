//! Spatial configuration (D16 defaults).

use std::ops::RangeInclusive;

use bevy_ecs::prelude::*;

use orrery_protocol::DEFAULT_CELL_EDGE_M;

/// The guaranteed radius for an interaction between two independently
/// hysteretic bodies in a 27-cell AOI.
///
/// Each body's committed cell may lag its geometric cell by
/// `hysteresis_frac * cell_edge_m`. Interest membership compares those two
/// committed cells, so a pairwise interaction must reserve both margins:
/// `cell_edge_m - 2 * hysteresis_frac * cell_edge_m`.
///
/// A non-positive result means this configuration provides no positive
/// pairwise guarantee; configuration validation remains the caller's concern.
#[must_use]
pub fn pairwise_aoi_radius_m(cell_edge_m: f32, hysteresis_frac: f32) -> f32 {
    cell_edge_m - 2.0 * hysteresis_frac * cell_edge_m
}

/// Spatial configuration for the [`OrrerySpatialPlugin`](crate::OrrerySpatialPlugin).
///
/// Defaults are the D16 parameter-table values
/// (docs/adr/0016-parameter-reference.md).
#[derive(Debug, Clone, Resource)]
pub struct SpatialConfig {
    /// Interest-cell edge in metres. Default 128.0 (D16).
    pub cell_edge_m: f32,
    /// Hysteresis margin as a fraction of the cell edge. Default 0.10 (D16).
    pub hysteresis_frac: f32,
    /// Bounded high-rate interest set size. Default 24 entities (D16).
    pub high_rate_cap: usize,
    /// Proxy extrapolation rate range in Hz. Default 1.0..=4.0 (D16).
    pub proxy_hz: RangeInclusive<f32>,
}

impl Default for SpatialConfig {
    fn default() -> Self {
        Self {
            cell_edge_m: DEFAULT_CELL_EDGE_M as f32,
            hysteresis_frac: 0.10,
            high_rate_cap: 24,
            proxy_hz: 1.0..=4.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairwise_aoi_radius_reserves_both_commitment_margins() {
        let edge_m = 512.0;
        let hysteresis_frac = 0.10;
        let margin_m = hysteresis_frac * edge_m;

        assert_eq!(
            pairwise_aoi_radius_m(edge_m, hysteresis_frac),
            edge_m - 2.0 * margin_m
        );
    }
}
