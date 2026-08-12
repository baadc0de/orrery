//! The fjall-backed segmented append-only journal (`journal-fjall`, default).
//!
//! Records are stored in a single fjall `Database` with **manual journal
//! persistence** — the group committer issues `db.persist(SyncData)`
//! (`fdatasync`) when it decides to, which is exactly the control the adaptive
//! group-commit design needs (docs/08-persistence.md §4).
//!
//! Keys are the [`Lsn`] `(segment, offset)` encoded big-endian, so byte order
//! equals LSN order and a replay is one forward range scan. Records carry their
//! payload crc (in [`JournalRecord::crc`]); replay re-verifies it.
//!
//! The "segment" is a logical unit inside the LSN (a segment seq advances when
//! cumulative bytes cross [`JournalConfig::segment_size`]); it is **not** a raw
//! 128 MiB file in this slice — the `journal-raw` feature is the planned
//! hand-rolled segment-file backend (D11 offers either).

use std::sync::Arc;

use fjall::{Database, Keyspace, PersistMode};

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

use crate::journal::group_commit::{spawn_committer, CommitterHandle};
use crate::journal::{AppendHandle, JournalConfig, JournalError, JournalScan, StoredRecord};

/// The fjall keyspace holding LSN-keyed journal records.
const RECORDS_KS: &str = "records";
/// The fjall keyspace holding per-segment metadata (future: cell index footers).
const SEGMENTS_KS: &str = "segments";

/// The per-node journal (docs/08-persistence.md §4).
pub struct Journal {
    db: Database,
    records: Keyspace,
    /// Monotonic next-LSN cursor, guarded by a mutex (segment advances too).
    cursor: std::sync::Mutex<Lsn>,
    segment_size: u64,
    committer: CommitterHandle,
    closed: std::sync::atomic::AtomicBool,
}

impl Journal {
    /// Open (or reopen) a journal in `cfg.dir`, starting a group-commit
    /// committer task.
    pub fn open(config: &JournalConfig) -> Result<Self, JournalError> {
        let db = Database::builder(&config.dir)
            .manual_journal_persist(true)
            .open()
            .map_err(|e| JournalError::Store(format!("open db: {e}")))?;
        let records = db
            .keyspace(RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open records ks: {e}")))?;
        let _segments = db
            .keyspace(SEGMENTS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open segments ks: {e}")))?;

        // Recover the next LSN from the last stored record so a reopened
        // journal continues without collisions and replay starts correctly.
        let next_lsn = next_lsn_after(&records)?;

        let db_flush = Arc::new({
            let db = db.clone();
            move || {
                db.persist(PersistMode::SyncData)
                    .map_err(|e| JournalError::Store(e.to_string()))
            }
        });

        let committer = spawn_committer(config.commit.clone(), db_flush);

        Ok(Self {
            db,
            records,
            cursor: std::sync::Mutex::new(next_lsn),
            segment_size: 128 * 1024 * 1024,
            committer,
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Append a record and return a handle that resolves once the record is
    /// durably flushed (the ack; §2.1).
    pub fn append(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, JournalError> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(JournalError::Closed);
        }

        let payload_bytes = record.payload.len();
        if payload_bytes > (u32::MAX as usize) {
            return Err(JournalError::PayloadTooLarge(payload_bytes));
        }

        // Assign the next LSN and encode the record under a single lock so LSN
        // assignment, segment advance, and the on-disk key stay consistent.
        let (lsn, key, encoded) = {
            let mut cursor = self.cursor.lock().expect("journal cursor lock");
            let span = encoded_len(&record);
            let lsn = advance(&mut cursor, span, self.segment_size);
            let key = lsn_key(lsn);
            let encoded = postcard::to_stdvec(&record)
                .map_err(|e| JournalError::Store(format!("encode record: {e}")))?;
            (lsn, key, encoded)
        };

        self.records
            .insert(key, encoded)
            .map_err(|e| JournalError::Store(format!("insert record: {e}")))?;

        let handle = Arc::new(AppendHandle::new(lsn));
        self.committer.submit(handle.clone(), payload_bytes);
        Ok(handle)
    }

    /// The highest LSN durably flushed so far.
    pub fn committed(&self) -> Lsn {
        self.committer.committed()
    }

    /// The number of fsyncs issued since open (test hook).
    #[cfg(test)]
    pub fn flush_count(&self) -> usize {
        self.committer.flush_count()
    }

    /// Scan records with `lsn >= from` in LSN order.
    ///
    /// Replay reads this forward scan and (in the actor) applies each record,
    /// discarding superseded epochs and re-verifying crc (§3.4, §4.1).
    pub fn scan_from<'a>(&'a self, from: Lsn) -> JournalScan<'a> {
        let start = lsn_key(from);
        let iter = self.records.range(start..).map(decode_kv);
        JournalScan {
            iter: Box::new(iter),
        }
    }

    /// Flush any pending appends and stop the committer, then close the store.
    ///
    /// Awaits the committer task's exit so its store clone (and the file lock)
    /// is released before returning — required before reopening the same dir.
    pub async fn close(&self) -> Result<(), JournalError> {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        self.committer.shutdown();
        self.committer.wait_exit().await;
        self.db
            .persist(PersistMode::SyncData)
            .map_err(|e| JournalError::Store(format!("final persist: {e}")))
    }

    /// Whether the journal is closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Recover the next LSN from the last record stored, or a fresh start.
///
/// This is an approximation: it advances past the last record's value length.
/// Segment boundaries are not correctness-critical — only monotonicity of the
/// LSN is — so the next append recomputes the exact position.
fn next_lsn_after(records: &Keyspace) -> Result<Lsn, JournalError> {
    let Some(last) = records.iter().next_back() else {
        return Ok(Lsn::new(0, 0));
    };
    let kv = last
        .into_inner()
        .map_err(|e| JournalError::Store(format!("last record: {e}")))?;
    let lsn = parse_lsn_key(kv.0.as_ref()).ok_or_else(|| JournalError::Corrupt {
        lsn: Lsn::new(0, 0),
        msg: "unparseable last record key".into(),
    })?;
    Ok(advance_from(lsn, kv.1.len() as u64))
}

/// Encode an LSN as a 16-byte big-endian key (byte order == LSN order).
fn lsn_key(lsn: Lsn) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&lsn.segment.to_be_bytes());
    key[8..16].copy_from_slice(&lsn.offset.to_be_bytes());
    key
}

/// Decode a 16-byte LSN key.
fn parse_lsn_key(key: &[u8]) -> Option<Lsn> {
    if key.len() != 16 {
        return None;
    }
    let mut seg = [0u8; 8];
    let mut off = [0u8; 8];
    seg.copy_from_slice(&key[0..8]);
    off.copy_from_slice(&key[8..16]);
    Some(Lsn {
        segment: u64::from_be_bytes(seg),
        offset: u64::from_be_bytes(off),
    })
}

/// Advance a cursor past `span` bytes of encoded record, rolling into a new
/// segment when the current segment would exceed `segment_size`. Returns the
/// LSN to use for the next record.
fn advance(cursor: &mut Lsn, span: u64, segment_size: u64) -> Lsn {
    let lsn = *cursor;
    let next_offset = lsn.offset + span;
    if next_offset > segment_size {
        cursor.segment += 1;
        cursor.offset = 0;
    } else {
        cursor.offset = next_offset;
    }
    lsn
}

fn advance_from(lsn: Lsn, bytes: u64) -> Lsn {
    Lsn {
        segment: lsn.segment,
        offset: lsn.offset + bytes,
    }
}

/// The encoded byte length of a record (approximation of its on-disk span).
fn encoded_len(record: &JournalRecord) -> u64 {
    // Exact postcard length would require encoding twice; approximate with the
    // payload length plus a fixed header. Segment boundaries are not
    // correctness-critical — only monotonicity of the LSN is.
    (record.payload.len() + 64) as u64
}

fn decode_kv(kv: fjall::Guard) -> Result<StoredRecord, JournalError> {
    let kv = kv
        .into_inner()
        .map_err(|e| JournalError::Store(format!("scan: {e}")))?;
    let lsn = parse_lsn_key(kv.0.as_ref()).ok_or_else(|| JournalError::Corrupt {
        lsn: Lsn::new(0, 0),
        msg: "unparseable key in scan".into(),
    })?;
    let record: JournalRecord =
        postcard::from_bytes(kv.1.as_ref()).map_err(|e| JournalError::Corrupt {
            lsn,
            msg: format!("decode: {e}"),
        })?;
    Ok(StoredRecord { lsn, record })
}
