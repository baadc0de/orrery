//! The `OrreryPredictPlugin` — P1 initial config (docs/11-roadmap.md §P1).
//!
//! Registers the prediction configuration and the two guard resources. The
//! lightyear wiring (the plan-B seam) lands with the full P1 integration; this
//! skeleton makes the config and monitor available to the rest of the stack.

use std::time::Duration;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use crate::config::PredictConfig;
use crate::monitor::ReconciliationMonitor;

/// The rollback budget guard (docs/10-crates.md §7).
///
/// Resimulation is amortized over ≤ 2 render frames; this bounds how much
/// simulation work a single frame may spend catching up after a rollback.
#[derive(Debug, Clone, Resource)]
pub struct RollbackBudget {
    /// The per-frame resimulation budget. Default ≈ 1 ms (D16).
    pub resim_budget: Duration,
}

impl Default for RollbackBudget {
    fn default() -> Self {
        Self {
            resim_budget: Duration::from_micros(1000),
        }
    }
}

/// The `orrery_predict` plugin.
#[derive(Default)]
pub struct OrreryPredictPlugin {
    /// Prediction configuration.
    pub config: PredictConfig,
}

impl Plugin for OrreryPredictPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config.clone())
            .init_resource::<ReconciliationMonitor>()
            .init_resource::<RollbackBudget>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::App;

    #[test]
    fn plugin_registers_resources() {
        let mut app = App::new();
        app.add_plugins(OrreryPredictPlugin::default());
        assert!(app.world().get_resource::<PredictConfig>().is_some());
        assert!(app
            .world()
            .get_resource::<ReconciliationMonitor>()
            .is_some());
        assert!(app.world().get_resource::<RollbackBudget>().is_some());
    }

    #[test]
    fn rollback_budget_default_is_one_ms() {
        assert_eq!(
            RollbackBudget::default().resim_budget,
            Duration::from_micros(1000)
        );
    }
}
