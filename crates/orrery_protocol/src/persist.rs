//! Persistence wire types (D11): intents, journal records, checkpoints.
//!
//! These are the engine-agnostic types for the `orrery_persistd` write paths
//! (docs/08-persistence.md, docs/10-crates.md §11). They are defined once here
//! so the persistence cluster, the client uplink, and the tests share one wire
//! surface, and so the two write classes — bulk (journal) and critical
//! (attested intents) — are serializable in isolation, before any FDB or
//! storage dependency exists.
//!
//! Canonical scalars (D15): [`Tick`] = u64 universe ticks, [`PersistId`] = u64
//! cluster-minted, [`Epoch`] = u64 shard-ownership fencing token, [`Lsn`] =
//! node-local journal position. [`CellId`] and [`GridId`] come from this crate.

use serde::{Deserialize, Serialize};

use crate::CellId;
use crate::GridId;
use crate::NodeId;
use crate::Signature;

/// A universe-global simulation tick (D8): u64, 60 Hz, anchored to a
/// coordinator-issued universe epoch. Signed logs, RNG seeds, witness epochs,
/// and journal records all reference absolute ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Tick(pub u64);

impl Tick {
    /// A tick from a raw u64.
    #[must_use]
    pub const fn new(tick: u64) -> Self {
        Self(tick)
    }
}

/// A stable, cluster-minted persistent entity id (D11, D15).
///
/// Never a Bevy `Entity`: this is the canonical id carried into every peer's
/// world and the storage key for `world/` rows. Minted either cluster-side
/// inside an intent transaction (returned in the commit receipt) or peer-side
/// from a journaled block grant (contiguous ranges, default 4096, usable
/// offline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PersistId(pub u64);

impl PersistId {
    /// A `PersistId` from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// A shard-ownership epoch (D11 §3.4): the fencing token that guards against
/// zombie actors committing stale checkpoints.
///
/// On assuming shard `S`, a node CASes `actor/{S}` from `(old_node, e)` to
/// `(self, e+1)`; every subsequent checkpoint transaction reads `actor/{S}`
/// and aborts if the epoch moved. Every [`JournalRecord`] carries the epoch it
/// was appended under, so recovery replay discards records from a superseded
/// epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Epoch(pub u64);

impl Epoch {
    /// An epoch from a raw u64.
    #[must_use]
    pub const fn new(epoch: u64) -> Self {
        Self(epoch)
    }
}

impl core::fmt::Display for Epoch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "epoch:{}", self.0)
    }
}

/// A node-local journal position: `(segment_seq, offset)` (D11 §4).
///
/// Monotonic per node. The `ckpt/{shard}` watermark row stores the LSN covered
/// by the last checkpoint; recovery replays records with `lsn > watermark`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Lsn {
    /// The segment sequence number.
    pub segment: u64,
    /// The byte offset within the segment.
    pub offset: u64,
}

impl Lsn {
    /// A journal position from a segment sequence and byte offset.
    #[must_use]
    pub const fn new(segment: u64, offset: u64) -> Self {
        Self { segment, offset }
    }
}

impl core::fmt::Display for Lsn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.segment, self.offset)
    }
}

/// The kind of a [`JournalRecord`] (D11 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecordKind {
    /// A component diff for an existing entity (bulk path, §2.1).
    ComponentDiff,
    /// A terrain delta for a chunk section (bulk path, §8).
    TerrainDelta,
    /// A new entity, carrying its freshly minted `PersistId`.
    Spawn,
    /// An entity despawn (tombstone).
    Despawn,
    /// A cross-cell movement commit (D7).
    Rekey,
    /// A checkpoint watermark marker.
    CheckpointMark,
}

/// A single append-only journal record (D11 §4).
///
/// One fsync stream per node is shared by all cell actors on that node; the
/// record is the unit of durable bulk state. Records are idempotent — keyed by
/// `(entity, tick)` with last-writer-wins per component within an entity's
/// single-writer stream — so unacked diffs can be resent on reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// Node-local, monotonic position.
    pub lsn: Lsn,
    /// The Morton `CellId` — the index key.
    pub cell: CellId,
    /// The grid this cell belongs to (root universe grid is 0).
    pub grid: GridId,
    /// The stable persistent entity id (never a Bevy `Entity`).
    pub entity: PersistId,
    /// The universe tick at append (D8).
    pub tick: Tick,
    /// The shard-ownership epoch at append (§3.4 fence).
    pub epoch: Epoch,
    /// The authoritative peer that produced the op.
    pub author: NodeId,
    /// The kind of record.
    pub kind: RecordKind,
    /// The postcard-encoded payload.
    pub payload: bytes::Bytes,
    /// CRC-32C over the record's payload (integrity check on replay).
    pub crc: u32,
}

/// A single operation inside an [`Intent`] (D11 §2.2).
///
/// The op payload is `Ruleset`-opaque: the game's `Ruleset` classifies ops as
/// bulk or critical and validates them against hot state. The wire type only
/// carries the op id and its encoded arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentOp {
    /// The `Ruleset`-defined op id.
    pub op: u16,
    /// The postcard-encoded op arguments.
    pub args: bytes::Bytes,
}

/// A witness co-signature on an [`Intent`] (D10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// The co-signing witness's NodeId.
    pub witness: NodeId,
    /// The witness's ed25519 signature over the intent.
    pub signature: Signature,
}

/// A signed, witness-attested critical-write envelope (D11 §2.2).
///
/// Intents are the only path for durable consequences (trades, currency,
/// progression, structure placement). The gateway verifies the issuer's
/// signature and K-of-N attestations, runs `Ruleset` validation against hot
/// state, then executes a FoundationDB serializable optimistic transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// The intent id — the idempotency key across retries.
    pub intent_id: u128,
    /// The issuing peer.
    pub issuer: NodeId,
    /// The cell-epoch the intent is bound to (binds the seeded witness set).
    pub cell_epoch: Epoch,
    /// The operations to apply.
    pub ops: Vec<IntentOp>,
    /// K-of-N witness co-signatures (default K=3 of N≥5, D16).
    pub attestations: Vec<Attestation>,
    /// The issuer's ed25519 signature over the intent.
    pub signature: Signature,
}

/// The outcome of an [`Intent`] execution (D11 §7).
///
/// Recorded in the `intent/{intent_id}` idempotency row so duplicate
/// submissions return the recorded outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentOutcome {
    /// The intent committed; the receipt carries cluster-minted `PersistId`s
    /// for any entities it created.
    Committed {
        /// The tick at which the commit was recorded.
        tick: Tick,
        /// `PersistId`s minted inside the transaction (crafting outputs, loot
        /// grants), in op order.
        minted: Vec<PersistId>,
    },
    /// The intent was rejected by `Ruleset` validation or a durable invariant.
    Rejected {
        /// A `Ruleset`-defined rejection reason code.
        reason: u16,
    },
}

/// A checkpoint watermark (D11 §3.4, §6 `ckpt/{shard}` row).
///
/// Records which journal LSN the last checkpoint covered, plus the epoch it
/// was taken under. Recovery replays records with `lsn > watermark`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The shard cell this checkpoint covers.
    pub shard: CellId,
    /// The journal LSN covered by this checkpoint.
    pub watermark: Lsn,
    /// The shard-ownership epoch the checkpoint was taken under.
    pub epoch: Epoch,
    /// The node that produced the checkpoint.
    pub node: NodeId,
    /// The wall-clock time the checkpoint was taken, as unix milliseconds.
    pub taken_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn sig() -> Signature {
        let seed = [0u8; 32];
        iroh_base::SecretKey::from_bytes(&seed).sign(b"test")
    }

    #[test]
    fn journal_record_roundtrips() {
        let record = JournalRecord {
            lsn: Lsn::new(3, 4096),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(42),
            tick: Tick::new(123_456),
            epoch: Epoch::new(7),
            author: node(1),
            kind: RecordKind::ComponentDiff,
            payload: bytes::Bytes::from_static(b"\x01\x02\x03"),
            crc: 0xdead_beef,
        };
        let bytes = postcard::to_stdvec(&record).unwrap();
        let back: JournalRecord = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn intent_roundtrips() {
        let intent = Intent {
            intent_id: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00,
            issuer: node(1),
            cell_epoch: Epoch::new(7),
            ops: vec![IntentOp {
                op: 3,
                args: bytes::Bytes::from_static(b"trade"),
            }],
            attestations: vec![Attestation {
                witness: node(2),
                signature: sig(),
            }],
            signature: sig(),
        };
        let bytes = postcard::to_stdvec(&intent).unwrap();
        let back: Intent = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, intent);
    }

    #[test]
    fn checkpoint_roundtrips() {
        let ckpt = Checkpoint {
            shard: CellId::ROOT,
            watermark: Lsn::new(9, 8192),
            epoch: Epoch::new(4),
            node: node(1),
            taken_at_ms: 1_700_000_000_000,
        };
        let bytes = postcard::to_stdvec(&ckpt).unwrap();
        let back: Checkpoint = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, ckpt);
    }

    #[test]
    fn scalar_newtypes_are_ordered() {
        assert!(Tick::new(1) < Tick::new(2));
        assert!(PersistId::new(1) < PersistId::new(2));
        assert!(Epoch::new(1) < Epoch::new(2));
        assert!(Lsn::new(1, 0) < Lsn::new(1, 1));
        assert!(Lsn::new(1, 1) < Lsn::new(2, 0));
    }
}
