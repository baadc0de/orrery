//! Seeded-world wipe support.

use std::collections::BTreeSet;

use futures::TryStreamExt;
use orrery_persistd::keyspace;
use orrery_protocol::{CellId, GridId};

use crate::scenario::ResolvedScenario;

/// Wipe options.
#[derive(Debug, Clone)]
pub struct WipeOptions {
    /// Operator confirmation.
    pub yes: bool,
    /// The content_build string typed back.
    pub typed_content_build: String,
    /// Flatten nested grids into grid 0.
    pub single_grid: bool,
}

#[cfg(feature = "fdb")]
/// Wipe the seeded-world families from FDB after confirming the operator's
/// typed input and checking that no live fence row overlaps the target range.
pub async fn run(
    _source: &str,
    mut scenario: ResolvedScenario,
    options: WipeOptions,
) -> Result<(), String> {
    if !options.yes {
        return Err("wipe requires --yes".to_string());
    }
    let expected = scenario
        .raw
        .scenario
        .content_build
        .clone()
        .unwrap_or_else(|| scenario.raw.scenario.name.clone());
    if expected != options.typed_content_build {
        return Err(format!(
            "wipe refused: typed content_build {:?} does not match {:?}",
            options.typed_content_build, expected
        ));
    }
    if options.single_grid {
        scenario = flatten(scenario);
    }
    let cluster_file = std::env::var("ORRERY_FDB_CLUSTER_FILE")
        .map_err(|_| "set ORRERY_FDB_CLUSTER_FILE to the FDB cluster file".to_string())?;
    let db = std::sync::Arc::new({
        crate::fdb_network();
        foundationdb::Database::from_path(&cluster_file).map_err(|e| format!("connect: {e}"))?
    });

    // The grids this scenario actually realizes into — not every declared
    // `[[grid]]`. A scenario may declare a grid it never emits to (the demo
    // scenarios declare grid 0 implicitly while emitting into an isolation
    // grid), and guarding — let alone clearing — a grid we never wrote is
    // wrong twice over: it blocks on unrelated fence rows, and `wipe` would
    // clear another world's rows.
    let grid_ids: BTreeSet<GridId> = scenario.emits.iter().map(|e| e.grid).collect();
    for grid in &grid_ids {
        let live = db
            .run(|trx, _| {
                let begin = keyspace::fence_grid_range_start(*grid);
                let end = keyspace::fence_grid_range_end(*grid);
                async move {
                    let opt = foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(begin.as_slice()),
                        end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                        ..foundationdb::RangeOption::default()
                    };
                    let mut stream = trx.get_ranges_keyvalues(opt, false);
                    let mut found = Vec::new();
                    while let Some(kv) = stream
                        .try_next()
                        .await
                        .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))?
                    {
                        found.push(kv.key().to_vec());
                    }
                    Ok::<_, foundationdb::FdbBindingError>(found)
                }
            })
            .await
            .map_err(|e| format!("scan fence rows: {e}"))?;
        if !live.is_empty() {
            return Err(format!(
                "wipe refused: live fence rows are still present ({} rows)",
                live.len()
            ));
        }
        let world_start = keyspace::world_range_start(*grid, CellId::ROOT);
        let world_end = keyspace::world_range_end(*grid, CellId::ROOT);
        let chunk_start = keyspace::chunk_range_start(*grid, CellId::ROOT);
        let chunk_end = keyspace::chunk_range_end(*grid, CellId::ROOT);
        db.run(|trx, _| {
            let world_start = world_start.clone();
            let world_end = world_end.clone();
            let chunk_start = chunk_start.clone();
            let chunk_end = chunk_end.clone();
            async move {
                trx.clear_range(&world_start, &world_end);
                trx.clear_range(&chunk_start, &chunk_end);
                trx.clear_range(
                    &keyspace::seedmap_range_start(),
                    &keyspace::seedmap_range_end(),
                );
                trx.clear_range(
                    &keyspace::seedprog_range_start(),
                    &keyspace::seedprog_range_end(),
                );
                trx.clear(&keyspace::content_version_key());
                Ok::<_, foundationdb::FdbBindingError>(())
            }
        })
        .await
        .map_err(|e| format!("wipe txn: {e}"))?;
    }
    Ok(())
}

#[cfg(not(feature = "fdb"))]
pub async fn run(
    _source: &str,
    _scenario: ResolvedScenario,
    _options: WipeOptions,
) -> Result<(), String> {
    Err("wipe requires the `fdb` feature".to_string())
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
