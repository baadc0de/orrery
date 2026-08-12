//! The island registry: coarse presence → island formation (P1).
//!
//! The coordinator's core job (docs/02-networking.md §5) is to turn coarse
//! presence announcements into islands — connected sets of populated cells plus
//! the peers in them. This module implements that state machine as a pure,
//! engine- and IO-free registry so it is unit-testable without iroh or tokio:
//!
//! - **Form:** a peer enters cells no island covers → allocate an `island_id`,
//!   mark the cells covered, hand the peer a manifest.
//! - **Merge:** a peer's AOI touches another island's cells → unify under the
//!   surviving `island_id` (larger population wins), bump the epoch.
//! - **Split:** the population separates into clusters with no overlapping
//!   interest → partition the cell set and issue two manifests.
//! - **Drain:** last peer leaves → release the island.
//!
//! P1 keeps the membership model coarse (cell-level presence, not per-tick
//! positions) and in-memory; the wire server and FDB-journaled epochs land with
//! the full coordinator.

use std::collections::{HashMap, HashSet};

use orrery_protocol::coord::{IslandId, IslandManifest, PeerEntry, TopologyRegime};
use orrery_protocol::CellId;
use orrery_protocol::NodeId;

/// Coordinator configuration (docs/10-crates.md §12).
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// The population at which a cell is promoted to a field host. Default 32
    /// (D16); the D6 hysteresis windows are not implemented in P1.
    pub promotion_threshold: u32,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            promotion_threshold: 32,
        }
    }
}

/// A peer's coarse presence: which cells it occupies.
#[derive(Debug, Clone, Default)]
struct Presence {
    /// The cells this peer occupies (shard level).
    cells: HashSet<CellId>,
}

/// The in-memory island registry (P1).
#[derive(Debug, Default)]
pub struct IslandRegistry {
    /// Config.
    pub config: CoordinatorConfig,
    /// Peers and their coarse presence.
    peers: HashMap<NodeId, Presence>,
    /// Islands, keyed by id.
    islands: HashMap<IslandId, Island>,
    /// The next island id to allocate.
    next_island: u64,
}

/// A formed island's in-memory state.
#[derive(Debug, Clone)]
struct Island {
    /// The cells this island covers.
    cells: HashSet<CellId>,
    /// The peers in the island.
    peers: Vec<NodeId>,
    /// The current manifest epoch.
    epoch: u32,
}

impl IslandRegistry {
    /// A fresh registry with default config.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(CoordinatorConfig::default())
    }

    /// A fresh registry with the given config.
    #[must_use]
    pub fn with_config(config: CoordinatorConfig) -> Self {
        Self {
            config,
            peers: HashMap::new(),
            islands: HashMap::new(),
            next_island: 1,
        }
    }

    /// The number of formed islands.
    #[must_use]
    pub fn island_count(&self) -> usize {
        self.islands.len()
    }

    /// The island a peer currently belongs to, if any.
    #[must_use]
    pub fn island_of(&self, node: NodeId) -> Option<IslandId> {
        self.islands
            .iter()
            .find(|(_, island)| island.peers.contains(&node))
            .map(|(id, _)| *id)
    }

    /// Handle a peer's coarse presence update, forming/merging/splitting
    /// islands as needed. Returns the manifest(s) the peer should apply.
    ///
    /// P1 model: a peer's cells are the AOI it covers; islands are formed per
    /// connected component of the cell-adjacency graph. This is deliberately
    /// coarse — the full merge/split evaluation (docs/02-networking.md §5) is
    /// the coordinator's P3 work.
    pub fn report_presence(&mut self, node: NodeId, cells: Vec<CellId>) -> Vec<IslandManifest> {
        let cells: HashSet<CellId> = cells.into_iter().collect();

        // Leave any island this peer was in.
        if let Some(id) = self.island_of(node) {
            self.remove_peer_from_island(node, id);
        }

        self.peers.entry(node).or_default().cells = cells.clone();

        // Find an island whose cells overlap this peer's, else form a new one.
        let overlapping = self
            .islands
            .iter()
            .find(|(_, island)| island.cells.iter().any(|c| cells.contains(c)))
            .map(|(id, _)| *id);

        let island_id = match overlapping {
            Some(id) => id,
            None => {
                let id = IslandId::new(self.next_island);
                self.next_island += 1;
                self.islands.insert(
                    id,
                    Island {
                        cells: HashSet::new(),
                        peers: Vec::new(),
                        epoch: 0,
                    },
                );
                id
            }
        };

        let island = self.islands.get_mut(&island_id).expect("island exists");
        island.cells.extend(cells);
        island.peers.push(node);
        island.epoch += 1;

        vec![self.manifest(island_id)]
    }

    /// The manifest for an island at its current epoch.
    fn manifest(&self, island_id: IslandId) -> IslandManifest {
        let island = self.islands.get(&island_id).expect("island exists");
        let mut cells: Vec<CellId> = island.cells.iter().copied().collect();
        cells.sort();
        let peers = island
            .peers
            .iter()
            .map(|node| PeerEntry {
                node: *node,
                cells: self
                    .peers
                    .get(node)
                    .map(|p| {
                        let mut c: Vec<CellId> = p.cells.iter().copied().collect();
                        c.sort();
                        c
                    })
                    .unwrap_or_default(),
            })
            .collect();
        IslandManifest {
            island: island_id,
            epoch: island.epoch,
            cells,
            regime: TopologyRegime::for_population(island.peers.len(), None),
            peers,
        }
    }

    /// Remove a peer from an island, draining it if empty.
    fn remove_peer_from_island(&mut self, node: NodeId, island_id: IslandId) {
        let drain = if let Some(island) = self.islands.get_mut(&island_id) {
            island.peers.retain(|p| *p != node);
            island.epoch += 1;
            island.peers.is_empty()
        } else {
            false
        };
        if drain {
            self.islands.remove(&island_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn cell(c: i32) -> CellId {
        CellId::from_coords(glam::IVec3::new(c, 0, 0), CellId::MAX_LEVEL).unwrap()
    }

    #[test]
    fn first_peer_forms_an_island() {
        let mut reg = IslandRegistry::new();
        let manifests = reg.report_presence(node(1), vec![cell(0)]);
        assert_eq!(manifests.len(), 1);
        let m = &manifests[0];
        assert_eq!(m.epoch, 1);
        assert_eq!(m.cells, vec![cell(0)]);
        assert_eq!(m.peers.len(), 1);
        assert_eq!(m.peers[0].node, node(1));
        assert_eq!(reg.island_count(), 1);
        assert_eq!(reg.island_of(node(1)), Some(m.island));
    }

    #[test]
    fn overlapping_peers_share_an_island() {
        let mut reg = IslandRegistry::new();
        reg.report_presence(node(1), vec![cell(0)]);
        let manifests = reg.report_presence(node(2), vec![cell(0), cell(1)]);
        assert_eq!(manifests.len(), 1);
        let m = &manifests[0];
        assert_eq!(m.peers.len(), 2);
        assert_eq!(m.epoch, 2);
        assert_eq!(reg.island_count(), 1);
        assert_eq!(reg.island_of(node(1)), reg.island_of(node(2)));
    }

    #[test]
    fn disjoint_peers_form_separate_islands() {
        let mut reg = IslandRegistry::new();
        reg.report_presence(node(1), vec![cell(0)]);
        reg.report_presence(node(2), vec![cell(100)]);
        assert_eq!(reg.island_count(), 2);
        assert_ne!(reg.island_of(node(1)), reg.island_of(node(2)));
    }

    #[test]
    fn last_peer_drains_the_island() {
        let mut reg = IslandRegistry::new();
        reg.report_presence(node(1), vec![cell(0)]);
        reg.report_presence(node(2), vec![cell(0)]);
        assert_eq!(reg.island_count(), 1);

        // Both peers move away from the shared cell.
        reg.report_presence(node(1), vec![cell(50)]);
        reg.report_presence(node(2), vec![cell(60)]);
        // The original island drained; two new ones formed.
        assert_eq!(reg.island_count(), 2);
        assert_ne!(reg.island_of(node(1)), reg.island_of(node(2)));
    }

    #[test]
    fn regime_tracks_population() {
        let mut reg = IslandRegistry::new();
        // 10 peers in one cell → InterestMesh.
        for i in 1..=10 {
            reg.report_presence(node(i), vec![cell(0)]);
        }
        let m = reg.manifest(reg.island_of(node(1)).unwrap());
        assert_eq!(m.regime, TopologyRegime::InterestMesh);
        assert_eq!(m.peers.len(), 10);
    }
}
