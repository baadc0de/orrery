//! Island membership lifecycle (P1, docs/11-roadmap.md §P1).
//!
//! An island is one replication session: a connected set of populated cells
//! plus the peers in them (docs/02-networking.md §5). This module tracks the
//! local peer's island membership — which island it belongs to, which peers are
//! in it, and which topology regime it is in — and emits [`NetEvent`]s on
//! membership changes.
//!
//! # Membership is a handout, not an observation
//!
//! The coordinator decides who is in an island; a peer does not infer it from
//! whoever happens to have dialled it. Those are different sets, and conflating
//! them inverts the direction of trust — a peer that opened a session would
//! have written itself into our island, and a manifest peer we had not reached
//! yet would be missing from it. So [`IslandMembership`] is driven by
//! [`IslandManifest`], and the connected-session set is what gets *reconciled
//! against* it.
//!
//! The one exception is [`IslandSource::ConnectedPeers`], used when no
//! coordinator is configured at all: a bare mesh with no authority to ask still
//! needs a peer list, and P0's transport tests run in exactly that shape.

use bevy_ecs::prelude::*;

use orrery_protocol::coord::{IslandId, IslandManifest, PeerEntry, TopologyRegime};
use orrery_protocol::NodeId;

/// Where the current [`IslandMembership`] came from.
///
/// Recorded rather than inferred, so a system reading membership can tell
/// coordinator truth from the coordinator-less fallback without consulting
/// config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IslandSource {
    /// No membership has been established yet.
    Unassigned,
    /// The coordinator's manifest handout (the normal path).
    Coordinator,
    /// Derived from the connected-session set, because no coordinator is
    /// configured. Never used while a coordinator link exists.
    ConnectedPeers,
}

/// The local peer's island membership (D6).
#[derive(Debug, Clone, Resource)]
pub struct IslandMembership {
    /// The island this peer belongs to, if any.
    pub island: Option<IslandId>,
    /// The manifest epoch this membership was built from.
    ///
    /// Meaningful only within one `island`: the coordinator keeps a counter per
    /// island and starts a newly formed one at 0
    /// (`orrery_coordinator::registry`).
    pub epoch: u32,
    /// The peers in the island (excluding the local peer), with the cells each
    /// occupies.
    pub peers: Vec<PeerEntry>,
    /// The topology regime of the island.
    pub regime: TopologyRegime,
    /// Where this membership came from.
    pub source: IslandSource,
}

impl Default for IslandMembership {
    fn default() -> Self {
        Self {
            island: None,
            epoch: 0,
            peers: Vec::new(),
            regime: TopologyRegime::Mesh,
            source: IslandSource::Unassigned,
        }
    }
}

/// Why a manifest was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleManifest {
    /// The island both manifests describe.
    pub island: IslandId,
    /// The epoch already applied.
    pub applied: u32,
    /// The epoch that arrived.
    pub offered: u32,
}

impl core::fmt::Display for StaleManifest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "manifest for island {} at epoch {} is not newer than the applied epoch {}",
            self.island.0, self.offered, self.applied
        )
    }
}

impl core::error::Error for StaleManifest {}

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
        self.peers.iter().any(|entry| entry.node == node)
    }

    /// The NodeIds of the island's peers.
    pub fn peer_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.peers.iter().map(|entry| entry.node)
    }

    /// Apply a coordinator manifest, returning the lifecycle events it caused.
    ///
    /// # Epoch gating
    ///
    /// Manifests ride unreliable datagrams (`CoordMsg` tagged
    /// `Channel::Control`), so they can arrive reordered or duplicated. A
    /// manifest is applied only if it names a *different* island or carries a
    /// strictly higher epoch than the one already applied. Without that, a
    /// delayed duplicate of an older manifest would resurrect peers that have
    /// since left, and the island would oscillate for as long as the reordering
    /// window lasts.
    ///
    /// An island change is always accepted, because island ids are reallocated
    /// on merge — the larger population's id survives — and the new island's
    /// epoch counter is unrelated to the old one's. Comparing across them would
    /// reject a legitimate move.
    ///
    /// # The local peer is filtered out here
    ///
    /// The coordinator broadcasts one manifest to every peer it names, so the
    /// roster on the wire *includes* the recipient — each peer removes itself.
    /// Leaving it in would report a peer count one too high and offer the local
    /// peer to the dialler as something to connect to.
    ///
    /// # Errors
    ///
    /// [`StaleManifest`] if the manifest is not newer than what is applied. That
    /// is an ordinary, expected outcome on a lossy link, not a fault.
    pub fn apply_manifest(
        &mut self,
        manifest: &IslandManifest,
        local: NodeId,
    ) -> Result<Vec<NetEvent>, StaleManifest> {
        if self.island == Some(manifest.island) && manifest.epoch <= self.epoch {
            return Err(StaleManifest {
                island: manifest.island,
                applied: self.epoch,
                offered: manifest.epoch,
            });
        }

        let others: Vec<PeerEntry> = manifest
            .peers
            .iter()
            .filter(|entry| entry.node != local)
            .cloned()
            .collect();

        let island_changed = self.island != Some(manifest.island);
        self.island = Some(manifest.island);
        self.epoch = manifest.epoch;
        self.source = IslandSource::Coordinator;
        Ok(self.replace_peers(&others, manifest.regime, island_changed))
    }

    /// Drive membership from the connected-session set.
    ///
    /// The coordinator-less path only ([`IslandSource::ConnectedPeers`]): with
    /// nobody to hand out a manifest, the sessions a peer has *are* the island.
    /// Returns the lifecycle events the change caused.
    pub fn follow_sessions(&mut self, connected: &[NodeId]) -> Vec<NetEvent> {
        let regime = TopologyRegime::for_population(connected.len(), None);
        let entries: Vec<PeerEntry> = connected
            .iter()
            .map(|node| PeerEntry {
                node: *node,
                cells: Vec::new(),
            })
            .collect();
        self.source = IslandSource::ConnectedPeers;
        self.replace_peers(&entries, regime, false)
    }

    /// Leave the current island (drain, disconnect, or coordinator eviction).
    ///
    /// Emits a `PeerLeft` per member so downstream systems tear down the same
    /// way they would for a one-by-one departure, plus a final `IslandChanged`.
    pub fn leave(&mut self) -> Vec<NetEvent> {
        let mut events: Vec<NetEvent> = self
            .peers
            .drain(..)
            .map(|entry| NetEvent::PeerLeft { node: entry.node })
            .collect();
        self.island = None;
        self.epoch = 0;
        self.regime = TopologyRegime::Mesh;
        self.source = IslandSource::Unassigned;
        events.push(NetEvent::IslandChanged {
            island: None,
            epoch: 0,
            regime: TopologyRegime::Mesh,
        });
        events
    }

    /// Swap in a new peer set and regime, emitting the diff.
    ///
    /// Diffs by NodeId rather than by comparing the vectors, because a manifest
    /// that merely reordered its peers — or restated one peer's cells — is not a
    /// join or a leave, and treating it as one would churn every downstream
    /// system on every coordinator tick.
    fn replace_peers(
        &mut self,
        peers: &[PeerEntry],
        regime: TopologyRegime,
        island_changed: bool,
    ) -> Vec<NetEvent> {
        let mut events = Vec::new();
        for entry in peers {
            if !self.contains(entry.node) {
                events.push(NetEvent::PeerJoined { node: entry.node });
            }
        }
        for entry in &self.peers {
            if !peers.iter().any(|new| new.node == entry.node) {
                events.push(NetEvent::PeerLeft { node: entry.node });
            }
        }

        let regime_changed = self.regime != regime;
        let roster_changed = self.peers != peers;
        self.peers = peers.to_vec();
        self.regime = regime;

        if island_changed || regime_changed || roster_changed {
            events.push(NetEvent::IslandChanged {
                island: self.island,
                epoch: self.epoch,
                regime,
            });
        }
        events
    }
}

/// A membership lifecycle event (docs/10-crates.md §4).
#[derive(Debug, Clone, PartialEq, Eq, Message)]
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
        /// The manifest epoch this membership was built from.
        epoch: u32,
        /// The new regime.
        regime: TopologyRegime,
    },
}

/// Drives [`IslandMembership`] from the connected-session set.
///
/// Runs **only when no coordinator is configured** — see the module docs. With a
/// coordinator, membership arrives as a manifest and this system would fight it,
/// overwriting a handout with an observation every frame.
pub fn follow_sessions_without_coordinator(
    mut membership: ResMut<IslandMembership>,
    mut events: MessageWriter<NetEvent>,
    peers: Query<&crate::plugin::Peer>,
) {
    let connected: Vec<NodeId> = peers.iter().map(|peer| peer.id).collect();
    for event in membership.follow_sessions(&connected) {
        events.write(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::CellId;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh::SecretKey::from_bytes(&seed).public()
    }

    /// The local peer for tests that are not about self-filtering: an id no
    /// fixture ever puts in a roster.
    fn me() -> NodeId {
        node(0xFF)
    }

    /// `apply_manifest` as the local peer, so each test states only what it is
    /// about.
    trait AsLocalPeer {
        fn apply_manifest_t(
            &mut self,
            manifest: &IslandManifest,
        ) -> Result<Vec<NetEvent>, StaleManifest>;
    }

    impl AsLocalPeer for IslandMembership {
        fn apply_manifest_t(
            &mut self,
            manifest: &IslandManifest,
        ) -> Result<Vec<NetEvent>, StaleManifest> {
            self.apply_manifest(manifest, me())
        }
    }

    fn entry(n: u8) -> PeerEntry {
        PeerEntry {
            node: node(n),
            cells: Vec::new(),
        }
    }

    fn manifest(island: u64, epoch: u32, peers: Vec<PeerEntry>) -> IslandManifest {
        IslandManifest {
            island: IslandId::new(island),
            epoch,
            cells: Vec::new(),
            regime: TopologyRegime::Mesh,
            peers,
        }
    }

    #[test]
    fn a_manifest_populates_membership() {
        let mut m = IslandMembership::default();
        assert!(!m.is_member());
        let events = m
            .apply_manifest_t(&manifest(7, 1, vec![entry(1), entry(2)]))
            .expect("the first manifest applies");
        assert!(m.is_member());
        assert_eq!(m.peer_count(), 2);
        assert!(m.contains(node(1)));
        assert_eq!(m.source, IslandSource::Coordinator);
        assert_eq!(
            events,
            vec![
                NetEvent::PeerJoined { node: node(1) },
                NetEvent::PeerJoined { node: node(2) },
                NetEvent::IslandChanged {
                    island: Some(IslandId::new(7)),
                    epoch: 1,
                    regime: TopologyRegime::Mesh,
                },
            ]
        );
    }

    #[test]
    fn a_replayed_older_manifest_is_refused() {
        // Manifests ride unreliable datagrams. A duplicate of an older one
        // arriving late would otherwise resurrect a peer that has left, and the
        // island would oscillate for the length of the reordering window.
        let mut m = IslandMembership::default();
        m.apply_manifest_t(&manifest(7, 1, vec![entry(1), entry(2)]))
            .unwrap();
        m.apply_manifest_t(&manifest(7, 2, vec![entry(1)])).unwrap();
        assert_eq!(m.peer_count(), 1);

        let stale = m
            .apply_manifest_t(&manifest(7, 1, vec![entry(1), entry(2)]))
            .expect_err("epoch 1 is not newer than 2");
        assert_eq!(
            stale,
            StaleManifest {
                island: IslandId::new(7),
                applied: 2,
                offered: 1,
            }
        );
        assert_eq!(m.peer_count(), 1, "the stale manifest changed nothing");
        assert_eq!(m.epoch, 2);
    }

    #[test]
    fn the_same_epoch_twice_is_refused() {
        // Duplicate delivery rather than reordering. Applying it again would
        // re-emit the whole join set to downstream systems.
        let mut m = IslandMembership::default();
        let one = manifest(7, 3, vec![entry(1)]);
        m.apply_manifest_t(&one).unwrap();
        assert!(m.apply_manifest_t(&one).is_err());
    }

    #[test]
    fn epoch_zero_is_a_real_epoch_for_a_freshly_formed_island() {
        // The coordinator starts an island's counter at 0 and bumps on change,
        // so the first manifest a peer ever sees can legitimately be epoch 0.
        // Gating on `epoch > self.epoch` alone — with `self.epoch` also 0 —
        // would drop it and leave the peer islandless.
        let mut m = IslandMembership::default();
        let events = m
            .apply_manifest_t(&manifest(1, 0, vec![entry(1)]))
            .expect("epoch 0 applies when no island is assigned");
        assert_eq!(m.island, Some(IslandId::new(1)));
        assert!(events.contains(&NetEvent::PeerJoined { node: node(1) }));
    }

    #[test]
    fn moving_to_another_island_is_accepted_at_any_epoch() {
        // Island ids are reallocated on merge — the larger population's id
        // survives — and the new island's epoch counter is unrelated to the old
        // one's. Comparing across them would reject a legitimate move.
        let mut m = IslandMembership::default();
        m.apply_manifest_t(&manifest(7, 9, vec![entry(1)])).unwrap();
        let events = m
            .apply_manifest_t(&manifest(8, 0, vec![entry(2)]))
            .expect("a different island is always newer");
        assert_eq!(m.island, Some(IslandId::new(8)));
        assert_eq!(m.epoch, 0);
        assert!(events.contains(&NetEvent::PeerLeft { node: node(1) }));
        assert!(events.contains(&NetEvent::PeerJoined { node: node(2) }));
    }

    #[test]
    fn reordering_the_same_peers_is_not_a_join_or_a_leave() {
        // The coordinator builds manifests from a map; nothing promises a
        // stable order. Diffing the vectors instead of the sets would churn
        // every downstream system on every coordinator tick.
        let mut m = IslandMembership::default();
        m.apply_manifest_t(&manifest(7, 1, vec![entry(1), entry(2)]))
            .unwrap();
        let events = m
            .apply_manifest_t(&manifest(7, 2, vec![entry(2), entry(1)]))
            .unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, NetEvent::PeerJoined { .. } | NetEvent::PeerLeft { .. })),
            "membership did not change: {events:?}"
        );
    }

    #[test]
    fn a_peers_cells_changing_is_a_roster_change_but_not_a_join() {
        // Downstream visibility mapping needs to hear that a peer moved; the
        // session layer must not tear its connection down and rebuild it.
        let mut m = IslandMembership::default();
        m.apply_manifest_t(&manifest(7, 1, vec![entry(1)])).unwrap();
        let moved = PeerEntry {
            node: node(1),
            cells: vec![CellId::ROOT],
        };
        let events = m.apply_manifest_t(&manifest(7, 2, vec![moved])).unwrap();
        assert_eq!(
            events,
            vec![NetEvent::IslandChanged {
                island: Some(IslandId::new(7)),
                epoch: 2,
                regime: TopologyRegime::Mesh,
            }]
        );
        assert_eq!(m.peers[0].cells, vec![CellId::ROOT]);
    }

    #[test]
    fn leaving_announces_every_departure() {
        // Downstream systems tear down per peer. A bare `IslandChanged` would
        // leave them holding sessions and proxies for an island that is gone.
        let mut m = IslandMembership::default();
        m.apply_manifest_t(&manifest(7, 1, vec![entry(1), entry(2)]))
            .unwrap();
        let events = m.leave();
        assert!(!m.is_member());
        assert!(m.peers.is_empty());
        assert!(events.contains(&NetEvent::PeerLeft { node: node(1) }));
        assert!(events.contains(&NetEvent::PeerLeft { node: node(2) }));
        assert_eq!(
            events.last(),
            Some(&NetEvent::IslandChanged {
                island: None,
                epoch: 0,
                regime: TopologyRegime::Mesh,
            })
        );
    }

    #[test]
    fn the_local_peer_is_removed_from_its_own_roster() {
        // The coordinator broadcasts one manifest to everyone it names, so the
        // recipient is in the list it receives. Keeping itself would inflate
        // `peer_count`, and — since a peer with an empty island still gets a
        // manifest — would make a solo peer look like it had company.
        let mut m = IslandMembership::default();
        let roster = vec![
            PeerEntry {
                node: me(),
                cells: vec![CellId::ROOT],
            },
            entry(1),
        ];
        let events = m.apply_manifest_t(&manifest(7, 1, roster)).unwrap();
        assert_eq!(m.peer_count(), 1);
        assert!(!m.contains(me()), "a peer is not its own island-mate");
        assert!(m.contains(node(1)));
        assert!(!events.contains(&NetEvent::PeerJoined { node: me() }));
    }

    #[test]
    fn a_solo_island_has_no_peers() {
        let mut m = IslandMembership::default();
        m.apply_manifest_t(&manifest(
            7,
            0,
            vec![PeerEntry {
                node: me(),
                cells: Vec::new(),
            }],
        ))
        .unwrap();
        assert!(m.is_member(), "the peer is in an island");
        assert_eq!(m.peer_count(), 0, "but alone in it");
    }

    #[test]
    fn the_coordinatorless_path_still_follows_sessions() {
        let mut m = IslandMembership::default();
        let events = m.follow_sessions(&[node(1), node(2)]);
        assert_eq!(m.peer_count(), 2);
        assert_eq!(m.source, IslandSource::ConnectedPeers);
        assert!(events.contains(&NetEvent::PeerJoined { node: node(1) }));
        // Idempotent: a steady session set emits nothing.
        assert!(m.follow_sessions(&[node(1), node(2)]).is_empty());
    }
}
