//! Authority lease wire types (D7).

use core::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::{CellId, GridId, Lsn, NodeId, PersistId, Tick};

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

/// Compact lease flags. This is deliberately a numeric bitset so new flags are
/// wire-compatible with older decoders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LeaseFlags(pub u8);

impl LeaseFlags {
    /// Lease belongs permanently to a player character.
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
        /// Fencing tokens the holder asks to renew.
        lease_ids: Vec<LeaseId>,
        /// Holder's current universe tick.
        tick: Tick,
    },
    /// Registrar response to a batched renewal. Rows include both successful
    /// renewals and the current state for stale ids; `invalid` explicitly
    /// identifies IDs that were not renewed so clients stop writing promptly.
    HeartbeatAck {
        /// Current rows returned for session-indexed IDs.
        leases: Vec<Lease>,
        /// IDs which were absent, expired, wrong-holder, or stale.
        invalid: Vec<LeaseId>,
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
            lease_ids: vec![LeaseId(7), LeaseId(9)],
            tick: Tick::new(12),
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<LeaseMsg>(&bytes).unwrap(), message);
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
