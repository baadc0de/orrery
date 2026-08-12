//! Journal-commit latency rig (slice 3, D16).
//!
//! Measures the **server-internal journal commit** latency (actor append →
//! group fsync → ack) under synthetic load, and reports p50/p99. This is a
//! measure-only harness: it asserts nothing, because CI machines vary and the
//! D16 targets (`journal commit < 2 ms`, `client ack p99 < 5 ms`) are validated
//! by the real latency rig in a controlled environment, not a flaky unit test.
//!
//! Run with:
//! ```sh
//! cargo bench -p orrery_persistd --bench journal_latency
//! ```

use std::time::{Duration, Instant};

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{payload_crc, Journal, JournalConfig};

use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(entity: u64, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: CellId::ROOT,
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::ComponentDiff,
        payload: bytes::Bytes::copy_from_slice(payload),
        crc: payload_crc(payload),
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize - 1;
    sorted[idx.min(sorted.len() - 1)]
}

fn run(mode: AdaptiveCommitMode, n: usize, payload: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = JournalConfig {
        dir: dir.path().to_path_buf(),
        commit: GroupCommitConfig {
            mode,
            batch_window: Duration::from_micros(500),
            batch_max_records: 8192,
            batch_max_bytes: 1 << 20,
        },
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let journal = rt.block_on(async { Journal::open(&cfg).unwrap() });

    let mut latencies = Vec::with_capacity(n);
    let start = Instant::now();
    rt.block_on(async {
        for i in 0..n {
            let rec = mk_record(i as u64, payload);
            let handle = journal.append(rec).unwrap();
            let t0 = Instant::now();
            handle.committed().await.unwrap();
            latencies.push(t0.elapsed());
        }
    });
    let total = start.elapsed();

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 0.50);
    let p99 = percentile(&latencies, 0.99);
    let max = latencies.last().copied().unwrap_or_default();
    let rate = n as f64 / total.as_secs_f64();

    println!(
        "mode={mode:?} n={n} payload={}B  total={total:?}  p50={p50:?}  p99={p99:?}  max={max:?}  rate={rate:.0} rec/s",
        payload.len()
    );
}

fn main() {
    let payload = vec![0u8; 64];
    println!("== journal-commit latency (server-internal, D16 target < 2 ms p99) ==");
    run(AdaptiveCommitMode::Adaptive, 10_000, &payload);
    run(AdaptiveCommitMode::AlwaysBatch, 10_000, &payload);
    run(AdaptiveCommitMode::AlwaysIdle, 1_000, &payload);
}
