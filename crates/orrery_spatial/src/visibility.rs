//! Mapping cell membership onto bevy_replicon per-client visibility (P1).
//!
//! The design's replication interest group is the 27-cell AOI (D5). This module
//! registers a replicon visibility scope and drives it per client: an entity is
//! visible to a client iff its committed [`Cell`] is in **that client's**
//! subscription.
//!
//! # Where a client's AOI comes from
//!
//! The coordinator already knows it. An `IslandManifest` names every peer with
//! the cells it covers (`PeerEntry.cells`), which *is* that peer's active
//! interest set — it is what the peer reported presence for and what its
//! interest grant was minted from. So a peer acting as a replication source does
//! not have to guess what its island-mates want: it reads the manifest it was
//! already handed.
//!
//! The one thing this crate cannot know is which replicon client entity belongs
//! to which peer, because that mapping is made by whatever transport adapter
//! accepted the session. The app attaches [`ClientNode`]; everything after that
//! is derived here.
//!
//! # A client with no known AOI sees nothing
//!
//! [`ClientAoi`] is required for visibility, not optional with a permissive
//! default. Replicating to a client whose interest you have not established is
//! the direction that leaks: a peer that had not yet appeared in a manifest
//! would otherwise receive the whole world for as long as that took.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_replicon::server::visibility::{
    client_visibility::ClientVisibility, filters_mask::FilterBit, registry::FilterRegistry,
};
use bevy_replicon::shared::replication::registry::ReplicationRegistry;
use bevy_replicon::shared::replication::visibility::ScopeLifetime;

use orrery_net::IslandMembership;
use orrery_protocol::{CellId, NodeId};

use crate::plugin::Cell;

/// The replicon visibility bit for the AOI scope, registered at app build.
#[derive(Debug, Resource)]
pub struct AoiVisibilityBit(pub FilterBit);

/// The peer behind a replicon client entity.
///
/// Attached by the transport adapter that accepted the session — it is the only
/// thing that knows the correspondence. Without it a client cannot be matched to
/// a manifest entry and stays invisible to everything.
#[derive(Debug, Clone, Copy, Component)]
pub struct ClientNode(pub NodeId);

/// One client's replication interest: the cells it subscribes to.
///
/// Derived from the coordinator's manifest by [`update_client_aoi`]. Kept as a
/// component rather than read from the manifest at visibility time so a client
/// whose peer has dropped out of the island retains a defined — and empty —
/// interest rather than an undefined one.
#[derive(Debug, Clone, Default, Component)]
pub struct ClientAoi {
    /// The cells this client is subscribed to.
    pub cells: Vec<CellId>,
}

impl ClientAoi {
    /// Whether `cell` is in this client's subscription.
    #[must_use]
    pub fn contains(&self, cell: CellId) -> bool {
        self.cells.contains(&cell)
    }
}

/// Registers the AOI visibility scope and the systems that drive it.
pub struct AoiVisibilityPlugin;

impl Plugin for AoiVisibilityPlugin {
    fn build(&self, app: &mut App) {
        // `init_resource` rather than a hard requirement on `OrreryNetPlugin`:
        // the default membership names no peers, so a host without a
        // coordinator replicates to nobody instead of failing to start.
        app.init_resource::<IslandMembership>()
            .init_resource::<AoiVisibilityBit>()
            .add_systems(
                Update,
                // Refresh each client's interest before gating on it, so a peer
                // that just moved is not replicated a frame of the wrong cells.
                (update_client_aoi, update_visibility).chain(),
            );
    }
}

impl FromWorld for AoiVisibilityBit {
    fn from_world(world: &mut World) -> Self {
        let bit = world.resource_scope(|world, mut filter_registry: Mut<FilterRegistry>| {
            world.resource_scope(|world, mut registry: Mut<ReplicationRegistry>| {
                filter_registry.register_scope::<Entity>(
                    world,
                    &mut registry,
                    ScopeLifetime::WhileVisible,
                )
            })
        });
        Self(bit)
    }
}

/// Copies each island peer's covered cells onto its client entity.
///
/// A client whose peer is no longer in the manifest is emptied rather than left
/// holding its last subscription: a peer that left the island must stop
/// receiving, and stale interest is indistinguishable from current interest at
/// the point where visibility is decided.
pub fn update_client_aoi(
    membership: Res<IslandMembership>,
    mut clients: Query<(&ClientNode, &mut ClientAoi)>,
) {
    for (node, mut aoi) in &mut clients {
        let cells = membership
            .peers
            .iter()
            .find(|entry| entry.node == node.0)
            .map(|entry| entry.cells.clone())
            .unwrap_or_default();
        if aoi.cells != cells {
            aoi.cells = cells;
        }
    }
}

/// Drives per-client visibility from each client's [`ClientAoi`].
pub fn update_visibility(
    bit: Res<AoiVisibilityBit>,
    mut clients: Query<(&mut ClientVisibility, &ClientAoi)>,
    entities: Query<(Entity, &Cell), Without<ClientVisibility>>,
) {
    let bit = bit.0;
    for (mut visibility, aoi) in &mut clients {
        for (entity, cell) in &entities {
            visibility.set(entity, bit, aoi.contains(cell.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_net::IslandSource;
    use orrery_protocol::coord::{IslandId, PeerEntry, TopologyRegime};

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn cell(x: i32) -> CellId {
        CellId::from_coords(glam::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).expect("in range")
    }

    fn island(peers: Vec<PeerEntry>) -> IslandMembership {
        IslandMembership {
            island: Some(IslandId::new(1)),
            epoch: 1,
            peers,
            regime: TopologyRegime::Mesh,
            source: IslandSource::Coordinator,
        }
    }

    /// An app running only the AOI derivation — visibility itself needs the
    /// replicon server stack, which these tests deliberately do not stand up.
    fn app(membership: IslandMembership) -> App {
        let mut app = App::new();
        app.insert_resource(membership)
            .add_systems(Update, update_client_aoi);
        app
    }

    #[test]
    fn a_clients_interest_comes_from_the_manifest() {
        // The peer told the coordinator what it covers and the coordinator told
        // everyone. Nobody has to guess.
        let mut app = app(island(vec![PeerEntry {
            node: node(1),
            cells: vec![cell(0), cell(1)],
        }]));
        let client = app
            .world_mut()
            .spawn((ClientNode(node(1)), ClientAoi::default()))
            .id();
        app.update();
        let aoi = app.world().get::<ClientAoi>(client).expect("aoi");
        assert_eq!(aoi.cells, vec![cell(0), cell(1)]);
        assert!(aoi.contains(cell(1)));
        assert!(!aoi.contains(cell(9)));
    }

    #[test]
    fn two_clients_get_their_own_interest_not_a_shared_one() {
        // The bug this replaced: one subscription applied to every client, so
        // whatever the local player could see, every peer received.
        let mut app = app(island(vec![
            PeerEntry {
                node: node(1),
                cells: vec![cell(0)],
            },
            PeerEntry {
                node: node(2),
                cells: vec![cell(50)],
            },
        ]));
        let first = app
            .world_mut()
            .spawn((ClientNode(node(1)), ClientAoi::default()))
            .id();
        let second = app
            .world_mut()
            .spawn((ClientNode(node(2)), ClientAoi::default()))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<ClientAoi>(first).expect("aoi").cells,
            vec![cell(0)]
        );
        assert_eq!(
            app.world().get::<ClientAoi>(second).expect("aoi").cells,
            vec![cell(50)]
        );
    }

    #[test]
    fn a_peer_that_left_the_island_stops_receiving() {
        // Stale interest is indistinguishable from current interest where
        // visibility is decided, so a departed peer is emptied rather than left
        // holding its last subscription.
        let mut app = app(island(vec![PeerEntry {
            node: node(1),
            cells: vec![cell(0)],
        }]));
        let client = app
            .world_mut()
            .spawn((ClientNode(node(1)), ClientAoi::default()))
            .id();
        app.update();
        assert!(!app
            .world()
            .get::<ClientAoi>(client)
            .expect("aoi")
            .cells
            .is_empty());

        *app.world_mut().resource_mut::<IslandMembership>() = island(Vec::new());
        app.update();
        assert!(
            app.world()
                .get::<ClientAoi>(client)
                .expect("aoi")
                .cells
                .is_empty(),
            "a peer no longer in the manifest subscribes to nothing"
        );
    }

    #[test]
    fn a_client_the_manifest_does_not_name_subscribes_to_nothing() {
        // Fail closed. A client whose interest has not been established must
        // not receive the world while that is sorted out.
        let mut app = app(island(vec![PeerEntry {
            node: node(1),
            cells: vec![cell(0)],
        }]));
        let stranger = app
            .world_mut()
            .spawn((ClientNode(node(7)), ClientAoi::default()))
            .id();
        app.update();
        assert!(app
            .world()
            .get::<ClientAoi>(stranger)
            .expect("aoi")
            .cells
            .is_empty());
    }
}
