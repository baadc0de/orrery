//! The client half of the exterior wire (#386): the frame grammar, join
//! handshake, and iroh pumps that slice 1 (#385/#388) settled on the host
//! side of `p1-swarm`.
//!
//! # Why this is a mirror and not a dependency
//!
//! `p1-swarm` ships no library target, and this client is deliberately its
//! own `[workspace]`; so the client carries its own copy of the *client* half
//! of the grammar. Everything here is pinned against slice 1's settled bytes
//! by unit tests (`join_request_bytes_match_slice_1`,
//! `frame_layout_matches_the_harness_wire`,
//! `ack_decode_matches_the_p1_contract`, `slot_keys_match_the_harness`), which
//! is the drift alarm for both sides. If `p1-swarm/src/exterior.rs` ever
//! changes a byte, those tests must be updated in the same commit — that is
//! the point of pinning constants rather than re-deriving them.
//!
//! # The one rule carried over from slice 1
//!
//! **Every send inside an async task is awaited.** A tokio `send` whose future
//! is never polled drops the frame silently while socket writes keep returning
//! `Ok` — the starvation that cost #385 most of a day. The render loop cannot
//! await, so it hands frames over with [`CampaignLink::try_uplink`], whose
//! failure is a *counted* backpressure event at the call site, never a
//! silently dropped future; the writer task awaits everything past that
//! boundary, and the reader awaits every hand-off into the inbound queue.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};

/// The connection's application protocol. Must match `p1-swarm`'s
/// `bridge::EXTERIOR_ALPN`; a grammar change bumps it there and here together.
pub const EXTERIOR_ALPN: &[u8] = b"orrery/exterior/2";

/// Longest frame the wire will carry or accept (`exterior::MAX_FRAME_BYTES`).
pub const MAX_FRAME_BYTES: u32 = 64 * 1_024;

/// Bounded queue depth shared with the harness's links (`LINK_QUEUE_DEPTH`).
pub const LINK_QUEUE_DEPTH: usize = 4_096;

/// Which lane a frame rides (`exterior::Lane`). Tags are wire bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Session datagram lane: lossy, unordered.
    Datagram,
    /// Stream lane's shared stream: ordered, reliable.
    StreamShared,
    /// Stream lane's per-message bulk streams: reliable, unordered.
    StreamBulk,
    /// Out-of-band connection facts: uplink acks, cell reports, goodbye.
    Meta,
}

impl Lane {
    const fn tag(self) -> u8 {
        match self {
            Self::Datagram => 0,
            Self::StreamShared => 1,
            Self::StreamBulk => 2,
            Self::Meta => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Datagram),
            1 => Some(Self::StreamShared),
            2 => Some(Self::StreamBulk),
            3 => Some(Self::Meta),
            _ => None,
        }
    }
}

/// One addressed message on the exterior leg (`exterior::Frame`).
///
/// Uplink names the *recipient's* swarm slot; downlink names the *sender's*
/// (see `exterior`'s module docs for why the meaning flips).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Uplink: recipient slot. Downlink: sender slot. `u32::MAX` on meta.
    pub peer: u32,
    /// Which lane carries it.
    pub lane: Lane,
    /// The payload.
    pub payload: Bytes,
}

impl Frame {
    /// Appends the wire form `[lane u8][peer u32 LE][len u32 LE][payload]`.
    ///
    /// # Errors
    /// When the payload exceeds [`MAX_FRAME_BYTES`].
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        let len = u32::try_from(self.payload.len()).map_err(|_| FrameError)?;
        if len > MAX_FRAME_BYTES {
            return Err(FrameError);
        }
        out.put_u8(self.lane.tag());
        out.put_u32_le(self.peer);
        out.put_u32_le(len);
        out.put_slice(&self.payload);
        Ok(())
    }
}

/// Wire error: the byte stream can no longer be trusted to resync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameError;

/// One sequenced uplink datagram (`exterior::UplinkDatagram`).
///
/// The envelope is `[sequence u64 LE][datagram bytes]`; the sequence is what
/// the host's impaired-router decision is reported against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UplinkDatagram {
    /// Connection-local identifier, copied into the matching [`UplinkAck`].
    pub sequence: u64,
    /// The datagram bytes the addressed peer receives when impairment keeps it.
    pub payload: Bytes,
}

impl UplinkDatagram {
    /// Encodes the envelope for a Datagram-lane [`Frame`] payload.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(8 + self.payload.len());
        out.put_u64_le(self.sequence);
        out.put_slice(&self.payload);
        out.freeze()
    }
}

/// The impaired router's settled decision for one uplink datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkOutcome {
    /// The router retained the datagram (immediate or delayed delivery).
    Delivered,
    /// The router's impairment decision discarded the datagram.
    Dropped,
}

/// Application-level evidence for one sequenced uplink datagram
/// (`exterior::UplinkAck`, landed with #393).
///
/// This is the ONLY honest source of uplink loss. The datagram itself rides a
/// reliable QUIC stream, whose transport acks the write before the host's
/// router decides anything — a figure built on QUIC acks reports success for
/// exactly the frames impairment dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UplinkAck {
    /// The connection-local sequence copied from [`UplinkDatagram`].
    pub sequence: u64,
    /// What the impaired router decided, after making that decision.
    pub outcome: UplinkOutcome,
}

impl UplinkAck {
    /// Meta payload discriminator (`exterior::UplinkAck::TAG`).
    const TAG: u8 = 0xa1;

    /// Decodes only the ACK member of the Meta lane grammar.
    ///
    /// Cell reports (exactly eight bytes), announce frames (empty), and the
    /// goodbye marker (`[0xff]`) share the lane and decode to `None` here.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let [tag, outcome, sequence @ ..] = payload else {
            return None;
        };
        if *tag != Self::TAG || sequence.len() != 8 {
            return None;
        }
        let outcome = match outcome {
            0 => UplinkOutcome::Delivered,
            1 => UplinkOutcome::Dropped,
            _ => return None,
        };
        Some(Self {
            sequence: u64::from_le_bytes(sequence.try_into().expect("eight bytes checked")),
            outcome,
        })
    }
}

/// The handshake a dialling peer sends before any combat traffic
/// (`exterior::JoinRequest`, version 2). Invite-bound session identity and
/// version pinning are slice 3's extension (#345 §8); until then this is the
/// whole wire representation, which is why dialling needs the host NodeId
/// alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    /// Build revision of the joining process, for the report and for pinning.
    pub client_rev: String,
}

impl JoinRequest {
    const MAGIC: [u8; 4] = *b"ORRX";
    const VERSION: u16 = 2;

    /// Encodes the request onto the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::MAGIC.len() + 2 + 1 + self.client_rev.len());
        out.extend_from_slice(&Self::MAGIC);
        out.extend_from_slice(&Self::VERSION.to_le_bytes());
        let rev = self.client_rev.as_bytes();
        out.push(u8::try_from(rev.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(&rev[..rev.len().min(u8::MAX.into())]);
        out
    }

    /// Decodes a request. Wrong magic or version is a refusal, not a guess.
    ///
    /// # Errors
    /// Wrong magic, unsupported version, or any truncation.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < Self::MAGIC.len() || bytes[..Self::MAGIC.len()] != Self::MAGIC {
            return Err("not an orrery exterior join");
        }
        let rest = &bytes[Self::MAGIC.len()..];
        if rest.len() < 2 {
            return Err("join truncated before version");
        }
        let version = u16::from_le_bytes(rest[0..2].try_into().expect("two bytes read"));
        if version != Self::VERSION {
            return Err("unsupported exterior protocol version");
        }
        let rest = &rest[2..];
        if rest.is_empty() {
            return Err("join truncated before revision");
        }
        let rev_len = rest[0] as usize;
        let rest = &rest[1..];
        if rest.len() < rev_len {
            return Err("join truncated inside revision");
        }
        Ok(Self {
            client_rev: String::from_utf8_lossy(&rest[..rev_len]).into_owned(),
        })
    }
}

/// The host's answer to a [`JoinRequest`] (`exterior::JoinReply`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinReply {
    /// The sender joins at this swarm slot; its entity derives from the slot
    /// exactly as a bot's does.
    Accept {
        /// The swarm slot assigned to this peer.
        index: usize,
    },
    /// The host refuses the join; the reason names itself.
    Reject {
        /// Why the join was refused.
        reason: String,
    },
}

impl JoinReply {
    /// Decodes a reply.
    ///
    /// # Errors
    /// Unknown tag or any truncation.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        match bytes.first() {
            Some(0) => {
                let Some(rest) = bytes.get(1..9) else {
                    return Err("accept truncated");
                };
                Ok(Self::Accept {
                    index: usize::try_from(u64::from_le_bytes(
                        rest.try_into().expect("eight bytes"),
                    ))
                    .map_err(|_| "accept index overflow")?,
                })
            }
            Some(1) => {
                let Some(&len) = bytes.get(1) else {
                    return Err("reject truncated");
                };
                let Some(reason) = bytes.get(2..2 + usize::from(len)) else {
                    return Err("reject reason truncated");
                };
                Ok(Self::Reject {
                    reason: String::from_utf8_lossy(reason).into_owned(),
                })
            }
            _ => Err("unknown join reply"),
        }
    }
}

/// What an operator or test needs to dial the host (`bridge::HostAddress`).
#[derive(Debug, Clone)]
pub struct HostAddress {
    /// Transport identity of the host.
    pub node: orrery_protocol::NodeId,
    /// Sockets the host is bound on; the first reachable one wins.
    pub direct: Vec<SocketAddr>,
}

impl HostAddress {
    /// Parses `<node id hex>` plus an optional `<ip:port>` — the two fields
    /// the harness's listening file writes.
    ///
    /// # Errors
    /// When the node id is not hex or the socket does not parse.
    pub fn parse(node_hex: &str, direct: Option<&str>) -> Result<Self, String> {
        let node = orrery_protocol::NodeId::from_str(node_hex)
            .map_err(|error| format!("host node id is not hex: {error}"))?;
        let direct_sockets = match direct {
            Some(socket) => vec![socket
                .parse::<SocketAddr>()
                .map_err(|error| format!("host direct address is not ip:port: {error}"))?],
            None => Vec::new(),
        };
        Ok(Self {
            node,
            direct: direct_sockets,
        })
    }

    /// The dial address, preferring a given socket when supplied and present.
    ///
    /// Carries #385's wildcard rewrite: a bind fact is not a destination, and
    /// dialing `0.0.0.0` blackholes every post-handshake path.
    #[must_use]
    pub fn to_addr(&self, prefer: Option<SocketAddr>) -> iroh::EndpointAddr {
        let socket = prefer.or_else(|| self.direct.first().copied());
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
            Some(socket) => {
                iroh::EndpointAddr::from_parts(self.node, [iroh::TransportAddr::Ip(socket)])
            }
            None => iroh::EndpointAddr::from_parts(self.node, []),
        }
    }
}

/// The transport secret for campaign slot `index`.
///
/// Same derivation as the harness's `bot_key`: the slot's identity is a
/// function of the slot number alone, and the host refuses a dialler whose
/// transport id does not match what it derived for the slot. Pinned against
/// the harness's actual keys by `slot_keys_match_the_harness`.
#[must_use]
pub fn slot_secret(index: usize) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    seed[31] = 0xB0;
    iroh_base::SecretKey::from_bytes(&seed)
}

/// Binds an endpoint for the exterior role (`bridge::bind`).
///
/// Relays stay disabled: where cohort traffic may travel is an operator
/// decision about exposure (#375), not a client default.
///
/// # Errors
/// A rendered string when iroh cannot bind a socket.
pub async fn bind(secret: iroh_base::SecretKey) -> Result<iroh::Endpoint, String> {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![EXTERIOR_ALPN.to_vec()])
        .relay_mode(iroh::RelayMode::Disabled)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|error| format!("bind exterior endpoint: {error}"))?;
    Ok(endpoint)
}

/// The live link after a successful join: queues plus liveness, mirroring
/// `exterior::RemoteLink` but with the synchronous entry points the render
/// loop needs.
///
/// Meta frames (uplink acks among them) arrive on the same inbound queue as
/// traffic, exactly as they do for the runner; the campaign layer decodes
/// them, so this type stays a pure transport handle.
pub struct CampaignLink {
    downlink: Arc<Mutex<tokio::sync::mpsc::Receiver<Frame>>>,
    uplink: tokio::sync::mpsc::Sender<Frame>,
    connected: Arc<AtomicBool>,
    closed_by_host: Arc<AtomicBool>,
}

impl CampaignLink {
    /// Hand one outbound frame toward the writer task without awaiting.
    ///
    /// `Err` means backpressure or a dead link; callers count it as a shed
    /// packet. This is deliberately NOT an unawaited async send — the failure
    /// mode slice 1 spent a day on — because the result is observed here, at
    /// the call site, every time.
    ///
    /// # Errors
    /// Queue full (backpressure, counted by the caller) or queue closed.
    pub fn try_uplink(
        &self,
        frame: Frame,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Frame>> {
        self.uplink.try_send(frame)
    }

    /// Drains every inbound frame waiting now, without blocking the loop.
    #[must_use]
    pub fn drain_downlink(&self) -> Vec<Frame> {
        let mut received = Vec::new();
        // Never held across an await: try_recv is synchronous.
        if let Ok(mut receiver) = self.downlink.lock() {
            while let Ok(frame) = receiver.try_recv() {
                received.push(frame);
            }
        }
        received
    }

    /// Whether the pumps still believe in the connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether the host sent the goodbye marker (a clean end from its side).
    #[must_use]
    pub fn host_said_goodbye(&self) -> bool {
        self.closed_by_host.load(Ordering::Relaxed)
    }

    /// Sends the runner's clean end-of-run marker and gives the wire a grace
    /// period to carry it. Called once, on session end.
    pub fn close(&self) {
        let goodbye = Frame {
            peer: u32::MAX,
            lane: Lane::Meta,
            payload: Bytes::from([0xFFu8].to_vec()),
        };
        let _ = self.try_uplink(goodbye);
        // The writer pump flushes each frame as it writes; this mirrors the
        // runner's grace period before dropping its runtime.
        std::thread::sleep(Duration::from_millis(200));
        self.connected.store(false, Ordering::Relaxed);
    }
}

/// Dials the host and runs the client half of slice 1's handshake.
///
/// No anchor is shipped: witnessing authoring stays out of #386's scope, so
/// this joins hosts run without their witnessed clause. The assigned slot is
/// verified against `expected_slot` — a host assigning a different slot is a
/// misconfiguration to refuse, exactly as the runner refuses one.
///
/// The returned link's pumps run on the caller's tokio runtime, which must
/// outlive the link.
///
/// # Errors
/// A rendered string for every refusal: dial failure, handshake timeout or
/// truncation, reject reply, slot mismatch, stream setup failure.
pub async fn remote_join(
    endpoint: &iroh::Endpoint,
    address: iroh::EndpointAddr,
    request: &JoinRequest,
    expected_slot: usize,
) -> Result<CampaignLink, String> {
    const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(10);
    let debug = std::env::var_os("REGOLITH_NET_DEBUG").is_some();
    let step = |stage: &str| {
        if debug {
            eprintln!("net[{}]: {stage}", std::process::id());
        }
    };

    step("dialing");
    let connection = endpoint
        .connect(address, EXTERIOR_ALPN)
        .await
        .map_err(|error| format!("dial exterior host: {error}"))?;
    step("transport established");

    // The remote opens and drives the handshake stream.
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| format!("open handshake stream: {error}"))?;
    step("handshake stream open");
    write_message(&mut send, &request.encode())
        .await
        .map_err(|error| format!("send join request: {error}"))?;
    step("join request written");
    let reply_bytes = tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, read_message(&mut recv))
        .await
        .map_err(|_| "handshake read timed out".to_string())?
        .map_err(|error| format!("read join reply: {error}"))?;
    step("join reply read");
    match JoinReply::decode(&reply_bytes) {
        Ok(JoinReply::Accept { index }) => {
            if index != expected_slot {
                return Err(format!(
                    "the host assigned slot {index}; this client was launched for slot \
                     {expected_slot}"
                ));
            }
        }
        Ok(JoinReply::Reject { reason }) => {
            return Err(format!("the host refused the join: {reason}"));
        }
        Err(reason) => return Err(format!("the host's reply did not decode: {reason}")),
    }

    // Data path mirrors the host's (#385): uplink on a uni stream this side
    // opens and announces, downlink on the uni stream the host opened and
    // announced. Both sides open before they accept, so neither accept can
    // wait on an open the other side has not made yet.
    drop(send);
    drop(recv);
    let mut uplink_send = connection
        .open_uni()
        .await
        .map_err(|error| format!("could not open uplink stream: {error}"))?;
    announce(&mut uplink_send)
        .await
        .map_err(|error| format!("uplink announce failed: {error}"))?;
    let downlink_recv = connection
        .accept_uni()
        .await
        .map_err(|error| format!("downlink stream never arrived: {error}"))?;

    let connected = Arc::new(AtomicBool::new(true));
    let closed_by_host = Arc::new(AtomicBool::new(false));
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);

    spawn_reader(
        connection.clone(),
        Arc::clone(&connected),
        Arc::clone(&closed_by_host),
        downlink_recv,
        inbound_tx,
    );
    spawn_writer(connection, Arc::clone(&connected), uplink_send, outbound_rx);

    // A freshly spawned task has not necessarily reached its first await; let
    // every pump park inside its socket read before traffic is produced
    // (#385's flaky-first-frame lesson).
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(CampaignLink {
        downlink: Arc::new(Mutex::new(inbound_rx)),
        uplink: outbound_tx,
        connected,
        closed_by_host,
    })
}

async fn announce(stream: &mut iroh::endpoint::SendStream) -> Result<(), String> {
    // A freshly opened uni stream is invisible until something arrives on it;
    // a zero-length Meta frame is the beacon slice 1 chose: parseable,
    // routable, dropped by the receiver's meta routing as too short to matter.
    let frame = Frame {
        peer: u32::MAX,
        lane: Lane::Meta,
        payload: Bytes::new(),
    };
    write_stream_frame(stream, &frame).await
}

async fn read_message(recv: &mut iroh::endpoint::RecvStream) -> Result<Vec<u8>, String> {
    let mut header = [0u8; 4];
    recv.read_exact(&mut header)
        .await
        .map_err(|_| "handshake closed mid-length".to_string())?;
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_BYTES as usize {
        return Err("handshake message exceeds the frame bound".to_string());
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|_| "handshake closed mid-message".to_string())?;
    Ok(body)
}

async fn write_message(send: &mut iroh::endpoint::SendStream, body: &[u8]) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;
    let len = u32::try_from(body.len()).map_err(|_| "handshake message too long".to_string())?;
    send.write_all(&len.to_le_bytes())
        .await
        .map_err(|error| format!("handshake write failed: {error}"))?;
    send.write_all(body)
        .await
        .map_err(|error| format!("handshake write failed: {error}"))?;
    // quinn buffers written bytes per-stream: without an explicit flush the
    // first small frame sat unsent while every later one did too (#385).
    send.flush().await.map_err(|error| format!("{error}"))
}

/// One frame read off a reliable stream: `[lane u8][peer u32][len u32][payload]`.
///
/// `Ok(None)` when the stream ended cleanly at a frame boundary; an error ends
/// the connection, because after a desync there are no boundaries left to find.
async fn read_stream_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<Option<Frame>, String> {
    let mut header = [0u8; 9];
    if recv.read_exact(&mut header).await.is_err() {
        return Ok(None);
    }
    let Some(lane) = Lane::from_tag(header[0]) else {
        return Err("unknown lane byte on the exterior stream".to_string());
    };
    let peer = u32::from_le_bytes(header[1..5].try_into().expect("nine bytes read"));
    let len = u32::from_le_bytes(header[5..9].try_into().expect("nine bytes read")) as usize;
    if len > MAX_FRAME_BYTES as usize {
        return Err("frame length exceeds the bound".to_string());
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

async fn write_stream_frame(
    send: &mut iroh::endpoint::SendStream,
    frame: &Frame,
) -> Result<(), String> {
    let mut wire = Vec::with_capacity(9 + frame.payload.len());
    frame
        .encode(&mut wire)
        .map_err(|_| "frame exceeds the wire bound".to_string())?;
    send.write_all(&wire)
        .await
        .map_err(|error| format!("{error}"))?;
    // quinn buffers written bytes per-stream; without an explicit flush the
    // first small frame sat unsent while every later one did too (#385).
    use tokio::io::AsyncWriteExt as _;
    send.flush().await.map_err(|error| format!("{error}"))
}

fn spawn_reader(
    connection: iroh::endpoint::Connection,
    connected: Arc<AtomicBool>,
    closed_by_host: Arc<AtomicBool>,
    mut recv: iroh::endpoint::RecvStream,
    inbound_tx: tokio::sync::mpsc::Sender<Frame>,
) {
    tokio::spawn(async move {
        // The stream halves do NOT keep the connection alive: once the last
        // `Connection` handle drops, the transport closes and every later
        // write succeeds into the void while reads starve (#385).
        let _keep_alive = connection;
        while let Ok(Some(frame)) = read_stream_frame(&mut recv).await {
            // The runner's clean end-of-run marker, seen from the far side:
            // one meta frame whose whole payload is a single 0xFF byte.
            if frame.lane == Lane::Meta && frame.payload.as_ref() == [0xFFu8] {
                closed_by_host.store(true, Ordering::Relaxed);
                continue;
            }
            // Awaited: a dropped future here would swallow replication state
            // or the very ack evidence the campaign banking needs. Backpressure
            // pauses the socket read; it never silently discards.
            let _ = inbound_tx.send(frame).await;
        }
        connected.store(false, Ordering::Relaxed);
    });
}

fn spawn_writer(
    connection: iroh::endpoint::Connection,
    connected: Arc<AtomicBool>,
    mut shared_send: iroh::endpoint::SendStream,
    mut outbound_rx: tokio::sync::mpsc::Receiver<Frame>,
) {
    tokio::spawn(async move {
        let _keep_alive = connection;
        while let Some(frame) = outbound_rx.recv().await {
            if write_stream_frame(&mut shared_send, &frame).await.is_err() {
                break;
            }
        }
        connected.store(false, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes slice 1's host reads. `exterior::JoinRequest` v2 is
    /// `[ORRX][02 00][rev len][rev]`; if this vector and `p1-swarm` ever
    /// disagree, one side changed the grammar and both must move together.
    #[test]
    fn join_request_bytes_match_slice_1() {
        let request = JoinRequest {
            client_rev: "abc1234".to_owned(),
        };
        assert_eq!(
            request.encode(),
            vec![
                b'O', b'R', b'R', b'X', // magic
                0x02, 0x00, // version 2, LE
                7,    // revision length
                b'a', b'b', b'c', b'1', b'2', b'3', b'4',
            ]
        );
        assert_eq!(JoinRequest::decode(&request.encode()), Ok(request));
        // And the refusal arms: wrong magic, wrong version, truncation.
        assert_eq!(
            JoinRequest::decode(b"NOPE\x01\x00"),
            Err("not an orrery exterior join")
        );
        let mut wrong_version = JoinRequest {
            client_rev: "x".into(),
        }
        .encode();
        wrong_version[4] = 0xFF;
        assert_eq!(
            JoinRequest::decode(&wrong_version),
            Err("unsupported exterior protocol version")
        );
        assert_eq!(
            // Magic + version only: the length byte has not arrived.
            JoinRequest::decode(
                &JoinRequest {
                    client_rev: "x".into()
                }
                .encode()[..6]
            ),
            Err("join truncated before revision")
        );
        assert_eq!(
            // Length byte present, payload cut short.
            JoinRequest::decode(
                &JoinRequest {
                    client_rev: "x".into()
                }
                .encode()[..7]
            ),
            Err("join truncated inside revision")
        );
    }

    /// Frame layout `[lane u8][peer u32 LE][len u32 LE][payload]`, the lane
    /// tags, and the bound.
    #[test]
    fn frame_layout_matches_the_harness_wire() {
        let frame = Frame {
            peer: 3,
            lane: Lane::Datagram,
            payload: Bytes::from_static(b"hello world"),
        };
        let mut wire = Vec::new();
        frame.encode(&mut wire).expect("in range");
        assert_eq!(
            wire[..9],
            [0u8, 3, 0, 0, 0, 11, 0, 0, 0],
            "datagram tag 0, recipient slot, payload length"
        );

        for (lane, tag) in [
            (Lane::Datagram, 0u8),
            (Lane::StreamShared, 1),
            (Lane::StreamBulk, 2),
            (Lane::Meta, 3),
        ] {
            let frame = Frame {
                peer: u32::MAX,
                lane,
                payload: Bytes::new(),
            };
            let mut wire = Vec::new();
            frame.encode(&mut wire).expect("in range");
            assert_eq!(wire[0], tag);
        }

        // The announce beacon IS this zero-payload meta frame with peer MAX.
        let announce = Frame {
            peer: u32::MAX,
            lane: Lane::Meta,
            payload: Bytes::new(),
        };
        let mut wire = Vec::new();
        announce.encode(&mut wire).expect("in range");
        assert_eq!(wire, vec![3u8, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);

        let big = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        let oversized = Frame {
            peer: 0,
            lane: Lane::Datagram,
            payload: Bytes::from(big),
        };
        assert_eq!(oversized.encode(&mut Vec::new()), Err(FrameError));
    }

    /// The #393 ack contract: `[0xa1][outcome][sequence u64 LE]`, outcomes
    /// Delivered=0 / Dropped=1, and nothing else on the meta lane decodes.
    #[test]
    fn ack_decode_matches_the_p1_contract() {
        for (outcome, byte) in [(UplinkOutcome::Delivered, 0u8), (UplinkOutcome::Dropped, 1)] {
            let mut payload = vec![0xa1, byte];
            payload.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
            let decoded = UplinkAck::decode(&payload).expect("a well-formed ack decodes");
            assert_eq!(decoded.sequence, 0x0123_4567_89ab_cdef);
            assert_eq!(decoded.outcome, outcome);
        }
        assert_eq!(
            UplinkAck::decode(&[0xa1, 2, 0, 0, 0, 0, 0, 0, 0, 0]),
            None,
            "unknown outcome"
        );
        assert_eq!(UplinkAck::decode(&[0xa1, 0, 1, 2]), None, "short sequence");
        assert_eq!(
            UplinkAck::decode(&[0xa2, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            None,
            "wrong tag"
        );
        assert_eq!(
            UplinkAck::decode(&7u64.to_le_bytes()),
            None,
            "cell report, not ack"
        );
        assert_eq!(UplinkAck::decode(&[]), None, "empty");
    }

    /// The uplink envelope is `[sequence u64 LE][payload]`.
    #[test]
    fn uplink_envelope_matches_the_harness() {
        let datagram = UplinkDatagram {
            sequence: 42,
            payload: Bytes::from_static(b"state-bytes"),
        };
        let encoded = datagram.encode();
        assert_eq!(&encoded[..8], &42u64.to_le_bytes());
        assert_eq!(&encoded[8..], b"state-bytes");
        assert_eq!(encoded.len(), 8 + 11);
    }

    /// The join reply decoder accepts both arms and refuses junk, matching
    /// what slice 1's host writes back.
    #[test]
    fn join_reply_decode_matches_slice_1() {
        let accept = JoinReply::Accept { index: 5 };
        let mut bytes = vec![0];
        bytes.extend_from_slice(&5u64.to_le_bytes());
        assert_eq!(JoinReply::decode(&bytes), Ok(accept));

        // reason length prefix then reason
        let reject_bytes = vec![1u8, 4, b'f', b'u', b'l', b'l'];
        assert_eq!(
            JoinReply::decode(&reject_bytes),
            Ok(JoinReply::Reject {
                reason: "full".to_owned()
            })
        );
        assert_eq!(JoinReply::decode(&[7]), Err("unknown join reply"));
        assert_eq!(JoinReply::decode(&[0, 1, 2]), Err("accept truncated"));
    }

    /// The client's slot keys are byte-for-byte the harness's `bot_key`
    /// derivation. These hex constants were produced by running that
    /// derivation (`p1-swarm/src/bot.rs`) under iroh-base 1.0.3; if either
    /// side changes its recipe, the host will refuse every dial with "slot N
    /// belongs to <other id>".
    #[test]
    fn slot_keys_match_the_harness() {
        for (slot, expected_hex) in [
            (
                0usize,
                "61a71521afb8e193d0d0fc248f85ed20bc78efa1120c83334579129b4171405b",
            ),
            (
                1,
                "23bf987bfc014fa8e7582d6b932ce5896d7e19a7b0e138a8eee72f3859b957d6",
            ),
            (
                2,
                "5996bc08944895b112a438d6efad8e65bc1e52f133ce6f4bc9b6b4a29d43bfee",
            ),
            (
                3,
                "86de0994a6d671b0b082603327e5442ffd4c6f829ddc8dccec843ea8e4d01dbc",
            ),
            (
                4,
                "fbe2845c3aadc8107c3c59116a736d768be329fe8d6b82040b818fc624b00293",
            ),
        ] {
            let public = slot_secret(slot).public();
            assert_eq!(format!("{public}"), expected_hex);
        }
    }

    /// Address parsing, including the wildcard rewrite that keeps a bind fact
    /// from becoming a blackhole destination (#385).
    #[test]
    fn host_address_parses_and_rewrites_wildcards() {
        let node_hex = "61a71521afb8e193d0d0fc248f85ed20bc78efa1120c83334579129b4171405b";
        let address = HostAddress::parse(node_hex, Some("0.0.0.0:4001")).expect("parses");
        let dial = address.to_addr(None);
        assert_eq!(dial.id, address.node);
        // The wildcard socket was rewritten to loopback before dialing.
        let socket = dial.addrs.first().expect("a direct address was supplied");
        match socket {
            iroh::TransportAddr::Ip(socket) => {
                assert_eq!(socket.to_string(), "127.0.0.1:4001");
            }
            other => panic!("expected a direct ip transport, got {other:?}"),
        }

        assert!(HostAddress::parse("nothex", None).is_err());
        assert!(HostAddress::parse(node_hex, Some("not-a-socket")).is_err());
    }
}
