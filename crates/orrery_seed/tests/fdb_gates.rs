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
use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

#[cfg(feature = "fdb")]
fn fdb_cluster_file() -> Option<String> {
    if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
        return Some(path);
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".fdb-dev/fdb.cluster");
        if candidate.exists() {
            return Some(candidate.display().to_string());
        }
        if !dir.pop() {
            return None;
        }
    }
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
async fn run_seed(args: &[&str], scenario: &std::path::Path) -> std::process::Output {
    Command::new(seed_bin())
        .args(args)
        .arg(scenario)
        .output()
        .expect("run seed binary")
}

#[cfg(feature = "fdb")]
async fn wipe_scenario(scenario: &std::path::Path, content_build: &str) {
    let output = run_seed(
        &["wipe", "--yes", "--content-build", content_build],
        scenario,
    )
    .await;
    maybe_assert_success(&output, "pre-wipe");
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
    postcard::from_bytes(&bytes).map_err(|e| format!("decode content/version: {e}"))
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_reseed_preserves_persist_ids() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let _ = cluster;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9402), false));
    wipe_scenario(temp.path(), "demo-2026-08-13").await;

    orrery_seed::fdb_network();
    let db = Database::from_path(&fdb_cluster_file().unwrap()).expect("db");
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
async fn every_written_value_carries_the_live_tag() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let _ = cluster;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("smoke.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9401), true));
    wipe_scenario(temp.path(), "smoke-2026-08-13").await;

    let output = run_seed(&["apply", "--allow-opaque"], temp.path()).await;
    maybe_assert_success(&output, "smoke apply");

    orrery_seed::fdb_network();
    let db = Database::from_path(&fdb_cluster_file().unwrap()).expect("db");
    let rows = scan_world_rows(&db, GridId::new(9401))
        .await
        .expect("scan world");
    assert_eq!(rows.len(), 1_000, "smoke writes exactly 1000 world rows");
    assert!(rows
        .iter()
        .all(|(_, value)| value.first() == Some(&keyspace::LIVE_TAG)));
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

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_content_version_roundtrips() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let _ = cluster;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9404), false));
    wipe_scenario(temp.path(), "demo-2026-08-13").await;
    let output = run_seed(
        &["apply", "--profile", "demo", "--allow-opaque"],
        temp.path(),
    )
    .await;
    maybe_assert_success(&output, "demo apply");

    orrery_seed::fdb_network();
    let db = Database::from_path(&fdb_cluster_file().unwrap()).expect("db");
    let version = read_content_version(&db).await.expect("content version");
    assert_eq!(version.content_build, "demo-2026-08-13");
    assert!(!version.manifest_digest.is_empty());
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn wipe_leaves_ckpt_rows_intact() {
    let Some(cluster) = skip_if_no_fdb() else {
        return;
    };
    let _ = cluster;
    orrery_seed::fdb_network();
    let db = Database::from_path(&fdb_cluster_file().unwrap()).expect("db");
    let store = Arc::new(FdbCheckpointStore::connect(&fdb_cluster_file().unwrap()).expect("store"));
    let store: Arc<dyn CheckpointStore> = store.clone();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, GridId::new(9406), false));
    wipe_scenario(temp.path(), "demo-2026-08-13").await;

    let rt = CellRuntime::open(&runtime_config(dir.path(), GridId::new(9406)), &store).unwrap();
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
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join("p2demo.toml");
    let source = std::fs::read_to_string(&root).expect("read scenario");
    let temp = write_temp_scenario("scenario", &with_grid(&source, grid, false));
    wipe_scenario(temp.path(), "demo-2026-08-13").await;

    let rt = CellRuntime::open(&runtime_config(dir.path(), grid), &store_ckpt).unwrap();
    for (i, cell) in cells.iter().copied().enumerate() {
        rt.apply(record(grid, cell, i as u64)).await.unwrap();
    }
    rt.checkpoint(store_ckpt.as_ref()).await.unwrap();
    rt.close().await.unwrap();

    let live_store: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let live = tokio::sync::Mutex::new(
        CellRuntime::open(&runtime_config(dir.path(), grid), &live_store).unwrap(),
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
    // `p2-dashboard`, which gates `area_first_page_ms` at 50_000 µs against the
    // rig's telemetry. Report the elapsed time so a pathological regression is
    // still visible in the log.
    eprintln!(
        "cold 27-cell area load: {:?} (D16 target < 50 ms, gated by p2-dashboard)",
        started.elapsed()
    );
}
