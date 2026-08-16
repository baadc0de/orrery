//! `CellId` property suite (docs/11-roadmap.md §P1 deliverable).
//!
//! The unit tests in `cell.rs` pin specific encodings. These pin the three
//! *invariants* the rest of the system is built on, over the whole coordinate
//! space rather than the handful of points someone thought to write down:
//!
//! 1. **Levels round-trip.** Coordinates in, the same coordinates out — at
//!    every level, everywhere in the addressable volume.
//! 2. **Parent is a prefix range.** `subtree_range` is what makes a cell's
//!    descendants one contiguous storage scan (D5, D11), so ancestry and range
//!    containment have to be the same predicate. If they ever diverge, an area
//!    read silently returns the wrong entities.
//! 3. **Sort order is spatial locality.** Sorting by the raw u64 has to group
//!    siblings, which is the entire reason for the Morton interleave. Stated
//!    sharply: **no cell outside a subtree may sort between two cells inside
//!    it** — a single interleaver bug would put a distant cell in the middle of
//!    a range scan, and no example-based test would be likely to find it.
//!
//! Level 21 addresses ±2²⁰ cells per axis. Coordinates are drawn per level so
//! the strategies never spend time generating rejects.

use orrery_protocol::{cell_id_from_metres, metres_from_cell_id, shard_of, CellId, INTEREST_LEVEL};
use proptest::prelude::*;

/// Coordinates valid at `level`, and the level itself.
fn cell_at_level() -> impl Strategy<Value = (glam::IVec3, u8)> {
    (1u8..=CellId::MAX_LEVEL).prop_flat_map(|level| {
        let half = 1i64 << (level - 1);
        let lo = -half as i32;
        let hi = (half - 1) as i32;
        (lo..=hi, lo..=hi, lo..=hi, Just(level))
            .prop_map(|(x, y, z, level)| (glam::IVec3::new(x, y, z), level))
    })
}

fn any_cell() -> impl Strategy<Value = CellId> {
    cell_at_level().prop_map(|(xyz, level)| CellId::from_coords(xyz, level).expect("in range"))
}

/// A cell at the interest level, where the AOI and metre conversions live.
fn interest_cell() -> impl Strategy<Value = CellId> {
    let half = 1i64 << (INTEREST_LEVEL - 1);
    let lo = -half as i32;
    let hi = (half - 1) as i32;
    (lo..=hi, lo..=hi, lo..=hi).prop_map(|(x, y, z)| {
        CellId::from_coords(glam::IVec3::new(x, y, z), INTEREST_LEVEL).expect("in range")
    })
}

proptest! {
    // ── 1. Round-trips ───────────────────────────────────────────────────

    #[test]
    fn coordinates_round_trip_at_every_level((xyz, level) in cell_at_level()) {
        let cell = CellId::from_coords(xyz, level).expect("in range");
        prop_assert_eq!(cell.coords(), (xyz, level));
        prop_assert_eq!(cell.level(), level);
    }

    #[test]
    fn raw_bits_round_trip(cell in any_cell()) {
        // Storage keys are raw u64s; a decode that lost information would
        // corrupt every range scan that reads one back.
        prop_assert_eq!(CellId::from_bits(cell.to_bits()), Some(cell));
    }

    #[test]
    fn a_coordinate_outside_the_level_is_refused((xyz, level) in cell_at_level()) {
        // The encoding biases by 2^(L-1); a coordinate one past the edge would
        // wrap into a *valid-looking* cell somewhere else entirely.
        let half = (1i64 << (level - 1)) as i32;
        prop_assert!(CellId::from_coords(glam::IVec3::new(half, xyz.y, xyz.z), level).is_err());
        prop_assert!(CellId::from_coords(glam::IVec3::new(-half - 1, xyz.y, xyz.z), level).is_err());
    }

    // ── 2. Parent is a prefix range ──────────────────────────────────────

    #[test]
    fn a_parent_contains_its_child(cell in any_cell()) {
        let parent = cell.parent().expect("level >= 1 has a parent");
        prop_assert_eq!(parent.level(), cell.level() - 1);
        prop_assert!(parent.is_prefix_of(cell));
        prop_assert!(parent.subtree_range().contains(&cell.to_bits()));
    }

    #[test]
    fn every_ancestor_contains_the_cell(cell in any_cell(), depth in 0u8..=CellId::MAX_LEVEL) {
        let level = depth.min(cell.level());
        let ancestor = cell.ancestor_at(level);
        prop_assert_eq!(ancestor.level(), level);
        prop_assert!(ancestor.is_prefix_of(cell));
    }

    #[test]
    fn ancestry_and_range_containment_are_the_same_predicate(
        a in any_cell(),
        b in any_cell(),
    ) {
        // The one that matters for storage: `subtree_range` is used as a scan
        // bound, `ancestor_at` as the logical test. If they ever disagree, an
        // area read returns entities from somewhere else.
        let contains = a.is_prefix_of(b);
        let by_ancestry = b.level() >= a.level() && b.ancestor_at(a.level()) == a;
        prop_assert_eq!(contains, by_ancestry, "{:?} vs {:?}", a, b);
    }

    #[test]
    fn the_eight_children_partition_their_parent(cell in any_cell()) {
        prop_assume!(cell.level() < CellId::MAX_LEVEL);
        let children = cell.children();
        for child in children {
            prop_assert_eq!(child.level(), cell.level() + 1);
            prop_assert_eq!(child.parent(), Some(cell));
            prop_assert!(cell.is_prefix_of(child));
        }
        // Distinct: eight children, eight ids.
        let mut ids: Vec<u64> = children.iter().map(|c| c.to_bits()).collect();
        ids.sort_unstable();
        ids.dedup();
        prop_assert_eq!(ids.len(), 8);
    }

    // ── 3. Sort order is spatial locality ────────────────────────────────

    #[test]
    fn nothing_outside_a_subtree_sorts_inside_it(
        parent in any_cell(),
        other in any_cell(),
    ) {
        // The sharp form of "sort order = locality". A subtree occupies one
        // contiguous span of the ordering, so an outsider is either strictly
        // below the whole span or strictly above it — never between. This is
        // exactly the guarantee a range scan relies on, and it is the one an
        // interleaver bug breaks.
        let range = parent.subtree_range();
        let id = other.to_bits();
        if !parent.is_prefix_of(other) {
            prop_assert!(
                id < *range.start() || id > *range.end(),
                "{other:?} sorts inside {parent:?}'s span without being a descendant"
            );
        }
    }

    #[test]
    fn a_subtree_range_starts_and_ends_within_itself(cell in any_cell()) {
        let range = cell.subtree_range();
        prop_assert!(range.contains(&cell.to_bits()));
        prop_assert!(range.start() <= range.end());
        // A cell is always in its own subtree.
        prop_assert!(cell.is_prefix_of(cell));
    }

    #[test]
    fn a_coarser_cells_span_encloses_a_finer_ones(cell in any_cell()) {
        prop_assume!(cell.level() >= 2);
        let parent = cell.parent().expect("level >= 1");
        let outer = parent.subtree_range();
        let inner = cell.subtree_range();
        prop_assert!(outer.start() <= inner.start() && inner.end() <= outer.end());
    }

    // ── Neighbourhood and conversions ────────────────────────────────────

    #[test]
    fn stepping_to_a_neighbour_and_back_returns_home(
        cell in any_cell(),
        dx in -1i32..=1,
        dy in -1i32..=1,
        dz in -1i32..=1,
    ) {
        let step = glam::IVec3::new(dx, dy, dz);
        // `None` at the volume edge is the documented outcome, not a failure.
        if let Some(neighbour) = cell.neighbor(step) {
            prop_assert_eq!(neighbour.level(), cell.level());
            prop_assert_eq!(neighbour.neighbor(-step), Some(cell));
        }
    }

    #[test]
    fn the_aoi_is_at_most_27_distinct_same_level_cells_including_self(cell in any_cell()) {
        let neighbours = cell.neighbors27();
        prop_assert!(neighbours.len() <= 27);
        prop_assert!(neighbours.contains(&cell), "the AOI always includes its centre");
        for n in &neighbours {
            prop_assert_eq!(n.level(), cell.level());
        }
        let mut ids: Vec<u64> = neighbours.iter().map(|c| c.to_bits()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        prop_assert_eq!(ids.len(), before, "the AOI must not repeat a cell");
    }

    #[test]
    fn an_interior_cell_has_a_full_27_cell_aoi(cell in any_cell()) {
        // Away from the volume edge the fast path must yield exactly 27; a
        // short AOI in the interior would silently narrow replication.
        let (coords, level) = cell.coords();
        let half = 1i64 << (level - 1);
        let interior = [coords.x, coords.y, coords.z]
            .iter()
            .all(|c| i64::from(*c) > -half && i64::from(*c) < half - 1);
        prop_assume!(interior);
        prop_assert_eq!(cell.neighbors27().len(), 27);
    }

    #[test]
    fn a_shard_is_the_ancestor_three_levels_up(cell in interest_cell()) {
        let shard = shard_of(cell);
        prop_assert_eq!(shard.level(), INTEREST_LEVEL - 3);
        prop_assert!(shard.is_prefix_of(cell));
        // 8×8×8 interest cells per shard.
        prop_assert_eq!(cell.ancestor_at(INTEREST_LEVEL - 3), shard);
    }

    #[test]
    fn a_cells_own_corner_maps_back_to_it(cell in interest_cell()) {
        // `metres_from_cell_id` returns the minimum corner, which must land
        // inside the cell it came from — an off-by-one here would put an entity
        // standing on its own origin in the neighbouring cell.
        let corner = metres_from_cell_id(cell, orrery_protocol::DEFAULT_CELL_EDGE_M);
        let back = cell_id_from_metres(corner, orrery_protocol::DEFAULT_CELL_EDGE_M)
            .expect("a cell's own corner is in range");
        prop_assert_eq!(back, cell);
    }
}
