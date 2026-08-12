//! Prediction configuration (D16 defaults, docs/10-crates.md §7).

use std::time::Duration;

use bevy_ecs::prelude::*;

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
}

impl Default for PredictConfig {
    fn default() -> Self {
        Self {
            tick_hz: 60,
            send_hz: 20,
            rollback_ticks: 9,
            interp_buffer: Duration::from_millis(100),
            hit_rewind_cap: Duration::from_millis(200),
        }
    }
}

impl PredictConfig {
    /// The tick duration implied by `tick_hz`.
    #[must_use]
    pub fn tick_duration(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.tick_hz))
    }

    /// The rollback window as a duration (`rollback_ticks` ticks).
    #[must_use]
    pub fn rollback_window(&self) -> Duration {
        self.tick_duration() * u32::from(self.rollback_ticks)
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
    }

    #[test]
    fn rollback_window_is_ticks_times_tick() {
        let cfg = PredictConfig::default();
        assert_eq!(cfg.tick_duration(), Duration::from_secs_f64(1.0 / 60.0));
        // 9 ticks at 60 Hz ≈ 150 ms (within a sub-ms float tolerance).
        let window = cfg.rollback_window().as_millis();
        assert!((150..=151).contains(&window), "window was {window} ms");
    }
}
