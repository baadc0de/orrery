//! Bounded-memory latency histogram for the D16 latency targets (D16).
//!
//! A fixed-bucket histogram covering the microsecond-to-second range required
//! by the four D16 latency targets:
//!
//! - journal commit < 2 ms
//! - bulk ack p99 < 5 ms
//! - intent commit p99 < 10 ms
//! - area first-page-in < 50 ms
//!
//! The bucket boundaries and the D16 series names are **not** defined here.
//! They live in [`orrery_protocol::metrics`], the one definition every
//! producer and consumer of the P2 artifact shares — the journal recorder, the
//! gateway's server-side timer, this histogram, and the `p2-dashboard` gate.
//! This module re-exports them so a consumer whose only dependency is this
//! crate reaches the contract without a new dependency edge.
//!
//! Memory is constant (independent of sample count). The histogram is
//! mergeable so per-process histograms can be combined into a global view.
//!
//! Do NOT introduce a `Vec`-and-sort approach here: a 30-minute run at 10k
//! entities × 4 Hz would produce ~72M samples, which does not fit in memory.

use std::time::Duration;

pub use orrery_protocol::metrics::{
    bucket_index, bucket_upper_us, is_known_series, GATED_SERIES, LATENCY_BOUNDARIES_US,
    NUM_LATENCY_BUCKETS, SERIES_AREA_FIRST_PAGE, SERIES_BULK_ACK, SERIES_GATEWAY_BULK_SERVER,
    SERIES_INTENT_COMMIT, SERIES_JOURNAL_COMMIT, UNGATED_SERIES,
};

/// The number of bucket boundaries.
const NUM_BOUNDARIES: usize = LATENCY_BOUNDARIES_US.len();

/// The number of buckets (boundaries + 1 for the overflow bucket).
const NUM_BUCKETS: usize = NUM_LATENCY_BUCKETS;

/// A bounded-memory latency histogram with fixed bucket boundaries.
///
/// Records latency samples into buckets whose boundaries are chosen to cover
/// the four D16 targets with several buckets per target. Memory is constant
/// (one `u64` per bucket, `NUM_LATENCY_BUCKETS` of them) regardless of
/// sample count.
///
/// # Merge
///
/// Two histograms with the same bucket layout can be combined via
/// [`merge`](Self::merge), enabling per-process histograms to be aggregated
/// into a global view.
///
/// # Percentiles
///
/// `p50`, `p90`, and `p99` return the upper bound of the bucket containing the
/// requested percentile, which is guaranteed to be within one bucket width of
/// the true value. The actual maximum is tracked separately.
#[derive(Debug, Clone)]
pub struct LatencyHistogram {
    /// Per-bucket counters. Bucket `i` covers `[LATENCY_BOUNDARIES_US[i-1], LATENCY_BOUNDARIES_US[i])`
    /// for `i < NUM_BOUNDARIES`, and `[LATENCY_BOUNDARIES_US[NUM_BOUNDARIES-1], ∞)` for the
    /// overflow bucket at index `NUM_BOUNDARIES`.
    buckets: [u64; NUM_BUCKETS],
    /// Total number of samples recorded.
    total: u64,
    /// The maximum observed latency (or `None` if no samples).
    max: Option<Duration>,
    /// The minimum observed latency (or `None` if no samples).
    min: Option<Duration>,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; NUM_BUCKETS],
            total: 0,
            max: None,
            min: None,
        }
    }
}

impl LatencyHistogram {
    /// A new, empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a latency sample.
    pub fn record(&mut self, latency: Duration) {
        let micros = latency.as_micros() as u64;
        // The shared bucket predicate, so a sample recorded here lands in the
        // same bucket the journal recorder and the gateway would give it.
        let idx = bucket_index(micros);
        // `bucket_index` returns at most `NUM_BOUNDARIES`, the overflow
        // bucket, so this index is always in range.
        self.buckets[idx] += 1;
        self.total += 1;

        // Track min/max.
        match self.max {
            Some(max) if latency > max => self.max = Some(latency),
            None => self.max = Some(latency),
            Some(_) => {}
        }
        match self.min {
            Some(min) if latency < min => self.min = Some(latency),
            None => self.min = Some(latency),
            Some(_) => {}
        }
    }

    /// The total number of samples recorded.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The approximate p50 latency (within one bucket width).
    ///
    /// Returns the upper bound of the bucket containing the median sample.
    /// Returns `Duration::ZERO` if no samples have been recorded.
    #[must_use]
    pub fn p50(&self) -> Duration {
        self.percentile(0.50)
    }

    /// The approximate p90 latency (within one bucket width).
    ///
    /// Returns `Duration::ZERO` if no samples have been recorded.
    #[must_use]
    pub fn p90(&self) -> Duration {
        self.percentile(0.90)
    }

    /// The approximate p99 latency (within one bucket width).
    ///
    /// Returns `Duration::ZERO` if no samples have been recorded.
    #[must_use]
    pub fn p99(&self) -> Duration {
        self.percentile(0.99)
    }

    /// The maximum observed latency.
    #[must_use]
    pub fn max(&self) -> Option<Duration> {
        self.max
    }

    /// The minimum observed latency.
    #[must_use]
    pub fn min(&self) -> Option<Duration> {
        self.min
    }

    /// The upper bound of the bucket containing the `pct` percentile.
    ///
    /// For example, `pct = 0.95` returns the 95th percentile. Returns the upper
    /// bound of the bucket (or `Duration::ZERO` if no samples). The result is
    /// within one bucket width of the true percentile value.
    fn percentile(&self, pct: f64) -> Duration {
        if self.total == 0 {
            return Duration::ZERO;
        }
        let target = (self.total as f64 * pct).ceil() as u64;
        let mut cumulative = 0u64;
        for (i, &count) in self.buckets.iter().enumerate() {
            cumulative += count;
            if cumulative >= target {
                if i < NUM_BOUNDARIES {
                    // The shared reconstruction rule: the bucket's upper bound.
                    return Duration::from_micros(bucket_upper_us(i, 0));
                }
                // Overflow bucket: return the tracked max, or Duration::MAX if none.
                return self.max.unwrap_or(Duration::MAX);
            }
        }
        Duration::ZERO
    }

    /// Merge another histogram into this one.
    ///
    /// Both histograms must have the same bucket layout (they do, since the
    /// layout is fixed by the module constants). After merging, this histogram
    /// contains the combined samples.
    pub fn merge(&mut self, other: &Self) {
        for (i, &count) in other.buckets.iter().enumerate() {
            self.buckets[i] += count;
        }
        self.total += other.total;
        match (self.max, other.max) {
            (Some(a), Some(b)) => self.max = Some(a.max(b)),
            (None, Some(b)) => self.max = Some(b),
            _ => {}
        }
        match (self.min, other.min) {
            (Some(a), Some(b)) => self.min = Some(a.min(b)),
            (None, Some(b)) => self.min = Some(b),
            _ => {}
        }
    }

    /// Reset the histogram, clearing all samples.
    pub fn reset(&mut self) {
        self.buckets = [0; NUM_BUCKETS];
        self.total = 0;
        self.max = None;
        self.min = None;
    }

    /// The number of buckets.
    ///
    /// Useful for diagnostic display.
    #[must_use]
    pub fn num_buckets(&self) -> usize {
        NUM_BUCKETS
    }

    /// Raw bucket counters (for testing).
    #[must_use]
    pub fn buckets(&self) -> &[u64; NUM_BUCKETS] {
        &self.buckets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_percentiles_are_within_one_bucket() {
        // Feed a known distribution: 1000 samples at 1 ms, 1000 at 5 ms,
        // 1000 at 20 ms, 1000 at 100 ms.
        let mut hist = LatencyHistogram::new();
        for _ in 0..1000 {
            hist.record(Duration::from_micros(1000)); // 1 ms
        }
        for _ in 0..1000 {
            hist.record(Duration::from_micros(5000)); // 5 ms
        }
        for _ in 0..1000 {
            hist.record(Duration::from_micros(20_000)); // 20 ms
        }
        for _ in 0..1000 {
            hist.record(Duration::from_micros(100_000)); // 100 ms
        }

        // Total: 4000 samples.
        // p50 (median) is at sample 2000, which falls in the 5 ms bucket
        // (5000 µs boundary). True p50 is 5 ms.
        let p50 = hist.p50();
        assert!(
            p50 >= Duration::from_micros(2000),
            "p50 should be >= 2 ms, got {p50:?}"
        );
        assert!(
            p50 <= Duration::from_micros(5000),
            "p50 should be <= 5 ms, got {p50:?}"
        );

        // p90 is at sample 3600, which falls in the 100 ms bucket
        // (100000 µs boundary). True p90 is 100 ms.
        let p90 = hist.p90();
        assert!(
            p90 >= Duration::from_micros(50_000),
            "p90 should be >= 50 ms, got {p90:?}"
        );
        assert!(
            p90 <= Duration::from_micros(100_000),
            "p90 should be <= 100 ms, got {p90:?}"
        );

        // p99 is at sample 3960, which falls in the 100 ms bucket.
        let p99 = hist.p99();
        assert!(
            p99 >= Duration::from_micros(50_000),
            "p99 should be >= 50 ms, got {p99:?}"
        );
        assert!(
            p99 <= Duration::from_micros(100_000),
            "p99 should be <= 100 ms, got {p99:?}"
        );

        // Max is 100 ms.
        assert_eq!(hist.max(), Some(Duration::from_micros(100_000)));
    }

    #[test]
    fn recorder_memory_is_constant() {
        // The struct size is independent of sample count.
        let empty = LatencyHistogram::new();
        let size_empty = std::mem::size_of_val(&empty);

        let mut full = LatencyHistogram::new();
        for _ in 0..1_000_000 {
            full.record(Duration::from_micros(42));
        }
        let size_full = std::mem::size_of_val(&full);

        assert_eq!(
            size_empty, size_full,
            "histogram memory must not grow with sample count"
        );
    }

    #[test]
    fn merge_combines_histograms() {
        let mut a = LatencyHistogram::new();
        let mut b = LatencyHistogram::new();
        for _ in 0..100 {
            a.record(Duration::from_micros(1000));
        }
        for _ in 0..100 {
            b.record(Duration::from_micros(5000));
        }
        a.merge(&b);
        assert_eq!(a.total(), 200);
        // p50 should be in the 5 ms bucket (since 100 samples at 1 ms + 100 at 5 ms,
        // median sample 100 is in the 5 ms bucket).
        let p50 = a.p50();
        assert!(
            p50 >= Duration::from_micros(1000),
            "p50 after merge should be >= 1 ms, got {p50:?}"
        );
    }

    #[test]
    fn empty_histogram_returns_zero_percentiles() {
        let hist = LatencyHistogram::new();
        assert_eq!(hist.p50(), Duration::ZERO);
        assert_eq!(hist.p90(), Duration::ZERO);
        assert_eq!(hist.p99(), Duration::ZERO);
        assert_eq!(hist.max(), None);
        assert_eq!(hist.min(), None);
    }

    #[test]
    fn reset_clears_all_samples() {
        let mut hist = LatencyHistogram::new();
        hist.record(Duration::from_micros(1000));
        hist.record(Duration::from_micros(2000));
        assert_eq!(hist.total(), 2);
        hist.reset();
        assert_eq!(hist.total(), 0);
        assert_eq!(hist.max(), None);
        assert_eq!(hist.p50(), Duration::ZERO);
    }

    #[test]
    fn single_sample_tracks_min_and_max() {
        let mut hist = LatencyHistogram::new();
        hist.record(Duration::from_micros(5000));
        assert_eq!(hist.min(), Some(Duration::from_micros(5000)));
        assert_eq!(hist.max(), Some(Duration::from_micros(5000)));
    }

    #[test]
    fn producer_and_consumer_agree_on_the_bucket() {
        // The round trip the P2 artifact is: a producer records a sample,
        // drains the bucket at its reported upper bound, writes that number
        // into JSONL, and the gate re-records it. Both sides must then report
        // the same percentile, or the gate is reading a different number from
        // the one the rig measured.
        for micros in [
            1, 49, 50, 51, 999, 1_000, 1_100, 1_500, 1_999, 2_000, 4_999, 30_001, 999_999,
        ] {
            let mut producer = LatencyHistogram::new();
            producer.record(Duration::from_micros(micros));
            let reported = producer.p99();

            let mut consumer = LatencyHistogram::new();
            consumer.record(reported);
            assert_eq!(
                consumer.p99(),
                reported,
                "{micros} µs reported as {reported:?} must re-read to the same bucket"
            );
        }
    }

    #[test]
    fn the_journal_band_no_longer_collapses_onto_the_gate_threshold() {
        // The defect the shared lattice fixes: with 1 ms and 2 ms adjacent,
        // every journal p99 anywhere in the 1.0–2.0 ms band reported as
        // exactly the 2 ms D16 threshold, and passed on the equality case.
        let mut hist = LatencyHistogram::new();
        for _ in 0..100 {
            hist.record(Duration::from_micros(1_100));
        }
        assert_eq!(hist.p99(), Duration::from_micros(1_250));
        assert!(hist.p99() < Duration::from_micros(2_000));
    }
}
