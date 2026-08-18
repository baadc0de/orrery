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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use orrery_protocol::{CellId, ClaimKind, Lease, LeaseId, NodeId, PersistId};

use orrery_protocol::GridId;

use orrery_protocol::JournalRecord;

use crate::actor::{CellActorHandle, FencedApply, Reject, SnapshotPage};
use crate::journal::{
    AppendHandle, ChainConfig, ChainReplicator, ChainTransport, JournalChainSink,
};
use crate::placement::{RendezvousHasher, RendezvousNode};
use crate::runtime::{CellRuntime, EntityStripeGates};

/// The three waits inside one `Router::apply_fenced`, counted separately.
///
/// `gateway_bulk_stage_delta` calls the whole of `apply_fenced` "router_apply",
/// and on the 2026-08-18 gate that one number was 8.198 ms mean against the
/// 2.734 ms mean measured before the lease lane moved off the receive loop —
/// which reads as an actor mailbox that has started to queue. It is not one
/// wait. It is three, and they live in different subsystems:
///
/// * `gate_wait` — the 1024-way striped per-entity mutex, held across both
///   waits below so a rekey cannot interleave with a fenced append.
/// * `locate` — `LeaseStore::locate`, which under `--fdb-cluster-file` is a
///   **FoundationDB read transaction**, one per admitted diff. Not a mailbox,
///   not a disk: a network round trip to the cluster, on the write path.
/// * `mailbox` — the actor round trip proper: `start_fenced_diff` send, queue,
///   turn, reply.
///
/// Splitting them is the whole point of the exercise. A staleness valve has to
/// go *after* the wait it bounds, and "router_apply" names three candidate
/// waits at once; placing against the aggregate is guessing.
///
/// Counted process-globally rather than per-runtime because `CellRuntime` is
/// frozen to this lane and cannot take a field. A node runs one gateway over
/// one router, so the process aggregate *is* the router's.
#[derive(Debug, Default)]
pub struct RouteStageMetrics {
    applies: AtomicU64,
    gate_wait_us_sum: AtomicU64,
    gate_wait_us_max: AtomicU64,
    locate_us_sum: AtomicU64,
    locate_us_max: AtomicU64,
    mailbox_us_sum: AtomicU64,
    mailbox_us_max: AtomicU64,
    batch_locks: AtomicU64,
    batch_gates_sum: AtomicU64,
    batch_hold_us_sum: AtomicU64,
    batch_hold_us_max: AtomicU64,
    mailbox_turns: AtomicU64,
    locate_fallbacks: AtomicU64,
    location_audits: AtomicU64,
    location_mismatches: AtomicU64,
}

/// A point-in-time read of [`RouteStageMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteStageSnapshot {
    /// Fenced applies that completed all three stages.
    pub applies: u64,
    /// Summed wait on the striped per-entity gate.
    pub gate_wait_us_sum: u64,
    /// Longest single wait on the striped per-entity gate.
    pub gate_wait_us_max: u64,
    /// Summed `LeaseStore::locate` time (an FDB read under `fdb`).
    pub locate_us_sum: u64,
    /// Longest single `LeaseStore::locate`.
    pub locate_us_max: u64,
    /// Summed actor round trip.
    pub mailbox_us_sum: u64,
    /// Longest single actor round trip.
    pub mailbox_us_max: u64,
    /// Multi-gate acquisitions by a batched lease operation.
    ///
    /// One per *actor group*, not one per batch: `heartbeat_leases` takes a
    /// group's gates around its own mailbox turn and releases them before the
    /// next group's, so a batch that used to be one 77-gate lock is now many
    /// one-gate locks. Compare `batch_hold_us_sum` and this counter together
    /// — a rise here with a fall there is the whole point, not a regression.
    pub batch_locks: u64,
    /// Total gates those acquisitions held (summed set sizes).
    pub batch_gates_sum: u64,
    /// Summed time an acquisition held its gate set.
    pub batch_hold_us_sum: u64,
    /// Longest single hold.
    pub batch_hold_us_max: u64,
    /// Actor mailbox turns those fenced applies spent.
    ///
    /// One per apply on the fast path. The route is bounded at **two** — ask
    /// the presented cell's owner, and at most one forwarded turn after a
    /// rowless reject — so `mailbox_turns / applies` is a number that must
    /// sit at 1.0 and can never exceed 2.0. It is the cheapest check that the
    /// fallback has not become the path.
    pub mailbox_turns: u64,
    /// Fenced applies that fell back to `LeaseStore::locate`.
    ///
    /// Structurally ~0: under `strict_authority` the admission predicate
    /// requires `by_cell[e] == record.cell` *before* the fold that would move
    /// it, so a fenced diff cannot relocate an entity and `record.cell` stays
    /// pinned at the grant cell for the entity's life. A hot fallback means
    /// that reasoning is wrong somewhere — and that the FDB read is back at
    /// close to full rate, plus a wasted mailbox turn.
    pub locate_fallbacks: u64,
    /// Sampled accepts that paid for a location audit (see
    /// `fenced_location_audit_due`).
    pub location_audits: u64,
    /// Audited accepts where the durable location named a cell **outside**
    /// the accepting actor's shard.
    ///
    /// Invariant J says this is impossible. A nonzero value is a stop-ship:
    /// it means an actor admitted a fenced write against a registrar row that
    /// is not the durably-located one, which is exactly the silent failure
    /// the per-diff locate used to make unreachable.
    pub location_mismatches: u64,
}

impl RouteStageMetrics {
    fn record(&self, gate_wait_us: u64, locate_us: u64, mailbox_us: u64) {
        self.applies.fetch_add(1, Ordering::Relaxed);
        for (sum, max, value) in [
            (&self.gate_wait_us_sum, &self.gate_wait_us_max, gate_wait_us),
            (&self.locate_us_sum, &self.locate_us_max, locate_us),
            (&self.mailbox_us_sum, &self.mailbox_us_max, mailbox_us),
        ] {
            sum.fetch_add(value, Ordering::Relaxed);
            max.fetch_max(value, Ordering::Relaxed);
        }
    }

    fn record_mailbox_turn(&self) {
        self.mailbox_turns.fetch_add(1, Ordering::Relaxed);
    }

    fn record_locate_fallback(&self) {
        self.locate_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    fn record_location_audit(&self, mismatched: bool) {
        self.location_audits.fetch_add(1, Ordering::Relaxed);
        if mismatched {
            self.location_mismatches.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_batch_hold(&self, gates: usize, hold_us: u64) {
        self.batch_locks.fetch_add(1, Ordering::Relaxed);
        self.batch_gates_sum
            .fetch_add(gates as u64, Ordering::Relaxed);
        self.batch_hold_us_sum.fetch_add(hold_us, Ordering::Relaxed);
        self.batch_hold_us_max.fetch_max(hold_us, Ordering::Relaxed);
    }

    /// Read every counter.
    #[must_use]
    pub fn snapshot(&self) -> RouteStageSnapshot {
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        RouteStageSnapshot {
            applies: load(&self.applies),
            gate_wait_us_sum: load(&self.gate_wait_us_sum),
            gate_wait_us_max: load(&self.gate_wait_us_max),
            locate_us_sum: load(&self.locate_us_sum),
            locate_us_max: load(&self.locate_us_max),
            mailbox_us_sum: load(&self.mailbox_us_sum),
            mailbox_us_max: load(&self.mailbox_us_max),
            batch_locks: load(&self.batch_locks),
            batch_gates_sum: load(&self.batch_gates_sum),
            batch_hold_us_sum: load(&self.batch_hold_us_sum),
            batch_hold_us_max: load(&self.batch_hold_us_max),
            mailbox_turns: load(&self.mailbox_turns),
            locate_fallbacks: load(&self.locate_fallbacks),
            location_audits: load(&self.location_audits),
            location_mismatches: load(&self.location_mismatches),
        }
    }
}

impl RouteStageSnapshot {
    /// This snapshot minus an earlier one: sums subtract, maxima do not — a
    /// maximum is a run-high, exactly as in `GatewayBulkMetrics`' own delta.
    #[must_use]
    pub fn delta(self, previous: Self) -> Self {
        let sub = |current: u64, previous: u64| current.saturating_sub(previous);
        Self {
            applies: sub(self.applies, previous.applies),
            gate_wait_us_sum: sub(self.gate_wait_us_sum, previous.gate_wait_us_sum),
            gate_wait_us_max: self.gate_wait_us_max,
            locate_us_sum: sub(self.locate_us_sum, previous.locate_us_sum),
            locate_us_max: self.locate_us_max,
            mailbox_us_sum: sub(self.mailbox_us_sum, previous.mailbox_us_sum),
            mailbox_us_max: self.mailbox_us_max,
            batch_locks: sub(self.batch_locks, previous.batch_locks),
            batch_gates_sum: sub(self.batch_gates_sum, previous.batch_gates_sum),
            batch_hold_us_sum: sub(self.batch_hold_us_sum, previous.batch_hold_us_sum),
            batch_hold_us_max: self.batch_hold_us_max,
            mailbox_turns: sub(self.mailbox_turns, previous.mailbox_turns),
            locate_fallbacks: sub(self.locate_fallbacks, previous.locate_fallbacks),
            location_audits: sub(self.location_audits, previous.location_audits),
            location_mismatches: sub(self.location_mismatches, previous.location_mismatches),
        }
    }
}

/// One in `ORRERY_FENCED_LOCATION_AUDIT_N` accepted fenced diffs pays for a
/// real `LeaseStore::locate` purely to check invariant J.
///
/// Default 1000 in release — 0.1 % of the FDB load this change removes, in
/// exchange for turning the one silent failure mode into a monitored one —
/// and **1** under `debug_assertions` and in the test suite, so every existing
/// test that routes a fenced diff becomes a test of the route. `0` disables
/// it, which is the only way to give up the signal and should not be the
/// default anywhere.
fn fenced_location_audit_due() -> bool {
    static EVERY: LazyLock<u64> = LazyLock::new(|| {
        std::env::var("ORRERY_FENCED_LOCATION_AUDIT_N")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(if cfg!(debug_assertions) { 1 } else { 1000 })
    });
    static SEEN: AtomicU64 = AtomicU64::new(0);
    match *EVERY {
        0 => false,
        every => SEEN.fetch_add(1, Ordering::Relaxed).is_multiple_of(every),
    }
}

static ROUTE_STAGE: LazyLock<Arc<RouteStageMetrics>> =
    LazyLock::new(|| Arc::new(RouteStageMetrics::default()));

/// The process-wide fenced-apply stage decomposition.
#[must_use]
pub fn route_stage_metrics() -> Arc<RouteStageMetrics> {
    Arc::clone(&ROUTE_STAGE)
}

/// A batch's whole gate set, timed until it drops.
///
/// A guard rather than a pair of statements because the batched paths have
/// `?` in them: an early return still releases the gates, so it must still
/// record the hold, or the metric under-counts exactly the slow cases.
struct HeldGates {
    #[allow(dead_code)]
    guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
    gates: usize,
    started: Instant,
}

impl Drop for HeldGates {
    fn drop(&mut self) {
        ROUTE_STAGE.record_batch_hold(self.gates, stage_elapsed_us(self.started));
    }
}

fn stage_elapsed_us(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Lock a batch's entity gates without inventing a deadlock.
///
/// Every other lease path takes exactly one gate, so it can never be half of a
/// cycle; a batch that takes several can be, unless all batches agree on an
/// order. The gates are striped, so entity order is *not* that order — two
/// batches sorted by entity can still reach the same pair of stripes in
/// opposite orders. The stripe's own address is a total order every caller
/// computes identically, and deduplicating it is what keeps two entities that
/// share a stripe from deadlocking a batch against itself.
async fn lock_entity_gates(
    gates: impl IntoIterator<Item = Arc<tokio::sync::Mutex<()>>>,
) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut gates: Vec<_> = gates.into_iter().collect();
    gates.sort_by_key(|gate| Arc::as_ptr(gate) as usize);
    gates.dedup_by_key(|gate| Arc::as_ptr(gate) as usize);
    let mut guards = Vec::with_capacity(gates.len());
    for gate in gates {
        guards.push(gate.lock_owned().await);
    }
    guards
}

/// One entry of a batched renewal: a pair to renew and the cell the session
/// index says holds it.
///
/// The cell travels *with* each entry rather than parameterising the whole
/// batch, because the batch is a peer's whole heartbeat and a peer's leases
/// are spread over as many cells as it holds entities. Which of those cells
/// share a mailbox is the router's knowledge, not the caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRenewal {
    /// The cell the caller believes owns the row (a hint; the router
    /// re-resolves the committed cell per entity).
    pub cell: CellId,
    /// The entity whose lease is being renewed.
    pub entity: PersistId,
    /// The fencing token the holder presents for that entity.
    pub lease_id: LeaseId,
}

/// Group a batch's indices by route key, keeping request order inside each
/// group so the positional reply stays aligned.
fn group_by_route<K: Copy + PartialEq>(routes: &[K]) -> Vec<(K, Vec<usize>)> {
    let mut groups: Vec<(K, Vec<usize>)> = Vec::new();
    for (index, key) in routes.iter().enumerate() {
        match groups.iter_mut().find(|(grouped, _)| grouped == key) {
            Some((_, members)) => members.push(index),
            None => groups.push((*key, vec![index])),
        }
    }
    groups
}

/// Group a batch's indices by the **actor** that owns each entry's route cell.
///
/// This is the fold that matters. An actor owns a shard and a shard holds very
/// many leaf cells, so grouping by the leaf cell groups by something strictly
/// finer than the mailbox: measured on the P2 workload, 2079 entities sat in
/// 2079 distinct leaf cells — one member per group — and the batched path cost
/// exactly what the unbatched one did. Resolving each route cell to its owning
/// shard first collapses all of those into one group per actor.
///
/// A route cell no shard covers keeps its own `None` group; those entries have
/// no actor to renew against and are answered `None` individually, never
/// silently merged with a routable group.
pub(crate) fn group_by_actor(
    shards: &[CellId],
    routes: &[CellId],
) -> Vec<(Option<CellId>, Vec<usize>)> {
    let keys: Vec<Option<CellId>> = routes
        .iter()
        .map(|cell| {
            shards
                .iter()
                .filter(|shard| shard.is_prefix_of(*cell))
                .max_by_key(|shard| shard.level())
                .copied()
        })
        .collect();
    group_by_route(&keys)
}

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
    /// Renew a whole batch of one session's leases, each entry naming its own
    /// cell.
    ///
    /// A peer heartbeats every lease it holds every 2.5 s. Renewing them one
    /// message at a time costs one actor turn per held entity through a
    /// bounded mailbox — 50 turns for a peer holding 50 entities — even though
    /// the rows share an actor and each check is independent of the others.
    ///
    /// The caller hands over **all** of a grid's renewals for one peer and the
    /// router folds them by the actor that owns each, because which cells
    /// share a mailbox is the router's knowledge: an actor owns a shard, and a
    /// shard holds very many leaf cells. Keying the batch on the leaf cell
    /// instead — which the caller *can* see — folds nothing on the workload
    /// that matters, where each entity sits in a leaf cell of its own.
    ///
    /// The reply is **positional**: one entry per requested pair, in request
    /// order, `None` where that pair did not renew. Batching must not blur the
    /// ack — a holder has to learn exactly which entity it may no longer
    /// write, not that "something" in its batch failed.
    ///
    /// The default fans out over [`Router::heartbeat_lease`] so routers with
    /// no actor of their own keep working unchanged; a routing failure for one
    /// pair is that pair's `None`, exactly as the caller treated it before.
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        let mut rows = Vec::with_capacity(renew.len());
        for entry in renew {
            rows.push(
                self.heartbeat_lease(
                    grid,
                    entry.cell,
                    entry.entity,
                    holder,
                    entry.lease_id,
                    now_ms,
                )
                .await
                .unwrap_or(None),
            );
        }
        Ok(rows)
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

/// Route-side helpers for the fenced bulk path.
impl CellRuntime {
    /// Sampled proof that invariant J held for an accepted fenced diff.
    ///
    /// The accept-set equivalence argument has exactly one silent failure
    /// mode: an actor holding a registrar row for an entity whose durable
    /// location sits in another actor's shard. Nothing else about a wrong
    /// route is silent — every other way it can go wrong is a `Rejected` the
    /// client sees as a `BulkNack`. So this ships, at 1-in-N, rather than
    /// living only in the test suite: a safety argument with a production
    /// counter proving it is the strongest form available here.
    ///
    /// Called with the entity gate still held, so the location it reads is
    /// the one that was true for the accept it is checking. `locate` = `None`
    /// is not a mismatch: the pre-change route answered a `None` with
    /// `unwrap_or(record.cell)`, which is the actor that just accepted.
    async fn audit_fenced_location(
        &self,
        grid: GridId,
        entity: PersistId,
        accepting_shard: Option<CellId>,
        stages: &RouteStageMetrics,
    ) {
        let Some(accepting_shard) = accepting_shard else {
            return;
        };
        if !fenced_location_audit_due() {
            return;
        }
        let located = match self.lease_location(entity).await {
            Ok(Some(cell)) => cell,
            // An audit is not an admission path: a store error costs the
            // sample, not the write that was already accepted.
            Ok(None) | Err(_) => return,
        };
        let owner = self.owning_shard(grid, located);
        let mismatched = owner != Some(accepting_shard);
        stages.record_location_audit(mismatched);
        if mismatched {
            tracing::warn!(
                ?grid,
                ?entity,
                ?located,
                ?owner,
                ?accepting_shard,
                "fenced apply: durable location is outside the accepting actor's shard \
                 (invariant J violated; the accept set is no longer provably unchanged)"
            );
        }
    }

    /// The pre-change fenced-apply implementation, retained as a
    /// differential oracle.
    ///
    /// A verbatim copy of what `<CellRuntime as Router>::apply_fenced` did
    /// before the bulk write path stopped reading FoundationDB: take the
    /// entity gate, resolve the route with one `LeaseStore::locate`, ask the
    /// actor that owns the resolved cell. It exists so a test can assert the
    /// two implementations return the same discriminant **and** the same
    /// `Option<Lease>` payload over an enumerated state matrix — the
    /// accept-set equivalence argument turned into a checked fact rather than
    /// a comment.
    ///
    /// Not `#[cfg(test)]`: the matrix that uses it is an integration test,
    /// and the same matrix has to run under both `MemLeaseStore` (default
    /// features) and `FdbLeaseStore` (`--features fdb`). Nothing in
    /// `persistd` calls it.
    #[doc(hidden)]
    pub async fn apply_fenced_via_locate(
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
        // Three timed stages, not one. See `RouteStageMetrics` for why the
        // aggregate this function reports as `router_apply` was not a usable
        // answer to "where is the queue".
        //
        // The middle stage — one FoundationDB read per fenced bulk diff —
        // used to be unconditional, and docs/14-capacity.md §5.1 measured it
        // as the single binding constraint on a whole box. It is gone from
        // the accept path. The reason it can go is that the locate never
        // entered the fence: `CellMsg::ApplyFencedDiff` evaluates five
        // conjuncts against *actor-local* state (`pending_rekeys`,
        // `by_cell[e] == record.cell`, and holder/lease_id/seq/expiry against
        // the actor's own registrar row), and this function never rewrites
        // `record.cell`. The locate only chose which actor evaluated them.
        //
        // Invariant J is what makes choosing by `record.cell` the same
        // choice: if an actor holds a registrar row for `e` then `locate(e)`
        // names a cell in that actor's shard, so an actor that *accepts* is
        // the actor the locate would have picked. J is enforced at four row
        // install sites (see the `debug_assert`s in `actor.rs`) and backed by
        // `LeaseStore::put` refusing to overwrite a different location. It is
        // also audited in production: see `fenced_location_audit_due`.
        //
        // J does not say a rejecting actor is the right one — cross-shard
        // duplicate `by_cell` entries are reachable — so a reject *without* a
        // row is not proof of absence, and only then does the locate happen.
        // `Rejected(Some(row))` is proof: a row present means, by J, this is
        // the location owner, and the NACK payload the D7 §5 duplicate-
        // authority detector consumes is the one it would have got before.
        let stages = route_stage_metrics();
        let started = Instant::now();
        let gate = self.entity_gate(record.grid, record.entity);
        let _guard = gate.lock_owned().await;
        let gate_wait_us = stage_elapsed_us(started);
        let grid = record.grid;
        let entity = record.entity;
        let presented = record.cell;
        // One clone per diff: a `bytes::Bytes` Arc bump plus ~64 bytes of
        // `Copy` scalars, against the 0.48–2.79 ms round trip it replaces.
        let forwarded = record.clone();
        let mailbox_started = Instant::now();
        let mut asked: Option<CellId> = None;
        let mut fast: Option<Result<FencedApply, Reject>> = None;
        if let Some(actor) = self.actor(grid, presented) {
            asked = Some(actor.shard());
            let answer = actor
                .start_fenced_diff(record, holder, lease_id, authority_seq, now_ms)
                .await;
            stages.record_mailbox_turn();
            // The only short-circuit, and deliberately the only one: an
            // accept, or a reject that carries the live row.
            if matches!(
                answer,
                Ok(FencedApply::Accepted(_)) | Ok(FencedApply::Rejected(Some(_)))
            ) {
                stages.record(gate_wait_us, 0, stage_elapsed_us(mailbox_started));
                if matches!(answer, Ok(FencedApply::Accepted(_))) {
                    self.audit_fenced_location(grid, entity, asked, &stages)
                        .await;
                }
                return answer;
            }
            fast = Some(answer);
        }
        // Fallback: exactly one locate, at most one more mailbox turn, no
        // loop. Entered when the presented cell has no actor here, when its
        // owner rejected without a row, or when its mailbox is gone — the
        // last because a closed mailbox is not an answer about the fence, and
        // the pre-change route would have asked whoever the locate named.
        stages.record_locate_fallback();
        let locate_started = Instant::now();
        let route_cell = self.lease_location(entity).await?.unwrap_or(presented);
        let locate_us = stage_elapsed_us(locate_started);
        let resolved = self.actor(grid, route_cell).ok_or(Reject::JournalClosed)?;
        // Compared by shard, not by handle identity: `CellRuntime::split`
        // swaps handles, and re-sending to the actor that already answered
        // would be a second turn for the same answer.
        if asked == Some(resolved.shard()) {
            stages.record(gate_wait_us, locate_us, stage_elapsed_us(mailbox_started));
            return fast.expect("a shard was asked, so it produced an answer");
        }
        let applied = resolved
            .start_fenced_diff(forwarded, holder, lease_id, authority_seq, now_ms)
            .await;
        stages.record_mailbox_turn();
        stages.record(gate_wait_us, locate_us, stage_elapsed_us(mailbox_started));
        applied
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
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        // Phase 1 holds nothing at all. Routing stays per entity — a lease
        // that migrated since the grant is owned by another actor, and a
        // batch may straddle the two — but one `LeaseStore::locate` per entry
        // is an FDB round trip, and the gate is not what makes that read
        // *readable*, only what makes it *stable*. The stripe's migration
        // counter proves the same stability afterwards for the price of an
        // atomic load, so the reads happen with every gate free. Sampling the
        // mark strictly before the locate it guards is what makes that proof
        // hold.
        let mut marks = Vec::with_capacity(renew.len());
        let mut routes = Vec::with_capacity(renew.len());
        for entry in renew {
            marks.push(self.entity_migration_mark(grid, entry.entity));
            routes.push(
                self.lease_location(entry.entity)
                    .await?
                    .unwrap_or(entry.cell),
            );
        }
        let shards = self.shard_cells();
        let mut rows = vec![None; renew.len()];
        // Entries whose stripe saw a migration land between their locate and
        // their gate. Re-answered one at a time below with the gate held
        // across the locate, which is what this batch used to do for every
        // entry unconditionally.
        let mut restale: Vec<usize> = Vec::new();
        for (shard, members) in group_by_actor(&shards, &routes) {
            let Some(actor) = shard.and_then(|shard| self.actor(grid, shard)).cloned() else {
                continue;
            };
            // Phase 2 takes gates per actor group, not per batch, and holds
            // them across one mailbox turn and nothing else. The order is
            // still `lock_entity_gates`'s deduplicated stripe address, so
            // groups that share a stripe still cannot cycle.
            let guards = lock_entity_gates(
                members
                    .iter()
                    .map(|index| self.entity_gate(grid, renew[*index].entity)),
            )
            .await;
            let held = guards.len();
            let _guards = HeldGates {
                guards,
                gates: held,
                started: Instant::now(),
            };
            let mut fresh = Vec::with_capacity(members.len());
            for index in members {
                if self.entity_migration_mark(grid, renew[index].entity) == marks[index] {
                    fresh.push(index);
                } else {
                    restale.push(index);
                }
            }
            if fresh.is_empty() {
                continue;
            }
            let batch: Vec<_> = fresh
                .iter()
                .map(|index| (renew[*index].entity, renew[*index].lease_id))
                .collect();
            let renewed = actor.heartbeat_leases(batch, holder, now_ms).await?;
            for (index, row) in fresh.into_iter().zip(renewed) {
                rows[index] = row;
            }
        }
        for index in restale {
            rows[index] = <Self as Router>::heartbeat_lease(
                self,
                grid,
                renew[index].cell,
                renew[index].entity,
                holder,
                renew[index].lease_id,
                now_ms,
            )
            .await?;
        }
        Ok(rows)
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
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        <CellRuntime as Router>::heartbeat_leases(self.as_ref(), grid, holder, renew, now_ms).await
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
        // Deliberately still gate-then-locate, unlike `CellRuntime`'s.
        //
        // This impl is not on the shipped P2 path (`persistd` wires
        // `ColdFallbackRouter<Arc<CellRuntime>>`, which delegates to
        // `CellRuntime`), and the ask-the-owner-first route rests on
        // invariant J holding for the *same* actor set the gate serialises
        // against. Making it consistent "for tidiness" would be re-deriving
        // that argument for a different structure by assumption rather than
        // by proof. If this path ever ships, prove J for it first, then take
        // the differential matrix in `tests/fenced_route_differential.rs`
        // with it.
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
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        let (entity_gates, store, runtime_grid, shards) = {
            let runtime = self.lock().await;
            (
                runtime.entity_gates_handle(),
                runtime.lease_store_handle(),
                runtime.grid(),
                runtime.shard_cells(),
            )
        };
        // Same two phases as the `CellRuntime` impl: locate with no gate
        // held, then take each actor group's gates around its mailbox turn
        // only, with the stripe migration counter standing in for the
        // atomicity the old whole-batch hold bought.
        let mut marks = Vec::with_capacity(renew.len());
        let mut routes = Vec::with_capacity(renew.len());
        for entry in renew {
            marks.push(entity_gates.migration_mark(grid, entry.entity));
            routes.push(if runtime_grid == grid {
                store
                    .locate(grid, entry.entity)
                    .await
                    .map_err(|_| Reject::LeaseStore)?
                    .unwrap_or(entry.cell)
            } else {
                entry.cell
            });
        }
        let mut rows = vec![None; renew.len()];
        let mut restale: Vec<usize> = Vec::new();
        for (shard, members) in group_by_actor(&shards, &routes) {
            // The runtime lock is taken to resolve the handle and released
            // before the mailbox is awaited, exactly as the single-entity
            // paths do: a heartbeat batch must never hold the whole node.
            let Some(shard) = shard else {
                continue;
            };
            let Some(actor) = self.lock().await.actor(grid, shard).cloned() else {
                continue;
            };
            let guards = lock_entity_gates(
                members
                    .iter()
                    .map(|index| entity_gates.gate(grid, renew[*index].entity)),
            )
            .await;
            let held = guards.len();
            let _guards = HeldGates {
                guards,
                gates: held,
                started: Instant::now(),
            };
            let mut fresh = Vec::with_capacity(members.len());
            for index in members {
                if entity_gates.migration_mark(grid, renew[index].entity) == marks[index] {
                    fresh.push(index);
                } else {
                    restale.push(index);
                }
            }
            if fresh.is_empty() {
                continue;
            }
            let batch: Vec<_> = fresh
                .iter()
                .map(|index| (renew[*index].entity, renew[*index].lease_id))
                .collect();
            let renewed = actor.heartbeat_leases(batch, holder, now_ms).await?;
            for (index, row) in fresh.into_iter().zip(renewed) {
                rows[index] = row;
            }
        }
        for index in restale {
            rows[index] = <Self as Router>::heartbeat_lease(
                self,
                grid,
                renew[index].cell,
                renew[index].entity,
                holder,
                renew[index].lease_id,
                now_ms,
            )
            .await?;
        }
        Ok(rows)
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
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        self.as_ref()
            .heartbeat_leases(grid, holder, renew, now_ms)
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
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        self.live
            .heartbeat_leases(grid, holder, renew, now_ms)
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
        let outcome = plan.execute().await;
        // `plan.execute` bumped the *node's* stripe table; this level has its
        // own, and its readers sample this one. Bump it while still holding
        // this level's gate, and unconditionally: an execute that failed
        // after `LeaseStore::migrate` still moved the committed location.
        self.entity_gates.migrated(rekey.source_grid, rekey.entity);
        outcome
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
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        // Locate with no gate held and prove afterwards that nothing
        // migrated, exactly as the node-level impl does; at this level the
        // "locate" is `committed_entity_cell`, which can walk every runtime.
        let mut marks = Vec::with_capacity(renew.len());
        let mut routes = Vec::with_capacity(renew.len());
        for entry in renew {
            marks.push(self.entity_gates.migration_mark(grid, entry.entity));
            routes.push(
                self.committed_entity_cell(grid, entry.entity)
                    .await?
                    .unwrap_or(entry.cell),
            );
        }
        // Two folds, each on the thing that is actually shared at its level:
        // here the **node** that HRW placement assigns the route cell to, and
        // then, inside that node, the actor that owns it. Grouping by the
        // route cell at this level would send one message per leaf cell to a
        // node that was going to put them all in one mailbox anyway.
        let hasher = RendezvousHasher::new(self.nodes.clone());
        let owners: Vec<Option<u64>> = routes.iter().map(|cell| hasher.owner(*cell)).collect();
        let mut rows = vec![None; renew.len()];
        let mut restale: Vec<usize> = Vec::new();
        for (owner, members) in group_by_route(&owners) {
            let Some(runtime) = owner.and_then(|owner| self.runtimes.get(&owner)) else {
                continue;
            };
            // The cluster gate is held around the delegated call and nothing
            // else. It is *not* held across the locates above, and the node
            // below takes its own gates for its own mailbox turn, so this is
            // a nested hold of two distinct tables, each in stripe order.
            let guards = lock_entity_gates(
                members
                    .iter()
                    .map(|index| self.entity_gates.gate(grid, renew[*index].entity)),
            )
            .await;
            let held = guards.len();
            let _guards = HeldGates {
                guards,
                gates: held,
                started: Instant::now(),
            };
            let mut fresh = Vec::with_capacity(members.len());
            for index in members {
                if self.entity_gates.migration_mark(grid, renew[index].entity) == marks[index] {
                    fresh.push(index);
                } else {
                    restale.push(index);
                }
            }
            if fresh.is_empty() {
                continue;
            }
            // Each entry carries the cell *it* resolved to, so the node's own
            // fold sees the true owning shard per entity rather than one
            // representative cell for the whole group.
            let batch: Vec<_> = fresh
                .iter()
                .map(|index| LeaseRenewal {
                    cell: routes[*index],
                    entity: renew[*index].entity,
                    lease_id: renew[*index].lease_id,
                })
                .collect();
            let renewed = <tokio::sync::Mutex<CellRuntime> as Router>::heartbeat_leases(
                runtime.as_ref(),
                grid,
                holder,
                &batch,
                now_ms,
            )
            .await?;
            for (index, row) in fresh.into_iter().zip(renewed) {
                rows[index] = row;
            }
        }
        for index in restale {
            rows[index] = <Self as Router>::heartbeat_lease(
                self,
                grid,
                renew[index].cell,
                renew[index].entity,
                holder,
                renew[index].lease_id,
                now_ms,
            )
            .await?;
        }
        Ok(rows)
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
        // This one keeps its locate for a reason that is not tidiness.
        //
        // `Cluster::committed_entity_cell` is doing real cross-runtime
        // routing — `record.cell` may name a cell hosted by a *different*
        // runtime, and picking the wrong runtime is not "ask an actor that
        // will reject", it is "ask a node that does not have the shard". The
        // `CellRuntime` change is an *actor-selection* change inside one
        // runtime whose whole safety argument is invariant J over that
        // runtime's own actors; J says nothing about which runtime holds a
        // shard. Applying it here needs its own proof. Do not.
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
