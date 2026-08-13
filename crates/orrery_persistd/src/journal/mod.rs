//! The per-node segmented append-only journal (docs/08-persistence.md §4).
//!
//! One journal per persistd node, shared by all cell actors on that node — one
//! fsync stream per disk is the point. Records are keyed by [`Lsn`] (segment
//! sequence + byte offset) and appended via an [`AppendHandle`] that resolves
//! only after an adaptive group fsync (§4, D16: journal commit < 2 ms
//! server-internal).

pub mod chain;
pub mod fjall;
mod group_commit;

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

pub use chain::{
    spawn_chain, ChainConfig, ChainReplicator, ChainSink, ChainTransport, JournalChainSink,
    MemChainTransport,
};
pub use fjall::Journal;
pub use group_commit::{AdaptiveCommitMode, GroupCommitConfig};

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
    /// Resolved by the committer after flush. `Err` means the write never
    /// became durable (e.g. the journal is shutting down).
    done: std::sync::Arc<tokio::sync::Notify>,
    result: std::sync::Mutex<Option<Result<Lsn, JournalError>>>,
}

impl AppendHandle {
    pub(crate) fn new(lsn: Lsn) -> Self {
        Self {
            lsn,
            done: std::sync::Arc::new(tokio::sync::Notify::new()),
            result: std::sync::Mutex::new(None),
        }
    }

    fn resolve(&self, result: Result<Lsn, JournalError>) {
        *self.result.lock().expect("handle lock") = Some(result);
        self.done.notify_waiters();
    }

    /// Wait until this append is durably flushed.
    pub async fn committed(&self) -> Result<Lsn, JournalError> {
        loop {
            {
                let guard = self.result.lock().expect("handle lock");
                if let Some(r) = guard.as_ref() {
                    return r.clone();
                }
            }
            self.done.notified().await;
        }
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
