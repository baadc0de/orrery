//! Orrery's guarded facade over `bevy_replicon`.
//!
//! This crate is the only first-party crate allowed to declare
//! `bevy_replicon`. Its registration API requires [`EngineHandleFree`], while
//! its non-registration exports are deliberately narrow. A caller therefore
//! cannot reach replicon's unguarded registration traits without first adding
//! a direct dependency that `scripts/core-gates.sh` refuses.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy_app::App;
use bevy_ecs::{component::Component, resource::Resource, world::World};
use bevy_replicon::shared::replication::{
    diff::Diffable,
    registry::{receive_fns::MutWrite, rule_fns as raw_rule_fns},
    rules::{
        component::{
            BundleRules as RawBundleRules, IntoComponentRule as RawIntoComponentRule,
            IntoComponentRules as RawIntoComponentRules, IntoResourceRule as RawIntoResourceRule,
            ReplicationMode,
        },
        filter::FilterRules,
        AppRuleExt as RawAppRuleExt,
    },
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

mod sealed {
    pub trait EngineHandleFree {}
    pub trait ComponentRule {}
    pub trait ComponentRules {}
    pub trait ResourceRule {}
    pub trait BundleRules {}
}

/// A sealed marker for payload shapes that contain no engine-local handles.
///
/// The facade implements this trait only for scalar values, selected
/// engine-neutral protocol values, and containers whose contents also carry
/// the marker. In particular there is no implementation for Bevy `Entity`,
/// `ComponentId`, or any other `bevy_ecs` type.
pub trait EngineHandleFree: sealed::EngineHandleFree {}

macro_rules! impl_engine_handle_free {
    ($($ty:ty),+ $(,)?) => {$(
        impl sealed::EngineHandleFree for $ty {}
        impl EngineHandleFree for $ty {}
    )+};
}

impl_engine_handle_free!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    String,
    orrery_protocol::AccountId,
    orrery_protocol::AccountInvalidation,
    orrery_protocol::AccountStandingUpdate,
    orrery_protocol::AccountStandings,
    orrery_protocol::AreaPage,
    orrery_protocol::AssetId,
    orrery_protocol::Attestation,
    orrery_protocol::AttestationRefusalReason,
    orrery_protocol::AttestationVerdict,
    orrery_protocol::AuthorityCorrectionClaimsV1,
    orrery_protocol::AuthorityCorrectionV1,
    orrery_protocol::AuthorityCorrectionVerificationError,
    orrery_protocol::CampaignJoinFileV1,
    orrery_protocol::CampaignJoinFileVersionError,
    orrery_protocol::CellEpoch,
    orrery_protocol::CellId,
    orrery_protocol::CellRangeError,
    orrery_protocol::ChainHash,
    orrery_protocol::Checkpoint,
    orrery_protocol::ClaimBasis,
    orrery_protocol::ClaimId,
    orrery_protocol::ClaimKind,
    orrery_protocol::CoordMsg,
    orrery_protocol::CoordinatorInterestSnapshot,
    orrery_protocol::DenyReason,
    orrery_protocol::DeviationKind,
    orrery_protocol::DiffUplink,
    orrery_protocol::DiscrepancyReport,
    orrery_protocol::EntityRekey,
    orrery_protocol::EntitySlice,
    orrery_protocol::Epoch,
    orrery_protocol::EvidenceBundle,
    orrery_protocol::EvidenceCommitment,
    orrery_protocol::ExpireDisposition,
    orrery_protocol::ExpireReason,
    orrery_protocol::FixedTokenClock,
    orrery_protocol::ForgeryProof,
    orrery_protocol::FrameHead,
    orrery_protocol::GatewayMsg,
    orrery_protocol::GatewayReply,
    orrery_protocol::GridId,
    orrery_protocol::InputRecord,
    orrery_protocol::Intent,
    orrery_protocol::IntentContextRef,
    orrery_protocol::IntentOp,
    orrery_protocol::IntentOutcome,
    orrery_protocol::IntentProposal,
    orrery_protocol::IntentResponse,
    orrery_protocol::InterestCellCrossing,
    orrery_protocol::InterestGrantClaimsV1,
    orrery_protocol::InterestGrantV1,
    orrery_protocol::InterestGrantVerificationError,
    orrery_protocol::IslandId,
    orrery_protocol::IslandManifest,
    orrery_protocol::IssuerKey,
    orrery_protocol::IssuerKeyId,
    orrery_protocol::ItemUid,
    orrery_protocol::JournalRecord,
    orrery_protocol::Lease,
    orrery_protocol::LeaseFlags,
    orrery_protocol::LeaseId,
    orrery_protocol::LeaseMsg,
    orrery_protocol::LogFrame,
    orrery_protocol::LogRangeRequest,
    orrery_protocol::LogRangeResponse,
    orrery_protocol::Lsn,
    orrery_protocol::NodeId,
    orrery_protocol::PeerEntry,
    orrery_protocol::PersistId,
    orrery_protocol::QueuedStandingUpdates,
    orrery_protocol::RecordKind,
    orrery_protocol::RecordSource,
    orrery_protocol::RollingHead,
    orrery_protocol::RulesetId,
    orrery_protocol::SeqPair,
    orrery_protocol::SessionStanding,
    orrery_protocol::SessionTokenClaimsV1,
    orrery_protocol::SessionTokenTtlMs,
    orrery_protocol::SessionTokenV1,
    orrery_protocol::SessionTokenVerificationError,
    orrery_protocol::Signature,
    orrery_protocol::StateClaim,
    orrery_protocol::Tick,
    orrery_protocol::TopologyRegime,
    orrery_protocol::UnadjudicableReason,
    orrery_protocol::UniverseSeed,
    orrery_protocol::UnixMillis,
    orrery_protocol::Verdict,
    orrery_protocol::VersionedError,
    orrery_protocol::WitnessEpochClaimsV1,
    orrery_protocol::WitnessEpochSnapshot,
    orrery_protocol::WitnessEpochV1,
    orrery_protocol::WitnessEpochVerificationError,
    orrery_protocol::WitnessMsg,
);

macro_rules! impl_container {
    ($container:ident < $($parameter:ident),+ >) => {
        impl<$($parameter: EngineHandleFree),+> sealed::EngineHandleFree
            for $container<$($parameter),+>
        {}
        impl<$($parameter: EngineHandleFree),+> EngineHandleFree for $container<$($parameter),+> {}
    };
}

impl_container!(Option<T>);
impl_container!(Box<T>);
impl_container!(Vec<T>);
impl_container!(VecDeque<T>);
impl_container!(BTreeSet<T>);
impl_container!(BTreeMap<K, V>);
impl_container!(Result<T, E>);

impl<T: EngineHandleFree, const N: usize> sealed::EngineHandleFree for [T; N] {}
impl<T: EngineHandleFree, const N: usize> EngineHandleFree for [T; N] {}

macro_rules! impl_tuple {
    ($($T:ident),*) => {
        impl<$($T: EngineHandleFree),*> sealed::EngineHandleFree for ($($T,)*) {}
        impl<$($T: EngineHandleFree),*> EngineHandleFree for ($($T,)*) {}
    };
}

variadics_please::all_tuples!(impl_tuple, 1, 15, T);

/// A Bevy component wrapper for structurally handle-free payload data.
///
/// The wrapper makes the sealed marker usable for application-owned payloads:
/// compose the payload from the marker's scalar, protocol, array, tuple and
/// collection implementations, then register `ReplicatedPayload<T>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Component)]
#[serde(transparent)]
pub struct ReplicatedPayload<T>(pub T);

impl<T: EngineHandleFree> sealed::EngineHandleFree for ReplicatedPayload<T> {}
impl<T: EngineHandleFree> EngineHandleFree for ReplicatedPayload<T> {}

/// Serialization functions accepted by the guarded custom-rule paths.
///
/// The inner replicon value stays private so it cannot be passed to an
/// unguarded registration path through this facade.
pub struct RuleFns<C: Component + EngineHandleFree>(raw_rule_fns::RuleFns<C>);

impl<C: Component + EngineHandleFree> RuleFns<C> {
    /// Creates guarded custom serialization functions for `C`.
    #[must_use]
    pub fn new(
        serialize: raw_rule_fns::SerializeFn<C>,
        deserialize: raw_rule_fns::DeserializeFn<C>,
    ) -> Self {
        Self(raw_rule_fns::RuleFns::new(serialize, deserialize))
    }

    /// Converts `C` through the handle-free wire representation `T`.
    #[must_use]
    pub fn new_as<T>() -> Self
    where
        T: EngineHandleFree + Serialize + DeserializeOwned,
        C: Clone + Into<T> + From<T>,
    {
        Self(raw_rule_fns::RuleFns::new_as::<T>())
    }

    /// Replaces the default in-place deserializer.
    #[must_use]
    pub fn with_in_place(mut self, deserialize: raw_rule_fns::DeserializeInPlaceFn<C>) -> Self {
        self.0 = self.0.with_in_place(deserialize);
        self
    }

    /// Replaces the default stale-update consumer.
    #[must_use]
    pub fn with_consume(mut self, consume: raw_rule_fns::ConsumeFn<C>) -> Self {
        self.0 = self.0.with_consume(consume);
        self
    }
}

impl<C> RuleFns<C>
where
    C: Component + EngineHandleFree + Diffable,
{
    /// Creates guarded diff-based serialization functions.
    #[must_use]
    pub fn new_diff() -> Self {
        Self(raw_rule_fns::RuleFns::new_diff())
    }
}

impl<C> Default for RuleFns<C>
where
    C: Component + EngineHandleFree + Serialize + DeserializeOwned,
{
    fn default() -> Self {
        Self(raw_rule_fns::RuleFns::default())
    }
}

/// A sealed guarded custom component rule.
#[doc(hidden)]
pub trait GuardedComponentRule: sealed::ComponentRule {
    /// The corresponding replicon rule, kept behind this sealed trait.
    #[doc(hidden)]
    type Raw: RawIntoComponentRule;

    /// Removes the facade wrappers immediately before forwarding.
    #[doc(hidden)]
    fn into_raw(self) -> Self::Raw;
}

impl<C> sealed::ComponentRule for RuleFns<C> where C: Component + EngineHandleFree {}
impl<C> GuardedComponentRule for RuleFns<C>
where
    C: Component<Mutability: MutWrite<C>> + EngineHandleFree,
{
    type Raw = raw_rule_fns::RuleFns<C>;

    fn into_raw(self) -> Self::Raw {
        self.0
    }
}

impl<C> sealed::ComponentRule for (RuleFns<C>, ReplicationMode) where C: Component + EngineHandleFree
{}
impl<C> GuardedComponentRule for (RuleFns<C>, ReplicationMode)
where
    C: Component<Mutability: MutWrite<C>> + EngineHandleFree,
{
    type Raw = (raw_rule_fns::RuleFns<C>, ReplicationMode);

    fn into_raw(self) -> Self::Raw {
        (self.0 .0, self.1)
    }
}

/// A sealed set of guarded custom component rules.
#[doc(hidden)]
pub trait GuardedComponentRules: sealed::ComponentRules {
    /// The corresponding replicon rule set, kept behind this sealed trait.
    #[doc(hidden)]
    type Raw: RawIntoComponentRules;

    /// Replicon's default priority for the rule set.
    #[doc(hidden)]
    const DEFAULT_PRIORITY: usize;

    /// Removes the facade wrappers immediately before forwarding.
    #[doc(hidden)]
    fn into_raw(self) -> Self::Raw;
}

impl<R: GuardedComponentRule> sealed::ComponentRules for R {}
impl<R: GuardedComponentRule> GuardedComponentRules for R {
    type Raw = R::Raw;
    const DEFAULT_PRIORITY: usize = 1;

    fn into_raw(self) -> Self::Raw {
        GuardedComponentRule::into_raw(self)
    }
}

macro_rules! impl_guarded_component_rules {
    ($(($n:tt, $R:ident)),*) => {
        impl<$($R: GuardedComponentRule),*> sealed::ComponentRules for ($($R,)*) {}

        impl<$($R: GuardedComponentRule),*> GuardedComponentRules for ($($R,)*) {
            type Raw = ($($R::Raw,)*);
            const DEFAULT_PRIORITY: usize = 0 $(+ { let _ = $n; 1 })*;

            fn into_raw(self) -> Self::Raw {
                ($(self.$n.into_raw(),)*)
            }
        }
    };
}

variadics_please::all_tuples_enumerated!(impl_guarded_component_rules, 1, 15, R);

/// A sealed custom rule for exactly one resource type.
#[doc(hidden)]
pub trait GuardedResourceRule<R>: sealed::ResourceRule
where
    R: Resource<Mutability: MutWrite<R>> + EngineHandleFree,
{
    /// The corresponding replicon resource rule.
    #[doc(hidden)]
    type Raw: RawIntoResourceRule<R>;

    /// Removes the facade wrapper immediately before forwarding.
    #[doc(hidden)]
    fn into_raw(self) -> Self::Raw;
}

impl<R> sealed::ResourceRule for RuleFns<R> where R: Resource + EngineHandleFree {}
impl<R> GuardedResourceRule<R> for RuleFns<R>
where
    R: Resource<Mutability: MutWrite<R>> + EngineHandleFree,
{
    type Raw = raw_rule_fns::RuleFns<R>;

    fn into_raw(self) -> Self::Raw {
        self.0
    }
}

impl<R> sealed::ResourceRule for (RuleFns<R>, ReplicationMode) where R: Resource + EngineHandleFree {}
impl<R> GuardedResourceRule<R> for (RuleFns<R>, ReplicationMode)
where
    R: Resource<Mutability: MutWrite<R>> + EngineHandleFree,
{
    type Raw = (raw_rule_fns::RuleFns<R>, ReplicationMode);

    fn into_raw(self) -> Self::Raw {
        (self.0 .0, self.1)
    }
}

/// A sealed tuple of handle-free components accepted by bundle registration.
#[doc(hidden)]
pub trait GuardedBundleRules: sealed::BundleRules + RawBundleRules {
    /// The tuple's default replication priority.
    #[doc(hidden)]
    const DEFAULT_PRIORITY: usize;
}

macro_rules! impl_guarded_bundle_rules {
    ($N:expr, $($C:ident),*) => {
        impl<$($C),*> sealed::BundleRules for ($($C,)*)
        where
            $($C: Component<Mutability: MutWrite<$C>> + EngineHandleFree + Serialize + DeserializeOwned),*
        {}

        impl<$($C),*> GuardedBundleRules for ($($C,)*)
        where
            $($C: Component<Mutability: MutWrite<$C>> + EngineHandleFree + Serialize + DeserializeOwned),*
        {
            const DEFAULT_PRIORITY: usize = $N;
        }
    };
}

variadics_please::all_tuples_with_size!(impl_guarded_bundle_rules, 1, 15, C);

/// Guarded replication registration methods for [`App`].
///
/// This mirrors replicon's component, resource, filtered, custom-rule and
/// tuple-bundle registration paths. Raw custom `BundleRules` implementations
/// are intentionally excluded because they receive the unguarded registry;
/// use guarded [`RuleFns`] tuples for custom multi-component rules instead.
pub trait OrreryRepliconAppExt {
    /// Registers a component with default serialization.
    fn replicate<C>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a component in once-only mode.
    fn replicate_once<C>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a diff-based component.
    fn replicate_diff<C>(&mut self) -> &mut Self
    where
        C: Diffable + EngineHandleFree;

    /// Registers a filtered diff-based component.
    fn replicate_diff_filtered<C, F>(&mut self) -> &mut Self
    where
        C: Diffable + EngineHandleFree,
        F: FilterRules;

    /// Registers a component through a handle-free wire representation.
    fn replicate_as<C, T>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a component through a handle-free wire representation in once-only mode.
    fn replicate_once_as<C, T>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a resource with default serialization.
    fn replicate_resource<R>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a diff-based resource.
    fn replicate_resource_diff<R>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Diffable;

    /// Registers a resource in once-only mode.
    fn replicate_resource_once<R>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a resource through a handle-free wire representation.
    fn replicate_resource_as<R, T>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers a resource through a handle-free wire representation in once-only mode.
    fn replicate_resource_once_as<R, T>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned;

    /// Registers guarded custom serialization functions for a resource.
    fn replicate_resource_with<R>(
        &mut self,
        resource_rule: impl GuardedResourceRule<R>,
    ) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree;

    /// Registers a filtered component with default serialization.
    fn replicate_filtered<C, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules;

    /// Registers a filtered component in once-only mode.
    fn replicate_once_filtered<C, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules;

    /// Registers a filtered component through a handle-free wire representation.
    fn replicate_filtered_as<C, T, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules;

    /// Registers a filtered component through a handle-free wire representation in once-only mode.
    fn replicate_once_filtered_as<C, T, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules;

    /// Registers one or more guarded custom component rules.
    fn replicate_with<R: GuardedComponentRules>(&mut self, rules: R) -> &mut Self;

    /// Registers guarded custom component rules with filters.
    fn replicate_with_filtered<R: GuardedComponentRules, F: FilterRules>(
        &mut self,
        rules: R,
    ) -> &mut Self;

    /// Registers guarded custom component rules with an explicit priority.
    fn replicate_with_priority<R: GuardedComponentRules>(
        &mut self,
        priority: usize,
        rules: R,
    ) -> &mut Self;

    /// Registers guarded custom component rules with an explicit priority and filters.
    fn replicate_with_priority_filtered<R: GuardedComponentRules, F: FilterRules>(
        &mut self,
        priority: usize,
        rules: R,
    ) -> &mut Self;

    /// Registers a tuple bundle of handle-free components.
    fn replicate_bundle<B: GuardedBundleRules>(&mut self) -> &mut Self;

    /// Registers a filtered tuple bundle of handle-free components.
    fn replicate_bundle_filtered<B: GuardedBundleRules, F: FilterRules>(&mut self) -> &mut Self;

    /// Registers a tuple bundle at an explicit priority.
    fn replicate_bundle_with<B: GuardedBundleRules>(&mut self, priority: usize) -> &mut Self;

    /// Registers a filtered tuple bundle at an explicit priority.
    fn replicate_bundle_with_filtered<B: GuardedBundleRules, F: FilterRules>(
        &mut self,
        priority: usize,
    ) -> &mut Self;
}

impl OrreryRepliconAppExt for App {
    fn replicate<C>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate::<C>(self)
    }

    fn replicate_once<C>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_once::<C>(self)
    }

    fn replicate_diff<C>(&mut self) -> &mut Self
    where
        C: Diffable + EngineHandleFree,
    {
        RawAppRuleExt::replicate_diff::<C>(self)
    }

    fn replicate_diff_filtered<C, F>(&mut self) -> &mut Self
    where
        C: Diffable + EngineHandleFree,
        F: FilterRules,
    {
        RawAppRuleExt::replicate_diff_filtered::<C, F>(self)
    }

    fn replicate_as<C, T>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_as::<C, T>(self)
    }

    fn replicate_once_as<C, T>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_once_as::<C, T>(self)
    }

    fn replicate_resource<R>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_resource::<R>(self)
    }

    fn replicate_resource_diff<R>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Diffable,
    {
        RawAppRuleExt::replicate_resource_diff::<R>(self)
    }

    fn replicate_resource_once<R>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_resource_once::<R>(self)
    }

    fn replicate_resource_as<R, T>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_resource_as::<R, T>(self)
    }

    fn replicate_resource_once_as<R, T>(&mut self) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
    {
        RawAppRuleExt::replicate_resource_once_as::<R, T>(self)
    }

    fn replicate_resource_with<R>(
        &mut self,
        resource_rule: impl GuardedResourceRule<R>,
    ) -> &mut Self
    where
        R: Resource<Mutability: MutWrite<R>> + EngineHandleFree,
    {
        RawAppRuleExt::replicate_resource_with::<R>(self, resource_rule.into_raw())
    }

    fn replicate_filtered<C, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules,
    {
        RawAppRuleExt::replicate_filtered::<C, F>(self)
    }

    fn replicate_once_filtered<C, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules,
    {
        RawAppRuleExt::replicate_once_filtered::<C, F>(self)
    }

    fn replicate_filtered_as<C, T, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules,
    {
        RawAppRuleExt::replicate_filtered_as::<C, T, F>(self)
    }

    fn replicate_once_filtered_as<C, T, F>(&mut self) -> &mut Self
    where
        C: Component<Mutability: MutWrite<C>> + EngineHandleFree + Clone + Into<T> + From<T>,
        T: EngineHandleFree + Serialize + DeserializeOwned,
        F: FilterRules,
    {
        RawAppRuleExt::replicate_once_filtered_as::<C, T, F>(self)
    }

    fn replicate_with<R: GuardedComponentRules>(&mut self, rules: R) -> &mut Self {
        RawAppRuleExt::replicate_with(self, rules.into_raw())
    }

    fn replicate_with_filtered<R: GuardedComponentRules, F: FilterRules>(
        &mut self,
        rules: R,
    ) -> &mut Self {
        RawAppRuleExt::replicate_with_filtered::<_, F>(self, rules.into_raw())
    }

    fn replicate_with_priority<R: GuardedComponentRules>(
        &mut self,
        priority: usize,
        rules: R,
    ) -> &mut Self {
        RawAppRuleExt::replicate_with_priority(self, priority, rules.into_raw())
    }

    fn replicate_with_priority_filtered<R: GuardedComponentRules, F: FilterRules>(
        &mut self,
        priority: usize,
        rules: R,
    ) -> &mut Self {
        RawAppRuleExt::replicate_with_priority_filtered::<_, F>(self, priority, rules.into_raw())
    }

    fn replicate_bundle<B: GuardedBundleRules>(&mut self) -> &mut Self {
        RawAppRuleExt::replicate_bundle::<B>(self)
    }

    fn replicate_bundle_filtered<B: GuardedBundleRules, F: FilterRules>(&mut self) -> &mut Self {
        RawAppRuleExt::replicate_bundle_filtered::<B, F>(self)
    }

    fn replicate_bundle_with<B: GuardedBundleRules>(&mut self, priority: usize) -> &mut Self {
        RawAppRuleExt::replicate_bundle_with::<B>(self, priority)
    }

    fn replicate_bundle_with_filtered<B: GuardedBundleRules, F: FilterRules>(
        &mut self,
        priority: usize,
    ) -> &mut Self {
        RawAppRuleExt::replicate_bundle_with_filtered::<B, F>(self, priority)
    }
}

/// Registers a component visibility scope without exposing replicon's raw
/// replication registry to the caller.
pub fn register_visibility_scope<S>(world: &mut World, lifetime: ScopeLifetime) -> FilterBit
where
    S: bevy_replicon::shared::replication::visibility::FilterScope,
{
    use bevy_ecs::change_detection::Mut;
    use bevy_replicon::{
        server::visibility::registry::FilterRegistry,
        shared::replication::registry::ReplicationRegistry,
    };

    world.resource_scope(|world, mut filter_registry: Mut<FilterRegistry>| {
        world.resource_scope(|world, mut registry: Mut<ReplicationRegistry>| {
            filter_registry.register_scope::<S>(world, &mut registry, lifetime)
        })
    })
}

/// The non-registration replicon surface Orrery's client crates use.
pub mod prelude {
    pub use super::{
        register_visibility_scope, EngineHandleFree, OrreryRepliconAppExt, ReplicatedPayload,
        RuleFns,
    };
    pub use bevy_replicon::{
        server::visibility::{client_visibility::ClientVisibility, filters_mask::FilterBit},
        shared::replication::{
            registry::ctx::{SerializeCtx, WriteCtx},
            rules::{component::ReplicationMode, filter::FilterRules},
            visibility::ScopeLifetime,
            Replicated,
        },
        RepliconPlugins,
    };
}

pub use bevy_replicon::{
    server::visibility::{client_visibility::ClientVisibility, filters_mask::FilterBit},
    shared::replication::{visibility::ScopeLifetime, Replicated},
    RepliconPlugins,
};

/// Uplink change-detection types exposed only when requested by the persistence client.
#[cfg(feature = "uplink")]
pub mod uplink {
    pub use bevy_replicon::{server::uplink::ComponentDiff, shared::replication::registry::FnsId};
}
