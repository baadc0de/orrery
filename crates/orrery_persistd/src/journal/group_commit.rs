//! Adaptive group commit (docs/08-persistence.md §4, D16).
//!
//! Appends from all actors on a node accumulate in a shared queue; a single
//! committer task issues the durable `fdatasync` and resolves **every** waiter
//! in the batch on that one fsync. Two regimes:
//!
//! - **Adaptive (default):** a lone append arriving while the disk is idle is
//!   flushed immediately (a lone record pays only device latency); under load,
//!   appends batch for [`GroupCommitConfig::batch_window`] (default 0.5 ms) or
//!   until a size cap, then flush once.
//! - The other [`AdaptiveCommitMode`]s exist to make batching deterministic in
//!   tests ([`AlwaysBatch`](AdaptiveCommitMode::AlwaysBatch) forces the window
//!   path even for a single record) and to measure worst case
//!   ([`AlwaysIdle`](AdaptiveCommitMode::AlwaysIdle) flushes per record).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, Notify};
use tokio::time::Instant;

use orrery_protocol::{JournalRecord, Lsn};

use crate::journal::{AppendHandle, JournalError};

/// How the committer decides when to fsync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveCommitMode {
    /// Flush a lone append immediately when idle; batch under load.
    Adaptive,
    /// Always batch for [`GroupCommitConfig::batch_window`] (deterministic tests).
    AlwaysBatch,
    /// Flush every append immediately (per-record fsync; baseline measurement).
    AlwaysIdle,
}

/// Adaptive group-commit parameters.
#[derive(Debug, Clone)]
pub struct GroupCommitConfig {
    /// The commit mode.
    pub mode: AdaptiveCommitMode,
    /// How long to accumulate a batch under load (D16: ~2 ms fsync group).
    pub batch_window: Duration,
    /// Hard cap on records per batch (prevents unbounded batches).
    pub batch_max_records: usize,
    /// Soft cap on encoded payload bytes per batch.
    pub batch_max_bytes: usize,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            mode: AdaptiveCommitMode::Adaptive,
            batch_window: Duration::from_micros(500),
            batch_max_records: 8192,
            batch_max_bytes: 1 << 20,
        }
    }
}

/// A pending append awaiting its group fsync.
#[derive(Debug)]
struct Pending {
    handle: Arc<AppendHandle>,
    bytes: usize,
    /// The record itself, published to chain-replication subscribers once the
    /// batch is durably flushed (§4) — unless `publish` is false (the record
    /// arrived via chain replication, so re-broadcasting it would echo it back
    /// to its origin and amplify without bound).
    record: JournalRecord,
    /// Whether the committer publishes this record on flush (§4).
    publish: bool,
    /// When this append entered the queue: the batch window is measured from
    /// the batch's *first* arrival, not from when the committer happens to
    /// notice the work.
    arrived: Instant,
}

/// Shared commit queue between the journal and the committer task.
#[derive(Debug)]
pub(crate) struct CommitQueue {
    inner: Mutex<VecDeque<Pending>>,
    wake: Notify,
}

impl CommitQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
        }
    }

    fn push(&self, pending: Pending) {
        self.inner
            .lock()
            .expect("commit queue lock")
            .push_back(pending);
        self.wake.notify_one();
    }

    fn len(&self) -> usize {
        self.inner.lock().expect("commit queue lock").len()
    }
}

/// The committer's shared state, owned by [`CommitterHandle`].
#[derive(Debug)]
pub(crate) struct CommitterState {
    config: GroupCommitConfig,
    queue: CommitQueue,
    shutdown_flag: AtomicBool,
    shutdown: Notify,
    /// Notified once the committer task has exited (releasing its store clone).
    exited: Notify,
    flushing: AtomicBool,
    committed: Mutex<Lsn>,
    flush_count: AtomicUsize,
    /// Committed records, published for chain replication (§4). Subscribers
    /// that fall behind rescan the journal from their watermark, so a bounded
    /// channel here is a lag signal, not a loss.
    published: broadcast::Sender<JournalRecord>,
}

impl CommitterState {
    fn is_flushing(&self) -> bool {
        self.flushing.load(Ordering::Acquire)
    }

    fn committed(&self) -> Lsn {
        *self.committed.lock().expect("committed lock")
    }

    fn set_committed(&self, lsn: Lsn) {
        *self.committed.lock().expect("committed lock") = lsn;
    }

    fn flush_count(&self) -> usize {
        self.flush_count.load(Ordering::Acquire)
    }
}

/// An owned handle to the committer, used by the journal to submit appends and
/// observe state. Dropping it signals the committer to drain and stop.
#[derive(Debug, Clone)]
pub(crate) struct CommitterHandle {
    state: Arc<CommitterState>,
}

impl CommitterHandle {
    /// Submit an append for group commit. The caller awaits [`AppendHandle::committed`].
    pub(crate) fn submit(
        &self,
        handle: Arc<AppendHandle>,
        payload_bytes: usize,
        record: JournalRecord,
        publish: bool,
    ) {
        self.state.queue.push(Pending {
            handle,
            bytes: payload_bytes,
            record,
            publish,
            arrived: Instant::now(),
        });
    }

    /// The highest LSN durably flushed so far.
    pub(crate) fn committed(&self) -> Lsn {
        self.state.committed()
    }

    /// The number of fsyncs issued (§4 group-commit observability).
    pub(crate) fn flush_count(&self) -> usize {
        self.state.flush_count()
    }

    /// Arm shutdown: the committer drains pending appends, flushes, then stops.
    pub(crate) fn shutdown(&self) {
        self.state.shutdown_flag.store(true, Ordering::Release);
        self.state.shutdown.notify_waiters();
    }

    /// Wait until the committer task has exited (releasing its store clone).
    pub(crate) async fn wait_exit(&self) {
        self.state.exited.notified().await;
    }
}

/// The flush callback: durably persist everything submitted so far. Runs on a
/// blocking thread (it is an `fdatasync`). Returns the store's error, if any.
pub(crate) type FlushFn = Arc<dyn Fn() -> Result<(), JournalError> + Send + Sync>;

/// Start the group-commit committer task.
///
/// `flush` must durably persist every payload already inserted into the store.
/// `flush` runs on a blocking thread so it never blocks the async runtime.
/// `published` receives each durably-flushed record (for chain replication).
pub(crate) fn spawn_committer(
    config: GroupCommitConfig,
    flush: FlushFn,
    published: broadcast::Sender<JournalRecord>,
) -> CommitterHandle {
    let state = Arc::new(CommitterState {
        config,
        queue: CommitQueue::new(),
        shutdown_flag: AtomicBool::new(false),
        shutdown: Notify::new(),
        exited: Notify::new(),
        flushing: AtomicBool::new(false),
        committed: Mutex::new(Lsn::new(0, 0)),
        flush_count: AtomicUsize::new(0),
        published,
    });

    let task_state = Arc::clone(&state);
    tokio::task::spawn(async move {
        run_committer(task_state, flush).await;
    });

    CommitterHandle { state }
}

async fn run_committer(state: Arc<CommitterState>, flush: FlushFn) {
    loop {
        // Shutdown with nothing pending: exit and release the store clone.
        if state.shutdown_armed() && state.queue.len() == 0 {
            break;
        }

        // Wait until there is work (or shutdown).
        if state.queue.len() == 0 {
            tokio::select! {
                _ = state.queue.wake.notified() => {}
                _ = state.shutdown.notified() => {}
            }
            continue;
        }

        // Decide whether to batch (wait for more appends) or flush immediately.
        let idle_fast_path = state.config.mode == AdaptiveCommitMode::Adaptive
            && state.queue.len() == 1
            && !state.is_flushing();

        if !idle_fast_path && state.config.mode != AdaptiveCommitMode::AlwaysIdle {
            // Batch: hold the fsync until the oldest pending append has been
            // waiting `batch_window`. The window is measured from the batch's
            // first arrival, not by a fresh `sleep(batch_window)` raced against
            // the wake `Notify` — under load the queue's `notify_one` permit
            // is always buffered (N pushes, 1 waiter), so the old select
            // resolved through the permit branch instantly and every batch
            // flushed after ~0 ms of accumulation.
            let deadline = {
                let queue = state.queue.inner.lock().expect("commit queue lock");
                queue
                    .front()
                    .map_or_else(Instant::now, |oldest| oldest.arrived)
                    + state.config.batch_window
            };
            loop {
                // A size cap already reached flushes early.
                let (qlen, qbytes) = {
                    let queue = state.queue.inner.lock().expect("commit queue lock");
                    (queue.len(), queue.iter().map(|p| p.bytes).sum::<usize>())
                };
                if qlen >= state.config.batch_max_records || qbytes >= state.config.batch_max_bytes
                {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::select! {
                    // New work: re-check the caps (and possibly the queue's
                    // first-arrival time is unchanged, so the deadline is).
                    _ = state.queue.wake.notified() => {}
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            }
        }

        // Drain the current queue into a batch, honoring caps.
        let batch = drain_batch(&state);
        if batch.is_empty() {
            continue;
        }

        let max_lsn = batch
            .iter()
            .map(|p| p.handle.lsn())
            .max()
            .expect("non-empty batch");

        state.flushing.store(true, Ordering::Release);
        let result = flush_inner(&flush).await;
        state.flushing.store(false, Ordering::Release);

        match result {
            Ok(()) => {
                state.flush_count.fetch_add(1, Ordering::AcqRel);
                state.set_committed(max_lsn);
                for p in batch {
                    p.handle.resolve(Ok(max_lsn));
                    // Publish for chain replication — except records that
                    // arrived via chain replication themselves (`!publish`),
                    // which must not echo back to their origin (§4). A full
                    // channel is a lag signal (the subscriber rescans from its
                    // watermark).
                    if p.publish {
                        let _ = state.published.send(p.record);
                    }
                }
            }
            Err(e) => {
                for p in batch {
                    p.handle.resolve(Err(e.clone()));
                }
            }
        }
    }
    state.exited.notify_one();
}

impl CommitterState {
    fn shutdown_armed(&self) -> bool {
        self.shutdown_flag.load(Ordering::Acquire)
    }
}

async fn flush_inner(flush: &FlushFn) -> Result<(), JournalError> {
    let flush = Arc::clone(flush);
    tokio::task::spawn_blocking(move || flush())
        .await
        .unwrap_or_else(|_| Err(JournalError::Store("committer task panicked".into())))
}

fn drain_batch(state: &CommitterState) -> Vec<Pending> {
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    let max_records = state.config.batch_max_records;
    let max_bytes = state.config.batch_max_bytes;

    if state.config.mode == AdaptiveCommitMode::AlwaysIdle {
        // Flush one record at a time.
        if let Some(pending) = state
            .queue
            .inner
            .lock()
            .expect("commit queue lock")
            .pop_front()
        {
            batch.push(pending);
        }
        return batch;
    }

    let mut queue = state.queue.inner.lock().expect("commit queue lock");
    while batch.len() < max_records && bytes < max_bytes {
        let Some(pending) = queue.pop_front() else {
            break;
        };
        bytes += pending.bytes;
        batch.push(pending);
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Journal, JournalConfig};

    fn test_node(n: u8) -> orrery_protocol::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn mk_record(entity: u64) -> JournalRecord {
        let payload = entity.to_le_bytes();
        JournalRecord {
            lsn: Lsn::new(0, 0), // assigned by the journal
            cell: orrery_protocol::CellId::ROOT,
            grid: orrery_protocol::GridId::ROOT,
            entity: orrery_protocol::PersistId::new(entity),
            tick: orrery_protocol::Tick::new(1),
            epoch: orrery_protocol::Epoch::new(0),
            author: test_node(1),
            kind: orrery_protocol::RecordKind::Spawn,
            payload: bytes::Bytes::copy_from_slice(&payload),
            crc: crate::payload_crc(&payload),
        }
    }

    #[tokio::test]
    async fn committed_resolves_when_resolve_races_the_await() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = std::sync::Arc::new(
            Journal::open(&JournalConfig {
                dir: dir.path().to_path_buf(),
                commit: GroupCommitConfig::default(),
            })
            .expect("open journal"),
        );

        // 1000 appends, each awaited from its own task while the committer
        // resolves from the committer task: the old `Notify + Mutex<Option>`
        // pair could lose the wakeup between the result check and the
        // `notified().await`; the oneshot channel carries the result so no
        // waiter can ever wedge.
        let mut waiters = Vec::new();
        for i in 0..1000u64 {
            let handle = journal.append(mk_record(i)).expect("append");
            waiters.push(tokio::spawn(async move { handle.committed().await }));
        }
        let all = tokio::time::timeout(Duration::from_secs(5), async {
            for w in waiters {
                w.await.expect("waiter task").expect("commit");
            }
        })
        .await;
        assert!(all.is_ok(), "1000 appends all resolved within 5 s");

        journal.close().await.expect("close");
    }

    #[tokio::test]
    async fn batch_window_is_honored_under_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = std::sync::Arc::new(
            Journal::open(&JournalConfig {
                dir: dir.path().to_path_buf(),
                commit: GroupCommitConfig {
                    mode: AdaptiveCommitMode::Adaptive,
                    batch_window: Duration::from_millis(200),
                    batch_max_records: 8192,
                    batch_max_bytes: 1 << 20,
                },
            })
            .expect("open journal"),
        );

        // Submit 8 appends concurrently and time the first commit.
        let t0 = Instant::now();
        let mut waiters = Vec::new();
        for i in 0..8u64 {
            let j = std::sync::Arc::clone(&journal);
            waiters.push(tokio::spawn(async move {
                let handle = j.append(mk_record(i)).expect("append");
                handle.committed().await.expect("commit");
                t0.elapsed()
            }));
        }
        let mut elapsed = Vec::new();
        for w in waiters {
            elapsed.push(w.await.expect("waiter task"));
        }
        elapsed.sort_unstable();

        // Under the broken committer every append flushed on arrival (~177 us,
        // 8 fsyncs). With the window measured from the batch's first arrival,
        // the 8 concurrent appends accumulate into ONE batch.
        assert_eq!(
            journal.flush_count(),
            1,
            "8 concurrent appends must share one fsync"
        );
        assert!(
            elapsed[0] >= Duration::from_millis(150),
            "first commit waited the batch window: {:?}",
            elapsed[0]
        );

        journal.close().await.expect("close");
    }
}
