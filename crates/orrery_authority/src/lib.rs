//! Optimistic client-side authority state (D7).
//!
//! This crate intentionally owns the one canonical
//! [`LocallyAuthoritative`] marker. Persistence may only uplink entities with
//! that marker, which is inserted after a registrar grant and removed on every
//! loss path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::time::Duration;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_platform::time::Instant;
use orrery_protocol::{
    ClaimBasis, ClaimKind, DenyReason, LeaseId, LeaseMsg, NodeId, PersistId, SeqPair, Tick,
};

/// Registrar TTL from D7/D16, in milliseconds.
pub const LEASE_TTL_MS: u64 = 10_000;
/// Lease renewal cadence from D7/D16.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(2_500);

/// Replicated authority information for an entity.
#[derive(Debug, Clone, Copy, Component)]
pub struct Authority {
    /// Current holder according to the best authoritative information known.
    pub holder: Option<NodeId>,
    /// Highest observed sequence pair.
    pub seq: SeqPair,
}

/// The canonical marker permitting local simulation writes and persistence
/// uplinks. It is present only while the client has a current registrar grant.
#[derive(Debug, Clone, Copy, Component)]
pub struct LocallyAuthoritative;

/// Client-visible status of a persistent entity's authority claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Component)]
pub enum AuthorityPhase {
    /// Another peer (or no peer) is authoritative.
    Remote,
    /// Locally simulated optimistically, but not allowed to persist yet.
    LocalPending,
    /// Registrar granted a fencing token; persistence uplinks are permitted.
    LocalGranted {
        lease_id: LeaseId,
        expires_at_ms: u64,
    },
    /// The local conservative expiry floor passed; writes are stopped pending a reply.
    LocalUncertain { lease_id: LeaseId },
}

/// An authority-state transition visible to game code.
#[derive(Debug, Clone, Message, PartialEq, Eq)]
pub enum AuthorityEvent {
    /// Optimistic claim began.
    ClaimPending { entity: Entity },
    /// Registrar granted authority.
    Granted { entity: Entity, lease_id: LeaseId },
    /// Registrar denied the claim and local prediction was rolled back.
    Denied { entity: Entity, reason: DenyReason },
    /// A grant expired or was revoked.
    Lost { entity: Entity, lease_id: LeaseId },
}

/// Queued lease control messages for the gateway adapter.
#[derive(Debug, Default, Resource)]
pub struct LeaseOutbox(pub Vec<LeaseMsg>);

/// Lease replies delivered by the gateway adapter.
#[derive(Debug, Default, Resource)]
pub struct LeaseInbox(pub Vec<LeaseMsg>);

/// Local record used for heartbeats and reply-to-entity mapping.
#[derive(Debug, Clone)]
struct LocalLease {
    entity: Entity,
    lease_id: LeaseId,
    expires_at_ms: u64,
}

/// Client-side lease bookkeeping. Gateway transport adapters set `now_ms` from
/// registrar-relative time when available; tests and standalone clients may
/// advance it explicitly.
#[derive(Debug, Resource)]
pub struct AuthorityState {
    /// This peer's authenticated transport identity.
    pub node: NodeId,
    /// Registrar-relative monotonic milliseconds.
    pub now_ms: u64,
    leases: BTreeMap<PersistId, LocalLease>,
    last_heartbeat: Instant,
}

impl Default for AuthorityState {
    fn default() -> Self {
        Self {
            node: NodeId::from_bytes(&[0; 32]).expect("zero node id is valid"),
            now_ms: 0,
            leases: BTreeMap::new(),
            last_heartbeat: Instant::now(),
        }
    }
}

impl AuthorityState {
    /// Set the registrar-relative monotonic clock used for local expiry safety.
    pub fn set_now_ms(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }
}

/// Ergonomic command interface for systems that initiate claims.
#[derive(SystemParam)]
pub struct LeaseClient<'w, 's> {
    commands: Commands<'w, 's>,
    outbox: ResMut<'w, LeaseOutbox>,
}

impl<'w, 's> LeaseClient<'w, 's> {
    /// Begin an optimistic claim. The entity remains unable to uplink until a
    /// matching [`LeaseMsg::Grant`] is processed.
    pub fn claim(
        &mut self,
        entity: Entity,
        persist: PersistId,
        cell: orrery_protocol::CellId,
        kind: ClaimKind,
        basis: ClaimBasis,
        observed: SeqPair,
        tick: Tick,
    ) {
        self.commands
            .entity(entity)
            .insert(AuthorityPhase::LocalPending);
        self.outbox.0.push(LeaseMsg::Claim {
            entity: persist,
            cell,
            kind,
            basis,
            observed,
            tick,
        });
    }
}

/// Apply received registrar lease messages to ECS authority state.
pub fn process_lease_replies(
    mut commands: Commands,
    mut inbox: ResMut<LeaseInbox>,
    mut state: ResMut<AuthorityState>,
    entities: Query<(Entity, &crate::PersistIdentity), With<AuthorityPhase>>,
    mut events: MessageWriter<AuthorityEvent>,
) {
    for message in std::mem::take(&mut inbox.0) {
        match message {
            LeaseMsg::Grant {
                entity,
                lease_id,
                seq,
                ttl_ms,
                ..
            } => {
                if let Some((entity_ref, _)) = entities.iter().find(|(_, id)| id.0 == entity) {
                    let expiry = state.now_ms.saturating_add(u64::from(ttl_ms));
                    state.leases.insert(
                        entity,
                        LocalLease {
                            entity: entity_ref,
                            lease_id,
                            expires_at_ms: expiry,
                        },
                    );
                    commands.entity(entity_ref).insert((
                        Authority {
                            holder: Some(state.node),
                            seq,
                        },
                        AuthorityPhase::LocalGranted {
                            lease_id,
                            expires_at_ms: expiry,
                        },
                        LocallyAuthoritative,
                    ));
                    events.write(AuthorityEvent::Granted {
                        entity: entity_ref,
                        lease_id,
                    });
                }
            }
            LeaseMsg::Deny { entity, reason, .. } => {
                if let Some((entity_ref, _)) = entities.iter().find(|(_, id)| id.0 == entity) {
                    state.leases.remove(&entity);
                    commands
                        .entity(entity_ref)
                        .remove::<LocallyAuthoritative>()
                        .insert(AuthorityPhase::Remote);
                    events.write(AuthorityEvent::Denied {
                        entity: entity_ref,
                        reason,
                    });
                }
            }
            LeaseMsg::Expire {
                entity, lease_id, ..
            } => {
                if let Some(lease) = state.leases.remove(&entity) {
                    commands
                        .entity(lease.entity)
                        .remove::<LocallyAuthoritative>()
                        .insert(AuthorityPhase::Remote);
                    events.write(AuthorityEvent::Lost {
                        entity: lease.entity,
                        lease_id,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Emit one compact batched heartbeat and stop uplinks before local expiry.
pub fn maintain_leases(
    mut commands: Commands,
    mut state: ResMut<AuthorityState>,
    mut outbox: ResMut<LeaseOutbox>,
    phases: Query<(Entity, &PersistIdentity, &AuthorityPhase)>,
    mut events: MessageWriter<AuthorityEvent>,
) {
    let uncertain_before = state
        .now_ms
        .saturating_add(HEARTBEAT_INTERVAL.as_millis() as u64);
    let expired: Vec<_> = state
        .leases
        .iter()
        .filter_map(|(id, lease)| {
            (lease.expires_at_ms <= uncertain_before).then_some((*id, lease.clone()))
        })
        .collect();
    for (id, lease) in expired {
        state.leases.remove(&id);
        commands
            .entity(lease.entity)
            .remove::<LocallyAuthoritative>()
            .insert(AuthorityPhase::LocalUncertain {
                lease_id: lease.lease_id,
            });
        events.write(AuthorityEvent::Lost {
            entity: lease.entity,
            lease_id: lease.lease_id,
        });
    }
    if state.last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL && !state.leases.is_empty() {
        outbox.0.push(LeaseMsg::Heartbeat {
            lease_ids: state.leases.values().map(|lease| lease.lease_id).collect(),
            tick: Tick::new(state.now_ms),
        });
        state.last_heartbeat = Instant::now();
    }
    let _ = phases; // keeps the system extensible without reading world state on the hot path.
}

/// Persistent identity component owned by authority. `orrery_persist_client`
/// retains its legacy wrapper and maps it during migration.
#[derive(Debug, Clone, Copy, Component)]
pub struct PersistIdentity(pub PersistId);

/// Bevy plugin providing client authority state and lease maintenance.
#[derive(Default)]
pub struct OrreryAuthorityPlugin;

impl Plugin for OrreryAuthorityPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AuthorityState>()
            .init_resource::<LeaseInbox>()
            .init_resource::<LeaseOutbox>()
            .add_message::<AuthorityEvent>()
            .add_systems(Update, (process_lease_replies, maintain_leases).chain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::App;
    use orrery_protocol::CellId;

    #[test]
    fn grant_enables_and_deny_removes_the_only_uplink_marker() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let e = app
            .world_mut()
            .spawn((
                PersistIdentity(PersistId::new(4)),
                AuthorityPhase::LocalPending,
            ))
            .id();
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                entity: PersistId::new(4),
                lease_id: LeaseId(2),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_some());
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Deny {
                entity: PersistId::new(4),
                reason: DenyReason::NotEligible,
                retry_after_ms: 0,
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_none());
        let _ = CellId::ROOT;
    }
}
