//! The one payload contract across the peer seam, pinned executably (#964).
//!
//! `PeerPacket.payload` and `SendPacket.payload` are the *same bytes*: an
//! `orrery_protocol::channels` `encode_*` blob, which carries a channel tag of
//! its own. `send_peer_packets` adds the transport's tag on the way out and
//! `receive_peer_packets` strips exactly that one on the way in, so what a
//! consumer holds still opens with the payload's tag — which is why every
//! sub-tag decoder calls `untag` first.
//!
//! Nothing stated this. `receive_peer_packets` only *said* it stripped a tag,
//! which reads equally well as "the payload is the untagged body", and
//! `orrery_regolith_client` wrote its witness uplink to that reading: single-
//! tagged frames, whose only tag this crate then ate, so `decode_witness`
//! returned `None` for 100% of a human seat's claims and frames and
//! `orrery_witness` discarded every one while the seat still reported
//! `witness_anchored = true`. #386/#387 was the identical mistake on the
//! replication lane. A producer that bypasses `send_peer_packets` and writes a
//! peer frame itself owes it the outer tag; these tests are what it owes the
//! tag *to*.

#![allow(missing_docs)]

use bevy::prelude::*;
use bevy_platform::time::Instant;
use bytes::Bytes;

use aeronet_io::packet::RecvPacket;
use aeronet_iroh::stream::{IrohStreamIo, RecvMessage};
use orrery_net::budget::{UploadBudget, UploadMeter};
use orrery_net::channels::{decode_witness, encode_witness, untag, Channel};
use orrery_net::peer_link::{receive_peer_packets, send_peer_packets, PeerLinkCounters};
use orrery_net::plugin::Peer;
use orrery_net::{PeerPacket, SendPacket, StreamMode};
use orrery_protocol::{ChainHash, NodeId, PersistId, RulesetId, StateClaim, Tick, WitnessMsg};

const MTU: usize = 1_200;

fn secret(n: u8) -> iroh::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh::SecretKey::from_bytes(&seed)
}

fn single_threaded(app: &mut App) {
    app.edit_schedule(Update, |schedule| {
        schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
    });
}

/// The exact witness record a client publishes: a state claim, encoded by the
/// same `encode_witness` `orrery_witness::plugin::publish_authored` calls.
fn claim_payload() -> Bytes {
    Bytes::from(encode_witness(&WitnessMsg::Claim(StateClaim {
        entity: PersistId::new(7),
        chain_epoch: 0,
        tick: Tick::new(42),
        input_head: ChainHash::EMPTY,
        state_hash: [0u8; 32],
        prev_claim: [0u8; 32],
        ruleset: RulesetId {
            version: 1,
            digest: [0u8; 32],
        },
        // The seam moves bytes; it neither makes nor checks signatures, and a
        // claim it mangled would be refused for the wrong reason if it did.
        sig: secret(1).sign(b"unsigned"),
    })))
}

fn send_app(peer: NodeId) -> App {
    let mut app = App::new();
    single_threaded(&mut app);
    app.add_plugins(MinimalPlugins)
        .add_message::<SendPacket>()
        .init_resource::<PeerLinkCounters>()
        .init_resource::<UploadBudget>()
        .init_resource::<UploadMeter>()
        .add_systems(Update, send_peer_packets);
    app.world_mut().spawn((
        Peer {
            id: peer,
            incoming: false,
        },
        aeronet_io::Session::new(Instant::now(), MTU),
        IrohStreamIo::detached(),
    ));
    app
}

fn recv_app(peer: NodeId) -> App {
    let mut app = App::new();
    single_threaded(&mut app);
    app.add_plugins(MinimalPlugins)
        .add_message::<PeerPacket>()
        .init_resource::<PeerLinkCounters>()
        .add_systems(Update, receive_peer_packets);
    app.world_mut().spawn((
        Peer {
            id: peer,
            incoming: true,
        },
        aeronet_io::Session::new(Instant::now(), MTU),
        IrohStreamIo::detached(),
    ));
    app
}

/// Everything `send_peer_packets` handed the transport, in wire form.
fn drain_wire(app: &mut App) -> (Vec<Bytes>, Vec<Bytes>) {
    let world = app.world_mut();
    let mut query = world.query::<(&mut aeronet_io::Session, &mut IrohStreamIo)>();
    let mut datagrams = Vec::new();
    let mut messages = Vec::new();
    for (mut session, mut streams) in query.iter_mut(world) {
        datagrams.append(&mut session.send);
        messages.extend(streams.send.drain(..).map(|message| message.payload));
    }
    (datagrams, messages)
}

fn deliver(app: &mut App, datagrams: Vec<Bytes>, messages: Vec<Bytes>) {
    let world = app.world_mut();
    let mut query = world.query::<(&mut aeronet_io::Session, &mut IrohStreamIo)>();
    let (mut session, mut streams) = query
        .iter_mut(world)
        .next()
        .expect("the receiving session exists");
    for payload in datagrams {
        session.recv.push(RecvPacket {
            recv_at: Instant::now(),
            payload,
        });
    }
    for payload in messages {
        streams.recv.push(RecvMessage {
            recv_at: Instant::now(),
            payload,
        });
    }
}

fn received(app: &mut App) -> Vec<PeerPacket> {
    app.world_mut()
        .resource_mut::<Messages<PeerPacket>>()
        .drain()
        .collect()
}

/// The identity, on both lanes: what a consumer reads is byte-for-byte what
/// the producer wrote — still tagged, because an `encode_*` blob is.
#[test]
fn peer_packet_payload_is_the_encoded_blob() {
    let peer = secret(11).public();
    let sender = secret(12).public();
    let payload = claim_payload();

    let mut out = send_app(peer);
    out.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket {
            to: peer,
            channel: Channel::State,
            payload: payload.clone(),
            mode: StreamMode::Shared,
        });
    out.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket {
            to: peer,
            channel: Channel::Control,
            payload: payload.clone(),
            mode: StreamMode::Shared,
        });
    out.update();
    let (datagrams, messages) = drain_wire(&mut out);
    assert_eq!(datagrams.len(), 1, "one state datagram left the sender");
    assert_eq!(messages.len(), 1, "one control message left the sender");

    // The wire carries *two* tags: the transport's, then the payload's own.
    // This is the double tag `orrery_predict`'s bridge documents, not a bug.
    assert_eq!(
        datagrams[0][..2],
        [0x00, 0x00],
        "the transport tag precedes the payload's own State tag on the wire"
    );

    let mut input = recv_app(sender);
    deliver(&mut input, datagrams, messages);
    input.update();

    let packets = received(&mut input);
    assert_eq!(packets.len(), 2, "both lanes delivered");
    for packet in &packets {
        assert_eq!(
            packet.payload, payload,
            "PeerPacket.payload must be the SendPacket.payload byte for byte — the only \
             tag stripped is the transport's own"
        );
        assert!(
            untag(&packet.payload).is_some(),
            "the delivered payload still carries its own channel tag, which is what every \
             sub-tag decoder untags"
        );
    }
}

/// The consequence, on the exact system that discarded them: a witness record
/// that crossed the seam still decodes. Asserting the identity alone would
/// pass on a seam that delivered some other correctly-shaped bytes.
#[test]
fn a_witness_record_that_crossed_the_seam_still_decodes() {
    let peer = secret(13).public();
    let sender = secret(14).public();
    let payload = claim_payload();

    let mut out = send_app(peer);
    out.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket {
            to: peer,
            channel: Channel::State,
            payload,
            mode: StreamMode::Shared,
        });
    out.update();
    let (datagrams, messages) = drain_wire(&mut out);

    let mut input = recv_app(sender);
    deliver(&mut input, datagrams, messages);
    input.update();

    let packets = received(&mut input);
    assert_eq!(packets.len(), 1, "the record crossed");
    let decoded = decode_witness::<WitnessMsg>(&packets[0].payload).expect(
        "`decode_witness` is the call `orrery_witness::ingest_peer_traffic` makes on this \
         very payload; when it returns `None` the record is silently discarded",
    );
    assert!(
        matches!(decoded, WitnessMsg::Claim(_)),
        "the claim survived the seam as a claim"
    );
}

/// The producer's obligation, stated as a test rather than as a comment.
///
/// A frame written by a producer that bypasses `send_peer_packets` — the
/// regolith client's uplink is one — reaches `receive_peer_packets` as an
/// ordinary inbound message. Single-tagged, it decodes to nothing: this is
/// #964's whole fault in four lines, and it is *not* reported, which is why
/// it survived a seated run.
#[test]
fn a_single_tagged_frame_loses_the_payloads_own_tag_and_is_silently_undecodable() {
    let sender = secret(15).public();
    let payload = claim_payload();

    let mut input = recv_app(sender);
    // What #964 shipped: the `encode_witness` blob straight onto the wire.
    deliver(&mut input, Vec::new(), vec![payload.clone()]);
    input.update();

    let packets = received(&mut input);
    assert_eq!(
        packets.len(),
        1,
        "the seam accepts it — the payload's own tag reads as a valid transport tag"
    );
    assert_ne!(
        packets[0].payload, payload,
        "a byte was eaten: the payload's own tag was consumed as the transport's"
    );
    assert!(
        decode_witness::<WitnessMsg>(&packets[0].payload).is_none(),
        "and the record is undecodable at the consumer"
    );
    assert_eq!(
        input.world().resource::<PeerLinkCounters>().untagged,
        0,
        "with nothing counted: the loss is invisible at the transport, so the contract has \
         to be kept by producers rather than detected here"
    );
}
