//! The intent execution path (D11 §2.2, §7; docs/11-roadmap.md §P2).
//!
//! An intent is the only path for durable consequences (trades, currency,
//! progression). This module defines the two seams the gateway wires together:
//!
//! - [`IntentValidator`] — the **admission filter**: a synchronous,
//!   `Ruleset`-linked check run before any transaction work. P2 ships a
//!   permissive default ([`PermissiveValidator`]) so the harness runs
//!   unconfigured; the `Ruleset` semantics themselves are P2's linked
//!   `Ruleset` stub (docs/11-roadmap.md §P2: "without witness attestation").
//! - [`IntentExecutor`] — the **authority**: executes the intent inside one
//!   serializable transaction and returns the outcome. The FDB-backed
//!   implementation ([`fdb`]) reads the `intent/{intent_id}` idempotency row
//!   first, so a replayed intent returns its recorded outcome instead of
//!   applying twice; [`MemIntentExecutor`] provides the same contract in
//!   memory for tests.
//!
//! The gateway ([`crate::gateway::route_intent`]) orders the checks: verify
//! the issuer signature, bind `intent.issuer` to the connection's
//! authenticated id, run the validator, and only then hand the intent to the
//! executor — sending the ack **after** the executor's future resolves, so a
//! `Committed` ack implies a durable commit (RPO 0, D11).

use std::collections::HashMap;
use std::sync::Arc;

use orrery_protocol::{
    AccountId, AssetId, Intent, IntentOutcome, NodeId, SessionStanding, REASON_CONTENTION_EXHAUSTED,
};

#[cfg(feature = "fdb")]
mod fdb;
#[cfg(feature = "fdb")]
pub use fdb::{FdbIntentExecutor, IntentFence};

pub mod provisional;
pub use provisional::{
    EvidenceSource, FetchedEvidence, Finalization, FinalizerReport, ProvisionalFinalizer,
    ProvisionalStore, ReplayJudge,
};

pub mod shadow;
pub use shadow::{
    CountingShadowObserver, ShadowObservation, ShadowObservationLog, ShadowObserver,
    ShadowUnevaluated, ShadowVerdict, SharedShadowObserver, ATTESTATION_QUORUM_CONTROL,
    SHADOW_TARGET,
};

pub mod ramp;
#[cfg(feature = "fdb")]
pub use ramp::FdbRampPostureStore;
pub use ramp::{
    AbsentControl, CohortEvidence, HonestCohort, PostureSource, Provenance, RampArtifact,
    RampMeter, RampMode, RampPosture, RampPostureError, RampSnapshot, UnattributedTally,
    RAMP_ARTIFACT_SCHEMA,
};

pub mod stages;
pub use stages::{intent_stage_metrics, IntentStageMetrics, IntentStageSnapshot, IntentTrace};

/// The result of admitting an [`Intent`] before execution.
///
/// `Ruleset`-opaque in the wire (`IntentOp` is deliberately uninterpreted,
/// docs/08-persistence.md §2.2) — but the validator may name the durable
/// ledger keys it will want read inside the transaction, so the executor can
/// register their serializable conflict ranges up front (§7 step 1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntentPrecheck {
    /// The durable keys the executor must read before writing, so their
    /// serializable conflict ranges register (docs/08-persistence.md §7:
    /// "these reads register conflict ranges"). Named by the validator
    /// because only the `Ruleset` knows which ledger keys an op touches.
    pub read_keys: Vec<Vec<u8>>,
}

/// A validator verdict: admit (optionally naming read keys) or reject with a
/// `Ruleset`-defined reason code carried in `IntentOutcome::Rejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentVerdict {
    /// Admit the intent to execution, with the cluster standing behind it: it
    /// carries the required co-signatures, or this gateway does not enforce a
    /// quorum at all.
    Admit(IntentPrecheck),
    /// Admit the intent to **D29's low-population path**: commit it, and
    /// quarantine it until spot replay finalizes or annuls it.
    ///
    /// # This is not a weaker `Admit`
    ///
    /// It is a different destination. The executor writes a different finality
    /// onto the durable row, the client is told a different outcome, and the
    /// value the intent creates is an input to nothing until the cluster has
    /// re-executed the history behind it. Every property of this path is worse
    /// for the submitter than the attested path — not spendable at commit,
    /// replayed with probability 1, and usable only after `D_finalize` — which
    /// is what makes manufacturing low population self-defeating (D29
    /// clause 3).
    ///
    /// Reached **only** when the announced witness set resolves but cannot
    /// supply `N` eligible members — D29's `low_pop(i)`
    /// ([`RejectionCause::LowPopulationEpoch`]). An unresolvable or expired
    /// cell-epoch refuses instead ([`RejectionCause::UnknownEpoch`],
    /// [`RejectionCause::EpochStale`] — D37's correction of D27 clause (e)),
    /// and so does an intent that could have been attested and simply was
    /// not ([`Self::Reject`], never this).
    AdmitProvisional(IntentPrecheck),
    /// Reject the intent; `reason` maps onto
    /// `IntentOutcome::Rejected { reason }` (persist.rs's never-before-
    /// constructed variant — validation failures are a *rejection*, not an
    /// error).
    Reject {
        /// The `Ruleset`-defined rejection reason code.
        reason: u16,
    },
}

/// What the gateway knows about the connection an intent arrived on.
///
/// The validator's argument is otherwise only the intent itself, which is
/// entirely peer-authored: without this, *authorization* has nothing to
/// authorize against. The gateway has already bound
/// [`issuer`](Self::issuer) to the connection's authenticated transport
/// identity before the validator runs ([`crate::gateway`] step 2), so an
/// `issuer` here is a fact about the connection, not a claim in the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentContext {
    /// The connection's authenticated peer identity, already checked equal to
    /// `intent.issuer`.
    pub issuer: NodeId,
    /// The account this connection's session token names, if the peer
    /// completed a `Hello`. `None` means the connection has no established
    /// session, so nothing on it can be billed or attributed to an account.
    pub account: Option<AccountId>,
    /// The standing the gateway read from the connection's verified session
    /// token. A quarantined standing requires full admission validation.
    pub standing: SessionStanding,
}

impl IntentContext {
    /// A context for a connection with no established session — the state a
    /// peer is in between the transport handshake and its `Hello` ack.
    #[must_use]
    pub fn unauthenticated(issuer: NodeId) -> Self {
        Self {
            issuer,
            account: None,
            standing: SessionStanding::Quarantined,
        }
    }
}

/// The synchronous admission seam (D11 §2.2, first stage).
///
/// Runs on the gateway before any executor work; the FDB transaction remains
/// the sole authority — this is only a fast filter (docs/08-persistence.md
/// §2.2: "reject obviously invalid intents without an FDB round trip").
pub trait IntentValidator: Send + Sync {
    /// Validate `intent` as submitted on the connection `cx` describes,
    /// returning the verdict (and, on admit, the durable keys the executor
    /// should read).
    fn validate(&self, intent: &Intent, cx: &IntentContext) -> IntentVerdict;
}

/// The permissive default: admits every intent with no read keys.
///
/// This is what lets the harness run unconfigured — the gateway works, the
/// FDB path exercises its idempotency and minting machinery, and a linked
/// `Ruleset` swaps in real validation without touching the wiring.
///
/// It is a **test and bring-up** default, not a deployment one: a deployed
/// node runs [`BaselineIntentValidator`] (or a linked `Ruleset`'s own), and
/// the difference between the two is exactly the set of checks that do not
/// need game rules to state.
#[derive(Debug, Default, Clone, Copy)]
pub struct PermissiveValidator;

impl IntentValidator for PermissiveValidator {
    fn validate(&self, _intent: &Intent, _cx: &IntentContext) -> IntentVerdict {
        IntentVerdict::Admit(IntentPrecheck::default())
    }
}

/// The most ops one intent may carry.
///
/// Every op costs a minted `PersistId` and a write in the executor's single
/// serializable transaction, which FDB bounds at 10 MB and 5 s. A cap here
/// turns "one peer submits a 100 000-op intent" from a transaction that fails
/// after the round trip (or succeeds and hurts everyone else) into a refusal
/// costing one signature check.
pub const MAX_OPS_PER_INTENT: usize = 64;

/// The most argument bytes one op may carry.
pub const MAX_OP_ARGS_BYTES: usize = 4 * 1024;

/// The most argument bytes one intent may carry across all its ops.
pub const MAX_INTENT_ARGS_BYTES: usize = 64 * 1024;

/// The most attestations one intent may carry.
///
/// D10's default is K=3 of N≥5. Verification is one ed25519 check each, run
/// on the gateway's receive loop, so the bound is what keeps an intent's
/// admission cost constant.
pub const MAX_ATTESTATIONS: usize = 16;

/// The op id whose arguments this cluster's own executor interprets: the
/// ledger credit of docs/08-persistence.md §7, `account ‖ asset ‖ delta` as
/// three little-endian 8-byte fields.
///
/// Every other op id is `Ruleset`-opaque (docs/08-persistence.md §2.2) and
/// this validator does not pretend to understand it.
pub const LEDGER_CREDIT_OP: u16 = 0;

/// The exact `args` width of [`LEDGER_CREDIT_OP`].
const LEDGER_CREDIT_ARGS_BYTES: usize = 24;

/// The op id of the **item ownership transfer** — the second and last op this
/// cluster's own executor interprets, and the reference two-party trade of
/// docs/08-persistence.md §7.
///
/// # Why the cluster interprets this one
///
/// `ledger/item/{item_uid}` is the anti-dupe invariant (§7: "single ownership
/// row = anti-dupe invariant"), and an invariant nothing writes is not an
/// invariant. Until this op existed the executor's only effect was
/// [`LEDGER_CREDIT_OP`]'s blind `MutationType::Add`, which reads nothing — so
/// there was no read-check-write anywhere in the intent path and therefore
/// nothing to double-spend. This op is the producer and consumer that makes
/// the row real.
///
/// # `args` layout — 40 bytes, five little-endian 8-byte fields
///
/// Stated here as precisely as [`LEDGER_CREDIT_OP`]'s 24-byte triple is,
/// because the validator and both executors decode it and a layout described
/// in three places is a layout that will drift:
///
/// | offset | field | type | meaning |
/// |---|---|---|---|
/// | `0..8`   | `item`   | `u64` | the [`ItemUid`] whose ownership row moves |
/// | `8..16`  | `seller` | `u64` | [`AccountId`] the row must currently name; receives `price` |
/// | `16..24` | `buyer`  | `u64` | [`AccountId`] the row will name; pays `price` |
/// | `24..32` | `asset`  | `u64` | [`AssetId`] the price is denominated in |
/// | `32..40` | `price`  | `i64` | the price, which must not be negative |
///
/// # Why `2` and not `1`
///
/// Op ids are `Ruleset`-opaque by default, and a cluster-interpreted id is a
/// *reservation* out of that space: every peer that was sending id `1` as an
/// opaque op keeps meaning what it meant, and would start being decoded as a
/// trade the moment this constant claimed it. Id `1` is already in that
/// position throughout this repository — the gateway suites, the client's
/// offline queue and the stage-decomposition rig all send it as a stand-in
/// "trade" the cluster does not interpret — so claiming it would silently
/// reinterpret traffic that already exists. `2` is unclaimed. The general
/// lesson, for whoever reserves the third: pick an id nothing sends, and
/// expect to have to look.
///
/// [`ItemUid`]: orrery_protocol::ItemUid
pub const LEDGER_ITEM_TRANSFER_OP: u16 = 2;

/// The exact `args` width of [`LEDGER_ITEM_TRANSFER_OP`].
pub const LEDGER_ITEM_TRANSFER_ARGS_BYTES: usize = 40;

/// The decoded `args` of a [`LEDGER_ITEM_TRANSFER_OP`].
///
/// Decoded in exactly one place ([`ItemTransferArgs::decode`]) and used by the
/// admission validator, the FDB executor and [`MemIntentExecutor`] alike, so
/// the byte layout above has a single implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemTransferArgs {
    /// The unique item whose ownership row moves.
    pub item: orrery_protocol::ItemUid,
    /// The account the ownership row must currently name — the divesting
    /// party, credited `price`.
    pub seller: AccountId,
    /// The account the ownership row will name — the acquiring party, debited
    /// `price`. This is the **debit side**, and therefore the account the
    /// submitting connection must have authenticated as.
    pub buyer: AccountId,
    /// The asset the price is denominated in.
    pub asset: AssetId,
    /// The price, in `asset`. Never negative: a negative price is a debit
    /// wearing a credit's clothes.
    pub price: i64,
}

impl ItemTransferArgs {
    /// Decode a [`LEDGER_ITEM_TRANSFER_OP`]'s `args`.
    ///
    /// # Errors
    ///
    /// [`RejectionCause::MalformedItemTransferOp`] if the width is not
    /// [`LEDGER_ITEM_TRANSFER_ARGS_BYTES`] or the price is negative.
    pub fn decode(args: &[u8]) -> Result<Self, RejectionCause> {
        if args.len() != LEDGER_ITEM_TRANSFER_ARGS_BYTES {
            return Err(RejectionCause::MalformedItemTransferOp);
        }
        let field = |i: usize| u64::from_le_bytes(args[i..i + 8].try_into().expect("slice len"));
        let price = i64::from_le_bytes(args[32..40].try_into().expect("slice len"));
        if price < 0 {
            return Err(RejectionCause::MalformedItemTransferOp);
        }
        Ok(Self {
            item: orrery_protocol::ItemUid::new(field(0)),
            seller: AccountId::new(field(8)),
            buyer: AccountId::new(field(16)),
            asset: AssetId::new(field(24)),
            price,
        })
    }

    /// Encode these arguments in the 40-byte layout
    /// [`LEDGER_ITEM_TRANSFER_OP`] documents.
    ///
    /// The inverse of [`decode`](Self::decode), so a client (and every test in
    /// this repository) builds the layout from the same definition that reads
    /// it.
    #[must_use]
    pub fn encode(&self) -> [u8; LEDGER_ITEM_TRANSFER_ARGS_BYTES] {
        let mut args = [0u8; LEDGER_ITEM_TRANSFER_ARGS_BYTES];
        args[0..8].copy_from_slice(&self.item.0.to_le_bytes());
        args[8..16].copy_from_slice(&self.seller.0.to_le_bytes());
        args[16..24].copy_from_slice(&self.buyer.0.to_le_bytes());
        args[24..32].copy_from_slice(&self.asset.0.to_le_bytes());
        args[32..40].copy_from_slice(&self.price.to_le_bytes());
        args
    }
}

/// The durable verdict of interpreting one intent's ops.
///
/// Separate from [`IntentError`] on purpose. An error is a *server fault* and
/// reaches the peer as `REASON_EXECUTOR_ERROR`; a durable rejection is the
/// correct answer to a well-formed request the ledger refuses — the item moved
/// first, the balance is short. The P5 dupe gauntlet cannot tell a working
/// anti-dupe invariant from a broken cluster unless the two are
/// distinguishable, which is what this type is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpsVerdict {
    /// Every op's durable checks passed; the planned writes may be applied.
    Applied,
    /// A durable invariant refused the intent. Carries the wire reason code
    /// (one of `orrery_protocol::REASON_NO_SUCH_ITEM`,
    /// `REASON_NOT_ITEM_OWNER`, `REASON_INSUFFICIENT_BALANCE`,
    /// `REASON_ITEM_TRANSFER_TO_SELF`, `REASON_MALFORMED_OP`).
    Rejected(u16),
}

/// Why an intent failed admission. Mostly not sent on the wire — the wire
/// reason is [`orrery_protocol::REASON_VALIDATION_FAILED`], because a
/// `Ruleset`-defined reason space is the `Ruleset`'s to define — but always
/// logged, so an operator reading a rejection rate can tell a malformed client
/// from an attack. [`RejectionCause::wire_reason`] holds the mapping, and
/// [`RejectionCause::SelfWitness`] is the one cause that carries a code of its
/// own, because it is the one that is never a malformed client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionCause {
    /// The intent carries no ops: nothing to commit, and an idempotency row
    /// would still be burned recording that nothing happened.
    NoOps,
    /// More than [`MAX_OPS_PER_INTENT`] ops.
    TooManyOps,
    /// One op's `args` exceeded [`MAX_OP_ARGS_BYTES`], or the intent's total
    /// exceeded [`MAX_INTENT_ARGS_BYTES`].
    ArgsTooLarge,
    /// A [`LEDGER_CREDIT_OP`] whose `args` are not the executor's 24-byte
    /// `account ‖ asset ‖ delta` triple. Refused here rather than becoming an
    /// executor error after an FDB round trip.
    MalformedLedgerOp,
    /// A [`LEDGER_CREDIT_OP`] naming an account this connection has not
    /// authenticated as — including the case of no session at all.
    LedgerOpForAnotherAccount,
    /// A [`LEDGER_ITEM_TRANSFER_OP`] whose `args` are not the executor's
    /// 40-byte `item ‖ seller ‖ buyer ‖ asset ‖ price` layout, or whose price
    /// is negative.
    MalformedItemTransferOp,
    /// A [`LEDGER_ITEM_TRANSFER_OP`] naming one account as both parties.
    ///
    /// Refused at admission because it needs no durable read to see: a
    /// self-transfer would write an ownership row the value it already holds
    /// and bank a receipt for a trade that did not happen. The executor
    /// refuses it again against the durable rows, because
    /// [`PermissiveValidator`] — the harness default — admits everything.
    ItemTransferToSelf,
    /// A [`LEDGER_ITEM_TRANSFER_OP`] whose **debit side** (the buyer) is not
    /// the account this connection authenticated as — including the case of
    /// no session at all.
    ///
    /// The same rule [`LEDGER_CREDIT_OP`] enforces, applied to the side that
    /// loses value: the seller's balance is a blind credit and the item row is
    /// guarded by the durable owner check, so the buyer's balance is the only
    /// thing a peer could move here that is not its own.
    ItemTransferForAnotherAccount,
    /// More than [`MAX_ATTESTATIONS`] attestations.
    TooManyAttestations,
    /// The same witness appears twice, which would let one signature count
    /// K times toward a future K-of-N threshold.
    DuplicateAttestation,
    /// An attestation's signature does not verify over the intent's canonical
    /// preimage: a forged co-signature.
    BadAttestation,
    /// An attestation names the intent's own issuer as its witness.
    ///
    /// D10 item 4 seeds the witness set "excluding **all parties to the
    /// intent**", and the issuer is the first of those parties;
    /// `docs/07-witnessing.md` §4.1 states the rule and §4.2 puts the gateway
    /// on the hook for it ("the gateway rejects party attestations
    /// regardless"). Unlike every other cause here this one is not a client
    /// bug, which is why it is the one admission cause with its own wire code
    /// ([`orrery_protocol::REASON_SELF_WITNESS`]).
    SelfWitness,
    /// An attestation's witness is bound to an account that is a **party** to
    /// the intent — the issuer's own account under a second NodeId, or the
    /// counterparty named by a `LEDGER_ITEM_TRANSFER_OP`.
    ///
    /// The account-level half of [`Self::SelfWitness`], and the half D10
    /// item 4 actually asks for: parties are excluded "matched on **accounts
    /// and every NodeId bound to them**", not on NodeIds. Kept a separate
    /// cause because the two are separate operator facts — `SelfWitness` is
    /// one key signing for itself, this is one *account* signing for itself
    /// through a device the NodeId comparison cannot see — and collapsed to
    /// the same wire code, for the reason [`Self::wire_reason`] gives.
    ///
    /// Refused at every enforcement mode, exactly as `SelfWitness` is: party
    /// exclusion is not part of the K-of-N quorum and must not switch off
    /// with it.
    PartyAccountWitness,
    /// Two attestations whose witnesses are different NodeIds bound to the
    /// **same** account.
    ///
    /// [`Self::DuplicateAttestation`]'s account-level half, and distinct from
    /// it on purpose: "one device, twice" is a broken client and "one account,
    /// two devices" is the Sybil shape D10 item 5's acquisition cost exists to
    /// price. An operator that cannot count them apart cannot tell the two
    /// situations apart either.
    DuplicateAttestingAccount,
    /// An attestation came from a NodeId the announcement selected, but whose
    /// account binding this gateway could not resolve without blocking — so
    /// D31 clause (f) excluded it from `E(I)`.
    ///
    /// Not [`Self::WitnessOutsideAnnouncedSet`], and D31 clause (f) says why
    /// in as many words: that label would send an operator hunting a forgery,
    /// when what actually happened is that a resolver missed. The witness may
    /// be entirely honest — a peer connected to a sibling gateway, or one
    /// whose cache entry aged past `T_stale`.
    ///
    /// It is still a refusal rather than a silent drop, for the reason every
    /// membership failure is: the intent's draw was computed over a vector
    /// this attestation is not in, so counting it would be counting a
    /// signature toward a slot it cannot fill.
    UnresolvedWitnessBinding,
    /// The intent names a cell-epoch this gateway holds no announcement for.
    ///
    /// D27 clause (e) cases 2 and 3: an announcement for a different epoch
    /// than the intent names, or a cell that has never had one. `E(I)` is
    /// undefined, `required(I)` is undefined, and there is no honest set to
    /// judge the attestations against.
    ///
    /// **This cause refuses; it never reaches D29's provisional path.** D27
    /// clause (e) originally answered "in all three cases the failure mode is
    /// *provisional commit*, never *refusal*", and
    /// [D37](../../../../docs/adr/0037-unavailable-witness-epoch.md) corrects
    /// exactly these two cases by erratum: absence of the announcement is
    /// submitter-selectable — a peer holding one produces this refusal merely
    /// by withholding it — so a branch reachable at zero cost by the party
    /// being checked must not admit, or required scrutiny falls from K
    /// co-signatures to zero before a durable write. The cure is presentation,
    /// not quarantine: present the named announcement and retry (D37 clause
    /// (c)). Only [`Self::LowPopulationEpoch`] attaches to the provisional
    /// branch.
    UnknownEpoch,
    /// The intent names a cell-epoch whose usability window has closed.
    ///
    /// Kept distinct from [`Self::UnknownEpoch`] and from every signature
    /// failure on purpose, and D28 clause (g) says why: past the grace, "the
    /// answer is a distinct `EpochStale` rejection and not a signature
    /// failure, because the two are operationally different". The first says
    /// re-collect under the current epoch; the second says somebody forged
    /// something. Conflating them puts honest netsplit survivors in the same
    /// bucket as attackers.
    EpochStale,
    /// The announced set has too few non-party members to draw from.
    ///
    /// D27 clause (d): below `N_floor` (= `WITNESS_SET_FLOOR_N`, 5) **no draw
    /// is made**, because a required subset drawn from four is not the
    /// hypergeometric draw the collusion arithmetic is computed over. Unlike
    /// [`Self::UnknownEpoch`] and [`Self::EpochStale`], which refuse since
    /// D37 corrected D27 clause (e), this is the sole cause that reaches
    /// D29's quarantined provisional path: the announcement resolved, so
    /// `low_pop(i)` is a defined, coordinator-signed fact rather than an
    /// absence — subject to D29 clause 3's `reversible(i)`.
    LowPopulationEpoch,
    /// An attestation names a witness the announcement did not select.
    ///
    /// The self-chosen-witness case D10 item 4 exists to stop. The signature
    /// may be cryptographically perfect and it still counts for nothing: a
    /// submitter that could nominate its own co-signers would be certifying
    /// its own trade. Judged against the **announced** set of the epoch the
    /// intent names, never against who is in the cell now (D28 clause (g)) —
    /// a witness that left one second after signing is still a valid signer.
    WitnessOutsideAnnouncedSet,
    /// Fewer than `WITNESS_QUORUM_K` valid attestations are present at all.
    ///
    /// Implied by [`Self::RequiredWitnessMissing`] — a set of fewer than K
    /// cannot contain the required K — and kept separate anyway, because the
    /// two describe different operator situations. This one is "the submitter
    /// did not collect enough co-signatures", which is the co-sign budget or
    /// the cell's population; the other is "it collected enough, and they were
    /// the wrong ones", which is either bad luck or attestation shopping.
    ThresholdNotMet,
    /// The intent names a cell-epoch whose cell the issuer holds no
    /// coordinator-confirmed interest in.
    ///
    /// [D30](../../../../docs/adr/0030-cell-epoch-standing.md) clause (a).
    /// The epoch resolved, the announcement is genuine, and this gateway is
    /// still refusing to judge the intent under it — because *which announced
    /// set judges an intent* must not be a submitter's choice. D27 §4.4's
    /// collusion arithmetic is stated per cell, and a submitter free to name
    /// any cell-epoch in this gateway's cache turns "do I hold `K` of this
    /// cell's draw" into "do I hold `K` of any reachable cell's draw".
    ///
    /// The standing is the peer's own live interest grant — the same
    /// coordinator signature [`crate::gateway::InterestAuthority::allows`]
    /// answers a `Claim` and D25's fan-out with, and the same predicate D28
    /// clause (d) step 6 already gates *presenting* an announcement on.
    ///
    /// Deliberately **not** the provisional case: only
    /// [`Self::LowPopulationEpoch`] describes a gateway that resolved the
    /// announcement but has nobody to draw from, and D29 answers that one
    /// with a quarantined commit. [`Self::UnknownEpoch`] and
    /// [`Self::EpochStale`] refuse outright — D37 corrected the old reading
    /// that grouped all three. This one describes a submitter that asked to
    /// be judged somewhere it does not stand, which is a refusal at every
    /// population.
    NoStandingInCell,
    /// The intent fell to D29's low-population path and is not the kind of
    /// intent a forward-written inverse can undo (D29 clause 3's
    /// `reversible(i)`).
    ///
    /// Value *creation* into the submitter's own rows is admitted; value
    /// *transfer* is refused, because annulling a transfer takes value from a
    /// second account that did nothing wrong and could not have known. See
    /// [`provisional::classify`] for the full statement and for the negative
    /// delta case.
    ProvisionalIneligible,
    /// The intent fell to D29's low-population path carrying no
    /// [`orrery_protocol::EvidenceCommitment`], so nothing could ever finalize
    /// it.
    ///
    /// Committing it would mint durable value with a guaranteed expiry five
    /// minutes later, turning a free refusal into the one outcome D29
    /// clause 9(b) is arranged to avoid.
    ProvisionalNoEvidence,
    /// A witness the draw named did not attest.
    ///
    /// The conjunct that makes K-of-N mean something: `required(I) ⊆ the
    /// witnesses that attested`. "Any first K of N" is precisely the
    /// attestation shopping D10 abolishes, so a missing *required* co-signer
    /// admits no substitute, however many other valid attestations arrived.
    ///
    /// **The cause does not say which witness is missing**, and neither does
    /// the wire code: `required(I)` is drawn with a secret held until epoch
    /// end, and naming the gap would leak the draw one intent at a time.
    RequiredWitnessMissing,
}

impl RejectionCause {
    /// A short stable label for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoOps => "no_ops",
            Self::TooManyOps => "too_many_ops",
            Self::ArgsTooLarge => "args_too_large",
            Self::MalformedLedgerOp => "malformed_ledger_op",
            Self::LedgerOpForAnotherAccount => "ledger_op_for_another_account",
            Self::MalformedItemTransferOp => "malformed_item_transfer_op",
            Self::ItemTransferToSelf => "item_transfer_to_self",
            Self::ItemTransferForAnotherAccount => "item_transfer_for_another_account",
            Self::TooManyAttestations => "too_many_attestations",
            Self::DuplicateAttestation => "duplicate_attestation",
            Self::BadAttestation => "bad_attestation",
            Self::SelfWitness => "self_witness",
            Self::PartyAccountWitness => "party_account_witness",
            Self::DuplicateAttestingAccount => "duplicate_attesting_account",
            Self::UnresolvedWitnessBinding => "unresolved_witness_binding",
            Self::UnknownEpoch => "unknown_epoch",
            Self::EpochStale => "epoch_stale",
            Self::LowPopulationEpoch => "low_population_epoch",
            Self::NoStandingInCell => "no_standing_in_cell",
            Self::WitnessOutsideAnnouncedSet => "witness_outside_announced_set",
            Self::ThresholdNotMet => "threshold_not_met",
            Self::RequiredWitnessMissing => "required_witness_missing",
            Self::ProvisionalIneligible => "provisional_ineligible",
            Self::ProvisionalNoEvidence => "provisional_no_evidence",
        }
    }

    /// The `IntentOutcome::Rejected { reason }` code this cause is answered
    /// with on the wire.
    ///
    /// Almost every cause collapses to
    /// [`orrery_protocol::REASON_VALIDATION_FAILED`], and deliberately so: the
    /// reason space below it is a `Ruleset`'s to define, and a cluster that
    /// enumerated its own envelope checks there would be spending numbers a
    /// game may want. [`Self::SelfWitness`] is the exception, for the reason
    /// [`orrery_protocol::REASON_SELF_WITNESS`] states at length — it is the
    /// only one of these that describes an attack rather than a bad client,
    /// and an operator must be able to count it without reading gateway logs.
    #[must_use]
    pub const fn wire_reason(self) -> u16 {
        match self {
            // Both halves of party exclusion answer one code. They are the
            // same fact at two resolutions — "a party to this intent attested
            // it" — the client's remedy is identical (re-collect from
            // non-parties), and an operator watching for party attestations
            // wants one counter rather than two that must be added up. The
            // sub-distinction stays in the log, where an attacker cannot read
            // it, following D30 clause (c).
            //
            // Minting a second code would also be strictly *worse* than the
            // collapse, and not merely redundant: an account-specific answer
            // is an oracle on the `id/` bindings. A submitter naming a chosen
            // account as the seller of a transfer and attaching one
            // attestation could read "is NodeId W bound to account A" straight
            // off the wire, one probe per guess. The refusal itself already
            // leaks a bit; a dedicated code would leak which bit.
            Self::SelfWitness | Self::PartyAccountWitness => orrery_protocol::REASON_SELF_WITNESS,
            // Every K-of-N refusal collapses to one code, and the collapse is
            // the design rather than laziness. A client needs exactly one bit
            // here — "your attestations were wrong" as against
            // `REASON_VALIDATION_FAILED`'s "your ops were wrong" — because
            // that is the bit that decides whether resubmitting the same
            // intent with more co-signatures can work. Splitting further would
            // tell a submitter *which* required witness it is missing, and
            // `required(I)` is drawn with a secret this gateway holds until
            // epoch end: a per-cause reply would leak the draw one intent at a
            // time and hand `intent_id` grinding back to the attacker. The
            // distinctions an operator needs are all present, in the logs,
            // where an attacker cannot read them.
            Self::UnknownEpoch
            | Self::EpochStale
            | Self::LowPopulationEpoch
            | Self::NoStandingInCell
            | Self::WitnessOutsideAnnouncedSet
            | Self::ThresholdNotMet
            | Self::UnresolvedWitnessBinding
            | Self::RequiredWitnessMissing => orrery_protocol::REASON_ATTESTATION_QUORUM,
            // The two provisional causes do **not** collapse into the quorum
            // code, and the reason is the reason the quorum code exists at
            // all: a client needs to know whether resubmitting can work.
            // `REASON_ATTESTATION_QUORUM` says "collect more co-signatures",
            // which is precisely the advice that cannot help here — there was
            // nobody to collect them from, which is how the intent reached
            // this path. These two say something else: change the intent
            // (drop the transfer) or attach a commitment.
            Self::ProvisionalIneligible => orrery_protocol::REASON_PROVISIONAL_INELIGIBLE,
            Self::ProvisionalNoEvidence => orrery_protocol::REASON_PROVISIONAL_NO_EVIDENCE,
            _ => orrery_protocol::REASON_VALIDATION_FAILED,
        }
    }
}

/// The admission filter a deployed `persistd` runs when no game `Ruleset` is
/// linked in (docs/11-roadmap.md §P2: the intent execution path "without
/// witness attestation", with the `Ruleset` check a stub).
///
/// # The contract this implements
///
/// Everything below is a property of the intent envelope and of this
/// cluster's own executor — none of it needs game rules to state, and all of
/// it is checkable in constant time without an FDB round trip:
///
/// 1. **Shape.** At least one op, at most [`MAX_OPS_PER_INTENT`]; each op's
///    `args` at most [`MAX_OP_ARGS_BYTES`] and the intent's total at most
///    [`MAX_INTENT_ARGS_BYTES`]. The executor mints one id and writes one
///    effect per op inside a single serializable transaction, so an unbounded
///    op list is an unbounded transaction.
/// 2. **The two ops this cluster interprets.** [`LEDGER_CREDIT_OP`]'s `args`
///    must be exactly the executor's 24-byte `account ‖ asset ‖ delta`
///    triple, and [`LEDGER_ITEM_TRANSFER_OP`]'s exactly the 40-byte
///    `item ‖ seller ‖ buyer ‖ asset ‖ price` layout with a non-negative
///    price. Without this check a malformed op costs a full FDB round trip
///    and comes back as `REASON_EXECUTOR_ERROR`, which reads as a server
///    fault rather than a bad request. A transfer naming one account as both
///    parties is refused here too — it needs no durable read to see.
/// 3. **Account authorization.** A [`LEDGER_CREDIT_OP`] may only name the
///    account whose session token this connection authenticated with, and a
///    [`LEDGER_ITEM_TRANSFER_OP`] may only debit that account. A connection
///    with no session has no account and may not submit either. This is the
///    check that keeps the executor's blind `MutationType::Add` from being a
///    "credit anyone" primitive reachable by any peer that can complete a
///    transport handshake.
/// 4. **Attestation authenticity.** A present attestation must be real: at
///    most [`MAX_ATTESTATIONS`], no repeated witness, and every signature
///    verifies against D27's domain-separated *witness* preimage
///    ([`Intent::attestation_preimage`]) — never
///    [`Intent::signing_preimage`], which is what the issuer signs and what
///    this filter checked before the two roles were separated. A forged
///    co-signature is refused here rather than being carried into the ledger's
///    audit trail.
/// 6. **The K-of-N quorum, when this validator enforces it**
///    ([`AttestationEnforcement`]). The intent's `cell_epoch` handle resolves
///    to a coordinator-announced witness set; **the issuer must hold live
///    coordinator-confirmed interest in that announcement's cell** (D30
///    clause (a)); every attestation must come from that set minus the
///    parties; and the `K` members D27's per-intent draw names must all have
///    signed. Off by default, because every production issuer in this tree
///    submits zero attestations and a hard requirement flipped on
///    unconditionally would refuse all of them at once.
/// 5. **Party exclusion, on NodeIds and on accounts.** No attestation may
///    name the issuer as its witness, and — given an
///    [`crate::gateway::BindingAuthority`] — none may come from a NodeId
///    bound to any account the intent's ops name as a party, nor may two
///    attestations come from two NodeIds bound to one account (D10 item 4,
///    "matched on **accounts and every NodeId bound to them**";
///    `docs/07-witnessing.md` §4.1's per-intent party exclusion, enforced by
///    the gateway per §4.2). The NodeId half is the cheapest check in the
///    function and runs above all of them, so a self-witnessed intent never
///    reaches signature verification and never produces a read plan. The
///    account half runs in [`Self::check_at`], which is where the resolver
///    is — above the enforcement switch, because party exclusion is not part
///    of the quorum and does not switch off with it.
///
/// On admit, the precheck names the `ledger/bal/{account}/{asset}` rows the
/// credited ops touch and the `ledger/item/{item_uid}` + debit-side balance
/// rows the transfers touch, so a `Ruleset`-linked executor can register their
/// conflict ranges up front (docs/08-persistence.md §7 step 1).
///
/// # What this deliberately does NOT validate
///
/// Stating this precisely is the point: a validator that admits everything is
/// exactly as dishonest as one that rejects everything, and the gap below is
/// what a linked `Ruleset` still owes.
///
/// - **Every durable invariant.** Balances, item ownership, single-ownership,
///   value conservation, progression gates, quest state. Nothing here reads
///   any durable row; the FDB transaction remains the sole authority (§2.2).
///   For a [`LEDGER_CREDIT_OP`] the executor checks none of them either — a
///   credit admitted here still mints value from nothing, which is a property
///   of that op and not something this filter can repair. A
///   [`LEDGER_ITEM_TRANSFER_OP`] is the exception and the reason it exists:
///   the executor reads `ledger/item/{item_uid}` and the debit-side balance
///   inside the transaction and refuses the transfer if either fails.
/// - **The counterparty's consent.** Nothing here or in the executor proves
///   the *seller* agreed to the trade; the durable rows record who owns what,
///   not who assented. docs/08-persistence.md §7 places that upstream of the
///   transaction ("signatures and attestations are verified **before** the
///   transaction"), and the K-of-N attestation threshold that carries it is
///   P5's, not landed. Until it is, a deployment must not expose
///   [`LEDGER_ITEM_TRANSFER_OP`] to untrusted peers: a buyer can name any
///   owner as seller and any price, including zero.
/// - **Any op other than the two above.** They are `Ruleset`-opaque by
///   design; their `args` are checked for size and for nothing else.
/// - **The K-of-N quorum, when [`AttestationEnforcement::Off`]** — which is
///   the default. In that mode `cell_epoch` is still carried and not checked,
///   and a present attestation proves only that some key signed the right
///   bytes. Turning the switch on is what makes it mean anything, and what
///   the switch does *not* decide is when a deployment flips it: the ramp is
///   policy and is tracked separately from this mechanism.
/// - **The low-population and provisional paths.** An intent whose cell-epoch
///   this gateway cannot resolve, or whose eligible set is below
///   `WITNESS_SET_FLOOR_N`, is not admitted by these envelope checks; the
///   disposition is D29's total admission function in [`Self::check_at`]. An
///   unavailable announcement **refuses**
///   ([`RejectionCause::UnknownEpoch`], [`RejectionCause::EpochStale`]):
///   D37 corrects D27 clause (e)'s "in all three cases the failure mode is
///   provisional commit, never refusal", because absence of the announcement
///   is submitter-selectable and no branch reachable by withholding input may
///   admit — the cure is presenting it and retrying. Only a *resolved*
///   announcement with too few eligible witnesses
///   ([`RejectionCause::LowPopulationEpoch`]) reaches D29's quarantined
///   provisional commit, subject to `reversible(i)`; that path is built, and
///   that cause alone attaches to it.
/// - **Account-level party exclusion, when no resolver is configured.** D10
///   item 4 excludes parties "matched on **accounts and every NodeId bound to
///   them**", and check 5's account half needs an
///   [`crate::gateway::BindingAuthority`] to answer `owner(n)`. Built through
///   [`Self::permissive`], this validator has none, and it does not pretend
///   to: it excludes on NodeId alone and says so by having no authority,
///   rather than by answering "not a party" to every question.
///
///   What it never does is fail *open*. Given a resolver, an unresolvable
///   NodeId is treated as a party and excluded (D31 clause (f)); given none,
///   an enforcing validator excludes every candidate and every attested
///   intent demotes to D29's provisional path. The objection this file used to
///   raise against itself — *a check that fails open on a miss is worse than
///   an absent one, because it reads as coverage* — is the sentence D31
///   clause (f) turned into the rule.
///
///   What is still approximated is the **coordinator's** selection-time half
///   (D28 clause (e)): a NodeId bound to the same account but connected to a
///   different coordinator is not deduped out of the candidate pool. That is
///   defence at selection, this is defence at admission, and D31 clause (d)
///   keeps the coordinator out of the `id/` rows deliberately.
/// - **Which *one* cell an intent belongs to.** D30 clause (a) narrows the
///   cell-epochs a submitter may be judged under to the cells its own
///   coordinator grant covers — D5's 27-cell neighbourhood, capped at
///   `MAX_INTEREST_GRANT_CELLS` — and not to a single cell, because nothing
///   in an intent identifies one. The ops are `Ruleset`-opaque and the two
///   this cluster does interpret address flat keys with no spatial term at
///   all (`keyspace::ledger_bal_key`, `keyspace::ledger_item_key`), so there
///   is no cell to derive. What survives is a bounded neighbourhood factor,
///   quantified in D30's Consequences.
/// - **Rate and quota.** Intent submission is bounded per connection by the
///   gateway's in-flight lane, not by a per-account budget the way reports
///   are.
/// - **Replay.** Handled durably by the `intent/{intent_id}` idempotency row
///   (§7 step 0), not by an admission-time cache.
/// - **The issuer signature and the issuer/connection binding**, which the
///   gateway checks *before* this runs; the validator would be the wrong
///   place to repeat them.
#[derive(Default, Clone)]
pub struct BaselineIntentValidator {
    enforcement: AttestationPosture,
    epochs: Option<Arc<crate::witness_epoch::WitnessEpochAuthority>>,
    interest: Option<Arc<dyn crate::gateway::InterestAuthority>>,
    bindings: Option<crate::gateway::SharedBindingAuthority>,
    /// Where [`AttestationEnforcement::Shadow`]'s observations go.
    ///
    /// `None` in every other mode, and `None` is not "discard": the `tracing`
    /// event in [`shadow::emit`] is unconditional, so a validator built
    /// through [`Self::shadow`] still discharges D32 clause (b)'s second
    /// obligation with nothing wired. This field is the *in-process* half,
    /// for the collector and the gate leg that want the observations back
    /// rather than in a log.
    observer: Option<SharedShadowObserver>,
}

/// Written out rather than derived because [`crate::gateway::InterestAuthority`]
/// is not `Debug`: it is an interface over the gateway's live, lock-guarded
/// snapshot table, and formatting one would put every peer's coordinator
/// interest set into whatever log line printed the validator. Whether an
/// authority is configured is the fact a reader needs here.
impl core::fmt::Debug for BaselineIntentValidator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BaselineIntentValidator")
            .field("enforcement", &self.enforcement.get())
            .field("epochs", &self.epochs)
            .field("interest", &self.interest.is_some())
            .field("bindings", &self.bindings.is_some())
            .field("observer", &self.observer.is_some())
            .finish()
    }
}

/// Whether this gateway requires D27's K-of-N quorum, or only checks that a
/// present attestation is real.
///
/// # Why this is a switch and not a constant
///
/// [`BaselineIntentValidator`] is what a deployed `persistd` runs with no
/// linked `Ruleset`, and it is also what every harness in this repository
/// runs. Every production issuer in the tree submits **zero** attestations
/// today, so a hard requirement flipped on unconditionally would refuse every
/// intent in the workspace at once — including the control arm an
/// attestation-overhead measurement needs, which is the zero-attestation path
/// by definition.
///
/// The ramp arrived as [D32](../../../../docs/adr/0032-enforcement-ramp.md),
/// which names this control **C1** and gives it the three positions below plus
/// a deployment lever that reaches them ([`AttestationPosture`], and
/// `persistd`'s `--attestation-enforcement`). What this code still takes no
/// position on is *policy*: when a deployment promotes, what evidence
/// justifies it, and what verdict rate demotes it are clause (e)'s and clause
/// (f)'s, computed from the observations this mode records rather than decided
/// here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AttestationEnforcement {
    /// Attestations are verified but not required, and no witness set is
    /// consulted: the pre-D27 behaviour, and the default.
    ///
    /// Note what stays enforced even here — the D27 preimage, the
    /// [`MAX_ATTESTATIONS`] cap, the no-repeat rule and the self-witness
    /// refusal. "Off" means *the quorum* is off, not that an attestation may
    /// be a forgery.
    ///
    /// **`Off` is not shadow**, and D32 clause (b) makes the distinction a
    /// rule: this arm evaluates nothing, so it observes nothing, so it
    /// calibrates nothing — a control in `Off` has no observation period and
    /// cannot be promoted out of one.
    #[default]
    Off,
    /// D32 clause (b)'s shadow: the full predicate is evaluated, the action
    /// `Required` would have taken is recorded, and none of it is taken.
    ///
    /// Every sub-predicate `Required` evaluates is evaluated here, D30's
    /// standing conjunct included, because a shadow measurement that skipped
    /// one understates the refusal count it exists to produce. The suppressed
    /// action is the refusal: an intent this arm would have refused is
    /// admitted exactly as `Off` admits it, and the would-be
    /// [`RejectionCause`] is handed to a [`ShadowObserver`] and emitted on
    /// [`SHADOW_TARGET`] instead.
    ///
    /// The always-on set is unchanged and unchangeable: this arm switches off
    /// the quorum and nothing else. On an internal error the arm degrades to
    /// [`ShadowVerdict::Unevaluated`] — never to an action.
    Shadow,
    /// D27's full admission predicate: every attestation from the announced
    /// eligible set, and the drawn required subset present in full.
    Required,
}

impl AttestationEnforcement {
    /// The CLI spelling D32 clause (c)'s inventory gives this mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Required => "required",
        }
    }

    /// Whether this mode refuses an intent the quorum predicate rejects.
    ///
    /// The one question the admission path actually asks, written once so that
    /// adding a fourth arm forces an answer here rather than at each `match`.
    #[must_use]
    pub const fn acts(self) -> bool {
        matches!(self, Self::Required)
    }

    /// Whether this mode evaluates the quorum predicate at all.
    #[must_use]
    pub const fn evaluates(self) -> bool {
        matches!(self, Self::Shadow | Self::Required)
    }
}

impl core::str::FromStr for AttestationEnforcement {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "required" => Ok(Self::Required),
            other => Err(format!(
                "unknown attestation enforcement mode `{other}` \
                 (expected one of: off, shadow, required)"
            )),
        }
    }
}

/// The runtime half of C1's lever: a posture cell every clone of one validator
/// shares.
///
/// # Why the mode is not a plain field any more
///
/// D32 clause (c) gives each control **two layers**, and the reason is an
/// incident timescale rather than tidiness. A CLI argument is a startup
/// default: changing it means rolling a gateway restart, which drops sessions
/// and takes minutes. Clause (f)'s auto-suspend has to demote a misbehaving
/// control *while the fleet runs*, within one poll interval, so the mode has
/// to be writable by something other than a process launch. This cell is that
/// something — the seam the durable `ramp/attestation_quorum` poller writes
/// into, and the seam a test writes into directly.
///
/// The load is `Relaxed` and on the admission path: one atomic byte read per
/// intent, against a check that already verifies ed25519 signatures. Ordering
/// buys nothing here because there is nothing to order it against — a posture
/// change is allowed to be observed one intent late by construction (clause
/// (c) bounds the fleet-wide latency at a poll interval, not at an
/// instruction).
///
/// # The asymmetry is enforced by the API, not by convention
///
/// Clause (f): *automation may make the fleet safer without asking, never less
/// safe.* [`Self::auto_suspend`] is therefore the only write automation gets,
/// and it moves `Required → Shadow` and nothing else — never to `Off`, which
/// would blind the cluster during exactly the incident that tripped it and
/// would make the trigger a censorship lever. Promotion is [`Self::set`], an
/// operator act.
#[derive(Debug, Clone)]
pub struct AttestationPosture(Arc<std::sync::atomic::AtomicU8>);

impl Default for AttestationPosture {
    fn default() -> Self {
        Self::new(AttestationEnforcement::Off)
    }
}

impl AttestationPosture {
    /// A posture cell starting at `mode`.
    #[must_use]
    pub fn new(mode: AttestationEnforcement) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU8::new(Self::code(mode))))
    }

    /// The mode in force right now.
    #[must_use]
    pub fn get(&self) -> AttestationEnforcement {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            0 => AttestationEnforcement::Off,
            1 => AttestationEnforcement::Shadow,
            // Unreachable: `code` is the only writer and it emits 0, 1 or 2.
            // Written as the enforcing arm rather than as a panic because a
            // torn read here would be a refusal, which is the safe direction.
            _ => AttestationEnforcement::Required,
        }
    }

    /// Set the mode. The operator lever, and the only one that may promote.
    pub fn set(&self, mode: AttestationEnforcement) {
        self.0
            .store(Self::code(mode), std::sync::atomic::Ordering::Relaxed);
    }

    /// D32 clause (f)'s trip: demote an acting control to shadow, and refuse
    /// to do anything else.
    ///
    /// Returns whether the posture moved. `false` is the ordinary answer for a
    /// control that is already `Shadow` or `Off` — there is no action left to
    /// suspend, and a trip that "succeeded" by moving `Off → Shadow` would be
    /// automation *starting* an evaluation nobody asked for.
    pub fn auto_suspend(&self) -> bool {
        self.0
            .compare_exchange(
                Self::code(AttestationEnforcement::Required),
                Self::code(AttestationEnforcement::Shadow),
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    const fn code(mode: AttestationEnforcement) -> u8 {
        match mode {
            AttestationEnforcement::Off => 0,
            AttestationEnforcement::Shadow => 1,
            AttestationEnforcement::Required => 2,
        }
    }
}

impl BaselineIntentValidator {
    /// A validator that verifies attestations but requires no quorum.
    ///
    /// The library and harness default, and the mode every pre-existing intent
    /// test runs under.
    #[must_use]
    pub fn permissive() -> Self {
        Self::default()
    }

    /// A validator that requires no quorum but still refuses **party**
    /// attestations at the account level, resolved through `bindings`.
    ///
    /// The pairing is deliberate and is the acceptance line #211 was written
    /// around: party exclusion is not part of D27's K-of-N predicate and must
    /// not switch off with it, exactly as
    /// [`RejectionCause::SelfWitness`] does not. `Off` means *the quorum* is
    /// off; it never means a party may certify its own trade.
    ///
    /// [`Self::permissive`] resolves no bindings at all, which leaves the
    /// pre-D31 NodeId-only behaviour — honest, because a validator with no
    /// resolver has no account to compare and says so by having none, rather
    /// than by answering "not a party" to every question.
    #[must_use]
    pub fn permissive_with_bindings(bindings: crate::gateway::SharedBindingAuthority) -> Self {
        Self {
            bindings: Some(bindings),
            ..Self::default()
        }
    }

    /// A validator that enforces D27's K-of-N predicate against the epochs
    /// `epochs` holds, for issuers `interest` says stand in the cell.
    ///
    /// All three authorities are required. The second is
    /// [D30](../../../../docs/adr/0030-cell-epoch-standing.md) clause (a) and
    /// the third is [D31](../../../../docs/adr/0031-id-account-subspace.md)
    /// clause (e)'s `owner(n)` resolver, without which `E(I)` can only be
    /// derived over NodeIds and D10 item 4's account half goes unenforced.
    /// Passing [`crate::gateway::UnboundBindingAuthority`] is legal and is not
    /// a way to switch the account half off: under D31 clause (f) a resolver
    /// that answers nothing excludes everybody, so every attested intent
    /// demotes to D29's provisional path. That is the intended shape of the
    /// interim, not a degenerate case.
    /// A cell-epoch resolves by handle out of a cache that holds every cell
    /// any peer has couriered an announcement for, so without a standing
    /// predicate the *submitter* decides which announced set judges its
    /// intent. `interest` is the gateway's own
    /// [`crate::gateway::InterestAuthority`] — the same coordinator-signed
    /// object D28 clause (d) step 6 already gates announcement *presentation*
    /// on, reused rather than a second notion of who belongs where.
    #[must_use]
    pub fn enforcing(
        epochs: Arc<crate::witness_epoch::WitnessEpochAuthority>,
        interest: Arc<dyn crate::gateway::InterestAuthority>,
        bindings: crate::gateway::SharedBindingAuthority,
    ) -> Self {
        Self {
            enforcement: AttestationPosture::new(AttestationEnforcement::Required),
            epochs: Some(epochs),
            interest: Some(interest),
            bindings: Some(bindings),
            observer: None,
        }
    }

    /// A validator that evaluates D27's K-of-N predicate exactly as
    /// [`Self::enforcing`] does and **refuses nothing on it**, recording the
    /// verdict it would have returned instead.
    ///
    /// D32 clause (b)'s shadow mode, and the same three authorities are
    /// required for the same reason: a shadow whose predicate is weaker than
    /// live mode's measures a different control from the one it is calibrating
    /// for. Passing a resolver that answers nothing is legal here too and
    /// means what it means everywhere else — everybody is excluded, `|E(I)|`
    /// collapses, and the recorded verdict is
    /// [`ShadowVerdict::WouldCommitProvisionally`] rather than an admission.
    /// That is an honest measurement of a gateway with no `id/` rows, which is
    /// the deployment this tree currently has.
    ///
    /// Observations reach [`SHADOW_TARGET`] as `tracing` events with no
    /// further wiring. Use [`Self::shadow_observing`] when they must also come
    /// back in-process.
    #[must_use]
    pub fn shadow(
        epochs: Arc<crate::witness_epoch::WitnessEpochAuthority>,
        interest: Arc<dyn crate::gateway::InterestAuthority>,
        bindings: crate::gateway::SharedBindingAuthority,
    ) -> Self {
        Self {
            enforcement: AttestationPosture::new(AttestationEnforcement::Shadow),
            epochs: Some(epochs),
            interest: Some(interest),
            bindings: Some(bindings),
            observer: None,
        }
    }

    /// [`Self::shadow`], with the observations also handed to `observer`.
    ///
    /// The in-process surface D32 clause (e)'s report and [#222]'s gate leg
    /// read: a [`ShadowObservationLog`] gives the observations back
    /// individually, and a [`CountingShadowObserver`] gives the numerator and
    /// denominator of a rate without retaining anything.
    ///
    /// [#222]: https://github.com/baadc0de/orrery/issues/222
    #[must_use]
    pub fn shadow_observing(
        epochs: Arc<crate::witness_epoch::WitnessEpochAuthority>,
        interest: Arc<dyn crate::gateway::InterestAuthority>,
        bindings: crate::gateway::SharedBindingAuthority,
        observer: SharedShadowObserver,
    ) -> Self {
        Self {
            observer: Some(observer),
            ..Self::shadow(epochs, interest, bindings)
        }
    }

    /// This validator's enforcement mode, as of this instant.
    ///
    /// A read of [`Self::posture`] rather than of a constant: the mode is
    /// runtime-settable (D32 clause (c)), so a caller that cached this answer
    /// would be reporting the posture at startup and calling it the posture
    /// now.
    #[must_use]
    pub fn enforcement(&self) -> AttestationEnforcement {
        self.enforcement.get()
    }

    /// The posture cell this validator reads on every admission.
    ///
    /// The handle a `ramp/attestation_quorum` poller — or clause (f)'s
    /// auto-suspend monitor — writes into to change the mode of a running
    /// process. Cloning it is cloning the `Arc`, so every holder sees every
    /// write.
    #[must_use]
    pub fn posture(&self) -> AttestationPosture {
        self.enforcement.clone()
    }
}

/// Append `account` to a party set unless it is already there.
///
/// Order is preserved and duplicates are not, for the same reason
/// [`push_read_key`] does it: an intent naming one account twice (a credit and
/// the debit side of a trade in the same intent) is ordinary, and the set is
/// then scanned once per announced candidate.
fn push_party(parties: &mut Vec<AccountId>, account: AccountId) {
    if !parties.contains(&account) {
        parties.push(account);
    }
}

/// `P(I)` — the accounts an intent is a transaction *between*.
///
/// [D10](../../../../docs/adr/0010-witnessing.md) item 4 seeds the witness set
/// "excluding **all parties to the intent** (matched on accounts and every
/// NodeId bound to them)". This function is the first half of that sentence;
/// [`eligible_after_party_exclusion`] is the second.
///
/// # Why this needs no reverse lookup
///
/// The party side was never the missing half, and D31's Context says so with
/// the line numbers: the ops this cluster interprets are keyed by
/// [`AccountId`] outright — `ledger_bal_key(account, asset)`,
/// `ItemTransferArgs { seller, buyer, .. }` — and the receipt's
/// `parties: Vec<AccountId>` is built straight from the planned writes. **The
/// party set is already a set of accounts.** What needed the `id/` reverse
/// index is the *candidate* side, where `WitnessEpochClaimsV1` carries
/// `Vec<NodeId>` and no account anywhere.
///
/// # `issuer_account`, and why the two derivation sites agree
///
/// `issuer_account` is the submitting connection's own account, from its
/// verified session token — the thing that makes an issuer's *other* NodeIds
/// excludable. The executor re-derives `E(I)` when it records `AttestRow`
/// (D27 clause (f)) and has no [`IntentContext`], so it supplies
/// [`crate::gateway::BindingAuthority::owner`] of `intent.issuer` there
/// instead.
///
/// The two cannot disagree on any intent where "party" means anything, and
/// that is a property of the ops rather than a coincidence: a
/// [`LEDGER_CREDIT_OP`] is refused unless it names the connection's own
/// account, and a [`LEDGER_ITEM_TRANSFER_OP`] is refused unless the
/// connection's account is the buyer. So for both interpreted ops the issuer's
/// account is already in the set the ops name. It can differ only for a
/// `Ruleset`-opaque op, which moves no value and names no party this filter
/// can see either way.
///
/// A malformed op contributes nothing rather than erroring: shape validation
/// ran above this at admission, and at commit an op that does not decode names
/// no account to exclude.
#[must_use]
fn party_accounts(intent: &Intent, issuer_account: Option<AccountId>) -> Vec<AccountId> {
    let mut parties = Vec::with_capacity(1 + intent.ops.len());
    if let Some(account) = issuer_account {
        push_party(&mut parties, account);
    }
    for op in &intent.ops {
        match op.op {
            LEDGER_CREDIT_OP => {
                if op.args.len() == LEDGER_CREDIT_ARGS_BYTES {
                    push_party(
                        &mut parties,
                        AccountId::new(u64::from_le_bytes(
                            op.args[0..8].try_into().expect("slice len"),
                        )),
                    );
                }
            }
            LEDGER_ITEM_TRANSFER_OP => {
                if let Ok(transfer) = ItemTransferArgs::decode(&op.args) {
                    push_party(&mut parties, transfer.seller);
                    push_party(&mut parties, transfer.buyer);
                }
            }
            // `Ruleset`-opaque (docs/08-persistence.md §2.2). Its `args` are
            // not a layout this crate may read, so it names no party here.
            _ => {}
        }
    }
    parties
}

/// `E(I)` with D10 item 4's account half applied: the announced set in
/// announced order, minus the issuer's NodeId, minus every NodeId bound to a
/// party account, **and minus every NodeId whose binding did not resolve**.
///
/// # The third term is the whole point
///
/// [D31](../../../../docs/adr/0031-id-account-subspace.md) clause (f): *a miss
/// excludes; it never admits*. The attacker decides whether a lookup misses —
/// binding a second NodeId to its own account is a credentialed operation on
/// its own account, and keeping that NodeId out of this gateway's resolver is
/// achieved by not connecting it here, or by waiting out the cache's staleness
/// bound. A predicate whose "unknown" branch the attacker selects must not
/// have "admit" on that branch, which is the objection this file already
/// raised against itself before the `id/` rows existed: *a check that fails
/// open on a miss is worse than an absent one, because it reads as coverage.*
///
/// # What closing costs, and why it is affordable
///
/// `|E(I)|` shrinks, and below [`orrery_protocol::WITNESS_SET_FLOOR_N`] that
/// is [`RejectionCause::LowPopulationEpoch`] — which is **D29's quarantined
/// provisional commit, not a refusal**. Fail-closed here means "exclude the
/// miss and let the population predicate speak": the announcement itself
/// resolved, so `low_pop(i)` is defined, and
/// [D37](../../../../docs/adr/0037-unavailable-witness-epoch.md) leaves this
/// demotion untouched while correcting D27 clause (e)'s old direction for
/// `UnknownEpoch` and `EpochStale` — those refuse, because there no
/// announcement exists to count an eligible vector from. An honest intent
/// whose witness bindings this gateway cannot fully resolve is committed
/// provisionally and finalized by replay; it is not lost.
///
/// # Announced order, unsorted and undeduplicated
///
/// `filter` over [`orrery_protocol::eligible_witnesses`]'s output, never a set
/// operation, because the recorded vector is the object an auditor draws over
/// (D27 clause (f)) and normalizing it here would silently make the audit's
/// object a different one from the announcement's.
#[must_use]
fn eligible_after_party_exclusion(
    selected: &[NodeId],
    intent: &Intent,
    parties: &[AccountId],
    bindings: &dyn crate::gateway::BindingAuthority,
) -> Vec<NodeId> {
    orrery_protocol::eligible_witnesses(selected, intent.issuer)
        .into_iter()
        .filter(|witness| {
            bindings
                .owner(witness)
                .is_some_and(|account| !parties.contains(&account))
        })
        .collect()
}

/// D27 clause (d)'s admission predicate, evaluated against one accepted epoch.
///
/// Returns the eligible vector `E(I)` the draw ran over. D27 clause (f)
/// requires the gateway to **record the eligible vector it derived over** with
/// the committed intent, because `E(I)` depends on party exclusion and party
/// exclusion matches on account-to-NodeId bindings that change over time. An
/// audit that recomputed `E(I)` a week later from current bindings could
/// derive a different `required(I)` and convict an honest gateway; the
/// recorded vector is what the audit reads instead.
///
/// # Which cell judges the intent: D30, closing D27's open question 2
///
/// `CellEpoch` is a bare `u64` handle (`persist.rs`: "wire-identical to
/// `Epoch`") and an [`Intent`] names no cell, no grid and no entity this code
/// can map to one. What the handle *does* name, exactly and under the
/// coordinator's signature, is one `(grid, cell, epoch)` — D28 clause (b)
/// makes the handle globally unique and the cell arrive signed. The gap D27
/// left open was never that the gateway cannot tell which cell an intent
/// names; it is that **nothing constrained which cell a submitter may name**,
/// because the cache resolves by handle and holds every cell any peer has
/// couriered an announcement for.
///
/// That is a quantified weakening, not untidiness. D27 §4.4's collusion
/// arithmetic `C(c,K)/C(N,K)` is stated over **one cell's** announced set, and
/// a submitter free to name another cell's epoch converts "do I hold `K` of
/// this cell's draw" into "do I hold `K` of any reachable cell's draw" — one
/// attempt per cell in the cache, at whichever cell holds the most of its
/// colluders.
///
/// [D30](../../../../docs/adr/0030-cell-epoch-standing.md) clause (a) closes
/// it with a standing predicate rather than a wire field: the issuer must hold
/// live coordinator-confirmed interest in the epoch's cell. The choice set
/// shrinks from "every cell in this gateway's cache" — a property of what
/// other peers couriered, not of the submitter at all — to the cells of the
/// issuer's own interest grant, each of which is a cell it is actually in.
/// The bound is per cell again, and the residual (a submitter may still pick
/// the best cell of its own ≤27-cell neighbourhood) is stated in the record.
///
/// # Errors
///
/// The first [`RejectionCause`] the intent's attestation set trips.
fn check_attestation_quorum(
    intent: &Intent,
    epochs: &crate::witness_epoch::WitnessEpochAuthority,
    interest: &dyn crate::gateway::InterestAuthority,
    bindings: &dyn crate::gateway::BindingAuthority,
    parties: &[AccountId],
    now_ms: u64,
) -> Result<Vec<NodeId>, RejectionCause> {
    // The epoch the intent *names*, resolved by handle. Never "the current
    // epoch": D28 clause (g) judges an intent against the announced set of the
    // epoch it names, which is what lets a co-signature collected a moment
    // before a turnover commit a moment after it.
    let Some(epoch) = epochs.resolve(intent.cell_epoch.0) else {
        return Err(RejectionCause::UnknownEpoch);
    };

    // D30 clause (a). The handle named a cell; this asks whether the issuer
    // has any business being judged there. The cell comes out of the
    // coordinator's signed announcement (D28 clause (b): "the cell arrives
    // signed, never asserted") and the standing out of the issuer's own
    // coordinator-signed interest grant, so neither term of the comparison is
    // anything the submitter said about itself.
    //
    // **Above the staleness check on purpose.** Standing is a fact about the
    // submitter and does not depend on the epoch's window, and answering
    // `EpochStale` to a peer with no standing would confirm the existence and
    // the age of a cell-epoch it has no business enumerating — the same
    // reason every quorum cause collapses to one wire code.
    if !interest.allows(
        intent.issuer,
        epoch.snapshot.grid,
        epoch.snapshot.cell,
        now_ms,
    ) {
        return Err(RejectionCause::NoStandingInCell);
    }

    if !epoch.snapshot.usable_at(now_ms) {
        return Err(RejectionCause::EpochStale);
    }

    // `E(I)` — the announced set in announced order, minus the parties.
    // Announced order is preserved because the recorded vector has to be the
    // object an auditor draws over, not a normalization of it.
    //
    // D10 item 4's account half runs here now, not just the NodeId half: a
    // candidate bound to a party account drops out, and so does one whose
    // binding did not resolve (D31 clause (f) — a miss excludes). The cost of
    // shrinking is `LowPopulationEpoch` below, which is D29's provisional
    // path, so closing here demotes rather than refuses.
    let eligible =
        eligible_after_party_exclusion(&epoch.snapshot.selected, intent, parties, bindings);
    if eligible.len() < orrery_protocol::WITNESS_SET_FLOOR_N {
        return Err(RejectionCause::LowPopulationEpoch);
    }

    // Set membership, before the draw. An attestation from outside `E(I)`
    // counts for nothing however well it verifies, and refusing the whole
    // intent rather than silently dropping the stray signature is deliberate:
    // a submitter that attached one is either broken or shopping, and both
    // want an answer rather than a mysteriously short quorum.
    //
    // The two ways to be outside `E(I)` are told apart because they describe
    // opposite situations, and D31 clause (f) asks for exactly this
    // separation: a witness the announcement never selected is a forgery or a
    // shopping attempt, while one it *did* select whose account this gateway
    // could not resolve is a resolver miss — possibly an entirely honest peer
    // connected to a sibling gateway. Labelling the second
    // `WitnessOutsideAnnouncedSet` would send an operator hunting a forgery
    // that is not there. Both answer `REASON_ATTESTATION_QUORUM` on the wire.
    for attestation in &intent.attestations {
        if !eligible.contains(&attestation.witness) {
            if epoch.snapshot.selected.contains(&attestation.witness)
                && bindings.owner(&attestation.witness).is_none()
            {
                return Err(RejectionCause::UnresolvedWitnessBinding);
            }
            return Err(RejectionCause::WitnessOutsideAnnouncedSet);
        }
    }

    // The count first, because it is the cheaper fact and the more common
    // operator situation — "the co-sign budget expired" rather than "the draw
    // did not land where the submitter hoped".
    if intent.attestations.len() < orrery_protocol::WITNESS_QUORUM_K {
        return Err(RejectionCause::ThresholdNotMet);
    }

    // The draw. This is the conjunct that makes the scheme more than a
    // counting exercise: the submitter broadcast to the full announced set
    // minus parties and submitted whatever came back inside the co-sign
    // budget, and it never learns which K of those this gateway will require —
    // because the draw key is this cluster's and stays secret until epoch end.
    for required in epoch.required_witnesses(intent.intent_id, &eligible) {
        if !intent
            .attestations
            .iter()
            .any(|attestation| attestation.witness == required)
        {
            return Err(RejectionCause::RequiredWitnessMissing);
        }
    }

    Ok(eligible)
}

/// Append `key` to an [`IntentPrecheck`]'s read set unless it is already
/// there.
///
/// Two ops in one intent naming the same balance row is ordinary (a trade's
/// debit and a separate credit in the same asset), and a duplicated key would
/// register the same conflict range twice for no benefit.
fn push_read_key(read_keys: &mut Vec<Vec<u8>>, key: Vec<u8>) {
    if !read_keys.contains(&key) {
        read_keys.push(key);
    }
}

/// Verify the bounded co-signature set carried by an intent, against
/// [D27](../../../../docs/adr/0027-attestation-envelope.md) clause (a)'s
/// witness preimage.
///
/// # What changed here, and why it is the whole point of the record
///
/// This used to verify each attestation against `Intent::signing_preimage()` —
/// **the identical bytes the issuer signs**. That is a role confusion, not a
/// subtlety: an issuer's own signature was a byte-valid `Attestation` naming
/// anybody, and a signature solicited for one role verified in the other.
/// `Attestation::verify` checks the domain-separated 157-byte attestation
/// preimage instead, which binds the co-signature to one intent hash, one
/// issuer signature, one cell-epoch and one witness identity.
///
/// # A signature over the legacy preimage is not an attestation
///
/// It does not verify here, and D27 clause (c) is explicit about what that
/// means: such a signature is **counted toward no required slot**, and on its
/// own it is not grounds to reject the intent either — an intent whose
/// attestations all fail this way is an intent with zero valid attestations,
/// which is D29's low-population case and not a forgery. That is what
/// preserves the `{V, V−1}` rolling-upgrade window across the switch without
/// a flag day, and without ever counting an undomained signature.
///
/// This function is the strict half of that: a *present* attestation that does
/// not verify is refused as [`RejectionCause::BadAttestation`], because a peer
/// that went to the trouble of attaching one is claiming it is real. The
/// permissive half — "an unverifiable attestation contributes nothing but
/// convicts nobody" — is what the quorum check below is written against: it
/// counts only witnesses that reached this point.
/// D10 item 4's account half applied to the attestations an intent actually
/// carries: no party may witness, and no account may witness twice.
///
/// # Why this is not inside [`check_attestation_quorum`]
///
/// Because it must hold with the quorum switched **off**. Party exclusion is
/// not part of D27's K-of-N predicate — it is the rule that makes a
/// co-signature mean a third party looked, and an issuer able to certify its
/// own trade through a second device defeats that at every enforcement
/// setting. [`RejectionCause::SelfWitness`] already sits above the switch for
/// exactly this reason; this is the same rule with the NodeId comparison
/// replaced by the account one.
///
/// # A miss is skipped here, and that is not fail-open
///
/// D31 clause (f)'s obligation is discharged in
/// [`eligible_after_party_exclusion`], where an unresolvable NodeId is dropped
/// from `E(I)` and an attestation from one is then refused as
/// [`RejectionCause::UnresolvedWitnessBinding`]. This function runs at every
/// enforcement mode, including `Off`, where *no* attestation counts toward
/// anything at all: there is no quorum for it to fill and no slot for it to
/// occupy. Refusing every unresolved witness here would therefore refuse every
/// attestation in a tree where nothing yet writes the `id/` rows, and would
/// buy no security in exchange, because the thing it would be protecting does
/// not exist in that mode.
fn check_attesting_accounts(
    intent: &Intent,
    parties: &[AccountId],
    bindings: &dyn crate::gateway::BindingAuthority,
) -> Result<(), RejectionCause> {
    let mut seen: Vec<AccountId> = Vec::with_capacity(intent.attestations.len());
    for attestation in &intent.attestations {
        let Some(account) = bindings.owner(&attestation.witness) else {
            continue;
        };
        // Before the duplicate check, because a party that attested twice is a
        // party first: answering `DuplicateAttestingAccount` would name the
        // lesser of the two facts.
        if parties.contains(&account) {
            return Err(RejectionCause::PartyAccountWitness);
        }
        if seen.contains(&account) {
            return Err(RejectionCause::DuplicateAttestingAccount);
        }
        seen.push(account);
    }
    Ok(())
}

fn check_attestations(intent: &Intent) -> Result<(), RejectionCause> {
    if intent.attestations.is_empty() {
        return Ok(());
    }

    let mut seen: Vec<NodeId> = Vec::with_capacity(intent.attestations.len());
    for attestation in &intent.attestations {
        if seen.contains(&attestation.witness) {
            return Err(RejectionCause::DuplicateAttestation);
        }
        seen.push(attestation.witness);
        if !attestation.verify(intent) {
            return Err(RejectionCause::BadAttestation);
        }
    }
    Ok(())
}

impl BaselineIntentValidator {
    /// The verdict, with the cause preserved for logging.
    ///
    /// # Errors
    ///
    /// Returns the first [`RejectionCause`] the intent trips. Good sessions are
    /// checked cheapest first so an oversized or malformed submission never
    /// pays for signature verification. A quarantined session verifies its
    /// bounded attestation set before returning any other rejection.
    pub fn check(intent: &Intent, cx: &IntentContext) -> Result<IntentPrecheck, RejectionCause> {
        if cx.standing == SessionStanding::Quarantined {
            if intent.attestations.len() > MAX_ATTESTATIONS {
                return Err(RejectionCause::TooManyAttestations);
            }
            check_attestations(intent)?;
        }
        if intent.ops.is_empty() {
            return Err(RejectionCause::NoOps);
        }
        if intent.ops.len() > MAX_OPS_PER_INTENT {
            return Err(RejectionCause::TooManyOps);
        }
        if intent.attestations.len() > MAX_ATTESTATIONS {
            return Err(RejectionCause::TooManyAttestations);
        }

        // ── Party exclusion: the issuer may not witness its own intent ──────
        //
        // D10 item 4 seeds the witness set "excluding **all parties to the
        // intent**"; docs/07 §4.2 makes the gateway enforce it independently
        // of who selected the set, because a gateway must never assume a set
        // it did not choose is well-formed.
        //
        // **What this prevents.** Attestations are not yet counted toward a
        // threshold, so today a self-attestation buys nothing. The K-of-N
        // enforcement of #147 makes them load-bearing, and on that day an
        // issuer that can appear in its own attestation list is signing its
        // own permission slip — it supplies K of the K signatures a durable
        // trade needs, and the co-signature requirement that exists to make a
        // counterparty's consent unforgeable proves nothing at all.
        //
        // **Why it is worse than it looks in this tree.** The loop below
        // verifies a witness signature over `Intent::signing_preimage()` — the
        // *identical bytes the issuer already signed* (persist.rs's preimage
        // is deliberately attestation-excluding, so co-signatures can be
        // appended without invalidating the author). An issuer therefore does
        // not even need a fresh signature: copying `intent.signature` verbatim
        // into an `Attestation { witness: intent.issuer, .. }` yields an
        // attestation that verifies. D27 closes *that* variant by giving a
        // witness its own domain-separated preimage, but a domain tag cannot
        // stop an issuer from correctly signing the witness preimage too. The
        // party check is required either way, which is why it is written here
        // rather than deferred to the envelope work.
        //
        // **Why it sits this early.** It is two NodeId comparisons — cheaper
        // than the arg-size walk below it, let alone the ed25519 verification
        // at the bottom — and it needs no durable row, so nothing downstream
        // of it should ever pay for a self-witnessed intent. Placing it above
        // the ops loop also means `check` cannot return a read plan for one:
        // an `IntentPrecheck`'s `read_keys` are exactly the durable rows the
        // executor will read, and a refusal here is a refusal before any of
        // them is named.
        //
        // Both identities are compared because they answer different
        // questions and only the gateway makes them agree: `intent.issuer` is
        // who signed the envelope, `cx.issuer` is the connection the envelope
        // arrived on. The gateway binds them before this runs, but a validator
        // that silently depends on a caller having done so is a validator that
        // stops holding the moment someone calls it directly.
        for attestation in &intent.attestations {
            if attestation.witness == intent.issuer || attestation.witness == cx.issuer {
                return Err(RejectionCause::SelfWitness);
            }
        }

        let mut total_args = 0usize;
        let mut read_keys = Vec::new();
        for op in &intent.ops {
            if op.args.len() > MAX_OP_ARGS_BYTES {
                return Err(RejectionCause::ArgsTooLarge);
            }
            total_args += op.args.len();
            if total_args > MAX_INTENT_ARGS_BYTES {
                return Err(RejectionCause::ArgsTooLarge);
            }
            match op.op {
                LEDGER_CREDIT_OP => {
                    if op.args.len() != LEDGER_CREDIT_ARGS_BYTES {
                        return Err(RejectionCause::MalformedLedgerOp);
                    }
                    let account = AccountId::new(u64::from_le_bytes(
                        op.args[0..8].try_into().expect("slice len"),
                    ));
                    // A session's account is the only account this connection
                    // may move value in. `None` is the unauthenticated case
                    // and is refused for the same reason, not a different one.
                    if cx.account != Some(account) {
                        return Err(RejectionCause::LedgerOpForAnotherAccount);
                    }
                    let asset = AssetId::new(u64::from_le_bytes(
                        op.args[8..16].try_into().expect("slice len"),
                    ));
                    push_read_key(
                        &mut read_keys,
                        crate::keyspace::ledger_bal_key(account, asset).to_vec(),
                    );
                }
                LEDGER_ITEM_TRANSFER_OP => {
                    let transfer = ItemTransferArgs::decode(&op.args)?;
                    if transfer.seller == transfer.buyer {
                        return Err(RejectionCause::ItemTransferToSelf);
                    }
                    // The **debit side** is the buyer, and it is the only side
                    // this filter can authorize: the seller's balance is a
                    // blind credit (nobody is harmed by being paid) and the
                    // ownership row is guarded durably by the owner check the
                    // executor runs inside the transaction.
                    if cx.account != Some(transfer.buyer) {
                        return Err(RejectionCause::ItemTransferForAnotherAccount);
                    }
                    // Both rows the executor reads before it writes anything,
                    // named up front so a `Ruleset`-linked executor registers
                    // their conflict ranges at step 1 (§7). The item row is
                    // the one that matters: it is the read two concurrent
                    // transfers of the same item share, and therefore the read
                    // that makes at most one of them commit.
                    push_read_key(
                        &mut read_keys,
                        crate::keyspace::ledger_item_key(transfer.item).to_vec(),
                    );
                    push_read_key(
                        &mut read_keys,
                        crate::keyspace::ledger_bal_key(transfer.buyer, transfer.asset).to_vec(),
                    );
                }
                // `Ruleset`-opaque (docs/08-persistence.md §2.2): checked for
                // size above and for nothing else.
                _ => {}
            }
        }

        // Signature work stays last for a good session: it is the only
        // non-trivial cost here, and an intent that fails any check above must
        // never pay it. Quarantined sessions paid this bounded cost before
        // shape validation so a forged co-signature cannot hide behind a
        // cheaper rejection.
        if cx.standing == SessionStanding::Good {
            check_attestations(intent)?;
        }

        Ok(IntentPrecheck { read_keys })
    }
}

impl BaselineIntentValidator {
    /// The full admission decision at `now_ms`: the envelope checks of
    /// [`Self::check`], then — when this validator enforces — D27's K-of-N
    /// predicate against the epoch the intent names.
    ///
    /// The clock is a parameter rather than read here so a test can place an
    /// intent inside or outside an epoch's usability window without sleeping
    /// through it; [`IntentValidator::validate`] passes the registrar clock,
    /// which is the same monotonic source the epoch cache stamps acceptance
    /// with.
    ///
    /// # D29 clause 2's admission function lives here
    ///
    /// ```text
    /// attested(i)                                ->  commit, finality = Final
    /// !attested(i) && low_pop(i) && reversible(i) ->  commit, finality = Provisional
    /// otherwise                                   ->  refuse
    /// ```
    ///
    /// with `low_pop(i)` evaluated against the **announced** `epoch/{cell_id}`
    /// record with the intent's parties removed — never live presence, which a
    /// submitter can manufacture by dropping its friends' sessions. The
    /// announced set is coordinator-seeded, rate-limited against reseed
    /// grinding and durable, so `|elig(i)|` is a fact about a committed record
    /// rather than about the instant of submission.
    ///
    /// The third line is the one that matters most, and it is why
    /// [`RejectionCause::ThresholdNotMet`] and
    /// [`RejectionCause::RequiredWitnessMissing`] stay refusals: an intent that
    /// *could* have been attested and simply was not is refused. Provisional
    /// commit is not a general-purpose relief valve for a missing signature; it
    /// is the answer to one specific fact about the world, namely that there
    /// was nobody there to sign.
    ///
    /// # Which causes reach the provisional path
    ///
    /// Exactly one: [`RejectionCause::LowPopulationEpoch`], which is
    /// `|elig(i)| < N` and nothing else.
    ///
    /// [`RejectionCause::UnknownEpoch`] and [`RejectionCause::EpochStale`]
    /// refuse. What this function once documented as a deliberate divergence
    /// from D27 clause (e)'s "in all three cases ... provisional commit,
    /// never refusal" is no longer a divergence:
    /// [D37](../../../../docs/adr/0037-unavailable-witness-epoch.md) resolved
    /// the contradiction by erratum and corrected D27 clause (e)'s two
    /// unavailable-input cases to refusals with a bounded cure. The reasoning
    /// this function already followed is now the accepted position: D29
    /// clause 2's second line needs `low_pop(i)` to be *defined*, and for an
    /// unresolvable or expired handle `E(c,e)` does not exist — so it is not.
    /// Absence of the announcement is submitter-selectable, which is why no
    /// branch reachable by withholding it may admit.
    ///
    /// [`RejectionCause::NoStandingInCell`] is emphatically **not** on this
    /// path either, and D30 says why in as many words: it describes a
    /// submitter that asked to be judged somewhere it does not stand — a
    /// refusal at every population.
    ///
    /// # Errors
    ///
    /// The first [`RejectionCause`] the intent trips.
    pub fn check_at(
        &self,
        intent: &Intent,
        cx: &IntentContext,
        now_ms: u64,
    ) -> Result<Admission, RejectionCause> {
        // D32 clause (e)'s coverage **denominator**, counted here and nowhere
        // else. Before `check`, not after, and unconditionally on the posture:
        // an intent refused by clause (b)'s always-on correctness set never
        // reaches the shadow arm, and a validator posted in `Off` never
        // evaluates one at all. Both are qualifying activity that went
        // unobserved, and a denominator that could not see them would report
        // full coverage of a control nobody ran — which is the exact evidence
        // D32 calls "blindness with a clean conscience". The numerator is
        // counted at the far end of this function, in `observe`.
        if let Some(observer) = self.observer.as_deref() {
            observer.record_qualifying(cx.account);
        }
        let precheck = Self::check(intent, cx)?;

        // The resolver, or the honest absence of one. A validator built
        // without bindings answers `None` to every `owner(n)`, which leaves
        // the pre-D31 NodeId-only behaviour in `Off` and excludes everybody in
        // `Required` — the fail-closed direction in both cases.
        static UNBOUND: crate::gateway::UnboundBindingAuthority =
            crate::gateway::UnboundBindingAuthority;
        let bindings: &dyn crate::gateway::BindingAuthority =
            self.bindings.as_deref().unwrap_or(&UNBOUND);

        // `P(I)`, derived once and used twice: by the account-level party
        // refusal immediately below, and by `E(I)`'s derivation inside the
        // quorum. The issuer's own account comes from its verified session
        // token when there is one, and from the resolver otherwise — never
        // from the intent, which is peer-authored.
        let parties = party_accounts(
            intent,
            cx.account.or_else(|| bindings.owner(&intent.issuer)),
        );

        // **Above the enforcement switch on purpose.** D10 item 4's party
        // exclusion is not part of the K-of-N quorum and does not switch off
        // with it, the same way the NodeId-level `SelfWitness` refusal in
        // `check` does not.
        check_attesting_accounts(intent, &parties, bindings)?;

        let enforcement = self.enforcement.get();
        if !enforcement.evaluates() {
            // No quorum is enforced, so no intent is *un*attested in the sense
            // clause 2 means, and the provisional path is unreachable. That is
            // the correct reading rather than a shortcut: D29's second line
            // needs `low_pop(i)`, and `low_pop` is a statement about an
            // announced set this validator is not consulting.
            return Ok(Admission::Attested(precheck));
        }
        // Enforcing with no cache configured is a misconfiguration, and it
        // fails closed: a gateway that cannot resolve any epoch holds no
        // announcement for any intent's cell-epoch, which is exactly
        // `UnknownEpoch` and not a special case.
        //
        // Shadow has no closed direction to fail in, because it never acts, so
        // D32 clause (b)'s degraded arm applies instead: record the
        // misconfiguration as *unevaluated* and admit. Borrowing `UnknownEpoch`
        // here would be worse than useless — it would enter clause (e)'s
        // `fp_count` as a would-be refusal of every honest account on the
        // cluster, and a gateway that cannot evaluate anything would read as a
        // control that refuses everything.
        let Some(epochs) = self.epochs.as_ref() else {
            if !enforcement.acts() {
                return Ok(self.observe(
                    intent,
                    cx,
                    now_ms,
                    precheck,
                    ShadowVerdict::Unevaluated(ShadowUnevaluated::NoEpochAuthority),
                ));
            }
            return Err(RejectionCause::UnknownEpoch);
        };
        // The same fail-closed reading for D30's second authority: a validator
        // enforcing with no interest authority can establish standing for
        // nobody, which is exactly `NoStandingInCell`. `enforcing` and
        // `shadow` both take all three, so this arm is unreachable through the
        // constructors and is written out rather than unwrapped.
        let Some(interest) = self.interest.as_ref() else {
            if !enforcement.acts() {
                return Ok(self.observe(
                    intent,
                    cx,
                    now_ms,
                    precheck,
                    ShadowVerdict::Unevaluated(ShadowUnevaluated::NoInterestAuthority),
                ));
            }
            return Err(RejectionCause::NoStandingInCell);
        };
        // The quorum runs *after* every shape check and after the envelope's
        // signature work, preserving the "signature work last" property this
        // filter already documents: an oversized or malformed submission never
        // pays for a witness-set resolution, and a self-witnessed one never
        // reaches the draw.
        let quorum = check_attestation_quorum(
            intent,
            epochs.as_ref(),
            interest.as_ref(),
            bindings,
            &parties,
            now_ms,
        );

        // D32 clause (b) obligation (1) is discharged above this line and
        // obligations (2) and (3) below it: the predicate has already run in
        // full, with every sub-predicate live mode evaluates, and the *only*
        // thing the mode decides is what happens to its answer.
        if !enforcement.acts() {
            let verdict = match quorum {
                Ok(_eligible) => ShadowVerdict::WouldAdmit,
                // The provisional branch is evaluated rather than assumed: D29
                // clause 3's `reversible(i)` is one of the sub-predicates live
                // mode would run, and a shadow that recorded every
                // low-population intent as "would have committed
                // provisionally" would hide the ones live mode would have
                // refused outright.
                Err(RejectionCause::LowPopulationEpoch) => {
                    match provisional::classify(intent, cx.account) {
                        Ok(()) => ShadowVerdict::WouldCommitProvisionally,
                        Err(cause) => ShadowVerdict::WouldRefuse(cause),
                    }
                }
                Err(cause) => ShadowVerdict::WouldRefuse(cause),
            };
            return Ok(self.observe(intent, cx, now_ms, precheck, verdict));
        }

        match quorum {
            Ok(_eligible) => Ok(Admission::Attested(precheck)),
            // The second line of clause 2's admission function, in full: the
            // population predicate held, so `reversible(i)` decides. A failure
            // here is a *refusal*, not a fallback to the fallback — an intent
            // that is both unwitnessable and unreversible has no safe home.
            Err(RejectionCause::LowPopulationEpoch) => {
                provisional::classify(intent, cx.account)?;
                Ok(Admission::Provisional(precheck))
            }
            Err(cause) => Err(cause),
        }
    }

    /// Record one shadow verdict and admit the intent regardless of it.
    ///
    /// The whole of D32 clause (b)'s third obligation is the return value:
    /// [`Admission::Attested`], the same answer [`AttestationEnforcement::Off`]
    /// gives, whatever `verdict` says. Not `Provisional` even when the verdict
    /// is [`ShadowVerdict::WouldCommitProvisionally`] — the quarantine, the
    /// outstanding-cap accounting and the annulment deadline D29's path
    /// applies are *actions*, and a shadow period that started quarantining
    /// intents would be live under a different name.
    ///
    /// One function rather than an inline tuple at each of the three call
    /// sites, so "record, then admit" is a single statement no future arm can
    /// half-implement.
    fn observe(
        &self,
        intent: &Intent,
        cx: &IntentContext,
        now_ms: u64,
        precheck: IntentPrecheck,
        verdict: ShadowVerdict,
    ) -> Admission {
        shadow::emit(
            self.observer.as_deref(),
            ShadowObservation {
                intent_id: intent.intent_id,
                issuer: intent.issuer,
                subject: cx.account,
                cell_epoch: intent.cell_epoch,
                verdict,
                observed_at_ms: now_ms,
            },
        );
        Admission::Attested(precheck)
    }
}

/// Which of D29 clause 2's two admitting outcomes an intent reached.
///
/// A two-arm enum rather than a boolean on [`IntentPrecheck`], for the reason
/// D29 gives for spending an [`IntentOutcome`] arm instead of a flag: the
/// compiler then names every site that has to decide. The two destinations
/// differ in the durable finality written, the outcome the client is told, and
/// whether the value is an input to anything — which is more than a
/// `provisional: bool` on an otherwise identical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Commit with the cluster standing behind it.
    Attested(IntentPrecheck),
    /// Commit on D29's low-population path, quarantined until spot replay.
    Provisional(IntentPrecheck),
}

impl Admission {
    /// The read plan, whichever path admitted the intent.
    #[must_use]
    pub fn precheck(&self) -> &IntentPrecheck {
        match self {
            Self::Attested(precheck) | Self::Provisional(precheck) => precheck,
        }
    }

    /// Whether this admission is D29's low-population path.
    #[must_use]
    pub const fn is_provisional(&self) -> bool {
        matches!(self, Self::Provisional(_))
    }
}

impl IntentValidator for BaselineIntentValidator {
    fn validate(&self, intent: &Intent, cx: &IntentContext) -> IntentVerdict {
        match self.check_at(intent, cx, crate::lease::registrar_now_ms()) {
            Ok(Admission::Attested(precheck)) => IntentVerdict::Admit(precheck),
            Ok(Admission::Provisional(precheck)) => {
                // Info, not debug: a provisional commit is a durable write the
                // cluster has not yet stood behind, and an operator reading a
                // nonzero rate of these is reading a fact about how empty the
                // world is. It is not an error and it is not routine either.
                tracing::info!(
                    intent_id = intent.intent_id,
                    issuer = %cx.issuer,
                    "intent admitted to the low-population provisional path"
                );
                IntentVerdict::AdmitProvisional(precheck)
            }
            Err(cause) => {
                // Debug, not warn: a rejection is an ordinary answer to a bad
                // request, and the gateway's `rejected` counter is the signal
                // an operator watches. This is what turns that count into a
                // diagnosis.
                tracing::debug!(
                    intent_id = intent.intent_id,
                    issuer = %cx.issuer,
                    cause = cause.as_str(),
                    "intent admission refused"
                );
                IntentVerdict::Reject {
                    reason: cause.wire_reason(),
                }
            }
        }
    }
}

/// An executor failure that is **not** a validation rejection.
#[derive(Debug)]
pub enum IntentError {
    /// The serializable transaction exhausted its bounded conflict retries
    /// (docs/08-persistence.md §7: "after 5 conflict retries … the gateway
    /// returns a definitive refusal"). Maps to
    /// `IntentOutcome::Rejected { reason: REASON_CONTENTION_EXHAUSTED }`.
    ContentionExhausted,
    /// The store failed for a non-conflict reason.
    Store(String),
}

impl core::fmt::Display for IntentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ContentionExhausted => write!(f, "intent contention retries exhausted"),
            Self::Store(s) => write!(f, "intent store: {s}"),
        }
    }
}

impl core::error::Error for IntentError {}

/// One durable write an intent's ops have earned, staged but not yet applied.
///
/// Staging exists so that **nothing is written until every op has passed its
/// checks**. An intent whose second op is refused must leave the ledger
/// exactly as it found it, and on the FDB path that is not automatic: writes
/// staged in a transaction are committed by `db.run` whether or not the
/// closure decided it wanted them, so "return early on a rejection" would
/// commit the earlier ops' effects. Deciding first and writing second removes
/// that class of bug rather than documenting around it, and it is also the
/// read-check-write order docs/08-persistence.md §7 specifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlannedWrite {
    /// A little-endian `MutationType::Add` of `delta` on
    /// `ledger/bal/{account}/{asset}`.
    BalanceAdd {
        /// The account whose balance row moves.
        account: AccountId,
        /// The asset the balance is denominated in.
        asset: AssetId,
        /// The signed delta; negative on the debit side of a trade.
        delta: i64,
    },
    /// A `set` of `ledger/item/{item}` to its new owner, carrying the
    /// `Ruleset`-opaque state across unchanged.
    ItemOwner {
        /// The item whose ownership row moves.
        item: orrery_protocol::ItemUid,
        /// Owner observed inside the transaction before this write. `None`
        /// is the honest prior state of a mint.
        before: Option<AccountId>,
        /// The row's new value.
        row: crate::keyspace::ItemRow,
    },
}

/// A non-empty set of ledger writes coupled to its mandatory receipt.
///
/// There is deliberately no constructor taking writes alone. Both executors
/// accept this bundle at their guarded mutation stage, so adding a new ledger
/// write cannot compile there unless the receipt generated from the same plan
/// travels with it. The fields stay private to keep that invariant local to
/// [`IntentPlan::into_mutation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LedgerMutation {
    writes: Vec<PlannedWrite>,
    receipt: crate::keyspace::ReceiptRow,
}

impl LedgerMutation {
    /// Build a receipt from the exact writes it guards. An empty write set is
    /// not a ledger mutation and therefore produces no bundle.
    fn from_writes(
        intent_id: u128,
        parties: Vec<AccountId>,
        ops: Vec<u16>,
        writes: Vec<PlannedWrite>,
    ) -> Option<Self> {
        if writes.is_empty() {
            return None;
        }
        let balance_deltas = writes
            .iter()
            .filter_map(|write| match write {
                PlannedWrite::BalanceAdd {
                    account,
                    asset,
                    delta,
                } => Some(crate::keyspace::ReceiptBalanceDelta {
                    account: *account,
                    asset: *asset,
                    delta: *delta,
                }),
                PlannedWrite::ItemOwner { .. } => None,
            })
            .collect();
        let ownership = writes
            .iter()
            .filter_map(|write| match write {
                PlannedWrite::ItemOwner { item, before, row } => {
                    Some(crate::keyspace::ReceiptOwnershipTransition {
                        item: *item,
                        before: *before,
                        after: Some(row.owner),
                    })
                }
                PlannedWrite::BalanceAdd { .. } => None,
            })
            .collect();
        Some(Self {
            writes,
            receipt: crate::keyspace::ReceiptRow {
                intent_id,
                parties,
                ops,
                balance_deltas,
                ownership,
            },
        })
    }

    fn writes(&self) -> &[PlannedWrite] {
        &self.writes
    }

    fn receipt(&self) -> &crate::keyspace::ReceiptRow {
        &self.receipt
    }
}

/// The in-flight decision about one intent: what the ledger currently says,
/// what this intent will write, and who it moved value between.
///
/// Shared by [`MemIntentExecutor`] and the FDB executor so the two tiers
/// cannot drift on op semantics. The rows are **loaded by the caller** —
/// synchronously from a `HashMap` in one case, awaited from FDB in the other —
/// and cached here, which is what lets one body of checking logic serve both.
///
/// The cache is also a read-your-own-writes view: an intent carrying two
/// transfers of the same item sees the first one's effect when it checks the
/// second, so it cannot move one item twice by naming it twice.
#[derive(Debug, Default)]
pub(crate) struct IntentPlan {
    /// `ledger/item/{uid}` as this intent currently sees it. `None` means the
    /// row is absent; a missing key means it has not been loaded yet.
    items: HashMap<orrery_protocol::ItemUid, Option<crate::keyspace::ItemRow>>,
    /// `ledger/bal/{account}/{asset}` as this intent currently sees it.
    balances: HashMap<(AccountId, AssetId), i128>,
    /// The writes earned so far, in the order §7 applies them.
    writes: Vec<PlannedWrite>,
    /// The accounts a transfer moved value between, first-seen order — the
    /// `parties` field of the `ledger/receipt/` row.
    parties: Vec<AccountId>,
}

impl IntentPlan {
    /// Whether `item`'s row has been loaded into the view yet.
    fn has_item(&self, item: orrery_protocol::ItemUid) -> bool {
        self.items.contains_key(&item)
    }

    /// Seed the view with `item`'s durable row (or its absence).
    fn load_item(&mut self, item: orrery_protocol::ItemUid, row: Option<crate::keyspace::ItemRow>) {
        self.items.insert(item, row);
    }

    /// Whether `(account, asset)`'s balance has been loaded into the view yet.
    fn has_balance(&self, account: AccountId, asset: AssetId) -> bool {
        self.balances.contains_key(&(account, asset))
    }

    /// Seed the view with a durable balance.
    fn load_balance(&mut self, account: AccountId, asset: AssetId, value: i128) {
        self.balances.insert((account, asset), value);
    }

    /// Stage a [`LEDGER_CREDIT_OP`]'s blind increment.
    fn credit(&mut self, account: AccountId, asset: AssetId, delta: i64) {
        if let Some(balance) = self.balances.get_mut(&(account, asset)) {
            *balance += i128::from(delta);
        }
        self.writes.push(PlannedWrite::BalanceAdd {
            account,
            asset,
            delta,
        });
        self.note_party(account);
    }

    /// Check a transfer against the current view and, if it passes, stage its
    /// three writes and update the view.
    fn transfer(&mut self, t: &ItemTransferArgs) -> OpsVerdict {
        let row = self.items.get(&t.item).and_then(Option::as_ref);
        let balance = self.balances.get(&(t.buyer, t.asset)).copied().unwrap_or(0);
        let verdict = item_transfer_verdict(t, row, balance);
        if verdict != OpsVerdict::Applied {
            return verdict;
        }
        // Unwrapping is sound: `item_transfer_verdict` returned `Applied`,
        // which it only does after finding the row present and owned.
        let mut row = self
            .items
            .get(&t.item)
            .and_then(Clone::clone)
            .expect("checked present");
        let before = Some(row.owner);
        row.owner = t.buyer;
        self.items.insert(t.item, Some(row.clone()));
        self.writes.push(PlannedWrite::ItemOwner {
            item: t.item,
            before,
            row,
        });
        for party in [t.seller, t.buyer] {
            self.note_party(party);
        }
        // The debit side keeps its read (the balance check above); the credit
        // side is blind, exactly as §7 specifies. Both are `Add`s here — what
        // makes the debit safe is that it was *checked* against a value read
        // in this same transaction, not that it is written differently.
        self.balance_delta(t.buyer, t.asset, -t.price);
        self.balance_delta(t.seller, t.asset, t.price);
        OpsVerdict::Applied
    }

    /// Stage a balance move and keep the view consistent with it.
    fn balance_delta(&mut self, account: AccountId, asset: AssetId, delta: i64) {
        if let Some(balance) = self.balances.get_mut(&(account, asset)) {
            *balance += i128::from(delta);
        }
        self.writes.push(PlannedWrite::BalanceAdd {
            account,
            asset,
            delta,
        });
        self.note_party(account);
    }

    fn note_party(&mut self, account: AccountId) {
        if !self.parties.contains(&account) {
            self.parties.push(account);
        }
    }

    /// Consume the plan into the only type the mutation stage accepts.
    ///
    /// An intent carrying only Ruleset-opaque ops has no cluster-interpreted
    /// ledger writes and therefore no mutation bundle. Every non-empty write
    /// set gets exactly one receipt generated from those same writes.
    fn into_mutation(self, intent: &Intent) -> Option<LedgerMutation> {
        LedgerMutation::from_writes(
            intent.intent_id,
            self.parties,
            intent.ops.iter().map(|op| op.op).collect(),
            self.writes,
        )
    }
}

/// The durable half of a [`LEDGER_ITEM_TRANSFER_OP`]'s checks, given the rows
/// the executor has already read.
///
/// Defined once and called by both executors, so "what does the ledger refuse"
/// has one implementation and the `fdb` and non-`fdb` test tiers cannot drift
/// apart on it. It is deliberately pure: the caller does the reading, this
/// decides, and the caller then writes — which is the read-check-write order
/// of docs/08-persistence.md §7 expressed in a type signature.
///
/// `item` is the decoded `ledger/item/{item_uid}` row, `None` when the key is
/// absent. `buyer_balance` is the debit side's `ledger/bal/{buyer}/{asset}`
/// value, `0` when the key is absent — an account with no row has no money,
/// which is the same thing.
#[must_use]
pub fn item_transfer_verdict(
    transfer: &ItemTransferArgs,
    item: Option<&crate::keyspace::ItemRow>,
    buyer_balance: i128,
) -> OpsVerdict {
    // Checked first because it is true or false regardless of what the rows
    // say, so a self-transfer gets its own answer instead of whichever durable
    // check it happens to trip on the way past.
    if transfer.seller == transfer.buyer {
        return OpsVerdict::Rejected(orrery_protocol::REASON_ITEM_TRANSFER_TO_SELF);
    }
    let Some(item) = item else {
        return OpsVerdict::Rejected(orrery_protocol::REASON_NO_SUCH_ITEM);
    };
    // §7's `Reject::NotOwner`, and **also** what losing a double-spend race
    // looks like from inside the retry: the loser's second attempt re-reads a
    // row the winner just rewrote, and refuses honestly rather than
    // committing a second transfer of the same item.
    if item.owner != transfer.seller {
        return OpsVerdict::Rejected(orrery_protocol::REASON_NOT_ITEM_OWNER);
    }
    if buyer_balance < i128::from(transfer.price) {
        return OpsVerdict::Rejected(orrery_protocol::REASON_INSUFFICIENT_BALANCE);
    }
    OpsVerdict::Applied
}

/// The execution seam (D11 §2.2, second stage): runs the intent inside one
/// serializable transaction and returns the recorded outcome.
///
/// Implementations must uphold the idempotency contract: the
/// `intent/{intent_id}` row is read **first** (registering its conflict
/// range), and a replayed intent returns the recorded outcome unchanged
/// rather than applying twice (docs/08-persistence.md §7 step 0).
#[async_trait::async_trait]
pub trait IntentExecutor: Send + Sync {
    /// Execute `intent`, returning its outcome. The gateway acks only after
    /// this future resolves.
    async fn execute(&self, intent: &Intent) -> Result<IntentOutcome, IntentError>;

    /// Execute `intent` on D29's low-population path, attributing the
    /// quarantine to `account`.
    ///
    /// The durable effects are applied exactly as [`Self::execute`] applies
    /// them — this is a real commit, and the reply is sent after the
    /// transaction resolves, so RPO 0 is untouched. What differs is what the
    /// row records and what the ledger then refuses: the
    /// `intent/{intent_id}` row carries
    /// [`crate::keyspace::IntentFinality::Provisional`] and a finalization
    /// deadline, the account's `provisional/{account}` row gains a hold
    /// naming every balance the intent wrote, and any later intent that names
    /// one of those balances is refused with
    /// [`orrery_protocol::REASON_PROVISIONAL_INPUT`] until this one is
    /// finalized.
    ///
    /// # Why a second method rather than a parameter on `execute`
    ///
    /// `execute` is implemented outside this crate — the harnesses carry
    /// tripwire executors that assert it is never reached — and widening its
    /// signature would make every one of them a compile error to gain a
    /// parameter all but one of them would ignore. A defaulted second method
    /// leaves the existing seam exactly as it was.
    ///
    /// # Errors
    ///
    /// The default implementation is
    /// [`IntentError::Store`], and that is the honest answer rather than a
    /// silent refusal: a gateway configured to enforce D27's quorum, handed an
    /// executor with no provisional path, is misconfigured. Reporting it as an
    /// executor fault puts it in the operator's error budget instead of in a
    /// player's rejection rate.
    async fn execute_provisional(
        &self,
        intent: &Intent,
        account: AccountId,
    ) -> Result<IntentOutcome, IntentError> {
        let _ = (intent, account);
        Err(IntentError::Store(
            "executor has no D29 provisional path".to_owned(),
        ))
    }
}

/// Everything one [`MemIntentExecutor`] owns, behind **one** lock.
///
/// One lock rather than one per map, because the contract this type exists to
/// imitate is *atomicity*: an intent that moves an item and two balances must
/// be all-or-nothing, and four independent mutexes would let a second intent
/// observe the ledger halfway through the first. Holding it across the whole
/// of `execute` is safe because that function has no await points — the FDB
/// executor is where the awaiting happens.
#[derive(Debug, Default)]
struct MemLedger {
    /// Recorded rows by `intent_id` — the `intent/{intent_id}` store,
    /// carrying the outcome *and* D29 clause 5's finality, exactly as the
    /// durable row does.
    outcomes: HashMap<u128, crate::keyspace::IntentRow>,
    /// The `provisional/{account}` store: each account's unfinalized
    /// provisional intents.
    provisional: HashMap<AccountId, crate::keyspace::ProvisionalRow>,
    /// The `ledger/item/{item_uid}` rows.
    items: HashMap<orrery_protocol::ItemUid, crate::keyspace::ItemRow>,
    /// The `ledger/bal/{account}/{asset}` rows. An absent key is zero.
    balances: HashMap<(AccountId, AssetId), i128>,
    /// The `ledger/receipt/{versionstamp}` rows, in commit order — which is
    /// what the versionstamp gives them durably.
    receipts: Vec<crate::keyspace::ReceiptRow>,
    /// The `pid/next` counter analogue.
    next_pid: u64,
    /// Tick counter standing in for the FDB commit-version tick.
    next_tick: u64,
}

/// An in-memory [`IntentExecutor`] with the FDB path's observable contract:
/// the idempotency row is honoured (a replay returns the first outcome), ids
/// are minted from a counter, each op produces one minted id, and both
/// executor-interpreted ops — [`LEDGER_CREDIT_OP`] and
/// [`LEDGER_ITEM_TRANSFER_OP`] — have the same durable semantics and the same
/// refusals, because both executors run the same [`IntentPlan`]. Used by the
/// gateway tests so the executor path is exercised without a live FDB.
///
/// What it deliberately cannot imitate is FDB's *optimistic* concurrency: this
/// one serializes on a mutex, so two concurrent transfers of the same item are
/// ordered rather than one of them being aborted by the resolver. The
/// observable outcome is the same — exactly one owner afterwards, the loser
/// refused with `REASON_NOT_ITEM_OWNER` — but only the `fdb` tier proves that
/// a *conflict range* is what produces it.
pub struct MemIntentExecutor {
    ledger: std::sync::Mutex<MemLedger>,
}

impl MemIntentExecutor {
    /// An empty executor; the first intent commits at tick 1 and mints from
    /// `PersistId` 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ledger: std::sync::Mutex::new(MemLedger {
                next_pid: 1,
                ..MemLedger::default()
            }),
        }
    }

    /// Place `item` under `owner` with the given `Ruleset`-opaque state, as an
    /// offline seeder or a loot grant would.
    ///
    /// There is no intent op that *mints* an item — creation is a `Ruleset`
    /// concern the cluster does not interpret (docs/08-persistence.md §2.2) —
    /// so a test needs a way to put one on the ledger before trading it.
    pub fn seed_item(&self, item: orrery_protocol::ItemUid, owner: AccountId, state: Vec<u8>) {
        self.ledger
            .lock()
            .expect("mutex")
            .items
            .insert(item, crate::keyspace::ItemRow { owner, state });
    }

    /// The account currently named by `ledger/item/{item}`, or `None` if the
    /// row is absent.
    #[must_use]
    pub fn item_owner(&self, item: orrery_protocol::ItemUid) -> Option<AccountId> {
        self.ledger
            .lock()
            .expect("mutex")
            .items
            .get(&item)
            .map(|row| row.owner)
    }

    /// Add `delta` to `ledger/bal/{account}/{asset}` outside any intent — the
    /// seeding counterpart of [`Self::seed_item`].
    pub fn credit(&self, account: AccountId, asset: AssetId, delta: i128) {
        *self
            .ledger
            .lock()
            .expect("mutex")
            .balances
            .entry((account, asset))
            .or_insert(0) += delta;
    }

    /// The value at `ledger/bal/{account}/{asset}`; an absent row is zero.
    #[must_use]
    pub fn balance(&self, account: AccountId, asset: AssetId) -> i128 {
        self.ledger
            .lock()
            .expect("mutex")
            .balances
            .get(&(account, asset))
            .copied()
            .unwrap_or(0)
    }

    /// The `ledger/receipt/` audit trail so far, in commit order.
    #[must_use]
    pub fn receipts(&self) -> Vec<crate::keyspace::ReceiptRow> {
        self.ledger.lock().expect("mutex").receipts.clone()
    }
}

impl Default for MemIntentExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl MemIntentExecutor {
    /// The whole of §7 for this tier, with D29's provisional path folded in.
    ///
    /// `provisional` is `Some(account)` when the gateway admitted the intent
    /// to the low-population path. Both paths run the same reads, the same
    /// checks and the same writes — the ledger effects of a provisional commit
    /// are real, which is the entire content of "durable, visible and
    /// attributable" — and they differ only in what is recorded about them.
    fn commit(
        &self,
        intent: &Intent,
        provisional: Option<AccountId>,
        now_ms: u64,
    ) -> Result<IntentOutcome, IntentError> {
        let mut ledger = self.ledger.lock().expect("mutex");

        // Step 0 (§7): the idempotency row. A replay returns the recorded
        // outcome unchanged — *including* a provisional one, which is the
        // property the dupe gauntlet's replay arm asserts: a replayed
        // provisional intent returns `Provisional` and not a second commit.
        if let Some(prev) = ledger.outcomes.get(&intent.intent_id) {
            // An annulled intent is the one replay that does not return its
            // recorded outcome, because its recorded outcome is no longer
            // true: the effects were reversed. The row is still here — D29
            // clause 9(c)'s GC interlock is what keeps it here — so the replay
            // applies nothing and is told what happened.
            if prev.finality == crate::keyspace::IntentFinality::Annulled {
                return Ok(IntentOutcome::Rejected {
                    reason: orrery_protocol::REASON_INTENT_ANNULLED,
                });
            }
            return Ok(prev.outcome.clone());
        }

        // D29 clause 4, before anything is read or staged: a provisionally
        // committed row is an input to nothing. Checked for *every* intent,
        // not only provisional ones — the rule is about the row's state, not
        // about who is naming it, and the intent this refuses is usually an
        // ordinary attested one trying to spend quarantined value.
        for (account, asset) in provisional::named_balances(intent) {
            if ledger
                .provisional
                .get(&account)
                .and_then(|row| row.holds_balance(account, asset))
                .is_some()
            {
                return Ok(IntentOutcome::Rejected {
                    reason: orrery_protocol::REASON_PROVISIONAL_INPUT,
                });
            }
        }

        // D29 clause 9(b): the per-account outstanding cap. A refusal, never a
        // queue — the cluster stops admitting long before it starts annulling,
        // because refusal costs the player nothing it had and expiry destroys
        // value the cluster already promised.
        if let Some(account) = provisional {
            let outstanding = ledger
                .provisional
                .get(&account)
                .map_or(0, |row| row.holds.len());
            if outstanding >= orrery_protocol::PROVISIONAL_OUTSTANDING_CAP {
                return Ok(IntentOutcome::Rejected {
                    reason: orrery_protocol::REASON_PROVISIONAL_CAP,
                });
            }
        }

        // Steps 1-2 (§7): read, then check, then stage — no row is touched
        // until every op has passed. A rejection therefore leaves the ledger
        // exactly as it found it, which is what the FDB executor gets by
        // staging its writes in the same order.
        let mut plan = IntentPlan::default();
        for op in &intent.ops {
            let verdict = match op.op {
                LEDGER_CREDIT_OP => {
                    if op.args.len() != LEDGER_CREDIT_ARGS_BYTES {
                        OpsVerdict::Rejected(orrery_protocol::REASON_MALFORMED_OP)
                    } else {
                        let field = |i: usize| {
                            u64::from_le_bytes(op.args[i..i + 8].try_into().expect("slice len"))
                        };
                        let account = AccountId::new(field(0));
                        let asset = AssetId::new(field(8));
                        let delta =
                            i64::from_le_bytes(op.args[16..24].try_into().expect("slice len"));
                        if !plan.has_balance(account, asset) {
                            let value =
                                ledger.balances.get(&(account, asset)).copied().unwrap_or(0);
                            plan.load_balance(account, asset, value);
                        }
                        plan.credit(account, asset, delta);
                        OpsVerdict::Applied
                    }
                }
                LEDGER_ITEM_TRANSFER_OP => match ItemTransferArgs::decode(&op.args) {
                    Err(_) => OpsVerdict::Rejected(orrery_protocol::REASON_MALFORMED_OP),
                    Ok(transfer) => {
                        if !plan.has_item(transfer.item) {
                            let row = ledger.items.get(&transfer.item).cloned();
                            plan.load_item(transfer.item, row);
                        }
                        if !plan.has_balance(transfer.buyer, transfer.asset) {
                            let value = ledger
                                .balances
                                .get(&(transfer.buyer, transfer.asset))
                                .copied()
                                .unwrap_or(0);
                            plan.load_balance(transfer.buyer, transfer.asset, value);
                        }
                        plan.transfer(&transfer)
                    }
                },
                // `Ruleset`-opaque (docs/08-persistence.md §2.2): carried, not
                // interpreted, and therefore never refused here.
                _ => OpsVerdict::Applied,
            };
            if let OpsVerdict::Rejected(reason) = verdict {
                return Ok(IntentOutcome::Rejected { reason });
            }
        }

        // Step 3 (§7): the guarded mutation bundle. A non-empty write set
        // cannot reach this stage without its receipt because the type carries
        // both and exposes no writes-only constructor.
        let mutation = plan.into_mutation(intent);
        if let Some(mutation) = &mutation {
            for write in mutation.writes() {
                match write {
                    PlannedWrite::BalanceAdd {
                        account,
                        asset,
                        delta,
                    } => {
                        *ledger.balances.entry((*account, *asset)).or_insert(0) +=
                            i128::from(*delta);
                    }
                    PlannedWrite::ItemOwner { item, row, .. } => {
                        ledger.items.insert(*item, row.clone());
                    }
                }
            }
            ledger.receipts.push(mutation.receipt().clone());
        }

        // Mint one PersistId per op (the harness default; a linked Ruleset
        // decides what an op actually mints).
        let start = ledger.next_pid;
        ledger.next_pid += intent.ops.len() as u64;
        let minted = (start..start + intent.ops.len() as u64)
            .map(orrery_protocol::PersistId::new)
            .collect::<Vec<_>>();
        ledger.next_tick += 1;
        let tick = orrery_protocol::Tick::new(ledger.next_tick);

        let (outcome, finality, finalize_by_ms) = match provisional {
            None => (
                IntentOutcome::Committed { tick, minted },
                crate::keyspace::IntentFinality::Final,
                0,
            ),
            Some(account) => {
                let finalize_by = provisional::finalize_by(now_ms);
                let writes = provisional::provisional_writes(
                    mutation
                        .as_ref()
                        .map_or(&[] as &[PlannedWrite], LedgerMutation::writes),
                );
                // The commitment is present: `provisional::classify` refuses
                // an intent without one before the gateway ever reaches this
                // executor, and an executor reached without that check is a
                // caller bug rather than a case to degrade for.
                let commitment = intent
                    .evidence
                    .expect("classify admits no provisional intent without a commitment");
                ledger.provisional.entry(account).or_default().holds.push(
                    crate::keyspace::ProvisionalHold {
                        intent_id: intent.intent_id,
                        account,
                        writes,
                        committed_ms: now_ms,
                        finalize_by_ms: finalize_by,
                        commitment,
                        subject: intent.issuer,
                    },
                );
                (
                    IntentOutcome::Provisional {
                        tick,
                        minted,
                        finalize_by,
                    },
                    crate::keyspace::IntentFinality::Provisional,
                    finalize_by,
                )
            }
        };
        ledger.outcomes.insert(
            intent.intent_id,
            crate::keyspace::IntentRow {
                outcome: outcome.clone(),
                gc_deadline_ms: now_ms + MEM_INTENT_ROW_RETENTION_MS,
                finality,
                finalize_by_ms,
            },
        );
        Ok(outcome)
    }

    /// The durable row this tier recorded for `intent_id`, for tests that
    /// assert on finality rather than on the reply.
    #[must_use]
    pub fn intent_row(&self, intent_id: u128) -> Option<crate::keyspace::IntentRow> {
        self.ledger
            .lock()
            .expect("mutex")
            .outcomes
            .get(&intent_id)
            .cloned()
    }

    /// This account's unfinalized provisional intents, oldest first.
    #[must_use]
    pub fn provisional_holds(&self, account: AccountId) -> Vec<crate::keyspace::ProvisionalHold> {
        self.ledger
            .lock()
            .expect("mutex")
            .provisional
            .get(&account)
            .map(|row| row.holds.clone())
            .unwrap_or_default()
    }
}

/// This tier's stand-in for `INTENT_ROW_RETENTION_MS` (1 h).
///
/// Defined here rather than imported because the FDB constant lives behind the
/// `fdb` feature and this tier has to compile without it. The two must agree —
/// the GC interlock is asserted against this one in the non-`fdb` test tier —
/// which is why the number is written once as an hour in milliseconds in both
/// places and never derived from the other.
const MEM_INTENT_ROW_RETENTION_MS: u64 = 60 * 60 * 1000;

#[async_trait::async_trait]
impl IntentExecutor for MemIntentExecutor {
    async fn execute(&self, intent: &Intent) -> Result<IntentOutcome, IntentError> {
        self.commit(intent, None, mem_now_ms())
    }

    async fn execute_provisional(
        &self,
        intent: &Intent,
        account: AccountId,
    ) -> Result<IntentOutcome, IntentError> {
        self.commit(intent, Some(account), mem_now_ms())
    }
}

/// Current unix time in milliseconds, for this tier's deadlines.
fn mem_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// D29 clauses 7 and 8 against the in-memory ledger.
///
/// The point of implementing this here as well as against FoundationDB is that
/// [`ProvisionalFinalizer`]'s sweep — the deadline rule, the verdict table, the
/// order of operations — then has exactly one implementation, exercised by the
/// tier that needs no cluster. What only the `fdb` tier can prove is that the
/// annulment's writes and its finality flip land in **one** serializable
/// transaction; this tier's mutex gives that for free and therefore proves
/// nothing about it.
#[async_trait::async_trait]
impl ProvisionalStore for MemIntentExecutor {
    async fn outstanding(&self) -> Result<Vec<crate::keyspace::ProvisionalHold>, IntentError> {
        let ledger = self.ledger.lock().expect("mutex");
        let mut holds: Vec<crate::keyspace::ProvisionalHold> = ledger
            .provisional
            .values()
            .flat_map(|row| row.holds.iter().cloned())
            .collect();
        // Oldest first (D29 clause 7). The tiebreak on `intent_id` is not
        // cosmetic: two intents committed in the same millisecond must sweep
        // in a defined order, or a test asserting on the sweep's report is
        // asserting on a `HashMap` iteration.
        holds.sort_by_key(|hold| (hold.committed_ms, hold.intent_id));
        Ok(holds)
    }

    async fn finalize(&self, hold: &crate::keyspace::ProvisionalHold) -> Result<(), IntentError> {
        let mut ledger = self.ledger.lock().expect("mutex");
        if let Some(row) = ledger.outcomes.get_mut(&hold.intent_id) {
            // The outcome stays `Provisional` on the wire-typed field and the
            // *finality* moves to `Final`, which is the split D29 clause 5
            // draws: the outcome records what the intent did, the finality
            // records whether the cluster has stood behind it. Rewriting the
            // outcome to `Committed` would make a replay of a finalized intent
            // indistinguishable from one that was never provisional, and the
            // client that is holding a `Provisional` status needs the tick and
            // the minted ids it was already told about to keep matching.
            row.finality = crate::keyspace::IntentFinality::Final;
            row.finalize_by_ms = 0;
        }
        release_hold(&mut ledger, hold.intent_id);
        Ok(())
    }

    async fn annul(&self, hold: &crate::keyspace::ProvisionalHold) -> Result<(), IntentError> {
        let mut ledger = self.ledger.lock().expect("mutex");
        let writes = hold
            .writes
            .iter()
            .map(|write| PlannedWrite::BalanceAdd {
                account: write.account,
                asset: write.asset,
                delta: -write.delta,
            })
            .collect();
        if let Some(mutation) = LedgerMutation::from_writes(
            hold.intent_id,
            hold.writes.iter().map(|write| write.account).collect(),
            Vec::new(),
            writes,
        ) {
            // The forward-written inverse and its compensating receipt travel
            // through the same coupled bundle as an ordinary intent commit.
            for write in mutation.writes() {
                let PlannedWrite::BalanceAdd {
                    account,
                    asset,
                    delta,
                } = write
                else {
                    unreachable!("provisional item transfers are ineligible")
                };
                *ledger.balances.entry((*account, *asset)).or_insert(0) += i128::from(*delta);
            }
            ledger.receipts.push(mutation.receipt().clone());
        }
        if let Some(row) = ledger.outcomes.get_mut(&hold.intent_id) {
            row.finality = crate::keyspace::IntentFinality::Annulled;
            row.finalize_by_ms = 0;
            // Restamped from the *annulment*, not from the commit, so an
            // annulled row outlives a client offline queue whose TTL must
            // already be shorter than the retention (D29 clause 9(c)).
            row.gc_deadline_ms = mem_now_ms() + MEM_INTENT_ROW_RETENTION_MS;
        }
        release_hold(&mut ledger, hold.intent_id);
        Ok(())
    }
}

/// Drop `intent_id` from whichever account's hold list carries it, and drop
/// the account's row once it is empty.
///
/// Emptying the row rather than leaving a zero-length one matters for the
/// sweep: the family is scanned, and a row per account that ever committed
/// provisionally would make the scan proportional to history instead of to
/// outstanding work.
fn release_hold(ledger: &mut MemLedger, intent_id: u128) {
    ledger.provisional.retain(|_, row| {
        row.holds.retain(|hold| hold.intent_id != intent_id);
        !row.holds.is_empty()
    });
}

/// Map an [`IntentError`] onto the outcome the gateway acks (the bounded-
/// retry refusal of §7 becomes a definitive `Rejected`).
#[must_use]
pub fn error_outcome(err: &IntentError) -> IntentOutcome {
    match err {
        IntentError::ContentionExhausted => IntentOutcome::Rejected {
            reason: REASON_CONTENTION_EXHAUSTED,
        },
        IntentError::Store(_) => IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_EXECUTOR_ERROR,
        },
    }
}

/// Intent fixtures shared by this module's tests and
/// [`provisional`](self::provisional)'s.
///
/// A separate module rather than helpers inside one `#[cfg(test)] mod tests`,
/// because two test modules in this crate build the same three shapes — a
/// self-credit, a transfer, and an intent carrying a commitment — and a second
/// copy of the 40-byte transfer layout is the kind of duplication that drifts
/// silently and then makes one of the two suites assert about a different
/// object than it names.
#[cfg(test)]
pub(crate) mod tests_support {
    use orrery_protocol::{
        AccountId, AssetId, CellEpoch, ChainHash, EvidenceCommitment, Intent, IntentOp, PersistId,
        RulesetId, Tick,
    };

    use super::{ItemTransferArgs, LEDGER_ITEM_TRANSFER_OP};

    /// The key every fixture intent is issued by, so a test can co-sign
    /// against it and so `intent_id` alone distinguishes two intents.
    pub(crate) fn issuer_key() -> iroh_base::SecretKey {
        let mut seed = [0u8; 32];
        seed[0] = 1;
        iroh_base::SecretKey::from_bytes(&seed)
    }

    /// A [`super::LEDGER_CREDIT_OP`]'s 24-byte `account ‖ asset ‖ delta`
    /// triple.
    pub(crate) fn credit_args(account: u64, asset: u64, delta: i64) -> bytes::Bytes {
        let mut args = Vec::with_capacity(24);
        args.extend_from_slice(&account.to_le_bytes());
        args.extend_from_slice(&asset.to_le_bytes());
        args.extend_from_slice(&delta.to_le_bytes());
        bytes::Bytes::from(args)
    }

    /// A [`LEDGER_ITEM_TRANSFER_OP`]'s 40-byte layout, built through the
    /// canonical encoder rather than by hand.
    pub(crate) fn transfer_args(
        item: u64,
        seller: u64,
        buyer: u64,
        asset: u64,
        price: i64,
    ) -> bytes::Bytes {
        bytes::Bytes::copy_from_slice(
            &ItemTransferArgs {
                item: orrery_protocol::ItemUid::new(item),
                seller: AccountId::new(seller),
                buyer: AccountId::new(buyer),
                asset: AssetId::new(asset),
                price,
            }
            .encode(),
        )
    }

    /// A commitment whose fields are distinct constants, so a mismatch shows
    /// up as a specific field rather than as a wall of zeroes.
    pub(crate) fn commitment() -> EvidenceCommitment {
        EvidenceCommitment {
            ruleset: RulesetId {
                version: 1,
                digest: [7; 32],
            },
            entity: PersistId::new(11),
            window_start: Tick::new(100),
            window_end: Tick::new(160),
            t0_claim_hash: [9; 32],
            log_head: ChainHash([5; 32]),
        }
    }

    /// A signed intent carrying `ops`, with or without D29's commitment.
    pub(crate) fn provisional_intent(id: u128, ops: Vec<IntentOp>, with_evidence: bool) -> Intent {
        let key = issuer_key();
        let mut intent = Intent {
            intent_id: id,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(0),
            ops,
            attestations: Vec::new(),
            evidence: with_evidence.then(commitment),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    /// The unused-import silencer for a fixture the parent module's tests do
    /// not reach for. Referencing it here keeps the `use` honest without a
    /// crate-wide allow.
    #[allow(dead_code)]
    pub(crate) const TRANSFER_OP: u16 = LEDGER_ITEM_TRANSFER_OP;
}

/// A shared validator for [`crate::gateway::GatewayConfig`].
pub type SharedValidator = Arc<dyn IntentValidator>;
/// A shared executor for [`crate::gateway::GatewayConfig`].
pub type SharedExecutor = Arc<dyn IntentExecutor>;

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{Attestation, CellEpoch, IntentOp};

    /// The first op id this cluster does not interpret, with room to spare.
    /// Tests that need "some opaque op" count up from here.
    const OPAQUE_OP_BASE: u16 = 100;

    fn intent(id: u128, ops: usize) -> Intent {
        let key = iroh_base::SecretKey::from_bytes(&{
            let mut seed = [0u8; 32];
            seed[0] = 1;
            seed
        });
        let mut intent = Intent {
            evidence: None,
            intent_id: id,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(0),
            ops: (0..ops)
                .map(|i| IntentOp {
                    // Offset past the two ids the cluster interprets: these
                    // ops are stand-ins for `Ruleset`-opaque traffic, and
                    // colliding with `LEDGER_CREDIT_OP` or
                    // `LEDGER_ITEM_TRANSFER_OP` would make them malformed
                    // instances of a real op instead.
                    op: (i as u16) + OPAQUE_OP_BASE,
                    args: bytes::Bytes::new(),
                })
                .collect(),
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    #[tokio::test]
    async fn mem_executor_mints_and_is_idempotent() {
        let exec = MemIntentExecutor::new();
        let first = exec.execute(&intent(7, 2)).await.unwrap();
        let IntentOutcome::Committed { tick, minted } = &first else {
            panic!("expected Committed");
        };
        assert_eq!(tick.0, 1);
        assert_eq!(minted.len(), 2, "one minted id per op");
        // A replay returns the first outcome unchanged (same tick, same ids).
        let second = exec.execute(&intent(7, 2)).await.unwrap();
        assert_eq!(second, first, "replay returns the recorded outcome");
        // A different intent mints fresh ids that do not overlap the first's.
        let third = exec.execute(&intent(8, 1)).await.unwrap();
        let IntentOutcome::Committed { minted: m3, .. } = third else {
            panic!("expected Committed");
        };
        let IntentOutcome::Committed { minted: m1, .. } = first else {
            unreachable!()
        };
        assert!(!m1.contains(&m3[0]), "minted ids are unique across intents");
    }

    #[test]
    fn permissive_validator_admits() {
        let v = PermissiveValidator;
        assert_eq!(
            v.validate(&intent(1, 0), &cx(None)),
            IntentVerdict::Admit(IntentPrecheck { read_keys: vec![] })
        );
    }

    // ── BaselineIntentValidator ─────────────────────────────────────────

    /// The key `intent()` above signs with, so tests can co-sign and
    /// impersonate deliberately.
    fn issuer_key() -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&{
            let mut seed = [0u8; 32];
            seed[0] = 1;
            seed
        })
    }

    /// The account every `cx(Some(..))` fixture in this module authenticates
    /// as. Named because D10 item 4's account half turns it from an opaque
    /// number into the party set.
    const ISSUER_ACCOUNT: u64 = 7;

    fn cx(account: Option<u64>) -> IntentContext {
        cx_with_standing(account, SessionStanding::Good)
    }

    fn cx_with_standing(account: Option<u64>, standing: SessionStanding) -> IntentContext {
        IntentContext {
            issuer: issuer_key().public(),
            account: account.map(AccountId::new),
            standing,
        }
    }

    #[test]
    fn unauthenticated_intent_context_is_quarantined() {
        assert_eq!(
            IntentContext::unauthenticated(issuer_key().public()).standing,
            SessionStanding::Quarantined
        );
    }

    /// An intent carrying exactly `ops`, signed by the same issuer.
    fn intent_with(ops: Vec<IntentOp>) -> Intent {
        let key = issuer_key();
        let mut intent = Intent {
            evidence: None,
            intent_id: 9,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(0),
            ops,
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    // -- D27 K-of-N enforcement -------------------------------------------
    //
    // Every test below runs `check_at` with an explicit clock so the epoch's
    // usability window is a parameter rather than a race.

    use crate::witness_epoch::test_support as epoch_fixture;

    /// The handle every fixture epoch is announced under.
    const EPOCH_HANDLE: u64 = 0x0001_0000_0000_0001;

    /// An intent naming [`EPOCH_HANDLE`], signed, with one opaque op.
    fn attestable_intent(id: u128) -> Intent {
        let key = issuer_key();
        let mut intent = Intent {
            evidence: None,
            intent_id: id,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(EPOCH_HANDLE),
            ops: vec![IntentOp {
                op: OPAQUE_OP_BASE,
                args: bytes::Bytes::new(),
            }],
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    /// The witness secret keys the fixture announcement selects, in announced
    /// order — the same identities `epoch_fixture::witnesses` produces, but
    /// with the private halves the tests need in order to co-sign.
    fn witness_keys(count: u8) -> Vec<iroh_base::SecretKey> {
        (0..count).map(epoch_fixture::witness_secret).collect()
    }

    /// The first account id handed to an announced witness.
    ///
    /// Well clear of the single-digit ids the `cx` fixtures give the issuer,
    /// so "N honest strangers" is what a fixture produces by default and a
    /// party has to be arranged on purpose. A fixture whose witnesses shared
    /// the issuer's account by accident would make every enforcement test pass
    /// for the wrong reason once D10 item 4's account half runs.
    const WITNESS_ACCOUNT_BASE: u64 = 1_000;

    /// One distinct, non-party account per announced NodeId — D31 clause (e)'s
    /// `owner(n)`, answered from a table instead of from the `id/` rows
    /// `orrery_identity` will write.
    fn distinct_bindings(announced: &[NodeId]) -> crate::gateway::SharedBindingAuthority {
        bindings_from(
            announced
                .iter()
                .enumerate()
                .map(|(index, node)| (*node, AccountId::new(WITNESS_ACCOUNT_BASE + index as u64))),
        )
    }

    /// An `owner(n)` resolver over an explicit table, for the tests that need
    /// two NodeIds to share an account.
    fn bindings_from(
        pairs: impl IntoIterator<Item = (NodeId, AccountId)>,
    ) -> crate::gateway::SharedBindingAuthority {
        Arc::new(crate::gateway::SnapshotBindingAuthority::from_bindings(
            pairs,
        ))
    }

    /// An enforcing validator over an already-built cache, binding one
    /// distinct account to every NodeId that epoch announced.
    fn enforcing_with(
        epochs: &Arc<crate::witness_epoch::WitnessEpochAuthority>,
    ) -> BaselineIntentValidator {
        let announced = epochs
            .resolve(EPOCH_HANDLE)
            .expect("fixture epoch is announced under EPOCH_HANDLE")
            .snapshot
            .selected
            .clone();
        BaselineIntentValidator::enforcing(
            Arc::clone(epochs),
            Arc::new(epoch_fixture::CoverAllInterest),
            distinct_bindings(&announced),
        )
    }

    /// A validator enforcing over one epoch announcing `witnesses`, accepted
    /// at t=1000.
    fn enforcing_over(
        witnesses: &[iroh_base::SecretKey],
    ) -> (
        BaselineIntentValidator,
        Arc<crate::witness_epoch::WitnessEpochAuthority>,
    ) {
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        let epochs = epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000);
        // D30's standing predicate is not what these tests are about, and a
        // fixture that denied it would make every one of them fail for the
        // wrong reason. The predicate has its own tests below, against a
        // grant that covers one cell of two.
        (
            BaselineIntentValidator::enforcing(
                Arc::clone(&epochs),
                Arc::new(epoch_fixture::CoverAllInterest),
                distinct_bindings(&announced),
            ),
            epochs,
        )
    }

    /// Attach co-signatures from every witness the draw requires.
    fn attest_required(
        intent: &mut Intent,
        witnesses: &[iroh_base::SecretKey],
        epochs: &crate::witness_epoch::WitnessEpochAuthority,
    ) -> Vec<NodeId> {
        let epoch = epochs
            .resolve(EPOCH_HANDLE)
            .expect("fixture epoch is cached");
        let eligible = orrery_protocol::eligible_witnesses(&epoch.snapshot.selected, intent.issuer);
        let required = epoch.required_witnesses(intent.intent_id, &eligible);
        for node in &required {
            let key = witnesses
                .iter()
                .find(|key| key.public() == *node)
                .expect("the draw only names announced witnesses");
            let attestation = intent.attest(key);
            intent.attestations.push(attestation);
        }
        required
    }

    /// The whole predicate, in one test: K required co-signatures admit, and
    /// dropping any single one of them refuses.
    ///
    /// The "any single one" half is what makes this more than a count. A test
    /// that dropped only the last attestation would pass against an
    /// implementation that checked `len() >= K`, which is exactly the
    /// attestation shopping D10 abolishes.
    #[test]
    fn k_required_co_signatures_admit_and_losing_any_one_of_them_refuses() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);

        let mut intent = attestable_intent(1);
        let required = attest_required(&mut intent, &witnesses, &epochs);
        assert_eq!(required.len(), orrery_protocol::WITNESS_QUORUM_K);
        assert!(
            validator.check_at(&intent, &cx(Some(7)), 2_000).is_ok(),
            "K valid attestations from the required subset commit"
        );

        for dropped in 0..required.len() {
            let mut short = attestable_intent(1);
            short.attestations = intent
                .attestations
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != dropped)
                .map(|(_, attestation)| attestation.clone())
                .collect();
            assert_eq!(
                validator.check_at(&short, &cx(Some(7)), 2_000),
                Err(RejectionCause::ThresholdNotMet),
                "one short by required witness {dropped} must be refused"
            );
        }
    }

    /// K valid attestations that are not the drawn subset admit nothing.
    ///
    /// This is the arm that separates "K of N" from "any first K of N". The
    /// substitutes are real, announced, non-party witnesses whose signatures
    /// verify — and the intent is still refused, because a missing *required*
    /// co-signer admits no substitute.
    #[test]
    fn k_valid_attestations_that_are_not_the_required_subset_are_refused() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);
        let epoch = epochs.resolve(EPOCH_HANDLE).expect("cached");

        let mut intent = attestable_intent(2);
        let eligible = orrery_protocol::eligible_witnesses(&epoch.snapshot.selected, intent.issuer);
        let required = epoch.required_witnesses(intent.intent_id, &eligible);

        let substitutes: Vec<&iroh_base::SecretKey> = witnesses
            .iter()
            .filter(|key| !required.contains(&key.public()))
            .take(orrery_protocol::WITNESS_QUORUM_K)
            .collect();
        assert_eq!(
            substitutes.len(),
            orrery_protocol::WITNESS_QUORUM_K,
            "an announced set of 7 leaves 4 non-required members to shop among"
        );
        for key in substitutes {
            let attestation = intent.attest(key);
            intent.attestations.push(attestation);
        }

        assert_eq!(
            intent.attestations.len(),
            orrery_protocol::WITNESS_QUORUM_K,
            "the count is met, so only the draw can refuse this"
        );
        for attestation in &intent.attestations {
            assert!(
                attestation.verify(&intent),
                "every substitute signature is genuine"
            );
        }
        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::RequiredWitnessMissing)
        );
    }

    /// A witness outside the announced set counts for nothing, however good
    /// its signature — the self-chosen-witness case D10 item 4 exists to stop.
    #[test]
    fn an_attestation_from_outside_the_announced_set_does_not_count() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);

        let mut intent = attestable_intent(3);
        attest_required(&mut intent, &witnesses, &epochs);
        assert!(validator.check_at(&intent, &cx(Some(7)), 2_000).is_ok());

        // Same intent, one extra co-signature from a real key that the
        // coordinator never announced.
        let outsider = epoch_fixture::secret(77);
        assert!(
            !epochs
                .resolve(EPOCH_HANDLE)
                .expect("cached")
                .snapshot
                .admits(&outsider.public()),
            "the outsider must genuinely be outside, or this proves nothing"
        );
        let attestation = intent.attest(&outsider);
        assert!(attestation.verify(&intent), "and its signature is real");
        intent.attestations.push(attestation);

        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::WitnessOutsideAnnouncedSet),
            "a submitter that nominates its own co-signers is certifying its \
             own trade"
        );
    }

    /// A co-signature over the legacy issuer preimage is not an attestation,
    /// even under enforcement — D27 clause (c)'s stated degradation.
    #[test]
    fn legacy_preimage_signatures_never_reach_the_quorum() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);
        let epoch = epochs.resolve(EPOCH_HANDLE).expect("cached");

        let mut intent = attestable_intent(4);
        let eligible = orrery_protocol::eligible_witnesses(&epoch.snapshot.selected, intent.issuer);
        let required = epoch.required_witnesses(intent.intent_id, &eligible);
        let preimage = intent.signing_preimage();
        for node in &required {
            let key = witnesses
                .iter()
                .find(|key| key.public() == *node)
                .expect("announced");
            intent.attestations.push(Attestation {
                witness: *node,
                signature: key.sign(&preimage),
            });
        }

        // Exactly the required subset, exactly K of them, every signature made
        // by the right witness — and all of it over the wrong bytes.
        assert_eq!(intent.attestations.len(), orrery_protocol::WITNESS_QUORUM_K);
        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::BadAttestation),
            "the switch to D27's preimage is what this asserts: a peer running \
             the pre-D27 semantics contributes zero valid attestations"
        );
    }

    /// An intent naming an epoch this gateway cannot resolve is refused, and
    /// a stale one is refused with a *different* cause.
    #[test]
    fn an_unresolvable_epoch_and_a_stale_one_are_named_separately() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);

        let mut unknown = attestable_intent(5);
        unknown.cell_epoch = CellEpoch::new(0x0002_0000_0000_0009);
        unknown.sign(&issuer_key());
        assert_eq!(
            validator.check_at(&unknown, &cx(Some(7)), 2_000),
            Err(RejectionCause::UnknownEpoch),
            "no announcement means no eligible vector and no required subset"
        );

        let mut attested = attestable_intent(6);
        attest_required(&mut attested, &witnesses, &epochs);
        assert!(validator.check_at(&attested, &cx(Some(7)), 60_999).is_ok());
        assert_eq!(
            validator.check_at(&attested, &cx(Some(7)), 61_000),
            Err(RejectionCause::EpochStale),
            "past the grace an operator must be able to tell a reconnect from \
             an attack, so this is not a signature failure"
        );

        // The two share one wire code — a client needs "your attestations were
        // wrong", not a taxonomy — and are distinct in the logs, which is
        // where the operator reads them and an attacker does not.
        assert_eq!(
            RejectionCause::UnknownEpoch.wire_reason(),
            orrery_protocol::REASON_ATTESTATION_QUORUM
        );
        assert_eq!(
            RejectionCause::EpochStale.wire_reason(),
            orrery_protocol::REASON_ATTESTATION_QUORUM
        );
        assert_ne!(
            RejectionCause::UnknownEpoch.as_str(),
            RejectionCause::EpochStale.as_str()
        );
    }

    /// An intent is judged against the announced set of the epoch it *names*,
    /// not against the newest one on file.
    #[test]
    fn epoch_turnover_judges_an_intent_against_the_epoch_it_names() {
        let first_set = witness_keys(7);
        let (_, epochs) = enforcing_over(&first_set);

        // A second epoch for the same cell, over a disjoint set of witnesses,
        // chained onto the first's commitment.
        let second_set: Vec<iroh_base::SecretKey> =
            (20..27).map(epoch_fixture::witness_secret).collect();
        let second_handle = 0x0001_0000_0000_0002;
        let announced: Vec<NodeId> = second_set
            .iter()
            .map(iroh_base::SecretKey::public)
            .collect();
        let encoded = epoch_fixture::announcement(
            orrery_protocol::GridId::ROOT,
            orrery_protocol::CellId::ROOT,
            2,
            second_handle,
            &announced,
            &[8u8; 32],
            Some([7u8; 32]),
        );
        epochs
            .apply_announcement(
                &encoded,
                epoch_fixture::secret(1).public(),
                &epoch_fixture::CoverAllInterest,
                1_500,
            )
            .expect("the chained announcement is accepted");

        // The validator resolves **both** announced sets. This test is about
        // which epoch judges an intent, so a candidate that dropped out of
        // `E(I)` for an unresolved binding (D31 clause (f)) would answer the
        // wrong question — the misdirected intent below would demote to the
        // low-population path instead of naming its witnesses as strangers.
        let mut every_announced: Vec<NodeId> =
            first_set.iter().map(iroh_base::SecretKey::public).collect();
        every_announced.extend_from_slice(&announced);
        let validator = BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            Arc::new(epoch_fixture::CoverAllInterest),
            distinct_bindings(&every_announced),
        );

        // An intent attested under epoch 1, submitted after epoch 2 arrived.
        // Nothing about epoch 2's arrival invalidates it: this is what lets a
        // co-signature collected a moment before a boundary commit a moment
        // after it, at Donnybrook-rate churn.
        let mut in_flight = attestable_intent(7);
        attest_required(&mut in_flight, &first_set, &epochs);
        assert!(
            validator.check_at(&in_flight, &cx(Some(7)), 2_000).is_ok(),
            "an in-flight attestation survives the boundary"
        );

        // And the same co-signatures, re-pointed at epoch 2, are worthless:
        // its announced set shares no member with epoch 1's.
        let mut misdirected = attestable_intent(8);
        misdirected.cell_epoch = CellEpoch::new(second_handle);
        misdirected.sign(&issuer_key());
        for key in first_set.iter().take(orrery_protocol::WITNESS_QUORUM_K) {
            let attestation = misdirected.attest(key);
            misdirected.attestations.push(attestation);
        }
        assert_eq!(
            validator.check_at(&misdirected, &cx(Some(7)), 2_000),
            Err(RejectionCause::WitnessOutsideAnnouncedSet)
        );
    }

    // -- D32 clause (b): the shadow arm -----------------------------------
    //
    // Every test below is written against a **pair**: one validator that acts
    // and one that watches, differing in nothing but the posture byte and the
    // observer. That is not a convenience — it is what makes "shadow evaluates
    // the same predicate live mode does" structural rather than asserted. A
    // shadow fixture built from its own authorities could drift from the
    // enforcing one and every test here would keep passing.

    /// The same validator, watching instead of acting, with its observations
    /// captured.
    fn watching(
        validator: &BaselineIntentValidator,
    ) -> (BaselineIntentValidator, Arc<ShadowObservationLog>) {
        let log = Arc::new(ShadowObservationLog::default());
        let shadowed = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Shadow),
            observer: Some(Arc::clone(&log) as SharedShadowObserver),
            ..validator.clone()
        };
        assert_eq!(shadowed.enforcement(), AttestationEnforcement::Shadow);
        (shadowed, log)
    }

    /// The one observation a test's single intent produced.
    fn only(log: &ShadowObservationLog) -> ShadowObservation {
        let observations = log.observations();
        assert_eq!(
            observations.len(),
            1,
            "shadow records exactly one observation per evaluated intent"
        );
        observations[0]
    }

    /// Obligations (2) and (3) on one intent, in both directions.
    ///
    /// The intent is one the quorum genuinely refuses — two of the three
    /// co-signatures the draw named — so:
    ///
    /// - `Required` refuses it. That is the control: without it, "shadow
    ///   admitted the intent" says nothing, because an intent nothing would
    ///   have refused is admitted by every mode.
    /// - `Shadow` **commits** it. That is obligation (3), the half a test
    ///   which only checked the log would miss entirely.
    /// - `Shadow` records the **exact** cause `Required` returned, compared
    ///   against the refusal itself rather than against a literal, so the two
    ///   cannot drift apart.
    #[test]
    fn shadow_admits_what_required_refuses_and_records_that_refusal() {
        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let (shadow, log) = watching(&enforcing);

        let mut short = attestable_intent(60);
        let required = attest_required(&mut short, &witnesses, &epochs);
        assert_eq!(required.len(), orrery_protocol::WITNESS_QUORUM_K);
        short.attestations.pop();

        let refused = enforcing
            .check_at(&short, &cx(Some(7)), 2_000)
            .expect_err("K-1 of the drawn subset is a refusal in live mode");
        assert_eq!(refused, RejectionCause::ThresholdNotMet);

        assert_eq!(
            shadow.check_at(&short, &cx(Some(7)), 2_000),
            Ok(Admission::Attested(IntentPrecheck::default())),
            "obligation (3): the refusal is the action, and shadow does not take it"
        );

        let observed = only(&log);
        assert_eq!(
            observed.verdict,
            ShadowVerdict::WouldRefuse(refused),
            "obligation (2): the recorded cause is the one live mode returned"
        );
        assert!(observed.verdict.would_act());
        assert_eq!(log.would_act(), 1);
        assert_eq!(observed.intent_id, short.intent_id);
        assert_eq!(observed.subject, Some(AccountId::new(7)));
        assert_eq!(observed.cell_epoch, short.cell_epoch);
        assert_eq!(observed.observed_at_ms, 2_000);
        assert_eq!(
            observed.verdict.as_str(),
            RejectionCause::ThresholdNotMet.as_str(),
            "the label is the rejection log's own, so a shadow report joins \
             against it with no translation table"
        );
    }

    /// The draw conjunct is observed too, not just the count.
    ///
    /// K co-signatures that are the *wrong* K is the attestation-shopping case
    /// D10 abolishes, and it is the one an implementation checking `len() >= K`
    /// would let through. Shadow has to see it, or its refusal count
    /// understates exactly the population the control exists for.
    #[test]
    fn shadow_records_a_missing_required_witness_rather_than_a_short_count() {
        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let (shadow, log) = watching(&enforcing);

        let mut shopped = attestable_intent(61);
        let required = attest_required(&mut shopped, &witnesses, &epochs);
        // Swap one drawn witness for an eligible one the draw did not name:
        // the count still reaches K, and the subset does not.
        shopped.attestations.pop();
        let substitute = witnesses
            .iter()
            .find(|key| !required.contains(&key.public()))
            .expect("N > K, so an undrawn announced witness exists");
        let attestation = shopped.attest(substitute);
        assert!(attestation.verify(&shopped), "a genuine co-signature");
        shopped.attestations.push(attestation);
        assert_eq!(
            shopped.attestations.len(),
            orrery_protocol::WITNESS_QUORUM_K,
            "the count is satisfied, which is the point"
        );

        let refused = enforcing
            .check_at(&shopped, &cx(Some(7)), 2_000)
            .expect_err("the drawn subset is not present");
        assert_eq!(refused, RejectionCause::RequiredWitnessMissing);
        assert!(shadow.check_at(&shopped, &cx(Some(7)), 2_000).is_ok());
        assert_eq!(only(&log).verdict, ShadowVerdict::WouldRefuse(refused));
    }

    /// An intent live mode would have admitted records no would-be refusal.
    ///
    /// Without this the signal is useless in the other direction: a shadow
    /// that flagged everything would read as a control about to refuse the
    /// whole cluster, and clause (e)'s `fp_count` would be the intent count.
    #[test]
    fn shadow_records_no_refusal_for_an_intent_required_would_admit() {
        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let (shadow, log) = watching(&enforcing);

        let mut attested = attestable_intent(62);
        attest_required(&mut attested, &witnesses, &epochs);
        assert!(
            enforcing.check_at(&attested, &cx(Some(7)), 2_000).is_ok(),
            "the control arm: live mode admits this intent"
        );

        assert!(shadow.check_at(&attested, &cx(Some(7)), 2_000).is_ok());
        let observed = only(&log);
        assert_eq!(observed.verdict, ShadowVerdict::WouldAdmit);
        assert!(!observed.verdict.would_act());
        assert_eq!(observed.verdict.cause(), None);
        assert_eq!(log.would_act(), 0);
        assert_eq!(
            log.evaluated(),
            1,
            "an admitted intent is still observed — it is clause (e)'s \
             coverage denominator, and a numerator with no denominator is not \
             evidence"
        );
    }

    /// D30's standing conjunct is evaluated in shadow, not skipped.
    ///
    /// It is the sub-predicate most easily lost, because it sits inside the
    /// quorum check rather than beside it, and losing it makes the shadow
    /// refusal count silently low — which reads as a control that is safe to
    /// promote.
    #[test]
    fn shadow_evaluates_d30s_standing_conjunct() {
        let (enforcing, epochs, _home, away) = two_cell_gateway();
        let (shadow, log) = watching(&enforcing);

        let mut shopped = intent_naming(AWAY_HANDLE, 63);
        attest_required_under(&mut shopped, AWAY_HANDLE, &away, &epochs);
        assert_eq!(
            enforcing.check_at(&shopped, &cx(Some(7)), 2_000),
            Err(RejectionCause::NoStandingInCell),
            "the control: the issuer stands in the home cell only"
        );

        assert!(shadow.check_at(&shopped, &cx(Some(7)), 2_000).is_ok());
        assert_eq!(
            only(&log).verdict,
            ShadowVerdict::WouldRefuse(RejectionCause::NoStandingInCell)
        );
    }

    /// The always-on set does not switch off with the quorum, and shadow is
    /// not an exception to that.
    ///
    /// D32 clause (b)'s second box lists what never ramps: the D27 preimage,
    /// the `MAX_ATTESTATIONS` cap, the duplicate rule, the self-witness
    /// refusal. Each of these is a *refusal* in shadow mode, which is the one
    /// place "shadow refuses nothing" would be exactly the wrong reading — a
    /// flag that could disable a signature check is a denial-of-service lever
    /// pointed at the cluster.
    ///
    /// The log is asserted empty as well, because these refusals happen above
    /// the quorum: an observation for one would mean the predicate had run,
    /// and the intent had reached a stage it must never reach.
    #[test]
    fn shadow_still_refuses_every_check_that_is_not_the_quorum() {
        let witnesses = witness_keys(7);
        let (enforcing, _epochs) = enforcing_over(&witnesses);
        let (shadow, log) = watching(&enforcing);

        // A forged co-signature: the D27 preimage, which no mode waives.
        let mut forged = attestable_intent(64);
        let mut attestation = forged.attest(&witnesses[0]);
        attestation.signature = witnesses[0].sign(b"not the attestation preimage");
        forged.attestations.push(attestation);

        // The same witness twice.
        let mut repeated = attestable_intent(65);
        let once = repeated.attest(&witnesses[1]);
        repeated.attestations.push(once.clone());
        repeated.attestations.push(once);

        // Over the cap.
        let mut flooded = attestable_intent(66);
        for index in 0..=MAX_ATTESTATIONS {
            let key = epoch_fixture::witness_secret(100 + index as u8);
            let attestation = flooded.attest(&key);
            flooded.attestations.push(attestation);
        }

        // The issuer witnessing itself.
        let mut selfish = attestable_intent(67);
        let attestation = selfish.attest(&issuer_key());
        selfish.attestations.push(attestation);

        for (intent, expected) in [
            (&forged, RejectionCause::BadAttestation),
            (&repeated, RejectionCause::DuplicateAttestation),
            (&flooded, RejectionCause::TooManyAttestations),
            (&selfish, RejectionCause::SelfWitness),
        ] {
            assert_eq!(
                shadow.check_at(intent, &cx(Some(7)), 2_000),
                Err(expected),
                "`{}` is correctness, not enforcement: it never ramps",
                expected.as_str()
            );
            assert_eq!(
                enforcing.check_at(intent, &cx(Some(7)), 2_000),
                Err(expected),
                "and the two modes agree on it exactly"
            );
        }
        assert_eq!(
            log.evaluated(),
            0,
            "every one of these is refused above the quorum, so the predicate \
             never ran and there is nothing to observe"
        );
    }

    /// D32 clause (b)'s degraded arm: an internal error records
    /// *unevaluated*, and never acts.
    ///
    /// The direction matters more than the mechanism. Live mode fails closed
    /// on a missing authority — `UnknownEpoch`, `NoStandingInCell` — because a
    /// refusal is the safe answer when it cannot judge. Shadow has no safe
    /// refusal to make, so borrowing those causes would put a would-be refusal
    /// of every honest account on the cluster into clause (e)'s `fp_count`,
    /// and a misconfigured gateway would read as a control that refuses
    /// everything.
    #[test]
    fn shadow_records_unevaluated_when_it_cannot_evaluate() {
        let intent = attestable_intent(68);
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();

        let cacheless_log = Arc::new(ShadowObservationLog::default());
        let cacheless = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Shadow),
            epochs: None,
            interest: None,
            bindings: None,
            observer: Some(Arc::clone(&cacheless_log) as SharedShadowObserver),
        };
        assert!(
            cacheless.check_at(&intent, &cx(Some(7)), 2_000).is_ok(),
            "a misconfiguration is not grounds to act"
        );
        assert_eq!(
            only(&cacheless_log).verdict,
            ShadowVerdict::Unevaluated(ShadowUnevaluated::NoEpochAuthority)
        );
        assert_eq!(cacheless_log.would_act(), 0);

        let standingless_log = Arc::new(ShadowObservationLog::default());
        let standingless = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Shadow),
            epochs: Some(epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000)),
            interest: None,
            bindings: Some(distinct_bindings(&announced)),
            observer: Some(Arc::clone(&standingless_log) as SharedShadowObserver),
        };
        assert!(standingless.check_at(&intent, &cx(Some(7)), 2_000).is_ok());
        assert_eq!(
            only(&standingless_log).verdict,
            ShadowVerdict::Unevaluated(ShadowUnevaluated::NoInterestAuthority)
        );
        assert_eq!(standingless_log.would_act(), 0);
    }

    /// A shadow commit is `Attested`, never `Provisional`, even when live mode
    /// would have quarantined the intent.
    ///
    /// The quarantine, the outstanding-account cap and the annulment deadline
    /// D29's path applies are *actions* — a shadow period that started
    /// applying them would be live under a different name. The verdict still
    /// records which of live mode's three outcomes it would have been, because
    /// collapsing "would have been quarantined" into "would have been
    /// admitted" mis-states the population D29's path serves.
    #[test]
    fn shadow_commits_attested_where_required_would_have_committed_provisionally() {
        let witnesses = witness_keys(7);
        let (enforcing, _epochs) = enforcing_over(&witnesses);
        // A resolver that answers nothing: D31 clause (f) excludes every
        // candidate, `|E(I)|` falls to zero, and live mode reaches D29's
        // low-population branch. This is the deployment this tree actually has
        // — nothing writes the `id/` rows yet.
        let blind = BaselineIntentValidator {
            bindings: Some(Arc::new(crate::gateway::UnboundBindingAuthority)),
            ..enforcing.clone()
        };
        let (shadow, log) = watching(&blind);

        let mut intent = attestable_intent(69);
        intent.evidence = Some(tests_support::commitment());
        intent.sign(&issuer_key());

        assert_eq!(
            blind.check_at(&intent, &cx(Some(7)), 2_000),
            Ok(Admission::Provisional(IntentPrecheck::default())),
            "the control: live mode quarantines this intent"
        );
        assert_eq!(
            shadow.check_at(&intent, &cx(Some(7)), 2_000),
            Ok(Admission::Attested(IntentPrecheck::default())),
            "shadow applies no quarantine — that is an action"
        );
        let observed = only(&log);
        assert_eq!(observed.verdict, ShadowVerdict::WouldCommitProvisionally);
        assert!(
            !observed.verdict.would_act(),
            "a provisional commit is a commit; clause (e) counts refusals"
        );
    }

    // -- D32 clause (e): the measurement -----------------------------------
    //
    // The tests above prove the shadow arm *observes*. These prove the
    // observations become clause (e)'s two numbers, and they run through the
    // real validator rather than against the meter directly for the reason
    // `watching` gives: a fixture that fed the meter by hand would keep passing
    // after the validator stopped feeding it.

    /// The same validator, watching, with its observations metered.
    fn metering(
        validator: &BaselineIntentValidator,
    ) -> (BaselineIntentValidator, Arc<ramp::RampMeter>) {
        let meter = Arc::new(ramp::RampMeter::new(ATTESTATION_QUORUM_CONTROL));
        let metered = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Shadow),
            observer: Some(Arc::clone(&meter) as SharedShadowObserver),
            ..validator.clone()
        };
        (metered, meter)
    }

    /// The would-have-acted counter moves on what `Required` would refuse and
    /// stands still on what it would admit.
    ///
    /// Both arms in one test on purpose: a counter that increments on the
    /// refused intent proves nothing on its own, because a counter that
    /// increments on *everything* does that too, and the resulting `fp_count`
    /// would be the intent count.
    #[test]
    fn the_would_have_acted_counter_moves_only_on_a_would_be_refusal() {
        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let (metered, meter) = metering(&enforcing);
        let subject = AccountId::new(5_000);
        let cohort = {
            let mut cohort = ramp::HonestCohort::new();
            cohort.arm(subject);
            cohort
        };

        let mut admitted = attestable_intent(90);
        attest_required(&mut admitted, &witnesses, &epochs);
        enforcing
            .check_at(&admitted, &cx(Some(subject.0)), 2_000)
            .expect("the control arm: live mode admits this intent");

        let mut refused = attestable_intent(91);
        attest_required(&mut refused, &witnesses, &epochs);
        refused.attestations.pop();
        let cause = enforcing
            .check_at(&refused, &cx(Some(subject.0)), 2_000)
            .expect_err("the control arm: live mode refuses this one");

        assert!(metered
            .check_at(&admitted, &cx(Some(subject.0)), 2_000)
            .is_ok());
        let clean = meter.snapshot(&cohort);
        assert_eq!(clean.cohort.fp_count, 0);
        assert_eq!(clean.cohort.observed, 1);
        assert_eq!(clean.cohort.coverage, Some(1.0));

        assert!(metered
            .check_at(&refused, &cx(Some(subject.0)), 2_000)
            .is_ok());
        let flagged = meter.snapshot(&cohort);
        assert_eq!(flagged.cohort.fp_count, 1);
        assert_eq!(flagged.cohort.observed, 2);
        assert_eq!(
            flagged.cohort.by_cause.get(cause.as_str()),
            Some(&1),
            "dimensioned by the rejection log's own label, so the two join \
             without a translation table"
        );
        assert_eq!(
            flagged.cohort.accounts_would_act, 1,
            "one account, whatever the event count"
        );
    }

    /// The coverage denominator counts activity the shadow arm never saw.
    ///
    /// This is the clause the whole measurement rests on, and it is the one a
    /// meter fed from the observation stream alone cannot have: the four
    /// always-on correctness checks refuse *above* the enforcement switch, so
    /// the predicate never runs, nothing is observed, and a denominator
    /// derived from `record` would report `coverage = 1.000` over a population
    /// the control judged half of. Both numbers here come from counting points
    /// at opposite ends of `check_at`, which is what makes the ratio a
    /// measurement rather than a restatement.
    #[test]
    fn the_coverage_denominator_counts_activity_the_shadow_arm_never_saw() {
        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let (metered, meter) = metering(&enforcing);
        let subject = AccountId::new(5_001);
        let cohort = {
            let mut cohort = ramp::HonestCohort::new();
            cohort.sample(subject);
            cohort
        };

        // Three intents the quorum sees and admits.
        for id in 92..95_u128 {
            let mut attested = attestable_intent(id);
            attest_required(&mut attested, &witnesses, &epochs);
            assert!(metered
                .check_at(&attested, &cx(Some(subject.0)), 2_000)
                .is_ok());
        }

        // One the always-on duplicate rule refuses before the quorum runs.
        let mut repeated = attestable_intent(95);
        let once = repeated.attest(&witnesses[1]);
        repeated.attestations.push(once.clone());
        repeated.attestations.push(once);
        assert_eq!(
            metered.check_at(&repeated, &cx(Some(subject.0)), 2_000),
            Err(RejectionCause::DuplicateAttestation),
            "correctness, not enforcement: it never ramps and never reaches \
             the shadow arm"
        );

        let snapshot = meter.snapshot(&cohort);
        assert_eq!(snapshot.cohort.observed, 3, "the shadow arm saw three");
        assert_eq!(
            snapshot.cohort.qualifying, 4,
            "and the gateway made four admission decisions for this account"
        );
        assert_eq!(snapshot.cohort.coverage, Some(0.75));
        assert_eq!(snapshot.cohort.fp_count, 0);
    }

    /// `Off` evaluates nothing, so its coverage is zero rather than clean.
    ///
    /// D32 clause (b): "a control in `Off` has no observation period and
    /// cannot be promoted from it." A fleet with half its gateways posted off
    /// has half the coverage, and this is the only counter that can say so —
    /// the observation stream from those gateways is empty, which is
    /// indistinguishable from quiet traffic.
    #[test]
    fn a_validator_posted_off_reports_no_coverage_rather_than_a_clean_sheet() {
        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let meter = Arc::new(ramp::RampMeter::new(ATTESTATION_QUORUM_CONTROL));
        let dark = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Off),
            observer: Some(Arc::clone(&meter) as SharedShadowObserver),
            ..enforcing.clone()
        };
        let subject = AccountId::new(5_002);
        let cohort = {
            let mut cohort = ramp::HonestCohort::new();
            cohort.arm(subject);
            cohort
        };

        let mut attested = attestable_intent(96);
        attest_required(&mut attested, &witnesses, &epochs);
        assert!(dark
            .check_at(&attested, &cx(Some(subject.0)), 2_000)
            .is_ok());

        let snapshot = meter.snapshot(&cohort);
        assert_eq!(snapshot.cohort.qualifying, 1);
        assert_eq!(snapshot.cohort.observed, 0);
        assert_eq!(snapshot.cohort.fp_count, 0);
        assert_eq!(
            snapshot.cohort.coverage,
            Some(0.0),
            "zero false positives at zero coverage is the shape of evidence \
             D32 refuses, and it has to be visible as such"
        );
    }

    /// Regenerate `docs/data/ramp-shadow-*.json` from a real shadow run.
    ///
    /// Ignored because it writes into the tree, in the same arrangement
    /// `orrery_conformance`'s golden and `orrery_games`' chains use. The
    /// traffic is a harness and the artifact says so in its own `provenance`
    /// block: the report script refuses to call a non-production run's
    /// production leg met, however good its numbers are.
    ///
    /// ```sh
    /// cargo test -p orrery_persistd --lib -- --ignored --nocapture emit_ramp_artifact
    /// ```
    #[test]
    #[ignore = "writes docs/data/ramp-shadow-*.json; run explicitly to regenerate"]
    fn emit_ramp_artifact() {
        const ARMED: u64 = 40;
        const NATURAL: u64 = 80;
        const OUTSIDE: u64 = 20;
        const INTENTS_PER_ACCOUNT: u64 = 30;
        /// One simulated intent every 10 ms, so the window is a span rather
        /// than an instant and `W` is a number the report can reject. Ten,
        /// not more: the whole run has to fit inside the fixture epoch's own
        /// 60 s usability window, or every intent is refused `EpochStale` and
        /// the artifact measures the fixture instead of the control.
        const TICK_MS: u64 = 10;

        let witnesses = witness_keys(7);
        let (enforcing, epochs) = enforcing_over(&witnesses);
        let (metered, meter) = metering(&enforcing);

        let mut cohort = ramp::HonestCohort::new();
        for index in 0..ARMED {
            cohort.arm(AccountId::new(5_000 + index));
        }
        for index in 0..NATURAL {
            cohort.sample(AccountId::new(6_000 + index));
        }
        let honest: Vec<u64> = (0..ARMED)
            .map(|index| 5_000 + index)
            .chain((0..NATURAL).map(|index| 6_000 + index))
            .collect();
        let outside: Vec<u64> = (0..OUTSIDE).map(|index| 9_000 + index).collect();

        let mut id: u128 = 1_000_000;
        let mut now_ms: u64 = 2_000;
        let mut submit = |account: u64, drop_one: bool, duplicate: bool| {
            id += 1;
            now_ms += TICK_MS;
            let mut intent = attestable_intent(id);
            if duplicate {
                // A buggy client attaching one co-signature twice: refused by
                // the always-on rule, above the switch, so the quorum never
                // runs. Qualifying activity that goes unobserved, which is the
                // only thing that can move coverage off 1.0.
                let once = intent.attest(&witnesses[1]);
                intent.attestations.push(once.clone());
                intent.attestations.push(once);
            } else {
                attest_required(&mut intent, &witnesses, &epochs);
                if drop_one {
                    intent.attestations.pop();
                }
            }
            let _ = metered.check_at(&intent, &cx(Some(account)), now_ms);
        };

        // The honest cohort acts honestly: every intent carries the drawn
        // subset. One client in the cohort has the duplicate-attestation bug,
        // once — that is the coverage story, and it is deliberately small
        // enough to stay above clause (e)'s three-nines floor so the report's
        // failing term is the one a harness genuinely cannot supply.
        for (index, account) in honest.iter().enumerate() {
            for round in 0..INTENTS_PER_ACCOUNT {
                submit(*account, false, index == 0 && round == 0);
            }
        }
        // Outside the cohort, a population that under-attests: real
        // would-have-acted events, spread across accounts, none of them a
        // false positive because none of these accounts is in H.
        for (index, account) in outside.iter().enumerate() {
            for round in 0..INTENTS_PER_ACCOUNT {
                submit(*account, (index as u64 + round).is_multiple_of(3), false);
            }
        }

        let artifact = ramp::RampArtifact::new(
            ramp::Provenance {
                traffic: "harness".to_owned(),
                source: "orrery_persistd::intent::tests::emit_ramp_artifact, \
                         BaselineIntentValidator in shadow over a fixture epoch of 7 witnesses"
                    .to_owned(),
                note: "Simulated traffic, not a fleet. Clause (e)'s W term is meaningless here \
                       by construction: the clock advances 10 ms per intent inside one fixture \
                       epoch's 60 s usability window, so W is under a minute rather than \
                       the 30 days a production leg needs. Everything else — fp_count, \
                       coverage and its denominator, |H|, account spread — is measured from \
                       the same shadow arm a deployment would run."
                    .to_owned(),
            },
            vec![meter.snapshot(&cohort)],
        );

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/data/ramp-shadow-2026-08-22.json");
        std::fs::write(&path, artifact.to_json().expect("serializable"))
            .expect("docs/data is writable");
        println!("wrote {}", path.display());
    }

    // -- D32 clause (c) and (f): the runtime lever -------------------------

    /// The posture is settable while the validator runs, and setting it
    /// changes what the *next* intent gets — which is the whole point of
    /// there being two layers rather than one.
    ///
    /// A CLI-only flag is not reversible on an incident timescale: rolling a
    /// gateway restart drops sessions and takes minutes. This is the seam a
    /// `ramp/attestation_quorum` poller writes into.
    #[test]
    fn the_posture_changes_a_running_validators_mode_without_a_restart() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);

        let mut short = attestable_intent(70);
        attest_required(&mut short, &witnesses, &epochs);
        short.attestations.pop();
        assert_eq!(
            validator.check_at(&short, &cx(Some(7)), 2_000),
            Err(RejectionCause::ThresholdNotMet)
        );

        // A clone shares the cell, exactly as the gateway's `Arc<dyn
        // IntentValidator>` and a poller's handle would.
        let posture = validator.posture();
        assert!(
            posture.auto_suspend(),
            "clause (f): an acting control demotes to shadow"
        );
        assert_eq!(validator.enforcement(), AttestationEnforcement::Shadow);
        assert!(
            validator.check_at(&short, &cx(Some(7)), 2_000).is_ok(),
            "the same validator, the same intent, and it no longer acts"
        );

        posture.set(AttestationEnforcement::Required);
        assert_eq!(
            validator.check_at(&short, &cx(Some(7)), 2_000),
            Err(RejectionCause::ThresholdNotMet),
            "promotion is an operator act, and it works"
        );
    }

    /// Automation may make the fleet safer and never less safe.
    ///
    /// D32 clause (f)'s asymmetry, enforced by the API rather than by
    /// convention: the trip demotes `Required → Shadow` and does nothing else.
    /// Falling to `Off` would blind the cluster during exactly the incident
    /// that tripped it and would make the trigger a censorship lever — spike
    /// the verdict rate, turn enforcement off.
    #[test]
    fn auto_suspend_only_demotes_and_only_as_far_as_shadow() {
        let acting = AttestationPosture::new(AttestationEnforcement::Required);
        assert!(acting.auto_suspend());
        assert_eq!(acting.get(), AttestationEnforcement::Shadow);
        assert!(
            !acting.auto_suspend(),
            "a suspended control has no action left to suspend"
        );
        assert_eq!(
            acting.get(),
            AttestationEnforcement::Shadow,
            "and it never falls further: shadow keeps observing"
        );

        let silent = AttestationPosture::new(AttestationEnforcement::Off);
        assert!(!silent.auto_suspend());
        assert_eq!(
            silent.get(),
            AttestationEnforcement::Off,
            "automation does not start an evaluation nobody asked for either"
        );
    }

    /// The CLI spellings D32 clause (c)'s inventory names, round-tripped.
    #[test]
    fn the_three_modes_parse_and_print_as_the_inventory_spells_them() {
        for mode in [
            AttestationEnforcement::Off,
            AttestationEnforcement::Shadow,
            AttestationEnforcement::Required,
        ] {
            assert_eq!(mode.as_str().parse::<AttestationEnforcement>(), Ok(mode));
        }
        assert_eq!(
            AttestationEnforcement::Off.as_str(),
            "off",
            "the inventory's spelling, and the flag's default"
        );
        assert!("live".parse::<AttestationEnforcement>().is_err());

        // Only `required` acts; only `shadow` and `required` evaluate. These
        // two predicates are what every mode decision in this crate is written
        // against, so a fourth arm has to answer them explicitly.
        assert!(AttestationEnforcement::Required.acts());
        assert!(!AttestationEnforcement::Shadow.acts());
        assert!(!AttestationEnforcement::Off.acts());
        assert!(AttestationEnforcement::Shadow.evaluates());
        assert!(!AttestationEnforcement::Off.evaluates());
    }

    // -- D10 item 4's account half (D31) ----------------------------------
    //
    // Everything below turns on one arrangement: a witness whose **NodeId** is
    // not the issuer's and is genuinely in the announced set, but whose
    // **account** is a party's. Every test asserts that the NodeId-level
    // checks admit it first, because a test that only re-proves the
    // same-NodeId case proves nothing this work added.

    /// The issuer's own account, under a second NodeId, may not witness — and
    /// it is refused with the quorum switched **off**.
    #[test]
    fn a_second_device_of_the_issuers_own_account_may_not_witness() {
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        // The attacker's second device: a distinct keypair, bound to the same
        // account the connection authenticated as.
        let attacker = &witnesses[0];
        assert_ne!(
            attacker.public(),
            issuer_key().public(),
            "the whole point is a NodeId the existing checks cannot object to"
        );
        let bindings = bindings_from(announced.iter().enumerate().map(|(index, node)| {
            let account = if index == 0 {
                ISSUER_ACCOUNT
            } else {
                WITNESS_ACCOUNT_BASE + index as u64
            };
            (*node, AccountId::new(account))
        }));

        let mut intent = attestable_intent(40);
        let attestation = intent.attest(attacker);
        assert!(
            attestation.verify(&intent),
            "a real co-signature over D27's witness preimage, not a forgery"
        );
        intent.attestations.push(attestation);

        // It passes all three of the NodeId-level checks that existed before
        // this: not the issuer, not the connection identity, not a repeat.
        assert!(
            BaselineIntentValidator::check(&intent, &cx(Some(ISSUER_ACCOUNT))).is_ok(),
            "the pre-D31 envelope checks admit it, which is the exposure"
        );

        // And the account-level check refuses it at the default enforcement
        // mode, because party exclusion is not part of the quorum.
        let validator = BaselineIntentValidator::permissive_with_bindings(bindings);
        assert_eq!(validator.enforcement(), AttestationEnforcement::Off);
        assert_eq!(
            validator.check_at(&intent, &cx(Some(ISSUER_ACCOUNT)), 2_000),
            Err(RejectionCause::PartyAccountWitness)
        );
        assert_eq!(
            RejectionCause::PartyAccountWitness.wire_reason(),
            orrery_protocol::REASON_SELF_WITNESS,
            "both halves of party exclusion answer one code; the sub-\
             distinction stays in the log"
        );
    }

    /// The **counterparty's** account may not witness either, and the
    /// counterparty is named by the intent's own ops rather than by the
    /// connection.
    #[test]
    fn a_witness_bound_to_the_counterparty_account_is_refused() {
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        // The seller of the transfer, which `party_accounts` reads out of the
        // op layout. Nothing on the connection names it.
        const SELLER: u64 = 8;
        let bindings = bindings_from(announced.iter().enumerate().map(|(index, node)| {
            let account = if index == 3 {
                SELLER
            } else {
                WITNESS_ACCOUNT_BASE + index as u64
            };
            (*node, AccountId::new(account))
        }));

        let key = issuer_key();
        let mut intent = Intent {
            evidence: None,
            intent_id: 41,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(EPOCH_HANDLE),
            ops: vec![transfer_op(1, SELLER, ISSUER_ACCOUNT, 10)],
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        let attestation = intent.attest(&witnesses[3]);
        intent.attestations.push(attestation);

        assert!(
            BaselineIntentValidator::check(&intent, &cx(Some(ISSUER_ACCOUNT))).is_ok(),
            "neither the issuer's NodeId nor the connection's, and not a repeat"
        );
        assert_eq!(
            BaselineIntentValidator::permissive_with_bindings(bindings).check_at(
                &intent,
                &cx(Some(ISSUER_ACCOUNT)),
                2_000
            ),
            Err(RejectionCause::PartyAccountWitness),
            "D10 item 4 excludes *all* parties, not just the submitter"
        );
    }

    /// Two devices of one non-party account are one witness, not two.
    #[test]
    fn two_node_ids_of_one_account_cannot_fill_two_slots() {
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        // An accomplice account with two devices, both announced, neither a
        // party to the intent.
        const ACCOMPLICE: u64 = 2_600;
        let bindings = bindings_from(announced.iter().enumerate().map(|(index, node)| {
            let account = if index < 2 {
                ACCOMPLICE
            } else {
                WITNESS_ACCOUNT_BASE + index as u64
            };
            (*node, AccountId::new(account))
        }));

        let mut intent = attestable_intent(42);
        for key in witnesses.iter().take(2) {
            let attestation = intent.attest(key);
            intent.attestations.push(attestation);
        }
        assert!(
            BaselineIntentValidator::check(&intent, &cx(Some(ISSUER_ACCOUNT))).is_ok(),
            "two different NodeIds, so the no-repeat rule has nothing to say"
        );
        assert_eq!(
            BaselineIntentValidator::permissive_with_bindings(bindings).check_at(
                &intent,
                &cx(Some(ISSUER_ACCOUNT)),
                2_000
            ),
            Err(RejectionCause::DuplicateAttestingAccount)
        );
        // The two causes are distinct so an operator can count "one account,
        // two devices" apart from "one device, twice".
        assert_ne!(
            RejectionCause::DuplicateAttestingAccount.as_str(),
            RejectionCause::DuplicateAttestation.as_str()
        );
    }

    /// D31 clause (f): **a miss excludes, and the closure is a demotion, not a
    /// refusal.**
    ///
    /// Named after the clause so a future reader can find the decision from
    /// the test. Both halves are asserted, because either one alone is
    /// misleading: excluding without demoting would lose honest intents, and
    /// demoting without excluding would be the fail-open behaviour that makes
    /// the whole check decorative.
    #[test]
    fn d31_clause_f_a_missing_binding_excludes_and_demotes() {
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();

        // Half one. Only four of the seven announced NodeIds resolve, so
        // `|E(I)|` is four — one below `WITNESS_SET_FLOOR_N` — and a
        // reversible intent carrying a commitment is committed
        // *provisionally* rather than refused.
        let partial =
            bindings_from(
                announced.iter().take(4).enumerate().map(|(index, node)| {
                    (*node, AccountId::new(WITNESS_ACCOUNT_BASE + index as u64))
                }),
            );
        let epochs = epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000);
        let closing = BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            Arc::new(epoch_fixture::CoverAllInterest),
            partial,
        );
        assert!(
            matches!(
                closing.check_at(
                    &low_population_intent(43, true),
                    &cx(Some(ISSUER_ACCOUNT)),
                    2_000
                ),
                Ok(Admission::Provisional(_))
            ),
            "seven were announced and three cannot be resolved, so the \
             gateway cannot judge — which is D29's path, not a refusal"
        );

        // The control: the same seven NodeIds, all resolved, and the same
        // intent is no longer under-populated. Without this the assertion
        // above would also pass on a validator that demoted everything.
        let resolving = BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            Arc::new(epoch_fixture::CoverAllInterest),
            distinct_bindings(&announced),
        );
        assert_eq!(
            resolving.check_at(
                &low_population_intent(44, true),
                &cx(Some(ISSUER_ACCOUNT)),
                2_000
            ),
            Err(RejectionCause::ThresholdNotMet),
            "|E(I)| = 7 with every binding resolved, so the intent is simply \
             un-attested"
        );

        // Half two. An attestation *from* an announced NodeId whose binding
        // did not resolve is refused, and it is named for what it is — a
        // resolver miss — rather than as a forgery.
        let six =
            bindings_from(
                announced.iter().take(6).enumerate().map(|(index, node)| {
                    (*node, AccountId::new(WITNESS_ACCOUNT_BASE + index as u64))
                }),
            );
        let mut unresolved = attestable_intent(45);
        let attestation = unresolved.attest(&witnesses[6]);
        unresolved.attestations.push(attestation);
        let validator = BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            Arc::new(epoch_fixture::CoverAllInterest),
            six,
        );
        assert_eq!(
            validator.check_at(&unresolved, &cx(Some(ISSUER_ACCOUNT)), 2_000),
            Err(RejectionCause::UnresolvedWitnessBinding),
            "announced, so `WitnessOutsideAnnouncedSet` would send an \
             operator hunting a forgery that is not there"
        );
        assert_eq!(
            RejectionCause::UnresolvedWitnessBinding.wire_reason(),
            orrery_protocol::REASON_ATTESTATION_QUORUM,
            "one wire code for the whole quorum space (D30 clause (c))"
        );
    }

    /// The regression guard: honest witnesses bound to unrelated accounts are
    /// untouched by any of the above, at both enforcement modes.
    #[test]
    fn honest_witnesses_on_unrelated_accounts_are_unaffected() {
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        let bindings = distinct_bindings(&announced);

        // `Off`: a real co-signature from a stranger still admits.
        let mut intent = attestable_intent(46);
        let attestation = intent.attest(&witnesses[2]);
        intent.attestations.push(attestation);
        assert!(
            BaselineIntentValidator::permissive_with_bindings(Arc::clone(&bindings))
                .check_at(&intent, &cx(Some(ISSUER_ACCOUNT)), 2_000)
                .is_ok()
        );

        // `Required`: the full K-of-N predicate still admits, so the account
        // filter has not quietly shrunk `E(I)` under an honest set.
        let (validator, epochs) = enforcing_over(&witnesses);
        let mut attested = attestable_intent(47);
        attest_required(&mut attested, &witnesses, &epochs);
        assert!(matches!(
            validator.check_at(&attested, &cx(Some(ISSUER_ACCOUNT)), 2_000),
            Ok(Admission::Attested(_))
        ));
    }

    /// `E(I)` keeps announced order when the account filter removes a member
    /// from the middle of it.
    ///
    /// The recorded vector is the object an auditor draws over (D27
    /// clause (f)), so a filter that sorted or de-duplicated would silently
    /// make the audit's object a different one from the announcement's.
    #[test]
    fn party_exclusion_preserves_announced_order() {
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        let bindings = bindings_from(announced.iter().enumerate().map(|(index, node)| {
            let account = if index == 3 {
                ISSUER_ACCOUNT
            } else {
                WITNESS_ACCOUNT_BASE + index as u64
            };
            (*node, AccountId::new(account))
        }));

        let intent = attestable_intent(48);
        let parties = party_accounts(&intent, Some(AccountId::new(ISSUER_ACCOUNT)));
        let eligible =
            eligible_after_party_exclusion(&announced, &intent, &parties, bindings.as_ref());

        let expected: Vec<NodeId> = announced
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 3)
            .map(|(_, node)| *node)
            .collect();
        assert_eq!(eligible, expected, "index 3 removed, the rest in place");
    }

    /// An announced set too small to draw from is refused rather than drawn
    /// over — and it is a *different* refusal from a forgery.
    #[test]
    fn an_epoch_below_the_population_floor_makes_no_draw() {
        // Five announced, one of whom is the issuer: four eligible, one below
        // `WITNESS_SET_FLOOR_N`. A draw from four is not the hypergeometric
        // draw the collusion arithmetic is computed over.
        let witnesses = witness_keys(4);
        let mut announced: Vec<NodeId> =
            witnesses.iter().map(iroh_base::SecretKey::public).collect();
        announced.push(issuer_key().public());
        let epochs = epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000);
        let validator = enforcing_with(&epochs);

        let mut intent = attestable_intent(9);
        for key in &witnesses {
            let attestation = intent.attest(key);
            intent.attestations.push(attestation);
        }
        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::ProvisionalNoEvidence),
            "the population predicate holds, so the refusal is now about the \
             intent's own classification and no longer about the draw"
        );
    }

    /// D29 clause 2's second line, end to end at the admission seam: an
    /// under-populated epoch plus a reversible intent plus a commitment is a
    /// **provisional** admission, not a refusal and not an ordinary one.
    #[test]
    fn an_under_populated_epoch_admits_a_reversible_intent_provisionally() {
        let epochs = under_populated_epoch();
        let validator = enforcing_with(&epochs);
        let intent = low_population_intent(30, true);
        assert!(
            matches!(
                validator.check_at(&intent, &cx(Some(7)), 2_000),
                Ok(Admission::Provisional(_))
            ),
            "|elig(i)| < N and reversible(i) is clause 2's second line"
        );
        // `validate` is deliberately not asserted here: it reads the
        // registrar clock, and this fixture's epoch was accepted at a
        // synthetic 1 000 ms, so the call would fail on `EpochStale` for a
        // reason that has nothing to do with the population predicate. The
        // `Admission -> IntentVerdict` mapping is three lines with no
        // branching, and the gateway asserts it end to end.
    }

    /// The bypass check, and the single most important assertion in #150: an
    /// intent whose announced set *could* have attested it and simply did not
    /// is refused. If this ever admits provisionally, provisional commit stops
    /// being the answer to "there was nobody there to sign" and becomes a
    /// universal bypass of the mechanism P5 exists to add.
    #[test]
    fn an_adequately_populated_epoch_with_missing_attestations_is_refused() {
        // Six announced, one of whom is the issuer: five eligible, exactly at
        // `WITNESS_SET_FLOOR_N`. There *were* witnesses; the submitter did not
        // bring their signatures.
        let witnesses = witness_keys(5);
        let mut announced: Vec<NodeId> =
            witnesses.iter().map(iroh_base::SecretKey::public).collect();
        announced.push(issuer_key().public());
        let epochs = epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000);
        let validator = enforcing_with(&epochs);

        // Carrying a commitment, so the refusal cannot be blamed on clause 6:
        // this intent is provisional-*eligible* in every respect except the
        // one that matters, which is that it did not need the path.
        let intent = low_population_intent(31, true);
        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::ThresholdNotMet),
            "clause 2's third line: otherwise, refuse"
        );

        // And the same intent with K attestations that are simply the wrong
        // ones is refused too — the other half of "could have been attested".
        let mut shopped = low_population_intent(32, true);
        for key in witnesses.iter().take(orrery_protocol::WITNESS_QUORUM_K) {
            let attestation = shopped.attest(key);
            shopped.attestations.push(attestation);
        }
        let verdict = validator.check_at(&shopped, &cx(Some(7)), 2_000);
        assert!(
            matches!(
                verdict,
                Ok(Admission::Attested(_)) | Err(RejectionCause::RequiredWitnessMissing)
            ),
            "either the draw landed on these three or it did not; neither \
             answer is a provisional commit, and this got {verdict:?}"
        );
    }

    /// Clause 3 at the admission seam: the population predicate holds and the
    /// intent is still refused, because a transfer is not something the
    /// cluster can undo by writing an inverse into the submitter's own rows.
    #[test]
    fn an_under_populated_epoch_still_refuses_a_transfer() {
        let epochs = under_populated_epoch();
        let validator = enforcing_with(&epochs);
        let key = issuer_key();
        let mut intent = Intent {
            intent_id: 33,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(EPOCH_HANDLE),
            ops: vec![transfer_op(1, 8, 7, 10)],
            attestations: Vec::new(),
            evidence: Some(tests_support::commitment()),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::ProvisionalIneligible)
        );
        assert_eq!(
            RejectionCause::ProvisionalIneligible.wire_reason(),
            orrery_protocol::REASON_PROVISIONAL_INELIGIBLE,
            "the cause is named on the wire, not collapsed into the quorum code"
        );
    }

    /// An announced set of four eligible witnesses — one below
    /// `WITNESS_SET_FLOOR_N`, which is `low_pop(i)`.
    fn under_populated_epoch() -> Arc<crate::witness_epoch::WitnessEpochAuthority> {
        let mut announced: Vec<NodeId> = witness_keys(4)
            .iter()
            .map(iroh_base::SecretKey::public)
            .collect();
        announced.push(issuer_key().public());
        epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000)
    }

    /// A provisional-eligible intent naming this module's fixture epoch: one
    /// self-credit into the submitting account.
    fn low_population_intent(id: u128, with_evidence: bool) -> Intent {
        let key = issuer_key();
        let mut intent = Intent {
            intent_id: id,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(EPOCH_HANDLE),
            ops: vec![ledger_op(7, GOLD, 100)],
            attestations: Vec::new(),
            evidence: with_evidence.then(tests_support::commitment),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    /// The switch's off position leaves the pre-existing path untouched: an
    /// intent with zero attestations, naming an epoch nothing announced, still
    /// commits.
    #[test]
    fn enforcement_off_admits_the_zero_attestation_path_every_issuer_sends() {
        let validator = BaselineIntentValidator::permissive();
        assert_eq!(validator.enforcement(), AttestationEnforcement::Off);

        let intent = attestable_intent(10);
        assert!(intent.attestations.is_empty());
        assert!(
            validator.check_at(&intent, &cx(Some(7)), 2_000).is_ok(),
            "every production issuer in this tree sends exactly this, and the \
             measurement task's control arm is this path by definition"
        );

        // And an enforcing validator with no cache fails closed rather than
        // open, which is the direction a misconfiguration must take.
        let blind = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Required),
            epochs: None,
            interest: None,
            bindings: None,
            observer: None,
        };
        assert_eq!(
            blind.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::UnknownEpoch)
        );

        // The same direction for D30's second authority: an enforcing
        // validator that can establish standing for nobody refuses, rather
        // than treating a missing predicate as a satisfied one.
        let witnesses = witness_keys(7);
        let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
        let standingless = BaselineIntentValidator {
            enforcement: AttestationPosture::new(AttestationEnforcement::Required),
            epochs: Some(epoch_fixture::cache_with(EPOCH_HANDLE, &announced, 1_000)),
            interest: None,
            bindings: Some(distinct_bindings(&announced)),
            observer: None,
        };
        assert_eq!(
            standingless.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::NoStandingInCell)
        );
    }

    /// The self-witness refusal outranks the quorum, and the quorum never
    /// rescues a party attestation.
    #[test]
    fn a_party_attestation_is_refused_even_when_the_draw_would_have_named_it() {
        let witnesses = witness_keys(7);
        let (validator, epochs) = enforcing_over(&witnesses);

        let mut intent = attestable_intent(11);
        attest_required(&mut intent, &witnesses, &epochs);
        let self_attestation = intent.attest(&issuer_key());
        intent.attestations.push(self_attestation);

        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 2_000),
            Err(RejectionCause::SelfWitness),
            "party exclusion is checked above the draw and carries its own \
             wire code, because it is never a bad client"
        );
    }

    // -- D30: which announced set is allowed to judge the intent ------------
    //
    // The cache resolves by handle and holds every cell any peer couriered an
    // announcement for, so the shape these tests need is two cells in one
    // cache and an issuer standing in exactly one of them.

    /// The handle of the cell the issuer stands in — the fixture cell
    /// [`epoch_fixture::cache_with`] announces under, `CellId::ROOT`.
    const HOME_HANDLE: u64 = EPOCH_HANDLE;
    /// The handle of a second cell, announced to the same gateway by some
    /// other peer. One `u64` away from any submitter that can reach it.
    const AWAY_HANDLE: u64 = 0x0001_0000_0000_0002;

    /// The second cell. A child of the root rather than the root itself, so
    /// the two announcements are genuinely for different cells and the
    /// standing grant can cover one without covering the other.
    fn away_cell() -> orrery_protocol::CellId {
        orrery_protocol::CellId::ROOT.children()[0]
    }

    /// A gateway holding announcements for two cells, and an issuer whose
    /// coordinator grant covers only the first.
    ///
    /// Returns the validator, the cache, the seven witnesses announced at
    /// home, and the seven announced away. The two announced sets are
    /// **disjoint**, which is what makes "the draw ran over the wrong cell"
    /// observable rather than a coincidence of overlapping membership.
    fn two_cell_gateway() -> (
        BaselineIntentValidator,
        Arc<crate::witness_epoch::WitnessEpochAuthority>,
        Vec<iroh_base::SecretKey>,
        Vec<iroh_base::SecretKey>,
    ) {
        let home = witness_keys(7);
        let away: Vec<iroh_base::SecretKey> = (10..17).map(epoch_fixture::witness_secret).collect();
        let home_ids: Vec<NodeId> = home.iter().map(iroh_base::SecretKey::public).collect();
        let away_ids: Vec<NodeId> = away.iter().map(iroh_base::SecretKey::public).collect();
        assert!(
            away_ids.iter().all(|node| !home_ids.contains(node)),
            "the two announced sets must be disjoint, or these tests prove nothing"
        );

        let epochs = epoch_fixture::cache_with(HOME_HANDLE, &home_ids, 1_000);
        epoch_fixture::add_cell_epoch(&epochs, away_cell(), AWAY_HANDLE, &away_ids, 1_000);

        let interest = Arc::new(crate::gateway::SnapshotInterestAuthority::from_snapshots([
            orrery_protocol::CoordinatorInterestSnapshot {
                peer: issuer_key().public(),
                epoch: orrery_protocol::Epoch::new(1),
                grid: orrery_protocol::GridId::ROOT,
                covered_cells: vec![orrery_protocol::CellId::ROOT],
                valid_until_ms: 50_000,
            },
        ]));
        // Both announced sets get bindings, and disjoint accounts: D30's
        // standing predicate is what these tests are about, so no candidate
        // may drop out of `E(I)` for a D31 reason.
        let mut announced = home_ids.clone();
        announced.extend_from_slice(&away_ids);
        let validator = BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            interest.clone() as Arc<_>,
            distinct_bindings(&announced),
        );
        (validator, epochs, home, away)
    }

    /// A signed intent naming `handle`, with one opaque op.
    fn intent_naming(handle: u64, id: u128) -> Intent {
        let mut intent = attestable_intent(id);
        intent.cell_epoch = CellEpoch::new(handle);
        intent.attestations.clear();
        intent.sign(&issuer_key());
        intent
    }

    /// Attach the co-signatures the draw under `handle` requires.
    fn attest_required_under(
        intent: &mut Intent,
        handle: u64,
        witnesses: &[iroh_base::SecretKey],
        epochs: &crate::witness_epoch::WitnessEpochAuthority,
    ) -> Vec<NodeId> {
        let epoch = epochs.resolve(handle).expect("fixture epoch is cached");
        let eligible = orrery_protocol::eligible_witnesses(&epoch.snapshot.selected, intent.issuer);
        let required = epoch.required_witnesses(intent.intent_id, &eligible);
        for node in &required {
            let key = witnesses
                .iter()
                .find(|key| key.public() == *node)
                .expect("the draw only names announced witnesses");
            let attestation = intent.attest(key);
            intent.attestations.push(attestation);
        }
        required
    }

    /// D30 clause (a). An intent naming a cell-epoch the issuer has no
    /// standing in is refused, under a cause of its own — even when it
    /// carries exactly the co-signatures that cell's draw requires.
    ///
    /// The control arm is the same submitter, the same op, the same clock,
    /// naming the cell it *does* stand in: that one commits. Without it this
    /// test would pass against an implementation that simply refused
    /// everything.
    #[test]
    fn an_intent_naming_a_cell_epoch_the_issuer_does_not_stand_in_is_refused() {
        let (validator, epochs, home, away) = two_cell_gateway();

        let mut shopped = intent_naming(AWAY_HANDLE, 21);
        let required_away = attest_required_under(&mut shopped, AWAY_HANDLE, &away, &epochs);
        assert_eq!(required_away.len(), orrery_protocol::WITNESS_QUORUM_K);
        for attestation in &shopped.attestations {
            assert!(
                attestation.verify(&shopped),
                "every co-signature is genuine, so only the standing check can refuse this"
            );
        }
        assert_eq!(
            validator.check_at(&shopped, &cx(Some(7)), 2_000),
            Err(RejectionCause::NoStandingInCell),
            "the submitter does not stand in the away cell, so that cell's \
             announced set does not judge its intent"
        );

        let mut at_home = intent_naming(HOME_HANDLE, 22);
        attest_required_under(&mut at_home, HOME_HANDLE, &home, &epochs);
        assert!(
            validator.check_at(&at_home, &cx(Some(7)), 2_000).is_ok(),
            "the same issuer, in the cell it stands in, commits"
        );
    }

    /// The required subset is drawn from the cell the intent is judged under,
    /// never from one the submitter picked for its membership.
    ///
    /// The colluders here are the away cell's required trio — the best
    /// possible hand under the set the submitter would like to be judged by.
    /// Both of the moves that hand enables are closed: presenting it under the
    /// away handle fails the standing check, and presenting it under the home
    /// handle fails set membership, because the home draw runs over the home
    /// announcement and names nobody from the away set.
    #[test]
    fn the_required_subset_is_drawn_from_the_cell_that_judges_the_intent() {
        let (validator, epochs, home, away) = two_cell_gateway();

        let mut probe = intent_naming(AWAY_HANDLE, 23);
        let colluders = attest_required_under(&mut probe, AWAY_HANDLE, &away, &epochs);

        // The same co-signatures, re-made over an intent naming the home
        // cell-epoch: real signatures from real announced witnesses — of
        // another cell.
        let mut shopped = intent_naming(HOME_HANDLE, 23);
        for node in &colluders {
            let key = away
                .iter()
                .find(|key| key.public() == *node)
                .expect("a colluder is an away witness");
            let attestation = shopped.attest(key);
            assert!(attestation.verify(&shopped), "the signature is genuine");
            shopped.attestations.push(attestation);
        }
        assert_eq!(
            validator.check_at(&shopped, &cx(Some(7)), 2_000),
            Err(RejectionCause::WitnessOutsideAnnouncedSet),
            "the home cell's draw runs over the home cell's announcement"
        );

        // And the home draw names only home witnesses, which is the same fact
        // from the other side.
        let mut honest = intent_naming(HOME_HANDLE, 23);
        let required_home = attest_required_under(&mut honest, HOME_HANDLE, &home, &epochs);
        assert!(
            required_home.iter().all(|node| !colluders.contains(node)),
            "disjoint announced sets cannot share a required subset"
        );
        assert!(validator.check_at(&honest, &cx(Some(7)), 2_000).is_ok());
    }

    /// Standing is checked above staleness, and the order is deliberate: a
    /// submitter with no standing must not learn the age of a cell-epoch it
    /// has no business enumerating.
    #[test]
    fn no_standing_outranks_a_stale_epoch() {
        let (validator, epochs, _home, away) = two_cell_gateway();
        let mut shopped = intent_naming(AWAY_HANDLE, 24);
        attest_required_under(&mut shopped, AWAY_HANDLE, &away, &epochs);

        // Past `epoch_ms + accept_grace_ms` from acceptance at t=1000, so the
        // away epoch is genuinely stale as well as unstood-in.
        let long_after = 1_000 + 60_000 + 1;
        assert!(
            !epochs
                .resolve(AWAY_HANDLE)
                .expect("cached")
                .snapshot
                .usable_at(long_after),
            "the epoch must really be stale, or this proves nothing"
        );
        assert_eq!(
            validator.check_at(&shopped, &cx(Some(7)), long_after),
            Err(RejectionCause::NoStandingInCell)
        );
    }

    /// A lapsed grant is no standing. Interest expiry is enforced on the read
    /// path and nowhere else (D25 rule 3), so the same intent that committed
    /// inside the grant's window is refused outside it.
    #[test]
    fn a_lapsed_interest_grant_stops_binding_the_intent() {
        let (validator, epochs, home, _away) = two_cell_gateway();
        let mut intent = intent_naming(HOME_HANDLE, 25);
        attest_required_under(&mut intent, HOME_HANDLE, &home, &epochs);

        assert!(validator.check_at(&intent, &cx(Some(7)), 2_000).is_ok());
        // The fixture grant is valid until 50_000; the home epoch is usable
        // until 61_000, so this instant separates the two.
        assert!(
            epochs
                .resolve(HOME_HANDLE)
                .expect("cached")
                .snapshot
                .usable_at(55_000),
            "the epoch is still live here, so only the grant can refuse"
        );
        assert_eq!(
            validator.check_at(&intent, &cx(Some(7)), 55_000),
            Err(RejectionCause::NoStandingInCell)
        );
    }

    /// The refusal is logged under its own label and answered on the wire
    /// with the one quorum code, which is what stops it leaking the draw.
    #[test]
    fn no_standing_is_labelled_in_logs_and_collapsed_on_the_wire() {
        assert_eq!(
            RejectionCause::NoStandingInCell.as_str(),
            "no_standing_in_cell"
        );
        assert_eq!(
            RejectionCause::NoStandingInCell.wire_reason(),
            orrery_protocol::REASON_ATTESTATION_QUORUM
        );
    }

    /// The asset every trade test below prices in.
    const GOLD: u64 = 3;

    fn transfer_args(item: u64, seller: u64, buyer: u64, price: i64) -> ItemTransferArgs {
        ItemTransferArgs {
            item: orrery_protocol::ItemUid::new(item),
            seller: AccountId::new(seller),
            buyer: AccountId::new(buyer),
            asset: AssetId::new(GOLD),
            price,
        }
    }

    fn transfer_op(item: u64, seller: u64, buyer: u64, price: i64) -> IntentOp {
        IntentOp {
            op: LEDGER_ITEM_TRANSFER_OP,
            args: bytes::Bytes::copy_from_slice(
                &transfer_args(item, seller, buyer, price).encode(),
            ),
        }
    }

    /// A signed intent carrying one transfer, with an explicit `intent_id` so
    /// replay and concurrency tests can control idempotency.
    fn transfer_intent(id: u128, item: u64, seller: u64, buyer: u64, price: i64) -> Intent {
        let key = issuer_key();
        let mut intent = Intent {
            evidence: None,
            intent_id: id,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(0),
            ops: vec![transfer_op(item, seller, buyer, price)],
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    fn ledger_op(account: u64, asset: u64, delta: i64) -> IntentOp {
        let mut args = Vec::with_capacity(24);
        args.extend_from_slice(&account.to_le_bytes());
        args.extend_from_slice(&asset.to_le_bytes());
        args.extend_from_slice(&delta.to_le_bytes());
        IntentOp {
            op: LEDGER_CREDIT_OP,
            args: bytes::Bytes::from(args),
        }
    }

    /// The load rig's shape: one `Ruleset`-opaque op with small args, no
    /// attestations, on an authenticated session. This is the case that was
    /// refused unconditionally, leaving `intent_commit_ms` with no samples.
    #[test]
    fn baseline_admits_a_well_formed_opaque_intent() {
        let intent = intent_with(vec![IntentOp {
            op: 57_019,
            args: bytes::Bytes::from_static(b"trade"),
        }]);
        assert_eq!(
            BaselineIntentValidator::permissive().validate(&intent, &cx(Some(7))),
            IntentVerdict::Admit(IntentPrecheck { read_keys: vec![] }),
            "an opaque op is admitted: only the envelope is this filter's business"
        );
        // And it does not secretly depend on a session: an opaque op names no
        // account, so there is nothing to bind it to.
        assert!(matches!(
            BaselineIntentValidator::permissive().validate(&intent, &cx(None)),
            IntentVerdict::Admit(_)
        ));
    }

    #[test]
    fn baseline_refuses_an_intent_with_nothing_to_commit() {
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(vec![]), &cx(Some(7))),
            Err(RejectionCause::NoOps)
        );
    }

    #[test]
    fn baseline_bounds_the_transaction_an_intent_can_ask_for() {
        let many = (0..=MAX_OPS_PER_INTENT)
            .map(|i| IntentOp {
                op: (i as u16) + OPAQUE_OP_BASE,
                args: bytes::Bytes::new(),
            })
            .collect();
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(many), &cx(Some(7))),
            Err(RejectionCause::TooManyOps)
        );

        let fat = vec![IntentOp {
            op: OPAQUE_OP_BASE,
            args: bytes::Bytes::from(vec![0u8; MAX_OP_ARGS_BYTES + 1]),
        }];
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(fat), &cx(Some(7))),
            Err(RejectionCause::ArgsTooLarge)
        );

        // Under the per-op cap, over the whole-intent cap.
        let spread = (0..MAX_OPS_PER_INTENT)
            .map(|i| IntentOp {
                op: (i as u16) + OPAQUE_OP_BASE,
                args: bytes::Bytes::from(vec![0u8; MAX_OP_ARGS_BYTES]),
            })
            .collect::<Vec<_>>();
        assert!(spread.len() * MAX_OP_ARGS_BYTES > MAX_INTENT_ARGS_BYTES);
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(spread), &cx(Some(7))),
            Err(RejectionCause::ArgsTooLarge)
        );
    }

    /// The one op this cluster's executor interprets. A malformed one used to
    /// cost an FDB round trip and come back as `REASON_EXECUTOR_ERROR`.
    #[test]
    fn baseline_refuses_a_malformed_ledger_credit() {
        let short = vec![IntentOp {
            op: LEDGER_CREDIT_OP,
            args: bytes::Bytes::from_static(b"too short"),
        }];
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(short), &cx(Some(7))),
            Err(RejectionCause::MalformedLedgerOp)
        );
    }

    /// The authorization check: the executor's blind `Add` is a credit-anyone
    /// primitive, and the session's account is the only thing that scopes it.
    #[test]
    fn baseline_binds_a_ledger_credit_to_the_session_account() {
        let intent = intent_with(vec![ledger_op(7, 3, 500)]);
        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(Some(8))),
            Err(RejectionCause::LedgerOpForAnotherAccount),
            "a peer may not credit an account it did not authenticate as"
        );
        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(None)),
            Err(RejectionCause::LedgerOpForAnotherAccount),
            "no session means no account, so there is nothing to credit under"
        );
        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(Some(7))),
            Ok(IntentPrecheck {
                read_keys: vec![crate::keyspace::ledger_bal_key(
                    AccountId::new(7),
                    AssetId::new(3)
                )
                .to_vec()],
            }),
            "its own account is admitted, and the row it touches is named"
        );
    }

    /// Attestations are not required in P2, but a present one must be real.
    #[test]
    fn baseline_refuses_forged_and_repeated_attestations() {
        let witness = iroh_base::SecretKey::from_bytes(&[9u8; 32]);
        let base = intent_with(vec![IntentOp {
            op: 5,
            args: bytes::Bytes::new(),
        }]);

        let mut genuine = base.clone();
        genuine.attestations.push(base.attest(&witness));
        assert!(
            BaselineIntentValidator::check(&genuine, &cx(Some(7))).is_ok(),
            "a real co-signature is admitted"
        );

        // D27 clause (c)'s stated degradation, and the proof that the preimage
        // switch actually happened: a signature over the *issuer's* preimage —
        // the identical bytes this validator used to check against, and the
        // bytes a peer running the pre-D27 semantics still emits — is not an
        // attestation. It is refused here rather than counted, because a peer
        // that attached one is asserting it is real.
        let mut legacy = base.clone();
        legacy.attestations.push(Attestation {
            witness: witness.public(),
            signature: witness.sign(&base.signing_preimage()),
        });
        assert!(
            witness
                .public()
                .verify(&base.signing_preimage(), &legacy.attestations[0].signature)
                .is_ok(),
            "the legacy signature is cryptographically perfect over the old bytes"
        );
        assert_eq!(
            BaselineIntentValidator::check(&legacy, &cx(Some(7))),
            Err(RejectionCause::BadAttestation),
            "a signature valid over the issuer preimage is not an attestation"
        );

        // The role confusion the switch closes, from the other direction: the
        // issuer's own signature used to be a byte-valid attestation naming
        // anybody. It is now not even a signature over the right message.
        let mut lifted = base.clone();
        lifted.attestations.push(Attestation {
            witness: witness.public(),
            signature: base.signature,
        });
        assert_eq!(
            BaselineIntentValidator::check(&lifted, &cx(Some(7))),
            Err(RejectionCause::BadAttestation),
            "an issuer signature replayed as a co-signature no longer verifies"
        );

        let mut forged = base.clone();
        forged.attestations.push(Attestation {
            witness: witness.public(),
            signature: witness.sign(b"some other message"),
        });
        assert_eq!(
            BaselineIntentValidator::check(&forged, &cx(Some(7))),
            Err(RejectionCause::BadAttestation)
        );

        let mut repeated = genuine.clone();
        repeated.attestations.push(genuine.attestations[0].clone());
        assert_eq!(
            BaselineIntentValidator::check(&repeated, &cx(Some(7))),
            Err(RejectionCause::DuplicateAttestation),
            "one witness must not count twice toward a future K-of-N"
        );

        let mut flooded = base;
        for i in 0..=MAX_ATTESTATIONS {
            let key = iroh_base::SecretKey::from_bytes(&[i as u8; 32]);
            flooded.attestations.push(Attestation {
                witness: key.public(),
                signature: key.sign(b"unchecked"),
            });
        }
        assert_eq!(
            BaselineIntentValidator::check(&flooded, &cx(Some(7))),
            Err(RejectionCause::TooManyAttestations),
            "the count is bounded before any signature is verified"
        );
    }

    /// The issuer is a party to its own intent, so it may not witness it
    /// (D10 item 4; docs/07 §4.1).
    ///
    /// The degenerate arm is the one that matters, and it is the one that
    /// works in this tree: because a witness is verified over
    /// `Intent::signing_preimage()` — the same bytes the issuer signed — the
    /// issuer's own `intent.signature`, copied verbatim, *is* a byte-valid
    /// attestation naming the issuer. No fresh signing required.
    #[test]
    fn baseline_refuses_the_issuer_as_its_own_witness() {
        let key = issuer_key();
        let base = intent_with(vec![IntentOp {
            op: OPAQUE_OP_BASE,
            args: bytes::Bytes::new(),
        }]);

        // (a) The zero-effort forgery: the issuer signature, reused as an
        // attestation. Byte-for-byte identical to `base.signature`.
        let mut replayed = base.clone();
        replayed.attestations.push(Attestation {
            witness: key.public(),
            signature: base.signature,
        });
        assert_eq!(
            replayed.attestations[0].signature, base.signature,
            "this arm is only meaningful while the copy is exact"
        );
        assert!(
            key.public()
                .verify(
                    &base.signing_preimage(),
                    &replayed.attestations[0].signature
                )
                .is_ok(),
            "the copied signature really does verify as an attestation — that is \
             the defect, and a test that skipped this would pass on a tree that \
             had merely stopped verifying it"
        );
        assert_eq!(
            BaselineIntentValidator::check(&replayed, &cx(Some(7))),
            Err(RejectionCause::SelfWitness),
            "an issuer must not witness its own intent, however it got the bytes"
        );

        // (b) A freshly and correctly made self-attestation — the variant
        // D27's separate witness preimage would *not* stop, which is why the
        // party check has to exist independently of it.
        let mut fresh = base.clone();
        fresh.attestations.push(Attestation {
            witness: key.public(),
            signature: key.sign(&base.signing_preimage()),
        });
        assert_eq!(
            BaselineIntentValidator::check(&fresh, &cx(Some(7))),
            Err(RejectionCause::SelfWitness)
        );

        // (c) Hidden among honest ones: position must not matter.
        let honest = iroh_base::SecretKey::from_bytes(&[9u8; 32]);
        let mut mixed = base.clone();
        mixed.attestations.push(Attestation {
            witness: honest.public(),
            signature: honest.sign(&base.signing_preimage()),
        });
        mixed.attestations.push(Attestation {
            witness: key.public(),
            signature: base.signature,
        });
        assert_eq!(
            BaselineIntentValidator::check(&mixed, &cx(Some(7))),
            Err(RejectionCause::SelfWitness),
            "one genuine co-signature does not launder the self-attestation \
             sitting next to it"
        );

        // And the refusal is legible on the wire, not folded into the
        // generic validation code: #145's precedent, and the whole cost of
        // `DenyReason::WrongOwner` on the authority path.
        assert_eq!(
            BaselineIntentValidator::permissive().validate(&replayed, &cx(Some(7))),
            IntentVerdict::Reject {
                reason: orrery_protocol::REASON_SELF_WITNESS
            }
        );
        assert_eq!(
            RejectionCause::SelfWitness.wire_reason(),
            orrery_protocol::REASON_SELF_WITNESS
        );
        // Every other cause keeps the opaque code it had: this change adds a
        // number, it does not start enumerating the whole enum on the wire.
        assert_eq!(
            RejectionCause::BadAttestation.wire_reason(),
            orrery_protocol::REASON_VALIDATION_FAILED
        );
    }

    /// Independent witnesses are untouched by the party check — the control
    /// arm, without which "refuses self-witnessing" is satisfied by refusing
    /// every attestation.
    #[test]
    fn baseline_admits_genuinely_independent_witnesses() {
        let base = intent_with(vec![IntentOp {
            op: OPAQUE_OP_BASE,
            args: bytes::Bytes::new(),
        }]);
        let mut attested = base.clone();
        for seed in [9u8, 10, 11] {
            let witness = iroh_base::SecretKey::from_bytes(&[seed; 32]);
            assert_ne!(
                witness.public(),
                base.issuer,
                "a control arm whose witness is the issuer proves nothing"
            );
            attested.attestations.push(base.attest(&witness));
        }
        assert_eq!(
            BaselineIntentValidator::check(&attested, &cx(Some(7))),
            Ok(IntentPrecheck::default()),
            "three independent co-signatures are exactly what D10 asks for"
        );
    }

    /// The party check must sit in the cheap-check band, above everything
    /// that plans or performs a durable read.
    ///
    /// `IntentPrecheck::read_keys` is this crate's definition of "the durable
    /// rows the executor will read" (§7 step 1), so the ordering is
    /// falsifiable here: take an intent whose admission *does* name read keys,
    /// self-witness it, and require that the refusal wins. If a later refactor
    /// moved the party check below the ops loop it would still refuse — but if
    /// it moved below the loop *and* the loop learned to read, this is the
    /// test that catches it, because a `SelfWitness` verdict can never carry a
    /// read plan. The end-to-end half of the claim — that the executor is
    /// never reached at all — is
    /// `tests/intent_self_witness.rs`.
    #[test]
    fn self_witness_is_refused_before_any_durable_read_is_planned() {
        // Control: this exact intent, unattested, names two durable rows.
        let transfer = transfer_intent(41, 0xBEEF, 7, 8, 500);
        let planned = BaselineIntentValidator::check(&transfer, &cx(Some(8)))
            .expect("the control arm must be admissible, or it proves nothing");
        assert_eq!(
            planned.read_keys.len(),
            2,
            "the control names ledger/item and the buyer's balance"
        );

        // The same intent, self-witnessed: no read plan is produced at all.
        let mut self_witnessed = transfer.clone();
        self_witnessed.attestations.push(Attestation {
            witness: transfer.issuer,
            signature: transfer.signature,
        });
        assert_eq!(
            BaselineIntentValidator::check(&self_witnessed, &cx(Some(8))),
            Err(RejectionCause::SelfWitness),
            "the party check must beat the read planner, not follow it"
        );

        // It also beats signature verification, which is the other thing on
        // this path that costs anything: a self-attestation carrying outright
        // garbage is still `SelfWitness`, never `BadAttestation`. That
        // ordering is what keeps the refusal constant-cost under a flood.
        let mut garbage = transfer;
        garbage.attestations.push(Attestation {
            witness: garbage.issuer,
            signature: issuer_key().sign(b"not a preimage of anything"),
        });
        assert_eq!(
            BaselineIntentValidator::check(&garbage, &cx(Some(8))),
            Err(RejectionCause::SelfWitness)
        );
    }

    /// A quarantined account cannot use a cheap shape rejection to avoid
    /// attestation-authenticity validation.
    #[test]
    fn quarantined_session_checks_attestations_before_shape_rejections() {
        let witness = iroh_base::SecretKey::from_bytes(&[9u8; 32]);
        let mut intent = intent_with(
            (0..=MAX_OPS_PER_INTENT)
                .map(|op| IntentOp {
                    op: op as u16 + OPAQUE_OP_BASE,
                    args: bytes::Bytes::new(),
                })
                .collect(),
        );
        intent.attestations.push(Attestation {
            witness: witness.public(),
            signature: witness.sign(b"forged attestation"),
        });

        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(Some(7))),
            Err(RejectionCause::TooManyOps),
            "a good session retains the cheapest-first path"
        );
        assert_eq!(
            BaselineIntentValidator::check(
                &intent,
                &cx_with_standing(Some(7), SessionStanding::Quarantined)
            ),
            Err(RejectionCause::BadAttestation),
            "a quarantined session observes the forged co-signature first"
        );
    }

    // ── The item ownership transfer: admission ──────────────────────────

    /// The layout is a contract between the client, the validator and both
    /// executors, so it round-trips through one definition or it is three
    /// definitions.
    #[test]
    fn item_transfer_args_round_trip() {
        let args = transfer_args(0x0102_0304_0506_0708, 7, 8, 500);
        assert_eq!(args.encode().len(), LEDGER_ITEM_TRANSFER_ARGS_BYTES);
        assert_eq!(ItemTransferArgs::decode(&args.encode()), Ok(args));
    }

    #[test]
    fn baseline_refuses_a_malformed_item_transfer() {
        let short = vec![IntentOp {
            op: LEDGER_ITEM_TRANSFER_OP,
            args: bytes::Bytes::from_static(b"much too short"),
        }];
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(short), &cx(Some(8))),
            Err(RejectionCause::MalformedItemTransferOp)
        );

        // A negative price is a debit dressed as a credit: it would take money
        // *from* the seller and hand it to the buyer, past the sufficiency
        // check, which only ever looks at the buyer's side.
        let negative = vec![transfer_op(1, 7, 8, -500)];
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(negative), &cx(Some(8))),
            Err(RejectionCause::MalformedItemTransferOp)
        );
    }

    #[test]
    fn baseline_refuses_a_self_transfer() {
        assert_eq!(
            BaselineIntentValidator::check(
                &intent_with(vec![transfer_op(1, 8, 8, 0)]),
                &cx(Some(8))
            ),
            Err(RejectionCause::ItemTransferToSelf),
            "a trade with one party is not a trade"
        );
    }

    /// The authorization rule, on the only side a peer could abuse: the buyer
    /// pays, so the buyer must be this connection.
    #[test]
    fn baseline_binds_an_item_transfer_to_the_debit_side() {
        let intent = intent_with(vec![transfer_op(0x99, 7, 8, 500)]);
        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(Some(7))),
            Err(RejectionCause::ItemTransferForAnotherAccount),
            "the seller may not spend the buyer's balance"
        );
        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(None)),
            Err(RejectionCause::ItemTransferForAnotherAccount),
            "no session means no account to debit"
        );
        assert_eq!(
            BaselineIntentValidator::check(&intent, &cx(Some(8))),
            Ok(IntentPrecheck {
                read_keys: vec![
                    crate::keyspace::ledger_item_key(orrery_protocol::ItemUid::new(0x99)).to_vec(),
                    crate::keyspace::ledger_bal_key(AccountId::new(8), AssetId::new(GOLD)).to_vec(),
                ],
            }),
            "the buyer is admitted, and both rows the executor reads are named"
        );
    }

    // ── The item ownership transfer: execution ─────────────────────────

    /// Seed a ledger the trades below run against: A owns item 1, B holds 500
    /// gold.
    fn seeded() -> MemIntentExecutor {
        let exec = MemIntentExecutor::new();
        exec.seed_item(
            orrery_protocol::ItemUid::new(1),
            AccountId::new(7),
            b"state".to_vec(),
        );
        exec.credit(AccountId::new(8), AssetId::new(GOLD), 500);
        exec
    }

    #[tokio::test]
    async fn mem_executor_moves_the_item_and_both_balances() {
        let exec = seeded();
        let outcome = exec
            .execute(&transfer_intent(1, 1, 7, 8, 500))
            .await
            .unwrap();
        assert!(
            matches!(outcome, IntentOutcome::Committed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            exec.item_owner(orrery_protocol::ItemUid::new(1)),
            Some(AccountId::new(8)),
            "the single ownership row now names the buyer"
        );
        assert_eq!(exec.balance(AccountId::new(8), AssetId::new(GOLD)), 0);
        assert_eq!(exec.balance(AccountId::new(7), AssetId::new(GOLD)), 500);

        let receipts = exec.receipts();
        assert_eq!(receipts.len(), 1, "one trade banks one receipt");
        assert_eq!(receipts[0].intent_id, 1);
        assert_eq!(
            receipts[0].parties,
            vec![AccountId::new(7), AccountId::new(8)]
        );
        assert_eq!(receipts[0].ops, vec![LEDGER_ITEM_TRANSFER_OP]);
        assert_eq!(
            receipts[0].balance_deltas,
            vec![
                crate::keyspace::ReceiptBalanceDelta {
                    account: AccountId::new(8),
                    asset: AssetId::new(GOLD),
                    delta: -500,
                },
                crate::keyspace::ReceiptBalanceDelta {
                    account: AccountId::new(7),
                    asset: AssetId::new(GOLD),
                    delta: 500,
                },
            ],
            "both sides of the trade are recoverable as account deltas"
        );
        assert_eq!(
            receipts[0].ownership,
            vec![crate::keyspace::ReceiptOwnershipTransition {
                item: orrery_protocol::ItemUid::new(1),
                before: Some(AccountId::new(7)),
                after: Some(AccountId::new(8)),
            }],
            "item id and ownership before/after are recoverable"
        );
    }

    #[tokio::test]
    async fn pure_credit_banks_a_mandatory_effect_receipt() {
        let exec = MemIntentExecutor::new();
        let intent = intent_with(vec![ledger_op(7, GOLD, 83)]);
        let outcome = exec.execute(&intent).await.expect("execute credit");
        assert!(matches!(outcome, IntentOutcome::Committed { .. }));
        assert_eq!(exec.balance(AccountId::new(7), AssetId::new(GOLD)), 83);
        let receipts = exec.receipts();
        assert_eq!(receipts.len(), 1, "one ledger mutation banks one receipt");
        assert_eq!(receipts[0].intent_id, intent.intent_id);
        assert_eq!(receipts[0].parties, vec![AccountId::new(7)]);
        assert_eq!(receipts[0].ops, vec![LEDGER_CREDIT_OP]);
        assert_eq!(
            receipts[0].balance_deltas,
            vec![crate::keyspace::ReceiptBalanceDelta {
                account: AccountId::new(7),
                asset: AssetId::new(GOLD),
                delta: 83,
            }]
        );
        assert!(receipts[0].ownership.is_empty());
    }

    /// Each durable refusal is its own reason code, and each leaves the rows
    /// exactly as it found them. Distinguishable is the whole point: a refused
    /// double-spend and a broken cluster must not look alike.
    #[tokio::test]
    async fn mem_executor_names_every_durable_refusal_and_writes_nothing() {
        for (intent, reason, why) in [
            (
                transfer_intent(10, 1, 8, 7, 0),
                orrery_protocol::REASON_NOT_ITEM_OWNER,
                "B does not own item 1",
            ),
            (
                transfer_intent(11, 2, 7, 8, 0),
                orrery_protocol::REASON_NO_SUCH_ITEM,
                "item 2 has no ownership row",
            ),
            (
                transfer_intent(12, 1, 7, 7, 0),
                orrery_protocol::REASON_ITEM_TRANSFER_TO_SELF,
                "one account cannot be both parties",
            ),
            (
                transfer_intent(13, 1, 7, 8, 501),
                orrery_protocol::REASON_INSUFFICIENT_BALANCE,
                "B holds 500 and the price is 501",
            ),
        ] {
            let exec = seeded();
            let outcome = exec.execute(&intent).await.unwrap();
            assert_eq!(
                outcome,
                IntentOutcome::Rejected { reason },
                "{why}: expected its own reason code, got {outcome:?}"
            );
            assert_ne!(
                reason,
                orrery_protocol::REASON_EXECUTOR_ERROR,
                "{why}: a durable refusal is never a server fault"
            );
            assert_eq!(
                exec.item_owner(orrery_protocol::ItemUid::new(1)),
                Some(AccountId::new(7)),
                "{why}: the ownership row is untouched"
            );
            assert_eq!(
                exec.balance(AccountId::new(8), AssetId::new(GOLD)),
                500,
                "{why}: the debit side is untouched"
            );
            assert_eq!(
                exec.balance(AccountId::new(7), AssetId::new(GOLD)),
                0,
                "{why}: the credit side is untouched"
            );
            assert!(exec.receipts().is_empty(), "{why}: no receipt is banked");

            // And no idempotency row was burned: a refusal is not a durable
            // fact, so resubmitting the same id after the ledger moves must be
            // free to succeed.
            let again = exec.execute(&intent).await.unwrap();
            assert_eq!(
                again,
                IntentOutcome::Rejected { reason },
                "{why}: the same refusal, re-derived rather than recorded"
            );
        }
    }

    /// A rejection anywhere in the intent leaves *every* op's effect unapplied,
    /// including the ops that passed. This is the property staging the writes
    /// buys, and it is the one an executor that wrote as it went would lose.
    #[tokio::test]
    async fn mem_executor_applies_no_op_of_a_rejected_intent() {
        let exec = seeded();
        let key = issuer_key();
        let mut intent = Intent {
            evidence: None,
            intent_id: 20,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(0),
            ops: vec![
                ledger_op(8, GOLD, 1_000),
                // Item 2 does not exist, so the whole intent is refused.
                transfer_op(2, 7, 8, 0),
            ],
            attestations: Vec::new(),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        assert_eq!(
            exec.execute(&intent).await.unwrap(),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_NO_SUCH_ITEM
            }
        );
        assert_eq!(
            exec.balance(AccountId::new(8), AssetId::new(GOLD)),
            500,
            "the credit in op 0 did not land either"
        );
    }

    /// Replay: the same `intent_id` twice moves the item once.
    #[tokio::test]
    async fn mem_executor_transfers_once_under_replay() {
        let exec = seeded();
        let intent = transfer_intent(30, 1, 7, 8, 500);
        let first = exec.execute(&intent).await.unwrap();
        let second = exec.execute(&intent).await.unwrap();
        assert_eq!(second, first, "a replay returns the recorded outcome");
        assert_eq!(
            exec.item_owner(orrery_protocol::ItemUid::new(1)),
            Some(AccountId::new(8))
        );
        assert_eq!(
            exec.balance(AccountId::new(7), AssetId::new(GOLD)),
            500,
            "the seller was paid once, not twice"
        );
        assert_eq!(exec.balance(AccountId::new(8), AssetId::new(GOLD)), 0);
        assert_eq!(exec.receipts().len(), 1, "one receipt, not two");
    }

    /// Two transfers of the same item, concurrently: one wins, one is refused
    /// with its own reason, and the item has exactly one owner afterwards.
    ///
    /// This tier serializes on a mutex rather than on FDB's resolver, so what
    /// it proves is the *check*, not the conflict range — the `fdb` tier's
    /// `fdb_two_transfers_of_one_item_leave_one_owner` proves the other half.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mem_concurrent_transfers_of_one_item_leave_one_owner() {
        let exec = Arc::new(seeded());
        exec.credit(AccountId::new(9), AssetId::new(GOLD), 500);

        let mut tasks = tokio::task::JoinSet::new();
        for (id, buyer) in [(40u128, 8u64), (41, 9)] {
            let exec = Arc::clone(&exec);
            tasks.spawn(async move { exec.execute(&transfer_intent(id, 1, 7, buyer, 500)).await });
        }
        let mut committed = 0;
        let mut refused = 0;
        while let Some(result) = tasks.join_next().await {
            match result
                .expect("task did not panic")
                .expect("no executor error")
            {
                IntentOutcome::Committed { .. } => committed += 1,
                // Unreachable: `execute` is the attested path, and only
                // `execute_provisional` produces this arm. Named rather than
                // wildcarded so a future change that lets `execute` commit
                // provisionally fails here instead of being counted as a
                // refusal.
                IntentOutcome::Provisional { .. } => {
                    panic!("execute never commits provisionally")
                }
                IntentOutcome::Rejected { reason } => {
                    assert_eq!(
                        reason,
                        orrery_protocol::REASON_NOT_ITEM_OWNER,
                        "the loser re-reads the winner's owner and refuses honestly"
                    );
                    refused += 1;
                }
            }
        }
        assert_eq!((committed, refused), (1, 1), "exactly one transfer commits");

        let owner = exec
            .item_owner(orrery_protocol::ItemUid::new(1))
            .expect("the item still has a row");
        assert_ne!(owner, AccountId::new(7), "the seller divested exactly once");
        assert_eq!(
            exec.balance(AccountId::new(7), AssetId::new(GOLD)),
            500,
            "the seller was paid for one sale, not two"
        );
        assert_eq!(exec.receipts().len(), 1, "one receipt: one trade happened");
    }
}
