//! Adaptive group commit (docs/08-persistence.md §4, D16).
//!
//! Appends from all actors on a node accumulate in a shared queue; a single
//! committer task stages a whole group in one store batch, issues the durable
//! `fdatasync`, and resolves **every** waiter in the batch on that one fsync.
//! Two regimes:
//!
//! - **Adaptive (default):** work already queued by concurrent submitters is
//!   drained as one group without an intentional timer. A lone append arriving
//!   while the disk is idle is therefore flushed immediately, while arrivals
//!   during a durability flush naturally form the next group.
//! - The other [`AdaptiveCommitMode`]s exist to make batching deterministic in
//!   tests ([`AlwaysBatch`](AdaptiveCommitMode::AlwaysBatch) forces the window
//!   path even for a single record) and to measure worst case
//!   ([`AlwaysIdle`](AdaptiveCommitMode::AlwaysIdle) flushes per record).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Notify};

use orrery_protocol::{JournalRecord, Lsn};

use crate::journal::{AppendHandle, JournalCommitMetrics, JournalError, JournalStageSnapshot};

/// Store-side portions of one successful durability flush.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoreCommitTimings {
    pub(crate) fjall_batch_commit: Duration,
    pub(crate) sync_data: Duration,
}

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
    /// Optional time to accumulate a batch under load.
    ///
    /// [`Default`] is zero — but zero is *not* what production runs, and the
    /// difference has misdirected one investigation already. Every `Journal`
    /// `persistd` opens (primary, follower, promotion recovery) overrides this
    /// to 200 µs, so the deployment the P2 gate measures has a timer in the
    /// commit path and the `Default` used by the measurement rigs does not.
    /// [`apply_batch_window_override`] makes the value settable at run time so
    /// the two can be compared without a rebuild per point.
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
            // Concurrent gateway submissions and arrivals during SyncData
            // already form groups. Avoid putting a timer wake in the D16 p99
            // path; the idle fast path and the loaded path both drain promptly.
            batch_window: Duration::ZERO,
            batch_max_records: 8192,
            batch_max_bytes: 1 << 20,
        }
    }
}

/// A pending append awaiting its group fsync.
#[derive(Debug)]
pub(crate) struct Pending {
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
    /// Encoded storage mutations. They remain out of the database until the
    /// committer has selected the complete durability group.
    pub(crate) staged: StagedAppend,
}

/// One append's owned database mutations, consumed by the Fjall commit
/// callback as part of a larger atomic write batch.
#[derive(Debug)]
pub(crate) struct StagedAppend {
    pub(crate) key: Vec<u8>,
    pub(crate) encoded: Vec<u8>,
    pub(crate) originated: bool,
    #[cfg(feature = "chain-grpc")]
    pub(crate) provenance: Option<(Vec<u8>, Vec<u8>)>,
}

impl StagedAppend {
    fn bytes(&self) -> usize {
        let bytes = self.key.len() + self.encoded.len();
        #[cfg(feature = "chain-grpc")]
        let bytes = bytes
            + self
                .provenance
                .as_ref()
                .map_or(0, |(key, value)| key.len() + value.len());
        bytes
    }
}

/// Shared commit queue between the journal and the committer task.
#[derive(Debug)]
pub(crate) struct CommitQueue {
    inner: Mutex<VecDeque<Pending>>,
    condvar: Condvar,
}

impl CommitQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
        }
    }

    fn push(&self, pending: Pending) {
        self.inner
            .lock()
            .expect("commit queue lock")
            .push_back(pending);
        self.condvar.notify_one();
    }
}

/// The committer's shared state, owned by [`CommitterHandle`].
#[derive(Debug)]
pub(crate) struct CommitterState {
    config: GroupCommitConfig,
    queue: CommitQueue,
    shutdown_flag: AtomicBool,
    exited_flag: AtomicBool,
    /// Notified once the committer task has exited (releasing its store clone).
    exited: Notify,
    flushing: AtomicBool,
    /// `None` distinguishes a fresh journal from one whose first record at
    /// LSN 0 has crossed the durability boundary.
    committed: Mutex<Option<Lsn>>,
    flush_count: AtomicUsize,
    metrics: Arc<JournalCommitMetrics>,
    /// Committed records, published for chain replication (§4). Subscribers
    /// that fall behind rescan the journal from their watermark, so a bounded
    /// channel here is a lag signal, not a loss.
    published: broadcast::Sender<JournalRecord>,
}

impl CommitterState {
    fn is_flushing(&self) -> bool {
        self.flushing.load(Ordering::Acquire)
    }

    fn committed(&self) -> Option<Lsn> {
        *self.committed.lock().expect("committed lock")
    }

    fn set_committed(&self, lsn: Lsn) {
        *self.committed.lock().expect("committed lock") = Some(lsn);
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
        staged: StagedAppend,
        record: JournalRecord,
        publish: bool,
    ) {
        let bytes = staged.bytes();
        self.state.queue.push(Pending {
            handle,
            bytes,
            record,
            publish,
            arrived: Instant::now(),
            staged,
        });
    }

    /// The highest LSN durably flushed so far.
    pub(crate) fn committed(&self) -> Option<Lsn> {
        self.state.committed()
    }

    /// The number of fsyncs issued (§4 group-commit observability).
    pub(crate) fn flush_count(&self) -> usize {
        self.state.flush_count()
    }

    /// Arm shutdown: the committer drains pending appends, flushes, then stops.
    pub(crate) fn shutdown(&self) {
        self.state.shutdown_flag.store(true, Ordering::Release);
        self.state.queue.condvar.notify_all();
    }

    /// Wait until the committer task has exited (releasing its store clone).
    ///
    /// The `notified()` future is created *before* the flag is read, and that
    /// order is load-bearing. `Notify::notify_waiters` — which the committer
    /// calls on its way out — stores no permit: it wakes only the waiters
    /// already registered at the moment it runs. Reading the flag first left a
    /// window in which the committer could set the flag and notify an empty
    /// waiter set, after which `notified().await` blocked forever. That is a
    /// hang in `Journal::close`, and through it in `CellRuntime::close`, so it
    /// stalled whichever test happened to lose the race rather than failing it.
    pub(crate) async fn wait_exit(&self) {
        let notified = self.state.exited.notified();
        if self.state.exited_flag.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

impl Drop for CommitterHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Atomically stage a selected group and durably persist it.
pub(crate) type CommitFn =
    Arc<dyn Fn(&[Pending]) -> Result<StoreCommitTimings, JournalError> + Send + Sync>;

/// Start the group-commit committer task on a dedicated OS thread.
pub(crate) fn spawn_committer(
    config: GroupCommitConfig,
    commit: CommitFn,
    published: broadcast::Sender<JournalRecord>,
    recovered_committed: Option<Lsn>,
    metrics: Arc<JournalCommitMetrics>,
) -> CommitterHandle {
    let config = apply_batch_window_override(config);
    let state = Arc::new(CommitterState {
        config,
        queue: CommitQueue::new(),
        shutdown_flag: AtomicBool::new(false),
        exited_flag: AtomicBool::new(false),
        exited: Notify::new(),
        flushing: AtomicBool::new(false),
        committed: Mutex::new(recovered_committed),
        flush_count: AtomicUsize::new(0),
        metrics,
        published,
    });

    let task_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("journal-committer".into())
        .spawn(move || {
            run_committer(task_state, commit);
        })
        .expect("spawn journal committer thread");

    CommitterHandle { state }
}

/// Environment override for [`GroupCommitConfig::batch_window`], in
/// microseconds (`ORRERY_JOURNAL_BATCH_WINDOW_US`).
///
/// The batch window is the one group-commit constant whose best value is a
/// property of the *device and the arrival rate*, not of the code: it trades
/// the p50 of a lone append against the fsync rate the store has to sustain,
/// and the crossover between the two moves with both. Every embedding of the
/// journal — `persistd`, the actor runtime, the measurement rigs — carries its
/// own literal, so a sweep otherwise costs one rebuild per point and cannot be
/// interleaved with a baseline on a box whose runs vary. Reading it here, at
/// the one place every embedding funnels through, makes the window measurable
/// in situ; absent the variable the caller's value is used unchanged.
///
/// `0` is a meaningful setting (take whatever is queued, no timer), so an
/// absent or unparseable value is the only thing that falls through.
fn apply_batch_window_override(mut config: GroupCommitConfig) -> GroupCommitConfig {
    if let Some(window) = parse_batch_window(std::env::var(BATCH_WINDOW_ENV).ok().as_deref()) {
        config.batch_window = window;
    }
    config
}

/// The variable [`apply_batch_window_override`] reads.
const BATCH_WINDOW_ENV: &str = "ORRERY_JOURNAL_BATCH_WINDOW_US";

/// Parse the override's *value*, separately from reading it.
///
/// Split out so the contract is testable without a process-global write:
/// `set_var` in one test races every other test in the binary that opens a
/// journal, and the committer this configures is exactly what those tests
/// exercise.
fn parse_batch_window(raw: Option<&str>) -> Option<Duration> {
    raw?.trim().parse::<u64>().ok().map(Duration::from_micros)
}

fn run_committer(state: Arc<CommitterState>, commit: CommitFn) {
    loop {
        let mut guard = state.queue.inner.lock().expect("commit queue lock");
        while guard.is_empty() && !state.shutdown_armed() {
            guard = state.queue.condvar.wait(guard).expect("condvar wait");
        }
        if guard.is_empty() {
            if state.shutdown_armed() {
                break;
            }
            continue;
        }

        let idle_fast_path = state.config.mode == AdaptiveCommitMode::Adaptive
            && guard.len() == 1
            && !state.is_flushing()
            && state.config.batch_window == Duration::ZERO;

        if !state.shutdown_armed()
            && !idle_fast_path
            && state.config.mode != AdaptiveCommitMode::AlwaysIdle
            && state.config.batch_window > Duration::ZERO
        {
            let deadline = guard
                .front()
                .map_or_else(Instant::now, |oldest| oldest.arrived)
                + state.config.batch_window;
            while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                if state.shutdown_armed() {
                    break;
                }
                let (qlen, qbytes) = (guard.len(), guard.iter().map(|p| p.bytes).sum::<usize>());
                if qlen >= state.config.batch_max_records || qbytes >= state.config.batch_max_bytes
                {
                    break;
                }
                let (next_guard, _) = state
                    .queue
                    .condvar
                    .wait_timeout(guard, remaining)
                    .expect("condvar wait_timeout");
                guard = next_guard;
            }
        }

        let mut batch = Vec::new();
        let mut bytes = 0usize;
        while let Some(p) = guard.pop_front() {
            bytes += p.bytes;
            batch.push(p);
            if state.config.mode == AdaptiveCommitMode::AlwaysIdle {
                break;
            }
            if batch.len() >= state.config.batch_max_records
                || bytes >= state.config.batch_max_bytes
            {
                break;
            }
        }
        drop(guard);

        if batch.is_empty() {
            continue;
        }

        let max_lsn = batch
            .iter()
            .map(|p| p.handle.lsn())
            .max()
            .expect("non-empty batch");
        let records = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let batch_bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let queue_wait = batch
            .iter()
            .map(|p| p.arrived.elapsed())
            .max()
            .unwrap_or_default();

        state.flushing.store(true, Ordering::Release);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| commit(&batch)))
            .unwrap_or_else(|_| Err(JournalError::Store("committer task panicked".into())));
        state.flushing.store(false, Ordering::Release);

        match result {
            Ok(store_timings) => {
                state.flush_count.fetch_add(1, Ordering::AcqRel);
                state.set_committed(max_lsn);
                let resolve_started = Instant::now();
                for p in batch {
                    if p.publish {
                        let _ = state.published.send(p.record);
                    }
                    p.handle.resolve(Ok(max_lsn));
                }
                let resolve = resolve_started.elapsed();
                state.metrics.record_group(JournalStageSnapshot {
                    flushes: 1,
                    records,
                    bytes: batch_bytes,
                    queue_wait_us_sum: duration_us(queue_wait),
                    queue_wait_us_max: duration_us(queue_wait),
                    blocking_dispatch_us_sum: 0,
                    blocking_dispatch_us_max: 0,
                    fjall_batch_commit_us_sum: duration_us(store_timings.fjall_batch_commit),
                    fjall_batch_commit_us_max: duration_us(store_timings.fjall_batch_commit),
                    sync_data_us_sum: duration_us(store_timings.sync_data),
                    sync_data_us_max: duration_us(store_timings.sync_data),
                    resolve_us_sum: duration_us(resolve),
                    resolve_us_max: duration_us(resolve),
                    // Derived inside `record_group` from `bytes`/`records`
                    // above: only the recorder knows whether this flush set a
                    // new maximum or crossed the slow threshold.
                    ..JournalStageSnapshot::default()
                });
            }
            Err(e) => {
                for p in batch {
                    p.handle.resolve(Err(e.clone()));
                }
            }
        }
    }
    state.exited_flag.store(true, Ordering::Release);
    state.exited.notify_waiters();
}

impl CommitterState {
    fn shutdown_armed(&self) -> bool {
        self.shutdown_flag.load(Ordering::Acquire)
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
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

    #[test]
    fn adaptive_default_has_no_intentional_batch_timer() {
        let config = GroupCommitConfig::default();
        assert_eq!(config.mode, AdaptiveCommitMode::Adaptive);
        assert_eq!(config.batch_window, Duration::ZERO);
    }

    #[test]
    fn batch_window_override_parses_zero_and_rejects_junk() {
        // `0` is the adaptive no-timer setting and must survive the parse: an
        // `unwrap_or_default`-style fallback or a `> 0` guard would silently
        // turn "flush what is queued" into "keep the caller's window", which
        // is the one value a sweep most needs to be able to ask for.
        assert_eq!(parse_batch_window(Some("0")), Some(Duration::ZERO));
        assert_eq!(
            parse_batch_window(Some("2000")),
            Some(Duration::from_micros(2000))
        );
        assert_eq!(
            parse_batch_window(Some(" 500 ")),
            Some(Duration::from_micros(500))
        );
        // Absent or unreadable leaves the caller's value alone -- signalled by
        // `None`, never by a zero window, which would be an fsync per wake.
        assert_eq!(parse_batch_window(None), None);
        assert_eq!(parse_batch_window(Some("")), None);
        assert_eq!(parse_batch_window(Some("2ms")), None);
        assert_eq!(parse_batch_window(Some("-1")), None);
    }

    #[test]
    fn batch_window_override_replaces_the_callers_window() {
        // The override has to reach a config whose window is already non-zero:
        // `persistd` passes 200 us, not the `Default` zero, so an override
        // applied only to a defaulted field would be inert in the one binary
        // the gate measures.
        let config = GroupCommitConfig {
            batch_window: Duration::from_micros(200),
            ..GroupCommitConfig::default()
        };
        let mut overridden = config.clone();
        if let Some(window) = parse_batch_window(Some("2000")) {
            overridden.batch_window = window;
        }
        assert_eq!(config.batch_window, Duration::from_micros(200));
        assert_eq!(overridden.batch_window, Duration::from_micros(2000));
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

    fn staged(entity: u64) -> StagedAppend {
        StagedAppend {
            key: entity.to_be_bytes().to_vec(),
            encoded: entity.to_le_bytes().to_vec(),
            originated: true,
            #[cfg(feature = "chain-grpc")]
            provenance: None,
        }
    }

    #[tokio::test]
    async fn store_failure_resolves_every_handle_without_publishing() {
        let (published, mut subscriber) = broadcast::channel(8);
        let metrics = Arc::new(crate::journal::JournalCommitMetrics::new());
        let committer = spawn_committer(
            GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(1),
                batch_max_records: 8,
                batch_max_bytes: 1 << 20,
            },
            // No assertion on how many records one call sees: the three
            // submits race the 1 ms window, so the committer may legitimately
            // flush them as 1+2 or 1+1+1 on a loaded runner. What the test is
            // named for holds either way -- every handle below resolves with
            // the injected error, which is also what proves all three records
            // reached the store, since a handle resolves only from the batch
            // that carried it.
            Arc::new(|_pending| Err(JournalError::Store("injected batch failure".into()))),
            published,
            None,
            Arc::clone(&metrics),
        );
        let mut handles = Vec::new();
        for entity in 0..3 {
            let record = mk_record(entity);
            let handle = Arc::new(AppendHandle::new(
                Lsn::new(0, entity),
                std::time::Instant::now(),
                Arc::clone(&metrics),
            ));
            committer.submit(Arc::clone(&handle), staged(entity), record, true);
            handles.push(handle);
        }
        for handle in handles {
            assert!(matches!(
                handle.committed().await,
                Err(JournalError::Store(message)) if message == "injected batch failure"
            ));
        }
        assert!(subscriber.try_recv().is_err());
        assert_eq!(committer.committed(), None);
        assert_eq!(committer.flush_count(), 0);
        committer.shutdown();
        committer.wait_exit().await;
    }

    #[tokio::test]
    async fn shutdown_drains_selected_rows_without_waiting_for_batch_window() {
        let (published, _) = broadcast::channel(8);
        let committed_rows = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&committed_rows);
        let metrics = Arc::new(crate::journal::JournalCommitMetrics::new());
        let committer = spawn_committer(
            GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_secs(30),
                batch_max_records: 8,
                batch_max_bytes: 1 << 20,
            },
            Arc::new(move |pending| {
                observed.fetch_add(pending.len(), Ordering::AcqRel);
                Ok(StoreCommitTimings {
                    fjall_batch_commit: Duration::ZERO,
                    sync_data: Duration::ZERO,
                })
            }),
            published,
            None,
            Arc::clone(&metrics),
        );
        let record = mk_record(9);
        let handle = Arc::new(AppendHandle::new(
            Lsn::new(0, 9),
            std::time::Instant::now(),
            metrics,
        ));
        committer.submit(Arc::clone(&handle), staged(9), record, true);
        committer.shutdown();

        tokio::time::timeout(Duration::from_secs(1), handle.committed())
            .await
            .expect("shutdown drain must bypass the batch timer")
            .expect("drain commit");
        committer.wait_exit().await;
        assert_eq!(committed_rows.load(Ordering::Acquire), 1);
    }
}
