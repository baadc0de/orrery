//! Orrery coordinator — islands and orchestration (P1 stub).
//!
//! **Bevy-free** binary (D15). Coarse presence tracking, island
//! form/merge/split/drain, and `NodeId` handout for island bootstrap
//! (docs/10-crates.md §12). State is in-memory and reconstructible from
//! presence announcements.
//!
//! Witness-seed epochs are seeded here as of D28 ([`witness`]): the
//! coordinator draws each cell-epoch's set and signs an announcement that the
//! peers covering the cell courier onward. What is still in-memory is the
//! *durability* — the `fdb-state` feature's journaled epochs and island
//! generation counters are not implemented, and under D28 clause (f) the
//! durable `epoch/` row is written by the gateway that accepted an
//! announcement rather than by this process at all.
//!
//! P1 scope: the coordinator *logic* — the island-formation state machine and
//! the manifest construction — is implemented here as a pure, testable library
//! (`IslandRegistry`). The iroh/tokio wire server that drives it with real
//! `CoordMsg`s lands with the full coordinator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod interest;
pub mod registry;
pub mod server;
pub mod witness;

pub use client::{ClientError, CoordinatorClient};
pub use interest::{InterestCrossingError, InterestIssuer, IssuedInterestCrossing};
pub use registry::{CoordinatorConfig, IslandDrain, IslandRegistry, MembershipChange};
pub use server::{
    CoordinatorServer, CoordinatorStats, FeedFailure, ServerConfig, ServerError,
    SharedStandingInvalidationFeed, StandingInvalidationFeed, StrikesMode, StrikesPosture,
};
pub use witness::{SeedOutcome, SeededEpoch, WitnessEpochIssuer, WitnessSeedConfig, WitnessSeeder};
