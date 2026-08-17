//! `orrery-seed` command-line entrypoint.
//!
//! The binary is a thin router over the library crate: it loads a scenario,
//! applies a profile overlay, validates it for the requested verb, and then
//! dispatches to the plan/apply/verify/wipe implementation.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use orrery_seed::apply::{self, ApplyOptions};
use orrery_seed::plan::{plan, PlanReport};
use orrery_seed::scenario::{ResolvedScenario, Scenario};
use orrery_seed::validate::{self, ValidationMode};
use orrery_seed::verify::{self, VerifyOptions};
use orrery_seed::wipe::{self, WipeOptions};

#[derive(Debug, Parser)]
struct PlanArgs {
    #[arg(value_name = "SCENARIO")]
    scenario: PathBuf,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    single_grid: bool,
}

#[derive(Debug, Parser)]
struct ApplyArgs {
    #[arg(value_name = "SCENARIO")]
    scenario: PathBuf,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    allow_opaque: bool,
    #[arg(long)]
    single_grid: bool,
}

#[derive(Debug, Parser)]
struct VerifyArgs {
    #[arg(value_name = "SCENARIO")]
    scenario: PathBuf,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    full: bool,
    /// Write the §9.3 manifest here: JSONL, one entry per line in
    /// `(grid, cell, ContentKey)` order, with the `content/version` record as
    /// the last line. Streamed, so the file's size is not bounded by memory.
    #[arg(long, value_name = "PATH")]
    emit_manifest: Option<PathBuf>,
    #[arg(long)]
    single_grid: bool,
}

/// `shards`: collapse an emitted manifest into the shard set the seeded world
/// occupies (docs/12-world-seeding.md §9.3, docs/08-persistence.md §3.1).
///
/// It reads a manifest and touches no cluster, so it builds and runs without
/// the `fdb` feature — a harness can derive the deployment's shard set from an
/// artifact long after the seeding run that produced it.
#[derive(Debug, Parser)]
struct ShardsArgs {
    /// The manifest emitted by `verify --emit-manifest`.
    #[arg(value_name = "MANIFEST")]
    manifest: PathBuf,
    /// The grid whose shards to report. persistd owns one grid per process.
    #[arg(long, default_value_t = 0)]
    grid: u32,
    /// Emit a JSON object (`grid`, `shard_level`, `entries`, `shards`) instead
    /// of one `--shard` operand per line.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct WipeArgs {
    #[arg(value_name = "SCENARIO")]
    scenario: PathBuf,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    yes: bool,
    #[arg(long, value_name = "CONTENT_BUILD")]
    content_build: String,
    #[arg(long)]
    single_grid: bool,
}

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() <= 1 {
        eprintln!("error: expected a verb or a scenario path");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: starting runtime: {e}");
            return ExitCode::from(2);
        }
    };
    match rt.block_on(dispatch(args)) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
    }
}

async fn dispatch(args: Vec<std::ffi::OsString>) -> Result<ExitCode, String> {
    if args.len() <= 1 {
        return Err("expected a scenario path".to_string());
    }
    let explicit = matches!(
        args[1].to_string_lossy().as_ref(),
        "plan" | "apply" | "verify" | "wipe" | "shards"
    );
    let command = if explicit {
        Some(args[1].to_string_lossy().to_string())
    } else {
        None
    };
    let argv = if explicit {
        let mut stripped = args.clone();
        stripped.remove(1);
        stripped
    } else {
        args
    };
    match command.as_deref() {
        Some("apply") => {
            let parsed = ApplyArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
            run_apply(parsed).await
        }
        Some("verify") => {
            let parsed = VerifyArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
            run_verify(parsed).await
        }
        Some("shards") => {
            let parsed = ShardsArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
            run_shards(&parsed)
        }
        Some("wipe") => {
            let parsed = WipeArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
            run_wipe(parsed).await
        }
        Some("plan") => {
            let parsed = PlanArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
            run_plan(parsed)
        }
        _ => {
            let parsed = PlanArgs::try_parse_from(argv).map_err(|e| e.to_string())?;
            run_plan(parsed)
        }
    }
}

fn load_resolved(
    path: &PathBuf,
    profile: Option<&str>,
) -> Result<(String, ResolvedScenario, String), String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let source = apply_profile(&source, profile)?;
    let scenario = Scenario::parse(&source).map_err(|e| e.to_string())?;
    let seed_display = match scenario.seed.scenario.as_str() {
        "random" => {
            let draw: [u8; 32] = rand::random();
            draw.iter().map(|b| format!("{b:02x}")).collect::<String>()
        }
        literal => literal.to_string(),
    };
    let resolved = scenario
        .resolve(seed_display.as_bytes().to_vec())
        .map_err(|e| e.to_string())?;
    Ok((source, resolved, seed_display))
}

fn run_plan(args: PlanArgs) -> Result<ExitCode, String> {
    let (source, resolved, seed_display) = load_resolved(&args.scenario, args.profile.as_deref())?;
    validate::validate(&source, &resolved, ValidationMode::Plan).map_err(|e| e.to_string())?;
    let report = plan(&resolved, &seed_display);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        print_summary(&report);
    }
    let _ = args.single_grid;
    Ok(ExitCode::SUCCESS)
}

async fn run_apply(args: ApplyArgs) -> Result<ExitCode, String> {
    let (source, resolved, _) = load_resolved(&args.scenario, args.profile.as_deref())?;
    validate::validate(&source, &resolved, ValidationMode::Apply).map_err(|e| e.to_string())?;
    let options = ApplyOptions {
        allow_opaque: args.allow_opaque,
        single_grid: args.single_grid,
    };
    let report = apply::run(&source, resolved, options)
        .await
        .map_err(|e| e.to_string())?;
    print_summary(&report.plan);
    println!(
        "  wrote             {} rows in {} txns",
        report.written_rows, report.batches
    );
    println!("  changed rows      {}", report.changed_rows);
    println!("  commit p99        {:.2} ms", report.commit_p99_ms);
    Ok(ExitCode::SUCCESS)
}

async fn run_verify(args: VerifyArgs) -> Result<ExitCode, String> {
    let (source, resolved, _) = load_resolved(&args.scenario, args.profile.as_deref())?;
    validate::validate(&source, &resolved, ValidationMode::Verify).map_err(|e| e.to_string())?;
    let report = verify::run(
        &source,
        resolved,
        VerifyOptions {
            full: args.full,
            emit_manifest: args.emit_manifest,
            single_grid: args.single_grid,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    println!("verify: {} rows checked", report.checked_rows);
    if let Some(path) = report.emit_manifest {
        println!("verify: manifest written to {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Print the shard set a seeded world occupies.
///
/// Default output is one `--shard` operand per line, so a harness can splice it
/// straight into a persistd command line. An empty shard set is an error and
/// not an empty list: it means the manifest named no entity in this grid, and a
/// deployment derived from it would silently own nothing.
fn run_shards(args: &ShardsArgs) -> Result<ExitCode, String> {
    use std::io::BufReader;

    let grid = orrery_protocol::GridId::new(args.grid);
    let file = std::fs::File::open(&args.manifest)
        .map_err(|e| format!("open {}: {e}", args.manifest.display()))?;
    let set = orrery_seed::shards::shard_set_from_manifest(BufReader::new(file), grid)?;
    if set.shards.is_empty() {
        return Err(format!(
            "{} names no entity in {grid}: there is no shard set to deploy",
            args.manifest.display()
        ));
    }
    if args.json {
        let doc = serde_json::json!({
            "grid": args.grid,
            "shard_level": orrery_protocol::SHARD_LEVEL,
            "entries": set.entries,
            "skipped_other_grid": set.skipped_other_grid,
            "shards": set.flag_operands(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?
        );
    } else {
        for operand in set.flag_operands() {
            println!("{operand}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_wipe(args: WipeArgs) -> Result<ExitCode, String> {
    let (source, resolved, _) = load_resolved(&args.scenario, args.profile.as_deref())?;
    validate::validate(&source, &resolved, ValidationMode::Wipe).map_err(|e| e.to_string())?;
    wipe::run(
        &source,
        resolved,
        WipeOptions {
            yes: args.yes,
            typed_content_build: args.content_build,
            single_grid: args.single_grid,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

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
        .cloned()
        .ok_or_else(|| format!("no [profile.{profile}] in the scenario"))?;
    merge_table(&mut doc, &overlay);
    toml::to_string(&doc).map_err(|e| format!("re-serializing scenario: {e}"))
}

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
