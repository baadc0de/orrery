//! Client persistence configuration (D16 defaults).

use std::ops::RangeInclusive;

use bevy_ecs::prelude::*;

/// Configuration for the [`OrreryPersistClientPlugin`](crate::OrreryPersistClientPlugin).
///
/// Defaults are the D16 parameter-table values (docs/DECISIONS.md D16).
#[derive(Debug, Clone, Resource)]
pub struct PersistClientConfig {
    /// The per-entity diff uplink rate range in Hz. Default `1.0..=4.0` (D16).
    ///
    /// The scheduler drives each locally-authoritative entity at a rate in this
    /// range, nearest entities fastest, so the aggregate uplink stays within
    /// the ≤ 1 Mbps peer budget (D6).
    pub uplink_hz: RangeInclusive<f32>,
    /// The per-entity diff priority accumulator: how much priority an entity
    /// accrues per second when it has unacked changes. Higher values send
    /// sooner. Default 1.0.
    pub priority_gain: f32,
    /// The byte budget per uplink flush, in bytes. Default 1024 (one QUIC
    /// datagram at the aeronet IP MTU).
    pub flush_budget_bytes: usize,
    /// The area-load page ordering: cells are requested nearest-first, and this
    /// is the maximum number of cells requested in one subscribe round.
    /// Default 27 (the full 3×3×3 AOI).
    pub area_cells_per_round: usize,
    /// The offline intent queue capacity. When full, new intents are rejected
    /// rather than dropped. Default 4096.
    pub queue_capacity: usize,
    /// Directory for the disk-backed offline intent queue (netsplit posture,
    /// D12). `None` (default) keeps the queue in memory only, lost on process
    /// exit. With the `disk-queue` feature, queued intents are appended here so
    /// they survive a crash and replay on the next run (idempotency keys make
    /// replay safe).
    pub queue_dir: Option<std::path::PathBuf>,
}

impl Default for PersistClientConfig {
    fn default() -> Self {
        Self {
            uplink_hz: 1.0..=4.0,
            priority_gain: 1.0,
            flush_budget_bytes: 1024,
            area_cells_per_round: 27,
            queue_capacity: 4096,
            queue_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_d16() {
        let cfg = PersistClientConfig::default();
        assert_eq!(cfg.uplink_hz, 1.0..=4.0);
        assert_eq!(cfg.priority_gain, 1.0);
        assert_eq!(cfg.flush_budget_bytes, 1024);
        assert_eq!(cfg.area_cells_per_round, 27);
        assert_eq!(cfg.queue_capacity, 4096);
        assert!(cfg.queue_dir.is_none());
    }
}
