//! The journal-to-archive tailer (#808), and the stage it guards: **the
//! archive watermark never advances past an object that is not verifiably in
//! the store.**
//!
//! Every test here drives [`ArchiveTailer::pass`] by hand rather than the
//! spawned driver, so a failure is a deterministic assertion rather than a
//! race against a backoff timer. The driver's own loop is one `match` over the
//! same passes.
//!
//! The journals are opened with a **4 KiB** logical segment
//! (`Journal::open_with_segment_size`) instead of D19's 128 MiB, because
//! sealing a segment at the default width means writing 128 MiB per segment.
//! Nothing else about the tailer changes with the width: `Lsn::segment` is
//! minted by the same `advance`/`successor` pair either way.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use orrery_persistd::archive::{
    decode_object, object_key, ArchiveStall, ArchiveStore, ArchiveStoreError, ArchiveTailer,
    ArchiveTailerConfig, FsArchiveStore, JarchiveIndex, MemJarchiveIndex, TailerPass,
};
use orrery_persistd::journal::{
    AdaptiveCommitMode, ArchiveClaimState, GroupCommitConfig, ReleaseBlocked,
};
use orrery_persistd::{payload_crc, Journal, JournalConfig};
use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, NodeId, PersistId, RecordKind};

/// Small enough that a few hundred records seal several segments.
const TEST_SEGMENT: u64 = 4096;

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn config(dir: &std::path::Path) -> JournalConfig {
    JournalConfig {
        dir: dir.to_path_buf(),
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::AlwaysBatch,
            batch_window: std::time::Duration::from_micros(200),
            ..GroupCommitConfig::default()
        },
    }
}

fn open(dir: &std::path::Path) -> Journal {
    Journal::open_with_segment_size(&config(dir), TEST_SEGMENT).expect("open journal")
}

/// A record whose cell varies with `entity`, so an object's `cell_ranges` and
/// the `(grid, cell, lsn)` re-sort are exercised rather than degenerate.
fn record(entity: u64) -> JournalRecord {
    let payload = vec![u8::try_from(entity % 251).unwrap_or(0); 128];
    JournalRecord {
        lsn: Lsn::new(0, 0),
        // Three distinct cells, cycled, so records arrive interleaved in LSN
        // order and the archive has something to re-sort.
        cell: CellId::from_bits(CellId::ROOT.to_bits() + (entity % 3)).expect("nonzero cell"),
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: orrery_protocol::Tick::new(entity),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::ComponentDiff,
        crc: payload_crc(&payload),
        payload: bytes::Bytes::from(payload),
    }
}

/// Append `n` records and await the last one's durability barrier, so
/// `committed()` reflects every one of them.
async fn fill(journal: &Journal, n: usize) -> Vec<Lsn> {
    let mut lsns = Vec::with_capacity(n);
    let mut last = None;
    for i in 0..n {
        let handle = journal.append(record(i as u64)).expect("append");
        lsns.push(handle.lsn());
        last = Some(handle);
    }
    last.expect("at least one record")
        .committed()
        .await
        .expect("durable");
    lsns
}

// ── A store that can be told to fail, on either side ─────────────────────

/// An [`ArchiveStore`] wrapping [`FsArchiveStore`] with two independent
/// faults: refuse uploads, and corrupt what `get` returns.
///
/// The corruption is applied on **read**, not on write, which is the point:
/// the tailer hashed the bytes it uploaded, so a fault that only changed the
/// stored bytes would be caught by any implementation, including one that
/// re-hashed its own buffer. Corrupting the read path is what distinguishes a
/// verification that re-reads the store from one that does not.
struct FaultyStore {
    inner: FsArchiveStore,
    fail_upload: AtomicBool,
    corrupt_reads: AtomicBool,
    unreachable: AtomicBool,
    uploads: AtomicU64,
    keys: Mutex<Vec<String>>,
}

impl FaultyStore {
    fn new(root: &std::path::Path) -> Self {
        Self {
            inner: FsArchiveStore::open(root).expect("open store"),
            fail_upload: AtomicBool::new(false),
            corrupt_reads: AtomicBool::new(false),
            unreachable: AtomicBool::new(false),
            uploads: AtomicU64::new(0),
            keys: Mutex::new(Vec::new()),
        }
    }

    fn uploaded_keys(&self) -> Vec<String> {
        self.keys.lock().expect("keys lock").clone()
    }
}

impl ArchiveStore for FaultyStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), ArchiveStoreError> {
        if self.unreachable.load(Ordering::Acquire) {
            return Err(ArchiveStoreError(
                "connection refused: archive endpoint is unreachable".into(),
            ));
        }
        if self.fail_upload.load(Ordering::Acquire) {
            return Err(ArchiveStoreError("injected upload failure".into()));
        }
        self.uploads.fetch_add(1, Ordering::Relaxed);
        self.keys.lock().expect("keys lock").push(key.to_owned());
        self.inner.put(key, bytes)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ArchiveStoreError> {
        if self.unreachable.load(Ordering::Acquire) {
            return Err(ArchiveStoreError(
                "connection refused: archive endpoint is unreachable".into(),
            ));
        }
        let got = self.inner.get(key)?;
        if self.corrupt_reads.load(Ordering::Acquire) {
            return Ok(got.map(|mut bytes| {
                if let Some(first) = bytes.first_mut() {
                    *first ^= 0xff;
                }
                bytes
            }));
        }
        Ok(got)
    }
}

async fn tailer(
    journal: &Arc<Journal>,
    store: &Arc<FaultyStore>,
    index: &Arc<MemJarchiveIndex>,
    node: NodeId,
) -> ArchiveTailer {
    ArchiveTailer::open(
        Arc::clone(journal),
        Arc::clone(store) as Arc<dyn ArchiveStore>,
        Arc::clone(index) as Arc<dyn JarchiveIndex>,
        node,
        "",
        ArchiveTailerConfig {
            alarm_after_failures: 1,
            ..ArchiveTailerConfig::default()
        },
    )
    .await
    .expect("tailer opens")
}

// ── Mutation direction 1: verification fails, the watermark does not move ──

/// **The guarded stage, stated as an assertion.** A store that accepts the
/// upload and then hands back different bytes must leave the watermark exactly
/// where it was, leave the `jarchive/` range empty, keep `release_before`
/// blocked with [`ReleaseBlocked::ArchiveLag`], and leave every record
/// readable through `scan_from`.
///
/// The corruption is on the *read* path, so nothing but a verification that
/// re-reads the store can catch it (docs/08-persistence.md §11.3: "a checksum
/// nobody re-reads is not a verification").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_verification_leaves_the_watermark_and_the_records_where_they_were() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(open(dir.path()));
    let lsns = fill(&journal, 200).await;
    assert!(
        journal.committed().segment > 0,
        "the fixture must seal at least one segment"
    );

    let store = Arc::new(FaultyStore::new(objects.path()));
    let index = Arc::new(MemJarchiveIndex::new());
    let node = test_node(1);
    let mut tailer = tailer(&journal, &store, &index, node).await;
    let before = tailer.status().watermark;
    assert_eq!(before, Lsn::new(0, 0));

    store.corrupt_reads.store(true, Ordering::Release);
    let stall = tailer.pass().await.expect_err("verification must fail");
    assert!(
        matches!(stall, ArchiveStall::Verify(_)),
        "the failure is named as a verification failure, not swallowed: {stall}"
    );
    assert_eq!(stall.stage(), "verify");

    // The object *was* uploaded. That is the interesting part: the upload
    // succeeding is not what advances the watermark.
    assert_eq!(store.uploads.load(Ordering::Relaxed), 1);
    assert_eq!(
        tailer.status().watermark,
        before,
        "the watermark does not advance past an unverified object"
    );
    assert!(
        index.is_empty(),
        "no jarchive/ row is committed for an unverified object"
    );
    assert_eq!(
        journal.archive_claim(),
        ArchiveClaimState::Verified {
            watermark: Lsn::new(0, 0)
        },
        "the journal's own view of the claim is unmoved too"
    );

    // And the clamp is holding: the records the archive still needs are all
    // still there.
    let blocked = journal
        .release_before(*lsns.last().expect("records"))
        .expect("release answers");
    assert_eq!(blocked.blocked, Some(ReleaseBlocked::ArchiveLag));
    assert_eq!(blocked.records_dropped, 0);
    assert_eq!(
        journal
            .scan_from(lsns[0])
            .collect::<Result<Vec<_>, _>>()
            .expect("records remain readable")
            .len(),
        200
    );

    // The easier case, for the same conclusion: the upload itself failing.
    store.corrupt_reads.store(false, Ordering::Release);
    store.fail_upload.store(true, Ordering::Release);
    let stall = tailer.pass().await.expect_err("upload must fail");
    assert_eq!(stall.stage(), "upload");
    assert_eq!(tailer.status().watermark, before);
    assert!(index.is_empty());

    // Clearing both faults lets the same segment through, which proves the two
    // arms above refused rather than corrupted state.
    store.fail_upload.store(false, Ordering::Release);
    let pass = tailer
        .pass()
        .await
        .expect("pass succeeds once the store does");
    assert!(matches!(pass, TailerPass::Published { segment_seq: 0, .. }));
    assert_eq!(tailer.status().watermark, Lsn::new(1, 0));
    assert_eq!(index.len(), 1);
    journal.close().await.expect("close");
}

// ── Mutation direction 2: a crash between upload and row commit ────────────

/// A crash after the object lands and before the `jarchive/` row commits costs
/// a retry, not a record: the same deterministic key is re-uploaded, exactly
/// one row results, and the watermark advances exactly once.
///
/// The "crash" is a metadata store that refuses the commit, followed by a
/// **fresh tailer** built over the same journal and the same index — which is
/// what a restart is, since the tailer holds no durable state of its own and
/// recovers its watermark from the rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_between_the_upload_and_the_row_costs_a_retry_and_not_a_duplicate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(open(dir.path()));
    fill(&journal, 200).await;

    let store = Arc::new(FaultyStore::new(objects.path()));
    let index = Arc::new(MemJarchiveIndex::new());
    let node = test_node(1);

    let mut first = tailer(&journal, &store, &index, node).await;
    index.fail_put.store(true, Ordering::Release);
    let stall = first.pass().await.expect_err("the row commit must fail");
    assert_eq!(stall.stage(), "metadata");
    assert_eq!(
        store.uploads.load(Ordering::Relaxed),
        1,
        "the object landed before the row was attempted"
    );
    let uploaded = store.uploaded_keys();
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0], object_key("", &node, 0));
    assert_eq!(
        first.status().watermark,
        Lsn::new(0, 0),
        "the watermark did not advance past a segment with no row"
    );
    drop(first);

    // Restart. The tailer re-derives its watermark from this node's rows —
    // there are none — and comes back to segment 0.
    index.fail_put.store(false, Ordering::Release);
    let mut restarted = tailer(&journal, &store, &index, node).await;
    assert_eq!(restarted.status().next_segment, 0);
    let pass = restarted.pass().await.expect("the retry succeeds");
    assert!(matches!(pass, TailerPass::Published { segment_seq: 0, .. }));

    assert_eq!(
        store.uploads.load(Ordering::Relaxed),
        2,
        "the retry re-uploaded the object"
    );
    let uploaded = store.uploaded_keys();
    assert_eq!(
        uploaded[0], uploaded[1],
        "both uploads used the same deterministic (node_id, segment_seq) key, \
         so the second overwrote the first rather than adding an object"
    );
    let rows = index.rows(&node).await.expect("rows");
    assert_eq!(rows.len(), 1, "exactly one jarchive/ row, not two");
    assert_eq!(rows[0].segment_seq, 0);
    assert_eq!(
        restarted.status().watermark,
        Lsn::new(1, 0),
        "the watermark advanced exactly once"
    );

    // A second restart over a committed row must not re-archive it.
    let uploads_before = store.uploads.load(Ordering::Relaxed);
    let again = tailer(&journal, &store, &index, node).await;
    assert_eq!(again.status().next_segment, 1);
    assert_eq!(again.status().watermark, Lsn::new(1, 0));
    assert_eq!(
        store.uploads.load(Ordering::Relaxed),
        uploads_before,
        "a restart past a committed row re-uploads nothing"
    );
    journal.close().await.expect("close");
}

// ── Mutation direction 3: an unreachable store is loud, not silent ─────────

/// An unreachable store produces a named, operator-visible reason and a
/// watermark that does not move — the alternative being a tailer that quietly
/// retries while the journal fills.
///
/// The three visible surfaces are asserted by name: the [`ArchiveStall`]
/// variant returned from the pass, the [`ArchiveTailerStatus`] the process
/// exposes without being stopped, and the journal's own
/// [`Journal::archive_gap`] — the number the checkpoint scheduler turns into
/// the §15 alarm.
///
/// [`ArchiveTailerStatus`]: orrery_persistd::archive::ArchiveTailerStatus
/// [`Journal::archive_gap`]: orrery_persistd::journal::Journal::archive_gap
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_store_names_itself_and_holds_the_watermark() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(open(dir.path()));
    let lsns = fill(&journal, 400).await;

    let store = Arc::new(FaultyStore::new(objects.path()));
    let index = Arc::new(MemJarchiveIndex::new());
    let node = test_node(1);
    let mut tailer = tailer(&journal, &store, &index, node).await;

    store.unreachable.store(true, Ordering::Release);
    for expected_failures in 1..=3u32 {
        let stall = tailer.pass().await.expect_err("the store is unreachable");
        assert_eq!(stall.stage(), "upload");
        assert!(
            format!("{stall}").contains("unreachable"),
            "the store's own message is surfaced, not swallowed: {stall}"
        );
        let status = tailer.status();
        assert_eq!(status.consecutive_failures, expected_failures);
        assert_eq!(status.watermark, Lsn::new(0, 0));
        assert_eq!(status.published, 0);
        assert_eq!(
            status.stall.as_ref().map(ArchiveStall::stage),
            Some("upload")
        );
    }
    assert!(index.is_empty());

    // The scheduler's half: a blocked release with a gap that says how much
    // journal is being held, on the byte axis an operator watches.
    let proposed = *lsns.last().expect("records");
    let release = journal.release_before(proposed).expect("release answers");
    assert_eq!(release.blocked, Some(ReleaseBlocked::ArchiveLag));
    let gap = journal
        .archive_gap(proposed)
        .expect("a registered claim reports a gap");
    assert_eq!(
        gap.claim,
        ArchiveClaimState::Verified {
            watermark: Lsn::new(0, 0)
        }
    );
    assert!(
        gap.segments_behind >= 1,
        "the gap counts the sealed segments the archive has not taken"
    );
    assert!(
        gap.bytes_behind >= gap.segments_behind * TEST_SEGMENT,
        "and converts them to the journal bytes the clamp is pinning"
    );

    // Recovery clears it, which is what makes the alarm actionable rather than
    // terminal.
    store.unreachable.store(false, Ordering::Release);
    assert!(matches!(
        tailer.pass().await.expect("recovered"),
        TailerPass::Published { segment_seq: 0, .. }
    ));
    assert_eq!(tailer.status().consecutive_failures, 0);
    assert_eq!(tailer.status().stall, None);
    journal.close().await.expect("close");
}

// ── The epic's second acceptance item: read the records back out ──────────

/// Archive a journal, let retention release past the archived range, and read
/// the released records back **out of the archive**.
///
/// #107's second acceptance item is "released journal records remain
/// recoverable from the archive path", and a tailer that writes objects nobody
/// reads back does not meet it. So this test releases for real — it asserts
/// the records are *gone* from the journal, by the `JournalError::Released`
/// the scan answers with — and then reconstructs them from the objects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_records_are_recoverable_from_the_archive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(open(dir.path()));
    let lsns = fill(&journal, 400).await;
    let sealed_through = journal.committed().segment;
    assert!(
        sealed_through >= 2,
        "the fixture must seal several segments; sealed through {sealed_through}"
    );

    let store = Arc::new(FaultyStore::new(objects.path()));
    let index = Arc::new(MemJarchiveIndex::new());
    let node = test_node(1);
    let mut tailer = tailer(&journal, &store, &index, node).await;

    let mut published = 0;
    loop {
        match tailer.pass().await.expect("pass") {
            TailerPass::Idle => break,
            TailerPass::Published { .. } => published += 1,
            TailerPass::Skipped { .. } => {}
        }
    }
    assert!(published > 0, "the tailer archived at least one segment");
    let watermark = tailer.status().watermark;
    assert_eq!(watermark, Lsn::new(sealed_through, 0));

    // Release for real, up to the archive watermark.
    let release = journal.release_before(watermark).expect("release answers");
    assert_eq!(
        release.blocked, None,
        "the verified archive lifted the clamp"
    );
    assert!(release.records_dropped > 0);

    // The records below the floor are *gone from the journal*: a scan that
    // reaches for them fails rather than answering short. Without this
    // assertion the round trip would prove nothing — nothing would have been
    // at risk.
    let floor = journal.released_floor();
    assert!(floor > lsns[0], "the floor moved past the first record");
    let error = journal
        .scan_from(lsns[0])
        .collect::<Result<Vec<_>, _>>()
        .expect_err("a scan below the floor fails rather than answering short");
    assert!(
        format!("{error}").contains("retention floor"),
        "the journal says the records were released: {error}"
    );

    // Now read them back out of the archive, through the metadata rows, as a
    // reader would: find the objects, fetch them, decode them.
    let mut recovered: Vec<orrery_persistd::journal::StoredRecord> = Vec::new();
    for row in index.rows(&node).await.expect("rows") {
        let bytes = store
            .get(&row.metadata.object_key)
            .expect("fetch object")
            .expect("the object the row names exists");
        assert_eq!(
            *blake3::hash(&bytes).as_bytes(),
            row.metadata.checksum,
            "the row's checksum still matches the stored object"
        );
        let mut decoded = decode_object(&bytes).expect("decode object");
        for stored in &decoded {
            assert!(
                stored.lsn >= row.metadata.lsn_span.start
                    && stored.lsn <= row.metadata.lsn_span.end,
                "every row is inside the lsn_span the metadata advertises"
            );
            assert!(
                row.metadata.cell_ranges.iter().any(|range| {
                    range.grid == stored.record.grid
                        && stored.record.cell >= range.start
                        && stored.record.cell < range.end
                }),
                "every row is inside a cell_range the metadata advertises"
            );
        }
        recovered.append(&mut decoded);
    }

    // Every released record is in the archive, byte-for-byte.
    recovered.sort_by_key(|stored| (stored.lsn.segment, stored.lsn.offset));
    let released: Vec<Lsn> = lsns.iter().copied().filter(|lsn| *lsn < floor).collect();
    assert!(!released.is_empty());
    assert_eq!(
        recovered.iter().filter(|stored| stored.lsn < floor).count(),
        released.len(),
        "every record the release destroyed is recoverable from the archive"
    );
    for (lsn, stored) in released
        .iter()
        .zip(recovered.iter().filter(|stored| stored.lsn < floor))
    {
        assert_eq!(*lsn, stored.lsn);
        assert_eq!(
            stored.record,
            record_at(&lsns, *lsn),
            "the record round-trips"
        );
    }
    journal.close().await.expect("close");
}

/// The record the fixture appended at `lsn`, with the LSN the journal assigned.
fn record_at(lsns: &[Lsn], lsn: Lsn) -> JournalRecord {
    let index = lsns
        .iter()
        .position(|candidate| *candidate == lsn)
        .expect("lsn was appended by the fixture");
    let mut expected = record(index as u64);
    expected.lsn = lsn;
    expected
}

// ── Node identity (#808 item 7) ───────────────────────────────────────────

/// A tailer never reads another node's `jarchive/` rows as its own watermark,
/// and never archives records it did not originate.
///
/// The second half is what keeps two LSN spaces out of one key prefix: a
/// mirrored record keeps the *origin's* `lsn` (`append_inner` restores it),
/// so archiving it under this node's `node_id` would file a foreign position
/// in the column §11.1 sorts and prunes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tailer_archives_only_its_own_node_and_reads_only_its_own_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(open(dir.path()));

    // A journal holding only *mirrored* records: nothing this node originated.
    let mut last = None;
    for entity in 0..200u64 {
        let mut mirrored = record(entity);
        // The source's LSN space, deliberately unlike this journal's.
        mirrored.lsn = Lsn::new(9000 + entity, 17);
        last = Some(
            journal
                .append_replicated(mirrored)
                .expect("append replicated"),
        );
    }
    last.expect("records").committed().await.expect("durable");
    assert!(journal.committed().segment > 0, "segments sealed");

    let store = Arc::new(FaultyStore::new(objects.path()));
    let index = Arc::new(MemJarchiveIndex::new());

    // Another node's rows exist in the same index, at a high segment number.
    let other = test_node(2);
    index
        .put_row(
            &other,
            5000,
            &orrery_persistd::keyspace::JarchiveMetadata {
                object_key: "file:///elsewhere".to_owned(),
                cell_ranges: Vec::new(),
                lsn_span: orrery_persistd::keyspace::JarchiveLsnSpan {
                    start: Lsn::new(5000, 0),
                    end: Lsn::new(5000, 1),
                },
                checksum: [0u8; 32],
            },
        )
        .await
        .expect("put another node's row");

    let node = test_node(1);
    let mut tailer = tailer(&journal, &store, &index, node).await;
    assert_eq!(
        tailer.status().next_segment,
        0,
        "another node's rows are not this node's watermark"
    );

    // Every sealed segment is skipped: nothing here was originated locally.
    let mut skipped = 0;
    loop {
        match tailer.pass().await.expect("pass") {
            TailerPass::Idle => break,
            TailerPass::Skipped { .. } => skipped += 1,
            TailerPass::Published { .. } => panic!("a mirrored record must not be archived here"),
        }
    }
    assert!(skipped > 0);
    assert_eq!(
        store.uploads.load(Ordering::Relaxed),
        0,
        "no object is written for records this node did not originate"
    );
    assert_eq!(
        index.rows(&node).await.expect("rows").len(),
        0,
        "and no row is filed under this node"
    );

    // But the watermark still advances, which is the rule that keeps a node
    // originating nothing from blocking its own release forever.
    assert_eq!(
        tailer.status().watermark,
        Lsn::new(journal.committed().segment, 0)
    );
    assert_eq!(
        journal.archive_claim(),
        ArchiveClaimState::Verified {
            watermark: Lsn::new(journal.committed().segment, 0)
        }
    );
    journal.close().await.expect("close");
}

/// The tailer will not read a segment the writer has not left.
///
/// `committed()` is the durable cursor, so the open segment is not sealed and
/// a pass over it is [`TailerPass::Idle`] — never a partial object that a
/// later append would extend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_segment_is_never_archived() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(open(dir.path()));
    // Few enough records that segment 0 is still the journal's open segment.
    fill(&journal, 4).await;
    assert_eq!(journal.committed().segment, 0, "still inside segment 0");

    let store = Arc::new(FaultyStore::new(objects.path()));
    let index = Arc::new(MemJarchiveIndex::new());
    let mut tailer = tailer(&journal, &store, &index, test_node(1)).await;
    assert_eq!(tailer.pass().await.expect("pass"), TailerPass::Idle);
    assert_eq!(store.uploads.load(Ordering::Relaxed), 0);
    assert_eq!(tailer.status().watermark, Lsn::new(0, 0));

    // Filling past the segment boundary seals it, and only then is it taken.
    fill(&journal, 200).await;
    assert!(journal.committed().segment > 0);
    assert!(matches!(
        tailer.pass().await.expect("pass"),
        TailerPass::Published { segment_seq: 0, .. }
    ));
    journal.close().await.expect("close");
}
