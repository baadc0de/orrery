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
const BOUNDARIES_US: [u64; 25] = [
    50, 100, 200, 500, 1_000, 1_250, 1_500, 1_750, 2_000, 3_000, 5_000, 7_000, 10_000, 15_000,
    20_000, 30_000, 50_000, 75_000, 100_000, 150_000, 200_000, 300_000, 500_000, 750_000,
    1_000_000,
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

/// Aggregate, fixed-memory measurements for completed group-commit flushes.
///
/// Sums make interval averages available without recording one event per
/// flush, while maxima expose tail excursions during diagnostic trials.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalStageSnapshot {
    /// Successful durability flushes.
    pub flushes: u64,
    /// Records included in those flushes.
    pub records: u64,
    /// Encoded key/value bytes included in those flushes.
    pub bytes: u64,
    /// Sum of oldest-record queue waits, in microseconds.
    pub queue_wait_us_sum: u64,
    /// Cumulative maximum oldest-record queue wait, in microseconds.
    pub queue_wait_us_max: u64,
    /// Sum of waits for a blocking worker to start, in microseconds.
    pub blocking_dispatch_us_sum: u64,
    /// Cumulative maximum blocking-worker dispatch wait, in microseconds.
    pub blocking_dispatch_us_max: u64,
    /// Sum of Fjall batch-commit calls, in microseconds.
    pub fjall_batch_commit_us_sum: u64,
    /// Cumulative maximum Fjall batch-commit call, in microseconds.
    pub fjall_batch_commit_us_max: u64,
    /// Sum of `SyncData` calls, in microseconds.
    pub sync_data_us_sum: u64,
    /// Cumulative maximum `SyncData` call, in microseconds.
    pub sync_data_us_max: u64,
    /// Sum of waiter-resolution and publication work, in microseconds.
    pub resolve_us_sum: u64,
    /// Cumulative maximum waiter-resolution/publication work, in microseconds.
    pub resolve_us_max: u64,
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
    stages: JournalStageCounters,
}

#[derive(Debug, Default)]
struct JournalStageCounters {
    flushes: AtomicU64,
    records: AtomicU64,
    bytes: AtomicU64,
    queue_wait_us_sum: AtomicU64,
    queue_wait_us_max: AtomicU64,
    blocking_dispatch_us_sum: AtomicU64,
    blocking_dispatch_us_max: AtomicU64,
    fjall_batch_commit_us_sum: AtomicU64,
    fjall_batch_commit_us_max: AtomicU64,
    sync_data_us_sum: AtomicU64,
    sync_data_us_max: AtomicU64,
    resolve_us_sum: AtomicU64,
    resolve_us_max: AtomicU64,
}

impl Default for JournalCommitMetrics {
    fn default() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; NUM_BUCKETS],
            max_us: AtomicU64::new(0),
            stages: JournalStageCounters::default(),
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

    /// Capture cumulative group-commit stage counters.
    #[must_use]
    pub fn stage_snapshot(&self) -> JournalStageSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        JournalStageSnapshot {
            flushes: load(&self.stages.flushes),
            records: load(&self.stages.records),
            bytes: load(&self.stages.bytes),
            queue_wait_us_sum: load(&self.stages.queue_wait_us_sum),
            queue_wait_us_max: load(&self.stages.queue_wait_us_max),
            blocking_dispatch_us_sum: load(&self.stages.blocking_dispatch_us_sum),
            blocking_dispatch_us_max: load(&self.stages.blocking_dispatch_us_max),
            fjall_batch_commit_us_sum: load(&self.stages.fjall_batch_commit_us_sum),
            fjall_batch_commit_us_max: load(&self.stages.fjall_batch_commit_us_max),
            sync_data_us_sum: load(&self.stages.sync_data_us_sum),
            sync_data_us_max: load(&self.stages.sync_data_us_max),
            resolve_us_sum: load(&self.stages.resolve_us_sum),
            resolve_us_max: load(&self.stages.resolve_us_max),
        }
    }

    /// Return group-commit counters added since `previous`.
    pub fn stage_delta(&self, previous: &mut JournalStageSnapshot) -> JournalStageSnapshot {
        let current = self.stage_snapshot();
        let delta = JournalStageSnapshot {
            flushes: current.flushes.saturating_sub(previous.flushes),
            records: current.records.saturating_sub(previous.records),
            bytes: current.bytes.saturating_sub(previous.bytes),
            queue_wait_us_sum: current
                .queue_wait_us_sum
                .saturating_sub(previous.queue_wait_us_sum),
            queue_wait_us_max: current.queue_wait_us_max,
            blocking_dispatch_us_sum: current
                .blocking_dispatch_us_sum
                .saturating_sub(previous.blocking_dispatch_us_sum),
            blocking_dispatch_us_max: current.blocking_dispatch_us_max,
            fjall_batch_commit_us_sum: current
                .fjall_batch_commit_us_sum
                .saturating_sub(previous.fjall_batch_commit_us_sum),
            fjall_batch_commit_us_max: current.fjall_batch_commit_us_max,
            sync_data_us_sum: current
                .sync_data_us_sum
                .saturating_sub(previous.sync_data_us_sum),
            sync_data_us_max: current.sync_data_us_max,
            resolve_us_sum: current
                .resolve_us_sum
                .saturating_sub(previous.resolve_us_sum),
            resolve_us_max: current.resolve_us_max,
        };
        *previous = current;
        delta
    }

    pub(crate) fn record_group(&self, sample: JournalStageSnapshot) {
        let add = |counter: &AtomicU64, value| {
            counter.fetch_add(value, Ordering::Relaxed);
        };
        add(&self.stages.flushes, 1);
        add(&self.stages.records, sample.records);
        add(&self.stages.bytes, sample.bytes);
        add(&self.stages.queue_wait_us_sum, sample.queue_wait_us_sum);
        self.stages
            .queue_wait_us_max
            .fetch_max(sample.queue_wait_us_max, Ordering::Relaxed);
        add(
            &self.stages.blocking_dispatch_us_sum,
            sample.blocking_dispatch_us_sum,
        );
        self.stages
            .blocking_dispatch_us_max
            .fetch_max(sample.blocking_dispatch_us_max, Ordering::Relaxed);
        add(
            &self.stages.fjall_batch_commit_us_sum,
            sample.fjall_batch_commit_us_sum,
        );
        self.stages
            .fjall_batch_commit_us_max
            .fetch_max(sample.fjall_batch_commit_us_max, Ordering::Relaxed);
        add(&self.stages.sync_data_us_sum, sample.sync_data_us_sum);
        self.stages
            .sync_data_us_max
            .fetch_max(sample.sync_data_us_max, Ordering::Relaxed);
        add(&self.stages.resolve_us_sum, sample.resolve_us_sum);
        self.stages
            .resolve_us_max
            .fetch_max(sample.resolve_us_max, Ordering::Relaxed);
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
                    value_us: 1_500,
                    count: 1
                },
                JournalCommitSample {
                    value_us: 2_000,
                    count: 1
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
                    value_us: 1_500,
                    count: 1
                },
            ]
        );
        assert!(metrics.delta(&mut cursor).is_empty());

        metrics.record(Duration::from_micros(1_500));
        assert_eq!(
            metrics.delta(&mut cursor),
            vec![JournalCommitSample {
                value_us: 1_500,
                count: 1
            }]
        );
    }

    #[test]
    fn stage_delta_reports_interval_totals_and_cumulative_maxima() {
        let metrics = JournalCommitMetrics::new();
        let mut cursor = metrics.stage_snapshot();
        metrics.record_group(JournalStageSnapshot {
            records: 12,
            bytes: 345,
            queue_wait_us_sum: 250,
            queue_wait_us_max: 250,
            blocking_dispatch_us_sum: 8,
            blocking_dispatch_us_max: 8,
            fjall_batch_commit_us_sum: 40,
            fjall_batch_commit_us_max: 40,
            sync_data_us_sum: 900,
            sync_data_us_max: 900,
            resolve_us_sum: 30,
            resolve_us_max: 30,
            ..JournalStageSnapshot::default()
        });

        let delta = metrics.stage_delta(&mut cursor);
        assert_eq!(delta.flushes, 1);
        assert_eq!(delta.records, 12);
        assert_eq!(delta.bytes, 345);
        assert_eq!(delta.sync_data_us_sum, 900);
        assert_eq!(delta.sync_data_us_max, 900);
        assert_eq!(metrics.stage_delta(&mut cursor).flushes, 0);
    }
}
