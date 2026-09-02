//! The peer-link fault counters are *surfaced*, not merely counted (#954).
//!
//! `no_session` counted every keyframe built for a rostered-but-unlinked seat
//! for the whole 2026-09-02 attempt and nothing reported it — the defect was
//! found by reading `send_peer_packets`, not by observing it. These tests pin
//! the surface: a warn that names the counter and the peer, on the first
//! occurrence, and quiet on a healthy run so the loud lines stay worth
//! reading.

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy_platform::time::Instant;
use bytes::Bytes;

use aeronet_io::packet::RecvPacket;
use aeronet_iroh::stream::IrohStreamIo;
use orrery_net::budget::{UploadBudget, UploadMeter};
use orrery_net::peer_link::{receive_peer_packets, send_peer_packets, PeerLinkCounters};
use orrery_net::plugin::Peer;
use orrery_net::SendPacket;
use orrery_protocol::NodeId;

const MTU: usize = 1_200;

fn secret(n: u8) -> iroh::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh::SecretKey::from_bytes(&seed)
}

/// A `MakeWriter` over a shared buffer, so a scoped tracing subscriber can be
/// read back per test without a global subscriber racing the parallel ones.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// Installs a WARN-level subscriber for the closure and returns what it wrote.
fn capturing<F: FnOnce()>(run: F) -> String {
    let buffer = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(buffer.clone())
        .without_time()
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    let captured = {
        let bytes = buffer.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes).expect("capture is valid utf-8")
    };
    captured
}

/// An app running only the send path, with no sessions at all — every send is
/// addressed to a peer the world does not know.
fn send_app() -> App {
    let mut app = App::new();
    single_threaded(&mut app);
    app.add_plugins(MinimalPlugins)
        .add_message::<SendPacket>()
        .init_resource::<PeerLinkCounters>()
        .init_resource::<UploadBudget>()
        .init_resource::<UploadMeter>()
        .add_systems(Update, (send_peer_packets, forget_departed_links).chain());
    app
}

/// The default executor runs systems on worker threads, which do not see the
/// thread-local subscriber these tests install; run everything on the caller.
fn single_threaded(app: &mut App) {
    app.edit_schedule(Update, |schedule| {
        schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
    });
}

use orrery_net::peer_link::forget_departed_links;

fn write_sends(app: &mut App, to: NodeId, count: usize) {
    let mut messages = app.world_mut().resource_mut::<Messages<SendPacket>>();
    for _ in 0..count {
        messages.write(SendPacket::state(to, Bytes::from(vec![0u8; 64])));
    }
}

#[test]
fn a_send_to_a_peer_with_no_session_warns_by_name() {
    let unlinked = secret(9).public();
    let mut app = send_app();
    write_sends(&mut app, unlinked, 1);

    let captured = capturing(|| app.update());
    assert!(
        captured.contains("no_session"),
        "the warn must name the counter an operator can grep for: {captured:?}"
    );
    assert!(
        captured.contains(&unlinked.to_string()),
        "the warn must name the peer it dropped a send for: {captured:?}"
    );
}

#[test]
fn a_flood_of_unlinked_sends_warns_once_per_window_not_once_per_packet() {
    let unlinked = secret(8).public();
    let mut app = send_app();
    write_sends(&mut app, unlinked, 50);

    let captured = capturing(|| app.update());
    let lines = captured.matches("no_session").count();
    assert_eq!(
        lines, 1,
        "one window, one line — a warn per packet trains its reader to stop reading: {captured:?}"
    );
}

#[test]
fn an_oversized_send_warns_by_name() {
    let peer = secret(7).public();
    let mut app = send_app();
    app.world_mut().spawn((
        Peer {
            id: peer,
            incoming: false,
        },
        aeronet_io::Session::new(Instant::now(), MTU),
        IrohStreamIo::detached(),
    ));
    // Framed is payload + the one-byte channel tag, so this exceeds the MTU.
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket::state(peer, Bytes::from(vec![0u8; MTU + 1])));

    let captured = capturing(|| app.update());
    assert!(
        captured.contains("oversized"),
        "the warn must name the counter an operator can grep for: {captured:?}"
    );
}

#[test]
fn an_untagged_inbound_packet_warns_by_name() {
    let sender = secret(6).public();
    let mut app = App::new();
    single_threaded(&mut app);
    app.add_plugins(MinimalPlugins)
        .add_message::<orrery_net::PeerPacket>()
        .init_resource::<PeerLinkCounters>()
        .add_systems(Update, receive_peer_packets);
    app.world_mut().spawn((
        Peer {
            id: sender,
            incoming: true,
        },
        aeronet_io::Session::new(Instant::now(), MTU),
    ));
    // `0xFF` is not a channel tag, so `untag` refuses the packet whole.
    app.world_mut()
        .query::<&mut aeronet_io::Session>()
        .iter_mut(app.world_mut())
        .next()
        .expect("the session exists")
        .recv
        .push(RecvPacket {
            recv_at: Instant::now(),
            payload: Bytes::from_static(&[0xFF]),
        });

    let captured = capturing(|| app.update());
    assert!(
        captured.contains("untagged"),
        "the warn must name the counter an operator can grep for: {captured:?}"
    );
    assert!(
        captured.contains(&sender.to_string()),
        "the warn must name the peer whose framing drifted: {captured:?}"
    );
}

#[test]
fn a_healthy_exchange_warns_nothing() {
    let peer = secret(5).public();
    let mut app = send_app();
    app.world_mut().spawn((
        Peer {
            id: peer,
            incoming: false,
        },
        aeronet_io::Session::new(Instant::now(), MTU),
        IrohStreamIo::detached(),
    ));
    write_sends(&mut app, peer, 10);

    let captured = capturing(|| app.update());
    assert!(
        captured.is_empty(),
        "a healthy exchange must stay quiet, or the fault warns stop being read: {captured:?}"
    );
}
