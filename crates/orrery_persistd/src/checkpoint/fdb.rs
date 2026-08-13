#![allow(unsafe_code)] // `foundationdb::boot()` is the one unsafe call, gated behind the `fdb` feature.

//! FoundationDB-backed checkpoint store (`fdb` feature, D11 §6, §5).
//!
//! Maps the checkpoint keyspace onto FDB as specified in docs/08-persistence.md
//! §6: `world/{cell_id}/{entity_id}` → component bag, `ckpt/{shard}` → the
//! watermark row. Written in serializable `db.run` transactions so a checkpoint
//! batch and its watermark commit atomically.
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

/// Key prefix for entity rows: `world/{grid_id}/{cell_id}/{entity_id}`.
///
/// `cell_id` is the entity's own interest cell (P-2, §6) and `entity_id` is its
/// 8-byte `PersistId`, all big-endian so range scans inherit Morton order
/// (§6: a shard cell's subtree is one contiguous range). The 4-byte `GridId`
/// (P-7) scopes the row to its grid's `CellId` space: the same cell id under
/// two grids is two disjoint keys, and a per-grid subtree scan is one range.
fn world_key(grid: GridId, cell: CellId, entity: PersistId) -> [u8; 21] {
    let mut key = [0u8; 21];
    key[0] = b'w';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&cell.to_bits().to_be_bytes());
    key[13..21].copy_from_slice(&entity.0.to_be_bytes());
    key
}

/// Key prefix for the checkpoint watermark: `ckpt/{grid_id}/{shard}` (P-7:
/// a shard cell id is grid-relative, so two grids never share a row).
fn ckpt_key(grid: GridId, shard: CellId) -> [u8; 13] {
    let mut key = [0u8; 13];
    key[0] = b'c';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

/// The first key of the `world/{grid_id}/{cell_id}/…` span for `shard` in
/// `grid`.
///
/// The span is the shard's **subtree** — every cell wall id `X` from
/// [`CellId::subtree_range()`] start — because a shard cell's subtree is one
/// contiguous range (D11 §6, parent = prefix). The exact-cell span
/// `[bits, bits+1)` is wrong here: `read_cold` must serve descendants and
/// `delete(shard)` must clear them (P-3).
fn world_range_start(grid: GridId, shard: CellId) -> Vec<u8> {
    let range = shard.subtree_range();
    let mut key = Vec::with_capacity(13);
    key.push(b'w');
    key.extend_from_slice(&grid.0.to_be_bytes());
    key.extend_from_slice(&range.start().to_be_bytes());
    key
}

/// The exclusive end of the `world/{grid_id}/{cell_id}/…` subtree span for
/// `shard` in `grid`.
///
/// The subtree is `[start, end]` inclusive in the raw cell id space; the
/// exclusive range bound is the first key past it — `'w' ‖ grid ‖ (end + 1)`.
/// A subtree abutting the top of the u64 space (`end == u64::MAX`, e.g. the
/// root or the outermost octant) owns every `world/` key in its grid, so the
/// bound is the first byte of the **next grid's** span — `'w' ‖ (grid + 1)` —
/// which sorts after every key of this grid (and, for the topmost grid id,
/// the single byte `'x'`).
fn world_range_end(grid: GridId, shard: CellId) -> Vec<u8> {
    let range = shard.subtree_range();
    let end = *range.end();
    if end < u64::MAX {
        let mut key = Vec::with_capacity(13);
        key.push(b'w');
        key.extend_from_slice(&grid.0.to_be_bytes());
        key.extend_from_slice(&(end + 1).to_be_bytes());
        key
    } else if grid.0 < u32::MAX {
        let mut key = Vec::with_capacity(5);
        key.push(b'w');
        key.extend_from_slice(&(grid.0 + 1).to_be_bytes());
        key
    } else {
        vec![b'x']
    }
}

/// The one-byte tag prefix of a `world/` value: distinguishes a live component
/// bag from a despawn tombstone (P-6, D11 §6).
///
/// Values are `LIVE_TAG ‖ component bag` or `TOMBSTONE_TAG ‖ postcard(Tombstone)`.
/// The tag lives in the key's value, never the key, so live rows, tombstone
/// rows, and the seeder's rows share one key convention.
const LIVE_TAG: u8 = 0x00;
const TOMBSTONE_TAG: u8 = 0x01;

/// Encode a live entity value: `LIVE_TAG ‖ components`.
fn encode_live_value(components: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(components.len() + 1);
    value.push(LIVE_TAG);
    value.extend_from_slice(components);
    value
}

/// Encode a despawn marker value: `TOMBSTONE_TAG ‖ postcard(Tombstone)`.
fn encode_tombstone_value(tombstone: &Tombstone) -> Result<Vec<u8>, CheckpointError> {
    let mut value = Vec::with_capacity(1 + 32);
    value.push(TOMBSTONE_TAG);
    value.extend_from_slice(
        &postcard::to_stdvec(tombstone)
            .map_err(|e| CheckpointError::Store(format!("tombstone encode: {e}")))?,
    );
    Ok(value)
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
    let start = world_range_start(grid, shard);
    let end = world_range_end(grid, shard);
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
            Some(&LIVE_TAG) => {
                entities.insert(
                    entity,
                    EntityRecord {
                        components: bytes::Bytes::copy_from_slice(&value[1..]),
                        dirty: false,
                    },
                );
                by_cell.insert(entity, cell);
            }
            Some(&TOMBSTONE_TAG) => {
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
                    let key = world_key(data.grid, cell, *entity);
                    trx.set(&key, &encode_live_value(&record.components));
                }
                for (entity, tombstone) in &data.tombstones {
                    let key = world_key(data.grid, tombstone.cell, *entity);
                    if tombstone.gc_deadline_ms <= data.taken_at_ms {
                        trx.clear(&key);
                    } else {
                        let value = encode_tombstone_value(tombstone).map_err(|e| {
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
                trx.set(&ckpt_key(data.grid, data.shard), &encoded);
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
            let raw = trx.get(&ckpt_key(grid, shard), false).await?;
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
            trx.clear(&ckpt_key(grid, shard));
            // Also clear the entity rows for this shard's subtree (P-3, P-7).
            let start = world_range_start(grid, shard);
            let end = world_range_end(grid, shard);
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
    use orrery_protocol::Tick;

    use super::*;

    /// A level-18 shard and two level-21 cells from the docs/01-spatial-model
    /// §3.3 worked example: shard `0xA924_9249_2492_4E00`, subtree
    /// `[0xA924_9249_2492_4C01, 0xA924_9249_2492_4FFF]`, containing cell
    /// `0xA924_9249_2492_4D65`.
    const SHARD: u64 = 0xA924_9249_2492_4E00;
    const CELL: u64 = 0xA924_9249_2492_4D65;
    /// A level-21 cell just *outside* the shard's subtree (0x...5200 > 0x...4FFF).
    const FOREIGN: u64 = 0xA924_9249_2492_5200;

    const GRID: GridId = GridId::ROOT;

    #[test]
    fn subtree_bounds_match_cellid_subtree_range() {
        // P-3 regression: the range helpers must span the cell's subtree, not
        // the exact-cell span `[bits, bits+1)`.
        let shard = CellId::from_bits(SHARD).unwrap();
        let expect = shard.subtree_range();
        let start = world_range_start(GRID, shard);
        let end = world_range_end(GRID, shard);

        assert_eq!(&start[..1], b"w");
        assert_eq!(
            u32::from_be_bytes(start[1..5].try_into().unwrap()),
            GRID.0,
            "begin is grid-scoped (P-7)"
        );
        assert_eq!(
            u64::from_be_bytes(start[5..13].try_into().unwrap()),
            *expect.start(),
            "begin = subtree start, not the cell id itself"
        );
        assert_eq!(&end[..1], b"w");
        assert_eq!(
            u64::from_be_bytes(end[5..13].try_into().unwrap()),
            *expect.end() + 1,
            "end = subtree end + 1, not bits + 1"
        );

        // The exact-cell span would both miss descendants (the cell's own id is
        // inside the subtree, but not equal to its start) and include nothing
        // of the subtree above the cell id — assert the honest read: a row
        // under CELL falls inside the shard's span, a row under FOREIGN does not.
        let in_subtree = world_key(GRID, shard, PersistId::new(1));
        assert!(in_subtree.as_slice() >= start.as_slice());
        assert!(in_subtree.as_slice() < end.as_slice());
        let in_cell = world_key(GRID, CellId::from_bits(CELL).unwrap(), PersistId::new(1));
        assert!(in_cell.as_slice() >= start.as_slice());
        assert!(in_cell.as_slice() < end.as_slice());
        let out = world_key(GRID, CellId::from_bits(FOREIGN).unwrap(), PersistId::new(1));
        assert!(
            out.as_slice() >= end.as_slice() || out.as_slice() < start.as_slice(),
            "FOREIGN cell lies outside the shard subtree span"
        );
    }

    #[test]
    fn root_subtree_is_unbounded() {
        // The root (and any subtree abutting u64::MAX) scans every `world/`
        // key of its grid: the end bound is the first key of the next grid
        // (`w ‖ grid+1`), not `w ‖ 0` and not a byte past `w`.
        let start = world_range_start(GRID, CellId::ROOT);
        let end = world_range_end(GRID, CellId::ROOT);
        assert_eq!(
            start,
            [b'w', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            "root starts at cell 1 in its grid"
        );
        assert_eq!(
            end,
            [b'w', 0, 0, 0, 1],
            "root's grid-0 subtree spans every world key of grid 0, not grid 1"
        );
        // A key under any valid cell still sorts inside the span.
        let k = world_key(GRID, CellId::from_bits(CELL).unwrap(), PersistId::new(1));
        assert!(k.as_slice() >= start.as_slice() && k.as_slice() < end.as_slice());
    }

    #[test]
    fn outermost_octant_subtree_is_unbounded() {
        // The (1,1,1) level-1 octant abuts the top of the u64 space: its subtree
        // end is u64::MAX, so the scan bound must reach into the next grid (P-3).
        let octant = CellId::from_bits(0xF000_0000_0000_0000).unwrap();
        assert_eq!(octant.subtree_range(), 0xE000_0000_0000_0001..=u64::MAX);
        assert_eq!(
            world_range_end(GRID, octant),
            [b'w', 0, 0, 0, 1],
            "the octant's unbounded subtree ends at grid 0's next-grid bound"
        );
    }

    #[test]
    fn grids_are_disjoint_under_identical_cells() {
        // P-7: the same (cell, entity) under two grids must produce disjoint
        // keys, and a grid-scoped span must never swallow another grid's rows —
        // nested-grid content and root-grid content with equal CellIds coexist.
        let a = GridId::ROOT;
        let b = GridId::new(3);
        let cell = CellId::from_bits(CELL).unwrap();
        let entity = PersistId::new(1);

        let ka = world_key(a, cell, entity);
        let kb = world_key(b, cell, entity);
        assert_ne!(ka, kb, "grid id discriminates the key");
        assert!(ka.as_slice() < kb.as_slice(), "grid ids sort by id");

        // A grid-3 scan of the same shard excludes grid 0's row and vice versa.
        let sa = world_range_start(a, CellId::from_bits(SHARD).unwrap());
        let ea = world_range_end(a, CellId::from_bits(SHARD).unwrap());
        assert!(!(kb.as_slice() >= sa.as_slice() && kb.as_slice() < ea.as_slice()));
        let sb = world_range_start(b, CellId::from_bits(SHARD).unwrap());
        let eb = world_range_end(b, CellId::from_bits(SHARD).unwrap());
        assert!(kb.as_slice() >= sb.as_slice() && kb.as_slice() < eb.as_slice());
        assert!(!(ka.as_slice() >= sb.as_slice() && ka.as_slice() < eb.as_slice()));
    }

    #[test]
    fn ckpt_keys_are_grid_scoped() {
        // P-7: two grids checkpointing the same shard id never collide.
        let shard = CellId::from_bits(SHARD).unwrap();
        assert_ne!(
            ckpt_key(GridId::ROOT, shard),
            ckpt_key(GridId::new(9), shard)
        );
        assert_eq!(ckpt_key(GridId::ROOT, shard).len(), 13);
    }

    #[test]
    fn tombstone_value_roundtrips_and_is_distinct_from_live() {
        // P-6: the tombstone encoding is unambiguous against a live bag — the
        // tag byte is the discriminator, and the marker decodes back.
        let tomb = Tombstone {
            cell: CellId::from_bits(CELL).unwrap(),
            tick: Tick::new(123_456),
            gc_deadline_ms: 1_700_100_000_000,
        };
        let encoded = encode_tombstone_value(&tomb).unwrap();
        assert_eq!(encoded[0], TOMBSTONE_TAG);
        assert_eq!(
            postcard::from_bytes::<Tombstone>(&encoded[1..]).unwrap(),
            tomb
        );
        // A live bag with the same payload sorts differently in the tag byte.
        let live = encode_live_value(b"hp=100");
        assert_ne!(encoded[0], live[0]);
    }

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

    #[test]
    fn from_bits_roundtrips_to_bits() {
        assert_eq!(CellId::from_bits(SHARD).unwrap().to_bits(), SHARD);
        assert_eq!(CellId::from_bits(CELL).unwrap().to_bits(), CELL);
        assert!(CellId::from_bits(0).is_none());
        assert_eq!(CellId::from_bits(u64::MAX).unwrap().to_bits(), u64::MAX);
    }
}
