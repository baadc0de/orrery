//! Acceptance evidence for #847: archive-selected forward restoration.

use std::sync::Arc;

use bytes::Bytes;
use orrery_persistd::archive::{
    encode_object, sort_for_archive, ArchiveStore, FsArchiveStore, JarchiveIndex, MemJarchiveIndex,
    RestoreApplier, RestoreApplyDisposition, RestoreDisposition, RestorePlan, RestorePlanner,
    RestoreRequest, RestoreSelection,
};
use orrery_persistd::journal::{JournalConfig, StoredRecord};
use orrery_persistd::keyspace::{JarchiveCellRange, JarchiveLsnSpan, JarchiveMetadata};
use orrery_persistd::{
    payload_crc, CellRuntime, CheckpointStore, FenceRow, FenceStatus, FenceStore,
    MemCheckpointStore, MemFenceStore, RuntimeConfig,
};
use orrery_protocol::{
    CellId, Epoch, GridId, JournalRecord, Lsn, NodeId, PersistId, RecordKind, RestoreRecord,
    RestoreTarget, Tick,
};

fn node(seed_byte: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = seed_byte;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn cell_after(cell: CellId) -> CellId {
    CellId::from_bits(cell.to_bits().saturating_add(1)).expect("successor cell bits")
}

fn record(
    entity: u64,
    tick: u64,
    author: NodeId,
    kind: RecordKind,
    payload: &'static [u8],
) -> JournalRecord {
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell: CellId::ROOT,
        grid: GridId::ROOT,
        entity: PersistId::new(entity),
        tick: Tick::new(tick),
        epoch: Epoch::new(0),
        author,
        kind,
        payload: Bytes::from_static(payload),
        crc: payload_crc(payload),
    }
}

struct Fixture {
    _journal_dir: tempfile::TempDir,
    _archive_dir: tempfile::TempDir,
    runtime: CellRuntime,
    checkpoints: Arc<MemCheckpointStore>,
    fence: Arc<MemFenceStore>,
    archive_store: Arc<FsArchiveStore>,
    archive_index: Arc<MemJarchiveIndex>,
    source_node: NodeId,
}

impl Fixture {
    async fn open() -> Self {
        let journal_dir = tempfile::tempdir().expect("journal tempdir");
        let archive_dir = tempfile::tempdir().expect("archive tempdir");
        let checkpoints = Arc::new(MemCheckpointStore::new());
        let fence = Arc::new(MemFenceStore::new());
        let row = FenceRow {
            owner: 7,
            epoch: Epoch::new(3),
            status: FenceStatus::Active,
        };
        fence
            .fence(GridId::ROOT, CellId::ROOT, None, &row)
            .await
            .expect("fence write");
        let config = RuntimeConfig {
            shards: vec![CellId::ROOT],
            grid: GridId::ROOT,
            journal: JournalConfig {
                dir: journal_dir.path().to_path_buf(),
                ..JournalConfig::default()
            },
            node_id: 7,
            epoch: Epoch::new(3),
            fence: fence.clone(),
        };
        let checkpoint_dyn: Arc<dyn CheckpointStore> = checkpoints.clone();
        let runtime = CellRuntime::open(&config, &checkpoint_dyn)
            .await
            .expect("runtime opens");
        let archive_store =
            Arc::new(FsArchiveStore::open(archive_dir.path()).expect("archive store opens"));
        Self {
            _journal_dir: journal_dir,
            _archive_dir: archive_dir,
            runtime,
            checkpoints,
            fence,
            archive_store,
            archive_index: Arc::new(MemJarchiveIndex::new()),
            source_node: node(90),
        }
    }

    fn journal_records(&self) -> Vec<StoredRecord> {
        self.runtime
            .journal()
            .scan_from(self.runtime.journal().released_floor())
            .collect::<Result<Vec<_>, _>>()
            .expect("journal scans")
    }

    async fn publish_archive(&self) {
        let mut records = self.journal_records();
        sort_for_archive(&mut records);
        let bytes = encode_object(&records).expect("archive object encodes");
        let key = "jarchive/source/0000000000000000.parquet";
        self.archive_store.put(key, &bytes).expect("archive put");
        let metadata = JarchiveMetadata {
            object_key: key.to_owned(),
            cell_ranges: vec![JarchiveCellRange {
                grid: GridId::ROOT,
                start: CellId::ROOT,
                end: cell_after(CellId::ROOT),
            }],
            lsn_span: JarchiveLsnSpan {
                start: records
                    .iter()
                    .map(|record| record.lsn)
                    .min()
                    .expect("record"),
                end: records
                    .iter()
                    .map(|record| record.lsn)
                    .max()
                    .expect("record"),
            },
            checksum: *blake3::hash(&bytes).as_bytes(),
        };
        self.archive_index
            .put_row(&self.source_node, 0, &metadata)
            .await
            .expect("metadata put");
    }

    fn planner(&self) -> RestorePlanner {
        let store: Arc<dyn ArchiveStore> = self.archive_store.clone();
        let index: Arc<dyn JarchiveIndex> = self.archive_index.clone();
        RestorePlanner::new(store, index)
    }

    fn request(&self, plan_id: &str, start_lsn: Lsn, end_lsn: Lsn) -> RestoreRequest {
        RestoreRequest {
            plan_id: plan_id.to_owned(),
            operator: "ops@example.invalid".to_owned(),
            selection: RestoreSelection {
                source_node: self.source_node,
                grid: GridId::ROOT,
                start_cell: CellId::ROOT,
                end_cell: cell_after(CellId::ROOT),
                start_lsn,
                end_lsn,
                author: Some(node(2)),
            },
        }
    }
}

fn only_applied_lsn(report: &orrery_persistd::archive::RestoreApplyReport) -> Lsn {
    assert_eq!(report.entities.len(), 1);
    match report.entities[0].disposition {
        RestoreApplyDisposition::Applied { lsn } => lsn,
        ref other => panic!("expected one applied entity, got {other:?}"),
    }
}

#[tokio::test]
async fn griefed_cell_restores_to_the_pre_grief_image() {
    let fixture = Fixture::open().await;
    fixture
        .runtime
        .apply(record(11, 10, node(1), RecordKind::Spawn, b"hp=100"))
        .await
        .expect("pre-grief append");
    let grief_lsn = fixture
        .runtime
        .apply(record(11, 11, node(2), RecordKind::ComponentDiff, b"hp=0"))
        .await
        .expect("grief append");
    fixture.publish_archive().await;

    let plan = fixture
        .planner()
        .plan(fixture.request("restore-town-1", grief_lsn, grief_lsn))
        .await
        .expect("plan");
    assert!(matches!(
        &plan.entities[0].disposition,
        RestoreDisposition::Restorable {
            target: RestoreTarget::Present { components, .. },
            ..
        } if components.as_ref() == b"hp=100"
    ));
    RestoreApplier::new(&fixture.runtime, node(9))
        .apply(&plan, Tick::new(99))
        .await
        .expect("apply");

    let latest = fixture
        .runtime
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .expect("live read");
    assert_eq!(
        latest.entities[&PersistId::new(11)].components.as_ref(),
        b"hp=100",
        "the latest live state is the assembled pre-grief image"
    );
    let reopen_config = RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: fixture._journal_dir.path().to_path_buf(),
            ..JournalConfig::default()
        },
        node_id: 7,
        epoch: Epoch::new(3),
        fence: fixture.fence.clone(),
    };
    fixture.runtime.close().await.expect("close");
    let checkpoint_dyn: Arc<dyn CheckpointStore> = fixture.checkpoints.clone();
    let reopened = CellRuntime::open(&reopen_config, &checkpoint_dyn)
        .await
        .expect("runtime reopens over restore record");
    let recovered = reopened
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .expect("recovered read");
    assert_eq!(
        recovered.entities[&PersistId::new(11)].components.as_ref(),
        b"hp=100",
        "journal replay treats Restore as the latest whole image"
    );
    reopened.close().await.expect("reopened close");
}

#[tokio::test]
async fn restore_appends_above_the_prior_tail_without_touching_checkpoint_or_epoch_fence() {
    let fixture = Fixture::open().await;
    fixture
        .runtime
        .apply(record(12, 20, node(1), RecordKind::Spawn, b"door=closed"))
        .await
        .expect("pre-grief append");
    fixture
        .runtime
        .checkpoint(fixture.checkpoints.as_ref())
        .await
        .expect("ordinary checkpoint");
    let checkpoint_before = fixture
        .checkpoints
        .load(CellId::ROOT, GridId::ROOT)
        .await
        .expect("checkpoint read")
        .expect("checkpoint exists")
        .watermark;
    let fence_before = fixture
        .fence
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .expect("fence read");
    let grief_lsn = fixture
        .runtime
        .apply(record(
            12,
            21,
            node(2),
            RecordKind::ComponentDiff,
            b"door=gone",
        ))
        .await
        .expect("grief append");
    fixture.publish_archive().await;
    let prior_tail = fixture
        .journal_records()
        .iter()
        .map(|record| record.lsn)
        .max()
        .expect("prior tail");

    let plan = fixture
        .planner()
        .plan(fixture.request("restore-town-2", grief_lsn, grief_lsn))
        .await
        .expect("plan");
    let applied = RestoreApplier::new(&fixture.runtime, node(9))
        .apply(&plan, Tick::new(500))
        .await
        .expect("apply");
    let restore_lsn = only_applied_lsn(&applied);
    assert!(restore_lsn > prior_tail, "restore is a forward append");

    let checkpoint_after = fixture
        .checkpoints
        .load(CellId::ROOT, GridId::ROOT)
        .await
        .expect("checkpoint read")
        .expect("checkpoint exists")
        .watermark;
    let fence_after = fixture
        .fence
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .expect("fence read");
    assert_eq!(checkpoint_after, checkpoint_before);
    assert_eq!(fence_after, fence_before);

    let stored = fixture
        .journal_records()
        .into_iter()
        .find(|record| record.lsn == restore_lsn)
        .expect("restore record");
    assert_eq!(stored.record.kind, RecordKind::Restore);
    assert_eq!(stored.record.author, node(9));
    assert_eq!(stored.record.tick, Tick::new(500));
    assert_eq!(stored.record.epoch, Epoch::new(3));
    let payload: RestoreRecord = postcard::from_bytes(&stored.record.payload).expect("payload");
    assert_eq!(payload.plan_id, "restore-town-2");
    assert_eq!(payload.operator, "ops@example.invalid");
    fixture.runtime.close().await.expect("close");
}

#[tokio::test]
async fn adversarial_ticks_cannot_move_a_grief_record_out_of_the_lsn_selection() {
    let fixture = Fixture::open().await;
    fixture
        .runtime
        .apply(record(
            13,
            u64::MAX,
            node(1),
            RecordKind::Spawn,
            b"wall=intact",
        ))
        .await
        .expect("pre-grief append");
    let grief_lsn = fixture
        .runtime
        .apply(record(
            13,
            0,
            node(2),
            RecordKind::ComponentDiff,
            b"wall=destroyed",
        ))
        .await
        .expect("grief append");
    assert_ne!(
        grief_lsn.offset, 0,
        "the adversarial tick differs from its LSN"
    );
    fixture.publish_archive().await;

    let plan = fixture
        .planner()
        .plan(fixture.request("restore-town-axis", grief_lsn, grief_lsn))
        .await
        .expect("plan");
    assert_eq!(
        plan.entities.len(),
        1,
        "the LSN-selected grief touch survives"
    );
    assert!(matches!(
        &plan.entities[0].disposition,
        RestoreDisposition::Restorable {
            target: RestoreTarget::Present { components, .. },
            ..
        } if components.as_ref() == b"wall=intact"
    ));
    RestoreApplier::new(&fixture.runtime, node(9))
        .apply(&plan, Tick::new(700))
        .await
        .expect("apply");
    let latest = fixture
        .runtime
        .read(GridId::ROOT, CellId::ROOT)
        .await
        .expect("live read");
    assert_eq!(
        latest.entities[&PersistId::new(13)].components.as_ref(),
        b"wall=intact"
    );
    fixture.runtime.close().await.expect("close");
}

#[tokio::test]
async fn a_partial_apply_can_be_rerun_without_duplicate_entity_records() {
    let fixture = Fixture::open().await;
    fixture
        .runtime
        .apply(record(21, 1, node(1), RecordKind::Spawn, b"a=before"))
        .await
        .expect("pre a");
    fixture
        .runtime
        .apply(record(22, 1, node(1), RecordKind::Spawn, b"b=before"))
        .await
        .expect("pre b");
    let grief_start = fixture
        .runtime
        .apply(record(
            21,
            2,
            node(2),
            RecordKind::ComponentDiff,
            b"a=grief",
        ))
        .await
        .expect("grief a");
    let grief_end = fixture
        .runtime
        .apply(record(
            22,
            2,
            node(2),
            RecordKind::ComponentDiff,
            b"b=grief",
        ))
        .await
        .expect("grief b");
    fixture.publish_archive().await;
    let plan = fixture
        .planner()
        .plan(fixture.request("restore-partial", grief_start, grief_end))
        .await
        .expect("plan");
    assert_eq!(plan.entities.len(), 2);

    let partial = RestorePlan {
        entities: plan.entities[..1].to_vec(),
        ..plan.clone()
    };
    let first = RestoreApplier::new(&fixture.runtime, node(9))
        .apply(&partial, Tick::new(800))
        .await
        .expect("partial apply");
    let first_lsn = only_applied_lsn(&first);
    let rerun = RestoreApplier::new(&fixture.runtime, node(9))
        .apply(&plan, Tick::new(801))
        .await
        .expect("rerun");
    assert!(matches!(
        rerun.entities[0].disposition,
        RestoreApplyDisposition::AlreadyApplied { lsn } if lsn == first_lsn
    ));
    assert!(matches!(
        rerun.entities[1].disposition,
        RestoreApplyDisposition::Applied { .. }
    ));

    let restores = fixture
        .journal_records()
        .into_iter()
        .filter(|record| record.record.kind == RecordKind::Restore)
        .collect::<Vec<_>>();
    assert_eq!(restores.len(), 2, "one restore record per planned entity");
    for entity in [PersistId::new(21), PersistId::new(22)] {
        assert_eq!(
            restores
                .iter()
                .filter(|record| record.record.entity == entity)
                .count(),
            1,
            "rerun does not duplicate entity {entity:?}"
        );
    }
    fixture.runtime.close().await.expect("close");
}
