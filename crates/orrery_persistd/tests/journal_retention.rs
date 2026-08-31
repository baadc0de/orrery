//! Journal retention (D20): the checkpoint floor bounds the journal, and the
//! bound is enforced loudly.
//!
//! `journal_open_scaling.rs` measures what an unbounded journal costs to open —
//! linearly, ~3.94 µs per record, for as long as the node has ever run. These are
//! the assertions that the bound exists, that crossing it fails instead of
//! answering short, and that a journal released to empty does not hand out an
//! LSN it has already acknowledged.

use orrery_persistd::journal::{
    AdaptiveCommitMode, GroupCommitConfig, JournalError, ReleaseBlocked,
};
use orrery_persistd::{payload_crc, Journal, JournalConfig};
use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

fn test_node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn mk_record(entity: u64) -> JournalRecord {
    mk_record_sized(entity, 256)
}

fn mk_record_sized(entity: u64, payload_len: usize) -> JournalRecord {
    let payload = vec![7u8; payload_len];
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: CellId::ROOT,
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: Tick::new(1),
        epoch: Epoch::new(0),
        author: test_node(1),
        kind: RecordKind::ComponentDiff,
        crc: payload_crc(&payload),
        payload: bytes::Bytes::from(payload),
    }
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

/// Append `n` records and return the LSN of each.
async fn fill(journal: &Journal, n: usize) -> Vec<Lsn> {
    let mut lsns = Vec::with_capacity(n);
    let mut last = None;
    for i in 0..n {
        let handle = journal.append(mk_record(i as u64)).expect("append");
        lsns.push(handle.lsn());
        last = Some(handle);
    }
    last.expect("at least one record")
        .committed()
        .await
        .expect("durable");
    lsns
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_bounds_the_index_and_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path());

    let journal = Journal::open(&config).expect("open");
    let lsns = fill(&journal, 200).await;
    assert_eq!(journal.released_floor(), Lsn::new(0, 0));

    let floor = lsns[120];
    let release = journal.release_before(floor).expect("release");
    assert_eq!(release.blocked, None);
    assert_eq!(release.floor, floor);
    assert_eq!(release.records_dropped, 120);

    // The live journal answers from the floor up, and refuses below it.
    assert_eq!(journal.scan_from(floor).count(), 80);
    assert_eq!(journal.released_floor(), floor);
    let below = journal
        .scan_from(Lsn::new(0, 0))
        .next()
        .expect("a refusal, not an empty scan");
    assert!(
        matches!(below, Err(JournalError::Released { floor: f, .. }) if f == floor),
        "a scan below the floor must fail Released, got {below:?}"
    );

    journal.close().await.expect("close");
    drop(journal);

    // And so does a journal reopened from what survived on disk. This is the
    // property that makes the floor a durable fact rather than a live one:
    // `truncate_before` reclaims whole segments, so some released records are
    // still physically present here, and the reopened index must not index them.
    let journal = Journal::open(&config).expect("reopen");
    assert_eq!(journal.released_floor(), floor);
    assert_eq!(journal.scan_from(floor).count(), 80);
    assert!(matches!(
        journal.scan_from(Lsn::new(0, 0)).next(),
        Some(Err(JournalError::Released { .. }))
    ));
    journal.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_journal_released_to_empty_does_not_reuse_an_lsn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path());

    let journal = Journal::open(&config).expect("open");
    let lsns = fill(&journal, 50).await;
    let committed = journal.committed();
    let last = *lsns.last().expect("records");

    // Release past every record there is: the whole journal is redundant.
    let release = journal
        .release_before(Lsn::new(last.segment, last.offset + 1))
        .expect("release");
    assert_eq!(release.blocked, None);
    assert_eq!(release.records_dropped, 50);
    assert_eq!(journal.scan_from(journal.released_floor()).count(), 0);
    journal.close().await.expect("close");
    drop(journal);

    // Nothing is left to derive a position from, so the marker has to carry it.
    // Without that, the next append reopens at 0:0 and mints an LSN a previous
    // incarnation already handed to a client as its durability ack.
    let journal = Journal::open(&config).expect("reopen");
    assert_eq!(
        journal.committed(),
        committed,
        "the committed watermark must survive a full release"
    );
    let next = journal.append(mk_record(99)).expect("append");
    assert!(
        next.lsn() > last,
        "post-release append at {} reused an acknowledged LSN (last was {last})",
        next.lsn()
    );
    next.committed().await.expect("durable");
    journal.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_that_cannot_advance_the_floor_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path());
    let journal = Journal::open(&config).expect("open");
    let lsns = fill(&journal, 20).await;

    journal.release_before(lsns[10]).expect("release");
    let again = journal.release_before(lsns[10]).expect("second release");
    assert_eq!(again.blocked, Some(ReleaseBlocked::AlreadyReleased));
    assert_eq!(again.records_dropped, 0);

    let lower = journal.release_before(lsns[5]).expect("lower release");
    assert_eq!(lower.blocked, Some(ReleaseBlocked::AlreadyReleased));
    assert_eq!(
        journal.released_floor(),
        lsns[10],
        "a lower request must never lower the floor"
    );
    journal.close().await.expect("close");
}

/// A release is durable before it is destructive: the marker and its barrier
/// precede `truncate_before`. This reopens a journal that was *not* closed
/// after a release — the crash case — and requires the floor to hold anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_floor_survives_a_journal_that_was_never_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path());

    let journal = Journal::open(&config).expect("open");
    let lsns = fill(&journal, 100).await;
    let floor = lsns[60];
    journal.release_before(floor).expect("release");
    // No `close()`: drop the handle the way a `kill -9` drops the process.
    drop(journal);

    let journal = Journal::open(&config).expect("reopen");
    assert_eq!(journal.released_floor(), floor);
    assert_eq!(journal.scan_from(floor).count(), 40);
    journal.close().await.expect("close");
}

/// Reclamation itself, which needs more than one 128 MiB segment to observe.
///
/// `#[ignore]`d for cost, not for doubt: the fast lane above proves the index
/// bound and the durability of the floor, and this proves the disk actually
/// comes back. wal-db drops whole segments, so a journal smaller than one
/// segment can be entirely released and still occupy every byte it did before.
///
/// ```sh
/// cargo test -p orrery_persistd --release --test journal_retention \
///   -- --ignored --nocapture release_reclaims_whole_segments
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "writes ~300 MiB; see the doc comment"]
async fn release_reclaims_whole_segments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path());
    let journal = Journal::open(&config).expect("open");

    // ~1.5 KiB per record, so ~375 MiB spans three 128 MiB segments.
    let mut lsns = Vec::new();
    let mut last = None;
    for i in 0..250_000u64 {
        let handle = journal.append(mk_record_sized(i, 1_400)).expect("append");
        lsns.push(handle.lsn());
        last = Some(handle);
    }
    last.expect("records").committed().await.expect("durable");
    let floor = lsns[240_000];
    let release = journal.release_before(floor).expect("release");
    println!(
        "bytes_before={} bytes_after={} dropped={}",
        release.bytes_before, release.bytes_after, release.records_dropped
    );
    assert!(
        release.bytes_after < release.bytes_before,
        "releasing 270k of 280k records reclaimed nothing: {} -> {}",
        release.bytes_before,
        release.bytes_after
    );
    assert_eq!(journal.scan_from(floor).count(), 10_000);
    assert!(
        release.bytes_before - release.bytes_after >= 128 * 1024 * 1024,
        "a release spanning two full segments reclaimed less than one"
    );
    journal.close().await.expect("close");
}

/// A follower's mirror is bounded by what its **primary** has released, and by
/// nothing else (D23).
///
/// The follower folds no mirrored record into an actor, so its own checkpoints
/// say nothing about what the mirror still owes; what a promotion needs from it
/// is exactly the tail the primary's durable tier does not already hold. The
/// primary therefore reports its own retention floor on the chain, and the
/// follower releases up to the local position of the first row at or above it.
///
/// A mirror whose primary has reported nothing is pinned whole — the pre-D23
/// behaviour, now the unconfigured case rather than the only case.
#[cfg(feature = "chain-grpc")]
#[tokio::test]
async fn a_follower_mirror_is_released_only_as_far_as_its_primary_has() {
    use orrery_persistd::journal::ChainTransport;
    use orrery_persistd::{spawn_chain_grpc, DurableChainId, GrpcChainTransport};

    let chain = DurableChainId {
        primary_node: test_node(1),
        follower_node: test_node(2),
        shard_set: b"root/0-7".to_vec(),
        epoch: 4,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = std::sync::Arc::new(Journal::open(&config(dir.path())).expect("open"));
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().expect("addr"),
        std::sync::Arc::clone(&journal),
        chain.clone(),
    )
    .await
    .expect("chain server");
    let transport = GrpcChainTransport::connect(server.addr(), chain)
        .await
        .expect("connect");
    // The LSN span a batch claims must match what its records encode to, so
    // these are `chain_grpc.rs`'s records verbatim rather than this file's.
    let mirror_record = |origin: u64| {
        let payload = origin.to_le_bytes();
        JournalRecord {
            lsn: Lsn::new(0, origin),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(origin),
            tick: Tick::new(origin),
            epoch: Epoch::new(0),
            author: test_node(1),
            kind: RecordKind::Spawn,
            payload: bytes::Bytes::copy_from_slice(&payload),
            crc: payload_crc(&payload),
        }
    };
    transport
        .append_batch(vec![
            mirror_record(10),
            mirror_record(82),
            mirror_record(154),
        ])
        .await
        .expect("mirror batch");

    // Nothing reported yet: the whole mirror is pinned, and the reason is
    // named rather than left as a journal that quietly never shrinks.
    let blocked = journal
        .release_before(Lsn::new(9_999, 0))
        .expect("release call answers");
    assert_eq!(blocked.blocked, Some(ReleaseBlocked::MirrorLag));
    assert_eq!(journal.released_floor(), Lsn::new(0, 0));
    assert_eq!(journal.scan_from(Lsn::new(0, 0)).count(), 3);

    // The primary has released through its record at origin 82. The follower's
    // own positions are its own — the mirror keeps the origin LSN inside the
    // record and takes an independent local key — so the floor is the *local*
    // position of the first row at or above that origin, which is the second
    // record's.
    let local: Vec<Lsn> = journal
        .scan_from(Lsn::new(0, 0))
        .map(|stored| stored.expect("mirrored record").lsn)
        .collect();
    ChainTransport::note_primary_floor(&transport, Lsn::new(0, 82));
    transport
        .append_batch(vec![mirror_record(226)])
        .await
        .expect("second mirror batch carries the floor");

    let release = journal
        .release_before(Lsn::new(9_999, 0))
        .expect("release call answers");
    assert_eq!(release.blocked, None);
    assert_eq!(release.floor, local[1]);
    assert_eq!(
        journal.scan_from(release.floor).count(),
        3,
        "the row at the primary's floor is retained, not released"
    );

    // The cursor the release moved out from under still rebuilds — over the
    // retained suffix, seeded by the persisted row — and the chain keeps
    // running across it.
    assert_eq!(transport.follower_watermark().await, Some(Lsn::new(0, 226)));
    transport
        .append_batch(vec![mirror_record(298)])
        .await
        .expect("the chain continues over a released prefix");
    drop(transport);
    server.shutdown().await;
    journal.close().await.expect("close");

    // And it survives the restart the persisted cursor exists for.
    let reopened = std::sync::Arc::new(Journal::open(&config(dir.path())).expect("reopen"));
    assert_eq!(reopened.released_floor(), local[1]);
    let chain = DurableChainId {
        primary_node: test_node(1),
        follower_node: test_node(2),
        shard_set: b"root/0-7".to_vec(),
        epoch: 4,
    };
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().expect("addr"),
        std::sync::Arc::clone(&reopened),
        chain.clone(),
    )
    .await
    .expect("chain server");
    let transport = GrpcChainTransport::connect(server.addr(), chain)
        .await
        .expect("reconnect");
    assert_eq!(
        transport.follower_watermark().await,
        Some(Lsn::new(0, 298)),
        "a restarted follower must not re-stream a released prefix"
    );
    drop(transport);
    server.shutdown().await;
    reopened.close().await.expect("close");
}

/// End to end, over the real replicator: the primary's floor travels the chain
/// and the follower's mirror shrinks behind it (D23).
///
/// The unit-level tests hand the follower a floor directly. This one is the
/// path a deployment actually runs — `spawn_chain` reads `released_floor()` at
/// each push, the transport carries it on the batch frame, the follower takes
/// the maximum and its own driver releases up to the first row at or above it —
/// because every one of those four links can be individually absent without any
/// of the others noticing.
#[cfg(feature = "chain-grpc")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_primary_floor_travels_the_chain_and_bounds_the_mirror() {
    use orrery_persistd::{
        spawn_chain, spawn_chain_grpc, ChainConfig, DurableChainId, GrpcChainTransport,
    };
    use std::sync::Arc;

    let chain = DurableChainId {
        primary_node: test_node(1),
        follower_node: test_node(2),
        shard_set: b"root/0-7".to_vec(),
        epoch: 4,
    };
    let primary_dir = tempfile::tempdir().expect("tempdir");
    let follower_dir = tempfile::tempdir().expect("tempdir");
    let follower = Arc::new(Journal::open(&config(follower_dir.path())).expect("open follower"));
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().expect("addr"),
        Arc::clone(&follower),
        chain.clone(),
    )
    .await
    .expect("chain server");
    let primary = Arc::new(Journal::open(&config(primary_dir.path())).expect("open primary"));
    let replicator = spawn_chain(
        Arc::clone(&primary),
        Arc::new(GrpcChainTransport::new(server.addr(), chain)),
        &ChainConfig {
            follower: 2,
            ..ChainConfig::default()
        },
    );

    // Poll rather than sleep: the replicator is a background task and every
    // deadline below is a failure, not a flake budget.
    async fn until(label: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..600 {
            if ready() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {label}");
    }

    let lsns = fill(&primary, 60).await;
    until("the follower to mirror the primary's journal", || {
        follower.scan_from(Lsn::new(0, 0)).count() == 60
    })
    .await;

    // A complete mirror is *not* the fact the release below depends on, and
    // this is what #286 failed on twice: nothing here checkpoints anything, so
    // the floor is decided entirely by D20's chain clamp — and that clamp is
    // the follower watermark the **primary** has recorded, which
    // `chain::update_progress` writes only after a push RPC has replied. The
    // follower indexes a batch's rows before it acknowledges it (commit, then
    // persist the cursor, then reply), so the scan above goes true one fsync
    // and one round trip early, and an exact floor asserted there reads
    // whichever earlier batch the primary had acknowledged. The runner saw
    // `lsns[16]`; delaying the follower's reply reproduces it exactly.
    //
    // So wait for the primary's own reading, and for it to be *settled* rather
    // than merely published: `update_progress` publishes the gauge and writes
    // the clamp as two separate steps, and a single task runs it, so a later
    // watermark can only be observed once the earlier one has finished both.
    // The extra record is what forces that second push — it is appended after
    // the follower already holds all sixty, so it cannot ride the same batch —
    // which makes the clamp known to be at `lsns[59]`, well past `lsns[30]`.
    let fence = fill(&primary, 1).await[0];
    until("the primary to record the follower's watermark", || {
        replicator.follower_watermark() == Some(fence)
    })
    .await;

    // The follower has everything and the primary knows it, so nothing clamps
    // this release below what it asks for.
    let release = primary.release_before(lsns[30]).expect("primary release");
    assert_eq!(release.blocked, None);
    assert_eq!(release.floor, lsns[30]);

    // Nothing has told the follower yet.
    assert_eq!(follower.released_floor(), Lsn::new(0, 0));

    // The next push carries the floor, and the follower's own driver — here,
    // this call — is then able to bound the mirror.
    fill(&primary, 10).await;
    until("the primary's floor to reach the follower", || {
        follower
            .retention_floor(None)
            .is_some_and(|floor| floor > Lsn::new(0, 0))
    })
    .await;
    let floor = follower.retention_floor(None).expect("mirror floor");
    let release = follower.release_before(floor).expect("follower release");
    assert_eq!(release.blocked, None);
    assert!(
        release.records_dropped > 0,
        "the mirror released nothing behind a floor that advanced"
    );
    assert!(
        follower.released_floor() > Lsn::new(0, 0),
        "the follower's mirror is still unbounded"
    );
    assert!(
        follower.scan_from(follower.released_floor()).count() < 70,
        "a released mirror must hold fewer records than the primary ever sent"
    );

    // And the chain keeps running across both floors.
    let tail = fill(&primary, 5).await;
    until("replication to continue over two released prefixes", || {
        follower
            .scan_from(follower.released_floor())
            .filter_map(Result::ok)
            .any(|stored| stored.record.lsn == tail[4])
    })
    .await;

    replicator.shutdown().await;
    server.shutdown().await;
    primary.close().await.expect("close primary");
    follower.close().await.expect("close follower");
}

/// Chain state is re-anchored above the cut, so a release cannot erase the
/// trace that a directory was opened under a different chain epoch.
///
/// A follower that loads and then mirrors nothing has chain state and no
/// provenance — the case `a_bumped_epoch_is_refused_even_when_the_mirror
/// _received_nothing` in `chain_grpc.rs` is built from, and the one case where
/// a journal that has been a follower is still releasable. If the release drops
/// that row with the segment it lived in, a bumped epoch stops being detected
/// and a superseded primary can resume onto a live follower session.
#[cfg(feature = "chain-grpc")]
#[tokio::test]
async fn a_release_does_not_erase_the_chain_epoch_trace() {
    use orrery_persistd::{spawn_chain_grpc, DurableChainId};

    let chain = DurableChainId {
        primary_node: test_node(1),
        follower_node: test_node(2),
        shard_set: b"root/0-7".to_vec(),
        epoch: 4,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = std::sync::Arc::new(Journal::open(&config(dir.path())).expect("open"));
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().expect("addr"),
        std::sync::Arc::clone(&journal),
        chain,
    )
    .await
    .expect("chain server");
    server.shutdown().await;

    let lsns = fill(&journal, 40).await;
    let release = journal.release_before(lsns[30]).expect("release");
    assert_eq!(
        release.blocked, None,
        "a mirror that received nothing holds no provenance to protect"
    );
    journal.close().await.expect("close");
    drop(journal);

    let journal = std::sync::Arc::new(Journal::open(&config(dir.path())).expect("reopen"));
    let bumped = DurableChainId {
        primary_node: test_node(1),
        follower_node: test_node(2),
        shard_set: b"root/0-7".to_vec(),
        epoch: 5,
    };
    let refused = spawn_chain_grpc("127.0.0.1:0".parse().expect("addr"), journal, bumped).await;
    assert!(
        refused.is_err(),
        "the released journal forgot it had been opened at another epoch"
    );
}

/// A chain follower's claim bounds the floor (D20).
///
/// A follower that falls behind resumes by rescanning the *primary's* journal
/// from its own watermark, so a primary that releases past that watermark turns
/// a lagging follower into an unrecoverable one — the durability argument for
/// the chain, spent on disk space.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chain_follower_bounds_the_release() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Journal::open(&config(dir.path())).expect("open");
    let lsns = fill(&journal, 100).await;

    // Registered as bounding retention, but the follower's watermark is not
    // yet known: every record
    // is potentially one it still has to be sent.
    journal.register_chain(true);
    let blocked = journal.release_before(lsns[80]).expect("release");
    assert_eq!(blocked.blocked, Some(ReleaseBlocked::ChainLag));
    assert_eq!(journal.released_floor(), Lsn::new(0, 0));

    // The follower has mirrored through record 40. The checkpoint floor says
    // 80 is redundant; the chain says only 40 is releasable. The lower wins.
    journal.note_chain_watermark(lsns[40]);
    let release = journal.release_before(lsns[80]).expect("release");
    assert_eq!(release.blocked, None);
    assert_eq!(release.floor, lsns[40]);
    assert_eq!(
        journal.scan_from(lsns[40]).count(),
        60,
        "the record at the follower's watermark is retained, not released"
    );

    // Catching up lets the rest go.
    journal.note_chain_watermark(lsns[90]);
    let release = journal.release_before(lsns[80]).expect("release");
    assert_eq!(release.blocked, None);
    assert_eq!(release.floor, lsns[80]);
    journal.close().await.expect("close");
}

/// A registered archive claim is a destructive-release guard (D20).
///
/// The checkpoint proposal can run ahead, but release must stop by name until
/// the archive has verified the records it would destroy. Once verification
/// advances, the archive watermark is the third term in the same minimum.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lagging_archive_claim_blocks_release_and_preserves_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Journal::open(&config(dir.path())).expect("open");
    let lsns = fill(&journal, 100).await;

    journal.register_archive();
    journal.note_archive_watermark(Lsn::new(0, 0));
    assert_eq!(
        journal.retention_floor(Some(lsns[80])),
        Some(Lsn::new(0, 0)),
        "the lagging archive is the binding third term"
    );

    let blocked = journal.release_before(lsns[80]).expect("release answers");
    assert_eq!(blocked.blocked, Some(ReleaseBlocked::ArchiveLag));
    assert_eq!(blocked.records_dropped, 0);
    assert_eq!(journal.released_floor(), Lsn::new(0, 0));
    assert_eq!(
        journal
            .scan_from(lsns[0])
            .collect::<Result<Vec<_>, _>>()
            .expect("guarded records remain readable")
            .len(),
        100,
        "the refused release preserves every record the archive still needs"
    );

    journal.note_archive_watermark(lsns[40]);
    journal.note_archive_watermark(lsns[20]);
    assert_eq!(
        journal.retention_floor(Some(lsns[80])),
        Some(lsns[40]),
        "a stale archive report cannot move the watermark backward"
    );
    let release = journal.release_before(lsns[80]).expect("release answers");
    assert_eq!(release.blocked, None);
    assert!(release.records_dropped > 0);
    assert_eq!(release.floor, lsns[40]);
    assert_eq!(
        journal.scan_from(lsns[40]).count(),
        60,
        "the record at the archive watermark is retained"
    );
    journal.close().await.expect("close");
}

/// The archive claim is opt-in. Without registration, its watermark abstains
/// and the pre-#806 checkpoint-plus-chain floor and release stay unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_archive_claim_the_existing_floor_and_release_are_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Journal::open(&config(dir.path())).expect("open");
    let lsns = fill(&journal, 100).await;

    // A watermark reported without registration cannot silently enable the
    // claim. The existing locally-originated floor is still the checkpoint.
    journal.note_archive_watermark(Lsn::new(0, 0));
    assert_eq!(journal.retention_floor(Some(lsns[80])), Some(lsns[80]));

    let release = journal.release_before(lsns[80]).expect("release answers");
    assert_eq!(release.blocked, None);
    assert!(release.records_dropped > 0);
    assert_eq!(release.floor, lsns[80]);
    journal.close().await.expect("close");
}

/// A promotion-adopted chain echoes the *source's* LSNs, so its watermark is
/// not a position in this journal at all. Registering as non-bounding is what
/// keeps that number from being mistaken for one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_chain_never_lifts_its_own_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Journal::open(&config(dir.path())).expect("open");
    let lsns = fill(&journal, 60).await;

    journal.register_chain(false);
    journal.note_chain_watermark(lsns[50]);
    let blocked = journal.release_before(lsns[40]).expect("release");
    assert_eq!(blocked.blocked, Some(ReleaseBlocked::ChainLag));
    assert_eq!(journal.released_floor(), Lsn::new(0, 0));
    journal.close().await.expect("close");
}

/// A release must not deadlock against the group committer, and this is the
/// test that says so because the first implementation did.
///
/// The committer's order is WAL first, index second: it appends, syncs, and
/// *then* takes the index write lock to record where the records landed. A
/// release that held the index lock across its own `append`/`sync` inverted
/// that — the committer waited for the index while the release waited for the
/// WAL — and the two wedged. It survived every single-threaded test in this
/// file and hung the workspace suite instead.
///
/// Two details make this fail rather than hang. The deadline is enforced by
/// `recv_timeout` on the *main* test thread, not by a future on the runtime,
/// because a wedged runtime is exactly the state under test and a timeout that
/// needs a worker to poll it is a timeout that never fires. And the workers are
/// OS threads for the same reason.
#[test]
fn releases_interleave_with_concurrent_appends() {
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(Journal::open(&config(dir.path())).expect("open"));

    // Prime the journal so the first release has something to drop.
    let lsns = runtime.block_on(fill(&journal, 200));
    let floor = Arc::new(std::sync::Mutex::new(lsns[100]));

    let (done_tx, done_rx) = mpsc::channel();
    for w in 0..3u64 {
        let journal = Arc::clone(&journal);
        let handle = runtime.handle().clone();
        let done = done_tx.clone();
        std::thread::spawn(move || {
            for i in 0..400u64 {
                let append = journal.append(mk_record(w * 1000 + i)).expect("append");
                if i % 50 == 0 {
                    handle.block_on(append.committed()).expect("durable");
                }
            }
            let _ = done.send(());
        });
    }
    {
        let journal = Arc::clone(&journal);
        let floor = Arc::clone(&floor);
        let done = done_tx.clone();
        std::thread::spawn(move || {
            for _ in 0..40 {
                let at = *floor.lock().expect("floor");
                let release = journal.release_before(at).expect("release");
                // Advance the floor toward the committed watermark, the way the
                // checkpoint scheduler does.
                *floor.lock().expect("floor") = journal.committed().max(release.floor);
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = done.send(());
        });
    }
    drop(done_tx);

    for worker in 0..4 {
        done_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| {
                panic!("appends and releases deadlocked: {worker} of 4 workers finished")
            });
    }

    // And the journal is still coherent: everything at or above the floor is
    // readable, and nothing below it is served.
    let retained = journal.released_floor();
    for record in journal.scan_from(retained) {
        record.expect("scan a retained record");
    }
    runtime.block_on(journal.close()).expect("close");
}
