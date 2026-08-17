//! P2 latency gate.
//!
//! Consumes the JSONL telemetry emitted by `p2-load --json`
//! (docs/11-roadmap.md §P2) and reports the four D16 latency series against
//! the demo-criterion targets verbatim (docs/adr/0016-parameter-reference.md):
//!
//! | series               | D16 target (p99, in-region) |
//! |----------------------|-----------------------------|
//! | `journal_commit_ms`  | < 2 ms (server-internal)    |
//! | `bulk_ack_ms`        | < 5 ms (client-observed)    |
//! | `intent_commit_ms`   | < 10 ms                     |
//! | `area_first_page_ms` | < 50 ms                     |
//!
//! A fifth series, `gateway_bulk_server_ms`, is the server-side half of
//! `bulk_ack_ms` that persistd appends to the same artifact. D16 sets no
//! target for it, so it is folded and reported for attribution and never
//! contributes to the verdict — present or absent. It used to be silently
//! discarded while the report printed zero malformed records.
//!
//! The JSONL input carries raw µs samples (one `sample`, or a compact
//! `sample_batch` with an explicit count); this tool buckets them into the bounded-memory
//! [`LatencyHistogram`] from the client crate, exactly as the rig does —
//! percentiles come out of one code path on both sides of the wire, so the
//! gate's reading and the rig's live CPU-side reading mean the same thing.
//! The series names, the boundaries and the reconstruction rule are one
//! definition in `orrery_protocol::metrics`, reached here through the client
//! crate's re-export.
//!
//! `--json` carries the stable machine contract (a `Report` struct); `--gate`
//! makes the process exit non-zero when any series misses its threshold.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

use orrery_persist_client::latency::{
    is_known_series, LatencyHistogram, GATED_SERIES, SERIES_AREA_FIRST_PAGE, SERIES_BULK_ACK,
    SERIES_INTENT_COMMIT, SERIES_JOURNAL_COMMIT, UNGATED_SERIES,
};

/// Every series this gate folds, in canonical report order: the four gated
/// D16 keys first, then the ungated ones. The keys, the bucket boundaries and
/// the reconstruction rule all come from `orrery_protocol::metrics` through
/// the client crate's re-export — one definition, shared with the rig that
/// produces the stream and the persistd that appends to it.
///
/// The length is derived, so growing `UNGATED_SERIES` upstream is a compile
/// error here rather than a series silently counted as unknown. That is the
/// intended failure: this workspace is excluded from the root one, so nothing
/// else would notice.
const SERIES_KEYS: [&str; GATED_SERIES.len() + UNGATED_SERIES.len()] = [
    GATED_SERIES[0],
    GATED_SERIES[1],
    GATED_SERIES[2],
    GATED_SERIES[3],
    UNGATED_SERIES[0],
    UNGATED_SERIES[1],
    UNGATED_SERIES[2],
];

/// How many of [`SERIES_KEYS`] carry a D16 threshold. The rest are folded and
/// reported, never gated.
const NUM_GATED: usize = GATED_SERIES.len();

/// D16 defaults (docs/adr/0016-parameter-reference.md) as **µs ceilings** on
/// the p99. These
/// are the demo-criterion numbers; `--<series>-us` flags override them
/// individually for a sample-size- or posture-specific gate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ThresholdsUs {
    /// journal commit < 2 ms (server-internal).
    journal_commit_ms: u64,
    /// client-observed bulk ack p99 < 5 ms.
    bulk_ack_ms: u64,
    /// intent commit p99 < 10 ms.
    intent_commit_ms: u64,
    /// area first page-in < 50 ms.
    area_first_page_ms: u64,
}

impl ThresholdsUs {
    /// The D16 values verbatim.
    const D16: Self = Self {
        journal_commit_ms: 2_000,
        bulk_ack_ms: 5_000,
        intent_commit_ms: 10_000,
        area_first_page_ms: 50_000,
    };
}

/// One summary per series: bounded-memory percentiles from the shared D16
/// histogram (crates/orrery_persist_client/src/latency.rs). `p50_us`/`p99_us`
/// are the upper bound of the containing bucket (within one bucket width of
/// the true value by construction); `max_us` is exact. All are `None` when the
/// series has no samples.
#[derive(Debug, Clone, Serialize)]
struct SeriesSummary {
    /// Sample count folded into this series.
    n: u64,
    /// Approximate p50, µs.
    p50_us: Option<u64>,
    /// Approximate p99, µs; the gate compares this against the threshold.
    p99_us: Option<u64>,
    /// Exact max, µs.
    max_us: Option<u64>,
    /// The threshold this series is gated against (µs), or `None` for a
    /// series D16 sets no target for.
    threshold_us: Option<u64>,
    /// Whether this series met its threshold.
    gate: SeriesGate,
}

/// Per-series gate outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SeriesGate {
    /// p99 at or below the threshold.
    Pass,
    /// p99 above the threshold.
    Fail,
    /// No samples were recorded for this series. A series the run never
    /// sampled cannot pass the D16 demo criterion by omission.
    MissingData,
    /// D16 sets no target for this series: it is reported for attribution
    /// and never contributes to the verdict, present or absent.
    NotGated,
}

/// The overall gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GateVerdict {
    /// Every series met its threshold.
    Pass,
    /// At least one series missed (or was missing).
    Fail,
}

/// The run context block the rig emits in its `run_header` record. All fields
/// optional on the wire; the report echoes them unchanged so a viewer knows
/// which run the numbers came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RunContext {
    /// The gateway's NodeId (hex, `iroh` display form).
    #[serde(default)]
    gateway: Option<String>,
    /// The `--addr` the rig dialed.
    #[serde(default)]
    addr: Option<String>,
    /// Entities driven by the run.
    #[serde(default)]
    entities: Option<u64>,
    /// Cells covered by the entity inventory.
    #[serde(default)]
    cells: Option<u64>,
    /// Sessions (client fan-out) opened against the gateway.
    #[serde(default)]
    sessions: Option<u64>,
    /// Requested per-entity diff rate (Hz).
    #[serde(default)]
    diff_hz: Option<f64>,
    /// The requested intent mix, as `kind=fraction` pairs.
    #[serde(default)]
    intent_mix: Option<BTreeMap<String, f64>>,
    /// The full run duration, seconds.
    #[serde(default)]
    duration_secs: Option<u64>,
}

/// The `--json` report — the stable machine contract for the gate.
#[derive(Debug, Serialize)]
struct Report {
    /// Total non-empty JSONL lines read.
    records: usize,
    /// Lines that failed to parse.
    malformed: usize,
    /// Sample records naming a series this contract does not define. These
    /// are counted and reported but never gated: a producer that grows a new
    /// series should not fail a nightly run, while a *typo* in one of the
    /// gated names shows up here instead of vanishing.
    unknown_series: usize,
    /// The distinct series names behind `unknown_series`, sorted. A count
    /// alone does not tell an operator which producer drifted.
    unknown_series_names: Vec<String>,
    /// Whether the run's p99s all met their thresholds.
    gate: GateVerdict,
    /// The run context echoed from the `run_header` record, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<RunContext>,
    /// Per-series summaries, keyed by the wire series name.
    series: BTreeMap<&'static str, SeriesSummary>,
}

/// One JSONL record (the subset of fields the dashboard needs). Unknown extra
/// fields are ignored so forward-compatible emitters do not break the gate.
#[derive(Debug, Deserialize)]
struct Record {
    /// The record kind: `run_header` | `sample` | `sample_batch` | `run_footer`.
    #[serde(rename = "type")]
    kind: String,
    /// For `run_header`: the run context.
    #[serde(default)]
    run: Option<RunContext>,
    /// For `sample`: which D16 series.
    #[serde(default)]
    series: Option<String>,
    /// For `sample`: the latency, µs.
    #[serde(default)]
    value_us: Option<u64>,
    /// For `sample_batch`: number of identical values represented.
    #[serde(default)]
    count: Option<u64>,
}

#[derive(Parser)]
#[command(
    name = "p2-dashboard",
    about = "P2 latency gate: aggregate p2-load --json telemetry against the D16 targets"
)]
struct Cli {
    /// One or more `.jsonl` telemetry files from `p2-load --json`.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// Emit the machine-readable JSON summary (the stable machine contract)
    /// instead of the human report.
    #[arg(long)]
    json: bool,
    /// Exit non-zero when any series' p99 misses its threshold. The D16 demo
    /// criterion gates on this flag; a series with no samples fails the gate.
    #[arg(long)]
    gate: bool,
    /// Threshold override for `journal_commit_ms` (µs). Default: the D16 2 ms
    /// target.
    #[arg(long, default_value_t = ThresholdsUs::D16.journal_commit_ms)]
    journal_commit_ms: u64,
    /// Threshold override for `bulk_ack_ms` (µs). Default: the D16 5 ms
    /// target.
    #[arg(long, default_value_t = ThresholdsUs::D16.bulk_ack_ms)]
    bulk_ack_ms: u64,
    /// Threshold override for `intent_commit_ms` (µs). Default: the D16 10 ms
    /// target.
    #[arg(long, default_value_t = ThresholdsUs::D16.intent_commit_ms)]
    intent_commit_ms: u64,
    /// Threshold override for `area_first_page_ms` (µs). Default: the D16 50
    /// ms target.
    #[arg(long, default_value_t = ThresholdsUs::D16.area_first_page_ms)]
    area_first_page_ms: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let thresholds = ThresholdsUs {
        journal_commit_ms: cli.journal_commit_ms,
        bulk_ack_ms: cli.bulk_ack_ms,
        intent_commit_ms: cli.intent_commit_ms,
        area_first_page_ms: cli.area_first_page_ms,
    };
    let loaded = load(&cli.files)?;
    Ok(report_and_maybe_gate(&cli, &thresholds, &loaded))
}

/// The fully-parsed input: per-series histograms plus the run context.
struct Loaded {
    /// One histogram per series, indexed by position in [`SERIES_KEYS`].
    histograms: [LatencyHistogram; SERIES_KEYS.len()],
    /// The run context from the `run_header` record, if one was present.
    run_ctx: Option<RunContext>,
    /// Total non-empty lines read.
    records: usize,
    /// Lines that failed to parse.
    malformed: usize,
    /// Sample records naming a series outside the shared contract.
    unknown_series: usize,
    /// The distinct names behind `unknown_series`.
    unknown_series_names: BTreeSet<String>,
}

/// Read every JSONL file once and fold it into the histogram set. Sample
/// values stream through constant memory (the bucket layout is fixed by
/// `orrery_protocol::metrics`), so a 30-minute soak at 10k entities × 4 Hz is
/// not a memory problem here either — the same argument as in the client
/// crate's `latency` module.
fn load(files: &[PathBuf]) -> Result<Loaded> {
    let mut histograms = std::array::from_fn(|_| LatencyHistogram::new());
    let mut run_ctx: Option<RunContext> = None;
    let mut records = 0usize;
    let mut malformed = 0usize;
    let mut unknown_series = 0usize;
    let mut unknown_series_names = BTreeSet::new();

    for path in files {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), lineno + 1))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            records += 1;
            match serde_json::from_str::<Record>(line) {
                Ok(record) => {
                    if let Some(name) = ingest(&mut histograms, &mut run_ctx, record) {
                        unknown_series += 1;
                        unknown_series_names.insert(name);
                    }
                }
                Err(e) => {
                    malformed += 1;
                    eprintln!(
                        "warning: {}:{}: malformed JSON: {e}",
                        path.display(),
                        lineno + 1
                    );
                }
            }
        }
    }

    Ok(Loaded {
        histograms,
        run_ctx,
        records,
        malformed,
        unknown_series,
        unknown_series_names,
    })
}

/// Fold one JSONL record into the live state. Returns the series name for a
/// sample record the shared contract does not define — the one case the
/// caller counts, since it is the only way a producer/consumer name mismatch
/// can be seen at all.
fn ingest(
    histograms: &mut [LatencyHistogram; SERIES_KEYS.len()],
    run_ctx: &mut Option<RunContext>,
    r: Record,
) -> Option<String> {
    match r.kind.as_str() {
        "run_header" => {
            if r.run.is_some() {
                *run_ctx = r.run;
            }
            None
        }
        "sample" | "sample_batch" => {
            let (Some(series), Some(value_us)) = (r.series, r.value_us) else {
                return None;
            };
            let Some(idx) = SERIES_KEYS.iter().position(|&k| k == series) else {
                debug_assert!(!is_known_series(&series));
                return Some(series);
            };
            let count = if r.kind == "sample_batch" {
                r.count.unwrap_or(0)
            } else {
                1
            };
            for _ in 0..count {
                histograms[idx].record(Duration::from_micros(value_us));
            }
            None
        }
        // run_footer and unknown kinds carry no latency data.
        _ => None,
    }
}

/// The D16 threshold for one series, by wire key, or `None` for a series D16
/// sets no target for.
fn threshold_for(t: &ThresholdsUs, key: &str) -> Option<u64> {
    match key {
        SERIES_JOURNAL_COMMIT => Some(t.journal_commit_ms),
        SERIES_BULK_ACK => Some(t.bulk_ack_ms),
        SERIES_INTENT_COMMIT => Some(t.intent_commit_ms),
        SERIES_AREA_FIRST_PAGE => Some(t.area_first_page_ms),
        _ => None,
    }
}

/// Build the report from the loaded input, print it, and return the process
/// exit code under `--gate`. This is the whole gate; `run()` is just this
/// plus file IO, so the tests exercise the exact binary path.
fn report_and_maybe_gate(cli: &Cli, thresholds: &ThresholdsUs, loaded: &Loaded) -> ExitCode {
    let mut series = BTreeMap::new();
    let mut any_fail = false;
    for (i, key) in SERIES_KEYS.iter().enumerate() {
        let hist = &loaded.histograms[i];
        let (p50_us, p99_us, max_us) = if hist.total() == 0 {
            (None, None, None)
        } else {
            (
                Some(hist.p50().as_micros() as u64),
                Some(hist.p99().as_micros() as u64),
                hist.max().map(|d| d.as_micros() as u64),
            )
        };
        let threshold_us = threshold_for(thresholds, key);
        let gate = match (threshold_us, p99_us) {
            // Ungated series are reported for attribution only: a missing
            // `gateway_bulk_server_ms` must not fail a run the way a missing
            // D16 series does.
            (None, _) => SeriesGate::NotGated,
            (Some(_), None) => SeriesGate::MissingData,
            (Some(t), Some(p99)) if p99 <= t => SeriesGate::Pass,
            (Some(_), Some(_)) => SeriesGate::Fail,
        };
        debug_assert_eq!(i < NUM_GATED, threshold_us.is_some());
        if gate != SeriesGate::Pass && gate != SeriesGate::NotGated {
            any_fail = true;
        }
        series.insert(
            *key,
            SeriesSummary {
                n: hist.total(),
                p50_us,
                p99_us,
                max_us,
                threshold_us,
                gate,
            },
        );
    }

    let report = Report {
        records: loaded.records,
        malformed: loaded.malformed,
        unknown_series: loaded.unknown_series,
        unknown_series_names: loaded.unknown_series_names.iter().cloned().collect(),
        gate: if any_fail {
            GateVerdict::Fail
        } else {
            GateVerdict::Pass
        },
        run: loaded.run_ctx.clone(),
        series,
    };

    if cli.json {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("error: report serialization failed: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        print_human(&report);
    }

    if cli.gate {
        if report.gate == GateVerdict::Pass {
            ExitCode::SUCCESS
        } else {
            eprintln!("GATE FAILED: one or more D16 series missed its p99 target");
            ExitCode::FAILURE
        }
    } else {
        ExitCode::SUCCESS
    }
}

fn print_human(r: &Report) {
    println!("P2 latency dashboard");
    println!("====================");
    println!(
        "records: {} ({} malformed, {} unknown series)",
        r.records, r.malformed, r.unknown_series
    );
    if !r.unknown_series_names.is_empty() {
        println!("unknown series: {}", r.unknown_series_names.join(", "));
    }
    if let Some(run) = &r.run {
        println!();
        println!("run context:");
        if let Some(g) = &run.gateway {
            println!("  gateway:  {g}");
        }
        if let Some(a) = &run.addr {
            println!("  addr:     {a}");
        }
        if let (Some(e), Some(c), Some(s)) = (run.entities, run.cells, run.sessions) {
            println!("  entities: {e}   cells: {c}   sessions: {s}");
        }
        if let (Some(d), Some(hz)) = (run.duration_secs, run.diff_hz) {
            println!("  duration: {d} s   diff_hz: {hz}");
        }
    }
    println!();
    println!(
        "{:<22} {:>9} {:>9} {:>9} {:>11} {:>9}",
        "series", "p50 µs", "p99 µs", "max µs", "threshold", "gate"
    );
    println!("{}", "-".repeat(74));
    for (key, s) in &r.series {
        let p50 = s.p50_us.map_or_else(|| "—".into(), |v| v.to_string());
        let p99 = s.p99_us.map_or_else(|| "—".into(), |v| v.to_string());
        let max = s.max_us.map_or_else(|| "—".into(), |v| v.to_string());
        let gate = match s.gate {
            SeriesGate::Pass => "PASS",
            SeriesGate::Fail => "FAIL",
            SeriesGate::MissingData => "MISSING",
            SeriesGate::NotGated => "—",
        };
        let threshold = s.threshold_us.map_or_else(|| "—".into(), |v| v.to_string());
        println!(
            "{:<22} {:>9} {:>9} {:>9} {:>11} {:>9}",
            key, p50, p99, max, threshold, gate
        );
    }
    println!();
    // p50/p99 are histogram bucket *upper bounds*; max is the exact observed
    // value. So `max` legitimately reads below `p99` whenever every sample sat
    // low inside its bucket. Say so, or the table looks impossible.
    println!("p50/p99 are bucket upper bounds; max is exact, so max < p99 is normal.");
    println!();
    println!(
        "GATE: {}",
        if r.gate == GateVerdict::Pass {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_persist_client::latency::SERIES_GATEWAY_BULK_SERVER;

    /// The two server-side spans persistd added alongside the bulk one. Named
    /// through `UNGATED_SERIES` rather than imported: the client crate
    /// re-exports the array, and the array is the contract this gate folds.
    const SERIES_GATEWAY_INTENT_SERVER: &str = UNGATED_SERIES[1];
    const SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER: &str = UNGATED_SERIES[2];
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique temp path per test invocation so parallel tests do not collide.
    fn tmp(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "p2-dashboard-test-{name}-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    fn write_jsonl(path: &std::path::Path, lines: &[serde_json::Value]) {
        let mut f = File::create(path).expect("create temp jsonl");
        for line in lines {
            writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
    }

    fn sample(series: &str, value_us: u64) -> serde_json::Value {
        serde_json::json!({"type": "sample", "series": series, "value_us": value_us})
    }

    fn sample_batch(series: &str, value_us: u64, count: u64) -> serde_json::Value {
        serde_json::json!({"type": "sample_batch", "series": series, "value_us": value_us, "count": count})
    }

    fn run_header() -> serde_json::Value {
        serde_json::json!({
            "type": "run_header",
            "run": {
                "gateway": "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
                "addr": "EndpointAddr { id: .., addrs: [] }",
                "entities": 1000u64,
                "cells": 128u64,
                "sessions": 6u64,
                "diff_hz": 2.0,
                "intent_mix": {"trade": 0.02, "craft": 0.01},
                "duration_secs": 30u64
            }
        })
    }

    /// Conforming test data: every series' samples sit below its D16 target
    /// (`p2-dashboard/testdata/demo.jsonl` mirrors this shape).
    fn conforming() -> Vec<serde_json::Value> {
        let mut v = vec![run_header()];
        // journal_commit < 2 ms.
        for i in 0..100u64 {
            v.push(sample("journal_commit_ms", 400 + (i % 3) * 250));
        }
        // bulk_ack < 5 ms.
        for i in 0..100u64 {
            v.push(sample("bulk_ack_ms", 1_500 + (i % 2) * 1_500));
        }
        // intent_commit < 10 ms.
        for i in 0..100u64 {
            v.push(sample("intent_commit_ms", 4_500 + (i % 2) * 2_500));
        }
        // area_first_page < 50 ms.
        for i in 0..100u64 {
            v.push(sample("area_first_page_ms", 12_000 + (i % 2) * 16_000));
        }
        v
    }

    fn gate_cli(files: Vec<PathBuf>) -> Cli {
        Cli {
            files,
            json: false,
            gate: true,
            journal_commit_ms: ThresholdsUs::D16.journal_commit_ms,
            bulk_ack_ms: ThresholdsUs::D16.bulk_ack_ms,
            intent_commit_ms: ThresholdsUs::D16.intent_commit_ms,
            area_first_page_ms: ThresholdsUs::D16.area_first_page_ms,
        }
    }

    /// Run the exact binary gate path on `lines` and return the exit code.
    /// Errors are surfaced as exit code 2 here (no anyhow propagation) so the
    /// tests assert on the process-visible contract only.
    fn run_gate_on(lines: Vec<serde_json::Value>) -> ExitCode {
        let path = tmp("gate");
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let thresholds = ThresholdsUs::D16;
        let loaded = load(&cli.files).expect("test jsonl loads");
        let exit = report_and_maybe_gate(&cli, &thresholds, &loaded);
        let _ = std::fs::remove_file(&path);
        exit
    }

    #[test]
    fn gate_passes_on_conforming_testdata() {
        assert_eq!(run_gate_on(conforming()), ExitCode::SUCCESS);
    }

    #[test]
    fn gate_fails_when_one_series_regresses() {
        // Mutate the bulk-ack series so its p99 lands above the 5 ms target.
        // 150 extra samples at 20 ms against a 100-sample base means p99 is
        // firmly in the 20 ms bucket — a real regression, not a tail spike.
        let mut lines = conforming();
        for _ in 0..150u64 {
            lines.push(sample("bulk_ack_ms", 20_000));
        }
        assert_ne!(run_gate_on(lines), ExitCode::SUCCESS);
    }

    #[test]
    fn gate_targets_p99_not_max() {
        // One 500 ms spike in a 100-sample series leaves p99 below threshold
        // (sample rank 99 of 101 is a conforming sample), so the run must
        // still pass. This pins the gate to p99 — reading max instead would
        // fail this test.
        let mut lines = conforming();
        lines.push(sample("bulk_ack_ms", 500_000));
        assert_eq!(run_gate_on(lines), ExitCode::SUCCESS);
    }

    #[test]
    fn missing_series_fails_the_gate() {
        // The D16 demo criterion requires all four series measured; a run
        // with no intent samples cannot pass by omission.
        let lines = conforming()
            .into_iter()
            .filter(|v| v.get("series").and_then(|s| s.as_str()) != Some("intent_commit_ms"))
            .collect::<Vec<_>>();
        assert_ne!(run_gate_on(lines), ExitCode::SUCCESS);
    }

    #[test]
    fn sample_batch_folds_exactly_like_repeated_samples() {
        // Rewritten with the shared lattice. This test used to assert that
        // 100 journal samples at 1 500 µs report a p99 of 2 000 µs — which is
        // exactly the D16 threshold this gate compares `p99 <= t` against, so
        // it encoded the collapse as correct. On the shared boundaries the
        // 1.5 ms band has its own bucket and the reported p99 is 1 500 µs.
        let path = tmp("batch");
        let lines = vec![
            sample_batch("journal_commit_ms", 1_500, 100),
            sample_batch("bulk_ack_ms", 3_000, 100),
            sample_batch("intent_commit_ms", 7_000, 100),
            sample_batch("area_first_page_ms", 28_000, 100),
        ];
        write_jsonl(&path, &lines);
        let loaded = load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(loaded.histograms[0].total(), 100);
        assert_eq!(loaded.histograms[0].p99(), Duration::from_micros(1_500));
        assert!(
            loaded.histograms[0].p99() < Duration::from_micros(ThresholdsUs::D16.journal_commit_ms)
        );
        assert_eq!(loaded.histograms[3].total(), 100);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_journal_band_resolves_instead_of_pinning_to_the_threshold() {
        // Rewritten. The old version asserted that a 1 500 µs sample reports
        // p50 == p99 == 2 000 µs, the D16 journal threshold — the defect, not
        // the contract: with 1 ms and 2 ms adjacent, *every* journal p99
        // anywhere in the 1.0–2.0 ms band read out as the threshold and
        // passed on `p99 <= threshold`. Three points across that band must
        // now report three different numbers, all below the threshold.
        let readings: Vec<u64> = [1_100u64, 1_400, 1_900]
            .into_iter()
            .map(|micros| {
                let mut hist = LatencyHistogram::new();
                for _ in 0..100 {
                    hist.record(Duration::from_micros(micros));
                }
                assert_eq!(hist.max(), Some(Duration::from_micros(micros)));
                hist.p99().as_micros() as u64
            })
            .collect();
        assert_eq!(readings, vec![1_250, 1_500, 2_000]);
        assert!(readings[0] < readings[1] && readings[1] < readings[2]);
        assert_eq!(LatencyHistogram::new().p99(), Duration::ZERO);
    }

    #[test]
    fn gateway_bulk_server_series_is_folded_and_never_gates() {
        // The fifth series persistd emits. It must be recognized (not counted
        // as unknown), reported, and inert in the verdict.
        let path = tmp("fifth");
        let mut lines = conforming();
        for _ in 0..50u64 {
            lines.push(sample(SERIES_GATEWAY_BULK_SERVER, 300_000));
        }
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let loaded = load(&cli.files).expect("test jsonl loads");
        assert_eq!(loaded.unknown_series, 0);
        let idx = SERIES_KEYS
            .iter()
            .position(|&k| k == SERIES_GATEWAY_BULK_SERVER)
            .expect("the fifth series has a slot");
        assert_eq!(loaded.histograms[idx].total(), 50);
        // 300 ms would fail every D16 threshold; the run still passes.
        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded),
            ExitCode::SUCCESS
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn server_side_spans_are_folded_separately_from_the_gated_round_trips() {
        // The failure this guards is silent and one-directional: a server
        // span is strictly shorter than the client round trip it attributes,
        // so folding it into the gated series would *lower* the p99 and pass
        // a gate that measured nothing. Same artifact, same fold, four
        // distinct histograms.
        let path = tmp("server-spans");
        let mut lines = conforming();
        for _ in 0..40u64 {
            lines.push(sample(SERIES_GATEWAY_INTENT_SERVER, 200));
            lines.push(sample(SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER, 500));
        }
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let loaded = load(&cli.files).expect("test jsonl loads");
        assert_eq!(loaded.unknown_series, 0, "both names are in the contract");

        let slot = |key: &str| {
            SERIES_KEYS
                .iter()
                .position(|&k| k == key)
                .expect("every contract series has a slot")
        };
        assert_eq!(
            loaded.histograms[slot(SERIES_GATEWAY_INTENT_SERVER)].total(),
            40
        );
        assert_eq!(
            loaded.histograms[slot(SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER)].total(),
            40
        );

        // The gated p99s are exactly what `conforming()` alone produces: the
        // server spans landed nowhere near them.
        let baseline = {
            let base = tmp("server-spans-baseline");
            write_jsonl(&base, &conforming());
            let loaded = load(std::slice::from_ref(&base)).expect("baseline loads");
            let p99s: Vec<_> = GATED_SERIES
                .iter()
                .map(|&key| loaded.histograms[slot(key)].p99())
                .collect();
            let _ = std::fs::remove_file(base);
            p99s
        };
        let observed: Vec<_> = GATED_SERIES
            .iter()
            .map(|&key| loaded.histograms[slot(key)].p99())
            .collect();
        assert_eq!(observed, baseline, "a server span moved a gated p99");

        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded),
            ExitCode::SUCCESS
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn every_ungated_series_is_reported_and_none_of_them_gates() {
        for key in UNGATED_SERIES {
            assert!(
                threshold_for(&ThresholdsUs::D16, key).is_none(),
                "{key} acquired a D16 threshold"
            );
            assert!(
                SERIES_KEYS.contains(&key),
                "{key} is in the contract but has no slot in the fold"
            );
        }
        assert_eq!(NUM_GATED, GATED_SERIES.len());
    }

    #[test]
    fn an_absent_fifth_series_does_not_fail_the_gate() {
        // The nightly artifact contains it only when persistd was configured
        // to export it, so absence must read as `not_gated`, not `missing`.
        assert_eq!(run_gate_on(conforming()), ExitCode::SUCCESS);
    }

    #[test]
    fn a_series_outside_the_contract_is_counted_rather_than_discarded() {
        // A typo in a gated name used to vanish into the default arm while
        // the report printed zero malformed records.
        let path = tmp("unknown");
        let mut lines = conforming();
        lines.push(sample("bulk_ack_us", 1_000));
        lines.push(sample_batch("journal_commit_millis", 1_000, 7));
        write_jsonl(&path, &lines);
        let loaded = load(std::slice::from_ref(&path)).expect("test jsonl loads");
        assert_eq!(loaded.unknown_series, 2, "both records are counted once");
        assert_eq!(loaded.malformed, 0, "they parse; they are just not ours");
        // Counting them is diagnostic, not gating.
        let cli = gate_cli(vec![path.clone()]);
        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded),
            ExitCode::SUCCESS
        );
        let _ = std::fs::remove_file(path);
    }
}
