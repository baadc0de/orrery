//! Orrery spatial model (P1 core).
//!
//! `CellId` assignment from big_space grid coordinates, the 27-cell AOI
//! subscription, mapping cell membership onto bevy_replicon per-client
//! visibility, the bounded high-rate interest-set selection (24 entities),
//! cell-crossing hysteresis (the 10% overlap zone), and 1-4 Hz extrapolated
//! proxies. The engine-independent `CellId` encoding lives in `orrery_protocol`;
//! this crate wires it to Bevy.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cell;
pub mod config;
pub mod hysteresis;
pub mod interest;
pub mod plugin;
pub mod visibility;

pub use config::SpatialConfig;
pub use hysteresis::{step_commit, GridPosition};
pub use interest::{HighRate, InterestSelection, Proxy};
pub use plugin::OrrerySpatialPlugin;
pub use visibility::AoiVisibilityPlugin;
