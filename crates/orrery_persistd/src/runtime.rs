//! The cell-actor runtime: spawn actors over shard cells with rendezvous
//! placement, route writes to the owning actor, and recover actors from the
//! journal (§3.4 restart-and-recovery).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use orrery_protocol::{
    CellId, EntityRekey, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind,
};

use crate::actor::{
    self, CellActorHandle, EntityRecord, SnapshotPage, SupersededRow, Tombstone,
    TOMBSTONE_RETENTION_MS,
};
use crate::checkpoint::{CheckpointData, CheckpointStore};
use crate::crc::crc32c;
use crate::fence::{
    ActivationOutcome, FenceFreshnessConfig, FenceFreshnessError, FenceFreshnessMonitor,
    FenceOutcome, FenceRow, FenceStatus, FenceStore, MemFenceStore, ShardActivation,
};
use crate::journal::{Journal, JournalConfig};
use crate::lease::{LeaseMigrate, LeaseStore, MemLeaseStore};
use crate::placement::{RendezvousHasher, RendezvousNode};

pub(crate) const ENTITY_STRIPE_COUNT: usize = 1_024;

pub(crate) struct EntityStripeGates {
    stripes: [Arc<tokio::sync::Mutex<()>>; ENTITY_STRIPE_COUNT],
}

impl Default for EntityStripeGates {
    fn default() -> Self {
        Self {
            stripes: std::array::from_fn(|_| Arc::new(tokio::sync::Mutex::new(()))),
        }
    }
}

impl EntityStripeGates {
    pub(crate) fn gate(&self, grid: GridId, entity: PersistId) -> Arc<tokio::sync::Mutex<()>> {
        let mut mixed = entity.0 ^ u64::from(grid.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^= mixed >> 31;
        let bytes = mixed.to_le_bytes();
        let stripe = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]) & 1_023);
        Arc::clone(&self.stripes[stripe])
    }
}

/// Runtime configuration.
#[derive(Clone)]
pub struct RuntimeConfig {
    /// The shard cells this runtime hosts.
    pub shards: Vec<CellId>,
    /// The grid the shards' `CellId` space belongs to (P-7, D11 §6: storage
    /// cell ids are grid-relative). A nested-grid deployment runs one runtime
    /// (or one set of shards) per grid.
    pub grid: GridId,
    /// Journal configuration.
    pub journal: JournalConfig,
    /// The node id of this runtime instance (for placement/HRW).
    pub node_id: u64,
    /// The epoch to assume for freshly-owned shards.
    pub epoch: Epoch,
    /// The fence store backing `actor/{shard}` rows (D11 §3.4/§3.5).
    ///
    /// Defaults to an in-process [`MemFenceStore`]; a real deployment passes an
    /// FDB-backed store.
    pub fence: std::sync::Arc<dyn FenceStore>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            shards: vec![CellId::ROOT],
            grid: GridId::ROOT,
            journal: JournalConfig::default(),
            node_id: 0,
            epoch: Epoch::new(0),
            fence: std::sync::Arc::new(MemFenceStore::new()),
        }
    }
}

/// A running cell-actor runtime.
pub struct CellRuntime {
    journal: Arc<Journal>,
    actors: HashMap<CellId, CellActorHandle>,
    epoch: Epoch,
    grid: GridId,
    fence: std::sync::Arc<dyn FenceStore>,
    node_id: u64,
    /// Shared registrar durable tier for all actors in this runtime.
    lease_store: Arc<dyn LeaseStore>,
    entity_gates: Arc<EntityStripeGates>,
    /// Join handles of this runtime's actor tasks; drained by [`Self::close`]
    /// (and by [`Self::split`] for the retired parent) so a closed runtime
    /// releases every actor's `Arc<Journal>` — and the journal's file lock —
    /// before returning.
    joins: Arc<actor::ActorJoinSet>,
}

pub(crate) struct CommittedRekeyPlan {
    source: CellActorHandle,
    destination: CellActorHandle,
    lease_store: Arc<dyn LeaseStore>,
    rekey: EntityRekey,
    record: JournalRecord,
    local: bool,
}

impl CommittedRekeyPlan {
    pub(crate) async fn execute(self) -> Result<(), actor::RekeyError> {
        let (transfer, handle) = self
            .source
            .prepare_rekey(self.rekey.clone(), self.record)
            .await?;
        if handle.committed().await.is_err() {
            self.source
                .abort_rekey(self.rekey.entity, self.rekey.expected_lease_id)
                .await;
            return Err(actor::RekeyError::Journal);
        }
        let migration = self
            .lease_store
            .migrate(
                self.rekey.source_grid,
                self.rekey.entity,
                self.rekey.source_cell,
                self.rekey.destination_cell,
                self.rekey.expected_lease_id,
            )
            .await;
        match migration {
            // The rekey record is already durable. Keep the source reservation
            // until restart reconciliation so later source writes cannot replay
            // beside the committed destination image.
            Err(_) => return Err(actor::RekeyError::LeaseStore),
            Ok(LeaseMigrate::Migrated) => {}
            Ok(LeaseMigrate::SourceMismatch {
                actual: Some(actual),
            }) if actual == self.rekey.destination_cell => {}
            Ok(LeaseMigrate::SourceMismatch { .. })
            | Ok(LeaseMigrate::LeaseIdMismatch { .. })
            | Ok(LeaseMigrate::IndexConflict) => {
                return Err(actor::RekeyError::LeaseMigrationRejected);
            }
        }
        if self.local {
            return self.source.complete_local_rekey(transfer).await;
        }
        self.destination.install_rekey(transfer).await?;
        self.source
            .retire_rekey(self.rekey.entity, self.rekey.expected_lease_id)
            .await
    }
}

/// Everything needed to checkpoint one actor without retaining a borrow of
/// the runtime that owns it.
///
/// The scheduler resolves this target while holding its topology mutex, then
/// drops that mutex before asking the actor for a copy-on-update snapshot or
/// awaiting durable storage. The cloned actor handle keeps the snapshot and
/// post-commit tombstone pruning tied to the same actor incarnation. Durable
/// stores still fence the captured `(node_id, epoch)` at commit time.
pub(crate) struct CheckpointTarget {
    handle: CellActorHandle,
    node_id: u64,
}

impl CheckpointTarget {
    /// Capture and durably store this actor's current state.
    pub(crate) async fn checkpoint(
        self,
        store: &dyn CheckpointStore,
    ) -> Result<(), crate::checkpoint::CheckpointError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let snap = self
            .handle
            .checkpoint_snapshot()
            .await
            .map_err(|_| crate::checkpoint::CheckpointError::Store("actor gone".into()))?;
        let superseded = snap.superseded.clone();
        let data = CheckpointData {
            shard: snap.shard,
            grid: snap.grid,
            node_id: self.node_id,
            epoch: snap.epoch,
            watermark: snap.ckpt_watermark,
            entities: snap.entities,
            by_cell: snap.by_cell,
            tombstones: snap.tombstones,
            superseded: snap.superseded,
            taken_at_ms: now,
        };
        store.checkpoint(&data).await?;
        // The store's GC pass cleared the expired tombstone rows (D11 §6,
        // P-6) and the vacated rows this checkpoint carried (P-9); drop both
        // from the same actor incarnation now so the next checkpoint does not
        // rewrite or re-clear them. Only the pairs that travelled with this
        // checkpoint are dropped — the actor may have recorded more since the
        // snapshot, and those are the next checkpoint's work. Safe on
        // failure: an interrupted checkpoint re-runs and clears them again,
        // so a stale actor entry cannot resurrect a row.
        self.handle
            .prune_checkpointed(now, superseded)
            .await
            .map_err(|_| {
                crate::checkpoint::CheckpointError::Store("actor gone after checkpoint".into())
            })?;
        Ok(())
    }
}

impl CellRuntime {
    /// Open the journal and spawn an actor per shard cell, seeded from the
    /// durable tier and recovered from the journal (§3.4).
    ///
    /// Each actor is seeded from its checkpoint in `checkpoints` (the durable
    /// tier is the system of record for bulk state, D11) and then rebuilt
    /// forward by one replay pass per runtime — the checkpoint is the base,
    /// the journal is the delta — rather than from the journal alone. An
    /// actor therefore serves at least what the durable tier holds from the
    /// moment it exists.
    ///
    /// The journal is scanned **once**, in LSN order, and each record is
    /// dispatched to the deepest matching shard (the [`CellRuntime::actor`]
    /// rule). One pass, not `shards × journal`, keeps `open` proportional to
    /// the journal length no matter how the shard set grows.
    ///
    /// The replay predicate is decision C-2 (docs/11-roadmap.md §P2): a
    /// record is dropped iff its epoch is below the **running maximum epoch
    /// seen so far in LSN order** — a superseded-at-write-time zombie — never
    /// merely below the runtime's configured epoch. A node's own journal has
    /// non-decreasing epochs, so a clean restart (even after `fence_shard`
    /// bumped the shard's epoch) replays its whole acked history; comparing
    /// against `config.epoch` would discard that history the moment fencing
    /// is live. Zombie protection comes from the `actor/{shard}` fence CAS,
    /// not from filtering a node's own journal.
    pub async fn open(
        config: &RuntimeConfig,
        checkpoints: &Arc<dyn CheckpointStore>,
    ) -> Result<Self, crate::journal::JournalError> {
        Self::open_with_lease_store(config, checkpoints, Arc::new(MemLeaseStore::new())).await
    }

    /// Open with an explicitly selected durable registrar tier. Production
    /// startup passes [`crate::FdbLeaseStore`] here; tests use the memory tier.
    ///
    /// The returned future is cancellation-safe while loading checkpoints and
    /// reconciling committed rekeys: dropping it drops the in-flight store
    /// future and no recovery worker remains detached.
    pub async fn open_with_lease_store(
        config: &RuntimeConfig,
        checkpoints: &Arc<dyn CheckpointStore>,
        lease_store: Arc<dyn LeaseStore>,
    ) -> Result<Self, crate::journal::JournalError> {
        let journal = Arc::new(Journal::open(&config.journal)?);
        let joins = Arc::new(actor::ActorJoinSet::new());
        // The replay below mints tombstone GC deadlines (P-6); one clock read
        // for the whole replay keeps it consistent.
        let now_ms = now_ms();

        // Seed every shard from the durable tier first, so the replay below
        // folds only the journal tail past each checkpoint's watermark.
        let mut seeds: HashMap<CellId, RecoveredState> = HashMap::new();
        let mut ckpt_coverage: HashMap<CellId, CheckpointCoverage> = HashMap::new();
        let mut ckpt_epochs: HashMap<CellId, Epoch> = HashMap::new();
        for &shard in &config.shards {
            let loaded = checkpoints
                .load(shard, config.grid)
                .await
                .map_err(|error| {
                    crate::journal::JournalError::Store(format!("checkpoint load: {error}"))
                })?;
            if let Some(ckpt) = loaded {
                ckpt_coverage.insert(shard, CheckpointCoverage::from_watermark(ckpt.watermark));
                ckpt_epochs.insert(shard, ckpt.epoch);
                seeds.insert(
                    shard,
                    RecoveredState {
                        state: ckpt.entities,
                        by_cell: ckpt.by_cell,
                        tombstones: ckpt.tombstones,
                        superseded: ckpt.superseded,
                    },
                );
            }
        }

        // One pass over the journal: dispatch each record to its deepest
        // matching shard, tracking each shard's running-maximum epoch as we
        // go (C-2). One pass, not `shards × journal`, keeps `open`
        // proportional to the journal length no matter how the shard set
        // grows.
        let mut gate = ReplayGate::new(config.shards.clone());
        for (&shard, &epoch) in &ckpt_epochs {
            gate.seed(shard, epoch);
        }
        // The freshest journal position folded into each shard's state: the
        // checkpoint watermark advanced by every kept tail record past it.
        // This is the actor's `ckpt_watermark` after open.
        let mut coverage: HashMap<CellId, Lsn> = ckpt_coverage
            .iter()
            .filter_map(|(&shard, covered)| covered.through().map(|through| (shard, through)))
            .collect();
        let stored_records = journal
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()?;
        for stored in stored_records {
            // The journal key's own position, which is not always the record's
            // `lsn`: a mirrored row keeps its origin LSN in the encoded record
            // while taking an independent local key (journal/fjall.rs). The
            // watermark and the coverage below are positions in *this*
            // journal, so they are compared against this one.
            let position = stored.lsn;
            let rec = stored.record;
            // P-7: storage cell ids are grid-relative, so a record from
            // another grid names a different entity universe. The live write
            // path refuses it (`CellRuntime::actor`'s grid guard) and so does
            // the rekey branch below; the plain diff path never did.
            if rec.grid != config.grid {
                continue;
            }
            // Resolve the owning shard *before* verifying the payload. A
            // corrupt record for a shard this node does not host is a fault
            // this node can neither see the consequences of nor repair, and
            // failing `open` on it bricks startup for a shard it was never
            // going to serve.
            let owner = deepest_shard(&config.shards, rec.cell);
            if rec.kind == RecordKind::Rekey {
                verify_crc(&rec)?;
                let rekey = actor::decode_entity_rekey(&rec).map_err(|error| {
                    crate::journal::JournalError::Corrupt {
                        lsn: rec.lsn,
                        msg: error.to_string(),
                    }
                })?;
                if rekey.source_grid != config.grid || rekey.destination_grid != config.grid {
                    return Err(crate::journal::JournalError::Corrupt {
                        lsn: rec.lsn,
                        msg: "rekey crosses an unavailable runtime grid".into(),
                    });
                }
                // `decode_entity_rekey` proved `rec.cell == rekey.source_cell`,
                // so the owner resolved above is the source shard.
                //
                // A rekey naming a shard this node does not host is skipped
                // for the same reason the plain-diff path skips one, and this
                // branch used to be the exception that bricked startup: a
                // follower's journal accumulates the *mirrored* rekeys of
                // every node it replicates, so one cross-node entity move
                // plus a restart failed `open` outright. The live write path
                // only ever emits a rekey when it hosts both actors
                // (`committed_rekey_plan`), so a foreign rekey here is never
                // this node's to replay — and the lease reconciliation below
                // is not its business either.
                let Some(source_shard) = owner else {
                    continue;
                };
                if !gate.admit(source_shard, rec.epoch) {
                    continue;
                }
                recover_rekey(
                    &config.shards,
                    &ckpt_coverage,
                    &mut seeds,
                    &mut coverage,
                    &rekey,
                    position,
                );
                let migration = lease_store
                    .migrate(
                        rekey.source_grid,
                        rekey.entity,
                        rekey.source_cell,
                        rekey.destination_cell,
                        rekey.expected_lease_id,
                    )
                    .await
                    .map_err(|error| crate::journal::JournalError::Store(error.to_string()))?;
                match migration {
                    LeaseMigrate::Migrated => {}
                    LeaseMigrate::SourceMismatch {
                        actual: Some(actual),
                    } if actual == rekey.destination_cell => {}
                    LeaseMigrate::SourceMismatch { actual: None }
                    | LeaseMigrate::SourceMismatch { actual: Some(_) }
                    | LeaseMigrate::LeaseIdMismatch { .. }
                    | LeaseMigrate::IndexConflict => {
                        return Err(crate::journal::JournalError::Store(
                            "committed rekey lease reconciliation rejected".into(),
                        ));
                    }
                }
                continue;
            }
            let Some(shard) = owner else {
                continue;
            };
            verify_crc(&rec)?;
            if !gate.admit(shard, rec.epoch) {
                continue;
            }
            // Records at or below the checkpoint watermark are already folded
            // into the seed; only the tail past it is replayed (§3.4 step 3).
            // [`CheckpointCoverage`] is what keeps "covers nothing" distinct
            // from "covers the record at 0:0" — the first record of a journal
            // sits at exactly the position an empty checkpoint reports.
            let covered = ckpt_coverage
                .get(&shard)
                .copied()
                .unwrap_or(CheckpointCoverage::NONE);
            if !covered.covers(position) {
                let seed = seeds.entry(shard).or_default();
                fold(
                    &mut seed.state,
                    &mut seed.by_cell,
                    &mut seed.tombstones,
                    &mut seed.superseded,
                    &rec,
                    now_ms,
                );
                let covered = coverage.entry(shard).or_insert(Lsn::new(0, 0));
                *covered = (*covered).max(position);
            }
        }

        let mut actors = HashMap::new();
        for &shard in &config.shards {
            let seed = seeds.remove(&shard).unwrap_or_default();
            let watermark = coverage.remove(&shard).unwrap_or(Lsn::new(0, 0));
            actors.insert(
                shard,
                actor::spawn_preloaded(
                    shard,
                    config.grid,
                    config.epoch,
                    Arc::clone(&journal),
                    Arc::clone(&lease_store),
                    seed.state,
                    seed.by_cell,
                    seed.tombstones,
                    seed.superseded,
                    watermark,
                    &joins,
                ),
            );
        }

        Ok(Self {
            journal,
            actors,
            epoch: config.epoch,
            grid: config.grid,
            fence: Arc::clone(&config.fence),
            node_id: config.node_id,
            lease_store,
            entity_gates: Arc::new(EntityStripeGates::default()),
            joins,
        })
    }

    /// The shared journal.
    pub fn journal(&self) -> &Arc<Journal> {
        &self.journal
    }

    /// The number of fsyncs issued by the journal's group committer since
    /// open (§4 adaptive group commit observability).
    pub fn flush_count(&self) -> usize {
        self.journal.flush_count()
    }

    /// The epoch this runtime owns its shards under.
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The shard cells this runtime hosts.
    pub fn shards(&self) -> impl Iterator<Item = &CellId> {
        self.actors.keys()
    }

    /// The fence store backing `actor/{shard}` rows.
    pub fn fence(&self) -> &std::sync::Arc<dyn FenceStore> {
        &self.fence
    }

    /// The grid this runtime's shards live in (P-7: storage cell ids are
    /// grid-relative, and the actors write rows under this grid).
    pub fn grid(&self) -> GridId {
        self.grid
    }

    /// Ask every live actor to park registrar rows whose monotonic TTL passed.
    pub async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<crate::lease::ParkedLease> {
        let mut parked = Vec::new();
        for actor in self.actors.values() {
            if let Ok(rows) = actor.sweep_leases(now_ms).await {
                parked.extend(rows);
            }
        }
        parked
    }

    /// Read one registrar row, its committed cell, and its uplink watermark.
    ///
    /// Routed through the durable location index, so a post-split or post-rekey
    /// entity is inspected on the actor that actually owns its row.
    pub async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<(Option<orrery_protocol::Lease>, Option<CellId>, Option<Lsn>), actor::Reject> {
        let Some(cell) = self.lease_location(entity).await? else {
            return Ok((None, None, None));
        };
        let Some(handle) = self.actor(grid, cell) else {
            return Ok((None, Some(cell), None));
        };
        handle.inspect_lease(entity).await
    }

    /// Find an entity's durable registrar location.
    ///
    /// Claim routing uses this before choosing an actor so a post-split claim
    /// cannot create a second row in a sibling shard.
    pub async fn lease_location(&self, entity: PersistId) -> Result<Option<CellId>, actor::Reject> {
        self.lease_store
            .locate(self.grid, entity)
            .await
            .map_err(|_| actor::Reject::LeaseStore)
    }

    pub(crate) fn entity_gate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.entity_gates.gate(grid, entity)
    }

    pub(crate) fn lease_store_handle(&self) -> Arc<dyn LeaseStore> {
        Arc::clone(&self.lease_store)
    }

    /// Start a bounded-staleness monitor for the exact active fence rows this
    /// runtime currently hosts. Inject the returned monitor as the gateway's
    /// bulk-ack admission policy before exposing the gateway after activation.
    ///
    /// The rows are read before the monitor starts; if another owner changes a
    /// row in that small interval, the monitor detects the mismatch on its
    /// immediate first poll and bulk acknowledgements become provisional.
    pub async fn start_fence_freshness_monitor(
        &self,
        config: FenceFreshnessConfig,
    ) -> Result<Arc<FenceFreshnessMonitor>, FenceFreshnessError> {
        let mut rows = Vec::with_capacity(self.actors.len());
        for &shard in self.actors.keys() {
            let Some(row) = self
                .fence
                .read(self.grid, shard)
                .await
                .map_err(|error| FenceFreshnessError::FenceRead(error.to_string()))?
            else {
                return Err(FenceFreshnessError::InactiveShard(shard));
            };
            if row.status != FenceStatus::Active || row.owner != self.node_id {
                return Err(FenceFreshnessError::InactiveShard(shard));
            }
            rows.push((shard, row));
        }
        FenceFreshnessMonitor::start(Arc::clone(&self.fence), self.grid, rows, config)
    }

    /// The actor owning `cell` in `grid`: the **deepest** shard actor whose
    /// subtree contains `cell` (an exact match when `cell` is itself a shard;
    /// otherwise the shard containing that interest cell). The deepest match
    /// matters because the root is a prefix of every cell — routing must pick
    /// the most specific shard, not an arbitrary one.
    ///
    /// The `grid` guard is the P-7 corollary: storage cell ids are
    /// grid-relative, so the same raw cell under a different grid is a
    /// different entity universe — this runtime must never serve it.
    pub fn actor(&self, grid: GridId, cell: CellId) -> Option<&CellActorHandle> {
        if grid != self.grid {
            return None;
        }
        self.actors
            .iter()
            .filter(|(shard, _)| shard.is_prefix_of(cell))
            .max_by_key(|(shard, _)| shard.level())
            .map(|(_, handle)| handle)
    }

    /// Clone every live actor handle, so a caller can await their mailboxes
    /// without holding a lock on the runtime itself.
    #[must_use]
    pub fn actor_handles(&self) -> Vec<CellActorHandle> {
        self.actors.values().cloned().collect()
    }

    /// Fence shard `S` for this node: CAS `actor/{S}` from `expected` to
    /// `(self, e+1)` and, on success, spawn the actor at the new epoch (§3.4
    /// step 1).
    ///
    /// On success the new actor is seeded from the durable tier's checkpoint
    /// (`checkpoints`) before serving — the checkpoint is the base, the live
    /// journal stream the delta — so the fenced-in actor never serves less
    /// than the durable tier holds.
    ///
    /// Returns the new epoch on success, or a [`FenceError::Conflict`] with
    /// the live row if the CAS preconditions do not hold.
    pub async fn fence_shard(
        &mut self,
        shard: CellId,
        expected: Option<&FenceRow>,
        checkpoints: &dyn CheckpointStore,
    ) -> Result<Epoch, crate::fence::FenceError> {
        let new_epoch = Epoch::new(expected.map_or(0, |r| r.epoch.0) + 1);
        let new = FenceRow {
            owner: self.node_id,
            epoch: new_epoch,
            status: FenceStatus::Active,
        };
        match self.fence.fence(self.grid, shard, expected, &new).await {
            Ok(FenceOutcome::Fenced) => {
                let ckpt = checkpoints
                    .load(shard, self.grid)
                    .await
                    .map_err(|e| crate::fence::FenceError::Store(e.to_string()))?;
                // The seed is the *fresher* of the two sources of truth:
                //
                //  * the outgoing actor's live state (an `open`-spawned actor
                //    was already journal-recovered, so it is ≥ the checkpoint
                //    in freshness), and
                //  * the durable-tier checkpoint (the shard's base when the
                //    outgoing actor is absent).
                //
                // Whichever has further coverage wins; the durable tier is
                // the base the checkpoint guarantees, the live actor the tail
                // it may not yet have checkpointed.
                let (mut state, mut by_cell, mut tombstones, mut superseded, mut watermark) = ckpt
                    .map_or_else(
                        || {
                            (
                                HashMap::new(),
                                HashMap::new(),
                                HashMap::new(),
                                HashSet::new(),
                                Lsn::new(0, 0),
                            )
                        },
                        |c| {
                            (
                                c.entities,
                                c.by_cell,
                                c.tombstones,
                                c.superseded,
                                c.watermark,
                            )
                        },
                    );
                if let Some(old) = self.actors.remove(&shard) {
                    if let Ok(snap) = old.checkpoint_snapshot().await {
                        if snap.ckpt_watermark >= watermark {
                            state = snap.entities;
                            by_cell = snap.by_cell;
                            tombstones = snap.tombstones;
                            superseded = snap.superseded;
                            watermark = snap.ckpt_watermark;
                        }
                    }
                    old.shutdown().await;
                }
                self.actors.insert(
                    shard,
                    actor::spawn_preloaded(
                        shard,
                        self.grid,
                        new_epoch,
                        Arc::clone(&self.journal),
                        Arc::clone(&self.lease_store),
                        state,
                        by_cell,
                        tombstones,
                        superseded,
                        watermark,
                        &self.joins,
                    ),
                );
                Ok(new_epoch)
            }
            Ok(FenceOutcome::Conflict { current }) => {
                Err(crate::fence::FenceError::Conflict { current })
            }
            Err(e) => Err(e),
        }
    }

    /// Atomically activate a canonical shard set for this node.
    ///
    /// This is the ownership hand-off entry point for bootstrap, restart, and
    /// follower promotion. The durable fence transition happens before any
    /// local actor is replaced, so a stale expected row cannot leave a
    /// half-activated local runtime. Callers must not expose a gateway until
    /// this returns [`ActivationOutcome::Activated`].
    pub async fn activate_shards(
        &mut self,
        requests: &[ShardActivation],
        checkpoints: &dyn CheckpointStore,
    ) -> Result<ActivationOutcome, crate::fence::FenceError> {
        let outcome = self
            .fence
            .activate_shards(self.grid, self.node_id, requests)
            .await?;
        let ActivationOutcome::Activated { rows } = &outcome else {
            return Ok(outcome);
        };

        // The committed rows are the sole authority for the local epoch. Only
        // after the whole set committed do we replace actors. Existing actors
        // supply the freshest journal-recovered state; an absent actor starts
        // from its durable checkpoint.
        for (shard, row) in rows {
            let ckpt = checkpoints
                .load(*shard, self.grid)
                .await
                .map_err(|e| crate::fence::FenceError::Store(e.to_string()))?;
            let (mut state, mut by_cell, mut tombstones, mut superseded, mut watermark) = ckpt
                .map_or_else(
                    || {
                        (
                            HashMap::new(),
                            HashMap::new(),
                            HashMap::new(),
                            HashSet::new(),
                            Lsn::new(0, 0),
                        )
                    },
                    |c| {
                        (
                            c.entities,
                            c.by_cell,
                            c.tombstones,
                            c.superseded,
                            c.watermark,
                        )
                    },
                );
            if let Some(old) = self.actors.remove(shard) {
                if let Ok(snapshot) = old.checkpoint_snapshot().await {
                    if snapshot.ckpt_watermark >= watermark {
                        state = snapshot.entities;
                        by_cell = snapshot.by_cell;
                        tombstones = snapshot.tombstones;
                        superseded = snapshot.superseded;
                        watermark = snapshot.ckpt_watermark;
                    }
                }
                old.shutdown().await;
            }
            self.actors.insert(
                *shard,
                actor::spawn_preloaded(
                    *shard,
                    self.grid,
                    row.epoch,
                    Arc::clone(&self.journal),
                    Arc::clone(&self.lease_store),
                    state,
                    by_cell,
                    tombstones,
                    superseded,
                    watermark,
                    &self.joins,
                ),
            );
        }
        self.epoch = rows
            .iter()
            .map(|(_, row)| row.epoch)
            .max()
            .unwrap_or(self.epoch);
        Ok(outcome)
    }

    /// Split a hot shard into its eight children (§3.5).
    ///
    /// 1. Partition the parent actor's state by child cell.
    /// 2. CAS `actor/{parent}` from `parent_row` to `Splitting` and write the
    ///    eight child rows at epoch `e+1` in one transaction.
    /// 3. Spawn the child actors preloaded with their partition, retire the
    ///    parent row, and drop the parent actor.
    ///
    /// Returns the child rows on success, or a [`FenceError::Conflict`] with
    /// the live parent row if the CAS preconditions do not hold.
    pub async fn split(
        &mut self,
        parent: CellId,
        parent_row: &FenceRow,
    ) -> Result<Vec<(CellId, FenceRow)>, crate::fence::FenceError> {
        let handle = self
            .actors
            .get(&parent)
            .ok_or_else(|| crate::fence::FenceError::Store("no actor for shard".into()))?;
        let snap = handle
            .split_snapshot()
            .await
            .map_err(|_| crate::fence::FenceError::Store("actor gone during split".into()))?;

        let new_epoch = Epoch::new(parent_row.epoch.0 + 1);
        let children = parent.children();
        let child_rows: Vec<(CellId, FenceRow)> = children
            .iter()
            .map(|&c| {
                (
                    c,
                    FenceRow {
                        owner: self.node_id,
                        epoch: new_epoch,
                        status: FenceStatus::Active,
                    },
                )
            })
            .collect();

        match self
            .fence
            .begin_split(self.grid, parent, parent_row, &child_rows)
            .await
        {
            Ok(FenceOutcome::Fenced) => {}
            Ok(FenceOutcome::Conflict { current }) => {
                return Err(crate::fence::FenceError::Conflict { current });
            }
            Err(e) => return Err(e),
        }

        // Spawn child actors preloaded with their partition.
        for (child, _) in &child_rows {
            let partition = snap.children.get(child).cloned().unwrap_or_default();
            let child_by_cell = snap.by_cell.get(child).cloned().unwrap_or_default();
            let child_tombstones = snap.tombstones.get(child).cloned().unwrap_or_default();
            // The parent's pending row clears travel with the partition: a
            // split never touches the parent's durable rows, so a vacated key
            // left behind here would become a ghost no later checkpoint can
            // reach (§3.5, P-9).
            let child_superseded = snap.superseded.get(child).cloned().unwrap_or_default();
            self.actors.insert(
                *child,
                actor::spawn_preloaded(
                    *child,
                    self.grid,
                    new_epoch,
                    Arc::clone(&self.journal),
                    Arc::clone(&self.lease_store),
                    partition,
                    child_by_cell,
                    child_tombstones,
                    child_superseded,
                    snap.ckpt_watermark,
                    &self.joins,
                ),
            );
        }

        // Retire the parent row and drop the parent actor.
        let _ = self.fence.retire(self.grid, parent).await;
        if let Some(old) = self.actors.remove(&parent) {
            old.shutdown().await;
        }

        Ok(child_rows)
    }

    /// Apply a diff to the actor owning its cell.
    pub async fn apply(&self, record: JournalRecord) -> Result<Lsn, actor::Reject> {
        let handle = self
            .actor(record.grid, record.cell)
            .ok_or(actor::Reject::JournalClosed)?;
        handle.apply_diff(record).await
    }

    pub(crate) fn committed_rekey_plan(
        &self,
        record: JournalRecord,
    ) -> Result<CommittedRekeyPlan, actor::RekeyError> {
        let rekey = actor::decode_entity_rekey(&record)?;
        if rekey.source_grid != self.grid || rekey.destination_grid != self.grid {
            return Err(actor::RekeyError::ActorUnavailable);
        }
        let source = self
            .actor(rekey.source_grid, rekey.source_cell)
            .cloned()
            .ok_or(actor::RekeyError::ActorUnavailable)?;
        let destination = self
            .actor(rekey.destination_grid, rekey.destination_cell)
            .cloned()
            .ok_or(actor::RekeyError::ActorUnavailable)?;
        Ok(CommittedRekeyPlan {
            local: source.same_actor(&destination),
            source,
            destination,
            lease_store: Arc::clone(&self.lease_store),
            rekey,
            record,
        })
    }

    /// Commit a server-owned storage rekey across the source and destination actors.
    pub async fn commit_rekey(&self, record: JournalRecord) -> Result<(), actor::RekeyError> {
        let rekey = actor::decode_entity_rekey(&record)?;
        let gate = self.entity_gate(rekey.source_grid, rekey.entity);
        let _guard = gate.lock_owned().await;
        self.committed_rekey_plan(record)?.execute().await
    }

    /// Read a snapshot from the actor owning `cell` in `grid` (P-7: the grid
    /// scopes which universe the cell id names).
    pub async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, actor::Reject> {
        let handle = self.actor(grid, cell).ok_or(actor::Reject::JournalClosed)?;
        handle.read_snapshot(vec![cell]).await
    }

    /// Capture a copy-on-update snapshot of the actor owning `cell`
    /// (entities, per-entity cells, and despawn markers).
    pub async fn actor_snapshot(
        &self,
        cell: CellId,
    ) -> Result<crate::actor::CheckpointSnapshot, actor::Reject> {
        let handle = self
            .actor(self.grid, cell)
            .ok_or(actor::Reject::JournalClosed)?;
        handle.checkpoint_snapshot().await
    }

    /// Write a checkpoint of every actor's current state to `store` (§8).
    ///
    /// The watermark recorded is the actor's `ckpt_watermark` — the LSN covered
    /// by the last checkpoint. The checkpoint is taken copy-on-update: the
    /// snapshot is cloned inside the actor's mailbox, so it does not block
    /// concurrent appends.
    pub async fn checkpoint(
        &self,
        store: &dyn CheckpointStore,
    ) -> Result<(), crate::checkpoint::CheckpointError> {
        for shard in self.actors.keys().copied().collect::<Vec<_>>() {
            self.checkpoint_shard(shard, store).await?;
        }
        Ok(())
    }

    /// Write a checkpoint of the actor owning `shard` to `store` (§8).
    ///
    /// Used by the checkpoint scheduler's per-shard jittered cadence and by
    /// quiesce-flush. Copy-on-update: the snapshot is cloned inside the actor's
    /// mailbox, so it does not block concurrent appends.
    pub async fn checkpoint_shard(
        &self,
        shard: CellId,
        store: &dyn CheckpointStore,
    ) -> Result<(), crate::checkpoint::CheckpointError> {
        self.checkpoint_target(shard)?.checkpoint(store).await
    }

    /// Resolve a shard checkpoint to a cloneable actor target.
    ///
    /// This method performs no actor or storage await. Callers that protect a
    /// runtime with a topology mutex can therefore resolve the target under
    /// that mutex and release it before [`CheckpointTarget::checkpoint`].
    pub(crate) fn checkpoint_target(
        &self,
        shard: CellId,
    ) -> Result<CheckpointTarget, crate::checkpoint::CheckpointError> {
        let handle = self.actor(self.grid, shard).cloned().ok_or_else(|| {
            crate::checkpoint::CheckpointError::Store(format!("no actor for shard {shard}"))
        })?;
        Ok(CheckpointTarget {
            handle,
            node_id: self.node_id,
        })
    }

    /// Restore an actor's state from `store`, then replay the journal tail.
    ///
    /// §3.4 step 3: load the checkpoint (watermark `W`), then replay journal
    /// records with `lsn > W` for this shard — so acked writes after the last
    /// checkpoint are recovered and the pre-checkpoint history, already folded
    /// into the checkpoint, is never replayed a second time. Zero-loss by
    /// construction, bounded by construction: the replay is the tail, not the
    /// whole journal.
    ///
    /// The epoch predicate is decision C-2 (docs/11-roadmap.md §P2), and it is
    /// literally [`CellRuntime::open`]'s: both drive [`ReplayGate`], so the
    /// dispatch rule (deepest hosted shard, not any prefix match) and the
    /// order of the epoch and watermark filters cannot drift apart again. The
    /// gate is seeded from the checkpoint's epoch precisely because this scan
    /// starts at the watermark and cannot see the records `open` folds before
    /// it — see [`ReplayGate::seed`].
    ///
    /// Note this method has no production caller: startup goes through
    /// [`CellRuntime::open_with_lease_store`], which recovers as it opens. It
    /// is a public entry point for tests and tooling, so its predicate still
    /// has to be the real one.
    ///
    /// Returns the number of journal records replayed.
    pub async fn restore(
        &self,
        shard: CellId,
        store: &dyn CheckpointStore,
    ) -> Result<usize, crate::checkpoint::CheckpointError> {
        let handle = self.actor(self.grid, shard).ok_or_else(|| {
            crate::checkpoint::CheckpointError::Store("no actor for shard".into())
        })?;

        // Load the checkpoint and fold its entity bag and despawn markers
        // into the actor. Its coverage bounds the replay below; a shard with
        // no checkpoint — or one whose watermark covers nothing — has the
        // whole journal as its tail.
        let mut gate = ReplayGate::new(self.actors.keys().copied().collect());
        let mut covered = CheckpointCoverage::NONE;
        if let Some(ckpt) = store.load(shard, self.grid).await? {
            covered = CheckpointCoverage::from_watermark(ckpt.watermark);
            gate.seed(shard, ckpt.epoch);
            handle
                .restore_entities(
                    ckpt.entities,
                    ckpt.by_cell,
                    ckpt.tombstones,
                    ckpt.superseded,
                )
                .await
                .map_err(|_| {
                    crate::checkpoint::CheckpointError::Store("actor gone during restore".into())
                })?;
            handle.set_watermark(ckpt.watermark).await.map_err(|_| {
                crate::checkpoint::CheckpointError::Store("actor gone during restore".into())
            })?;
        }

        // Replay the journal tail, tracking the running maximum epoch (C-2)
        // and re-verifying crc. Start the scan at the covered position (the
        // [brief-mandated `scan_from(watermark)`](docs/08-persistence.md §3.4)
        // bounds the read); the coverage filter below then keeps the tail past
        // it. A checkpoint that covers nothing — none at all, or one whose
        // watermark is the 0:0 sentinel — scans and replays the whole journal,
        // first record included.
        let scan_from = covered.through().unwrap_or(Lsn::new(0, 0));
        let mut replayed = 0usize;
        for item in self.journal.scan_from(scan_from) {
            let stored =
                item.map_err(|e| crate::checkpoint::CheckpointError::Store(format!("{e}")))?;
            // The local journal position, not the record's own `lsn`, for the
            // same reason `open` uses it: a mirrored row's encoded LSN belongs
            // to the origin's journal, and the watermark is a position here.
            let position = stored.lsn;
            let rec = &stored.record;
            if rec.grid != self.grid {
                continue;
            }
            // Same dispatch as `open`: the deepest hosted shard owning the
            // record's own cell. For a rekey that is the source shard, which
            // is also the shard whose epoch gates it.
            let Some(owner) = gate.owner(rec.cell) else {
                continue;
            };
            verify_crc(rec)
                .map_err(|e| crate::checkpoint::CheckpointError::Store(format!("{e}")))?;
            // The gate is advanced for every record `open` would advance it
            // for, not only the ones this shard folds — otherwise a rekey
            // arriving from another shard would be judged against a running
            // maximum that had never seen that shard's writes.
            if !gate.admit(owner, rec.epoch) {
                continue;
            }
            let rekey = if rec.kind == RecordKind::Rekey {
                Some(actor::decode_entity_rekey(rec).map_err(|error| {
                    crate::checkpoint::CheckpointError::Store(error.to_string())
                })?)
            } else {
                None
            };
            let relevant = rekey.as_ref().map_or(owner == shard, |rekey| {
                owner == shard || gate.owner(rekey.destination_cell) == Some(shard)
            });
            if !relevant {
                continue;
            }
            if covered.covers(position) {
                continue;
            }
            if let Some(rekey) = rekey {
                let migration = self
                    .lease_store
                    .migrate(
                        rekey.source_grid,
                        rekey.entity,
                        rekey.source_cell,
                        rekey.destination_cell,
                        rekey.expected_lease_id,
                    )
                    .await
                    .map_err(|error| {
                        crate::checkpoint::CheckpointError::Store(error.to_string())
                    })?;
                match migration {
                    LeaseMigrate::Migrated => {}
                    LeaseMigrate::SourceMismatch {
                        actual: Some(actual),
                    } if actual == rekey.destination_cell => {}
                    LeaseMigrate::SourceMismatch { .. }
                    | LeaseMigrate::LeaseIdMismatch { .. }
                    | LeaseMigrate::IndexConflict => {
                        return Err(crate::checkpoint::CheckpointError::Store(
                            "committed rekey lease reconciliation rejected".into(),
                        ));
                    }
                }
            }
            handle.restore_apply(rec.clone()).await.map_err(|_| {
                crate::checkpoint::CheckpointError::Store("actor gone during replay".into())
            })?;
            replayed += 1;
        }
        Ok(replayed)
    }

    /// Stop all actors, then close the journal (flush + stop the committer).
    ///
    /// Consumes the runtime so the actor tasks (and their `Arc<Journal>`) are
    /// awaited and dropped, releasing the journal file lock before returning —
    /// required before reopening the same journal dir.
    pub async fn close(self) -> Result<(), crate::journal::JournalError> {
        for (_, handle) in self.actors.into_iter() {
            handle.shutdown().await;
        }
        self.joins.join_all().await;
        self.journal.close().await
    }

    /// Compute the HRW owner of `cell` over a node set.
    pub fn placement_owner(nodes: &[RendezvousNode], cell: CellId) -> Option<u64> {
        RendezvousHasher::new(nodes.to_vec()).owner(cell)
    }
}

/// One shard's recovered hot state (entity bag, per-entity cells, despawn
/// markers, pending row clears) — the checkpoint seed folded with the replayed
/// journal tail.
#[derive(Default)]
struct RecoveredState {
    state: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, CellId>,
    tombstones: HashMap<PersistId, Tombstone>,
    superseded: HashSet<SupersededRow>,
}

/// How much of *this node's* journal a loaded checkpoint already accounts for.
///
/// The durable `ckpt/{shard}` row stores the watermark as a bare [`Lsn`]
/// (docs/08-persistence.md §6), an **inclusive** upper bound: recovery replays
/// `lsn > watermark`. That encoding has no value for "covers nothing" — and
/// `0:0`, the value every empty checkpoint carries, is also a perfectly valid
/// record position, because a fresh journal opens at `next_lsn = 0:0` and
/// `append` stamps that onto the first record it ever stores
/// (`journal/fjall.rs::next_lsn_after`). Read literally, a `0:0` watermark
/// therefore swallows the first record of the journal: a checkpoint taken
/// before the first append, and the row-only load `FdbCheckpointStore` (a
/// shard seeded by `orrery-seed`, a split child, a lost meta row) synthesises
/// at `0:0`, both claim coverage they do not have.
///
/// This type is the read-side adjudication of that ambiguity, and it resolves
/// it in the only direction that cannot lose data: **`0:0` means "covers
/// nothing"**, so position `0:0` is always replayed.
///
/// The cost of that choice is bounded and one-sided. When a `0:0` watermark
/// really did cover the record at `0:0` — a checkpoint taken after exactly one
/// append — this replays that one record a second time, which is a no-op:
/// replay is a *state-replacing* fold ([`fold`] assigns `entry.components`, it
/// never accumulates a delta), so re-folding a covered record re-derives the
/// same state. The converse mistake is unbounded: the record is gone for good.
///
/// **Why not change what is stored.** Making the watermark an `Option<Lsn>`
/// (or an exclusive bound) would be an at-rest format change to `ckpt/{shard}`
/// governed by docs/08-persistence.md §16, which versions *component* payloads
/// and journal/archive records — it has no scheme for the checkpoint meta row
/// itself, so every existing checkpoint would have to be re-read under a rule
/// that is not written down. Keeping the at-rest bytes exactly as they are and
/// fixing the *interpretation* costs nothing at rest: an existing checkpoint
/// with a non-zero watermark behaves identically, and one with a `0:0`
/// watermark replays at most one extra, idempotent record.
///
/// The complementary fix — never assigning `0:0` to a record, so the sentinel
/// stops colliding with a real position — belongs to the journal
/// (`journal/fjall.rs`) and would not retire this rule anyway: journals that
/// already hold a record at `0:0` outlive the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckpointCoverage(Option<Lsn>);

impl CheckpointCoverage {
    /// A shard with no checkpoint at all: the whole journal is the tail.
    const NONE: Self = Self(None);

    /// Interpret a durable watermark, mapping the `0:0` sentinel to "covers
    /// nothing" per the type docs.
    fn from_watermark(watermark: Lsn) -> Self {
        if watermark == Lsn::new(0, 0) {
            Self::NONE
        } else {
            Self(Some(watermark))
        }
    }

    /// Whether the checkpoint already folded the record at `position`, i.e.
    /// whether replay must skip it (§3.4 step 3).
    fn covers(self, position: Lsn) -> bool {
        self.0.is_some_and(|through| position <= through)
    }

    /// The last position known covered, if any — the lower bound a `scan_from`
    /// may start at without skipping a record that still has to be replayed.
    fn through(self) -> Option<Lsn> {
        self.0
    }
}

/// The C-2 replay predicate (docs/11-roadmap.md §P2), in one place.
///
/// Both recovery paths — [`CellRuntime::open`] and [`CellRuntime::restore`] —
/// drive this, because they used to implement C-2 twice and the two copies had
/// drifted: `open` advanced the running maximum before the watermark filter
/// while `restore` skipped at the watermark first, and `open` dispatched by
/// deepest shard while `restore` matched on `is_prefix_of`. Under a nested
/// shard set that second difference alone made them fold different records —
/// a record inside a child shard is a prefix match for its parent, but the
/// parent is not the actor that owns it.
///
/// The predicate is per shard, never global: a node's own journal has
/// non-decreasing epochs per shard, and a record is a zombie only relative to
/// the shard whose ownership it claims.
struct ReplayGate {
    shards: Vec<CellId>,
    max_epoch: HashMap<CellId, Epoch>,
}

impl ReplayGate {
    fn new(shards: Vec<CellId>) -> Self {
        Self {
            shards,
            max_epoch: HashMap::new(),
        }
    }

    /// Seed a shard's running maximum from the epoch its checkpoint was taken
    /// under.
    ///
    /// This is what lets `restore` reach `open`'s answer at all. `restore`
    /// scans from the watermark, so it structurally cannot see the
    /// pre-watermark records whose epochs `open` folds into the running
    /// maximum before it ever reaches the tail. The checkpoint's own epoch is
    /// precisely the maximum those records left behind — it is the epoch the
    /// owning actor held when it wrote them — so seeding from it reconstructs
    /// the state of the gate at the watermark instead of restarting it at
    /// zero and admitting a zombie the tail happens to contain.
    fn seed(&mut self, shard: CellId, epoch: Epoch) {
        let entry = self.max_epoch.entry(shard).or_insert(Epoch::new(0));
        *entry = (*entry).max(epoch);
    }

    /// The shard that owns `cell`: the deepest hosted shard whose subtree
    /// contains it, the same most-specific rule [`CellRuntime::actor`] routes
    /// live writes by.
    fn owner(&self, cell: CellId) -> Option<CellId> {
        deepest_shard(&self.shards, cell)
    }

    /// C-2: admit `epoch` for `shard` unless a strictly higher epoch was
    /// already seen at a lower LSN (a zombie write from a superseded ownership
    /// epoch), advancing the running maximum when it is admitted.
    fn admit(&mut self, shard: CellId, epoch: Epoch) -> bool {
        let max_seen = self.max_epoch.entry(shard).or_insert(Epoch::new(0));
        if epoch < *max_seen {
            return false;
        }
        *max_seen = (*max_seen).max(epoch);
        true
    }
}

/// The deepest shard in `shards` whose subtree contains `cell` — the same
/// most-specific-match rule [`CellRuntime::actor`] routes by — or `None` if
/// no shard covers the cell.
fn deepest_shard(shards: &[CellId], cell: CellId) -> Option<CellId> {
    shards
        .iter()
        .filter(|shard| shard.is_prefix_of(cell))
        .max_by_key(|shard| shard.level())
        .copied()
}

/// Fold one committed rekey into the shards this node hosts.
///
/// Each half is applied independently, because a node need not host both.
/// Requiring the destination — which is what this used to do — bricked `open`
/// on any journal holding a rekey out of this node's shard set, and a
/// follower's journal holds exactly those. An unhosted half is nothing this
/// node can seed, repair, or serve, so it is skipped rather than raised.
fn recover_rekey(
    shards: &[CellId],
    checkpoint_coverage: &HashMap<CellId, CheckpointCoverage>,
    seeds: &mut HashMap<CellId, RecoveredState>,
    coverage: &mut HashMap<CellId, Lsn>,
    rekey: &EntityRekey,
    lsn: Lsn,
) {
    if let Some(source) = deepest_shard(shards, rekey.source_cell) {
        if !checkpoint_coverage
            .get(&source)
            .copied()
            .unwrap_or(CheckpointCoverage::NONE)
            .covers(lsn)
        {
            let seed = seeds.entry(source).or_default();
            seed.state.remove(&rekey.entity);
            // The source keeps no tombstone (the entity is alive in the
            // destination shard), so its vacated row is only ever cleared because
            // the recovery records it here, exactly as the live path does.
            actor::note_row_moved(&mut seed.superseded, &seed.by_cell, rekey.entity, None);
            seed.by_cell.remove(&rekey.entity);
            actor::cancel_tombstone(
                &mut seed.superseded,
                &mut seed.tombstones,
                rekey.entity,
                None,
            );
            coverage
                .entry(source)
                .and_modify(|covered| *covered = (*covered).max(lsn))
                .or_insert(lsn);
        }
    }
    if let Some(destination) = deepest_shard(shards, rekey.destination_cell) {
        if !checkpoint_coverage
            .get(&destination)
            .copied()
            .unwrap_or(CheckpointCoverage::NONE)
            .covers(lsn)
        {
            let seed = seeds.entry(destination).or_default();
            seed.state.insert(
                rekey.entity,
                EntityRecord {
                    components: rekey.source_record.clone(),
                    dirty: true,
                },
            );
            actor::note_row_moved(
                &mut seed.superseded,
                &seed.by_cell,
                rekey.entity,
                Some(rekey.destination_cell),
            );
            seed.by_cell.insert(rekey.entity, rekey.destination_cell);
            actor::cancel_tombstone(
                &mut seed.superseded,
                &mut seed.tombstones,
                rekey.entity,
                Some(rekey.destination_cell),
            );
            coverage
                .entry(destination)
                .and_modify(|covered| *covered = (*covered).max(lsn))
                .or_insert(lsn);
        }
    }
}

/// Re-verify a record's payload crc (§4.1 replay integrity).
fn verify_crc(record: &JournalRecord) -> Result<(), crate::journal::JournalError> {
    let actual = crc32c(&record.payload);
    if actual != record.crc {
        return Err(crate::journal::JournalError::Corrupt {
            lsn: record.lsn,
            msg: format!(
                "crc mismatch: stored {:#010x}, computed {:#010x}",
                record.crc, actual
            ),
        });
    }
    Ok(())
}

/// The current wall-clock time as unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Fold a record into an entity map, tombstone set, and superseded-row set
/// (last-writer-wins per entity). Mirrors the actor's fold logic for the
/// `open`-time replay, which runs before any actor exists; `now_ms` seeds the
/// despawn markers' GC deadlines (P-6), and the superseded set carries the
/// vacated `world/` keys the first checkpoint after recovery must clear
/// (P-9) — which is also why the set need not be durable: replay re-derives
/// it from the same records that created it.
fn fold(
    state: &mut HashMap<PersistId, EntityRecord>,
    by_cell: &mut HashMap<PersistId, CellId>,
    tombstones: &mut HashMap<PersistId, Tombstone>,
    superseded: &mut HashSet<SupersededRow>,
    record: &JournalRecord,
    now_ms: u64,
) {
    match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff => {
            let entry = state.entry(record.entity).or_default();
            entry.components = record.payload.clone();
            entry.dirty = true;
            actor::note_row_moved(superseded, by_cell, record.entity, Some(record.cell));
            by_cell.insert(record.entity, record.cell);
            actor::cancel_tombstone(superseded, tombstones, record.entity, Some(record.cell));
        }
        RecordKind::Despawn => {
            state.remove(&record.entity);
            actor::note_row_moved(superseded, by_cell, record.entity, Some(record.cell));
            by_cell.remove(&record.entity);
            tombstones.insert(
                record.entity,
                Tombstone {
                    cell: record.cell,
                    tick: record.tick,
                    gc_deadline_ms: now_ms + TOMBSTONE_RETENTION_MS,
                },
            );
        }
        RecordKind::TerrainDelta | RecordKind::Rekey | RecordKind::CheckpointMark => {}
    }
}

/// Compute the CRC for a record's payload (used by the test/synthetic writers).
pub fn payload_crc(payload: &[u8]) -> u32 {
    crc32c(payload)
}
