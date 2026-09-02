//! The upload budget as the send path actually applies it (D6, D16).
//!
//! The meter's arithmetic has unit tests. What those cannot show is the part
//! that matters operationally: that `send_peer_packets` stops handing packets to
//! the IO layer once the window is spent, and that it stops handing over the
//! *right* ones. So this drives the real system over real `aeronet_io::Session`
//! components and reads the buffer the IO layer would have drained.
//!
//! No network: a `Session` is a pair of byte buffers plus an MTU and an
//! `IrohStreamIo` is a pair of message buffers, so both lanes can be driven
//! without a socket. Standing up QUIC would make the budget incidental to the
//! test instead of its subject.

use core::time::Duration;

use bevy::prelude::*;
use bevy_ecs::message::Messages;
use bevy_platform::time::Instant;

use aeronet_iroh::stream::IrohStreamIo;
use orrery_net::budget::{
    stream_wire_bytes, Bandwidth, UploadBudget, UploadMeter, DATAGRAM_OVERHEAD_BYTES,
};
use orrery_net::channels::{untag, Channel};
use orrery_net::peer_link::{forget_departed_links, send_peer_packets, PeerLinkCounters};
use orrery_net::plugin::Peer;
use orrery_net::{SendPacket, StreamMode};
use orrery_protocol::{
    channels::{
        encode_delivered_input, encode_delta_patch, encode_hit, encode_replication,
        encode_replication_delta, encode_witness_keyframe, ReplicationDelta, TAG_DELIVERED_INPUT,
        TAG_HIT, TAG_REPLICATION, TAG_REPLICATION_DELTA, TAG_WITNESS_KEYFRAME,
    },
    HitClaim, HitMsg, HitSurface, InterpBasis, LatticePoint, NodeId, PersistId, QuantizedDir,
    QuantizedRay, Tick, WeaponRef,
};

const MTU: usize = 1_200;
const PAYLOAD: usize = 500;
/// Wire cost of one test packet: payload + channel tag + datagram overhead.
const WIRE: u64 = PAYLOAD as u64 + 1 + DATAGRAM_OVERHEAD_BYTES;

fn secret(n: u8) -> iroh::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh::SecretKey::from_bytes(&seed)
}

/// An app running only the send path, with one fake session per peer.
fn app(peers: &[NodeId]) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<SendPacket>()
        .init_resource::<PeerLinkCounters>()
        .init_resource::<UploadBudget>()
        .init_resource::<UploadMeter>()
        .add_systems(Update, (send_peer_packets, forget_departed_links).chain());
    for peer in peers {
        app.world_mut().spawn((
            Peer {
                id: *peer,
                incoming: false,
            },
            aeronet_io::Session::new(Instant::now(), MTU),
            // Control rides QUIC streams, not datagrams (D3). A peer without
            // this lane has nowhere to put a control packet at all.
            IrohStreamIo::detached(),
        ));
    }
    app
}

fn queue(app: &mut App, to: NodeId, channel: Channel, count: usize) {
    let mut messages = app.world_mut().resource_mut::<Messages<SendPacket>>();
    for _ in 0..count {
        messages.write(SendPacket {
            to,
            channel,
            payload: bytes::Bytes::from(vec![0u8; PAYLOAD]),
            mode: StreamMode::Shared,
        });
    }
}

/// How many sends the IO layer was handed, across all sessions and both lanes.
fn queued_for_io(app: &mut App) -> usize {
    let datagrams: usize = app
        .world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .map(|session| session.send.len())
        .sum();
    let messages: usize = app
        .world_mut()
        .query::<&IrohStreamIo>()
        .iter(app.world())
        .map(|streams| streams.send.len())
        .sum();
    datagrams + messages
}

/// Packets the budget allows in one window, at this test's packet size.
fn allowance() -> usize {
    allowance_for(UploadBudget::default())
}

fn allowance_for(budget: UploadBudget) -> usize {
    (budget.sustained.bytes_over(budget.window) / WIRE) as usize
}

fn state_subtag(payload: &[u8]) -> Option<u8> {
    let (_, body) = untag(payload)?;
    body.first().copied()
}

fn replication_subtag(packet: &[u8]) -> Option<u8> {
    let (_, payload) = untag(packet)?;
    state_subtag(payload)
}

fn hit_claim() -> HitMsg {
    HitMsg::Claim(HitClaim {
        shooter: PersistId::new(1),
        target: PersistId::new(2),
        weapon: WeaponRef(1),
        fire_tick: Tick::new(100),
        basis: InterpBasis::exact(Tick::new(95)),
        ray: QuantizedRay {
            origin: LatticePoint::new(0, 0, 0),
            direction: QuantizedDir::new(1, 0, 0),
        },
        claimed: HitSurface(0),
        input_seq: 1,
    })
}

#[test]
fn everything_flows_while_there_is_budget() {
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, 10);
    app.update();

    assert_eq!(queued_for_io(&mut app), 10);
    assert_eq!(app.world().resource::<UploadMeter>().shed, 0);
}

#[test]
fn state_is_shed_once_the_window_is_spent() {
    // The whole point. 1 Mbps is 125 kB/s; at ~561 wire bytes a packet that is
    // ~222 packets in a one-second window, so 400 must not all go out.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    let cap = allowance();
    queue(&mut app, peer, Channel::State, cap + 200);
    app.update();

    let sent = queued_for_io(&mut app);
    let meter = app.world().resource::<UploadMeter>();
    assert!(
        sent <= cap + 1,
        "sent {sent} packets against an allowance of {cap}"
    );
    assert_eq!(sent + meter.shed as usize, cap + 200, "nothing vanishes");
    assert!(meter.shed_bytes >= meter.shed * WIRE);
}

#[test]
fn a_keyframe_is_shed_only_after_every_delta() {
    // Put every delta *before* its keyframe. FIFO therefore admits the deltas
    // and sheds the anchor; only the wire-sub-tag batch classifier can put the
    // keyframe first. The payloads come from the real encoders, so this asserts
    // what the send path reads from wire bytes rather than a pre-decided kind.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    let absolute = (0..PAYLOAD)
        .map(|index| ((index * 73 + 19) % 251) as u8)
        .collect::<Vec<_>>();
    let keyframe = bytes::Bytes::from(encode_replication(&absolute));
    let delta = bytes::Bytes::from(encode_replication_delta(
        &absolute,
        &ReplicationDelta {
            entity: PersistId::new(7),
            tick: 60,
            keyframe_age: 60,
            cell: None,
            patch: encode_delta_patch(&absolute, &absolute),
        },
    ));
    assert_eq!(state_subtag(&keyframe), Some(TAG_REPLICATION));
    assert_eq!(state_subtag(&delta), Some(TAG_REPLICATION_DELTA));

    let budget = UploadBudget::default();
    let limit = budget.sustained.bytes_over(Duration::from_secs(1));
    let keyframe_wire = keyframe.len() as u64 + 1 + DATAGRAM_OVERHEAD_BYTES;
    let delta_wire = delta.len() as u64 + 1 + DATAGRAM_OVERHEAD_BYTES;
    // This many deltas fit before the keyframe under FIFO, but the keyframe
    // plus all of them does not fit. The correct order sheds exactly one delta.
    let delta_count = usize::try_from((limit - keyframe_wire) / delta_wire + 1).unwrap();
    assert!(u64::try_from(delta_count).unwrap() * delta_wire <= limit);
    assert!(keyframe_wire + u64::try_from(delta_count).unwrap() * delta_wire > limit);

    let mut messages = app.world_mut().resource_mut::<Messages<SendPacket>>();
    for _ in 0..delta_count {
        messages.write(SendPacket::state(peer, delta.clone()));
    }
    messages.write(SendPacket::state(peer, keyframe));
    app.update();

    let sent = app
        .world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .next()
        .expect("test session")
        .send
        .iter()
        .filter_map(|packet| replication_subtag(packet))
        .collect::<Vec<_>>();
    let keyframes = sent.iter().filter(|&&tag| tag == TAG_REPLICATION).count();
    let deltas = sent
        .iter()
        .filter(|&&tag| tag == TAG_REPLICATION_DELTA)
        .count();
    assert_eq!(
        keyframes, 1,
        "the keyframe must survive before any delta sheds"
    );
    assert_eq!(deltas, delta_count - 1, "exactly one delta must be shed");
    assert_eq!(
        app.world().resource::<UploadMeter>().shed,
        1,
        "the overage must be the final delta, not the keyframe"
    );
}

#[test]
fn a_witness_link_keyframe_survives_a_500_kbps_squeeze() {
    // A20 §4 observed 89 false positives at 500 kbps when a witness lost the
    // keyframe its deltas depend on. The distinct wire family, not a caller
    // priority field, places this anchor on the unsheddable side of the meter.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    let budget = UploadBudget {
        sustained: Bandwidth::from_kbps(500),
        ..UploadBudget::default()
    };
    *app.world_mut().resource_mut::<UploadBudget>() = budget;
    queue(&mut app, peer, Channel::State, allowance_for(budget) + 200);
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::state(
            peer,
            bytes::Bytes::from(encode_witness_keyframe(&vec![7u8; PAYLOAD])),
        ));

    app.update();

    let sent = app
        .world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .next()
        .expect("test session")
        .send
        .iter()
        .filter_map(|packet| replication_subtag(packet))
        .collect::<Vec<_>>();
    assert_eq!(
        sent.iter()
            .filter(|&&tag| tag == TAG_WITNESS_KEYFRAME)
            .count(),
        1,
        "the witness-link keyframe must survive the squeeze"
    );
    let meter = app.world().resource::<UploadMeter>();
    assert!(
        meter.shed > 0,
        "the replication flood must actually be shed"
    );
    assert!(
        meter.lanes.witness_keyframe_bytes > 0,
        "the preserved anchor gets its own audited lane"
    );
}

#[test]
fn control_still_goes_out_when_state_is_being_shed() {
    // Shedding a gap repair or a lease operation turns one dropped datagram
    // into a permanent hole — a repair that never arrives is indistinguishable
    // from an authority refusing to answer. State loss is expected and already
    // has a repair path; control loss does not.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    let cap = allowance();
    queue(&mut app, peer, Channel::State, cap + 200);
    queue(&mut app, peer, Channel::Control, 5);
    app.update();

    let (shed, control) = {
        let meter = app.world().resource::<UploadMeter>();
        (meter.shed, meter.unsheddable_over_budget)
    };
    assert!(shed > 0, "the state flood must actually be shed");
    assert_eq!(
        control, 0,
        "unsheddable control must be admitted before the replication flood spends the window"
    );
    // Sent = whatever state fit, plus all five control packets.
    let sent = queued_for_io(&mut app);
    assert_eq!(sent + shed as usize, cap + 205);
}

#[test]
fn hit_claims_survive_upload_pressure_ahead_of_replication() {
    // A hit claim is small, latency-critical input retried until its verdict.
    // Flooding the State lane with bulk snapshots must not discard it before
    // the next replication update, which will supersede the one we shed.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, allowance() + 200);
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::state(
            peer,
            bytes::Bytes::from(encode_hit(&hit_claim())),
        ));

    app.update();

    let sent = app
        .world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .next()
        .expect("test session")
        .send
        .iter()
        .filter_map(|packet| replication_subtag(packet))
        .collect::<Vec<_>>();
    assert_eq!(
        sent.iter().filter(|&&subtag| subtag == TAG_HIT).count(),
        1,
        "the hit claim must be admitted before replication is shed"
    );
    let meter = app.world().resource::<UploadMeter>();
    assert!(
        meter.shed > 0,
        "the replication flood must actually be shed"
    );
    assert!(
        meter.lanes.hit_bytes > 0,
        "hit traffic gets its own accounting lane"
    );
}

#[test]
fn delivered_inputs_survive_upload_pressure_on_a_datagram() {
    // `Game::deliver` currently uses a reliable stream, but its delivery
    // guarantee belongs to the tagged class. This deliberately sends the real
    // delivered-input frame as a datagram: changing transports cannot turn a
    // damage, pickup, or door-open input into replication that the meter sheds.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, allowance() + 200);
    // Fill the fractional remainder that `allowance()` leaves after 500-byte
    // packets, so a misclassified input has no accidental room to slip through.
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::state(peer, bytes::Bytes::from(vec![0u8; 397])));
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::state(
            peer,
            bytes::Bytes::from(encode_delivered_input(
                PersistId::new(1),
                PersistId::new(2),
                b"damage",
            )),
        ));
    app.update();

    let delivered = app
        .world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .next()
        .expect("test session")
        .send
        .iter()
        .any(|packet| {
            let (_, logical) = untag(packet).expect("outer transport tag");
            let (channel, body) = untag(logical).expect("inner protocol tag");
            channel == Channel::Control && body.first() == Some(&TAG_DELIVERED_INPUT)
        });
    assert!(
        delivered,
        "the delivered input must be admitted before replication is shed"
    );
    assert!(
        app.world().resource::<UploadMeter>().shed > 0,
        "the replication flood must actually be shed"
    );
}

#[test]
fn the_budget_is_shared_across_links_not_granted_per_link() {
    // The D6 number is a peer's uplink. Two links each sending half the
    // allowance put the peer at the ceiling — this is the mesh scaling problem
    // the 24-entity interest set exists to bound.
    let (first, second) = (secret(1).public(), secret(2).public());
    let mut app = app(&[first, second]);
    let half = allowance() / 2;
    queue(&mut app, first, Channel::State, half + 50);
    queue(&mut app, second, Channel::State, half + 50);
    app.update();

    let meter = app.world().resource::<UploadMeter>();
    assert!(
        meter.shed > 0,
        "two links under budget individually can still put the peer over it"
    );
}

#[test]
fn a_packet_larger_than_the_mtu_is_refused_before_it_is_charged() {
    // An oversized packet is never sent, so charging it would inflate the
    // measured rate with bytes that never left.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket {
            to: peer,
            channel: Channel::State,
            payload: bytes::Bytes::from(vec![0u8; MTU * 2]),
            mode: StreamMode::Shared,
        });
    app.update();

    assert_eq!(queued_for_io(&mut app), 0);
    assert_eq!(app.world().resource::<PeerLinkCounters>().oversized, 1);
    let budget = *app.world().resource::<UploadBudget>();
    let now = app.world().resource::<Time<Real>>().elapsed();
    let mut meter = std::mem::take(&mut *app.world_mut().resource_mut::<UploadMeter>());
    assert_eq!(meter.rate(budget, now).bits_per_sec(), 0);
}

#[test]
fn a_departed_peers_meter_is_forgotten() {
    // Otherwise a long-lived peer accumulates a meter per NodeId it has ever
    // talked to, and the per-link division the accumulator reads is computed
    // against a link count that only grows.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, 5);
    app.update();
    assert_eq!(app.world().resource::<UploadMeter>().links().count(), 1);

    let session = app
        .world_mut()
        .query_filtered::<Entity, With<Peer>>()
        .iter(app.world())
        .next()
        .expect("session");
    app.world_mut().despawn(session);
    app.update();

    assert_eq!(
        app.world().resource::<UploadMeter>().links().count(),
        0,
        "a peer with no session is no longer a link"
    );
}

#[test]
fn control_rides_the_stream_lane_and_state_rides_datagrams() {
    // The channel policy is D3's, and until the stream lane landed it was
    // aspirational: both channels went out as datagrams and `Channel::Control`
    // bought routing rather than reliability. This asserts the split is real.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, 3);
    queue(&mut app, peer, Channel::Control, 2);
    app.update();

    let datagrams = app
        .world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .map(|session| session.send.len())
        .sum::<usize>();
    let messages = app
        .world_mut()
        .query::<&IrohStreamIo>()
        .iter(app.world())
        .map(|streams| streams.send.len())
        .sum::<usize>();
    assert_eq!((datagrams, messages), (3, 2));

    let counters = app.world().resource::<PeerLinkCounters>();
    assert_eq!((counters.sent, counters.stream_sent), (3, 2));
    assert_eq!(counters.oversized, 0);
}

#[test]
fn a_control_payload_far_past_the_mtu_is_carried_rather_than_refused() {
    // This is the change gap repair was waiting for. On the datagram lane a
    // 40 kB range response was refused outright, so the authority served what
    // fit in ~1200 B and the requester asked again — twenty exchanges per
    // witness for a one-second hole, and more repair budget only put more of
    // them in flight. A stream has no MTU to run into.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    let payload = vec![0u8; 40_000];
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::control(
            peer,
            bytes::Bytes::from(payload.clone()),
        ));
    app.update();

    assert_eq!(queued_for_io(&mut app), 1);
    assert_eq!(app.world().resource::<PeerLinkCounters>().oversized, 0);

    // And it is charged what it costs: one framing per packet it occupies, not
    // one for the whole message.
    let budget = *app.world().resource::<UploadBudget>();
    let mut meter = app.world_mut().resource_mut::<UploadMeter>();
    let charged = meter.peer_rate(budget, Duration::ZERO, peer);
    let expected = stream_wire_bytes(payload.len() + 1, MTU);
    assert_eq!(charged.bits_per_sec(), expected * 8);
}

#[test]
fn a_control_packet_beyond_the_stream_limit_is_refused_not_sent() {
    // The lane's ceiling is a memory bound rather than a path bound, but it is
    // still a bound: the IO layer would drop the session over an oversized
    // message, so it is refused here instead.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    let too_large = usize::try_from(orrery_net::peer_link::MAX_STREAM_MESSAGE_LEN).unwrap() + 1;
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::control(
            peer,
            bytes::Bytes::from(vec![0u8; too_large]),
        ));
    app.update();

    assert_eq!(queued_for_io(&mut app), 0);
    assert_eq!(app.world().resource::<PeerLinkCounters>().oversized, 1);
}
