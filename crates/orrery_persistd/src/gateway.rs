//! The gateway: the iroh endpoint that terminates client gateway sessions and
//! routes them onto the cell-actor runtime (docs/10-crates.md §9, §11).
//!
//! This is the server mirror of `orrery_persist_client::gateway`. A client
//! connects over iroh (ALPN `orrery/gateway/0`), completes the aeronet-style
//! admission handshake (one uni-stream carrying `[ACCEPTED]`), then speaks two
//! lanes (D3): unreliable datagrams for bulk state, and reliable
//! unidirectional streams for control (see [`crate::reliable`]). Both carry
//! tagged [`GatewayMsg`]s, and the tag — not the lane — is what routes them:
//!
//! - [`GatewayMsg::Diff`] → route to the owning cell actor (journal append +
//!   fold) and ack with the durable LSN (the ack *is* the durability contract,
//!   D11 §2.1).
//! - [`GatewayMsg::Subscribe`] → read the requested cells from the owning
//!   actors and stream [`AreaPage`]s back (D11 §9; the client orders
//!   nearest-first so it can spawn-in against page one, D16).
//! - [`GatewayMsg::SubmitIntent`] → the intent execution path (D11 §2.2):
//!   verify the issuer signature, bind `intent.issuer` to the connection's
//!   authenticated id, run the [`IntentValidator`] admission check, then hand
//!   the intent to the configured [`IntentExecutor`] and ack **only after**
//!   its future resolves — a `Committed` ack implies a durable commit (RPO 0).
//!   With no executor configured the reply is `Rejected`, never a fake commit.
//! - [`GatewayMsg::Hello`] / [`GatewayMsg::VersionedHello`] → acknowledge with
//!   the gateway node id + protocol (the versioned form is checked against the
//!   `{V, V−1}` window and refused with [`GatewayReply::HelloRefused`]; the
//!   unversioned one is accepted unchecked)
//!   version.
//!
//! The transport is the **raw** iroh endpoint — this crate is **Bevy-free**
//! (D15) and does not run the aeronet session stack. It speaks exactly the wire
//! surface `aeronet_iroh`'s client side expects: the admission uni-stream, then
//! datagrams and `[u32 LE length][payload]`-framed uni-streams, so the existing
//! gateway client connects unmodified.
//!
//! Because `CellRuntime` is `Send` but not `Sync` (its `CellActorHandle`s hold
//! `JoinHandle`s), the gateway shares it behind a `tokio::sync::Mutex`. For a
//! single persistd node this is correct serialization; a real distributed
//! deployment would route by rendezvous placement instead (docs/08-persistence.md
//! §3), but the current reference binary does not ship that transport.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
};
use orrery_protocol::{
    verify_interest_grant, AccountId, AreaPage, CellId, CoordinatorInterestSnapshot, DiffUplink,
    DiscrepancyReport, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOutcome, IssuerKey,
    JournalRecord, LeaseId, LeaseMsg, Lsn, NodeId, PersistId, SessionTokenClaimsV1, SessionTokenV1,
    SessionTokenVerificationError, SessionTokenVerifier, Tick, UnixMillis, Verdict,
    MAX_AREA_PAGE_FRAME_BYTES, MAX_SESSION_TOKEN_TTL_MS, PROTOCOL_VERSION, REASON_BAD_SIGNATURE,
    REASON_ISSUER_MISMATCH, REASON_NO_EXECUTOR, REPORT_ADJUDICATED, REPORT_REFUSED_NO_ADJUDICATOR,
    REPORT_REFUSED_NO_SESSION, REPORT_REFUSED_RATE_LIMITED, REPORT_REFUSED_REPORTER_MISMATCH,
};

use crate::actor::{FencedApply, Reject};
use crate::adjudication::AdjudicationExecutor;
use crate::cluster::{LeaseRenewal, Router};
use crate::intent::{
    error_outcome, IntentContext, IntentVerdict, PermissiveValidator, SharedExecutor,
    SharedValidator,
};
use crate::lease::registrar_now_ms;
use crate::payload_crc;
use crate::reliable;

/// The ALPN the gateway advertises and accepts. Matches the client's
/// `orrery_persist_client::gateway::GATEWAY_ALPN`.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The admission response byte, mirroring `aeronet_iroh`'s `ACCEPTED`.
const ACCEPTED: u8 = 0;

/// Result of verifying a gateway session token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayAuthorization {
    /// The token is valid at the injected gateway clock.
    Valid(SessionTokenClaimsV1),
    /// The token is authentic and transport-bound, but its lifetime elapsed.
    Expired(SessionTokenClaimsV1),
}

/// Verifies the identity token carried by [`GatewayMsg::Hello`].
pub trait GatewayAuthorizer: Send + Sync {
    /// Verify `token` for the connected transport `node` at `now_ms`.
    fn authorize(
        &self,
        token: &[u8],
        node: &NodeId,
        now_ms: UnixMillis,
    ) -> Result<GatewayAuthorization, SessionTokenVerificationError>;
}

/// Shared gateway session-token verifier.
pub type SharedGatewayAuthorizer = Arc<dyn GatewayAuthorizer>;

/// Default-deny authorizer used until a deployment explicitly supplies keys.
#[derive(Debug, Default)]
pub struct DenyAllGatewayAuthorizer;

impl GatewayAuthorizer for DenyAllGatewayAuthorizer {
    fn authorize(
        &self,
        _token: &[u8],
        _node: &NodeId,
        _now_ms: UnixMillis,
    ) -> Result<GatewayAuthorization, SessionTokenVerificationError> {
        Err(SessionTokenVerificationError::Malformed)
    }
}

/// V1 authorizer backed by configured identity issuer keys.
#[derive(Debug, Clone)]
pub struct SessionTokenV1Authorizer {
    issuer_keys: Vec<IssuerKey>,
}

impl SessionTokenV1Authorizer {
    /// Build an authorizer from the trusted identity issuer-key set.
    #[must_use]
    pub fn new(issuer_keys: impl IntoIterator<Item = IssuerKey>) -> Self {
        Self {
            issuer_keys: issuer_keys.into_iter().collect(),
        }
    }
}

impl GatewayAuthorizer for SessionTokenV1Authorizer {
    fn authorize(
        &self,
        token: &[u8],
        node: &NodeId,
        now_ms: UnixMillis,
    ) -> Result<GatewayAuthorization, SessionTokenVerificationError> {
        let verifier = SessionTokenVerifier::new(
            orrery_protocol::FixedTokenClock::new(now_ms),
            self.issuer_keys.clone(),
        );
        match verifier.verify(token, node) {
            Ok(claims) => Ok(GatewayAuthorization::Valid(claims)),
            Err(SessionTokenVerificationError::Expired) => {
                let decoded = SessionTokenV1::decode(token)?;
                let issued_at = decoded.claims.issued_at_ms;
                let expiry_verifier = SessionTokenVerifier::new(
                    orrery_protocol::FixedTokenClock::new(issued_at),
                    self.issuer_keys.clone(),
                );
                expiry_verifier
                    .verify(token, node)
                    .map(GatewayAuthorization::Expired)
            }
            Err(error) => Err(error),
        }
    }
}

/// Injected Unix clock used for session-token admission.
pub trait GatewayClock: Send + Sync {
    /// Return the current Unix timestamp in milliseconds.
    fn now_ms(&self) -> UnixMillis;
}

/// Shared gateway clock.
pub type SharedGatewayClock = Arc<dyn GatewayClock>;

/// Production Unix wall clock.
#[derive(Debug, Default)]
pub struct SystemGatewayClock;

impl GatewayClock for SystemGatewayClock {
    fn now_ms(&self) -> UnixMillis {
        let milliseconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        UnixMillis::new(milliseconds)
    }
}

/// Injected monotonic clock used for claim-rate admission.
pub trait ClaimClock: Send + Sync {
    /// Return elapsed monotonic milliseconds from an arbitrary process-local origin.
    fn now_ms(&self) -> u64;
}

/// Shared monotonic clock used by the D16 claim limiter.
pub type SharedClaimClock = Arc<dyn ClaimClock>;

/// Production monotonic clock for claim-rate admission.
#[derive(Debug)]
pub struct SystemClaimClock {
    started: Instant,
}

impl Default for SystemClaimClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl ClaimClock for SystemClaimClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Reports whether identity is reachable for expiry decisions.
pub trait IdentityHealth: Send + Sync {
    /// Return `true` only while the identity service is known to be available.
    fn is_available(&self) -> bool;
}

/// Shared identity-service health source.
pub type SharedIdentityHealth = Arc<dyn IdentityHealth>;

/// Healthy-by-default production health until an outage monitor reports otherwise.
#[derive(Debug, Default)]
pub struct AvailableIdentityHealth;

impl IdentityHealth for AvailableIdentityHealth {
    fn is_available(&self) -> bool {
        true
    }
}

/// How many bulk updates one connection routes concurrently.
///
/// The P2 load client spreads 10k entities over 125 sessions.  A session emits
/// roughly 80 diffs on each 2 Hz scheduler tick.  Keeping the old eight-route
/// cap made the receive loop wait for seven durability waves before it could
/// even read the later datagrams, turning an otherwise sub-2 ms journal commit
/// into a 20+ ms client acknowledgement.  This cap admits one complete P2
/// tick per session while still bounding a misbehaving peer's task count.
const MAX_INFLIGHT_DIFF_ROUTES_PER_CONN: usize = 128;

/// Slow control reads are separately bounded so they cannot consume the bulk
/// acknowledgement budget.  In particular, an FDB cold area-load must not
/// head-of-line block a durability acknowledgement on the same connection.
const MAX_INFLIGHT_CONTROL_ROUTES_PER_CONN: usize = 8;

/// How many critical intent transactions one connection may execute at once.
///
/// Intent commits perform an FDB transaction and can take materially longer
/// than a journal append.  They therefore have their own non-waiting lane:
/// the datagram reader must keep draining bulk/control traffic while an
/// intent is in flight.  Unlike the bulk and control lanes, saturation is not
/// queued behind an `await acquire`: it is an immediate, definitive refusal.
/// This prevents a peer from turning queued tasks into unbounded memory use.
const MAX_INFLIGHT_INTENT_ROUTES_PER_CONN: usize = 16;

const MAX_PEER_REGISTRY_ENTRIES: usize = 4_096;

const MAX_PEER_LIVE_LEASES: usize = 256;

/// The adjudication executor a gateway routes discrepancy reports to
/// (docs/07-witnessing.md §3 stage 4).
///
/// `Arc<AdjudicationExecutor>` rather than `Arc<dyn …>`: the executor is
/// already the indirection — it boxes one worker closure per retained rules
/// build — so a second trait object would only hide the seam. It is shared
/// across connections because retention is a *cluster* property (D16 keeps
/// three builds), not a session's.
pub type SharedAdjudicator = Arc<AdjudicationExecutor>;

/// Reports one account may file per second, sustained.
///
/// Derived rather than invented: a witness watches at most
/// `MAX_WITNESS_LINKS` = 7 subjects, escalates a given divergence episode
/// **once** (`Catchup::reported` in `orrery_witness`), and every window it can
/// file spans up to `MAX_ADJUDICATION_TICKS` = 180 ticks, i.e. three seconds.
/// An honest reporter therefore tops out near 7 reports per 3 s ≈ 2.3/s, and
/// only if every one of its subjects diverged at once.
const REPORTS_PER_SECOND: u64 = 3;

/// Reports one account may file back to back before the sustained rate binds.
///
/// One per watched subject, twice over: a witness that rejoins an island and
/// re-anchors seven watches can legitimately find several stale divergences in
/// the same frame, and shedding those would lose exactly the evidence the
/// phase exists to collect.
const REPORT_BURST: u64 = 16;

/// Accounts the report limiter tracks at once.
///
/// The limiter is the one piece of gateway state keyed by *account* rather
/// than by connection, so it needs its own bound: without one, a flooder
/// cycling accounts turns rate limiting into unbounded memory. At the cap the
/// limiter first drops entries that have refilled to full — those are
/// indistinguishable from absent ones — and refuses only if none has.
const MAX_REPORT_LIMITER_ACCOUNTS: usize = 4_096;

/// Per-account report admission (docs/07-witnessing.md §7, "observer is the
/// liar": report spam is rate-limited per account, never struck).
///
/// Per **account**, which is why this cannot live on `PeerState` the way
/// `ClaimBucket` does: that map is keyed by `NodeId`, and one account may hold
/// several. Metering per connection would leave the limit worth as many
/// multiples as the flooder cares to dial.
struct ReportLimiter {
    buckets: tokio::sync::Mutex<HashMap<AccountId, ReportBucket>>,
}

impl ReportLimiter {
    fn new() -> Self {
        Self {
            buckets: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Whether `account` may file one report at `now_ms`.
    async fn admit(&self, account: AccountId, now_ms: u64) -> bool {
        let mut buckets = self.buckets.lock().await;
        if !buckets.contains_key(&account) && buckets.len() >= MAX_REPORT_LIMITER_ACCOUNTS {
            // A bucket at full tokens says exactly what an absent one says, so
            // reclaiming those costs nothing. Anything still spending is the
            // state this limiter exists to keep.
            buckets.retain(|_, bucket| !bucket.is_full(now_ms));
            if buckets.len() >= MAX_REPORT_LIMITER_ACCOUNTS {
                return false;
            }
        }
        buckets
            .entry(account)
            .or_insert_with(|| ReportBucket::new(now_ms))
            .take(now_ms)
    }
}

/// One account's report token bucket. Same shape as [`ClaimBucket`], in
/// thousandths of a token so the refill is exact at millisecond resolution.
#[derive(Debug, Clone, Copy)]
struct ReportBucket {
    token_millis: u64,
    updated_ms: u64,
}

impl ReportBucket {
    const TOKEN_MILLIS_PER_REPORT: u64 = 1_000;
    const BURST_TOKEN_MILLIS: u64 = REPORT_BURST * Self::TOKEN_MILLIS_PER_REPORT;

    const fn new(now_ms: u64) -> Self {
        Self {
            token_millis: Self::BURST_TOKEN_MILLIS,
            updated_ms: now_ms,
        }
    }

    fn refill(&mut self, now_ms: u64) {
        if now_ms > self.updated_ms {
            let replenished = now_ms
                .saturating_sub(self.updated_ms)
                .saturating_mul(REPORTS_PER_SECOND);
            self.token_millis = self
                .token_millis
                .saturating_add(replenished)
                .min(Self::BURST_TOKEN_MILLIS);
            self.updated_ms = now_ms;
        }
    }

    fn take(&mut self, now_ms: u64) -> bool {
        self.refill(now_ms);
        if self.token_millis < Self::TOKEN_MILLIS_PER_REPORT {
            false
        } else {
            self.token_millis -= Self::TOKEN_MILLIS_PER_REPORT;
            true
        }
    }

    fn is_full(&self, now_ms: u64) -> bool {
        let mut probe = *self;
        probe.refill(now_ms);
        probe.token_millis >= Self::BURST_TOKEN_MILLIS
    }
}

/// Decides whether a successful bulk route can make the normal durable-ack
/// claim.
///
/// The current single-node service is always fresh. During fenced activation,
/// the owner monitor supplies this boundary with its grid/cell freshness view;
/// a stale or lost lease deliberately downgrades a successful local journal
/// append to a provisional ack. Intents do not consult this interface: their
/// `Committed` reply remains an RPO-0 statement about the intent executor.
pub trait BulkAckAdmission: Send + Sync {
    /// Assess the ownership/fence freshness for a bulk acknowledgement.
    fn assess(&self, grid: GridId, cell: CellId) -> BulkAckDisposition;
}

/// The durability strength a bulk acknowledgement may claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkAckDisposition {
    /// Ownership is fresh, so the local journal acknowledgement is durable
    /// evidence for the P2 recovery gate.
    Durable,
    /// Ownership is stale or unavailable. The write may have reached the
    /// local actor, but clients must not treat its acknowledgement as durable
    /// recovery evidence.
    Provisional,
}

impl BulkAckDisposition {
    fn is_provisional(self) -> bool {
        matches!(self, Self::Provisional)
    }
}

/// Shared bulk-ack admission policy used by a gateway.
pub type SharedBulkAckAdmission = Arc<dyn BulkAckAdmission>;

/// Supplies the coordinator's latest active-interest snapshot for a peer.
///
/// This synchronous seam deliberately exposes only immutable coordinator
/// snapshots. Gateway client traffic, including [`GatewayMsg::Subscribe`],
/// cannot mutate it.
pub trait InterestAuthority: Send + Sync {
    /// Return the latest coordinator snapshot for `peer`, if one is active.
    fn snapshot_for(&self, peer: NodeId) -> Option<CoordinatorInterestSnapshot>;

    /// Accept a coordinator-signed grant a peer has presented.
    ///
    /// `presenter` is the transport-authenticated identity that handed the
    /// bytes over, and `now_ms` is this gateway's own monotonic clock — the
    /// grant carries a lifetime, never a coordinator timestamp, so the
    /// deadline is stamped here.
    ///
    /// The default refuses: an authority with no coordinator keys configured
    /// has no basis on which to believe anything, and silently ignoring a
    /// grant would leave a peer unable to tell why its claims fail.
    fn apply_grant(
        &self,
        grant: &[u8],
        presenter: &NodeId,
        now_ms: u64,
    ) -> Result<Epoch, orrery_protocol::InterestGrantVerificationError> {
        let _ = (grant, presenter, now_ms);
        Err(orrery_protocol::InterestGrantVerificationError::Unsupported)
    }

    /// Reclaim interest whose gateway-local deadline has passed.
    ///
    /// Expiry is enforced on the read path regardless; this exists so a
    /// long-running gateway does not retain every peer that ever connected.
    /// The default does nothing, which is correct for a fixed snapshot set.
    fn prune_expired(&self, now_ms: u64) {
        let _ = now_ms;
    }

    /// Return whether `peer` currently covers `cell` in `grid`.
    #[must_use]
    fn allows(&self, peer: NodeId, grid: GridId, cell: CellId, now_ms: u64) -> bool {
        self.snapshot_for(peer).is_some_and(|snapshot| {
            snapshot.peer == peer
                && snapshot.grid == grid
                && snapshot.valid_until_ms > now_ms
                && snapshot.covered_cells.contains(&cell)
        })
    }
}

/// Shared coordinator-interest authority injected into a gateway.
pub type SharedInterestAuthority = Arc<dyn InterestAuthority>;

/// The most peers whose interest a gateway retains at once.
///
/// Sized against the peer registry: a grant is only useful while its peer has
/// a session, and this bound is what stops grant presentation from being an
/// unbounded allocation channel.
pub const MAX_INTEREST_PEERS: usize = MAX_PEER_REGISTRY_ENTRIES;

/// Coordinator interest, accepted from peers that carry their own signed grant.
///
/// This is the production [`InterestAuthority`]. It holds only the
/// coordinator's **public** keys: it verifies handouts, it never mints them.
/// A peer therefore cannot widen its own interest, and the gateway needs no
/// connection to the coordinator — the peer is the courier, exactly as it is
/// for its identity token.
#[derive(Debug)]
pub struct CoordinatorHandoutAuthority {
    keys: Vec<IssuerKey>,
    snapshots: std::sync::RwLock<HashMap<NodeId, CoordinatorInterestSnapshot>>,
}

impl CoordinatorHandoutAuthority {
    /// Trust these coordinator signing keys, and nothing else.
    ///
    /// More than one key is the rotation overlap: a new key can be accepted
    /// before the old one is retired, so a rotation needs no flag day.
    #[must_use]
    pub fn new(keys: impl IntoIterator<Item = IssuerKey>) -> Self {
        Self {
            keys: keys.into_iter().collect(),
            snapshots: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// How many peers currently have interest on file.
    #[must_use]
    pub fn tracked_peers(&self) -> usize {
        self.snapshots.read().map(|held| held.len()).unwrap_or(0)
    }
}

impl InterestAuthority for CoordinatorHandoutAuthority {
    fn snapshot_for(&self, peer: NodeId) -> Option<CoordinatorInterestSnapshot> {
        self.snapshots.read().ok()?.get(&peer).cloned()
    }

    fn prune_expired(&self, now_ms: u64) {
        if let Ok(mut held) = self.snapshots.write() {
            held.retain(|_, snapshot| snapshot.valid_until_ms > now_ms);
        }
    }

    fn apply_grant(
        &self,
        grant: &[u8],
        presenter: &NodeId,
        now_ms: u64,
    ) -> Result<Epoch, orrery_protocol::InterestGrantVerificationError> {
        use orrery_protocol::InterestGrantVerificationError as GrantError;

        let claims = verify_interest_grant(grant, presenter, &self.keys)?;
        let snapshot = CoordinatorInterestSnapshot::from_grant(claims, now_ms);
        let epoch = snapshot.epoch;

        let mut held = self.snapshots.write().map_err(|_| GrantError::Malformed)?;
        match held.get(presenter) {
            // A strictly older epoch is a replay of interest the coordinator
            // has already narrowed; refusing it is the point of the epoch.
            Some(current) if epoch < current.epoch => return Err(GrantError::Superseded),
            // The same epoch means the same coverage — the coordinator bumps
            // on every change — so this is a refresh, and re-accepting it
            // extends the deadline without widening anything. Coverage that
            // disagrees at one epoch cannot have come from one coordinator.
            Some(current)
                if epoch == current.epoch && current.covered_cells != snapshot.covered_cells =>
            {
                return Err(GrantError::Superseded);
            }
            _ => {}
        }
        if !held.contains_key(presenter) && held.len() >= MAX_INTEREST_PEERS {
            // Reclaim expired entries before refusing: the cap exists to bound
            // memory, not to lock out a peer behind peers that have left.
            held.retain(|_, snapshot| snapshot.valid_until_ms > now_ms);
            if held.len() >= MAX_INTEREST_PEERS {
                return Err(GrantError::Superseded);
            }
        }
        held.insert(*presenter, snapshot);
        Ok(epoch)
    }
}

/// Default coordinator-interest adapter until coordinator transport lands.
///
/// No snapshot is trusted unless a deployment injects one from its coordinator
/// integration.
#[derive(Debug, Default)]
pub struct DenyAllInterestAuthority;

impl InterestAuthority for DenyAllInterestAuthority {
    fn snapshot_for(&self, _peer: NodeId) -> Option<CoordinatorInterestSnapshot> {
        None
    }
}

/// Immutable in-memory coordinator snapshot adapter for deterministic tests
/// and deployments that already have a snapshot handout path.
#[derive(Debug, Default)]
pub struct SnapshotInterestAuthority {
    snapshots: HashMap<NodeId, CoordinatorInterestSnapshot>,
}

impl SnapshotInterestAuthority {
    /// Build an authority from snapshots, retaining the latest epoch per peer.
    #[must_use]
    pub fn from_snapshots(
        snapshots: impl IntoIterator<Item = CoordinatorInterestSnapshot>,
    ) -> Self {
        let mut latest: HashMap<NodeId, CoordinatorInterestSnapshot> = HashMap::new();
        for snapshot in snapshots {
            match latest.get(&snapshot.peer) {
                Some(current) if current.epoch >= snapshot.epoch => {}
                _ => {
                    latest.insert(snapshot.peer, snapshot);
                }
            }
        }
        Self { snapshots: latest }
    }
}

impl InterestAuthority for SnapshotInterestAuthority {
    fn snapshot_for(&self, peer: NodeId) -> Option<CoordinatorInterestSnapshot> {
        self.snapshots.get(&peer).cloned()
    }
}

/// One candidate for inheriting a lease whose holder was lost (D7 §5).
///
/// Candidacy is deliberately narrow: a peer qualifies only when the
/// coordinator's own interest snapshot still covers the entity's committed
/// cell. That is the same admission rule a weak claim passes, so crash
/// redistribution can never place authority somewhere a live claim could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuccessorCandidate {
    /// The candidate peer's authenticated transport identity.
    pub node: NodeId,
    /// Leases this peer already holds on this gateway.
    pub held_leases: usize,
    /// Whether one of those leases is on an entity committed to the same cell
    /// — the observable proxy for "already interacting with this entity".
    pub holds_lease_in_cell: bool,
}

/// The registrar's request for a successor to a lost lease.
#[derive(Debug, Clone)]
pub struct SuccessorRequest<'a> {
    /// Grid containing `cell`.
    pub grid: GridId,
    /// The entity's committed cell.
    pub cell: CellId,
    /// The entity whose authority is being redistributed.
    pub entity: PersistId,
    /// The holder that was lost.
    pub previous_holder: NodeId,
    /// What ended the lease.
    pub reason: orrery_protocol::ExpireReason,
    /// Eligible peers, in no particular order.
    pub candidates: &'a [SuccessorCandidate],
}

/// Chooses which peer inherits a lease whose holder was lost.
///
/// Returning `None` parks the entity, which is always safe: a parked entity is
/// served read-only from persistence and the first ordinary claim unparks it
/// (D7 §7).
pub trait SuccessorPolicy: Send + Sync {
    /// Pick a successor from `request.candidates`, or `None` to park.
    ///
    /// An implementation must return a node that appears in `candidates`; the
    /// gateway ignores anything else rather than granting to an unvetted peer.
    fn select(&self, request: &SuccessorRequest<'_>) -> Option<NodeId>;
}

/// Shared successor policy injected into a gateway.
pub type SharedSuccessorPolicy = Arc<dyn SuccessorPolicy>;

/// The policy that always parks — the behaviour before successor selection
/// landed, kept as an explicit choice for deployments that do not want
/// redistribution.
#[derive(Debug, Default)]
pub struct ParkOnLossPolicy;

impl SuccessorPolicy for ParkOnLossPolicy {
    fn select(&self, _request: &SuccessorRequest<'_>) -> Option<NodeId> {
        None
    }
}

/// The default "nearest interacting peer" policy (D7 §5).
///
/// True metric proximity needs peer positions the coordinator does not yet
/// hand the gateway, so nearness is read off the data that *is* authoritative
/// here, in this order:
///
/// 1. a peer already holding a lease in the same cell — it is demonstrably
///    interacting with the entity's neighbourhood;
/// 2. the fewest leases already held, so one peer does not inherit a crashed
///    holder's entire working set;
/// 3. the node id, so the choice is deterministic and reproducible in a replay.
///
/// Every candidate covers the entity's exact cell — that is what eligibility
/// means — so coverage itself carries no ranking signal.
#[derive(Debug, Default)]
pub struct NearestInterestSuccessorPolicy;

impl SuccessorPolicy for NearestInterestSuccessorPolicy {
    fn select(&self, request: &SuccessorRequest<'_>) -> Option<NodeId> {
        request
            .candidates
            .iter()
            .filter(|candidate| candidate.node != request.previous_holder)
            .min_by_key(|candidate| {
                (
                    usize::from(!candidate.holds_lease_in_cell),
                    candidate.held_leases,
                    *candidate.node.as_bytes(),
                )
            })
            .map(|candidate| candidate.node)
    }
}

/// One observation of two peers simultaneously believing they held authority
/// over the same entity (D7 §5, the single-writer invariant checker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateAuthoritySample {
    /// The contested entity.
    pub entity: PersistId,
    /// The tick the losing writer stamped on its diff.
    pub tick: orrery_protocol::Tick,
    /// The peer whose write was fenced out.
    pub rejected_writer: NodeId,
    /// The token that peer presented.
    pub rejected_lease_id: LeaseId,
    /// The peer the registrar considers authoritative.
    pub current_holder: NodeId,
    /// The registrar's live token for the entity.
    pub current_lease_id: LeaseId,
}

/// A point-in-time read of [`AuthorityMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuthoritySnapshot {
    /// Fenced writes that arrived while a *different* peer held a live lease.
    pub duplicate_authority: u64,
    /// Leases placed with a selected successor, by crash redistribution or by
    /// negotiated handoff.
    pub reassigned: u64,
    /// Lost leases parked because no successor was eligible.
    pub parked_without_successor: u64,
    /// Leases a holder released or handed off by consent.
    pub divested: u64,
    /// Divestitures refused before any registrar mutation.
    pub divest_rejected: u64,
    /// Divest requests the registrar sent to a holder on a claimant's behalf.
    pub divest_requested: u64,
    /// Requests a holder did not answer before the deadline.
    pub handoff_timed_out: u64,
}

/// Always-on authority telemetry.
///
/// `duplicate_authority` is the single-writer invariant checker the phase
/// requires: it counts bulk writes the registrar fenced out **while another
/// peer held a live, unexpired lease on the same entity** — the only
/// externally observable form of "two peers both believed they were the
/// writer". A healthy cluster leaves it at zero; every increment is also
/// logged at `warn` with both node ids, so the signal exists without a scrape.
#[derive(Debug, Default)]
pub struct AuthorityMetrics {
    duplicate_authority: AtomicU64,
    reassigned: AtomicU64,
    parked_without_successor: AtomicU64,
    divested: AtomicU64,
    divest_rejected: AtomicU64,
    divest_requested: AtomicU64,
    handoff_timed_out: AtomicU64,
    last_duplicate: std::sync::Mutex<Option<DuplicateAuthoritySample>>,
}

impl AuthorityMetrics {
    /// Read every counter.
    #[must_use]
    pub fn snapshot(&self) -> AuthoritySnapshot {
        AuthoritySnapshot {
            duplicate_authority: self.duplicate_authority.load(Ordering::Relaxed),
            reassigned: self.reassigned.load(Ordering::Relaxed),
            parked_without_successor: self.parked_without_successor.load(Ordering::Relaxed),
            divested: self.divested.load(Ordering::Relaxed),
            divest_rejected: self.divest_rejected.load(Ordering::Relaxed),
            divest_requested: self.divest_requested.load(Ordering::Relaxed),
            handoff_timed_out: self.handoff_timed_out.load(Ordering::Relaxed),
        }
    }

    /// The most recent duplicate-authority observation, if any.
    #[must_use]
    pub fn last_duplicate_authority(&self) -> Option<DuplicateAuthoritySample> {
        self.last_duplicate.lock().ok().and_then(|last| *last)
    }

    fn record_reassigned(&self) {
        self.reassigned.fetch_add(1, Ordering::Relaxed);
    }

    fn record_parked_without_successor(&self) {
        self.parked_without_successor
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_divested(&self) {
        self.divested.fetch_add(1, Ordering::Relaxed);
    }

    fn record_divest_rejected(&self) {
        self.divest_rejected.fetch_add(1, Ordering::Relaxed);
    }

    fn record_divest_requested(&self) {
        self.divest_requested.fetch_add(1, Ordering::Relaxed);
    }

    fn record_handoff_timed_out(&self) {
        self.handoff_timed_out.fetch_add(1, Ordering::Relaxed);
    }

    fn record_duplicate_authority(&self, sample: DuplicateAuthoritySample) {
        self.duplicate_authority.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last_duplicate.lock() {
            *last = Some(sample);
        }
        warn!(
            entity = ?sample.entity,
            tick = ?sample.tick,
            rejected_writer = %sample.rejected_writer,
            current_holder = %sample.current_holder,
            "gateway: duplicate-authority write fenced out"
        );
    }
}

/// Inspect a fencing rejection for the single-writer invariant.
///
/// Only a rejection whose live row names a *different* holder with an
/// unexpired lease counts. A rejection against a parked row, an expired row,
/// or the writer's own superseded token is ordinary fencing, not two live
/// writers.
fn observe_fencing_rejection(
    metrics: &AuthorityMetrics,
    entity: PersistId,
    tick: orrery_protocol::Tick,
    rejected_writer: NodeId,
    rejected_lease_id: LeaseId,
    lease: Option<&orrery_protocol::Lease>,
    now_ms: u64,
) {
    let Some(lease) = lease else { return };
    let Some(current_holder) = lease.holder else {
        return;
    };
    if current_holder == rejected_writer || lease.expires_at <= now_ms {
        return;
    }
    metrics.record_duplicate_authority(DuplicateAuthoritySample {
        entity,
        tick,
        rejected_writer,
        rejected_lease_id,
        current_holder,
        current_lease_id: lease.lease_id,
    });
}

/// The shared D16 lattice (`orrery_protocol::metrics`), not a local copy:
/// the server-side histogram must bucket a sample exactly as the client and
/// the gate do, or the two halves of `bulk_ack_ms` are not comparable.
use orrery_protocol::metrics::LATENCY_BOUNDARIES_US as BULK_LATENCY_BOUNDARIES_US;
const NUM_BULK_LATENCY_BUCKETS: usize = BULK_LATENCY_BOUNDARIES_US.len() + 1;

/// One compact server-side latency histogram bucket.
///
/// Shared by every gateway histogram on the D16 lattice — the bulk total, the
/// intent server span and the area first-page span — because the JSONL
/// `sample_batch` record they all drain into has exactly this shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayBulkSample {
    /// Bucket upper bound in microseconds; overflow uses the observed maximum.
    pub value_us: u64,
    /// Successful acknowledgements in this bucket.
    pub count: u64,
}

/// Point-in-time view of the fixed-memory server-side bulk histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayBulkLatencySnapshot {
    buckets: [u64; NUM_BULK_LATENCY_BUCKETS],
    max_us: u64,
}

/// Fixed-memory timing counters for successful bulk acknowledgements.
///
/// These stages bracket the server work outside the journal's own commit
/// telemetry. Sums support interval averages while maxima expose excursions;
/// recording one acknowledgement is a small, fixed number of relaxed atomic
/// operations and never allocates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GatewayBulkSnapshot {
    /// Successfully sent durable or provisional bulk acknowledgements.
    pub acknowledgements: u64,
    /// Time from decoded gateway receipt until the bounded route task starts.
    pub route_queue_us_sum: u64,
    /// Largest decoded-receipt to route-task-start latency.
    pub route_queue_us_max: u64,
    /// Time spent routing to the cell actor and obtaining its append handle.
    pub router_apply_us_sum: u64,
    /// Largest router-apply latency.
    pub router_apply_us_max: u64,
    /// Time from obtaining the append handle until durable resolution.
    pub journal_wait_us_sum: u64,
    /// Largest append-handle durability wait.
    pub journal_wait_us_max: u64,
    /// Admission, acknowledgement encoding, and datagram-send call time.
    pub reply_us_sum: u64,
    /// Largest admission, encoding, and send-call latency.
    pub reply_us_max: u64,
    /// Complete decoded-receipt through datagram-send-call latency.
    pub total_us_sum: u64,
    /// Largest complete decoded-receipt through send-call latency.
    pub total_us_max: u64,
}

/// Thread-safe gateway bulk-stage recorder.
#[derive(Debug)]
pub struct GatewayBulkMetrics {
    acknowledgements: AtomicU64,
    route_queue_us_sum: AtomicU64,
    route_queue_us_max: AtomicU64,
    router_apply_us_sum: AtomicU64,
    router_apply_us_max: AtomicU64,
    journal_wait_us_sum: AtomicU64,
    journal_wait_us_max: AtomicU64,
    reply_us_sum: AtomicU64,
    reply_us_max: AtomicU64,
    total_us_sum: AtomicU64,
    total_us_max: AtomicU64,
    total_buckets: [AtomicU64; NUM_BULK_LATENCY_BUCKETS],
    total_latency_max_us: AtomicU64,
}

impl Default for GatewayBulkMetrics {
    fn default() -> Self {
        Self {
            acknowledgements: AtomicU64::new(0),
            route_queue_us_sum: AtomicU64::new(0),
            route_queue_us_max: AtomicU64::new(0),
            router_apply_us_sum: AtomicU64::new(0),
            router_apply_us_max: AtomicU64::new(0),
            journal_wait_us_sum: AtomicU64::new(0),
            journal_wait_us_max: AtomicU64::new(0),
            reply_us_sum: AtomicU64::new(0),
            reply_us_max: AtomicU64::new(0),
            total_us_sum: AtomicU64::new(0),
            total_us_max: AtomicU64::new(0),
            total_buckets: [const { AtomicU64::new(0) }; NUM_BULK_LATENCY_BUCKETS],
            total_latency_max_us: AtomicU64::new(0),
        }
    }
}

impl GatewayBulkMetrics {
    /// Capture cumulative bulk acknowledgement stage counters.
    #[must_use]
    pub fn snapshot(&self) -> GatewayBulkSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        GatewayBulkSnapshot {
            acknowledgements: load(&self.acknowledgements),
            route_queue_us_sum: load(&self.route_queue_us_sum),
            route_queue_us_max: load(&self.route_queue_us_max),
            router_apply_us_sum: load(&self.router_apply_us_sum),
            router_apply_us_max: load(&self.router_apply_us_max),
            journal_wait_us_sum: load(&self.journal_wait_us_sum),
            journal_wait_us_max: load(&self.journal_wait_us_max),
            reply_us_sum: load(&self.reply_us_sum),
            reply_us_max: load(&self.reply_us_max),
            total_us_sum: load(&self.total_us_sum),
            total_us_max: load(&self.total_us_max),
        }
    }

    /// Return counters added since `previous` and advance that cursor.
    pub fn delta(&self, previous: &mut GatewayBulkSnapshot) -> GatewayBulkSnapshot {
        let current = self.snapshot();
        let sub = |now: u64, before: u64| now.saturating_sub(before);
        let delta = GatewayBulkSnapshot {
            acknowledgements: sub(current.acknowledgements, previous.acknowledgements),
            route_queue_us_sum: sub(current.route_queue_us_sum, previous.route_queue_us_sum),
            route_queue_us_max: current.route_queue_us_max,
            router_apply_us_sum: sub(current.router_apply_us_sum, previous.router_apply_us_sum),
            router_apply_us_max: current.router_apply_us_max,
            journal_wait_us_sum: sub(current.journal_wait_us_sum, previous.journal_wait_us_sum),
            journal_wait_us_max: current.journal_wait_us_max,
            reply_us_sum: sub(current.reply_us_sum, previous.reply_us_sum),
            reply_us_max: current.reply_us_max,
            total_us_sum: sub(current.total_us_sum, previous.total_us_sum),
            total_us_max: current.total_us_max,
        };
        *previous = current;
        delta
    }

    /// Capture the server receipt-through-send-call latency histogram.
    #[must_use]
    pub fn latency_snapshot(&self) -> GatewayBulkLatencySnapshot {
        GatewayBulkLatencySnapshot {
            buckets: self
                .total_buckets
                .each_ref()
                .map(|bucket| bucket.load(Ordering::Relaxed)),
            max_us: self.total_latency_max_us.load(Ordering::Relaxed),
        }
    }

    /// Return histogram buckets added since `previous` and advance the cursor.
    pub fn latency_delta(
        &self,
        previous: &mut GatewayBulkLatencySnapshot,
    ) -> Vec<GatewayBulkSample> {
        let current = self.latency_snapshot();
        let samples = current
            .buckets
            .iter()
            .zip(previous.buckets.iter())
            .enumerate()
            .filter_map(|(index, (&now, &before))| {
                let count = now.saturating_sub(before);
                (count != 0).then_some(GatewayBulkSample {
                    value_us: BULK_LATENCY_BOUNDARIES_US
                        .get(index)
                        .copied()
                        .unwrap_or(current.max_us),
                    count,
                })
            })
            .collect();
        *previous = current;
        samples
    }

    fn record(
        &self,
        route_queue_us: u64,
        router_apply_us: u64,
        journal_wait_us: u64,
        reply_us: u64,
        total_us: u64,
    ) {
        let stage = |sum: &AtomicU64, max: &AtomicU64, value| {
            sum.fetch_add(value, Ordering::Relaxed);
            max.fetch_max(value, Ordering::Relaxed);
        };
        self.acknowledgements.fetch_add(1, Ordering::Relaxed);
        stage(
            &self.route_queue_us_sum,
            &self.route_queue_us_max,
            route_queue_us,
        );
        stage(
            &self.router_apply_us_sum,
            &self.router_apply_us_max,
            router_apply_us,
        );
        stage(
            &self.journal_wait_us_sum,
            &self.journal_wait_us_max,
            journal_wait_us,
        );
        stage(&self.reply_us_sum, &self.reply_us_max, reply_us);
        stage(&self.total_us_sum, &self.total_us_max, total_us);
        let index = BULK_LATENCY_BOUNDARIES_US.partition_point(|&boundary| total_us > boundary);
        self.total_buckets[index].fetch_add(1, Ordering::Relaxed);
        self.total_latency_max_us
            .fetch_max(total_us, Ordering::Relaxed);
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// A fixed-memory latency histogram on the shared D16 lattice.
///
/// [`GatewayBulkMetrics`] keeps its own copy of these buckets inline, next to
/// the per-stage sums only the bulk path has. The intent and area paths
/// measure one span each and need nothing else, so they share this.
#[derive(Debug)]
pub struct GatewayServerLatency {
    buckets: [AtomicU64; NUM_BULK_LATENCY_BUCKETS],
    max_us: AtomicU64,
}

impl Default for GatewayServerLatency {
    fn default() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; NUM_BULK_LATENCY_BUCKETS],
            max_us: AtomicU64::new(0),
        }
    }
}

/// Point-in-time view of a [`GatewayServerLatency`], usable as a drain cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayServerLatencySnapshot {
    buckets: [u64; NUM_BULK_LATENCY_BUCKETS],
    max_us: u64,
}

impl Default for GatewayServerLatencySnapshot {
    fn default() -> Self {
        Self {
            buckets: [0; NUM_BULK_LATENCY_BUCKETS],
            max_us: 0,
        }
    }
}

impl GatewayServerLatency {
    fn record(&self, micros: u64) {
        let index = BULK_LATENCY_BOUNDARIES_US.partition_point(|&boundary| micros > boundary);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
    }

    /// Capture every bucket and the observed maximum.
    #[must_use]
    pub fn snapshot(&self) -> GatewayServerLatencySnapshot {
        GatewayServerLatencySnapshot {
            buckets: self
                .buckets
                .each_ref()
                .map(|bucket| bucket.load(Ordering::Relaxed)),
            max_us: self.max_us.load(Ordering::Relaxed),
        }
    }

    /// Return the buckets added since `previous` and advance that cursor.
    ///
    /// The reported `value_us` is the bucket's upper bound — the overflow
    /// bucket reports the observed maximum — which is the one reconstruction
    /// rule `orrery_protocol::metrics` defines, so a percentile computed from
    /// the artifact means what the server measured.
    pub fn delta(&self, previous: &mut GatewayServerLatencySnapshot) -> Vec<GatewayBulkSample> {
        let current = self.snapshot();
        let samples = current
            .buckets
            .iter()
            .zip(previous.buckets.iter())
            .enumerate()
            .filter_map(|(index, (&now, &before))| {
                let count = now.saturating_sub(before);
                (count != 0).then_some(GatewayBulkSample {
                    value_us: BULK_LATENCY_BOUNDARIES_US
                        .get(index)
                        .copied()
                        .unwrap_or(current.max_us),
                    count,
                })
            })
            .collect();
        *previous = current;
        samples
    }

    /// Total samples recorded since the process started.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .sum()
    }
}

/// A point-in-time read of [`GatewayIntentMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayIntentSnapshot {
    /// Definitive intent acknowledgements sent, refusals included.
    pub replies: u64,
    /// Acknowledgements reporting a durable commit.
    pub committed: u64,
    /// Acknowledgements reporting a rejection, for any reason.
    pub rejected: u64,
    /// Rejections because no executor is configured: this gateway cannot
    /// commit an intent at all, which reads as a deployment fault rather than
    /// a `Ruleset` verdict.
    pub rejected_no_executor: u64,
    /// Intents refused because the bounded execution lane was full. Counted
    /// separately because the wire reason is indistinguishable from an
    /// executor error, and the operator response is the opposite one.
    pub lane_saturated: u64,
    /// Summed receipt-through-reply server span, microseconds.
    pub server_us_sum: u64,
    /// Largest receipt-through-reply server span, microseconds.
    pub server_us_max: u64,
}

/// Always-on intent-path telemetry: the receipt-through-reply server span and
/// the outcome split behind it.
///
/// The span is emitted under `gateway_intent_server_ms`, never under the gated
/// `intent_commit_ms`: this measurement starts when the gateway has already
/// received the submission and ends at its send call, so it is strictly
/// shorter than the client round trip D16 budgets at p99 < 10 ms.
#[derive(Debug, Default)]
pub struct GatewayIntentMetrics {
    replies: AtomicU64,
    committed: AtomicU64,
    rejected: AtomicU64,
    rejected_no_executor: AtomicU64,
    lane_saturated: AtomicU64,
    server_us_sum: AtomicU64,
    server_us_max: AtomicU64,
    latency: GatewayServerLatency,
}

impl GatewayIntentMetrics {
    /// Read every counter.
    #[must_use]
    pub fn snapshot(&self) -> GatewayIntentSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        GatewayIntentSnapshot {
            replies: load(&self.replies),
            committed: load(&self.committed),
            rejected: load(&self.rejected),
            rejected_no_executor: load(&self.rejected_no_executor),
            lane_saturated: load(&self.lane_saturated),
            server_us_sum: load(&self.server_us_sum),
            server_us_max: load(&self.server_us_max),
        }
    }

    /// The receipt-through-reply histogram, for the JSONL reporter.
    #[must_use]
    pub fn latency(&self) -> &GatewayServerLatency {
        &self.latency
    }

    fn record_lane_saturated(&self) {
        self.lane_saturated.fetch_add(1, Ordering::Relaxed);
    }

    fn record_reply(&self, outcome: &IntentOutcome, server_us: u64) {
        self.replies.fetch_add(1, Ordering::Relaxed);
        match outcome {
            IntentOutcome::Committed { .. } => {
                self.committed.fetch_add(1, Ordering::Relaxed);
            }
            IntentOutcome::Rejected { reason } => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                if *reason == REASON_NO_EXECUTOR {
                    self.rejected_no_executor.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.server_us_sum.fetch_add(server_us, Ordering::Relaxed);
        self.server_us_max.fetch_max(server_us, Ordering::Relaxed);
        self.latency.record(server_us);
    }
}

/// A point-in-time read of [`GatewayAreaMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayAreaSnapshot {
    /// `Subscribe` messages routed.
    pub subscribes: u64,
    /// Subscribes that produced a first `AreaPage` frame, and therefore a
    /// latency sample.
    pub first_pages: u64,
    /// `AreaPage` frames sent, across every chunk of every cell.
    pub frames: u64,
    /// Cell reads that failed and answered with an `AreaLoadError`.
    pub cell_read_errors: u64,
    /// Summed receipt-through-first-page server span, microseconds.
    pub first_page_us_sum: u64,
    /// Largest receipt-through-first-page server span, microseconds.
    pub first_page_us_max: u64,
}

/// Always-on area-load telemetry: the receipt-through-first-page server span
/// and the frame and failure counts around it.
///
/// The span is emitted under `gateway_area_first_page_server_ms`, never under
/// the gated `area_first_page_ms`, for the same reason the intent span carries
/// its own name.
#[derive(Debug, Default)]
pub struct GatewayAreaMetrics {
    subscribes: AtomicU64,
    first_pages: AtomicU64,
    frames: AtomicU64,
    cell_read_errors: AtomicU64,
    first_page_us_sum: AtomicU64,
    first_page_us_max: AtomicU64,
    latency: GatewayServerLatency,
}

impl GatewayAreaMetrics {
    /// Read every counter.
    #[must_use]
    pub fn snapshot(&self) -> GatewayAreaSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        GatewayAreaSnapshot {
            subscribes: load(&self.subscribes),
            first_pages: load(&self.first_pages),
            frames: load(&self.frames),
            cell_read_errors: load(&self.cell_read_errors),
            first_page_us_sum: load(&self.first_page_us_sum),
            first_page_us_max: load(&self.first_page_us_max),
        }
    }

    /// The receipt-through-first-page histogram, for the JSONL reporter.
    #[must_use]
    pub fn latency(&self) -> &GatewayServerLatency {
        &self.latency
    }

    fn record_subscribe(&self) {
        self.subscribes.fetch_add(1, Ordering::Relaxed);
    }

    fn record_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    fn record_cell_read_error(&self) {
        self.cell_read_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn record_first_page(&self, server_us: u64) {
        self.first_pages.fetch_add(1, Ordering::Relaxed);
        self.first_page_us_sum
            .fetch_add(server_us, Ordering::Relaxed);
        self.first_page_us_max
            .fetch_max(server_us, Ordering::Relaxed);
        self.latency.record(server_us);
    }
}

/// A point-in-time read of [`GatewayReportMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayReportSnapshot {
    /// Every answer sent for a discrepancy report, refusals included. A report
    /// has exactly one reply, so this is also the number of reports handled.
    pub verdicts: u64,
    /// Reports that reached the adjudicator and came back with a verdict.
    pub adjudicated: u64,
    /// Verdicts proving a deviation.
    pub confirms: u64,
    /// Verdicts finding re-execution matched within the tolerance bands. The
    /// denominator of the shadow-mode false-positive rate (D17.3).
    pub exonerates: u64,
    /// Verdicts finding the *reporter* fabricated evidence.
    pub evidence_forged: u64,
    /// Verdicts that could not decide.
    pub unadjudicable: u64,
    /// Refused because this gateway has no adjudicator linked. A stock build
    /// refuses every report this way by design (docs/09 §1), which is exactly
    /// why it needs its own counter and not an "errors" bucket.
    pub refused_no_adjudicator: u64,
    /// Refused because the reporter's account is over its D16 report budget.
    pub refused_rate_limited: u64,
    /// Refused because the report was filed in another peer's name.
    pub refused_reporter_mismatch: u64,
    /// Refused because the connection has no bound account to bill.
    pub refused_no_session: u64,
    /// Refused with a reason code this build does not know. Non-zero means a
    /// refusal path was added without a counter.
    pub refused_other: u64,
}

/// Always-on discrepancy-report telemetry, split by outcome and refusal
/// reason.
///
/// Recorded at the single reply choke point, so the split is exhaustive by
/// construction: a report that produced no reply is a bug in the gateway, not
/// a gap here. The refusal split is what makes shadow mode legible — a cluster
/// answering every report `REPORT_REFUSED_NO_ADJUDICATOR` looks, from the
/// witness side alone, exactly like one that exonerates everybody.
#[derive(Debug, Default)]
pub struct GatewayReportMetrics {
    verdicts: AtomicU64,
    adjudicated: AtomicU64,
    confirms: AtomicU64,
    exonerates: AtomicU64,
    evidence_forged: AtomicU64,
    unadjudicable: AtomicU64,
    refused_no_adjudicator: AtomicU64,
    refused_rate_limited: AtomicU64,
    refused_reporter_mismatch: AtomicU64,
    refused_no_session: AtomicU64,
    refused_other: AtomicU64,
}

impl GatewayReportMetrics {
    /// Read every counter.
    #[must_use]
    pub fn snapshot(&self) -> GatewayReportSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        GatewayReportSnapshot {
            verdicts: load(&self.verdicts),
            adjudicated: load(&self.adjudicated),
            confirms: load(&self.confirms),
            exonerates: load(&self.exonerates),
            evidence_forged: load(&self.evidence_forged),
            unadjudicable: load(&self.unadjudicable),
            refused_no_adjudicator: load(&self.refused_no_adjudicator),
            refused_rate_limited: load(&self.refused_rate_limited),
            refused_reporter_mismatch: load(&self.refused_reporter_mismatch),
            refused_no_session: load(&self.refused_no_session),
            refused_other: load(&self.refused_other),
        }
    }

    fn record(&self, verdict: Option<&Verdict>, reason: u16) {
        self.verdicts.fetch_add(1, Ordering::Relaxed);
        if let Some(verdict) = verdict {
            self.adjudicated.fetch_add(1, Ordering::Relaxed);
            let counter = match verdict {
                Verdict::Confirms { .. } => &self.confirms,
                Verdict::Exonerates => &self.exonerates,
                Verdict::EvidenceForged(_) => &self.evidence_forged,
                Verdict::Unadjudicable(_) => &self.unadjudicable,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let counter = match reason {
            REPORT_REFUSED_NO_ADJUDICATOR => &self.refused_no_adjudicator,
            REPORT_REFUSED_RATE_LIMITED => &self.refused_rate_limited,
            REPORT_REFUSED_REPORTER_MISMATCH => &self.refused_reporter_mismatch,
            REPORT_REFUSED_NO_SESSION => &self.refused_no_session,
            _ => &self.refused_other,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Every always-on server-side gateway counter, in one shareable handle.
///
/// Collection is unconditional — the precedent is [`AuthorityMetrics`], and
/// the reason is the same: a counter that only exists when someone remembered
/// a flag is not telemetry, it is a debugging session you have to schedule in
/// advance. The JSONL sink stays optional; the counters do not.
///
/// **What this does not do.** `persistd` has no scrape endpoint and no admin
/// surface, so a node started without `--metrics-jsonl` accumulates these and
/// exposes them to nothing. This keeps them warm and correct for the D12 OTel
/// bridge that will read them; it does **not** make them reachable on a
/// running node without a restart.
#[derive(Debug, Default)]
pub struct GatewayMetrics {
    /// Bulk acknowledgement stages and the server-side bulk histogram.
    pub bulk: GatewayBulkMetrics,
    /// Intent receipt-through-reply span and outcome split.
    pub intent: GatewayIntentMetrics,
    /// Area-load receipt-through-first-page span and frame counts.
    pub area: GatewayAreaMetrics,
    /// Discrepancy-report outcome and refusal split.
    pub report: GatewayReportMetrics,
}

/// The current single-node policy: its ownership is always fresh.
#[derive(Debug, Default)]
pub struct FreshBulkAckAdmission;

impl BulkAckAdmission for FreshBulkAckAdmission {
    fn assess(&self, _grid: GridId, _cell: CellId) -> BulkAckDisposition {
        BulkAckDisposition::Durable
    }
}

/// The registrar-monotonic clock driving the periodic lease-TTL sweep.
///
/// Deliberately the same trait as the claim-admission clock: both read the
/// registrar's own monotonic milliseconds and never a peer or wall clock.
#[derive(Debug, Default)]
pub struct RegistrarSweepClock;

impl ClaimClock for RegistrarSweepClock {
    fn now_ms(&self) -> u64 {
        registrar_now_ms()
    }
}

/// Configuration for the [`GatewayServer`].
#[derive(Clone)]
pub struct GatewayConfig {
    /// The application protocol to advertise/accept. Defaults to
    /// [`GATEWAY_ALPN`].
    pub alpn: Vec<u8>,
    /// The iroh relay mode. `RelayMode::Disabled` for loopback tests.
    pub relay_mode: RelayMode,
    /// An optional secret key pinning a stable gateway node id across runs.
    pub secret_key: Option<iroh::SecretKey>,
    /// The local address to bind. Port `0` asks the OS for an ephemeral port.
    pub bind: SocketAddr,
    /// The protocol version reported in [`GatewayReply::HelloAck`]. Defaults to
    /// [`PROTOCOL_VERSION`].
    pub protocol_version: u16,
    /// The intent executor (D11 §2.2, second stage). `None` means intents
    /// cannot commit durably, so the gateway rejects them honestly
    /// ([`REASON_NO_EXECUTOR`]) rather than acking a commit that never
    /// happened — the inverted RPO-0 the stub had.
    pub executor: Option<SharedExecutor>,
    /// The discrepancy-report adjudicator (docs/07-witnessing.md §3 stage 4).
    /// `None` means this gateway cannot judge evidence, so it refuses reports
    /// with [`REPORT_REFUSED_NO_ADJUDICATOR`] rather than dropping them — the
    /// same honesty [`executor`](Self::executor) owes intents, for the same
    /// reason: silence is indistinguishable from a slow cluster, and a witness
    /// would re-file against it forever.
    ///
    /// **The default is `None` and stays `None`.** Adjudication re-runs a
    /// concrete `Ruleset`, and docs/09-services-and-ops.md §1 is normative that
    /// "the game team links its `Ruleset` and builds the deployed binary" —
    /// so this crate ships the registration seam and registers nothing. A
    /// cluster with an adjudicator is one somebody built one into.
    pub adjudicator: Option<SharedAdjudicator>,
    /// The intent admission validator (D11 §2.2, first stage). Defaults to
    /// the permissive stub so the harness runs unconfigured; a linked
    /// `Ruleset` swaps in real validation.
    pub validator: SharedValidator,
    /// Ownership/fence freshness policy for bulk acknowledgements. The default
    /// preserves the current single-node durable-ack behavior; activation can
    /// inject its three-second fence freshness monitor here.
    pub bulk_ack_admission: SharedBulkAckAdmission,
    /// Coordinator-owned active-interest source for future weak-claim
    /// plausibility checks. The default denies every peer until coordinator
    /// transport injects a snapshot authority.
    pub interest_authority: SharedInterestAuthority,
    /// Session-token verifier. Defaults to explicit denial.
    pub authorizer: SharedGatewayAuthorizer,
    /// Unix clock used by the session-token verifier.
    pub identity_clock: SharedGatewayClock,
    /// Identity-service health used only for established-token expiry grace.
    pub identity_health: SharedIdentityHealth,
    /// Always-on server-side telemetry: bulk stages, the intent and area
    /// server spans, and the report outcome split. Share the handle to read
    /// it; a fresh one is created when the caller does not, and collection is
    /// unconditional either way (see [`GatewayMetrics`] for what that does and
    /// does not buy).
    pub metrics: Arc<GatewayMetrics>,
    /// Maximum retained authority peers, capped at 4,096.
    pub peer_registry_capacity: usize,
    /// Maximum live leases tracked for one authenticated NodeId, capped at 256.
    pub peer_lease_capacity: usize,
    /// Idle established-identity retention used for identity-outage grace.
    pub peer_idle_retention_ms: u64,
    /// Monotonic source for NodeId-scoped D16 claim admission.
    pub claim_clock: SharedClaimClock,
    /// Who inherits a lease whose holder was lost (D7 §5).
    ///
    /// The default ranks by coordinator interest. Candidacy still requires a
    /// live coordinator snapshot covering the entity's cell, so the shipped
    /// default combined with the default [`DenyAllInterestAuthority`] parks
    /// every lost lease — redistribution turns on with the coordinator, not
    /// before it.
    pub successor_policy: SharedSuccessorPolicy,
    /// Always-on single-writer invariant telemetry. Share the handle to scrape
    /// it; a fresh one is created when the caller does not.
    pub authority_metrics: Arc<AuthorityMetrics>,
    /// Registrar clock the periodic TTL sweep reads. Injectable so a test can
    /// advance expiry without sleeping through a 10 s lease.
    pub lease_sweep_clock: SharedClaimClock,
    /// How long a holder has to answer a registrar divest request, in
    /// milliseconds (D7 §4.2 default: 300).
    ///
    /// Past it, a *weak* holder is divested unconditionally — an interaction
    /// must not stall on an unresponsive peer — while strong ownership is
    /// kept, because stealing by timeout is exactly what "not stealable"
    /// forbids.
    pub handoff_deadline_ms: u32,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            alpn: GATEWAY_ALPN.to_vec(),
            relay_mode: RelayMode::Disabled,
            secret_key: None,
            bind: "127.0.0.1:0".parse().expect("static valid loopback addr"),
            protocol_version: PROTOCOL_VERSION,
            executor: None,
            adjudicator: None,
            validator: Arc::new(PermissiveValidator),
            bulk_ack_admission: Arc::new(FreshBulkAckAdmission),
            interest_authority: Arc::new(DenyAllInterestAuthority),
            authorizer: Arc::new(DenyAllGatewayAuthorizer),
            identity_clock: Arc::new(SystemGatewayClock),
            identity_health: Arc::new(AvailableIdentityHealth),
            metrics: Arc::new(GatewayMetrics::default()),
            peer_registry_capacity: MAX_PEER_REGISTRY_ENTRIES,
            peer_lease_capacity: MAX_PEER_LIVE_LEASES,
            peer_idle_retention_ms: MAX_SESSION_TOKEN_TTL_MS,
            claim_clock: Arc::new(SystemClaimClock::default()),
            successor_policy: Arc::new(NearestInterestSuccessorPolicy),
            authority_metrics: Arc::new(AuthorityMetrics::default()),
            lease_sweep_clock: Arc::new(RegistrarSweepClock),
            handoff_deadline_ms: 300,
        }
    }
}

/// Errors from binding or running the [`GatewayServer`].
#[derive(Debug)]
pub enum GatewayError {
    /// Failed to bind the iroh endpoint.
    Bind(iroh::endpoint::BindError),
    /// Failed to set the bind address.
    BindAddr(String),
}

impl core::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(e) => write!(f, "gateway bind: {e}"),
            Self::BindAddr(s) => write!(f, "gateway bind addr: {s}"),
        }
    }
}

impl core::error::Error for GatewayError {}

#[derive(Clone)]
struct GatewayAdmission {
    authorizer: SharedGatewayAuthorizer,
    clock: SharedGatewayClock,
    health: SharedIdentityHealth,
    claim_clock: SharedClaimClock,
    peers: Arc<PeerRegistry>,
}

#[derive(Clone, PartialEq, Eq)]
struct EstablishedIdentity {
    claims: SessionTokenClaimsV1,
    token: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SessionGeneration(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionLease {
    entity: PersistId,
    lease_id: LeaseId,
    grid: GridId,
    cell: CellId,
    owner: SessionLeaseOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLeaseOwner {
    Active(SessionGeneration),
    Parking(SessionGeneration),
}

/// Renew a session's due leases, one router call per grid.
///
/// A heartbeat arrives every 2.5 s naming every lease the peer holds. Those
/// leases share an *actor* — an actor owns a shard, and a shard holds very
/// many leaf cells — but they do not share a leaf cell: measured on the P2
/// workload, 2079 entities sat in 2079 distinct leaf cells, so grouping here
/// by `(grid, cell)` produced 2079 groups of one and folded nothing.
///
/// The gateway therefore does not group by cell at all. It hands the router
/// every renewal for a grid, each carrying its own cell, and the router folds
/// them by the actor that owns each — shard layout is the router's knowledge,
/// and teaching the gateway to resolve shards would put a copy of the routing
/// table on the wrong side of the [`Router`] boundary and stale it on the
/// first rekey. Grouping by grid is kept because `grid` scopes the cell-id
/// space itself (P-7): two grids are two entity universes, not two groups.
///
/// The ack stays per entity. Every pair that did not renew is named
/// individually in the returned `invalid` list, whether it failed on its own
/// row or because its whole group failed to route, because that list is what
/// stops a holder writing an entity it no longer owns. The current row is
/// still returned even when it refuses the renewal: the holder needs to see
/// who has it now, not merely that it lost it.
async fn renew_session_leases(
    router: &Arc<dyn Router>,
    holder: NodeId,
    renewable: &[SessionLease],
    now_ms: u64,
) -> (Vec<orrery_protocol::Lease>, Vec<(PersistId, LeaseId)>) {
    let mut groups: Vec<(GridId, Vec<&SessionLease>)> = Vec::new();
    for lease in renewable {
        match groups
            .iter_mut()
            .find(|(grouped, _)| *grouped == lease.grid)
        {
            Some((_, members)) => members.push(lease),
            None => groups.push((lease.grid, vec![lease])),
        }
    }
    let mut rows = Vec::with_capacity(renewable.len());
    let mut invalid = Vec::new();
    for (grid, members) in groups {
        let batch: Vec<_> = members
            .iter()
            .map(|lease| LeaseRenewal {
                cell: lease.cell,
                entity: lease.entity,
                lease_id: lease.lease_id,
            })
            .collect();
        let renewed = router.heartbeat_leases(grid, holder, &batch, now_ms).await;
        let Ok(renewed) = renewed else {
            invalid.extend(
                batch
                    .iter()
                    .map(|entry| (entry.entity, entry.lease_id))
                    .collect::<Vec<_>>(),
            );
            continue;
        };
        let mut answered = 0;
        for (lease, row) in members.iter().zip(renewed) {
            answered += 1;
            match row {
                Some(row) => {
                    let current = row.holder == Some(holder)
                        && row.lease_id == lease.lease_id
                        && row.expires_at > now_ms;
                    rows.push(row);
                    if !current {
                        invalid.push((lease.entity, lease.lease_id));
                    }
                }
                None => invalid.push((lease.entity, lease.lease_id)),
            }
        }
        // A router that answers short has said nothing about the tail, and a
        // silent tail is exactly the ack blur batching must not introduce.
        for lease in members.iter().skip(answered) {
            invalid.push((lease.entity, lease.lease_id));
        }
    }
    (rows, invalid)
}

/// Resolve a batched renewal against the session's lease index.
///
/// Returns the rows to renew and the pairs to refuse. `LeaseId` is a *per-row*
/// counter ([`crate::lease`] bumps it on acquire), so a renewal only names one
/// lease when it is paired with its entity: every entity a peer freshly claims
/// carries `LeaseId(1)`. Keying on the entity makes this one map lookup per
/// requested renewal and, downstream, one actor round trip per *held* lease —
/// where filtering the session set by bare id cost O(requested x held) of both,
/// i.e. quadratic in the entities a single peer holds.
fn resolve_renewals(
    leases: &HashMap<PersistId, SessionLease>,
    generation: SessionGeneration,
    renew: &[(PersistId, LeaseId)],
) -> (Vec<SessionLease>, Vec<(PersistId, LeaseId)>) {
    let mut renewable = Vec::with_capacity(renew.len().min(leases.len()));
    let mut invalid = Vec::new();
    let mut seen = HashSet::with_capacity(renew.len());
    for &(entity, lease_id) in renew {
        // A holder that repeats a pair within one batch gets one renewal, not
        // one actor turn per copy; the answer would be identical, and the ack
        // still carries it once.
        if !seen.insert((entity, lease_id)) {
            continue;
        }
        match leases.get(&entity) {
            Some(lease)
                if lease.lease_id == lease_id
                    && lease.owner == SessionLeaseOwner::Active(generation) =>
            {
                renewable.push(*lease);
            }
            // Absent, superseded, or held by a previous generation of this
            // session: refused explicitly so the holder stops writing rather
            // than waiting out its conservative expiry floor.
            _ => invalid.push((entity, lease_id)),
        }
    }
    (renewable, invalid)
}

enum LeaseClaimCompletion {
    Granted,
    Compensate(SessionLease),
    Denied,
}

/// Sends one encoded frame back down a peer's live connection.
///
/// The registrar needs this to reach a peer that did not ask anything: a
/// successor learning it inherited a lease, or a silent holder learning its
/// lease expired. Every other gateway reply is a response to a datagram the
/// peer just sent.
type PeerNotifier = Arc<dyn Fn(Bytes) + Send + Sync>;

struct PeerState {
    established: EstablishedIdentity,
    current: Option<SessionGeneration>,
    live: HashSet<SessionGeneration>,
    notify: Option<(SessionGeneration, PeerNotifier)>,
    leases: HashMap<PersistId, SessionLease>,
    pending_lease_claims: usize,
    lease_capacity: usize,
    idle_since_ms: Option<u64>,
    claim_bucket: ClaimBucket,
}

struct PeerRegistry {
    entries: tokio::sync::Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<PeerState>>>>,
    next_generation: AtomicU64,
    capacity: usize,
    lease_capacity: usize,
    idle_retention_ms: u64,
}

#[derive(Clone)]
struct PeerSession {
    node: NodeId,
    generation: SessionGeneration,
    /// The account this session authenticated as.
    ///
    /// Copied out of the established identity rather than read through the
    /// mutex on every use: the per-account report limit is consulted on the
    /// receive loop, where taking a peer lock would serialize reports behind
    /// whatever lease operation currently holds it. A session's account cannot
    /// change — a token naming a different one activates a new generation.
    account: AccountId,
    state: Arc<tokio::sync::Mutex<PeerState>>,
}

impl PeerSession {
    async fn lock_current(&self) -> Option<tokio::sync::OwnedMutexGuard<PeerState>> {
        let state = Arc::clone(&self.state).lock_owned().await;
        (state.current == Some(self.generation)).then_some(state)
    }

    /// Install this session's send path so the registrar can push to it.
    async fn install_notifier(&self, notify: PeerNotifier) {
        let mut peer = self.state.lock().await;
        if peer.current == Some(self.generation) {
            peer.notify = Some((self.generation, notify));
        }
    }

    /// Push an unsolicited reply, if this session is still the current one.
    async fn notify(&self, reply: &GatewayReply) -> bool {
        let frame = Bytes::from(encode_stream_frame(reply));
        let peer = self.state.lock().await;
        match &peer.notify {
            Some((generation, notify)) if *generation == self.generation => {
                notify(frame);
                true
            }
            _ => false,
        }
    }

    async fn try_reserve_lease_slot(&self) -> bool {
        let Some(mut peer) = self.lock_current().await else {
            return false;
        };
        if peer.leases.len().saturating_add(peer.pending_lease_claims) >= peer.lease_capacity {
            return false;
        }
        peer.pending_lease_claims += 1;
        true
    }

    async fn complete_lease_claim(&self, lease: Option<SessionLease>) -> LeaseClaimCompletion {
        let mut peer = self.state.lock().await;
        let Some(pending_lease_claims) = peer.pending_lease_claims.checked_sub(1) else {
            return LeaseClaimCompletion::Denied;
        };
        peer.pending_lease_claims = pending_lease_claims;
        let Some(lease) = lease else {
            return LeaseClaimCompletion::Denied;
        };
        if peer.current == Some(self.generation) {
            return match peer.leases.get(&lease.entity) {
                Some(indexed) if *indexed == lease => LeaseClaimCompletion::Granted,
                Some(_) => LeaseClaimCompletion::Denied,
                None => {
                    peer.leases.insert(lease.entity, lease);
                    LeaseClaimCompletion::Granted
                }
            };
        }
        let compensation = SessionLease {
            owner: SessionLeaseOwner::Parking(self.generation),
            ..lease
        };
        match peer.leases.get(&lease.entity) {
            Some(indexed) if *indexed == compensation => {
                LeaseClaimCompletion::Compensate(compensation)
            }
            Some(indexed)
                if indexed.entity == lease.entity
                    && indexed.lease_id == lease.lease_id
                    && indexed.grid == lease.grid
                    && indexed.cell == lease.cell
                    && matches!(
                        indexed.owner,
                        SessionLeaseOwner::Active(generation)
                            if Some(generation) == peer.current
                    ) =>
            {
                LeaseClaimCompletion::Denied
            }
            Some(_) => LeaseClaimCompletion::Compensate(compensation),
            None => {
                peer.leases.insert(lease.entity, compensation);
                LeaseClaimCompletion::Compensate(compensation)
            }
        }
    }
}

impl PeerRegistry {
    fn new(capacity: usize, idle_retention_ms: u64, lease_capacity: usize) -> Self {
        Self {
            entries: tokio::sync::Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(0),
            capacity: capacity.min(MAX_PEER_REGISTRY_ENTRIES),
            lease_capacity: lease_capacity.min(MAX_PEER_LIVE_LEASES),
            idle_retention_ms,
        }
    }

    async fn activate(
        &self,
        node: NodeId,
        authorization: GatewayAuthorization,
        token: &[u8],
        retiring: Option<SessionGeneration>,
        now_ms: u64,
        claim_now_ms: u64,
    ) -> Option<PeerSession> {
        self.evict_idle(now_ms).await;
        let valid = matches!(authorization, GatewayAuthorization::Valid(_));
        let state = {
            let mut entries = self.entries.lock().await;
            if let Some(state) = entries.get(&node) {
                Arc::clone(state)
            } else {
                if !valid || entries.len() >= self.capacity {
                    return None;
                }
                let claims = match &authorization {
                    GatewayAuthorization::Valid(claims) => claims.clone(),
                    GatewayAuthorization::Expired(_) => return None,
                };
                let state = Arc::new(tokio::sync::Mutex::new(PeerState {
                    established: EstablishedIdentity {
                        claims,
                        token: token.to_vec(),
                    },
                    current: None,
                    live: HashSet::new(),
                    notify: None,
                    leases: HashMap::new(),
                    pending_lease_claims: 0,
                    lease_capacity: self.lease_capacity,
                    idle_since_ms: Some(now_ms),
                    claim_bucket: ClaimBucket::new(claim_now_ms),
                }));
                entries.insert(node, Arc::clone(&state));
                state
            }
        };
        let mut peer = state.lock().await;
        match authorization {
            GatewayAuthorization::Valid(claims) => {
                peer.established = EstablishedIdentity {
                    claims,
                    token: token.to_vec(),
                };
            }
            GatewayAuthorization::Expired(claims) => {
                let retained = peer.idle_since_ms.is_none_or(|idle_since| {
                    now_ms.saturating_sub(idle_since) <= self.idle_retention_ms
                });
                if !retained || peer.established.claims != claims || peer.established.token != token
                {
                    return None;
                }
            }
        }
        if let Some(retiring) = retiring {
            peer.live.remove(&retiring);
        }
        let generation = SessionGeneration(
            self.next_generation
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    current.checked_add(1)
                })
                .ok()?
                .checked_add(1)?,
        );
        peer.current = Some(generation);
        peer.live.insert(generation);
        peer.idle_since_ms = None;
        for lease in peer.leases.values_mut() {
            if let SessionLeaseOwner::Active(_) = lease.owner {
                lease.owner = SessionLeaseOwner::Active(generation);
            }
        }
        let account = peer.established.claims.account;
        drop(peer);
        Some(PeerSession {
            node,
            generation,
            account,
            state,
        })
    }

    /// A handle to `node`'s current session, if it has one.
    ///
    /// The registrar uses this to reach a peer that is not the one it is
    /// currently serving — the successor of a lost lease, or the loser of a
    /// negotiated handoff.
    async fn current_session(&self, node: NodeId) -> Option<PeerSession> {
        let state = Arc::clone(self.entries.lock().await.get(&node)?);
        let (generation, account) = {
            let peer = state.lock().await;
            (peer.current?, peer.established.claims.account)
        };
        Some(PeerSession {
            node,
            generation,
            account,
            state,
        })
    }

    /// Every peer with a live session, and the leases it currently holds.
    async fn live_peer_leases(&self) -> Vec<(NodeId, Vec<SessionLease>)> {
        let entries = {
            let entries = self.entries.lock().await;
            entries
                .iter()
                .map(|(node, state)| (*node, Arc::clone(state)))
                .collect::<Vec<_>>()
        };
        let mut live = Vec::new();
        for (node, state) in entries {
            let peer = state.lock().await;
            let Some(current) = peer.current else {
                continue;
            };
            live.push((
                node,
                peer.leases
                    .values()
                    .filter(|lease| lease.owner == SessionLeaseOwner::Active(current))
                    .copied()
                    .collect(),
            ));
        }
        live
    }

    async fn evict_idle(&self, now_ms: u64) {
        self.entries.lock().await.retain(|_, state| {
            let Ok(peer) = state.try_lock() else {
                return true;
            };
            let retained = peer.idle_since_ms.is_none_or(|idle_since| {
                now_ms.saturating_sub(idle_since) <= self.idle_retention_ms
            });
            retained || !peer.live.is_empty() || !peer.leases.is_empty() || peer.current.is_some()
        });
    }
}

impl GatewayAdmission {
    async fn authorize(
        &self,
        token: &[u8],
        remote: &NodeId,
        retiring: Option<SessionGeneration>,
    ) -> Option<PeerSession> {
        let now_ms = self.clock.now_ms();
        let authorization = match self.authorizer.authorize(token, remote, now_ms) {
            Ok(valid @ GatewayAuthorization::Valid(_)) => valid,
            Ok(expired @ GatewayAuthorization::Expired(_)) if !self.health.is_available() => {
                expired
            }
            Ok(GatewayAuthorization::Expired(_)) | Err(_) => return None,
        };
        self.peers
            .activate(
                *remote,
                authorization,
                token,
                retiring,
                now_ms.0,
                self.claim_clock.now_ms(),
            )
            .await
    }
}

/// The registrar-side half of authority redistribution.
///
/// Successor selection lives on the gateway, not in the cell actor, because
/// only the gateway knows which peers currently hold authenticated sessions
/// and what the coordinator says they are interested in. The actor stays the
/// single writer of the row; this type only decides *who to offer it to* and
/// then goes through the ordinary serialized claim path to make it so.
///
/// A successor must therefore be reachable on **this** gateway. A peer
/// connected to a sibling gateway is not a candidate — redistribution across
/// gateways needs a cluster-wide session directory, which is later work.
/// A registrar divest request awaiting the holder's answer (D7 §4.2).
///
/// This is the *claimant-triggered* half of cooperative handoff: B explicitly
/// grabs something A holds, and the registrar asks A rather than refusing B
/// outright. Keyed by the entity and its current holder, because that is what
/// the holder's answer names when it arrives on its own connection.
#[derive(Debug, Clone, Copy)]
struct PendingHandoff {
    entity: PersistId,
    grid: GridId,
    cell: CellId,
    holder: NodeId,
    holder_lease_id: LeaseId,
    claimant: NodeId,
    claim_id: orrery_protocol::ClaimId,
    /// The tier the claimant asked for. A grab confers strong ownership, so
    /// granting the successor weak authority would make the object stealable
    /// the instant it changed hands.
    kind: orrery_protocol::ClaimKind,
    /// Whether the holder's authority was strong when the request went out.
    /// This decides what a missed deadline means, and it is read from the
    /// registrar row rather than from the holder's own account of itself.
    strong_held: bool,
}

struct Redistributor {
    peers: Arc<PeerRegistry>,
    interest: SharedInterestAuthority,
    policy: SharedSuccessorPolicy,
    metrics: Arc<AuthorityMetrics>,
    handoff_deadline_ms: u32,
    pending: tokio::sync::Mutex<HashMap<(PersistId, NodeId), PendingHandoff>>,
}

impl Redistributor {
    /// Peers eligible to inherit an entity committed to `cell`.
    async fn candidates(
        &self,
        grid: GridId,
        cell: CellId,
        exclude: NodeId,
        now_ms: u64,
    ) -> Vec<SuccessorCandidate> {
        let mut candidates = Vec::new();
        for (node, leases) in self.peers.live_peer_leases().await {
            if node == exclude {
                continue;
            }
            // Eligibility is the *same* predicate a live claim passes, called
            // through the same seam rather than reimplemented beside it. An
            // earlier version matched an ancestor of the entity's cell here
            // while `allows` required the exact cell, so a peer could be handed
            // a lease it could not have claimed for itself.
            if !self.interest.allows(node, grid, cell, now_ms) {
                continue;
            }
            candidates.push(SuccessorCandidate {
                node,
                held_leases: leases.len(),
                holds_lease_in_cell: leases
                    .iter()
                    .any(|lease| lease.grid == grid && lease.cell == cell),
            });
        }
        candidates
    }

    /// Grant a parked entity to `successor` and tell it, or return `None`
    /// having left the registrar exactly as it found it.
    #[allow(clippy::too_many_arguments)] // One handoff's parameters, all explicit.
    async fn hand_to(
        &self,
        router: &Arc<dyn Router>,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        successor: NodeId,
        prev_holder: Option<NodeId>,
        claim_id: orrery_protocol::ClaimId,
        kind: orrery_protocol::ClaimKind,
    ) -> Option<orrery_protocol::Lease> {
        let session = self.peers.current_session(successor).await?;
        if !session.try_reserve_lease_slot().await {
            return None;
        }
        let granted = match router
            .claim_lease(grid, cell, entity, successor, kind, registrar_now_ms())
            .await
        {
            Ok(crate::lease::ClaimResult::Granted(row)) => row,
            Ok(crate::lease::ClaimResult::Denied(_)) | Err(_) => {
                let _ = session.complete_lease_claim(None).await;
                return None;
            }
        };
        let lease = SessionLease {
            entity,
            lease_id: granted.lease_id,
            grid,
            cell,
            owner: SessionLeaseOwner::Active(session.generation),
        };
        match session.complete_lease_claim(Some(lease)).await {
            LeaseClaimCompletion::Granted => {
                let delivered = session
                    .notify(&GatewayReply::Lease {
                        message: LeaseMsg::Grant {
                            claim_id,
                            entity,
                            lease_id: granted.lease_id,
                            seq: granted.seq,
                            ttl_ms: crate::lease::LEASE_TTL_MS as u32,
                            prev_holder,
                        },
                    })
                    .await;
                if delivered {
                    Some(granted)
                } else {
                    // The successor's session went away between the grant and
                    // the push. Nobody would ever learn it holds this lease,
                    // so undo rather than leave an unreachable holder.
                    self.unwind_grant(router, &session, lease).await;
                    None
                }
            }
            LeaseClaimCompletion::Compensate(compensation) => {
                self.unwind_grant(router, &session, compensation).await;
                None
            }
            LeaseClaimCompletion::Denied => {
                let _ = router
                    .park_lease(grid, cell, entity, successor, granted.lease_id)
                    .await;
                None
            }
        }
    }

    /// Park a grant back out and drop the session index entry it created.
    async fn unwind_grant(
        &self,
        router: &Arc<dyn Router>,
        session: &PeerSession,
        lease: SessionLease,
    ) {
        let parked = router
            .park_lease(
                lease.grid,
                lease.cell,
                lease.entity,
                session.node,
                lease.lease_id,
            )
            .await
            .is_ok();
        if parked {
            let mut peer = session.state.lock().await;
            if peer.leases.get(&lease.entity) == Some(&lease) {
                peer.leases.remove(&lease.entity);
            }
        }
    }

    /// Ask a live holder to give an entity up on a claimant's behalf.
    ///
    /// Returns `false` when there is nobody to ask — no live session, or a
    /// request already outstanding for this holder and entity — in which case
    /// the caller falls back to the ordinary claim path and its refusal.
    /// Deduplication matters: without it, a contested object would fan a
    /// divest request out to its holder once per claimant per tick.
    async fn request_divest(
        self: &Arc<Self>,
        router: &Arc<dyn Router>,
        pending: PendingHandoff,
    ) -> bool {
        let Some(holder_session) = self.peers.current_session(pending.holder).await else {
            return false;
        };
        {
            let mut outstanding = self.pending.lock().await;
            let key = (pending.entity, pending.holder);
            if outstanding.contains_key(&key) {
                return false;
            }
            outstanding.insert(key, pending);
        }
        let delivered = holder_session
            .notify(&GatewayReply::Lease {
                message: LeaseMsg::Divest {
                    entity: pending.entity,
                    lease_id: pending.holder_lease_id,
                    to: Some(pending.claimant),
                    final_seq: orrery_protocol::SeqPair::default(),
                    cursor: None,
                },
            })
            .await;
        if !delivered {
            self.pending
                .lock()
                .await
                .remove(&(pending.entity, pending.holder));
            return false;
        }
        self.metrics.record_divest_requested();

        // The deadline is armed here rather than swept, so an unanswered
        // request resolves on its own schedule instead of waiting for whatever
        // else happens to run.
        let deadline = std::time::Duration::from_millis(u64::from(self.handoff_deadline_ms));
        let router = Arc::clone(router);
        let redistributor = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            redistributor.expire_handoff(&router, pending).await;
        });
        true
    }

    /// Resolve a request the holder never answered (D7 §4.2 deadline rules).
    async fn expire_handoff(&self, router: &Arc<dyn Router>, pending: PendingHandoff) {
        let key = (pending.entity, pending.holder);
        if self.pending.lock().await.remove(&key).is_none() {
            // The holder answered in time; its reply already resolved this.
            return;
        }
        self.metrics.record_handoff_timed_out();

        if pending.strong_held {
            // Stealing by timeout is exactly what "not stealable" forbids.
            // Only expiry — a crash — breaks strong ownership.
            self.answer_claimant(
                pending.claimant,
                LeaseMsg::Deny {
                    claim_id: Some(pending.claim_id),
                    entity: pending.entity,
                    reason: orrery_protocol::DenyReason::StrongHeld,
                    retry_after_ms: 0,
                },
            )
            .await;
            return;
        }

        // Weak authority converts to unconditional divestiture: an
        // interaction must not stall on an unresponsive peer. The successor
        // inherits last-committed state, which is what redistribution gives it
        // anyway — there is no cursor to gate on, because the holder never
        // answered.
        let parked = router
            .park_lease(
                pending.grid,
                pending.cell,
                pending.entity,
                pending.holder,
                pending.holder_lease_id,
            )
            .await;
        let unconditional =
            matches!(&parked, Ok(Some(row)) if parked_by_us(row, pending.holder_lease_id));
        let handed = unconditional
            && self
                .hand_to(
                    router,
                    pending.grid,
                    pending.cell,
                    pending.entity,
                    pending.claimant,
                    Some(pending.holder),
                    pending.claim_id,
                    pending.kind,
                )
                .await
                .is_some();
        if handed {
            self.metrics.record_reassigned();
            // Tell the silent holder its lease ended, addressed by the token
            // it still believes it holds.
            if let Some(session) = self.peers.current_session(pending.holder).await {
                session
                    .notify(&GatewayReply::Lease {
                        message: LeaseMsg::Expire {
                            entity: pending.entity,
                            lease_id: pending.holder_lease_id,
                            last_holder: Some(pending.holder),
                            reason: orrery_protocol::ExpireReason::Revoked,
                            disposition: orrery_protocol::ExpireDisposition::Reassigned {
                                to: pending.claimant,
                            },
                        },
                    })
                    .await;
            }
        } else {
            self.answer_claimant(
                pending.claimant,
                LeaseMsg::Deny {
                    claim_id: Some(pending.claim_id),
                    entity: pending.entity,
                    reason: orrery_protocol::DenyReason::NotEligible,
                    retry_after_ms: 0,
                },
            )
            .await;
        }
    }

    /// Take the request outstanding for this holder and entity, if any.
    async fn take_pending(&self, entity: PersistId, holder: NodeId) -> Option<PendingHandoff> {
        self.pending.lock().await.remove(&(entity, holder))
    }

    /// Push a control reply to a claimant that is waiting on one.
    async fn answer_claimant(&self, claimant: NodeId, message: LeaseMsg) {
        if let Some(session) = self.peers.current_session(claimant).await {
            session.notify(&GatewayReply::Lease { message }).await;
        }
    }

    /// Choose where a lost lease goes, put it there, and tell the loser.
    async fn redistribute(&self, router: &Arc<dyn Router>, parked: crate::lease::ParkedLease) {
        let entity = parked.lease.entity;
        let disposition = self.place(router, &parked).await;
        match &disposition {
            orrery_protocol::ExpireDisposition::Reassigned { .. } => {
                self.metrics.record_reassigned()
            }
            _ => self.metrics.record_parked_without_successor(),
        }
        // Tell the losing holder, addressed by the token it still believes it
        // has installed — parking already bumped the row's own `lease_id`
        // past it. On a disconnect there is nobody left to tell; on a TTL
        // sweep this is what stops a silent zombie from writing again.
        if let Some(session) = self.peers.current_session(parked.previous_holder).await {
            session
                .notify(&GatewayReply::Lease {
                    message: LeaseMsg::Expire {
                        entity,
                        lease_id: parked.previous_lease_id,
                        last_holder: Some(parked.previous_holder),
                        reason: parked.reason,
                        disposition,
                    },
                })
                .await;
        }
    }

    /// Decide and enact the disposition of one parked row.
    async fn place(
        &self,
        router: &Arc<dyn Router>,
        parked: &crate::lease::ParkedLease,
    ) -> orrery_protocol::ExpireDisposition {
        // D7 §5: a strong-owned entity whose owner crashed re-parks with its
        // `own_seq` intact rather than being regranted. Only weak authority
        // is redistributed without consent.
        //
        // A player-bound entity is excluded even more firmly (D7 §4.3): a
        // character parks and is exclusively reclaimable by the account that
        // owns it, so it is never offered to anyone, at any tier.
        if parked
            .lease
            .flags
            .contains(orrery_protocol::LeaseFlags::STRONG_HELD)
            || parked
                .lease
                .flags
                .contains(orrery_protocol::LeaseFlags::PLAYER_BOUND)
        {
            return orrery_protocol::ExpireDisposition::Parked;
        }
        let candidates = self
            .candidates(
                parked.grid,
                parked.cell,
                parked.previous_holder,
                registrar_now_ms(),
            )
            .await;
        if candidates.is_empty() {
            return orrery_protocol::ExpireDisposition::Parked;
        }
        let chosen = self.policy.select(&SuccessorRequest {
            grid: parked.grid,
            cell: parked.cell,
            entity: parked.lease.entity,
            previous_holder: parked.previous_holder,
            reason: parked.reason,
            candidates: &candidates,
        });
        // A policy may only pick from the vetted set; anything else is
        // ignored rather than granted to an unchecked peer.
        let Some(successor) = chosen.filter(|node| {
            *node != parked.previous_holder
                && candidates.iter().any(|candidate| candidate.node == *node)
        }) else {
            return orrery_protocol::ExpireDisposition::Parked;
        };
        if self
            .hand_to(
                router,
                parked.grid,
                parked.cell,
                parked.lease.entity,
                successor,
                Some(parked.previous_holder),
                orrery_protocol::ClaimId::REGISTRAR,
                orrery_protocol::ClaimKind::Weak,
            )
            .await
            .is_some()
        {
            orrery_protocol::ExpireDisposition::Reassigned { to: successor }
        } else {
            orrery_protocol::ExpireDisposition::Parked
        }
    }
}

/// A running gateway: an iroh endpoint that accepts client sessions and routes
/// them onto a [`Router`] (a single runtime or a test cluster harness).
pub struct GatewayServer {
    endpoint: Arc<Endpoint>,
    interest_authority: SharedInterestAuthority,
    authority_metrics: Arc<AuthorityMetrics>,
    metrics: Arc<GatewayMetrics>,
    send_failures: Arc<AtomicU64>,
    shutdown: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

impl GatewayServer {
    /// Bind an iroh endpoint from `config` and spawn the accept loop against
    /// `router`.
    pub async fn spawn(
        config: GatewayConfig,
        router: Arc<dyn Router>,
    ) -> Result<Self, GatewayError> {
        let mut builder = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0);
        builder = builder
            .bind_addr(config.bind)
            .map_err(|e| GatewayError::BindAddr(e.to_string()))?;
        builder = builder.alpns(vec![config.alpn.clone()]);
        builder = builder.relay_mode(config.relay_mode.clone());
        if let Some(key) = &config.secret_key {
            builder = builder.secret_key(key.clone());
        }
        let endpoint = Arc::new(builder.bind().await.map_err(GatewayError::Bind)?);

        let gateway = endpoint.id();
        let protocol = config.protocol_version;
        let executor = config.executor;
        let adjudicator = config.adjudicator;
        let validator = config.validator;
        let bulk_ack_admission = config.bulk_ack_admission;
        let interest_authority = config.interest_authority;
        let admission = GatewayAdmission {
            authorizer: config.authorizer,
            clock: config.identity_clock,
            health: config.identity_health,
            claim_clock: config.claim_clock,
            peers: Arc::new(PeerRegistry::new(
                config.peer_registry_capacity,
                config.peer_idle_retention_ms,
                config.peer_lease_capacity,
            )),
        };
        let metrics = Arc::clone(&config.metrics);
        let authority_metrics = Arc::clone(&config.authority_metrics);
        let redistributor = Arc::new(Redistributor {
            peers: Arc::clone(&admission.peers),
            interest: Arc::clone(&interest_authority),
            policy: config.successor_policy,
            metrics: Arc::clone(&authority_metrics),
            handoff_deadline_ms: config.handoff_deadline_ms,
            pending: tokio::sync::Mutex::new(HashMap::new()),
        });
        let lease_sweep_clock = config.lease_sweep_clock;
        // One limiter per gateway, not per connection: the limit is per
        // account (docs/07 §7) and an account may hold several connections.
        let report_limiter = Arc::new(ReportLimiter::new());
        let (shutdown, rx) = oneshot::channel();
        let send_failures = Arc::new(AtomicU64::new(0));
        let join = tokio::spawn(accept_loop(
            endpoint.clone(),
            router,
            gateway,
            protocol,
            executor,
            adjudicator,
            validator,
            bulk_ack_admission,
            Arc::clone(&interest_authority),
            admission,
            report_limiter,
            Arc::clone(&metrics),
            redistributor,
            lease_sweep_clock,
            Arc::clone(&send_failures),
            rx,
        ));
        Ok(Self {
            endpoint,
            interest_authority,
            authority_metrics,
            metrics,
            send_failures,
            shutdown,
            join,
        })
    }

    /// The gateway's own node id (transport identity, D3).
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.endpoint.id()
    }

    /// The gateway's addressing info (id + direct/relay addresses), for a
    /// client to dial.
    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// The coordinator-owned interest authority supplied at gateway startup.
    ///
    /// This read-only seam is retained for coordinator integration; gateway
    /// client messages do not update it.
    #[must_use]
    pub fn interest_authority(&self) -> &dyn InterestAuthority {
        self.interest_authority.as_ref()
    }

    /// The always-on authority telemetry, including the single-writer
    /// invariant checker (D7 §5). `duplicate_authority` must stay at zero.
    #[must_use]
    pub fn authority_metrics(&self) -> &Arc<AuthorityMetrics> {
        &self.authority_metrics
    }

    /// The always-on server-side telemetry: bulk stages, the intent and area
    /// server spans, and the report outcome split. Collected whether or not a
    /// metrics sink was configured.
    #[must_use]
    pub fn metrics(&self) -> &Arc<GatewayMetrics> {
        &self.metrics
    }

    /// The number of reply datagram sends that failed since startup (e.g. an
    /// oversize frame rejected by QUIC). Every failure is also logged with the
    /// remote and the byte length; this counter is the always-on signal that a
    /// page exceeded the datagram budget or the connection tore mid-send
    /// (docs/08-persistence.md §9).
    #[must_use]
    pub fn area_page_send_failures(&self) -> u64 {
        self.send_failures.load(Ordering::Relaxed)
    }

    /// Stop the accept loop and close the endpoint, awaiting the accept task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.endpoint.close().await;
        let _ = self.join.await;
    }
}

/// Accept client connections forever, spawning one handler task per connection,
/// until `shutdown` resolves or the endpoint closes.
#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    endpoint: Arc<Endpoint>,
    router: Arc<dyn Router>,
    gateway: NodeId,
    protocol: u16,
    executor: Option<SharedExecutor>,
    adjudicator: Option<SharedAdjudicator>,
    validator: SharedValidator,
    bulk_ack_admission: SharedBulkAckAdmission,
    interest_authority: SharedInterestAuthority,
    admission: GatewayAdmission,
    report_limiter: Arc<ReportLimiter>,
    metrics: Arc<GatewayMetrics>,
    redistributor: Arc<Redistributor>,
    lease_sweep_clock: SharedClaimClock,
    send_failures: Arc<AtomicU64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                // Expiry and redistribution are one step: a swept row that is
                // parked and then left is exactly the orphan the phase exists
                // to eliminate.
                for parked in router.sweep_expired_leases(lease_sweep_clock.now_ms()).await {
                    redistributor.redistribute(&router, parked).await;
                }
                admission.peers.evict_idle(admission.clock.now_ms().0).await;
                interest_authority.prune_expired(registrar_now_ms());
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let router = Arc::clone(&router);
                let executor = executor.clone();
                let adjudicator = adjudicator.clone();
                let validator = Arc::clone(&validator);
                let bulk_ack_admission = Arc::clone(&bulk_ack_admission);
                let interest_authority = Arc::clone(&interest_authority);
                let admission = admission.clone();
                let report_limiter = Arc::clone(&report_limiter);
                let metrics = Arc::clone(&metrics);
                let redistributor = Arc::clone(&redistributor);
                let send_failures = Arc::clone(&send_failures);
                tokio::spawn(handle_connection(
                    incoming,
                    router,
                    gateway,
                    protocol,
                    executor,
                    adjudicator,
                    validator,
                    bulk_ack_admission,
                    interest_authority,
                    admission,
                    report_limiter,
                    metrics,
                    redistributor,
                    send_failures,
                ));
            }
        }
    }
}

/// Drive one client session: complete the iroh handshake, send the admission
/// uni-stream, then read tagged datagrams and route each [`GatewayMsg`].
///
/// Each decoded message is routed on its own spawned task (bounded by
/// separate bulk and control limits per connection) so a slow 27-cell
/// subscribe — FDB cold scans — never head-of-line blocks diffs on the same
/// connection: the bulk ack is the client-observed durability contract
/// (docs/08-persistence.md §2.1, D16 p99 < 5 ms) and must not queue behind an
/// area load.
#[allow(clippy::too_many_arguments)] // Connection dependencies are explicit at this boundary.
async fn handle_connection(
    incoming: iroh::endpoint::Incoming,
    router: Arc<dyn Router>,
    gateway: NodeId,
    protocol: u16,
    executor: Option<SharedExecutor>,
    adjudicator: Option<SharedAdjudicator>,
    validator: SharedValidator,
    bulk_ack_admission: SharedBulkAckAdmission,
    interest_authority: SharedInterestAuthority,
    admission: GatewayAdmission,
    report_limiter: Arc<ReportLimiter>,
    metrics: Arc<GatewayMetrics>,
    redistributor: Arc<Redistributor>,
    send_failures: Arc<AtomicU64>,
) {
    let conn = match incoming.accept() {
        Ok(accepting) => match accepting.await {
            Ok(conn) => conn,
            Err(e) => {
                debug!(?e, "gateway: connection handshake failed");
                return;
            }
        },
        Err(e) => {
            debug!(?e, "gateway: accept failed");
            return;
        }
    };
    let conn = Arc::new(conn);
    let remote = conn.remote_id();

    // Admission: mirror `aeronet_iroh`'s server side, which streams [ACCEPTED]
    // on a uni stream before any datagrams flow.
    if let Err(e) = send_admission(&conn).await {
        debug!(?e, %remote, "gateway: admission failed");
        return;
    }

    // The reliable lanes are opened lazily on first use, so a connection that
    // only ever uplinks diffs costs the peer no stream. Starting the receiver
    // *after* `send_admission` matters: it accepts every inbound uni-stream
    // from the moment it runs, and the client's own admission read would
    // otherwise be racing it.
    let reliable = reliable::spawn(Arc::clone(&conn), remote, Arc::clone(&send_failures));
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    reliable::spawn_receiver(Arc::clone(&conn), remote, inbound_tx.clone());
    spawn_datagram_reader(Arc::clone(&conn), remote, inbound_tx);

    // One `send` for every reply path, routing on the tag the payload already
    // carries: state replies (bulk acks and nacks) stay on datagrams, where a
    // stale ack is worth less than a timely one, and every control reply rides
    // the ordered reliable lane. Callers do not choose a lane — the channel
    // policy (D3) does, in one place.
    let send: Arc<dyn Fn(Bytes) + Send + Sync> = {
        let conn = Arc::clone(&conn);
        let reliable = reliable.clone();
        let send_failures = Arc::clone(&send_failures);
        Arc::new(move |bytes: Bytes| {
            if matches!(untag(&bytes), Some((Channel::Control, _))) {
                reliable.send(reliable::Lane::Control, bytes);
                return;
            }
            let len = bytes.len();
            if let Err(e) = conn.send_datagram(bytes) {
                // Never swallow a failed send: an oversize payload or a torn
                // connection is counted and logged, not silently dropped.
                send_failures.fetch_add(1, Ordering::Relaxed);
                warn!(?e, %remote, len, "gateway: reply datagram send failed");
            }
        })
    };
    // Area pages take the second reliable lane, so a 27-cell page-in never
    // sits in front of an intent ack the D16 table budgets at p99 < 10 ms.
    let send_area: Arc<dyn Fn(Bytes) + Send + Sync> = {
        let reliable = reliable.clone();
        Arc::new(move |bytes: Bytes| reliable.send(reliable::Lane::Area, bytes))
    };
    let inflight_diffs = Arc::new(Semaphore::new(MAX_INFLIGHT_DIFF_ROUTES_PER_CONN));
    let inflight_control = Arc::new(Semaphore::new(MAX_INFLIGHT_CONTROL_ROUTES_PER_CONN));
    let inflight_intents = Arc::new(Semaphore::new(MAX_INFLIGHT_INTENT_ROUTES_PER_CONN));
    let mut session: Option<PeerSession> = None;

    // Both lanes feed one queue, so the dispatch below is written once and does
    // not care which lane a message arrived on. The channel closes when both
    // feeder tasks have ended, which is how a torn connection ends this loop.
    loop {
        let Some(pkt) = inbound_rx.recv().await else {
            debug!(%remote, "gateway: connection closed");
            break;
        };
        let Some((channel, _)) = untag(&pkt) else {
            continue;
        };
        // A control payload is accepted from either lane. Clients send it on
        // the stream lane now, but the encoding is lane-independent and a
        // receiver that insisted on the lane would reject a well-formed
        // message for a reason the sender cannot see.
        let msg: Option<GatewayMsg> = match channel {
            Channel::State => decode_datagram(&pkt),
            Channel::Control => decode_stream_frame(&pkt),
        };
        let Some(msg) = msg else {
            debug!(%remote, "gateway: undecodable message");
            continue;
        };
        // A versioned bootstrap is an ordinary one once its version is inside
        // the rolling-upgrade window, so the two share the admission path
        // below rather than duplicating it. Enforcement is confined to the
        // versioned form on purpose: the unversioned `Hello` stays accepted
        // unchecked, so this gateway does not cut off a peer that has not
        // adopted the new variant, and version checking is opt-in until
        // `Hello` is retired.
        let msg = match msg {
            GatewayMsg::VersionedHello {
                token,
                node,
                version,
            } => {
                if !GatewayMsg::protocol_accepted(protocol, version) {
                    warn!(
                        %remote,
                        version, protocol,
                        "gateway: refused a client outside the accepted protocol window"
                    );
                    let reply = GatewayReply::HelloRefused {
                        gateway,
                        protocol,
                        reason: GatewayReply::HELLO_REFUSED_PROTOCOL,
                    };
                    send(Bytes::from(encode_stream_frame(&reply)));
                    continue;
                }
                GatewayMsg::Hello { token, node }
            }
            other => other,
        };
        match msg {
            GatewayMsg::Hello { token, node } => {
                // The iroh transport identity is the authority identity; a
                // claimed wire NodeId must never be allowed to substitute it.
                let retiring = session.as_ref().map(|session| session.generation);
                let authorized = if node == remote {
                    admission.authorize(&token, &remote, retiring).await
                } else {
                    None
                };
                if let Some(authorized) = authorized {
                    // Install the push path before acknowledging: from here on
                    // the registrar can reach this peer unprompted, which is
                    // what makes reassignment and expiry visible to it.
                    authorized.install_notifier(Arc::clone(&send)).await;
                    session = Some(authorized);
                    let reply = GatewayReply::HelloAck { gateway, protocol };
                    send(Bytes::from(encode_stream_frame(&reply)));
                } else {
                    if let Some(retiring) = session.take() {
                        cleanup_peer_session(
                            &retiring,
                            &router,
                            &redistributor,
                            admission.clock.now_ms().0,
                        )
                        .await;
                    }
                    if node != remote {
                        warn!(%remote, "gateway: Hello node did not match transport identity");
                    }
                }
            }
            // Normalized into `GatewayMsg::Hello` above. Spelled out rather
            // than folded into a wildcard arm, so a future variant still fails
            // to compile here instead of being silently dropped.
            GatewayMsg::VersionedHello { .. } => {}
            GatewayMsg::Lease { message } => {
                let Some(active_session) = session.as_ref() else {
                    continue;
                };
                match message {
                    LeaseMsg::Claim {
                        claim_id,
                        entity,
                        grid,
                        cell,
                        kind,
                        basis,
                        ..
                    } => {
                        let Some(mut peer) = active_session.lock_current().await else {
                            send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                                message: LeaseMsg::Deny {
                                    claim_id: Some(claim_id),
                                    entity,
                                    reason: orrery_protocol::DenyReason::NotEligible,
                                    retry_after_ms: 0,
                                },
                            })));
                            continue;
                        };
                        let claim_now_ms = admission.claim_clock.now_ms();
                        if !peer.claim_bucket.take(claim_now_ms) {
                            let retry_after_ms = peer.claim_bucket.retry_after_ms();
                            send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                                message: LeaseMsg::Deny {
                                    claim_id: Some(claim_id),
                                    entity,
                                    reason: orrery_protocol::DenyReason::RateLimited,
                                    retry_after_ms,
                                },
                            })));
                            continue;
                        }
                        drop(peer);
                        if !active_session.try_reserve_lease_slot().await {
                            send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                                message: LeaseMsg::Deny {
                                    claim_id: Some(claim_id),
                                    entity,
                                    reason: orrery_protocol::DenyReason::NotEligible,
                                    retry_after_ms: 0,
                                },
                            })));
                            continue;
                        }
                        let now_ms = registrar_now_ms();
                        let player_basis = matches!(
                            basis,
                            orrery_protocol::ClaimBasis::Contact { .. }
                                | orrery_protocol::ClaimBasis::Explicit
                        );
                        let committed_cell = if player_basis {
                            router
                                .committed_entity_cell(grid, entity)
                                .await
                                .ok()
                                .flatten()
                        } else {
                            None
                        };
                        let plausible = committed_cell.is_some_and(|resolved| {
                            resolved == cell
                                && match kind {
                                    orrery_protocol::ClaimKind::Weak => {
                                        interest_authority.allows(remote, grid, resolved, now_ms)
                                    }
                                    orrery_protocol::ClaimKind::Strong => true,
                                }
                        });
                        // A strong claim on something another peer is
                        // actively holding is a *request*, not a refusal
                        // (D7 §4.2): the registrar asks the holder to divest
                        // rather than telling the claimant no. The reply then
                        // arrives on the holder's answer or on the deadline,
                        // so nothing is sent here.
                        let contested = if plausible
                            && matches!(kind, orrery_protocol::ClaimKind::Strong)
                        {
                            match router.inspect_lease(grid, entity).await {
                                Ok((Some(row), Some(committed_cell), _)) => row
                                    .holder
                                    .filter(|holder| *holder != remote && row.expires_at > now_ms)
                                    .map(|holder| PendingHandoff {
                                        entity,
                                        grid,
                                        cell: committed_cell,
                                        holder,
                                        holder_lease_id: row.lease_id,
                                        claimant: remote,
                                        claim_id,
                                        kind,
                                        strong_held: row
                                            .flags
                                            .contains(orrery_protocol::LeaseFlags::STRONG_HELD),
                                    }),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        if let Some(pending) = contested {
                            if redistributor.request_divest(&router, pending).await {
                                // The reservation is released now and retaken
                                // when the handoff lands, so a peer waiting on
                                // a deadline does not sit on lease capacity.
                                let _ = active_session.complete_lease_claim(None).await;
                                continue;
                            }
                        }
                        let outcome = if plausible {
                            router
                                .claim_lease(grid, cell, entity, remote, kind, now_ms)
                                .await
                        } else {
                            Ok(crate::lease::ClaimResult::Denied(
                                orrery_protocol::DenyReason::NotEligible,
                            ))
                        };
                        let message = match outcome {
                            Ok(crate::lease::ClaimResult::Granted(row)) => {
                                let lease = SessionLease {
                                    entity,
                                    lease_id: row.lease_id,
                                    grid,
                                    cell,
                                    owner: SessionLeaseOwner::Active(active_session.generation),
                                };
                                match active_session.complete_lease_claim(Some(lease)).await {
                                    LeaseClaimCompletion::Granted => LeaseMsg::Grant {
                                        claim_id,
                                        entity,
                                        lease_id: row.lease_id,
                                        seq: row.seq,
                                        ttl_ms: crate::lease::LEASE_TTL_MS as u32,
                                        prev_holder: None,
                                    },
                                    LeaseClaimCompletion::Compensate(compensation) => {
                                        let parked = router
                                            .park_lease(grid, cell, entity, remote, row.lease_id)
                                            .await
                                            .is_ok();
                                        if parked {
                                            let mut peer = active_session.state.lock().await;
                                            if peer.leases.get(&compensation.entity)
                                                == Some(&compensation)
                                            {
                                                peer.leases.remove(&compensation.entity);
                                            }
                                        }
                                        LeaseMsg::Deny {
                                            claim_id: Some(claim_id),
                                            entity,
                                            reason: orrery_protocol::DenyReason::NotEligible,
                                            retry_after_ms: 0,
                                        }
                                    }
                                    LeaseClaimCompletion::Denied => LeaseMsg::Deny {
                                        claim_id: Some(claim_id),
                                        entity,
                                        reason: orrery_protocol::DenyReason::NotEligible,
                                        retry_after_ms: 0,
                                    },
                                }
                            }
                            Ok(crate::lease::ClaimResult::Denied(reason)) => {
                                let _ = active_session.complete_lease_claim(None).await;
                                // A herd loser is refused for a bounded reason,
                                // so tell it when coming back is worth anything
                                // rather than leaving it to spin or to give up.
                                let retry_after_ms = crate::lease::retry_after_ms(&reason);
                                LeaseMsg::Deny {
                                    claim_id: Some(claim_id),
                                    entity,
                                    reason,
                                    retry_after_ms,
                                }
                            }
                            Err(_) => {
                                let _ = active_session.complete_lease_claim(None).await;
                                LeaseMsg::Deny {
                                    claim_id: Some(claim_id),
                                    entity,
                                    reason: orrery_protocol::DenyReason::NotEligible,
                                    retry_after_ms: 100,
                                }
                            }
                        };
                        send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                            message,
                        })));
                    }
                    LeaseMsg::Heartbeat { renew, .. } => {
                        let Some(peer) = active_session.lock_current().await else {
                            send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                                message: LeaseMsg::HeartbeatAck {
                                    leases: Vec::new(),
                                    invalid: renew,
                                },
                            })));
                            continue;
                        };
                        let (renewable, mut invalid) =
                            resolve_renewals(&peer.leases, active_session.generation, &renew);
                        drop(peer);
                        let (rows, refused) =
                            renew_session_leases(&router, remote, &renewable, registrar_now_ms())
                                .await;
                        let mut rows = rows;
                        invalid.extend(refused);
                        if active_session.lock_current().await.is_none() {
                            rows.clear();
                            invalid = renew;
                        }
                        send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                            message: LeaseMsg::HeartbeatAck {
                                leases: rows,
                                invalid,
                            },
                        })));
                    }
                    LeaseMsg::Divest {
                        entity,
                        lease_id,
                        to,
                        final_seq,
                        cursor,
                    } => {
                        let message = divest_lease(
                            active_session,
                            &router,
                            &redistributor,
                            admission.claim_clock.now_ms(),
                            DivestRequest {
                                entity,
                                lease_id,
                                to,
                                final_seq,
                                cursor,
                            },
                        )
                        .await;
                        send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                            message,
                        })));
                    }
                    LeaseMsg::Rekey { entity, .. } => {
                        send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                            message: LeaseMsg::Deny {
                                claim_id: None,
                                entity,
                                reason: orrery_protocol::DenyReason::NotEligible,
                                retry_after_ms: 0,
                            },
                        })));
                    }
                    _ => {}
                }
            }
            GatewayMsg::InterestGrant { grant } => {
                // A grant is self-authenticating — it is signed by the
                // coordinator and names its peer — but it is still only
                // accepted inside an established session, so no gateway state
                // exists for a peer that has not passed admission.
                if session.as_ref().is_none() {
                    continue;
                }
                let outcome = interest_authority.apply_grant(&grant, &remote, registrar_now_ms());
                if let Err(error) = &outcome {
                    debug!(%remote, %error, "gateway: interest grant refused");
                }
                let reply = match outcome {
                    Ok(epoch) => GatewayReply::InterestAck {
                        epoch: Some(epoch),
                        reason: orrery_protocol::INTEREST_ACK_OK,
                    },
                    Err(error) => GatewayReply::InterestAck {
                        epoch: None,
                        reason: interest_ack_reason(error),
                    },
                };
                send(Bytes::from(encode_stream_frame(&reply)));
            }
            GatewayMsg::Diff { diff } => {
                if diff.kind == orrery_protocol::RecordKind::Rekey {
                    send(Bytes::from(encode_datagram(&GatewayReply::BulkNack {
                        entity: diff.entity,
                        tick: diff.tick,
                        reason: 2,
                        lease: None,
                    })));
                    continue;
                }
                // Persistent state is never admitted until the peer has
                // bound its claimed NodeId to the iroh-authenticated identity.
                let Some(active_session) = session.clone() else {
                    send(Bytes::from(encode_datagram(&GatewayReply::BulkNack {
                        entity: diff.entity,
                        tick: diff.tick,
                        reason: 2,
                        lease: None,
                    })));
                    continue;
                };
                let received_at = Instant::now();
                let send = Arc::clone(&send);
                let router = Arc::clone(&router);
                let bulk_ack_admission = Arc::clone(&bulk_ack_admission);
                let metrics = Arc::clone(&metrics);
                let authority_metrics = Arc::clone(&redistributor.metrics);
                let permit = Arc::clone(&inflight_diffs).acquire_owned().await;
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_session_diff(
                                send.as_ref(),
                                diff,
                                &active_session,
                                &router,
                                &bulk_ack_admission,
                                &metrics.bulk,
                                authority_metrics,
                                received_at,
                            )
                            .await;
                        });
                    }
                    Err(_) => {
                        route_session_diff(
                            send.as_ref(),
                            diff,
                            &active_session,
                            &router,
                            &bulk_ack_admission,
                            &metrics.bulk,
                            authority_metrics,
                            received_at,
                        )
                        .await
                    }
                }
            }
            GatewayMsg::Subscribe { grid, cells } => {
                // Stamped before the permit wait, like the diff arm above: the
                // D16 span a client observes starts when the request lands,
                // and a subscribe that queues behind 27 cold scans has spent
                // that time whether or not this task was running.
                let received_at = Instant::now();
                // Pages answer on the area lane, not the control lane.
                let send = Arc::clone(&send_area);
                let router = Arc::clone(&router);
                let metrics = Arc::clone(&metrics);
                let permit = Arc::clone(&inflight_control).acquire_owned().await;
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_subscribe(
                                send.as_ref(),
                                grid,
                                cells,
                                remote,
                                &router,
                                &metrics.area,
                                received_at,
                            )
                            .await;
                        });
                    }
                    Err(_) => {
                        route_subscribe(
                            send.as_ref(),
                            grid,
                            cells,
                            remote,
                            &router,
                            &metrics.area,
                            received_at,
                        )
                        .await;
                    }
                }
            }
            GatewayMsg::SubmitIntent { intent } => {
                // Keep signature/identity/admission checks at the edge, then
                // route the potentially slow FDB transaction on its own
                // bounded lane. In particular, never await a semaphore here:
                // waiting would recreate the receive-loop HOL blocking this
                // lane is intended to prevent.
                // Receipt, before the first edge check: an intent refused
                // for a bad signature is measured over the same span as one
                // that commits, so the histogram cannot be flattered by
                // counting only the cheap exits.
                let received_at = Instant::now();
                // The session is what an account-scoped admission check has to
                // authorize against; an intent submitted before `Hello` is
                // answered simply carries no account (`IntentContext`).
                let cx = IntentContext {
                    issuer: remote,
                    account: session.as_ref().map(|session| session.account),
                };
                if let Err(outcome) = admit_intent(&intent, validator.as_ref(), &cx) {
                    send_intent_reply(
                        send.as_ref(),
                        intent.intent_id,
                        outcome,
                        &metrics.intent,
                        received_at,
                    );
                    continue;
                }
                match reserve_intent_lane(Arc::clone(&inflight_intents)) {
                    Ok(permit) => {
                        let send = Arc::clone(&send);
                        let executor = executor.clone();
                        let metrics = Arc::clone(&metrics);
                        tokio::spawn(async move {
                            let _permit = permit;
                            execute_admitted_intent(
                                send.as_ref(),
                                intent,
                                &executor,
                                &metrics.intent,
                                received_at,
                            )
                            .await;
                        });
                    }
                    Err(outcome) => {
                        // There is deliberately no deferred task waiting for
                        // capacity. The client receives a definitive outcome
                        // and may submit a new, idempotently keyed intent on
                        // its normal retry policy.
                        warn!(%remote, intent_id = intent.intent_id, "gateway: intent lane saturated");
                        metrics.intent.record_lane_saturated();
                        send_intent_reply(
                            send.as_ref(),
                            intent.intent_id,
                            outcome,
                            &metrics.intent,
                            received_at,
                        );
                    }
                }
            }
            GatewayMsg::Report { report } => {
                // Edge checks first, on the receive loop: both are constant
                // time and both decide whether the expensive part runs at all.
                if report.reporter != remote {
                    warn!(%remote, "gateway: report filed in another peer's name");
                    send_report_refusal(
                        send.as_ref(),
                        &report,
                        REPORT_REFUSED_REPORTER_MISMATCH,
                        &metrics.report,
                    );
                    continue;
                }
                let Some(account) = session.as_ref().map(|session| session.account) else {
                    // No session, no account, nothing to bill the report to.
                    send_report_refusal(
                        send.as_ref(),
                        &report,
                        REPORT_REFUSED_NO_SESSION,
                        &metrics.report,
                    );
                    continue;
                };
                if !report_limiter
                    .admit(account, admission.claim_clock.now_ms())
                    .await
                {
                    send_report_refusal(
                        send.as_ref(),
                        &report,
                        REPORT_REFUSED_RATE_LIMITED,
                        &metrics.report,
                    );
                    continue;
                }
                let Some(adjudicator) = adjudicator.clone() else {
                    send_report_refusal(
                        send.as_ref(),
                        &report,
                        REPORT_REFUSED_NO_ADJUDICATOR,
                        &metrics.report,
                    );
                    continue;
                };
                // Replay is CPU-bound — a full 180-tick single-entity window
                // is budgeted at < 5 ms (docs/07 §7) — so it goes to the
                // blocking pool rather than occupying a runtime worker that
                // owes other connections their bulk acks.
                let send = Arc::clone(&send);
                let metrics = Arc::clone(&metrics);
                let permit = Arc::clone(&inflight_control).acquire_owned().await;
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    adjudicate_report(send.as_ref(), &adjudicator, &report, &metrics.report);
                });
            }
        }
    }
    if let Some(session) = session {
        cleanup_peer_session(
            &session,
            &router,
            &redistributor,
            admission.clock.now_ms().0,
        )
        .await;
    }
}

/// Whether `row` is the result of *this* caller's park of `lease_id`.
///
/// `park_lease` returns the live row unchanged when the presented holder or
/// token no longer matches, so a returned row is not by itself evidence that
/// anything was parked. Treating one as evidence would redistribute a lease
/// that is legitimately held by somebody else.
fn parked_by_us(row: &orrery_protocol::Lease, lease_id: LeaseId) -> bool {
    row.holder.is_none() && row.lease_id > lease_id
}

/// One holder-initiated divestiture, as it arrives on the wire.
struct DivestRequest {
    entity: PersistId,
    lease_id: LeaseId,
    to: Option<NodeId>,
    final_seq: orrery_protocol::SeqPair,
    cursor: Option<Lsn>,
}

fn divest_denied(entity: PersistId, retry_after_ms: u32) -> LeaseMsg {
    LeaseMsg::Deny {
        claim_id: None,
        entity,
        reason: orrery_protocol::DenyReason::NotEligible,
        retry_after_ms,
    }
}

/// Cooperative handoff: the holder consents to give the lease up, naming the
/// successor it wants (or `None` to release), its final sequence pair and the
/// journal position it last saw acknowledged (D7 §5).
///
/// The consent is checked, not trusted. The registrar refuses — without
/// mutating anything — when the session does not hold the named token, when
/// the holder's `final_seq` claims an authority generation the registrar never
/// issued, or when its `cursor` names a journal position the cluster never
/// wrote. A handoff to a named successor additionally requires a cursor at
/// all: the successor must start from state the predecessor actually
/// committed, and without a cursor there is nothing to check that against.
///
/// A refusal is definitive and leaves the holder authoritative; the holder
/// stops writing when it *sends* a divest, so a refusal is conservative in the
/// safe direction — nobody writes until it reclaims.
async fn divest_lease(
    session: &PeerSession,
    router: &Arc<dyn Router>,
    redistributor: &Redistributor,
    claim_now_ms: u64,
    request: DivestRequest,
) -> LeaseMsg {
    let DivestRequest {
        entity,
        lease_id,
        to,
        final_seq,
        cursor,
    } = request;
    let metrics = &redistributor.metrics;

    let indexed = {
        let Some(mut peer) = session.lock_current().await else {
            metrics.record_divest_rejected();
            return divest_denied(entity, 0);
        };
        // Divesting is lease control like claiming, and a refused one still
        // costs an actor round trip, so it draws from the same NodeId-scoped
        // budget rather than being an unmetered path into the registrar.
        if !peer.claim_bucket.take(claim_now_ms) {
            let retry_after_ms = peer.claim_bucket.retry_after_ms();
            metrics.record_divest_rejected();
            return LeaseMsg::Deny {
                claim_id: None,
                entity,
                reason: orrery_protocol::DenyReason::RateLimited,
                retry_after_ms,
            };
        }
        peer.leases.get(&entity).copied()
    };
    let Some(indexed) = indexed.filter(|lease| {
        lease.lease_id == lease_id && lease.owner == SessionLeaseOwner::Active(session.generation)
    }) else {
        metrics.record_divest_rejected();
        return divest_denied(entity, 0);
    };

    // Uplink-completeness gate. Everything here is read-only: a refusal must
    // leave the holder exactly as authoritative as it was.
    let Ok((row, _, watermark)) = router.inspect_lease(indexed.grid, entity).await else {
        metrics.record_divest_rejected();
        return divest_denied(entity, 100);
    };
    let holder_matches = row.as_ref().is_some_and(|row| {
        row.holder == Some(session.node)
            && row.lease_id == lease_id
            && !final_seq.supersedes(row.seq)
    });
    let cursor_is_committed = match (cursor, watermark) {
        // A cursor past the cluster's own watermark for this entity names
        // state that was never journaled.
        (Some(cursor), Some(watermark)) => cursor <= watermark,
        (Some(_), None) => false,
        (None, _) => to.is_none(),
    };
    if !holder_matches || !cursor_is_committed {
        metrics.record_divest_rejected();
        return divest_denied(entity, 0);
    }

    let parked = router
        .park_lease(indexed.grid, indexed.cell, entity, session.node, lease_id)
        .await;
    if !matches!(&parked, Ok(Some(row)) if parked_by_us(row, lease_id)) {
        metrics.record_divest_rejected();
        return divest_denied(entity, 100);
    }
    {
        let mut peer = session.state.lock().await;
        if peer.leases.get(&entity) == Some(&indexed) {
            peer.leases.remove(&entity);
        }
    }
    metrics.record_divested();

    // A request the registrar made on a claimant's behalf is answered by this
    // reply, whichever way the holder decided.
    let requested = redistributor.take_pending(entity, session.node).await;

    let disposition = match to {
        None => orrery_protocol::ExpireDisposition::Parked,
        Some(successor) if successor == session.node => orrery_protocol::ExpireDisposition::Parked,
        Some(successor) => {
            // The named successor passes exactly the admission a claim of its
            // own would: a live session on this gateway plus live coordinator
            // interest covering the cell. Consent does not widen it.
            let eligible = redistributor
                .candidates(indexed.grid, indexed.cell, session.node, registrar_now_ms())
                .await
                .iter()
                .any(|candidate| candidate.node == successor);
            // When this consent answers a registrar request naming the same
            // successor, the grant carries that claimant's own correlation, so
            // its pending `Claim` resolves rather than looking unanswered.
            let claim_id = requested
                .filter(|pending| pending.claimant == successor)
                .map_or(orrery_protocol::ClaimId::REGISTRAR, |pending| {
                    pending.claim_id
                });
            let handed = eligible
                && redistributor
                    .hand_to(
                        router,
                        indexed.grid,
                        indexed.cell,
                        entity,
                        successor,
                        Some(session.node),
                        claim_id,
                        // A handoff answering an explicit grab confers the tier
                        // that grab asked for; an unsolicited release does not
                        // make the receiver an owner.
                        requested
                            .filter(|pending| pending.claimant == successor)
                            .map_or(orrery_protocol::ClaimKind::Weak, |pending| pending.kind),
                    )
                    .await
                    .is_some();
            if handed {
                metrics.record_reassigned();
                orrery_protocol::ExpireDisposition::Reassigned { to: successor }
            } else {
                metrics.record_parked_without_successor();
                orrery_protocol::ExpireDisposition::Parked
            }
        }
    };

    // A claimant whose request the holder answered by parking, or by handing
    // the entity to somebody else, is told so rather than left waiting for its
    // deadline to elapse.
    if let Some(pending) = requested.filter(|pending| {
        !matches!(
            disposition,
            orrery_protocol::ExpireDisposition::Reassigned { to } if to == pending.claimant
        )
    }) {
        redistributor
            .answer_claimant(
                pending.claimant,
                LeaseMsg::Deny {
                    claim_id: Some(pending.claim_id),
                    entity,
                    reason: orrery_protocol::DenyReason::NotEligible,
                    retry_after_ms: 0,
                },
            )
            .await;
    }

    LeaseMsg::Expire {
        entity,
        lease_id,
        last_holder: Some(session.node),
        // A consented release parks deliberately; a consented handoff ends the
        // holder's lease by registrar action. The disposition carries where
        // authority actually went.
        reason: if matches!(disposition, orrery_protocol::ExpireDisposition::Parked) {
            orrery_protocol::ExpireReason::Parked
        } else {
            orrery_protocol::ExpireReason::Revoked
        },
        disposition,
    }
}

/// Map a verification failure onto its stable wire code.
///
/// A peer gets a reason rather than silence: a refused grant otherwise shows
/// up only as claims failing `NotEligible`, which is the hardest possible way
/// to discover a key-rotation or clock problem.
fn interest_ack_reason(error: orrery_protocol::InterestGrantVerificationError) -> u8 {
    use orrery_protocol::InterestGrantVerificationError as GrantError;
    match error {
        GrantError::Malformed => orrery_protocol::INTEREST_ACK_MALFORMED,
        GrantError::UnknownIssuer(_) | GrantError::BadSignature => {
            orrery_protocol::INTEREST_ACK_UNTRUSTED
        }
        GrantError::WrongPeer => orrery_protocol::INTEREST_ACK_WRONG_PEER,
        GrantError::CellCount | GrantError::OverTtl => orrery_protocol::INTEREST_ACK_BOUNDS,
        GrantError::Superseded => orrery_protocol::INTEREST_ACK_SUPERSEDED,
        GrantError::Unsupported => orrery_protocol::INTEREST_ACK_UNSUPPORTED,
    }
}

async fn cleanup_peer_session(
    session: &PeerSession,
    router: &Arc<dyn Router>,
    redistributor: &Redistributor,
    now_ms: u64,
) {
    let leases = {
        let mut peer = session.state.lock().await;
        peer.live.remove(&session.generation);
        if peer.current == Some(session.generation) {
            peer.current = None;
            if matches!(&peer.notify, Some((generation, _)) if *generation == session.generation) {
                peer.notify = None;
            }
            for lease in peer.leases.values_mut() {
                if lease.owner == SessionLeaseOwner::Active(session.generation) {
                    lease.owner = SessionLeaseOwner::Parking(session.generation);
                }
            }
        }
        peer.leases
            .iter()
            .filter(|(_, lease)| lease.owner == SessionLeaseOwner::Parking(session.generation))
            .map(|(_, lease)| (lease.lease_id, *lease))
            .collect::<Vec<_>>()
    };

    let mut orphaned = Vec::new();
    for (lease_id, lease) in leases {
        let parked = router
            .park_lease(lease.grid, lease.cell, lease.entity, session.node, lease_id)
            .await;
        let Ok(parked_row) = parked else { continue };
        {
            let mut peer = session.state.lock().await;
            if peer.leases.get(&lease.entity) == Some(&lease) {
                peer.leases.remove(&lease.entity);
            }
        }
        // `park_lease` returns the row it just parked; a `None` means the
        // registrar had no row to park, and an unchanged row means someone
        // else now holds it — neither is ours to redistribute.
        if let Some(row) = parked_row.filter(|row| parked_by_us(row, lease_id)) {
            orphaned.push(crate::lease::ParkedLease {
                grid: lease.grid,
                cell: lease.cell,
                previous_holder: session.node,
                previous_lease_id: lease_id,
                lease: row,
                reason: orrery_protocol::ExpireReason::Disconnect,
            });
        }
    }

    // Redistribute only after every park has landed and every session lock is
    // released: a successor's grant path locks peer state of its own.
    for parked in orphaned {
        redistributor.redistribute(router, parked).await;
    }

    let mut peer = session.state.lock().await;
    if peer.current.is_none() && peer.live.is_empty() && peer.leases.is_empty() {
        peer.idle_since_ms = Some(now_ms);
    }
}

struct ClaimBucket {
    token_millis: u64,
    updated_ms: u64,
}

impl ClaimBucket {
    const CLAIMS_PER_SECOND: u64 = 20;
    const BURST_CLAIMS: u64 = 64;
    const TOKEN_MILLIS_PER_CLAIM: u64 = 1_000;
    const BURST_TOKEN_MILLIS: u64 = Self::BURST_CLAIMS * Self::TOKEN_MILLIS_PER_CLAIM;

    const fn new(now_ms: u64) -> Self {
        Self {
            token_millis: Self::BURST_TOKEN_MILLIS,
            updated_ms: now_ms,
        }
    }
    fn take(&mut self, now_ms: u64) -> bool {
        if now_ms > self.updated_ms {
            let replenished = now_ms
                .saturating_sub(self.updated_ms)
                .saturating_mul(Self::CLAIMS_PER_SECOND);
            self.token_millis = self
                .token_millis
                .saturating_add(replenished)
                .min(Self::BURST_TOKEN_MILLIS);
            self.updated_ms = now_ms;
        }
        if self.token_millis < Self::TOKEN_MILLIS_PER_CLAIM {
            false
        } else {
            self.token_millis -= Self::TOKEN_MILLIS_PER_CLAIM;
            true
        }
    }

    fn retry_after_ms(&self) -> u32 {
        let missing_token_millis = Self::TOKEN_MILLIS_PER_CLAIM.saturating_sub(self.token_millis);
        let wait_ms = missing_token_millis.saturating_add(Self::CLAIMS_PER_SECOND - 1)
            / Self::CLAIMS_PER_SECOND;
        u32::try_from(wait_ms).unwrap_or(u32::MAX)
    }
}

/// Reserve a slot in the per-connection intent lane without waiting. Keeping
/// the admission decision in a small helper makes its bounded behaviour
/// directly testable and prevents an accidental future `.await` in the
/// datagram reader.
fn reserve_intent_lane(lane: Arc<Semaphore>) -> Result<OwnedSemaphorePermit, IntentOutcome> {
    lane.try_acquire_owned()
        .map_err(|_| IntentOutcome::Rejected {
            // The protocol currently represents service-side admission failure as
            // an executor error. It is still definitive: no execution was
            // scheduled and therefore no commit is claimed.
            reason: orrery_protocol::REASON_EXECUTOR_ERROR,
        })
}

/// Run the synchronous edge checks for one submitted intent (D11 §2.2).
///
/// The checks run in edge-to-authority order, each rejection a definitive
/// `Rejected` ack carrying its reason code:
///
/// 1. **Signature** — [`Intent::verify_issuer`] over the canonical,
///    attestation-excluding preimage. Failed signatures never reach the
///    validator.
/// 2. **Issuer binding** — `intent.issuer` must be the connection's
///    authenticated id (`cx.issuer`): a peer may not submit intents in
///    another's name.
/// 3. **Admission** — the configured [`crate::intent::IntentValidator`]. The
///    library default is [`PermissiveValidator`], which admits everything;
///    the deployed binary runs [`crate::intent::BaselineIntentValidator`], and
///    a linked `Ruleset` rejects with its own reason code. It is handed the
///    connection context (`cx`) as well as the intent, because an
///    account-scoped check has nothing to authorize against otherwise — the
///    intent itself is entirely peer-authored.
/// 4. **Execution** — the configured [`IntentExecutor`]'s future must resolve
///    before the ack is sent, so a `Committed` outcome implies a durable
///    commit (RPO 0). With no executor configured the reply is
///    [`REASON_NO_EXECUTOR`] — the gateway never acks a commit that did not
///    happen (the pre-existing stub's inverted RPO-0).
fn admit_intent(
    intent: &Intent,
    validator: &dyn crate::intent::IntentValidator,
    cx: &IntentContext,
) -> Result<(), IntentOutcome> {
    // 1. Signature (docs/08-persistence.md §2.2: signature checks at the
    //    edge, before any transaction work).
    if !intent.verify_issuer() {
        return Err(IntentOutcome::Rejected {
            reason: REASON_BAD_SIGNATURE,
        });
    }

    // 2. Issuer binding (the connection's authenticated id is the only
    //    identity the gateway can trust).
    if intent.issuer != cx.issuer {
        return Err(IntentOutcome::Rejected {
            reason: REASON_ISSUER_MISMATCH,
        });
    }

    // 3. Admission (the Ruleset stub for now).
    let precheck = match validator.validate(intent, cx) {
        IntentVerdict::Admit(precheck) => precheck,
        IntentVerdict::Reject { reason } => return Err(IntentOutcome::Rejected { reason }),
    };
    // The FDB executor derives its read set from the intent's ops; the
    // precheck's named keys are reserved for a Ruleset-linked executor.
    let _ = precheck;
    Ok(())
}

/// Execute an intent that already passed the edge checks, then send its
/// definitive result. This is intentionally separate from [`admit_intent`]
/// so an FDB await never occupies the connection receive loop.
async fn execute_admitted_intent(
    send: &(dyn Fn(Bytes) + Send + Sync),
    intent: Intent,
    executor: &Option<SharedExecutor>,
    metrics: &GatewayIntentMetrics,
    received_at: Instant,
) {
    let intent_id = intent.intent_id;

    // 4. Execution — ack only after the future resolves. An executor error
    //    becomes a definitive rejection (bounded-retry refusal, §7).
    let outcome = match executor {
        None => IntentOutcome::Rejected {
            reason: REASON_NO_EXECUTOR,
        },
        Some(exec) => match exec.execute(&intent).await {
            Ok(outcome) => outcome,
            Err(err) => error_outcome(&err),
        },
    };
    send_intent_reply(send, intent_id, outcome, metrics, received_at);
}

/// Re-run one report's evidence and answer with the verdict.
///
/// The executor believes nothing the reporter said: it checks the reporter
/// signature, routes by the `RulesetId` the bundle pins, and re-executes.
/// Whatever comes back — including `EvidenceForged`, which strikes the
/// reporter that sent it — goes to the reporter, because a reporter that
/// cannot see its own verdict cannot tell a cheat it caught from a bundle it
/// assembled wrong.
fn adjudicate_report(
    send: &(dyn Fn(Bytes) + Send + Sync),
    adjudicator: &AdjudicationExecutor,
    report: &DiscrepancyReport,
    metrics: &GatewayReportMetrics,
) {
    let verdict = adjudicator.adjudicate(report);
    send_report_verdict(send, report, Some(verdict), REPORT_ADJUDICATED, metrics);
}

/// Refuse a report with a stable code and no verdict.
///
/// Never silence: a witness that files into a gateway with no adjudicator
/// configured, or over its account's rate limit, has to be able to tell that
/// from an exoneration — the two call for opposite responses.
fn send_report_refusal(
    send: &(dyn Fn(Bytes) + Send + Sync),
    report: &DiscrepancyReport,
    reason: u16,
    metrics: &GatewayReportMetrics,
) {
    send_report_verdict(send, report, None, reason, metrics);
}

/// Encode and send one report's answer. Every path uses this helper so a
/// report has exactly one reply, refusals included.
fn send_report_verdict(
    send: &(dyn Fn(Bytes) + Send + Sync),
    report: &DiscrepancyReport,
    verdict: Option<Verdict>,
    reason: u16,
    metrics: &GatewayReportMetrics,
) {
    // One choke point, so the outcome split is exhaustive by construction:
    // four refusal exits and one adjudication exit all pass through here, and
    // a future fifth refusal cannot forget to be counted.
    metrics.record(verdict.as_ref(), reason);
    let reply = GatewayReply::ReportVerdict {
        subject: report.subject,
        entity: report.bundle.entity,
        window_end: Tick::new(report.bundle.window_end.0),
        verdict,
        reason,
    };
    send(Bytes::from(encode_stream_frame(&reply)));
}

/// Encode and send an intent result. Every path uses this helper so an intent
/// has exactly one definitive acknowledgement, including lane saturation.
fn send_intent_reply(
    send: &(dyn Fn(Bytes) + Send + Sync),
    intent_id: u128,
    outcome: IntentOutcome,
    metrics: &GatewayIntentMetrics,
    received_at: Instant,
) {
    // Measured up to the send call, not past it: everything after is the
    // wire, which is precisely the part `intent_commit_ms` covers and this
    // series must not.
    metrics.record_reply(&outcome, elapsed_us(received_at));
    let reply = GatewayReply::IntentAck { intent_id, outcome };
    send(Bytes::from(encode_stream_frame(&reply)));
}

/// Feed the connection's datagrams into the shared inbound queue.
///
/// Its counterpart is [`reliable::spawn_receiver`]. Both write to the same
/// queue and the task ends when its source does, so the queue closes — and the
/// receive loop with it — only once *both* lanes are gone.
fn spawn_datagram_reader(
    conn: Arc<iroh::endpoint::Connection>,
    remote: NodeId,
    sink: tokio::sync::mpsc::UnboundedSender<Bytes>,
) {
    tokio::spawn(async move {
        loop {
            match conn.read_datagram().await {
                Ok(pkt) => {
                    if sink.send(pkt).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    debug!(?e, %remote, "gateway: datagram lane closed");
                    return;
                }
            }
        }
    });
}

/// Send the `[ACCEPTED]` admission response on a fresh uni stream.
async fn send_admission(conn: &iroh::endpoint::Connection) -> Result<(), String> {
    let mut stream = conn
        .open_uni()
        .await
        .map_err(|e| format!("open admission: {e}"))?;
    stream
        .write_all(&[ACCEPTED])
        .await
        .map_err(|e| format!("write admission: {e}"))?;
    stream
        .finish()
        .map_err(|e| format!("finish admission: {e}"))
}

#[allow(clippy::too_many_arguments)] // One session diff's dependencies, explicit.
async fn route_session_diff(
    send: &(dyn Fn(Bytes) + Send + Sync),
    diff: DiffUplink,
    session: &PeerSession,
    router: &Arc<dyn Router>,
    bulk_ack_admission: &SharedBulkAckAdmission,
    bulk_metrics: &GatewayBulkMetrics,
    authority_metrics: Arc<AuthorityMetrics>,
    received_at: Instant,
) {
    // Liveness only, and the guard must not outlive this check.
    //
    // `route_diff` below awaits an actor mailbox round trip and then the
    // journal fsync, so a `PeerState` lock held across it serializes every
    // diff from this peer behind the slowest commit — collapsing the
    // connection's `MAX_INFLIGHT_DIFF_ROUTES_PER_CONN` concurrent routes to
    // exactly one, which is worse than the eight-route cap that constant's
    // comment was written to replace.
    //
    // This read `let Some(_peer) = ...`, and the leading underscore is not
    // enough: a named binding lives to the end of the scope, where a bare `_`
    // would have dropped it immediately. Nothing here reads the guard's
    // contents. `PeerSession` already avoids the same trap for the account
    // field (see its `account` doc comment). Covered by
    // `session_diffs_do_not_serialize_on_the_peer_lock`.
    if session.lock_current().await.is_none() {
        send(Bytes::from(encode_datagram(&GatewayReply::BulkNack {
            entity: diff.entity,
            tick: diff.tick,
            reason: 2,
            lease: None,
        })));
        return;
    }
    route_diff(
        send,
        DiffRoute {
            diff,
            author: session.node,
            received_at,
            strict_authority: true,
            authority_metrics,
        },
        router,
        bulk_ack_admission,
        bulk_metrics,
    )
    .await;
}

struct DiffRoute {
    diff: DiffUplink,
    author: NodeId,
    received_at: Instant,
    strict_authority: bool,
    authority_metrics: Arc<AuthorityMetrics>,
}

/// Journal a bulk diff via the owning cell actor, then ack with the durable
/// LSN (or nack on rejection). The gateway fills in the server-assigned
/// `epoch`/`lsn`/`author`/`crc` (docs/08-persistence.md §2.1).
async fn route_diff(
    send: &(dyn Fn(Bytes) + Send + Sync),
    route: DiffRoute,
    router: &Arc<dyn Router>,
    bulk_ack_admission: &SharedBulkAckAdmission,
    bulk_metrics: &GatewayBulkMetrics,
) {
    let DiffRoute {
        diff,
        author,
        received_at,
        strict_authority,
        authority_metrics,
    } = route;
    let route_started = Instant::now();
    let route_queue_us = elapsed_us(received_at);
    let entity = diff.entity;
    let tick = diff.tick;
    let crc = payload_crc(&diff.payload);

    let record = JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: diff.cell,
        grid: diff.grid,
        entity,
        tick,
        epoch: Epoch::new(0),
        author,
        kind: diff.kind,
        payload: diff.payload,
        crc,
    };
    let result = if strict_authority {
        // The actor performs this comparison and append in one mailbox turn.
        // A missing pair deliberately uses the never-granted zero token so it
        // still returns the current row in the lease-specific NACK.
        let (lease_id, authority_seq) = diff
            .lease_id
            .zip(diff.authority_seq)
            .unwrap_or((LeaseId(0), Default::default()));
        let fence_now_ms = registrar_now_ms();
        match router
            .apply_fenced(record, author, lease_id, authority_seq, fence_now_ms)
            .await
        {
            Ok(FencedApply::Accepted(handle)) => Ok(handle),
            Ok(FencedApply::Rejected(lease)) => {
                // The single-writer invariant checker (D7 §5): a fenced-out
                // write whose live row names a *different* unexpired holder is
                // two peers believing they were the writer at once.
                observe_fencing_rejection(
                    &authority_metrics,
                    entity,
                    tick,
                    author,
                    lease_id,
                    lease.as_ref(),
                    fence_now_ms,
                );
                send(Bytes::from(encode_datagram(&GatewayReply::BulkNack {
                    entity,
                    tick,
                    reason: 2,
                    lease,
                })));
                return;
            }
            Err(error) => Err(error),
        }
    } else {
        router.apply(record).await
    };
    let router_apply_us = elapsed_us(route_started);
    let journal_wait_started = Instant::now();

    // The actor has already stamped, appended, and folded before returning
    // this handle. Keep the durability wait in this existing bounded route
    // task rather than spawning one resolver task per append.
    let result = match result {
        Ok(handle) => {
            let own_lsn = handle.lsn();
            handle
                .committed()
                .await
                .map(|_| own_lsn)
                .map_err(|_| Reject::JournalClosed)
        }
        Err(error) => Err(error),
    };
    let journal_wait_us = elapsed_us(journal_wait_started);

    match result {
        Ok(lsn) => {
            let reply_started = Instant::now();
            // Check after the actor reports its local journal append so the
            // reply states the ownership freshness at acknowledgement time.
            // A stale/lost owner remains observable to the caller but cannot
            // contaminate the durable recovery evidence set.
            let provisional = bulk_ack_admission
                .assess(diff.grid, diff.cell)
                .is_provisional();
            let reply = GatewayReply::BulkAck {
                entity,
                tick,
                lsn,
                provisional,
            };
            send(Bytes::from(encode_datagram(&reply)));
            bulk_metrics.record(
                route_queue_us,
                router_apply_us,
                journal_wait_us,
                elapsed_us(reply_started),
                elapsed_us(received_at),
            );
        }
        Err(_) => {
            let reply = GatewayReply::BulkNack {
                entity,
                tick,
                reason: 1,
                lease: None,
            };
            send(Bytes::from(encode_datagram(&reply)));
        }
    }
}

/// Serve an area load: read each requested cell from its owning actor and
/// stream an [`AreaPage`] back **as the cell resolves** — never buffered for a
/// trailing flush, so the centre cell's page lands before the 27th cell is
/// scanned (D16: < 50 ms to first page-in). `live` reports whether a live
/// actor held the cell (vs a cold FDB scan). `grid` scopes the cold scans
/// (P-7: storage cell ids are grid-relative).
///
/// Every requested cell gets a reply: an empty cell is an empty page, and a
/// failed read is a logged [`GatewayReply::AreaLoadError`] — never silent, so
/// a failed FDB scan is diagnosable rather than indistinguishable from an
/// empty cell (docs/08-persistence.md §9).
async fn route_subscribe(
    send: &(dyn Fn(Bytes) + Send + Sync),
    grid: GridId,
    cells: Vec<CellId>,
    remote: NodeId,
    router: &Arc<dyn Router>,
    metrics: &GatewayAreaMetrics,
    received_at: Instant,
) {
    // A per-send page counter: each cell's page (and each chunk of it) is
    // stamped with a distinct `page_seq`, so a client's reassembly never mixes
    // chunks of two sends of the same cell (a retried subscribe re-sends the
    // page under a new seq).
    let mut page_seq = 0u32;
    // The whole span lives inside this one scope — the Subscribe arrived
    // before it and the first frame leaves inside it — so an `Instant` and a
    // bool are the entire measurement. `first_page` flips on the send call
    // that carries the first frame, page or not: a subscribe whose every cell
    // read failed sends no page and contributes no sample.
    let mut first_page = true;
    metrics.record_subscribe();
    for cell in cells {
        // Live cells come from actor memory (authoritative, ≥ checkpoint
        // freshness); cold cells from the durable tier range scan
        // (docs/08-persistence.md §9).
        let live = router.has_actor(grid, cell).await;
        let read = if live {
            router.read(grid, cell).await.map(Some)
        } else {
            router.read_cold(grid, cell).await
        };
        match read {
            Ok(page) => {
                let page = page.unwrap_or_default();
                let mut entities = Vec::with_capacity(page.entities.len());
                let mut payloads = Vec::with_capacity(page.entities.len());
                for (id, record) in page.entities {
                    entities.push(id);
                    payloads.push(record.components);
                }
                page_seq = page_seq.wrapping_add(1);
                for frame in chunk_area_page(cell, entities, payloads, live, page_seq) {
                    send(Bytes::from(frame));
                    metrics.record_frame();
                    if first_page {
                        first_page = false;
                        metrics.record_first_page(elapsed_us(received_at));
                    }
                }
            }
            Err(e) => {
                let kind = if live {
                    orrery_protocol::AREA_LOAD_ERR_LIVE
                } else {
                    orrery_protocol::AREA_LOAD_ERR_COLD
                };
                warn!(?e, ?cell, %grid, %remote, kind, "gateway: area-load cell read failed");
                metrics.record_cell_read_error();
                send(Bytes::from(encode_stream_frame(
                    &GatewayReply::AreaLoadError { cell, kind },
                )));
            }
        }
    }
}

/// Split one cell's page into as many sequenced [`AreaPage`] frames as needed
/// to keep every encoded frame under [`MAX_AREA_PAGE_FRAME_BYTES`].
///
/// Pages ride the reliable lane, so this is no longer about fitting an MTU —
/// QUIC re-segments a stream write for us. It is about bounding the *message*:
/// the peer's reader refuses a length prefix past
/// `MAX_RELIABLE_MESSAGE_BYTES` before allocating for it, and a receiver
/// holding partial chunks for 27 cells wants each chunk's footprint knowable
/// in advance. If one entity's bag alone cannot fit, its frame is emitted
/// oversize, refused at the sender, and counted (an entity that big is a
/// Ruleset bug, not a transport problem).
fn chunk_area_page(
    cell: CellId,
    entities: Vec<PersistId>,
    payloads: Vec<Bytes>,
    live: bool,
    page_seq: u32,
) -> Vec<Vec<u8>> {
    debug_assert_eq!(entities.len(), payloads.len());
    let total = entities.len();

    // Greedy chunking: grow each chunk while the *encoded* frame stays under
    // the budget — measure the real bytes, never guess at postcard's per-item
    // overhead.
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    while start < total {
        let mut end = start + 1;
        while end < total {
            #[allow(clippy::cast_possible_truncation)]
            let frame = encode_chunk(
                cell,
                &entities,
                &payloads,
                start,
                end + 1,
                live,
                page_seq,
                0,
                1,
            );
            if frame.len() > MAX_AREA_PAGE_FRAME_BYTES {
                break;
            }
            end += 1;
        }
        chunks.push((start, end));
        start = end;
    }
    if chunks.is_empty() {
        // An empty cell is still a page: one empty chunk
        // (docs/08-persistence.md §9 — every requested cell gets a reply).
        chunks.push((0, 0));
    }

    #[allow(clippy::cast_possible_truncation)]
    let total_chunks = chunks.len() as u32;
    chunks
        .iter()
        .enumerate()
        .map(|(i, &(start, end))| {
            #[allow(clippy::cast_possible_truncation)]
            encode_chunk(
                cell,
                &entities,
                &payloads,
                start,
                end,
                live,
                page_seq,
                i as u32,
                total_chunks,
            )
        })
        .collect()
}

/// Encode one chunk of a cell's page: `entities[start..end]` with its chunk
/// coordinates (`page_seq`/`chunk_index`/`total_chunks`).
#[allow(clippy::too_many_arguments)]
fn encode_chunk(
    cell: CellId,
    entities: &[PersistId],
    payloads: &[Bytes],
    start: usize,
    end: usize,
    live: bool,
    page_seq: u32,
    chunk_index: u32,
    total_chunks: u32,
) -> Vec<u8> {
    encode_stream_frame(&GatewayReply::AreaPage {
        cell,
        page: AreaPage {
            cell,
            page_seq,
            chunk_index,
            total_chunks,
            entities: entities[start..end].to_vec(),
            payloads: payloads[start..end].to_vec(),
            live,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::actor::{Reject, SnapshotPage};
    use crate::fence::{
        FenceFreshnessConfig, FenceFreshnessMonitor, FenceOutcome, FenceRow, FenceStatus,
        FenceStore, MemFenceStore,
    };
    use orrery_protocol::{LeaseFlags, SeqPair};

    fn successor_node(seed: u8) -> NodeId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn nearest_interest_ranks_interaction_then_load_then_node_id() {
        // Given: three eligible peers. Eligibility already means each one's
        // coordinator interest covers this exact cell, so coverage carries no
        // ranking signal — what separates them is interaction and load.
        let cell = CellId::ROOT.children()[0].children()[1];
        let interacting = SuccessorCandidate {
            node: successor_node(1),
            held_leases: 40,
            holds_lease_in_cell: true,
        };
        let idle = SuccessorCandidate {
            node: successor_node(3),
            held_leases: 0,
            holds_lease_in_cell: false,
        };
        let busy = SuccessorCandidate {
            node: successor_node(4),
            held_leases: 9,
            holds_lease_in_cell: false,
        };
        let policy = NearestInterestSuccessorPolicy;
        let request = |candidates: &[SuccessorCandidate]| {
            policy.select(&SuccessorRequest {
                grid: GridId::ROOT,
                cell,
                entity: PersistId::new(1),
                previous_holder: successor_node(9),
                reason: orrery_protocol::ExpireReason::Timeout,
                candidates,
            })
        };

        // Then: demonstrated interaction outranks a far lighter load.
        assert_eq!(request(&[idle, interacting, busy]), Some(interacting.node));
        // Then: with nobody interacting, the least loaded inherits, so one
        // peer does not absorb a crashed holder's whole working set.
        assert_eq!(request(&[busy, idle]), Some(idle.node));
        // Then: a tie is broken deterministically, so a replay reproduces it.
        let tie_a = SuccessorCandidate {
            node: successor_node(2),
            held_leases: 0,
            holds_lease_in_cell: false,
        };
        let chosen = request(&[idle, tie_a]);
        assert_eq!(chosen, request(&[tie_a, idle]), "order must not decide");
        assert!(chosen == Some(idle.node) || chosen == Some(tie_a.node));
    }

    #[test]
    fn a_policy_never_selects_the_holder_it_is_replacing() {
        // Given: the lost holder is somehow still in the candidate list.
        let previous_holder = successor_node(1);
        let candidates = [SuccessorCandidate {
            node: previous_holder,
            held_leases: 0,
            holds_lease_in_cell: true,
        }];

        // Then: it is filtered out and the entity parks instead.
        assert_eq!(
            NearestInterestSuccessorPolicy.select(&SuccessorRequest {
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                entity: PersistId::new(1),
                previous_holder,
                reason: orrery_protocol::ExpireReason::Disconnect,
                candidates: &candidates,
            }),
            None
        );
    }

    #[test]
    fn a_returned_row_is_only_evidence_of_our_own_park() {
        // `park_lease` answers with the live row when the presented holder or
        // token no longer matches, so the reply alone proves nothing.
        let row = |holder: Option<NodeId>, lease_id: LeaseId| orrery_protocol::Lease {
            entity: PersistId::new(3),
            holder,
            seq: orrery_protocol::SeqPair::default(),
            lease_id,
            expires_at: 0,
            flags: orrery_protocol::LeaseFlags::PARKED,
            bound_to: None,
        };

        // A park of ours advances the token and clears the holder.
        assert!(parked_by_us(&row(None, LeaseId(5)), LeaseId(4)));
        // Someone else holds it now: redistributing would take it from them.
        assert!(!parked_by_us(
            &row(Some(successor_node(1)), LeaseId(5)),
            LeaseId(4)
        ));
        // The row is untouched, so nothing was parked.
        assert!(!parked_by_us(&row(None, LeaseId(4)), LeaseId(4)));
    }

    #[test]
    fn duplicate_authority_counts_only_a_second_live_writer() {
        let writer = successor_node(1);
        let other = successor_node(2);
        let entity = PersistId::new(7);
        let tick = orrery_protocol::Tick::new(3);
        let row = |holder: Option<NodeId>, expires_at: u64| orrery_protocol::Lease {
            entity,
            holder,
            seq: orrery_protocol::SeqPair::default(),
            lease_id: LeaseId(9),
            expires_at,
            flags: orrery_protocol::LeaseFlags::default(),
            bound_to: None,
        };
        let observe = |lease: Option<orrery_protocol::Lease>| {
            let metrics = AuthorityMetrics::default();
            observe_fencing_rejection(
                &metrics,
                entity,
                tick,
                writer,
                LeaseId(8),
                lease.as_ref(),
                100,
            );
            metrics.snapshot().duplicate_authority
        };

        // Ordinary fencing, not two writers: no row at all, a parked row, an
        // expired row, or the writer's own superseded token.
        assert_eq!(observe(None), 0);
        assert_eq!(observe(Some(row(None, 1_000))), 0);
        assert_eq!(observe(Some(row(Some(other), 100))), 0);
        assert_eq!(observe(Some(row(Some(writer), 1_000))), 0);

        // A different holder with an unexpired lease is the real thing: two
        // peers believed they were the writer at the same tick.
        let metrics = AuthorityMetrics::default();
        let live = row(Some(other), 1_000);
        observe_fencing_rejection(&metrics, entity, tick, writer, LeaseId(8), Some(&live), 100);
        assert_eq!(metrics.snapshot().duplicate_authority, 1);
        assert_eq!(
            metrics.last_duplicate_authority(),
            Some(DuplicateAuthoritySample {
                entity,
                tick,
                rejected_writer: writer,
                rejected_lease_id: LeaseId(8),
                current_holder: other,
                current_lease_id: LeaseId(9),
            })
        );
    }

    fn coordinator_secret(seed: u8) -> iroh::SecretKey {
        iroh::SecretKey::from_bytes(&[seed; 32])
    }

    fn signed_grant(
        key: &iroh::SecretKey,
        key_id: u32,
        peer: NodeId,
        epoch: u64,
        cells: Vec<CellId>,
        ttl_ms: u64,
    ) -> Vec<u8> {
        orrery_protocol::InterestGrantV1::sign(
            orrery_protocol::InterestGrantClaimsV1::new(
                peer,
                Epoch::new(epoch),
                GridId::ROOT,
                cells,
                ttl_ms,
                orrery_protocol::IssuerKeyId::new(key_id),
            ),
            key,
        )
        .expect("sign grant")
        .encode()
        .expect("encode grant")
    }

    #[test]
    fn a_verified_grant_becomes_interest_dated_by_the_gateways_own_clock() {
        // Given: a gateway trusting one coordinator key.
        let coordinator = coordinator_secret(9);
        let authority = CoordinatorHandoutAuthority::new([IssuerKey::new(
            orrery_protocol::IssuerKeyId::new(3),
            coordinator.public(),
        )]);
        let peer = successor_node(1);
        let cell = CellId::ROOT.children()[0];

        // When: the peer presents its coordinator-signed grant at t=1000.
        let epoch = authority
            .apply_grant(
                &signed_grant(&coordinator, 3, peer, 7, vec![cell], 30_000),
                &peer,
                1_000,
            )
            .expect("a genuine grant is accepted");

        // Then: interest is in force, and its deadline is this gateway's own
        // clock plus the grant's lifetime — never a coordinator timestamp.
        assert_eq!(epoch, Epoch::new(7));
        let snapshot = authority.snapshot_for(peer).expect("interest on file");
        assert_eq!(snapshot.valid_until_ms, 31_000);
        assert!(authority.allows(peer, GridId::ROOT, cell, 30_999));
        assert!(!authority.allows(peer, GridId::ROOT, cell, 31_000));
        // Coverage is exact: a neighbouring cell was never granted.
        assert!(!authority.allows(peer, GridId::ROOT, CellId::ROOT.children()[1], 1_100));
    }

    #[test]
    fn a_peer_cannot_grant_itself_interest() {
        // The attack this whole path exists to stop: self-declared interest
        // would be self-granted authority, since interest gates weak claims.
        let coordinator = coordinator_secret(9);
        let authority = CoordinatorHandoutAuthority::new([IssuerKey::new(
            orrery_protocol::IssuerKeyId::new(3),
            coordinator.public(),
        )]);
        let peer = successor_node(1);
        let forged = coordinator_secret(1);

        assert_eq!(
            authority.apply_grant(
                &signed_grant(&forged, 3, peer, 1, vec![CellId::ROOT], 30_000),
                &peer,
                1_000
            ),
            Err(orrery_protocol::InterestGrantVerificationError::BadSignature)
        );
        assert!(authority.snapshot_for(peer).is_none());
    }

    #[test]
    fn a_narrowed_interest_cannot_be_widened_by_replaying_the_old_grant() {
        // Given: a peer that moved, so the coordinator issued a narrower grant.
        let coordinator = coordinator_secret(9);
        let authority = CoordinatorHandoutAuthority::new([IssuerKey::new(
            orrery_protocol::IssuerKeyId::new(3),
            coordinator.public(),
        )]);
        let peer = successor_node(1);
        let cells = CellId::ROOT.children();
        let wide = signed_grant(&coordinator, 3, peer, 1, vec![cells[0], cells[1]], 30_000);
        let narrow = signed_grant(&coordinator, 3, peer, 2, vec![cells[0]], 30_000);

        authority
            .apply_grant(&wide, &peer, 1_000)
            .expect("accepted");
        authority
            .apply_grant(&narrow, &peer, 1_100)
            .expect("newer epoch accepted");
        assert!(!authority.allows(peer, GridId::ROOT, cells[1], 1_200));

        // When: the peer replays the older, wider grant.
        assert_eq!(
            authority.apply_grant(&wide, &peer, 1_200),
            Err(orrery_protocol::InterestGrantVerificationError::Superseded)
        );

        // Then: the narrowed coverage stands.
        assert!(!authority.allows(peer, GridId::ROOT, cells[1], 1_200));
        assert!(authority.allows(peer, GridId::ROOT, cells[0], 1_200));
    }

    #[test]
    fn re_presenting_the_current_grant_refreshes_its_deadline() {
        // A peer holding steady interest must be able to keep it alive without
        // the coordinator inventing a new epoch on every presence tick.
        let coordinator = coordinator_secret(9);
        let authority = CoordinatorHandoutAuthority::new([IssuerKey::new(
            orrery_protocol::IssuerKeyId::new(3),
            coordinator.public(),
        )]);
        let peer = successor_node(1);
        let grant = signed_grant(&coordinator, 3, peer, 5, vec![CellId::ROOT], 30_000);

        authority
            .apply_grant(&grant, &peer, 1_000)
            .expect("accepted");
        authority
            .apply_grant(&grant, &peer, 20_000)
            .expect("same epoch refreshes");

        assert_eq!(
            authority
                .snapshot_for(peer)
                .expect("on file")
                .valid_until_ms,
            50_000
        );
    }

    #[test]
    fn expired_interest_is_reclaimed_rather_than_retained_forever() {
        let coordinator = coordinator_secret(9);
        let authority = CoordinatorHandoutAuthority::new([IssuerKey::new(
            orrery_protocol::IssuerKeyId::new(3),
            coordinator.public(),
        )]);
        let peer = successor_node(1);
        authority
            .apply_grant(
                &signed_grant(&coordinator, 3, peer, 1, vec![CellId::ROOT], 30_000),
                &peer,
                0,
            )
            .expect("accepted");
        assert_eq!(authority.tracked_peers(), 1);

        authority.prune_expired(29_999);
        assert_eq!(authority.tracked_peers(), 1, "still inside its lifetime");
        authority.prune_expired(30_000);
        assert_eq!(authority.tracked_peers(), 0);
    }

    #[test]
    fn a_gateway_with_no_coordinator_keys_says_so_instead_of_going_quiet() {
        // `DenyAllInterestAuthority` is the default, and a peer presenting a
        // grant to it must learn that grants are not accepted here rather than
        // discovering it later as unexplained `NotEligible` claims.
        assert_eq!(
            DenyAllInterestAuthority.apply_grant(b"anything", &successor_node(1), 0),
            Err(orrery_protocol::InterestGrantVerificationError::Unsupported)
        );
    }

    /// A redistributor for unit tests: no coordinator interest, so no peer is
    /// ever a candidate and every lost lease parks — the behaviour these tests
    /// were written against.
    fn parking_redistributor(peers: Arc<PeerRegistry>) -> Redistributor {
        Redistributor {
            peers,
            interest: Arc::new(DenyAllInterestAuthority),
            policy: Arc::new(ParkOnLossPolicy),
            metrics: Arc::new(AuthorityMetrics::default()),
            handoff_deadline_ms: 300,
            pending: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    fn interest_snapshot(
        peer: NodeId,
        epoch: u64,
        grid: GridId,
        covered_cells: Vec<CellId>,
        valid_until_ms: u64,
    ) -> CoordinatorInterestSnapshot {
        CoordinatorInterestSnapshot {
            peer,
            epoch: Epoch::new(epoch),
            grid,
            covered_cells,
            valid_until_ms,
        }
    }

    struct WrongPeerInterestAuthority {
        snapshot: CoordinatorInterestSnapshot,
    }

    impl InterestAuthority for WrongPeerInterestAuthority {
        fn snapshot_for(&self, _peer: NodeId) -> Option<CoordinatorInterestSnapshot> {
            Some(self.snapshot.clone())
        }
    }

    struct SuccessfulRouter;

    #[async_trait::async_trait]
    impl Router for SuccessfulRouter {
        async fn apply(
            &self,
            _record: JournalRecord,
        ) -> Result<Arc<crate::journal::AppendHandle>, Reject> {
            Ok(crate::journal::AppendHandle::completed(Lsn::new(7, 11)))
        }

        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }

        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }
    }

    struct BlockingParkRouter {
        entered: tokio::sync::mpsc::Sender<LeaseId>,
        release: Arc<tokio::sync::Notify>,
        parked: Mutex<Vec<LeaseId>>,
    }

    #[async_trait::async_trait]
    impl Router for BlockingParkRouter {
        async fn apply(
            &self,
            _record: JournalRecord,
        ) -> Result<Arc<crate::journal::AppendHandle>, Reject> {
            Ok(crate::journal::AppendHandle::completed(Lsn::new(7, 11)))
        }

        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }

        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }

        async fn park_lease(
            &self,
            _grid: GridId,
            _cell: CellId,
            _entity: PersistId,
            _holder: NodeId,
            lease_id: LeaseId,
        ) -> Result<Option<orrery_protocol::Lease>, Reject> {
            self.entered
                .send(lease_id)
                .await
                .map_err(|_| Reject::JournalClosed)?;
            self.release.notified().await;
            self.parked
                .lock()
                .expect("parked lease lock")
                .push(lease_id);
            Ok(None)
        }
    }

    struct StaleAdmission;

    impl BulkAckAdmission for StaleAdmission {
        fn assess(&self, _grid: GridId, _cell: CellId) -> BulkAckDisposition {
            BulkAckDisposition::Provisional
        }
    }

    #[test]
    fn saturated_intent_lane_is_definitively_rejected_without_waiting() {
        let lane = Arc::new(Semaphore::new(1));
        let held = reserve_intent_lane(Arc::clone(&lane)).expect("first slot");

        let outcome = reserve_intent_lane(lane).expect_err("full lane rejects immediately");
        assert_eq!(
            outcome,
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_EXECUTOR_ERROR,
            }
        );
        drop(held);
    }

    #[test]
    fn claim_bucket_allows_d16_burst_then_refills_at_twenty_per_second() {
        let mut bucket = ClaimBucket::new(0);
        for _ in 0..64 {
            assert!(bucket.take(0), "configured burst is admitted");
        }
        assert!(!bucket.take(0), "65th immediate claim is rate limited");
        assert_eq!(bucket.retry_after_ms(), 50);
        assert!(!bucket.take(1), "one millisecond cannot admit a claim");
        assert_eq!(bucket.retry_after_ms(), 49);

        // A deterministic monotonic second restores only the D16 sustained rate.
        for _ in 0..20 {
            assert!(bucket.take(1_000), "one second restores 20 claim tokens");
        }
        assert!(
            !bucket.take(1_000),
            "refill does not exceed sustained D16 rate"
        );
    }

    #[tokio::test]
    async fn peer_registry_shares_claim_bucket_across_replacement_sessions() {
        // Given: one authenticated NodeId has exhausted half its burst.
        let registry = Arc::new(PeerRegistry::new(2, 10_000, MAX_PEER_LIVE_LEASES));
        let node = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let authorization = GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
            orrery_protocol::AccountId::new(1),
            node,
            UnixMillis::new(0),
            orrery_protocol::SessionTokenTtlMs::new(1_000),
            orrery_protocol::SessionStanding::Good,
            orrery_protocol::IssuerKeyId::new(1),
        ));
        let first = registry
            .activate(node, authorization.clone(), b"first", None, 0, 0)
            .await
            .expect("valid peer is admitted");
        let mut first_state = first.lock_current().await.expect("first is current");
        for _ in 0..16 {
            assert!(first_state.claim_bucket.take(0));
        }
        drop(first_state);

        // When: a parallel authenticated connection replaces the first one.
        let replacement = registry
            .activate(
                node,
                authorization,
                b"replacement",
                Some(first.generation),
                0,
                0,
            )
            .await
            .expect("replacement is admitted");
        let mut replacement_state = replacement
            .lock_current()
            .await
            .expect("replacement is current");

        // Then: both connections consumed one 64-claim burst, and only a
        // deterministic one-second refill admits the next twenty claims.
        for _ in 0..16 {
            assert!(replacement_state.claim_bucket.take(0));
        }
        drop(replacement_state);
        let router: Arc<dyn Router> = Arc::new(SuccessfulRouter);
        cleanup_peer_session(
            &replacement,
            &router,
            &parking_redistributor(Arc::clone(&registry)),
            1,
        )
        .await;
        let reconnect = registry
            .activate(
                node,
                GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
                    orrery_protocol::AccountId::new(1),
                    node,
                    UnixMillis::new(0),
                    orrery_protocol::SessionTokenTtlMs::new(1_000),
                    orrery_protocol::SessionStanding::Good,
                    orrery_protocol::IssuerKeyId::new(1),
                )),
                b"reconnect",
                None,
                1,
                0,
            )
            .await
            .expect("retained peer reconnects");
        let mut reconnect_state = reconnect
            .lock_current()
            .await
            .expect("reconnect is current");

        for _ in 0..32 {
            assert!(reconnect_state.claim_bucket.take(0));
        }
        assert!(!reconnect_state.claim_bucket.take(0));
        for _ in 0..20 {
            assert!(reconnect_state.claim_bucket.take(1_000));
        }
        assert!(!reconnect_state.claim_bucket.take(1_000));

        let other_node = iroh::SecretKey::from_bytes(&[2; 32]).public();
        let other = registry
            .activate(
                other_node,
                GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
                    orrery_protocol::AccountId::new(2),
                    other_node,
                    UnixMillis::new(0),
                    orrery_protocol::SessionTokenTtlMs::new(1_000),
                    orrery_protocol::SessionStanding::Good,
                    orrery_protocol::IssuerKeyId::new(1),
                )),
                b"other",
                None,
                1_000,
                1_000,
            )
            .await
            .expect("independent peer is admitted");
        let mut other_state = other.lock_current().await.expect("other is current");
        for _ in 0..64 {
            assert!(other_state.claim_bucket.take(1_000));
        }
        assert!(!other_state.claim_bucket.take(1_000));
    }

    #[tokio::test]
    async fn peer_registry_rejects_claim_when_lease_capacity_is_full() {
        // Given: a NodeId-scoped peer has one indexed live lease, filling its capacity.
        let registry = PeerRegistry::new(2, 10_000, 1);
        let node = iroh::SecretKey::from_bytes(&[4; 32]).public();
        let authorization = GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
            orrery_protocol::AccountId::new(4),
            node,
            UnixMillis::new(0),
            orrery_protocol::SessionTokenTtlMs::new(1_000),
            orrery_protocol::SessionStanding::Good,
            orrery_protocol::IssuerKeyId::new(1),
        ));
        let first = registry
            .activate(node, authorization.clone(), b"first", None, 0, 0)
            .await
            .expect("valid peer is admitted");
        first.state.lock().await.leases.insert(
            PersistId::new(44),
            SessionLease {
                entity: PersistId::new(44),
                lease_id: LeaseId(44),
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                owner: SessionLeaseOwner::Active(first.generation),
            },
        );

        // When: that same NodeId reconnects and attempts one more claim.
        let replacement = registry
            .activate(
                node,
                authorization,
                b"replacement",
                Some(first.generation),
                0,
                0,
            )
            .await
            .expect("replacement preserves the established peer");
        let admitted = replacement.try_reserve_lease_slot().await;

        // Then: the bound survives replacement, no admission is reserved, and
        // the live-lease index cannot grow before actor routing.
        let peer = replacement
            .lock_current()
            .await
            .expect("replacement remains current");
        assert!(!admitted);
        assert_eq!(peer.leases.len(), 1);
        assert_eq!(peer.pending_lease_claims, 0);
    }

    #[tokio::test]
    async fn cleanup_peer_session_releases_peer_state_while_park_is_pending() {
        // Given: a current session owns an indexed lease and parking blocks at the router seam.
        let registry = Arc::new(PeerRegistry::new(2, 10_000, MAX_PEER_LIVE_LEASES));
        let node = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let authorization = GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
            orrery_protocol::AccountId::new(3),
            node,
            UnixMillis::new(0),
            orrery_protocol::SessionTokenTtlMs::new(1_000),
            orrery_protocol::SessionStanding::Good,
            orrery_protocol::IssuerKeyId::new(1),
        ));
        let first = registry
            .activate(node, authorization.clone(), b"first", None, 0, 0)
            .await
            .expect("first session is admitted");
        let parked_lease = LeaseId(41);
        first.state.lock().await.leases.insert(
            PersistId::new(41),
            SessionLease {
                entity: PersistId::new(41),
                lease_id: parked_lease,
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                owner: SessionLeaseOwner::Active(first.generation),
            },
        );
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(2);
        let release = Arc::new(tokio::sync::Notify::new());
        let router = Arc::new(BlockingParkRouter {
            entered: entered_tx,
            release: Arc::clone(&release),
            parked: Mutex::new(Vec::new()),
        });
        let cleanup_router: Arc<dyn Router> = router.clone();
        let cleanup_session = first.clone();
        let cleanup_registry = Arc::clone(&registry);
        let cleanup = tokio::spawn(async move {
            cleanup_peer_session(
                &cleanup_session,
                &cleanup_router,
                &parking_redistributor(cleanup_registry),
                1,
            )
            .await;
        });
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(250), entered_rx.recv())
                .await
                .expect("cleanup reaches the blocking park seam"),
            Some(parked_lease)
        );

        // When: a replacement needs the same PeerState while the old cleanup is blocked.
        let replacement = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            registry.activate(
                node,
                authorization,
                b"replacement",
                Some(first.generation),
                1,
                1,
            ),
        )
        .await
        .expect("blocked parking does not retain the peer-state mutex")
        .expect("replacement session is admitted");
        let replacement_lease = LeaseId(42);
        let mut replacement_state = replacement
            .lock_current()
            .await
            .expect("replacement owns peer state while old cleanup parks");
        assert_eq!(
            replacement_state
                .leases
                .get(&PersistId::new(41))
                .map(|lease| lease.owner),
            Some(SessionLeaseOwner::Parking(first.generation)),
            "a pending old cleanup lease stays reserved for its original generation"
        );
        replacement_state.leases.insert(
            PersistId::new(42),
            SessionLease {
                entity: PersistId::new(42),
                lease_id: replacement_lease,
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                owner: SessionLeaseOwner::Active(replacement.generation),
            },
        );
        drop(replacement_state);

        // Then: cancellation leaves the pending old lease resumable without touching the replacement.
        cleanup.abort();
        assert!(cleanup
            .await
            .expect_err("blocked cleanup is cancelled")
            .is_cancelled());
        let state = first.state.lock().await;
        assert!(state.leases.contains_key(&PersistId::new(41)));
        assert!(state.leases.contains_key(&PersistId::new(42)));
        drop(state);

        let resume_router: Arc<dyn Router> = router.clone();
        let resume_session = first.clone();
        let resume_registry = Arc::clone(&registry);
        let resume = tokio::spawn(async move {
            cleanup_peer_session(
                &resume_session,
                &resume_router,
                &parking_redistributor(resume_registry),
                2,
            )
            .await;
        });
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(250), entered_rx.recv())
                .await
                .expect("resumed cleanup reaches the blocking park seam"),
            Some(parked_lease)
        );
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_millis(250), resume)
            .await
            .expect("resumed cleanup completes after park release")
            .expect("resumed cleanup task does not panic");
        assert_eq!(
            router.parked.lock().expect("parked lease lock").as_slice(),
            &[parked_lease],
            "obsolete cleanup parks only its own pre-replacement lease"
        );

        let replacement_router: Arc<dyn Router> = router.clone();
        let replacement_session = replacement.clone();
        let replacement_registry = Arc::clone(&registry);
        let replacement_cleanup = tokio::spawn(async move {
            cleanup_peer_session(
                &replacement_session,
                &replacement_router,
                &parking_redistributor(replacement_registry),
                3,
            )
            .await;
        });
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(250), entered_rx.recv())
                .await
                .expect("current cleanup reaches the blocking park seam"),
            Some(replacement_lease)
        );
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_millis(250), replacement_cleanup)
            .await
            .expect("current cleanup completes after park release")
            .expect("current cleanup task does not panic");
        assert_eq!(
            router.parked.lock().expect("parked lease lock").as_slice(),
            &[parked_lease, replacement_lease],
            "current cleanup parks its replacement-owned lease after the old cleanup resumes"
        );
    }

    #[test]
    fn default_bulk_ack_admission_is_fresh() {
        assert_eq!(
            FreshBulkAckAdmission.assess(GridId::ROOT, CellId::ROOT),
            BulkAckDisposition::Durable
        );
    }

    #[test]
    fn default_gateway_config_denies_interest() {
        let peer = iroh::SecretKey::from_bytes(&[6; 32]).public();

        assert!(!GatewayConfig::default().interest_authority.allows(
            peer,
            GridId::ROOT,
            CellId::ROOT,
            0
        ));
    }

    #[test]
    fn interest_authority_allows_an_exact_live_snapshot_match() {
        let peer = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let grid = GridId::new(4);
        let cell = CellId::ROOT;
        let authority = SnapshotInterestAuthority::from_snapshots([interest_snapshot(
            peer,
            2,
            grid,
            vec![cell],
            101,
        )]);

        assert!(authority.allows(peer, grid, cell, 100));
    }

    #[test]
    fn interest_authority_denies_missing_stale_or_replaced_snapshot() {
        let peer = iroh::SecretKey::from_bytes(&[8; 32]).public();
        let grid = GridId::new(5);
        let cell = CellId::ROOT;

        assert!(!DenyAllInterestAuthority.allows(peer, grid, cell, 0));

        let stale = SnapshotInterestAuthority::from_snapshots([interest_snapshot(
            peer,
            1,
            grid,
            vec![cell],
            100,
        )]);
        assert!(!stale.allows(peer, grid, cell, 100));

        let replaced = SnapshotInterestAuthority::from_snapshots([
            interest_snapshot(peer, 1, grid, vec![cell], 200),
            interest_snapshot(peer, 2, grid, Vec::new(), 200),
        ]);
        assert!(!replaced.allows(peer, grid, cell, 100));
    }

    #[test]
    fn interest_authority_denies_wrong_peer_grid_or_uncovered_cell() {
        let peer = iroh::SecretKey::from_bytes(&[9; 32]).public();
        let other_peer = iroh::SecretKey::from_bytes(&[10; 32]).public();
        let grid = GridId::new(6);
        let cell = CellId::ROOT;
        let snapshot = interest_snapshot(peer, 1, grid, vec![cell], 200);
        let authority = WrongPeerInterestAuthority { snapshot };

        assert!(!authority.allows(other_peer, grid, cell, 100));

        let covered = SnapshotInterestAuthority::from_snapshots([interest_snapshot(
            peer,
            1,
            grid,
            vec![cell],
            200,
        )]);
        assert!(!covered.allows(peer, GridId::new(7), cell, 100));
        assert!(!covered.allows(peer, grid, CellId::ROOT.children()[0], 100));
    }

    #[tokio::test]
    async fn stale_ownership_downgrades_a_successful_bulk_route_to_provisional() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&sent);
        let send = move |bytes| capture.lock().expect("capture lock").push(bytes);
        let router: Arc<dyn Router> = Arc::new(SuccessfulRouter);
        let admission: SharedBulkAckAdmission = Arc::new(StaleAdmission);
        let metrics = GatewayBulkMetrics::default();

        route_diff(
            &send,
            DiffRoute {
                diff: DiffUplink {
                    cell: CellId::ROOT,
                    grid: GridId::ROOT,
                    entity: PersistId::new(12),
                    tick: orrery_protocol::Tick::new(3),
                    kind: orrery_protocol::RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"state"),
                    seq: 3,
                    lease_id: None,
                    authority_seq: None,
                },
                authority_metrics: Arc::new(AuthorityMetrics::default()),
                author: iroh::SecretKey::from_bytes(&[1; 32]).public(),
                received_at: Instant::now(),
                strict_authority: false,
            },
            &router,
            &admission,
            &metrics,
        )
        .await;

        let bytes = sent
            .lock()
            .expect("capture lock")
            .pop()
            .expect("bulk reply");
        assert!(matches!(
            decode_datagram(&bytes),
            Some(GatewayReply::BulkAck {
                entity,
                tick,
                lsn,
                provisional: true,
            }) if entity == PersistId::new(12)
                && tick == orrery_protocol::Tick::new(3)
                && lsn == Lsn::new(7, 11)
        ));
        let mut cursor = GatewayBulkSnapshot::default();
        let delta = metrics.delta(&mut cursor);
        assert_eq!(delta.acknowledgements, 1);
        assert_eq!(metrics.delta(&mut cursor).acknowledgements, 0);
        let mut latency_cursor = GatewayBulkMetrics::default().latency_snapshot();
        let samples = metrics.latency_delta(&mut latency_cursor);
        assert_eq!(samples.iter().map(|sample| sample.count).sum::<u64>(), 1);
        assert!(metrics.latency_delta(&mut latency_cursor).is_empty());
    }

    #[tokio::test]
    async fn fence_monitor_downgrades_a_successful_bulk_route_to_provisional() {
        let fences = Arc::new(MemFenceStore::new());
        let expected = FenceRow {
            owner: 13,
            epoch: Epoch::new(2),
            status: FenceStatus::Active,
        };
        assert_eq!(
            fences
                .fence(GridId::ROOT, CellId::ROOT, None, &expected)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        let monitor = FenceFreshnessMonitor::start(
            fences.clone(),
            GridId::ROOT,
            vec![(CellId::ROOT, expected)],
            FenceFreshnessConfig {
                poll_interval: std::time::Duration::from_millis(2),
                max_staleness: std::time::Duration::from_secs(3),
            },
        )
        .unwrap();
        let replacement = FenceRow {
            owner: 14,
            epoch: Epoch::new(3),
            status: FenceStatus::Active,
        };
        assert_eq!(
            fences
                .fence(GridId::ROOT, CellId::ROOT, Some(&expected), &replacement,)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                if monitor.assess(GridId::ROOT, CellId::ROOT) == BulkAckDisposition::Provisional {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("fence mismatch reaches admission");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&sent);
        let send = move |bytes| capture.lock().expect("capture lock").push(bytes);
        let router: Arc<dyn Router> = Arc::new(SuccessfulRouter);
        let admission: SharedBulkAckAdmission = monitor.clone();
        route_diff(
            &send,
            DiffRoute {
                diff: DiffUplink {
                    cell: CellId::ROOT,
                    grid: GridId::ROOT,
                    entity: PersistId::new(22),
                    tick: orrery_protocol::Tick::new(4),
                    kind: orrery_protocol::RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"state"),
                    seq: 4,
                    lease_id: None,
                    authority_seq: None,
                },
                authority_metrics: Arc::new(AuthorityMetrics::default()),
                author: iroh::SecretKey::from_bytes(&[2; 32]).public(),
                received_at: Instant::now(),
                strict_authority: false,
            },
            &router,
            &admission,
            &GatewayBulkMetrics::default(),
        )
        .await;
        let bytes = sent
            .lock()
            .expect("capture lock")
            .pop()
            .expect("bulk reply");
        assert!(matches!(
            decode_datagram(&bytes),
            Some(GatewayReply::BulkAck {
                provisional: true,
                ..
            })
        ));
        monitor.shutdown();
    }

    /// A router whose apply blocks until released, announcing each arrival.
    struct BlockingApplyRouter {
        entered: tokio::sync::mpsc::Sender<()>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl Router for BlockingApplyRouter {
        async fn apply(
            &self,
            _record: JournalRecord,
        ) -> Result<Arc<crate::journal::AppendHandle>, Reject> {
            self.entered
                .send(())
                .await
                .map_err(|_| Reject::JournalClosed)?;
            // A semaphore, not a `Notify`: `notify_waiters` stores no permit,
            // so a task that has not reached the await yet would miss the
            // wake and hang. Permits are stored, so the release cannot be lost.
            let permit = self
                .release
                .acquire()
                .await
                .map_err(|_| Reject::JournalClosed)?;
            permit.forget();
            Ok(crate::journal::AppendHandle::completed(Lsn::new(7, 11)))
        }

        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }

        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }
    }

    /// A router that records how many renewal round trips it was asked for,
    /// batched and unbatched, and can refuse one named pair.
    struct CountingRenewalRouter {
        /// Whether this router folds a batch, or leaves the trait default to
        /// fan the batch back out one entity at a time (the shape before the
        /// batched method existed).
        batched: bool,
        /// The shard cells this router hosts actors for. A real router folds a
        /// batch by the actor that owns each entry, and an actor owns a whole
        /// shard subtree — so this double resolves cells to shards with the
        /// *production* grouping function rather than inventing its own rule.
        shards: Vec<CellId>,
        turns: Arc<AtomicUsize>,
        batch_sizes: Arc<Mutex<Vec<usize>>>,
        refuse: Option<PersistId>,
        /// Answer with fewer rows than were asked for.
        short: bool,
        holder: NodeId,
    }

    impl CountingRenewalRouter {
        fn row(&self, entity: PersistId, lease_id: LeaseId) -> Option<orrery_protocol::Lease> {
            (self.refuse != Some(entity)).then_some(orrery_protocol::Lease {
                entity,
                holder: Some(self.holder),
                seq: SeqPair::default(),
                lease_id,
                expires_at: u64::MAX,
                flags: LeaseFlags::default(),
                bound_to: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl Router for CountingRenewalRouter {
        async fn apply(
            &self,
            _record: JournalRecord,
        ) -> Result<Arc<crate::journal::AppendHandle>, Reject> {
            Err(Reject::JournalClosed)
        }
        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }
        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }
        async fn heartbeat_lease(
            &self,
            _grid: GridId,
            _cell: CellId,
            entity: PersistId,
            _holder: NodeId,
            lease_id: LeaseId,
            _now_ms: u64,
        ) -> Result<Option<orrery_protocol::Lease>, Reject> {
            self.turns.fetch_add(1, Ordering::SeqCst);
            self.batch_sizes.lock().expect("sizes").push(1);
            Ok(self.row(entity, lease_id))
        }
        async fn heartbeat_leases(
            &self,
            grid: GridId,
            holder: NodeId,
            renew: &[LeaseRenewal],
            now_ms: u64,
        ) -> Result<Vec<Option<orrery_protocol::Lease>>, Reject> {
            if !self.batched {
                // Exactly what a router with no actor of its own does.
                let mut rows = Vec::with_capacity(renew.len());
                for entry in renew {
                    rows.push(
                        self.heartbeat_lease(
                            grid,
                            entry.cell,
                            entry.entity,
                            holder,
                            entry.lease_id,
                            now_ms,
                        )
                        .await?,
                    );
                }
                return Ok(rows);
            }
            let routes: Vec<CellId> = renew.iter().map(|entry| entry.cell).collect();
            let mut rows = vec![None; renew.len()];
            for (_shard, members) in crate::cluster::group_by_actor(&self.shards, &routes) {
                // One mailbox turn per owning actor: this is the instrument.
                self.turns.fetch_add(1, Ordering::SeqCst);
                self.batch_sizes.lock().expect("sizes").push(members.len());
                for index in members {
                    rows[index] = self.row(renew[index].entity, renew[index].lease_id);
                }
            }
            if self.short {
                // A genuinely truncated reply, not a padded one: the caller
                // must treat the missing tail as refused, not as absent rows.
                rows.truncate(1);
            }
            Ok(rows)
        }
    }

    fn session_leases(count: u64, cell: CellId) -> Vec<SessionLease> {
        (1..=count)
            .map(|id| SessionLease {
                entity: PersistId::new(id),
                lease_id: LeaseId(1),
                grid: GridId::ROOT,
                cell,
                owner: SessionLeaseOwner::Active(SessionGeneration(1)),
            })
            .collect()
    }

    /// One heartbeat from a peer holding N entities is one actor turn, not N.
    ///
    /// This is the turns-per-heartbeat instrument: `renew_session_leases` asks
    /// the router exactly once per `(grid, cell)` group, and the router folds
    /// each group into one mailbox turn. Measured here for a peer holding 50
    /// entities in one cell: **50 turns before, 1 after** — every 2.5 s,
    /// through one bounded mailbox, per peer.
    #[tokio::test]
    async fn one_heartbeat_from_a_holder_of_fifty_costs_one_actor_turn() {
        const HELD: u64 = 50;
        let holder = iroh::SecretKey::from_bytes(&[11; 32]).public();
        let leases = session_leases(HELD, CellId::ROOT);

        let mut measured = Vec::new();
        for batched in [false, true] {
            let turns = Arc::new(AtomicUsize::new(0));
            let router: Arc<dyn Router> = Arc::new(CountingRenewalRouter {
                batched,
                shards: vec![CellId::ROOT],
                turns: Arc::clone(&turns),
                batch_sizes: Arc::new(Mutex::new(Vec::new())),
                refuse: None,
                short: false,
                holder,
            });
            let (rows, invalid) = renew_session_leases(&router, holder, &leases, 0).await;
            assert_eq!(rows.len(), HELD as usize, "every held row is acked");
            assert!(invalid.is_empty(), "nothing was refused: {invalid:?}");
            measured.push(turns.load(Ordering::SeqCst));
        }
        assert_eq!(
            measured,
            vec![HELD as usize, 1],
            "one entity per turn before, one turn for the whole batch after"
        );
    }

    /// `count` leaf cells, all distinct, all inside `shard`.
    ///
    /// This is the shape the P2 criterion actually runs: one entity per leaf
    /// cell, every one of them owned by the same actor.
    fn leaf_cells_under(shard: CellId, count: usize) -> Vec<CellId> {
        let mut level = vec![shard];
        while level.len() < count {
            level = level
                .into_iter()
                .flat_map(|cell| cell.children().into_iter())
                .collect();
        }
        level.truncate(count);
        level
    }

    /// One entity per leaf cell — the P2 workload — is still one actor turn.
    ///
    /// The leaf cell is the wrong key to fold on: an actor owns a *shard*, and
    /// a shard holds very many leaf cells. Tonight's P2 evidence had 2079
    /// entities in 2079 distinct leaf cells, so a leaf-keyed batch is 2079
    /// groups of one and costs exactly what no batching costs. Folding by the
    /// owning actor instead: **50 turns before, 1 after**, on a workload where
    /// no two entities share a cell.
    #[tokio::test]
    async fn fifty_entities_in_fifty_distinct_cells_of_one_shard_cost_one_turn() {
        const HELD: usize = 50;
        let holder = iroh::SecretKey::from_bytes(&[15; 32]).public();
        let shard = CellId::ROOT.children()[0];
        let cells = leaf_cells_under(shard, HELD);
        assert_eq!(
            cells.iter().collect::<HashSet<_>>().len(),
            HELD,
            "the workload is one entity per cell, not one cell for all"
        );
        let leases: Vec<SessionLease> = cells
            .iter()
            .enumerate()
            .map(|(index, &cell)| SessionLease {
                entity: PersistId::new(index as u64 + 1),
                lease_id: LeaseId(1),
                grid: GridId::ROOT,
                cell,
                owner: SessionLeaseOwner::Active(SessionGeneration(1)),
            })
            .collect();

        let mut measured = Vec::new();
        for batched in [false, true] {
            let turns = Arc::new(AtomicUsize::new(0));
            let router: Arc<dyn Router> = Arc::new(CountingRenewalRouter {
                batched,
                shards: vec![shard],
                turns: Arc::clone(&turns),
                batch_sizes: Arc::new(Mutex::new(Vec::new())),
                refuse: None,
                short: false,
                holder,
            });
            let (rows, invalid) = renew_session_leases(&router, holder, &leases, 0).await;
            assert_eq!(rows.len(), HELD, "every held row is acked");
            assert!(invalid.is_empty(), "nothing was refused: {invalid:?}");
            measured.push(turns.load(Ordering::SeqCst));
        }
        assert_eq!(
            measured,
            vec![HELD, 1],
            "one turn per entity before, one turn for the whole shard after"
        );
    }

    /// Entities under different **shards** cannot share a turn — one per actor.
    ///
    /// The fold is by owning actor, so this is the boundary that still costs a
    /// turn, and a batch that straddles two shards is answered positionally
    /// across both.
    #[tokio::test]
    async fn a_heartbeat_spanning_two_shards_costs_one_turn_per_shard() {
        let holder = iroh::SecretKey::from_bytes(&[12; 32]).public();
        let shards = CellId::ROOT.children();
        let mut leases: Vec<SessionLease> = Vec::new();
        for (shard_index, shard) in shards[..2].iter().enumerate() {
            for (index, cell) in leaf_cells_under(*shard, 4).into_iter().enumerate() {
                leases.push(SessionLease {
                    entity: PersistId::new((shard_index * 100 + index) as u64 + 1),
                    lease_id: LeaseId(1),
                    grid: GridId::ROOT,
                    cell,
                    owner: SessionLeaseOwner::Active(SessionGeneration(1)),
                });
            }
        }
        let turns = Arc::new(AtomicUsize::new(0));
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let router: Arc<dyn Router> = Arc::new(CountingRenewalRouter {
            batched: true,
            shards: shards[..2].to_vec(),
            turns: Arc::clone(&turns),
            batch_sizes: Arc::clone(&sizes),
            refuse: None,
            short: false,
            holder,
        });

        let (rows, invalid) = renew_session_leases(&router, holder, &leases, 0).await;

        assert_eq!(turns.load(Ordering::SeqCst), 2);
        assert_eq!(*sizes.lock().expect("sizes"), vec![4, 4]);
        assert_eq!(rows.len(), 8);
        assert!(invalid.is_empty());
    }

    /// Batching must not blur the ack: one bad pair in a batch invalidates
    /// that pair and nothing else, so the holder stops writing exactly one
    /// entity and keeps writing the other 49.
    #[tokio::test]
    async fn a_refused_pair_inside_a_batch_is_acked_alone() {
        const HELD: u64 = 50;
        let holder = iroh::SecretKey::from_bytes(&[13; 32]).public();
        let refused = PersistId::new(7);
        let leases = session_leases(HELD, CellId::ROOT);
        let turns = Arc::new(AtomicUsize::new(0));
        let router: Arc<dyn Router> = Arc::new(CountingRenewalRouter {
            batched: true,
            shards: vec![CellId::ROOT],
            turns: Arc::clone(&turns),
            batch_sizes: Arc::new(Mutex::new(Vec::new())),
            refuse: Some(refused),
            short: false,
            holder,
        });

        let (rows, invalid) = renew_session_leases(&router, holder, &leases, 0).await;

        assert_eq!(turns.load(Ordering::SeqCst), 1);
        assert_eq!(invalid, vec![(refused, LeaseId(1))]);
        assert_eq!(rows.len(), HELD as usize - 1);
        assert!(rows.iter().all(|row| row.entity != refused));
    }

    /// A router that answers short has said nothing about the tail, and a
    /// silent tail is an entity the holder keeps writing while believing it is
    /// still the authority. Every unanswered pair is refused instead.
    #[tokio::test]
    async fn an_unanswered_tail_of_a_batch_is_refused_rather_than_dropped() {
        let holder = iroh::SecretKey::from_bytes(&[14; 32]).public();
        let leases = session_leases(4, CellId::ROOT);
        let router: Arc<dyn Router> = Arc::new(CountingRenewalRouter {
            batched: true,
            shards: vec![CellId::ROOT],
            turns: Arc::new(AtomicUsize::new(0)),
            batch_sizes: Arc::new(Mutex::new(Vec::new())),
            refuse: None,
            short: true,
            holder,
        });

        let (rows, invalid) = renew_session_leases(&router, holder, &leases, 0).await;

        assert_eq!(rows.len(), 1);
        assert_eq!(
            invalid,
            leases[1..]
                .iter()
                .map(|lease| (lease.entity, lease.lease_id))
                .collect::<Vec<_>>()
        );
    }

    struct AlwaysDurableAdmission;

    impl BulkAckAdmission for AlwaysDurableAdmission {
        fn assess(&self, _grid: GridId, _cell: CellId) -> BulkAckDisposition {
            BulkAckDisposition::Durable
        }
    }

    #[tokio::test]
    async fn session_diffs_do_not_serialize_on_the_peer_lock() {
        // A connection admits `MAX_INFLIGHT_DIFF_ROUTES_PER_CONN` concurrent
        // routes precisely so one durability wave does not stall the diffs
        // behind it. Holding the `PeerState` guard across the journal wait
        // quietly reduced that to one. This pins the concurrency rather than
        // the spelling, so re-introducing any guard over `route_diff` fails
        // here rather than in a load test months later.
        let registry = Arc::new(PeerRegistry::new(2, 10_000, MAX_PEER_LIVE_LEASES));
        let node = iroh::SecretKey::from_bytes(&[9; 32]).public();
        let authorization = GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
            orrery_protocol::AccountId::new(1),
            node,
            UnixMillis::new(0),
            orrery_protocol::SessionTokenTtlMs::new(1_000),
            orrery_protocol::SessionStanding::Good,
            orrery_protocol::IssuerKeyId::new(1),
        ));
        let session = registry
            .activate(node, authorization, b"conn", None, 0, 0)
            .await
            .expect("valid peer is admitted");

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(4);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let router: Arc<dyn Router> = Arc::new(BlockingApplyRouter {
            entered: entered_tx,
            release: Arc::clone(&release),
        });
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let bulk_metrics = Arc::new(GatewayBulkMetrics::default());

        let mut routes = Vec::new();
        for entity in [1u64, 2] {
            let session = session.clone();
            let router = Arc::clone(&router);
            let admission = Arc::clone(&admission);
            let bulk_metrics = Arc::clone(&bulk_metrics);
            routes.push(tokio::spawn(async move {
                let send = |_bytes: Bytes| {};
                route_session_diff(
                    &send,
                    DiffUplink {
                        cell: CellId::ROOT,
                        grid: GridId::ROOT,
                        entity: PersistId::new(entity),
                        tick: orrery_protocol::Tick::new(1),
                        kind: orrery_protocol::RecordKind::ComponentDiff,
                        payload: Bytes::from_static(b"state"),
                        seq: 1,
                        lease_id: None,
                        authority_seq: None,
                    },
                    &session,
                    &router,
                    &admission,
                    &bulk_metrics,
                    Arc::new(AuthorityMetrics::default()),
                    Instant::now(),
                )
                .await;
            }));
        }

        // Both diffs must be inside the journal at once. Serialized on the
        // peer lock the second never arrives, and this times out.
        for reached in 1..=2 {
            tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "only {} of 2 diffs reached the journal; the peer lock is serializing them",
                        reached - 1
                    )
                })
                .expect("router channel stays open");
        }

        release.add_permits(2);
        for route in routes {
            tokio::time::timeout(std::time::Duration::from_secs(10), route)
                .await
                .expect("routes finish once released")
                .expect("route task does not panic");
        }
        drop(registry);
    }
    /// One held entity resolves to one renewable row, not one per requested id.
    ///
    /// Resolving by bare `LeaseId` returned every held row for every requested
    /// id — 50 entities all sitting at `LeaseId(1)` produced 2500 sequential
    /// turns through one bounded cell mailbox every 2.5 s. This bounds the
    /// *batch*; `one_heartbeat_from_a_holder_of_fifty_costs_one_actor_turn`
    /// then counts the turns that batch actually costs.
    #[test]
    fn renewal_costs_one_round_trip_per_held_entity() {
        const HELD: u64 = 50;
        let generation = SessionGeneration(3);
        let mut leases = HashMap::new();
        let mut renew = Vec::new();
        for id in 1..=HELD {
            let entity = PersistId::new(id);
            // Every fresh claim mints LeaseId(1); this collision is the whole
            // reason the id alone cannot name a row.
            leases.insert(
                entity,
                SessionLease {
                    entity,
                    lease_id: LeaseId(1),
                    grid: GridId::ROOT,
                    cell: CellId::ROOT,
                    owner: SessionLeaseOwner::Active(generation),
                },
            );
            renew.push((entity, LeaseId(1)));
        }

        let (renewable, invalid) = resolve_renewals(&leases, generation, &renew);

        assert_eq!(
            renewable.len(),
            HELD as usize,
            "round trips must be O(N) in held entities, not O(N^2)"
        );
        assert!(invalid.is_empty(), "every held lease renews: {invalid:?}");
        let distinct: HashSet<_> = renewable.iter().map(|lease| lease.entity).collect();
        assert_eq!(distinct.len(), HELD as usize, "each entity resolves once");
    }

    /// A stale token refuses its own entity and nothing else.
    ///
    /// Under bare-id acking this was unexpressible: `invalid: [LeaseId(1)]`
    /// told the holder to drop every entity it happened to hold at that
    /// per-row counter value.
    #[test]
    fn a_stale_token_invalidates_only_its_own_entity() {
        let generation = SessionGeneration(1);
        let held: Vec<_> = (1..=3).map(PersistId::new).collect();
        let leases: HashMap<_, _> = held
            .iter()
            .map(|entity| {
                (
                    *entity,
                    SessionLease {
                        entity: *entity,
                        lease_id: LeaseId(1),
                        grid: GridId::ROOT,
                        cell: CellId::ROOT,
                        owner: SessionLeaseOwner::Active(generation),
                    },
                )
            })
            .collect();

        let (renewable, invalid) = resolve_renewals(
            &leases,
            generation,
            &[
                (held[0], LeaseId(1)),
                // Superseded token on a still-held entity.
                (held[1], LeaseId(2)),
                (held[2], LeaseId(1)),
                // Never held at all.
                (PersistId::new(99), LeaseId(1)),
            ],
        );

        assert_eq!(
            invalid,
            vec![(held[1], LeaseId(2)), (PersistId::new(99), LeaseId(1))]
        );
        let renewed: Vec<_> = renewable.iter().map(|lease| lease.entity).collect();
        assert_eq!(renewed, vec![held[0], held[2]]);
    }

    /// A batch repeating one pair still costs one turn.
    #[test]
    fn repeated_pairs_in_one_batch_cost_one_round_trip() {
        let generation = SessionGeneration(7);
        let entity = PersistId::new(4);
        let leases = HashMap::from([(
            entity,
            SessionLease {
                entity,
                lease_id: LeaseId(2),
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                owner: SessionLeaseOwner::Active(generation),
            },
        )]);

        let (renewable, invalid) =
            resolve_renewals(&leases, generation, &[(entity, LeaseId(2)); 16]);

        assert_eq!(renewable.len(), 1);
        assert!(invalid.is_empty());
    }

    /// Leases left over from a superseded session generation are refused.
    #[test]
    fn a_parking_generation_lease_does_not_renew() {
        let generation = SessionGeneration(2);
        let entity = PersistId::new(8);
        let leases = HashMap::from([(
            entity,
            SessionLease {
                entity,
                lease_id: LeaseId(1),
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                owner: SessionLeaseOwner::Parking(SessionGeneration(1)),
            },
        )]);

        let (renewable, invalid) = resolve_renewals(&leases, generation, &[(entity, LeaseId(1))]);

        assert!(renewable.is_empty());
        assert_eq!(invalid, vec![(entity, LeaseId(1))]);
    }
}
