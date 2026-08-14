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

#[cfg(feature = "chain-grpc")]
use std::collections::HashMap;
use std::sync::Arc;

use fjall::{Database, Keyspace, PersistMode};
use tokio::sync::broadcast;

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

use crate::journal::group_commit::{CommitterHandle, StagedAppend, spawn_committer};
use crate::journal::{
    AppendHandle, JournalCommitMetrics, JournalConfig, JournalError, JournalScan, StoredRecord,
};

/// The number of committed records buffered for chain-replication subscribers
/// before the channel reports lag. Subscribers that fall behind rescan the
/// journal from their watermark, so this bounds memory, not correctness.
const PUBLISH_CAPACITY: usize = 4096;
#[cfg(feature = "chain-grpc")]
const CHAIN_RECORDS_KS: &str = "chain_records";
#[cfg(feature = "chain-grpc")]
const CHAIN_STATE_KS: &str = "chain_state";
#[cfg(feature = "chain-grpc")]
const ADOPTIONS_KS: &str = "chain_adoptions";
#[cfg(feature = "chain-grpc")]
const ADOPTED_RECORDS_KS: &str = "adopted_chain_records";

/// The fjall keyspace holding LSN-keyed journal records.
const RECORDS_KS: &str = "records";
/// LSN keys for records authored by this journal (and therefore eligible for
/// outbound chain replication). Mirrored records are deliberately absent.
const ORIGINATED_RECORDS_KS: &str = "originated_records";
/// Journal-local schema metadata. Kept separate from record data so an
/// interrupted migration can be retried idempotently on the next open.
const JOURNAL_META_KS: &str = "journal_meta";
const ORIGINATED_INDEX_VERSION_KEY: &[u8] = b"originated_records_version";
const ORIGINATED_INDEX_VERSION: &[u8] = b"1";
/// The fjall keyspace holding per-segment metadata (future: cell index footers).
const SEGMENTS_KS: &str = "segments";

/// The per-node journal (docs/08-persistence.md §4).
pub struct Journal {
    db: Database,
    records: Keyspace,
    originated_records: Keyspace,
    /// Monotonic next-LSN cursor, guarded by a mutex (segment advances too).
    cursor: std::sync::Mutex<Lsn>,
    segment_size: u64,
    committer: CommitterHandle,
    closed: std::sync::atomic::AtomicBool,
    /// Fixed-memory append-to-durable-resolve telemetry. Kept separate from
    /// the committer queue so recording never affects batch selection.
    metrics: Arc<JournalCommitMetrics>,
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
        let originated_records = db
            .keyspace(ORIGINATED_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open originated records ks: {e}")))?;
        let metadata = db
            .keyspace(JOURNAL_META_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open journal metadata ks: {e}")))?;
        let _segments = db
            .keyspace(SEGMENTS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open segments ks: {e}")))?;

        // Recover the next LSN from the last stored record so a reopened
        // journal continues without collisions and replay starts correctly.
        migrate_originated_index(&db, &records, &originated_records, &metadata)?;
        let recovered_committed = last_stored_lsn(&records)?;
        let next_lsn = next_lsn_after(&records)?;

        #[cfg(feature = "chain-grpc")]
        let chain_records = db
            .keyspace(CHAIN_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open chain record index: {e}")))?;

        let db_commit = Arc::new({
            let db = db.clone();
            let records = records.clone();
            let originated_records = originated_records.clone();
            #[cfg(feature = "chain-grpc")]
            let chain_records = chain_records.clone();
            move |pending: &[crate::journal::group_commit::Pending]| {
                let mut batch = db.batch();
                for pending in pending {
                    let staged = &pending.staged;
                    batch.insert(&records, &staged.key, &staged.encoded);
                    if staged.originated {
                        batch.insert(&originated_records, &staged.key, b"");
                    }
                    #[cfg(feature = "chain-grpc")]
                    if let Some((key, value)) = &staged.provenance {
                        batch.insert(&chain_records, key, value);
                    }
                }
                batch
                    .commit()
                    .map_err(|e| JournalError::Store(format!("insert journal batch: {e}")))?;
                db.persist(PersistMode::SyncData)
                    .map_err(|e| JournalError::Store(e.to_string()))
            }
        });

        let (published, _) = broadcast::channel(PUBLISH_CAPACITY);
        let metrics = Arc::new(JournalCommitMetrics::new());
        let committer = spawn_committer(
            config.commit.clone(),
            db_commit,
            published.clone(),
            recovered_committed,
        );

        Ok(Self {
            db,
            records,
            originated_records,
            cursor: std::sync::Mutex::new(next_lsn),
            segment_size: 128 * 1024 * 1024,
            committer,
            closed: std::sync::atomic::AtomicBool::new(false),
            metrics,
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
        if self
            .chain_records()?
            .contains_key(&index_key)
            .map_err(|e| JournalError::Store(format!("read chain record index: {e}")))?
        {
            return Ok(None);
        }
        self.append_inner_with_index(record, false, Some((index_key, provenance.to_vec())))
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
        #[cfg(feature = "chain-grpc")] index: Option<(Vec<u8>, Vec<u8>)>,
        #[cfg(not(feature = "chain-grpc"))] _index: Option<()>,
    ) -> Result<Arc<AppendHandle>, JournalError> {
        // Includes journal staging plus queue/batch/fsync time, ending exactly
        // when `AppendHandle::committed` becomes resolvable.
        let started = std::time::Instant::now();
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

        let handle = Arc::new(AppendHandle::new(lsn, started, Arc::clone(&self.metrics)));
        self.committer.submit(
            handle.clone(),
            StagedAppend {
                key: key.to_vec(),
                encoded,
                originated: publish,
                #[cfg(feature = "chain-grpc")]
                provenance: index,
            },
            record,
            publish,
        );
        Ok(handle)
    }

    /// The highest LSN durably flushed so far.
    pub fn committed(&self) -> Lsn {
        self.committer.committed().unwrap_or(Lsn::new(0, 0))
    }

    /// The actual durable boundary, preserving `None` for an empty journal.
    pub(crate) fn committed_watermark(&self) -> Option<Lsn> {
        self.committer.committed()
    }

    /// The number of fsyncs issued since open (§4 group-commit observability:
    /// the count that proves adaptive batching is engaging).
    pub fn flush_count(&self) -> usize {
        self.committer.flush_count()
    }

    /// Fixed-memory D16 journal commit telemetry.
    ///
    /// The returned recorder exposes cumulative snapshots and delta batches
    /// whose `{ value_us, count }` values can be written as P2
    /// `journal_commit_ms` `sample_batch` JSONL records. Reporting is kept
    /// out of the append and group-commit paths.
    #[must_use]
    pub fn commit_metrics(&self) -> Arc<JournalCommitMetrics> {
        Arc::clone(&self.metrics)
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

    /// Scan only records authored by this journal, in local LSN order.
    ///
    /// Outbound chain catch-up uses this index instead of the complete replay
    /// journal so a node that also mirrors another primary never sends those
    /// mirrored records back around a replication ring.
    pub(crate) fn scan_originated_from<'a>(&'a self, from: Lsn) -> JournalScan<'a> {
        let start = lsn_key(from);
        let records = &self.records;
        let iter = self.originated_records.range(start..).map(move |entry| {
            let key = entry
                .key()
                .map_err(|e| JournalError::Store(format!("scan originated records: {e}")))?;
            let value = records
                .get(&key)
                .map_err(|e| JournalError::Store(format!("read originated record: {e}")))?
                .ok_or_else(|| JournalError::Corrupt {
                    lsn: parse_lsn_key(&key).unwrap_or(Lsn::new(0, 0)),
                    msg: "originated record index points to a missing record".into(),
                })?;
            decode_pair(&key, &value)
        });
        JournalScan {
            iter: Box::new(iter),
        }
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn scan_source_from<'a>(
        &'a self,
        source: &crate::journal::chain::ChainSource,
        from: Lsn,
    ) -> Result<JournalScan<'a>, JournalError> {
        match source {
            crate::journal::chain::ChainSource::Originated => Ok(self.scan_originated_from(from)),
            crate::journal::chain::ChainSource::Adopted(history) => {
                self.scan_adopted_from(history, from)
            }
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

    /// Adopt one follower chain after ownership fencing. Provenance must form
    /// one complete, unambiguous prefix; partial tails are never promoted.
    #[cfg(feature = "chain-grpc")]
    pub fn adopt_chain_history(
        &self,
        source: crate::journal::chain_grpc::DurableChainId,
    ) -> Result<crate::journal::chain::AdoptedChainHistory, JournalError> {
        use crate::journal::chain::AdoptedChainHistory;
        use crate::journal::chain_grpc::{
            AdoptedRecord, AdoptionMarker, RecordProvenance, chain_key_for_adoption,
        };
        use std::collections::{BTreeMap, HashMap};

        let source_key = chain_key_for_adoption(&source)?;
        let marker_key = adoption_key(&source_key);
        let markers = self
            .db
            .keyspace(ADOPTIONS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open adoption markers: {e}")))?;
        if let Some(value) = markers
            .get(&marker_key)
            .map_err(|e| JournalError::Store(format!("read adoption marker: {e}")))?
        {
            let marker: AdoptionMarker = postcard::from_bytes(&value)
                .map_err(|e| JournalError::Store(format!("decode adoption marker: {e}")))?;
            if marker.source_key != source_key {
                return Err(JournalError::Store(
                    "adoption marker identity mismatch".into(),
                ));
            }
            return Ok(AdoptedChainHistory::new(source, marker.watermark));
        }

        let mut batches: BTreeMap<u64, Vec<(Lsn, RecordProvenance)>> = BTreeMap::new();
        for (origin, bytes) in self.chain_grpc_records(&source_key)? {
            let provenance: RecordProvenance = postcard::from_bytes(&bytes)
                .map_err(|e| JournalError::Store(format!("decode chain provenance: {e}")))?;
            batches
                .entry(provenance.batch_seq)
                .or_default()
                .push((origin, provenance));
        }

        // Mirrored records retain their source LSN in the encoded value while
        // receiving a follower-local key.  Build that reverse lookup once.
        //
        // Do not use the chain provenance index for this lookup: the check is
        // intentionally against the complete journal, so a local record that
        // collides with a source identity makes promotion ambiguous.  The old
        // implementation performed this full scan for every source record,
        // making promotion O(chain_records * journal_records).  A 30-second
        // P2 run has hundreds of thousands of records, so that prevented the
        // promoted follower from ever becoming ready.
        let mut local_by_origin = HashMap::<Lsn, Option<Lsn>>::new();
        for stored in self.scan_from(Lsn::new(0, 0)).filter_map(Result::ok) {
            match local_by_origin.entry(stored.record.lsn) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Some(stored.lsn));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    // Keep scanning: two local rows for one source identity
                    // are indistinguishable from the repeated scans above and
                    // must reject adoption rather than choose arbitrarily.
                    *entry.get_mut() = None;
                }
            }
        }

        let mut previous = None;
        let mut adopted_records = Vec::new();
        for (expected, (seq, mut batch)) in batches.into_iter().enumerate() {
            if seq != expected as u64 {
                return Err(JournalError::Store(
                    "cannot adopt chain history with a batch gap".into(),
                ));
            }
            batch.sort_by_key(|(_, p)| p.ordinal);
            let Some((_, first)) = batch.first() else {
                return Err(JournalError::Store("cannot adopt empty chain batch".into()));
            };
            if first.predecessor != previous
                || usize::try_from(first.batch_len).ok() != Some(batch.len())
            {
                return Err(JournalError::Store(
                    "cannot adopt discontinuous chain history".into(),
                ));
            }
            for (ordinal, (origin, p)) in batch.iter().enumerate() {
                if p.batch_seq != seq
                    || p.ordinal as usize != ordinal
                    || p.batch_len != first.batch_len
                    || p.predecessor != first.predecessor
                    || p.first_lsn != first.first_lsn
                    || p.last_lsn != first.last_lsn
                    || p.next_lsn != first.next_lsn
                    || (ordinal == 0 && *origin != first.first_lsn)
                    || (ordinal + 1 == batch.len() && *origin != first.last_lsn)
                {
                    return Err(JournalError::Store(
                        "cannot adopt ambiguous chain provenance".into(),
                    ));
                }
                let local = local_by_origin
                    .get(origin)
                    .and_then(|local| *local)
                    .ok_or_else(|| {
                        JournalError::Store("cannot adopt ambiguous source record identity".into())
                    })?;
                adopted_records.push(AdoptedRecord {
                    origin: *origin,
                    local,
                });
            }
            previous = Some(first.last_lsn);
        }
        let marker = AdoptionMarker {
            source_key: source_key.clone(),
            watermark: previous,
            records: adopted_records,
        };
        let adopted = self
            .db
            .keyspace(ADOPTED_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open adopted record index: {e}")))?;
        let mut write = self.db.batch();
        for record in &marker.records {
            write.insert(
                &adopted,
                adoption_record_key(&source_key, record.origin),
                lsn_key(record.local).to_vec(),
            );
        }
        write.insert(
            &markers,
            marker_key,
            postcard::to_stdvec(&marker)
                .map_err(|e| JournalError::Store(format!("encode adoption marker: {e}")))?,
        );
        write
            .commit()
            .map_err(|e| JournalError::Store(format!("write adoption marker: {e}")))?;
        self.db
            .persist(PersistMode::SyncData)
            .map_err(|e| JournalError::Store(format!("persist adoption marker: {e}")))?;
        Ok(AdoptedChainHistory::new(source, previous))
    }

    #[cfg(feature = "chain-grpc")]
    fn scan_adopted_from<'a>(
        &'a self,
        history: &crate::journal::chain::AdoptedChainHistory,
        from: Lsn,
    ) -> Result<JournalScan<'a>, JournalError> {
        use crate::journal::chain_grpc::chain_key_for_adoption;
        let source_key = chain_key_for_adoption(history.source())?;
        let adopted = self
            .db
            .keyspace(ADOPTED_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open adopted record index: {e}")))?;
        let prefix = chain_record_prefix(&source_key);
        let end = prefix_successor(&prefix).ok_or_else(|| {
            JournalError::Store("adopted chain identity has no finite key-range successor".into())
        })?;
        let records = &self.records;
        let iter = adopted
            .range(adoption_record_key(&source_key, from)..end)
            .map(move |entry| {
                let entry = entry
                    .into_inner()
                    .map_err(|e| JournalError::Store(format!("scan adopted record index: {e}")))?;
                let local = parse_lsn_key(entry.1.as_ref())
                    .ok_or_else(|| JournalError::Store("invalid adopted local LSN".into()))?;
                let value = records
                    .get(lsn_key(local))
                    .map_err(|e| JournalError::Store(format!("read adopted record: {e}")))?
                    .ok_or_else(|| JournalError::Corrupt {
                        lsn: local,
                        msg: "adopted record missing".into(),
                    })?;
                decode_pair(&lsn_key(local), &value)
            });
        Ok(JournalScan {
            iter: Box::new(iter),
        })
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
fn adoption_key(chain_key: &[u8]) -> Vec<u8> {
    let mut key = b"adoption/".to_vec();
    key.extend_from_slice(&chain_record_prefix(chain_key));
    key
}

#[cfg(feature = "chain-grpc")]
fn adoption_record_key(chain_key: &[u8], lsn: Lsn) -> Vec<u8> {
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

/// Upgrade journals written before the originated-record index existed.
///
/// Locally authored rows have always stored the same LSN in their key and in
/// the encoded record. Replicated rows preserve the origin LSN in the encoded
/// record while receiving an independent local key, so that equality is the
/// compatibility discriminator. The durable gRPC provenance index resolves
/// the one ambiguous case where an origin LSN happens to equal the follower's
/// local key; an already-present originated row is always trusted because new
/// writes create it atomically with the journal row.
fn migrate_originated_index(
    db: &Database,
    records: &Keyspace,
    originated_records: &Keyspace,
    metadata: &Keyspace,
) -> Result<(), JournalError> {
    if metadata
        .get(ORIGINATED_INDEX_VERSION_KEY)
        .map_err(|e| JournalError::Store(format!("read originated index version: {e}")))?
        .is_some_and(|value| value.as_ref() == ORIGINATED_INDEX_VERSION)
    {
        return Ok(());
    }

    #[cfg(feature = "chain-grpc")]
    let unresolved_provenance = {
        let chain_records = db
            .keyspace(CHAIN_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .map_err(|e| JournalError::Store(format!("open chain record index: {e}")))?;
        let mut provenance_counts = HashMap::<Lsn, usize>::new();
        for entry in chain_records.iter() {
            let entry = entry
                .into_inner()
                .map_err(|e| JournalError::Store(format!("scan chain record index: {e}")))?;
            let key = entry.0.as_ref();
            if key.len() < 16 {
                return Err(JournalError::Corrupt {
                    lsn: Lsn::new(0, 0),
                    msg: "chain provenance key is shorter than an LSN".into(),
                });
            }
            let origin =
                parse_lsn_key(&key[key.len() - 16..]).ok_or_else(|| JournalError::Corrupt {
                    lsn: Lsn::new(0, 0),
                    msg: "invalid origin LSN in chain provenance key".into(),
                })?;
            *provenance_counts.entry(origin).or_default() += 1;
        }

        // A mirrored row whose local key differs from its embedded origin is
        // already unambiguous. Consume its provenance count so a distinct
        // locally-authored row at the same LSN is not accidentally excluded.
        for entry in records.iter() {
            let stored = decode_kv(entry)?;
            if stored.lsn != stored.record.lsn {
                if let Some(count) = provenance_counts.get_mut(&stored.record.lsn) {
                    *count = count.saturating_sub(1);
                }
            }
        }
        provenance_counts
    };

    for entry in records.iter() {
        let stored = decode_kv(entry)?;
        let key = lsn_key(stored.lsn);
        if originated_records
            .contains_key(key)
            .map_err(|e| JournalError::Store(format!("read originated record index: {e}")))?
        {
            continue;
        }
        if stored.lsn != stored.record.lsn {
            continue;
        }
        #[cfg(feature = "chain-grpc")]
        if unresolved_provenance
            .get(&stored.record.lsn)
            .is_some_and(|count| *count > 0)
        {
            continue;
        }
        originated_records
            .insert(key, b"")
            .map_err(|e| JournalError::Store(format!("backfill originated record: {e}")))?;
    }

    metadata
        .insert(ORIGINATED_INDEX_VERSION_KEY, ORIGINATED_INDEX_VERSION)
        .map_err(|e| JournalError::Store(format!("write originated index version: {e}")))?;
    db.persist(PersistMode::SyncData)
        .map_err(|e| JournalError::Store(format!("persist originated index migration: {e}")))
}

/// Recover the next LSN from the last record stored, or a fresh start.
///
/// This is an approximation: it advances past the last record's value length.
/// Segment boundaries are not correctness-critical — only monotonicity of the
/// LSN is — so the next append recomputes the exact position.
fn next_lsn_after(records: &Keyspace) -> Result<Lsn, JournalError> {
    let Some(lsn) = last_stored_lsn(records)? else {
        return Ok(Lsn::new(0, 0));
    };
    let last = records
        .get(lsn_key(lsn))
        .map_err(|e| JournalError::Store(format!("last record: {e}")))?
        .ok_or_else(|| JournalError::Corrupt {
            lsn,
            msg: "last record disappeared during open".into(),
        })?;
    Ok(advance_from(lsn, last.len() as u64))
}

fn last_stored_lsn(records: &Keyspace) -> Result<Option<Lsn>, JournalError> {
    let Some(last) = records.iter().next_back() else {
        return Ok(None);
    };
    let kv = last
        .into_inner()
        .map_err(|e| JournalError::Store(format!("last record: {e}")))?;
    let lsn = parse_lsn_key(kv.0.as_ref()).ok_or_else(|| JournalError::Corrupt {
        lsn: Lsn::new(0, 0),
        msg: "unparseable last record key".into(),
    })?;
    Ok(Some(lsn))
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
    decode_pair(kv.0.as_ref(), kv.1.as_ref())
}

fn decode_pair(key: &[u8], value: &[u8]) -> Result<StoredRecord, JournalError> {
    let lsn = parse_lsn_key(key).ok_or_else(|| JournalError::Corrupt {
        lsn: Lsn::new(0, 0),
        msg: "unparseable key in scan".into(),
    })?;
    let record: JournalRecord = postcard::from_bytes(value).map_err(|e| JournalError::Corrupt {
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

    #[tokio::test]
    async fn durable_append_is_recorded_once_at_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&JournalConfig {
            dir: dir.path().to_path_buf(),
            commit: GroupCommitConfig::default(),
        })
        .expect("open journal");
        let metrics = journal.commit_metrics();
        let mut cursor = metrics.snapshot();

        let handle = journal.append(mk_record(42)).expect("append");
        // Staging an append has not crossed the durable boundary yet.
        assert_eq!(metrics.snapshot().total(), 0);
        handle.committed().await.expect("commit");

        let delta = metrics.delta(&mut cursor);
        assert_eq!(delta.iter().map(|sample| sample.count).sum::<u64>(), 1);
        assert!(metrics.delta(&mut cursor).is_empty());
        journal.close().await.expect("close");
    }

    #[tokio::test]
    async fn selected_group_is_staged_and_made_visible_together() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&JournalConfig {
            dir: dir.path().to_path_buf(),
            commit: GroupCommitConfig {
                mode: crate::journal::AdaptiveCommitMode::AlwaysBatch,
                batch_window: std::time::Duration::from_millis(100),
                batch_max_records: 64,
                batch_max_bytes: 1 << 20,
            },
        })
        .expect("open journal");

        let handles = (0..16)
            .map(|entity| journal.append(mk_record(entity)).expect("queue append"))
            .collect::<Vec<_>>();

        assert_eq!(
            journal.scan_from(Lsn::new(0, 0)).count(),
            0,
            "queued rows must not be individually staged before group selection"
        );
        for handle in handles {
            handle.committed().await.expect("commit selected group");
        }

        assert_eq!(journal.flush_count(), 1, "one selected group, one commit");
        let stored = journal
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .expect("scan committed group");
        assert_eq!(stored.len(), 16);
        assert!(stored.windows(2).all(|pair| pair[0].lsn < pair[1].lsn));
        journal.close().await.expect("close");
    }

    #[cfg(feature = "chain-grpc")]
    #[tokio::test]
    async fn adoption_indexes_a_large_mirrored_prefix_in_one_pass() {
        use crate::journal::chain_grpc::{
            chain_key_for_adoption, DurableChainId, RecordProvenance,
        };

        const RECORDS: usize = 1_024;
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&JournalConfig {
            dir: dir.path().to_path_buf(),
            commit: GroupCommitConfig::default(),
        })
        .expect("open journal");
        let source = DurableChainId {
            primary_node: test_node(11),
            follower_node: test_node(12),
            shard_set: b"adoption-perf".to_vec(),
            epoch: 7,
        };
        let source_key = chain_key_for_adoption(&source).expect("source key");
        let first = Lsn::new(0, 100);
        let last = Lsn::new(0, 100 + (RECORDS as u64 - 1) * 100);
        let mut handles = Vec::with_capacity(RECORDS);
        for ordinal in 0..RECORDS {
            let origin = Lsn::new(0, 100 + ordinal as u64 * 100);
            let mut record = mk_record(ordinal as u64);
            record.lsn = origin;
            let provenance = RecordProvenance {
                batch_seq: 0,
                ordinal: ordinal as u32,
                batch_len: RECORDS as u32,
                predecessor: None,
                first_lsn: first,
                last_lsn: last,
                next_lsn: Lsn::new(0, last.offset + 100),
            };
            handles.push(
                journal
                    .append_replicated_indexed(
                        record,
                        &source_key,
                        &postcard::to_stdvec(&provenance).expect("encode provenance"),
                    )
                    .expect("stage mirrored record")
                    .expect("new mirrored record"),
            );
        }
        for handle in handles {
            handle.committed().await.expect("commit mirrored record");
        }

        let history = journal.adopt_chain_history(source).expect("adopt prefix");
        assert_eq!(history.watermark(), Some(last));
        assert_eq!(
            journal
                .scan_adopted_from(&history, Lsn::new(0, 0))
                .expect("scan adopted history")
                .count(),
            RECORDS
        );
        journal.close().await.expect("close");
    }

    #[cfg(feature = "chain-grpc")]
    #[tokio::test]
    async fn legacy_journal_backfills_only_originated_records_for_catch_up() {
        let primary_dir = tempfile::tempdir().expect("primary tempdir");
        let follower_dir = tempfile::tempdir().expect("follower tempdir");

        // Reproduce the on-disk shape left by a version that had records and
        // an empty originated_records keyspace but no migration marker.
        let db = Database::builder(primary_dir.path())
            .manual_journal_persist(true)
            .open()
            .expect("open legacy db");
        let records = db
            .keyspace(RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .expect("legacy records");
        db.keyspace(ORIGINATED_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .expect("empty legacy originated index");

        let mut local = mk_record(1);
        local.lsn = Lsn::new(0, 0);
        records
            .insert(lsn_key(local.lsn), postcard::to_stdvec(&local).unwrap())
            .unwrap();

        let mut mirrored = mk_record(2);
        mirrored.lsn = Lsn::new(7, 700);
        records
            .insert(
                lsn_key(Lsn::new(0, 100)),
                postcard::to_stdvec(&mirrored).unwrap(),
            )
            .unwrap();

        // Exercise the otherwise ambiguous collision: the mirrored origin LSN
        // equals its follower-local key. Durable gRPC provenance identifies it
        // as mirrored, so migration must still exclude it.
        let mut colliding_mirror = mk_record(3);
        colliding_mirror.lsn = Lsn::new(0, 200);
        records
            .insert(
                lsn_key(colliding_mirror.lsn),
                postcard::to_stdvec(&colliding_mirror).unwrap(),
            )
            .unwrap();
        let chain_records = db
            .keyspace(CHAIN_RECORDS_KS, fjall::KeyspaceCreateOptions::default)
            .expect("legacy chain provenance");
        chain_records
            .insert(
                chain_record_key(b"legacy-chain", colliding_mirror.lsn),
                b"provenance",
            )
            .unwrap();
        db.persist(PersistMode::SyncData)
            .expect("persist legacy db");
        drop(chain_records);
        drop(records);
        drop(db);

        let primary = Arc::new(
            Journal::open(&JournalConfig {
                dir: primary_dir.path().to_path_buf(),
                commit: GroupCommitConfig::default(),
            })
            .expect("upgrade legacy journal"),
        );
        let originated = primary
            .scan_originated_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .expect("scan migrated originated index");
        assert_eq!(
            originated
                .iter()
                .map(|stored| stored.record.entity)
                .collect::<Vec<_>>(),
            vec![orrery_protocol::PersistId::new(1)],
            "migration must backfill local history without amplifying mirrors"
        );

        let follower = Arc::new(
            Journal::open(&JournalConfig {
                dir: follower_dir.path().to_path_buf(),
                commit: GroupCommitConfig::default(),
            })
            .expect("open follower"),
        );
        let sink = Arc::new(crate::journal::JournalChainSink::new(Arc::clone(&follower)));
        let transport = Arc::new(crate::journal::MemChainTransport::new(sink));
        let replicator = crate::journal::spawn_chain(
            Arc::clone(&primary),
            transport,
            &crate::journal::ChainConfig::default(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while follower.scan_from(Lsn::new(0, 0)).count() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "migrated local history did not catch up"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let caught_up = follower
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            caught_up[0].record.entity,
            orrery_protocol::PersistId::new(1)
        );

        replicator.shutdown().await;
        primary.close().await.unwrap();
        follower.close().await.unwrap();
    }
}
