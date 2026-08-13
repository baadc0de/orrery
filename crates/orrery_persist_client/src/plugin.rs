//! The `OrreryPersistClientPlugin` — the P2 client (docs/11-roadmap.md §P2).
//!
//! Registers the gateway session, the diff uplink scheduler, the area loader,
//! and the intent queue, and wires the flush/drain systems to the gateway
//! session. The replicon change-detection wiring (feeding `DiffUplink`s from
//! replicon diffs) is provided here, and the iroh stream plumbing lands with the
//! full P2 integration; this plugin provides the resources and the
//! transport-agnostic systems on top of the aeronet session.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use crate::area::{drive_area_loader, AreaLoader};
use crate::config::PersistClientConfig;
use crate::feed::{feed_uplink, UplinkSeq};
use crate::gateway::{connect_gateway, disconnect_gateway, hello_gateway, GatewaySession};
use crate::intents::{drain_intents, IntentQueue};
use crate::replies::process_replies;
use crate::uplink::{flush_uplink, UplinkScheduler};

/// The system set for the persist-client systems, so the rest of the stack can
/// order against the uplink flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SystemSet)]
pub enum PersistClientSet {
    /// Flush the uplink scheduler and drain the intent queue to the gateway.
    Flush,
}

/// The `orrery_persist_client` plugin.
#[derive(Default)]
pub struct OrreryPersistClientPlugin {
    /// Client persistence configuration.
    pub config: PersistClientConfig,
}

impl Plugin for OrreryPersistClientPlugin {
    fn build(&self, app: &mut App) {
        let queue =
            IntentQueue::with_store(self.config.queue_capacity, self.config.queue_dir.as_deref());
        app.insert_resource(self.config.clone())
            .init_resource::<GatewaySession>()
            .init_resource::<UplinkScheduler>()
            .init_resource::<AreaLoader>()
            .insert_resource(queue)
            .init_resource::<UplinkSeq>()
            .add_message::<bevy_replicon::server::uplink::ComponentDiff>()
            .configure_sets(Update, PersistClientSet::Flush)
            .add_systems(
                Update,
                (flush_uplink, drain_intents, drive_area_loader).in_set(PersistClientSet::Flush),
            )
            .add_systems(Update, (connect_gateway, hello_gateway, disconnect_gateway))
            .add_systems(Update, process_replies)
            // Feed replicon change-detection diffs into the scheduler before the
            // flush, so the same update flushes what just changed.
            .add_systems(Update, feed_uplink.before(PersistClientSet::Flush));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::App;

    #[test]
    fn plugin_registers_resources() {
        let mut app = App::new();
        app.add_plugins(OrreryPersistClientPlugin::default());
        assert!(app.world().get_resource::<PersistClientConfig>().is_some());
        assert!(app.world().get_resource::<GatewaySession>().is_some());
        assert!(app.world().get_resource::<UplinkScheduler>().is_some());
        assert!(app.world().get_resource::<AreaLoader>().is_some());
        assert!(app.world().get_resource::<IntentQueue>().is_some());
        assert!(app.world().get_resource::<UplinkSeq>().is_some());
    }
}
