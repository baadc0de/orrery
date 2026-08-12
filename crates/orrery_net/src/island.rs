//! Island membership lifecycle (P1, docs/11-roadmap.md §P1).
//!
//! An island is one replication session: a connected set of populated cells
//! plus the peers in them (docs/02-networking.md §5). This module tracks the
//! local peer's island membership — which island it belongs to, which peers are
//! in it, and which topology regime it is in — and emits [`NetEvent`]s on
//! membership changes.
//!
//! P1 scope: the *client-side* membership state and lifecycle. The coordinator
//! (`orrery_coordinator`) is a stub that hands out islands and NodeIds; the
//! wire protocol that drives this state lives in `orrery_protocol::coord`.

use bevy_ecs::prelude::*;

use orrery_protocol::coord::{IslandId, TopologyRegime};
use orrery_protocol::NodeId;

/// The local peer's island membership (D6).
#[derive(Debug, Clone, Resource)]
pub struct IslandMembership {
    /// The island this peer belongs to, if any.
    pub island: Option<IslandId>,
    /// The peers in the island (excluding the local peer).
    pub peers: Vec<NodeId>,
    /// The topology regime of the island.
    pub regime: TopologyRegime,
}

impl Default for IslandMembership {
    fn default() -> Self {
        Self {
            island: None,
            peers: Vec::new(),
            regime: TopologyRegime::Mesh,
        }
    }
}

impl IslandMembership {
    /// Whether the local peer is a member of an island.
    #[must_use]
    pub fn is_member(&self) -> bool {
        self.island.is_some()
    }

    /// The number of peers in the island (excluding the local peer).
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Whether `node` is a peer in the current island.
    #[must_use]
    pub fn contains(&self, node: NodeId) -> bool {
        self.peers.contains(&node)
    }

    /// Apply a new island assignment, returning whether anything changed.
    ///
    /// This is the idempotent apply point: the coordinator's manifests carry an
    /// epoch, and only monotonically newer assignments should reach here
    /// (docs/02-networking.md §3). P1 keeps it simple — the caller decides when
    /// to apply; this just records the new state and reports the diff.
    pub fn assign(&mut self, island: IslandId, peers: Vec<NodeId>, regime: TopologyRegime) -> bool {
        let changed = self.island != Some(island) || self.peers != peers || self.regime != regime;
        self.island = Some(island);
        self.peers = peers;
        self.regime = regime;
        changed
    }

    /// Leave the current island (drain, disconnect, or coordinator eviction).
    pub fn leave(&mut self) {
        self.island = None;
        self.peers.clear();
        self.regime = TopologyRegime::Mesh;
    }
}

/// A membership lifecycle event (docs/10-crates.md §4).
#[derive(Debug, Clone, Message)]
pub enum NetEvent {
    /// A peer joined the island.
    PeerJoined {
        /// The joining peer.
        node: NodeId,
    },
    /// A peer left the island.
    PeerLeft {
        /// The leaving peer.
        node: NodeId,
    },
    /// The island membership or regime changed.
    IslandChanged {
        /// The new island, if any.
        island: Option<IslandId>,
        /// The new regime.
        regime: TopologyRegime,
    },
}

/// Reconciles the [`IslandMembership`] resource against the connected peers
/// and emits [`NetEvent`]s on changes.
///
/// P1 skeleton: the membership is driven by the coordinator handout (stub) and
/// by the connected-session set. This system keeps the peer list and regime in
/// sync with what the network layer actually has, so downstream systems
/// (replication, prediction) read one source of truth.
pub fn update_island_membership(
    mut membership: ResMut<IslandMembership>,
    mut events: MessageWriter<NetEvent>,
    peers: Query<&crate::plugin::Peer>,
) {
    let connected: Vec<NodeId> = peers.iter().map(|p| p.id).collect();

    // Regime follows population (no promoted host in P1).
    let regime = TopologyRegime::for_population(connected.len(), None);

    // Diff the peer set against the current membership.
    let mut joined = Vec::new();
    let mut left = Vec::new();
    for node in &connected {
        if !membership.contains(*node) {
            joined.push(*node);
        }
    }
    for node in &membership.peers {
        if !connected.contains(node) {
            left.push(*node);
        }
    }

    let regime_changed = membership.regime != regime;
    let peers_changed = !joined.is_empty() || !left.is_empty();

    if peers_changed {
        membership.peers = connected;
        membership.regime = regime;
        for node in joined {
            events.write(NetEvent::PeerJoined { node });
        }
        for node in left {
            events.write(NetEvent::PeerLeft { node });
        }
    } else if regime_changed {
        membership.regime = regime;
    }

    if regime_changed || peers_changed {
        events.write(NetEvent::IslandChanged {
            island: membership.island,
            regime: membership.regime,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh::SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn assign_and_leave() {
        let mut m = IslandMembership::default();
        assert!(!m.is_member());
        let changed = m.assign(
            IslandId::new(7),
            vec![node(1), node(2)],
            TopologyRegime::Mesh,
        );
        assert!(changed);
        assert!(m.is_member());
        assert_eq!(m.peer_count(), 2);
        assert!(m.contains(node(1)));
        // Re-assigning the same state is a no-op.
        let changed = m.assign(
            IslandId::new(7),
            vec![node(1), node(2)],
            TopologyRegime::Mesh,
        );
        assert!(!changed);
        m.leave();
        assert!(!m.is_member());
        assert!(m.peers.is_empty());
    }

    #[test]
    fn membership_tracks_peers() {
        let mut m = IslandMembership::default();
        m.assign(
            IslandId::new(1),
            vec![node(1), node(2)],
            TopologyRegime::Mesh,
        );
        assert!(m.contains(node(2)));
        assert!(!m.contains(node(3)));
    }
}
