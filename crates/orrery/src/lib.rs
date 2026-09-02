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
//! There is now a second such system, [`queue_filed_reports`], and it is here
//! for the same reason: `orrery_witness` files a signed `DiscrepancyReport`
//! and has no gateway session, `orrery_persist_client` has the session and
//! must not learn about witnessing, and only this crate depends on both.
//!
//! And a third, [`divest_on_drain`]: a coordinator drain order lands on
//! `orrery_net`'s session and the leases it releases are `orrery_authority`'s,
//! which is the same crossing as the first system and in the same direction.
//! The exception is meant to stay this narrow — every one of the three moves a
//! value between two resources and decides nothing beyond *whether* the move
//! applies.
//!
//! [`hit`] adds the fourth and fifth, and they are here under the same rule
//! rather than a relaxed one (#871). `orrery_authority` owns the pose ring and
//! may not name `orrery_net`'s peer link; `orrery_net` must not learn what a
//! pose is; and a game crate is Bevy-free and sits below both. So the wire
//! that carries a game's canonical pose into the ring, and the wire that
//! carries a claim off a link to the ring and the verdict back, can only be
//! written where every one of those crates is already a dependency.
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

use orrery_authority::{
    track_island_binding, Authority, IslandBinding, LeaseClient, LeaseDivest, OrreryAuthorityPlugin,
};
use orrery_core::Ruleset;
use orrery_net::plugin::NetConfig;
use orrery_net::{
    apply_island_drain, CoordinatorConfig, CoordinatorLink, IslandMembership, OrreryNetPlugin,
};
use orrery_persist_client::{
    AuthorityCorrectionQueue, OrreryPersistClientPlugin, PersistClientConfig, ReportQueue,
};
use orrery_predict::{
    AuthorityCorrectionInbox, ConfigDefect, OrreryPredictPlugin, OrreryReplicationBridgePlugin,
    PredictConfig, HIGH_RATE_SET,
};
use orrery_protocol::SeqPair;
use orrery_spatial::{OrrerySpatialPlugin, SpatialConfig};
use orrery_witness::{ReportFiled, WitnessPlugin};

pub mod hit;
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

/// Releases this peer's leases when the coordinator orders its island drained
/// (D24).
///
/// The facade's **third** cross-crate system, and the same rule puts it here as
/// the other two: the order arrives on `orrery_net`'s coordinator session, the
/// leases live in `orrery_authority`, authority is the lower layer of the two,
/// and `orrery_authority` may not depend on `orrery_net`. Only this crate
/// depends on both.
///
/// It is ordered **before**
/// [`apply_island_drain`], which is the system
/// that consumes [`CoordinatorLink::drain`](orrery_net::CoordinatorLink::drain)
/// and calls `IslandMembership::leave()`. Divesting has to see the membership
/// the order names; once `leave()` has run there is no island left to compare
/// against.
///
/// # Why the island filter is just the membership check
///
/// D24 §(a) states the drain predicate per entity, over the cell set `C(I)` of
/// the drained island: release every lease whose committed cell is in it. There
/// is no cell set to intersect against here, and there does not need to be. A
/// peer is in exactly one island at a time
/// ([`IslandMembership::island`](orrery_net::IslandMembership::island) is one
/// `Option`), the island's cells are the union of its peers' reported presence,
/// and a lease is granted only over a cell this peer's interest covers. So
/// `membership.island == Some(island)` *is* the containment test, and every
/// lease this peer holds satisfies it. Filtering again on a cell set the peer
/// would have to reconstruct from a manifest it may no longer hold would be a
/// second, weaker copy of the same fact.
///
/// The substitution is also forced rather than merely convenient:
/// `CoordMsg::Drain` is `{ island, deadline }` and carries no cell set, so when
/// the two disagree there is nothing else to test against. That is the move
/// case — a peer that emptied island `A` by joining `B` is told to drain `A`
/// while holding `B`, takes the early return here, and lets `A`'s rows park on
/// the registrar's 11 s expiry sweep rather than one RTT. An accepted latency
/// cost on an already-correct backstop, not an oversight; see
/// [`apply_island_drain`], which documents the
/// case in full.
///
/// # `to: None`, `cursor: None`
///
/// The sanctioned cooperative release (D7 §5, docs/04 §5): there is no
/// successor, because the island is being retired rather than handed over —
/// naming one would be an evacuation, which D24 §(b) explicitly declines to
/// invent. The registrar's uplink-completeness gate accepts an absent cursor on
/// exactly that condition, and refuses one otherwise
/// (`orrery_persistd::gateway`'s `(None, _) => to.is_none()`), so the two
/// `None`s are one decision rather than two.
///
/// `final_seq` is the entity's last known [`Authority::seq`], for the reason
/// the expiry path carries the pair forward: INV-2 forbids the sequences going
/// backwards, and a default `(0, 0)` is a pair the registrar's row supersedes.
///
/// # This system is an optimisation, and must stay one
///
/// If the order never arrives — the usual case being a peer that crashed, which
/// has no session to receive it on — nothing here runs and the drain still
/// completes: the registrar's 1 s expiry sweep parks every row within
/// `TTL + S = 11 s` with no message at all (D24 §(a), path 3). What this buys
/// is the difference between 11 s and one round trip on a graceful departure.
/// Nothing downstream may be written so that it is only correct when the notice
/// was delivered.
pub fn divest_on_drain(
    link: Res<CoordinatorLink>,
    membership: Res<IslandMembership>,
    mut client: LeaseClient,
    authority: Query<&Authority>,
) {
    let Some((island, _deadline)) = link.drain else {
        return;
    };
    // The deadline is read and discarded deliberately. D24 §(d) sets the grace
    // at exactly one lease TTL and says a peer past it "is not punished — it is
    // simply no longer the reason anything is waiting", so there is nothing to
    // enforce: the response is to divest now, in this frame, which is the only
    // schedule this system has.
    if membership.island != Some(island) {
        return;
    }
    for (persist, entity) in client.held_leases() {
        let final_seq = authority
            .get(entity)
            .map_or_else(|_| SeqPair::default(), |known| known.seq);
        client.divest(LeaseDivest {
            entity,
            persist,
            to: None,
            final_seq,
            cursor: None,
        });
    }
}

/// Carries a filed discrepancy report from the witness to the gateway egress.
///
/// The facade's **second** cross-crate system, and it exists for exactly the
/// reason the first one does — see this module's docs. `orrery_witness` files
/// reports and has no gateway session; `orrery_persist_client` owns the
/// gateway session and must not learn about witnessing. The two sit side by
/// side on the dependency spine (D15), neither may reach for the other, and
/// this crate is the only one that depends on both.
///
/// The exception stays narrow deliberately: this moves a value between two
/// resources and decides nothing. The judgement about *whether* to file lives
/// in `orrery_witness` (shadow mode, the audit window, the signing identity),
/// and the judgement about how to send lives in `orrery_persist_client` (the
/// bounded queue, the reliable lane).
pub fn queue_filed_reports(
    mut filed: MessageReader<ReportFiled>,
    mut reports: ResMut<ReportQueue>,
) {
    for report in filed.read() {
        // A clone, because a message reader borrows and other readers (a
        // game's own telemetry, most obviously) are entitled to see the same
        // message. It is one bundle per divergence episode, not per frame —
        // the engine signals a disputed claim once — so the copy is paid at
        // the rate accusations happen rather than at the tick rate.
        reports.push(report.report.clone());
    }
}

/// Move signature-verified gateway corrections into prediction reconciliation.
///
/// This is the same narrow facade crossing as [`queue_filed_reports`], in the
/// opposite direction: the persistence client owns gateway trust and
/// `orrery_predict` owns rollback-versus-snap. Neither lower crate depends on
/// the other, so the facade moves the value and makes no decision about it.
pub fn queue_authority_corrections(
    mut verified: ResMut<AuthorityCorrectionQueue>,
    mut reconciliation: ResMut<AuthorityCorrectionInbox>,
) {
    while let Some(correction) = verified.pop() {
        reconciliation.push(correction);
    }
}

/// Installs [`queue_filed_reports`].
///
/// A `PluginGroup` can only add plugins, so this carries the wire the same way
/// [`OrreryIslandBindingPlugin`] carries the other one. It is a member of
/// [`OrreryClientPlugins`] rather than something a game adds, because a client
/// with both the witness and the persist plugin and not this wire is a client
/// whose reports are assembled, signed, and dropped on the floor.
#[derive(Debug, Default, Clone, Copy)]
pub struct OrreryEscalationPlugin;

impl Plugin for OrreryEscalationPlugin {
    fn build(&self, app: &mut App) {
        // Before the drain, so a report filed this frame leaves in it rather
        // than waiting a frame per hop.
        app.add_systems(
            bevy_app::Update,
            (
                queue_filed_reports.before(orrery_persist_client::drain_reports),
                queue_authority_corrections.before(orrery_predict::reconcile_authority_corrections),
            ),
        );
    }
}

/// Installs the two net↔authority island systems, each ordered against its
/// consumer: [`bind_island_membership`] and [`divest_on_drain`].
///
/// A `PluginGroup` can only add plugins, so the facade's systems need a plugin
/// to carry them. It is a member of [`OrreryClientPlugins`] rather than
/// something a game adds separately, because a client that has both the net and
/// the authority plugin and not these wires is the broken configuration they
/// exist to prevent — no island binding means no ephemeral namespace, and no
/// drain wire means a drained peer whose leases sit until the registrar's
/// expiry sweep collects them.
#[derive(Debug, Default, Clone, Copy)]
pub struct OrreryIslandBindingPlugin;

impl Plugin for OrreryIslandBindingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            bevy_app::Update,
            (
                // Before the tear-down, because divesting needs to see the
                // membership the order names — `apply_island_drain` clears it.
                divest_on_drain.before(apply_island_drain),
                // After the tear-down and before the registry: a drain that
                // cleared the island reaches `IslandBinding` in the frame it
                // happened, so `EphemeralRegistry::spawn` stops minting into a
                // namespace this peer has left rather than minting for one more
                // frame. The same edge carries the ordinary case — the binding a
                // manifest produced this frame reaches the registry in it.
                bind_island_membership
                    .after(apply_island_drain)
                    .before(track_island_binding),
            ),
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
/// 3. [`OrreryAuthorityPlugin`] — claims, leases, the ephemeral namespace,
///    and the pose ring hit claims are validated against, sized from
///    [`PredictConfig::hit_window`].
/// 4. [`OrreryIslandBindingPlugin`] — this crate's own wires, between the two
///    above: the membership binding, and the drain divestiture.
/// 5. [`OrreryPredictPlugin`] — lightyear's client stack, per D8/D16.
/// 6. [`OrreryReplicationBridgePlugin`] — established Orrery sessions become
///    P2P replication links with a Replicon-backed sender and receiver.
/// 7. [`WitnessPlugin<R>`] — the log stream and the discrepancy path.
/// 8. [`OrreryPersistClientPlugin`] — the gateway session, uplink, area loader.
/// 9. [`OrreryEscalationPlugin`] — this crate's other wire, from the witness's
///    filed reports to the gateway's report queue.
///
/// # What the host must drive
///
/// Every other resource the members register is either configuration or is
/// written by a system the group installs. Four are not, and a game that
/// leaves them alone gets a subsystem that runs and quietly does nothing — so
/// they are enumerated here, and `crates/orrery/tests/client_group.rs` pins the
/// list. All four carry the same reason: their units are the *game's*, and no
/// plugin here can invent them.
///
/// | Resource | Written from | What is lost without it |
/// |---|---|---|
/// | [`ContactObservations`](orrery_authority::ContactObservations) | the physics step's contact report | contact-island weak claims: the planner sees an empty graph and proposes nothing (D7 §5) |
/// | [`ContactTick::tick`](orrery_authority::ContactTick) | the universe tick that step ran on | every weak claim carries `ClaimBasis::Contact{tick: 0}` as its evidence, which the registrar's plausibility gate reads |
/// | [`CanonicalPosePublications`](orrery_authority::CanonicalPosePublications) | each held entity's canonical end-of-step pose and that step's universe tick — write [`CanonicalPose`](hit::CanonicalPose) and add [`OrreryHitRegistrationPlugin`](hit::OrreryHitRegistrationPlugin) | no hit claim can be validated: every [`HitClaim`](orrery_protocol::HitClaim) returns `BasisNotRetained`, so hit registration silently does nothing |
/// | [`WitnessClock`](orrery_witness::plugin::WitnessClock) | the same universe tick | the repair-timeout sweep, which is the only check a subject that goes *silent* can trip (D10) |
///
/// One more resource is host-supplied and is *configuration* rather than
/// per-frame state, so it is not in the table:
/// [`WitnessIdentity`](orrery_witness::WitnessIdentity), the key a witness
/// signs discrepancy reports with. Absent, the witness detects and counts and
/// escalates nothing — filing is opt-in, and shadow mode
/// ([`WitnessConfig::shadow_mode`](orrery_witness::WitnessConfig::shadow_mode))
/// is on by default besides. No plugin here can invent it: `NetConfig`'s
/// secret key is consumed into the iroh endpoint and never handed back.
///
/// [`OrreryHitRegistrationPlugin`](hit::OrreryHitRegistrationPlugin) narrows the
/// pose row without removing it, and it is deliberately *not* a member of this
/// group: it is generic over the game's [`HitRules`](orrery_authority::HitRules)
/// table, which a [`Ruleset`] does not imply and this group cannot invent. A
/// game that adds it still writes [`CanonicalPose`](hit::CanonicalPose) itself
/// — the pose and its tick remain the game's — but no longer has to know that
/// `CanonicalPosePublications` exists.
///
/// The game also registers its replicated component schemas and their
/// interpolation/correction policy, then attaches replication and prediction
/// targets to its entities. Those are type- and gameplay-specific declarations
/// that the generic facade cannot infer; the facade supplies their transport
/// and sender once declared.
///
/// `ContactTick::now_ms` is **not** on that list, and neither is
/// [`TickBridge`](orrery_predict::TickBridge): those are clocks rather than
/// game state, so `OrreryAuthorityPlugin` and `OrreryPredictPlugin` drive them
/// themselves.
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
        let replication_bridge = OrreryReplicationBridgePlugin {
            tick_duration: config.predict.tick_duration(),
        };
        assert_eq!(
            config.spatial.high_rate_cap,
            usize::from(HIGH_RATE_SET),
            "SpatialConfig.high_rate_cap must equal orrery_predict::HIGH_RATE_SET; the rollback budget halves HIGH_RATE_SET under load"
        );
        assert_eq!(
            config.predict.tick_hz,
            orrery_core::TICK_HZ,
            "PredictConfig.tick_hz must equal orrery_core::TICK_HZ; prediction and canonical simulation cannot run on different tick bases"
        );
        PluginGroupBuilder::start::<Self>()
            .add(OrreryNetPlugin {
                config: config.net,
                coordinator: config.coordinator,
            })
            .add(OrrerySpatialPlugin {
                config: config.spatial,
            })
            // The pose ring's depth and the rewind cap are `orrery_predict`'s
            // derivations (docs/05 §7: 12 + 6 + 9 → 32) and the ring is
            // `orrery_authority`'s; neither may depend on the other, so the
            // numbers cross here.
            .add(OrreryAuthorityPlugin::default().with_hit_window(config.predict.hit_window()))
            .add(OrreryIslandBindingPlugin)
            .add(OrreryPredictPlugin {
                config: config.predict,
            })
            .add(replication_bridge)
            .add(WitnessPlugin::<R>::new())
            .add(OrreryPersistClientPlugin {
                config: config.persist,
            })
            // After both endpoints exist: the wire is between them, and
            // ordering against `drain_reports` names a system the persist
            // plugin registers.
            .add(OrreryEscalationPlugin)
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
    #[should_panic(expected = "SpatialConfig.high_rate_cap must equal")]
    fn group_refuses_a_high_rate_double_edit() {
        let config = OrreryConfig::default().with_spatial(SpatialConfig {
            high_rate_cap: usize::from(HIGH_RATE_SET) - 1,
            ..SpatialConfig::default()
        });
        let _ = OrreryClientPlugins::<FacadeRules>::new(config).build();
    }

    #[test]
    #[should_panic(expected = "PredictConfig.tick_hz must equal")]
    fn group_refuses_a_split_tick_basis() {
        let predict = PredictConfig {
            tick_hz: 30,
            send_hz: 10,
            rollback_ticks: 5,
            interp_buffer: core::time::Duration::from_millis(200),
            redundant_input_ticks: 6,
            ..PredictConfig::default()
        };
        let config = OrreryConfig::default()
            .with_predict(predict)
            .expect("the coupled 30 Hz prediction retune is internally valid");
        let _ = OrreryClientPlugins::<FacadeRules>::new(config).build();
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

    /// A private zero-behaviour ruleset for tests that exercise group build
    /// validation without depending on a game crate.
    struct FacadeRules;

    impl Ruleset for FacadeRules {
        type CoreState = FacadeState;
        type CoreInput = FacadeNever;
        type CoreEvent = FacadeNever;

        fn id(&self) -> orrery_protocol::RulesetId {
            orrery_protocol::RulesetId {
                version: 1,
                digest: [0x87; 32],
            }
        }

        fn step(
            &self,
            _view: &mut orrery_core::StateView<'_, Self::CoreState>,
            _inputs: &orrery_core::OrderedInputs<'_, Self::CoreInput>,
            _rng: &mut orrery_core::TickRng,
        ) -> orrery_core::StepOutput<Self::CoreEvent> {
            orrery_core::StepOutput::default()
        }
    }

    #[derive(Clone)]
    struct FacadeState;

    impl orrery_core::Quantized for FacadeState {
        fn quantize(&mut self) {}
    }

    impl orrery_core::CoreCodec for FacadeState {
        fn encode(&self, _out: &mut Vec<u8>) {}

        fn decode(bytes: &[u8]) -> Result<Self, orrery_core::CodecError> {
            if bytes.is_empty() {
                Ok(Self)
            } else {
                Err(orrery_core::CodecError("facade state is empty"))
            }
        }
    }

    #[derive(Clone)]
    enum FacadeNever {}

    impl orrery_core::CoreCodec for FacadeNever {
        fn encode(&self, _out: &mut Vec<u8>) {
            match *self {}
        }

        fn decode(_bytes: &[u8]) -> Result<Self, orrery_core::CodecError> {
            Err(orrery_core::CodecError("facade input/event is uninhabited"))
        }
    }
}
