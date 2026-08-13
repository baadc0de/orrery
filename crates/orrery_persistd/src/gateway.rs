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
use tokio::sync::oneshot;
use tracing::debug;

use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, untag, Channel,
};
use orrery_protocol::{
    AreaPage, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, IntentOutcome, JournalRecord,
    Lsn, NodeId, Tick, PROTOCOL_VERSION,
};

use crate::cluster::Router;
use crate::payload_crc;

/// The ALPN the gateway advertises and accepts. Matches the client's
/// `orrery_persist_client::gateway::GATEWAY_ALPN`.
pub const GATEWAY_ALPN: &[u8] = b"orrery/gateway/0";

/// The admission response byte, mirroring `aeronet_iroh`'s `ACCEPTED`.
const ACCEPTED: u8 = 0;

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
        let join = tokio::spawn(accept_loop(
            endpoint.clone(),
            router,
            gateway,
            protocol,
            tick,
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
    tick: Arc<AtomicU64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let router = Arc::clone(&router);
                let tick = Arc::clone(&tick);
                tokio::spawn(handle_connection(incoming, router, gateway, protocol, tick));
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
    tick: Arc<AtomicU64>,
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
            GatewayMsg::Subscribe { cells } => route_subscribe(&send, cells, &router).await,
            GatewayMsg::SubmitIntent { intent } => {
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
        }
    }
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
/// cell (vs a cold FDB scan, which this slice does not implement).
async fn route_subscribe(
    send: &(dyn Fn(Bytes) + Send + Sync),
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
            router.read_cold(cell).await.ok().flatten()
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
