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
//! It enforces the *ceiling*: over budget, [`Channel::State`] packets are shed
//! and [`Channel::Control`] packets always pass. That asymmetry is the same one
//! gap repair rests on — state loss is expected and repaired, while shedding a
//! repair or a lease operation turns one dropped datagram into a permanent
//! hole.
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

use crate::channels::Channel;
use orrery_protocol::NodeId;

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
    /// Control packets sent while over budget — never shed, always counted.
    ///
    /// A non-zero value here with `shed` climbing is the honest picture of an
    /// oversubscribed peer: the reliable lane is still being paid for, so the
    /// overrun is real rather than an artefact of the backstop.
    pub control_over_budget: u64,
    /// Whether the last send pass was over budget.
    ///
    /// docs/03-replication.md §9.3: sustained oversubscription across an
    /// island's links is a promotion signal alongside raw population, so this is
    /// reported rather than merely acted on.
    pub oversubscribed: bool,
}

impl UploadMeter {
    /// Charge a packet of `payload` bytes to `peer` at `now`, with wire
    /// overhead included.
    pub fn record(&mut self, budget: UploadBudget, now: Duration, peer: NodeId, payload: usize) {
        self.record_wire(budget, now, peer, datagram_wire_bytes(payload));
    }

    /// Charge an already-costed send of `wire` bytes to `peer` at `now`.
    ///
    /// The two lanes cost differently — see [`stream_wire_bytes`] — so the
    /// caller that knows which lane it is on computes the figure and this
    /// records it.
    pub fn record_wire(&mut self, budget: UploadBudget, now: Duration, peer: NodeId, wire: u64) {
        self.total_meter(budget).record(now, wire);
        self.peer_meter(budget, peer).record(now, wire);
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

/// Whether a packet on `channel` may be shed when the budget is spent.
///
/// State only. Shedding a control packet would turn one dropped datagram into a
/// permanent hole — a gap repair that never arrives is indistinguishable from an
/// authority refusing to answer, and a lease operation that never arrives is a
/// stall rather than a retry. Loss on the state lane is expected and already
/// has a repair path.
#[must_use]
pub const fn is_sheddable(channel: Channel) -> bool {
    matches!(channel, Channel::State)
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
    fn only_state_is_sheddable() {
        // Shedding a repair or a lease operation turns one dropped datagram
        // into a permanent hole; state loss is expected and already repaired.
        assert!(is_sheddable(Channel::State));
        assert!(!is_sheddable(Channel::Control));
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
