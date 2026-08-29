//! The in-process peer lane, with injectable impairment.
//!
//! Moves bytes between peers' `aeronet_io::Session` buffers: it drains each
//! peer's `send` and appends to the addressed peer's `recv`, which is exactly
//! what the IO layer does. Everything above it — `send_peer_packets`, the
//! upload meter, the channel policy — is the shipping code, unmodified.
//!
//! # Why loss is a parameter and not a real link
//!
//! P4's criterion requires **3–5% packet loss and 100 ms jitter spikes**
//! sustained over hundreds of hours. Over a real link that is a netem setup
//! whose behaviour is itself under test; here it is a seeded number, so a run
//! that finds a false positive can be replayed exactly. The seeded RNG makes
//! impairment reproducible, which matters because the thing being measured is a
//! *rate* of false positives — an unreproducible one cannot be investigated.
//!
//! # Two lanes, because there are two lanes
//!
//! State rides datagrams and control rides QUIC streams (D3), and the two fail
//! differently. A lost datagram is gone. A lost stream segment is retransmitted
//! about a round trip later — and, on a *shared* stream, everything queued
//! behind it waits with it. Both are modelled here.
//!
//! It would be tempting to exempt control from impairment since it models a
//! reliable lane, but QUIC's reliability is retransmission, not magic:
//! modelling control as lossless would hide every place the code assumes a
//! repair arrives promptly. What it gets instead is delay proportional to loss,
//! which is what reliability actually costs.
//!
//! The head-of-line term is not a guess. `gates/p4-streams-bench` measures it over
//! real QUIC: at 3% loss and a 40 ms RTT with the link near saturation, sparse
//! control sharing a stream with 40 kB repairs went from a 54 ms median to
//! 1154 ms. That is the effect this model exists to reproduce cheaply.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use serde::Serialize;

use orrery_net::peer_link::StreamMode;
use orrery_protocol::NodeId;

/// Link conditions applied to every packet the router carries.
///
/// Serializable because a report that does not name the link it was measured
/// over cannot be compared against another one.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Impairment {
    /// Fraction of packets dropped, 0.0–1.0.
    pub loss: f64,
    /// Ticks a delayed packet is held for. Zero means no reordering.
    pub jitter_ticks: u32,
    /// Fraction of packets that take the jitter delay.
    pub jitter_rate: f64,
    /// Ticks a lost *stream* segment costs before its retransmission lands.
    ///
    /// Roughly one round trip. At 60 Hz sim ticks, three ticks is 50 ms — the
    /// same order as the 40 ms link `gates/p4-streams-bench` measures over.
    pub retransmit_ticks: u32,
}

impl Default for Impairment {
    fn default() -> Self {
        Self {
            loss: 0.0,
            jitter_ticks: 0,
            jitter_rate: 0.0,
            retransmit_ticks: 3,
        }
    }
}

impl Impairment {
    /// The P4 impairment profile: 3% loss with 100 ms jitter spikes.
    ///
    /// 100 ms at 60 Hz is six ticks — the delay a witness must absorb without
    /// deciding a chain has a hole that will never fill.
    ///
    /// 3% is the *floor* of the criterion's 3–5% band, and it is what the
    /// nightly gate runs. The other end is reachable through
    /// [`Self::p4_profile_at_loss`] rather than hardcoded, because a band that
    /// only ever gets exercised at one end is a band in name.
    #[must_use]
    pub fn p4_profile() -> Self {
        Self::p4_profile_at_loss(0.03)
    }

    /// The P4 profile at a chosen point in the criterion's 3–5% loss band.
    #[must_use]
    pub fn p4_profile_at_loss(loss: f64) -> Self {
        Self {
            loss,
            jitter_ticks: 6,
            jitter_rate: 0.10,
            retransmit_ticks: 3,
        }
    }

    /// Whether this profile perturbs anything.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.loss <= 0.0 && (self.jitter_ticks == 0 || self.jitter_rate <= 0.0)
    }
}

/// One packet in flight.
struct InFlight {
    to: usize,
    from: NodeId,
    payload: Bytes,
    /// Tick at which it may be delivered.
    due: u64,
    /// Which stream it took, if it is on the reliable lane.
    ///
    /// `None` is a datagram. The distinction survives into the delivery queue
    /// because the recipient has to put it back on the lane it came from.
    stream: Option<StreamMode>,
}

/// What the link did, for the report.
#[derive(Debug, Default, Clone, Copy)]
pub struct RouterCounters {
    /// Packets carried end to end.
    pub delivered: u64,
    /// Datagrams dropped by the loss model.
    pub dropped: u64,
    /// Stream messages carried end to end.
    pub stream_delivered: u64,
    /// Retransmissions a stream message needed before it landed.
    ///
    /// Not losses: every one of these still arrived. It is what reliability
    /// cost in round trips, which is the number the repair path lives or dies
    /// by.
    pub stream_retransmits: u64,
    /// Ticks stream messages spent waiting behind an earlier message on the
    /// same shared stream.
    ///
    /// The head-of-line tax, separated out so it can be read rather than
    /// inferred from a latency histogram the harness does not keep.
    pub stream_head_of_line_ticks: u64,
    /// Packets held back by the jitter model.
    pub delayed: u64,
    /// Packets addressed to a peer the router does not know.
    pub misaddressed: u64,
    /// Total wire bytes carried, including per-datagram overhead.
    pub bytes: u64,
}

/// One directed peer-to-peer link: who sent, and which seat receives.
///
/// Named rather than a bare `(NodeId, usize)` tuple because the two fields are
/// not interchangeable and a tuple key says nothing about which is which at the
/// use site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct LinkId {
    from: NodeId,
    to: usize,
}

/// Draw indices, so independent decisions about one packet never share a value.
const DRAW_LOSS: u64 = 0;
const DRAW_JITTER: u64 = 1;
const DRAW_SEGMENT_BASE: u64 = 16;
/// Retransmit attempts per segment, bounded by `MAX_RETRANSMITS`.
const MAX_DRAWS_PER_SEGMENT: u64 = 64;

/// The bucket an occurrence index is counted within.
///
/// Named rather than a `(LinkId, FateLane, u64)` tuple: the fields are not
/// interchangeable and a tuple key says nothing about which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FateSlot {
    link: LinkId,
    lane: FateLane,
    tick: u64,
}

/// The identity a packet's impairment fate is a function of.
///
/// Deliberately not a position in a sequence: `link` and `lane` say where it
/// travels, `tick` when it was offered, and `payload` what it carries. Adding
/// or removing other traffic changes none of those, so no other packet's fate
/// moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PacketFate {
    link: LinkId,
    lane: FateLane,
    tick: u64,
    payload: u64,
    /// Which identical packet this is, within one link, lane and tick.
    ///
    /// Without it, ten thousand identical payloads offered in one tick share a
    /// single identity and therefore a single fate -- all dropped or none, which
    /// makes the loss *rate* meaningless. This restores an order dependence,
    /// but a strictly bounded one: inserting a packet can only move the fate of
    /// packets in the *same tick* on the *same link and lane*. The failure this
    /// whole change exists to kill was unbounded — one inserted keyframe moved
    /// a shot by a retransmission interval and the divergence ran for 180,000
    /// ticks (#699).
    occurrence: u32,
}

/// Which lane a fate draw belongs to, so the two never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum FateLane {
    Datagram,
    Stream,
}

impl PacketFate {
    fn of(link: LinkId, lane: FateLane, tick: u64, payload: &[u8], occurrence: u32) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(payload, &mut hasher);
        Self {
            link,
            lane,
            tick,
            payload: std::hash::Hasher::finish(&hasher),
            occurrence,
        }
    }
}

/// Carries packets between peers' session buffers.
pub struct Router {
    /// Impairment is a pure function of packet identity, not a stream.
    ///
    /// The former single `ChaCha8Rng` drew loss and jitter in packet order
    /// across every link, so a change that altered one link shifted every
    /// other link's impairment realisation. Per-link streams remove that
    /// cross-link coupling (#700).
    ///
    /// They deliberately do not isolate traffic classes on the *same* link:
    /// state datagrams and control streams below both draw from this map entry.
    /// #699's swept-margin A/B exposed that remaining insertion coupling. An
    /// extra state audience on one directed link can shift a later control
    /// retransmission on that same link even though unrelated links remain
    /// byte-for-byte isolated. See `docs/spikes/swept-margin-audience.md`.
    ///
    /// Seeded from the run seed and the link identity, so a link draws the same
    /// sequence regardless of what any other link did.
    seed: u64,
    /// Per (link, lane, tick) offer counts, so identical packets differ.
    /// Pruned as ticks retire; it never holds more than one tick's worth.
    occurrences: BTreeMap<FateSlot, u32>,
    impairment: Impairment,
    in_flight: VecDeque<InFlight>,
    /// The tick the last `Shared` message on each link is due.
    ///
    /// One stream is one ordered byte sequence: a message cannot be delivered
    /// before the one in front of it, however lucky its own segments were. This
    /// is that ordering, and it is the whole difference between the two modes.
    shared_tail: Vec<(LinkId, u64)>,
    /// Counters, exposed for the report.
    pub counters: RouterCounters,
}

impl Router {
    /// A router with the given impairment, seeded for reproducibility.
    #[must_use]
    pub fn new(impairment: Impairment, seed: u64) -> Self {
        Self {
            seed,
            occurrences: BTreeMap::new(),
            impairment,
            in_flight: VecDeque::new(),
            shared_tail: Vec::new(),
            counters: RouterCounters::default(),
        }
    }

    /// The impairment stream for one directed link, created on first use.
    /// The identity of the next packet offered on this link, lane and tick.
    fn next_fate(&mut self, link: LinkId, lane: FateLane, tick: u64, payload: &[u8]) -> PacketFate {
        self.occurrences.retain(|slot, _| slot.tick >= tick);
        let slot = self
            .occurrences
            .entry(FateSlot { link, lane, tick })
            .or_insert(0);
        let occurrence = *slot;
        *slot = slot.saturating_add(1);
        PacketFate::of(link, lane, tick, payload, occurrence)
    }

    /// Whether one draw for one logical packet comes up inside `probability`.
    ///
    /// Stateless on purpose. A per-link *stream* is consumed in packet order,
    /// so inserting a packet shifts the fate of every packet behind it on that
    /// link -- which is how #692's swept margin came to fail an unrelated
    /// boundary-thrash clause (#699): eight extra keyframes moved a later
    /// `ShotResolved` by one retransmission interval, and the collision that
    /// followed was real. Keying the draw on what the packet *is* means adding
    /// traffic cannot move anyone else's fate.
    ///
    /// `draw` separates independent decisions about the same packet (loss from
    /// jitter, one segment's retransmits from another's), so they do not share
    /// a value.
    ///
    /// Known limit, stated rather than hidden: two byte-identical payloads on
    /// the same link and lane in the same tick are one identity here and share
    /// a fate. That is the price of order-independence, and it is the right
    /// trade -- a duplicate packet arguably *should* share the original's luck,
    /// whereas order-dependence silently corrupts every A/B comparison.
    fn draws(&self, packet: PacketFate, draw: u64, probability: f64) -> bool {
        if probability <= 0.0 {
            return false;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&(self.seed, packet, draw), &mut hasher);
        // Top 53 bits give a uniform f64 in [0, 1) without rounding surprises.
        let bits = std::hash::Hasher::finish(&hasher) >> 11;
        #[allow(clippy::cast_precision_loss)]
        let unit = bits as f64 / (1u64 << 53) as f64;
        unit < probability
    }

    /// Decide the fate of one packet: dropped, delayed, or delivered now.
    ///
    /// Separated from the buffer plumbing so the impairment model can be tested
    /// without standing up peers.
    fn schedule(&mut self, tick: u64, link: LinkId, payload: &[u8]) -> Fate {
        let impairment = self.impairment;
        let packet = self.next_fate(link, FateLane::Datagram, tick, payload);
        if self.draws(packet, DRAW_LOSS, impairment.loss) {
            return Fate::Dropped;
        }
        if impairment.jitter_ticks > 0 && self.draws(packet, DRAW_JITTER, impairment.jitter_rate) {
            return Fate::Delayed(tick + u64::from(self.impairment.jitter_ticks));
        }
        Fate::Now(tick)
    }

    /// Accept a reliable message from `from` addressed to peer index `to`.
    ///
    /// Reliable means it is never dropped — only delayed. Each lost segment
    /// costs a round trip, and a `Shared` message additionally waits for
    /// everything already queued on that stream. Both are what QUIC does, and
    /// the second is what `gates/p4-streams-bench` measures the price of.
    pub fn accept_stream(
        &mut self,
        tick: u64,
        from: NodeId,
        to: usize,
        mode: StreamMode,
        payload: Bytes,
    ) {
        self.counters.bytes += payload.len() as u64 + DATAGRAM_OVERHEAD;

        // Retransmit until it lands. A real stream retries a *segment*, so a
        // large message is more likely to need at least one — charge it per
        // MTU's worth rather than per message, or a 40 kB repair would look as
        // robust as a 120 B lease ack.
        let segments = (payload.len() as u64).div_ceil(MTU_BYTES).max(1);
        let mut due = tick + 1;
        let link = LinkId { from, to };
        let loss = self.impairment.loss;
        let jitter_rate = self.impairment.jitter_rate;
        let packet = self.next_fate(link, FateLane::Stream, tick, &payload);
        if loss > 0.0 {
            for segment in 0..segments {
                let mut attempts = 0u32;
                while self.draws(
                    packet,
                    DRAW_SEGMENT_BASE + segment * MAX_DRAWS_PER_SEGMENT + u64::from(attempts),
                    loss,
                ) && attempts < MAX_RETRANSMITS
                {
                    attempts += 1;
                }
                if attempts > 0 {
                    self.counters.stream_retransmits += u64::from(attempts);
                    // Retransmissions of different segments overlap, so the
                    // cost is the worst segment's, not the sum of all of them.
                    due = due.max(
                        tick + 1
                            + u64::from(attempts) * u64::from(self.impairment.retransmit_ticks),
                    );
                }
            }
        }
        if self.impairment.jitter_ticks > 0 && self.draws(packet, DRAW_JITTER, jitter_rate) {
            due += u64::from(self.impairment.jitter_ticks);
        }

        if mode == StreamMode::Shared {
            let key = link;
            let tail = self.shared_tail.iter_mut().find(|(held, _)| *held == key);
            let blocked_until = tail.as_ref().map_or(0, |(_, due)| *due);
            if blocked_until > due {
                self.counters.stream_head_of_line_ticks += blocked_until - due;
                due = blocked_until;
            }
            match tail {
                Some((_, held)) => *held = due,
                None => self.shared_tail.push((key, due)),
            }
        }

        self.in_flight.push_back(InFlight {
            to,
            from,
            payload,
            due,
            stream: Some(mode),
        });
    }

    /// Accept a datagram from `from` addressed to peer index `to`.
    pub fn accept(
        &mut self,
        tick: u64,
        from: NodeId,
        to: usize,
        payload: Bytes,
    ) -> DatagramDisposition {
        self.counters.bytes += payload.len() as u64 + DATAGRAM_OVERHEAD;
        match self.schedule(tick, LinkId { from, to }, &payload) {
            Fate::Dropped => {
                self.counters.dropped += 1;
                DatagramDisposition::Dropped
            }
            Fate::Delayed(due) => {
                self.counters.delayed += 1;
                self.in_flight.push_back(InFlight {
                    to,
                    from,
                    payload,
                    due,
                    stream: None,
                });
                DatagramDisposition::Delivered
            }
            Fate::Now(due) => {
                self.in_flight.push_back(InFlight {
                    to,
                    from,
                    payload,
                    due,
                    stream: None,
                });
                DatagramDisposition::Delivered
            }
        }
    }

    /// Every packet due at or before `tick`, as `(recipient, sender, lane, bytes)`.
    ///
    /// Drains in arrival order, so a delayed packet lands behind ones that
    /// overtook it — the reordering a jitter spike actually causes, which is
    /// what the chain-gap path has to absorb.
    pub fn deliver_due(&mut self, tick: u64) -> Vec<Delivery> {
        let mut out = Vec::new();
        let mut held = VecDeque::with_capacity(self.in_flight.len());
        while let Some(packet) = self.in_flight.pop_front() {
            if packet.due <= tick {
                if packet.stream.is_some() {
                    self.counters.stream_delivered += 1;
                } else {
                    self.counters.delivered += 1;
                }
                out.push(Delivery {
                    to: packet.to,
                    from: packet.from,
                    stream: packet.stream,
                    payload: packet.payload,
                });
            } else {
                held.push_back(packet);
            }
        }
        self.in_flight = held;
        out
    }

    /// Packets still in flight — a run must not end with the link holding work.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

/// One packet the router has carried to its recipient.
pub struct Delivery {
    /// Recipient bot index.
    pub to: usize,
    /// Who sent it.
    pub from: NodeId,
    /// Which stream it took, or `None` for a datagram. The recipient has to put
    /// it back on the lane it came from.
    pub stream: Option<StreamMode>,
    /// The bytes.
    pub payload: Bytes,
}

/// Per-datagram wire overhead, matching `orrery_net::budget`.
const DATAGRAM_OVERHEAD: u64 = 60;

/// The MTU a stream message is cut into segments of, for the retransmission
/// model. Matches `orrery_witness::ASSUMED_MTU`.
const MTU_BYTES: u64 = 1_200;

/// A ceiling on modelled retransmissions of one segment.
///
/// At 3% loss the chance of needing this many is about one in ten trillion; it
/// exists so an impairment set to near-total loss cannot spin here rather than
/// returning an absurd delivery time, which is the honest answer for a link
/// that is effectively down.
const MAX_RETRANSMITS: u32 = 8;

enum Fate {
    Dropped,
    Delayed(u64),
    Now(u64),
}

/// The application-visible result of the datagram impairment decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramDisposition {
    /// The datagram was retained for immediate or delayed delivery.
    Delivered,
    /// The datagram was discarded by the configured loss model.
    Dropped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn packet() -> Bytes {
        Bytes::from_static(b"0123456789")
    }

    #[test]
    fn a_clean_link_delivers_everything_in_the_tick_it_was_sent() {
        let mut router = Router::new(Impairment::default(), 1);
        for _ in 0..100 {
            router.accept(0, node(1), 0, packet());
        }
        assert_eq!(router.deliver_due(0).len(), 100);
        assert_eq!(router.counters.dropped, 0);
        assert_eq!(router.in_flight(), 0);
    }

    #[test]
    fn loss_is_applied_at_roughly_the_configured_rate() {
        // Not an exact count — it is a random process. The assertion is that
        // the model is neither inert nor a black hole, both of which would make
        // every impairment result meaningless.
        let mut router = Router::new(
            Impairment {
                loss: 0.30,
                ..Impairment::default()
            },
            7,
        );
        for _ in 0..10_000 {
            router.accept(0, node(1), 0, packet());
        }
        let dropped = router.counters.dropped;
        assert!(
            (2_500..=3_500).contains(&dropped),
            "expected ~3000 drops in 10000, got {dropped}"
        );
    }

    #[test]
    fn the_same_seed_produces_the_same_losses() {
        // A false-positive rate that cannot be replayed cannot be investigated,
        // which is the whole reason impairment is seeded rather than ambient.
        let run = |seed| {
            let mut router = Router::new(Impairment::p4_profile(), seed);
            for tick in 0..1_000 {
                router.accept(tick, node(1), 0, packet());
            }
            (router.counters.dropped, router.counters.delayed)
        };
        assert_eq!(run(42), run(42));
        assert_ne!(run(42), run(43));
    }

    #[test]
    fn a_jittered_packet_arrives_late_rather_than_never() {
        // The witness must absorb a delayed frame without deciding the chain
        // has a hole that will never fill; a jitter model that dropped instead
        // of delaying would never exercise that.
        let mut router = Router::new(
            Impairment {
                loss: 0.0,
                jitter_ticks: 6,
                jitter_rate: 1.0,
                ..Impairment::default()
            },
            3,
        );
        router.accept(0, node(1), 0, packet());
        assert!(router.deliver_due(0).is_empty(), "held back");
        assert!(router.deliver_due(5).is_empty(), "still held");
        assert_eq!(router.deliver_due(6).len(), 1, "arrives on time, late");
        assert_eq!(router.counters.dropped, 0);
    }

    #[test]
    fn delivery_order_is_arrival_order_so_a_delayed_packet_is_overtaken() {
        let mut router = Router::new(
            Impairment {
                loss: 0.0,
                jitter_ticks: 3,
                jitter_rate: 0.0,
                ..Impairment::default()
            },
            3,
        );
        // Force one delayed packet by hand, then a prompt one behind it.
        router.in_flight.push_back(InFlight {
            to: 0,
            from: node(1),
            payload: Bytes::from_static(b"late"),
            due: 5,
            stream: None,
        });
        router.accept(0, node(1), 0, Bytes::from_static(b"prompt"));
        let now = router.deliver_due(0);
        assert_eq!(now.len(), 1);
        assert_eq!(&now[0].payload[..], b"prompt");
        assert_eq!(&router.deliver_due(5)[0].payload[..], b"late");
    }

    #[test]
    fn a_stream_message_is_never_dropped_only_delayed() {
        // The whole reason gap repair moved onto this lane. A dropped repair
        // turns one lost datagram into a permanent hole, and an unfillable hole
        // is the one witness input that is reportable — so loss here would
        // manufacture accusations out of ordinary packet loss.
        let mut router = Router::new(
            Impairment {
                loss: 0.30,
                ..Impairment::default()
            },
            5,
        );
        for _ in 0..500 {
            router.accept_stream(0, node(1), 0, StreamMode::Bulk, packet());
        }
        let delivered: usize = (0..200).map(|tick| router.deliver_due(tick).len()).sum();
        assert_eq!(delivered, 500, "every message arrives, however late");
        assert!(
            router.counters.stream_retransmits > 0,
            "at 30% loss some of them must have cost a retransmission"
        );
        assert_eq!(router.counters.dropped, 0);
    }

    #[test]
    fn a_shared_stream_holds_later_messages_behind_a_retransmitted_one() {
        // Head of line, and the reason `gates/p4-streams-bench` exists: a 40 kB
        // repair that loses a segment does not only delay itself.
        let mut router = Router::new(
            Impairment {
                loss: 0.50,
                ..Impairment::default()
            },
            9,
        );
        let big = Bytes::from(vec![0u8; 40_000]);
        router.accept_stream(0, node(1), 0, StreamMode::Shared, big);
        for _ in 0..10 {
            router.accept_stream(0, node(1), 0, StreamMode::Shared, packet());
        }
        assert!(
            router.counters.stream_head_of_line_ticks > 0,
            "ten tiny messages queued behind a 40 kB one must have waited"
        );

        // The same messages on their own streams wait for nobody.
        let mut independent = Router::new(
            Impairment {
                loss: 0.50,
                ..Impairment::default()
            },
            9,
        );
        let big = Bytes::from(vec![0u8; 40_000]);
        independent.accept_stream(0, node(1), 0, StreamMode::Bulk, big);
        for _ in 0..10 {
            independent.accept_stream(0, node(1), 0, StreamMode::Bulk, packet());
        }
        assert_eq!(independent.counters.stream_head_of_line_ticks, 0);
    }

    #[test]
    fn a_bigger_stream_message_is_likelier_to_need_a_retransmission() {
        // Charged per segment, not per message: a real stream retries the
        // segment that was lost, so a 40 kB repair meets loss thirty times as
        // often as a lease ack does.
        let attempts = |bytes: usize| {
            let mut router = Router::new(
                Impairment {
                    loss: 0.05,
                    ..Impairment::default()
                },
                17,
            );
            for _ in 0..200 {
                router.accept_stream(
                    0,
                    node(1),
                    0,
                    StreamMode::Bulk,
                    Bytes::from(vec![0u8; bytes]),
                );
            }
            router.counters.stream_retransmits
        };
        assert!(
            attempts(40_000) > attempts(120) * 5,
            "a 40 kB message spans ~34 segments and a 120 B one spans one"
        );
    }

    #[test]
    fn the_p4_profile_is_within_the_criterion_band() {
        // docs/11-roadmap.md §P4: 3–5% packet loss, 100 ms jitter spikes.
        let profile = Impairment::p4_profile();
        assert!((0.03..=0.05).contains(&profile.loss));
        assert_eq!(profile.jitter_ticks, 6, "100 ms at 60 Hz");
        assert!(!profile.is_clean());
        assert!(Impairment::default().is_clean());

        // The far end of the band differs in loss and in nothing else — the
        // jitter spike is stated separately by the criterion.
        let worst = Impairment::p4_profile_at_loss(0.05);
        assert!((0.03..=0.05).contains(&worst.loss));
        assert_eq!(worst.jitter_ticks, profile.jitter_ticks);
        assert_eq!(worst.jitter_rate, profile.jitter_rate);
        assert_eq!(worst.retransmit_ticks, profile.retransmit_ticks);
    }
}

#[cfg(test)]
mod payload_tests {
    use orrery_net::channels::{decode_datagram, encode_datagram};
    use orrery_protocol::CellId;

    /// The harness sends the body bytes plus the authority's committed cell.
    ///
    /// If this pair does not survive the codec, every peer silently holds zero
    /// replicas — and every clause about interest passes by being empty, which
    /// is the most expensive kind of green.
    #[test]
    fn the_state_payload_round_trips() {
        let cell = CellId::from_coords(glam::IVec3::new(3, -2, 5), CellId::MAX_LEVEL).unwrap();
        let body = vec![7u8; 52];
        let wire = encode_datagram(&(body.clone(), cell));
        let decoded = decode_datagram::<(Vec<u8>, CellId)>(&wire);
        assert_eq!(decoded, Some((body, cell)));
    }
}
