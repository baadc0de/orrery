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
//! cluster-minted, [`Epoch`] = u64 shard-ownership fencing token,
//! [`CellEpoch`] = u64 witness-set epoch an intent binds to, [`Lsn`] =
//! node-local journal position. [`CellId`] and [`GridId`] come from this crate.

use serde::{Deserialize, Serialize};

use crate::CellId;
use crate::EvidenceCommitment;
use crate::GridId;
use crate::NodeId;
use crate::Signature;
use crate::EVIDENCE_COMMITMENT_PREIMAGE_LEN;

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

/// A witness-set cell epoch (D10): the epoch of the cell's seeded witness set
/// that an [`Intent`] is bound to.
///
/// **Not an [`Epoch`].** That token is minted cluster-side when a persistd
/// node assumes a shard; this one is chosen peer-side and names the witness
/// set whose K-of-N attestations the intent carries. The two are separate
/// namespaces and are never comparable — they shared a type once, and the
/// intent fence silently compared them.
///
/// Wire-identical to [`Epoch`]: both are a newtype over one u64, so the
/// postcard encoding of an [`Intent`] is unchanged by the distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellEpoch(pub u64);

impl CellEpoch {
    /// A cell epoch from a raw u64.
    #[must_use]
    pub const fn new(epoch: u64) -> Self {
        Self(epoch)
    }
}

impl core::fmt::Display for CellEpoch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "cell-epoch:{}", self.0)
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

impl JournalRecord {
    /// Encode this record as a versioned logical frame:
    /// `postcard(record) ‖ [JOURNAL_RECORD_ENCODING]`.
    ///
    /// Every journal backend writes through this rather than calling postcard
    /// directly, so there is one answer to "what encoding is a record on
    /// disk?" and it is stamped by the writer rather than inferred by the
    /// reader (D38 clause (d)(5)).
    ///
    /// # Errors
    ///
    /// Returns the postcard error if the record does not serialize.
    pub fn encode_frame(&self) -> Result<Vec<u8>, postcard::Error> {
        crate::atrest::encode_versioned(self, JOURNAL_RECORD_ENCODING)
    }

    /// Decode a logical frame, returning the record and the encoding version
    /// it was written under.
    ///
    /// A frame with no trailer is [`ENCODING_V0`](crate::atrest::ENCODING_V0):
    /// a record written before journals were self-describing, read rather than
    /// refused (the bootstrap rule, [`crate::atrest`]).
    ///
    /// # Errors
    ///
    /// [`crate::atrest::VersionedError`] if the body does not decode or more
    /// than a version byte follows it.
    pub fn decode_frame(
        bytes: &[u8],
    ) -> Result<(Self, crate::atrest::EncodingVersion), crate::atrest::VersionedError> {
        crate::atrest::decode_versioned(bytes)
    }
}

/// The encoding version [`JournalRecord::encode_frame`] stamps on every
/// logical journal record it writes (D38 clause (d)(5)).
///
/// **Bump this whenever the logical record's shape or its payload framing
/// changes** — a new field, a `RecordKind` whose payload changes layout, a
/// change to how `payload` is framed. It is not a rules version and not a
/// physical-envelope version: the WAL's own `RawEnvelope` versions the *file
/// format*, and D38 is explicit that the physical envelope is the upgrade
/// vehicle and not the answer to "what schema is this record?".
///
/// Version 1 is the shape as of the change that made journal records
/// self-describing. Everything written before it decodes as
/// [`ENCODING_V0`](crate::atrest::ENCODING_V0) under the bootstrap rule
/// ([`crate::atrest`]), which is what lets an existing journal be replayed
/// rather than refused.
pub const JOURNAL_RECORD_ENCODING: crate::atrest::EncodingVersion = 1;

/// Current serialization version for [`EntityRekey`].
///
/// **Bumped to 2** by D38 clause (d)(2), which added
/// [`EntityRekey::source_schema_floor`]. A version-1 payload is refused rather
/// than bootstrapped, and that is this field's whole point: the bootstrap rule
/// of [`crate::atrest`] gives *unversioned* bytes a defined meaning, while a
/// payload that states a version it no longer matches is a mismatch, not an
/// old era. Journals are retention-bounded (D20), so the two shapes coexist
/// only for as long as one deployment's rollout takes.
///
/// Predates [`JOURNAL_RECORD_ENCODING`] and stays: this one versions the
/// *payload* of a single server-owned `RecordKind` from inside the payload,
/// which is the per-kind mechanism D38 clause (d)(5) names as the alternative
/// to a record-level version. The two compose — the record frame says how the
/// record is shaped, this says how one kind's bytes are shaped — and neither
/// is derived from the other.
pub const ENTITY_REKEY_VERSION: u8 = 2;

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
    /// The schema floor of [`Self::source_record`] (D38 clause (d)(2)).
    ///
    /// A rekey is the one path on which a `world/` row's bag crosses from one
    /// actor to another, and the destination writes the row it lands in. Without
    /// this field the destination would have to invent a floor for a bag it
    /// cannot open — and the only value it could invent is
    /// [`SCHEMA_V0`](crate::atrest::SCHEMA_V0), silently demoting an
    /// up-to-date row to the oldest era and inviting a later sweep to migrate
    /// it from a version it was never written at. So the floor travels with the
    /// image it describes.
    pub source_schema_floor: crate::atrest::SchemaVersion,
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
    /// The witness's ed25519 signature over D27's attestation preimage.
    pub signature: Signature,
}

impl Attestation {
    /// Verify this witness signature against D27's role-separated preimage.
    ///
    /// This deliberately does not use [`Intent::signing_preimage`]. The
    /// attestation preimage commits to the issuer signature and to
    /// [`Self::witness`], so neither an issuer signature nor an attestation
    /// made by another witness can be substituted here.
    #[must_use]
    pub fn verify(&self, intent: &Intent) -> bool {
        self.witness
            .verify(&intent.attestation_preimage(self.witness), &self.signature)
            .is_ok()
    }
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
    pub cell_epoch: CellEpoch,
    /// The operations to apply.
    pub ops: Vec<IntentOp>,
    /// K-of-N witness co-signatures (default K=3 of N≥5, D16).
    pub attestations: Vec<Attestation>,
    /// D29 clause 6's fixed-size commitment to the history behind this
    /// intent, or `None` when the submitter offers none.
    ///
    /// **Only the low-population path consumes it.** An attested intent is
    /// judged by its co-signatures and this field is ignored; an intent that
    /// falls to D29's provisional path is finalized by spot replay, and this
    /// is what the replay is held to. A submitter that offers none is
    /// therefore submitting an intent the cluster can commit but never
    /// finalize, which is why admission refuses the provisional path without
    /// it rather than committing something guaranteed to expire.
    ///
    /// Covered by [`Self::signing_preimage`], so it is the issuer's own
    /// signed statement (see [`EvidenceCommitment`]).
    pub evidence: Option<EvidenceCommitment>,
    /// The issuer's ed25519 signature over the intent.
    pub signature: Signature,
}

/// The domain separation tag for [`Intent::signing_preimage`]. Versioned so a
/// future preimage change can never collide with a signature made under this
/// one.
pub const INTENT_PREIMAGE_TAG: &[u8] = b"orrery/intent/v1";

/// The domain separator at offset 0 of D27's attestation preimage.
pub const ATTESTATION_PREIMAGE_TAG: &[u8; 21] = b"orrery/attestation/v1";

/// The fixed byte length of D27's attestation preimage.
pub const ATTESTATION_PREIMAGE_LEN: usize = 157;

/// The domain tag mixed into every per-intent required-subset draw
/// (D27 clause (d)).
pub const ATTESTATION_DRAW_V1_DOMAIN: &[u8] = b"orrery/attestation-draw/v1";

/// The number of announced witnesses whose co-signatures an attested intent
/// must carry: D10's and [D16][d16]'s `K = 3` of `N >= 5`.
///
/// This is a *required subset*, not a count. Three attestations that are not
/// the three [`required_witnesses`] names admit nothing — which is exactly
/// what removes attestation shopping, because the submitter cannot learn which
/// three count until the epoch's draw key is revealed.
///
/// [d16]: https://github.com/baadc0de/orrery/blob/main/docs/adr/0016-parameter-reference.md
pub const WITNESS_QUORUM_K: usize = 3;

/// The eligible vector `E(I)` of D27 clause (d): the announced witness set in
/// announced order, minus every party to the intent.
///
/// # What "party" means here, precisely
///
/// D10 item 4 excludes parties "matched on **accounts and every NodeId bound
/// to them**". The only party this function can see is the issuer's NodeId,
/// and it does not pretend otherwise: the account↔NodeId binding lives in
/// `orrery_identity` and is not reachable from an [`Intent`].
///
/// This is therefore the **first** of two filters, not the whole of `E(I)`.
/// The gateway composes an account-level pass on top of this one, resolving
/// each surviving candidate through D31 clause (e)'s `owner(n)` and dropping
/// every NodeId bound to a party account — and, per D31 clause (f), every
/// NodeId whose binding does not resolve at all. The signature stays as it is
/// because the composition belongs where the resolver is; a `NodeId`-only
/// function with a published test vector should not grow an authority
/// parameter to express that.
///
/// The coordinator's selection-time half remains *approximated* (D28
/// clause (e)): a NodeId bound to the same account but connected to a
/// different coordinator is not deduped out of the candidate pool.
///
/// # Why announced order is preserved
///
/// The order is part of the audited artifact. A verifier recomputing
/// [`required_witnesses`] after the reveal draws over this exact vector, and
/// D27 clause (f) requires the gateway to *record* it with the committed
/// intent rather than let an auditor rebuild it from bindings that have since
/// moved. Sorting or de-duplicating here would silently make the recorded
/// vector a different object from the announced one.
#[must_use]
pub fn eligible_witnesses(selected: &[NodeId], issuer: NodeId) -> Vec<NodeId> {
    selected
        .iter()
        .copied()
        .filter(|witness| *witness != issuer)
        .collect()
}

/// The required subset `required(I)` of D27 clause (d): the
/// [`WITNESS_QUORUM_K`] members of `eligible` with the smallest keyed hash.
///
/// ```text
/// r_i         = blake3::keyed_hash(draw_key,
///                   ATTESTATION_DRAW_V1_DOMAIN ‖ intent_id_le ‖ eligible[i])
/// required(I) = the K members with the smallest r_i, compared as big-endian
///               32-byte integers, ties broken by NodeId bytewise ascending
/// ```
///
/// `blake3::keyed_hash` is a MAC, so this is docs/07 §4.2's
/// `HMAC(seed, intent_id)` construction with a different primitive and — the
/// substantive change D27 makes — a different key holder: `draw_key` is
/// generated by the persistence cluster and never leaves it, because the only
/// party that consumes the draw is the party that checks the intent.
///
/// **The secret is what makes this un-grindable.** `intent_id` is a
/// submitter-chosen `u128` ([`Intent::intent_id`]), so a *public* draw would
/// let a submitter grind ids offline until the required slots landed on its
/// three colluders — about `C(7,3)/C(3,3) = 35` hashes. With `draw_key`
/// secret until epoch end there is nothing to grind against.
///
/// Returns fewer than [`WITNESS_QUORUM_K`] entries only when `eligible` is
/// shorter than that, which a caller must treat as "no draw was made" rather
/// than as a smaller quorum: D27 makes no draw at all below
/// [`WITNESS_SET_FLOOR_N`](crate::WITNESS_SET_FLOOR_N) and hands the intent to
/// D29's low-population path.
#[must_use]
pub fn required_witnesses(
    draw_key: &[u8; 32],
    intent_id: u128,
    eligible: &[NodeId],
) -> Vec<NodeId> {
    let mut scored: Vec<([u8; 32], NodeId)> = eligible
        .iter()
        .map(|witness| {
            let mut input =
                Vec::with_capacity(ATTESTATION_DRAW_V1_DOMAIN.len() + 16 + NodeId::LENGTH);
            input.extend_from_slice(ATTESTATION_DRAW_V1_DOMAIN);
            input.extend_from_slice(&intent_id.to_le_bytes());
            input.extend_from_slice(witness.as_bytes());
            (*blake3::keyed_hash(draw_key, &input).as_bytes(), *witness)
        })
        .collect();
    // Big-endian integer order *is* lexicographic order over the digest bytes,
    // so the derived `Ord` on `[u8; 32]` is D27's comparison verbatim. The
    // NodeId is the tiebreak and is compared bytewise ascending — two equal
    // digests are a blake3 collision and will not happen, but a total order
    // written down is a total order two implementations agree on, and this
    // draw has to be reproducible by an auditor that is not this code.
    scored.sort_unstable_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.as_bytes().cmp(right.1.as_bytes()))
    });
    scored
        .into_iter()
        .take(WITNESS_QUORUM_K)
        .map(|(_, witness)| witness)
        .collect()
}

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
        let mut buf = Vec::with_capacity(
            INTENT_PREIMAGE_TAG.len()
                + 16
                + 32
                + 8
                + 4
                + ops_len
                + 1
                + EVIDENCE_COMMITMENT_PREIMAGE_LEN,
        );
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
        // D29 clause 6's commitment, appended **after** the ops and tagged
        // with a presence byte rather than being encoded as a length.
        //
        // The tag matters more than it looks. Without it, "no commitment" and
        // "a commitment of 124 zero bytes" would produce the same preimage,
        // and a submitter could strip a signed commitment off an intent and
        // still present a verifying signature — which is precisely the
        // after-the-fact substitution the commitment exists to prevent, run
        // in the one direction that turns a finalizable intent into an
        // unfinalizable one.
        match &self.evidence {
            None => buf.push(0),
            Some(commitment) => {
                buf.push(1);
                buf.extend_from_slice(&commitment.preimage());
            }
        }
        buf
    }

    /// D27's canonical, fixed-length byte string a witness signs.
    ///
    /// The layout is exact and has no length prefixes: the 21-byte
    /// [`ATTESTATION_PREIMAGE_TAG`], the 32-byte blake3 digest of
    /// [`Self::signing_preimage`], `cell_epoch` as eight little-endian bytes,
    /// the issuer's 64-byte Ed25519 signature, and the witness's 32-byte
    /// [`NodeId`], in that order. The redundant epoch is intentional: its
    /// constant offset keeps the epoch binding visible even if a future intent
    /// preimage changes shape.
    #[must_use]
    pub fn attestation_preimage(&self, witness: NodeId) -> [u8; ATTESTATION_PREIMAGE_LEN] {
        let intent_hash = blake3::hash(&self.signing_preimage());
        let mut preimage = [0_u8; ATTESTATION_PREIMAGE_LEN];
        preimage[..21].copy_from_slice(ATTESTATION_PREIMAGE_TAG);
        preimage[21..53].copy_from_slice(intent_hash.as_bytes());
        preimage[53..61].copy_from_slice(&self.cell_epoch.0.to_le_bytes());
        preimage[61..125].copy_from_slice(&self.signature.to_bytes());
        preimage[125..157].copy_from_slice(witness.as_bytes());
        preimage
    }

    /// Co-sign this already issuer-signed intent as `key`'s witness.
    ///
    /// Call this only after [`Self::sign`]. The issuer signature is itself
    /// inside [`Self::attestation_preimage`], so replacing or re-making it
    /// invalidates the returned attestation even when `intent_id` is reused.
    #[must_use]
    pub fn attest(&self, key: &iroh_base::SecretKey) -> Attestation {
        let witness = key.public();
        Attestation {
            witness,
            signature: key.sign(&self.attestation_preimage(witness)),
        }
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
    /// The intent committed **provisionally** on D29's low-population path:
    /// durable, attributable, and quarantined until the cluster finalizes it
    /// by spot replay.
    ///
    /// # This is not a hedge about durability
    ///
    /// The reply is sent after the FoundationDB transaction resolves, exactly
    /// as [`Self::Committed`] is, so RPO 0 is untouched (D11). What is
    /// provisional is the *verdict*, not the write: the row is there, the
    /// value is real, and the cluster has not yet re-executed the history
    /// behind it.
    ///
    /// # What the submitter may do with it
    ///
    /// Hold it and display it. **Not** spend it: D29 clause 4 makes a
    /// provisional row an input to nothing until its originating intent is
    /// finalized, which is what bounds the annulment set at exactly one
    /// intent. A client's local prediction *holds* on this arm — it neither
    /// rolls back (the value is real) nor resolves terminally (the verdict is
    /// not in).
    ///
    /// # It shares nothing but a word with the other two "provisional"s
    ///
    /// D29 clause 5's second normative sentence: this is unrelated to
    /// `LeaseFlags::PROVISIONAL` and to the *bulk*-ack disposition of the same
    /// name. A provisional bulk ack means "resend this"; a provisional intent
    /// outcome means the exact opposite — never resend, the durable row is
    /// already there and a replay returns this same answer.
    Provisional {
        /// The tick at which the provisional commit was recorded.
        tick: Tick,
        /// `PersistId`s minted inside the transaction, in op order. Minted
        /// ids are **not** returned by annulment: D29 clause 8 records that
        /// as one of the four things a forward-written inverse cannot undo.
        minted: Vec<PersistId>,
        /// Unix-millisecond deadline by which the cluster must finalize or
        /// annul this intent
        /// ([`PROVISIONAL_FINALIZE_DEADLINE_MS`]). Reaching it **annuls**;
        /// it never auto-finalizes, because auto-finalizing would make
        /// outlasting the replay queue a winning strategy.
        finalize_by: u64,
    },
}

/// How long a commit may stay provisional before the cluster annuls it
/// (D29 clause 9, `provisional_finalize_deadline`, default 5 min).
///
/// # Chosen, not measured
///
/// Five minutes exceeds an evidence fetch — one RTT, plus a retry, plus a
/// reconnect window for a submitter whose 10 s lease has lapsed (D7) — by
/// three orders of magnitude, and sits an order of magnitude under the
/// `intent/` row retention that bounds it. D29 marks it proposed into D16 and
/// asks for it to be re-derived from the first shadow-mode telemetry rather
/// than defended.
///
/// The interlock it must satisfy is
/// `PROVISIONAL_FINALIZE_DEADLINE_MS ≪ INTENT_ROW_RETENTION_MS` (5 min against
/// 1 h, a factor of twelve), so a provisional row is always resolved with ≥ 55
/// minutes of retention left and can never vanish under a replay.
pub const PROVISIONAL_FINALIZE_DEADLINE_MS: u64 = 5 * 60 * 1000;

/// The most unfinalized provisional intents one account may hold at once
/// (D29 clause 9(b), `C = 8`).
///
/// # Why a cap exists at all
///
/// It is the per-account value-at-risk dial: `VaR(account) ≤ C · v_max`. Past
/// it, further low-population intents from that account are **refused**, not
/// queued — and refusal is the routine response the whole clause is arranged
/// to produce, because expiry is the one outcome that destroys value the
/// cluster already promised. An expiry in production is an incident; a
/// refusal is a defined, liability-free answer the admission function already
/// knows how to give.
///
/// `C = 8` has no derivation. It is set low because nothing in a
/// low-population cell should be producing eight unfinalized intents at once,
/// and an account that is has already told the operator something.
pub const PROVISIONAL_OUTSTANDING_CAP: usize = 8;

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

// The durable-invariant refusals of the item-ownership transfer
// (docs/08-persistence.md §7's `Reject::NoSuchItem` / `Reject::NotOwner` /
// `Reject::Insufficient`). They are **not** `REASON_EXECUTOR_ERROR`, and that
// is the whole reason they exist: a trade refused because the item moved a
// millisecond earlier is an ordinary, correct answer, while an executor error
// is a server fault. Collapsing the two makes a working anti-dupe invariant
// indistinguishable from a broken cluster on the operator's dashboard, and
// makes the dupe gauntlet (P5) unable to tell a refused double-spend from a
// crashed one. Each cause therefore gets its own code, in the same spirit as
// [`crate::DenyReason::WrongOwner`] on the authority path.

/// `ledger/item/{item_uid}` has no row: the item does not exist.
pub const REASON_NO_SUCH_ITEM: u16 = 7;
/// The durable `ledger/item/{item_uid}` row names a different owner than the
/// transfer's divesting party. **This is also what a lost double-spend race
/// looks like**: the loser's retry re-reads the row, sees the winner's owner,
/// and fails its check honestly (docs/08-persistence.md §7).
pub const REASON_NOT_ITEM_OWNER: u16 = 8;
/// The debited party's `ledger/bal/{account}/{asset}` row does not cover the
/// transfer's price.
pub const REASON_INSUFFICIENT_BALANCE: u16 = 9;
/// The transfer names one account as both parties. A self-transfer is not a
/// trade: it would write an ownership row that already holds that value and
/// bank a receipt for an event that did not happen.
pub const REASON_ITEM_TRANSFER_TO_SELF: u16 = 10;
/// An op this cluster's own executor interprets carried `args` it could not
/// decode. A bad request, not a server fault — which is why it is a rejection
/// reason rather than [`REASON_EXECUTOR_ERROR`].
pub const REASON_MALFORMED_OP: u16 = 11;

/// An [`Attestation`] names the intent's own `issuer` as its witness — the
/// issuer signed its own permission slip.
///
/// D10 item 4 seeds the witness set "excluding **all parties to the intent**",
/// and the issuer is the first of those parties; `docs/07-witnessing.md` §4.1
/// states the same rule and §4.2 makes the gateway the enforcer ("the gateway
/// rejects party attestations regardless"). This is the admission-time half of
/// that rule — the selection-time half is the coordinator's (D28/#143), and a
/// gateway must not assume a set it did not choose is well-formed.
///
/// # Why this is not folded into [`REASON_VALIDATION_FAILED`]
///
/// It is the one admission refusal that is never a client bug. Every other
/// cause the baseline validator can raise describes a malformed or
/// misaddressed request — a short `args` field, an account the connection did
/// not authenticate as — and a client author reading `REASON_VALIDATION_FAILED`
/// will find it. A self-witnessed intent is a *forgery attempt*, and an
/// operator watching the rejection rate needs to be able to count it
/// separately from the noise floor of bad clients. Collapsing the two costs
/// exactly what [`crate::DenyReason::WrongOwner`] cost on the authority path:
/// an opaque refusal that every subsequent investigation has to re-derive.
pub const REASON_SELF_WITNESS: u16 = 12;

/// The intent's attestations do not satisfy D27's K-of-N admission predicate.
///
/// One code covers every way the quorum can fail — too few attestations, a
/// signer outside the announced set, a required co-signer missing, an epoch
/// this gateway cannot resolve or has aged out — and the split is deliberate
/// in exactly one place: this code separates **"your attestations were
/// wrong"** from [`REASON_VALIDATION_FAILED`]'s "your ops were wrong". That
/// is the distinction a co-signing client needs to retry sensibly: an ops
/// failure is final, while a quorum failure is answered by collecting more
/// co-signatures (or waiting for the current epoch's announcement to reach
/// this gateway) and resubmitting the same intent.
///
/// # Why the *causes* are not enumerated here
///
/// They are, but in the gateway's logs rather than on the wire
/// (`orrery_persistd`'s `RejectionCause`, one stable label each). Two reasons.
/// A submitter must not be told **which** required witness it is missing:
/// `required(I)` is drawn with a secret the gateway holds until epoch end
/// (D27 clause (d)), and a per-cause reply would leak the draw one intent at
/// a time, turning `intent_id` grinding back on. And an operator — who is the
/// one party that needs the distinction between a netsplit and a forgery —
/// reads it from the gateway, where the labels already are.
pub const REASON_ATTESTATION_QUORUM: u16 = 13;

/// The intent named a row an unfinalized **provisional** commit wrote, and
/// D29 clause 4 makes such a row an input to nothing.
///
/// # Why this is its own code and not folded into `REASON_VALIDATION_FAILED`
///
/// Every other refusal on this path tells the submitter something it did
/// wrong. This one tells it something about *timing*: the same intent, resent
/// after the originating intent finalizes, succeeds unchanged. Collapsing it
/// into the generic validation code would leave a client with no way to tell
/// "never do this" from "do this in a moment", and the correct behaviour for
/// the two is opposite.
///
/// The quarantine it enforces is the whole cascade defence. If a provisional
/// output could be spent, the set of intents that must be reversed when the
/// original is annulled is the transitive closure of everything derived from
/// it — across accounts, and including intents that have since *finalized*.
/// Containment at depth 1 makes that set exactly one intent, and this code is
/// what containment sounds like from the outside.
pub const REASON_PROVISIONAL_INPUT: u16 = 14;

/// The intent was eligible for D29's low-population path by population, and
/// refused by its **classification**: it is not reversible by a
/// forward-written inverse the cluster can write on its own (D29 clause 3).
///
/// Value *creation into escrow* is admitted on this path — loot, crafting
/// output, progression, anything whose credit and debit are both inside the
/// submitting account's own rows. Value *transfer* is refused: any op naming a
/// second account, any sink the cluster cannot re-credit.
///
/// **The reference two-party trade is refused in a low-population cell**, and
/// that is a deliberate, player-visible product hole rather than an oversight.
/// Party exclusion removes both traders from the eligible set anyway, so a
/// trade in a two-person cell has no witnesses by construction — and
/// committing it provisionally would be committing the most cascade-prone
/// operation there is on the least evidence.
pub const REASON_PROVISIONAL_INELIGIBLE: u16 = 15;

/// This account already holds
/// [`PROVISIONAL_OUTSTANDING_CAP`] unfinalized provisional intents
/// (D29 clause 9(b)).
///
/// A refusal, never a queue. The cluster's response to a finalizer that
/// cannot keep up is to stop *admitting* provisional intents long before it
/// starts annulling old ones, because refusal is a defined answer that costs
/// the player nothing they had, and expiry destroys value the cluster already
/// promised.
pub const REASON_PROVISIONAL_CAP: u16 = 16;

/// The intent id is a replay of an intent the cluster **annulled**
/// (D29 clause 8).
///
/// The durable row is still there — that is what makes this answerable at all,
/// and D29 clause 9(c)'s GC interlock is what keeps it there — so the replay
/// applies nothing and is told what happened to the original. Distinct from
/// every other rejection because nothing the submitter can do changes it: the
/// commit happened, was reversed, and the reversal is in the ledger's history
/// forever.
pub const REASON_INTENT_ANNULLED: u16 = 17;

/// The intent fell to D29's low-population path and carries no
/// [`EvidenceCommitment`], so nothing could ever finalize it.
///
/// Committing it would mean minting durable value with a guaranteed expiry
/// five minutes later, which converts the honest answer (refuse now) into the
/// one outcome D29 clause 9(b) exists to avoid (annul later). Refusal is free
/// and the submitter can resubmit with a commitment attached.
pub const REASON_PROVISIONAL_NO_EVIDENCE: u16 = 18;

/// [`GatewayReply::ReportVerdict`] reason: the report was adjudicated, and the
/// reply's `verdict` carries the answer.
///
/// # Why this is its own numbering rather than more `REASON_*`
///
/// The `REASON_*` codes above are `IntentOutcome::Rejected` reasons: they
/// answer "why was this durable write refused", and the space below `13` is
/// partly a `Ruleset`'s to extend. A refused *report* is a different question
/// on a different message, so it gets its own space rather than borrowing
/// numbers whose meaning a game may redefine. The two never appear in the same
/// field.
///
/// [`GatewayReply::ReportVerdict`]: crate::GatewayReply::ReportVerdict
pub const REPORT_ADJUDICATED: u16 = 0;
/// [`GatewayReply::ReportVerdict`] reason: this gateway has no adjudication
/// executor configured, so nothing here can judge the evidence.
///
/// The honest answer to a report the cluster cannot adjudicate, and the
/// counterpart of [`REASON_NO_EXECUTOR`] on the intent path: silence would
/// leave a witness re-filing forever against a gateway that was never going to
/// answer. It is deliberately reachable in a default build — the executor is
/// registered by the deployed binary the game team links its `Ruleset` into
/// (docs/09-services-and-ops.md §1), not by this crate.
///
/// [`GatewayReply::ReportVerdict`]: crate::GatewayReply::ReportVerdict
pub const REPORT_REFUSED_NO_ADJUDICATOR: u16 = 1;
/// [`GatewayReply::ReportVerdict`] reason: this account is over its report
/// rate limit (docs/07-witnessing.md §7, "observer is the liar").
///
/// Not a strike, and never confused with one: `Unadjudicable` verdicts carry
/// no weight either, so a reporter cannot be punished for a flood — only shed.
///
/// [`GatewayReply::ReportVerdict`]: crate::GatewayReply::ReportVerdict
pub const REPORT_REFUSED_RATE_LIMITED: u16 = 2;
/// [`GatewayReply::ReportVerdict`] reason: the report's `reporter` is not the
/// connection's authenticated transport identity.
///
/// The same binding [`REASON_ISSUER_MISMATCH`] enforces for intents, and it is
/// what makes the per-account limit above meaningful: without it a flooder
/// would simply spend somebody else's budget.
///
/// [`GatewayReply::ReportVerdict`]: crate::GatewayReply::ReportVerdict
pub const REPORT_REFUSED_REPORTER_MISMATCH: u16 = 3;
/// [`GatewayReply::ReportVerdict`] reason: the connection has no established
/// session, so there is no account to bill the report to.
///
/// Rate limiting is per *account* (§7), which a pre-`Hello` connection does
/// not have. Accepting reports there would leave the one unmetered path into
/// the adjudicator.
///
/// [`GatewayReply::ReportVerdict`]: crate::GatewayReply::ReportVerdict
pub const REPORT_REFUSED_NO_SESSION: u16 = 4;

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
    fn cell_epoch_is_wire_identical_to_epoch() {
        // Splitting `Intent::cell_epoch` off `Epoch` is a type-system change
        // only: both are one u64, so no encoded intent moved a byte.
        for raw in [0u64, 1, 7, u64::MAX] {
            assert_eq!(
                postcard::to_stdvec(&CellEpoch::new(raw)).unwrap(),
                postcard::to_stdvec(&Epoch::new(raw)).unwrap(),
                "cell epoch {raw} must encode exactly as the ownership epoch did"
            );
        }
    }

    #[test]
    fn entity_rekey_payload_round_trips_with_explicit_version_and_source_record() {
        // Given: a committed cross-grid move with an exact source image and fence.
        let cells = CellId::ROOT.children();
        let rekey = EntityRekey {
            source_schema_floor: 0,
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
            evidence: None,
            intent_id: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00,
            issuer: node(1),
            cell_epoch: CellEpoch::new(7),
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
            evidence: None,
            intent_id: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00,
            issuer: node(1),
            cell_epoch: CellEpoch::new(7),
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
        // D29 clause 6's presence tag, and the reason a bare `Option` encoding
        // would not do: without a discriminator byte, "no commitment" and "a
        // commitment of 124 zero bytes" produce the same preimage, and a
        // submitter could strip a signed commitment off an intent and still
        // present a verifying signature — turning a finalizable intent into an
        // unfinalizable one, which is exactly the after-the-fact substitution
        // the commitment exists to prevent.
        expected.push(0);
        assert_eq!(before, expected, "canonical layout, derived by hand");

        // And the present case: the same intent carrying a commitment signs
        // over different bytes, so the two are not interchangeable.
        let mut with_evidence = intent.clone();
        with_evidence.evidence = Some(crate::EvidenceCommitment {
            ruleset: crate::RulesetId {
                version: 1,
                digest: [7; 32],
            },
            entity: PersistId::new(11),
            window_start: Tick::new(100),
            window_end: Tick::new(160),
            t0_claim_hash: [9; 32],
            log_head: crate::ChainHash([5; 32]),
        });
        let committed = with_evidence.signing_preimage();
        assert_ne!(before, committed);
        assert_eq!(
            committed.len(),
            before.len() + crate::EVIDENCE_COMMITMENT_PREIMAGE_LEN,
            "the tag is already in `before`; the present case adds the body"
        );
        assert_eq!(committed[before.len() - 1], 1, "the presence tag flips");
    }

    #[test]
    fn signature_roundtrips() {
        let mut intent = Intent {
            evidence: None,
            intent_id: 42,
            issuer: node(1),
            cell_epoch: CellEpoch::new(0),
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
    fn attestation_preimage_matches_d27_and_separates_signature_roles() {
        let issuer = key(1);
        let witness = key(2);
        let mut intent = intent_for_test(42, issuer.public());
        intent.sign(&issuer);

        let attestation = intent.attest(&witness);
        let preimage = intent.attestation_preimage(witness.public());
        assert_eq!(preimage.len(), ATTESTATION_PREIMAGE_LEN);

        let intent_hash = blake3::hash(&intent.signing_preimage());
        let mut expected = [0_u8; ATTESTATION_PREIMAGE_LEN];
        expected[..21].copy_from_slice(ATTESTATION_PREIMAGE_TAG);
        expected[21..53].copy_from_slice(intent_hash.as_bytes());
        expected[53..61].copy_from_slice(&intent.cell_epoch.0.to_le_bytes());
        expected[61..125].copy_from_slice(&intent.signature.to_bytes());
        expected[125..157].copy_from_slice(witness.public().as_bytes());
        assert_eq!(
            preimage, expected,
            "D27's offset table is transcribed exactly"
        );

        assert!(attestation.verify(&intent));
        assert!(
            witness
                .public()
                .verify(&intent.signing_preimage(), &attestation.signature)
                .is_err(),
            "a witness signature must not verify over the issuer preimage"
        );
    }

    #[test]
    fn attestation_is_bound_to_the_issuer_signature_even_when_id_is_reused() {
        let issuer = key(1);
        let witness = key(2);
        let mut original = intent_for_test(77, issuer.public());
        original.sign(&issuer);
        let attestation = original.attest(&witness);
        assert!(attestation.verify(&original));

        let mut resigned = original.clone();
        resigned.signature = key(3).sign(&resigned.signing_preimage());
        assert_eq!(resigned.intent_id, original.intent_id);
        assert_ne!(resigned.signature, original.signature);
        assert!(
            !attestation.verify(&resigned),
            "offset 61 commits the attestation to the issuer signature"
        );
    }

    fn key(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    fn witnesses(count: u8) -> Vec<NodeId> {
        (1..=count).map(|seed| key(seed + 100).public()).collect()
    }

    #[test]
    fn the_draw_is_deterministic_and_moves_with_the_key_and_the_intent() {
        let eligible = witnesses(7);
        let draw_key = [3u8; 32];

        let required = required_witnesses(&draw_key, 42, &eligible);
        assert_eq!(required.len(), WITNESS_QUORUM_K);
        assert_eq!(
            required,
            required_witnesses(&draw_key, 42, &eligible),
            "the same key, id and eligible vector must always name the same K"
        );
        for witness in &required {
            assert!(
                eligible.contains(witness),
                "the draw never invents a member"
            );
        }
        let mut distinct = required.clone();
        distinct.sort_by_key(|node| *node.as_bytes());
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            WITNESS_QUORUM_K,
            "no witness is drawn twice"
        );

        // The two inputs an attacker would want to control, each moving the
        // answer on its own. Without the first, a submitter grinds `intent_id`
        // offline until the required slots land on its colluders; without the
        // second, one epoch's observed draws would predict the next.
        assert_ne!(
            required_witnesses(&draw_key, 43, &eligible),
            required,
            "a different intent id draws a different subset"
        );
        assert_ne!(
            required_witnesses(&[4u8; 32], 42, &eligible),
            required,
            "a different draw key draws a different subset"
        );
    }

    #[test]
    fn the_draw_is_a_function_of_the_set_and_not_of_its_order() {
        // Announced order is preserved in `E(I)` because the recorded vector
        // has to be the object an auditor draws over — but the *result* must
        // not depend on it, or a gateway and its auditor listing the same set
        // differently would disagree about who was required.
        let eligible = witnesses(7);
        let mut reversed = eligible.clone();
        reversed.reverse();

        let mut from_announced = required_witnesses(&[5u8; 32], 9, &eligible);
        let mut from_reversed = required_witnesses(&[5u8; 32], 9, &reversed);
        from_announced.sort_by_key(|node| *node.as_bytes());
        from_reversed.sort_by_key(|node| *node.as_bytes());
        assert_eq!(from_announced, from_reversed);
    }

    #[test]
    fn a_short_eligible_vector_yields_fewer_than_k_rather_than_a_smaller_quorum() {
        // The caller's obligation, stated as a test so nobody reads a
        // two-element answer as "a 2-of-N quorum": D27 makes no draw at all
        // below the floor, and this function reports what it has rather than
        // padding or panicking.
        let two = witnesses(2);
        assert_eq!(required_witnesses(&[1u8; 32], 1, &two).len(), 2);
        assert!(required_witnesses(&[1u8; 32], 1, &[]).is_empty());
    }

    #[test]
    fn eligible_witnesses_drops_the_issuer_and_keeps_announced_order() {
        let announced = witnesses(5);
        let issuer = announced[2];

        let eligible = eligible_witnesses(&announced, issuer);
        assert_eq!(eligible.len(), 4, "the issuer is the one party we can see");
        assert!(!eligible.contains(&issuer));
        assert_eq!(
            eligible,
            vec![announced[0], announced[1], announced[3], announced[4]],
            "announced order survives the filter"
        );

        // An issuer outside the announced set removes nobody — the ordinary
        // case, and the one that must not silently shrink the eligible vector.
        assert_eq!(
            eligible_witnesses(&announced, key(200).public()),
            announced,
            "an issuer that is not in the set is not a member to remove"
        );
    }

    fn intent_for_test(intent_id: u128, issuer: NodeId) -> Intent {
        Intent {
            evidence: None,
            intent_id,
            issuer,
            cell_epoch: CellEpoch::new(9),
            ops: vec![IntentOp {
                op: 4,
                args: bytes::Bytes::from_static(b"transfer"),
            }],
            attestations: Vec::new(),
            signature: key(9).sign(b"placeholder"),
        }
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
