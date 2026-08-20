//! Store-level comparison for the P2 journal's durability barrier
//! (docs/08-persistence.md §4.8).
//!
//! §4.7 traced P2's `journal_commit_ms` tail to fjall 3.1.9's write
//! backpressure — `Batch::commit` calls `local_backpressure()`, which sleeps in
//! 100 ms steps while four or more sealed memtables are queued — and showed it
//! survives every storage change, including running the journal on tmpfs. The
//! open question that leaves is whether the pathology is *fjall's* or *an
//! LSM's*, and that is a question about a second store.
//!
//! This rig answers it by driving two stores through the same write pattern the
//! journal produces and reporting the same statistics for both. It is
//! deliberately **not** a second `Journal`: `orrery_persistd::journal::Journal`
//! is concrete on fjall and reimplementing it against RocksDB would put a
//! thousand lines of untested code between the question and the answer. What
//! the journal actually asks of a store is narrow — batch N keyed records,
//! commit the batch with one WAL fsync, measure that call — and that is what is
//! reproduced here, identically for both arms.
//!
//! ```sh
//! p2-journal-bench --store fjall   --dir /mnt/nvme/bench --seconds 60
//! p2-journal-bench --store rocksdb --dir /mnt/nvme/bench --seconds 60   # needs --features rocksdb-store
//! ```
//!
//! **What is comparable, and what is not.** Both arms see the same arrival
//! process, the same coalescing window and caps, the same key ordering
//! (monotonic big-endian LSNs, which is what the journal produces and what an
//! LSM's compaction behaviour is most sensitive to), the same value sizes, and
//! the same two column families. Both are asked for a WAL write plus fsync per
//! batch: fjall's `PersistMode::SyncData` and RocksDB's `WriteOptions::set_sync(true)`.
//! Neither arm is tuned. That last point cuts both ways and is stated in the
//! output: a default RocksDB is not a tuned RocksDB, and this rig measures the
//! stall behaviour of stock configurations, not the best either store can do.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Ten times the D16 `journal_commit_ms` budget, matching
/// `orrery_persistd::journal::SLOW_SYNC_THRESHOLD_US` so the two rigs' "slow
/// barrier" counts mean the same thing.
const SLOW_BARRIER_US: u64 = 20_000;

/// Latency histogram bounds, in microseconds. Coarse on purpose: the question
/// is the far tail, and the percentiles are computed from raw samples anyway.
const CDF_MS: [u64; 8] = [1, 2, 5, 10, 20, 50, 100, 200];

#[derive(Debug, Clone)]
struct Args {
    store: String,
    dir: std::path::PathBuf,
    seconds: u64,
    bursts_per_sec: u64,
    burst: usize,
    value_len: usize,
    batch_window_us: u64,
    batch_max_records: usize,
    batch_max_bytes: usize,
    json: bool,
    /// Diagnostic control only. Turning the fsync off must make the barrier
    /// dramatically cheaper on a real device; if it does not, the "durable"
    /// arm was never syncing and the comparison is void.
    no_sync: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            store: "fjall".into(),
            dir: std::path::PathBuf::from("bench-data"),
            seconds: 60,
            // The P2 gate's bulk shape: 125 sessions at 2 Hz, so ~250 bursts/s
            // of ~71 records, ~17.7k records/s. Identical to
            // `crates/orrery_persistd/tests/journal_arrival_rate.rs`.
            bursts_per_sec: 250,
            burst: 71,
            // ~140-170 B per record is what the gate and the rig both measure
            // (5.05 KB / 37 records and 3.3 KB / 20 records respectively).
            value_len: 152,
            // persistd's production window. The journal's own `Default` is
            // zero; every `Journal` persistd opens overrides it to 200 us.
            batch_window_us: 200,
            batch_max_records: 8192,
            batch_max_bytes: 1 << 20,
            json: false,
            no_sync: false,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--store" => a.store = val()?,
            "--dir" => a.dir = val()?.into(),
            "--seconds" => a.seconds = val()?.parse().map_err(|e| format!("{e}"))?,
            "--bursts" => a.bursts_per_sec = val()?.parse().map_err(|e| format!("{e}"))?,
            "--burst-size" => a.burst = val()?.parse().map_err(|e| format!("{e}"))?,
            "--value-bytes" => a.value_len = val()?.parse().map_err(|e| format!("{e}"))?,
            "--batch-window-us" => {
                a.batch_window_us = val()?.parse().map_err(|e| format!("{e}"))?
            }
            "--json" => a.json = true,
            "--no-sync" => a.no_sync = true,
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

/// One durable batch commit, timed. This is the whole store interface the P2
/// journal needs, and therefore the whole surface this rig compares.
trait Store: Send {
    /// Insert every `(key, value)` and make the batch durable with one WAL
    /// fsync. Returns nothing: the caller times the call.
    fn commit(&mut self, batch: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String>;
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// fjall — configured exactly as `orrery_persistd::journal::fjall` configures it
// ---------------------------------------------------------------------------
#[cfg(feature = "fjall-store")]
mod fjall_store {
    use super::Store;

    pub struct FjallStore {
        db: fjall::Database,
        records: fjall::Keyspace,
        originated: fjall::Keyspace,
        sync: bool,
    }

    impl FjallStore {
        pub fn open(dir: &std::path::Path, sync: bool) -> Result<Self, String> {
            // `manual_journal_persist(true)` and one `SyncData` per batch is
            // what the journal does; anything else would measure a different
            // durability contract.
            let db = fjall::Database::builder(dir)
                .manual_journal_persist(true)
                .open()
                .map_err(|e| format!("open fjall: {e}"))?;
            let records = db
                .keyspace("records", fjall::KeyspaceCreateOptions::default)
                .map_err(|e| format!("open records: {e}"))?;
            let originated = db
                .keyspace("originated", fjall::KeyspaceCreateOptions::default)
                .map_err(|e| format!("open originated: {e}"))?;
            Ok(Self {
                db,
                records,
                originated,
                sync,
            })
        }
    }

    impl Store for FjallStore {
        fn commit(&mut self, batch: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
            let mode = if self.sync {
                fjall::PersistMode::SyncData
            } else {
                fjall::PersistMode::Buffer
            };
            let mut b = self.db.batch().durability(Some(mode));
            for (k, v) in batch {
                b.insert(&self.records, k, v);
                // The journal also writes a presence marker per record into a
                // second keyspace; keeping it makes the two arms write the same
                // number of column families.
                b.insert(&self.originated, k, b"");
            }
            b.commit().map_err(|e| format!("fjall commit: {e}"))
        }
        fn name(&self) -> &'static str {
            "fjall"
        }
    }
}

// ---------------------------------------------------------------------------
// RocksDB — stock configuration, WAL fsync per batch
// ---------------------------------------------------------------------------
#[cfg(feature = "rocksdb-store")]
mod rocksdb_store {
    use super::Store;
    use rocksdb::{ColumnFamilyDescriptor, DB, Options, WriteBatch, WriteOptions};

    pub struct RocksStore {
        db: DB,
        write: WriteOptions,
    }

    impl RocksStore {
        pub fn open(dir: &std::path::Path, sync: bool) -> Result<Self, String> {
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.create_missing_column_families(true);
            let cfs = vec![
                ColumnFamilyDescriptor::new("records", Options::default()),
                ColumnFamilyDescriptor::new("originated", Options::default()),
            ];
            let db = DB::open_cf_descriptors(&opts, dir, cfs)
                .map_err(|e| format!("open rocksdb: {e}"))?;
            let mut write = WriteOptions::default();
            // The equivalent of fjall's `PersistMode::SyncData`: the WAL write
            // is fsynced before the call returns. Without this the two arms
            // would not be measuring the same durability contract at all.
            write.set_sync(sync);
            Ok(Self { db, write })
        }
    }

    impl Store for RocksStore {
        fn commit(&mut self, batch: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
            let records = self.db.cf_handle("records").ok_or("no records cf")?;
            let originated = self.db.cf_handle("originated").ok_or("no originated cf")?;
            let mut wb = WriteBatch::default();
            for (k, v) in batch {
                wb.put_cf(&records, k, v);
                wb.put_cf(&originated, k, b"");
            }
            self.db
                .write_opt(wb, &self.write)
                .map_err(|e| format!("rocksdb write: {e}"))
        }
        fn name(&self) -> &'static str {
            "rocksdb"
        }
    }
}

// ---------------------------------------------------------------------------
// wal-db — a pure WAL, which is the shape the journal actually is
// ---------------------------------------------------------------------------
//
// NOT an apples-to-apples store: a WAL keeps no keyed index, so this arm does
// strictly less work than the two LSMs and its numbers are a **lower bound**,
// not a like-for-like result. It is here because the question §4.8 asks is
// narrow — does the durability path stall? — and because `journal-raw`
// (docs/08 §4, planned: raw segment files with a sparse footer index) is
// exactly this shape. What an adopted wal-db would still owe the journal is
// the index layer, and that is not measured here.
#[cfg(feature = "waldb-store")]
mod waldb_store {
    use super::Store;

    pub struct WalStore {
        wal: wal_db::Wal,
        sync: bool,
    }

    impl WalStore {
        pub fn open(dir: &std::path::Path, sync: bool) -> Result<Self, String> {
            let wal = wal_db::Wal::open(dir.join("wal")).map_err(|e| format!("open wal: {e}"))?;
            Ok(Self { wal, sync })
        }
    }

    impl Store for WalStore {
        fn commit(&mut self, batch: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
            // The journal's contract: stage every record, then one durability
            // barrier for the batch. `append` returns once the record is in the
            // page cache; `sync` is the barrier the caller times.
            let mut framed = Vec::with_capacity(64);
            for (k, v) in batch {
                framed.clear();
                framed.extend_from_slice(k);
                framed.extend_from_slice(v);
                let _lsn = self
                    .wal
                    .append(&framed)
                    .map_err(|e| format!("wal append: {e}"))?;
            }
            if self.sync {
                self.wal.sync().map_err(|e| format!("wal sync: {e}"))?;
            }
            Ok(())
        }
        fn name(&self) -> &'static str {
            "wal-db"
        }
    }
}

/// A record waiting for its group commit.
struct Pending {
    key: Vec<u8>,
    value: Vec<u8>,
    queued: Instant,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Pending>,
    closed: bool,
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() as f64) * p).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)] as f64 / 1000.0
}

/// Bytes actually on disk, so an arm that wrote less than the other cannot
/// quietly look faster for that reason.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(m) if m.is_dir() => total += dir_bytes(&entry.path()),
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e != "help" {
                eprintln!("error: {e}");
            }
            eprintln!(
                "usage: p2-journal-bench --store fjall|rocksdb|wal-db --dir DIR \
                 [--seconds N] [--bursts N] [--burst-size N] [--value-bytes N] \
                 [--batch-window-us N] [--json] [--no-sync]"
            );
            std::process::exit(if e == "help" { 0 } else { 2 });
        }
    };

    std::fs::create_dir_all(&args.dir).expect("create dir");
    let mut store: Box<dyn Store> = match args.store.as_str() {
        #[cfg(feature = "fjall-store")]
        "fjall" => {
            Box::new(fjall_store::FjallStore::open(&args.dir, !args.no_sync).expect("open fjall"))
        }
        #[cfg(feature = "rocksdb-store")]
        "rocksdb" => Box::new(
            rocksdb_store::RocksStore::open(&args.dir, !args.no_sync).expect("open rocksdb"),
        ),
        #[cfg(feature = "waldb-store")]
        "wal-db" | "waldb" => {
            Box::new(waldb_store::WalStore::open(&args.dir, !args.no_sync).expect("open wal-db"))
        }
        other => {
            eprintln!(
                "store `{other}` is not compiled in; rebuild with the matching feature \
                 (fjall-store / rocksdb-store / waldb-store)"
            );
            std::process::exit(2);
        }
    };
    let store_name = store.name();

    let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
    let latencies = Arc::new(Mutex::new(Vec::<u64>::with_capacity(2_000_000)));
    let flushes = Arc::new(AtomicU64::new(0));
    let records_done = Arc::new(AtomicU64::new(0));
    let bytes_done = Arc::new(AtomicU64::new(0));
    let sync_us_sum = Arc::new(AtomicU64::new(0));
    let sync_us_max = Arc::new(AtomicU64::new(0));
    let slow_barriers = Arc::new(AtomicU64::new(0));
    let worst_bytes = Arc::new(AtomicU64::new(0));
    let worst_records = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));

    // ---- the committer: coalesce, then one durable barrier ---------------
    let committer = {
        let queue = Arc::clone(&queue);
        let latencies = Arc::clone(&latencies);
        let (flushes, records_done, bytes_done) = (
            Arc::clone(&flushes),
            Arc::clone(&records_done),
            Arc::clone(&bytes_done),
        );
        let (sync_us_sum, sync_us_max) = (Arc::clone(&sync_us_sum), Arc::clone(&sync_us_max));
        let (slow_barriers, worst_bytes, worst_records) = (
            Arc::clone(&slow_barriers),
            Arc::clone(&worst_bytes),
            Arc::clone(&worst_records),
        );
        let window = Duration::from_micros(args.batch_window_us);
        let (max_records, max_bytes) = (args.batch_max_records, args.batch_max_bytes);
        std::thread::spawn(move || {
            let (lock, cvar) = &*queue;
            loop {
                let mut guard = lock.lock().expect("queue lock");
                while guard.pending.is_empty() && !guard.closed {
                    guard = cvar.wait(guard).expect("queue wait");
                }
                if guard.pending.is_empty() && guard.closed {
                    return;
                }
                // The batch window, measured from the first arrival exactly as
                // `journal::group_commit` measures it.
                if !window.is_zero() {
                    drop(guard);
                    std::thread::sleep(window);
                    guard = lock.lock().expect("queue lock");
                }
                let mut batch = Vec::new();
                let mut batch_bytes = 0usize;
                while let Some(p) = guard.pending.front() {
                    let size = p.key.len() + p.value.len();
                    if !batch.is_empty()
                        && (batch.len() >= max_records || batch_bytes + size >= max_bytes)
                    {
                        break;
                    }
                    batch_bytes += size;
                    batch.push(guard.pending.pop_front().expect("front"));
                }
                drop(guard);
                if batch.is_empty() {
                    continue;
                }

                let pairs: Vec<(Vec<u8>, Vec<u8>)> = batch
                    .iter()
                    .map(|p| (p.key.clone(), p.value.clone()))
                    .collect();
                let started = Instant::now();
                store.commit(&pairs).expect("commit");
                let sync = started.elapsed();
                let sync_us = u64::try_from(sync.as_micros()).unwrap_or(u64::MAX);
                let done = Instant::now();

                flushes.fetch_add(1, Ordering::Relaxed);
                records_done.fetch_add(batch.len() as u64, Ordering::Relaxed);
                bytes_done.fetch_add(batch_bytes as u64, Ordering::Relaxed);
                sync_us_sum.fetch_add(sync_us, Ordering::Relaxed);
                // The worst barrier's shape travels with its cost, the same
                // pairing `JournalStageSnapshot` makes (docs/08 §4.7): a slow
                // barrier carrying an ordinary batch is the store's own doing.
                let mut observed = sync_us_max.load(Ordering::Relaxed);
                while sync_us > observed {
                    match sync_us_max.compare_exchange_weak(
                        observed,
                        sync_us,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            worst_bytes.store(batch_bytes as u64, Ordering::Relaxed);
                            worst_records.store(batch.len() as u64, Ordering::Relaxed);
                            break;
                        }
                        Err(cur) => observed = cur,
                    }
                }
                if sync_us >= SLOW_BARRIER_US {
                    slow_barriers.fetch_add(1, Ordering::Relaxed);
                }

                let mut lat = latencies.lock().expect("lat lock");
                for p in &batch {
                    lat.push(u64::try_from(done.duration_since(p.queued).as_micros()).unwrap_or(0));
                }
            }
        })
    };

    // ---- open-loop arrivals: a wall clock, not a feedback loop -----------
    let value = vec![0xABu8; args.value_len];
    let period = Duration::from_nanos(1_000_000_000 / args.bursts_per_sec.max(1));
    let total_bursts = args.seconds * args.bursts_per_sec;
    let started = Instant::now();
    let mut lsn: u64 = 0;
    for b in 0..total_bursts {
        let due = started + period * u32::try_from(b).unwrap_or(u32::MAX);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let queued = Instant::now();
        {
            let (lock, cvar) = &*queue;
            let mut guard = lock.lock().expect("queue lock");
            for _ in 0..args.burst {
                lsn += 1;
                guard.pending.push_back(Pending {
                    // Monotonic big-endian keys: what the journal's LSN
                    // ordering produces, and the ordering an LSM's compaction
                    // is most sensitive to.
                    key: lsn.to_be_bytes().to_vec(),
                    value: value.clone(),
                    queued,
                });
            }
            cvar.notify_one();
        }
    }
    {
        let (lock, cvar) = &*queue;
        lock.lock().expect("queue lock").closed = true;
        cvar.notify_all();
    }
    committer.join().expect("committer");
    running.store(false, Ordering::Relaxed);
    let wall = started.elapsed();

    // ---- report ----------------------------------------------------------
    let mut lat = latencies.lock().expect("lat lock").clone();
    lat.sort_unstable();
    let n = lat.len() as u64;
    let f = flushes.load(Ordering::Relaxed).max(1);
    let bytes = bytes_done.load(Ordering::Relaxed);
    let worst_ms = sync_us_max.load(Ordering::Relaxed) as f64 / 1000.0;
    let worst_kb = worst_bytes.load(Ordering::Relaxed) as f64 / 1024.0;
    let ordinary_kb = bytes as f64 / f as f64 / 1024.0;
    let under = |ms: u64| {
        let bound = ms * 1000;
        lat.partition_point(|v| *v <= bound) as f64 * 100.0 / n.max(1) as f64
    };

    if args.json {
        let cdf: Vec<String> = CDF_MS
            .iter()
            .map(|ms| format!("\"{ms}\":{:.4}", under(*ms)))
            .collect();
        println!(
            "{{\"store\":\"{store_name}\",\"records\":{n},\"wall_s\":{:.2},\
             \"rate_per_s\":{:.0},\"flushes\":{},\"flushes_per_s\":{:.1},\
             \"records_per_flush\":{:.2},\"kb_per_flush\":{ordinary_kb:.3},\
             \"p50_ms\":{:.3},\"p90_ms\":{:.3},\"p99_ms\":{:.3},\"p99_9_ms\":{:.3},\
             \"p99_99_ms\":{:.3},\"max_ms\":{:.3},\"sync_us_per_flush\":{:.1},\
             \"slow_barriers\":{},\"slow_threshold_ms\":{},\"worst_ms\":{worst_ms:.3},\
             \"worst_kb\":{worst_kb:.3},\"worst_records\":{},\"cdf_pct\":{{{}}}}}",
            wall.as_secs_f64(),
            n as f64 / wall.as_secs_f64(),
            f,
            f as f64 / wall.as_secs_f64(),
            records_done.load(Ordering::Relaxed) as f64 / f as f64,
            percentile(&lat, 0.50),
            percentile(&lat, 0.90),
            percentile(&lat, 0.99),
            percentile(&lat, 0.999),
            percentile(&lat, 0.9999),
            percentile(&lat, 1.0),
            sync_us_sum.load(Ordering::Relaxed) as f64 / f as f64,
            slow_barriers.load(Ordering::Relaxed),
            SLOW_BARRIER_US / 1000,
            worst_records.load(Ordering::Relaxed),
            cdf.join(","),
        );
        return;
    }

    println!("== p2-journal-bench: {store_name} ==");
    println!(
        "n={n} wall={:.2}s rate={:.0} rec/s flushes={f} ({:.0}/s, {:.1} rec/flush, {ordinary_kb:.2} KB/flush)",
        wall.as_secs_f64(),
        n as f64 / wall.as_secs_f64(),
        f as f64 / wall.as_secs_f64(),
        records_done.load(Ordering::Relaxed) as f64 / f as f64,
    );
    println!(
        "commit_ms p50={:.3} p90={:.3} p99={:.3} p99.9={:.3} p99.99={:.3} max={:.3}",
        percentile(&lat, 0.50),
        percentile(&lat, 0.90),
        percentile(&lat, 0.99),
        percentile(&lat, 0.999),
        percentile(&lat, 0.9999),
        percentile(&lat, 1.0),
    );
    let cdf: Vec<String> = CDF_MS
        .iter()
        .map(|ms| format!("<={ms}ms {:.1}%", under(*ms)))
        .collect();
    println!("cdf {}", cdf.join(" | "));
    println!(
        "barrier: mean {:.1} us/flush | slow (>= {} ms) {} of {f} | worst {worst_ms:.1} ms carrying \
         {worst_kb:.2} KB / {} records against {ordinary_kb:.2} KB for an ordinary flush",
        sync_us_sum.load(Ordering::Relaxed) as f64 / f as f64,
        SLOW_BARRIER_US / 1000,
        slow_barriers.load(Ordering::Relaxed),
        worst_records.load(Ordering::Relaxed),
    );
    println!(
        "on disk: {:.1} MB in {} (durability={})",
        dir_bytes(&args.dir) as f64 / 1e6,
        args.dir.display(),
        if args.no_sync {
            "BUFFERED — control only"
        } else {
            "fsync per batch"
        },
    );
    println!(
        "note: neither store is tuned. This measures stock configurations under the journal's \
         write pattern, not the best either can be made to do."
    );
}
