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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
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
use crate::intent::stages::{self, intent_stage_metrics, IntentStageSnapshot, IntentTrace};
use crate::intent::{
    error_outcome, IntentContext, IntentVerdict, PermissiveValidator, SharedExecutor,
    SharedValidator,
};
use crate::lease::registrar_now_ms;
use crate::lease::stages::{elapsed_us as lease_stage_us, lease_stage_metrics, HeartbeatTrace};
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

/// How long a bulk diff may sit in the connection's inbound queue before the
/// receive loop drops it instead of routing it.
///
/// # Why a deadline and not a bigger cap
///
/// The measured defect (docs/11-roadmap.md §P2) was not sustained overload: a
/// 30 s run sent and acknowledged the same ~540 000 diffs, so arrival and
/// service rates matched. It was a *standing queue* — a transient (the claim
/// phase, whose lease work runs inline in this loop) built a backlog that
/// never drained, because at ~100 % utilisation there is no slack to drain it
/// with. A larger [`MAX_INFLIGHT_DIFF_ROUTES_PER_CONN`], or a faster route,
/// changes how long the backlog takes to form and nothing else. The only
/// thing that removes a standing queue is destroying work, which is what this
/// deadline does: it *is* the slack.
///
/// # Why 25 ms
///
/// D16 budgets the whole client-observed round trip at p99 < 5 ms. A diff
/// that has already spent five budgets waiting to be *looked at* cannot be
/// acknowledged usefully — and this lane is QUIC datagrams, so the honest
/// answer to "I cannot serve this in time" is the same one the network would
/// have given: drop it. The client holds one pending diff per entity and
/// resends until it is acked (`UplinkScheduler::flush`), so the write is not
/// lost, it is re-offered — usually as a *newer* tick, which is strictly
/// better than the stale one being dropped here.
///
/// Chosen well above the healthy p50 (0.05 ms) so it never fires on a
/// gateway that is keeping up: this is a shed valve, not a rate limit.
const MAX_INGRESS_QUEUE_WAIT_US: u64 = 25_000;

/// The same budget, enforced where the wait actually is.
///
/// [`MAX_INGRESS_QUEUE_WAIT_US`] is evaluated on a diff's arrival age at the
/// instant the receive loop dequeues it. That was a real bound while the loop
/// was the queue. It is not one now: with lease work moved onto its own lane
/// the loop is instant (`gateway_ingress_queue_ms` p99 0.05 ms), so the check
/// passes for everything and bounds nothing — and the standing queue did not
/// go away, it moved downstream of admission.
///
/// Measured on the merged branch (three gate runs, 30 s, 125 sessions,
/// 10 000 entities, 128 shards), per acknowledged diff:
///
/// | stage                      | mean      | max      |
/// |----------------------------|-----------|----------|
/// | `route_queue` (spawn→task) | 0.005 ms  | 0.45 ms  |
/// | `router_apply`             | 7.8–8.7 ms| 2.2 s    |
/// | ├ entity gate wait         | 7.2–8.2 ms| 2.2 s    |
/// | ├ `LeaseStore::locate`     | 0.40 ms   | 15 ms    |
/// | └ actor mailbox round trip | 0.006 ms  | 1.0 ms   |
/// | `journal_wait`             | 2.0–4.3 ms| 35–275 ms|
///
/// So it is not the tokio scheduler (`route_queue` is noise), not the actor
/// mailbox (0.006 ms — the actors are idle), and not primarily the committer.
/// It is the striped per-entity mutex inside `CellRuntime::apply_fenced`,
/// which is held across an FDB read and which the lease lane's batched
/// heartbeats take 77 at a time for 16 ms mean / 50 ms max
/// (`crate::cluster::RouteStageMetrics`). The head-of-line block did not go
/// away either; it moved from the receive loop onto the entity stripes, where
/// the offloaded lease lane can now contend with live diff traffic.
///
/// **That gate hold has since been removed** (docs/08-persistence.md §2.1.1:
/// `heartbeat_leases` resolves locations with no gate held and takes gates
/// per actor group around the mailbox turn alone). On the same gate the
/// entity-gate wait is now 0.011–0.015 ms mean / 5–21 ms max with this valve
/// **off**, and `shed_slow_route` was 0 on every run at every budget down to
/// 25 ms — this valve no longer fires on the P2 workload. It is retained as a
/// bound for workloads that study did not run, not because that one needs it.
///
/// A deadline only bounds a wait it is evaluated *after*, so this one is
/// evaluated around the whole router round trip, against the diff's age since
/// arrival — the same clock and the same budget as the ingress check, applied
/// where the time is actually spent.
///
/// **It stops at the journal, deliberately.** Once the actor has admitted the
/// record, the write is going to be durable; refusing to wait for the ack
/// after that would drop an acknowledgement for a write that happened, which
/// is a different and worse kind of dishonesty. `journal_wait` is therefore
/// outside this valve — and it is also frozen to another lane.
const MAX_ROUTE_ADMISSION_WAIT_US: u64 = 25_000;

/// Environment override for [`MAX_ROUTE_ADMISSION_WAIT_US`].
///
/// This valve has a genuine trade behind it (see docs/08-persistence.md
/// §3.6): the budget buys tail latency with shed rate, and where to sit on
/// that curve is an operator's call, not a compile-time one. Read once, so a
/// running node's policy cannot change under it.
const ROUTE_ADMISSION_WAIT_ENV: &str = "ORRERY_GATEWAY_MAX_ROUTE_WAIT_US";

/// The configured route-admission budget, in microseconds.
///
/// `0` disables the valve — which is exactly the merged branch's behaviour,
/// and therefore the "before" leg of any A/B against it.
fn route_admission_budget_us() -> u64 {
    static BUDGET: std::sync::LazyLock<u64> =
        std::sync::LazyLock::new(|| match std::env::var(ROUTE_ADMISSION_WAIT_ENV) {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(value) => value,
                Err(_) => {
                    warn!(
                        raw,
                        default = MAX_ROUTE_ADMISSION_WAIT_US,
                        "gateway: unparseable route-admission budget; using the default"
                    );
                    MAX_ROUTE_ADMISSION_WAIT_US
                }
            },
            Err(_) => MAX_ROUTE_ADMISSION_WAIT_US,
        });
    *BUDGET
}

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

/// How many lease operations one connection may have waiting on its lease
/// worker.
///
/// The lane is a FIFO of one, by construction (`serve_lease_message` explains
/// why the fencing protocol needs it to be), so this bounds only the *backlog*
/// — the memory a peer can make the gateway hold by offering lease work faster
/// than it can be served. Sized well above anything a correct client produces:
/// a holder heartbeats its whole lease set in one batched message every
/// `LEASE_TTL_MS / 2`, and claims are already limited per peer by
/// `PeerState::claim_bucket`, so reaching this depth means a peer is
/// misbehaving rather than busy.
const MAX_QUEUED_LEASE_OPS_PER_CONN: usize = 1_024;

/// What a claimant is told to wait when its own connection's lease lane is
/// full. Long enough that the retry lands after the backlog drained, short
/// enough not to read as a refusal.
const LEASE_LANE_RETRY_AFTER_MS: u32 = 50;

const MAX_PEER_REGISTRY_ENTRIES: usize = 4_096;

const MAX_PEER_LIVE_LEASES: usize = 256;

/// The most non-holder `Expire` advisories one expiry may produce (D25 rule 8).
///
/// D6's per-cell player ceiling, reused verbatim: a cell with more admitted
/// sessions than this is already past that ceiling, so the excess is dropped
/// rather than sent. Dropping happens in ascending `NodeId` order — not in
/// whatever order a `HashMap` walk produced — so the same expiry replayed
/// against the same session set drops the same peers. Without that clause an
/// over-cap run is not reproducible from its own inputs.
pub const EXPIRE_FANOUT_MAX_RECIPIENTS: usize = 128;

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

    /// Which of `sessions` cover `cell` in `grid` right now, capped at `limit`.
    ///
    /// This is D25's recipient set `A(G, grid, cell, t)` with the sessions
    /// term supplied by the caller, and the split is the seam D25 rule 3
    /// names: an authority knows *interest*, a gateway knows *who is
    /// addressable*, and a later cluster-wide session directory widens the
    /// second term without touching the first. There is no reverse
    /// `peers_covering(cell)` index anywhere in the system and D25 declined to
    /// build one — a coordinator grant's expiry is enforced on this read path
    /// and nowhere else, so a second structure would leak advisories to peers
    /// whose interest had lapsed.
    ///
    /// The default filters through [`InterestAuthority::allows`], the same
    /// predicate a live `Claim` and a successor nomination pass. That is not
    /// an incidental implementation choice an override may re-derive: fan-out
    /// eligibility *is* claim admission, so an override that disagreed would
    /// address a peer the gateway would refuse to grant to. An override exists
    /// to answer the same question with fewer locks, never with a different
    /// answer.
    ///
    /// Cost is `O(sessions.len())`, itself bounded by
    /// `MAX_PEER_REGISTRY_ENTRIES`, and callers are expected to pay it once
    /// per `(grid, cell)` rather than once per entity (D25 rule 8).
    #[must_use]
    fn covering_peers(
        &self,
        sessions: &[NodeId],
        grid: GridId,
        cell: CellId,
        now_ms: u64,
        limit: usize,
    ) -> CoveringPeers {
        let covering = sessions
            .iter()
            .copied()
            .filter(|peer| self.allows(*peer, grid, cell, now_ms))
            .collect::<Vec<_>>();
        CoveringPeers::bounded(covering, limit)
    }
}

/// The addressable audience for one cell's non-holder `Expire` advisories
/// (D25 rule 1).
///
/// Carrying the cut-off count rather than truncating silently is what makes
/// the bound observable. An `over_limit` that tracks a cell's population is a
/// cell past D6's ceiling — a capacity signal, not a fault — and one that
/// tracks nothing in particular is a cap that is inert, which is the reading
/// D25's open question asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoveringPeers {
    /// Peers covering the cell, in ascending `NodeId` order, truncated to the
    /// caller's limit.
    pub peers: Vec<NodeId>,
    /// How many covering peers the limit cut off.
    pub over_limit: usize,
}

impl CoveringPeers {
    /// Sort `covering` by `NodeId` and keep at most `limit` of them.
    ///
    /// Sorting before truncating is the whole point: an unordered truncation
    /// of a registry walk drops a different subset on every pass, so a run
    /// that exceeded the cap could not be reproduced from the same inputs.
    #[must_use]
    pub fn bounded(mut covering: Vec<NodeId>, limit: usize) -> Self {
        covering.sort_unstable_by_key(|peer| *peer.as_bytes());
        let over_limit = covering.len().saturating_sub(limit);
        covering.truncate(limit);
        Self {
            peers: covering,
            over_limit,
        }
    }

    /// How many peers are actually addressed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether nobody addressable covers the cell.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
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

    /// Answer the whole cell in one read lock instead of one per peer.
    ///
    /// The predicate is character-for-character the default's — this is the
    /// same map, read the same way — and the only thing the override buys is
    /// that `snapshot_for` is not called once per session, each call taking
    /// the lock and *cloning* a `covered_cells` vector that is discarded a
    /// line later. At `MAX_PEER_REGISTRY_ENTRIES` sessions that is 4 096 lock
    /// acquisitions and 4 096 allocations per `(grid, cell)`; here it is one
    /// and none.
    fn covering_peers(
        &self,
        sessions: &[NodeId],
        grid: GridId,
        cell: CellId,
        now_ms: u64,
        limit: usize,
    ) -> CoveringPeers {
        let Ok(held) = self.snapshots.read() else {
            // A poisoned map is not evidence that anybody covers the cell,
            // and an advisory is best-effort by construction (D25 rule 9), so
            // the safe answer is to address nobody.
            return CoveringPeers::default();
        };
        let covering = sessions
            .iter()
            .copied()
            .filter(|peer| {
                held.get(peer).is_some_and(|snapshot| {
                    snapshot.peer == *peer
                        && snapshot.grid == grid
                        && snapshot.valid_until_ms > now_ms
                        && snapshot.covered_cells.contains(&cell)
                })
            })
            .collect::<Vec<_>>();
        CoveringPeers::bounded(covering, limit)
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
    /// Bulk diffs whose presented cell is not the one this session's lease
    /// for that entity is indexed at.
    ///
    /// Not a fencing event, and not necessarily an error: a registrar-driven
    /// `commit_rekey` moves an entity without telling the gateway, and the
    /// holder's first write at the new cell counts here once before the index
    /// is repaired. It is a **capacity** number. Such a diff misses the
    /// fenced route's fast path and pays one `LeaseStore::locate` plus a
    /// second mailbox turn — the pre-change FoundationDB cost, plus a turn —
    /// and the cell it turns on arrives on the wire. See `locate_fallbacks`
    /// in `crate::cluster::RouteStageSnapshot`.
    ///
    /// Healthy shape: small, and **not** growing with a peer's diff rate. A
    /// value that tracks a peer's throughput is a peer addressing an entity
    /// at a cell it will never be admitted at.
    ///
    /// Counts **both** shapes the index fails to confirm: an entry naming a
    /// different cell, and no entry at all. The second used to be invisible
    /// here, which made this counter blind to the one vector that needs no
    /// lease to drive — see `unindexed_diffs`.
    pub misrouted_diffs: u64,
    /// The subset of `misrouted_diffs` where the session's lease index held
    /// **no** entry for the entity, as opposed to an entry naming another
    /// cell.
    ///
    /// Split out because the two have different healthy values and different
    /// causes. A rekey drives `misrouted_diffs` and leaves this at zero: the
    /// entry exists throughout, only its cell moves. This one has no
    /// legitimate producer at all — `complete_lease_claim` inserts the entry
    /// before either `LeaseMsg::Grant` emitter sends it, and every removal is
    /// paired with a `park_lease` that already makes the router reject — so
    /// **any** sustained value is a peer writing at entities it holds no
    /// lease for. Unlike the rekey case it is never repaired, so it settles
    /// at `MisrouteBucket::PROBES_PER_SECOND` rather than decaying to zero.
    pub unindexed_diffs: u64,
    /// Those of `misrouted_diffs` (either shape) that were answered with a
    /// `BulkNack` without routing, because the connection's probe bucket was
    /// empty.
    ///
    /// Zero on a healthy node, including through a rekey: the bucket bursts
    /// to 256 and one admitted probe repairs the index. Nonzero means one
    /// connection is presenting routes that are never admitted, fast enough
    /// to exhaust its allowance — and if `unindexed_diffs` is moving with it,
    /// that connection is doing so at entities it holds no lease for.
    pub misroute_throttled: u64,
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
    /// Non-holder `Expire` advisories actually pushed to a peer's live
    /// connection (D25 rule 1).
    ///
    /// Purely additive to the disposition counters above: every increment
    /// here accompanies an `Expire` the losing holder was already sent (or
    /// would have been, had it still been reachable), and none of it changes
    /// `reassigned` or `parked_without_successor`.
    ///
    /// Healthy shape: roughly `parked_without_successor × |A|` for the cells
    /// that actually park, and **zero** across a field host's disconnect,
    /// whose leases reassign and which D25 rule 7 keeps holder-only.
    pub expire_fanout_sent: u64,
    /// Recipients that covered the cell but had no live connection left to
    /// push to by the time the advisory was addressed.
    ///
    /// Enumeration and delivery are not one atomic step — a session can end
    /// between them — so this is an ordinary race rather than a fault. A
    /// value that tracks `expire_fanout_sent` means the gateway is enumerating
    /// sessions it can no longer reach, which is a peer-registry eviction
    /// question and not a fan-out one.
    pub expire_fanout_skipped: u64,
    /// Advisories dropped by a bound: over the per-expiry recipient cap, or
    /// refused by a recipient's own egress bucket (D25 rules 8 and 9).
    ///
    /// Dropping is safe *because* the advisory is an optimisation: a
    /// recipient that loses one falls back to exactly the pre-D25 behaviour —
    /// the entity stops being written, its proxy decays, and any peer that
    /// cares issues a `Claim` and gets the authoritative `Deny{Parked}`.
    /// Queueing instead would put a hint in front of `Grant` and `Deny` on the
    /// same lane, degrading the one thing on this path that is *not*
    /// best-effort.
    ///
    /// A count that tracks a cell's population is a cell past D6's ceiling; a
    /// count that tracks one peer is that peer's bucket doing its job.
    pub expire_fanout_dropped: u64,
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
    misrouted_diffs: AtomicU64,
    unindexed_diffs: AtomicU64,
    misroute_throttled: AtomicU64,
    reassigned: AtomicU64,
    parked_without_successor: AtomicU64,
    divested: AtomicU64,
    divest_rejected: AtomicU64,
    divest_requested: AtomicU64,
    handoff_timed_out: AtomicU64,
    expire_fanout_sent: AtomicU64,
    expire_fanout_skipped: AtomicU64,
    expire_fanout_dropped: AtomicU64,
    last_duplicate: std::sync::Mutex<Option<DuplicateAuthoritySample>>,
}

impl AuthorityMetrics {
    /// Read every counter.
    #[must_use]
    pub fn snapshot(&self) -> AuthoritySnapshot {
        AuthoritySnapshot {
            duplicate_authority: self.duplicate_authority.load(Ordering::Relaxed),
            misrouted_diffs: self.misrouted_diffs.load(Ordering::Relaxed),
            unindexed_diffs: self.unindexed_diffs.load(Ordering::Relaxed),
            misroute_throttled: self.misroute_throttled.load(Ordering::Relaxed),
            reassigned: self.reassigned.load(Ordering::Relaxed),
            parked_without_successor: self.parked_without_successor.load(Ordering::Relaxed),
            divested: self.divested.load(Ordering::Relaxed),
            divest_rejected: self.divest_rejected.load(Ordering::Relaxed),
            divest_requested: self.divest_requested.load(Ordering::Relaxed),
            handoff_timed_out: self.handoff_timed_out.load(Ordering::Relaxed),
            expire_fanout_sent: self.expire_fanout_sent.load(Ordering::Relaxed),
            expire_fanout_skipped: self.expire_fanout_skipped.load(Ordering::Relaxed),
            expire_fanout_dropped: self.expire_fanout_dropped.load(Ordering::Relaxed),
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

    /// One bulk diff whose route this session's lease index does not confirm:
    /// either it names another cell, or it names nothing.
    fn record_misrouted_diff(&self) {
        self.misrouted_diffs.fetch_add(1, Ordering::Relaxed);
    }

    /// One of those where the index held no entry for the entity at all.
    /// Always paired with `record_misrouted_diff`, never on its own.
    fn record_unindexed_diff(&self) {
        self.unindexed_diffs.fetch_add(1, Ordering::Relaxed);
    }

    /// One of those refused a probe because the connection's bucket was empty.
    fn record_misroute_throttled(&self) {
        self.misroute_throttled.fetch_add(1, Ordering::Relaxed);
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

    /// One non-holder `Expire` copy handed to a live connection.
    fn record_expire_fanout_sent(&self) {
        self.expire_fanout_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// One recipient that covered the cell but had no live session left.
    fn record_expire_fanout_skipped(&self) {
        self.expire_fanout_skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// `count` advisories a bound refused. Called with the whole over-cap
    /// remainder at once, and with `1` for a recipient whose bucket is empty.
    fn record_expire_fanout_dropped(&self, count: u64) {
        if count == 0 {
            return;
        }
        self.expire_fanout_dropped
            .fetch_add(count, Ordering::Relaxed);
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

/// Server-side transport-boundary series: the two spans that sit *outside*
/// [`SERIES_GATEWAY_BULK_SERVER`]'s receipt-through-send-call measurement.
///
/// # Why these exist
///
/// A P2 evidence run put `client_bulk_wire_ms` p99 at 2104 ms against a
/// `gateway_bulk_server_ms` p99 of 150 ms. Both numbers are honest and they
/// describe the same round trip, so roughly 1 950 ms of it was in neither —
/// it sat between the rig's socket write and the instant the gateway's own
/// span *starts*, or between the gateway's send call and the reply reaching
/// the rig. Those are different subsystems, and a report that cannot name
/// which one owns the gap sends the reader to guess.
///
/// The gateway's measured span begins at `received_at`, stamped in the
/// connection's receive loop after a message has been taken off the inbound
/// queue and decoded. Between the endpoint driver handing a datagram to
/// [`spawn_datagram_reader`] and that stamp there is one unbounded queue and
/// one serialized loop, and until now nothing measured either. That is what
/// [`SERIES_GATEWAY_INGRESS_QUEUE`] is.
///
/// # Why the names are spelled here and not in `orrery_protocol::metrics`
///
/// They belong beside `UNGATED_SERIES`, next to `gateway_bulk_server_ms`,
/// for exactly the reason that array's doc comment gives: one definition
/// shared by the producer and the consumer. They are declared here only
/// because `orrery_protocol` is frozen to another lane. The consumer half is
/// `orrery_persist_client::latency::GATEWAY_BOUNDARY_SERIES`, whose doc
/// comment carries the same note.
///
/// The guard against the two spellings drifting is not a unit test — neither
/// crate can see the other, so any test either could write would compare a
/// literal to itself. It is the gate: `p2-dashboard` folds by series name and
/// reports anything it does not recognize under `unknown_series_names`, and
/// `scripts/p2-kill9-gate.sh` fails the run when that list is non-empty. A
/// typo here does not silently vanish; it fails P2.
///
/// # None of them is gated
///
/// D16 sets no target for any of them, and they carry the `gateway_*_ms`
/// shape the P2 harness refuses to see gated. They are attribution.
///
/// Endpoint-driver dequeue of an inbound datagram through the instant the
/// connection's receive loop picks that message up — the gateway's own
/// ingress backlog, upstream of every span it already measures.
pub const SERIES_GATEWAY_INGRESS_QUEUE: &str = "gateway_ingress_queue_ms";

/// The age a bulk diff had already reached when the receive loop refused it.
///
/// Separate from [`SERIES_GATEWAY_INGRESS_QUEUE`] on purpose. Both are "time
/// spent in the inbound queue", but they answer different questions, and
/// pooling them made the ingress series stop meaning anything: once refusals
/// exist, a histogram over *every* arrival measures the age of a backlog being
/// destroyed rather than the delay the gateway imposed on work it actually
/// did, so it no longer predicts client-observed latency. It is the shed
/// records that carry the long waits, by construction — they are shed *for*
/// waiting.
pub const SERIES_GATEWAY_SHED_AGE: &str = "gateway_shed_age_ms";

/// A reply being handed to the transport through the instant the transport's
/// send call returns: the gateway's hand-off cost, and nothing after it.
///
/// `quinn::Connection::send_datagram` enqueues into the endpoint driver's
/// datagram buffer and returns; it does not wait for the packet to leave the
/// NIC. So this span closes the *near* half of the egress boundary and
/// [`SERIES_GATEWAY_SEND_BUFFER`] observes the far half.
pub const SERIES_GATEWAY_REPLY_HANDOFF: &str = "gateway_reply_handoff_ms";

/// Bytes resident in the connection's outbound QUIC datagram buffer at the
/// moment a reply has just been handed to it — **bytes, not microseconds**,
/// recorded on the shared bucket lattice because its range (50 B … 1 MiB) is
/// the range that lattice covers.
///
/// This is the measurement that decides the endpoint-driver question. A QUIC
/// datagram send can sit in the driver's queue without that ever appearing in
/// the path RTT, because the RTT estimate is computed from ACK timing on
/// packets that *did* go out. If that queue is where the missing round-trip
/// time lives, this gauge is non-zero; if it reads zero at p99, the driver
/// accepted and drained every datagram immediately and the time is elsewhere.
/// Asserting either without this number is guessing.
pub const SERIES_GATEWAY_SEND_BUFFER: &str = "gateway_send_buffer_bytes";

/// The four transport-boundary series, in canonical report order.
pub const GATEWAY_BOUNDARY_SERIES: [&str; 4] = [
    SERIES_GATEWAY_INGRESS_QUEUE,
    SERIES_GATEWAY_SHED_AGE,
    SERIES_GATEWAY_REPLY_HANDOFF,
    SERIES_GATEWAY_SEND_BUFFER,
];

/// The transport-boundary histograms, sharing the D16 bucket lattice with
/// every other gateway span so a percentile means the same thing across them.
#[derive(Debug, Default)]
pub struct GatewayBoundaryMetrics {
    /// [`SERIES_GATEWAY_INGRESS_QUEUE`]. Admitted messages only.
    pub ingress_queue: GatewayServerLatency,
    /// [`SERIES_GATEWAY_SHED_AGE`]. Refused bulk diffs only.
    pub shed_age: GatewayServerLatency,
    /// [`SERIES_GATEWAY_REPLY_HANDOFF`].
    pub reply_handoff: GatewayServerLatency,
    /// [`SERIES_GATEWAY_SEND_BUFFER`], in bytes.
    pub send_buffer: GatewayServerLatency,
}

impl GatewayBoundaryMetrics {
    /// Record one inbound message's wait between transport dequeue and the
    /// receive loop picking it up.
    pub fn record_ingress(&self, micros: u64) {
        self.ingress_queue.record(micros);
    }

    /// Record the age of a bulk diff the receive loop refused.
    ///
    /// Deliberately not folded into [`Self::record_ingress`]: a refused diff
    /// waited, but it was never served, and a series that mixes the two stops
    /// being a predictor of what the client sees.
    pub fn record_shed_age(&self, micros: u64) {
        self.shed_age.record(micros);
    }

    /// Record one reply hand-off: `micros` on the send call, and the datagram
    /// buffer occupancy observed straight after it.
    pub fn record_reply(&self, micros: u64, buffered_bytes: u64) {
        self.reply_handoff.record(micros);
        self.send_buffer.record(buffered_bytes);
    }

    /// The four histograms paired with their series names, in report order.
    ///
    /// A slice rather than three accessors so a reporter drains them in a
    /// loop: the one that matters lives in `persistd`'s binary, which this
    /// lane may not edit, and a caller-side loop is what makes wiring it up a
    /// single added line rather than three.
    #[must_use]
    pub fn series(&self) -> [(&'static str, &GatewayServerLatency); 4] {
        [
            (SERIES_GATEWAY_INGRESS_QUEUE, &self.ingress_queue),
            (SERIES_GATEWAY_SHED_AGE, &self.shed_age),
            (SERIES_GATEWAY_REPLY_HANDOFF, &self.reply_handoff),
            (SERIES_GATEWAY_SEND_BUFFER, &self.send_buffer),
        ]
    }
}

/// What the receive loop did with every bulk diff it took off the inbound
/// queue: routed it, or refused it and said so.
///
/// # Why refusals are counted rather than queued
///
/// [`SERIES_GATEWAY_INGRESS_QUEUE`] exists because the gateway used to accept
/// work it could not route in time and hide the delay: the datagram reader
/// pushed into an unbounded channel that the receive loop stopped draining
/// whenever the connection's route cap was saturated, so a client saw 2 s
/// while the gateway's own span read 30 ms. Every honest answer to that is
/// one of three — route faster, refuse visibly, or push back on the peer —
/// and the bulk lane's contract (unreliable datagrams, idempotent
/// `(entity, tick)` records, client-side resend; docs/08-persistence.md §2.1)
/// makes refusal the cheapest correct one. These counters are what makes it
/// *visible*: a silent drop is exactly as dishonest as a hidden queue, so
/// overload now reads as a number here and in the operator log instead of as
/// latency in someone else's histogram.
///
/// Both refusals drop the datagram without a reply, deliberately. A
/// `BulkNack` means "this write was rejected, do not resend it"
/// (`UplinkScheduler::on_nack`, docs/08-persistence.md §3.5) and would
/// discard the peer's pending diff; silence leaves it pending, so the client
/// re-offers it on its own cadence. Un-acked is the one state the bulk
/// contract already defines as lossy — "the unacked tail is lost by design
/// (bulk class)", docs/08-persistence.md §9 — and P2's criterion promises
/// RPO 0 for *acked* intents and "bulk loss bounded by the journal/
/// replication window", never delivery of something never acknowledged.
#[derive(Debug, Default)]
pub struct GatewayIngressMetrics {
    admitted: AtomicU64,
    shed_saturated: AtomicU64,
    shed_stale: AtomicU64,
    shed_slow_route: AtomicU64,
}

impl GatewayIngressMetrics {
    /// One diff dispatched to a route task (or routed inline).
    pub fn record_admitted(&self) {
        self.admitted.fetch_add(1, Ordering::Relaxed);
    }

    /// One diff dropped because every route slot on this connection was busy.
    pub fn record_shed_saturated(&self) {
        self.shed_saturated.fetch_add(1, Ordering::Relaxed);
    }

    /// One diff dropped because it had already waited past
    /// [`MAX_INGRESS_QUEUE_WAIT_US`] in the inbound queue.
    pub fn record_shed_stale(&self) {
        self.shed_stale.fetch_add(1, Ordering::Relaxed);
    }

    /// One diff dropped because the router could not admit it to a journal
    /// within [`MAX_ROUTE_ADMISSION_WAIT_US`] of its arrival.
    ///
    /// Counted apart from [`Self::record_shed_stale`] for the reason that
    /// enum has three arms at all: "it aged out before I looked at it" and
    /// "it aged out while I was routing it" are different subsystems, and an
    /// operator reading one number must not have to guess which queue grew.
    pub fn record_shed_slow_route(&self) {
        self.shed_slow_route.fetch_add(1, Ordering::Relaxed);
    }

    /// Capture the cumulative totals.
    #[must_use]
    pub fn snapshot(&self) -> GatewayIngressSnapshot {
        GatewayIngressSnapshot {
            admitted: self.admitted.load(Ordering::Relaxed),
            shed_saturated: self.shed_saturated.load(Ordering::Relaxed),
            shed_stale: self.shed_stale.load(Ordering::Relaxed),
            shed_slow_route: self.shed_slow_route.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time read of [`GatewayIngressMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayIngressSnapshot {
    /// Diffs the receive loop routed.
    pub admitted: u64,
    /// Diffs dropped with every route slot busy.
    pub shed_saturated: u64,
    /// Diffs dropped for having outlived [`MAX_INGRESS_QUEUE_WAIT_US`] in the
    /// inbound queue.
    pub shed_stale: u64,
    /// Diffs dropped for having outlived [`MAX_ROUTE_ADMISSION_WAIT_US`]
    /// *downstream* of admission, waiting on the router.
    ///
    /// This one overlaps `admitted` on purpose and is not a subtraction bug:
    /// the receive loop admitted the diff and the route task then refused it,
    /// so a run's served count is `admitted - shed_slow_route`, while
    /// `shed_saturated` and `shed_stale` are disjoint from `admitted`.
    ///
    /// **Read this counter's history before reading a number from it.** Every
    /// nonzero reading taken between #86 and 2026-08-19 — which is all of
    /// docs/14-capacity.md §11's 73-point study — was the sampled invariant-J
    /// audit, not route slowness. The audit was awaited inside
    /// `apply_fenced`, therefore inside the timeout below, so a sampled diff
    /// whose audit read overran the budget was cancelled and counted here.
    /// The identity `shed_slow_route == (decided audits) - (completed audits)`
    /// held exactly at all 73 points, on both storage engines, over three
    /// orders of magnitude of shed rate. The audit is detached now
    /// (`crate::cluster::CellRuntime::begin_location_audit`) and this counter
    /// means what it says again, but a historical JSONL does not.
    pub shed_slow_route: u64,
}

impl GatewayIngressSnapshot {
    /// Every diff this gateway refused, whatever the reason.
    #[must_use]
    pub fn shed(&self) -> u64 {
        self.shed_saturated
            .saturating_add(self.shed_stale)
            .saturating_add(self.shed_slow_route)
    }
}

/// What the receive loop did with every lease operation it took off the
/// inbound queue: queued it on this connection's lease lane, or refused it
/// because the lane was already full.
///
/// Counted for the same reason as [`GatewayIngressMetrics`]: the refusal path
/// drops two of the three lease kinds without a reply (see
/// `refuse_saturated_lease` for why silence is the honest answer there), and a
/// silent drop that is not also a number is indistinguishable from a stall.
#[derive(Debug, Default)]
pub struct GatewayLeaseMetrics {
    queued: AtomicU64,
    refused: AtomicU64,
}

impl GatewayLeaseMetrics {
    /// One lease operation handed to a connection's lease worker.
    pub fn record_queued(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
    }

    /// One lease operation refused with the connection's lane at
    /// [`MAX_QUEUED_LEASE_OPS_PER_CONN`].
    pub fn record_refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Capture the cumulative totals.
    #[must_use]
    pub fn snapshot(&self) -> GatewayLeaseSnapshot {
        GatewayLeaseSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time read of [`GatewayLeaseMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayLeaseSnapshot {
    /// Lease operations dispatched to a lease worker.
    pub queued: u64,
    /// Lease operations refused with the connection's lane full.
    pub refused: u64,
}

/// The receive loop's verdict on one inbound diff.
///
/// A three-way enum rather than an `Option<permit>` because the two refusals
/// are different operational events — a saturated route cap is a slow
/// downstream, a stale queue entry is a burst this connection has not yet
/// worked off — and an operator reading one counter must not have to guess
/// which happened.
enum DiffAdmission {
    /// Route it; the permit bounds the connection's concurrent routes.
    Route(OwnedSemaphorePermit),
    /// Refuse: it has waited longer than the ack could still be worth.
    ShedStale,
    /// Refuse: every route slot on this connection is occupied.
    ShedSaturated,
}

/// Decide whether one diff is routed, on the two bounds that matter, without
/// ever awaiting.
///
/// **Never make this `async`.** The whole defect was one `.await` here: the
/// receive loop parked on `Semaphore::acquire_owned` while the datagram
/// reader kept filling an unbounded channel behind it, which is how a 30 ms
/// server span became a 2 s client observation. `reserve_intent_lane` avoids
/// the same trap on the intent lane for the same reason, and this is the bulk
/// lane's version of it. Split out so the policy is directly testable and so
/// a future `.await` has to be added deliberately rather than by editing a
/// long match arm.
///
/// Staleness is checked *before* the permit, so a diff nobody can still use
/// does not consume a route slot a fresh one could have had — which is how
/// the backlog gets worked off rather than merely re-ordered.
fn admit_diff_route(lane: &Arc<Semaphore>, ingress_queue_us: u64) -> DiffAdmission {
    if ingress_queue_us > MAX_INGRESS_QUEUE_WAIT_US {
        return DiffAdmission::ShedStale;
    }
    match Arc::clone(lane).try_acquire_owned() {
        Ok(permit) => DiffAdmission::Route(permit),
        // `NoPermits` is the saturation case. `Closed` cannot happen — this
        // semaphore is never closed — and if it ever did, refusing is the
        // same correct answer, because no route could be spawned either way.
        Err(_) => DiffAdmission::ShedSaturated,
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
    /// Transport-boundary spans that bracket [`GatewayBulkMetrics`]'s own.
    pub boundary: GatewayBoundaryMetrics,
    /// Bulk-ingress admission: routed, or refused and counted.
    pub ingress: GatewayIngressMetrics,
    /// Lease-lane admission: queued off the receive loop, or refused.
    pub lease: GatewayLeaseMetrics,
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
    /// How long, from a diff's arrival, the router has to admit it to a
    /// journal before the gateway sheds it ([`MAX_ROUTE_ADMISSION_WAIT_US`]).
    /// Zero disables the valve.
    ///
    /// Config rather than a process global so a test can state the policy it
    /// is testing — `tests/gateway_ingress.rs` needs the valve *off* to hold
    /// a connection at its route cap — and so a second gateway in one process
    /// is not silently bound to the first one's environment. The default
    /// still reads [`ROUTE_ADMISSION_WAIT_ENV`], so an operator moving the
    /// operating point needs no code and persistd's own frozen
    /// `..GatewayConfig::default()` construction needs no edit.
    pub route_admission_wait_us: u64,
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
            route_admission_wait_us: route_admission_budget_us(),
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

/// One renewal this node could not answer because it hosts no shard over the
/// cell the lease sits in — carried out of the batch so the heartbeat reply
/// can name the reason instead of only the refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MisaddressedRenewal {
    entity: PersistId,
    grid: GridId,
    cell: CellId,
    epoch: Epoch,
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
///
/// The third list is narrower than `invalid` and additive to it: the renewals
/// that failed *because this node hosts no shard over their cell*, so the
/// caller can say why (docs/08-persistence.md §3.5) without changing what the
/// ack itself refuses.
async fn renew_session_leases(
    router: &Arc<dyn Router>,
    holder: NodeId,
    renewable: &[SessionLease],
    now_ms: u64,
) -> (
    Vec<orrery_protocol::Lease>,
    Vec<(PersistId, LeaseId)>,
    Vec<MisaddressedRenewal>,
) {
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
    // Every pair that did not renew, with the cell it was presented at, so the
    // classification below can ask about the cell rather than guess from the
    // entity. Refusals only — the happy path adds nothing to this list and
    // pays nothing for it.
    let mut refused_at: Vec<(PersistId, GridId, CellId)> = Vec::new();
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
            refused_at.extend(batch.iter().map(|entry| (entry.entity, grid, entry.cell)));
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
                        refused_at.push((lease.entity, grid, lease.cell));
                    }
                }
                None => {
                    invalid.push((lease.entity, lease.lease_id));
                    refused_at.push((lease.entity, grid, lease.cell));
                }
            }
        }
        // A router that answers short has said nothing about the tail, and a
        // silent tail is exactly the ack blur batching must not introduce.
        for lease in members.iter().skip(answered) {
            invalid.push((lease.entity, lease.lease_id));
            refused_at.push((lease.entity, grid, lease.cell));
        }
    }
    // Classified *after* the refusals are decided, and only over them.
    //
    // The refusals themselves are untouched: every pair that did not renew is
    // still named individually in `invalid`, because batching must not blur
    // the ack (docs/04-authority.md §3) and a holder has to learn exactly
    // which entity it may no longer write. What this adds is the *reason*, for
    // the one class where the refusal is not about the lease at all — the
    // shard the lease sits in is not hosted here, so re-addressing is the
    // response and standing down is not.
    //
    // Asked of the router rather than inferred from the router's answer,
    // because the batched renewal path deliberately degrades an unroutable
    // entry to a per-entry `None` (`CellRuntime::heartbeat_leases`) rather
    // than failing a whole batch that may straddle owned and unowned cells.
    // A `None` therefore carries no reason of its own, and one lookup per
    // *refused* renewal is what recovers it.
    let mut misaddressed = Vec::new();
    for (entity, grid, cell) in refused_at {
        if let Some(epoch) = router.wrong_owner_epoch(grid, cell).await {
            misaddressed.push(MisaddressedRenewal {
                entity,
                grid,
                cell,
                epoch,
            });
        }
    }
    (rows, invalid, misaddressed)
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
    misroute_bucket: MisrouteBucket,
    expire_fanout_bucket: ExpireFanoutBucket,
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

    /// Spend one token from this peer's `Expire`-advisory egress budget.
    ///
    /// Taken against the `NodeId`'s own state rather than this generation's,
    /// for the reason [`ExpireFanoutBucket`] records: reconnecting must not
    /// refill the allowance. A `false` here is a *drop*, not a deferral.
    async fn take_expire_fanout_token(&self, now_ms: u64) -> bool {
        let mut peer = self.state.lock().await;
        peer.expire_fanout_bucket.take(now_ms)
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
                    misroute_bucket: MisrouteBucket::new(claim_now_ms),
                    expire_fanout_bucket: ExpireFanoutBucket::new(claim_now_ms),
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
    /// The clock the per-recipient advisory buckets are metered on.
    ///
    /// The *same* clock the per-peer claim bucket uses, because the two limits
    /// are two ends of one path and a test that freezes one must freeze both;
    /// `registrar_now_ms` is process uptime and injectable by nothing.
    claim_clock: SharedClaimClock,
    pending: tokio::sync::Mutex<HashMap<(PersistId, NodeId), PendingHandoff>>,
}

/// Addressable audiences memoised for the length of one redistribution pass.
///
/// D25 rule 8's third limit: the covering set is enumerated **once per
/// `(grid, cell)`**, never once per entity. The difference is the one that
/// bites before bandwidth does. Enumerating walks the whole peer registry,
/// locking every entry's mutex, so a lost peer holding `MAX_PEER_LIVE_LEASES`
/// rows would cost `256 × 4 096 ≈ 1.05 M` lock-and-check operations in a
/// single `cleanup_peer_session` pass. Per `(grid, cell)` the same pass is
/// bounded by the cells one grant may cover, `MAX_INTEREST_GRANT_CELLS = 64`,
/// for `64 × 4 096 ≈ 262 K` — and on the reassignment path it is zero extra,
/// because `place` already produced the set as its candidate list.
///
/// Deliberately **not** a field on [`Redistributor`]: this is a pass-scoped
/// memo, and a long-lived one would answer with sessions that have since gone
/// and grants that have since lapsed. It is created by the loop that parks and
/// dropped when that loop ends.
///
/// The cached value is the *unfiltered* covering set. One sweep pass carries
/// leases from several holders, so the peer to exclude varies within a pass
/// while `A(G, grid, cell, t)` does not; folding the exclusion into the key
/// would defeat the memo exactly when a cell is busiest.
#[derive(Debug, Default)]
struct ExpireAudiences {
    by_cell: HashMap<(GridId, CellId), Vec<NodeId>>,
}

/// What [`Redistributor::place`] decided, and what it already enumerated.
///
/// The candidate set is carried out of `place` rather than recomputed by the
/// caller because it *is* D25's `A(G, grid, cell, t)` minus the previous
/// holder — the same walk, through the same `allows` seam — so fan-out on the
/// redistribution path costs no enumeration the registrar was not already
/// paying (D25 rule 1).
struct Placement {
    disposition: orrery_protocol::ExpireDisposition,
    /// The vetted candidates, when `place` got far enough to compute them.
    ///
    /// `None` means the row was never offered at all — `STRONG_HELD` or
    /// `PLAYER_BOUND` returns before any enumeration happens — and that is
    /// precisely the case where an audience must still be found, because a
    /// strong-owned entity parks with a live audience watching it.
    candidates: Option<Vec<NodeId>>,
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

    /// Everyone this gateway may hand a non-holder `Expire` for `cell`.
    ///
    /// D25's `A(G, grid, cell, t) \ {exclude}`, enumerated the only way a
    /// registrar can: `Sessions(G, t)` comes from the peer registry — peers
    /// with a live authenticated session and a current generation on **this**
    /// gateway — and the interest predicate is applied to it through the
    /// `InterestAuthority` seam.
    ///
    /// This is a strict subset of D5's interest set, and D25 rule 2 names what
    /// it leaves out: peers on sibling gateways (no cluster-wide session
    /// directory yet — the same sentence `candidates` records above), peers
    /// whose grant lapsed between refreshes while still rendering the cell,
    /// and pure mesh peers that never talk to this registrar at all. Widening
    /// happens at the `Sessions` term and nowhere else.
    ///
    /// Called once per `(grid, cell)`, never once per entity: on the
    /// redistribution path `place` has already produced this set as its
    /// candidate list, and this method is the fallback for the one disposition
    /// that never computes one.
    async fn fanout_audience(
        &self,
        audiences: &mut ExpireAudiences,
        grid: GridId,
        cell: CellId,
        exclude: NodeId,
        now_ms: u64,
    ) -> CoveringPeers {
        let covering = match audiences.by_cell.get(&(grid, cell)) {
            Some(cached) => cached.clone(),
            None => {
                let sessions = self
                    .peers
                    .live_peer_leases()
                    .await
                    .into_iter()
                    .map(|(node, _)| node)
                    .collect::<Vec<_>>();
                // No cap here: the memo holds `A` itself, and the cap belongs
                // to one expiry's recipient list rather than to the cell's
                // membership. Capping first would make the drop count depend
                // on which entity happened to be enumerated first.
                let covering = self
                    .interest
                    .covering_peers(&sessions, grid, cell, now_ms, usize::MAX)
                    .peers;
                audiences.by_cell.insert((grid, cell), covering.clone());
                covering
            }
        };
        CoveringPeers::bounded(
            covering
                .into_iter()
                .filter(|node| *node != exclude)
                .collect(),
            EXPIRE_FANOUT_MAX_RECIPIENTS,
        )
    }

    /// Push one advisory copy of `message` to every peer in `audience`.
    ///
    /// The message is reused **verbatim** — same `entity`, same `lease_id`,
    /// same `disposition` as the losing holder's copy — which is what makes
    /// this deployable ahead of any client change (D25 rule 4): a client that
    /// has not learned to read a non-holder copy already drops one, because it
    /// has no installed lease for that entity to match the token against.
    ///
    /// Every refusal on this path is a **drop**. Queueing an advisory would
    /// put a hint in front of `Grant`, `Deny` and `HeartbeatAck` on the same
    /// lane, so a fan-out storm would degrade arbitration — the one thing here
    /// that is not best-effort (D25 rule 9).
    async fn fan_out_expire(&self, audience: CoveringPeers, message: &LeaseMsg) {
        // The over-cap remainder is counted before anything is sent: those
        // recipients were dropped by the cap, not by their own buckets, and
        // conflating the two would hide a cell past D6's ceiling behind a peer
        // that is merely busy.
        self.metrics
            .record_expire_fanout_dropped(audience.over_limit as u64);
        let now_ms = self.claim_clock.now_ms();
        for node in audience.peers {
            let Some(session) = self.peers.current_session(node).await else {
                // Enumeration and delivery are not one atomic step; a session
                // can end in between. An ordinary race, not a fault.
                self.metrics.record_expire_fanout_skipped();
                continue;
            };
            if !session.take_expire_fanout_token(now_ms).await {
                self.metrics.record_expire_fanout_dropped(1);
                continue;
            }
            if session
                .notify(&GatewayReply::Lease {
                    message: message.clone(),
                })
                .await
            {
                self.metrics.record_expire_fanout_sent();
            } else {
                self.metrics.record_expire_fanout_skipped();
            }
        }
    }

    /// Whether a disposition has any self-healing path of its own.
    ///
    /// `Reassigned` does and is therefore holder-only (D25 rule 7): INV-4
    /// converges every observer on the successor's first replicated envelope,
    /// because the successor's grant bumped the pair (INV-2), so an advisory
    /// buys at most one send interval — 50 ms at D16's 20 Hz — in the case
    /// that already heals itself. `Parked` and `Free` have no successor
    /// stream, nothing ever raises the pair, and the advisory is the *only*
    /// mechanism by which an observer stops extrapolating a proxy of an entity
    /// no node writes.
    ///
    /// This asymmetry is also what makes the bound tractable rather than
    /// merely smaller: a field host's disconnect reassigns its whole working
    /// set, so the worst case in the system fans out approximately nothing.
    const fn fans_out(disposition: &orrery_protocol::ExpireDisposition) -> bool {
        matches!(
            disposition,
            orrery_protocol::ExpireDisposition::Parked | orrery_protocol::ExpireDisposition::Free
        )
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
            //
            // Holder-only, and deliberately so: a timed-out handoff always
            // ends `Reassigned`, which D25 rule 7 excludes from fan-out
            // because INV-4 converges every observer on the claimant's first
            // replicated envelope — its grant bumped the pair — so an
            // advisory here would buy one send interval and nothing else.
            // The `else` arm below sends no `Expire` at all, to anybody,
            // which is pre-existing behaviour this change does not disturb:
            // there is no expiry message to fan out.
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
    async fn redistribute(
        &self,
        router: &Arc<dyn Router>,
        parked: crate::lease::ParkedLease,
        audiences: &mut ExpireAudiences,
    ) {
        let entity = parked.lease.entity;
        let Placement {
            disposition,
            candidates,
        } = self.place(router, &parked).await;
        match &disposition {
            orrery_protocol::ExpireDisposition::Reassigned { .. } => {
                self.metrics.record_reassigned()
            }
            _ => self.metrics.record_parked_without_successor(),
        }
        let fans_out = Self::fans_out(&disposition);
        let message = LeaseMsg::Expire {
            entity,
            lease_id: parked.previous_lease_id,
            last_holder: Some(parked.previous_holder),
            reason: parked.reason,
            disposition,
        };
        // Tell the losing holder first, addressed by the token it still
        // believes it has installed — parking already bumped the row's own
        // `lease_id` past it. On a disconnect there is nobody left to tell;
        // on a TTL sweep this is what stops a silent zombie from writing
        // again. Either way the copies below go out regardless, which is the
        // whole point: the disconnect case is exactly the one where the
        // holder's own copy is undeliverable and every survivor's is not.
        if let Some(session) = self.peers.current_session(parked.previous_holder).await {
            session
                .notify(&GatewayReply::Lease {
                    message: message.clone(),
                })
                .await;
        }
        if !fans_out {
            return;
        }
        // Reuse the set `place` already walked wherever it produced one — it
        // *is* `A` minus the previous holder, computed through the same seam
        // (D25 rule 1). The `None` arm is the strong-owned and player-bound
        // case, which returns before any enumeration and is the very case
        // where the advisory does the most work: a strong-owned entity parks
        // with a live audience and no successor stream will ever repoint it.
        let audience = match candidates {
            Some(nodes) => CoveringPeers::bounded(nodes, EXPIRE_FANOUT_MAX_RECIPIENTS),
            None => {
                self.fanout_audience(
                    audiences,
                    parked.grid,
                    parked.cell,
                    parked.previous_holder,
                    registrar_now_ms(),
                )
                .await
            }
        };
        self.fan_out_expire(audience, &message).await;
    }

    /// Decide and enact the disposition of one parked row.
    async fn place(
        &self,
        router: &Arc<dyn Router>,
        parked: &crate::lease::ParkedLease,
    ) -> Placement {
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
            // No candidate set is computed here, and `Placement::candidates`
            // says so rather than reporting an empty one: "nobody covers this
            // cell" and "nobody was asked" are different facts, and the
            // fan-out path has to tell them apart.
            return Placement {
                disposition: orrery_protocol::ExpireDisposition::Parked,
                candidates: None,
            };
        }
        let candidates = self
            .candidates(
                parked.grid,
                parked.cell,
                parked.previous_holder,
                registrar_now_ms(),
            )
            .await;
        let addressable = || {
            Some(
                candidates
                    .iter()
                    .map(|candidate| candidate.node)
                    .collect::<Vec<_>>(),
            )
        };
        if candidates.is_empty() {
            // D25's load-bearing case: `candidates` *is* `A` minus the
            // previous holder, so an empty one says `|A \ {P}| = 0` and the
            // fan-out term for this entity is zero by construction. A lease
            // parks for want of a successor only when there is nobody to tell.
            return Placement {
                disposition: orrery_protocol::ExpireDisposition::Parked,
                candidates: addressable(),
            };
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
            return Placement {
                disposition: orrery_protocol::ExpireDisposition::Parked,
                candidates: addressable(),
            };
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
            Placement {
                // Holder-only by D25 rule 7; the candidate set is carried
                // anyway so the caller never has to know which arm produced
                // the disposition it is looking at.
                disposition: orrery_protocol::ExpireDisposition::Reassigned { to: successor },
                candidates: addressable(),
            }
        } else {
            // The handoff failed after the row was parked. The audience is
            // real and there is now no successor stream, so this parks *and*
            // fans out — the case D25's arithmetic calls `declined(P)`.
            Placement {
                disposition: orrery_protocol::ExpireDisposition::Parked,
                candidates: addressable(),
            }
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
            claim_clock: Arc::clone(&admission.claim_clock),
            pending: tokio::sync::Mutex::new(HashMap::new()),
        });
        let lease_sweep_clock = config.lease_sweep_clock;
        let route_admission_wait_us = config.route_admission_wait_us;
        // One limiter per gateway, not per connection: the limit is per
        // account (docs/07 §7) and an account may hold several connections.
        let report_limiter = Arc::new(ReportLimiter::new());
        let (shutdown, rx) = oneshot::channel();
        let send_failures = Arc::new(AtomicU64::new(0));
        spawn_boundary_reporter(Arc::clone(&metrics));
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
            route_admission_wait_us,
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

/// Environment variable naming a JSONL file the transport-boundary series are
/// appended to. Unset — the default — collects the histograms in memory and
/// writes nothing, exactly as every other gateway counter behaves without
/// `--metrics-jsonl`.
///
/// # This is a bridge, and it should not survive
///
/// The right home for this is `persistd`'s own reporter loop, beside
/// `write_gateway_server_latency`, which already takes a series name and a
/// [`GatewayServerLatency`] and would need one added call over
/// [`GatewayBoundaryMetrics::series`] — see
/// `crates/orrery_persistd/src/bin/persistd.rs:345` (the function) and its
/// call sites in the reporter loop. That file is frozen to another lane while
/// this measurement is being taken, and a boundary you cannot read is a
/// boundary you cannot attribute, so the histograms get their own opt-in sink
/// here instead. When `bin/` is writable, move the drain into the reporter,
/// delete this and its environment variable, and nothing else changes: the
/// records emitted are byte-for-byte the `sample_batch` contract
/// `p2-dashboard` already parses.
pub const BOUNDARY_JSONL_ENV: &str = "ORRERY_GATEWAY_BOUNDARY_JSONL";

/// How often the boundary sink drains. Short enough that a run terminated
/// between ticks loses a quarter second of samples, long enough that the
/// drain itself is not the thing being measured.
const BOUNDARY_REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// The record kind carrying [`GatewayIngressMetrics`] in the boundary sink.
///
/// Not a `sample_batch`, deliberately: these are cumulative counts, not a
/// latency population, and folding them into a histogram would invent a
/// percentile out of a total. `p2-dashboard` ignores record kinds it does not
/// know (its `Record` documents exactly that) and counts unrecognized
/// *series* names, so a new kind is additive — it rides the same merged
/// artifact without touching the frozen gate.
const INGRESS_RECORD_KIND: &str = "gateway_ingress";

/// The lease-lane admission counters, on the same additive-record footing as
/// [`INGRESS_RECORD_KIND`].
const LEASE_RECORD_KIND: &str = "gateway_lease";

/// The fenced-apply stage decomposition, on the same additive-record footing.
///
/// `gateway_bulk_stage_delta` (persistd's own reporter, frozen to this lane)
/// reports the whole of `Router::apply_fenced` as one `router_apply` number.
/// This record splits that number into the striped entity gate, the
/// `LeaseStore::locate` read, and the actor round trip — see
/// [`crate::cluster::RouteStageMetrics`].
const ROUTE_STAGE_RECORD_KIND: &str = "gateway_route_stage";

/// The intent-path stage decomposition, on the same additive-record footing.
///
/// Emitted **twice** per interval: `"scope":"all"` over every intent that got
/// a definitive reply, and `"scope":"slow"` over only those whose server span
/// exceeded [`crate::intent::stages::slow_threshold_us`]. The second is the
/// point — a p99 cannot be read out of a mean, and the tail's own stage
/// decomposition is what attributes it. See [`crate::intent::stages`] for the
/// denominators, which are **not** the same for the gateway stages and the
/// FDB stages.
const INTENT_STAGE_RECORD_KIND: &str = "gateway_intent_stage";

/// The single slowest intent of a report interval, stage by stage.
///
/// A mean over the tail still averages; this is one real sample, so a 150 ms
/// intent can be read off directly rather than reconstructed.
const INTENT_EXEMPLAR_RECORD_KIND: &str = "gateway_intent_exemplar";

/// Append the transport-boundary histograms to [`BOUNDARY_JSONL_ENV`]'s file
/// on a fixed interval, as `sample_batch` records, plus the bulk-ingress
/// admission counters as [`INGRESS_RECORD_KIND`] records.
///
/// A no-op when the variable is unset or the file cannot be opened: telemetry
/// never takes a gateway down, and the counters keep accumulating either way.
/// Shedding is *also* logged here rather than only at the drop site: a
/// per-datagram warning under overload is a log flood, and one line per drain
/// interval carrying the totals is the same information an operator can
/// actually read. This is the "overload is a number, not a latency" half of
/// the contract in [`GatewayIngressMetrics`] — the JSONL half only exists
/// when a sink is configured, the log line always does.
/// Render one [`IntentStageSnapshot`] as an [`INTENT_STAGE_RECORD_KIND`]
/// record.
///
/// Written field by field rather than through a serializer so the JSONL stays
/// dependency-free and every key is greppable in this file — the same choice
/// the route-stage record above makes.
fn intent_stage_fields(scope: &str, d: &IntentStageSnapshot) -> String {
    let mut out = format!("{{\"type\":\"{INTENT_STAGE_RECORD_KIND}\",\"scope\":\"{scope}\"");
    for (key, value) in [
        ("intents", d.intents),
        ("executed", d.executed),
        ("attempts", d.attempts),
        ("alloc_refills", d.alloc_refills),
        ("fence_reads", d.fence_reads),
        ("ingress_us_sum", d.ingress_us_sum),
        ("ingress_us_max", d.ingress_us_max),
        ("admit_us_sum", d.admit_us_sum),
        ("admit_us_max", d.admit_us_max),
        ("spawn_wait_us_sum", d.spawn_wait_us_sum),
        ("spawn_wait_us_max", d.spawn_wait_us_max),
        ("exec_us_sum", d.exec_us_sum),
        ("exec_us_max", d.exec_us_max),
        ("alloc_wait_us_sum", d.alloc_wait_us_sum),
        ("alloc_wait_us_max", d.alloc_wait_us_max),
        ("alloc_refill_us_sum", d.alloc_refill_us_sum),
        ("alloc_refill_us_max", d.alloc_refill_us_max),
        ("grv_us_sum", d.grv_us_sum),
        ("grv_us_max", d.grv_us_max),
        ("idem_read_us_sum", d.idem_read_us_sum),
        ("idem_read_us_max", d.idem_read_us_max),
        ("fence_us_sum", d.fence_us_sum),
        ("fence_us_max", d.fence_us_max),
        ("fence_read_max_us", d.fence_read_max_us),
        ("commit_us_sum", d.commit_us_sum),
        ("commit_us_max", d.commit_us_max),
        ("backoff_us_sum", d.backoff_us_sum),
        ("backoff_us_max", d.backoff_us_max),
        ("server_us_sum", d.server_us_sum),
        ("server_us_max", d.server_us_max),
        ("reply_us_sum", d.reply_us_sum),
        ("reply_us_max", d.reply_us_max),
        ("server_gap_us_sum", d.server_gap_us_sum),
        ("server_gap_us_max", d.server_gap_us_max),
        ("fdb_gap_us_sum", d.fdb_gap_us_sum),
        ("fdb_gap_us_max", d.fdb_gap_us_max),
    ] {
        out.push_str(&format!(",\"{key}\":{value}"));
    }
    out.push_str("}\n");
    out
}

/// Render one intent's whole trace as an [`INTENT_EXEMPLAR_RECORD_KIND`]
/// record, including both derived gaps so a reader never has to subtract.
fn intent_exemplar_record(t: &IntentTrace) -> String {
    let mut out = format!("{{\"type\":\"{INTENT_EXEMPLAR_RECORD_KIND}\"");
    for (key, value) in [
        ("server_us", t.server_us),
        ("ingress_us", t.ingress_us),
        ("admit_us", t.admit_us),
        ("spawn_wait_us", t.spawn_wait_us),
        ("exec_us", t.exec_us),
        ("alloc_wait_us", t.alloc_wait_us),
        ("alloc_refill_us", t.alloc_refill_us),
        ("grv_us", t.grv_us),
        ("idem_read_us", t.idem_read_us),
        ("fence_us", t.fence_us),
        ("fence_read_max_us", t.fence_read_max_us),
        ("fence_reads", t.fence_reads),
        ("commit_us", t.commit_us),
        ("backoff_us", t.backoff_us),
        ("attempts", t.attempts),
        ("last_err_code", t.last_err_code),
        ("reply_us", t.reply_us),
        ("server_gap_us", t.server_gap_us()),
        ("fdb_gap_us", t.fdb_gap_us()),
    ] {
        out.push_str(&format!(",\"{key}\":{value}"));
    }
    out.push_str("}\n");
    out
}

fn spawn_boundary_reporter(metrics: Arc<GatewayMetrics>) {
    // The sink is optional; the reporter is not. Shed counters reach the log
    // on every node, configured sink or none, because the deployment that
    // most needs to hear "I am dropping writes" is the one nobody remembered
    // to point at a JSONL file.
    let file = match std::env::var(BOUNDARY_JSONL_ENV) {
        Err(_) => None,
        Ok(path) => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => Some(file),
            Err(e) => {
                warn!(
                    ?e,
                    path, "gateway: boundary metrics sink could not be opened"
                );
                None
            }
        },
    };
    tokio::spawn(async move {
        use std::io::Write as _;
        let mut file = file;
        let mut cursors: [GatewayServerLatencySnapshot; GATEWAY_BOUNDARY_SERIES.len()] =
            std::array::from_fn(|_| GatewayServerLatencySnapshot::default());
        let mut ingress_cursor = GatewayIngressSnapshot::default();
        let mut lease_cursor = GatewayLeaseSnapshot::default();
        let route_stages = crate::cluster::route_stage_metrics();
        let mut route_stage_cursor = crate::cluster::RouteStageSnapshot::default();
        let intent_stages = intent_stage_metrics();
        let mut intent_all_cursor = IntentStageSnapshot::default();
        let mut intent_slow_cursor = IntentStageSnapshot::default();
        loop {
            tokio::time::sleep(BOUNDARY_REPORT_INTERVAL).await;
            let ingress = metrics.ingress.snapshot();
            let lease = metrics.lease.snapshot();
            // Warned, not debugged: a gateway that refused a durable write is
            // an operational event even when refusing is the correct answer,
            // and it is the event this admission path exists to make sayable.
            // Totals, so a reader never has to add up interval deltas to
            // learn how much a run shed.
            if ingress.shed() != ingress_cursor.shed() {
                warn!(
                    shed_saturated = ingress.shed_saturated,
                    shed_stale = ingress.shed_stale,
                    shed_slow_route = ingress.shed_slow_route,
                    admitted = ingress.admitted,
                    "gateway: shedding bulk diffs at ingress"
                );
            }
            if lease.refused != lease_cursor.refused {
                warn!(
                    refused = lease.refused,
                    queued = lease.queued,
                    "gateway: refusing lease operations, lane full"
                );
            }
            let Some(sink) = file.as_mut() else {
                ingress_cursor = ingress;
                lease_cursor = lease;
                continue;
            };
            let mut out = String::new();
            for ((series, latency), cursor) in metrics
                .boundary
                .series()
                .into_iter()
                .zip(cursors.iter_mut())
            {
                for sample in latency.delta(cursor) {
                    out.push_str(&format!(
                        "{{\"type\":\"sample_batch\",\"series\":\"{}\",\"value_us\":{},\"count\":{}}}\n",
                        series, sample.value_us, sample.count
                    ));
                }
            }
            // Cumulative totals rather than an interval delta, and emitted
            // whenever any of them moved: a run's shed count is then the last
            // such record, not a sum a reader has to reconstruct.
            if ingress != ingress_cursor {
                out.push_str(&format!(
                    "{{\"type\":\"{}\",\"admitted\":{},\"shed_saturated\":{},\"shed_stale\":{},\"shed_slow_route\":{}}}\n",
                    INGRESS_RECORD_KIND,
                    ingress.admitted,
                    ingress.shed_saturated,
                    ingress.shed_stale,
                    ingress.shed_slow_route
                ));
            }
            if lease != lease_cursor {
                out.push_str(&format!(
                    "{{\"type\":\"{}\",\"queued\":{},\"refused\":{}}}\n",
                    LEASE_RECORD_KIND, lease.queued, lease.refused
                ));
            }
            // An interval delta, not a total, and deliberately unlike the two
            // above: these are sums over a varying number of applies, so only
            // a delta divided by its own `applies` is a mean anyone can read.
            // Check your denominators: `applies` is per fenced apply, not per
            // acknowledgement and not per flush.
            let route_stage = route_stages.snapshot();
            let route_delta = route_stage.delta(route_stage_cursor);
            route_stage_cursor = route_stage;
            if route_delta.applies > 0 || route_delta.batch_locks > 0 {
                out.push_str(&format!(
                    "{{\"type\":\"{}\",\"applies\":{},\"gate_wait_us_sum\":{},\"gate_wait_us_max\":{},\"locate_us_sum\":{},\"locate_us_max\":{},\"mailbox_us_sum\":{},\"mailbox_us_max\":{},\"batch_locks\":{},\"batch_gates_sum\":{},\"batch_hold_us_sum\":{},\"batch_hold_us_max\":{},\"mailbox_turns\":{},\"locate_fallbacks\":{},\"location_audits_decided\":{},\"location_audits\":{},\"location_mismatches\":{},\"location_audit_errors\":{},\"location_audits_dropped\":{},\"location_audit_us_sum\":{},\"location_audit_us_max\":{}}}\n",
                    ROUTE_STAGE_RECORD_KIND,
                    route_delta.applies,
                    route_delta.gate_wait_us_sum,
                    route_delta.gate_wait_us_max,
                    route_delta.locate_us_sum,
                    route_delta.locate_us_max,
                    route_delta.mailbox_us_sum,
                    route_delta.mailbox_us_max,
                    route_delta.batch_locks,
                    route_delta.batch_gates_sum,
                    route_delta.batch_hold_us_sum,
                    route_delta.batch_hold_us_max,
                    route_delta.mailbox_turns,
                    route_delta.locate_fallbacks,
                    route_delta.location_audits_decided,
                    route_delta.location_audits,
                    route_delta.location_mismatches,
                    route_delta.location_audit_errors,
                    route_delta.location_audits_dropped,
                    route_delta.location_audit_us_sum,
                    route_delta.location_audit_us_max,
                ));
            }
            // Same delta discipline as the route stages above, and the same
            // warning about denominators: gateway stages divide by `intents`,
            // FDB stages by `executed`. The `slow` scope carries the tail's own
            // decomposition; the exemplar carries one real tail sample.
            let intent_all = intent_stages.all.snapshot();
            let intent_slow = intent_stages.slow.snapshot();
            let all_delta = intent_all.delta(intent_all_cursor);
            let slow_delta = intent_slow.delta(intent_slow_cursor);
            intent_all_cursor = intent_all;
            intent_slow_cursor = intent_slow;
            if all_delta.intents > 0 {
                out.push_str(&intent_stage_fields("all", &all_delta));
                if slow_delta.intents > 0 {
                    out.push_str(&intent_stage_fields("slow", &slow_delta));
                }
                if let Some(exemplar) = intent_stages.take_exemplar() {
                    out.push_str(&intent_exemplar_record(&exemplar));
                }
            }
            ingress_cursor = ingress;
            lease_cursor = lease;
            if out.is_empty() {
                continue;
            }
            if let Err(e) = sink.write_all(out.as_bytes()).and_then(|()| sink.flush()) {
                warn!(?e, "gateway: boundary metrics sink write failed");
                return;
            }
        }
    });
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
    route_admission_wait_us: u64,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                // Expiry and redistribution are one step: a swept row that is
                // parked and then left is exactly the orphan the phase exists
                // to eliminate.
                // One memo per sweep, dropped with it: a sweep is a pass in
                // D25 rule 8's sense, and several of its rows routinely share
                // a cell — a busy cell is exactly where several leases lapse
                // together — so enumerating that cell's audience once is the
                // difference between `O(leases × sessions)` and
                // `O(cells × sessions)`.
                let mut audiences = ExpireAudiences::default();
                for parked in router.sweep_expired_leases(lease_sweep_clock.now_ms()).await {
                    redistributor.redistribute(&router, parked, &mut audiences).await;
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
                    route_admission_wait_us,
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
    route_admission_wait_us: u64,
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
    // Every inbound message carries the instant the transport handed it over,
    // so the receive loop can measure its own backlog (D16 attribution;
    // `SERIES_GATEWAY_INGRESS_QUEUE`).
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::unbounded_channel::<(Bytes, Instant)>();
    // The reliable receiver writes plain `Bytes` and lives outside this file,
    // so its stamp is taken by a one-hop forwarder here instead. Control
    // traffic is not the bulk path and the extra hop costs it nothing that
    // matters; the bulk lane is stamped at `read_datagram` itself, below.
    let (reliable_tx, mut reliable_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    reliable::spawn_receiver(Arc::clone(&conn), remote, reliable_tx);
    {
        let inbound_tx = inbound_tx.clone();
        tokio::spawn(async move {
            while let Some(pkt) = reliable_rx.recv().await {
                if inbound_tx.send((pkt, Instant::now())).is_err() {
                    return;
                }
            }
        });
    }
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
        let metrics = Arc::clone(&metrics);
        // Read once, here, while the connection's datagram buffer is still
        // empty: `datagram_send_buffer_space` reports the space *left*, and
        // the configured size is not otherwise observable. Sampling it after
        // the first send would build the baseline out of an already-occupied
        // buffer and understate every later occupancy.
        let datagram_buffer_capacity =
            u64::try_from(conn.datagram_send_buffer_space()).unwrap_or(u64::MAX);
        Arc::new(move |bytes: Bytes| {
            if matches!(untag(&bytes), Some((Channel::Control, _))) {
                reliable.send(reliable::Lane::Control, bytes);
                return;
            }
            let len = bytes.len();
            // The hand-off boundary, measured on both halves: how long the
            // transport took to accept the datagram, and how much is already
            // queued in the endpoint driver behind it. `send_datagram`
            // returns as soon as the driver has buffered the payload, so the
            // occupancy — not the call — is what says whether the driver is
            // where a reply waits.
            let handed_at = Instant::now();
            let outcome = conn.send_datagram(bytes);
            let handoff_us = elapsed_us(handed_at);
            let buffered = datagram_buffer_capacity
                .saturating_sub(u64::try_from(conn.datagram_send_buffer_space()).unwrap_or(0));
            metrics.boundary.record_reply(handoff_us, buffered);
            if let Err(e) = outcome {
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
    // The lease lane. One worker, one queue, strictly FIFO: it is the whole
    // point that this is not a spawn per message — see `serve_lease_message`
    // for the ordering the fencing protocol requires and where that is read
    // from. The receive loop's only remaining cost for a lease operation is a
    // channel push.
    let (lease_tx, lease_worker) = spawn_lease_lane(LeaseContext {
        send: Arc::clone(&send),
        router: Arc::clone(&router),
        redistributor: Arc::clone(&redistributor),
        interest_authority: Arc::clone(&interest_authority),
        claim_clock: Arc::clone(&admission.claim_clock),
        remote,
    });
    let inflight_diffs = Arc::new(Semaphore::new(MAX_INFLIGHT_DIFF_ROUTES_PER_CONN));
    let inflight_control = Arc::new(Semaphore::new(MAX_INFLIGHT_CONTROL_ROUTES_PER_CONN));
    let inflight_intents = Arc::new(Semaphore::new(MAX_INFLIGHT_INTENT_ROUTES_PER_CONN));
    let mut session: Option<PeerSession> = None;

    // Both lanes feed one queue, so the dispatch below is written once and does
    // not care which lane a message arrived on. The channel closes when both
    // feeder tasks have ended, which is how a torn connection ends this loop.
    loop {
        let Some((pkt, transport_at)) = inbound_rx.recv().await else {
            debug!(%remote, "gateway: connection closed");
            break;
        };
        // Measured before the decode and before any per-variant stamp, so it
        // is exactly the wait this loop imposed and contains none of the work
        // the loop then does.
        //
        // Recorded where the OUTCOME is known, not here. A diff that is about
        // to be refused did wait, but it was never served, and a histogram
        // over every arrival therefore measures the age of a backlog being
        // destroyed rather than the delay the gateway imposed on work it
        // actually did — which is precisely how this series stopped predicting
        // client-observed latency once refusals landed. Refused diffs go to
        // `gateway_shed_age_ms` instead; see the two shed arms below.
        let ingress_queue_us = elapsed_us(transport_at);
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
        // Served by definition — only the bulk lane refuses. Diffs record in
        // their own arm, once admission has decided.
        if !matches!(msg, GatewayMsg::Diff { .. }) {
            metrics.boundary.record_ingress(ingress_queue_us);
        }
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
                let Some(active_session) = session.clone() else {
                    continue;
                };
                // Handed to this connection's lease worker instead of being
                // served here. The queue is what preserves the ordering the
                // fencing protocol depends on; `serve_lease_message` records
                // which ordering that is and where it is relied on.
                //
                // `try_send`, never `send().await`: waiting for lane capacity
                // would put the head-of-line block back, one indirection
                // further down, which is the same mistake the intent lane
                // documents at `reserve_intent_lane`.
                match lease_tx.try_send(LeaseWork {
                    session: active_session,
                    message,
                }) {
                    Ok(()) => metrics.lease.record_queued(),
                    Err(mpsc::error::TrySendError::Full(work)) => {
                        metrics.lease.record_refused();
                        warn!(
                            %remote,
                            cap = MAX_QUEUED_LEASE_OPS_PER_CONN,
                            "gateway: lease lane saturated"
                        );
                        refuse_saturated_lease(send.as_ref(), &work.message);
                    }
                    // The worker outlives this loop by construction — it is
                    // joined below, after the loop ends — so a closed lane
                    // means the worker task itself is gone and there is
                    // nothing left to serve the operation with.
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        warn!(%remote, "gateway: lease worker is gone");
                    }
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
                // Admission is decided here, in the loop, and never waits.
                // Waiting for a route slot is what made this loop the
                // producer of its own backlog: the datagram reader behind it
                // never stops, so every microsecond parked here is another
                // datagram accumulating in memory with an ageing stamp.
                match admit_diff_route(&inflight_diffs, ingress_queue_us) {
                    DiffAdmission::Route(permit) => {
                        metrics.ingress.record_admitted();
                        metrics.boundary.record_ingress(ingress_queue_us);
                        let send = Arc::clone(&send);
                        let router = Arc::clone(&router);
                        let bulk_ack_admission = Arc::clone(&bulk_ack_admission);
                        let metrics = Arc::clone(&metrics);
                        let authority_metrics = Arc::clone(&redistributor.metrics);
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_session_diff(
                                send.as_ref(),
                                diff,
                                &active_session,
                                &router,
                                &bulk_ack_admission,
                                &metrics.bulk,
                                &metrics.ingress,
                                authority_metrics,
                                received_at,
                                route_admission_wait_us,
                            )
                            .await;
                        });
                    }
                    // Both refusals drop the datagram in silence, and the
                    // silence is the message: an un-acked diff stays pending
                    // in the peer's scheduler and is re-offered, where a
                    // `BulkNack` would tell it to discard the write. See
                    // `GatewayIngressMetrics` for why that is the honest
                    // answer on this lane, and where the count surfaces.
                    DiffAdmission::ShedStale => {
                        metrics.ingress.record_shed_stale();
                        metrics.boundary.record_shed_age(ingress_queue_us);
                        debug!(
                            %remote,
                            entity = ?diff.entity,
                            ingress_queue_us,
                            "gateway: shed stale diff"
                        );
                    }
                    DiffAdmission::ShedSaturated => {
                        metrics.ingress.record_shed_saturated();
                        metrics.boundary.record_shed_age(ingress_queue_us);
                        debug!(
                            %remote,
                            entity = ?diff.entity,
                            cap = MAX_INFLIGHT_DIFF_ROUTES_PER_CONN,
                            "gateway: shed diff, route cap saturated"
                        );
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
                // The wait upstream of `received_at`, carried into the intent's
                // own stage record. It is already summed into
                // `gateway_ingress_queue_ms`, but that series is ~100:1 diffs
                // by count, so an intent-specific ingress tail is
                // arithmetically invisible in it.
                let mut trace = IntentTrace {
                    ingress_us: ingress_queue_us,
                    ..IntentTrace::default()
                };
                let admit = admit_intent(&intent, validator.as_ref(), &cx);
                trace.admit_us = elapsed_us(received_at);
                if let Err(outcome) = admit {
                    send_intent_reply(
                        send.as_ref(),
                        intent.intent_id,
                        outcome,
                        &metrics.intent,
                        received_at,
                        trace,
                        false,
                    );
                    continue;
                }
                match reserve_intent_lane(Arc::clone(&inflight_intents)) {
                    Ok(permit) => {
                        let send = Arc::clone(&send);
                        let executor = executor.clone();
                        let metrics = Arc::clone(&metrics);
                        // Stamped on the receive loop, read as the spawned
                        // task's first act: the difference is the runtime's
                        // own queueing delay, which is otherwise billed to the
                        // executor.
                        let spawn_at = Instant::now();
                        tokio::spawn(async move {
                            let _permit = permit;
                            execute_admitted_intent(
                                send.as_ref(),
                                intent,
                                &executor,
                                &metrics.intent,
                                received_at,
                                trace,
                                spawn_at,
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
                            trace,
                            false,
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
    // Drain the lane before tearing the session down. `cleanup_peer_session`
    // parks this peer's leases, and a lease operation still in flight after
    // that would be operating on a session the registrar has already
    // released — which the inline arm could never do, because the loop *was*
    // the worker.
    drop(lease_tx);
    if let Err(e) = lease_worker.await {
        warn!(?e, %remote, "gateway: lease worker did not finish cleanly");
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

/// One lease operation, with the session it was received under.
///
/// The session is captured at *receive* time rather than read again in the
/// worker, because that is what the inline arm did: it bound
/// `session.as_ref()` the moment the message came off the inbound queue. A
/// worker that re-read the connection's current session instead would serve
/// an operation under a generation the peer never addressed it to.
struct LeaseWork {
    session: PeerSession,
    message: LeaseMsg,
}

/// Everything one connection's lease worker needs, cloned once at connection
/// setup rather than threaded through the message loop.
struct LeaseContext {
    send: Arc<dyn Fn(Bytes) + Send + Sync>,
    router: Arc<dyn Router>,
    redistributor: Arc<Redistributor>,
    interest_authority: SharedInterestAuthority,
    claim_clock: SharedClaimClock,
    remote: NodeId,
}

/// Refuse one lease operation that could not be queued, without lying to the
/// peer about what happened to it.
///
/// Only a claim has an honest refusal here. `Deny { RateLimited }` is exactly
/// true of a full per-connection lane — it *is* a per-peer limit — and it
/// carries a retry delay, so the claimant comes back rather than spinning.
/// A heartbeat has no such reply: `HeartbeatAck { invalid: renew }` would tell
/// a holder its leases are gone and make it stop writing to entities it still
/// legitimately owns, which is a far worse answer than silence, and the holder
/// re-heartbeats well inside `LEASE_TTL_MS`. A divest likewise resolves on the
/// handoff deadline. Both are counted by `GatewayLeaseMetrics::record_refused`
/// and logged at the call site, so the drop is a number an operator can read
/// rather than an unexplained stall.
fn refuse_saturated_lease(send: &(dyn Fn(Bytes) + Send + Sync), message: &LeaseMsg) {
    if let LeaseMsg::Claim {
        claim_id, entity, ..
    } = message
    {
        send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
            message: LeaseMsg::Deny {
                claim_id: Some(*claim_id),
                entity: *entity,
                reason: orrery_protocol::DenyReason::RateLimited,
                retry_after_ms: LEASE_LANE_RETRY_AFTER_MS,
            },
        })));
    }
}

/// Start one connection's lease lane: a queue and the single task that drains
/// it.
///
/// One task, not one per message. `serve_lease_message` records why the
/// fencing protocol requires that, and `a_blocked_lease_operation_does_not_let_the_next_one_overtake_it`
/// pins it.
fn spawn_lease_lane(cx: LeaseContext) -> (mpsc::Sender<LeaseWork>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<LeaseWork>(MAX_QUEUED_LEASE_OPS_PER_CONN);
    let worker = tokio::spawn(async move {
        while let Some(work) = rx.recv().await {
            serve_lease_message(&cx, work).await;
        }
    });
    (tx, worker)
}

/// Serve one lease operation: claim, heartbeat, divest or rekey.
///
/// # Why this is not on the receive loop
///
/// This body has sixteen `.await` points, several of them per-`(grid, cell)`
/// actor round trips, and it used to run inline in `handle_connection`'s
/// dispatch. Every microsecond it spent was a microsecond the connection's
/// bulk diffs sat in the inbound queue ageing toward
/// `MAX_INGRESS_QUEUE_WAIT_US`, after which they are shed — so lease work,
/// not actor saturation, was what a quarter of the offered diffs were being
/// destroyed for. Measured on the P2 rig: 128 actors at 37.8 % utilisation
/// (17 684 diffs/s x 2.734 ms mean `router_apply` = 48.4 actor-seconds per
/// wall second), while 25.1 % of diffs were shed for queue age. The actors
/// were never the limit; this dispatch was.
///
/// # What ordering it needs, and where that is read from
///
/// A fencing protocol cannot be reordered, so this is not free to run
/// concurrently per message. Read out of the code rather than assumed:
///
/// * **Per entity.** `PeerState::leases` is keyed by `PersistId`, and
///   `complete_lease_claim` decides Granted/Compensate/Denied by comparing
///   the entry already indexed at that key. A divest that removed the entry
///   after a claim inserted it, but was *applied* before it, would leave the
///   session indexing a lease the registrar no longer records.
/// * **Per session.** `resolve_renewals` refuses any lease whose owner is not
///   `Active(self.generation)`, and `try_reserve_lease_slot` /
///   `complete_lease_claim` are a reserve-then-commit pair over
///   `pending_lease_claims`. A second claim overtaking the first between
///   those two halves would see a capacity figure that the first claim has
///   reserved against but not yet spent.
/// * **Not per connection-wide message order.** Nothing here reads or writes
///   state shared with the diff, intent, subscribe or report arms, so those
///   lanes were already free to run beside lease work (they spawn), and
///   moving lease work off the loop does not change what they observe.
///
/// A per-connection FIFO worker is therefore the smallest shape that is
/// *exactly* as ordered as the inline arm was: strictly one lease operation
/// at a time, in arrival order, per connection — which subsumes both the
/// per-entity and per-session requirements, since a peer's entities and its
/// session are reachable only through its own connection. Naive
/// `tokio::spawn` per message would satisfy neither.
///
/// Two orderings are deliberately *not* preserved, both already reachable
/// before this change:
///
/// * A `Hello` is still served on the receive loop, so a re-`Hello` can
///   activate a new generation while an operation for the old one is queued.
///   That operation then finds `lock_current()` returning `None` and is
///   denied, or completes into the `Compensate` park path — the same two
///   outcomes `Redistributor` and `cleanup_peer_session` already produce
///   concurrently (see `cleanup_peer_session_releases_peer_state_while_park_is_pending`).
/// * The registrar's own paths (`Redistributor::request_divest`,
///   `expire_handoff`, the TTL sweep) never ran on this loop and are
///   unaffected.
///
/// # The peer mutex
///
/// `lock_current` returns an `OwnedMutexGuard` over the whole `PeerState`,
/// which is shared with the diff path. Holding one across an await serialises
/// every other operation on the connection — that is the bug that once cost
/// this project a 2 s bulk p99. Every guard below is dropped before the first
/// actor round trip, and this worker does not add one.
#[allow(clippy::too_many_lines)]
async fn serve_lease_message(cx: &LeaseContext, work: LeaseWork) {
    let LeaseContext {
        send,
        router,
        redistributor,
        interest_authority,
        claim_clock,
        remote,
    } = cx;
    let send = send.as_ref();
    let remote = *remote;
    let LeaseWork {
        session: active_session,
        message,
    } = work;
    let active_session = &active_session;
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
                return;
            };
            let claim_now_ms = claim_clock.now_ms();
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
                return;
            }
            drop(peer);
            // Asked before anything arbitrates, because arbitration cannot
            // reach the right answer here. A claim for a cell no shard of
            // this node covers fails `plausible` below — `committed_entity_
            // cell` finds nothing for an entity this node hosts no actor for
            // — and comes back `NotEligible`, which is a statement about the
            // *claimant*. It is the wrong statement: the claimant may be
            // perfectly eligible, at a node that is not this one
            // (docs/08-persistence.md §3.5). The claim-rate token has already
            // been spent above, so this cannot be driven any faster than an
            // ordinary claim.
            if let Some(epoch) = router.wrong_owner_epoch(grid, cell).await {
                send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                    message: LeaseMsg::Deny {
                        claim_id: Some(claim_id),
                        entity,
                        reason: orrery_protocol::DenyReason::WrongOwner {
                            grid,
                            shard: cell,
                            epoch,
                            // ADR-0026's question, deliberately unanswered
                            // here. See `DenyReason::WrongOwner`.
                            owner: None,
                        },
                        // No timer helps: re-addressing is the only thing
                        // that changes this answer.
                        retry_after_ms: 0,
                    },
                })));
                return;
            }
            if !active_session.try_reserve_lease_slot().await {
                send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                    message: LeaseMsg::Deny {
                        claim_id: Some(claim_id),
                        entity,
                        reason: orrery_protocol::DenyReason::NotEligible,
                        retry_after_ms: 0,
                    },
                })));
                return;
            }
            let now_ms = registrar_now_ms();
            let player_basis = matches!(
                basis,
                orrery_protocol::ClaimBasis::Contact { .. } | orrery_protocol::ClaimBasis::Explicit
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
            let contested = if plausible && matches!(kind, orrery_protocol::ClaimKind::Strong) {
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
                if redistributor.request_divest(router, pending).await {
                    // The reservation is released now and retaken
                    // when the handoff lands, so a peer waiting on
                    // a deadline does not sit on lease capacity.
                    let _ = active_session.complete_lease_claim(None).await;
                    return;
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
                                if peer.leases.get(&compensation.entity) == Some(&compensation) {
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
                Err(error) => {
                    let _ = active_session.complete_lease_claim(None).await;
                    // A route that found no actor is answered by name, not as
                    // a generic ineligibility: the claimant is eligible, it is
                    // simply asking the wrong node. Every other routing
                    // failure keeps the old answer, including its 100 ms
                    // backoff — those *are* worth retrying here.
                    match error {
                        Reject::WrongOwner { grid, shard, epoch } => LeaseMsg::Deny {
                            claim_id: Some(claim_id),
                            entity,
                            reason: orrery_protocol::DenyReason::WrongOwner {
                                grid,
                                shard,
                                epoch,
                                owner: None,
                            },
                            retry_after_ms: 0,
                        },
                        Reject::JournalClosed | Reject::LeaseStore => LeaseMsg::Deny {
                            claim_id: Some(claim_id),
                            entity,
                            reason: orrery_protocol::DenyReason::NotEligible,
                            retry_after_ms: 100,
                        },
                    }
                }
            };
            send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                message,
            })));
        }
        LeaseMsg::Heartbeat { renew, .. } => {
            // Five waits, timed separately. See `lease::stages` for why the
            // aggregate this arm used to report as nothing at all was not a
            // usable answer to "what does a renewal cost above the router".
            let started = Instant::now();
            let mut trace = HeartbeatTrace {
                entries: renew.len() as u64,
                ..HeartbeatTrace::default()
            };
            let session_started = Instant::now();
            let Some(peer) = active_session.lock_current().await else {
                // A heartbeat on a session that is already gone is still a
                // served heartbeat, and its cost is still real: it takes the
                // lock, finds nothing, and encodes an ack refusing every pair.
                // Recording it keeps `heartbeats` the count of messages served
                // rather than the count that found a session.
                trace.session_us = lease_stage_us(session_started);
                let encode_started = Instant::now();
                send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                    message: LeaseMsg::HeartbeatAck {
                        leases: Vec::new(),
                        invalid: renew,
                    },
                })));
                trace.encode_us = lease_stage_us(encode_started);
                trace.heartbeat_us = lease_stage_us(started);
                lease_stage_metrics().record(&trace);
                return;
            };
            trace.session_us = lease_stage_us(session_started);
            let resolve_started = Instant::now();
            let (renewable, mut invalid) =
                resolve_renewals(&peer.leases, active_session.generation, &renew);
            drop(peer);
            trace.resolve_us = lease_stage_us(resolve_started);
            let route_started = Instant::now();
            let (rows, refused, misaddressed) =
                renew_session_leases(router, remote, &renewable, registrar_now_ms()).await;
            trace.route_us = lease_stage_us(route_started);
            let mut rows = rows;
            invalid.extend(refused);
            let recheck_started = Instant::now();
            let vanished = active_session.lock_current().await.is_none();
            trace.recheck_us = lease_stage_us(recheck_started);
            if vanished {
                rows.clear();
                invalid = renew;
            }
            let encode_started = Instant::now();
            send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                message: LeaseMsg::HeartbeatAck {
                    leases: rows,
                    invalid,
                },
            })));
            // After the ack, never instead of it. `HeartbeatAck` is the
            // renewal contract and it is unchanged; these say *why* a pair in
            // its `invalid` list could not be answered here, one message per
            // entity because that is the granularity the ack itself keeps.
            //
            // `claim_id: None` because no claim was made: this is an
            // unsolicited statement about routing, and a `Deny` is the only
            // message on the lease surface that carries a `DenyReason` at all.
            // A client with no matching pending claim ignores it, which is the
            // correct floor — a peer that has not been taught to read it is no
            // worse off than before this existed.
            if !vanished {
                for entry in misaddressed {
                    send(Bytes::from(encode_stream_frame(&GatewayReply::Lease {
                        message: LeaseMsg::Deny {
                            claim_id: None,
                            entity: entry.entity,
                            reason: orrery_protocol::DenyReason::WrongOwner {
                                grid: entry.grid,
                                shard: entry.cell,
                                epoch: entry.epoch,
                                owner: None,
                            },
                            retry_after_ms: 0,
                        },
                    })));
                }
            }
            trace.encode_us = lease_stage_us(encode_started);
            trace.heartbeat_us = lease_stage_us(started);
            lease_stage_metrics().record(&trace);
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
                router,
                redistributor,
                claim_clock.now_ms(),
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

    // The audience `place` would have produced, kept when this path already
    // walked the registry for its own reasons so the fan-out below never pays
    // for a second enumeration of the same `(grid, cell)` (D25 rule 8).
    let mut audience: Option<Vec<NodeId>> = None;
    let disposition = match to {
        None => orrery_protocol::ExpireDisposition::Parked,
        Some(successor) if successor == session.node => orrery_protocol::ExpireDisposition::Parked,
        Some(successor) => {
            // The named successor passes exactly the admission a claim of its
            // own would: a live session on this gateway plus live coordinator
            // interest covering the cell. Consent does not widen it.
            let candidates = redistributor
                .candidates(indexed.grid, indexed.cell, session.node, registrar_now_ms())
                .await;
            let eligible = candidates
                .iter()
                .any(|candidate| candidate.node == successor);
            audience = Some(candidates.iter().map(|candidate| candidate.node).collect());
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

    let fans_out = Redistributor::fans_out(&disposition);
    let message = LeaseMsg::Expire {
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
    };

    // A deliberate release is the purest case D25 exists for: the entity is
    // parked, no successor stream will ever raise its pair, and every observer
    // would otherwise extrapolate a proxy of a body nobody writes. A consented
    // *handoff* is `Reassigned` and stays holder-only (rule 7).
    //
    // The caller sends `message` back down this holder's own connection, so
    // the holder's copy is not sent here and the holder is excluded from the
    // audience either way — `candidates` excludes it, and so does
    // `fanout_audience`.
    if fans_out {
        let audience = match audience {
            Some(nodes) => CoveringPeers::bounded(nodes, EXPIRE_FANOUT_MAX_RECIPIENTS),
            None => {
                // One divest is one expiry, so the memo has nothing to reuse
                // and exists only to satisfy the signature. Constructing it
                // here rather than holding one on the redistributor is the
                // point: an audience outliving its pass would answer with
                // sessions that have gone and grants that have lapsed.
                redistributor
                    .fanout_audience(
                        &mut ExpireAudiences::default(),
                        indexed.grid,
                        indexed.cell,
                        session.node,
                        registrar_now_ms(),
                    )
                    .await
            }
        };
        redistributor.fan_out_expire(audience, &message).await;
    }

    message
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
    //
    // This is also the fan-out path's hardest case and the reason D25 exists.
    // The holder is by definition gone here, so its own `Expire` goes nowhere;
    // before D25 that meant a park on this path was observable from no peer at
    // all. `redistribute` now addresses the survivors that cover the cell, so
    // the burst for one lost peer is `Σ min(|A|, R)` over the leases that
    // actually park — which for a field host, whose leases reassign, is
    // approximately nothing, and for a strong-owned working set is held down
    // by each recipient's own egress bucket rather than by this loop.
    let mut audiences = ExpireAudiences::default();
    for parked in orphaned {
        redistributor
            .redistribute(router, parked, &mut audiences)
            .await;
    }

    let mut peer = session.state.lock().await;
    if peer.current.is_none() && peer.live.is_empty() && peer.leases.is_empty() {
        peer.idle_since_ms = Some(now_ms);
    }
}

/// A connection's allowance for routing bulk diffs at a cell its own lease
/// index does not name.
///
/// Such a diff misses the fenced route's fast path and pays a
/// `LeaseStore::locate` plus a second mailbox turn. It is not necessarily
/// abuse — a registrar-driven rekey moves an entity without telling the
/// gateway, and the first write at the new cell looks exactly like this — so
/// the allowance is a bucket rather than a refusal, and an admitted probe
/// repairs the index so the next diff needs no token at all.
///
/// Sized for the repair case, not the abuse case: a mass rekey should not
/// stall, so the burst covers 256 entities at once, and the steady rate is
/// 32/s because past the repair there is nothing legitimate left to spend it
/// on. What a wrong cell buys at 32/s is bounded work; what it bought before
/// was the client's own choice of rate.
#[derive(Debug, Clone, Copy)]
struct MisrouteBucket {
    token_millis: u64,
    updated_ms: u64,
}

impl MisrouteBucket {
    const PROBES_PER_SECOND: u64 = 32;
    const BURST_PROBES: u64 = 256;
    const TOKEN_MILLIS_PER_PROBE: u64 = 1_000;
    const BURST_TOKEN_MILLIS: u64 = Self::BURST_PROBES * Self::TOKEN_MILLIS_PER_PROBE;

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
                .saturating_mul(Self::PROBES_PER_SECOND);
            self.token_millis = self
                .token_millis
                .saturating_add(replenished)
                .min(Self::BURST_TOKEN_MILLIS);
            self.updated_ms = now_ms;
        }
        if self.token_millis < Self::TOKEN_MILLIS_PER_PROBE {
            false
        } else {
            self.token_millis -= Self::TOKEN_MILLIS_PER_PROBE;
            true
        }
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

/// One peer's allowance for *receiving* non-holder `Expire` advisories
/// (D25 rule 8, second limit).
///
/// Deliberately the same shape as [`ClaimBucket`] — 32/s sustained, burst 64 —
/// so the ingress limit on `Claim` and the egress limit on the advisory it
/// answers read alike, and neither has to be looked up to reason about the
/// other. It is the limit that actually binds: the per-expiry cap is a
/// property of one cell's population, while this one is what stops a single
/// `cleanup_peer_session` pass over a strong-owned working set from delivering
/// `MAX_PEER_LIVE_LEASES` advisories to one peer in a burst. With it, one pass
/// costs any single peer at most 64 frames — about 4 KB — and the sustained
/// rate is 16.4 kbit/s, 1.6 % of D6's per-peer upload budget.
///
/// Kept on [`PeerState`] rather than on the session, and so shared across
/// replacement generations exactly as `claim_bucket` is: the budget belongs to
/// the `NodeId`, and a peer that reconnected should not find its allowance
/// refilled by the reconnect.
///
/// An empty bucket **drops**, never queues (D25 rule 9).
#[derive(Debug, Clone, Copy)]
struct ExpireFanoutBucket {
    token_millis: u64,
    updated_ms: u64,
}

impl ExpireFanoutBucket {
    const ADVISORIES_PER_SECOND: u64 = 32;
    const BURST_ADVISORIES: u64 = 64;
    const TOKEN_MILLIS_PER_ADVISORY: u64 = 1_000;
    const BURST_TOKEN_MILLIS: u64 = Self::BURST_ADVISORIES * Self::TOKEN_MILLIS_PER_ADVISORY;

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
                .saturating_mul(Self::ADVISORIES_PER_SECOND);
            self.token_millis = self
                .token_millis
                .saturating_add(replenished)
                .min(Self::BURST_TOKEN_MILLIS);
            self.updated_ms = now_ms;
        }
        if self.token_millis < Self::TOKEN_MILLIS_PER_ADVISORY {
            false
        } else {
            self.token_millis -= Self::TOKEN_MILLIS_PER_ADVISORY;
            true
        }
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
#[allow(clippy::too_many_arguments)]
async fn execute_admitted_intent(
    send: &(dyn Fn(Bytes) + Send + Sync),
    intent: Intent,
    executor: &Option<SharedExecutor>,
    metrics: &GatewayIntentMetrics,
    received_at: Instant,
    seed: IntentTrace,
    spawn_at: Instant,
) {
    let intent_id = intent.intent_id;
    let executed = executor.is_some();

    // The trace is task-scoped rather than threaded through
    // `IntentExecutor::execute`: that trait's one method takes only the
    // intent, and widening the authority seam to carry a metrics handle would
    // make every future executor implement observability to compile. The FDB
    // executor writes its own phases into this same trace because it runs on
    // this task.
    let (outcome, trace) = stages::with_trace(async {
        stages::trace(|t| {
            *t = seed;
            t.spawn_wait_us = elapsed_us(spawn_at);
        });
        // 4. Execution — ack only after the future resolves. An executor error
        //    becomes a definitive rejection (bounded-retry refusal, §7).
        match executor {
            None => IntentOutcome::Rejected {
                reason: REASON_NO_EXECUTOR,
            },
            Some(exec) => match stages::timed(|t| &mut t.exec_us, exec.execute(&intent)).await {
                Ok(outcome) => outcome,
                Err(err) => error_outcome(&err),
            },
        }
    })
    .await;
    send_intent_reply(
        send,
        intent_id,
        outcome,
        metrics,
        received_at,
        trace,
        executed,
    );
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
    mut trace: IntentTrace,
    executed: bool,
) {
    // Measured up to the send call, not past it: everything after is the
    // wire, which is precisely the part `intent_commit_ms` covers and this
    // series must not.
    let server_us = elapsed_us(received_at);
    metrics.record_reply(&outcome, server_us);
    trace.server_us = server_us;
    let reply = GatewayReply::IntentAck { intent_id, outcome };
    // The encode and the lane push, timed: both are supposed to be free (an
    // unbounded mpsc send), and a stage that is supposed to be free is exactly
    // the one worth being able to prove is.
    let reply_at = Instant::now();
    send(Bytes::from(encode_stream_frame(&reply)));
    trace.reply_us = elapsed_us(reply_at);
    // One fold per intent, at the single choke point every reply passes
    // through — so `intents` is per definitive acknowledgement and can never
    // drift from `GatewayIntentSnapshot::replies`.
    intent_stage_metrics().record(&trace, executed);
}

/// Feed the connection's datagrams into the shared inbound queue.
///
/// Its counterpart is [`reliable::spawn_receiver`]. Both write to the same
/// queue and the task ends when its source does, so the queue closes — and the
/// receive loop with it — only once *both* lanes are gone.
fn spawn_datagram_reader(
    conn: Arc<iroh::endpoint::Connection>,
    remote: NodeId,
    sink: tokio::sync::mpsc::UnboundedSender<(Bytes, Instant)>,
) {
    tokio::spawn(async move {
        loop {
            match conn.read_datagram().await {
                Ok(pkt) => {
                    // Stamped here, the instant the endpoint driver gave the
                    // datagram up: everything between this and the receive
                    // loop's own `received_at` is gateway ingress backlog,
                    // and it used to be invisible at both ends of the wire.
                    if sink.send((pkt, Instant::now())).is_err() {
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
    ingress_metrics: &GatewayIngressMetrics,
    authority_metrics: Arc<AuthorityMetrics>,
    received_at: Instant,
    budget_us: u64,
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
    // contents beyond the one lookup below, and the guard is dropped at the
    // end of this block, never across the route. `PeerSession` already avoids
    // the same trap for the account field (see its `account` doc comment).
    // Covered by `session_diffs_do_not_serialize_on_the_peer_lock`.
    let indexed = {
        let Some(peer) = session.lock_current().await else {
            send(Bytes::from(encode_datagram(&GatewayReply::BulkNack {
                entity: diff.entity,
                tick: diff.tick,
                reason: 2,
                lease: None,
            })));
            return;
        };
        peer.leases.get(&diff.entity).copied()
    };
    // The cell a diff is routed by comes off the wire, and it selects between
    // a cheap path and an expensive one.
    //
    // `record.cell` is `diff.cell`, straight from the client's `DiffUplink`,
    // and the actor's `by_cell[e] == record.cell` conjunct is evaluated
    // *after* the route has chosen an actor. So a peer holding a perfectly
    // valid lease that presents some other cell misses the fenced route's
    // fast path on every diff and pays its fallback: one `LeaseStore::locate`
    // -- an FDB read, the resource docs/14-capacity.md 5.1 measured as the
    // binding constraint on a whole box -- plus a second mailbox turn, at the
    // peer's chosen rate. Capacity was bimodal on an unvalidated field with
    // nothing rate-limiting the expensive branch.
    //
    // It is **not** a refusal, and the first draft of this was. The session's
    // indexed cell is the cell the lease was *granted* at, and a registrar-
    // driven `commit_rekey` moves an entity without telling the gateway, so a
    // holder writing at the new cell presents one the index has never heard
    // of -- legitimately: `rekeyed_entity_rejects_stale_presented_cell_with_
    // current_lease` asserts that write is acknowledged, and refusing on a
    // mismatch broke it.
    //
    // So a mismatch buys a **probe**, from a per-connection token bucket, and
    // an admitted probe corrects the index below. A rekey then costs one
    // token per entity and routes at full speed afterwards, while a peer
    // whose cell is simply wrong is never admitted, never corrects the index,
    // and is throttled to `MisrouteBucket::PROBES_PER_SECOND` fallbacks a
    // second. `cluster::fenced_locate_fallback_permits` bounds what every
    // connection together can have in flight; this bounds one connection's
    // share of the queue for it.
    //
    // **A missing index entry is the same vector, and it used to be free.**
    // The predicate was `indexed.is_some_and(...)`, so an entity this session
    // holds no lease for -- the one shape a peer can pick with no setup at
    // all -- read as "not misrouted", took no token, incremented nothing, and
    // routed. On the router side an entity with no row anywhere is
    // `Rejected(None)`, and `Rejected(None)` is exactly the answer that does
    // *not* short-circuit: it takes the fallback and spends one
    // `LeaseStore::locate` (`tests/fenced_route_bounds.rs`, "still exactly
    // one locate"). So the bucket, the repair and the `misrouted_diffs` alarm
    // all missed the cheapest way to reach the expensive branch, and only the
    // process-wide permit pool applied -- which caps *concurrency*, not rate.
    //
    // Treating `None` as unproven is safe because there is no legitimate
    // producer of it. A peer learns it holds a lease only from `LeaseMsg::
    // Grant`, and both emitters send it *after* `complete_lease_claim` has
    // inserted the entry, so no diff can outrun its own index entry; every
    // removal (`divest_lease`, `unwind_grant`, `cleanup_peer_session`, the
    // compensation path) is paired with a `park_lease` that has already made
    // the router reject the write anyway; and a failed renewal reports
    // `invalid` without touching the map. The rekey case that made this a
    // probe rather than a refusal keeps its entry throughout -- only the
    // *cell* moves.
    //
    // It is still a probe and not a refusal, so if that reasoning is ever
    // falsified by a new grant path the cost is a metered 32/s rather than a
    // hard stop. What an unindexed probe deliberately does **not** do is
    // repair: the repair below writes only through `get_mut`, so it cannot
    // invent a `SessionLease` the gateway never granted. An invented entry
    // would enter `lease_capacity` accounting, `resolve_renewals` and
    // `cleanup_peer_session`'s park loop, which is a much larger change than
    // this bound, and admission does not tell us the row's owner generation.
    //
    // One detection consequence, accepted: a throttled diff is no longer
    // routed, so it can no longer raise `duplicate_authority` against a
    // *different* live holder. The first `MisrouteBucket::BURST_PROBES` of
    // them still do, so the detector still fires -- it simply cannot be
    // driven at a peer's chosen rate, which is the point of the bucket.
    let unproven = match indexed {
        Some(lease) => lease.cell != diff.cell || lease.grid != diff.grid,
        None => true,
    };
    // Kept for the repair below: only an entry that exists can be corrected.
    let misrouted = unproven && indexed.is_some();
    if unproven {
        authority_metrics.record_misrouted_diff();
        if indexed.is_none() {
            authority_metrics.record_unindexed_diff();
        }
        let probe = match session.lock_current().await {
            Some(mut peer) => peer.misroute_bucket.take(registrar_now_ms()),
            None => false,
        };
        if !probe {
            authority_metrics.record_misroute_throttled();
            debug!(
                entity = ?diff.entity,
                tick = ?diff.tick,
                presented_cell = ?diff.cell,
                presented_grid = ?diff.grid,
                indexed = ?indexed.map(|lease| (lease.grid, lease.cell)),
                "gateway: throttled a bulk diff whose route this session's lease index does not prove"
            );
            // No row travels back, and none is owed. Where the index names a
            // lease of ours, the row the router would have returned names
            // *this* session's own holder, so `observe_fencing_rejection`
            // returns early on it either way -- it fires only for a
            // **different** unexpired holder. Where the index names nothing,
            // there is nothing this session is entitled to be told about:
            // handing back a row for an entity it holds no lease for would
            // turn a throttled write into a free registrar read.
            send(Bytes::from(encode_datagram(&GatewayReply::BulkNack {
                entity: diff.entity,
                tick: diff.tick,
                reason: 2,
                lease: None,
            })));
            return;
        }
    }
    let entity = diff.entity;
    let (grid, cell) = (diff.grid, diff.cell);
    let admitted = route_diff(
        send,
        DiffRoute {
            diff,
            author: session.node,
            received_at,
            strict_authority: true,
            budget_us,
            authority_metrics,
        },
        router,
        bulk_ack_admission,
        bulk_metrics,
        ingress_metrics,
    )
    .await;
    // Only ever reached by an admitted probe, so it costs a lock on a path
    // that already paid for a fallback locate -- and it is what stops the
    // next diff paying the same price. An admission is proof of the location:
    // the actor admitted it only because its own `by_cell[entity]` names this
    // cell and its registrar row names this holder's live lease.
    if misrouted && admitted {
        if let Some(mut peer) = session.lock_current().await {
            if let Some(indexed) = peer.leases.get_mut(&entity) {
                indexed.grid = grid;
                indexed.cell = cell;
            }
        }
    }
}

/// The wire reason code a [`Reject`] travels back to the client as.
///
/// The distinction that earns the function: `NotOwned` is not a failure of
/// this write, it is a statement about this *node*. A client that folds it
/// into [`orrery_protocol::BULK_NACK_JOURNAL`] cannot tell a broken gateway
/// from the wrong one, which is the whole defect this reason code closes
/// (docs/08-persistence.md §3.5).
fn bulk_nack_reason(reject: &Reject) -> u16 {
    match reject {
        Reject::WrongOwner { .. } => orrery_protocol::BULK_NACK_WRONG_OWNER,
        Reject::JournalClosed | Reject::LeaseStore => orrery_protocol::BULK_NACK_JOURNAL,
    }
}

struct DiffRoute {
    diff: DiffUplink,
    author: NodeId,
    received_at: Instant,
    strict_authority: bool,
    authority_metrics: Arc<AuthorityMetrics>,
    /// How long, since `received_at`, the router has to admit this diff to a
    /// journal before it is shed. Zero disables the valve. Carried per route
    /// rather than read from the process config inside `route_diff` so a test
    /// can state the policy it is testing; the receive loop passes
    /// [`route_admission_budget_us`].
    budget_us: u64,
}

/// What the router did with a record, once, so the whole round trip can sit
/// inside one timed future rather than being spliced by an early `return`.
enum RouteAdmission {
    /// The record reached (or failed to reach) the journal.
    Journaled(Result<Arc<crate::journal::AppendHandle>, Reject>),
    /// The fence refused it; the live row travels back for the NACK.
    Fenced {
        lease: Option<orrery_protocol::Lease>,
        lease_id: LeaseId,
        fence_now_ms: u64,
    },
}

/// Run the router round trip under the route-admission budget, measured from
/// the diff's *arrival*, not from here.
///
/// `None` means the budget was already spent before the call or ran out
/// during it. Dropping the future cancels it, which is safe at every point it
/// can be dropped: waiting on the entity gate and the `LeaseStore::locate`
/// read leave no trace, and if it is dropped after the actor took the mailbox
/// message the actor still journals the record — the client simply gets no
/// ack for a write that happened, and re-offers it, where the fold is
/// last-writer-wins per `(entity, tick)` and the replay is a no-op.
///
/// A zero budget disables the valve outright, which is the merged branch's
/// behaviour and therefore the "before" leg of an A/B against it.
async fn within_route_budget<T>(
    received_at: Instant,
    budget: u64,
    route: impl std::future::Future<Output = T>,
) -> Option<T> {
    if budget == 0 {
        return Some(route.await);
    }
    let remaining = budget.saturating_sub(elapsed_us(received_at));
    if remaining == 0 {
        return None;
    }
    tokio::time::timeout(Duration::from_micros(remaining), route)
        .await
        .ok()
}

/// Journal a bulk diff via the owning cell actor, then ack with the durable
/// LSN (or nack on rejection). The gateway fills in the server-assigned
/// `epoch`/`lsn`/`author`/`crc` (docs/08-persistence.md §2.1).
///
/// Returns whether the router **admitted** the record — took it into a
/// journal at the cell the diff named. Not whether it committed, and not
/// whether the client was acknowledged: the caller uses it as proof of the
/// entity's location, and admission is where that is decided
/// (`by_cell[entity] == record.cell`, inside the actor's fence).
async fn route_diff(
    send: &(dyn Fn(Bytes) + Send + Sync),
    route: DiffRoute,
    router: &Arc<dyn Router>,
    bulk_ack_admission: &SharedBulkAckAdmission,
    bulk_metrics: &GatewayBulkMetrics,
    ingress_metrics: &GatewayIngressMetrics,
) -> bool {
    let DiffRoute {
        diff,
        author,
        received_at,
        strict_authority,
        budget_us,
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
    // The valve, and the only reason it is here rather than in the receive
    // loop: this `await` is where the queue is (`MAX_ROUTE_ADMISSION_WAIT_US`).
    let admission = within_route_budget(received_at, budget_us, async {
        if strict_authority {
            // The actor performs this comparison and append in one mailbox
            // turn. A missing pair deliberately uses the never-granted zero
            // token so it still returns the current row in the lease-specific
            // NACK.
            let (lease_id, authority_seq) = diff
                .lease_id
                .zip(diff.authority_seq)
                .unwrap_or((LeaseId(0), Default::default()));
            let fence_now_ms = registrar_now_ms();
            match router
                .apply_fenced(record, author, lease_id, authority_seq, fence_now_ms)
                .await
            {
                Ok(FencedApply::Accepted(handle)) => RouteAdmission::Journaled(Ok(handle)),
                Ok(FencedApply::Rejected(lease)) => RouteAdmission::Fenced {
                    lease,
                    lease_id,
                    fence_now_ms,
                },
                Err(error) => RouteAdmission::Journaled(Err(error)),
            }
        } else {
            RouteAdmission::Journaled(router.apply(record).await)
        }
    })
    .await;

    let result = match admission {
        // Refused, in silence, on exactly the convention the two ingress
        // refusals already set: an un-acked diff stays pending in the peer's
        // scheduler and is re-offered, usually as a newer tick, where a
        // `BulkNack` would tell it to discard the write. The count is the
        // honest part — see `GatewayIngressMetrics::record_shed_slow_route`.
        None => {
            ingress_metrics.record_shed_slow_route();
            debug!(
                entity = ?entity,
                ?tick,
                waited_us = elapsed_us(received_at),
                budget_us,
                "gateway: shed diff, router did not admit it inside its budget"
            );
            return false;
        }
        Some(RouteAdmission::Fenced {
            lease,
            lease_id,
            fence_now_ms,
        }) => {
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
            return false;
        }
        Some(RouteAdmission::Journaled(result)) => result,
    };
    // Past this point the record is in a journal at `diff.cell`, whatever the
    // durability wait below reports.
    let admitted = result.is_ok();
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
        Err(error) => {
            let reply = GatewayReply::BulkNack {
                entity,
                tick,
                reason: bulk_nack_reason(&error),
                lease: None,
            };
            send(Bytes::from(encode_datagram(&reply)));
        }
    }
    admitted
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
        // A cold miss on a cell this node owns no shard over is not an empty
        // cell, and an empty `AreaPage` would assert that it is. The order
        // matters: a cold store that *did* find rows still answers, because a
        // global durable tier can legitimately serve a cell whose live actor
        // lives elsewhere — only the "found nothing" case has to fall back to
        // saying who could not answer.
        let read = match read {
            Ok(None) => match router.wrong_owner_epoch(grid, cell).await {
                Some(epoch) => Err(Reject::WrongOwner {
                    grid,
                    shard: cell,
                    epoch,
                }),
                None => Ok(None),
            },
            other => other,
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
                let kind = match (&e, live) {
                    (Reject::WrongOwner { .. }, _) => orrery_protocol::AREA_LOAD_ERR_WRONG_OWNER,
                    (_, true) => orrery_protocol::AREA_LOAD_ERR_LIVE,
                    (_, false) => orrery_protocol::AREA_LOAD_ERR_COLD,
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

    /// D25 rule 8's per-expiry cap drops the excess, and drops it in a
    /// reproducible order.
    ///
    /// The ordering clause is the half worth testing. Truncating an unordered
    /// registry walk keeps a *different* 128 peers on every pass, so a run
    /// that exceeded the cap could not be reproduced from its own inputs, and
    /// the drop count would be the only thing about it that was stable.
    #[test]
    fn the_per_expiry_cap_drops_the_excess_in_node_id_order() {
        let peers = (1u8..=200).map(successor_node).collect::<Vec<_>>();
        let bounded = CoveringPeers::bounded(peers.clone(), EXPIRE_FANOUT_MAX_RECIPIENTS);

        assert_eq!(bounded.len(), EXPIRE_FANOUT_MAX_RECIPIENTS);
        assert_eq!(bounded.over_limit, 200 - EXPIRE_FANOUT_MAX_RECIPIENTS);

        let mut expected = peers;
        expected.sort_unstable_by_key(|peer| *peer.as_bytes());
        expected.truncate(EXPIRE_FANOUT_MAX_RECIPIENTS);
        assert_eq!(bounded.peers, expected);

        // Presented in a different order, the same set keeps the same 128.
        let mut shuffled = expected.clone();
        shuffled.reverse();
        assert_eq!(
            CoveringPeers::bounded(shuffled, EXPIRE_FANOUT_MAX_RECIPIENTS).peers,
            expected
        );

        // Under the cap nothing is dropped and the count says so.
        let under = CoveringPeers::bounded(
            (1u8..=8).map(successor_node).collect(),
            EXPIRE_FANOUT_MAX_RECIPIENTS,
        );
        assert_eq!(under.len(), 8);
        assert_eq!(under.over_limit, 0);
        assert!(!under.is_empty());
        assert!(CoveringPeers::bounded(Vec::new(), EXPIRE_FANOUT_MAX_RECIPIENTS).is_empty());
    }

    /// Exceeding the cap moves `expire_fanout_dropped`, and by the whole
    /// remainder rather than by one.
    ///
    /// The remainder is counted before any delivery is attempted, which is why
    /// this can be asserted against a registry holding no sessions at all: a
    /// peer dropped by the cap was never addressed, so its own reachability is
    /// not part of the question. The recipients that *were* addressed and
    /// found unreachable land on `expire_fanout_skipped` instead, and keeping
    /// the two apart is the point — one says a cell is past D6's population
    /// ceiling, the other says the registry is enumerating sessions it can no
    /// longer reach.
    #[test]
    fn over_cap_advisories_are_dropped_and_counted_before_delivery() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let registry = Arc::new(PeerRegistry::new(
                MAX_PEER_REGISTRY_ENTRIES,
                10_000,
                MAX_PEER_LIVE_LEASES,
            ));
            let redistributor = parking_redistributor(registry);
            let audience = CoveringPeers::bounded(
                (1u8..=200).map(successor_node).collect(),
                EXPIRE_FANOUT_MAX_RECIPIENTS,
            );
            let message = LeaseMsg::Expire {
                entity: PersistId::new(1),
                lease_id: LeaseId(3),
                last_holder: Some(successor_node(250)),
                reason: orrery_protocol::ExpireReason::Disconnect,
                disposition: orrery_protocol::ExpireDisposition::Parked,
            };

            redistributor.fan_out_expire(audience, &message).await;

            let snapshot = redistributor.metrics.snapshot();
            assert_eq!(
                snapshot.expire_fanout_dropped,
                (200 - EXPIRE_FANOUT_MAX_RECIPIENTS) as u64,
                "the whole over-cap remainder is one drop each"
            );
            assert_eq!(
                snapshot.expire_fanout_skipped, EXPIRE_FANOUT_MAX_RECIPIENTS as u64,
                "the capped recipients were addressed and had no session"
            );
            assert_eq!(snapshot.expire_fanout_sent, 0);
            // Purely additive: nothing on this path touches a disposition.
            assert_eq!(snapshot.parked_without_successor, 0);
            assert_eq!(snapshot.reassigned, 0);
            assert_eq!(snapshot.duplicate_authority, 0);
        });
    }

    /// An [`InterestAuthority`] that admits everything and counts how often it
    /// was asked.
    ///
    /// Counting is the whole point: D25 rule 8's per-pass limit is a statement
    /// about *how many times* the registry is walked, and an assertion about
    /// the answers cannot distinguish one enumeration from a hundred.
    #[derive(Default)]
    struct CountingInterestAuthority {
        enumerations: AtomicU64,
    }

    impl InterestAuthority for CountingInterestAuthority {
        fn snapshot_for(&self, _peer: NodeId) -> Option<CoordinatorInterestSnapshot> {
            None
        }

        fn allows(&self, _peer: NodeId, _grid: GridId, _cell: CellId, _now_ms: u64) -> bool {
            true
        }

        fn covering_peers(
            &self,
            sessions: &[NodeId],
            _grid: GridId,
            _cell: CellId,
            _now_ms: u64,
            limit: usize,
        ) -> CoveringPeers {
            self.enumerations.fetch_add(1, Ordering::Relaxed);
            CoveringPeers::bounded(sessions.to_vec(), limit)
        }
    }

    /// One pass enumerates each `(grid, cell)` once, however many entities in
    /// it expire — and the memo still excludes the right holder each time.
    ///
    /// The second clause is why the cache stores `A` unfiltered rather than
    /// `A \ {holder}`: a single TTL sweep carries rows from several holders,
    /// so the peer to exclude varies within the pass while the cell's
    /// membership does not. Keying on the holder as well would defeat the memo
    /// exactly where a cell is busiest.
    #[test]
    fn one_pass_enumerates_each_cell_once_whatever_the_holder() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let registry = Arc::new(PeerRegistry::new(
                MAX_PEER_REGISTRY_ENTRIES,
                10_000,
                MAX_PEER_LIVE_LEASES,
            ));
            let interest = Arc::new(CountingInterestAuthority::default());
            let redistributor = Redistributor {
                interest: Arc::clone(&interest) as SharedInterestAuthority,
                ..parking_redistributor(Arc::clone(&registry))
            };
            let cell = CellId::ROOT.children()[0];
            let elsewhere = CellId::ROOT.children()[1];
            let mut audiences = ExpireAudiences::default();

            // No sessions are registered, so every audience is empty — this
            // test is about the number of walks, not their contents.
            for _ in 0..16 {
                redistributor
                    .fanout_audience(&mut audiences, GridId::ROOT, cell, successor_node(1), 0)
                    .await;
            }
            assert_eq!(
                interest.enumerations.load(Ordering::Relaxed),
                1,
                "sixteen expiries in one cell cost one enumeration"
            );

            // A different holder in the same cell reuses the memo...
            redistributor
                .fanout_audience(&mut audiences, GridId::ROOT, cell, successor_node(2), 0)
                .await;
            assert_eq!(interest.enumerations.load(Ordering::Relaxed), 1);

            // ...and a different cell does not.
            redistributor
                .fanout_audience(
                    &mut audiences,
                    GridId::ROOT,
                    elsewhere,
                    successor_node(1),
                    0,
                )
                .await;
            assert_eq!(interest.enumerations.load(Ordering::Relaxed), 2);

            // A fresh pass starts cold, which is the property that keeps a
            // memo from outliving the sessions and grants it was built from.
            redistributor
                .fanout_audience(
                    &mut ExpireAudiences::default(),
                    GridId::ROOT,
                    cell,
                    successor_node(1),
                    0,
                )
                .await;
            assert_eq!(interest.enumerations.load(Ordering::Relaxed), 3);
        });
    }

    /// The per-recipient bucket is D16's claim-bucket shape at D25's rate:
    /// burst 64, then 32/s.
    ///
    /// This is the limit that actually binds. The per-expiry cap is a property
    /// of one cell's population and is inert below D6's ceiling; this one is
    /// what holds a single `cleanup_peer_session` pass over a strong-owned
    /// working set — up to `MAX_PEER_LIVE_LEASES` parks — down to 64 frames
    /// at any one peer.
    #[test]
    fn expire_fanout_bucket_bursts_to_sixty_four_then_refills_at_thirty_two_per_second() {
        let mut bucket = ExpireFanoutBucket::new(0);
        for advisory in 0..64 {
            assert!(bucket.take(0), "burst advisory {advisory} is admitted");
        }
        // The 65th in one pass is dropped, not queued: an advisory ahead of a
        // `Grant` on the same lane would degrade arbitration.
        assert!(!bucket.take(0));

        // One second later, exactly 32 more.
        for advisory in 0..32 {
            assert!(bucket.take(1_000), "refilled advisory {advisory}");
        }
        assert!(!bucket.take(1_000));

        // A long idle refills to the burst and no further.
        for advisory in 0..64 {
            assert!(bucket.take(1_000_000), "post-idle advisory {advisory}");
        }
        assert!(!bucket.take(1_000_000));
    }

    /// `covering_peers` is `allows` applied to the caller's session set, and
    /// the production authority's override answers identically.
    ///
    /// The override exists to take one read lock instead of `|Sessions|` of
    /// them; if it ever answered a different question, fan-out would address
    /// peers the same gateway would refuse to grant to.
    #[test]
    fn covering_peers_agrees_with_allows_on_every_impl() {
        let cell = CellId::ROOT.children()[0];
        let elsewhere = CellId::ROOT.children()[1];
        let sessions = (1u8..=4).map(successor_node).collect::<Vec<_>>();
        let snapshots = vec![
            interest_snapshot(successor_node(1), 1, GridId::ROOT, vec![cell], 10_000),
            interest_snapshot(successor_node(2), 1, GridId::ROOT, vec![elsewhere], 10_000),
            // Covers the cell, but its gateway-stamped deadline has passed.
            interest_snapshot(successor_node(3), 1, GridId::ROOT, vec![cell], 500),
            interest_snapshot(successor_node(4), 1, GridId::ROOT, vec![cell], 10_000),
        ];

        let authority = SnapshotInterestAuthority::from_snapshots(snapshots.clone());
        let covering = authority.covering_peers(
            &sessions,
            GridId::ROOT,
            cell,
            1_000,
            EXPIRE_FANOUT_MAX_RECIPIENTS,
        );
        let mut expected = vec![successor_node(1), successor_node(4)];
        expected.sort_unstable_by_key(|peer| *peer.as_bytes());
        assert_eq!(covering.peers, expected);
        assert_eq!(covering.over_limit, 0);

        // The same answer, arrived at through the per-peer seam.
        for peer in &sessions {
            assert_eq!(
                authority.allows(*peer, GridId::ROOT, cell, 1_000),
                covering.peers.contains(peer),
                "{peer} disagrees between allows and covering_peers"
            );
        }

        // And the production authority, whose override reads the map directly.
        let handout = CoordinatorHandoutAuthority::new([]);
        {
            let mut held = handout.snapshots.write().expect("fresh lock");
            for snapshot in snapshots {
                held.insert(snapshot.peer, snapshot);
            }
        }
        assert_eq!(
            handout
                .covering_peers(
                    &sessions,
                    GridId::ROOT,
                    cell,
                    1_000,
                    EXPIRE_FANOUT_MAX_RECIPIENTS
                )
                .peers,
            expected
        );

        // The default authority trusts nothing, so it addresses nobody — and
        // the defaulted method is what keeps it, and every test double,
        // compiling without a fan-out implementation of its own.
        assert!(DenyAllInterestAuthority
            .covering_peers(
                &sessions,
                GridId::ROOT,
                cell,
                1_000,
                EXPIRE_FANOUT_MAX_RECIPIENTS
            )
            .is_empty());
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
            claim_clock: Arc::new(SystemClaimClock::default()),
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
    fn a_saturated_diff_lane_sheds_rather_than_waiting_for_a_route_slot() {
        // The defect this pins: the receive loop used to `await` here, and
        // an unbounded reader kept filling the queue behind it. Waiting is
        // not one of the answers any more — a full lane refuses, now, and
        // says which bound it hit.
        let lane = Arc::new(Semaphore::new(1));
        let held = admit_diff_route(&lane, 0);
        assert!(matches!(held, DiffAdmission::Route(_)), "first slot routes");

        assert!(
            matches!(admit_diff_route(&lane, 0), DiffAdmission::ShedSaturated),
            "a full lane refuses immediately instead of queueing behind it"
        );

        drop(held);
        assert!(
            matches!(admit_diff_route(&lane, 0), DiffAdmission::Route(_)),
            "the slot is reusable once the route that held it finished"
        );
    }

    #[test]
    fn a_diff_that_outlived_the_ingress_deadline_is_shed_without_taking_a_slot() {
        // Staleness is checked before the permit, and that ordering is the
        // point: a backlog is worked off by refusing the entries whose ack
        // can no longer be worth anything, so the slot goes to a fresh diff.
        let lane = Arc::new(Semaphore::new(1));

        assert!(
            matches!(
                admit_diff_route(&lane, MAX_INGRESS_QUEUE_WAIT_US + 1),
                DiffAdmission::ShedStale
            ),
            "a diff past the ingress deadline is refused"
        );
        assert_eq!(
            lane.available_permits(),
            1,
            "a shed diff must not consume the route slot a fresh one could use"
        );

        assert!(
            matches!(
                admit_diff_route(&lane, MAX_INGRESS_QUEUE_WAIT_US),
                DiffAdmission::Route(_)
            ),
            "the deadline is inclusive: exactly at the bound is still routed"
        );
    }

    /// A router that never answers: it reports that it was entered, then
    /// parks forever. Stands in for the entity-gate convoy the valve exists
    /// to bound, without needing a real registrar to build one.
    struct StalledRouter {
        entered: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl Router for StalledRouter {
        async fn apply(
            &self,
            _record: JournalRecord,
        ) -> Result<Arc<crate::journal::AppendHandle>, Reject> {
            self.entered.fetch_add(1, Ordering::Relaxed);
            std::future::pending::<()>().await;
            unreachable!("the stalled router never answers")
        }

        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }

        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }
    }

    fn valve_route(received_at: Instant, budget_us: u64) -> DiffRoute {
        DiffRoute {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(31),
                tick: orrery_protocol::Tick::new(5),
                kind: orrery_protocol::RecordKind::ComponentDiff,
                payload: Bytes::from_static(b"state"),
                seq: 5,
                lease_id: None,
                authority_seq: None,
            },
            author: successor_node(9),
            received_at,
            strict_authority: false,
            authority_metrics: Arc::new(AuthorityMetrics::default()),
            budget_us,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_router_that_cannot_admit_a_diff_inside_its_budget_sheds_it_and_says_so() {
        // The defect the whole change is about: the ingress deadline is
        // evaluated on arrival age at dequeue, so once the receive loop went
        // instant it always passed and nothing bounded the wait that
        // followed. This is that bound, and it is evaluated after the wait.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&sent);
        let send = move |bytes| capture.lock().expect("capture lock").push(bytes);
        let entered = Arc::new(AtomicU64::new(0));
        let router: Arc<dyn Router> = Arc::new(StalledRouter {
            entered: Arc::clone(&entered),
        });
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let bulk = GatewayBulkMetrics::default();
        let ingress = GatewayIngressMetrics::default();

        // The outer deadline is a hundred budgets and exists only so that a
        // valve deleted from `within_route_budget` fails this test loudly
        // instead of hanging a CI job: `StalledRouter` never answers, so
        // without the inner bound there is nothing else to end the await.
        tokio::time::timeout(
            Duration::from_micros(25_000 * 100),
            route_diff(
                &send,
                valve_route(Instant::now(), 25_000),
                &router,
                &admission,
                &bulk,
                &ingress,
            ),
        )
        .await
        .expect("the route budget, not the test harness, must end this route");

        assert_eq!(
            entered.load(Ordering::Relaxed),
            1,
            "the router was reached; this is a downstream refusal, not an ingress one"
        );
        let counted = ingress.snapshot();
        assert_eq!(counted.shed_slow_route, 1, "the refusal is a number");
        assert_eq!(
            (counted.shed_stale, counted.shed_saturated),
            (0, 0),
            "and it is its own number: the ingress queue was not what grew"
        );
        assert!(
            sent.lock().expect("capture lock").is_empty(),
            "silence, not a BulkNack: a nack would tell the peer to discard a write it should re-offer"
        );
        assert_eq!(
            bulk.snapshot().acknowledgements,
            0,
            "a shed diff is not an acknowledged one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_diff_already_past_its_budget_on_arrival_never_reaches_the_router() {
        // Little's law in one assertion: work that cannot be answered in time
        // must not occupy the router either, or the valve relieves the client
        // and not the queue.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&sent);
        let send = move |bytes| capture.lock().expect("capture lock").push(bytes);
        let entered = Arc::new(AtomicU64::new(0));
        let router: Arc<dyn Router> = Arc::new(StalledRouter {
            entered: Arc::clone(&entered),
        });
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let ingress = GatewayIngressMetrics::default();

        // Real `std::time::Instant` arithmetic, not `tokio::time::advance`:
        // the arrival stamp is a `std::time::Instant` and tokio's paused
        // clock does not move it. That is not a detail of the test — a valve
        // that measured tokio time would stop bounding anything the moment a
        // caller paused the clock.
        let arrived = Instant::now() - Duration::from_micros(25_001);
        route_diff(
            &send,
            valve_route(arrived, 25_000),
            &router,
            &admission,
            &GatewayBulkMetrics::default(),
            &ingress,
        )
        .await;

        assert_eq!(
            entered.load(Ordering::Relaxed),
            0,
            "the budget is spent; the router must not be entered at all"
        );
        assert_eq!(ingress.snapshot().shed_slow_route, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_router_that_answers_inside_the_budget_is_acknowledged_not_shed() {
        // The mutation check for the two above: with the valve set to the
        // shipped budget and a router that answers, nothing is shed. Delete
        // the deadline arithmetic and this still passes, which is the point —
        // it is here so a valve that fires on a healthy gateway fails loudly.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&sent);
        let send = move |bytes| capture.lock().expect("capture lock").push(bytes);
        let router: Arc<dyn Router> = Arc::new(SuccessfulRouter);
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let ingress = GatewayIngressMetrics::default();

        route_diff(
            &send,
            valve_route(Instant::now(), MAX_ROUTE_ADMISSION_WAIT_US),
            &router,
            &admission,
            &GatewayBulkMetrics::default(),
            &ingress,
        )
        .await;

        assert_eq!(ingress.snapshot().shed_slow_route, 0);
        assert!(
            matches!(
                decode_datagram(&sent.lock().expect("capture lock").pop().expect("reply")),
                Some(GatewayReply::BulkAck { .. })
            ),
            "a gateway that is keeping up acknowledges"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_budget_is_the_valve_switched_off() {
        // This is the "before" leg of every A/B in docs/08-persistence.md
        // §3.6: with the budget at zero the route waits exactly as the merged
        // branch did, so the two legs differ in one number and not in code.
        let entered = Arc::new(AtomicU64::new(0));
        let router: Arc<dyn Router> = Arc::new(StalledRouter {
            entered: Arc::clone(&entered),
        });
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let ingress = GatewayIngressMetrics::default();
        let send = |_bytes: Bytes| {};

        let route = tokio::spawn({
            let router = Arc::clone(&router);
            let admission = Arc::clone(&admission);
            async move {
                route_diff(
                    &send,
                    valve_route(Instant::now(), 0),
                    &router,
                    &admission,
                    &GatewayBulkMetrics::default(),
                    &GatewayIngressMetrics::default(),
                )
                .await;
            }
        });

        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(
            !route.is_finished(),
            "a zero budget must wait, however long the router takes"
        );
        route.abort();
        assert_eq!(ingress.snapshot().shed_slow_route, 0);
    }

    #[test]
    fn the_two_deadlines_are_one_budget() {
        // One staleness policy, evaluated twice: once on arrival age at
        // dequeue and once around the router round trip. If these ever
        // diverge silently, `shed_stale` and `shed_slow_route` stop being
        // two views of the same rule and start being two rules.
        assert_eq!(MAX_ROUTE_ADMISSION_WAIT_US, MAX_INGRESS_QUEUE_WAIT_US);
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

    /// The transport-boundary histograms must land on the same bucket lattice
    /// every other D16 series uses, record into the series they name, and
    /// drain once — a cursor that re-emitted would double the ingress tail in
    /// the artifact and make the gateway look slower every 250 ms.
    #[test]
    fn boundary_metrics_land_in_their_own_series_and_drain_exactly_once() {
        let metrics = GatewayBoundaryMetrics::default();
        // 3 ms of ingress backlog on a diff that was SERVED, a 120 µs
        // hand-off, 4 096 bytes already in the driver's datagram buffer behind
        // it — and a 2 s wait on a diff that was REFUSED.
        //
        // The refused one is the point of this test. Both are "time in the
        // inbound queue", and pooling them is what made the ingress series
        // stop predicting client-observed latency: refused diffs are refused
        // *for* waiting, so they carry every long sample by construction and
        // drag a healthy served-latency distribution into the seconds.
        metrics.record_ingress(3_000);
        metrics.record_shed_age(2_000_000);
        metrics.record_reply(120, 4_096);

        let mut cursors: [GatewayServerLatencySnapshot; GATEWAY_BOUNDARY_SERIES.len()] =
            std::array::from_fn(|_| GatewayServerLatencySnapshot::default());
        let drained: Vec<(&str, Vec<GatewayBulkSample>)> = metrics
            .series()
            .into_iter()
            .zip(cursors.iter_mut())
            .map(|((series, latency), cursor)| (series, latency.delta(cursor)))
            .collect();

        assert_eq!(
            drained,
            vec![
                (
                    SERIES_GATEWAY_INGRESS_QUEUE,
                    vec![GatewayBulkSample {
                        value_us: 3_000,
                        count: 1
                    }]
                ),
                (
                    SERIES_GATEWAY_SHED_AGE,
                    vec![GatewayBulkSample {
                        value_us: 2_000_000,
                        count: 1
                    }]
                ),
                (
                    SERIES_GATEWAY_REPLY_HANDOFF,
                    vec![GatewayBulkSample {
                        value_us: 200,
                        count: 1
                    }]
                ),
                (
                    SERIES_GATEWAY_SEND_BUFFER,
                    vec![GatewayBulkSample {
                        // 4 096 B lands in (4 000, 4 500] since the lattice
                        // gained 500 µs steps through the bulk-ack band; it
                        // used to round all the way up to 5 000.
                        value_us: 4_500,
                        count: 1
                    }]
                ),
            ],
            "each measurement belongs to exactly one series, at its bucket's \
             upper bound on the shared lattice — and a refused diff's two-second \
             wait must NOT appear in the served-latency series"
        );

        let second: Vec<Vec<GatewayBulkSample>> = metrics
            .series()
            .into_iter()
            .zip(cursors.iter_mut())
            .map(|((_, latency), cursor)| latency.delta(cursor))
            .collect();
        assert!(
            second.iter().all(Vec::is_empty),
            "a drained sample must not be emitted again by the next tick"
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
                budget_us: 0,
            },
            &router,
            &admission,
            &metrics,
            &GatewayIngressMetrics::default(),
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
                budget_us: 0,
            },
            &router,
            &admission,
            &GatewayBulkMetrics::default(),
            &GatewayIngressMetrics::default(),
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
            let (rows, invalid, _) = renew_session_leases(&router, holder, &leases, 0).await;
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
            let (rows, invalid, _) = renew_session_leases(&router, holder, &leases, 0).await;
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

        let (rows, invalid, _) = renew_session_leases(&router, holder, &leases, 0).await;

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

        let (rows, invalid, _) = renew_session_leases(&router, holder, &leases, 0).await;

        assert_eq!(turns.load(Ordering::SeqCst), 1);
        assert_eq!(invalid, vec![(refused, LeaseId(1))]);
        assert_eq!(rows.len(), HELD as usize - 1);
        assert!(rows.iter().all(|row| row.entity != refused));
    }

    /// A router that renews nothing and disowns one cell, so the two halves of
    /// a refusal — *that* it failed and *why* — can be told apart.
    struct DisowningRenewalRouter {
        disowned: CellId,
        epoch: Epoch,
    }

    #[async_trait::async_trait]
    impl Router for DisowningRenewalRouter {
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
        async fn wrong_owner_epoch(
            &self,
            _grid: GridId,
            cell: CellId,
        ) -> Option<orrery_protocol::Epoch> {
            (cell == self.disowned).then_some(self.epoch)
        }
        async fn heartbeat_lease(
            &self,
            _grid: GridId,
            _cell: CellId,
            _entity: PersistId,
            _holder: NodeId,
            _lease_id: LeaseId,
            _now_ms: u64,
        ) -> Result<Option<orrery_protocol::Lease>, Reject> {
            Ok(None)
        }
    }

    /// A renewal refused because its shard is not hosted here is reported with
    /// its reason, and one refused for any other cause is not.
    ///
    /// The ack itself is unchanged in both cases — both pairs are named
    /// individually in `invalid`, because batching must not blur it. What the
    /// classification adds is the second list, and the whole value of that
    /// list is that it is *narrower* than the refusals: a peer told "wrong
    /// owner" about a lease this node does own would re-address a write that
    /// belonged here.
    #[tokio::test]
    async fn only_the_renewal_whose_shard_is_elsewhere_is_reported_as_misaddressed() {
        let holder = iroh::SecretKey::from_bytes(&[15; 32]).public();
        let shards = CellId::ROOT.children();
        let (mine, theirs) = (shards[0], shards[1]);
        let leases = vec![
            SessionLease {
                entity: PersistId::new(1),
                lease_id: LeaseId(1),
                grid: GridId::ROOT,
                cell: mine,
                owner: SessionLeaseOwner::Active(SessionGeneration(1)),
            },
            SessionLease {
                entity: PersistId::new(2),
                lease_id: LeaseId(1),
                grid: GridId::ROOT,
                cell: theirs,
                owner: SessionLeaseOwner::Active(SessionGeneration(1)),
            },
        ];
        let router: Arc<dyn Router> = Arc::new(DisowningRenewalRouter {
            disowned: theirs,
            epoch: Epoch::new(4),
        });

        let (rows, invalid, misaddressed) = renew_session_leases(&router, holder, &leases, 0).await;

        assert!(rows.is_empty());
        assert_eq!(
            invalid,
            vec![
                (PersistId::new(1), LeaseId(1)),
                (PersistId::new(2), LeaseId(1))
            ],
            "both pairs are still refused individually; the ack does not change"
        );
        assert_eq!(
            misaddressed,
            vec![MisaddressedRenewal {
                entity: PersistId::new(2),
                grid: GridId::ROOT,
                cell: theirs,
                epoch: Epoch::new(4),
            }],
            "only the lease whose shard is elsewhere carries a routing reason"
        );
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

        let (rows, invalid, _) = renew_session_leases(&router, holder, &leases, 0).await;

        assert_eq!(rows.len(), 1);
        assert_eq!(
            invalid,
            leases[1..]
                .iter()
                .map(|lease| (lease.entity, lease.lease_id))
                .collect::<Vec<_>>()
        );
    }

    /// A router that counts what the gateway actually asked it to route, and
    /// can be told whether the fence admits it.
    struct CountingFencedRouter {
        fenced: Arc<AtomicUsize>,
        admits: bool,
    }

    #[async_trait::async_trait]
    impl Router for CountingFencedRouter {
        async fn apply(
            &self,
            _record: JournalRecord,
        ) -> Result<Arc<crate::journal::AppendHandle>, Reject> {
            Err(Reject::JournalClosed)
        }
        async fn apply_fenced(
            &self,
            _record: JournalRecord,
            _holder: NodeId,
            _lease_id: LeaseId,
            _authority_seq: orrery_protocol::SeqPair,
            _now_ms: u64,
        ) -> Result<FencedApply, Reject> {
            self.fenced.fetch_add(1, Ordering::SeqCst);
            if self.admits {
                Ok(FencedApply::Accepted(
                    crate::journal::AppendHandle::completed(Lsn::new(7, 11)),
                ))
            } else {
                Ok(FencedApply::Rejected(None))
            }
        }
        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }
        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }
    }

    /// The cell a bulk diff declares reaches the router straight off the wire,
    /// and it decides whether the route is cheap or expensive.
    ///
    /// A diff at a cell this session's lease index does not name misses the
    /// fenced route's fast path and pays its fallback: one
    /// `LeaseStore::locate` — the FoundationDB read docs/14-capacity.md §5.1
    /// measured as the binding constraint on a whole box — plus a second
    /// mailbox turn. Nothing bounded how often a peer could ask for that.
    ///
    /// Refusing outright is wrong, and was the first attempt: a registrar-
    /// driven rekey moves an entity without telling the gateway, so the
    /// holder's first legitimate write at the new cell looks exactly like the
    /// abuse (`rekeyed_entity_rejects_stale_presented_cell_with_current_lease`
    /// asserts that write is acknowledged). So the mismatch buys a probe from
    /// a per-connection bucket, and an admitted probe repairs the index.
    #[tokio::test]
    async fn a_diff_at_an_unindexed_cell_probes_once_repairs_the_index_and_is_then_throttled() {
        let registry = Arc::new(PeerRegistry::new(2, 10_000, MAX_PEER_LIVE_LEASES));
        let node = iroh::SecretKey::from_bytes(&[21; 32]).public();
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

        let granted_cell = CellId::ROOT.children()[0];
        let elsewhere = CellId::ROOT.children()[1];
        let entity = PersistId::new(4_242);
        let indexed_lease = SessionLease {
            entity,
            lease_id: LeaseId(9),
            grid: GridId::ROOT,
            cell: granted_cell,
            owner: SessionLeaseOwner::Active(session.generation),
        };
        session
            .state
            .lock()
            .await
            .leases
            .insert(entity, indexed_lease);

        let fenced = Arc::new(AtomicUsize::new(0));
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let bulk_metrics = Arc::new(GatewayBulkMetrics::default());
        let authority = Arc::new(AuthorityMetrics::default());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = {
            let sent = Arc::clone(&sent);
            move |bytes: Bytes| sent.lock().expect("capture lock").push(bytes)
        };
        let uplink = |cell: CellId, tick: u64| DiffUplink {
            cell,
            grid: GridId::ROOT,
            entity,
            tick: orrery_protocol::Tick::new(tick),
            kind: orrery_protocol::RecordKind::ComponentDiff,
            payload: Bytes::from_static(b"state"),
            seq: 1,
            lease_id: Some(LeaseId(9)),
            authority_seq: Some(orrery_protocol::SeqPair::default()),
        };

        // -- a probe is routed, and an admitted probe repairs the index -----
        let admitting: Arc<dyn Router> = Arc::new(CountingFencedRouter {
            fenced: Arc::clone(&fenced),
            admits: true,
        });
        route_session_diff(
            &capture,
            uplink(elsewhere, 1),
            &session,
            &admitting,
            &admission,
            &bulk_metrics,
            &GatewayIngressMetrics::default(),
            Arc::clone(&authority),
            Instant::now(),
            0,
        )
        .await;
        assert_eq!(
            fenced.load(Ordering::SeqCst),
            1,
            "the first diff at an unindexed cell must still be routed: a rekey looks like this"
        );
        assert_eq!(authority.snapshot().misrouted_diffs, 1);
        assert_eq!(authority.snapshot().misroute_throttled, 0);
        assert_eq!(
            session.state.lock().await.leases[&entity].cell,
            elsewhere,
            "an admitted probe is proof of the entity's cell, and repairs the index"
        );

        route_session_diff(
            &capture,
            uplink(elsewhere, 2),
            &session,
            &admitting,
            &admission,
            &bulk_metrics,
            &GatewayIngressMetrics::default(),
            Arc::clone(&authority),
            Instant::now(),
            0,
        )
        .await;
        assert_eq!(fenced.load(Ordering::SeqCst), 2);
        assert_eq!(
            authority.snapshot().misrouted_diffs,
            1,
            "the repaired index costs no further probes: a rekey is one token, not a rate"
        );

        // -- an exhausted bucket refuses without routing ---------------------
        // A peer whose cell is simply wrong never gets an admission, so it
        // never repairs the index, so it drains its allowance and stops.
        {
            let mut peer = session.state.lock().await;
            peer.leases.insert(entity, indexed_lease);
            peer.misroute_bucket = MisrouteBucket {
                token_millis: 0,
                // Far future, so `take` cannot replenish under this test.
                updated_ms: u64::MAX,
            };
        }
        let rejecting: Arc<dyn Router> = Arc::new(CountingFencedRouter {
            fenced: Arc::clone(&fenced),
            admits: false,
        });
        sent.lock().expect("capture lock").clear();
        route_session_diff(
            &capture,
            uplink(elsewhere, 3),
            &session,
            &rejecting,
            &admission,
            &bulk_metrics,
            &GatewayIngressMetrics::default(),
            Arc::clone(&authority),
            Instant::now(),
            0,
        )
        .await;
        assert_eq!(
            fenced.load(Ordering::SeqCst),
            2,
            "with no probe left, the diff must not reach the router at all"
        );
        assert_eq!(authority.snapshot().misrouted_diffs, 2);
        assert_eq!(authority.snapshot().misroute_throttled, 1);
        let bytes = sent.lock().expect("capture lock").pop().expect("a reply");
        assert!(
            matches!(
                decode_datagram(&bytes),
                Some(GatewayReply::BulkNack { entity: e, tick, lease: None, .. })
                    if e == entity && tick == orrery_protocol::Tick::new(3)
            ),
            "the client must be told, not left waiting"
        );

        // And a diff at the indexed cell still routes, with the bucket empty:
        // the throttle is on the expensive branch, not on the connection.
        route_session_diff(
            &capture,
            uplink(granted_cell, 4),
            &session,
            &rejecting,
            &admission,
            &bulk_metrics,
            &GatewayIngressMetrics::default(),
            Arc::clone(&authority),
            Instant::now(),
            0,
        )
        .await;
        assert_eq!(fenced.load(Ordering::SeqCst), 3);
        assert_eq!(authority.snapshot().misrouted_diffs, 2);

        drop(registry);
    }

    /// The cheapest way onto the fenced route's expensive branch needs no
    /// lease at all, and it used to be free.
    ///
    /// The probe predicate was `indexed.is_some_and(...)`, so an entity this
    /// session holds no lease for read as "not misrouted": no token, no
    /// counter, no throttle, and the diff routed. On the router side an
    /// entity with no row anywhere is `Rejected(None)`, which is precisely
    /// the answer that does *not* short-circuit — it takes the fallback and
    /// spends one `LeaseStore::locate` (`tests/fenced_route_bounds.rs`,
    /// "still exactly one locate"). So a peer could drive one FoundationDB
    /// read per diff at its own send rate against entities it does not hold,
    /// while `misrouted_diffs` — the counter docs/08-persistence.md §2.1.2
    /// names as the alarm for exactly that — read zero throughout.
    ///
    /// The reviewer's probe was 1000 diffs at a foreign cell for an entity
    /// absent from `peer.leases`, and it measured `routed_to_router=1000
    /// misrouted_diffs=0 misroute_throttled=0`. This is that probe.
    ///
    /// The bucket is pinned at `updated_ms: u64::MAX` so `take` cannot
    /// replenish: what the router sees is then exactly the burst, and the
    /// assertion is a count rather than a bound that a slow machine could
    /// satisfy by accident.
    #[tokio::test]
    async fn diffs_for_an_entity_this_session_holds_no_lease_for_are_metered_too() {
        const DIFFS: u64 = 1_000;

        let registry = Arc::new(PeerRegistry::new(2, 10_000, MAX_PEER_LIVE_LEASES));
        let node = iroh::SecretKey::from_bytes(&[22; 32]).public();
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

        // The whole point: nothing is inserted into `peer.leases`. The peer
        // presents a `lease_id` it was never granted, which is what a peer
        // with no lease at all has to do.
        let foreign = CellId::ROOT.children()[1];
        let entity = PersistId::new(9_101);
        {
            let mut peer = session.state.lock().await;
            assert!(peer.leases.is_empty(), "the fixture holds no lease");
            peer.misroute_bucket = MisrouteBucket {
                token_millis: MisrouteBucket::BURST_TOKEN_MILLIS,
                updated_ms: u64::MAX,
            };
        }

        let fenced = Arc::new(AtomicUsize::new(0));
        let admission: SharedBulkAckAdmission = Arc::new(AlwaysDurableAdmission);
        let bulk_metrics = Arc::new(GatewayBulkMetrics::default());
        let authority = Arc::new(AuthorityMetrics::default());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let capture = {
            let sent = Arc::clone(&sent);
            move |bytes: Bytes| sent.lock().expect("capture lock").push(bytes)
        };
        let uplink = |tick: u64| DiffUplink {
            cell: foreign,
            grid: GridId::ROOT,
            entity,
            tick: orrery_protocol::Tick::new(tick),
            kind: orrery_protocol::RecordKind::ComponentDiff,
            payload: Bytes::from_static(b"state"),
            seq: 1,
            lease_id: Some(LeaseId(9)),
            authority_seq: Some(orrery_protocol::SeqPair::default()),
        };

        let rejecting: Arc<dyn Router> = Arc::new(CountingFencedRouter {
            fenced: Arc::clone(&fenced),
            admits: false,
        });
        for tick in 1..=DIFFS {
            route_session_diff(
                &capture,
                uplink(tick),
                &session,
                &rejecting,
                &admission,
                &bulk_metrics,
                &GatewayIngressMetrics::default(),
                Arc::clone(&authority),
                Instant::now(),
                0,
            )
            .await;
        }

        let snapshot = authority.snapshot();
        assert_eq!(
            fenced.load(Ordering::SeqCst) as u64,
            MisrouteBucket::BURST_PROBES,
            "an unleased entity must buy the same bounded probe a wrong cell does, \
             not one fallback locate per diff at the peer's send rate"
        );
        assert_eq!(
            snapshot.misrouted_diffs, DIFFS,
            "the documented alarm must see this vector"
        );
        assert_eq!(
            snapshot.unindexed_diffs, DIFFS,
            "and must say which vector it is"
        );
        assert_eq!(
            snapshot.misroute_throttled,
            DIFFS - MisrouteBucket::BURST_PROBES,
            "everything past the burst is refused without routing"
        );
        let bytes = sent.lock().expect("capture lock").pop().expect("a reply");
        assert!(
            matches!(
                decode_datagram(&bytes),
                Some(GatewayReply::BulkNack { entity: e, tick, lease: None, .. })
                    if e == entity && tick == orrery_protocol::Tick::new(DIFFS)
            ),
            "a throttled peer is told, and told without a row it holds no lease for"
        );

        // An *admitted* unindexed probe must not invent an index entry. The
        // repair path writes through `get_mut` precisely so it cannot: a
        // `SessionLease` the gateway never granted would enter
        // `lease_capacity` accounting, `resolve_renewals` and the park loop in
        // `cleanup_peer_session`, and admission does not name the row's owner
        // generation. So this vector is metered forever rather than repaired,
        // which is what makes `unindexed_diffs` a level and not a spike.
        {
            let mut peer = session.state.lock().await;
            peer.misroute_bucket = MisrouteBucket {
                token_millis: MisrouteBucket::BURST_TOKEN_MILLIS,
                updated_ms: u64::MAX,
            };
        }
        let admitting: Arc<dyn Router> = Arc::new(CountingFencedRouter {
            fenced: Arc::clone(&fenced),
            admits: true,
        });
        route_session_diff(
            &capture,
            uplink(DIFFS + 1),
            &session,
            &admitting,
            &admission,
            &bulk_metrics,
            &GatewayIngressMetrics::default(),
            Arc::clone(&authority),
            Instant::now(),
            0,
        )
        .await;
        assert_eq!(
            fenced.load(Ordering::SeqCst) as u64,
            MisrouteBucket::BURST_PROBES + 1,
            "a refilled bucket routes the probe"
        );
        assert!(
            session.state.lock().await.leases.is_empty(),
            "an admitted probe must not fabricate a lease the gateway never granted"
        );
        assert_eq!(authority.snapshot().unindexed_diffs, DIFFS + 1);

        drop(registry);
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
                    &GatewayIngressMetrics::default(),
                    Arc::new(AuthorityMetrics::default()),
                    Instant::now(),
                    0,
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

    /// A router whose renewals block until released, recording arrival order.
    ///
    /// The instrument for the lease lane: `entered` says *when* an operation
    /// reached the router, so a second arrival while the first is still
    /// blocked is visible as an extra message rather than having to be
    /// inferred from a timing.
    struct GatedRenewalRouter {
        entered: tokio::sync::mpsc::UnboundedSender<PersistId>,
        release: Arc<Semaphore>,
        holder: NodeId,
    }

    #[async_trait::async_trait]
    impl Router for GatedRenewalRouter {
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
        async fn heartbeat_leases(
            &self,
            _grid: GridId,
            _holder: NodeId,
            renew: &[LeaseRenewal],
            _now_ms: u64,
        ) -> Result<Vec<Option<orrery_protocol::Lease>>, Reject> {
            for entry in renew {
                let _ = self.entered.send(entry.entity);
            }
            let permit = Arc::clone(&self.release)
                .acquire_owned()
                .await
                .expect("release semaphore stays open");
            drop(permit);
            Ok(renew
                .iter()
                .map(|entry| {
                    Some(orrery_protocol::Lease {
                        entity: entry.entity,
                        holder: Some(self.holder),
                        seq: SeqPair::default(),
                        lease_id: entry.lease_id,
                        expires_at: u64::MAX,
                        flags: LeaseFlags::default(),
                        bound_to: None,
                    })
                })
                .collect())
        }
    }

    /// Give `session` `count` leases, through the same reserve-then-commit
    /// pair a granted claim uses.
    async fn hold_leases(session: &PeerSession, count: u64) -> Vec<PersistId> {
        let mut held = Vec::new();
        for id in 1..=count {
            let entity = PersistId::new(id);
            assert!(
                session.try_reserve_lease_slot().await,
                "capacity for a test lease"
            );
            assert!(
                matches!(
                    session
                        .complete_lease_claim(Some(SessionLease {
                            entity,
                            lease_id: LeaseId(1),
                            grid: GridId::ROOT,
                            cell: CellId::ROOT,
                            owner: SessionLeaseOwner::Active(session.generation),
                        }))
                        .await,
                    LeaseClaimCompletion::Granted
                ),
                "a reserved claim commits"
            );
            held.push(entity);
        }
        held
    }

    fn valid_authorization(node: NodeId) -> GatewayAuthorization {
        GatewayAuthorization::Valid(SessionTokenClaimsV1::new(
            orrery_protocol::AccountId::new(1),
            node,
            UnixMillis::new(0),
            orrery_protocol::SessionTokenTtlMs::new(1_000),
            orrery_protocol::SessionStanding::Good,
            orrery_protocol::IssuerKeyId::new(1),
        ))
    }

    /// The lease lane is a FIFO of one: a blocked operation is never overtaken.
    ///
    /// This is the property that makes moving lease work off the receive loop
    /// safe. Authority is a fencing protocol — a claim that overtook a
    /// heartbeat, or a divest that overtook a claim, would reorder the
    /// reserve-then-commit pair (`try_reserve_lease_slot` /
    /// `complete_lease_claim`) and the per-entity index it writes
    /// (`PeerState::leases`). `tokio::spawn` per message is the obvious fix
    /// and it fails here: with the first operation held inside the router, a
    /// spawned second one reaches the router immediately, and the arrival
    /// channel below carries two entries instead of one.
    #[tokio::test]
    async fn a_blocked_lease_operation_does_not_let_the_next_one_overtake_it() {
        const HELD: u64 = 3;
        let node = iroh::SecretKey::from_bytes(&[21; 32]).public();
        let registry = Arc::new(PeerRegistry::new(2, 10_000, MAX_PEER_LIVE_LEASES));
        let session = registry
            .activate(node, valid_authorization(node), b"lane", None, 0, 0)
            .await
            .expect("valid peer is admitted");
        let held = hold_leases(&session, HELD).await;

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let router: Arc<dyn Router> = Arc::new(GatedRenewalRouter {
            entered: entered_tx,
            release: Arc::clone(&release),
            holder: node,
        });
        let (lease_tx, worker) = spawn_lease_lane(LeaseContext {
            send: Arc::new(|_bytes: Bytes| {}),
            router,
            redistributor: Arc::new(parking_redistributor(Arc::clone(&registry))),
            interest_authority: Arc::new(DenyAllInterestAuthority),
            claim_clock: Arc::new(SystemClaimClock::default()),
            remote: node,
        });

        // Three heartbeats, offered back to back exactly as the receive loop
        // offers them: one entity each, so each is one router round trip.
        for entity in &held {
            lease_tx
                .send(LeaseWork {
                    session: session.clone(),
                    message: LeaseMsg::Heartbeat {
                        renew: vec![(*entity, LeaseId(1))],
                        tick: orrery_protocol::Tick::new(1),
                    },
                })
                .await
                .expect("the lane accepts every offered operation");
        }

        // The first reaches the router and blocks there.
        let first = tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .expect("the first heartbeat reaches the router")
            .expect("arrival channel stays open");
        assert_eq!(first, held[0]);

        // And nothing follows it while it is held. The window is generous in
        // the direction that matters: a serial lane never produces a second
        // arrival however long this waits, while a spawn-per-message lane
        // produces one in microseconds.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            matches!(
                entered_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a second lease operation overtook the one still in flight"
        );

        // Released, the rest run — in arrival order, not in whatever order
        // the runtime happens to wake them.
        release.add_permits(HELD as usize);
        let mut order = vec![first];
        for _ in 1..HELD {
            order.push(
                tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
                    .await
                    .expect("every queued heartbeat is served")
                    .expect("arrival channel stays open"),
            );
        }
        assert_eq!(order, held, "the lane serves in arrival order");

        drop(lease_tx);
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("the worker ends with its queue")
            .expect("the worker does not panic");
        drop(registry);
    }

    /// A refused lease operation answers a claimant and lies to nobody else.
    #[test]
    fn a_full_lease_lane_refuses_a_claim_and_stays_silent_to_a_holder() {
        let entity = PersistId::new(7);
        let replies = Arc::new(Mutex::new(Vec::new()));
        let send = {
            let replies = Arc::clone(&replies);
            move |bytes: Bytes| {
                replies.lock().expect("replies").push(bytes);
            }
        };

        refuse_saturated_lease(
            &send,
            &LeaseMsg::Claim {
                claim_id: orrery_protocol::ClaimId(3),
                entity,
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                kind: orrery_protocol::ClaimKind::Weak,
                basis: orrery_protocol::ClaimBasis::Explicit,
                observed: SeqPair::default(),
                tick: orrery_protocol::Tick::new(1),
            },
        );
        let frame = replies.lock().expect("replies").pop().expect("one reply");
        let reply: GatewayReply = decode_stream_frame(&frame).expect("a decodable reply");
        assert!(
            matches!(
                reply,
                GatewayReply::Lease {
                    message: LeaseMsg::Deny {
                        claim_id: Some(claim_id),
                        entity: denied,
                        reason: orrery_protocol::DenyReason::RateLimited,
                        retry_after_ms: LEASE_LANE_RETRY_AFTER_MS,
                    }
                } if claim_id == orrery_protocol::ClaimId(3) && denied == entity
            ),
            "a claimant is told to come back, with a delay"
        );

        // A holder is not. `HeartbeatAck { invalid }` would make it stop
        // writing to entities it still owns, and it re-heartbeats anyway.
        refuse_saturated_lease(
            &send,
            &LeaseMsg::Heartbeat {
                renew: vec![(entity, LeaseId(1))],
                tick: orrery_protocol::Tick::new(1),
            },
        );
        assert!(
            replies.lock().expect("replies").is_empty(),
            "a refused heartbeat is counted, never answered with a lie"
        );
    }
}
