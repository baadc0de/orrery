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

#[test]
fn high_rate_cap_bounds_the_interest_set() {
    let mut app = app();

    // The local player at the origin.
    let origin = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
    app.world_mut()
        .spawn((LocalPlayer, Cell(origin), GridPosition(Vec3::ZERO)));

    // 40 candidates at increasing distance.
    let mut candidates = Vec::new();
    for i in 0..40 {
        let pos = Vec3::new(i as f32 * 0.5, 0.0, 0.0);
        let cell = CellId::from_coords(glam::IVec3::new(i / 2, 0, 0), CellId::MAX_LEVEL).unwrap();
        candidates.push(app.world_mut().spawn((Cell(cell), GridPosition(pos))).id());
    }

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
    for i in 0..30 {
        let pos = Vec3::new(i as f32 * 0.1, 0.0, 0.0);
        let cell = CellId::from_coords(glam::IVec3::new(i / 10, 0, 0), CellId::MAX_LEVEL).unwrap();
        app.world_mut().spawn((Cell(cell), GridPosition(pos)));
    }
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
