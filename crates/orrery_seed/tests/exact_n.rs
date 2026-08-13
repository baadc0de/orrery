//! Property tests for the exact-N splitter (docs/12 §7.1, → A.2.2–A.2.4):
//! the summed emitted counts equal N **exactly**, and every cell's count is
//! within ±1 of its proportional target (systematic, not multinomial).

use orrery_seed::split::{largest_remainder, proportional_target};
use proptest::prelude::*;

proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(256))]

    /// A.2.4 / D-B: largest-remainder apportionment is EXACT — the dealt
    /// counts sum to N, no matter the mass vector.
    #[test]
    fn achieved_count_equals_target_exactly(
        n in 1u64..=100_000,
        // Varied mass vectors: 1..=16 buckets, weights 0..=10 000 (zero
        // included: a zero-mass bucket must still receive exactly 0).
        masses in proptest::collection::vec(0u64..=10_000, 1..=16),
    ) {
        let total: u64 = masses.iter().sum();
        proptest::prop_assume!(total > 0);
        let counts = largest_remainder(n, &masses, total);
        prop_assert_eq!(
            counts.iter().sum::<u64>(),
            n,
            "the dealt counts sum to N exactly"
        );
    }

    /// A.2.2: systematic allocation deviates from the exact proportional
    /// target by at most ±1 for EVERY bucket — not on average, always.
    #[test]
    fn per_cell_deviation_is_at_most_one(
        n in 1u64..=100_000,
        masses in proptest::collection::vec(1u64..=10_000, 1..=16),
    ) {
        let total: u64 = masses.iter().sum();
        let counts = largest_remainder(n, &masses, total);
        for (i, (&c, &m)) in counts.iter().zip(masses.iter()).enumerate() {
            let exact = proportional_target(n, m, total);
            prop_assert!(
                (c as f64 - exact).abs() <= 1.0,
                "bucket {i}: count {c} deviates from exact {exact} by more than 1 (n={n})"
            );
        }
    }

    /// A.2.4: the tie-break is deterministic — same inputs, same seats, every
    /// run (this is what makes the manifest thread-count-invariant).
    #[test]
    fn apportionment_is_deterministic(
        n in 1u64..=100_000,
        masses in proptest::collection::vec(0u64..=1_000, 1..=16),
    ) {
        let total: u64 = masses.iter().sum();
        proptest::prop_assume!(total > 0);
        let a = largest_remainder(n, &masses, total);
        let b = largest_remainder(n, &masses, total);
        prop_assert_eq!(a, b);
    }

    /// Zero-mass buckets receive exactly zero — the remainder never lands
    /// where there is no mass.
    #[test]
    fn zero_mass_buckets_receive_zero(
        n in 1u64..=100_000,
        masses in proptest::collection::vec(0u64..=1_000, 1..=16),
    ) {
        let total: u64 = masses.iter().sum();
        proptest::prop_assume!(total > 0);
        let counts = largest_remainder(n, &masses, total);
        for (&c, &m) in counts.iter().zip(masses.iter()) {
            if m == 0 {
                prop_assert_eq!(c, 0, "a zero-mass bucket must receive zero");
            }
        }
    }
}

/// A hand-computed regression against the A.2.4 worked form, so the property
/// suite is anchored to a closed-form answer and not just to itself.
#[test]
fn hand_computed_apportionment() {
    // Hare quota, n = 10 over masses [3, 1, 1] (total 5): exact shares
    // [6.0, 2.0, 2.0] → floors exact, no remainder.
    assert_eq!(largest_remainder(10, &[3, 1, 1], 5), vec![6, 2, 2]);
    // n = 7 over [1, 1, 1]: exact [2.33…; 3], floors [2, 2, 2], remainder 1,
    // residues tie → the seat goes to the lowest child index (A.2.4).
    assert_eq!(largest_remainder(7, &[1, 1, 1], 3), vec![3, 2, 2]);
}
