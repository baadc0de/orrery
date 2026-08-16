//! The upload budget as the send path actually applies it (D6, D16).
//!
//! The meter's arithmetic has unit tests. What those cannot show is the part
//! that matters operationally: that `send_peer_packets` stops handing packets to
//! the IO layer once the window is spent, and that it stops handing over the
//! *right* ones. So this drives the real system over real `aeronet_io::Session`
//! components and reads the buffer the IO layer would have drained.
//!
//! No network: a `Session` is a pair of byte buffers plus an MTU, and standing
//! up QUIC would make the budget incidental to the test instead of its subject.

use core::time::Duration;

use bevy::prelude::*;
use bevy_ecs::message::Messages;
use bevy_platform::time::Instant;

use orrery_net::budget::{UploadBudget, UploadMeter, DATAGRAM_OVERHEAD_BYTES};
use orrery_net::channels::Channel;
use orrery_net::peer_link::{forget_departed_links, send_peer_packets, PeerLinkCounters};
use orrery_net::plugin::Peer;
use orrery_net::SendPacket;
use orrery_protocol::NodeId;

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
        });
    }
}

/// How many packets the IO layer was handed, across all sessions.
fn queued_for_io(app: &mut App) -> usize {
    app.world_mut()
        .query::<&aeronet_io::Session>()
        .iter(app.world())
        .map(|session| session.send.len())
        .sum()
}

/// Packets the budget allows in one window, at this test's packet size.
fn allowance() -> usize {
    (UploadBudget::default()
        .sustained
        .bytes_over(Duration::from_secs(1))
        / WIRE) as usize
}

#[test]
fn everything_flows_while_there_is_budget() {
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, 10);
    app.update();

    assert_eq!(queued_for_io(&mut app), 10);
    assert_eq!(app.world().resource::<UploadMeter>().shed, 0);
    assert!(!app.world().resource::<UploadMeter>().oversubscribed);
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
    assert!(meter.oversubscribed);
    assert!(meter.shed_bytes >= meter.shed * WIRE);
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
        (meter.shed, meter.control_over_budget)
    };
    assert!(shed > 0, "the state flood must actually be shed");
    assert_eq!(
        control, 5,
        "every control packet went out over budget, and was counted"
    );
    // Sent = whatever state fit, plus all five control packets.
    let sent = queued_for_io(&mut app);
    assert_eq!(sent + shed as usize, cap + 205);
}

#[test]
fn an_overrun_is_reported_rather_than_only_absorbed() {
    // docs/03-replication.md §9.3: sustained oversubscription across an
    // island's links is a promotion signal alongside raw population, so it has
    // to be visible and not merely handled.
    let peer = secret(1).public();
    let mut app = app(&[peer]);
    queue(&mut app, peer, Channel::State, allowance() + 50);
    app.update();
    assert!(app.world().resource::<UploadMeter>().oversubscribed);

    // A quiet frame clears the flag — the signal is "right now", and the
    // coordinator's promotion decision is over sustained overruns, not one.
    app.update();
    assert!(!app.world().resource::<UploadMeter>().oversubscribed);
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
