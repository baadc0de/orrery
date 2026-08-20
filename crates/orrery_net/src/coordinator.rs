//! The coordinator client (P1, docs/10-crates.md §4).
//!
//! Dials the coordinator, keeps the session alive across drops, and turns what
//! arrives into ECS state: [`IslandMembership`] from manifests, [`ActiveInterest`]
//! from signed grants.
//!
//! # One endpoint, one identity
//!
//! The session runs over the *game* endpoint (`IrohEndpoint::raw`), not a
//! private one. A peer that talked to the coordinator under a second NodeId
//! would be handed manifests naming an identity its island-mates cannot dial,
//! and the interest grant — which is bound to a peer key — would authorize
//! nobody. The coordinator ALPN is only ever dialled out, never accepted, so it
//! does not join the endpoint's accept list.
//!
//! # The coordinator is not on the data path
//!
//! Losing the coordinator does **not** dissolve the island. Peers replicate to
//! each other directly (D6); the coordinator hands out membership and interest
//! and is otherwise absent from the steady state. Tearing membership down on a
//! coordinator blip would drop every session in the island for the duration of a
//! reconnect, which is a far worse failure than running briefly on a stale
//! manifest. So a disconnect updates [`LinkStatus`] and nothing else.
//!
//! # Relationship to `orrery_coordinator::client`
//!
//! That client binds its own endpoint, for processes with no ECS to attach to
//! (the authority harness, integration tests). This one is the Bevy path. They
//! are deliberately separate — docs/10-crates.md §4 places the coordinator
//! client in `orrery_net`, and depending on the coordinator *server* crate from
//! every game client would invert the layering. What keeps them from drifting is
//! not shared code but `tests/coordinator_session.rs`, which drives this client
//! against the real [`orrery_coordinator::CoordinatorServer`].

use core::time::Duration;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bytes::Bytes;
use tokio::sync::mpsc;

use aeronet_iroh::endpoint::IrohEndpoint;
use aeronet_tokio_runtime::TokioRuntime;

use orrery_protocol::channels::{decode_stream_frame, encode_stream_frame, untag, Channel};
use orrery_protocol::coord::{
    verify_interest_grant, InterestGrantClaimsV1, InterestGrantVerificationError, IslandId,
    IslandManifest, COORD_ALPN, MAX_PRESENCE_CELLS,
};
use orrery_protocol::{CellId, CoordMsg, IssuerKey, NodeId};

use crate::island::{IslandMembership, NetEvent};
use crate::plugin::{PeerRegistry, ALPN};

/// How to reach the coordinator, and whose signatures to trust.
#[derive(Debug, Clone, Resource)]
pub struct CoordinatorConfig {
    /// The coordinator's address. `None` runs the coordinator-less path — see
    /// [`crate::island::IslandSource::ConnectedPeers`].
    pub address: Option<iroh::EndpointAddr>,
    /// The session token from `orrery_identity` login.
    pub token: Vec<u8>,
    /// The coordinator issuer keys whose interest grants this peer will accept.
    ///
    /// Empty means "cannot verify": grants are still forwarded to the gateway,
    /// which verifies them itself, but this peer will not read their claims. See
    /// [`ActiveInterest::accept`].
    pub issuer_keys: Vec<IssuerKey>,
    /// How long to wait for the admission stream and the welcome.
    pub handshake_timeout: Duration,
    /// How long to wait before redialling after a dropped session.
    pub reconnect_delay: Duration,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            address: None,
            token: Vec::new(),
            issuer_keys: Vec::new(),
            handshake_timeout: Duration::from_secs(10),
            reconnect_delay: Duration::from_secs(2),
        }
    }
}

/// Something the coordinator session produced, on its way into the ECS.
#[derive(Debug, Clone)]
pub enum CoordinatorUpdate {
    /// The handshake completed; the coordinator identified itself.
    Connected {
        /// The coordinator's NodeId, as it named itself in the welcome.
        coordinator: NodeId,
    },
    /// An island membership handout.
    Manifest(IslandManifest),
    /// A postcard-encoded `InterestGrantV1`, still opaque.
    Grant(Vec<u8>),
    /// The coordinator asked this island to drain by a deadline.
    Drain {
        /// The island to drain.
        island: IslandId,
        /// Drain deadline as unix milliseconds.
        deadline: u64,
    },
    /// The session ended. The task will redial.
    Disconnected {
        /// Why, for logs and [`LinkStatus`].
        reason: String,
    },
}

/// Something the ECS wants to tell the coordinator.
#[derive(Debug, Clone)]
pub enum CoordinatorRequest {
    /// Report the cells this peer's active interest covers.
    Presence(Vec<CellId>),
}

/// Where the coordinator session currently stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LinkStatus {
    /// No coordinator is configured; the coordinator-less path is running.
    #[default]
    Disabled,
    /// Dialling, or redialling after a drop.
    Connecting,
    /// Handshake complete.
    Connected {
        /// The coordinator that answered.
        coordinator: NodeId,
    },
    /// The session dropped; a redial is pending.
    Disconnected {
        /// Why the last session ended.
        reason: String,
    },
}

/// The ECS end of the coordinator session.
#[derive(Resource)]
pub struct CoordinatorLink {
    inbound: mpsc::UnboundedReceiver<CoordinatorUpdate>,
    outbound: mpsc::UnboundedSender<CoordinatorRequest>,
    /// Where the session stands.
    pub status: LinkStatus,
    /// The most recent drain order, if one is outstanding.
    ///
    /// Written by [`pump_coordinator`] and consumed, in the same frame, by
    /// [`apply_island_drain`] — which is the only thing that clears it. It is
    /// the hand-off point between the session layer and everything a drain
    /// actually costs: releasing leases is authority's business (D7), and the
    /// wire that does it lives above both crates in the `orrery` facade, so the
    /// order is parked here for one system-ordering hop rather than acted on
    /// where it lands.
    ///
    /// The order is **advisory** (D24 §(b)). If this peer had crashed there
    /// would be no session to deliver it on, and the registrar's 1 s expiry
    /// sweep would still park every row within `TTL + S = 11 s`. Honouring it
    /// converts that into a one-RTT cooperative release on the graceful
    /// departures, which are the majority — it buys latency, never correctness,
    /// and nothing downstream may come to depend on the notice arriving.
    pub drain: Option<(IslandId, u64)>,
}

impl CoordinatorLink {
    /// Report this peer's covered cells.
    ///
    /// Queued rather than sent: the session may be redialling. The task keeps
    /// only the newest report and replays it on reconnect, because presence is
    /// current state rather than an event — a coordinator that came back to a
    /// silent peer would hold a stale interest set until the peer next moved.
    ///
    /// Reports longer than [`MAX_PRESENCE_CELLS`] are refused; the coordinator
    /// would reject them anyway, and truncating would silently narrow interest.
    pub fn report_presence(&self, cells: Vec<CellId>) -> bool {
        if cells.len() > MAX_PRESENCE_CELLS {
            return false;
        }
        self.outbound
            .send(CoordinatorRequest::Presence(cells))
            .is_ok()
    }

    /// Whether the session is up.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self.status, LinkStatus::Connected { .. })
    }
}

/// This peer's coordinator-confirmed active interest (D12).
///
/// The `grant` bytes are what a gateway wants: it verifies the coordinator
/// signature itself, so the peer is a courier rather than a trusted reporter.
#[derive(Debug, Default, Resource)]
pub struct ActiveInterest {
    /// The verified claims, if this peer could verify them.
    pub claims: Option<InterestGrantClaimsV1>,
    /// The opaque grant to forward to a gateway.
    pub grant: Vec<u8>,
    /// Grants accepted without verification, because no issuer keys are
    /// configured.
    pub unverified: u64,
    /// Grants refused: bad signature, wrong peer, or out of bounds.
    pub rejected: u64,
    /// Grants dropped for carrying an epoch no newer than the one held.
    pub stale: u64,
}

impl ActiveInterest {
    /// Take a grant off the wire.
    ///
    /// # Verification
    ///
    /// With issuer keys configured the grant is verified here, which also
    /// confirms the coordinator issued it to *this* peer rather than relaying
    /// someone else's — `verify_interest_grant` binds the presenter. Without
    /// keys the bytes are still stored for forwarding, because the gateway is
    /// the party that must verify and it always does; what this peer loses is
    /// its own view of its covered cells, not the gateway's safety.
    ///
    /// # Epoch gating
    ///
    /// A grant is dropped unless it is strictly newer than the one held. The
    /// gateway applies the same rule, so forwarding a replayed older grant could
    /// not widen anything — but a peer that overwrote its own state with one
    /// would forward the *narrower* stale grant afterwards and lose coverage it
    /// still has.
    ///
    /// # Errors
    ///
    /// [`InterestGrantVerificationError`] when keys are configured and the grant
    /// does not verify against them.
    pub fn accept(
        &mut self,
        encoded: Vec<u8>,
        local: NodeId,
        keys: &[IssuerKey],
    ) -> Result<(), InterestGrantVerificationError> {
        if keys.is_empty() {
            self.unverified += 1;
            self.grant = encoded;
            self.claims = None;
            return Ok(());
        }

        let claims = verify_interest_grant(&encoded, &local, keys).inspect_err(|_| {
            self.rejected += 1;
        })?;

        if let Some(held) = &self.claims {
            if claims.epoch <= held.epoch {
                self.stale += 1;
                return Ok(());
            }
        }

        self.grant = encoded;
        self.claims = Some(claims);
        Ok(())
    }
}

/// Adds the coordinator client to an app.
///
/// Registered by [`crate::OrreryNetPlugin`]; separate so a headless test can
/// drive the session without the rest of the net stack.
pub struct CoordinatorPlugin {
    /// Coordinator address, credentials, and trusted issuer keys.
    pub config: CoordinatorConfig,
}

impl Plugin for CoordinatorPlugin {
    fn build(&self, app: &mut App) {
        let (to_ecs, inbound) = mpsc::unbounded_channel();
        let (outbound, to_task) = mpsc::unbounded_channel();
        app.insert_resource(self.config.clone())
            .insert_resource(CoordinatorLink {
                inbound,
                outbound,
                status: if self.config.address.is_some() {
                    LinkStatus::Connecting
                } else {
                    LinkStatus::Disabled
                },
                drain: None,
            })
            .insert_resource(TaskEnds {
                updates: to_ecs,
                requests: Some(to_task),
            })
            .init_resource::<ActiveInterest>()
            .init_resource::<LocalNodeId>()
            .add_systems(
                Update,
                (
                    start_coordinator_session,
                    pump_coordinator,
                    // In the frame the order lands, not the one after: the
                    // deadline D24 §(d) stamps is one lease TTL away, and a
                    // drain that waited a frame per hop would be spending that
                    // budget on scheduling rather than on the round trip it
                    // exists to pay for.
                    apply_island_drain,
                    // After the drain, so the frame a peer leaves its island is
                    // not also a frame it dials the island it just left.
                    dial_island_peers,
                )
                    .chain(),
            );
    }
}

/// The session task's channel ends, parked until the endpoint is open.
///
/// The receiver is moved out on the first spawn; the sender is cloned, so a
/// coordinator-less app still has somewhere for `pump_coordinator` to read from.
#[derive(Resource)]
struct TaskEnds {
    updates: mpsc::UnboundedSender<CoordinatorUpdate>,
    requests: Option<mpsc::UnboundedReceiver<CoordinatorRequest>>,
}

/// This peer's own NodeId, learned once the endpoint is open.
///
/// Needed to verify a grant (which names its subject) and to break dial ties.
#[derive(Debug, Default, Resource)]
pub struct LocalNodeId(pub Option<NodeId>);

/// Marks that the session task has been spawned, so it is spawned once.
#[derive(Resource)]
struct SessionStarted;

/// Spawns the coordinator session once the endpoint exists.
///
/// Deferred rather than done at Startup because the session runs over the game
/// endpoint, and `IrohEndpoint::open` completes asynchronously.
fn start_coordinator_session(
    mut commands: Commands,
    started: Option<Res<SessionStarted>>,
    config: Res<CoordinatorConfig>,
    runtime: Res<TokioRuntime>,
    mut ends: ResMut<TaskEnds>,
    mut local: ResMut<LocalNodeId>,
    endpoints: Query<&IrohEndpoint>,
) {
    if started.is_some() {
        return;
    }
    let Ok(endpoint) = endpoints.single() else {
        return;
    };
    local.0 = Some(endpoint.id());
    commands.insert_resource(SessionStarted);

    let Some(address) = config.address.clone() else {
        return;
    };
    let Some(inbox) = ends.requests.take() else {
        return;
    };

    let raw = endpoint.raw().clone();
    let updates = ends.updates.clone();
    let token = config.token.clone();
    let handshake = config.handshake_timeout;
    let backoff = config.reconnect_delay;
    runtime.spawn_on_self(async move {
        run_session(raw, address, token, handshake, backoff, updates, inbox).await;
    });
}

/// Dial, serve, redial. Runs until the app drops the inbound channel.
async fn run_session(
    endpoint: iroh::Endpoint,
    address: iroh::EndpointAddr,
    token: Vec<u8>,
    handshake: Duration,
    backoff: Duration,
    updates: mpsc::UnboundedSender<CoordinatorUpdate>,
    mut requests: mpsc::UnboundedReceiver<CoordinatorRequest>,
) {
    // Survives reconnects: presence is current state, not an event, so a
    // coordinator that comes back to a silent peer would otherwise hold a stale
    // interest set until the peer next moved.
    let mut presence: Option<Vec<CellId>> = None;

    loop {
        match one_session(
            &endpoint,
            &address,
            &token,
            handshake,
            &updates,
            &mut requests,
            &mut presence,
        )
        .await
        {
            Ok(()) => return,
            Err(reason) => {
                if updates
                    .send(CoordinatorUpdate::Disconnected { reason })
                    .is_err()
                {
                    return;
                }
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// One connect-through-teardown cycle. `Ok(())` means the app went away.
async fn one_session(
    endpoint: &iroh::Endpoint,
    address: &iroh::EndpointAddr,
    token: &[u8],
    handshake: Duration,
    updates: &mpsc::UnboundedSender<CoordinatorUpdate>,
    requests: &mut mpsc::UnboundedReceiver<CoordinatorRequest>,
    presence: &mut Option<Vec<CellId>>,
) -> Result<(), String> {
    let expected = address.id;
    let connection = endpoint
        .connect(address.clone(), COORD_ALPN)
        .await
        .map_err(|error| format!("connect: {error}"))?;

    // The coordinator streams `[ACCEPTED]` before any datagrams flow; reading it
    // first is what keeps the hello from racing admission.
    let mut admission = tokio::time::timeout(handshake, connection.accept_uni())
        .await
        .map_err(|_| "admission stream did not arrive".to_owned())?
        .map_err(|error| format!("admission stream: {error}"))?;
    let accepted = admission
        .read_to_end(16)
        .await
        .map_err(|error| format!("admission read: {error}"))?;
    if accepted != vec![0u8] {
        return Err("admission refused".to_owned());
    }

    let node = endpoint.id();
    send(
        &connection,
        &CoordMsg::Hello {
            token: token.to_vec(),
            node,
        },
    )?;

    let welcome = tokio::time::timeout(handshake, recv(&connection))
        .await
        .map_err(|_| "welcome did not arrive".to_owned())?;
    match welcome {
        Some(CoordMsg::Welcome { coordinator, .. }) => {
            if coordinator != expected {
                return Err("welcome named a different coordinator".to_owned());
            }
            updates
                .send(CoordinatorUpdate::Connected { coordinator })
                .map_err(|_| String::new())?;
        }
        Some(other) => return Err(format!("expected a welcome, got {other:?}")),
        None => return Err("session closed before the welcome".to_owned()),
    }

    // Replay the last presence so a reconnected coordinator is not left holding
    // whatever this peer reported before the drop.
    if let Some(cells) = presence.clone() {
        send(&connection, &CoordMsg::Presence { cells })?;
    }

    loop {
        tokio::select! {
            request = requests.recv() => match request {
                Some(CoordinatorRequest::Presence(cells)) => {
                    *presence = Some(cells.clone());
                    send(&connection, &CoordMsg::Presence { cells })?;
                }
                // The app dropped its sender: shut the task down for good.
                None => return Ok(()),
            },
            message = recv(&connection) => match message {
                Some(CoordMsg::IslandAssignment { manifest }) => {
                    updates.send(CoordinatorUpdate::Manifest(manifest)).map_err(|_| String::new())?;
                }
                Some(CoordMsg::InterestGrant { grant }) => {
                    updates.send(CoordinatorUpdate::Grant(grant)).map_err(|_| String::new())?;
                }
                Some(CoordMsg::Drain { island, deadline }) => {
                    updates.send(CoordinatorUpdate::Drain { island, deadline }).map_err(|_| String::new())?;
                }
                // Anything else is either ours to send or a version we do not
                // know; ignoring is what lets the coordinator add messages.
                Some(_) => continue,
                None => return Err("session closed".to_owned()),
            },
        }
    }
}

fn send(connection: &iroh::endpoint::Connection, message: &CoordMsg) -> Result<(), String> {
    connection
        .send_datagram(Bytes::from(encode_stream_frame(message)))
        .map_err(|error| format!("send: {error}"))
}

/// The next coordinator message, or `None` once the connection is gone.
async fn recv(connection: &iroh::endpoint::Connection) -> Option<CoordMsg> {
    loop {
        let packet = connection.read_datagram().await.ok()?;
        let Some((Channel::Control, _)) = untag(&packet) else {
            continue;
        };
        if let Some(message) = decode_stream_frame(&packet) {
            return Some(message);
        }
    }
}

/// Drains the session task's updates into ECS state.
pub fn pump_coordinator(
    mut link: ResMut<CoordinatorLink>,
    mut membership: ResMut<IslandMembership>,
    mut interest: ResMut<ActiveInterest>,
    mut events: MessageWriter<NetEvent>,
    config: Res<CoordinatorConfig>,
    local: Res<LocalNodeId>,
) {
    while let Ok(update) = link.inbound.try_recv() {
        match update {
            CoordinatorUpdate::Connected { coordinator } => {
                link.status = LinkStatus::Connected { coordinator };
            }
            CoordinatorUpdate::Manifest(manifest) => {
                let Some(local) = local.0 else {
                    tracing::warn!("a manifest arrived before the endpoint was open");
                    continue;
                };
                match membership.apply_manifest(&manifest, local) {
                    Ok(caused) => {
                        for event in caused {
                            events.write(event);
                        }
                    }
                    Err(stale) => tracing::debug!(%stale, "dropped a stale island manifest"),
                }
            }
            CoordinatorUpdate::Grant(grant) => {
                let Some(local) = local.0 else {
                    tracing::warn!("an interest grant arrived before the endpoint was open");
                    continue;
                };
                if let Err(error) = interest.accept(grant, local, &config.issuer_keys) {
                    tracing::warn!(?error, "refused an interest grant");
                }
            }
            CoordinatorUpdate::Drain { island, deadline } => {
                link.drain = Some((island, deadline));
            }
            CoordinatorUpdate::Disconnected { reason } => {
                // Membership deliberately survives: see the module docs.
                tracing::info!(%reason, "coordinator session dropped; redialling");
                link.status = LinkStatus::Disconnected { reason };
            }
        }
    }
}

/// Tear the island down when the coordinator has ordered it drained (D24).
///
/// The second half of the drain. The first half — divesting the leases this
/// peer holds in the island, as `Divest { to: None }`, so the registrar parks
/// the rows — cannot live here: `orrery_authority` is the lower layer of the
/// two and may not be reached for from the transport crate, so that step is a
/// facade system (`orrery::divest_on_drain`) ordered *before* this one. This
/// system does the part that is membership's own: emit the departure events and
/// forget the island.
///
/// # Why this is allowed to be a no-op
///
/// A drain order is advisory (D24 §(b)). The coordinator sends it only for an
/// island whose population has already reached zero, to the peer whose
/// departure emptied it, and only if that peer's session happens to still be
/// open. On the departure mode a drain most needs to survive — a crash — there
/// is no session, no recipient, and no notice; the registrar's expiry sweep
/// parks every row regardless within `TTL + S = 11 s`
/// (`orrery_persistd::lease::LEASE_TTL_MS` plus the gateway's 1 s sweep). So
/// this system is a latency optimisation over a backstop that is already
/// correct, and it is deliberately not the thing that makes the drain happen.
///
/// # A drain naming another island is dropped, not deferred
///
/// The order is cleared either way. An order for an island this peer is not in
/// is either a duplicate of one already honoured or a straggler from a
/// membership the peer has since left, and in both cases there is nothing left
/// to drain. Keeping it would arm a tear-down against whatever island the peer
/// joins next, on the strength of a datagram about an island that no longer
/// exists — and `CoordMsg` rides unreliable datagrams, so duplicates and
/// reordering are the expected case rather than the anomalous one.
///
/// The clear is also what stops a redial from re-draining: the session task
/// keeps [`IslandMembership`] across a drop by design (see the module docs), so
/// an order left standing would fire again against the membership a reconnect
/// restored.
pub fn apply_island_drain(
    mut link: ResMut<CoordinatorLink>,
    mut membership: ResMut<IslandMembership>,
    mut events: MessageWriter<NetEvent>,
) {
    // Read through the immutable deref first: `ResMut` change detection fires
    // on any `DerefMut`, and taking the option unconditionally would mark the
    // link changed on every frame of a session that has no drain at all.
    let Some((island, deadline)) = link.drain else {
        return;
    };
    link.drain = None;

    if membership.island != Some(island) {
        tracing::debug!(
            island = island.0,
            held = ?membership.island.map(|held| held.0),
            "dropped a drain order for an island this peer is not in"
        );
        return;
    }

    tracing::info!(
        island = island.0,
        deadline,
        peers = membership.peer_count(),
        "draining the island on the coordinator's order"
    );
    for event in membership.leave() {
        events.write(event);
    }
}

/// Which island peers to open a session to right now.
///
/// Separated from the system so the rule can be tested without an endpoint —
/// the decision is all of the logic, and dialling is the only part that needs a
/// network.
///
/// # Who dials
///
/// Only the numerically lower NodeId dials. Both peers hold the same manifest,
/// so without a tiebreak both would dial and the island would carry two sessions
/// per pair — each side tracking a different one, and a disconnect on either
/// looking like a departure. The rule is arbitrary; it only has to be *shared*,
/// and every peer can evaluate it from the manifest alone.
///
/// `dialing` is pruned in place: a peer that answered, or that has left the
/// manifest, is no longer outstanding.
#[must_use]
pub fn peers_to_dial(
    local: NodeId,
    membership: &IslandMembership,
    connected: &[NodeId],
    dialing: &mut Vec<NodeId>,
) -> Vec<NodeId> {
    dialing.retain(|node| membership.contains(*node) && !connected.contains(node));

    let mut open = Vec::new();
    for node in membership.peer_ids() {
        if local >= node || connected.contains(&node) || dialing.contains(&node) {
            continue;
        }
        dialing.push(node);
        open.push(node);
    }
    open
}

/// Opens sessions to the island peers this peer is not yet connected to.
///
/// # Reaching a peer by NodeId alone
///
/// A manifest names identities, not addresses, so the dial goes out as a bare
/// [`NodeId`] and iroh's discovery resolves it. That works under the `N0`
/// preset [`crate::plugin::NetConfig`] builds on. An endpoint configured with
/// neither discovery nor a relay can authenticate to a coordinator and be given
/// an island it cannot reach — which is why the loopback tests assert on
/// membership rather than on sessions.
pub fn dial_island_peers(
    mut commands: Commands,
    membership: Res<IslandMembership>,
    registry: Res<PeerRegistry>,
    local: Res<LocalNodeId>,
    endpoints: Query<&IrohEndpoint>,
    mut dialing: Local<Vec<NodeId>>,
) {
    let (Some(local), Ok(endpoint)) = (local.0, endpoints.single()) else {
        return;
    };
    let connected: Vec<NodeId> = registry.peers.iter().map(|(_, peer)| peer.id).collect();
    for node in peers_to_dial(local, &membership, &connected, &mut dialing) {
        commands.spawn_empty().queue(endpoint.connect(node, ALPN));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::coord::{InterestGrantV1, MAX_INTEREST_GRANT_CELLS};
    use orrery_protocol::{Epoch, GridId, IssuerKeyId};

    fn key(seed: u8) -> iroh::SecretKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh::SecretKey::from_bytes(&bytes)
    }

    const KEY_ID: IssuerKeyId = IssuerKeyId(7);

    fn issuer() -> (iroh::SecretKey, Vec<IssuerKey>) {
        let secret = key(200);
        let keys = vec![IssuerKey::new(KEY_ID, secret.public())];
        (secret, keys)
    }

    fn grant_for(issuer: &iroh::SecretKey, peer: NodeId, epoch: u64, cells: usize) -> Vec<u8> {
        let claims = InterestGrantClaimsV1::new(
            peer,
            Epoch::new(epoch),
            GridId::ROOT,
            (0..cells).map(|_| CellId::ROOT).collect(),
            60_000,
            KEY_ID,
        );
        InterestGrantV1::sign(claims, issuer)
            .expect("sign")
            .encode()
            .expect("encode")
    }

    #[test]
    fn a_grant_for_this_peer_is_accepted_and_kept_for_forwarding() {
        let (secret, keys) = issuer();
        let me = key(1).public();
        let encoded = grant_for(&secret, me, 1, 1);
        let mut interest = ActiveInterest::default();
        interest.accept(encoded.clone(), me, &keys).expect("accept");
        assert_eq!(
            interest.grant, encoded,
            "the opaque bytes are what a gateway wants"
        );
        assert_eq!(interest.claims.expect("claims").epoch, Epoch::new(1));
    }

    #[test]
    fn a_grant_issued_to_somebody_else_is_refused() {
        // `verify_interest_grant` binds the presenter, so a coordinator that
        // misrouted a grant — or a relayed one — never becomes this peer's
        // interest. Interest gates authority claims, so accepting one would let
        // a peer claim entities it has no coverage for.
        let (secret, keys) = issuer();
        let me = key(1).public();
        let someone_else = key(2).public();
        let mut interest = ActiveInterest::default();
        let error = interest
            .accept(grant_for(&secret, someone_else, 1, 1), me, &keys)
            .expect_err("a grant naming another peer must not be adopted");
        assert_eq!(error, InterestGrantVerificationError::WrongPeer);
        assert!(interest.grant.is_empty());
        assert_eq!(interest.rejected, 1);
    }

    #[test]
    fn a_grant_from_an_untrusted_issuer_is_refused() {
        let (_, keys) = issuer();
        let impostor = key(201);
        let me = key(1).public();
        let mut interest = ActiveInterest::default();
        assert!(interest
            .accept(grant_for(&impostor, me, 1, 1), me, &keys)
            .is_err());
        assert!(interest.claims.is_none());
    }

    #[test]
    fn a_replayed_older_grant_does_not_replace_a_newer_one() {
        // Both grants are genuine. The gateway keeps the highest epoch per peer,
        // so replaying the old one could not widen coverage there — but a peer
        // that overwrote its own state would go on forwarding the narrower stale
        // grant and lose coverage it still holds.
        let (secret, keys) = issuer();
        let me = key(1).public();
        let wide = grant_for(&secret, me, 1, 4);
        let narrow = grant_for(&secret, me, 2, 1);
        let mut interest = ActiveInterest::default();
        interest.accept(wide.clone(), me, &keys).unwrap();
        interest.accept(narrow.clone(), me, &keys).unwrap();
        interest.accept(wide, me, &keys).unwrap();
        assert_eq!(interest.grant, narrow, "the newest grant is still held");
        assert_eq!(interest.claims.expect("claims").epoch, Epoch::new(2));
        assert_eq!(interest.stale, 1);
    }

    #[test]
    fn without_issuer_keys_a_grant_is_forwarded_but_not_read() {
        // The gateway is the party that must verify, and it always does. What a
        // keyless peer loses is its own view of its covered cells — not the
        // gateway's safety — so refusing outright would break a peer that is
        // merely under-configured.
        let (secret, _) = issuer();
        let me = key(1).public();
        let encoded = grant_for(&secret, me, 1, 1);
        let mut interest = ActiveInterest::default();
        interest
            .accept(encoded.clone(), me, &[])
            .expect("forwarded");
        assert_eq!(interest.grant, encoded);
        assert!(interest.claims.is_none());
        assert_eq!(interest.unverified, 1);
    }

    /// Membership holding exactly `nodes`, as if a manifest had named them.
    fn island_of(nodes: &[NodeId]) -> IslandMembership {
        IslandMembership {
            island: Some(IslandId::new(1)),
            peers: nodes
                .iter()
                .map(|node| orrery_protocol::coord::PeerEntry {
                    node: *node,
                    cells: Vec::new(),
                })
                .collect(),
            ..IslandMembership::default()
        }
    }

    /// Two ids, lower first.
    fn ordered_pair() -> (NodeId, NodeId) {
        let (a, b) = (key(10).public(), key(11).public());
        if a < b {
            (a, b)
        } else {
            (b, a)
        }
    }

    #[test]
    fn only_the_lower_node_id_dials() {
        // Both peers hold the same manifest. Without a tiebreak both dial, and
        // the pair ends up with two sessions — each side tracking a different
        // one, so a drop on either reads as a departure.
        let (low, high) = ordered_pair();
        let mut outstanding = Vec::new();
        assert_eq!(
            peers_to_dial(low, &island_of(&[high]), &[], &mut outstanding),
            vec![high],
            "the lower id opens the session"
        );
        let mut outstanding = Vec::new();
        assert!(
            peers_to_dial(high, &island_of(&[low]), &[], &mut outstanding).is_empty(),
            "the higher id waits to be dialled"
        );
    }

    #[test]
    fn a_peer_never_dials_itself() {
        // Defence in depth: `apply_manifest` already filters the local peer out,
        // but the tiebreak has to hold on its own — `local >= node` is only true
        // for self because the comparison is not strict.
        let me = key(10).public();
        let mut outstanding = Vec::new();
        assert!(peers_to_dial(me, &island_of(&[me]), &[], &mut outstanding).is_empty());
    }

    #[test]
    fn a_dial_in_flight_is_not_reissued_every_frame() {
        // This system runs each Update. Re-dialling a peer that has not answered
        // yet would spawn a session entity per frame until it did.
        let (low, high) = ordered_pair();
        let island = island_of(&[high]);
        let mut outstanding = Vec::new();
        assert_eq!(
            peers_to_dial(low, &island, &[], &mut outstanding),
            vec![high]
        );
        assert!(peers_to_dial(low, &island, &[], &mut outstanding).is_empty());
        assert!(peers_to_dial(low, &island, &[], &mut outstanding).is_empty());
    }

    #[test]
    fn a_peer_that_answered_is_not_dialled_again() {
        let (low, high) = ordered_pair();
        let island = island_of(&[high]);
        let mut outstanding = Vec::new();
        let _ = peers_to_dial(low, &island, &[], &mut outstanding);
        assert!(peers_to_dial(low, &island, &[high], &mut outstanding).is_empty());
        assert!(
            outstanding.is_empty(),
            "a connected peer is no longer outstanding"
        );
    }

    #[test]
    fn a_peer_that_left_the_island_is_redialled_if_it_returns() {
        // The outstanding set must be pruned when a peer leaves the manifest,
        // or a peer that leaves mid-dial and comes back is never dialled again.
        let (low, high) = ordered_pair();
        let mut outstanding = Vec::new();
        let _ = peers_to_dial(low, &island_of(&[high]), &[], &mut outstanding);
        assert!(peers_to_dial(low, &island_of(&[]), &[], &mut outstanding).is_empty());
        assert!(outstanding.is_empty(), "dropped from the manifest");
        assert_eq!(
            peers_to_dial(low, &island_of(&[high]), &[], &mut outstanding),
            vec![high],
            "and dialled again when it comes back"
        );
    }

    /// An island member, named the way this module's fixtures name peers.
    fn member(n: u8) -> NodeId {
        key(n).public()
    }

    /// A world holding `membership`, a link carrying `drain`, and somewhere for
    /// [`NetEvent`]s to land.
    fn drain_world(membership: IslandMembership, drain: Option<(IslandId, u64)>) -> World {
        let (_to_ecs, inbound) = mpsc::unbounded_channel();
        let (outbound, _to_task) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(CoordinatorLink {
            inbound,
            outbound,
            status: LinkStatus::Disabled,
            drain,
        });
        world.insert_resource(membership);
        world.init_resource::<Messages<NetEvent>>();
        world
    }

    /// Run [`apply_island_drain`] once and collect what it announced.
    fn run_drain(world: &mut World) -> Vec<NetEvent> {
        bevy_ecs::system::RunSystemOnce::run_system_once(&mut *world, apply_island_drain)
            .expect("the drain system runs");
        world
            .resource::<Messages<NetEvent>>()
            .iter_current_update_messages()
            .cloned()
            .collect()
    }

    #[test]
    fn a_drain_for_the_held_island_announces_every_departure_and_leaves() {
        // The order this peer is actually the recipient of (D24 §(b)): the
        // island it is in, emptied by its own departure. Honouring it has to
        // produce exactly what any other way of leaving produces, or downstream
        // systems that tear down per peer would be left holding sessions and
        // proxies for an island that is gone.
        let mut world = drain_world(
            island_of(&[member(1), member(2)]),
            Some((IslandId::new(1), 99)),
        );

        let events = run_drain(&mut world);

        assert!(events.contains(&NetEvent::PeerLeft { node: member(1) }));
        assert!(events.contains(&NetEvent::PeerLeft { node: member(2) }));
        assert_eq!(
            events.last(),
            Some(&NetEvent::IslandChanged {
                island: None,
                epoch: 0,
                regime: orrery_protocol::coord::TopologyRegime::Mesh,
            }),
            "the island change is last, after every departure: {events:?}"
        );
        let membership = world.resource::<IslandMembership>();
        assert!(!membership.is_member());
        assert!(membership.peers.is_empty());
    }

    #[test]
    fn a_drain_naming_another_island_is_dropped() {
        // `CoordMsg` rides unreliable datagrams, so a duplicate or a straggler
        // from a membership this peer has since left is the expected case. It
        // must not tear down the island the peer is in now — and it must not be
        // kept, or it would arm a tear-down against whatever island comes next.
        let mut world = drain_world(island_of(&[member(1)]), Some((IslandId::new(2), 99)));

        let events = run_drain(&mut world);

        assert!(events.is_empty(), "nothing was announced: {events:?}");
        let membership = world.resource::<IslandMembership>();
        assert_eq!(membership.island, Some(IslandId::new(1)));
        assert_eq!(membership.peer_count(), 1);
        assert!(world.resource::<CoordinatorLink>().drain.is_none());
    }

    #[test]
    fn a_drain_fires_once_and_does_not_survive_a_redial() {
        // Membership deliberately outlives a dropped session (see the module
        // docs), so an order left standing would fire a second time against the
        // membership a reconnect restored — a peer torn out of an island it had
        // legitimately rejoined, on the strength of an order about an island the
        // coordinator has already forgotten.
        let mut world = drain_world(island_of(&[member(1)]), Some((IslandId::new(1), 99)));
        assert!(!run_drain(&mut world).is_empty(), "the first drain acts");
        assert!(world.resource::<CoordinatorLink>().drain.is_none());

        // The redial: a fresh manifest puts the peer back in the same island.
        world.insert_resource(island_of(&[member(1)]));
        world.resource_mut::<Messages<NetEvent>>().clear();

        let events = run_drain(&mut world);
        assert!(events.is_empty(), "the order was consumed: {events:?}");
        assert!(world.resource::<IslandMembership>().is_member());
    }

    #[test]
    fn a_drain_that_never_arrives_changes_nothing_here() {
        // The advisory property, at the only layer that can hold it (D24 §(b)):
        // this system is a *latency* optimisation over the registrar's expiry
        // sweep, which parks every row within `TTL + S = 11 s` with no message
        // at all. A peer that crashed has no session to be told on, and the
        // undelivered case must therefore be indistinguishable, here, from no
        // order at all — no partial tear-down, no half-left island, nothing that
        // a later system could read as "a drain is in progress".
        //
        // What this cannot assert is the other half of the property, because the
        // other half is not in this process: that the island still drains. That
        // is the registrar's sweep (`orrery_persistd::gateway`'s 1 s
        // `sweep_expired_leases`), and it is proved where it lives.
        let mut world = drain_world(island_of(&[member(1), member(2)]), None);

        let events = run_drain(&mut world);

        assert!(events.is_empty(), "nothing was announced: {events:?}");
        let membership = world.resource::<IslandMembership>();
        assert_eq!(membership.island, Some(IslandId::new(1)));
        assert_eq!(membership.peer_count(), 2);
    }

    #[test]
    fn an_oversized_grant_is_refused() {
        let (secret, keys) = issuer();
        let me = key(1).public();
        let mut interest = ActiveInterest::default();
        assert!(interest
            .accept(
                grant_for(&secret, me, 1, MAX_INTEREST_GRANT_CELLS + 1),
                me,
                &keys
            )
            .is_err());
    }
}
