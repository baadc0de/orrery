//! The coordinator service: presence in, islands and interest out (D12).
//!
//! This is the process that was missing. [`IslandRegistry`] knew how to turn
//! presence into islands and [`InterestIssuer`](crate::InterestIssuer) knew how
//! to sign a peer's coverage, but nothing drove either, so island manifests had
//! no source and interest grants had to be minted by hand in harnesses. A peer
//! now connects here, authenticates, reports what it covers, and receives two
//! things back:
//!
//! - an [`IslandManifest`] naming its island, the cells it spans, its regime
//!   and its peers — pushed to *every* peer in an affected island, not just
//!   the reporter, because a merge changes everybody's membership;
//! - a signed interest grant it forwards to whichever gateway it talks to,
//!   which is what authorizes its weak claims and makes it eligible to inherit
//!   a lease (D7 §5).
//!
//! **Bevy-free, iroh-native** (docs/10-crates.md §6): the coordinator is a
//! service, not a game process. It speaks the same shape the persistd gateway
//! does — an admission uni-stream, then tagged datagrams — so a peer needs one
//! client pattern for both.
//!
//! Presence is authenticated for the same reason interest is signed. Presence
//! decides island membership *and* what a peer may claim, so an unauthenticated
//! peer reporting presence would be granting itself authority by another route.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use orrery_protocol::channels::{decode_stream_frame, encode_stream_frame, untag, Channel};
use orrery_protocol::{
    CellId, CoordMsg, GridId, IssuerKey, NodeId, SessionTokenVerifier, TokenClock, UnixMillis,
    COORD_ALPN, COORD_PROTOCOL_VERSION, MAX_PRESENCE_CELLS,
};

use crate::interest::InterestIssuer;
use crate::registry::IslandRegistry;

/// The admission response byte, mirroring the gateway's `ACCEPTED`.
const ACCEPTED: u8 = 0;

/// Maximum peers a coordinator tracks at once.
const MAX_TRACKED_PEERS: usize = 4_096;

/// Presence reports allowed per peer per second, and the burst above it.
///
/// Presence is coarse and rate-limited by design (docs/02-networking.md §3):
/// each report costs an island re-evaluation and a fresh signature, so an
/// unmetered one is a cheap way to make a coordinator expensive.
const PRESENCE_PER_SECOND: u64 = 4;
const PRESENCE_BURST: u64 = 16;

/// A source of monotonic milliseconds for presence rate limiting.
pub trait PresenceClock: Send + Sync {
    /// Milliseconds from an arbitrary but monotonic origin.
    fn now_ms(&self) -> u64;
}

/// The default process-monotonic presence clock.
#[derive(Debug)]
pub struct SystemPresenceClock {
    started: std::time::Instant,
}

impl Default for SystemPresenceClock {
    fn default() -> Self {
        Self {
            started: std::time::Instant::now(),
        }
    }
}

impl PresenceClock for SystemPresenceClock {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// A fixed Unix clock for token verification in tests and simple embeddings.
#[derive(Debug, Clone, Copy)]
pub struct FixedUnixClock(pub u64);

impl TokenClock for FixedUnixClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(self.0)
    }
}

/// The system Unix clock used to verify session tokens.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemUnixClock;

impl TokenClock for SystemUnixClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis() as u64)
                .unwrap_or(0),
        )
    }
}

/// Startup configuration for a [`CoordinatorServer`].
pub struct ServerConfig {
    /// The application protocol to advertise. Defaults to [`COORD_ALPN`].
    pub alpn: Vec<u8>,
    /// The iroh relay mode. `RelayMode::Disabled` for loopback tests.
    pub relay_mode: RelayMode,
    /// An optional secret key pinning the coordinator's NodeId across runs.
    pub secret_key: Option<iroh::SecretKey>,
    /// The local address to bind; port `0` asks the OS for an ephemeral one.
    pub bind: SocketAddr,
    /// Identity issuer keys whose session tokens this coordinator accepts.
    pub issuer_keys: Vec<IssuerKey>,
    /// The signing key for the interest grants this coordinator hands out.
    pub interest_issuer: InterestIssuer,
    /// The grid presence and grants are relative to (P-7: cell ids are
    /// grid-relative, so a coordinator serves one grid's cell space).
    pub grid: GridId,
    /// Unix clock used to verify session-token expiry.
    pub token_clock: Arc<dyn TokenClock + Send + Sync>,
    /// Monotonic clock used for presence rate limiting.
    pub presence_clock: Arc<dyn PresenceClock>,
}

impl ServerConfig {
    /// A configuration accepting `issuer_keys` and signing with `issuer`.
    #[must_use]
    pub fn new(issuer_keys: impl IntoIterator<Item = IssuerKey>, issuer: InterestIssuer) -> Self {
        Self {
            alpn: COORD_ALPN.to_vec(),
            relay_mode: RelayMode::Disabled,
            secret_key: None,
            bind: "127.0.0.1:0".parse().expect("static valid loopback addr"),
            issuer_keys: issuer_keys.into_iter().collect(),
            interest_issuer: issuer,
            grid: GridId::ROOT,
            token_clock: Arc::new(SystemUnixClock),
            presence_clock: Arc::new(SystemPresenceClock::default()),
        }
    }
}

/// Failures binding or running a [`CoordinatorServer`].
#[derive(Debug)]
pub enum ServerError {
    /// The iroh endpoint could not be bound.
    Bind(iroh::endpoint::BindError),
    /// The bind address was rejected.
    BindAddr(String),
}

impl core::fmt::Display for ServerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(error) => write!(f, "bind coordinator endpoint: {error}"),
            Self::BindAddr(error) => write!(f, "coordinator bind address: {error}"),
        }
    }
}

impl core::error::Error for ServerError {}

/// A refill-over-time budget for one peer's presence reports.
#[derive(Debug)]
struct PresenceBudget {
    token_millis: u64,
    updated_ms: u64,
}

impl PresenceBudget {
    const MILLIS_PER_REPORT: u64 = 1_000;
    const BURST_MILLIS: u64 = PRESENCE_BURST * Self::MILLIS_PER_REPORT;

    fn new(now_ms: u64) -> Self {
        Self {
            token_millis: Self::BURST_MILLIS,
            updated_ms: now_ms,
        }
    }

    fn take(&mut self, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.updated_ms);
        self.updated_ms = now_ms;
        self.token_millis = self
            .token_millis
            .saturating_add(elapsed.saturating_mul(PRESENCE_PER_SECOND))
            .min(Self::BURST_MILLIS);
        if self.token_millis >= Self::MILLIS_PER_REPORT {
            self.token_millis -= Self::MILLIS_PER_REPORT;
            true
        } else {
            false
        }
    }
}

/// One connected peer: how to reach it, and its presence budget.
struct PeerSession {
    notify: Arc<dyn Fn(Bytes) + Send + Sync>,
    budget: PresenceBudget,
}

/// The coordinator's shared state, owned by the accept loop and its sessions.
struct Shared {
    registry: tokio::sync::Mutex<IslandRegistry>,
    peers: tokio::sync::Mutex<HashMap<NodeId, PeerSession>>,
    issuer: InterestIssuer,
    grid: GridId,
    presence_clock: Arc<dyn PresenceClock>,
    presence_reports: AtomicU64,
    grants_issued: AtomicU64,
    manifests_pushed: AtomicU64,
}

impl Shared {
    /// Push a message to one peer, if it still has a live session.
    async fn notify(&self, node: NodeId, message: &CoordMsg) -> bool {
        let frame = Bytes::from(encode_stream_frame(message));
        let peers = self.peers.lock().await;
        match peers.get(&node) {
            Some(session) => {
                (session.notify)(frame);
                true
            }
            None => false,
        }
    }

    /// Hand `node` a freshly signed grant for its current coverage.
    async fn issue_interest(&self, node: NodeId) {
        let grant = {
            let registry = self.registry.lock().await;
            self.issuer.issue(&registry, node, self.grid)
        };
        let Some(Ok(grant)) = grant else {
            // No presence on file yet, or an encoding failure. Neither is
            // worth pushing an empty grant for: a verifier refuses one.
            return;
        };
        if self.notify(node, &CoordMsg::InterestGrant { grant }).await {
            self.grants_issued.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Push a manifest to every peer it names.
    ///
    /// A merge or split changes membership for peers that never reported
    /// anything, so telling only the reporter would leave the rest of the
    /// island believing an obsolete roster.
    async fn broadcast(&self, manifests: Vec<orrery_protocol::IslandManifest>) {
        for manifest in manifests {
            for entry in manifest.peers.clone() {
                if self
                    .notify(
                        entry.node,
                        &CoordMsg::IslandAssignment {
                            manifest: manifest.clone(),
                        },
                    )
                    .await
                {
                    self.manifests_pushed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// A point-in-time read of a coordinator's activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoordinatorStats {
    /// Presence reports accepted.
    pub presence_reports: u64,
    /// Interest grants signed and delivered.
    pub grants_issued: u64,
    /// Island manifests delivered.
    pub manifests_pushed: u64,
    /// Peers with a live session.
    pub connected_peers: usize,
    /// Islands currently formed.
    pub islands: usize,
}

/// A running coordinator: an iroh endpoint accepting peer sessions.
pub struct CoordinatorServer {
    endpoint: Arc<Endpoint>,
    shared: Arc<Shared>,
    shutdown: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

impl CoordinatorServer {
    /// Bind an endpoint from `config` and start accepting peers.
    pub async fn spawn(config: ServerConfig) -> Result<Self, ServerError> {
        let mut builder = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0);
        builder = builder
            .bind_addr(config.bind)
            .map_err(|error| ServerError::BindAddr(error.to_string()))?;
        builder = builder.alpns(vec![config.alpn.clone()]);
        builder = builder.relay_mode(config.relay_mode.clone());
        if let Some(key) = &config.secret_key {
            builder = builder.secret_key(key.clone());
        }
        let endpoint = Arc::new(builder.bind().await.map_err(ServerError::Bind)?);

        let shared = Arc::new(Shared {
            registry: tokio::sync::Mutex::new(IslandRegistry::new()),
            peers: tokio::sync::Mutex::new(HashMap::new()),
            issuer: config.interest_issuer,
            grid: config.grid,
            presence_clock: config.presence_clock,
            presence_reports: AtomicU64::new(0),
            grants_issued: AtomicU64::new(0),
            manifests_pushed: AtomicU64::new(0),
        });
        let verifier = Arc::new(SessionTokenVerifier::new(
            ClockBox(config.token_clock),
            config.issuer_keys,
        ));

        let (shutdown, rx) = oneshot::channel();
        let join = tokio::spawn(accept_loop(
            Arc::clone(&endpoint),
            Arc::clone(&shared),
            verifier,
            rx,
        ));
        Ok(Self {
            endpoint,
            shared,
            shutdown,
            join,
        })
    }

    /// The coordinator's node id — a peer dials this.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.endpoint.id()
    }

    /// The coordinator's full dial document.
    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Current activity counters.
    pub async fn stats(&self) -> CoordinatorStats {
        CoordinatorStats {
            presence_reports: self.shared.presence_reports.load(Ordering::Relaxed),
            grants_issued: self.shared.grants_issued.load(Ordering::Relaxed),
            manifests_pushed: self.shared.manifests_pushed.load(Ordering::Relaxed),
            connected_peers: self.shared.peers.lock().await.len(),
            islands: self.shared.registry.lock().await.island_count(),
        }
    }

    /// Stop accepting, close the endpoint, and await the accept task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.endpoint.close().await;
        let _ = self.join.await;
    }
}

/// Adapts a shared clock into the by-value `TokenClock` the verifier holds.
struct ClockBox(Arc<dyn TokenClock + Send + Sync>);

impl TokenClock for ClockBox {
    fn now_ms(&self) -> UnixMillis {
        self.0.now_ms()
    }
}

async fn accept_loop(
    endpoint: Arc<Endpoint>,
    shared: Arc<Shared>,
    verifier: Arc<SessionTokenVerifier<ClockBox>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let shared = Arc::clone(&shared);
                let verifier = Arc::clone(&verifier);
                let coordinator = endpoint.id();
                tokio::spawn(handle_connection(incoming, shared, verifier, coordinator));
            }
        }
    }
}

async fn handle_connection(
    incoming: iroh::endpoint::Incoming,
    shared: Arc<Shared>,
    verifier: Arc<SessionTokenVerifier<ClockBox>>,
    coordinator: NodeId,
) {
    let conn = match incoming.accept() {
        Ok(accepting) => match accepting.await {
            Ok(conn) => conn,
            Err(error) => {
                debug!(?error, "coordinator: handshake failed");
                return;
            }
        },
        Err(error) => {
            debug!(?error, "coordinator: accept failed");
            return;
        }
    };
    let conn = Arc::new(conn);
    let remote = conn.remote_id();

    // Admission first, mirroring the gateway: a peer knows it is through
    // before it sends anything.
    match conn.open_uni().await {
        Ok(mut stream) => {
            if stream.write_all(&[ACCEPTED]).await.is_err() || stream.finish().is_err() {
                debug!(%remote, "coordinator: admission stream failed");
                return;
            }
        }
        Err(error) => {
            debug!(?error, %remote, "coordinator: could not open admission stream");
            return;
        }
    }

    let send: Arc<dyn Fn(Bytes) + Send + Sync> = {
        let conn = Arc::clone(&conn);
        Arc::new(move |bytes: Bytes| {
            if let Err(error) = conn.send_datagram(bytes) {
                warn!(?error, %remote, "coordinator: push failed");
            }
        })
    };

    let mut admitted = false;
    loop {
        let packet = match conn.read_datagram().await {
            Ok(packet) => packet,
            Err(error) => {
                debug!(?error, %remote, "coordinator: connection closed");
                break;
            }
        };
        let Some((Channel::Control, _)) = untag(&packet) else {
            continue;
        };
        let Some(message) = decode_stream_frame::<CoordMsg>(&packet) else {
            debug!(%remote, "coordinator: undecodable message");
            continue;
        };

        match message {
            CoordMsg::Hello { token, node } => {
                // The iroh transport identity is the identity. A claimed wire
                // NodeId must never substitute it, or a peer could report
                // presence — and so mint interest — as somebody else.
                if node != remote {
                    warn!(%remote, "coordinator: hello node did not match transport identity");
                    break;
                }
                match verifier.verify(&token, &remote) {
                    Ok(_claims) => {}
                    Err(error) => {
                        debug!(%remote, ?error, "coordinator: rejected session token");
                        break;
                    }
                }
                let now_ms = shared.presence_clock.now_ms();
                {
                    let mut peers = shared.peers.lock().await;
                    if !peers.contains_key(&remote) && peers.len() >= MAX_TRACKED_PEERS {
                        warn!(%remote, "coordinator: peer capacity reached");
                        break;
                    }
                    peers.insert(
                        remote,
                        PeerSession {
                            notify: Arc::clone(&send),
                            budget: PresenceBudget::new(now_ms),
                        },
                    );
                }
                admitted = true;
                send(Bytes::from(encode_stream_frame(&CoordMsg::Welcome {
                    coordinator,
                    protocol: COORD_PROTOCOL_VERSION,
                })));
                // A reconnecting peer may already have presence on file; hand
                // it a fresh grant so it is not left unable to claim until it
                // next moves.
                shared.issue_interest(remote).await;
            }
            CoordMsg::Presence { cells } => {
                if !admitted {
                    continue;
                }
                if cells.is_empty() || cells.len() > MAX_PRESENCE_CELLS {
                    debug!(%remote, count = cells.len(), "coordinator: unusable presence");
                    continue;
                }
                let now_ms = shared.presence_clock.now_ms();
                {
                    let mut peers = shared.peers.lock().await;
                    let Some(session) = peers.get_mut(&remote) else {
                        break;
                    };
                    if !session.budget.take(now_ms) {
                        debug!(%remote, "coordinator: presence rate limited");
                        continue;
                    }
                }
                shared.presence_reports.fetch_add(1, Ordering::Relaxed);

                let manifests = {
                    let mut registry = shared.registry.lock().await;
                    registry.report_presence(remote, cells)
                };
                // Interest first: a peer that receives its manifest and starts
                // claiming should already hold the grant those claims need.
                shared.issue_interest(remote).await;
                shared.broadcast(manifests).await;
            }
            // Everything else is coordinator→peer. A peer sending one is
            // confused rather than hostile; ignore it.
            CoordMsg::Welcome { .. }
            | CoordMsg::IslandAssignment { .. }
            | CoordMsg::InterestGrant { .. }
            | CoordMsg::Drain { .. } => {}
        }
    }

    if admitted {
        // Forget the peer, then tell whoever is left. A departed peer must not
        // linger in a manifest, or survivors will try to reach a ghost.
        let manifests = {
            let mut registry = shared.registry.lock().await;
            registry.forget_peer(remote)
        };
        shared.peers.lock().await.remove(&remote);
        shared.broadcast(manifests).await;
    }
}

/// Presence cells for a peer whose interest is the 27-cell neighbourhood of
/// `centre` (D5), clamped to what one report may carry.
#[must_use]
pub fn presence_for(centre: CellId) -> Vec<CellId> {
    let mut cells = centre.neighbors27();
    cells.truncate(MAX_PRESENCE_CELLS);
    cells
}
