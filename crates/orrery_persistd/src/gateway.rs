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
//! single persistd node this is correct serialization; a multi-node deployment
//! would route by rendezvous placement instead (docs/08-persistence.md §3).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::sync::oneshot;
use tracing::debug;

use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
};
use orrery_protocol::{
    AreaPage, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOutcome,
    JournalRecord, Lsn, NodeId, PROTOCOL_VERSION, REASON_BAD_SIGNATURE, REASON_ISSUER_MISMATCH,
    REASON_NO_EXECUTOR,
};

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
        let (shutdown, rx) = oneshot::channel();
        let join = tokio::spawn(accept_loop(
            endpoint.clone(),
            router,
            gateway,
            protocol,
            executor,
            validator,
            rx,
        ));
        Ok(Self {
            endpoint,
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

    /// Stop the accept loop and close the endpoint, awaiting the accept task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.endpoint.close().await;
        let _ = self.join.await;
    }
}

/// Accept client connections forever, spawning one handler task per connection,
/// until `shutdown` resolves or the endpoint closes.
async fn accept_loop(
    endpoint: Arc<Endpoint>,
    router: Arc<dyn Router>,
    gateway: NodeId,
    protocol: u16,
    executor: Option<SharedExecutor>,
    validator: SharedValidator,
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
                tokio::spawn(handle_connection(
                    incoming, router, gateway, protocol, executor, validator,
                ));
            }
        }
    }
}

/// Drive one client session: complete the iroh handshake, send the admission
/// uni-stream, then read tagged datagrams and route each [`GatewayMsg`].
async fn handle_connection(
    incoming: iroh::endpoint::Incoming,
    router: Arc<dyn Router>,
    gateway: NodeId,
    protocol: u16,
    executor: Option<SharedExecutor>,
    validator: SharedValidator,
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

    let send = {
        let conn = Arc::clone(&conn);
        move |bytes: Bytes| {
            let _ = conn.send_datagram(bytes);
        }
    };

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
            GatewayMsg::Diff { diff } => route_diff(&send, diff, remote, &router).await,
            GatewayMsg::Subscribe { grid, cells } => {
                route_subscribe(&send, grid, cells, &router).await
            }
            GatewayMsg::SubmitIntent { intent } => {
                route_intent(&send, intent, remote, &executor, &validator).await
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
/// stream an [`AreaPage`] back. `live` reports whether a live actor held the
/// cell (vs a cold FDB scan). `grid` scopes the cold scans (P-7: storage cell
/// ids are grid-relative).
async fn route_subscribe(
    send: &(dyn Fn(Bytes) + Send + Sync),
    grid: GridId,
    cells: Vec<CellId>,
    router: &Arc<dyn Router>,
) {
    let mut frames = Vec::new();
    for cell in cells {
        // Live cells come from actor memory (authoritative, ≥ checkpoint
        // freshness); cold cells from the durable tier range scan
        // (docs/08-persistence.md §9).
        let live = router.has_actor(cell).await;
        let page = if live {
            router.read(cell).await.ok()
        } else {
            router.read_cold(grid, cell).await.ok().flatten()
        };
        if let Some(page) = page {
            let mut entities = Vec::with_capacity(page.entities.len());
            let mut payloads = Vec::with_capacity(page.entities.len());
            for (id, record) in page.entities {
                entities.push(id);
                payloads.push(record.components.clone());
            }
            frames.push(encode_stream_frame(&GatewayReply::AreaPage {
                cell,
                page: AreaPage {
                    cell,
                    entities,
                    payloads,
                    live,
                },
            }));
        }
    }

    for frame in frames {
        send(Bytes::from(frame));
    }
}
