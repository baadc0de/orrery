//! In-memory lease registrar state machine (D7).
//!
//! A cell actor owns one instance in the integrated runtime. Keeping the CAS
//! rules in this small type makes the invariant testable independently of the
//! journal and transport.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use async_trait::async_trait;

use orrery_protocol::{
    CellId, ClaimKind, DenyReason, GridId, Lease, LeaseFlags, LeaseId, NodeId, PersistId, SeqPair,
};

#[cfg(feature = "fdb")]
pub mod fdb;
#[cfg(feature = "fdb")]
pub use fdb::FdbLeaseStore;

/// Registrar lease duration in registrar-monotonic milliseconds.
pub const LEASE_TTL_MS: u64 = 10_000;

/// Return milliseconds from the process-local monotonic registrar clock.
///
/// Lease expiry is deliberately never based on peer timestamps or wall clock.
/// Every actor and gateway path uses this one source, so a restored TTL and a
/// subsequent heartbeat or sweep share the same epoch.
pub(crate) fn registrar_now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Failure from the durable lease tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseStoreError(pub String);

impl core::fmt::Display for LeaseStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}
impl core::error::Error for LeaseStoreError {}

/// Outcome of conditionally persisting a lease row at a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeasePut {
    /// The row was persisted at its existing or newly established location.
    Stored,
    /// Another actor already committed this entity to a different cell.
    LocationConflict(CellId),
}

/// Outcome of conditionally migrating a lease's durable cell indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseMigrate {
    /// The row and its indexes moved to the destination.
    Migrated,
    /// The entity is not committed to the caller's expected source.
    SourceMismatch {
        /// Current committed source, or `None` when the entity is absent.
        actual: Option<CellId>,
    },
    /// The durable row has a different fencing token.
    LeaseIdMismatch {
        /// Current durable fencing token.
        actual: LeaseId,
    },
    /// Durable row and index state disagree, so migration cannot proceed.
    IndexConflict,
}

/// Durable lease rows plus their entity-cell index.  The actor writes a row
/// before making the corresponding transition visible in its hot registrar.
#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// Load rows whose recorded location is in `shard`'s subtree.
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError>;
    /// Atomically write a lease row and its cell-location indexes.
    ///
    /// A different existing location is never overwritten; the caller must
    /// route the request to that actor instead.
    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError>;
    /// Locate an entity's committed cell without scanning every shard index.
    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError>;
    /// Atomically move an exact source row and fencing token to another cell.
    async fn migrate(
        &self,
        grid: GridId,
        entity: PersistId,
        from: CellId,
        to: CellId,
        expected_lease_id: LeaseId,
    ) -> Result<LeaseMigrate, LeaseStoreError> {
        let _ = (grid, entity, from, to, expected_lease_id);
        Err(LeaseStoreError("lease migration is unsupported".into()))
    }
    /// Atomically delete a lease row and its cell-location index.
    async fn remove(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError>;
}

/// Test-only in-process lease store.
#[derive(Debug, Default)]
pub struct MemLeaseStore {
    rows: Mutex<HashMap<(GridId, PersistId), (CellId, Lease)>>,
}
impl MemLeaseStore {
    /// Create an empty in-process lease store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}
#[async_trait]
impl LeaseStore for MemLeaseStore {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        Ok(self
            .rows
            .lock()
            .expect("mem lease lock")
            .iter()
            .filter_map(|((g, _), (cell, row))| {
                (*g == grid && shard.is_prefix_of(*cell)).then(|| (*cell, row.clone()))
            })
            .collect())
    }
    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        let mut rows = self.rows.lock().expect("mem lease lock");
        if let Some((committed, _)) = rows.get(&(grid, lease.entity)) {
            if *committed != cell {
                return Ok(LeasePut::LocationConflict(*committed));
            }
        }
        rows.insert((grid, lease.entity), (cell, lease.clone()));
        Ok(LeasePut::Stored)
    }
    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        Ok(self
            .rows
            .lock()
            .expect("mem lease lock")
            .get(&(grid, entity))
            .map(|(cell, _)| *cell))
    }
    async fn migrate(
        &self,
        grid: GridId,
        entity: PersistId,
        from: CellId,
        to: CellId,
        expected_lease_id: LeaseId,
    ) -> Result<LeaseMigrate, LeaseStoreError> {
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| LeaseStoreError("mem lease lock poisoned".into()))?;
        let Some((cell, row)) = rows.get_mut(&(grid, entity)) else {
            return Ok(LeaseMigrate::SourceMismatch { actual: None });
        };
        if *cell != from {
            return Ok(LeaseMigrate::SourceMismatch {
                actual: Some(*cell),
            });
        }
        if row.lease_id != expected_lease_id {
            return Ok(LeaseMigrate::LeaseIdMismatch {
                actual: row.lease_id,
            });
        }
        *cell = to;
        Ok(LeaseMigrate::Migrated)
    }
    async fn remove(
        &self,
        grid: GridId,
        _cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        self.rows
            .lock()
            .expect("mem lease lock")
            .remove(&(grid, entity));
        Ok(())
    }
}

/// Outcome of a serialized claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    /// Claim won and received a freshly advanced fencing token.
    Granted(Lease),
    /// Claim lost without mutating the row.
    Denied(DenyReason),
}

/// Entity-keyed, single-writer authority registrar.
#[derive(Debug, Clone, Default)]
pub struct LeaseRegistrar {
    leases: HashMap<PersistId, Lease>,
}

impl LeaseRegistrar {
    /// Restore one durable row without changing its token or sequences.
    pub fn restore(&mut self, lease: Lease) {
        self.leases.insert(lease.entity, lease);
    }

    pub(crate) fn remove(&mut self, entity: PersistId) {
        self.leases.remove(&entity);
    }

    /// Clone the row for an id. Kept owned so an actor can release registrar
    /// borrows before awaiting its durable store.
    #[must_use]
    pub fn current(&self, entity: PersistId) -> Option<Lease> {
        self.leases.get(&entity).cloned()
    }

    /// Find a row by session-facing lease id.
    #[must_use]
    pub fn by_lease_id(&self, lease_id: LeaseId) -> Option<Lease> {
        self.leases
            .values()
            .find(|row| row.lease_id == lease_id)
            .cloned()
    }
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
        if let Some(holder) = row.holder {
            if row.expires_at > now_ms {
                if holder == claimant {
                    return ClaimResult::Granted(row.clone());
                }
                if row.flags.contains(LeaseFlags::STRONG_HELD) {
                    return ClaimResult::Denied(DenyReason::StrongHeld);
                }
                if matches!(kind, ClaimKind::Strong) {
                    return ClaimResult::Denied(DenyReason::Held {
                        holder,
                        seq: row.seq,
                    });
                }
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

    #[test]
    fn current_holder_unexpired_claim_preserves_existing_lease() {
        let mut registrar = LeaseRegistrar::default();
        let holder = node(1);
        let entity = PersistId::new(9);
        let ClaimResult::Granted(first) = registrar.claim(entity, holder, ClaimKind::Weak, 0)
        else {
            panic!("initial weak claim must be granted");
        };

        let ClaimResult::Granted(repeated) = registrar.claim(entity, holder, ClaimKind::Weak, 1)
        else {
            panic!("current unexpired holder must retain its lease");
        };

        assert_eq!(repeated, first);
        assert_eq!(registrar.current(entity), Some(first));
    }

    #[test]
    fn source_retirement_removes_only_the_selected_registrar_row() {
        let mut registrar = LeaseRegistrar::default();
        let first = Lease {
            entity: PersistId::new(10),
            holder: Some(node(1)),
            seq: SeqPair {
                own_seq: 2,
                auth_seq: 3,
            },
            lease_id: LeaseId(4),
            expires_at: 5,
            flags: LeaseFlags::STRONG_HELD,
        };
        let second = Lease {
            entity: PersistId::new(11),
            ..first.clone()
        };
        registrar.restore(first.clone());
        registrar.restore(second.clone());

        registrar.remove(first.entity);

        assert_eq!(registrar.current(first.entity), None);
        assert_eq!(registrar.current(second.entity), Some(second));
    }
}
