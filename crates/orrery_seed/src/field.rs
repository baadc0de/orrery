//! Quantized scalar fields and the fold algebra (docs/12-world-seeding.md
//! §5.3, §8.3–§8.4).
//!
//! A layer computes a non-negative scalar field `f(cell)` in `f64`; the value
//! is rounded to [`Q16_16`] fixed point **before** any comparison, threshold,
//! accumulation or split (§8.3: "the quantization boundary is the contract").
//! That single rule is what makes every count-determining path integer and
//! bit-identical across platforms, compilers and thread counts — a libm
//! change can move an `f64` in the last bit, but it cannot move a value that
//! was quantized before it decided anything.
//!
//! Accumulators are [`BTreeMap`]-keyed (§8.4: no `HashMap` iteration in any
//! reduction) and fold with `union` only in v1 — the other seven ops, the
//! `where` predicate grammar and `spread` are out of scope and are rejected
//! at scenario validation with a typed "unsupported in v1" error.

use std::collections::BTreeMap;

use orrery_protocol::CellId;

/// The default field clamp, applied after every fold (docs/12 §5.3): field
/// values clamp to `[0, field_clamp]` with default 64.0. A non-zero clamped
/// count is almost always a `blend`-weight bug, so the plan reports it.
pub const DEFAULT_FIELD_CLAMP: f64 = 64.0;

/// The implicit accumulator every scenario starts with, at 0 (docs/12 §5.3:
/// `"main"` exists implicitly at 0). Layers that do not name `into` fold
/// here; emits that do not name `from` read here.
pub const MAIN_ACCUMULATOR: &str = "main";

/// A Q16.16 fixed-point scalar: a 32-bit integer read as `i / 65536`
/// (docs/12 §8.3, → A.14.4).
///
/// Non-negative by construction in the seeder's use — fields are
/// non-negative per §5.3 — but the type is signed so subtraction in
/// intermediate expressions does not wrap silently.
///
/// All count-determining arithmetic on these is `u128` (or `i128` where a
/// difference can go negative) so the splitter is bit-identical on every
/// platform.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Q16_16(pub i32);

impl Q16_16 {
    /// One unit (`1.0`) in Q16.16.
    pub const ONE: Self = Self(1 << 16);

    /// Zero.
    pub const ZERO: Self = Self(0);

    /// The largest representable value (`32767.99998…`).
    pub const MAX: Self = Self(i32::MAX);

    /// Quantize an `f64` to Q16.16, rounding to nearest with ties away from
    /// zero, saturating at both ends.
    ///
    /// **This is the quantization boundary** (docs/12 §8.3). It is the only
    /// place a float enters a count-determining value: call it on a
    /// generator's `f64` output before the value is compared, thresholded,
    /// accumulated or split. `round()` (rather than truncation) keeps the
    /// expected total mass closest to the continuous field's.
    ///
    /// NaN saturates to zero: a generator that produced NaN has a bug, but a
    /// NaN reaching the splitter would poison every downstream count, so it
    /// is clamped to the field minimum here and the layer reports it
    /// upstream (a non-finite field is a validation error at the layer).
    #[must_use]
    pub fn from_f64(v: f64) -> Self {
        if v.is_nan() {
            return Self::ZERO;
        }
        let scaled = v * 65_536.0;
        if scaled >= f64::from(i32::MAX) {
            return Self::MAX;
        }
        if scaled <= f64::from(i32::MIN) {
            return Self(i32::MIN);
        }
        #[allow(clippy::cast_possible_truncation)]
        // The range checks above make truncation unreachable; `round` as i32
        // is exact for |scaled| < 2^31.
        Self(scaled.round() as i32)
    }

    /// Back to `f64`. Only for *reporting* (histograms, plan output) — never
    /// feed this into a comparison, threshold or split (§8.3).
    #[must_use]
    pub fn to_f64(self) -> f64 {
        f64::from(self.0) / 65_536.0
    }

    /// Saturating add in the quantized domain (fold arithmetic is here, not
    /// in floats, so it is associative and platform-independent).
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Saturating clamp to `[0, max]` — the §5.3 post-fold clamp.
    #[must_use]
    pub fn clamp_field(self, max: Self) -> Self {
        Self(self.0.clamp(0, max.0))
    }
}

impl From<Q16_16> for f64 {
    fn from(v: Q16_16) -> f64 {
        v.to_f64()
    }
}

impl core::fmt::Display for Q16_16 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Reporting only; six digits so the quantum (1/65536 ≈ 0.0000153) is
        // always visible.
        write!(f, "{:.6}", self.to_f64())
    }
}

/// A quantized scalar field over cells: the accumulated result of folding
/// layers into one named accumulator (docs/12 §5.3, D-A).
///
/// `BTreeMap` so iteration is sorted by `CellId` — Morton order — which is
/// both the determinism rule (§8.4) and the storage order the splitter emits
/// into (§7.1).
#[derive(Debug, Default, Clone)]
pub struct FieldAccumulator {
    /// Sparse per-cell mass at the accumulator's level. Cells absent from the
    /// map have mass 0 — the accumulator is zero everywhere until folded.
    pub cells: BTreeMap<CellId, Q16_16>,
}

impl FieldAccumulator {
    /// The `union` fold (docs/12 §5.3: `A' = A + f`): superposition,
    /// mass-additive, estimator-exact. Applies the §5.3 post-fold clamp to
    /// `[0, field_clamp]` and counts how many cells clamped — a non-zero
    /// count is reported by the plan because it is almost always a
    /// `blend`-weight bug rather than intent.
    ///
    /// v1 implements only this op; the other seven are rejected at scenario
    /// validation.
    pub fn union(
        &mut self,
        cell: CellId,
        mass: Q16_16,
        field_clamp: Q16_16,
        clamped_cells: &mut u64,
    ) {
        let current = self.cells.get(&cell).copied().unwrap_or(Q16_16::ZERO);
        let sum = current.saturating_add(mass);
        let clamped = sum.clamp_field(field_clamp);
        if clamped != sum {
            *clamped_cells += 1;
        }
        if clamped == Q16_16::ZERO {
            // Keep the map sparse: a cell that folds back to zero is
            // indistinguishable from untouched, and storing it would grow the
            // map with no information.
            self.cells.remove(&cell);
        } else {
            self.cells.insert(cell, clamped);
        }
    }

    /// The mass at one cell (0 when absent).
    #[must_use]
    pub fn mass_at(&self, cell: CellId) -> Q16_16 {
        self.cells.get(&cell).copied().unwrap_or(Q16_16::ZERO)
    }

    /// Total mass as `u128` (the splitter's accumulator width, → A.2.3).
    #[must_use]
    pub fn total_mass(&self) -> u128 {
        self.cells
            .values()
            .map(|m| u128::from(m.0.max(0) as u32))
            .sum()
    }

    /// Number of cells with positive mass.
    #[must_use]
    pub fn occupied_cells(&self) -> u64 {
        self.cells.len() as u64
    }
}

/// The named accumulators of a scenario, keyed `BTreeMap` (§8.4) with
/// `"main"` implicit at 0 (§5.3).
#[derive(Debug, Default, Clone)]
pub struct Accumulators {
    map: BTreeMap<String, FieldAccumulator>,
}

impl Accumulators {
    /// The accumulator map with the implicit `"main"` present at 0.
    #[must_use]
    pub fn new() -> Self {
        let mut map = BTreeMap::new();
        map.insert(MAIN_ACCUMULATOR.to_string(), FieldAccumulator::default());
        Self { map }
    }

    /// A mutable handle to `name`, creating it at 0 if absent.
    pub fn entry(&mut self, name: &str) -> &mut FieldAccumulator {
        self.map.entry(name.to_string()).or_default()
    }

    /// Read an accumulator; the implicit `"main"` (and any named one) is 0
    /// when absent.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&FieldAccumulator> {
        self.map.get(name)
    }

    /// Sorted iteration over `(name, accumulator)` — the only iteration
    /// order this type exposes (§8.4).
    pub fn iter(&self) -> impl Iterator<Item = (&String, &FieldAccumulator)> {
        self.map.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::CellId;

    fn cell(bits: u64) -> CellId {
        CellId::from_bits(bits).expect("nonzero bits")
    }

    #[test]
    fn quantization_boundary_rounds_to_nearest() {
        // The quantum is 1/65536 ≈ 0.0000153; values round to the nearest
        // quantum, ties away from zero.
        assert_eq!(Q16_16::from_f64(1.0), Q16_16::ONE);
        assert_eq!(Q16_16::from_f64(0.0), Q16_16::ZERO);
        assert_eq!(Q16_16::from_f64(0.5).0, 32_768);
        // Just below/above a half-quantum boundary at 1.5 quanta.
        assert_eq!(Q16_16::from_f64(1.4 / 65_536.0).0, 1);
        assert_eq!(Q16_16::from_f64(1.5 / 65_536.0).0, 2);
        assert_eq!(Q16_16::from_f64(1.6 / 65_536.0).0, 2);
    }

    #[test]
    fn quantization_saturates_and_rejects_nan() {
        assert_eq!(Q16_16::from_f64(f64::INFINITY), Q16_16::MAX);
        assert_eq!(Q16_16::from_f64(f64::NEG_INFINITY), Q16_16(i32::MIN));
        // NaN poisons a split; it saturates to zero here and is reported at
        // the layer that produced it.
        assert_eq!(Q16_16::from_f64(f64::NAN), Q16_16::ZERO);
    }

    #[test]
    fn field_arithmetic_is_integer_and_associative() {
        // The fold must be associative: (a+b)+c == a+(b+c) exactly, which is
        // what makes thread-count-invariant accumulation possible.
        let a = Q16_16::from_f64(0.1);
        let b = Q16_16::from_f64(0.2);
        let c = Q16_16::from_f64(0.3);
        assert_eq!(
            a.saturating_add(b).saturating_add(c),
            a.saturating_add(b.saturating_add(c))
        );
    }

    #[test]
    fn union_is_mass_additive_and_clamps() {
        let mut acc = FieldAccumulator::default();
        let clamp = Q16_16::from_f64(DEFAULT_FIELD_CLAMP);
        let mut clamped = 0u64;

        let c1 = cell(0xA924_9249_2492_4D65);
        acc.union(c1, Q16_16::from_f64(1.5), clamp, &mut clamped);
        acc.union(c1, Q16_16::from_f64(2.25), clamp, &mut clamped);
        assert_eq!(acc.mass_at(c1), Q16_16::from_f64(3.75));
        assert_eq!(clamped, 0);

        // Push one cell past the clamp: 64.0 is the default ceiling (§5.3).
        acc.union(c1, Q16_16::from_f64(100.0), clamp, &mut clamped);
        assert_eq!(acc.mass_at(c1), clamp);
        assert_eq!(clamped, 1, "the clamped cell is counted for the report");
    }

    #[test]
    fn union_keeps_the_map_sparse() {
        let mut acc = FieldAccumulator::default();
        let clamp = Q16_16::from_f64(DEFAULT_FIELD_CLAMP);
        let mut clamped = 0u64;
        let c1 = cell(0xA924_9249_2492_4D65);
        acc.union(c1, Q16_16::ZERO, clamp, &mut clamped);
        assert_eq!(acc.occupied_cells(), 0, "zero mass does not occupy a cell");
    }

    #[test]
    fn main_accumulator_is_implicit_at_zero() {
        let accs = Accumulators::new();
        let main = accs.get(MAIN_ACCUMULATOR).expect("main exists implicitly");
        assert_eq!(main.occupied_cells(), 0);
        assert_eq!(main.total_mass(), 0);
    }
}
