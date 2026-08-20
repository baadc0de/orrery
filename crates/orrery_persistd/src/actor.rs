//! The single-writer cell actor (docs/08-persistence.md §3).
//!
//! A cell actor is a tokio task owning all hot state for one shard cell — the
//! persistence-side twin of the single-writer invariant (D2). All mutation
//! flows through its mailbox; readers get snapshots via message, never shared
//! mutable access. The actor applies a diff, appends to the journal, and only
//! then acks — the ack *is* the durability contract (§2.1).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use orrery_protocol::{
    CellId, ClaimKind, EntityRekey, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId,
    PersistId, RecordKind, Tick, ENTITY_REKEY_VERSION,
};

use crate::journal::{AppendHandle, Journal};
use crate::lease::{
    registrar_now_ms, ClaimResult, LeasePut, LeaseRegistrar, LeaseStore, ParkedLease,
};

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

/// A durable `world/` row an actor has moved off but not yet cleared.
///
/// The `world/` key carries the entity's **own cell** in its bytes
/// (`keyspace::world_key`), so every cell change writes the entity to a *new*
/// key and leaves the old one addressable. Nothing else clears it: the
/// checkpoint only ever `set`s at the current cell, and the tombstone GC pass
/// clears only the despawn marker's own key. A superseded row is therefore the
/// one way the durable tier can end up holding two live rows for one entity —
/// which a cold scan or a recovery seed collapses by Morton order, silently
/// serving whichever cell sorts higher.
///
/// The actor records the vacated `(entity, cell)` pair here the moment its
/// hot state moves, the checkpoint clears exactly those keys in the same pass
/// that writes the new ones, and the runtime prunes the pair only after that
/// checkpoint commits — the `prune_tombstones` post-commit template.
///
/// The set is **derived, not durable**, and deliberately so: every transition
/// that records one is a journal record, so a checkpoint that dies before its
/// clears commit leaves its watermark unadvanced, and the replay past that
/// watermark re-derives the same pairs. No new durable state has to be
/// consistent with the rows for the mechanism to converge.
pub type SupersededRow = (PersistId, CellId);

/// Move `entity`'s durable-row bookkeeping from wherever `by_cell` currently
/// places it to `now_at` — `None` when the entity leaves this actor entirely
/// (despawn is *not* that case: a despawn keeps a row, as a tombstone).
///
/// Call this **before** updating `by_cell`; it reads the outgoing cell there.
pub(crate) fn note_row_moved(
    superseded: &mut HashSet<SupersededRow>,
    by_cell: &HashMap<PersistId, CellId>,
    entity: PersistId,
    now_at: Option<CellId>,
) {
    if let Some(previous) = by_cell.get(&entity).copied() {
        if now_at != Some(previous) {
            superseded.insert((entity, previous));
        }
    }
    // Moving *into* a cell revives whatever row that key holds, so a pair
    // recorded by an earlier move away from it is no longer superseded.
    if let Some(cell) = now_at {
        superseded.remove(&(entity, cell));
    }
}

/// Drop `entity`'s despawn marker, recording the marker's own `world/` row as
/// superseded when it does not sit at `now_at`.
///
/// A re-spawn (or an arriving rekey) cancels the marker in hot state, but the
/// marker *row* is keyed by the cell the entity died in. If the entity comes
/// back somewhere else, nothing rewrites that key and nothing GCs it — the
/// actor has forgotten the tombstone whose deadline drives the GC pass — so it
/// is recorded here instead.
pub(crate) fn cancel_tombstone(
    superseded: &mut HashSet<SupersededRow>,
    tombstones: &mut HashMap<PersistId, Tombstone>,
    entity: PersistId,
    now_at: Option<CellId>,
) {
    if let Some(tomb) = tombstones.remove(&entity) {
        if now_at != Some(tomb.cell) {
            superseded.insert((entity, tomb.cell));
        }
    }
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

/// Result of atomically checking a lease fence and admitting a diff.
#[derive(Debug, Clone)]
pub enum FencedApply {
    /// The actor checked the exact live lease row and accepted the diff.
    Accepted(Arc<AppendHandle>),
    /// The actor rejected the presented pair and returns its current row for
    /// the gateway's lease-specific NACK.
    Rejected(Option<Lease>),
}

/// A rejection of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// The journal refused the append.
    JournalClosed,
    /// The durable lease row could not be committed.
    LeaseStore,
    /// No shard hosted on this node covers the cell the request named, so
    /// there is no actor here that could have answered it
    /// (docs/08-persistence.md §3.5).
    ///
    /// This used to be [`Reject::JournalClosed`], and conflating the two cost
    /// exactly what the distinction buys: a peer, an operator and a harness
    /// could not tell "this node is broken" from "you are talking to the
    /// wrong node", and the two call for opposite responses — one is a reason
    /// to stop writing, the other a reason to write somewhere else.
    ///
    /// `shard` is the cell the request named. This node cannot name a coarser
    /// owning shard for it: hosting no shard over it is the condition being
    /// reported. `epoch` is the shard-ownership epoch this runtime activated
    /// its own shards under (`FenceRow.epoch`), which is what §3.5's "reroute
    /// on epoch bump" is written against; [`Epoch::new(0)`](Epoch) means this
    /// level of the routing stack has no epoch of its own to report.
    ///
    /// Deliberately carries **no redirect target**: which node should have
    /// been asked is ADR-0026's question, and that record is still Proposed.
    /// The wire form ([`orrery_protocol::DenyReason::WrongOwner`]) keeps a
    /// shaped hole for it; this one does not need it until something can fill
    /// it, because it never crosses a process boundary.
    WrongOwner {
        /// The grid the refused request named.
        grid: GridId,
        /// The cell the refused request named.
        shard: CellId,
        /// This node's shard-ownership epoch, or `0` when unknown here.
        epoch: Epoch,
    },
}

/// Why a server committed-rekey record was rejected before actor transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyError {
    /// The journal envelope is not a rekey record.
    WrongRecordKind,
    /// Payload bytes or their integrity checksum are invalid.
    MalformedPayload,
    /// Payload schema version is unsupported.
    VersionMismatch,
    /// Envelope identity or source location disagrees with the payload.
    SourceMismatch,
    /// Source and destination identify the same durable row.
    SelfMove,
    /// A zero fence cannot authorize migration.
    MissingExpectedFence,
    /// Recovery cannot proceed without the source component image.
    MissingSourceRecord,
    /// The source actor does not contain the committed entity location.
    SourceEntityMissing,
    /// The durable source image does not match the actor-owned component bag.
    SourceRecordMismatch,
    /// The actor-owned registrar row does not match the expected fencing token.
    FenceMismatch,
    /// Source or destination actor routing failed.
    ActorUnavailable,
    /// Durable journal append or flush failed.
    Journal,
    /// Durable lease migration failed.
    LeaseStore,
    /// Durable lease migration rejected the expected source or fence.
    LeaseMigrationRejected,
    /// Destination already contains a different copy of the entity.
    DestinationConflict,
}

impl core::fmt::Display for RekeyError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl core::error::Error for RekeyError {}

/// Decode and validate a server-owned committed-rekey journal envelope.
pub(crate) fn decode_entity_rekey(record: &JournalRecord) -> Result<EntityRekey, RekeyError> {
    if record.kind != RecordKind::Rekey {
        return Err(RekeyError::WrongRecordKind);
    }
    if crate::payload_crc(&record.payload) != record.crc {
        return Err(RekeyError::MalformedPayload);
    }
    let rekey: EntityRekey =
        postcard::from_bytes(&record.payload).map_err(|_| RekeyError::MalformedPayload)?;
    if rekey.version != ENTITY_REKEY_VERSION {
        return Err(RekeyError::VersionMismatch);
    }
    if rekey.entity != record.entity
        || rekey.source_grid != record.grid
        || rekey.source_cell != record.cell
    {
        return Err(RekeyError::SourceMismatch);
    }
    if (rekey.source_grid, rekey.source_cell) == (rekey.destination_grid, rekey.destination_cell) {
        return Err(RekeyError::SelfMove);
    }
    if rekey.expected_lease_id == LeaseId(0) {
        return Err(RekeyError::MissingExpectedFence);
    }
    if rekey.source_record.is_empty() {
        return Err(RekeyError::MissingSourceRecord);
    }
    Ok(rekey)
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
    /// `world/` rows this actor has vacated and the checkpoint must clear
    /// ([`SupersededRow`]).
    pub superseded: HashSet<SupersededRow>,
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
    /// Each partition's superseded rows, by the child whose subtree holds the
    /// vacated key. A split spawns children from the parent's in-memory
    /// partition and never touches the parent's durable rows, so a pending
    /// clear that did not travel with the partition would be laundered into a
    /// permanent ghost.
    pub superseded: HashMap<orrery_protocol::CellId, HashSet<SupersededRow>>,
    /// The journal LSN covered by the last checkpoint.
    pub ckpt_watermark: Lsn,
}

#[derive(Debug, Clone)]
pub(crate) struct RekeyTransfer {
    entity: PersistId,
    source_cell: CellId,
    destination_cell: CellId,
    record: EntityRecord,
    lease: Lease,
}

/// A message to a cell actor's mailbox (§3.1).
#[derive(Debug)]
pub(crate) enum CellMsg {
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
    /// Check a fencing pair and enqueue a diff in the same mailbox turn.
    ///
    /// This closes the validate-then-append race: a competing claim cannot
    /// change the row between the comparison and journal admission.
    ApplyFencedDiff {
        /// The record to apply after successful validation.
        record: JournalRecord,
        /// Authenticated holder presenting the fence.
        holder: NodeId,
        /// Presented registrar token.
        lease_id: LeaseId,
        /// Presented sequence pair.
        authority_seq: orrery_protocol::SeqPair,
        /// Current registrar-monotonic time.
        now_ms: u64,
        /// Reply channel for the admitted handle or current rejected row.
        reply: oneshot::Sender<Result<FencedApply, Reject>>,
    },
    /// Read a snapshot of the given cells.
    ReadSnapshot {
        /// The cells to read.
        cells: Vec<orrery_protocol::CellId>,
        /// Reply channel for the page.
        reply: oneshot::Sender<SnapshotPage>,
    },
    /// Resolve an entity's committed cell from the actor-owned hot-state index.
    LocateEntity {
        /// Persistent entity to locate.
        entity: PersistId,
        /// Reply channel for the committed cell, if the entity exists.
        reply: oneshot::Sender<Option<CellId>>,
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
        /// Superseded `world/` rows carried by the checkpoint — the durable
        /// tier reports the duplicates its own scan found, so a restore
        /// adopts the clean-up an older writer never performed.
        superseded: HashSet<SupersededRow>,
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
    /// Drop the bookkeeping a just-committed checkpoint made durable: the
    /// tombstones whose GC deadline has passed (D11 §6, checkpoint GC pass)
    /// and the superseded rows it cleared. Both were acted on by the
    /// checkpoint that just committed; dropping them here stops the next
    /// checkpoint from rewriting or re-clearing them.
    PruneCheckpointed {
        /// Wall-clock "now", as unix milliseconds.
        now_ms: u64,
        /// Exactly the superseded rows the committed checkpoint carried —
        /// never the live set, which may have grown since the snapshot.
        superseded: HashSet<SupersededRow>,
        /// Reply channel.
        reply: oneshot::Sender<()>,
    },
    /// Serialized registrar claim for one entity.
    ClaimLease {
        entity: PersistId,
        cell: CellId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
        reply: oneshot::Sender<Result<ClaimResult, Reject>>,
    },
    /// Renew one current session lease.
    HeartbeatLease {
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
        reply: oneshot::Sender<Result<Option<Lease>, Reject>>,
    },
    /// Renew a batch of one session's leases in a single mailbox turn.
    ///
    /// One peer heartbeats every lease it holds every 2.5 s, and the rows all
    /// belong to this actor: sending them one at a time makes a holder of N
    /// entities cost N turns through a bounded mailbox for no arbitration
    /// benefit, since each renewal is an independent check against its own
    /// row. The reply is positional — one entry per requested pair, `None`
    /// where the pair did not renew — so the ack still names every invalid
    /// pair individually and a holder stops writing that entity promptly.
    HeartbeatLeases {
        renew: Vec<(PersistId, LeaseId)>,
        holder: NodeId,
        now_ms: u64,
        reply: oneshot::Sender<Result<Vec<Option<Lease>>, Reject>>,
    },
    /// Return the current row while checking its fencing token.
    ValidateLease {
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
        reply: oneshot::Sender<Result<Option<Lease>, Reject>>,
    },
    /// Park a known lease on disconnect or expiry.
    ParkLease {
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        reply: oneshot::Sender<Result<Option<Lease>, Reject>>,
    },
    /// Park all silent holders in this actor.
    SweepLeases {
        now_ms: u64,
        reply: oneshot::Sender<Result<Vec<ParkedLease>, Reject>>,
    },
    /// Read one registrar row and the entity's durable uplink cursor without
    /// mutating either.
    InspectLease {
        entity: PersistId,
        reply: oneshot::Sender<(Option<Lease>, Option<CellId>, Option<Lsn>)>,
    },
    /// Validate and reserve a source entity, then append its committed rekey.
    PrepareRekey {
        rekey: EntityRekey,
        record: JournalRecord,
        reply: oneshot::Sender<Result<(RekeyTransfer, Arc<AppendHandle>), RekeyError>>,
    },
    /// Release a failed source reservation without changing actor state.
    AbortRekey {
        entity: PersistId,
        lease_id: LeaseId,
    },
    /// Install a durably migrated entity and its unchanged fencing row.
    InstallRekey {
        transfer: RekeyTransfer,
        reply: oneshot::Sender<Result<(), RekeyError>>,
    },
    /// Retire the source copy after destination installation completes.
    RetireRekey {
        entity: PersistId,
        lease_id: LeaseId,
        reply: oneshot::Sender<Result<(), RekeyError>>,
    },
    /// Finish a rekey whose source and destination share one actor mailbox.
    CompleteLocalRekey {
        transfer: RekeyTransfer,
        reply: oneshot::Sender<Result<(), RekeyError>>,
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
    /// `world/` rows this actor has vacated and the next checkpoint must
    /// clear ([`SupersededRow`]).
    pub superseded: HashSet<SupersededRow>,
    /// The journal LSN covered by the last checkpoint.
    pub ckpt_watermark: Lsn,
    /// Actor-owned lease registrar; no gateway path mutates it directly.
    pub leases: LeaseRegistrar,
    /// Durable entity-location index mirrored by the lease store.
    pub lease_cells: HashMap<PersistId, CellId>,
    /// Highest journal position this actor has folded for each entity.
    ///
    /// A divesting holder names the journal position it last saw acked
    /// (`Divest.cursor`, D7 §5). Comparing it against this watermark is what
    /// lets the registrar refuse to hand a successor state the predecessor
    /// never actually committed.
    pub entity_lsn: HashMap<PersistId, Lsn>,
    pending_rekeys: HashMap<PersistId, LeaseId>,
}

/// The actor task's environment: its state plus the shared journal.
struct ActorEnv {
    state: CellActorState,
    journal: Arc<Journal>,
    lease_store: Arc<dyn LeaseStore>,
}

/// Run the actor's mailbox loop until [`CellMsg::Shutdown`].
async fn actor_loop(env: &mut ActorEnv, mut rx: mpsc::Receiver<CellMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            CellMsg::ApplyDiff { record, reply } => {
                if env.state.pending_rekeys.contains_key(&record.entity) {
                    let _ = reply.send(Err(Reject::JournalClosed));
                } else {
                    apply_diff(env, record, reply);
                }
            }
            CellMsg::ApplyFencedDiff {
                record,
                holder,
                lease_id,
                authority_seq,
                now_ms,
                reply,
            } => {
                let current = env.state.leases.current(record.entity);
                let admitted = !env.state.pending_rekeys.contains_key(&record.entity)
                    && env.state.by_cell.get(&record.entity) == Some(&record.cell)
                    && current.as_ref().is_some_and(|row| {
                        row.holder == Some(holder)
                            && row.lease_id == lease_id
                            && row.seq == authority_seq
                            && row.expires_at > now_ms
                    });
                let result = if admitted {
                    start_diff(env, record).map(FencedApply::Accepted)
                } else {
                    Ok(FencedApply::Rejected(current))
                };
                let _ = reply.send(result);
            }
            CellMsg::ReadSnapshot { cells, reply } => {
                let _ = reply.send(read_snapshot(env, &cells));
            }
            CellMsg::LocateEntity { entity, reply } => {
                let _ = reply.send(env.state.by_cell.get(&entity).copied());
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
                superseded,
                watermark,
                reply,
            } => {
                env.state.entities = entities;
                env.state.by_cell = by_cell;
                env.state.tombstones = tombstones;
                env.state.superseded = superseded;
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
            CellMsg::PruneCheckpointed {
                now_ms,
                superseded,
                reply,
            } => {
                env.state
                    .tombstones
                    .retain(|_, t| t.gc_deadline_ms > now_ms);
                for row in &superseded {
                    env.state.superseded.remove(row);
                }
                let _ = reply.send(());
            }
            CellMsg::ClaimLease {
                entity,
                cell,
                holder,
                kind,
                now_ms,
                reply,
            } => {
                if env.state.pending_rekeys.contains_key(&entity) {
                    let _ = reply.send(Ok(ClaimResult::Denied(
                        orrery_protocol::DenyReason::NotEligible,
                    )));
                } else {
                    let _ = reply.send(claim_lease(env, entity, cell, holder, kind, now_ms).await);
                }
            }
            CellMsg::HeartbeatLease {
                entity,
                holder,
                lease_id,
                now_ms,
                reply,
            } => {
                let _ = reply.send(heartbeat_lease(env, entity, holder, lease_id, now_ms));
            }
            CellMsg::HeartbeatLeases {
                renew,
                holder,
                now_ms,
                reply,
            } => {
                let _ = reply.send(heartbeat_leases(env, &renew, holder, now_ms));
            }
            CellMsg::ValidateLease {
                entity,
                holder,
                lease_id,
                now_ms,
                reply,
            } => {
                let row = env.state.leases.current(entity);
                let _ = (holder, lease_id, now_ms);
                // Return the row on both success and failure. The gateway
                // compares it with the presented fencing pair and includes it
                // in a lease-specific NACK when it does not match.
                let _ = reply.send(Ok(row));
            }
            CellMsg::ParkLease {
                entity,
                holder,
                lease_id,
                reply,
            } => {
                if env.state.pending_rekeys.contains_key(&entity) {
                    let _ = reply.send(Ok(env.state.leases.current(entity)));
                } else {
                    let _ = reply.send(park_lease(env, entity, holder, lease_id).await);
                }
            }
            CellMsg::InspectLease { entity, reply } => {
                let _ = reply.send((
                    env.state.leases.current(entity),
                    env.state.lease_cells.get(&entity).copied(),
                    env.state.entity_lsn.get(&entity).copied(),
                ));
            }
            CellMsg::SweepLeases { now_ms, reply } => {
                if env.state.pending_rekeys.is_empty() {
                    let _ = reply.send(sweep_leases(env, now_ms).await);
                } else {
                    let _ = reply.send(Ok(Vec::new()));
                }
            }
            CellMsg::PrepareRekey {
                rekey,
                record,
                reply,
            } => {
                let _ = reply.send(prepare_rekey(env, &rekey, record));
            }
            CellMsg::AbortRekey { entity, lease_id } => {
                if env.state.pending_rekeys.get(&entity) == Some(&lease_id) {
                    env.state.pending_rekeys.remove(&entity);
                }
            }
            CellMsg::InstallRekey { transfer, reply } => {
                let _ = reply.send(install_rekey(env, transfer));
            }
            CellMsg::RetireRekey {
                entity,
                lease_id,
                reply,
            } => {
                let _ = reply.send(retire_rekey(env, entity, lease_id));
            }
            CellMsg::CompleteLocalRekey { transfer, reply } => {
                let _ = reply.send(complete_local_rekey(env, transfer));
            }
            CellMsg::Shutdown => break,
        }
    }
}

fn prepare_rekey(
    env: &mut ActorEnv,
    rekey: &EntityRekey,
    mut record: JournalRecord,
) -> Result<(RekeyTransfer, Arc<AppendHandle>), RekeyError> {
    if env.state.grid != rekey.source_grid
        || env.state.by_cell.get(&rekey.entity) != Some(&rekey.source_cell)
    {
        return Err(RekeyError::SourceEntityMissing);
    }
    let entity_record = env
        .state
        .entities
        .get(&rekey.entity)
        .cloned()
        .ok_or(RekeyError::SourceEntityMissing)?;
    if entity_record.components != rekey.source_record {
        return Err(RekeyError::SourceRecordMismatch);
    }
    let lease = env
        .state
        .leases
        .current(rekey.entity)
        .filter(|row| row.lease_id == rekey.expected_lease_id)
        .ok_or(RekeyError::FenceMismatch)?;
    if env
        .state
        .pending_rekeys
        .insert(rekey.entity, rekey.expected_lease_id)
        .is_some()
    {
        return Err(RekeyError::FenceMismatch);
    }
    record.epoch = env.state.epoch;
    let handle = match env.journal.append(record) {
        Ok(handle) => handle,
        Err(_) => {
            env.state.pending_rekeys.remove(&rekey.entity);
            return Err(RekeyError::Journal);
        }
    };
    env.state.ckpt_watermark = env.state.ckpt_watermark.max(handle.lsn());
    Ok((
        RekeyTransfer {
            entity: rekey.entity,
            source_cell: rekey.source_cell,
            destination_cell: rekey.destination_cell,
            record: entity_record,
            lease,
        },
        handle,
    ))
}

/// Invariant J's four enforcement points, made a **process** failure rather
/// than a code comment.
///
/// Returns the cell it checked, so the assertion and the write are one
/// expression: a later edit cannot move the row without moving the check.
///
/// **(J)** if an actor's registrar holds a row for entity `e`, then
/// `LeaseStore::locate(e)` names a cell inside that actor's shard subtree (or
/// is `None`). `lease_cells` is the actor's mirror of exactly that durable
/// key, written in the same turn as the durable write, so asserting the row
/// goes in under this actor's own shard is asserting J at the moment it could
/// first be broken.
///
/// J is what lets `CellRuntime::apply_fenced` route by `record.cell` instead
/// of reading FoundationDB per diff: an actor that *accepts* holds a row, so
/// by J it is the actor the locate would have named, so the accept set is
/// unchanged. If one of these fires, that argument is false and the change
/// that rests on it has to come out — see docs/08-persistence.md §2.
///
/// A real `assert!`, not a `debug_assert!`. It was the latter, which compiles
/// out of exactly the configuration the capacity sweep and production run, so
/// the documented "four enforcement sites" were four enforcement sites in the
/// test suite and none at all where it matters. The cost of promoting it is
/// nil: none of the four callers is on the bulk write path — a lease grant, a
/// rekey install, its intra-shard twin, and one row per entry at actor-spawn
/// recovery, so **zero** calls per fenced diff — and `is_prefix_of` is a
/// range containment on a `u64`, measured at 0.98 ns per call in release.
/// Panicking is the correct response and not a severity judgement made
/// lightly: past this point the actor would be admitting fenced writes
/// against a row whose durable location it does not own, which is silent
/// divergence of the accept set, and there is no local recovery from it —
/// the actor's supervisor failing the shard closed is strictly safer than
/// serving it.
fn checked_row_cell(shard: CellId, cell: CellId, site: &str) -> CellId {
    assert!(
        shard.is_prefix_of(cell),
        "invariant J: {site} installed a registrar row at {cell:?}, outside shard {shard:?}"
    );
    cell
}

fn install_rekey(env: &mut ActorEnv, transfer: RekeyTransfer) -> Result<(), RekeyError> {
    if let Some(existing) = env.state.entities.get(&transfer.entity) {
        let idempotent = existing == &transfer.record
            && env.state.by_cell.get(&transfer.entity) == Some(&transfer.destination_cell)
            && env
                .state
                .leases
                .current(transfer.entity)
                .is_some_and(|row| row == transfer.lease);
        return if idempotent {
            Ok(())
        } else {
            Err(RekeyError::DestinationConflict)
        };
    }
    env.state.entities.insert(transfer.entity, transfer.record);
    note_row_moved(
        &mut env.state.superseded,
        &env.state.by_cell,
        transfer.entity,
        Some(transfer.destination_cell),
    );
    env.state
        .by_cell
        .insert(transfer.entity, transfer.destination_cell);
    cancel_tombstone(
        &mut env.state.superseded,
        &mut env.state.tombstones,
        transfer.entity,
        Some(transfer.destination_cell),
    );
    env.state.leases.restore(transfer.lease);
    env.state.lease_cells.insert(
        transfer.entity,
        checked_row_cell(env.state.shard, transfer.destination_cell, "install_rekey"),
    );
    Ok(())
}

fn retire_rekey(
    env: &mut ActorEnv,
    entity: PersistId,
    lease_id: LeaseId,
) -> Result<(), RekeyError> {
    if env.state.pending_rekeys.get(&entity) != Some(&lease_id) {
        return Err(RekeyError::FenceMismatch);
    }
    env.state.pending_rekeys.remove(&entity);
    env.state.entities.remove(&entity);
    // The source side of a cross-shard move: no tombstone (the entity lives
    // on in the destination shard), so the vacated key is only ever cleared
    // because it is recorded here.
    note_row_moved(&mut env.state.superseded, &env.state.by_cell, entity, None);
    env.state.by_cell.remove(&entity);
    cancel_tombstone(
        &mut env.state.superseded,
        &mut env.state.tombstones,
        entity,
        None,
    );
    env.state.leases.remove(entity);
    env.state.lease_cells.remove(&entity);
    Ok(())
}

fn complete_local_rekey(env: &mut ActorEnv, transfer: RekeyTransfer) -> Result<(), RekeyError> {
    if env.state.pending_rekeys.get(&transfer.entity) != Some(&transfer.lease.lease_id)
        || env.state.by_cell.get(&transfer.entity) != Some(&transfer.source_cell)
        || env.state.entities.get(&transfer.entity) != Some(&transfer.record)
    {
        return Err(RekeyError::DestinationConflict);
    }
    env.state.pending_rekeys.remove(&transfer.entity);
    // An intra-shard move keeps one actor and one row, but the row's *key*
    // still changes, so the source key is superseded exactly as it is in the
    // cross-shard case.
    note_row_moved(
        &mut env.state.superseded,
        &env.state.by_cell,
        transfer.entity,
        Some(transfer.destination_cell),
    );
    env.state
        .by_cell
        .insert(transfer.entity, transfer.destination_cell);
    env.state.lease_cells.insert(
        transfer.entity,
        checked_row_cell(
            env.state.shard,
            transfer.destination_cell,
            "complete_local_rekey",
        ),
    );
    env.state.leases.restore(transfer.lease);
    Ok(())
}

async fn claim_lease(
    env: &mut ActorEnv,
    entity: PersistId,
    cell: CellId,
    holder: NodeId,
    kind: ClaimKind,
    now_ms: u64,
) -> Result<ClaimResult, Reject> {
    if let Some(committed) = env.state.lease_cells.get(&entity) {
        if *committed != cell {
            return Ok(ClaimResult::Denied(
                orrery_protocol::DenyReason::NotEligible,
            ));
        }
    }
    let mut next = env.state.leases.clone();
    let result = next.claim(entity, holder, kind, now_ms);
    if let ClaimResult::Granted(row) = &result {
        match env
            .lease_store
            .put(env.state.grid, cell, row)
            .await
            .map_err(|_| Reject::LeaseStore)?
        {
            LeasePut::Stored => {}
            LeasePut::LocationConflict(_) => {
                return Ok(ClaimResult::Denied(
                    orrery_protocol::DenyReason::NotEligible,
                ));
            }
        }
        env.state.lease_cells.insert(
            entity,
            checked_row_cell(env.state.shard, cell, "claim_lease"),
        );
    }
    env.state.leases = next;
    Ok(result)
}

/// Renew one pair against this actor's registrar.
///
/// In place, not against a clone. Every other registrar mutation here writes
/// the durable tier partway through and needs a copy to abandon when that
/// write fails; a heartbeat has no durable half at all. See
/// [`heartbeat_leases`] for the full argument and what it costs.
fn heartbeat_lease(
    env: &mut ActorEnv,
    entity: PersistId,
    holder: NodeId,
    lease_id: LeaseId,
    now_ms: u64,
) -> Result<Option<Lease>, Reject> {
    env.state.leases.heartbeat(entity, holder, lease_id, now_ms);
    Ok(env.state.leases.current(entity))
}

/// Renew every pair in one batch against this actor's registrar.
///
/// **The registrar is not copied.** Every other mutation in this file builds a
/// `next` and installs it only after the durable write succeeds, because a
/// half-applied claim or sweep must be abandonable. A heartbeat has nothing to
/// abandon: it writes no journal record and no `LeaseStore` row, it advances no
/// sequence and mints no token, and `LeaseRegistrar::heartbeat` either sets one
/// row's `expires_at` or leaves the registrar untouched. There is no failure
/// between the first pair and the last for a copy to unwind to.
///
/// The copy was the whole cost of the path. A renewal batch is grouped by the
/// actor that owns each entity (`cluster::group_by_actor`), and at the P2
/// operating point a session's 40 leases sit in 40 different shards — so the
/// batch is 40 groups of one, each turn cloning that shard's entire registrar
/// to renew a single row. Both hash maps are copied whole, so the cost is the
/// *shard's* population, not the batch's: ~3 100 rows copied per heartbeat to
/// update 40 `expires_at` fields, and it grows with the world while the
/// renewal that pays it does not. `benches/lease_renewal.rs` isolates the
/// copy by holding everything else fixed and growing the registrar: on one
/// shard of 10 000 rows a batch costs 0.072 ms with the copy and 0.018 ms
/// without it.
///
/// Semantics are unchanged, not merely similar: the old code mutated `next`
/// in the same loop, so each pair already saw the preceding pairs' writes, and
/// nothing between the clone and the install could observe `env.state.leases`
/// — this function is synchronous and the actor is single-writer.
fn heartbeat_leases(
    env: &mut ActorEnv,
    renew: &[(PersistId, LeaseId)],
    holder: NodeId,
    now_ms: u64,
) -> Result<Vec<Option<Lease>>, Reject> {
    let mut rows = Vec::with_capacity(renew.len());
    for (entity, lease_id) in renew {
        env.state
            .leases
            .heartbeat(*entity, holder, *lease_id, now_ms);
        rows.push(env.state.leases.current(*entity));
    }
    Ok(rows)
}

fn with_fresh_recovery_ttl(mut row: Lease, now_ms: u64) -> Lease {
    // The registrar clock is process-local, so a durable instant minted in a
    // previous process means nothing here — which is why a held row is
    // restored with a full fresh TTL. A *parked* row carries no TTL; when it
    // is a crashed strong owner's, `expires_at` is its reservation deadline,
    // and it is re-armed for the same reason and in the same direction: the
    // owner gets its full window measured from this process's clock.
    row.expires_at = if row.holder.is_some() {
        now_ms.saturating_add(crate::lease::LEASE_TTL_MS)
    } else if row.flags.contains(orrery_protocol::LeaseFlags::STRONG_HELD) {
        now_ms.saturating_add(crate::lease::STRONG_PARK_GRACE_MS)
    } else {
        0
    };
    row
}

async fn park_lease(
    env: &mut ActorEnv,
    entity: PersistId,
    holder: NodeId,
    lease_id: LeaseId,
) -> Result<Option<Lease>, Reject> {
    let Some(current) = env.state.leases.current(entity) else {
        return Ok(None);
    };
    if current.holder != Some(holder) || current.lease_id != lease_id {
        return Ok(Some(current));
    }
    let mut next = env.state.leases.clone();
    // Parking is a registrar-clock event like expiry, and this path has no
    // caller-supplied instant: a disconnect is observed by the gateway, not
    // scheduled by it.
    let row = next
        .disconnect(holder, crate::lease::registrar_now_ms())
        .into_iter()
        .find(|row| row.entity == entity)
        .expect("checked holder");
    let cell = *env
        .state
        .lease_cells
        .get(&entity)
        .unwrap_or(&env.state.shard);
    if !matches!(
        env.lease_store
            .put(env.state.grid, cell, &row)
            .await
            .map_err(|_| Reject::LeaseStore)?,
        LeasePut::Stored
    ) {
        return Err(Reject::LeaseStore);
    }
    env.state.leases = next;
    Ok(Some(row))
}

/// Park this actor's registrar rows whose monotonic TTL passed.
///
/// The copy below is load-bearing, unlike the one `heartbeat_leases` no longer
/// makes: a sweep writes a durable row per parked lease and abandons the whole
/// sweep if one fails, so it needs a state to abandon *to*. What it does not
/// need is to make that copy in order to discover there is nothing to sweep,
/// which is the steady state — holders renew every 3 s against a 10 s TTL, and
/// the gateway sweeps every second, so the overwhelming majority of ticks park
/// nothing. Copying both of the registrar's maps whole costs the *shard's*
/// population, so the unguarded version paid for the whole world once per
/// actor per second to answer "no".
async fn sweep_leases(env: &mut ActorEnv, now_ms: u64) -> Result<Vec<ParkedLease>, Reject> {
    if !env.state.leases.has_expired(now_ms) {
        return Ok(Vec::new());
    }
    let mut next = env.state.leases.clone();
    let expired = next.sweep_expired(now_ms);
    let mut parked = Vec::with_capacity(expired.len());
    for row in expired {
        let cell = *env
            .state
            .lease_cells
            .get(&row.lease.entity)
            .unwrap_or(&env.state.shard);
        if !matches!(
            env.lease_store
                .put(env.state.grid, cell, &row.lease)
                .await
                .map_err(|_| Reject::LeaseStore)?,
            LeasePut::Stored
        ) {
            return Err(Reject::LeaseStore);
        }
        parked.push(ParkedLease {
            grid: env.state.grid,
            cell,
            previous_holder: row.previous_holder,
            previous_lease_id: row.previous_lease_id,
            lease: row.lease,
            reason: orrery_protocol::ExpireReason::Timeout,
        });
    }
    env.state.leases = next;
    Ok(parked)
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
fn start_diff(env: &mut ActorEnv, mut record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
    // The actor is the epoch authority: the record is durably stamped with
    // the shard-ownership epoch here (§3.4), overwriting the gateway's
    // placeholder `Epoch::new(0)` (D11 §2.1: the server assigns epoch/lsn).
    // The stamping must precede the append so the journaled bytes carry it.
    record.epoch = env.state.epoch;
    let handle = match env.journal.append(record.clone()) {
        Ok(handle) => handle,
        Err(_) => return Err(Reject::JournalClosed),
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

    Ok(handle)
}

fn apply_diff(
    env: &mut ActorEnv,
    record: JournalRecord,
    reply: oneshot::Sender<Result<Arc<AppendHandle>, Reject>>,
) {
    let _ = reply.send(start_diff(env, record));
}

/// Fold a journal record into in-memory state (no durability work).
fn fold(env: &mut ActorEnv, record: &JournalRecord, now_ms: u64) {
    match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff => {
            let entry = env.state.entities.entry(record.entity).or_default();
            entry.components = record.payload.clone();
            entry.dirty = true;
            // An ordinary diff at a new cell moves the durable row's key as
            // surely as a rekey does: the vacated key must be cleared, or the
            // checkpoint leaves a second live row behind.
            note_row_moved(
                &mut env.state.superseded,
                &env.state.by_cell,
                record.entity,
                Some(record.cell),
            );
            env.state.by_cell.insert(record.entity, record.cell);
            // A re-spawn (id reuse across a despawn) cancels the marker.
            cancel_tombstone(
                &mut env.state.superseded,
                &mut env.state.tombstones,
                record.entity,
                Some(record.cell),
            );
        }
        RecordKind::Despawn => {
            env.state.entities.remove(&record.entity);
            // The marker is keyed by the despawn record's cell; if the live
            // row sits at a different one, the marker does not overwrite it
            // and the stale row would outlive the entity.
            note_row_moved(
                &mut env.state.superseded,
                &env.state.by_cell,
                record.entity,
                Some(record.cell),
            );
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
        RecordKind::Rekey => {
            if let Ok(rekey) = decode_entity_rekey(record) {
                if env.state.grid == rekey.source_grid
                    && env.state.by_cell.get(&rekey.entity) == Some(&rekey.source_cell)
                {
                    env.state.entities.remove(&rekey.entity);
                    // Deliberately no tombstone here (the entity is alive
                    // elsewhere), so the source row needs the superseded
                    // record instead — otherwise nothing ever clears it.
                    note_row_moved(
                        &mut env.state.superseded,
                        &env.state.by_cell,
                        rekey.entity,
                        None,
                    );
                    env.state.by_cell.remove(&rekey.entity);
                    cancel_tombstone(
                        &mut env.state.superseded,
                        &mut env.state.tombstones,
                        rekey.entity,
                        None,
                    );
                    env.state.leases.remove(rekey.entity);
                    env.state.lease_cells.remove(&rekey.entity);
                }
                if env.state.grid == rekey.destination_grid
                    && env.state.shard.is_prefix_of(rekey.destination_cell)
                {
                    env.state.entities.insert(
                        rekey.entity,
                        EntityRecord {
                            components: rekey.source_record,
                            dirty: true,
                        },
                    );
                    note_row_moved(
                        &mut env.state.superseded,
                        &env.state.by_cell,
                        rekey.entity,
                        Some(rekey.destination_cell),
                    );
                    env.state
                        .by_cell
                        .insert(rekey.entity, rekey.destination_cell);
                    cancel_tombstone(
                        &mut env.state.superseded,
                        &mut env.state.tombstones,
                        rekey.entity,
                        Some(rekey.destination_cell),
                    );
                }
            }
        }
        RecordKind::TerrainDelta | RecordKind::CheckpointMark => {}
    }
    match record.kind {
        RecordKind::Spawn | RecordKind::ComponentDiff | RecordKind::Despawn => {
            let watermark = env
                .state
                .entity_lsn
                .entry(record.entity)
                .or_insert(Lsn::new(0, 0));
            *watermark = (*watermark).max(record.lsn);
        }
        // A rekey moves the entity to another actor, which folds its own
        // records from there; keeping the source watermark would let a stale
        // cursor comparison outlive the entity's residence here.
        RecordKind::Rekey => {
            env.state.entity_lsn.remove(&record.entity);
        }
        RecordKind::TerrainDelta | RecordKind::CheckpointMark => {}
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
        superseded: env.state.superseded.clone(),
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
    let mut superseded: HashMap<_, HashSet<_>> =
        children.iter().map(|&c| (c, HashSet::new())).collect();
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
    for &(entity, cell) in &env.state.superseded {
        // The vacated key lives under exactly one child's subtree; that child
        // inherits the pending clear, because after the split only it can
        // fence a write to that key.
        let child = children
            .iter()
            .find(|c| c.is_prefix_of(cell))
            .copied()
            .unwrap_or(env.state.shard);
        superseded.entry(child).or_default().insert((entity, cell));
    }
    SplitSnapshot {
        shard: env.state.shard,
        grid: env.state.grid,
        epoch: env.state.epoch,
        children: out,
        by_cell,
        tombstones,
        superseded,
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
    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }
    /// Resolve an entity's committed cell from this actor's hot-state index.
    pub async fn committed_entity_cell(&self, entity: PersistId) -> Result<Option<CellId>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::LocateEntity { entity, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }

    pub(crate) async fn prepare_rekey(
        &self,
        rekey: EntityRekey,
        record: JournalRecord,
    ) -> Result<(RekeyTransfer, Arc<AppendHandle>), RekeyError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::PrepareRekey {
                rekey,
                record,
                reply,
            })
            .await
            .map_err(|_| RekeyError::ActorUnavailable)?;
        rx.await.map_err(|_| RekeyError::ActorUnavailable)?
    }

    pub(crate) async fn abort_rekey(&self, entity: PersistId, lease_id: LeaseId) {
        let _ = self.tx.send(CellMsg::AbortRekey { entity, lease_id }).await;
    }

    pub(crate) async fn install_rekey(&self, transfer: RekeyTransfer) -> Result<(), RekeyError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::InstallRekey { transfer, reply })
            .await
            .map_err(|_| RekeyError::ActorUnavailable)?;
        rx.await.map_err(|_| RekeyError::ActorUnavailable)?
    }

    pub(crate) async fn retire_rekey(
        &self,
        entity: PersistId,
        lease_id: LeaseId,
    ) -> Result<(), RekeyError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::RetireRekey {
                entity,
                lease_id,
                reply,
            })
            .await
            .map_err(|_| RekeyError::ActorUnavailable)?;
        rx.await.map_err(|_| RekeyError::ActorUnavailable)?
    }

    pub(crate) async fn complete_local_rekey(
        &self,
        transfer: RekeyTransfer,
    ) -> Result<(), RekeyError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::CompleteLocalRekey { transfer, reply })
            .await
            .map_err(|_| RekeyError::ActorUnavailable)?;
        rx.await.map_err(|_| RekeyError::ActorUnavailable)?
    }

    /// Execute a serialized claim in this actor.
    pub async fn claim_lease(
        &self,
        entity: PersistId,
        cell: CellId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<ClaimResult, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ClaimLease {
                entity,
                cell,
                holder,
                kind,
                now_ms,
                reply,
            })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }
    /// Renew a lease and return its current row.
    pub async fn heartbeat_lease(
        &self,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::HeartbeatLease {
                entity,
                holder,
                lease_id,
                now_ms,
                reply,
            })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }
    /// Renew a batch of this session's leases in one mailbox turn.
    ///
    /// Returns one entry per requested pair, in request order.
    pub async fn heartbeat_leases(
        &self,
        renew: Vec<(PersistId, LeaseId)>,
        holder: NodeId,
        now_ms: u64,
    ) -> Result<Vec<Option<Lease>>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::HeartbeatLeases {
                renew,
                holder,
                now_ms,
                reply,
            })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }
    /// Check a fencing token, returning the row only if it admits the write.
    pub async fn validate_lease(
        &self,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<Lease>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ValidateLease {
                entity,
                holder,
                lease_id,
                now_ms,
                reply,
            })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }
    /// Park a lease only when the holder and token are still current.
    pub async fn park_lease(
        &self,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ParkLease {
                entity,
                holder,
                lease_id,
                reply,
            })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }
    /// Read one registrar row, its committed cell, and its uplink watermark.
    pub async fn inspect_lease(
        &self,
        entity: PersistId,
    ) -> Result<(Option<Lease>, Option<CellId>, Option<Lsn>), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::InspectLease { entity, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)
    }
    /// Park leases whose registrar TTL elapsed.
    pub async fn sweep_leases(&self, now_ms: u64) -> Result<Vec<ParkedLease>, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::SweepLeases { now_ms, reply })
            .await
            .map_err(|_| Reject::JournalClosed)?;
        rx.await.map_err(|_| Reject::JournalClosed)?
    }
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

    /// Atomically validate a persistent lease fence and enqueue its diff.
    ///
    /// The compare and journal admission share one actor mailbox turn, so a
    /// transfer cannot create a stale-token write between them.
    pub async fn start_fenced_diff(
        &self,
        record: JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<FencedApply, Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::ApplyFencedDiff {
                record,
                holder,
                lease_id,
                authority_seq,
                now_ms,
                reply,
            })
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
        superseded: HashSet<SupersededRow>,
    ) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::Restore {
                entities,
                by_cell,
                tombstones,
                superseded,
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

    /// Drop the bookkeeping a just-committed checkpoint discharged: tombstones
    /// past `now_ms`, whose rows the checkpoint cleared (D11 §6 checkpoint GC
    /// pass), and `superseded` — exactly the rows that checkpoint carried and
    /// therefore cleared. Awaiting this before the next checkpoint is what
    /// stops the marker from being rewritten, and the clear from being
    /// reissued, forever.
    pub async fn prune_checkpointed(
        &self,
        now_ms: u64,
        superseded: HashSet<SupersededRow>,
    ) -> Result<(), Reject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CellMsg::PruneCheckpointed {
                now_ms,
                superseded,
                reply,
            })
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
    lease_store: Arc<dyn LeaseStore>,
    joins: &Arc<ActorJoinSet>,
) -> CellActorHandle {
    spawn_preloaded(
        shard,
        grid,
        epoch,
        journal,
        lease_store,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashSet::new(),
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
    lease_store: Arc<dyn LeaseStore>,
    entities: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    tombstones: HashMap<PersistId, Tombstone>,
    superseded: HashSet<SupersededRow>,
    ckpt_watermark: Lsn,
    joins: &Arc<ActorJoinSet>,
) -> CellActorHandle {
    spawn_preloaded_with_recovery_now(
        shard,
        grid,
        epoch,
        journal,
        lease_store,
        entities,
        by_cell,
        tombstones,
        superseded,
        ckpt_watermark,
        registrar_now_ms,
        joins,
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_preloaded_with_recovery_now<F>(
    shard: orrery_protocol::CellId,
    grid: GridId,
    epoch: Epoch,
    journal: Arc<Journal>,
    lease_store: Arc<dyn LeaseStore>,
    entities: HashMap<PersistId, EntityRecord>,
    by_cell: HashMap<PersistId, orrery_protocol::CellId>,
    tombstones: HashMap<PersistId, Tombstone>,
    superseded: HashSet<SupersededRow>,
    ckpt_watermark: Lsn,
    recovery_now: F,
    joins: &Arc<ActorJoinSet>,
) -> CellActorHandle
where
    F: FnOnce() -> u64 + Send + 'static,
{
    let (tx, rx) = mpsc::channel(4096);
    let mut env = ActorEnv {
        state: CellActorState {
            shard,
            grid,
            epoch,
            entities,
            by_cell,
            tombstones,
            superseded,
            ckpt_watermark,
            leases: LeaseRegistrar::default(),
            lease_cells: HashMap::new(),
            entity_lsn: HashMap::new(),
            pending_rekeys: HashMap::new(),
        },
        journal,
        lease_store,
    };
    let join = tokio::spawn(async move {
        // Startup recovery is deliberately inside this single task: queued
        // requests cannot observe a partially restored registrar.
        let rows = match env
            .lease_store
            .load_cell(env.state.grid, env.state.shard)
            .await
        {
            Ok(rows) => rows,
            // Serving an empty actor after a failed durable restore could
            // mint a duplicate lease. Dropping its mailbox makes routing fail
            // closed until the runtime is recreated successfully.
            Err(_) => return,
        };
        let now = recovery_now();
        for (cell, row) in rows {
            let row = with_fresh_recovery_ttl(row, now);
            // Recovery is itself a durable transition; do not expose the
            // fresh TTL until its row has been stored.
            if !matches!(
                env.lease_store.put(env.state.grid, cell, &row).await,
                Ok(LeasePut::Stored)
            ) {
                // As above, fail closed rather than start with an incomplete
                // registrar after a durable transition failure.
                return;
            }
            env.state.lease_cells.insert(
                row.entity,
                checked_row_cell(env.state.shard, cell, "actor recovery"),
            );
            env.state.leases.restore(row);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovery_at_controlled_time_sets_exact_hot_and_durable_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(&crate::journal::JournalConfig {
                dir: dir.path().join("journal"),
                commit: crate::journal::GroupCommitConfig::default(),
            })
            .unwrap(),
        );
        let store = Arc::new(crate::lease::MemLeaseStore::new());
        let holder = iroh_base::SecretKey::from_bytes(&[1; 32]).public();
        let initial_joins = Arc::new(ActorJoinSet::new());
        let initial = spawn(
            CellId::ROOT,
            GridId::ROOT,
            Epoch::new(0),
            Arc::clone(&journal),
            store.clone(),
            &initial_joins,
        );
        let ClaimResult::Granted(prior) = initial
            .claim_lease(
                PersistId::new(1),
                CellId::ROOT,
                holder,
                orrery_protocol::ClaimKind::Weak,
                0,
            )
            .await
            .unwrap()
        else {
            panic!("initial claim should be granted");
        };
        initial.shutdown().await;
        initial_joins.join_all().await;

        let joins = Arc::new(ActorJoinSet::new());
        let recovery_now = 42;
        let actor = spawn_preloaded_with_recovery_now(
            CellId::ROOT,
            GridId::ROOT,
            Epoch::new(0),
            Arc::clone(&journal),
            store.clone(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashSet::new(),
            Lsn::new(0, 0),
            move || recovery_now,
            &joins,
        );
        let hot = actor
            .validate_lease(prior.entity, holder, prior.lease_id, recovery_now)
            .await
            .unwrap()
            .unwrap();
        let durable = store.load_cell(GridId::ROOT, CellId::ROOT).await.unwrap();

        assert_eq!(hot.expires_at, recovery_now + crate::lease::LEASE_TTL_MS);
        assert_eq!(durable, vec![(CellId::ROOT, hot)]);

        actor.shutdown().await;
        joins.join_all().await;
        journal.close().await.unwrap();
    }
}
