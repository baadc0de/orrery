//! Orrery coordinator — islands and orchestration (P1 stub).
//!
//! **Bevy-free** binary (D15). Coarse presence tracking, island
//! form/merge/split/drain, and `NodeId` handout for island bootstrap
//! (docs/10-crates.md §12). State is in-memory and reconstructible from
//! presence announcements; witness-seed epochs and island generation counters
//! are durably journaled to FDB behind the `fdb-state` feature (not yet
//! implemented in P1).
//!
//! P1 scope: the coordinator *logic* — the island-formation state machine and
//! the manifest construction — is implemented here as a pure, testable library
//! (`IslandRegistry`). The iroh/tokio wire server that drives it with real
//! `CoordMsg`s lands with the full coordinator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod registry;

pub use registry::{CoordinatorConfig, IslandRegistry};
