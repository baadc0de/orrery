//! Read-back verification for seeded worlds.

use std::path::PathBuf;

use crate::apply;
use crate::scenario::ResolvedScenario;

/// Verification options.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    /// Check every row rather than sampling.
    pub full: bool,
    /// Emit the manifest to this path.
    pub emit_manifest: Option<PathBuf>,
    /// Flatten nested grids into grid 0.
    pub single_grid: bool,
}

/// Verification summary.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Rows checked.
    pub checked_rows: u64,
    /// Output manifest path, if any.
    pub emit_manifest: Option<PathBuf>,
}

/// Rebuild the seeded world from the scenario and compare it to the live
/// FDB rows, optionally emitting the manifest snapshot.
#[cfg(feature = "fdb")]
pub async fn run(
    source: &str,
    mut scenario: ResolvedScenario,
    options: VerifyOptions,
) -> Result<VerifyReport, String> {
    if options.single_grid {
        // Use the same flattening as apply to keep the read side aligned.
        scenario = flatten(scenario);
    }
    let seed_display = String::from_utf8_lossy(&scenario.seed_material).to_string();
    let root = crate::seedtree::SeedRoot::derive(&scenario.seed_context, &scenario.seed_material);
    let content_build = scenario
        .raw
        .scenario
        .content_build
        .clone()
        .unwrap_or_else(|| scenario.raw.scenario.name.clone());

    let cluster_file = std::env::var("ORRERY_FDB_CLUSTER_FILE")
        .map_err(|_| "set ORRERY_FDB_CLUSTER_FILE to the FDB cluster file".to_string())?;
    let db = std::sync::Arc::new({
        crate::fdb_network();
        foundationdb::Database::from_path(&cluster_file).map_err(|e| format!("connect: {e}"))?
    });
    let existing_seedmap = crate::idmap::read_seedmap(&db).await?;
    let config_digest = blake3::hash(source.as_bytes()).to_hex().to_string();
    let desired = apply::build_desired_rows(
        &db,
        &scenario,
        &root,
        &existing_seedmap,
        &content_build,
        &seed_display,
        &config_digest,
    )
    .await?;
    let existing = apply::load_existing_rows(&db, &scenario, &desired).await?;
    let checked_rows = desired
        .iter()
        .filter(|row| existing.get(&row.key) == Some(&row.value))
        .count() as u64;

    if let Some(path) = &options.emit_manifest {
        let manifest: Vec<_> = desired
            .iter()
            .filter_map(|row| row.manifest.clone())
            .collect();
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("write manifest: {e}"))?;
    }

    let _ = source;
    let _ = options.full;
    Ok(VerifyReport {
        checked_rows,
        emit_manifest: options.emit_manifest,
    })
}

#[cfg(not(feature = "fdb"))]
pub async fn run(
    _source: &str,
    _scenario: ResolvedScenario,
    options: VerifyOptions,
) -> Result<VerifyReport, String> {
    let _ = options;
    Err("verify requires the `fdb` feature".to_string())
}

fn flatten(mut scenario: ResolvedScenario) -> ResolvedScenario {
    let root = scenario
        .grids
        .get(&0)
        .copied()
        .unwrap_or(crate::scenario::ResolvedGrid {
            id: orrery_protocol::GridId::ROOT,
            cell_edge_m: orrery_protocol::DEFAULT_CELL_EDGE_M,
        });
    scenario.grids.clear();
    scenario.grids.insert(0, root);
    for layer in &mut scenario.layers {
        layer.grid = orrery_protocol::GridId::ROOT;
        layer.cell_edge_m = root.cell_edge_m;
    }
    for emit in &mut scenario.emits {
        emit.grid = orrery_protocol::GridId::ROOT;
    }
    scenario
}
