//! The peer upload budget — ≤ 1 Mbps sustained (D6, D16).
//!
//! Every P2P bandwidth claim in the design rests on this number: the Donnybrook
//! `~12·n kb/s` scaling, the 32-peer mesh ceiling, the 24-entity interest set,
//! the 1–4 Hz proxy floor. P1's demo criterion is *"every peer's sustained
//! upload stays ≤ 1 Mbps"* over an hour. Until now nothing counted a byte.
//!
//! # Wire bytes, not payload bytes
//!
//! The meter charges each packet its payload **plus**
//! [`DATAGRAM_OVERHEAD_BYTES`] — IP+UDP 28 B and QUIC short header + AEAD ≈ 32 B
//! (docs/03-replication.md §7). That is not a rounding detail. At 20 Hz across
//! many links the framing floor dominates: docs/02-networking.md §5 works out
//! that a 128-peer full mesh cannot afford *empty* packets, because 1 Mbps over
//! 127 links leaves ~49 bytes per send against ~60 bytes of overhead. A meter
//! that counted only payload would report roughly half the truth and would
//! certify a budget the wire does not honour.
//!
//! # What this enforces, and what it does not
//!
//! It enforces the *ceiling*: over budget, [`Lane::Replication`] packets are
//! shed and [`Lane::Control`] and [`Lane::Witness`] packets always pass. That
//! asymmetry is the same one gap repair rests on — a replication update lost is
//! superseded 50 ms later by the next, while shedding a repair, a lease
//! operation, or a link in a hash chain turns one dropped datagram into a
//! permanent hole. [`is_sheddable`] carries the measurement.
//!
//! It does **not** decide *which* state to drop. docs/03-replication.md §9.3
//! specifies shedding by relevance class from the bottom, apportioned by a
//! priority accumulator, with high-rate spend capped at
//! [`HIGH_RATE_SHARE`] of each link so the proxy floor cannot be starved. That
//! machinery is `orrery_predict`'s (D8) and reads [`UploadBudget`] rather than
//! reimplementing it. What lives here is the backstop, because
//! docs/03-replication.md §4 is explicit that *"senders enforce their own upload
//! budget regardless of requests — a subscription is a request, not a
//! contract"*, and that has to hold whether or not an accumulator is wired up.

use core::time::Duration;

use bevy_ecs::prelude::*;

use crate::channels::{untag, Channel, TAG_REPLICATION_DELTA};
use orrery_protocol::{channels::WireFamily, NodeId};

/// Per-datagram wire overhead: IP+UDP 28 B, QUIC short header + AEAD ≈ 32 B.
///
/// docs/03-replication.md §7 uses 60 B for the same modelling.
pub const DATAGRAM_OVERHEAD_BYTES: u64 = 60;

/// Per-message overhead on the reliable lane: the lane's own `u32` length
/// prefix, plus a QUIC `STREAM` frame header (type, stream id, offset, length).
///
/// This is charged **once per message**, not once per packet — the packet-level
/// cost is [`DATAGRAM_OVERHEAD_BYTES`] per MTU of payload, which
/// [`stream_wire_bytes`] adds separately.
pub const STREAM_MESSAGE_OVERHEAD_BYTES: u64 = 4 + 12;

/// What one datagram of `payload` bytes costs on the wire.
#[must_use]
pub const fn datagram_wire_bytes(payload: usize) -> u64 {
    payload as u64 + DATAGRAM_OVERHEAD_BYTES
}

/// What one reliable-lane message of `payload` bytes costs on the wire.
///
/// A stream message is not one datagram. It is cut into as many packets as the
/// path MTU requires, each carrying the same IP+UDP+QUIC framing a datagram
/// does, plus one `STREAM` frame header for the message itself. Charging it a
/// flat [`DATAGRAM_OVERHEAD_BYTES`] would *understate* a 40 kB repair by about
/// two kilobytes and *overstate* a 30-byte lease ack — which is the wrong sign
/// in both directions, since the first is what the budget exists to bound.
///
/// Retransmission is deliberately not modelled. The meter's job is to bound
/// what this peer *offers* the link; what loss then costs is the link's to
/// report, and folding an assumed loss rate in here would make the budget
/// depend on a number no sender knows.
#[must_use]
pub fn stream_wire_bytes(payload: usize, mtu: usize) -> u64 {
    let bytes = payload as u64 + STREAM_MESSAGE_OVERHEAD_BYTES;
    let mtu = (mtu as u64).max(1);
    let packets = bytes.div_ceil(mtu).max(1);
    bytes + packets * DATAGRAM_OVERHEAD_BYTES
}

/// The share of a link's budget the high-rate set may spend (docs/03 §9.3).
///
/// The residual is reserved for the 1 Hz proxy floor, so a crowded interest set
/// cannot starve ring-1 and ambient entities indefinitely. Exposed here because
/// the budget is defined here; the accumulator that applies it is `orrery_predict`'s.
pub const HIGH_RATE_SHARE: f32 = 0.80;

/// A rate in bits per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bandwidth(u64);

impl Bandwidth {
    /// A rate from bits per second.
    #[must_use]
    pub const fn from_bits_per_sec(bits: u64) -> Self {
        Self(bits)
    }

    /// A rate from megabits per second (10⁶ bits, the network convention).
    #[must_use]
    pub const fn from_mbps(mbps: u64) -> Self {
        Self(mbps * 1_000_000)
    }

    /// A rate from kilobits per second.
    #[must_use]
    pub const fn from_kbps(kbps: u64) -> Self {
        Self(kbps * 1_000)
    }

    /// The rate in bits per second.
    #[must_use]
    pub const fn bits_per_sec(self) -> u64 {
        self.0
    }

    /// The rate in bytes per second, rounded down.
    #[must_use]
    pub const fn bytes_per_sec(self) -> u64 {
        self.0 / 8
    }

    /// How many bytes this rate affords over `window`.
    #[must_use]
    pub fn bytes_over(self, window: Duration) -> u64 {
        // Nanosecond arithmetic in u128: a 1 Mbps × 1 s product overflows
        // nothing, but a large window against a datacenter budget would.
        let bits = u128::from(self.0) * u128::from(window.as_nanos() as u64);
        (bits / 1_000_000_000 / 8) as u64
    }
}

impl core::fmt::Display for Bandwidth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 >= 1_000_000 {
            write!(f, "{:.2} Mbps", self.0 as f64 / 1_000_000.0)
        } else {
            write!(f, "{:.1} kbps", self.0 as f64 / 1_000.0)
        }
    }
}

/// This peer's upload budget (D6, D16).
#[derive(Debug, Clone, Copy, Resource)]
pub struct UploadBudget {
    /// Sustained upload ceiling. Default 1 Mbps — a consumer uplink figure, not
    /// a datacenter one. Field hosts raise it (≤ 35 Mbps hot-cell egress, D6).
    pub sustained: Bandwidth,
    /// The averaging window "sustained" is measured over.
    ///
    /// One second by default. Much shorter and ordinary 20 Hz burstiness reads
    /// as oversubscription — a send tick is 50 ms, so a quarter-second window
    /// sees only five of them. Much longer and a real overrun goes unnoticed
    /// for seconds, which on a consumer uplink means a filled buffer and a
    /// latency spike well before the meter admits anything is wrong.
    pub window: Duration,
}

impl Default for UploadBudget {
    fn default() -> Self {
        Self {
            sustained: Bandwidth::from_mbps(1),
            window: Duration::from_secs(1),
        }
    }
}

impl UploadBudget {
    /// The share of the budget one link may spend when `links` are active.
    ///
    /// The flat division; docs/03-replication.md §5.3 weights interest-set links
    /// above proxy-only ones, which is the accumulator's job. Zero links yields
    /// the whole budget rather than dividing by zero — a peer alone in an island
    /// is not constrained by sharing.
    #[must_use]
    pub fn per_link(self, links: usize) -> Bandwidth {
        Bandwidth::from_bits_per_sec(self.sustained.bits_per_sec() / links.max(1) as u64)
    }

    /// The share of a link's budget the high-rate set may spend (docs/03 §9.3).
    #[must_use]
    pub fn high_rate_share(self, links: usize) -> Bandwidth {
        let per_link = self.per_link(links).bits_per_sec() as f64;
        Bandwidth::from_bits_per_sec((per_link * f64::from(HIGH_RATE_SHARE)) as u64)
    }
}

/// A sliding-window byte meter.
///
/// Bucketed rather than a queue of timestamps so memory is fixed: a burst
/// cannot grow the meter, which matters because the thing being metered is
/// precisely the case where too much is being sent.
#[derive(Debug, Clone)]
pub struct RateMeter {
    buckets: Vec<u64>,
    cursor: usize,
    /// Start of the bucket `cursor` points at. `None` until the first record.
    bucket_start: Option<Duration>,
    window: Duration,
}

impl RateMeter {
    /// The number of buckets a window is divided into.
    ///
    /// Twenty at a one-second window is one bucket per 20 Hz send tick, so the
    /// window edge never cuts a tick in half.
    pub const BUCKETS: usize = 20;

    /// A meter over `window`.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            buckets: vec![0; Self::BUCKETS],
            cursor: 0,
            bucket_start: None,
            window,
        }
    }

    /// Duration of one bucket.
    #[must_use]
    fn bucket_span(&self) -> Duration {
        self.window / Self::BUCKETS as u32
    }

    /// Roll the ring forward to `now`, clearing buckets that have aged out.
    fn advance(&mut self, now: Duration) {
        let span = self.bucket_span();
        let Some(start) = self.bucket_start else {
            self.bucket_start = Some(now);
            return;
        };
        if now < start {
            // Time went backwards (a clock the caller controls, in tests).
            // Treat it as a fresh window rather than rolling backwards.
            self.buckets.fill(0);
            self.bucket_start = Some(now);
            return;
        }
        let elapsed = now - start;
        let steps = (elapsed.as_nanos() / span.as_nanos().max(1)) as usize;
        if steps == 0 {
            return;
        }
        // More than a whole window of silence: everything has aged out.
        if steps >= Self::BUCKETS {
            self.buckets.fill(0);
        } else {
            for step in 1..=steps {
                let index = (self.cursor + step) % Self::BUCKETS;
                self.buckets[index] = 0;
            }
        }
        self.cursor = (self.cursor + steps) % Self::BUCKETS;
        self.bucket_start = Some(start + span * steps as u32);
    }

    /// Charge `bytes` at `now`.
    pub fn record(&mut self, now: Duration, bytes: u64) {
        self.advance(now);
        self.buckets[self.cursor] = self.buckets[self.cursor].saturating_add(bytes);
    }

    /// Bytes charged within the window ending at `now`.
    pub fn bytes_in_window(&mut self, now: Duration) -> u64 {
        self.advance(now);
        self.buckets.iter().sum()
    }

    /// The rate over the window ending at `now`.
    pub fn rate(&mut self, now: Duration) -> Bandwidth {
        let bytes = self.bytes_in_window(now);
        let nanos = self.window.as_nanos().max(1);
        let bits = u128::from(bytes) * 8 * 1_000_000_000 / nanos;
        Bandwidth::from_bits_per_sec(bits as u64)
    }

    /// Whether charging `bytes` now would put the window over `budget`.
    pub fn would_exceed(&mut self, now: Duration, bytes: u64, budget: UploadBudget) -> bool {
        let allowance = budget.sustained.bytes_over(budget.window);
        self.bytes_in_window(now).saturating_add(bytes) > allowance
    }
}

/// What the upload lane has spent, and what it had to shed.
#[derive(Debug, Default, Resource)]
pub struct UploadMeter {
    /// The whole peer's upload — what the ≤ 1 Mbps budget is actually about.
    total: Option<RateMeter>,
    /// Per-link spend, which the accumulator apportions against.
    per_peer: Vec<(NodeId, RateMeter)>,
    /// Packets shed for want of budget.
    pub shed: u64,
    /// Bytes shed for want of budget.
    pub shed_bytes: u64,
    /// Unsheddable packets sent while over budget (see [`is_sheddable`]) —
    /// never shed, always counted.
    ///
    /// A non-zero value here with `shed` climbing is the honest picture of an
    /// oversubscribed peer: the lanes that cannot be shed are still being paid
    /// for, so the overrun is real rather than an artefact of the backstop.
    ///
    /// This deliberately replaces the old `oversubscribed` last-send-pass
    /// boolean. It had no reader, and a single quiet pass cleared the only
    /// signal that could establish the sustained condition docs/03 §9.3 needs.
    /// Consumers instead sample [`Self::rate`] over their own promotion window
    /// and use this counter to identify an overrun the backstop could not shed.
    pub unsheddable_over_budget: u64,
    /// What each lane spent, cumulatively over the session.
    ///
    /// Cumulative rather than windowed because the question it answers is a
    /// budgeting one — *what share of a peer's uplink does witnessing cost?* —
    /// and that is a property of the run, not of the last second of it.
    pub lanes: LaneTally,
}

impl UploadMeter {
    /// Charge a packet of `payload` bytes to `peer` at `now`, with wire
    /// overhead included.
    pub fn record(&mut self, budget: UploadBudget, now: Duration, peer: NodeId, payload: usize) {
        self.record_wire(budget, now, peer, datagram_wire_bytes(payload));
    }

    /// Charge an already-costed send of `wire` bytes to `peer` at `now`.
    ///
    /// The two channels cost differently — see [`stream_wire_bytes`] — so the
    /// caller that knows which one it is on computes the figure and this
    /// records it.
    ///
    /// Charged to [`Lane::Replication`]. Callers that know the lane use
    /// [`Self::charge`]; this exists for the ones that only have a byte count,
    /// and it charges the lane the budget exists to protect rather than
    /// silently discounting an unattributed send.
    pub fn record_wire(&mut self, budget: UploadBudget, now: Duration, peer: NodeId, wire: u64) {
        self.charge(budget, now, peer, Lane::Replication, wire);
    }

    /// Charge a send of `wire` bytes on `lane` to `peer` at `now`.
    ///
    /// The rate meters do not care which lane a byte came from — the ≤ 1 Mbps
    /// ceiling is over the whole uplink — but the budget *decision* does, which
    /// is why the tally is kept beside them rather than instead of them.
    pub fn charge(
        &mut self,
        budget: UploadBudget,
        now: Duration,
        peer: NodeId,
        lane: Lane,
        wire: u64,
    ) {
        self.total_meter(budget).record(now, wire);
        self.peer_meter(budget, peer).record(now, wire);
        self.lanes.charge(lane, wire);
    }

    /// Whether a packet of `payload` bytes would exceed the budget now.
    pub fn would_exceed(&mut self, budget: UploadBudget, now: Duration, payload: usize) -> bool {
        self.would_exceed_wire(budget, now, datagram_wire_bytes(payload))
    }

    /// Whether an already-costed send of `wire` bytes would exceed the budget.
    pub fn would_exceed_wire(&mut self, budget: UploadBudget, now: Duration, wire: u64) -> bool {
        self.total_meter(budget).would_exceed(now, wire, budget)
    }

    /// This peer's total upload rate over the window ending at `now`.
    pub fn rate(&mut self, budget: UploadBudget, now: Duration) -> Bandwidth {
        self.total_meter(budget).rate(now)
    }

    /// One link's upload rate over the window ending at `now`.
    pub fn peer_rate(&mut self, budget: UploadBudget, now: Duration, peer: NodeId) -> Bandwidth {
        self.peer_meter(budget, peer).rate(now)
    }

    /// The links this meter has seen traffic to.
    pub fn links(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.per_peer.iter().map(|(node, _)| *node)
    }

    /// Forget a link's meter — a peer that has left the island.
    pub fn forget(&mut self, peer: NodeId) {
        self.per_peer.retain(|(node, _)| *node != peer);
    }

    fn total_meter(&mut self, budget: UploadBudget) -> &mut RateMeter {
        self.total
            .get_or_insert_with(|| RateMeter::new(budget.window))
    }

    fn peer_meter(&mut self, budget: UploadBudget, peer: NodeId) -> &mut RateMeter {
        if let Some(index) = self.per_peer.iter().position(|(node, _)| *node == peer) {
            return &mut self.per_peer[index].1;
        }
        self.per_peer.push((peer, RateMeter::new(budget.window)));
        let last = self.per_peer.len() - 1;
        &mut self.per_peer[last].1
    }
}

/// Whether a packet on `lane` may be shed when the budget is spent.
///
/// Replication only, and the two exclusions are the same argument at different
/// strengths: **a packet whose loss is repaired by the next one is cheap to
/// drop, and a packet whose loss opens a hole is not.**
///
/// A control packet was never sheddable — a gap repair that never arrives is
/// indistinguishable from an authority refusing to answer, and a lease
/// operation that never arrives is a stall rather than a retry.
///
/// [`Lane::Witness`] joined it on measurement. A witness frame is not a
/// snapshot that the next send supersedes; it is a link in a hash chain, and
/// dropping one converts a *sheddable* 316-byte datagram into an
/// *unsheddable* `LogRangeRequest` and its response on the control lane. So
/// shedding the witness lane does not relieve an overrun, it deepens one, and
/// it does so on the lane the backstop cannot touch. P1's 32-peer swarm showed
/// exactly that: with the two lanes shed indifferently, 14 630 shed packets
/// produced 9 224 chain gaps and drove observation coverage from 100% to 81% —
/// a witness that had stopped watching, reported as a witness that found
/// nothing.
///
/// docs/03-replication.md §5.3a calls witness records "low priority", and they
/// are: the lane is bounded at source by its frame cadence
/// (`orrery_witness::plugin::WITNESS_LANE_SHARE`) so it *cannot* be the thing
/// that exhausts the budget. Low priority in what it may spend, not in what
/// survives when the spend is already committed.
///
/// [`Lane::Hit`] has the same survival rule for a different reason: a claim is
/// small, latency-critical gameplay input and is retried until its verdict
/// names the claim key. Shedding it behind continuous bulk replication loses a
/// shot the player has already taken; shedding replication instead loses only
/// a snapshot superseded by the next send.
#[must_use]
pub const fn is_sheddable(lane: Lane) -> bool {
    matches!(lane, Lane::Replication)
}

/// The order in which one send tick admits packets to the upload meter.
///
/// The backstop cannot drop control, witness, hit, or witness-keyframe
/// traffic. Within replication, an absolute keyframe is an anchor for
/// subsequent deltas, so it is admitted before deltas. The delta distinction
/// is read from the packet's wire sub-tag, not supplied by the caller as a
/// second piece of metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BatchPriority {
    /// Control, witness, hit, and witness-keyframe packets, never shed.
    Unsheddable,
    /// Absolute replication state, including keyframes.
    Keyframe,
    /// A replication delta that references a keyframe.
    Delta,
}

/// Classify a packet for admission within this update's send batch.
///
/// A state payload with no valid replication-delta wire sub-tag remains an
/// absolute replication packet. In particular, omitting a sub-tag cannot
/// reclassify state traffic as control, witness, hit, or witness-keyframe, and
/// a caller has no caller-provided priority to lie about.
#[must_use]
pub(crate) fn batch_priority(channel: Channel, payload: &[u8]) -> BatchPriority {
    if !is_sheddable(lane_of(channel, payload)) {
        return BatchPriority::Unsheddable;
    }

    match untag(payload) {
        Some((Channel::State, body)) if body.first() == Some(&TAG_REPLICATION_DELTA) => {
            BatchPriority::Delta
        }
        _ => BatchPriority::Keyframe,
    }
}

/// Which kind of traffic a packet carries, for accounting and shedding order.
///
/// Read *off the wire* rather than declared by the sender. `Channel::State`
/// already carries a sub-tag — [`WireFamily`] — so a receiver can route a
/// datagram without parsing it, and the meter reads the same byte the receiver
/// routes on. A field on the packet would be a second source of truth for one
/// fact, and it would drift from the wire the first time a caller left it at
/// its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// Replicated entity state — the interactive lane, and the only sheddable
    /// one. A dropped update is superseded 50 ms later by the next.
    Replication,
    /// Verifiable-core log frames and state claims (docs/03-replication.md §5.3a).
    Witness,
    /// An absolute replication anchor sent to a witness-set link (A20 §4).
    WitnessKeyframe,
    /// Hit claims and verdicts: latency-critical state traffic retried until a
    /// verdict, not a snapshot superseded by the next replication send.
    Hit,
    /// The reliable lane: gap repairs, leases, handshakes.
    Control,
}

/// Read the inner, protocol-owned channel tag and sub-tag from a send payload.
///
/// `SendPacket` adds its transport tag later, so its payload is the complete
/// logical frame produced by the protocol encoder. Deliberately consult that
/// inner frame first: a delivered input remains a delivered input if a future
/// transport change moves it from a stream to a datagram.
fn wire_family(payload: &[u8]) -> Option<WireFamily> {
    let (channel, body) = untag(payload)?;
    WireFamily::from_frame(channel, body.first().copied()?)
}

/// The delivery classification for a declared [`WireFamily`].
///
/// There is intentionally no wildcard arm. Adding a protocol sub-tag means
/// adding a [`WireFamily`] variant, which makes this match fail to compile
/// until its delivery class is decided.
const fn lane_for_family(transport: Channel, family: WireFamily) -> Lane {
    match family {
        WireFamily::Replication
        | WireFamily::ReplicationCompressed
        | WireFamily::ReplicationDelta => Lane::Replication,
        WireFamily::WitnessKeyframe => Lane::WitnessKeyframe,
        // Range repairs share the witness envelope but ride the control
        // transport. They remain the separately named, uncapped control family
        // in #925's source-cap audit; the ordinary witness log stays witness.
        WireFamily::Witness | WireFamily::WitnessCompressed => {
            if transport.is_stream() {
                Lane::Control
            } else {
                Lane::Witness
            }
        }
        // This arm intentionally ignores `transport`: a delivered input stays
        // unsheddable if a future transport moves it to a datagram.
        WireFamily::DeliveredInput => Lane::Control,
        WireFamily::Hit => Lane::Hit,
    }
}

/// Which lane a packet belongs to, from its protocol-owned wire class.
///
/// Declared families are read from their protocol bytes. The one exception is
/// the witness envelope's control transport, which distinguishes range repair
/// from normal witness log traffic without a second caller-provided field.
/// Opaque bytes retain the conservative transport fallback: raw state is
/// charged to replication, and raw stream bytes to control.
#[must_use]
pub fn lane_of(channel: Channel, payload: &[u8]) -> Lane {
    match wire_family(payload) {
        Some(family) => lane_for_family(channel, family),
        None if channel.is_datagram() => Lane::Replication,
        None => Lane::Control,
    }
}

/// Wire bytes and packet counts, split by [`Lane`].
///
/// The split is the whole point of the resource. "Peak upload was 1006 kbps"
/// says a peer is over budget; it does not say which lane to change, and P4's
/// witness question is precisely *which*. Charged in wire bytes, matching the
/// meter, so a share is a fraction of the same number the ceiling is expressed
/// in.
#[derive(Debug, Default, Clone, Copy)]
pub struct LaneTally {
    /// Wire bytes charged to replication.
    pub replication_bytes: u64,
    /// Wire bytes charged to the witness lane.
    pub witness_bytes: u64,
    /// Wire bytes charged to witness-link replication keyframes.
    pub witness_keyframe_bytes: u64,
    /// Wire bytes charged to the hit-registration lane.
    pub hit_bytes: u64,
    /// Wire bytes charged to the control lane.
    pub control_bytes: u64,
    /// Replication packets shed for want of budget.
    pub replication_shed: u64,
}

impl LaneTally {
    /// Every wire byte this peer has offered the link, across all lanes.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.replication_bytes
            + self.witness_bytes
            + self.witness_keyframe_bytes
            + self.hit_bytes
            + self.control_bytes
    }

    /// The witness lane's share of everything sent, in \[0, 1\].
    ///
    /// Zero when nothing has been sent — a peer that has said nothing has not
    /// spent a disproportionate share of anything.
    #[must_use]
    pub fn witness_share(&self) -> f64 {
        let total = self.total_bytes();
        if total == 0 {
            return 0.0;
        }
        self.witness_bytes as f64 / total as f64
    }

    fn charge(&mut self, lane: Lane, wire: u64) {
        match lane {
            Lane::Replication => self.replication_bytes += wire,
            Lane::Witness => self.witness_bytes += wire,
            Lane::WitnessKeyframe => self.witness_keyframe_bytes += wire,
            Lane::Hit => self.hit_bytes += wire,
            Lane::Control => self.control_bytes += wire,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh::SecretKey::from_bytes(&seed).public()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn one_mbps_is_the_default_and_is_125_kb_per_second() {
        let budget = UploadBudget::default();
        assert_eq!(budget.sustained, Bandwidth::from_mbps(1));
        assert_eq!(budget.sustained.bytes_per_sec(), 125_000);
        assert_eq!(budget.sustained.bytes_over(Duration::from_secs(1)), 125_000);
        assert_eq!(budget.sustained.bytes_over(ms(500)), 62_500);
    }

    #[test]
    fn a_packet_is_charged_its_wire_size_not_its_payload() {
        // At 20 Hz across many links the framing floor dominates: 1 Mbps over
        // 127 links leaves ~49 bytes per send against ~60 bytes of overhead, so
        // a payload-only meter would certify a budget the wire cannot honour.
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        meter.record(budget, ms(0), node(1), 40);
        assert_eq!(
            meter.total_meter(budget).bytes_in_window(ms(0)),
            40 + DATAGRAM_OVERHEAD_BYTES
        );
    }

    #[test]
    fn a_steady_send_reports_its_rate() {
        // 20 Hz × 500 B payload ≈ 20 × 560 wire B/s = 11 200 B/s = 89.6 kbps.
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        for tick in 0..20 {
            meter.record(budget, ms(tick * 50), node(1), 500);
        }
        let rate = meter.rate(budget, ms(950));
        assert!(
            (85_000..=92_000).contains(&rate.bits_per_sec()),
            "expected ~89.6 kbps, got {rate}"
        );
    }

    #[test]
    fn traffic_ages_out_of_the_window() {
        // Otherwise "sustained" would mean "cumulative", and a peer that sent
        // hard for one second would be over budget forever.
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        for tick in 0..20 {
            meter.record(budget, ms(tick * 50), node(1), 5_000);
        }
        assert!(meter.rate(budget, ms(950)).bits_per_sec() > 0);
        // A full window later, with nothing sent, the meter is empty again.
        assert_eq!(meter.rate(budget, ms(3_000)).bits_per_sec(), 0);
    }

    #[test]
    fn the_budget_is_the_whole_peer_not_one_link() {
        // The D6 number is a peer's uplink. Two links each under budget can put
        // the peer over it, which is exactly the mesh scaling problem.
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        // 70 kB to each of two peers inside one window: 140 kB > 125 kB.
        for tick in 0..20 {
            meter.record(budget, ms(tick * 50), node(1), 3_500);
            meter.record(budget, ms(tick * 50), node(2), 3_500);
        }
        assert!(meter.would_exceed(budget, ms(950), 1_000));
        assert!(
            meter.peer_rate(budget, ms(950), node(1)).bits_per_sec()
                < budget.sustained.bits_per_sec(),
            "each link alone is under budget"
        );
    }

    #[test]
    fn a_link_that_leaves_is_forgotten() {
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        meter.record(budget, ms(0), node(1), 100);
        assert_eq!(meter.links().count(), 1);
        meter.forget(node(1));
        assert_eq!(meter.links().count(), 0);
    }

    #[test]
    fn per_link_division_never_divides_by_zero() {
        // A peer alone in an island is not constrained by sharing.
        let budget = UploadBudget::default();
        assert_eq!(budget.per_link(0), budget.sustained);
        assert_eq!(budget.per_link(1), budget.sustained);
        assert_eq!(budget.per_link(4), Bandwidth::from_kbps(250));
    }

    #[test]
    fn the_high_rate_set_cannot_spend_the_whole_link() {
        // docs/03 §9.3: the residual is reserved for the 1 Hz proxy floor, so a
        // crowded interest set cannot starve ambient entities indefinitely.
        let budget = UploadBudget::default();
        let link = budget.per_link(4);
        let high = budget.high_rate_share(4);
        assert!(high < link);
        assert_eq!(high.bits_per_sec(), 200_000);
    }

    #[test]
    fn only_replication_is_sheddable() {
        // Shedding a repair or a lease operation turns one dropped datagram
        // into a permanent hole; replication loss is expected and repaired by
        // the next send 50 ms later.
        assert!(is_sheddable(Lane::Replication));
        assert!(!is_sheddable(Lane::Control));
        assert!(!is_sheddable(Lane::WitnessKeyframe));
        assert!(!is_sheddable(Lane::Hit));
    }

    #[test]
    fn shedding_a_witness_frame_would_cost_more_than_it_saves() {
        // A log frame is a link in a hash chain, not a snapshot the next send
        // supersedes: dropping one converts a sheddable 316-byte datagram into
        // an unsheddable `LogRangeRequest` and response on the control lane. On
        // P1's 32-peer swarm, shedding the two lanes indifferently turned 14 630
        // shed packets into 9 224 chain gaps and drove observation coverage from
        // 100% to 81%. The lane is bounded at source instead — see
        // `orrery_witness::plugin::WITNESS_LANE_SHARE_PCT`.
        assert!(!is_sheddable(Lane::Witness));
    }

    #[test]
    fn the_lane_is_read_off_the_wire_rather_than_taken_on_trust() {
        use crate::channels::{
            encode_replication, encode_witness, encode_witness_compressed, encode_witness_keyframe,
            tag, TAG_HIT,
        };

        // The sub-tag a receiver routes on is the same byte the meter reads, so
        // the two cannot disagree about what a datagram was.
        assert_eq!(
            lane_of(Channel::State, &encode_witness(&[1u8, 2, 3])),
            Lane::Witness
        );
        assert_eq!(
            lane_of(Channel::State, &encode_replication(&[1u8, 2, 3])),
            Lane::Replication
        );
        assert_eq!(
            lane_of(
                Channel::State,
                &encode_witness_compressed(&vec![0u8; 4_096])
            ),
            Lane::Witness
        );
        assert_eq!(
            lane_of(Channel::State, &tag(Channel::State, &[TAG_HIT, 0])),
            Lane::Hit
        );
        assert_eq!(
            lane_of(Channel::State, &encode_witness_keyframe(&[1u8, 2, 3])),
            Lane::WitnessKeyframe
        );
        // Witness range repair shares the envelope but is control traffic when
        // sent over a stream; #925 names its missing cap separately.
        assert_eq!(
            lane_of(Channel::Control, &encode_witness(&[1u8, 2, 3])),
            Lane::Control
        );
    }

    #[test]
    fn an_untagged_state_payload_is_still_charged_to_replication() {
        // State traffic is replication unless its actual wire bytes positively
        // identify it as witness traffic. Omitting the tag never makes it
        // cheaper by moving it onto an unsheddable lane.
        assert_eq!(lane_of(Channel::State, &[]), Lane::Replication);
        assert_eq!(lane_of(Channel::State, &[0, 99, 1]), Lane::Replication);
    }

    #[test]
    fn a_delivered_input_is_its_class_not_its_channel() {
        // The delivered-input sub-tag names the class; the channel byte is
        // framing. Today the frame rides the control channel; the move that
        // made hits sheddable — re-homing the family onto the state channel —
        // must leave the classification where the class put it. Both readings
        // land on the same unsheddable lane.
        use crate::channels::{tag, TAG_DELIVERED_INPUT};
        let body = &[TAG_DELIVERED_INPUT, 1, 2, 3];
        assert_eq!(
            lane_of(Channel::Control, &tag(Channel::Control, body)),
            Lane::Control
        );
        assert_eq!(
            lane_of(Channel::State, &tag(Channel::State, body)),
            Lane::Control
        );
    }

    #[test]
    fn the_witness_envelope_names_one_family_on_either_channel() {
        // The log rides state and the meter reads it as witness; range repair
        // shares the envelope on the control transport, where the transport —
        // not a second caller field — puts it in the control lane.
        use crate::channels::{encode_witness, encode_witness_compressed};
        assert_eq!(
            lane_of(Channel::State, &encode_witness(&[1u8, 2, 3])),
            Lane::Witness
        );
        assert_eq!(
            lane_of(
                Channel::State,
                &encode_witness_compressed(&vec![0u8; 4_096])
            ),
            Lane::Witness
        );
        assert_eq!(
            lane_of(Channel::Control, &encode_witness(&[1u8, 2, 3])),
            Lane::Control
        );
        assert_eq!(
            lane_of(Channel::Control, &encode_witness_compressed(&[1u8, 2, 3])),
            Lane::Control
        );
    }

    #[test]
    fn re_homing_a_state_family_onto_the_stream_cannot_buy_it_a_shed() {
        // The families whose home channel is the state datagram keep it in
        // `WireFamily::from_frame`; a pairing the table does not name is not a
        // family, and falls back to the transport's default lane. On the
        // stream that fallback is `Lane::Control` — unsheddable — so moving a
        // hit, a witness anchor, or replication itself onto the stream can
        // only cost its sender accounting, never survival. This is the claim
        // `from_frame`'s doc makes, and it is what lets the delivered input be
        // the one family matched on both channels: every other state family
        // lands here if its channel moves.
        use crate::channels::{tag, TAG_HIT, TAG_REPLICATION, TAG_WITNESS_KEYFRAME};
        for family_tag in [TAG_HIT, TAG_REPLICATION, TAG_WITNESS_KEYFRAME] {
            assert_eq!(
                lane_of(Channel::Control, &tag(Channel::Control, &[family_tag, 0])),
                Lane::Control,
                "sub-tag {family_tag:#04x} on the stream must not fall into a sheddable lane"
            );
        }
    }

    #[test]
    fn a_delta_datagram_is_charged_to_the_replication_lane() {
        use orrery_protocol::channels::{
            encode_delta_patch, encode_replication_delta, ReplicationDelta, TAG_REPLICATION_DELTA,
        };

        let absolute = (0u8..=u8::MAX).collect::<Vec<_>>();
        let delta = ReplicationDelta {
            entity: orrery_protocol::PersistId::new(7),
            tick: 16_384,
            keyframe_age: 60,
            cell: None,
            patch: encode_delta_patch(&absolute, &absolute),
        };
        let encoded = encode_replication_delta(&absolute, &delta);
        let (_, body) = untag(&encoded).expect("state channel tag");
        assert_eq!(body.first(), Some(&TAG_REPLICATION_DELTA));
        assert_eq!(lane_of(Channel::State, &encoded), Lane::Replication);
    }

    #[test]
    fn the_tally_answers_which_lane_spent_the_budget() {
        // "Peak upload was 1006 kbps" does not say which dial to turn, and that
        // was precisely P4's question.
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        meter.charge(budget, ms(0), node(1), Lane::Replication, 800);
        meter.charge(budget, ms(0), node(1), Lane::Witness, 200);
        assert_eq!(meter.lanes.total_bytes(), 1_000);
        assert!((meter.lanes.witness_share() - 0.2).abs() < 1e-9);
        // Every byte still counts against the one ceiling that exists.
        assert_eq!(meter.total_meter(budget).bytes_in_window(ms(0)), 1_000);
        // And an idle peer has not overspent on anything.
        assert_eq!(UploadMeter::default().lanes.witness_share(), 0.0);
    }

    #[test]
    fn a_meter_with_no_traffic_reports_zero_rather_than_panicking() {
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        assert_eq!(meter.rate(budget, ms(0)).bits_per_sec(), 0);
        assert!(!meter.would_exceed(budget, ms(0), 100));
    }

    #[test]
    fn a_clock_that_goes_backwards_resets_rather_than_corrupting() {
        // `Time<Real>` does not go backwards, but the meter takes an explicit
        // instant so it can be tested; a rewind must not roll the ring the
        // wrong way and resurrect aged-out buckets.
        let budget = UploadBudget::default();
        let mut meter = UploadMeter::default();
        meter.record(budget, ms(5_000), node(1), 1_000);
        assert!(meter.rate(budget, ms(5_000)).bits_per_sec() > 0);
        assert_eq!(meter.rate(budget, ms(10)).bits_per_sec(), 0);
    }

    #[test]
    fn display_reads_in_the_unit_the_budget_is_written_in() {
        assert_eq!(Bandwidth::from_mbps(1).to_string(), "1.00 Mbps");
        assert_eq!(Bandwidth::from_kbps(250).to_string(), "250.0 kbps");
    }
}
