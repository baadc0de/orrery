//! The agreement table: `agree(ray) = both miss || (both hit && |d_unreal − d_rust| ≤ τ)`,
//! `rate(τ)` at τ ∈ {10, 50, 250, 1000} mm, the hit/miss disagreement count, max and p99 |Δd|,
//! the |Δd| distribution, and a breakdown by which actor answered on each side.

use std::error::Error;
use std::path::Path;

use serde::Serialize;

use crate::format::{read_hits, read_rays, Hit, KIND_INSTANCE, KIND_TERRAIN};

const TAUS_MM: [i64; 4] = [10, 50, 250, 1000];
const BUCKETS_MM: [i64; 9] = [0, 1, 2, 5, 10, 50, 250, 1000, i64::MAX];

#[derive(Serialize)]
pub struct Report {
    pub label: String,
    pub rays: usize,
    pub ray_seed: u64,
    pub unreal_trace_complex: u8,
    pub both_hit: usize,
    pub both_miss: usize,
    pub unreal_hit_rust_miss: usize,
    pub rust_hit_unreal_miss: usize,
    pub hit_miss_disagreements: usize,
    /// `rate(τ)` per τ, as a fraction of all rays.
    pub rate: Vec<RateRow>,
    pub max_abs_delta_mm: i64,
    pub p50_abs_delta_mm: i64,
    pub p99_abs_delta_mm: i64,
    /// Histogram of |Δd| over both-hit rays: bucket upper bounds in mm and counts.
    pub abs_delta_histogram: Vec<Bucket>,
    /// Both-hit rays where Unreal and the ruleset answered from different objects (terrain vs scatter).
    pub both_hit_different_actor: usize,
    pub unreal_hits_by_kind: KindCount,
    pub rust_hits_by_kind: KindCount,
    pub unreal_start_penetrating: usize,
    pub rust_start_penetrating: usize,
    /// Rays whose origin is under the terrain surface behave differently by construction
    /// (a solid representation answers 0; a surface answers the exit distance).
    pub origin_below_surface: usize,
    pub agreement_excluding_below_surface: Vec<RateRow>,
    /// The same table over only the rays where at least one side hit (both-miss rays agree trivially).
    pub rays_with_any_hit: usize,
    pub agreement_among_rays_with_any_hit: Vec<RateRow>,
    pub worst: Vec<Worst>,
}

#[derive(Serialize)]
pub struct RateRow {
    pub tau_mm: i64,
    pub agree: usize,
    pub rate: f64,
}

#[derive(Serialize)]
pub struct Bucket {
    pub upto_mm: i64,
    pub count: usize,
}

#[derive(Serialize, Default)]
pub struct KindCount {
    pub terrain: usize,
    pub instance: usize,
    pub other: usize,
}

#[derive(Serialize)]
pub struct Worst {
    pub ray: usize,
    pub unreal_mm: i64,
    pub rust_mm: i64,
    pub delta_mm: i64,
    pub unreal_kind: u8,
    pub rust_kind: u8,
    pub unreal_penetrating: bool,
    pub origin_below_surface: bool,
    pub cause: &'static str,
}

fn kind_count(hits: &[Hit]) -> KindCount {
    let mut k = KindCount::default();
    for h in hits.iter().filter(|h| h.hit) {
        match h.kind {
            KIND_TERRAIN => k.terrain += 1,
            KIND_INSTANCE => k.instance += 1,
            _ => k.other += 1,
        }
    }
    k
}

fn rates(pairs: &[(Hit, Hit)]) -> Vec<RateRow> {
    TAUS_MM
        .iter()
        .map(|&tau| {
            let agree = pairs
                .iter()
                .filter(|(u, r)| {
                    (!u.hit && !r.hit) || (u.hit && r.hit && (u.dist_mm - r.dist_mm).abs() <= tau)
                })
                .count();
            RateRow {
                tau_mm: tau,
                agree,
                rate: if pairs.is_empty() {
                    0.0
                } else {
                    agree as f64 / pairs.len() as f64
                },
            }
        })
        .collect()
}

pub fn run(
    unreal: &Path,
    rust: &Path,
    rays: &Path,
    out: Option<&Path>,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let u = read_hits(&std::fs::read(unreal)?)?;
    let r = read_hits(&std::fs::read(rust)?)?;
    let ray_file = read_rays(&std::fs::read(rays)?)?;
    if u.hits.len() != r.hits.len() || u.hits.len() != ray_file.rays.len() {
        return Err(format!(
            "length mismatch: unreal {} rust {} rays {}",
            u.hits.len(),
            r.hits.len(),
            ray_file.rays.len()
        )
        .into());
    }
    let pairs: Vec<(Hit, Hit)> = u.hits.iter().copied().zip(r.hits.iter().copied()).collect();

    // "Below surface": the ruleset's own vertical probe is not in the ray file, so use Unreal's answer:
    // a ray starting under the terrain that Unreal reports as start-penetrating or whose upward trace
    // hits terrain from below. Cheap proxy that needs no geometry here: Unreal's bStartPenetrating on a
    // terrain hit, or the ruleset answering 0 mm on terrain. Both sides are recorded per ray.
    let below: Vec<bool> = pairs
        .iter()
        .map(|(uh, rh)| {
            (uh.hit && uh.penetrating && uh.kind == KIND_TERRAIN)
                || (rh.hit && rh.dist_mm == 0 && rh.kind == KIND_TERRAIN)
        })
        .collect();

    let both_hit = pairs.iter().filter(|(a, b)| a.hit && b.hit).count();
    let both_miss = pairs.iter().filter(|(a, b)| !a.hit && !b.hit).count();
    let uh_rm = pairs.iter().filter(|(a, b)| a.hit && !b.hit).count();
    let rh_um = pairs.iter().filter(|(a, b)| !a.hit && b.hit).count();
    let mut deltas: Vec<i64> = pairs
        .iter()
        .filter(|(a, b)| a.hit && b.hit)
        .map(|(a, b)| (a.dist_mm - b.dist_mm).abs())
        .collect();
    deltas.sort_unstable();
    let pct = |p: f64| -> i64 {
        if deltas.is_empty() {
            0
        } else {
            deltas[((deltas.len() as f64 * p).ceil() as usize).clamp(1, deltas.len()) - 1]
        }
    };
    let histogram = BUCKETS_MM
        .windows(2)
        .map(|w| Bucket {
            upto_mm: w[1],
            count: deltas.iter().filter(|&&d| d > w[0] && d <= w[1]).count(),
        })
        .chain(core::iter::once(Bucket {
            upto_mm: 0,
            count: deltas.iter().filter(|&&d| d == 0).count(),
        }))
        .collect::<Vec<_>>();
    let different_actor = pairs
        .iter()
        .filter(|(a, b)| a.hit && b.hit && a.kind != b.kind)
        .count();

    let mut worst: Vec<Worst> = pairs
        .iter()
        .enumerate()
        .filter_map(|(i, (uh, rh))| {
            let delta = match (uh.hit, rh.hit) {
                (true, true) => (uh.dist_mm - rh.dist_mm).abs(),
                (false, false) => return None,
                _ => i64::MAX,
            };
            let cause = match (uh.hit, rh.hit) {
                (true, false) => "unreal hit, ruleset miss",
                (false, true) => "ruleset hit, unreal miss",
                _ if uh.kind != rh.kind => "different actor answered",
                _ if below[i] => "origin below surface (solid vs surface semantics)",
                _ if uh.penetrating || rh.penetrating => "start penetrating",
                _ if delta <= 2 => "millimetre rounding",
                _ => "geometry mismatch (representation coarseness)",
            };
            Some(Worst {
                ray: i,
                unreal_mm: uh.dist_mm,
                rust_mm: rh.dist_mm,
                delta_mm: delta,
                unreal_kind: uh.kind,
                rust_kind: rh.kind,
                unreal_penetrating: uh.penetrating,
                origin_below_surface: below[i],
                cause,
            })
        })
        .collect();
    worst.sort_by(|a, b| b.delta_mm.cmp(&a.delta_mm).then(a.ray.cmp(&b.ray)));
    worst.truncate(25);

    let excluding: Vec<(Hit, Hit)> = pairs
        .iter()
        .zip(below.iter())
        .filter(|(_, b)| !**b)
        .map(|(p, _)| *p)
        .collect();

    let any_hit: Vec<(Hit, Hit)> = pairs
        .iter()
        .filter(|(a, b)| a.hit || b.hit)
        .copied()
        .collect();

    let report = Report {
        label: label.to_string(),
        rays: pairs.len(),
        ray_seed: ray_file.seed,
        unreal_trace_complex: u.source,
        both_hit,
        both_miss,
        unreal_hit_rust_miss: uh_rm,
        rust_hit_unreal_miss: rh_um,
        hit_miss_disagreements: uh_rm + rh_um,
        rate: rates(&pairs),
        max_abs_delta_mm: deltas.last().copied().unwrap_or(0),
        p50_abs_delta_mm: pct(0.50),
        p99_abs_delta_mm: pct(0.99),
        abs_delta_histogram: histogram,
        both_hit_different_actor: different_actor,
        unreal_hits_by_kind: kind_count(&u.hits),
        rust_hits_by_kind: kind_count(&r.hits),
        unreal_start_penetrating: u.hits.iter().filter(|h| h.hit && h.penetrating).count(),
        rust_start_penetrating: r.hits.iter().filter(|h| h.hit && h.penetrating).count(),
        origin_below_surface: below.iter().filter(|b| **b).count(),
        agreement_excluding_below_surface: rates(&excluding),
        rays_with_any_hit: any_hit.len(),
        agreement_among_rays_with_any_hit: rates(&any_hit),
        worst,
    };
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(out) = out {
        std::fs::write(out, &text)?;
    }
    println!("{text}");
    Ok(())
}
