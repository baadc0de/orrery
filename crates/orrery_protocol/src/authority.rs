//! Authority lease wire types (D7).

use core::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::{CellId, Epoch, GridId, Lsn, NodeId, PersistId, Tick};

/// The authoritative sequence pair for an entity.
///
/// Ownership is the most significant part of the ordering: a strong ownership
/// transition always supersedes any number of weak-authority transitions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SeqPair {
    /// Monotonic strong-ownership sequence.
    pub own_seq: u32,
    /// Monotonic weak-authority sequence.
    pub auth_seq: u32,
}

impl Ord for SeqPair {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.own_seq, self.auth_seq).cmp(&(other.own_seq, other.auth_seq))
    }
}

impl PartialOrd for SeqPair {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl SeqPair {
    /// Whether this pair supersedes `known` under D7's lexicographic rule.
    #[must_use]
    pub const fn supersedes(self, known: Self) -> bool {
        self.own_seq > known.own_seq
            || (self.own_seq == known.own_seq && self.auth_seq > known.auth_seq)
    }
}

/// Monotonic, entity-scoped fencing token issued by the registrar.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct LeaseId(pub u64);

/// Client-session-scoped identifier correlating a claim with its reply.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ClaimId(pub u64);

impl ClaimId {
    /// The reserved correlation for a grant the registrar issued on its own
    /// initiative — successor selection after a holder was lost, or the
    /// receiving half of a negotiated divestiture.
    ///
    /// Clients allocate correlations from `1` upwards, so this value can never
    /// collide with a pending client claim. A client accepts a grant carrying
    /// it without a matching pending claim, but still only when it advances
    /// the fence it already has installed (D7 §5).
    pub const REGISTRAR: Self = Self(0);
}

/// Compact lease flags. This is deliberately a numeric bitset so new flags are
/// wire-compatible with older decoders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LeaseFlags(pub u8);

impl LeaseFlags {
    /// Lease belongs permanently to a player character. Such an entity is
    /// never redistributed to a successor and never claimable by anyone but
    /// the identity in [`Lease::bound_to`].
    pub const PLAYER_BOUND: Self = Self(1 << 0);
    /// Lease currently represents non-stealable strong ownership.
    pub const STRONG_HELD: Self = Self(1 << 1);
    /// Client is operating conservatively while the gateway is unavailable.
    pub const PROVISIONAL: Self = Self(1 << 2);
    /// Entity has no live holder and is served by persistence.
    pub const PARKED: Self = Self(1 << 3);

    /// Test whether all bits in `flag` are present.
    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    /// Set or clear a flag.
    pub fn set(&mut self, flag: Self, value: bool) {
        if value {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

/// Durable registrar row, keyed as `lease/{entity}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Persistent entity identity.
    pub entity: PersistId,
    /// Current writer; `None` is parked/free.
    pub holder: Option<NodeId>,
    /// Authoritative ordering pair.
    pub seq: SeqPair,
    /// Current fencing token.
    pub lease_id: LeaseId,
    /// Registrar-monotonic expiry time in milliseconds.
    pub expires_at: u64,
    /// Lease state flags.
    pub flags: LeaseFlags,
    /// The identity a [`LeaseFlags::PLAYER_BOUND`] entity belongs to.
    ///
    /// A player's character parks when they disconnect and is *exclusively*
    /// reclaimable by them (D7 §4.3), so the binding has to outlive `holder`,
    /// which parking clears. `None` on every entity that is not player-bound.
    #[serde(default)]
    pub bound_to: Option<NodeId>,
}

/// Requested authority tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimKind {
    /// Interaction-derived authority that another claimant may supersede.
    Weak,
    /// Explicit ownership that requires consent or expiry to transfer.
    Strong,
}

/// Evidence supporting a claim. P3 accepts contact, explicit, and orphan
/// recovery; later phases add promotion/reconciliation arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimBasis {
    /// Physics contact observed at this universe tick.
    Contact {
        /// Universe tick at which contact occurred.
        tick: Tick,
    },
    /// Explicit gameplay action such as a grab.
    Explicit,
    /// Registrar-selected successor after a holder vanished.
    Orphan,
}

/// Why the registrar denied a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// Another holder won the serialized race.
    Held {
        /// Node that currently holds the lease.
        holder: NodeId,
        /// Authoritative sequence pair held by that node.
        seq: SeqPair,
    },
    /// Strong ownership cannot be stolen.
    StrongHeld,
    /// Claim did not meet the registrar's admission requirements.
    NotEligible,
    /// Per-peer rate limit was exceeded.
    RateLimited,
    /// Entity is intentionally parked.
    Parked,
    /// This node hosts no shard covering the cell the claim named, so it is
    /// not the owner for that entity (docs/08-persistence.md §3.5).
    ///
    /// Distinct from every other variant because it says nothing about the
    /// *claim*: the claim may be perfectly well-formed and the claimant
    /// perfectly eligible. What is wrong is the address. A peer that reads it
    /// as [`DenyReason::NotEligible`] backs off against a node that will never
    /// answer for this cell however long it waits.
    ///
    /// `shard` echoes the cell the request named. It is the finest shard
    /// identity this node can state: hosting no shard over that cell is
    /// precisely the condition being reported, so a coarser owning shard is
    /// not something it can look up. `epoch` is the shard-ownership epoch
    /// (`FenceRow.epoch`) this node last activated its *own* shards under,
    /// which is what §3.5's "reroute on epoch bump" is written against — a
    /// claimant that has seen a higher epoch knows its routing table is the
    /// fresher of the two.
    ///
    /// **`owner` is always `None` today, and the field exists so that stays
    /// cheap to change.** Which node the claimant should have asked is
    /// ADR-0026's question, and that record is still *Proposed* — nothing here
    /// resolves, follows or depends on a redirect. But postcard encodes a
    /// struct variant's fields positionally, so a field appended after the
    /// fact is a wire break, whereas an `Option` that is already on the wire
    /// costs one zero byte now and nothing at all later. The hole is shaped,
    /// not filled.
    WrongOwner {
        /// The grid the refused request named (P-7: cell ids are
        /// grid-relative, so a cell without its grid names nothing).
        grid: GridId,
        /// The cell the refused request named.
        shard: CellId,
        /// The shard-ownership epoch this node last activated under.
        epoch: Epoch,
        /// The node that should have been asked, once anything can say
        /// (ADR-0026). Always `None` from this build.
        owner: Option<NodeId>,
    },
}

/// Why a lease was withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpireReason {
    /// The holder did not renew before registrar expiry.
    Timeout,
    /// The holder's gateway session disconnected.
    Disconnect,
    /// The registrar revoked the lease.
    Revoked,
    /// The entity was deliberately parked.
    Parked,
}

/// Resulting location of authority after expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpireDisposition {
    /// Authority transferred to another node.
    Reassigned {
        /// Node receiving authority.
        to: NodeId,
    },
    /// The entity remains persistent without a live holder.
    Parked,
    /// The entity is available for a future claimant.
    Free,
}

/// Typed authority traffic carried by the gateway reliable-control surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseMsg {
    /// Request a registrar-arbitrated lease.
    Claim {
        /// Client-generated identifier echoed by the registrar reply.
        claim_id: ClaimId,
        /// Persistent entity to claim.
        entity: PersistId,
        /// Grid containing `cell`.
        grid: GridId,
        /// Cell that currently contains the entity.
        cell: CellId,
        /// Requested authority tier.
        kind: ClaimKind,
        /// Evidence supporting this request.
        basis: ClaimBasis,
        /// Sequence pair observed by the claimant.
        observed: SeqPair,
        /// Universe tick at which the request was formed.
        tick: Tick,
    },
    /// Registrar grant. `ttl_ms` is a duration, not a peer-clock timestamp.
    Grant {
        /// Claim request which produced this grant.
        claim_id: ClaimId,
        /// Persistent entity granted to the requester.
        entity: PersistId,
        /// Newly issued fencing token.
        lease_id: LeaseId,
        /// Authoritative sequence pair after the grant.
        seq: SeqPair,
        /// Lease duration in registrar-monotonic milliseconds.
        ttl_ms: u32,
        /// Previous holder, when the grant replaced one.
        prev_holder: Option<NodeId>,
    },
    /// Definitive claim refusal.
    Deny {
        /// Claim request which was refused, or `None` for non-claim rejection.
        claim_id: Option<ClaimId>,
        /// Persistent entity whose claim was refused.
        entity: PersistId,
        /// Registrar reason for refusing the claim.
        reason: DenyReason,
        /// Minimum registrar-monotonic delay before retrying.
        retry_after_ms: u32,
    },
    /// Current holder's cooperative handoff acknowledgement.
    Divest {
        /// Persistent entity being handed off or released.
        entity: PersistId,
        /// Fencing token being divested.
        lease_id: LeaseId,
        /// Successor holder, or `None` when releasing the entity.
        to: Option<NodeId>,
        /// Holder's final authoritative sequence pair.
        final_seq: SeqPair,
        /// Last journal position acknowledged by the holder.
        cursor: Option<Lsn>,
    },
    /// Batched holder renewal.
    Heartbeat {
        /// Entity/token pairs the holder asks to renew. The entity is part of
        /// the name because `LeaseId` is a *per-row* counter (`lease.rs`
        /// increments `row.lease_id` on acquire), so `LeaseId(1)` identifies
        /// nothing on its own: every freshly claimed entity carries it. A
        /// registrar given bare ids has to search its whole session set per
        /// requested id, which is quadratic in the entities one peer holds.
        renew: Vec<(PersistId, LeaseId)>,
        /// Holder's current universe tick.
        tick: Tick,
    },
    /// Registrar response to a batched renewal. Rows include both successful
    /// renewals and the current state for stale ids; `invalid` explicitly
    /// identifies renewals that were refused so clients stop writing promptly.
    HeartbeatAck {
        /// Current rows returned for the entities named in the renewal.
        leases: Vec<Lease>,
        /// Renewals which were absent, expired, wrong-holder, or stale. Keyed
        /// by entity for the same reason as [`LeaseMsg::Heartbeat::renew`]: a
        /// bare id would invalidate every other entity the holder happens to
        /// hold at the same per-row counter value.
        invalid: Vec<(PersistId, LeaseId)>,
    },
    /// Expiry or revocation notification.
    Expire {
        /// Persistent entity whose lease ended.
        entity: PersistId,
        /// Fencing token that is no longer current.
        lease_id: LeaseId,
        /// Holder immediately before expiry, if any.
        last_holder: Option<NodeId>,
        /// Event that ended the lease.
        reason: ExpireReason,
        /// Authority state after expiry.
        disposition: ExpireDisposition,
    },
    /// Commit an entity's cell location without changing its holder or token.
    Rekey {
        /// Persistent entity whose location changed.
        entity: PersistId,
        /// Previously committed cell.
        old_cell: CellId,
        /// Newly committed cell.
        new_cell: CellId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_ordering_is_ownership_first() {
        assert!(
            SeqPair {
                own_seq: 1,
                auth_seq: 0
            } > SeqPair {
                own_seq: 0,
                auth_seq: u32::MAX
            }
        );
        assert!(SeqPair {
            own_seq: 1,
            auth_seq: 2
        }
        .supersedes(SeqPair {
            own_seq: 1,
            auth_seq: 1
        }));
    }

    #[test]
    fn lease_message_roundtrips() {
        let message = LeaseMsg::Heartbeat {
            renew: vec![
                (PersistId::new(4), LeaseId(7)),
                (PersistId::new(9), LeaseId(1)),
            ],
            tick: Tick::new(12),
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<LeaseMsg>(&bytes).unwrap(), message);
    }

    /// The wrong-owner refusal decodes under the current `PROTOCOL_VERSION`,
    /// and appending it left every older variant on the byte it already had.
    ///
    /// Postcard keys enum variants **positionally**, and this test is what
    /// keeps that honest: appending a variant leaves every older one on the
    /// byte it already had, and a *reordering* would break them silently and
    /// in both directions.
    ///
    /// The `{V, V−1}` acceptance window this argument used to serve is closed
    /// (D29 clause 5), so no peer decodes these bytes under an older version
    /// any more. The assertion stays for the other reader it always had: a
    /// stored value. Rows written by an earlier build are still decoded by
    /// this one, and a reordering would misread them with no version
    /// negotiation anywhere to catch it.
    #[test]
    fn the_wrong_owner_refusal_decodes_without_moving_the_variants_before_it() {
        assert_eq!(
            crate::PROTOCOL_VERSION,
            2,
            "this test pins the wire under a specific version; re-check it when the version moves"
        );

        let wrong_owner = DenyReason::WrongOwner {
            grid: GridId::ROOT,
            shard: CellId::ROOT,
            epoch: Epoch(7),
            owner: None,
        };
        let bytes = postcard::to_stdvec(&wrong_owner).unwrap();
        assert_eq!(
            bytes[0], 5,
            "WrongOwner must be appended after Parked, never inserted"
        );
        assert_eq!(
            postcard::from_bytes::<DenyReason>(&bytes).unwrap(),
            wrong_owner
        );

        // Carried on the message a claimant actually reads it from.
        let message = LeaseMsg::Deny {
            claim_id: Some(ClaimId(3)),
            entity: PersistId::new(11),
            reason: wrong_owner,
            retry_after_ms: 0,
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<LeaseMsg>(&bytes).unwrap(), message);

        // The V−1 half: every frame a peer built before this variant existed
        // still decodes to exactly what it encoded.
        let previously_encodable = [
            DenyReason::Held {
                holder: {
                    let mut seed = [0u8; 32];
                    seed[0] = 7;
                    iroh_base::SecretKey::from_bytes(&seed).public()
                },
                seq: SeqPair {
                    own_seq: 2,
                    auth_seq: 3,
                },
            },
            DenyReason::StrongHeld,
            DenyReason::NotEligible,
            DenyReason::RateLimited,
            DenyReason::Parked,
        ];
        for (discriminant, reason) in previously_encodable.into_iter().enumerate() {
            let bytes = postcard::to_stdvec(&reason).unwrap();
            assert_eq!(
                usize::from(bytes[0]),
                discriminant,
                "a V−1 peer's {reason:?} moved on the wire"
            );
            assert_eq!(postcard::from_bytes::<DenyReason>(&bytes).unwrap(), reason);
        }
    }

    /// The bulk-NACK reason codes are distinct and stable.
    ///
    /// They ride a `u16` field that already existed, so there is no
    /// discriminant to protect — what has to hold is that the not-owned code
    /// is not one of the two a V−1 gateway already emits, because a peer
    /// classifying on it would otherwise revoke a lease on an ordinary
    /// journal failure.
    #[test]
    fn the_bulk_nack_reason_codes_do_not_collide() {
        use crate::{BULK_NACK_JOURNAL, BULK_NACK_REFUSED, BULK_NACK_WRONG_OWNER};
        assert_eq!((BULK_NACK_JOURNAL, BULK_NACK_REFUSED), (1, 2));
        assert_eq!(BULK_NACK_WRONG_OWNER, 3);
        assert_ne!(BULK_NACK_WRONG_OWNER, BULK_NACK_JOURNAL);
        assert_ne!(BULK_NACK_WRONG_OWNER, BULK_NACK_REFUSED);
        assert_eq!(crate::AREA_LOAD_ERR_WRONG_OWNER, 3);
        assert_ne!(crate::AREA_LOAD_ERR_WRONG_OWNER, crate::AREA_LOAD_ERR_LIVE);
        assert_ne!(crate::AREA_LOAD_ERR_WRONG_OWNER, crate::AREA_LOAD_ERR_COLD);
    }

    #[test]
    fn correlated_claim_reply_roundtrips_and_rejects_truncation() {
        let message = LeaseMsg::Deny {
            claim_id: Some(ClaimId(17)),
            entity: PersistId::new(4),
            reason: DenyReason::NotEligible,
            retry_after_ms: 25,
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<LeaseMsg>(&bytes).unwrap(), message);
        assert!(postcard::from_bytes::<LeaseMsg>(&bytes[..bytes.len() - 1]).is_err());
    }
}
