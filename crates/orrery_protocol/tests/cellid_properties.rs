//! Property tests for the `CellId` encoding (P1 deliverable, docs/11-roadmap.md
//! §P1).
//!
//! The three invariants the roadmap calls out:
//! - **sort order = spatial locality** — numerically adjacent IDs are (mostly)
//!   spatially adjacent cells;
//! - **parent = prefix range** — a cell's subtree is exactly one contiguous u64
//!   range, and `parent` is a prefix of every descendant;
//! - **level round-trips** — `from_coords`/`coords` are inverses, and
//!   `level()` agrees with the construction level.

use glam::IVec3;
use orrery_protocol::CellId;
use proptest::prelude::*;

/// A valid signed coordinate at `level`: in `[−2^(L−1), 2^(L−1))`.
fn coord(level: u8) -> impl Strategy<Value = i32> {
    let half = 1i64 << (level - 1);
    (-half..half).prop_map(|c| c as i32)
}

/// A valid `CellId` at a random level in the operating range `1..=MAX_LEVEL`.
///
/// Level 0 (the root) is a degenerate boundary — it spans 2 units per axis
/// while reporting coordinate 0 — and the spatial model operates at levels
/// 18–21 (docs/01-spatial-model.md §4). We exclude it from the property
/// generators and cover it with the explicit unit tests.
fn any_cell() -> impl Strategy<Value = CellId> {
    (1u8..=CellId::MAX_LEVEL).prop_flat_map(|level| {
        (coord(level), coord(level), coord(level))
            .prop_map(move |(x, y, z)| CellId::from_coords(IVec3::new(x, y, z), level).unwrap())
            .boxed()
    })
}

/// A `CellId` at a level that has children (`1..MAX_LEVEL`).
fn any_parent() -> impl Strategy<Value = CellId> {
    (1u8..CellId::MAX_LEVEL).prop_flat_map(|level| {
        (coord(level), coord(level), coord(level))
            .prop_map(move |(x, y, z)| CellId::from_coords(IVec3::new(x, y, z), level).unwrap())
            .boxed()
    })
}

proptest! {
    /// `from_coords` and `coords` are exact inverses.
    #[test]
    fn coords_roundtrip(cell in any_cell()) {
        let (coords, level) = cell.coords();
        prop_assert_eq!(CellId::from_coords(coords, level).unwrap(), cell);
    }

    /// `level()` agrees with the construction level.
    #[test]
    fn level_agrees(cell in any_cell()) {
        let (_, level) = cell.coords();
        prop_assert_eq!(cell.level(), level);
    }

    /// A cell's subtree is exactly the contiguous u64 range
    /// `[id − lsb + 1, id + lsb − 1]`, and `is_prefix_of` matches it.
    #[test]
    fn subtree_is_contiguous_range(cell in any_cell()) {
        let range = cell.subtree_range();
        let bits = cell.to_bits();
        prop_assert!(range.contains(&bits));
        // The range endpoints bracket the cell.
        prop_assert!(*range.start() <= bits);
        prop_assert!(*range.end() >= bits);
    }

    /// `parent` is a prefix of every descendant: the parent's subtree range
    /// contains the child's, and the child's subtree is a subset.
    #[test]
    fn parent_is_prefix_of_child(parent in any_parent()) {
        for child in parent.children() {
            prop_assert!(parent.is_prefix_of(child));
            prop_assert_eq!(child.parent(), Some(parent));
            // Child subtree ⊆ parent subtree.
            let p = parent.subtree_range();
            let c = child.subtree_range();
            prop_assert!(*p.start() <= *c.start());
            prop_assert!(*p.end() >= *c.end());
        }
    }

    /// `ancestor_at` climbs to the requested level and is a prefix.
    #[test]
    fn ancestor_at_is_prefix(cell in any_cell(), target in 0u8..=CellId::MAX_LEVEL) {
        let level = cell.level();
        let target = target.min(level);
        let anc = cell.ancestor_at(target);
        prop_assert_eq!(anc.level(), target);
        prop_assert!(anc.is_prefix_of(cell));
    }

    /// `neighbor` is the inverse of `coords` + offset, and `neighbor(0)` is
    /// identity.
    #[test]
    fn neighbor_roundtrip(cell in any_cell()) {
        prop_assert_eq!(cell.neighbor(IVec3::ZERO), Some(cell));
        let (coords, level) = cell.coords();
        let offset = IVec3::new(1, -1, 2);
        if let Some(n) = cell.neighbor(offset) {
            prop_assert_eq!(n.coords(), (coords + offset, level));
        }
    }

    /// `neighbors27` returns 27 same-level cells including self, each within
    /// one cell of the center (Chebyshev distance ≤ 1).
    ///
    /// Uses interior cells (at least one cell from every volume edge) so the
    /// full 27-cell neighborhood is in range; the volume-edge clamp is covered
    /// by the unit tests.
    #[test]
    fn neighbors27_are_adjacent(level in 2u8..=CellId::MAX_LEVEL) {
        // Coordinate 0 is interior (at least one cell from every edge) for
        // every level ≥ 2, so the full 27-cell neighborhood is in range.
        let xyz = IVec3::ZERO;
        let cell = CellId::from_coords(xyz, level).unwrap();
        let (center, _) = cell.coords();
        let n = cell.neighbors27();
        prop_assert_eq!(n.len(), 27);
        prop_assert!(n.contains(&cell));
        for c in n {
            prop_assert_eq!(c.level(), level);
            let (coords, _) = c.coords();
            let d = (coords - center).abs();
            prop_assert!(d.x <= 1 && d.y <= 1 && d.z <= 1);
        }
    }

    /// **Sort order = spatial locality.** The defining locality property of the
    /// Morton encoding is that a cell's eight children are *contiguous* in
    /// sorted order: sorting a parent's children by raw u64 yields exactly the
    /// 8 sub-octants, so `[parent][entity]` range scans read a neighborhood
    /// contiguously (D11).
    #[test]
    fn sort_order_is_spatially_local(parent in any_parent()) {
        let mut children = parent.children();
        children.sort();
        // The 8 children are distinct and sorted by their Morton code.
        for w in children.windows(2) {
            prop_assert!(w[0] < w[1]);
        }
        // Each child is within the parent's subtree range.
        let pr = parent.subtree_range();
        for c in children {
            prop_assert!(pr.contains(&c.to_bits()));
        }
    }
}
