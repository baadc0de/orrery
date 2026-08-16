//! Gateway reply processing: consume `GatewayReply`s from the session recv
//! buffer and route them to the uplink scheduler, the area loader, and the
//! intent queue.
//!
//! This closes the client loop: bulk acks/nacks update the scheduler's
//! pending state, area pages fill the loader, and intent acks update the
//! queue. It runs on both of the session's inbound buffers — the datagram
//! lane's and the reliable lane's — which the IO layer fills each update.

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_platform::time::Instant;

use orrery_authority::{
    Authority, AuthorityEvent, AuthorityPhase, AuthorityState, InterestGrant, LeaseInbox,
    LocallyAuthoritative, PersistIdentity,
};
use std::collections::HashMap;

use orrery_protocol::{GatewayReply, Lease, LeaseId, PersistId};

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

    // Both lanes are drained into one list before anything is decoded, so the
    // routing below is written once and does not care which lane a reply came
    // in on. Datagrams first: a bulk ack that arrived in the same update as an
    // area page is the older of the two by construction, since the page's lane
    // is the one that waits for retransmits.
    let mut inbound: Vec<bytes::Bytes> = Vec::new();
    if let Ok(mut io) = context.sessions.get_mut(entity) {
        inbound.extend(std::mem::take(&mut io.recv).into_iter().map(|p| p.payload));
    }
    if let Ok(mut streams) = context.streams.get_mut(entity) {
        inbound.extend(
            std::mem::take(&mut streams.recv)
                .into_iter()
                .map(|m| m.payload),
        );
    }
    if inbound.is_empty() {
        return;
    }

    // Rows adopted while draining this batch. Component writes are queued as
    // commands and land after the system returns, so within one drain the
    // `Authority` the query reads is still the pre-batch one — without this,
    // two rows for the same entity in one batch would resolve last-writer-wins
    // instead of newest-fence-wins, which is exactly the reordering the lane
    // produces.
    let mut adopted: HashMap<PersistId, LeaseId> = HashMap::new();

    for packet in inbound {
        let Some(reply) = decode_reply(&packet) else {
            continue;
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
                        &mut adopted,
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
                    end_session(&mut context.session);
                    continue;
                }
                // Acceptance is mutual. A gateway outside this client's
                // `{V, V−1}` window speaks a wire surface this build cannot
                // read, and storing its version would leave the session
                // nominally up while every later message was decoded against
                // the wrong shape.
                if !orrery_protocol::GatewayMsg::protocol_accepted(
                    orrery_protocol::PROTOCOL_VERSION,
                    protocol,
                ) {
                    tracing::warn!(
                        protocol,
                        ours = orrery_protocol::PROTOCOL_VERSION,
                        "gateway: hello ack names an unsupported protocol version"
                    );
                    end_session(&mut context.session);
                    continue;
                }
                context.session.protocol = protocol;
                context.session.gateway = Some(gateway);
                context.session.connected_at = Some(Instant::now());
                context.session.hello_sent = false;
                context.session.state = GatewayState::Connected;
            }
            GatewayReply::HelloRefused {
                gateway,
                protocol,
                reason,
            } => {
                // The gateway said no. Re-dialing with the same version would
                // only repeat the refusal, but the session lifecycle owns the
                // backoff, so this reports the skew and ends the session the
                // same way every other bootstrap failure does.
                tracing::warn!(
                    %gateway,
                    protocol,
                    reason,
                    ours = orrery_protocol::PROTOCOL_VERSION,
                    "gateway: refused the session"
                );
                end_session(&mut context.session);
            }
            GatewayReply::Lease { message } => {
                if let Some(inbox) = &mut context.lease_inbox {
                    inbox.0.push(message);
                }
            }
            GatewayReply::InterestAck { epoch, reason } => {
                if let Some(interest) = &mut context.interest {
                    interest.accepted_epoch = epoch;
                    interest.last_reason = reason;
                    if epoch.is_some() {
                        interest.pending = false;
                    }
                }
                if epoch.is_none() {
                    // Without this the peer would only learn its interest was
                    // refused by watching claims fail as `NotEligible`.
                    tracing::warn!(
                        reason,
                        "gateway: coordinator interest grant refused; weak claims will be denied"
                    );
                }
            }
        }
    }
}

/// Drop back to disconnected so the next frame re-dials, and the IO layer
/// despawns the stale session entity.
fn end_session(session: &mut GatewaySession) {
    session.state = GatewayState::Disconnected;
    session.session = None;
    session.hello_sent = false;
    session.disconnected_at = Some(Instant::now());
}

/// Decode one tagged payload into a [`GatewayReply`], from either lane.
///
/// The tag says which encoding the payload carries; the lane it arrived on
/// does not, and deliberately is not consulted. A gateway is free to answer a
/// control message on the datagram lane if it is small enough to fit, and a
/// receiver that refused it on lane grounds would drop a well-formed reply for
/// a reason its sender cannot observe.
fn decode_reply(payload: &[u8]) -> Option<GatewayReply> {
    let (channel, body) = orrery_net::channels::untag(payload)?;
    match channel {
        orrery_net::channels::Channel::State => postcard::from_bytes(body).ok(),
        orrery_net::channels::Channel::Control => {
            orrery_protocol::channels::decode_stream_frame(payload)
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
    interest: Option<ResMut<'w, InterestGrant>>,
    authority_state: Option<ResMut<'w, AuthorityState>>,
    authority_events: Option<ResMut<'w, Messages<AuthorityEvent>>>,
    identities: Query<'w, 's, (Entity, &'static PersistIdentity, &'static Authority)>,
    sessions: Query<'w, 's, &'static mut aeronet_io::Session>,
    streams: Query<'w, 's, &'static mut aeronet_iroh::stream::IrohStreamIo>,
}

/// Adopt the registrar row a fencing NACK carried, when it genuinely
/// supersedes what this peer holds.
///
/// The NACK rides the unreliable, unordered datagram lane, and the uplink
/// resends an unchanged pending diff until it is acked — so the row that fenced
/// an older write can arrive late, arrive twice, or arrive after the registrar
/// has granted this peer a newer fence. Revoking on every row would hand
/// authority away on the strength of a datagram the registrar has itself moved
/// past. Every other lease path gates the same way (`orrery_authority`: a Grant
/// needs a strictly greater `LeaseId`, an Expire or Divest needs equality).
///
/// Refusing a row costs no liveness: `sweep_lease_expiry` drops a local fence a
/// heartbeat before its TTL whatever the gateway says, so a peer that really
/// has lost the lease still stops writing.
// Every argument is one piece of `ReplyProcessingContext` this needs; bundling
// them into a struct would only re-borrow the same fields under a new name.
#[allow(clippy::too_many_arguments)]
fn reconcile_lease_nack(
    commands: &mut Commands,
    scheduler: &mut UplinkScheduler,
    authority_state: Option<&mut AuthorityState>,
    authority_events: Option<&mut Messages<AuthorityEvent>>,
    identities: &Query<(Entity, &PersistIdentity, &Authority)>,
    adopted: &mut HashMap<PersistId, LeaseId>,
    persisted: PersistId,
    current: &Lease,
) {
    let Some((entity, _, authority)) = identities
        .iter()
        .find(|(_, identity, _)| identity.0 == persisted)
    else {
        return;
    };
    // A row naming this peer as the holder is not a revocation at all: the
    // write was fenced for some other reason, and the grant it was written
    // under is the one the row itself reports.
    let local_node = authority_state.as_ref().map(|state| state.node);
    if current.holder.is_some() && current.holder == local_node {
        return;
    }
    // With a fence installed, the fencing token decides: it is the registrar's
    // monotonic per-entity token, so anything not strictly greater describes a
    // state this peer has already left. Without one the token says nothing
    // about which row is newer, and the sequence pair is the ordering D7
    // leaves.
    let superseding = match adopted.get(&persisted).copied().or_else(|| {
        authority_state
            .as_ref()
            .and_then(|state| state.local_lease_id(persisted))
    }) {
        Some(held) => current.lease_id > held,
        None => current.seq.supersedes(authority.seq),
    };
    if !superseding {
        return;
    }
    adopted.insert(persisted, current.lease_id);
    scheduler.unregister(persisted);
    let revoked = authority_state.and_then(|state| state.revoke_local_lease(persisted));
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
    fn an_interest_ack_records_the_epoch_and_a_refusal_clears_it() {
        // Given: a peer holding a coordinator grant it has presented.
        let (mut app, session_entity) = reply_app();
        app.init_resource::<orrery_authority::InterestGrant>();
        {
            let mut interest = app
                .world_mut()
                .resource_mut::<orrery_authority::InterestGrant>();
            interest.set(vec![1, 2, 3]);
        }

        // When: the gateway accepts it.
        push_reply(
            &mut app,
            session_entity,
            GatewaySession::encode_stream(&GatewayReply::InterestAck {
                epoch: Some(orrery_protocol::Epoch::new(4)),
                reason: orrery_protocol::INTEREST_ACK_OK,
            }),
        );
        app.update();

        // Then: the peer knows its interest is on file and stops re-presenting.
        let interest = app.world().resource::<orrery_authority::InterestGrant>();
        assert_eq!(
            interest.accepted_epoch,
            Some(orrery_protocol::Epoch::new(4))
        );
        assert!(interest.is_accepted());
        assert!(!interest.pending);

        // When: a later grant is refused (a rotated-out coordinator key, say).
        push_reply(
            &mut app,
            session_entity,
            GatewaySession::encode_stream(&GatewayReply::InterestAck {
                epoch: None,
                reason: orrery_protocol::INTEREST_ACK_UNTRUSTED,
            }),
        );
        app.update();

        // Then: the peer can see it has no interest on file, rather than
        // discovering it later as unexplained `NotEligible` claim denials.
        let interest = app.world().resource::<orrery_authority::InterestGrant>();
        assert!(!interest.is_accepted());
        assert_eq!(
            interest.last_reason,
            orrery_protocol::INTEREST_ACK_UNTRUSTED
        );
    }

    #[test]
    fn a_reconnect_re_presents_the_grant_because_a_new_session_holds_none() {
        let mut interest = orrery_authority::InterestGrant::default();
        interest.set(vec![9]);
        interest.pending = false;
        interest.accepted_epoch = Some(orrery_protocol::Epoch::new(2));

        interest.resend();

        assert!(interest.pending, "a fresh session has no interest on file");
        assert!(!interest.is_accepted());

        // A peer that never had a grant has nothing to re-present.
        let mut empty = orrery_authority::InterestGrant::default();
        empty.resend();
        assert!(!empty.pending);
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
            bound_to: None,
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

    /// Give `persisted` a fenced local grant at `lease_id` and return its ECS
    /// entity.
    ///
    /// `reply_app` does not run `sync_authority_identity`, so this peer's own
    /// `NodeId` is installed here: a NACK naming this peer as the holder is
    /// only recognizable against it.
    fn granted_entity(
        app: &mut bevy_app::App,
        persisted: PersistId,
        lease_id: LeaseId,
        peer: orrery_protocol::NodeId,
    ) -> Entity {
        app.world_mut().resource_mut::<AuthorityState>().node = peer;
        let entity = app
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
        let claim_id = begin_claim(app, entity, persisted);
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(orrery_protocol::LeaseMsg::Grant {
                claim_id,
                entity: persisted,
                lease_id,
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            });
        app.update();
        app.world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .clear();
        entity
    }

    /// A registrar row naming `holder` at `lease_id`, as a fencing NACK
    /// carries it.
    fn lease_row(
        persisted: PersistId,
        holder: Option<orrery_protocol::NodeId>,
        lease_id: LeaseId,
        seq: SeqPair,
    ) -> Lease {
        Lease {
            entity: persisted,
            holder,
            seq,
            lease_id,
            expires_at: 99,
            flags: LeaseFlags::default(),
            bound_to: None,
        }
    }

    fn push_nack(
        app: &mut bevy_app::App,
        session_entity: Entity,
        persisted: PersistId,
        tick: Tick,
        lease: Option<Lease>,
    ) {
        push_reply(
            app,
            session_entity,
            Bytes::from(GatewaySession::encode_datagram(&GatewayReply::BulkNack {
                entity: persisted,
                tick,
                reason: 1,
                lease,
            })),
        );
    }

    #[test]
    fn a_stale_lease_bearing_nack_after_a_newer_grant_keeps_authority() {
        // The NACK lane is unreliable and unordered, and the uplink resends an
        // unchanged pending diff until it is acked (uplink.rs) — so the row
        // that fenced an *older* write can be delivered after the registrar has
        // granted this peer a newer fence. Acting on it revokes a live grant
        // and hands the entity to a holder the registrar has moved past.
        let (mut app, session_entity) = reply_app();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(21);
        let local = granted_entity(&mut app, persisted, LeaseId(9), node(1));

        push_nack(
            &mut app,
            session_entity,
            persisted,
            Tick::new(3),
            Some(lease_row(
                persisted,
                Some(node(8)),
                LeaseId(6),
                SeqPair {
                    own_seq: 0,
                    auth_seq: 1,
                },
            )),
        );
        app.update();

        assert!(
            app.world().get::<LocallyAuthoritative>(local).is_some(),
            "a superseded row must not revoke the fence this peer holds"
        );
        assert!(matches!(
            app.world().get::<AuthorityPhase>(local),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(9),
                ..
            })
        ));
        assert_eq!(
            app.world()
                .resource::<AuthorityState>()
                .local_lease_id(persisted),
            Some(LeaseId(9))
        );
        assert!(
            app.world_mut()
                .resource_mut::<Messages<AuthorityEvent>>()
                .drain()
                .next()
                .is_none(),
            "no authority was lost, so nothing is reported lost"
        );
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
    fn a_duplicate_lease_bearing_nack_revokes_once() {
        // The same datagram delivered twice — the lane duplicates freely, and
        // the uplink resends the fenced diff that provoked it. The second copy
        // no longer supersedes anything, so it must not report a second loss
        // or overwrite the row the first one installed.
        let (mut app, session_entity) = reply_app();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(22);
        let local = granted_entity(&mut app, persisted, LeaseId(4), node(1));
        let row = lease_row(
            persisted,
            Some(node(8)),
            LeaseId(5),
            SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
        );

        push_nack(
            &mut app,
            session_entity,
            persisted,
            Tick::new(3),
            Some(row.clone()),
        );
        app.update();
        let lost: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<AuthorityEvent>>()
            .drain()
            .collect();
        assert_eq!(lost.len(), 1, "the first row revoked the fence");

        push_nack(&mut app, session_entity, persisted, Tick::new(3), Some(row));
        app.update();

        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<AuthorityEvent>>()
                .drain()
                .count(),
            0,
            "the replayed row has nothing left to revoke"
        );
        let authority = app.world().get::<Authority>(local).unwrap();
        assert_eq!(authority.holder, Some(node(8)));
    }

    #[test]
    fn a_nack_naming_this_peer_as_the_holder_keeps_authority() {
        // A fenced write can be rejected for a reason that has nothing to do
        // with the fence — and the row then names this peer. Treating it as a
        // revocation would have the client hand authority to itself and drop
        // to `Remote`, which no registrar ever asked for.
        let (mut app, session_entity) = reply_app();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(23);
        let local = granted_entity(&mut app, persisted, LeaseId(4), node(1));

        push_nack(
            &mut app,
            session_entity,
            persisted,
            Tick::new(3),
            Some(lease_row(
                persisted,
                Some(node(1)),
                LeaseId(9),
                SeqPair {
                    own_seq: 1,
                    auth_seq: 0,
                },
            )),
        );
        app.update();

        assert!(app.world().get::<LocallyAuthoritative>(local).is_some());
        assert!(matches!(
            app.world().get::<AuthorityPhase>(local),
            Some(AuthorityPhase::LocalGranted {
                lease_id: LeaseId(4),
                ..
            })
        ));
    }

    #[test]
    fn two_rows_in_one_batch_resolve_by_fence_not_arrival_order() {
        // Both lanes drain into one list, so a reordered pair is delivered in
        // one update. Component writes are queued as commands and land after
        // the system returns, so the older row must be refused against the
        // fence the newer one just installed, not against the stale `Authority`
        // the query still sees.
        let (mut app, session_entity) = reply_app();
        app.add_plugins(OrreryAuthorityPlugin);
        let persisted = PersistId::new(24);
        let local = granted_entity(&mut app, persisted, LeaseId(4), node(1));

        push_nack(
            &mut app,
            session_entity,
            persisted,
            Tick::new(3),
            Some(lease_row(
                persisted,
                Some(node(8)),
                LeaseId(12),
                SeqPair {
                    own_seq: 2,
                    auth_seq: 0,
                },
            )),
        );
        push_nack(
            &mut app,
            session_entity,
            persisted,
            Tick::new(3),
            Some(lease_row(
                persisted,
                Some(node(3)),
                LeaseId(6),
                SeqPair {
                    own_seq: 1,
                    auth_seq: 0,
                },
            )),
        );
        app.update();

        let authority = app.world().get::<Authority>(local).unwrap();
        assert_eq!(
            authority.holder,
            Some(node(8)),
            "the newest fence owns the row regardless of arrival order"
        );
        assert_eq!(authority.seq.own_seq, 2);
    }

    #[test]
    fn hello_ack_naming_an_unsupported_protocol_ends_the_session() {
        let (mut app, session_entity) = hello_app();
        push_reply(
            &mut app,
            session_entity,
            GatewaySession::encode_stream(&GatewayReply::HelloAck {
                gateway: node(1),
                protocol: orrery_protocol::PROTOCOL_VERSION + 2,
            }),
        );
        app.update();

        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Disconnected);
        assert!(session.session.is_none());
        assert_ne!(
            session.protocol,
            orrery_protocol::PROTOCOL_VERSION + 2,
            "an unsupported version is refused, not stored"
        );
    }

    #[test]
    fn a_refused_hello_ends_the_session() {
        let (mut app, session_entity) = hello_app();
        push_reply(
            &mut app,
            session_entity,
            GatewaySession::encode_stream(&GatewayReply::HelloRefused {
                gateway: node(1),
                protocol: orrery_protocol::PROTOCOL_VERSION + 3,
                reason: GatewayReply::HELLO_REFUSED_PROTOCOL,
            }),
        );
        app.update();

        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Disconnected);
        assert!(session.session.is_none());
        assert!(!session.hello_sent, "the next dial sends a fresh hello");
    }

    /// A connecting session pointed at gateway `node(1)`, for the handshake
    /// tests.
    fn hello_app() -> (bevy_app::App, Entity) {
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
            session.state = GatewayState::Connecting;
        }
        app.add_systems(bevy_app::Update, process_replies);
        (app, session_entity)
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
            protocol: orrery_protocol::PROTOCOL_VERSION,
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
        assert_eq!(session.protocol, orrery_protocol::PROTOCOL_VERSION);
    }

    fn node(n: u8) -> orrery_protocol::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }
}
