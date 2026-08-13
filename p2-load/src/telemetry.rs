//! JSONL telemetry output for the P2 latency gate.
//!
//! The p0-nat-test contract: one JSON object per line on stdout, tracing on
//! stderr. Three record kinds — one `run_header` (run context), then one
//! `sample` per latency observation in any of the four D16 series, then one
//! `run_footer`. Percentiles are computed by the consumer (`p2-dashboard`)
//! from the raw samples with the shared bounded-memory histogram
//! (`orrery_persist_client::latency`), so this stream is the full record — no
//! server-side aggregation is lost between the rig and the gate.
//!
//! The schema is documented in `p2-load/README.md` and parsed by
//! `p2-dashboard/src/main.rs`; the two are one contract.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use orrery_persist_client::latency::LatencyHistogram;

/// The four D16 series keys — the wire contract with `p2-dashboard`.
/// `journal_commit_ms` is server-internal (D16): the rig has no wire access
/// to it, so no samples are ever drained under this key — the gateway
/// operator appends them out of band (see README). The constant exists so
/// the contract's four names live in one place.
#[allow(dead_code)]
pub const SERIES_JOURNAL_COMMIT: &str = "journal_commit_ms";
/// Client-observed bulk ack (D16: p99 < 5 ms).
pub const SERIES_BULK_ACK: &str = "bulk_ack_ms";
/// Intent commit (D16: p99 < 10 ms).
pub const SERIES_INTENT_COMMIT: &str = "intent_commit_ms";
/// Area first page-in (D16: < 50 ms).
pub const SERIES_AREA_FIRST_PAGE: &str = "area_first_page_ms";

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
    RunHeader { run: &'a RunContext },
    Sample { series: &'a str, value_us: u64 },
    RunFooter { note: &'a str },
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
/// those samples out of band (see `p2-load/README.md`).
#[derive(Debug, Default)]
pub struct TelemetrySink {
    /// Watermarks: how many samples per histogram have already been drained,
    /// so a long run re-emits only the delta.
    drained: Mutex<BTreeMap<&'static str, u64>>,
}

impl TelemetrySink {
    /// A new sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain the not-yet-emitted samples from `hist` for `series` as JSONL
    /// sample records. Buckets serialize in boundary order; each bucket count
    /// delta becomes that many identical samples at the bucket's upper bound,
    /// which lands the consumer's histogram in the same bucket the rig
    /// recorded — percentiles agree bucket-for-bucket.
    pub fn drain_histogram(&self, series: &'static str, hist: &LatencyHistogram) {
        let mut drained = self.drained.lock().expect("telemetry sink lock");
        let already = drained.entry(series).or_insert(0);
        let total = hist.total();
        if total <= *already {
            return;
        }
        // Walk buckets in boundary order, skipping the first `already` samples
        // (they were emitted by a previous drain) and emitting the rest.
        let mut skip = *already;
        for (i, &count) in hist.buckets().iter().enumerate() {
            let mut count = count;
            if skip >= count {
                skip -= count;
                continue;
            }
            count -= skip;
            skip = 0;
            let value_us = bucket_upper_us(hist, i);
            for _ in 0..count {
                sample(series, value_us);
            }
        }
        *already = total;
    }
}

/// The µs value a drained bucket's samples are emitted at: the bucket's upper
/// bound (identical bucketing semantics to the histogram's own percentile
/// methods). The overflow bucket has no upper bound, so its samples carry the
/// histogram's tracked max — the same value the histogram itself reports for
/// a percentile landing in overflow.
///
/// The boundary table mirrors the client crate's (kept in lockstep with
/// `orrery_persist_client/src/latency.rs`; both cite the same D16 rationale).
/// The client crate does not export the table, and a percentile landing in
/// the overflow bucket reports the tracked max — so the round-trip through
/// this table lands the consumer's histogram in the same bucket the rig
/// recorded, which is what "percentiles agree on both sides of the wire"
/// reduces to.
fn bucket_upper_us(hist: &LatencyHistogram, index: usize) -> u64 {
    const BOUNDARIES_US: [u64; 22] = [
        50, 100, 200, 500, 1_000, 2_000, 3_000, 5_000, 7_000, 10_000, 15_000, 20_000, 30_000,
        50_000, 75_000, 100_000, 150_000, 200_000, 300_000, 500_000, 750_000, 1_000_000,
    ];
    BOUNDARIES_US
        .get(index)
        .copied()
        .unwrap_or_else(|| hist.max().map_or(0, |d| d.as_micros() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn drain_is_delta_only() {
        let sink = TelemetrySink::new();
        let mut hist = LatencyHistogram::new();
        for _ in 0..10 {
            hist.record(Duration::from_micros(1_500));
        }
        sink.drain_histogram(SERIES_BULK_ACK, &hist);
        assert_eq!(
            *sink.drained.lock().unwrap().get(SERIES_BULK_ACK).unwrap(),
            10
        );
        // A second drain with no new samples emits nothing.
        sink.drain_histogram(SERIES_BULK_ACK, &hist);
        assert_eq!(
            *sink.drained.lock().unwrap().get(SERIES_BULK_ACK).unwrap(),
            10
        );
        // New samples drain on the next pass.
        for _ in 0..5 {
            hist.record(Duration::from_micros(1_500));
        }
        sink.drain_histogram(SERIES_BULK_ACK, &hist);
        assert_eq!(
            *sink.drained.lock().unwrap().get(SERIES_BULK_ACK).unwrap(),
            15
        );
    }

    #[test]
    fn bucket_upper_bound_mirrors_histogram_boundaries() {
        // The 2 000 µs boundary is the D16 journal-commit bucket edge
        // (latency.rs boundaries table). A 1 500 µs sample must serialize at
        // the 2 000 µs upper bound — the same value the histogram's p50
        // reports for it — so the gate's reconstruction and the rig's live
        // view agree bucket-for-bucket.
        let mut hist = LatencyHistogram::new();
        hist.record(Duration::from_micros(1_500));
        assert_eq!(hist.p50().as_micros(), 2_000);
        // The bucket index of the 1 500 µs sample is the first boundary
        // ≥ 1 500, which is 1 000 < 1 500 ≤ 2 000 → index 5.
        assert_eq!(bucket_upper_us(&hist, 5), 2_000);
        // The overflow bucket (index 22) carries the histogram max.
        assert_eq!(bucket_upper_us(&hist, 22), 1_500);
    }
}
