//! The peer packet lane (D3).
//!
//! Turns the IO layer's per-session buffers into addressed Bevy messages and
//! back, so systems that have something to say to a peer name the peer rather
//! than hunting for its session entity.
//!
//! # Two lanes, two primitives
//!
//! [`Channel::State`] rides RFC 9221 datagrams and [`Channel::Control`] rides
//! QUIC streams, on the same connection, with no head-of-line blocking between
//! them (docs/02-networking.md §7). That mapping is what the channel policy has
//! always *said*; until the stream lane landed in `aeronet_iroh` both lanes were
//! datagrams and `Channel::Control` bought routing rather than reliability.
//!
//! The difference is not only that a control packet now arrives. It is that a
//! control payload is no longer capped at the path MTU, which is what made gap
//! repair cost a round trip per 1200 bytes: a 180-tick window took ~20 exchanges
//! *per witness*, and more repair budget only put more of them in flight. See
//! [`control_payload_budget`].
//!
//! # Which stream a control packet takes
//!
//! [`SendPacket::mode`] chooses, per message, between the session's one shared
//! stream and a stream of its own — [`StreamMode`]. Sparse ordered traffic
//! belongs on the shared stream; a bulk transfer belongs out of its way. The
//! measured trade is recorded in `gates/p4-streams-bench`.
//!
//! # Why this is untyped
//!
//! Payloads are [`Bytes`], not a decoded message. Replication, authority, and
//! the witness all send over the same two lanes with different payload types,
//! and a lane that knew about any of them would have to know about all of them
//! — pulling every consumer's types into the session crate. Decoding belongs to
//! whoever owns the message; this owns only the routing and the channel policy.

use bevy_ecs::prelude::*;
use bevy_time::{Real, Time};
use bytes::Bytes;

use aeronet_iroh::stream::{IrohStreamIo, SendMessage};

use crate::budget::{
    batch_priority, datagram_wire_bytes, is_sheddable, lane_of, stream_wire_bytes, UploadBudget,
    UploadMeter,
};
use crate::channels::{tag, untag, Channel};
use crate::plugin::Peer;
use orrery_protocol::NodeId;

pub use aeronet_iroh::session::MAX_STREAM_MESSAGE_LEN;
pub use aeronet_iroh::stream::StreamMode;

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
/// Dropped if that peer has no session. On the state lane there is nowhere to
/// queue it and pretending otherwise would hide a departed peer behind an
/// ever-growing backlog; on the control lane the session *is* the queue, so a
/// peer without one has no queue to join either.
#[derive(Debug, Clone, Message)]
pub struct SendPacket {
    /// Who to send it to.
    pub to: NodeId,
    /// Which lane to send it on.
    pub channel: Channel,
    /// The payload; tagged on the way out.
    pub payload: Bytes,
    /// Which stream to take, on [`Channel::Control`]. Ignored on the state
    /// lane, which has no streams.
    pub mode: StreamMode,
}

impl SendPacket {
    /// A state packet: unreliable, MTU-bounded, shed first when over budget.
    #[must_use]
    pub const fn state(to: NodeId, payload: Bytes) -> Self {
        Self {
            to,
            channel: Channel::State,
            payload,
            mode: StreamMode::Shared,
        }
    }

    /// A control packet on the session's shared stream: reliable, ordered with
    /// every other shared-stream message.
    #[must_use]
    pub const fn control(to: NodeId, payload: Bytes) -> Self {
        Self {
            to,
            channel: Channel::Control,
            payload,
            mode: StreamMode::Shared,
        }
    }

    /// A control packet on a stream of its own: reliable, and unable to hold up
    /// anything else. For transfers large enough that their retransmissions
    /// would stall the sparse traffic sharing a stream with them.
    #[must_use]
    pub const fn bulk(to: NodeId, payload: Bytes) -> Self {
        Self {
            to,
            channel: Channel::Control,
            payload,
            mode: StreamMode::Bulk,
        }
    }
}

/// What the lane has moved, and what it could not.
#[derive(Debug, Default, Clone, Copy, Resource)]
pub struct PeerLinkCounters {
    /// Datagrams handed to the IO layer.
    pub sent: u64,
    /// Datagrams delivered from the IO layer.
    pub received: u64,
    /// Control messages handed to the stream lane.
    pub stream_sent: u64,
    /// Control messages delivered from the stream lane.
    pub stream_received: u64,
    /// Sends addressed to a peer with no session.
    pub no_session: u64,
    /// Sends refused for exceeding the lane's size limit.
    ///
    /// On the state lane that is the session MTU; on the control lane it is
    /// [`MAX_STREAM_MESSAGE_LEN`], which is roughly a thousand times larger.
    pub oversized: u64,
    /// Inbound packets with no channel tag, or an unknown one.
    pub untagged: u64,
}

/// Drains each session's receive buffers into [`PeerPacket`] messages.
///
/// Both lanes are drained here, and both keep their channel tag on the wire.
/// The tag is redundant with the lane a packet arrived on, and deliberately so:
/// `orrery_persistd`'s gateway speaks the same framing over raw iroh streams
/// without this crate's session machinery, and two encodings of one wire
/// surface is how they drift.
pub fn receive_peer_packets(
    mut sessions: Query<(&Peer, &mut aeronet_io::Session, Option<&mut IrohStreamIo>)>,
    mut packets: MessageWriter<PeerPacket>,
    mut counters: ResMut<PeerLinkCounters>,
) {
    for (peer, mut session, streams) in &mut sessions {
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

        let Some(mut streams) = streams else {
            continue;
        };
        for message in streams.recv.drain(..) {
            let Some((channel, payload)) = untag(&message.payload) else {
                counters.untagged += 1;
                continue;
            };
            counters.stream_received += 1;
            packets.write(PeerPacket {
                from: peer.id,
                channel,
                payload: Bytes::copy_from_slice(payload),
            });
        }
    }
}

/// Routes [`SendPacket`] messages onto the addressed session's lane, charging
/// each one against the peer upload budget (D6, D16).
///
/// # Shedding
///
/// Over budget, replication packets are dropped while control and witness
/// packets still go out — the lane is read off the wire by
/// [`crate::budget::lane_of`] and the order is [`crate::budget::is_sheddable`]'s.
/// This is the backstop, not the policy: docs/03-replication.md §9.3 sheds by
/// relevance class from the bottom via a priority accumulator, which is
/// `orrery_predict`'s job and reads [`UploadBudget`] rather than reimplementing
/// it, and §5.3a bounds the witness lane at source so it never reaches here.
/// The backstop exists because §4 is explicit that a sender enforces its own
/// budget regardless of what was requested of it, and that has to hold whether
/// or not an accumulator is wired up.
///
/// # Charging a stream message
///
/// A control message is charged [`stream_wire_bytes`] rather than a flat
/// per-datagram overhead, because it is cut into as many packets as its size
/// requires. A large repair therefore costs what it actually costs; the meter
/// does not get cheaper by moving lane.
pub fn send_peer_packets(
    mut outbound: MessageReader<SendPacket>,
    mut sessions: Query<(&Peer, &mut aeronet_io::Session, Option<&mut IrohStreamIo>)>,
    mut counters: ResMut<PeerLinkCounters>,
    budget: Res<UploadBudget>,
    mut meter: ResMut<UploadMeter>,
    time: Res<Time<Real>>,
) {
    let now = time.elapsed();
    let mut batch = outbound.read().collect::<Vec<_>>();
    // The backstop sees one update's messages together. Preserve arrival order
    // within a class, but make the loss asymmetry explicit across classes:
    // unsheddable traffic, then absolute replication anchors, then deltas.
    batch.sort_by_key(|packet| batch_priority(packet.channel, &packet.payload));

    for packet in batch {
        let Some((_, mut session, streams)) =
            sessions.iter_mut().find(|(peer, ..)| peer.id == packet.to)
        else {
            counters.no_session += 1;
            continue;
        };
        let framed = tag(packet.channel, &packet.payload);
        let mtu = session.mtu();
        let lane = lane_of(packet.channel, &packet.payload);

        let wire = match packet.channel {
            Channel::State => {
                if framed.len() > mtu {
                    counters.oversized += 1;
                    continue;
                }
                datagram_wire_bytes(framed.len())
            }
            Channel::Control => {
                if framed.len() as u64 > MAX_STREAM_MESSAGE_LEN {
                    // Refused at this boundary rather than by the IO layer,
                    // which would drop the session over it.
                    counters.oversized += 1;
                    continue;
                }
                stream_wire_bytes(framed.len(), mtu)
            }
        };

        if meter.would_exceed_wire(*budget, now, wire) {
            if is_sheddable(lane) {
                meter.shed += 1;
                // Wire bytes, matching what the meter charges — otherwise
                // "shed" and "sent" are in different units and the obvious
                // question (what fraction of the budget did I have to shed?)
                // has no correct answer.
                meter.shed_bytes += wire;
                meter.lanes.replication_shed += 1;
                continue;
            }
            // Control and witness go out anyway, and are charged anyway.
            // Counting without sending would understate the overrun; sending
            // without counting would hide one. See `is_sheddable` for why the
            // witness lane is on this side of the line.
            meter.unsheddable_over_budget += 1;
        }

        meter.charge(*budget, now, packet.to, lane, wire);
        match packet.channel {
            Channel::State => {
                counters.sent += 1;
                session.send.push(Bytes::from(framed));
            }
            Channel::Control => {
                let Some(mut streams) = streams else {
                    // A session with no stream lane cannot carry control. That
                    // is a wiring fault, not a network condition, so it is
                    // counted as an unaddressable send rather than silently
                    // demoted onto the datagram lane — where it would be
                    // sheddable, MTU-bounded, and exactly the thing this lane
                    // exists to stop being.
                    counters.no_session += 1;
                    continue;
                };
                counters.stream_sent += 1;
                streams.send.push(SendMessage {
                    payload: Bytes::from(framed),
                    mode: packet.mode,
                });
            }
        }
    }
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

/// The largest payload that will fit one **state** packet on a session of `mtu`.
///
/// Callers that build variable-size datagram payloads size against this rather
/// than the raw MTU, because the channel tag is added afterwards and would
/// otherwise push the packet over.
#[must_use]
pub const fn payload_budget(mtu: usize) -> usize {
    mtu.saturating_sub(1)
}

/// The largest payload that will fit one **control** message.
///
/// The reliable lane has no MTU — a message is cut into as many packets as it
/// needs and reassembled whole — so this is a memory bound, not a path bound,
/// and it is roughly a thousand times [`payload_budget`].
///
/// That ratio is the point. A caller that serves as much of a range as fits
/// (`AuthorityLog::serve_range`) sized against the MTU turned a 180-tick repair
/// into ~20 round trips per witness; sized against this, the same repair is one
/// exchange. The resume-from-here machinery stays — a range can still exceed
/// even this — but it stops being the common path.
#[must_use]
pub const fn control_payload_budget() -> usize {
    // Saturating rather than wrapping: on a 32-bit target the limit exceeds
    // `usize::MAX` and the honest answer is "as much as this machine can hold".
    match MAX_STREAM_MESSAGE_LEN.checked_sub(1) {
        Some(budget) if budget <= usize::MAX as u64 => budget as usize,
        _ => usize::MAX,
    }
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

    #[test]
    fn the_control_budget_dwarfs_the_datagram_one() {
        // The whole reason for moving repair onto the reliable lane: one
        // exchange instead of one per MTU.
        assert!(control_payload_budget() > payload_budget(1200) * 500);
        assert_eq!(
            tag(Channel::Control, &vec![0u8; control_payload_budget()]).len() as u64,
            MAX_STREAM_MESSAGE_LEN
        );
    }

    #[test]
    fn a_stream_message_is_charged_per_packet_it_occupies() {
        // A flat per-datagram overhead would understate a large message by the
        // framing of every packet after the first.
        let one_packet = stream_wire_bytes(100, 1200);
        assert_eq!(one_packet, 100 + 16 + 60);

        let ten_packets = stream_wire_bytes(12_000, 1200);
        assert!(
            ten_packets > 12_000 + 16 + 60 * 10,
            "a 12 kB message spans more than ten 1200 B packets once framed"
        );
    }
}
