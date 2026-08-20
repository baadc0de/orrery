//! The default indexed wal-db-backed journal (`journal-raw`, D19).
//!
//! wal-db owns segmented framing, CRC32C validation, torn-tail recovery, and
//! the platform durability barrier. Orrery keeps its logical [`Lsn`] space and
//! rebuilds the indexes required by replay and chain replication from durable
//! WAL entries at open.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use orrery_protocol::{JournalRecord, Lsn};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use wal_db::{SegmentedStore, Wal, WalConfig};

use crate::journal::group_commit::{
    spawn_committer, CommitterHandle, StagedAppend, StoreCommitTimings,
};
use crate::journal::{
    AppendHandle, JournalCommitMetrics, JournalConfig, JournalError, JournalRelease, JournalScan,
    StoredRecord,
};

const PUBLISH_CAPACITY: usize = 4096;
const SEGMENT_SIZE: u64 = 128 * 1024 * 1024;
const WAL_SUBDIR: &str = "raw-wal";
#[cfg(feature = "chain-grpc")]
const MAX_U64_VARINT_LEN: usize = 10;

/// A versioned Orrery record inside wal-db's own CRC-framed record.
#[derive(Debug, Serialize, Deserialize)]
enum RawEnvelope {
    V1(RawEntry),
}

#[derive(Debug, Serialize, Deserialize)]
enum RawEntry {
    Record {
        local_lsn: Lsn,
        encoded: Vec<u8>,
        originated: bool,
        provenance: Option<(Vec<u8>, Vec<u8>)>,
    },
    ChainState {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Adoption {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    /// A retention marker (D20): everything below `floor` has been released.
    ///
    /// It carries the two positions that would otherwise be *derived* from
    /// records that are no longer there. Without them a journal released to
    /// empty would reopen at LSN 0:0 and mint logical LSNs a previous
    /// incarnation had already acknowledged.
    Release {
        /// The retention floor. No record below it survives in the index.
        floor: Lsn,
        /// The LSN the next append must take.
        next_lsn: Lsn,
        /// The committed watermark at release time.
        committed: Option<Lsn>,
    },
}

/// A running chain's claim on the records this journal may release (D20).
#[derive(Clone, Copy, Debug)]
struct ChainClaim {
    /// Whether the follower's watermark is in *this* journal's LSN space, and
    /// so can bound a release at all. False for a promotion-adopted chain,
    /// which echoes the source's LSNs; such a chain blocks release for as long
    /// as it is registered rather than lifting the block with a number that
    /// means something else.
    bounds: bool,
    /// The highest watermark the follower has confirmed durable.
    watermark: Option<Lsn>,
}

#[derive(Clone, Copy, Debug)]
struct RecordLocation {
    physical: wal_db::Lsn,
    originated: bool,
}

#[derive(Default)]
struct RawIndex {
    records: BTreeMap<Lsn, RecordLocation>,
    /// The retention floor recovered from (or advanced by) a `Release` marker.
    /// Records below it are not indexed even when their segment survived, so
    /// a reopened journal behaves identically whether or not `truncate_before`
    /// happened to drop the segment they were in.
    released_below: Option<Lsn>,
    /// The positions a `Release` marker carried, when one was seen.
    released_next_lsn: Option<Lsn>,
    released_committed: Option<Lsn>,
    #[cfg(feature = "chain-grpc")]
    chain_records: BTreeMap<Vec<u8>, Vec<u8>>,
    #[cfg(feature = "chain-grpc")]
    chain_state: BTreeMap<Vec<u8>, Vec<u8>>,
    #[cfg(feature = "chain-grpc")]
    adoptions: BTreeMap<Vec<u8>, Vec<u8>>,
    #[cfg(feature = "chain-grpc")]
    adopted_records: BTreeMap<Vec<u8>, Lsn>,
}

/// The default per-node indexed wal-db journal.
pub struct Journal {
    wal: Arc<Wal<SegmentedStore>>,
    wal_dir: std::path::PathBuf,
    index: Arc<RwLock<RawIndex>>,
    cursor: std::sync::Mutex<Lsn>,
    segment_size: u64,
    committer: CommitterHandle,
    closed: std::sync::atomic::AtomicBool,
    metrics: Arc<JournalCommitMetrics>,
    published: broadcast::Sender<JournalRecord>,
    /// The outbound chain's claim on this journal (D20). `None` when no chain
    /// has registered, in which case retention answers to the checkpoint floor
    /// alone.
    chain_floor: std::sync::Mutex<Option<ChainClaim>>,
    #[cfg(test)]
    scan_fault: std::sync::atomic::AtomicBool,
}

impl Journal {
    /// Open or recover a raw wal-db journal and start its group committer.
    pub fn open(config: &JournalConfig) -> Result<Self, JournalError> {
        let wal_dir = prepare_wal_dir(&config.dir)?;
        let wal = Arc::new(
            Wal::open_segmented_with(
                &wal_dir,
                SEGMENT_SIZE,
                WalConfig::new().with_max_record_size(u32::MAX),
            )
            .map_err(|error| JournalError::Store(format!("open raw wal: {error}")))?,
        );

        let mut recovered = RawIndex::default();
        for entry in wal
            .iter()
            .map_err(|error| JournalError::Store(format!("scan raw wal: {error}")))?
        {
            let record =
                entry.map_err(|error| JournalError::Store(format!("recover raw wal: {error}")))?;
            let envelope = decode_envelope(record.data(), None)?;
            apply_recovered_entry(&mut recovered, record.lsn(), envelope)?;
        }
        // Records below the recovered retention floor are dropped rather than
        // indexed. `truncate_before` reclaims whole segments, so the segment
        // holding the floor keeps every record below it that shares it; index
        // them and a scan would answer from a prefix whose length is an
        // accident of segment alignment.
        if let Some(floor) = recovered.released_below {
            recovered.records = recovered.records.split_off(&floor);
            #[cfg(feature = "chain-grpc")]
            recovered.adopted_records.retain(|_, local| *local >= floor);
        }

        let recovered_committed = recovered
            .records
            .last_key_value()
            .map(|(lsn, _)| *lsn)
            .max(recovered.released_committed);
        let next_lsn = match recovered.records.last_key_value() {
            Some((lsn, location)) => {
                let record = read_record_at(&wal, *lsn, location.physical)?;
                successor(*lsn, encoded_len(&record.record), SEGMENT_SIZE)
            }
            None => Lsn::new(0, 0),
        }
        // A release marker's `next_lsn` is a floor on the cursor, not a
        // replacement for it: the marker is written before the appends that
        // may follow it, so the derived position wins whenever there is one.
        .max(recovered.released_next_lsn.unwrap_or(Lsn::new(0, 0)));
        let index = Arc::new(RwLock::new(recovered));

        let metrics = Arc::new(JournalCommitMetrics::new());
        let commit = Arc::new({
            let wal = Arc::clone(&wal);
            let index = Arc::clone(&index);
            move |pending: &[crate::journal::group_commit::Pending]| {
                let started = std::time::Instant::now();
                let mut durable = Vec::with_capacity(pending.len());
                for pending in pending {
                    let staged = &pending.staged;
                    let local_lsn = parse_lsn_key(&staged.key).ok_or_else(|| {
                        JournalError::Store("invalid staged raw journal LSN".into())
                    })?;
                    decode_record(local_lsn, &staged.encoded)?;
                    let envelope = RawEnvelope::V1(RawEntry::Record {
                        local_lsn,
                        encoded: staged.encoded.clone(),
                        originated: staged.originated,
                        provenance: {
                            #[cfg(feature = "chain-grpc")]
                            {
                                staged.provenance.clone()
                            }
                            #[cfg(not(feature = "chain-grpc"))]
                            {
                                None
                            }
                        },
                    });
                    let bytes = postcard::to_stdvec(&envelope).map_err(|error| {
                        JournalError::Store(format!("encode raw journal entry: {error}"))
                    })?;
                    let physical = wal.append(&bytes).map_err(|error| {
                        JournalError::Store(format!("append raw journal entry: {error}"))
                    })?;
                    durable.push((local_lsn, physical, staged.originated));
                }
                wal.sync()
                    .map_err(|error| JournalError::Store(format!("sync raw journal: {error}")))?;
                let mut guard = write_index(&index)?;
                for (local_lsn, physical, originated) in durable {
                    guard.records.insert(
                        local_lsn,
                        RecordLocation {
                            physical,
                            originated,
                        },
                    );
                }
                #[cfg(feature = "chain-grpc")]
                for pending in pending {
                    if let Some((key, value)) = &pending.staged.provenance {
                        guard.chain_records.insert(key.clone(), value.clone());
                    }
                }
                Ok(StoreCommitTimings {
                    fjall_batch_commit: Duration::ZERO,
                    sync_data: started.elapsed(),
                })
            }
        });

        let (published, _) = broadcast::channel(PUBLISH_CAPACITY);
        let committer = spawn_committer(
            config.commit.clone(),
            commit,
            published.clone(),
            recovered_committed,
            Arc::clone(&metrics),
        );

        Ok(Self {
            wal,
            wal_dir,
            index,
            cursor: std::sync::Mutex::new(next_lsn),
            segment_size: SEGMENT_SIZE,
            committer,
            closed: std::sync::atomic::AtomicBool::new(false),
            metrics,
            published,
            chain_floor: std::sync::Mutex::new(None),
            #[cfg(test)]
            scan_fault: std::sync::atomic::AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(crate) fn inject_scan_fault(&self) {
        self.scan_fault
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Append a locally originated record and return its durability handle.
    pub fn append(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, JournalError> {
        self.append_inner(record, true)
    }

    /// Append a mirrored record without publishing it back to the chain.
    pub fn append_replicated(
        &self,
        record: JournalRecord,
    ) -> Result<Arc<AppendHandle>, JournalError> {
        self.append_inner(record, false)
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn append_replicated_indexed(
        &self,
        record: JournalRecord,
        chain_key: &[u8],
        provenance: &[u8],
    ) -> Result<Option<Arc<AppendHandle>>, JournalError> {
        let index_key = chain_record_key(chain_key, record.lsn);
        if read_index(&self.index)?
            .chain_records
            .contains_key(&index_key)
        {
            return Ok(None);
        }
        self.append_inner_with_index(record, false, Some((index_key, provenance.to_vec())))
            .map(Some)
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_successor(&self, record: &JournalRecord) -> Lsn {
        successor(record.lsn, encoded_len(record), self.segment_size)
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
        #[cfg(feature = "chain-grpc")] provenance: Option<(Vec<u8>, Vec<u8>)>,
        #[cfg(not(feature = "chain-grpc"))] _provenance: Option<()>,
    ) -> Result<Arc<AppendHandle>, JournalError> {
        let started = std::time::Instant::now();
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(JournalError::Closed);
        }
        if record.payload.len() > u32::MAX as usize {
            return Err(JournalError::PayloadTooLarge(record.payload.len()));
        }

        let (local_lsn, key, encoded) = {
            let origin_lsn = record.lsn;
            let mut cursor = self.cursor.lock().expect("journal cursor lock");
            let local_lsn = advance(&mut cursor, encoded_len(&record), self.segment_size);
            record.lsn = local_lsn;
            let key = lsn_key(local_lsn);
            if !publish {
                record.lsn = origin_lsn;
            }
            let encoded = postcard::to_stdvec(&record)
                .map_err(|error| JournalError::Store(format!("encode record: {error}")))?;
            (local_lsn, key, encoded)
        };

        let handle = Arc::new(AppendHandle::new(
            local_lsn,
            started,
            Arc::clone(&self.metrics),
        ));
        self.committer.submit(
            Arc::clone(&handle),
            StagedAppend {
                key: key.to_vec(),
                encoded,
                originated: publish,
                #[cfg(feature = "chain-grpc")]
                provenance,
            },
            record,
            publish,
        );
        Ok(handle)
    }

    /// Return the highest logical LSN durably flushed so far.
    pub fn committed(&self) -> Lsn {
        self.committer.committed().unwrap_or(Lsn::new(0, 0))
    }

    pub(crate) fn committed_watermark(&self) -> Option<Lsn> {
        self.committer.committed()
    }

    pub(crate) fn segment_size(&self) -> u64 {
        self.segment_size
    }

    /// Return the number of group durability barriers issued since open.
    pub fn flush_count(&self) -> usize {
        self.committer.flush_count()
    }

    /// Return the fixed-memory commit-latency recorder.
    #[must_use]
    pub fn commit_metrics(&self) -> Arc<JournalCommitMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Subscribe to records after their group durability barrier completes.
    pub fn subscribe(&self) -> broadcast::Receiver<JournalRecord> {
        self.published.subscribe()
    }

    /// Release every record below `before`, reclaiming the segments that hold
    /// only released records (D20).
    ///
    /// **The caller asserts the precondition**: every record below `before` is
    /// already folded into durable state that survives this journal — in
    /// `persistd` that is the minimum checkpoint watermark across the shards
    /// this node hosts. The journal cannot check that for itself; what it does
    /// instead is make a violation loud rather than silent, by failing any
    /// later scan that reaches below the floor
    /// ([`JournalError::Released`]) instead of answering with the surviving
    /// suffix.
    ///
    /// Ordering, which is the whole of the crash-safety argument:
    ///
    /// 1. Take the physical cut *before* anything is appended, so nothing
    ///    written by this call can fall below it.
    /// 2. Re-anchor the metadata the surviving suffix needs — chain state and
    ///    adoption markers are keyed maps rebuilt by replaying the log, so a
    ///    copy has to exist above the cut before the originals are dropped.
    /// 3. Write the release marker and `sync`. Until this barrier completes
    ///    nothing has changed: a crash here reopens at the old floor with a
    ///    duplicate copy of some metadata, which replay folds idempotently.
    /// 4. Only then drop the segments, and only then prune the in-memory index.
    ///
    /// So the durable marker always precedes the deletion it describes, and
    /// the window in between costs a retry rather than a record.
    pub fn release_before(&self, before: Lsn) -> Result<JournalRelease, JournalError> {
        use crate::journal::ReleaseBlocked;

        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(JournalError::Closed);
        }

        let mut index = write_index(&self.index)?;
        let floor_now = index.released_below.unwrap_or(Lsn::new(0, 0));
        if before <= floor_now {
            return Ok(JournalRelease::blocked(
                before,
                floor_now,
                ReleaseBlocked::AlreadyReleased,
            ));
        }

        // A follower's mirror is not released. `chain_grpc::rebuild_cursor`
        // reconstructs the durable cursor by walking the provenance index from
        // batch zero and stopping at the first gap, so releasing a prefix of it
        // would rebuild an empty cursor and cost a full re-stream of the
        // primary's journal — the failure `refuse_sibling_epoch` documents.
        // Bounding a follower's mirror needs that cursor persisted first; until
        // it is, the primary is what retention covers (D20 §residual).
        #[cfg(feature = "chain-grpc")]
        if !index.chain_records.is_empty() {
            return Ok(JournalRelease::blocked(
                before,
                floor_now,
                ReleaseBlocked::FollowerProvenance,
            ));
        }

        // A lagging follower resumes by rescanning *this* journal from its own
        // watermark (`chain::spawn_chain_from`), so the floor is bounded by
        // what the follower has confirmed as well as by what the checkpoints
        // cover. Clamped rather than refused: releasing up to the follower's
        // watermark is exactly as safe as releasing up to a checkpoint's.
        let before = match *self.chain_floor.lock().expect("chain floor lock") {
            None => before,
            Some(ChainClaim {
                watermark: Some(watermark),
                bounds: true,
            }) => before.min(watermark),
            Some(_) => {
                return Ok(JournalRelease::blocked(
                    before,
                    floor_now,
                    ReleaseBlocked::ChainLag,
                ))
            }
        };
        if before <= floor_now {
            return Ok(JournalRelease::blocked(
                before,
                floor_now,
                ReleaseBlocked::ChainLag,
            ));
        }

        let bytes_before = dir_bytes(&self.wal_dir);

        // Step 1. The cut is the lowest physical position any *retained* record
        // occupies. Taken as a minimum over the retained range rather than as
        // the first entry's position, so the invariant it rests on — that
        // logical and physical order agree — is enforced here rather than
        // assumed. With nothing retained, the cut is the current tail: every
        // byte written so far is releasable and the marker below carries the
        // positions that would otherwise be derived from the dropped records.
        let cut = index
            .records
            .range(before..)
            .map(|(_, location)| location.physical.get())
            .min()
            .unwrap_or_else(|| self.wal.len());

        // Step 2. Re-anchor keyed metadata above the cut.
        #[cfg(feature = "chain-grpc")]
        {
            let chain_state: Vec<(Vec<u8>, Vec<u8>)> = index
                .chain_state
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (key, value) in chain_state {
                append_metadata_unsynced(
                    &self.wal,
                    &RawEnvelope::V1(RawEntry::ChainState { key, value }),
                )?;
            }
            let adoptions: Vec<(Vec<u8>, Vec<u8>)> = index
                .adoptions
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (key, value) in adoptions {
                let value = prune_adoption_marker(&value, before)?;
                append_metadata_unsynced(
                    &self.wal,
                    &RawEnvelope::V1(RawEntry::Adoption { key, value }),
                )?;
            }
        }

        // Step 3. The marker, then the barrier.
        let next_lsn = *self.cursor.lock().expect("journal cursor lock");
        let committed = self.committer.committed();
        append_metadata_unsynced(
            &self.wal,
            &RawEnvelope::V1(RawEntry::Release {
                floor: before,
                next_lsn,
                committed,
            }),
        )?;
        self.wal
            .sync()
            .map_err(|error| JournalError::Store(format!("sync release marker: {error}")))?;

        // Step 4. Drop the segments, then the index entries.
        let head = self
            .wal
            .truncate_before(wal_db::Lsn::new(cut))
            .map_err(|error| JournalError::Store(format!("release journal segments: {error}")))?;
        debug_assert!(
            head.get() <= cut,
            "wal-db moved the head above the requested cut"
        );

        let retained = index.records.split_off(&before);
        let records_dropped = index.records.len() as u64;
        index.records = retained;
        index.released_below = Some(before);
        index.released_next_lsn = Some(next_lsn);
        index.released_committed = committed;
        #[cfg(feature = "chain-grpc")]
        index.adopted_records.retain(|_, local| *local >= before);
        drop(index);

        Ok(JournalRelease {
            requested: before,
            floor: before,
            records_dropped,
            bytes_before,
            bytes_after: dir_bytes(&self.wal_dir),
            blocked: None,
        })
    }

    /// Register an outbound chain: nothing is released until its follower
    /// watermark is known (D20).
    ///
    /// A registration lasts for the life of the journal. A chain that has
    /// *stopped* — faulted, or its follower gone — keeps its claim, because a
    /// follower that is behind and unreachable is exactly the one that would
    /// lose the records a release reclaims. Retention resumes when a chain
    /// probes successfully again, and until it does the reason the journal is
    /// not shrinking is reported as [`ReleaseBlocked::ChainLag`].
    pub fn register_chain(&self, bounds_retention: bool) {
        *self.chain_floor.lock().expect("chain floor lock") = Some(ChainClaim {
            bounds: bounds_retention,
            watermark: None,
        });
    }

    /// Record a follower watermark an outbound chain has confirmed.
    ///
    /// Ignored unless the chain registered as bounding retention, so the
    /// caller does not have to know whether its own watermark is comparable
    /// with this journal's — it says so once, at registration.
    pub fn note_chain_watermark(&self, watermark: Lsn) {
        let mut floor = self.chain_floor.lock().expect("chain floor lock");
        if let Some(claim) = floor.as_mut() {
            if claim.bounds {
                claim.watermark = Some(claim.watermark.map_or(watermark, |c| c.max(watermark)));
            }
        }
    }

    /// The lowest LSN this journal still retains (D20). `0:0` until a release.
    pub fn released_floor(&self) -> Lsn {
        read_index(&self.index)
            .ok()
            .and_then(|index| index.released_below)
            .unwrap_or(Lsn::new(0, 0))
    }

    /// Scan records with `lsn >= from` in logical LSN order.
    ///
    /// Fails [`JournalError::Released`] rather than answering short when
    /// `from` is below the retention floor.
    pub fn scan_from<'a>(&'a self, from: Lsn) -> JournalScan<'a> {
        if let Err(error) = self.guard_floor(from) {
            return one_error_scan(error);
        }
        match record_locations_from(&self.index, from, false) {
            Ok(locations) => self.scan_locations(locations),
            Err(error) => one_error_scan(error),
        }
    }

    fn guard_floor(&self, from: Lsn) -> Result<(), JournalError> {
        let floor = read_index(&self.index)?.released_below;
        match floor {
            Some(floor) if from < floor => Err(JournalError::Released {
                requested: from,
                floor,
            }),
            _ => Ok(()),
        }
    }

    pub(crate) fn scan_originated_from<'a>(&'a self, from: Lsn) -> JournalScan<'a> {
        if let Err(error) = self.guard_floor(from) {
            return one_error_scan(error);
        }
        #[cfg(test)]
        if self.scan_fault.load(std::sync::atomic::Ordering::Acquire) {
            return one_error_scan(JournalError::Store(
                "injected originated scan failure".into(),
            ));
        }
        match record_locations_from(&self.index, from, true) {
            Ok(locations) => self.scan_locations(locations),
            Err(error) => one_error_scan(error),
        }
    }

    fn scan_locations<'a>(&'a self, locations: Vec<(Lsn, wal_db::Lsn)>) -> JournalScan<'a> {
        let iter = locations
            .into_iter()
            .map(move |(local, physical)| read_record_at(&self.wal, local, physical));
        JournalScan {
            iter: Box::new(iter),
        }
    }

    pub(crate) fn scan_source_from<'a>(
        &'a self,
        source: &crate::journal::chain::ChainSource,
        from: Lsn,
    ) -> Result<JournalScan<'a>, JournalError> {
        match source {
            crate::journal::chain::ChainSource::Originated => Ok(self.scan_originated_from(from)),
            #[cfg(feature = "chain-grpc")]
            crate::journal::chain::ChainSource::Adopted(history) => {
                self.scan_adopted_from(history, from)
            }
        }
    }

    /// Flush pending appends and stop the group committer.
    pub async fn close(&self) -> Result<(), JournalError> {
        if self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        self.committer.shutdown();
        self.committer.wait_exit().await;
        self.wal
            .sync()
            .map_err(|error| JournalError::Store(format!("final raw wal sync: {error}")))
    }

    /// Return whether this journal has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_records(
        &self,
        chain_key: &[u8],
    ) -> Result<Vec<(Lsn, Vec<u8>)>, JournalError> {
        let prefix = chain_record_prefix(chain_key);
        let end = prefix_successor(&prefix).ok_or_else(|| {
            JournalError::Store("chain identity has no finite key-range successor".into())
        })?;
        read_index(&self.index)?
            .chain_records
            .range(prefix.clone()..end)
            .map(|(key, value)| {
                let origin =
                    parse_lsn_key(&key[prefix.len()..]).ok_or_else(|| JournalError::Corrupt {
                        lsn: Lsn::new(0, 0),
                        msg: "invalid origin LSN in raw chain index".into(),
                    })?;
                Ok((origin, value.clone()))
            })
            .collect()
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_epoch_sibling(
        &self,
        family: &[u8],
        own_key: &[u8],
    ) -> Result<Option<Vec<u8>>, JournalError> {
        let index = read_index(&self.index)?;
        for width in 1..=MAX_U64_VARINT_LEN {
            let length = family.len() + width;
            let Ok(encoded_len) = u32::try_from(length) else {
                continue;
            };
            let mut base = encoded_len.to_be_bytes().to_vec();
            base.extend_from_slice(family);
            let Some(end) = prefix_successor(&base) else {
                continue;
            };
            let mut ranges = Vec::with_capacity(2);
            if own_key.len() == length && own_key.starts_with(family) {
                let own = chain_record_prefix(own_key);
                if base < own {
                    ranges.push((base.clone(), own.clone()));
                }
                if let Some(after) = prefix_successor(&own) {
                    if after < end {
                        ranges.push((after, end));
                    }
                }
            } else {
                ranges.push((base, end));
            }
            for (start, stop) in ranges {
                let Some((key, _)) = index.chain_records.range(start..stop).next() else {
                    continue;
                };
                let head: [u8; 4] = key
                    .get(..4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .ok_or_else(|| JournalError::Store("truncated chain index key".into()))?;
                let sibling_len = u32::from_be_bytes(head) as usize;
                let sibling = key
                    .get(4..4 + sibling_len)
                    .ok_or_else(|| JournalError::Store("truncated chain index key".into()))?;
                return Ok(Some(sibling.to_vec()));
            }
        }
        Ok(None)
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_state_epoch_sibling(
        &self,
        family: &[u8],
        own_key: &[u8],
    ) -> Result<Option<Vec<u8>>, JournalError> {
        let Some(end) = prefix_successor(family) else {
            return Ok(None);
        };
        Ok(read_index(&self.index)?
            .chain_state
            .range(family.to_vec()..end)
            .map(|(key, _)| key)
            .find(|key| key.as_slice() != own_key)
            .cloned())
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_record(
        &self,
        chain_key: &[u8],
        origin: Lsn,
    ) -> Result<Option<Vec<u8>>, JournalError> {
        Ok(read_index(&self.index)?
            .chain_records
            .get(&chain_record_key(chain_key, origin))
            .cloned())
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn chain_grpc_state(
        &self,
        chain_key: &[u8],
    ) -> Result<Option<Vec<u8>>, JournalError> {
        Ok(read_index(&self.index)?.chain_state.get(chain_key).cloned())
    }

    #[cfg(feature = "chain-grpc")]
    pub(crate) fn set_chain_grpc_state(
        &self,
        chain_key: &[u8],
        value: &[u8],
    ) -> Result<(), JournalError> {
        let entry = RawEnvelope::V1(RawEntry::ChainState {
            key: chain_key.to_vec(),
            value: value.to_vec(),
        });
        append_metadata(&self.wal, &entry)?;
        write_index(&self.index)?
            .chain_state
            .insert(chain_key.to_vec(), value.to_vec());
        Ok(())
    }

    /// Adopt a complete follower history after the caller has fenced ownership.
    #[cfg(feature = "chain-grpc")]
    pub fn adopt_chain_history(
        &self,
        source: crate::journal::chain_grpc::DurableChainId,
    ) -> Result<crate::journal::chain::AdoptedChainHistory, JournalError> {
        use crate::journal::chain::AdoptedChainHistory;
        use crate::journal::chain_grpc::{
            chain_key_for_adoption, AdoptedRecord, AdoptionMarker, RecordProvenance,
        };
        use std::collections::HashMap;

        let source_key = chain_key_for_adoption(&source)?;
        let marker_key = adoption_key(&source_key);
        if let Some(value) = read_index(&self.index)?.adoptions.get(&marker_key).cloned() {
            let marker: AdoptionMarker = postcard::from_bytes(&value)
                .map_err(|error| JournalError::Store(format!("decode adoption marker: {error}")))?;
            if marker.source_key != source_key {
                return Err(JournalError::Store(
                    "adoption marker identity mismatch".into(),
                ));
            }
            return Ok(AdoptedChainHistory::new(source, marker.watermark));
        }

        let mut batches: BTreeMap<u64, Vec<(Lsn, RecordProvenance)>> = BTreeMap::new();
        for (origin, bytes) in self.chain_grpc_records(&source_key)? {
            let provenance: RecordProvenance = postcard::from_bytes(&bytes).map_err(|error| {
                JournalError::Store(format!("decode chain provenance: {error}"))
            })?;
            batches
                .entry(provenance.batch_seq)
                .or_default()
                .push((origin, provenance));
        }

        let mut local_by_origin = HashMap::<Lsn, Option<Lsn>>::new();
        for stored in self.scan_from(Lsn::new(0, 0)).filter_map(Result::ok) {
            match local_by_origin.entry(stored.record.lsn) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Some(stored.lsn));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    *entry.get_mut() = None;
                }
            }
        }

        let mut previous = None;
        let mut adopted_records = Vec::new();
        for (expected, (sequence, mut batch)) in batches.into_iter().enumerate() {
            if sequence != expected as u64 {
                return Err(JournalError::Store(
                    "cannot adopt chain history with a batch gap".into(),
                ));
            }
            batch.sort_by_key(|(_, provenance)| provenance.ordinal);
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
            for (ordinal, (origin, provenance)) in batch.iter().enumerate() {
                if provenance.batch_seq != sequence
                    || provenance.ordinal as usize != ordinal
                    || provenance.batch_len != first.batch_len
                    || provenance.predecessor != first.predecessor
                    || provenance.first_lsn != first.first_lsn
                    || provenance.last_lsn != first.last_lsn
                    || provenance.next_lsn != first.next_lsn
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
        let value = postcard::to_stdvec(&marker)
            .map_err(|error| JournalError::Store(format!("encode adoption marker: {error}")))?;
        append_metadata(
            &self.wal,
            &RawEnvelope::V1(RawEntry::Adoption {
                key: marker_key.clone(),
                value: value.clone(),
            }),
        )?;
        let mut index = write_index(&self.index)?;
        apply_adoption(&mut index, marker_key, value)?;
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
        let prefix = chain_record_prefix(&source_key);
        let end = prefix_successor(&prefix).ok_or_else(|| {
            JournalError::Store("adopted chain identity has no finite key-range successor".into())
        })?;
        let index = read_index(&self.index)?;
        let mut locations = Vec::new();
        for (_, local) in index
            .adopted_records
            .range(adoption_record_key(&source_key, from)..end)
        {
            let location = index
                .records
                .get(local)
                .ok_or_else(|| JournalError::Corrupt {
                    lsn: *local,
                    msg: "adopted record missing".into(),
                })?;
            locations.push((*local, location.physical));
        }
        drop(index);
        Ok(self.scan_locations(locations))
    }
}

/// Resolve the backend-owned directory without silently treating a previous
/// backend's journal as an empty raw journal.
///
/// Fjall stored its files directly in `JournalConfig::dir`; raw owns the
/// `raw-wal/` child. A first raw open over a non-empty Fjall directory would
/// otherwise acknowledge a fresh LSN history while ignoring durable records
/// beside it. D19 requires an explicit drain/checkpoint or migration instead.
fn prepare_wal_dir(root: &std::path::Path) -> Result<std::path::PathBuf, JournalError> {
    std::fs::create_dir_all(root)
        .map_err(|error| JournalError::Store(format!("create journal directory: {error}")))?;
    let wal_dir = root.join(WAL_SUBDIR);
    if wal_dir.exists() {
        return Ok(wal_dir);
    }

    let first = std::fs::read_dir(root)
        .map_err(|error| JournalError::Store(format!("inspect journal directory: {error}")))?
        .next()
        .transpose()
        .map_err(|error| JournalError::Store(format!("inspect journal entry: {error}")))?;
    if let Some(entry) = first {
        return Err(JournalError::Store(format!(
            "journal directory is non-empty (found {:?}) but has no {WAL_SUBDIR}/; refusing to initialize journal-raw over possible Fjall data; drain/checkpoint or migrate it explicitly",
            entry.file_name()
        )));
    }
    Ok(wal_dir)
}

/// Total bytes of every file under `dir`, for release accounting.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Append one metadata envelope **without** a durability barrier.
///
/// The release path writes several and pays for one barrier at the end;
/// [`append_metadata`] is the single-entry form that syncs for itself.
fn append_metadata_unsynced(
    wal: &Wal<SegmentedStore>,
    envelope: &RawEnvelope,
) -> Result<(), JournalError> {
    let bytes = postcard::to_stdvec(envelope)
        .map_err(|error| JournalError::Store(format!("encode raw metadata: {error}")))?;
    wal.append(&bytes)
        .map(|_| ())
        .map_err(|error| JournalError::Store(format!("append raw metadata: {error}")))
}

/// Drop the record entries an adoption marker names below `floor`, so a
/// re-anchored marker never points at a released record.
#[cfg(feature = "chain-grpc")]
fn prune_adoption_marker(value: &[u8], floor: Lsn) -> Result<Vec<u8>, JournalError> {
    use crate::journal::chain_grpc::AdoptionMarker;
    let mut marker: AdoptionMarker = postcard::from_bytes(value)
        .map_err(|error| JournalError::Store(format!("decode adoption marker: {error}")))?;
    marker.records.retain(|record| record.local >= floor);
    postcard::to_stdvec(&marker)
        .map_err(|error| JournalError::Store(format!("encode adoption marker: {error}")))
}

#[cfg(feature = "chain-grpc")]
fn append_metadata(wal: &Wal<SegmentedStore>, envelope: &RawEnvelope) -> Result<(), JournalError> {
    let bytes = postcard::to_stdvec(envelope)
        .map_err(|error| JournalError::Store(format!("encode raw metadata: {error}")))?;
    let _physical = wal
        .append(&bytes)
        .map_err(|error| JournalError::Store(format!("append raw metadata: {error}")))?;
    wal.sync()
        .map_err(|error| JournalError::Store(format!("sync raw metadata: {error}")))
}

fn apply_recovered_entry(
    index: &mut RawIndex,
    physical: wal_db::Lsn,
    envelope: RawEnvelope,
) -> Result<(), JournalError> {
    match envelope {
        RawEnvelope::V1(RawEntry::Record {
            local_lsn,
            encoded,
            originated,
            provenance,
        }) => {
            decode_record(local_lsn, &encoded)?;
            if index
                .records
                .insert(
                    local_lsn,
                    RecordLocation {
                        physical,
                        originated,
                    },
                )
                .is_some()
            {
                return Err(JournalError::Corrupt {
                    lsn: local_lsn,
                    msg: "duplicate logical LSN in raw journal".into(),
                });
            }
            #[cfg(feature = "chain-grpc")]
            if let Some((key, value)) = provenance {
                index.chain_records.insert(key, value);
            }
            #[cfg(not(feature = "chain-grpc"))]
            let _ = provenance;
        }
        RawEnvelope::V1(RawEntry::ChainState { key, value }) => {
            #[cfg(feature = "chain-grpc")]
            index.chain_state.insert(key, value);
            #[cfg(not(feature = "chain-grpc"))]
            let _ = (key, value);
        }
        RawEnvelope::V1(RawEntry::Release {
            floor,
            next_lsn,
            committed,
        }) => {
            // Markers are replayed in append order, so a later one wins; take
            // the maximum anyway so an out-of-order recovery cannot lower a
            // floor that is already in force.
            index.released_below = Some(index.released_below.map_or(floor, |c| c.max(floor)));
            index.released_next_lsn = Some(
                index
                    .released_next_lsn
                    .map_or(next_lsn, |c| c.max(next_lsn)),
            );
            index.released_committed = index.released_committed.max(committed);
        }
        RawEnvelope::V1(RawEntry::Adoption { key, value }) => {
            #[cfg(feature = "chain-grpc")]
            apply_adoption(index, key, value)?;
            #[cfg(not(feature = "chain-grpc"))]
            let _ = (key, value);
        }
    }
    Ok(())
}

#[cfg(feature = "chain-grpc")]
fn apply_adoption(index: &mut RawIndex, key: Vec<u8>, value: Vec<u8>) -> Result<(), JournalError> {
    use crate::journal::chain_grpc::AdoptionMarker;
    let marker: AdoptionMarker = postcard::from_bytes(&value)
        .map_err(|error| JournalError::Store(format!("decode adoption marker: {error}")))?;
    for record in &marker.records {
        index.adopted_records.insert(
            adoption_record_key(&marker.source_key, record.origin),
            record.local,
        );
    }
    index.adoptions.insert(key, value);
    Ok(())
}

fn record_locations_from(
    index: &RwLock<RawIndex>,
    from: Lsn,
    originated_only: bool,
) -> Result<Vec<(Lsn, wal_db::Lsn)>, JournalError> {
    Ok(read_index(index)?
        .records
        .range(from..)
        .filter(|(_, location)| !originated_only || location.originated)
        .map(|(logical, location)| (*logical, location.physical))
        .collect())
}

fn read_record_at(
    wal: &Wal<SegmentedStore>,
    expected: Lsn,
    physical: wal_db::Lsn,
) -> Result<StoredRecord, JournalError> {
    let mut iter = wal
        .iter_from(physical)
        .map_err(|error| JournalError::Store(format!("seek raw wal: {error}")))?;
    let record = iter
        .next()
        .ok_or_else(|| JournalError::Corrupt {
            lsn: expected,
            msg: "raw index points past durable WAL tail".into(),
        })?
        .map_err(|error| JournalError::Corrupt {
            lsn: expected,
            msg: format!("read raw WAL frame: {error}"),
        })?;
    let envelope = decode_envelope(record.data(), Some(expected))?;
    match envelope {
        RawEnvelope::V1(RawEntry::Record {
            local_lsn, encoded, ..
        }) if local_lsn == expected => decode_record(local_lsn, &encoded),
        RawEnvelope::V1(RawEntry::Record { local_lsn, .. }) => Err(JournalError::Corrupt {
            lsn: expected,
            msg: format!("raw index points to record {local_lsn}"),
        }),
        _ => Err(JournalError::Corrupt {
            lsn: expected,
            msg: "raw index points to metadata".into(),
        }),
    }
}

fn decode_envelope(bytes: &[u8], lsn: Option<Lsn>) -> Result<RawEnvelope, JournalError> {
    postcard::from_bytes(bytes).map_err(|error| JournalError::Corrupt {
        lsn: lsn.unwrap_or(Lsn::new(0, 0)),
        msg: format!("decode raw envelope: {error}"),
    })
}

fn decode_record(local_lsn: Lsn, bytes: &[u8]) -> Result<StoredRecord, JournalError> {
    let record: JournalRecord =
        postcard::from_bytes(bytes).map_err(|error| JournalError::Corrupt {
            lsn: local_lsn,
            msg: format!("decode journal record: {error}"),
        })?;
    Ok(StoredRecord {
        lsn: local_lsn,
        record,
    })
}

fn one_error_scan(error: JournalError) -> JournalScan<'static> {
    JournalScan {
        iter: Box::new(std::iter::once(Err(error))),
    }
}

fn read_index(
    index: &RwLock<RawIndex>,
) -> Result<std::sync::RwLockReadGuard<'_, RawIndex>, JournalError> {
    index
        .read()
        .map_err(|_| JournalError::Store("raw journal index lock poisoned".into()))
}

fn write_index(
    index: &RwLock<RawIndex>,
) -> Result<std::sync::RwLockWriteGuard<'_, RawIndex>, JournalError> {
    index
        .write()
        .map_err(|_| JournalError::Store("raw journal index lock poisoned".into()))
}

fn lsn_key(lsn: Lsn) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&lsn.segment.to_be_bytes());
    key[8..].copy_from_slice(&lsn.offset.to_be_bytes());
    key
}

fn parse_lsn_key(key: &[u8]) -> Option<Lsn> {
    if key.len() != 16 {
        return None;
    }
    let segment = u64::from_be_bytes(key[..8].try_into().ok()?);
    let offset = u64::from_be_bytes(key[8..].try_into().ok()?);
    Some(Lsn { segment, offset })
}

fn advance(cursor: &mut Lsn, span: u64, segment_size: u64) -> Lsn {
    let assigned = *cursor;
    *cursor = successor(assigned, span, segment_size);
    assigned
}

fn successor(lsn: Lsn, span: u64, segment_size: u64) -> Lsn {
    let next_offset = lsn.offset.saturating_add(span);
    if next_offset > segment_size {
        Lsn::new(lsn.segment.saturating_add(1), 0)
    } else {
        Lsn::new(lsn.segment, next_offset)
    }
}

fn encoded_len(record: &JournalRecord) -> u64 {
    u64::try_from(record.payload.len().saturating_add(64)).unwrap_or(u64::MAX)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::GroupCommitConfig;
    use proptest::prelude::*;

    fn test_node(n: u8) -> orrery_protocol::NodeId {
        let mut seed = [0; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn record(entity: u64) -> JournalRecord {
        let payload = entity.to_le_bytes();
        JournalRecord {
            lsn: Lsn::new(0, 0),
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

    fn config(dir: &std::path::Path) -> JournalConfig {
        JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig::default(),
        }
    }

    #[test]
    fn raw_envelope_v1_wire_shape_is_pinned_across_feature_sets() {
        let envelope = RawEnvelope::V1(RawEntry::Record {
            local_lsn: Lsn::new(2, 3),
            encoded: vec![4, 5],
            originated: true,
            provenance: Some((vec![6], vec![7, 8])),
        });
        let encoded = postcard::to_stdvec(&envelope).expect("encode V1 fixture");

        assert_eq!(encoded, [0, 0, 2, 3, 2, 4, 5, 1, 1, 1, 6, 2, 7, 8]);
    }

    #[test]
    fn a_nonempty_legacy_directory_is_not_silently_opened_as_empty_raw() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("manifest"), b"legacy fjall marker")
            .expect("write legacy marker");

        let error = match Journal::open(&config(dir.path())) {
            Ok(_) => panic!("raw journal must reject an unmarked non-empty directory"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("possible Fjall data"),
            "unexpected error: {error}"
        );
        assert!(!dir.path().join(WAL_SUBDIR).exists());
    }

    #[tokio::test]
    async fn replay_reopen_and_next_lsn_match_the_journal_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&config(dir.path())).expect("open raw journal");
        let first = journal
            .append(record(1))
            .expect("append first")
            .committed()
            .await
            .expect("commit first");
        let second = journal
            .append(record(2))
            .expect("append second")
            .committed()
            .await
            .expect("commit second");
        assert!(first < second);
        journal.close().await.expect("close raw journal");
        drop(journal);

        let reopened = Journal::open(&config(dir.path())).expect("reopen raw journal");
        let recovered = reopened
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .expect("replay raw journal");
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].lsn, first);
        assert_eq!(recovered[0].record.lsn, first);
        assert_eq!(recovered[1].lsn, second);
        assert_eq!(reopened.committed_watermark(), Some(second));

        let third = reopened
            .append(record(3))
            .expect("append after reopen")
            .committed()
            .await
            .expect("commit after reopen");
        assert!(third > second, "reopen must continue the logical LSN space");
        reopened.close().await.expect("close reopened journal");
    }

    proptest! {
        #[test]
        fn a_torn_final_frame_recovers_the_last_intact_record(cut in 1_u64..32) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let dir = tempfile::tempdir().expect("tempdir");
                let journal = Journal::open(&config(dir.path())).expect("open raw journal");
                let first = journal.append(record(1)).expect("append first");
                first.committed().await.expect("commit first");
                let second = journal.append(record(2)).expect("append second");
                let second_lsn = second.committed().await.expect("commit second");
                let third = journal.append(record(3)).expect("append third");
                third.committed().await.expect("commit third");
                journal.close().await.expect("close raw journal");
                drop(journal);

                let segment = dir
                    .path()
                    .join(WAL_SUBDIR)
                    .join("00000000000000000000.wal");
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&segment)
                    .expect("open final segment");
                let len = file.metadata().expect("segment metadata").len();
                file.set_len(len - cut).expect("truncate final frame");
                file.sync_all().expect("sync torn segment");
                drop(file);

                let recovered = Journal::open(&config(dir.path())).expect("recover torn tail");
                let records = recovered
                    .scan_from(Lsn::new(0, 0))
                    .collect::<Result<Vec<_>, _>>()
                    .expect("scan recovered prefix");
                assert_eq!(records.len(), 2);
                assert_eq!(records[1].lsn, second_lsn);
                recovered.close().await.expect("close recovered journal");
            });
        }
    }
}
