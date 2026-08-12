//! The reconciliation-error monitor (D10, docs/10-crates.md §7).
//!
//! Prediction error is a free discrepancy signal during interactions: when a
//! predicted entity's reconciled state deviates from what the authority
//! confirmed, the monitor records the error. A *sustained* violation — error
//! beyond the tolerance band for a sustained window — is the witness signal
//! that feeds `orrery_witness` (D10).
//!
//! P1 scope: the per-entity error accumulator and the sustained-violation
//! query. The actual reconciliation feed (from lightyear) lands with the full
//! P1 integration.

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::*;

/// A window of sustained reconciliation error for one entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViolationWindow {
    /// The entity.
    pub entity: Entity,
    /// How long the violation has been sustained.
    pub duration: Duration,
}

/// Per-entity reconciliation error statistics (D10).
#[derive(Debug, Default, Resource)]
pub struct ReconciliationMonitor {
    /// Per-entity accumulated error and how long it has exceeded the band.
    entities: HashMap<Entity, EntityStats>,
}

/// Per-entity accumulation state.
#[derive(Debug, Clone, Copy)]
struct EntityStats {
    /// Accumulated prediction error (arbitrary units, e.g. metres).
    error: f32,
    /// How long the error has exceeded the tolerance band.
    violation_duration: Duration,
}

impl ReconciliationMonitor {
    /// Record a reconciliation error for an entity.
    ///
    /// `error` is the magnitude of the deviation; `dt` is the time since the
    /// last sample. If `error` exceeds `tolerance`, the violation window grows;
    /// otherwise it resets.
    pub fn record(&mut self, entity: Entity, error: f32, dt: Duration, tolerance: f32) {
        let stats = self.entities.entry(entity).or_insert(EntityStats {
            error: 0.0,
            violation_duration: Duration::ZERO,
        });
        stats.error = error;
        if error > tolerance {
            stats.violation_duration += dt;
        } else {
            stats.violation_duration = Duration::ZERO;
        }
    }

    /// Whether `entity` has a sustained violation of at least `min_duration`.
    #[must_use]
    pub fn sustained_violation(
        &self,
        entity: Entity,
        min_duration: Duration,
    ) -> Option<ViolationWindow> {
        let stats = self.entities.get(&entity)?;
        (stats.violation_duration >= min_duration).then_some(ViolationWindow {
            entity,
            duration: stats.violation_duration,
        })
    }

    /// The current error for an entity, if any.
    #[must_use]
    pub fn error(&self, entity: Entity) -> Option<f32> {
        self.entities.get(&entity).map(|s| s.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(n: u64) -> Entity {
        Entity::from_bits(n)
    }

    #[test]
    fn violation_accumulates_and_resets() {
        let mut m = ReconciliationMonitor::default();
        let e = entity(1);
        let dt = Duration::from_millis(100);

        // Below tolerance: no violation.
        m.record(e, 0.5, dt, 1.0);
        assert_eq!(m.sustained_violation(e, Duration::from_millis(200)), None);

        // Above tolerance, accumulates.
        m.record(e, 1.5, dt, 1.0);
        m.record(e, 1.5, dt, 1.0);
        let v = m.sustained_violation(e, Duration::from_millis(150));
        assert!(v.is_some());
        assert_eq!(v.unwrap().entity, e);
        assert_eq!(v.unwrap().duration, Duration::from_millis(200));

        // Recovery resets the window.
        m.record(e, 0.5, dt, 1.0);
        assert_eq!(m.sustained_violation(e, Duration::from_millis(50)), None);
        assert_eq!(m.error(e), Some(0.5));
    }

    #[test]
    fn unknown_entity_has_no_violation() {
        let m = ReconciliationMonitor::default();
        assert_eq!(m.sustained_violation(entity(9), Duration::ZERO), None);
        assert_eq!(m.error(entity(9)), None);
    }
}
