//! Orrery spatial model (P1 core).
//!
//! `CellId` assignment from big_space grid coordinates, the 27-cell AOI
//! subscription, mapping cell membership onto bevy_replicon per-client
//! visibility, high-rate interest-set selection, cell-crossing hysteresis, and
//! 1-4 Hz extrapolated proxies. The engine-independent `CellId` encoding lives
//! in `orrery_protocol`; this crate wires it to Bevy.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cell;
pub mod config;
pub mod plugin;
pub mod visibility;

pub use config::SpatialConfig;
pub use plugin::OrrerySpatialPlugin;
pub use visibility::AoiVisibilityPlugin;
