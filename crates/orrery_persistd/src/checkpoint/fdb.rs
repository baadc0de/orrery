#![allow(unsafe_code)] // `foundationdb::boot()` is the one unsafe call, gated behind the `fdb` feature.

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
//! - **Rows are grid-scoped** (P-7): the `world/` key carries the 4-byte
//!   `GridId` (§6 calls `cell_id` grid-relative), so nested-grid content and
//!   root-grid content with the same `CellId` cannot collide and per-grid
//!   subtree scans are one contiguous range.
//!
//! This adapter compiles only under the `fdb` feature (it links `libfdb_c`).
//! It needs a **live cluster** to actually run; tests that use it self-skip when
//! no cluster is reachable.

use std::collections::HashMap;
use std::sync::Arc;

use foundationdb::{Database, KeySelector, RangeOption};
use futures::TryStreamExt;

use orrery_protocol::{CellId, Epoch, GridId, Lsn, PersistId};

use crate::keyspace;

use crate::actor::{EntityRecord, SnapshotPage, Tombstone};
use crate::checkpoint::{CheckpointData, CheckpointError, CheckpointStore, ColdCellReader};

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

/// Boot the FoundationDB network once per process and leak the stop guard.
///
/// The FDB C API can only be initialized once per process (docs §5). We boot on
/// first use and intentionally leak the [`NetworkAutoStop`] so the network stays
/// alive for the process lifetime; the OS reclaims it at exit. Safe because this
/// is only ever called once (guarded by `OnceLock`).
fn fdb_network() -> Result<(), CheckpointError> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // `foundationdb::boot()` panics on failure, so once we get here the network
    // is up. We leak the guard so the network lives for the process lifetime.
    ONCE.call_once(|| {
        // SAFETY: boot() is called exactly once per process (Once), and the
        // returned guard is leaked so the network outlives every use.
        let guard = unsafe { foundationdb::boot() };
        std::mem::forget(guard);
    });
    Ok(())
}

/// Scan the `world/` rows of `shard`'s subtree in `grid`, rebuilding the entity
/// bag, the per-entity cell map, and the despawn markers (P-2: rows are keyed
/// by the entity's cell, which each key records; P-6: tombstone rows are
/// decoded, never surfaced as entities; P-7: the scan is grid-scoped). Shared
/// by [`FdbCheckpointStore::load`] — P-8 — and [`ColdCellReader::read_cold`].
async fn scan_world(
    trx: &foundationdb::Transaction,
    grid: GridId,
    shard: CellId,
) -> Result<
    (
        HashMap<PersistId, EntityRecord>,
        HashMap<PersistId, CellId>,
        HashMap<PersistId, Tombstone>,
    ),
    foundationdb::FdbBindingError,
> {
    let start = keyspace::world_range_start(grid, shard);
    let end = keyspace::world_range_end(grid, shard);
    let opt = RangeOption {
        begin: KeySelector::first_greater_or_equal(start.as_slice()),
        end: KeySelector::first_greater_or_equal(end.as_slice()),
        ..RangeOption::default()
    };
    let mut entities = HashMap::new();
    let mut by_cell = HashMap::new();
    let mut tombstones = HashMap::new();
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
                entities.insert(
                    entity,
                    EntityRecord {
                        components: bytes::Bytes::copy_from_slice(&value[1..]),
                        dirty: false,
                    },
                );
                by_cell.insert(entity, cell);
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
                tombstones.insert(entity, tombstone);
            }
            // Unknown tag: a row written by a future version; skip it rather
            // than fail the whole scan.
            _ => {}
        }
    }
    Ok((entities, by_cell, tombstones))
}

/// An FDB-backed checkpoint store.
pub struct FdbCheckpointStore {
    db: Arc<Database>,
}

impl FdbCheckpointStore {
    /// Connect to the cluster at `cluster_file`.
    pub fn connect(cluster_file: &str) -> Result<Self, CheckpointError> {
        fdb_network()?;
        let db = Database::from_path(cluster_file)
            .map_err(|e| CheckpointError::Store(format!("connect: {e}")))?;
        Ok(Self { db: Arc::new(db) })
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
        db.run(|trx, _| {
            let data = data.clone();
            async move {
                for (entity, record) in &data.entities {
                    // P-2: keyed by the entity's own cell (§6
                    // `world/{cell_id}/…`), not the shard. `by_cell` is carried
                    // on the checkpoint for exactly this. P-7: grid-scoped.
                    let cell = data.by_cell.get(entity).copied().unwrap_or(data.shard);
                    let key = keyspace::world_key(data.grid, cell, *entity);
                    trx.set(&key, &keyspace::encode_live_value(&record.components));
                }
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
            CheckpointError::Store(format!("checkpoint txn: {e}"))
        })
    }

    async fn load(
        &self,
        shard: CellId,
        grid: GridId,
    ) -> Result<Option<CheckpointData>, CheckpointError> {
        let db = Arc::clone(&self.db);
        db.run(|trx, _| async move {
            let raw = trx.get(&keyspace::ckpt_key(grid, shard), false).await?;
            let Some(bytes) = raw else {
                return Ok(None);
            };
            let meta: CheckpointMeta = postcard::from_bytes(bytes.as_ref()).map_err(|e| {
                foundationdb::FdbBindingError::new_custom_error(Box::new(CheckpointError::Store(
                    format!("decode: {e}"),
                )))
            })?;
            // Rebuild the entity bag, cell map, and tombstone set from the
            // `world/` rows the checkpoint wrote — the `ckpt/` value is the
            // watermark only (P-8).
            let (entities, by_cell, tombstones) = scan_world(&trx, grid, shard).await?;
            Ok(Some(CheckpointData {
                shard,
                grid,
                node_id: meta.node_id,
                epoch: meta.epoch,
                watermark: meta.watermark,
                entities,
                by_cell,
                tombstones,
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
            let (entities, _, _) = scan_world(&trx, grid, cell).await?;
            if entities.is_empty() {
                return Ok(None);
            }
            Ok(Some(SnapshotPage { entities }))
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
