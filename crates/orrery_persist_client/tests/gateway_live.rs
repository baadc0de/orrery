//! End-to-end test of the client connect/hello lifecycle against the **real**
//! `orrery_persistd` gateway (docs/11-roadmap.md §P2).
//!
//! This proves the full client → gateway → cell-actor path in-process: the
//! client's `connect_gateway` dials the gateway, `hello_gateway` sends the
//! `Hello`, the gateway's `HelloAck` flips the session to `Connected`, and a
//! diff uplink reaches the cell actor and is acked — no fake gateway.

#[path = "../../orrery_persistd/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use orrery_authority::{
    Authority, AuthorityPhase, AuthorityState, LeaseOutbox, OrreryAuthorityPlugin, PersistIdentity,
};
use orrery_net::plugin::NetConfig;
use orrery_net::OrreryNetPlugin;
use orrery_persist_client::{
    GatewayConfig, GatewaySession, GatewayState, OrreryPersistClientPlugin, PersistClientConfig,
    UplinkScheduler,
};
use orrery_persistd::Router;
use orrery_protocol::{
    CellId, ClaimBasis, ClaimKind, DiffUplink, Epoch, GatewayMsg, GridId, JournalRecord, LeaseMsg,
    Lsn, PersistId, RecordKind, Tick,
};
use tokio::sync::Mutex;

fn runtime_config(dir: &std::path::Path) -> orrery_persistd::RuntimeConfig {
    orrery_persistd::RuntimeConfig {
        shards: vec![orrery_protocol::CellId::ROOT],
        grid: GridId::ROOT,
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

fn client_app(gateway: &orrery_persistd::GatewayServer, client_key: &iroh_base::SecretKey) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        OrreryNetPlugin {
            config: NetConfig {
                relay_mode: aeronet_iroh::iroh::RelayMode::Disabled,
                secret_key: Some(client_key.clone()),
            },
        },
    ));
    app.add_plugins(OrreryAuthorityPlugin);
    app.add_plugins(OrreryPersistClientPlugin::default());
    app.insert_resource(GatewayConfig::new(gateway.addr(), gateway.id()));
    app.insert_resource(GatewaySession::new(support::valid_session_token(
        client_key.public(),
    )));
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
        rt.block_on(async {
            orrery_persistd::CellRuntime::open(
                &runtime_config(dir.path()),
                &(Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new())
                    as Arc<dyn orrery_persistd::checkpoint::CheckpointStore>),
            )
            .await
        })
        .unwrap(),
    ));
    let client_key = support::secret(7);
    rt.block_on(async {
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, CellId::ROOT)
            .expect("root actor")
            .clone();
        let payload = bytes::Bytes::from_static(b"seeded");
        actor
            .start_diff(JournalRecord {
                lsn: Lsn::new(0, 0),
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(1),
                tick: Tick::new(0),
                epoch: Epoch::new(0),
                author: client_key.public(),
                kind: RecordKind::Spawn,
                crc: orrery_persistd::payload_crc(&payload),
                payload,
            })
            .await
            .unwrap()
            .committed()
            .await
            .unwrap();
    });
    let router: Arc<dyn Router> = runtime.clone();
    let server = rt
        .block_on(orrery_persistd::GatewayServer::spawn(
            support::authority_config(client_key.public(), GridId::ROOT, vec![CellId::ROOT]),
            router,
        ))
        .unwrap();

    let mut app = client_app(&server, &client_key);

    // The client should dial the gateway and, once the hello is acked, reach
    // `Connected`.
    wait_until(&mut app, |world| {
        world.resource::<GatewaySession>().state == GatewayState::Connected
    });
    {
        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.gateway, Some(server.id()), "gateway id recorded");
        assert!(session.is_connected());
        assert_eq!(
            app.world().resource::<AuthorityState>().node,
            session.node,
            "authority must use the iroh identity bound by Hello"
        );
    }

    // Claim first: strict P3 uplinks must carry the granted fencing pair.
    let entity = app
        .world_mut()
        .spawn((
            PersistIdentity(PersistId::new(1)),
            Authority {
                holder: None,
                seq: Default::default(),
            },
            AuthorityPhase::Remote,
        ))
        .id();
    let claim_id = app
        .world_mut()
        .resource_mut::<AuthorityState>()
        .begin_claim(PersistId::new(1))
        .expect("test claim id space is available");
    app.world_mut()
        .entity_mut(entity)
        .insert(AuthorityPhase::LocalPending { claim_id });
    app.world_mut()
        .resource_mut::<LeaseOutbox>()
        .0
        .push(LeaseMsg::Claim {
            claim_id,
            entity: PersistId::new(1),
            grid: GridId::ROOT,
            cell: orrery_protocol::CellId::ROOT,
            kind: ClaimKind::Weak,
            basis: ClaimBasis::Explicit,
            observed: Default::default(),
            tick: Tick::new(1),
        });
    wait_until(&mut app, |world| {
        matches!(
            world.get::<AuthorityPhase>(entity),
            Some(AuthorityPhase::LocalGranted { .. })
        )
    });
    let (lease_id, authority_seq) = {
        let phase = app.world().get::<AuthorityPhase>(entity).unwrap();
        let AuthorityPhase::LocalGranted { lease_id, .. } = *phase else {
            unreachable!("wait condition requires a grant");
        };
        (lease_id, app.world().get::<Authority>(entity).unwrap().seq)
    };
    assert_eq!(
        app.world().get::<Authority>(entity).unwrap().holder,
        Some(app.world().resource::<GatewaySession>().node),
        "grant ownership is recorded under the authenticated local node"
    );

    // Register an entity and queue a fenced diff; drive the scheduler so it sends.
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
            lease_id: Some(lease_id),
            authority_seq: Some(authority_seq),
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
            let msg = GatewayMsg::Diff { diff };
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
            rt.read(orrery_protocol::GridId::ROOT, orrery_protocol::CellId::ROOT)
                .await
                .unwrap()
        });
        let entity = page
            .entities
            .get(&PersistId::new(1))
            .expect("entity journaled");
        assert_eq!(entity.components.as_ref(), b"hp=100");
    }

    // Given: the real client still holds local-authority markers after its acknowledged write.
    assert!(app
        .world()
        .get::<orrery_authority::LocallyAuthoritative>(entity)
        .is_some());
    let journal_before_nack = rt.block_on(async {
        runtime
            .lock()
            .await
            .journal()
            .scan_from(Lsn::new(0, 0))
            .count()
    });

    // When: its next live uplink presents a stale lease id and receives the actor's current row.
    app.world_mut()
        .resource_mut::<UplinkScheduler>()
        .queue(DiffUplink {
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(1),
            tick: Tick::new(2),
            kind: RecordKind::ComponentDiff,
            payload: bytes::Bytes::from_static(b"must-not-journal"),
            seq: 2,
            lease_id: Some(orrery_protocol::LeaseId(0)),
            authority_seq: Some(authority_seq),
        });
    let stale_diffs = {
        let mut sched = app.world_mut().resource_mut::<UplinkScheduler>();
        sched.flush(&cfg, Duration::from_millis(500))
    };
    assert_eq!(
        stale_diffs.len(),
        1,
        "stale fenced diff reached the live wire"
    );
    {
        let session_entity = app.world().resource::<GatewaySession>().session.unwrap();
        let mut io = app
            .world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .unwrap();
        for diff in stale_diffs {
            io.send
                .push(bytes::Bytes::from(GatewaySession::encode_datagram(
                    &GatewayMsg::Diff { diff },
                )));
        }
    }
    wait_until(&mut app, |world| {
        matches!(
            world.get::<AuthorityPhase>(entity),
            Some(AuthorityPhase::Remote)
        )
    });

    // Then: the lease-bearing NACK revokes every local writer marker and never appends.
    assert!(app
        .world()
        .get::<orrery_authority::LocallyAuthoritative>(entity)
        .is_none());
    assert_eq!(app.world().resource::<UplinkScheduler>().len(), 0);
    assert_eq!(
        rt.block_on(async {
            runtime
                .lock()
                .await
                .journal()
                .scan_from(Lsn::new(0, 0))
                .count()
        }),
        journal_before_nack
    );
    let page = rt.block_on(async {
        runtime
            .lock()
            .await
            .read(GridId::ROOT, CellId::ROOT)
            .await
            .unwrap()
    });
    assert_eq!(
        page.entities[&PersistId::new(1)].components.as_ref(),
        b"hp=100"
    );

    // Drop the Bevy app (and its internal runtime) before the gateway, and drop
    // the gateway before the test runtime, all in a synchronous context.
    drop(app);
    rt.block_on(server.shutdown());
}
