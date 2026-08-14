//! Low-overhead journal commit-latency telemetry (D16).
//!
//! This recorder measures the time from entering [`Journal::append`] through
//! durable resolution of its [`AppendHandle`].  It deliberately keeps only
//! fixed bucket counters: recording is one relaxed counter increment (plus a
//! CAS only when a new maximum is observed), and reporting can emit compact
//! `{ value_us, count }` batches for the P2 JSONL artifact.
//!
//! [`Journal::append`]: super::Journal::append
//! [`AppendHandle`]: super::AppendHandle

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Boundaries shared with the P2 latency artifact's D16 histogram.
///
/// A sample belongs to the first boundary greater than or equal to it. The
/// final bucket is overflow and is serialized at the observed maximum.
const BOUNDARIES_US: [u64; 22] = [
    50, 100, 200, 500, 1_000, 2_000, 3_000, 5_000, 7_000, 10_000, 15_000, 20_000, 30_000, 50_000,
    75_000, 100_000, 150_000, 200_000, 300_000, 500_000, 750_000, 1_000_000,
];
const NUM_BUCKETS: usize = BOUNDARIES_US.len() + 1;

/// One compact journal-latency batch, suitable for a `sample_batch` JSONL
/// record. `value_us` is the bucket's upper bound; overflow uses the observed
/// maximum, matching the P2 dashboard's reconstruction semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalCommitSample {
    /// Bucket upper bound, in microseconds (or observed maximum for overflow).
    pub value_us: u64,
    /// Number of committed appends in this bucket.
    pub count: u64,
}

/// A point-in-time, bounded-memory view of journal commit telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalCommitSnapshot {
    buckets: [u64; NUM_BUCKETS],
    max_us: u64,
}

impl JournalCommitSnapshot {
    /// The total number of successfully durable appends in this snapshot.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// Non-empty bucket batches in ascending latency order.
    #[must_use]
    pub fn samples(&self) -> Vec<JournalCommitSample> {
        samples_for(&self.buckets, self.max_us)
    }
}

/// Thread-safe, fixed-size journal commit recorder.
#[derive(Debug)]
pub struct JournalCommitMetrics {
    buckets: [AtomicU64; NUM_BUCKETS],
    max_us: AtomicU64,
}

impl Default for JournalCommitMetrics {
    fn default() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; NUM_BUCKETS],
            max_us: AtomicU64::new(0),
        }
    }
}

impl JournalCommitMetrics {
    /// Create an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture a coherent-enough monotonic snapshot for telemetry export.
    #[must_use]
    pub fn snapshot(&self) -> JournalCommitSnapshot {
        JournalCommitSnapshot {
            buckets: self
                .buckets
                .each_ref()
                .map(|bucket| bucket.load(Ordering::Relaxed)),
            max_us: self.max_us.load(Ordering::Relaxed),
        }
    }

    /// Return batches recorded since `previous`, then advance that cursor.
    ///
    /// A caller owns its cursor, so independent JSONL writers can drain the
    /// same journal without coordinating. Counts are monotonic; the relaxed
    /// loads may defer a concurrent append to the next drain, but never invent
    /// one or make the hot append path wait for telemetry.
    pub fn delta(&self, previous: &mut JournalCommitSnapshot) -> Vec<JournalCommitSample> {
        let current = self.snapshot();
        let mut delta = [0; NUM_BUCKETS];
        for (out, (now, before)) in delta
            .iter_mut()
            .zip(current.buckets.iter().zip(previous.buckets.iter()))
        {
            *out = now.saturating_sub(*before);
        }
        *previous = current.clone();
        samples_for(&delta, current.max_us)
    }

    pub(crate) fn record(&self, latency: Duration) {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        let index = BOUNDARIES_US.partition_point(|&boundary| micros > boundary);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
    }
}

fn samples_for(buckets: &[u64; NUM_BUCKETS], max_us: u64) -> Vec<JournalCommitSample> {
    buckets
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| {
            (count != 0).then_some(JournalCommitSample {
                value_us: BOUNDARIES_US.get(index).copied().unwrap_or(max_us),
                count,
            })
        })
        .collect()
}

pub(crate) type SharedJournalCommitMetrics = Arc<JournalCommitMetrics>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_use_p2_bucket_boundaries_and_overflow_maximum() {
        let metrics = JournalCommitMetrics::new();
        metrics.record(Duration::from_micros(1_500));
        metrics.record(Duration::from_micros(2_000));
        metrics.record(Duration::from_micros(2_001));
        metrics.record(Duration::from_secs(2));

        assert_eq!(
            metrics.snapshot().samples(),
            vec![
                JournalCommitSample {
                    value_us: 2_000,
                    count: 2
                },
                JournalCommitSample {
                    value_us: 3_000,
                    count: 1
                },
                JournalCommitSample {
                    value_us: 2_000_000,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn delta_is_cumulative_cursor_and_does_not_repeat_samples() {
        let metrics = JournalCommitMetrics::new();
        let mut cursor = metrics.snapshot();
        metrics.record(Duration::from_micros(900));
        metrics.record(Duration::from_micros(1_500));
        assert_eq!(
            metrics.delta(&mut cursor),
            vec![
                JournalCommitSample {
                    value_us: 1_000,
                    count: 1
                },
                JournalCommitSample {
                    value_us: 2_000,
                    count: 1
                },
            ]
        );
        assert!(metrics.delta(&mut cursor).is_empty());

        metrics.record(Duration::from_micros(1_500));
        assert_eq!(
            metrics.delta(&mut cursor),
            vec![JournalCommitSample {
                value_us: 2_000,
                count: 1
            }]
        );
    }
}
