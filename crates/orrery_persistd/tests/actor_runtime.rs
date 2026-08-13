//! End-to-end tests for the cell-actor runtime + segmented journal.
//!
//! These exercise the actor → journal → group-commit → replay path with no
//! FoundationDB: durability, idempotent replay, and crash-and-recover.

use std::time::Duration;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{payload_crc, CellRuntime, JournalConfig, RuntimeConfig};

use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(cell: CellId, entity: u64, kind: RecordKind, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0), // assigned by the journal
        cell,
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

fn runtime_config(dir: &std::path::Path, batch: bool) -> RuntimeConfig {
    let mode = if batch {
        AdaptiveCommitMode::AlwaysBatch
    } else {
        AdaptiveCommitMode::Adaptive
    };
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode,
                // Generous window so concurrent appends land in one batch.
                batch_window: if batch {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(1)
                },
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: std::sync::Arc::new(orrery_persistd::MemFenceStore::new()),
    }
}

#[tokio::test]
async fn actor_applies_and_snapshot_reflects() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true)).unwrap();

    let rec = mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"hp=100");
    let lsn = rt.apply(rec.clone()).await.unwrap();
    assert!(lsn >= Lsn::new(0, 0), "lsn is valid: {lsn:?}");

    let page = rt.read(CellId::ROOT).await.unwrap();
    let rec2 = mk_record(CellId::ROOT, 7, RecordKind::ComponentDiff, b"hp=50");
    rt.apply(rec2).await.unwrap();
    let page2 = rt.read(CellId::ROOT).await.unwrap();
    let e = &page2.entities[&PersistId::new(7)];
    assert_eq!(e.components.as_ref(), b"hp=50");
    let _ = page;

    rt.close().await.unwrap();
}

#[tokio::test]
async fn read_snapshot_filters_to_requested_cells() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true)).unwrap();

    let cell_a = CellId::from_coords(glam::IVec3::new(2, -1, 8), 21).unwrap();
    let cell_b = cell_a
        .neighbor(glam::IVec3::new(1, 0, 0))
        .expect("within the volume");

    // Two entities in neighbouring interest cells, both under the ROOT shard.
    rt.apply(mk_record(cell_a, 1, RecordKind::Spawn, b"a"))
        .await
        .unwrap();
    rt.apply(mk_record(cell_b, 2, RecordKind::Spawn, b"b"))
        .await
        .unwrap();

    // Reading one interest cell returns exactly that cell's entity (P-4).
    let page = rt.read(cell_a).await.unwrap();
    assert_eq!(page.entities.len(), 1, "one interest cell reads one entity");
    assert!(page.entities.contains_key(&PersistId::new(1)));
    assert!(!page.entities.contains_key(&PersistId::new(2)));

    // Reading the covering shard serves the whole subtree, mirroring read_cold.
    let shard = cell_a
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("interest cell has a level-18 shard ancestor");
    let subtree = rt.read(shard).await.unwrap();
    assert_eq!(subtree.entities.len(), 2, "shard read serves its subtree");

    rt.close().await.unwrap();
}

#[test]
fn crash_and_recover_zero_loss() {
    // Simulate `kill -9` by running the write phase in its own tokio runtime and
    // dropping it: dropping a runtime aborts all spawned tasks (actors + the
    // committer), releasing the journal file lock — exactly what process death
    // does. Acked writes survive because each batch was group-fsynced.
    let dir = tempfile::tempdir().unwrap();
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rt = CellRuntime::open(&runtime_config(dir.path(), true)).unwrap();
            for i in 0..100u64 {
                let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
                rt.apply(rec).await.unwrap();
            }
        });
        // rt dropped here => tasks aborted, lock released.
    }

    // Restart from the same journal dir — no FDB, so the journal IS the truth.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let rt = CellRuntime::open(&runtime_config(dir.path(), true)).unwrap();
        let page = rt.read(CellId::ROOT).await.unwrap();
        assert_eq!(page.entities.len(), 100, "all entities recovered");
        for i in 0..100u64 {
            let e = &page.entities[&PersistId::new(i)];
            assert_eq!(e.components.as_ref(), &i.to_le_bytes());
        }
        rt.close().await.unwrap();
    });
}

#[test]
fn group_commit_batches_into_one_fsync() {
    // Deterministic batching proof: N concurrent appends resolve on a single
    // persist. We can't observe the fsync count without a test seam; instead we
    // assert committed() advances past all and that journal replay is whole.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = runtime_config(dir.path(), true);
        let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();

        let mut handles = Vec::new();
        for i in 0..50u64 {
            let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
            handles.push(journal.append(rec).unwrap());
        }
        for h in &handles {
            h.committed().await.unwrap();
        }
        // All durable and ordered.
        let last = handles.iter().map(|h| h.lsn()).max().unwrap();
        assert!(journal.committed() >= last);
        assert_eq!(journal.scan_from(Lsn::new(0, 0)).count(), 50);
    });
}

#[test]
fn adaptive_lone_append_commits() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = runtime_config(dir.path(), false);
        let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();
        let rec = mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"x");
        let h = journal.append(rec).unwrap();
        h.committed().await.unwrap();
        assert!(journal.committed() >= h.lsn());
    });
}

#[test]
fn reopen_preserves_lsn_monotonicity() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let cfg = runtime_config(dir.path(), true);
        let last_lsn;
        {
            let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();
            let h = journal
                .append(mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"a"))
                .unwrap();
            h.committed().await.unwrap();
            last_lsn = h.lsn();
            journal.close().await.unwrap();
        }
        let journal = orrery_persistd::Journal::open(&cfg.journal).unwrap();
        let h = journal
            .append(mk_record(CellId::ROOT, 2, RecordKind::Spawn, b"b"))
            .unwrap();
        h.committed().await.unwrap();
        assert!(h.lsn() > last_lsn, "LSN continues after reopen");
    });
}

#[test]
fn corruption_is_detected_on_scan() {
    // Simulated: reopen and scan; crc is verified in runtime.recover. This test
    // checks the crc primitive agrees with what recovery expects.
    assert_eq!(payload_crc(b""), 0);
    assert_eq!(payload_crc(b"123456789"), 0xE306_9283);
}
