//! FDB keyspace layout — one public definition of every key family.
//!
//! This module is the **single source** for the cluster keyspace
//! (docs/08-persistence.md §6). The checkpointer, the cold reader, the seeder,
//! the fence store, and any future reader all call these same functions so key
//! layout is defined once and tested once. It compiles and is testable without
//! the `fdb` feature — it is pure byte layout.
//!
//! Normative source: docs/08-persistence.md §6, docs/12-world-seeding.md §9.2,
//! §9.3, §11.1, and docs/adr/0011-persistence.md.

use orrery_protocol::atrest::{EncodingVersion, SchemaVersion, SCHEMA_V0};
use orrery_protocol::{AccountId, AssetId, CellId, GridId, ItemUid, Lsn, NodeId, PersistId};

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
///
/// **This tag is the v0 live envelope** (D38 clause (d)(1)). It carries no
/// schema marker, so under the bootstrap rule of [`orrery_protocol::atrest`] a
/// row wearing it is [`SCHEMA_V0`] — read, not refused, and not guessed at by
/// peeking into the bag. Writers in this workspace emit
/// [`LIVE_VERSIONED_TAG`] instead; this one stays because the rows already on
/// disk wear it and must keep decoding.
pub const LIVE_TAG: u8 = 0x00;
/// The tombstone tag (P-6).
pub const TOMBSTONE_TAG: u8 = 0x01;
/// The versioned live tag (D38 clause (d)(2)): `LIVE_VERSIONED_TAG ‖
/// schema_floor:u32 BE ‖ component bag`.
///
/// **The marker persistd can read without decoding game types.** docs/08 §16
/// puts schema versions *inside* the bag, per component, and also asks a
/// background sweep to walk cold ranges — and the cell actor never decodes
/// game types (§3.1). Something outside the bag therefore has to answer "is
/// this row behind?", which is what the floor is: the **minimum** schema
/// version over the bag's component slots, stamped by persistd on every
/// write-back. Per-component versions govern *what* migrates; this governs
/// *whether*, at a fixed offset, to code that never opens the bag.
///
/// A floor rather than a generation counter because it is derivable from the
/// bag it describes (see `ComponentBag::schema_floor` in [`crate::schema`])
/// rather than being a second, independently-driftable number: a row whose
/// floor disagrees with its bag is a bug the bag itself convicts, whereas a
/// counter's disagreement is undetectable.
///
/// Big-endian so a value dump sorts and reads the way every other multi-byte
/// field in this module does.
pub const LIVE_VERSIONED_TAG: u8 = 0x02;

/// The width of the schema-floor field in a [`LIVE_VERSIONED_TAG`] value.
const SCHEMA_FLOOR_LEN: usize = 4;

/// The fixed prefix a [`LIVE_VERSIONED_TAG`] value carries before its bag:
/// the tag byte plus the floor. Named because it is the offset the sweep's
/// read is "fixed" at, and because the accessors below all bound against it.
const LIVE_VERSIONED_HEADER_LEN: usize = 1 + SCHEMA_FLOOR_LEN;

/// Encode a live entity value in the v0 envelope: `LIVE_TAG ‖ components`.
///
/// Retained for the callers that genuinely mean "no schema statement" —
/// chiefly tests that construct the legacy shape on purpose. New writers use
/// [`encode_versioned_live_value`], which states the floor rather than leaving
/// a reader to bootstrap it.
#[must_use]
pub fn encode_live_value(components: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(components.len() + 1);
    value.push(LIVE_TAG);
    value.extend_from_slice(components);
    value
}

/// Encode a live entity value with its bag-level schema floor:
/// `LIVE_VERSIONED_TAG ‖ floor:u32 BE ‖ components`.
#[must_use]
pub fn encode_versioned_live_value(floor: SchemaVersion, components: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(components.len() + LIVE_VERSIONED_HEADER_LEN);
    value.push(LIVE_VERSIONED_TAG);
    value.extend_from_slice(&floor.to_be_bytes());
    value.extend_from_slice(components);
    value
}

/// The bag-level schema floor of a `world/` value, **without decoding the
/// bag**.
///
/// This is the read the background sweep of docs/08 §16 makes: it walks cold
/// `world/` ranges it cannot interpret and has to decide, per row, whether the
/// row is behind. Returns `None` for anything that is not a live row — a
/// tombstone carries no components and so no schema — and [`SCHEMA_V0`] for a
/// [`LIVE_TAG`] row, which is the bootstrap rule and not an inference about
/// its contents.
#[must_use]
pub fn world_value_schema_floor(value: &[u8]) -> Option<SchemaVersion> {
    match value.first() {
        Some(&LIVE_TAG) => Some(SCHEMA_V0),
        Some(&LIVE_VERSIONED_TAG) if value.len() >= LIVE_VERSIONED_HEADER_LEN => {
            let mut floor = [0u8; SCHEMA_FLOOR_LEN];
            floor.copy_from_slice(&value[1..LIVE_VERSIONED_HEADER_LEN]);
            Some(SchemaVersion::from_be_bytes(floor))
        }
        // A truncated versioned row, a tombstone, or a tag from a future
        // writer: not a live row this reader can speak for.
        _ => None,
    }
}

/// The component bag inside a `world/` live value, whichever envelope it
/// wears. `None` for a tombstone, an unknown tag, or a truncated value.
#[must_use]
pub fn world_value_components(value: &[u8]) -> Option<&[u8]> {
    match value.first() {
        Some(&LIVE_TAG) => Some(&value[1..]),
        Some(&LIVE_VERSIONED_TAG) if value.len() >= LIVE_VERSIONED_HEADER_LEN => {
            Some(&value[LIVE_VERSIONED_HEADER_LEN..])
        }
        _ => None,
    }
}

/// Whether a `world/` value is behind `current` — the sweep's filter (D38
/// clause (d)(2)).
///
/// False for anything that is not a live row: a tombstone has no schema to be
/// behind, and a row this reader cannot parse is not one it may declare stale.
#[must_use]
pub fn world_value_is_stale(value: &[u8], current: SchemaVersion) -> bool {
    world_value_schema_floor(value).is_some_and(|floor| floor < current)
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
// Fence / actor family: `actor/{grid}/{shard_cell_id}`
// ---------------------------------------------------------------------------

/// Key for the fence row: `actor/{grid}/{shard_cell_id}`.
///
/// `grid` scopes the shard to one nested-grid `CellId` space (P-7 / C-8),
/// and `shard_cell_id` is the 8-byte big-endian Morton `CellId` (D11 §6).
#[must_use]
pub fn fence_key(grid: GridId, shard: CellId) -> [u8; 13] {
    let mut key = [0u8; 13];
    key[0] = b'a';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

/// The first key of the `actor/{grid}/…` family span.
#[must_use]
pub fn fence_range_start() -> Vec<u8> {
    vec![b'a']
}

/// The exclusive end of the `actor/{grid}/…` family span.
#[must_use]
pub fn fence_range_end() -> Vec<u8> {
    vec![b'b']
}

/// The first key of the `actor/{grid}/…` span for one grid.
#[must_use]
pub fn fence_grid_range_start(grid: GridId) -> Vec<u8> {
    let mut key = Vec::with_capacity(5);
    key.push(b'a');
    key.extend_from_slice(&grid.0.to_be_bytes());
    key
}

/// The exclusive end of the `actor/{grid}/…` span for one grid.
#[must_use]
pub fn fence_grid_range_end(grid: GridId) -> Vec<u8> {
    if grid.0 < u32::MAX {
        let mut key = Vec::with_capacity(5);
        key.push(b'a');
        key.extend_from_slice(&(grid.0 + 1).to_be_bytes());
        key
    } else {
        vec![b'b']
    }
}

/// Durable registrar row `lease/{grid}/{entity}` — `le ‖ grid ‖ entity`.
///
/// The fourth ASCII-discriminated kind of the registered `l` family (D35
/// clause (a)): `'l'`, then the discriminator `'e'`, then the 4-byte
/// [`GridId`] and 8-byte [`PersistId`], both big-endian so rows sort by grid
/// within one sub-span. The sub-span is `[b"le", b"lf") ⊂ [b'l', b'm')`,
/// ordered `lb < le < li < lr`; the disjointness guard at the bottom of this
/// module proves it beside its three siblings.
///
/// **The discriminator exists because byte 1 must never be an id's high
/// byte.** Before D35 this row put `grid.0`'s most significant byte where
/// the ledger puts `b`/`i`/`r`, so a grid id ≥ `0x6200_0000` sorted a lease
/// row into `ledger/bal/` (`0x6900_0000` → items, `0x7200_0000` → receipts)
/// and the harness receipt scans over `[lr, ls)` would decode it as
/// corruption. The value encoding is unchanged: still a postcard-encoded
/// `orrery_protocol::Lease`, only the key mutated, with no migration — any
/// old-shape row is dev-cluster garbage by policy (D35 clause (b), audited
/// loudly in the fdb tier), and fencing turns a missed read into a re-claim,
/// never a rivalry.
#[must_use]
pub fn lease_key(grid: GridId, entity: PersistId) -> [u8; 14] {
    let mut key = [0; 14];
    key[0] = b'l';
    key[1] = b'e';
    key[2..6].copy_from_slice(&grid.0.to_be_bytes());
    key[6..].copy_from_slice(&entity.0.to_be_bytes());
    key
}

/// Registered `(byte-1 discriminator, key length)` shapes inside `[l, m)`.
///
/// Kept in byte order: `la` is the hot-ledger sweep cursor, `lb` balances,
/// `le` durable lease registrar rows, `li` item ownership, and `lr` receipts.
/// The ambient FoundationDB audit uses this list rather than restating it, and
/// the test-only family registry proves that both views contain the same bytes.
pub const L_FAMILY_KEY_SHAPES: &[(u8, usize)] =
    &[(b'a', 2), (b'b', 18), (b'e', 14), (b'i', 10), (b'r', 12)];

/// Whether `key` belongs to one of the registered sub-spans of `[l, m)`.
///
/// The exact lengths are load-bearing: a pre-D35 registrar row is 13 bytes, so
/// accepting discriminators alone would miss one whose grid high byte happened
/// to be `a`, `b`, `e`, `i`, or `r`.
#[must_use]
pub fn is_registered_l_family_key(key: &[u8]) -> bool {
    key.first() == Some(&b'l')
        && key.get(1).is_some_and(|byte| {
            L_FAMILY_KEY_SHAPES
                .iter()
                .any(|(registered, len)| byte == registered && key.len() == *len)
        })
}

/// Location index `lease-cell/{grid}/{cell}/{entity}` used for actor restore.
#[must_use]
pub fn lease_cell_key(grid: GridId, cell: CellId, entity: PersistId) -> [u8; 21] {
    let mut key = [0; 21];
    key[0] = b'm';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&cell.to_bits().to_be_bytes());
    key[13..].copy_from_slice(&entity.0.to_be_bytes());
    key
}
/// Reverse location index `lease-location/{grid}/{entity}` → `cell`.
///
/// The cell-first [`lease_cell_key`] makes shard startup scans contiguous;
/// this entity-first companion lets claim routing find the actor that already
/// owns a lease after a shard split, without scanning every location row.
#[must_use]
pub fn lease_location_key(grid: GridId, entity: PersistId) -> [u8; 13] {
    let mut key = [0; 13];
    key[0] = b'o';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..].copy_from_slice(&entity.0.to_be_bytes());
    key
}
/// Start of a shard's location-index subtree.
#[must_use]
pub fn lease_cell_range_start(grid: GridId, shard: CellId) -> Vec<u8> {
    let mut key = Vec::with_capacity(13);
    key.push(b'm');
    key.extend_from_slice(&grid.0.to_be_bytes());
    key.extend_from_slice(&shard.subtree_range().start().to_be_bytes());
    key
}
/// Exclusive end of a shard's location-index subtree.
#[must_use]
pub fn lease_cell_range_end(grid: GridId, shard: CellId) -> Vec<u8> {
    let end = *shard.subtree_range().end();
    if end < u64::MAX {
        let mut key = Vec::with_capacity(13);
        key.push(b'm');
        key.extend_from_slice(&grid.0.to_be_bytes());
        key.extend_from_slice(&(end + 1).to_be_bytes());
        key
    } else if grid.0 < u32::MAX {
        let mut key = Vec::with_capacity(5);
        key.push(b'm');
        key.extend_from_slice(&(grid.0 + 1).to_be_bytes());
        key
    } else {
        vec![b'n']
    }
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

/// Decode a `seedprog/{emit_hash}/{grid_id}/{cell_id}` key.
///
/// Returns `None` unless `key` has the fixed-width seed-progress layout.
#[must_use]
pub fn decode_seedprog_key(key: &[u8]) -> Option<([u8; 8], GridId, CellId)> {
    if key.len() != 21 || key[0] != b'p' {
        return None;
    }
    let mut emit_hash = [0u8; 8];
    emit_hash.copy_from_slice(&key[1..9]);
    let grid = GridId(u32::from_be_bytes(key[9..13].try_into().ok()?));
    let cell = CellId::from_bits(u64::from_be_bytes(key[13..21].try_into().ok()?))?;
    Some((emit_hash, grid, cell))
}

// D51 intentionally leaves one clean prefix byte rather than retaining an
// unwritten durable family as a placeholder. A later allocation is its own ADR.

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

/// Key for D32's durable `ramp/{control}` posture row.
///
/// Ramp rows share the registered `v` family and occupy the `b"vr"`
/// sub-span; `control` is one of D32 clause (c)'s stable control names.
#[must_use]
pub fn ramp_key(control: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + control.len());
    key.extend_from_slice(b"vr");
    key.extend_from_slice(control.as_bytes());
    key
}

/// Inclusive start of the durable posture-row sub-span.
#[must_use]
pub fn ramp_range_start() -> Vec<u8> {
    b"vr".to_vec()
}

/// Exclusive end of the durable posture-row sub-span.
#[must_use]
pub fn ramp_range_end() -> Vec<u8> {
    b"vs".to_vec()
}

/// Key for one D32 clause (e) cohort-membership row:
/// `rampc/{account}` where `account` is a big-endian `u64`.
///
/// The known-honest cohort is clause (e)'s `H`, and a membership row records
/// a human's sample decision — which half the account belongs to, when, and
/// why — so `|H| ≥ 100` can be assembled across restarts instead of living
/// only in one process's [`crate::intent::HonestCohort`].
///
/// The rows share the registered `v` family and occupy their own `b"vc"`
/// sub-span, per D32 clause (c)'s allocation rule: a new key kind takes an
/// ASCII sub-discriminator inside the family whose writer, retention and scan
/// profile it matches. The profile match is exact — written rarely by the
/// operator plane, read by measurement tooling, never on any hot path, and
/// retained for incident history — which is the same profile the record gives
/// as `v`'s. `b"vc"` sorts before the posture span, so the two sub-spans are
/// disjoint by construction and no family byte is spent.
#[must_use]
pub fn cohort_key(account: AccountId) -> Vec<u8> {
    let mut key = Vec::with_capacity(10);
    key.extend_from_slice(b"vc");
    key.extend_from_slice(&account.0.to_be_bytes());
    key
}

/// Inclusive start of the cohort-membership sub-span.
#[must_use]
pub fn cohort_range_start() -> Vec<u8> {
    b"vc".to_vec()
}

/// Exclusive end of the cohort-membership sub-span.
#[must_use]
pub fn cohort_range_end() -> Vec<u8> {
    b"vd".to_vec()
}

/// Key for one D32 clause (e) measurement-window row:
/// `rampw/{control}`, one per clause (c) control.
///
/// The row holds the window's bounds (`W`'s two endpoints) and the counters
/// observed inside it, with clause (e)'s armed/natural split carried through
/// verbatim. It exists because `W` and the tallies were fields of an
/// in-process `Mutex<Tallies>`: every `persistd` restart reset them, so
/// clause (e)'s `W ≥ 30 days` term was unreachable no matter how long the
/// fleet ran — a routine deploy was enough to start the window again.
///
/// The rows share the registered `v` family and occupy their own `b"vm"`
/// sub-span, per D32 clause (c)'s allocation rule. The profile match against
/// the family is the argument for the sub-span, exactly as it was for
/// [`cohort_key`]: written by the measurement plane on a slow cadence (one
/// read-modify-write per control per flush interval, never on the intent
/// path), read by measurement tooling and at process startup, and retained
/// for the whole promotion window. `b"vm"` sorts strictly between the `b"vc"`
/// cohort span and the `b"vr"` posture span, so all three are disjoint by
/// construction and no family byte is spent.
#[must_use]
pub fn ramp_window_key(control: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + control.len());
    key.extend_from_slice(b"vm");
    key.extend_from_slice(control.as_bytes());
    key
}

/// Inclusive start of the measurement-window sub-span.
#[must_use]
pub fn ramp_window_range_start() -> Vec<u8> {
    b"vm".to_vec()
}

/// Exclusive end of the measurement-window sub-span.
#[must_use]
pub fn ramp_window_range_end() -> Vec<u8> {
    b"vn".to_vec()
}

/// Key for one D32 open question 2 posture-change history row:
/// `vh/{control}/{versionstamp}`, one append-only row per posture write or
/// clear.
///
/// [`ramp_key`] holds a control's *current* posture and is overwritten by
/// every write, so "who suspended what, when, why" — the incident history
/// open question 2 argues for keeping — survived nowhere. This span is the
/// shadow: one row per change, written in the same transaction that replaces
/// or removes the live row, never updated and never swept by the code that
/// writes it.
///
/// The rows share the registered `v` family and occupy their own `b"vh"`
/// sub-span, per D32 clause (c)'s allocation rule. The profile match is the
/// argument, exactly as it was for [`cohort_key`] and [`ramp_window_key`]:
/// written rarely by the operator plane, read by measurement and review
/// tooling, never on any hot path, retained for the whole promotion window.
/// `b"vh"` sorts strictly between the `b"vc"` cohort span and the `b"vm"`
/// window span, so all four sub-spans are disjoint by construction and no
/// family byte is spent.
///
/// The layout is `b"vh"` ‖ `control` ‖ `0x00` ‖ `versionstamp:[u8;10]`. The
/// `0x00` separator is load-bearing, not decoration: control names are
/// variable-length, and a per-control scan that ended at the name's bytes
/// alone would leak rows from any future control whose name extends another's
/// (`strikes`, `strikes2`). Control names are stable ASCII words, so `0x00`
/// terminates the name unambiguously and the scan's exclusive end is the
/// separator incremented — see [`posture_history_range_end`].
///
/// The versionstamp placeholder carries ten zero bytes until the write
/// substitutes the commit version with `MutationType::SetVersionstampedKey`
/// and the parameter [`posture_history_versionstamped_key`] builds, so a
/// control's history orders by commit and by nothing else — the property
/// [`binding_history_key`] states, and for the same reason: `recorded_at_ms`
/// in the value is the writer's clock, and it is evidence rather than an
/// index.
#[must_use]
pub fn posture_history_key(control: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + control.len() + 10);
    key.extend_from_slice(b"vh");
    key.extend_from_slice(control.as_bytes());
    key.push(0x00);
    key.extend_from_slice(&[0u8; 10]);
    key
}

/// The [`posture_history_key`] in the exact form
/// `MutationType::SetVersionstampedKey` wants it: the key followed by the
/// placeholder offset as a little-endian `u32`.
///
/// The offset is computed rather than a named constant, because the control
/// name is variable-length and a constant would be wrong for every control
/// but one. Built here rather than at the call site for the reason
/// [`binding_history_versionstamped_key`] gives: the offset and the
/// placeholder are two halves of one fact, and a caller that hardcoded the
/// length would keep compiling after the layout moved and would corrupt the
/// key instead of the versionstamp.
#[must_use]
pub fn posture_history_versionstamped_key(control: &str) -> Vec<u8> {
    let key = posture_history_key(control);
    let offset = u32::try_from(key.len() - 10).unwrap_or(u32::MAX);
    let mut param = key;
    param.extend_from_slice(&offset.to_le_bytes());
    param
}

/// First key of one control's contiguous history scan.
#[must_use]
pub fn posture_history_range_start(control: &str) -> Vec<u8> {
    let mut start = Vec::with_capacity(3 + control.len());
    start.extend_from_slice(b"vh");
    start.extend_from_slice(control.as_bytes());
    start.push(0x00);
    start
}

/// Exclusive end of one control's contiguous history scan.
///
/// The scan start's separator byte incremented, not the name's last byte:
/// ending at `name + 1` would leak a future control whose name extends this
/// one's, and the leak would be silent — the extra rows decode fine and only
/// the reviewer reading them would notice history that is not this control's.
#[must_use]
pub fn posture_history_range_end(control: &str) -> Vec<u8> {
    let mut end = posture_history_range_start(control);
    let last = end
        .last_mut()
        .expect("posture_history_range_start is never empty");
    *last = 0x01;
    end
}

/// Inclusive start of the whole posture-history sub-span.
#[must_use]
pub fn posture_history_span_start() -> Vec<u8> {
    b"vh".to_vec()
}

/// Exclusive end of the whole posture-history sub-span.
#[must_use]
pub fn posture_history_span_end() -> Vec<u8> {
    b"vi".to_vec()
}

/// Decode a `vh/…` key back into its `(control, versionstamp)` components.
///
/// The inverse of [`posture_history_key`] once FDB has substituted the
/// versionstamp. Returns `None` for any key that does not begin `vh`, carry a
/// non-empty control name, terminate it with exactly one `0x00`, and hold
/// exactly ten bytes past the separator — the shape the substitution leaves
/// behind, and nothing else.
#[must_use]
pub fn decode_posture_history_key(key: &[u8]) -> Option<(String, [u8; 10])> {
    if key.len() < 3 || &key[..2] != b"vh" {
        return None;
    }
    let separator = key[2..].iter().position(|byte| *byte == 0x00)? + 2;
    if key.len() - separator - 1 != 10 {
        return None;
    }
    let control = std::str::from_utf8(&key[2..separator]).ok()?;
    if control.is_empty() {
        return None;
    }
    let versionstamp: [u8; 10] = key[separator + 1..].try_into().ok()?;
    Some((control.to_owned(), versionstamp))
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

/// Where one intent stands between commit and certainty
/// ([D29](../../../../docs/adr/0029-low-population-path.md) clause 5).
///
/// # Why a field on the existing row and not a second key family
///
/// A `provisional/{intent_id}` family was considered by the record and
/// rejected: it would put the answer to "did this intent happen" in two rows
/// that a crash between them can disagree about, in the one code path whose
/// entire purpose is that there is only ever one answer. The idempotency read
/// at the top of every intent transaction keeps returning exactly one row, and
/// that row now carries both halves of the answer — what the intent did, and
/// whether the cluster has stood behind it yet.
///
/// # The three states are not a progression a reader may assume
///
/// [`Self::Final`] is reached two ways — an attested commit is born final, and
/// a provisional one is promoted by spot replay — and the durable row does not
/// distinguish them, deliberately: a finalized intent is a finalized intent,
/// and a reader that treated "was once provisional" as a taint would be
/// re-inventing the cascade D29 clause 4 exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum IntentFinality {
    /// Committed with the cluster standing behind it: either attested at
    /// admission, or provisional and since finalized by spot replay.
    #[default]
    Final,
    /// Committed on D29's low-population path and **quarantined**: durable,
    /// visible and attributable, and an input to nothing until the cluster
    /// finalizes it.
    Provisional,
    /// Committed provisionally and reversed by a forward-written inverse
    /// (D29 clause 8). The row survives its reversal — that is what lets a
    /// replay be answered — and its GC deadline is restamped from the
    /// annulment rather than from the commit.
    Annulled,
}

impl IntentFinality {
    /// A short stable label for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Final => "final",
            Self::Provisional => "provisional",
            Self::Annulled => "annulled",
        }
    }
}

/// The value stored at [`intent_key`]: the recorded outcome, the GC deadline
/// (docs/08-persistence.md §6 — default 1 h retention, swept by the same
/// checkpoint pass that GCs despawn tombstones), and D29 clause 5's finality
/// state. The deadline is carried on the row, not re-derived, so the sweep is
/// a pure deadline comparison.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntentRow {
    /// The outcome the intent committed (or was rejected) with.
    pub outcome: orrery_protocol::IntentOutcome,
    /// Unix-millisecond deadline after which the checkpoint pass may clear
    /// the row — **and only if [`Self::finality`] permits**, see
    /// [`sweepable`].
    pub gc_deadline_ms: u64,
    /// D29 clause 5's durable finality state.
    pub finality: IntentFinality,
    /// Unix-millisecond deadline by which a [`IntentFinality::Provisional`]
    /// row must be finalized or annulled, `0` on any other finality.
    ///
    /// Carried on the row rather than derived from a commit timestamp for the
    /// same reason `gc_deadline_ms` is: the finalizer's sweep is then a
    /// comparison and not a reconstruction, and raising the deadline for new
    /// commits cannot retroactively extend old ones.
    pub finalize_by_ms: u64,
}

/// D29 clause 9(c)'s GC interlock, in one predicate.
///
/// ```text
/// sweepable(row)  <=>  row.finality in {Final, Annulled}
///                      && now_ms >= row.gc_deadline_ms
/// ```
///
/// # What the finality conjunct buys
///
/// docs/08-persistence.md §6 promises a checkpoint pass that clears these rows
/// after their retention, and names the hazard it creates: "A client's offline
/// intent queue TTL must be shorter than this, or a replay after a long
/// netsplit can double-apply". A *provisional* row makes that hazard sharper —
/// if it could be swept, a replayed provisional intent would find no
/// idempotency row and commit a second time, which is a dupe vector wearing a
/// garbage-collection costume. The interlock removes it by construction: a row
/// is not sweepable while it is unresolved, whatever its deadline says.
///
/// It is written to be the assertion that never fires. `D_finalize` is 5
/// minutes and the retention is 1 hour, a factor of twelve, so a provisional
/// row is always resolved with ~55 minutes of retention left. The interlock is
/// what catches it if that ratio is ever changed carelessly.
///
/// The sweep this predicate is the contract for is still unwritten — D29
/// deliberately specified it before it exists, so whoever writes it inherits
/// the condition rather than discovering the race in production. This function
/// is that inheritance, and it is exported so the sweep cannot be written
/// against a second, differently-worded version of the rule.
#[must_use]
pub const fn sweepable(row: &IntentRow, now_ms: u64) -> bool {
    matches!(
        row.finality,
        IntentFinality::Final | IntentFinality::Annulled
    ) && now_ms >= row.gc_deadline_ms
}

// ---------------------------------------------------------------------------
// Provisional-hold family: `provisional/{account}`
// ---------------------------------------------------------------------------
//
// D29 clauses 4 and 9(b)'s durable index, and the one row three questions are
// answered from:
//
//   1. **Is this ledger row quarantined?** (Clause 4 — a provisional commit is
//      an input to nothing.) An intent that reads a balance row reads this row
//      too and refuses if the balance is held.
//   2. **Is this account over its outstanding cap?** (Clause 9(b).) The entry
//      count is the answer.
//   3. **What must the finalizer look at next?** (Clause 7's sweep, oldest
//      first.) The family is small — one row per account with outstanding
//      provisional work, and empty in the steady state — so the sweep is a
//      short range scan rather than a walk of the whole `intent/` family.
//
// One row rather than three families, and the reason is the reason `IntentRow`
// carries `finality`: an account's outstanding provisional set is one fact, and
// a second copy of it is a second thing that can disagree.
//
// **Why keyed by account and not by ledger key.** Clause 3 admits on the
// provisional path only ops "whose credit and debit are both inside the
// submitting account's rows", so every key a provisional intent can write
// belongs to one account — the submitter's. A held key is therefore always
// reachable from the account that owns it, and the account is what both the cap
// and the sweep are already scoped by. A key-addressed hold family would buy
// nothing and cost a read per named key.

/// Key for an account's outstanding provisional set: `provisional/{account}`.
///
/// `'r'` then the 8-byte [`AccountId`] big-endian, so accounts sort by id and
/// the family is one contiguous range for the finalizer's sweep. The prefix is
/// `'r'` for want of a mnemonic — `'p'` is the seed-progress family and `'i'`
/// is the intent family this one indexes into — and `'r'` was unclaimed.
#[must_use]
pub fn provisional_key(account: AccountId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'r';
    key[1..9].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// The first byte of the `provisional/` family span.
#[must_use]
pub fn provisional_range_start() -> Vec<u8> {
    vec![b'r']
}

/// The exclusive end of the `provisional/` family span.
///
/// `b's'` is the `seedmap/` prefix, which is the correct exclusive bound here
/// for the reason [`epoch_range_end`] gives: the families are adjacent and
/// disjoint, so one past the end of `'r'` is the first key of the next family.
#[must_use]
pub fn provisional_range_end() -> Vec<u8> {
    vec![b's']
}

/// One durable write a provisional intent applied, kept so annulment can write
/// its exact inverse.
///
/// # Why the *applied* writes and not the intent's ops
///
/// D29 clause 8 requires annulment to apply "the exact inverse of the original
/// ops", and an op is not its own inverse: `LEDGER_CREDIT_OP` carries a delta,
/// but a `Ruleset`-opaque op carries bytes this cluster never interpreted and
/// therefore never applied. Recording what was *written* rather than what was
/// *asked for* makes the inverse a mechanical negation with nothing to
/// re-interpret — and it means an annulment cannot drift from the commit if
/// the op semantics change under it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProvisionalWrite {
    /// The account whose balance row moved.
    pub account: AccountId,
    /// The asset the balance is denominated in.
    pub asset: AssetId,
    /// The delta applied. Annulment applies `-delta`.
    pub delta: i64,
}

/// One unfinalized provisional intent, as its submitting account's
/// `provisional/{account}` row records it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProvisionalHold {
    /// The intent whose `intent/{intent_id}` row is `Provisional`.
    pub intent_id: u128,
    /// The account this hold is filed under — the row's own key, carried in
    /// the value so a hold read out of a range scan knows where to be written
    /// back. Not derivable from [`Self::writes`]: an intent of nothing but
    /// `Ruleset`-opaque ops writes no balance and still holds a slot against
    /// the cap.
    pub account: AccountId,
    /// The balance rows this intent wrote, and which are therefore an input
    /// to nothing until it is finalized (D29 clause 4).
    pub writes: Vec<ProvisionalWrite>,
    /// Unix milliseconds at which the provisional commit landed. The sweep
    /// orders by this — oldest first (D29 clause 7).
    pub committed_ms: u64,
    /// Unix-millisecond finalization deadline, mirrored from the intent row
    /// so the sweep needs one read rather than two.
    pub finalize_by_ms: u64,
    /// D29 clause 6's commitment, mirrored here for the same reason: the
    /// finalizer fetches a bundle against it and never needs the intent row.
    pub commitment: orrery_protocol::EvidenceCommitment,
    /// The issuer the verdict is attributed to, and whose key the fetched
    /// bundle's signatures must verify under.
    pub subject: orrery_protocol::NodeId,
}

/// The value stored at [`provisional_key`].
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ProvisionalRow {
    /// The account's unfinalized provisional intents, oldest first.
    ///
    /// Bounded at [`orrery_protocol::PROVISIONAL_OUTSTANDING_CAP`] by
    /// admission, which is what keeps this row small enough to read on the
    /// intent path.
    pub holds: Vec<ProvisionalHold>,
}

impl ProvisionalRow {
    /// Whether `(account, asset)` is written by an unfinalized provisional
    /// intent — D29 clause 4's predicate, and the reason this row is read on
    /// the intent path at all.
    #[must_use]
    pub fn holds_balance(&self, account: AccountId, asset: AssetId) -> Option<u128> {
        self.holds
            .iter()
            .find(|hold| {
                hold.writes
                    .iter()
                    .any(|write| write.account == account && write.asset == asset)
            })
            .map(|hold| hold.intent_id)
    }
}

// ---------------------------------------------------------------------------
// Witness-epoch families: `epoch/{grid}/{cell}/{epoch}` and
// `epoch-handle/{handle}`
// ---------------------------------------------------------------------------
//
// The durable half of D28 clause (f) and D27 clause (d). Two families over one
// fact, and the reason is the reason `lease_cell_key` and `lease_location_key`
// both exist: two read patterns, neither of which should scan for the other.
// An auditor and the adjudication executor scan by cell — a cell's epochs sort
// in order and a grid's subtree is one contiguous range — while the intent
// path resolves by the globally unique handle an `Intent::cell_epoch` names.
//
// docs/08-persistence.md writes this family as `epoch/{cell_id}` with no grid.
// That is a D22 violation the sweep missed: cell ids are grid-relative, so two
// grids' identically numbered cells would share a row, and a witness set
// silently shared between nested grids is a witness set chosen by neither
// cell's population. D28 clause (f) fixes it, and this is that fix.

/// Key for the witness-epoch record: `epoch/{grid}/{cell}/{epoch}`.
///
/// `'e'` then the 4-byte `GridId`, the 8-byte `CellId` and the 4-byte epoch
/// counter, all big-endian so a cell's epochs sort in announcement order.
#[must_use]
pub fn epoch_key(grid: GridId, cell: CellId, epoch: u32) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[0] = b'e';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..13].copy_from_slice(&cell.to_bits().to_be_bytes());
    key[13..].copy_from_slice(&epoch.to_be_bytes());
    key
}

/// The first byte of the `epoch/` family span.
#[must_use]
pub fn epoch_range_start() -> Vec<u8> {
    vec![b'e']
}

/// The exclusive end of the `epoch/` family span (one byte past `e`).
///
/// `b'f'` is also the `epoch-handle/` prefix, which is the correct exclusive
/// bound here for exactly the reason it looks alarming: the two families are
/// adjacent and disjoint, so "one past the end of `e`" is the first key of the
/// next family and no `epoch/` row can sort at or beyond it.
#[must_use]
pub fn epoch_range_end() -> Vec<u8> {
    vec![b'f']
}

/// Key for the handle index: `epoch-handle/{handle}` -> the [`epoch_key`] of
/// the row it names.
///
/// The handle is D28 clause (b)'s `(incarnation << 48) | counter`, big-endian.
/// This is the only lookup on the intent path, and it exists because an
/// `Intent` carries a handle and nothing else — no grid, no cell, no counter —
/// so resolving one by scanning the cell family would mean scanning the whole
/// family.
#[must_use]
pub fn epoch_handle_key(handle: u64) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'f';
    key[1..].copy_from_slice(&handle.to_be_bytes());
    key
}

/// The value stored at [`epoch_key`]: the announcement verbatim, plus the draw
/// state this gateway minted for the cell-epoch.
///
/// **The envelope is stored undecomposed, and that is the whole security value
/// of the row** (D28 clause (f)): a reader recomputes the coordinator
/// signature from these bytes and needs to trust neither the gateway that
/// wrote them nor FoundationDB. A decomposed row would be the gateway's
/// *assertion* about an announcement, which is the trust inversion the
/// courier model exists to refuse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EpochRow {
    /// The coordinator-signed `WitnessEpochV1` envelope, verbatim.
    pub announcement: Vec<u8>,
    /// When the accepting gateway first saw it, on its own clock.
    pub first_seen_ms: u64,
    /// `blake3(DOMAIN ‖ grid ‖ cell ‖ epoch ‖ draw_key)`.
    ///
    /// The commitment D27 clause (d) requires to be durable before any intent
    /// in the cell-epoch is admitted. Without it a gateway could choose the
    /// draw key after seeing which attestations arrived, and every
    /// retrospective audit of the draw would be theatre.
    pub draw_commit: [u8; 32],
    /// The cell-epoch's draw key itself.
    ///
    /// Stored rather than held only in memory, and D27 clause (d) is explicit
    /// about why: it is what makes the scheme survive a D26 sibling handover.
    /// A sibling that adopts the shard mid-epoch **reads** this key instead of
    /// minting a new one, so a handover does not silently re-roll every
    /// outstanding required subset and invalidate every co-signature already
    /// collected.
    ///
    /// "Secret" here means *not exported*, not *not stored*: no peer holds a
    /// FoundationDB handle, so the cluster's trust boundary is the disclosure
    /// boundary. This row is also the surface a cluster-side auditor reads the
    /// key from after epoch end, which is D27's reveal. A *peer*-side audit of
    /// the draw is deferred — no peer-visible message carries the commitment,
    /// and D27 leaves naming one as an open question.
    pub draw_key: [u8; 32],
    /// The coordinator's seed key for this epoch, once the next announcement's
    /// `prev_seed_key` has opened its commitment (D28 clause (c)).
    ///
    /// `None` until then. This is `k_epoch`, not [`Self::draw_key`]: it checks
    /// the coordinator's *selection* shuffle, not this gateway's per-intent
    /// draw, and the two are different secrets held by different processes.
    pub revealed_key: Option<[u8; 32]>,
    /// Unix-millisecond deadline after which the checkpoint pass may clear the
    /// row. Carried rather than re-derived, the shape [`IntentRow`] uses, so
    /// the sweep is a pure deadline comparison.
    pub gc_deadline_ms: u64,
}

// ---------------------------------------------------------------------------
// Attested-intent family: `attest/{intent_id}`
// ---------------------------------------------------------------------------
//
// D27 clause (f) item 5, the one an implementer is most likely to skip and
// which makes the whole retrospective audit vacuous if omitted: **the eligible
// vector the gateway actually derived over**, recorded alongside the committed
// intent.
//
// `E(I)` is the announced set minus the parties, and party exclusion matches
// on accounts and every NodeId bound to them — bindings that live in
// `orrery_identity` and change over time. An auditor recomputing `E(I)` a week
// later from *current* bindings can silently derive a different eligible list,
// therefore a different `required(I)`, and conclude the gateway cheated when
// it did not. So the gateway records what it drew over, and the audit reads
// the record rather than reconstructing history.
//
// What that buys, exactly, is worth being precise about because a later reader
// will otherwise over-trust it: the audit proves "given the eligibility list
// you recorded, did you draw the required subset correctly", not "was that
// eligibility list honest". A gateway that lied about `E(I)` would pass. That
// is acceptable only because the gateway is already the sole writer of durable
// truth (D11) — its compromise ends the game by other means — and D27 accepts
// the bound explicitly rather than leaving it in a reviewer's memory.

/// Key for the recorded eligible vector: `attest/{intent_id}`, big-endian.
///
/// The prefix is `b'g'` for want of a mnemonic: `'a'` is the actor/fence
/// family, `'i'` is the intent idempotency row, and `'e'`/`'f'` are this
/// record's own epoch families. `'g'` is unclaimed and adjacent, which is all
/// a one-byte discriminator has to be.
#[must_use]
pub fn attest_key(intent_id: u128) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[0] = b'g';
    key[1..].copy_from_slice(&intent_id.to_be_bytes());
    key
}

/// The first byte of the `attest/` family span.
#[must_use]
pub fn attest_range_start() -> Vec<u8> {
    vec![b'g']
}

/// The exclusive end of the `attest/` family span (one byte past `g`).
#[must_use]
pub fn attest_range_end() -> Vec<u8> {
    vec![b'h']
}

/// The value stored at [`attest_key`]: what this intent's required subset was
/// drawn over.
///
/// Written in the same transaction as the intent's effects and its
/// idempotency row, so a committed intent and the record of how it was judged
/// are one atomic fact. An audit reads both.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttestRow {
    /// The cell-epoch handle the intent named, so an auditor can find the
    /// announcement and the draw key without re-deriving either.
    pub epoch_handle: u64,
    /// `E(I)`: the eligible vector, **in announced order**.
    ///
    /// Order is load-bearing. The draw is a keyed hash per member and the
    /// smallest K win, so order does not change the *result* — but the audit
    /// recomputes over this vector, and a normalized (sorted, deduplicated)
    /// copy would no longer be the object the gateway drew over.
    pub eligible: Vec<orrery_protocol::NodeId>,
    /// Unix-millisecond deadline after which the checkpoint pass may clear the
    /// row.
    ///
    /// **The audit window is therefore this deadline, and it is the same one
    /// [`IntentRow`] carries.** D27 asks for the record to be kept "for as
    /// long as the intent is auditable", and this makes those two the same
    /// span by construction rather than by coincidence — a longer retention
    /// here would preserve a proof about an intent whose own row is gone.
    pub gc_deadline_ms: u64,
    /// Whether the quorum this row records was **enforced** at admission, or
    /// merely observed.
    ///
    /// [D32](../../adr/0032-enforcement-ramp.md) clause (d). A shadow-period
    /// commit writes its row like any other, with this field `false`;
    /// `required` writes `true`; `off` writes no row at all. The alternatives
    /// both fail, and the record says why: omitting the row leaves
    /// shadow-period attested commits unauditable against D27 clause (f), and
    /// writing it *unmarked* fabricates an audit trail claiming the cluster
    /// stood behind a quorum it deliberately waived. With the marker an
    /// auditor reads a coherent story — insufficient co-signatures, admitted
    /// by policy, observed and not trusted.
    ///
    /// **This is a durable value-shape change.** `postcard` encodes fields
    /// positionally and `postcard::from_bytes` refuses trailing bytes, so a
    /// reader of the older three-field shape does **not** decode-and-drop this
    /// field — it fails outright, which is where D32's Consequences overstate
    /// the compatibility. What makes that affordable rather than a migration
    /// is the retention: an `attest/` row is swept an hour after its intent
    /// commits (the `INTENT_ROW_RETENTION_MS` the executor stamps
    /// [`Self::gc_deadline_ms`] with), so the two shapes can only coexist for
    /// as long as one deployment's rollout takes.
    pub enforced: bool,
}

// ---------------------------------------------------------------------------
// Player family: `player/{account_id}` (+ `/loc` placement pointer)
// ---------------------------------------------------------------------------
//
// Critical-class rows written by the intent path (docs/08-persistence.md §6):
// the profile row and the login placement pointer. The account id is the
// 8-byte big-endian `AccountId`.

/// Key for the account profile row: `player/{account_id}` (§6). Critical-class;
/// written inside intent transactions only.
///
/// **Unversioned, and the reason is that nothing writes it yet** (D38 clause
/// (d)(1)). Grepping the workspace finds this constructor called from no
/// writer and no reader outside this module's own guards: the profile row is
/// designed and keyed but not yet produced. There are therefore zero rows at
/// rest to bootstrap, and the version arrives with the first writer — which is
/// strictly better than picking a value shape now and versioning a row nobody
/// has written.
#[must_use]
pub fn player_key(account: AccountId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'u';
    key[1..9].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// Key for the login placement pointer: `player/{account_id}/loc` →
/// `(cell_id, entity_id)` (§6). Written by the cell actor on rekey, read at
/// login. The trailing 0x01 keeps it inside the account's span without
/// colliding with the profile row.
#[must_use]
pub fn player_loc_key(account: AccountId) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'u';
    key[1..9].copy_from_slice(&account.0.to_be_bytes());
    key[9] = 0x01; // "loc" suffix inside the account span
    key
}

// ---------------------------------------------------------------------------
// Identity family: `id/{…}` — `da` accounts, `db` bindings, `dc` cooldown
// entries, `dh` history, `dn` bans, `dw` binding-rate window
// ---------------------------------------------------------------------------
//
// D31's account subspace. One family byte, `b'd'`, spanning `[b"d", b"e")`,
// with four sub-spans discriminated by an ASCII byte at a fixed offset:
//
//   da ‖ account:u64 BE                       10 B  ->  AccountRow
//   db ‖ node:[u8;32]                         34 B  ->  BindingRow
//   dh ‖ node:[u8;32] ‖ versionstamp:[u8;10]  44 B  ->  BindingHistoryRow
//   dn ‖ account:u64 BE                       10 B  ->  banned_at_ms:u64 BE (D33)
//   dw ‖ account:u64 BE                       10 B  ->  postcard Vec<u64>,
//                                                   ascending event stamps (D36)
//
// `orrery_identity` (D12) is the sole writer of every row here. The gateway is
// the only durable reader, and it reads FoundationDB directly rather than
// calling identity on the intent path, because an identity outage must never
// be a play outage (docs/09-services-and-ops.md §8's grace rule). The
// coordinator reads nothing: every candidate it seeds a witness set from
// already holds a token-verified session with it, so its account is in hand
// without a lookup (D31 (d)).
//
// **Why one byte carries four families.** Seventeen of the twenty-six
// lowercase bytes were already taken as family prefixes before this record, and
// six more are spoken for as exclusive range ends, leaving `d`, `y` and `z`
// against four documented-but-unbuilt families. Taking one byte each runs out.
// So `id/` spends one and discriminates inside it, which is the pattern the
// ledger already established with `lb`/`li`/`lr`; sub-span ordering
// `a < b < h < w` makes the scans disjoint by construction, exactly as
// `lb < le < li < lr` does. All four are written by one service — the window
// rides the same transaction as the rows it guards (D36 (b)) — in one
// transaction, under one retention rule, so there is nothing a second byte
// would separate that is not already together.
//
// The discriminator is an ASCII byte at a **fixed offset**, never the high byte
// of an id. [`lease_key`] observes the same discipline now — D35 moved the
// registrar row into an `le` sub-span beside `lb`/`li`/`lr`, which is what
// this family's drafting pass asked for. Until then it put a `GridId`'s most
// significant byte where the ledger puts `b`/`i`/`r`, so a
// `grid.0 >= 0x6200_0000` would have landed a lease row inside `ledger/bal/`;
// latent only because grid ids were small, and fixed as its own on-disk-format
// record rather than here.
//
// **Why the reverse index is inside the family and not beside it.** The only
// direction any consumer reads is node -> account: a gateway deriving `E(I)`
// asks `owner(n)` for the <= 7 announced NodeIds, and never asks which nodes an
// account holds. Answering that from `da` alone is an O(accounts) range read —
// at 10^7 accounts, a full-subspace scan on a path D16 budgets at 10 ms. `db`
// makes it a point read. A *second family* would cost the same point read,
// spend one of the three remaining clean bytes, and — the load-bearing half —
// separate two rows that must be written in one transaction. A reverse index
// maintained in a second transaction has a window in which `db` names an
// account `da` no longer binds, and under D31 (f) that is a *wrong* answer
// rather than a miss: a miss excludes, a wrong answer admits (D31 (b)).
//
// **Why the history is keyed by node rather than by account.** Same reason.
// The audit's question is `owner_t(n)` for the <= 7 announced NodeIds, so a
// node-keyed log answers it with <= 7 bounded, contiguous range reads. The
// per-account question — which devices has this account ever held — is a
// support and abuse-investigation query that no hot path makes, and it is
// served offline by scanning the same rows rather than by a second log that
// would be a second thing to keep consistent (D31 (b)).
//
// **Why no `GridId`, when D22 says a grid id stays a key discriminator.** D22
// scopes *cell-id spaces*: a `CellId` is grid-relative, so two grids'
// identically numbered cells must not share a row. An `AccountId` is not a cell
// id and has no grid — an account is one cluster-global identity that plays
// wherever it likes, and a per-grid account record would be several accounts
// wearing one id. Every other account-keyed family in this module is grid-free
// for the same reason: `player/{account_id}`, `ledger/bal/{account}/{asset}`
// and `provisional/{account}`. D22 is satisfied by there being no cell id here
// to be ambiguous.
//
// **What this subspace does not do.** It does not replace [`AttestRow`] as the
// audit's source for `E(I)` (D31 (h)): the recorded eligible vector answers
// "given that list, did you draw correctly", and `dh` answers the separate
// question "was that list consistent with the bindings that existed at the
// time" — a cross-check, and one whose disagreements mean nothing inside the
// reader's cache staleness bound.
//
// **The writer is `orrery_identity` (issue #210), and it is now in the tree.**
// D31 decided bytes, directions and semantics and deferred the writer; the
// crate implements it, and every mutation there stages `da`, `db` and `dh` in
// one transaction as clause (b) requires. What has *not* changed is the
// deployment posture: nothing in this cluster runs that service yet, so the
// subspace a live gateway reads is still empty, every binding lookup still
// misses, and D31 (f)'s fail-closed rule still excludes every announced NodeId
// a gateway is not directly connected to — which remains the reason the
// enforcement switch defaulting off matters.

/// Maximum NodeIds bound to one account (D31 (g), proposed as a D16 row).
///
/// This is what bounds [`AccountRow`]: eight inline NodeIds put the row at
/// ~282 B, ~2.8 GB across 10^7 accounts. Enforced at identity, on the write
/// path; stated here because it is the reason the row is safe to read whole.
pub const MAX_BOUND_NODES_PER_ACCOUNT: usize = 8;

/// How long a `dh` row is retained before hard deletion: 90 days (D31 (g) and
/// its resolved question 2, proposed as a D16 row).
///
/// The horizon is set by the strike and appeal window, not by the audit
/// cross-check — [`AttestRow`] is swept an hour after its intent commits, so no
/// attestation-side artifact survives even a day. D16's 14-day strike half-life
/// is what justifies 90: a strike retains 2^(-90/14) ~= 1.2 % of its weight by
/// then, so the history outlives every dispute that could cite it.
///
/// Expiry is a **pure range delete** with no read-modify-write, and that is
/// what [`AccountRow::binding_event_count`] and [`AccountRow::first_event_ms`]
/// buy: the lifetime-churn signal a dispute actually asks for is folded into
/// the account row at write time and survives the deletion of the rows it was
/// folded from, at ~12 B per account.
pub const BINDING_HISTORY_RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// Key for an account record: `id/da/{account_id}`.
///
/// `'d'`, then the sub-space discriminator `'a'`, then the 8-byte [`AccountId`]
/// big-endian so accounts sort by id and the sub-space is one contiguous range.
///
/// **Not to be confused with [`player_key`]**, which is also account-keyed.
/// That row is the *game* profile written by the intent path; this one is the
/// *identity* record written by identity alone. Different families, different
/// writers, different retention; the only thing they share is the id in the
/// key.
#[must_use]
pub fn account_key(account: AccountId) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'd';
    key[1] = b'a';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// Key for the reverse binding index: `id/db/{node_id}` -> [`BindingRow`].
///
/// The 32 raw bytes of the NodeId, which is an ed25519 public key
/// (`orrery_protocol::identity`), so the key *is* the identity and no hashing
/// or truncation stands between a lookup and its row. Written in the same
/// FoundationDB transaction as the [`account_key`] row it derives from
/// (D31 (b)), so the two are never observed disagreeing.
#[must_use]
pub fn binding_key(node: &NodeId) -> [u8; 34] {
    let mut key = [0u8; 34];
    key[0] = b'd';
    key[1] = b'b';
    key[2..34].copy_from_slice(node.as_bytes());
    key
}

/// Key for one append-only binding event:
/// `id/dh/{node_id}/{versionstamp}` -> [`BindingHistoryRow`].
///
/// The returned key carries 10 zero bytes at the versionstamp position (byte
/// offset [`BINDING_HISTORY_VERSIONSTAMP_OFFSET`]); write it with
/// `MutationType::SetVersionstampedKey` and the parameter
/// [`binding_history_versionstamped_key`] builds, so FDB substitutes the commit
/// versionstamp. Ordering within a node's span is therefore commit order and no
/// clock is involved — the property [`ledger_receipt_key`] relies on, and for
/// the same reason: `at_ms` in the value is the writer's clock, and it is
/// evidence rather than an index.
///
/// **One per transaction.** Every versionstamped write in a transaction gets
/// the same 10 bytes, so two events for one node in one transaction would be
/// one key written twice. A bind and an unbind are separate credentialed
/// actions and therefore separate transactions, so this bounds batching and
/// not the model.
#[must_use]
pub fn binding_history_key(node: &NodeId) -> [u8; 44] {
    let mut key = [0u8; 44];
    key[0] = b'd';
    key[1] = b'h';
    key[2..34].copy_from_slice(node.as_bytes());
    // bytes 34..44: the zero placeholder the versionstamp is written into.
    key
}

/// Byte offset of the versionstamp placeholder inside [`binding_history_key`].
///
/// Immediately after the two discriminator bytes and the 32-byte node id, so a
/// node's events sort by commit version and by nothing else.
pub const BINDING_HISTORY_VERSIONSTAMP_OFFSET: u32 = 34;

/// The [`binding_history_key`] in the exact form
/// `MutationType::SetVersionstampedKey` wants it: the 44-byte key followed by
/// the placeholder offset as a little-endian `u32`.
///
/// Built here rather than at the call site for the reason
/// [`ledger_receipt_versionstamped_key`] gives: the offset and the placeholder
/// are two halves of one fact, and a caller that hardcoded `34` would keep
/// compiling after the placeholder moved and would corrupt the node id instead
/// of the versionstamp.
#[must_use]
pub fn binding_history_versionstamped_key(node: &NodeId) -> [u8; 48] {
    let mut param = [0u8; 48];
    param[..44].copy_from_slice(&binding_history_key(node));
    param[44..48].copy_from_slice(&BINDING_HISTORY_VERSIONSTAMP_OFFSET.to_le_bytes());
    param
}

/// Decode an `id/dh/…` key back into its `(node, versionstamp)` components.
///
/// The inverse of [`binding_history_key`] once FDB has substituted the
/// versionstamp. Returns `None` for any key that is not exactly 44 bytes
/// beginning `d`, `h`, and `None` when the 32 node bytes are not a valid
/// ed25519 public key — a NodeId is a curve point, so not every 32-byte string
/// is one.
///
/// This is what serves the per-account query D31 (b) sends offline: scan the
/// `dh` sub-space, take the account from the value and the device from the key.
#[must_use]
pub fn decode_binding_history_key(key: &[u8]) -> Option<(NodeId, [u8; 10])> {
    if key.len() != 44 || key[0] != b'd' || key[1] != b'h' {
        return None;
    }
    let node = NodeId::from_bytes(key[2..34].try_into().ok()?).ok()?;
    let versionstamp: [u8; 10] = key[34..44].try_into().ok()?;
    Some((node, versionstamp))
}

/// Key for one account's binding-rate window: `id/dw/{account_id}`.
///
/// The value is a postcard `Vec<u64>` of ascending event timestamps in ms —
/// every binding event the account filed within its trailing 30 days, both
/// directions counted — which is what makes D31 clause (g)'s rate cap
/// answerable with one point read instead of a scan of every node's `dh`
/// span filtered on values (D36 §Decision (a), `docs/adr/0036-binding-rate-window.md`).
/// Identity is this row's only
/// writer, and it writes it **inside** the transaction that stages `da`, `db`
/// and `dh`: the window check that refuses the 9th event in 24 h or the 65th
/// in 30 days reads and writes this row non-snapshot there, so a refusal
/// stages nothing and an abort leaves the window exactly as it was.
///
/// `'w'` keeps the hand-written bound away from `dh`'s end for the same
/// reason D35 refused an `'s'` beside `lr`: boundaries that mean two things
/// are how sub-span defects breed.
#[must_use]
pub fn binding_window_key(account: AccountId) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'd';
    key[1] = b'w';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// The first byte of the whole `id/` family span.
#[must_use]
pub fn id_range_start() -> Vec<u8> {
    vec![b'd']
}

/// The exclusive end of the whole `id/` family span (one byte past `d`).
///
/// `b'e'` is also the `epoch/` prefix, which is the correct exclusive bound
/// here for the reason [`epoch_range_end`] and [`provisional_range_end`] both
/// give: the families are adjacent and disjoint, so one past the end of `'d'`
/// is the first key of the next family. An exclusive bound of `[b'e']` cannot
/// include any key `e ‖ …`, because `[0x65] < [0x65, …]`.
#[must_use]
pub fn id_range_end() -> Vec<u8> {
    vec![b'e']
}

/// The first key of the `id/da/…` account sub-space.
#[must_use]
pub fn account_range_start() -> Vec<u8> {
    vec![b'd', b'a']
}

/// The exclusive end of the `id/da/…` account sub-space.
///
/// `b"db"` is the binding sub-space's first key, adjacent and disjoint — the
/// same construction the family bounds use one level up.
#[must_use]
pub fn account_range_end() -> Vec<u8> {
    vec![b'd', b'b']
}

/// The first key of the `id/db/…` binding sub-space.
#[must_use]
pub fn binding_range_start() -> Vec<u8> {
    vec![b'd', b'b']
}

/// The exclusive end of the `id/db/…` binding sub-space.
///
/// `b"dc"` is [`cooldown_range_start`], the first key of D33's cooldown-entry
/// sub-space: adjacent and disjoint, the same construction every other bound in
/// this family uses. It was the gap the discriminators `a < b < h` deliberately
/// left until D33 claimed it, and the bound is correct either way — one past
/// every binding key and short of every cooldown key.
#[must_use]
pub fn binding_range_end() -> Vec<u8> {
    vec![b'd', b'c']
}

/// Key for D33's cooldown entry: `id/dc/{account_id}` -> `entered_at_ms:u64-be`.
///
/// `'d'`, then the sub-space discriminator `'c'`, then the 8-byte [`AccountId`]
/// big-endian — the same shape as [`account_key`], so cooldown entries sort by
/// account and the sub-space is one contiguous range. The value is a bare
/// eight-byte big-endian instant with no postcard framing, which is what lets a
/// reader decode a row without linking the writer's types.
///
/// # Why an identity-owned row's key lives here
///
/// `orrery_identity` is this family's **sole writer** (D31 clause (b)) and this
/// module does not change that: nothing in `orrery_persistd` writes a `dc` row.
/// What lives here is the *bytes*, beside the five sibling bounds of the same
/// family — `account_key`/[`account_range_start`] (`da`),
/// [`binding_range_start`] (`db`), [`binding_history_range_start`] (`dh`),
/// [`binding_window_range_start`] (`dw`). This builder used to be private to
/// `orrery_identity::fdb`, the one `d`-family key builder in the tree outside
/// this module, which was safe only while identity was also the family's sole
/// *reader*. It is not: `orrery_persistd::standing_feed::DcCooldownFeed` reads
/// it, and `orrery_identity` cannot be named from a `persistd` binary
/// (`docs/spikes/862-gateway-consumer-dependency-cycle.md` carries the cargo
/// error). Two copies of these ten bytes in two crates is the outcome
/// `crates/orrery_identity/Cargo.toml:29` records as "the one thing D31 clause
/// (b) cannot survive", so there is one definition and it is this one.
#[must_use]
pub fn cooldown_entry_key(account: AccountId) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'd';
    key[1] = b'c';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// The first key of the `id/dc/…` cooldown-entry sub-space (D33).
///
/// Identical to [`binding_range_end`] by construction, because the sub-spaces
/// are adjacent: one family's exclusive end *is* the next family's inclusive
/// start. Call the one that names what you are scanning.
#[must_use]
pub fn cooldown_range_start() -> Vec<u8> {
    vec![b'd', b'c']
}

/// The exclusive end of the `id/dc/…` cooldown-entry sub-space.
///
/// `b"dd"` is not a key anything writes; it is the successor of the two-byte
/// `dc` prefix, so `[dc, dd)` contains every `dc ‖ account` row and nothing
/// else. Deriving the bound from the prefix rather than from `account =
/// u64::MAX` keeps it correct if the row ever grows a suffix.
#[must_use]
pub fn cooldown_range_end() -> Vec<u8> {
    vec![b'd', b'd']
}

/// Key for D33's terminal ban row: `id/dn/{account_id}` -> `banned_at_ms:u64-be`.
///
/// `'d'`, then the sub-space discriminator `'n'`, then the 8-byte [`AccountId`]
/// big-endian — deliberately the same ten bytes and the same bare eight-byte
/// big-endian value as [`cooldown_entry_key`], because it is the same kind of
/// fact recorded the same way, and a reader can decode either without linking
/// the writer's types.
///
/// # Why a row at all
///
/// D33 clause (e) ends "ban never reverses by decay". `StandingLevel::Banned`
/// is a *band the live score is currently in*, and the score decays: at the
/// clause (d) defaults an account banned at 9.0 re-enters the cooldown band
/// after 5.08 days and would release on the ordinary dwell. Without a durable
/// row there is nothing for the mint path to consult that outlives the
/// arithmetic, so the strongest sanction expires on a timer (#1059). This row
/// is that fact: written the first time identity observes `S ≥ B`, never
/// rewritten, and cleared only by an explicit administrative lift.
///
/// # Why `'n'` and not `'b'`
///
/// `'b'` is the binding sub-space and `'c'` is the cooldown entry, so the
/// discriminator is `'n'` — "no return" — which keeps the family's
/// `a < b < c < h < n < w` ordering disjoint by construction, exactly as the
/// four original sub-spans and the ledger's `lb < le < li < lr` do.
#[must_use]
pub fn ban_entry_key(account: AccountId) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'd';
    key[1] = b'n';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// The first key of the `id/dn/…` ban sub-space (D33 clause (e)).
#[must_use]
pub fn ban_range_start() -> Vec<u8> {
    vec![b'd', b'n']
}

/// The exclusive end of the `id/dn/…` ban sub-space.
///
/// `b"do"` is one past `'n'` — house style for a sub-span end — and still
/// inside the `d` family, so a sweep of the ban rows stops short of `dw` and
/// can never reach another family.
#[must_use]
pub fn ban_range_end() -> Vec<u8> {
    vec![b'd', b'o']
}

/// The first key of the `id/dh/…` binding-history sub-space.
#[must_use]
pub fn binding_history_range_start() -> Vec<u8> {
    vec![b'd', b'h']
}

/// The exclusive end of the `id/dh/…` binding-history sub-space.
///
/// `b"di"` is one past every history key and still inside the `d` family, so a
/// sweep of the history never reaches another family's rows even though the
/// family's own end bound would also be correct arithmetic.
#[must_use]
pub fn binding_history_range_end() -> Vec<u8> {
    vec![b'd', b'i']
}

/// The first key of the `id/dw/…` binding-rate-window sub-space (D36).
#[must_use]
pub fn binding_window_range_start() -> Vec<u8> {
    vec![b'd', b'w']
}

/// The exclusive end of the `id/dw/…` binding-rate-window sub-space.
///
/// `b"dx"` is one past `'w'` — house style for a sub-span end — and still
/// inside the `d` family. The whole window span `[dw, dx)` sorts above every
/// `dh` key (`dh`'s own span ends at `[b"di"]`) and below the family end
/// `[b"e")`, so neither the `dh` retention sweep nor any full-family scan can
/// reach it by accident.
#[must_use]
pub fn binding_window_range_end() -> Vec<u8> {
    vec![b'd', b'x']
}

/// The first key of one node's `id/dh/{node_id}/…` span.
///
/// The 34-byte prefix with no versionstamp, which sorts at or before every
/// event for that node. This and [`binding_history_node_range_end`] are the
/// bounded, contiguous read D31 (b) keys the history by node to get.
#[must_use]
pub fn binding_history_node_range_start(node: &NodeId) -> Vec<u8> {
    let mut key = Vec::with_capacity(34);
    key.extend_from_slice(b"dh");
    key.extend_from_slice(node.as_bytes());
    key
}

/// The 32 node bytes incremented as one big-endian integer, or `None` when
/// they are all `0xFF` and there is no successor.
///
/// Split out from [`binding_history_node_range_end`] so the top of the node
/// space is testable at all. `0xFF…FF` *is* a well-formed [`NodeId`] encoding —
/// it decompresses, checked — but reaching it means finding a secret key whose
/// public key is that exact point, so no test can construct the case through
/// the public API and no fallback branch would ever be exercised. Here it is
/// one call.
fn next_node_bytes(node: &[u8; 32]) -> Option<[u8; 32]> {
    let mut next = *node;
    for byte in next.iter_mut().rev() {
        let (incremented, carry) = byte.overflowing_add(1);
        *byte = incremented;
        if !carry {
            return Some(next);
        }
    }
    None
}

/// The exclusive end of one node's `id/dh/{node_id}/…` span.
///
/// The first key of the *next* node's span — the 32 node bytes incremented as a
/// big-endian integer — which sorts after every 44-byte key sharing this node's
/// prefix and before every key of any later node. At the top of the node space
/// the bound is the end of the whole sub-space, exactly as [`world_range_end`]
/// falls back to the next family at the top of its own.
#[must_use]
pub fn binding_history_node_range_end(node: &NodeId) -> Vec<u8> {
    let Some(next) = next_node_bytes(node.as_bytes()) else {
        return binding_history_range_end();
    };
    let mut key = Vec::with_capacity(34);
    key.extend_from_slice(b"dh");
    key.extend_from_slice(&next);
    key
}

/// The value stored at [`account_key`]: D31's account record.
///
/// Postcard-encoded, like [`IntentRow`] and [`ItemRow`], and for the same
/// reason: one small, versionless value written and read inside this cluster.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AccountRow {
    /// The NodeIds currently bound to this account, at most
    /// [`MAX_BOUND_NODES_PER_ACCOUNT`] of them.
    ///
    /// Inline rather than a sub-family: the cap bounds the row, and the forward
    /// direction is read whole or not at all. The *reverse* direction is the
    /// one that needs an index, and it has one — [`binding_key`].
    pub bound_nodes: Vec<NodeId>,
    /// Bumped on every binding change, and the half of a reader's cache
    /// validity that is not a clock (D31 (e)).
    ///
    /// A gateway's binding cache entry is valid while it is younger than
    /// `T_stale` **and** this counter has not moved since the fill; identity
    /// pushes the new value on a change. Losing the push channel degrades the
    /// cache to TTL-only, which is safe precisely because a miss excludes.
    pub binding_epoch: u32,
    /// Unix milliseconds at which the account was created.
    ///
    /// The durable answer to D28 clause (e)'s account-age row, and D31's
    /// resolved question 5 settled what is done with it: identity reads this
    /// row to authenticate a login anyway, compares it against the configured
    /// probation window, and signs the *verdict* into the session token's
    /// `on_probation` claim. Nobody downstream reads this field — the
    /// coordinator that enforces the filter has no FoundationDB at all (D31
    /// clause (d)) — so this is the origin of the fact and the token is its
    /// only carrier.
    pub created_ms: u64,
    /// Lifetime count of binding events appended for this account, maintained
    /// in the same transaction that appends the `dh` row (D31, resolved
    /// question 2).
    ///
    /// A write-time fold, not a live count of surviving rows: it is never
    /// decremented when history expires, which is exactly what makes expiry a
    /// pure range delete. It is also the churn signal a dispute asks for, and
    /// it outlives the rows it was folded from.
    pub binding_event_count: u32,
    /// Unix milliseconds of this account's **first** binding event, kept for
    /// the same reason and with the same discipline as
    /// [`Self::binding_event_count`]: together they still say "this account has
    /// rebound N times since T" after every row that would have proved it has
    /// been deleted.
    pub first_event_ms: u64,
}

/// The value stored at [`binding_key`]: the current owner of one NodeId.
///
/// Deleted, not tombstoned, when the binding is released — unbinding is
/// immediate (docs/09-services-and-ops.md §8) — and the released NodeId's
/// lookup becomes a miss. Under D31 (f) a miss excludes, so an attacker gains
/// nothing by shedding a NodeId just before submitting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BindingRow {
    /// The account this NodeId is currently bound to.
    pub account: AccountId,
    /// Unix milliseconds at which the binding was established.
    pub bound_at_ms: u64,
}

/// Which way a binding event moved (D31 (c)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BindKind {
    /// The NodeId became bound to the account.
    Bind,
    /// The NodeId was released from the account.
    Unbind,
}

/// The value stored at [`binding_history_key`]: one append-only binding event.
///
/// `da` and `db` are current-state rows and are mutated; this is the log they
/// are a fold of, and it is never updated in place (D31 (c)). It is what makes
/// `E(I)` reconstructible at all — up to the reader's cache staleness bound,
/// which D31 (c) is explicit is *not* exactness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BindingHistoryRow {
    /// The account the event bound the node to, or released it from.
    pub account: AccountId,
    /// Which way the event moved.
    pub kind: BindKind,
    /// Unix milliseconds on identity's clock, as evidence. The row's *order*
    /// comes from the key's versionstamp; this field is never an index.
    pub at_ms: u64,
}

// ---------------------------------------------------------------------------
// PersistId allocator family: `pid/next`
// ---------------------------------------------------------------------------
//
// The cluster-minted id counter (docs/08-persistence.md §6 `pid/next`,
// §7 "Id minting in the receipt"): intents allocate `PersistId`s inside the
// block grants. Reserving a block reads then atomically increments this key;
// that serializes only grant replenishment, while individual intents use the
// locally held durable grant. The value is an 8-byte
// little-endian u64 — little-endian because that is the representation
// `MutationType::Add` requires.

/// Key for the `PersistId` counter: `pid/{grid_id}/next` (§6 `pid/next`).
///
/// Grid-scoped so tests (and nested grids) allocate from independent counters;
/// the production grid is [`GridId::ROOT`]. Mutated **only** via
/// `MutationType::Add`; its value is 8-byte little-endian. A grant can be
/// abandoned after a crash, so callers must tolerate permanent gaps and must
/// never attempt to reclaim them.
#[must_use]
pub fn pid_next_key(grid: GridId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'n';
    key[1..5].copy_from_slice(&grid.0.to_be_bytes());
    key[5..9].copy_from_slice(b"next");
    key
}

// ---------------------------------------------------------------------------
// Ledger family: `ledger/bal/…`, `ledger/item/…`, `ledger/receipt/…`
// ---------------------------------------------------------------------------
//
// Critical-class rows, FDB-transaction-only writers (docs/08-persistence.md
// §6, §7). All three families share the `b'l'` prefix, discriminated by the
// second byte so range scans of one kind never see another. Balances encode
// **little-endian** so `MutationType::Add` applies directly (§7: the credit
// side is a blind atomic increment).
//
// **On versioning these three (D38 clause (d)(1)).** `ledger/bal/` is the one
// family in this module that *cannot* take a version field, and the reason is
// structural rather than a matter of scheduling: the value is a bare 16-byte
// little-endian i128 mutated in place by `MutationType::Add`, which is what
// makes a credit a blind atomic increment with no read and no conflict range.
// A version byte appended to it would be arithmetic — `Add` would carry it
// into the balance — and a version byte prepended would shift every digit.
// The row's self-description is its fixed width and its family byte, and it
// gains a version field only if it ever stops being an integer. The other two
// carry their reasons on `ItemRow` and `ReceiptRow`.

/// Key for a balance row: `ledger/bal/{account_id}/{asset_id}` → integer
/// balance (§6). Ids are big-endian so accounts and assets sort by id; the
/// **value** is a 16-byte little-endian i128 (or an 8-byte LE i64 prefix of
/// it) so `MutationType::Add` works (§7).
#[must_use]
pub fn ledger_bal_key(account: AccountId, asset: AssetId) -> [u8; 18] {
    let mut key = [0u8; 18];
    key[0] = b'l';
    key[1] = b'b';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key[10..18].copy_from_slice(&asset.0.to_be_bytes());
    key
}

/// Key for a unique item row: `ledger/item/{item_uid}` →
/// `(owner_ref, item_state)` (§6). One ownership row per unique item — the
/// single-ownership row is the anti-dupe invariant (§7).
#[must_use]
pub fn ledger_item_key(item: ItemUid) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'l';
    key[1] = b'i';
    key[2..10].copy_from_slice(&item.0.to_be_bytes());
    key
}

/// Key for a trade receipt row: `ledger/receipt/{versionstamp}` →
/// `(intent_id, parties, ops)` (§6) — the strictly-ordered audit trail.
///
/// The returned key carries 10 zero bytes at the versionstamp position (byte
/// offset 2, right after the two prefix bytes); write it with
/// `MutationType::SetVersionstampedKey` and a parameter whose final 4 bytes
/// encode that offset little-endian, so FDB substitutes the commit
/// versionstamp (the strict ordering comes from commit order itself).
#[must_use]
pub fn ledger_receipt_key() -> [u8; 12] {
    let mut key = [0u8; 12];
    key[0] = b'l';
    key[1] = b'r';
    // bytes 2..12: the zero placeholder the versionstamp is written into.
    key
}

/// Byte offset of the versionstamp placeholder inside [`ledger_receipt_key`].
///
/// Immediately after the two discriminator bytes, so a receipt row sorts by
/// commit version and by nothing else.
pub const LEDGER_RECEIPT_VERSIONSTAMP_OFFSET: u32 = 2;

/// The [`ledger_receipt_key`] in the exact form
/// `MutationType::SetVersionstampedKey` wants it: the 12-byte key followed by
/// the placeholder offset as a little-endian `u32`.
///
/// FDB strips those final four bytes, reads them as `pos`, and substitutes the
/// commit versionstamp over `key[pos..pos + 10]`. Building the parameter here
/// rather than at the call site keeps the offset and the placeholder in one
/// place: they are two halves of one fact, and a caller that hardcoded `2`
/// would keep compiling after the placeholder moved and would corrupt the
/// discriminator bytes instead of the versionstamp.
///
/// **One per transaction.** Every versionstamped write in a transaction gets
/// the *same* 10 bytes, so two receipts in one intent would be one key written
/// twice. An intent therefore banks exactly one receipt covering all its ops,
/// which is also what the `(intent_id, parties, ops)` value of §6 describes.
#[must_use]
pub fn ledger_receipt_versionstamped_key() -> [u8; 16] {
    let mut param = [0u8; 16];
    param[..12].copy_from_slice(&ledger_receipt_key());
    param[12..16].copy_from_slice(&LEDGER_RECEIPT_VERSIONSTAMP_OFFSET.to_le_bytes());
    param
}

/// The value stored at [`ledger_item_key`]: the `(owner_ref, item_state)` pair
/// of docs/08-persistence.md §6.
///
/// **This row is the anti-dupe invariant** (§7). There is exactly one of them
/// per unique item, so "who holds item X" has exactly one durable answer, and
/// a transfer is a read-check-write over it inside one serializable
/// transaction. Two concurrent transfers of X read the same row and therefore
/// share a read conflict range: at most one of them can commit, and the loser
/// re-runs and re-reads the winner's owner.
///
/// Postcard-encoded, like [`IntentRow`] — the same reason: one small,
/// versionless value written and read by this crate alone.
///
/// **Still unversioned, and this is the recorded reason** (D38 clause (d)(1),
/// which puts a version on `player/` and `ledger/` rows "at their next shape
/// change at the latest"). The mechanism is already in the tree —
/// [`orrery_protocol::atrest::encode_versioned`] appends the byte and
/// [`orrery_protocol::atrest::decode_versioned`] reads today's rows back as
/// v0 — so adopting it is a two-line change at each of this row's writers.
/// What buys the wait is that the row's shape is *not* changing in this work
/// item, and adopting the trailer now would rewrite six harness call sites
/// (`gates/p5-dupe-gauntlet`, `gates/p3-siblings` and the intent-commit tests all encode
/// this row by hand) for no byte that is not already decidable. The deadline
/// is real and it is a shape change, not a date: **the next field added here
/// adds the trailer in the same commit.**
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ItemRow {
    /// The `owner_ref` of §6: the account that holds this item.
    pub owner: AccountId,
    /// `Ruleset`-opaque item state (durability, charges, socketed contents).
    ///
    /// The cluster never interprets these bytes; a transfer carries them
    /// across unchanged, which is what makes an ownership move a move rather
    /// than a re-mint. `Vec<u8>` rather than a typed struct because
    /// docs/08-persistence.md §2.2 keeps op semantics `Ruleset`-side, and a
    /// typed state here would be this crate quietly defining a game.
    pub state: Vec<u8>,
}

/// One balance mutation recorded in a [`ReceiptRow`].
///
/// `AccountId`, rather than the transport's `NodeId`, is deliberate: ledger
/// rows are account-scoped, so a conservation reader can sum this record
/// without joining P5's `id/db` binding history (#832).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReceiptBalanceDelta {
    /// Account whose balance changed.
    pub account: AccountId,
    /// Asset in which the balance is denominated.
    pub asset: AssetId,
    /// Signed amount applied to the balance row.
    pub delta: i64,
}

/// One item-ownership mutation recorded in a [`ReceiptRow`].
///
/// Both ends are optional so the same honest shape covers transfers
/// (`Some -> Some`), mints (`None -> Some`) and burns (`Some -> None`). The
/// current cluster-interpreted item op is a transfer; making the endpoints
/// explicit now prevents a future mint writer from inventing an audit shape
/// that cannot express its missing prior owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReceiptOwnershipTransition {
    /// Stable unique item whose ownership row changed.
    pub item: ItemUid,
    /// Owner before this transaction, or `None` for a mint.
    pub before: Option<AccountId>,
    /// Owner after this transaction, or `None` for a burn.
    pub after: Option<AccountId>,
}

/// The value stored at [`ledger_receipt_key`]: the complete economic effects
/// of one intent transaction.
///
/// Strictly ordered by construction — the key is the commit versionstamp, so
/// receipt order *is* commit order and no clock is involved. This is the row
/// an operator (or the P5 dupe gauntlet) reads to answer "what actually
/// happened, in what order", independently of the `intent/` idempotency rows,
/// which are swept after an hour.
///
/// Every balance or ownership write represented by the intent is present in
/// `balance_deltas` or `ownership`, including pure credits. This is the source
/// #615's archive sweep can reconcile; `ops` alone remains useful provenance
/// but is not treated as an effect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReceiptRow {
    /// The intent that committed these effects.
    pub intent_id: u128,
    /// The accounts the intent moved value between, in first-seen order.
    pub parties: Vec<AccountId>,
    /// The op ids the intent carried, in op order — including the
    /// `Ruleset`-opaque ones, which the cluster records without interpreting.
    pub ops: Vec<u16>,
    /// Every signed balance mutation staged by the intent, in write order.
    pub balance_deltas: Vec<ReceiptBalanceDelta>,
    /// Every ownership mutation staged by the intent, in write order.
    pub ownership: Vec<ReceiptOwnershipTransition>,
}

/// Encoding trailer written by the first enriched receipt shape.
pub const RECEIPT_ENCODING_V1: EncodingVersion = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct ReceiptRowV0 {
    intent_id: u128,
    parties: Vec<AccountId>,
    ops: Vec<u16>,
}

/// A receipt value could not be decoded according to its at-rest generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptDecodeError(String);

impl core::fmt::Display for ReceiptDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for ReceiptDecodeError {}

/// Encode an enriched receipt with D38's version trailer.
///
/// # Errors
///
/// Returns postcard's serialization error if the row cannot be encoded.
pub fn encode_receipt_row(row: &ReceiptRow) -> Result<Vec<u8>, postcard::Error> {
    orrery_protocol::atrest::encode_versioned(row, RECEIPT_ENCODING_V1)
}

/// Decode a permanent receipt row, including the pre-Shape-C v0 generation.
///
/// The bootstrap is deterministic: a v0 body consumes the complete value. If
/// bytes remain after that body, the final byte is the D38 trailer and the
/// whole preceding body is decoded according to that named generation. Old
/// receipts necessarily recover with empty effect vectors; they remain honest
/// provenance but cannot retroactively supply effects that were never stored.
///
/// # Errors
///
/// Returns [`ReceiptDecodeError`] for a corrupt body or an unsupported trailer.
pub fn decode_receipt_row(
    bytes: &[u8],
) -> Result<(ReceiptRow, EncodingVersion), ReceiptDecodeError> {
    let (legacy, rest) = postcard::take_from_bytes::<ReceiptRowV0>(bytes)
        .map_err(|error| ReceiptDecodeError(format!("receipt body decode: {error}")))?;
    if rest.is_empty() {
        return Ok((
            ReceiptRow {
                intent_id: legacy.intent_id,
                parties: legacy.parties,
                ops: legacy.ops,
                balance_deltas: Vec::new(),
                ownership: Vec::new(),
            },
            orrery_protocol::atrest::ENCODING_V0,
        ));
    }

    let (&version, body) = bytes
        .split_last()
        .ok_or_else(|| ReceiptDecodeError("receipt value is empty".to_owned()))?;
    if version != RECEIPT_ENCODING_V1 {
        return Err(ReceiptDecodeError(format!(
            "unsupported receipt encoding version {version}"
        )));
    }
    let row = postcard::from_bytes(body)
        .map_err(|error| ReceiptDecodeError(format!("receipt v1 body decode: {error}")))?;
    Ok((row, version))
}

/// Key for the hot-ledger sweep's resumable cursor:
/// `ledger/audit-cursor` → postcard [`AuditCursorRow`].
///
/// **Auditor-owned state inside the ledger family, not a ledger row.** The
/// sweep (#330) reads `ledger/bal`, `ledger/item` and `ledger/receipt`, and it
/// needs one durable byte of its own to remember where the receipt walk
/// stopped. D32 clause (g) consumes "live and sweeping", and a sweeper that
/// forgets its position on restart re-reads receipts it has already judged —
/// unbounded work over a family nothing ever sweeps, which is precisely the
/// growth the cursor exists to bound.
///
/// **Sub-discriminated, not a new family.** The one-byte key-family budget is
/// nearly exhausted (D35), so the cursor claims no fresh letter: it lives under
/// the ledger's own `l` prefix at discriminator `a` — before `lb`, so the
/// registered sub-kind table stays in discriminator order and every existing
/// disjointness proof walks it unchanged. The sweep's family walk skips this
/// sub-span explicitly: auditor bookkeeping is not an economy row, and a sweep
/// that audited its own cursor would be one bug away from reporting on itself.
///
/// Written with the module's recognized literal form (`key[0]` then `key[1]`)
/// so the pair-aware guard sees it and demands exactly what this entry does:
/// registration beside the other four `(l, …)` pairs.
#[must_use]
pub fn ledger_audit_cursor_key() -> [u8; 2] {
    let mut key = [0u8; 2];
    key[0] = b'l';
    key[1] = b'a';
    key
}

/// Inclusive start of the sweep-cursor sub-span (`la`, one row today).
#[must_use]
pub fn ledger_audit_cursor_range_start() -> Vec<u8> {
    ledger_audit_cursor_key().to_vec()
}

/// Exclusive end of the sweep-cursor sub-span (the first byte past `la`).
///
/// `[lb, lc)` begins at `ledger/bal`'s first possible key, so this bound and
/// that one meet without overlapping whatever lands between — which is nothing,
/// because the registered table proves it.
#[must_use]
pub fn ledger_audit_cursor_range_end() -> Vec<u8> {
    vec![b'l', b'b']
}

/// The value stored at [`ledger_audit_cursor_key`]: where the incremental
/// sweep's receipt walk stopped.
///
/// `last_receipt_key` is the **complete raw key** of the last receipt the sweep
/// processed — all twelve bytes, versionstamp included — stored verbatim rather
/// than decomposed into parts, so resuming needs no re-derivation and cannot
/// invent a second ordering: the next pass begins at
/// `last_receipt_key ‖ 0x00`, exclusive, which is every commit after the one
/// already judged and nothing else. Receipt keys sort by commit versionstamp
/// alone ([`ReceiptRow`]), so "after the cursor" *is* "after the pass's start
/// time" by construction.
///
/// Postcard-encoded, like [`ItemRow`] and [`ReceiptRow`]: one small,
/// versionless value written and read by this crate alone. It is unversioned on
/// [`ItemRow`]'s recorded reason (D38 clause (d)(1)) — the shape is not
/// changing here, and the next field added adds the trailer in the same
/// commit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditCursorRow {
    /// Raw bytes of the last receipt key the sweep processed; empty when the
    /// sweep has never run, meaning the next pass starts at the family's head.
    ///
    /// A well-formed row always holds either zero or exactly twelve bytes
    /// ([`ledger_receipt_key`]'s width); anything else decodes as corrupt and
    /// the sweep restarts from the head rather than guessing.
    pub last_receipt_key: Vec<u8>,
    /// Unix milliseconds at which the sweep wrote the row. Diagnostic only —
    /// the order comes from the receipt keys, never from this clock.
    pub updated_at_ms: u64,
}

// Ledger sub-span bounds, for readers that walk one kind.
//
// These are *range bounds*, written in the `vec![b'x']` form the module's
// scanner deliberately does not register as families — an exclusive end is one
// past a span, not a span. The four economic spans are exactly the registered
// `(l, …)` sub-kind pairs; the audit-cursor pair has its own bounds above.

/// Inclusive start of the whole ledger family.
#[must_use]
pub fn ledger_range_start() -> Vec<u8> {
    vec![b'l']
}

/// Exclusive end of the whole ledger family.
#[must_use]
pub fn ledger_range_end() -> Vec<u8> {
    vec![b'm']
}

/// Inclusive start of the balance sub-span (`lb`).
#[must_use]
pub fn ledger_bal_range_start() -> Vec<u8> {
    vec![b'l', b'b']
}

/// Exclusive end of the balance sub-span (`lc`).
#[must_use]
pub fn ledger_bal_range_end() -> Vec<u8> {
    vec![b'l', b'c']
}

/// Inclusive start of the ownership sub-span (`li`).
#[must_use]
pub fn ledger_item_range_start() -> Vec<u8> {
    vec![b'l', b'i']
}

/// Exclusive end of the ownership sub-span (`lj`).
#[must_use]
pub fn ledger_item_range_end() -> Vec<u8> {
    vec![b'l', b'j']
}

/// Inclusive start of the receipt sub-span (`lr`).
#[must_use]
pub fn ledger_receipt_range_start() -> Vec<u8> {
    vec![b'l', b'r']
}

/// Exclusive end of the receipt sub-span (`ls`) — the range both harness
/// receipt scanners walk.
#[must_use]
pub fn ledger_receipt_range_end() -> Vec<u8> {
    vec![b'l', b's']
}

// ---------------------------------------------------------------------------
// Journal archive metadata family: `jarchive/{node_id}/{segment_seq}`
// ---------------------------------------------------------------------------
//
// Archive tailer metadata rows (docs/08-persistence.md §11). Each row records
// one archived journal segment bundle: where the Parquet object lives, which
// (grid, cell) ranges it contains, the LSN span it covers, and a checksum over
// the object bytes. The family prefix is `z` and the sub-kind discriminator is
// `a`, so the row key is `za ‖ node_id: 32 bytes ‖ segment_seq: u64 BE`.

/// Family byte for the journal-archive metadata family.
pub const JARCHIVE_PREFIX: u8 = b'z';
/// Sub-kind discriminator for archive metadata rows inside the `z` family.
pub const JARCHIVE_DISCRIMINATOR: u8 = b'a';

/// One cell range included in an archive object.
///
/// `start` and `end` are Morton `CellId`s in `grid`'s coordinate space; the
/// range is half-open `[start, end)` so contiguous ranges can be concatenated
/// without overlap or gaps. The tailer keeps a object's `cell_ranges` sorted
/// and non-overlapping.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JarchiveCellRange {
    /// Grid whose `CellId` space the bounds are expressed in.
    pub grid: GridId,
    /// First cell in the half-open range (inclusive).
    pub start: CellId,
    /// Cell after the last cell in the range (exclusive).
    pub end: CellId,
}

/// The LSN span covered by one archive object.
///
/// Both bounds are inclusive: the object contains every journal record whose
/// `lsn` satisfies `start <= lsn <= end`. This is the server-assigned,
/// monotonic axis the archive selects and sorts on (docs/08-persistence.md §11,
/// #813); `tick` is stored as displayed metadata and for the `(entity, tick)`
/// idempotency key it already serves, but it is never the archive's time axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JarchiveLsnSpan {
    /// First LSN included in the object (inclusive).
    pub start: Lsn,
    /// Last LSN included in the object (inclusive).
    pub end: Lsn,
}

/// Metadata row stored under `jarchive/{node_id}/{segment_seq}`.
///
/// The value is postcard-encoded and carries enough information for a reader
/// to decide whether an object is relevant to a `(cell range, lsn range)` query
/// without downloading the object itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JarchiveMetadata {
    /// Object-store key of the Parquet object (e.g. `s3://bucket/...`).
    pub object_key: String,
    /// Cell ranges included in the object. Kept sorted and non-overlapping by
    /// the tailer on write.
    pub cell_ranges: Vec<JarchiveCellRange>,
    /// LSN span the object covers.
    pub lsn_span: JarchiveLsnSpan,
    /// BLAKE3 digest over the object bytes, used to verify the object before
    /// the archive watermark advances (docs/08-persistence.md §11).
    pub checksum: [u8; 32],
}

/// Key for a journal-archive metadata row:
/// `za ‖ node_id: 32 bytes ‖ segment_seq: u64 BE`.
///
/// The discriminator bytes are written as literals so the source scanners in
/// [`every_family_prefix_written_in_this_module_is_registered`] and
/// [`every_discriminated_constructor_is_registered_with_its_pair`] can see the
/// family and its `(z, a)` pair; the named constants [`JARCHIVE_PREFIX`] and
/// [`JARCHIVE_DISCRIMINATOR`] are the source of truth for readers and tests.
#[must_use]
pub fn jarchive_key(node_id: &NodeId, segment_seq: u64) -> [u8; 42] {
    let mut key = [0u8; 42];
    key[0] = b'z';
    key[1] = b'a';
    key[2..34].copy_from_slice(node_id.as_bytes());
    key[34..].copy_from_slice(&segment_seq.to_be_bytes());
    key
}

/// Decode a `jarchive/{node_id}/{segment_seq}` key.
///
/// Returns `None` for any key that is not exactly 42 bytes beginning with the
/// registered `za` pair, or when the 32 node bytes are not a valid ed25519
/// public key — a `NodeId` is a curve point, so not every 32-byte string is
/// one.
#[must_use]
pub fn decode_jarchive_key(key: &[u8]) -> Option<(NodeId, u64)> {
    if key.len() != 42 || key[0] != JARCHIVE_PREFIX || key[1] != JARCHIVE_DISCRIMINATOR {
        return None;
    }
    let node_id = NodeId::from_bytes(key[2..34].try_into().ok()?).ok()?;
    let segment_seq = u64::from_be_bytes(key[34..42].try_into().ok()?);
    Some((node_id, segment_seq))
}

/// Inclusive start of the whole `jarchive/` family span.
#[must_use]
pub fn jarchive_range_start() -> Vec<u8> {
    vec![JARCHIVE_PREFIX, JARCHIVE_DISCRIMINATOR]
}

/// Exclusive end of the whole `jarchive/` family span.
#[must_use]
pub fn jarchive_range_end() -> Vec<u8> {
    vec![JARCHIVE_PREFIX, JARCHIVE_DISCRIMINATOR.wrapping_add(1)]
}

/// Inclusive start of the `jarchive/{node_id}/…` span for one node.
#[must_use]
pub fn jarchive_node_range_start(node_id: &NodeId) -> Vec<u8> {
    let mut key = Vec::with_capacity(34);
    key.push(JARCHIVE_PREFIX);
    key.push(JARCHIVE_DISCRIMINATOR);
    key.extend_from_slice(node_id.as_bytes());
    key
}

/// Exclusive end of the `jarchive/{node_id}/…` span for one node.
#[must_use]
pub fn jarchive_node_range_end(node_id: &NodeId) -> Vec<u8> {
    let mut key = jarchive_node_range_start(node_id);
    for byte in key.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            return key;
        }
    }
    // Overflow of the entire 34-byte prefix: the next byte past `za‖node` is
    // the single byte `zb`, which is also the family end bound.
    vec![JARCHIVE_PREFIX, JARCHIVE_DISCRIMINATOR.wrapping_add(1)]
}

/// Encode a [`JarchiveMetadata`] value as postcard bytes.
///
/// # Errors
///
/// Returns [`CheckpointError::Store`] if postcard serialization fails.
pub fn encode_jarchive_metadata(meta: &JarchiveMetadata) -> Result<Vec<u8>, CheckpointError> {
    postcard::to_stdvec(meta)
        .map_err(|e| CheckpointError::Store(format!("jarchive metadata encode: {e}")))
}

/// Decode a [`JarchiveMetadata`] value from postcard bytes.
///
/// # Errors
///
/// Returns [`CheckpointError::Store`] if postcard deserialization fails.
pub fn decode_jarchive_metadata(bytes: &[u8]) -> Result<JarchiveMetadata, CheckpointError> {
    postcard::from_bytes(bytes)
        .map_err(|e| CheckpointError::Store(format!("jarchive metadata decode: {e}")))
}

// ---------------------------------------------------------------------------
// Receipt archive metadata: `rarchive/{last receipt versionstamp}`
// ---------------------------------------------------------------------------
//
// Shares the archive `z` family under ASCII discriminator `r`. One row is the
// durable publication marker for one bounded receipt page. The key uses the
// page's last commit versionstamp, so the greatest row is also the restart
// cursor and no second durable watermark can disagree with it.

/// Sub-kind discriminator for receipt-archive metadata inside the `z` family.
pub const RARCHIVE_DISCRIMINATOR: u8 = b'r';
/// At-rest encoding generation of [`RarchiveMetadata`].
pub const RARCHIVE_METADATA_ENCODING: EncodingVersion = 1;

/// Durable publication record for one archived receipt page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RarchiveMetadata {
    /// Object-store key of the page's Parquet object.
    pub object_key: String,
    /// Complete first `ledger/receipt/` key included in the page.
    pub first_receipt_key: [u8; 12],
    /// Complete last `ledger/receipt/` key included in the page.
    pub last_receipt_key: [u8; 12],
    /// Number of receipt rows in the page.
    pub rows: u32,
    /// BLAKE3 digest verified by re-reading the uploaded object.
    pub checksum: [u8; 32],
}

/// `zr || last receipt versionstamp:[u8;10]`.
///
/// `last_receipt_key` must be a complete `lr` key. Keeping the prefix check in
/// the constructor makes it impossible to file an arbitrary cursor as receipt
/// archive progress.
#[must_use]
pub fn rarchive_key(last_receipt_key: &[u8; 12]) -> Option<[u8; 12]> {
    if &last_receipt_key[..2] != b"lr" {
        return None;
    }
    let mut key = [0u8; 12];
    key[0] = b'z';
    key[1] = b'r';
    key[2..].copy_from_slice(&last_receipt_key[2..]);
    Some(key)
}

/// Decode a receipt-archive metadata key back to the complete `lr` cursor.
#[must_use]
pub fn decode_rarchive_key(key: &[u8]) -> Option<[u8; 12]> {
    if key.len() != 12 || key[0] != JARCHIVE_PREFIX || key[1] != RARCHIVE_DISCRIMINATOR {
        return None;
    }
    let mut receipt_key = [0u8; 12];
    receipt_key[..2].copy_from_slice(b"lr");
    receipt_key[2..].copy_from_slice(&key[2..]);
    Some(receipt_key)
}

/// Inclusive start of receipt-archive metadata.
#[must_use]
pub fn rarchive_range_start() -> Vec<u8> {
    vec![JARCHIVE_PREFIX, RARCHIVE_DISCRIMINATOR]
}

/// Exclusive end of receipt-archive metadata.
#[must_use]
pub fn rarchive_range_end() -> Vec<u8> {
    vec![JARCHIVE_PREFIX, RARCHIVE_DISCRIMINATOR.wrapping_add(1)]
}

/// Encode receipt-archive metadata with its D38 trailer.
///
/// # Errors
///
/// Returns [`CheckpointError::Store`] if postcard serialization fails.
pub fn encode_rarchive_metadata(meta: &RarchiveMetadata) -> Result<Vec<u8>, CheckpointError> {
    orrery_protocol::atrest::encode_versioned(meta, RARCHIVE_METADATA_ENCODING)
        .map_err(|e| CheckpointError::Store(format!("rarchive metadata encode: {e}")))
}

/// Decode receipt-archive metadata and require its known generation.
///
/// # Errors
///
/// Returns [`CheckpointError::Store`] for corrupt bytes or an unknown version.
pub fn decode_rarchive_metadata(bytes: &[u8]) -> Result<RarchiveMetadata, CheckpointError> {
    let (meta, version) = orrery_protocol::atrest::decode_versioned(bytes)
        .map_err(|e| CheckpointError::Store(format!("rarchive metadata decode: {e}")))?;
    if version != RARCHIVE_METADATA_ENCODING {
        return Err(CheckpointError::Store(format!(
            "unsupported rarchive metadata encoding version {version}"
        )));
    }
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The byte offset at which FoundationDB substitutes the commit versionstamp.
pub const STRIKE_VERSIONSTAMP_OFFSET: u32 = 10;

/// `ya || account:u64-be || versionstamp:[u8;10]`, before substitution.
#[must_use]
pub fn strike_key(account: AccountId) -> [u8; 20] {
    let mut key = [0; 20];
    key[0] = b'y';
    key[1] = b'a';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// `yb || account:u64-be || blake3(ruleset, episode):[u8;32]`.
///
/// The marker makes a confirmed divergence episode idempotent without
/// changing D33's immutable `ya` strike-row value format.
#[must_use]
pub fn strike_episode_key(account: AccountId, episode_digest: &[u8; 32]) -> [u8; 42] {
    let mut key = [0; 42];
    key[0] = b'y';
    key[1] = b'b';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key[10..].copy_from_slice(episode_digest);
    key
}

/// [`strike_key`] in `SetVersionstampedKey` parameter form.
#[must_use]
pub fn strike_versionstamped_key(account: AccountId) -> [u8; 24] {
    let mut key = [0; 24];
    key[..20].copy_from_slice(&strike_key(account));
    key[20..].copy_from_slice(&STRIKE_VERSIONSTAMP_OFFSET.to_le_bytes());
    key
}

/// First key in one account's contiguous `ya` span.
#[must_use]
pub fn strike_account_range_start(account: AccountId) -> Vec<u8> {
    strike_key(account)[..10].to_vec()
}

/// Exclusive end of one account's contiguous `ya` span.
#[must_use]
pub fn strike_account_range_end(account: AccountId) -> Vec<u8> {
    let mut end = strike_account_range_start(account);
    for byte in end.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            return end;
        }
    }
    vec![b'y', b'b']
}

// ---------------------------------------------------------------------------
// Restore-hold index: `yc/{source node}/{entity}/{product}`
// ---------------------------------------------------------------------------

/// The `y`-family discriminator for the restore-hold join.
///
/// `ya` is the strike ledger and `yb` is the episode-dedup row.  This index is
/// deliberately a third, independent subspace: its rows are projections of
/// those products, not another representation of either product.
pub const RESTORE_HOLD_DISCRIMINATOR: u8 = b'c';

const RESTORE_HOLD_PREFIX_LEN: usize = 2 + 32 + 8;
const RESTORE_HOLD_STRIKE_KIND: u8 = b'a';
const RESTORE_HOLD_ANNULMENT_KIND: u8 = b'b';

/// One durable product named by a restore-hold index key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreHoldProduct {
    /// A `ya` strike row, named by its account and committed versionstamp.
    Strike {
        /// The account prefix of the strike product.
        account: AccountId,
        /// The FDB versionstamp suffix of the strike key.
        versionstamp: [u8; 10],
    },
    /// An `intent/` row whose finality became `Annulled`.
    Annulment {
        /// The durable intent key's id component.
        intent_id: u128,
    },
}

impl RestoreHoldProduct {
    /// A stable, operator-facing spelling of the product's actual FDB key.
    #[must_use]
    pub fn stable_key(self) -> String {
        match self {
            Self::Strike {
                account,
                versionstamp,
            } => format!("ya/{:016x}/{}", account.0, versionstamp_hex(&versionstamp)),
            Self::Annulment { intent_id } => format!("i/{intent_id:032x}"),
        }
    }
}

fn versionstamp_hex(versionstamp: &[u8; 10]) -> String {
    use core::fmt::Write as _;

    let mut text = String::with_capacity(20);
    for byte in versionstamp {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// `yc || source_node || entity`, the common entity-scoped prefix.
///
/// The durable products name evidence windows in client-supplied ticks, while
/// restore selects server-assigned journal LSNs. Neither product carries a
/// server LSN for its evidence window, so filing order cannot safely join
/// them. An entity-scoped projection is deliberately conservative: any
/// retained adjudication product for the entity holds its restore.
fn restore_hold_prefix(source_node: &NodeId, entity: PersistId) -> Vec<u8> {
    let mut key = vec![0; RESTORE_HOLD_PREFIX_LEN];
    key[0] = b'y';
    key[1] = b'c';
    key[2..34].copy_from_slice(source_node.as_bytes());
    key[34..42].copy_from_slice(&entity.0.to_be_bytes());
    key
}

/// A `SetVersionstampedKey` parameter for the index of one `ya` strike.
///
/// The versionstamp is substituted in the same FDB transaction as the strike
/// key, so the product name returned to an operator names the exact `ya` row.
#[must_use]
pub fn restore_hold_strike_versionstamped_key(
    source_node: &NodeId,
    entity: PersistId,
    account: AccountId,
) -> Vec<u8> {
    let mut key = restore_hold_prefix(source_node, entity);
    key.push(RESTORE_HOLD_STRIKE_KIND);
    key.extend_from_slice(&account.0.to_be_bytes());
    let versionstamp_offset = u32::try_from(key.len()).expect("restore hold key fits u32");
    key.extend_from_slice(&[0; 10]);
    key.extend_from_slice(&versionstamp_offset.to_le_bytes());
    key
}

/// The ordinary index key for one annulled `intent/` product.
#[must_use]
pub fn restore_hold_annulment_key(
    source_node: &NodeId,
    entity: PersistId,
    intent_id: u128,
) -> Vec<u8> {
    let mut key = restore_hold_prefix(source_node, entity);
    key.push(RESTORE_HOLD_ANNULMENT_KIND);
    key.extend_from_slice(&intent_id.to_be_bytes());
    key
}

/// The inclusive-start key for all held products of one source/entity.
#[must_use]
pub fn restore_hold_range_start(source_node: &NodeId, entity: PersistId) -> Vec<u8> {
    restore_hold_prefix(source_node, entity)
}

/// Exclusive end for all held products of one source/entity.
#[must_use]
pub fn restore_hold_range_end(source_node: &NodeId, entity: PersistId) -> Vec<u8> {
    let mut prefix = restore_hold_prefix(source_node, entity);
    for byte in prefix.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            return prefix;
        }
    }
    vec![b'z']
}

/// Decode an index key into its source, entity and durable product.
#[must_use]
pub fn decode_restore_hold_key(key: &[u8]) -> Option<(NodeId, PersistId, RestoreHoldProduct)> {
    if key.len() < RESTORE_HOLD_PREFIX_LEN + 1
        || key[0] != b'y'
        || key[1] != RESTORE_HOLD_DISCRIMINATOR
    {
        return None;
    }
    let source_node = NodeId::from_bytes(key[2..34].try_into().ok()?).ok()?;
    let entity = PersistId::new(u64::from_be_bytes(key[34..42].try_into().ok()?));
    let product = match key[42] {
        RESTORE_HOLD_STRIKE_KIND if key.len() == 61 => RestoreHoldProduct::Strike {
            account: AccountId::new(u64::from_be_bytes(key[43..51].try_into().ok()?)),
            versionstamp: key[51..61].try_into().ok()?,
        },
        RESTORE_HOLD_ANNULMENT_KIND if key.len() == 59 => RestoreHoldProduct::Annulment {
            intent_id: u128::from_be_bytes(key[43..59].try_into().ok()?),
        },
        _ => return None,
    };
    Some((source_node, entity, product))
}

/// First key in the complete executor-owned D33 `ya` strike ledger.
///
/// Retention maintenance walks this span in account order. It is deliberately
/// separate from the hot identity read, which stays on one account's bounded
/// contiguous subrange.
#[must_use]
pub fn strike_range_start() -> Vec<u8> {
    vec![b'y', b'a']
}

/// Exclusive end of the complete executor-owned D33 `ya` strike ledger.
#[must_use]
pub fn strike_range_end() -> Vec<u8> {
    vec![b'y', b'b']
}

/// First key in the `yb` episode-dedup family.
#[must_use]
pub fn strike_episode_range_start() -> Vec<u8> {
    vec![b'y', b'b']
}

/// Exclusive end of the `yb` episode-dedup family.
#[must_use]
pub fn strike_episode_range_end() -> Vec<u8> {
    vec![b'y', b'c']
}

/// First key in the whole `yc` restore-hold family, across every source.
///
/// Retention maintenance needs the family span; the hot restore check stays
/// on one source-and-entity subrange via [`restore_hold_range_start`].
#[must_use]
pub fn restore_hold_family_range_start() -> Vec<u8> {
    vec![b'y', RESTORE_HOLD_DISCRIMINATOR]
}

/// Exclusive end of the whole `yc` restore-hold family.
#[must_use]
pub fn restore_hold_family_range_end() -> Vec<u8> {
    vec![b'y', RESTORE_HOLD_DISCRIMINATOR + 1]
}

// ---------------------------------------------------------------------------
// Filing-notice queue: `yd || account:u64-be`
// ---------------------------------------------------------------------------

/// The `y`-family discriminator for D33 clause (e)'s filing-notice queue.
///
/// `ya` is the strike ledger, `yb` the episode-dedup marker and `yc` the
/// restore-hold join. This fourth subspace is none of those: it is a *work
/// queue*, written by the executor at the instant it files a strike and
/// drained by identity once it has evaluated the account's standing.
///
/// It exists because D33 clause (e) requires standing to be evaluated "after
/// every live filing", and the two halves of that sentence sit in different
/// processes that may not call each other. `orrery_identity` depends on this
/// crate and never the reverse, so the executor cannot notify identity
/// in-process; a durable row it writes and identity reads is the only
/// direction the dependency graph allows. See `orrery_identity::filing`.
///
/// The value is the filing instant, not a strike: a notice carries no verdict,
/// no weight and no [`StrikeMode`](crate::adjudication::StrikeMode). The
/// scorer over `ya` remains the sole authority on what an account's standing
/// is, so a notice can never disagree with it — the worst a spurious notice
/// can do is make identity re-score an account and find nothing.
pub const FILING_NOTICE_DISCRIMINATOR: u8 = b'd';

/// `yd || account:u64-be`, one account's pending filing notice.
///
/// Keyed by account rather than by versionstamp, so the family is bounded by
/// the number of accounts awaiting evaluation rather than by the number of
/// filings ever made. A second filing for an account already queued overwrites
/// the notice with the later instant instead of appending, which is exactly
/// the collapse a queue of "re-score this account" wants.
#[must_use]
pub fn filing_notice_key(account: AccountId) -> [u8; 10] {
    let mut key = [0; 10];
    key[0] = b'y';
    // Spelled as a literal, not as [`FILING_NOTICE_DISCRIMINATOR`], for the
    // same reason `restore_hold_prefix` spells `b'c'`: D35 clause (c)'s
    // registry gate scans this module's *text* for a byte literal assigned to
    // the second key byte, and a constructor that hides its discriminator
    // behind a constant is invisible to it. The two are held together by
    // [`tests::the_filing_notice_span_does_not_overlap_the_restore_hold_span`],
    // which places this key inside the const-derived family span.
    key[1] = b'd';
    key[2..10].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// First key in the whole `yd` filing-notice family.
#[must_use]
pub fn filing_notice_range_start() -> Vec<u8> {
    vec![b'y', FILING_NOTICE_DISCRIMINATOR]
}

/// Exclusive end of the whole `yd` filing-notice family.
#[must_use]
pub fn filing_notice_range_end() -> Vec<u8> {
    vec![b'y', FILING_NOTICE_DISCRIMINATOR + 1]
}

/// The account a [`filing_notice_key`] names, or `None` if it is not one.
///
/// Returns an option rather than panicking: this decodes keys a range read
/// produced, and a foreign key in the span is a data fact to skip rather than
/// a reason to take down the sweep.
#[must_use]
pub fn filing_notice_account(key: &[u8]) -> Option<AccountId> {
    let key: &[u8; 10] = key.try_into().ok()?;
    if key[0] != b'y' || key[1] != FILING_NOTICE_DISCRIMINATOR {
        return None;
    }
    Some(AccountId::new(u64::from_be_bytes(
        key[2..10].try_into().expect("8 bytes"),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ComponentBag, ComponentSlot};

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

    // -----------------------------------------------------------------------
    // The `world/` value envelope is self-describing (D38 clause (d))
    // -----------------------------------------------------------------------

    #[test]
    fn a_genuinely_unversioned_world_value_reads_as_v0() {
        // The bootstrap rule, on bytes a writer that predates versioning
        // produced: `LIVE_TAG ‖ bag`, nothing else, no floor anywhere in it.
        // The requirement is that this row is *read as v0* — not refused, and
        // not guessed at by peeking into the bag.
        let bag = b"hp=100";
        let legacy = encode_live_value(bag);
        assert_eq!(legacy.len(), bag.len() + 1, "no floor field is present");

        assert_eq!(
            world_value_schema_floor(&legacy),
            Some(SCHEMA_V0),
            "absent == v0 (D38 (d)(1)): an unversioned live row is version 0"
        );
        assert_eq!(
            world_value_components(&legacy),
            Some(&bag[..]),
            "and its bag is recovered whole, so the row stays readable"
        );
        assert!(
            !world_value_is_stale(&legacy, SCHEMA_V0),
            "a v0 row is not behind a v0 current"
        );
    }

    #[test]
    fn a_stale_world_value_is_detectable_without_decoding_its_components() {
        // D38 clause (d)(2). The sweep walks cold `world/` ranges it cannot
        // interpret: the bag here is deliberately **not** decodable as
        // anything — random bytes standing in for a game type this crate has
        // never seen — and staleness must still come out right.
        let opaque_bag: &[u8] = &[0xFF, 0x9E, 0x00, 0x7B, 0xC3, 0xFF, 0xFF];
        assert!(
            ComponentBag::decode(opaque_bag).is_err(),
            "the fixture must be undecodable, or this test proves nothing \
             about a reader that cannot decode game types"
        );

        let behind = encode_versioned_live_value(3, opaque_bag);
        let current = encode_versioned_live_value(9, opaque_bag);

        assert!(
            world_value_is_stale(&behind, 9),
            "a row at floor 3 is behind a current schema of 9"
        );
        assert!(
            !world_value_is_stale(&current, 9),
            "a row already at 9 is not"
        );
        // And the decision is a fixed-offset read, not a parse: the floor sits
        // in the envelope, and the bytes after it were never touched.
        assert_eq!(world_value_schema_floor(&behind), Some(3));
        assert_eq!(world_value_components(&behind), Some(opaque_bag));
    }

    #[test]
    fn the_envelope_floor_is_the_bag_floor() {
        // The marker is a *summary* of the bag, not an independent counter
        // (D38 (d)(2)): a writer stamps what the bag says, so a row whose
        // envelope disagrees with its slots is convicted by the bag itself.
        let bag = ComponentBag {
            slots: vec![
                ComponentSlot {
                    component: orrery_core::ComponentTypeId(1),
                    schema_version: 5,
                    payload: bytes::Bytes::from_static(b"a"),
                },
                ComponentSlot {
                    component: orrery_core::ComponentTypeId(2),
                    schema_version: 2,
                    payload: bytes::Bytes::from_static(b"b"),
                },
            ],
        };
        let encoded = bag.encode().expect("encodes");
        let value = encode_versioned_live_value(bag.schema_floor(), &encoded);
        assert_eq!(world_value_schema_floor(&value), Some(2));
        assert_eq!(
            ComponentBag::decode(world_value_components(&value).expect("live"))
                .expect("decodes")
                .schema_floor(),
            2,
            "the envelope repeats what the slots already say"
        );
    }

    #[test]
    fn the_three_world_value_tags_are_distinct_and_only_live_ones_carry_schema() {
        let tomb = encode_tombstone_value(&Tombstone {
            cell: CellId::from_bits(CELL).unwrap(),
            tick: orrery_protocol::Tick::new(1),
            gc_deadline_ms: 2,
        })
        .expect("encodes");
        let tags = [
            encode_live_value(b"x")[0],
            tomb[0],
            encode_versioned_live_value(1, b"x")[0],
        ];
        assert_eq!(
            std::collections::BTreeSet::from(tags).len(),
            3,
            "LIVE_TAG, TOMBSTONE_TAG and LIVE_VERSIONED_TAG are three values"
        );
        assert_eq!(
            world_value_schema_floor(&tomb),
            None,
            "a tombstone has no components and so no schema to be behind"
        );
        assert!(!world_value_is_stale(&tomb, u32::MAX));
        assert_eq!(
            world_value_schema_floor(&[0x7F, 0x00]),
            None,
            "an unknown tag is a row this reader may not speak for"
        );
        assert_eq!(
            world_value_schema_floor(&[LIVE_VERSIONED_TAG, 0, 0]),
            None,
            "a versioned value too short to hold its floor is truncated, not v0"
        );
    }

    #[test]
    fn the_versioned_envelope_costs_four_bytes_over_the_v0_one() {
        // D38 clause (f) prices the marker; the seeder's WORLD_ROW_OVERHEAD
        // depends on this number being what it says it is.
        let bag = b"hp=100";
        assert_eq!(
            encode_versioned_live_value(0, bag).len() - encode_live_value(bag).len(),
            4
        );
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
        assert_eq!(decode_seedprog_key(&key), Some((emit_hash, grid, cell)));
        assert_eq!(decode_seedprog_key(&key[..20]), None);
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

    #[test]
    fn ramp_rows_are_the_registered_v_family_subspan() {
        let key = ramp_key("authority_correction");
        assert_eq!(key, b"vrauthority_correction");
        assert!(content_version_key().as_slice() < ramp_range_start().as_slice());
        assert!(ramp_range_start() <= key);
        assert!(key < ramp_range_end());
        assert!(ramp_range_end().as_slice() < [b'w'].as_slice());
    }

    #[test]
    fn cohort_rows_are_a_distinct_v_family_subspan() {
        // The extreme account id, because a maximal `u64` is the key a
        // too-short bound would let escape its own sub-span.
        let key = cohort_key(AccountId::new(u64::MAX));
        assert_eq!(key.len(), 10, "`vc` discriminator plus a big-endian u64");
        assert_eq!(&key[..2], b"vc");
        assert!(content_version_key().as_slice() < cohort_range_start().as_slice());
        assert!(cohort_range_start() <= key);
        assert!(key < cohort_range_end());
        assert!(cohort_range_end().as_slice() < [b'w'].as_slice());

        // Disjoint from the posture span by construction, and ordered so a
        // reader of one sub-span never observes the other's rows: the
        // failure ADR-0032's amendment records for `y` is exactly a
        // discriminator chosen without checking the accepted set.
        assert!(
            cohort_range_end() <= ramp_range_start(),
            "cohort rows sort strictly before every `vr` posture row"
        );
        let posture = ramp_key("attestation_quorum");
        assert!(posture >= ramp_range_start() && posture < ramp_range_end());
        assert!(key < posture, "even the extreme cohort key stays below");
    }

    #[test]
    fn ramp_window_rows_are_a_third_disjoint_v_family_subspan() {
        let key = ramp_window_key("attestation_quorum");
        assert_eq!(key, b"vmattestation_quorum");
        assert!(content_version_key().as_slice() < ramp_window_range_start().as_slice());
        assert!(ramp_window_range_start() <= key);
        assert!(key < ramp_window_range_end());
        assert!(ramp_window_range_end().as_slice() < [b'w'].as_slice());

        // The three `v` sub-spans in allocation order, each strictly below the
        // next, so a range read over any one of them never observes another's
        // rows. ADR-0032's amendment records the failure this asserts against:
        // a discriminator chosen without checking the accepted set.
        assert!(
            cohort_range_end() <= ramp_window_range_start(),
            "cohort rows sort strictly before every `vm` window row"
        );
        assert!(
            ramp_window_range_end() <= ramp_range_start(),
            "window rows sort strictly before every `vr` posture row"
        );
        assert!(key > cohort_key(AccountId::new(u64::MAX)));
        assert!(key < ramp_key("attestation_quorum"));

        // The empty control name is the shortest key the family admits, and it
        // must still land inside the span rather than on its boundary's far
        // side.
        assert!(ramp_window_range_start() <= ramp_window_key(""));
        assert!(ramp_window_key("") < ramp_window_range_end());
    }

    #[test]
    fn posture_history_rows_are_a_fourth_disjoint_v_family_subspan() {
        let key = posture_history_key("attestation_quorum");
        assert_eq!(&key[..2], b"vh");
        assert_eq!(&key[2..20], b"attestation_quorum");
        assert_eq!(key[20], 0x00, "the name is terminated, not merely ended");
        assert!(
            key[22..].iter().all(|byte| *byte == 0),
            "ten zero bytes of versionstamp placeholder"
        );
        assert!(content_version_key().as_slice() < posture_history_span_start().as_slice());
        assert!(posture_history_span_start() <= key);
        assert!(key < posture_history_span_end());
        assert!(posture_history_span_end().as_slice() < [b'w'].as_slice());

        // The four `v` sub-spans in allocation order, each strictly below the
        // next, so a range read over any one of them never observes another's
        // rows.
        assert!(
            cohort_range_end() <= posture_history_span_start(),
            "cohort rows sort strictly before every `vh` history row"
        );
        assert!(
            posture_history_span_end() <= ramp_window_range_start(),
            "history rows sort strictly before every `vm` window row"
        );
        assert!(key.as_slice() > cohort_range_end().as_slice());
        assert!(key.as_slice() < ramp_window_range_start().as_slice());
        assert!(key < ramp_key("attestation_quorum"));

        // The empty control name must still land inside the span.
        assert!(posture_history_span_start() <= posture_history_key(""));
        assert!(posture_history_key("") < posture_history_span_end());
    }

    #[test]
    fn the_posture_history_param_carries_the_placeholder_offset_it_names() {
        // "strikes" is the shortest control name in D32's inventory, so it is
        // the one whose offset a hardcoded constant would get wrong.
        for control in ["strikes", "attestation_quorum", "quarantine_validation"] {
            let param = posture_history_versionstamped_key(control);
            let key = posture_history_key(control);
            assert_eq!(
                param.len(),
                key.len() + 4,
                "the param is the key plus a little-endian u32 offset"
            );
            assert_eq!(&param[..key.len()], key.as_slice());
            let offset = u32::from_le_bytes(param[key.len()..].try_into().expect("4 bytes"));
            let placeholder_at = key.len() - 10;
            assert_eq!(
                offset as usize, placeholder_at,
                "the named offset is where the ten placeholder bytes begin"
            );
            assert!(
                key[placeholder_at..].iter().all(|byte| *byte == 0),
                "the placeholder is ten zero bytes, ready for substitution"
            );
        }
    }

    #[test]
    fn the_posture_history_scan_and_decode_survive_an_extending_control_name() {
        // The failure the `0x00` separator exists to prevent: a control whose
        // name extends another's ("strikes", "strikes2") would leak its rows
        // into the shorter name's scan if the scan ended at the name's bytes.
        assert!(
            posture_history_range_end("strikes") <= posture_history_range_start("strikes2"),
            "the scan ends at the separator, so no `strikes2` row sorts inside it"
        );

        // Round-trip a key the way FDB would leave it: placeholder replaced by
        // a real versionstamp, control and separator untouched.
        let mut substituted = posture_history_key("strikes");
        substituted[2 + "strikes".len() + 1..].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let (control, versionstamp) =
            decode_posture_history_key(&substituted).expect("a substituted key decodes");
        assert_eq!(control, "strikes");
        assert_eq!(versionstamp, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

        // And the strays a corrupted or foreign row could arrive as.
        assert_eq!(decode_posture_history_key(b"va"), None, "wrong span");
        assert_eq!(
            decode_posture_history_key(&substituted[..substituted.len() - 1]),
            None,
            "a short versionstamp is not a history key"
        );
        assert_eq!(
            decode_posture_history_key(&substituted[..substituted.len() - 11]),
            None,
            "no versionstamp at all is not a history key"
        );
        let mut nameless = posture_history_key("strikes");
        nameless[2..2 + "strikes".len()].fill(0x00);
        assert_eq!(
            decode_posture_history_key(&nameless),
            None,
            "an empty control name decodes to nothing"
        );
        assert_eq!(
            decode_posture_history_key(b"vh\xc3\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a"),
            None,
            "a control name that is not UTF-8 decodes to nothing"
        );
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
    fn provisional_key_is_9_bytes_with_r_prefix_and_big_endian_account() {
        let key = provisional_key(AccountId::new(0x0102_0304_0506_0708));
        assert_eq!(key.len(), 9);
        assert_eq!(key[0], b'r');
        assert_eq!(&key[1..], &0x0102_0304_0506_0708_u64.to_be_bytes());
        // Accounts sort by id, which is what makes the finalizer's sweep one
        // range read over a family that is empty in the steady state.
        assert!(provisional_key(AccountId::new(1)) < provisional_key(AccountId::new(2)));
    }

    #[test]
    fn provisional_range_spans_r_prefix_and_touches_no_neighbour() {
        let start = provisional_range_start();
        let end = provisional_range_end();
        assert_eq!(start, vec![b'r']);
        assert_eq!(end, vec![b's']);
        let key = provisional_key(AccountId::new(u64::MAX)).to_vec();
        assert!(key >= start && key < end);
        // `'s'` is the seedmap family and `'q'` is one past seed-progress:
        // adjacent and disjoint, which is the only property the bound needs.
        assert!(seedmap_key([0u8; 16]).to_vec() >= end);
    }

    #[test]
    fn the_gc_interlock_is_a_conjunction_and_not_a_deadline_alone() {
        // D29 clause 9(c). The deadline half is what docs/08 §6 already
        // promised; the finality half is what stops a *provisional* row from
        // vanishing under a replay and taking the idempotency answer with it.
        let row = |finality| IntentRow {
            outcome: orrery_protocol::IntentOutcome::Committed {
                tick: orrery_protocol::Tick::new(1),
                minted: Vec::new(),
            },
            gc_deadline_ms: 1_000,
            finality,
            finalize_by_ms: 0,
        };
        assert!(sweepable(&row(IntentFinality::Final), 1_000));
        assert!(sweepable(&row(IntentFinality::Annulled), 1_000));
        assert!(!sweepable(&row(IntentFinality::Final), 999));
        assert!(
            !sweepable(&row(IntentFinality::Provisional), u64::MAX),
            "unresolved is not sweepable at any clock"
        );
    }

    #[test]
    fn a_provisional_row_answers_which_balances_it_holds() {
        let hold = ProvisionalHold {
            intent_id: 7,
            account: AccountId::new(1),
            writes: vec![ProvisionalWrite {
                account: AccountId::new(1),
                asset: AssetId::new(3),
                delta: 100,
            }],
            committed_ms: 0,
            finalize_by_ms: 1,
            commitment: orrery_protocol::EvidenceCommitment {
                ruleset: orrery_protocol::RulesetId {
                    version: 1,
                    digest: [0; 32],
                },
                entity: PersistId::new(1),
                window_start: orrery_protocol::Tick::new(0),
                window_end: orrery_protocol::Tick::new(1),
                t0_claim_hash: [0; 32],
                log_head: orrery_protocol::ChainHash::EMPTY,
            },
            subject: iroh_base::SecretKey::from_bytes(&[3; 32]).public(),
        };
        let row = ProvisionalRow { holds: vec![hold] };
        assert_eq!(
            row.holds_balance(AccountId::new(1), AssetId::new(3)),
            Some(7)
        );
        assert_eq!(row.holds_balance(AccountId::new(1), AssetId::new(4)), None);
        assert_eq!(row.holds_balance(AccountId::new(2), AssetId::new(3)), None);
        // Round-trips, because the row is read on the intent path and a
        // decode failure there would refuse an intent for a storage reason.
        let bytes = postcard::to_stdvec(&row).expect("encode");
        let back: ProvisionalRow = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, row);
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

    // D51: v1 has no retired-terrain key layout to exercise here.

    // -----------------------------------------------------------------------
    // Intent-adjacent families: player, pid, ledger
    // -----------------------------------------------------------------------

    #[test]
    fn player_keys_share_the_account_span() {
        let account = AccountId::new(0x0102_0304_0506_0708);
        let profile = player_key(account);
        let loc = player_loc_key(account);
        assert_eq!(profile.len(), 9);
        assert_eq!(loc.len(), 10);
        assert_eq!(profile[0], b'u');
        // Hand-computed: 'u' ‖ big-endian account id.
        let mut expected = [0u8; 9];
        expected[0] = b'u';
        expected[1..9].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(profile, expected);
        // The loc row sorts inside the account's span, after the profile row.
        assert_eq!(loc[..9], profile[..]);
        assert_eq!(loc[9], 0x01);
        assert!(loc.as_slice() > profile.as_slice());
        // A different account is a different span.
        assert_ne!(player_key(AccountId::new(2)), profile);
    }

    #[test]
    fn pid_next_key_is_grid_scoped() {
        let root = pid_next_key(GridId::ROOT);
        let g9 = pid_next_key(GridId::new(9));
        assert_eq!(root.len(), 9);
        assert_eq!(root[0], b'n');
        assert_eq!(&root[5..9], b"next");
        // Hand-computed: 'n' ‖ big-endian grid ‖ "next".
        let mut expected = [0u8; 9];
        expected[0] = b'n';
        expected[5..9].copy_from_slice(b"next");
        assert_eq!(root, expected);
        assert_ne!(root, g9, "each grid allocates from its own counter");
        assert!(root.as_slice() < g9.as_slice(), "grids sort by id");
    }

    #[test]
    fn ledger_keys_share_one_prefix_and_discriminate() {
        let account = AccountId::new(7);
        let asset = AssetId::new(3);
        let item = ItemUid::new(0x0102_0304_0506_0708);

        let bal = ledger_bal_key(account, asset);
        assert_eq!(bal.len(), 18);
        assert_eq!(&bal[..2], b"lb");
        assert_eq!(
            u64::from_be_bytes(bal[2..10].try_into().unwrap()),
            7,
            "big-endian account id"
        );
        assert_eq!(
            u64::from_be_bytes(bal[10..18].try_into().unwrap()),
            3,
            "big-endian asset id"
        );

        let item_key = ledger_item_key(item);
        assert_eq!(item_key.len(), 10);
        assert_eq!(&item_key[..2], b"li");
        assert_eq!(
            u64::from_be_bytes(item_key[2..10].try_into().unwrap()),
            item.0
        );

        let receipt = ledger_receipt_key();
        assert_eq!(receipt.len(), 12);
        assert_eq!(&receipt[..2], b"lr");
        assert_eq!(
            receipt[2..],
            [0u8; 10],
            "the versionstamp placeholder is ten zero bytes"
        );

        // The `SetVersionstampedKey` parameter is the key plus the offset the
        // binding strips off, and the offset must point at the placeholder or
        // FDB overwrites the discriminator bytes instead.
        let param = ledger_receipt_versionstamped_key();
        assert_eq!(&param[..12], &receipt[..]);
        assert_eq!(
            u32::from_le_bytes(param[12..16].try_into().unwrap()),
            LEDGER_RECEIPT_VERSIONSTAMP_OFFSET
        );
        assert_eq!(
            &param[LEDGER_RECEIPT_VERSIONSTAMP_OFFSET as usize
                ..LEDGER_RECEIPT_VERSIONSTAMP_OFFSET as usize + 10],
            &[0u8; 10],
            "the offset names the ten zero bytes, not the 'lr' discriminator"
        );

        // All three share the ledger prefix but live in disjoint sub-spans:
        // 'b' < 'i' < 'r', so a balance range scan never touches item or
        // receipt rows.
        assert!(bal.as_slice() < item_key.as_slice());
        assert!(item_key.as_slice() < receipt.as_slice());
    }

    #[test]
    fn ledger_audit_cursor_subspan_sits_before_bal_and_outside_every_economic_row() {
        let cursor = ledger_audit_cursor_key();
        assert_eq!(cursor, [b'l', b'a'], "`la`, sub-discriminated under `l`");
        assert_eq!(
            ledger_audit_cursor_range_start(),
            cursor.to_vec(),
            "the one-row span starts at the key itself"
        );

        // The cursor sorts before every balance row and after nothing else in
        // the family: `la` < `lb` is what lets the sweep's family walk skip
        // auditor state with one range check instead of a key comparison.
        let bal_first = ledger_bal_key(AccountId::new(u64::MIN), AssetId::new(u64::MIN));
        assert!(cursor.as_slice() < bal_first.as_slice());
        assert_eq!(
            ledger_audit_cursor_range_end(),
            bal_first[..2].to_vec(),
            "the span ends where balances begin"
        );
        let bal_last = ledger_bal_key(AccountId::new(u64::MAX), AssetId::new(u64::MAX));
        assert!(ledger_audit_cursor_range_end().as_slice() <= bal_last.as_slice());

        // And it stays clear of the three economic sub-spans entirely.
        let item = ledger_item_key(ItemUid::new(u64::MIN));
        let receipt = ledger_receipt_key();
        for row in [item.as_slice(), receipt.as_slice()] {
            assert!(
                row < ledger_audit_cursor_range_start().as_slice()
                    || row >= ledger_audit_cursor_range_end().as_slice(),
                "no economic ledger row may sort into the audit-cursor sub-span"
            );
        }

        // The registered table carries the pair — asserted structurally by
        // `every_discriminated_constructor_is_registered_with_its_pair`; this
        // names the sample so the failure reads in this test's vocabulary.
        let families = registered_families();
        let ledger = families
            .iter()
            .find(|family| family.prefix == b'l')
            .expect("ledger family registered");
        let Kinds::SubKinds { table } = &ledger.kinds else {
            panic!("the ledger family is discriminated");
        };
        let first = table.first().expect("the ledger family declares sub-kinds");
        assert_eq!(
            (first.discriminator, first.name),
            (b'a', "ledger/audit sweep cursor"),
            "the cursor is the ledger family's first sub-kind, in discriminator order"
        );
    }

    #[test]
    fn l_family_shape_predicate_matches_registered_sub_discriminators() {
        let predicate: Vec<u8> = L_FAMILY_KEY_SHAPES
            .iter()
            .map(|(discriminator, _)| *discriminator)
            .collect();
        let registry = registered_families()
            .into_iter()
            .find(|family| family.prefix == b'l')
            .and_then(|family| match family.kinds {
                Kinds::SubKinds { table } => Some(table),
                Kinds::WholeSpan { .. } => None,
            })
            .expect("the l family has registered sub-kinds");
        let registry_discriminators: Vec<u8> =
            registry.iter().map(|kind| kind.discriminator).collect();
        let predicate_names: String = predicate.iter().map(|byte| char::from(*byte)).collect();
        let registry_names: String = registry_discriminators
            .iter()
            .map(|byte| char::from(*byte))
            .collect();

        assert_eq!(
            predicate,
            registry_discriminators,
            "the l-family shape predicate and registry disagree about the count or identity of \
             permitted byte-1 discriminators; the ambient FDB audit must accept every registered \
             writer and reject every unregistered one: predicate count {} ({predicate_names}), \
             registry count {} ({registry_names})",
            predicate_names.len(),
            registry_names.len(),
        );

        for kind in registry {
            assert!(
                is_registered_l_family_key(&kind.sample),
                "the l-family shape predicate rejects the registered `{}` sample ({} bytes): \
                 {:x?}",
                kind.name,
                kind.sample.len(),
                kind.sample,
            );
        }

        for (discriminator, _) in L_FAMILY_KEY_SHAPES {
            let mut old_shape = vec![0; 13];
            old_shape[0] = b'l';
            old_shape[1] = *discriminator;
            assert!(
                !is_registered_l_family_key(&old_shape),
                "a 13-byte old-shape registrar candidate with registered discriminator `{}` \
                 must still fail the ambient audit",
                char::from(*discriminator),
            );
        }
    }

    #[test]
    fn new_families_do_not_collide_with_w_c_a_or_i() {
        // The brief's disjointness requirement: the ledger/player/pid
        // prefixes must not collide with world ('w'), ckpt ('c'), actor
        // ('a') — or the already-landed intent ('i').
        let shard = CellId::from_bits(SHARD).unwrap();
        let existing = [
            world_key(GRID, shard, PersistId::new(1)).to_vec(),
            ckpt_key(GRID, shard).to_vec(),
            fence_key(GRID, shard).to_vec(),
            intent_key(42).to_vec(),
        ];
        let new = [
            ledger_bal_key(AccountId::new(1), AssetId::new(1)).to_vec(),
            ledger_item_key(ItemUid::new(1)).to_vec(),
            ledger_receipt_key().to_vec(),
            player_key(AccountId::new(1)).to_vec(),
            player_loc_key(AccountId::new(1)).to_vec(),
            pid_next_key(GridId::ROOT).to_vec(),
        ];
        for e in &existing {
            for n in &new {
                assert_ne!(
                    e[0], n[0],
                    "prefix byte collision between existing and new family"
                );
            }
        }
        // And among themselves the new first bytes are distinct from every
        // existing family byte: l, u, n.
        let mut bytes: Vec<u8> = new.iter().map(|k| k[0]).collect();
        bytes.extend(existing.iter().map(|k| k[0]));
        bytes.sort();
        bytes.dedup();
        assert_eq!(
            bytes.len(),
            4 + 3,
            "the seven first bytes are distinct (l, u, n against w, c, a, i)"
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
    // Identity family (D31): `da` accounts, `db` bindings, `dh` history
    // -----------------------------------------------------------------------

    /// A deterministic, valid [`NodeId`] from a one-byte discriminant.
    ///
    /// Derived from a secret key rather than written down, because a NodeId is
    /// a compressed ed25519 point and most 32-byte strings are not one — the
    /// same helper `orrery_protocol::identity`'s tests use. Its byte *order* is
    /// therefore not the order of `n`, which is why the span test below sorts.
    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn a_filing_notice_key_is_yd_account_and_round_trips() {
        let account = AccountId::new(0x0862_0000_dead_beef);
        let key = filing_notice_key(account);
        assert_eq!(&key[..2], b"yd", "the notice family is `yd`");
        assert_eq!(
            filing_notice_account(&key),
            Some(account),
            "a sweep must recover the account a notice names"
        );
    }

    /// The `yd` span must begin exactly where the `yc` span ends, or a
    /// restore-hold retention sweep would delete identity's pending work.
    #[test]
    fn the_filing_notice_span_does_not_overlap_the_restore_hold_span() {
        assert_eq!(
            restore_hold_family_range_end(),
            filing_notice_range_start(),
            "`yc`'s exclusive end is `yd`'s inclusive start"
        );
        let key = filing_notice_key(AccountId::new(u64::MAX)).to_vec();
        assert!(
            key >= filing_notice_range_start() && key < filing_notice_range_end(),
            "every notice key lies inside the family span"
        );
        assert!(
            key >= restore_hold_family_range_end(),
            "no notice key lies inside the restore-hold span"
        );
    }

    /// A foreign key handed to the decoder is skipped, not fatal: the sweep
    /// reads a range, and a range can contain something it did not write.
    #[test]
    fn a_foreign_key_is_not_read_as_a_filing_notice() {
        assert_eq!(filing_notice_account(&strike_key(AccountId::new(1))), None);
        assert_eq!(filing_notice_account(b"yd"), None);
    }

    #[test]
    fn id_keys_have_the_widths_and_discriminators_d31_specifies() {
        let account = account_key(AccountId::new(0x0102_0304_0506_0708));
        assert_eq!(account.len(), 10, "da ‖ account:u64 BE");
        assert_eq!(&account[..2], b"da");
        assert_eq!(
            u64::from_be_bytes(account[2..10].try_into().unwrap()),
            0x0102_0304_0506_0708,
            "account id is big-endian, so accounts sort by id"
        );

        let subject = node(0x11);
        let binding = binding_key(&subject);
        assert_eq!(binding.len(), 34, "db ‖ node:[u8;32]");
        assert_eq!(&binding[..2], b"db");
        assert_eq!(
            &binding[2..],
            subject.as_bytes(),
            "the key is the node id itself — no hashing, no truncation"
        );

        let history = binding_history_key(&subject);
        assert_eq!(
            history.len(),
            44,
            "dh ‖ node:[u8;32] ‖ versionstamp:[u8;10]"
        );
        assert_eq!(&history[..2], b"dh");
        assert_eq!(&history[2..34], subject.as_bytes());
        assert_eq!(
            &history[34..],
            &[0u8; 10],
            "the versionstamp is a zero placeholder until FDB substitutes it"
        );

        let ban = ban_entry_key(AccountId::new(0x0102_0304_0506_0708));
        assert_eq!(ban.len(), 10, "dn ‖ account:u64 BE (D33 (e))");
        assert_eq!(&ban[..2], b"dn");
        assert_eq!(
            u64::from_be_bytes(ban[2..10].try_into().unwrap()),
            0x0102_0304_0506_0708,
            "the ban row is account-keyed and shares `dc`'s ten-byte shape"
        );
        assert_ne!(
            ban,
            cooldown_entry_key(AccountId::new(0x0102_0304_0506_0708)),
            "a ban is a separate row from the cooldown entry, not the same one \
             re-stamped: the cooldown entry is released and the ban is not"
        );

        let window = binding_window_key(AccountId::new(0x0102_0304_0506_0708));
        assert_eq!(window.len(), 10, "dw ‖ account:u64 BE (D36 (a))");
        assert_eq!(&window[..2], b"dw");
        assert_eq!(
            u64::from_be_bytes(window[2..10].try_into().unwrap()),
            0x0102_0304_0506_0708,
            "the window is account-keyed — the rate cap's question is per-account"
        );
    }

    #[test]
    fn id_sub_spans_are_ordered_disjoint_and_inside_the_family() {
        let start = id_range_start();
        let end = id_range_end();
        assert_eq!(start, vec![b'd']);
        assert_eq!(end, vec![b'e'], "one past `d`; also the `epoch/` start");
        assert!(
            start.as_slice() < end.as_slice(),
            "an exclusive bound of [0x65] cannot include any key `e ‖ …`"
        );

        // `a < b < c < h < n < w` makes the scans disjoint by construction,
        // exactly as `lb < le < li < lr` does for the ledger.
        let spans = [
            (account_range_start(), account_range_end(), "da"),
            (binding_range_start(), binding_range_end(), "db"),
            (cooldown_range_start(), cooldown_range_end(), "dc"),
            (
                binding_history_range_start(),
                binding_history_range_end(),
                "dh",
            ),
            (ban_range_start(), ban_range_end(), "dn"),
            (
                binding_window_range_start(),
                binding_window_range_end(),
                "dw",
            ),
        ];
        for (lo, hi, name) in &spans {
            assert!(lo.as_slice() < hi.as_slice(), "{name} span is non-empty");
            assert!(
                start.as_slice() <= lo.as_slice() && hi.as_slice() <= end.as_slice(),
                "{name} sits inside the `id/` family span"
            );
        }
        for (i, (_, hi, a)) in spans.iter().enumerate() {
            for (lo, _, b) in spans.iter().skip(i + 1) {
                assert!(
                    hi.as_slice() <= lo.as_slice(),
                    "{a} must end at or before {b} begins"
                );
            }
        }

        // And the concrete keys land where the spans say they do — with the
        // extreme account id, because `da` is the sub-space a maximal key could
        // push out of its own bound.
        let account = account_key(AccountId::new(u64::MAX)).to_vec();
        let binding = binding_key(&node(0xEE)).to_vec();
        let cooldown = cooldown_entry_key(AccountId::new(u64::MAX)).to_vec();
        let history = binding_history_key(&node(0x00)).to_vec();
        let ban = ban_entry_key(AccountId::new(u64::MAX)).to_vec();
        let window = binding_window_key(AccountId::new(u64::MAX)).to_vec();
        assert!(account_range_start() <= account && account < account_range_end());
        assert!(binding_range_start() <= binding && binding < binding_range_end());
        assert!(cooldown_range_start() <= cooldown && cooldown < cooldown_range_end());
        assert!(binding_history_range_start() <= history && history < binding_history_range_end());
        assert!(ban_range_start() <= ban && ban < ban_range_end());
        assert!(binding_window_range_start() <= window && window < binding_window_range_end());
        assert!(
            account < binding
                && binding < cooldown
                && cooldown < history
                && history < ban
                && ban < window,
            "da < db < dc < dh < dn < dw — the window sorts above every history \
             key, so the `dh` retention sweep over [dh, di) cannot reach it \
             (D36 (c)), and the ban span sits between the two touching neither"
        );
    }

    #[test]
    fn binding_history_node_span_brackets_exactly_that_node() {
        // Sorted by key bytes, not by discriminant: a NodeId's byte order is
        // whatever the curve gives it.
        let mut nodes = [node(1), node(2), node(3)];
        nodes.sort_by_key(|n| *n.as_bytes());
        let [lower, subject, upper] = nodes;

        let lo = binding_history_node_range_start(&subject);
        let hi = binding_history_node_range_end(&subject);

        assert_eq!(lo.len(), 34);
        assert_eq!(hi.len(), 34, "the end is the next node's 34-byte prefix");
        assert!(lo.as_slice() < hi.as_slice());

        // Every 44-byte key for this node, at either extreme of the
        // versionstamp space, is inside the span.
        let mut oldest = binding_history_key(&subject).to_vec();
        let mut newest = binding_history_key(&subject).to_vec();
        newest[34..].copy_from_slice(&[0xFF; 10]);
        for key in [&mut oldest, &mut newest] {
            assert!(
                lo.as_slice() <= key.as_slice() && key.as_slice() < hi.as_slice(),
                "a versionstamped row for this node is inside its own span"
            );
        }

        // Neighbouring nodes are outside it, on both sides.
        let below = binding_history_key(&lower).to_vec();
        let above = binding_history_key(&upper).to_vec();
        assert!(below.as_slice() < lo.as_slice());
        assert!(above.as_slice() >= hi.as_slice());

        // And the whole span stays inside the `dh` sub-space.
        assert!(binding_history_range_start() <= lo);
        assert!(hi <= binding_history_range_end());
    }

    #[test]
    fn next_node_bytes_carries_and_saturates() {
        assert_eq!(next_node_bytes(&[0x11; 32]).unwrap()[31], 0x12);
        assert_eq!(&next_node_bytes(&[0x11; 32]).unwrap()[..31], &[0x11; 31]);

        let mut trailing_ff = [0x00; 32];
        trailing_ff[30] = 0x07;
        trailing_ff[31] = 0xFF;
        let carried = next_node_bytes(&trailing_ff).unwrap();
        assert_eq!(carried[30], 0x08, "the carry propagates left");
        assert_eq!(carried[31], 0x00);

        assert!(
            next_node_bytes(&[0xFF; 32]).is_none(),
            "no successor at the top of the node space; the caller falls back \
             to the sub-space end"
        );
    }

    #[test]
    fn binding_history_versionstamp_parameter_names_its_own_placeholder() {
        let subject = node(0x11);
        let param = binding_history_versionstamped_key(&subject);
        assert_eq!(param.len(), 48, "44-byte key ‖ 4-byte LE offset");
        assert_eq!(&param[..44], binding_history_key(&subject).as_slice());

        let offset = u32::from_le_bytes(param[44..48].try_into().unwrap());
        assert_eq!(offset, BINDING_HISTORY_VERSIONSTAMP_OFFSET);
        assert_eq!(offset, 34, "immediately after `dh` and the 32 node bytes");
        assert_eq!(
            &param[offset as usize..44],
            &[0u8; 10],
            "the offset must name the zero placeholder, not the node id"
        );
    }

    #[test]
    fn binding_history_key_round_trips_through_its_decoder() {
        let subject = node(0x11);
        let mut key = binding_history_key(&subject).to_vec();
        let stamp = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x00, 0x09];
        key[34..].copy_from_slice(&stamp);

        let (decoded, versionstamp) =
            decode_binding_history_key(&key).expect("a well-formed dh key decodes");
        assert_eq!(decoded, subject);
        assert_eq!(versionstamp, stamp);

        // Everything that is not a `dh` key is refused rather than
        // misinterpreted: wrong family, wrong sub-space, wrong width, and a
        // node id that is not a curve point.
        assert!(decode_binding_history_key(&[]).is_none());
        assert!(decode_binding_history_key(&key[..43]).is_none());
        assert!(decode_binding_history_key(&binding_key(&subject)).is_none());
        assert!(decode_binding_history_key(&account_key(AccountId::new(1))).is_none());
        let mut wrong_family = key.clone();
        wrong_family[0] = b'e';
        assert!(decode_binding_history_key(&wrong_family).is_none());
        let mut not_a_point = key.clone();
        // `[0x02; 32]` does not decompress to a curve point. Checked rather
        // than assumed: roughly half of all 32-byte strings do not, and a
        // decoder that swallowed one would hand a caller a node that cannot
        // exist.
        not_a_point[2..34].copy_from_slice(&[0x02; 32]);
        assert!(NodeId::from_bytes(&[0x02; 32]).is_err());
        assert!(
            decode_binding_history_key(&not_a_point).is_none(),
            "a NodeId is a curve point, so not every 32-byte string is one"
        );
    }

    #[test]
    fn id_rows_round_trip_through_postcard() {
        let row = AccountRow {
            bound_nodes: (0..MAX_BOUND_NODES_PER_ACCOUNT)
                .map(|n| node(u8::try_from(n).unwrap() + 0x11))
                .collect(),
            binding_epoch: 7,
            created_ms: 1_700_000_000_000,
            binding_event_count: 3,
            first_event_ms: 1_600_000_000_000,
        };
        let bytes = postcard::to_allocvec(&row).expect("encode");
        assert_eq!(postcard::from_bytes::<AccountRow>(&bytes).unwrap(), row);
        // D31 (c) prices the row at ~282 B with eight NodeIds inline; the
        // budget that matters is that it stays a bounded, readable-whole row.
        assert!(
            bytes.len() < 512,
            "an account row is {} B; D31 (c) budgets ~282 B and \
             MAX_BOUND_NODES_PER_ACCOUNT is what bounds it",
            bytes.len()
        );

        let binding = BindingRow {
            account: AccountId::new(42),
            bound_at_ms: 1_700_000_000_000,
        };
        let bytes = postcard::to_allocvec(&binding).expect("encode");
        assert_eq!(postcard::from_bytes::<BindingRow>(&bytes).unwrap(), binding);

        for kind in [BindKind::Bind, BindKind::Unbind] {
            let event = BindingHistoryRow {
                account: AccountId::new(42),
                kind,
                at_ms: 1_700_000_000_000,
            };
            let bytes = postcard::to_allocvec(&event).expect("encode");
            assert_eq!(
                postcard::from_bytes::<BindingHistoryRow>(&bytes).unwrap(),
                event
            );
        }
    }

    #[test]
    fn account_keys_are_adjacent_and_ordered_for_adjacent_account_ids() {
        // The property the big-endian encoding exists for: `da` is one
        // contiguous, id-ordered range, so a scan of the account sub-space
        // walks accounts in id order and an id-bounded sub-scan is a range.
        let keys: Vec<[u8; 10]> = (0u64..4).map(|n| account_key(AccountId::new(n))).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "account keys sort by account id");
            let mut expected = pair[0];
            expected[9] += 1;
            assert_eq!(
                pair[1], expected,
                "adjacent account ids are adjacent keys — nothing can sort \
                 between them"
            );
        }

        // The carry crosses byte boundaries the way big-endian promises.
        assert!(account_key(AccountId::new(0xFF)) < account_key(AccountId::new(0x100)));
        assert!(account_key(AccountId::new(u64::MAX - 1)) < account_key(AccountId::new(u64::MAX)));
    }

    #[test]
    fn id_range_helpers_span_the_family_and_nothing_beyond_it() {
        let start = id_range_start();
        let end = id_range_end();

        // Every key the family can produce is inside the span, at both
        // extremes of every sub-space.
        let mut inside = vec![
            account_key(AccountId::new(0)).to_vec(),
            account_key(AccountId::new(u64::MAX)).to_vec(),
            binding_key(&node(1)).to_vec(),
            binding_history_key(&node(1)).to_vec(),
        ];
        let mut newest = binding_history_key(&node(1)).to_vec();
        newest[34..].copy_from_slice(&[0xFF; 10]);
        inside.push(newest);
        for key in &inside {
            assert!(
                start.as_slice() <= key.as_slice() && key.as_slice() < end.as_slice(),
                "an `id/` key must be inside [{start:?}, {end:?})"
            );
        }

        // And no neighbouring family's key is. `c` is `ckpt/` and `e` is
        // `epoch/`: the span must touch neither.
        let shard = CellId::from_bits(SHARD).unwrap();
        let outside = [
            ckpt_key(GRID, shard).to_vec(),
            epoch_key(GRID, shard, 0).to_vec(),
            epoch_handle_key(0).to_vec(),
        ];
        for key in &outside {
            assert!(
                key.as_slice() < start.as_slice() || end.as_slice() <= key.as_slice(),
                "a neighbouring family's key must be outside the `id/` span"
            );
        }
    }

    #[test]
    fn an_account_with_no_bound_nodes_encodes_and_decodes() {
        // An account exists before its first device binds — identity mints the
        // `da` row at account creation, and D31 (f) then excludes every NodeId
        // it cannot resolve, which is all of them until a bind happens.
        let fresh = AccountRow {
            created_ms: 1_700_000_000_000,
            ..AccountRow::default()
        };
        assert!(fresh.bound_nodes.is_empty());
        let bytes = postcard::to_allocvec(&fresh).expect("encode");
        assert_eq!(postcard::from_bytes::<AccountRow>(&bytes).unwrap(), fresh);
        assert_eq!(fresh.binding_event_count, 0);
        assert_eq!(fresh.first_event_ms, 0, "no first event yet");
    }

    #[test]
    fn bound_node_order_is_preserved_but_is_not_load_bearing() {
        // Two bindings, because one proves nothing about order.
        let (first, second) = (node(1), node(2));
        let row = AccountRow {
            bound_nodes: vec![first, second],
            ..AccountRow::default()
        };
        let reversed = AccountRow {
            bound_nodes: vec![second, first],
            ..row.clone()
        };

        // The encoding is faithful: a round-trip returns the vector it was
        // given, in the order it was given.
        for original in [&row, &reversed] {
            let bytes = postcard::to_allocvec(original).expect("encode");
            assert_eq!(
                &postcard::from_bytes::<AccountRow>(&bytes).unwrap(),
                original
            );
        }

        // But order carries no meaning, and this is the half worth pinning.
        // D31 makes `bound_nodes` a *set* — the questions asked of it are
        // "is this node bound" and "how many are bound", and the reverse
        // direction, which is the only one any consumer reads, goes through
        // `db` and never through this vector at all.
        //
        // Contrast [`AttestRow::eligible`], where order *is* load-bearing and
        // says so: the audit recomputes the draw over that exact vector, so a
        // normalized copy would no longer be the object the gateway drew over.
        // Nothing recomputes anything over `bound_nodes`.
        let as_set = |r: &AccountRow| {
            r.bound_nodes
                .iter()
                .map(|n| *n.as_bytes())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_ne!(row.bound_nodes, reversed.bound_nodes);
        assert_eq!(
            as_set(&row),
            as_set(&reversed),
            "the two rows bind the same devices; only the order differs"
        );
        for node in &reversed.bound_nodes {
            assert!(
                row.bound_nodes.contains(node),
                "membership, not position, is what a reader of this field asks"
            );
        }
    }

    #[test]
    fn binding_history_retention_is_ninety_days() {
        // D31 resolved question 2. The value is load-bearing twice over: it is
        // what makes the history outlive every dispute a strike could raise
        // (D16's 14-day half-life leaves ~1.2 % of a strike's weight at 90 d),
        // and it is what bounds the adversarial storage term in D31 (c).
        assert_eq!(BINDING_HISTORY_RETENTION_MS, 7_776_000_000);
        assert_eq!(BINDING_HISTORY_RETENTION_MS / (24 * 60 * 60 * 1000), 90);
    }

    // -----------------------------------------------------------------------
    // Pairwise disjointness: every key family's range is non-overlapping
    // -----------------------------------------------------------------------

    /// One declared sub-kind of a discriminated family (D35 clause (c)).
    ///
    /// `discriminator` is byte 1 of every key of this kind — an ASCII literal
    /// the kind's constructor writes immediately after the family byte — and
    /// `sample` is drawn from that same constructor, so a constructor that
    /// moves its bytes moves the table with it instead of being contradicted.
    struct SubKind {
        discriminator: u8,
        name: &'static str,
        sample: Vec<u8>,
    }

    /// What one family byte holds: the whole span, or an ordered sub-kind
    /// table.
    ///
    /// The marker is not decoration. A whole-span family asserts there is no
    /// second kind inside it — which is exactly why `m` (`lease-cell/`) and
    /// `o` (`lease-location/`) are *not* defects although they put a grid id's
    /// high byte at byte 1: a collision needs cohabitation without
    /// discrimination, and those families have nothing inside to collide
    /// with (D35 Context §3).
    enum Kinds {
        /// The family owns `[prefix, prefix+1)` entire.
        WholeSpan {
            /// A key drawn from the family's own constructor.
            sample: Vec<u8>,
        },
        /// ASCII-discriminated kinds at byte 1, kept in discriminator order.
        ///
        /// Declared sub-spans must be pairwise disjoint; the guard proves it
        /// from distinct discriminators the way the between-family proof is
        /// proven from distinct prefixes.
        SubKinds {
            /// One row per kind, in ascending [`SubKind::discriminator`]
            /// order.
            table: Vec<SubKind>,
        },
    }

    /// One registered key family: its one-byte prefix, the name it goes by,
    /// and either a whole-span marker or its sub-kind table.
    ///
    /// **One table, not two.** Until D31 this registry was two arrays — the
    /// prefixes in one, the sample keys in another — correlated by nothing but
    /// position, and a family added to one and not the other still passed. A
    /// family is one row here and cannot be half-registered.
    ///
    /// Since D35 clause (c) the row also models what sits *inside* the byte:
    /// a family is either whole-span or declares every `(byte, discriminator)`
    /// pair it writes, so a constructor that shares a registered byte cannot
    /// exist without declaring its sub-span — the class of defect
    /// [`every_discriminated_constructor_is_registered_with_its_pair`] closes,
    /// of which pre-D35's `lease_key` was the only host.
    struct Family {
        prefix: u8,
        name: &'static str,
        kinds: Kinds,
    }

    fn registered_families() -> Vec<Family> {
        let shard = CellId::from_bits(SHARD).unwrap();
        vec![
            Family {
                prefix: b'a',
                name: "fence/actor",
                kinds: Kinds::WholeSpan {
                    sample: fence_key(GRID, shard).to_vec(),
                },
            },
            Family {
                prefix: b'c',
                name: "ckpt",
                kinds: Kinds::WholeSpan {
                    sample: ckpt_key(GRID, shard).to_vec(),
                },
            },
            Family {
                prefix: b'd',
                name: "id",
                kinds: Kinds::SubKinds {
                    table: vec![
                        SubKind {
                            discriminator: b'a',
                            name: "id/da accounts",
                            sample: account_key(AccountId::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'b',
                            name: "id/db bindings",
                            sample: binding_key(&node(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'c',
                            name: "id/dc cooldown entries",
                            sample: cooldown_entry_key(AccountId::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'h',
                            name: "id/dh binding-history",
                            sample: binding_history_key(&node(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'n',
                            name: "id/dn terminal bans",
                            sample: ban_entry_key(AccountId::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'w',
                            name: "id/dw binding-rate window",
                            sample: binding_window_key(AccountId::new(1)).to_vec(),
                        },
                    ],
                },
            },
            Family {
                prefix: b'e',
                name: "epoch",
                kinds: Kinds::WholeSpan {
                    sample: epoch_key(GRID, shard, 3).to_vec(),
                },
            },
            Family {
                prefix: b'f',
                name: "epoch-handle",
                kinds: Kinds::WholeSpan {
                    sample: epoch_handle_key(1 << 48).to_vec(),
                },
            },
            Family {
                prefix: b'g',
                name: "attest",
                kinds: Kinds::WholeSpan {
                    sample: attest_key(42).to_vec(),
                },
            },
            Family {
                prefix: b'i',
                name: "intent",
                kinds: Kinds::WholeSpan {
                    sample: intent_key(42).to_vec(),
                },
            },
            // D51 deliberately leaves the next prefix byte clean; do not
            // reintroduce a registry row without its own allocation decision.
            Family {
                prefix: b'l',
                name: "ledger",
                kinds: Kinds::SubKinds {
                    table: vec![
                        SubKind {
                            discriminator: b'a',
                            name: "ledger/audit sweep cursor",
                            sample: ledger_audit_cursor_key().to_vec(),
                        },
                        SubKind {
                            discriminator: b'b',
                            name: "ledger/bal balances",
                            sample: ledger_bal_key(AccountId::new(1), AssetId::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'e',
                            name: "lease registrar rows",
                            sample: lease_key(GridId::ROOT, PersistId::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'i',
                            name: "ledger/item items",
                            sample: ledger_item_key(ItemUid::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'r',
                            name: "ledger/receipt receipts",
                            sample: ledger_receipt_key().to_vec(),
                        },
                    ],
                },
            },
            Family {
                prefix: b'm',
                name: "lease-cell",
                kinds: Kinds::WholeSpan {
                    sample: lease_cell_key(GRID, shard, PersistId::new(1)).to_vec(),
                },
            },
            Family {
                prefix: b'n',
                name: "pid/next",
                kinds: Kinds::WholeSpan {
                    sample: pid_next_key(GridId::ROOT).to_vec(),
                },
            },
            Family {
                prefix: b'o',
                name: "lease-location",
                kinds: Kinds::WholeSpan {
                    sample: lease_location_key(GRID, PersistId::new(1)).to_vec(),
                },
            },
            Family {
                prefix: b'p',
                name: "seedprog",
                kinds: Kinds::WholeSpan {
                    sample: seedprog_key([0xDE; 8], GRID, shard).to_vec(),
                },
            },
            Family {
                prefix: b'r',
                name: "provisional",
                kinds: Kinds::WholeSpan {
                    sample: provisional_key(AccountId::new(1)).to_vec(),
                },
            },
            Family {
                prefix: b's',
                name: "seedmap",
                kinds: Kinds::WholeSpan {
                    sample: seedmap_key([0xAB; 16]).to_vec(),
                },
            },
            Family {
                prefix: b'u',
                name: "player",
                kinds: Kinds::WholeSpan {
                    sample: player_key(AccountId::new(1)).to_vec(),
                },
            },
            Family {
                prefix: b'v',
                name: "content/version",
                kinds: Kinds::WholeSpan {
                    sample: content_version_key().to_vec(),
                },
            },
            Family {
                prefix: b'w',
                name: "world",
                kinds: Kinds::WholeSpan {
                    sample: world_key(GRID, shard, PersistId::new(1)).to_vec(),
                },
            },
            Family {
                prefix: b'y',
                name: "strike",
                kinds: Kinds::SubKinds {
                    table: vec![
                        SubKind {
                            discriminator: b'a',
                            name: "strike/ya account facts",
                            sample: strike_key(AccountId::new(1)).to_vec(),
                        },
                        SubKind {
                            discriminator: b'b',
                            name: "strike/yb episode dedup",
                            sample: strike_episode_key(AccountId::new(1), &[0; 32]).to_vec(),
                        },
                        SubKind {
                            discriminator: b'c',
                            name: "strike/yc restore holds",
                            sample: restore_hold_annulment_key(&node(1), PersistId::new(1), 1),
                        },
                        SubKind {
                            discriminator: FILING_NOTICE_DISCRIMINATOR,
                            name: "strike/yd filing notices",
                            sample: filing_notice_key(AccountId::new(1)).to_vec(),
                        },
                    ],
                },
            },
            Family {
                prefix: b'z',
                name: "archive metadata",
                kinds: Kinds::SubKinds {
                    table: vec![
                        SubKind {
                            discriminator: b'a',
                            name: "jarchive/za journal metadata",
                            sample: jarchive_key(&node(1), 0).to_vec(),
                        },
                        SubKind {
                            discriminator: b'r',
                            name: "rarchive/zr receipt metadata",
                            sample: rarchive_key(&ledger_receipt_key())
                                .expect("receipt key is an lr cursor")
                                .to_vec(),
                        },
                    ],
                },
            },
        ]
    }

    impl Family {
        /// Every `(declared leading bytes, label, sample key)` triple this
        /// row owns: one for a whole-span family, one per sub-kind otherwise.
        ///
        /// The guards walk these rather than reaching into [`Kinds`], so a
        /// restructure of the enum cannot quietly drop a sample from proof.
        /// Each triple's `declared` is the span the row claims in the order
        /// space — `[prefix]` or `[prefix, discriminator]` — and the disjoint-
        /// ness tests check the sample actually begins with it.
        fn proven_spans(&self) -> Vec<(Vec<u8>, String, Vec<u8>)> {
            match &self.kinds {
                Kinds::WholeSpan { sample } => {
                    vec![(vec![self.prefix], self.name.to_owned(), sample.clone())]
                }
                Kinds::SubKinds { table } => table
                    .iter()
                    .map(|kind| {
                        (
                            vec![self.prefix, kind.discriminator],
                            format!("{}/{}", self.name, kind.name),
                            kind.sample.clone(),
                        )
                    })
                    .collect(),
            }
        }
    }

    /// Families are disjoint iff for every pair (a, b):
    ///   max_key(a) < min_key(b)  or  max_key(b) < min_key(a)
    /// For fixed-byte-prefix families (all of ours), min_key = [prefix] and
    /// max_key sorts before [prefix+1], so it is enough that the prefix bytes
    /// are all different. Since D35 clause (c) a discriminated family's span
    /// is still `[prefix, prefix+1)` — its sub-spans live inside that byte —
    /// so the between-family proof walks every sub-kind sample unchanged.
    ///
    /// Each sample must actually begin with its declared leading bytes —
    /// `[prefix]`, or `[prefix, discriminator]` for a sub-kind — or everything
    /// below is a property of the table rather than of the keyspace.
    ///
    /// This test provides an explicit concrete assertion for each pair so a
    /// future addition that reuses a prefix is caught with the offending pair
    /// named. Completeness of the table — that *every* family in the module is
    /// in it — is the separate obligation of
    /// [`every_family_prefix_written_in_this_module_is_registered`], because a
    /// disjointness proof over a subset proves nothing about the rest.
    #[test]
    fn all_key_families_are_range_disjoint() {
        let families = registered_families();

        for family in &families {
            for (declared, label, sample) in family.proven_spans() {
                assert_eq!(
                    &sample[..declared.len()],
                    declared.as_slice(),
                    "{label} sample key must begin with its declared prefix \
                     bytes {declared:02x?}"
                );
            }
        }

        // All prefix bytes must be distinct, and the collision is named rather
        // than reported as a count: "18 != 19" sends a reader counting rows.
        let mut seen: std::collections::BTreeMap<u8, &'static str> =
            std::collections::BTreeMap::new();
        for family in &families {
            if let Some(other) = seen.insert(family.prefix, family.name) {
                panic!(
                    "family prefix 0x{:02x} ('{}') is claimed by both {} and {}",
                    family.prefix,
                    char::from(family.prefix),
                    other,
                    family.name
                );
            }
        }

        // For each pair (a, b), verify that every key family A can produce
        // sorts before every key family B can produce when prefix_a <
        // prefix_b. Each sample begins with its family's prefix byte (checked
        // above), so distinct prefixes guarantee disjoint ranges whatever the
        // bytes behind them hold.
        for (i, a) in families.iter().enumerate() {
            for b in families.iter().skip(i + 1) {
                for (_, a_label, a_sample) in a.proven_spans() {
                    for (_, b_label, b_sample) in b.proven_spans() {
                        assert!(
                            a_sample.as_slice() < b_sample.as_slice(),
                            "{} (0x{:02x}) must sort before {} (0x{:02x}); \
                             one of them is in the wrong place or they collide",
                            a_label,
                            a.prefix,
                            b_label,
                            b.prefix
                        );
                    }
                }
            }
        }
    }

    /// The within-family half of D35 clause (c): declared sub-spans are
    /// pairwise disjoint, ordered by discriminator.
    ///
    /// Between families this property comes free from distinct prefix bytes;
    /// inside one byte it is exactly what the old byte-only guard could not
    /// see. Distinct discriminators imply disjoint spans the same way — min
    /// key = `[prefix, disc]`, which sorts before `[prefix, disc+1]` — but
    /// the assertion below spells out the concrete ordering anyway, with the
    /// offenders named, because a proof over samples nobody checked is where
    /// this defect class lived.
    #[test]
    fn declared_sub_kinds_are_pairwise_disjoint_inside_each_family() {
        for family in registered_families() {
            let Kinds::SubKinds { table } = &family.kinds else {
                continue;
            };

            // Ascending discriminators: the table is kept in sort order, as
            // `registered_families` itself is, so a row added out of order is
            // caught here rather than silently re-sorted by a reader.
            for pair in table.windows(2) {
                let [lower, higher] = pair else {
                    unreachable!("windows(2) yields slices of two")
                };
                assert!(
                    lower.discriminator < higher.discriminator,
                    "family 'c{}' keeps its sub-kinds in discriminator order; \
                     '{}' (0x{:02x}) is listed after '{}' (0x{:02x})",
                    char::from(family.prefix),
                    higher.name,
                    higher.discriminator,
                    lower.name,
                    lower.discriminator
                );
            }

            // And the concrete keys land in the declared order: lb < le < li
            // < lr, da < db < dh. A sample that does not begin with its own
            // declared pair is caught above; one that begins with it but sorts
            // against declaration would mean the discriminator is not actually
            // at byte 1 of the constructor's output.
            let named: Vec<(String, Vec<u8>)> = table
                .iter()
                .map(|kind| {
                    (
                        format!("{} {}", family.name, kind.name),
                        kind.sample.clone(),
                    )
                })
                .collect();
            for (i, (_, a_sample)) in named.iter().enumerate() {
                for (b_label, b_sample) in named.iter().skip(i + 1) {
                    assert!(
                        a_sample.as_slice() < b_sample.as_slice(),
                        "{b_label} must sort after the sub-kind before it inside \
                         family '{}': the declared sub-spans overlap",
                        char::from(family.prefix)
                    );
                }
            }
        }
    }

    /// The text of the key-writing modules, read back so the completeness
    /// clause below has a second source. A clause that read both sides out of
    /// [`registered_families`] would pass on exactly the family nobody
    /// registered.
    const KEYSPACE_SOURCE: &str = include_str!("keyspace.rs");
    const ADJUDICATION_SOURCE: &str = include_str!("adjudication.rs");

    /// Every one-byte family prefix a persistd key constructor writes.
    ///
    /// Three recognized forms, and each is unambiguous by construction:
    ///
    /// ```text
    ///     key[0] = b'x'      the fixed-size array constructors
    ///     key.push(b'x')     the `Vec` range-bound builders
    ///     \n    [b'x']       a single-byte key returned as a bare array
    ///     key[..2].copy_from_slice(b"xy")  a two-byte family/discriminator
    /// ```
    ///
    /// `key[1] = b'x'` is deliberately **not** one of them: the second byte is
    /// a sub-discriminator *inside* a family (`lb`/`li`/`lr`, `da`/`db`/`dh`),
    /// not a family. Since D35 clause (c) those literals have their own scan —
    /// [`discriminated_pairs_written_in_this_module`] — which pairs them into
    /// the registry's sub-kind tables; this scan stays byte-level so the two
    /// sources of truth stay separate. Neither is `vec![b'x']`, which is how
    /// range *bounds* are written — an exclusive end is one past a family, not
    /// a family, which is why `b`, `h`, `j`, `q`, `t` and `x` are spoken for
    /// without being families.
    ///
    /// The test half of the file is excluded, because a test may build a
    /// deliberately colliding key and that must not register a family.
    fn scan_family_prefixes(source: &str) -> std::collections::BTreeSet<u8> {
        let bytes = source.as_bytes();

        let mut found = std::collections::BTreeSet::new();
        for (at, _) in source.match_indices("b'") {
            // `b'x'` is four bytes; anything else is not a byte literal.
            if bytes.len() < at + 4 || bytes[at + 3] != b'\'' {
                continue;
            }
            let before = &source[..at];
            let writes_a_family_prefix = before.ends_with("key[0] = ")
                || before.ends_with("key.push(")
                || before.ends_with("\n    [");
            if writes_a_family_prefix {
                found.insert(bytes[at + 2]);
            }
        }
        for (at, _) in source.match_indices("key[..2].copy_from_slice(b\"") {
            let family_at = at + "key[..2].copy_from_slice(b\"".len();
            if let Some(prefix) = bytes.get(family_at) {
                found.insert(*prefix);
            }
        }
        found
    }

    fn family_prefixes_written_by_persistd() -> std::collections::BTreeSet<u8> {
        let mut found = std::collections::BTreeSet::new();
        for source in [KEYSPACE_SOURCE, ADJUDICATION_SOURCE] {
            let production = source
                .split_once("\n#[cfg(test)]\n")
                .map_or(source, |(head, _)| head);
            found.extend(scan_family_prefixes(production));
        }
        found
    }

    /// Every `(family byte, discriminator byte)` pair a discriminated
    /// constructor in this module writes.
    ///
    /// D35 clause (c)'s extension of [`family_prefixes_written_in_this_module`]
    /// from bytes to byte *pairs*. It recognizes `key[1] = b'…'` literal writes
    /// in the non-test half and pairs each with the nearest preceding
    /// `key[0] = b'…'` literal — a heuristic that works because every
    /// discriminated constructor here assigns byte 0 immediately before byte 1,
    /// true of all seven sites today. A constructor that computes byte 1
    /// without a literal (e.g. `copy_from_slice`) remains invisible, exactly as
    /// this module's byte-0 scan cannot see one that computes its prefix;
    /// should such a site ever land it must go through a named form the scanner
    /// recognizes, and the floor assertion in
    /// [`every_discriminated_constructor_is_registered_with_its_pair`] is what
    /// turns any drift of that decision into a failure rather than a silent
    /// pass.
    fn discriminated_pairs_written_in_this_module() -> std::collections::BTreeSet<(u8, u8)> {
        let source = KEYSPACE_SOURCE
            .split_once("\n#[cfg(test)]\n")
            .map_or(KEYSPACE_SOURCE, |(head, _)| head);
        let bytes = source.as_bytes();

        let mut pairs = std::collections::BTreeSet::new();
        let mut last_byte0_write: Option<u8> = None;
        for (at, _) in source.match_indices("b'") {
            // `b'x'` is four bytes; anything else is not a byte literal.
            if bytes.len() < at + 4 || bytes[at + 3] != b'\'' {
                continue;
            }
            let before = &source[..at];
            if before.ends_with("key[0] = ") {
                last_byte0_write = Some(bytes[at + 2]);
            } else if before.ends_with("key[1] = ") {
                if let Some(family) = last_byte0_write {
                    pairs.insert((family, bytes[at + 2]));
                }
            }
        }
        pairs
    }

    /// The completeness half of the disjointness proof: the registry must name
    /// **every** family the module actually writes.
    ///
    /// D31 clause (a) makes this a condition rather than a nicety. Before it,
    /// the table held fourteen families while seventeen prefix bytes were live
    /// — `m` (`lease-cell/`), `o` (`lease-location/`) and `r` (D29's
    /// `provisional/`) had landed without ever being registered — so the guard
    /// proved fourteen of seventeen bytes distinct and would not have noticed a
    /// new family colliding with any of the three. Registering `d` in a guard
    /// that is itself incomplete is half a check.
    ///
    /// The two sides are the *text of the module* and the *typed registry*,
    /// which is what makes this a check and not a restatement: adding a family
    /// and forgetting the table now fails here, by name.
    ///
    /// **What it does not catch**, said out loud so it is not over-trusted: the
    /// scan yields a set of *bytes*, so a second family that reuses an
    /// already-registered byte without an ASCII sub-discriminator is invisible
    /// to it **by construction** — a reused byte adds nothing to a set of
    /// bytes. That class stopped being unguarded when D35 clause (c) landed:
    /// [`every_discriminated_constructor_is_registered_with_its_pair`] models
    /// `(byte, discriminator)` pairs, and pre-D35 [`lease_key`]'s overlap with
    /// the ledger was the one live instance of the class.
    #[test]
    fn every_family_prefix_written_in_this_module_is_registered() {
        let written = family_prefixes_written_by_persistd();
        let registered: std::collections::BTreeSet<u8> =
            registered_families().iter().map(|f| f.prefix).collect();

        // Sanity: the scanner must find something, or the two-source property
        // is vacuous and every future family passes unnoticed.
        assert!(
            written.len() >= 19,
            "the source scan found only {} family prefixes; \
             the recognized constructor forms have drifted from the code",
            written.len()
        );

        let unregistered: Vec<char> = written
            .difference(&registered)
            .map(|b| char::from(*b))
            .collect();
        assert!(
            unregistered.is_empty(),
            "key families written by persistd but absent from \
             `registered_families`: {unregistered:?} — a disjointness proof \
             that skips a family is not a disjointness proof (D31 (a))"
        );

        let unwritten: Vec<char> = registered
            .difference(&written)
            .map(|b| char::from(*b))
            .collect();
        assert!(
            unwritten.is_empty(),
            "families registered but written by no persistd constructor: \
             {unwritten:?} — the table has outlived its code"
        );
    }

    /// The completeness half of the sub-span proof (D35 clause (c)): every
    /// `(byte, discriminator)` pair this module writes must be named in the
    /// registry's sub-kind tables, and every pair named must actually be
    /// written.
    ///
    /// This is the guard that closes the class [`lease_key`] belonged to. A
    /// second constructor sharing an already-registered family byte adds
    /// nothing to [`family_prefixes_written_in_this_module`]'s set of bytes,
    /// so [`every_family_prefix_written_in_this_module_is_registered`] cannot
    /// see it; a set of *pairs* grows by one instead, and the defect fails
    /// here with both bytes named.
    ///
    /// **The floor is the anti-vacuity clause.** Set-equality between two
    /// empty sets passes, so the test asserts the scan found at least the
    /// ten discriminated constructors known to exist today (`da db dh dw lb
    /// le li lr ya za`) — nine as the previous guard counted them, plus `za`,
    /// which this change adds and registers so neither side of this test can
    /// pass without the other. If the recognized pairing idiom drifts from the
    /// code — a helper rename, a new construction form — the floor fires first
    /// and names the drift, rather than letting two empty sides pass as equal.
    #[test]
    fn every_discriminated_constructor_is_registered_with_its_pair() {
        let written = discriminated_pairs_written_in_this_module();

        let mut registered = std::collections::BTreeSet::new();
        for family in registered_families() {
            if let Kinds::SubKinds { table } = family.kinds {
                for kind in table {
                    registered.insert((family.prefix, kind.discriminator));
                }
            }
        }

        let spell = |pairs: &std::collections::BTreeSet<(u8, u8)>| {
            pairs
                .iter()
                .map(|(b, d)| format!("{}{}", char::from(*b), char::from(*d)))
                .collect::<Vec<_>>()
                .join(" ")
        };

        // Sanity, floored: ten discriminated constructors exist today.
        assert!(
            written.len() >= 10,
            "the source scan found only {} discriminated constructors \
             ({}); the recognized pairing idiom has drifted from the code",
            written.len(),
            spell(&written)
        );

        let unregistered: Vec<String> = written
            .difference(&registered)
            .map(|(b, d)| format!("{}{}", char::from(*b), char::from(*d)))
            .collect();
        assert!(
            unregistered.is_empty(),
            "discriminated constructors written by this module but absent \
             from `registered_families`: {unregistered:?} — a constructor \
             that shares a registered family byte must declare its sub-span \
             in the registry (D35 clause (c))"
        );

        let unwritten: Vec<String> = registered
            .difference(&written)
            .map(|(b, d)| format!("{}{}", char::from(*b), char::from(*d)))
            .collect();
        assert!(
            unwritten.is_empty(),
            "sub-kinds registered but written by no constructor in this \
             module: {unwritten:?} — the table has outlived its code"
        );
    }

    /// The acceptance test D35 clause (a) specifies, inverted from the
    /// recording test it replaces.
    ///
    /// Until August 2026 `lease/{grid}/{entity}` shared the ledger's `b'l'`
    /// without its discriminator discipline — byte 1 held the `GridId`'s most
    /// significant byte, so `grid.0 >= 0x6200_0000` sorted a lease row inside
    /// `ledger/bal/`, `0x6900_0000` inside `ledger/item/`, `0x7200_0000`
    /// inside `ledger/receipt/`. `lease_key_overlaps_the_ledger_family`
    /// recorded that overlap as present-tense so no undiscussed "fix" could
    /// land; D35 discussed it and fixed it, and this test holds the fix to
    /// the record's own assertions: byte 1 is `'e'` at the grid ids that used
    /// to collide, the key escapes all three ledger sub-spans, and the extreme
    /// grid id still sorts inside the family and its sub-span.
    #[test]
    fn lease_key_is_discriminated_inside_the_ledger_family() {
        let entity = PersistId::new(1);

        // `0x6200_0000` is the smallest grid id whose high byte used to land a
        // lease row inside `ledger/bal/`; the item and receipt collisions began
        // at `0x6900_0000` and `0x7200_0000`.
        let colliding = lease_key(GridId::new(0x6200_0000), entity);
        assert_eq!(colliding[0], b'l');
        assert_eq!(
            colliding[1], b'e',
            "byte 1 is the ASCII discriminator, never the grid id's high byte"
        );
        assert_eq!(colliding.len(), 14, "`le ‖ grid ‖ entity`");

        let bal = ledger_bal_key(AccountId::new(u64::MIN), AssetId::new(u64::MIN));
        assert!(
            !(colliding.as_slice() >= bal.as_slice()
                && colliding.as_slice() < [b'l', b'c'].as_slice()),
            "the once-colliding grid id must not sort inside [lb, lc)"
        );
        let item = ledger_item_key(ItemUid::new(u64::MIN));
        assert!(
            !(colliding.as_slice() >= item.as_slice()
                && colliding.as_slice() < [b'l', b'j'].as_slice()),
            "…nor inside [li, lj)"
        );
        let receipt = ledger_receipt_key();
        assert!(
            !(colliding.as_slice() >= receipt.as_slice()
                && colliding.as_slice() < [b'l', b's'].as_slice()),
            "…nor inside [lr, ls) — the range both harness receipt scanners walk"
        );

        // The extreme grid stays inside the family: the sub-span is bounded by
        // the discriminators, not by anything beneath them.
        let max = lease_key(GridId::new(u32::MAX), entity);
        assert_eq!(max[1], b'e');
        assert!(max.as_slice() >= [b'l', b'e'].as_slice());
        assert!(
            max.as_slice() < [b'l', b'f'].as_slice(),
            "`lease_key(GridId::new(u32::MAX))` still sorts inside [le, lf) ⊂ [l, m)"
        );
    }

    #[test]
    fn epoch_keys_are_grid_scoped_and_sort_by_epoch_within_a_cell() {
        let shard = CellId::from_bits(SHARD).unwrap();
        let grid_a = GridId::new(7);
        let grid_b = GridId::new(9);

        // D22 in one assertion: two grids' identically numbered cells must not
        // share a witness-epoch row, or a set drawn from one grid's population
        // would govern the other's.
        assert_ne!(epoch_key(grid_a, shard, 1), epoch_key(grid_b, shard, 1));

        let first = epoch_key(grid_a, shard, 1);
        let second = epoch_key(grid_a, shard, 2);
        assert_eq!(first[0], b'e');
        assert_eq!(first.len(), 17);
        assert!(
            first.as_slice() < second.as_slice(),
            "a cell's epochs must sort in announcement order, so an auditor's \
             scan is chronological without a secondary sort"
        );
        assert!(first.as_slice() >= epoch_range_start().as_slice());
        assert!(first.as_slice() < epoch_range_end().as_slice());
        assert!(second.as_slice() < epoch_range_end().as_slice());

        // The handle index is a separate family, and the exclusive end of the
        // epoch span is its first byte — adjacent and disjoint.
        let handle = epoch_handle_key(orrery_protocol::WitnessEpochClaimsV1::compose_handle(1, 4));
        assert_eq!(handle[0], b'f');
        assert_eq!(handle.len(), 9);
        assert!(handle.as_slice() >= epoch_range_end().as_slice());
        assert!(
            epoch_key(GridId::new(u32::MAX), shard, u32::MAX).as_slice()
                < epoch_range_end().as_slice(),
            "no epoch row, however extreme, may sort into the handle family"
        );

        // Handles are big-endian so an incarnation bump sorts after every
        // handle the previous incarnation could mint.
        assert!(
            epoch_handle_key(orrery_protocol::WitnessEpochClaimsV1::compose_handle(
                1,
                0x0000_ffff_ffff_ffff
            ))
            .as_slice()
                < epoch_handle_key(orrery_protocol::WitnessEpochClaimsV1::compose_handle(2, 0))
                    .as_slice()
        );
    }

    #[test]
    fn attest_key_is_17_bytes_with_g_prefix_and_shares_the_intent_id_order() {
        let key = attest_key(42);
        assert_eq!(key[0], b'g');
        assert_eq!(key.len(), 17);
        assert_eq!(&key[1..], &42u128.to_be_bytes());
        // Same encoding as `intent_key`, one prefix apart: the two rows are
        // written in one transaction and read together by an audit, so a
        // reader that has one id has both keys with no re-derivation.
        assert_eq!(&attest_key(42)[1..], &intent_key(42)[1..]);
        assert!(attest_key(0).as_slice() >= attest_range_start().as_slice());
        assert!(attest_key(u128::MAX).as_slice() < attest_range_end().as_slice());
    }

    #[test]
    fn fence_keys_are_grid_scoped_and_ranges_do_not_overlap() {
        let shard = CellId::from_bits(SHARD).unwrap();
        let grid_a = GridId::new(7);
        let grid_b = GridId::new(9);

        let key_a = fence_key(grid_a, shard);
        let key_b = fence_key(grid_b, shard);
        let expected_a = [
            b'a', 0x00, 0x00, 0x00, 0x07, 0xA9, 0x24, 0x92, 0x49, 0x24, 0x92, 0x4E, 0x00,
        ];
        let expected_b = [
            b'a', 0x00, 0x00, 0x00, 0x09, 0xA9, 0x24, 0x92, 0x49, 0x24, 0x92, 0x4E, 0x00,
        ];
        assert_eq!(key_a.as_slice(), expected_a.as_slice());
        assert_eq!(key_b.as_slice(), expected_b.as_slice());
        assert_ne!(key_a, key_b);

        let grid_a_start = fence_grid_range_start(grid_a);
        let grid_a_end = fence_grid_range_end(grid_a);
        let grid_b_start = fence_grid_range_start(grid_b);
        let grid_b_end = fence_grid_range_end(grid_b);
        assert_eq!(grid_a_start.as_slice(), [b'a', 0x00, 0x00, 0x00, 0x07]);
        assert_eq!(grid_a_end.as_slice(), [b'a', 0x00, 0x00, 0x00, 0x08]);
        assert_eq!(grid_b_start.as_slice(), [b'a', 0x00, 0x00, 0x00, 0x09]);
        assert_eq!(grid_b_end.as_slice(), [b'a', 0x00, 0x00, 0x00, 0x0A]);
        assert!(grid_a_end.as_slice() <= grid_b_start.as_slice());
        assert!(grid_b_end.as_slice() > grid_b_start.as_slice());

        assert_eq!(fence_range_start(), vec![b'a']);
        assert_eq!(fence_range_end(), vec![b'b']);
    }

    #[test]
    fn jarchive_key_round_trips_metadata() {
        let node = node(1);
        let segment_seq = 42u64;
        let key = jarchive_key(&node, segment_seq);

        // The key layout is exactly `za ‖ node_id ‖ segment_seq`.
        assert_eq!(key.len(), 42, "jarchive key is 42 bytes");
        assert_eq!(key[0], b'z', "family byte is z");
        assert_eq!(key[1], b'a', "discriminator byte is a");
        assert_eq!(&key[2..34], node.as_bytes(), "node id occupies bytes 2..34");
        assert_eq!(
            u64::from_be_bytes(key[34..42].try_into().unwrap()),
            segment_seq,
            "segment_seq is big-endian at bytes 34..42"
        );

        let (decoded_node, decoded_seq) = decode_jarchive_key(&key)
            .expect("decode_jarchive_key must accept a well-formed jarchive key");
        assert_eq!(decoded_node, node, "round-tripped node id");
        assert_eq!(decoded_seq, segment_seq, "round-tripped segment_seq");

        let shard = CellId::from_bits(SHARD).unwrap();
        let meta = JarchiveMetadata {
            object_key: "s3://orrery-archive/jarchive/001/0000002a.parquet".to_owned(),
            cell_ranges: vec![JarchiveCellRange {
                grid: GRID,
                start: shard,
                end: CellId::from_bits(shard.to_bits().wrapping_add(1)).unwrap(),
            }],
            lsn_span: JarchiveLsnSpan {
                start: Lsn::new(1, 0),
                end: Lsn::new(3, 4096),
            },
            checksum: [0xAB; 32],
        };
        let encoded = encode_jarchive_metadata(&meta).expect("encode metadata");
        let decoded = decode_jarchive_metadata(&encoded).expect("decode metadata");
        assert_eq!(decoded, meta, "metadata round-trips byte-for-byte");
    }

    #[test]
    fn jarchive_family_is_disjoint_from_neighbours() {
        let node = node(1);
        let jarchive = jarchive_key(&node, 0);

        // The family sits after `y` (strike) and before the byte after `z`.
        let strike = strike_key(AccountId::new(1));
        assert!(
            jarchive.as_slice() > strike.as_slice(),
            "jarchive/ must sort after strike/"
        );
        assert!(
            jarchive.as_slice() >= jarchive_range_start().as_slice(),
            "jarchive key is inside its family start"
        );
        assert!(
            jarchive.as_slice() < jarchive_range_end().as_slice(),
            "jarchive key is inside its family end"
        );
        assert!(
            jarchive_node_range_end(&node).as_slice() > jarchive_node_range_start(&node).as_slice(),
            "per-node jarchive span is non-empty"
        );
        assert!(
            jarchive.as_slice() < jarchive_node_range_end(&node).as_slice(),
            "a concrete jarchive key sits inside its per-node span"
        );
    }

    #[test]
    fn jarchive_neighbouring_keys_do_not_parse_as_jarchive() {
        let node = node(1);

        // A strike key (`y…`) is the immediate predecessor; it must not decode.
        let strike = strike_key(AccountId::new(1));
        assert_eq!(strike.len(), 20);
        assert!(
            decode_jarchive_key(&strike).is_none(),
            "strike/ key must not parse as jarchive/"
        );

        // A content/version key (`v`) is also a different family; it must not decode.
        let content = content_version_key();
        assert!(
            decode_jarchive_key(&content).is_none(),
            "content/version key must not parse as jarchive/"
        );

        // A truncated or over-long key must not decode.
        assert!(decode_jarchive_key(&jarchive_key(&node, 0)[..41]).is_none());
        let mut too_long = jarchive_key(&node, 0).to_vec();
        too_long.push(0x00);
        assert!(decode_jarchive_key(&too_long).is_none());
    }

    #[test]
    fn jarchive_rejects_mutated_discriminator() {
        let node = node(1);
        let mut key = jarchive_key(&node, 7).to_vec();

        // Mutate the discriminator byte after construction.
        key[1] = b'x';
        assert!(
            decode_jarchive_key(&key).is_none(),
            "decode_jarchive_key must reject a key whose discriminator byte is wrong"
        );

        // Mutate the family byte after construction.
        key[0] = b'y';
        key[1] = b'a';
        assert!(
            decode_jarchive_key(&key).is_none(),
            "decode_jarchive_key must reject a key whose family byte is wrong"
        );
    }

    #[test]
    fn jarchive_family_prefix_and_discriminator_are_za() {
        // This test is the guarded stage for the discriminator byte. A
        // mutation of `JARCHIVE_PREFIX` or `JARCHIVE_DISCRIMINATOR` must fail
        // here by name, not silently move the family in the keyspace.
        let key = jarchive_key(&node(1), 0);
        assert_eq!(key[0], b'z', "jarchive family prefix is z");
        assert_eq!(key[1], b'a', "jarchive sub-kind discriminator is a");
    }

    #[test]
    fn enriched_receipt_round_trips_every_economic_effect() {
        let receipt = ReceiptRow {
            intent_id: 0x832,
            parties: vec![AccountId::new(7), AccountId::new(9)],
            ops: vec![3, 5],
            balance_deltas: vec![
                ReceiptBalanceDelta {
                    account: AccountId::new(7),
                    asset: AssetId::new(11),
                    delta: -40,
                },
                ReceiptBalanceDelta {
                    account: AccountId::new(9),
                    asset: AssetId::new(11),
                    delta: 40,
                },
            ],
            ownership: vec![ReceiptOwnershipTransition {
                item: ItemUid::new(13),
                before: Some(AccountId::new(9)),
                after: Some(AccountId::new(7)),
            }],
        };
        let encoded = encode_receipt_row(&receipt).expect("encode enriched receipt");
        let (decoded, version) = decode_receipt_row(&encoded).expect("decode enriched receipt");
        assert_eq!(version, RECEIPT_ENCODING_V1);
        assert_eq!(decoded, receipt, "deltas, item id and both owners recover");
    }

    #[test]
    fn legacy_receipt_bootstraps_as_v0_without_inventing_effects() {
        let legacy = ReceiptRowV0 {
            intent_id: 77,
            parties: vec![AccountId::new(5)],
            ops: vec![1],
        };
        let bytes = postcard::to_stdvec(&legacy).expect("encode legacy row");
        let (decoded, version) = decode_receipt_row(&bytes).expect("decode legacy row");
        assert_eq!(version, orrery_protocol::atrest::ENCODING_V0);
        assert_eq!(decoded.intent_id, legacy.intent_id);
        assert_eq!(decoded.parties, legacy.parties);
        assert_eq!(decoded.ops, legacy.ops);
        assert!(decoded.balance_deltas.is_empty());
        assert!(decoded.ownership.is_empty());
    }

    #[test]
    fn rarchive_metadata_key_and_value_round_trip() {
        let mut receipt_key = ledger_receipt_key();
        receipt_key[2..].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let key = rarchive_key(&receipt_key).expect("an lr cursor is accepted");
        assert_eq!(&key[..2], b"zr");
        assert_eq!(decode_rarchive_key(&key), Some(receipt_key));
        assert!(key.as_slice() >= rarchive_range_start().as_slice());
        assert!(key.as_slice() < rarchive_range_end().as_slice());

        let meta = RarchiveMetadata {
            object_key: "archive/rarchive/00010203040506070809.parquet".to_owned(),
            first_receipt_key: receipt_key,
            last_receipt_key: receipt_key,
            rows: 1,
            checksum: [0x83; 32],
        };
        let encoded = encode_rarchive_metadata(&meta).expect("encode rarchive metadata");
        assert_eq!(
            *encoded.last().expect("version trailer"),
            RARCHIVE_METADATA_ENCODING
        );
        assert_eq!(decode_rarchive_metadata(&encoded).expect("decode"), meta);
    }

    /// The registry is written in prefix order, and that order is load-bearing
    /// twice: it is what makes the pairwise loop in
    /// [`all_key_families_are_range_disjoint`] a *sorted* comparison rather
    /// than an unordered one, and it is what makes a new row's correct place
    /// obvious to whoever adds it.
    ///
    /// This used to be an eleven-entry table of its own, written down beside
    /// the fourteen-entry one it was meant to corroborate — two hand-kept
    /// copies of a fact, neither complete. It reads the registry now.
    #[test]
    fn prefix_order_is_ascii_and_stable() {
        let families = registered_families();
        let mut prev: Option<&Family> = None;
        for family in &families {
            if let Some(before) = prev {
                assert!(
                    before.prefix < family.prefix,
                    "the registry is kept in prefix order; {} (0x{:02x}) is \
                     listed after {} (0x{:02x})",
                    family.name,
                    family.prefix,
                    before.name,
                    before.prefix
                );
            }
            assert!(
                family.prefix.is_ascii_lowercase(),
                "{} takes 0x{:02x}, which is outside the lowercase family \
                 space the byte budget is counted in",
                family.name,
                family.prefix
            );
            prev = Some(family);
        }
    }
}
