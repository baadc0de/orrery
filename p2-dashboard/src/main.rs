//! P2 latency gate.
//!
//! Consumes the JSONL telemetry emitted by `p2-load --json`
//! (docs/11-roadmap.md §P2) and reports the four D16 latency series against
//! the demo-criterion targets verbatim (docs/DECISIONS.md D16):
//!
//! | series               | D16 target (p99, in-region) |
//! |----------------------|-----------------------------|
//! | `journal_commit_ms`  | < 2 ms (server-internal)    |
//! | `bulk_ack_ms`        | < 5 ms (client-observed)    |
//! | `intent_commit_ms`   | < 10 ms                     |
//! | `area_first_page_ms` | < 50 ms                     |
//!
//! The JSONL input carries *raw µs samples* (one JSON object per line, one
//! sample per `series` field); this tool buckets them into the bounded-memory
//! [`LatencyHistogram`] from the client crate, exactly as the rig does —
//! percentiles come out of one code path on both sides of the wire, so the
//! gate's reading and the rig's live CPU-side reading mean the same thing.
//!
//! `--json` carries the stable machine contract (a `Report` struct); `--gate`
//! makes the process exit non-zero when any series misses its threshold.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

use orrery_persist_client::latency::LatencyHistogram;

/// The four D16 series keys, in canonical report order. These are the wire
/// keys in the JSONL stream and the field names in the `Report`; they are the
/// contract between `p2-load` and this dashboard.
const SERIES_KEYS: [&str; 4] = [
    "journal_commit_ms",
    "bulk_ack_ms",
    "intent_commit_ms",
    "area_first_page_ms",
];

/// D16 defaults (docs/DECISIONS.md D16) as **µs ceilings** on the p99. These
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
    /// The threshold this series is gated against (µs).
    threshold_us: u64,
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
    /// The record kind: `run_header` | `sample` | `run_footer`.
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
    histograms: [LatencyHistogram; 4],
    /// The run context from the `run_header` record, if one was present.
    run_ctx: Option<RunContext>,
    /// Total non-empty lines read.
    records: usize,
    /// Lines that failed to parse.
    malformed: usize,
}

/// Read every JSONL file once and fold it into the histogram set. Sample
/// values stream through constant memory (the 22-bucket layout is fixed), so
/// a 30-minute soak at 10k entities × 4 Hz is not a memory problem here
/// either — the same argument as in the client crate's `latency` module.
fn load(files: &[PathBuf]) -> Result<Loaded> {
    let mut histograms = [
        LatencyHistogram::new(),
        LatencyHistogram::new(),
        LatencyHistogram::new(),
        LatencyHistogram::new(),
    ];
    let mut run_ctx: Option<RunContext> = None;
    let mut records = 0usize;
    let mut malformed = 0usize;

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
                Ok(record) => ingest(&mut histograms, &mut run_ctx, record),
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
    })
}

/// Fold one JSONL record into the live state.
fn ingest(histograms: &mut [LatencyHistogram; 4], run_ctx: &mut Option<RunContext>, r: Record) {
    match r.kind.as_str() {
        "run_header" => {
            if r.run.is_some() {
                *run_ctx = r.run;
            }
        }
        "sample" => {
            if let (Some(series), Some(value_us)) = (r.series, r.value_us) {
                if let Some(idx) = SERIES_KEYS.iter().position(|&k| k == series) {
                    histograms[idx].record(Duration::from_micros(value_us));
                }
            }
        }
        // run_footer and unknown kinds carry no latency data.
        _ => {}
    }
}

/// The D16 threshold for one series, by wire key.
fn threshold_for(t: &ThresholdsUs, key: &str) -> u64 {
    match key {
        "journal_commit_ms" => t.journal_commit_ms,
        "bulk_ack_ms" => t.bulk_ack_ms,
        "intent_commit_ms" => t.intent_commit_ms,
        "area_first_page_ms" => t.area_first_page_ms,
        _ => u64::MAX,
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
        let gate = match p99_us {
            None => SeriesGate::MissingData,
            Some(p99) if p99 <= threshold_us => SeriesGate::Pass,
            Some(_) => SeriesGate::Fail,
        };
        if gate != SeriesGate::Pass {
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
    println!("records: {} ({} malformed)", r.records, r.malformed);
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
        };
        println!(
            "{:<22} {:>9} {:>9} {:>9} {:>11} {:>9}",
            key, p50, p99, max, s.threshold_us, gate
        );
    }
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
    fn percentiles_compose_with_the_client_histogram() {
        // Cross-check the p99 this gate reports against a hand-computed bucket
        // upper bound: 100 samples at 1500 µs fall entirely in the 1–2 ms
        // bucket (boundaries table in crates/orrery_persist_client/src/
        // latency.rs), so both p50 and p99 report the bucket's 2000 µs upper
        // bound while max tracks the true 1500 µs.
        let mut hist = LatencyHistogram::new();
        for _ in 0..100 {
            hist.record(Duration::from_micros(1_500));
        }
        assert_eq!(hist.p50(), Duration::from_micros(2_000));
        assert_eq!(hist.p99(), Duration::from_micros(2_000));
        assert_eq!(hist.max(), Some(Duration::from_micros(1_500)));
        assert_eq!(LatencyHistogram::new().p99(), Duration::ZERO);
    }
}
