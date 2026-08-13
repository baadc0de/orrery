//! Gateway reply processing: consume `GatewayReply`s from the session recv
//! buffer and route them to the uplink scheduler, the area loader, and the
//! intent queue.
//!
//! This closes the client loop: bulk acks/nacks update the scheduler's
//! pending state, area pages fill the loader, and intent acks update the
//! queue. It runs on the aeronet session's recv buffer, which the IO layer
//! drains each update.

use bevy_ecs::prelude::*;
use bevy_platform::time::Instant;

use orrery_protocol::GatewayReply;

use crate::area::{AreaLoader, LoadedPage};
use crate::gateway::{GatewayConfig, GatewaySession, GatewayState};
use crate::intents::IntentQueue;
use crate::uplink::UplinkScheduler;

/// Consume gateway replies from the session recv buffer.
///
/// Runs each update after the IO layer has polled. Decodes each tagged
/// datagram/stream frame and routes it:
///
/// - [`GatewayReply::BulkAck`] / [`GatewayReply::BulkNack`] → the uplink
///   scheduler (the ack is the durability contract, D11 §2.1).
/// - [`GatewayReply::AreaPage`] → the area loader.
/// - [`GatewayReply::IntentAck`] → the intent queue.
pub fn process_replies(
    mut session: ResMut<GatewaySession>,
    config: Option<Res<GatewayConfig>>,
    mut scheduler: ResMut<UplinkScheduler>,
    mut loader: ResMut<AreaLoader>,
    mut queue: ResMut<IntentQueue>,
    mut sessions: Query<&mut aeronet_io::Session>,
) {
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut io) = sessions.get_mut(entity) else {
        return;
    };
    if io.recv.is_empty() {
        return;
    }

    let recv = std::mem::take(&mut io.recv);
    for packet in recv {
        let (channel, payload) = match orrery_net::channels::untag(&packet.payload) {
            Some(pair) => pair,
            None => continue,
        };
        let reply = match channel {
            orrery_net::channels::Channel::State => match postcard::from_bytes(payload) {
                Ok(reply) => reply,
                Err(_) => continue,
            },
            orrery_net::channels::Channel::Control => {
                // Length-prefixed stream frame.
                let Ok(len) = usize::try_from(u32::from_le_bytes(
                    payload[..4].try_into().unwrap_or_default(),
                )) else {
                    continue;
                };
                let Some(frame) = payload.get(4..4 + len) else {
                    continue;
                };
                match postcard::from_bytes(frame) {
                    Ok(reply) => reply,
                    Err(_) => continue,
                }
            }
        };
        match reply {
            GatewayReply::BulkAck {
                entity,
                tick,
                provisional,
                ..
            } => scheduler.on_ack(entity, tick, provisional),
            GatewayReply::BulkNack { entity, tick, .. } => scheduler.on_nack(entity, tick),
            GatewayReply::AreaPage { cell, page } => loader.record(LoadedPage {
                cell,
                entities: page.entities,
                payloads: page.payloads,
                live: page.live,
            }),
            GatewayReply::IntentAck { intent_id, outcome } => {
                queue.on_ack(intent_id, outcome);
            }
            GatewayReply::HelloAck { gateway, protocol } => {
                // The gateway accepted our hello; the session is now up. If the
                // ack names a different gateway than we configured, treat it as
                // a session error: drop back to disconnected so the next frame
                // re-dials (and the IO layer will despawn the stale session).
                let expected = config.as_ref().map(|c| c.gateway);
                if expected.is_some_and(|expected| expected != gateway) {
                    session.state = GatewayState::Disconnected;
                    session.session = None;
                    session.hello_sent = false;
                    session.disconnected_at = Some(Instant::now());
                    continue;
                }
                session.protocol = protocol;
                session.gateway = Some(gateway);
                session.connected_at = Some(Instant::now());
                session.hello_sent = false;
                session.state = GatewayState::Connected;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeronet_iroh::iroh;
    use orrery_protocol::{DiffUplink, GridId, Lsn, PersistId, RecordKind, Tick};

    #[test]
    fn bulk_ack_routes_to_scheduler() {
        let mut scheduler = UplinkScheduler::new();
        scheduler.register(PersistId::new(1), 4.0);
        scheduler.queue(DiffUplink {
            cell: orrery_protocol::CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(1),
            tick: Tick::new(1),
            kind: RecordKind::ComponentDiff,
            payload: bytes::Bytes::from_static(b"hp=50"),
            seq: 1,
        });
        // Simulate the ack arriving in the recv buffer.
        let reply = GatewayReply::BulkAck {
            entity: PersistId::new(1),
            tick: Tick::new(1),
            lsn: Lsn::new(1, 0),
            provisional: false,
        };
        let bytes = GatewaySession::encode_datagram(&reply);
        let mut app = bevy_app::App::new();
        app.insert_resource(GatewaySession::default())
            .insert_resource(scheduler)
            .insert_resource(AreaLoader::default())
            .insert_resource(IntentQueue::default());
        let session_entity = app
            .world_mut()
            .spawn(aeronet_io::Session::new(
                bevy_platform::time::Instant::now(),
                1024,
            ))
            .id();
        app.world_mut().resource_mut::<GatewaySession>().session = Some(session_entity);
        app.world_mut().resource_mut::<GatewaySession>().state =
            crate::gateway::GatewayState::Connected;
        app.world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .unwrap()
            .recv
            .push(aeronet_io::packet::RecvPacket {
                recv_at: bevy_platform::time::Instant::now(),
                payload: bytes::Bytes::from(bytes),
            });
        app.add_systems(bevy_app::Update, process_replies);
        app.update();
        assert!(!app
            .world()
            .resource::<UplinkScheduler>()
            .has_pending(PersistId::new(1)));
    }

    #[test]
    fn hello_ack_from_wrong_gateway_disconnects() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .init_resource::<UplinkScheduler>()
            .init_resource::<AreaLoader>()
            .init_resource::<IntentQueue>();

        // We configured gateway A, but the ack claims to be gateway B.
        let gateway_a = node(1);
        let gateway_b = node(2);
        app.insert_resource(GatewayConfig::new(
            iroh::EndpointAddr::new(gateway_a),
            gateway_a,
        ));

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
            session.state = crate::gateway::GatewayState::Connecting;
        }
        let reply = GatewayReply::HelloAck {
            gateway: gateway_b,
            protocol: 1,
        };
        app.world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .unwrap()
            .recv
            .push(aeronet_io::packet::RecvPacket {
                recv_at: bevy_platform::time::Instant::now(),
                payload: GatewaySession::encode_stream(&reply),
            });

        app.add_systems(bevy_app::Update, process_replies);
        app.update();

        // The session must not be connected; it drops back to disconnected so
        // the next frame re-dials.
        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Disconnected);
        assert!(session.session.is_none());
        assert!(session.gateway.is_none());
    }

    #[test]
    fn hello_ack_from_expected_gateway_connects() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .init_resource::<UplinkScheduler>()
            .init_resource::<AreaLoader>()
            .init_resource::<IntentQueue>();

        let gateway = node(1);
        app.insert_resource(GatewayConfig::new(
            iroh::EndpointAddr::new(gateway),
            gateway,
        ));

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
            session.state = crate::gateway::GatewayState::Connecting;
        }
        let reply = GatewayReply::HelloAck {
            gateway,
            protocol: 7,
        };
        app.world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .unwrap()
            .recv
            .push(aeronet_io::packet::RecvPacket {
                recv_at: bevy_platform::time::Instant::now(),
                payload: GatewaySession::encode_stream(&reply),
            });

        app.add_systems(bevy_app::Update, process_replies);
        app.update();

        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Connected);
        assert_eq!(session.gateway, Some(gateway));
        assert_eq!(session.protocol, 7);
    }

    fn node(n: u8) -> orrery_protocol::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }
}
