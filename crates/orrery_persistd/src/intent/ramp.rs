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
//! - **`RulesetId` as a per-observation trigger dimension.** Clause (f)
//!   scopes the auto-suspend trigger per rule version only "where the control
//!   is verdict-driven — C3, C4, C5"; it says in the same sentence that "C1
//!   and C2 are protocol-level and suspend globally". C1 owes no `RulesetId`
//!   dimension, and inventing one would be a column that is always the same
//!   value. What *is* here, since the owner decided D32 open question 6 on
//!   2026-09-03, is the window-level stamp: [`RampMeter::observe_ruleset`]
//!   records the rulesets a control's counters saw, the durable window unions
//!   them into one set, and the artifact publishes it — so a window that
//!   spanned a ruleset change says so and a reviewer judges it, instead of
//!   the fleet resetting for them. The per-observation dimension clause (f)'s
//!   trigger needs remains unbuilt.
//! - **Network-quality bucket.** R-6's early warning is a discrepancy rate
//!   "correlating with peer RTT/loss rather than accounts", and clause (f)
//!   calls the bucket a required dimension. [`ShadowObservation`] carries no
//!   RTT or loss, and neither does [`super::IntentContext`], which is what the
//!   validator is handed — so the bucket cannot be measured from what is
//!   emitted today. Supplying it means the gateway putting connection quality
//!   into the validator's context; until it does, this module reports no
//!   bucket rather than a bucket everything falls into.
//!
//! # `W` and the counters outlive the process
//!
//! They did not always. `first_ms`, `last_ms` and every tally were fields of
//! this module's in-process `Mutex<Tallies>`, so a `persistd` restart reset
//! them — and a routine deploy is a restart. Clause (e)'s `W ≥ 30 days` term
//! was therefore unreachable no matter how long a fleet ran, which is not a
//! traffic problem and no amount of production traffic would have fixed it
//! ([#990]).
//!
//! [`super::window`] is the durable form: one `rampw/{control}` row holding
//! the window's bounds and its counters, with clause (e)'s armed/natural split
//! carried through per count. [`RampMeter::take_delta`] drains the live
//! counters into it on a background cadence, [`RampMeter::restore`] reads it
//! back at startup so a restart *continues* the window, and
//! [`RampMeter::snapshot`] reports the sum. `orrery-ramp window show` is the
//! operator's view of the row and `orrery-ramp window reset` is the deliberate
//! way to start a fresh one.
//!
//! # Getting the counters back out
//!
//! Metering them and being able to *read* them are two different landings, and
//! for a while only the first had happened: the meters counted in the deployed
//! composition and nothing in the tree called [`RampMeter::snapshot`] outside
//! a test, so the durable cohort and the durable window could not reach an
//! artifact at all ([#991]). That was a code gap and not a traffic gap.
//!
//! [`super::report`] is the exit. `orrery-ramp report` loads the durable
//! cohort and every control's durable window, replays them into a meter that
//! never metered anything — [`RampMeter::restore`] then
//! [`RampMeter::snapshot`], the same two calls `persistd` makes at startup —
//! and writes a [`RampArtifact`]. It runs in an operator tool and not in the
//! coordinator, which ADR-0031 clause (d) forbids from reading.
//!
//! # The posture changes themselves are history now
//!
//! D32 open question 2 asked what becomes of a superseded `ramp/{control}`
//! row, and until now the answer was *nothing*: the row is current state and
//! every write overwrites it, so "who suspended what, when, why" — the
//! incident history the question argues for keeping — survived nowhere, and
//! an incident review after the fact had the last row only. The durable
//! posture-change history is the shadow: [`FdbRampPostureStore::write`] and
//! [`FdbRampPostureStore::clear`] append a
//! [`super::posture::PostureHistoryRow`] to the `vh/{control}` span in the
//! same transaction that replaces or removes the live row, so a commit that
//! changed the posture without recording the change is not a state the
//! cluster can be in, and the history cannot be forgotten by any writer —
//! there is no other writer seam. [`FdbRampPostureStore::history`] reads it
//! back oldest-first and `orrery-ramp history` renders it for a promotion
//! review. The journal archive remains the record's likely long-term home
//! for this shadow; the span is the bounded form that makes the history
//! durable before that machinery has a posture-shaped event to carry.
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
//! [#990]: https://github.com/baadc0de/orrery/issues/990
//! [#991]: https://github.com/baadc0de/orrery/issues/991

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use orrery_protocol::{AccountId, RulesetId};
use serde::{Deserialize, Serialize};

use super::shadow::{
    QuarantineValidationObservation, QuarantineValidationObserver, ShadowObservation,
    ShadowObserver, ShadowVerdict, ATTESTATION_QUORUM_CONTROL,
};
use super::window::{DurableTally, RampWindowDelta, RampWindowRow, WindowCounts};

/// The schema string every artifact this module writes carries.
///
/// Versioned from the first write, because `scripts/ramp-report.py` reads it
/// and a reader that guesses at a shape it was not written for reports numbers
/// that are wrong rather than absent.
///
/// # Why `/2`
///
/// `/1` could only be produced by an in-memory harness, and it shows: three of
/// its fields cannot be filled honestly by an artifact assembled from the
/// durable window [`super::window`] landed, and one identity it implies does
/// not hold on traffic with sessionless submissions in it. `/2` is the shape
/// an emit path reading durable state can fill without inventing anything:
///
/// - [`RampSnapshot::accounts_qualifying`], `accounts_observed`,
///   `accounts_would_act` and `accounts_truncated` became `Option<u64>`. They
///   are distinct-account cardinalities, which are per-process by construction
///   — see [`super::window`]'s module docs — so an assembler that never
///   metered anything has no value for them. `None` says so. This is the
///   change that forces the version rather than an additive one: a `/1` reader
///   handed `null` there either raises formatting it or, reading it through a
///   defaulting accessor, treats "unknown" as `0` — and `0` distinct accounts
///   *satisfies* clause (f)'s spread term. A reader that guesses reports a
///   safety term as met.
/// - [`RampSnapshot::truncation_seen`] is new, and is the only durable form
///   the truncation warning has: the count is a cardinality and does not fold,
///   so [`super::window::WindowCounts::fleet_truncation_seen`] carries a flag
///   and this carries it into the artifact. Without it a restored meter's
///   snapshot reported `accounts_truncated = 0` for a window an *earlier*
///   process had truncated, which understates two figures silently.
/// - [`UnattributedTally::unevaluated`] is new, for the reconciliation its own
///   documentation states.
/// - [`Provenance::windows`] is new: the durable rows' own identity —
///   generation, open time, flush count, reset reason — so a `traffic:
///   production` claim is checkable against the rows it was made from instead
///   of taken on the producer's word.
///
/// Two fields joined `provenance.windows` later, additively and still `/2`:
/// `ruleset_ids` and `rulesets_truncated`, the stamp behind D32 open question
/// 6's 2026-09-03 resolution. The same test that forced the version marks
/// them additive: a previous reader cannot misread a field it does not know,
/// and nothing — gate figure, bound, verdict — defaults through it. The `/1`
/// failure was a *known* field whose value changed meaning under a
/// defaulting accessor; a new field beside the old ones has no such path.
pub const RAMP_ARTIFACT_SCHEMA: &str = "orrery.ramp.report/2";

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

    /// Write one already-signed posture row, and append the change to the
    /// control's durable posture-change history in the same transaction.
    ///
    /// This takes a [`super::posture::SignedRampPosture`] and not a
    /// [`RampPosture`], and there is no overload that takes the latter: the
    /// type system is where "an unauthenticated posture write does not exist"
    /// is stated. Signing is [`super::posture::sign_posture`], which needs the
    /// operator secret this process does not hold — `persistd` links this
    /// method and can never call it usefully.
    ///
    /// The history append is what makes D32 open question 2's incident
    /// history durable: `ramp/{control}` holds only *current* state and every
    /// write overwrites it, so without the shadow, "who suspended what, when,
    /// why" survived nowhere. Appending here — the one seam every posture
    /// write goes through — means the history cannot be forgotten by a
    /// writer, and a commit that replaced the row but failed to record the
    /// change is not a state the cluster can be in. The append-only shadow
    /// lives at [`crate::keyspace::posture_history_key`]; the journal archive
    /// remains the record's likely long-term home, and this span does not
    /// presume to have settled the question.
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
        let history = super::posture::PostureHistoryRow {
            recorded_at_ms: wall_clock_ms(),
            change: super::posture::PostureChange::Set(row.clone()),
        };
        let history = postcard::to_allocvec(&history)
            .map_err(|error| RampPostureError(format!("encode posture history: {error}")))?;
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_key(control);
        let history_key = crate::keyspace::posture_history_versionstamped_key(control);
        db.run(move |transaction, _| {
            let (key, value, history_key, history) = (
                key.clone(),
                value.clone(),
                history_key.clone(),
                history.clone(),
            );
            async move {
                transaction.set(&key, &value);
                transaction.atomic_op(
                    &history_key,
                    &history,
                    foundationdb::options::MutationType::SetVersionstampedKey,
                );
                Ok(())
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampPostureError(format!("write ramp posture transaction: {error}"))
        })
    }

    /// Remove one control's posture row, restoring the CLI startup default,
    /// and append the removal to the control's durable posture-change
    /// history in the same transaction.
    ///
    /// The removal is recorded rather than left implicit: "the incident ended"
    /// is itself a posture event an operator reviewing the history needs to
    /// find, and a `clear` that left no trace would read in the history as an
    /// un-ended incident.
    ///
    /// # Errors
    ///
    /// A FoundationDB transaction failure.
    pub async fn clear(&self, control: &str) -> Result<(), RampPostureError> {
        let history = super::posture::PostureHistoryRow {
            recorded_at_ms: wall_clock_ms(),
            change: super::posture::PostureChange::Cleared,
        };
        let history = postcard::to_allocvec(&history)
            .map_err(|error| RampPostureError(format!("encode posture history: {error}")))?;
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_key(control);
        let history_key = crate::keyspace::posture_history_versionstamped_key(control);
        db.run(move |transaction, _| {
            let (key, history_key, history) = (key.clone(), history_key.clone(), history.clone());
            async move {
                transaction.clear(&key);
                transaction.atomic_op(
                    &history_key,
                    &history,
                    foundationdb::options::MutationType::SetVersionstampedKey,
                );
                Ok(())
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampPostureError(format!("clear ramp posture transaction: {error}"))
        })
    }

    /// One control's durable posture-change history, oldest first.
    ///
    /// The review view of D32 open question 2's shadow: every superseded row
    /// and every removal, in commit order, each carrying the envelope it was
    /// written with so a reviewer can re-verify it the way a poller would.
    /// Ordering is the key's versionstamp — the only "when" the cluster
    /// vouches for — and `recorded_at_ms` in each row is the writer's clock,
    /// which is evidence about the writer and not a substitute for it.
    ///
    /// # Errors
    ///
    /// A FoundationDB transaction failure, or a row that does not decode.
    pub async fn history(
        &self,
        control: &str,
    ) -> Result<Vec<super::posture::PostureHistoryEntry>, RampPostureError> {
        use futures::TryStreamExt;

        let db = std::sync::Arc::clone(&self.db);
        let start = crate::keyspace::posture_history_range_start(control);
        let end = crate::keyspace::posture_history_range_end(control);
        let rows: Vec<super::posture::PostureHistoryEntry> = db
            .run(move |transaction, _| {
                let (start, end) = (start.clone(), end.clone());
                async move {
                    let mut stream = transaction.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(&start),
                            end: foundationdb::KeySelector::first_greater_or_equal(&end),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    let mut entries = Vec::new();
                    while let Some(kv) = stream
                        .try_next()
                        .await
                        .map_err(history_err("scan history"))?
                    {
                        let (found_control, versionstamp) =
                            crate::keyspace::decode_posture_history_key(kv.key()).ok_or_else(
                                || {
                                    history_err("decode history key")(
                                        "a row inside the span does not decode to a history key",
                                    )
                                },
                            )?;
                        if found_control != control {
                            return Err(history_err("decode history key")(
                                "a row inside this control's scan names another control",
                            ));
                        }
                        let row: super::posture::PostureHistoryRow =
                            postcard::from_bytes(kv.value())
                                .map_err(history_err("decode history row"))?;
                        entries.push(super::posture::PostureHistoryEntry { versionstamp, row });
                    }
                    Ok(entries)
                }
            })
            .await
            .map_err(|error: foundationdb::FdbBindingError| {
                RampPostureError(format!("read posture history transaction: {error}"))
            })?;
        Ok(rows)
    }
}

/// The writer's wall clock, in Unix milliseconds.
///
/// Used where a change is *recorded* — the history row's `recorded_at_ms` —
/// and never where one is ordered: ordering is the commit versionstamp, and
/// this clock is evidence about the writer, not an index. Clamped the same
/// way [`FdbRampPostureStore::read`] clamps its own read, for the same
/// reason: a clock before the epoch or past `u64::MAX` records as a boundary
/// value rather than failing the write that carries it.
#[cfg(feature = "fdb")]
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Lift a step description into the mapper every `db.run` closure needs at
/// each fallible step — the same shape `super::cohort::store_err` gives the
/// cohort store, and for the same reason: a transaction's failure type is
/// [`foundationdb::FdbBindingError`], and the step name survives only if the
/// failure is wrapped on the way out.
#[cfg(feature = "fdb")]
fn history_err<E: std::fmt::Display>(
    what: &'static str,
) -> impl Fn(E) -> foundationdb::FdbBindingError {
    move |error| {
        foundationdb::FdbBindingError::new_custom_error(Box::new(PostureHistoryStepError(format!(
            "{what}: {error}"
        ))))
    }
}

/// The error wrapper `new_custom_error` needs: a named step that failed
/// inside a history transaction, carrying the underlying reason.
#[cfg(feature = "fdb")]
#[derive(Debug)]
struct PostureHistoryStepError(String);

#[cfg(feature = "fdb")]
impl std::fmt::Display for PostureHistoryStepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(feature = "fdb")]
impl std::error::Error for PostureHistoryStepError {}

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
    /// The rulesets this control's counters saw since the last drain — the
    /// live half of the window stamp. Small by construction and folded by
    /// union, so a delta re-sending it after a failed flush cannot
    /// double-count.
    ruleset_ids: BTreeSet<RulesetId>,
}

/// The per-control counters D32 clause (e)'s predicate and clause (f)'s
/// trigger are computed from.
///
/// One meter per control. It is a [`ShadowObserver`], so
/// [`super::BaselineIntentValidator::shadow_observing`] takes it with no new
/// wiring, and it overrides [`ShadowObserver::record_qualifying`] — the
/// denominator's counting point — which the default implementation ignores.
///
/// # Durability
///
/// The counters and `W`'s bounds are periodically flushed to
/// [`super::window`]'s `rampw/{control}` row and reloaded at startup, so a
/// restart *continues* the window instead of opening a new one. The meter
/// therefore holds two things: the live [`Tallies`] the admission path writes,
/// and a [`MeterState`] recording what the durable row already contains.
/// [`Self::snapshot`] reports their sum.
///
/// # Cost, against D16's budget
///
/// **On the admission path**, unchanged: two `Mutex` acquisitions per intent,
/// each holding a `BTreeMap` lookup and a handful of integer adds, and only
/// when a validator was built with an observer at all (the field is an
/// `Option` and the default is `None`). D16 budgets the whole intent commit at
/// a 10 ms p99; this is an uncontended lock and a tree descent of depth
/// `log₂(|accounts|)`. Shadow is a temporary posture paying a temporary tax,
/// which is the same trade D32 clause (d) prices for the marked `AttestRow`.
///
/// The durability work adds **nothing** to that path. It is not a second lock
/// on it: [`Self::take_delta`] takes the same `tallies` lock the counters
/// already use, once per flush interval, and the cohort join it performs runs
/// on the drained copy after the lock is released. The flush itself is one
/// FoundationDB read-modify-write per control per interval, on a background
/// task — at the default 60 s cadence and four measurable controls that is
/// `4/60 ≈ 0.067` transactions per second per process, against a budget
/// written for a 10 ms *per intent* path. The only figure a deployment should
/// weigh is the loss bound: an ungraceful stop discards at most one flush
/// interval of counters, which over clause (e)'s thirty days is
/// `60 s / 30 d ≈ 0.0023%` of the window.
#[derive(Debug)]
pub struct RampMeter {
    control: &'static str,
    capacity: usize,
    tallies: Mutex<Tallies>,
    durable: Mutex<MeterState>,
}

/// What the meter knows about its durable window, plus the process-local
/// bookkeeping a drain must not destroy.
///
/// `base` is what the `rampw/{control}` row held as of the last successful
/// flush or load — including contributions from *other* processes metering the
/// same control. `pending` is what this process has drained out of `tallies`
/// and not yet had acknowledged by a write.
///
/// The three-way split (live / pending / base) is what makes a failed flush
/// lossless: a drained delta lives in `pending` until the store confirms it,
/// and the next flush retries with `pending` plus whatever `tallies` collected
/// meanwhile. Nothing is counted twice, because a count is in exactly one of
/// the three at any moment.
#[derive(Debug, Default)]
struct MeterState {
    base: RampWindowRow,
    pending: WindowCounts,
    retained: RetainedAccounts,
}

/// The distinct-account sets a drain moves out of [`Tallies`] and keeps.
///
/// Volumes can be summed after a drain; cardinalities cannot, so the ids
/// themselves have to survive it. Without this, every flush would reset
/// [`RampSnapshot::accounts_qualifying`] and friends to zero and the reported
/// spread would describe one flush interval instead of the process's run —
/// a regression the durability work must not introduce while fixing the
/// window.
///
/// These are **not** persisted, for the reason [`super::window`]'s module docs
/// give: at [`DEFAULT_ACCOUNT_CAPACITY`] the fleet-side ids do not fit in a
/// FoundationDB value, and an approximation would be a fabricated number. They
/// are bounded by the meter's own capacity, since only accounts that earned an
/// individual tally ever enter them.
#[derive(Debug, Default)]
struct RetainedAccounts {
    qualifying: BTreeSet<AccountId>,
    observed: BTreeSet<AccountId>,
    would_act: BTreeSet<AccountId>,
    truncated: BTreeSet<AccountId>,
}

impl RetainedAccounts {
    /// Absorb the account identities out of a drained `Tallies`.
    fn absorb(&mut self, tallies: &Tallies) {
        for (account, tally) in &tallies.per_account {
            if tally.qualifying > 0 {
                self.qualifying.insert(*account);
            }
            if tally.observed > 0 {
                self.observed.insert(*account);
            }
            if tally.would_act > 0 {
                self.would_act.insert(*account);
            }
        }
        self.truncated
            .extend(tallies.truncated_accounts.iter().copied());
    }
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
            durable: Mutex::new(MeterState::default()),
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

    /// Stamp one `RulesetId` into the window this meter is measuring — the
    /// owner's 2026-09-03 resolution of D32 open question 6.
    ///
    /// A window that spanned a ruleset change must say so, because the owner
    /// declined the automatic reset: a reset discards evidence irreversibly
    /// and would fire only for the controls clause (f) scopes to a rule
    /// version, an inconsistency worse than the problem. The stamp is the
    /// alternative that neither loses evidence nor hides the span. Call it
    /// where the meter first sees the ruleset — the same counting point as
    /// the denominator, so the set describes the window's counters and not
    /// one arm of them. C1's and C2's meters are never handed one: clause (f)
    /// scopes the dimension to the verdict-driven C3/C4/C5, and their stamps
    /// would be a column that is always the same value.
    ///
    /// The durable set unions across flushes like the account sets and is
    /// bounded by
    /// [`MAX_DURABLE_RULESET_IDS`](super::window::MAX_DURABLE_RULESET_IDS)
    /// with the overflow reported rather than absorbed.
    pub fn observe_ruleset(&self, ruleset: RulesetId) {
        self.lock().ruleset_ids.insert(ruleset);
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

    fn durable(&self) -> std::sync::MutexGuard<'_, MeterState> {
        self.durable
            .lock()
            .expect("ramp meter durable state poisoned")
    }

    /// The durable window this meter believes it is measuring, as of its last
    /// load or flush, **plus** what it has drained but not yet written.
    ///
    /// Not the live counters: those are still in `tallies` until a flush
    /// drains them. [`Self::snapshot`] is what sums all three.
    #[must_use]
    pub fn durable_window(&self) -> RampWindowRow {
        let durable = self.durable();
        let mut row = durable.base.clone();
        row.counts.fold(&durable.pending);
        row
    }

    /// The generation this meter is measuring into.
    #[must_use]
    pub fn window_id(&self) -> u64 {
        self.durable().base.window_id
    }

    /// Adopt a durable window read at startup, so the process *continues* it.
    ///
    /// This is the half of the fix that makes clause (e)'s `W ≥ 30 days`
    /// reachable at all: without it, `first_ms` was `None` after every deploy
    /// and the window restarted on a cadence far shorter than thirty days.
    ///
    /// Call it before the meter is attached to anything. Any drained-but-
    /// unwritten counters are discarded, because a process that has just
    /// started has none and one that has not cannot know they belong to the
    /// generation it just read.
    pub fn restore(&self, row: RampWindowRow) {
        let mut durable = self.durable();
        durable.base = row;
        durable.pending = WindowCounts::default();
        durable.retained = RetainedAccounts::default();
    }

    /// Record that a flush was accepted, adopting the row the store returned.
    ///
    /// The row is taken from the store rather than computed locally on
    /// purpose: it carries every *other* process's contribution to the same
    /// control, so this meter's snapshot reports the fleet's window and not
    /// its own share of it.
    pub fn commit_flush(&self, applied: RampWindowRow) {
        let mut durable = self.durable();
        durable.base = applied;
        durable.pending = WindowCounts::default();
    }

    /// Adopt a window generation this meter was not measuring into, discarding
    /// the delta it was holding.
    ///
    /// The reset path. The discarded counters were observed under the retired
    /// generation, and folding them into the fresh one would put pre-reset
    /// observations into a window opened precisely to exclude them — see
    /// [`super::window`]'s module docs on why a reset is allowed to cost one
    /// flush interval.
    pub fn adopt_window(&self, row: RampWindowRow) {
        self.restore(row);
    }

    /// Drain the live counters into a flushable delta, joined to `H`.
    ///
    /// The cohort join happens here rather than at count time because the
    /// counting points are on the admission path and the cohort is an
    /// operator-written roster: a per-intent membership lookup would put a
    /// second tree descent inside D16's budget for no measurement benefit.
    /// This runs once per flush interval, on a background task, over only the
    /// accounts that produced traffic since the last one.
    ///
    /// An account sampled into **both** halves is counted as armed, matching
    /// [`HonestCohort::len`]'s treatment of it as one member: the two halves'
    /// volumes must sum to the cohort's, and counting such an account twice
    /// would inflate `coverage`'s denominator with traffic that happened once.
    ///
    /// The returned delta includes anything a previous flush drained and did
    /// not get written, so a failed flush is retried rather than lost. Exactly
    /// one flusher may be in flight per meter, since a second would clear a
    /// `pending` the store has not acknowledged; `persistd` runs a single task
    /// that visits every control in turn.
    pub fn take_delta(&self, cohort: &HonestCohort) -> RampWindowDelta {
        let drained = std::mem::take(&mut *self.lock());
        let counts = Self::counts_from(&drained, cohort);
        let mut durable = self.durable();
        durable.retained.absorb(&drained);
        durable.pending.fold(&counts);
        RampWindowDelta {
            window_id: durable.base.window_id,
            counts: durable.pending.clone(),
        }
    }

    /// Fold one drained `Tallies` into the durable shape, splitting `H` into
    /// its two halves on the way.
    fn counts_from(tallies: &Tallies, cohort: &HonestCohort) -> WindowCounts {
        let mut counts = WindowCounts {
            first_ms: tallies.first_ms,
            last_ms: tallies.last_ms,
            fleet_truncation_seen: !tallies.truncated_accounts.is_empty(),
            ruleset_ids: tallies.ruleset_ids.clone(),
            ..WindowCounts::default()
        };
        for (account, tally) in &tallies.per_account {
            let durable = durable_tally(tally);
            counts.fleet.fold(&durable);
            // Armed first: an account in both halves is one member, and the
            // half whose honesty is a property of the operator's harness is
            // the one that describes it.
            let half = if cohort.armed.contains(account) {
                Some((
                    &mut counts.armed,
                    &mut counts.armed_active,
                    &mut counts.armed_would_act,
                ))
            } else if cohort.natural.contains(account) {
                Some((
                    &mut counts.natural,
                    &mut counts.natural_active,
                    &mut counts.natural_would_act,
                ))
            } else {
                None
            };
            if let Some((volumes, active, would_act)) = half {
                volumes.fold(&durable);
                if tally.qualifying > 0 {
                    active.insert(*account);
                }
                if tally.would_act > 0 {
                    would_act.insert(*account);
                }
            }
        }
        // The truncation bucket joins the fleet total and is deliberately kept
        // out of the halves', for `snapshot`'s reason: its accounts are not
        // individually known, so attributing any of it to `H` would be a guess.
        counts.fleet.fold(&durable_tally(&tallies.truncated));
        counts.unattributed = durable_tally(&tallies.unattributed);
        counts.by_verdict = tallies
            .by_verdict
            .iter()
            .map(|(verdict, count)| ((*verdict).to_owned(), *count))
            .collect();
        counts
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
        // The fleet-side cardinalities are unions and not running counts: a
        // drain moves the counters out of `per_account` but keeps the ids in
        // `retained`, so an account that produced traffic in two flush
        // intervals is one account here rather than two.
        let (
            mut accounts_qualifying,
            mut accounts_observed,
            mut accounts_would_act,
            mut accounts_truncated,
        ) = {
            let durable = self.durable();
            (
                durable.retained.qualifying.clone(),
                durable.retained.observed.clone(),
                durable.retained.would_act.clone(),
                durable.retained.truncated.clone(),
            )
        };
        let mut honest_active: BTreeSet<AccountId> = BTreeSet::new();
        let mut honest_accounts_would_act: BTreeSet<AccountId> = BTreeSet::new();

        for (account, tally) in &tallies.per_account {
            all.fold(tally);
            if tally.qualifying > 0 {
                accounts_qualifying.insert(*account);
            }
            if tally.observed > 0 {
                accounts_observed.insert(*account);
            }
            if tally.would_act > 0 {
                accounts_would_act.insert(*account);
            }
            if cohort.contains(*account) {
                honest.fold(tally);
                if tally.qualifying > 0 {
                    honest_active.insert(*account);
                }
                if tally.would_act > 0 {
                    honest_accounts_would_act.insert(*account);
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
        accounts_truncated.extend(tallies.truncated_accounts.iter().copied());

        // Everything the durable window already holds — this process's earlier
        // flushes, every other process's, and every process that ran before
        // the last restart. Summed here rather than in the counting path so
        // the admission path never touches it.
        let carried = self.durable_window();
        let mut fleet = durable_tally(&all);
        fleet.fold(&carried.counts.fleet);
        let mut honest_volumes = durable_tally(&honest);
        honest_volumes.fold(&carried.counts.armed);
        honest_volumes.fold(&carried.counts.natural);
        let mut unattributed = durable_tally(&tallies.unattributed);
        unattributed.fold(&carried.counts.unattributed);

        let coverage = if honest_volumes.qualifying == 0 {
            None
        } else {
            // `f64::from` on a u64 does not exist for a reason; both counts are
            // event counts and exceeding 2^53 of them is not a shadow period.
            #[allow(clippy::cast_precision_loss)]
            Some(honest_volumes.observed as f64 / honest_volumes.qualifying as f64)
        };

        let first = merge_bound(tallies.first_ms, carried.counts.first_ms, u64::min);
        let last = merge_bound(tallies.last_ms, carried.counts.last_ms, u64::max);
        #[allow(clippy::cast_precision_loss)]
        let window_days = (last
            .unwrap_or_default()
            .saturating_sub(first.unwrap_or_default())) as f64
            / 86_400_000.0;

        let mut by_verdict = string_keys(&tallies.by_verdict);
        fold_string_counts(&mut by_verdict, &carried.counts.by_verdict);

        for account in carried
            .counts
            .armed_active
            .union(&carried.counts.natural_active)
        {
            honest_active.insert(*account);
        }
        for account in carried
            .counts
            .armed_would_act
            .union(&carried.counts.natural_would_act)
        {
            honest_accounts_would_act.insert(*account);
        }

        RampSnapshot {
            control: self.control.to_owned(),
            observed_from_ms: first.unwrap_or_default(),
            observed_to_ms: last.unwrap_or_default(),
            window_days,
            qualifying: fleet.qualifying,
            observed: fleet.observed,
            unevaluated: fleet.unevaluated,
            would_act: fleet.would_act,
            accounts_qualifying: Some(accounts_qualifying.len() as u64),
            accounts_observed: Some(accounts_observed.len() as u64),
            accounts_would_act: Some(accounts_would_act.len() as u64),
            accounts_truncated: Some(accounts_truncated.len() as u64),
            // Either this process truncated, or a process that flushed into
            // this window before it did. The second half is why the flag
            // exists at all: the count cannot fold, so a restored meter would
            // otherwise report a clean sheet for a window that is understated.
            truncation_seen: !accounts_truncated.is_empty() || carried.counts.fleet_truncation_seen,
            unattributed: UnattributedTally {
                qualifying: unattributed.qualifying,
                observed: unattributed.observed,
                unevaluated: unattributed.unevaluated,
                would_act: unattributed.would_act,
            },
            by_verdict,
            by_cause: fleet.causes,
            cohort: CohortEvidence {
                armed: cohort.armed.len() as u64,
                natural: cohort.natural.len() as u64,
                size: cohort.len() as u64,
                active: honest_active.len() as u64,
                qualifying: honest_volumes.qualifying,
                observed: honest_volumes.observed,
                unevaluated: honest_volumes.unevaluated,
                coverage,
                fp_count: honest_volumes.would_act,
                accounts_would_act: honest_accounts_would_act.len() as u64,
                by_cause: honest_volumes.causes,
            },
        }
    }
}

/// One in-process account tally in the durable shape.
fn durable_tally(tally: &AccountTally) -> DurableTally {
    DurableTally {
        qualifying: tally.qualifying,
        observed: tally.observed,
        unevaluated: tally.unevaluated,
        would_act: tally.would_act,
        causes: string_keys(&tally.causes),
    }
}

/// Take one side of `W` across the live and durable halves.
fn merge_bound(live: Option<u64>, durable: Option<u64>, pick: fn(u64, u64) -> u64) -> Option<u64> {
    match (live, durable) {
        (Some(live), Some(durable)) => Some(pick(live, durable)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn fold_string_counts(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (label, count) in from {
        let slot = into.entry(label.clone()).or_default();
        *slot = slot.saturating_add(*count);
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
    /// Recorded evaluations that produced no verdict (clause (b)'s degraded
    /// arm), which are neither numerator nor denominator.
    ///
    /// Carried because [`RampSnapshot::by_verdict`] counts *every* recorded
    /// verdict, unattributed ones included, while
    /// [`RampSnapshot::observed`] and [`RampSnapshot::unevaluated`] count only
    /// attributed traffic and the truncation bucket. Without this field the
    /// outcome split cannot be reconciled against the volumes that produced
    /// it on any traffic with sessionless submissions in it — which is all
    /// production traffic, and none of the harness traffic the `/1` artifact
    /// was written from. The reconciliation is
    /// `sum(by_verdict) = observed + unevaluated + unattributed.observed +
    /// unattributed.unevaluated`, and `scripts/ramp-report.py` checks it.
    pub unevaluated: u64,
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
    /// Distinct accounts with any qualifying activity, **over this process's
    /// run** rather than over the durable window — or `None` when no process
    /// run stands behind the figure at all.
    ///
    /// The scope is stated rather than papered over, because it is the one
    /// figure in this struct that a restart still resets. Distinct-account
    /// counts cannot be added across flushes or across processes — the same
    /// account seen in two of them is one account, and only the ids can say so
    /// — and the fleet-side ids do not fit in a FoundationDB value: the meter
    /// tracks up to [`DEFAULT_ACCOUNT_CAPACITY`] of them. See
    /// [`super::window`]'s module docs for the table of what is durable and
    /// why this is not, and note that the cohort-side cardinalities in
    /// [`CohortEvidence::active`] and [`CohortEvidence::accounts_would_act`]
    /// *are* durable, because `H` is a hand-sampled roster whose ids do fit.
    ///
    /// A flush does not disturb it. The ids are retained in-process when the
    /// counters are drained, so this counts the run and not the interval since
    /// the last flush.
    ///
    /// `None` is the artifact assembler's answer — see
    /// [`Self::without_process_cardinalities`]. An operator tool reading the
    /// durable window has *no* process run behind it, and reporting `0` there
    /// would be a plausible number where there is no measurement: a window
    /// holding five million admission decisions "across 0 accounts" reads as a
    /// finding rather than as an absence.
    pub accounts_qualifying: Option<u64>,
    /// Distinct accounts with any observation, over this process's run — see
    /// [`Self::accounts_qualifying`].
    pub accounts_observed: Option<u64>,
    /// Distinct accounts with a would-have-acted event — clause (f)'s
    /// `spread`, which is cardinality rather than volume because
    /// docs/07:237's alarm is "across unrelated accounts" and an event counter
    /// cannot answer it.
    ///
    /// Over this process's run, and no promotion decision hangs on that: the
    /// auto-suspend breaker computes clause (f)'s `spread` independently in
    /// [`super::autosuspend::SuspendMonitor`], over its own
    /// [`WINDOW_MS`](super::autosuspend::WINDOW_MS) rolling window, and does
    /// not read this field.
    ///
    /// This is the field that makes `/2` a version bump rather than an
    /// addition. `None` here must render as *not evaluated*: `0` distinct
    /// accounts is under clause (f)'s spread bound, so a reader that defaults
    /// the absence to zero reports a safety term as satisfied by an artifact
    /// that never measured it.
    pub accounts_would_act: Option<u64>,
    /// Distinct accounts folded into the truncation bucket, over this
    /// process's run. Nonzero means account spread and the cohort denominator
    /// are both understated.
    ///
    /// `None` for an assembled artifact, whose truncation evidence is
    /// [`Self::truncation_seen`] instead — a flag, because the count is a
    /// cardinality and cardinalities do not fold across flushes.
    pub accounts_truncated: Option<u64>,
    /// Whether *any* traffic in this window was folded into the meter's
    /// past-capacity truncation bucket, by this process or an earlier one.
    ///
    /// The durable form of the truncation warning, carried from
    /// [`super::window::WindowCounts::fleet_truncation_seen`]. True means the
    /// fleet-wide account spread and the cohort denominator are both
    /// understated by an unknown amount, and `scripts/ramp-report.py` refuses
    /// to cite the artifact.
    pub truncation_seen: bool,
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

impl RampSnapshot {
    /// The same measurement with the fleet-wide distinct-account
    /// cardinalities marked *unassembled* rather than reported as zero.
    ///
    /// The artifact assembler's honesty valve. Those four fields count
    /// distinct accounts over the metering process's own run
    /// ([`Self::accounts_qualifying`] says why they can be nothing else), and
    /// an assembler that loaded a durable window has no run: it never sat on
    /// an admission path. The counters it did load are a fleet's, so writing
    /// `0` beside them would not be a small error, it would be a fabricated
    /// finding — and for [`Self::accounts_would_act`] a fabricated *safe*
    /// finding, since zero spread is under clause (f)'s bound.
    ///
    /// [`Self::truncation_seen`] is deliberately left alone: it is a flag and
    /// it does fold, so it survives the durable window and is the one piece of
    /// truncation evidence an assembled artifact can carry honestly.
    #[must_use]
    pub fn without_process_cardinalities(mut self) -> Self {
        self.accounts_qualifying = None;
        self.accounts_observed = None;
        self.accounts_would_act = None;
        self.accounts_truncated = None;
        self
    }
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

/// One durable measurement window's own identity, as the artifact producer
/// found it.
///
/// This is what makes a `traffic: production` claim *checkable* rather than
/// merely asserted. Nothing in a `rampw/{control}` row records which fleet
/// wrote it — the row is counters, and [`super::window`]'s module docs are
/// explicit that possession of the cluster file is the trust boundary here —
/// so an artifact cannot prove its own provenance from the bytes. What it can
/// do is carry the row's identity, so a reviewer holding the same cluster file
/// can read the same generation back and see the same numbers, and so a
/// reviewer *not* holding it can see the shape of the measurement: a window
/// whose counters are large and whose `flushes` is 1 was written by one
/// process once, which is not a fleet.
///
/// `flushes` is reported, never compared against a floor. Clause (e)'s terms
/// and their thresholds are the record's; adding a "flushes ≥ n" gate here
/// would invent one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowProvenance {
    /// The `rampw/{control}` suffix this row was read from.
    pub control: String,
    /// [`super::window::RampWindowRow::window_id`] — which generation of the
    /// window these counters belong to.
    pub window_id: u64,
    /// When the generation was opened, as distinct from its first observation.
    pub opened_at_ms: u64,
    /// How many flushes folded into the row.
    pub flushes: u64,
    /// The operator's reason for the reset that opened this generation, absent
    /// for the one a first flush opened implicitly.
    pub reset_reason: Option<String>,
    /// `H` account ids the row could not record because a set was full.
    /// Nonzero means the cohort's `active` and `accounts_would_act` figures
    /// are understated by at most this much.
    pub cohort_accounts_truncated: u64,
    /// Every `RulesetId` the window's counters observed — the stamp the owner
    /// chose for D32 open question 6 on 2026-09-03, spelled by
    /// [`ruleset_stamp_label`](super::window::ruleset_stamp_label) as
    /// `v<version>:<64 hex>` and ordered by version then digest.
    ///
    /// A reviewer reads this list for the span: two ids mean the window's
    /// counters straddle a ruleset change, and the promotion evidence in this
    /// artifact was observed under more than one ruleset — theirs to judge,
    /// not the fleet's to reset away. Protocol-level controls (C1, C2) carry
    /// an empty list by construction; clause (f) scopes the dimension to
    /// verdict-driven C3/C4/C5.
    ///
    /// Absent (`[]` by this field's `default`) in artifacts written before
    /// the stamp landed. The field is additive within `/2` deliberately: no
    /// previous reader knows it, so none can misread it, and nothing — no
    /// gate figure, no bound, no verdict — derives from it. That is the test
    /// `/2`'s own bump applied, and it is why this is not `/3`.
    #[serde(default)]
    pub ruleset_ids: Vec<String>,
    /// Distinct rulesets [`Self::ruleset_ids`] could not name because the row
    /// was already at its bound. Nonzero means the span above is understated.
    #[serde(default)]
    pub rulesets_truncated: u64,
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
    /// The durable windows this artifact was assembled from, one per control
    /// that had a row.
    ///
    /// Empty for an artifact produced from in-process counters — a harness
    /// run has no durable window, and an empty list beside `traffic:
    /// production` is itself a fact a reviewer should notice.
    pub windows: Vec<WindowProvenance>,
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
        assert_eq!(one.accounts_would_act, Some(1));
        assert_eq!(two.accounts_would_act, Some(2));
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
            snapshot.accounts_would_act,
            Some(0),
            "spread is over accounts, and this submission has none — and a meter that \
             *did* meter reports zero of them, which is not the same as the absent \
             cardinality an assembled artifact carries"
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
        assert_eq!(snapshot.accounts_qualifying, Some(2));
        assert_eq!(snapshot.accounts_truncated, Some(3));
        assert!(
            snapshot.truncation_seen,
            "the flag is the only form the warning takes in a durable window"
        );
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
                windows: Vec::new(),
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

    /// A cohort with both halves populated, so a test can check the split is
    /// still legible after whatever it is testing.
    fn split_cohort(
        armed: impl IntoIterator<Item = u64>,
        natural: impl IntoIterator<Item = u64>,
    ) -> HonestCohort {
        let mut cohort = HonestCohort::new();
        for member in armed {
            cohort.arm(AccountId::new(member));
        }
        for member in natural {
            cohort.sample(AccountId::new(member));
        }
        cohort
    }

    /// Stand in for the durable store: fold a delta into a row the way
    /// [`super::super::window::FdbRampWindowStore::flush`] does, without
    /// needing a cluster.
    ///
    /// The FDB-backed proof of the same property is
    /// `window::tests::a_window_survives_a_simulated_persistd_restart`; this
    /// one runs in the default lane, so a regression in the *merge* is caught
    /// even in a checkout with no cluster — which is where `fdb-tests.sh` is
    /// not run.
    fn apply(row: &mut RampWindowRow, delta: &RampWindowDelta) {
        assert_eq!(row.window_id, delta.window_id, "generations must match");
        row.counts.fold(&delta.counts);
        row.flushes += 1;
    }

    /// The defect #990 names, at the seam that fixes it.
    ///
    /// Before this, `first_ms`/`last_ms` and every counter were fields of the
    /// in-process `Mutex<Tallies>`: dropping the meter — which is what a
    /// deploy does — reset `W` to zero and lost the counts observed over `H`.
    /// A thirty-day window was therefore unreachable no matter how long the
    /// fleet ran.
    #[test]
    fn a_window_and_both_halves_survive_dropping_the_meter() {
        const DAY_MS: u64 = 86_400_000;
        let cohort = split_cohort([1, 2], [3, 4]);
        let mut stored = RampWindowRow::opened(0, 0, None);

        // ── The process that ran for the first nineteen days ──────────────
        let before = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        for day in 0..20_u64 {
            for account in [1_u64, 2, 3, 4] {
                before.record_qualifying(Some(AccountId::new(account)));
                before.record(obs(Some(account), ShadowVerdict::WouldAdmit, day * DAY_MS));
            }
        }
        // One would-have-acted event in each half, which is the distinction a
        // promotion reviewer actually reads.
        for account in [1_u64, 3] {
            before.record_qualifying(Some(AccountId::new(account)));
            before.record(obs(
                Some(account),
                ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet),
                19 * DAY_MS,
            ));
        }

        let first_half = before.snapshot(&cohort);
        assert!((first_half.window_days - 19.0).abs() < 1e-9);
        assert_eq!(first_half.cohort.fp_count, 2);

        let delta = before.take_delta(&cohort);
        apply(&mut stored, &delta);
        before.commit_flush(stored.clone());

        // The armed and natural volumes reached the durable row apart, which
        // is the non-negotiable part: a row that summed them could not tell
        // "would have refused a bot" from "would have refused a player".
        assert_eq!(stored.counts.armed.would_act, 1);
        assert_eq!(stored.counts.natural.would_act, 1);
        assert_eq!(stored.counts.armed_would_act.len(), 1);
        assert_eq!(stored.counts.natural_would_act.len(), 1);

        // ── The deploy ────────────────────────────────────────────────────
        drop(before);

        // ── The process that came up after it ─────────────────────────────
        let after = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        let restarted = after.snapshot(&cohort);
        assert!(
            restarted.window_days.abs() < f64::EPSILON,
            "a meter that has not reloaded is exactly the old behaviour, and \
             this is the assertion that would have caught #990"
        );

        after.restore(stored.clone());
        let continued = after.snapshot(&cohort);
        assert_eq!(continued.observed_from_ms, 0);
        assert_eq!(continued.observed_to_ms, 19 * DAY_MS);
        assert!(
            (continued.window_days - 19.0).abs() < 1e-9,
            "the window continued rather than restarted"
        );
        assert_eq!(continued.qualifying, first_half.qualifying);
        assert_eq!(continued.observed, first_half.observed);
        assert_eq!(continued.cohort.fp_count, 2);
        assert_eq!(continued.cohort.active, 4);
        assert_eq!(continued.cohort.accounts_would_act, 2);
        assert_eq!(continued.cohort.coverage, first_half.cohort.coverage);

        // ── Eleven more days, and clause (e)'s time term is reached ───────
        for day in 20..31_u64 {
            for account in [1_u64, 2, 3, 4] {
                after.record_qualifying(Some(AccountId::new(account)));
                after.record(obs(Some(account), ShadowVerdict::WouldAdmit, day * DAY_MS));
            }
        }
        let whole = after.snapshot(&cohort);
        assert!(
            whole.window_days >= 30.0,
            "thirty days across a restart is exactly what #990 said was \
             structurally unreachable; it is {} days",
            whole.window_days
        );
        assert_eq!(whole.qualifying, first_half.qualifying + 44);
        assert_eq!(
            whole.cohort.fp_count, 2,
            "the pre-restart false positives are still counted against H"
        );
    }

    /// A flush that fails is retried, not lost: the drained counters stay in
    /// the meter's pending half and the next delta carries them again.
    #[test]
    fn an_unacknowledged_flush_is_carried_into_the_next_delta() {
        let cohort = split_cohort([7], []);
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        meter.record_qualifying(Some(AccountId::new(7)));
        meter.record(obs(Some(7), ShadowVerdict::WouldAdmit, 5_000));

        let lost = meter.take_delta(&cohort);
        assert_eq!(lost.counts.armed.qualifying, 1);
        // No `commit_flush`: the write failed.

        meter.record_qualifying(Some(AccountId::new(7)));
        meter.record(obs(Some(7), ShadowVerdict::WouldAdmit, 9_000));
        let retried = meter.take_delta(&cohort);
        assert_eq!(
            retried.counts.armed.qualifying, 2,
            "the unacknowledged counts are in the retry, not dropped"
        );
        assert_eq!(retried.counts.first_ms, Some(5_000));
        assert_eq!(retried.counts.last_ms, Some(9_000));

        // And the snapshot never double-counted them while they were pending.
        let snapshot = meter.snapshot(&cohort);
        assert_eq!(snapshot.cohort.qualifying, 2);
    }

    /// Draining does not disturb what a snapshot reports, which is the
    /// property that lets the flush run on its own cadence beside a producer.
    #[test]
    fn a_drain_is_invisible_to_the_snapshot() {
        let cohort = split_cohort([], [11]);
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        for tick in 0..50_u64 {
            meter.record_qualifying(Some(AccountId::new(11)));
            meter.record(obs(Some(11), ShadowVerdict::WouldAdmit, 1_000 + tick));
        }
        let before = meter.snapshot(&cohort);
        let delta = meter.take_delta(&cohort);
        assert_eq!(
            before,
            meter.snapshot(&cohort),
            "a drain moves counters, it does not spend them"
        );

        let mut stored = RampWindowRow::opened(0, 0, None);
        apply(&mut stored, &delta);
        meter.commit_flush(stored);
        assert_eq!(
            meter.snapshot(&cohort),
            before,
            "nor does the acknowledgement"
        );
    }

    /// An account sampled into both halves is one member with one set of
    /// volumes, so `armed + natural` still sums to the cohort's total.
    #[test]
    fn a_member_of_both_halves_is_counted_once_and_as_armed() {
        let mut cohort = HonestCohort::new();
        cohort.arm(AccountId::new(5));
        cohort.sample(AccountId::new(5));
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        for tick in 0..4_u64 {
            meter.record_qualifying(Some(AccountId::new(5)));
            meter.record(obs(Some(5), ShadowVerdict::WouldAdmit, tick));
        }
        let delta = meter.take_delta(&cohort);
        assert_eq!(delta.counts.armed.qualifying, 4);
        assert_eq!(delta.counts.natural.qualifying, 0);
        assert_eq!(
            delta.counts.armed.qualifying + delta.counts.natural.qualifying,
            meter.snapshot(&cohort).cohort.qualifying,
            "the halves sum to the cohort, which is what makes them a split"
        );
    }

    /// A delta measured under a retired generation is discarded, so a
    /// straggler cannot undo a reset.
    #[test]
    fn adopting_a_new_generation_discards_the_delta_measured_under_the_old_one() {
        let cohort = split_cohort([2], []);
        let meter = RampMeter::new(ATTESTATION_QUORUM_CONTROL);
        for tick in 0..10_u64 {
            meter.record_qualifying(Some(AccountId::new(2)));
            meter.record(obs(Some(2), ShadowVerdict::WouldAdmit, tick));
        }
        let stale = meter.take_delta(&cohort);
        assert_eq!(stale.window_id, 0);

        // The operator reset the window while that delta was in flight.
        meter.adopt_window(RampWindowRow::opened(
            1,
            500_000,
            Some("ruleset v9".to_owned()),
        ));

        let snapshot = meter.snapshot(&cohort);
        assert_eq!(
            snapshot.cohort.qualifying, 0,
            "observations taken before a semantic change do not enter the \
             window opened to exclude them"
        );
        assert!(snapshot.window_days.abs() < f64::EPSILON);
        assert_eq!(meter.window_id(), 1);
        assert_eq!(meter.take_delta(&cohort).window_id, 1);
    }
}
