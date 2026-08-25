//! JSONL telemetry output for the P2 latency gate.
//!
//! The gates/p0-nat-test contract: one JSON object per line on stdout, tracing on
//! stderr. Four record kinds — one `run_header` (run context), then `sample`
//! or compact `sample_batch` latency observations in any D16 series, then one
//! `run_footer`. Percentiles are computed by the consumer (`gates/p2-dashboard`)
//! from the raw samples with the shared bounded-memory histogram
//! (`orrery_persist_client::latency`), so this stream is the full record — no
//! server-side aggregation is lost between the rig and the gate.
//!
//! The schema is documented in `gates/p2-load/README.md` and parsed by
//! `gates/p2-dashboard/src/main.rs`; the two are one contract.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use orrery_persist_client::latency::LatencyHistogram;

/// The D16 series keys — the wire contract with `gates/p2-dashboard` — re-exported
/// from the one definition in `orrery_protocol::metrics`. The rig used to
/// declare its own four; that copy is gone.
///
/// `journal_commit_ms` is server-internal (D16): the rig has no wire access
/// to it, so no samples are ever drained under that key — the gateway
/// operator appends them out of band (see README).
#[allow(unused_imports)]
pub use orrery_persist_client::latency::{
    SERIES_AREA_FIRST_PAGE, SERIES_BULK_ACK, SERIES_INTENT_COMMIT, SERIES_JOURNAL_COMMIT,
};

/// The client-side stage attribution for `bulk_ack_ms` — rig backlog, the
/// rig's send path, the wire, and the rig's ack handling. Ungated (D16 sets no
/// target for them); they exist so a bulk-ack tail can be attributed without
/// re-running, the client-side counterpart of `gateway_bulk_server_ms`.
#[allow(unused_imports)]
pub use orrery_persist_client::latency::{
    SERIES_CLIENT_BULK_DISPATCH, SERIES_CLIENT_BULK_QUEUE, SERIES_CLIENT_BULK_SEND,
    SERIES_CLIENT_BULK_WIRE, SERIES_CLIENT_QUIC_RTT, SERIES_CLIENT_SEND_BUFFER,
};

#[allow(dead_code)]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The run context emitted once as the `run_header` record.
#[derive(Debug, Clone, Serialize)]
pub struct RunContext {
    /// The gateway's NodeId (hex display form).
    pub gateway: String,
    /// The socket address the rig dialed.
    pub addr: String,
    /// Entities driven.
    pub entities: u64,
    /// Distinct interest cells the inventory spans.
    pub cells: u64,
    /// Sessions the load was fanned out over.
    pub sessions: u64,
    /// Per-entity diff rate (Hz).
    pub diff_hz: f64,
    /// The intent mix (`kind` → fraction of diff sends).
    pub intent_mix: BTreeMap<String, f64>,
    /// Run duration, seconds.
    pub duration_secs: u64,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record<'a> {
    RunHeader {
        run: &'a RunContext,
    },
    Sample {
        series: &'a str,
        value_us: u64,
    },
    SampleBatch {
        series: &'a str,
        value_us: u64,
        count: u64,
    },
    RunFooter {
        note: &'a str,
    },
}

/// Serialize and print one JSONL record on stdout. Never panics on a closed
/// pipe: a broken stdout (harness died) degrades the run to tracing only,
/// mirroring persistd's startup-line posture.
///
/// Silent under `cfg(test)`: unit tests share the process's stdout with the
/// test harness, and an interleaved sample line would corrupt its protocol.
/// The sink's bookkeeping is covered by its own tests without touching
/// stdout.
fn emit(record: &Record<'_>) {
    if cfg!(test) {
        return;
    }
    match serde_json::to_string(record) {
        Ok(line) => {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            let _ = writeln!(handle, "{line}");
            let _ = handle.flush();
        }
        Err(e) => tracing::warn!(error = %e, "telemetry: serialize failed"),
    }
}

/// Emit the `run_header` record.
pub fn run_header(run: &RunContext) {
    emit(&Record::RunHeader { run });
}

/// Emit one latency sample.
pub fn sample(series: &'static str, value_us: u64) {
    emit(&Record::Sample { series, value_us });
}

/// Emit `count` identical latency samples compactly. This is principally for
/// journal group-commit metrics, whose bucket counts originate in persistd.
pub fn sample_batch(series: &'static str, value_us: u64, count: u64) {
    if count != 0 {
        emit(&Record::SampleBatch {
            series,
            value_us,
            count,
        });
    }
}

/// Emit the `run_footer` record.
pub fn run_footer(note: &str) {
    emit(&Record::RunFooter { note });
}

/// The rig-side sampler: drains the client's bounded-memory histograms into
/// the JSONL stream as raw µs samples.
///
/// The histograms are the live measurement (bulk ack in
/// `UplinkScheduler::on_ack`, intent commit in `IntentQueue::on_ack`); each
/// drain serializes every bucket count as that many individual `sample`
/// records so the gate can recompute percentiles from one shared code path
/// (`LatencyHistogram`) on both sides of the wire. A series is only as
/// bounded as the histogram itself — constant memory, exactly as the D16
/// latency recorder is designed to be (crates/orrery_persist_client/src/
/// latency.rs).
///
/// `journal_commit_ms` has no sampler here: that series is server-internal
/// (D16) and the wire has no message for it; the gateway operator appends
/// those samples out of band (see `gates/p2-load/README.md`).
#[derive(Debug, Default)]
pub struct TelemetrySink {
    /// Watermarks for every bucket in every series. Bucket-local cursors are
    /// required because a later sample can land before samples emitted by an
    /// earlier drain; a single total-count cursor cannot represent that.
    drained: Mutex<BTreeMap<&'static str, Vec<u64>>>,
}

impl TelemetrySink {
    /// A new sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain the not-yet-emitted samples from `hist` for `series` as compact
    /// JSONL sample-batch records. Buckets serialize in boundary order; each
    /// bucket count delta becomes one batch at the bucket's upper bound,
    /// which lands the consumer's histogram in the same bucket the rig
    /// recorded — percentiles agree bucket-for-bucket.
    pub fn drain_histogram(&self, series: &'static str, hist: &LatencyHistogram) {
        for (value_us, count) in self.take_delta(series, hist) {
            sample_batch(series, value_us, count);
        }
    }

    /// Advance this series' per-bucket cursors and return exact bucket deltas.
    fn take_delta(&self, series: &'static str, hist: &LatencyHistogram) -> Vec<(u64, u64)> {
        let mut drained = self.drained.lock().expect("telemetry sink lock");
        let buckets = hist.buckets();
        let previous = drained
            .entry(series)
            .or_insert_with(|| vec![0; buckets.len()]);
        debug_assert_eq!(previous.len(), buckets.len());

        let mut delta = Vec::new();
        for (i, (&current, before)) in buckets.iter().zip(previous.iter_mut()).enumerate() {
            let count = current.saturating_sub(*before);
            *before = current;
            if count != 0 {
                delta.push((bucket_upper_us(hist, i), count));
            }
        }
        delta
    }
}

/// The µs value a drained bucket's samples are emitted at: the bucket's upper
/// bound (identical bucketing semantics to the histogram's own percentile
/// methods). The overflow bucket has no upper bound, so its samples carry the
/// histogram's tracked max — the same value the histogram itself reports for
/// a percentile landing in overflow.
///
/// The boundary table used to be copied here, deliberately, because the
/// client crate did not export it. It does now
/// (`orrery_protocol::metrics`, re-exported through
/// `orrery_persist_client::latency`), so this is the shared rule and not a
/// mirror of it: the round trip through it lands the consumer's histogram in
/// the same bucket the rig recorded, which is what "percentiles agree on both
/// sides of the wire" reduces to.
fn bucket_upper_us(hist: &LatencyHistogram, index: usize) -> u64 {
    let observed_max_us = hist.max().map_or(0, |d| d.as_micros() as u64);
    orrery_persist_client::latency::bucket_upper_us(index, observed_max_us)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_persist_client::latency::{bucket_index, NUM_LATENCY_BUCKETS};
    use std::time::Duration;

    #[test]
    fn drain_is_delta_only() {
        let sink = TelemetrySink::new();
        let mut hist = LatencyHistogram::new();
        for _ in 0..10 {
            hist.record(Duration::from_micros(1_500));
        }
        assert_eq!(sink.take_delta(SERIES_BULK_ACK, &hist), vec![(1_500, 10)]);
        // A second drain with no new samples emits nothing.
        assert!(sink.take_delta(SERIES_BULK_ACK, &hist).is_empty());
        // New samples drain on the next pass.
        for _ in 0..5 {
            hist.record(Duration::from_micros(1_500));
        }
        assert_eq!(sink.take_delta(SERIES_BULK_ACK, &hist), vec![(1_500, 5)]);
    }

    #[test]
    fn mixed_bucket_multi_drain_emits_exact_dashboard_reconstruction() {
        let sink = TelemetrySink::new();
        let mut source = LatencyHistogram::new();
        let mut reconstructed = LatencyHistogram::new();

        // First drain contains only a high bucket. The old total-count cursor
        // would later skip a newly populated lower bucket and re-emit part of
        // this high bucket after bucket ordering changed beneath it.
        for _ in 0..3 {
            source.record(Duration::from_micros(12_000));
        }
        let first = sink.take_delta(SERIES_BULK_ACK, &source);
        assert_eq!(first, vec![(15_000, 3)]);
        replay_batches(&mut reconstructed, &first);

        for _ in 0..5 {
            source.record(Duration::from_micros(1_500));
        }
        for _ in 0..2 {
            source.record(Duration::from_micros(12_000));
        }
        let second = sink.take_delta(SERIES_BULK_ACK, &source);
        assert_eq!(second, vec![(1_500, 5), (15_000, 2)]);
        replay_batches(&mut reconstructed, &second);

        assert_eq!(reconstructed.total(), source.total());
        assert_eq!(reconstructed.buckets(), source.buckets());
        assert_eq!(reconstructed.p50(), source.p50());
        assert_eq!(reconstructed.p99(), source.p99());
        assert!(sink.take_delta(SERIES_BULK_ACK, &source).is_empty());
    }

    /// Mirror `gates/p2-dashboard`'s `sample_batch` ingestion: recording the batch's
    /// bucket-upper-bound value `count` times must reconstruct identical
    /// bucket counts and percentiles.
    fn replay_batches(hist: &mut LatencyHistogram, batches: &[(u64, u64)]) {
        for &(value_us, count) in batches {
            for _ in 0..count {
                hist.record(Duration::from_micros(value_us));
            }
        }
    }

    #[test]
    fn bucket_upper_bound_mirrors_histogram_boundaries() {
        // A 1 500 µs sample must serialize at the upper bound of the bucket
        // it landed in — the same value the histogram's own p50 reports for
        // it — so the gate's reconstruction and the rig's live view agree
        // bucket-for-bucket. On the shared lattice that bound is 1 500 µs,
        // not the 2 000 µs D16 threshold.
        let mut hist = LatencyHistogram::new();
        hist.record(Duration::from_micros(1_500));
        assert_eq!(hist.p50().as_micros(), 1_500);
        assert_eq!(
            bucket_upper_us(&hist, bucket_index(1_500)),
            hist.p50().as_micros() as u64
        );
        // The overflow bucket carries the histogram max.
        assert_eq!(bucket_upper_us(&hist, NUM_LATENCY_BUCKETS - 1), 1_500);
    }

    #[test]
    fn every_drained_bucket_replays_into_the_bucket_it_came_from() {
        // The producer/consumer contract in one assertion: whatever the rig
        // drains, the gate re-records into the same bucket, so the two report
        // the same percentile.
        let sink = TelemetrySink::new();
        let mut source = LatencyHistogram::new();
        for micros in [40, 900, 1_100, 1_600, 4_200, 9_000, 44_000, 1_400_000] {
            source.record(Duration::from_micros(micros));
        }
        let batches = sink.take_delta(SERIES_BULK_ACK, &source);
        let mut reconstructed = LatencyHistogram::new();
        replay_batches(&mut reconstructed, &batches);
        assert_eq!(reconstructed.total(), source.total());
        assert_eq!(reconstructed.p50(), source.p50());
        assert_eq!(reconstructed.p99(), source.p99());
    }
}
