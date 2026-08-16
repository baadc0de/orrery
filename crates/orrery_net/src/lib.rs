//! Orrery session lifecycle (P0 skeleton + P1 island membership).
//!
//! Owns everything about being *on the network* that is not replication:
//! bootstrapping the endpoint via `orrery_aeronet_iroh`, peer connect/
//! disconnect tracking, channel policy (datagrams = state, streams =
//! control/bulk — D3), relay-path telemetry aggregation, island membership, and
//! the coordinator client that drives it (docs/11-roadmap.md §P0, §P1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod budget;
pub mod channels;
pub mod coordinator;
pub mod island;
pub mod peer_link;
pub mod plugin;

/// The async runtime the IO layer and the coordinator session share.
///
/// Re-exported because a host that already owns a runtime should hand its
/// handle in — `IrohRuntime::from(handle)` — rather than let the plugin create
/// and leak a second one.
pub use aeronet_iroh::IrohRuntime;
pub use budget::{Bandwidth, UploadBudget, UploadMeter};
pub use coordinator::{
    ActiveInterest, CoordinatorConfig, CoordinatorLink, CoordinatorPlugin, LinkStatus,
};
pub use island::{IslandMembership, IslandSource, NetEvent};
pub use orrery_protocol::coord::{IslandId, TopologyRegime};
pub use peer_link::{PeerLinkCounters, PeerPacket, SendPacket, StreamMode};
pub use plugin::OrreryNetPlugin;
