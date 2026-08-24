//! Two real peers on one impaired link.
//!
//! Builds a Bevy app carrying two `aeronet_iroh` endpoints — a *subject* that
//! authors and serves, and a *witness* that follows and repairs — attached to
//! the [`crate::impaired`] link. Everything above the socket is the shipping
//! code: `orrery_net`'s `send_peer_packets` and `receive_peer_packets`, its
//! channel policy, its upload meter, and `aeronet_iroh`'s two lanes.
//!
//! # Both peers in one process
//!
//! Not a shortcut. It is what makes a latency measurement possible without a
//! clock exchange: the send stamp and the arrival stamp come off the same
//! `Instant` clock, so a figure is a figure rather than a figure plus an
//! unknown offset. The two peers still hold separate endpoints, separate QUIC
//! connections and separate congestion state; nothing about the transport is
//! shared except the link they both send over, which is the point.
//!
//! # Why the upload budget is raised
//!
//! The default 1 Mbps would shed state to pay for repairs, and what that costs
//! was already measured — it is the finding this benchmark follows from. Here
//! the budget is set high enough not to bind, so the transport is what the
//! numbers describe. Bytes offered to the link are reported anyway, so a
//! transport that wins on latency by spending more cannot hide it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommand;

use aeronet_iroh::endpoint::IrohEndpoint;
use aeronet_iroh::session::{IrohSession, SessionRequest, SessionResponse, SessionSide};
use aeronet_iroh::{IrohPlugin, IrohRuntime};
use orrery_net::budget::{Bandwidth, UploadBudget, UploadMeter};
use orrery_net::peer_link::{
    receive_peer_packets, send_peer_packets, PeerLinkCounters, PeerPacket, SendPacket,
};
use orrery_net::plugin::Peer;
use orrery_protocol::NodeId;

use crate::impaired::{addr_of, ImpairedTransport, Impairment, Link as ImpairedLink};

/// The ALPN this benchmark's endpoints speak.
pub const ALPN: &[u8] = b"orrery/gates/p4-streams-bench/0";

/// A budget high enough not to bind. See the module docs.
const UNBOUNDED_UPLOAD: u64 = 50_000_000;

/// How long to wait on a connection before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The two peers and the link between them.
pub struct Link {
    /// The app both peers live in.
    pub app: App,
    /// The peer that authors frames and serves repairs.
    pub subject: NodeId,
    /// The peer that follows the subject and asks for repairs.
    pub witness: NodeId,
    /// The impaired link they talk over, and what it carried.
    pub wire: ImpairedLink,
}

/// Stand up two endpoints on an impaired link and connect them.
///
/// Both are built with `clear_ip_transports()`, so the impaired transport is
/// the only one they have. There is no better path for iroh to migrate to,
/// because there is no other path at all.
pub fn establish(
    runtime: &tokio::runtime::Handle,
    impairment: Impairment,
    seed: u64,
) -> Result<Link> {
    let wire = ImpairedLink::new(runtime.clone(), impairment, seed);

    // Identities are derived from the seed rather than random, so a rerun of a
    // seed is the same run — including which peer's id sorts first, which is
    // the sort of thing that quietly changes a tie-break somewhere.
    let subject_key = key_from_seed(seed, 1);
    let witness_key = key_from_seed(seed, 2);
    let subject = subject_key.public();
    let witness = witness_key.public();

    let subject_transport = wire.attach(subject).context("attaching the subject")?;
    let witness_transport = wire.attach(witness).context("attaching the witness")?;

    let mut app = App::new();
    // No `MinimalPlugins`: this benchmark drives `app.update()` itself, and a
    // runner would take the thread the measurement runs on. `Time` is added
    // because `send_peer_packets` meters against it.
    app.add_plugins(bevy_time::TimePlugin)
        .add_plugins(IrohPlugin)
        .insert_resource(IrohRuntime::from(runtime.clone()))
        .insert_resource(UploadBudget {
            sustained: Bandwidth::from_bits_per_sec(UNBOUNDED_UPLOAD),
            window: Duration::from_secs(1),
        })
        .init_resource::<UploadMeter>()
        .init_resource::<PeerLinkCounters>()
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .add_observer(|mut request: On<SessionRequest>| {
            request.respond(SessionResponse::Accepted);
        })
        .add_systems(
            Update,
            // Receive before send, so a reply built this frame goes out this
            // frame — the same ordering `OrreryNetPlugin` uses, and the reason
            // a repair does not cost a frame per hop.
            (track_peers, receive_peer_packets).chain(),
        )
        // Explicitly before the IO flush. Both lanes are drained by
        // `IoSystems::Flush`, and an unordered send system would hand its
        // packets over after the flush roughly half the time — a frame of
        // latency, applied at random, to a measurement whose subject is
        // latency.
        .add_systems(
            PostUpdate,
            send_peer_packets.before(aeronet_io::IoSystems::Flush),
        );

    let witness_endpoint = open_endpoint(&mut app, witness_key, witness_transport)?;
    let subject_endpoint = open_endpoint(&mut app, subject_key, subject_transport)?;
    wait_until(&mut app, "both endpoints to open", |world| {
        world.get::<IrohEndpoint>(witness_endpoint).is_some()
            && world.get::<IrohEndpoint>(subject_endpoint).is_some()
    })?;

    // The witness's address on the impaired link, and nothing else — there is
    // no IP address to offer even if something wanted to.
    let target = iroh::EndpointAddr::new(witness)
        .with_addrs([iroh::TransportAddr::Custom(addr_of(witness))]);
    let connect = app
        .world()
        .get::<IrohEndpoint>(subject_endpoint)
        .context("the subject endpoint just opened")?
        .connect(target, ALPN);
    let session = app.world_mut().spawn_empty().id();
    connect.apply(app.world_mut().entity_mut(session));

    wait_until(&mut app, "the two peers to connect", |world| {
        let mut sessions = world.query::<&aeronet_io::Session>();
        sessions.iter(world).count() >= 2
    })?;

    Ok(Link {
        app,
        subject,
        witness,
        wire,
    })
}

/// A deterministic secret key, so a seed reproduces a whole run.
fn key_from_seed(seed: u64, role: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    bytes[31] = role;
    iroh::SecretKey::from_bytes(&bytes)
}

/// Attaches an `orrery_net` [`Peer`] to every session the IO layer opens.
///
/// `OrreryNetPlugin` does this as part of a stack this benchmark does not want
/// (a coordinator client, island membership); the send path only needs the peer
/// identity, so that is all this adds.
fn track_peers(
    mut commands: Commands,
    sessions: Query<(Entity, &IrohSession), Added<aeronet_io::Session>>,
) {
    for (entity, iroh) in &sessions {
        commands.entity(entity).insert(Peer {
            id: iroh.peer_id(),
            incoming: iroh.side() == SessionSide::Incoming,
        });
    }
}

/// Open one endpoint whose only transport is the impaired link.
fn open_endpoint(
    app: &mut App,
    key: iroh::SecretKey,
    transport: Arc<ImpairedTransport>,
) -> Result<Entity> {
    let builder = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(iroh::RelayMode::Disabled)
        // Both halves matter. Clearing the IP transports removes the path iroh
        // would otherwise find and migrate to; adding the custom one gives it
        // the only path it has.
        .clear_ip_transports()
        .add_custom_transport(transport);
    let entity = app.world_mut().spawn_empty().id();
    IrohEndpoint::open(builder).apply(app.world_mut().entity_mut(entity));
    Ok(entity)
}

/// Run the app until `condition` holds, or fail after [`CONNECT_TIMEOUT`].
pub fn wait_until(
    app: &mut App,
    what: &str,
    mut condition: impl FnMut(&mut World) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        app.update();
        if condition(app.world_mut()) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}
