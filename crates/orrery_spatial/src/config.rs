//! Spatial configuration (D16 defaults).

use std::ops::RangeInclusive;

use bevy_ecs::prelude::*;

use orrery_protocol::DEFAULT_CELL_EDGE_M;

/// Spatial configuration for the [`OrrerySpatialPlugin`](crate::OrrerySpatialPlugin).
///
/// Defaults are the D16 parameter-table values (docs/DECISIONS.md D16).
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
