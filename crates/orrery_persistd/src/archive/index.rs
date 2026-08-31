//! The `jarchive/{node_id}/{segment_seq}` metadata rows: the durable record of
//! what is archived, and the thing a restarting node re-derives its watermark
//! from (#808 item 6).
//!
//! #807 landed the key and the value ([`crate::keyspace::jarchive_key`],
//! [`crate::keyspace::JarchiveMetadata`]); this is the read/write seam over
//! them. It is a trait for the same reason [`crate::archive::ArchiveStore`] is:
//! the tailer's whole crash-safety argument is about the *ordering* of a write
//! against an upload, and that argument is testable against any store that can
//! set a key and scan a range.
//!
//! **The row is the authority, and the watermark is derived from it.** Nothing
//! persists the archive watermark as a number of its own. That is deliberate —
//! a separately persisted watermark is a second durable fact that can disagree
//! with the rows, and the disagreement would be discovered as either a
//! re-archive or a permanently blocked release. `recover_watermark` below reads
//! the rows and computes the number, every time.

use orrery_protocol::NodeId;

use crate::checkpoint::CheckpointError;
use crate::keyspace::{self, JarchiveMetadata};

/// One archived segment, as recorded durably.
///
/// A named pair rather than `(u64, JarchiveMetadata)`: this is the unit both
/// the recovery scan and the sweep readers of #615 iterate, and the segment
/// number is not incidental to the metadata — it is the row's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarchiveRow {
    /// The logical journal segment this object covers — `Lsn::segment`.
    pub segment_seq: u64,
    /// What #807's schema records about the object.
    pub metadata: JarchiveMetadata,
}

/// Read and write access to one node's `jarchive/` rows.
#[async_trait::async_trait]
pub trait JarchiveIndex: Send + Sync {
    /// Commit the metadata row for one archived segment.
    ///
    /// Idempotent by key: committing the same `(node_id, segment_seq)` twice
    /// leaves one row. That is what makes the crash window between the upload
    /// and this call cost a retry rather than a duplicate row.
    ///
    /// # Errors
    ///
    /// [`CheckpointError`] if the row is not durably committed.
    async fn put_row(
        &self,
        node_id: &NodeId,
        segment_seq: u64,
        metadata: &JarchiveMetadata,
    ) -> Result<(), CheckpointError>;

    /// Every row under `jarchive/{node_id}/`, in `segment_seq` order.
    ///
    /// **Scoped to one node by construction.** The range bounds come from
    /// [`keyspace::jarchive_node_range_start`] and `_end`, so an implementation
    /// cannot accidentally serve another node's rows — which is #808 item 7's
    /// rule made mechanical rather than remembered (see
    /// [`crate::archive::tailer`]).
    ///
    /// # Errors
    ///
    /// [`CheckpointError`] if the range could not be read or a value could not
    /// be decoded.
    async fn rows(&self, node_id: &NodeId) -> Result<Vec<JarchiveRow>, CheckpointError>;
}

/// The next segment a node should archive, and the watermark that follows from
/// its rows.
///
/// `floor` is the journal's own retention floor, and it is the second term for
/// a reason worth stating: a node that ran without `--archive-retention` and is
/// then started with it has released segments no object covers. Those records
/// are gone — no tailer can archive them — so resuming from `max(row) + 1`
/// would either re-scan a released range (`JournalError::Released`) or, if the
/// range were empty, start at zero and block release forever. Resuming at the
/// floor's segment says the honest thing instead: this node's archive begins
/// where its journal still does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveredWatermark {
    /// The first segment the tailer should try to archive.
    pub next_segment: u64,
    /// The watermark to report to `note_archive_watermark`: the start of
    /// `next_segment`, i.e. everything below it is verified-or-gone.
    pub watermark: orrery_protocol::Lsn,
}

/// Derive a node's archive watermark from its own `jarchive/` rows.
///
/// # Errors
///
/// [`CheckpointError`] if the rows cannot be read.
pub async fn recover_watermark(
    index: &dyn JarchiveIndex,
    node_id: &NodeId,
    floor: orrery_protocol::Lsn,
) -> Result<RecoveredWatermark, CheckpointError> {
    let rows = index.rows(node_id).await?;
    // **Gaps in the row range are normal, and taking the maximum is still
    // right.** A sealed segment holding no locally originated records writes no
    // object and no row (the tailer's rule 3), so the rows are not contiguous
    // in general. What *is* invariant is that the tailer only ever moves to
    // `seq + 1` after `seq` has been either published-and-verified or found
    // empty: it never jumps forward past a segment it failed on. So every
    // segment below the highest row is accounted for — archived, or empty —
    // and `max + 1` is the first one that is not.
    //
    // Walking for a gap and resuming at it would therefore be *wrong*, not
    // merely slower: it would re-archive every skipped segment on every
    // restart, forever, since a skipped segment never gains a row.
    let archived_through = rows.iter().map(|row| row.segment_seq).max();
    let next_segment = archived_through
        .map_or(floor.segment, |seq| seq.saturating_add(1))
        .max(floor.segment);
    Ok(RecoveredWatermark {
        next_segment,
        watermark: orrery_protocol::Lsn::new(next_segment, 0),
    })
}

/// An in-memory [`JarchiveIndex`] for tests and single-process harnesses.
#[derive(Debug, Default)]
pub struct MemJarchiveIndex {
    rows: std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
    /// Set to fail every `put_row`, to exercise the metadata-commit failure
    /// arm of the ordering discipline.
    pub fail_put: std::sync::atomic::AtomicBool,
}

impl MemJarchiveIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many rows exist across every node — the count the duplicate-row
    /// assertions read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.lock().expect("jarchive index lock").len()
    }

    /// Whether the index holds no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl JarchiveIndex for MemJarchiveIndex {
    async fn put_row(
        &self,
        node_id: &NodeId,
        segment_seq: u64,
        metadata: &JarchiveMetadata,
    ) -> Result<(), CheckpointError> {
        if self.fail_put.load(std::sync::atomic::Ordering::Acquire) {
            return Err(CheckpointError::Store(
                "injected jarchive metadata commit failure".into(),
            ));
        }
        let value = keyspace::encode_jarchive_metadata(metadata)?;
        self.rows
            .lock()
            .expect("jarchive index lock")
            .insert(keyspace::jarchive_key(node_id, segment_seq).to_vec(), value);
        Ok(())
    }

    async fn rows(&self, node_id: &NodeId) -> Result<Vec<JarchiveRow>, CheckpointError> {
        let start = keyspace::jarchive_node_range_start(node_id);
        let end = keyspace::jarchive_node_range_end(node_id);
        let guard = self.rows.lock().expect("jarchive index lock");
        guard
            .range(start..end)
            .map(|(key, value)| {
                let (_, segment_seq) = keyspace::decode_jarchive_key(key).ok_or_else(|| {
                    CheckpointError::Store("undecodable jarchive key in index".into())
                })?;
                Ok(JarchiveRow {
                    segment_seq,
                    metadata: keyspace::decode_jarchive_metadata(value)?,
                })
            })
            .collect()
    }
}

/// A FoundationDB-backed [`JarchiveIndex`] (`fdb` feature).
///
/// The `jarchive/` family lives in the same keyspace as `ckpt/` and `world/`
/// (§6), so the durable record of what is archived is durable in the same
/// place, and by the same means, as the checkpoints that bound the other half
/// of the release precondition.
#[cfg(feature = "fdb")]
pub struct FdbJarchiveIndex {
    db: std::sync::Arc<foundationdb::Database>,
}

#[cfg(feature = "fdb")]
impl FdbJarchiveIndex {
    /// Wrap a connected database handle.
    #[must_use]
    pub fn new(db: std::sync::Arc<foundationdb::Database>) -> Self {
        Self { db }
    }
}

#[cfg(feature = "fdb")]
#[async_trait::async_trait]
impl JarchiveIndex for FdbJarchiveIndex {
    async fn put_row(
        &self,
        node_id: &NodeId,
        segment_seq: u64,
        metadata: &JarchiveMetadata,
    ) -> Result<(), CheckpointError> {
        let key = keyspace::jarchive_key(node_id, segment_seq);
        let value = keyspace::encode_jarchive_metadata(metadata)?;
        // A blind `set`, in its own transaction. There is nothing to read
        // first: the key is derived from `(node_id, segment_seq)` and the value
        // from the object those two name, so a concurrent writer of the same
        // key is writing the same bytes. Adding a read would buy a conflict
        // range and no invariant.
        self.db
            .run(|trx, _| {
                let value = value.clone();
                async move {
                    trx.set(&key, &value);
                    Ok(())
                }
            })
            .await
            .map_err(|e| CheckpointError::Store(format!("jarchive row commit: {e}")))
    }

    async fn rows(&self, node_id: &NodeId) -> Result<Vec<JarchiveRow>, CheckpointError> {
        use futures::TryStreamExt;
        let start = keyspace::jarchive_node_range_start(node_id);
        let end = keyspace::jarchive_node_range_end(node_id);
        let raw: Vec<(Vec<u8>, Vec<u8>)> = self
            .db
            .run(|trx, _| {
                let start = start.clone();
                let end = end.clone();
                async move {
                    let range = foundationdb::RangeOption::from((start.as_slice(), end.as_slice()));
                    let mut out = Vec::new();
                    let mut stream = trx.get_ranges_keyvalues(range, false);
                    while let Some(kv) = stream.try_next().await? {
                        out.push((kv.key().to_vec(), kv.value().to_vec()));
                    }
                    Ok(out)
                }
            })
            .await
            .map_err(|e| CheckpointError::Store(format!("jarchive range read: {e}")))?;
        raw.into_iter()
            .map(|(key, value)| {
                let (_, segment_seq) = keyspace::decode_jarchive_key(&key)
                    .ok_or_else(|| CheckpointError::Store("undecodable jarchive key".into()))?;
                Ok(JarchiveRow {
                    segment_seq,
                    metadata: keyspace::decode_jarchive_metadata(&value)?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::Lsn;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh::SecretKey::from_bytes(&seed).public()
    }

    fn meta(seq: u64) -> JarchiveMetadata {
        JarchiveMetadata {
            object_key: format!("file:///archive/{seq}"),
            cell_ranges: Vec::new(),
            lsn_span: keyspace::JarchiveLsnSpan {
                start: Lsn::new(seq, 0),
                end: Lsn::new(seq, 4096),
            },
            checksum: [seq as u8; 32],
        }
    }

    #[tokio::test]
    async fn rows_are_scoped_to_one_node() {
        let index = MemJarchiveIndex::new();
        index.put_row(&node(1), 0, &meta(0)).await.expect("put");
        index.put_row(&node(1), 1, &meta(1)).await.expect("put");
        index.put_row(&node(2), 9, &meta(9)).await.expect("put");
        let mine = index.rows(&node(1)).await.expect("rows");
        assert_eq!(
            mine.iter().map(|r| r.segment_seq).collect::<Vec<_>>(),
            vec![0, 1],
            "another node's rows are never served as this node's"
        );
        assert_eq!(index.len(), 3);
    }

    #[tokio::test]
    async fn the_same_row_written_twice_stays_one_row() {
        let index = MemJarchiveIndex::new();
        index.put_row(&node(1), 4, &meta(4)).await.expect("put");
        index.put_row(&node(1), 4, &meta(4)).await.expect("re-put");
        assert_eq!(index.rows(&node(1)).await.expect("rows").len(), 1);
    }

    #[tokio::test]
    async fn an_empty_range_recovers_to_the_retention_floor_not_to_zero() {
        let index = MemJarchiveIndex::new();
        let recovered = recover_watermark(&index, &node(1), Lsn::new(6, 512))
            .await
            .expect("recover");
        assert_eq!(recovered.next_segment, 6);
        assert_eq!(recovered.watermark, Lsn::new(6, 0));
    }

    #[tokio::test]
    async fn rows_resume_after_the_highest_archived_segment() {
        let index = MemJarchiveIndex::new();
        for seq in 0..3 {
            index.put_row(&node(1), seq, &meta(seq)).await.expect("put");
        }
        let recovered = recover_watermark(&index, &node(1), Lsn::new(0, 0))
            .await
            .expect("recover");
        assert_eq!(recovered.next_segment, 3);
        assert_eq!(recovered.watermark, Lsn::new(3, 0));
    }

    #[tokio::test]
    async fn a_floor_above_every_row_wins_the_maximum() {
        let index = MemJarchiveIndex::new();
        index.put_row(&node(1), 1, &meta(1)).await.expect("put");
        let recovered = recover_watermark(&index, &node(1), Lsn::new(9, 0))
            .await
            .expect("recover");
        assert_eq!(
            recovered.next_segment, 9,
            "a journal released past the archive resumes at its floor"
        );
    }
}
