use bevy_app::App;
use orrery_protocol::PersistId;
use orrery_replicon::{OrreryRepliconAppExt, ReplicatedPayload};

type StablePayload = ReplicatedPayload<(PersistId, u64)>;

fn register(app: &mut App) {
    app.replicate::<StablePayload>();
}

fn main() {}
