//! Channel policy + wire framing shared by the gateway client and server (D3,
//! docs/10-crates.md §Dependency spine).
//!
//! The gateway boundary is split into two logical channels: **state** (bulk
//! diffs) over unreliable datagrams and **control** (area load, intents, hello)
//! over what the client treats as reliable streams (D3: datagrams = state,
//! streams = control/bulk). This module owns the one-byte tag that prefixes
//! every datagram so the receiver can route it without a separate framing
//! layer, plus the encode/decode helpers both sides use — so `orrery_persistd`
//! (Bevy-free) and `orrery_persist_client` (Bevy) share **one** wire surface,
//! not two drifted copies.
//!
//! Frame layouts (each carried inside one iroh datagram):
//! - **state**: `[TAG_STATE (0)] [ postcard ]`
//! - **stream/control**: `[TAG_CONTROL (1)] [ u32 LE length ] [ postcard ]`

use serde::de::DeserializeOwned;
use serde::Serialize;

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

/// The one-byte tag prefixing every datagram so the receiver can route it to
/// the right channel. `State` is the default (most traffic).
pub const TAG_STATE: u8 = 0;
/// Tag for control datagrams (rare; most control rides streams).
pub const TAG_CONTROL: u8 = 1;

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
/// framing layer. Both directions share this encoding.
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
}
