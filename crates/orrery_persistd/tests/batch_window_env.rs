//! `ORRERY_JOURNAL_BATCH_WINDOW_US` actually reaches the committer
//! (docs/08-persistence.md §4.3).
//!
//! Its own test binary, and that is the point. The override is read from the
//! process environment, so setting it is a process-global write that races
//! every other test which opens a `Journal` — and those tests are precisely
//! the ones this variable reconfigures. A dedicated binary with exactly one
//! test has no one to race.
//!
//! The parse itself is unit-tested next to the function; what is proved here
//! is the *wiring*: that the value survives from the environment into the
//! running committer's `GroupCommitConfig`, past the caller's own window. The
//! P2 batch-window sweep depends on that end to end, and without this the
//! whole study reduces to six runs of the same configuration.

use std::sync::Arc;
use std::time::{Duration, Instant};

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{payload_crc, Journal, JournalConfig};
use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(entity: u64) -> JournalRecord {
    let payload = entity.to_le_bytes();
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: CellId::ROOT,
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::ComponentDiff,
        payload: bytes::Bytes::copy_from_slice(&payload),
        crc: payload_crc(&payload),
    }
}

#[tokio::test]
async fn env_override_widens_a_callers_batch_window() {
    // Deliberately a *non-default* caller window: `persistd` passes 200 us, so
    // an override that only filled in a defaulted field would be inert in the
    // one binary the P2 gate measures.
    let caller_window = Duration::from_micros(200);
    let overridden = Duration::from_millis(200);

    // SAFETY: single-test binary; nothing else in this process reads or writes
    // the environment concurrently, and the value is set before the journal
    // (and therefore the committer) exists.
    unsafe {
        std::env::set_var(
            "ORRERY_JOURNAL_BATCH_WINDOW_US",
            overridden.as_micros().to_string(),
        );
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        Journal::open(&JournalConfig {
            dir: dir.path().to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                batch_window: caller_window,
                ..GroupCommitConfig::default()
            },
        })
        .expect("open journal"),
    );

    let started = Instant::now();
    let mut waiters = Vec::new();
    for entity in 0..8u64 {
        let journal = Arc::clone(&journal);
        waiters.push(tokio::spawn(async move {
            let handle = journal.append(mk_record(entity)).expect("append");
            handle.committed().await.expect("commit");
        }));
    }
    for waiter in waiters {
        waiter.await.expect("waiter task");
    }
    let elapsed = started.elapsed();

    // The caller's 200 us would have resolved these in well under a
    // millisecond; the overridden 200 ms cannot. The margin is wide on purpose
    // -- this asserts *which window is in force*, not the timer's accuracy.
    assert!(
        elapsed >= Duration::from_millis(150),
        "the environment override did not reach the committer: 8 concurrent \
         appends resolved in {elapsed:?}, which is the caller's {caller_window:?} \
         window, not the overridden {overridden:?}"
    );
    assert_eq!(
        journal.flush_count(),
        1,
        "the overridden window must still form one group, not one fsync each"
    );

    journal.close().await.expect("close");
}
