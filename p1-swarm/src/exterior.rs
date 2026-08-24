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
//!   for a whole island membership, so facts a real peer would learn from many
//!   sources — its current interest cell, once per simulated second — travel
//!   beside the combat traffic rather than inside it.
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

// The host-side bridge and the external-peer mode that consume this wire land
// in the following commits of #385; until they exist only the tests read it.
#![allow(dead_code)]

use bytes::{BufMut, Bytes, BytesMut};

/// `std::sync::mpsc`, named so call sites read as deliberately std rather than
/// accidentally unqualified.
pub(crate) mod std_mpsc {
    pub use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
}

/// Longest frame the wire will carry or accept.
///
/// Bots link at MTU 1_200 (`Bot::link`), but witness log frames and repair
/// control travel whole on the stream lane, so the cap is generous rather than
/// MTU-bound. It exists to bound a hostile or desynced length field, not to
/// police the senders, who never approach it.
pub const MAX_FRAME_BYTES: u32 = 64 * 1_024;

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
    /// Out-of-band facts about the connection itself. Currently one: the
    /// external peer's interest cell, once per simulated second.
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

    const fn from_tag(tag: u8) -> Option<Self> {
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
#[derive(Debug)]
pub struct HostLink {
    /// Frames arriving from the remote peer.
    pub uplink: std_mpsc::Receiver<Frame>,
    /// Frames queued for delivery to the remote peer.
    pub downlink: std_mpsc::SyncSender<Frame>,
    /// Cell updates carried on the meta lane, oldest first.
    pub meta: std_mpsc::Receiver<u64>,
    /// False for as long as the pump believes the connection is alive.
    pub connected: arc_swap::Arc<std::sync::atomic::AtomicBool>,
}

/// The remote-side mirror of [`HostLink`]: same queues, opposite directions.
#[derive(Debug)]
pub struct RemoteLink {
    /// Frames arriving from the host.
    pub downlink: std_mpsc::Receiver<Frame>,
    /// Frames queued for transmission to the host.
    pub uplink: std_mpsc::SyncSender<Frame>,
    /// Cell updates this side should send, once per simulated second.
    pub meta_tx: std_mpsc::SyncSender<u64>,
    /// False for as long as the pump believes the connection is alive.
    pub connected: arc_swap::Arc<std::sync::atomic::AtomicBool>,
}

/// Alias so both link types can name the shared liveness flag without
/// depending on an atomic-swap crate that is not otherwise used here.
mod arc_swap {
    pub type Arc<T> = std::sync::Arc<T>;
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
    let (uplink_tx, uplink_rx) = std_mpsc::sync_channel(LINK_QUEUE_DEPTH);
    let (downlink_tx, downlink_rx) = std_mpsc::sync_channel(LINK_QUEUE_DEPTH);
    let (meta_tx, meta_rx) = std_mpsc::sync_channel(LINK_QUEUE_DEPTH);
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    (
        HostLink {
            uplink: uplink_rx,
            downlink: downlink_tx,
            meta: meta_rx,
            connected: std::sync::Arc::clone(&connected),
        },
        RemoteLink {
            downlink: downlink_rx,
            uplink: uplink_tx,
            meta_tx,
            connected,
        },
    )
}

/// The handshake a dialling peer sends before any combat traffic.
///
/// iroh authenticates the dialler's `NodeId` at the transport layer, so the
/// handshake carries no key material — what it establishes is protocol fit and
/// build provenance. Slice 3 extends this with invite-bound session identity
/// and version pinning (#345 §8); the fields reserved now keep that extension
/// from becoming a grammar change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    /// Build revision of the joining process, for the report and for pinning.
    pub client_rev: String,
}

impl JoinRequest {
    const MAGIC: [u8; 4] = *b"ORRX";
    const VERSION: u16 = 1;

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
        Ok(Self {
            client_rev: String::from_utf8_lossy(&rest[..rev_len]).into_owned(),
        })
    }
}

/// The host's answer to a [`JoinRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinReply {
    /// The sender joins at this swarm index; its `PersistId` derives from the
    /// index exactly as a bot's does.
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
    /// Encodes the reply onto the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Accept { index } => {
                out.push(0);
                let index = u64::try_from(*index).unwrap_or(u64::MAX);
                out.extend_from_slice(&index.to_le_bytes());
            }
            Self::Reject { reason } => {
                out.push(1);
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
        let request = JoinRequest {
            client_rev: "0ce6b28b".to_owned(),
        };
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

    #[test]
    fn join_replies_round_trip_both_arms() {
        let accept = JoinReply::Accept { index: 32 };
        assert_eq!(JoinReply::decode(&accept.encode()), Ok(accept.clone()));

        let reject = JoinReply::Reject {
            reason: "client rev pinned out".to_owned(),
        };
        assert_eq!(JoinReply::decode(&reject.encode()), Ok(reject.clone()));

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
