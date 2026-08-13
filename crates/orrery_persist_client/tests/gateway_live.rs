//! End-to-end test of the client connect/hello lifecycle against the **real**
//! `orrery_persistd` gateway (docs/11-roadmap.md §P2).
//!
//! This proves the full client → gateway → cell-actor path in-process: the
//! client's `connect_gateway` dials the gateway, `hello_gateway` sends the
//! `Hello`, the gateway's `HelloAck` flips the session to `Connected`, and a
//! diff uplink reaches the cell actor and is acked — no fake gateway.

use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use orrery_net::OrreryNetPlugin;
use orrery_persist_client::{
    GatewayConfig, GatewaySession, GatewayState, OrreryPersistClientPlugin, PersistClientConfig,
    UplinkScheduler,
};
use orrery_persistd::Router;
use orrery_protocol::{DiffUplink, GridId, PersistId, RecordKind, Tick};
use tokio::sync::Mutex;

fn runtime_config(dir: &std::path::Path) -> orrery_persistd::RuntimeConfig {
    orrery_persistd::RuntimeConfig {
        shards: vec![orrery_protocol::CellId::ROOT],
        journal: orrery_persistd::JournalConfig {
            dir: dir.to_path_buf(),
            commit: orrery_persistd::journal::GroupCommitConfig {
                mode: orrery_persistd::journal::AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: orrery_protocol::Epoch::new(0),
        fence: Arc::new(orrery_persistd::MemFenceStore::new()),
    }
}

fn client_app(gateway: &orrery_persistd::GatewayServer) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, OrreryNetPlugin::default()));
    app.add_plugins(OrreryPersistClientPlugin::default());
    app.insert_resource(GatewayConfig::new(gateway.addr(), gateway.id()));
    app
}

/// Drive the app until `condition` holds or the timeout elapses.
fn wait_until(app: &mut App, mut condition: impl FnMut(&mut World) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.update();
        if condition(app.world_mut()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for condition"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn client_connects_hellos_and_uplinks_to_real_gateway() {
    // The gateway server runs on this tokio runtime; Bevy's own IO layer runs on
    // its own internal runtime. A multi-thread runtime lets the gateway's tasks
    // progress while this thread drives the Bevy app.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(
        rt.block_on(async { orrery_persistd::CellRuntime::open(&runtime_config(dir.path())) })
            .unwrap(),
    ));
    let router: Arc<dyn Router> = runtime.clone();
    let server = rt
        .block_on(orrery_persistd::GatewayServer::spawn(
            orrery_persistd::GatewayConfig::default(),
            router,
        ))
        .unwrap();

    let mut app = client_app(&server);

    // The client should dial the gateway and, once the hello is acked, reach
    // `Connected`.
    wait_until(&mut app, |world| {
        world.resource::<GatewaySession>().state == GatewayState::Connected
    });
    {
        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.gateway, Some(server.id()), "gateway id recorded");
        assert!(session.is_connected());
    }

    // Register an entity and queue a diff; drive the scheduler so it sends.
    {
        let mut sched = app.world_mut().resource_mut::<UplinkScheduler>();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(DiffUplink {
            cell: orrery_protocol::CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(1),
            tick: Tick::new(1),
            kind: RecordKind::Spawn,
            payload: bytes::Bytes::from_static(b"hp=100"),
            seq: 1,
        });
    }
    let cfg = app.world().resource::<PersistClientConfig>().clone();
    let diffs = {
        let mut sched = app.world_mut().resource_mut::<UplinkScheduler>();
        sched.flush(&cfg, Duration::from_millis(0));
        sched.flush(&cfg, Duration::from_millis(250))
    };
    assert_eq!(diffs.len(), 1, "scheduler selected the diff");
    {
        let entity = app.world().resource::<GatewaySession>().session.unwrap();
        let mut io = app
            .world_mut()
            .get_mut::<aeronet_io::Session>(entity)
            .unwrap();
        for diff in diffs {
            let msg = orrery_protocol::GatewayMsg::Diff { diff };
            io.send
                .push(bytes::Bytes::from(GatewaySession::encode_datagram(&msg)));
        }
    }

    // The gateway journals it; the actor snapshot reflects the diff.
    wait_until(&mut app, |world| {
        !world
            .resource::<UplinkScheduler>()
            .has_pending(PersistId::new(1))
    });
    {
        let page = rt.block_on(async {
            let rt = runtime.lock().await;
            rt.read(orrery_protocol::CellId::ROOT).await.unwrap()
        });
        let entity = page
            .entities
            .get(&PersistId::new(1))
            .expect("entity journaled");
        assert_eq!(entity.components.as_ref(), b"hp=100");
    }

    // Drop the Bevy app (and its internal runtime) before the gateway, and drop
    // the gateway before the test runtime, all in a synchronous context.
    drop(app);
    rt.block_on(server.shutdown());
}
