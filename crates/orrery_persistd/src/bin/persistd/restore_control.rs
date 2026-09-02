//! File-backed operator surface for archive restoration.
//!
//! This lives under the binary rather than the library deliberately: the
//! planner and applier are mechanisms, while watching, claiming, and reporting
//! an operator request is `persistd`'s deployment surface.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orrery_persistd::archive::{
    RestoreApplier, RestoreApplyReport, RestorePlan, RestorePlanner, RestoreRequest,
    RestoreSelection,
};
use orrery_persistd::CellRuntime;
use orrery_protocol::{NodeId, Tick};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The missing join is reported on every outcome so a no-hold plan cannot be
/// mistaken for a schema-backed adjudication finding.
const HOLD_DETECTION_STUB: &str = "stub_no_schema_backed_implementation";

/// §11.1 deliberately leaves this policy to the deployment owner.
const AUTHORIZATION_OWNER_RESERVED: &str = "unresolved_owner_reserved";

const RESTORE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum RestoreOperatorRequest {
    Plan {
        plan_id: String,
        operator: String,
        selection: RestoreSelection,
    },
    Apply {
        plan_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestoreOperatorRefusalName {
    MalformedRequest,
    ReplayedRequest,
    UnknownPlan,
    PlanningFailed,
    ApplyFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RestoreOperatorRefusal {
    name: RestoreOperatorRefusalName,
    detail: String,
}

/// The JSON written to `<request>.result`.
///
/// Plan and apply payloads are flattened so the former is the inspectable
/// `RestorePlan` and the latter is the `RestoreApplyReport`, with only the two
/// unresolved deployment-posture statements added beside them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum RestoreOperatorResult {
    Planned {
        hold_detection: String,
        authorization: String,
        #[serde(flatten)]
        plan: RestorePlan,
    },
    Applied {
        hold_detection: String,
        authorization: String,
        #[serde(flatten)]
        report: RestoreApplyReport,
    },
    Refused {
        hold_detection: String,
        authorization: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        plan_id: Option<String>,
        refusal: RestoreOperatorRefusal,
    },
}

#[derive(Default)]
struct RestoreRegistry {
    plans: HashMap<String, RestorePlan>,
    applied: HashSet<String>,
}

impl RestoreRegistry {
    fn recover_latest(&mut self, path: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(result) = serde_json::from_slice::<RestoreOperatorResult>(&bytes) else {
            return;
        };
        match result {
            RestoreOperatorResult::Planned { plan, .. } => {
                self.plans.insert(plan.plan_id.clone(), plan);
            }
            RestoreOperatorResult::Applied { report, .. } => {
                self.applied.insert(report.plan_id);
            }
            RestoreOperatorResult::Refused { .. } => {}
        }
    }
}

pub(super) struct RestoreControlContext {
    planner: RestorePlanner,
    runtime: Arc<CellRuntime>,
    applying_node: NodeId,
}

impl RestoreControlContext {
    pub(super) fn new(
        planner: RestorePlanner,
        runtime: Arc<CellRuntime>,
        applying_node: NodeId,
    ) -> Self {
        Self {
            planner,
            runtime,
            applying_node,
        }
    }

    fn refused(
        plan_id: Option<String>,
        name: RestoreOperatorRefusalName,
        detail: impl Into<String>,
    ) -> RestoreOperatorResult {
        RestoreOperatorResult::Refused {
            hold_detection: HOLD_DETECTION_STUB.to_owned(),
            authorization: AUTHORIZATION_OWNER_RESERVED.to_owned(),
            plan_id,
            refusal: RestoreOperatorRefusal {
                name,
                detail: detail.into(),
            },
        }
    }

    async fn process(&self, bytes: &[u8], registry: &mut RestoreRegistry) -> RestoreOperatorResult {
        let request = match serde_json::from_slice::<RestoreOperatorRequest>(bytes) {
            Ok(request) => request,
            Err(error) => {
                return Self::refused(
                    best_effort_plan_id(bytes),
                    RestoreOperatorRefusalName::MalformedRequest,
                    error.to_string(),
                );
            }
        };

        match request {
            RestoreOperatorRequest::Plan {
                plan_id,
                operator,
                selection,
            } => {
                if registry.plans.contains_key(&plan_id) || registry.applied.contains(&plan_id) {
                    return Self::refused(
                        Some(plan_id.clone()),
                        RestoreOperatorRefusalName::ReplayedRequest,
                        format!("plan request {plan_id:?} has already been accepted"),
                    );
                }
                let request = RestoreRequest {
                    plan_id: plan_id.clone(),
                    operator,
                    selection,
                };
                match self.planner.plan(request).await {
                    Ok(plan) => {
                        registry.plans.insert(plan.plan_id.clone(), plan.clone());
                        RestoreOperatorResult::Planned {
                            hold_detection: HOLD_DETECTION_STUB.to_owned(),
                            authorization: AUTHORIZATION_OWNER_RESERVED.to_owned(),
                            plan,
                        }
                    }
                    Err(error) => Self::refused(
                        Some(plan_id),
                        RestoreOperatorRefusalName::PlanningFailed,
                        error.to_string(),
                    ),
                }
            }
            RestoreOperatorRequest::Apply { plan_id } => {
                if registry.applied.contains(&plan_id) {
                    return Self::refused(
                        Some(plan_id.clone()),
                        RestoreOperatorRefusalName::ReplayedRequest,
                        format!("apply request {plan_id:?} has already completed"),
                    );
                }
                let Some(plan) = registry.plans.get(&plan_id).cloned() else {
                    return Self::refused(
                        Some(plan_id.clone()),
                        RestoreOperatorRefusalName::UnknownPlan,
                        format!("no prior inspected plan named {plan_id:?}"),
                    );
                };
                match RestoreApplier::new(&self.runtime, self.applying_node)
                    .apply(&plan, current_tick())
                    .await
                {
                    Ok(report) => {
                        registry.applied.insert(plan_id);
                        RestoreOperatorResult::Applied {
                            hold_detection: HOLD_DETECTION_STUB.to_owned(),
                            authorization: AUTHORIZATION_OWNER_RESERVED.to_owned(),
                            report,
                        }
                    }
                    Err(error) => Self::refused(
                        Some(plan_id),
                        RestoreOperatorRefusalName::ApplyFailed,
                        error.to_string(),
                    ),
                }
            }
        }
    }
}

fn best_effort_plan_id(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("plan_id")?
        .as_str()
        .map(str::to_owned)
}

/// There is no global simulation clock in persistd. The restore record's tick
/// is therefore a server-generated nominal 60 Hz wall-clock tick; selection
/// and ordering use only server-assigned LSNs, never this field.
fn current_tick() -> Tick {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let nominal = millis.saturating_mul(60) / 1_000;
    Tick::new(u64::try_from(nominal).unwrap_or(u64::MAX))
}

fn result_path(path: &Path) -> PathBuf {
    path.with_extension("result")
}

fn taken_path(path: &Path) -> PathBuf {
    path.with_extension("taken")
}

fn write_result(path: &Path, result: &RestoreOperatorResult) {
    match serde_json::to_vec(result) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path, json) {
                tracing::warn!(%error, "restore: could not write the result file");
            }
        }
        Err(error) => tracing::warn!(%error, "restore: could not encode the result"),
    }
}

/// Watch and exclusively claim restore requests. The rename happens before
/// parsing or applying, so one file appearance can never execute twice.
pub(super) fn spawn_restore_control(
    context: RestoreControlContext,
    request_path: PathBuf,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (shutdown, mut stop) = oneshot::channel();
    let task = tokio::spawn(async move {
        let result_path = result_path(&request_path);
        let mut registry = RestoreRegistry::default();
        registry.recover_latest(&result_path);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                () = tokio::time::sleep(RESTORE_POLL_INTERVAL) => {}
            }
            let Ok(bytes) = std::fs::read(&request_path) else {
                continue;
            };
            if let Err(error) = std::fs::rename(&request_path, taken_path(&request_path)) {
                tracing::warn!(%error, "restore: could not claim the request file");
                continue;
            }
            let result = context.process(&bytes, &mut registry).await;
            write_result(&result_path, &result);
        }
    });
    (shutdown, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use orrery_persistd::archive::{
        encode_object, sort_for_archive, ArchiveStore, FsArchiveStore, JarchiveIndex,
        MemJarchiveIndex, RestoreApplyDisposition, RestoreDisposition,
    };
    use orrery_persistd::journal::{JournalConfig, StoredRecord};
    use orrery_persistd::keyspace::{JarchiveCellRange, JarchiveLsnSpan, JarchiveMetadata};
    use orrery_persistd::{
        payload_crc, CheckpointStore, FenceRow, FenceStatus, FenceStore, MemCheckpointStore,
        MemFenceStore, RuntimeConfig,
    };
    use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind};

    fn node(seed_byte: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = seed_byte;
        iroh::SecretKey::from_bytes(&seed).public()
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
        control_dir: tempfile::TempDir,
        runtime: Arc<CellRuntime>,
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
            let control_dir = tempfile::tempdir().expect("control tempdir");
            let checkpoints = Arc::new(MemCheckpointStore::new());
            let fence = Arc::new(MemFenceStore::new());
            fence
                .fence(
                    GridId::ROOT,
                    CellId::ROOT,
                    None,
                    &FenceRow {
                        owner: 7,
                        epoch: Epoch::new(3),
                        status: FenceStatus::Active,
                    },
                )
                .await
                .expect("fence write");
            let checkpoint_dyn: Arc<dyn CheckpointStore> = checkpoints.clone();
            let archive_store =
                Arc::new(FsArchiveStore::open(archive_dir.path()).expect("archive store opens"));
            let runtime = CellRuntime::open(
                &RuntimeConfig {
                    shards: vec![CellId::ROOT],
                    grid: GridId::ROOT,
                    journal: JournalConfig {
                        dir: journal_dir.path().to_path_buf(),
                        ..JournalConfig::default()
                    },
                    node_id: 7,
                    epoch: Epoch::new(3),
                    fence: fence.clone(),
                },
                &checkpoint_dyn,
            )
            .await
            .expect("runtime opens");
            Self {
                _journal_dir: journal_dir,
                _archive_dir: archive_dir,
                control_dir,
                runtime: Arc::new(runtime),
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

        async fn publish_archive_without_genesis_proof(&self) {
            let mut records = self.journal_records();
            sort_for_archive(&mut records);
            let bytes = encode_object(&records).expect("archive object encodes");
            let key = "jarchive/source/0000000000000001.parquet";
            self.archive_store.put(key, &bytes).expect("archive put");
            self.archive_index
                .put_row(
                    &self.source_node,
                    1,
                    &JarchiveMetadata {
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
                    },
                )
                .await
                .expect("metadata put");
        }

        fn request_path(&self) -> PathBuf {
            self.control_dir.path().join("restore.json")
        }

        fn context(&self) -> RestoreControlContext {
            let store: Arc<dyn ArchiveStore> = self.archive_store.clone();
            let index: Arc<dyn JarchiveIndex> = self.archive_index.clone();
            RestoreControlContext::new(
                RestorePlanner::new(store, index),
                Arc::clone(&self.runtime),
                node(9),
            )
        }

        fn plan_request(&self, plan_id: &str, start_lsn: Lsn, end_lsn: Lsn) -> Vec<u8> {
            serde_json::to_vec(&RestoreOperatorRequest::Plan {
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
            })
            .expect("request encodes")
        }

        async fn close(self) {
            let runtime = match Arc::try_unwrap(self.runtime) {
                Ok(runtime) => runtime,
                Err(_) => panic!("control released runtime"),
            };
            runtime.close().await.expect("runtime closes");
        }
    }

    async fn submit_matching(
        request_path: &Path,
        bytes: &[u8],
        matches: impl Fn(&RestoreOperatorResult) -> bool,
    ) -> RestoreOperatorResult {
        std::fs::write(request_path, bytes).expect("request write");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !request_path.exists() {
                    if let Ok(bytes) = std::fs::read(result_path(request_path)) {
                        if let Ok(result) = serde_json::from_slice(&bytes) {
                            if matches(&result) {
                                return result;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("restore result appears")
    }

    async fn prepare_two_candidates(fixture: &Fixture) -> (Lsn, Lsn) {
        fixture
            .runtime
            .apply(record(11, 10, node(1), RecordKind::Spawn, b"hp=100"))
            .await
            .expect("pre-grief append");
        let start = fixture
            .runtime
            .apply(record(11, 11, node(2), RecordKind::ComponentDiff, b"hp=0"))
            .await
            .expect("grief append");
        let end = fixture
            .runtime
            .apply(record(
                12,
                11,
                node(2),
                RecordKind::ComponentDiff,
                b"unknown=grief",
            ))
            .await
            .expect("history-hole append");
        fixture.publish_archive_without_genesis_proof().await;
        (start, end)
    }

    #[tokio::test]
    async fn binary_plan_result_names_every_candidate_and_appends_nothing() {
        let fixture = Fixture::open().await;
        let (start, end) = prepare_two_candidates(&fixture).await;
        let before = fixture.journal_records().len();
        let request_path = fixture.request_path();
        let (shutdown, task) = spawn_restore_control(fixture.context(), request_path.clone());

        let result = submit_matching(
            &request_path,
            &fixture.plan_request("operator-plan", start, end),
            |result| matches!(result, RestoreOperatorResult::Planned { .. }),
        )
        .await;
        let RestoreOperatorResult::Planned {
            hold_detection,
            authorization,
            plan,
        } = result
        else {
            unreachable!("matching result is planned")
        };
        assert_eq!(hold_detection, HOLD_DETECTION_STUB);
        assert_eq!(authorization, AUTHORIZATION_OWNER_RESERVED);
        assert_eq!(plan.entities.len(), 2, "every candidate is named");
        assert!(matches!(
            plan.entities[0].disposition,
            RestoreDisposition::Restorable { .. }
        ));
        assert!(matches!(
            plan.entities[1].disposition,
            RestoreDisposition::Refused(_)
        ));
        assert_eq!(
            fixture.journal_records().len(),
            before,
            "planning appends nothing"
        );

        let _ = shutdown.send(());
        task.await.expect("restore control joins");
        fixture.close().await;
    }

    #[tokio::test]
    async fn binary_apply_appends_exactly_the_planned_restores_without_moving_fences() {
        let fixture = Fixture::open().await;
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
        let (start, end) = prepare_two_candidates(&fixture).await;
        let prior_tail = fixture
            .journal_records()
            .iter()
            .map(|record| record.lsn)
            .max()
            .expect("prior tail");
        let request_path = fixture.request_path();
        let (shutdown, task) = spawn_restore_control(fixture.context(), request_path.clone());
        let planned = submit_matching(
            &request_path,
            &fixture.plan_request("operator-apply", start, end),
            |result| matches!(result, RestoreOperatorResult::Planned { .. }),
        )
        .await;
        let RestoreOperatorResult::Planned { plan, .. } = planned else {
            unreachable!("matching result is planned")
        };
        let expected = plan
            .entities
            .iter()
            .filter(|entity| matches!(entity.disposition, RestoreDisposition::Restorable { .. }))
            .count();

        let apply = serde_json::to_vec(&RestoreOperatorRequest::Apply {
            plan_id: "operator-apply".to_owned(),
        })
        .expect("apply encodes");
        let applied = submit_matching(&request_path, &apply, |result| {
            matches!(result, RestoreOperatorResult::Applied { .. })
        })
        .await;
        let RestoreOperatorResult::Applied { report, .. } = applied else {
            unreachable!("matching result is applied")
        };
        assert_eq!(report.entities.len(), plan.entities.len());
        assert_eq!(
            report
                .entities
                .iter()
                .filter(|entity| matches!(
                    entity.disposition,
                    RestoreApplyDisposition::Applied { .. }
                ))
                .count(),
            expected
        );
        let restores = fixture
            .journal_records()
            .into_iter()
            .filter(|record| record.record.kind == RecordKind::Restore)
            .collect::<Vec<_>>();
        assert_eq!(
            restores.len(),
            expected,
            "one append per restorable plan entry"
        );
        assert!(restores.iter().all(|record| record.lsn > prior_tail));
        assert_eq!(
            fixture
                .checkpoints
                .load(CellId::ROOT, GridId::ROOT)
                .await
                .expect("checkpoint read")
                .expect("checkpoint exists")
                .watermark,
            checkpoint_before
        );
        assert_eq!(
            fixture
                .fence
                .read(GridId::ROOT, CellId::ROOT)
                .await
                .expect("fence read"),
            fence_before
        );

        let _ = shutdown.send(());
        task.await.expect("restore control joins");
        fixture.close().await;
    }

    #[tokio::test]
    async fn binary_malformed_and_replayed_requests_are_refused_without_duplicate_records() {
        let fixture = Fixture::open().await;
        let (start, end) = prepare_two_candidates(&fixture).await;
        let request_path = fixture.request_path();
        let (shutdown, task) = spawn_restore_control(fixture.context(), request_path.clone());
        submit_matching(
            &request_path,
            &fixture.plan_request("operator-replay", start, end),
            |result| matches!(result, RestoreOperatorResult::Planned { .. }),
        )
        .await;
        let apply = serde_json::to_vec(&RestoreOperatorRequest::Apply {
            plan_id: "operator-replay".to_owned(),
        })
        .expect("apply encodes");
        submit_matching(&request_path, &apply, |result| {
            matches!(result, RestoreOperatorResult::Applied { .. })
        })
        .await;
        let restore_count = fixture
            .journal_records()
            .iter()
            .filter(|record| record.record.kind == RecordKind::Restore)
            .count();

        let replay = submit_matching(&request_path, &apply, |result| {
            matches!(
                result,
                RestoreOperatorResult::Refused {
                    refusal: RestoreOperatorRefusal {
                        name: RestoreOperatorRefusalName::ReplayedRequest,
                        ..
                    },
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            replay,
            RestoreOperatorResult::Refused {
                plan_id: Some(ref plan_id),
                ..
            } if plan_id == "operator-replay"
        ));
        assert_eq!(
            fixture
                .journal_records()
                .iter()
                .filter(|record| record.record.kind == RecordKind::Restore)
                .count(),
            restore_count,
            "a replay never appends twice"
        );

        let malformed = br#"{"action":"apply","plan_id":"bad-shape","extra":true}"#;
        let refused = submit_matching(&request_path, malformed, |result| {
            matches!(
                result,
                RestoreOperatorResult::Refused {
                    refusal: RestoreOperatorRefusal {
                        name: RestoreOperatorRefusalName::MalformedRequest,
                        ..
                    },
                    ..
                }
            )
        })
        .await;
        assert!(matches!(
            refused,
            RestoreOperatorResult::Refused {
                plan_id: Some(ref plan_id),
                ..
            } if plan_id == "bad-shape"
        ));
        assert_eq!(
            fixture
                .journal_records()
                .iter()
                .filter(|record| record.record.kind == RecordKind::Restore)
                .count(),
            restore_count,
            "a malformed request appends nothing"
        );

        let _ = shutdown.send(());
        task.await.expect("restore control joins");
        fixture.close().await;
    }
}
