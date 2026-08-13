//! The `orrery-seed` reference binary (docs/12-world-seeding.md §4).
//!
//! A thin wrapper over the [`orrery_seed`] library (D12): it loads a TOML
//! scenario, applies a `--profile` overlay (C-5), and runs the requested
//! verb. `plan` is the **default verb and never writes** (§7.3 — the
//! Terraform posture). The write verbs (`apply`/`verify`/`wipe`) are gated
//! behind the `fdb` feature and not implemented in v1; invoking them is a
//! typed "unsupported in v1" error, not a stub.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use orrery_seed::plan::{plan, PlanReport};
use orrery_seed::scenario::Scenario;

/// Command-line interface for the world seeder.
#[derive(Debug, Parser)]
#[command(
    name = "orrery-seed",
    about = "Orrery offline world seeder (docs/12-world-seeding.md)"
)]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

/// The verbs. `plan` is the default (§7.3).
#[derive(Debug, Subcommand)]
enum Verb {
    /// The analytic dry run: exact counts, distribution, byte estimate,
    /// manifest digest. Never writes (§7.3). Default verb.
    Plan {
        /// The scenario TOML file.
        #[arg(value_name = "SCENARIO")]
        scenario: String,
        /// A `[profile.<name>]` overlay (C-5).
        #[arg(long)]
        profile: Option<String>,
        /// Emit the machine-readable JSON report to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Bulk-write the world (docs/12 §11). Gated behind `fdb`; not in v1.
    Apply {
        /// The scenario TOML file.
        #[arg(value_name = "SCENARIO")]
        scenario: String,
        /// A `[profile.<name>]` overlay.
        #[arg(long)]
        profile: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.verb {
        Verb::Plan {
            scenario,
            profile,
            json,
        } => run_plan(&scenario, profile.as_deref(), json),
        Verb::Apply { .. } => {
            eprintln!(
                "error: `apply` is unsupported in v1 — the FDB write path (docs/12 §11) is gated behind the `fdb` feature and lands with the writer task"
            );
            ExitCode::from(2)
        }
    }
}

/// Load, resolve, and plan a scenario. `plan` never writes.
fn run_plan(path: &str, profile: Option<&str>, json: bool) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: reading {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let source = match apply_profile(&source, profile) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(2);
        }
    };
    let scenario = match Scenario::parse(&source) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    // §8 item 5: the seed is an OUTPUT. For `scenario = "random"` the OS
    // draw is printed as the FIRST line, before anything else happens.
    let (material, display) = match scenario.seed.scenario.as_str() {
        "random" => {
            let draw: [u8; 32] = rand::random();
            let hex: String = draw.iter().map(|b| format!("{b:02x}")).collect();
            println!(
                "seed = \"{hex}\"   (drawn from the OS; paste into [seed] scenario to reproduce)"
            );
            (draw.to_vec(), hex)
        }
        literal => (literal.as_bytes().to_vec(), literal.to_string()),
    };

    let resolved = match scenario.resolve(material) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let report = plan(&resolved, &display);
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(j) => println!("{j}"),
            Err(e) => {
                eprintln!("error: serializing plan: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        print_summary(&report);
    }
    ExitCode::SUCCESS
}

/// Apply a `[profile.<name>]` overlay (C-5, docs/12 §18.6): tables merge
/// key-wise, scalars override, arrays of tables replace wholesale.
fn apply_profile(source: &str, profile: Option<&str>) -> Result<String, String> {
    let Some(profile) = profile else {
        return Ok(source.to_string());
    };
    let mut doc: toml::Table =
        toml::from_str(source).map_err(|e| format!("parsing scenario: {e}"))?;
    let overlays = doc
        .get("profile")
        .and_then(|p| p.as_table())
        .cloned()
        .unwrap_or_default();
    let overlay = overlays
        .get(profile)
        .and_then(|p| p.as_table())
        .ok_or_else(|| format!("no [profile.{profile}] in the scenario"))?
        .clone();
    merge_table(&mut doc, &overlay);
    toml::to_string(&doc).map_err(|e| format!("re-serializing scenario: {e}"))
}

/// Key-wise table merge (docs/12 §18.6): scalars override, tables recurse,
/// arrays replace wholesale.
fn merge_table(base: &mut toml::Table, overlay: &toml::Table) {
    for (k, v) in overlay {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_table(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// The terminal summary (docs/12 §12.1).
fn print_summary(r: &PlanReport) {
    println!("orrery-seed plan");
    println!("  seed          {}", r.seed);
    println!("  scenario      {}", r.scenario);
    println!(
        "  plan tier     {:<24} payload class: {}",
        match r.oracle_tier {
            orrery_seed::plan::OracleTier::Analytic => "analytic",
        },
        r.payload_class
    );
    for l in &r.layers {
        println!(
            "  layer  {:<11} uniform  → {:?}, {} cells",
            l.name, l.into, l.field_cells
        );
    }
    for e in &r.emits {
        println!(
            "  emit   {:<11} entity   {:<12} → {} rows (achieved == target)",
            e.name, e.target_count, e.achieved_count
        );
        println!(
            "    entities/cell  p50 {}  p90 {}  p99 {}  max {}",
            e.p50, e.p90, e.p99, e.max
        );
    }
    println!(
        "  cells occupied  {} / {}  ({:.1}%)",
        r.occupied_cells,
        r.candidate_cells,
        r.occupied_fraction * 100.0
    );
    println!("  rows            {} world", r.total_entities);
    println!(
        "  logical bytes   {} ({:.2} MiB)",
        r.total_logical_bytes,
        r.total_logical_bytes as f64 / 1048576.0
    );
    // Achieved-vs-target for every declared target (§7.2), then any
    // limit violations (V10).
    for t in &r.targets {
        let mark = if t.within { "✓" } else { "✗" };
        println!(
            "  target {:<18} target {:.3}  achieved {:.4}  Δ {:+.4}  {} (tol {:.2})",
            t.name,
            t.target,
            t.achieved,
            t.achieved - t.target,
            mark,
            t.tolerance
        );
    }
    for v in &r.limit_violations {
        println!("  ⚠ LIMIT: {v}");
    }
    println!("  manifest        {}", r.manifest_digest);
    println!("  toolchain       {}", r.toolchain);
}
