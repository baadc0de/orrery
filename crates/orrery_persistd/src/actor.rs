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

use orrery_protocol::{Epoch, JournalRecord, Lsn, PersistId, RecordKind};

use crate::journal::Journal;

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
    /// The shard-ownership epoch.
    pub epoch: Epoch,
    /// The entity bag.
    pub entities: HashMap<PersistId, EntityRecord>,
    /// The cell each entity currently lives in (split partitioning, §3.5).
    pub by_cell: HashMap<PersistId, orrery_protocol::CellId>,
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
    /// The shard-ownership epoch.
    pub epoch: Epoch,
    /// Entities partitioned by which child cell they belong to.
    pub children: HashMap<orrery_protocol::CellId, HashMap<PersistId, EntityRecord>>,
    /// Each partition's per-entity cell map (so a child actor is fully
    /// initialized for a subsequent split, §3.5).
    pub by_cell: HashMap<orrery_protocol::CellId, HashMap<PersistId, orrery_protocol::CellId>>,
    /// The journal LSN covered by the last checkpoint.
    pub ckpt_watermark: Lsn,
}

/// A message to a cell actor's mailbox (§3.1).
#[derive(Debug)]
pub enum CellMsg {
    /// Apply a diff and return the durable LSN.
    ApplyDiff {
        /// The record to apply.
        record: JournalRecord,
        /// Reply channel for the result.
        reply: oneshot::Sender<Result<Lsn, Reject>>,
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
    /// Shut the actor down after draining the mailbox.
    Shutdown,
}

/// The actor's in-memory state (§3.1).
#[derive(Debug)]
pub struct CellActorState {
    /// The shard cell this actor owns.
    pub shard: orrery_protocol::CellId,
    /// The shard-ownership epoch (fencing token, §3.4).
    pub epoch: Epoch,
    /// Entities in this actor's cells, keyed by `PersistId`.
    pub entities: HashMap<PersistId, EntityRecord>,
    /// The cell each entity currently lives in (split partitioning, §3.5).
    pub by_cell: HashMap<PersistId, orrery_protocol::CellId>,
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
                let result = apply_diff(env, &record).await;
                let _ = reply.send(result);
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
                watermark,
                reply,
            } => {
                env.state.entities = entities;
                env.state.by_cell = by_cell;
                env.state.ckpt_watermark = watermark;
                let _ = reply.send(());
            }
            CellMsg::SetWatermark { watermark, reply } => {
                env.state.ckpt_watermark = watermark;
                let _ = reply.send(());
            }
            CellMsg::RestoreRecord { record, reply } => {
                fold(env, &record);
                let _ = reply.send(());
            }
            CellMsg::Shutdown => break,
        }
    }
}

/// Apply a diff: journal it durably, then fold it into in-memory state.
async fn apply_diff(env: &mut ActorEnv, record: &JournalRecord) -> Result<Lsn, Reject> {
    // Journal first; the append resolves only after the group fsync.
    let handle = env
        .journal
        .append(record.clone())
        .map_err(|_| Reject::JournalClosed)?;
    let lsn = handle
        .committed()
        .await
        .map_err(|_| Reject::JournalClosed)?;

    // Fold into state. Idempotent (last-writer-wins per entity), keyed by
    // (entity, tick): replay is safe because re-applying a record is a no-op.
    fold(env, record);
    Ok(lsn)
}

/// Fold a journal record into in-memory state (no durability work).
fn fold(env: &mut ActorEnv, record: &JournalRecord) {
    match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff => {
            let entry = env.state.entities.entry(record.entity).or_default();
            entry.components = record.payload.clone();
            entry.dirty = true;
            env.state.by_cell.insert(record.entity, record.cell);
        }
        RecordKind::Despawn => {
            env.state.entities.remove(&record.entity);
            env.state.by_cell.remove(&record.entity);
        }
        // Terrain, Rekey, CheckpointMark are recognized but not folded into the
        // entity map in this slice.
        RecordKind::TerrainDelta | RecordKind::Rekey | RecordKind::CheckpointMark => {}
    }
    env.state.ckpt_watermark = env.state.ckpt_watermark.max(record.lsn);
}

fn read_snapshot(env: &ActorEnv, cells: &[orrery_protocol::CellId]) -> SnapshotPage {
    let _ = cells;
    SnapshotPage {
        entities: env.state.entities.clone(),
    }
}

/// Capture the actor's full state for a checkpoint (§8, copy-on-update posture:
/// the snapshot is a clone taken inside the mailbox so serialization never
/// blocks appends).
fn checkpoint_snapshot(env: &ActorEnv) -> CheckpointSnapshot {
    CheckpointSnapshot {
        shard: env.state.shard,
        epoch: env.state.epoch,
        entities: env.state.entities.clone(),
        by_cell: env.state.by_cell.clone(),
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
    SplitSnapshot {
        shard: env.state.shard,
        epoch: env.state.epoch,
        children: out,
        by_cell,
        ckpt_watermark: env.state.ckpt_watermark,
    }
}

/// A handle to a running cell actor (sender + shared config).
#[derive(Debug)]
pub struct CellActorHandle {
    tx: mpsc::Sender<CellMsg>,
    shard: orrery_protocol::CellId,
    epoch: Epoch,
    /// The actor task; awaited on shutdown so the journal Arc is released.
    join: tokio::task::JoinHandle<()>,
}

impl CellActorHandle {
    /// Apply a diff, awaiting the durable LSN.
    pub async fn apply_diff(&self, record: JournalRecord) -> Result<Lsn, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ApplyDiff { record, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
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

    /// Ask the actor to shut down after draining its mailbox, then await the
    /// task so its `Arc<Journal>` (and the journal's file lock) is released.
    pub async fn shutdown(self) {
        let _ = self.tx.send(CellMsg::Shutdown).await;
        let _ = self.join.await;
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

    /// Restore the entity bag, per-entity cell map, and watermark into the
    /// actor (recovery, §3.4).
    pub async fn restore_entities(
        &self,
        entities: HashMap<PersistId, EntityRecord>,
        by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    ) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::Restore {
                entities,
                by_cell,
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

    /// The shard cell this actor owns.
    #[must_use]
    pub fn shard(&self) -> orrery_protocol::CellId {
        self.shard
    }
}

/// Spawn a cell actor for `shard` at `epoch`, sharing `journal`, starting empty.
pub fn spawn(
    shard: orrery_protocol::CellId,
    epoch: Epoch,
    journal: Arc<Journal>,
) -> CellActorHandle {
    spawn_preloaded(
        shard,
        epoch,
        journal,
        HashMap::new(),
        HashMap::new(),
        Lsn::new(0, 0),
    )
}

/// Spawn a cell actor for `shard` at `epoch`, sharing `journal`, preloaded with
/// recovered state (used by restart/recovery, §3.4).
pub fn spawn_preloaded(
    shard: orrery_protocol::CellId,
    epoch: Epoch,
    journal: Arc<Journal>,
    entities: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    ckpt_watermark: Lsn,
) -> CellActorHandle {
    let (tx, rx) = mpsc::channel(64);
    let mut env = ActorEnv {
        state: CellActorState {
            shard,
            epoch,
            entities,
            by_cell,
            ckpt_watermark,
        },
        journal,
    };
    let join = tokio::spawn(async move {
        actor_loop(&mut env, rx).await;
    });
    CellActorHandle {
        tx,
        shard,
        epoch,
        join,
    }
}
