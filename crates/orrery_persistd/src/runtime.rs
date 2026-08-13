//! The cell-actor runtime: spawn actors over shard cells with rendezvous
//! placement, route writes to the owning actor, and recover actors from the
//! journal (§3.4 restart-and-recovery).

use std::collections::HashMap;
use std::sync::Arc;

use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind};

use crate::actor::{
    self, CellActorHandle, EntityRecord, SnapshotPage, Tombstone, TOMBSTONE_RETENTION_MS,
};
use crate::checkpoint::{CheckpointData, CheckpointStore};
use crate::crc::crc32c;
use crate::fence::{FenceOutcome, FenceRow, FenceStatus, FenceStore, MemFenceStore};
use crate::journal::{Journal, JournalConfig};
use crate::placement::{RendezvousHasher, RendezvousNode};

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
    /// Join handles of this runtime's actor tasks; drained by [`Self::close`]
    /// (and by [`Self::split`] for the retired parent) so a closed runtime
    /// releases every actor's `Arc<Journal>` — and the journal's file lock —
    /// before returning.
    joins: Arc<actor::ActorJoinSet>,
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
    pub fn open(
        config: &RuntimeConfig,
        checkpoints: &Arc<dyn CheckpointStore>,
    ) -> Result<Self, crate::journal::JournalError> {
        let journal = Arc::new(Journal::open(&config.journal)?);
        let joins = Arc::new(actor::ActorJoinSet::new());
        // The replay below mints tombstone GC deadlines (P-6); one clock read
        // for the whole replay keeps it consistent.
        let now_ms = now_ms();

        // Seed every shard from the durable tier first, so the replay below
        // folds only the journal tail past each checkpoint's watermark.
        let mut seeds: HashMap<CellId, RecoveredState> = HashMap::new();
        let mut ckpt_watermarks: HashMap<CellId, Lsn> = HashMap::new();
        for &shard in &config.shards {
            let store = Arc::clone(checkpoints);
            let grid = config.grid;
            let loaded = block_on_store(
                move || async move { store.load(shard, grid).await },
                "checkpoint load",
            )?;
            if let Some(ckpt) = loaded {
                ckpt_watermarks.insert(shard, ckpt.watermark);
                seeds.insert(
                    shard,
                    RecoveredState {
                        state: ckpt.entities,
                        by_cell: ckpt.by_cell,
                        tombstones: ckpt.tombstones,
                    },
                );
            }
        }

        // One pass over the journal: dispatch each record to its deepest
        // matching shard, tracking each shard's running-maximum epoch as we
        // go (C-2). One pass, not `shards × journal`, keeps `open`
        // proportional to the journal length no matter how the shard set
        // grows.
        let mut max_epoch_by_shard: HashMap<CellId, Epoch> = HashMap::new();
        // The freshest journal position folded into each shard's state: the
        // checkpoint watermark advanced by every kept tail record past it.
        // This is the actor's `ckpt_watermark` after open.
        let mut coverage: HashMap<CellId, Lsn> = ckpt_watermarks.clone();
        for item in journal.scan_from(Lsn::new(0, 0)) {
            let stored = item?;
            let rec = stored.record;
            let Some(shard) = deepest_shard(&config.shards, rec.cell) else {
                continue;
            };
            // C-2: drop iff a strictly higher epoch was already observed at a
            // lower LSN (a zombie write from a superseded ownership epoch).
            let max_seen = max_epoch_by_shard.entry(shard).or_insert(Epoch::new(0));
            if rec.epoch < *max_seen {
                continue;
            }
            *max_seen = (*max_seen).max(rec.epoch);
            verify_crc(&rec)?;
            // The watermark strictly bounds the tail: the checkpoint covers
            // LSNs `1..=watermark`, but the very first record of a journal is
            // LSN 0:0 — equal to the absent-checkpoint watermark, not covered
            // by it. Only filter when a checkpoint exists.
            let covered_through = ckpt_watermarks.get(&shard).copied();
            // Records at or below the checkpoint watermark are already folded
            // into the seed; only the tail past it is replayed (§3.4 step 3).
            if covered_through.is_none_or(|w| rec.lsn > w) {
                let seed = seeds.entry(shard).or_default();
                fold(
                    &mut seed.state,
                    &mut seed.by_cell,
                    &mut seed.tombstones,
                    &rec,
                    now_ms,
                );
                let covered = coverage.entry(shard).or_insert(Lsn::new(0, 0));
                *covered = (*covered).max(rec.lsn);
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
                    seed.state,
                    seed.by_cell,
                    seed.tombstones,
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
        match self.fence.fence(shard, expected, &new).await {
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
                let (mut state, mut by_cell, mut tombstones, mut watermark) = ckpt.map_or_else(
                    || (HashMap::new(), HashMap::new(), HashMap::new(), Lsn::new(0, 0)),
                    |c| (c.entities, c.by_cell, c.tombstones, c.watermark),
                );
                if let Some(old) = self.actors.remove(&shard) {
                    if let Ok(snap) = old.checkpoint_snapshot().await {
                        if snap.ckpt_watermark >= watermark {
                            state = snap.entities;
                            by_cell = snap.by_cell;
                            tombstones = snap.tombstones;
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
                        state,
                        by_cell,
                        tombstones,
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
            .begin_split(parent, parent_row, &child_rows)
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
            self.actors.insert(
                *child,
                actor::spawn_preloaded(
                    *child,
                    self.grid,
                    new_epoch,
                    Arc::clone(&self.journal),
                    partition,
                    child_by_cell,
                    child_tombstones,
                    snap.ckpt_watermark,
                    &self.joins,
                ),
            );
        }

        // Retire the parent row and drop the parent actor.
        let _ = self.fence.retire(parent).await;
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
        let handle = self.actor(self.grid, shard).ok_or_else(|| {
            crate::checkpoint::CheckpointError::Store(format!("no actor for shard {shard}"))
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let snap = handle
            .checkpoint_snapshot()
            .await
            .map_err(|_| crate::checkpoint::CheckpointError::Store("actor gone".into()))?;
        let data = CheckpointData {
            shard: snap.shard,
            grid: snap.grid,
            node_id: self.node_id,
            epoch: snap.epoch,
            watermark: snap.ckpt_watermark,
            entities: snap.entities,
            by_cell: snap.by_cell,
            tombstones: snap.tombstones,
            taken_at_ms: now,
        };
        store.checkpoint(&data).await?;
        // The store's GC pass cleared the expired tombstone rows (D11 §6,
        // P-6); drop them from the actor now so the next checkpoint does not
        // rewrite them. Safe on failure: an interrupted checkpoint re-runs and
        // clears them again, so a stale actor entry cannot resurrect a row.
        handle.prune_tombstones(now).await.map_err(|_| {
            crate::checkpoint::CheckpointError::Store("actor gone after checkpoint".into())
        })?;
        Ok(())
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
    /// The epoch predicate is decision C-2 (docs/11-roadmap.md §P2), identical
    /// to [`CellRuntime::open`]'s: scan in LSN order, drop a record iff its
    /// epoch is below the running maximum seen so far.
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
        // into the actor. The watermark W bounds the replay below; `None` when
        // there is no checkpoint, meaning the whole journal is the tail.
        let mut watermark = None;
        if let Some(ckpt) = store.load(shard, self.grid).await? {
            watermark = Some(ckpt.watermark);
            handle
                .restore_entities(ckpt.entities, ckpt.by_cell, ckpt.tombstones)
                .await
                .map_err(|_| {
                    crate::checkpoint::CheckpointError::Store("actor gone during restore".into())
                })?;
            handle
                .set_watermark(ckpt.watermark)
                .await
                .map_err(|_| {
                    crate::checkpoint::CheckpointError::Store("actor gone during restore".into())
                })?;
        }

        // Replay the journal tail, tracking the running maximum epoch (C-2)
        // and re-verifying crc. Start the scan at the loaded watermark (the
        // [brief-mandated `scan_from(watermark)`](docs/08-persistence.md §3.4)
        // bounds the read); the strict `lsn > watermark` filter then keeps the
        // tail past it. With no checkpoint the watermark is 0:0 and the tail
        // is the whole journal — except LSN 0:0 itself, the very first record,
        // which is below no checkpoint and must replay.
        let scan_from = watermark.unwrap_or(Lsn::new(0, 0));
        let mut replayed = 0usize;
        let mut max_epoch = Epoch::new(0);
        for item in self.journal.scan_from(scan_from) {
            let stored =
                item.map_err(|e| crate::checkpoint::CheckpointError::Store(format!("{e}")))?;
            let rec = &stored.record;
            if !shard.is_prefix_of(rec.cell) {
                continue;
            }
            if watermark.is_some_and(|w| rec.lsn <= w) {
                continue;
            }
            // C-2: drop iff a strictly higher epoch was already observed at a
            // lower LSN (a zombie write from a superseded ownership epoch).
            if rec.epoch < max_epoch {
                continue;
            }
            max_epoch = max_epoch.max(rec.epoch);
            verify_crc(rec)
                .map_err(|e| crate::checkpoint::CheckpointError::Store(format!("{e}")))?;
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
/// markers) — the checkpoint seed folded with the replayed journal tail.
#[derive(Default)]
struct RecoveredState {
    state: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, CellId>,
    tombstones: HashMap<PersistId, Tombstone>,
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

/// Drive a [`CheckpointStore`] call to completion from synchronous recovery
/// code (`open` runs before the runtime's async surface is usable).
///
/// The thunk produces the store's future on the executing thread: inside a
/// Tokio context that is a freshly spawned worker thread (the future is
/// `Send` and the store `Sync`, and the `Mem`/`Fdb` stores are
/// runtime-agnostic), avoiding `block_in_place`'s multi-thread-runtime
/// requirement; outside any Tokio context a current-thread runtime drives it
/// in place.
fn block_on_store<F, Fut, T>(call: F, what: &'static str) -> Result<T, crate::journal::JournalError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: core::future::Future<Output = Result<T, crate::checkpoint::CheckpointError>> + Send,
    T: Send + 'static,
{
    let run = move || {
        futures::executor::block_on(call())
            .map_err(|e| crate::journal::JournalError::Store(format!("{what}: {e}")))
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(run)
            .join()
            .map_err(|_| crate::journal::JournalError::Store(format!("{what} thread panicked")))?
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| crate::journal::JournalError::Store(format!("{what} runtime: {e}")))?
            .block_on(async { run() })
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

/// Fold a record into an entity map and tombstone set (last-writer-wins per
/// entity). Mirrors the actor's fold logic for the `open`-time replay, which
/// runs before any actor exists; `now_ms` seeds the despawn markers' GC
/// deadlines (P-6).
fn fold(
    state: &mut HashMap<PersistId, EntityRecord>,
    by_cell: &mut HashMap<PersistId, CellId>,
    tombstones: &mut HashMap<PersistId, Tombstone>,
    record: &JournalRecord,
    now_ms: u64,
) {
    match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff => {
            let entry = state.entry(record.entity).or_default();
            entry.components = record.payload.clone();
            entry.dirty = true;
            by_cell.insert(record.entity, record.cell);
            tombstones.remove(&record.entity);
        }
        RecordKind::Despawn => {
            state.remove(&record.entity);
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
