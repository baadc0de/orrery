//! Mapping cell membership onto bevy_replicon per-client visibility (P1).
//!
//! The design's replication interest group is the 27-cell AOI (D5). This
//! module registers a replicon visibility scope and drives it from the
//! [`AoiSubscription`]: an entity is visible to a client iff its `Cell` is in
//! that client's 27-cell neighborhood.
//!
//! This is the P1 skeleton of the "big_space → replicon visibility" integration
//! (docs/11-roadmap.md §P1). The high-rate interest-set selection and proxy
//! extrapolation build on top of this base visibility.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_replicon::server::visibility::{
    client_visibility::ClientVisibility, filters_mask::FilterBit, registry::FilterRegistry,
};
use bevy_replicon::shared::replication::registry::ReplicationRegistry;
use bevy_replicon::shared::replication::visibility::ScopeLifetime;

use crate::plugin::{AoiSubscription, Cell};

/// The replicon visibility bit for the AOI scope, registered at app build.
#[derive(Debug, Resource)]
pub struct AoiVisibilityBit(pub FilterBit);

/// Registers the AOI visibility scope and the system that drives it.
pub struct AoiVisibilityPlugin;

impl Plugin for AoiVisibilityPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AoiVisibilityBit>()
            .add_systems(Update, update_visibility);
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

/// Drives per-client visibility from each client's [`AoiSubscription`].
///
/// For every client (an entity with [`ClientVisibility`]) and every replicated
/// entity with a [`Cell`], the entity is visible iff its cell is in the
/// client's AOI. This is the base interest-group gate; the high-rate set and
/// proxies refine it.
fn update_visibility(
    bit: Res<AoiVisibilityBit>,
    aoi: Res<AoiSubscription>,
    mut clients: Query<&mut ClientVisibility>,
    entities: Query<(Entity, &Cell), Without<ClientVisibility>>,
) {
    let bit = bit.0;
    for mut client in &mut clients {
        for (entity, cell) in &entities {
            let visible = aoi.contains(cell.0);
            client.set(entity, bit, visible);
        }
    }
}
