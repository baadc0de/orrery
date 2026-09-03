//! Verifiable-core wire types (D9, docs/06-verifiable-core.md §6).
//!
//! Every authority keeps a PeerReview-style tamper-evident log for each core
//! entity it holds. These are the types that cross the wire; the folding,
//! signing, verification and replay logic lives in `orrery_core`, which is
//! engine-agnostic and links identically into peers, field hosts and
//! `persistd`.
//!
//! The split matters: a witness must be able to check a log without linking a
//! game's `Ruleset`, and `orrery_protocol` is the one crate everything already
//! depends on.
//!
//! **Deferred from the §6 sketch, and marked where they belong:** the
//! `GeometryFrame`, `FieldFrame`, `FrameChange` and `TerrainPromotion` record
//! sources. Each closes replay over a subsystem that does not exist yet
//! (mutable terrain, environmental fields, nested-grid migration, terrain↔entity
//! promotion). Adding one later is additive on [`RecordSource`]; leaving a
//! half-specified variant in place would let a replay claim closure it does
//! not have.

use serde::{Deserialize, Serialize};

use crate::{Attestation, Intent, NodeId, PersistId, Signature, Tick};

/// A blake3 chain hash over a log's records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChainHash(pub [u8; 32]);

impl ChainHash {
    /// The head of an empty chain.
    pub const EMPTY: Self = Self([0; 32]);

    /// The truncated head that travels on the wire for gap detection.
    #[must_use]
    pub fn rolling(self) -> RollingHead {
        let mut head = [0u8; 8];
        head.copy_from_slice(&self.0[..8]);
        RollingHead(head)
    }
}

/// A truncated [`ChainHash`] — **gap detection only**, never proof.
///
/// Frames carry these because a full head per entity per send is bandwidth a
/// 20 Hz link should not spend. A receiver recomputes the full 32-byte head by
/// folding the records it was given; the frame signature and the 2 Hz
/// [`StateClaim`] both commit to the full head, so nothing is ever *proven*
/// against eight bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RollingHead(pub [u8; 8]);

/// Ruleset version identity: pinned into handshakes, log frames, state claims
/// and evidence bundles (docs/06 §3).
///
/// `version` is the game-assigned monotonic rules version; `digest` is the
/// 32-byte build digest. Adjudication routes a bundle to the matching build
/// and answers `Unadjudicable` rather than guessing when it has none (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RulesetId {
    /// Game-assigned monotonic rules version.
    pub version: u32,
    /// 32-byte build digest.
    pub digest: [u8; 32],
}

/// The per-universe randomness key (VC-3).
///
/// Every `TickRng` is derived from this by a keyed hash over
/// `entity ‖ absolute tick`, so a replay anywhere reproduces the same draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniverseSeed(pub [u8; 32]);

/// The blake3 `derive_key` context for [`UniverseSeed::fingerprint`].
///
/// Domain separation is the whole safety argument: this context appears
/// nowhere else, so a fingerprint cannot be replayed as, or confused with, a
/// seed-tree key (`orrery.seeder.v1`) or any other derived material.
pub const UNIVERSE_SEED_FINGERPRINT_CONTEXT: &str = "orrery.universe-seed-fingerprint.v1";

/// A one-way, domain-separated fingerprint of a [`UniverseSeed`].
///
/// It exists so a durable record can say *which* universe it belongs to
/// without ever holding the universe's secret. 128 bits is a name, not a
/// secret: it is published in `content/version` and in the seeding manifest,
/// and inverting it to the seed would mean inverting blake3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UniverseSeedFingerprint(pub [u8; 16]);

impl UniverseSeedFingerprint {
    /// The 32 hexadecimal characters an operator types or reads.
    #[must_use]
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse the 32-hexadecimal-character form.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when `text` is not exactly 32
    /// hexadecimal characters.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.len() != 32 {
            return Err(format!(
                "expected a 16-byte universe seed fingerprint as 32 hexadecimal characters, got {} characters",
                text.len()
            ));
        }
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|_| format!("invalid hexadecimal digit at byte {i} in fingerprint"))?;
        }
        Ok(Self(out))
    }
}

impl core::fmt::Display for UniverseSeedFingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl UniverseSeed {
    /// The published name of the universe this seed opens.
    ///
    /// `blake3::derive_key(UNIVERSE_SEED_FINGERPRINT_CONTEXT, seed)`, truncated
    /// to its first 16 bytes. One-way and domain-separated: what lands in the
    /// keyspace names the universe, and the seed itself never does.
    #[must_use]
    pub fn fingerprint(&self) -> UniverseSeedFingerprint {
        let derived = blake3::derive_key(UNIVERSE_SEED_FINGERPRINT_CONTEXT, &self.0);
        let mut out = [0u8; 16];
        out.copy_from_slice(&derived[..16]);
        UniverseSeedFingerprint(out)
    }
}

/// Where one logged input came from (docs/06 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordSource {
    /// A command from the **frame's own signer** — the overwhelmingly common
    /// case, and the reason it has a variant of its own.
    ///
    /// An authority logs its own player's inputs every tick, and repeating a
    /// 32-byte public key on each of them is the single largest avoidable cost
    /// in the witness stream: at 60 Hz across a seven-link witness set it is
    /// ~13 kB/s of key material per peer, and it inflates every gap repair that
    /// carries those records.
    ///
    /// Attribution is not weakened. The signer is bound by the frame signature
    /// already, so "whose input" is answered by the same thing that answers
    /// "who wrote this frame" — which is strictly *harder* to equivocate over
    /// than a per-record field would be.
    OwnPlayer {
        /// The signer's monotonic input sequence, checked for legality on
        /// replay but never used to re-sort (VC-2).
        input_seq: u32,
    },
    /// A command from some peer other than the frame's signer.
    Player {
        /// The commanding peer.
        node: NodeId,
        /// That peer's monotonic input sequence, checked for legality on
        /// replay but never used to re-sort (VC-2).
        input_seq: u32,
    },
    /// A `CoreEvent` emitted by another entity's step at the previous tick.
    ///
    /// Cross-entity effects travel only this way, which is what keeps each
    /// entity's replay self-contained.
    InboundEvent {
        /// The emitting entity.
        from: PersistId,
    },
    /// The quantized neighbour fields `StateView` actually read this tick.
    ///
    /// Recording the read — not just the neighbour's identity — is what closes
    /// the input set: replay never needs the neighbour's live state, and an
    /// authority that feeds itself fabricated neighbour state to justify an
    /// outcome produces checkable evidence against itself.
    NeighborFrame {
        /// The neighbour whose fields were read.
        neighbor: PersistId,
        /// Whether the lookup returned state.
        ///
        /// An absent lookup is still part of the replay closure: omitting it
        /// would let a dropped frame masquerade as an honest `None` result.
        present: bool,
        /// The neighbour-state tick the reader actually observed.
        ///
        /// This is deliberately not the reader's tick: replication lag is
        /// ordinary, and cross-checking the payload against a claim from a
        /// newer tick would manufacture evidence against an honest peer.
        observed_tick: Tick,
    },
    /// Chain-epoch boundary on authority handoff, binding the new chain to the
    /// old head and the registrar's lease sequence (docs/06 §9).
    AuthorityChange {
        /// Head of the previous authority's chain.
        prev_head: ChainHash,
        /// The registrar lease sequence this authority holds.
        lease_seq: u64,
    },
}

/// One logged input, in the total order the authority fixed (VC-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRecord {
    /// Offset from the frame's shared tick base. Sparse: only ticks with
    /// activity appear at all.
    pub tick_off: u16,
    /// Position within this tick's total order.
    pub seq: u16,
    /// Where the input came from.
    pub source: RecordSource,
    /// Canonical `CoreCodec` bytes.
    pub payload: bytes::Bytes,
}

/// One entity's slice of a [`LogFrame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntitySlice {
    /// The entity these records belong to.
    pub entity: PersistId,
    /// Increments on authority handoff; adjudication windows never span one.
    pub chain_epoch: u32,
    /// Chain head before these records.
    pub prev_head: RollingHead,
    /// Records, with tick base `LogFrame::first_tick`.
    pub records: Vec<InputRecord>,
    /// Chain head after folding `records`.
    pub head: RollingHead,
}

/// One signed frame per send per link, covering every core entity the sender
/// holds authority over (docs/06 §6).
///
/// One signature per *frame*, not per entity or per record: the preimage
/// covers the ruleset, the tick range, and every entity's full 32-byte
/// `(prev_head, head)` pair, so signing cost is flat in entity count. A
/// verifier folds the records to recompute those heads before checking the
/// signature, which is what makes the truncated wire heads sufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFrame {
    /// The rules build in force; fixed per session at handshake.
    pub ruleset: RulesetId,
    /// First tick covered.
    pub first_tick: Tick,
    /// Half-open: covers `[first_tick, first_tick + tick_count)`.
    pub tick_count: u16,
    /// Per-entity slices.
    pub entities: Vec<EntitySlice>,
    /// Ed25519 over the frame preimage, made with the authority's transport
    /// key — iroh `NodeId`s are ed25519 public keys, so log signatures need no
    /// PKI beyond the one peers already dial with (D3).
    pub sig: Signature,
}

/// A periodic commitment to quantized core state (docs/06 §6).
///
/// Claims are hashes, not snapshots: the authority retains the snapshot within
/// its retention window and serves it on request, and this hash is what proves
/// the served bytes are the ones it committed to at the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateClaim {
    /// The entity being claimed for.
    pub entity: PersistId,
    /// The chain epoch this claim sits in.
    pub chain_epoch: u32,
    /// The tick claimed.
    pub tick: Tick,
    /// Full input-chain head at `tick`.
    pub input_head: ChainHash,
    /// blake3 over the canonical quantized `CoreState`.
    pub state_hash: [u8; 32],
    /// Hash of the previous claim, chaining claims as well as inputs.
    pub prev_claim: [u8; 32],
    /// The rules build in force.
    pub ruleset: RulesetId,
    /// Ed25519 over the claim preimage.
    pub sig: Signature,
}

/// A self-verifying dispute: everything needed to reach a verdict without
/// trusting whoever assembled it (docs/06 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    /// The rules build the window was executed under.
    pub ruleset: RulesetId,
    /// The disputed entity.
    pub entity: PersistId,
    /// Half-open window, at most [`MAX_ADJUDICATION_TICKS`], ending at a claim
    /// tick.
    pub window_start: Tick,
    /// Exclusive end of the window.
    pub window_end: Tick,
    /// The claim the window starts from.
    pub t0_claim: StateClaim,
    /// The snapshot that claim commits to. Mandatory: without it there is
    /// nothing to start replay from, and its hash must match the claim.
    pub t0_snapshot: bytes::Bytes,
    /// Contiguous frames for `entity` across the window.
    pub frames: Vec<LogFrame>,
    /// Full `(prev_head, head)` pairs for each frame's *other* entities, in
    /// frame order, reconstructing each frame's signature preimage.
    ///
    /// The reporter folded those chains too; without these a verifier could
    /// not rebuild the preimage of a multi-entity frame and would have to take
    /// the signature on trust.
    pub sibling_heads: Vec<Vec<(ChainHash, ChainHash)>>,
    /// What the authority signed across the window.
    pub disputed_claims: Vec<StateClaim>,
    /// A per-tick trajectory the reporter supplies, **advisory only**.
    ///
    /// The subject never signs this, so no verdict may rest on it: doing so
    /// would let a reporter convict an honest peer by inventing numbers. It
    /// exists so an adjudicator can jump straight to the neighbourhood of a
    /// divergence instead of scanning. What the subject *is* held to is
    /// `disputed_claims`, which carry its signature.
    pub claimed_hashes: Vec<[u8; 32]>,
    /// The reporter's own re-execution, also advisory, for the same reason.
    pub computed_hashes: Vec<[u8; 32]>,
}

/// The fixed-size commitment a low-population intent carries **instead of** an
/// [`EvidenceBundle`] (D29 clause 6).
///
/// # Why a commitment and not the bundle
///
/// `docs/07 §4.5` asks for "full evidence attached (submitter's log segment
/// covering the intent window, `t₀` claim)". That is an [`EvidenceBundle`],
/// which carries a `t0_snapshot`, up to [`MAX_ADJUDICATION_TICKS`] frames,
/// every frame's sibling heads and the disputed claims — unbounded in
/// principle and routinely past FoundationDB's 100 KB value limit. Attaching
/// it would mean sharding a blob across rows inside the one transaction whose
/// p99 budget is 10 ms.
///
/// So the intent path carries this instead — 108 bytes, fixed — and the
/// finalizer fetches the bundle out of band at replay time. The property the
/// intent path actually needed from "attached evidence" is that the submitter
/// **cannot substitute a friendlier history after the fact**, and that is a
/// property of committing early, not of storing much: a bundle the finalizer
/// later receives either reproduces every field here or is refused.
///
/// # The five fields, and what each one pins
///
/// | field | pins |
/// |---|---|
/// | `ruleset` | which retained build replays the window — never "the current one" |
/// | `entity` | whose history is being replayed |
/// | `window_start` / `window_end` | the half-open `[t₀, t_intent)` span |
/// | `t0_claim_hash` | the signed claim replay starts from |
/// | `log_head` | the submitter's chain head, so the segment cannot be re-cut |
///
/// # It is inside the issuer's signing preimage
///
/// [`Intent::signing_preimage`](crate::Intent::signing_preimage) covers this
/// field, so the commitment is the submitter's own signed statement about the
/// history it is asking the cluster to accept. D29 clause 6 only requires the
/// commitment to be *stored*; signing it is strictly stronger and costs
/// nothing, because the wire break that admits the field is already being
/// taken. A commitment the submitter could disown would give the finalizer
/// nothing to hold it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCommitment {
    /// The rules build the window was executed under, and the build the
    /// finalizer must route to.
    pub ruleset: RulesetId,
    /// The entity whose history the window covers.
    pub entity: PersistId,
    /// First tick of the half-open replay window.
    pub window_start: Tick,
    /// Exclusive end of the replay window — the intent's own tick.
    pub window_end: Tick,
    /// blake3 over the canonical preimage of the `t₀` [`StateClaim`] replay
    /// starts from.
    pub t0_claim_hash: [u8; 32],
    /// The submitter's full input-chain head at `window_end`.
    pub log_head: ChainHash,
}

/// The byte width of [`EvidenceCommitment::preimage`].
///
/// `4 + 32` ruleset, `8` entity, `8 + 8` window, `32` claim hash, `32` head.
pub const EVIDENCE_COMMITMENT_PREIMAGE_LEN: usize = 124;

impl EvidenceCommitment {
    /// The canonical fixed-width byte string this commitment contributes to
    /// [`Intent::signing_preimage`](crate::Intent::signing_preimage).
    ///
    /// Fixed-width and little-endian throughout, with no length prefixes,
    /// for the same reason the attestation preimage is: an encoding whose
    /// field offsets are constants is an encoding two implementations cannot
    /// disagree about.
    #[must_use]
    pub fn preimage(&self) -> [u8; EVIDENCE_COMMITMENT_PREIMAGE_LEN] {
        let mut buf = [0_u8; EVIDENCE_COMMITMENT_PREIMAGE_LEN];
        buf[0..4].copy_from_slice(&self.ruleset.version.to_le_bytes());
        buf[4..36].copy_from_slice(&self.ruleset.digest);
        buf[36..44].copy_from_slice(&self.entity.0.to_le_bytes());
        buf[44..52].copy_from_slice(&self.window_start.0.to_le_bytes());
        buf[52..60].copy_from_slice(&self.window_end.0.to_le_bytes());
        buf[60..92].copy_from_slice(&self.t0_claim_hash);
        buf[92..124].copy_from_slice(&self.log_head.0);
        buf
    }

    /// The commitment an [`EvidenceBundle`] *would* have produced, so a
    /// finalizer can compare the bundle it fetched against the one the intent
    /// pinned.
    ///
    /// Two of the six fields are supplied rather than read out of the bundle,
    /// and both for the same reason — this crate cannot compute them.
    /// `t0_claim_hash` is `orrery_core::claim_hash`, which lives above this
    /// crate in the dependency order, and `log_head` is the submitter's chain
    /// head, which the bundle's frames advance *toward* rather than state, so
    /// it comes from the fold the fetcher just performed. Everything else
    /// comes out of the bundle itself, which is what makes the comparison a
    /// check on the *evidence* rather than on the fetcher's bookkeeping.
    #[must_use]
    pub fn from_bundle(
        bundle: &EvidenceBundle,
        t0_claim_hash: [u8; 32],
        log_head: ChainHash,
    ) -> Self {
        Self {
            ruleset: bundle.ruleset,
            entity: bundle.entity,
            window_start: bundle.window_start,
            window_end: bundle.window_end,
            t0_claim_hash,
            log_head,
        }
    }
}

/// A dispute filed at stage 3 (docs/07-witnessing.md §3).
///
/// The bundle inside is self-verifying, so an adjudicator needs nothing from
/// the reporter but this. The extra fields are the two things a bundle cannot
/// carry about itself: **who is accused**, which the adjudicator needs to know
/// whose key the signatures should verify under, and **who is reporting**,
/// which is what makes a fabricated report attributable to an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscrepancyReport {
    /// The accused authority. Signatures in the bundle must verify under this
    /// key, and a bundle that does not is forgery by the reporter.
    pub subject: NodeId,
    /// The self-verifying evidence.
    pub bundle: EvidenceBundle,
    /// The reporter's own signature over the bundle, binding the accusation to
    /// a strikeable account (D12).
    pub reporter: NodeId,
    /// Ed25519 over the reporter preimage.
    pub reporter_sig: Signature,
}

/// A witness asking an authority to fill a gap in its chain (docs/06 §6).
///
/// Datagram loss is expected, so a receiver detecting a rolling-head mismatch
/// repairs it over the reliable control lane rather than treating it as an
/// accusation. Refusal or timeout *is* reportable, but that is a separate
/// judgement from a missing packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRangeRequest {
    /// The entity whose chain has a gap.
    pub entity: PersistId,
    /// The chain epoch the gap is in; a request never spans an epoch.
    pub chain_epoch: u32,
    /// First tick the requester is missing.
    pub from_tick: Tick,
    /// Exclusive end of the missing range.
    pub to_tick: Tick,
}

/// The authority's answer to a [`LogRangeRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRangeResponse {
    /// The entity the frames are for.
    pub entity: PersistId,
    /// The frames covering the requested range, in tick order. Empty means the
    /// authority cannot serve it — retention, or refusal.
    pub frames: Vec<LogFrame>,
}

/// The adjudication window ceiling: 3 s at 60 Hz (D16).
pub const MAX_ADJUDICATION_TICKS: u64 = 180;

/// One entity's full head transition, sent alongside a [`LogFrame`].
///
/// A frame's signature commits to every entity's *full* 32-byte
/// `(prev_head, head)` pair, but the slices carry only [`RollingHead`]s. A
/// receiver recomputes the full pair by folding — which it can only do for
/// entities whose history it has been following. For the rest it has to be
/// told, or it cannot rebuild the preimage and check the signature at all.
///
/// Supplying these is not a trust concession. A receiver uses its own fold for
/// anything it is following and ignores the sender's version, and a sender that
/// lies about the rest only makes its own frame fail to verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameHead {
    /// The entity this pair belongs to.
    pub entity: PersistId,
    /// Full chain head before the frame's records.
    pub prev_head: ChainHash,
    /// Full chain head after them.
    pub head: ChainHash,
}

/// One signed-log position a witness may consult while judging an intent.
///
/// These are routing hints into the witness's already-replicated view, not
/// additional claims and not additional signed fields. D27 fixes the
/// attestation preimage to the enclosing [`Intent`], so a game that needs a
/// context position to be binding must also encode it in the intent op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentContextRef {
    /// The persistent entity whose witnessed history carries the context.
    pub entity: PersistId,
    /// The universe tick to inspect in that history.
    pub tick: Tick,
}

/// A signed intent offered to one member of its announced witness set.
///
/// `parties` lets an honest witness perform D10's mandatory self-exclusion.
/// The gateway still derives and checks the authoritative party set: this
/// peer-carried list is protective for an honest witness, not trusted input to
/// durable admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentProposal {
    /// The issuer-signed intent, before witness attestations are appended.
    pub intent: Intent,
    /// Every party NodeId known to the submitter, including the issuer.
    pub parties: Vec<NodeId>,
    /// Positions in the already-streamed log that may help judge plausibility.
    pub context_refs: Vec<IntentContextRef>,
}

/// Why a witness explicitly declined an [`IntentProposal`].
///
/// A refusal is a received answer. It is deliberately a different wire value
/// from silence, because the submitter must distinguish a negative judgement
/// from an unreachable witness when the 150 ms budget closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttestationRefusalReason {
    /// This witness is the issuer or appears in the proposal's party set.
    Party,
    /// The issuer signature does not verify over [`Intent::signing_preimage`].
    BadIssuerSignature,
    /// The peer has no witness signing identity configured.
    MissingSigningIdentity,
    /// A game-defined plausibility precondition failed in the replicated view.
    PlausibilityFailed(u16),
    /// This witness already judged a conflicting intent in the same epoch.
    ConflictingIntent,
}

/// One witness's explicit answer to an [`IntentProposal`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttestationVerdict {
    /// The witness signed D27's attestation preimage.
    Attested(Attestation),
    /// The witness declined and supplied a machine-readable reason.
    Refused(AttestationRefusalReason),
}

/// A witness answer routed back to the intent submitter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentResponse {
    /// The proposal's intent id, used to route concurrent collections.
    pub intent_id: u128,
    /// The positive attestation or explicit refusal.
    pub verdict: AttestationVerdict,
}

/// Verifiable-core traffic between peers (docs/06 §6, docs/07 §3).
///
/// # Which lane each rides
///
/// [`Self::Frame`] and [`Self::Claim`] go on `Channel::State` with replication:
/// they are the steady 20 Hz stream, and a lost one is repaired by
/// [`Self::RangeRequest`] rather than retransmitted. Everything else rides
/// `Channel::Control`, because a repair that could itself be dropped would turn
/// one lost datagram into a permanent hole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WitnessMsg {
    /// A signed frame, with the full head pairs a partial follower needs.
    Frame {
        /// The frame.
        frame: LogFrame,
        /// Full head pairs for the frame's entities. A receiver uses its own
        /// fold where it has one.
        heads: Vec<FrameHead>,
    },
    /// A periodic signed commitment to quantized state.
    Claim(StateClaim),
    /// Fill a hole in a chain. Never an accusation.
    RangeRequest(LogRangeRequest),
    /// Frames answering a [`Self::RangeRequest`], with their head pairs.
    ///
    /// May be **partial**: an authority serves what fits in one packet and the
    /// requester asks again for the remainder. A 180-tick window does not fit
    /// in an MTU, and silently truncating would leave the requester believing
    /// the gap was unfillable.
    RangeResponse {
        /// The frames, in tick order. Empty means the authority cannot serve
        /// the range — retention, or refusal.
        response: LogRangeResponse,
        /// Full head pairs for every entity in `response.frames`.
        heads: Vec<FrameHead>,
        /// First tick still missing after these frames, if any. `None` means
        /// the request was served completely.
        resume_from: Option<Tick>,
    },
    /// A self-verifying accusation, bound to its reporter.
    Report(Box<DiscrepancyReport>),
    /// Ask one announced, non-party witness to judge a signed intent.
    IntentProposal(Box<IntentProposal>),
    /// Return an attestation or an explicit refusal to the submitter.
    IntentResponse(IntentResponse),
}

/// How a claimed trajectory failed against a replayed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviationKind {
    /// Integer or fixed-point state differed. Bit-exact by construction, so
    /// any difference at all is a deviation (VC-5).
    DiscreteMismatch,
    /// Continuous state left the tolerance band for long enough to count
    /// (docs/06 §5).
    ContinuousOutOfBand,
}

/// Why an adjudicator could not decide.
///
/// None of these is ever a strike: bogus-report pressure is absorbed by
/// per-account rate limits, not by punishing reporters for gaps the cluster
/// owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnadjudicableReason {
    /// No retained build matches the bundle's `RulesetId` (D11 retains 3).
    UnknownRuleset,
    /// The window exceeds [`MAX_ADJUDICATION_TICKS`], or is empty.
    WindowOutOfRange,
    /// Frames are missing, out of order, or do not chain.
    IncompleteChain,
    /// Structurally malformed in a way that is not provably fabricated.
    Malformed,
}

/// Proof that a reporter fabricated its evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeryProof {
    /// A frame signature the reporter presented as the authority's does not
    /// verify under that authority's key.
    FrameSignatureInvalid,
    /// A claim signature does not verify under the authority's key.
    ClaimSignatureInvalid,
    /// The supplied `t0` snapshot does not hash to what the claim commits to.
    SnapshotHashMismatch,
    /// The bundle does not reproduce the [`EvidenceCommitment`] the intent was
    /// committed under (D29 clause 6).
    ///
    /// The odd one out, and deliberately so: every other arm names a signature
    /// or a hash that failed to check. Nothing failed to check here — the
    /// signatures on the presented history are perfectly good signatures over a
    /// *different* history. What is proven is substitution, which is only
    /// provable because the submitter pinned the history before it knew what
    /// the cluster would ask for.
    ///
    /// It strikes the submitter rather than a reporter, because on the
    /// provisional-finalization path the evidence's author and the intent's
    /// submitter are the same account.
    CommitmentMismatch,
}

/// The outcome of adjudicating an [`EvidenceBundle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Deviation proven, with the first offending tick.
    Confirms {
        /// First tick at which the claim and the re-execution disagree.
        at: Tick,
        /// How they disagreed.
        kind: DeviationKind,
    },
    /// Re-execution matches the claims within bands. No strike — this feeds
    /// the ε calibration telemetry that has to converge before enforcement
    /// leaves shadow mode (D17 risk 3).
    Exonerates,
    /// The reporter fabricated evidence. Strikes the *reporter*.
    EvidenceForged(ForgeryProof),
    /// Undecidable. Never a strike.
    Unadjudicable(UnadjudicableReason),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_names_its_seed_and_only_its_seed() {
        let a = UniverseSeed([0x11; 32]);
        let b = UniverseSeed([0x12; 32]);
        assert_eq!(
            a.fingerprint(),
            UniverseSeed([0x11; 32]).fingerprint(),
            "the name of a universe must not depend on when it was asked for"
        );
        assert_ne!(
            a.fingerprint(),
            b.fingerprint(),
            "two universes must not share a name"
        );
    }

    #[test]
    fn a_fingerprint_discloses_no_part_of_its_seed() {
        // The carve-out this type exists under (docs/08 §6) is that a
        // *fingerprint* may live in the keyspace and the seed may not. That
        // holds only if the fingerprint is not the seed with a haircut.
        let seed = UniverseSeed([0x5a; 32]);
        let fingerprint = seed.fingerprint();
        assert_ne!(
            fingerprint.0.as_slice(),
            &seed.0[..16],
            "a truncation is not a one-way function"
        );
        let undomained: [u8; 16] = blake3::hash(&seed.0).as_bytes()[..16]
            .try_into()
            .expect("16 bytes");
        assert_ne!(
            fingerprint.0, undomained,
            "an undomained hash would collide with every other use of blake3 on this seed"
        );
    }

    #[test]
    fn a_fingerprint_round_trips_through_the_form_an_operator_types() {
        let fingerprint = UniverseSeed([0x77; 32]).fingerprint();
        let hex = fingerprint.to_hex();
        assert_eq!(hex.len(), 32, "32 hexadecimal characters, not 64");
        assert_eq!(hex, fingerprint.to_string());
        assert_eq!(
            UniverseSeedFingerprint::from_hex(&hex).expect("parses"),
            fingerprint
        );
        assert!(UniverseSeedFingerprint::from_hex("beef").is_err());
        assert!(UniverseSeedFingerprint::from_hex(&"z".repeat(32)).is_err());
    }

    fn node(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh_base::SecretKey::from_bytes(&bytes).public()
    }

    #[test]
    fn a_rolling_head_is_the_prefix_of_its_full_hash() {
        // The wire carries eight bytes for gap detection; a verifier folds to
        // recompute the full head. If these ever disagree, a receiver would
        // report phantom gaps against a chain that is actually intact.
        let full = ChainHash([7; 32]);
        assert_eq!(full.rolling().0, [7u8; 8]);
        assert_eq!(ChainHash::EMPTY.rolling(), RollingHead([0; 8]));
    }

    #[test]
    fn log_frame_roundtrips() {
        let frame = LogFrame {
            ruleset: RulesetId {
                version: 3,
                digest: [9; 32],
            },
            first_tick: Tick::new(1_000),
            tick_count: 3,
            entities: vec![EntitySlice {
                entity: PersistId::new(42),
                chain_epoch: 1,
                prev_head: RollingHead([1; 8]),
                records: vec![InputRecord {
                    tick_off: 2,
                    seq: 0,
                    source: RecordSource::Player {
                        node: node(1),
                        input_seq: 17,
                    },
                    payload: bytes::Bytes::from_static(b"move"),
                }],
                head: RollingHead([2; 8]),
            }],
            sig: iroh_base::SecretKey::from_bytes(&[3; 32]).sign(b"preimage"),
        };
        let bytes = postcard::to_stdvec(&frame).unwrap();
        assert_eq!(postcard::from_bytes::<LogFrame>(&bytes).unwrap(), frame);
        // A truncated frame must not decode into something half-formed.
        assert!(postcard::from_bytes::<LogFrame>(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn state_claim_roundtrips() {
        let claim = StateClaim {
            entity: PersistId::new(7),
            chain_epoch: 2,
            tick: Tick::new(600),
            input_head: ChainHash([4; 32]),
            state_hash: [5; 32],
            prev_claim: [6; 32],
            ruleset: RulesetId {
                version: 1,
                digest: [0; 32],
            },
            sig: iroh_base::SecretKey::from_bytes(&[3; 32]).sign(b"preimage"),
        };
        let bytes = postcard::to_stdvec(&claim).unwrap();
        assert_eq!(postcard::from_bytes::<StateClaim>(&bytes).unwrap(), claim);
    }

    #[test]
    fn verdict_variants_roundtrip() {
        // The verdict crosses the wire back to the reporter and into the
        // strike ledger, so every arm has to survive the trip.
        for verdict in [
            Verdict::Confirms {
                at: Tick::new(12),
                kind: DeviationKind::DiscreteMismatch,
            },
            Verdict::Exonerates,
            Verdict::EvidenceForged(ForgeryProof::SnapshotHashMismatch),
            Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset),
        ] {
            let bytes = postcard::to_stdvec(&verdict).unwrap();
            assert_eq!(postcard::from_bytes::<Verdict>(&bytes).unwrap(), verdict);
        }
    }
}
