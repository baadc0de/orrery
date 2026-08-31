//! The migration benchmark harness (A10 F-12, stage S1.e of the #626
//! programme).
//!
//! Three verbs:
//!
//! - `capture` — run the suite, emit a *candidate* baseline document. It
//!   never writes into `docs/plans/baselines/` itself: committing a baseline
//!   is a human act, which is what makes the committed file mean something.
//! - `compare --baseline <path>` — the differential run. Refuses, without
//!   running anything, unless a committed baseline exists at the given path
//!   **and** its environment manifest matches this environment. These are
//!   distinct refusals with distinct exit codes: exit 3 for "no usable
//!   committed baseline", exit 4 for "environment mismatch".
//! - `--help`.
//!
//! The guarded stage is A10's baseline-refusal rule: **no differential run
//! without a committed baseline**. A comparison against nothing is worse than
//! no comparison, because it produces a number. Accordingly, a refusal emits
//! nothing on stdout — no partial report, no candidate metrics — only the
//! refusal on stderr and a non-zero exit.

#![warn(missing_docs)]

mod baseline;
mod capacity;
mod environment;
mod report;
mod suite;

use std::path::PathBuf;
use std::time::Instant;

use baseline::BaselineDocument;
use environment::EnvironmentManifest;

/// Exit code: a differential run refused because there is no usable committed
/// baseline at the path given. The message names the path.
const EXIT_NO_BASELINE: i32 = 3;
/// Exit code: a differential run refused because the baseline's environment
/// manifest does not match this environment. The message names every field
/// that differs.
const EXIT_ENVIRONMENT_MISMATCH: i32 = 4;
/// Exit code: bad usage.
const EXIT_USAGE: i32 = 2;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("capture") => capture(&args[1..]),
        Some("compare") => compare(&args[1..]),
        Some("--help" | "-h") | None => {
            usage();
            std::process::exit(EXIT_USAGE);
        }
        Some(other) => {
            eprintln!("migration-bench: unknown argument '{other}'");
            usage();
            std::process::exit(EXIT_USAGE);
        }
    }
}

fn usage() {
    println!(
        "migration-bench — A10 F-12: the migration benchmark harness and the \
         baseline-refusal rule

USAGE:
    migration-bench capture [--out <path>]
        Run the suite and emit a candidate baseline document (JSON). Never
        writes into docs/plans/baselines/ — review it, commit it there, and
        only then can a differential run refuse or proceed against it.

    migration-bench compare --baseline <path>
        The differential run. Refuses — without running anything, without
        emitting a report — unless a committed baseline exists at <path> and
        its environment matches this one. Ratios only; no thresholds are
        asserted here, ever (A10 §8.1).

REFUSALS:
    exit 3  no usable committed baseline at the path given
    exit 4  environment mismatch (toolchain, target triple, profile, CPU)
    exit 2  usage"
    );
}

/// `capture`: run the suite, emit the candidate.
fn capture(args: &[String]) {
    let mut out: Option<PathBuf> = None;
    let mut rest = args;
    while let Some(flag) = rest.first() {
        match flag.as_str() {
            "--out" => {
                let Some(value) = rest.get(1) else {
                    eprintln!("migration-bench: --out needs a path");
                    std::process::exit(EXIT_USAGE);
                };
                out = Some(PathBuf::from(value));
                rest = &rest[2..];
            }
            other => {
                eprintln!("migration-bench: unknown capture argument '{other}'");
                std::process::exit(EXIT_USAGE);
            }
        }
    }

    let started = Instant::now();
    let metrics = suite::run();
    let absent = suite::absent_legs(&metrics);
    let doc = BaselineDocument::capture(metrics, absent);
    let elapsed = started.elapsed();

    let rendered = match serde_json::to_string_pretty(&doc) {
        Ok(rendered) => rendered,
        Err(err) => {
            eprintln!("migration-bench: could not render the candidate: {err}");
            std::process::exit(1);
        }
    };
    match &out {
        Some(path) => {
            if let Err(err) = std::fs::write(path, rendered) {
                eprintln!("migration-bench: could not write {path:?}: {err}");
                std::process::exit(1);
            }
            eprintln!(
                "migration-bench: capture: candidate written to {}",
                path.display()
            );
        }
        None => println!("{rendered}"),
    }
    eprintln!(
        "migration-bench: capture: {} corpus + {} battery legs, {} absent legs, in {:.1}s",
        doc.metrics.b1_corpus.len(),
        doc.metrics.b1_battery.len(),
        doc.absent.len(),
        elapsed.as_secs_f64()
    );
    eprintln!(
        "migration-bench: capture: review, then commit as \
         docs/plans/baselines/a18-baseline-<date>.json — capture never writes there itself"
    );
}

/// `compare`: the differential run, behind the baseline-refusal rule.
fn compare(args: &[String]) {
    let mut baseline_path: Option<PathBuf> = None;
    let mut rest = args;
    while let Some(flag) = rest.first() {
        match flag.as_str() {
            "--baseline" => {
                let Some(value) = rest.get(1) else {
                    eprintln!("migration-bench: --baseline needs a path");
                    std::process::exit(EXIT_USAGE);
                };
                baseline_path = Some(PathBuf::from(value));
                rest = &rest[2..];
            }
            other => {
                eprintln!("migration-bench: unknown compare argument '{other}'");
                std::process::exit(EXIT_USAGE);
            }
        }
    }
    let Some(baseline_path) = baseline_path else {
        eprintln!(
            "migration-bench: refuse: no baseline given — a differential run without a \
             committed baseline is not a run (A10 §4.4, F-12)"
        );
        std::process::exit(EXIT_NO_BASELINE);
    };

    // Refusal 1: no usable committed baseline. Nothing has run yet, and
    // nothing will.
    let doc = match BaselineDocument::load(&baseline_path) {
        Ok(doc) => doc,
        Err(err @ baseline::BaselineError::Io(_)) => {
            eprintln!(
                "migration-bench: refuse: no committed baseline at {} — {err}",
                baseline_path.display()
            );
            eprintln!(
                "migration-bench: refuse: a differential run without a committed baseline \
                 is not a run — a comparison against nothing produces a number (A10 §4.4, F-12)"
            );
            std::process::exit(EXIT_NO_BASELINE);
        }
        Err(err @ baseline::BaselineError::Unusable(_)) => {
            eprintln!(
                "migration-bench: refuse: unusable committed baseline at {} — {err}",
                baseline_path.display()
            );
            eprintln!(
                "migration-bench: refuse: a comparison against an unusable baseline is \
                 still a comparison against nothing (A10 §4.4, F-12)"
            );
            std::process::exit(EXIT_NO_BASELINE);
        }
    };
    eprintln!(
        "migration-bench: compare: baseline {} loaded (captured {}, commit {})",
        baseline_path.display(),
        doc.captured_at,
        short(&doc.environment.commit)
    );

    // Refusal 2: environment mismatch. The baseline exists, but it is a
    // baseline for a different environment, and a ratio across environments
    // is a number about nothing.
    let candidate_environment = EnvironmentManifest::capture();
    let refusals = doc.environment.refusals(&candidate_environment);
    if !refusals.is_empty() {
        eprintln!(
            "migration-bench: refuse: environment mismatch — the baseline at {} was not \
             captured in this environment",
            baseline_path.display()
        );
        for refusal in &refusals {
            eprintln!(
                "migration-bench: refuse:   {}: baseline '{}', this environment '{}'",
                refusal.field, refusal.baseline, refusal.current
            );
        }
        eprintln!(
            "migration-bench: refuse: a differential run against a baseline from a \
             different environment produces a number about nothing (A10 §8.1, F-12); \
             capture and commit a baseline in this environment instead"
        );
        std::process::exit(EXIT_ENVIRONMENT_MISMATCH);
    }
    eprintln!(
        "migration-bench: compare: environment matched on rustc_version, target_triple, \
         build_profile, cpu.model"
    );
    if doc.environment.cargo_lock_blake3 != candidate_environment.cargo_lock_blake3 {
        eprintln!(
            "migration-bench: warning: this workspace's Cargo.lock differs from the \
             baseline's ({} vs {}) — the dependency graph moved; treat the ratios with \
             that in mind",
            short(&doc.environment.cargo_lock_blake3),
            short(&candidate_environment.cargo_lock_blake3)
        );
    }

    // The comparison itself. Benches observe: ratios, both environments, and
    // the standing statement that no threshold is asserted here.
    let started = Instant::now();
    let metrics = suite::run();
    eprintln!(
        "migration-bench: compare: candidate measured in {:.1}s ({} corpus + {} battery \
         legs; {} absent legs recorded by the baseline)",
        started.elapsed().as_secs_f64(),
        metrics.b1_corpus.len(),
        metrics.b1_battery.len(),
        doc.absent.len()
    );
    let candidate_map = suite::metric_map(&metrics);
    let baseline_map = doc.metrics();
    let rep = report::build(
        &baseline_path.display().to_string(),
        doc.environment.clone(),
        candidate_environment,
        &baseline_map,
        &candidate_map,
    );
    eprintln!(
        "migration-bench: compare: {} metrics compared, {} not in baseline, {} not in \
         candidate — observe only, no thresholds asserted (A10 §8.1)",
        rep.comparisons.len(),
        rep.not_in_baseline.len(),
        rep.not_in_candidate.len()
    );
    match serde_json::to_string_pretty(&rep) {
        Ok(rendered) => println!("{rendered}"),
        Err(err) => {
            eprintln!("migration-bench: could not render the report: {err}");
            std::process::exit(1);
        }
    }
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}
