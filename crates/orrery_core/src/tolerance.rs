//! The tolerance-band comparator (docs/06 §5).
//!
//! Discrete state is compared bit-exact — any mismatch is a deviation, full
//! stop, and that is where the persistent value lives (VC-5). Continuous state
//! is compared within bands, because peers, field hosts and `persistd` run the
//! same `RulesetId` but not necessarily the same binary: three OSes, two
//! architectures, differing LLVM codegen can reorder non-associative float ops
//! even under libm.
//!
//! The comparator is **entirely integer arithmetic** over the quantization
//! lattice. That is deliberate: a comparator that itself used floats could
//! disagree between the witness that reports and the adjudicator that decides,
//! which would make verdicts platform-dependent — the exact property the bands
//! exist to remove.
//!
//! False-positive strikes on honest players are the failure mode that kills
//! witness-based trust (D17 risk 3), which is why a single noisy tick is not a
//! violation: the error must be sustained.

use orrery_protocol::{DeviationKind, Tick};

use crate::quantize::{QPos, QVel};

/// D16 bands and the sustain rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tolerance {
    /// Positional band, in millimetres. D16: 1 cm.
    pub eps_pos_mm: i64,
    /// Velocity band, in millimetres per second. D16: 1 cm/s.
    pub eps_vel_mms: i64,
    /// Consecutive ticks the error must exceed the band to count. D16: 250 ms,
    /// which is 15 ticks at 60 Hz.
    pub sustain_ticks: u32,
    /// Instantaneous escalation multiple — one tick this far out is a
    /// violation with no sustain needed. An invented default (docs/06 §5).
    pub hard_snap_multiple: i64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            eps_pos_mm: 10,
            eps_vel_mms: 10,
            sustain_ticks: 15,
            hard_snap_multiple: 8,
        }
    }
}

/// One tick of each trajectory, already on the lattice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrajectorySample {
    /// The tick this sample is for.
    pub tick: Tick,
    /// What the authority claimed.
    pub claimed_pos: QPos,
    /// What the authority claimed.
    pub claimed_vel: QVel,
    /// What re-execution computed.
    pub computed_pos: QPos,
    /// What re-execution computed.
    pub computed_vel: QVel,
}

/// The comparator's answer over a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToleranceOutcome {
    /// Every tick sat inside the bands, or outside them for too short a run.
    Within,
    /// A violation, with the tick it is attributed to.
    Violation {
        /// For a sustained run, the tick the run *started* — the first moment
        /// the trajectories parted, not the moment the counter happened to
        /// reach its threshold. An adjudicator quoting the latter would point
        /// at a tick where nothing began.
        at: Tick,
        /// Always [`DeviationKind::ContinuousOutOfBand`] here; discrete
        /// mismatches are found by hash comparison, not by this comparator.
        kind: DeviationKind,
    },
}

impl Tolerance {
    /// Whether one sample is outside the bands at all (normalized error > 1).
    ///
    /// `e = max(|Δpos| / ε_pos, |Δvel| / ε_vel)`, evaluated as a comparison of
    /// squared magnitudes so no square root and no float ever appears.
    #[must_use]
    pub fn exceeds(&self, sample: &TrajectorySample) -> bool {
        self.exceeds_multiple(sample, 1)
    }

    /// Whether a sample exceeds `multiple` times the bands.
    #[must_use]
    pub fn exceeds_multiple(&self, sample: &TrajectorySample, multiple: i64) -> bool {
        let pos_limit = i128::from(self.eps_pos_mm.saturating_mul(multiple));
        let vel_limit = i128::from(self.eps_vel_mms.saturating_mul(multiple));
        sample.claimed_pos.distance_squared(sample.computed_pos) > pos_limit * pos_limit
            || sample.claimed_vel.difference_squared(sample.computed_vel) > vel_limit * vel_limit
    }

    /// Judge a whole window.
    ///
    /// Both trajectories start from the same t₀ snapshot, so error inside a
    /// window is accumulated deviation rather than per-tick noise. A cheater
    /// riding just under the band gains at most about ε of position per
    /// adjudicated window, which the quantization lattice then mostly erases.
    ///
    /// Samples are expected in tick order; a run of exceeding ticks is broken
    /// by any tick back inside the bands.
    #[must_use]
    pub fn judge(&self, samples: &[TrajectorySample]) -> ToleranceOutcome {
        let mut run_start: Option<Tick> = None;
        let mut run_len: u32 = 0;

        for sample in samples {
            // A single tick far enough out needs no sustain: nothing honest
            // moves eight bands in one tick, so waiting would only delay a
            // verdict that is already certain.
            if self.exceeds_multiple(sample, self.hard_snap_multiple) {
                return ToleranceOutcome::Violation {
                    at: sample.tick,
                    kind: DeviationKind::ContinuousOutOfBand,
                };
            }
            if self.exceeds(sample) {
                run_start.get_or_insert(sample.tick);
                run_len += 1;
                if run_len >= self.sustain_ticks {
                    return ToleranceOutcome::Violation {
                        at: run_start.unwrap_or(sample.tick),
                        kind: DeviationKind::ContinuousOutOfBand,
                    };
                }
            } else {
                run_start = None;
                run_len = 0;
            }
        }
        ToleranceOutcome::Within
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tick: u64, claimed_mm: i64, computed_mm: i64) -> TrajectorySample {
        TrajectorySample {
            tick: Tick::new(tick),
            claimed_pos: QPos {
                x: claimed_mm,
                y: 0,
                z: 0,
            },
            claimed_vel: QVel::default(),
            computed_pos: QPos {
                x: computed_mm,
                y: 0,
                z: 0,
            },
            computed_vel: QVel::default(),
        }
    }

    #[test]
    fn drift_inside_the_band_is_never_a_violation() {
        // Platform float drift lives here. If this ever became a violation,
        // honest players on a different OS would be strikeable for existing.
        let tolerance = Tolerance::default();
        let window: Vec<_> = (0..180).map(|t| sample(t, 0, 9)).collect();
        assert_eq!(tolerance.judge(&window), ToleranceOutcome::Within);
    }

    #[test]
    fn a_brief_excursion_is_absorbed() {
        // Packet loss and a late correction produce short excursions. The
        // sustain window is what keeps them out of the strike pipeline.
        let tolerance = Tolerance::default();
        let mut window: Vec<_> = (0..180).map(|t| sample(t, 0, 0)).collect();
        for (offset, entry) in window.iter_mut().skip(10).take(14).enumerate() {
            *entry = sample(10 + offset as u64, 0, 50);
        }
        assert_eq!(tolerance.judge(&window), ToleranceOutcome::Within);
    }

    #[test]
    fn a_sustained_excursion_is_attributed_to_where_it_began() {
        // An adjudicator quoting the tick the counter tripped would point at a
        // tick where nothing happened; the useful answer is where the
        // trajectories parted.
        let tolerance = Tolerance::default();
        let mut window: Vec<_> = (0..180).map(|t| sample(t, 0, 0)).collect();
        for (offset, entry) in window.iter_mut().skip(40).take(20).enumerate() {
            *entry = sample(40 + offset as u64, 0, 50);
        }
        assert_eq!(
            tolerance.judge(&window),
            ToleranceOutcome::Violation {
                at: Tick::new(40),
                kind: DeviationKind::ContinuousOutOfBand,
            }
        );
    }

    #[test]
    fn a_run_broken_by_one_good_tick_starts_over() {
        // Otherwise an entity oscillating around the band would accumulate a
        // violation it never sustained.
        let tolerance = Tolerance::default();
        let mut window = Vec::new();
        for round in 0..12u64 {
            for offset in 0..14 {
                window.push(sample(round * 15 + offset, 0, 50));
            }
            window.push(sample(round * 15 + 14, 0, 0));
        }
        assert_eq!(tolerance.judge(&window), ToleranceOutcome::Within);
    }

    #[test]
    fn a_hard_snap_needs_no_sustain() {
        // Nothing honest moves eight bands in one tick, so waiting out the
        // sustain window would only delay a certain verdict.
        let tolerance = Tolerance::default();
        let window = vec![sample(0, 0, 0), sample(1, 0, 81), sample(2, 0, 0)];
        assert_eq!(
            tolerance.judge(&window),
            ToleranceOutcome::Violation {
                at: Tick::new(1),
                kind: DeviationKind::ContinuousOutOfBand,
            }
        );
    }

    #[test]
    fn the_band_is_a_combined_magnitude_not_per_axis() {
        // 7 mm on each of three axes is 12.1 mm combined — outside a 10 mm
        // band, even though no single axis is. Judging per-axis would let a
        // cheater take 73% more distance for free by moving diagonally.
        let tolerance = Tolerance::default();
        let diagonal = TrajectorySample {
            tick: Tick::new(0),
            claimed_pos: QPos::default(),
            claimed_vel: QVel::default(),
            computed_pos: QPos { x: 7, y: 7, z: 7 },
            computed_vel: QVel::default(),
        };
        assert!(tolerance.exceeds(&diagonal));
    }

    #[test]
    fn velocity_alone_can_trip_the_band() {
        // Position and velocity are separate terms of the same max; a
        // comparator that only watched position would miss an entity holding
        // position while claiming impossible speed.
        let tolerance = Tolerance::default();
        let fast = TrajectorySample {
            tick: Tick::new(0),
            claimed_pos: QPos::default(),
            claimed_vel: QVel::default(),
            computed_pos: QPos::default(),
            computed_vel: QVel { x: 500, y: 0, z: 0 },
        };
        assert!(tolerance.exceeds(&fast));
    }
}
