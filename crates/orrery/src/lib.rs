//! The Orrery client facade (D15, docs/10-crates.md §10).
//!
//! One crate for a game to depend on: [`OrreryClientPlugins`] composes the
//! client plugins in dependency order, [`OrreryConfig`] aggregates their
//! configurations, and [`prelude`] re-exports what a client app names.
//!
//! There is no subsystem logic here and there deliberately cannot be. A
//! `PluginGroup` has to live in a crate that depends on every member plugin,
//! and none of the functional crates can do that without inverting the spine —
//! which is also why the one system this crate owns lives here: the
//! [`IslandMembership`](orrery_net::IslandMembership) →
//! [`IslandBinding`](orrery_authority::IslandBinding) wire crosses `orrery_net`
//! and `orrery_authority`, authority is the lower layer of the two, and the net
//! layer pushes rather than the authority layer pulling.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use orrery::prelude::*;
//! use orrery_games::Skirmish;
//!
//! App::new()
//!     .add_plugins(DefaultPlugins)
//!     .add_plugins(OrreryClientPlugins::<Skirmish>::new(
//!         OrreryConfig::default().with_coordinator(CoordinatorConfig::default()),
//!     ))
//!     .run();
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bevy_app::{App, Plugin, PluginGroup, PluginGroupBuilder};
use bevy_ecs::prelude::*;

use orrery_authority::{track_island_binding, IslandBinding, OrreryAuthorityPlugin};
use orrery_core::Ruleset;
use orrery_net::plugin::NetConfig;
use orrery_net::{CoordinatorConfig, IslandMembership, OrreryNetPlugin};
use orrery_persist_client::{OrreryPersistClientPlugin, PersistClientConfig};
use orrery_predict::{ConfigDefect, OrreryPredictPlugin, PredictConfig};
use orrery_spatial::{OrrerySpatialPlugin, SpatialConfig};
use orrery_witness::WitnessPlugin;

pub mod prelude;

/// The aggregate client configuration: one struct carrying every member
/// plugin's config.
///
/// # Why the fields are private
///
/// [`OrreryPredictPlugin::build`] asserts on [`PredictConfig::validate`],
/// because a configuration that breaks docs/05 §12's couplings produces a game
/// that runs and is quietly wrong. A config type that could hand the group a
/// broken `PredictConfig` would turn that assert into a panic at app start, so
/// this one cannot: [`OrreryConfig::with_predict`] is the only way in and it
/// returns the defects instead of taking them.
///
/// # Why the default disables relays
///
/// [`OrreryNetPlugin`]'s Startup system opens a real iroh endpoint, and
/// [`NetConfig::default`] names iroh's production relay map. A default that
/// reached the relay fleet would make every headless test and every offline
/// run depend on n0's infrastructure, so the facade's default is
/// [`iroh::RelayMode::Disabled`] and a game that wants relays says so.
#[derive(Debug, Clone)]
pub struct OrreryConfig {
    net: NetConfig,
    coordinator: CoordinatorConfig,
    spatial: SpatialConfig,
    predict: PredictConfig,
    persist: PersistClientConfig,
}

impl Default for OrreryConfig {
    fn default() -> Self {
        Self {
            net: NetConfig {
                relay_mode: iroh::RelayMode::Disabled,
                secret_key: None,
            },
            coordinator: CoordinatorConfig::default(),
            spatial: SpatialConfig::default(),
            predict: PredictConfig::default(),
            persist: PersistClientConfig::default(),
        }
    }
}

impl OrreryConfig {
    /// The endpoint configuration.
    #[must_use]
    pub fn net(&self) -> &NetConfig {
        &self.net
    }

    /// The coordinator address, credentials, and trusted issuer keys.
    #[must_use]
    pub fn coordinator(&self) -> &CoordinatorConfig {
        &self.coordinator
    }

    /// The spatial configuration: cell edge, hysteresis, AOI.
    #[must_use]
    pub fn spatial(&self) -> &SpatialConfig {
        &self.spatial
    }

    /// The prediction configuration. Always one that
    /// [`PredictConfig::validate`] accepts — see the type docs.
    #[must_use]
    pub fn predict(&self) -> &PredictConfig {
        &self.predict
    }

    /// The client persistence configuration.
    #[must_use]
    pub fn persist(&self) -> &PersistClientConfig {
        &self.persist
    }

    /// Set the endpoint configuration.
    #[must_use]
    pub fn with_net(mut self, net: NetConfig) -> Self {
        self.net = net;
        self
    }

    /// Set the coordinator configuration.
    #[must_use]
    pub fn with_coordinator(mut self, coordinator: CoordinatorConfig) -> Self {
        self.coordinator = coordinator;
        self
    }

    /// Set the spatial configuration.
    #[must_use]
    pub fn with_spatial(mut self, spatial: SpatialConfig) -> Self {
        self.spatial = spatial;
        self
    }

    /// Set the client persistence configuration.
    #[must_use]
    pub fn with_persist(mut self, persist: PersistClientConfig) -> Self {
        self.persist = persist;
        self
    }

    /// Set the prediction configuration.
    ///
    /// # Errors
    ///
    /// The [`ConfigDefect`]s the candidate breaks, when it breaks docs/05 §12's
    /// coupling invariants. Refusing here is the whole point of the private
    /// field: the alternative is the plugin's assert firing at app start.
    pub fn with_predict(mut self, predict: PredictConfig) -> Result<Self, Vec<ConfigDefect>> {
        let defects = predict.validate();
        if !defects.is_empty() {
            return Err(defects);
        }
        self.predict = predict;
        Ok(self)
    }
}

/// Mirrors [`IslandMembership`] into [`IslandBinding`] once per frame.
///
/// The wire named in docs/11-roadmap.md §P3: without it `IslandBinding` is
/// written by nothing, so [`EphemeralRegistry::spawn`] returns `None` on its
/// first line and no peer can mint an [`EphemeralId`] at all.
///
/// [`EphemeralRegistry::spawn`]: orrery_authority::ephemeral::EphemeralRegistry::spawn
/// [`EphemeralId`]: orrery_protocol::EphemeralId
///
/// Guarded on the compared values rather than written unconditionally:
/// `track_island_binding` reacts to `IslandBinding`'s change flag, and
/// re-stamping the same island every frame would dirty the registry every frame
/// for a binding that never moved.
pub fn bind_island_membership(
    membership: Res<IslandMembership>,
    mut binding: ResMut<IslandBinding>,
) {
    if binding.island != membership.island || binding.epoch != membership.epoch {
        binding.island = membership.island;
        binding.epoch = membership.epoch;
    }
}

/// Installs [`bind_island_membership`], ordered before its consumer.
///
/// A `PluginGroup` can only add plugins, so the facade's one system needs one
/// plugin to carry it. It is a member of [`OrreryClientPlugins`] rather than
/// something a game adds separately, because a client that has both the net and
/// the authority plugin and not this wire is the broken configuration the wire
/// exists to prevent.
#[derive(Debug, Default, Clone, Copy)]
pub struct OrreryIslandBindingPlugin;

impl Plugin for OrreryIslandBindingPlugin {
    fn build(&self, app: &mut App) {
        // Before, not after: the binding a manifest produced this frame reaches
        // the ephemeral registry in the same frame, rather than leaving one
        // frame in which a spawn mints into the previous island's namespace.
        app.add_systems(
            bevy_app::Update,
            bind_island_membership.before(track_island_binding),
        );
    }
}

/// The Orrery client plugin group (D15), in dependency order.
///
/// `R` is the game's ruleset, needed by [`WitnessPlugin`]. The group does not
/// insert `WitnessState`: the app inserts it once it knows the universe seed,
/// so that a peer can join a universe before it has one.
///
/// # Members
///
/// 1. [`OrreryNetPlugin`] — the iroh endpoint, peers, coordinator client,
///    island membership. It adds aeronet's `IrohPlugin` itself; there is no
///    separate transport plugin.
/// 2. [`OrrerySpatialPlugin`] — cell commitment, AOI, interest set.
/// 3. [`OrreryAuthorityPlugin`] — claims, leases, the ephemeral namespace.
/// 4. [`OrreryIslandBindingPlugin`] — this crate's own wire, between the two
///    above.
/// 5. [`OrreryPredictPlugin`] — lightyear's client stack, per D8/D16.
/// 6. [`WitnessPlugin<R>`] — the log stream and the discrepancy path.
/// 7. [`OrreryPersistClientPlugin`] — the gateway session, uplink, area loader.
///
/// # What the group deliberately leaves out
///
/// [`AoiVisibilityPlugin`](orrery_spatial::visibility::AoiVisibilityPlugin) is
/// not a member. Its `AoiVisibilityBit` is built `FromWorld` out of replicon's
/// `FilterRegistry` and `ReplicationRegistry`, so adding it before
/// `RepliconPlugins` panics — and the group cannot add `RepliconPlugins`
/// itself, since a game configures replication (and lightyear brings its own
/// copy). Add it after `RepliconPlugins`, from the game.
///
/// # Overriding a member
///
/// Ordinary `PluginGroupBuilder` surgery applies: `.set(…)` a member with a
/// different configuration, `.disable::<…>()` one the game replaces.
pub struct OrreryClientPlugins<R: Ruleset> {
    config: OrreryConfig,
    marker: core::marker::PhantomData<fn() -> R>,
}

impl<R: Ruleset> OrreryClientPlugins<R> {
    /// The group for a game whose rules are `R`, from `config`.
    #[must_use]
    pub fn new(config: OrreryConfig) -> Self {
        Self {
            config,
            marker: core::marker::PhantomData,
        }
    }

    /// The configuration this group will build its members from.
    #[must_use]
    pub fn config(&self) -> &OrreryConfig {
        &self.config
    }
}

impl<R: Ruleset> Default for OrreryClientPlugins<R> {
    fn default() -> Self {
        Self::new(OrreryConfig::default())
    }
}

impl<R: Ruleset + Send + Sync + 'static> PluginGroup for OrreryClientPlugins<R>
where
    R::CoreState: Send + Sync,
    R::CoreInput: Send + Sync,
{
    fn build(self) -> PluginGroupBuilder {
        let config = self.config;
        PluginGroupBuilder::start::<Self>()
            .add(OrreryNetPlugin {
                config: config.net,
                coordinator: config.coordinator,
            })
            .add(OrrerySpatialPlugin {
                config: config.spatial,
            })
            .add(OrreryAuthorityPlugin)
            .add(OrreryIslandBindingPlugin)
            .add(OrreryPredictPlugin {
                config: config.predict,
            })
            .add(WitnessPlugin::<R>::new())
            .add(OrreryPersistClientPlugin {
                config: config.persist,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::system::RunSystemOnce;
    use orrery_protocol::IslandId;

    #[test]
    fn default_config_disables_relays_and_validates_predict() {
        let config = OrreryConfig::default();
        assert!(matches!(config.net().relay_mode, iroh::RelayMode::Disabled));
        assert!(config.predict().validate().is_empty());
    }

    #[test]
    fn with_predict_refuses_a_broken_retune() {
        // Halving the send rate alone starves the interpolation buffer, which
        // docs/05 §12 states as exactly two send intervals.
        let broken = PredictConfig {
            send_hz: 5,
            ..PredictConfig::default()
        };
        let defects = OrreryConfig::default()
            .with_predict(broken)
            .expect_err("a config breaking §12's couplings must be refused");
        assert!(!defects.is_empty());
    }

    #[test]
    fn binding_mirrors_membership() {
        let mut world = World::new();
        world.insert_resource(IslandMembership {
            island: Some(IslandId::new(9)),
            epoch: 4,
            ..IslandMembership::default()
        });
        world.init_resource::<IslandBinding>();
        world
            .run_system_once(bind_island_membership)
            .expect("system runs");

        let binding = world.resource::<IslandBinding>();
        assert_eq!(binding.island, Some(IslandId::new(9)));
        assert_eq!(binding.epoch, 4);
    }
}
