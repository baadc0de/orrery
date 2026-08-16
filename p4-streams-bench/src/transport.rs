//! The four ways to carry control traffic, and the run loop that exercises them.
//!
//! # The candidates
//!
//! | name | sparse control | repairs |
//! |---|---|---|
//! | [`Transport::Datagram`] | one datagram, unreliable | chunked to the MTU, one chunk per round trip |
//! | [`Transport::Shared`] | shared stream | shared stream |
//! | [`Transport::Bulk`] | own stream each | own stream each |
//! | [`Transport::Split`] | shared stream | own stream each |
//!
//! `Datagram` is the status quo, reproduced rather than modelled: it is what
//! `orrery_witness` did when `Channel::Control` was a datagram with a different
//! first byte. `serve_range` fitted what it could into one packet and named a
//! resume point, the requester asked again, and a 180-tick window therefore
//! cost a round trip per 1200 bytes. It is here to size the win, not as a
//! contender.
//!
//! `Split` is the hypothesis: sparse ordered traffic wants one stream (it is
//! cheap and ordering is meaningful), and a 40 kB repair wants to be out of
//! that stream's way (its retransmissions would otherwise hold up a lease op
//! that has nothing to do with it). `Shared` and `Bulk` bracket it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};

use orrery_net::channels::Channel;
use orrery_net::peer_link::payload_budget;
use orrery_net::{SendPacket, StreamMode};
use orrery_protocol::NodeId;

use crate::workload::{self, Class, Received, Samples};

/// Which transport the control lane uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Transport {
    /// The status quo: control as MTU-capped datagrams, repairs chunked with a
    /// resume point and a round trip per chunk.
    Datagram,
    /// Everything on the session's one shared stream.
    Shared,
    /// Every message on a stream of its own.
    Bulk,
    /// Sparse control shared, repairs on their own streams.
    Split,
}

impl Transport {
    /// How this transport prints in a report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Datagram => "datagram",
            Self::Shared => "shared",
            Self::Bulk => "bulk",
            Self::Split => "split",
        }
    }

    /// Which stream a message of `class` takes, or `None` if this transport
    /// does not use the stream lane at all.
    #[must_use]
    const fn stream_for(self, class: Class) -> Option<StreamMode> {
        match (self, class) {
            (Self::Datagram, _) => None,
            (Self::Shared, _) => Some(StreamMode::Shared),
            (Self::Bulk, _) => Some(StreamMode::Bulk),
            (Self::Split, Class::Repair) => Some(StreamMode::Bulk),
            (Self::Split, _) => Some(StreamMode::Shared),
        }
    }
}

/// The MTU the chunked baseline sizes against, matching `orrery_witness`.
const ASSUMED_MTU: usize = 1_200;
/// `[kind][seq][offset][total]`.
const CHUNK_HEADER: usize = 1 + 4 + 4 + 4;
/// Payload bytes per chunk on the datagram baseline.
const CHUNK_BYTES: usize = payload_budget(ASSUMED_MTU) - CHUNK_HEADER;
/// How long a requester waits before re-asking for a chunk that never came.
///
/// The status quo has no retransmission, so a lost chunk stalls the whole
/// repair until the requester notices. `orrery_witness` backs off over several
/// attempts; this is the first interval, which is the generous end.
const CHUNK_RETRY: Duration = Duration::from_millis(200);

/// Chunk-framing discriminants, chosen not to collide with [`Class::tag`].
///
/// They share a lane with whole messages, so a state datagram whose first byte
/// happened to be a chunk kind would be reassembled instead of decoded — which
/// is exactly what a first version of this did, and it reported zero
/// completions for every class on the baseline.
const KIND_CHUNK: u8 = 0xC0;
/// See [`KIND_CHUNK`].
const KIND_REQUEST: u8 = 0xC1;

/// What one transport cost, over one run.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Completion latencies, per class.
    pub samples: HashMap<&'static str, Samples>,
    /// Messages the sender started, per class.
    pub sent: HashMap<&'static str, u64>,
    /// Packets offered to the link, delivered or not.
    pub link_packets: u64,
    /// Bytes offered to the link.
    pub link_bytes: u64,
    /// Chunk re-requests the datagram baseline had to make.
    pub chunk_retries: u64,
}

impl Outcome {
    /// The share of started messages that completed, per class.
    #[must_use]
    pub fn completion(&self, class: Class) -> f64 {
        let sent = self.sent.get(class.name()).copied().unwrap_or(0);
        if sent == 0 {
            return 0.0;
        }
        let done = self
            .samples
            .get(class.name())
            .map_or(0, workload::Samples::count) as f64;
        done / sent as f64
    }
}

/// The sender's per-run state.
#[derive(Default)]
pub struct Sender {
    next_seq: HashMap<&'static str, u32>,
    /// Repairs the datagram baseline has started but not finished serving.
    serving: HashMap<u32, Bytes>,
}

/// The receiver's per-run state.
#[derive(Default)]
pub struct Receiver {
    /// Partially reassembled repairs, keyed by sequence.
    partial: HashMap<u32, Partial>,
}

struct Partial {
    buf: Vec<u8>,
    /// The first byte not yet received. Chunks arrive in order on this path,
    /// because the requester asks for one at a time.
    filled: usize,
    /// When the outstanding request was last sent.
    asked_at: Instant,
}

impl Sender {
    /// Emit one message of `class`, however this transport carries it.
    #[allow(clippy::too_many_arguments)] // One send's dependencies, explicit.
    pub fn emit(
        &mut self,
        transport: Transport,
        class: Class,
        to: NodeId,
        now: Instant,
        origin: Instant,
        out: &mut Vec<SendPacket>,
        outcome: &mut Outcome,
    ) {
        let seq = self.next_seq.entry(class.name()).or_insert(0);
        let payload = workload::encode(class, *seq, now, origin, bytes_for(class));
        let this_seq = *seq;
        *seq += 1;
        *outcome.sent.entry(class.name()).or_insert(0) += 1;

        // State is datagrams under every transport — that is the design, not a
        // variable. Only control is under test.
        if class == Class::State {
            out.push(SendPacket::state(to, payload));
            return;
        }

        match transport.stream_for(class) {
            Some(mode) => out.push(SendPacket {
                to,
                channel: Channel::Control,
                payload,
                mode,
            }),
            None if class == Class::Repair => {
                // The status quo: serve what fits, and wait to be asked again.
                self.serving.insert(this_seq, payload.clone());
                out.push(SendPacket::state(
                    to,
                    chunk(this_seq, 0, payload.len(), &payload),
                ));
            }
            None => {
                // Sparse control fitted one datagram already, so the status quo
                // sent it as one — unreliably, which is the defect.
                out.push(SendPacket::state(to, payload));
            }
        }
    }

    /// Answer a chunk request from the requester.
    pub fn serve_request(&self, seq: u32, offset: usize, to: NodeId, out: &mut Vec<SendPacket>) {
        let Some(payload) = self.serving.get(&seq) else {
            return;
        };
        if offset >= payload.len() {
            return;
        }
        out.push(SendPacket::state(
            to,
            chunk(seq, offset, payload.len(), payload),
        ));
    }
}

/// Build one `[KIND_CHUNK][seq][offset][total][data]` datagram.
fn chunk(seq: u32, offset: usize, total: usize, payload: &Bytes) -> Bytes {
    let end = (offset + CHUNK_BYTES).min(payload.len());
    let mut buf = BytesMut::with_capacity(CHUNK_HEADER + (end - offset));
    buf.put_u8(KIND_CHUNK);
    buf.put_u32_le(seq);
    buf.put_u32_le(offset as u32);
    buf.put_u32_le(total as u32);
    buf.put_slice(&payload[offset..end]);
    buf.freeze()
}

/// Build one `[KIND_REQUEST][seq][offset]` datagram.
fn request(seq: u32, offset: usize) -> Bytes {
    let mut buf = BytesMut::with_capacity(1 + 4 + 4);
    buf.put_u8(KIND_REQUEST);
    buf.put_u32_le(seq);
    buf.put_u32_le(offset as u32);
    buf.freeze()
}

/// What an inbound payload turned out to be.
pub enum Inbound {
    /// A whole message completed.
    Complete(Received),
    /// A chunk request, to be served.
    Request { seq: u32, offset: usize },
    /// Absorbed — a chunk that did not complete a message, or noise.
    Partial,
}

impl Receiver {
    /// Take one inbound payload.
    ///
    /// On the stream transports a payload *is* a message; on the datagram
    /// baseline it may be a chunk, a request, or a whole sparse message, so
    /// this has to tell them apart.
    #[allow(clippy::too_many_arguments)] // One receive's dependencies, explicit.
    pub fn accept(
        &mut self,
        transport: Transport,
        payload: &[u8],
        now: Instant,
        origin: Instant,
        to: NodeId,
        out: &mut Vec<SendPacket>,
        outcome: &mut Outcome,
    ) -> Inbound {
        if transport != Transport::Datagram {
            return workload::decode(payload, origin, now)
                .map_or(Inbound::Partial, Inbound::Complete);
        }

        match payload.first().copied() {
            Some(KIND_REQUEST) if payload.len() >= 9 => {
                let seq = u32::from_le_bytes(payload[1..5].try_into().unwrap_or_default());
                let offset = u32::from_le_bytes(payload[5..9].try_into().unwrap_or_default());
                Inbound::Request {
                    seq,
                    offset: offset as usize,
                }
            }
            Some(KIND_CHUNK) if payload.len() >= CHUNK_HEADER => {
                self.accept_chunk(payload, now, origin, to, out, outcome)
            }
            // Not chunked: a sparse control message, or a state datagram.
            _ => workload::decode(payload, origin, now).map_or(Inbound::Partial, Inbound::Complete),
        }
    }

    fn accept_chunk(
        &mut self,
        payload: &[u8],
        now: Instant,
        origin: Instant,
        to: NodeId,
        out: &mut Vec<SendPacket>,
        outcome: &mut Outcome,
    ) -> Inbound {
        let seq = u32::from_le_bytes(payload[1..5].try_into().unwrap_or_default());
        let offset = u32::from_le_bytes(payload[5..9].try_into().unwrap_or_default()) as usize;
        let total = u32::from_le_bytes(payload[9..13].try_into().unwrap_or_default()) as usize;
        let data = &payload[CHUNK_HEADER..];

        let partial = self.partial.entry(seq).or_insert_with(|| Partial {
            buf: vec![0u8; total],
            filled: 0,
            asked_at: now,
        });
        if offset != partial.filled || offset + data.len() > partial.buf.len() {
            // Out of order or out of range. The status quo asks for one chunk
            // at a time, so this only happens on a duplicate — ignore it rather
            // than corrupting the reassembly.
            return Inbound::Partial;
        }
        partial.buf[offset..offset + data.len()].copy_from_slice(data);
        partial.filled += data.len();
        partial.asked_at = now;

        if partial.filled >= partial.buf.len() {
            let done = self.partial.remove(&seq).map(|partial| partial.buf);
            return done
                .and_then(|buf| workload::decode(&buf, origin, now))
                .map_or(Inbound::Partial, Inbound::Complete);
        }
        let _ = outcome;
        out.push(SendPacket::state(to, request(seq, partial.filled)));
        Inbound::Partial
    }

    /// Re-ask for anything that has gone quiet.
    ///
    /// The datagram baseline has no retransmission: a lost chunk, or a lost
    /// request, stalls the whole repair until the requester notices. This is
    /// that noticing, and it is what makes twenty round trips into rather more
    /// than twenty under loss.
    pub fn retry_stalled(
        &mut self,
        now: Instant,
        to: NodeId,
        out: &mut Vec<SendPacket>,
        outcome: &mut Outcome,
    ) {
        for (seq, partial) in &mut self.partial {
            if now.saturating_duration_since(partial.asked_at) < CHUNK_RETRY {
                continue;
            }
            partial.asked_at = now;
            outcome.chunk_retries += 1;
            out.push(SendPacket::state(to, request(*seq, partial.filled)));
        }
    }
}

/// Payload size for a class.
const fn bytes_for(class: Class) -> usize {
    match class {
        Class::State => workload::STATE_BYTES,
        Class::Sparse => workload::SPARSE_BYTES,
        Class::Repair => workload::REPAIR_BYTES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_is_the_only_transport_that_treats_the_two_classes_differently() {
        // If it did not, it would not be a third option — it would be a
        // relabelling of one of the two it sits between.
        for transport in [Transport::Shared, Transport::Bulk] {
            assert_eq!(
                transport.stream_for(Class::Sparse),
                transport.stream_for(Class::Repair)
            );
        }
        assert_ne!(
            Transport::Split.stream_for(Class::Sparse),
            Transport::Split.stream_for(Class::Repair)
        );
    }

    #[test]
    fn a_forty_kilobyte_repair_is_about_thirty_four_chunks() {
        // The round-trip count the status quo pays, stated as arithmetic rather
        // than as a recollection.
        let chunks = workload::REPAIR_BYTES.div_ceil(CHUNK_BYTES);
        assert!(
            (30..=40).contains(&chunks),
            "expected ~34 chunks, got {chunks}"
        );
    }

    #[test]
    fn a_chunk_carries_enough_to_reassemble_from() {
        let payload = Bytes::from(vec![7u8; 5_000]);
        let framed = chunk(3, CHUNK_BYTES, payload.len(), &payload);
        assert_eq!(framed[0], KIND_CHUNK);
        assert_eq!(
            u32::from_le_bytes(framed[5..9].try_into().unwrap()) as usize,
            CHUNK_BYTES
        );
        assert_eq!(
            u32::from_le_bytes(framed[9..13].try_into().unwrap()) as usize,
            payload.len()
        );
        assert!(framed.len() <= payload_budget(ASSUMED_MTU));
    }
}
