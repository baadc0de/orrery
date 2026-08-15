//! Gateway reply processing: consume `GatewayReply`s from the session recv
//! buffer and route them to the uplink scheduler, the area loader, and the
//! intent queue.
//!
//! This closes the client loop: bulk acks/nacks update the scheduler's
//! pending state, area pages fill the loader, and intent acks update the
//! queue. It runs on the aeronet session's recv buffer, which the IO layer
//! drains each update.

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_platform::time::Instant;

use orrery_authority::{
    Authority, AuthorityEvent, AuthorityPhase, AuthorityState, LeaseInbox, LocallyAuthoritative,
    PersistIdentity,
};
use orrery_protocol::{GatewayReply, Lease, PersistId};

use crate::area::AreaLoader;
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
pub(crate) fn process_replies(mut context: ReplyProcessingContext) {
    let Some(entity) = context.session.session else {
        return;
    };
    let Ok(mut io) = context.sessions.get_mut(entity) else {
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
            } => context.scheduler.on_ack(entity, tick, provisional),
            GatewayReply::BulkNack {
                entity,
                tick,
                lease,
                ..
            } => {
                context.scheduler.on_nack(entity, tick);
                if let Some(current) = lease {
                    reconcile_lease_nack(
                        &mut context.commands,
                        &mut context.scheduler,
                        context.authority_state.as_deref_mut(),
                        context.authority_events.as_deref_mut(),
                        &context.identities,
                        entity,
                        &current,
                    );
                }
            }
            GatewayReply::AreaPage { page, .. } => context.loader.record_frame(page),
            GatewayReply::AreaLoadError { cell, kind } => {
                // A failed scan is diagnosable (distinct from an empty cell);
                // the retry floor re-requests the cell set.
                tracing::warn!(?cell, kind, "gateway: area-load cell failed");
            }
            GatewayReply::IntentAck { intent_id, outcome } => {
                context.queue.on_ack(intent_id, outcome);
            }
            GatewayReply::HelloAck { gateway, protocol } => {
                // The gateway accepted our hello; the session is now up. If the
                // ack names a different gateway than we configured, treat it as
                // a session error: drop back to disconnected so the next frame
                // re-dials (and the IO layer will despawn the stale session).
                let expected = context.config.as_ref().map(|c| c.gateway);
                if expected.is_some_and(|expected| expected != gateway) {
                    context.session.state = GatewayState::Disconnected;
                    context.session.session = None;
                    context.session.hello_sent = false;
                    context.session.disconnected_at = Some(Instant::now());
                    continue;
                }
                context.session.protocol = protocol;
                context.session.gateway = Some(gateway);
                context.session.connected_at = Some(Instant::now());
                context.session.hello_sent = false;
                context.session.state = GatewayState::Connected;
            }
            GatewayReply::Lease { message } => {
                if let Some(inbox) = &mut context.lease_inbox {
                    inbox.0.push(message);
                }
            }
        }
    }
}

#[derive(SystemParam)]
pub(crate) struct ReplyProcessingContext<'w, 's> {
    commands: Commands<'w, 's>,
    session: ResMut<'w, GatewaySession>,
    config: Option<Res<'w, GatewayConfig>>,
    scheduler: ResMut<'w, UplinkScheduler>,
    loader: ResMut<'w, AreaLoader>,
    queue: ResMut<'w, IntentQueue>,
    lease_inbox: Option<ResMut<'w, LeaseInbox>>,
    authority_state: Option<ResMut<'w, AuthorityState>>,
    authority_events: Option<ResMut<'w, Messages<AuthorityEvent>>>,
    identities: Query<'w, 's, (Entity, &'static PersistIdentity)>,
    sessions: Query<'w, 's, &'static mut aeronet_io::Session>,
}

fn reconcile_lease_nack(
    commands: &mut Commands,
    scheduler: &mut UplinkScheduler,
    authority_state: Option<&mut AuthorityState>,
    authority_events: Option<&mut Messages<AuthorityEvent>>,
    identities: &Query<(Entity, &PersistIdentity)>,
    persisted: PersistId,
    current: &Lease,
) {
    scheduler.unregister(persisted);
    let revoked = authority_state.and_then(|state| state.revoke_local_lease(persisted));
    let Some((entity, _)) = identities
        .iter()
        .find(|(_, identity)| identity.0 == persisted)
    else {
        return;
    };
    commands
        .entity(entity)
        .remove::<LocallyAuthoritative>()
        .insert((
            Authority {
                holder: current.holder,
                seq: current.seq,
            },
            AuthorityPhase::Remote,
        ));
    if let (Some(lease_id), Some(events)) = (revoked, authority_events) {
        events.write(AuthorityEvent::Lost { entity, lease_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use aeronet_iroh::iroh;
    use bytes::Bytes;
    use orrery_authority::{
        Authority, AuthorityPhase, LeaseInbox, LocallyAuthoritative, OrreryAuthorityPlugin,
        PersistIdentity,
    };
    use orrery_protocol::{
        ClaimId, DiffUplink, GridId, Lease, LeaseFlags, LeaseId, Lsn, PersistId, RecordKind,
        SeqPair, Tick,
    };

    fn begin_claim(app: &mut bevy_app::App, entity: Entity, persisted: PersistId) -> ClaimId {
        let claim_id = app
            .world_mut()
            .resource_mut::<AuthorityState>()
            .begin_claim(persisted)
            .expect("test claim id space is available");
        app.world_mut()
            .entity_mut(entity)
            .insert(AuthorityPhase::LocalPending { claim_id });
        claim_id
    }

    /// An app with the reply path wired and a connected session entity.
    fn reply_app() -> (bevy_app::App, bevy_ecs::entity::Entity) {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .init_resource::<UplinkScheduler>()
            .init_resource::<AreaLoader>()
            .init_resource::<IntentQueue>();
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
        app.add_systems(bevy_app::Update, process_replies);
        (app, session_entity)
    }

    fn push_reply(app: &mut bevy_app::App, session_entity: bevy_ecs::entity::Entity, bytes: Bytes) {
        app.world_mut()
            .get_mut::<aeronet_io::Session>(session_entity)
            .unwrap()
            .recv
            .push(aeronet_io::packet::RecvPacket {
                recv_at: bevy_platform::time::Instant::now(),
                payload: bytes,
            });
    }

    #[test]
    fn oversized_cell_arrives_intact() {
        // One cell with 200 entities × 256-byte bags, chunked by hand exactly
        // as the gateway does (sequenced frames under the 1100-byte datagram
        // budget, orrery_protocol::MAX_AREA_PAGE_FRAME_BYTES): the client's
        // `LoadedPage` for the cell must hold all 200 PersistIds.
        let cell = orrery_protocol::CellId::ROOT;
        let bag = bytes::Bytes::from(vec![0xAB; 256]);
        let mut frames: Vec<orrery_protocol::AreaPage> = Vec::new();
        let mut chunk: Vec<u64> = Vec::new();
        for i in 0..200u64 {
            chunk.push(i);
            // 4 entities × (8 B id + ~266 B bag) ≈ 1096 B — just under the
            // budget once the framing overhead is added; 5 would exceed it.
            if chunk.len() == 4 {
                frames.push(chunk_frame(cell, frames.len() as u32, &chunk, &bag));
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            let index = frames.len() as u32;
            frames.push(chunk_frame(cell, index, &chunk, &bag));
        }
        assert_eq!(frames.len(), 50, "200 entities / 4 per chunk");
        // Every chunk carries the total; every encoded frame is under the
        // budget.
        let total = frames.len() as u32;
        for frame in &mut frames {
            frame.total_chunks = total;
            let encoded = GatewaySession::encode_stream(&GatewayReply::AreaPage {
                cell,
                page: frame.clone(),
            });
            assert!(
                encoded.len() <= orrery_protocol::MAX_AREA_PAGE_FRAME_BYTES,
                "chunk {} is {} B",
                frame.chunk_index,
                encoded.len()
            );
        }

        let (mut app, session_entity) = reply_app();
        // Deliver the frames out of order (the lane is unreliable and
        // unordered, D3): reassembly must hold partials in any arrival order
        // and complete on the full set.
        let mut shuffled = frames.clone();
        shuffled.reverse();
        for frame in shuffled {
            push_reply(
                &mut app,
                session_entity,
                GatewaySession::encode_stream(&GatewayReply::AreaPage { cell, page: frame }),
            );
        }
        app.update();

        let loader = app.world().resource::<AreaLoader>();
        assert_eq!(loader.page_count(), 1, "the chunked page reassembled");
        let page = &loader.pages[0];
        assert_eq!(page.cell, cell);
        let mut ids: Vec<u64> = page.entities.iter().map(|p| p.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 200, "all 200 PersistIds arrived intact");
        assert_eq!(page.payloads.len(), 200);
        assert!(page.payloads.iter().all(|p| p.len() == 256));
    }

    fn chunk_frame(
        cell: orrery_protocol::CellId,
        index: u32,
        ids: &[u64],
        bag: &Bytes,
    ) -> orrery_protocol::AreaPage {
        orrery_protocol::AreaPage {
            cell,
            page_seq: 1,
            chunk_index: index,
            // Set by the caller once the chunk count is known.
            total_chunks: 0,
            entities: ids.iter().map(|&id| PersistId::new(id)).collect(),
            payloads: ids.iter().map(|_| bag.clone()).collect(),
            live: true,
        }
    }

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
            lease_id: None,
            authority_seq: None,
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
    fn lease_bearing_nack_revokes_stale_writer_and_allows_reclaim() {
        // Given: this client has a fenced local grant and one queued write.
        let (mut app, session_entity) = reply_app();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(17);
        let local_entity = app
            .world_mut()
            .spawn((
                PersistIdentity(persisted),
                Authority {
                    holder: None,
                    seq: SeqPair::default(),
                },
                AuthorityPhase::Remote,
            ))
            .id();
        let claim_id = begin_claim(&mut app, local_entity, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(orrery_protocol::LeaseMsg::Grant {
                claim_id,
                entity: persisted,
                lease_id: LeaseId(5),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        app.world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .clear();
        app.world_mut()
            .resource_mut::<UplinkScheduler>()
            .register(persisted, 4.0);
        app.world_mut()
            .resource_mut::<UplinkScheduler>()
            .queue(test_diff(persisted, Tick::new(3)));
        let cfg = crate::config::PersistClientConfig::default();
        let sent = {
            let mut scheduler = app.world_mut().resource_mut::<UplinkScheduler>();
            scheduler.flush(&cfg, Duration::ZERO);
            scheduler.flush(&cfg, Duration::from_millis(250))
        };
        assert_eq!(
            sent.len(),
            1,
            "the stale fence entered in-flight bookkeeping"
        );
        assert_eq!(
            app.world()
                .resource::<UplinkScheduler>()
                .in_flight_bookkeeping_len(),
            (1, 1)
        );

        let current = Lease {
            entity: persisted,
            holder: Some(node(8)),
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
            lease_id: LeaseId(6),
            expires_at: 99,
            flags: LeaseFlags::default(),
        };

        // When: the registrar rejects the stale write and returns its current
        // lease row.
        push_reply(
            &mut app,
            session_entity,
            Bytes::from(GatewaySession::encode_datagram(&GatewayReply::BulkNack {
                entity: persisted,
                tick: Tick::new(3),
                reason: 1,
                lease: Some(current.clone()),
            })),
        );
        app.update();

        // Then: no stale fenced write remains eligible, and the carried row
        // becomes the entity's remote reconciliation state.
        assert!(app
            .world()
            .get::<LocallyAuthoritative>(local_entity)
            .is_none());
        assert_eq!(
            app.world().get::<AuthorityPhase>(local_entity),
            Some(&AuthorityPhase::Remote)
        );
        let authority = app
            .world()
            .get::<Authority>(local_entity)
            .expect("local entity keeps its authority component");
        assert_eq!(authority.holder, current.holder);
        assert_eq!(authority.seq, current.seq);
        let scheduler = app.world().resource::<UplinkScheduler>();
        assert!(scheduler.is_empty(), "stale writer has no queued fence");
        assert_eq!(scheduler.in_flight_bookkeeping_len(), (0, 0));
        let events: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .drain()
            .collect();
        assert!(matches!(
            events.as_slice(),
            [AuthorityEvent::Lost {
                entity: lost_entity,
                lease_id: LeaseId(5),
            }] if *lost_entity == local_entity
        ));

        let reclaim_id = begin_claim(&mut app, local_entity, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(orrery_protocol::LeaseMsg::Grant {
                claim_id: reclaim_id,
                entity: persisted,
                lease_id: LeaseId(7),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 2,
                },
                ttl_ms: 10_000,
                prev_holder: current.holder,
            });
        app.update();
        assert!(matches!(
            app.world().get::<AuthorityPhase>(local_entity),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(7),
                ..
            })
        ));
        assert!(app
            .world()
            .get::<LocallyAuthoritative>(local_entity)
            .is_some());
    }

    #[test]
    fn lease_less_nack_keeps_local_authority() {
        // Given: a granted local writer with a queued diff.
        let (mut app, session_entity) = reply_app();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(18);
        let local_entity = app
            .world_mut()
            .spawn((
                PersistIdentity(persisted),
                Authority {
                    holder: None,
                    seq: SeqPair::default(),
                },
                AuthorityPhase::Remote,
            ))
            .id();
        let claim_id = begin_claim(&mut app, local_entity, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(orrery_protocol::LeaseMsg::Grant {
                claim_id,
                entity: persisted,
                lease_id: LeaseId(8),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        app.world_mut()
            .resource_mut::<UplinkScheduler>()
            .register(persisted, 4.0);
        app.world_mut()
            .resource_mut::<UplinkScheduler>()
            .queue(test_diff(persisted, Tick::new(4)));

        // When: the nack has no lease row (for example, an invariant failure).
        push_reply(
            &mut app,
            session_entity,
            Bytes::from(GatewaySession::encode_datagram(&GatewayReply::BulkNack {
                entity: persisted,
                tick: Tick::new(4),
                reason: 2,
                lease: None,
            })),
        );
        app.update();

        // Then: existing nack behavior only drops that diff; it does not
        // revoke the still-current local grant.
        assert!(app
            .world()
            .get::<LocallyAuthoritative>(local_entity)
            .is_some());
        assert!(matches!(
            app.world().get::<AuthorityPhase>(local_entity),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(8),
                ..
            })
        ));
        let scheduler = app.world().resource::<UplinkScheduler>();
        assert_eq!(scheduler.len(), 1);
        assert!(!scheduler.has_pending(persisted));
    }

    fn test_diff(entity: PersistId, tick: Tick) -> DiffUplink {
        DiffUplink {
            cell: orrery_protocol::CellId::ROOT,
            grid: GridId::ROOT,
            entity,
            tick,
            kind: RecordKind::ComponentDiff,
            payload: Bytes::from_static(b"hp=50"),
            seq: 3,
            lease_id: Some(LeaseId(5)),
            authority_seq: Some(SeqPair::default()),
        }
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
