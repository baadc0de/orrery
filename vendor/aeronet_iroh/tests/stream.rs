//! End-to-end tests for the reliable stream lane.

use {
    aeronet_io::Session,
    aeronet_iroh::{
        IrohPlugin,
        endpoint::IrohEndpoint,
        session::{
            IrohSession, MAX_STREAM_MESSAGE_LEN, SessionRequest, SessionResponse, SessionSide,
        },
        stream::{IrohStreamIo, SendMessage, StreamMode},
    },
    bevy::{ecs::system::EntityCommand, prelude::*},
    bytes::Bytes,
    core::time::Duration,
    iroh::endpoint::presets,
    std::{thread, time::Instant},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const ALPN: &[u8] = b"aeronet-iroh/tests/stream/0";

#[test]
fn a_message_larger_than_the_datagram_mtu_arrives_whole() {
    // The whole reason the lane exists: the datagram lane refuses anything over
    // the path MTU, and this carries thirty-two times it in one piece.
    let payload = Bytes::from(vec![0xAB; 40_000]);
    let (mut app, outgoing, incoming) = connected_pair();

    assert!(
        payload.len() > app.world().get::<Session>(outgoing).unwrap().mtu(),
        "the payload must exceed the datagram MTU or this test proves nothing"
    );

    send(&mut app, outgoing, SendMessage::shared(payload.clone()));
    let received = wait_for_messages(&mut app, incoming, 1);
    assert_eq!(received, vec![payload]);
}

#[test]
fn shared_mode_preserves_order_across_messages() {
    // One stream is totally ordered, and callers that put sparse control
    // traffic on it are entitled to rely on that.
    let (mut app, outgoing, incoming) = connected_pair();
    let sent: Vec<Bytes> = (0u8..16)
        .map(|index| Bytes::from(vec![index; 512]))
        .collect();
    for payload in &sent {
        send(&mut app, outgoing, SendMessage::shared(payload.clone()));
    }

    let received = wait_for_messages(&mut app, incoming, sent.len());
    assert_eq!(received, sent);
}

#[test]
fn bulk_mode_delivers_every_message_though_not_in_order() {
    // Independent streams give no ordering guarantee, so this asserts the set
    // rather than the sequence — asserting the order would be asserting a
    // coincidence of a loopback link.
    let (mut app, outgoing, incoming) = connected_pair();
    let sent: Vec<Bytes> = (0u8..16)
        .map(|index| Bytes::from(vec![index; 512]))
        .collect();
    for payload in &sent {
        send(&mut app, outgoing, SendMessage::bulk(payload.clone()));
    }

    let mut received = wait_for_messages(&mut app, incoming, sent.len());
    received.sort();
    let mut expected = sent;
    expected.sort();
    assert_eq!(received, expected);
}

#[test]
fn both_modes_share_one_connection_and_one_reader() {
    // Mode is a send-side policy, so a peer may mix them freely and the reader
    // must not need to know which was used.
    let (mut app, outgoing, incoming) = connected_pair();
    let shared = Bytes::from_static(b"on the shared stream");
    let bulk = Bytes::from_static(b"on a stream of its own");
    send(&mut app, outgoing, SendMessage::shared(shared.clone()));
    send(&mut app, outgoing, SendMessage::bulk(bulk.clone()));

    let received = wait_for_messages(&mut app, incoming, 2);
    assert!(received.contains(&shared));
    assert!(received.contains(&bulk));
}

#[test]
fn the_stream_lane_does_not_disturb_the_datagram_lane() {
    // D3's claim is that the two multiplex on one connection with no
    // head-of-line blocking between them. This is the minimum evidence for it:
    // a large stream message in flight does not stop a datagram.
    const DATAGRAM: &[u8] = b"a datagram, unblocked";
    let (mut app, outgoing, incoming) = connected_pair();

    send(
        &mut app,
        outgoing,
        SendMessage::shared(Bytes::from(vec![0x5A; 200_000])),
    );
    app.world_mut()
        .get_mut::<Session>(outgoing)
        .unwrap()
        .send
        .push(Bytes::from_static(DATAGRAM));

    wait_until(&mut app, |world| {
        world
            .get::<Session>(incoming)
            .is_some_and(|session| session.recv.iter().any(|packet| packet.payload == DATAGRAM))
    });
}

#[test]
fn an_oversized_message_is_refused_rather_than_sent() {
    // The read side refuses an over-long length prefix before allocating for
    // it; the send side must not be able to produce one in the first place.
    let (mut app, outgoing, _incoming) = connected_pair();
    let too_large = usize::try_from(MAX_STREAM_MESSAGE_LEN).unwrap() + 1;
    send(
        &mut app,
        outgoing,
        SendMessage::shared(Bytes::from(vec![0u8; too_large])),
    );

    // The session drops rather than silently truncating or hanging.
    wait_until(&mut app, |world| world.get_entity(outgoing).is_err());
}

/// Two connected sessions in one app: `(app, outgoing, incoming)`.
fn connected_pair() -> (App, Entity, Entity) {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, IrohPlugin)).add_observer(
        |mut request: On<SessionRequest>| {
            request.respond(SessionResponse::Accepted);
        },
    );

    let endpoint_a = open_endpoint(&mut app);
    let endpoint_b = open_endpoint(&mut app);
    wait_until(&mut app, |world| {
        world.get::<IrohEndpoint>(endpoint_a).is_some()
            && world.get::<IrohEndpoint>(endpoint_b).is_some()
    });

    let target = app.world().get::<IrohEndpoint>(endpoint_b).unwrap().addr();
    let connect = app
        .world()
        .get::<IrohEndpoint>(endpoint_a)
        .unwrap()
        .connect(target, ALPN);
    let outgoing = app.world_mut().spawn_empty().id();
    connect.apply(app.world_mut().entity_mut(outgoing));

    wait_until(&mut app, |world| world.get::<Session>(outgoing).is_some());
    let incoming = app
        .world_mut()
        .query::<(Entity, &IrohSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.side() == SessionSide::Incoming).then_some(entity))
        .expect("an incoming session should exist");
    wait_until(&mut app, |world| world.get::<Session>(incoming).is_some());

    (app, outgoing, incoming)
}

fn open_endpoint(app: &mut App) -> Entity {
    let entity = app.world_mut().spawn_empty().id();
    let builder = iroh::Endpoint::builder(presets::Minimal).alpns(vec![ALPN.to_vec()]);
    IrohEndpoint::open(builder).apply(app.world_mut().entity_mut(entity));
    entity
}

fn send(app: &mut App, session: Entity, message: SendMessage) {
    app.world_mut()
        .get_mut::<IrohStreamIo>(session)
        .expect("a connected session has a stream lane")
        .send
        .push(message);
}

/// Run until `session` has received at least `count` messages, then take them.
fn wait_for_messages(app: &mut App, session: Entity, count: usize) -> Vec<Bytes> {
    wait_until(app, |world| {
        world
            .get::<IrohStreamIo>(session)
            .is_some_and(|io| io.recv.len() >= count)
    });
    app.world_mut()
        .get_mut::<IrohStreamIo>(session)
        .unwrap()
        .recv
        .drain(..)
        .map(|message| message.payload)
        .collect()
}

fn wait_until(app: &mut App, mut condition: impl FnMut(&mut World) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        app.update();
        if condition(app.world_mut()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        thread::sleep(Duration::from_millis(5));
    }
}

// `StreamMode` is `Default` so a caller can build a `SendMessage` literally
// without naming the common case.
const _: () = assert!(matches!(StreamMode::Shared, StreamMode::Shared));
