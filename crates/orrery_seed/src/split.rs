//! Exact-N realization by hierarchical binomial splitting (docs/12 §7.1, →
//! A.2.2–A.2.4).
//!
//! `[[emit]] count = N` is honoured **exactly** by recursively splitting `N`
//! down the [`CellId`] octree: at each node, the parent's count is distributed
//! among its eight children in proportion to their accumulated field mass
//! using integer arithmetic with a deterministic remainder rule. The routine
//! here follows A.2.3/A.2.4 literally — all arithmetic is `u128` over Q16.16
//! masses, so the result is bit-identical on every platform (§8.3).
//!
//! Three properties fall out of the construction and are relied on
//! everywhere else:
//!
//! 1. **Exact counts** — `Σ counts = n` at every node, by construction of
//!    the largest-remainder apportionment.
//! 2. **Morton-order streaming with O(depth) memory** — output arrives
//!    pre-sorted in `CellId` (storage-key) order, no sort pass.
//! 3. **±1 per-cell deviation** from the target profile — systematic rather
//!    than multinomial allocation (→ A.2.2: max |count − N·w| of 0.995 vs 6.2
//!    at N = 10 000 over 32 768 cells).

use orrery_protocol::CellId;

use crate::field::Q16_16;

/// Largest-remainder apportionment (docs/12 → A.2.4 — the Hare quota method,
/// the same routine that allocates parliamentary seats by vote share).
///
/// Distributes exactly `n` indivisible units among `masses.len()` buckets in
/// proportion to `masses`, with `total = Σ masses` supplied by the caller (it
/// is a loop invariant of the recursive splitter). Returns one count per
/// bucket, in bucket order, summing to exactly `n`.
///
/// Determinism: the tie-break on equal fractional remainders is **bucket
/// index ascending** (A.2.4) — not a hash, not iteration order — so the
/// result is a pure function of the inputs.
///
/// All arithmetic is `u128` (A.2.3). `n * mass` cannot overflow: `n` is an
/// entity count (well under 2^64) and `mass` a Q16.16 quantum count (under
/// 2^32), so the product stays under 2^96.
///
/// # Panics
///
/// Panics if `total` is zero while `n > 0` — a degenerate field the caller
/// must handle before apportioning (the recursive splitter emits the
/// fallback itself; see [`split_cell`]).
#[must_use]
pub fn largest_remainder(n: u64, masses: &[u64], total: u64) -> Vec<u64> {
    let n = u128::from(n);
    let total = u128::from(total);
    assert!(
        total > 0 || n == 0,
        "largest_remainder with zero total mass and nonzero n"
    );

    // floor(n * m / total) per bucket, keeping the exact numerator for the
    // fractional-remainder comparison.
    let mut counts = Vec::with_capacity(masses.len());
    let mut numerators = Vec::with_capacity(masses.len());
    let mut assigned = 0u128;
    for &m in masses {
        let num = n * u128::from(m);
        let floor = num / total.max(1);
        counts.push(floor);
        numerators.push(num);
        assigned += floor;
    }
    debug_assert!(assigned <= n, "floors never overshoot");
    let remainder = n - assigned;

    // Order buckets by (fractional remainder desc, index asc). The fractional
    // remainder of num/total is (num % total)/total; comparing two fractions
    // with the same denominator compares the residues — no floats, ever.
    let mut order: Vec<usize> = (0..masses.len()).collect();
    order.sort_by(|&a, &b| {
        let ra = numerators[a] % total.max(1);
        let rb = numerators[b] % total.max(1);
        rb.cmp(&ra).then(a.cmp(&b))
    });
    for &i in order.iter().take(remainder as usize) {
        counts[i] += 1;
    }
    debug_assert_eq!(
        counts.iter().sum::<u128>(),
        n,
        "largest remainder is exact by construction"
    );

    counts
        .into_iter()
        .map(|c| u64::try_from(c).expect("a bucket count never exceeds n"))
        .collect()
}

/// An O(depth) field-mass oracle (docs/12 §7.1): how much mass lies under a
/// cell, in Q16.16 quanta, computed without materializing the subtree.
///
/// The analytic dry run (§7.3) exists because closed-form fields answer this
/// in O(depth); iterative generators cannot, and are out of scope in v1.
pub trait FieldOracle {
    /// The quantized mass of the whole subtree under `cell` (the cell itself
    /// included when `cell.level() == emit_level`).
    fn field_mass(&self, cell: CellId) -> Q16_16;
}

/// Recursively split `n` entities down the octree from `root` to
/// `emit_level`, streaming `(cell, count)` leaves to `sink` in Morton order
/// (docs/12 → A.2.3).
///
/// - `n == 0` emits nothing.
/// - At `emit_level` the whole remaining count lands on the cell.
/// - A node whose subtree mass is zero emits nothing — the count stays with
///   its mass-bearing siblings (the parent distributed `n` by mass, so a
///   zero-mass child was already dealt 0). A *root-level* call with zero
///   total mass is the degenerate case and emits `(root, n)` so no entity is
///   ever lost (A.2.3's fallback).
///
/// Output is pre-sorted in Morton (storage-key) order because children are
/// visited in child-index order, which *is* ascending `CellId` order at every
/// level (§6.2: the octree's Morton prefix *is* the storage key). Memory is
/// O(depth × fanout); a million-entity load never holds more than one batch.
pub fn split_cell(
    oracle: &impl FieldOracle,
    root: CellId,
    n: u64,
    emit_level: u8,
    sink: &mut impl FnMut(CellId, u64),
) {
    debug_assert!(
        root.level() <= emit_level,
        "split root must be at or above the emit level"
    );
    if n == 0 {
        return;
    }
    if root.level() == emit_level {
        sink(root, n);
        return;
    }
    let children = root.children();
    // `CellId::children()` returns child-index order (triplet `i` with
    // `dx = i & 1`), which is NOT ascending Morton-bits order: the emitted
    // triplet bits are `x y z` MSB-first, so ascending bits order is triplet
    // value `x<<2 | y<<1 | z`. Streaming pre-sorted output (A.2.2 property 1)
    // requires visiting in ascending `to_bits()` order, so sort the eight
    // children (constant size) before descending. The masses travel with
    // their child, so the apportionment is unaffected.
    let mut order = [0usize; 8];
    for (i, o) in order.iter_mut().enumerate() {
        *o = i;
    }
    order.sort_by_key(|&i| children[i].to_bits());
    debug_assert!(
        order.windows(2).all(|w| children[w[0]] < children[w[1]]),
        "children visited in ascending Morton order"
    );
    let mut masses = [0u64; 8];
    let mut total = 0u64;
    for &i in &order {
        masses[i] = u64::from(oracle.field_mass(children[i]).0.max(0) as u32);
        total += masses[i];
    }
    if total == 0 {
        // A.2.3's degenerate case: mass is concentrated outside every child
        // of this node (only possible when the split root does not cover the
        // field's bounds — a scenario bug the plan reports, but the count
        // must not vanish).
        sink(root, n);
        return;
    }
    let counts = largest_remainder(n, &masses, total);
    for &i in &order {
        split_cell(oracle, children[i], counts[i], emit_level, sink);
    }
}

/// The per-cell proportional target of cell `i` with mass `m_i`:
/// `n * m_i / total`. Used by the ±1 deviation bound test (→ A.2.2).
#[must_use]
pub fn proportional_target(n: u64, mass: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (u128::from(n) * u128::from(mass)) as f64 / (total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_remainder_matches_hand_computation() {
        // Hare quota by hand: n = 10 over masses [3, 1, 1], total 5.
        // exact = [6.0, 2.0, 2.0] → floors [6, 2, 2], remainder 0.
        assert_eq!(largest_remainder(10, &[3, 1, 1], 5), vec![6, 2, 2]);

        // n = 7 over [1, 1, 1], total 3: exact = [7/3, 7/3, 7/3], floors
        // [2,2,2], remainder 1, all remainders equal (1/3) → tie-break by
        // index ascending: bucket 0 gets the seat.
        assert_eq!(largest_remainder(7, &[1, 1, 1], 3), vec![3, 2, 2]);

        // n = 5 over [2, 1, 1], total 4: exact = [2.5, 1.25, 1.25] → floors
        // [2,1,1], remainder 1, residues 0.5 > 0.25 → bucket 0.
        assert_eq!(largest_remainder(5, &[2, 1, 1], 4), vec![3, 1, 1]);

        // Tie-break exercises the index: n = 2 over [1, 1], exact = [1, 1],
        // remainder 0.
        assert_eq!(largest_remainder(2, &[1, 1], 2), vec![1, 1]);
    }

    #[test]
    fn largest_remainder_is_exact_and_within_one() {
        // Spot-checked closed form (the property test hammers this over
        // random inputs): every count is within 1 of the exact share.
        let masses = [7u64, 3, 9, 1, 5];
        let total: u64 = masses.iter().sum();
        let n = 10_000u64;
        let counts = largest_remainder(n, &masses, total);
        assert_eq!(counts.iter().sum::<u64>(), n);
        for (&c, &m) in counts.iter().zip(masses.iter()) {
            let exact = proportional_target(n, m, total);
            assert!(
                (c as f64 - exact).abs() <= 1.0,
                "count {c} deviates from exact {exact} by more than 1"
            );
        }
    }

    #[test]
    fn largest_remainder_zero_mass_buckets_get_nothing() {
        // Zero-mass buckets have exact share 0 and must receive exactly 0.
        assert_eq!(largest_remainder(4, &[0, 3, 0, 1], 4), vec![0, 3, 0, 1]);
    }

    /// A uniform oracle over an explicit cell set: mass 1.0 per cell in the
    /// set at level 21, subtree sums above. Written out longhand (a BTreeMap
    /// walk) rather than via any production helper.
    struct ExplicitOracle(std::collections::BTreeMap<CellId, u64>);

    impl ExplicitOracle {
        fn uniform_box(cells: &[CellId]) -> Self {
            let mut map = std::collections::BTreeMap::new();
            for &c in cells {
                map.insert(c, 1u64 << 16);
            }
            Self(map)
        }
    }

    impl FieldOracle for ExplicitOracle {
        fn field_mass(&self, cell: CellId) -> Q16_16 {
            let mut total = 0u128;
            for (&c, &m) in &self.0 {
                if cell.is_prefix_of(c) || c == cell {
                    total += u128::from(m);
                }
            }
            Q16_16(u32::try_from(total.min(u128::from(u32::MAX))).unwrap_or(0) as i32)
        }
    }

    #[test]
    fn split_streams_in_morton_order_with_exact_total() {
        // A 4-cell box at level 21: coords (0..2, 0..2, 0..2) restricted to a
        // 2×1×2 slab, mass 1.0 each. n = 10 → each cell's exact share is 2.5
        // → counts are 2 or 3, summing to 10.
        use glam::IVec3;
        let mut cells = Vec::new();
        for x in 0..2 {
            for z in 0..2 {
                cells.push(CellId::from_coords(IVec3::new(x, 0, z), 21).expect("in range"));
            }
        }
        let oracle = ExplicitOracle::uniform_box(&cells);
        let mut out = Vec::new();
        split_cell(&oracle, CellId::ROOT, 10, 21, &mut |cell, count| {
            out.push((cell, count));
        });
        let total: u64 = out.iter().map(|&(_, c)| c).sum();
        assert_eq!(total, 10, "exact N");
        for w in out.windows(2) {
            assert!(w[0].0 < w[1].0, "output streams in Morton order");
        }
        for (cell, count) in &out {
            assert!(
                cells.contains(cell),
                "emitted cell is inside the box: {cell:?}"
            );
            assert!(
                (2..=3).contains(count),
                "uniform 10 over 4 cells deals 2 or 3, got {count}"
            );
        }
        assert_eq!(out.len(), 4, "every occupied cell appears once");
    }

    #[test]
    fn split_zero_mass_subtree_falls_back_to_node() {
        // If the oracle reports no mass anywhere, the split cannot descend;
        // it must still account for every entity (at the root) rather than
        // dropping them.
        struct Zero;
        impl FieldOracle for Zero {
            fn field_mass(&self, _cell: CellId) -> Q16_16 {
                Q16_16::ZERO
            }
        }
        let mut out = Vec::new();
        split_cell(&Zero, CellId::ROOT, 7, 21, &mut |cell, count| {
            out.push((cell, count))
        });
        assert_eq!(out, vec![(CellId::ROOT, 7)]);
    }
}
