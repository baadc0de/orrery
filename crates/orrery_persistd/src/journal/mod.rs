//! The per-node segmented append-only journal (docs/08-persistence.md §4).
//!
//! One journal per persistd node, shared by all cell actors on that node — one
//! fsync stream per disk is the point. Records are keyed by [`Lsn`] (segment
//! sequence + byte offset) and appended via an [`AppendHandle`] that resolves
//! only after an adaptive group fsync (§4, D16: journal commit < 2 ms
//! server-internal).

pub mod chain;
#[cfg(feature = "chain-grpc")]
pub mod chain_grpc;
pub mod fjall;
mod group_commit;
mod metrics;

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

#[cfg(feature = "chain-grpc")]
pub use chain::{spawn_adopted_chain, AdoptedChainHistory};
pub use chain::{
    spawn_chain, ChainConfig, ChainReplicator, ChainSink, ChainTransport, JournalChainSink,
    MemChainTransport,
};
#[cfg(feature = "chain-grpc")]
pub use chain_grpc::{spawn_chain_grpc, ChainGrpcServer, DurableChainId, GrpcChainTransport};
pub use fjall::Journal;
pub use group_commit::{AdaptiveCommitMode, GroupCommitConfig};
pub use metrics::{
    JournalCommitMetrics, JournalCommitSample, JournalCommitSnapshot, JournalStageSnapshot,
};

/// Configuration for a node's [`Journal`].
#[derive(Debug, Clone)]
pub struct JournalConfig {
    /// Directory holding the journal backing store.
    pub dir: std::path::PathBuf,
    /// Adaptive group-commit parameters.
    pub commit: GroupCommitConfig,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            dir: std::path::PathBuf::from("journal"),
            commit: GroupCommitConfig::default(),
        }
    }
}

/// A handle representing one pending journal append.
///
/// Created by [`Journal::append`] and resolved by the group committer once the
/// record is durably flushed. This is the ack the cell actor returns to its
/// client (§2.1: the ack *is* the durability contract).
#[derive(Debug)]
pub struct AppendHandle {
    lsn: Lsn,
    /// Measurement starts at `Journal::append` entry and completes when the
    /// committer resolves this durable append.
    started: std::time::Instant,
    metrics: metrics::SharedJournalCommitMetrics,
    /// Completion state, shared with the committer: `Some(result)` once the
    /// record's batch is durably flushed (`Err` means the write never became
    /// durable, e.g. the journal is shutting down).
    state: std::sync::Arc<AppendHandleState>,
}

#[derive(Debug)]
struct AppendHandleState {
    /// Set by the committer's `resolve`, read by `committed`.
    result: std::sync::Mutex<Option<Result<Lsn, JournalError>>>,
    /// Wakes the waiter. `notify_one` (not `notify_waiters`) stores a permit
    /// when no waiter is registered, so a resolve that lands before the
    /// waiter suspends is never lost.
    done: tokio::sync::Notify,
}

impl AppendHandle {
    pub(crate) fn new(
        lsn: Lsn,
        started: std::time::Instant,
        metrics: metrics::SharedJournalCommitMetrics,
    ) -> Self {
        Self {
            lsn,
            started,
            metrics,
            state: std::sync::Arc::new(AppendHandleState {
                result: std::sync::Mutex::new(None),
                done: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Construct an already-durable handle for router adapters that do not
    /// own a journal (principally deterministic gateway harnesses).
    ///
    /// Production cell actors return handles created by [`Journal::append`];
    /// this constructor exists so alternate [`crate::Router`] implementations
    /// can still satisfy the same durability-handle contract.
    #[must_use]
    pub fn completed(lsn: Lsn) -> std::sync::Arc<Self> {
        let handle = std::sync::Arc::new(Self::new(
            lsn,
            std::time::Instant::now(),
            std::sync::Arc::new(metrics::JournalCommitMetrics::new()),
        ));
        handle.resolve(Ok(lsn));
        handle
    }

    fn resolve(&self, result: Result<Lsn, JournalError>) {
        if result.is_ok() {
            self.metrics.record(self.started.elapsed());
        }
        *self.state.result.lock().expect("handle lock") = Some(result);
        self.state.done.notify_one();
    }

    /// Wait until this append is durably flushed.
    ///
    /// The lost-wakeup window of the previous `Mutex + Notify` pair is closed
    /// by construction: the `Notified` future is created (and thus registered
    /// with the `Notify`) **before** the result is re-checked, so a `resolve`
    /// landing between the check and the suspend either finds the future
    /// already registered or leaves the `notify_one` permit for it. Either
    /// way the waiter wakes and observes the stored result.
    pub async fn committed(&self) -> Result<Lsn, JournalError> {
        // 1. Fast path: already resolved.
        if let Some(r) = self.state.result.lock().expect("handle lock").as_ref() {
            return r.clone();
        }
        // 2. Register interest, THEN re-check. Any resolve after the
        //    registration notifies this future (or stores the permit it will
        //    consume); any resolve before it is visible in the re-check.
        //    `pin!` gives the `Notified` an address so it stays registered
        //    across the re-check and the await.
        let notified = std::pin::pin!(self.state.done.notified());
        if let Some(r) = self.state.result.lock().expect("handle lock").as_ref() {
            return r.clone();
        }
        notified.await;
        self.state
            .result
            .lock()
            .expect("handle lock")
            .clone()
            .unwrap_or(Err(JournalError::Closed))
    }

    /// The LSN assigned to this append (before durability).
    pub fn lsn(&self) -> Lsn {
        self.lsn
    }
}

/// Errors from journal append/read/replay.
#[derive(Debug, Clone)]
pub enum JournalError {
    /// The append never became durable (journal closed).
    Closed,
    /// The append's payload is too large for a single record.
    PayloadTooLarge(usize),
    /// The underlying store failed.
    Store(String),
    /// A stored record failed to decode.
    Corrupt {
        /// The offending record's LSN.
        lsn: Lsn,
        /// A human-readable cause.
        msg: String,
    },
}

impl core::fmt::Display for JournalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => write!(f, "journal is closed"),
            Self::PayloadTooLarge(n) => write!(f, "record payload too large: {n} bytes"),
            Self::Store(s) => write!(f, "store error: {s}"),
            Self::Corrupt { lsn, msg } => write!(f, "corrupt record at {lsn}: {msg}"),
        }
    }
}

impl core::error::Error for JournalError {}

/// A decoded journal record read back from the store.
#[derive(Debug, Clone)]
pub struct StoredRecord {
    /// The record's journal position.
    pub lsn: Lsn,
    /// The record itself.
    pub record: JournalRecord,
}

/// A journal read: iterate records at or after a starting LSN.
pub struct JournalScan<'a> {
    pub(crate) iter: Box<dyn Iterator<Item = Result<StoredRecord, JournalError>> + 'a>,
}

impl Iterator for JournalScan<'_> {
    type Item = Result<StoredRecord, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}
