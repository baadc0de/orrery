//! In-memory lease registrar state machine (D7).
//!
//! A cell actor owns one instance in the integrated runtime. Keeping the CAS
//! rules in this small type makes the invariant testable independently of the
//! journal and transport.

use std::collections::HashMap;

use orrery_protocol::{
    ClaimKind, DenyReason, Lease, LeaseFlags, LeaseId, NodeId, PersistId, SeqPair,
};

/// Registrar lease duration in registrar-monotonic milliseconds.
pub const LEASE_TTL_MS: u64 = 10_000;

/// Outcome of a serialized claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    /// Claim won and received a freshly advanced fencing token.
    Granted(Lease),
    /// Claim lost without mutating the row.
    Denied(DenyReason),
}

/// Entity-keyed, single-writer authority registrar.
#[derive(Debug, Default)]
pub struct LeaseRegistrar {
    leases: HashMap<PersistId, Lease>,
}

impl LeaseRegistrar {
    /// Return the current lease row.
    #[must_use]
    pub fn get(&self, entity: PersistId) -> Option<&Lease> {
        self.leases.get(&entity)
    }

    /// Claim a lease at registrar time `now_ms`.
    ///
    /// Weak claims may replace another weak holder. Strong claims are only
    /// granted when free; a strong-held row is never stolen.
    pub fn claim(
        &mut self,
        entity: PersistId,
        claimant: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> ClaimResult {
        let row = self.leases.entry(entity).or_insert_with(|| Lease {
            entity,
            holder: None,
            seq: SeqPair::default(),
            lease_id: LeaseId(0),
            expires_at: 0,
            flags: LeaseFlags::PARKED,
        });
        if row.holder.is_some() && row.expires_at > now_ms {
            if row.holder == Some(claimant) {
                return ClaimResult::Granted(row.clone());
            }
            if row.flags.contains(LeaseFlags::STRONG_HELD) {
                return ClaimResult::Denied(DenyReason::StrongHeld);
            }
            if matches!(kind, ClaimKind::Strong) {
                return ClaimResult::Denied(DenyReason::Held {
                    holder: row.holder.expect("checked"),
                    seq: row.seq,
                });
            }
        }
        if matches!(kind, ClaimKind::Weak) {
            row.seq.auth_seq = row.seq.auth_seq.saturating_add(1);
        } else {
            row.seq.own_seq = row.seq.own_seq.saturating_add(1);
        }
        row.lease_id.0 = row.lease_id.0.saturating_add(1);
        row.holder = Some(claimant);
        row.expires_at = now_ms.saturating_add(LEASE_TTL_MS);
        row.flags.set(LeaseFlags::PARKED, false);
        row.flags
            .set(LeaseFlags::STRONG_HELD, matches!(kind, ClaimKind::Strong));
        ClaimResult::Granted(row.clone())
    }

    /// Renew a holder's current token. Heartbeats never change a durable
    /// sequence or token, and therefore do not need a durable transition.
    pub fn heartbeat(
        &mut self,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> bool {
        let Some(row) = self.leases.get_mut(&entity) else {
            return false;
        };
        if row.holder == Some(holder) && row.lease_id == lease_id && row.expires_at > now_ms {
            row.expires_at = now_ms.saturating_add(LEASE_TTL_MS);
            true
        } else {
            false
        }
    }

    /// Check the exact current fencing token before admitting a bulk write.
    #[must_use]
    pub fn admits_write(
        &self,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> bool {
        self.leases.get(&entity).is_some_and(|row| {
            row.holder == Some(holder) && row.lease_id == lease_id && row.expires_at > now_ms
        })
    }

    /// Park every lease owned by a disconnected session immediately.
    pub fn disconnect(&mut self, holder: NodeId) -> Vec<Lease> {
        self.leases
            .values_mut()
            .filter_map(|row| {
                (row.holder == Some(holder)).then(|| {
                    row.holder = None;
                    row.expires_at = 0;
                    row.lease_id.0 = row.lease_id.0.saturating_add(1);
                    row.flags.set(LeaseFlags::PARKED, true);
                    row.clone()
                })
            })
            .collect()
    }

    /// Park silent holders whose registrar-clock TTL elapsed.
    pub fn sweep_expired(&mut self, now_ms: u64) -> Vec<Lease> {
        self.leases
            .values_mut()
            .filter_map(|row| {
                (row.holder.is_some() && row.expires_at <= now_ms).then(|| {
                    row.holder = None;
                    row.lease_id.0 = row.lease_id.0.saturating_add(1);
                    row.flags.set(LeaseFlags::PARKED, true);
                    row.clone()
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(n: u8) -> NodeId {
        let mut seed = [0; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn stale_token_never_writes_after_transfer_or_expiry() {
        let mut registrar = LeaseRegistrar::default();
        let a = node(1);
        let b = node(2);
        let entity = PersistId::new(9);
        let ClaimResult::Granted(first) = registrar.claim(entity, a, ClaimKind::Weak, 0) else {
            panic!()
        };
        let ClaimResult::Granted(second) = registrar.claim(entity, b, ClaimKind::Weak, 1) else {
            panic!()
        };
        assert!(!registrar.admits_write(entity, a, first.lease_id, 1));
        assert!(registrar.admits_write(entity, b, second.lease_id, 1));
        registrar.sweep_expired(LEASE_TTL_MS + 2);
        assert!(!registrar.admits_write(entity, b, second.lease_id, LEASE_TTL_MS + 2));
    }

    #[test]
    fn strong_ownership_is_not_stealable_but_disconnect_parks_it() {
        let mut registrar = LeaseRegistrar::default();
        let a = node(1);
        let b = node(2);
        let entity = PersistId::new(9);
        assert!(matches!(
            registrar.claim(entity, a, ClaimKind::Strong, 0),
            ClaimResult::Granted(_)
        ));
        assert_eq!(
            registrar.claim(entity, b, ClaimKind::Weak, 1),
            ClaimResult::Denied(DenyReason::StrongHeld)
        );
        assert_eq!(registrar.disconnect(a).len(), 1);
        assert!(registrar
            .get(entity)
            .unwrap()
            .flags
            .contains(LeaseFlags::PARKED));
    }
}
