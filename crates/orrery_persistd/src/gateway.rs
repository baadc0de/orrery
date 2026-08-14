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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::{Semaphore, oneshot};
use tracing::{debug, warn};

use orrery_protocol::channels::{
    Channel, decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag,
};
use orrery_protocol::{
    AreaPage, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOutcome,
    JournalRecord, Lsn, MAX_AREA_PAGE_FRAME_BYTES, NodeId, PROTOCOL_VERSION, PersistId,
    REASON_BAD_SIGNATURE, REASON_ISSUER_MISMATCH, REASON_NO_EXECUTOR,
};

use crate::cluster::Router;
use crate::intent::{
    IntentVerdict, PermissiveValidator, SharedExecutor, SharedValidator, error_outcome,
};
use crate::payload_crc;

/// The ALPN the gateway advertises and accepts. Matches the client's
/// `orrery_persist_client::gateway::GATEWAY_ALPN`.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The admission response byte, mirroring `aeronet_iroh`'s `ACCEPTED`.
const ACCEPTED: u8 = 0;

/// How many client messages one connection routes concurrently.
///
/// Per-message routing is spawned (`tokio::spawn`) with this semaphore bounding
/// the in-flight routes per connection, so a slow 27-cell subscribe (FDB cold
/// scans) does not head-of-line block diffs — and their acks — on the same
/// connection (D16: the ack *is* the client-observed durability contract).
const MAX_INFLIGHT_ROUTES_PER_CONN: usize = 8;

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
                let send_failures = Arc::clone(&send_failures);
                tokio::spawn(handle_connection(
                    incoming,
                    router,
                    gateway,
                    protocol,
                    executor,
                    validator,
                    bulk_ack_admission,
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
/// [`MAX_INFLIGHT_ROUTES_PER_CONN`] per connection) so a slow 27-cell
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
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_ROUTES_PER_CONN));

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
            GatewayMsg::Diff { diff } => {
                let send = Arc::clone(&send);
                let router = Arc::clone(&router);
                let bulk_ack_admission = Arc::clone(&bulk_ack_admission);
                let permit = Arc::clone(&inflight).acquire_owned().await;
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_diff(send.as_ref(), diff, remote, &router, &bulk_ack_admission)
                                .await;
                        });
                    }
                    Err(_) => {
                        route_diff(send.as_ref(), diff, remote, &router, &bulk_ack_admission).await
                    }
                }
            }
            GatewayMsg::Subscribe { grid, cells } => {
                let send = Arc::clone(&send);
                let router = Arc::clone(&router);
                let permit = Arc::clone(&inflight).acquire_owned().await;
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
                route_intent(send.as_ref(), intent, remote, &executor, &validator).await;
            }
        }
    }
}

/// Execute one submitted intent and reply with its outcome (D11 §2.2).
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
async fn route_intent(
    send: &(dyn Fn(Bytes) + Send + Sync),
    intent: Intent,
    remote: NodeId,
    executor: &Option<SharedExecutor>,
    validator: &SharedValidator,
) {
    let intent_id = intent.intent_id;

    // 1. Signature (docs/08-persistence.md §2.2: signature checks at the
    //    edge, before any transaction work).
    if !intent.verify_issuer() {
        let reply = GatewayReply::IntentAck {
            intent_id,
            outcome: IntentOutcome::Rejected {
                reason: REASON_BAD_SIGNATURE,
            },
        };
        send(Bytes::from(encode_stream_frame(&reply)));
        return;
    }

    // 2. Issuer binding (the connection's authenticated id is the only
    //    identity the gateway can trust).
    if intent.issuer != remote {
        let reply = GatewayReply::IntentAck {
            intent_id,
            outcome: IntentOutcome::Rejected {
                reason: REASON_ISSUER_MISMATCH,
            },
        };
        send(Bytes::from(encode_stream_frame(&reply)));
        return;
    }

    // 3. Admission (the Ruleset stub for now).
    let precheck = match validator.validate(&intent) {
        IntentVerdict::Admit(precheck) => precheck,
        IntentVerdict::Reject { reason } => {
            let reply = GatewayReply::IntentAck {
                intent_id,
                outcome: IntentOutcome::Rejected { reason },
            };
            send(Bytes::from(encode_stream_frame(&reply)));
            return;
        }
    };
    // The FDB executor derives its read set from the intent's ops; the
    // precheck's named keys are reserved for a Ruleset-linked executor.
    let _ = precheck;

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
) {
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

    match result {
        Ok(lsn) => {
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

    struct SuccessfulRouter;

    #[async_trait::async_trait]
    impl Router for SuccessfulRouter {
        async fn apply(&self, _record: JournalRecord) -> Result<Lsn, Reject> {
            Ok(Lsn::new(7, 11))
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
            },
            iroh::SecretKey::from_bytes(&[1; 32]).public(),
            &router,
            &admission,
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
    }
}
