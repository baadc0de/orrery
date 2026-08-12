//! Orrery session lifecycle (P0 skeleton + P1 island membership).
//!
//! Owns everything about being *on the network* that is not replication:
//! bootstrapping the endpoint via `orrery_aeronet_iroh`, peer connect/
//! disconnect tracking, channel policy (datagrams = state, streams =
//! control/bulk — D3), relay-path telemetry aggregation, and island membership
//! (docs/11-roadmap.md §P0, §P1). The coordinator *client* (dialing peers from
//! a handout) lands with the full coordinator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod channels;
pub mod island;
pub mod plugin;

pub use island::{IslandMembership, NetEvent};
pub use orrery_protocol::coord::{IslandId, TopologyRegime};
pub use plugin::OrreryNetPlugin;
