//! Bulk world writes for `orrery-seed`.
//!
//! The apply path is offline by default: it checks fences, preserves or mints
//! `PersistId`s through the `seedmap/` subspace, and writes the resulting
//! rows in blind batches.

use std::collections::BTreeMap;
#[cfg(feature = "fdb")]
use std::collections::BTreeSet;
#[cfg(feature = "fdb")]
use std::sync::Arc;

#[cfg(feature = "fdb")]
use orrery_persistd::keyspace;
#[cfg(feature = "fdb")]
use orrery_protocol::CellId;
#[cfg(feature = "fdb")]
use orrery_protocol::{GridId, PersistId};

#[cfg(feature = "fdb")]
use crate::content::ContentKey;
#[cfg(feature = "fdb")]
use crate::idmap::{self, BlockGrantCursor, SeedMap, SeedMapRow};
use crate::manifest::ManifestEntry;
#[cfg(feature = "fdb")]
use crate::manifest::ManifestWriter;
use crate::scenario::ResolvedScenario;
#[cfg(feature = "fdb")]
use crate::seedtree::SeedRoot;
#[cfg(feature = "fdb")]
use crate::split::{split_cell, FieldOracle};
#[cfg(feature = "fdb")]
use crate::write::{self, EncodedRow};

/// Writer options.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApplyOptions {
    /// Allow the opaque encoder to stand in for a `ruleset` payload class.
    pub allow_opaque: bool,
    /// Flatten nested grids into grid 0.
    pub single_grid: bool,
}

/// A content-version row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentVersion {
    /// The content build id.
    pub content_build: String,
    /// The manifest digest.
    pub manifest_digest: String,
    /// The scenario seed used to derive the world.
    pub scenario_seed: String,
    /// A digest of the resolved scenario config.
    pub config_digest: String,
    /// The rustc version.
    pub toolchain: String,
    /// Wall-clock time the seeder wrote the world.
    pub seeded_at_ms: u64,
}

/// A row planned for writing.
#[derive(Debug, Clone)]
pub struct DesiredRow {
    /// The row key.
    pub key: Vec<u8>,
    /// The row value.
    pub value: Vec<u8>,
    /// The manifest entry for world rows.
    pub manifest: Option<ManifestEntry>,
}

/// The result of an `apply`.
#[derive(Debug, Clone)]
pub struct ApplyReport {
    /// The dry-run plan.
    pub plan: crate::plan::PlanReport,
    /// Total rows written.
    pub written_rows: u64,
    /// Rows whose value changed from what was already in FDB.
    pub changed_rows: u64,
    /// Number of transactions committed.
    pub batches: u64,
    /// Commit p99 in milliseconds.
    pub commit_p99_ms: f64,
    /// The content-version row that was written.
    pub content_version: ContentVersion,
}

/// A closed-form oracle over one layer.
#[cfg(feature = "fdb")]
struct UniformOracle<'a> {
    layer: &'a crate::scenario::ResolvedLayer,
}

#[cfg(feature = "fdb")]
impl FieldOracle for UniformOracle<'_> {
    fn field_mass(&self, cell: CellId) -> crate::field::Q16_16 {
        let quanta = self
            .layer
            .bounds
            .field_mass_under(cell, self.layer.intensity);
        crate::field::Q16_16(
            i32::try_from(quanta.min(i128::from(i32::MAX) as u64)).unwrap_or(i32::MAX),
        )
    }
}

/// Apply a scenario by writing its rows into FDB.
#[cfg(feature = "fdb")]
pub async fn run(
    source: &str,
    mut scenario: ResolvedScenario,
    options: ApplyOptions,
) -> Result<ApplyReport, String> {
    if options.single_grid {
        flatten_to_single_grid(&mut scenario);
    }
    if scenario.raw.payload.class.as_deref() == Some("ruleset") && !options.allow_opaque {
        return Err(
            "apply refuses `[payload] class = \"ruleset\"` without `--allow-opaque`".to_string(),
        );
    }

    let seed_display = String::from_utf8_lossy(&scenario.seed_material).to_string();
    let root = SeedRoot::derive(&scenario.seed_context, &scenario.seed_material);
    let plan_report = crate::plan::plan(&scenario, &seed_display);
    let config_digest = blake3::hash(source.as_bytes()).to_hex().to_string();
    let content_build = scenario
        .raw
        .scenario
        .content_build
        .clone()
        .unwrap_or_else(|| scenario.raw.scenario.name.clone());

    let db = crate::fdb_open(&crate::cluster_file_from_env()?)?;

    let existing_seedmap = idmap::read_seedmap(&db).await?;
    let desired = build_desired_rows(
        &db,
        &scenario,
        &root,
        &existing_seedmap,
        &content_build,
        &seed_display,
        &config_digest,
    )
    .await?;
    let existing = load_existing_rows(&db, &scenario, &desired).await?;
    let changed_rows = desired
        .iter()
        .filter(|row| existing.get(&row.key) != Some(&row.value))
        .count() as u64;

    let mut batches = Vec::new();
    let mut encoded = Vec::with_capacity(desired.len());
    for row in &desired {
        encoded.push(EncodedRow {
            key: row.key.clone(),
            value: row.value.clone(),
        });
    }
    batches.extend(write::pack_batches(encoded));
    let stats = write::commit_batches(Arc::clone(&db), batches).await?;

    let content_version = desired
        .iter()
        .find(|row| row.key == keyspace::content_version_key().to_vec())
        .map(|row| postcard::from_bytes::<ContentVersion>(&row.value))
        .transpose()
        .map_err(|e| format!("decode content/version: {e}"))?
        .unwrap_or_else(|| ContentVersion {
            content_build,
            manifest_digest: plan_report.manifest_digest.clone(),
            scenario_seed: seed_display,
            config_digest,
            toolchain: plan_report.toolchain.clone(),
            seeded_at_ms: now_ms(),
        });

    Ok(ApplyReport {
        plan: plan_report,
        written_rows: stats.written_rows,
        changed_rows,
        batches: stats.batches,
        commit_p99_ms: stats.commit_p99_ms(),
        content_version,
    })
}

/// Fallback when the `fdb` feature is off.
#[cfg(not(feature = "fdb"))]
pub async fn run(
    _source: &str,
    _scenario: ResolvedScenario,
    _options: ApplyOptions,
) -> Result<ApplyReport, String> {
    Err("apply requires the `fdb` feature".to_string())
}

/// Build the desired row set from a resolved scenario and an existing
/// `seedmap/` snapshot.
#[cfg(feature = "fdb")]
pub async fn build_desired_rows(
    db: &foundationdb::Database,
    scenario: &ResolvedScenario,
    root: &SeedRoot,
    existing_seedmap: &SeedMap,
    content_build: &str,
    seed_display: &str,
    config_digest: &str,
) -> Result<Vec<DesiredRow>, String> {
    let mut out = Vec::new();
    let mut manifest = ManifestWriter::new();
    let mut grants: BTreeMap<GridId, BlockGrantCursor> = BTreeMap::new();
    for emit in &scenario.emits {
        let layer = scenario
            .layers
            .iter()
            .find(|l| l.into == emit.from)
            .ok_or_else(|| format!("emit {:?} has no matching layer", emit.name))?;
        let oracle = UniformOracle { layer };
        let mut cell_counts = BTreeMap::new();
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
        let mut descriptors = Vec::new();
        for (&cell, &count) in &cell_counts {
            let cell_key = SeedRoot::cell_key(&layer_key, cell);
            let assignments = crate::place::apportion_archetypes(count, &mix, cell_key);
            for (index, &arch_idx) in assignments.iter().enumerate() {
                let archetype = emit.archetypes[arch_idx as usize].0.clone();
                let slot_key = SeedRoot::slot_key(&cell_key, index as u64);
                let content_key = ContentKey::derive(&crate::content::ContentKeyPreimage {
                    scenario: &scenario.raw.scenario.name,
                    emit: &emit.name,
                    layer: &layer.name,
                    grid: emit.grid,
                    cell,
                    index: index as u64,
                    archetype: &archetype,
                });
                let local_pos = crate::place::hash_local_pos(slot_key, cell_edge as f32);
                descriptors.push((
                    cell,
                    index as u64,
                    archetype,
                    slot_key,
                    content_key,
                    local_pos,
                ));
            }
        }

        // §9.3's canonical order is `(grid, cell, ContentKey)` ascending. Cells
        // already arrive ascending from the splitter, but within a cell the
        // descriptors are in *slot index* order and `ContentKey` is a blake3
        // hash of the derivation path — so index order is not key order. Sort
        // each cell's slice before emitting. This is not the global sort §9.3
        // rules out: it is bounded by one cell's population.
        descriptors.sort_by_key(|d| (d.0, d.4));

        for (cell, _index, archetype, slot_key, content_key, local_pos) in descriptors {
            let seed_row = existing_seedmap.get(&content_key).cloned();
            let persist_id = if let Some(existing) = seed_row.as_ref() {
                existing.persist_id
            } else {
                next_persist_id(db, emit.grid, &mut grants).await?
            };
            let bag = crate::plan::encode_bag(
                scenario,
                emit,
                &archetype,
                cell,
                local_pos,
                slot_key,
                content_key,
                persist_id,
            );
            let manifest_entry = ManifestEntry {
                content_key,
                persist_id,
                grid: emit.grid,
                cell,
                value_digest: crate::manifest::value_digest(&bag),
                byte_len: bag.len() as u32,
                archetype: archetype.clone(),
                layer: layer.name.clone(),
                emit: emit.name.clone(),
            };
            manifest.push(manifest_entry.clone());
            let world_key = keyspace::world_key(emit.grid, cell, persist_id);
            out.push(DesiredRow {
                key: world_key.to_vec(),
                value: keyspace::encode_versioned_live_value(
                    crate::plan::bag_schema_floor(scenario, emit, &archetype),
                    &bag,
                ),
                manifest: Some(manifest_entry),
            });
            let seedmap_value = if let Some(existing) = seed_row {
                idmap::encode_seedmap_value(&existing)
            } else {
                idmap::encode_seedmap_value(&SeedMapRow {
                    persist_id,
                    grid: emit.grid,
                    cell,
                    first_seen_build: content_build.to_string(),
                })
            };
            out.push(DesiredRow {
                key: idmap::seedmap_key(&content_key).to_vec(),
                value: seedmap_value,
                manifest: None,
            });
            out.push(DesiredRow {
                key: idmap::seedprog_key(&emit.name, emit.grid, cell).to_vec(),
                value: Vec::new(),
                manifest: None,
            });
        }
    }

    let content_version = ContentVersion {
        content_build: content_build.to_string(),
        manifest_digest: manifest
            .finish(&crate::manifest::ToolchainStamp::current())
            .into_iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        scenario_seed: seed_display.to_string(),
        config_digest: config_digest.to_string(),
        toolchain: crate::manifest::ToolchainStamp::current().rustc,
        seeded_at_ms: now_ms(),
    };

    out.push(DesiredRow {
        key: keyspace::content_version_key().to_vec(),
        value: postcard::to_stdvec(&content_version).map_err(|e| e.to_string())?,
        manifest: None,
    });
    Ok(out)
}

#[cfg(feature = "fdb")]
async fn next_persist_id(
    db: &foundationdb::Database,
    grid: GridId,
    grants: &mut BTreeMap<GridId, BlockGrantCursor>,
) -> Result<PersistId, String> {
    if let Some(cursor) = grants.get_mut(&grid) {
        if let Some(id) = cursor.next_id() {
            return Ok(id);
        }
    }
    let grant = idmap::reserve_block(db, grid, idmap::DEFAULT_BLOCK_GRANT).await?;
    let cursor = grants
        .entry(grid)
        .and_modify(|cursor| *cursor = BlockGrantCursor::new(grant))
        .or_insert_with(|| BlockGrantCursor::new(grant));
    cursor
        .next_id()
        .ok_or_else(|| "pid grant exhausted".to_string())
}

/// Load the existing values for the keys we are about to write.
#[cfg(feature = "fdb")]
pub async fn load_existing_rows(
    db: &foundationdb::Database,
    _scenario: &ResolvedScenario,
    desired: &[DesiredRow],
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    use foundationdb::{KeySelector, RangeOption};
    use futures::stream::TryStreamExt;

    let mut out = BTreeMap::new();
    let unique_shards: BTreeSet<(GridId, CellId)> = desired
        .iter()
        .filter_map(|row| {
            row.manifest.as_ref().and_then(|m| {
                keyspace::decode_world_key(&row.key).map(|(_, cell, _)| (m.grid, cell))
            })
        })
        .collect();
    for (grid, shard) in unique_shards {
        let start = keyspace::world_range_start(grid, shard);
        let end = keyspace::world_range_end(grid, shard);
        let opt = RangeOption {
            begin: KeySelector::first_greater_or_equal(start.as_slice()),
            end: KeySelector::first_greater_or_equal(end.as_slice()),
            ..RangeOption::default()
        };
        let mut stream = db
            .run(|trx, _| {
                let opt = opt.clone();
                async move {
                    let mut rows = Vec::new();
                    let mut stream = trx.get_ranges_keyvalues(opt, false);
                    while let Some(kv) = stream.try_next().await? {
                        rows.push((kv.key().to_vec(), kv.value().to_vec()));
                    }
                    Ok::<_, foundationdb::FdbBindingError>(rows)
                }
            })
            .await
            .map_err(|e| format!("existing world scan: {e}"))?;
        for (key, value) in stream.drain(..) {
            out.insert(key, value);
        }
    }

    // Whole-family scans for the small auxiliary families.
    for (start, end) in [
        (
            keyspace::seedmap_range_start(),
            keyspace::seedmap_range_end(),
        ),
        (vec![b'p'], vec![b'q']),
    ] {
        let opt = foundationdb::RangeOption {
            begin: foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
            end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
            ..foundationdb::RangeOption::default()
        };
        let rows = db
            .run(|trx, _| {
                let opt = opt.clone();
                async move {
                    let mut rows = Vec::new();
                    let mut stream = trx.get_ranges_keyvalues(opt, false);
                    while let Some(kv) = stream.try_next().await? {
                        rows.push((kv.key().to_vec(), kv.value().to_vec()));
                    }
                    Ok::<_, foundationdb::FdbBindingError>(rows)
                }
            })
            .await
            .map_err(|e| format!("aux scan: {e}"))?;
        for (key, value) in rows {
            out.insert(key, value);
        }
    }

    if let Some(v) = db
        .run(|trx, _| async move {
            trx.get(&keyspace::content_version_key(), false)
                .await
                .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))
        })
        .await
        .map_err(|e| format!("content/version scan: {e}"))?
    {
        out.insert(keyspace::content_version_key().to_vec(), v.to_vec());
    }

    Ok(out)
}

/// Stand-in for the FoundationDB read path when the `fdb` feature is off:
/// there is no cluster to read, so every desired row is treated as absent.
#[cfg(not(feature = "fdb"))]
pub async fn load_existing_rows(
    _db: &(),
    _scenario: &ResolvedScenario,
    _desired: &[DesiredRow],
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    Ok(BTreeMap::new())
}

/// Flatten nested grids into grid 0.
#[cfg(feature = "fdb")]
fn flatten_to_single_grid(scenario: &mut ResolvedScenario) {
    let root = scenario
        .grids
        .get(&0)
        .copied()
        .unwrap_or(crate::scenario::ResolvedGrid {
            id: GridId::ROOT,
            cell_edge_m: orrery_protocol::DEFAULT_CELL_EDGE_M,
        });
    scenario.grids.clear();
    scenario.grids.insert(0, root);
    for layer in &mut scenario.layers {
        layer.grid = GridId::ROOT;
        layer.cell_edge_m = root.cell_edge_m;
    }
    for emit in &mut scenario.emits {
        emit.grid = GridId::ROOT;
    }
}

#[cfg(feature = "fdb")]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
