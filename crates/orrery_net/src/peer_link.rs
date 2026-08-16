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
use bevy_time::{Real, Time};
use bytes::Bytes;

use crate::budget::{is_sheddable, UploadBudget, UploadMeter, DATAGRAM_OVERHEAD_BYTES};
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

/// Routes [`SendPacket`] messages into the addressed session's send buffer,
/// charging each one against the peer upload budget (D6, D16).
///
/// # Shedding
///
/// Over budget, [`Channel::State`] packets are dropped and [`Channel::Control`]
/// packets still go out — see [`crate::budget::is_sheddable`]. This is the
/// backstop, not the policy: docs/03-replication.md §9.3 sheds by relevance
/// class from the bottom via a priority accumulator, which is `orrery_predict`'s
/// job and reads [`UploadBudget`] rather than reimplementing it. The backstop
/// exists because §4 is explicit that a sender enforces its own budget
/// regardless of what was requested of it, and that has to hold whether or not
/// an accumulator is wired up.
pub fn send_peer_packets(
    mut outbound: MessageReader<SendPacket>,
    mut sessions: Query<(&Peer, &mut aeronet_io::Session)>,
    mut counters: ResMut<PeerLinkCounters>,
    budget: Res<UploadBudget>,
    mut meter: ResMut<UploadMeter>,
    time: Res<Time<Real>>,
) {
    let now = time.elapsed();
    let mut over = false;
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

        if meter.would_exceed(*budget, now, framed.len()) {
            over = true;
            if is_sheddable(packet.channel) {
                meter.shed += 1;
                // Wire bytes, matching what the meter charges — otherwise
                // "shed" and "sent" are in different units and the obvious
                // question (what fraction of the budget did I have to shed?)
                // has no correct answer.
                meter.shed_bytes += framed.len() as u64 + DATAGRAM_OVERHEAD_BYTES;
                continue;
            }
            // Control goes out anyway, and is charged anyway. Counting it
            // without sending it would understate the overrun; sending it
            // without counting it would hide one.
            meter.control_over_budget += 1;
        }

        meter.record(*budget, now, packet.to, framed.len());
        counters.sent += 1;
        session.send.push(Bytes::from(framed));
    }
    meter.oversubscribed = over;
}

/// Drops per-link meters for peers that no longer have a session.
///
/// Without this a long-lived peer accumulates a meter per NodeId it has ever
/// talked to, and the per-link division the accumulator reads is computed
/// against a link count that only grows.
pub fn forget_departed_links(
    mut meter: ResMut<UploadMeter>,
    sessions: Query<&Peer>,
    mut departed: Local<Vec<orrery_protocol::NodeId>>,
) {
    departed.clear();
    departed.extend(
        meter
            .links()
            .filter(|node| !sessions.iter().any(|peer| peer.id == *node)),
    );
    for node in departed.iter() {
        meter.forget(*node);
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
