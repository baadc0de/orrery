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

use crate::area::{drive_area_loader, sync_aoi_to_loader, AreaLoader};
use crate::config::PersistClientConfig;
use crate::feed::{feed_uplink, UplinkSeq};
use crate::gateway::{
    connect_gateway, disconnect_gateway, flush_interest_grant, flush_lease_control, hello_gateway,
    sync_authority_identity, GatewaySession,
};
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
                (
                    flush_uplink,
                    drain_intents,
                    // The AOI wiring runs first so a crossing updates the
                    // loader's cell set before the driver issues the subscribe
                    // (D16: < 50 ms to first page-in). `sync_aoi_to_loader`
                    // runs even without the optional `orrery_spatial` AOI
                    // resource: with no `Res<AoiSubscription>` in the world it
                    // is a no-op.
                    sync_aoi_to_loader,
                    drive_area_loader,
                )
                    .chain()
                    .in_set(PersistClientSet::Flush),
            )
            .add_systems(
                Update,
                (
                    connect_gateway,
                    hello_gateway,
                    sync_authority_identity,
                    // Interest before lease control: a claim sent in the same
                    // frame as a fresh session's grant would otherwise be
                    // judged against interest the gateway has not seen yet.
                    flush_interest_grant,
                    flush_lease_control,
                    disconnect_gateway,
                ),
            )
            .add_systems(Update, process_replies.before(feed_uplink))
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

    #[test]
    fn aoi_change_drives_one_subscribe_and_evicts_departed_pages() {
        use crate::gateway::GatewayState;
        use orrery_protocol::{CellId, GatewayMsg, PersistId};
        use orrery_spatial::plugin::{AoiSubscription, Cell, LocalPlayer};

        fn cell(x: i32, y: i32, z: i32) -> CellId {
            CellId::from_coords(glam::IVec3::new(x, y, z), CellId::MAX_LEVEL).unwrap()
        }

        let mut app = App::new();
        // Not the full `OrrerySpatialPlugin` — its interest-set and
        // visibility systems need replicon's `ReplicationRegistry`, and the
        // visibility bit is a server-side concern. This test drives the AOI
        // from the player's cell the same way `update_aoi` does, isolating
        // the persist-client wiring under test (`sync_aoi_to_loader` +
        // `drive_area_loader`).
        app.init_resource::<AoiSubscription>()
            // `flush_uplink` needs a `Time` (the plugin pair below provides it).
            .add_plugins((
                bevy_app::TaskPoolPlugin::default(),
                bevy_app::ScheduleRunnerPlugin::default(),
                bevy_time::TimePlugin,
            ))
            .add_systems(
                Update,
                |aoi: ResMut<AoiSubscription>, player: Query<&Cell, With<LocalPlayer>>| {
                    let mut aoi = aoi;
                    let Ok(cell) = player.single() else {
                        return;
                    };
                    let cells = cell.0.neighbors27();
                    if aoi.cells != cells {
                        aoi.cells = cells;
                    }
                },
            )
            .add_plugins(OrreryPersistClientPlugin::default());
        let session_entity = app
            .world_mut()
            .spawn(aeronet_io::Session::new(
                bevy_platform::time::Instant::now(),
                1024,
            ))
            .id();
        {
            let mut session = app.world_mut().resource_mut::<GatewaySession>();
            session.session = Some(session_entity);
            session.state = GatewayState::Connected;
        }
        let player = app
            .world_mut()
            .spawn((LocalPlayer, Cell(cell(0, 0, 0))))
            .id();

        let count_subscribes = |app: &App| -> usize {
            app.world()
                .get::<aeronet_io::Session>(session_entity)
                .unwrap()
                .send
                .iter()
                .filter(|bytes| {
                    let Some((_, payload)) = orrery_net::channels::untag(bytes) else {
                        return false;
                    };
                    let Ok(len) = usize::try_from(u32::from_le_bytes(
                        payload[..4].try_into().unwrap_or_default(),
                    )) else {
                        return false;
                    };
                    matches!(
                        payload
                            .get(4..4 + len)
                            .and_then(|f| postcard::from_bytes::<GatewayMsg>(f).ok()),
                        Some(GatewayMsg::Subscribe { .. })
                    )
                })
                .count()
        };
        let drain_send = |app: &mut App| {
            app.world_mut()
                .get_mut::<aeronet_io::Session>(session_entity)
                .unwrap()
                .send
                .clear();
        };

        // The initial neighborhood: exactly one subscribe.
        app.update();
        assert_eq!(
            count_subscribes(&app),
            1,
            "one subscribe for the initial AOI"
        );
        assert_eq!(
            app.world().resource::<AreaLoader>().cells.len(),
            27,
            "the loader holds the AOI neighborhood"
        );

        // Stationary: no further subscribe (the 50 ms floor is a retry floor,
        // not a trigger — it cannot fire inside one window).
        app.update();
        app.update();
        assert_eq!(count_subscribes(&app), 1, "no resubscribe while stationary");

        // Record pages for the origin neighbourhood, then walk the centre
        // across 5 cells: one subscribe per crossing, and pages whose cell
        // departed are evicted.
        {
            let mut loader = app.world_mut().resource_mut::<AreaLoader>();
            for i in 0..27u64 {
                let cell_id = loader.cells[i as usize];
                loader.record(crate::area::LoadedPage {
                    cell: cell_id,
                    entities: vec![PersistId::new(i + 1)],
                    payloads: vec![bytes::Bytes::from_static(b"x")],
                    live: true,
                });
            }
        }
        assert_eq!(app.world().resource::<AreaLoader>().page_count(), 27);

        for step in 1..=5 {
            drain_send(&mut app);
            app.world_mut().get_mut::<Cell>(player).unwrap().0 = cell(step, 0, 0);
            app.update();
            assert_eq!(
                count_subscribes(&app),
                1,
                "exactly one subscribe for crossing {step}"
            );
            let loader = app.world().resource::<AreaLoader>();
            assert_eq!(loader.cells.len(), 27);
            assert!(
                loader.page_count() <= 27,
                "page_count bounded by the subscription"
            );
            assert!(
                loader.pages.iter().all(|p| loader.cells.contains(&p.cell)),
                "every kept page is inside the current subscription"
            );
        }
        // After 5 crossings east, no page from the origin neighborhood
        // survives; the AOI tracked the walk.
        let loader = app.world().resource::<AreaLoader>();
        assert!(!loader.pages.iter().any(|p| p.cell == cell(0, 0, 0)));
        assert_eq!(
            loader.page_count(),
            0,
            "every origin-neighborhood page departed by crossing 5"
        );
        let aoi = app.world().resource::<AoiSubscription>();
        assert!(aoi.contains(cell(5, 0, 0)));
    }
}
