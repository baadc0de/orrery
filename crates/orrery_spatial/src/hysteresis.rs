//! Cell-crossing hysteresis — the 10% overlap zone (docs/01-spatial-model.md §7).
//!
//! Commitment to an interest cell is a Schmitt trigger on penetration depth.
//! An entity is committed to exactly one cell (the [`Cell`] component), and the
//! commitment changes **only** when its position leaves the committed cell's
//! bounds *expanded by a margin `m = hysteresis_frac · edge` on every face*.
//! Re-entering the committed cell from the overlap zone costs nothing, so an
//! entity oscillating on a boundary (amplitude < m) never thrashes — no
//! `CellCrossing`, no subscription change, no storage re-key (D5).
//!
//! Two peers may therefore briefly disagree about an entity's *geometric* cell
//! but never about its *committed* cell: the commitment is a single-writer
//! value emitted by the authority holder (D2), and the hysteresis guarantees a
//! latching commitment so that value is stable.

use bevy_ecs::prelude::*;
use bevy_math::Vec3;
use glam::IVec3;
use orrery_protocol::CellId;

use crate::config::SpatialConfig;
use crate::plugin::Cell;

/// An entity's position in **grid units** — world metres ÷ cell edge.
///
/// The integer part of each component is the geometric cell index; the
/// committed [`Cell`] lags the geometric cell by at most the hysteresis margin.
/// This is engine-agnostic with respect to `big_space`: the game maps its
/// `GridCell` world position to grid units, or uses [`GridPosition`] directly
/// in the headless bot harness.
#[derive(Debug, Clone, Copy, Component)]
pub struct GridPosition(pub Vec3);

impl GridPosition {
    /// A grid position from world metres at the given cell edge.
    #[must_use]
    pub fn from_world(world: Vec3, cell_edge_m: f32) -> Self {
        Self(world / cell_edge_m)
    }
}

/// Decide the committed cell for a position given the current commitment.
///
/// Returns `None` when the position is still within the committed cell's bounds
/// expanded by `margin` (keep the current commitment — the common, traffic-free
/// case), or `Some(geometric)` when the entity has penetrated more than `margin`
/// into a neighbor (commit the new cell). `margin` is in grid units and is
/// `hysteresis_frac · 1.0` at the default 128 m edge (D16).
#[must_use]
pub fn step_commit(committed: CellId, pos: Vec3, margin: f32) -> Option<CellId> {
    let (c, level) = committed.coords();
    // Committed cell spans [c, c+1) per axis in grid units; expanded by margin.
    let within = |a: i32, p: f32| {
        let lo = a as f32 - margin;
        let hi = a as f32 + 1.0 + margin;
        p >= lo && p < hi
    };
    if within(c.x, pos.x) && within(c.y, pos.y) && within(c.z, pos.z) {
        return None;
    }
    // Outside the expanded bounds: commit the geometric cell (floor per axis).
    let g = IVec3::new(
        pos.x.floor() as i32,
        pos.y.floor() as i32,
        pos.z.floor() as i32,
    );
    CellId::from_coords(g, level).ok()
}

/// Recompute each entity's committed [`Cell`] under the Schmitt trigger.
///
/// Runs *before* the AOI recomputation so the AOI always reflects the committed
/// cell, not the raw geometric one.
pub fn update_cell_commit(cfg: Res<SpatialConfig>, mut query: Query<(&mut Cell, &GridPosition)>) {
    let margin = cfg.hysteresis_frac;
    for (mut cell, pos) in &mut query {
        if let Some(committed) = step_commit(cell.0, pos.0, margin) {
            cell.0 = committed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::LocalPlayer;
    use bevy_app::{App, Update};

    fn cell_at(c: IVec3) -> CellId {
        CellId::from_coords(c, CellId::MAX_LEVEL).unwrap()
    }

    #[test]
    fn interior_position_keeps_commitment() {
        // Inside the committed cell: no change.
        let committed = cell_at(IVec3::new(4, 0, 0));
        assert_eq!(step_commit(committed, Vec3::new(4.9, 0.0, 0.0), 0.1), None);
        assert_eq!(step_commit(committed, Vec3::new(4.0, 0.0, 0.0), 0.1), None);
    }

    #[test]
    fn geometric_cross_held_inside_margin() {
        // The boundary between cells (4,0,0) and (5,0,0) is at x = 5.0. At
        // x = 5.05 the *geometric* cell is (5,0,0), but penetration (0.05) is
        // under the 0.1 margin, so the commitment stays on (4,0,0).
        let committed = cell_at(IVec3::new(4, 0, 0));
        assert_eq!(step_commit(committed, Vec3::new(5.05, 0.0, 0.0), 0.1), None);
    }

    #[test]
    fn crossing_commits_when_past_margin() {
        // Penetration 0.15 > margin 0.1 → commit the geometric cell (5,0,0).
        let committed = cell_at(IVec3::new(4, 0, 0));
        assert_eq!(
            step_commit(committed, Vec3::new(5.15, 0.0, 0.0), 0.1),
            Some(cell_at(IVec3::new(5, 0, 0)))
        );
    }

    #[test]
    fn no_thrash_on_boundary_oscillation() {
        let cfg = SpatialConfig::default();
        let mut app = App::new();
        app.insert_resource(cfg)
            .add_systems(Update, update_cell_commit);

        // An entity straddling the x=5.0 boundary between cells (4,0,0) and
        // (5,0,0), oscillating by ±0.1 (just under the 0.1 grid-unit margin
        // at the default 10% hysteresis).
        let committed = cell_at(IVec3::new(4, 0, 0));
        let entity = app
            .world_mut()
            .spawn((Cell(committed), GridPosition(Vec3::new(4.95, 0.0, 0.0))))
            .id();

        let pos = |app: &mut App, x: f32| {
            *app.world_mut()
                .get_mut::<GridPosition>(entity)
                .expect("grid position") = GridPosition(Vec3::new(x, 0.0, 0.0));
            app.update();
            *app.world_mut().get::<Cell>(entity).expect("cell")
        };

        // Oscillate across the boundary within the margin: the commitment
        // must never flip. (Geometric cells alternate 4 and 5.)
        assert_eq!(pos(&mut app, 5.05), Cell(committed));
        assert_eq!(pos(&mut app, 4.9), Cell(committed));
        assert_eq!(pos(&mut app, 5.08), Cell(committed));
        assert_eq!(pos(&mut app, 4.92), Cell(committed));
        // Still no thrash.
        assert_eq!(pos(&mut app, 4.95), Cell(committed));

        // Drive more than m deep (5.15) → commits (5,0,0).
        assert_eq!(pos(&mut app, 5.15), Cell(cell_at(IVec3::new(5, 0, 0))));

        // Now latched on (5,0,0): drifting back inside the margin holds it.
        assert_eq!(pos(&mut app, 4.95), Cell(cell_at(IVec3::new(5, 0, 0))));
        assert_eq!(pos(&mut app, 5.0), Cell(cell_at(IVec3::new(5, 0, 0))));
        // Fully back to interior of (5,0,0) after a real crossing.
        assert_eq!(pos(&mut app, 4.85), Cell(cell_at(IVec3::new(4, 0, 0))));
    }

    #[test]
    fn grid_position_from_world() {
        let p = GridPosition::from_world(Vec3::new(128.0, 0.0, 0.0), 128.0);
        assert_eq!(p.0, Vec3::new(1.0, 0.0, 0.0));
    }

    #[test]
    fn local_player_still_commits() {
        // The hysteresis system applies to the local player too (it is a Cell +
        // GridPosition entity), independent of the LocalPlayer marker used for
        // the AOI center.
        let committed = cell_at(IVec3::new(4, 0, 0));
        let mut app = App::new();
        app.insert_resource(SpatialConfig::default())
            .add_systems(Update, update_cell_commit);
        let entity = app
            .world_mut()
            .spawn((
                Cell(committed),
                GridPosition(Vec3::new(5.2, 0.0, 0.0)),
                LocalPlayer,
            ))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<Cell>(entity).expect("cell"),
            &Cell(cell_at(IVec3::new(5, 0, 0)))
        );
    }
}
