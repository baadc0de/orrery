//! End-to-end test of the persist-client pipeline against an in-memory gateway
//! harness (docs/11-roadmap.md §P2).
//!
//! This exercises the three client responsibilities together — the diff uplink
//! scheduler, the area loader, and the intent queue — against a fake gateway
//! that speaks the `orrery_protocol` wire surface. It proves the client can
//! actually feed cell actors (the P2 demo criterion) without a live cluster.

use bevy::prelude::*;
use orrery_persist_client::{
    AreaLoader, GatewaySession, GatewayState, IntentQueue, IntentStatus, OrreryPersistClientPlugin,
    PersistClientConfig, UplinkScheduler,
};
use orrery_protocol::{
    DiffUplink, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome, PersistId,
    RecordKind, Tick,
};

/// A fake gateway: collects the diffs, pages, and intents the client sends.
#[derive(Default)]
struct FakeGateway {
    diffs: Vec<DiffUplink>,
    subscribes: Vec<orrery_protocol::CellId>,
    intents: Vec<Intent>,
}

fn node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn sig() -> orrery_protocol::Signature {
    let seed = [0u8; 32];
    iroh_base::SecretKey::from_bytes(&seed).sign(b"test")
}

fn intent(id: u128) -> Intent {
    Intent {
        intent_id: id,
        issuer: node(1),
        cell_epoch: orrery_protocol::Epoch::new(0),
        ops: vec![IntentOp {
            op: 1,
            args: bytes::Bytes::from_static(b"trade"),
        }],
        attestations: vec![orrery_protocol::Attestation {
            witness: node(2),
            signature: sig(),
        }],
        signature: sig(),
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, OrreryPersistClientPlugin::default()));
    app
}

/// Simulate the client sending its buffered datagrams/streams to the gateway
/// and the gateway replying. This stands in for the aeronet session transport:
/// drains the client's send buffer into the gateway, and pushes the gateway's
/// replies into the client's recv buffer (which `process_replies` consumes).
fn pump(app: &mut App, gateway: &mut FakeGateway) {
    let session_entity = app
        .world()
        .resource::<GatewaySession>()
        .session
        .expect("session connected");
    let mut replies: Vec<bytes::Bytes> = Vec::new();
    {
        let mut session = app
            .world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .expect("session component");
        let sends = std::mem::take(&mut session.send);
        for bytes in sends {
            // Decode the tagged datagram or stream frame.
            let (channel, payload) = orrery_net::channels::untag(&bytes).unwrap();
            match channel {
                orrery_net::channels::Channel::State => {
                    let msg: GatewayMsg = postcard::from_bytes(payload).unwrap();
                    if let GatewayMsg::Diff { diff } = msg {
                        gateway.diffs.push(diff.clone());
                        replies.push(bytes::Bytes::from(GatewaySession::encode_datagram(
                            &GatewayReply::BulkAck {
                                entity: diff.entity,
                                tick: diff.tick,
                                lsn: orrery_protocol::Lsn::new(1, 0),
                                provisional: false,
                            },
                        )));
                    }
                }
                orrery_net::channels::Channel::Control => {
                    let len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
                    let msg: GatewayMsg = postcard::from_bytes(&payload[4..4 + len]).unwrap();
                    match msg {
                        // Subscribe carries the grid (P-7); the driver asserts it stays ROOT in v1.
                        GatewayMsg::Subscribe { grid, cells } => {
                            assert_eq!(grid, GridId::ROOT);
                            gateway.subscribes.extend(cells);
                        }
                        GatewayMsg::SubmitIntent { intent } => {
                            gateway.intents.push(intent.clone());
                            replies.push(GatewaySession::encode_stream(&GatewayReply::IntentAck {
                                intent_id: intent.intent_id,
                                outcome: IntentOutcome::Committed {
                                    tick: Tick::new(9),
                                    minted: vec![],
                                },
                            }));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    // Push the gateway's replies into the client's recv buffer.
    let mut session = app
        .world_mut()
        .get_mut::<aeronet_io::Session>(session_entity)
        .expect("session component");
    for reply in replies {
        session.recv.push(aeronet_io::packet::RecvPacket {
            recv_at: bevy_platform::time::Instant::now(),
            payload: reply,
        });
    }
}

#[test]
fn client_feeds_cell_actors_end_to_end() {
    let mut app = app();
    let mut gateway = FakeGateway::default();

    // Connect the gateway session.
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

    // Register an entity and queue a diff.
    {
        let mut sched = app.world_mut().resource_mut::<UplinkScheduler>();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(DiffUplink {
            cell: orrery_protocol::CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(1),
            tick: Tick::new(1),
            kind: RecordKind::ComponentDiff,
            payload: bytes::Bytes::from_static(b"hp=50"),
            seq: 1,
            lease_id: None,
            authority_seq: None,
        });
    }

    // Submit an intent.
    let ticket = {
        let mut queue = app.world_mut().resource_mut::<IntentQueue>();
        queue.submit(intent(1)).unwrap()
    };

    // Drive the scheduler directly (the real clock barely moves over rapid test
    // updates): the first flush establishes the baseline, the second accrues
    // priority and selects the diff. This proves the scheduler → gateway wire
    // path.
    let cfg = app.world().resource::<PersistClientConfig>().clone();
    let diffs = {
        let mut sched = app.world_mut().resource_mut::<UplinkScheduler>();
        sched.flush(&cfg, std::time::Duration::from_millis(0));
        sched.flush(&cfg, std::time::Duration::from_millis(250))
    };
    assert_eq!(diffs.len(), 1, "the scheduler selected the diff");
    {
        let mut session = app
            .world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .expect("session component");
        for diff in diffs {
            let msg = GatewayMsg::Diff { diff };
            session
                .send
                .push(bytes::Bytes::from(GatewaySession::encode_datagram(&msg)));
        }
    }

    // The intent drains through the Bevy system on the next update.
    app.update();

    // The gateway received the diff and the intent.
    pump(&mut app, &mut gateway);
    assert_eq!(gateway.diffs.len(), 1, "the diff reached the gateway");
    assert_eq!(gateway.diffs[0].entity, PersistId::new(1));
    assert_eq!(gateway.intents.len(), 1, "the intent reached the gateway");

    // Run an update so the client consumes the gateway's replies.
    app.update();

    // The intent is committed (the gateway acked it).
    let status = app.world().resource::<IntentQueue>().status(ticket);
    assert_eq!(status, IntentStatus::Committed(Tick::new(9)));
    // The diff's ack cleared its pending state.
    assert!(!app
        .world()
        .resource::<UplinkScheduler>()
        .has_pending(PersistId::new(1)));
}

#[test]
fn area_load_subscribes_nearest_first() {
    let mut app = app();
    let mut gateway = FakeGateway::default();

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

    // Subscribe to a 27-cell neighborhood.
    let center = orrery_protocol::CellId::from_coords(glam::IVec3::ZERO, 21).unwrap();
    {
        let mut loader = app.world_mut().resource_mut::<AreaLoader>();
        loader.cells = orrery_persist_client::order_nearest_first(center, center.neighbors27());
    }

    // Run updates so the area loader issues the subscribe.
    for _ in 0..5 {
        app.update();
    }
    pump(&mut app, &mut gateway);

    // The gateway received the subscribe, nearest-first (center first).
    assert!(!gateway.subscribes.is_empty());
    assert_eq!(gateway.subscribes[0], center, "center cell requested first");
}
