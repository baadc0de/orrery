//! `orrery-conformance` — emit, check and compare determinism reports.
//!
//! Three verbs, matching the three things CI needs:
//!
//! ```text
//! orrery-conformance emit    --out <file> [--compact]   # this platform's report
//! orrery-conformance check   --golden <file>            # vs the committed golden
//! orrery-conformance compare --baseline <file> <files…> # across the matrix
//! ```
//!
//! Argument parsing is hand-rolled rather than `clap`-driven on purpose: this
//! binary is built on five targets on every commit and its only job is to be
//! identical everywhere, so it takes no dependency it does not need.

use std::process::ExitCode;

use orrery_conformance::compare::compare;
use orrery_conformance::corpus::{run_all, Report};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str);

    let result = match verb {
        Some("emit") => emit(&args[1..]),
        Some("check") => check(&args[1..]),
        Some("compare") => compare_cmd(&args[1..]),
        Some("--help" | "-h") | None => {
            eprintln!("{}", USAGE);
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(format!("unknown verb: {other}\n\n{USAGE}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("orrery-conformance: {message}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
orrery-conformance — cross-platform determinism corpus (docs/06 §8)

  emit    --out <file> [--compact]     run the corpus, write this platform's report
  check   --golden <file>              run the corpus, compare against a committed golden
  compare --baseline <file> <files…>   compare reports from different platforms
";

/// Pull `--name <value>` out of the argument list.
fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn read_report(path: &str) -> Result<Report, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse {path}: {e}"))
}

fn emit(args: &[String]) -> Result<(), String> {
    let out = flag_value(args, "--out").ok_or("emit needs --out <file>")?;
    // `--compact` drops the per-tick detail: that is the shape the committed
    // golden takes, because the detail is diagnostic weight, not the thing
    // under test. The chain hash is identical either way.
    let detail = !args.iter().any(|a| a == "--compact");

    let report = run_all(detail);
    let json = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("cannot serialize report: {e}"))?;
    std::fs::write(&out, json).map_err(|e| format!("cannot write {out}: {e}"))?;

    eprintln!(
        "orrery-conformance: {} wrote {} cases to {out}",
        report.target,
        report.cases.len()
    );
    for case in &report.cases {
        eprintln!("  {:<18} {}", case.name, case.chain);
    }
    Ok(())
}

fn check(args: &[String]) -> Result<(), String> {
    let golden_path = flag_value(args, "--golden").ok_or("check needs --golden <file>")?;
    let golden = read_report(&golden_path)?;
    // Compact: the golden carries no per-tick detail, and comparing detail
    // against absent detail would be a false mismatch.
    let current = run_all(false);

    let divergences = compare(&golden, &current);
    if divergences.is_empty() {
        eprintln!(
            "orrery-conformance: {} matches the golden corpus ({} cases)",
            current.target,
            current.cases.len()
        );
        return Ok(());
    }

    let mut message = format!("{} diverges from {golden_path}:\n", current.target);
    for d in &divergences {
        message.push_str(&format!("  {d}\n"));
    }
    message.push_str(
        "\nIf the rules changed on purpose, bump REFERENCE_RULESET.version and \
         regenerate the golden with `emit --compact`.",
    );
    Err(message)
}

fn compare_cmd(args: &[String]) -> Result<(), String> {
    let baseline_path = flag_value(args, "--baseline").ok_or("compare needs --baseline <file>")?;
    let baseline = read_report(&baseline_path)?;

    // Everything that is not the flag or its value is a report to compare.
    let others: Vec<String> = {
        let mut v = Vec::new();
        let mut skip_next = false;
        for a in args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if a == "--baseline" {
                skip_next = true;
                continue;
            }
            v.push(a.clone());
        }
        v
    };
    if others.is_empty() {
        return Err("compare needs at least one report besides the baseline".into());
    }

    let mut failures = Vec::new();
    for path in &others {
        let other = read_report(path)?;
        let divergences = compare(&baseline, &other);
        if divergences.is_empty() {
            eprintln!(
                "orrery-conformance: {} == {} ({} cases)",
                baseline.target,
                other.target,
                other.cases.len()
            );
            continue;
        }
        for d in divergences {
            failures.push(format!("{} vs {}: {d}", baseline.target, other.target));
        }
    }

    if failures.is_empty() {
        eprintln!(
            "orrery-conformance: all {} platforms agree with {}",
            others.len(),
            baseline.target
        );
        return Ok(());
    }

    let mut message = String::from("cross-platform determinism FAILED:\n");
    for f in &failures {
        message.push_str(&format!("  {f}\n"));
    }
    Err(message)
}
