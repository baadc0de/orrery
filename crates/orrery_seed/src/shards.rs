//! Collapse a seeded manifest into the shard set a deployment must own.
//!
//! A manifest row (docs/12-world-seeding.md §9.3) names an entity's *interest*
//! cell — level [`orrery_protocol::INTEREST_LEVEL`]. A persistd process owns
//! *shard* cells — level [`orrery_protocol::SHARD_LEVEL`] — and spawns one
//! single-writer actor per shard (docs/11-roadmap.md §P2: "single-writer cell
//! actor runtime with rendezvous-hash placement over shard cells"). The
//! collapse between the two is canonical and lives in the protocol crate:
//! [`orrery_protocol::shard_of`].
//!
//! This module applies that collapse to a manifest, because the manifest is
//! the seeder's format and the seeder is the only crate that owns it. A
//! harness that wants "the shard set this world occupies" therefore derives it
//! from the world it actually seeded, never from a constant: the P2 demo
//! profile happens to span 128 level-18 shards today, and a scenario edit
//! moves that number without touching any code.
//!
//! Deliberately *not* a `jq`/`python` one-liner in the harness: the collapse is
//! `ancestor_at(SHARD_LEVEL)` over a packed `CellId`, which a shell
//! reimplementation would have to duplicate bit-for-bit, and a shard set that
//! is subtly wrong does not fail loudly — it routes part of the world to an
//! actor no process owns.

use std::collections::BTreeSet;
use std::io::BufRead;

use orrery_protocol::{shard_of, CellId, GridId};

/// The shard set one manifest implies, and what it took to get there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSet {
    /// The grid the shards are cells of. persistd owns one grid per process
    /// (`RuntimeConfig::grid`), so a shard set is only meaningful with it.
    pub grid: GridId,
    /// The distinct shard cells, ascending by raw bits — the order persistd's
    /// `canonical_shard_set` sorts into, so an operand list built from this is
    /// already in the durable chain identity's order.
    pub shards: Vec<CellId>,
    /// Manifest entries counted in `grid`.
    pub entries: u64,
    /// Manifest entries skipped because they belong to another grid.
    pub skipped_other_grid: u64,
}

impl ShardSet {
    /// The shard set as persistd `--shard` operands: raw hex `CellId` bits,
    /// which persistd's `parse_shard_raw` accepts in the `0x…` form.
    #[must_use]
    pub fn flag_operands(&self) -> Vec<String> {
        self.shards
            .iter()
            .map(|cell| format!("0x{:016X}", cell.to_bits()))
            .collect()
    }
}

/// Read a §9.3 JSONL manifest and collapse it to the shard set for `grid`.
///
/// Streaming: it holds the distinct shards, not the entries, so a 10 M-entity
/// manifest costs at most one set element per shard.
///
/// The `content/version` trailer is recognized by shape (it carries a
/// `content_version` field and no `cell`), the same discriminator the P2 load
/// rig uses. Nothing else is tolerated: an unparseable line is an error, not a
/// skip, because a silently dropped entry is a silently missing shard.
///
/// # Errors
///
/// Returns a message on an I/O failure or a line that is neither a §9.3 entry
/// nor the trailer.
pub fn shard_set_from_manifest<R: BufRead>(reader: R, grid: GridId) -> Result<ShardSet, String> {
    #[derive(serde::Deserialize)]
    struct Line {
        cell: Option<CellId>,
        grid: Option<GridId>,
        content_version: Option<serde_json::Value>,
    }

    let mut shards = BTreeSet::new();
    let mut entries = 0_u64;
    let mut skipped_other_grid = 0_u64;
    for (idx, line) in reader.lines().enumerate() {
        let lineno = idx + 1;
        let line = line.map_err(|e| format!("read manifest line {lineno}: {e}"))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: Line =
            serde_json::from_str(line).map_err(|e| format!("parse manifest line {lineno}: {e}"))?;
        let Some(cell) = parsed.cell else {
            if parsed.content_version.is_some() {
                continue;
            }
            return Err(format!(
                "manifest line {lineno} is neither a section 9.3 entry (it has no `cell`) nor \
                 the `content_version` trailer"
            ));
        };
        // An entry from before the manifest carried a grid is a root-grid
        // entry: `--single-grid` flattens everything onto `GridId::ROOT`.
        if parsed.grid.unwrap_or(GridId::ROOT) != grid {
            skipped_other_grid += 1;
            continue;
        }
        entries += 1;
        shards.insert(shard_of(cell).to_bits());
    }

    Ok(ShardSet {
        grid,
        shards: shards
            .into_iter()
            .map(|bits| CellId::from_bits(bits).expect("shard_of preserves a non-zero cell"))
            .collect(),
        entries,
        skipped_other_grid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{INTEREST_LEVEL, SHARD_LEVEL};

    fn manifest_line(cell: CellId, grid: GridId) -> String {
        format!(
            "{{\"content_key\":{key},\"persist_id\":{pid},\"grid\":{grid},\"cell\":{cell},\
             \"value_digest\":{key},\"byte_len\":4,\"archetype\":\"a\",\"layer\":\"l\",\
             \"emit\":\"e\"}}",
            key = serde_json::to_string(&[0_u8; 16]).expect("bytes"),
            pid = 1,
            grid = serde_json::to_string(&grid).expect("grid"),
            cell = serde_json::to_string(&cell).expect("cell"),
        )
    }

    fn interest_cell(x: i32, y: i32, z: i32) -> CellId {
        CellId::from_coords(glam::IVec3::new(x, y, z), INTEREST_LEVEL).expect("in range")
    }

    /// The property the gate depends on: entities in distinct interest cells
    /// collapse onto the shard cells that contain them, and two children of
    /// one shard produce one shard, not two.
    #[test]
    fn interest_cells_collapse_to_their_shard_parents() {
        let a = interest_cell(0, 0, 0);
        let b = interest_cell(1, 0, 0);
        let far = interest_cell(4096, 4096, 4096);
        let text = [
            manifest_line(a, GridId::ROOT),
            manifest_line(b, GridId::ROOT),
            manifest_line(far, GridId::ROOT),
            "{\"content_version\":{\"content_build\":\"x\"},\"toolchain\":{}}".to_string(),
        ]
        .join("\n");

        let set = shard_set_from_manifest(text.as_bytes(), GridId::ROOT).expect("collapse");
        assert_eq!(set.entries, 3);
        assert_eq!(set.skipped_other_grid, 0);
        assert_eq!(set.shards.len(), 2, "a and b share a shard; far does not");
        for shard in &set.shards {
            assert_eq!(shard.level(), SHARD_LEVEL);
        }
        assert!(set.shards.contains(&shard_of(a)));
        assert!(set.shards.contains(&shard_of(far)));
        assert!(shard_of(a).is_prefix_of(a));
        assert!(shard_of(b).is_prefix_of(b));
    }

    /// Ascending raw-bit order, so the operand list is already canonical.
    #[test]
    fn operands_are_ascending_hex_bits() {
        let text = [
            manifest_line(interest_cell(4096, 4096, 4096), GridId::ROOT),
            manifest_line(interest_cell(0, 0, 0), GridId::ROOT),
        ]
        .join("\n");
        let set = shard_set_from_manifest(text.as_bytes(), GridId::ROOT).expect("collapse");
        let operands = set.flag_operands();
        assert_eq!(operands.len(), 2);
        let mut sorted = operands.clone();
        sorted.sort();
        assert_eq!(operands, sorted);
        assert!(operands.iter().all(|o| o.starts_with("0x")));
    }

    /// A garbled line stops the run: a dropped entry is a missing shard, and a
    /// missing shard is a silently unowned slice of the world.
    #[test]
    fn an_unrecognizable_line_is_an_error_not_a_skip() {
        let err = shard_set_from_manifest(&b"{\"persist_id\":1}"[..], GridId::ROOT)
            .expect_err("must refuse");
        assert!(err.contains("neither"), "unexpected error: {err}");
    }
}
