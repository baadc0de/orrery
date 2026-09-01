//! P0 punch-rate dashboard.
//!
//! Aggregates the JSONL telemetry emitted by `gates/p0-nat-test --json`
//! (docs/11-roadmap.md §P0) into the permanent regression artifact the demo
//! criterion wants: a direct-path rate and direct-bytes fraction compared
//! against iroh's production baseline (~90% direct connections, ~95% of bytes
//! on direct paths).
//!
//! It correlates the host and peer sides of each pair (by `node`/`peer`),
//! counts how many pairs reached a direct path, and estimates the fraction of
//! bytes carried on direct paths from the per-peer datagram stats. The output
//! is a human-readable report plus `--json` for CI gating.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

/// The iroh production baseline (docs/11-roadmap.md §P0, iroh FAQ).
const BASELINE_DIRECT_RATE: f64 = 0.90;
const BASELINE_DIRECT_BYTES: f64 = 0.95;

#[derive(Parser)]
#[command(about = "P0 punch-rate dashboard: aggregate gates/p0-nat-test --json soak telemetry")]
struct Cli {
    /// One or more `.jsonl` telemetry files from `gates/p0-nat-test --json`.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// Emit a machine-readable JSON summary (for CI gating) instead of the
    /// human report.
    #[arg(long, env = "ORRERY_JSON")]
    json: bool,
    /// Fail (exit non-zero) if the direct-path rate is below the baseline.
    ///
    /// The iroh baselines (~90% direct, ~95% direct bytes) are population
    /// numbers; a small soak (e.g. 7 pairs with one forced-relay peer) lands
    /// below them by design. Override with `--min-direct-rate`/`--min-direct-bytes`
    /// for the sample size, or omit `--gate` to report without failing.
    #[arg(long, env = "ORRERY_GATE")]
    gate: bool,
    /// Gate threshold for the direct-path rate (0..=1). Defaults to the iroh
    /// baseline.
    #[arg(long, default_value_t = BASELINE_DIRECT_RATE, env = "ORRERY_MIN_DIRECT_RATE")]
    min_direct_rate: f64,
    /// Gate threshold for the direct-bytes fraction (0..=1). Defaults to the
    /// iroh baseline.
    #[arg(long, default_value_t = BASELINE_DIRECT_BYTES, env = "ORRERY_MIN_DIRECT_BYTES")]
    min_direct_bytes: f64,
}

/// One JSONL record (the subset of fields the dashboard needs).
#[derive(Debug, Deserialize)]
struct Record {
    #[allow(dead_code)]
    ts: u64,
    node: String,
    #[allow(dead_code)]
    role: String,
    peer: usize,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ttd_ms: Option<u64>,
    #[serde(default)]
    sent: Option<u64>,
    #[serde(default)]
    received: Option<u64>,
    #[serde(default)]
    dropped: Option<u64>,
    #[serde(default)]
    rtt_p50_us: Option<u64>,
    #[serde(default)]
    rtt_p95_us: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    remote: Option<String>,
}

/// A single peer pair, correlated across both sides.
#[derive(Debug, Default)]
struct Pair {
    /// Whether either side reported a direct path.
    direct: bool,
    /// The relay path (if never direct).
    relay: bool,
    /// Time-to-direct-path (ms) from the side that reported it.
    ttd_ms: Option<u64>,
    /// Total datagrams sent across both sides.
    sent: u64,
    /// Total datagrams received across both sides.
    received: u64,
    /// Total datagrams dropped across both sides.
    dropped: u64,
    /// RTT percentiles (µs) from the side that reported them.
    rtt_p50_us: Vec<u64>,
    rtt_p95_us: Vec<u64>,
    /// Any error records.
    errors: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Read every record once, then fold in two passes: first learn the remote
    // NodeId for each (node, peer) from `connected` records, then key each pair
    // by the unordered {node, remote} so host and peer sides of the same
    // connection collapse into one pair.
    let mut records = Vec::new();
    let mut total_records = 0usize;
    let mut malformed = 0usize;

    for path in &cli.files {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), lineno + 1))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            total_records += 1;
            let record: Record = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(e) => {
                    malformed += 1;
                    eprintln!(
                        "warning: {}:{}: malformed JSON: {e}",
                        path.display(),
                        lineno + 1
                    );
                    continue;
                }
            };
            records.push(record);
        }
    }

    // Pass 1: (node, peer) -> remote NodeId, from `connected` records.
    let mut remote_of: HashMap<(String, usize), String> = HashMap::new();
    for r in &records {
        if r.kind == "connected" {
            if let Some(remote) = &r.remote {
                remote_of.insert((r.node.clone(), r.peer), remote.clone());
            }
        }
    }

    // Pass 2: fold into unordered pairs.
    let mut pairs: HashMap<(String, String), Pair> = HashMap::new();
    for r in &records {
        let remote = r
            .remote
            .clone()
            .or_else(|| remote_of.get(&(r.node.clone(), r.peer)).cloned());
        let Some(remote) = remote else {
            // No way to correlate this record to a pair; skip.
            continue;
        };
        let key = if r.node <= remote {
            (r.node.clone(), remote)
        } else {
            (remote, r.node.clone())
        };
        ingest(pairs.entry(key).or_default(), r);
    }

    // Build the report.
    let mut direct = 0usize;
    let mut relay_only = 0usize;
    let mut sent_direct = 0u64;
    let mut sent_total = 0u64;
    let mut dropped_total = 0u64;
    let mut ttds = Vec::new();
    let mut rtt50s = Vec::new();
    let mut rtt95s = Vec::new();

    for pair in pairs.values() {
        if pair.direct {
            direct += 1;
            sent_direct += pair.sent;
        } else if pair.relay {
            relay_only += 1;
        }
        sent_total += pair.sent;
        dropped_total += pair.dropped;
        if let Some(t) = pair.ttd_ms {
            ttds.push(t);
        }
        rtt50s.extend(pair.rtt_p50_us.iter().copied());
        rtt95s.extend(pair.rtt_p95_us.iter().copied());
    }

    let report = Report {
        records: total_records,
        malformed,
        pairs: pairs.len(),
        direct_pairs: direct,
        relay_only_pairs: relay_only,
        direct_rate: if pairs.is_empty() {
            0.0
        } else {
            direct as f64 / pairs.len() as f64
        },
        direct_bytes: if sent_total > 0 {
            sent_direct as f64 / sent_total as f64
        } else {
            0.0
        },
        dropped: dropped_total,
        ttd_ms: percentile(&mut ttds, 0.50),
        rtt_p50_us: percentile(&mut rtt50s, 0.50),
        rtt_p95_us: percentile(&mut rtt95s, 0.95),
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report, cli.min_direct_rate, cli.min_direct_bytes);
    }

    // CI gate: fail if below the configured thresholds.
    if cli.gate && report.pairs > 0 {
        let ok = report.direct_rate >= cli.min_direct_rate
            && report.direct_bytes >= cli.min_direct_bytes;
        if !ok {
            eprintln!(
                "GATE FAILED: direct rate {:.1}% < {:.1}% or direct bytes {:.1}% < {:.1}%",
                report.direct_rate * 100.0,
                cli.min_direct_rate * 100.0,
                report.direct_bytes * 100.0,
                cli.min_direct_bytes * 100.0
            );
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Fold one record into a pair.
fn ingest(pair: &mut Pair, r: &Record) {
    match r.kind.as_str() {
        "path" => match r.path.as_deref() {
            Some("direct") => {
                pair.direct = true;
                if pair.ttd_ms.is_none() {
                    pair.ttd_ms = r.ttd_ms;
                }
            }
            Some("relay") | Some("mixed") => {
                pair.relay = true;
            }
            _ => {}
        },
        "stats" => {
            pair.sent += r.sent.unwrap_or(0);
            pair.received += r.received.unwrap_or(0);
            pair.dropped += r.dropped.unwrap_or(0);
            if let Some(v) = r.rtt_p50_us {
                pair.rtt_p50_us.push(v);
            }
            if let Some(v) = r.rtt_p95_us {
                pair.rtt_p95_us.push(v);
            }
        }
        "error" => {
            if let Some(e) = &r.error {
                pair.errors.push(e.clone());
            }
        }
        _ => {}
    }
}

/// Percentile of a sorted sample (0..=1). Returns 0 for empty.
fn percentile(samples: &mut [u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[idx]
}

/// Machine-readable summary.
#[derive(Debug, Default, serde::Serialize)]
struct Report {
    records: usize,
    malformed: usize,
    pairs: usize,
    direct_pairs: usize,
    relay_only_pairs: usize,
    direct_rate: f64,
    direct_bytes: f64,
    dropped: u64,
    ttd_ms: u64,
    rtt_p50_us: u64,
    rtt_p95_us: u64,
}

fn print_human(r: &Report, min_rate: f64, min_bytes: f64) {
    println!("P0 punch-rate dashboard");
    println!("=======================");
    println!("records: {} ({} malformed)", r.records, r.malformed);
    println!("pairs:   {}", r.pairs);
    println!();
    println!(
        "direct pairs:   {} ({:.1}%)",
        r.direct_pairs,
        r.direct_rate * 100.0
    );
    println!(
        "relay-only:     {} ({:.1}%)",
        r.relay_only_pairs,
        if r.pairs > 0 {
            r.relay_only_pairs as f64 / r.pairs as f64 * 100.0
        } else {
            0.0
        }
    );
    println!(
        "direct bytes:   {:.1}%  (threshold {:.0}%)",
        r.direct_bytes * 100.0,
        min_bytes * 100.0
    );
    println!(
        "direct rate:    {:.1}%  (threshold {:.0}%)",
        r.direct_rate * 100.0,
        min_rate * 100.0
    );
    println!("dropped:        {}", r.dropped);
    println!("ttd p50:        {} ms", r.ttd_ms);
    println!("rtt p50/p95:    {} / {} µs", r.rtt_p50_us, r.rtt_p95_us);
    println!();
    let pass = r.direct_rate >= min_rate && r.direct_bytes >= min_bytes;
    println!(
        "threshold:      {}",
        if pass { "PASS" } else { "BELOW THRESHOLD" }
    );
}
