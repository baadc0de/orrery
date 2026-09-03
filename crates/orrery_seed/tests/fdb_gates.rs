//! FDB-gated acceptance tests for the seeder write path.
//!
//! These tests self-skip when no cluster file is discoverable, so a sandboxed
//! run stays green while the orchestrator can exercise them against the live
//! dev cluster.

#[cfg(feature = "fdb")]
use std::process::Command;
#[cfg(feature = "fdb")]
use std::sync::Arc;
#[cfg(feature = "fdb")]
use std::time::Instant;

#[cfg(feature = "fdb")]
use foundationdb::{Database, KeySelector, RangeOption};
#[cfg(feature = "fdb")]
use futures::TryStreamExt;
#[cfg(feature = "fdb")]
use orrery_persistd::checkpoint::{
    CheckpointStore, ColdCellReader, FdbCheckpointStore, MemCheckpointStore,
};
#[cfg(feature = "fdb")]
use orrery_persistd::cluster::{ColdFallbackRouter, Router};
#[cfg(feature = "fdb")]
use orrery_persistd::keyspace;
#[cfg(feature = "fdb")]
use orrery_persistd::{payload_crc, CellRuntime, JournalConfig, MemFenceStore, RuntimeConfig};
#[cfg(feature = "fdb")]
use orrery_persistd::{FenceOutcome, FenceRow, FenceStatus, FenceStore};
#[cfg(feature = "fdb")]
use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

/// The one cluster-file discovery rule, shared with persistd's FDB tier.
#[cfg(feature = "fdb")]
fn fdb_cluster_file() -> Option<String> {
    orrery_persistd::fdb::discover_cluster_file()
}

/// Open a raw handle for the assertions that read rows back directly.
///
/// Goes through the seeder's bounded opener rather than
/// `Database::from_path`, so a wedged cluster fails these tests instead of
/// hanging the suite.
#[cfg(feature = "fdb")]
fn open_db(cluster: &str) -> Arc<Database> {
    orrery_seed::fdb_open(cluster).expect("open bounded FDB handle")
}

#[cfg(feature = "fdb")]
fn seed_bin() -> String {
    std::env::var("CARGO_BIN_EXE_orrery-seed")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_orrery_seed"))
        .expect("orrery-seed binary path")
}

#[cfg(feature = "fdb")]
fn skip_if_no_fdb() -> Option<String> {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return None;
    };
    Some(cluster)
}

#[cfg(feature = "fdb")]
async fn activate_fdb_checkpoint_fence(cluster: &str, grid: GridId) {
    let store = orrery_persistd::fence::FdbFenceStore::connect(cluster).expect("fence store");
    let expected = FenceRow {
        owner: 0,
        epoch: Epoch::new(0),
        status: FenceStatus::Active,
    };
    match store.read(grid, CellId::ROOT).await.expect("read fence") {
        Some(row) => assert_eq!(row, expected, "test grid has unexpected fence"),
        None => assert!(matches!(
            store
                .fence(grid, CellId::ROOT, None, &expected)
                .await
                .expect("activate fence"),
            FenceOutcome::Fenced
        )),
    }
}

#[cfg(feature = "fdb")]
async fn retire_fdb_checkpoint_fence(cluster: &str, grid: GridId) {
    orrery_persistd::fence::FdbFenceStore::connect(cluster)
        .expect("fence store")
        .retire(grid, CellId::ROOT)
        .await
        .expect("retire test fence");
}

#[cfg(feature = "fdb")]
async fn run_seed(args: &[&str], scenario: &std::path::Path) -> std::process::Output {
    Command::new(seed_bin())
        .args(args)
        .arg(scenario)
        .output()
        .expect("run seed binary")
}

/// Run the production `wipe` verb.
///
/// Only for the tests whose subject *is* the wipe: the verb clears the
/// cluster-global `content/version` row alongside the grid it is aimed at,
/// which is right for an operator and wrong for a test fixture (#999 — see
/// [`reset_grid`]).
#[cfg(feature = "fdb")]
async fn wipe_scenario(scenario: &std::path::Path, content_build: &str) {
    let output = run_seed(
        &["wipe", "--yes", "--content-build", content_build],
        scenario,
    )
    .await;
    maybe_assert_success(&output, "wipe");
}

/// Clear exactly the seeded rows a test owns for one grid — the per-test
/// fixture reset (#999).
///
/// Every reset that is not itself under test clears only what the test
/// claims: its grid's world span and the grid-scoped rows of the two
/// global id families — the same per-grid scope the `wipe` verb clears,
/// minus the global `content/version` row no test may clear except by
/// testing the wipe, and minus the checkpoint rows the wipe deliberately
/// leaves. Before this, the suite's fixture resets were `wipe` calls, and
/// every one of them cleared the global row a sibling gate was about to
/// read.
#[cfg(feature = "fdb")]
async fn reset_grid(db: &Database, grid: GridId) {
    let world_start = keyspace::world_range_start(grid, CellId::ROOT);
    let world_end = keyspace::world_range_end(grid, CellId::ROOT);
    let seedmap_start = keyspace::seedmap_range_start();
    let seedmap_end = keyspace::seedmap_range_end();
    let seedprog_start = keyspace::seedprog_range_start();
    let seedprog_end = keyspace::seedprog_range_end();
    db.run(|trx, _| {
        let world_start = world_start.clone();
        let world_end = world_end.clone();
        let seedmap_start = seedmap_start.clone();
        let seedmap_end = seedmap_end.clone();
        let seedprog_start = seedprog_start.clone();
        let seedprog_end = seedprog_end.clone();
        async move {
            trx.clear_range(&world_start, &world_end);
            let seedmap_opt = RangeOption {
                begin: KeySelector::first_greater_or_equal(seedmap_start.as_slice()),
                end: KeySelector::first_greater_or_equal(seedmap_end.as_slice()),
                ..RangeOption::default()
            };
            let mut seedmap_rows = trx.get_ranges_keyvalues(seedmap_opt, false);
            while let Some(row) = seedmap_rows.try_next().await? {
                if orrery_seed::idmap::decode_seedmap_value(row.value())
                    .is_ok_and(|seed_row| seed_row.grid == grid)
                {
                    trx.clear(row.key());
                }
            }
            let seedprog_opt = RangeOption {
                begin: KeySelector::first_greater_or_equal(seedprog_start.as_slice()),
                end: KeySelector::first_greater_or_equal(seedprog_end.as_slice()),
                ..RangeOption::default()
            };
            let mut seedprog_rows = trx.get_ranges_keyvalues(seedprog_opt, false);
            while let Some(row) = seedprog_rows.try_next().await? {
                if keyspace::decode_seedprog_key(row.key())
                    .is_some_and(|(_, row_grid, _)| row_grid == grid)
                {
                    trx.clear(row.key());
                }
            }
            Ok::<_, foundationdb::FdbBindingError>(())
        }
    })
    .await
    .expect("reset the test grid's rows");
}

#[cfg(feature = "fdb")]
async fn scan_world_rows(db: &Database, grid: GridId) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    let start = keyspace::world_range_start(grid, CellId::ROOT);
    let end = keyspace::world_range_end(grid, CellId::ROOT);
    db.run(|trx, _| {
        let start = start.clone();
        let end = end.clone();
        async move {
            let opt = RangeOption {
                begin: KeySelector::first_greater_or_equal(start.as_slice()),
                end: KeySelector::first_greater_or_equal(end.as_slice()),
                ..RangeOption::default()
            };
            let mut out = Vec::new();
            let mut stream = trx.get_ranges_keyvalues(opt, false);
            while let Some(kv) = stream.try_next().await? {
                out.push((kv.key().to_vec(), kv.value().to_vec()));
            }
            Ok::<_, foundationdb::FdbBindingError>(out)
        }
    })
    .await
    .map_err(|e| format!("scan world: {e}"))
}

#[cfg(feature = "fdb")]
fn runtime_config(dir: &std::path::Path, grid: GridId) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: orrery_persistd::journal::GroupCommitConfig {
                mode: orrery_persistd::journal::AdaptiveCommitMode::AlwaysBatch,
                batch_window: std::time::Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

#[cfg(feature = "fdb")]
fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    orrery_protocol::NodeId::from_bytes(&seed).expect("valid node id")
}

#[cfg(feature = "fdb")]
fn record(grid: GridId, cell: CellId, entity: u64) -> JournalRecord {
    let payload = entity.to_le_bytes();
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::Spawn,
        payload: bytes::Bytes::copy_from_slice(&payload),
        crc: payload_crc(&payload),
    }
}

#[cfg(feature = "fdb")]
fn cell(x: i32, y: i32, z: i32) -> CellId {
    CellId::from_coords(glam::IVec3::new(x, y, z), CellId::MAX_LEVEL).unwrap()
}

#[cfg(feature = "fdb")]
fn maybe_assert_success(output: &std::process::Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed: stdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "fdb")]
fn with_grid(source: &str, grid_id: GridId, smoke: bool) -> String {
    if smoke {
        let injected = format!(
            "{source}\n\n[[grid]]\nid = {}\ncell_edge_m = 128.0\n",
            grid_id.0
        );
        return injected;
    }
    source.replace("id          = 0", &format!("id          = {}", grid_id.0))
}

#[cfg(feature = "fdb")]
fn write_temp_scenario(name: &str, source: &str) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("temp scenario");
    std::fs::write(file.path(), source).expect(name);
    file
}

#[cfg(feature = "fdb")]
async fn read_content_version(db: &Database) -> Result<orrery_seed::apply::ContentVersion, String> {
    let bytes = db
        .run(|trx, _| async move {
            trx.get(&keyspace::content_version_key(), false)
                .await
                .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))
        })
        .await
        .map_err(|e| format!("content/version read: {e}"))?
        .ok_or_else(|| "missing content/version row".to_string())?;
    orrery_persistd::content_version::decode(&bytes)
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn wipe_clears_seeded_v1_world_rows_after_terrain_removal() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let grid = GridId::new(9421);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("smoke.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("terrainless wipe", &with_grid(&source, grid, true));
    let db = open_db(&cluster);
    reset_grid(&db, grid).await;

    let applied = run_seed(&["apply", "--allow-opaque"], temp.path()).await;
    maybe_assert_success(&applied, "apply before terrainless wipe");
    assert!(
        !scan_world_rows(&db, grid)
            .await
            .expect("scan world before terrainless wipe")
            .is_empty(),
        "the scenario seeded world rows for the wipe to clear"
    );

    wipe_scenario(temp.path(), "smoke-2026-08-13").await;
    assert!(
        scan_world_rows(&db, grid)
            .await
            .expect("scan world after terrainless wipe")
            .is_empty(),
        "terrainless v1 wipe clears every seeded world row"
    );
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_reseed_preserves_persist_ids() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9402), false));
    let db = open_db(&cluster);
    reset_grid(&db, GridId::new(9402)).await;

    let first = run_seed(
        &["apply", "--profile", "demo", "--allow-opaque"],
        temp.path(),
    )
    .await;
    maybe_assert_success(&first, "first apply");
    let rows1 = scan_world_rows(&db, GridId::new(9402))
        .await
        .expect("scan1");
    let ids1: Vec<_> = rows1
        .iter()
        .filter_map(|(k, _)| keyspace::decode_world_key(k).map(|(_, _, pid)| pid))
        .collect();

    let second = run_seed(
        &["apply", "--profile", "demo", "--allow-opaque"],
        temp.path(),
    )
    .await;
    maybe_assert_success(&second, "second apply");
    let rows2 = scan_world_rows(&db, GridId::new(9402))
        .await
        .expect("scan2");
    let ids2: Vec<_> = rows2
        .iter()
        .filter_map(|(k, _)| keyspace::decode_world_key(k).map(|(_, _, pid)| pid))
        .collect();

    assert_eq!(
        ids1, ids2,
        "content keys keep the same PersistId across reseed"
    );
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn wipe_in_another_grid_preserves_seedmap_ids() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let retained_grid = GridId::new(9410);
    let wiped_grid = GridId::new(9411);
    let retained = write_temp_scenario(
        "retained scenario",
        &with_grid(&source, retained_grid, false),
    );
    let wiped = write_temp_scenario("wiped scenario", &with_grid(&source, wiped_grid, false));
    let db = open_db(&cluster);
    reset_grid(&db, retained_grid).await;
    reset_grid(&db, wiped_grid).await;

    let first = run_seed(
        &["apply", "--profile", "demo", "--allow-opaque"],
        retained.path(),
    )
    .await;
    maybe_assert_success(&first, "retained apply");

    let before: Vec<_> = scan_world_rows(&db, retained_grid)
        .await
        .expect("scan retained before wipe")
        .iter()
        .filter_map(|(key, _)| keyspace::decode_world_key(key).map(|(_, _, pid)| pid))
        .collect();

    wipe_scenario(wiped.path(), "demo-2026-08-13").await;
    let second = run_seed(
        &["apply", "--profile", "demo", "--allow-opaque"],
        retained.path(),
    )
    .await;
    maybe_assert_success(&second, "retained reseed");
    let after: Vec<_> = scan_world_rows(&db, retained_grid)
        .await
        .expect("scan retained after wipe")
        .iter()
        .filter_map(|(key, _)| keyspace::decode_world_key(key).map(|(_, _, pid)| pid))
        .collect();

    assert_eq!(before, after, "a different grid's wipe keeps stable ids");
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn block_grants_begin_after_the_fdb_allocator() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let grid = GridId::new(9412);
    let db = open_db(&cluster);
    db.run(|trx, _| async move {
        let key = keyspace::pid_next_key(grid);
        trx.set(&key, &10_000u64.to_le_bytes());
        Ok::<_, foundationdb::FdbBindingError>(())
    })
    .await
    .expect("seed allocator counter");

    let first = orrery_seed::idmap::reserve_block(&db, grid, 2)
        .await
        .expect("first block");
    let second = orrery_seed::idmap::reserve_block(&db, grid, 2)
        .await
        .expect("second block");

    assert_eq!(first.start, PersistId::new(10_001));
    assert_eq!(second.start, PersistId::new(10_003));
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn every_written_value_carries_the_live_tag() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("smoke.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9401), true));
    let db = open_db(&cluster);
    reset_grid(&db, GridId::new(9401)).await;

    let output = run_seed(&["apply", "--allow-opaque"], temp.path()).await;
    maybe_assert_success(&output, "smoke apply");

    let rows = scan_world_rows(&db, GridId::new(9401))
        .await
        .expect("scan world");
    assert_eq!(rows.len(), 1_000, "smoke writes exactly 1000 world rows");
    // Every seeded row is live *and* self-describing (D38 clause (d)(2)): the
    // versioned envelope, carrying the archetype's declared schema version as
    // the bag's floor. Asserting the tag alone would still pass on a writer
    // that dropped the floor, so the floor is read back too — without decoding
    // the bag, which is the whole point of putting it in the envelope.
    assert!(rows
        .iter()
        .all(|(_, value)| value.first() == Some(&keyspace::LIVE_VERSIONED_TAG)));
    assert!(
        rows.iter()
            .all(|(_, value)| keyspace::world_value_schema_floor(value) == Some(0)),
        "smoke declares no schema_version, so its archetype floors at 0 — and \
         the row says 0 out loud rather than saying nothing, which is the \
         difference between a self-describing row and one a reader bootstraps"
    );
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn oversize_value_is_rejected_at_plan_time() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let _ = cluster;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let mut source = std::fs::read_to_string(&root).expect("read scenario");
    source = source.replace("declared_size = \"256B\"", "declared_size = \"200KiB\"");
    let temp = write_temp_scenario("scenario", &source);
    let output = Command::new(seed_bin())
        .args(["plan", "--profile", "demo", "--json"])
        .arg(temp.path())
        .output()
        .expect("plan");
    assert!(
        !output.status.success(),
        "oversized plan should fail: stdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("100 KB"),
        "expected a plan-time size rejection naming the 100 KB limit"
    );
}

/// The `content/version` row round-trips: `apply` writes the scenario's
/// content build and manifest digest, and reading the durable row back
/// yields both.
///
/// `content/version` is a single global key, so a concurrent gate in this
/// same binary can overwrite or clear it between this test's write and its
/// read (#999). The scenario therefore carries a `content_build` nobody
/// else uses, and the read is retried until the row it finds is the one
/// this test wrote — which is also what stops the assertion from passing
/// against somebody else's row. The fixture reset is grid-scoped
/// (`reset_grid`), so this test never clears the row for anyone else; the
/// only remaining clearers are the wipes this suite runs because the wipe
/// itself is under test.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_content_version_roundtrips() {
    const BUILD: &str = "demo-999-roundtrip";

    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root)
        .expect("read scenario")
        .replace("\"demo-2026-08-13\"", &format!("\"{BUILD}\""));
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9404), false));
    let db = open_db(&cluster);
    reset_grid(&db, GridId::new(9404)).await;

    let mut seen = None;
    for _ in 0..4 {
        let output = run_seed(
            &["apply", "--profile", "demo", "--allow-opaque"],
            temp.path(),
        )
        .await;
        maybe_assert_success(&output, "demo apply");
        let Ok(version) = read_content_version(&db).await else {
            continue;
        };
        if version.content_build == BUILD {
            seen = Some(version);
            break;
        }
    }
    let version =
        seen.expect("a concurrent gate overwrote or cleared content/version on every attempt");
    assert_eq!(version.content_build, BUILD);
    assert!(!version.manifest_digest.is_empty());
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn wipe_leaves_ckpt_rows_intact() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let _ = cluster;
    let db = open_db(&fdb_cluster_file().unwrap());
    let store = Arc::new(FdbCheckpointStore::connect(&fdb_cluster_file().unwrap()).expect("store"));
    let store: Arc<dyn CheckpointStore> = store.clone();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9406), false));
    retire_fdb_checkpoint_fence(&fdb_cluster_file().unwrap(), GridId::new(9406)).await;
    reset_grid(&db, GridId::new(9406)).await;
    activate_fdb_checkpoint_fence(&fdb_cluster_file().unwrap(), GridId::new(9406)).await;

    let rt = CellRuntime::open(&runtime_config(dir.path(), GridId::new(9406)), &store)
        .await
        .unwrap();
    for i in 0..4u64 {
        rt.apply(record(GridId::new(9406), CellId::ROOT, i))
            .await
            .unwrap();
    }
    rt.checkpoint(store.as_ref()).await.unwrap();
    rt.close().await.unwrap();

    let before = db
        .run(|trx, _| async move {
            trx.get(&keyspace::ckpt_key(GridId::new(9406), CellId::ROOT), false)
                .await
                .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))
        })
        .await
        .unwrap();
    assert!(before.is_some(), "checkpoint row exists before wipe");

    retire_fdb_checkpoint_fence(&fdb_cluster_file().unwrap(), GridId::new(9406)).await;
    let output = run_seed(
        &["wipe", "--yes", "--content-build", "demo-2026-08-13"],
        temp.path(),
    )
    .await;
    maybe_assert_success(&output, "wipe");
    let after = db
        .run(|trx, _| async move {
            trx.get(&keyspace::ckpt_key(GridId::new(9406), CellId::ROOT), false)
                .await
                .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))
        })
        .await
        .unwrap();
    assert!(after.is_some(), "wipe leaves ckpt rows intact");
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn cold_area_load_returns_seeded_entities() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let store = Arc::new(FdbCheckpointStore::connect(&cluster).expect("store"));
    let store_ckpt: Arc<dyn CheckpointStore> = store.clone();
    let store_cold: Arc<dyn ColdCellReader> = store.clone();
    let dir = tempfile::tempdir().expect("tempdir");
    let grid = GridId::new(9403);
    let centre = cell(4, 0, 0);
    let cells = centre.neighbors27();
    let db = open_db(&cluster);
    retire_fdb_checkpoint_fence(&cluster, grid).await;
    reset_grid(&db, grid).await;
    activate_fdb_checkpoint_fence(&cluster, grid).await;

    let rt = CellRuntime::open(&runtime_config(dir.path(), grid), &store_ckpt)
        .await
        .unwrap();
    for (i, cell) in cells.iter().copied().enumerate() {
        rt.apply(record(grid, cell, i as u64)).await.unwrap();
    }
    rt.checkpoint(store_ckpt.as_ref()).await.unwrap();
    rt.close().await.unwrap();

    let live_store: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let live = tokio::sync::Mutex::new(
        CellRuntime::open(&runtime_config(dir.path(), grid), &live_store)
            .await
            .unwrap(),
    );
    let router = ColdFallbackRouter::new(live, store_cold);
    let started = Instant::now();
    let mut pages = 0usize;
    for cell in cells {
        let page = router.read_cold(grid, cell).await.expect("cold read");
        let page = page.expect("page");
        pages += 1;
        assert!(!page.entities.is_empty(), "seeded cell has entities");
    }
    assert_eq!(pages, 27, "area load covers the seeded neighbourhood");

    // Gate A3's correctness half is above: a cold, never-loaded, seeded world
    // serves all 27 cells of the neighbourhood. Its *latency* half — D16's
    // < 50 ms first page-in — is deliberately NOT asserted here. A wall-clock
    // bound inside a unit test that shares a machine and one FDB with the rest
    // of the suite is green on an idle laptop and red on a busy CI box: it
    // passes standalone in ~3 s and fails in the full four-package run. The
    // target is enforced where it can be measured under controlled load, by
    // `gates/p2-dashboard`, which gates `area_first_page_ms` at 50_000 µs against the
    // rig's telemetry. Report the elapsed time so a pathological regression is
    // still visible in the log.
    eprintln!(
        "cold 27-cell area load: {:?} (D16 target < 50 ms, gated by gates/p2-dashboard)",
        started.elapsed()
    );
}

/// #947: `apply --universe-seed-fingerprint` seals the world to a universe,
/// and the seal survives the round trip through the durable row.
///
/// The value is a fingerprint, never the seed: this is the whole reason the
/// flag takes 32 hexadecimal characters rather than 64, and the reason the
/// universe's secret never has to reach a seeding host.
///
/// `content/version` is a single global key, so a concurrent gate in this same
/// binary can overwrite or clear it between this test's write and its read. The
/// scenario therefore carries a `content_build` nobody else uses, and the read
/// is retried until the row it finds is the one this test wrote — which is also
/// what stops the assertion from passing against somebody else's row. An absent
/// row is as little ours as a stranger's build, so a sibling's wipe is retried
/// the same way (#999). The fixture resets around the applies are grid-scoped
/// (`reset_grid`), so this test never clears the row for anyone else; the only
/// remaining clearers are the wipes this suite runs because the wipe itself is
/// under test.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn apply_seals_the_world_to_a_universe_seed_fingerprint() {
    const BUILD: &str = "smoke-947-seal";

    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("smoke.toml");
    let source = std::fs::read_to_string(&root)
        .expect("read scenario")
        .replace("\"smoke-2026-08-13\"", &format!("\"{BUILD}\""));
    let temp = write_temp_scenario("seal", &with_grid(&source, GridId::new(9470), true));
    let db = open_db(&cluster);
    reset_grid(&db, GridId::new(9470)).await;

    let seed = orrery_protocol::UniverseSeed([0x94; 32]);
    let expected = seed.fingerprint();

    let mut seen = None;
    for _ in 0..4 {
        let output = run_seed(
            &[
                "apply",
                "--allow-opaque",
                "--universe-seed-fingerprint",
                &expected.to_hex(),
            ],
            temp.path(),
        )
        .await;
        maybe_assert_success(&output, "sealed apply");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&expected.to_hex()),
            "the report must show the operator what the world was sealed to"
        );
        let Ok(version) = read_content_version(&db).await else {
            continue;
        };
        if version.content_build == BUILD {
            seen = Some(version);
            break;
        }
    }
    let version =
        seen.expect("a concurrent gate overwrote or cleared content/version on every attempt");
    assert_eq!(
        version.universe_seed_fingerprint,
        Some(expected),
        "the seal the operator passed must be the seal the cluster holds"
    );

    // A re-apply without the flag leaves the world unsealed rather than
    // inventing a seal — the seeder never derives one, because it never sees
    // a seed.
    reset_grid(&db, GridId::new(9470)).await;
    for _ in 0..4 {
        let output = run_seed(&["apply", "--allow-opaque"], temp.path()).await;
        maybe_assert_success(&output, "unsealed apply");
        let Ok(version) = read_content_version(&db).await else {
            continue;
        };
        if version.content_build == BUILD {
            assert_eq!(
                version.universe_seed_fingerprint, None,
                "no flag means no seal, not a guessed one"
            );
            return;
        }
    }
    panic!("a concurrent gate overwrote or cleared content/version on every attempt");
}
