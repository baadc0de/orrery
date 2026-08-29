//! The tokio half of the exterior wire: one real iroh connection per external
//! peer, pumped into the queue pairs the synchronous swarm already speaks.
//!
//! # Lane mapping
//!
//! The frame grammar in [`crate::exterior`] is transport-agnostic; this module
//! decides how each lane rides iroh, preserving the semantics the bots get
//! from aeronet's session machinery:
//!
//! All three lanes ride **one long-lived bidirectional stream** — the same one
//! the handshake ran on — as self-delimiting frames, with the lane byte
//! preserving which lane each frame belongs to.
//!
//! # Why the lossy lane rides a reliable wire here, and why that is honest
//!
//! Aeronet gives bots a lossy datagram lane, and making the exterior leg lossy
//! at the *socket* would seem to match it. It would also measure nothing: the
//! impairment the criterion samples is injected inside the host's virtual
//! router, **before** the wire — a packet the router drops never reaches any
//! socket, reliable or not, and the client observes exactly that drop as a gap
//! in its expected sequence. What the wire itself loses additionally would be
//! indistinguishable from router loss anyway; carrying frames reliably only
//! means the two loss sources cannot be confounded. The lane bytes still
//! travel, so upstream behaviour (what sheds, what repairs) is unchanged.
//!
//! # Handshake order
//!
//! The remote opens the first bidirectional stream and drives it: join
//! request, then — when the run witnesses — its anchor at the manifest tick. The host
//! answers with the accept between the two reads. Only after that does either
//! side start pumping frames, so "the slot exists" and "the slot carries
//! traffic" cannot be reordered.
//!
//! # Liveness
//!
//! Every pump ends by clearing one shared flag; queue senders check it. The
//! swarm's criterion reads that flag at report time, so a mid-run disconnect
//! fails the run rather than quietly banking an hour against a dead link.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use iroh::endpoint::{Connection, ConnectionError, QuicTransportConfig, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, RelayMode};
use orrery_core::CoreCodec;
use orrery_games::regolith::state::RegolithState;
use orrery_games::regolith::REGOLITH_RULESET;
use orrery_protocol::{NodeId, PersistId, StateClaim, Tick};

use crate::exterior::{
    encode_frame, AnchorFrame, Frame, HostLink, JoinReply, JoinRequest, Lane, RemoteLink,
    StartManifest, TransportCloseReason, LINK_QUEUE_DEPTH, MAX_FRAME_BYTES,
};

/// The connection's application protocol. A grammar change bumps this as well
/// as `JoinRequest::VERSION`; both sides must refuse what they do not speak.
pub const EXTERIOR_ALPN: &[u8] = b"orrery/exterior/4";

/// How long any single handshake read may take before the attempt is refused.
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection-wide QUIC inactivity allowed before a vanished exterior closes.
///
/// iroh's five-second keep-alive stays enabled, so a reachable idle player is
/// retained. Ten seconds spans two keep-alive intervals while bounding a
/// dead path's seat release to this timeout plus the host's two-second grace.
pub const EXTERIOR_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

fn exterior_transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            EXTERIOR_MAX_IDLE_TIMEOUT
                .try_into()
                .expect("ten seconds fits QUIC's idle-timeout varint"),
        ))
        .build()
}

/// Binds an endpoint for the exterior role, optionally at one exact socket.
///
/// Relays are disabled: the loopback proof needs no relay, and turning one on
/// is an operator decision about where cohort traffic may travel (#375), not a
/// harness default. `None` deliberately leaves iroh's wildcard, ephemeral-port
/// preset untouched for existing harness and test callers.
pub async fn bind(secret: iroh::SecretKey, bind_addr: Option<SocketAddr>) -> Result<Endpoint> {
    let mut builder = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![EXTERIOR_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(secret)
        .transport_config(exterior_transport_config());
    if let Some(bind_addr) = bind_addr {
        builder = builder
            .clear_ip_transports()
            .bind_addr(bind_addr)
            .context("configure exterior bind address")?;
    }
    let endpoint = builder.bind().await.context("bind exterior endpoint")?;
    Ok(endpoint)
}

/// What an operator or test needs to dial the host.
#[derive(Debug, Clone)]
pub struct HostAddress {
    /// Transport identity of the host.
    pub node: NodeId,
    /// Sockets the host is bound on; the first reachable one wins.
    pub direct: Vec<SocketAddr>,
}

impl HostAddress {
    /// The dial address, preferring a given socket when supplied and present.
    #[must_use]
    pub fn to_addr(&self, prefer: Option<SocketAddr>) -> EndpointAddr {
        let socket = prefer.or_else(|| self.direct.first().copied());
        // A wildcard bind address is a *bind* fact, not a destination: dialing
        // 0.0.0.0 connects (Linux routes it to loopback) and then iroh's path
        // handling blackholes everything after the handshake — the
        // joined-but-deaf slot #385 spent an afternoon on. The wildcard is
        // rewritten to its loopback form; real destinations pass through.
        let socket = socket.map(|socket| {
            let ip = match socket.ip() {
                std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                }
                std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                    std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                }
                other => other,
            };
            SocketAddr::new(ip, socket.port())
        });
        match socket {
            Some(socket) => EndpointAddr::from_parts(self.node, [iroh::TransportAddr::Ip(socket)]),
            None => EndpointAddr::from_parts(self.node, []),
        }
    }
}

fn mark_dead(connected: &Arc<AtomicBool>) {
    connected.store(false, Ordering::Relaxed);
}

/// Announces a freshly opened uni stream: the peer's `accept_uni` only sees a
/// stream once a frame arrives on it, and every pump starts by waiting for a
/// header - so an unannounced stream is a stream nobody will ever accept. A
/// zero-length Meta frame is the perfect beacon: parseable, routable, and
/// dropped by the receiver's meta routing as too short to be a cell report.
async fn announce(stream: &mut SendStream) -> Result<()> {
    let frame = Frame {
        peer: u32::MAX,
        lane: Lane::Meta,
        payload: Bytes::new(),
    };
    write_stream_frame(stream, &frame).await
}

/// Length-prefix read of one handshake message.
async fn read_message(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, recv.read_exact(&mut header))
        .await
        .context("handshake read timed out")?
        .context("handshake closed mid-length")?;
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_BYTES as usize {
        bail!("handshake message exceeds the frame bound");
    }
    let mut body = vec![0u8; len];
    tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, recv.read_exact(&mut body))
        .await
        .context("handshake read timed out")?
        .context("handshake closed mid-message")?;
    Ok(body)
}

async fn write_message(send: &mut SendStream, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len()).context("handshake message too long")?;
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(body).await?;
    Ok(())
}

/// One frame read off a reliable stream: `[lane u8][peer u32][len u32][payload]`.
///
/// `Ok(None)` when the stream ended cleanly at a frame boundary. An error ends
/// the connection, not just this frame: after a desync the boundaries are
/// unknowable, which is what the length bound alone cannot fix.
async fn read_stream_frame(recv: &mut RecvStream) -> Result<Option<Frame>> {
    // A short read here is a clean end or a dead link; either way this pump's
    // work is done and the flag says so.
    let mut header = [0u8; 9];
    let debug = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
    if debug {
        eprintln!("bridge[{}]: awaiting header", std::process::id());
    }
    if recv.read_exact(&mut header).await.is_err() {
        if debug {
            eprintln!("bridge[{}]: header read ended", std::process::id());
        }
        return Ok(None);
    }
    if debug {
        eprintln!(
            "bridge[{}]: header lane {} peer {} len {}",
            std::process::id(),
            header[0],
            u32::from_le_bytes(header[1..5].try_into().unwrap()),
            u32::from_le_bytes(header[5..9].try_into().unwrap()),
        );
    }
    let Some(lane) = Lane::from_tag(header[0]) else {
        bail!("unknown lane byte on the exterior stream");
    };
    let peer = u32::from_le_bytes(header[1..5].try_into().expect("nine bytes read"));
    let len = u32::from_le_bytes(header[5..9].try_into().expect("nine bytes read")) as usize;
    if len > MAX_FRAME_BYTES as usize {
        bail!("frame length exceeds the bound");
    }
    let mut payload = vec![0u8; len];
    if recv.read_exact(&mut payload).await.is_err() {
        return Ok(None);
    }
    Ok(Some(Frame {
        peer,
        lane,
        payload: Bytes::from(payload),
    }))
}

async fn write_stream_frame(send: &mut SendStream, frame: &Frame) -> Result<()> {
    let mut wire = Vec::with_capacity(9 + frame.payload.len());
    if encode_frame(frame, &mut wire).is_err() {
        bail!("frame exceeds the wire bound");
    }
    send.write_all(&wire).await?;
    // noq buffers written bytes per-stream; without an explicit flush the
    // first small frame sat unsent while every later one did too (#385).
    use tokio::io::AsyncWriteExt as _;
    send.flush().await?;
    Ok(())
}

/// Routes one inbound combat-lane frame: meta goes to its own channel so the
/// swarm can update rosters, everything else is traffic. A meta frame with a
/// body that is not one cell encoding is dropped, not guessed at.
async fn route_inbound(
    frame: Frame,
    uplink_tx: &tokio::sync::mpsc::Sender<Frame>,
    meta_tx: Option<&tokio::sync::mpsc::Sender<u64>>,
) {
    if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
        eprintln!(
            "bridge[{}]: routed lane {:?} peer {} payload {}",
            std::process::id(),
            frame.lane,
            frame.peer,
            frame.payload.len()
        );
    }
    match frame.lane {
        Lane::Meta => {
            if let (Some(meta_tx), Ok(raw)) = (meta_tx, <[u8; 8]>::try_from(frame.payload.as_ref()))
            {
                // A dropped future is an unsent frame: every send in the
                // pumps is awaited (#385's starvation lesson).
                let _ = meta_tx.send(u64::from_le_bytes(raw)).await;
            }
        }
        _ => {
            let _ = uplink_tx.send(frame).await;
        }
    }
}

/// A transport-authenticated join held until the lobby freezes membership.
pub struct PendingJoin {
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
    remote: NodeId,
    index: usize,
    session_id: Option<String>,
}

impl PendingJoin {
    /// Admission-authoritative seat requested by this client.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// QUIC-authenticated transport identity.
    #[must_use]
    pub const fn remote(&self) -> NodeId {
        self.remote
    }

    /// Admission session whose reservation authorized this connection.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Send a named lobby refusal before dropping the pending connection.
    pub async fn refuse(mut self, reason: String) -> Result<()> {
        write_message(
            &mut self.send,
            &JoinReply::Reject {
                reason: reason.clone(),
            }
            .encode(),
        )
        .await?;
        let _ = self.send.finish();
        tokio::time::sleep(Duration::from_millis(200)).await;
        bail!("join refused: {reason}")
    }

    /// Bind this accepted seat, send its personalized current `StartV1`,
    /// receive its join-tick anchor, and arm the data pumps.
    pub async fn finish(
        mut self,
        manifest: Option<StartManifest>,
        wants_anchor: bool,
    ) -> Result<(HostLink, Option<AnchorFrame>, NodeId, usize)> {
        let anchor_tick = manifest.as_ref().map_or(0, |start| start.tick);
        write_message(
            &mut self.send,
            &JoinReply::Accept {
                index: self.index,
                manifest,
            }
            .encode(),
        )
        .await?;

        let anchor = if wants_anchor {
            let claim = read_message(&mut self.recv).await?;
            let state = read_message(&mut self.recv).await?;
            if claim.is_empty() {
                if !state.is_empty() {
                    bail!("empty witness anchor claim carried a non-empty state");
                }
                None
            } else {
                let frame = AnchorFrame {
                    claim_json: claim,
                    state,
                };
                verify_join_anchor(&frame, self.remote, self.index, anchor_tick)?;
                Some(frame)
            }
        } else {
            None
        };

        drop(self.send);
        drop(self.recv);
        let mut downlink_send = self
            .connection
            .open_uni()
            .await
            .context("could not open downlink stream")?;
        announce(&mut downlink_send)
            .await
            .context("downlink announce failed")?;
        let uplink_recv = self
            .connection
            .accept_uni()
            .await
            .context("uplink stream never arrived")?;

        let connected = Arc::new(AtomicBool::new(true));
        let (uplink_tx, uplink_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
        let (downlink_tx, downlink_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
        let (meta_tx, meta_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
        let goodbye = Arc::new(AtomicBool::new(false));
        let transport_close = Arc::new(Mutex::new(None));
        watch_connection(
            "host",
            self.connection.clone(),
            Arc::clone(&connected),
            Some(Arc::clone(&transport_close)),
        );
        pump_ordered_reader_to(
            "host",
            Arc::clone(&goodbye),
            self.connection.clone(),
            uplink_recv,
            uplink_tx,
            Some(meta_tx),
        );
        pump_writer("host", self.connection, downlink_send, downlink_rx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok((
            HostLink {
                uplink: std::sync::Mutex::new(uplink_rx),
                downlink: downlink_tx,
                meta: std::sync::Mutex::new(meta_rx),
                connected,
                goodbye,
                transport_close,
            },
            anchor,
            self.remote,
            self.index,
        ))
    }
}

/// Accept and authenticate one join request, but do not send `Accept` until
/// the caller freezes the lobby through [`PendingJoin::finish`].
pub async fn host_prepare(
    endpoint: &Endpoint,
    expected: Option<(usize, NodeId)>,
    admission: &crate::exterior::Admission,
) -> Result<PendingJoin> {
    let incoming = endpoint
        .accept()
        .await
        .context("exterior endpoint closed while waiting for the join")?;
    let connection = incoming
        .accept()
        .context("join failed to start")?
        .await
        .context("join handshake failed")?;
    let remote = connection.remote_id();
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("no handshake stream arrived")?;
    let request_bytes = read_message(&mut recv).await?;
    let request =
        JoinRequest::decode(&request_bytes).map_err(|reason| anyhow::anyhow!("{reason}"))?;
    let index = request
        .slot
        .or_else(|| expected.map(|(slot, _)| slot))
        .ok_or_else(|| anyhow::anyhow!("no admission-granted slot was presented"))?;

    if let Some((expected_index, expected_node)) = expected {
        if index != expected_index {
            let reason = format!(
                "reservation_slot_mismatch: requested slot {index}, this host exposes slot {expected_index}"
            );
            write_message(
                &mut send,
                &JoinReply::Reject {
                    reason: reason.clone(),
                }
                .encode(),
            )
            .await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            bail!("join refused: {reason}");
        }
        if admission.issuer.is_none() && remote != expected_node {
            bail!(
                "a connection arrived from {remote}, but slot {index} belongs to {expected_node}"
            );
        }
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    if let Err(reason) = admission.judge_at_for_slot(&request, &remote, index, now_ms) {
        write_message(
            &mut send,
            &JoinReply::Reject {
                reason: reason.clone(),
            }
            .encode(),
        )
        .await?;
        let _ = send.finish();
        tokio::time::sleep(Duration::from_millis(200)).await;
        bail!("join refused: {reason}");
    }

    Ok(PendingJoin {
        connection,
        send,
        recv,
        remote,
        index,
        session_id: request.session_id,
    })
}

/// Host side: accepts the exterior peer's connection and runs the handshake.
///
/// Returns the live host queues plus the anchor the peer shipped (`None` on
/// runs without witnessing). Pumps are spawned here, so the caller gets a
/// working link or an error — never a half-wired slot.
#[allow(dead_code)]
pub async fn host_accept(
    endpoint: &Endpoint,
    expected: NodeId,
    index: usize,
    wants_anchor: bool,
    admission: &crate::exterior::Admission,
) -> Result<(HostLink, Option<AnchorFrame>, NodeId)> {
    let joined = host_prepare(endpoint, Some((index, expected)), admission).await?;
    let (link, anchor, remote, joined_index) = joined.finish(None, wants_anchor).await?;
    debug_assert_eq!(joined_index, index);
    Ok((link, anchor, remote))
}

/// Verify the signed commitment before the caller can seat the exterior slot.
fn verify_join_anchor(
    anchor: &AnchorFrame,
    subject: NodeId,
    index: usize,
    expected_tick: u64,
) -> Result<()> {
    let claim: StateClaim = serde_json::from_slice(&anchor.claim_json)
        .context("witness anchor claim did not decode")?;
    let state =
        RegolithState::decode(&anchor.state).context("witness anchor state did not decode")?;
    let expected_entity = PersistId::new(index as u64 + 1);
    if claim.entity != expected_entity {
        bail!(
            "witness anchor names entity {:?}, but slot {index} owns {:?}",
            claim.entity,
            expected_entity
        );
    }
    if claim.tick != Tick::new(expected_tick) || claim.chain_epoch != 0 {
        bail!("witness anchor is not the slot's tick-{expected_tick} epoch-zero claim");
    }
    if claim.ruleset != REGOLITH_RULESET {
        bail!("witness anchor names a different ruleset");
    }
    orrery_core::log::verify_claim(&claim, subject)
        .context("witness anchor signature does not verify for the joined node")?;
    if orrery_core::state_hash(&state) != claim.state_hash {
        bail!("witness anchor state does not match its signed join-tick hash");
    }
    Ok(())
}

/// Remote side: dials the host and runs the client half of the handshake.
///
/// Returns the mirror queues. The assigned slot comes back verified against
/// what the caller derived from the seed — a host assigning a different slot
/// is a misconfiguration to refuse, not something to adapt to.
pub async fn remote_join(
    endpoint: &Endpoint,
    address: EndpointAddr,
    request: &JoinRequest,
    index: usize,
    anchor: Option<AnchorFrame>,
) -> Result<RemoteLink> {
    let connection = endpoint
        .connect(address, EXTERIOR_ALPN)
        .await
        .context("dial exterior host")?;

    // The remote opens and drives the handshake stream.
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("open handshake stream")?;
    write_message(&mut send, &request.encode()).await?;
    let reply_bytes = read_message(&mut recv).await?;
    match JoinReply::decode(&reply_bytes).map_err(|reason| anyhow::anyhow!("{reason}"))? {
        JoinReply::Accept {
            index: assigned,
            manifest: _,
        } => {
            if assigned != index {
                bail!("the host assigned slot {assigned}; this peer derived {index}");
            }
        }
        JoinReply::Reject { reason } => bail!("the host refused the join: {reason}"),
    }
    // Two length-prefixed messages, mirroring the host's reads: claim first,
    // then the state it commits to. An absent log is the explicit empty pair,
    // preserving the unanchored compatibility path.
    let (claim, state) = anchor.as_ref().map_or((&[][..], &[][..]), |anchor| {
        (anchor.claim_json.as_slice(), anchor.state.as_slice())
    });
    let _ = write_message(&mut send, claim).await;
    let _ = write_message(&mut send, state).await;

    // Data path mirrors the host's: uplink on a uni stream this side opens
    // and announces, downlink on the uni stream the host opened and
    // announced. Both sides open before they accept, so neither accept can
    // wait on an open the other side has not made yet (#385).
    drop(send);
    drop(recv);
    let mut uplink_send = connection
        .open_uni()
        .await
        .context("could not open uplink stream")?;
    if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
        eprintln!(
            "bridge[{}]: opened uplink uni {:?}",
            std::process::id(),
            uplink_send.id()
        );
    }
    announce(&mut uplink_send)
        .await
        .context("uplink announce failed")?;
    let downlink_recv = connection
        .accept_uni()
        .await
        .context("downlink stream never arrived")?;

    let connected = Arc::new(AtomicBool::new(true));
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);

    // Host traffic and application-level uplink ACKs share this inbound
    // queue. ACKs stay Meta frames so the remote can settle router outcomes
    // without confusing them with replicated state.
    let goodbye = Arc::new(AtomicBool::new(false));
    watch_connection("remote", connection.clone(), Arc::clone(&connected), None);
    pump_ordered_reader_to(
        "remote",
        Arc::clone(&goodbye),
        connection.clone(),
        downlink_recv,
        inbound_tx,
        None,
    );
    pump_writer("remote", connection.clone(), uplink_send, outbound_rx);

    Ok(RemoteLink {
        downlink: std::sync::Mutex::new(inbound_rx),
        uplink: outbound_tx,
        connected,
        connection: Some(connection),
    })
}

fn classify_close(error: &ConnectionError) -> TransportCloseReason {
    match error {
        ConnectionError::ApplicationClosed(_) => TransportCloseReason::ApplicationClose,
        ConnectionError::Reset => TransportCloseReason::PeerReset,
        ConnectionError::TimedOut => TransportCloseReason::IdleTimeout,
        ConnectionError::ConnectionClosed(_) => TransportCloseReason::TransportClose,
        ConnectionError::LocallyClosed => TransportCloseReason::LocalClose,
        other => TransportCloseReason::Other(other.to_string()),
    }
}

/// The transport, rather than a stream read or application-frame timer, is the
/// sole broken-link signal. `closed()` also preserves the close classification
/// the seat-release log needs.
fn watch_connection(
    side: &'static str,
    connection: Connection,
    connected: Arc<AtomicBool>,
    close_reason: Option<Arc<Mutex<Option<TransportCloseReason>>>>,
) {
    tokio::spawn(async move {
        let error = connection.closed().await;
        let reason = classify_close(&error);
        if let Some(close_reason) = close_reason {
            *close_reason.lock().expect("transport-close lock") = Some(reason.clone());
        }
        if side == "host" {
            eprintln!("gates/p1-swarm: exterior QUIC closed ({reason})");
        } else if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
            eprintln!("bridge[{side}]: QUIC connection closed ({reason})");
        }
        mark_dead(&connected);
    });
}

/// Frame reader over the ordered stream: everything arriving is routed by
/// lane. A mid-frame end or a bad length ends the connection — after a desync
/// there are no frame boundaries left to find.
#[allow(clippy::too_many_arguments)]
fn pump_ordered_reader_to(
    side: &'static str,
    goodbye: Arc<AtomicBool>,
    connection: Connection,
    mut recv: RecvStream,
    uplink_tx: tokio::sync::mpsc::Sender<Frame>,
    meta_tx: Option<tokio::sync::mpsc::Sender<u64>>,
) {
    let debug = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
    let pid = std::process::id();
    tokio::spawn(async move {
        // The stream halves do NOT keep the connection alive: once the last
        // `Connection` handle drops, the transport closes and every later
        // write succeeds into the void while reads starve (#385's
        // joined-but-deaf slot). The pumps hold one for their whole life.
        let _keep_alive = connection;
        if debug {
            eprintln!(
                "bridge[{side}][{}]: reader armed on stream {:?}",
                pid,
                recv.id()
            );
        }
        while let Ok(Some(frame)) = read_stream_frame(&mut recv).await {
            // The runner's clean end-of-run marker: a meta frame whose whole
            // payload is one 0xFF byte. Nothing else may look like this.
            if frame.lane == Lane::Meta && frame.payload.as_ref() == [0xFFu8] {
                goodbye.store(true, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            if debug {
                eprintln!(
                    "bridge[{side}][{}]: got lane {:?} peer {}",
                    pid, frame.lane, frame.peer
                );
            }
            route_inbound(frame, &uplink_tx, meta_tx.as_ref()).await;
        }
    });
}

/// The writer every side runs: takes its outbound queue and writes each frame
/// onto the stream in whatever order the queue holds.
fn pump_writer(
    side: &'static str,
    connection: Connection,
    mut shared_send: SendStream,
    outbound_rx: tokio::sync::mpsc::Receiver<Frame>,
) {
    let debug = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
    let pid = std::process::id();
    tokio::spawn(async move {
        let _keep_alive = connection;
        let mut outbound_rx = outbound_rx;
        if debug {
            eprintln!(
                "bridge[{side}][{}]: writer armed on stream {:?}",
                pid,
                shared_send.id()
            );
        }
        while let Some(frame) = outbound_rx.recv().await {
            if debug {
                eprintln!(
                    "bridge[{side}][{}]: writing lane {:?} peer {}",
                    pid, frame.lane, frame.peer
                );
            }
            match write_stream_frame(&mut shared_send, &frame).await {
                Ok(()) => {
                    if debug {
                        eprintln!(
                            "bridge[{side}][{}]: wrote lane {:?} peer {}",
                            pid, frame.lane, frame.peer
                        );
                    }
                }
                Err(error) => {
                    if debug {
                        eprintln!("bridge[{}]: WRITE FAILED: {error}", pid);
                    }
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{bot_key, host_key};
    use crate::exterior::{Frame, JoinRequest, Lane};
    use orrery_games::{Game, Regolith};

    fn valid_anchor(slot: usize, key: &iroh_base::SecretKey) -> AnchorFrame {
        let entity = PersistId::new(slot as u64 + 1);
        let game = Regolith::honest();
        let state = game.spawn(entity, slot as u64);
        let mut producer =
            orrery_core::InputLogProducer::new(key.clone(), entity, REGOLITH_RULESET, 0, 30, 10);
        let claim = producer.anchor(0, &state);
        AnchorFrame {
            claim_json: serde_json::to_vec(&claim).expect("claim serializes"),
            state: state.to_canonical(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_configured_exterior_bind_uses_the_requested_address_and_port() {
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve UDP port");
        let requested = reservation.local_addr().expect("reserved address");
        drop(reservation);

        let endpoint = bind(
            iroh_base::SecretKey::from_bytes(&[0xA1; 32]),
            Some(requested),
        )
        .await
        .expect("configured endpoint binds");
        assert_eq!(
            endpoint.bound_sockets(),
            vec![requested],
            "the configured bind must replace iroh's preset sockets"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_exterior_bind_keeps_irohs_wildcard_ephemeral_default() {
        let first = bind(iroh_base::SecretKey::from_bytes(&[0xA2; 32]), None)
            .await
            .expect("first default endpoint");
        let second = bind(iroh_base::SecretKey::from_bytes(&[0xA3; 32]), None)
            .await
            .expect("second default endpoint");
        let first_v4 = first
            .bound_sockets()
            .into_iter()
            .find(SocketAddr::is_ipv4)
            .expect("iroh preset binds IPv4");
        let second_v4 = second
            .bound_sockets()
            .into_iter()
            .find(SocketAddr::is_ipv4)
            .expect("iroh preset binds IPv4");

        assert!(first_v4.ip().is_unspecified(), "default remains wildcard");
        assert_ne!(first_v4.port(), 0, "the OS assigns the ephemeral port");
        assert_ne!(
            first_v4.port(),
            second_v4.port(),
            "simultaneous default binds receive distinct ephemeral ports"
        );
    }

    /// The whole bridge over loopback iroh: real endpoints, the real
    /// handshake with an anchor, then frames pushed through both queue pairs.
    /// This is the seam #385's two-process proof rides on; if frames can lose
    /// here they can lose anywhere.
    ///
    #[tokio::test(flavor = "multi_thread")]
    async fn the_bridge_carries_frames_both_ways() {
        let slot = 2usize;
        let expected = bot_key(slot).public();

        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(bot_key(slot), None).await.expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];
        let address = HostAddress {
            node: host_ep.id(),
            direct: vec![socket],
        };

        // Accept on a task so the dial can proceed concurrently. The
        // endpoint handle comes back out: dropping it closes every
        // connection, which is how a joined slot can go silently deaf.
        let host_task = {
            let host_ep = host_ep.clone();
            tokio::spawn(async move {
                let link_and_anchor = host_accept(
                    &host_ep,
                    expected,
                    slot,
                    true,
                    &crate::exterior::Admission::open(),
                )
                .await;
                (host_ep, link_and_anchor)
            })
        };
        let anchor_frame = valid_anchor(slot, &bot_key(slot));
        let remote_ep_keep = remote_ep.clone();
        let remote_link = remote_join(
            &remote_ep,
            HostAddress {
                node: address.node,
                direct: vec![socket],
            }
            .to_addr(Some(socket)),
            &JoinRequest::plain("test".into()),
            slot,
            Some(anchor_frame),
        )
        .await
        .expect("remote join completes");
        let _keep_endpoint = remote_ep_keep;
        let (_host_ep_back, joined) = host_task.await.expect("host task");
        let (host_link, anchor, remote) = joined.expect("join ok");
        assert!(anchor.is_some(), "witnessing runs ship their anchor");
        assert_eq!(remote, expected);

        // Uplink: two combat frames and one meta report.
        for peer in [0u32, 1] {
            remote_link
                .uplink
                .send(Frame {
                    peer,
                    lane: Lane::Datagram,
                    payload: bytes::Bytes::from_static(b"state"),
                })
                .await
                .expect("outbound queue accepts");
        }
        remote_link
            .uplink
            .send(Frame {
                peer: u32::MAX,
                lane: Lane::Meta,
                payload: bytes::Bytes::from(7u64.to_le_bytes().to_vec()),
            })
            .await
            .expect("outbound queue accepts");

        // Downlink: the host queues a frame for the remote.
        host_link
            .downlink
            .send(Frame {
                peer: 0,
                lane: Lane::StreamShared,
                payload: bytes::Bytes::from_static(b"replica"),
            })
            .await
            .expect("downlink queue accepts");

        // Meta rides the reliable lane; poll it with sync try_recv so no
        // guard is held across an await.
        let mut found_meta = None;
        for _ in 0..100 {
            let attempt = {
                let mut r = host_link.meta.lock().expect("meta lock");
                r.try_recv()
            };
            if attempt == Ok(7u64) {
                found_meta = Some(7u64);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(found_meta, Some(7u64), "the cell report crossed");

        let mut up1 = None;
        for _ in 0..50 {
            let attempt = {
                let mut r = host_link.uplink.lock().expect("uplink lock");
                r.try_recv()
            };
            if let Ok(f) = attempt {
                up1 = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let up1 = up1.expect("first uplink frame routed");
        assert!(
            matches!(&up1, f if f.lane == Lane::Datagram && f.peer == 0),
            "first uplink frame routed: {up1:?}"
        );
    }

    /// The socket accept and `with_external` call are the production seating
    /// seam used by `main`; no fixture substitutes for either half.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_verified_join_anchor_seats_the_real_exterior_slot_anchored() {
        let slot = 1usize;
        let client_key = bot_key(slot);
        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(client_key.clone(), None)
            .await
            .expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];
        let host_task = {
            let host_ep = host_ep.clone();
            let expected = client_key.public();
            tokio::spawn(async move {
                host_accept(
                    &host_ep,
                    expected,
                    slot,
                    true,
                    &crate::exterior::Admission::open(),
                )
                .await
            })
        };
        let remote_link = remote_join(
            &remote_ep,
            HostAddress {
                node: host_ep.id(),
                direct: vec![socket],
            }
            .to_addr(Some(socket)),
            &JoinRequest::plain("test".into()),
            slot,
            Some(valid_anchor(slot, &client_key)),
        )
        .await
        .expect("client joins");
        let (host_link, anchor, node) = host_task
            .await
            .expect("accept task")
            .expect("host verifies anchor");
        let anchor = anchor.expect("verified anchor returned for seating");
        let claim = serde_json::from_slice(&anchor.claim_json).expect("claim decodes");
        let state = RegolithState::decode(&anchor.state).expect("state decodes");
        let swarm = crate::swarm::Swarm::new(crate::swarm::SwarmConfig {
            peers: slot,
            witnessing: true,
            ..crate::swarm::SwarmConfig::default()
        })
        .with_external(node, Some((claim, state)), host_link);
        assert_eq!(swarm.exterior_witness_anchored(), Some(true));
        drop(remote_link);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_accept_refuses_an_anchor_signed_by_a_different_key() {
        let slot = 1usize;
        let client_key = bot_key(slot);
        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(client_key.clone(), None)
            .await
            .expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];
        let host_task = {
            let host_ep = host_ep.clone();
            let expected = client_key.public();
            tokio::spawn(async move {
                host_accept(
                    &host_ep,
                    expected,
                    slot,
                    true,
                    &crate::exterior::Admission::open(),
                )
                .await
            })
        };
        let impostor = iroh_base::SecretKey::from_bytes(&[0x99; 32]);
        let _remote = remote_join(
            &remote_ep,
            HostAddress {
                node: host_ep.id(),
                direct: vec![socket],
            }
            .to_addr(Some(socket)),
            &JoinRequest::plain("test".into()),
            slot,
            Some(valid_anchor(slot, &impostor)),
        )
        .await;
        let error = host_task
            .await
            .expect("accept task")
            .expect_err("an invalid signature must not seat")
            .to_string();
        assert!(
            error.contains("witness anchor signature does not verify"),
            "named signature refusal, got: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_accept_refuses_state_that_does_not_match_the_tick_zero_claim() {
        let slot = 1usize;
        let client_key = bot_key(slot);
        let mut anchor = valid_anchor(slot, &client_key);
        let other = Regolith::honest().spawn(PersistId::new(slot as u64 + 1), 99);
        anchor.state = other.to_canonical();
        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(client_key.clone(), None)
            .await
            .expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];
        let host_task = {
            let host_ep = host_ep.clone();
            let expected = client_key.public();
            tokio::spawn(async move {
                host_accept(
                    &host_ep,
                    expected,
                    slot,
                    true,
                    &crate::exterior::Admission::open(),
                )
                .await
            })
        };
        let _remote = remote_join(
            &remote_ep,
            HostAddress {
                node: host_ep.id(),
                direct: vec![socket],
            }
            .to_addr(Some(socket)),
            &JoinRequest::plain("test".into()),
            slot,
            Some(anchor),
        )
        .await;
        let error = host_task
            .await
            .expect("accept task")
            .expect_err("mismatched join-tick state must not seat")
            .to_string();
        assert!(
            error.contains("state does not match its signed join-tick hash"),
            "named state-hash refusal, got: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_explicit_empty_anchor_still_seats_unanchored() {
        let slot = 1usize;
        let client_key = bot_key(slot);
        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(client_key.clone(), None)
            .await
            .expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];
        let host_task = {
            let host_ep = host_ep.clone();
            let expected = client_key.public();
            tokio::spawn(async move {
                host_accept(
                    &host_ep,
                    expected,
                    slot,
                    true,
                    &crate::exterior::Admission::open(),
                )
                .await
            })
        };
        let remote_link = remote_join(
            &remote_ep,
            HostAddress {
                node: host_ep.id(),
                direct: vec![socket],
            }
            .to_addr(Some(socket)),
            &JoinRequest::plain("test".into()),
            slot,
            None,
        )
        .await
        .expect("empty-anchor client joins");
        let (host_link, anchor, node) = host_task
            .await
            .expect("accept task")
            .expect("host retains the explicit compatibility path");
        assert!(anchor.is_none());
        let swarm = crate::swarm::Swarm::new(crate::swarm::SwarmConfig {
            peers: slot,
            witnessing: true,
            ..crate::swarm::SwarmConfig::default()
        })
        .with_external(node, None, host_link);
        assert_eq!(swarm.exterior_witness_anchored(), Some(false));
        drop(remote_link);
    }

    /// A campaign token authenticates the presented durable identity. The
    /// public slot-derived key is deliberately different, proving the accept
    /// path does not reintroduce it ahead of token verification.
    #[tokio::test(flavor = "multi_thread")]
    async fn token_admission_seats_the_presented_node_not_the_public_slot_key() {
        let slot = 3usize;
        let public_slot_key = bot_key(slot).public();
        let client_key = iroh_base::SecretKey::from_bytes(&[0x71; 32]);
        assert_ne!(client_key.public(), public_slot_key);
        let issuer = iroh_base::SecretKey::from_bytes(&[0x72; 32]);
        let key_id = orrery_protocol::IssuerKeyId::new(409);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as u64;
        let claims = orrery_protocol::SessionTokenClaimsV1::new(
            orrery_protocol::AccountId(7),
            client_key.public(),
            orrery_protocol::UnixMillis::new(now_ms),
            orrery_protocol::SessionTokenTtlMs(60_000),
            orrery_protocol::SessionStanding::Good,
            key_id,
            true,
        );
        let token = orrery_protocol::SessionTokenV1::sign(claims, &issuer)
            .expect("sign token")
            .encode()
            .expect("encode token");
        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(client_key.clone(), None)
            .await
            .expect("client endpoint");
        let socket = host_ep.bound_sockets()[0];
        let admission = crate::exterior::Admission {
            require_client_rev: None,
            require_session: None,
            issuer: Some(orrery_protocol::IssuerKey::new(key_id, issuer.public())),
            reservation_journal: None,
        };
        let host_task = {
            let host_ep = host_ep.clone();
            tokio::spawn(async move {
                host_accept(&host_ep, public_slot_key, slot, false, &admission).await
            })
        };
        let request = JoinRequest {
            client_rev: "test".to_owned(),
            session_id: None,
            token: Some(token),
            slot: Some(slot),
        };
        let _link = remote_join(
            &remote_ep,
            HostAddress {
                node: host_ep.id(),
                direct: vec![socket],
            }
            .to_addr(Some(socket)),
            &request,
            slot,
            None,
        )
        .await
        .expect("presented client joins");
        let (_, _, admitted_node) = host_task
            .await
            .expect("host task")
            .expect("host accepts token-bound identity");
        assert_eq!(admitted_node, client_key.public());
    }

    /// The admission verdict is wired to the accept, not merely computable:
    /// a host pinning a client rev sends `Reject` (the dialler reads the
    /// reason) and errors out instead of seating the slot (#345 §8).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pinned_host_rejects_a_stale_client_at_join() {
        let slot = 3usize;
        let expected = bot_key(slot).public();
        let host_ep = bind(host_key(), None).await.expect("host endpoint");
        let remote_ep = bind(bot_key(slot), None).await.expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];

        let host_task = {
            let host_ep = host_ep.clone();
            tokio::spawn(async move {
                host_accept(
                    &host_ep,
                    expected,
                    slot,
                    false,
                    &crate::exterior::Admission {
                        require_client_rev: Some("pinned-rev".to_owned()),
                        require_session: None,
                        issuer: None,
                        reservation_journal: None,
                    },
                )
                .await
            })
        };
        let address = HostAddress {
            node: host_ep.id(),
            direct: vec![socket],
        };
        let refused = remote_join(
            &remote_ep,
            address.to_addr(Some(socket)),
            &JoinRequest::plain("stale-rev".into()),
            slot,
            None,
        )
        .await;
        let reason = refused
            .expect_err("a stale client must not join")
            .to_string();
        assert!(
            reason.contains("download the current build"),
            "the dialler reads the remedy, got: {reason}"
        );
        let host_side = host_task.await.expect("host task");
        let host_error = host_side
            .expect_err("the host must not seat the slot")
            .to_string();
        assert!(
            host_error.contains("join refused"),
            "the host names the refusal, got: {host_error}"
        );
    }
}
