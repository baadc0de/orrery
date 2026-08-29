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
//! - **Drain:** last peer leaves → release the island, and tell the peer
//!   whose departure did it, so the retirement is an event and not just a
//!   missing hash-map entry.
//!
//! P1 keeps the membership model coarse (cell-level presence, not per-tick
//! positions) and in-memory; the wire server and FDB-journaled epochs land with
//! the full coordinator.

use std::collections::{HashMap, HashSet};

use orrery_protocol::coord::{
    CoordinatorInterestSnapshot, InterestGrantClaimsV1, IslandId, IslandManifest, PeerEntry,
    TopologyRegime,
};
use orrery_protocol::{CellId, Epoch, GridId, InterestCellCrossing, NodeId, SeqPair, Tick};

/// Coordinator configuration (docs/10-crates.md §12).
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// The population at which a cell is promoted to a field host. Default 32
    /// (D16); the D6 hysteresis windows are not implemented in P1.
    pub promotion_threshold: u32,
    /// Lifetime stamped on issued interest grants, in milliseconds.
    ///
    /// A grant is a *lease on being believed*: short enough that a peer which
    /// stops reporting presence stops being able to claim authority, long
    /// enough to survive ordinary presence jitter. The default is one minute,
    /// six presence intervals at the D16 cadence.
    pub interest_grant_ttl_ms: u64,
    /// Grace stamped on a drain order, in milliseconds past the coordinator's
    /// wall clock.
    ///
    /// The default is D7's **10 s** lease TTL, and that number is not a
    /// coincidence: the drain finishes when the island's authority leases are
    /// released or expire, and a lease nobody heartbeats expires in exactly
    /// one TTL. A shorter grace would order a peer to be done before the
    /// registrar could agree it was; a longer one would keep cells nominally
    /// live after every lease over them had already lapsed.
    pub drain_grace_ms: u64,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            promotion_threshold: 32,
            interest_grant_ttl_ms: 60_000,
            drain_grace_ms: 10_000,
        }
    }
}

/// An island retired because its last peer left (docs/02-networking.md §5).
///
/// The registry used to drain an island by deleting a hash-map entry, which
/// made the event unobservable: nothing downstream could say *which* island
/// had gone, or over which cells. This is that deletion made explicit, so the
/// server can put a [`CoordMsg::Drain`](orrery_protocol::CoordMsg::Drain) on
/// the wire before the record stops existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandDrain {
    /// The island that lost its last peer.
    pub island: IslandId,
    /// The cells it covered, sorted — the set now parked (D7): no live
    /// authority, state served from the hot tier.
    pub cells: Vec<CellId>,
    /// The roster the island held at its last populated epoch.
    ///
    /// Today that is exactly the one peer whose departure emptied it, and it
    /// is the addressee of the drain order. It is a list rather than a single
    /// `NodeId` because the island is what drains, not the peer: a
    /// coordinator-initiated evacuation, if one is ever decided on, would
    /// retire an island with several names still in it.
    pub peers: Vec<NodeId>,
}

/// Everything a membership change owes the peers it touched.
///
/// Manifests and drains travel together because they are two halves of one
/// answer: a peer that moves between islands needs the roster of the island it
/// joined *and* — if that island emptied behind it — the order retiring the
/// one it left. Splitting them across two calls would let a caller ship one
/// and forget the other, which is how the drain went unobserved in the first
/// place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MembershipChange {
    /// Manifests whose islands survive and whose rosters changed. Every peer
    /// a manifest names must receive it, not just the reporter.
    pub manifests: Vec<IslandManifest>,
    /// Islands this change retired.
    pub drains: Vec<IslandDrain>,
}

/// A peer's coarse presence: which cells it occupies.
#[derive(Debug, Clone, Default)]
struct Presence {
    /// The cells this peer occupies (shard level).
    cells: HashSet<CellId>,
    /// Monotonic epoch, bumped whenever `cells` changes.
    ///
    /// A gateway keeps the highest epoch it has seen per peer, so this is what
    /// stops a peer replaying a stale, wider grant after moving away.
    interest_epoch: u32,
    /// Last committed cell learned from an immediate crossing.
    ///
    /// Bulk presence predates the explicit centre and therefore cannot fill
    /// this in. Once a crossing establishes it, later crossing events must
    /// chain from it so reordering cannot walk the roster backwards.
    committed_cell: Option<CellId>,
    /// Ordering fence for the last immediate crossing applied.
    last_crossing: Option<(SeqPair, Tick)>,
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

    /// Every peer whose reported presence covers `cell`, in byte order.
    ///
    /// This is the candidate pool a witness epoch is drawn from (D28), and it
    /// is deliberately taken from **presence** rather than from island
    /// membership: docs/07 §4.1 says the pool is the entity's interest set,
    /// and two islands whose peers both cover a cell are one cell's
    /// population, not two. Unioning here is the "one pool per cell-epoch"
    /// reading D28 leaves open, and it is the only reading under which the
    /// witnesses of a cell are chosen by the people actually in it.
    ///
    /// Sorted bytewise so the caller gets a canonical set rather than
    /// whatever order a `HashMap` iterated in — the draw is a function of the
    /// set, and this is where that starts.
    #[must_use]
    pub fn peers_covering(&self, cell: CellId) -> Vec<NodeId> {
        let mut covering: Vec<NodeId> = self
            .peers
            .iter()
            .filter(|(_, presence)| presence.cells.contains(&cell))
            .map(|(node, _)| *node)
            .collect();
        covering.sort_by_key(|node| *node.as_bytes());
        covering
    }

    /// Whether `cell` is a valid source for `node`'s next crossing.
    ///
    /// Before the first event, bulk presence can establish only that the source
    /// is covered. Afterwards, events must form an exact committed-cell chain;
    /// this rejects a delayed crossing even when its old source remains inside
    /// the wider swept set.
    #[must_use]
    pub fn accepts_crossing(&self, node: NodeId, crossing: &InterestCellCrossing) -> bool {
        self.peers.get(&node).is_some_and(|presence| {
            if !presence.cells.contains(&crossing.from)
                || presence
                    .last_crossing
                    .is_some_and(|(known_seq, known_tick)| {
                        crossing.seq <= known_seq || crossing.tick <= known_tick
                    })
            {
                return false;
            }
            // Consecutive crossing sequences must chain exactly. A larger gap
            // may mean an event was lost; bulk presence repairs the set, and
            // coverage of `from` is then the recovery proof.
            presence.last_crossing.is_none_or(|(known_seq, _)| {
                crossing.seq.own_seq != known_seq.own_seq
                    || crossing.seq.auth_seq != known_seq.auth_seq.saturating_add(1)
                    || presence.committed_cell == Some(crossing.from)
            })
        })
    }

    /// Record post-crossing coverage and remember the new committed cell.
    pub fn report_crossing(
        &mut self,
        node: NodeId,
        crossing: &InterestCellCrossing,
        cells: Vec<CellId>,
    ) -> MembershipChange {
        let change = self.report_presence(node, cells);
        self.peers
            .get_mut(&node)
            .expect("report_presence records the peer")
            .committed_cell = Some(crossing.to);
        self.peers
            .get_mut(&node)
            .expect("report_presence records the peer")
            .last_crossing = Some((crossing.seq, crossing.tick));
        change
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
    /// islands as needed. Returns the manifest(s) to push and any island the
    /// move drained.
    ///
    /// P1 model: a peer's cells are the AOI it covers; islands are formed per
    /// connected component of the cell-adjacency graph. This is deliberately
    /// coarse — the full merge/split evaluation (docs/02-networking.md §5) is
    /// the coordinator's P3 work.
    ///
    /// A *move* returns two manifests: the island joined, and the island left
    /// when that one still has somebody in it. Returning only the joined one —
    /// which is what this did — left the survivors of the vacated island
    /// holding a roster that still named the mover, and blind to the epoch
    /// bump its departure made. [`Self::forget_peer`] has always closed that
    /// hole for a disconnect; to the roster left behind, a move is not a
    /// different kind of departure.
    pub fn report_presence(&mut self, node: NodeId, cells: Vec<CellId>) -> MembershipChange {
        let cells: HashSet<CellId> = cells.into_iter().collect();

        // Leave any island this peer was in — and remember which, because a
        // survivor of it is owed the new roster and a drained one is owed an
        // order. Neither is knowable once the peer is somewhere else.
        let mut vacated = None;
        let mut drains = Vec::new();
        if let Some(id) = self.island_of(node) {
            match self.remove_peer_from_island(node, id) {
                Some(drain) => drains.push(drain),
                None => vacated = Some(id),
            }
        }

        {
            let presence = self.peers.entry(node).or_default();
            if presence.cells != cells {
                presence.cells = cells.clone();
                presence.interest_epoch = presence.interest_epoch.saturating_add(1);
            }
        }

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

        let mut manifests = vec![self.manifest(island_id)];
        // A peer whose new cells still overlap its old island rejoins the one
        // it just left; the manifest above already describes it, and sending
        // it twice would only invite a peer to act on the staler copy.
        if let Some(vacated) = vacated.filter(|id| *id != island_id) {
            manifests.push(self.manifest(vacated));
        }
        MembershipChange { manifests, drains }
    }

    /// Drop a peer entirely: its presence, and its place in any island.
    ///
    /// Returns the manifests the *remaining* peers must apply, or the drain if
    /// there are none left. A departed peer left in a roster is worse than a
    /// missing one — survivors would keep trying to reach a ghost.
    pub fn forget_peer(&mut self, node: NodeId) -> MembershipChange {
        let island = self.island_of(node);
        self.peers.remove(&node);
        let Some(island) = island else {
            return MembershipChange::default();
        };
        match self.remove_peer_from_island(node, island) {
            // A drained island has no manifest left to send — only the order
            // retiring it, which is the whole reason the drain is returned
            // rather than swallowed.
            Some(drain) => MembershipChange {
                manifests: Vec::new(),
                drains: vec![drain],
            },
            None => MembershipChange {
                manifests: vec![self.manifest(island)],
                drains: Vec::new(),
            },
        }
    }

    /// Mint the interest claims authorizing `node`'s current coverage.
    ///
    /// Returns `None` for a peer with no reported presence: there is nothing
    /// to authorize, and a grant covering no cells is refused by verifiers
    /// anyway. The cells are sorted so the same presence always produces the
    /// same bytes, which keeps a re-issued grant diffable in a log.
    #[must_use]
    pub fn interest_claims(
        &self,
        node: NodeId,
        grid: GridId,
        issuer_key_id: orrery_protocol::IssuerKeyId,
    ) -> Option<InterestGrantClaimsV1> {
        let presence = self.peers.get(&node)?;
        if presence.cells.is_empty() {
            return None;
        }
        let mut covered_cells: Vec<CellId> = presence.cells.iter().copied().collect();
        covered_cells.sort();
        Some(InterestGrantClaimsV1::new(
            node,
            Epoch::new(u64::from(presence.interest_epoch)),
            grid,
            covered_cells,
            self.config.interest_grant_ttl_ms,
            issuer_key_id,
        ))
    }

    /// The unsigned snapshot a gateway would hold for `node`, for tests and
    /// in-process embeddings that skip the signature round trip.
    ///
    /// `accepted_at_ms` is the *consumer's* clock, not the coordinator's —
    /// same rule as a verified grant.
    #[must_use]
    pub fn interest_snapshot(
        &self,
        node: NodeId,
        grid: GridId,
        accepted_at_ms: u64,
    ) -> Option<CoordinatorInterestSnapshot> {
        self.interest_claims(node, grid, orrery_protocol::IssuerKeyId::new(0))
            .map(|claims| CoordinatorInterestSnapshot::from_grant(claims, accepted_at_ms))
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

    /// Remove a peer from an island, draining it if that emptied it.
    ///
    /// Returns the drain when the departure was the last one. The cells and
    /// the final roster are captured *here* because this is the last moment
    /// they exist: the `remove` below is the entirety of what "drain" has
    /// meant so far, and after it nothing downstream can reconstruct what was
    /// retired. Returning `None` is not "nothing happened" — the surviving
    /// island's epoch has still been bumped, and its roster still owes every
    /// remaining peer a manifest.
    fn remove_peer_from_island(
        &mut self,
        node: NodeId,
        island_id: IslandId,
    ) -> Option<IslandDrain> {
        let island = self.islands.get_mut(&island_id)?;
        let last_roster = island.peers.clone();
        island.peers.retain(|p| *p != node);
        island.epoch += 1;
        if !island.peers.is_empty() {
            return None;
        }
        let island = self.islands.remove(&island_id)?;
        let mut cells: Vec<CellId> = island.cells.into_iter().collect();
        cells.sort();
        Some(IslandDrain {
            island: island_id,
            cells,
            peers: last_roster,
        })
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
        let change = reg.report_presence(node(1), vec![cell(0)]);
        assert_eq!(change.manifests.len(), 1);
        assert!(change.drains.is_empty());
        let m = &change.manifests[0];
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
        let change = reg.report_presence(node(2), vec![cell(0), cell(1)]);
        assert_eq!(change.manifests.len(), 1);
        let m = &change.manifests[0];
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

        let original = reg.island_of(node(1)).expect("shared island");

        // Both peers move away from the shared cell. The first move leaves a
        // survivor behind, so it drains nothing; the second empties the island.
        let first = reg.report_presence(node(1), vec![cell(50)]);
        assert!(
            first.drains.is_empty(),
            "an island with a peer still in it has not drained"
        );
        let second = reg.report_presence(node(2), vec![cell(60)]);

        // The departure that emptied the island says so, and says what it
        // retired: without this the drain is only an absence, and nothing on
        // the wire could name the island whose cells are now parked.
        assert_eq!(second.drains.len(), 1);
        let drain = &second.drains[0];
        assert_eq!(drain.island, original);
        assert_eq!(drain.cells, vec![cell(0)]);
        assert_eq!(drain.peers, vec![node(2)]);

        // The original island drained; two new ones formed.
        assert_eq!(reg.island_count(), 2);
        assert!(reg.island_of(node(2)) != Some(original));
        assert_ne!(reg.island_of(node(1)), reg.island_of(node(2)));
    }

    #[test]
    fn a_move_tells_the_island_left_behind_as_well_as_the_one_joined() {
        // Given: two peers sharing an island, and a third island elsewhere is
        // where one of them is headed.
        let mut reg = IslandRegistry::new();
        reg.report_presence(node(1), vec![cell(0)]);
        reg.report_presence(node(2), vec![cell(0)]);
        let vacated = reg.island_of(node(1)).expect("shared island");

        // When: one of them moves out of range of the other.
        let change = reg.report_presence(node(1), vec![cell(100)]);

        // Then: both rosters come back. The island left behind survives with
        // one peer, and a survivor that never hears about the bump keeps
        // reaching for a peer that is no longer in its session.
        assert!(change.drains.is_empty());
        assert_eq!(change.manifests.len(), 2);
        let joined = &change.manifests[0];
        let left = &change.manifests[1];
        assert_eq!(reg.island_of(node(1)), Some(joined.island));
        assert_eq!(left.island, vacated);
        assert_eq!(left.peers.len(), 1);
        assert_eq!(left.peers[0].node, node(2));
        assert!(
            left.peers.iter().all(|entry| entry.node != node(1)),
            "the island left behind must not still list the mover"
        );
        assert!(
            left.epoch > 2,
            "the departure bumps the vacated island's epoch past the join that formed it"
        );
    }

    #[test]
    fn a_move_within_one_island_yields_one_manifest() {
        // A peer whose new cells still overlap its old island rejoins the one
        // it just left. The vacated-island manifest would describe the same
        // island at a staler epoch, so it is not sent.
        let mut reg = IslandRegistry::new();
        reg.report_presence(node(1), vec![cell(0)]);
        reg.report_presence(node(2), vec![cell(0)]);
        let change = reg.report_presence(node(1), vec![cell(0), cell(1)]);
        assert_eq!(change.manifests.len(), 1);
        assert!(change.drains.is_empty());
        assert_eq!(reg.island_count(), 1);
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
