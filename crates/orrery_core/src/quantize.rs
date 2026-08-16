//! Quantization at tick boundaries (VC-7).
//!
//! Continuous core state is snapped to a fixed lattice at the end of every
//! tick, and the snapped value is what the next tick reads. Two things follow,
//! and both are load-bearing:
//!
//! - The state hash in a [`StateClaim`](orrery_protocol::StateClaim) commits to
//!   exactly what replication and persistence saw — one representation, not a
//!   float that happens to round the same way today.
//! - Residual float drift cannot accumulate across ticks. Each tick starts from
//!   a lattice point, so two builds that disagree by less than half a quantum
//!   re-converge instead of walking apart.
//!
//! The lattice is **1 mm** for position and **1 mm/s** for velocity: an order
//! of magnitude finer than the D16 tolerance bands (ε_pos 1 cm, ε_vel 1 cm/s),
//! so quantization noise can never by itself trip the comparator, and coarse
//! enough that the integer representations stay small on the wire. The specific
//! values are an invented default — D16 fixes the bands, not the lattice — and
//! are stated here so games and witnesses cannot disagree about them.

/// Position lattice: millimetres per quantum.
pub const POS_QUANTA_PER_METRE: f64 = 1_000.0;
/// Velocity lattice: millimetres per second per quantum.
pub const VEL_QUANTA_PER_METRE_PER_SEC: f64 = 1_000.0;

/// A quantized position, in millimetres, relative to its grid origin.
///
/// `i64` rather than `i32`: a grid-relative position at millimetre resolution
/// exceeds `i32`'s ±2 147 km at the scales D5's `CellId` space allows, and
/// silently wrapping a position is the kind of bug that reads as a teleport
/// cheat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QPos {
    /// Millimetres along x.
    pub x: i64,
    /// Millimetres along y.
    pub y: i64,
    /// Millimetres along z.
    pub z: i64,
}

/// A quantized velocity, in millimetres per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QVel {
    /// Millimetres per second along x.
    pub x: i64,
    /// Millimetres per second along y.
    pub y: i64,
    /// Millimetres per second along z.
    pub z: i64,
}

/// Round half away from zero, via `libm` rather than `std` (VC-6).
///
/// Ties matter: `round_ties_even` and round-half-away disagree on exactly the
/// values a lattice produces most often, so the choice has to be pinned rather
/// than inherited from whichever function was reached for.
fn quantum(value: f64, per_unit: f64) -> i64 {
    libm::round(value * per_unit) as i64
}

impl QPos {
    /// Snap metres to the position lattice.
    #[must_use]
    pub fn from_metres(x: f64, y: f64, z: f64) -> Self {
        Self {
            x: quantum(x, POS_QUANTA_PER_METRE),
            y: quantum(y, POS_QUANTA_PER_METRE),
            z: quantum(z, POS_QUANTA_PER_METRE),
        }
    }

    /// The lattice point as metres. Exact: every quantum is representable.
    #[must_use]
    pub fn to_metres(self) -> (f64, f64, f64) {
        (
            self.x as f64 / POS_QUANTA_PER_METRE,
            self.y as f64 / POS_QUANTA_PER_METRE,
            self.z as f64 / POS_QUANTA_PER_METRE,
        )
    }

    /// Squared distance to `other`, in squared millimetres.
    ///
    /// `i128` because a squared `i64` millimetre difference overflows `i64` at
    /// a few kilometres — and the comparator that consumes this must not have
    /// a range past which it silently stops detecting deviation.
    #[must_use]
    pub fn distance_squared(self, other: Self) -> i128 {
        let dx = i128::from(self.x - other.x);
        let dy = i128::from(self.y - other.y);
        let dz = i128::from(self.z - other.z);
        dx * dx + dy * dy + dz * dz
    }
}

impl QVel {
    /// Snap metres per second to the velocity lattice.
    #[must_use]
    pub fn from_metres_per_sec(x: f64, y: f64, z: f64) -> Self {
        Self {
            x: quantum(x, VEL_QUANTA_PER_METRE_PER_SEC),
            y: quantum(y, VEL_QUANTA_PER_METRE_PER_SEC),
            z: quantum(z, VEL_QUANTA_PER_METRE_PER_SEC),
        }
    }

    /// The lattice point as metres per second.
    #[must_use]
    pub fn to_metres_per_sec(self) -> (f64, f64, f64) {
        (
            self.x as f64 / VEL_QUANTA_PER_METRE_PER_SEC,
            self.y as f64 / VEL_QUANTA_PER_METRE_PER_SEC,
            self.z as f64 / VEL_QUANTA_PER_METRE_PER_SEC,
        )
    }

    /// Squared difference to `other`, in squared millimetres per second.
    #[must_use]
    pub fn difference_squared(self, other: Self) -> i128 {
        let dx = i128::from(self.x - other.x);
        let dy = i128::from(self.y - other.y);
        let dz = i128::from(self.z - other.z);
        dx * dx + dy * dy + dz * dz
    }
}

/// Core state that can be snapped to the lattice at a tick boundary (VC-7).
///
/// The executor calls this after every `step`, so a `Ruleset` never has to
/// remember to — forgetting would produce a state hash that commits to an
/// unquantized float, which is precisely the drift the lattice exists to stop.
pub trait Quantized {
    /// Snap every continuous field to its lattice.
    fn quantize(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization_is_idempotent() {
        // The property VC-7 actually needs: the next tick reads a lattice
        // point, so re-snapping it must be a no-op. Without this, state would
        // creep by a fraction of a quantum every tick.
        let once = QPos::from_metres(1.234_5, -9.876_4, 0.000_5);
        let (x, y, z) = once.to_metres();
        assert_eq!(QPos::from_metres(x, y, z), once);
    }

    #[test]
    fn ties_round_away_from_zero_symmetrically() {
        // A half-millimetre is the value a lattice hits constantly. Rounding
        // it inconsistently between positive and negative would bias motion in
        // one direction — small, cumulative, and invisible until it is not.
        assert_eq!(QPos::from_metres(0.0005, 0.0, 0.0).x, 1);
        assert_eq!(QPos::from_metres(-0.0005, 0.0, 0.0).x, -1);
        assert_eq!(QPos::from_metres(0.0015, 0.0, 0.0).x, 2);
        assert_eq!(QPos::from_metres(-0.0015, 0.0, 0.0).x, -2);
    }

    #[test]
    fn sub_quantum_differences_collapse_to_the_same_lattice_point() {
        // Two builds disagreeing by less than half a quantum must land on the
        // same point — that is how the lattice keeps float drift from
        // accumulating into a false deviation.
        let a = QPos::from_metres(5.000_0, 0.0, 0.0);
        let b = QPos::from_metres(5.000_4, 0.0, 0.0);
        assert_eq!(a, b);
    }

    #[test]
    fn distance_survives_kilometre_scale_without_overflow() {
        // A naive i64 square overflows here. A comparator that stopped
        // detecting deviation past a few kilometres would be worse than none.
        let origin = QPos::default();
        let far = QPos::from_metres(2_000_000.0, 0.0, 0.0);
        assert_eq!(far.distance_squared(origin), (2_000_000_000i128).pow(2));
    }

    #[test]
    fn round_trip_through_metres_is_exact_on_the_lattice() {
        for mm in [-1_000_000i64, -1, 0, 1, 7, 1_000_000] {
            let pos = QPos {
                x: mm,
                y: mm,
                z: mm,
            };
            let (x, y, z) = pos.to_metres();
            assert_eq!(QPos::from_metres(x, y, z), pos);
        }
    }
}
