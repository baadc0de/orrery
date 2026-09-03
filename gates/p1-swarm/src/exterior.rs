//! The external-peer wire: frame grammar over one real iroh connection, and
//! the join handshake that precedes any traffic.
//!
//! # Why a bridge and not a socket swap
//!
//! #385's decision (#375, owner record of 2026-08-24) puts the human client on
//! the network as an ordinary island member. This module is only its *wire*;
//! where the impairment applies is the part worth stating up front.
//!
//! The swarm couples peers through an in-process [`Router`](crate::router::Router)
//! that models seeded loss and jitter. An external peer does not bypass it: the
//! host bridges the remote connection *into* the router, so every packet to or
//! from the human path is impaired exactly like a bot's. A direct socket would
//! be faster and would measure nothing comparable — the criterion's hours are
//! hours under injected impairment, and an unimpaired leg is not one of them.
//!
//! # What travels
//!
//! Peers exchange three lane kinds here, and the frames mirror the in-process
//! triples 1:1 so the host-side bridge can move
//! `(NodeId, Option<StreamMode>, Bytes)` without reinterpreting them:
//!
//! - the **datagram** lane (`aeronet_io::Session` packets — lossy, unordered),
//! - the **stream** lane (`aeronet_iroh::stream` messages, `Shared` or `Bulk`),
//! - the **meta** lane, which exists only on this leg: one connection stands in
//!   for a whole island membership, so connection facts travel beside combat
//!   traffic. The remote sends its interest cell once per simulated second;
//!   the host returns settled uplink-datagram acknowledgments and hearsay
//!   contact folds.
//!
//! Every frame names a **swarm index** — but which end of the hop it names
//! depends on the direction, because each side routes by the only index it
//! cannot infer:
//!
//! - **uplink** (external → host): the *recipient's* slot, since one
//!   connection carries traffic for many island-mates;
//! - **downlink** (host → external): the *sender's* slot, since the recipient
//!   is always the remote peer itself but must know which linked session to
//!   hand the bytes to.
//!
//! Frame grammar, little-endian:
//! `[lane u8][peer u32][len u32][payload]`. Lengths beyond
//! [`MAX_FRAME_BYTES`] are refused rather than read — a length field is only
//! trustworthy until the first desync, and a bounded refusal turns a desync
//! into a disconnect instead of an allocation.
//!
//! An uplink datagram's payload has one more application envelope:
//! `[sequence u64][datagram bytes]`. After the host's impaired router decides
//! its fate, the host returns one [`UplinkAck`] on the Meta lane. This is
//! deliberately above QUIC: the reliable bridge accepting the write is not
//! evidence that the later impairment decision kept the logical datagram.

// The host-side bridge and the external-peer mode that consume this wire land
// in the following commits of #385; until they exist only the tests read it.
#![allow(dead_code)]

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// `std::sync::mpsc`, named so call sites read as deliberately std rather than
/// accidentally unqualified.
/// Longest frame the wire will carry or accept.
///
/// Bots link at MTU 1_200 (`Bot::link`), but witness log frames and repair
/// control travel whole on the stream lane, so the cap is generous rather than
/// MTU-bound. It exists to bound a hostile or desynced length field, not to
/// police the senders, who never approach it.
pub const MAX_FRAME_BYTES: u32 = 64 * 1_024;

/// Why QUIC declared an exterior connection closed.
///
/// The host retains this classification through the two-second release grace
/// so the unbind log says which transport event actually freed the seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportCloseReason {
    /// The peer sent an application `CONNECTION_CLOSE` frame.
    ApplicationClose,
    /// The peer's UDP endpoint reset the connection.
    PeerReset,
    /// No packet arrived within the negotiated QUIC idle timeout.
    IdleTimeout,
    /// The peer's QUIC stack closed the connection for a transport reason.
    TransportClose,
    /// This process closed its side of the connection.
    LocalClose,
    /// Another transport failure, retaining iroh's diagnostic.
    Other(String),
}

impl std::fmt::Display for TransportCloseReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplicationClose => formatter.write_str("application close"),
            Self::PeerReset => formatter.write_str("peer reset"),
            Self::IdleTimeout => formatter.write_str("idle timeout"),
            Self::TransportClose => formatter.write_str("transport close"),
            Self::LocalClose => formatter.write_str("local close"),
            Self::Other(reason) => write!(formatter, "transport error: {reason}"),
        }
    }
}

/// Which lane a frame belongs to, matching what `collect_sends` drains and
/// `deliver` refills for in-process peers — plus the meta lane only this leg
/// carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The session's datagram lane: lossy, unordered.
    Datagram,
    /// The stream lane's shared stream: ordered, reliable.
    StreamShared,
    /// The stream lane's per-message bulk streams: reliable, unordered.
    StreamBulk,
    /// Out-of-band facts about the connection: remote interest-cell reports,
    /// host acknowledgments of impaired uplink datagrams and host hearsay
    /// contact folds.
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

    pub(crate) fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Datagram),
            1 => Some(Self::StreamShared),
            2 => Some(Self::StreamBulk),
            3 => Some(Self::Meta),
            _ => None,
        }
    }
}

/// One addressed message on the external leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Uplink: the recipient's swarm index. Downlink: the sender's swarm
    /// index. See the module docs for why the meaning flips.
    pub peer: u32,
    /// Which lane carries it.
    pub lane: Lane,
    /// The payload.
    pub payload: Bytes,
}

/// One sequenced datagram sent from an external peer into the host router.
///
/// The sequence belongs to the connection, not to a recipient: one exterior
/// connection carries datagrams for every island-mate, so a single monotonic
/// series lets the remote settle every write without per-peer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UplinkDatagram {
    /// Connection-local identifier copied into the corresponding [`UplinkAck`].
    pub sequence: u64,
    /// The datagram bytes the addressed bot receives when impairment keeps it.
    pub payload: Bytes,
}

impl UplinkDatagram {
    /// Encodes `[sequence u64][datagram bytes]` for a Datagram-lane [`Frame`].
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(8 + self.payload.len());
        out.put_u64_le(self.sequence);
        out.put_slice(&self.payload);
        out.freeze()
    }

    /// Decodes a Datagram-lane payload, refusing an absent sequence envelope.
    #[must_use]
    pub fn decode(payload: Bytes) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        let sequence = u64::from_le_bytes(payload[..8].try_into().expect("eight bytes read"));
        Some(Self {
            sequence,
            payload: payload.slice(8..),
        })
    }
}

/// The impaired router's settled decision for one uplink datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkOutcome {
    /// The router retained the datagram for immediate or delayed delivery.
    Delivered,
    /// The router's impairment decision discarded the datagram.
    Dropped,
}

/// Application-level evidence for one sequenced uplink datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UplinkAck {
    /// The connection-local sequence copied from [`UplinkDatagram`].
    pub sequence: u64,
    /// What the impaired router decided, after making that decision.
    pub outcome: UplinkOutcome,
}

impl UplinkAck {
    /// Meta payload discriminator. Cell reports are exactly eight bytes,
    /// announce frames are empty, and goodbye is the single byte `0xff`.
    const TAG: u8 = 0xa1;

    /// Encodes `[ack tag][outcome][sequence u64]` for a Meta-lane [`Frame`].
    #[must_use]
    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(10);
        out.put_u8(Self::TAG);
        out.put_u8(match self.outcome {
            UplinkOutcome::Delivered => 0,
            UplinkOutcome::Dropped => 1,
        });
        out.put_u64_le(self.sequence);
        out.freeze()
    }

    /// Decodes only the ACK member of the Meta lane grammar.
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

/// The party that computed a hearsay contact fold.
///
/// This is transmitted with every [`HearsayContacts`] record so hearsay
/// provenance survives the whole downlink rather than becoming a client-side
/// convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HearsaySource {
    /// The campaign host's fold over its roster of committed craft cells.
    HostRosterFold,
}

impl HearsaySource {
    const HOST_ROSTER_FOLD_TAG: u8 = 0x01;

    const fn tag(self) -> u8 {
        match self {
            Self::HostRosterFold => Self::HOST_ROSTER_FOLD_TAG,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            Self::HOST_ROSTER_FOLD_TAG => Some(Self::HostRosterFold),
            _ => None,
        }
    }
}

/// One source- and age-labelled craft contact in a [`HearsayContacts`] fold.
///
/// `cell` is raw 512-metre `CellId` bits. `fact_age_ticks` is the age of that
/// cell fact at `fold_tick`, rather than the age of the record envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HearsayContact {
    /// The roster seat to resolve to a craft identity.
    pub seat: u8,
    /// Raw `CellId` bits at the committed 512-metre level.
    pub cell: u64,
    /// Age of this cell fact when the source folded its roster.
    pub fact_age_ticks: u16,
}

/// A form-agnostic hearsay fold sent from the host on the existing Meta lane.
///
/// The encoding is `[tag 0xa2][source][fold_tick u64][count]` followed by
/// `count` `[seat][cell u64][fact_age_ticks u16]` triples, all little-endian.
/// It deliberately carries no rendering information: consumers may draw edge
/// arrows today or a minimap later from the same `(seat, cell, age)` facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HearsayContacts {
    /// Who computed this fold, transmitted to satisfy hearsay provenance.
    pub source: HearsaySource,
    /// Tick at which the source took this roster snapshot.
    pub fold_tick: u64,
    /// The source- and age-labelled contact facts in this fold.
    pub contacts: Vec<HearsayContact>,
}

impl HearsayContacts {
    /// Meta payload discriminator, distinct from the ACK tag (`0xa1`).
    const TAG: u8 = 0xa2;
    const HEADER_BYTES: usize = 11;
    const CONTACT_BYTES: usize = 11;

    /// Encodes this fold for a host-to-exterior Meta-lane [`Frame`].
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let count = u8::try_from(self.contacts.len()).expect("hearsay contact count fits u8");
        let mut out =
            BytesMut::with_capacity(Self::HEADER_BYTES + Self::CONTACT_BYTES * self.contacts.len());
        out.put_u8(Self::TAG);
        out.put_u8(self.source.tag());
        out.put_u64_le(self.fold_tick);
        out.put_u8(count);
        for contact in &self.contacts {
            out.put_u8(contact.seat);
            out.put_u64_le(contact.cell);
            out.put_u16_le(contact.fact_age_ticks);
        }
        out.freeze()
    }

    /// Decodes only the HearsayContacts member of the Meta lane grammar.
    ///
    /// Tags this version does not own, unknown sources, truncated records and
    /// length mismatches are ignored. The exact-length check keeps a corrupted
    /// count from being accepted as a plausible prefix of a larger fold.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let [tag, source, tail @ ..] = payload else {
            return None;
        };
        if *tag != Self::TAG || tail.len() < 9 {
            return None;
        }
        let source = HearsaySource::from_tag(*source)?;
        let fold_tick = u64::from_le_bytes(tail[..8].try_into().expect("eight bytes read"));
        let count = usize::from(tail[8]);
        let expected_len = Self::HEADER_BYTES + Self::CONTACT_BYTES * count;
        if payload.len() != expected_len {
            return None;
        }

        let mut contacts = Vec::with_capacity(count);
        for entry in payload[Self::HEADER_BYTES..].chunks_exact(Self::CONTACT_BYTES) {
            contacts.push(HearsayContact {
                seat: entry[0],
                cell: u64::from_le_bytes(entry[1..9].try_into().expect("entry has eleven bytes")),
                fact_age_ticks: u16::from_le_bytes(
                    entry[9..11].try_into().expect("entry has eleven bytes"),
                ),
            });
        }
        Some(Self {
            source,
            fold_tick,
            contacts,
        })
    }
}

/// Wire error: the byte stream can no longer be trusted to resync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameError;

/// Appends one framed message to `out`.
pub fn encode_frame(frame: &Frame, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = u32::try_from(frame.payload.len()).map_err(|_| FrameError)?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError);
    }
    out.put_u8(frame.lane.tag());
    out.put_u32_le(frame.peer);
    out.put_u32_le(len);
    out.put_slice(&frame.payload);
    Ok(())
}

/// Reads one complete frame from the head of `buf`, draining exactly its bytes.
///
/// Returns `Ok(None)` when `buf` holds less than one full frame — the caller
/// keeps accumulating and calls again. `Err` means the stream is unusable: an
/// unknown lane or a length past [`MAX_FRAME_BYTES`] cannot be skipped safely,
/// because frame boundaries after a desync are unknowable.
pub fn decode_frame(buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
    const HEADER: usize = 9;
    if buf.len() < HEADER {
        return Ok(None);
    }
    let Some(lane) = Lane::from_tag(buf[0]) else {
        return Err(FrameError);
    };
    let peer = u32::from_le_bytes(buf[1..5].try_into().expect("nine bytes read"));
    let len = u32::from_le_bytes(buf[5..9].try_into().expect("nine bytes read"));
    if len > MAX_FRAME_BYTES {
        return Err(FrameError);
    }
    let total = usize::try_from(u64::from(len) + u64::try_from(HEADER).expect("constant"))
        .expect("bounded by MAX_FRAME_BYTES");
    if buf.len() < total {
        return Ok(None);
    }
    let mut frame = buf.split_to(total);
    let payload = frame.split_off(HEADER);
    Ok(Some(Frame {
        peer,
        lane,
        payload: payload.freeze(),
    }))
}

/// The host-side handle for one connected external peer.
///
/// The swarm loop is synchronous and the socket is not, so a pump thread owns
/// the connection and these queues are the whole interface: it forwards
/// [`Frame`]s up, drains them down, and reports liveness through
/// [`PeerLink::connected`]. Bounded queues mean backpressure is visible — a
/// full downlink queue is counted as drops by the host rather than silently
/// buffering unbounded.
/// The channels are tokio's: the pumps are async tasks, and a blocking
/// `std` recv inside one parks a worker thread instead of yielding - the
/// exact bug class that made spawned pump writes vanish (#385).
#[derive(Debug)]
pub struct HostLink {
    /// Frames arriving from the remote peer.
    pub uplink: std::sync::Mutex<tokio::sync::mpsc::Receiver<Frame>>,
    /// Frames queued for delivery to the remote peer.
    pub downlink: tokio::sync::mpsc::Sender<Frame>,
    /// Cell updates carried on the meta lane, oldest first.
    pub meta: std::sync::Mutex<tokio::sync::mpsc::Receiver<u64>>,
    /// True for as long as `Connection::closed()` has not resolved.
    pub connected: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the reader when the runner's clean end-of-run marker arrived.
    pub goodbye: Arc<std::sync::atomic::AtomicBool>,
    /// Set only by `Connection::closed()`, with iroh's close classification.
    pub transport_close: Arc<Mutex<Option<TransportCloseReason>>>,
}

/// The remote-side mirror of [`HostLink`]: same queues, opposite directions.
///
/// The remote reports its own interest cell by pushing a `Meta`-lane frame
/// onto [`RemoteLink::uplink`] like any other traffic; there is no separate
/// meta channel on this side.
#[derive(Debug)]
pub struct RemoteLink {
    /// Frames arriving from the host.
    pub downlink: std::sync::Mutex<tokio::sync::mpsc::Receiver<Frame>>,
    /// Frames queued for transmission to the host, meta included.
    pub uplink: tokio::sync::mpsc::Sender<Frame>,
    /// True for as long as `Connection::closed()` has not resolved.
    pub connected: Arc<std::sync::atomic::AtomicBool>,
    /// The real connection, retained so graceful shutdown can send
    /// `CONNECTION_CLOSE` instead of relying on runtime destruction.
    pub connection: Option<iroh::endpoint::Connection>,
}

impl RemoteLink {
    /// Close the live QUIC connection after the application goodbye is flushed.
    pub fn close_transport(&self) {
        if let Some(connection) = &self.connection {
            connection.close(0u8.into(), b"external client shutdown");
        }
    }
}

/// A bounded queue pair's depth, chosen so a stalled pump is visible within a
/// second at criterion traffic rates rather than growing without bound.
pub const LINK_QUEUE_DEPTH: usize = 4_096;

/// The two ends of one logical connection, built before any IO starts.
///
/// Splitting construction from pumping keeps every property testable without a
/// runtime: the swarm tests can drive a [`HostLink`] directly, and the real
/// pumps in the binaries are the only code that needs tokio.
#[must_use]
pub fn link_pair() -> (HostLink, RemoteLink) {
    let (uplink_tx, uplink_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
    let (downlink_tx, downlink_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
    let (_, meta_rx) = tokio::sync::mpsc::channel(LINK_QUEUE_DEPTH);
    let connected = Arc::new(AtomicBool::new(true));
    let goodbye = Arc::new(AtomicBool::new(false));
    let transport_close = Arc::new(Mutex::new(None));
    (
        HostLink {
            uplink: std::sync::Mutex::new(uplink_rx),
            downlink: downlink_tx,
            meta: std::sync::Mutex::new(meta_rx),
            connected: Arc::clone(&connected),
            goodbye: Arc::clone(&goodbye),
            transport_close,
        },
        RemoteLink {
            downlink: std::sync::Mutex::new(downlink_rx),
            uplink: uplink_tx,
            // The remote's own cell reports ride its uplink as Meta frames;
            // this channel would only exist for a symmetry nobody uses.
            connected,
            connection: None,
        },
    )
}

/// The handshake a dialling peer sends before any combat traffic.
///
/// iroh authenticates the dialler's `NodeId` at the transport layer, so the
/// handshake carries no key material of its own — what it establishes is
/// protocol fit, build provenance and, since #387, the invite-bound session
/// identity (#345 §8): the pre-minted session UUIDv7 the invite carries and
/// the operator-signed `SessionTokenV1` authorizing this transport identity.
///
/// Version 4 appends the admission-granted slot to the identity tail. The slot
/// is an echo, not an allocator: the host checks it against admission's shared
/// reservation journal and refuses disagreement instead of correcting it.
/// Versions 2 and 3 are rejected rather than guessed because they cannot state
/// the reservation binding #583 requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    /// Build revision of the joining process, for the report and for pinning.
    pub client_rev: String,
    /// Pre-minted campaign session id from the invite (UUIDv7), when joining
    /// as a campaign participant.
    pub session_id: Option<String>,
    /// Encoded `orrery_protocol::SessionTokenV1` authorizing the dialler's
    /// transport identity, when joining as a campaign participant.
    pub token: Option<Vec<u8>>,
    /// Seat admission reserved for this session. Campaign joins must present
    /// it; identity-less harness probes may omit it.
    pub slot: Option<usize>,
}

impl JoinRequest {
    const MAGIC: [u8; 4] = *b"ORRX";
    const VERSION: u16 = 5;

    /// A plain, identity-less join — what the headless bot runner sends.
    #[must_use]
    pub fn plain(client_rev: String) -> Self {
        Self {
            client_rev,
            session_id: None,
            token: None,
            slot: None,
        }
    }

    /// Encodes the request onto the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::MAGIC.len() + 2 + 1 + self.client_rev.len());
        out.extend_from_slice(&Self::MAGIC);
        out.extend_from_slice(&Self::VERSION.to_le_bytes());
        let rev = self.client_rev.as_bytes();
        out.push(u8::try_from(rev.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(&rev[..rev.len().min(u8::MAX.into())]);
        if self.session_id.is_some() || self.token.is_some() || self.slot.is_some() {
            let session = self.session_id.as_deref().unwrap_or("").as_bytes();
            out.push(u8::try_from(session.len()).unwrap_or(u8::MAX));
            out.extend_from_slice(&session[..session.len().min(u8::MAX.into())]);
            let token = self.token.as_deref().unwrap_or(&[]);
            let token_len = u16::try_from(token.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&token_len.to_le_bytes());
            out.extend_from_slice(&token[..token.len().min(u16::MAX.into())]);
            let slot = self
                .slot
                .and_then(|slot| u64::try_from(slot).ok())
                .unwrap_or(u64::MAX);
            out.extend_from_slice(&slot.to_le_bytes());
        }
        out
    }

    /// Decodes a request. Wrong magic or version is a rejection, not a guess.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let magic_len = Self::MAGIC.len();
        if bytes.len() < magic_len || bytes[..magic_len] != Self::MAGIC {
            return Err("not an orrery exterior join");
        }
        let rest = &bytes[magic_len..];
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
        let client_rev = String::from_utf8_lossy(&rest[..rev_len]).into_owned();
        let rest = &rest[rev_len..];
        if rest.is_empty() {
            // A pre-#387 request: no identity tail was sent.
            return Ok(Self {
                client_rev,
                session_id: None,
                token: None,
                slot: None,
            });
        }
        let session_len = rest[0] as usize;
        let rest = &rest[1..];
        if rest.len() < session_len {
            return Err("join truncated inside session id");
        }
        let session_id = match &rest[..session_len] {
            [] => None,
            bytes => Some(String::from_utf8_lossy(bytes).into_owned()),
        };
        let rest = &rest[session_len..];
        if rest.len() < 2 {
            return Err("join truncated before token length");
        }
        let token_len = u16::from_le_bytes(rest[0..2].try_into().expect("two bytes read")) as usize;
        let rest = &rest[2..];
        if rest.len() < token_len {
            return Err("join truncated inside token");
        }
        let token = match &rest[..token_len] {
            [] => None,
            bytes => Some(bytes.to_vec()),
        };
        let rest = &rest[token_len..];
        if rest.len() != 8 {
            return Err("join truncated before reserved slot");
        }
        let encoded_slot = u64::from_le_bytes(rest.try_into().expect("eight bytes checked"));
        let slot = if encoded_slot == u64::MAX {
            None
        } else {
            Some(usize::try_from(encoded_slot).map_err(|_| "join slot overflow")?)
        };
        Ok(Self {
            client_rev,
            session_id,
            token,
            slot,
        })
    }
}

/// One authoritative row written atomically by `scripts/admission.py`.
#[derive(Debug, Deserialize)]
struct ReservationRow {
    attempt_id: String,
    slot: usize,
    session_id: String,
    node: String,
    expires_at: u64,
}

/// Host-visible view of admission's reservation journal (#583).
///
/// This is intentionally a co-location contract, not a cryptographic grant:
/// admission and the harness must see the same storage. Today the two systemd
/// units run on hel1 and the supervisor passes admission's `slots.json` path
/// into the child. If either service moves, this design no longer holds and
/// must be replaced by a signed reservation grant. Every read, parse, expiry,
/// generation, identity, session, or slot failure refuses the join; an
/// unavailable journal must never degrade to admission.
#[derive(Debug, Clone)]
pub struct ReservationJournal {
    /// Atomic JSON journal written by admission.
    pub path: PathBuf,
    /// Supervisor-owned attempt generation this host is running.
    pub attempt_id: String,
}

impl ReservationJournal {
    fn verify(
        &self,
        request: &JoinRequest,
        remote: &orrery_protocol::NodeId,
        slot: usize,
        now_ms: u64,
    ) -> Result<(), String> {
        let bytes = std::fs::read(&self.path).map_err(|error| {
            format!(
                "reservation_journal_unreadable: cannot read {}: {error}",
                self.path.display()
            )
        })?;
        let rows: Vec<ReservationRow> = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "reservation_journal_unreadable: {} did not decode: {error}",
                self.path.display()
            )
        })?;
        let session = request.session_id.as_deref().ok_or_else(|| {
            "reservation_missing_session: no invite session id was presented".to_owned()
        })?;
        let row = rows
            .iter()
            .find(|row| row.session_id == session)
            .ok_or_else(|| {
                format!("reservation_not_found: session {session} has no host-visible reservation")
            })?;
        if row.attempt_id != self.attempt_id {
            return Err(format!(
                "reservation_journal_stale: session {session} belongs to attempt {}, host is running {}",
                row.attempt_id, self.attempt_id
            ));
        }
        if row.expires_at <= now_ms / 1_000 {
            return Err(format!(
                "reservation_journal_stale: session {session} expired at {}",
                row.expires_at
            ));
        }
        if row.node != remote.to_string() {
            return Err(format!(
                "reservation_node_mismatch: session {session} is reserved for another transport identity"
            ));
        }
        if row.slot != slot {
            return Err(format!(
                "reservation_slot_mismatch: requested slot {slot}, journal reserved {}",
                row.slot
            ));
        }
        Ok(())
    }
}

/// What the host demands of a [`JoinRequest`] before it answers Accept
/// (#345 §8, wired by #387).
///
/// Every field is opt-in so the pure-bot legs and the headless runner keep
/// joining exactly as before; a campaign host configures all three. The
/// judgement is a pure function of the request, the dialler's authenticated
/// transport identity, and a clock — so it is testable without a socket, and
/// `bridge::host_accept` only has to deliver its verdict.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    /// Exact build revision the host pins (#345 §8's version pinning). A
    /// mismatching or absent revision is refused with a remedy in the reason.
    pub require_client_rev: Option<String>,
    /// The pre-minted invite session id this slot was reserved for. The
    /// request must present exactly this UUIDv7.
    pub require_session: Option<String>,
    /// Trusted issuer key. When set, the request must carry a
    /// `SessionTokenV1` that verifies under it *for the dialler's transport
    /// identity* — a token minted for any other node is refused.
    pub issuer: Option<orrery_protocol::IssuerKey>,
    /// Required for a standing, token-gated campaign with no single pinned
    /// session. See [`ReservationJournal`] for the co-location boundary.
    pub reservation_journal: Option<ReservationJournal>,
}

impl Admission {
    /// An open door: what every pre-#387 call site had.
    #[must_use]
    pub fn open() -> Self {
        Self::default()
    }

    /// Judge one request from `remote` against this policy, at the wall clock.
    ///
    /// # Errors
    /// The refusal reason, worded for the volunteer who will read it.
    pub fn judge(
        &self,
        request: &JoinRequest,
        remote: &orrery_protocol::NodeId,
    ) -> Result<(), String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        self.judge_at(request, remote, now_ms)
    }

    /// [`Self::judge`] with an injected clock, for tests.
    ///
    /// # Errors
    /// The refusal reason, worded for the volunteer who will read it.
    pub fn judge_at(
        &self,
        request: &JoinRequest,
        remote: &orrery_protocol::NodeId,
        now_ms: u64,
    ) -> Result<(), String> {
        let slot = request.slot.unwrap_or(usize::MAX);
        self.judge_at_for_slot(request, remote, slot, now_ms)
    }

    /// Judge a request for the exact slot the host is about to assign.
    pub fn judge_at_for_slot(
        &self,
        request: &JoinRequest,
        remote: &orrery_protocol::NodeId,
        slot: usize,
        now_ms: u64,
    ) -> Result<(), String> {
        if let Some(pinned) = &self.require_client_rev {
            if &request.client_rev != pinned {
                return Err(format!(
                    "client build {} is not the pinned build {pinned}; download the current build",
                    request.client_rev
                ));
            }
        }
        if let Some(expected) = &self.require_session {
            match request.session_id.as_deref() {
                Some(session) if session == expected => {}
                Some(session) => {
                    return Err(format!(
                        "invite session {session} does not match the session this host is running"
                    ));
                }
                None => return Err("no invite session id was presented".to_owned()),
            }
        }
        if let Some(issuer) = &self.issuer {
            let Some(token) = request.token.as_deref() else {
                return Err("no session token was presented".to_owned());
            };
            let verifier = orrery_protocol::SessionTokenVerifier::new(
                orrery_protocol::FixedTokenClock::new(orrery_protocol::UnixMillis::new(now_ms)),
                [issuer.clone()],
            );
            verifier
                .verify(token, remote)
                .map_err(|error| format!("session token refused: {error:?}"))?;
        }
        if let Some(claimed) = request.slot {
            if claimed != slot {
                return Err(format!(
                    "reservation_slot_mismatch: requested slot {claimed}, host is assigning {slot}"
                ));
            }
        }
        if let Some(journal) = &self.reservation_journal {
            journal.verify(request, remote, slot, now_ms)?;
        }
        Ok(())
    }
}

/// One active seat in a frozen [`StartManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActiveSeat {
    /// Stable swarm slot.
    pub slot: usize,
    /// Hex transport identity holding the slot.
    pub node: String,
    /// Persistent entity id flown from tick zero.
    pub entity: u64,
}

/// `StartV1`, field-for-field with the client half in PR #582.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StartManifest {
    /// Supervisor-owned attempt generation.
    pub attempt_id: String,
    /// Host swarm seed. Carried for provenance; the client does not compare it
    /// to its distinct 32-byte universe seed.
    pub seed: u64,
    /// Membership freeze tick; zero in this lobby cut.
    pub tick: u64,
    /// Configured seat namespace, including inactive seats.
    pub island_seats: u16,
    /// Bots and humans connected when membership froze.
    pub active: Vec<ActiveSeat>,
    /// This subject's host-chosen frozen witness ring.
    pub witness_recipients: Vec<usize>,
    /// Active run duration after Start, in ticks.
    pub duration_ticks: u64,
}

/// The host's answer to a [`JoinRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinReply {
    /// The sender joins at this swarm index; its `PersistId` derives from the
    /// index exactly as a bot's does.
    Accept {
        /// The swarm slot assigned to this peer.
        index: usize,
        /// Frozen active membership. A bare nine-byte accept remains the
        /// compatibility form and carries `None`.
        manifest: Option<StartManifest>,
    },
    /// The host refuses the join; the reason names itself.
    Reject {
        /// Why the join was refused.
        reason: String,
    },
    /// The lobby is still filling and this seat is still held.
    ///
    /// Sent on a fixed cadence for as long as the seat waits (#994). Two
    /// things ride on it. It is *traffic*: a lobby wait of minutes across a
    /// ten-second [`crate::bridge::EXTERIOR_MAX_IDLE_TIMEOUT`] should never be
    /// carried by QUIC keep-alive PINGs alone, and a write that fails is how
    /// the host learns it has lost the peer while the lobby is still open
    /// rather than at `StartV1`. And it is *news*: a client that has heard one
    /// knows it is queued, and a client that stops hearing them knows it is
    /// not, instead of waiting out a dial deadline for a truncated handshake.
    LobbyWait {
        /// Human seats connected to this lobby so far.
        seated: u16,
        /// Human seats the lobby is waiting to fill.
        needed: u16,
    },
    /// The host gave this seat back while the peer waited in the lobby.
    ///
    /// Best effort by construction: the common cause is a connection the host
    /// can no longer write to. The client's own lobby-heartbeat grace is what
    /// makes the outcome intelligible when this cannot be delivered; a peer
    /// whose path is degraded rather than gone still gets told why.
    Evicted {
        /// Why the seat was given back.
        reason: String,
    },
}

impl JoinReply {
    /// Encodes the reply onto the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Accept { index, manifest } => {
                out.push(0);
                let index = u64::try_from(*index).unwrap_or(u64::MAX);
                out.extend_from_slice(&index.to_le_bytes());
                if let Some(manifest) = manifest {
                    out.extend_from_slice(
                        &serde_json::to_vec(manifest).expect("StartManifest serializes"),
                    );
                }
            }
            Self::Reject { reason } => {
                out.push(1);
                let reason = reason.as_bytes();
                out.push(u8::try_from(reason.len()).unwrap_or(u8::MAX));
                out.extend_from_slice(&reason[..reason.len().min(u8::MAX.into())]);
            }
            Self::LobbyWait { seated, needed } => {
                out.push(2);
                out.extend_from_slice(&seated.to_le_bytes());
                out.extend_from_slice(&needed.to_le_bytes());
            }
            Self::Evicted { reason } => {
                out.push(3);
                let reason = reason.as_bytes();
                out.push(u8::try_from(reason.len()).unwrap_or(u8::MAX));
                out.extend_from_slice(&reason[..reason.len().min(u8::MAX.into())]);
            }
        }
        out
    }

    /// Decodes a reply.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        match bytes.first() {
            Some(0) => {
                let Some(rest) = bytes.get(1..9) else {
                    return Err("accept truncated");
                };
                let index =
                    usize::try_from(u64::from_le_bytes(rest.try_into().expect("eight bytes")))
                        .map_err(|_| "accept index overflow")?;
                let manifest = match bytes.get(9..) {
                    None | Some([]) => None,
                    Some(payload) => Some(
                        serde_json::from_slice(payload)
                            .map_err(|_| "accept manifest did not decode")?,
                    ),
                };
                Ok(Self::Accept { index, manifest })
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
            Some(2) => {
                let Some(body) = bytes.get(1..5) else {
                    return Err("lobby wait truncated");
                };
                Ok(Self::LobbyWait {
                    seated: u16::from_le_bytes(body[0..2].try_into().expect("two bytes")),
                    needed: u16::from_le_bytes(body[2..4].try_into().expect("two bytes")),
                })
            }
            Some(3) => {
                let Some(&len) = bytes.get(1) else {
                    return Err("eviction truncated");
                };
                let Some(reason) = bytes.get(2..2 + usize::from(len)) else {
                    return Err("eviction reason truncated");
                };
                Ok(Self::Evicted {
                    reason: String::from_utf8_lossy(reason).into_owned(),
                })
            }
            _ => Err("unknown join reply"),
        }
    }
}

/// The witness anchor a joining peer ships after `JoinReply::Accept`.
///
/// In-process, `seed_witnesses` reads each subject's tick-zero claim straight
/// out of its own `Chain`. The external peer's chain lives in another process,
/// so the claim travels instead — the same signed bytes its watchers would
/// otherwise have read locally. A run with witnessing off needs no anchor and
/// sends none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorFrame {
    /// The joining peer's signed tick-zero claim.
    pub claim_json: Vec<u8>,
    /// The state that claim commits to, in canonical encoding.
    pub state: Vec<u8>,
}

impl AnchorFrame {
    /// Encodes the anchor onto the wire: `[len u32][claim json][state]`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.claim_json.len() + self.state.len());
        let len = u32::try_from(self.claim_json.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.claim_json[..len as usize]);
        out.extend_from_slice(&self.state);
        out
    }

    /// Decodes an anchor from the exact bytes read off the wire.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < 4 {
            return Err("anchor truncated before length");
        }
        let len = u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes read")) as usize;
        let rest = bytes.get(4..).ok_or("anchor truncated before claim")?;
        if rest.len() < len {
            return Err("anchor truncated inside claim");
        }
        Ok(Self {
            claim_json: rest[..len].to_vec(),
            state: rest[len..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(peer: u32, lane: Lane, payload: &[u8]) -> Frame {
        Frame {
            peer,
            lane,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn frames_round_trip_per_lane() {
        for (lane, payload) in [
            (Lane::Datagram, &b"replication bytes"[..]),
            (Lane::StreamShared, b"log frame".as_ref()),
            (Lane::StreamBulk, b"".as_ref()),
            (Lane::Meta, b"cell bytes".as_ref()),
        ] {
            let sent = frame(17, lane, payload);
            let mut wire = Vec::new();
            encode_frame(&sent, &mut wire).expect("in range");
            let mut buf = BytesMut::from(&wire[..]);
            assert_eq!(
                decode_frame(&mut buf)
                    .expect("well formed")
                    .expect("complete"),
                sent
            );
            assert!(buf.is_empty(), "exactly one frame consumed");
        }
    }

    #[test]
    fn uplink_datagrams_and_meta_acks_round_trip_their_sequence() {
        let datagram = UplinkDatagram {
            sequence: 0x0123_4567_89ab_cdef,
            payload: Bytes::from_static(b"logical datagram"),
        };
        assert_eq!(UplinkDatagram::decode(datagram.encode()), Some(datagram));

        for outcome in [UplinkOutcome::Delivered, UplinkOutcome::Dropped] {
            let ack = UplinkAck {
                sequence: 0xfeed_face_cafe_beef,
                outcome,
            };
            assert_eq!(UplinkAck::decode(&ack.encode()), Some(ack));
        }
        assert_eq!(UplinkAck::decode(&7u64.to_le_bytes()), None, "not a cell");
        assert_eq!(UplinkAck::decode(&[0xff]), None, "not goodbye");
    }

    #[test]
    fn hearsay_contacts_round_trip_at_the_eight_craft_budget() {
        let contacts = HearsayContacts {
            source: HearsaySource::HostRosterFold,
            fold_tick: 0x0123_4567_89ab_cdef,
            contacts: (0..8)
                .map(|seat| HearsayContact {
                    seat,
                    cell: 0x1000_0000_0000_0000 + u64::from(seat),
                    fact_age_ticks: 256 + u16::from(seat),
                })
                .collect(),
        };

        let wire = contacts.encode();
        assert_eq!(wire.len(), 99, "11-byte header plus eight 11-byte contacts");
        assert_eq!(&wire[..2], &[0xa2, 0x01], "tag and source are on the wire");
        assert_eq!(HearsayContacts::decode(&wire), Some(contacts));
        assert_eq!(
            UplinkAck::decode(&wire),
            None,
            "an ACK-only older client ignores the unknown Meta tag"
        );
    }

    #[test]
    fn hearsay_contacts_decode_source_cell_and_age_from_wire() {
        let wire = [
            0xa2, 0x01, // tag, host-roster-fold source
            0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01, // fold tick
            0x02, // count
            0x03, // seat
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // cell
            0x00, 0x01, // fact age
            0x07, // seat
            0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, // cell
            0xfe, 0xca, // fact age
        ];
        assert_eq!(
            HearsayContacts::decode(&wire),
            Some(HearsayContacts {
                source: HearsaySource::HostRosterFold,
                fold_tick: 0x0123_4567_89ab_cdef,
                contacts: vec![
                    HearsayContact {
                        seat: 3,
                        cell: 0x1122_3344_5566_7788,
                        fact_age_ticks: 256,
                    },
                    HearsayContact {
                        seat: 7,
                        cell: 0x99aa_bbcc_ddee_ff00,
                        fact_age_ticks: 0xcafe,
                    },
                ],
            })
        );
    }

    #[test]
    fn hearsay_contacts_reject_corrupted_count_in_transit() {
        let contacts = HearsayContacts {
            source: HearsaySource::HostRosterFold,
            fold_tick: 120,
            contacts: vec![
                HearsayContact {
                    seat: 2,
                    cell: 11,
                    fact_age_ticks: 256,
                },
                HearsayContact {
                    seat: 5,
                    cell: 22,
                    fact_age_ticks: 512,
                },
            ],
        };
        let mut wire = contacts.encode().to_vec();
        wire[10] = 1; // Transit corruption: only one of the two entries is claimed.

        assert_eq!(
            HearsayContacts::decode(&wire),
            None,
            "a corrupted count must not decode to a plausible contact prefix"
        );
    }

    #[test]
    fn hearsay_contacts_ignore_unknown_tags_and_sources() {
        let contacts = HearsayContacts {
            source: HearsaySource::HostRosterFold,
            fold_tick: 0,
            contacts: Vec::new(),
        };
        let mut wire = contacts.encode().to_vec();
        wire[0] = 0xa3;
        assert_eq!(HearsayContacts::decode(&wire), None, "unknown tag ignored");

        wire[0] = 0xa2;
        wire[1] = 0x02;
        assert_eq!(
            HearsayContacts::decode(&wire),
            None,
            "unknown source ignored"
        );
    }

    #[test]
    fn partial_frames_wait_and_then_complete() {
        let mut wire = Vec::new();
        encode_frame(&frame(1, Lane::Datagram, b"hello world"), &mut wire).expect("in range");
        let mut buf = BytesMut::new();

        // Header split across arrivals: nothing decoded, nothing lost.
        buf.extend_from_slice(&wire[..3]);
        assert_eq!(decode_frame(&mut buf), Ok(None));
        buf.extend_from_slice(&wire[3..12]);
        assert_eq!(decode_frame(&mut buf), Ok(None));
        buf.extend_from_slice(&wire[12..]);
        let decoded = decode_frame(&mut buf)
            .expect("well formed")
            .expect("complete");
        assert_eq!(decoded.payload.as_ref(), b"hello world");
        assert_eq!(decoded.peer, 1);
    }

    #[test]
    fn two_frames_in_one_arrival_decode_in_order() {
        let mut wire = Vec::new();
        for (peer, payload) in [(4u32, &b"first"[..]), (9, &b"second"[..])] {
            encode_frame(&frame(peer, Lane::Datagram, payload), &mut wire).expect("in range");
        }
        let mut buf = BytesMut::from(&wire[..]);
        assert_eq!(
            decode_frame(&mut buf)
                .expect("ok")
                .expect("full")
                .payload
                .as_ref(),
            b"first"
        );
        let second = decode_frame(&mut buf).expect("ok").expect("full");
        assert_eq!(
            (second.peer, second.payload.as_ref()),
            (9, b"second".as_ref())
        );
        assert_eq!(decode_frame(&mut buf), Ok(None), "drained exactly");
    }

    #[test]
    fn an_unknown_lane_and_an_oversized_length_are_fatal_not_skippable() {
        let mut buf = BytesMut::from(&[9u8, 1, 0, 0, 0, 1, 0, 0, 0][..]);
        assert_eq!(decode_frame(&mut buf), Err(FrameError));

        let mut buf = BytesMut::from(&[0u8, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f][..]);
        assert_eq!(decode_frame(&mut buf), Err(FrameError));

        // And encoding refuses the same bound symmetrically.
        let big = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        assert_eq!(
            encode_frame(&frame(0, Lane::Datagram, &big), &mut Vec::new()),
            Err(FrameError)
        );
    }

    #[test]
    fn join_requests_round_trip_and_reject_wrong_versions() {
        let request = JoinRequest::plain("0ce6b28b".to_owned());
        let decoded = JoinRequest::decode(&request.encode()).expect("well formed");
        assert_eq!(decoded, request);

        let mut wrong_version = request.encode();
        wrong_version[4] = 0xFF;
        assert!(JoinRequest::decode(&wrong_version).is_err());

        assert!(
            JoinRequest::decode(b"NOPE\x01\x00").is_err(),
            "magic checked"
        );
        assert!(JoinRequest::decode(b"ORRX").is_err(), "truncation checked");
    }

    /// The #387 identity tail round-trips, and its absence decodes to `None`
    /// on both fields — a pre-#387 runner's request still parses.
    #[test]
    fn join_request_identity_tail_round_trips_and_stays_optional() {
        let full = JoinRequest {
            client_rev: "abc123".to_owned(),
            session_id: Some("018f8f4e-5c90-7abc-8123-000000000001".to_owned()),
            token: Some(vec![1, 2, 3, 4]),
            slot: Some(7),
        };
        assert_eq!(JoinRequest::decode(&full.encode()), Ok(full.clone()));

        let plain = JoinRequest::plain("abc123".to_owned());
        assert_eq!(JoinRequest::decode(&plain.encode()), Ok(plain));

        let mut truncated = full.encode();
        truncated.truncate(truncated.len() - 1);
        assert!(
            JoinRequest::decode(&truncated).is_err(),
            "a partial token is a refusal, not a shorter token"
        );
    }

    fn signed_token(
        issuer_secret: &iroh_base::SecretKey,
        key_id: u32,
        node: orrery_protocol::NodeId,
        issued_at_ms: u64,
    ) -> Vec<u8> {
        let claims = orrery_protocol::SessionTokenClaimsV1::new(
            orrery_protocol::AccountId(7),
            node,
            orrery_protocol::UnixMillis::new(issued_at_ms),
            orrery_protocol::SessionTokenTtlMs(3_600_000),
            orrery_protocol::SessionStanding::Good,
            orrery_protocol::IssuerKeyId::new(key_id),
            true,
        );
        orrery_protocol::SessionTokenV1::sign(claims, issuer_secret)
            .expect("sign")
            .encode()
            .expect("encode")
    }

    /// #345 §8 at the only admission point: a stale build, a wrong or absent
    /// invite session, and a token minted for a different transport identity
    /// are each refused with their own reason; the invited request passes.
    #[test]
    fn admission_judges_rev_session_and_token() {
        const NOW_MS: u64 = 1_756_000_000_000;
        let issuer_secret = iroh_base::SecretKey::from_bytes(&[0x51; 32]);
        let dialler = iroh_base::SecretKey::from_bytes(&[0x52; 32]).public();
        let other = iroh_base::SecretKey::from_bytes(&[0x53; 32]).public();
        let session = "018f8f4e-5c90-7abc-8123-000000000042";
        let admission = Admission {
            require_client_rev: Some("pinned-rev".to_owned()),
            require_session: Some(session.to_owned()),
            issuer: Some(orrery_protocol::IssuerKey::new(
                orrery_protocol::IssuerKeyId::new(41),
                issuer_secret.public(),
            )),
            reservation_journal: None,
        };
        let invited = JoinRequest {
            client_rev: "pinned-rev".to_owned(),
            session_id: Some(session.to_owned()),
            token: Some(signed_token(&issuer_secret, 41, dialler, NOW_MS - 1_000)),
            slot: None,
        };
        assert_eq!(admission.judge_at(&invited, &dialler, NOW_MS), Ok(()));

        // Stale build: refused with the download remedy (#345 §8).
        let stale = JoinRequest {
            client_rev: "older-rev".to_owned(),
            ..invited.clone()
        };
        let reason = admission.judge_at(&stale, &dialler, NOW_MS).unwrap_err();
        assert!(
            reason.contains("download the current build"),
            "got: {reason}"
        );

        // Absent and mismatching session ids are distinct refusals.
        let mut no_session = invited.clone();
        no_session.session_id = None;
        assert!(admission
            .judge_at(&no_session, &dialler, NOW_MS)
            .unwrap_err()
            .contains("no invite session id"));
        let mut wrong_session = invited.clone();
        wrong_session.session_id = Some("018f8f4e-5c90-7abc-8123-00000000dead".to_owned());
        assert!(admission
            .judge_at(&wrong_session, &dialler, NOW_MS)
            .unwrap_err()
            .contains("does not match"));

        // A verifying token minted for a *different* node is refused: the
        // judgement binds the token to the authenticated transport identity.
        let reason = admission.judge_at(&invited, &other, NOW_MS).unwrap_err();
        assert!(reason.contains("session token refused"), "got: {reason}");

        // An expired token is refused even from the right node.
        let mut expired = invited.clone();
        expired.token = Some(signed_token(
            &issuer_secret,
            41,
            dialler,
            NOW_MS - 3_600_001,
        ));
        assert!(admission
            .judge_at(&expired, &dialler, NOW_MS)
            .unwrap_err()
            .contains("session token refused"));

        // No token at all, while an issuer is configured, is its own refusal.
        let mut tokenless = invited.clone();
        tokenless.token = None;
        assert!(admission
            .judge_at(&tokenless, &dialler, NOW_MS)
            .unwrap_err()
            .contains("no session token"));

        // And the open door still admits a plain pre-#387 request.
        assert_eq!(
            Admission::open().judge_at(&JoinRequest::plain("any".into()), &dialler, NOW_MS),
            Ok(())
        );
    }

    fn reservation_path(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "p1-swarm-{name}-{}-{}-{nonce}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    /// #583's load-bearing mutation: the token is valid for this node and
    /// session, but the request edits the admission-granted seat. The journal
    /// is authoritative and the host refuses instead of correcting it.
    #[test]
    fn valid_token_for_a_different_reserved_slot_is_refused() {
        const NOW_MS: u64 = 1_756_000_000_000;
        let issuer = iroh_base::SecretKey::from_bytes(&[0x61; 32]);
        let dialler = iroh_base::SecretKey::from_bytes(&[0x62; 32]).public();
        let session = "018f8f4e-5c90-7abc-8123-000000000083";
        let path = reservation_path("wrong-slot");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!([{
                "attempt_id": "attempt-live",
                "slot": 4,
                "session_id": session,
                "node": dialler.to_string(),
                "expires_at": NOW_MS / 1_000 + 60
            }]))
            .expect("journal serializes"),
        )
        .expect("journal written");
        let admission = Admission {
            issuer: Some(orrery_protocol::IssuerKey::new(
                orrery_protocol::IssuerKeyId::new(583),
                issuer.public(),
            )),
            reservation_journal: Some(ReservationJournal {
                path: path.clone(),
                attempt_id: "attempt-live".to_owned(),
            }),
            ..Admission::default()
        };
        let request = JoinRequest {
            client_rev: "test".to_owned(),
            session_id: Some(session.to_owned()),
            token: Some(signed_token(&issuer, 583, dialler, NOW_MS - 1_000)),
            slot: Some(5),
        };
        let reason = admission
            .judge_at_for_slot(&request, &dialler, 5, NOW_MS)
            .expect_err("a valid node token does not authorize another seat");
        assert_eq!(
            reason,
            "reservation_slot_mismatch: requested slot 5, journal reserved 4"
        );
        std::fs::remove_file(path).expect("journal removed");
    }

    /// Journal availability is part of admission, never an optional hint.
    #[test]
    fn unreadable_reservation_journal_refuses_the_join() {
        const NOW_MS: u64 = 1_756_000_000_000;
        let issuer = iroh_base::SecretKey::from_bytes(&[0x63; 32]);
        let dialler = iroh_base::SecretKey::from_bytes(&[0x64; 32]).public();
        let path = reservation_path("unreadable");
        std::fs::create_dir(&path).expect("directory stands in for unreadable file");
        let admission = Admission {
            issuer: Some(orrery_protocol::IssuerKey::new(
                orrery_protocol::IssuerKeyId::new(584),
                issuer.public(),
            )),
            reservation_journal: Some(ReservationJournal {
                path: path.clone(),
                attempt_id: "attempt-live".to_owned(),
            }),
            ..Admission::default()
        };
        let request = JoinRequest {
            client_rev: "test".to_owned(),
            session_id: Some("018f8f4e-5c90-7abc-8123-000000000084".to_owned()),
            token: Some(signed_token(&issuer, 584, dialler, NOW_MS - 1_000)),
            slot: Some(4),
        };
        let reason = admission
            .judge_at_for_slot(&request, &dialler, 4, NOW_MS)
            .expect_err("journal failure must fail closed");
        assert!(
            reason.starts_with("reservation_journal_unreadable:"),
            "named refusal, got: {reason}"
        );
        std::fs::remove_dir(path).expect("directory removed");
    }

    #[test]
    fn join_replies_round_trip_both_arms() {
        let accept = JoinReply::Accept {
            index: 32,
            manifest: None,
        };
        assert_eq!(JoinReply::decode(&accept.encode()), Ok(accept.clone()));

        let manifest = StartManifest {
            attempt_id: "attempt-7".to_owned(),
            seed: 99,
            tick: 0,
            island_seats: 8,
            active: vec![ActiveSeat {
                slot: 4,
                node: "ab".repeat(32),
                entity: 5,
            }],
            witness_recipients: vec![0, 1, 2],
            duration_ticks: 54_000,
        };
        let started = JoinReply::Accept {
            index: 4,
            manifest: Some(manifest),
        };
        assert_eq!(JoinReply::decode(&started.encode()), Ok(started));
        let mut broken_manifest = JoinReply::Accept {
            index: 4,
            manifest: None,
        }
        .encode();
        broken_manifest.extend_from_slice(b"{");
        assert_eq!(
            JoinReply::decode(&broken_manifest),
            Err("accept manifest did not decode"),
            "trailing bytes are a manifest, never ignored padding"
        );

        let reject = JoinReply::Reject {
            reason: "client rev pinned out".to_owned(),
        };
        assert_eq!(JoinReply::decode(&reject.encode()), Ok(reject.clone()));

        let waiting = JoinReply::LobbyWait {
            seated: 2,
            needed: 3,
        };
        assert_eq!(JoinReply::decode(&waiting.encode()), Ok(waiting.clone()));
        assert_eq!(
            JoinReply::decode(&waiting.encode()[..3]),
            Err("lobby wait truncated"),
            "a half-read beat is not a beat"
        );

        let evicted = JoinReply::Evicted {
            reason: "the host lost contact while the run was filling".to_owned(),
        };
        assert_eq!(JoinReply::decode(&evicted.encode()), Ok(evicted.clone()));
        assert_ne!(
            evicted.encode()[0],
            reject.encode()[0],
            "an eviction is not a refusal: the client reaches a different state for each"
        );

        assert!(JoinReply::decode(&[7]).is_err(), "unknown tag refused");
        assert!(
            JoinReply::decode(&[0, 1, 2]).is_err(),
            "short index refused"
        );
    }

    #[test]
    fn anchors_round_trip_claim_and_state_bytes() {
        let anchor = AnchorFrame {
            claim_json: br#"{"entity":33}"#.to_vec(),
            state: vec![1, 2, 3, 4],
        };
        assert_eq!(AnchorFrame::decode(&anchor.encode()), Ok(anchor.clone()));
        assert!(AnchorFrame::decode(&[0, 0]).is_err(), "truncation refused");
    }
}
