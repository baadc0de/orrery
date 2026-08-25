//! Low-overhead journal commit-latency telemetry (D16).
//!
//! This recorder measures the time from entering [`Journal::append`] through
//! durable resolution of its [`AppendHandle`].  It deliberately keeps only
//! fixed bucket counters: recording is one relaxed counter increment (plus a
//! CAS only when a new maximum is observed), and reporting can emit compact
//! `{ value_us, count }` batches for the P2 JSONL artifact under the
//! [`SERIES_JOURNAL_COMMIT`] key.
//!
//! [`SERIES_JOURNAL_COMMIT`]: orrery_protocol::metrics::SERIES_JOURNAL_COMMIT
//!
//! [`Journal::append`]: super::Journal::append
//! [`AppendHandle`]: super::AppendHandle

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_protocol::metrics::{bucket_index, bucket_upper_us, NUM_LATENCY_BUCKETS};

/// Buckets, boundaries and the reconstruction rule all come from
/// [`orrery_protocol::metrics`] — the same definition the gateway, the client
/// histogram and the `gates/p2-dashboard` gate use. This module's doc used to claim
/// its own table was "shared with the P2 latency artifact's D16 histogram";
/// it was a fourth copy, and the finest one. It is now the shared table.
const NUM_BUCKETS: usize = NUM_LATENCY_BUCKETS;

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
    /// Encoded bytes carried by the flush that set [`Self::sync_data_us_max`].
    ///
    /// The whole point of the pair (docs/08-persistence.md §4.6): the stall
    /// that sets `journal_commit_ms` p99 survived removing every co-tenant from
    /// the device, on two filesystems, so what is left is either fjall's own
    /// work or the *shape* of the I/O it asks for. Those two predict different
    /// numbers here. A 90 ms barrier carrying an ordinary batch is fjall or the
    /// kernel taking that long over ~4 KB; one carrying megabytes is a memtable
    /// flush or compaction handing a single `fdatasync` far more to persist
    /// than the steady state does.
    pub sync_data_us_max_bytes: u64,
    /// Records carried by the flush that set [`Self::sync_data_us_max`].
    pub sync_data_us_max_records: u64,
    /// Flushes whose `SyncData` reached [`SLOW_SYNC_THRESHOLD_US`].
    ///
    /// The maximum above describes one flush per interval. These three describe
    /// every slow one, so a single unlucky barrier cannot be mistaken for a
    /// population.
    pub slow_syncs: u64,
    /// Encoded bytes summed over the flushes counted by [`Self::slow_syncs`].
    pub slow_sync_bytes_sum: u64,
    /// Records summed over the flushes counted by [`Self::slow_syncs`].
    pub slow_sync_records_sum: u64,
}

/// What counts as a slow durability barrier, in microseconds.
///
/// Ten times the D16 `journal_commit_ms` budget. High enough that a healthy
/// flush on any measured hardware never trips it — the quietest box measured
/// 0.09 ms at p99 and the reference box 3.7 ms — and low enough to catch the
/// 50–240 ms excursions docs/08-persistence.md §4.6 is chasing.
pub const SLOW_SYNC_THRESHOLD_US: u64 = 20_000;

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
    sync_data_us_max_bytes: AtomicU64,
    sync_data_us_max_records: AtomicU64,
    slow_syncs: AtomicU64,
    slow_sync_bytes_sum: AtomicU64,
    slow_sync_records_sum: AtomicU64,
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
            sync_data_us_max_bytes: load(&self.stages.sync_data_us_max_bytes),
            sync_data_us_max_records: load(&self.stages.sync_data_us_max_records),
            slow_syncs: load(&self.stages.slow_syncs),
            slow_sync_bytes_sum: load(&self.stages.slow_sync_bytes_sum),
            slow_sync_records_sum: load(&self.stages.slow_sync_records_sum),
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
            // The bytes travel with the maximum, so they are cumulative in the
            // same sense it is: both describe the worst flush seen so far, not
            // the worst flush in this interval. Reporting the bytes as a
            // difference would be meaningless.
            sync_data_us_max_bytes: current.sync_data_us_max_bytes,
            sync_data_us_max_records: current.sync_data_us_max_records,
            slow_syncs: current.slow_syncs.saturating_sub(previous.slow_syncs),
            slow_sync_bytes_sum: current
                .slow_sync_bytes_sum
                .saturating_sub(previous.slow_sync_bytes_sum),
            slow_sync_records_sum: current
                .slow_sync_records_sum
                .saturating_sub(previous.slow_sync_records_sum),
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
        // `fetch_max` cannot carry the flush's shape with it, and the shape is
        // the measurement (§4.6). A compare-exchange loop keeps the triple
        // consistent: only the caller that installs a new maximum writes the
        // bytes and records beside it. The committer is the single writer of
        // all three, so the loop is exact rather than merely eventually right.
        let mut observed = self.stages.sync_data_us_max.load(Ordering::Relaxed);
        while sample.sync_data_us_max > observed {
            match self.stages.sync_data_us_max.compare_exchange_weak(
                observed,
                sample.sync_data_us_max,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.stages
                        .sync_data_us_max_bytes
                        .store(sample.bytes, Ordering::Relaxed);
                    self.stages
                        .sync_data_us_max_records
                        .store(sample.records, Ordering::Relaxed);
                    break;
                }
                Err(current) => observed = current,
            }
        }
        if sample.sync_data_us_max >= SLOW_SYNC_THRESHOLD_US {
            add(&self.stages.slow_syncs, 1);
            add(&self.stages.slow_sync_bytes_sum, sample.bytes);
            add(&self.stages.slow_sync_records_sum, sample.records);
        }
        add(&self.stages.resolve_us_sum, sample.resolve_us_sum);
        self.stages
            .resolve_us_max
            .fetch_max(sample.resolve_us_max, Ordering::Relaxed);
    }

    pub(crate) fn record(&self, latency: Duration) {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        let index = bucket_index(micros);
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
                value_us: bucket_upper_us(index, max_us),
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
                    value_us: 2_500,
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

    /// docs/08-persistence.md §4.6: the worst barrier's *shape* has to travel
    /// with its cost, or the number cannot discriminate between fjall's own
    /// work and the volume it hands one `fdatasync`. Three flushes, and only
    /// the slowest one's bytes may survive.
    #[test]
    fn the_worst_barriers_bytes_travel_with_its_cost() {
        let metrics = JournalCommitMetrics::new();
        let mut cursor = metrics.stage_snapshot();
        let flush = |us: u64, bytes: u64, records: u64| JournalStageSnapshot {
            records,
            bytes,
            sync_data_us_sum: us,
            sync_data_us_max: us,
            ..JournalStageSnapshot::default()
        };
        // An ordinary flush, then a far slower and much larger one, then a
        // third that is large but fast — the last must not displace the second.
        metrics.record_group(flush(300, 4_000, 30));
        metrics.record_group(flush(90_000, 6_000_000, 40));
        metrics.record_group(flush(400, 9_000_000, 50));

        let delta = metrics.stage_delta(&mut cursor);
        assert_eq!(delta.sync_data_us_max, 90_000);
        assert_eq!(
            delta.sync_data_us_max_bytes, 6_000_000,
            "the bytes must be the slowest flush's, not the largest flush's"
        );
        assert_eq!(delta.sync_data_us_max_records, 40);
        // Only the 90 ms flush crossed the threshold, so the slow-population
        // counters must describe it alone.
        assert_eq!(delta.slow_syncs, 1);
        assert_eq!(delta.slow_sync_bytes_sum, 6_000_000);
        assert_eq!(delta.slow_sync_records_sum, 40);
    }

    /// The slow counters are an interval difference; the maximum and its bytes
    /// are cumulative, exactly as `sync_data_us_max` already was. A reader that
    /// mistook either for the other would misreport every interval after the
    /// first.
    #[test]
    fn slow_counters_are_interval_totals_and_the_maximum_is_cumulative() {
        let metrics = JournalCommitMetrics::new();
        let mut cursor = metrics.stage_snapshot();
        let slow = JournalStageSnapshot {
            records: 7,
            bytes: 1_234,
            sync_data_us_sum: SLOW_SYNC_THRESHOLD_US,
            sync_data_us_max: SLOW_SYNC_THRESHOLD_US,
            ..JournalStageSnapshot::default()
        };
        metrics.record_group(slow);
        let first = metrics.stage_delta(&mut cursor);
        assert_eq!(first.slow_syncs, 1);
        assert_eq!(first.sync_data_us_max_bytes, 1_234);

        // A quiet interval: no new slow flush, but the cumulative maximum and
        // its shape still stand.
        metrics.record_group(JournalStageSnapshot {
            records: 3,
            bytes: 99,
            sync_data_us_sum: 100,
            sync_data_us_max: 100,
            ..JournalStageSnapshot::default()
        });
        let second = metrics.stage_delta(&mut cursor);
        assert_eq!(second.slow_syncs, 0, "slow counters are per interval");
        assert_eq!(second.slow_sync_bytes_sum, 0);
        assert_eq!(second.sync_data_us_max, SLOW_SYNC_THRESHOLD_US);
        assert_eq!(
            second.sync_data_us_max_bytes, 1_234,
            "the maximum and its bytes are cumulative together"
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
