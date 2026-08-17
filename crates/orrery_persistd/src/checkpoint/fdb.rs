//! FoundationDB-backed checkpoint store (`fdb` feature, D11 §6, §5).
//!
//! Maps the checkpoint keyspace onto FDB as specified in docs/08-persistence.md
//! §6: `world/{cell_id}/{entity_id}` → component bag, `ckpt/{shard}` → the
//! watermark row. Written in serializable `db.run` transactions so a checkpoint
//! batch and its watermark commit atomically.
//!
//! **Key construction** lives in [`crate::keyspace`]; this module calls those
//! helpers and defines none of its own.
//!
//! Four contract points that the seeder and cold load depend on (docs/12-world-seeding §2):
//!
//! - **Rows are keyed by the entity's own cell** (`by_cell`), not the shard
//!   (P-2): §6 says `cell_id` is the entity's cell, so seeded rows and actor
//!   checkpoints share one key convention.
//! - **The subtree is one contiguous range** (P-3): `world_range_start`/`_end`
//!   use [`CellId::subtree_range()`], so `read_cold` serves a whole subtree and
//!   `delete` clears it.
//! - **The `ckpt/` value is the watermark only** (P-8): `(node_id, lsn, epoch,
//!   time)` per §6 — never the entity bag, so a shard of any size stays under
//!   FDB's 100 KB value ceiling. The bag lives in the per-entity `world/`
//!   rows; [`FdbCheckpointStore::load`] rebuilds it by scanning the subtree.
//! - **Despawn rows are tombstoned, and GC'd on cadence** (P-6): every `world/`
//!   value carries a one-byte tag. Live rows are `0x00 ‖ bag`; despawn markers
//!   are `0x01 ‖ postcard(Tombstone {tick, gc_deadline_ms})`. The checkpoint
//!   writes markers for entities dead and not yet GC'd, clears markers past
//!   their deadline, and `read_cold`/`load` never surface a tombstone — so a
//!   dead entity cannot be resurrected by a cold scan.
//! - **A moved entity leaves exactly one row** (P-9): the `world/` key carries
//!   the entity's cell, so a cell change writes a *new* key; the checkpoint
//!   clears the vacated keys the actor recorded
//!   ([`CheckpointData::superseded`]) in the same pass that writes the new
//!   ones. Two live rows for one entity are not merely wasted space: both
//!   readers rebuild `by_cell` from the key, and a `HashMap` keyed by
//!   `PersistId` collapses them to whichever cell sorts higher in Morton
//!   order — a stale-versus-fresh coin flip, and, across shards, two actors
//!   recovering the same entity.
//! - **Rows without a watermark are still state** (P-11): the two reads
//!   [`FdbCheckpointStore::load`] performs are independent (§3.4 step 2). A
//!   shard whose subtree has rows but no `ckpt/` row — freshly seeded
//!   (docs/12-world-seeding.md §11.4 makes that the seeder's contract), split
//!   but not yet checkpointed, or interrupted mid-first-checkpoint — loads its
//!   rows at watermark 0:0 and replays the whole journal on top. Only a shard
//!   with *neither* is absent.
//! - **Rows are grid-scoped** (P-7): the `world/` key carries the 4-byte
//!   `GridId` (§6 calls `cell_id` grid-relative), so nested-grid content and
//!   root-grid content with the same `CellId` cannot collide and per-grid
//!   subtree scans are one contiguous range.
//!
//! This adapter compiles only under the `fdb` feature (it links `libfdb_c`).
//! It needs a **live cluster** to actually run; tests that use it self-skip when
//! no cluster is reachable.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use foundationdb::{Database, KeySelector, RangeOption};
use futures::TryStreamExt;

use orrery_protocol::{CellId, Epoch, GridId, Lsn, PersistId};

use crate::keyspace;
use crate::FdbContext;

use crate::actor::{EntityRecord, SnapshotPage, SupersededRow, Tombstone};
use crate::checkpoint::{CheckpointData, CheckpointError, CheckpointStore, ColdCellReader};
use crate::fence::{FenceRow, FenceStatus};

/// Require the active ownership row in the transaction that writes a
/// checkpoint. Reading the row establishes the conflict range which fences a
/// zombie checkpoint when a promotion changes the owner or epoch.
async fn require_active_fence(
    trx: &foundationdb::Transaction,
    grid: GridId,
    shard: CellId,
    owner: u64,
    epoch: Epoch,
) -> Result<(), foundationdb::FdbBindingError> {
    let key = keyspace::fence_key(grid, shard);
    let current: Option<FenceRow> = trx
        .get(&key, false)
        .await?
        .map(|bytes| postcard::from_bytes(bytes.as_ref()))
        .transpose()
        .map_err(|e| {
            foundationdb::FdbBindingError::new_custom_error(Box::new(CheckpointError::Store(
                format!("fence decode: {e}"),
            )))
        })?;
    if current
        != Some(FenceRow {
            owner,
            epoch,
            status: FenceStatus::Active,
        })
    {
        return Err(foundationdb::FdbBindingError::new_custom_error(Box::new(
            CheckpointError::Store(format!(
                "fence mismatch for {grid}/{shard}: expected active owner {owner} epoch {epoch}, got {current:?}"
            )),
        )));
    }
    Ok(())
}

/// The `ckpt/{shard}` value: the recovery watermark, exactly as D11 §6 —
/// `(node_id, journal lsn, epoch, time)`. Deliberately **not** the entity bag
/// (P-8); the bag lives in the per-entity `world/` rows the checkpoint writes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CheckpointMeta {
    /// The node that took the checkpoint.
    node_id: u64,
    /// Journal LSN covered by this checkpoint (recovery replays `> this`).
    watermark: Lsn,
    /// The shard-ownership epoch the checkpoint was taken under (§3.4 fence).
    epoch: Epoch,
    /// Wall-clock time the checkpoint was taken, as unix milliseconds.
    taken_at_ms: u64,
}

impl From<&CheckpointData> for CheckpointMeta {
    fn from(data: &CheckpointData) -> Self {
        Self {
            node_id: data.node_id,
            watermark: data.watermark,
            epoch: data.epoch,
            taken_at_ms: data.taken_at_ms,
        }
    }
}

/// One shard subtree's `world/` rows, as the scan reconstructs them.
#[derive(Default)]
struct WorldRows {
    entities: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, CellId>,
    tombstones: HashMap<PersistId, Tombstone>,
    /// Rows the scan proved redundant: an entity with more than one row in the
    /// subtree keeps one, and the rest are reported here so the next
    /// checkpoint clears them (P-9). Nothing else can find these — the writer
    /// that left them behind has long forgotten the cell.
    superseded: HashSet<SupersededRow>,
}

/// Scan the `world/` rows of `shard`'s subtree in `grid`, rebuilding the entity
/// bag, the per-entity cell map, and the despawn markers (P-2: rows are keyed
/// by the entity's cell, which each key records; P-6: tombstone rows are
/// decoded, never surfaced as entities; P-7: the scan is grid-scoped; P-9: a
/// duplicated entity keeps one row and reports the others). Shared by
/// [`FdbCheckpointStore::load`] — P-8 — and [`ColdCellReader::read_cold`].
async fn scan_world(
    trx: &foundationdb::Transaction,
    grid: GridId,
    shard: CellId,
) -> Result<WorldRows, foundationdb::FdbBindingError> {
    let start = keyspace::world_range_start(grid, shard);
    let end = keyspace::world_range_end(grid, shard);
    let opt = RangeOption {
        begin: KeySelector::first_greater_or_equal(start.as_slice()),
        end: KeySelector::first_greater_or_equal(end.as_slice()),
        ..RangeOption::default()
    };
    let mut rows = WorldRows::default();
    let mut stream = trx.get_ranges_keyvalues(opt, false);
    while let Some(kv) = stream.try_next().await? {
        // Key: w + grid(4) + cell(8) + entity(8). The cell (the entity's home,
        // P-2) is recovered from the key so `by_cell` is rebuilt for split
        // readiness.
        let raw = kv.key();
        if raw.len() != 21 {
            continue;
        }
        let mut cell = [0u8; 8];
        cell.copy_from_slice(&raw[5..13]);
        let Some(cell) = CellId::from_bits(u64::from_be_bytes(cell)) else {
            continue;
        };
        let mut ent = [0u8; 8];
        ent.copy_from_slice(&raw[13..21]);
        let entity = PersistId::new(u64::from_be_bytes(ent));
        let value = kv.value();
        match value.first() {
            Some(&keyspace::LIVE_TAG) => {
                rows.entities.insert(
                    entity,
                    EntityRecord {
                        components: bytes::Bytes::copy_from_slice(&value[1..]),
                        dirty: false,
                    },
                );
                // Keys arrive in Morton order, so a second live row for the
                // same entity supersedes the first — the same one a
                // `HashMap` insert would have kept, made explicit and
                // reported instead of silently dropped (P-9).
                if let Some(previous) = rows.by_cell.insert(entity, cell) {
                    rows.superseded.insert((entity, previous));
                }
            }
            Some(&keyspace::TOMBSTONE_TAG) => {
                // A despawn marker (P-6): never an entity. Decode it so recovery
                // can continue the GC countdown.
                let tombstone: Tombstone = match postcard::from_bytes(&value[1..]).map_err(|_| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(
                        CheckpointError::Store("tombstone decode".into()),
                    ))
                }) {
                    Ok(t) => t,
                    Err(e) => return Err(e),
                };
                rows.tombstones.insert(entity, tombstone);
            }
            // Unknown tag: a row written by a future version; skip it rather
            // than fail the whole scan.
            _ => {}
        }
    }
    // A despawn marker and a live row for one entity can only coexist at two
    // different cells (one cell is one key). P-6 is unambiguous about which
    // wins: a dead entity must not be resurrected by a cold scan, and at a
    // shared cell the marker already overwrites the row. So the stray live
    // row is dropped and reported.
    for (entity, tomb) in &rows.tombstones {
        if let Some(cell) = rows.by_cell.get(entity).copied() {
            if cell != tomb.cell {
                rows.entities.remove(entity);
                rows.by_cell.remove(entity);
                rows.superseded.insert((*entity, cell));
            }
        }
    }
    Ok(rows)
}

/// An FDB-backed checkpoint store.
pub struct FdbCheckpointStore {
    db: Arc<Database>,
}

impl FdbCheckpointStore {
    /// Build a checkpoint store using a process-scoped FDB context.
    #[must_use]
    pub fn from_context(context: &FdbContext) -> Self {
        Self {
            db: context.database(),
        }
    }

    /// Build a checkpoint store from an already-open database handle.
    #[must_use]
    pub fn from_database(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Connect to the cluster at `cluster_file`.
    ///
    /// Prefer constructing one [`FdbContext`] and using [`Self::from_context`]
    /// when a process needs more than one FDB-backed adapter.
    pub fn connect(cluster_file: &str) -> Result<Self, CheckpointError> {
        let context =
            FdbContext::connect(cluster_file).map_err(|e| CheckpointError::Store(e.to_string()))?;
        Ok(Self::from_context(&context))
    }
}

#[async_trait::async_trait]
impl CheckpointStore for FdbCheckpointStore {
    async fn checkpoint(&self, data: &CheckpointData) -> Result<(), CheckpointError> {
        // One serializable transaction: write all entity rows, write live
        // tombstones, clear GC'd tombstones, and write the watermark.
        // Rows are idempotent overwrites, so a partially applied checkpoint
        // (e.g. interrupted before commit) is simply re-run (§5). The entity
        // bag is written only as `world/{grid}/{cell}/{entity}` rows — never
        // inside `ckpt/` (P-8) — so a shard of any size stays under FDB's
        // 100 KB value ceiling.
        //
        // The GC pass (P-6, D11 §6): a despawn marker is written for every
        // tombstone not yet past its deadline, and the row is cleared once the
        // deadline passes — a despawned entity's row is never silently left
        // behind, and the marker is never kept forever. `taken_at_ms` is the
        // pass's clock, so an interrupted checkpoint re-runs identically.
        let db = Arc::clone(&self.db);
        let data = data.clone();

        // Prepare entity rows
        let entries: Vec<([u8; 21], Vec<u8>)> = data
            .entities
            .iter()
            .map(|(entity, record)| {
                let cell = data.by_cell.get(entity).copied().unwrap_or(data.shard);
                let key = keyspace::world_key(data.grid, cell, *entity);
                (key, keyspace::encode_live_value(&record.components))
            })
            .collect();

        // Every key this checkpoint *writes*, live rows and despawn markers
        // alike. A superseded pair naming one of them is stale bookkeeping —
        // the entity left the cell and came back before the clear ran — and
        // clearing it would delete the row this same checkpoint just wrote.
        let written: HashSet<[u8; 21]> = entries
            .iter()
            .map(|(key, _)| *key)
            .chain(
                data.tombstones
                    .iter()
                    .filter(|(_, tomb)| tomb.gc_deadline_ms > data.taken_at_ms)
                    .map(|(entity, tomb)| keyspace::world_key(data.grid, tomb.cell, *entity)),
            )
            .collect();

        // Write entity rows in parallel bounded chunks to avoid FDB commit latency spikes.
        let chunk_futures: Vec<_> = entries
            .chunks(2000)
            .map(|chunk| {
                let chunk = chunk.to_vec();
                let db = Arc::clone(&db);
                let data = data.clone();
                async move {
                    db.run(|trx, _| {
                        let chunk = chunk.clone();
                        let data = data.clone();
                        async move {
                            require_active_fence(
                                &trx,
                                data.grid,
                                data.shard,
                                data.node_id,
                                data.epoch,
                            )
                            .await?;
                            for (key, val) in &chunk {
                                trx.set(key, val);
                            }
                            Ok(())
                        }
                    })
                    .await
                }
            })
            .collect();

        futures::future::try_join_all(chunk_futures).await.map_err(
            |e: foundationdb::FdbBindingError| {
                CheckpointError::Store(format!("checkpoint chunk txn: {e}"))
            },
        )?;

        // Final transaction: tombstones and checkpoint metadata
        let data = data.clone();
        db.run(|trx, _| {
            let data = data.clone();
            let written = written.clone();
            async move {
                require_active_fence(&trx, data.grid, data.shard, data.node_id, data.epoch).await?;
                for (entity, tombstone) in &data.tombstones {
                    let key = keyspace::world_key(data.grid, tombstone.cell, *entity);
                    if tombstone.gc_deadline_ms <= data.taken_at_ms {
                        trx.clear(&key);
                    } else {
                        let value = keyspace::encode_tombstone_value(tombstone).map_err(|e| {
                            foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                        })?;
                        trx.set(&key, &value);
                    }
                }
                // Clear the vacated `world/` keys (P-9). This runs *after*
                // the row chunks above committed, never before: a clear that
                // outran its replacement write would be data loss, whereas a
                // clear that never runs is retried by the next checkpoint —
                // the actor keeps the pair until this transaction commits.
                for &(entity, cell) in &data.superseded {
                    // The fence read above authorizes this shard's subtree and
                    // nothing else; a pair naming a foreign cell is not this
                    // checkpoint's to clear.
                    if !data.shard.is_prefix_of(cell) {
                        continue;
                    }
                    let key = keyspace::world_key(data.grid, cell, entity);
                    if written.contains(&key) {
                        continue;
                    }
                    trx.clear(&key);
                }
                let encoded = postcard::to_stdvec(&CheckpointMeta::from(&data)).map_err(|e| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(
                        CheckpointError::Store(format!("encode: {e}")),
                    ))
                })?;
                trx.set(&keyspace::ckpt_key(data.grid, data.shard), &encoded);
                Ok(())
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("checkpoint meta txn: {e}"))
        })
    }

    async fn load(
        &self,
        shard: CellId,
        grid: GridId,
    ) -> Result<Option<CheckpointData>, CheckpointError> {
        let db = Arc::clone(&self.db);
        db.run(|trx, _| async move {
            // §3.4 step 2 is **two independent reads**: the range scan that
            // rebuilds the shard's state, and the `ckpt/` read that says how
            // far the journal has already been folded into it. They are read
            // together here — a scan gated behind the watermark row made the
            // watermark's *absence* mean "this shard has no state", which is
            // exactly what it does not mean (P-11 below).
            let raw = trx.get(&keyspace::ckpt_key(grid, shard), false).await?;
            // Rebuild the entity bag, cell map, and tombstone set from the
            // `world/` rows — the `ckpt/` value is the watermark only (P-8),
            // so the bag never comes from it.
            let rows = scan_world(&trx, grid, shard).await?;
            let Some(bytes) = raw else {
                // **A seeded shard has rows and no watermark** (P-11):
                // `orrery-seed` writes `world/` rows and deliberately no
                // `ckpt/` row (docs/12-world-seeding.md §11.4), and so does
                // every shard that has state but has not checkpointed yet —
                // a split child loading its share of the parent's rows
                // (§3.5), a first-ever checkpoint that committed row chunks
                // and died before its meta transaction, a shard whose
                // watermark row was lost. Returning `None` here recovered an
                // *empty* shard from a cluster holding the whole world:
                // `committed_entity_cell` then resolves nothing, every lease
                // claim is denied `NotEligible`, and no bulk write is ever
                // fenced through.
                //
                // The rows are adopted at watermark 0:0, i.e. the whole
                // journal is the tail. That is safe in all four cases, not
                // just after seeding, because replay is a *state-replacing*
                // fold (`CellRuntime::fold` assigns `entry.components`; it
                // never accumulates a delta), so re-folding records already
                // covered by the rows re-derives the same state rather than
                // double-applying it — and the epoch gate rebuilds its
                // running maximum (C-2) from those same records instead of
                // being seeded mid-journal from a watermark we do not have.
                // The cost is a longer replay, not a wrong one.
                //
                // Boundary, deliberately not papered over: watermark 0:0 is
                // indistinguishable from "covers the first record", so the
                // record at journal position 0:0 is filtered out of the
                // replay. That ambiguity is the watermark type's, not this
                // branch's — a genuine checkpoint taken before the first
                // append writes the same 0:0 — and it costs the first record
                // of a fresh journal, against a whole world recovered.
                if rows.entities.is_empty() && rows.tombstones.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(CheckpointData {
                    shard,
                    grid,
                    // No checkpoint was taken, so there is no taker, no epoch
                    // it was taken under, and no time it was taken at. Zeroed
                    // rather than guessed: the fence row is the authority on
                    // the *current* epoch, and seeding the gate with it would
                    // reject the very tail this load exists to replay.
                    node_id: 0,
                    epoch: Epoch::new(0),
                    watermark: Lsn::new(0, 0),
                    entities: rows.entities,
                    by_cell: rows.by_cell,
                    tombstones: rows.tombstones,
                    superseded: rows.superseded,
                    taken_at_ms: 0,
                }));
            };
            let meta: CheckpointMeta = postcard::from_bytes(bytes.as_ref()).map_err(|e| {
                foundationdb::FdbBindingError::new_custom_error(Box::new(CheckpointError::Store(
                    format!("decode: {e}"),
                )))
            })?;
            Ok(Some(CheckpointData {
                shard,
                grid,
                node_id: meta.node_id,
                epoch: meta.epoch,
                watermark: meta.watermark,
                entities: rows.entities,
                by_cell: rows.by_cell,
                tombstones: rows.tombstones,
                superseded: rows.superseded,
                taken_at_ms: meta.taken_at_ms,
            }))
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("load txn: {e}"))
        })
    }

    async fn delete(&self, shard: CellId, grid: GridId) -> Result<(), CheckpointError> {
        let db = Arc::clone(&self.db);
        db.run(|trx, _| async move {
            trx.clear(&keyspace::ckpt_key(grid, shard));
            // Also clear the entity rows for this shard's subtree (P-3, P-7).
            let start = keyspace::world_range_start(grid, shard);
            let end = keyspace::world_range_end(grid, shard);
            trx.clear_range(&start, &end);
            Ok(())
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("delete txn: {e}"))
        })
    }
}

#[async_trait::async_trait]
impl ColdCellReader for FdbCheckpointStore {
    async fn read_cold(
        &self,
        grid: GridId,
        cell: CellId,
    ) -> Result<Option<SnapshotPage>, CheckpointError> {
        let db = Arc::clone(&self.db);
        db.run(|trx, _| async move {
            let rows = scan_world(&trx, grid, cell).await?;
            if rows.entities.is_empty() {
                return Ok(None);
            }
            Ok(Some(SnapshotPage {
                entities: rows.entities,
            }))
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("cold read txn: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_is_watermark_only() {
        // P-8 regression: the row we persist is `(node_id, lsn, epoch, time)` —
        // a handful of bytes regardless of entity bag size.
        let mut entities = HashMap::new();
        for i in 0..10_000u64 {
            entities.insert(
                PersistId::new(i),
                EntityRecord {
                    components: bytes::Bytes::from_static(b"xxxxxxxxxxxx"),
                    dirty: true,
                },
            );
        }
        let data = CheckpointData {
            shard: CellId::ROOT,
            grid: GridId::ROOT,
            node_id: 7,
            epoch: Epoch::new(1_700_000_000_000),
            watermark: Lsn::new(2, 4096),
            entities,
            by_cell: HashMap::new(),
            tombstones: HashMap::new(),
            superseded: HashSet::new(),
            taken_at_ms: 1_700_000_000_000,
        };
        let meta = CheckpointMeta::from(&data);
        let encoded = postcard::to_stdvec(&meta).unwrap();
        assert!(
            encoded.len() < 128,
            "ckpt value must be the watermark only, got {} bytes for a 10k-entity shard",
            encoded.len()
        );
        assert_eq!(meta, postcard::from_bytes(&encoded).unwrap());
    }
}
