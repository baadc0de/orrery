//! Channel policy + wire framing shared by the gateway client and server (D3,
//! docs/10-crates.md §Dependency spine).
//!
//! The gateway boundary is split into two logical channels: **state** (bulk
//! diffs) over unreliable datagrams and **control** (area load, intents, hello)
//! over reliable QUIC streams (D3: datagrams = state, streams = control/bulk).
//! This module owns the one-byte tag that prefixes every payload so the
//! receiver can route it without a separate framing layer, plus the
//! encode/decode helpers both sides use — so `orrery_persistd` (Bevy-free) and
//! `orrery_persist_client` (Bevy) share **one** wire surface, not two drifted
//! copies.
//!
//! Payload layouts:
//! - **state**: `[TAG_STATE (0)] [ postcard ]`, one iroh datagram each
//! - **control**: `[TAG_CONTROL (1)] [ u32 LE length ] [ postcard ]`, one
//!   stream-lane message each
//!
//! # Why the control payload keeps its own length prefix
//!
//! The stream lane already delimits messages — the transport writes
//! `[u32 LE length][payload]` and hands the reader whole payloads — so the
//! inner prefix is, on that lane, redundant. It stays for two reasons. One
//! decoder then serves both lanes, which matters because the *receiving* side
//! of both the gateway and the client still accepts a control payload that
//! arrives as a datagram; and the tag is what tells a receiver which of the two
//! kinds it is holding regardless of how it arrived. Five bytes on a lane whose
//! messages are pages and intents is not a trade worth making twice.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::PersistId;

/// The two logical channels the design defines (docs/02-networking.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Unreliable state replication: per-tick entity state, bulk diffs. Carried
    /// by unhealthy datagrams.
    State,
    /// Reliable control and bulk transfer: connection handshakes, area load,
    /// intents. Carried by reliable streams.
    Control,
}

impl Channel {
    /// Whether this channel is carried by unreliable datagrams.
    #[must_use]
    pub const fn is_datagram(self) -> bool {
        matches!(self, Self::State)
    }

    /// Whether this channel is carried by reliable streams.
    #[must_use]
    pub const fn is_stream(self) -> bool {
        matches!(self, Self::Control)
    }
}

/// The one-byte tag prefixing every payload so the receiver can route it to
/// the right channel. `State` is the default (most traffic).
pub const TAG_STATE: u8 = 0;
/// Tag for control payloads: area load, intents, hello, lease control.
pub const TAG_CONTROL: u8 = 1;

/// The largest control message either side will write to — or accept from —
/// the reliable stream lane, in bytes.
///
/// A stream message is length-prefixed, and the length is chosen by the
/// *sender*, which on the receiving side means it is attacker-chosen. Both
/// readers therefore compare the prefix against this cap **before** reserving
/// a buffer for it, so a peer cannot name a gigabyte and have one allocated.
///
/// This must equal the transport's own cap — `aeronet_iroh`'s
/// `MAX_STREAM_MESSAGE_LEN`, re-exported as
/// `orrery_net::peer_link::MAX_STREAM_MESSAGE_LEN` — because the Bevy client
/// rides that implementation while `orrery_persistd` (Bevy-free, D15) speaks
/// raw iroh and cannot link it. A drift between the two would not fail loudly:
/// the larger side would emit messages the smaller side refuses, and the loss
/// would surface as a missing reply. `orrery_persist_client` links both and
/// asserts they agree.
pub const MAX_RELIABLE_MESSAGE_BYTES: usize = 1024 * 1024;

/// Tag a datagram payload with its channel. Returns a new `Vec` with the tag
/// prepended.
#[must_use]
pub fn tag(channel: Channel, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(match channel {
        Channel::State => TAG_STATE,
        Channel::Control => TAG_CONTROL,
    });
    out.extend_from_slice(payload);
    out
}

/// Strip a channel tag from a received datagram, returning the channel and the
/// payload (excluding the tag byte). Returns `None` if the payload is empty.
pub fn untag(payload: &[u8]) -> Option<(Channel, &[u8])> {
    let (&tag, rest) = payload.split_first()?;
    let channel = match tag {
        TAG_STATE => Channel::State,
        TAG_CONTROL => Channel::Control,
        _ => return None,
    };
    Some((channel, rest))
}

/// Sub-tag marking a state datagram as replication traffic.
///
/// Replication and witness traffic share `Channel::State`, so both have to be
/// positively identified: tagging only one leaves the other as "everything
/// else", and a receiver still hands foreign bytes to a decoder that reads
/// length prefixes out of them.
pub const TAG_REPLICATION: u8 = 0xE6;

/// Encode replication traffic as a state datagram.
pub fn encode_replication<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(TAG_REPLICATION);
    payload.extend_from_slice(&postcard::to_stdvec(msg).expect("wire message is serializable"));
    tag(Channel::State, &payload)
}

/// Decode replication traffic from a state datagram.
pub fn decode_replication<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    decode_sub_tagged(payload, TAG_REPLICATION)
}

/// Shared body of the sub-tagged state decoders.
fn decode_sub_tagged<T: DeserializeOwned>(payload: &[u8], expect: u8) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State {
        return None;
    }
    let (marker, body) = rest.split_first()?;
    if *marker != expect {
        return None;
    }
    postcard::from_bytes(body).ok()
}

/// Sub-tag marking a state datagram as verifiable-core traffic.
///
/// Replication payloads and witness records share `Channel::State` — docs/03
/// §5.3 has log records riding *in the same datagrams* at low priority — so the
/// channel tag alone cannot say which is which. Without a discriminator every
/// receiver attempts to parse every replication datagram as a `LogFrame`, and
/// postcard reads a length prefix out of unrelated bytes before it can fail:
/// slow at best, and an allocation the sender chooses at worst.
///
/// Its state-lane sibling is [`TAG_REPLICATION`]: both kinds are tagged, so a
/// receiver never hands foreign bytes to a decoder that would read a length
/// prefix out of them. Reliable delivered inputs have their own control-lane
/// [`TAG_DELIVERED_INPUT`] discriminator.
pub const TAG_WITNESS: u8 = 0xE7;

/// Sub-tag marking a reliable, addressed core input produced by
/// `Game::deliver` from another authority's outcome.
///
/// The payload is `[TAG_DELIVERED_INPUT][from u64 LE][recipient u64 LE]
/// [canonical input]`. The input bytes belong to the negotiated ruleset; this
/// envelope owns only routing/provenance and deliberately does not invent a
/// second command schema.
pub const TAG_DELIVERED_INPUT: u8 = 0xE8;

/// One delivered core input addressed to the authority of `recipient`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredInput {
    /// Entity whose authoritative outcome produced the input.
    pub from: PersistId,
    /// Entity whose authority may apply the input.
    pub recipient: PersistId,
    /// The ruleset's canonical `CoreInput` bytes.
    pub input: Vec<u8>,
}

/// Encode one addressed delivered input on the reliable control channel.
///
/// Cross-entity effects are canonical inputs, not replication snapshots. They
/// therefore use the reliable shared stream and retain emission/arrival order.
#[must_use]
pub fn encode_delivered_input(from: PersistId, recipient: PersistId, input: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(17 + input.len());
    payload.push(TAG_DELIVERED_INPUT);
    payload.extend_from_slice(&from.0.to_le_bytes());
    payload.extend_from_slice(&recipient.0.to_le_bytes());
    payload.extend_from_slice(input);
    tag(Channel::Control, &payload)
}

/// Decode one addressed delivered input, refusing every other channel member.
#[must_use]
pub fn decode_delivered_input(payload: &[u8]) -> Option<DeliveredInput> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::Control || rest.first() != Some(&TAG_DELIVERED_INPUT) {
        return None;
    }
    let from = rest.get(1..9)?;
    let recipient = rest.get(9..17)?;
    let input = rest.get(17..)?;
    Some(DeliveredInput {
        from: PersistId::new(u64::from_le_bytes(from.try_into().ok()?)),
        recipient: PersistId::new(u64::from_le_bytes(recipient.try_into().ok()?)),
        input: input.to_vec(),
    })
}

/// Encode verifiable-core traffic as a state datagram.
pub fn encode_witness<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(TAG_WITNESS);
    payload.extend_from_slice(&postcard::to_stdvec(msg).expect("wire message is serializable"));
    tag(Channel::State, &payload)
}

/// Decode verifiable-core traffic from a state datagram.
///
/// Returns `None` for anything not carrying [`TAG_WITNESS`], *before* handing
/// bytes to postcard — which is the point.
pub fn decode_witness<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    decode_sub_tagged(payload, TAG_WITNESS)
}

/// Encode a message as a **state** datagram: `[TAG_STATE][postcard]`.
///
/// Used for bulk diffs and their acks (D11 §2.1). Both directions share this
/// encoding.
///
/// # Panics
///
/// Panics if `msg` is not postcard-serializable.
#[must_use]
pub fn encode_datagram<T: Serialize>(msg: &T) -> Vec<u8> {
    let payload = postcard::to_stdvec(msg).expect("wire message is serializable");
    tag(Channel::State, &payload)
}

/// Decode a **state** datagram into `T`.
///
/// Returns `None` if the tag is not the state channel or the payload does not
/// decode.
pub fn decode_datagram<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State {
        return None;
    }
    postcard::from_bytes(rest).ok()
}

/// Encode a message as a **stream/control** frame: `[TAG_CONTROL][u32 LE
/// length][postcard]`.
///
/// Reliable-stream traffic is length-prefixed so one channel can carry many
/// messages, and tagged so the receiver can route it without a separate
/// framing layer. Both directions share this encoding. See the [module
/// docs](self#why-the-control-payload-keeps-its-own-length-prefix) for why the
/// prefix survives the move onto a lane that already frames.
///
/// # Panics
///
/// Panics if `msg` is not postcard-serializable.
#[must_use]
pub fn encode_stream_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let payload = postcard::to_stdvec(msg).expect("wire message is serializable");
    let len = u32::try_from(payload.len()).expect("stream frame fits in u32");
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(TAG_CONTROL);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode a **stream/control** frame into `T`.
///
/// Returns `None` if the tag is not the control channel, the frame is
/// malformed, or the payload does not decode.
pub fn decode_stream_frame<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::Control {
        return None;
    }
    let len = usize::try_from(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
    let frame = rest.get(4..4 + len)?;
    postcard::from_bytes(frame).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_untag_roundtrip() {
        let payload = b"state-bytes";
        let tagged = tag(Channel::State, payload);
        assert_eq!(untag(&tagged), Some((Channel::State, payload.as_slice())));
        let tagged = tag(Channel::Control, payload);
        assert_eq!(untag(&tagged), Some((Channel::Control, payload.as_slice())));
        assert_eq!(untag(&[]), None);
    }

    #[test]
    fn datagram_frame_roundtrips() {
        let msg = crate::GatewayMsg::Hello {
            token: b"tok".to_vec(),
            node: crate::NodeId::from_bytes(&[3u8; 32]).unwrap(),
        };
        let bytes = encode_datagram(&msg);
        assert_eq!(bytes[0], TAG_STATE);
        let back: crate::GatewayMsg = decode_datagram(&bytes).unwrap();
        assert_eq!(back, msg);
        // A control frame does not decode as a datagram.
        assert!(decode_datagram::<crate::GatewayMsg>(&encode_stream_frame(&msg)).is_none());
    }

    #[test]
    fn stream_frame_is_length_prefixed_and_tagged() {
        let msg = crate::GatewayReply::HelloAck {
            gateway: crate::NodeId::from_bytes(&[3u8; 32]).unwrap(),
            protocol: 1,
        };
        let frame = encode_stream_frame(&msg);
        assert_eq!(frame[0], TAG_CONTROL);
        let len = u32::from_le_bytes(frame[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 5);
        let back: crate::GatewayReply = decode_stream_frame(&frame).unwrap();
        assert_eq!(back, msg);
        // A datagram does not decode as a stream frame.
        assert!(decode_stream_frame::<crate::GatewayReply>(&encode_datagram(&msg)).is_none());
    }

    #[test]
    fn delivered_input_roundtrips_and_is_not_replication() {
        let delivered =
            encode_delivered_input(PersistId::new(7), PersistId::new(42), b"canonical order");
        assert_eq!(
            decode_delivered_input(&delivered),
            Some(DeliveredInput {
                from: PersistId::new(7),
                recipient: PersistId::new(42),
                input: b"canonical order".to_vec(),
            })
        );
        assert!(decode_replication::<Vec<u8>>(&delivered).is_none());
        assert!(decode_delivered_input(&encode_datagram(&42u64)).is_none());
    }
}
