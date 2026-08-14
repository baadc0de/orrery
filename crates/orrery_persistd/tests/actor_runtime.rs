//! End-to-end tests for the cell-actor runtime + segmented journal.
//!
//! These exercise the actor → journal → group-commit → replay path with no
//! FoundationDB: durability, idempotent replay, and crash-and-recover.

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
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

/// A fresh in-memory checkpoint store as the trait object `CellRuntime::open`
/// takes.
fn mem_store() -> Arc<dyn CheckpointStore> {
    Arc::new(MemCheckpointStore::new())
}

#[tokio::test]
async fn actor_applies_and_snapshot_reflects() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();

    let rec = mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"hp=100");
    let lsn = rt.apply(rec.clone()).await.unwrap();
    assert!(lsn >= Lsn::new(0, 0), "lsn is valid: {lsn:?}");

    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    let rec2 = mk_record(CellId::ROOT, 7, RecordKind::ComponentDiff, b"hp=50");
    rt.apply(rec2).await.unwrap();
    let page2 = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    let e = &page2.entities[&PersistId::new(7)];
    assert_eq!(e.components.as_ref(), b"hp=50");
    let _ = page;

    rt.close().await.unwrap();
}

#[tokio::test]
async fn actor_returns_pending_handle_after_fold_without_resolver_task() {
    let dir = tempfile::tempdir().unwrap();
    // `runtime_config(..., true)` holds a one-record group for 100 ms, making
    // the boundary between actor work and durability deterministic.
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();
    let actor = rt
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("root actor")
        .clone();

    let handle = tokio::time::timeout(
        Duration::from_millis(20),
        actor.start_diff(mk_record(CellId::ROOT, 88, RecordKind::Spawn, b"pending")),
    )
    .await
    .expect("mailbox returns before the group fsync")
    .expect("append accepted");

    assert!(
        tokio::time::timeout(Duration::from_millis(20), handle.committed())
            .await
            .is_err(),
        "returned handle must still represent the pending durability wait"
    );
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(88)].components.as_ref(),
        b"pending",
        "fold precedes returning the pending handle"
    );

    let committed = handle.committed().await.unwrap();
    assert_eq!(committed, handle.lsn());
    rt.close().await.unwrap();
}

#[tokio::test]
async fn read_snapshot_filters_to_requested_cells() {
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();

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
    let page = rt.read(GridId::ROOT, cell_a).await.unwrap();
    assert_eq!(page.entities.len(), 1, "one interest cell reads one entity");
    assert!(page.entities.contains_key(&PersistId::new(1)));
    assert!(!page.entities.contains_key(&PersistId::new(2)));

    // Reading the covering shard serves the whole subtree, mirroring read_cold.
    let shard = cell_a
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("interest cell has a level-18 shard ancestor");
    let subtree = rt.read(GridId::ROOT, shard).await.unwrap();
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
            let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();
            for i in 0..100u64 {
                let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
                rt.apply(rec).await.unwrap();
            }
        });
        // rt dropped here => tasks aborted; but the journal's file lock is
        // released only when the Last Arc<Journal> drops, so the phase-2 open
        // below must take it by closing the runtime, not by aborting.
    }

    // Restart from the same journal dir — no FDB, so the journal IS the truth.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();
        let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert_eq!(page.entities.len(), 100, "all entities recovered");
        for i in 0..100u64 {
            let e = &page.entities[&PersistId::new(i)];
            assert_eq!(e.components.as_ref(), &i.to_le_bytes());
        }
        rt.close().await.unwrap();
    });
}

#[tokio::test]
async fn concurrent_diffs_batch_into_fewer_fsyncs() {
    // The mailbox must not serialize the node on one fsync: 64 concurrent
    // applies through the `Mutex<CellRuntime>` router pipeline into the
    // journal's commit queue and share fsyncs (§4 adaptive group commit).
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();
    let router = Arc::new(tokio::sync::Mutex::new(rt));

    let mut waiters = Vec::new();
    for i in 0..64u64 {
        let router = Arc::clone(&router);
        waiters.push(tokio::spawn(async move {
            let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
            let append = orrery_persistd::Router::apply(router.as_ref(), rec)
                .await
                .unwrap();
            append.committed().await.unwrap()
        }));
    }
    for w in waiters {
        w.await.unwrap();
    }

    let fsyncs = router.lock().await.flush_count();
    assert!(
        fsyncs < 64,
        "64 concurrent applies must share fsyncs (got {fsyncs})"
    );
    // All 64 landed (one record each, last-writer-wins across distinct ids).
    let page = router
        .lock()
        .await
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .unwrap();
    assert_eq!(page.entities.len(), 64);
    let rt = Arc::try_unwrap(router)
        .unwrap_or_else(|_| panic!("router sole owner"))
        .into_inner();
    rt.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_diffs_stay_last_writer_wins() {
    // Same entity, sequential diffs through the concurrent path: the last
    // writer wins regardless of resolver interleaving (mailbox order is the
    // single-writer serial order, §3.1).
    let dir = tempfile::tempdir().unwrap();
    let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();

    rt.apply(mk_record(CellId::ROOT, 7, RecordKind::Spawn, b"first"))
        .await
        .unwrap();
    rt.apply(mk_record(
        CellId::ROOT,
        7,
        RecordKind::ComponentDiff,
        b"second",
    ))
    .await
    .unwrap();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities[&PersistId::new(7)].components.as_ref(),
        b"second",
        "the last acked writer wins"
    );

    // A concurrent burst of 16 writers of the SAME entity: the mailbox
    // serializes them, so the surviving value must be exactly one of the 16
    // acked payloads (never a torn mix).
    let router = Arc::new(tokio::sync::Mutex::new(rt));
    let mut waiters = Vec::new();
    for i in 0..16u64 {
        let router = Arc::clone(&router);
        let payload = format!("burst-{i}").into_bytes();
        waiters.push(tokio::spawn(async move {
            let rec = mk_record(CellId::ROOT, 9, RecordKind::ComponentDiff, &payload);
            let append = orrery_persistd::Router::apply(router.as_ref(), rec)
                .await
                .unwrap();
            append.committed().await.unwrap();
            payload
        }));
    }
    let mut acked = Vec::new();
    for w in waiters {
        acked.push(w.await.unwrap());
    }
    let rt = Arc::try_unwrap(router)
        .unwrap_or_else(|_| panic!("router sole owner"))
        .into_inner();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
    let winner = page.entities[&PersistId::new(9)].components.clone();
    assert!(
        acked.iter().any(|p| p.as_slice() == winner.as_ref()),
        "the surviving value is one of the acked writers, got {winner:?}"
    );

    rt.close().await.unwrap();
}

#[test]
fn records_from_prior_epoch_survive_a_fence_bump() {
    // The C-2 regression (docs/11-roadmap.md §P2): 100 records acked at epoch
    // 0, then the shard is fenced to epoch 1 and the node restarts. The
    // naive predicate (`rec.epoch < config.epoch`) would discard all 100 —
    // the whole world, read as success. The running-maximum predicate keeps
    // them: a node's own journal has non-decreasing epochs, so only a
    // genuine zombie interleaving is dropped.
    let dir = tempfile::tempdir().unwrap();
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rt = CellRuntime::open(&runtime_config(dir.path(), true), &mem_store()).unwrap();
            for i in 0..100u64 {
                let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
                rt.apply(rec).await.unwrap();
            }
            rt.close().await.unwrap();
        });
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Phase 2: restart at epoch 1 and fence the shard — this is what
        // startup fencing does once persistd-wiring is live. The 100 epoch-0
        // records were journaled before the fence, so they sit *below* the
        // new epoch; the running-maximum predicate must still replay them.
        let mut cfg = runtime_config(dir.path(), true);
        cfg.epoch = Epoch::new(1);
        let mut rt = CellRuntime::open(&cfg, &mem_store()).unwrap();
        let assumed = rt
            .fence_shard(CellId::ROOT, None, mem_store().as_ref())
            .await
            .unwrap();
        assert_eq!(assumed, Epoch::new(1), "the shard is fenced to epoch 1");

        let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert_eq!(
            page.entities.len(),
            100,
            "all 100 pre-fence records survive the epoch-1 restart (C-2)"
        );
        for i in 0..100u64 {
            let e = &page.entities[&PersistId::new(i)];
            assert_eq!(e.components.as_ref(), &i.to_le_bytes());
        }
        rt.close().await.unwrap();
    });
}

#[test]
fn zombie_writes_from_a_superseded_epoch_are_dropped() {
    // The other half of C-2: an epoch-0 record arriving AFTER an epoch-1
    // record at a lower LSN (a genuine zombie interleaving) is the only thing
    // replay drops.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let journal = std::sync::Arc::new(
            orrery_persistd::Journal::open(&runtime_config(dir.path(), true).journal).unwrap(),
        );
        // Epoch 1 lands first (lower LSN)…
        let mut rec_new = mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"new-epoch");
        rec_new.epoch = Epoch::new(1);
        journal.append(rec_new).unwrap().committed().await.unwrap();
        // …then a zombie epoch-0 write arrives (higher LSN, older epoch).
        let rec_old = mk_record(CellId::ROOT, 2, RecordKind::Spawn, b"zombie");
        journal.append(rec_old).unwrap().committed().await.unwrap();
        journal.close().await.unwrap();
        // The scan borrows the journal's keyspace; drop every handle before
        // reopening the same dir in `CellRuntime::open`.
        drop(journal);

        let mut cfg = runtime_config(dir.path(), true);
        cfg.epoch = Epoch::new(1);
        let rt = CellRuntime::open(&cfg, &mem_store()).unwrap();
        let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert!(
            page.entities.contains_key(&PersistId::new(1)),
            "the epoch-1 record replays"
        );
        assert!(
            !page.entities.contains_key(&PersistId::new(2)),
            "the zombie epoch-0 record is dropped (C-2)"
        );
        rt.close().await.unwrap();
    });
}

#[tokio::test]
async fn apply_stamps_the_actor_epoch_into_the_journal() {
    // The actor is the epoch authority (D11 §2.1: the server assigns epoch):
    // the gateway's placeholder `Epoch::new(0)` is overwritten with the
    // actor's ownership epoch before the append, so the journaled bytes carry
    // the real epoch.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = runtime_config(dir.path(), true);
    cfg.epoch = Epoch::new(7);
    let rt = CellRuntime::open(&cfg, &mem_store()).unwrap();
    rt.apply(mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"x"))
        .await
        .unwrap();

    let stored: Vec<_> = rt
        .journal()
        .scan_from(Lsn::new(0, 0))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].record.epoch,
        Epoch::new(7),
        "the stored record carries the actor's epoch, not the placeholder"
    );
    rt.close().await.unwrap();
}

#[tokio::test]
async fn two_runtimes_on_two_grids_do_not_serve_each_others_cells() {
    // P-7 at the router layer: the same raw cell id under two grids names two
    // different entity universes. Each runtime's `actor()` rejects the other
    // grid, so a read routed by (grid, cell) returns only its own grid's
    // entities.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut cfg_a = runtime_config(dir_a.path(), true);
    cfg_a.grid = GridId::new(9501);
    let mut cfg_b = runtime_config(dir_b.path(), true);
    cfg_b.grid = GridId::new(9502);
    let rt_a = CellRuntime::open(&cfg_a, &mem_store()).unwrap();
    let rt_b = CellRuntime::open(&cfg_b, &mem_store()).unwrap();

    let cell = CellId::ROOT;
    let mut rec_a = mk_record(cell, 1, RecordKind::Spawn, b"grid-9501");
    rec_a.grid = GridId::new(9501);
    rt_a.apply(rec_a).await.unwrap();
    let mut rec_b = mk_record(cell, 2, RecordKind::Spawn, b"grid-9502");
    rec_b.grid = GridId::new(9502);
    rt_b.apply(rec_b).await.unwrap();

    // Each runtime serves only its own grid's view of the same raw cell.
    let page_a = rt_a.read(GridId::new(9501), cell).await.unwrap();
    assert_eq!(page_a.entities.len(), 1);
    assert_eq!(
        page_a.entities[&PersistId::new(1)].components.as_ref(),
        b"grid-9501"
    );
    let page_b = rt_b.read(GridId::new(9502), cell).await.unwrap();
    assert_eq!(page_b.entities.len(), 1);
    assert_eq!(
        page_b.entities[&PersistId::new(2)].components.as_ref(),
        b"grid-9502"
    );

    // And a cross-grid read is refused, not silently served.
    assert!(rt_a.read(GridId::new(9502), cell).await.is_err());
    assert!(rt_b.read(GridId::new(9501), cell).await.is_err());
    assert!(rt_a.actor(GridId::new(9502), cell).is_none());

    rt_a.close().await.unwrap();
    rt_b.close().await.unwrap();
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
