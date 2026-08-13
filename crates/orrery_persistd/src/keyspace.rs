//! FDB keyspace layout — one public definition of every key family.
//!
//! This module is the **single source** for the cluster keyspace
//! (docs/08-persistence.md §6). The checkpointer, the cold reader, the seeder,
//! the fence store, and any future reader all call these same functions so key
//! layout is defined once and tested once. It compiles and is testable without
//! the `fdb` feature — it is pure byte layout.
//!
//! Normative source: docs/08-persistence.md §6, docs/12-world-seeding.md §9.2,
//! §9.3, §11.1, and docs/DECISIONS.md D11.

use orrery_protocol::{CellId, GridId, PersistId};

use crate::actor::Tombstone;
use crate::checkpoint::CheckpointError;

// ---------------------------------------------------------------------------
// World row family: `world/{grid_id}/{cell_id}/{entity_id}`
// ---------------------------------------------------------------------------

/// Key prefix for entity rows: `world/{grid_id}/{cell_id}/{entity_id}`.
///
/// `cell_id` is the entity's own interest cell (P-2, §6) and `entity_id` is its
/// 8-byte `PersistId`, all big-endian so range scans inherit Morton order
/// (§6: a shard cell's subtree is one contiguous range). The 4-byte `GridId`
/// (P-7) scopes the row to its grid's `CellId` space: the same cell id under
/// two grids is two disjoint keys, and a per-grid subtree scan is one range.
#[must_use]
pub fn world_key(grid: GridId, cell: CellId, entity: PersistId) -> [u8; 21] {
    let mut key = [0u8; 21];
    key[0] = b'w';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&cell.to_bits().to_be_bytes());
    key[13..21].copy_from_slice(&entity.0.to_be_bytes());
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
#[must_use]
pub fn world_range_start(grid: GridId, shard: CellId) -> Vec<u8> {
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
#[must_use]
pub fn world_range_end(grid: GridId, shard: CellId) -> Vec<u8> {
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

/// Decode a `world/` key back into its `(grid, cell, entity)` components.
///
/// This is the inverse of [`world_key`]. Returns `None` for any key that is
/// not exactly 21 bytes starting with `b'w'`. The cold reader and the seeder's
/// verify pass use this to recover the cell from a scanned key rather than
/// re-deriving it.
#[must_use]
pub fn decode_world_key(key: &[u8]) -> Option<(GridId, CellId, PersistId)> {
    if key.len() != 21 || key[0] != b'w' {
        return None;
    }
    let grid = GridId(u32::from_be_bytes(key[1..5].try_into().ok()?));
    let cell = CellId::from_bits(u64::from_be_bytes(key[5..13].try_into().ok()?))?;
    let entity = PersistId::new(u64::from_be_bytes(key[13..21].try_into().ok()?));
    Some((grid, cell, entity))
}

// ---------------------------------------------------------------------------
// World row value tags
// ---------------------------------------------------------------------------

/// The one-byte tag prefix of a `world/` value: distinguishes a live component
/// bag from a despawn tombstone (P-6, D11 §6).
///
/// Values are `LIVE_TAG ‖ component bag` or `TOMBSTONE_TAG ‖ postcard(Tombstone)`.
/// The tag lives in the key's value, never the key, so live rows, tombstone
/// rows, and the seeder's rows share one key convention.
pub const LIVE_TAG: u8 = 0x00;
/// The tombstone tag (P-6).
pub const TOMBSTONE_TAG: u8 = 0x01;

/// Encode a live entity value: `LIVE_TAG ‖ components`.
#[must_use]
pub fn encode_live_value(components: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(components.len() + 1);
    value.push(LIVE_TAG);
    value.extend_from_slice(components);
    value
}

/// Encode a despawn marker value: `TOMBSTONE_TAG ‖ postcard(Tombstone)`.
///
/// # Errors
///
/// Returns [`CheckpointError::Store`] if postcard serialization fails (should
/// never happen for a well-formed tombstone).
pub fn encode_tombstone_value(tombstone: &Tombstone) -> Result<Vec<u8>, CheckpointError> {
    let mut value = Vec::with_capacity(1 + 32);
    value.push(TOMBSTONE_TAG);
    value.extend_from_slice(
        &postcard::to_stdvec(tombstone)
            .map_err(|e| CheckpointError::Store(format!("tombstone encode: {e}")))?,
    );
    Ok(value)
}

// ---------------------------------------------------------------------------
// Checkpoint watermark family: `ckpt/{grid_id}/{shard}`
// ---------------------------------------------------------------------------

/// Key prefix for the checkpoint watermark: `ckpt/{grid_id}/{shard}` (P-7:
/// a shard cell id is grid-relative, so two grids never share a row).
#[must_use]
pub fn ckpt_key(grid: GridId, shard: CellId) -> [u8; 13] {
    let mut key = [0u8; 13];
    key[0] = b'c';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

// ---------------------------------------------------------------------------
// Fence / actor family: `actor/{shard_cell_id}`
// ---------------------------------------------------------------------------

/// Key for the fence row: `actor/{shard_cell_id}`.
///
/// `shard_cell_id` is the 8-byte big-endian Morton `CellId` (D11 §6).
#[must_use]
pub fn fence_key(shard: CellId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'a';
    key[1..9].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

// ---------------------------------------------------------------------------
// Seed content map family: `seedmap/{content_key}`
// ---------------------------------------------------------------------------
//
// The idmap subspace (docs/12-world-seeding.md §9.2): maps a 16-byte
// `ContentKey` to the minted `PersistId`, the entity's (grid, cell), and the
// first_seen_build. Used so a re-seed does not renumber the world.

/// Key for a seedmap row: `seedmap/{content_key}` where `content_key` is 16
/// bytes (docs/12-world-seeding.md §9.2).
///
/// The `ContentKey` is already a blake3 digest truncated to 16 bytes; it is
/// taken here as a raw `[u8; 16]` to avoid adding a blake3 dependency.
#[must_use]
pub fn seedmap_key(content_key: [u8; 16]) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[0] = b's';
    key[1..].copy_from_slice(&content_key);
    key
}

/// The first byte of the `seedmap/` family span.
#[must_use]
pub fn seedmap_range_start() -> Vec<u8> {
    vec![b's']
}

/// The exclusive end of the `seedmap/` family span (one byte past `s`).
#[must_use]
pub fn seedmap_range_end() -> Vec<u8> {
    vec![b't']
}

// ---------------------------------------------------------------------------
// Seed progress family: `seedprog/{emit_hash}/{grid}/{cell}`
// ---------------------------------------------------------------------------
//
// Per-subtree resume markers (docs/12-world-seeding.md §11.1). The key encodes
// the emitting subtree so that generation is resumable at cell granularity.

/// Key for a seed progress row: `seedprog/{emit_hash}/{grid_id}/{cell_id}`.
///
/// `emit_hash` is the first 8 bytes of `blake3(emit_name)`, pre-hashed by the
/// caller so this module does not depend on blake3. The fixed-width 8-byte hash
/// keeps the key layout fixed-length (docs/12-world-seeding.md §11.1).
#[must_use]
pub fn seedprog_key(emit_hash: [u8; 8], grid: GridId, cell: CellId) -> [u8; 21] {
    let mut key = [0u8; 21];
    key[0] = b'p';
    key[1..9].copy_from_slice(&emit_hash);
    key[9..13].copy_from_slice(&grid.0.to_be_bytes());
    key[13..21].copy_from_slice(&cell.to_bits().to_be_bytes());
    key
}

/// The first byte of the `seedprog/` family span.
#[must_use]
pub fn seedprog_range_start() -> Vec<u8> {
    vec![b'p']
}

/// The exclusive end of the `seedprog/` family span (one byte past `p`).
#[must_use]
pub fn seedprog_range_end() -> Vec<u8> {
    vec![b'q']
}

/// The range covering all `seedprog/` rows for one emit hash.
///
/// Returns `(start, exclusive_end)` over the full key space of that emit's
/// subtree markers — every `(grid, cell)` under this emit hash — so the caller
/// can clear them by range.
#[must_use]
pub fn seedprog_emit_range(emit_hash: [u8; 8]) -> (Vec<u8>, Vec<u8>) {
    let mut start = Vec::with_capacity(9);
    start.push(b'p');
    start.extend_from_slice(&emit_hash);

    // The exclusive end: `b'p' ‖ (emit_hash + 1)`, or if emit_hash ==
    // [0xFF; 8] then the end is `b'q'` (the single byte past the prefix,
    // which is also the family-level end bound).
    let mut end = Vec::with_capacity(9);
    // Check whether emit_hash is all-0xFF.
    if emit_hash.iter().all(|&b| b == 0xFF) {
        // Overflow: the next byte past this hash is the family end.
        end.push(b'q');
    } else {
        // Carry-add 1 to the 8-byte big-endian emit_hash.
        let mut hash = u64::from_be_bytes(emit_hash);
        hash = hash.wrapping_add(1);
        end.push(b'p');
        end.extend_from_slice(&hash.to_be_bytes());
    }
    (start, end)
}

// ---------------------------------------------------------------------------
// Terrain chunk family: `chunk/{grid}/{cell}/{n}`
// ---------------------------------------------------------------------------
//
// Terrain shard rows (docs/08-persistence.md §6, §8). Each row is a ≤100 KB
// compressed terrain section. `n` is the section index, big-endian u16, so
// sections of one cell sort together and a whole cell's sections form one
// contiguous key range.

/// Key for a terrain chunk row: `chunk/{grid_id}/{cell_id}/{n}`.
///
/// `n` is the section index, a big-endian `u16`. The same grid scoping and
/// subtree-span reasoning as `world/` (docs/08-persistence.md §6, §8).
#[must_use]
pub fn chunk_key(grid: GridId, cell: CellId, section: u16) -> [u8; 15] {
    let mut key = [0u8; 15];
    key[0] = b'k';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&cell.to_bits().to_be_bytes());
    key[13..15].copy_from_slice(&section.to_be_bytes());
    key
}

/// The first key of the `chunk/{grid_id}/{cell_id}/…` span for `shard` in
/// `grid`.
///
/// Same subtree-span reasoning as [`world_range_start`] — a shard's chunk
/// subtree is one contiguous range (D11 §6, parent = prefix). The span
/// includes all chunk rows for every interest cell in the shard's subtree,
/// at every section index.
#[must_use]
pub fn chunk_range_start(grid: GridId, shard: CellId) -> Vec<u8> {
    let range = shard.subtree_range();
    let mut key = Vec::with_capacity(13);
    key.push(b'k');
    key.extend_from_slice(&grid.0.to_be_bytes());
    key.extend_from_slice(&range.start().to_be_bytes());
    key
}

/// The exclusive end of the `chunk/{grid_id}/{cell_id}/…` subtree span for
/// `shard` in `grid`.
///
/// Same edge-case handling as [`world_range_end`]: when the subtree abuts
/// `u64::MAX` the bound reaches into the next grid, and at the topmost grid
/// id the bound is the single byte past the `'k'` prefix.
#[must_use]
pub fn chunk_range_end(grid: GridId, shard: CellId) -> Vec<u8> {
    let range = shard.subtree_range();
    let end = *range.end();
    if end < u64::MAX {
        let mut key = Vec::with_capacity(13);
        key.push(b'k');
        key.extend_from_slice(&grid.0.to_be_bytes());
        key.extend_from_slice(&(end + 1).to_be_bytes());
        key
    } else if grid.0 < u32::MAX {
        let mut key = Vec::with_capacity(5);
        key.push(b'k');
        key.extend_from_slice(&(grid.0 + 1).to_be_bytes());
        key
    } else {
        vec![b'l']
    }
}

// ---------------------------------------------------------------------------
// Content version row: `content/version`
// ---------------------------------------------------------------------------
//
// A single row recording the content build id, manifest digest, scenario seed,
// config digest, toolchain, and seeded_at timestamp (docs/12-world-seeding.md
// §9.3).

/// The single-byte key for the `content/version` row (docs/12-world-seeding.md
/// §9.3). The value is `(content_build, manifest_digest, scenario_seed,
/// config_digest, toolchain, seeded_at)`.
#[must_use]
pub fn content_version_key() -> [u8; 1] {
    [b'v']
}

// ---------------------------------------------------------------------------
// Intent idempotency family: `intent/{intent_id}`
// ---------------------------------------------------------------------------
//
// Records the outcome of a signed, witness-attested intent
// (docs/08-persistence.md §2.2, §6). The value is a postcard-encoded
// `IntentOutcome`. A duplicate submission reads the recorded outcome rather
// than re-executing.

/// Key for the intent idempotency row: `intent/{intent_id}` where
/// `intent_id` is a 16-byte `u128` encoded big-endian
/// (docs/08-persistence.md §2.2, §6).
///
/// `Intent::intent_id` is a `u128`; the value is a postcard-encoded
/// `IntentOutcome`. A duplicate submission returns the recorded outcome rather
/// than re-executing the intent.
#[must_use]
pub fn intent_key(intent_id: u128) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[0] = b'i';
    key[1..].copy_from_slice(&intent_id.to_be_bytes());
    key
}

/// The first byte of the `intent/` family span.
#[must_use]
pub fn intent_range_start() -> Vec<u8> {
    vec![b'i']
}

/// The exclusive end of the `intent/` family span (one byte past `i`).
#[must_use]
pub fn intent_range_end() -> Vec<u8> {
    vec![b'j']
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Worked-example constants matching docs/01-spatial-model.md §3.3.
    const SHARD: u64 = 0xA924_9249_2492_4E00;
    const CELL: u64 = 0xA924_9249_2492_4D65;
    const FOREIGN: u64 = 0xA924_9249_2492_5200;

    const GRID: GridId = GridId::ROOT;

    // -----------------------------------------------------------------------
    // World key layout (ported from checkpoint/fdb.rs)
    // -----------------------------------------------------------------------

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
        // under SHARD falls inside the shard's span, a row under FOREIGN does not.
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
            tick: orrery_protocol::Tick::new(123_456),
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
    fn from_bits_roundtrips_to_bits() {
        assert_eq!(CellId::from_bits(SHARD).unwrap().to_bits(), SHARD);
        assert_eq!(CellId::from_bits(CELL).unwrap().to_bits(), CELL);
        assert!(CellId::from_bits(0).is_none());
        assert_eq!(CellId::from_bits(u64::MAX).unwrap().to_bits(), u64::MAX);
    }

    // -----------------------------------------------------------------------
    // Seedmap key layout
    // -----------------------------------------------------------------------

    #[test]
    fn seedmap_key_is_17_bytes_with_s_prefix() {
        // Independently computed: `b's'` (0x73) followed by 16 content key bytes.
        let ck = [0xABu8; 16];
        let key = seedmap_key(ck);
        assert_eq!(key.len(), 17, "seedmap key is 17 bytes");
        assert_eq!(key[0], b's', "first byte is 's'");

        let mut expected = [0u8; 17];
        expected[0] = b's';
        expected[1..].copy_from_slice(&ck);
        assert_eq!(
            key, expected,
            "all 17 bytes match independently-computed layout"
        );
    }

    #[test]
    fn seedmap_range_spans_s_prefix() {
        let start = seedmap_range_start();
        let end = seedmap_range_end();
        assert_eq!(start, vec![b's']);
        assert_eq!(end, vec![b't']);
        // A real key falls within this span.
        let key = seedmap_key([0xAB; 16]);
        assert!(key.as_slice() >= start.as_slice());
        assert!(key.as_slice() < end.as_slice());
    }

    // -----------------------------------------------------------------------
    // Seedprog key layout
    // -----------------------------------------------------------------------

    #[test]
    fn seedprog_key_is_21_bytes_with_p_prefix() {
        let emit_hash = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let grid = GridId::new(42);
        let cell = CellId::from_bits(SHARD).unwrap();
        let key = seedprog_key(emit_hash, grid, cell);
        assert_eq!(key.len(), 21, "seedprog key is 21 bytes");
        assert_eq!(key[0], b'p', "first byte is 'p'");
        assert_eq!(key[1..9], emit_hash, "bytes 1-9 are the emit hash");
        assert_eq!(
            u32::from_be_bytes(key[9..13].try_into().unwrap()),
            grid.0,
            "bytes 9-12 are the grid id"
        );
        assert_eq!(
            u64::from_be_bytes(key[13..21].try_into().unwrap()),
            cell.to_bits(),
            "bytes 13-20 are the cell id"
        );

        // Independently computed: full key.
        let mut expected = [0u8; 21];
        expected[0] = b'p';
        expected[1..9].copy_from_slice(&emit_hash);
        expected[9..13].copy_from_slice(&42u32.to_be_bytes());
        expected[13..21].copy_from_slice(&cell.to_bits().to_be_bytes());
        assert_eq!(key, expected);
    }

    #[test]
    fn seedprog_range_spans_p_prefix() {
        let start = seedprog_range_start();
        let end = seedprog_range_end();
        assert_eq!(start, vec![b'p']);
        assert_eq!(end, vec![b'q']);
        // A real key falls within this span.
        let key = seedprog_key([0xDE; 8], GridId::ROOT, CellId::from_bits(SHARD).unwrap());
        assert!(key.as_slice() >= start.as_slice());
        assert!(key.as_slice() < end.as_slice());
    }

    #[test]
    fn seedprog_emit_range_bounds_cover_same_hash() {
        let hash = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let (start, end) = seedprog_emit_range(hash);
        // Start is b'p' ‖ hash.
        assert_eq!(&start[..1], b"p");
        assert_eq!(start[1..], hash);
        // End is b'p' ‖ (hash + 1).
        assert_eq!(&end[..1], b"p");
        let hash_plus_one = u64::from_be_bytes(hash).wrapping_add(1);
        assert_eq!(end[1..], hash_plus_one.to_be_bytes());

        // Keys with this hash fall inside.
        let key_a = seedprog_key(hash, GridId::new(1), CellId::from_bits(SHARD).unwrap());
        assert!(key_a.as_slice() >= start.as_slice());
        assert!(key_a.as_slice() < end.as_slice());

        // Keys with a different hash fall outside.
        let mut other_hash = hash;
        other_hash[7] = hash[7].wrapping_add(1);
        let key_b = seedprog_key(other_hash, GridId::ROOT, CellId::ROOT);
        assert!(
            key_b.as_slice() < start.as_slice() || key_b.as_slice() >= end.as_slice(),
            "a key from a different emit hash must lie outside this emit's range"
        );
    }

    #[test]
    fn seedprog_emit_range_handles_max_hash() {
        // When emit_hash is [0xFF; 8], the emit range end is b'q' (the family
        // boundary), not a wrapped hash.
        let max_hash = [0xFFu8; 8];
        let (_start, end) = seedprog_emit_range(max_hash);
        assert_eq!(
            end,
            vec![b'q'],
            "max-hash emit range end is the family boundary 'q'"
        );
    }

    // -----------------------------------------------------------------------
    // Content version key layout
    // -----------------------------------------------------------------------

    #[test]
    fn content_version_key_is_single_v_byte() {
        let key = content_version_key();
        assert_eq!(key.len(), 1);
        assert_eq!(key[0], b'v', "content version key is the single byte 'v'");
    }

    // -----------------------------------------------------------------------
    // Intent key layout
    // -----------------------------------------------------------------------

    #[test]
    fn intent_key_is_17_bytes_with_i_prefix_and_big_endian_u128() {
        let id: u128 = 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10;
        let key = intent_key(id);
        assert_eq!(key.len(), 17, "intent key is 17 bytes");
        assert_eq!(key[0], b'i', "first byte is 'i'");

        let mut expected = [0u8; 17];
        expected[0] = b'i';
        expected[1..].copy_from_slice(&id.to_be_bytes());
        assert_eq!(key, expected, "big-endian u128 layout");

        // Sort order: smaller intent_id sorts before larger.
        let small = intent_key(0);
        let large = intent_key(u128::MAX);
        assert!(
            small.as_slice() < large.as_slice(),
            "smaller intent_id sorts before larger"
        );
    }

    #[test]
    fn intent_range_spans_i_prefix() {
        let start = intent_range_start();
        let end = intent_range_end();
        assert_eq!(start, vec![b'i']);
        assert_eq!(end, vec![b'j']);
        let key = intent_key(42);
        assert!(key.as_slice() >= start.as_slice());
        assert!(key.as_slice() < end.as_slice());
    }

    // -----------------------------------------------------------------------
    // Terrain chunk key layout
    // -----------------------------------------------------------------------

    #[test]
    fn chunk_key_is_15_bytes_with_k_prefix() {
        let grid = GridId::new(7);
        let cell = CellId::from_bits(SHARD).unwrap();
        let key = chunk_key(grid, cell, 42);
        assert_eq!(key.len(), 15, "chunk key is 15 bytes");
        assert_eq!(key[0], b'k', "first byte is 'k'");
        assert_eq!(
            u32::from_be_bytes(key[1..5].try_into().unwrap()),
            grid.0,
            "bytes 1-4 are the grid id"
        );
        assert_eq!(
            u64::from_be_bytes(key[5..13].try_into().unwrap()),
            cell.to_bits(),
            "bytes 5-12 are the cell id"
        );
        assert_eq!(
            u16::from_be_bytes(key[13..15].try_into().unwrap()),
            42,
            "bytes 13-14 are the section index"
        );

        // Independently computed.
        let mut expected = [0u8; 15];
        expected[0] = b'k';
        expected[1..5].copy_from_slice(&7u32.to_be_bytes());
        expected[5..13].copy_from_slice(&cell.to_bits().to_be_bytes());
        expected[13..15].copy_from_slice(&42u16.to_be_bytes());
        assert_eq!(key, expected);
    }

    #[test]
    fn chunk_key_sections_sort_contiguously() {
        let cell = CellId::from_bits(SHARD).unwrap();
        let keys: Vec<[u8; 15]> = (0..5).map(|n| chunk_key(GRID, cell, n)).collect();
        for i in 1..keys.len() {
            assert!(
                keys[i - 1].as_slice() < keys[i].as_slice(),
                "chunk sections sort by section index"
            );
        }
    }

    #[test]
    fn chunk_range_uses_subtree_same_as_world() {
        let shard = CellId::from_bits(SHARD).unwrap();
        let w_start = world_range_start(GRID, shard);
        let w_end = world_range_end(GRID, shard);
        let c_start = chunk_range_start(GRID, shard);
        let c_end = chunk_range_end(GRID, shard);

        // Same cell-id span as world; only the prefix byte differs.
        assert_eq!(c_start[0], b'k');
        assert_eq!(c_end[0], b'k');
        assert_eq!(c_start[1..], w_start[1..], "same subtree start as world");
        assert_eq!(c_end[1..], w_end[1..], "same subtree end as world");

        // A chunk key inside the span sorts in.
        let inner = chunk_key(GRID, CellId::from_bits(CELL).unwrap(), 0);
        assert!(inner.as_slice() >= c_start.as_slice());
        assert!(inner.as_slice() < c_end.as_slice());
    }

    #[test]
    fn chunk_subtree_is_unbounded_at_root() {
        // Same as world's root: the whole grid's chunk rows.
        let start = chunk_range_start(GRID, CellId::ROOT);
        let end = chunk_range_end(GRID, CellId::ROOT);
        assert_eq!(
            start,
            [b'k', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            "root chunk starts at cell 1"
        );
        assert_eq!(
            end,
            [b'k', 0, 0, 0, 1],
            "root's grid-0 chunk subtree spans every chunk key of grid 0"
        );
    }

    // -----------------------------------------------------------------------
    // decode_world_key
    // -----------------------------------------------------------------------

    #[test]
    fn decode_world_key_roundtrips() {
        let triples = [
            (GridId::ROOT, CellId::ROOT, PersistId::new(0)),
            (
                GridId::ROOT,
                CellId::from_bits(SHARD).unwrap(),
                PersistId::new(1),
            ),
            (
                GridId::new(7),
                CellId::from_bits(CELL).unwrap(),
                PersistId::new(u64::MAX),
            ),
            (
                GridId::new(u32::MAX),
                CellId::from_bits(0xA924_9249_2492_4D65).unwrap(),
                PersistId::new(42),
            ),
        ];
        for &(grid, cell, entity) in &triples {
            let key = world_key(grid, cell, entity);
            let decoded = decode_world_key(&key);
            assert_eq!(decoded, Some((grid, cell, entity)));
        }
    }

    #[test]
    fn decode_world_key_rejects_wrong_prefix() {
        let key = ckpt_key(GRID, CellId::from_bits(SHARD).unwrap());
        assert!(
            decode_world_key(&key).is_none(),
            "ckpt key does not start with 'w'"
        );
    }

    #[test]
    fn decode_world_key_rejects_wrong_length() {
        // Too short.
        let key = [b'w', 0, 0, 0, 0];
        assert!(decode_world_key(&key).is_none(), "short key rejected");

        // Too long.
        let mut key = [0u8; 22];
        key[0] = b'w';
        assert!(decode_world_key(&key).is_none(), "long key rejected");

        // Empty.
        assert!(decode_world_key(b"").is_none(), "empty key rejected");
    }

    #[test]
    fn decode_world_key_rejects_invalid_cell_id() {
        // Cell id 0 is invalid (CellId::from_bits returns None for zero).
        let mut key = [0u8; 21];
        key[0] = b'w';
        // grid = 0 (already zero), cell = 0 (invalid), entity = 0.
        assert!(decode_world_key(&key).is_none(), "cell id 0 is invalid");
    }

    // -----------------------------------------------------------------------
    // Pairwise disjointness: every key family's range is non-overlapping
    // -----------------------------------------------------------------------

    /// Each family is described by its first-byte prefix and an (inclusive)
    /// maximum key. Families are disjoint iff for every pair (a, b):
    ///   max_key(a) < min_key(b)  or  max_key(b) < min_key(a)
    /// For fixed-byte-prefix families (all of ours), min_key = [prefix] and
    /// max_key sorts before [prefix+1], so we just check that the prefix bytes
    /// are all different.
    ///
    /// This test provides an explicit concrete assertion for each pair so a
    /// future addition that reuses a prefix is caught with the offending pair
    /// named.
    #[test]
    fn all_key_families_are_range_disjoint() {
        // The seven families and their one-byte prefix.
        struct Family {
            prefix: u8,
        }

        let shard = CellId::from_bits(SHARD).unwrap();
        let families = [
            Family { prefix: b'a' }, // fence/actor
            Family { prefix: b'c' }, // ckpt
            Family { prefix: b'i' }, // intent
            Family { prefix: b'k' }, // chunk
            Family { prefix: b'p' }, // seedprog
            Family { prefix: b's' }, // seedmap
            Family { prefix: b'v' }, // content/version
            Family { prefix: b'w' }, // world
        ];

        // All prefix bytes must be distinct.
        let mut prefixes: Vec<u8> = families.iter().map(|f| f.prefix).collect();
        prefixes.sort();
        prefixes.dedup();
        assert_eq!(
            prefixes.len(),
            families.len(),
            "all eight family prefixes must be distinct"
        );

        // For each pair (a, b), verify that a key from family A sorts before
        // a key from family B when prefix_a < prefix_b. Since each family has
        // a fixed one-byte prefix, the full-family range is [prefix, prefix+1),
        // so distinct prefixes guarantee disjoint ranges.
        let keys: [Vec<u8>; 8] = [
            fence_key(shard).to_vec(),                          // 'a'
            ckpt_key(GRID, shard).to_vec(),                     // 'c'
            intent_key(42).to_vec(),                            // 'i'
            chunk_key(GRID, shard, 0).to_vec(),                 // 'k'
            seedprog_key([0xDE; 8], GRID, shard).to_vec(),      // 'p'
            seedmap_key([0xAB; 16]).to_vec(),                   // 's'
            content_version_key().to_vec(),                     // 'v'
            world_key(GRID, shard, PersistId::new(1)).to_vec(), // 'w'
        ];
        for (i, ka) in keys.iter().enumerate() {
            for (j, kb) in keys.iter().enumerate() {
                if i >= j {
                    continue;
                }
                assert!(
                    ka.as_slice() < kb.as_slice(),
                    "key family at index {i} should sort before family at index {j}"
                );
            }
        }
    }

    #[test]
    fn prefix_order_is_ascii_and_stable() {
        // The prefix bytes must be chosen so that no prefix is a prefix of
        // another family's range end. Since we use single bytes and ensure
        // ranges are [prefix, prefix+1), this is automatic if prefixes are
        // distinct. This test records the sort order for documentation.
        let order = [
            (b'a', "fence/actor"),
            (b'c', "ckpt"),
            (b'i', "intent"),
            (b'k', "chunk"),
            (b'p', "seedprog"),
            (b's', "seedmap"),
            (b'v', "content/version"),
            (b'w', "world"),
        ];
        for (i, (p1, n1)) in order.iter().enumerate() {
            for (j, (p2, n2)) in order.iter().enumerate() {
                if i < j {
                    assert!(
                        p1 < p2,
                        "prefix {n1} (0x{p1:02x}) should sort before {n2} (0x{p2:02x})"
                    );
                }
            }
        }
        // Verify there is no gap where a different prefix could collide.
        let mut prev = 0u8;
        for (p, name) in &order {
            assert!(
                *p > prev,
                "prefix bytes are in strictly increasing order; {name} = 0x{p:02x} after 0x{prev:02x}"
            );
            prev = *p;
        }
    }
}
