//! The traffic the control lane actually carries, and how it is measured.
//!
//! Three classes, taken from docs/02-networking.md §7 rather than invented:
//!
//! | class | size | rate | what it is |
//! |---|---|---|---|
//! | [`Class::State`] | 500 B | 20 Hz | replication + witness frames — the flow that is *always* there and that everything else competes with |
//! | [`Class::Sparse`] | 120 B | 4 Hz | lease traffic, handoff acks, manifest deltas. "Must arrive, order matters, tiny volume" |
//! | [`Class::Repair`] | 40 kB | 2 Hz | a `LogRangeResponse` filling a one-second hole. Bursty, and the reason this benchmark exists |
//!
//! The point of running all three at once is that the interesting failures are
//! *between* classes. A repair alone looks fine on any transport; a repair that
//! delays a lease op by a round trip is the cost a shared stream actually
//! charges, and it is invisible unless both are in flight together.
//!
//! # What is measured
//!
//! Every message carries its class, a sequence number and its send timestamp,
//! so the receiver computes completion latency without a clock exchange — both
//! peers are in one process, so the two `Instant`s are the same clock. Latency
//! is measured to the message being *whole*: a repair that arrives in thirty
//! packets is complete when the last one lands, which is what a witness waits
//! for and what a stream lane delivers as one unit.

use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};

/// What a message is, and therefore what its latency means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Replication and witness frames. Unreliable by design; loss is expected.
    State,
    /// Sparse ordered control: lease traffic, handoff acks, manifest deltas.
    Sparse,
    /// A gap repair: a log range response.
    Repair,
}

impl Class {
    /// The class byte on the wire.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::State => 0,
            Self::Sparse => 1,
            Self::Repair => 2,
        }
    }

    /// The class a wire byte names.
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::State),
            1 => Some(Self::Sparse),
            2 => Some(Self::Repair),
            _ => None,
        }
    }

    /// How this class prints in a report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Sparse => "sparse",
            Self::Repair => "repair",
        }
    }
}

/// Payload size per class, in bytes.
pub const STATE_BYTES: usize = 500;
/// See [`STATE_BYTES`].
pub const SPARSE_BYTES: usize = 120;
/// A `LogRangeResponse` covering a one-second hole: 20 frames at ~2 kB.
pub const REPAIR_BYTES: usize = 40_000;

/// Send cadences, in sends per second.
pub const STATE_HZ: u32 = 20;
/// See [`STATE_HZ`].
pub const SPARSE_HZ: u32 = 4;
/// See [`STATE_HZ`]. A default; `--repair-hz` moves it, because how close the
/// repair stream comes to filling the link is itself a variable worth sweeping.
pub const REPAIR_HZ: u32 = 2;

/// The fixed part of every payload: class, sequence, send timestamp.
const HEADER_LEN: usize = 1 + 4 + 16;

/// Build one message of `class`, numbered `seq`, stamped `sent` and padded to
/// `len`.
///
/// # Panics
///
/// Panics if `len` is smaller than the header, which is a programming error
/// rather than a runtime condition — the class sizes are constants.
#[must_use]
pub fn encode(class: Class, seq: u32, sent: Instant, origin: Instant, len: usize) -> Bytes {
    assert!(len >= HEADER_LEN, "a message must carry its own header");
    let mut buf = BytesMut::with_capacity(len);
    buf.put_u8(class.tag());
    buf.put_u32_le(seq);
    // Nanoseconds since the run's origin rather than since the epoch: an
    // `Instant` has no wire representation, and a run is minutes long.
    buf.put_u128_le(sent.saturating_duration_since(origin).as_nanos());
    buf.resize(len, 0xA5);
    buf.freeze()
}

/// What a received payload was.
#[derive(Debug, Clone, Copy)]
pub struct Received {
    /// Which class it belongs to.
    pub class: Class,
    /// Its sequence number within that class. Carried so a duplicate or an
    /// out-of-order arrival is identifiable while debugging a transport.
    #[allow(dead_code)]
    pub seq: u32,
    /// How long it took to arrive, whole.
    pub latency: Duration,
}

/// Read a payload back, given the run's origin and the arrival instant.
#[must_use]
pub fn decode(payload: &[u8], origin: Instant, arrived: Instant) -> Option<Received> {
    if payload.len() < HEADER_LEN {
        return None;
    }
    let class = Class::from_tag(payload[0])?;
    let seq = u32::from_le_bytes(payload[1..5].try_into().ok()?);
    let sent_nanos = u128::from_le_bytes(payload[5..21].try_into().ok()?);
    let sent = origin.checked_add(Duration::from_nanos(u64::try_from(sent_nanos).ok()?))?;
    Some(Received {
        class,
        seq,
        latency: arrived.saturating_duration_since(sent),
    })
}

/// Latency samples for one class, kept whole so quantiles are exact.
///
/// A run is minutes of a few hundred hertz, so this is tens of thousands of
/// `Duration`s — small enough to sort, and sorting means a p99 that is the
/// actual 99th sample rather than a bucket boundary. The tail is the subject
/// here, so approximating it would be approximating the answer.
#[derive(Debug, Default)]
pub struct Samples {
    latencies: Vec<Duration>,
}

impl Samples {
    /// Record one completion.
    pub fn record(&mut self, latency: Duration) {
        self.latencies.push(latency);
    }

    /// How many completed.
    #[must_use]
    pub fn count(&self) -> usize {
        self.latencies.len()
    }

    /// The `q`-quantile, `q` in 0.0–1.0. `None` if nothing was recorded.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        let index = ((sorted.len() - 1) as f64 * q).round() as usize;
        sorted.get(index).copied()
    }

    /// The worst sample.
    #[must_use]
    pub fn max(&self) -> Option<Duration> {
        self.latencies.iter().copied().max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_class_tag_can_be_mistaken_for_a_chunk_header() {
        // The two share a lane on the datagram baseline, and the receiver tells
        // them apart by the first byte. A collision reassembles state datagrams
        // as repair chunks and reports zero completions everywhere.
        for class in [Class::State, Class::Sparse, Class::Repair] {
            assert!(
                class.tag() < 0xC0,
                "{} collides with a chunk kind",
                class.name()
            );
        }
    }

    #[test]
    fn a_message_round_trips_through_its_own_encoding() {
        let origin = Instant::now();
        let sent = origin + Duration::from_millis(1_500);
        let arrived = sent + Duration::from_millis(37);
        let payload = encode(Class::Repair, 42, sent, origin, REPAIR_BYTES);
        assert_eq!(payload.len(), REPAIR_BYTES);

        let received = decode(&payload, origin, arrived).expect("decodes");
        assert_eq!(received.class, Class::Repair);
        assert_eq!(received.seq, 42);
        assert_eq!(received.latency, Duration::from_millis(37));
    }

    #[test]
    fn a_truncated_payload_decodes_to_nothing_rather_than_a_wrong_latency() {
        // A datagram-lane chunk is a fragment of a message, and counting one as
        // a completion is exactly how the status quo would flatter itself.
        let origin = Instant::now();
        let payload = encode(Class::Sparse, 1, origin, origin, SPARSE_BYTES);
        assert!(decode(&payload[..8], origin, origin).is_none());
    }

    #[test]
    fn quantiles_come_from_the_samples_not_from_buckets() {
        let mut samples = Samples::default();
        for ms in 1..=100 {
            samples.record(Duration::from_millis(ms));
        }
        assert_eq!(samples.count(), 100);
        // Nearest-rank: the median of 100 samples is the 51st, not an
        // interpolation between the 50th and the 51st. Either convention is
        // defensible; this one never invents a value that was not measured.
        assert_eq!(samples.quantile(0.5), Some(Duration::from_millis(51)));
        assert_eq!(samples.quantile(0.99), Some(Duration::from_millis(99)));
        assert_eq!(samples.max(), Some(Duration::from_millis(100)));
    }
}
