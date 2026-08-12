//! Channel policy (D3): datagrams = state, streams = control/bulk.
//!
//! The design routes state replication over unreliable datagrams and control/
//! bulk transfers over reliable streams, with no head-of-line blocking between
//! them. This module names that policy so the rest of the stack can tag packets
//! and pick a transport without hardcoding the mapping.

/// The two logical channels the design defines (docs/02-networking.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Unreliable state replication: per-tick entity state, interest-set
    /// updates. Rides datagrams.
    State,
    /// Reliable control and bulk transfer: connection handshakes, lease
    /// control, bulk entity spawns, file/terrain chunks. Rides streams.
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
/// payload. Returns `None` if the payload is empty (no tag).
pub fn untag(payload: &[u8]) -> Option<(Channel, &[u8])> {
    let (&tag, rest) = payload.split_first()?;
    let channel = match tag {
        TAG_STATE => Channel::State,
        TAG_CONTROL => Channel::Control,
        _ => return None,
    };
    Some((channel, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_roundtrip() {
        let payload = b"state-bytes";
        let tagged = tag(Channel::State, payload);
        assert_eq!(untag(&tagged), Some((Channel::State, payload.as_slice())));

        let tagged = tag(Channel::Control, payload);
        assert_eq!(untag(&tagged), Some((Channel::Control, payload.as_slice())));
    }

    #[test]
    fn empty_payload_untags_none() {
        assert_eq!(untag(&[]), None);
    }

    #[test]
    fn policy_matches_design() {
        assert!(Channel::State.is_datagram());
        assert!(!Channel::State.is_stream());
        assert!(Channel::Control.is_stream());
        assert!(!Channel::Control.is_datagram());
    }
}
