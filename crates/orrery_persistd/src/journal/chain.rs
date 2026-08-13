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
/// The task subscribes to the journal's committed-record broadcast. If it falls
/// behind (the bounded channel reports a gap), it rescans the journal from its
/// last-known watermark so no committed record is skipped — the broadcast is a
/// fast path, the rescan is the correctness backstop. Each transport round trip
/// carries up to [`ChainConfig::batch_max`] records. Lag is reported through
/// the returned [`ChainReplicator`]'s watermark and lag gauge; a
/// `tracing::warn!` fires whenever the lag exceeds [`ChainConfig::lag_alarm`]
/// records.
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
        let mut last: Option<Lsn> = None;
        loop {
            // The flag is checked at every await boundary: `push_batch`'s
            // per-append select races shutdown too, so a permit-style wakeup
            // could be consumed there and missed here.
            if sd.armed() {
                break;
            }
            // Fill one transport round trip with up to `batch_max` broadcast
            // records. The first `recv().await` suspends until a record (or
            // shutdown); the subsequent `try_recv` sweep drains whatever else
            // is already committed, so a busy journal batches instead of
            // paying one round trip per record.
            let mut batch: Vec<JournalRecord> = Vec::new();
            let mut rescans: usize = 0;
            let recv = tokio::select! {
                r = rx.recv() => r,
                _ = sd.wake.notified() => break,
            };
            match recv {
                Ok(record) => batch.push(record),
                Err(broadcast::error::RecvError::Lagged(_)) => rescans += 1,
                Err(broadcast::error::RecvError::Closed) => break,
            }
            while batch.len() + rescans < batch_max {
                match rx.try_recv() {
                    Ok(record) => batch.push(record),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => rescans += 1,
                    Err(broadcast::error::TryRecvError::Empty)
                    | Err(broadcast::error::TryRecvError::Closed) => break,
                }
            }

            // Fell behind the bounded channel: rescan from the last durable
            // watermark so nothing is skipped. The scan iterator is not
            // `Send`, so collect it before awaiting.
            if rescans > 0 {
                let from = last.unwrap_or(Lsn::new(0, 0));
                let records: Vec<JournalRecord> = journal
                    .scan_from(from)
                    .filter_map(|item| item.ok())
                    .map(|stored| stored.record)
                    .collect();
                for chunk in records.chunks(batch_max) {
                    let pushed = push_batch(
                        &journal,
                        &*transport,
                        chunk,
                        &sd,
                        &wm,
                        &lag,
                        alarm_bytes,
                        follower,
                    )
                    .await;
                    if let Some(l) = pushed.last() {
                        last = Some(l);
                    }
                    if !pushed.complete() {
                        break;
                    }
                }
            } else if batch.is_empty() {
                continue;
            } else {
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
                if let Some(l) = pushed.last() {
                    last = Some(l);
                }
                // On an incomplete push (transport error or shutdown), back
                // off; the flag check at the top of the loop exits on
                // shutdown, and the rescan path re-sends from the last
                // durable watermark on error.
                if !pushed.complete() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
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

/// Push one batch of records to the follower: one `ChainTransport::append`
/// round trip per record, capped at `batch_max` records per outer-loop
/// iteration by the caller. `shutdown` races every append so a stalled
/// transport cannot wedge `ChainReplicator::shutdown` forever. Lag bookkeeping
/// runs after **each** record — a follower that stalls mid-batch is exactly
/// the case the lag alarm exists for, so the gauge cannot wait for the batch
/// to complete.
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
            Ok(_) => {
                last = Some(record.lsn);
                // The follower's watermark is an *origin* LSN (the sink
                // echoes the record's own lsn), so it subtracts cleanly from
                // this journal's committed cursor.
                if let Some(w) = transport.follower_watermark().await {
                    *wm.lock().expect("chain watermark lock") = Some(w);
                    if let Some(l) = lag_in_bytes(journal.committed(), Some(w)) {
                        lag.store(l, std::sync::atomic::Ordering::Release);
                        if l > alarm_bytes {
                            tracing::warn!(
                                follower,
                                lag_bytes = l,
                                alarm_bytes,
                                "chain: replication lag exceeds lag_alarm"
                            );
                        }
                    }
                }
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
