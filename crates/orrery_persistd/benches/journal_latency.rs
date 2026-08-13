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
//!
//! Options (env `ORRERY_JOURNAL_DIR` is the fallback for `--dir`):
//! ```text
//! --dir <PATH>          journal backing directory (default: a tempdir —
//!                       tmpfs on most hosts, i.e. RAM, so the default
//!                       measures memcpy not disk; point this at the target
//!                       device for numbers that mean anything)
//! --concurrency <N>     appends kept in flight at once (default 1, i.e.
//!                       sequential request/response)
//! ```

use std::path::PathBuf;
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

/// Command-line overrides. Parsed by hand (the bench harness is `false`, so
/// there is no clap derive here — the workspace pins clap for the real bins).
struct Args {
    dir: Option<PathBuf>,
    concurrency: usize,
}

impl Args {
    fn parse() -> Self {
        let mut dir = std::env::var_os("ORRERY_JOURNAL_DIR").map(PathBuf::from);
        let mut concurrency = 1usize;
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--dir" => {
                    let v = it.next().expect("--dir requires a value");
                    dir = Some(PathBuf::from(v));
                }
                "--concurrency" => {
                    let v = it.next().expect("--concurrency requires a value");
                    concurrency = v.parse().expect("--concurrency must be a positive integer");
                    assert!(concurrency > 0, "--concurrency must be > 0");
                }
                other => {
                    if let Some(v) = other.strip_prefix("--dir=") {
                        dir = Some(PathBuf::from(v));
                    } else if let Some(v) = other.strip_prefix("--concurrency=") {
                        concurrency = v.parse().expect("--concurrency must be a positive integer");
                        assert!(concurrency > 0, "--concurrency must be > 0");
                    }
                }
            }
        }
        Self { dir, concurrency }
    }
}

fn run(mode: AdaptiveCommitMode, n: usize, payload: &[u8], args: &Args) {
    // A `--dir`/`ORRERY_JOURNAL_DIR` override measures a real device; the
    // default tempdir is tmpfs on most hosts, so published numbers from the
    // default are RAM speeds. A run dir is reused in place so repeated runs
    // hit the same filesystem; it is cleared first.
    let _tempdir; // keeps the default tempdir alive for the run
    let dir = match &args.dir {
        Some(d) => {
            let sub = d.join(format!("journal-latency-{mode:?}"));
            let _ = std::fs::remove_dir_all(&sub);
            std::fs::create_dir_all(&sub).expect("create bench dir");
            _tempdir = None;
            sub
        }
        None => {
            let t = tempfile::tempdir().unwrap();
            let p = t.path().to_path_buf();
            _tempdir = Some(t);
            p
        }
    };
    let cfg = JournalConfig {
        dir,
        commit: GroupCommitConfig {
            mode,
            batch_window: Duration::from_micros(500),
            batch_max_records: 8192,
            batch_max_bytes: 1 << 20,
        },
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let journal = std::sync::Arc::new(rt.block_on(async { Journal::open(&cfg).unwrap() }));

    let mut latencies = Vec::with_capacity(n);
    let start = Instant::now();
    rt.block_on(async {
        // Keep `concurrency` appends in flight: at 1 this is the old
        // sequential request/response loop; above 1 the group committer's
        // batching is actually exercised (the whole point of D16's group
        // commit is that N in-flight appends share one fsync). Handles are
        // FIFO: appends commit in LSN order, so draining from the front is
        // never blocked behind a later batch.
        let mut in_flight: std::collections::VecDeque<(
            Instant,
            std::sync::Arc<orrery_persistd::AppendHandle>,
        )> = std::collections::VecDeque::new();
        let mut submitted = 0usize;
        let mut completed = 0usize;
        while completed < n {
            while submitted < n && in_flight.len() < args.concurrency {
                let rec = mk_record(submitted as u64, payload);
                let handle = journal.append(rec).unwrap();
                in_flight.push_back((Instant::now(), handle));
                submitted += 1;
            }
            let (t0, handle) = in_flight.pop_front().expect("in-flight non-empty");
            handle.committed().await.unwrap();
            latencies.push(t0.elapsed());
            completed += 1;
        }
    });
    let total = start.elapsed();
    rt.block_on(journal.close()).unwrap();

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 0.50);
    let p99 = percentile(&latencies, 0.99);
    let max = latencies.last().copied().unwrap_or_default();
    let rate = n as f64 / total.as_secs_f64();

    println!(
        "mode={mode:?} n={n} payload={}B concurrency={}  total={total:?}  p50={p50:?}  p99={p99:?}  max={max:?}  rate={rate:.0} rec/s",
        payload.len(),
        args.concurrency,
    );
}

fn main() {
    let args = Args::parse();
    let payload = vec![0u8; 64];
    println!("== journal-commit latency (server-internal, D16 target < 2 ms p99) ==");
    run(AdaptiveCommitMode::Adaptive, 10_000, &payload, &args);
    run(AdaptiveCommitMode::AlwaysBatch, 10_000, &payload, &args);
    run(AdaptiveCommitMode::AlwaysIdle, 1_000, &payload, &args);
}
