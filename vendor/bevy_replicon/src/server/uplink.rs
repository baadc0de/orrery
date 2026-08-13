//! Surfaces replicon's change-detection stream for downstream consumers (orrery
//! D11: the persistence uplink).
//!
//! Replicon already computes which replicated components changed this tick and
//! serializes them in [`collect_changes`](super::collect_changes), but keeps the
//! per-component serialized payloads private (they're packed into replication
//! messages keyed by per-client visibility). This module re-walks the same
//! replicated archetypes and emits one [`ComponentDiff`] message per changed
//! component, regardless of visibility — the persistence tier needs every
//! change from the entity's owner, not just what a specific client can see.
//!
//! The emitted [`ComponentDiff`] is engine-agnostic by construction: it carries
//! the Bevy [`Entity`], the replicon [`FnsId`] (identifies the replicated
//! component rule), and the postcard-encoded payload. Downstream crates map the
//! `Entity` to a stable `PersistId` and wrap the payload in their own wire type.
//!
//! # Scheduling
//!
//! Register [`collect_uplink_diffs`] in `PostUpdate` inside
//! [`ServerSystems::Send`] so it runs in the same tick window as replicon's own
//! change collection. The `uplink` feature (off by default) gates this module.
//!
//! # Upstreaming status
//!
//! This module is an orrery prototype, not yet upstreamed. It is intentionally
//! orrery-shaped (owner-side, visibility-agnostic, a single `ComponentDiff` sink)
//! and is expected to need generalization before it would be accepted into
//! `bevy_replicon` — see `.agents/memory/replicon-uplink-pr.md`. Keep the
//! consumer decoupled from `ComponentDiff` until the surfaced API stabilizes.

use bevy::{
    ecs::{archetype::Archetypes, system::SystemChangeTick},
    prelude::*,
};
use log::debug;

use super::server_tick::ServerTick;
use crate::{
    server::{
        replicated_archetypes::ReplicatedArchetypes,
        replication_messages::serialized_data::{ErasedComponent, SerializedData},
        replication_query::ReplicationQuery,
    },
    shared::replication::{
        registry::{FnsId, ReplicationRegistry, ctx::SerializeCtx},
        rules::{ReplicationRules, component::ReplicationMode},
        storage::ReplicationStorage,
    },
};

/// A single changed replicated component in this tick.
///
/// The persistence uplink's input: `entity` identifies the changed entity,
/// `fns_id` identifies the replicated component rule (what changed), and
/// `payload` is the postcard-encoded component value (or recorded diff).
///
/// Intentionally decoupled from client visibility — the owning peer wants to
/// persist every change it authored, not just what a given client can see.
#[derive(Message)]
pub struct ComponentDiff {
    /// The changed entity.
    pub entity: Entity,
    /// The replicon rule id identifying the component.
    pub fns_id: FnsId,
    /// The postcard-encoded component value / diff.
    pub payload: bytes::Bytes,
}

/// Collects this tick's replicated-component changes into [`ComponentDiff`]
/// messages.
///
/// Mirrors [`collect_changes`](super::collect_changes) but writes every changed
/// component (owner-side) instead of gating on per-client visibility. Runs inside
/// [`ServerSystems::Send`]; guarded by the `uplink` feature.
//
// `pub(super)`: only `server.rs` (the parent) wires this into the schedule; it
// takes crate-private types like [`ReplicationQuery`] and `ReplicatedArchetypes`.
pub(super) fn collect_uplink_diffs(
    archetypes: &Archetypes,
    query: ReplicationQuery,
    change_tick: SystemChangeTick,
    server_tick: Res<ServerTick>,
    registry: Res<ReplicationRegistry>,
    type_registry: Res<AppTypeRegistry>,
    rules: Res<ReplicationRules>,
    mut replication_storage: ResMut<ReplicationStorage>,
    mut replicated_archetypes: ResMut<ReplicatedArchetypes>,
    mut serialized: ResMut<SerializedData>,
    mut writer: MessageWriter<ComponentDiff>,
) {
    // TODO(upstream): this re-walks `ReplicatedArchetypes`/`ReplicationQuery`
    // in parallel with `collect_changes`, duplicating the change-detection +
    // serialization walk and risking drift if replicon's internals change. Before
    // this can be upstreamed it should be generalized — ideally folding the tap
    // into `collect_changes` via a pluggable sink/observer rather than a second
    // archetype walk (see `.agents/memory/replicon-uplink-pr.md`).
    replicated_archetypes.update(archetypes, &rules);

    let last_run = change_tick.last_run();
    let this_run = change_tick.this_run();

    for replicated_archetype in replicated_archetypes.iter() {
        // SAFETY: all IDs from replicated archetypes obtained from real archetypes.
        let archetype = unsafe { archetypes.get(replicated_archetype.id).unwrap_unchecked() };

        for archetype_entity in archetype.entities() {
            let entity = archetype_entity.id();

            for &(rule, storage) in &replicated_archetype.components {
                // `Once` rules only init a client; they're not persisted state.
                if rule.mode == ReplicationMode::Once {
                    continue;
                }

                let (_component_index, component_id, fns) = registry.get(rule.fns_id);

                // SAFETY: component and storage were obtained from this archetype.
                let (ptr, ticks) = unsafe {
                    query.get_component_unchecked(
                        archetype_entity,
                        archetype.table_id(),
                        storage,
                        component_id,
                    )
                };

                if !ticks.is_changed(last_run, this_run) {
                    continue;
                }

                // SAFETY: `fns` and `ptr` were created for the same component type.
                let mut component = unsafe { ErasedComponent::new(fns, ptr, rule.fns_id) };
                let mut ctx = SerializeCtx {
                    entity,
                    component_id,
                    last_changed: ticks.changed,
                    server_tick: **server_tick,
                    diff_cursor: None,
                    type_registry: &type_registry,
                    storage: &mut replication_storage,
                };

                let mut range = None;
                let Ok(range) =
                    serialized.write_cached_component(&mut ctx, &mut range, &mut component)
                else {
                    debug!(
                        "failed to serialize changed component `{:?}` for `{entity}`",
                        rule.fns_id
                    );
                    continue;
                };

                let payload = bytes::Bytes::copy_from_slice(&serialized[range]);
                writer.write(ComponentDiff {
                    entity,
                    fns_id: rule.fns_id,
                    payload,
                });
            }
        }
    }
}
