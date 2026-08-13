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
}

impl CellRuntime {
    /// Open the journal and spawn an actor per shard cell, recovering each from
    /// the journal (§3.4 step 3: replay records with `lsn > watermark`, skipping
    /// superseded epochs, re-verifying crc).
    pub fn open(config: &RuntimeConfig) -> Result<Self, crate::journal::JournalError> {
        let journal = Arc::new(Journal::open(&config.journal)?);
        // The replay below mints tombstone GC deadlines (P-6); one clock read
        // for the whole replay keeps it consistent.
        let now_ms = now_ms();

        let mut actors = HashMap::new();
        for &shard in &config.shards {
            // Rebuild state by replaying this shard's records (crc-checked).
            let mut state = HashMap::new();
            let mut by_cell = HashMap::new();
            let mut tombstones = HashMap::new();
            let mut watermark = Lsn::new(0, 0);
            for item in journal.scan_from(Lsn::new(0, 0)) {
                let stored = item?;
                let rec = &stored.record;
                if !shard.is_prefix_of(rec.cell) {
                    continue;
                }
                if rec.epoch < config.epoch {
                    continue;
                }
                verify_crc(rec)?;
                fold(&mut state, &mut by_cell, &mut tombstones, rec, now_ms);
                watermark = watermark.max(rec.lsn);
            }
            let handle = actor::spawn_preloaded(
                shard,
                config.grid,
                config.epoch,
                Arc::clone(&journal),
                state,
                by_cell,
                tombstones,
                watermark,
            );
            actors.insert(shard, handle);
        }

        Ok(Self {
            journal,
            actors,
            epoch: config.epoch,
            grid: config.grid,
            fence: Arc::clone(&config.fence),
            node_id: config.node_id,
        })
    }

    /// The shared journal.
    pub fn journal(&self) -> &Arc<Journal> {
        &self.journal
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

    /// The actor owning `cell`: the **deepest** shard actor whose subtree
    /// contains `cell` (an exact match when `cell` is itself a shard;
    /// otherwise the shard containing that interest cell). The deepest match
    /// matters because the root is a prefix of every cell — routing must pick
    /// the most specific shard, not an arbitrary one.
    pub fn actor(&self, cell: CellId) -> Option<&CellActorHandle> {
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
    /// Returns the new epoch on success, or a [`FenceError::Conflict`] with
    /// the live row if the CAS preconditions do not hold.
    pub async fn fence_shard(
        &mut self,
        shard: CellId,
        expected: Option<&FenceRow>,
    ) -> Result<Epoch, crate::fence::FenceError> {
        let new_epoch = Epoch::new(expected.map_or(0, |r| r.epoch.0) + 1);
        let new = FenceRow {
            owner: self.node_id,
            epoch: new_epoch,
            status: FenceStatus::Active,
        };
        match self.fence.fence(shard, expected, &new).await {
            Ok(FenceOutcome::Fenced) => {
                self.actors.insert(
                    shard,
                    actor::spawn(shard, self.grid, new_epoch, Arc::clone(&self.journal)),
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
            .actor(record.cell)
            .ok_or(actor::Reject::JournalClosed)?;
        handle.apply_diff(record).await
    }

    /// Read a snapshot from the actor owning `cell`.
    pub async fn read(&self, cell: CellId) -> Result<SnapshotPage, actor::Reject> {
        let handle = self.actor(cell).ok_or(actor::Reject::JournalClosed)?;
        handle.read_snapshot(vec![cell]).await
    }

    /// Capture a copy-on-update snapshot of the actor owning `cell`
    /// (entities, per-entity cells, and despawn markers).
    pub async fn actor_snapshot(
        &self,
        cell: CellId,
    ) -> Result<crate::actor::CheckpointSnapshot, actor::Reject> {
        let handle = self.actor(cell).ok_or(actor::Reject::JournalClosed)?;
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
        let handle = self.actor(shard).ok_or_else(|| {
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
    /// checkpoint are recovered. Zero-loss by construction.
    ///
    /// Returns the number of journal records replayed.
    pub async fn restore(
        &self,
        shard: CellId,
        store: &dyn CheckpointStore,
    ) -> Result<usize, crate::checkpoint::CheckpointError> {
        let handle = self.actor(shard).ok_or_else(|| {
            crate::checkpoint::CheckpointError::Store("no actor for shard".into())
        })?;

        // Load the checkpoint and fold its entity bag and despawn markers
        // into the actor.
        if let Some(ckpt) = store.load(shard, self.grid).await? {
            handle
                .restore_entities(ckpt.entities, ckpt.by_cell, ckpt.tombstones)
                .await
                .map_err(|_| {
                    crate::checkpoint::CheckpointError::Store("actor gone during restore".into())
                })?;
            let watermark = ckpt.watermark;
            handle.set_watermark(watermark).await.map_err(|_| {
                crate::checkpoint::CheckpointError::Store("actor gone during restore".into())
            })?;
        }

        // Replay the journal tail (lsn > watermark), skipping superseded epochs
        // and re-verifying crc.
        let mut replayed = 0usize;
        for item in self.journal.scan_from(Lsn::new(0, 0)) {
            let stored =
                item.map_err(|e| crate::checkpoint::CheckpointError::Store(format!("{e}")))?;
            let rec = &stored.record;
            if !shard.is_prefix_of(rec.cell) {
                continue;
            }
            if rec.epoch < self.epoch {
                continue;
            }
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
        self.journal.close().await
    }

    /// Compute the HRW owner of `cell` over a node set.
    pub fn placement_owner(nodes: &[RendezvousNode], cell: CellId) -> Option<u64> {
        RendezvousHasher::new(nodes.to_vec()).owner(cell)
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
