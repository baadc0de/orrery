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

/// An account identity on the ledger/player rows (D11 §6).
///
/// Distinct from [`NodeId`]: a `NodeId` is a transport identity (D3), an
/// `AccountId` is the durable identity ledger balances, item ownership, and
/// profile rows are keyed by (`id/{account_id}` binds the two, D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountId(pub u64);

impl AccountId {
    /// An `AccountId` from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// A currency/asset class on the `ledger/bal/` rows (D11 §6).
///
/// `Ruleset`-defined (gold, crafting materials, …); the wire type only
/// carries the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetId(pub u64);

impl AssetId {
    /// An `AssetId` from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// A unique item instance id on the `ledger/item/` rows (D11 §6).
///
/// Unique items get exactly one ownership row each — the single-ownership row
/// *is* the anti-dupe invariant (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemUid(pub u64);

impl ItemUid {
    /// An `ItemUid` from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
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

/// Current serialization version for [`EntityRekey`].
pub const ENTITY_REKEY_VERSION: u8 = 1;

/// Server-owned payload for one committed storage-location transition.
///
/// This payload is journaled as [`RecordKind::Rekey`] and is intentionally not
/// part of the client [`crate::DiffUplink`] surface. `source_record` is the
/// opaque component image from the source actor, allowing later recovery to
/// reconstruct the destination without consulting mutable client state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRekey {
    /// Payload schema version.
    pub version: u8,
    /// Entity whose durable row moves.
    pub entity: PersistId,
    /// Grid containing the current durable row.
    pub source_grid: GridId,
    /// Cell containing the current durable row.
    pub source_cell: CellId,
    /// Grid receiving the durable row.
    pub destination_grid: GridId,
    /// Cell receiving the durable row.
    pub destination_cell: CellId,
    /// Exact registrar fence which must still own the source row.
    pub expected_lease_id: crate::LeaseId,
    /// Opaque source component image used by deterministic recovery.
    pub source_record: bytes::Bytes,
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

/// The domain separation tag for [`Intent::signing_preimage`]. Versioned so a
/// future preimage change can never collide with a signature made under this
/// one.
pub const INTENT_PREIMAGE_TAG: &[u8] = b"orrery/intent/v1";

impl Intent {
    /// The canonical byte string the issuer signs: domain-separated and
    /// **attestation-excluding** — it covers exactly `(intent_id, issuer,
    /// cell_epoch, ops)`, so a witness pushing an [`Attestation`] onto a
    /// signed intent can never invalidate the author's signature (the D10
    /// flow: the peer signs, *then* collects K-of-N co-signatures).
    ///
    /// This is the one canonical function used by both signer
    /// ([`Intent::sign`]) and verifier ([`Intent::verify_issuer`]) — a
    /// preimage computed in two places would drift. Lengths and counts are
    /// fixed-width little-endian so the encoding is unambiguous.
    #[must_use]
    pub fn signing_preimage(&self) -> Vec<u8> {
        let ops_len: usize = self.ops.iter().map(|op| 2 + 4 + op.args.len()).sum();
        let mut buf = Vec::with_capacity(INTENT_PREIMAGE_TAG.len() + 16 + 32 + 8 + 4 + ops_len);
        buf.extend_from_slice(INTENT_PREIMAGE_TAG);
        buf.extend_from_slice(&self.intent_id.to_le_bytes());
        buf.extend_from_slice(self.issuer.as_bytes());
        buf.extend_from_slice(&self.cell_epoch.0.to_le_bytes());
        buf.extend_from_slice(&(self.ops.len() as u32).to_le_bytes());
        for op in &self.ops {
            buf.extend_from_slice(&op.op.to_le_bytes());
            buf.extend_from_slice(&(op.args.len() as u32).to_le_bytes());
            buf.extend_from_slice(&op.args);
        }
        buf
    }

    /// Sign [`Intent::signing_preimage`] with `key`, replacing any previous
    /// signature. Call this **after** `intent_id`, `issuer`, `cell_epoch` and
    /// `ops` are final; attestations may be pushed afterwards (they are not
    /// covered).
    pub fn sign(&mut self, key: &iroh_base::SecretKey) {
        self.signature = key.sign(&self.signing_preimage());
    }

    /// Verify the issuer's ed25519 signature over
    /// [`Intent::signing_preimage`]. The gateway runs this before anything
    /// else (docs/08-persistence.md §2.2: signature checks happen at the edge,
    /// before any transaction work).
    #[must_use]
    pub fn verify_issuer(&self) -> bool {
        self.issuer
            .verify(&self.signing_preimage(), &self.signature)
            .is_ok()
    }
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

/// The issuer's ed25519 signature did not verify (gateway edge check, §2.2).
pub const REASON_BAD_SIGNATURE: u16 = 1;
/// The intent's `issuer` does not match the connection's authenticated
/// transport identity — a peer may not submit intents in another's name.
pub const REASON_ISSUER_MISMATCH: u16 = 2;
/// The gateway has no intent executor configured: intents cannot be committed
/// durably, so the honest reply is a rejection, never a fake commit (the
/// inverted RPO-0 the gateway stub had before the intent execution path).
pub const REASON_NO_EXECUTOR: u16 = 3;
/// The `Ruleset` admission check rejected the intent (validator verdict).
pub const REASON_VALIDATION_FAILED: u16 = 4;
/// The executor's serializable transaction exhausted its bounded retries
/// (docs/08-persistence.md §7: after 5 conflict retries the gateway returns a
/// definitive refusal and the client's predicted outcome rolls back).
pub const REASON_CONTENTION_EXHAUSTED: u16 = 5;
/// The executor failed for a non-conflict reason (store unavailable, …).
pub const REASON_EXECUTOR_ERROR: u16 = 6;

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

    #[test]
    fn entity_rekey_payload_round_trips_with_explicit_version_and_source_record() {
        // Given: a committed cross-grid move with an exact source image and fence.
        let cells = CellId::ROOT.children();
        let rekey = EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity: PersistId::new(77),
            source_grid: GridId::ROOT,
            source_cell: cells[0],
            destination_grid: GridId::new(8),
            destination_cell: cells[1],
            expected_lease_id: crate::LeaseId(42),
            source_record: bytes::Bytes::from_static(b"component-image"),
        };

        // When: the server-owned payload crosses its postcard boundary.
        let encoded = postcard::to_allocvec(&rekey).unwrap();
        let decoded: EntityRekey = postcard::from_bytes(&encoded).unwrap();

        // Then: recovery-critical fields survive without DiffUplink involvement.
        assert_eq!(decoded, rekey);
    }

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
    fn preimage_is_stable_and_attestation_independent() {
        // D10 flow: the peer signs, *then* collects witness co-signatures — so
        // pushing an Attestation must not change the bytes the author signed.
        let mut intent = Intent {
            intent_id: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00,
            issuer: node(1),
            cell_epoch: Epoch::new(7),
            ops: vec![IntentOp {
                op: 3,
                args: bytes::Bytes::from_static(b"trade"),
            }],
            attestations: Vec::new(),
            signature: sig(),
        };
        let before = intent.signing_preimage();
        intent.attestations.push(Attestation {
            witness: node(2),
            signature: sig(),
        });
        let after = intent.signing_preimage();
        assert_eq!(
            before, after,
            "pushing a co-signature must not move the author's preimage"
        );

        // Independent derivation: the preimage is the domain tag followed by
        // the fixed-width fields, with no attestation bytes in it.
        let mut expected = INTENT_PREIMAGE_TAG.to_vec();
        expected.extend_from_slice(&intent.intent_id.to_le_bytes());
        expected.extend_from_slice(intent.issuer.as_bytes());
        expected.extend_from_slice(&intent.cell_epoch.0.to_le_bytes());
        expected.extend_from_slice(&1u32.to_le_bytes()); // ops count
        expected.extend_from_slice(&3u16.to_le_bytes()); // op id
        expected.extend_from_slice(&5u32.to_le_bytes()); // args len
        expected.extend_from_slice(b"trade");
        assert_eq!(before, expected, "canonical layout, derived by hand");
    }

    #[test]
    fn signature_roundtrips() {
        let mut intent = Intent {
            intent_id: 42,
            issuer: node(1),
            cell_epoch: Epoch::new(0),
            ops: vec![IntentOp {
                op: 1,
                args: bytes::Bytes::from_static(b"buy"),
            }],
            attestations: Vec::new(),
            signature: sig(), // overwritten by sign()
        };
        let key = iroh_base::SecretKey::from_bytes(&{
            let mut seed = [0u8; 32];
            seed[0] = 1;
            seed
        });
        intent.sign(&key);
        assert!(intent.verify_issuer(), "sign-then-verify_issuer is true");

        // Mutating any ops byte invalidates the author signature.
        intent.ops[0].args = bytes::Bytes::from_static(b"buz");
        assert!(
            !intent.verify_issuer(),
            "a mutated ops byte must fail verification"
        );
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
