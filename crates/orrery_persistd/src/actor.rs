//! The single-writer cell actor (docs/08-persistence.md §3).
//!
//! A cell actor is a tokio task owning all hot state for one shard cell — the
//! persistence-side twin of the single-writer invariant (D2). All mutation
//! flows through its mailbox; readers get snapshots via message, never shared
//! mutable access. The actor applies a diff, appends to the journal, and only
//! then acks — the ack *is* the durability contract (§2.1).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use orrery_protocol::{CellId, Epoch, GridId, JournalRecord, Lsn, PersistId, RecordKind, Tick};

use crate::journal::{AppendHandle, Journal};

/// How long a despawned entity's `world/` tombstone persists before the
/// checkpoint GC pass clears it (D11 §6). Must comfortably exceed the 20 s
/// checkpoint cadence (D16) so a tombstone is at least one full checkpoint
/// cycle old before it can disappear. An implementation default, not a D16
/// parameter.
pub const TOMBSTONE_RETENTION_MS: u64 = 300_000;

/// A despawn marker (D11 §6 `world/{cell_id}/{entity_id}` *tombstone*).
///
/// When an entity despawns, the actor keeps a tombstone so the next checkpoint
/// overwrites the entity's `world/` row with the marker (never silently
/// leaving the stale live row to be resurrected by a cold scan), and so the
/// checkpoint GC pass knows when the row can be cleared. The tombstone
/// outlives the entity in the actor map, but only until its GC deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tombstone {
    /// The cell the entity lived in when it despawned — the key scope of the
    /// `world/` row (grid-relative, grid carried by the checkpoint).
    pub cell: CellId,
    /// The universe tick the entity despawned at.
    pub tick: Tick,
    /// Wall-clock time past which the checkpoint GC pass clears the row,
    /// as unix milliseconds.
    pub gc_deadline_ms: u64,
}

/// An opaque entity record: component bytes plus a dirty flag (§3.1).
///
/// Components are stored as postcard bytes so the actor never needs the game's
/// component types — only the `Ruleset` does (which lands in a later slice).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityRecord {
    /// The last component bag for this entity (opaque bytes).
    pub components: bytes::Bytes,
    /// Whether this entity was touched since the last checkpoint.
    pub dirty: bool,
}

/// The result of admitting a diff (currently: accepted with an LSN, or NACKed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accept {
    /// The record was accepted and durably flushed at `lsn`.
    Accepted(Lsn),
}

/// A rejection of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// The journal refused the append.
    JournalClosed,
}

/// A snapshot page returned to a reader (§3.1 `ReadSnapshot`).
#[derive(Debug, Clone, Default)]
pub struct SnapshotPage {
    /// Entities in the requested cells, keyed by `PersistId`.
    pub entities: HashMap<PersistId, EntityRecord>,
}

/// The actor state needed to write a checkpoint (§8).
#[derive(Debug, Clone)]
pub struct CheckpointSnapshot {
    /// The shard cell this actor owns.
    pub shard: orrery_protocol::CellId,
    /// The grid the shard's `CellId` space belongs to (P-7, D11 §6).
    pub grid: GridId,
    /// The shard-ownership epoch.
    pub epoch: Epoch,
    /// The entity bag.
    pub entities: HashMap<PersistId, EntityRecord>,
    /// The cell each entity currently lives in (split partitioning, §3.5).
    pub by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    /// Despawn markers not yet past their GC deadline (D11 §6).
    pub tombstones: HashMap<PersistId, Tombstone>,
    /// The journal LSN covered by the last checkpoint.
    pub ckpt_watermark: Lsn,
}

/// The partitioned state of a parent actor at split time (§3.5).
///
/// The parent's entity bag is split by which of its eight child cells each
/// entity belongs to, so the runtime can spawn one child actor per partition
/// and retire the parent.
#[derive(Debug, Clone)]
pub struct SplitSnapshot {
    /// The shard cell being split.
    pub shard: orrery_protocol::CellId,
    /// The grid the shard's `CellId` space belongs to (P-7).
    pub grid: GridId,
    /// The shard-ownership epoch.
    pub epoch: Epoch,
    /// Entities partitioned by which child cell they belong to.
    pub children: HashMap<orrery_protocol::CellId, HashMap<PersistId, EntityRecord>>,
    /// Each partition's per-entity cell map (so a child actor is fully
    /// initialized for a subsequent split, §3.5).
    pub by_cell: HashMap<orrery_protocol::CellId, HashMap<PersistId, orrery_protocol::CellId>>,
    /// Each partition's despawn markers (a child actor inherits the tombstones
    /// of the entities that lived in its subtree).
    pub tombstones: HashMap<orrery_protocol::CellId, HashMap<PersistId, Tombstone>>,
    /// The journal LSN covered by the last checkpoint.
    pub ckpt_watermark: Lsn,
}

/// A message to a cell actor's mailbox (§3.1).
#[derive(Debug)]
pub enum CellMsg {
    /// Apply a diff and return the durable LSN.
    ///
    /// The actor stamps its ownership epoch into the record (it is the epoch
    /// authority — the gateway's `Epoch::new(0)` is a placeholder, D11 §2.1),
    /// appends it to the journal and folds it into hot state **synchronously**
    /// (so LSN assignment order, hot-state order, and mailbox order agree,
    /// §3.1), then returns the pending append handle. The mailbox returns to
    /// `rx.recv()` immediately; the gateway route task owns the durability
    /// wait and only acks after group fsync (§2.1).
    ApplyDiff {
        /// The record to apply.
        record: JournalRecord,
        /// Reply channel for the result.
        reply: oneshot::Sender<Result<Arc<AppendHandle>, Reject>>,
    },
    /// Read a snapshot of the given cells.
    ReadSnapshot {
        /// The cells to read.
        cells: Vec<orrery_protocol::CellId>,
        /// Reply channel for the page.
        reply: oneshot::Sender<SnapshotPage>,
    },
    /// Capture the actor's full state for a checkpoint (§8).
    CheckpointSnapshot {
        /// Reply channel for the snapshot.
        reply: oneshot::Sender<CheckpointSnapshot>,
    },
    /// Partition the actor's state for a hotspot split (§3.5).
    Split {
        /// Reply channel for the partitioned snapshot.
        reply: oneshot::Sender<SplitSnapshot>,
    },
    /// Restore state into the actor without journaling (recovery, §3.4).
    ///
    /// Never durable: used to load a checkpoint base and fold the journal tail.
    Restore {
        /// Entities to (re)set into the map.
        entities: HashMap<PersistId, EntityRecord>,
        /// The cell each entity lives in (split partitioning, §3.5).
        by_cell: HashMap<PersistId, orrery_protocol::CellId>,
        /// Despawn markers carried by the checkpoint (D11 §6).
        tombstones: HashMap<PersistId, Tombstone>,
        /// The new checkpoint watermark.
        watermark: Lsn,
        /// Reply channel.
        reply: oneshot::Sender<()>,
    },
    /// Set only the checkpoint watermark (recovery, §3.4).
    ///
    /// Distinct from [`CellMsg::Restore`] so setting the watermark does not
    /// wipe the entity bag / cell map just restored from a checkpoint.
    SetWatermark {
        /// The new checkpoint watermark.
        watermark: Lsn,
        /// Reply channel.
        reply: oneshot::Sender<()>,
    },
    /// Fold a single replayed record into state (recovery, no journaling).
    RestoreRecord {
        /// The record to fold.
        record: JournalRecord,
        /// Reply channel.
        reply: oneshot::Sender<()>,
    },
    /// Drop tombstones whose GC deadline has passed (D11 §6, checkpoint GC
    /// pass). The durable rows were cleared by the checkpoint that just
    /// committed; dropping them here stops the next checkpoint from rewriting
    /// them.
    PruneTombstones {
        /// Wall-clock "now", as unix milliseconds.
        now_ms: u64,
        /// Reply channel.
        reply: oneshot::Sender<()>,
    },
    /// Shut the actor down after draining the mailbox.
    Shutdown,
}

/// The actor's in-memory state (§3.1).
#[derive(Debug)]
pub struct CellActorState {
    /// The shard cell this actor owns.
    pub shard: orrery_protocol::CellId,
    /// The grid the shard's `CellId` space belongs to (P-7).
    pub grid: GridId,
    /// The shard-ownership epoch (fencing token, §3.4).
    pub epoch: Epoch,
    /// Entities in this actor's cells, keyed by `PersistId`.
    pub entities: HashMap<PersistId, EntityRecord>,
    /// The cell each entity currently lives in (split partitioning, §3.5).
    pub by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    /// Despawn markers not yet past their GC deadline (D11 §6).
    pub tombstones: HashMap<PersistId, Tombstone>,
    /// The journal LSN covered by the last checkpoint.
    pub ckpt_watermark: Lsn,
}

/// The actor task's environment: its state plus the shared journal.
struct ActorEnv {
    state: CellActorState,
    journal: Arc<Journal>,
}

/// Run the actor's mailbox loop until [`CellMsg::Shutdown`].
async fn actor_loop(env: &mut ActorEnv, mut rx: mpsc::Receiver<CellMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            CellMsg::ApplyDiff { record, reply } => {
                apply_diff(env, record, reply);
            }
            CellMsg::ReadSnapshot { cells, reply } => {
                let _ = reply.send(read_snapshot(env, &cells));
            }
            CellMsg::CheckpointSnapshot { reply } => {
                let _ = reply.send(checkpoint_snapshot(env));
            }
            CellMsg::Split { reply } => {
                let _ = reply.send(split_snapshot(env));
            }
            CellMsg::Restore {
                entities,
                by_cell,
                tombstones,
                watermark,
                reply,
            } => {
                env.state.entities = entities;
                env.state.by_cell = by_cell;
                env.state.tombstones = tombstones;
                env.state.ckpt_watermark = watermark;
                let _ = reply.send(());
            }
            CellMsg::SetWatermark { watermark, reply } => {
                env.state.ckpt_watermark = watermark;
                let _ = reply.send(());
            }
            CellMsg::RestoreRecord { record, reply } => {
                fold(env, &record, now_ms());
                let _ = reply.send(());
            }
            CellMsg::PruneTombstones { now_ms, reply } => {
                env.state
                    .tombstones
                    .retain(|_, t| t.gc_deadline_ms > now_ms);
                let _ = reply.send(());
            }
            CellMsg::Shutdown => break,
        }
    }
}

/// The current wall-clock time as unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Apply a diff: stamp the epoch, journal the record, fold it into hot state,
/// and return the pending append handle — so the mailbox returns to
/// `rx.recv()` immediately instead of serializing on the fsync.
///
/// The record is stamped, journaled, and folded synchronously (this is the
/// serial single-writer section — LSN assignment order and hot-state order
/// are both mailbox order, §3.1), then the [`AppendHandle`] moves back to the
/// gateway route task through the reply oneshot. That task awaits group fsync
/// before sending the ack, while many appends from one actor coexist in the
/// commit queue and share fsyncs (§4 adaptive group commit, D16).
fn apply_diff(
    env: &mut ActorEnv,
    mut record: JournalRecord,
    reply: oneshot::Sender<Result<Arc<AppendHandle>, Reject>>,
) {
    // The actor is the epoch authority: the record is durably stamped with
    // the shard-ownership epoch here (§3.4), overwriting the gateway's
    // placeholder `Epoch::new(0)` (D11 §2.1: the server assigns epoch/lsn).
    // The stamping must precede the append so the journaled bytes carry it.
    record.epoch = env.state.epoch;
    let handle = match env.journal.append(record.clone()) {
        Ok(handle) => handle,
        Err(_) => {
            let _ = reply.send(Err(Reject::JournalClosed));
            return;
        }
    };
    // `append` stamped the assigned LSN into the stored bytes but not into
    // `record` (it took a clone); stamp it from the handle so the fold below
    // advances the actor's `ckpt_watermark` past this record.
    record.lsn = handle.lsn();

    // Fold into hot state before returning the handle: an ack resolves
    // only after the record is BOTH durably journaled and reflected in the
    // snapshot, so a kill between commit and fold cannot lose the fold (the
    // fold precedes the ack). Last-writer-wins per entity, keyed by
    // (entity, tick), so replay of the same record is a no-op.
    fold(env, &record, now_ms());

    let _ = reply.send(Ok(handle));
}

/// Fold a journal record into in-memory state (no durability work).
fn fold(env: &mut ActorEnv, record: &JournalRecord, now_ms: u64) {
    match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff => {
            let entry = env.state.entities.entry(record.entity).or_default();
            entry.components = record.payload.clone();
            entry.dirty = true;
            env.state.by_cell.insert(record.entity, record.cell);
            // A re-spawn (id reuse across a despawn) cancels the marker.
            env.state.tombstones.remove(&record.entity);
        }
        RecordKind::Despawn => {
            env.state.entities.remove(&record.entity);
            env.state.by_cell.remove(&record.entity);
            // Tombstone, never plain deletion: the `world/` row must be
            // overwritten by the marker at the next checkpoint, not left to
            // resurrect a dead entity on the next cold scan (D11 §6, P-6).
            env.state.tombstones.insert(
                record.entity,
                Tombstone {
                    cell: record.cell,
                    tick: record.tick,
                    gc_deadline_ms: now_ms + TOMBSTONE_RETENTION_MS,
                },
            );
        }
        // Terrain, Rekey, CheckpointMark are recognized but not folded into the
        // entity map in this slice.
        RecordKind::TerrainDelta | RecordKind::Rekey | RecordKind::CheckpointMark => {}
    }
    env.state.ckpt_watermark = env.state.ckpt_watermark.max(record.lsn);
}

fn read_snapshot(env: &ActorEnv, cells: &[orrery_protocol::CellId]) -> SnapshotPage {
    let mut entities = HashMap::new();
    for (id, record) in &env.state.entities {
        let entity_cell = env
            .state
            .by_cell
            .get(id)
            .copied()
            .unwrap_or(env.state.shard);
        if cells.iter().any(|c| c.is_prefix_of(entity_cell)) {
            entities.insert(*id, record.clone());
        }
    }
    SnapshotPage { entities }
}

/// Capture the actor's full state for a checkpoint (§8, copy-on-update posture:
/// the snapshot is a clone taken inside the mailbox so serialization never
/// blocks appends).
fn checkpoint_snapshot(env: &ActorEnv) -> CheckpointSnapshot {
    CheckpointSnapshot {
        shard: env.state.shard,
        grid: env.state.grid,
        epoch: env.state.epoch,
        entities: env.state.entities.clone(),
        by_cell: env.state.by_cell.clone(),
        tombstones: env.state.tombstones.clone(),
        ckpt_watermark: env.state.ckpt_watermark,
    }
}

/// Partition the actor's entity bag by which of its eight child cells each
/// entity belongs to (§3.5).
fn split_snapshot(env: &ActorEnv) -> SplitSnapshot {
    let children = env.state.shard.children();
    let mut out: HashMap<_, HashMap<_, _>> =
        children.iter().map(|&c| (c, HashMap::new())).collect();
    let mut by_cell: HashMap<_, HashMap<_, _>> =
        children.iter().map(|&c| (c, HashMap::new())).collect();
    let mut tombstones: HashMap<_, HashMap<_, _>> =
        children.iter().map(|&c| (c, HashMap::new())).collect();
    for (entity, record) in &env.state.entities {
        let cell = env
            .state
            .by_cell
            .get(entity)
            .copied()
            .unwrap_or(env.state.shard);
        // The entity's cell is a descendant of exactly one child.
        let child = children
            .iter()
            .find(|c| c.is_prefix_of(cell))
            .copied()
            .unwrap_or(env.state.shard);
        out.get_mut(&child)
            .expect("child present")
            .insert(*entity, record.clone());
        by_cell
            .get_mut(&child)
            .expect("child present")
            .insert(*entity, cell);
    }
    for (entity, tomb) in &env.state.tombstones {
        // The entity's cell is a descendant of exactly one child.
        let child = children
            .iter()
            .find(|c| c.is_prefix_of(tomb.cell))
            .copied()
            .unwrap_or(env.state.shard);
        tombstones
            .get_mut(&child)
            .expect("child present")
            .insert(*entity, *tomb);
    }
    SplitSnapshot {
        shard: env.state.shard,
        grid: env.state.grid,
        epoch: env.state.epoch,
        children: out,
        by_cell,
        tombstones,
        ckpt_watermark: env.state.ckpt_watermark,
    }
}

/// A handle to a running cell actor (sender + shared config).
///
/// Cloneable (the mailbox is multi-producer by design): every clone talks to
/// the same actor task. The task's join handle lives in the shared
/// [`ActorJoinSet`] handed to [`spawn_preloaded`], so a runtime shutting down
/// awaits all of its actors from one place instead of consuming handles one
/// by one.
#[derive(Debug, Clone)]
pub struct CellActorHandle {
    tx: mpsc::Sender<CellMsg>,
    shard: orrery_protocol::CellId,
    grid: GridId,
    epoch: Epoch,
}

/// The join handles of a runtime's actor tasks.
///
/// Spawn registers each actor's task here; [`CellRuntime::close`] drains the
/// set after sending every actor its `Shutdown`, releasing their
/// `Arc<Journal>`s (and the journal's file lock) before returning.
#[derive(Debug, Default)]
pub struct ActorJoinSet {
    joins: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ActorJoinSet {
    /// A new, empty join set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an actor task's join handle.
    pub(crate) fn add(&self, join: tokio::task::JoinHandle<()>) {
        self.joins.lock().expect("actor join set lock").push(join);
    }

    /// Await every registered actor task (after their mailboxes were shut
    /// down), releasing each task's `Arc<Journal>`.
    pub async fn join_all(&self) {
        let joins: Vec<_> = self
            .joins
            .lock()
            .expect("actor join set lock")
            .drain(..)
            .collect();
        for join in joins {
            let _ = join.await;
        }
    }
}

impl CellActorHandle {
    /// Stamp, append, and fold a diff, returning its pending durability handle.
    /// The caller owns the wait so the actor creates no task per append.
    pub async fn start_diff(&self, record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ApplyDiff { record, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }

    /// Apply a diff, awaiting its own durable LSN.
    pub async fn apply_diff(&self, record: JournalRecord) -> Result<Lsn, Reject> {
        let handle = self.start_diff(record).await?;
        let lsn = handle.lsn();
        handle
            .committed()
            .await
            .map(|_| lsn)
            .map_err(|_| Reject::JournalClosed)
    }

    /// Read a snapshot of the given cells.
    pub async fn read_snapshot(
        &self,
        cells: Vec<orrery_protocol::CellId>,
    ) -> Result<SnapshotPage, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ReadSnapshot { cells, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// Ask the actor to shut down after draining its mailbox. The task itself
    /// is awaited via the runtime's [`ActorJoinSet`] (`CellRuntime::close` or
    /// `split`), which releases its `Arc<Journal>` (and the journal's file
    /// lock).
    pub async fn shutdown(&self) {
        let _ = self.tx.send(CellMsg::Shutdown).await;
    }

    /// Capture the actor's full state for a checkpoint (§8).
    pub async fn checkpoint_snapshot(&self) -> Result<CheckpointSnapshot, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::CheckpointSnapshot { reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// Partition the actor's state for a hotspot split (§3.5).
    pub async fn split_snapshot(&self) -> Result<SplitSnapshot, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::Split { reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// The shard-ownership epoch this actor runs under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Restore the entity bag, per-entity cell map, despawn markers, and
    /// watermark into the actor (recovery, §3.4).
    pub async fn restore_entities(
        &self,
        entities: HashMap<PersistId, EntityRecord>,
        by_cell: HashMap<PersistId, orrery_protocol::CellId>,
        tombstones: HashMap<PersistId, Tombstone>,
    ) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::Restore {
                entities,
                by_cell,
                tombstones,
                watermark: Lsn::new(0, 0),
                reply,
            })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// Set the actor's checkpoint watermark (recovery, §3.4).
    pub async fn set_watermark(&self, watermark: Lsn) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::SetWatermark { watermark, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// Fold a single replayed record into state (recovery, no journaling).
    pub async fn restore_apply(&self, record: JournalRecord) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::RestoreRecord { record, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// Drop tombstones past `now_ms`, after the checkpoint cleared their rows
    /// (D11 §6 checkpoint GC pass). Awaiting this before the next checkpoint is
    /// what stops the marker from being rewritten forever.
    pub async fn prune_tombstones(&self, now_ms: u64) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::PruneTombstones { now_ms, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    /// The shard cell this actor owns.
    #[must_use]
    pub fn shard(&self) -> orrery_protocol::CellId {
        self.shard
    }

    /// The grid this actor's shard lives in (P-7).
    #[must_use]
    pub fn grid(&self) -> GridId {
        self.grid
    }
}

/// Spawn a cell actor for `shard` in `grid` at `epoch`, sharing `journal`,
/// starting empty. The task's join handle is registered in `joins`.
pub fn spawn(
    shard: orrery_protocol::CellId,
    grid: GridId,
    epoch: Epoch,
    journal: Arc<Journal>,
    joins: &Arc<ActorJoinSet>,
) -> CellActorHandle {
    spawn_preloaded(
        shard,
        grid,
        epoch,
        journal,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        Lsn::new(0, 0),
        joins,
    )
}

/// Spawn a cell actor for `shard` in `grid` at `epoch`, sharing `journal`,
/// preloaded with recovered state (used by restart/recovery, §3.4). The
/// task's join handle is registered in `joins`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_preloaded(
    shard: orrery_protocol::CellId,
    grid: GridId,
    epoch: Epoch,
    journal: Arc<Journal>,
    entities: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    tombstones: HashMap<PersistId, Tombstone>,
    ckpt_watermark: Lsn,
    joins: &Arc<ActorJoinSet>,
) -> CellActorHandle {
    let (tx, rx) = mpsc::channel(4096);
    let mut env = ActorEnv {
        state: CellActorState {
            shard,
            grid,
            epoch,
            entities,
            by_cell,
            tombstones,
            ckpt_watermark,
        },
        journal,
    };
    let join = tokio::spawn(async move {
        actor_loop(&mut env, rx).await;
    });
    joins.add(join);
    CellActorHandle {
        tx,
        shard,
        grid,
        epoch,
    }
}
