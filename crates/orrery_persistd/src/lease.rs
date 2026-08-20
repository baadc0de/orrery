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
    CellId, ClaimKind, DenyReason, ExpireReason, GridId, Lease, LeaseFlags, LeaseId, NodeId,
    PersistId, SeqPair,
};

#[cfg(feature = "fdb")]
pub mod fdb;
pub mod stages;
#[cfg(feature = "fdb")]
pub use fdb::FdbLeaseStore;

/// Registrar lease duration in registrar-monotonic milliseconds.
pub const LEASE_TTL_MS: u64 = 10_000;

/// How long after an unpark the registrar refuses a *competing* weak claim.
///
/// A park — a TTL sweep or a disconnect — makes an entity claimable for
/// everyone who can see it at the same instant, so the claims that follow are
/// a herd, not a gameplay steal. The first one legitimately wins. Without a
/// damper the rest are granted too, in turn, each bumping the token the last
/// winner just installed, and none of the losers is ever told: the registrar
/// serves the herd instead of arbitrating it. That is also why broadcasting
/// lease expiry to interested peers was rejected for P3 — it manufactures the
/// herd and there was nothing here to absorb it.
///
/// The window is the 9-tick / 150 ms rollback budget (D8, docs/04-authority.md
/// §4.1): a loser told inside it absorbs its optimistic claim through the
/// ordinary rollback path, and the deny names the winner and its `seq` so the
/// loser reconciles to the right stream. A claim arriving *after* the window
/// is a real interaction against a peer that is visibly simulating the entity,
/// and still takes it under INV-4 — the damper bounds the race, it does not
/// make weak authority sticky.
pub const CLAIM_HERD_DAMPER_MS: u64 = 150;

/// How long a parked strong-owned row stays reserved for the owner it lost.
///
/// D7 §4.3 says a strong-owned entity whose owner crashed is never regranted
/// without consent, and §7 says the first claim on a parked entity unparks it.
/// Both are right, at different timescales: the crashed owner is usually back
/// within seconds, and handing its entity to whoever walks past in the meantime
/// is the theft strong ownership exists to prevent — but reserving it *forever*
/// for a node key that may never return strands the entity, since nothing in
/// the protocol releases it. Permanent reservation is what `PLAYER_BOUND` is
/// for, and that flag is checked first and without a deadline.
///
/// Three lease TTLs: comfortably past the < 30 s worst case for detecting a
/// zombie holder (§4.3), so the owner's reconnect is not racing its own
/// expiry, and short enough that a lost prop rejoins the world.
pub const STRONG_PARK_GRACE_MS: u64 = 3 * LEASE_TTL_MS;

/// Retry advice the registrar attaches to a `Deny`, in milliseconds.
///
/// Only the two arbitration outcomes are worth retrying on a timer: a herd
/// loser should come back once the damper window is over, and a parked
/// strong-owned row only becomes claimable when its owner consents, which no
/// timer predicts — so it gets the same modest backoff rather than a
/// fabricated one. Everything else is a decision, not a race, and retrying it
/// on a schedule is just load.
#[must_use]
pub fn retry_after_ms(reason: &DenyReason) -> u32 {
    match reason {
        DenyReason::Held { .. } | DenyReason::Parked => CLAIM_HERD_DAMPER_MS as u32,
        DenyReason::StrongHeld | DenyReason::NotEligible | DenyReason::RateLimited => 0,
        // Not a race and not a decision either: the claim went to a node that
        // hosts no shard over the cell. Waiting changes nothing about that,
        // and advising a wait would invite exactly the wrong response — the
        // claimant has to re-address, not re-time.
        DenyReason::WrongOwner { .. } => 0,
    }
}

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
    /// Registrar-clock time each row was last unparked, for the claim-herd
    /// damper. Hot state only: it is an arbitration aid for the milliseconds
    /// around a park, and an actor that restarts mid-herd simply arbitrates
    /// the tail of that herd the old way rather than reviving a window whose
    /// clock epoch it no longer shares. Entries are dropped when the row parks
    /// or is retired, so this never outgrows `leases`.
    unparked_at: HashMap<PersistId, u64>,
}

/// Park one row in place: drop the holder, fence the token it was using, and
/// record whose consent a strong-owned row now waits for.
///
/// Parking is where a strong owner's identity would otherwise be lost —
/// `holder` is the only place it lives while the lease is live — so the park
/// moves it into `bound_to`, which is durable and already means "the identity
/// this row is reserved for". An existing binding (a player character) is
/// never overwritten: it is the stronger claim of the two.
fn park(row: &mut Lease, unparked_at: &mut HashMap<PersistId, u64>, now_ms: u64) {
    let reserved = row.flags.contains(LeaseFlags::STRONG_HELD);
    if reserved && row.bound_to.is_none() {
        row.bound_to = row.holder;
    }
    row.holder = None;
    row.lease_id.0 = row.lease_id.0.saturating_add(1);
    row.flags.set(LeaseFlags::PARKED, true);
    // A parked row's `expires_at` is dead data — every path that reads it
    // first requires a holder — so the reservation deadline reuses it rather
    // than widening the durable row. It is a registrar-clock instant like any
    // other, which is exactly why actor recovery re-arms it (`actor.rs`)
    // instead of trusting a value minted in a previous process's epoch.
    row.expires_at = if reserved {
        now_ms.saturating_add(STRONG_PARK_GRACE_MS)
    } else {
        0
    };
    unparked_at.remove(&row.entity);
}

impl LeaseRegistrar {
    /// Restore one durable row without changing its token or sequences.
    pub fn restore(&mut self, lease: Lease) {
        self.leases.insert(lease.entity, lease);
    }

    /// Mark an entity as one identity's character (D7 §4.3).
    ///
    /// From here on the row is claimable only by `owner`, is never offered to
    /// a successor, and keeps both properties while parked — that is the whole
    /// point: a player's character must still be theirs after they disconnect.
    /// Returns `false` when the entity has no row yet.
    pub fn bind_to_player(&mut self, entity: PersistId, owner: NodeId) -> bool {
        let Some(row) = self.leases.get_mut(&entity) else {
            return false;
        };
        row.flags.set(LeaseFlags::PLAYER_BOUND, true);
        row.bound_to = Some(owner);
        true
    }

    pub(crate) fn remove(&mut self, entity: PersistId) {
        self.leases.remove(&entity);
        self.unparked_at.remove(&entity);
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
        let unparked_at = self.unparked_at.get(&entity).copied();
        let row = self.leases.entry(entity).or_insert_with(|| Lease {
            entity,
            holder: None,
            seq: SeqPair::default(),
            lease_id: LeaseId(0),
            expires_at: 0,
            flags: LeaseFlags::PARKED,
            bound_to: None,
        });
        // A player's character is theirs whether they are here or not: parking
        // it on disconnect must not make it claimable by whoever walks past
        // (D7 §4.3). This is checked before the holder/expiry rules, because
        // the whole point is that it holds while the row is parked.
        if row.flags.contains(LeaseFlags::PLAYER_BOUND)
            && row.bound_to.is_some_and(|bound| bound != claimant)
        {
            return ClaimResult::Denied(DenyReason::NotEligible);
        }
        // Strong ownership outlives the park that interrupted it (D7 §4.3,
        // and the grid of dispositions in docs/04-authority.md §4.3: a
        // strong-owned entity whose owner crashed re-parks with `own_seq`
        // intact and is "never regranted without consent"). The gateway
        // already refuses to *push* such a row to a successor; without this
        // the registrar still handed it to whoever *pulled* — the check lived
        // inside `if let Some(holder)`, and parking is exactly what clears
        // `holder`. `bound_to` records whose consent the row is waiting for,
        // written by the park itself.
        if row.holder.is_none() && row.flags.contains(LeaseFlags::STRONG_HELD) {
            match row.bound_to {
                // Bounded by the park's reservation deadline: past it, §7's
                // "the first Claim by anyone unparks it" resumes, so a crashed
                // owner that never comes back cannot strand the entity.
                Some(owner) if owner != claimant && row.expires_at > now_ms => {
                    return ClaimResult::Denied(DenyReason::Parked);
                }
                // A row parked before this rule existed names no owner, so
                // there is nobody to ask and nobody to refuse on behalf of.
                // Denying everyone would strand the entity forever, which is
                // worse than the regrant it used to get: fall through.
                None | Some(_) => {}
            }
        }
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
                // The claim-herd damper. Inside the window after an unpark
                // this weak claim is not a gameplay steal, it is the rest of
                // the herd that saw the same parked row: tell it who won and
                // with which pair, so it rolls its optimistic claim back and
                // reconciles instead of installing a lease that the next
                // claimant in line is about to invalidate.
                if unparked_at.is_some_and(|at| now_ms.saturating_sub(at) < CLAIM_HERD_DAMPER_MS) {
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
        let was_parked = row.flags.contains(LeaseFlags::PARKED);
        row.flags.set(LeaseFlags::PARKED, false);
        if !row.flags.contains(LeaseFlags::PLAYER_BOUND) {
            // The reservation a park wrote is consumed by the grant that
            // answers it, so a later strong owner's park is free to record its
            // own. A player binding is permanent and is never cleared here.
            row.bound_to = None;
        }
        row.flags
            .set(LeaseFlags::STRONG_HELD, matches!(kind, ClaimKind::Strong));
        let granted = row.clone();
        // Only an *unpark* opens a herd window; a steal from a live holder is
        // one interaction against one peer and needs no damper.
        if was_parked {
            self.unparked_at.insert(entity, now_ms);
        }
        ClaimResult::Granted(granted)
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
    ///
    /// `now_ms` is the registrar clock the park is stamped with: a strong
    /// owner's row is reserved for it until `now_ms + STRONG_PARK_GRACE_MS`.
    pub fn disconnect(&mut self, holder: NodeId, now_ms: u64) -> Vec<Lease> {
        let unparked_at = &mut self.unparked_at;
        self.leases
            .values_mut()
            .filter_map(|row| {
                (row.holder == Some(holder)).then(|| {
                    park(row, unparked_at, now_ms);
                    row.clone()
                })
            })
            .collect()
    }

    /// Whether [`Self::sweep_expired`] would park anything at `now_ms`.
    ///
    /// Exactly `sweep_expired`'s own predicate, read-only and allocating
    /// nothing, so a caller that must copy the registrar to be able to abandon
    /// a half-applied sweep can find out first whether there is anything to
    /// apply. The two conditions are kept adjacent deliberately: a sweep that
    /// parked a row this said nothing about would be a sweep whose copy was
    /// skipped and whose durable write then had nothing to unwind to.
    /// `tests/lease_sweep_cost.rs` holds them equal over a state matrix.
    #[must_use]
    pub fn has_expired(&self, now_ms: u64) -> bool {
        self.leases
            .values()
            .any(|row| row.holder.is_some() && row.expires_at <= now_ms)
    }

    /// Whether [`Self::divest_all`] would park anything.
    ///
    /// `has_expired`'s predicate with the clock term dropped, for the same
    /// reason it exists: the caller copies the registrar so a half-applied
    /// pass can be abandoned, and a copy of a shard's whole registrar is the
    /// expensive part.
    #[must_use]
    pub fn has_holders(&self) -> bool {
        self.leases.values().any(|row| row.holder.is_some())
    }

    /// Park **every** held row, whatever its TTL says (D26 rule 3 step 4).
    ///
    /// [`Self::sweep_expired`] with the clock term dropped, and the difference
    /// is the whole point of a live handover: the rows being divested are
    /// *live*, their holders are heartbeating, and the drain exists precisely
    /// so they stop being live before the `actor/{grid}/{shard}` row moves to
    /// a node those holders have no session to. Each entry carries the holder
    /// and the token the row had before the park, because the `Expire` the
    /// caller then delivers has to be addressed by the token the holder still
    /// believes it has installed.
    ///
    /// `own_seq` and `auth_seq` are untouched — `park` moves `holder`,
    /// `lease_id`, `flags` and `expires_at` and nothing else — so the
    /// successor adopts the row at the sequence the outgoing owner left it at
    /// (D26 rule 3 step 3, docs/04-authority.md §9).
    pub fn divest_all(&mut self, now_ms: u64) -> Vec<ExpiredLease> {
        let unparked_at = &mut self.unparked_at;
        self.leases
            .values_mut()
            .filter_map(|row| {
                let previous_holder = row.holder?;
                let previous_lease_id = row.lease_id;
                park(row, unparked_at, now_ms);
                Some(ExpiredLease {
                    previous_holder,
                    previous_lease_id,
                    lease: row.clone(),
                })
            })
            .collect()
    }

    /// Park silent holders whose registrar-clock TTL elapsed.
    ///
    /// Each entry carries the holder and fencing token the row had **before**
    /// the park. A reassignment policy needs the former to rank successors and
    /// the latter to address the loser's `Expire`: the client only acts on an
    /// `Expire` naming the token it still has installed, and parking has
    /// already bumped the row's own `lease_id` past it.
    pub fn sweep_expired(&mut self, now_ms: u64) -> Vec<ExpiredLease> {
        let unparked_at = &mut self.unparked_at;
        self.leases
            .values_mut()
            .filter_map(|row| {
                let previous_holder = row.holder?;
                (row.expires_at <= now_ms).then(|| {
                    let previous_lease_id = row.lease_id;
                    park(row, unparked_at, now_ms);
                    ExpiredLease {
                        previous_holder,
                        previous_lease_id,
                        lease: row.clone(),
                    }
                })
            })
            .collect()
    }
}

/// One registrar row parked by a TTL sweep, with the identity it lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredLease {
    /// Holder immediately before the park.
    pub previous_holder: NodeId,
    /// Fencing token that holder still believes it has installed.
    pub previous_lease_id: LeaseId,
    /// The parked row, with its bumped token and `PARKED` flag.
    pub lease: Lease,
}

/// A lease that lost its holder, addressed well enough for a reassignment
/// policy to act on it without re-reading the registrar (D7 §5).
///
/// The gateway owns successor selection because only the gateway knows which
/// peers have live authenticated sessions; the actor owns the row. This is the
/// value that crosses between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedLease {
    /// Grid containing `cell`.
    pub grid: GridId,
    /// The entity's committed cell, as the durable lease index records it.
    pub cell: CellId,
    /// Holder immediately before the park.
    pub previous_holder: NodeId,
    /// Fencing token that holder still believes it has installed.
    pub previous_lease_id: LeaseId,
    /// The parked row.
    pub lease: Lease,
    /// What ended the lease.
    pub reason: ExpireReason,
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
        // Past the claim-herd window, so this is an ordinary weak steal from
        // a live holder rather than the tail of a herd: it is granted, which
        // is what makes the fencing question below meaningful.
        let steal_at = CLAIM_HERD_DAMPER_MS + 1;
        let ClaimResult::Granted(second) = registrar.claim(entity, b, ClaimKind::Weak, steal_at)
        else {
            panic!()
        };
        assert!(!registrar.admits_write(entity, a, first.lease_id, steal_at));
        assert!(registrar.admits_write(entity, b, second.lease_id, steal_at));
        let after_ttl = steal_at + LEASE_TTL_MS + 1;
        registrar.sweep_expired(after_ttl);
        assert!(!registrar.admits_write(entity, b, second.lease_id, after_ttl));
    }

    /// A claim herd is arbitrated, not served.
    ///
    /// A park makes the row claimable for everyone at once. The first claim
    /// legitimately wins; before the damper the rest were *also* granted, in
    /// turn, each bumping the token the previous winner had just installed —
    /// so every claimant but the last held a lease that was already dead, and
    /// none of them was told.
    #[test]
    fn a_second_claimant_in_the_herd_is_refused_not_handed_a_doomed_lease() {
        let mut registrar = LeaseRegistrar::default();
        let (winner, loser, straggler) = (node(1), node(2), node(3));
        let entity = PersistId::new(70);

        // Given: a row that a TTL sweep just parked, so it is claimable.
        assert!(matches!(
            registrar.claim(entity, node(9), ClaimKind::Weak, 0),
            ClaimResult::Granted(_)
        ));
        let parked_at = LEASE_TTL_MS + 1;
        assert_eq!(registrar.sweep_expired(parked_at).len(), 1);

        // When: the herd arrives.
        let ClaimResult::Granted(won) = registrar.claim(entity, winner, ClaimKind::Weak, parked_at)
        else {
            panic!("the first claimant unparks the row and wins");
        };
        let refused = registrar.claim(entity, loser, ClaimKind::Weak, parked_at + 1);

        // Then: the loser is told it lost, and by whom.
        assert_eq!(
            refused,
            ClaimResult::Denied(DenyReason::Held {
                holder: winner,
                seq: won.seq,
            })
        );
        // And the winner's lease is intact — the refusal cost it nothing.
        assert!(registrar.admits_write(entity, winner, won.lease_id, parked_at + 1));
        assert_eq!(registrar.current(entity), Some(won.clone()));

        // Every straggler in the same window gets the same answer rather than
        // a turn at the wheel.
        for arrival in 2..CLAIM_HERD_DAMPER_MS {
            assert!(matches!(
                registrar.claim(entity, straggler, ClaimKind::Weak, parked_at + arrival),
                ClaimResult::Denied(DenyReason::Held { .. })
            ));
        }
        assert!(registrar.admits_write(entity, winner, won.lease_id, parked_at + 1));

        // The damper bounds the herd; it does not make weak authority sticky.
        let ClaimResult::Granted(stolen) = registrar.claim(
            entity,
            straggler,
            ClaimKind::Weak,
            parked_at + CLAIM_HERD_DAMPER_MS,
        ) else {
            panic!("a claim past the window is an interaction, and still wins");
        };
        assert_eq!(stolen.holder, Some(straggler));
        assert_eq!(stolen.seq.auth_seq, won.seq.auth_seq + 1);
    }

    /// Strong ownership survives the park that interrupted it.
    ///
    /// The gateway already refuses to *push* a crashed strong owner's entity
    /// to a successor (D7 §5). The registrar used to hand it to whoever
    /// *pulled*: every holder check sat behind `if let Some(holder)`, and
    /// parking is precisely what clears `holder`.
    #[test]
    fn a_crashed_strong_owner_is_not_regranted_to_whoever_claims_next() {
        let mut registrar = LeaseRegistrar::default();
        let owner = node(1);
        let stranger = node(2);
        let entity = PersistId::new(71);
        let ClaimResult::Granted(owned) = registrar.claim(entity, owner, ClaimKind::Strong, 0)
        else {
            panic!("strong claim on a free entity is granted");
        };

        // When: the owner goes silent and the TTL sweep parks the row.
        let parked_at = LEASE_TTL_MS + 1;
        assert_eq!(registrar.sweep_expired(parked_at).len(), 1);

        // Then: nobody else can take it, at either tier — not now, and not
        // once the herd window has long passed.
        for at in [parked_at, parked_at + LEASE_TTL_MS] {
            assert_eq!(
                registrar.claim(entity, stranger, ClaimKind::Weak, at),
                ClaimResult::Denied(DenyReason::Parked)
            );
            assert_eq!(
                registrar.claim(entity, stranger, ClaimKind::Strong, at),
                ClaimResult::Denied(DenyReason::Parked)
            );
        }

        // And: the owner reclaims it, with `own_seq` carried through the park
        // rather than reset by someone else's grant.
        let ClaimResult::Granted(reclaimed) =
            registrar.claim(entity, owner, ClaimKind::Strong, parked_at + 1)
        else {
            panic!("the owner's own claim is the consent the row was waiting for");
        };
        assert_eq!(reclaimed.holder, Some(owner));
        assert_eq!(reclaimed.seq.own_seq, owned.seq.own_seq + 1);
        assert!(reclaimed.flags.contains(LeaseFlags::STRONG_HELD));
    }

    /// A disconnect parks strong ownership the same way a TTL lapse does.
    #[test]
    fn a_disconnected_strong_owner_keeps_its_entity_reserved() {
        let mut registrar = LeaseRegistrar::default();
        let owner = node(1);
        let stranger = node(2);
        let entity = PersistId::new(72);
        assert!(matches!(
            registrar.claim(entity, owner, ClaimKind::Strong, 0),
            ClaimResult::Granted(_)
        ));

        assert_eq!(registrar.disconnect(owner, 0).len(), 1);

        assert_eq!(
            registrar.claim(entity, stranger, ClaimKind::Weak, 1),
            ClaimResult::Denied(DenyReason::Parked)
        );
        assert!(matches!(
            registrar.claim(entity, owner, ClaimKind::Strong, 1),
            ClaimResult::Granted(_)
        ));
    }

    /// The reservation is a grace period, not a life sentence.
    ///
    /// An owner that never comes back must not take the entity with it: past
    /// the grace, §7's ordinary "the first Claim by anyone unparks it" rule
    /// resumes. A permanent hold is what `PLAYER_BOUND` is for, and the test
    /// above shows that one has no deadline.
    #[test]
    fn a_strong_reservation_expires_so_a_lost_owner_cannot_strand_the_entity() {
        let mut registrar = LeaseRegistrar::default();
        let owner = node(1);
        let stranger = node(2);
        let entity = PersistId::new(75);
        assert!(matches!(
            registrar.claim(entity, owner, ClaimKind::Strong, 0),
            ClaimResult::Granted(_)
        ));
        let parked_at = LEASE_TTL_MS + 1;
        assert_eq!(registrar.sweep_expired(parked_at).len(), 1);

        // Still reserved a moment before the grace runs out.
        assert_eq!(
            registrar.claim(
                entity,
                stranger,
                ClaimKind::Weak,
                parked_at + STRONG_PARK_GRACE_MS - 1
            ),
            ClaimResult::Denied(DenyReason::Parked)
        );

        // And claimable a moment after it.
        assert!(matches!(
            registrar.claim(
                entity,
                stranger,
                ClaimKind::Weak,
                parked_at + STRONG_PARK_GRACE_MS
            ),
            ClaimResult::Granted(_)
        ));
    }

    /// A row parked by an older build names no owner, and is not stranded.
    ///
    /// The reservation is written by the park, so a durable row that parked
    /// before this rule existed has nobody to ask and nobody to refuse on
    /// behalf of. Denying everyone would strand the entity for good — worse
    /// than the regrant it used to get — so it stays claimable. This pins that
    /// trade-off rather than leaving it to be rediscovered.
    #[test]
    fn a_strong_parked_row_with_no_recorded_owner_stays_claimable() {
        let mut registrar = LeaseRegistrar::default();
        let entity = PersistId::new(73);
        registrar.restore(Lease {
            entity,
            holder: None,
            seq: SeqPair {
                own_seq: 4,
                auth_seq: 0,
            },
            lease_id: LeaseId(6),
            expires_at: 0,
            flags: LeaseFlags(LeaseFlags::STRONG_HELD.0 | LeaseFlags::PARKED.0),
            bound_to: None,
        });

        assert!(matches!(
            registrar.claim(entity, node(2), ClaimKind::Weak, 1),
            ClaimResult::Granted(_)
        ));
    }

    /// The reservation a park writes is consumed by the grant that answers it.
    ///
    /// It is `bound_to`, the same durable field a player binding uses, so
    /// leaving a spent reservation behind would quietly reserve the row for an
    /// identity that no longer owns it. A player binding is a different claim
    /// on that field and is never cleared here — the test above proves the
    /// character still comes back to its account.
    #[test]
    fn a_reservation_does_not_outlive_the_grant_that_answered_it() {
        let mut registrar = LeaseRegistrar::default();
        let owner = node(1);
        let entity = PersistId::new(74);
        assert!(matches!(
            registrar.claim(entity, owner, ClaimKind::Strong, 0),
            ClaimResult::Granted(_)
        ));
        assert_eq!(registrar.disconnect(owner, 0).len(), 1);
        assert_eq!(
            registrar.get(entity).unwrap().bound_to,
            Some(owner),
            "the park records whose consent the row waits for"
        );

        let ClaimResult::Granted(reclaimed) = registrar.claim(entity, owner, ClaimKind::Strong, 1)
        else {
            panic!("the owner reclaims its own row");
        };

        assert_eq!(reclaimed.bound_to, None);
        assert!(!reclaimed.flags.contains(LeaseFlags::PLAYER_BOUND));
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
        assert_eq!(registrar.disconnect(a, 0).len(), 1);
        assert!(registrar
            .get(entity)
            .unwrap()
            .flags
            .contains(LeaseFlags::PARKED));
    }

    #[test]
    fn a_players_character_stays_theirs_across_a_disconnect() {
        // Given: a character bound to the account playing it.
        let mut registrar = LeaseRegistrar::default();
        let player = node(1);
        let stranger = node(2);
        let character = PersistId::new(50);
        assert!(matches!(
            registrar.claim(character, player, ClaimKind::Strong, 0),
            ClaimResult::Granted(_)
        ));
        assert!(registrar.bind_to_player(character, player));

        // When: the player disconnects, so the character parks.
        assert_eq!(registrar.disconnect(player, 0).len(), 1);
        assert!(registrar
            .get(character)
            .unwrap()
            .flags
            .contains(LeaseFlags::PARKED));

        // Then: a parked character is *not* up for grabs. Without this, the
        // ordinary "first claim unparks it" rule would hand someone else's
        // character to whoever walked past it.
        assert_eq!(
            registrar.claim(character, stranger, ClaimKind::Weak, 1),
            ClaimResult::Denied(DenyReason::NotEligible)
        );
        assert_eq!(
            registrar.claim(character, stranger, ClaimKind::Strong, 1),
            ClaimResult::Denied(DenyReason::NotEligible)
        );

        // And: the owner reclaims it on return, with the binding intact.
        let ClaimResult::Granted(reclaimed) =
            registrar.claim(character, player, ClaimKind::Strong, 2)
        else {
            panic!("the returning account must be able to reclaim its character");
        };
        assert_eq!(reclaimed.holder, Some(player));
        assert!(reclaimed.flags.contains(LeaseFlags::PLAYER_BOUND));
        assert_eq!(reclaimed.bound_to, Some(player));
    }

    #[test]
    fn binding_needs_an_existing_row_and_survives_expiry() {
        let mut registrar = LeaseRegistrar::default();
        let player = node(1);
        let character = PersistId::new(51);

        // Nothing to bind to yet.
        assert!(!registrar.bind_to_player(character, player));

        assert!(matches!(
            registrar.claim(character, player, ClaimKind::Strong, 0),
            ClaimResult::Granted(_)
        ));
        assert!(registrar.bind_to_player(character, player));

        // A TTL lapse parks it like any other lease, and the binding rides
        // through: expiry is not a transfer of ownership.
        assert_eq!(registrar.sweep_expired(LEASE_TTL_MS + 1).len(), 1);
        let row = registrar.get(character).unwrap();
        assert!(row.flags.contains(LeaseFlags::PLAYER_BOUND));
        assert_eq!(row.bound_to, Some(player));
        assert_eq!(
            registrar.claim(character, node(2), ClaimKind::Weak, LEASE_TTL_MS + 2),
            ClaimResult::Denied(DenyReason::NotEligible)
        );
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
            bound_to: None,
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
