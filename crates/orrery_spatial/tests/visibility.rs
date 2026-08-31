//! Integration test for the cell → replicon visibility mapping (P1).
//!
//! Each client's [`ClientAoi`] gates which replicated entities it can see: an
//! entity whose committed [`Cell`] is in that client's subscription is visible,
//! one outside is not. This is the base interest-group gate the P1 demo builds
//! on — *"a late-joining peer receives only its 27-cell neighborhood"*.
//!
//! # Reading visibility back
//!
//! `ClientVisibility::set` had no public inverse, so until now these tests
//! could only assert on the *input* to the decision. `is_visible` is the
//! counterpart, added to the vendored replicon fork — P1's upstream milestone
//! is "visibility-API ergonomics feedback/patches to bevy_replicon"
//! (docs/11-roadmap.md §P1), and this is that patch. It is additive and changes
//! no behaviour, so it should upstream cleanly.
//!
//! With it, these tests assert the thing that actually matters: which entities
//! a given client can see.

use bevy::prelude::*;
use orrery_net::{IslandMembership, IslandSource};
use orrery_protocol::coord::{IslandId, PeerEntry, TopologyRegime};
use orrery_protocol::{CellId, NodeId};
use orrery_replicon::{ClientVisibility, Replicated, RepliconPlugins};
use orrery_spatial::plugin::{AoiSubscription, Cell, LocalPlayer};
use orrery_spatial::visibility::{AoiVisibilityBit, ClientAoi, ClientNode};
use orrery_spatial::{AoiVisibilityPlugin, OrrerySpatialPlugin};

fn server_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        bevy::state::app::StatesPlugin,
        RepliconPlugins,
        OrrerySpatialPlugin::default(),
        AoiVisibilityPlugin,
    ));
    app
}

fn node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn cell(x: i32) -> CellId {
    CellId::from_coords(glam::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).unwrap()
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

#[test]
fn each_client_is_gated_by_its_own_manifest_entry() {
    // Two peers standing in different places. Before this, one `AoiSubscription`
    // was applied to every client, so whatever the *local* player could see,
    // every connected peer was replicated.
    let mut server = server_app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();

    *server.world_mut().resource_mut::<IslandMembership>() = island(vec![
        PeerEntry {
            node: node(1),
            cells: origin.neighbors27(),
        },
        PeerEntry {
            node: node(2),
            cells: vec![cell(100)],
        },
    ]);

    let here = server.world_mut().spawn((Replicated, Cell(origin))).id();
    let there = server.world_mut().spawn((Replicated, Cell(cell(100)))).id();

    let near = server
        .world_mut()
        .spawn((
            ClientVisibility::default(),
            ClientNode(node(1)),
            ClientAoi::default(),
        ))
        .id();
    let far = server
        .world_mut()
        .spawn((
            ClientVisibility::default(),
            ClientNode(node(2)),
            ClientAoi::default(),
        ))
        .id();

    server.update();

    let bit = server.world().resource::<AoiVisibilityBit>().0;
    let sees = |server: &App, client: Entity, entity: Entity| {
        server
            .world()
            .get::<ClientVisibility>(client)
            .expect("visibility")
            .is_visible(entity, bit)
    };

    // Each peer sees what is in its own cells, and only that.
    assert!(sees(&server, near, here));
    assert!(!sees(&server, near, there));
    assert!(sees(&server, far, there));
    assert!(
        !sees(&server, far, here),
        "the distant peer must not inherit the local player's neighbourhood"
    );
}

#[test]
fn a_client_with_no_manifest_entry_subscribes_to_nothing() {
    // Fail closed. Replicating to a client whose interest has not been
    // established is the direction that leaks the world.
    let mut server = server_app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    *server.world_mut().resource_mut::<IslandMembership>() = island(Vec::new());
    let entity = server.world_mut().spawn((Replicated, Cell(origin))).id();

    let client = server
        .world_mut()
        .spawn((
            ClientVisibility::default(),
            ClientNode(node(9)),
            ClientAoi::default(),
        ))
        .id();
    server.update();

    assert!(server
        .world()
        .get::<ClientAoi>(client)
        .expect("aoi")
        .cells
        .is_empty());
    let bit = server.world().resource::<AoiVisibilityBit>().0;
    assert!(
        !server
            .world()
            .get::<ClientVisibility>(client)
            .expect("visibility")
            .is_visible(entity, bit),
        "an unestablished client must not be replicated anything"
    );
}

#[test]
fn moving_into_a_clients_cells_makes_an_entity_visible_to_it() {
    // The gate has to *open* as well as close, or "fail closed" would just be
    // "closed" — and nothing would ever replicate.
    let mut server = server_app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    *server.world_mut().resource_mut::<IslandMembership>() = island(vec![PeerEntry {
        node: node(1),
        cells: vec![origin],
    }]);
    let mover = server.world_mut().spawn((Replicated, Cell(cell(100)))).id();
    let client = server
        .world_mut()
        .spawn((
            ClientVisibility::default(),
            ClientNode(node(1)),
            ClientAoi::default(),
        ))
        .id();

    server.update();
    let bit = server.world().resource::<AoiVisibilityBit>().0;
    let sees = |server: &App| {
        server
            .world()
            .get::<ClientVisibility>(client)
            .expect("visibility")
            .is_visible(mover, bit)
    };
    assert!(!sees(&server), "starts outside the client's cells");

    *server.world_mut().get_mut::<Cell>(mover).unwrap() = Cell(origin);
    server.update();
    assert!(sees(&server), "and becomes visible on arrival");

    *server.world_mut().get_mut::<Cell>(mover).unwrap() = Cell(cell(100));
    server.update();
    assert!(!sees(&server), "and hidden again on departure");
}

#[test]
fn local_player_centers_aoi() {
    let mut server = server_app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    server.world_mut().spawn((LocalPlayer, Cell(origin)));

    server.update();

    let aoi = server.world().resource::<AoiSubscription>();
    assert_eq!(aoi.cells.len(), 27);
    assert!(aoi.contains(origin));
}

#[test]
fn aoi_contains_gates_cells() {
    // The core gating decision: a cell is visible iff it's in the AOI.
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    let aoi = AoiSubscription {
        cells: origin.neighbors27(),
    };
    assert!(aoi.contains(origin));
    assert!(aoi.contains(origin.neighbor(glam::IVec3::new(1, 0, 0)).unwrap()));
    assert!(!aoi.contains(cell(100)));
}
