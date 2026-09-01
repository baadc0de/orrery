//! The analytic dry run: `plan` (docs/12-world-seeding.md §7.3, §12.1).
//!
//! `plan` is the **default verb and never writes** — the Terraform posture
//! (§7.3). For a closed-form scenario (v1: `uniform` + `union` only) the
//! analytic tier runs in milliseconds with no cluster and no FDB: exact
//! entity count (D-B, honoured by construction), occupied-cell count,
//! per-shard distribution, byte estimate, manifest digest.
//!
//! **Per-row cost model correction.** docs/12 §13.1 assumes a 17-byte
//! untagged `world/` row. The landed keyspace (P-7, P-6;
//! `orrery_persistd::keyspace`) is a **21-byte key** — `b'w' ‖ grid(4) ‖
//! cell(8) ‖ entity(8)` ([`world_key`](orrery_persistd::keyspace::world_key)) — plus a **1-byte value tag**
//! (`LIVE_TAG`/`TOMBSTONE_TAG`) plus the bag. This module uses the landed
//! shape; §13.2's ladder numbers move slightly as a result, and that is
//! expected (the plan states the model it used).

use std::collections::BTreeMap;

use orrery_protocol::CellId;
use serde::Serialize;

use crate::content::{ContentKey, ContentKeyPreimage};
use crate::encode::{OpaqueEncoder, SeedEncoder};
use crate::field::Q16_16;
use crate::manifest::{value_digest, ManifestWriter, ToolchainStamp};
use crate::scenario::{ResolvedEmit, ResolvedLayer, ResolvedScenario};
use crate::seedtree::SeedRoot;
use crate::split::{split_cell, FieldOracle};
use orrery_persistd::keyspace::encode_versioned_live_value;
use orrery_protocol::atrest::SchemaVersion;

/// The landed per-row overhead: 21-byte `world/` key + 1-byte value tag +
/// 4-byte schema floor (`orrery_persistd::keyspace::world_key`,
/// `LIVE_VERSIONED_TAG`). See the module docs: this supersedes docs/12
/// §13.1's 17-byte untagged model.
///
/// The floor is what D38 clause (d)(2) buys, and it is priced here rather than
/// hidden: 4 B a row is 40 MB across 10^7 rows, noise against the bags
/// themselves, and it is the field that lets a sweep decide staleness without
/// opening one.
pub const WORLD_ROW_OVERHEAD: usize = 21 + 1 + 4;

/// Which dry-run tier a scenario is in (docs/12 §7.3). v1 implements the
/// analytic tier only; iterative generators (which would degrade the oracle
/// to "generate the bounded region") are rejected at resolve time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleTier {
    /// Closed-form field with an O(depth) `field_mass` oracle — the plan is
    /// exact and millisecond-cheap (§7.1).
    Analytic,
}

/// Per-layer plan line (§12.1: achieved-vs-target with deltas).
#[derive(Debug, Clone, Serialize)]
pub struct LayerPlan {
    /// Layer name.
    pub name: String,
    /// Generator kind (always `uniform` in v1).
    pub kind: String,
    /// Cells with positive mass in the layer's bounds.
    pub field_cells: u64,
    /// The accumulator this layer folds into.
    pub into: String,
    /// Cells clamped by the post-fold `field_clamp` (§5.3) — a non-zero
    /// count is almost always a config bug, so the plan reports it.
    pub clamped_cells: u64,
}

/// Per-emit plan line.
#[derive(Debug, Clone, Serialize)]
pub struct EmitPlan {
    /// Emit name.
    pub name: String,
    /// Declared count (D-B) and achieved count — equal by construction.
    pub target_count: u64,
    /// Achieved count (the splitter is exact; this asserts it).
    pub achieved_count: u64,
    /// Occupied cells at the emit level.
    pub occupied_cells: u64,
    /// The entity-per-cell histogram: `(per-cell count, number of cells)`,
    /// sorted by per-cell count (§12.1).
    pub cell_histogram: BTreeMap<u64, u64>,
    /// p50/p90/p99/max of entities per occupied cell.
    pub p50: u64,
    /// p90.
    pub p90: u64,
    /// p99.
    pub p99: u64,
    /// max.
    pub max: u64,
    /// Cells per level emitted into (level → cell count), sorted.
    pub level_distribution: BTreeMap<u8, u64>,
    /// Per-shard entity counts (shard bits → count), sorted by bits.
    pub shard_distribution: BTreeMap<u64, u64>,
    /// Logical byte estimate: Σ over rows of ([`WORLD_ROW_OVERHEAD`] + bag).
    pub logical_bytes: u64,
    /// `world/` rows written (entity rows; terrain is not durable state in v1).
    pub world_rows: u64,
}

/// One realized entity row, including the storage value written by the
/// seeder. The manifest entry stays separate so dry-run reporting and write
/// path batching can share the same deterministic ordering without re-encoding
/// the scenario twice.
#[derive(Debug, Clone)]
pub struct PlannedRow {
    /// The manifest entry for the row.
    pub entry: crate::manifest::ManifestEntry,
    /// The landed `world/` value: `LIVE_VERSIONED_TAG || schema floor || bag`.
    pub value: Vec<u8>,
}

/// The whole plan report (§12.1): machine-readable JSON plus what the
/// terminal summary is rendered from.
#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    /// Scenario name.
    pub scenario: String,
    /// The copy-pasteable scenario seed (§8 item 5: printed first).
    pub seed: String,
    /// The derivation context (§8 item 2).
    pub context: String,
    /// Which oracle tier ran (§7.3).
    pub oracle_tier: OracleTier,
    /// Payload class (§4.1; the shipped binary is `opaque`).
    pub payload_class: String,
    /// Per-layer lines.
    pub layers: Vec<LayerPlan>,
    /// Per-emit lines.
    pub emits: Vec<EmitPlan>,
    /// Total entities across emits.
    pub total_entities: u64,
    /// Occupied cells across emits (union of emit-level cells).
    pub occupied_cells: u64,
    /// Total candidate cells in bounds (Σ layer bounds at the emit level).
    pub candidate_cells: u64,
    /// Occupied fraction (occupied / candidate).
    pub occupied_fraction: f64,
    /// Total logical bytes.
    pub total_logical_bytes: u64,
    /// The rolling manifest digest (hex).
    pub manifest_digest: String,
    /// Entries in the manifest.
    pub manifest_entries: u64,
    /// The toolchain stamp's rustc string.
    pub toolchain: String,
    /// Achieved-vs-target for every declared `[target]` (§7.2: "the plan
    /// reports achieved-vs-target for every target, not just the ones that
    /// failed").
    pub targets: Vec<TargetOutcome>,
    /// `[limits]` violations (V10). Empty means the plan is inside every
    /// declared guard.
    pub limit_violations: Vec<String>,
}

/// One achieved-vs-target line (docs/12 §7.2). Non-exact targets carry the
/// scenario's `tolerance`; `within` records whether the target was met.
#[derive(Debug, Clone, Serialize)]
pub struct TargetOutcome {
    /// The target name (`count`, `occupied_fraction`, …).
    pub name: String,
    /// The declared target value.
    pub target: f64,
    /// The achieved value.
    pub achieved: f64,
    /// The tolerance applied (0 for exact targets like `count`).
    pub tolerance: f64,
    /// Whether `|achieved − target| ≤ tolerance` (or equality for exact).
    pub within: bool,
}

/// A uniform-field oracle over one layer's resolved bounds (docs/12 §6.1):
/// a constant per-cell weight function of `(K_layer, cell)` restricted to
/// `bounds`, with an O(depth) `field_mass(subtree)` oracle (§7.1).
struct UniformOracle<'a> {
    layer: &'a ResolvedLayer,
}

impl FieldOracle for UniformOracle<'_> {
    fn field_mass(&self, cell: CellId) -> Q16_16 {
        let quanta = self
            .layer
            .bounds
            .field_mass_under(cell, self.layer.intensity);
        // The mass can exceed Q16.16's i32 range for large subtrees; the
        // splitter only compares ratios, and per-cell intensity ≤ 64 (the
        // clamp) keeps any plausible subtree under 2^31 quanta for the
        // levels the split actually descends. Saturate rather than wrap.
        Q16_16(i32::try_from(quanta.min(i128::from(i32::MAX) as u64)).unwrap_or(i32::MAX))
    }
}

/// Run the analytic plan over a resolved scenario (docs/12 §7.3, §12.1).
/// Never writes.
#[must_use]
pub fn plan(scenario: &ResolvedScenario, seed_display: &str) -> PlanReport {
    let root = SeedRoot::derive(&scenario.seed_context, &scenario.seed_material);
    let stamp = ToolchainStamp::current();

    let mut layers = Vec::new();
    let mut emits = Vec::new();
    let mut manifest = ManifestWriter::new();
    let mut all_occupied: std::collections::BTreeSet<CellId> = std::collections::BTreeSet::new();
    let mut candidate_cells = 0u64;
    let mut total_logical = 0u64;

    for layer in &scenario.layers {
        candidate_cells += layer.bounds.cell_count();
        layers.push(LayerPlan {
            name: layer.name.clone(),
            kind: "uniform".to_string(),
            field_cells: layer.bounds.cell_count(),
            into: layer.into.clone(),
            clamped_cells: 0, // uniform intensity ≤ 64 by validation; see below
        });
    }

    let mut total_entities = 0u64;
    for emit in &scenario.emits {
        // The accumulator the emit reads: union of the layers feeding it.
        // v1 has a single layer per accumulator, so the oracle is that
        // layer's bounds/intensity (validated: one layer into "main").
        let layer = scenario
            .layers
            .iter()
            .find(|l| l.into == emit.from)
            .unwrap_or_else(|| {
                panic!(
                    "emit {:?} reads accumulator {:?} with no layer",
                    emit.name, emit.from
                )
            });
        let oracle = UniformOracle { layer };

        let mut cell_counts: BTreeMap<CellId, u64> = BTreeMap::new();
        split_cell(
            &oracle,
            CellId::ROOT,
            emit.count,
            emit.level,
            &mut |cell, count| {
                cell_counts.insert(cell, count);
            },
        );

        let achieved: u64 = cell_counts.values().sum();
        debug_assert_eq!(achieved, emit.count, "the splitter is exact (D-B)");

        // Per-cell histogram and distribution stats.
        let mut histogram: BTreeMap<u64, u64> = BTreeMap::new();
        let mut level_dist: BTreeMap<u8, u64> = BTreeMap::new();
        let mut shard_dist: BTreeMap<u64, u64> = BTreeMap::new();
        let mut sorted_counts: Vec<u64> = Vec::with_capacity(cell_counts.len());
        for (&cell, &count) in &cell_counts {
            *histogram.entry(count).or_insert(0) += 1;
            *level_dist.entry(cell.level()).or_insert(0) += 1;
            let shard = orrery_protocol::shard_of(cell);
            *shard_dist.entry(shard.to_bits()).or_insert(0) += count;
            sorted_counts.push(count);
            all_occupied.insert(cell);
        }
        sorted_counts.sort_unstable();
        let pct = |p: f64| -> u64 {
            if sorted_counts.is_empty() {
                return 0;
            }
            let idx = ((sorted_counts.len() - 1) as f64 * p).round() as usize;
            sorted_counts[idx.min(sorted_counts.len() - 1)]
        };

        // Stream manifest entries in (grid, cell, ContentKey) ascending —
        // generation order (§9.3). The entry stream is a pure function of
        // (scenario, emit, seed root), factored into [`emit_manifest_entries`]
        // so the writer's parallel-subtree partition can re-fold it.
        let entries = emit_manifest_entries(scenario, emit, layer, &root);
        let mut logical_bytes = 0u64;
        for e in &entries {
            logical_bytes += (WORLD_ROW_OVERHEAD + e.byte_len as usize) as u64;
        }
        for e in entries {
            manifest.push(e);
        }

        total_entities += achieved;
        total_logical += logical_bytes;
        emits.push(EmitPlan {
            name: emit.name.clone(),
            target_count: emit.count,
            achieved_count: achieved,
            occupied_cells: cell_counts.len() as u64,
            cell_histogram: histogram,
            p50: pct(0.50),
            p90: pct(0.90),
            p99: pct(0.99),
            max: sorted_counts.last().copied().unwrap_or(0),
            level_distribution: level_dist,
            shard_distribution: shard_dist,
            logical_bytes,
            world_rows: achieved,
        });
    }

    let digest = manifest.finish(&stamp);
    let occupied = all_occupied.len() as u64;
    let occupied_fraction = if candidate_cells == 0 {
        0.0
    } else {
        occupied as f64 / candidate_cells as f64
    };

    // Achieved-vs-target for every declared [target] (§7.2) and the
    // [limits] guards (V10) — evaluated at plan time so an over-limit
    // projection is discovered before any write.
    let targets = evaluate_targets(scenario, total_entities, occupied_fraction);
    let limit_violations = check_limits(scenario, total_entities, total_logical);

    PlanReport {
        scenario: scenario.raw.scenario.name.clone(),
        seed: seed_display.to_string(),
        context: scenario.seed_context.clone(),
        oracle_tier: OracleTier::Analytic,
        payload_class: scenario
            .raw
            .payload
            .class
            .clone()
            .unwrap_or_else(|| "opaque".to_string()),
        layers,
        emits,
        total_entities,
        occupied_cells: occupied,
        candidate_cells,
        occupied_fraction,
        total_logical_bytes: total_logical,
        manifest_digest: hex32(&digest),
        manifest_entries: total_entities,
        toolchain: stamp.rustc,
        targets,
        limit_violations,
    }
}

/// Evaluate `[target]` against the achieved plan (docs/12 §7.2). Only the
/// targets v1 can compute are evaluated (`count`, `occupied_fraction`);
/// targets requiring the solver or the sampled tier (`gini`,
/// `hot_shard_share`, `hotspots`, `max_bytes` inversion) are skipped with a
/// note rather than silently reported as met.
fn evaluate_targets(
    scenario: &ResolvedScenario,
    total_entities: u64,
    occupied_fraction: f64,
) -> Vec<TargetOutcome> {
    let t = &scenario.raw.target;
    let tol = t.tolerance.unwrap_or(0.0);
    let mut out = Vec::new();
    if let Some(count) = t.count {
        out.push(TargetOutcome {
            name: "count".to_string(),
            target: count as f64,
            achieved: total_entities as f64,
            tolerance: 0.0, // exact (D-B)
            within: total_entities == count,
        });
    }
    if let Some(frac) = t.occupied_fraction {
        out.push(TargetOutcome {
            name: "occupied_fraction".to_string(),
            target: frac,
            achieved: occupied_fraction,
            tolerance: tol,
            within: (occupied_fraction - frac).abs() <= tol,
        });
    }
    out
}

/// The `[limits]` guards (docs/12 §10, V10): `max_entities`, `max_bytes`.
/// `max_wall_clock` cannot be evaluated by an analytic plan and is left to
/// the writer; `protect` guards `wipe` (§9.5), not `plan`.
fn check_limits(
    scenario: &ResolvedScenario,
    total_entities: u64,
    total_logical_bytes: u64,
) -> Vec<String> {
    let l = &scenario.raw.limits;
    let mut violations = Vec::new();
    if let Some(max) = l.max_entities {
        if total_entities > max {
            violations.push(format!(
                "max_entities: plan realizes {total_entities} entities, over the limit {max} (V10)"
            ));
        }
    }
    if let Some(max_bytes) = &l.max_bytes {
        match crate::scenario::parse_byte_size(max_bytes) {
            Ok(max) => {
                if total_logical_bytes > max as u64 {
                    violations.push(format!(
                        "max_bytes: plan projects {total_logical_bytes} logical bytes, over the limit {max} (V10)"
                    ));
                }
            }
            Err(e) => violations.push(format!("max_bytes: {e}")),
        }
    }
    violations
}

/// Generate one emit's manifest entries in canonical `(grid, cell,
/// ContentKey)` ascending order (docs/12 §9.3) — a pure function of the
/// scenario, the emit, its feeding layer, and the seed root.
///
/// Factored out of [`plan`] so the writer's parallel-subtree decomposition
/// (§7.1 property 3: each worker owns a contiguous `CellId` range) can
/// partition the stream and re-fold partial manifests, and so the
/// thread-count-invariance test exercises the same code the writer will.
///
/// The plan encodes each bag exactly once and uses the MEASURED length for
/// both the byte estimate and the value digest — the analytic tier stays
/// cheap because the opaque bag is a fixed-size ChaCha fill (µs each), and
/// "reported, not assumed" (§4.1) beats a declared-size shortcut that could
/// drift from the encoder.
#[must_use]
pub fn emit_manifest_entries(
    scenario: &ResolvedScenario,
    emit: &ResolvedEmit,
    layer: &ResolvedLayer,
    root: &SeedRoot,
) -> Vec<crate::manifest::ManifestEntry> {
    emit_rows(scenario, emit, layer, root)
        .into_iter()
        .map(|row| row.entry)
        .collect()
}

/// Build the deterministic entity rows for one emit, including the landed
/// `world/` value. This is the shared seam used by the writer and by
/// verification; the dry-run manifest is just the entry side of the same
/// stream.
#[must_use]
pub fn emit_rows(
    scenario: &ResolvedScenario,
    emit: &ResolvedEmit,
    layer: &ResolvedLayer,
    root: &SeedRoot,
) -> Vec<PlannedRow> {
    let oracle = UniformOracle { layer };
    let mut cell_counts: BTreeMap<CellId, u64> = BTreeMap::new();
    split_cell(
        &oracle,
        CellId::ROOT,
        emit.count,
        emit.level,
        &mut |cell, count| {
            cell_counts.insert(cell, count);
        },
    );

    let layer_key = root.layer_key(&layer.name);
    let mix: Vec<crate::place::ArchetypeWeight> = emit
        .archetypes
        .iter()
        .map(|(n, w)| crate::place::ArchetypeWeight {
            name: n.clone(),
            weight: *w,
        })
        .collect();
    let cell_edge = scenario
        .grids
        .get(&emit.grid.0)
        .map_or(orrery_protocol::DEFAULT_CELL_EDGE_M, |g| g.cell_edge_m);

    let mut out = Vec::new();
    for (&cell, &count) in &cell_counts {
        let cell_key = SeedRoot::cell_key(&layer_key, cell);
        let assignments = crate::place::apportion_archetypes(count, &mix, cell_key);
        // Within a cell the manifest order is ContentKey-ascending (§9.3's
        // (grid, cell, ContentKey)), which is NOT slot-index order —
        // ContentKey is a hash. Build the cell's rows, sort by ContentKey,
        // then append. Cells are few-entity, so the per-cell sort is tiny.
        let mut rows: Vec<PlannedRow> = Vec::with_capacity(count as usize);
        for (index, &arch_idx) in assignments.iter().enumerate() {
            let archetype = &emit.archetypes[arch_idx as usize].0;
            let slot_key = SeedRoot::slot_key(&cell_key, index as u64);
            let ck = ContentKey::derive(&ContentKeyPreimage {
                scenario: &scenario.raw.scenario.name,
                emit: &emit.name,
                layer: &layer.name,
                grid: emit.grid,
                cell,
                index: index as u64,
                archetype,
            });
            let local_pos = crate::place::hash_local_pos(slot_key, cell_edge as f32);
            let persist_id = orrery_protocol::PersistId::new(
                // Deterministic plan-mode id stream: cell low bits folded
                // with the slot index. The writer mints real ids from
                // pid/next block grants (§9.2); plan ids fill the row.
                (cell.to_bits() & 0xFFFF_FFFF) << 16 | (index as u64 & 0xFFFF),
            );
            let bag = encode_bag(
                scenario, emit, archetype, cell, local_pos, slot_key, ck, persist_id,
            );
            let entry = crate::manifest::ManifestEntry {
                content_key: ck,
                persist_id,
                grid: emit.grid,
                cell,
                value_digest: value_digest(&bag),
                byte_len: bag.len() as u32,
                archetype: archetype.clone(),
                layer: layer.name.clone(),
                emit: emit.name.clone(),
            };
            rows.push(PlannedRow {
                entry,
                value: encode_versioned_live_value(
                    bag_schema_floor(scenario, emit, archetype),
                    &bag,
                ),
            });
        }
        rows.sort_by_key(|e| e.entry.content_key);
        out.extend(rows);
    }
    out
}

/// Encode one bag for the manifest digest or the write path (opaque path; the
/// hex escape when the archetype declares one).
// Every argument is a distinct part of the row identity the encoder needs;
// bundling them into a struct would only move the same eight values.
#[allow(clippy::too_many_arguments)]
pub fn encode_bag(
    scenario: &ResolvedScenario,
    emit: &ResolvedEmit,
    archetype: &str,
    cell: CellId,
    local_pos: [f32; 3],
    slot_key: [u8; 32],
    content_key: ContentKey,
    persist_id: orrery_protocol::PersistId,
) -> bytes::Bytes {
    let fields = scenario
        .archetypes
        .get(archetype)
        .unwrap_or_else(|| panic!("emit {:?} archetype {archetype:?} validated", emit.name));
    if let Some(hex) = &fields.bytes_hex {
        return crate::encode::encode_hex_escape(hex).expect("validated at resolve");
    }
    let mut rng = SeedRoot::slot_rng(slot_key);
    let ctx = crate::encode::EncodeCtx {
        archetype,
        fields,
        cell,
        grid: emit.grid,
        local_pos,
        content_key,
        persist_id,
        rng: &mut rng,
    };
    OpaqueEncoder
        .encode(&ctx)
        .expect("opaque encode of a validated archetype")
}

/// The schema floor the writer stamps into an archetype's `world/` envelope
/// (D38 clause (d)(2)).
///
/// The seeder is the one first-party producer of `world/` bags today, and it
/// already knows the number: `[archetype.…] schema_version` is the scenario's
/// declaration of what shape the bag it asks for is in (docs/12 §5.5), and
/// [`crate::encode::OpaqueEncoder`] writes that same number *inside* the bag.
/// The envelope floor is therefore a restatement of the bag's own content at a
/// fixed offset, which is exactly what clause (d)(2) asks the marker to be —
/// derivable from what it describes, not an independent counter that can drift
/// away from it.
///
/// One archetype's bag is one slot's worth of schema today, so the minimum
/// over its slots is that one version. A multi-slot encoder floors over its
/// slots instead ([`orrery_persistd::ComponentBag::schema_floor`]).
#[must_use]
pub fn bag_schema_floor(
    scenario: &ResolvedScenario,
    emit: &ResolvedEmit,
    archetype: &str,
) -> SchemaVersion {
    let fields = scenario
        .archetypes
        .get(archetype)
        .unwrap_or_else(|| panic!("emit {:?} archetype {archetype:?} validated", emit.name));
    SchemaVersion::from(fields.schema_version)
}

fn hex32(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;

    const DEMO: &str = include_str!("../scenarios/p2demo.toml");

    #[test]
    fn plan_is_exact_and_analytic() {
        let sc = Scenario::parse(DEMO).expect("parses");
        let resolved = sc.resolve(b"p2demo-2026-08-13".to_vec()).expect("resolves");
        let report = plan(&resolved, "p2demo-2026-08-13");
        assert_eq!(report.total_entities, 10_000);
        assert!(report.occupied_cells >= 100);
        // Systematic allocation (A.2.2) deals 10 000 entities to 10 000
        // distinct cells (λ = 0.305 < 1 rounds to 0-or-1 per cell), so the
        // realized occupancy is N/C = 0.3052 — inside the 0.30 ± 0.05
        // target. The ladder's 8 618 is the *Poisson* occupancy
        // expectation (A.3.3); the deterministic splitter's systematic
        // occupancy differs from it by construction, and the scenario's
        // tolerance exists to absorb exactly this (§7.2).
        assert!(
            (report.occupied_fraction - 0.30).abs() <= 0.05,
            "occupied fraction {} within tolerance of 0.30",
            report.occupied_fraction
        );
    }
}
