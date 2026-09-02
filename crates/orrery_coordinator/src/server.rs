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
use tracing::{debug, info, warn};

use orrery_protocol::channels::{decode_stream_frame, encode_stream_frame, untag, Channel};
use orrery_protocol::{
    AccountId, AccountInvalidation, AccountStandings, CellId, CoordMsg, FixedTokenClock, GridId,
    IssuerKey, NodeId, SessionTokenClaimsV1, SessionTokenV1, SessionTokenVerificationError,
    SessionTokenVerifier, TokenClock, UnixMillis, COORD_ALPN, COORD_PROTOCOL_VERSION,
    MAX_PRESENCE_CELLS,
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

/// The name D32 clause (c) gives control C5: the `control` field of every
/// shadow observation this process emits for standing enforcement.
const STRIKES_CONTROL: &str = "strikes";

/// The tracing target standing-shadow observations are emitted on.
///
/// Deliberately the same string persistd's shadow arms use, so one filter
/// catches every control's would-be actions — and deliberately a copied
/// literal rather than a shared constant, for the same reason
/// [`IdentityHealth`] is a copied trait: there is no crate both services
/// already share that owns enforcement vocabulary. When one exists, move both.
const STRIKES_SHADOW_TARGET: &str = "orrery::ramp::shadow";

/// Where this coordinator learns that identity has invalidated accounts'
/// outstanding session tokens (D33 clause (e)).
///
/// A deliberate second copy of the same-named seam in
/// `orrery_persistd::gateway`, for the same reason [`IdentityHealth`] is: the
/// two services read identity's publication from different places, and
/// neither has an implementation to share yet.
#[async_trait::async_trait]
pub trait StandingInvalidationFeed: Send + Sync {
    /// The invalidations currently in force, in full each call. Absence is
    /// never read as a retraction (see `orrery_persistd`'s seam for the full
    /// argument): recovery runs through minting, not through un-publishing.
    ///
    /// # Errors
    ///
    /// A failure keeps the previous entries; a flaky feed degrades to stale
    /// enforcement, never to none.
    async fn invalidations(&self) -> Result<Vec<AccountInvalidation>, FeedFailure>;
}

/// Shared standing-invalidation feed.
pub type SharedStandingInvalidationFeed = Arc<dyn StandingInvalidationFeed>;

/// Why reading the invalidation feed failed.
///
/// A copy of `orrery_persistd::gateway::FeedFailure`, kept local so the two
/// crates can drift in their error reporting without a shared dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedFailure(pub String);

impl core::fmt::Display for FeedFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "standing-invalidation feed: {}", self.0)
    }
}

impl core::error::Error for FeedFailure {}

/// D32 clause (c)'s three postures for control C5, as this coordinator
/// consumes them.
///
/// A deliberate second copy of `orrery_persistd::gateway`'s
/// `StrikesEnforcement`, like [`IdentityHealth`] and [`FeedFailure`] above:
/// one lever per process, and no shared crate to own the vocabulary yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrikesMode {
    /// The control does not exist here: the feed is not consulted, nothing is
    /// evaluated or counted. D32 clause (b): "Off observes nothing".
    #[default]
    Off,
    /// The full predicate runs against real admissions — every invalidation
    /// is evaluated exactly as `Live` would — and the would-be action is
    /// recorded on [`STRIKES_SHADOW_TARGET`] while admission proceeds.
    Shadow,
    /// Refuse invalidated accounts at `Hello` and terminate their open
    /// sessions: presence gone, island membership gone, witness pool gone.
    Live,
}

impl core::str::FromStr for StrikesMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "live" => Ok(Self::Live),
            other => Err(format!(
                "unknown strikes mode `{other}` (expected one of: off, shadow, live)"
            )),
        }
    }
}

/// The runtime half of C5's lever on this process: a posture cell shared by
/// everything that consults standing state here.
///
/// The same design as persistd's gateway-side cell, and for the same reason:
/// a startup argument cannot demote a running control, so the cell is the
/// seam an operator-plane writer sets and a test writes into directly.
#[derive(Debug, Clone)]
pub struct StrikesPosture(Arc<std::sync::atomic::AtomicU8>);

impl Default for StrikesPosture {
    fn default() -> Self {
        Self::new(StrikesMode::Off)
    }
}

impl StrikesPosture {
    /// A posture cell starting at `mode`.
    #[must_use]
    pub fn new(mode: StrikesMode) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU8::new(Self::code(mode))))
    }

    /// The mode in force right now.
    #[must_use]
    pub fn get(&self) -> StrikesMode {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            0 => StrikesMode::Off,
            1 => StrikesMode::Shadow,
            // Unreachable: `code` is the only writer and it emits 0, 1 or 2.
            // Written as the acting arm rather than as a panic because a torn
            // read here would under-enforce, which is the wrong direction.
            _ => StrikesMode::Live,
        }
    }

    /// Set the mode. The operator lever, and the only one that may promote.
    pub fn set(&self, mode: StrikesMode) {
        self.0
            .store(Self::code(mode), std::sync::atomic::Ordering::Relaxed);
    }

    /// D32 clause (f)'s trip: demote an acting control to shadow, and refuse
    /// to do anything else.
    ///
    /// Automation may make the fleet safer without asking, never less safe, so
    /// this moves `Live → Shadow` and nothing else. Returns whether the posture
    /// moved; `false` is the ordinary answer for a control already `Shadow` or
    /// `Off`.
    pub fn auto_suspend(&self) -> bool {
        self.0
            .compare_exchange(
                Self::code(StrikesMode::Live),
                Self::code(StrikesMode::Shadow),
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    const fn code(mode: StrikesMode) -> u8 {
        match mode {
            StrikesMode::Off => 0,
            StrikesMode::Shadow => 1,
            StrikesMode::Live => 2,
        }
    }
}

/// Read-only access to the coordinator's C5 durable posture.
///
/// The interface intentionally contains no write operation: authenticating
/// operator posture writes is D32 open question 1. `None` means the startup
/// default stands.
#[async_trait::async_trait]
pub trait StrikesPostureReader: Send + Sync {
    /// Read the current durable C5 mode.
    async fn read_strikes(&self) -> Result<Option<StrikesMode>, String>;
}

/// A process-shared C5 posture reader.
pub type SharedStrikesPostureReader = Arc<dyn StrikesPostureReader>;

/// Poll C5's durable posture into the cell every coordinator consumer shares.
///
/// The cadence and immediate first tick match persistd's C1/C4/C5 pollers, and
/// an absent row restores `startup_default`.
///
/// Read failure is [`StrikesPosture::auto_suspend`]: `Live → Shadow`,
/// suppressing punitive action while retaining incident evidence. `Off` and
/// `Shadow` remain unchanged, so a failure never promotes or blinds the
/// control. Retaining the last known mode instead would leave refusals armed at
/// exactly the moment the operator's demotion lever is unreadable — the
/// p1-swarm incident's shape (#926), where a value the process could no longer
/// re-read kept refusing every client until a restart.
#[must_use]
pub fn spawn_strikes_posture_poller(
    reader: SharedStrikesPostureReader,
    posture: StrikesPosture,
    startup_default: StrikesMode,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            match reader.read_strikes().await {
                Ok(mode) => posture.set(mode.unwrap_or(startup_default)),
                Err(error) => {
                    let suspended = posture.auto_suspend();
                    warn!(
                        control = STRIKES_CONTROL,
                        %error,
                        suspended,
                        "could not refresh enforcement posture; suppressing any live action"
                    );
                }
            }
        }
    })
}

/// What a `Hello` should do about standing, given the posture in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandingVerdict {
    /// Nothing applies, or shadow suppresses it.
    Admit,
    /// Live mode refuses; the connection closes before any admission effect.
    Refuse,
    /// Shadow mode records what live would have done, then admits anyway.
    WouldRefuse,
}

/// This coordinator's copy of identity's invalidation set, plus the change
/// signal open sessions wait on.
///
/// The watermark rule is the gateway consumer's (`AccountInvalidations`
/// there): an entry kills exactly the tokens minted *before* it, entries are
/// never removed because they went missing from a poll — recovery runs
/// through minting — and the map grows only by distinct struck accounts. The
/// watch channel is what makes termination reach an idle peer within one poll
/// interval: every open session subscribes before its `Hello` and re-checks
/// itself on each change, so the accept loop never has to walk the peers
/// table to find whose account moved.
struct StandingState {
    feed: Option<SharedStandingInvalidationFeed>,
    posture: StrikesPosture,
    entries: tokio::sync::RwLock<HashMap<AccountId, u64>>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl StandingState {
    fn new(feed: Option<SharedStandingInvalidationFeed>, posture: StrikesPosture) -> Self {
        Self {
            feed,
            posture,
            entries: tokio::sync::RwLock::new(HashMap::new()),
            changed: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Subscribe a session to invalidation changes. Call before `Hello`, so
    /// no entry can land unobserved between the admission check and the
    /// subscription.
    fn watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    async fn invalidates(&self, account: AccountId, issued_at_ms: u64) -> bool {
        self.entries
            .read()
            .await
            .get(&account)
            .is_some_and(|watermark| *watermark > issued_at_ms)
    }

    /// What `Hello` should do with these claims under the current posture.
    async fn hello_verdict(&self, account: AccountId, issued_at_ms: u64) -> StandingVerdict {
        // Off observes nothing: not the predicate, not even its evaluation.
        let mode = self.posture.get();
        if mode == StrikesMode::Off {
            return StandingVerdict::Admit;
        }
        if !self.invalidates(account, issued_at_ms).await {
            return StandingVerdict::Admit;
        }
        match mode {
            StrikesMode::Shadow => StandingVerdict::WouldRefuse,
            StrikesMode::Live => StandingVerdict::Refuse,
            StrikesMode::Off => unreachable!("handled above"),
        }
    }

    /// One posture poll: refresh from the feed when the control exists.
    ///
    /// D32 clause (b): `Off` observes nothing, not even the poll. A failed
    /// fetch keeps the previous entries. Any applied change bumps the watch
    /// epoch every open session is waiting on.
    async fn sweep(&self) {
        let Some(feed) = self.feed.as_ref() else {
            return;
        };
        if self.posture.get() == StrikesMode::Off {
            return;
        }
        let fetched = match feed.invalidations().await {
            Ok(fetched) => fetched,
            Err(error) => {
                warn!(%error, "coordinator: standing-invalidation feed failed; keeping previous entries");
                return;
            }
        };
        let mut entries = self.entries.write().await;
        let mut applied = 0usize;
        for invalidation in fetched {
            let effective_from_ms = invalidation.effective_from_ms.0;
            match entries.get_mut(&invalidation.account) {
                Some(held) => {
                    if *held < effective_from_ms {
                        *held = effective_from_ms;
                        applied += 1;
                    }
                }
                None => {
                    entries.insert(invalidation.account, effective_from_ms);
                    applied += 1;
                }
            }
        }
        drop(entries);
        if applied > 0 {
            let next = self.changed.borrow().wrapping_add(1);
            let _ = self.changed.send(next);
        }
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
    /// Where identity's account-generation invalidations are read from (D33
    /// clause (e)): cooldown and ban admission decisions enforced against
    /// open sessions here. `None` is C5 absent at this coordinator — with it,
    /// [`strikes_posture`](Self::strikes_posture) has nothing to act on.
    ///
    /// The publisher is identity's scorer, whose service half does not exist
    /// yet; until it does only harnesses and tests wire one, which keeps every
    /// default behaviour-preserving.
    pub standing_feed: Option<SharedStandingInvalidationFeed>,
    /// Runtime lever for control C5 on this coordinator (`ramp/strikes`'s
    /// consumption half). Defaults to [`StrikesMode::Off`], preserving landed
    /// behaviour exactly.
    ///
    /// It governs [`standing_updates`](Self::standing_updates) too. One
    /// control, one lever: quarantine propagation and cooldown/ban termination
    /// are both C5, so a clause (f) auto-suspend that demoted only one of them
    /// would not have demoted C5.
    pub strikes_posture: StrikesPosture,
    /// Durable C5 posture reader. `None` leaves the startup posture in force.
    pub strikes_posture_reader: Option<SharedStrikesPostureReader>,
    /// Identity's standing assertions for accounts whose sessions are already
    /// open (D33 clause (e), the **quarantine** half).
    ///
    /// The sibling of [`standing_feed`](Self::standing_feed), not a
    /// replacement: that one carries cooldown/ban invalidations and ends
    /// sessions, this one carries a standing value and moves a live session in
    /// place. D28 clause (e) reads a session's standing for witness
    /// eligibility, so this is the seam that stops a stale standing seating a
    /// quarantined account on a set that judges intents.
    ///
    /// **The default is inert**, and with C5 at `Off` not even the poll
    /// happens, so a deployment configuring neither behaves exactly as it did
    /// before this field existed.
    pub standing_updates: Arc<AccountStandings>,
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
            standing_feed: None,
            strikes_posture: StrikesPosture::default(),
            strikes_posture_reader: None,
            standing_updates: Arc::new(AccountStandings::inert()),
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
    /// C5's consumption half: identity's invalidation set, its posture, and
    /// the change signal open sessions wait on (D33 clause (e)).
    standing: Arc<StandingState>,
    /// C5's *other* consumption half: identity's standing assertions, drained
    /// on the same 1 s tick. Terminating half above, in-place half here.
    standing_updates: Arc<AccountStandings>,
    presence_reports: AtomicU64,
    interest_crossings: AtomicU64,
    grants_issued: AtomicU64,
    manifests_pushed: AtomicU64,
    drains_issued: AtomicU64,
    witness_epochs_seeded: AtomicU64,
    witness_epochs_delivered: AtomicU64,
    /// Hellos refused because identity had invalidated the account.
    standing_hellos_refused: AtomicU64,
    /// Open sessions terminated for an invalidated account.
    standing_sessions_terminated: AtomicU64,
    /// Shadow Hellos live would have refused.
    shadow_hellos_would_refuse: AtomicU64,
    /// Shadow open sessions live would have terminated.
    shadow_sessions_would_terminate: AtomicU64,
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
        self.deliver_interest(node, grant).await;
    }

    /// Deliver one already-signed interest grant and account it once.
    async fn deliver_interest(&self, node: NodeId, grant: Vec<u8>) {
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
    /// Immediate committed-cell crossings accepted.
    pub interest_crossings: u64,
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
    /// Hellos refused because identity had invalidated the account
    /// (`Live` only).
    pub standing_hellos_refused: u64,
    /// Open sessions terminated for an invalidated account (`Live` only).
    pub standing_sessions_terminated: u64,
    /// Shadow Hellos that would have been refused — the numerator of D32
    /// clause (e)'s false-positive count for this control's coordinator half.
    pub shadow_hellos_would_refuse: u64,
    /// Shadow open sessions that would have been terminated.
    pub shadow_sessions_would_terminate: u64,
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
    posture_poller: Option<tokio::task::JoinHandle<()>>,
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
        let startup_strikes = config.strikes_posture.get();
        let standing = Arc::new(StandingState::new(
            config.standing_feed,
            config.strikes_posture.clone(),
        ));
        let posture_poller = config.strikes_posture_reader.map(|reader| {
            spawn_strikes_posture_poller(reader, config.strikes_posture, startup_strikes)
        });
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
            standing: Arc::clone(&standing),
            standing_updates: config.standing_updates,
            presence_reports: AtomicU64::new(0),
            interest_crossings: AtomicU64::new(0),
            grants_issued: AtomicU64::new(0),
            manifests_pushed: AtomicU64::new(0),
            drains_issued: AtomicU64::new(0),
            witness_epochs_seeded: AtomicU64::new(0),
            witness_epochs_delivered: AtomicU64::new(0),
            standing_hellos_refused: AtomicU64::new(0),
            standing_sessions_terminated: AtomicU64::new(0),
            shadow_hellos_would_refuse: AtomicU64::new(0),
            shadow_sessions_would_terminate: AtomicU64::new(0),
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
            posture_poller,
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
            interest_crossings: self.shared.interest_crossings.load(Ordering::Relaxed),
            grants_issued: self.shared.grants_issued.load(Ordering::Relaxed),
            manifests_pushed: self.shared.manifests_pushed.load(Ordering::Relaxed),
            drains_issued: self.shared.drains_issued.load(Ordering::Relaxed),
            witness_epochs_seeded: self.shared.witness_epochs_seeded.load(Ordering::Relaxed),
            witness_epochs_delivered: self.shared.witness_epochs_delivered.load(Ordering::Relaxed),
            standing_hellos_refused: self.shared.standing_hellos_refused.load(Ordering::Relaxed),
            standing_sessions_terminated: self
                .shared
                .standing_sessions_terminated
                .load(Ordering::Relaxed),
            shadow_hellos_would_refuse: self
                .shared
                .shadow_hellos_would_refuse
                .load(Ordering::Relaxed),
            shadow_sessions_would_terminate: self
                .shared
                .shadow_sessions_would_terminate
                .load(Ordering::Relaxed),
            connected_peers: self.shared.peers.lock().await.len(),
            islands: self.shared.registry.lock().await.island_count(),
        }
    }

    /// Stop accepting, close the endpoint, and await the accept task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.endpoint.close().await;
        let _ = self.join.await;
        if let Some(poller) = self.posture_poller {
            poller.abort();
            let _ = poller.await;
        }
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
            // D33 clause (e)'s poll rides a 1 s tick of its own, matching the
            // gateway's maintenance cadence: one posture poll plus apply is
            // D32 clause (c)'s ≤2 s fleet bound, and every open session learns
            // of an applied change through its own watch subscription rather
            // than through this walk.
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                shared.standing.sweep().await;
                // The quarantine half rides the same tick, because it is the
                // same control and the same publisher cadence. Unlike the
                // termination half above it cannot be delivered through the
                // per-session watch: what changes is a *fact this process
                // holds about* the session — the standing D28 clause (e) reads
                // when it draws a witness set — not something the peer must be
                // told. So this one does walk the seeder's table, under the
                // lock the handshake path already takes for a moment.
                for (node, standing) in shared
                    .seeder
                    .lock()
                    .await
                    .apply_standing_updates(&shared.standing_updates, shared.standing.posture.get())
                {
                    debug!(
                        %node,
                        ?standing,
                        "coordinator: identity moved an open session's standing"
                    );
                }
            }
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
    // The account this session authenticated as, and the instant its token
    // was minted — the pair the watermark rule reads. Set on admission, so
    // the invalidation watch below knows what to re-check.
    let mut session_identity: Option<(orrery_protocol::AccountId, u64)> = None;
    // Subscribed before the first `Hello`, so no entry can land unobserved
    // between the admission check and the watch going live (D33 clause (e)).
    let mut standing_changes = shared.standing.watch();
    loop {
        tokio::select! {
            packet = conn.read_datagram() => {
                let packet = match packet {
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
                    // D33 clause (e): identity refused this account at mint
                    // time, so a token minted before the refusal must not
                    // establish or re-establish presence here — not even on
                    // grace, which exists for an identity *outage* and cannot
                    // resurrect a standing decision that predates it.
                    match shared
                        .standing
                        .hello_verdict(claims.account, claims.issued_at_ms.0)
                        .await
                    {
                        StandingVerdict::Admit => {}
                        StandingVerdict::WouldRefuse => {
                            shared.shadow_hellos_would_refuse.fetch_add(1, Ordering::Relaxed);
                            info!(
                                target: STRIKES_SHADOW_TARGET,
                                control = STRIKES_CONTROL,
                                issuer = %remote,
                                account = claims.account.0,
                                action = "would_refuse_hello",
                                observed_at_ms = unix_now_ms,
                                "standing invalidation would have refused this Hello; admitted in shadow"
                            );
                        }
                        StandingVerdict::Refuse => {
                            warn!(
                                issuer = %remote,
                                account = claims.account.0,
                                issued_at_ms = claims.issued_at_ms.0,
                                "coordinator: refusing Hello for an account whose tokens identity invalidated"
                            );
                            shared.standing_hellos_refused.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                    let now_ms = shared.presence_clock.now_ms();
                    // Keep the identity this session authenticated as — the
                    // account, the standing, and the probation flag. These used
                    // to be dropped on the floor the instant the signature
                    // checked out, and five of D28 clause (e)'s six
                    // witness-eligibility filters rest on nothing more than
                    // retaining them: they are already inside a signature this
                    // process verifies, from an issuer it already trusts.
                    // A graced session's `standing` is only as fresh as an expired
                    // token, so it plays but does not witness — D28 clause (e)
                    // reads that field, and a quarantine applied during the
                    // outage is invisible here. `note_grace_session` carries the
                    // reasoning.
                    if graced {
                        shared
                            .seeder
                            .lock()
                            .await
                            .note_grace_session(remote, &claims, now_ms);
                    } else {
                        shared
                            .seeder
                            .lock()
                            .await
                            .note_session(remote, &claims, now_ms);
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
                    session_identity = Some((claims.account, claims.issued_at_ms.0));
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
                CoordMsg::InterestCellCrossing { crossing } => {
                    if !admitted {
                        continue;
                    }
                    if crossing.covered_cells.is_empty()
                        || crossing.covered_cells.len() > MAX_PRESENCE_CELLS
                    {
                        debug!(
                            %remote,
                            count = crossing.covered_cells.len(),
                            "coordinator: unusable crossing coverage"
                        );
                        continue;
                    }
                    // Crossings share the presence bucket: the event is
                    // immediate relative to the one-hertz bulk clock, not an
                    // unmetered way to force island evaluation and signing.
                    let now_ms = shared.presence_clock.now_ms();
                    {
                        let mut peers = shared.peers.lock().await;
                        let Some(session) = peers.get_mut(&remote) else {
                            break;
                        };
                        if !session.budget.take(now_ms) {
                            debug!(%remote, "coordinator: crossing rate limited");
                            continue;
                        }
                    }
                    let covered = crossing.covered_cells.clone();
                    let (issued, grace_ms) = {
                        let mut registry = shared.registry.lock().await;
                        let grace_ms = registry.config.drain_grace_ms;
                        (
                            shared.issuer.apply_crossing(
                                &mut registry,
                                remote,
                                shared.grid,
                                crossing,
                            ),
                            grace_ms,
                        )
                    };
                    let Ok(issued) = issued else {
                        debug!(%remote, error = %issued.unwrap_err(), "coordinator: refused crossing");
                        continue;
                    };
                    shared.interest_crossings.fetch_add(1, Ordering::Relaxed);
                    // Same ordering as bulk presence, but on the crossing
                    // edge: grant before roster, witness set after both.
                    shared.deliver_interest(remote, issued.grant).await;
                    shared.apply(issued.membership, grace_ms).await;
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
            changed = standing_changes.changed() => {
                // A gone sender means the coordinator is shutting down; the
                // endpoint close ends this session either way.
                if changed.is_err() {
                    break;
                }
                if let Some((account, issued_at_ms)) = session_identity {
                    if shared.standing.invalidates(account, issued_at_ms).await {
                        warn!(
                            issuer = %remote,
                            account = account.0,
                            "coordinator: terminating a session whose tokens identity invalidated"
                        );
                        shared.standing_sessions_terminated.fetch_add(1, Ordering::Relaxed);
                        // Breaking runs the ordinary disconnect path below:
                        // presence forgotten, island roster rebroadcast, the
                        // account out of every witness pool it sat in.
                        break;
                    }
                }
            }
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

#[cfg(test)]
mod posture_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    enum FakeRead {
        Mode(Option<StrikesMode>),
        Failure,
    }

    struct MutableReader(Mutex<FakeRead>);

    impl MutableReader {
        fn new(read: FakeRead) -> Arc<Self> {
            Arc::new(Self(Mutex::new(read)))
        }

        fn set(&self, read: FakeRead) {
            *self.0.lock().expect("posture reader lock") = read;
        }
    }

    #[async_trait::async_trait]
    impl StrikesPostureReader for MutableReader {
        async fn read_strikes(&self) -> Result<Option<StrikesMode>, String> {
            match self.0.lock().expect("posture reader lock").clone() {
                FakeRead::Mode(mode) => Ok(mode),
                FakeRead::Failure => Err("injected failure".into()),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn c5_coordinator_poller_applies_store_changes_without_a_restart() {
        let reader = MutableReader::new(FakeRead::Mode(None));
        let posture = StrikesPosture::new(StrikesMode::Off);
        let state = StandingState::new(None, posture.clone());
        let account = AccountId::new(7);
        state.entries.write().await.insert(account, 2_000);
        let poller = spawn_strikes_posture_poller(
            Arc::clone(&reader) as SharedStrikesPostureReader,
            posture.clone(),
            StrikesMode::Off,
        );
        tokio::task::yield_now().await;
        assert_eq!(
            state.hello_verdict(account, 1_000).await,
            StandingVerdict::Admit
        );

        reader.set(FakeRead::Mode(Some(StrikesMode::Live)));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            state.hello_verdict(account, 1_000).await,
            StandingVerdict::Refuse,
            "the durable live row must change the running coordinator's C5 action"
        );

        reader.set(FakeRead::Mode(None));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            state.hello_verdict(account, 1_000).await,
            StandingVerdict::Admit,
            "removing the row restores the off startup default"
        );
        poller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn c5_coordinator_posture_read_failure_demotes_live_but_does_not_promote_off() {
        let reader = MutableReader::new(FakeRead::Failure);
        let posture = StrikesPosture::new(StrikesMode::Live);
        let state = StandingState::new(None, posture.clone());
        let account = AccountId::new(8);
        state.entries.write().await.insert(account, 2_000);
        let poller = spawn_strikes_posture_poller(
            Arc::clone(&reader) as SharedStrikesPostureReader,
            posture,
            StrikesMode::Live,
        );
        tokio::task::yield_now().await;
        assert_eq!(
            state.hello_verdict(account, 1_000).await,
            StandingVerdict::WouldRefuse,
            "an unreadable posture suppresses C5 action but preserves evaluation"
        );
        poller.abort();

        let posture = StrikesPosture::new(StrikesMode::Off);
        let state = StandingState::new(None, posture.clone());
        state.entries.write().await.insert(account, 2_000);
        let poller = spawn_strikes_posture_poller(
            reader as SharedStrikesPostureReader,
            posture,
            StrikesMode::Off,
        );
        tokio::task::yield_now().await;
        assert_eq!(
            state.hello_verdict(account, 1_000).await,
            StandingVerdict::Admit,
            "an unreadable posture must not promote off into observation"
        );
        poller.abort();
    }
}
