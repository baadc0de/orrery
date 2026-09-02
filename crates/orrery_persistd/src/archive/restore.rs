//! Archive-backed operator restoration: plan from immutable history, then
//! apply the computed image forward at the live journal tail (§11.1).
//!
//! The planner has no write dependency. It first prunes `jarchive/` rows by
//! their cell and LSN spans, discovers candidate entities strictly on the
//! server-assigned LSN axis, and only then reads the earlier objects needed to
//! assemble each candidate's last complete image before the cut. The applier
//! writes one ordinary `RecordKind::Restore` per restorable entity through the
//! owning actor mailbox. Neither half has access to a ledger writer,
//! checkpoint store, or fence mutator.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use orrery_protocol::{
    atrest::SCHEMA_V0, CellId, EntityRekey, Epoch, GridId, JournalRecord, Lsn, NodeId, PersistId,
    RecordKind, RestoreRecord, RestoreTarget, Tick, RESTORE_RECORD_VERSION,
};

use crate::actor;
use crate::fence::FenceStatus;
use crate::journal::StoredRecord;
use crate::{payload_crc, CellRuntime};

use super::{decode_object, ArchiveStore, JarchiveIndex, JarchiveRow};

/// Inclusive LSN window and half-open cell window selected by an operator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreSelection {
    /// Journal origin whose node-local LSN space the range belongs to.
    pub source_node: NodeId,
    /// Grid whose cell-id space the range uses.
    pub grid: GridId,
    /// First selected cell (inclusive).
    pub start_cell: CellId,
    /// Cell after the selected range (exclusive, except saturated `u64::MAX`).
    pub end_cell: CellId,
    /// First grief-window LSN (inclusive).
    pub start_lsn: Lsn,
    /// Last grief-window LSN (inclusive).
    pub end_lsn: Lsn,
    /// Optional server-assigned transport author whose touches define candidates.
    #[serde(default)]
    pub author: Option<NodeId>,
}

/// One operator request. Reusing `plan_id` is what makes a partial apply safe
/// to resume without appending a second record for entities already applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreRequest {
    /// Stable request/idempotency identifier, carried into every restore record.
    pub plan_id: String,
    /// Identity authorized by the operator surface, carried in every payload.
    pub operator: String,
    /// Archive selection to plan.
    pub selection: RestoreSelection,
}

/// Why one candidate cannot be restored automatically.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RestoreRefusal {
    /// No complete state-bearing record exists before the cut, and the
    /// selected range contains no spawn that positively proves prior absence.
    PreimageUnavailable,
}

/// The planner's disposition for one candidate entity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RestoreDisposition {
    /// A complete target image is ready to append.
    Restorable {
        /// Cell in which the entity existed (or was absent) at the cut.
        cell: CellId,
        /// Whole target image or absence.
        target: RestoreTarget,
    },
    /// The archive cannot prove a target image; no guess is made.
    Refused(RestoreRefusal),
    /// An adjudication product supersedes this operator restore.
    Held {
        /// Stable description/key of the product that holds the entity.
        product: String,
    },
}

/// One named entity in a restore plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestorePlanEntity {
    /// Candidate selected from a grief-window record.
    pub entity: PersistId,
    /// Per-entity plan outcome.
    pub disposition: RestoreDisposition,
}

/// A read-only plan. Constructing it mutates no durable or live state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestorePlan {
    /// Stable request/idempotency identifier.
    pub plan_id: String,
    /// Operator identity copied into every applied payload.
    pub operator: String,
    /// Exact selection used to discover candidates.
    pub selection: RestoreSelection,
    /// Metadata-pruned objects that supplied grief-window candidates.
    pub selected_objects: Vec<String>,
    /// Every selected candidate, named even when refused or held.
    pub entities: Vec<RestorePlanEntity>,
}

/// Read-only source of adjudication products that must hold an entity at its
/// current value.
#[async_trait::async_trait]
pub trait RestoreHoldDetector: Send + Sync {
    /// Return the product holding `entity`, or `None` when restoration may proceed.
    async fn held_by(
        &self,
        selection: &RestoreSelection,
        entity: PersistId,
    ) -> Result<Option<String>, RestoreError>;
}

#[derive(Debug, Default)]
struct NoRestoreHolds;

#[async_trait::async_trait]
impl RestoreHoldDetector for NoRestoreHolds {
    async fn held_by(
        &self,
        _selection: &RestoreSelection,
        _entity: PersistId,
    ) -> Result<Option<String>, RestoreError> {
        Ok(None)
    }
}

/// FoundationDB-backed join over the `yc` restore-hold index.
///
/// The index is ordered by `(source node, entity, product)`. Evidence products
/// carry client ticks rather than a server LSN, so the reader conservatively
/// reads the entity's retained products instead of comparing incomparable
/// evidence and restore axes.
#[cfg(feature = "fdb")]
#[derive(Clone)]
pub struct FdbRestoreHoldDetector {
    db: Arc<foundationdb::Database>,
}

#[cfg(feature = "fdb")]
impl FdbRestoreHoldDetector {
    /// Bind the detector to a process-scoped FoundationDB handle.
    #[must_use]
    pub fn from_database(db: Arc<foundationdb::Database>) -> Self {
        Self { db }
    }
}

#[cfg(feature = "fdb")]
#[async_trait::async_trait]
impl RestoreHoldDetector for FdbRestoreHoldDetector {
    async fn held_by(
        &self,
        selection: &RestoreSelection,
        entity: PersistId,
    ) -> Result<Option<String>, RestoreError> {
        use futures::TryStreamExt as _;

        let start = crate::keyspace::restore_hold_range_start(&selection.source_node, entity);
        let end = crate::keyspace::restore_hold_range_end(&selection.source_node, entity);
        self.db
            .run(|trx, _| {
                let start = start.clone();
                let end = end.clone();
                async move {
                    let mut rows = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                start.as_slice(),
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                            limit: Some(1),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    let Some(row) = rows.try_next().await? else {
                        return Ok(None);
                    };
                    let Some((source, indexed_entity, product)) =
                        crate::keyspace::decode_restore_hold_key(row.key())
                    else {
                        return Err(foundationdb::FdbBindingError::new_custom_error(Box::new(
                            RestoreError::Index("malformed restore-hold index key".into()),
                        )));
                    };
                    if source != selection.source_node || indexed_entity != entity {
                        return Err(foundationdb::FdbBindingError::new_custom_error(Box::new(
                            RestoreError::Index("restore-hold index escaped entity range".into()),
                        )));
                    }
                    Ok(Some(product.stable_key()))
                }
            })
            .await
            .map_err(|error| RestoreError::Index(format!("restore-hold entity read: {error}")))
    }
}

/// Planner over the landed archive store and `jarchive/` metadata seams.
pub struct RestorePlanner {
    store: Arc<dyn ArchiveStore>,
    index: Arc<dyn JarchiveIndex>,
    holds: Arc<dyn RestoreHoldDetector>,
}

impl RestorePlanner {
    /// Construct a planner with no deployment-specific adjudication join.
    #[must_use]
    pub fn new(store: Arc<dyn ArchiveStore>, index: Arc<dyn JarchiveIndex>) -> Self {
        Self {
            store,
            index,
            holds: Arc::new(NoRestoreHolds),
        }
    }

    /// Install the read-only adjudication join used to produce held outcomes.
    #[must_use]
    pub fn with_hold_detector(mut self, holds: Arc<dyn RestoreHoldDetector>) -> Self {
        self.holds = holds;
        self
    }

    /// Select archive objects and assemble each affected entity's image
    /// strictly before `selection.start_lsn`.
    ///
    /// # Errors
    ///
    /// Refuses malformed ranges, unreadable/corrupt objects, invalid record
    /// payloads, and metadata/index failures. A history hole affecting only an
    /// entity is represented by `RestoreDisposition::Refused` instead.
    pub async fn plan(&self, request: RestoreRequest) -> Result<RestorePlan, RestoreError> {
        validate_request(&request)?;
        let rows = self.index.rows(&request.selection.source_node).await?;
        let selected_rows: Vec<&JarchiveRow> = rows
            .iter()
            .filter(|row| metadata_overlaps(&row.metadata, &request.selection))
            .collect();
        let selected_objects = selected_rows
            .iter()
            .map(|row| row.metadata.object_key.clone())
            .collect();

        let mut cache = HashMap::<u64, Vec<StoredRecord>>::new();
        // A spawn in the selected LSN/cell window is positive evidence that
        // the entity was absent immediately before that spawn. It is the one
        // case with no earlier image where an absence target is safe to
        // construct. This evidence is independent of the optional author
        // filter, which only selects whose touches are restore candidates.
        let mut candidates = BTreeMap::<PersistId, CellId>::new();
        let mut spawned_in_selection = BTreeSet::<PersistId>::new();
        for row in selected_rows {
            let records = self.read_row(row, &mut cache)?;
            for stored in records {
                let record = &stored.record;
                let in_selection = record.grid == request.selection.grid
                    && cell_contains(
                        request.selection.start_cell,
                        request.selection.end_cell,
                        record.cell,
                    )
                    && request.selection.start_lsn <= stored.lsn
                    && stored.lsn <= request.selection.end_lsn;
                if in_selection && record.kind == RecordKind::Spawn {
                    spawned_in_selection.insert(record.entity);
                }
                if in_selection
                    && request
                        .selection
                        .author
                        .as_ref()
                        .is_none_or(|author| author == &record.author)
                    && record.kind != RecordKind::CheckpointMark
                {
                    candidates.entry(record.entity).or_insert(record.cell);
                }
            }
        }

        // A state-bearing record is a whole-image replacement in the landed
        // actor fold. Consequently the last one before L0 is sufficient; no
        // opaque component bytes are guessed or merged here.
        let candidate_ids: BTreeSet<PersistId> = candidates.keys().copied().collect();
        let mut images = BTreeMap::<PersistId, Preimage>::new();
        for row in rows
            .iter()
            .filter(|row| row.metadata.lsn_span.start < request.selection.start_lsn)
        {
            let records = self.read_row(row, &mut cache)?;
            for stored in records {
                if stored.lsn >= request.selection.start_lsn
                    || !candidate_ids.contains(&stored.record.entity)
                {
                    continue;
                }
                if images
                    .get(&stored.record.entity)
                    .is_some_and(|image| image.lsn >= stored.lsn)
                {
                    continue;
                }
                if let Some(image) = preimage_from_record(stored)? {
                    images.insert(stored.record.entity, image);
                }
            }
        }

        let mut entities = Vec::with_capacity(candidates.len());
        for (entity, grief_cell) in candidates {
            let disposition =
                if let Some(product) = self.holds.held_by(&request.selection, entity).await? {
                    RestoreDisposition::Held { product }
                } else if let Some(image) = images.remove(&entity) {
                    RestoreDisposition::Restorable {
                        cell: image.cell,
                        target: image.target,
                    }
                } else if spawned_in_selection.contains(&entity) {
                    RestoreDisposition::Restorable {
                        cell: grief_cell,
                        target: RestoreTarget::Absent,
                    }
                } else {
                    RestoreDisposition::Refused(RestoreRefusal::PreimageUnavailable)
                };
            entities.push(RestorePlanEntity {
                entity,
                disposition,
            });
        }

        Ok(RestorePlan {
            plan_id: request.plan_id,
            operator: request.operator,
            selection: request.selection,
            selected_objects,
            entities,
        })
    }

    fn read_row<'a>(
        &self,
        row: &JarchiveRow,
        cache: &'a mut HashMap<u64, Vec<StoredRecord>>,
    ) -> Result<&'a [StoredRecord], RestoreError> {
        let records = match cache.entry(row.segment_seq) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let bytes = self
                    .store
                    .get(&row.metadata.object_key)?
                    .ok_or_else(|| RestoreError::ObjectMissing(row.metadata.object_key.clone()))?;
                let actual = *blake3::hash(&bytes).as_bytes();
                if actual != row.metadata.checksum {
                    return Err(RestoreError::ChecksumMismatch(
                        row.metadata.object_key.clone(),
                    ));
                }
                let records = decode_object(&bytes)?;
                for stored in &records {
                    if stored.lsn != stored.record.lsn {
                        return Err(RestoreError::CorruptRecord {
                            lsn: stored.lsn,
                            reason: "archive LSN column disagrees with record LSN".into(),
                        });
                    }
                    if payload_crc(&stored.record.payload) != stored.record.crc {
                        return Err(RestoreError::CorruptRecord {
                            lsn: stored.lsn,
                            reason: "payload crc mismatch".into(),
                        });
                    }
                }
                entry.insert(records)
            }
        };
        Ok(records)
    }
}

#[derive(Debug)]
struct Preimage {
    lsn: Lsn,
    cell: CellId,
    target: RestoreTarget,
}

fn preimage_from_record(stored: &StoredRecord) -> Result<Option<Preimage>, RestoreError> {
    let record = &stored.record;
    let (cell, target) = match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff => (
            record.cell,
            RestoreTarget::Present {
                components: record.payload.clone(),
                schema_floor: SCHEMA_V0,
            },
        ),
        RecordKind::Despawn => (record.cell, RestoreTarget::Absent),
        RecordKind::Rekey => {
            let rekey: EntityRekey = postcard::from_bytes(&record.payload).map_err(|error| {
                RestoreError::CorruptRecord {
                    lsn: stored.lsn,
                    reason: format!("rekey payload: {error}"),
                }
            })?;
            (
                rekey.destination_cell,
                RestoreTarget::Present {
                    components: rekey.source_record,
                    schema_floor: rekey.source_schema_floor,
                },
            )
        }
        RecordKind::Restore => {
            let restore = actor::decode_restore_record(record).map_err(|error| {
                RestoreError::CorruptRecord {
                    lsn: stored.lsn,
                    reason: error.to_string(),
                }
            })?;
            (record.cell, restore.target)
        }
        RecordKind::CheckpointMark => return Ok(None),
    };
    Ok(Some(Preimage {
        lsn: stored.lsn,
        cell,
        target,
    }))
}

fn validate_request(request: &RestoreRequest) -> Result<(), RestoreError> {
    if request.plan_id.is_empty() {
        return Err(RestoreError::InvalidRequest("plan_id is empty".into()));
    }
    if request.operator.is_empty() {
        return Err(RestoreError::InvalidRequest("operator is empty".into()));
    }
    if request.selection.start_cell > request.selection.end_cell {
        return Err(RestoreError::InvalidRequest(
            "cell range start exceeds end".into(),
        ));
    }
    if request.selection.start_lsn > request.selection.end_lsn {
        return Err(RestoreError::InvalidRequest(
            "LSN range start exceeds end".into(),
        ));
    }
    Ok(())
}

fn metadata_overlaps(
    metadata: &crate::keyspace::JarchiveMetadata,
    selection: &RestoreSelection,
) -> bool {
    metadata.lsn_span.start <= selection.end_lsn
        && selection.start_lsn <= metadata.lsn_span.end
        && metadata.cell_ranges.iter().any(|range| {
            range.grid == selection.grid
                && cell_ranges_overlap(
                    range.start,
                    range.end,
                    selection.start_cell,
                    selection.end_cell,
                )
        })
}

fn cell_contains(start: CellId, end: CellId, cell: CellId) -> bool {
    let cell = cell.to_bits();
    let start = start.to_bits();
    let end = end.to_bits();
    cell >= start && (cell < end || (end == u64::MAX && cell == u64::MAX))
}

fn cell_ranges_overlap(a_start: CellId, a_end: CellId, b_start: CellId, b_end: CellId) -> bool {
    cell_contains(a_start, a_end, b_start)
        || cell_contains(b_start, b_end, a_start)
        || (a_end.to_bits() == u64::MAX && b_end.to_bits() == u64::MAX)
}

/// Per-entity result of applying a plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RestoreApplyDisposition {
    /// A new restoration record was appended and durably committed.
    Applied {
        /// Current journal position assigned to the restore.
        lsn: Lsn,
    },
    /// This plan/entity pair already has a retained restoration record.
    AlreadyApplied {
        /// Position of the retained record proving this entity was applied.
        lsn: Lsn,
    },
    /// The planner refused this entity; the applier wrote nothing.
    Refused(RestoreRefusal),
    /// An adjudication product held this entity; the applier wrote nothing.
    Held {
        /// Stable description/key of the adjudication product.
        product: String,
    },
}

/// One entity in an apply outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreApplyEntity {
    /// Candidate entity.
    pub entity: PersistId,
    /// What the applier did.
    pub disposition: RestoreApplyDisposition,
}

/// Complete result suitable for the operator surface's `<request>.result`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreApplyReport {
    /// Applied plan/request id.
    pub plan_id: String,
    /// Per-entity outcomes in plan order.
    pub entities: Vec<RestoreApplyEntity>,
}

/// Forward applier bound to one running cell runtime.
pub struct RestoreApplier<'a> {
    runtime: &'a CellRuntime,
    applying_node: NodeId,
}

impl<'a> RestoreApplier<'a> {
    /// Bind an applier. `applying_node` becomes `JournalRecord::author`.
    #[must_use]
    pub fn new(runtime: &'a CellRuntime, applying_node: NodeId) -> Self {
        Self {
            runtime,
            applying_node,
        }
    }

    /// Append every restorable image at the current journal tail.
    ///
    /// The entity stripe gate covers both the rerun check and the append, so
    /// concurrent attempts with the same plan cannot both pass it. The fence
    /// row is read and required to name this runtime as active; it is never
    /// changed. Checkpoints and every critical/ledger family are unreachable
    /// from this type.
    pub async fn apply(
        &self,
        plan: &RestorePlan,
        current_tick: Tick,
    ) -> Result<RestoreApplyReport, RestoreError> {
        let mut entities = Vec::with_capacity(plan.entities.len());
        for planned in &plan.entities {
            let disposition = match &planned.disposition {
                RestoreDisposition::Refused(reason) => {
                    RestoreApplyDisposition::Refused(reason.clone())
                }
                RestoreDisposition::Held { product } => RestoreApplyDisposition::Held {
                    product: product.clone(),
                },
                RestoreDisposition::Restorable { cell, target } => {
                    let gate = self
                        .runtime
                        .entity_gate(plan.selection.grid, planned.entity);
                    let _guard = gate.lock_owned().await;
                    if let Some(lsn) = already_applied(
                        self.runtime,
                        &plan.plan_id,
                        &plan.operator,
                        planned.entity,
                        *cell,
                        target,
                    )? {
                        RestoreApplyDisposition::AlreadyApplied { lsn }
                    } else {
                        // A single Restore record is replayed by the actor
                        // owning its envelope cell. Refuse a live cross-shard
                        // move rather than leave the current actor holding a
                        // second copy; committed location moves require the
                        // dedicated rekey transaction.
                        let mut current_cell = None;
                        let shards = self.runtime.shards().collect::<Vec<_>>();
                        for shard in shards {
                            let Some(actor) = self.runtime.actor(plan.selection.grid, shard) else {
                                continue;
                            };
                            if let Some(found) =
                                actor
                                    .committed_entity_cell(planned.entity)
                                    .await
                                    .map_err(|error| RestoreError::Apply(format!("{error:?}")))?
                            {
                                if current_cell.replace(found).is_some() {
                                    return Err(RestoreError::Apply(format!(
                                        "entity {} exists in more than one actor",
                                        planned.entity.0
                                    )));
                                }
                            }
                        }
                        if current_cell.is_some_and(|current| current != *cell) {
                            return Err(RestoreError::Apply(format!(
                                "entity {} is currently at {}, but plan targets {}; use the committed rekey path",
                                planned.entity.0,
                                current_cell.expect("checked Some"),
                                cell
                            )));
                        }
                        let shard = self
                            .runtime
                            .owning_shard(plan.selection.grid, *cell)
                            .ok_or(RestoreError::WrongOwner {
                                grid: plan.selection.grid,
                                cell: *cell,
                            })?;
                        let fence = self
                            .runtime
                            .fence()
                            .read(plan.selection.grid, shard)
                            .await
                            .map_err(|error| RestoreError::Fence(error.to_string()))?;
                        let expected_owner = self.runtime.cluster_node_id();
                        if !fence.is_some_and(|row| {
                            row.owner == expected_owner && row.status == FenceStatus::Active
                        }) {
                            return Err(RestoreError::Fence(format!(
                                "shard {shard} is not actively owned by node {expected_owner}"
                            )));
                        }

                        let payload = postcard::to_stdvec(&RestoreRecord {
                            version: RESTORE_RECORD_VERSION,
                            plan_id: plan.plan_id.clone(),
                            operator: plan.operator.clone(),
                            target: target.clone(),
                        })
                        .map_err(|error| RestoreError::Encode(error.to_string()))?;
                        let record = JournalRecord {
                            lsn: Lsn::new(0, 0),
                            cell: *cell,
                            grid: plan.selection.grid,
                            entity: planned.entity,
                            tick: current_tick,
                            epoch: Epoch::new(0),
                            author: self.applying_node,
                            kind: RecordKind::Restore,
                            crc: payload_crc(&payload),
                            payload: payload.into(),
                        };
                        actor::decode_restore_record(&record)
                            .map_err(|error| RestoreError::Encode(error.to_string()))?;
                        let handle = self
                            .runtime
                            .actor(record.grid, record.cell)
                            .ok_or(RestoreError::WrongOwner {
                                grid: record.grid,
                                cell: record.cell,
                            })?
                            .start_diff(record)
                            .await
                            .map_err(|error| RestoreError::Apply(format!("{error:?}")))?;
                        let lsn = handle
                            .committed()
                            .await
                            .map_err(|error| RestoreError::Apply(error.to_string()))?;
                        RestoreApplyDisposition::Applied { lsn }
                    }
                }
            };
            entities.push(RestoreApplyEntity {
                entity: planned.entity,
                disposition,
            });
        }
        Ok(RestoreApplyReport {
            plan_id: plan.plan_id.clone(),
            entities,
        })
    }
}

fn already_applied(
    runtime: &CellRuntime,
    plan_id: &str,
    operator: &str,
    entity: PersistId,
    cell: CellId,
    target: &RestoreTarget,
) -> Result<Option<Lsn>, RestoreError> {
    for item in runtime
        .journal()
        .scan_from(runtime.journal().released_floor())
    {
        let stored = item?;
        if stored.record.kind != RecordKind::Restore || stored.record.entity != entity {
            continue;
        }
        let restore = actor::decode_restore_record(&stored.record).map_err(|error| {
            RestoreError::CorruptRecord {
                lsn: stored.lsn,
                reason: error.to_string(),
            }
        })?;
        if restore.plan_id == plan_id {
            if stored.record.cell != cell
                || restore.operator != operator
                || &restore.target != target
            {
                return Err(RestoreError::InvalidRequest(format!(
                    "plan_id {plan_id:?} was already used for a different restore of entity {}",
                    entity.0
                )));
            }
            return Ok(Some(stored.lsn));
        }
    }
    Ok(None)
}

/// Planner/applier failure that cannot be reduced to one named entity refusal.
#[derive(Debug)]
pub enum RestoreError {
    /// Request fields do not form a meaningful selection.
    InvalidRequest(String),
    /// An indexed object is missing from the archive store.
    ObjectMissing(String),
    /// Indexed checksum and stored bytes disagree.
    ChecksumMismatch(String),
    /// One archived record cannot be trusted or interpreted.
    CorruptRecord {
        /// Archived journal position of the bad record.
        lsn: Lsn,
        /// Integrity or decoding failure.
        reason: String,
    },
    /// The live runtime no longer owns the planned cell.
    WrongOwner {
        /// Grid the plan targets.
        grid: GridId,
        /// Cell no active local actor owns.
        cell: CellId,
    },
    /// The current durable epoch fence does not authorize this runtime.
    Fence(String),
    /// Restore payload serialization failed.
    Encode(String),
    /// Actor/journal application failed.
    Apply(String),
    /// Archive metadata/index access failed.
    Index(String),
    /// Archive object-store access failed.
    Store(String),
    /// Parquet object decoding failed.
    Object(String),
    /// Journal scan failed.
    Journal(String),
}

impl core::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidRequest(reason) => write!(f, "invalid restore request: {reason}"),
            Self::ObjectMissing(key) => write!(f, "archive object is missing: {key}"),
            Self::ChecksumMismatch(key) => write!(f, "archive checksum mismatch: {key}"),
            Self::CorruptRecord { lsn, reason } => {
                write!(f, "corrupt archive record at {lsn}: {reason}")
            }
            Self::WrongOwner { grid, cell } => {
                write!(f, "runtime does not own restore target {grid}/{cell}")
            }
            Self::Fence(reason) => write!(f, "restore fence refused: {reason}"),
            Self::Encode(reason) => write!(f, "restore payload encode: {reason}"),
            Self::Apply(reason) => write!(f, "restore apply: {reason}"),
            Self::Index(reason) => write!(f, "restore metadata index: {reason}"),
            Self::Store(reason) => write!(f, "restore archive store: {reason}"),
            Self::Object(reason) => write!(f, "restore archive object: {reason}"),
            Self::Journal(reason) => write!(f, "restore journal scan: {reason}"),
        }
    }
}

impl core::error::Error for RestoreError {}

impl From<crate::checkpoint::CheckpointError> for RestoreError {
    fn from(error: crate::checkpoint::CheckpointError) -> Self {
        Self::Index(error.to_string())
    }
}

impl From<super::ArchiveStoreError> for RestoreError {
    fn from(error: super::ArchiveStoreError) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<super::ArchiveObjectError> for RestoreError {
    fn from(error: super::ArchiveObjectError) -> Self {
        Self::Object(error.to_string())
    }
}

impl From<crate::journal::JournalError> for RestoreError {
    fn from(error: crate::journal::JournalError) -> Self {
        Self::Journal(error.to_string())
    }
}
