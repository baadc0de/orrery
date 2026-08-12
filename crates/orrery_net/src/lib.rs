//! Orrery session lifecycle (P0 skeleton).
//!
//! Owns everything about being *on the network* that is not replication:
//! bootstrapping the endpoint via `orrery_aeronet_iroh`, peer connect/
//! disconnect tracking, channel policy (datagrams = state, streams =
//! control/bulk — D3), and relay-path telemetry aggregation. This is the
//! minimal P0 skeleton; the coordinator client and island membership land with
//! P1 (docs/11-roadmap.md §P0, §P1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod channels;
pub mod plugin;

pub use plugin::OrreryNetPlugin;
