//! Optimistic client-side authority state (D7).
//!
//! This crate intentionally owns the one canonical
//! [`LocallyAuthoritative`] marker. Persistence may only uplink entities with
//! that marker, which is inserted after a registrar grant and removed on every
//! loss path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod contact;
pub mod ephemeral;
pub mod hit;

use std::collections::BTreeMap;
use std::time::Duration;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_platform::time::Instant;
use orrery_protocol::{
    ClaimBasis, ClaimId, ClaimKind, DenyReason, ExpireDisposition, GridId, HitWindow, IslandId,
    LeaseId, LeaseMsg, NodeId, PersistId, SeqPair, Tick,
};

pub use contact::{
    advance_contact_clock, propagate_contact_islands, ContactBody, ContactBurst, ContactClaim,
    ContactNode, ContactObservations, ContactPropagator, ContactStatus, ContactTick,
    InterestCoverage, CONTACT_BATCH_CAP,
};
pub use ephemeral::{
    process_island_claims, Ephemeral, EphemeralId, EphemeralOutcome, EphemeralRegistry,
    IslandAuthoritative, IslandAuthorityEvent, IslandClaim, IslandClient, IslandInbox,
    IslandOutbox,
};
pub use hit::{
    CanonicalPosePublications, ClaimAnswer, HitRules, PoseHistory, PoseRing, PoseSample,
    MAX_CLAIM_SOURCES,
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
    /// The registrar reported where an entity's authority went, to a peer
    /// that never held a fence for it (D25, `docs/adr/0025-expire-fan-out.md`,
    /// rule 5).
    ///
    /// Deliberately not [`AuthorityEvent::Lost`], which keeps its single
    /// meaning: *a fence I held has ended*. Nothing ended here — this peer
    /// was an observer of the entity before the message and is an observer of
    /// it after, and the only thing that changed is which peer it believes is
    /// writing the entity. Game code reads it to demote a body from "simulated
    /// by peer X" to "parked, render-only" without a claim round trip, or to
    /// repoint an interpolated proxy at the successor.
    ///
    /// It is an **advisory**, not a correctness mechanism: D25 §8-9 let the
    /// gateway drop it under load, and `Deny{Parked}` on an actual claim
    /// remains the authoritative answer. Nothing downstream may become correct
    /// only if this event arrives.
    DispositionObserved {
        /// ECS entity whose holder belief changed.
        entity: Entity,
        /// The holder this peer now believes in: the named successor for
        /// [`ExpireDisposition::Reassigned`], and none for
        /// [`ExpireDisposition::Parked`] and [`ExpireDisposition::Free`].
        holder: Option<NodeId>,
        /// What the registrar said became of the entity, carried verbatim so
        /// game code can tell a parked body from a claimable one — a
        /// distinction `holder: None` alone erases.
        disposition: ExpireDisposition,
        /// The registrar's per-row token the advisory was ordered by. Purely
        /// informational to a recipient that never installed it; it is not a
        /// fence, and holding this event does not authorize any write.
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
    // The highest `lease_id` whose *observed* disposition has been applied,
    // per entity (D25 rule 6). One `u64` per entity this peer watches but does
    // not hold, and the only defence an observer has against a re-delivered
    // advisory: a fan-out copy carries no `seq`, so the sequence pair cannot
    // order it, and this peer holds no fence to order it by either. Without
    // this map a reconnect that replays an old advisory repoints
    // `Authority.holder` at a peer the registrar has already replaced —
    // silently, since an observer raises no `Lost`.
    observed_expiries: BTreeMap<PersistId, LeaseId>,
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
            observed_expiries: BTreeMap::new(),
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

    /// Every persistent entity this peer currently holds a fence for, paired
    /// with the ECS entity carrying it.
    ///
    /// Exposed for the callers that have to act on *all* of them at once —
    /// today that is the island drain (D24), which releases the lot rather than
    /// selecting among them. Ordinary gameplay names one entity and wants
    /// [`Self::local_lease_id`]; iterating here to find a single lease is a
    /// linear scan of a map that answers the question directly.
    ///
    /// Ordered by [`PersistId`], because the backing map is: two peers
    /// draining the same set emit their divestitures in the same order, which
    /// makes a captured [`LeaseOutbox`] comparable across runs.
    pub fn held_leases(&self) -> impl Iterator<Item = (PersistId, Entity)> + '_ {
        self.leases.iter().map(|(id, lease)| (*id, lease.entity))
    }

    /// The highest fan-out `lease_id` whose disposition this peer has applied
    /// as an observer of `entity`, if any (D25 rule 6).
    ///
    /// Independent of [`AuthorityState::local_lease_id`], and never a fence:
    /// an entry here says only "an advisory this old has already been acted
    /// on", which is what makes a re-delivered copy a no-op.
    #[must_use]
    pub fn observed_disposition_high_water(&self, entity: PersistId) -> Option<LeaseId> {
        self.observed_expiries.get(&entity).copied()
    }

    /// Forget the observed-disposition high-water mark for `entity`.
    ///
    /// Call this when the entity leaves this peer's interest set, which is
    /// what bounds the map: D25 sizes it by the interest set (24 high-rate
    /// bodies plus proxies, D16) on the understanding that it is evicted with
    /// the cell subscription that produced it. Re-entering interest starts the
    /// entity from no mark, which is correct — the peer has no belief to
    /// protect at that point, and the first advisory it hears is the freshest
    /// thing it knows.
    pub fn forget_observed_disposition(&mut self, entity: PersistId) {
        self.observed_expiries.remove(&entity);
    }

    /// Allocate and record a claim correlation identifier for `entity`.
    pub fn begin_claim(&mut self, entity: PersistId) -> Option<ClaimId> {
        let claim_id = self.next_claim_id;
        self.next_claim_id = ClaimId(claim_id.0.checked_add(1)?);
        self.pending_claims.insert(entity, claim_id);
        Some(claim_id)
    }
}

/// Milliseconds since this process first read a client clock.
///
/// One origin for every client-process clock this crate drives, so
/// [`AuthorityState::now_ms`] and [`contact::ContactTick::now_ms`] are
/// comparable: they measure lease expiry and claim back-off against the same
/// monotonic zero, and two independently anchored clocks would disagree by
/// however long the app took to reach the second one.
pub(crate) fn process_uptime_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Advance the client-process monotonic safety clock once per update.
pub fn advance_lease_clock(mut state: ResMut<AuthorityState>) {
    state.now_ms = process_uptime_ms();
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

    /// Every lease this peer currently holds, as
    /// [`AuthorityState::held_leases`] reports them.
    ///
    /// Collected rather than borrowed: the one caller — the island drain — is
    /// going to call [`Self::divest`] for each, and `divest` takes `&mut self`.
    /// A held set is at most the entities one peer has authority over, so the
    /// allocation is paid once per drain rather than per frame.
    #[must_use]
    pub fn held_leases(&self) -> Vec<(PersistId, Entity)> {
        self.state.held_leases().collect()
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
    // `Option<&Authority>` because the expiry path must not reset the pair it
    // stamps: INV-2 says the sequences never decrease, and an entity's last
    // known pair is the only value on hand that satisfies it.
    entities: Query<(Entity, &crate::PersistIdentity, Option<&Authority>), With<AuthorityPhase>>,
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
                if let Some((entity_ref, _, _)) = entities.iter().find(|(_, id, _)| id.0 == entity)
                {
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
                // A wrong-owner refusal is the one `DenyReason` that is not
                // about the claimant at all: the node it reached hosts no
                // shard over the cell (docs/08-persistence.md §3.5). The
                // gateway also sends it **unsolicited** — with no `claim_id`
                // — when a batched heartbeat could not be routed, and a peer
                // that dropped that silently would see only the refusal
                // accompanying it and read it as an unexplained lease loss.
                //
                // So it is surfaced even uncorrelated, and surfacing is all
                // it does: no phase moves, no fence is touched, nothing is
                // revoked. Re-addressing is the response, and this crate does
                // not know where to (ADR-0026); game code reading the reason
                // decides.
                let correlated =
                    claim_id.is_some() && state.pending_claims.get(&entity) == claim_id.as_ref();
                if !correlated {
                    if matches!(reason, DenyReason::WrongOwner { .. }) {
                        if let Some((entity_ref, _, _)) =
                            entities.iter().find(|(_, id, _)| id.0 == entity)
                        {
                            events.write(AuthorityEvent::Denied {
                                entity: entity_ref,
                                reason,
                            });
                        }
                    }
                    continue;
                }
                if let Some((entity_ref, _, _)) = entities.iter().find(|(_, id, _)| id.0 == entity)
                {
                    state.pending_claims.remove(&entity);
                    // A `Deny` refuses the *claim*, and the claim is all it can
                    // refuse: it carries no `lease_id` (docs/04 §3), so it says
                    // nothing about a fence this peer already holds. Dropping
                    // one here would take a weakly held body away from its
                    // legitimate writer the moment a strong upgrade was
                    // refused, while the registrar still names this peer as the
                    // holder — an entity nobody writes until the TTL runs out.
                    // Only expiry, revocation and divestiture end a fence.
                    if let Some(lease) = state.leases.get(&entity) {
                        commands
                            .entity(entity_ref)
                            .insert(AuthorityPhase::LocalGranted {
                                lease_id: lease.lease_id,
                                expires_at_ms: lease.expires_at_ms,
                            });
                    } else {
                        commands
                            .entity(entity_ref)
                            .remove::<LocallyAuthoritative>()
                            .insert(AuthorityPhase::Remote);
                    }
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
                // One message with two readings of one field, and the reading
                // is decided here, locally, by whether this peer has the
                // entity in `state.leases` (D25 rule 4). To the holder,
                // `lease_id` is the fencing token being revoked. To everybody
                // else it is an ordering token on an advisory about somebody
                // else's fence. The branches share the message and nothing
                // else: the observer half installs no fence, drops none,
                // touches no `SeqPair` and moves no phase.
                let installed = state.leases.get(&entity).map(|lease| lease.lease_id);
                if let Some(installed) = installed {
                    if installed != lease_id {
                        // A fence is installed and this is not about it. The
                        // dangerous case is the one the fan-out created: a
                        // copy addressed to the *previous* holder arriving
                        // after the registrar granted this entity to this
                        // peer. Falling through to the observer branch would
                        // clear a fence the registrar had just issued, on the
                        // strength of a message that is not addressed to this
                        // peer at all — the INV-2 failure
                        // `orrery_persist_client`'s stale-NACK test exists to
                        // prevent, arriving through the new door.
                        continue;
                    }
                } else {
                    // Observer half (D25 rule 5): change exactly one thing,
                    // this peer's belief about who holds the entity.
                    //
                    // The ordering gate first, because it is the whole of the
                    // observer's defence. A non-holder has no fence to compare
                    // against and must not touch the pair, so `lease_id` —
                    // monotone per row, since the registrar increments it on
                    // every acquire — is the only order available. Apply only
                    // what is strictly newer than the newest already applied.
                    if state
                        .observed_expiries
                        .get(&entity)
                        .is_some_and(|applied| lease_id <= *applied)
                    {
                        continue;
                    }
                    // Two conditions in one pattern, and both are drops rather
                    // than spawns. An unknown `PersistId` is a body this peer
                    // does not replicate: it is ignored, silently, because a
                    // fan-out set is a superset of who cares and a `warn!` per
                    // copy would be a log line per uninteresting expiry. An
                    // entity with no `Authority` yet is subtler — it has no
                    // pair on file, so honouring the advisory would mean
                    // *minting* one, and a row at `(0, 0)` is superseded by
                    // every row there has ever been, including ones the
                    // registrar has already moved past. That is the exact
                    // repointing hazard D25 rule 6 forbids, so the peer waits
                    // for replication to give it a pair to defend.
                    let Some((entity_ref, _, Some(authority))) =
                        entities.iter().find(|(_, id, _)| id.0 == entity)
                    else {
                        continue;
                    };
                    let holder = match &disposition {
                        ExpireDisposition::Reassigned { to } => Some(*to),
                        // No successor stream will ever arrive for these two,
                        // which is why they are the dispositions D25 rule 7
                        // fans out at all: nothing else would ever repoint an
                        // observer off the departed holder.
                        ExpireDisposition::Parked | ExpireDisposition::Free => None,
                    };
                    // The pair is carried over verbatim. `Expire` has no `seq`
                    // field and a recipient may not synthesise, reset or zero
                    // one (D25 rule 6, INV-2).
                    let seq = authority.seq;
                    state.observed_expiries.insert(entity, lease_id);
                    commands
                        .entity(entity_ref)
                        .insert(Authority { holder, seq });
                    events.write(AuthorityEvent::DispositionObserved {
                        entity: entity_ref,
                        holder,
                        disposition,
                        lease_id,
                    });
                    continue;
                }
                if let Some(lease) = state.leases.remove(&entity) {
                    // The pair carries over: `Expire` has no `seq` field
                    // (docs/04 §3), and INV-2 forbids the sequences going
                    // backwards. Resetting to the default would leave the row
                    // at `(0, 0)`, which *any* later row supersedes — so a
                    // duplicated or reordered gateway NACK naming a peer the
                    // registrar has already moved past would repoint the
                    // holder, on the strength of a datagram that lost its race.
                    let known = entities
                        .get(lease.entity)
                        .ok()
                        .and_then(|(_, _, authority)| authority)
                        .map_or_else(SeqPair::default, |authority| authority.seq);
                    let mut entity_commands = lease_commands(&mut commands, lease.entity);
                    // The disposition names where authority actually went, so
                    // the loser can render the entity against its real holder
                    // instead of a stale one it no longer is.
                    if let ExpireDisposition::Reassigned { to } = disposition {
                        entity_commands.insert(Authority {
                            holder: Some(to),
                            seq: known,
                        });
                    } else {
                        entity_commands.insert(Authority {
                            holder: None,
                            seq: known,
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
                //
                // Matched on the entity as well as the token: `LeaseId` is a
                // per-row counter, so one refused `LeaseId(1)` would otherwise
                // drop every other entity this peer holds at its first token.
                let invalidated: Vec<_> = state
                    .leases
                    .iter()
                    .filter_map(|(persist, local)| {
                        invalid
                            .contains(&(*persist, local.lease_id))
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
            renew: state
                .leases
                .iter()
                .map(|(persist, lease)| (*persist, lease.lease_id))
                .collect(),
            tick: Tick::new(state.now_ms),
        });
        state.last_heartbeat = Instant::now();
    }
    let _ = phases; // keeps the system extensible without reading world state on the hot path.
}

/// The island this peer belongs to, as the coordinator most recently said.
///
/// Kept here rather than read from `orrery_net::IslandMembership` because
/// authority is the lower layer: the ephemeral namespace and the in-island
/// tiebreak both need the island id and the manifest epoch, and neither may
/// depend on the transport crate that happens to receive the manifest. The
/// net layer pushes; this crate never pulls.
#[derive(Debug, Default, Resource)]
pub struct IslandBinding {
    /// The island this peer belongs to, if the coordinator has assigned one.
    pub island: Option<IslandId>,
    /// The manifest epoch that assignment came from.
    pub epoch: u32,
}

/// Mirror the current island binding into the ephemeral registry.
///
/// Runs unconditionally rather than on change detection: the registry is the
/// only thing that mints ephemeral ids, and a peer that spawned a projectile
/// into a stale namespace would be minting ids nobody else recognises.
pub fn track_island_binding(
    binding: Res<IslandBinding>,
    state: Res<AuthorityState>,
    mut registry: ResMut<ephemeral::EphemeralRegistry>,
) {
    if binding.is_changed() {
        registry.set_island(binding.island, binding.epoch);
    }
    // Compared rather than change-detected: `advance_lease_clock` writes
    // `AuthorityState` every frame, so its change flag says nothing about the
    // identity, and re-stamping it would dirty the registry every frame.
    if registry.node() != state.node {
        registry.set_node(state.node);
    }
}

/// Persistent identity component owned by authority. `orrery_persist_client`
/// retains its legacy wrapper and maps it during migration.
#[derive(Debug, Clone, Copy, Component)]
pub struct PersistIdentity(pub PersistId);

/// Bevy plugin providing client authority state and lease maintenance.
///
/// Also owns [`PoseHistory`], the pose ring hit claims are validated against
/// (docs/05 §7).
/// Its window comes from `orrery_predict`'s numbers by way of the facade —
/// [`with_hit_window`](Self::with_hit_window) — and defaults to
/// [`HitWindow::CLOSED`], which refuses every claim, so a plugin composed
/// without the facade fails closed rather than with a copied figure.
#[derive(Debug, Default, Clone, Copy)]
pub struct OrreryAuthorityPlugin {
    hit_window: HitWindow,
}

impl OrreryAuthorityPlugin {
    /// Validate hit claims within `window` (the rewind cap and the pose ring
    /// depth, from `PredictConfig::hit_window()`).
    #[must_use]
    pub fn with_hit_window(mut self, window: HitWindow) -> Self {
        self.hit_window = window;
        self
    }

    /// The window this plugin will size [`PoseHistory`] with.
    #[must_use]
    pub fn hit_window(&self) -> HitWindow {
        self.hit_window
    }
}

impl Plugin for OrreryAuthorityPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PoseHistory::new(self.hit_window))
            .init_resource::<AuthorityState>()
            .init_resource::<InterestGrant>()
            .init_resource::<IslandBinding>()
            .init_resource::<LeaseInbox>()
            .init_resource::<LeaseOutbox>()
            .init_resource::<contact::ContactObservations>()
            .init_resource::<contact::ContactPropagator>()
            .init_resource::<contact::ContactTick>()
            .init_resource::<contact::InterestCoverage>()
            .init_resource::<ephemeral::EphemeralRegistry>()
            .init_resource::<ephemeral::IslandInbox>()
            .init_resource::<ephemeral::IslandOutbox>()
            .init_resource::<hit::CanonicalPosePublications>()
            .add_message::<AuthorityEvent>()
            .add_message::<ephemeral::IslandAuthorityEvent>()
            .add_systems(
                Update,
                (
                    advance_lease_clock,
                    advance_contact_clock,
                    track_island_binding,
                    // Replies first: a body granted or denied this frame must
                    // be seen with its settled status before the propagator
                    // decides whether to claim it again.
                    process_lease_replies,
                    // The settled live-fence set decides which game-published
                    // canonical poses may enter the authority's hit rings.
                    hit::record_published_held_poses,
                    ephemeral::process_island_claims,
                    // Verdicts reach the planner's back-off state *before* it
                    // plans, or a body refused this frame is re-claimed in the
                    // same frame it was refused.
                    contact::absorb_contact_denials,
                    contact::propagate_contact_islands,
                    maintain_leases,
                )
                    .chain(),
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
        // `advance_lease_clock` derives `now_ms` from a process-wide start
        // instant, so it is only zero if this test happens to be the first in
        // the process to run. Asserting an absolute expiry makes the test a
        // race that a loaded CI runner loses; the invariant under test is that
        // the grant's TTL is applied to the clock the grant was processed on.
        let now_ms = app.world().resource::<AuthorityState>().now_ms;
        assert_eq!(
            app.world().get::<AuthorityPhase>(inherited),
            Some(&AuthorityPhase::LocalGranted {
                lease_id: LeaseId(4),
                expires_at_ms: now_ms + 10_000,
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
                seq: SeqPair {
                    own_seq: 1,
                    auth_seq: 4,
                },
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
        // And the pair does not go backwards. `Expire` carries no `seq`
        // (docs/04 §3), so the loser keeps the last one it knew: stamping the
        // default would leave a row at `(0, 0)` that every later row
        // supersedes, and the next stale gateway NACK would repoint the holder
        // (INV-2, and `orrery_persist_client::replies`).
        assert_eq!(
            app.world().get::<Authority>(lost).map(|a| a.seq),
            Some(SeqPair {
                own_seq: 1,
                auth_seq: 4,
            })
        );
    }

    #[test]
    fn current_grant_deny_and_expire_replies_apply_the_documented_transitions() {
        // Given: two optimistic claims awaiting their registrar replies.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
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
        // `advance_lease_clock` derives `now_ms` from a process-wide start
        // instant, so it is only zero if this test happens to be the first in
        // the process to run. Asserting an absolute expiry makes the test a
        // race that a loaded CI runner loses; the invariant under test is that
        // the grant's TTL is applied to the clock the grant was processed on.
        let now_ms = app.world().resource::<AuthorityState>().now_ms;
        assert_eq!(
            app.world().get::<AuthorityPhase>(expired),
            Some(&AuthorityPhase::LocalGranted {
                lease_id: LeaseId(9),
                expires_at_ms: now_ms + 10_000,
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
            app.add_plugins(OrreryAuthorityPlugin::default());
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
    fn a_grant_installs_the_only_uplink_marker_and_a_denied_claim_never_does() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let e = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(4)), AuthorityPhase::Remote))
            .id();
        let refused = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(5)), AuthorityPhase::Remote))
            .id();
        let first_claim = begin_test_claim(&mut app, e, PersistId::new(4));
        let refused_claim = begin_test_claim(&mut app, refused, PersistId::new(5));
        app.world_mut().resource_mut::<LeaseInbox>().0.extend([
            LeaseMsg::Grant {
                claim_id: first_claim,
                entity: PersistId::new(4),
                lease_id: LeaseId(2),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            },
            LeaseMsg::Deny {
                claim_id: Some(refused_claim),
                entity: PersistId::new(5),
                reason: DenyReason::NotEligible,
                retry_after_ms: 0,
            },
        ]);
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_some());
        assert!(
            app.world().get::<LocallyAuthoritative>(refused).is_none(),
            "a claim that was never granted must not carry the uplink marker"
        );
        assert_eq!(
            app.world().get::<AuthorityPhase>(refused),
            Some(&AuthorityPhase::Remote)
        );
        let _ = CellId::ROOT;
    }

    #[test]
    fn denying_an_upgrade_claim_leaves_the_fence_the_peer_already_holds() {
        // The failure this catches: dropping a live lease because a *later*
        // claim on the same body was refused. A `Deny` names a `claim_id` and
        // no `lease_id` (docs/04 §3), so it can only refuse the claim; the
        // registrar still has this peer down as the holder, and a client that
        // stopped writing here would leave the body unwritten by anyone until
        // the 10 s TTL ran out — while `LeaseClient::claim` never checked for a
        // held lease before sending the upgrade in the first place.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let held = app
            .world_mut()
            .spawn((PersistIdentity(PersistId::new(6)), AuthorityPhase::Remote))
            .id();
        let weak_claim = begin_test_claim(&mut app, held, PersistId::new(6));
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: weak_claim,
                entity: PersistId::new(6),
                lease_id: LeaseId(3),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        let granted = app
            .world()
            .get::<AuthorityPhase>(held)
            .copied()
            .expect("the weak claim was granted");

        // When: the strong upgrade for the same body is refused.
        let upgrade = begin_test_claim(&mut app, held, PersistId::new(6));
        app.world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .clear();
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Deny {
                claim_id: Some(upgrade),
                entity: PersistId::new(6),
                reason: DenyReason::StrongHeld,
                retry_after_ms: 0,
            });
        app.update();

        // Then: the refusal is reported and the weak fence is untouched.
        assert!(
            app.world().get::<LocallyAuthoritative>(held).is_some(),
            "only expiry, revocation or divestiture ends a fence"
        );
        assert_eq!(app.world().get::<AuthorityPhase>(held), Some(&granted));
        assert_eq!(
            app.world()
                .resource::<AuthorityState>()
                .local_lease_id(PersistId::new(6)),
            Some(LeaseId(3))
        );
        let events: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .drain()
            .collect();
        assert!(matches!(
            events.as_slice(),
            [AuthorityEvent::Denied {
                entity: denied,
                reason: DenyReason::StrongHeld,
            }] if *denied == held
        ));
    }

    #[test]
    fn claim_queues_the_complete_request_and_marks_the_entity_pending() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
        app.add_plugins(OrreryAuthorityPlugin::default());
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
                invalid: vec![(PersistId::new(5), LeaseId(3))],
            });
        app.update();
        assert!(app.world().get::<LocallyAuthoritative>(e).is_none());
        assert_eq!(
            app.world().get::<AuthorityPhase>(e),
            Some(&AuthorityPhase::Remote)
        );
    }

    /// A refusal drops exactly the entity it names.
    ///
    /// `LeaseId` is a per-row counter, so a peer holding several freshly
    /// claimed entities holds several `LeaseId(1)`s. Matching an ack's
    /// `invalid` list on the bare token therefore dropped every one of them —
    /// the holder stops writing entities the registrar never refused.
    #[test]
    fn an_invalid_ack_does_not_drop_a_sibling_at_the_same_token() {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let refused = PersistId::new(1);
        let kept = PersistId::new(2);
        let mut spawn = |persisted: PersistId| {
            let entity = app
                .world_mut()
                .spawn((PersistIdentity(persisted), AuthorityPhase::Remote))
                .id();
            let claim_id = begin_test_claim(&mut app, entity, persisted);
            app.world_mut()
                .resource_mut::<LeaseInbox>()
                .0
                .push(LeaseMsg::Grant {
                    claim_id,
                    entity: persisted,
                    // Both entities are freshly claimed, so both sit at the
                    // first token: the collision is the normal case, not a
                    // contrived one.
                    lease_id: LeaseId(1),
                    seq: SeqPair::default(),
                    ttl_ms: 10_000,
                    prev_holder: None,
                });
            app.update();
            entity
        };
        let refused_entity = spawn(refused);
        let kept_entity = spawn(kept);
        assert!(app
            .world()
            .get::<LocallyAuthoritative>(kept_entity)
            .is_some());

        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::HeartbeatAck {
                leases: Vec::new(),
                invalid: vec![(refused, LeaseId(1))],
            });
        app.update();

        assert!(
            app.world()
                .get::<LocallyAuthoritative>(refused_entity)
                .is_none(),
            "the refused entity must stop writing"
        );
        assert!(
            app.world()
                .get::<LocallyAuthoritative>(kept_entity)
                .is_some(),
            "a sibling at the same per-row token keeps its fence"
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
        // Absolute is correct here, unlike the plugin-driven tests: this app
        // never registers `advance_lease_clock`, so `now_ms` is exactly what
        // `set_now_ms` put there and the deadline is fully determined.
        assert_eq!(
            app.world().get::<AuthorityPhase>(e),
            Some(&AuthorityPhase::LocalGranted {
                lease_id: LeaseId(4),
                expires_at_ms: 10_500,
            })
        );
    }

    /// An `Expire` for an entity this peer merely watches, addressed by the
    /// token whichever peer actually held it believed it had installed.
    fn observed_expire(
        entity: PersistId,
        lease_id: LeaseId,
        last_holder: NodeId,
        disposition: ExpireDisposition,
    ) -> LeaseMsg {
        LeaseMsg::Expire {
            entity,
            lease_id,
            last_holder: Some(last_holder),
            reason: ExpireReason::Disconnect,
            disposition,
        }
    }

    /// A replicated body this peer renders and does not hold: it knows the
    /// persistent identity and a sequence pair, and holds no fence.
    fn observed_entity(
        app: &mut App,
        persisted: PersistId,
        holder: NodeId,
        seq: SeqPair,
    ) -> Entity {
        app.world_mut()
            .spawn((
                PersistIdentity(persisted),
                AuthorityPhase::Remote,
                Authority {
                    holder: Some(holder),
                    seq,
                },
            ))
            .id()
    }

    fn drain_events(app: &mut App) -> Vec<AuthorityEvent> {
        app.world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .drain()
            .collect()
    }

    #[test]
    fn an_observed_reassignment_repoints_the_holder_and_touches_nothing_else() {
        // Given: a body this peer replicates from node 1 and never claimed.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let persisted = PersistId::new(70);
        let pair = SeqPair {
            own_seq: 3,
            auth_seq: 7,
        };
        let watched = observed_entity(&mut app, persisted, node_id(1), pair);

        // When: a fan-out copy says authority went to node 2. Its `lease_id`
        // is node 1's token, which this peer never installed — to it the field
        // is an ordering token and nothing more (D25 rule 4). `Reassigned` is
        // holder-only in the registrar D25 rule 7 describes, but a client is
        // permissive about what it accepts.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(observed_expire(
                persisted,
                LeaseId(9),
                node_id(1),
                ExpireDisposition::Reassigned { to: node_id(2) },
            ));
        app.update();

        // Then: exactly one thing changed.
        let authority = app.world().get::<Authority>(watched).expect("still known");
        assert_eq!(
            authority.holder,
            Some(node_id(2)),
            "the observer repoints at the successor the disposition names"
        );
        assert_eq!(
            authority.seq, pair,
            "an `Expire` carries no `seq`, so the pair this peer learned from \
             replication is the pair it keeps (INV-2, D25 rule 6)"
        );
        assert!(
            app.world().get::<LocallyAuthoritative>(watched).is_none(),
            "an advisory never authorizes a local write"
        );
        assert_eq!(
            app.world().get::<AuthorityPhase>(watched),
            Some(&AuthorityPhase::Remote),
            "the observer was remote before the message and is remote after it"
        );
        assert!(
            app.world()
                .resource::<AuthorityState>()
                .local_lease_id(persisted)
                .is_none(),
            "no fence was installed by a message addressed to somebody else"
        );
        assert_eq!(
            drain_events(&mut app),
            vec![AuthorityEvent::DispositionObserved {
                entity: watched,
                holder: Some(node_id(2)),
                disposition: ExpireDisposition::Reassigned { to: node_id(2) },
                lease_id: LeaseId(9),
            }],
            "reported as an observation, never as `Lost`: nothing this peer \
             held ended"
        );
    }

    #[test]
    fn an_observed_park_clears_the_holder_once_however_many_copies_arrive() {
        // Given: a body this peer renders, written by node 1.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let persisted = PersistId::new(71);
        let pair = SeqPair {
            own_seq: 1,
            auth_seq: 4,
        };
        let watched = observed_entity(&mut app, persisted, node_id(1), pair);

        // When: node 1 goes away and the lease parks for want of a successor.
        // This is the disposition with no self-healing path — no successor
        // stream will ever raise the pair — so the advisory is the only thing
        // that will ever tell this peer to stop expecting writes.
        let parked = observed_expire(persisted, LeaseId(9), node_id(1), ExpireDisposition::Parked);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(parked.clone());
        app.update();

        let authority = app.world().get::<Authority>(watched).expect("still known");
        assert_eq!(authority.holder, None, "a parked entity has no holder");
        assert_eq!(authority.seq, pair, "and the pair is untouched");
        assert_eq!(
            drain_events(&mut app),
            vec![AuthorityEvent::DispositionObserved {
                entity: watched,
                holder: None,
                disposition: ExpireDisposition::Parked,
                lease_id: LeaseId(9),
            }],
            "the parked-render demotion is reported exactly once"
        );

        // When: the same copy is delivered again — a reconnect replays what
        // the peer missed, and the lane is free to duplicate.
        app.world_mut().resource_mut::<LeaseInbox>().0.push(parked);
        app.update();

        // Then: nothing at all happens. `lease_id` does not exceed the mark
        // the first copy left, so there is no second demotion for game code
        // to act on.
        assert!(
            drain_events(&mut app).is_empty(),
            "a duplicate advisory changes nothing on second delivery"
        );
        let authority = app.world().get::<Authority>(watched).expect("still known");
        assert_eq!(authority.holder, None);
        assert_eq!(authority.seq, pair);
    }

    #[test]
    fn an_observed_copy_cannot_clear_the_fence_this_peer_now_holds() {
        // Given: this peer watched node 1's body, then claimed it and was
        // granted it. The registrar's per-row token advanced, as it does on
        // every acquire.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let persisted = PersistId::new(72);
        let watched = observed_entity(
            &mut app,
            persisted,
            node_id(1),
            SeqPair {
                own_seq: 0,
                auth_seq: 4,
            },
        );
        let claim_id = begin_test_claim(&mut app, watched, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id,
                entity: persisted,
                lease_id: LeaseId(10),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 5,
                },
                ttl_ms: 10_000,
                prev_holder: Some(node_id(1)),
            });
        app.update();
        drain_events(&mut app);

        // When: the fan-out copy addressed to node 1 arrives late — the
        // registrar sent it before it granted, and the lanes reordered.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(observed_expire(
                persisted,
                LeaseId(9),
                node_id(1),
                ExpireDisposition::Parked,
            ));
        app.update();

        // Then: it is not an observer copy at all, because this peer is now
        // the holder. Treating it as one would revoke a fence the registrar
        // has just issued, on the strength of a message addressed to the peer
        // that lost the entity — the INV-2 failure the stale-NACK regression
        // in `orrery_persist_client::replies` exists to prevent, restated for
        // the fan-out lane.
        assert_eq!(
            app.world()
                .resource::<AuthorityState>()
                .local_lease_id(persisted),
            Some(LeaseId(10)),
            "the fence this peer holds survives an advisory about the last one"
        );
        assert!(app.world().get::<LocallyAuthoritative>(watched).is_some());
        assert!(matches!(
            app.world().get::<AuthorityPhase>(watched),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(10),
                ..
            })
        ));
        let local_node = app.world().resource::<AuthorityState>().node;
        assert_eq!(
            app.world().get::<Authority>(watched).expect("held").holder,
            Some(local_node),
            "this peer is still the holder it was granted as"
        );
        assert!(
            drain_events(&mut app).is_empty(),
            "no loss and no observation: the message was about somebody else"
        );
    }

    #[test]
    fn an_observed_copy_below_the_high_water_mark_is_ignored() {
        // Given: this peer has already applied the advisory that parked the
        // entity at token 9.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let persisted = PersistId::new(73);
        let pair = SeqPair {
            own_seq: 2,
            auth_seq: 6,
        };
        let watched = observed_entity(&mut app, persisted, node_id(1), pair);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(observed_expire(
                persisted,
                LeaseId(9),
                node_id(1),
                ExpireDisposition::Parked,
            ));
        app.update();
        drain_events(&mut app);
        assert_eq!(
            app.world()
                .resource::<AuthorityState>()
                .observed_disposition_high_water(persisted),
            Some(LeaseId(9)),
            "applying an advisory raises the mark that orders the next one"
        );

        // When: an older advisory is re-delivered after a reconnect, naming a
        // holder the registrar has already moved past.
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(observed_expire(
                persisted,
                LeaseId(5),
                node_id(1),
                ExpireDisposition::Reassigned { to: node_id(3) },
            ));
        app.update();

        // Then: it is dropped. This peer holds no fence and the message
        // carries no `seq`, so `lease_id` — monotone per row — is the only
        // order there is; without this gate the reconnect repoints the body at
        // node 3 with no `Lost` event to show for it (D25 rule 6).
        let authority = app.world().get::<Authority>(watched).expect("still known");
        assert_eq!(
            authority.holder, None,
            "the newer parking still stands after the older copy"
        );
        assert_eq!(authority.seq, pair);
        assert!(
            drain_events(&mut app).is_empty(),
            "a superseded advisory is not an event"
        );
    }

    #[test]
    fn an_observed_expire_for_an_unknown_entity_is_dropped_silently() {
        // Given: a peer that replicates one body and knows nothing of another.
        // The fan-out set is "everyone whose interest covers the cell", a
        // superset of everyone who cares, so copies for bodies outside this
        // peer's replicated set are ordinary traffic rather than an anomaly:
        // neither spawned nor logged at `warn`.
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        let known = PersistId::new(74);
        let unknown = PersistId::new(75);
        observed_entity(
            &mut app,
            known,
            node_id(1),
            SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
        );

        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(observed_expire(
                unknown,
                LeaseId(9),
                node_id(1),
                ExpireDisposition::Parked,
            ));
        app.update();

        let mut identities = app.world_mut().query::<&PersistIdentity>();
        let live: Vec<_> = identities.iter(app.world()).map(|id| id.0).collect();
        assert_eq!(
            live,
            vec![known],
            "an advisory never spawns: `AreaPage` carries no holder, so a peer \
             that does not already replicate the body has nothing to attach it to"
        );
        assert!(
            drain_events(&mut app).is_empty(),
            "and nothing is reported about a body game code has never seen"
        );
        assert!(
            app.world()
                .resource::<AuthorityState>()
                .observed_disposition_high_water(unknown)
                .is_none(),
            "a dropped advisory leaves no mark, so the map stays bounded by \
             the interest set rather than by everything the gateway ever sent"
        );
    }
}
