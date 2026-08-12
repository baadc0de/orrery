//! The `OrrerySpatialPlugin` — P1 core (docs/11-roadmap.md §P1).
//!
//! `CellId` assignment from `big_space` grid coordinates, the 27-cell AOI
//! subscription, mapping cell membership onto bevy_replicon per-client
//! visibility, cell-crossing hysteresis (the 10% overlap zone), and the bounded
//! high-rate interest set with 1-4 Hz proxies.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use orrery_protocol::CellId;

use crate::hysteresis::update_cell_commit;
use crate::interest::{update_interest_set, InterestSelection};
use crate::SpatialConfig;

/// The `orrery_spatial` plugin.
#[derive(Default)]
pub struct OrrerySpatialPlugin {
    /// Spatial configuration.
    pub config: SpatialConfig,
}

impl Plugin for OrrerySpatialPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config.clone())
            .init_resource::<AoiSubscription>()
            .init_resource::<InterestSelection>()
            .add_systems(
                Update,
                // Hysteresis first (commit the cell), then the AOI from the
                // committed cell, then the interest set from positions.
                (update_cell_commit, update_aoi, update_interest_set).chain(),
            );
    }
}

/// An entity's hysteresis-stable current cell (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub struct Cell(pub CellId);

/// The local client's 27-cell AOI subscription (D5).
#[derive(Debug, Default, Resource)]
pub struct AoiSubscription {
    /// The 27 cells of the 3×3×3 neighborhood, self included.
    pub cells: Vec<CellId>,
}

impl AoiSubscription {
    /// Whether `cell` is in the current AOI.
    #[must_use]
    pub fn contains(&self, cell: CellId) -> bool {
        self.cells.contains(&cell)
    }
}

/// Recomputes the [`AoiSubscription`] from the local player's [`Cell`].
///
/// This is the P1 skeleton: it derives the 27-cell neighborhood from the
/// player's cell. The replicon visibility mapping (task 4) consumes this
/// subscription to gate per-client replication.
fn update_aoi(mut aoi: ResMut<AoiSubscription>, player: Query<&Cell, With<LocalPlayer>>) {
    let Ok(cell) = player.single() else {
        return;
    };
    aoi.cells = cell.0.neighbors27();
}

/// Marker for the local player entity (the AOI center).
#[derive(Debug, Component)]
pub struct LocalPlayer;
