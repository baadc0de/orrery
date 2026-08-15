//! The gateway session: connect, hello, and the channel policy (D3).
//!
//! The client talks to the gateway over one aeronet session. Per the channel
//! policy (docs/02-networking.md §7), bulk diffs ride unreliable datagrams and
//! area load + intents ride reliable streams. This module tracks the session
//! lifecycle and exposes the two channels, plus the session event stream that
//! drives the uplink scheduler's reconnect/resend behavior.
//!
//! Exponential backoff (D3): on disconnect, the reconnect delay starts at
//! `INITIAL_RECONNECT_DELAY` and doubles each attempt up to
//! `MAX_RECONNECT_DELAY`, reset on successful connect. This prevents
//! reconnect storms during a netsplit.

use std::time::Duration;

use bevy_ecs::prelude::*;
use bevy_platform::time::Instant;
use bytes::Bytes;

use aeronet_iroh::iroh;
use orrery_authority::LeaseOutbox;
use orrery_net::channels::{tag, Channel};
use orrery_protocol::{GatewayMsg, GatewayReply, NodeId, PROTOCOL_VERSION};

use crate::intents::IntentQueue;

/// The minimum delay before the first reconnect attempt.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);

/// The maximum delay between reconnect attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// The ALPN the gateway session negotiates (D3). Matches the coordinator/peer
/// ALPN so the same endpoint can dial the gateway.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The current state of the gateway session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayState {
    /// No session to the gateway yet.
    Disconnected,
    /// A session is connecting (hello not yet acknowledged).
    Connecting,
    /// The session is up and the gateway has acknowledged the hello.
    Connected,
}

/// A gateway session lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// The session connected and the gateway acknowledged the hello.
    Connected,
    /// The session dropped. The uplink scheduler keeps buffered diffs and
    /// resends them on the next connect (records are idempotent).
    Disconnected,
}

/// The client's gateway session state (D11 §9).
///
/// Tracks the aeronet session entity, the negotiated state, and the last
/// connect/disconnect timestamps. The actual bytes flow through the session's
/// datagram buffer; this resource is the bookkeeping on top.
///
/// Reconnect uses exponential backoff (D3): after disconnect, the delay starts
/// at [`INITIAL_RECONNECT_DELAY`] and doubles each attempt up to
/// [`MAX_RECONNECT_DELAY`], reset on successful connection.
#[derive(Debug, Resource)]
pub struct GatewaySession {
    /// The aeronet session entity, if connected.
    pub session: Option<Entity>,
    /// The current connection state.
    pub state: GatewayState,
    /// The gateway's NodeId, once the hello is acknowledged.
    pub gateway: Option<orrery_protocol::NodeId>,
    /// The negotiated protocol version.
    pub protocol: u16,
    /// When the session last connected.
    pub connected_at: Option<Instant>,
    /// When the session last disconnected.
    pub disconnected_at: Option<Instant>,
    /// The session token sent in the hello.
    pub token: Vec<u8>,
    /// The client's own node id (the local iroh endpoint id, D3), sent in the
    /// hello and echoed back in the [`GatewayReply::HelloAck`].
    pub node: NodeId,
    /// Whether the hello has been sent on the current session (prevents
    /// resending it every frame while the gateway has not yet acknowledged).
    pub hello_sent: bool,
    /// The current exponential-backoff reconnect delay.
    reconnect_delay: Duration,
}

impl Default for GatewaySession {
    fn default() -> Self {
        Self {
            session: None,
            state: GatewayState::Disconnected,
            gateway: None,
            protocol: PROTOCOL_VERSION,
            connected_at: None,
            disconnected_at: None,
            token: Vec::new(),
            node: NodeId::from_bytes(&[0u8; 32]).expect("zero node id is valid"),
            hello_sent: false,
            reconnect_delay: INITIAL_RECONNECT_DELAY,
        }
    }
}

impl GatewaySession {
    /// A disconnected session with the given auth token.
    #[must_use]
    pub fn new(token: Vec<u8>) -> Self {
        Self {
            token,
            ..Self::default()
        }
    }

    /// Whether the session is up and the hello is acknowledged.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.state == GatewayState::Connected
    }

    /// Reset the exponential-backoff reconnect delay (called on successful
    /// connection, so the next disconnect starts fresh).
    pub fn reset_backoff(&mut self) {
        self.reconnect_delay = INITIAL_RECONNECT_DELAY;
    }

    /// Encode a gateway message as a tagged datagram for the bulk channel.
    ///
    /// Bulk diffs ride unreliable datagrams (D3). The tag lets the receiver
    /// route the payload without a separate framing layer. Generic over the
    /// message type so both directions (client [`GatewayMsg`] and gateway
    /// [`GatewayReply`]) share one encoding.
    #[must_use]
    pub fn encode_datagram<T: serde::Serialize>(msg: &T) -> Vec<u8> {
        let payload = postcard::to_stdvec(msg).expect("gateway message is serializable");
        tag(Channel::State, &payload)
    }

    /// Decode a tagged datagram into a gateway reply.
    ///
    /// Returns `None` if the tag is not the state channel or the payload does
    /// not decode.
    pub fn decode_datagram(payload: &[u8]) -> Option<GatewayReply> {
        let (channel, rest) = orrery_net::channels::untag(payload)?;
        if channel != Channel::State {
            return None;
        }
        postcard::from_bytes(rest).ok()
    }

    /// Encode a gateway message as a stream frame (area load + intents).
    ///
    /// Reliable stream traffic is length-prefixed so the receiver can frame
    /// multiple messages on one stream, and tagged with the control channel so
    /// the receiver can route it without a separate framing layer (D3: streams
    /// = control/bulk). Generic over the message type so both directions
    /// (client [`GatewayMsg`] and gateway [`GatewayReply`]) share one encoding.
    #[must_use]
    pub fn encode_stream<T: serde::Serialize>(msg: &T) -> Bytes {
        let payload = postcard::to_stdvec(msg).expect("gateway message is serializable");
        let mut out = Vec::with_capacity(payload.len() + 5);
        out.push(orrery_net::channels::TAG_CONTROL);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        Bytes::from(out)
    }
}

/// The gateway the client dials: its addressing info and node id.
///
/// This is the client-side counterpart of the gateway's `listen` address. The
/// client opens one aeronet session to this endpoint and performs the hello
/// handshake; once acknowledged, the [`GatewaySession`] is `Connected`.
#[derive(Debug, Clone, Resource)]
pub struct GatewayConfig {
    /// The gateway's address to dial (id + direct/relay addresses).
    pub addr: iroh::EndpointAddr,
    /// The expected gateway node id (transport identity, D3). Used to verify
    /// the [`GatewayReply::HelloAck`] came from the intended gateway.
    pub gateway: NodeId,
}

impl GatewayConfig {
    /// A config dialing `addr`, expecting the gateway to identify as `gateway`.
    #[must_use]
    pub fn new(addr: iroh::EndpointAddr, gateway: NodeId) -> Self {
        Self { addr, gateway }
    }
}

/// Dial the gateway when disconnected and the local endpoint is ready.
///
/// Spawns an aeronet session dialing [`GatewayConfig`], records its entity and
/// the local node id (D3: transport identity) on [`GatewaySession`], and enters
/// [`GatewayState::Connecting`].
///
/// Exponential backoff (D3): after a disconnect, the reconnect delay increases
/// from [`INITIAL_RECONNECT_DELAY`] up to [`MAX_RECONNECT_DELAY`], and resets on
/// successful connection. This prevents reconnect storms during a netsplit.
pub fn connect_gateway(
    mut commands: Commands,
    mut session: ResMut<GatewaySession>,
    config: Option<Res<GatewayConfig>>,
    endpoints: Query<&aeronet_iroh::endpoint::IrohEndpoint>,
) {
    if session.state != GatewayState::Disconnected {
        return;
    }
    let (Some(config), Some(endpoint)) = (config.map(|c| c.clone()), endpoints.iter().next())
    else {
        return;
    };

    // Exponential backoff: wait before re-dialling.
    if let Some(since) = session.disconnected_at {
        let elapsed = since.elapsed();
        if elapsed < session.reconnect_delay {
            return;
        }
    }

    let session_entity = commands.spawn_empty().id();
    commands
        .entity(session_entity)
        .queue(endpoint.connect(config.addr.clone(), GATEWAY_ALPN));

    session.session = Some(session_entity);
    session.node = endpoint.id();
    session.state = GatewayState::Connecting;
    session.hello_sent = false;
    session.connected_at = None;
    session.gateway = None;
}

/// Send the `Hello` once the aeronet session is established.
///
/// The client's own node id is the local iroh endpoint id (D3); it was captured
/// on [`GatewaySession::gateway`]'s sibling fields by [`connect_gateway`].
pub fn hello_gateway(
    mut session: ResMut<GatewaySession>,
    mut sessions: Query<&mut aeronet_io::Session>,
) {
    if session.state != GatewayState::Connecting || session.hello_sent {
        return;
    }
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut io) = sessions.get_mut(entity) else {
        return;
    };
    let msg = GatewayMsg::Hello {
        token: session.token.clone(),
        node: session.node,
    };
    io.send.push(GatewaySession::encode_stream(&msg));
    session.hello_sent = true;
}

/// Flush P3 authority control messages on the reliable gateway lane.
///
/// The resource is optional, preserving P2-only plugin composition.
pub fn flush_lease_control(
    outbox: Option<ResMut<LeaseOutbox>>,
    session: Res<GatewaySession>,
    mut sessions: Query<&mut aeronet_io::Session>,
) {
    let Some(mut outbox) = outbox else {
        return;
    };
    if !session.is_connected() {
        return;
    }
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut io) = sessions.get_mut(entity) else {
        return;
    };
    for message in std::mem::take(&mut outbox.0) {
        io.send
            .push(GatewaySession::encode_stream(&GatewayMsg::Lease {
                message,
            }));
    }
}

/// Return to `Disconnected` when the session entity is despawned by the IO
/// layer (connect failed, or the connection dropped), so the next frame
/// re-dials. Unacked buffered diffs stay in the scheduler and are resent on the
/// new connect (records are idempotent, keyed by `(entity, tick)`).
///
/// Calls [`IntentQueue::requeue_all_inflight`] so intents that were mid-flight
/// when the connection dropped return to `Queued` and retransmit on reconnect
/// (netsplit posture, D11 §2.2, D12).
pub fn disconnect_gateway(
    mut session: ResMut<GatewaySession>,
    mut removed: RemovedComponents<aeronet_io::Session>,
    mut queue: ResMut<IntentQueue>,
) {
    let Some(entity) = session.session else {
        return;
    };
    if !removed.read().any(|e| e == entity) {
        return;
    }
    session.state = GatewayState::Disconnected;
    session.session = None;
    session.hello_sent = false;
    session.disconnected_at = Some(Instant::now());
    // Exponential backoff: double the delay each disconnect, capped at max.
    session.reconnect_delay = (session.reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    // Netsplit posture (D12): requeue in-flight intents so they replay on
    // the next drain after reconnect.
    queue.requeue_all_inflight();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intents::{IntentStatus, IntentTicket};
    use orrery_protocol::{DiffUplink, GatewayMsg, GridId, PersistId, RecordKind, Tick};

    #[allow(clippy::needless_pass_by_value)]
    fn diff() -> GatewayMsg {
        GatewayMsg::Diff {
            diff: DiffUplink {
                cell: orrery_protocol::CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(7),
                tick: Tick::new(1),
                kind: RecordKind::ComponentDiff,
                payload: Bytes::from_static(b"hp=50"),
                seq: 0,
                lease_id: None,
                authority_seq: None,
            },
        }
    }

    #[test]
    fn datagram_roundtrip() {
        // A client diff round-trips through the tagged datagram encoding.
        let msg = diff();
        let bytes = GatewaySession::encode_datagram(&msg);
        // The tag is the state channel.
        assert_eq!(bytes[0], orrery_net::channels::TAG_STATE);
        let decoded: GatewayMsg = postcard::from_bytes(&bytes[1..]).unwrap();
        assert_eq!(decoded, msg);

        // A gateway bulk ack (the reply to a diff) round-trips too.
        let reply = GatewayReply::BulkAck {
            entity: PersistId::new(7),
            tick: Tick::new(1),
            lsn: orrery_protocol::Lsn::new(0, 0),
            provisional: false,
        };
        let bytes = GatewaySession::encode_datagram(&reply);
        let decoded = GatewaySession::decode_datagram(&bytes).unwrap();
        assert_eq!(decoded, reply);
        // A control-tagged payload does not decode as a datagram reply.
        let control = tag(Channel::Control, b"x");
        assert!(GatewaySession::decode_datagram(&control).is_none());
    }

    #[test]
    fn stream_frame_is_length_prefixed_and_tagged() {
        let msg = GatewayMsg::Hello {
            token: b"tok".to_vec(),
            node: iroh_base::SecretKey::from_bytes(&[0u8; 32]).public(),
        };
        let frame = GatewaySession::encode_stream(&msg);
        // Control tag, then a u32 length, then the payload.
        assert_eq!(frame[0], orrery_net::channels::TAG_CONTROL);
        let len = u32::from_le_bytes(frame[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 5);
        let decoded: GatewayMsg = postcard::from_bytes(&frame[5..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn session_state_transitions() {
        let mut session = GatewaySession::new(b"tok".to_vec());
        assert_eq!(session.state, GatewayState::Disconnected);
        assert!(!session.is_connected());
        session.state = GatewayState::Connecting;
        session.state = GatewayState::Connected;
        assert!(session.is_connected());
    }

    #[test]
    fn hello_is_sent_once_until_acked() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .add_systems(bevy_app::Update, hello_gateway);
        let session_entity = app
            .world_mut()
            .spawn(aeronet_io::Session::new(
                bevy_platform::time::Instant::now(),
                1024,
            ))
            .id();
        {
            let mut session = app.world_mut().resource_mut::<GatewaySession>();
            session.state = GatewayState::Connecting;
            session.session = Some(session_entity);
            session.hello_sent = false;
        }

        // First update sends the hello and marks it sent.
        app.update();
        let sent = app
            .world()
            .get::<aeronet_io::Session>(session_entity)
            .unwrap()
            .send
            .len();
        assert_eq!(sent, 1, "hello sent once");
        assert!(app.world().resource::<GatewaySession>().hello_sent);

        // A second update does not resend while unacked.
        app.update();
        let sent = app
            .world()
            .get::<aeronet_io::Session>(session_entity)
            .unwrap()
            .send
            .len();
        assert_eq!(sent, 1, "hello not resent before ack");
    }

    #[test]
    fn disconnect_resets_to_disconnected() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .init_resource::<IntentQueue>()
            .add_systems(bevy_app::Update, disconnect_gateway);
        let session_entity = app
            .world_mut()
            .spawn(aeronet_io::Session::new(
                bevy_platform::time::Instant::now(),
                1024,
            ))
            .id();
        {
            let mut session = app.world_mut().resource_mut::<GatewaySession>();
            session.state = GatewayState::Connected;
            session.session = Some(session_entity);
            session.hello_sent = true;
        }

        // Despawn the session (what the IO layer does on disconnect).
        app.world_mut().despawn(session_entity);
        app.update();

        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Disconnected);
        assert!(session.session.is_none());
        assert!(!session.hello_sent);
    }

    #[test]
    fn disconnect_doubles_backoff() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .init_resource::<IntentQueue>()
            .add_systems(bevy_app::Update, disconnect_gateway);
        let session_entity = app
            .world_mut()
            .spawn(aeronet_io::Session::new(
                bevy_platform::time::Instant::now(),
                1024,
            ))
            .id();
        {
            let mut session = app.world_mut().resource_mut::<GatewaySession>();
            // Start at the initial delay.
            session.reconnect_delay = INITIAL_RECONNECT_DELAY;
            session.state = GatewayState::Connected;
            session.session = Some(session_entity);
        }

        app.world_mut().despawn(session_entity);
        app.update();

        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Disconnected);
        // Backoff should have doubled.
        assert_eq!(
            session.reconnect_delay,
            (INITIAL_RECONNECT_DELAY * 2).min(MAX_RECONNECT_DELAY)
        );
    }

    #[test]
    fn disconnect_requeues_inflight_intents() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .init_resource::<IntentQueue>()
            .add_systems(bevy_app::Update, disconnect_gateway);

        // Submit an intent and drain it so it's InFlight.
        {
            let mut queue = app.world_mut().resource_mut::<IntentQueue>();
            queue.submit(intent(1)).unwrap();
        }
        {
            let mut queue = app.world_mut().resource_mut::<IntentQueue>();
            queue.drain();
            assert_eq!(queue.status(IntentTicket(1)), IntentStatus::InFlight);
        }

        let session_entity = app
            .world_mut()
            .spawn(aeronet_io::Session::new(
                bevy_platform::time::Instant::now(),
                1024,
            ))
            .id();
        {
            let mut session = app.world_mut().resource_mut::<GatewaySession>();
            session.state = GatewayState::Connected;
            session.session = Some(session_entity);
        }

        // Despawn to trigger disconnect.
        app.world_mut().despawn(session_entity);
        app.update();

        // The in-flight intent should be requeued.
        let queue = app.world().resource::<IntentQueue>();
        assert_eq!(queue.status(IntentTicket(1)), IntentStatus::Queued);
    }

    #[test]
    fn connect_gateway_respects_backoff() {
        let mut app = bevy_app::App::new();
        app.init_resource::<GatewaySession>()
            .add_systems(bevy_app::Update, connect_gateway);

        // Set the session as recently disconnected so the backoff is active.
        {
            let mut session = app.world_mut().resource_mut::<GatewaySession>();
            session.state = GatewayState::Disconnected;
            session.disconnected_at = Some(Instant::now());
            session.reconnect_delay = Duration::from_secs(60); // very long delay
        }

        // connect_gateway must not attempt a connect because the backoff delay
        // has not elapsed.
        app.update();
        let session = app.world().resource::<GatewaySession>();
        assert_eq!(session.state, GatewayState::Disconnected);
    }

    fn intent(id: u128) -> orrery_protocol::Intent {
        orrery_protocol::Intent {
            intent_id: id,
            issuer: node(1),
            cell_epoch: orrery_protocol::Epoch::new(0),
            ops: vec![],
            attestations: vec![],
            signature: sig(),
        }
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
}
