//! Open-loop journal-commit rig (D16): a fixed *arrival rate* with bursty
//! submission, matching the P2 gate's bulk shape (125 sessions at 2 Hz, so
//! ~250 bursts/s of ~71 records each, ~17.7 k records/s).
//!
//! The closed-loop bench (`benches/journal_latency.rs`) can never show the D16
//! tail: it never offers more work than the committer drains, so the queue can
//! not build. This rig submits on a wall clock regardless of what the committer
//! is doing, which is what the gate does.
//!
//! Run it explicitly (it is `#[ignore]`d — it is a measurement, not an
//! assertion):
//! ```sh
//! ORRERY_JOURNAL_DIR=/some/disk cargo test -p orrery_persistd --release \
//!     --test journal_arrival_rate -- --ignored --nocapture
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orrery_persistd::journal::GroupCommitConfig;
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

fn pct(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() as f64) * p).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)] as f64 / 1000.0
}

#[test]
#[ignore = "measurement rig; run explicitly with --ignored --nocapture"]
fn open_loop_arrival_rate() {
    let seconds: u64 = std::env::var("RIG_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let bursts_per_sec: u64 = std::env::var("RIG_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250);
    let burst: usize = std::env::var("RIG_BURST_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(71);
    let payload_len: usize = std::env::var("RIG_PAYLOAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96);

    let _tempdir;
    let dir = match std::env::var_os("ORRERY_JOURNAL_DIR") {
        Some(d) => {
            let sub = std::path::PathBuf::from(d).join("journal-arrival-rig");
            let _ = std::fs::remove_dir_all(&sub);
            std::fs::create_dir_all(&sub).expect("create rig dir");
            _tempdir = None;
            sub
        }
        None => {
            let t = tempfile::tempdir().expect("tempdir");
            let p = t.path().to_path_buf();
            _tempdir = Some(t);
            p
        }
    };

    let cfg = JournalConfig {
        dir,
        commit: GroupCommitConfig::default(),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("runtime");
    let journal = Arc::new(Journal::open(&cfg).expect("open journal"));
    let payload = vec![7u8; payload_len];

    let latencies: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(
        (seconds * bursts_per_sec) as usize * burst,
    )));
    let submitted = Arc::new(AtomicUsize::new(0));

    // Per-second stage trace. The gate's committer does not degrade at a
    // constant rate: it runs at one service time for the first ~22 s and at
    // another, four to twelve times worse, after that. A run summary averages
    // the two regimes together and hides the transition entirely, which is how
    // a 20 s rig and a 30 s gate can disagree by an order of magnitude on the
    // same code. Print the same per-flush quantities `persistd`'s reporter
    // writes, once a second, so the onset is visible rather than inferred.
    let trace = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let tracer = {
        let journal = Arc::clone(&journal);
        let trace = Arc::clone(&trace);
        std::thread::spawn(move || {
            let mut cursor = journal.commit_metrics().stage_snapshot();
            let mut second = 0u64;
            println!("sec flushes  records sync_us/flush  qwait_us/flush  rec/flush");
            while trace.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let d = journal.commit_metrics().stage_delta(&mut cursor);
                // Denominator: `record_group` is called once per flush, so a
                // stage sum divides by `flushes`, never by `records`.
                let f = d.flushes.max(1) as f64;
                println!(
                    "{second:3} {:7} {:8} {:13.0} {:15.0} {:10.1}",
                    d.flushes,
                    d.records,
                    d.sync_data_us_sum as f64 / f,
                    d.queue_wait_us_sum as f64 / f,
                    d.records as f64 / f,
                );
                second += 1;
            }
        })
    };

    let period = Duration::from_nanos(1_000_000_000 / bursts_per_sec);
    let total_bursts = seconds * bursts_per_sec;
    let started = Instant::now();

    rt.block_on(async {
        let mut tasks = Vec::new();
        for b in 0..total_bursts {
            let due = started + period * u32::try_from(b).unwrap_or(u32::MAX);
            let now = Instant::now();
            if due > now {
                tokio::time::sleep(due - now).await;
            }
            for i in 0..burst {
                let j = Arc::clone(&journal);
                let lat = Arc::clone(&latencies);
                let sub = Arc::clone(&submitted);
                let entity = b * burst as u64 + i as u64;
                let payload = payload.clone();
                tasks.push(tokio::spawn(async move {
                    let t0 = Instant::now();
                    let handle = j.append(mk_record(entity, &payload)).expect("append");
                    sub.fetch_add(1, Ordering::Relaxed);
                    handle.committed().await.expect("commit");
                    let us = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
                    lat.lock().expect("lat lock").push(us);
                }));
            }
        }
        for t in tasks {
            t.await.expect("task");
        }
    });
    let wall = started.elapsed();
    trace.store(false, Ordering::Relaxed);
    tracer.join().expect("stage tracer");
    let flushes = journal.flush_count();
    let stages = journal.commit_metrics().stage_snapshot();
    rt.block_on(journal.close()).expect("close");

    let mut v = latencies.lock().expect("lat lock").clone();
    v.sort_unstable();
    let n = v.len();
    let rate = n as f64 / wall.as_secs_f64();
    let under = |ms: u64| {
        let limit = ms * 1000;
        let c = v.partition_point(|&x| x <= limit);
        100.0 * c as f64 / n as f64
    };
    println!("== journal open-loop rig ==");
    println!(
        "n={n} wall={:.2}s rate={rate:.0} rec/s flushes={flushes} ({:.0}/s, {:.1} rec/flush)",
        wall.as_secs_f64(),
        flushes as f64 / wall.as_secs_f64(),
        n as f64 / flushes.max(1) as f64,
    );
    println!(
        "journal_commit_ms p50={:.3} p90={:.3} p99={:.3} p99.9={:.3} max={:.3}",
        pct(&v, 0.50),
        pct(&v, 0.90),
        pct(&v, 0.99),
        pct(&v, 0.999),
        pct(&v, 1.0)
    );
    println!(
        "cdf <=1ms {:.1}% | <=2ms {:.1}% | <=5ms {:.1}% | <=10ms {:.1}% | <=50ms {:.1}% | <=100ms {:.1}% | <=200ms {:.1}%",
        under(1),
        under(2),
        under(5),
        under(10),
        under(50),
        under(100),
        under(200)
    );
    println!(
        "per-flush stages (us): queue_wait_sum={} max={} sync_data_sum={} max={} resolve_sum={} max={} fjall_sum={}",
        stages.queue_wait_us_sum,
        stages.queue_wait_us_max,
        stages.sync_data_us_sum,
        stages.sync_data_us_max,
        stages.resolve_us_sum,
        stages.resolve_us_max,
        stages.fjall_batch_commit_us_sum,
    );
    println!(
        "per-flush means (us): queue_wait={:.1} sync_data={:.1} resolve={:.1}  => flush service {:.3} ms",
        stages.queue_wait_us_sum as f64 / stages.flushes.max(1) as f64,
        stages.sync_data_us_sum as f64 / stages.flushes.max(1) as f64,
        stages.resolve_us_sum as f64 / stages.flushes.max(1) as f64,
        (stages.sync_data_us_sum + stages.resolve_us_sum + stages.fjall_batch_commit_us_sum) as f64
            / stages.flushes.max(1) as f64
            / 1000.0,
    );
}
