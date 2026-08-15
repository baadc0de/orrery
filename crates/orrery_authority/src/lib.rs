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
    ClaimBasis, ClaimId, ClaimKind, DenyReason, ExpireDisposition, GridId, LeaseId, LeaseMsg,
    NodeId, PersistId, SeqPair, Tick,
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
    LocalPending {
        /// Client-session identifier of the claim awaiting a reply.
        claim_id: ClaimId,
    },
    /// Registrar granted a fencing token; persistence uplinks are permitted.
    LocalGranted {
        /// Registrar-issued fencing token that authorizes local uplinks.
        lease_id: LeaseId,
        /// Client-monotonic deadline after which local writes must stop.
        expires_at_ms: u64,
    },
    /// The local conservative expiry floor passed; writes are stopped pending a reply.
    LocalUncertain {
        /// The last fencing token held before local authority became uncertain.
        lease_id: LeaseId,
    },
}

/// An authority-state transition visible to game code.
#[derive(Debug, Clone, Message, PartialEq, Eq)]
pub enum AuthorityEvent {
    /// Optimistic claim began.
    ClaimPending {
        /// ECS entity whose optimistic claim began.
        entity: Entity,
    },
    /// Registrar granted authority in reply to this peer's own claim.
    Granted {
        /// ECS entity that received authority.
        entity: Entity,
        /// Registrar-issued fencing token for the grant.
        lease_id: LeaseId,
    },
    /// The registrar handed this peer authority it never asked for: it was
    /// selected as successor to a lost holder, or received a negotiated
    /// handoff (D7 §5).
    ///
    /// Distinct from [`AuthorityEvent::Granted`] because the entity was not
    /// being simulated optimistically beforehand — game code typically has to
    /// promote it from an interpolated proxy to a simulated body rather than
    /// confirm a prediction it was already running.
    Inherited {
        /// ECS entity that received authority.
        entity: Entity,
        /// Registrar-issued fencing token for the grant.
        lease_id: LeaseId,
        /// The holder this peer succeeded, when the registrar named one.
        from: Option<NodeId>,
    },
    /// Registrar denied the claim and local prediction was rolled back.
    Denied {
        /// ECS entity whose optimistic claim was rolled back.
        entity: Entity,
        /// Registrar reason for refusing the claim.
        reason: DenyReason,
    },
    /// The registrar is asking this peer to give an entity up, because
    /// another peer explicitly claimed it (D7 §4.2).
    ///
    /// Nothing happens automatically: game code decides whether to consent,
    /// by calling [`LeaseClient::divest`] with the named successor. Ignoring
    /// the request is a legitimate answer — past the registrar's deadline,
    /// *weak* authority is taken anyway, so an interaction never stalls on an
    /// unresponsive peer, while *strong* ownership is kept, because stealing
    /// by timeout is what "not stealable" forbids. The "ask to trade" UX lives
    /// above this protocol, which is why the decision is surfaced rather than
    /// taken here.
    DivestRequested {
        /// ECS entity the registrar is asking about.
        entity: Entity,
        /// The fencing token this peer currently holds for it.
        lease_id: LeaseId,
        /// The peer that claimed it, and the successor to name when consenting.
        to: Option<NodeId>,
    },
    /// A grant expired or was revoked.
    Lost {
        /// ECS entity that lost local authority.
        entity: Entity,
        /// Fencing token that is no longer valid.
        lease_id: LeaseId,
    },
}

/// Queued lease control messages for the gateway adapter.
#[derive(Debug, Default, Resource)]
pub struct LeaseOutbox(pub Vec<LeaseMsg>);

/// This peer's coordinator interest grant, and what the gateway made of it.
///
/// Interest is what authorizes weak claims and makes this peer eligible to
/// inherit a lease, and only the coordinator can assert it. The peer is
/// merely the courier: it holds opaque signed bytes, presents them, and reads
/// back whether they were accepted.
///
/// Put the coordinator's handout in `grant` and the gateway adapter presents
/// it on the next connected frame, re-presenting it after a reconnect — a new
/// session starts with no interest on file.
#[derive(Debug, Default, Resource)]
pub struct InterestGrant {
    /// The coordinator-signed grant to present, if one has been received.
    pub grant: Option<Vec<u8>>,
    /// The coordinator epoch the gateway currently holds for this peer.
    ///
    /// `None` before the first acceptance, and after a refusal — a peer with
    /// no epoch on file should expect its weak claims to be refused.
    pub accepted_epoch: Option<orrery_protocol::Epoch>,
    /// The reason code from the most recent reply, `INTEREST_ACK_OK` when the
    /// grant was accepted.
    pub last_reason: u8,
    /// Whether `grant` still needs presenting on the current session.
    pub pending: bool,
}

impl InterestGrant {
    /// Install a freshly received coordinator grant, to be presented next.
    pub fn set(&mut self, grant: Vec<u8>) {
        self.grant = Some(grant);
        self.pending = true;
    }

    /// Mark the held grant as needing presentation again, after a reconnect.
    pub fn resend(&mut self) {
        self.accepted_epoch = None;
        self.pending = self.grant.is_some();
    }

    /// Whether the gateway currently believes this peer's interest.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        self.accepted_epoch.is_some()
    }
}

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

/// Client-side lease bookkeeping.
///
/// `now_ms` is a client-process monotonic safety clock. Registrar timestamps
/// are never used as a client clock because the two processes have unrelated
/// monotonic origins; an acknowledged grant or heartbeat instead establishes a
/// fresh local deadline from the registrar-issued TTL.
#[derive(Debug, Resource)]
pub struct AuthorityState {
    /// This peer's authenticated transport identity.
    pub node: NodeId,
    /// Client-process monotonic milliseconds used only for local expiry safety.
    pub now_ms: u64,
    leases: BTreeMap<PersistId, LocalLease>,
    pending_claims: BTreeMap<PersistId, ClaimId>,
    next_claim_id: ClaimId,
    last_heartbeat: Instant,
}

impl Default for AuthorityState {
    fn default() -> Self {
        Self {
            node: NodeId::from_bytes(&[0; 32]).expect("zero node id is valid"),
            now_ms: 0,
            leases: BTreeMap::new(),
            pending_claims: BTreeMap::new(),
            next_claim_id: ClaimId(1),
            last_heartbeat: Instant::now(),
        }
    }
}

impl AuthorityState {
    /// Set the local monotonic clock used for expiry safety.
    pub fn set_now_ms(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    /// Remove the local fencing record for an entity and return its token.
    pub fn revoke_local_lease(&mut self, entity: PersistId) -> Option<LeaseId> {
        self.leases.remove(&entity).map(|lease| lease.lease_id)
    }

    /// The fencing token currently installed for `entity`, if any.
    #[must_use]
    pub fn local_lease_id(&self, entity: PersistId) -> Option<LeaseId> {
        self.leases.get(&entity).map(|lease| lease.lease_id)
    }

    /// Allocate and record a claim correlation identifier for `entity`.
    pub fn begin_claim(&mut self, entity: PersistId) -> Option<ClaimId> {
        let claim_id = self.next_claim_id;
        self.next_claim_id = ClaimId(claim_id.0.checked_add(1)?);
        self.pending_claims.insert(entity, claim_id);
        Some(claim_id)
    }
}

/// Advance the client-process monotonic safety clock once per update.
pub fn advance_lease_clock(mut state: ResMut<AuthorityState>) {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    state.now_ms = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
}

/// Ergonomic command interface for systems that initiate claims.
#[derive(SystemParam)]
pub struct LeaseClient<'w, 's> {
    commands: Commands<'w, 's>,
    outbox: ResMut<'w, LeaseOutbox>,
    state: ResMut<'w, AuthorityState>,
}

/// Complete gameplay request for an optimistic persistent-entity lease claim.
///
/// This keeps the ECS entity context and registrar protocol fields together so
/// callers cannot lose part of a claim while forwarding it through systems.
#[derive(Debug, Clone, Copy)]
pub struct LeaseClaim {
    /// ECS entity to mark as locally pending before registrar confirmation.
    pub entity: Entity,
    /// Stable persistent identity whose lease is requested.
    pub persist: PersistId,
    /// Nested grid containing the persistent entity.
    pub grid: GridId,
    /// Cell containing the persistent entity in `grid`.
    pub cell: orrery_protocol::CellId,
    /// Requested weak authority or strong ownership tier.
    pub kind: ClaimKind,
    /// Evidence supporting the requested tier.
    pub basis: ClaimBasis,
    /// Most recent authority and ownership sequences observed by the claimant.
    pub observed: SeqPair,
    /// Universe tick at which the claim is submitted.
    pub tick: Tick,
}

impl<'w, 's> LeaseClient<'w, 's> {
    /// Begin an optimistic claim. The entity remains unable to uplink until a
    /// matching [`LeaseMsg::Grant`] is processed.
    #[must_use]
    pub fn claim(&mut self, request: LeaseClaim) -> Option<ClaimId> {
        let claim_id = self.state.begin_claim(request.persist)?;
        self.commands
            .entity(request.entity)
            .insert(AuthorityPhase::LocalPending { claim_id });
        self.outbox.0.push(LeaseMsg::Claim {
            claim_id,
            entity: request.persist,
            grid: request.grid,
            cell: request.cell,
            kind: request.kind,
            basis: request.basis,
            observed: request.observed,
            tick: request.tick,
        });
        Some(claim_id)
    }

    /// Consent to give up a lease: hand it to `to`, or release it when `to`
    /// is `None` (D7 §5).
    ///
    /// Local authority is dropped **before** the message goes out, not when
    /// the registrar replies. A holder that has offered a lease away must stop
    /// writing immediately; if the registrar refuses the divestiture the
    /// entity is simply unowned until this peer claims it again, which is the
    /// safe direction to be wrong in.
    ///
    /// Returns `false` when this peer holds no fence for the entity.
    pub fn divest(&mut self, request: LeaseDivest) -> bool {
        let Some(lease_id) = self.state.revoke_local_lease(request.persist) else {
            return false;
        };
        self.commands
            .entity(request.entity)
            .remove::<LocallyAuthoritative>()
            .insert(AuthorityPhase::Remote);
        self.outbox.0.push(LeaseMsg::Divest {
            entity: request.persist,
            lease_id,
            to: request.to,
            final_seq: request.final_seq,
            cursor: request.cursor,
        });
        true
    }
}

/// Complete gameplay request for a cooperative handoff or release.
#[derive(Debug, Clone, Copy)]
pub struct LeaseDivest {
    /// ECS entity giving up local authority.
    pub entity: Entity,
    /// Stable persistent identity whose lease is being divested.
    pub persist: PersistId,
    /// Successor to offer the lease to, or `None` to release and park.
    pub to: Option<NodeId>,
    /// The holder's final authoritative sequence pair.
    pub final_seq: SeqPair,
    /// The last journal position this peer saw acknowledged for the entity.
    /// The registrar requires one before handing state to a named successor.
    pub cursor: Option<orrery_protocol::Lsn>,
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
                claim_id,
                entity,
                lease_id,
                seq,
                ttl_ms,
                prev_holder,
            } => {
                // A grant is legitimate either as the reply to this peer's
                // own pending claim, or as a registrar-initiated placement
                // carrying the reserved correlation — successor selection or
                // the receiving half of a handoff (D7 §5). Both still have to
                // advance the fence this peer already has installed, so a
                // delayed duplicate can never reinstate stale authority.
                let inherited = claim_id == ClaimId::REGISTRAR;
                let is_current_claim = state.pending_claims.get(&entity) == Some(&claim_id);
                let advances_fence = state
                    .leases
                    .get(&entity)
                    .is_none_or(|current| lease_id > current.lease_id);
                if !(inherited || is_current_claim) || !advances_fence {
                    continue;
                }
                if let Some((entity_ref, _)) = entities.iter().find(|(_, id)| id.0 == entity) {
                    state.pending_claims.remove(&entity);
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
                    events.write(if inherited {
                        AuthorityEvent::Inherited {
                            entity: entity_ref,
                            lease_id,
                            from: prev_holder,
                        }
                    } else {
                        AuthorityEvent::Granted {
                            entity: entity_ref,
                            lease_id,
                        }
                    });
                }
            }
            LeaseMsg::Deny {
                claim_id,
                entity,
                reason,
                ..
            } => {
                if claim_id.is_none() || state.pending_claims.get(&entity) != claim_id.as_ref() {
                    continue;
                }
                if let Some((entity_ref, _)) = entities.iter().find(|(_, id)| id.0 == entity) {
                    state.pending_claims.remove(&entity);
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
                entity,
                lease_id,
                disposition,
                ..
            } => {
                if state
                    .leases
                    .get(&entity)
                    .is_none_or(|lease| lease.lease_id != lease_id)
                {
                    continue;
                }
                if let Some(lease) = state.leases.remove(&entity) {
                    let mut entity_commands = lease_commands(&mut commands, lease.entity);
                    // The disposition names where authority actually went, so
                    // the loser can render the entity against its real holder
                    // instead of a stale one it no longer is.
                    if let ExpireDisposition::Reassigned { to } = disposition {
                        entity_commands.insert(Authority {
                            holder: Some(to),
                            seq: SeqPair::default(),
                        });
                    } else {
                        entity_commands.insert(Authority {
                            holder: None,
                            seq: SeqPair::default(),
                        });
                    }
                    events.write(AuthorityEvent::Lost {
                        entity: lease.entity,
                        lease_id,
                    });
                }
            }
            LeaseMsg::Divest {
                entity,
                lease_id,
                to,
                ..
            } => {
                // Only a request naming the fence this peer actually holds is
                // surfaced; a late one for a superseded token is noise.
                let Some(local) = state.leases.get(&entity) else {
                    continue;
                };
                if local.lease_id != lease_id {
                    continue;
                }
                events.write(AuthorityEvent::DivestRequested {
                    entity: local.entity,
                    lease_id,
                    to,
                });
            }
            LeaseMsg::HeartbeatAck { leases, invalid } => {
                // An explicit failed heartbeat wins over any row carried for
                // status. This drops the local fence immediately instead of
                // waiting for the conservative expiry floor.
                let invalidated: Vec<_> = state
                    .leases
                    .iter()
                    .filter_map(|(persist, local)| {
                        invalid
                            .contains(&local.lease_id)
                            .then_some((*persist, local.clone()))
                    })
                    .collect();
                for (persist, local) in invalidated {
                    state.leases.remove(&persist);
                    commands
                        .entity(local.entity)
                        .remove::<LocallyAuthoritative>()
                        .insert(AuthorityPhase::Remote);
                    events.write(AuthorityEvent::Lost {
                        entity: local.entity,
                        lease_id: local.lease_id,
                    });
                }
                let local_node = state.node;
                let refreshed_expiry = state.now_ms.saturating_add(LEASE_TTL_MS);
                for row in leases {
                    let Some(local) = state.leases.get_mut(&row.entity) else {
                        continue;
                    };
                    if local.lease_id != row.lease_id || row.holder != Some(local_node) {
                        continue;
                    }
                    // `row.expires_at` is registrar-process monotonic time,
                    // not comparable with this client process. A successful
                    // renewal response therefore refreshes the local safety
                    // deadline from the fixed registrar TTL.
                    local.expires_at_ms = refreshed_expiry;
                    commands.entity(local.entity).insert((
                        Authority {
                            holder: row.holder,
                            seq: row.seq,
                        },
                        AuthorityPhase::LocalGranted {
                            lease_id: row.lease_id,
                            expires_at_ms: refreshed_expiry,
                        },
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Drop the local write marker and return the entity's commands for the
/// caller to stamp the new authority state onto.
fn lease_commands<'a>(
    commands: &'a mut Commands<'_, '_>,
    entity: Entity,
) -> bevy_ecs::system::EntityCommands<'a> {
    let mut entity_commands = commands.entity(entity);
    entity_commands
        .remove::<LocallyAuthoritative>()
        .insert(AuthorityPhase::Remote);
    entity_commands
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
            .init_resource::<InterestGrant>()
            .init_resource::<LeaseInbox>()
            .init_resource::<LeaseOutbox>()
            .add_message::<AuthorityEvent>()
            .add_systems(
                Update,
                (advance_lease_clock, process_lease_replies, maintain_leases).chain(),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::App;
    use bevy_ecs::system::RunSystemOnce;
    use orrery_protocol::{CellId, ExpireDisposition, ExpireReason, Lease, LeaseFlags};

    fn begin_test_claim(app: &mut App, entity: Entity, persisted: PersistId) -> ClaimId {
        let claim_id = app
            .world_mut()
            .resource_mut::<AuthorityState>()
            .begin_claim(persisted)
            .expect("test claim id space is available");
        app.world_mut()
            .entity_mut(entity)
            .insert(AuthorityPhase::LocalPending { claim_id });
        claim_id
    }

    fn node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh_base::SecretKey::from_bytes(&bytes).public()
    }

    #[test]
    fn a_registrar_initiated_grant_installs_authority_without_a_pending_claim() {
        // Given: an entity this peer replicates but never claimed.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let inherited = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(60)), AuthorityPhase::Remote))
            .id();

        // When: the registrar hands it over — successor selection after
        // another holder was lost.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: ClaimId::REGISTRAR,
                entity: PersistId::new(60),
                lease_id: LeaseId(4),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 3,
                },
                ttl_ms: 10_000,
                prev_holder: Some(node_id(7)),
            });
        app.update();

        // Then: local authority is installed, and the event says it was
        // inherited rather than confirming a prediction this peer was running.
        assert!(app.world().get::<LocallyAuthoritative>(inherited).is_some());
        assert_eq!(
            app.world().get::<AuthorityPhase>(inherited),
            Some(&AuthorityPhase::LocalGranted {
                lease_id: LeaseId(4),
                expires_at_ms: 10_000,
            })
        );
        assert!(app
            .world()
            .resource::<Messages<AuthorityEvent>>()
            .iter_current_update_messages()
            .any(|event| *event
                == AuthorityEvent::Inherited {
                    entity: inherited,
                    lease_id: LeaseId(4),
                    from: Some(node_id(7)),
                }));
    }

    #[test]
    fn a_registrar_grant_that_does_not_advance_the_fence_is_ignored() {
        // Given: a peer already holding a newer fence than a delayed grant
        // names — a duplicate push, or one reordered behind a later transfer.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let held = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(61)), AuthorityPhase::Remote))
            .id();
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: ClaimId::REGISTRAR,
                entity: PersistId::new(61),
                lease_id: LeaseId(9),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();

        // When: an older registrar-initiated grant arrives late.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: ClaimId::REGISTRAR,
                entity: PersistId::new(61),
                lease_id: LeaseId(8),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();

        // Then: the installed fence is untouched — a stale push can never
        // reinstate superseded authority.
        assert_eq!(
            app.world()
                .resource::<AuthorityState>()
                .local_lease_id(PersistId::new(61)),
            Some(LeaseId(9))
        );
        assert!(matches!(
            app.world().get::<AuthorityPhase>(held),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(9),
                ..
            })
        ));
    }

    #[test]
    fn divesting_stops_local_writes_before_the_message_leaves() {
        // Given: a granted lease this peer is writing under.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let held = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(62)), AuthorityPhase::Remote))
            .id();
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: ClaimId::REGISTRAR,
                entity: PersistId::new(62),
                lease_id: LeaseId(3),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(held).is_some());

        // When: gameplay hands the entity to another peer.
        let handed = app
            .world_mut()
            .run_system_once(move |mut client: LeaseClient| {
                client.divest(LeaseDivest {
                    entity: held,
                    persist: PersistId::new(62),
                    to: Some(node_id(5)),
                    final_seq: SeqPair::default(),
                    cursor: Some(orrery_protocol::Lsn::new(1, 64)),
                })
            });
        assert!(handed.expect("divest system runs"));
        app.update();

        // Then: the write marker is gone immediately — before any registrar
        // reply — and the offer is queued for the gateway adapter.
        assert!(app.world().get::<LocallyAuthoritative>(held).is_none());
        assert_eq!(
            app.world()
                .resource::<AuthorityState>()
                .local_lease_id(PersistId::new(62)),
            None
        );
        assert!(app
            .world()
            .resource::<LeaseOutbox>()
            .0
            .iter()
            .any(|message| *message
                == LeaseMsg::Divest {
                    entity: PersistId::new(62),
                    lease_id: LeaseId(3),
                    to: Some(node_id(5)),
                    final_seq: SeqPair::default(),
                    cursor: Some(orrery_protocol::Lsn::new(1, 64)),
                }));
    }

    #[test]
    fn a_reassigning_expiry_points_the_loser_at_the_new_holder() {
        // Given: a peer holding a lease the registrar is about to move.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let lost = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(63)), AuthorityPhase::Remote))
            .id();
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: ClaimId::REGISTRAR,
                entity: PersistId::new(63),
                lease_id: LeaseId(2),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();

        // When: the registrar reassigns it to a named successor.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Expire {
                entity: PersistId::new(63),
                lease_id: LeaseId(2),
                last_holder: None,
                reason: ExpireReason::Timeout,
                disposition: ExpireDisposition::Reassigned { to: node_id(6) },
            });
        app.update();

        // Then: local authority is dropped and the entity now renders against
        // its real holder, not the stale local one.
        assert!(app.world().get::<LocallyAuthoritative>(lost).is_none());
        assert_eq!(
            app.world().get::<AuthorityPhase>(lost),
            Some(&AuthorityPhase::Remote)
        );
        assert_eq!(
            app.world().get::<Authority>(lost).map(|a| a.holder),
            Some(Some(node_id(6)))
        );
    }

    #[test]
    fn current_grant_deny_and_expire_replies_apply_the_documented_transitions() {
        // Given: two optimistic claims awaiting their registrar replies.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let denied = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(40)), AuthorityPhase::Remote))
            .id();
        let expired = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(41)), AuthorityPhase::Remote))
            .id();
        let denied_claim = begin_test_claim(&mut app, denied, PersistId::new(40));
        let expired_claim = begin_test_claim(&mut app, expired, PersistId::new(41));
        app.world_mut().resource_mut::<LeaseInbox>().0.extend([
            LeaseMsg::Deny {
                claim_id: Some(denied_claim),
                entity: PersistId::new(40),
                reason: DenyReason::NotEligible,
                retry_after_ms: 0,
            },
            LeaseMsg::Grant {
                claim_id: expired_claim,
                entity: PersistId::new(41),
                lease_id: LeaseId(9),
                seq: SeqPair {
                    own_seq: 1,
                    auth_seq: 2,
                },
                ttl_ms: 10_000,
                prev_holder: None,
            },
        ]);
        app.update();
        assert_eq!(
            app.world().get::<AuthorityPhase>(expired),
            Some(&AuthorityPhase::LocalGranted {
                lease_id: LeaseId(9),
                expires_at_ms: 10_000,
            })
        );
        assert!(app.world().get::<LocallyAuthoritative>(expired).is_some());

        // When: the registrar expires the currently installed fence.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Expire {
                entity: PersistId::new(41),
                lease_id: LeaseId(9),
                last_holder: None,
                reason: ExpireReason::Revoked,
                disposition: ExpireDisposition::Free,
            });
        app.update();

        // Then: both legitimate current replies revoke local authority.
        assert_eq!(
            app.world().get::<AuthorityPhase>(denied),
            Some(&AuthorityPhase::Remote)
        );
        assert_eq!(
            app.world().get::<AuthorityPhase>(expired),
            Some(&AuthorityPhase::Remote)
        );
        assert!(app.world().get::<LocallyAuthoritative>(denied).is_none());
        assert!(app.world().get::<LocallyAuthoritative>(expired).is_none());
    }

    #[test]
    fn delayed_control_replies_do_not_replace_or_revoke_a_newer_grant() {
        // Given: the Bevy scheduler installed a second grant after an earlier grant.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(42);
        let entity = app
            .world_mut()
            .spawn((PersistIdentity(persisted), AuthorityPhase::Remote))
            .id();
        let claim1 = begin_test_claim(&mut app, entity, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: claim1,
                entity: persisted,
                lease_id: LeaseId(1),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 1,
                },
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        let claim2 = begin_test_claim(&mut app, entity, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: claim2,
                entity: persisted,
                lease_id: LeaseId(2),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 2,
                },
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        app.world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .clear();

        // When: Grant1, Deny1, and Expire1 arrive after Grant2.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: claim1,
                entity: persisted,
                lease_id: LeaseId(1),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 1,
                },
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        assert!(matches!(
            app.world().get::<AuthorityPhase>(entity),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(2),
                ..
            })
        ));

        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Deny {
                claim_id: Some(claim1),
                entity: persisted,
                reason: DenyReason::NotEligible,
                retry_after_ms: 0,
            });
        app.update();
        assert!(matches!(
            app.world().get::<AuthorityPhase>(entity),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(2),
                ..
            })
        ));

        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Expire {
                entity: persisted,
                lease_id: LeaseId(1),
                last_holder: None,
                reason: ExpireReason::Revoked,
                disposition: ExpireDisposition::Free,
            });
        app.update();

        // Then: Grant2 remains the sole local fence until Expire2 arrives.
        assert!(matches!(
            app.world().get::<AuthorityPhase>(entity),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(2),
                ..
            })
        ));
        assert_eq!(
            app.world()
                .get::<Authority>(entity)
                .map(|authority| authority.seq),
            Some(SeqPair {
                own_seq: 0,
                auth_seq: 2,
            })
        );
        assert!(app.world().get::<LocallyAuthoritative>(entity).is_some());
        assert!(app
            .world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .drain()
            .next()
            .is_none());

        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Expire {
                entity: persisted,
                lease_id: LeaseId(2),
                last_holder: None,
                reason: ExpireReason::Revoked,
                disposition: ExpireDisposition::Free,
            });
        app.update();
        assert_eq!(
            app.world().get::<AuthorityPhase>(entity),
            Some(&AuthorityPhase::Remote)
        );
        assert!(app.world().get::<LocallyAuthoritative>(entity).is_none());
        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<AuthorityEvent>>()
                .drain()
                .collect::<Vec<_>>(),
            vec![AuthorityEvent::Lost {
                entity,
                lease_id: LeaseId(2),
            }]
        );
    }

    #[test]
    fn stale_control_reply_permutations_remain_idempotent() {
        let permutations = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for order in permutations {
            // Given: claim two owns the current fence.
            let mut app = App::new();
            app.add_plugins(OrreryAuthorityPlugin);
            let persisted = PersistId::new(43);
            let entity = app
                .world_mut()
                .spawn((PersistIdentity(persisted), AuthorityPhase::Remote))
                .id();
            let stale_claim = begin_test_claim(&mut app, entity, persisted);
            let current_claim = begin_test_claim(&mut app, entity, persisted);
            app.world_mut()
                .resource_mut::<LeaseInbox>()
                .0
                .push(LeaseMsg::Grant {
                    claim_id: current_claim,
                    entity: persisted,
                    lease_id: LeaseId(2),
                    seq: SeqPair {
                        own_seq: 0,
                        auth_seq: 2,
                    },
                    ttl_ms: 10_000,
                    prev_holder: None,
                });
            app.update();
            app.world_mut()
                .resource_mut::<Messages<AuthorityEvent>>()
                .clear();
            let stale = [
                LeaseMsg::Grant {
                    claim_id: stale_claim,
                    entity: persisted,
                    lease_id: LeaseId(1),
                    seq: SeqPair {
                        own_seq: 0,
                        auth_seq: 1,
                    },
                    ttl_ms: 10_000,
                    prev_holder: None,
                },
                LeaseMsg::Deny {
                    claim_id: Some(stale_claim),
                    entity: persisted,
                    reason: DenyReason::NotEligible,
                    retry_after_ms: 0,
                },
                LeaseMsg::Expire {
                    entity: persisted,
                    lease_id: LeaseId(1),
                    last_holder: None,
                    reason: ExpireReason::Revoked,
                    disposition: ExpireDisposition::Free,
                },
            ];

            // When: stale replies are repeated in every ordering within one update.
            let repeated = order
                .into_iter()
                .chain(order)
                .map(|index| stale[index].clone());
            app.world_mut()
                .resource_mut::<LeaseInbox>()
                .0
                .extend(repeated);
            app.update();

            // Then: no ordering or duplicate interrupts the current writer.
            assert!(matches!(
                app.world().get::<AuthorityPhase>(entity),
                Some(AuthorityPhase::LocalGranted {
                    lease_id: LeaseId(2),
                    ..
                })
            ));
            assert!(app.world().get::<LocallyAuthoritative>(entity).is_some());
            assert!(app
                .world_mut()
                .resource_mut::<Messages<AuthorityEvent>>()
                .drain()
                .next()
                .is_none());
        }
    }

    #[test]
    fn grant_enables_and_deny_removes_the_only_uplink_marker() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let e = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(4)), AuthorityPhase::Remote))
            .id();
        let first_claim = begin_test_claim(&mut app, e, PersistId::new(4));
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: first_claim,
                entity: PersistId::new(4),
                lease_id: LeaseId(2),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_some());
        let second_claim = begin_test_claim(&mut app, e, PersistId::new(4));
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Deny {
                claim_id: Some(second_claim),
                entity: PersistId::new(4),
                reason: DenyReason::NotEligible,
                retry_after_ms: 0,
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_none());
        let _ = CellId::ROOT;
    }

    #[test]
    fn claim_queues_the_complete_request_and_marks_the_entity_pending() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let entity = app.world_mut().spawn_empty().id();
        let persist = PersistId::new(7);
        let grid = GridId::new(8);
        let cell = CellId::ROOT;
        let observed = SeqPair {
            own_seq: 2,
            auth_seq: 3,
        };
        let tick = Tick::new(9);
        app.add_systems(Update, move |mut client: LeaseClient| {
            let _ = client.claim(LeaseClaim {
                entity,
                persist,
                grid,
                cell,
                kind: ClaimKind::Weak,
                basis: ClaimBasis::Contact { tick },
                observed,
                tick,
            });
        });

        app.update();

        assert_eq!(
            app.world().get::<AuthorityPhase>(entity),
            Some(&AuthorityPhase::LocalPending {
                claim_id: ClaimId(1),
            })
        );
        assert_eq!(
            app.world().resource::<LeaseOutbox>().0,
            vec![LeaseMsg::Claim {
                claim_id: ClaimId(1),
                entity: persist,
                grid,
                cell,
                kind: ClaimKind::Weak,
                basis: ClaimBasis::Contact { tick },
                observed,
                tick,
            }]
        );
    }

    #[test]
    fn client_revoke_returns_the_granted_fencing_token() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let persist = PersistId::new(8);
        let entity = app
            .world_mut()
            .spawn((PersistIdentity(persist), AuthorityPhase::Remote))
            .id();
        let claim_id = begin_test_claim(&mut app, entity, persist);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id,
                entity: persist,
                lease_id: LeaseId(5),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });

        app.update();

        assert_eq!(
            app.world_mut()
                .resource_mut::<AuthorityState>()
                .revoke_local_lease(persist),
            Some(LeaseId(5))
        );
    }

    #[test]
    fn invalid_heartbeat_ack_immediately_removes_the_local_fence() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin);
        let e = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(5)), AuthorityPhase::Remote))
            .id();
        let claim_id = begin_test_claim(&mut app, e, PersistId::new(5));
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id,
                entity: PersistId::new(5),
                lease_id: LeaseId(3),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_some());

        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::HeartbeatAck {
                leases: Vec::new(),
                invalid: vec![LeaseId(3)],
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_none());
        assert_eq!(
            app.world().get::<AuthorityPhase>(e),
            Some(&AuthorityPhase::Remote)
        );
    }

    #[test]
    fn heartbeat_refreshes_the_local_deadline_not_the_gateway_clock() {
        let mut app = App::new();
        app.init_resource::<AuthorityState>()
            .init_resource::<LeaseInbox>()
            .add_message::<AuthorityEvent>()
            .add_systems(Update, process_lease_replies);
        let e = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(6)), AuthorityPhase::Remote))
            .id();
        let claim_id = begin_test_claim(&mut app, e, PersistId::new(6));
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id,
                entity: PersistId::new(6),
                lease_id: LeaseId(4),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        app.world_mut()
            .resource_mut::<AuthorityState>()
            .set_now_ms(500);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::HeartbeatAck {
                leases: vec![Lease {
                    entity: PersistId::new(6),
                    holder: Some(NodeId::from_bytes(&[0; 32]).unwrap()),
                    seq: SeqPair::default(),
                    lease_id: LeaseId(4),
                    // Deliberately unrelated gateway-process monotonic time.
                    expires_at: 4,
                    flags: LeaseFlags::default(),
                    bound_to: None,
                }],
                invalid: Vec::new(),
            });
        app.update();
        assert_eq!(
            app.world().get::<AuthorityPhase>(e),
            Some(&AuthorityPhase::LocalGranted {
                lease_id: LeaseId(4),
                expires_at_ms: 10_500,
            })
        );
    }
}
