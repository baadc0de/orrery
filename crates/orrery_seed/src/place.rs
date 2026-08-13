//! Placement and per-cell archetype apportionment (docs/12 §5.4–§5.5, D-C).
//!
//! Two cell-local derivations:
//!
//! - **`placement = "hash"`** derives an entity's position inside its cell
//!   from its slot key alone, so position is independent of the cell's
//!   population: a count change in a neighbouring cell cannot move this
//!   cell's entities (the D-C property that makes patching work).
//!   `stratified` is explicitly count-coupled and out of scope in v1.
//! - **Archetype selection is per-cell** (§5.5): within a cell, the weighted
//!   multiset is apportioned by [`largest_remainder`] over *that cell's*
//!   count and then permuted by the cell key — never as a global pass.

use rand::Rng;

use crate::field::Q16_16;
use crate::seedtree::SeedRoot;
use crate::split::largest_remainder;

/// Derive the in-cell position for `placement = "hash"` (docs/12 §5.4):
/// three uniform draws from the slot-key RNG, scaled to the cell edge.
///
/// The position is a pure function of `K_slot` — not of the cell's count,
/// not of any neighbour — so an entity's `local_pos` survives a re-split of
/// its own cell's population. That independence is exactly what §9.3's
/// manifest digest needs to mean "same entity".
///
/// Values are float *content* (docs/12 §8: bit-identical for a fixed
/// toolchain, not across platforms — `local_pos` lives inside the bag, and
/// the manifest pins the toolchain).
#[must_use]
pub fn hash_local_pos(slot_key: [u8; 32], cell_edge_m: f32) -> [f32; 3] {
    let mut rng = SeedRoot::slot_rng(slot_key);
    [
        rng.random::<f32>() * cell_edge_m,
        rng.random::<f32>() * cell_edge_m,
        rng.random::<f32>() * cell_edge_m,
    ]
}

/// One archetype's share of an emit's mix: `(name, weight)`.
///
/// Weights are non-negative `f64` from the scenario file; they are quantized
/// to Q16.16 before apportionment (§8.3 — the quantization boundary applies
/// to every count-determining path, and apportionment decides integer
/// counts).
#[derive(Debug, Clone, PartialEq)]
pub struct ArchetypeWeight {
    /// `[archetype.<name>]`.
    pub name: String,
    /// Non-negative mix weight (relative, not normalized).
    pub weight: f64,
}

/// Apportion `count` entities of one cell among the archetype mix by largest
/// remainder over quantized weights, then permute the per-slot assignment by
/// the cell key (docs/12 §5.5, → A.2.4).
///
/// Returns one archetype index per slot (`0..count`), where the multiset of
/// indices is exactly the largest-remainder apportionment and the *order* is
/// a deterministic permutation seeded by `cell_key` — so slot `i`'s
/// archetype is stable under any change to a *different* cell, and slot
/// counts per archetype are integral and exact.
///
/// Weights are sorted by name before apportionment so the result is
/// independent of the scenario file's map ordering (§8.4).
///
/// # Panics
///
/// Panics if `weights` is empty or every weight quantizes to zero while
/// `count > 0` — the scenario validator rejects both first, so reaching this
/// is a caller bug.
#[must_use]
pub fn apportion_archetypes(
    count: u64,
    weights: &[ArchetypeWeight],
    cell_key: [u8; 32],
) -> Vec<u32> {
    assert!(
        !weights.is_empty(),
        "an emit's archetype mix is never empty (validated)"
    );
    if count == 0 {
        return Vec::new();
    }

    // Quantize the mix weights before they decide anything (§8.3).
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&a, &b| weights[a].name.cmp(&weights[b].name));
    let mut masses: Vec<u64> = Vec::with_capacity(weights.len());
    let mut total = 0u64;
    for &i in &order {
        let q = Q16_16::from_f64(weights[i].weight.max(0.0));
        let m = u64::from(q.0.max(0) as u32);
        masses.push(m);
        total += m;
    }
    assert!(
        total > 0,
        "the archetype mix quantized to zero mass (validated)"
    );
    let seats = largest_remainder(count, &masses, total);

    // Build the multiset in sorted-name order: seats[k] copies of the k-th
    // archetype (in sorted order).
    let mut assignment: Vec<u32> = Vec::with_capacity(count as usize);
    for (seat_count, &i) in seats.iter().zip(order.iter()) {
        for _ in 0..*seat_count {
            assignment.push(i as u32);
        }
    }
    debug_assert_eq!(assignment.len(), count as usize);

    // Permute by the cell key (§5.5): Fisher–Yates driven by a
    // cell-key-seeded ChaCha8 stream, so the permutation is deterministic
    // and cell-local.
    let mut rng = SeedRoot::slot_rng(cell_key);
    for i in (1..assignment.len()).rev() {
        let j = rng.random_range(0..=i);
        assignment.swap(i, j);
    }
    assignment
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seedtree::SeedRoot;
    use orrery_protocol::CellId;

    fn weights(mix: &[(&str, f64)]) -> Vec<ArchetypeWeight> {
        mix.iter()
            .map(|&(name, weight)| ArchetypeWeight {
                name: name.to_string(),
                weight,
            })
            .collect()
    }

    #[test]
    fn hash_local_pos_is_population_independent() {
        // §5.4: position comes from the slot key alone. Two cells with
        // different populations derive their slot-0 position from their own
        // slot keys; the same slot key gives the same position no matter
        // what count the cell was dealt. Assert against an explicit
        // re-derivation rather than a re-call.
        let root = SeedRoot::derive("orrery.seeder.v1", b"place-check");
        let cell = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let slot = root.slot_key_for("world", cell, 0);
        let pos = hash_local_pos(slot, 128.0);

        // Independent re-derivation: same slot key through an explicitly
        // constructed RNG.
        let mut rng = SeedRoot::slot_rng(slot);
        let expect = [
            rng.random::<f32>() * 128.0,
            rng.random::<f32>() * 128.0,
            rng.random::<f32>() * 128.0,
        ];
        assert_eq!(pos, expect);
        for p in pos {
            assert!((0.0..128.0).contains(&p), "in-cell range [0, edge)");
        }

        // A different slot index lands elsewhere (overwhelmingly probable;
        // this asserts the index is an input, not that the RNG is random).
        let other = hash_local_pos(root.slot_key_for("world", cell, 1), 128.0);
        assert_ne!(pos, other);
    }

    #[test]
    fn apportion_is_integral_and_exact_per_cell() {
        // §5.5: per-cell integer counts summing to the cell count. Hand
        // check: 10 entities at 0.7/0.3 → exact shares [7, 3], remainder 0.
        let mix = weights(&[("crate", 0.7), ("barrel", 0.3)]);
        let cell_key = [7u8; 32];
        let assignment = apportion_archetypes(10, &mix, cell_key);
        assert_eq!(assignment.len(), 10);
        let crates = assignment.iter().filter(|&&a| a == 0).count();
        let barrels = assignment.iter().filter(|&&a| a == 1).count();
        // Sorted by name: barrel is index 0 in sorted order, crate index 1 —
        // but the returned indices are into the CALLER's `weights` slice:
        // crate = 0, barrel = 1 there.
        assert_eq!((crates, barrels), (7, 3), "0.7/0.3 of 10 apportions 7/3");
    }

    #[test]
    fn apportion_uses_largest_remainder_for_fractional_shares() {
        // 3 entities at 0.5/0.5: exact [1.5, 1.5], floors [1, 1], remainder
        // 1, residues tie → sorted-name tie-break by index: "barrel" (sorted
        // first) gets the seat → barrel 2, crate 1.
        let mix = weights(&[("crate", 0.5), ("barrel", 0.5)]);
        let assignment = apportion_archetypes(3, &mix, [9u8; 32]);
        let barrels = assignment.iter().filter(|&&a| a == 1).count();
        assert_eq!(barrels, 2, "largest-remainder tie goes to the earlier seat");
    }

    #[test]
    fn apportion_permutation_is_cell_key_seeded() {
        // Same multiset, different cell keys → (almost surely) different
        // orders; same cell key → identical order. The multiset itself must
        // be identical across keys (it is count-determined, not key-driven).
        let mix = weights(&[("crate", 0.7), ("barrel", 0.3)]);
        let a = apportion_archetypes(100, &mix, [1u8; 32]);
        let b = apportion_archetypes(100, &mix, [2u8; 32]);
        let a_again = apportion_archetypes(100, &mix, [1u8; 32]);
        assert_eq!(a, a_again, "same cell key, same permutation");
        let mut sorted_a = a.clone();
        sorted_a.sort_unstable();
        let mut sorted_b = b.clone();
        sorted_b.sort_unstable();
        assert_eq!(sorted_a, sorted_b, "the multiset is key-invariant");
        assert_ne!(a, b, "the permutation is seeded by the cell key");
    }

    #[test]
    fn neighbouring_cell_counts_do_not_move_this_cell() {
        // D-C at the archetype level: this cell's assignment is a function of
        // (its own count, the mix, its own cell key) — a neighbour's count is
        // not an input. Assert by construction: run the apportionment for one
        // cell under two different "neighbourhoods" (which the function
        // cannot even see) and check identity.
        let mix = weights(&[("crate", 0.7), ("barrel", 0.3)]);
        let root = SeedRoot::derive("orrery.seeder.v1", b"neighbour-check");
        let this_cell = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let layer_key = root.layer_key("world");
        let cell_key = SeedRoot::cell_key(&layer_key, this_cell);

        let solo = apportion_archetypes(7, &mix, cell_key);
        // "After" a neighbour's count changes: nothing in the inputs moved.
        let after = apportion_archetypes(7, &mix, cell_key);
        assert_eq!(solo, after);
        // And the multiset matches the hand-computed 0.7/0.3 of 7:
        // exact [4.9, 2.1] → floors [4, 2], remainder 1 → residue 0.9 (crate).
        let crates = solo.iter().filter(|&&a| a == 0).count();
        assert_eq!(crates, 5);
    }

    #[test]
    fn single_archetype_mix_is_identity() {
        let mix = weights(&[("prop", 1.0)]);
        let assignment = apportion_archetypes(64, &mix, [3u8; 32]);
        assert!(assignment.iter().all(|&a| a == 0));
        assert_eq!(assignment.len(), 64);
    }

    /// The brief's named test: a 0.7/0.3 mix over 100 cells yields per-cell
    /// integer counts summing to the cell count, and a neighbouring cell's
    /// count does not change this cell's assignment (docs/12 §5.5, D-C).
    #[test]
    fn archetype_mix_is_per_cell_and_integral() {
        let mix = weights(&[("crate", 0.7), ("barrel", 0.3)]);
        let root = SeedRoot::derive("orrery.seeder.v1", b"per-cell-mix");
        let layer_key = root.layer_key("world");

        // 100 distinct cells, each dealt a count of 7 (so the exact shares
        // are [4.9, 2.1] — fractional, exercising largest remainder).
        let mut assignments = Vec::new();
        for i in 0..100u64 {
            let cell = CellId::from_bits(0xA924_9249_2492_4D65 + i * 8).expect("nonzero");
            let cell_key = SeedRoot::cell_key(&layer_key, cell);
            let a = apportion_archetypes(7, &mix, cell_key);
            // Integral and summing to the cell count.
            assert_eq!(a.len(), 7, "per-cell assignment covers every slot");
            let crates = a.iter().filter(|&&x| x == 0).count();
            let barrels = a.iter().filter(|&&x| x == 1).count();
            assert_eq!(crates + barrels, 7);
            // The multiset is the largest-remainder apportionment of 7 over
            // [0.7, 0.3]: floors [4, 2], remainder 1, residue 0.9 (crate) →
            // [5, 2] in EVERY cell (the multiset is count-determined, so it
            // is identical across cells).
            assert_eq!(
                (crates, barrels),
                (5, 2),
                "every cell apportions the mix to [5, 2]"
            );
            assignments.push(a);
        }

        // D-C: changing a NEIGHBOURING cell's count does not change this
        // cell's assignment. The assignment is a function of (own count, mix,
        // own cell key); recompute cell 0 with its neighbours hypothetically
        // dealt different counts (which the function cannot see) and assert
        // identity.
        let cell0 = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let key0 = SeedRoot::cell_key(&layer_key, cell0);
        let recomputed = apportion_archetypes(7, &mix, key0);
        assert_eq!(
            assignments[0], recomputed,
            "this cell's assignment is independent of any neighbour's count"
        );

        // But the permutation DOES vary by cell key (assignments are not all
        // identical across cells, though the multiset is).
        let all_identical = assignments.iter().all(|a| *a == assignments[0]);
        assert!(
            !all_identical,
            "the cell-key-seeded permutation varies the order across cells"
        );
    }
}
