//! What the facade is for, checked against a real `App`.
//!
//! Two failures are worth a test here and neither is visible from any single
//! crate. The first is facade drift (docs/10-crates.md §"Edge cases"): a member
//! plugin gains a resource, or the group's order stops matching the dependency
//! order, and nothing notices because no app in the tree composes all six
//! client plugins — `p1-swarm` is spatial+witness, the persist-client's live
//! test is net+authority+persist_client, and `orrery_predict` is added beside
//! the others nowhere at all. The second is the island wire: `IslandBinding`
//! was written by nothing outside unit tests, so `EphemeralRegistry::spawn`
//! bailed on its first line in every real app.

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;

use orrery::{OrreryClientPlugins, OrreryConfig, OrreryIslandBindingPlugin};
use orrery_authority::ephemeral::EphemeralRegistry;
use orrery_authority::{AuthorityState, IslandBinding, OrreryAuthorityPlugin};
use orrery_games::Skirmish;
use orrery_net::island::IslandMembership;
use orrery_net::plugin::{NetConfig, OrreryNetPlugin, PathTelemetry, PeerRegistry};
use orrery_net::{CoordinatorConfig, IslandSource};
use orrery_persist_client::plugin::OrreryPersistClientPlugin;
use orrery_persist_client::{AreaLoader, GatewaySession, IntentQueue, UplinkScheduler};
use orrery_predict::plugin::OrreryPredictPlugin;
use orrery_predict::{PredictConfig, ReconciliationMonitor, RollbackBudget, TickBridge};
use orrery_protocol::coord::{IslandManifest, PeerEntry, TopologyRegime};
use orrery_protocol::{CellId, IslandId, NodeId, Tick};
use orrery_spatial::plugin::{AoiSubscription, OrrerySpatialPlugin};
use orrery_spatial::{InterestSelection, SpatialConfig};
use orrery_witness::plugin::{AuthoredLog, WitnessPlugin, WitnessSet};

/// Records the order the group's members were built in.
///
/// The only way to observe it: `PluginGroupBuilder` keeps its order private, so
/// the test inserts a probe immediately before each member and lets the build
/// itself report. A probe that runs out of turn is a group whose order drifted.
#[derive(Resource, Clone, Default)]
struct BuildOrder(Arc<Mutex<Vec<&'static str>>>);

macro_rules! probe {
    ($name:ident, $label:literal) => {
        struct $name;

        impl Plugin for $name {
            fn build(&self, app: &mut App) {
                let order = app.world().resource::<BuildOrder>().clone();
                order.0.lock().expect("probe order").push($label);
            }
        }
    };
}

probe!(BeforeNet, "net");
probe!(BeforeSpatial, "spatial");
probe!(BeforeAuthority, "authority");
probe!(BeforeBinding, "binding");
probe!(BeforePredict, "predict");
probe!(BeforeWitness, "witness");
probe!(BeforePersist, "persist");

fn node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes).public()
}

/// A headless client: the group, plus the two engine plugins a real game gets
/// from `DefaultPlugins` (`MinimalPlugins` has no states, and lightyear's
/// replication backend calls `init_state`).
fn client(config: OrreryConfig) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(OrreryClientPlugins::<Skirmish>::new(config));
    app.finish();
    app
}

#[test]
fn group_builds_every_member_in_dependency_order() {
    let order = BuildOrder::default();

    let mut app = App::new();
    app.insert_resource(order.clone());
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(
        OrreryClientPlugins::<Skirmish>::new(OrreryConfig::default())
            .build()
            .add_before::<OrreryNetPlugin>(BeforeNet)
            .add_before::<OrrerySpatialPlugin>(BeforeSpatial)
            .add_before::<OrreryAuthorityPlugin>(BeforeAuthority)
            .add_before::<OrreryIslandBindingPlugin>(BeforeBinding)
            .add_before::<OrreryPredictPlugin>(BeforePredict)
            .add_before::<WitnessPlugin<Skirmish>>(BeforeWitness)
            .add_before::<OrreryPersistClientPlugin>(BeforePersist),
    );
    app.finish();

    let observed = order.0.lock().expect("probe order").clone();
    assert_eq!(
        observed,
        vec![
            "net",
            "spatial",
            "authority",
            "binding",
            "predict",
            "witness",
            "persist",
        ],
        "the group's registration order must stay the dependency order"
    );

    // The AOI visibility mapping is deliberately not a member: its
    // `AoiVisibilityBit` is built out of replicon's registries and panics
    // unless `RepliconPlugins` was added first.
    assert!(!app.is_plugin_added::<orrery_spatial::AoiVisibilityPlugin>());
}

#[test]
fn group_registers_every_member_plugins_resources() {
    let app = client(OrreryConfig::default());
    let world = app.world();

    // orrery_net
    assert!(world.get_resource::<NetConfig>().is_some());
    assert!(world.get_resource::<PeerRegistry>().is_some());
    assert!(world.get_resource::<IslandMembership>().is_some());
    assert!(world.get_resource::<PathTelemetry>().is_some());

    // orrery_spatial
    assert!(world.get_resource::<SpatialConfig>().is_some());
    assert!(world.get_resource::<AoiSubscription>().is_some());
    assert!(world.get_resource::<InterestSelection>().is_some());

    // orrery_authority
    assert!(world.get_resource::<AuthorityState>().is_some());
    assert!(world.get_resource::<IslandBinding>().is_some());
    assert!(world.get_resource::<EphemeralRegistry>().is_some());

    // orrery_predict
    assert!(world.get_resource::<PredictConfig>().is_some());
    assert!(world.get_resource::<ReconciliationMonitor>().is_some());
    assert!(world.get_resource::<RollbackBudget>().is_some());
    assert!(world.get_resource::<TickBridge>().is_some());

    // orrery_witness
    assert!(world.get_resource::<AuthoredLog>().is_some());
    assert!(world.get_resource::<WitnessSet>().is_some());

    // orrery_persist_client
    assert!(world.get_resource::<GatewaySession>().is_some());
    assert!(world.get_resource::<UplinkScheduler>().is_some());
    assert!(world.get_resource::<AreaLoader>().is_some());
    assert!(world.get_resource::<IntentQueue>().is_some());
}

#[test]
fn coordinator_manifest_lets_a_peer_mint_ephemeral_ids() {
    // A coordinator address, so `follow_sessions_without_coordinator` is not
    // installed: the no-coordinator fallback re-derives membership from the
    // connected-session set every frame, which is the wrong path for a
    // manifest. The address is never reachable and never needs to be — this
    // test drives the manifest in by hand.
    let config = OrreryConfig::default().with_coordinator(CoordinatorConfig {
        address: Some(iroh::EndpointAddr::new(node_id(3))),
        ..CoordinatorConfig::default()
    });
    let mut app = client(config);

    // The peer's identity travels gateway session -> `AuthorityState` ->
    // `EphemeralRegistry` (`sync_authority_identity`, then
    // `track_island_binding`), so it is seeded where the shipping path seeds
    // it; writing `AuthorityState` directly would be overwritten this frame.
    let local = node_id(1);
    app.world_mut().resource_mut::<GatewaySession>().node = local;
    // And on `AuthorityState` too: the two systems that carry the identity
    // across are not ordered against each other, so on the first frame
    // `track_island_binding` may read the state before
    // `sync_authority_identity` has written it. Seeding both is what a peer
    // looks like from the second frame on.
    app.world_mut().resource_mut::<AuthorityState>().node = local;

    // A manifest is the coordinator's membership handout; the roster it carries
    // includes the recipient, which `apply_manifest` filters out.
    let manifest = IslandManifest {
        island: IslandId::new(42),
        epoch: 7,
        cells: vec![CellId::ROOT],
        regime: TopologyRegime::Mesh,
        peers: vec![
            PeerEntry {
                node: local,
                cells: vec![CellId::ROOT],
            },
            PeerEntry {
                node: node_id(2),
                cells: vec![CellId::ROOT],
            },
        ],
    };
    app.world_mut()
        .resource_mut::<IslandMembership>()
        .apply_manifest(&manifest, local)
        .expect("the first manifest is never stale");

    // Before the wire runs, the registry has no namespace to mint into.
    assert!(app
        .world_mut()
        .resource_mut::<EphemeralRegistry>()
        .spawn(Tick::new(1))
        .is_none());

    app.update();

    assert_eq!(
        app.world().resource::<IslandMembership>().source,
        IslandSource::Coordinator
    );

    let binding = app.world().resource::<IslandBinding>();
    assert_eq!(binding.island, Some(IslandId::new(42)));
    assert_eq!(binding.epoch, 7, "the manifest epoch reached the binding");

    let mut registry = app.world_mut().resource_mut::<EphemeralRegistry>();
    assert_eq!(registry.island(), Some(IslandId::new(42)));
    let id = registry
        .spawn(Tick::new(2))
        .expect("a peer with an island assignment can mint ephemeral ids");
    assert_eq!(id.island, IslandId::new(42));
    assert_eq!(id.spawner, local);
}
