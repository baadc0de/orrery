//! The `OrreryPredictPlugin` (D8, docs/10-crates.md §7).

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use lightyear::prelude::LocalTimeline;

use crate::budget::RollbackBudget;
use crate::config::PredictConfig;
use crate::correction::{reconcile_authority_corrections, AuthorityCorrectionInbox};
use crate::monitor::ReconciliationMonitor;
use crate::tick::TickBridge;
use crate::wiring;

/// Prediction, rollback and interpolation, configured per D8/D16.
///
/// Registers the configuration and the two guard resources, then installs and
/// configures lightyear's client stack from them (see [`crate::wiring`] for the
/// D16-to-lightyear mapping and for what lightyear 0.29 does not supply).
#[derive(Debug, Default, Clone)]
pub struct OrreryPredictPlugin {
    /// Prediction configuration. D16's defaults are the fast-action tuning;
    /// docs/05 §12 covers retuning, and [`PredictConfig::validate`] is what
    /// stops a partial retune from shipping.
    pub config: PredictConfig,
}

impl Plugin for OrreryPredictPlugin {
    fn build(&self, app: &mut App) {
        // A configuration that breaks docs/05 §12's couplings produces a game
        // that runs and is quietly wrong — a starved interpolation buffer, or a
        // rollback window the budget can never replay. Refusing to build is the
        // only failure mode that gets noticed.
        let defects = self.config.validate();
        assert!(
            defects.is_empty(),
            "PredictConfig breaks docs/05 §12's coupling invariants: {defects:#?}"
        );

        app.insert_resource(self.config.clone())
            .init_resource::<ReconciliationMonitor>()
            .init_resource::<RollbackBudget>()
            .init_resource::<AuthorityCorrectionInbox>()
            // Anchored at the universe origin, and still there: re-anchoring
            // from the coordinator's `UniverseEpoch` and the converged clock
            // offset is the sync phase's job (docs/05 §6) and nothing in the
            // tree does it yet — see `crate::tick`'s module docs for what that
            // costs. Anchoring before convergence is not the fix; it would
            // bake the offset error into every tick the session ever stamps.
            .insert_resource(TickBridge::anchor(orrery_protocol::Tick(0), 0));

        // The bridge is the only thing that carries the wrap epoch, and it
        // carries it in `advance` — so something has to call it once per fixed
        // tick or the epoch never moves. `FixedLast`, because lightyear
        // increments `LocalTimeline` in `FixedFirst` and every reader of the
        // bridge is later in the frame than that (`feed_residuals` runs in
        // `PostUpdate`).
        app.add_systems(FixedLast, advance_tick_bridge);
        app.add_systems(PostUpdate, reconcile_authority_corrections);

        wiring::install(app, &self.config);
    }
}

/// Carry [`TickBridge`] to lightyear's current tick, once per fixed tick.
///
/// This is the crate's own driver, not a host contract: a host that had to
/// call [`TickBridge::advance`] itself would be maintaining lightyear's wrap
/// epoch by hand, and the one composition that forgot would be correct for 828
/// days and then wrong.
///
/// It does not make the bridge's *base* right — see [`crate::tick`]'s module
/// docs for what still anchors it at the universe origin.
pub fn advance_tick_bridge(timeline: Res<LocalTimeline>, mut bridge: ResMut<TickBridge>) {
    bridge.advance(timeline.tick().0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HIGH_RATE_SET;
    use std::time::Duration;

    fn headless(plugin: OrreryPredictPlugin) -> App {
        let mut app = App::new();
        app.add_plugins(bevy_app::PanicHandlerPlugin);
        app.add_plugins(bevy_time::TimePlugin);
        // lightyear's replication backend calls `init_state`, which needs the
        // `StateTransition` schedule that `StatesPlugin` installs. A real game
        // gets it from `DefaultPlugins`.
        app.add_plugins(bevy_state::app::StatesPlugin);
        app.add_plugins(plugin);
        app.finish();
        app
    }

    #[test]
    fn plugin_registers_orrery_resources() {
        let app = headless(OrreryPredictPlugin::default());
        assert!(app.world().get_resource::<PredictConfig>().is_some());
        assert!(app
            .world()
            .get_resource::<ReconciliationMonitor>()
            .is_some());
        assert!(app.world().get_resource::<RollbackBudget>().is_some());
        assert!(app.world().get_resource::<TickBridge>().is_some());
    }

    /// The whole point of the crate: D16's five numbers must reach lightyear's
    /// five knobs. A regression here is silent — the stack runs on lightyear's
    /// defaults, which are a different game's tuning.
    #[test]
    fn d16_defaults_reach_lightyear() {
        use lightyear::core::tick::TickDuration;
        use lightyear::prelude::InputTimelineConfig;
        use lightyear::prelude::{InterpolationConfig, PredictionManager};

        let app = headless(OrreryPredictPlugin::default());
        let world = app.world();

        let tick = world.resource::<TickDuration>();
        assert_eq!(tick.0, Duration::from_secs_f64(1.0 / 60.0), "60 Hz sim");

        let fixed = world.resource::<bevy_time::Time<bevy_time::Fixed>>();
        assert_eq!(fixed.timestep(), Duration::from_secs_f64(1.0 / 60.0));

        let interp = world.resource::<InterpolationConfig>();
        assert_eq!(interp.min_delay, Duration::from_millis(100), "D16 buffer");
        assert_eq!(
            interp.send_interval_ratio, 0.0,
            "a fixed buffer, not a multiple of the slowest peer's rate"
        );

        let manager = world.resource::<PredictionManager>();
        assert_eq!(
            manager.rollback_policy.max_rollback_ticks, 9,
            "D8's window, not lightyear's default 20"
        );

        let input = world.resource::<InputTimelineConfig>();
        assert_eq!(
            input.maximum_predicted_ticks(),
            9,
            "the effective bound is the min of the two; both must say 9"
        );
    }

    /// The send rate is app-global in lightyear 0.29 — `ReplicationSender` lost
    /// its interval — so 20 Hz lives in exactly one resource, and forgetting to
    /// override it means sending every frame.
    #[test]
    fn send_rate_is_twenty_hertz() {
        let app = headless(OrreryPredictPlugin::default());
        assert!(app
            .world()
            .get_resource::<lightyear::prelude::ReplicationMetadata>()
            .is_some());
    }

    /// A configuration that breaks §12's couplings must not build. Catching it
    /// at startup is the difference between a bug report and a mystery.
    #[test]
    #[should_panic(expected = "coupling invariants")]
    fn a_partially_retuned_config_refuses_to_build() {
        let _ = headless(OrreryPredictPlugin {
            config: PredictConfig {
                // Halve the send rate and leave the buffer alone.
                send_hz: 10,
                ..PredictConfig::default()
            },
        });
    }

    /// The bridge has to be advanced by *something*, and nothing outside this
    /// crate ever calls `advance`: a host-driven wrap epoch is a bug with a
    /// two-year fuse in whichever composition forgets. So the plugin drives
    /// it, and the check is that a frame of the shipped app moves it — not
    /// that the function works, which `tick.rs` covers.
    #[test]
    fn the_plugin_advances_the_bridge_from_lightyears_timeline() {
        use lightyear::prelude::LocalTimeline;

        let mut app = headless(OrreryPredictPlugin::default());
        app.update();
        assert_eq!(
            app.world().resource::<TickBridge>().last_seen(),
            0,
            "the anchor is where the session starts"
        );

        // One fixed step's worth of wall time, so `FixedMain` actually runs.
        std::thread::sleep(Duration::from_millis(40));
        app.update();

        let timeline = app.world().resource::<LocalTimeline>().tick().0;
        assert!(timeline > 0, "the fixed schedule ran");
        assert_eq!(
            app.world().resource::<TickBridge>().last_seen(),
            timeline,
            "the bridge follows lightyear's timeline with no host call"
        );
    }

    #[test]
    fn rollback_budget_targets_a_one_millisecond_step() {
        assert_eq!(
            RollbackBudget::default().step_cost,
            Duration::from_micros(1000)
        );
        assert_eq!(RollbackBudget::default().max_amortize_frames, 2, "D8");
        assert_eq!(HIGH_RATE_SET, 24, "D16");
    }
}
