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

use tokio::sync::Notify;

use orrery_protocol::Lsn;

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

    #[cfg(test)]
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
    pub(crate) fn submit(&self, handle: Arc<AppendHandle>, payload_bytes: usize) {
        self.state.queue.push(Pending {
            handle,
            bytes: payload_bytes,
        });
    }

    /// The highest LSN durably flushed so far.
    pub(crate) fn committed(&self) -> Lsn {
        self.state.committed()
    }

    /// The number of fsyncs issued (test hook).
    #[cfg(test)]
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
pub(crate) fn spawn_committer(config: GroupCommitConfig, flush: FlushFn) -> CommitterHandle {
    let state = Arc::new(CommitterState {
        config,
        queue: CommitQueue::new(),
        shutdown_flag: AtomicBool::new(false),
        shutdown: Notify::new(),
        exited: Notify::new(),
        flushing: AtomicBool::new(false),
        committed: Mutex::new(Lsn::new(0, 0)),
        flush_count: AtomicUsize::new(0),
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
            tokio::select! {
                _ = state.queue.wake.notified() => {}
                _ = tokio::time::sleep(state.config.batch_window) => {}
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
