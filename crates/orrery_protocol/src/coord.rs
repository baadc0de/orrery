//! Coordinator wire surface (docs/10-crates.md §12, docs/02-networking.md §3).
//!
//! The coordinator (`orrery_coordinator`) is a Bevy-free binary; peers speak to
//! it over iroh. The message set and the island/topology types it carries are
//! defined here, engine-agnostic, so both the Bevy-free coordinator and the
//! Bevy `orrery_net` plugin share one wire surface.

use serde::{Deserialize, Serialize};

use crate::CellId;
use crate::NodeId;

/// A coordinator-allocated island identifier (docs/02-networking.md §3).
///
/// An island is one replication session: a connected set of populated cells
/// plus the peers in them (D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IslandId(pub u64);

impl IslandId {
    /// An island id from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

impl core::fmt::Display for IslandId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "island:{}", self.0)
    }
}

/// The topology regime of an island (D6, docs/02-networking.md §6).
///
/// - [`Mesh`](TopologyRegime::Mesh): ≤ 8 peers, full mesh.
/// - [`InterestMesh`](TopologyRegime::InterestMesh): 9–32 peers, partial mesh
///   with the bounded high-rate set and 1–4 Hz proxies.
/// - [`Promoted`](TopologyRegime::Promoted): > 32 sustained, a coordinator-
///   spawned field host holds cell-entity authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopologyRegime {
    /// Full mesh, ≤ 8 peers.
    Mesh,
    /// Interest mesh, 9–32 peers.
    InterestMesh,
    /// Coordinator-spawned field host, > 32 sustained.
    Promoted {
        /// The field host's NodeId.
        host: NodeId,
    },
}

impl TopologyRegime {
    /// The population threshold at which an island leaves the full-mesh regime.
    pub const MESH_MAX: usize = 8;
    /// The population threshold at which an island enters the promoted regime.
    pub const INTEREST_MAX: usize = 32;

    /// The regime for a population, given the optional promoted host.
    #[must_use]
    pub fn for_population(pop: usize, host: Option<NodeId>) -> Self {
        match host {
            Some(host) => Self::Promoted { host },
            None if pop <= Self::MESH_MAX => Self::Mesh,
            None => Self::InterestMesh,
        }
    }
}

/// A peer's entry in an island manifest (docs/02-networking.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEntry {
    /// The peer's NodeId.
    pub node: NodeId,
    /// The cells this peer occupies.
    pub cells: Vec<CellId>,
}

/// An island manifest: the coordinator's membership handout (D12).
///
/// Epochs make manifests idempotent — a peer applies only monotonically newer
/// manifests (docs/02-networking.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IslandManifest {
    /// The coordinator-allocated island id.
    pub island: IslandId,
    /// Bumped on any membership/topology change.
    pub epoch: u32,
    /// The populated cells this island covers.
    pub cells: Vec<CellId>,
    /// The topology regime.
    pub regime: TopologyRegime,
    /// The peers in the island (excluding the local peer).
    pub peers: Vec<PeerEntry>,
}

/// A coordinator message (docs/10-crates.md §12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordMsg {
    /// Peer join: authenticate and report coarse presence.
    Hello {
        /// The session token from `orrery_identity` login.
        token: Vec<u8>,
        /// The peer's NodeId.
        node: NodeId,
    },
    /// Coarse presence update (shard level, rate-limited).
    Presence {
        /// The peer's coarse cell.
        cell: CellId,
    },
    /// Island membership handout.
    IslandAssignment {
        /// The manifest.
        manifest: IslandManifest,
    },
    /// Drain an island: leases released, cells parked.
    Drain {
        /// The island to drain.
        island: IslandId,
        /// Drain deadline as unix milliseconds.
        deadline: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn regime_thresholds() {
        assert_eq!(
            TopologyRegime::for_population(0, None),
            TopologyRegime::Mesh
        );
        assert_eq!(
            TopologyRegime::for_population(8, None),
            TopologyRegime::Mesh
        );
        assert_eq!(
            TopologyRegime::for_population(9, None),
            TopologyRegime::InterestMesh
        );
        assert_eq!(
            TopologyRegime::for_population(32, None),
            TopologyRegime::InterestMesh
        );
        assert_eq!(
            TopologyRegime::for_population(33, None),
            TopologyRegime::InterestMesh
        );
        // A promoted host overrides population.
        assert_eq!(
            TopologyRegime::for_population(4, Some(node(1))),
            TopologyRegime::Promoted { host: node(1) }
        );
    }

    #[test]
    fn manifest_roundtrips() {
        let manifest = IslandManifest {
            island: IslandId::new(7),
            epoch: 3,
            cells: vec![CellId::ROOT],
            regime: TopologyRegime::Mesh,
            peers: vec![PeerEntry {
                node: node(1),
                cells: vec![CellId::ROOT],
            }],
        };
        let bytes = postcard::to_stdvec(&manifest).unwrap();
        let back: IslandManifest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, manifest);
    }
}
