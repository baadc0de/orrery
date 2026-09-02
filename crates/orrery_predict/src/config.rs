//! Prediction configuration (D16 defaults, docs/05-prediction-rollback.md §12).
//!
//! The five numbers in [`PredictConfig`] are not five independent dials. D16's
//! defaults are the fast-action configuration of *one coupled system*, and
//! docs/05 §12 states the couplings as invariants rather than as values:
//! the rollback window is ~150 ms of real time whatever the tick rate, the
//! interpolation buffer is exactly two send intervals, the hit-rewind cap
//! covers the interpolation delay plus half a typical round trip, and the
//! resimulation cost of a full window fits two render frames.
//!
//! That is why [`PredictConfig::validate`] exists and why the plugin runs it.
//! A game retuning for a slower pace (§12's 30 Hz and 20 Hz columns) changes
//! several of these together; changing one alone produces a configuration that
//! *runs* and is quietly wrong — an interpolation buffer shorter than two send
//! intervals starves on every second packet, and a rollback window longer than
//! the budget can replay is the spiral of death with extra steps.

use std::time::Duration;

use bevy_ecs::prelude::*;
use orrery_protocol::HitWindow;

/// The bounded high-rate interest set (D16: 24 entities).
///
/// Lives here rather than in `orrery_spatial` because the rollback budget's
/// hysteresis halves *this* number when a machine cannot keep up
/// (docs/05 §3); the selector reads it as its ceiling.
pub const HIGH_RATE_SET: u16 = 24;

/// Prediction and rollback configuration (D8, D16).
#[derive(Debug, Clone, Resource)]
pub struct PredictConfig {
    /// Fixed simulation tick rate in Hz. Default 60 (D16).
    pub tick_hz: u32,
    /// Send rate in Hz, ≤ 30 for small islands. Default 20 (D16).
    pub send_hz: u32,
    /// Rollback window in ticks. Default 9 (~150 ms) (D16).
    pub rollback_ticks: u16,
    /// Interpolation buffer for remote entities. Default 100 ms (D16).
    pub interp_buffer: Duration,
    /// Hit-rewind cap. Default 200 ms (D16).
    pub hit_rewind_cap: Duration,
    /// How many ticks of unacked input every outgoing packet re-carries
    /// (docs/05 §4). Default 20 (~333 ms at 60 Hz). Redundancy is what keeps
    /// the input path free of retransmission round trips, so this is a
    /// *floor* set by the send interval, not a bandwidth dial.
    pub redundant_input_ticks: u16,
    /// RTT to an entity's authority beyond which hit-*presentation* prediction
    /// is disabled (D8; Overwatch's ~220 ms precedent). Default 250 ms.
    pub presentation_cutoff_rtt: Duration,
    /// Assumed render frame time for the budget invariant. Default 16.67 ms
    /// (60 fps) — the figure §12's `step_ms × window ≤ 2 × frame_ms` is stated
    /// against.
    pub frame_time: Duration,
}

impl Default for PredictConfig {
    fn default() -> Self {
        Self {
            tick_hz: 60,
            send_hz: 20,
            rollback_ticks: 9,
            interp_buffer: Duration::from_millis(100),
            hit_rewind_cap: Duration::from_millis(200),
            redundant_input_ticks: 20,
            presentation_cutoff_rtt: Duration::from_millis(250),
            frame_time: Duration::from_nanos(16_666_667),
        }
    }
}

/// A way a [`PredictConfig`] breaks one of docs/05 §12's coupling invariants.
///
/// Each variant carries what was configured *and* what the invariant implies,
/// because the useful half of the message for someone retuning a game is the
/// number they should have written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDefect {
    /// A rate was zero. Everything downstream divides by it.
    ZeroRate {
        /// Which rate.
        what: &'static str,
    },
    /// The send rate exceeds the sim tick rate, which cannot mean anything:
    /// there is no state to send between ticks.
    SendFasterThanTick {
        /// Configured send rate, Hz.
        send_hz: u32,
        /// Configured tick rate, Hz.
        tick_hz: u32,
    },
    /// The rollback window is not ~150 ms of real time
    /// (§12: `ceil(0.15 × tick_rate)`).
    RollbackWindowOffTarget {
        /// Configured window, ticks.
        configured: u16,
        /// What `ceil(0.15 × tick_hz)` implies.
        implied: u16,
    },
    /// The interpolation buffer is not two send intervals (§12: `2 / send_rate`).
    ///
    /// One interval buys immunity to a single lost packet, the second buys
    /// jitter headroom; a shorter buffer spends the loss immunity it was sized
    /// for and shows the player the hole.
    InterpBufferNotTwoSendIntervals {
        /// Configured buffer.
        configured: Duration,
        /// What `2 / send_hz` implies.
        implied: Duration,
    },
    /// The hit-rewind cap is below the interpolation buffer, so the shooter
    /// cannot legally rewind as far as its own view is delayed: every honest
    /// shot would fail the authority's rewind-cap check.
    RewindCapBelowInterpBuffer {
        /// Configured cap.
        configured: Duration,
        /// The interpolation buffer it must at least cover.
        interp_buffer: Duration,
    },
    /// Redundant input resend covers less than two send intervals (§12), so a
    /// single lost packet can drop inputs the authority never sees again.
    RedundancyBelowTwoSendIntervals {
        /// Configured redundancy, ticks.
        configured: u16,
        /// What two send intervals imply, in ticks.
        implied: u16,
    },
    /// A full-window replay at the target step cost does not fit two render
    /// frames (§12: `step_ms × window ≤ 2 × frame_ms`) — the budget guard
    /// would be in its eviction rung on every ordinary rollback.
    WindowExceedsTwoFrames {
        /// Projected worst-case replay cost.
        projected: Duration,
        /// Two render frames.
        allowed: Duration,
    },
}

impl PredictConfig {
    /// The tick duration implied by `tick_hz`.
    #[must_use]
    pub fn tick_duration(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.tick_hz.max(1)))
    }

    /// The send interval implied by `send_hz`.
    #[must_use]
    pub fn send_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.send_hz.max(1)))
    }

    /// The rollback window as a duration (`rollback_ticks` ticks).
    #[must_use]
    pub fn rollback_window(&self) -> Duration {
        self.tick_duration() * u32::from(self.rollback_ticks)
    }

    /// The hit-rewind cap expressed in ticks — the unit the target's authority
    /// checks a [`HitClaim`](orrery_protocol::HitClaim) basis against.
    #[must_use]
    pub fn hit_rewind_ticks(&self) -> u16 {
        let ticks = self.hit_rewind_cap.as_secs_f64() * f64::from(self.tick_hz);
        ticks.round() as u16
    }

    /// The interpolation buffer expressed in ticks.
    #[must_use]
    pub fn interp_ticks(&self) -> u16 {
        (self.interp_buffer.as_secs_f64() * f64::from(self.tick_hz)).round() as u16
    }

    /// Depth of the per-entity prediction history ring.
    ///
    /// The rollback window plus half again for margin, rounded up to a power of
    /// two — which is how docs/05 §1's "16-tick ring" falls out of a 9-tick
    /// window. The margin absorbs an update that arrives a tick or two later
    /// than the window nominally allows without forcing a snap, and that is the
    /// difference between a correction the player never notices and one that
    /// needs error smoothing.
    #[must_use]
    pub fn history_ticks(&self) -> u16 {
        (self.rollback_ticks + self.rollback_ticks / 2).next_power_of_two()
    }

    /// Retained pose-history depth on an authority (docs/05 §7).
    ///
    /// Hit-rewind cap + interpolation buffer + one rollback window of
    /// transit/jitter margin, rounded up to a power of two: 12 + 6 + 9 → 32
    /// ticks (~533 ms) on D16's defaults, the figure docs/05 §7 quotes. An
    /// authority whose ring is shorter than a legal claim's basis has to reject
    /// honest hits, so the margin is a correctness term, not slack.
    #[must_use]
    pub fn pose_history_ticks(&self) -> u16 {
        (self.hit_rewind_ticks() + self.interp_ticks() + self.rollback_ticks).next_power_of_two()
    }

    /// The two figures a hit validator is configured by, as the wire crate's
    /// [`HitWindow`]: the rewind cap and the retained ring depth.
    ///
    /// This is how the numbers reach `orrery_authority`'s pose ring without
    /// either crate depending on the other — the facade reads it here and
    /// hands it to the authority plugin.
    #[must_use]
    pub fn hit_window(&self) -> HitWindow {
        HitWindow::new(self.hit_rewind_ticks(), self.pose_history_ticks())
    }

    /// Check docs/05 §12's coupling invariants.
    ///
    /// Returns every defect rather than the first, because a retune that broke
    /// one coupling has usually broken two, and reporting them one build at a
    /// time is how a game ends up shipping the third.
    #[must_use]
    pub fn validate(&self) -> Vec<ConfigDefect> {
        let mut out = Vec::new();

        if self.tick_hz == 0 {
            out.push(ConfigDefect::ZeroRate { what: "tick_hz" });
        }
        if self.send_hz == 0 {
            out.push(ConfigDefect::ZeroRate { what: "send_hz" });
        }
        if !out.is_empty() {
            // Every check below divides by one of these.
            return out;
        }

        if self.send_hz > self.tick_hz {
            out.push(ConfigDefect::SendFasterThanTick {
                send_hz: self.send_hz,
                tick_hz: self.tick_hz,
            });
        }

        let implied_window = (f64::from(self.tick_hz) * 0.15).ceil() as u16;
        if self.rollback_ticks != implied_window {
            out.push(ConfigDefect::RollbackWindowOffTarget {
                configured: self.rollback_ticks,
                implied: implied_window,
            });
        }

        let implied_interp = self.send_interval() * 2;
        // Compare on whole milliseconds: `2 / send_hz` is not exactly
        // representable, and a sub-millisecond difference is not a defect.
        if self.interp_buffer.as_millis() != implied_interp.as_millis() {
            out.push(ConfigDefect::InterpBufferNotTwoSendIntervals {
                configured: self.interp_buffer,
                implied: implied_interp,
            });
        }

        if self.hit_rewind_cap < self.interp_buffer {
            out.push(ConfigDefect::RewindCapBelowInterpBuffer {
                configured: self.hit_rewind_cap,
                interp_buffer: self.interp_buffer,
            });
        }

        let implied_redundancy = ((2.0 / f64::from(self.send_hz)) * f64::from(self.tick_hz)).ceil();
        let implied_redundancy = implied_redundancy as u16;
        if self.redundant_input_ticks < implied_redundancy {
            out.push(ConfigDefect::RedundancyBelowTwoSendIntervals {
                configured: self.redundant_input_ticks,
                implied: implied_redundancy,
            });
        }

        // §12's step budget: one predicted-subset step targets ≈ 1 ms, and a
        // full-window replay must fit two render frames.
        let projected = Duration::from_millis(1) * u32::from(self.rollback_ticks);
        let allowed = self.frame_time * 2;
        if projected > allowed {
            out.push(ConfigDefect::WindowExceedsTwoFrames { projected, allowed });
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_d16() {
        let cfg = PredictConfig::default();
        assert_eq!(cfg.tick_hz, 60);
        assert_eq!(cfg.send_hz, 20);
        assert_eq!(cfg.rollback_ticks, 9);
        assert_eq!(cfg.interp_buffer, Duration::from_millis(100));
        assert_eq!(cfg.hit_rewind_cap, Duration::from_millis(200));
        assert_eq!(cfg.redundant_input_ticks, 20);
    }

    #[test]
    fn rollback_window_is_ticks_times_tick() {
        let cfg = PredictConfig::default();
        assert_eq!(cfg.tick_duration(), Duration::from_secs_f64(1.0 / 60.0));
        // 9 ticks at 60 Hz ≈ 150 ms (within a sub-ms float tolerance).
        let window = cfg.rollback_window().as_millis();
        assert!((150..=151).contains(&window), "window was {window} ms");
    }

    /// The D16 defaults are the configuration everything else is specified
    /// against; if they ever fail their own invariants the invariants are
    /// wrong, and a build should say so before a game inherits them.
    #[test]
    fn d16_defaults_satisfy_every_invariant() {
        assert_eq!(PredictConfig::default().validate(), vec![]);
    }

    /// docs/05 §12's three retuning columns are worked examples; they must
    /// pass the same checks the defaults do, or §12 is advice the code
    /// rejects.
    #[test]
    fn doc_tuning_columns_validate() {
        let mid = PredictConfig {
            tick_hz: 30,
            send_hz: 15,
            rollback_ticks: 5,
            interp_buffer: Duration::from_nanos(133_333_333),
            hit_rewind_cap: Duration::from_millis(250),
            redundant_input_ticks: 10,
            frame_time: Duration::from_nanos(16_666_667),
            ..PredictConfig::default()
        };
        assert_eq!(mid.validate(), vec![], "30 Hz column");

        let slow = PredictConfig {
            tick_hz: 20,
            send_hz: 10,
            rollback_ticks: 3,
            interp_buffer: Duration::from_millis(200),
            hit_rewind_cap: Duration::from_millis(250),
            redundant_input_ticks: 6,
            frame_time: Duration::from_nanos(16_666_667),
            ..PredictConfig::default()
        };
        assert_eq!(slow.validate(), vec![], "20 Hz column");
    }

    /// The classic retune mistake: drop the send rate for bandwidth and leave
    /// the interpolation buffer at the old value. It runs, and every second
    /// lost packet shows.
    #[test]
    fn lowering_send_rate_without_widening_interp_buffer_is_a_defect() {
        let cfg = PredictConfig {
            send_hz: 10,
            ..PredictConfig::default()
        };
        assert!(cfg
            .validate()
            .iter()
            .any(|d| matches!(d, ConfigDefect::InterpBufferNotTwoSendIntervals { .. })));
    }

    /// A window widened past the two-frame budget is the spiral of death
    /// configured in, and it must be caught at startup rather than at the
    /// first bad connection.
    #[test]
    fn window_beyond_two_frames_is_a_defect() {
        let cfg = PredictConfig {
            tick_hz: 240,
            send_hz: 30,
            rollback_ticks: 36,
            interp_buffer: Duration::from_nanos(66_666_667),
            redundant_input_ticks: 16,
            ..PredictConfig::default()
        };
        assert!(cfg
            .validate()
            .iter()
            .any(|d| matches!(d, ConfigDefect::WindowExceedsTwoFrames { .. })));
    }

    /// A rewind cap under the interpolation delay rejects every honest shot,
    /// because the shooter's own view is already further back than the cap.
    #[test]
    fn rewind_cap_under_interp_buffer_is_a_defect() {
        let cfg = PredictConfig {
            hit_rewind_cap: Duration::from_millis(50),
            ..PredictConfig::default()
        };
        assert!(cfg
            .validate()
            .iter()
            .any(|d| matches!(d, ConfigDefect::RewindCapBelowInterpBuffer { .. })));
    }

    /// A zero rate must be reported as itself and must not produce a cascade
    /// of derived nonsense from dividing by it.
    #[test]
    fn zero_rate_short_circuits_the_derived_checks() {
        let cfg = PredictConfig {
            send_hz: 0,
            ..PredictConfig::default()
        };
        assert_eq!(
            cfg.validate(),
            vec![ConfigDefect::ZeroRate { what: "send_hz" }]
        );
    }

    /// The two ring depths docs/05 states as figures — a 16-tick prediction
    /// ring and a 32-tick pose ring — must fall out of the defaults rather
    /// than being written down twice.
    #[test]
    fn ring_depths_match_the_documented_figures() {
        let cfg = PredictConfig::default();
        assert_eq!(cfg.history_ticks(), 16, "docs/05 §1");
        assert_eq!(cfg.hit_rewind_ticks(), 12, "200 ms at 60 Hz");
        assert_eq!(cfg.pose_history_ticks(), 32, "docs/05 §7");
        assert_eq!(cfg.hit_window(), HitWindow::new(12, 32));
    }
}
