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
//! - [`GatewayMsg::SubmitIntent`] → the intent-validator **stub**: accepts the
//!   wire shape and commits optimistically without minting entities. Real
//!   signature/K-of-N/`Ruleset` validation lands in P5.
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
//! single persistd node this is correct serialization; a multi-node deployment
//! would route by rendezvous placement instead (docs/08-persistence.md §3).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::{oneshot, Semaphore};
use tracing::{debug, warn};

use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
};
use orrery_protocol::{
    AreaPage, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, IntentOutcome,
    JournalRecord, Lsn, NodeId, PersistId, Tick, MAX_AREA_PAGE_FRAME_BYTES, PROTOCOL_VERSION,
};

use crate::cluster::Router;
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

/// Configuration for the [`GatewayServer`].
#[derive(Debug, Clone)]
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
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            alpn: GATEWAY_ALPN.to_vec(),
            relay_mode: RelayMode::Disabled,
            secret_key: None,
            bind: "127.0.0.1:0".parse().expect("static valid loopback addr"),
            protocol_version: PROTOCOL_VERSION,
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
/// them onto a [`Router`] (a single runtime or a multi-node cluster).
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
        let (shutdown, rx) = oneshot::channel();
        let tick = Arc::new(AtomicU64::new(0));
        let send_failures = Arc::new(AtomicU64::new(0));
        let join = tokio::spawn(accept_loop(
            endpoint.clone(),
            router,
            gateway,
            protocol,
            tick,
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
    tick: Arc<AtomicU64>,
    send_failures: Arc<AtomicU64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let router = Arc::clone(&router);
                let tick = Arc::clone(&tick);
                let send_failures = Arc::clone(&send_failures);
                tokio::spawn(handle_connection(
                    incoming,
                    router,
                    gateway,
                    protocol,
                    tick,
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
async fn handle_connection(
    incoming: iroh::endpoint::Incoming,
    router: Arc<dyn Router>,
    gateway: NodeId,
    protocol: u16,
    tick: Arc<AtomicU64>,
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
                let permit = Arc::clone(&inflight).acquire_owned().await;
                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            route_diff(send.as_ref(), diff, remote, &router).await;
                        });
                    }
                    Err(_) => route_diff(send.as_ref(), diff, remote, &router).await,
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
                route_intent(send.as_ref(), intent, &tick, &router).await;
            }
        }
    }
}

/// Execute one submitted intent and reply with its outcome.
///
/// **Stub.** Accepts the wire shape and commits optimistically without
/// validating the signature, without a `Ruleset` check, and without an FDB
/// transaction. The P2 deliverable is signature check → `Ruleset` validation
/// stub → FDB serializable optimistic transaction (docs/11-roadmap.md §P2);
/// witness attestation is P5.
async fn route_intent(
    send: &(dyn Fn(Bytes) + Send + Sync),
    intent: orrery_protocol::Intent,
    tick: &AtomicU64,
    router: &Arc<dyn Router>,
) {
    let _ = router;
    let t = Tick::new(tick.fetch_add(1, Ordering::Relaxed) + 1);
    let reply = GatewayReply::IntentAck {
        intent_id: intent.intent_id,
        outcome: IntentOutcome::Committed {
            tick: t,
            minted: Vec::new(),
        },
    };
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
            let reply = GatewayReply::BulkAck {
                entity,
                tick,
                lsn,
                provisional: false,
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
        let live = router.has_actor(cell).await;
        let read = if live {
            router.read(cell).await.map(Some)
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
