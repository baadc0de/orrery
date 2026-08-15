//! The gateway: the iroh endpoint that terminates client gateway sessions and
//! routes them onto the cell-actor runtime (docs/10-crates.md §9, §11).
//!
//! This is the server mirror of `orrery_persist_client::gateway`. A client
//! connects over iroh (ALPN `orrery/gateway/0`), completes the aeronet-style
//! admission handshake (one uni-stream carrying `[ACCEPTED]`), then streams
//! tagged datagrams carrying [`GatewayMsg`]s:
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
//! - [`GatewayMsg::Hello`] → acknowledge with the gateway node id + protocol
//!   version.
//!
//! The transport is the **raw** iroh endpoint — this crate is **Bevy-free**
//! (D15) and does not run the aeronet session stack. It speaks exactly the wire
//! surface `aeronet_iroh`'s client side expects (admission uni-stream, then
//! datagrams), so the existing gateway client connects unmodified.
//!
//! Because `CellRuntime` is `Send` but not `Sync` (its `CellActorHandle`s hold
//! `JoinHandle`s), the gateway shares it behind a `tokio::sync::Mutex`. For a
//! single persistd node this is correct serialization; a real distributed
//! deployment would route by rendezvous placement instead (docs/08-persistence.md
//! §3), but the current reference binary does not ship that transport.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
};
use orrery_protocol::{
    AreaPage, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOutcome,
    JournalRecord, Lsn, NodeId, PersistId, MAX_AREA_PAGE_FRAME_BYTES, PROTOCOL_VERSION,
    REASON_BAD_SIGNATURE, REASON_ISSUER_MISMATCH, REASON_NO_EXECUTOR,
};

use crate::actor::Reject;
use crate::cluster::Router;
use crate::intent::{
    error_outcome, IntentVerdict, PermissiveValidator, SharedExecutor, SharedValidator,
};
use crate::payload_crc;

/// The ALPN the gateway advertises and accepts. Matches the client's
/// `orrery_persist_client::gateway::GATEWAY_ALPN`.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The admission response byte, mirroring `aeronet_iroh`'s `ACCEPTED`.
const ACCEPTED: u8 = 0;

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

const BULK_LATENCY_BOUNDARIES_US: [u64; 22] = [
    50, 100, 200, 500, 1_000, 2_000, 3_000, 5_000, 7_000, 10_000, 15_000, 20_000, 30_000, 50_000,
    75_000, 100_000, 150_000, 200_000, 300_000, 500_000, 750_000, 1_000_000,
];
const NUM_BULK_LATENCY_BUCKETS: usize = BULK_LATENCY_BOUNDARIES_US.len() + 1;

/// One compact server-side bulk latency histogram bucket.
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

/// The current single-node policy: its ownership is always fresh.
#[derive(Debug, Default)]
pub struct FreshBulkAckAdmission;

impl BulkAckAdmission for FreshBulkAckAdmission {
    fn assess(&self, _grid: GridId, _cell: CellId) -> BulkAckDisposition {
        BulkAckDisposition::Durable
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
    /// The intent admission validator (D11 §2.2, first stage). Defaults to
    /// the permissive stub so the harness runs unconfigured; a linked
    /// `Ruleset` swaps in real validation.
    pub validator: SharedValidator,
    /// Ownership/fence freshness policy for bulk acknowledgements. The default
    /// preserves the current single-node durable-ack behavior; activation can
    /// inject its three-second fence freshness monitor here.
    pub bulk_ack_admission: SharedBulkAckAdmission,
    /// Bulk acknowledgement stage telemetry shared with the metrics reporter.
    pub bulk_metrics: Option<Arc<GatewayBulkMetrics>>,
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
            validator: Arc::new(PermissiveValidator),
            bulk_ack_admission: Arc::new(FreshBulkAckAdmission),
            bulk_metrics: None,
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

/// A running gateway: an iroh endpoint that accepts client sessions and routes
/// them onto a [`Router`] (a single runtime or a test cluster harness).
pub struct GatewayServer {
    endpoint: Arc<Endpoint>,
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
        let validator = config.validator;
        let bulk_ack_admission = config.bulk_ack_admission;
        let bulk_metrics = config.bulk_metrics;
        let (shutdown, rx) = oneshot::channel();
        let send_failures = Arc::new(AtomicU64::new(0));
        let join = tokio::spawn(accept_loop(
            endpoint.clone(),
            router,
            gateway,
            protocol,
            executor,
            validator,
            bulk_ack_admission,
            bulk_metrics,
            Arc::clone(&send_failures),
            rx,
        ));
        Ok(Self {
            endpoint,
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
    validator: SharedValidator,
    bulk_ack_admission: SharedBulkAckAdmission,
    bulk_metrics: Option<Arc<GatewayBulkMetrics>>,
    send_failures: Arc<AtomicU64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let router = Arc::clone(&router);
                let executor = executor.clone();
                let validator = Arc::clone(&validator);
                let bulk_ack_admission = Arc::clone(&bulk_ack_admission);
                let bulk_metrics = bulk_metrics.clone();
                let send_failures = Arc::clone(&send_failures);
                tokio::spawn(handle_connection(
                    incoming,
                    router,
                    gateway,
                    protocol,
                    executor,
                    validator,
                    bulk_ack_admission,
                    bulk_metrics,
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
    validator: SharedValidator,
    bulk_ack_admission: SharedBulkAckAdmission,
    bulk_metrics: Option<Arc<GatewayBulkMetrics>>,
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

    let send: Arc<dyn Fn(Bytes) + Send + Sync> = {
        let conn = Arc::clone(&conn);
        Arc::new(move |bytes: Bytes| {
            let len = bytes.len();
            if let Err(e) = conn.send_datagram(bytes) {
                // Never swallow a failed send: an oversize page or a torn
                // connection is counted and logged, not silently dropped.
                send_failures.fetch_add(1, Ordering::Relaxed);
                warn!(?e, %remote, len, "gateway: reply datagram send failed");
            }
        })
    };
    let inflight_diffs = Arc::new(Semaphore::new(MAX_INFLIGHT_DIFF_ROUTES_PER_CONN));
    let inflight_control = Arc::new(Semaphore::new(MAX_INFLIGHT_CONTROL_ROUTES_PER_CONN));
    let inflight_intents = Arc::new(Semaphore::new(MAX_INFLIGHT_INTENT_ROUTES_PER_CONN));

    loop {
        let pkt = match conn.read_datagram().await {
            Ok(pkt) => pkt,
            Err(e) => {
                debug!(?e, %remote, "gateway: connection closed");
                break;
            }
        };
        let Some((channel, _)) = untag(&pkt) else {
            continue;
        };
        let msg: Option<GatewayMsg> = match channel {
            Channel::State => decode_datagram(&pkt),
            Channel::Control => decode_stream_frame(&pkt),
        };
        let Some(msg) = msg else {
            debug!(%remote, "gateway: undecodable message");
            continue;
        };
        match msg {
            GatewayMsg::Hello { .. } => {
                let reply = GatewayReply::HelloAck { gateway, protocol };
                send(Bytes::from(encode_stream_frame(&reply)));
            }
            // The P3 wire lane is present before the distributed registrar is
            // wired into this P2 gateway process. Never silently accept a
            // lease request: an explicit denial keeps optimistic clients from
            // treating an unsupported endpoint as an authority grant.
            GatewayMsg::Lease { message } => {
                if let orrery_protocol::LeaseMsg::Claim { entity, .. } = message {
                    let reply = GatewayReply::Lease {
                        message: orrery_protocol::LeaseMsg::Deny {
                            entity,
                            reason: orrery_protocol::DenyReason::NotEligible,
                            retry_after_ms: 0,
                        },
                    };
                    send(Bytes::from(encode_stream_frame(&reply)));
                }
            }
            GatewayMsg::Diff { diff } => {
                let received_at = Instant::now();
                let send = Arc::clone(&send);
                let router = Arc::clone(&router);
                let bulk_ack_admission = Arc::clone(&bulk_ack_admission);
                let bulk_metrics = bulk_metrics.clone();
                let permit = Arc::clone(&inflight_diffs).acquire_owned().await;
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_diff(
                                send.as_ref(),
                                diff,
                                remote,
                                &router,
                                &bulk_ack_admission,
                                bulk_metrics.as_deref(),
                                received_at,
                            )
                            .await;
                        });
                    }
                    Err(_) => {
                        route_diff(
                            send.as_ref(),
                            diff,
                            remote,
                            &router,
                            &bulk_ack_admission,
                            bulk_metrics.as_deref(),
                            received_at,
                        )
                        .await
                    }
                }
            }
            GatewayMsg::Subscribe { grid, cells } => {
                let send = Arc::clone(&send);
                let router = Arc::clone(&router);
                let permit = Arc::clone(&inflight_control).acquire_owned().await;
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_subscribe(send.as_ref(), grid, cells, remote, &router).await;
                        });
                    }
                    Err(_) => route_subscribe(send.as_ref(), grid, cells, remote, &router).await,
                }
            }
            GatewayMsg::SubmitIntent { intent } => {
                // Keep signature/identity/admission checks at the edge, then
                // route the potentially slow FDB transaction on its own
                // bounded lane. In particular, never await a semaphore here:
                // waiting would recreate the receive-loop HOL blocking this
                // lane is intended to prevent.
                if let Err(outcome) = admit_intent(&intent, remote, validator.as_ref()) {
                    send_intent_reply(send.as_ref(), intent.intent_id, outcome);
                    continue;
                }
                match reserve_intent_lane(Arc::clone(&inflight_intents)) {
                    Ok(permit) => {
                        let send = Arc::clone(&send);
                        let executor = executor.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            execute_admitted_intent(send.as_ref(), intent, &executor).await;
                        });
                    }
                    Err(outcome) => {
                        // There is deliberately no deferred task waiting for
                        // capacity. The client receives a definitive outcome
                        // and may submit a new, idempotently keyed intent on
                        // its normal retry policy.
                        warn!(%remote, intent_id = intent.intent_id, "gateway: intent lane saturated");
                        send_intent_reply(send.as_ref(), intent.intent_id, outcome);
                    }
                }
            }
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
///    authenticated `remote` id: a peer may not submit intents in another's
///    name.
/// 3. **Admission** — the [`PermissiveValidator`] default admits everything;
///    a linked `Ruleset` rejects with its own reason code.
/// 4. **Execution** — the configured [`IntentExecutor`]'s future must resolve
///    before the ack is sent, so a `Committed` outcome implies a durable
///    commit (RPO 0). With no executor configured the reply is
///    [`REASON_NO_EXECUTOR`] — the gateway never acks a commit that did not
///    happen (the pre-existing stub's inverted RPO-0).
fn admit_intent(
    intent: &Intent,
    remote: NodeId,
    validator: &dyn crate::intent::IntentValidator,
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
    if intent.issuer != remote {
        return Err(IntentOutcome::Rejected {
            reason: REASON_ISSUER_MISMATCH,
        });
    }

    // 3. Admission (the Ruleset stub for now).
    let precheck = match validator.validate(intent) {
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
    send_intent_reply(send, intent_id, outcome);
}

/// Encode and send an intent result. Every path uses this helper so an intent
/// has exactly one definitive acknowledgement, including lane saturation.
fn send_intent_reply(
    send: &(dyn Fn(Bytes) + Send + Sync),
    intent_id: u128,
    outcome: IntentOutcome,
) {
    let reply = GatewayReply::IntentAck { intent_id, outcome };
    send(Bytes::from(encode_stream_frame(&reply)));
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

/// Journal a bulk diff via the owning cell actor, then ack with the durable
/// LSN (or nack on rejection). The gateway fills in the server-assigned
/// `epoch`/`lsn`/`author`/`crc` (docs/08-persistence.md §2.1).
async fn route_diff(
    send: &(dyn Fn(Bytes) + Send + Sync),
    diff: DiffUplink,
    author: NodeId,
    router: &Arc<dyn Router>,
    bulk_ack_admission: &SharedBulkAckAdmission,
    bulk_metrics: Option<&GatewayBulkMetrics>,
    received_at: Instant,
) {
    let route_started = Instant::now();
    let route_queue_us = elapsed_us(received_at);
    let entity = diff.entity;
    let tick = diff.tick;
    let crc = payload_crc(&diff.payload);

    let result = router
        .apply(JournalRecord {
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
        })
        .await;
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
            if let Some(bulk_metrics) = bulk_metrics {
                bulk_metrics.record(
                    route_queue_us,
                    router_apply_us,
                    journal_wait_us,
                    elapsed_us(reply_started),
                    elapsed_us(received_at),
                );
            }
        }
        Err(_) => {
            let reply = GatewayReply::BulkNack {
                entity,
                tick,
                reason: 1,
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
) {
    // A per-send page counter: each cell's page (and each chunk of it) is
    // stamped with a distinct `page_seq`, so a client's reassembly never mixes
    // chunks of two sends of the same cell (a retried subscribe re-sends the
    // page under a new seq).
    let mut page_seq = 0u32;
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
                }
            }
            Err(e) => {
                let kind = if live {
                    orrery_protocol::AREA_LOAD_ERR_LIVE
                } else {
                    orrery_protocol::AREA_LOAD_ERR_COLD
                };
                warn!(?e, ?cell, %grid, %remote, kind, "gateway: area-load cell read failed");
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
/// The lane is packet-only (D3 datagrams; the reliable-stream class of
/// docs/08-persistence.md §2.1 does not exist in this build), so an oversized
/// frame is rejected by QUIC and lost — chunking is the P2 answer: sequence
/// the frames (`page_index`/`last`) and let the client reassemble. The frame
/// cap is conservative; if one entity's bag alone cannot fit, its frame is
/// emitted oversize and the send is counted (an entity that big is a Ruleset
/// bug, not a transport problem).
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
    use std::sync::Mutex;

    use super::*;
    use crate::actor::{Reject, SnapshotPage};
    use crate::fence::{
        FenceFreshnessConfig, FenceFreshnessMonitor, FenceOutcome, FenceRow, FenceStatus,
        FenceStore, MemFenceStore,
    };

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
    fn default_bulk_ack_admission_is_fresh() {
        assert_eq!(
            FreshBulkAckAdmission.assess(GridId::ROOT, CellId::ROOT),
            BulkAckDisposition::Durable
        );
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
            DiffUplink {
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
            iroh::SecretKey::from_bytes(&[1; 32]).public(),
            &router,
            &admission,
            Some(&metrics),
            Instant::now(),
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
            DiffUplink {
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
            iroh::SecretKey::from_bytes(&[2; 32]).public(),
            &router,
            &admission,
            None,
            Instant::now(),
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
}
