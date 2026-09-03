//! The enforcement ramp, measured: D32 clause (e)'s promotion evidence
//! computed from the shadow arm's own observations
//! ([D32](../../../../docs/adr/0032-enforcement-ramp.md)).
//!
//! # The numerator was the easy half
//!
//! [`super::shadow`] emits one [`ShadowObservation`] per intent the control
//! evaluated, and [`super::CountingShadowObserver`] already turns that stream
//! into `evaluated` and `would_act`. That is D32 clause (e)'s `fp_count`
//! numerator and a denominator — but it is the *wrong* denominator, and the
//! difference is the whole reason this module exists:
//!
//! ```text
//! fp_count(H, C, W) = |{o ∈ obs(C, W) : o.subject ∈ H ∧ o.would_act}|
//! coverage(H, C, W) = observed qualifying H activity / total qualifying H activity
//! ```
//!
//! `evaluated` is *observed* H activity. `coverage`'s denominator is **total**
//! qualifying H activity, and the two differ by exactly the population the
//! shadow arm never saw: intents refused upstream of the enforcement switch by
//! clause (b)'s always-on correctness checks, intents handled by a validator
//! posted in `Off` (which "evaluates nothing and therefore calibrates
//! nothing"), and intents whose evaluation degraded to
//! [`ShadowVerdict::Unevaluated`]. Counting those into the numerator is what
//! D32 refuses in as many words — *"a false-positive rate of 0 over a cohort
//! nobody watched is not evidence, it is blindness with a clean conscience"* —
//! and counting them into neither is how a coverage figure silently becomes
//! `1.000` by construction.
//!
//! So this module counts at **two** points, not one:
//!
//! | Counter | Incremented in | Counts |
//! |---|---|---|
//! | `qualifying` | [`super::BaselineIntentValidator::check_at`], first statement | every admission decision the gateway made for that account |
//! | `observed` | [`ShadowObserver::record`] | those that produced an actual verdict about the control's predicate |
//!
//! The two cannot drift together: one is a fact about entering the path and
//! the other a fact about reaching its far end, and the gap between them is
//! the measurement. A meter that read both out of one call site would report
//! `coverage = 1.0` on a gateway that evaluated nothing.
//!
//! # A rate that does not exist is `null`, not zero
//!
//! [`CohortEvidence::coverage`] is an `Option<f64>` and is `None` when the
//! denominator is zero. This is the load-bearing type decision in the file. A
//! report that renders `0 false positives, coverage 0.000` and a report that
//! renders `0 false positives, coverage —` describe opposite situations, and
//! collapsing them into one `f64` is precisely the failure D32's coverage term
//! exists to prevent. `0` would-have-acted out of `10_000` observed is
//! evidence; `0` out of `0` is a control nobody ran.
//!
//! # What this measures and what it cannot
//!
//! The same meter now consumes **C1** attestation observations, **C2**
//! quarantined-intent incidence, **C4** correction evaluations and **C5**
//! strike-ledger filing results. Each mechanism supplies the account before a
//! snapshot joins it to [`HonestCohort`]. **C3** remains absent, for reasons
//! specific enough to check against the tree: its shadow evaluation computes a
//! refusal set (pending intents) plus an annulment set (journaled effects,
//! inverse ops computed), and no seam can produce either. The control has no
//! enforcement machinery to shadow — no posture type, no poller, no evaluation
//! path — so there is no C3 predicate to meter; the verdict seam where C4 and
//! C5 meter has no join to the write machinery that would compute those sets;
//! and the store exposes no per-account journaled-effects surface to size them
//! from. The annulment machinery that does exist — D29's deadline-expiry and
//! spot-replay sweep — is the always-on fail-closed path clause (h) excludes
//! from the ramp, so an annulment-shaped count would fabricate C3 evidence
//! rather than measure it.
//!
//! Two dimensions D32 and [#221] ask for are **not** here, and are named rather
//! than approximated:
//!
//! - **`RulesetId`.** Clause (f) scopes the auto-suspend trigger per rule
//!   version only "where the control is verdict-driven — C3, C4, C5"; it says
//!   in the same sentence that "C1 and C2 are protocol-level and suspend
//!   globally". C1 owes no `RulesetId` dimension, and inventing one would be a
//!   column that is always the same value.
//! - **Network-quality bucket.** R-6's early warning is a discrepancy rate
//!   "correlating with peer RTT/loss rather than accounts", and clause (f)
//!   calls the bucket a required dimension. [`ShadowObservation`] carries no
//!   RTT or loss, and neither does [`super::IntentContext`], which is what the
//!   validator is handed — so the bucket cannot be measured from what is
//!   emitted today. Supplying it means the gateway putting connection quality
//!   into the validator's context; until it does, this module reports no
//!   bucket rather than a bucket everything falls into.
//!
//! # Regenerating the committed artifact
//!
//! ```sh
//! cargo test -p orrery_persistd --lib -- --ignored --nocapture emit_ramp_artifact
//! ```
//!
//! writes `docs/data/ramp-shadow-<date>.json`, which
//! `scripts/ramp-report.py` reads. The traffic behind it is a harness, not a
//! fleet, and the artifact says so in its own `provenance` block — a
//! production promotion note needs a production run, and clause (e)'s `W ≥ 30
//! days` term is the one no harness can supply.
//!
//! [#221]: https://github.com/baadc0de/orrery/issues/221

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use orrery_protocol::AccountId;
use serde::{Deserialize, Serialize};

use super::shadow::{
    QuarantineValidationObservation, QuarantineValidationObserver, ShadowObservation,
    ShadowObserver, ShadowVerdict, ATTESTATION_QUORUM_CONTROL,
};

/// The schema string every artifact this module writes carries.
///
/// Versioned from the first write, because `scripts/ramp-report.py` reads it
/// and a reader that guesses at a shape it was not written for reports numbers
/// that are wrong rather than absent.
pub const RAMP_ARTIFACT_SCHEMA: &str = "orrery.ramp.report/1";

/// D32's three cached enforcement postures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RampMode {
    /// Do not evaluate the control.
    Off,
    /// Evaluate and measure every would-be action, but suppress all actions.
    Shadow,
    /// Evaluate and perform the control's actions.
    Live,
}

/// Who last selected a durable enforcement posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostureSource {
    /// The startup default; normally represented by an absent row.
    Default,
    /// An authenticated operator-plane write.
    Operator,
    /// D32's verdict-rate circuit breaker.
    AutoSuspend,
}

/// The value stored at D32's `ramp/{control}` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RampPosture {
    /// Current mode.
    pub mode: RampMode,
    /// Writer class that selected the mode.
    pub source: PostureSource,
    /// Unix timestamp at which it was selected.
    pub set_at_ms: u64,
    /// Human-readable reason, limited to 256 bytes by writers.
    pub reason: String,
    /// Auto-suspend incident handle; cleared by an operator write.
    pub incident_id: Option<[u8; 16]>,
}

impl RampPosture {
    /// Whether this row is one its claimed writer was permitted to write.
    ///
    /// **Every poller must call this before applying a row.** D32 clause (f)
    /// says automation *"may make the fleet safer without asking, never less
    /// safe"*, and this is where that stops being a property of the code that
    /// writes and becomes a property of the code that reads.
    ///
    /// The rule is one line: a row whose `source` is
    /// [`PostureSource::AutoSuspend`] is admissible only at
    /// [`RampMode::Shadow`].
    ///
    /// - `AutoSuspend` + `Live` is a **promotion by automation**, which clause
    ///   (e)'s review gate exists to prevent.
    /// - `AutoSuspend` + `Off` is the censorship lever the record names
    ///   explicitly: *"induce spikes, blind the cluster"*. It is refused as
    ///   hard as a promotion, and for a better reason — shadow keeps observing,
    ///   and the incident is the calibration data.
    ///
    /// # Why the check lives here and not at the writer
    ///
    /// A writer-side check authenticates the API, not the row —
    /// [#932](https://github.com/baadc0de/orrery/pull/932) demonstrated
    /// exactly this against a real cluster by verifying an envelope and then
    /// writing the plain row anyway. Placing the constraint at the reader
    /// means it holds against a monitor that has been compromised, a future
    /// writer that forgets it, and a raw FoundationDB write by hand: the fleet
    /// refuses the row because of what it *says*, not because of who sent it.
    ///
    /// This is independent of #932's authenticator and does not wait on it. A
    /// signature answers "who wrote this"; this answers "was anyone allowed to
    /// write this at all", and for the demote-only source the second question
    /// is the one that bounds the damage — a *forged* demotion is still only a
    /// demotion, which clause (f) permits without asking.
    ///
    /// Operator rows are unconstrained here by design: promotion is an
    /// operator act, and authenticating *that* writer is open question 1's
    /// business, not this predicate's.
    #[must_use]
    pub const fn admissible(&self) -> bool {
        match self.source {
            PostureSource::AutoSuspend => matches!(self.mode, RampMode::Shadow),
            PostureSource::Default | PostureSource::Operator => true,
        }
    }

    /// [`Self::admissible`], plus the part that needs to know what the control
    /// is doing right now.
    ///
    /// An automation row must *strictly lower the acting rank*: it may only
    /// take a control that is acting and stop it. [`Self::admissible`] cannot
    /// see the current mode, so it enforces the half that is a property of the
    /// row alone; this adds the half that is a property of the transition.
    ///
    /// The two are both required and neither implies the other, which is the
    /// reason they are written as a conjunction rather than folded into one
    /// rank comparison:
    ///
    /// - A rank comparison alone admits `AutoSuspend` → `Off` from `Live`,
    ///   because `off` does rank below `live`. Clause (f) forbids exactly that
    ///   — *"Fallback is shadow, never off"* — because blinding the cluster
    ///   during the incident that tripped the breaker is a denial-of-service
    ///   lever against enforcement, not an abundance of caution.
    /// - [`Self::admissible`] alone admits `AutoSuspend` → `Shadow` from
    ///   `Off`, which raises the rank: automation starting an evaluation an
    ///   operator switched off.
    ///
    /// Only the conjunction is clause (f).
    #[must_use]
    pub const fn admissible_from(&self, current: RampMode) -> bool {
        if !self.admissible() {
            return false;
        }
        match self.source {
            PostureSource::AutoSuspend => self.mode.rank() < current.rank(),
            PostureSource::Default | PostureSource::Operator => true,
        }
    }
}

impl RampMode {
    /// Hardening rank: how much the control acts. Higher acts more.
    ///
    /// This is emphatically **not** "safer" — D32's default table makes C2 ship
    /// `live`, so lowering C2's rank de-hardens the fleet while lowering C1's
    /// merely stops an experiment. Rank orders *action*, and the safety
    /// question is answered per control against that table, not by this
    /// function.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Shadow => 1,
            Self::Live => 2,
        }
    }
}

/// The row a poller may act on, or `None` for one no writer was permitted to
/// produce.
///
/// **Every posture poller reads through this.** It is the single seam where
/// [`RampPosture::admissible`] is applied, so a new constraint on who may write
/// what — [#932]'s accepted operator-signature check among them — reaches C1,
/// C4 and C5 by being added to `admissible`, not by editing three pollers and
/// hoping the fourth remembers.
///
/// An inadmissible row is treated exactly as an **absent** one: the control
/// falls back to its startup default. That is the conservative direction in
/// both senses — a forged promotion is refused rather than obeyed, and the
/// fallback is a value an operator chose at launch rather than one a writer
/// asserted. The refusal is logged loudly, because a row that reached the
/// store while being unwritable is either a bug in a writer or someone with
/// cluster access, and both deserve an operator's attention.
///
/// [#932]: https://github.com/baadc0de/orrery/pull/932
#[must_use]
pub fn admitted<'row>(row: Option<&'row RampPosture>, control: &str) -> Option<&'row RampPosture> {
    let row = row?;
    if row.admissible() {
        return Some(row);
    }
    tracing::error!(
        control,
        mode = ?row.mode,
        source = ?row.source,
        set_at_ms = row.set_at_ms,
        "refusing a durable ramp posture its claimed writer was not permitted          to write; falling back to this control's startup default"
    );
    None
}

/// Failure reading a durable ramp posture.
#[derive(Debug)]
pub struct RampPostureError(pub String);

impl std::fmt::Display for RampPostureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RampPostureError {}

/// Read-only access to D32's durable posture rows.
///
/// The operator-plane writer is deliberately not part of this interface:
/// authenticating posture writes is D32 open question 1. Production readers
/// use [`FdbRampPostureStore`], while poller tests provide an in-memory reader
/// and never acquire a production write capability.
#[async_trait::async_trait]
pub trait RampPostureReader: Send + Sync {
    /// Read one control's posture. An absent row means the startup default.
    async fn read(&self, control: &str) -> Result<Option<RampPosture>, RampPostureError>;
}

/// A process-shared durable posture reader.
pub type SharedRampPostureReader = std::sync::Arc<dyn RampPostureReader>;

/// FoundationDB adapter for D32's rarely-written, one-second-polled rows.
///
/// # Authentication happens here because this is where the bytes are
///
/// [`admitted`] is the seam every poller reads through, and it stays that way.
/// It cannot do clause (i)'s job, and the reason is structural rather than a
/// matter of preference: `admitted` takes a [`RampPosture`], and a
/// `RampPosture` has no signature, no signer and no expiry. Those live in the
/// [`super::posture::SignedRampPosture`] envelope, which exists only between
/// the FoundationDB `get` and the postcard decode — inside this method and
/// nowhere else. By the time a poller holds a row, the authenticator is gone.
///
/// So the two checks are one pipeline at two altitudes, not two competing
/// seams:
///
/// | Layer | Question | Needs |
/// |---|---|---|
/// | `FdbRampPostureStore::read` | *who wrote this?* — signature, signer, control binding, expiry, C2's closed arm | the stored bytes |
/// | [`admitted`] / [`RampPosture::admissible`] | *was anyone allowed to write this at all?* | the decoded row |
/// | [`RampPosture::admissible_from`] | *is this a legal transition from what we are doing now?* | the poller's acting mode |
///
/// Each is strictly narrower than the last and each refuses independently. A
/// row must clear all three, and a row that clears authentication still faces
/// `admitted` at the poller — which is why deleting either layer is a real
/// regression rather than a tidy-up.
///
/// **A refused row is reported as an absent one**, which is
/// [`admitted`]'s landed convention and now this method's too. The control
/// falls back to the startup default an operator chose at launch, rather than
/// to any value a writer asserted. See [`super::posture::PostureVerdict`] for
/// why that beats "fall to shadow".
#[cfg(feature = "fdb")]
pub struct FdbRampPostureStore {
    db: std::sync::Arc<foundationdb::Database>,
    operator_keys: Vec<orrery_protocol::NodeId>,
}

#[cfg(feature = "fdb")]
impl FdbRampPostureStore {
    /// Construct from the process-scoped FDB context, trusting no operator key.
    ///
    /// The empty set is the safe default and not a placeholder: a process
    /// configured with no `--operator-key` refuses every operator row it finds,
    /// which is the correct posture for a process that has been told about no
    /// operator. Add keys with [`Self::with_operator_keys`].
    #[must_use]
    pub fn from_context(context: &crate::FdbContext) -> Self {
        Self {
            db: context.database(),
            operator_keys: Vec::new(),
        }
    }

    /// Trust this `--operator-key` set for clause (i) verification.
    #[must_use]
    pub fn with_operator_keys(
        mut self,
        keys: impl IntoIterator<Item = orrery_protocol::NodeId>,
    ) -> Self {
        self.operator_keys = keys.into_iter().collect();
        self
    }

    /// Read one control's posture, verified per D32 clause (i).
    ///
    /// See the type docs for the three outcomes. Wall clock is read here
    /// because the expiry rule is about the fleet's time, not the row's; the
    /// clock-injected form is [`super::posture::verdict`], which every unit
    /// test uses.
    ///
    /// # Errors
    ///
    /// Only a FoundationDB transaction failure. A row this process refuses is
    /// **not** an error — an error makes a poller retain its last known mode,
    /// which is exactly the "retaining the unverified mode" clause (i) forbids.
    pub async fn read(&self, control: &str) -> Result<Option<RampPosture>, RampPostureError> {
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_key(control);
        let value: Option<Vec<u8>> = db
            .run(move |transaction, _| {
                let key = key.clone();
                async move {
                    Ok(transaction
                        .get(&key, false)
                        .await?
                        .map(|bytes| bytes.as_ref().to_vec()))
                }
            })
            .await
            .map_err(|error: foundationdb::FdbBindingError| {
                RampPostureError(format!("read ramp posture transaction: {error}"))
            })?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        Ok(
            match super::posture::verdict(control, value.as_deref(), &self.operator_keys, now_ms) {
                super::posture::PostureVerdict::StartupDefault => None,
                super::posture::PostureVerdict::Admitted(posture) => Some(posture),
                super::posture::PostureVerdict::Refused(refusal) => {
                    // `error`, and the same shape `admitted` logs in: a row that
                    // reached FoundationDB and was refused is an incident, and
                    // the two refusals should read alike in a log because they
                    // are two halves of one rule.
                    tracing::error!(
                        control,
                        %refusal,
                        "refusing a durable ramp posture that failed D32 clause (i) \
                         verification; falling back to this control's startup default"
                    );
                    None
                }
            },
        )
    }

    /// Write one already-signed posture row.
    ///
    /// This takes a [`super::posture::SignedRampPosture`] and not a
    /// [`RampPosture`], and there is no overload that takes the latter: the
    /// type system is where "an unauthenticated posture write does not exist"
    /// is stated. Signing is [`super::posture::sign_posture`], which needs the
    /// operator secret this process does not hold — `persistd` links this
    /// method and can never call it usefully.
    ///
    /// # Errors
    ///
    /// A FoundationDB transaction failure, or a postcard encoding failure.
    pub async fn write(
        &self,
        control: &str,
        row: &super::posture::SignedRampPosture,
    ) -> Result<(), RampPostureError> {
        let value = super::posture::encode(row)
            .map_err(|error| RampPostureError(format!("encode ramp posture: {error}")))?;
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_key(control);
        db.run(move |transaction, _| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                transaction.set(&key, &value);
                Ok(())
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampPostureError(format!("write ramp posture transaction: {error}"))
        })
    }

    /// Remove one control's posture row, restoring the CLI startup default.
    ///
    /// # Errors
    ///
    /// A FoundationDB transaction failure.
    pub async fn clear(&self, control: &str) -> Result<(), RampPostureError> {
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_key(control);
        db.run(move |transaction, _| {
            let key = key.clone();
            async move {
                transaction.clear(&key);
                Ok(())
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampPostureError(format!("clear ramp posture transaction: {error}"))
        })
    }
}

#[cfg(feature = "fdb")]
#[async_trait::async_trait]
impl RampPostureReader for FdbRampPostureStore {
    async fn read(&self, control: &str) -> Result<Option<RampPosture>, RampPostureError> {
        Self::read(self, control).await
    }
}

/// How many distinct accounts a [`RampMeter`] keeps individual tallies for
/// before folding the rest into one truncation bucket.
///
/// Bounded for the reason [`super::ShadowObservationLog`] is bounded: the meter
/// is reachable from the admission path and a map keyed by an attacker-chosen
/// account id is a memory leak with a shadow period behind it. The bound is
/// high enough that a real known-honest cohort (D32 wants `|H| ≥ 100`) never
/// approaches it, and the overflow is *reported* rather than absorbed —
/// [`RampSnapshot::accounts_truncated`] is nonzero exactly when account spread
/// and the cohort denominator are both understated, and the report script
/// refuses an artifact that carries one.
pub const DEFAULT_ACCOUNT_CAPACITY: usize = 100_000;

/// D32 clause (e)'s known-honest cohort `H`, with the two halves the record
/// names kept apart.
///
/// The split is not decoration. *Armed-honest* accounts are operator-driven
/// automation — the control P4's swarm harness already runs — and their
/// honesty is a property of the harness. *Natural* accounts are real players
/// past the 7-day probation with a clean archive, "sampled by a human into the
/// cohort", and their honesty is a recorded judgement. A promotion reviewer
/// reading `|H| = 100` needs to know whether that is a hundred bots, because a
/// cohort of pure automation exercises the traffic the automation was written
/// to produce and nothing else.
///
/// Membership is an **input**. Nothing in this module infers it: D32 requires
/// it be "derivable from durable facts plus a recorded sample decision — never
/// from 'seemed fine'", and a meter that promoted quiet accounts into `H`
/// would be scoring its own homework.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HonestCohort {
    /// Operator-controlled accounts acting honestly under automation.
    pub armed: BTreeSet<AccountId>,
    /// Accounts past probation with no upheld adverse finding, sampled in by a
    /// human.
    pub natural: BTreeSet<AccountId>,
}

impl HonestCohort {
    /// An empty cohort.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an armed-honest member.
    pub fn arm(&mut self, account: AccountId) {
        self.armed.insert(account);
    }

    /// Add a naturally-honest member.
    pub fn sample(&mut self, account: AccountId) {
        self.natural.insert(account);
    }

    /// Whether `account` is in `H`.
    #[must_use]
    pub fn contains(&self, account: AccountId) -> bool {
        self.armed.contains(&account) || self.natural.contains(&account)
    }

    /// `|H|` — the union, so an account sampled into both halves is one
    /// member.
    #[must_use]
    pub fn len(&self) -> usize {
        self.armed.union(&self.natural).count()
    }

    /// Whether `H` is empty, which makes every rate over it meaningless.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.armed.is_empty() && self.natural.is_empty()
    }
}

/// What one account did, from both counting points.
#[derive(Debug, Clone, Default)]
struct AccountTally {
    /// Admission decisions made for this account.
    qualifying: u64,
    /// Of those, the ones the control actually produced a verdict for.
    observed: u64,
    /// Of the recorded ones, the ones that produced no verdict at all.
    unevaluated: u64,
    /// Of the observed ones, the ones live mode would have acted on.
    would_act: u64,
    /// Would-be actions by [`super::RejectionCause::as_str`], never a parallel
    /// vocabulary (D32 clause (b)).
    causes: BTreeMap<&'static str, u64>,
}

impl AccountTally {
    fn fold(&mut self, other: &Self) {
        self.qualifying += other.qualifying;
        self.observed += other.observed;
        self.unevaluated += other.unevaluated;
        self.would_act += other.would_act;
        for (cause, count) in &other.causes {
            *self.causes.entry(cause).or_default() += count;
        }
    }
}

#[derive(Debug, Default)]
struct Tallies {
    per_account: BTreeMap<AccountId, AccountTally>,
    /// Submissions with no session, which D32 clause (e) puts outside `H` by
    /// construction — kept separately rather than dropped, because their size
    /// is a fact about the traffic a reviewer should see.
    unattributed: AccountTally,
    /// Everything past [`RampMeter::capacity`], folded together.
    truncated: AccountTally,
    /// How many distinct accounts landed in `truncated`.
    truncated_accounts: BTreeSet<AccountId>,
    by_verdict: BTreeMap<&'static str, u64>,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
}

/// The per-control counters D32 clause (e)'s predicate and clause (f)'s
/// trigger are computed from.
///
/// One meter per control. It is a [`ShadowObserver`], so
/// [`super::BaselineIntentValidator::shadow_observing`] takes it with no new
/// wiring, and it overrides [`ShadowObserver::record_qualifying`] — the
/// denominator's counting point — which the default implementation ignores.
///
/// # Cost, against D16's budget
///
/// Two `Mutex` acquisitions per intent on the admission path, each holding a
/// `BTreeMap` lookup and a handful of integer adds, and only when a validator
/// was built with an observer at all (the field is an `Option` and the default
/// is `None`). D16 budgets the whole intent commit at a 10 ms p99; this is an
/// uncontended lock and a tree descent of depth `log₂(|accounts|)`. Shadow is
/// a temporary posture paying a temporary tax, which is the same trade D32
/// clause (d) prices for the marked `AttestRow`.
#[derive(Debug)]
pub struct RampMeter {
    control: &'static str,
    capacity: usize,
    tallies: Mutex<Tallies>,
}

impl Default for RampMeter {
    fn default() -> Self {
        Self::new(ATTESTATION_QUORUM_CONTROL)
    }
}

impl RampMeter {
    /// A meter for `control`, named as D32 clause (c) names it.
    #[must_use]
    pub fn new(control: &'static str) -> Self {
        Self::with_capacity(control, DEFAULT_ACCOUNT_CAPACITY)
    }

    /// A meter keeping at most `capacity` individual account tallies.
    #[must_use]
    pub fn with_capacity(control: &'static str, capacity: usize) -> Self {
        Self {
            control,
            capacity: capacity.max(1),
            tallies: Mutex::new(Tallies::default()),
        }
    }

    /// The control this meter measures.
    #[must_use]
    pub const fn control(&self) -> &'static str {
        self.control
    }

    /// Count one admission decision — clause (e)'s coverage **denominator**.
    ///
    /// Called once per [`super::BaselineIntentValidator::check_at`], before any
    /// check runs and regardless of the posture, because every one of those
    /// facts is a way for qualifying activity to go unobserved and the point of
    /// the denominator is to see them.
    pub fn qualify(&self, subject: Option<AccountId>) {
        let mut tallies = self.lock();
        Self::entry(&mut tallies, self.capacity, subject).qualifying += 1;
    }

    /// Count C1 qualifying activity through the established observer spelling.
    ///
    /// This inherent method keeps direct callers unambiguous now that the same
    /// meter also implements C2's observer trait.
    pub fn record_qualifying(&self, subject: Option<AccountId>) {
        self.qualify(subject);
    }

    /// Record one C1 observation through the established observer spelling.
    pub fn record(&self, observation: ShadowObservation) {
        <Self as ShadowObserver>::record(self, observation);
    }

    /// Record one completed control evaluation.
    ///
    /// `verdict` is the exhaustive outcome label. `action` is `Some` only
    /// when live mode would act; for C1 it is the stable rejection-cause label,
    /// while controls without a [`super::RejectionCause`] use the stable action
    /// name D32 clause (d) assigns them.
    pub fn observe(
        &self,
        subject: Option<AccountId>,
        verdict: &'static str,
        action: Option<&'static str>,
        observed_at_ms: u64,
    ) {
        let capacity = self.capacity;
        let mut tallies = self.lock();
        Self::record_time_and_verdict(&mut tallies, verdict, observed_at_ms);
        let tally = Self::entry(&mut tallies, capacity, subject);
        tally.observed += 1;
        if let Some(action) = action {
            tally.would_act += 1;
            *tally.causes.entry(action).or_default() += 1;
        }
    }

    /// Record an evaluation which could not produce a verdict.
    ///
    /// It is present in the artifact but absent from coverage's numerator,
    /// matching D32 clause (b)'s fail-open shadow rule.
    pub fn observe_unevaluated(
        &self,
        subject: Option<AccountId>,
        reason: &'static str,
        observed_at_ms: u64,
    ) {
        let capacity = self.capacity;
        let mut tallies = self.lock();
        Self::record_time_and_verdict(&mut tallies, reason, observed_at_ms);
        Self::entry(&mut tallies, capacity, subject).unevaluated += 1;
    }

    fn record_time_and_verdict(tallies: &mut Tallies, verdict: &'static str, observed_at_ms: u64) {
        tallies.first_ms = Some(
            tallies
                .first_ms
                .map_or(observed_at_ms, |first| first.min(observed_at_ms)),
        );
        tallies.last_ms = Some(
            tallies
                .last_ms
                .map_or(observed_at_ms, |last| last.max(observed_at_ms)),
        );
        *tallies.by_verdict.entry(verdict).or_default() += 1;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tallies> {
        self.tallies.lock().expect("ramp meter poisoned")
    }

    /// The tally `subject` belongs in, folding past-capacity accounts together.
    fn entry(
        tallies: &mut Tallies,
        capacity: usize,
        subject: Option<AccountId>,
    ) -> &mut AccountTally {
        let Some(account) = subject else {
            return &mut tallies.unattributed;
        };
        if tallies.per_account.contains_key(&account) {
            return tallies
                .per_account
                .get_mut(&account)
                .expect("just checked present");
        }
        if tallies.per_account.len() >= capacity {
            tallies.truncated_accounts.insert(account);
            return &mut tallies.truncated;
        }
        tallies.per_account.entry(account).or_default()
    }

    /// Freeze the counters into D32 clause (e)'s terms, computed against `H`.
    ///
    /// Every gate figure the report renders is computed here and nowhere else:
    /// `scripts/ramp-report.py` compares them against clause (e)'s floors and
    /// re-derives none of them, for the reason `AGENTS.md` gives about
    /// `gate-status.sh` — "a figure this script computed itself would be a
    /// second implementation of the gate, and the two would disagree exactly
    /// when it mattered."
    #[must_use]
    pub fn snapshot(&self, cohort: &HonestCohort) -> RampSnapshot {
        let tallies = self.lock();

        let mut all = AccountTally::default();
        let mut honest = AccountTally::default();
        let mut accounts_qualifying = 0_u64;
        let mut accounts_observed = 0_u64;
        let mut accounts_would_act = 0_u64;
        let mut honest_active = 0_u64;
        let mut honest_accounts_would_act = 0_u64;

        for (account, tally) in &tallies.per_account {
            all.fold(tally);
            if tally.qualifying > 0 {
                accounts_qualifying += 1;
            }
            if tally.observed > 0 {
                accounts_observed += 1;
            }
            if tally.would_act > 0 {
                accounts_would_act += 1;
            }
            if cohort.contains(*account) {
                honest.fold(tally);
                if tally.qualifying > 0 {
                    honest_active += 1;
                }
                if tally.would_act > 0 {
                    honest_accounts_would_act += 1;
                }
            }
        }
        // The truncation bucket joins the fleet-wide totals and is deliberately
        // kept out of the cohort's: its accounts are not individually known, so
        // attributing any of it to `H` would be a guess, and attributing none
        // of it silently deflates the denominator. Reporting
        // `accounts_truncated` is the honest third option, and the report
        // script refuses an artifact with a nonzero one.
        all.fold(&tallies.truncated);

        let coverage = if honest.qualifying == 0 {
            None
        } else {
            // `f64::from` on a u64 does not exist for a reason; both counts are
            // event counts and exceeding 2^53 of them is not a shadow period.
            #[allow(clippy::cast_precision_loss)]
            Some(honest.observed as f64 / honest.qualifying as f64)
        };

        let first = tallies.first_ms.unwrap_or_default();
        let last = tallies.last_ms.unwrap_or_default();
        #[allow(clippy::cast_precision_loss)]
        let window_days = (last.saturating_sub(first)) as f64 / 86_400_000.0;

        RampSnapshot {
            control: self.control.to_owned(),
            observed_from_ms: first,
            observed_to_ms: last,
            window_days,
            qualifying: all.qualifying,
            observed: all.observed,
            unevaluated: all.unevaluated,
            would_act: all.would_act,
            accounts_qualifying,
            accounts_observed,
            accounts_would_act,
            accounts_truncated: tallies.truncated_accounts.len() as u64,
            unattributed: UnattributedTally {
                qualifying: tallies.unattributed.qualifying,
                observed: tallies.unattributed.observed,
                would_act: tallies.unattributed.would_act,
            },
            by_verdict: string_keys(&tallies.by_verdict),
            by_cause: string_keys(&all.causes),
            cohort: CohortEvidence {
                armed: cohort.armed.len() as u64,
                natural: cohort.natural.len() as u64,
                size: cohort.len() as u64,
                active: honest_active,
                qualifying: honest.qualifying,
                observed: honest.observed,
                unevaluated: honest.unevaluated,
                coverage,
                fp_count: honest.would_act,
                accounts_would_act: honest_accounts_would_act,
                by_cause: string_keys(&honest.causes),
            },
        }
    }
}

fn string_keys(counts: &BTreeMap<&'static str, u64>) -> BTreeMap<String, u64> {
    counts
        .iter()
        .map(|(label, count)| ((*label).to_owned(), *count))
        .collect()
}

impl ShadowObserver for RampMeter {
    fn record(&self, observation: ShadowObservation) {
        // A degraded evaluation is *recorded* and is not *observed*, and the
        // split is the one D32 clause (b) draws when it refuses to let a
        // misconfiguration become a would-be refusal. Folding it into
        // `observed` would let a gateway with no epoch authority report full
        // coverage of a predicate it never ran.
        if matches!(observation.verdict, ShadowVerdict::Unevaluated(_)) {
            self.observe_unevaluated(
                observation.subject,
                observation.verdict.as_str(),
                observation.observed_at_ms,
            );
            return;
        }
        self.observe(
            observation.subject,
            observation.verdict.as_str(),
            observation.verdict.cause().map(|cause| cause.as_str()),
            observation.observed_at_ms,
        );
    }

    fn record_qualifying(&self, subject: Option<AccountId>) {
        self.qualify(subject);
    }
}

impl QuarantineValidationObserver for RampMeter {
    fn record(&self, observation: QuarantineValidationObservation) {
        self.observe(
            observation.subject,
            "would_force_full_validation",
            Some("force_full_validation"),
            observation.observed_at_ms,
        );
    }

    fn record_qualifying(&self, subject: Option<AccountId>) {
        self.qualify(subject);
    }
}

/// Submissions on a connection with no established session.
///
/// D32 clause (e)'s `H` is a set of accounts, so these are outside it by
/// construction and can never be a false positive against it. They are still
/// traffic, and a shadow period in which most of the population is
/// unattributed is a fact about the measurement rather than about the control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnattributedTally {
    /// Admission decisions.
    pub qualifying: u64,
    /// Of those, the ones the control produced a verdict for.
    pub observed: u64,
    /// Of those, the ones live mode would have acted on.
    pub would_act: u64,
}

/// D32 clause (e)'s terms over the known-honest cohort `H`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CohortEvidence {
    /// `|H_armed|`.
    pub armed: u64,
    /// `|H_natural|`.
    pub natural: u64,
    /// `|H|`, the union — clause (e)'s `|H| ≥ 100` term.
    pub size: u64,
    /// Members of `H` that produced any qualifying activity at all.
    ///
    /// Not the same number as [`Self::size`], and the difference is the one a
    /// promotion review has to see: a hundred sampled accounts of which four
    /// were online is a cohort of four.
    pub active: u64,
    /// Total qualifying `H` activity — coverage's denominator.
    pub qualifying: u64,
    /// Observed qualifying `H` activity — coverage's numerator.
    pub observed: u64,
    /// Recorded-but-unevaluated `H` activity, which is neither.
    pub unevaluated: u64,
    /// `coverage(H, C, W)`, or `None` when the denominator is zero.
    ///
    /// See the module docs: a rate that does not exist is not `0.0` and is not
    /// `1.0`.
    pub coverage: Option<f64>,
    /// `fp_count(H, C, W)` — clause (e)'s `= 0` term.
    pub fp_count: u64,
    /// Distinct `H` accounts with at least one would-have-acted event.
    pub accounts_would_act: u64,
    /// The would-be actions by rejection-cause label.
    pub by_cause: BTreeMap<String, u64>,
}

/// One control's measurement over one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RampSnapshot {
    /// D32 clause (c)'s control name — the `ramp/{control}` row's suffix.
    pub control: String,
    /// Earliest observation timestamp, in the clock `check_at` was called with.
    pub observed_from_ms: u64,
    /// Latest observation timestamp.
    pub observed_to_ms: u64,
    /// `W`, in days — clause (e)'s `W ≥ 30 days` term.
    pub window_days: f64,
    /// Admission decisions, fleet-wide.
    pub qualifying: u64,
    /// Of those, the ones the control produced a verdict for.
    pub observed: u64,
    /// Recorded evaluations that produced no verdict (clause (b)'s degraded
    /// arm).
    pub unevaluated: u64,
    /// Of the observed ones, the ones live mode would have acted on.
    pub would_act: u64,
    /// Distinct accounts with any qualifying activity.
    pub accounts_qualifying: u64,
    /// Distinct accounts with any observation.
    pub accounts_observed: u64,
    /// Distinct accounts with a would-have-acted event — clause (f)'s
    /// `spread`, which is cardinality rather than volume because
    /// docs/07:237's alarm is "across unrelated accounts" and an event counter
    /// cannot answer it.
    pub accounts_would_act: u64,
    /// Distinct accounts folded into the truncation bucket. Nonzero means
    /// account spread and the cohort denominator are both understated.
    pub accounts_truncated: u64,
    /// Submissions with no account.
    pub unattributed: UnattributedTally,
    /// Every recorded verdict by its label, admitting ones included: the
    /// outcome split, exhaustive.
    pub by_verdict: BTreeMap<String, u64>,
    /// Would-be actions by stable action label, fleet-wide. C1 uses
    /// [`super::RejectionCause::as_str`] verbatim.
    pub by_cause: BTreeMap<String, u64>,
    /// The same, restricted to `H`.
    pub cohort: CohortEvidence,
}

/// A control D32 names that this tree cannot measure yet, and why.
///
/// Present so the artifact enumerates all five of clause (c)'s controls. An
/// absent control that simply did not appear in the report would read as a
/// control with nothing to report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsentControl {
    /// The `ramp/{control}` suffix.
    pub control: String,
    /// Why nothing is measured for it.
    pub reason: String,
}

/// Where an artifact's numbers came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// `harness` or `production`. Clause (e)'s production leg requires the
    /// second, and the report script says so out loud when it reads the first.
    pub traffic: String,
    /// What produced the run, in one line.
    pub source: String,
    /// Anything a reader needs in order not to over-read the numbers.
    pub note: String,
}

/// The whole artifact `scripts/ramp-report.py` reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RampArtifact {
    /// [`RAMP_ARTIFACT_SCHEMA`].
    pub schema: String,
    /// Where the numbers came from.
    pub provenance: Provenance,
    /// One entry per measurable control.
    pub controls: Vec<RampSnapshot>,
    /// The D32 controls this tree cannot measure.
    pub absent: Vec<AbsentControl>,
}

impl RampArtifact {
    /// An artifact carrying `controls`, with absent controls filled in from
    /// D32 clause (c) and (d).
    #[must_use]
    pub fn new(provenance: Provenance, controls: Vec<RampSnapshot>) -> Self {
        Self {
            schema: RAMP_ARTIFACT_SCHEMA.to_owned(),
            provenance,
            controls,
            absent: absent_controls(),
        }
    }

    /// The artifact as pretty JSON, newline-terminated.
    ///
    /// # Errors
    ///
    /// Propagates any `serde_json` failure.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut rendered = serde_json::to_string_pretty(self)?;
        rendered.push('\n');
        Ok(rendered)
    }
}

/// The controls D32 clause (c) that this tree still cannot measure, with the
/// reason the missing mechanism prevents a cohort-restricted snapshot.
#[must_use]
pub fn absent_controls() -> Vec<AbsentControl> {
    [(
        "write_annulment",
        "D32 clause (d): C3's shadow evaluation computes a refusal set (pending intents) \
             plus an annulment set (journaled effects, inverse ops computed), and no seam \
             can produce either, so neither qualifying honest activity nor would-be \
             annulments can be measured. The control has no enforcement machinery to \
             shadow — no posture type, no poller, no evaluation path; the verdict seam \
             where C4 and C5 meter has no join to the write machinery that would compute \
             those sets; and the store exposes no per-account journaled-effects surface \
             to size them from. D29's annulment sweep is the always-on fail-closed path \
             clause (h) excludes from the ramp, so an annulment-shaped count would \
             fabricate C3 evidence rather than measure it.",
    )]
    .into_iter()
    .map(|(control, reason)| AbsentControl {
        control: control.to_owned(),
        reason: reason.to_owned(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::RejectionCause;
    use orrery_protocol::{CellEpoch, NodeId};

    use crate::intent::NetworkQuality;

    fn node() -> NodeId {
        iroh_base::SecretKey::from_bytes(&[5; 32]).public()
    }

    fn obs(subject: Option<u64>, verdict: ShadowVerdict, at_ms: u64) -> ShadowObservation {
        ShadowObservation {
            intent_id: u128::from(at_ms),
            issuer: node(),
            subject: subject.map(AccountId::new),
            cell_epoch: CellEpoch::new(9),
            verdict,
            observed_at_ms: at_ms,
            network: NetworkQuality::Unknown,
        }
    }

    fn cohort_of(members: impl IntoIterator<Item = u64>) -> HonestCohort {
        let mut cohort = HonestCohort::new();
        for member in members {
            cohort.arm(AccountId::new(member));
        }
        cohort
    }

    /// The distinction D32 clause (e) is written to force, at the type level.
    ///
    /// `0` would-have-acted out of `10_000` observed and `0` out of `0` both
    /// report `fp_count = 0`. The only thing separating them is the coverage
    /// term, and it separates them as *absent* rather than as a small number —
    /// there is no `f64` a reader could mistake for a measurement.
    #[test]
    fn zero_of_ten_thousand_and_zero_of_zero_do_not_look_alike() {
        let cohort = cohort_of([1]);

        let watched = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        for tick in 0..10_000_u64 {
            watched.record_qualifying(Some(AccountId::new(1)));
            watched.record(obs(Some(1), ShadowVerdict::WouldAdmit, 1_000 + tick));
        }
        let watched = watched.snapshot(&cohort);

        let unwatched = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        let unwatched = unwatched.snapshot(&cohort);

        assert_eq!(watched.cohort.fp_count, 0);
        assert_eq!(unwatched.cohort.fp_count, 0, "the numerators agree");

        assert_eq!(watched.cohort.qualifying, 10_000);
        assert_eq!(watched.cohort.observed, 10_000);
        assert_eq!(watched.cohort.coverage, Some(1.0));

        assert_eq!(unwatched.cohort.qualifying, 0);
        assert_eq!(
            unwatched.cohort.coverage, None,
            "a rate over an empty denominator is absent, not zero and not one — \
             collapsing the two is the failure D32's coverage term exists to prevent"
        );
        assert_ne!(watched.cohort.coverage, unwatched.cohort.coverage);
    }

    /// The denominator counts activity the shadow arm never saw, which is the
    /// only reason it is a second counting point rather than a restatement.
    #[test]
    fn coverage_falls_when_qualifying_activity_goes_unobserved() {
        let cohort = cohort_of([4]);
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);

        // Ten admission decisions; eight reach the shadow arm.
        for _ in 0..10_u64 {
            meter.record_qualifying(Some(AccountId::new(4)));
        }
        for tick in 0..8_u64 {
            meter.record(obs(Some(4), ShadowVerdict::WouldAdmit, 1_000 + tick));
        }

        let snapshot = meter.snapshot(&cohort);
        assert_eq!(snapshot.cohort.qualifying, 10);
        assert_eq!(snapshot.cohort.observed, 8);
        assert_eq!(snapshot.cohort.coverage, Some(0.8));
    }

    /// A degraded evaluation is recorded and is not observed, so it lowers
    /// coverage instead of inflating it — and it is never a false positive.
    #[test]
    fn an_unevaluated_observation_is_recorded_but_not_observed() {
        use crate::intent::ShadowUnevaluated;
        let cohort = cohort_of([4]);
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);

        for tick in 0..4_u64 {
            meter.record_qualifying(Some(AccountId::new(4)));
            meter.record(obs(
                Some(4),
                ShadowVerdict::Unevaluated(ShadowUnevaluated::NoEpochAuthority),
                1_000 + tick,
            ));
        }

        let snapshot = meter.snapshot(&cohort);
        assert_eq!(snapshot.cohort.qualifying, 4);
        assert_eq!(snapshot.cohort.observed, 0);
        assert_eq!(snapshot.cohort.unevaluated, 4);
        assert_eq!(snapshot.cohort.fp_count, 0);
        assert_eq!(
            snapshot.cohort.coverage,
            Some(0.0),
            "a gateway that could not evaluate anything reports zero coverage, \
             not a clean sheet"
        );
    }

    /// Account spread is cardinality, not volume: two events from one account
    /// and two events from two accounts read differently (docs/07:237).
    #[test]
    fn account_spread_counts_accounts_and_not_events() {
        let one = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        let two = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        let refusal = ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet);

        for account in [11_u64, 11] {
            one.record_qualifying(Some(AccountId::new(account)));
            one.record(obs(Some(account), refusal, 1_000));
        }
        for account in [11_u64, 12] {
            two.record_qualifying(Some(AccountId::new(account)));
            two.record(obs(Some(account), refusal, 1_000));
        }

        let cohort = cohort_of([11, 12]);
        let one = one.snapshot(&cohort);
        let two = two.snapshot(&cohort);

        assert_eq!(one.would_act, two.would_act, "the event counts agree");
        assert_eq!(one.accounts_would_act, 1);
        assert_eq!(two.accounts_would_act, 2);
        assert_eq!(one.cohort.accounts_would_act, 1);
        assert_eq!(two.cohort.accounts_would_act, 2);
    }

    /// The cause dimension is the rejection log's own vocabulary, so a shadow
    /// report joins against it with no translation table (D32 clause (b)).
    #[test]
    fn causes_are_keyed_by_the_rejection_causes_own_label() {
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        for (account, cause) in [
            (21_u64, RejectionCause::ThresholdNotMet),
            (21, RejectionCause::ThresholdNotMet),
            (22, RejectionCause::RequiredWitnessMissing),
        ] {
            meter.record_qualifying(Some(AccountId::new(account)));
            meter.record(obs(Some(account), ShadowVerdict::WouldRefuse(cause), 1_000));
        }

        let snapshot = meter.snapshot(&cohort_of([21]));
        assert_eq!(
            snapshot
                .by_cause
                .get(RejectionCause::ThresholdNotMet.as_str()),
            Some(&2)
        );
        assert_eq!(
            snapshot
                .by_cause
                .get(RejectionCause::RequiredWitnessMissing.as_str()),
            Some(&1)
        );
        assert_eq!(
            snapshot.cohort.by_cause.len(),
            1,
            "account 22 is outside H, so its cause is fleet-wide only"
        );
        assert_eq!(snapshot.cohort.fp_count, 2);
    }

    /// An unauthenticated submission is outside `H` and cannot be a false
    /// positive against it, but it is still counted and reported.
    #[test]
    fn an_unattributed_submission_is_outside_the_cohort_and_still_counted() {
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        meter.record_qualifying(None);
        meter.record(obs(
            None,
            ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet),
            1_000,
        ));

        let snapshot = meter.snapshot(&cohort_of([1]));
        assert_eq!(snapshot.unattributed.would_act, 1);
        assert_eq!(snapshot.cohort.fp_count, 0);
        assert_eq!(snapshot.cohort.coverage, None);
        assert_eq!(
            snapshot.accounts_would_act, 0,
            "spread is over accounts, and this submission has none"
        );
    }

    /// Past the cap, accounts fold into one bucket and the artifact says how
    /// many did — an understated denominator that announced itself.
    #[test]
    fn the_account_table_reports_its_own_truncation() {
        let meter = RampMeter::with_capacity(ATTESTATION_QUORUM_CONTROL, 2);
        for account in 1..=5_u64 {
            meter.record_qualifying(Some(AccountId::new(account)));
        }
        let snapshot = meter.snapshot(&HonestCohort::new());
        assert_eq!(snapshot.qualifying, 5, "no event is lost");
        assert_eq!(snapshot.accounts_qualifying, 2);
        assert_eq!(snapshot.accounts_truncated, 3);
    }

    /// `|H|` is the union: an account in both halves is one member.
    #[test]
    fn the_cohort_size_is_the_union_of_its_two_halves() {
        let mut cohort = HonestCohort::new();
        cohort.arm(AccountId::new(1));
        cohort.sample(AccountId::new(1));
        cohort.sample(AccountId::new(2));
        assert_eq!(cohort.armed.len(), 1);
        assert_eq!(cohort.natural.len(), 2);
        assert_eq!(cohort.len(), 2);
    }

    /// A would-be action against *either* half of `H` is a false positive
    /// against it, and one against an account in neither half is not.
    ///
    /// The armed/natural split is the whole point of the cohort: it is what
    /// lets an operator tell "would have refused 40 honest players" from
    /// "would have refused 40 cheats", and the halves are reported separately
    /// so a promotion reviewer can tell a hundred bots from a hundred players.
    /// Every control this module meters scores over the union — a natural
    /// member's would-act is exactly as disqualifying as an armed one — so the
    /// split must survive the snapshot for every control seam, not just the
    /// fixture meter.
    #[test]
    fn a_would_act_by_any_cohort_half_reaches_fp_count() {
        let armed = AccountId::new(31);
        let natural = AccountId::new(32);
        let outside = AccountId::new(39);
        let mut cohort = HonestCohort::new();
        cohort.arm(armed);
        cohort.sample(natural);

        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        let refusal = ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet);
        for (account, at_ms) in [(armed, 1_000), (natural, 1_001), (outside, 1_002)] {
            meter.record_qualifying(Some(account));
            meter.record(obs(Some(account.0), refusal, at_ms));
        }

        let snapshot = meter.snapshot(&cohort);
        assert_eq!(
            snapshot.cohort.armed, 1,
            "the halves are reported separately"
        );
        assert_eq!(snapshot.cohort.natural, 1);
        assert_eq!(snapshot.cohort.size, 2);
        assert_eq!(
            snapshot.cohort.fp_count, 2,
            "the armed member's and the natural member's would-acts both count; \
             the account outside H does not"
        );
        assert_eq!(snapshot.cohort.accounts_would_act, 2);
        assert_eq!(
            snapshot.would_act, 3,
            "the outside account's would-act still counts fleet-wide"
        );
        assert_eq!(snapshot.cohort.coverage, Some(1.0));
    }

    /// The artifact round-trips, and enumerates all five of clause (c)'s
    /// controls between `controls` and `absent`.
    #[test]
    fn the_artifact_round_trips_and_names_every_control() {
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        meter.record_qualifying(Some(AccountId::new(1)));
        meter.record(obs(Some(1), ShadowVerdict::WouldAdmit, 1_000));
        let cohort = cohort_of([1]);
        let artifact = RampArtifact::new(
            Provenance {
                traffic: "harness".to_owned(),
                source: "unit test".to_owned(),
                note: String::new(),
            },
            vec![
                meter.snapshot(&cohort),
                RampMeter::new("quarantine_validation").snapshot(&cohort),
                RampMeter::new("authority_correction").snapshot(&cohort),
                RampMeter::new("strikes").snapshot(&cohort),
            ],
        );

        let json = artifact.to_json().expect("serializable");
        let parsed: RampArtifact = serde_json::from_str(&json).expect("round trip");
        assert_eq!(parsed, artifact);
        assert_eq!(parsed.schema, RAMP_ARTIFACT_SCHEMA);
        assert_eq!(parsed.controls.len() + parsed.absent.len(), 5);
        assert_eq!(parsed.absent, absent_controls());
        assert!(json.contains("\"coverage\": 1.0"));
    }

    #[test]
    fn absent_controls_names_only_c3_and_why_it_cannot_be_measured() {
        let absent = absent_controls();
        assert_eq!(absent.len(), 1);
        assert_eq!(absent[0].control, "write_annulment");
        // The reason is what an operator reads in the artifact, so it carries
        // each specific missing seam rather than a summary: what C3's shadow
        // would compute, why neither half of it is computable today, and why
        // the annulment count that does exist must not stand in for it.
        assert!(absent[0].reason.contains("refusal set"));
        assert!(absent[0].reason.contains("pending intents"));
        assert!(absent[0].reason.contains("annulment set"));
        assert!(absent[0].reason.contains("journaled effects"));
        assert!(absent[0].reason.contains("no posture type"));
        assert!(absent[0].reason.contains("no poller"));
        assert!(absent[0].reason.contains("no evaluation path"));
        assert!(absent[0].reason.contains("verdict seam"));
        assert!(absent[0].reason.contains("per-account journaled-effects"));
        assert!(absent[0].reason.contains("qualifying honest activity"));
        assert!(absent[0].reason.contains("would-be annulments"));
        assert!(absent[0].reason.contains("clause (h)"));
        assert!(absent[0].reason.contains("fabricate C3 evidence"));
    }
}
