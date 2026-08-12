//! The `OrreryNetPlugin` — minimal P0 skeleton (docs/11-roadmap.md §P0).
//!
//! Owns everything about being *on the network* that is not replication:
//! bootstrapping the iroh endpoint via `orrery_aeronet_iroh`, peer connect/
//! disconnect tracking, and relay-path telemetry aggregation. The coordinator
//! client and island membership land with P1.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use aeronet_iroh::endpoint::IrohEndpoint;
use aeronet_iroh::session::{PathKind, PathReport, SessionRequest, SessionResponse};
use aeronet_iroh::{IrohPlugin, IrohRuntime};
use iroh::endpoint::Builder;

/// The application protocol this endpoint advertises and accepts (D3). Matches
/// the ALPN the coordinator and peers use.
pub const ALPN: &[u8] = b"orrery/0";

/// Configuration for the [`OrreryNetPlugin`].
///
/// Stores the cloneable pieces of the iroh endpoint (relay mode, optional
/// secret key); the [`Builder`] is assembled in the Startup system because it
/// is not `Clone`.
#[derive(Debug, Clone, Resource)]
pub struct NetConfig {
    /// The relay mode: the punch rendezvous + fallback (D3). Defaults to
    /// iroh's production relay map.
    pub relay_mode: iroh::RelayMode,
    /// An optional secret key, pinning a stable NodeId across runs (needed for
    /// mesh rosters). `None` generates a fresh identity per bind.
    pub secret_key: Option<iroh::SecretKey>,
}

impl NetConfig {
    /// A default config: iroh's production relay map, ephemeral identity.
    #[must_use]
    pub fn default_builder() -> Builder {
        iroh::endpoint::Builder::new(iroh::endpoint::presets::N0).alpns(vec![ALPN.to_vec()])
    }

    /// Assemble the iroh endpoint [`Builder`] from this config.
    #[must_use]
    pub fn builder(&self) -> Builder {
        let mut builder = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(self.relay_mode.clone());
        if let Some(key) = &self.secret_key {
            builder = builder.secret_key(key.clone());
        }
        builder
    }
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            relay_mode: iroh::RelayMode::Default,
            secret_key: None,
        }
    }
}

/// The orrery networking plugin (P0 skeleton).
///
/// Adds the iroh IO layer, opens an endpoint from [`NetConfig`], accepts
/// incoming sessions, and tracks peers + relay-path telemetry.
#[derive(Default)]
pub struct OrreryNetPlugin {
    /// Endpoint configuration.
    pub config: NetConfig,
}

impl Plugin for OrreryNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(IrohPlugin)
            .init_resource::<IrohRuntime>()
            .insert_resource(self.config.clone())
            .init_resource::<PeerRegistry>()
            .add_systems(Startup, open_endpoint)
            .add_systems(Update, (track_peers, track_paths))
            .add_observer(on_session_request);
    }
}

/// Opens the iroh endpoint from [`NetConfig`] (Startup).
fn open_endpoint(mut commands: Commands, config: Res<NetConfig>) {
    let builder = config.builder();
    commands.spawn_empty().queue(IrohEndpoint::open(builder));
}

/// Accepts every incoming session (P0 skeleton: no admission policy yet).
/// `orrery_identity` will gate this in P5.
fn on_session_request(mut request: On<SessionRequest>) {
    request.respond(SessionResponse::Accepted);
}

/// A connected peer, tracked by the [`PeerRegistry`].
#[derive(Debug, Clone, Component)]
pub struct Peer {
    /// The remote iroh endpoint id (transport identity, D3).
    pub id: iroh::EndpointId,
    /// Whether this peer initiated the connection.
    pub incoming: bool,
}

/// Registry of connected peers, keyed by session entity.
#[derive(Debug, Default, Resource)]
pub struct PeerRegistry {
    /// Session entity -> peer.
    pub peers: Vec<(Entity, Peer)>,
}

impl PeerRegistry {
    /// The number of connected peers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether no peers are connected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// Tracks session connect/disconnect into the [`PeerRegistry`].
fn track_peers(
    mut commands: Commands,
    mut registry: ResMut<PeerRegistry>,
    sessions: Query<
        (
            Entity,
            &aeronet_io::Session,
            &aeronet_iroh::session::IrohSession,
        ),
        Added<aeronet_io::Session>,
    >,
    mut disconnected: RemovedComponents<aeronet_io::Session>,
) {
    for (entity, _session, iroh) in &sessions {
        let peer = Peer {
            id: iroh.peer_id(),
            incoming: iroh.side() == aeronet_iroh::session::SessionSide::Incoming,
        };
        registry.peers.retain(|(e, _)| *e != entity);
        registry.peers.push((entity, peer));
    }
    for entity in disconnected.read() {
        registry.peers.retain(|(e, _)| *e != entity);
        commands.entity(entity).try_despawn();
    }
}

/// Aggregates relay-path telemetry into a resource for the dashboard and the
/// coordinator (P1).
#[derive(Debug, Default, Resource)]
pub struct PathTelemetry {
    /// Per-session path reports.
    pub reports: Vec<(Entity, PathReport)>,
}

/// Collects [`PathReport`] from each session into [`PathTelemetry`].
fn track_paths(
    mut telemetry: ResMut<PathTelemetry>,
    sessions: Query<(Entity, &PathReport), With<aeronet_io::Session>>,
) {
    telemetry.reports.clear();
    for (entity, report) in &sessions {
        telemetry.reports.push((entity, report.clone()));
    }
}

/// A convenience accessor: whether a session is on a direct path.
#[must_use]
pub fn is_direct(report: &PathReport) -> bool {
    matches!(report.kind, PathKind::Direct)
}
