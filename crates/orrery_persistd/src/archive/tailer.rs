//! The journal-to-archive tailer (#808, docs/08-persistence.md §11.6).
//!
//! One pass of this module is: find the next **sealed** logical segment, read
//! the locally originated records in it, re-sort them into §11.1's
//! `(grid, cell, lsn)` order, encode one Parquet object, upload it, **re-read
//! and verify it**, commit the `jarchive/{node_id}/{segment_seq}` row, and only
//! then advance the archive watermark. Everything interesting is in the order
//! of those steps and in what happens when one of them fails.
//!
//! ## 1. Why the logical scan, and not wal-db's segments
//!
//! There is no "list sealed segments" API and this module does not add one. It
//! reads through [`Journal::scan_from`]'s logical view, and it defines a
//! segment as a value of `Lsn::segment`.
//!
//! That is not a workaround; it is the only definition that is *this journal's*
//! own. `raw.rs`'s append cursor mints logical LSNs through `advance`/
//! `successor`, which roll `segment` forward by one whenever a record's
//! accounted span would carry `offset` past
//! [`JournalConfig::segment_size`](crate::journal::JournalConfig::segment_size).
//! So `Lsn::segment` *is* a segment sequence number, minted by the journal, in
//! the journal's own space — and it is exactly the number #807 put in the
//! `jarchive/{node_id}/{segment_seq}` key. Reading wal-db's physical files
//! instead would mean re-deriving that mapping from a framing layer `raw.rs:3`
//! explicitly assigns to wal-db ("wal-db owns segmented framing, CRC32C
//! validation, torn-tail recovery"), and would put a second reader on files the
//! writer is appending to and truncating underneath.
//!
//! **Sealing follows from the same source.** Segment `s` is sealed when
//! [`Journal::committed`] — the highest logical position through a group
//! durability barrier — has a `segment` strictly greater than `s`. The cursor
//! is monotone and never returns to a segment it has left, so a sealed segment
//! is closed for good: the tailer cannot read a segment the writer is still
//! appending to, because the writer has demonstrably moved past it. Using
//! `committed()` rather than the append cursor is the second half of that: a
//! record that has been assigned an LSN but has not passed its barrier is not
//! yet durable, and archiving it would put a record in object storage that a
//! crash could remove from the journal.
//!
//! ## 2. The buffer, and its bound
//!
//! §11.1 sorts an object by `(grid, cell, lsn)` while the journal appends in
//! `lsn` order, so the whole of a segment's records must be in hand before the
//! first byte of its object can be written. The tailer therefore buffers one
//! sealed segment.
//!
//! The bound, stated exactly: a segment spans `segment_size` *accounted* bytes,
//! where a record's accounted span is `payload.len() + 64` (`raw.rs`'s
//! `encoded_len`). Peak resident memory for one pass is therefore
//!
//! ```text
//!   payload bytes (< segment_size)
//! + per-record struct overhead (~112 B x record count)
//! + the encoded Parquet object (~ payload bytes, uncompressed)
//! ```
//!
//! and the record count is itself bounded by `segment_size / 64`. At D19's
//! default 128 MiB that is **at most ~2.1 M records, ~128 MiB of payload, and
//! ~370 MiB peak** — the worst case being all-empty payloads, where the struct
//! overhead dominates; a segment of the P2 gate's own ~256-byte records holds
//! ~420 k records and peaks near 300 MiB. `segment_size` is a
//! [`JournalConfig`](crate::journal::JournalConfig) field precisely so that
//! bound is an operator's to set: halving it halves the tailer's footprint and
//! doubles the object count.
//!
//! This is a real cost and it is the one the object granularity buys. The
//! alternative — streaming a segment into the object in LSN order and sorting
//! nothing — would produce objects whose `cell_ranges` cover everything, which
//! is the same as having no clustering at all, and #615's and #809's pruning
//! both read that column.
//!
//! ## 3. Publication order, and what a crash costs
//!
//! D20 rule 6's discipline, on the archive axis: *the durable marker always
//! precedes the thing it describes*, and the window between costs a retry
//! rather than a record.
//!
//! ```text
//!   encode  ->  put(key)  ->  get(key) and re-hash  ->  put_row  ->  note_archive_watermark
//! ```
//!
//! - A crash before `put` completes: nothing exists; the segment is retried.
//! - A crash between `put` and `put_row`: the object exists and no row does.
//!   Restart re-derives the watermark from the rows ([`recover_watermark`]),
//!   finds this segment unarchived, re-reads the **same** records, re-encodes
//!   the **same** bytes, and `put`s them to the **same** key. The key is
//!   `jarchive/{node hex}/{segment_seq:016x}.parquet` — derived from
//!   `(node_id, segment_seq)` and nothing else — so the retry overwrites rather
//!   than duplicating, and `put_row` is a set on a key derived the same way, so
//!   exactly one row results.
//! - A crash between `put_row` and `note_archive_watermark`: the row is the
//!   durable fact and the watermark is derived from it, so the restart simply
//!   computes the higher watermark. Nothing is repeated.
//!
//! **Verification re-reads the store.** §11.3: "a checksum nobody re-reads is
//! not a verification". [`ArchiveTailer::publish_segment`] hashes the bytes it
//! encoded, uploads, then calls [`ArchiveStore::get`] and hashes *those* bytes,
//! and only a match lets the row be committed. Hashing the in-memory buffer a
//! second time would pass against a store that dropped the object on the floor.
//!
//! ## 4. Node identity (#808 item 7)
//!
//! `jarchive/{node_id}/{segment_seq}` is per node and LSNs are node-local
//! ("Node-local, monotonic position", `orrery_protocol::persist`). D20 rule 3
//! refuses to compare a promotion-adopted chain's watermark with this
//! journal's, and the same refusal governs here. Three rules, all mechanical:
//!
//! 1. **A tailer archives only what its own node originated.** It reads
//!    through `scan_originated_from`, not `scan_from`. `append_inner` overwrites
//!    `record.lsn` with the local position for an originated record and
//!    *restores the origin's LSN* for a mirrored one — so mirrored records
//!    carry a foreign LSN space in the very column §11.1 sorts and prunes on.
//!    Archiving them under this node's `node_id` would put two incomparable
//!    LSN spaces under one key prefix.
//! 2. **Recovery reads only this node's rows.** [`JarchiveIndex::rows`] takes
//!    the `node_id` and bounds its range with
//!    [`keyspace::jarchive_node_range_start`]/`_end`, so a promoted node cannot
//!    adopt the source's rows as its own watermark even by accident.
//! 3. **A sealed segment with no originated records still advances the
//!    watermark**, publishing no object and writing no row. Without this, a
//!    passive chain follower — which originates nothing at all (D23) — would
//!    register an archive claim it could never satisfy, and the clamp would
//!    block its mirror's release forever. That is the silent countdown in its
//!    purest form, and the rule that avoids it is one line.
//!
//! The consequence to state rather than hide: **a mirrored record is archived
//! by the node that originated it, or not at all.** A promoted node inherits
//! the source's history in its journal but not in its archive; the source's own
//! `jarchive/{source}` rows cover it. If the source never ran a tailer, those
//! records reach the durable tier through the checkpoints that released them
//! and are absent from the archive — which is a gap in the source's archive
//! coverage, not something the promoted node can repair by relabelling another
//! node's LSNs as its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use orrery_protocol::{Lsn, NodeId};

use crate::archive::index::{recover_watermark, JarchiveIndex};
use crate::archive::object::{encode_object, sort_for_archive, ArchiveSortKey};
use crate::archive::store::ArchiveStore;
use crate::journal::{Journal, JournalError, StoredRecord};
use crate::keyspace::{JarchiveCellRange, JarchiveLsnSpan, JarchiveMetadata};

/// Why the tailer could not finish a pass.
///
/// Named rather than collapsed into a string, because #808 item 4 asks for the
/// failure to be *surfaced*: the three variants are three different things for
/// an operator to go and look at — the object store, the object store's read
/// path, and the metadata store — and the alarm names which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveStall {
    /// The object could not be uploaded.
    Upload(String),
    /// The object was uploaded and did not read back byte-identical — or did
    /// not read back at all.
    Verify(String),
    /// The object verified and the `jarchive/` row would not commit.
    Metadata(String),
    /// The journal could not be read.
    Journal(String),
    /// The records could not be encoded.
    Encode(String),
}

impl core::fmt::Display for ArchiveStall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Upload(m) => write!(f, "archive upload failed: {m}"),
            Self::Verify(m) => write!(f, "archive object failed verification: {m}"),
            Self::Metadata(m) => write!(f, "jarchive row would not commit: {m}"),
            Self::Journal(m) => write!(f, "journal read failed: {m}"),
            Self::Encode(m) => write!(f, "archive object encode failed: {m}"),
        }
    }
}

impl core::error::Error for ArchiveStall {}

impl ArchiveStall {
    /// The single word an operator greps for.
    #[must_use]
    pub const fn stage(&self) -> &'static str {
        match self {
            Self::Upload(_) => "upload",
            Self::Verify(_) => "verify",
            Self::Metadata(_) => "metadata",
            Self::Journal(_) => "journal",
            Self::Encode(_) => "encode",
        }
    }
}

/// What one pass of the tailer did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailerPass {
    /// No sealed segment was waiting. The tailer is caught up.
    Idle,
    /// A sealed segment held no locally originated records. No object was
    /// written and no row committed; the watermark advanced past it (rule 3
    /// of this module's §4).
    Skipped {
        /// The segment that held nothing of this node's.
        segment_seq: u64,
    },
    /// A segment was archived, verified, recorded and released.
    Published {
        /// The segment archived.
        segment_seq: u64,
        /// Rows written into the object.
        records: u64,
        /// Object size in bytes.
        bytes: u64,
    },
}

/// Retry and cadence parameters.
#[derive(Debug, Clone)]
pub struct ArchiveTailerConfig {
    /// How long to wait after a pass that found nothing sealed.
    pub idle_interval: Duration,
    /// The first backoff after a failed pass.
    pub backoff_initial: Duration,
    /// The ceiling the backoff doubles up to.
    ///
    /// Bounded rather than unbounded: the failure this backoff is waiting out
    /// costs journal disk for as long as it lasts, so the retry interval must
    /// not grow past the point where a recovered store is noticed promptly.
    pub backoff_max: Duration,
    /// Consecutive failures before the stall is logged at `warn` rather than
    /// `debug`. One transient upload error on a 20 s cadence is not an
    /// incident; three in a row is.
    pub alarm_after_failures: u32,
}

impl Default for ArchiveTailerConfig {
    fn default() -> Self {
        Self {
            idle_interval: Duration::from_secs(1),
            backoff_initial: Duration::from_millis(250),
            backoff_max: Duration::from_secs(30),
            alarm_after_failures: 3,
        }
    }
}

/// The operator-visible state of a tailer, readable without stopping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveTailerStatus {
    /// The next segment the tailer will try to archive.
    pub next_segment: u64,
    /// The watermark last reported to `note_archive_watermark`.
    pub watermark: Lsn,
    /// Objects published and verified since this tailer started.
    pub published: u64,
    /// Sealed segments that held nothing of this node's.
    pub skipped: u64,
    /// Consecutive failed passes. Zero after any pass that made progress.
    pub consecutive_failures: u32,
    /// Why the last pass failed, if it did.
    pub stall: Option<ArchiveStall>,
}

/// The archive tailer.
///
/// Owns no task of its own: [`ArchiveTailer::pass`] is one unit of work and
/// [`spawn_archive_tailer`] is the driver. Splitting them is what lets every
/// mutation test in `tests/archive_tailer.rs` drive the exact sequence it needs
/// — a failed verification, a crash between two steps — without racing a timer.
pub struct ArchiveTailer {
    journal: Arc<Journal>,
    store: Arc<dyn ArchiveStore>,
    index: Arc<dyn JarchiveIndex>,
    node_id: NodeId,
    key_prefix: String,
    config: ArchiveTailerConfig,
    next_segment: u64,
    watermark: Lsn,
    published: AtomicU64,
    skipped: AtomicU64,
    consecutive_failures: u32,
    stall: Option<ArchiveStall>,
}

/// The deterministic object key for one `(node_id, segment_seq)`.
///
/// Derived from those two and nothing else — no timestamp, no attempt counter,
/// no random suffix — because that is what makes a re-upload after a crash an
/// overwrite rather than a duplicate. `prefix` is the operator's bucket path
/// and is fixed for the life of a deployment.
#[must_use]
pub fn object_key(prefix: &str, node_id: &NodeId, segment_seq: u64) -> String {
    let node = hex(node_id.as_bytes());
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        format!("jarchive/{node}/{segment_seq:016x}.parquet")
    } else {
        format!("{prefix}/jarchive/{node}/{segment_seq:016x}.parquet")
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

impl ArchiveTailer {
    /// Open a tailer, recovering its watermark from this node's own
    /// `jarchive/` rows (#808 item 6).
    ///
    /// The journal's archive claim is registered here and the recovered
    /// watermark reported immediately, so the clamp is armed before the first
    /// pass runs: a tailer that starts and then fails must block release, not
    /// leave it unbounded until its first success.
    ///
    /// # Errors
    ///
    /// [`crate::checkpoint::CheckpointError`] if the `jarchive/` rows cannot be
    /// read. Refusing to start is the correct answer — a tailer that could not
    /// read its rows and started at zero would re-archive everything, and one
    /// that started at the tail would leave a gap no later pass fills.
    pub async fn open(
        journal: Arc<Journal>,
        store: Arc<dyn ArchiveStore>,
        index: Arc<dyn JarchiveIndex>,
        node_id: NodeId,
        key_prefix: impl Into<String>,
        config: ArchiveTailerConfig,
    ) -> Result<Self, crate::checkpoint::CheckpointError> {
        let floor = journal.released_floor();
        let recovered = recover_watermark(index.as_ref(), &node_id, floor).await?;
        journal.register_archive();
        journal.note_archive_watermark(recovered.watermark);
        tracing::info!(
            node = %node_id.fmt_short(),
            next_segment = recovered.next_segment,
            watermark = %recovered.watermark,
            floor = %floor,
            "archive tailer recovered its watermark from jarchive/ rows"
        );
        Ok(Self {
            journal,
            store,
            index,
            node_id,
            key_prefix: key_prefix.into(),
            config,
            next_segment: recovered.next_segment,
            watermark: recovered.watermark,
            published: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            consecutive_failures: 0,
            stall: None,
        })
    }

    /// The tailer's operator-visible state.
    #[must_use]
    pub fn status(&self) -> ArchiveTailerStatus {
        ArchiveTailerStatus {
            next_segment: self.next_segment,
            watermark: self.watermark,
            published: self.published.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            consecutive_failures: self.consecutive_failures,
            stall: self.stall.clone(),
        }
    }

    /// Whether `next_segment` is sealed: the journal's durable cursor has left
    /// it and will never return.
    fn sealed(&self) -> bool {
        self.journal.committed().segment > self.next_segment
    }

    /// Run one pass.
    ///
    /// # Errors
    ///
    /// [`ArchiveStall`] naming the stage that failed. The watermark is
    /// unchanged on every error path — that is the guarded invariant, and it
    /// holds by construction: the only assignment to `self.watermark` is after
    /// the `jarchive/` row commits.
    pub async fn pass(&mut self) -> Result<TailerPass, ArchiveStall> {
        if !self.sealed() {
            return Ok(TailerPass::Idle);
        }
        let segment_seq = self.next_segment;
        let outcome = self.publish_segment(segment_seq).await;
        match &outcome {
            Ok(_) => {
                self.consecutive_failures = 0;
                self.stall = None;
            }
            Err(stall) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.stall = Some(stall.clone());
                self.report_stall(segment_seq, stall);
            }
        }
        outcome
    }

    /// The `info`/`warn` pair, in the shape `release_journal` uses: one line
    /// per pass, at a level that says whether an operator has to act.
    fn report_stall(&self, segment_seq: u64, stall: &ArchiveStall) {
        let gap = self.journal.archive_gap(self.journal.committed());
        let bytes_behind = gap.map_or(0, |g| g.bytes_behind);
        let segments_behind = gap.map_or(0, |g| g.segments_behind);
        if self.consecutive_failures >= self.config.alarm_after_failures {
            // The §15 alarm's tailer half. The journal is not shrinking, the
            // reason is this stall, and `bytes_behind` is what the journal disk
            // is being asked to hold until it clears.
            tracing::warn!(
                stage = stall.stage(),
                %stall,
                segment_seq,
                watermark = %self.watermark,
                segments_behind,
                bytes_behind,
                failures = self.consecutive_failures,
                "archive tailer is stalled; the journal cannot release past its watermark"
            );
        } else {
            tracing::debug!(
                stage = stall.stage(),
                %stall,
                segment_seq,
                failures = self.consecutive_failures,
                "archive tailer pass failed; retrying"
            );
        }
    }

    async fn publish_segment(&mut self, segment_seq: u64) -> Result<TailerPass, ArchiveStall> {
        let mut records = self.read_segment(segment_seq)?;

        // Rule 3 of §4: a sealed segment this node originated nothing in is
        // archived by advancing past it. No object, no row — there is nothing
        // to record — but the watermark must still move, or a node that
        // originates nothing never releases.
        if records.is_empty() {
            self.advance_past(segment_seq);
            self.skipped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                segment_seq,
                watermark = %self.watermark,
                "sealed segment held no locally originated records; watermark advanced"
            );
            return Ok(TailerPass::Skipped { segment_seq });
        }

        sort_for_archive(&mut records);
        let record_count = records.len() as u64;
        let lsn_span = JarchiveLsnSpan {
            // The records are sorted by `(grid, cell, lsn)`, so the LSN span is
            // taken over the whole slice rather than from its ends.
            start: records
                .iter()
                .map(|r| r.lsn)
                .min()
                .unwrap_or(Lsn::new(segment_seq, 0)),
            end: records
                .iter()
                .map(|r| r.lsn)
                .max()
                .unwrap_or(Lsn::new(segment_seq, 0)),
        };
        let cell_ranges = cell_ranges(&records);

        let key = object_key(&self.key_prefix, &self.node_id, segment_seq);

        // Steps (a) and (b) — encode, upload, re-read, hash — are CPU and
        // blocking IO measured in hundreds of milliseconds for a 128 MiB
        // segment, and this task shares a runtime with the bulk write path
        // whose D16 budget is 2 ms per journal commit. They therefore run on a
        // blocking thread. Step (c) is genuinely async (an FDB transaction) and
        // stays here; the journal read at the top is also blocking, but it is
        // the same synchronous `scan_originated_from` every other caller in
        // this crate performs on the runtime and moving it would mean lifting
        // the whole borrow of `self.journal` into a closure — not free, and not
        // this change's to spend. It is bounded by one segment either way.
        //
        // Keeping (a)/(b) as one closure and (c) outside it is also what keeps
        // the ordering discipline legible in this function rather than buried
        // in a spawned block.
        let (bytes_len, expected) = {
            let store = Arc::clone(&self.store);
            let key = key.clone();
            tokio::task::spawn_blocking(move || publish_bytes(store.as_ref(), &key, &records))
                .await
                .map_err(|e| ArchiveStall::Encode(format!("archive publish task: {e}")))??
        };

        // (c) The metadata row, committed. Only now is the object a durable,
        // findable fact.
        let metadata = JarchiveMetadata {
            // The **store-relative key**, not a fully qualified URI, even
            // though #807's field doc shows one by example. The scheme, host
            // and root are the store's configuration; stamping them into every
            // row would mean that moving a bucket, changing an endpoint or
            // migrating from the filesystem backend to S3 invalidated every
            // row ever written, for a value the reader already has to have a
            // configured store to fetch with. What the row must carry is the
            // part no configuration can supply — which object, under which
            // node, for which segment — and that is the key.
            object_key: key.clone(),
            cell_ranges,
            lsn_span,
            checksum: expected,
        };
        self.index
            .put_row(&self.node_id, segment_seq, &metadata)
            .await
            .map_err(|e| ArchiveStall::Metadata(e.to_string()))?;

        // (d) And only now may the journal release past it.
        self.advance_past(segment_seq);
        self.published.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            segment_seq,
            key = %key,
            records = record_count,
            bytes = bytes_len,
            watermark = %self.watermark,
            "archived a sealed journal segment and advanced the archive watermark"
        );
        Ok(TailerPass::Published {
            segment_seq,
            records: record_count,
            bytes: bytes_len,
        })
    }

    /// Move the watermark to the start of the next segment.
    ///
    /// `Lsn::new(seq + 1, 0)` rather than the last archived record's LSN:
    /// `release_before(w)` drops records strictly *below* `w`, so the whole of
    /// segment `seq` becomes releasable exactly when the watermark reaches the
    /// first position of `seq + 1`.
    fn advance_past(&mut self, segment_seq: u64) {
        self.next_segment = segment_seq.saturating_add(1);
        self.watermark = Lsn::new(self.next_segment, 0);
        self.journal.note_archive_watermark(self.watermark);
    }

    /// Read the locally originated records in one sealed segment.
    fn read_segment(&self, segment_seq: u64) -> Result<Vec<StoredRecord>, ArchiveStall> {
        let from = Lsn::new(segment_seq, 0);
        let mut out = Vec::new();
        for entry in self.journal.scan_originated_from(from) {
            match entry {
                Ok(stored) if stored.lsn.segment == segment_seq => out.push(stored),
                // The scan is in LSN order, so the first record past the
                // segment ends it. Stopping here is what makes the buffer one
                // segment rather than the whole journal tail.
                Ok(_) => break,
                // A journal released past this segment while the tailer was
                // behind it. That is not a tailer failure: the clamp is what
                // prevents it, and if it happened the clamp was off. Report it
                // as the stall it is rather than looping on a segment whose
                // records no longer exist.
                Err(JournalError::Released { requested, floor }) => {
                    return Err(ArchiveStall::Journal(format!(
                        "segment {segment_seq} was released before it was archived \
                         (scan from {requested}, floor {floor})"
                    )));
                }
                Err(error) => return Err(ArchiveStall::Journal(error.to_string())),
            }
        }
        Ok(out)
    }
}

/// Encode, upload, and verify one object — the blocking half of a pass.
///
/// Returns the object's length and its BLAKE3 digest. Everything here is
/// synchronous by design (see [`ArchiveStore`]) and everything here is what a
/// 128 MiB segment makes expensive, which is why it is the part that runs off
/// the async runtime.
///
/// **The verification re-reads the store.** It would be one line shorter to
/// return `blake3::hash(&bytes)` from the encode and call it verified;
/// docs/08-persistence.md §11.3 exists to say that is not a verification, and
/// `a_failed_verification_leaves_the_watermark_and_the_records_where_they_were`
/// injects a fault on the *read* path specifically so the shorter version
/// fails it.
fn publish_bytes(
    store: &dyn ArchiveStore,
    key: &str,
    records: &[StoredRecord],
) -> Result<(u64, [u8; 32]), ArchiveStall> {
    let bytes = encode_object(records).map_err(|e| ArchiveStall::Encode(e.to_string()))?;
    let expected = blake3::hash(&bytes);

    // (a) The object, written.
    store
        .put(key, &bytes)
        .map_err(|e| ArchiveStall::Upload(e.to_string()))?;

    // (b) The object, verified — from the store, not from the buffer above.
    let stored = store
        .get(key)
        .map_err(|e| ArchiveStall::Verify(e.to_string()))?
        .ok_or_else(|| {
            ArchiveStall::Verify(format!(
                "{key} is absent from the store after a successful put"
            ))
        })?;
    let observed = blake3::hash(&stored);
    if observed != expected {
        return Err(ArchiveStall::Verify(format!(
            "{key} read back with digest {}, expected {}",
            observed.to_hex(),
            expected.to_hex()
        )));
    }
    Ok((bytes.len() as u64, *expected.as_bytes()))
}

/// The sorted records' `(grid, cell)` coverage, as half-open ranges.
///
/// Coalesced: consecutive cells in one grid become one range, so an object over
/// a contiguous shard costs one entry rather than one per cell. The records are
/// already in `(grid, cell, lsn)` order when this runs, which is why a single
/// forward pass suffices and why this is called after `sort_for_archive`.
///
/// **The one cell a half-open `u64` end cannot express.** `end` is a `CellId`,
/// so the largest exclusive bound representable is `u64::MAX` — which does not
/// cover the cell whose own encoding is `u64::MAX` (reachable: a level-21 cell
/// with every Morton bit set, under the sentinel at bit 63). Rather than
/// silently drop that cell from the coverage it is in, the range saturates and
/// **a range whose `end` is `u64::MAX` is read as covering `u64::MAX` too**.
/// That reader rule is stated in docs/08-persistence.md §11.6 and is the only
/// place `cell_ranges` is not a plain half-open interval.
fn cell_ranges(records: &[StoredRecord]) -> Vec<JarchiveCellRange> {
    /// The exclusive bound covering `cell`, saturating at the top of the
    /// `u64` space (see this function's docs).
    fn end_after(cell: u64) -> Option<orrery_protocol::CellId> {
        orrery_protocol::CellId::from_bits(cell.saturating_add(1))
    }

    let mut ranges: Vec<JarchiveCellRange> = Vec::new();
    for record in records {
        let key = ArchiveSortKey::of(record);
        let grid = record.record.grid;
        let Some(end) = end_after(key.cell) else {
            // `CellId::to_bits` is never zero and the successor saturates
            // upward, so this is unreachable. Skipping keeps a malformed
            // record from taking the tailer down; the metadata it would have
            // widened is only a pruning hint, and a narrower one costs a
            // reader a fetch rather than a wrong answer.
            tracing::warn!(
                cell = key.cell,
                "archive cell range skipped a record with an unrepresentable cell"
            );
            continue;
        };
        match ranges.last_mut() {
            // Extend when this record's cell is the one the range already ends
            // at (a repeat) or the next one along (contiguous). The records
            // arrive in `(grid, cell)` order, so a non-contiguous cell starts a
            // new range and never reopens a closed one.
            Some(last) if last.grid == grid && last.end.to_bits() >= key.cell => {
                if last.end.to_bits() < end.to_bits() {
                    last.end = end;
                }
            }
            _ => ranges.push(JarchiveCellRange {
                grid,
                start: record.record.cell,
                end,
            }),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, PersistId, RecordKind, Tick};

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh::SecretKey::from_bytes(&seed).public()
    }

    fn stored(grid: u32, cell: u64, offset: u64) -> StoredRecord {
        let lsn = Lsn::new(0, offset);
        StoredRecord {
            lsn,
            record: JournalRecord {
                lsn,
                cell: CellId::from_bits(cell).expect("nonzero cell"),
                grid: GridId(grid),
                entity: PersistId::new(offset),
                tick: Tick::new(0),
                epoch: Epoch::new(0),
                author: node(1),
                kind: RecordKind::ComponentDiff,
                crc: 0,
                payload: bytes::Bytes::new(),
            },
            encoding: 1,
        }
    }

    #[test]
    fn the_object_key_is_a_pure_function_of_the_node_and_the_segment() {
        let this = node(3);
        let key = object_key("bucket/prefix", &this, 42);
        assert_eq!(
            key,
            object_key("bucket/prefix/", &this, 42),
            "a trailing slash in the prefix is not a different key"
        );
        assert!(
            key.ends_with("/000000000000002a.parquet"),
            "the segment is zero-padded hex so keys sort: {key}"
        );
        assert!(key.starts_with("bucket/prefix/jarchive/"));
        assert_ne!(key, object_key("bucket/prefix", &this, 43));
        assert_ne!(key, object_key("bucket/prefix", &node(4), 42));
        assert_eq!(
            object_key("", &this, 0),
            format!("jarchive/{}/0000000000000000.parquet", hex(this.as_bytes())),
            "an empty prefix leaves no leading slash"
        );
    }

    #[test]
    fn contiguous_cells_coalesce_and_gaps_and_grids_do_not() {
        let records = vec![
            stored(0, 10, 0),
            stored(0, 10, 64),
            stored(0, 11, 128),
            stored(0, 20, 192),
            stored(1, 21, 256),
        ];
        let ranges = cell_ranges(&records);
        let shape: Vec<(u32, u64, u64)> = ranges
            .iter()
            .map(|r| (r.grid.0, r.start.to_bits(), r.end.to_bits()))
            .collect();
        assert_eq!(
            shape,
            vec![(0, 10, 12), (0, 20, 21), (1, 21, 22)],
            "10 and 11 coalesce, the gap to 20 opens a range, and grid 1 never merges with grid 0"
        );
        for record in &records {
            assert!(
                ranges.iter().any(|r| r.grid == record.record.grid
                    && record.record.cell >= r.start
                    && record.record.cell < r.end),
                "every record is covered"
            );
        }
    }

    #[test]
    fn the_top_of_the_cell_space_saturates_rather_than_dropping_the_record() {
        let ranges = cell_ranges(&[stored(0, u64::MAX, 0)]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start.to_bits(), u64::MAX);
        assert_eq!(
            ranges[0].end.to_bits(),
            u64::MAX,
            "the one cell a half-open u64 end cannot express saturates; \
             a range ending at u64::MAX is read as covering it"
        );
    }
}

/// A running tailer.
pub struct ArchiveTailerHandle {
    shutdown: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

impl ArchiveTailerHandle {
    /// Stop the tailer and await its task.
    pub async fn shutdown(self) {
        self.shutdown.notify_waiters();
        let _ = self.join.await;
    }
}

/// Drive a tailer on its own task: pass, sleep, repeat, with exponential
/// backoff on failure.
///
/// The backoff is what #808 item 4 asks for and the ceiling is why it is
/// bounded: every second of backoff is a second of journal the clamp will not
/// let go of, so the retry interval stops doubling at
/// [`ArchiveTailerConfig::backoff_max`] rather than growing to hours.
#[must_use]
pub fn spawn_archive_tailer(mut tailer: ArchiveTailer) -> ArchiveTailerHandle {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_task = Arc::clone(&shutdown);
    let join = tokio::spawn(async move {
        let mut backoff = tailer.config.backoff_initial;
        loop {
            let wait = match tailer.pass().await {
                Ok(TailerPass::Idle) => {
                    backoff = tailer.config.backoff_initial;
                    tailer.config.idle_interval
                }
                Ok(_) => {
                    backoff = tailer.config.backoff_initial;
                    // A pass that made progress goes straight round again:
                    // there may be more sealed segments waiting, and catching
                    // up is exactly what shrinks the journal.
                    Duration::ZERO
                }
                Err(_) => {
                    let wait = backoff;
                    backoff = (backoff * 2).min(tailer.config.backoff_max);
                    wait
                }
            };
            tokio::select! {
                () = shutdown_task.notified() => break,
                () = tokio::time::sleep(wait) => {}
            }
        }
    });
    ArchiveTailerHandle { shutdown, join }
}

#[cfg(test)]
mod bound_tests {
    use crate::journal::StoredRecord;

    /// The memory bound docs/08-persistence.md §11.6 states is arithmetic over
    /// `size_of::<StoredRecord>()`, so it is pinned here rather than left to
    /// drift with the struct.
    ///
    /// The claim: buffering one 128 MiB segment costs at most
    /// `segment_size / MIN_ACCOUNTED_SPAN` records of struct overhead, on top
    /// of at most `segment_size` of payload. If `StoredRecord` grows, this
    /// fails and the doc's number is updated with it.
    #[test]
    fn the_documented_per_segment_memory_bound_matches_the_struct() {
        /// `raw.rs`'s `encoded_len`: `payload.len() + 64`.
        const MIN_ACCOUNTED_SPAN: u64 = 64;
        let struct_bytes = u64::try_from(core::mem::size_of::<StoredRecord>()).expect("usize fits");
        assert_eq!(
            struct_bytes, 152,
            "docs/08 §11.6 quotes 152 B per buffered record; update it together with this"
        );
        let segment = crate::journal::DEFAULT_SEGMENT_SIZE;
        let max_records = segment / MIN_ACCOUNTED_SPAN;
        assert_eq!(max_records, 2_097_152, "128 MiB / 64 B");
        let worst_case_mib = (max_records * struct_bytes + segment) / (1024 * 1024);
        assert_eq!(
            worst_case_mib, 432,
            "§11.6's stated worst case for the buffer, before the encoded object"
        );
    }
}
