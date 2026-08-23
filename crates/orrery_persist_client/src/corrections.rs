//! Verified gateway authority corrections awaiting prediction reconciliation.

use std::collections::VecDeque;

use bevy_ecs::prelude::*;
use orrery_protocol::{AuthorityCorrectionClaimsV1, AuthorityCorrectionV1, NodeId};

/// Maximum verified corrections retained while the prediction bridge catches up.
pub const AUTHORITY_CORRECTION_QUEUE_CAPACITY: usize = 64;

/// Corrections whose signature verified under the gateway this client dialled.
#[derive(Debug, Resource)]
pub struct AuthorityCorrectionQueue {
    pending: VecDeque<AuthorityCorrectionClaimsV1>,
    accepted: u64,
    rejected: u64,
    dropped: u64,
}

impl Default for AuthorityCorrectionQueue {
    fn default() -> Self {
        Self {
            pending: VecDeque::with_capacity(AUTHORITY_CORRECTION_QUEUE_CAPACITY),
            accepted: 0,
            rejected: 0,
            dropped: 0,
        }
    }
}

impl AuthorityCorrectionQueue {
    /// Verify and enqueue one correction from `expected_gateway`.
    pub fn accept(&mut self, correction: AuthorityCorrectionV1, expected_gateway: NodeId) -> bool {
        if correction.verify(expected_gateway).is_err() {
            self.rejected = self.rejected.saturating_add(1);
            return false;
        }
        if self.pending.len() == AUTHORITY_CORRECTION_QUEUE_CAPACITY {
            self.pending.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.pending.push_back(correction.claims);
        self.accepted = self.accepted.saturating_add(1);
        true
    }

    /// Take the oldest verified correction.
    pub fn pop(&mut self) -> Option<AuthorityCorrectionClaimsV1> {
        self.pending.pop_front()
    }

    /// Verified corrections waiting for reconciliation.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether no correction is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// `(accepted, rejected, dropped)` cumulative totals.
    #[must_use]
    pub const fn counters(&self) -> (u64, u64, u64) {
        (self.accepted, self.rejected, self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{PersistId, RulesetId, Tick};

    fn correction(key: &iroh_base::SecretKey) -> AuthorityCorrectionV1 {
        AuthorityCorrectionV1::sign(
            AuthorityCorrectionClaimsV1 {
                issuer: key.public(),
                subject: iroh_base::SecretKey::from_bytes(&[2; 32]).public(),
                entity: PersistId::new(1),
                reconcile_from: Tick::new(10),
                authoritative_tick: Tick::new(12),
                authoritative_state: vec![7],
                ruleset: RulesetId {
                    version: 1,
                    digest: [3; 32],
                },
                adjudication: [4; 32],
            },
            key,
        )
    }

    #[test]
    fn a_peer_refuses_any_signer_but_its_pinned_gateway() {
        let gateway = iroh_base::SecretKey::from_bytes(&[1; 32]);
        let stranger = iroh_base::SecretKey::from_bytes(&[9; 32]);
        let mut queue = AuthorityCorrectionQueue::default();

        assert!(!queue.accept(correction(&stranger), gateway.public()));
        assert!(queue.is_empty());
        assert_eq!(queue.counters(), (0, 1, 0));

        assert!(queue.accept(correction(&gateway), gateway.public()));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.counters(), (1, 1, 0));
    }
}
