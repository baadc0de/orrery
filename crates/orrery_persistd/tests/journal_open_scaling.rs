//! What it costs to open a journal, as a function of how much journal there is
//! (D19 *Consequences*: "Opening the journal rebuilds indexes in one forward
//! WAL scan. Startup work and index memory are therefore linear in retained
//! journal metadata and records. Segment retention and future persisted index
//! footers must be measured before treating arbitrarily old journals as free
//! to open.").
//!
//! This rig is the measurement that sentence asks for. It appends in equal
//! steps, closing and reopening the *same* journal between them, and reports
//! the open cost at each cumulative size. One journal grown in place, not N
//! journals built independently, because the question is what a long-lived
//! node pays at restart — and because rebuilding each size from scratch would
//! cost the sum of the sizes rather than the largest.
//!
//! It is `#[ignore]`d: it is a measurement, not an assertion. The assertion
//! that retention *bounds* this curve lives in `journal_retention.rs`, which
//! runs on every commit.
//!
//! ```sh
//! ORRERY_OPEN_SCALING_OUT=/tmp/open-scaling.jsonl \
//!   cargo test -p orrery_persistd --release --test journal_open_scaling \
//!   -- --ignored --nocapture
//! ```
//!
//! Knobs, all optional: `ORRERY_JOURNAL_DIR` (where the journal lives — put it
//! on the device you are asking about), `ORRERY_OPEN_SCALING_STEPS`,
//! `ORRERY_OPEN_SCALING_RECORDS_PER_STEP`, `ORRERY_OPEN_SCALING_PAYLOAD`.

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

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

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Total bytes of every file under `dir`, recursively.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_bytes(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// This process's resident set size in kilobytes, or 0 where unavailable.
///
/// Index memory is the other half of D19's sentence, and the index is the only
/// thing this rig holds that grows, so RSS after open is a serviceable proxy
/// for it. Linux-only by design — the number is reported, not asserted.
fn rss_kb() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement rig, not an assertion; see the module docs"]
async fn open_cost_scales_with_retained_journal() {
    let steps = env_usize("ORRERY_OPEN_SCALING_STEPS", 8);
    let per_step = env_usize("ORRERY_OPEN_SCALING_RECORDS_PER_STEP", 50_000);
    let payload_len = env_usize("ORRERY_OPEN_SCALING_PAYLOAD", 1_400);

    let scratch = tempfile::tempdir().expect("scratch dir");
    let dir = match std::env::var("ORRERY_JOURNAL_DIR") {
        Ok(d) => {
            let dir = std::path::PathBuf::from(d).join("open-scaling");
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }
        Err(_) => scratch.path().join("open-scaling"),
    };

    let config = JournalConfig {
        dir: dir.clone(),
        commit: GroupCommitConfig {
            // Always batch rather than adaptive: this rig measures *open*, and
            // a fill phase whose batching drifts with load makes the on-disk
            // shape it produces drift with it.
            mode: AdaptiveCommitMode::AlwaysBatch,
            batch_window: std::time::Duration::from_micros(200),
            ..GroupCommitConfig::default()
        },
    };

    let payload = vec![0x5au8; payload_len];
    let mut out = std::env::var("ORRERY_OPEN_SCALING_OUT")
        .ok()
        .map(|p| std::fs::File::create(p).expect("open scaling output"));

    let mut total_records = 0u64;
    for step in 1..=steps {
        // Fill: open, append `per_step` records, close.
        let journal = Journal::open(&config).expect("open journal for fill");
        let mut last = None;
        for i in 0..per_step {
            last = Some(
                journal
                    .append(mk_record((total_records + i as u64) % 4096, &payload))
                    .expect("append"),
            );
        }
        if let Some(handle) = last {
            handle.committed().await.expect("fill durable");
        }
        journal.close().await.expect("close after fill");
        drop(journal);
        total_records += per_step as u64;

        // Measure: the open that a restart pays at this retained size.
        let disk_bytes = dir_bytes(&dir);
        let before_rss = rss_kb();
        let started = Instant::now();
        let journal = Arc::new(Journal::open(&config).expect("open journal for measurement"));
        let open_ms = started.elapsed().as_secs_f64() * 1e3;
        let after_rss = rss_kb();
        // The full replay a recovery pays when its checkpoint watermark is
        // older than the whole retained journal — the worst case the index
        // rebuild above is only the first half of.
        let scan_started = Instant::now();
        let mut scanned = 0u64;
        for record in journal.scan_from(Lsn::new(0, 0)) {
            record.expect("scan record");
            scanned += 1;
        }
        let scan_ms = scan_started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(
            scanned, total_records,
            "the rebuilt index must see every record it retains"
        );
        journal.close().await.expect("close after measurement");
        drop(journal);

        let line = format!(
            "{{\"step\":{step},\"records\":{total_records},\"disk_bytes\":{disk_bytes},\
             \"open_ms\":{open_ms:.3},\"scan_ms\":{scan_ms:.3},\
             \"rss_kb_after_open\":{after_rss},\
             \"rss_kb_delta\":{},\"payload_bytes\":{payload_len}}}",
            after_rss.saturating_sub(before_rss)
        );
        println!("{line}");
        if let Some(file) = out.as_mut() {
            writeln!(file, "{line}").expect("write scaling line");
        }
    }
}
