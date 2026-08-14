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
    payload_crc, CellRuntime, ChainConfig, ChainTransport, Journal, JournalChainSink,
    JournalConfig, JournalError, MemChainTransport, RuntimeConfig,
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

/// Count the records in a journal (replay-scan length).
fn record_count(journal: &Journal) -> usize {
    journal
        .scan_from(Lsn::new(0, 0))
        .collect::<Result<Vec<_>, _>>()
        .expect("scan")
        .len()
}

/// Build a 2-node replication ring exactly the way
/// [`Cluster::new`](orrery_persistd::Cluster::new) wires it (cluster.rs
/// `Cluster::new`: sorted ids, each node's follower is the next, last wraps to
/// first): node 0 → node 1 and node 1 → node 0, each through a
/// [`JournalChainSink`] + [`MemChainTransport`] pair.
fn build_ring(
    a: &Arc<Journal>,
    b: &Arc<Journal>,
) -> (
    orrery_persistd::ChainReplicator,
    orrery_persistd::ChainReplicator,
) {
    let sink_a = Arc::new(JournalChainSink::new(Arc::clone(a)));
    let transport_a: Arc<dyn ChainTransport> = Arc::new(MemChainTransport::new(sink_a));
    let sink_b = Arc::new(JournalChainSink::new(Arc::clone(b)));
    let transport_b: Arc<dyn ChainTransport> = Arc::new(MemChainTransport::new(sink_b));
    let chain_a = orrery_persistd::spawn_chain(
        Arc::clone(a),
        transport_b,
        &ChainConfig {
            follower: 1,
            ..ChainConfig::default()
        },
    );
    let chain_b = orrery_persistd::spawn_chain(
        Arc::clone(b),
        transport_a,
        &ChainConfig {
            follower: 0,
            ..ChainConfig::default()
        },
    );
    (chain_a, chain_b)
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

/// Records are inserted into Fjall before the group fsync, so the originated
/// index can see them before the client durability boundary. This transport
/// makes any premature replication directly observable without introducing a
/// follower commit of its own.
#[derive(Default)]
struct RecordingTransport {
    records: std::sync::Mutex<Vec<JournalRecord>>,
}

#[async_trait::async_trait]
impl ChainTransport for RecordingTransport {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        let lsn = record.lsn;
        self.records.lock().unwrap().push(record);
        Ok(lsn)
    }

    async fn follower_watermark(&self) -> Option<Lsn> {
        None
    }
}

#[tokio::test]
async fn chain_waits_for_primary_commit_before_mirroring() {
    let primary_dir = tempfile::tempdir().unwrap();
    let primary = Arc::new(
        Journal::open(&JournalConfig {
            dir: primary_dir.path().to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(500),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        })
        .unwrap(),
    );
    let transport = Arc::new(RecordingTransport::default());
    // Stage the row before spawning the replicator. Its initial correctness
    // scan therefore deterministically encounters the originated index entry
    // while the primary committed watermark is still empty.
    let handle = primary
        .append(mk_record(CellId::ROOT, 1, RecordKind::Spawn, b"pending"))
        .unwrap();
    let replicator = orrery_persistd::spawn_chain(
        Arc::clone(&primary),
        transport.clone(),
        &ChainConfig::default(),
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        transport.records.lock().unwrap().is_empty(),
        "an uncommitted primary record must not reach the follower"
    );

    tokio::time::timeout(Duration::from_secs(2), handle.committed())
        .await
        .expect("primary commit timed out")
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while transport.records.lock().unwrap().len() != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "committed record was not mirrored after the commit wakeup"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    replicator.shutdown().await;
    primary.close().await.unwrap();
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
    let store: std::sync::Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
        std::sync::Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
    let rt = CellRuntime::open(&runtime_config(follower_dir.path(), 2), &store).unwrap();
    let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
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

/// A follower transport that can be taken offline without replacing its
/// durable sink. While offline both append and watermark probes fail from the
/// replicator's point of view; once restored, the sink exposes its last durable
/// origin LSN and accepts the missing tail.
struct RecoveringTransport {
    sink: Arc<JournalChainSink>,
    online: std::sync::atomic::AtomicBool,
    attempts: std::sync::atomic::AtomicUsize,
}

impl RecoveringTransport {
    fn new(sink: Arc<JournalChainSink>) -> Self {
        Self {
            sink,
            online: std::sync::atomic::AtomicBool::new(false),
            attempts: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn restore(&self) {
        self.online
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[async_trait::async_trait]
impl ChainTransport for RecoveringTransport {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if !self.online.load(std::sync::atomic::Ordering::Acquire) {
            return Err(JournalError::Store("follower unavailable".into()));
        }
        orrery_persistd::ChainSink::append(&*self.sink, record).await
    }

    async fn follower_watermark(&self) -> Option<Lsn> {
        self.online
            .load(std::sync::atomic::Ordering::Acquire)
            .then(|| self.sink.watermark())
            .flatten()
    }
}

#[tokio::test]
async fn follower_outage_replays_complete_primary_tail_without_new_wakeup() {
    let primary_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();
    let primary = Arc::new(Journal::open(&journal_config(primary_dir.path())).unwrap());
    let follower = Arc::new(Journal::open(&journal_config(follower_dir.path())).unwrap());

    // These commits precede the subscription and therefore exercise initial
    // recovery from the durable journal rather than the broadcast fast path.
    for i in 0..3u64 {
        let handle = primary
            .append(mk_record(
                CellId::ROOT,
                i,
                RecordKind::Spawn,
                &i.to_le_bytes(),
            ))
            .unwrap();
        handle.committed().await.unwrap();
    }
    primary.close().await.unwrap();
    drop(primary);
    let primary = Arc::new(Journal::open(&journal_config(primary_dir.path())).unwrap());

    let sink = Arc::new(JournalChainSink::new(Arc::clone(&follower)));
    let transport = Arc::new(RecoveringTransport::new(sink));
    let replicator = orrery_persistd::spawn_chain(
        Arc::clone(&primary),
        transport.clone(),
        &ChainConfig {
            follower: 2,
            batch_max: 2,
            ..ChainConfig::default()
        },
    );

    // Local durable acknowledgements continue while the follower is down.
    // No later append is made after recovery, so catch-up cannot depend on a
    // fresh broadcast message to wake the task.
    for i in 3..6u64 {
        let handle = primary
            .append(mk_record(
                CellId::ROOT,
                i,
                RecordKind::Spawn,
                &i.to_le_bytes(),
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.committed())
            .await
            .expect("primary ack must not wait for follower")
            .unwrap();
    }

    let failed_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while transport
        .attempts
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        assert!(
            std::time::Instant::now() < failed_deadline,
            "replicator never attempted the unavailable follower"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    transport.restore();

    let caught_up = primary.committed();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while replicator.follower_watermark() != Some(caught_up) {
        assert!(
            std::time::Instant::now() < deadline,
            "follower did not replay the complete outage tail"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let primary_records = primary
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .collect::<Vec<_>>();
    let follower_records = follower
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.unwrap().record)
        .collect::<Vec<_>>();
    assert_eq!(follower_records.len(), 6, "no duplicate durable records");
    assert_eq!(
        follower_records
            .iter()
            .map(|record| (record.lsn, record.entity, record.payload.clone()))
            .collect::<Vec<_>>(),
        primary_records
            .iter()
            .map(|record| (record.lsn, record.entity, record.payload.clone()))
            .collect::<Vec<_>>(),
        "replay preserves the primary's complete contiguous order"
    );

    replicator.shutdown().await;
    primary.close().await.unwrap();
    follower.close().await.unwrap();
}

#[tokio::test]
async fn ring_does_not_amplify() {
    // A 2-node ring wired exactly as `Cluster::new` wires it. Records applied
    // to node 0 replicate to node 1; node 1 must NOT re-broadcast the
    // replicated records back to node 0 (they arrived via chain replication,
    // so `Journal::append_replicated` keeps them out of the follower's
    // `published` stream). Before the fix this grew without bound.
    let dir0 = tempfile::tempdir().unwrap();
    let dir1 = tempfile::tempdir().unwrap();
    let node0 = Arc::new(Journal::open(&journal_config(dir0.path())).unwrap());
    let node1 = Arc::new(Journal::open(&journal_config(dir1.path())).unwrap());

    let (chain0, chain1) = build_ring(&node0, &node1);

    const N: u64 = 10;
    for i in 0..N {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        let handle = node0.append(rec).unwrap();
        handle.committed().await.unwrap();
    }

    // Let replication settle, then count.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        record_count(&node0),
        N as usize,
        "node 0 holds the N originals"
    );
    assert_eq!(record_count(&node1), N as usize, "node 1 holds N replicas");

    // Counts must be stable: any re-replication loop would keep growing them.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        record_count(&node0),
        N as usize,
        "node 0 count unchanged after a further 500 ms"
    );
    assert_eq!(
        record_count(&node1),
        N as usize,
        "node 1 count unchanged after a further 500 ms"
    );

    chain0.shutdown().await;
    chain1.shutdown().await;
    node0.close().await.unwrap();
    node1.close().await.unwrap();
}

/// A test transport that delivers only the first `released` appends and
/// stalls on the rest, so the follower watermark stops while the primary
/// keeps committing. An atomic counter (not a `Notify`) is the release
/// channel: permits can never be missed, and a stalled in-flight append is
/// released by the replicator's shutdown race.
struct StallingTransport {
    released: std::sync::atomic::AtomicUsize,
    appended: std::sync::atomic::AtomicUsize,
    watermark: std::sync::Mutex<Option<Lsn>>,
}

impl StallingTransport {
    fn new() -> Self {
        Self {
            released: std::sync::atomic::AtomicUsize::new(0),
            appended: std::sync::atomic::AtomicUsize::new(0),
            watermark: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl ChainTransport for StallingTransport {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        let n = self
            .appended
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        // Stall until the test has released this delivery: the primary's
        // committed cursor advances while the follower watermark stays put.
        while n >= self.released.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let mut wm = self.watermark.lock().expect("stall wm");
        *wm = Some(record.lsn);
        Ok(record.lsn)
    }

    async fn follower_watermark(&self) -> Option<Lsn> {
        *self.watermark.lock().expect("stall wm")
    }
}

#[tokio::test]
async fn lag_alarm_fires_above_threshold() {
    let primary_dir = tempfile::tempdir().unwrap();
    let primary = Arc::new(Journal::open(&journal_config(primary_dir.path())).unwrap());

    let transport = Arc::new(StallingTransport::new());
    // A lag alarm of 0 ms maps to a 0-byte budget: ANY committed-but-not-
    // durable record trips it.
    let replicator = orrery_persistd::spawn_chain(
        Arc::clone(&primary),
        transport.clone(),
        &ChainConfig {
            follower: 2,
            lag_alarm: Duration::from_millis(0),
            batch_max: 4,
        },
    );

    // Commit 8 records; the transport is fully stalled (released == 0), so
    // the follower watermark never moves while the primary's committed cursor
    // advances to the 8th record's LSN.
    for i in 0..8u64 {
        let rec = mk_record(CellId::ROOT, i, RecordKind::Spawn, &i.to_le_bytes());
        let handle = primary.append(rec).unwrap();
        handle.committed().await.unwrap();
    }
    // Let the replicator reach its stalled first append before releasing it:
    // it must be parked inside `StallingTransport::append` when the release
    // lands, otherwise an early delivery would observe a committed cursor
    // equal to the watermark (lag 0).
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Release ONE delivery: the replicator's first batch pushes up to
    // batch_max records; the first completes (watermark advances to record
    // 0's LSN), the second stalls forever. Lag bookkeeping runs per record,
    // so the gauge now reads committed(8) - watermark(1) > 0 = lag_alarm.
    transport
        .released
        .store(1, std::sync::atomic::Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if replicator.lag_bytes() > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "replicator never reported lag above lag_alarm"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Sanity: the exposed lag equals primary.committed() - follower watermark.
    let committed = primary.committed();
    let wm = replicator.follower_watermark().expect("watermark known");
    assert_eq!(wm.segment, committed.segment);
    assert_eq!(
        replicator.lag_bytes(),
        committed.offset - wm.offset,
        "exposed lag is primary.committed() - follower watermark"
    );

    // The second append of the batch is stalled forever; shutdown races it.
    replicator.shutdown().await;
    primary.close().await.unwrap();
}
