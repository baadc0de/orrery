//! What a client app names: the group, the configurations, and the handful of
//! types a game touches on its first day.
//!
//! Deliberately narrow. Everything else stays reachable through the member
//! crates, which the facade re-exports as modules of itself; a prelude that
//! pulled in every subsystem's vocabulary would collide with the game's own.

pub use crate::hit::{CanonicalPose, OrreryHitRegistrationPlugin};
pub use crate::ipc::{IpcOutbound, OrreryIpcExportPlugin, PresentationFrame};
pub use crate::{
    bind_island_membership, queue_authority_corrections, track_predicted_authority,
    OrreryAuthorityAttributionPlugin, OrreryClientPlugins, OrreryConfig, OrreryIslandBindingPlugin,
};

pub use orrery_authority::{HitRules, IslandBinding, OrreryAuthorityPlugin, PoseSample};
pub use orrery_core::Ruleset;
pub use orrery_net::plugin::NetConfig;
pub use orrery_net::{CoordinatorConfig, IslandMembership, OrreryNetPlugin};
pub use orrery_persist_client::{OrreryPersistClientPlugin, PersistClientConfig};
pub use orrery_predict::{
    AuthorityCorrectionReconciler, OrreryPredictPlugin, PredictConfig, PredictedBy,
    SharedAuthorityCorrectionReconciler,
};
pub use orrery_protocol::{CellId, IslandId, Tick};
// `AoiVisibilityPlugin` is not a member of the group — see its docs — so a game
// that replicates adds it itself, which makes it prelude material.
pub use orrery_spatial::{AoiVisibilityPlugin, OrrerySpatialPlugin, SpatialConfig};
pub use orrery_witness::{WitnessPlugin, WitnessState};
