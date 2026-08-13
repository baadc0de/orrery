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
//! LSN it has durably persisted — the follower watermark that bounds RPO.

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
/// highest LSN durably persisted. The sink is the *only* thing the transport
/// writes to on the follower side.
#[async_trait::async_trait]
pub trait ChainSink: Send + Sync {
    /// Durably persist `record` and return the follower's new watermark (the
    /// highest LSN now durable on the follower).
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError>;

    /// The follower's durable watermark (highest LSN persisted), if any.
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
    /// watermark after this record.
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError>;

    /// The follower's durable watermark (highest LSN persisted on the
    /// follower), or `None` if unknown.
    async fn follower_watermark(&self) -> Option<Lsn>;
}

/// A running chain-replication task: subscribes to the primary journal's
/// committed records and pushes them to the follower.
pub struct ChainReplicator {
    /// The follower's durable watermark, updated as the transport reports it.
    watermark: Arc<std::sync::Mutex<Option<Lsn>>>,
    /// Join handle; awaited on shutdown.
    join: tokio::task::JoinHandle<()>,
    /// Signals the task to stop.
    shutdown: Arc<tokio::sync::Notify>,
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

    /// Stop the replicator task, awaiting its exit.
    pub async fn shutdown(self) {
        // `notify_one` stores a permit if the task is not currently waiting, so
        // the shutdown signal is never lost (unlike `notify_waiters`). There is
        // exactly one replicator task, so one permit is exactly right.
        self.shutdown.notify_one();
        let _ = self.join.await;
    }
}

/// Spawn a chain-replication task streaming `journal`'s committed records to
/// `transport` (which writes into the follower's [`ChainSink`]).
///
/// The task subscribes to the journal's committed-record broadcast. If it falls
/// behind (the bounded channel reports a gap), it rescans the journal from its
/// last-known watermark so no committed record is skipped — the broadcast is a
/// fast path, the rescan is the correctness backstop. Lag is reported through
/// the returned [`ChainReplicator`]'s watermark.
pub fn spawn_chain(
    journal: Arc<Journal>,
    transport: Arc<dyn ChainTransport>,
    config: &ChainConfig,
) -> ChainReplicator {
    let watermark = Arc::new(std::sync::Mutex::new(None));
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let follower = config.follower;
    let mut rx = journal.subscribe();
    let wm = Arc::clone(&watermark);
    let shutdown_task = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        let mut last: Option<Lsn> = None;
        loop {
            // Try the fast path: the next committed record, or shutdown.
            let recv = tokio::select! {
                r = rx.recv() => r,
                _ = shutdown_task.notified() => break,
            };
            match recv {
                Ok(record) => {
                    if let Err(e) = transport.append(record.clone()).await {
                        tracing::warn!(follower, ?e, "chain: follower append failed");
                        // Back off and retry; the rescan path below re-sends
                        // from the last durable watermark.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    last = Some(record.lsn);
                    if let Some(w) = transport.follower_watermark().await {
                        *wm.lock().expect("chain watermark lock") = Some(w);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Fell behind the bounded channel: rescan from the last
                    // durable watermark so nothing is skipped. The scan
                    // iterator is not `Send`, so collect it before awaiting.
                    let from = last.unwrap_or(Lsn::new(0, 0));
                    let records: Vec<JournalRecord> = journal
                        .scan_from(from)
                        .filter_map(|item| item.ok())
                        .map(|stored| stored.record)
                        .collect();
                    for record in records {
                        if let Err(e) = transport.append(record.clone()).await {
                            tracing::warn!(follower, ?e, "chain: rescan append failed");
                            break;
                        }
                        last = Some(record.lsn);
                    }
                    if let Some(w) = transport.follower_watermark().await {
                        *wm.lock().expect("chain watermark lock") = Some(w);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    ChainReplicator {
        watermark,
        join,
        shutdown,
        follower,
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
/// highest LSN persisted.
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

    /// The highest LSN durably persisted on the follower journal.
    #[must_use]
    pub fn watermark(&self) -> Option<Lsn> {
        *self.watermark.lock().expect("sink watermark lock")
    }
}

#[async_trait::async_trait]
impl ChainSink for JournalChainSink {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        let handle = self.journal.append(record)?;
        let lsn = handle.committed().await?;
        *self.watermark.lock().expect("sink watermark lock") = Some(lsn);
        Ok(lsn)
    }

    async fn watermark(&self) -> Option<Lsn> {
        self.watermark()
    }
}
