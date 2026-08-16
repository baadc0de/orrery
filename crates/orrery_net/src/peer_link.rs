//! The peer packet lane (D3).
//!
//! Turns the IO layer's per-session byte buffers into addressed Bevy messages
//! and back, so systems that have something to say to a peer name the peer
//! rather than hunting for its session entity.
//!
//! # Why this is untyped
//!
//! Payloads are [`Bytes`], not a decoded message. Replication, authority, and
//! the witness all send over the same two lanes with different payload types,
//! and a lane that knew about any of them would have to know about all of them
//! — pulling every consumer's types into the session crate. Decoding belongs to
//! whoever owns the message; this owns only the routing and the channel policy.
//!
//! # MTU
//!
//! The IO layer sends datagrams, so a payload longer than the session's MTU is
//! dropped rather than fragmented. [`SendPacket`] is refused and counted at
//! that boundary instead of failing silently — a caller that quietly lost every
//! oversized packet would look like a lossy link and be repaired forever.

use bevy_ecs::prelude::*;
use bytes::Bytes;

use crate::channels::{tag, untag, Channel};
use crate::plugin::Peer;
use orrery_protocol::NodeId;

/// A packet that arrived from a peer.
#[derive(Debug, Clone, Message)]
pub struct PeerPacket {
    /// Who sent it.
    pub from: NodeId,
    /// Which lane it came in on.
    pub channel: Channel,
    /// The untagged payload.
    pub payload: Bytes,
}

/// A packet to send to a peer.
///
/// Dropped if that peer has no session — this is a datagram lane, so there is
/// nowhere to queue it, and pretending otherwise would hide a peer that has
/// gone away behind an ever-growing backlog.
#[derive(Debug, Clone, Message)]
pub struct SendPacket {
    /// Who to send it to.
    pub to: NodeId,
    /// Which lane to send it on.
    pub channel: Channel,
    /// The payload; tagged on the way out.
    pub payload: Bytes,
}

/// What the lane has moved, and what it could not.
#[derive(Debug, Default, Clone, Copy, Resource)]
pub struct PeerLinkCounters {
    /// Packets handed to the IO layer.
    pub sent: u64,
    /// Packets delivered from the IO layer.
    pub received: u64,
    /// Sends addressed to a peer with no session.
    pub no_session: u64,
    /// Sends refused for exceeding the session MTU.
    pub oversized: u64,
    /// Inbound packets with no channel tag, or an unknown one.
    pub untagged: u64,
}

/// Drains each session's receive buffer into [`PeerPacket`] messages.
pub fn receive_peer_packets(
    mut sessions: Query<(&Peer, &mut aeronet_io::Session)>,
    mut packets: MessageWriter<PeerPacket>,
    mut counters: ResMut<PeerLinkCounters>,
) {
    for (peer, mut session) in &mut sessions {
        for received in session.recv.drain(..) {
            let Some((channel, payload)) = untag(&received.payload) else {
                counters.untagged += 1;
                continue;
            };
            counters.received += 1;
            packets.write(PeerPacket {
                from: peer.id,
                channel,
                payload: Bytes::copy_from_slice(payload),
            });
        }
    }
}

/// Routes [`SendPacket`] messages into the addressed session's send buffer.
pub fn send_peer_packets(
    mut outbound: MessageReader<SendPacket>,
    mut sessions: Query<(&Peer, &mut aeronet_io::Session)>,
    mut counters: ResMut<PeerLinkCounters>,
) {
    for packet in outbound.read() {
        let Some((_, mut session)) = sessions.iter_mut().find(|(peer, _)| peer.id == packet.to)
        else {
            counters.no_session += 1;
            continue;
        };
        let framed = tag(packet.channel, &packet.payload);
        if framed.len() > session.mtu() {
            counters.oversized += 1;
            continue;
        }
        counters.sent += 1;
        session.send.push(Bytes::from(framed));
    }
}

/// The largest payload that will fit one packet on a session of `mtu`.
///
/// Callers that build variable-size payloads — a range response serving as many
/// frames as fit — size against this rather than the raw MTU, because the
/// channel tag is added afterwards and would otherwise push the packet over.
#[must_use]
pub const fn payload_budget(mtu: usize) -> usize {
    mtu.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_leaves_room_for_the_channel_tag() {
        // `tag` prepends one byte. A caller that filled the raw MTU would build
        // a payload that is refused the moment it is framed.
        let payload = vec![0u8; payload_budget(64)];
        assert_eq!(tag(Channel::State, &payload).len(), 64);
    }

    #[test]
    fn a_tiny_mtu_yields_an_empty_budget_rather_than_underflowing() {
        assert_eq!(payload_budget(0), 0);
    }
}
