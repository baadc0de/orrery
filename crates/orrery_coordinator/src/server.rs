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
    CellId, CoordMsg, FixedTokenClock, GridId, IssuerKey, NodeId, SessionTokenClaimsV1,
    SessionTokenV1, SessionTokenVerificationError, SessionTokenVerifier, TokenClock, UnixMillis,
    COORD_ALPN, COORD_PROTOCOL_VERSION, MAX_PRESENCE_CELLS,
};

use crate::interest::InterestIssuer;
use crate::registry::{IslandDrain, IslandRegistry, MembershipChange};
use crate::witness::{SeedOutcome, WitnessEpochIssuer, WitnessSeedConfig, WitnessSeeder};

/// The admission response byte, mirroring the gateway's `ACCEPTED`.
const ACCEPTED: u8 = 0;

/// Maximum peers a coordinator tracks at once.
const MAX_TRACKED_PEERS: usize = 4_096;

/// How long an ended session stays "established", and how far past its expiry
/// a token may be graced (docs/09 §8).
///
/// One number for both halves, and one rationale: a session token's cap is an
/// hour (`MAX_SESSION_TOKEN_TTL_MS`) and clients refresh at half-TTL, so an
/// hour of grace is at most one missed refresh cycle turned into a second
/// token lifetime. Past that the outage has stopped being something to ride
/// out — identity is stateless replicas behind a well-known address
/// (docs/09 §1), so an hour down is an incident, not a blip, and a
/// coordinator quietly running on hour-old standing for a shift is worse than
/// making the peers log in again when identity returns.
///
/// The gateway pins no bound at all today (`gateway.rs`'s `Expired` arm
/// checks only its peer-registry retention), so this is deliberately the
/// stricter of the two. Bringing the gateway to the same bound is a follow-up
/// on its own, not something to smuggle in from here.
const TOKEN_GRACE_MS: u64 = orrery_protocol::MAX_SESSION_TOKEN_TTL_MS;

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

/// Reports whether the identity service is reachable (docs/09 §8).
///
/// The same shape as `orrery_persistd`'s `IdentityHealth`, and deliberately a
/// second copy of it rather than a trait lifted into `orrery_protocol`: the
/// two services answer the question from different places — a gateway from
/// its own identity client, a coordinator from whatever its deployment gives
/// it — and neither of them has an implementation to share yet. Factor it out
/// when there is a probe worth sharing, not before.
pub trait IdentityHealth: Send + Sync {
    /// Return `true` only while the identity service is known to be available.
    fn is_available(&self) -> bool;
}

/// Shared identity-service health source.
pub type SharedIdentityHealth = Arc<dyn IdentityHealth>;

/// Healthy-by-default production health until an outage monitor reports
/// otherwise. Grace is off under this one, which is the safe default: a
/// deployment that has not wired a probe has not earned the relaxation.
#[derive(Debug, Default)]
pub struct AvailableIdentityHealth;

impl IdentityHealth for AvailableIdentityHealth {
    fn is_available(&self) -> bool {
        true
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
    /// The witness-epoch issuer, when this coordinator seeds witness sets.
    ///
    /// `None` is P4's posture and the default: no announcements are made, and
    /// `orrery_witness` keeps its self-chosen fallback, which is only
    /// tolerable while nothing is filed against a report. Configuring an
    /// issuer is what turns D10 item 4 on for this deployment.
    pub witness_issuer: Option<WitnessEpochIssuer>,
    /// The epoch cadence and eligibility windows for witness seeding.
    pub witness_seed: WitnessSeedConfig,
    /// The grid presence and grants are relative to (P-7: cell ids are
    /// grid-relative, so a coordinator serves one grid's cell space).
    pub grid: GridId,
    /// Unix clock used to verify session-token expiry.
    pub token_clock: Arc<dyn TokenClock + Send + Sync>,
    /// Monotonic clock used for presence rate limiting.
    pub presence_clock: Arc<dyn PresenceClock>,
    /// Identity-service health, consulted only to decide token grace.
    pub identity_health: SharedIdentityHealth,
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
            witness_issuer: None,
            witness_seed: WitnessSeedConfig::default(),
            grid: GridId::ROOT,
            token_clock: Arc::new(SystemUnixClock),
            presence_clock: Arc::new(SystemPresenceClock::default()),
            identity_health: Arc::new(AvailableIdentityHealth),
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

/// How a `Hello`'s session token verified.
///
/// The distinction the coordinator did not use to draw: `Expired` is a token
/// this coordinator would have accepted a moment ago and whose signature,
/// issuer, node binding and TTL cap all still check out — everything except
/// the wall clock. Every other rejection stays a rejection.
#[derive(Debug, Clone)]
enum Admission {
    /// Valid at the coordinator's clock.
    Valid(SessionTokenClaimsV1),
    /// Authentic and node-bound, but its lifetime elapsed.
    Expired(SessionTokenClaimsV1),
}

/// Verifies `Hello` tokens, telling "late" apart from "invalid".
///
/// Mirrors `orrery_persistd`'s `SessionTokenV1Authorizer`: on `Expired`, the
/// same bytes are verified a second time against a clock pinned to the
/// token's own `issued_at_ms`, so an expired token that also has a forged
/// signature, a wrong node binding, an unknown issuer or an over-cap TTL
/// still fails — grace admits a *late* token, never a weaker check.
struct SessionAuthorizer {
    clock: Arc<dyn TokenClock + Send + Sync>,
    issuer_keys: Vec<IssuerKey>,
}

impl SessionAuthorizer {
    fn authorize(
        &self,
        token: &[u8],
        node: &NodeId,
    ) -> Result<Admission, SessionTokenVerificationError> {
        let verifier =
            SessionTokenVerifier::new(ClockBox(Arc::clone(&self.clock)), self.issuer_keys.clone());
        match verifier.verify(token, node) {
            Ok(claims) => Ok(Admission::Valid(claims)),
            Err(SessionTokenVerificationError::Expired) => {
                let issued_at = SessionTokenV1::decode(token)?.claims.issued_at_ms;
                SessionTokenVerifier::new(FixedTokenClock::new(issued_at), self.issuer_keys.clone())
                    .verify(token, node)
                    .map(Admission::Expired)
            }
            Err(error) => Err(error),
        }
    }
}

/// The identity one peer established a session with, kept past the session.
struct EstablishedPeer {
    claims: SessionTokenClaimsV1,
    token: Vec<u8>,
    /// When the peer's last session ended; `None` while one is live.
    idle_since_ms: Option<u64>,
}

/// What "a peer this coordinator already knows" means, in state.
///
/// docs/09 §8 grants grace to *established* sessions, and nothing here could
/// answer that before: `shared.peers`, the registry's presence and the
/// seeder's session facts are all torn down the moment a connection ends, so
/// a peer whose QUIC connection blipped was indistinguishable from a stranger
/// — which is exactly the peer the grace rule is written for. This is the one
/// piece of state that outlives a session, and it is bounded on both axes:
/// [`MAX_TRACKED_PEERS`] entries, each held [`TOKEN_GRACE_MS`] past the
/// session that created it.
#[derive(Default)]
struct EstablishedPeers {
    entries: HashMap<NodeId, EstablishedPeer>,
}

impl EstablishedPeers {
    /// Record a peer admitted on a valid token, and mark its session live.
    fn admit(&mut self, node: NodeId, claims: SessionTokenClaimsV1, token: &[u8], now_ms: u64) {
        self.evict_idle(now_ms);
        if !self.entries.contains_key(&node) && self.entries.len() >= MAX_TRACKED_PEERS {
            // Nothing is refused here — the session is admitted either way.
            // What is lost is grace for *this* peer later, which is the
            // conservative direction to fail in.
            debug!(%node, "coordinator: established-peer table full, no grace recorded");
            return;
        }
        self.entries.insert(
            node,
            EstablishedPeer {
                claims,
                token: token.to_vec(),
                idle_since_ms: None,
            },
        );
    }

    /// Note that a peer's session ended, starting its retention clock.
    fn retire(&mut self, node: NodeId, now_ms: u64) {
        if let Some(entry) = self.entries.get_mut(&node) {
            entry.idle_since_ms = Some(now_ms);
        }
        self.evict_idle(now_ms);
    }

    /// Is this the same peer, presenting the same token it established with?
    ///
    /// The token bytes and the claims must both match, which is the gateway's
    /// test too. It matters: without it, grace would accept *any* expired
    /// token for a known NodeId, including one with a different account or a
    /// better `standing` than the session actually held.
    fn recognises(
        &mut self,
        node: NodeId,
        claims: &SessionTokenClaimsV1,
        token: &[u8],
        now_ms: u64,
    ) -> bool {
        self.evict_idle(now_ms);
        self.entries
            .get(&node)
            .is_some_and(|entry| &entry.claims == claims && entry.token == token)
    }

    fn evict_idle(&mut self, now_ms: u64) {
        self.entries.retain(|_, entry| {
            entry
                .idle_since_ms
                .is_none_or(|idle_since| now_ms.saturating_sub(idle_since) <= TOKEN_GRACE_MS)
        });
    }
}

/// Is a token that has expired still inside the grace window?
///
/// Measured from the token's own signed expiry, not from when the session
/// ended, so a peer cannot extend grace by reconnecting repeatedly.
fn within_grace(claims: &SessionTokenClaimsV1, now_ms: u64) -> bool {
    let expires_ms = claims
        .issued_at_ms
        .0
        .saturating_add(claims.ttl_ms.0)
        .saturating_add(TOKEN_GRACE_MS);
    now_ms <= expires_ms
}

/// The coordinator's shared state, owned by the accept loop and its sessions.
struct Shared {
    registry: tokio::sync::Mutex<IslandRegistry>,
    peers: tokio::sync::Mutex<HashMap<NodeId, PeerSession>>,
    issuer: InterestIssuer,
    /// The witness-epoch signing key, when this coordinator seeds sets.
    witness_issuer: Option<WitnessEpochIssuer>,
    /// Per-cell epoch state. Kept even when no issuer is configured so that
    /// enabling seeding on a running deployment does not need a restart's
    /// worth of session history to rebuild before anything is eligible.
    seeder: tokio::sync::Mutex<WitnessSeeder>,
    grid: GridId,
    presence_clock: Arc<dyn PresenceClock>,
    /// Wall clock, used only to stamp drain deadlines.
    ///
    /// Deliberately not `presence_clock`: that one is monotonic from an
    /// arbitrary origin, which is right for a rate limiter and useless for a
    /// deadline, because a deadline goes on the wire and has to mean something
    /// on the recipient's clock too. This is the same clock that verifies
    /// session-token expiry, for the same reason.
    unix_clock: Arc<dyn TokenClock + Send + Sync>,
    /// Identities that have completed a `Hello` here, held past their
    /// sessions so docs/09 §8's "established" qualifier is answerable.
    established: tokio::sync::Mutex<EstablishedPeers>,
    /// Whether identity is reachable. Read on exactly one path: an otherwise
    /// valid token that has expired.
    identity_health: SharedIdentityHealth,
    presence_reports: AtomicU64,
    grants_issued: AtomicU64,
    manifests_pushed: AtomicU64,
    drains_issued: AtomicU64,
    witness_epochs_seeded: AtomicU64,
    witness_epochs_delivered: AtomicU64,
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

    /// Seed the cells a presence report touched, and hand every announcement
    /// to the peers that cover its cell (D28 clause (a)).
    ///
    /// The peers are the delivery mechanism, and that is the design rather
    /// than a shortcut: a gateway needs only the coordinator's public key to
    /// check an announcement, so adding gateways adds no coordinator fan-out
    /// and an epoch is recorded exactly when some peer's traffic makes it
    /// load-bearing. It is also why the announcement goes to *everyone*
    /// covering the cell and not only to the drawn witnesses — a peer that is
    /// not witnessing still submits intents that will be judged against these
    /// bytes, and any of them can carry them.
    async fn seed_witness_epochs(&self, cells: &[CellId]) {
        let Some(issuer) = self.witness_issuer.as_ref() else {
            return;
        };
        let now_ms = self.presence_clock.now_ms();
        let mut announcements = Vec::new();
        {
            let registry = self.registry.lock().await;
            let mut seeder = self.seeder.lock().await;
            for cell in cells {
                match seeder.maybe_seed(issuer, &registry, *cell, now_ms) {
                    SeedOutcome::Seeded(epoch) => announcements.push(epoch),
                    // A cell inside its reseed floor, an unchanged pool, or a
                    // pool below the floor are all "no announcement", and the
                    // last one is deliberately not a short set: D29's
                    // low-population path is what covers a cell that cannot
                    // field a witness set, not a set of four pretending.
                    SeedOutcome::Cooling
                    | SeedOutcome::Unchanged
                    | SeedOutcome::BelowFloor { .. } => {}
                }
            }
        }
        for epoch in announcements {
            self.witness_epochs_seeded.fetch_add(1, Ordering::Relaxed);
            debug!(
                cell = %epoch.claims.cell,
                epoch = epoch.claims.epoch,
                handle = epoch.claims.handle,
                pool = epoch.claims.candidates.len(),
                selected = epoch.claims.selected.len(),
                "coordinator: seeded a witness epoch"
            );
            let message = CoordMsg::WitnessEpoch {
                announcement: epoch.announcement,
            };
            for node in epoch.recipients {
                if self.notify(node, &message).await {
                    self.witness_epochs_delivered
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
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

    /// Order every drained island retired, addressed to the roster it held at
    /// its last populated epoch.
    ///
    /// This is the one membership change that has no manifest to carry it: a
    /// drained island has no roster left, so a peer told only in manifests
    /// would learn about the drain by never hearing anything again. The
    /// deadline is the coordinator's wall clock plus the configured grace —
    /// D7's lease TTL by default, so the order expires no sooner than the
    /// leases it is asking to see released.
    async fn order_drains(&self, drains: &[IslandDrain], grace_ms: u64) {
        if drains.is_empty() {
            return;
        }
        let deadline = self.unix_clock.now_ms().0.saturating_add(grace_ms);
        for drain in drains {
            for node in &drain.peers {
                let order = CoordMsg::Drain {
                    island: drain.island,
                    deadline,
                };
                if self.notify(*node, &order).await {
                    self.drains_issued.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Ship a membership change: drains first, then manifests.
    ///
    /// Order matters for a peer that moved out of the last cell of one island
    /// and into another. It receives the order retiring what it left before
    /// the assignment naming what it joined, so it never has both islands live
    /// at once — which is the state the drain exists to avoid.
    async fn apply(&self, change: MembershipChange, grace_ms: u64) {
        self.order_drains(&change.drains, grace_ms).await;
        self.broadcast(change.manifests).await;
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
    /// Drain orders delivered.
    pub drains_issued: u64,
    /// Witness epochs drawn and signed.
    pub witness_epochs_seeded: u64,
    /// Witness-epoch announcements handed to a peer to courier.
    pub witness_epochs_delivered: u64,
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

        let token_clock = Arc::clone(&config.token_clock);
        let shared = Arc::new(Shared {
            registry: tokio::sync::Mutex::new(IslandRegistry::new()),
            peers: tokio::sync::Mutex::new(HashMap::new()),
            issuer: config.interest_issuer,
            witness_issuer: config.witness_issuer,
            seeder: tokio::sync::Mutex::new(WitnessSeeder::with_config(
                config.grid,
                config.witness_seed,
            )),
            grid: config.grid,
            presence_clock: config.presence_clock,
            unix_clock: token_clock,
            established: tokio::sync::Mutex::new(EstablishedPeers::default()),
            identity_health: config.identity_health,
            presence_reports: AtomicU64::new(0),
            grants_issued: AtomicU64::new(0),
            manifests_pushed: AtomicU64::new(0),
            drains_issued: AtomicU64::new(0),
            witness_epochs_seeded: AtomicU64::new(0),
            witness_epochs_delivered: AtomicU64::new(0),
        });
        let authorizer = Arc::new(SessionAuthorizer {
            clock: config.token_clock,
            issuer_keys: config.issuer_keys,
        });

        let (shutdown, rx) = oneshot::channel();
        let join = tokio::spawn(accept_loop(
            Arc::clone(&endpoint),
            Arc::clone(&shared),
            authorizer,
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
            drains_issued: self.shared.drains_issued.load(Ordering::Relaxed),
            witness_epochs_seeded: self.shared.witness_epochs_seeded.load(Ordering::Relaxed),
            witness_epochs_delivered: self.shared.witness_epochs_delivered.load(Ordering::Relaxed),
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
    authorizer: Arc<SessionAuthorizer>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let shared = Arc::clone(&shared);
                let authorizer = Arc::clone(&authorizer);
                let coordinator = endpoint.id();
                tokio::spawn(handle_connection(incoming, shared, authorizer, coordinator));
            }
        }
    }
}

async fn handle_connection(
    incoming: iroh::endpoint::Incoming,
    shared: Arc<Shared>,
    authorizer: Arc<SessionAuthorizer>,
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
                // docs/09 §8's grace rule: an expired-but-otherwise-valid
                // token is accepted for an *established* session while
                // identity is unreachable. Without it a transient identity
                // outage is a topology event — a peer whose connection blips
                // cannot re-establish presence, drops out of its island
                // manifest, and enough of them trip D28 clause (g)'s churn
                // and pool-collapse reseeds. An outage locks out new logins;
                // it must not end in-flight play.
                let unix_now_ms = shared.unix_clock.now_ms().0;
                let (claims, graced) = match authorizer.authorize(&token, &remote) {
                    Ok(Admission::Valid(claims)) => (claims, false),
                    Ok(Admission::Expired(claims)) => {
                        if shared.identity_health.is_available() {
                            debug!(
                                %remote,
                                "coordinator: rejected an expired token while identity is up"
                            );
                            break;
                        }
                        if !within_grace(&claims, unix_now_ms) {
                            debug!(
                                %remote,
                                issued_at_ms = claims.issued_at_ms.0,
                                ttl_ms = claims.ttl_ms.0,
                                grace_ms = TOKEN_GRACE_MS,
                                "coordinator: expired token is past the grace window"
                            );
                            break;
                        }
                        if !shared.established.lock().await.recognises(
                            remote,
                            &claims,
                            &token,
                            unix_now_ms,
                        ) {
                            debug!(
                                %remote,
                                "coordinator: no established session to grace — a login, not a reconnect"
                            );
                            break;
                        }
                        // Loud on purpose. An operator reading a coordinator
                        // log mid-incident has to be able to tell a graced
                        // admission from an ordinary one, and how stale the
                        // token it rode in on was.
                        warn!(
                            %remote,
                            account = claims.account.0,
                            expired_for_ms = unix_now_ms.saturating_sub(
                                claims.issued_at_ms.0.saturating_add(claims.ttl_ms.0)
                            ),
                            grace_ms = TOKEN_GRACE_MS,
                            "coordinator: token grace — established session admitted while identity is unreachable"
                        );
                        (claims, true)
                    }
                    Err(error) => {
                        debug!(%remote, ?error, "coordinator: rejected session token");
                        break;
                    }
                };
                let now_ms = shared.presence_clock.now_ms();
                // Keep the account and the standing this session authenticated
                // as. These used to be dropped on the floor the instant the
                // signature checked out, and four of D28 clause (e)'s six
                // witness-eligibility filters rest on nothing more than
                // retaining them: they are already inside a signature this
                // process verifies, from an issuer it already trusts.
                // A graced session's `standing` is only as fresh as an expired
                // token, so it plays but does not witness — D28 clause (e)
                // reads that field, and a quarantine applied during the
                // outage is invisible here. `note_grace_session` carries the
                // reasoning.
                if graced {
                    shared.seeder.lock().await.note_grace_session(
                        remote,
                        claims.account,
                        claims.standing,
                        now_ms,
                    );
                } else {
                    shared.seeder.lock().await.note_session(
                        remote,
                        claims.account,
                        claims.standing,
                        now_ms,
                    );
                    // Only a token identity itself vouched for right now
                    // establishes a session to grace later.
                    shared.established.lock().await.admit(
                        remote,
                        claims.clone(),
                        &token,
                        unix_now_ms,
                    );
                }
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

                let covered = cells.clone();
                let (change, grace_ms) = {
                    let mut registry = shared.registry.lock().await;
                    let grace_ms = registry.config.drain_grace_ms;
                    (registry.report_presence(remote, cells), grace_ms)
                };
                // Interest first: a peer that receives its manifest and starts
                // claiming should already hold the grant those claims need.
                shared.issue_interest(remote).await;
                shared.apply(change, grace_ms).await;
                // Then the witness sets for the cells this report touched. It
                // goes last because a peer needs its island roster before an
                // announcement naming peers in it means anything, and because
                // most reports seed nothing at all — the reseed floor makes
                // this a hash lookup per cell in the common case.
                shared.seed_witness_epochs(&covered).await;
            }
            // Everything else is coordinator→peer. A peer sending one is
            // confused rather than hostile; ignore it.
            CoordMsg::Welcome { .. }
            | CoordMsg::IslandAssignment { .. }
            | CoordMsg::InterestGrant { .. }
            | CoordMsg::Drain { .. }
            | CoordMsg::WitnessEpoch { .. } => {}
        }
    }

    if admitted {
        // Forget the peer, then tell whoever is left. A departed peer must not
        // linger in a manifest, or survivors will try to reach a ghost.
        let (change, grace_ms) = {
            let mut registry = shared.registry.lock().await;
            let grace_ms = registry.config.drain_grace_ms;
            (registry.forget_peer(remote), grace_ms)
        };
        // The drain order goes out while the departing peer's session is still
        // in the table. Dropping it first would leave `notify` with nowhere to
        // send, and the order is addressed to exactly that peer — it is the
        // one that emptied the island.
        shared.order_drains(&change.drains, grace_ms).await;
        shared.peers.lock().await.remove(&remote);
        shared.broadcast(change.manifests).await;
        // A departure puts the account on the reseed cooldown (D28 clause
        // (g)): a colluder's only lever on the draw is to leave and come back
        // to force a redraw, and forfeiting six draws to buy one is what
        // makes that a losing move. The session facts go with it, so a
        // reconnect is a fresh presence clock as well.
        shared
            .seeder
            .lock()
            .await
            .forget_session(remote, shared.presence_clock.now_ms());
        // What does *not* go with it is the identity this session
        // established. That record is the only reason a reconnect during an
        // identity outage can be told from a fresh login, so it starts a
        // retention clock here instead of being dropped.
        shared
            .established
            .lock()
            .await
            .retire(remote, shared.unix_clock.now_ms().0);
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
