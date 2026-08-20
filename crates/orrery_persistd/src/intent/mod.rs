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
    AccountId, AssetId, Intent, IntentOutcome, NodeId, REASON_CONTENTION_EXHAUSTED,
};

#[cfg(feature = "fdb")]
mod fdb;
#[cfg(feature = "fdb")]
pub use fdb::{FdbIntentExecutor, IntentFence};

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
    /// Admit the intent to execution.
    Admit(IntentPrecheck),
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
}

impl IntentContext {
    /// A context for a connection with no established session — the state a
    /// peer is in between the transport handshake and its `Hello` ack.
    #[must_use]
    pub fn unauthenticated(issuer: NodeId) -> Self {
        Self {
            issuer,
            account: None,
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

/// Why an intent failed admission. Never sent on the wire — the wire reason is
/// [`orrery_protocol::REASON_VALIDATION_FAILED`], because a `Ruleset`-defined
/// reason space is the `Ruleset`'s to define — but logged, so an operator
/// reading a rejection rate can tell a malformed client from an attack.
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
/// 4. **Attestation authenticity.** Attestations are not *required* (P5 owes
///    the K-of-N threshold), but a present one must be real: at most
///    [`MAX_ATTESTATIONS`], no repeated witness, and every signature verifies
///    against [`Intent::signing_preimage`]. A forged co-signature is refused
///    here rather than being carried into the ledger's audit trail.
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
/// - **Witness attestation thresholds.** K-of-N and the seeded cell-epoch
///   witness set are P5 (docs/11-roadmap.md §P2). `cell_epoch` is carried,
///   not checked: nothing here knows which witness set it names.
/// - **Rate and quota.** Intent submission is bounded per connection by the
///   gateway's in-flight lane, not by a per-account budget the way reports
///   are.
/// - **Replay.** Handled durably by the `intent/{intent_id}` idempotency row
///   (§7 step 0), not by an admission-time cache.
/// - **The issuer signature and the issuer/connection binding**, which the
///   gateway checks *before* this runs; the validator would be the wrong
///   place to repeat them.
#[derive(Debug, Default, Clone, Copy)]
pub struct BaselineIntentValidator;

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

impl BaselineIntentValidator {
    /// The verdict, with the cause preserved for logging.
    ///
    /// # Errors
    ///
    /// Returns the first [`RejectionCause`] the intent trips, checked cheapest
    /// first so an oversized or malformed submission never pays for signature
    /// verification.
    pub fn check(intent: &Intent, cx: &IntentContext) -> Result<IntentPrecheck, RejectionCause> {
        if intent.ops.is_empty() {
            return Err(RejectionCause::NoOps);
        }
        if intent.ops.len() > MAX_OPS_PER_INTENT {
            return Err(RejectionCause::TooManyOps);
        }
        if intent.attestations.len() > MAX_ATTESTATIONS {
            return Err(RejectionCause::TooManyAttestations);
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

        // Signature work last: it is the only non-trivial cost here, and an
        // intent that fails any check above must never pay it.
        if !intent.attestations.is_empty() {
            let preimage = intent.signing_preimage();
            let mut seen: Vec<NodeId> = Vec::with_capacity(intent.attestations.len());
            for attestation in &intent.attestations {
                if seen.contains(&attestation.witness) {
                    return Err(RejectionCause::DuplicateAttestation);
                }
                seen.push(attestation.witness);
                if attestation
                    .witness
                    .verify(&preimage, &attestation.signature)
                    .is_err()
                {
                    return Err(RejectionCause::BadAttestation);
                }
            }
        }

        Ok(IntentPrecheck { read_keys })
    }
}

impl IntentValidator for BaselineIntentValidator {
    fn validate(&self, intent: &Intent, cx: &IntentContext) -> IntentVerdict {
        match Self::check(intent, cx) {
            Ok(precheck) => IntentVerdict::Admit(precheck),
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
                    reason: orrery_protocol::REASON_VALIDATION_FAILED,
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
        /// The row's new value.
        row: crate::keyspace::ItemRow,
    },
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
    /// Whether any op transferred an item, and therefore whether this intent
    /// banks a receipt.
    transferred: bool,
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
        row.owner = t.buyer;
        self.items.insert(t.item, Some(row.clone()));
        self.writes
            .push(PlannedWrite::ItemOwner { item: t.item, row });
        // The debit side keeps its read (the balance check above); the credit
        // side is blind, exactly as §7 specifies. Both are `Add`s here — what
        // makes the debit safe is that it was *checked* against a value read
        // in this same transaction, not that it is written differently.
        self.balance_delta(t.buyer, t.asset, -t.price);
        self.balance_delta(t.seller, t.asset, t.price);
        for party in [t.seller, t.buyer] {
            if !self.parties.contains(&party) {
                self.parties.push(party);
            }
        }
        self.transferred = true;
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
    }

    /// The staged writes, in application order.
    fn writes(&self) -> &[PlannedWrite] {
        &self.writes
    }

    /// The `ledger/receipt/` row this intent banks, or `None` if it moved no
    /// item. A pure credit writes no receipt: the audit trail of §6 is
    /// `(intent_id, parties, ops)` for a *trade*.
    fn receipt(&self, intent: &Intent) -> Option<crate::keyspace::ReceiptRow> {
        self.transferred.then(|| crate::keyspace::ReceiptRow {
            intent_id: intent.intent_id,
            parties: self.parties.clone(),
            ops: intent.ops.iter().map(|op| op.op).collect(),
        })
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
    /// Recorded outcomes by `intent_id` — the `intent/{intent_id}` store.
    outcomes: HashMap<u128, IntentOutcome>,
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

#[async_trait::async_trait]
impl IntentExecutor for MemIntentExecutor {
    async fn execute(&self, intent: &Intent) -> Result<IntentOutcome, IntentError> {
        let mut ledger = self.ledger.lock().expect("mutex");

        // Step 0 (§7): the idempotency row. A replay returns the recorded
        // outcome unchanged.
        if let Some(prev) = ledger.outcomes.get(&intent.intent_id) {
            return Ok(prev.clone());
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

        // Step 3 (§7): the writes, now that every check has passed.
        for write in plan.writes() {
            match write {
                PlannedWrite::BalanceAdd {
                    account,
                    asset,
                    delta,
                } => {
                    *ledger.balances.entry((*account, *asset)).or_insert(0) += i128::from(*delta);
                }
                PlannedWrite::ItemOwner { item, row } => {
                    ledger.items.insert(*item, row.clone());
                }
            }
        }
        if let Some(receipt) = plan.receipt(intent) {
            ledger.receipts.push(receipt);
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

        let outcome = IntentOutcome::Committed { tick, minted };
        ledger.outcomes.insert(intent.intent_id, outcome.clone());
        Ok(outcome)
    }
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

    fn cx(account: Option<u64>) -> IntentContext {
        IntentContext {
            issuer: issuer_key().public(),
            account: account.map(AccountId::new),
        }
    }

    /// An intent carrying exactly `ops`, signed by the same issuer.
    fn intent_with(ops: Vec<IntentOp>) -> Intent {
        let key = issuer_key();
        let mut intent = Intent {
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
            BaselineIntentValidator.validate(&intent, &cx(Some(7))),
            IntentVerdict::Admit(IntentPrecheck { read_keys: vec![] }),
            "an opaque op is admitted: only the envelope is this filter's business"
        );
        // And it does not secretly depend on a session: an opaque op names no
        // account, so there is nothing to bind it to.
        assert!(matches!(
            BaselineIntentValidator.validate(&intent, &cx(None)),
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
        genuine.attestations.push(Attestation {
            witness: witness.public(),
            signature: witness.sign(&base.signing_preimage()),
        });
        assert!(
            BaselineIntentValidator::check(&genuine, &cx(Some(7))).is_ok(),
            "a real co-signature is admitted"
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
