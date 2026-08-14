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
use tokio::sync::broadcast;

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

use crate::journal::group_commit::{spawn_committer, CommitterHandle};
use crate::journal::{AppendHandle, JournalConfig, JournalError, JournalScan, StoredRecord};

/// The number of committed records buffered for chain-replication subscribers
/// before the channel reports lag. Subscribers that fall behind rescan the
/// journal from their watermark, so this bounds memory, not correctness.
const PUBLISH_CAPACITY: usize = 4096;
#[cfg(feature = "chain-grpc")]
const CHAIN_RECORDS_KS: &str = "chain_records";
#[cfg(feature = "chain-grpc")]
const CHAIN_STATE_KS: &str = "chain_state";

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
    /// Committed records, published for chain replication (§4).
    published: broadcast::Sender<JournalRecord>,
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

        let (published, _) = broadcast::channel(PUBLISH_CAPACITY);
        let committer = spawn_committer(config.commit.clone(), db_flush, published.clone());

        Ok(Self {
            db,
            records,
            cursor: std::sync::Mutex::new(next_lsn),
            segment_size: 128 * 1024 * 1024,
            committer,
            closed: std::sync::atomic::AtomicBool::new(false),
            published,
        })
    }

    /// Append a record and return a handle that resolves once the record is
    /// durably flushed (the ack; §2.1).
    ///
    /// Takes the record by value: its `lsn` field is meaningless on entry
    /// (callers conventionally pass `Lsn::new(0, 0)`) and is **assigned here**
    /// — the stored record is encoded *after* stamping, so the encoded bytes
    /// and the record's key agree and a replayed record knows its own LSN.
    pub fn append(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, JournalError> {
        self.append_inner(record, true)
    }

    /// Append a record that arrived via chain replication from another node
    /// (docs/08-persistence.md §4, D11).
    ///
    /// Identical to [`Journal::append`] (durable, group-fsynced, resolves with
    /// the follower's local LSN) except the committed record is **not**
    /// published to this journal's replication broadcast: without this, a
    /// 2-node ring echoes every replicated record back to its origin forever.
    /// The record's *origin* LSN is preserved in the record itself, which the
    /// replicator echoes to the origin as the follower watermark.
    pub fn append_replicated(
        &self,
        record: JournalRecord,
    ) -> Result<Arc<AppendHandle>, JournalError> {
        self.append_inner(record, false)
    }

    /// Atomically stage a mirrored record and its chain-scoped provenance.
    ///
    /// The provenance key is `(durable_chain_id, origin_lsn)`.  Returning
    /// `None` means that exact origin record is already present, making a
    /// retry harmless.  The journal row and provenance row enter fjall in one
    /// write batch and become durable in the same group fsync.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn append_replicated_indexed(
        &self,
        record: JournalRecord,
        chain_key: &[u8],
        provenance: &[u8],
    ) -> Result<Option<Arc<AppendHandle>>, JournalError> {
        let origin = record.lsn;
        let index_key = chain_record_key(chain_key, origin);
        let index = self.chain_records()?;
        if index
            .contains_key(&index_key)
            .map_err(|e| JournalError::Store(format!("read chain record index: {e}")))?
        {
            return Ok(None);
        }
        self.append_inner_with_index(record, false, Some((index, index_key, provenance)))
            .map(Some)
    }

    /// Compute the primary LSN that must immediately follow `record`.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_successor(&self, record: &JournalRecord) -> Lsn {
        let mut next = record.lsn;
        advance(&mut next, encoded_len(record), self.segment_size);
        next
    }

    fn append_inner(
        &self,
        record: JournalRecord,
        publish: bool,
    ) -> Result<Arc<AppendHandle>, JournalError> {
        self.append_inner_with_index(record, publish, None)
    }

    fn append_inner_with_index(
        &self,
        mut record: JournalRecord,
        publish: bool,
        #[cfg(feature = "chain-grpc")] index: Option<(Keyspace, Vec<u8>, &[u8])>,
        #[cfg(not(feature = "chain-grpc"))] _index: Option<()>,
    ) -> Result<Arc<AppendHandle>, JournalError> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(JournalError::Closed);
        }

        let payload_bytes = record.payload.len();
        if payload_bytes > (u32::MAX as usize) {
            return Err(JournalError::PayloadTooLarge(payload_bytes));
        }

        // Assign the next LSN and encode the record under a single lock so LSN
        // assignment, segment advance, and the on-disk key stay consistent.
        // The assigned LSN is stamped into the record *before* encoding, so
        // the stored bytes and the key agree (checkpoint watermarks and chain
        // lag both read `record.lsn`).
        let (lsn, key, encoded) = {
            let origin_lsn = record.lsn;
            let mut cursor = self.cursor.lock().expect("journal cursor lock");
            let span = encoded_len(&record);
            let lsn = advance(&mut cursor, span, self.segment_size);
            record.lsn = lsn;
            let key = lsn_key(lsn);
            if !publish {
                record.lsn = origin_lsn;
            }
            let encoded = postcard::to_stdvec(&record)
                .map_err(|e| JournalError::Store(format!("encode record: {e}")))?;
            (lsn, key, encoded)
        };

        #[cfg(feature = "chain-grpc")]
        if let Some((index, index_key, provenance)) = index {
            let mut batch = self.db.batch();
            batch.insert(&self.records, key, encoded);
            batch.insert(&index, index_key, provenance);
            batch
                .commit()
                .map_err(|e| JournalError::Store(format!("insert mirrored record: {e}")))?;
        } else {
            self.records
                .insert(key, encoded)
                .map_err(|e| JournalError::Store(format!("insert record: {e}")))?;
        }
        #[cfg(not(feature = "chain-grpc"))]
        self.records
            .insert(key, encoded)
            .map_err(|e| JournalError::Store(format!("insert record: {e}")))?;

        let handle = Arc::new(AppendHandle::new(lsn));
        self.committer
            .submit(handle.clone(), payload_bytes, record, publish);
        Ok(handle)
    }

    /// The highest LSN durably flushed so far.
    pub fn committed(&self) -> Lsn {
        self.committer.committed()
    }

    /// The number of fsyncs issued since open (§4 group-commit observability:
    /// the count that proves adaptive batching is engaging).
    pub fn flush_count(&self) -> usize {
        self.committer.flush_count()
    }

    /// Subscribe to durably-flushed records, for chain replication (§4).
    ///
    /// The receiver yields each committed record in LSN order. If the receiver
    /// falls behind the bounded channel, it is notified of the gap and must
    /// rescan the journal from its watermark (see [`Journal::scan_from`]).
    pub fn subscribe(&self) -> broadcast::Receiver<JournalRecord> {
        self.published.subscribe()
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

    #[cfg(feature = "chain-grpc")]
    fn chain_records(&self) -> Result<Keyspace, JournalError> {
        self.db
            .keyspace(CHAIN_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open chain record index: {e}")))
    }

    /// Read all provenance rows for exactly one durable chain.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_records(
        &self,
        chain_key: &[u8],
    ) -> Result<Vec<(Lsn, Vec<u8>)>, JournalError> {
        let index = self.chain_records()?;
        let prefix = chain_record_prefix(chain_key);
        let end = prefix_successor(&prefix).ok_or_else(|| {
            JournalError::Store("chain identity has no finite key-range successor".into())
        })?;
        index
            .range(prefix.clone()..end)
            .map(|entry| {
                let entry = entry
                    .into_inner()
                    .map_err(|e| JournalError::Store(format!("scan chain index: {e}")))?;
                let suffix = &entry.0.as_ref()[prefix.len()..];
                let lsn = parse_lsn_key(suffix).ok_or_else(|| JournalError::Corrupt {
                    lsn: Lsn::new(0, 0),
                    msg: "invalid origin LSN in chain index".into(),
                })?;
                Ok((lsn, entry.1.as_ref().to_vec()))
            })
            .collect()
    }

    /// Read provenance for one chain/origin dedupe key.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_record(
        &self,
        chain_key: &[u8],
        origin: Lsn,
    ) -> Result<Option<Vec<u8>>, JournalError> {
        self.chain_records()?
            .get(chain_record_key(chain_key, origin))
            .map(|value| value.map(|value| value.as_ref().to_vec()))
            .map_err(|e| JournalError::Store(format!("read chain record provenance: {e}")))
    }

    /// Load opaque cursor state for exactly one durable chain.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_state(
        &self,
        chain_key: &[u8],
    ) -> Result<Option<Vec<u8>>, JournalError> {
        let state = self
            .db
            .keyspace(CHAIN_STATE_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open chain state: {e}")))?;
        state
            .get(chain_key)
            .map(|value| value.map(|value| value.as_ref().to_vec()))
            .map_err(|e| JournalError::Store(format!("read chain state: {e}")))
    }

    /// Persist opaque cursor state after its referenced records are durable.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn set_chain_grpc_state(
        &self,
        chain_key: &[u8],
        value: &[u8],
    ) -> Result<(), JournalError> {
        let state = self
            .db
            .keyspace(CHAIN_STATE_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open chain state: {e}")))?;
        state
            .insert(chain_key, value)
            .map_err(|e| JournalError::Store(format!("write chain state: {e}")))?;
        self.db
            .persist(PersistMode::SyncData)
            .map_err(|e| JournalError::Store(format!("persist chain state: {e}")))
    }
}

#[cfg(feature = "chain-grpc")]
fn chain_record_prefix(chain_key: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + chain_key.len());
    key.extend_from_slice(&(chain_key.len() as u32).to_be_bytes());
    key.extend_from_slice(chain_key);
    key
}

#[cfg(feature = "chain-grpc")]
fn chain_record_key(chain_key: &[u8], lsn: Lsn) -> Vec<u8> {
    let mut key = chain_record_prefix(chain_key);
    key.extend_from_slice(&lsn_key(lsn));
    key
}

#[cfg(feature = "chain-grpc")]
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    let position = end.iter().rposition(|byte| *byte != u8::MAX)?;
    end[position] += 1;
    end.truncate(position + 1);
    Some(end)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::GroupCommitConfig;

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
    async fn appended_record_carries_its_own_lsn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&JournalConfig {
            dir: dir.path().to_path_buf(),
            commit: GroupCommitConfig::default(),
        })
        .expect("open journal");

        for i in 0..3u64 {
            let handle = journal.append(mk_record(i)).expect("append");
            handle.committed().await.expect("commit");
        }

        let stored: Vec<StoredRecord> = journal
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<_, _>>()
            .expect("scan");
        assert_eq!(stored.len(), 3);
        for item in &stored {
            assert_eq!(
                item.record.lsn, item.lsn,
                "encoded record lsn must match its key"
            );
        }
        assert!(
            stored[0].lsn < stored[1].lsn && stored[1].lsn < stored[2].lsn,
            "LSNs strictly increasing: {:?}",
            stored.iter().map(|s| s.lsn).collect::<Vec<_>>()
        );

        journal.close().await.expect("close");
    }
}
