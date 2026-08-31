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

/// The keyed metadata a release re-anchors above its cut, snapshotted under
/// the index lock and written without it.
#[cfg(feature = "chain-grpc")]
struct ReleaseMetadata {
    chain_state: Vec<(Vec<u8>, Vec<u8>)>,
    adoptions: Vec<(Vec<u8>, Vec<u8>)>,
}

#[cfg(not(feature = "chain-grpc"))]
struct ReleaseMetadata {}

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

/// The archive tailer's claim on the records this journal may release (D20).
#[derive(Clone, Copy, Debug)]
struct ArchiveClaim {
    /// The highest watermark whose archive object has been verified.
    watermark: Option<Lsn>,
}

#[derive(Clone, Copy, Debug)]
struct RecordLocation {
    physical: wal_db::Lsn,
    originated: bool,
}

/// One row of the follower dedupe index: the batch provenance a mirrored
/// record arrived with, and the **local** position it was written to.
///
/// The key carries the record's *origin* LSN (the primary's), which is the
/// identity §4.1 dedupes on; retention works in this journal's own space, so
/// the local position has to be recoverable from the row rather than derived
/// from the key. It is not a second durable copy: both halves come out of the
/// one `RawEntry::Record` that carried the record and its provenance, at
/// commit and at recovery alike.
#[cfg(feature = "chain-grpc")]
#[derive(Clone, Debug)]
struct ChainRow {
    provenance: Vec<u8>,
    local: Lsn,
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
    /// Locally originated records still in the index. Retention needs to know
    /// whether this journal holds any at all — a pure mirror is bounded by its
    /// primary's floor rather than by local checkpoints (D23) — and counting
    /// them at insert and at prune keeps that a read rather than a scan.
    originated: u64,
    #[cfg(feature = "chain-grpc")]
    chain_records: BTreeMap<Vec<u8>, ChainRow>,
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
    /// Serializes releases (D20), so each one can take the index lock in short
    /// sections instead of holding it across its WAL work.
    release: std::sync::Mutex<()>,
    /// The outbound chain's claim on this journal (D20). `None` when no chain
    /// has registered, in which case retention answers to the checkpoint floor
    /// alone.
    chain_floor: std::sync::Mutex<Option<ChainClaim>>,
    /// The archive tailer's claim on this journal (D20). `None` until an
    /// operator opts in, so nodes without a running tailer retain the exact
    /// checkpoint-plus-chain behaviour they had before the claim existed.
    archive_floor: std::sync::Mutex<Option<ArchiveClaim>>,
    /// Per mirrored chain, the retention floor its **primary** has itself
    /// reached, in that primary's LSN space (D23). A mirror is bounded by this
    /// and not by local checkpoints: no local actor folds a mirrored record,
    /// and what a promotion still needs from the mirror is exactly what the
    /// primary's durable tier does not already hold.
    #[cfg(feature = "chain-grpc")]
    mirror_floors: std::sync::Mutex<BTreeMap<Vec<u8>, Lsn>>,
    /// What retention has done since open, for the operator-facing snapshot.
    retention: std::sync::Mutex<crate::journal::JournalRetention>,
    /// What this journal's own `open` cost, in milliseconds — the D16
    /// `journal_open_ms` budget's measurand, reported by the node that paid it
    /// rather than inferred from a log timestamp.
    open_ms: f64,
    #[cfg(test)]
    scan_fault: std::sync::atomic::AtomicBool,
}

impl Journal {
    /// Open or recover a raw wal-db journal and start its group committer.
    pub fn open(config: &JournalConfig) -> Result<Self, JournalError> {
        Self::open_with_segment_size(config, crate::journal::DEFAULT_SEGMENT_SIZE)
    }

    /// [`Journal::open`] with an explicit logical segment span.
    ///
    /// One width for both the physical wal-db segment and the logical
    /// `Lsn::segment` this journal mints, because the archive tailer's object
    /// granularity is the logical one and #807's
    /// `jarchive/{node_id}/{segment_seq}` key is that number
    /// (docs/08-persistence.md §11.6).
    ///
    /// **Every production caller wants [`DEFAULT_SEGMENT_SIZE`].** This entry
    /// point exists so the archive tests can seal segments without writing
    /// 128 MiB per segment, and so an operator who has measured the tailer's
    /// per-segment footprint (§11.6's memory bound) has a way to trade it
    /// against object count. `0` is read as the default rather than as a
    /// segment that can hold nothing.
    ///
    /// # Errors
    ///
    /// See [`Journal::open`].
    ///
    /// [`DEFAULT_SEGMENT_SIZE`]: crate::journal::DEFAULT_SEGMENT_SIZE
    pub fn open_with_segment_size(
        config: &JournalConfig,
        segment_size: u64,
    ) -> Result<Self, JournalError> {
        let opened_at = std::time::Instant::now();
        let segment_size = if segment_size == 0 {
            crate::journal::DEFAULT_SEGMENT_SIZE
        } else {
            segment_size
        };
        let wal_dir = prepare_wal_dir(&config.dir)?;
        let wal = Arc::new(
            Wal::open_segmented_with(
                &wal_dir,
                segment_size,
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
            prune_released(&mut recovered, floor);
        }

        let recovered_committed = recovered
            .records
            .last_key_value()
            .map(|(lsn, _)| *lsn)
            .max(recovered.released_committed);
        let next_lsn = match recovered.records.last_key_value() {
            Some((lsn, location)) => {
                let record = read_record_at(&wal, *lsn, location.physical)?;
                successor(*lsn, encoded_len(&record.record), segment_size)
            }
            None => Lsn::new(0, 0),
        }
        // A release marker's `next_lsn` is a floor on the cursor, not a
        // replacement for it: the marker is written before the appends that
        // may follow it, so the derived position wins whenever there is one.
        .max(recovered.released_next_lsn.unwrap_or(Lsn::new(0, 0)));
        let released_floor = recovered.released_below.unwrap_or(Lsn::new(0, 0));
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
                    if originated {
                        guard.originated += 1;
                    }
                }
                #[cfg(feature = "chain-grpc")]
                for pending in pending {
                    if let Some((key, value)) = &pending.staged.provenance {
                        let local = parse_lsn_key(&pending.staged.key).ok_or_else(|| {
                            JournalError::Store("invalid staged raw journal LSN".into())
                        })?;
                        guard.chain_records.insert(
                            key.clone(),
                            ChainRow {
                                provenance: value.clone(),
                                local,
                            },
                        );
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
            segment_size,
            committer,
            closed: std::sync::atomic::AtomicBool::new(false),
            metrics,
            published,
            release: std::sync::Mutex::new(()),
            chain_floor: std::sync::Mutex::new(None),
            archive_floor: std::sync::Mutex::new(None),
            #[cfg(feature = "chain-grpc")]
            mirror_floors: std::sync::Mutex::new(BTreeMap::new()),
            retention: std::sync::Mutex::new(crate::journal::JournalRetention {
                floor: released_floor,
                ..crate::journal::JournalRetention::default()
            }),
            open_ms: opened_at.elapsed().as_secs_f64() * 1e3,
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
            // The logical record is written through its versioned frame (D38
            // (d)(5)), never bare postcard: the encoding version is stamped by
            // the writer rather than inferred by whoever reads the WAL back.
            let encoded = record
                .encode_frame()
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

    /// The logical segment span this journal mints LSNs within.
    ///
    /// The archive tailer's object granularity and memory bound
    /// (docs/08-persistence.md §11.6), and the multiplier
    /// [`Journal::archive_gap`] turns a segment count into bytes with.
    #[must_use]
    pub fn segment_size(&self) -> u64 {
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
    ///
    /// Three clamps are the journal's own rather than the caller's, because
    /// they are facts only it holds: a registered outbound chain's follower
    /// watermark (D20), the archive tailer's verified watermark (D20), and —
    /// for each chain this journal *mirrors* — the floor that chain's primary
    /// has itself reached (D23). Asking to release past any of them is
    /// answered with a bounded release or a named block, never with the
    /// records.
    pub fn release_before(&self, before: Lsn) -> Result<JournalRelease, JournalError> {
        let outcome = self.release_locked(before);
        // The operator-facing tally, updated on every answered call: a blocked
        // release is a normal outcome and the reason is the whole of what
        // "this journal is not shrinking" means.
        if let Ok(release) = &outcome {
            let mut retention = self.retention.lock().expect("journal retention lock");
            retention.blocked = release.blocked;
            if release.blocked.is_none() {
                retention.releases += 1;
                retention.records_dropped += release.records_dropped;
                retention.floor = release.floor;
            }
        }
        outcome
    }

    fn release_locked(&self, before: Lsn) -> Result<JournalRelease, JournalError> {
        use crate::journal::ReleaseBlocked;

        #[allow(unused_mut)]
        let mut before = before;

        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(JournalError::Closed);
        }

        // Serialize releases against each other. Only the checkpoint scheduler
        // calls this today, but the lock is what lets every step below take the
        // index lock briefly instead of holding it across the whole operation.
        let _release = self.release.lock().expect("journal release lock");

        // **No WAL call happens while the index lock is held.** The group
        // committer appends, syncs, and *then* takes the index write lock, so a
        // release that held the index lock across `append`/`sync` would deadlock
        // against it: the committer waits for the index while the release waits
        // for the WAL. The lock order is therefore one way for both — WAL
        // first, index second — and the release takes the index in short,
        // separate sections around its WAL work.
        //
        // The journal tail is read here, before the index lock, for the same
        // reason: the mirror clamp below needs it and must not hold two locks
        // to get it.
        let tail = *self.cursor.lock().expect("journal cursor lock");
        #[cfg(not(feature = "chain-grpc"))]
        let _ = tail;
        let (floor_now, cut, metadata) = {
            let index = read_index(&self.index)?;
            let floor_now = index.released_below.unwrap_or(Lsn::new(0, 0));

            // A verified archive object is the second precondition for
            // deleting local segments (docs/08 §11). Clamp before choosing
            // the physical cut below: computing the cut from the checkpoint
            // proposal and only then lowering the logical floor could reclaim
            // a segment that still contains unarchived records.
            //
            // With no registration this term abstains, preserving the
            // pre-archive release path exactly.
            if let Some(claim) = *self.archive_floor.lock().expect("archive floor lock") {
                let Some(watermark) = claim.watermark else {
                    return Ok(JournalRelease::blocked(
                        before,
                        floor_now,
                        ReleaseBlocked::ArchiveLag,
                    ));
                };
                before = before.min(watermark);
                if before <= floor_now {
                    return Ok(JournalRelease::blocked(
                        before,
                        floor_now,
                        ReleaseBlocked::ArchiveLag,
                    ));
                }
            }
            if before <= floor_now {
                return Ok(JournalRelease::blocked(
                    before,
                    floor_now,
                    ReleaseBlocked::AlreadyReleased,
                ));
            }

            // A mirror is released only as far as its primary has released,
            // and only while the durable dedupe cursor that seeds a rebuild
            // exists (D23). Both are checked here, under the same read lock
            // that took the floor, so the clamp cannot be computed against an
            // index a concurrent release has already pruned.
            #[cfg(feature = "chain-grpc")]
            for (key, cut) in self.mirror_cuts(&index, tail)? {
                if !index.chain_state.contains_key(&key) {
                    return Ok(JournalRelease::blocked(
                        before,
                        floor_now,
                        ReleaseBlocked::MirrorCursorAbsent,
                    ));
                }
                if cut <= floor_now {
                    return Ok(JournalRelease::blocked(
                        before,
                        floor_now,
                        ReleaseBlocked::MirrorLag,
                    ));
                }
                before = before.min(cut);
            }

            // The cut is the lowest physical position any *retained* record
            // occupies. Taken as a minimum over the retained range rather than
            // as the first entry's position, so the invariant it rests on —
            // that logical and physical order agree — is enforced here rather
            // than assumed. With nothing retained, the cut is the current tail:
            // every byte written so far is releasable, and the marker below
            // carries the positions that would otherwise be derived from the
            // dropped records. Records appended after this point take LSNs
            // above `before` and land above the cut, so a concurrent committer
            // cannot invalidate it.
            let cut = index
                .records
                .range(before..)
                .map(|(_, location)| location.physical.get())
                .min()
                .unwrap_or_else(|| self.wal.len());

            #[cfg(feature = "chain-grpc")]
            let metadata = ReleaseMetadata {
                chain_state: index
                    .chain_state
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                adoptions: index
                    .adoptions
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };
            #[cfg(not(feature = "chain-grpc"))]
            let metadata = ReleaseMetadata {};

            (floor_now, cut, metadata)
        };

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

        // Step 1 (no index lock). Re-anchor keyed metadata above the cut: chain
        // state and adoption markers are maps rebuilt by replaying the log, so
        // a copy has to exist above the cut before the originals are dropped.
        #[cfg(feature = "chain-grpc")]
        {
            for (key, value) in metadata.chain_state {
                append_metadata_unsynced(
                    &self.wal,
                    &RawEnvelope::V1(RawEntry::ChainState { key, value }),
                )?;
            }
            for (key, value) in metadata.adoptions {
                let value = prune_adoption_marker(&value, before)?;
                append_metadata_unsynced(
                    &self.wal,
                    &RawEnvelope::V1(RawEntry::Adoption { key, value }),
                )?;
            }
        }
        #[cfg(not(feature = "chain-grpc"))]
        let ReleaseMetadata {} = metadata;

        // Step 2 (no index lock). The marker, then the barrier. Until this
        // returns nothing has changed: a crash here reopens at the old floor
        // with a duplicate copy of some metadata, which replay folds
        // idempotently.
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

        // Step 3 (index lock, briefly). Prune what the durable marker now says
        // is gone. Nothing between here and the marker can have lowered the
        // floor: `_release` is the only writer of it.
        let records_dropped = {
            let mut index = write_index(&self.index)?;
            index.released_below = Some(before);
            index.released_next_lsn = Some(next_lsn);
            index.released_committed = committed;
            prune_released(&mut index, before)
        };

        // Step 4 (no index lock). Drop the segments the marker already
        // accounted for.
        let head = self
            .wal
            .truncate_before(wal_db::Lsn::new(cut))
            .map_err(|error| JournalError::Store(format!("release journal segments: {error}")))?;
        debug_assert!(
            head.get() <= cut,
            "wal-db moved the head above the requested cut"
        );

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
    /// `bounds_retention` says whether that watermark will be a position in
    /// *this* journal's LSN space. It is false for a promotion-adopted chain,
    /// which echoes the source's LSNs, and such a chain blocks release for as
    /// long as it is registered rather than lifting the block with a number
    /// that means something else.
    ///
    /// A registration lasts for the life of the journal. A chain that has
    /// *stopped* — faulted, or its follower gone — keeps its claim, because a
    /// follower that is behind and unreachable is exactly the one that would
    /// lose the records a release reclaims. Retention resumes when a chain
    /// probes successfully again, and until it does the reason the journal is
    /// not shrinking is reported as [`ReleaseBlocked::ChainLag`].
    ///
    /// [`ReleaseBlocked::ChainLag`]: crate::journal::ReleaseBlocked::ChainLag
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

    /// Register the archive tailer's claim on this journal (D20).
    ///
    /// Nothing is released until a verified archive watermark is known.
    /// Registration lasts for the life of the journal: an unreachable or
    /// stopped tailer keeps its claim because releasing then would destroy the
    /// unread input it needs to recover.
    pub fn register_archive(&self) {
        *self.archive_floor.lock().expect("archive floor lock") =
            Some(ArchiveClaim { watermark: None });
    }

    /// Record the highest journal position whose archive object is verified.
    ///
    /// The watermark is monotone and ignored until the archive claim is
    /// registered, matching [`Journal::note_chain_watermark`]'s abstention
    /// rule.
    pub fn note_archive_watermark(&self, watermark: Lsn) {
        let mut floor = self.archive_floor.lock().expect("archive floor lock");
        if let Some(claim) = floor.as_mut() {
            claim.watermark = Some(claim.watermark.map_or(watermark, |c| c.max(watermark)));
        }
    }

    /// The archive claim's state: whether a tailer has registered, and how far
    /// it has verified.
    ///
    /// The read half of [`Journal::register_archive`] and
    /// [`Journal::note_archive_watermark`]. The tailer uses it to reconcile a
    /// recovered watermark against the one already in force; the checkpoint
    /// scheduler uses it to turn a blocked release into the §15 alarm.
    pub fn archive_claim(&self) -> crate::journal::ArchiveClaimState {
        use crate::journal::ArchiveClaimState;
        match *self.archive_floor.lock().expect("archive floor lock") {
            None => ArchiveClaimState::Unregistered,
            Some(ArchiveClaim { watermark: None }) => ArchiveClaimState::Registered,
            Some(ArchiveClaim {
                watermark: Some(watermark),
            }) => ArchiveClaimState::Verified { watermark },
        }
    }

    /// How far the archive is behind a proposed retention floor.
    ///
    /// `None` when no tailer has claimed this journal — there is no gap to
    /// report and the archive term is not what is holding the floor.
    ///
    /// The distance is measured on the accounted-byte axis the journal's own
    /// cursor advances on: `segment` counts whole
    /// [`JournalConfig::segment_size`](crate::journal::JournalConfig::segment_size)
    /// spans and `offset` the bytes within one, so
    /// `segments * segment_size + offset` is directly comparable with the
    /// journal disk an operator is watching fill. An unregistered *watermark*
    /// (a tailer that has verified nothing) is reported as the whole distance
    /// from `0:0`, because that is exactly how much the journal must keep.
    pub fn archive_gap(&self, proposed: Lsn) -> Option<crate::journal::JournalArchiveGap> {
        use crate::journal::{ArchiveClaimState, JournalArchiveGap};
        let claim = self.archive_claim();
        let watermark = match claim {
            ArchiveClaimState::Unregistered => return None,
            ArchiveClaimState::Registered => Lsn::new(0, 0),
            ArchiveClaimState::Verified { watermark } => watermark,
        };
        let segments_behind = proposed.segment.saturating_sub(watermark.segment);
        let bytes_behind = segments_behind
            .saturating_mul(self.segment_size)
            .saturating_add(proposed.offset)
            .saturating_sub(watermark.offset);
        Some(JournalArchiveGap {
            proposed,
            claim,
            segments_behind,
            bytes_behind,
        })
    }

    /// Record the retention floor a mirrored chain's **primary** has reached,
    /// in that primary's LSN space (D23).
    ///
    /// Monotone per chain: the floor only ever moves forward on the primary,
    /// and a stale frame that said otherwise must not un-release a mirror.
    #[cfg(feature = "chain-grpc")]
    pub(crate) fn note_primary_floor(&self, chain_key: &[u8], floor: Lsn) {
        let mut floors = self.mirror_floors.lock().expect("mirror floor lock");
        let entry = floors.entry(chain_key.to_vec()).or_insert(floor);
        *entry = (*entry).max(floor);
    }

    /// The lowest local position each mirrored chain still needs retained.
    ///
    /// A mirrored record is needed until the primary that wrote it has itself
    /// released it — at which point the durable tier holds it and a promotion
    /// recovering from this mirror reads it from there instead (D23). The cut
    /// is therefore the local position of the lowest retained row whose
    /// *origin* is at or above the primary's floor, and the journal tail when
    /// the primary has released past everything here. A chain whose primary
    /// has advertised no floor at all pins its whole mirror.
    #[cfg(feature = "chain-grpc")]
    fn mirror_cuts(
        &self,
        index: &RawIndex,
        tail: Lsn,
    ) -> Result<Vec<(Vec<u8>, Lsn)>, JournalError> {
        let floors = self.mirror_floors.lock().expect("mirror floor lock");
        let mut cuts = Vec::new();
        let mut start = Vec::new();
        // One seek per chain rather than one step per row: the index carries a
        // row per mirrored record and this runs on the checkpoint cadence.
        while let Some(key) = index
            .chain_records
            .range(start.clone()..)
            .next()
            .map(|(key, _)| key.clone())
        {
            let head: [u8; 4] = key
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| JournalError::Store("truncated chain index key".into()))?;
            let chain_key = key
                .get(4..4 + u32::from_be_bytes(head) as usize)
                .ok_or_else(|| JournalError::Store("truncated chain index key".into()))?
                .to_vec();
            let prefix = chain_record_prefix(&chain_key);
            let end = prefix_successor(&prefix).ok_or_else(|| {
                JournalError::Store("chain identity has no finite key-range successor".into())
            })?;
            let from = match floors.get(&chain_key) {
                Some(floor) => chain_record_key(&chain_key, *floor),
                None => prefix,
            };
            let cut = index
                .chain_records
                .range(from..end.clone())
                .map(|(_, row)| row.local)
                .min()
                .unwrap_or(tail);
            cuts.push((chain_key, cut));
            start = end;
        }
        Ok(cuts)
    }

    /// The floor a release should ask for, given what this node's own
    /// checkpoints cover — `None` while a hosted shard has yet to report one,
    /// and `None` again when there is nothing to bound.
    ///
    /// The journal's record sources and archive answer to different
    /// authorities, so the floor is the lower of all participating terms
    /// (D20, D23):
    ///
    /// - **Locally originated records** are bounded by the checkpoint floor,
    ///   which is what the node's own actors have folded into the durable
    ///   tier. A journal holding none of them is not bounded by it at all —
    ///   a passive follower's actors fold nothing, and reading their empty
    ///   watermark as a floor of `0:0` is what kept every mirror unbounded.
    /// - **Mirrored records** are bounded by [`Journal::mirror_cuts`].
    /// - **Archived records** are bounded by the verified archive watermark
    ///   after an archive claim registers. With no claim, this term abstains.
    pub fn retention_floor(&self, checkpoint_floor: Option<Lsn>) -> Option<Lsn> {
        let tail = *self.cursor.lock().expect("journal cursor lock");
        let index = read_index(&self.index).ok()?;
        let mut floor: Option<Lsn> = None;
        if index.originated > 0 {
            // A hosted shard that has never checkpointed abstains, and its
            // abstention is binding: this journal holds records no durable
            // state covers yet.
            floor = Some(checkpoint_floor?);
        }
        #[cfg(feature = "chain-grpc")]
        for (_, cut) in self.mirror_cuts(&index, tail).ok()? {
            floor = Some(floor.map_or(cut, |floor: Lsn| floor.min(cut)));
        }
        #[cfg(not(feature = "chain-grpc"))]
        let _ = tail;
        match *self.archive_floor.lock().expect("archive floor lock") {
            None => {}
            Some(ArchiveClaim {
                watermark: Some(watermark),
            }) => floor = Some(floor.map_or(watermark, |floor| floor.min(watermark))),
            Some(ArchiveClaim { watermark: None }) => {
                floor = Some(index.released_below.unwrap_or(Lsn::new(0, 0)));
            }
        }
        floor
    }

    /// What retention has done to this journal since it was opened.
    pub fn retention(&self) -> crate::journal::JournalRetention {
        *self.retention.lock().expect("journal retention lock")
    }

    /// What this journal's `open` cost, in milliseconds (D16
    /// `journal_open_ms`).
    #[must_use]
    pub fn open_ms(&self) -> f64 {
        self.open_ms
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
            .map(|(key, row)| {
                let origin =
                    parse_lsn_key(&key[prefix.len()..]).ok_or_else(|| JournalError::Corrupt {
                        lsn: Lsn::new(0, 0),
                        msg: "invalid origin LSN in raw chain index".into(),
                    })?;
                Ok((origin, row.provenance.clone()))
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
            .map(|row| row.provenance.clone()))
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

        // From the retention floor, not from zero: a mirror that has been
        // released no longer holds its prefix, and asking for it is
        // `JournalError::Released` rather than a short answer (D20 §4).
        let mut local_by_origin = HashMap::<Lsn, Option<Lsn>>::new();
        for stored in self.scan_from(self.released_floor()).filter_map(Result::ok) {
            match local_by_origin.entry(stored.record.lsn) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Some(stored.lsn));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    *entry.get_mut() = None;
                }
            }
        }

        // Adoption walks the same index `rebuild_cursor` does, and is seeded
        // the same way (D23): a released mirror starts at the first retained
        // batch, and the persisted cursor supplies the watermark and the batch
        // number the prefix ended at. Without the seed a released mirror reads
        // as a history that begins at a gap, which is a promotion that refuses
        // to start rather than one that starts short.
        let seed = crate::journal::chain_grpc::seed_cursor(self, &source_key)?;
        let seeded_through = seed.and_then(|cursor| cursor.batch_seq);
        let mut previous = seed.and_then(|cursor| cursor.watermark);
        let mut expected = seeded_through.map_or(0, |through| through + 1);
        let mut adopted_records = Vec::new();
        for (sequence, mut batch) in batches {
            // A batch the seed already covers may survive in part: the floor
            // is a checkpoint watermark, not a batch boundary. Its records are
            // not re-adopted — the marker names what is *below* the cutoff it
            // reports, and the seed's watermark already is that cutoff.
            if seeded_through.is_some_and(|through| sequence <= through) {
                continue;
            }
            if sequence != expected {
                return Err(JournalError::Store(
                    "cannot adopt chain history with a batch gap".into(),
                ));
            }
            expected += 1;
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

/// Drop everything a retention floor has released from an index, and return
/// how many record entries went.
///
/// One function for both callers, because the two used to be written out
/// separately and the pair is exactly where a divergence is invisible: a
/// reopened journal and a released-in-place one must present the same index,
/// or a property holds until the process restarts. The mirrored dedupe rows
/// go with the records they point at (D23) — their key carries the *origin*
/// LSN, so they are pruned by the local position in the row rather than by
/// the key.
fn prune_released(index: &mut RawIndex, floor: Lsn) -> u64 {
    let retained = index.records.split_off(&floor);
    let dropped = std::mem::replace(&mut index.records, retained);
    for location in dropped.values() {
        if location.originated {
            index.originated = index.originated.saturating_sub(1);
        }
    }
    #[cfg(feature = "chain-grpc")]
    {
        index.chain_records.retain(|_, row| row.local >= floor);
        index.adopted_records.retain(|_, local| *local >= floor);
    }
    dropped.len() as u64
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
            if originated {
                index.originated += 1;
            }
            #[cfg(feature = "chain-grpc")]
            if let Some((key, value)) = provenance {
                index.chain_records.insert(
                    key,
                    ChainRow {
                        provenance: value,
                        local: local_lsn,
                    },
                );
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
    // A frame written before journals were self-describing carries no version
    // trailer, and the bootstrap rule reads it as encoding v0 rather than
    // refusing it (D38 (d)(1), `orrery_protocol::atrest`). The version travels
    // out on the `StoredRecord`: the archive's `encoding_version` column must
    // say what was on disk, not what this binary would write today.
    let (record, encoding) =
        JournalRecord::decode_frame(bytes).map_err(|error| JournalError::Corrupt {
            lsn: local_lsn,
            msg: format!("decode journal record: {error}"),
        })?;
    Ok(StoredRecord {
        lsn: local_lsn,
        record,
        encoding,
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

    /// Close a journal, or **fail** rather than wait forever.
    ///
    /// `Journal::close` arms the committer's shutdown and then awaits its
    /// exit, and every defect that path has had — #293's lost wakeup on the
    /// commit-queue condvar, and the `Notify` variant before it — presented as
    /// an unbounded wait, not as a wrong answer. An unbounded wait in a test
    /// is the worst of both: it proves nothing, it takes a job's whole timeout
    /// to notice, and it cannot be told apart from a merely slow machine. Two
    /// CI jobs were cancelled after thirty silent minutes before one line said
    /// which test it was.
    ///
    /// The bound is deliberately far above the work: this closes a journal
    /// holding three records, which is microseconds of `fdatasync`. Anything
    /// approaching thirty seconds is a wedge, so the bound cannot fire on
    /// slowness and a regression in the shutdown handshake fails here, named,
    /// in seconds.
    async fn close_or_fail(journal: &Journal, which: &str) {
        match tokio::time::timeout(Duration::from_secs(30), journal.close()).await {
            Ok(result) => result.unwrap_or_else(|error| panic!("close {which} journal: {error}")),
            Err(_) => panic!(
                "closing the {which} journal did not return within 30s: the group committer's \
                 shutdown handshake is wedged (#293), not slow"
            ),
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
    fn a_journal_record_written_before_versioning_replays_as_encoding_v0() {
        // D38 clause (d)(1) on the journal half: the bytes an older writer
        // produced are a bare postcard body with no trailer. `decode_record`
        // is the one door every replay path enters through, so this is the
        // reader that must bootstrap them rather than refuse them — and a
        // refusal here is an unreadable journal, not a degraded one.
        let record = record(7);
        let unversioned = postcard::to_stdvec(&record).expect("legacy encode");
        let framed = record.encode_frame().expect("frame encode");
        assert_eq!(
            framed.len(),
            unversioned.len() + 1,
            "the frame is the legacy body plus one version byte"
        );
        assert_eq!(
            *framed.last().expect("nonempty"),
            orrery_protocol::JOURNAL_RECORD_ENCODING
        );

        let stored = decode_record(Lsn::new(1, 2), &unversioned).expect("bootstraps to v0");
        assert_eq!(stored.record, record, "the record survives unchanged");
        assert_eq!(
            orrery_protocol::JournalRecord::decode_frame(&unversioned)
                .expect("bootstraps")
                .1,
            orrery_protocol::atrest::ENCODING_V0,
            "absent == v0: an unversioned record is v0, not a decode failure"
        );

        // And a frame this writer produced round-trips through the same door
        // carrying its stamped version.
        assert_eq!(
            decode_record(Lsn::new(1, 2), &framed)
                .expect("decodes")
                .record,
            record
        );
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
        close_or_fail(&journal, "raw").await;
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
        close_or_fail(&reopened, "reopened").await;
    }

    /// A journal that holds only *mirrored* records is not bounded by its own
    /// checkpoints, and this is the asymmetry D23 turns on.
    ///
    /// A passive follower's actors fold nothing, so every one of its shards
    /// reports a checkpoint watermark of `0:0` — which read as a floor is the
    /// reason a mirror was never released even after the block came off. What
    /// bounds a mirror is the floor its primary reports; what the local
    /// checkpoints bound is the records this node originated, of which a
    /// passive follower has none.
    #[cfg(feature = "chain-grpc")]
    #[tokio::test]
    async fn a_pure_mirror_answers_to_its_primary_and_not_to_local_checkpoints() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(&config(dir.path())).expect("open raw journal");
        let key = b"chain".to_vec();
        let mut mirrored = Vec::new();
        for entity in 0..4 {
            let mut record = record(entity);
            record.lsn = Lsn::new(0, entity * 100);
            mirrored.push(
                journal
                    .append_replicated_indexed(record, &key, b"provenance")
                    .expect("mirror")
                    .expect("new row")
                    .committed()
                    .await
                    .expect("durable"),
            );
        }

        // No cursor row: a mirror written by a binary that never persisted one
        // cannot be seeded, so it is not released and says why.
        journal.note_primary_floor(&key, Lsn::new(0, 300));
        let blocked = journal
            .release_before(Lsn::new(9_999, 0))
            .expect("release answers");
        assert_eq!(
            blocked.blocked,
            Some(crate::journal::ReleaseBlocked::MirrorCursorAbsent)
        );

        journal
            .set_chain_grpc_state(&key, b"cursor")
            .expect("persist cursor");
        // An empty checkpoint floor does not pin a journal with nothing of its
        // own in it; the primary's floor is what the release answers to.
        assert_eq!(
            journal.retention_floor(Some(Lsn::new(0, 0))),
            Some(mirrored[3]),
            "the mirror cut is the local position of the first row at the primary's floor"
        );
        let release = journal
            .release_before(Lsn::new(9_999, 0))
            .expect("release answers");
        assert_eq!(release.blocked, None);
        assert_eq!(release.floor, mirrored[3]);

        // One locally originated record, and the checkpoint floor is binding
        // again — an uncheckpointed shard abstains for the whole journal.
        journal
            .append(record(9))
            .expect("append")
            .committed()
            .await
            .expect("durable");
        assert_eq!(journal.retention_floor(None), None);
        close_or_fail(&journal, "mirror").await;
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
                close_or_fail(&journal, "written").await;
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
                close_or_fail(&recovered, "recovered").await;
            });
        }
    }
}
