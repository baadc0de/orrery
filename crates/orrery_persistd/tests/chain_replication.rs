//! Chain replication integration tests (P2 gap #1).
//!
//! Exercises the async journal→follower replication path: a primary journal
//! streams committed records to a follower journal via the in-process
//! [`MemChainTransport`], the follower's watermark advances, and a follower
//! journal can serve as the recovery source (RPO ≤ replication lag).

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ChainConfig, Journal, JournalChainSink, JournalConfig,
    MemChainTransport, RuntimeConfig,
};

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

fn journal_config(dir: &std::path::Path) -> JournalConfig {
    JournalConfig {
        dir: dir.to_path_buf(),
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::AlwaysBatch,
            batch_window: Duration::from_millis(100),
            batch_max_records: 100_000,
            batch_max_bytes: 1 << 20,
        },
    }
}

fn runtime_config(dir: &std::path::Path, node_id: u64) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: journal_config(dir),
        node_id,
        epoch: Epoch::new(0),
        fence: std::sync::Arc::new(orrery_persistd::MemFenceStore::new()),
    }
}

#[tokio::test]
async fn chain_replicates_records_to_follower() {
    let primary_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let primary = Arc::new(Journal::open(&journal_config(primary_dir.path())).unwrap());
    let follower = Arc::new(Journal::open(&journal_config(follower_dir.path())).unwrap());

    // Follower sink + in-process transport.
    let sink = Arc::new(JournalChainSink::new(Arc::clone(&follower)));
    let transport = Arc::new(MemChainTransport::new(sink));

    let replicator = orrery_persistd::spawn_chain(
        Arc::clone(&primary),
        transport,
        &ChainConfig {
            follower: 2,
            ..ChainConfig::default()
        },
    );
    assert_eq!(replicator.follower(), 2);

    // Write records on the primary.
    for i in 0..50u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        let handle = primary.append(rec).unwrap();
        handle.committed().await.unwrap();
    }

    // Wait for the follower to catch up.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let wm = replicator.follower_watermark();
        if wm.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "follower never caught up"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The follower journal holds all 50 records.
    assert_eq!(follower.scan_from(Lsn::new(0, 0)).count(), 50);

    replicator.shutdown().await;
    primary.close().await.unwrap();
    follower.close().await.unwrap();
}

#[tokio::test]
async fn follower_serves_as_recovery_source() {
    // Simulate primary node loss: the follower journal (which replicated every
    // acked record) can rebuild the world with zero loss.
    let primary_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let primary = Arc::new(Journal::open(&journal_config(primary_dir.path())).unwrap());
    let follower = Arc::new(Journal::open(&journal_config(follower_dir.path())).unwrap());

    let sink = Arc::new(JournalChainSink::new(Arc::clone(&follower)));
    let transport: Arc<dyn orrery_persistd::ChainTransport> =
        Arc::new(MemChainTransport::new(sink.clone()));
    let replicator = orrery_persistd::spawn_chain(
        Arc::clone(&primary),
        Arc::clone(&transport),
        &ChainConfig::default(),
    );

    // Write 100 acked records on the primary.
    for i in 0..100u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        let handle = primary.append(rec).unwrap();
        handle.committed().await.unwrap();
    }

    // Wait for the follower to catch up.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if replicator.follower_watermark().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "follower never caught up"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Drop the primary (simulating node loss); the follower journal is intact.
    primary.close().await.unwrap();
    drop(primary);

    // Stop replication and release the follower journal's lock. The sink,
    // transport, and replicator task all hold `Arc<Journal>` clones, and the
    // `follower` Arc itself holds the Database, so drop them all before
    // reopening the same directory.
    replicator.shutdown().await;
    drop(sink);
    drop(transport);
    follower.close().await.unwrap();
    drop(follower);

    // Recover the world from the follower journal alone.
    let rt = CellRuntime::open(&runtime_config(follower_dir.path(), 2)).unwrap();
    let page = rt.read(CellId::ROOT).await.unwrap();
    assert_eq!(
        page.entities.len(),
        100,
        "all entities recovered from follower"
    );
    for i in 0..100u64 {
        let e = &page.entities[&PersistId::new(i)];
        assert_eq!(e.components.as_ref(), &i.to_le_bytes());
    }
    rt.close().await.unwrap();
}
