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
/// 2. **The one op this cluster interprets.** [`LEDGER_CREDIT_OP`]'s `args`
///    must be exactly the executor's 24-byte `account ‖ asset ‖ delta`
///    triple. Without this check a malformed credit costs a full FDB round
///    trip and comes back as `REASON_EXECUTOR_ERROR`, which reads as a server
///    fault rather than a bad request.
/// 3. **Account authorization.** A [`LEDGER_CREDIT_OP`] may only name the
///    account whose session token this connection authenticated with. A
///    connection with no session has no account and may not submit one at
///    all. This is the check that keeps the executor's blind
///    `MutationType::Add` from being a "credit anyone" primitive reachable by
///    any peer that can complete a transport handshake.
/// 4. **Attestation authenticity.** Attestations are not *required* (P5 owes
///    the K-of-N threshold), but a present one must be real: at most
///    [`MAX_ATTESTATIONS`], no repeated witness, and every signature verifies
///    against [`Intent::signing_preimage`]. A forged co-signature is refused
///    here rather than being carried into the ledger's audit trail.
///
/// On admit, the precheck names the `ledger/bal/{account}/{asset}` rows the
/// credited ops touch, so a `Ruleset`-linked executor can register their
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
///   any durable row; the FDB transaction remains the sole authority (§2.2),
///   and this cluster's stub executor checks none of them either — a credit
///   admitted here still mints value from nothing. That is a property of the
///   P2 stub executor, not something this filter can repair.
/// - **Any op other than [`LEDGER_CREDIT_OP`].** They are `Ruleset`-opaque by
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
            if op.op != LEDGER_CREDIT_OP {
                continue;
            }
            if op.args.len() != LEDGER_CREDIT_ARGS_BYTES {
                return Err(RejectionCause::MalformedLedgerOp);
            }
            let account = AccountId::new(u64::from_le_bytes(
                op.args[0..8].try_into().expect("slice len"),
            ));
            // A session's account is the only account this connection may
            // move value in. `None` is the unauthenticated case and is
            // refused for the same reason, not a different one.
            if cx.account != Some(account) {
                return Err(RejectionCause::LedgerOpForAnotherAccount);
            }
            let asset = AssetId::new(u64::from_le_bytes(
                op.args[8..16].try_into().expect("slice len"),
            ));
            let key = crate::keyspace::ledger_bal_key(account, asset).to_vec();
            if !read_keys.contains(&key) {
                read_keys.push(key);
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

/// An in-memory [`IntentExecutor`] with the FDB path's observable contract:
/// the idempotency row is honoured (a replay returns the first outcome), ids
/// are minted from a counter, and each op produces one minted id. Used by the
/// gateway tests so the executor path is exercised without a live FDB.
pub struct MemIntentExecutor {
    /// Recorded outcomes by `intent_id` — the idempotency store.
    outcomes: std::sync::Mutex<HashMap<u128, IntentOutcome>>,
    /// The `pid/next` counter analogue.
    next_pid: std::sync::Mutex<u64>,
    /// Tick counter standing in for the FDB commit-version tick.
    next_tick: std::sync::Mutex<u64>,
}

impl MemIntentExecutor {
    /// An empty executor; the first intent commits at tick 1 and mints from
    /// `PersistId` 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            outcomes: std::sync::Mutex::new(HashMap::new()),
            next_pid: std::sync::Mutex::new(1),
            next_tick: std::sync::Mutex::new(0),
        }
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
        // Step 0 (§7): the idempotency row. A replay returns the recorded
        // outcome unchanged.
        if let Some(prev) = self.outcomes.lock().expect("mutex").get(&intent.intent_id) {
            return Ok(prev.clone());
        }
        // Mint one PersistId per op (the harness default; a linked Ruleset
        // decides what an op actually mints).
        let minted = {
            let mut pid = self.next_pid.lock().expect("mutex");
            let start = *pid;
            *pid += intent.ops.len() as u64;
            (start..start + intent.ops.len() as u64)
                .map(orrery_protocol::PersistId::new)
                .collect::<Vec<_>>()
        };
        let tick = {
            let mut t = self.next_tick.lock().expect("mutex");
            *t += 1;
            orrery_protocol::Tick::new(*t)
        };
        let outcome = IntentOutcome::Committed { tick, minted };
        self.outcomes
            .lock()
            .expect("mutex")
            .insert(intent.intent_id, outcome.clone());
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
                    op: i as u16,
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
                op: (i as u16) + 1,
                args: bytes::Bytes::new(),
            })
            .collect();
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(many), &cx(Some(7))),
            Err(RejectionCause::TooManyOps)
        );

        let fat = vec![IntentOp {
            op: 1,
            args: bytes::Bytes::from(vec![0u8; MAX_OP_ARGS_BYTES + 1]),
        }];
        assert_eq!(
            BaselineIntentValidator::check(&intent_with(fat), &cx(Some(7))),
            Err(RejectionCause::ArgsTooLarge)
        );

        // Under the per-op cap, over the whole-intent cap.
        let spread = (0..MAX_OPS_PER_INTENT)
            .map(|i| IntentOp {
                op: (i as u16) + 1,
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
}
