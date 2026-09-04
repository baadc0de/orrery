//! Spike #1044 — the ruleset half of the one-body cook.
//!
//! `collision-trace rays|trace|compare|digest|sizes`. See `README.md` in this
//! crate for the exact reproduction commands. Every hit test in here is
//! integer-only (D43 envelope); floats appear in the ray *generator* and in
//! the report's percentages, never in a hit decision or a distance.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::missing_docs_in_private_items,
    clippy::struct_excessive_bools,
    clippy::similar_names,
    // Research-crate noise under the workspace's pedantic+nursery set; none of these
    // touch correctness and the crate never merges (#1042 rule 6).
    clippy::std_instead_of_core,
    clippy::missing_const_for_fn,
    clippy::trivially_copy_pass_by_ref,
    clippy::struct_field_names,
    clippy::needless_range_loop,
    clippy::doc_markdown,
    clippy::suboptimal_flops,
    clippy::option_if_let_else,
    clippy::suspicious_operation_groupings
)]

mod compare;
mod digest;
mod format;
mod geom;
mod hf;
mod rays;
mod tri;
mod vox;

use std::error::Error;
use std::path::PathBuf;

/// Minimal argument parsing: `--key value` pairs after the subcommand.
struct Args {
    cmd: String,
    kv: Vec<(String, String)>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut it = std::env::args().skip(1);
        let cmd = it
            .next()
            .ok_or("usage: collision-trace <rays|trace|compare|digest|sizes> --key value ...")?;
        let mut kv = Vec::new();
        while let Some(k) = it.next() {
            let key = k
                .strip_prefix("--")
                .ok_or_else(|| format!("expected --key, got {k}"))?
                .to_string();
            let v = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
            kv.push((key, v));
        }
        Ok(Self { cmd, kv })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.kv
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.kv
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn req(&self, key: &str) -> Result<&str, Box<dyn Error>> {
        self.get(key)
            .ok_or_else(|| format!("missing --{key}").into())
    }

    fn path(&self, key: &str) -> Result<PathBuf, Box<dyn Error>> {
        Ok(PathBuf::from(self.req(key)?))
    }

    fn num<T: core::str::FromStr>(&self, key: &str, default: T) -> Result<T, Box<dyn Error>>
    where
        T::Err: core::fmt::Display,
    {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v.parse::<T>().map_err(|e| format!("--{key}: {e}").into()),
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("collision-trace: {e}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    match args.cmd.as_str() {
        "rays" => rays::run(
            &args.path("collision")?,
            &args.path("out")?,
            args.num("n", 5000u32)?,
            args.num("seed", 42u64)?,
            args.num("max-mm", 500_000i64)?,
        ),
        "trace" => {
            let out = args.path("out")?;
            let stats = trace_file(&args.path("collision")?, &args.path("rays")?, &out)?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
            Ok(())
        }
        "compare" => compare::run(
            &args.path("unreal")?,
            &args.path("rust")?,
            &args.path("rays")?,
            args.get("out").map(PathBuf::from).as_deref(),
            args.get("label").unwrap_or("unnamed"),
        ),
        "digest" => digest::run(
            &args.path("unreal")?,
            &args.all("collision"),
            args.get("out").map(PathBuf::from).as_deref(),
        ),
        "sizes" => digest::sizes(
            &args.all("file"),
            args.get("out").map(PathBuf::from).as_deref(),
        ),
        other => Err(format!("unknown subcommand {other}").into()),
    }
}

/// Dispatch on the package magic, trace every ray, write the hit file, return timing and counts.
fn trace_file(
    collision: &std::path::Path,
    rays: &std::path::Path,
    out: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let bytes = std::fs::read(collision)?;
    let ray_file = format::read_rays(&std::fs::read(rays)?)?;
    let t_load = std::time::Instant::now();
    let tracer: Box<dyn geom::Tracer> = match &bytes[..8] {
        b"ORRYTRI1" => Box::new(tri::TriWorld::load(&bytes)?),
        b"ORRYHF_1" => Box::new(hf::HeightfieldWorld::load(&bytes)?),
        b"ORRYVOX1" => Box::new(vox::VoxelWorld::load(&bytes)?),
        other => return Err(format!("unknown collision magic {other:?}").into()),
    };
    let load_s = t_load.elapsed().as_secs_f64();
    let t_trace = std::time::Instant::now();
    let hits: Vec<format::Hit> = ray_file.rays.iter().map(|r| tracer.trace(r)).collect();
    let trace_s = t_trace.elapsed().as_secs_f64();
    let n_hit = hits.iter().filter(|h| h.hit).count();
    let n_terrain = hits
        .iter()
        .filter(|h| h.hit && h.kind == format::KIND_TERRAIN)
        .count();
    let n_inst = hits
        .iter()
        .filter(|h| h.hit && h.kind == format::KIND_INSTANCE)
        .count();
    let n_pen = hits.iter().filter(|h| h.hit && h.penetrating).count();
    std::fs::write(out, format::write_hits(&hits, format::SOURCE_RUST))?;
    Ok(serde_json::json!({
        "representation": tracer.name(),
        "collision": collision.display().to_string(),
        "rays": ray_file.rays.len(),
        "ray_seed": ray_file.seed,
        "load_and_build_s": load_s,
        "trace_s": trace_s,
        "hits": n_hit,
        "terrain_hits": n_terrain,
        "instance_hits": n_inst,
        "start_penetrating": n_pen,
        "out": out.display().to_string(),
    }))
}
