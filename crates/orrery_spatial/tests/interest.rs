//! Integration test for the P1 interest-set + hysteresis pipeline
//! (docs/11-roadmap.md §P1).
//!
//! The `OrrerySpatialPlugin` chains hysteresis → AOI → interest-set selection
//! each update. This exercises the bounded high-rate set (24 entities, D16) and
//! the 1–4 Hz proxies end-to-end: a local player plus a population of
//! candidates, nearest `high_rate_cap` tagged [`HighRate`], the rest [`Proxy`].

use bevy::prelude::*;
use orrery_protocol::CellId;
use orrery_spatial::hysteresis::GridPosition;
use orrery_spatial::interest::{HighRate, InterestSelection, Proxy};
use orrery_spatial::plugin::{Cell, LocalPlayer};
use orrery_spatial::OrrerySpatialPlugin;
use orrery_spatial::SpatialConfig;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, OrrerySpatialPlugin::default()));
    app
}

/// A cell on the x axis at the interest level.
fn cell(x: i32) -> CellId {
    CellId::from_coords(glam::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).unwrap()
}

/// Spawn `count` candidates spread across the AOI's x span, nearest first.
///
/// Positions stay inside cells 0 and 1, which are in the origin's 27-cell
/// neighbourhood. Placing them further would be testing the coarse filter, not
/// the cap.
fn populate(app: &mut App, count: usize) -> Vec<Entity> {
    (0..count)
        .map(|i| {
            let x = i as f32 * (2.0 / count as f32);
            app.world_mut()
                .spawn((
                    Cell(cell(x.floor() as i32)),
                    GridPosition(Vec3::new(x, 0.0, 0.0)),
                ))
                .id()
        })
        .collect()
}

#[test]
fn high_rate_cap_bounds_the_interest_set() {
    let mut app = app();

    // The local player at the origin.
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    app.world_mut()
        .spawn((LocalPlayer, Cell(origin), GridPosition(Vec3::ZERO)));

    let candidates = populate(&mut app, 40);

    app.update();

    let selection = app.world().resource::<InterestSelection>();
    let high_rate = selection.high_rate.clone();
    let proxy_count = selection.proxies.len();
    assert_eq!(high_rate.len(), 24);
    assert_eq!(proxy_count, 16);

    // Every candidate carries exactly one of HighRate / Proxy (the local
    // player is excluded — it has neither).
    let mut high = 0;
    let mut proxied = 0;
    for entity in &candidates {
        let hr = app.world().get::<HighRate>(*entity);
        let pr = app.world().get::<Proxy>(*entity);
        assert!(hr.is_some() ^ pr.is_some(), "exactly one tag per entity");
        if hr.is_some() {
            high += 1;
        } else {
            proxied += 1;
        }
    }
    assert_eq!(high, 24);
    assert_eq!(proxied, 40 - 24);

    // The high-rate set is the 24 nearest candidates.
    assert_eq!(high_rate.len(), 24);
}

#[test]
fn proxy_rates_fall_within_config_range() {
    let mut app = app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    app.world_mut()
        .spawn((LocalPlayer, Cell(origin), GridPosition(Vec3::ZERO)));

    // Candidates spread across the AOI so proxy rates span the range.
    populate(&mut app, 30);
    app.update();

    let cfg = app.world().resource::<SpatialConfig>();
    let selection = app.world().resource::<InterestSelection>();
    for (_, rate) in &selection.proxies {
        assert!(
            *rate >= *cfg.proxy_hz.start() && *rate <= *cfg.proxy_hz.end(),
            "proxy rate {rate} outside {:?}",
            cfg.proxy_hz
        );
    }
}

#[test]
fn an_entity_outside_the_aoi_is_neither_high_rate_nor_proxied() {
    // Cells are the coarse filter; distance only orders what survives it (D5,
    // D6). Ranking the whole world would hand a 1 Hz proxy to something a
    // hundred kilometres away, and the receive-cost bound the Donnybrook
    // pattern rests on is over the *in-range* population, not the global one.
    let mut app = app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    app.world_mut()
        .spawn((LocalPlayer, Cell(origin), GridPosition(Vec3::ZERO)));

    let near = app
        .world_mut()
        .spawn((Cell(cell(1)), GridPosition(Vec3::new(1.5, 0.0, 0.0))))
        .id();
    // Cell 40 is nowhere near the origin's 3×3×3 neighbourhood.
    let far = app
        .world_mut()
        .spawn((Cell(cell(40)), GridPosition(Vec3::new(40.5, 0.0, 0.0))))
        .id();

    app.update();

    assert!(app.world().get::<HighRate>(near).is_some());
    assert!(
        app.world().get::<HighRate>(far).is_none() && app.world().get::<Proxy>(far).is_none(),
        "an out-of-AOI entity carries no interest tag at all"
    );
    let selection = app.world().resource::<InterestSelection>();
    assert_eq!(selection.high_rate, vec![near]);
    assert!(selection.proxies.is_empty());
}

#[test]
fn leaving_the_aoi_strips_a_tag_the_entity_already_had() {
    // Stale interest is indistinguishable from current interest wherever the
    // tags are read, so a departure has to clear them rather than leave the
    // last one standing.
    let mut app = app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    app.world_mut()
        .spawn((LocalPlayer, Cell(origin), GridPosition(Vec3::ZERO)));
    let mover = app
        .world_mut()
        .spawn((Cell(cell(1)), GridPosition(Vec3::new(1.5, 0.0, 0.0))))
        .id();

    app.update();
    assert!(app.world().get::<HighRate>(mover).is_some());

    // It walks out of the neighbourhood.
    *app.world_mut().get_mut::<Cell>(mover).unwrap() = Cell(cell(40));
    *app.world_mut().get_mut::<GridPosition>(mover).unwrap() =
        GridPosition(Vec3::new(40.5, 0.0, 0.0));
    app.update();

    assert!(
        app.world().get::<HighRate>(mover).is_none(),
        "the tag must not survive the entity leaving the AOI"
    );
    assert!(app.world().get::<Proxy>(mover).is_none());
}

#[test]
fn hysteresis_prevents_boundary_thrash_through_plugin() {
    let mut app = app();
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    let entity = app
        .world_mut()
        .spawn((
            LocalPlayer,
            Cell(origin),
            GridPosition(Vec3::new(0.95, 0.0, 0.0)),
        ))
        .id();

    // Oscillate across the x=1.0 boundary within the 0.1 margin.
    for x in [1.05, 0.9, 1.08, 0.92, 1.0] {
        *app.world_mut().get_mut::<GridPosition>(entity).unwrap() =
            GridPosition(Vec3::new(x, 0.0, 0.0));
        app.update();
        assert_eq!(
            app.world().get::<Cell>(entity).unwrap().0,
            origin,
            "commitment must not thrash at x={x}"
        );
    }

    // Drive past the margin → commits the neighbor.
    *app.world_mut().get_mut::<GridPosition>(entity).unwrap() =
        GridPosition(Vec3::new(1.2, 0.0, 0.0));
    app.update();
    let expected = CellId::from_coords(glam::IVec3::new(1, 0, 0), CellId::MAX_LEVEL).unwrap();
    assert_eq!(app.world().get::<Cell>(entity).unwrap().0, expected);
}
