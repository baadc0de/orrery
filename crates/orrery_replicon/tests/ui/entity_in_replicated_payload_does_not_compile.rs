use bevy_app::App;
use bevy_ecs::entity::Entity;
use orrery_replicon::{OrreryRepliconAppExt, ReplicatedPayload};

type ContainsEntity = ReplicatedPayload<Entity>;

fn register(app: &mut App) {
    app.replicate::<ContainsEntity>();
}

fn main() {}
