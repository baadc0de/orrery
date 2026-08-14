//! Chain replication of the journal to one async follower (docs/08-persistence.md
//! §4, D11).
//!
//! Each node streams its journal to exactly one async follower — the next node
//! in HRW order over node ids (ops-overridable, placed in a different AZ). The
//! follower persists segments verbatim. Replication is **async**: it is not in
//! the ack path, so it never adds to client-observed latency. Lag is monitored
//! and alarmed above [`ChainConfig::lag_alarm`] (default 100 ms, D11).
//!
//! The transport is intentionally pluggable: a [`ChainTransport`] carries
//! committed records to the follower and reports the follower's durable
//! watermark. The default [`MemChainTransport`] is an in-process shim that
//! writes into a follower's [`ChainSink`] directly, so the replication logic is
//! testable without a network. A real deployment supplies a transport that
//! pushes over the node-to-node link (tonic/gRPC per D12) into the follower's
//! sink.
//!
//! The follower's [`ChainSink`] is the durable landing point: it appends each
//! received record to the follower's own journal (so the follower can serve as
//! the recovery source if the primary's disk is lost) and tracks the highest
//! *origin* LSN it has durably persisted — the follower watermark that bounds
//! RPO.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

use crate::journal::{Journal, JournalError};

/// Configuration for chain replication (D11 §4).
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// The node id of this node's follower (the next node in HRW order).
    pub follower: u64,
    /// The lag (primary committed LSN vs follower durable LSN) above which an
    /// alarm fires. Default 100 ms of journal time; the exact mapping from LSN
    /// to wall time is the transport's report.
    pub lag_alarm: Duration,
    /// The maximum number of records to batch per replication flush.
    pub batch_max: usize,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            follower: 0,
            lag_alarm: Duration::from_millis(100),
            batch_max: 1024,
        }
    }
}

/// The durable landing point on the follower for replicated records.
///
/// A [`ChainSink`] appends records to the follower's journal and reports the
/// highest *primary* (origin) LSN durably persisted. The sink is the *only*
/// thing the transport writes to on the follower side.
#[async_trait::async_trait]
pub trait ChainSink: Send + Sync {
    /// Durably persist `record` and return the follower's new watermark: the
    /// highest *origin* LSN now durable on the follower. The record carries
    /// its origin LSN (stamped by the origin journal before encoding), so the
    /// follower echoes the record's own LSN — not the locally assigned one,
    /// which lives in the follower's independent LSN space and would be
    /// meaningless as lag to the origin.
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError>;

    /// The follower's durable watermark (highest origin LSN persisted), if any.
    async fn watermark(&self) -> Option<Lsn>;
}

/// A transport that carries committed records from a primary to its follower.
///
/// The transport is responsible for delivery and for reporting the follower's
/// durable watermark (so the primary can measure lag). It is **not** in the ack
/// path.
#[async_trait::async_trait]
pub trait ChainTransport: Send + Sync {
    /// Push `record` to the follower and return the follower's durable
    /// watermark after this record (in the primary's LSN space).
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError>;

    /// The follower's durable watermark (highest primary LSN persisted on the
    /// follower), or `None` if unknown.
    async fn follower_watermark(&self) -> Option<Lsn>;
}

/// The replicator's shutdown signal: an `AtomicBool` flag plus a `Notify`
/// early-wakeup. The flag is the source of truth, checked at every await
/// boundary; a bare `Notify` permit can be consumed by the wrong `select!`
/// (e.g. a stalled transport push races shutdown) and leave the outer loop
/// waiting forever.
#[derive(Debug, Default)]
struct ShutdownSignal {
    flag: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

impl ShutdownSignal {
    fn arm(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_waiters();
    }

    fn armed(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// A running chain-replication task: subscribes to the primary journal's
/// committed records and pushes them to the follower.
pub struct ChainReplicator {
    /// The follower's durable watermark, updated as the transport reports it.
    watermark: Arc<std::sync::Mutex<Option<Lsn>>>,
    /// Replication lag: `primary.committed()` minus the follower's reported
    /// durable watermark. LSN offsets are byte positions, so the raw gap is in
    /// bytes; [`ChainReplicator::lag_bytes`] exposes it and the alarm compares
    /// it against [`ChainConfig::lag_alarm`]'s byte budget (D11's "~100 ms of
    /// journal" mapped to bytes — see [`lag_alarm_bytes`]).
    lag_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Join handle; awaited on shutdown.
    join: tokio::task::JoinHandle<()>,
    /// Signals the task to stop.
    shutdown: Arc<ShutdownSignal>,
    /// The follower node id (for diagnostics).
    follower: u64,
}

impl ChainReplicator {
    /// The follower node id this replicator streams to.
    #[must_use]
    pub fn follower(&self) -> u64 {
        self.follower
    }

    /// The highest LSN durably persisted on the follower, if known.
    #[must_use]
    pub fn follower_watermark(&self) -> Option<Lsn> {
        *self.watermark.lock().expect("chain watermark lock")
    }

    /// The current replication lag in journal bytes: `primary.committed()`
    /// minus the highest primary LSN the follower has reported durable. Both
    /// are in the primary's LSN space (the follower echoes each record's own
    /// LSN), so the difference is a byte gap; the lag alarm fires (a
    /// `tracing::warn!`) while this exceeds the [`ChainConfig::lag_alarm`]
    /// byte budget.
    #[must_use]
    pub fn lag_bytes(&self) -> u64 {
        self.lag_bytes.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Stop the replicator task, awaiting its exit.
    pub async fn shutdown(self) {
        self.shutdown.arm();
        let _ = self.join.await;
    }
}

/// Compute the lag between the primary's committed LSN and the follower's
/// reported durable watermark, in journal bytes. `committed` is stamped by
/// this journal and `watermark` was echoed by the follower from the records'
/// own LSNs, so both live in the primary's LSN space. Returns `None` when the
/// watermark is in a different segment — a cross-segment gap is ill-defined
/// as a byte distance, and segment transitions are rare enough that the alarm
/// simply skips that sample.
fn lag_in_bytes(committed: Lsn, watermark: Option<Lsn>) -> Option<u64> {
    let w = watermark?;
    if w.segment != committed.segment {
        return None;
    }
    Some(committed.offset.saturating_sub(w.offset))
}

/// Convert the [`ChainConfig::lag_alarm`] duration into the journal byte
/// budget the lag gauge is compared against. The lag alarm is D11's "~100 ms
/// of journal"; LSNs carry no wall-clock time, so the replicator maps the
/// duration through the journal's sustained byte rate. At the D16 envelope a
/// node commits one record group per ~0.5 ms batch window; the constant below
/// is the conservative floor (bytes per lag-alarm second) used until the
/// tonic transport (D12) reports the follower's wall-clock watermark age.
const LAG_BYTES_PER_ALARM_SECOND: f64 = 1.0;

/// The byte budget for [`ChainConfig::lag_alarm`]: the alarm fires when the
/// follower's durable watermark trails the primary's committed LSN by more
/// journal bytes than the primary produces in `lag_alarm` wall time.
fn lag_alarm_bytes(lag_alarm: Duration) -> u64 {
    (lag_alarm.as_secs_f64() * LAG_BYTES_PER_ALARM_SECOND) as u64
}

/// Spawn a chain-replication task streaming `journal`'s committed records to
/// `transport` (which writes into the follower's [`ChainSink`]).
///
/// The journal scan is the correctness path and the committed-record broadcast
/// is only its wake-up signal. The task probes the follower's durable watermark
/// on startup and after every transport error, then scans and replays everything
/// after that cursor. This also catches records committed before the task was
/// spawned and records committed while the follower was unavailable.
pub fn spawn_chain(
    journal: Arc<Journal>,
    transport: Arc<dyn ChainTransport>,
    config: &ChainConfig,
) -> ChainReplicator {
    let watermark = Arc::new(std::sync::Mutex::new(None));
    let lag_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let shutdown = Arc::new(ShutdownSignal::default());
    let follower = config.follower;
    let batch_max = config.batch_max.max(1);
    let alarm_bytes = lag_alarm_bytes(config.lag_alarm);
    let mut rx = journal.subscribe();
    let wm = Arc::clone(&watermark);
    let lag = Arc::clone(&lag_bytes);
    let sd = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        let mut cursor: Option<Lsn> = None;
        let mut needs_probe = true;
        loop {
            // The flag is checked at every await boundary: `push_batch`'s
            // per-append select races shutdown too, so a permit-style wakeup
            // could be consumed there and missed here.
            if sd.armed() {
                break;
            }
            if needs_probe {
                // A probe is deliberately cancellable: an unreachable network
                // follower must not make shutdown wait for connector timeout.
                let remote = tokio::select! {
                    value = transport.follower_watermark() => value,
                    _ = sd.wake.notified() => break,
                };
                if let Some(remote) = remote {
                    cursor = Some(remote);
                    update_progress(&journal, &wm, &lag, alarm_bytes, follower, remote);
                }
                needs_probe = false;
            }

            // `scan_from` is inclusive, while the durable watermark is the
            // record already held by the follower. Filtering it out is what
            // makes normal reconnects duplicate-free. Limit each collection so
            // a hot journal cannot make this non-Send iterator monopolize the
            // replication task indefinitely.
            let from = cursor.unwrap_or(Lsn::new(0, 0));
            let committed = journal.committed_watermark();
            let batch = committed
                .map(|committed| {
                    journal
                        .scan_originated_from(from)
                        .filter(|item| match item {
                            Ok(stored) => {
                                stored.lsn <= committed
                                    && cursor.is_none_or(|last| stored.record.lsn > last)
                            }
                            Err(_) => true,
                        })
                        .take(batch_max)
                        .map(|item| item.map(|stored| stored.record))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap_or_else(|| Ok(Vec::new()));
            let batch = match batch {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::error!(follower, ?error, "chain: primary journal scan failed");
                    break;
                }
            };

            if !batch.is_empty() {
                let pushed = push_batch(
                    &journal,
                    &*transport,
                    &batch,
                    &sd,
                    &wm,
                    &lag,
                    alarm_bytes,
                    follower,
                )
                .await;
                if let Some(last) = pushed.last() {
                    cursor = Some(last);
                }
                if !pushed.complete() {
                    // The failed append is intentionally not retained in
                    // volatile state. Re-probe the follower's durable cursor,
                    // then reconstruct the complete tail from the primary.
                    needs_probe = true;
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                        _ = sd.wake.notified() => break,
                    }
                }
                continue;
            }

            // We reached the journal tail represented by `cursor`. Broadcast
            // contents are never delivered directly; any message (or lag
            // notification) merely tells us to scan again from the durable
            // cursor, preserving primary order even across channel overflow.
            let recv = tokio::select! {
                result = rx.recv() => result,
                _ = sd.wake.notified() => break,
            };
            match recv {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    ChainReplicator {
        watermark,
        lag_bytes,
        join,
        shutdown,
        follower,
    }
}

/// The outcome of [`push_batch`]: the LSNs actually delivered, and whether
/// the whole batch went through.
struct PushOutcome {
    /// The last record durably pushed (if any), used to advance the rescan
    /// cursor.
    last: Option<Lsn>,
    /// False when a transport error or shutdown interrupted the batch.
    complete: bool,
}

impl PushOutcome {
    fn last(&self) -> Option<Lsn> {
        self.last
    }
    fn complete(&self) -> bool {
        self.complete
    }
}

fn update_progress(
    journal: &Journal,
    wm: &std::sync::Mutex<Option<Lsn>>,
    lag: &std::sync::atomic::AtomicU64,
    alarm_bytes: u64,
    follower: u64,
    durable: Lsn,
) {
    *wm.lock().expect("chain watermark lock") = Some(durable);
    if let Some(bytes) = lag_in_bytes(journal.committed(), Some(durable)) {
        lag.store(bytes, std::sync::atomic::Ordering::Release);
        if bytes > alarm_bytes {
            tracing::warn!(
                follower,
                lag_bytes = bytes,
                alarm_bytes,
                "chain: replication lag exceeds lag_alarm"
            );
        }
    }
}

/// Push one batch of records to the follower. `shutdown` races every append so
/// a stalled transport cannot wedge [`ChainReplicator::shutdown`] forever. The
/// append result is itself the follower's durable watermark, avoiding a second
/// transport probe after every record.
#[allow(clippy::too_many_arguments)]
async fn push_batch(
    journal: &Journal,
    transport: &dyn ChainTransport,
    records: &[JournalRecord],
    shutdown: &ShutdownSignal,
    wm: &std::sync::Mutex<Option<Lsn>>,
    lag: &std::sync::atomic::AtomicU64,
    alarm_bytes: u64,
    follower: u64,
) -> PushOutcome {
    let mut last = None;
    for record in records {
        if shutdown.armed() {
            return PushOutcome {
                last,
                complete: false,
            };
        }
        let pushed = tokio::select! {
            r = transport.append(record.clone()) => r,
            _ = shutdown.wake.notified() => {
                return PushOutcome { last, complete: false };
            }
        };
        match pushed {
            Ok(durable) => {
                if durable < record.lsn {
                    tracing::warn!(
                        follower,
                        record_lsn = %record.lsn,
                        durable_lsn = %durable,
                        "chain: follower returned a regressed watermark"
                    );
                    return PushOutcome {
                        last,
                        complete: false,
                    };
                }
                last = Some(durable);
                update_progress(journal, wm, lag, alarm_bytes, follower, durable);
            }
            Err(e) => {
                tracing::warn!(follower, ?e, "chain: follower append failed");
                return PushOutcome {
                    last,
                    complete: false,
                };
            }
        }
    }
    PushOutcome {
        last,
        complete: true,
    }
}

/// An in-process chain transport: pushes committed records straight into a
/// follower's [`ChainSink`].
///
/// Used for tests and single-process cluster harnesses where the "follower" is
/// another journal in the same process. It reports the sink's watermark after
/// each append.
pub struct MemChainTransport {
    sink: Arc<dyn ChainSink>,
}

impl MemChainTransport {
    /// A transport writing into `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn ChainSink>) -> Self {
        Self { sink }
    }
}

#[async_trait::async_trait]
impl ChainTransport for MemChainTransport {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        self.sink.append(record).await
    }

    async fn follower_watermark(&self) -> Option<Lsn> {
        self.sink.watermark().await
    }
}

/// A [`ChainSink`] that appends replicated records to a [`Journal`].
///
/// This is the standard follower-side sink: it journals each replicated record
/// durably (so the follower can serve as the recovery source) and tracks the
/// highest *origin* LSN persisted. The journal write goes through
/// [`Journal::append_replicated`], which keeps the record out of the
/// follower's own replication broadcast — otherwise a ring of nodes echoes
/// every record around forever.
pub struct JournalChainSink {
    journal: Arc<Journal>,
    watermark: std::sync::Mutex<Option<Lsn>>,
}

impl JournalChainSink {
    /// A sink appending into `journal`.
    #[must_use]
    pub fn new(journal: Arc<Journal>) -> Self {
        Self {
            journal,
            watermark: std::sync::Mutex::new(None),
        }
    }

    /// The highest origin LSN durably persisted on the follower journal.
    #[must_use]
    pub fn watermark(&self) -> Option<Lsn> {
        *self.watermark.lock().expect("sink watermark lock")
    }
}

#[async_trait::async_trait]
impl ChainSink for JournalChainSink {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        // The record's own (origin) LSN becomes the watermark — this is what
        // makes the primary's lag computation meaningful. `append_replicated`
        // stamps the *local* LSN into the record's key but the record's
        // origin LSN is what the sink reports.
        let origin_lsn = record.lsn;
        let handle = self.journal.append_replicated(record)?;
        handle.committed().await?;
        *self.watermark.lock().expect("sink watermark lock") = Some(origin_lsn);
        Ok(origin_lsn)
    }

    async fn watermark(&self) -> Option<Lsn> {
        self.watermark()
    }
}
