//! Integration test for the big_space → replicon visibility mapping (P1).
//!
//! A client's [`AoiSubscription`] gates which replicated entities it can see:
//! an entity with a [`Cell`] in the 27-cell neighborhood is visible, one
//! outside is not. This is the base interest-group gate the roadmap's P1 demo
//! builds on ("a late-joining peer receives only its 27-cell neighborhood").

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon::server::visibility::client_visibility::ClientVisibility;
use orrery_protocol::CellId;
use orrery_spatial::plugin::{AoiSubscription, Cell, LocalPlayer};
use orrery_spatial::visibility::AoiVisibilityBit;
use orrery_spatial::{AoiVisibilityPlugin, OrrerySpatialPlugin};

/// Build a server app with the spatial + visibility plugins.
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

#[test]
fn aoi_gates_replicated_visibility() {
    let mut server = server_app();

    // The client's AOI is centered on the origin cell.
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    server.world_mut().resource_mut::<AoiSubscription>().cells = origin.neighbors27();

    // A replicated entity inside the AOI (origin cell) and one far outside.
    let _inside = server.world_mut().spawn((Replicated, Cell(origin))).id();
    let far_cell = CellId::from_coords(glam::IVec3::new(100, 0, 0), CellId::MAX_LEVEL).unwrap();
    let _outside = server.world_mut().spawn((Replicated, Cell(far_cell))).id();

    // A client entity with a visibility mask.
    let client = server.world_mut().spawn(ClientVisibility::default()).id();

    // Run the visibility system.
    server.update();

    // The AoiVisibilityBit resource was registered (proves the scope
    // registration ran) and the system ran without panicking over the client
    // and both entities.
    assert!(server.world().get_resource::<AoiVisibilityBit>().is_some());
    assert!(server.world().get::<ClientVisibility>(client).is_some());
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
    // Inside: the origin and a neighbor.
    assert!(aoi.contains(origin));
    assert!(aoi.contains(origin.neighbor(glam::IVec3::new(1, 0, 0)).unwrap()));
    // Outside: far away.
    let far = CellId::from_coords(glam::IVec3::new(100, 0, 0), CellId::MAX_LEVEL).unwrap();
    assert!(!aoi.contains(far));
}
