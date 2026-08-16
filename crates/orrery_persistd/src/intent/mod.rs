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

use orrery_protocol::{Intent, IntentOutcome, REASON_CONTENTION_EXHAUSTED};

#[cfg(feature = "fdb")]
mod fdb;
#[cfg(feature = "fdb")]
pub use fdb::{FdbIntentExecutor, IntentFence};

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

/// The synchronous admission seam (D11 §2.2, first stage).
///
/// Runs on the gateway before any executor work; the FDB transaction remains
/// the sole authority — this is only a fast filter (docs/08-persistence.md
/// §2.2: "reject obviously invalid intents without an FDB round trip").
pub trait IntentValidator: Send + Sync {
    /// Validate `intent`, returning the verdict (and, on admit, the durable
    /// keys the executor should read).
    fn validate(&self, intent: &Intent) -> IntentVerdict;
}

/// The permissive default: admits every intent with no read keys.
///
/// This is what lets the harness run unconfigured — the gateway works, the
/// FDB path exercises its idempotency and minting machinery, and a linked
/// `Ruleset` swaps in real validation without touching the wiring.
#[derive(Debug, Default, Clone, Copy)]
pub struct PermissiveValidator;

impl IntentValidator for PermissiveValidator {
    fn validate(&self, _intent: &Intent) -> IntentVerdict {
        IntentVerdict::Admit(IntentPrecheck::default())
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
    use orrery_protocol::{CellEpoch, IntentOp};

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
            v.validate(&intent(1, 0)),
            IntentVerdict::Admit(IntentPrecheck { read_keys: vec![] })
        );
    }
}
