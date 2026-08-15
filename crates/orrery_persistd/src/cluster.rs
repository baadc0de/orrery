//! The in-process persistence cluster harness (docs/08-persistence.md §3.2,
//! P2 gaps #2/#7).
//!
//! A [`Cluster`] owns the node set and routes each shard cell to the node that
//! rendezvous (HRW) placement assigns it to. This is the library-side harness
//! the tests use to exercise placement and replication logic without a real
//! node-to-node transport; the reference binary itself stays single-node until
//! that transport exists.
//!
//! Each node is a [`CellRuntime`] with its own journal and actors. The cluster
//! also wires chain replication between nodes using the in-process
//! [`MemChainTransport`]: each node's journal streams to its follower (the next
//! node in HRW order), so the replication logic is testable without pretending
//! that the process-local shim is a distributed failover transport.

use std::collections::HashMap;
use std::sync::Arc;

use orrery_protocol::{CellId, ClaimKind, Lease, LeaseId, NodeId, PersistId};

use orrery_protocol::GridId;

use orrery_protocol::JournalRecord;

use crate::actor::{CellActorHandle, FencedApply, Reject, SnapshotPage};
use crate::journal::{
    AppendHandle, ChainConfig, ChainReplicator, ChainTransport, JournalChainSink,
};
use crate::placement::{RendezvousHasher, RendezvousNode};
use crate::runtime::{CellRuntime, EntityStripeGates};

async fn gated_mutex_actor(
    runtime: &tokio::sync::Mutex<CellRuntime>,
    grid: GridId,
    presented_cell: CellId,
    entity: PersistId,
) -> Result<(tokio::sync::OwnedMutexGuard<()>, CellActorHandle), Reject> {
    let (gate, store, runtime_grid) = {
        let runtime = runtime.lock().await;
        (
            runtime.entity_gate(grid, entity),
            runtime.lease_store_handle(),
            runtime.grid(),
        )
    };
    let guard = gate.lock_owned().await;
    let route_cell = if runtime_grid == grid {
        store
            .locate(grid, entity)
            .await
            .map_err(|_| Reject::LeaseStore)?
            .unwrap_or(presented_cell)
    } else {
        presented_cell
    };
    let actor = runtime
        .lock()
        .await
        .actor(grid, route_cell)
        .cloned()
        .ok_or(Reject::JournalClosed)?;
    Ok((guard, actor))
}

/// The routing surface the gateway uses to reach cell actors.
///
/// A single-node deployment routes everything to its one runtime; a multi-node
/// [`Cluster`] routes each cell to the node rendezvous placement assigns it to
/// (docs/08-persistence.md §3.2). The gateway depends only on this trait, so
/// the routing topology is swappable.
#[async_trait::async_trait]
pub trait Router: Send + Sync {
    /// Sweep registrar TTLs for every live actor this router owns, returning
    /// the rows that lost their holder so the caller can select successors.
    async fn sweep_expired_leases(&self, _now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        Vec::new()
    }

    /// Read one registrar row, its committed cell, and the highest journal
    /// position folded for that entity.
    ///
    /// The uplink watermark is what makes a `Divest.cursor` checkable: a
    /// cursor ahead of it names state the cluster never journaled.
    async fn inspect_lease(
        &self,
        _grid: GridId,
        _entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        Ok((None, None, None))
    }
    /// Apply a journal record to the actor owning its cell, returning the
    /// handle the gateway must await before acknowledging durability.
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject>;

    /// Validate a server-owned committed rekey before actor transfer.
    ///
    /// Task 11 deliberately stops after establishing this trusted entrypoint;
    /// actor export/import and journal application are implemented by Task 12.
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        crate::actor::decode_entity_rekey(&record)?;
        Err(crate::actor::RekeyError::ActorUnavailable)
    }

    /// Atomically check a persistent authority fence and append its diff.
    ///
    /// Real actor routers override this to keep the comparison and admission
    /// in one mailbox turn. The fallback preserves non-authority test routers.
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        _holder: NodeId,
        _lease_id: LeaseId,
        _authority_seq: orrery_protocol::SeqPair,
        _now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        self.apply(record).await.map(FencedApply::Accepted)
    }

    /// Read a snapshot of `cell` in `grid` from its owning actor (P-7:
    /// storage cell ids are grid-relative, so the grid scopes which universe
    /// the cell id names).
    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject>;

    /// Whether a live actor holds `cell` in `grid` (vs a cold FDB scan).
    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool;

    /// Resolve an entity's committed cell without trusting a client cell hint.
    async fn committed_entity_cell(
        &self,
        _grid: GridId,
        _entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        Ok(None)
    }

    /// Read a cold cell from the durable tier (an FDB range scan), if this
    /// router has a cold-store fallback. Returns `None` when there is no cold
    /// store or the cell has no durable rows.
    ///
    /// Area load serves **live cells** from actor memory (authoritative, ≥
    /// checkpoint freshness) and **cold cells** from this scan
    /// (docs/08-persistence.md §9). `grid` scopes the scan: storage cell ids
    /// are grid-relative (P-7, D11 §6).
    async fn read_cold(&self, grid: GridId, cell: CellId) -> Result<Option<SnapshotPage>, Reject> {
        let _ = (grid, cell);
        Ok(None)
    }
    /// Serialized registrar claim routed to the actor owning `cell`.
    async fn claim_lease(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
        _holder: NodeId,
        _kind: ClaimKind,
        _now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        Err(Reject::JournalClosed)
    }
    /// Renew or inspect a session lease.
    async fn heartbeat_lease(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
        _holder: NodeId,
        _lease_id: LeaseId,
        _now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        Err(Reject::JournalClosed)
    }
    /// Validate a bulk fencing token, returning the current row on failure.
    async fn validate_lease(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
        _holder: NodeId,
        _lease_id: LeaseId,
        _now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        Err(Reject::JournalClosed)
    }
    /// Park a disconnecting holder's indexed lease.
    async fn park_lease(
        &self,
        _grid: GridId,
        _cell: CellId,
        _entity: PersistId,
        _holder: NodeId,
        _lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        Err(Reject::JournalClosed)
    }
}

/// A router over a single runtime (one-node deployment).
///
/// Direct dispatch into the actor mailbox without lock acquisition, pipelining
/// concurrent applies directly into the journal's commit queue (§4).
#[async_trait::async_trait]
impl Router for CellRuntime {
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        self.sweep_expired_leases(now_ms).await
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        self.inspect_lease(grid, entity).await
    }
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        self.actor(record.grid, record.cell)
            .ok_or(Reject::JournalClosed)?
            .start_diff(record)
            .await
    }
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        self.commit_rekey(record).await
    }
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        let gate = self.entity_gate(record.grid, record.entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self
            .lease_location(record.entity)
            .await?
            .unwrap_or(record.cell);
        self.actor(record.grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .start_fenced_diff(record, holder, lease_id, authority_seq, now_ms)
            .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        self.actor(grid, cell)
            .ok_or(Reject::JournalClosed)?
            .read_snapshot(vec![cell])
            .await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        self.actor(grid, cell).is_some()
    }
    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        if self.grid() != grid {
            return Ok(None);
        }
        if let Some(cell) = self.lease_location(entity).await? {
            return Ok(Some(cell));
        }
        let actors: Vec<_> = self
            .shards()
            .filter_map(|shard| self.actor(grid, *shard).cloned())
            .collect();
        for actor in actors {
            if let Some(cell) = actor.committed_entity_cell(entity).await? {
                return Ok(Some(cell));
            }
        }
        Ok(None)
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        let gate = self.entity_gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self.lease_location(entity).await?.unwrap_or(cell);
        self.actor(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .claim_lease(entity, cell, holder, kind, now_ms)
            .await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let gate = self.entity_gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self.lease_location(entity).await?.unwrap_or(cell);
        self.actor(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .heartbeat_lease(entity, holder, lease_id, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let gate = self.entity_gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self.lease_location(entity).await?.unwrap_or(cell);
        self.actor(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .validate_lease(entity, holder, lease_id, now_ms)
            .await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        let gate = self.entity_gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self.lease_location(entity).await?.unwrap_or(cell);
        self.actor(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .park_lease(entity, holder, lease_id)
            .await
    }
}

/// A router over a shared runtime.
#[async_trait::async_trait]
impl Router for Arc<CellRuntime> {
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        <CellRuntime as Router>::sweep_expired_leases(self.as_ref(), now_ms).await
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        <CellRuntime as Router>::inspect_lease(self.as_ref(), grid, entity).await
    }
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        <CellRuntime as Router>::apply(self.as_ref(), record).await
    }
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        <CellRuntime as Router>::commit_rekey(self.as_ref(), record).await
    }
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        <CellRuntime as Router>::apply_fenced(
            self.as_ref(),
            record,
            holder,
            lease_id,
            authority_seq,
            now_ms,
        )
        .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        <CellRuntime as Router>::read(self.as_ref(), grid, cell).await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        <CellRuntime as Router>::has_actor(self.as_ref(), grid, cell).await
    }
    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        <CellRuntime as Router>::committed_entity_cell(self.as_ref(), grid, entity).await
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        self.as_ref()
            .claim_lease(grid, cell, entity, holder, kind, now_ms)
            .await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        self.as_ref()
            .heartbeat_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        self.as_ref()
            .validate_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        self.as_ref()
            .park_lease(grid, cell, entity, holder, lease_id)
            .await
    }
}

/// A router over a single runtime behind a Mutex (test compatibility).
///
/// The guard is never held across an actor await: the handle is resolved
/// under the lock, the lock is dropped, and the actor mailbox is awaited
/// outside it — so concurrent applies pipeline into the journal's commit
/// queue instead of serializing the whole node behind one fsync (§4).
#[async_trait::async_trait]
impl Router for tokio::sync::Mutex<CellRuntime> {
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        // The actor mailboxes are awaited outside the runtime lock: a sweep
        // must never hold the whole node while each actor drains its queue.
        let actors = self.lock().await.actor_handles();
        let mut parked = Vec::new();
        for actor in actors {
            if let Ok(rows) = actor.sweep_leases(now_ms).await {
                parked.extend(rows);
            }
        }
        parked
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        let (cell, handle) = {
            let rt = self.lock().await;
            let Some(cell) = rt.lease_location(entity).await? else {
                return Ok((None, None, None));
            };
            (cell, rt.actor(grid, cell).cloned())
        };
        match handle {
            Some(handle) => handle.inspect_lease(entity).await,
            None => Ok((None, Some(cell), None)),
        }
    }
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        let handle = {
            let rt = self.lock().await;
            rt.actor(record.grid, record.cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .start_diff(record)
            .await
    }
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        let rekey = crate::actor::decode_entity_rekey(&record)?;
        let gate = self
            .lock()
            .await
            .entity_gate(rekey.source_grid, rekey.entity);
        let _guard = gate.lock_owned().await;
        let plan = self.lock().await.committed_rekey_plan(record)?;
        plan.execute().await
    }
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        let (_guard, handle) =
            gated_mutex_actor(self, record.grid, record.cell, record.entity).await?;
        handle
            .start_fenced_diff(record, holder, lease_id, authority_seq, now_ms)
            .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        let handle = {
            let rt = self.lock().await;
            rt.actor(grid, cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .read_snapshot(vec![cell])
            .await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        let rt = self.lock().await;
        rt.actor(grid, cell).is_some()
    }
    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        let (store, actors): (_, Vec<_>) = {
            let runtime = self.lock().await;
            if runtime.grid() != grid {
                return Ok(None);
            }
            (
                runtime.lease_store_handle(),
                runtime
                    .shards()
                    .filter_map(|shard| runtime.actor(grid, *shard).cloned())
                    .collect(),
            )
        };
        if let Some(cell) = store
            .locate(grid, entity)
            .await
            .map_err(|_| Reject::LeaseStore)?
        {
            return Ok(Some(cell));
        }
        for actor in actors {
            if let Some(cell) = actor.committed_entity_cell(entity).await? {
                return Ok(Some(cell));
            }
        }
        Ok(None)
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        let (_guard, actor) = gated_mutex_actor(self, grid, cell, entity).await?;
        actor.claim_lease(entity, cell, holder, kind, now_ms).await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let (_guard, actor) = gated_mutex_actor(self, grid, cell, entity).await?;
        actor
            .heartbeat_lease(entity, holder, lease_id, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let (_guard, actor) = gated_mutex_actor(self, grid, cell, entity).await?;
        actor.validate_lease(entity, holder, lease_id, now_ms).await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        let (_guard, actor) = gated_mutex_actor(self, grid, cell, entity).await?;
        actor.park_lease(entity, holder, lease_id).await
    }
}

/// A router over a shared runtime handle.
///
/// This lets the single-node `persistd` binary keep one `Arc<Mutex<CellRuntime>>`
/// for shutdown while still composing that runtime into the cold-fallback
/// router used when FoundationDB is available.
#[async_trait::async_trait]
impl Router for Arc<tokio::sync::Mutex<CellRuntime>> {
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        self.as_ref().sweep_expired_leases(now_ms).await
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        self.as_ref().inspect_lease(grid, entity).await
    }
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        self.as_ref().apply(record).await
    }
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        self.as_ref().commit_rekey(record).await
    }
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        self.as_ref()
            .apply_fenced(record, holder, lease_id, authority_seq, now_ms)
            .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        self.as_ref().read(grid, cell).await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        self.as_ref().has_actor(grid, cell).await
    }
    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        self.as_ref().committed_entity_cell(grid, entity).await
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        self.as_ref()
            .claim_lease(grid, cell, entity, holder, kind, now_ms)
            .await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        self.as_ref()
            .heartbeat_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        self.as_ref()
            .validate_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        self.as_ref()
            .park_lease(grid, cell, entity, holder, lease_id)
            .await
    }
}

/// A router that serves cold cells from a durable [`ColdCellReader`], falling
/// back to a live [`Router`] for hot cells.
///
/// Composes the live routing topology with the FDB-backed cold-store fallback
/// (docs/08-persistence.md §9): live cells come from actor memory, cold cells
/// from the durable range scan.
pub struct ColdFallbackRouter<R> {
    /// The live router (single runtime or cluster).
    live: R,
    /// The durable cold-cell reader.
    cold: Arc<dyn crate::checkpoint::ColdCellReader>,
}

impl<R> ColdFallbackRouter<R> {
    /// A router serving `live` with `cold` as the cold-cell fallback.
    #[must_use]
    pub fn new(live: R, cold: Arc<dyn crate::checkpoint::ColdCellReader>) -> Self {
        Self { live, cold }
    }
}

#[async_trait::async_trait]
impl<R: Router + Send + Sync> Router for ColdFallbackRouter<R> {
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        self.live.sweep_expired_leases(now_ms).await
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        self.live.inspect_lease(grid, entity).await
    }
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        self.live.apply(record).await
    }
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        self.live.commit_rekey(record).await
    }
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        self.live
            .apply_fenced(record, holder, lease_id, authority_seq, now_ms)
            .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        self.live.read(grid, cell).await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        self.live.has_actor(grid, cell).await
    }

    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        self.live.committed_entity_cell(grid, entity).await
    }

    async fn read_cold(&self, grid: GridId, cell: CellId) -> Result<Option<SnapshotPage>, Reject> {
        self.cold
            .read_cold(grid, cell)
            .await
            .map_err(|_| Reject::JournalClosed)
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        self.live
            .claim_lease(grid, cell, entity, holder, kind, now_ms)
            .await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        self.live
            .heartbeat_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        self.live
            .validate_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        self.live
            .park_lease(grid, cell, entity, holder, lease_id)
            .await
    }
}

/// A running cluster harness.
pub struct Cluster {
    /// The node set for rendezvous placement.
    nodes: Vec<RendezvousNode>,
    /// Each node's runtime, keyed by its `u64` node id.
    runtimes: HashMap<u64, Arc<tokio::sync::Mutex<CellRuntime>>>,
    /// Chain-replication tasks (primary → follower), one per node.
    chains: Vec<ChainReplicator>,
    entity_gates: Arc<EntityStripeGates>,
}

impl Cluster {
    /// Build a cluster from one runtime per node id.
    ///
    /// `runtimes` maps each node's `u64` id to its runtime. Chain replication is
    /// wired between consecutive nodes in sorted id order (each node's follower
    /// is the next node; the last wraps to the first), so every node's journal
    /// has a follower. Pass `None` for `chain` to disable replication.
    pub fn new(
        runtimes: HashMap<u64, Arc<tokio::sync::Mutex<CellRuntime>>>,
        chain: Option<&ChainConfig>,
    ) -> Self {
        let mut ids: Vec<u64> = runtimes.keys().copied().collect();
        ids.sort_unstable();
        let nodes: Vec<RendezvousNode> = ids.iter().map(|&id| RendezvousNode::new(id)).collect();

        let mut chains = Vec::new();
        if let Some(chain) = chain {
            if ids.len() > 1 {
                for (i, &id) in ids.iter().enumerate() {
                    let follower_id = ids[(i + 1) % ids.len()];
                    let source = runtimes.get(&id).expect("source present");
                    let follower = runtimes.get(&follower_id).expect("follower present");
                    let source_journal = journal_of(source);
                    let follower_journal = journal_of(follower);
                    let sink = Arc::new(JournalChainSink::new(follower_journal));
                    let transport: Arc<dyn ChainTransport> =
                        Arc::new(crate::journal::MemChainTransport::new(sink));
                    let cfg = ChainConfig {
                        follower: follower_id,
                        ..chain.clone()
                    };
                    let replicator = crate::journal::spawn_chain(source_journal, transport, &cfg);
                    chains.push(replicator);
                }
            }
        }

        Self {
            nodes,
            runtimes,
            chains,
            entity_gates: Arc::new(EntityStripeGates::default()),
        }
    }

    /// The node id that owns `cell` under rendezvous placement.
    #[must_use]
    pub fn owner(&self, cell: CellId) -> Option<u64> {
        RendezvousHasher::new(self.nodes.clone()).owner(cell)
    }

    /// The runtime owning `(grid, cell)`, if this cluster hosts it.
    ///
    /// Placement is keyed by `(grid, cell)` (P-7: storage cell ids are
    /// grid-relative). Today each cluster serves exactly one grid — a nested
    /// deployment runs one cluster per grid — so a mismatched grid has no
    /// runtime to route to and this returns `None`. Grid validity is checked
    /// after the selected runtime lock is acquired, by the runtime's own
    /// `actor()` guard; routing selection must not depend on lock availability.
    #[must_use]
    pub fn runtime_for(
        &self,
        _grid: GridId,
        cell: CellId,
    ) -> Option<&Arc<tokio::sync::Mutex<CellRuntime>>> {
        let owner = self.owner(cell)?;
        let rt = self.runtimes.get(&owner)?;
        Some(rt)
    }

    /// The node set (for diagnostics).
    #[must_use]
    pub fn nodes(&self) -> &[RendezvousNode] {
        &self.nodes
    }

    /// The number of nodes in the cluster.
    #[must_use]
    pub fn len(&self) -> usize {
        self.runtimes.len()
    }

    /// Whether the cluster has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }

    /// Stop chain replication and close every node's journal.
    pub async fn close(self) {
        for chain in self.chains {
            chain.shutdown().await;
        }
        for (_, rt) in self.runtimes {
            let rt = Arc::try_unwrap(rt)
                .unwrap_or_else(|_| panic!("cluster runtime still referenced"))
                .into_inner();
            let _ = rt.close().await;
        }
    }
}

/// Clone the journal `Arc` out of a runtime. Safe at cluster-build time: no
/// other task holds the runtime's lock yet.
fn journal_of(rt: &Arc<tokio::sync::Mutex<CellRuntime>>) -> Arc<crate::journal::Journal> {
    let guard = rt.try_lock().expect("cluster build holds no runtime lock");
    Arc::clone(guard.journal())
}

/// A router over a multi-node cluster: each cell routes to the node rendezvous
/// placement assigns it to (docs/08-persistence.md §3.2).
#[async_trait::async_trait]
impl Router for Cluster {
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        let mut parked = Vec::new();
        for runtime in self.runtimes.values() {
            parked.extend(runtime.sweep_expired_leases(now_ms).await);
        }
        parked
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<orrery_protocol::Lsn>), Reject> {
        for runtime in self.runtimes.values() {
            let found = runtime.inspect_lease(grid, entity).await?;
            if found.0.is_some() || found.1.is_some() {
                return Ok(found);
            }
        }
        Ok((None, None, None))
    }
    async fn apply(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        let rt = self
            .runtime_for(record.grid, record.cell)
            .ok_or(Reject::JournalClosed)?;
        let handle = {
            let rt = rt.lock().await;
            rt.actor(record.grid, record.cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .start_diff(record)
            .await
    }
    async fn commit_rekey(&self, record: JournalRecord) -> Result<(), crate::actor::RekeyError> {
        let rekey = crate::actor::decode_entity_rekey(&record)?;
        let gate = self.entity_gates.gate(rekey.source_grid, rekey.entity);
        let _guard = gate.lock_owned().await;
        let source_owner = self
            .owner(rekey.source_cell)
            .ok_or(crate::actor::RekeyError::ActorUnavailable)?;
        if self.owner(rekey.destination_cell) != Some(source_owner) {
            return Err(crate::actor::RekeyError::ActorUnavailable);
        }
        let runtime = self
            .runtimes
            .get(&source_owner)
            .ok_or(crate::actor::RekeyError::ActorUnavailable)?;
        let plan = runtime.lock().await.committed_rekey_plan(record)?;
        plan.execute().await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        let rt = self.runtime_for(grid, cell).ok_or(Reject::JournalClosed)?;
        let handle = {
            let rt = rt.lock().await;
            rt.actor(grid, cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .read_snapshot(vec![cell])
            .await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        let Some(rt) = self.runtime_for(grid, cell) else {
            return false;
        };
        let rt = rt.lock().await;
        rt.actor(grid, cell).is_some()
    }
    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, Reject> {
        let runtimes: Vec<_> = self.runtimes.values().cloned().collect();
        for runtime in runtimes {
            if let Some(cell) = runtime.committed_entity_cell(grid, entity).await? {
                return Ok(Some(cell));
            }
        }
        Ok(None)
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<crate::lease::ClaimResult, Reject> {
        let gate = self.entity_gates.gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self
            .committed_entity_cell(grid, entity)
            .await?
            .unwrap_or(cell);
        self.runtime_for(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .claim_lease(grid, cell, entity, holder, kind, now_ms)
            .await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let gate = self.entity_gates.gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self
            .committed_entity_cell(grid, entity)
            .await?
            .unwrap_or(cell);
        self.runtime_for(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .heartbeat_lease(grid, route_cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let gate = self.entity_gates.gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self
            .committed_entity_cell(grid, entity)
            .await?
            .unwrap_or(cell);
        self.runtime_for(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .validate_lease(grid, route_cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        let gate = self.entity_gates.gate(grid, entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self
            .committed_entity_cell(grid, entity)
            .await?
            .unwrap_or(cell);
        self.runtime_for(grid, route_cell)
            .ok_or(Reject::JournalClosed)?
            .park_lease(grid, route_cell, entity, holder, lease_id)
            .await
    }
    async fn apply_fenced(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        let gate = self.entity_gates.gate(record.grid, record.entity);
        let _guard = gate.lock_owned().await;
        let route_cell = self
            .committed_entity_cell(record.grid, record.entity)
            .await?
            .unwrap_or(record.cell);
        let rt = self
            .runtime_for(record.grid, route_cell)
            .ok_or(Reject::JournalClosed)?;
        let handle = {
            let rt = rt.lock().await;
            rt.actor(record.grid, route_cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .start_fenced_diff(record, holder, lease_id, authority_seq, now_ms)
            .await
    }
}
