//! The adjudication executor (docs/07 §3 stage 4, D11/D12).
//!
//! A discrepancy report arrives pinned to the `RulesetId` its subject ran. The
//! executor routes it to the matching **version-keyed worker** and re-runs the
//! window there. It does not adjudicate under whatever build happens to be
//! current: rules change, and judging an old window under new rules would
//! manufacture deviations out of a version bump.
//!
//! **Retention is three builds** (D16). Older than that is
//! [`UnadjudicableReason::UnknownRuleset`] — never a strike. That asymmetry is
//! deliberate and it points at the cluster: an operator who ships four rules
//! versions in a week has made old disputes unjudgeable, and the honest answer
//! is to say so rather than to punish whoever reported one.
//!
//! Verdicts are reached by `orrery_core::verify_bundle`, which is a pure
//! function of the evidence. The cluster therefore re-runs what it was sent
//! rather than believing the reporter — the whole point of a self-verifying
//! bundle.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use orrery_core::{verify_bundle, ReplayHarness, Ruleset};
use orrery_protocol::{
    AccountId, DiscrepancyReport, NodeId, PersistId, RulesetId, Tick, UnadjudicableReason,
    UniverseSeed, Verdict,
};
use serde::{Deserialize, Serialize};

use crate::intent::RampMeter;

/// How many rules builds the cluster keeps adjudicable at once (D16).
pub const RETAINED_BUILDS: usize = 3;

/// D33's 14-day strike half-life, in milliseconds.
pub const STRIKE_HALF_LIFE_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// D33's hard 90-day strike-row retention, in milliseconds.
pub const STRIKE_RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1000;
/// D33 clause (a)'s non-zero strike-weight table, in milli-points.
///
/// Keep the named weights below as aliases into this table. Consumers which
/// need a property of the whole table (such as its maximum) must derive it
/// from this value so changing or extending the table cannot leave a second
/// restatement behind.
pub const STRIKE_WEIGHT_TABLE_MILLI: [i32; 3] = [3_000, 1_000, 500];
/// A confirmed replay deviation or fabricated evidence is one major finding.
pub const MAJOR_STRIKE_WEIGHT_MILLI: i32 = STRIKE_WEIGHT_TABLE_MILLI[0];
/// A proved non-cooperation finding.
pub const NON_COOPERATION_WEIGHT_MILLI: i32 = STRIKE_WEIGHT_TABLE_MILLI[1];
/// A reviewed timing-pattern finding.
pub const TIMING_PATTERN_WEIGHT_MILLI: i32 = STRIKE_WEIGHT_TABLE_MILLI[2];

/// Whether a filed strike is calibration-only or affects standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrikeMode {
    /// D32 C5 shadow: file the fact, but never count it toward standing.
    Shadow,
    /// The fact counts toward standing at read time.
    Live,
}

/// The evidence-quality class that assigned a strike's weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrikeKind {
    /// A replay confirmed that the subject deviated from its claim.
    Deviation,
    /// The reporter supplied evidence whose subject signature was false.
    EvidenceForged,
    /// An attester signed a statement adjudication later disproved.
    FalseAttestation,
    /// The responsible account failed the existing proof threshold.
    NonCooperation,
    /// A reviewed timing pattern met the finding threshold.
    TimingPattern,
    /// A compensating fact authorized by an upheld appeal.
    Appeal,
}

impl StrikeKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Deviation => "deviation",
            Self::EvidenceForged => "evidence_forged",
            Self::FalseAttestation => "false_attestation",
            Self::NonCooperation => "non_cooperation",
            Self::TimingPattern => "timing_pattern",
            Self::Appeal => "appeal",
        }
    }
}

/// The adjudicator-derived identity of one confirmed divergence episode.
///
/// The reporter chooses evidence-window bounds, but not any of these values:
/// the subject signs the chain epoch, the ruleset is pinned in the evidence,
/// and replay finds the first divergence. Together they make a confirmed
/// episode one durable strike fact even when several valid windows cover it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrikeEpisodeRef {
    /// The authority whose signed chain replay convicted this episode.
    pub subject: NodeId,
    /// The signed chain epoch containing the divergence.
    pub chain_epoch: u32,
    /// The first tick replay found to diverge from the signed claim chain.
    pub first_diverging_tick: Tick,
}

/// Stable coordinates plus a digest for the evidence behind one strike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrikeEvidenceRef {
    /// The persistent entity whose evidence window was adjudicated.
    pub entity: PersistId,
    /// Inclusive first tick in the evidence window.
    pub window_start: Tick,
    /// Exclusive end tick in the evidence window.
    pub window_end: Tick,
    /// BLAKE3 of the canonical postcard-encoded discrepancy report.
    pub digest: [u8; 32],
}

/// One immutable D33 `ya` ledger value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrikeRow {
    /// Wall-clock filing instant. Evidence and decay input, never key order.
    pub issued_at_ms: u64,
    /// Signed milli-points: 3000, 1000, 500, or a compensating negative.
    pub weight_milli: i32,
    /// Why this fact has that weight.
    pub kind: StrikeKind,
    /// Durable evidence coordinates and digest.
    pub evidence_ref: StrikeEvidenceRef,
    /// The exact rules build that adjudicated the evidence.
    pub ruleset: RulesetId,
    /// Shadow rows are retained for calibration but never affect standing.
    pub mode: StrikeMode,
    /// Hard deletion deadline, exactly 90 days after issuance.
    pub expires_at_ms: u64,
}

// The `strike/` key builders live in `keyspace`, with every other key
// builder in this crate. `keyspace`'s registry guard scans that module's own
// source for constructors, so a builder outside it registers a sub-kind the
// guard cannot see written — which is exactly what it reported when these
// lived here.
pub use crate::keyspace::{
    strike_account_range_end, strike_account_range_start, strike_episode_key, strike_key,
    strike_range_end, strike_range_start, strike_versionstamped_key, STRIKE_VERSIONSTAMP_OFFSET,
};

/// Result of attempting to append one verdict-derived fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrikeFileOutcome {
    /// One new row was appended.
    Filed {
        /// Account selected by the verdict-specific subject/reporter rule.
        account: AccountId,
    },
    /// The same evidence and kind already had a row; no weight was stacked.
    Duplicate {
        /// Account whose existing row made the submission idempotent.
        account: AccountId,
    },
    /// D31's reverse binding did not resolve; fail closed and file nothing.
    UnresolvedBinding,
}

/// A strike-ledger write failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrikeLedgerError(String);

impl fmt::Display for StrikeLedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StrikeLedgerError {}

/// The adjudication executor's append seam.
pub trait StrikeLedger: Send + Sync {
    /// Resolve `target` through its D31 binding at `offence` and append
    /// `row` once.
    ///
    /// `offence` is an [`OffenceTime`], not a bare millisecond, because a
    /// [`Tick`] is not a Unix millisecond: comparing the two would silently
    /// turn a rebinding into a misattribution. A caller with no authenticated
    /// instant passes [`OffenceTime::Unknown`] and is refused rather than
    /// misattributed.
    ///
    /// `episode` is written to a separate durable dedup index, leaving
    /// immutable D33 row bytes backward-readable.
    fn file(
        &self,
        target: NodeId,
        offence: OffenceTime,
        row: &StrikeRow,
        episode: Option<&StrikeEpisodeRef>,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError>;
}

/// In-memory strike ledger for tests and harnesses.
#[derive(Debug, Default)]
pub struct MemStrikeLedger {
    state: Mutex<MemStrikeState>,
}

#[derive(Debug, Default)]
struct MemStrikeState {
    bindings: HashMap<NodeId, crate::keyspace::BindingRow>,
    history: HashMap<NodeId, Vec<crate::keyspace::BindingHistoryRow>>,
    rows: HashMap<AccountId, Vec<StrikeRow>>,
    episodes: HashMap<AccountId, HashMap<[u8; 32], u64>>,
}

impl MemStrikeLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one current D31 binding for a harness.
    pub fn bind(&self, node: NodeId, account: AccountId) {
        self.bind_at(node, account, 0);
    }

    /// Append a binding event at a known wall-clock instant for a harness.
    pub fn bind_at(&self, node: NodeId, account: AccountId, at_ms: u64) {
        let mut state = Self::lock(&self.state);
        state.bindings.insert(
            node,
            crate::keyspace::BindingRow {
                account,
                bound_at_ms: at_ms,
            },
        );
        state
            .history
            .entry(node)
            .or_default()
            .push(crate::keyspace::BindingHistoryRow {
                account,
                kind: crate::keyspace::BindKind::Bind,
                at_ms,
            });
    }

    /// Append an unbinding event for a harness.
    pub fn unbind_at(&self, node: NodeId, account: AccountId, at_ms: u64) {
        let mut state = Self::lock(&self.state);
        state.bindings.remove(&node);
        state
            .history
            .entry(node)
            .or_default()
            .push(crate::keyspace::BindingHistoryRow {
                account,
                kind: crate::keyspace::BindKind::Unbind,
                at_ms,
            });
    }

    /// Read one account's filed facts in append order.
    #[must_use]
    pub fn rows(&self, account: AccountId) -> Vec<StrikeRow> {
        Self::lock(&self.state)
            .rows
            .get(&account)
            .cloned()
            .unwrap_or_default()
    }

    /// Hard-delete retained facts whose carried deadline has passed.
    pub fn sweep_expired(&self, now_ms: u64) -> usize {
        let mut state = Self::lock(&self.state);
        let mut removed = 0;
        for rows in state.rows.values_mut() {
            let before = rows.len();
            rows.retain(|row| row.expires_at_ms > now_ms);
            removed += before - rows.len();
        }
        removed
    }

    fn lock(state: &Mutex<MemStrikeState>) -> MutexGuard<'_, MemStrikeState> {
        state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl StrikeLedger for MemStrikeLedger {
    fn file(
        &self,
        target: NodeId,
        offence: OffenceTime,
        row: &StrikeRow,
        episode: Option<&StrikeEpisodeRef>,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        let mut state = Self::lock(&self.state);
        let account = binding_account_at(
            state.bindings.get(&target),
            state
                .history
                .get(&target)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            offence,
        );
        let Some(account) = account else {
            return Ok(StrikeFileOutcome::UnresolvedBinding);
        };
        if episode.is_none()
            && state.rows.get(&account).is_some_and(|rows| {
                rows.iter().any(|existing| {
                    existing.evidence_ref.digest == row.evidence_ref.digest
                        && existing.kind == row.kind
                })
            })
        {
            return Ok(StrikeFileOutcome::Duplicate { account });
        }
        if let Some(episode) = episode {
            let key = episode_dedup_digest(row.ruleset, episode);
            let episodes = state.episodes.entry(account).or_default();
            if episodes
                .get(&key)
                .is_some_and(|expires_at_ms| *expires_at_ms > row.issued_at_ms)
            {
                return Ok(StrikeFileOutcome::Duplicate { account });
            }
            episodes.insert(key, row.expires_at_ms);
        }
        state.rows.entry(account).or_default().push(row.clone());
        Ok(StrikeFileOutcome::Filed { account })
    }
}

/// When an offence happened, on identity's Unix-millisecond binding clock.
///
/// D31 binding events are wall-clock addressed, while signed evidence is
/// tick-addressed: an [`EvidenceBundle`] carries `window_start`, `window_end`
/// and a `chain_epoch`, and no field of it — nor any durable row in this
/// crate — maps a chain epoch onto a wall clock. There is therefore no
/// authenticated offence instant to be had from evidence alone, and this
/// enum makes that absence explicit instead of letting a fabricated
/// projection masquerade as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffenceTime {
    /// An instant a caller can actually authenticate for this evidence.
    KnownMs(u64),
    /// No authenticated instant exists for this evidence.
    ///
    /// Attribution then refuses to guess: it demands that the node have had
    /// exactly one owner across its whole recorded binding history. Any
    /// rebinding makes the offender unknowable, and the strike is refused
    /// rather than landed on whoever happens to hold the binding now.
    Unknown,
}

/// Resolve D31's append-only binding facts to the account that earned a
/// strike, never merely the account that holds the node today.
///
/// Binding history is the authority whenever present. The current `db` row is
/// only a migration fallback for a legacy node with no `dh` events yet, and
/// under [`OffenceTime::KnownMs`] its `bound_at_ms` still prevents assigning
/// an earlier offence to a later owner. Equal timestamps retain commit order
/// from the history scan.
fn binding_account_at(
    current: Option<&crate::keyspace::BindingRow>,
    history: &[crate::keyspace::BindingHistoryRow],
    offence: OffenceTime,
) -> Option<AccountId> {
    let offence_at_ms = match offence {
        OffenceTime::KnownMs(at_ms) => at_ms,
        // Without an instant, one continuously-owned node is the only case
        // where the offender is knowable. Two distinct owners in the history
        // means the strike could belong to either, so it belongs to neither.
        OffenceTime::Unknown => {
            let current = current?;
            let mut sole_owner = Some(current.account);
            for event in history {
                if event.account != current.account {
                    sole_owner = None;
                    break;
                }
            }
            return sole_owner;
        }
    };

    if history.is_empty() {
        return current
            .and_then(|binding| (binding.bound_at_ms <= offence_at_ms).then_some(binding.account));
    }

    let mut account = None;
    for event in history {
        if event.at_ms > offence_at_ms {
            continue;
        }
        match event.kind {
            crate::keyspace::BindKind::Bind => account = Some(event.account),
            crate::keyspace::BindKind::Unbind if account == Some(event.account) => {
                account = None;
            }
            crate::keyspace::BindKind::Unbind => {}
        }
    }
    account
}

/// Strike-filing counters, separate from verdict delivery counters.
#[derive(Debug, Default)]
pub struct StrikeMetrics {
    filed_subject: AtomicU64,
    filed_reporter: AtomicU64,
    duplicate: AtomicU64,
    suppressed_off: AtomicU64,
    suppressed_non_striking: AtomicU64,
    suppressed_unresolved: AtomicU64,
    suppressed_error: AtomicU64,
}

/// Point-in-time [`StrikeMetrics`] read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrikeMetricsSnapshot {
    /// Confirmed deviations filed against subjects.
    pub filed_subject: u64,
    /// Fabricated-evidence findings filed against reporters.
    pub filed_reporter: u64,
    /// Idempotent replays that found the row already present.
    pub duplicate: u64,
    /// Striking verdicts suppressed because C5 was off.
    pub suppressed_off: u64,
    /// Exonerations and unadjudicable outcomes, which never file.
    pub suppressed_non_striking: u64,
    /// Striking verdicts whose selected NodeId had no D31 binding.
    pub suppressed_unresolved: u64,
    /// Durable filing failures; verdict delivery remains unchanged.
    pub suppressed_error: u64,
}

impl StrikeMetrics {
    /// Read every filing counter.
    #[must_use]
    pub fn snapshot(&self) -> StrikeMetricsSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        StrikeMetricsSnapshot {
            filed_subject: load(&self.filed_subject),
            filed_reporter: load(&self.filed_reporter),
            duplicate: load(&self.duplicate),
            suppressed_off: load(&self.suppressed_off),
            suppressed_non_striking: load(&self.suppressed_non_striking),
            suppressed_unresolved: load(&self.suppressed_unresolved),
            suppressed_error: load(&self.suppressed_error),
        }
    }
}

#[derive(Clone)]
struct StrikeFiler {
    ledger: Arc<dyn StrikeLedger>,
    mode: StrikeMode,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

/// One registered rules build, boxed so the executor is not generic over a
/// single `Ruleset`.
///
/// A cluster serves many games' worth of history across a rules upgrade, and
/// making the executor generic would force one binary per build. The closure
/// captures the concrete build and hands back a verdict.
type Worker =
    Box<dyn Fn(NodeId, &orrery_protocol::EvidenceBundle) -> AdjudicationOutcome + Send + Sync>;

/// Canonical state produced by the cluster's replay of a guilty window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjudicatedState {
    /// The last executed tick represented by `canonical`.
    pub tick: Tick,
    /// The `Ruleset::CoreState` canonical encoding at `tick`.
    pub canonical: Vec<u8>,
}

/// A verdict together with state usable by D10's correction response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjudicationOutcome {
    /// The ordinary stage-4 verdict.
    pub verdict: Verdict,
    /// Replayed state, present only for a confirmed deviation whose replay
    /// completed all the way through the evidence window.
    pub corrected: Option<AdjudicatedState>,
}

struct Registered {
    id: RulesetId,
    worker: Worker,
}

/// Routes reports to the matching rules build and re-runs the window.
pub struct AdjudicationExecutor {
    seed: UniverseSeed,
    /// Newest last. Bounded at [`RETAINED_BUILDS`]; registering a fourth
    /// retires the oldest, which is what makes `UnknownRuleset` reachable and
    /// therefore worth testing.
    builds: VecDeque<Registered>,
    strike_filer: Option<StrikeFiler>,
    strike_metrics: StrikeMetrics,
    strike_ramp_meter: Option<Arc<RampMeter>>,
}

impl AdjudicationExecutor {
    /// An executor for one universe, with no builds registered yet.
    #[must_use]
    pub fn new(seed: UniverseSeed) -> Self {
        Self {
            seed,
            builds: VecDeque::new(),
            strike_filer: None,
            strike_metrics: StrikeMetrics::default(),
            strike_ramp_meter: None,
        }
    }

    /// Configure D32 C5 filing. An unconfigured executor is C5 `off`.
    #[must_use]
    pub fn with_strike_ledger(mut self, ledger: Arc<dyn StrikeLedger>, mode: StrikeMode) -> Self {
        self.strike_filer = Some(StrikeFiler {
            ledger,
            mode,
            clock: Arc::new(system_time_ms),
        });
        self
    }

    /// Attach D32 clause (e)'s C5 meter.
    ///
    /// The cohort is deliberately not stored here: as with C1, membership is
    /// handed to [`RampMeter::snapshot`] by the artifact producer. This seam
    /// records only shadow-stamped filing attempts, never live actions.
    #[must_use]
    pub fn with_strike_ramp_meter(mut self, meter: Arc<RampMeter>) -> Self {
        self.strike_ramp_meter = Some(meter);
        self
    }

    /// Read the filing counters without mixing them into verdict counters.
    #[must_use]
    pub fn strike_metrics(&self) -> StrikeMetricsSnapshot {
        self.strike_metrics.snapshot()
    }

    /// Register a rules build as adjudicable, retiring the oldest past the cap.
    ///
    /// `factory` is called per report rather than once, because a replay needs
    /// its own executor and a `Ruleset` is a cheap pure value — cheaper than
    /// requiring `Clone` from every game.
    pub fn register<R: Ruleset>(&mut self, factory: fn() -> R) {
        let id = factory().id();
        let seed = self.seed;
        let worker: Worker =
            Box::new(move |authority, bundle| adjudicate_bundle(factory, seed, authority, bundle));
        self.builds.retain(|registered| registered.id != id);
        self.builds.push_back(Registered { id, worker });
        while self.builds.len() > RETAINED_BUILDS {
            self.builds.pop_front();
        }
    }

    /// The builds currently adjudicable, oldest first.
    pub fn retained(&self) -> impl Iterator<Item = RulesetId> + '_ {
        self.builds.iter().map(|registered| registered.id)
    }

    /// Adjudicate one report.
    ///
    /// The reporter's signature is checked first and separately: a report
    /// nobody signed cannot be attributed to an account, and attribution is the
    /// only thing that makes `EvidenceForged` actionable. That check is *not*
    /// a judgement of the evidence — a well-formed accusation and a false one
    /// have to stay distinguishable.
    #[must_use]
    pub fn adjudicate(&self, report: &DiscrepancyReport) -> Verdict {
        self.adjudicate_outcome(report).verdict
    }

    /// Adjudicate one report whose evidence window has been projected onto
    /// identity's Unix-millisecond clock.
    ///
    /// Callers that configure a strike ledger must use this form: the signed
    /// evidence is tick-addressed, while D31 binding events are wall-clock
    /// addressed. The projection belongs at the coordinator-epoch boundary,
    /// not inside the ledger where a tick would otherwise masquerade as ms.
    #[must_use]
    pub fn adjudicate_at(&self, report: &DiscrepancyReport, offence: OffenceTime) -> Verdict {
        self.adjudicate_outcome_at(report, offence).verdict
    }

    /// Adjudicate and retain the replayed state a guilty response needs.
    ///
    /// If C5 is configured, filing uses its required evidence-time projection
    /// and happens exactly once. [`Self::adjudicate_outcome_at`] is available
    /// to a caller that has already computed the projection for this report.
    #[must_use]
    pub fn adjudicate_outcome(&self, report: &DiscrepancyReport) -> AdjudicationOutcome {
        let outcome = self.evaluate_outcome(report);
        // Signed evidence carries no authenticated wall clock, so the default
        // adjudication path cannot name the offence instant. It says so,
        // rather than substituting "now" — which is precisely the current-
        // binding misattribution this seam exists to prevent.
        self.file_report_verdict_at(report, outcome.verdict, OffenceTime::Unknown);
        outcome
    }

    fn evaluate_outcome(&self, report: &DiscrepancyReport) -> AdjudicationOutcome {
        if orrery_witness::verify_report(report).is_err() {
            // Unsigned or tampered-in-transit. Not `EvidenceForged`: that
            // verdict strikes the named reporter, and an unverifiable
            // signature is exactly the case where the name means nothing.
            AdjudicationOutcome {
                verdict: Verdict::Unadjudicable(UnadjudicableReason::Malformed),
                corrected: None,
            }
        } else if let Some(registered) = self
            .builds
            .iter()
            .find(|registered| registered.id == report.bundle.ruleset)
        {
            (registered.worker)(report.subject, &report.bundle)
        } else {
            AdjudicationOutcome {
                verdict: Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset),
                corrected: None,
            }
        }
    }

    /// Adjudicate and file against the account bound when the evidence window
    /// occurred, rather than the account bound when replay completed.
    #[must_use]
    pub fn adjudicate_outcome_at(
        &self,
        report: &DiscrepancyReport,
        offence: OffenceTime,
    ) -> AdjudicationOutcome {
        let outcome = self.evaluate_outcome(report);
        self.file_report_verdict_at(report, outcome.verdict, offence);
        outcome
    }

    #[cfg(test)]
    fn file_report_verdict(&self, report: &DiscrepancyReport, verdict: Verdict) {
        self.file_report_verdict_at(report, verdict, OffenceTime::Unknown);
    }

    fn file_report_verdict_at(
        &self,
        report: &DiscrepancyReport,
        verdict: Verdict,
        offence: OffenceTime,
    ) {
        let (target, kind, filed_counter, episode) = match verdict {
            Verdict::Confirms { at, .. } => (
                report.subject,
                StrikeKind::Deviation,
                &self.strike_metrics.filed_subject,
                Some(StrikeEpisodeRef {
                    subject: report.subject,
                    chain_epoch: report.bundle.t0_claim.chain_epoch,
                    first_diverging_tick: at,
                }),
            ),
            Verdict::EvidenceForged(_) => (
                report.reporter,
                StrikeKind::EvidenceForged,
                &self.strike_metrics.filed_reporter,
                None,
            ),
            Verdict::Exonerates | Verdict::Unadjudicable(_) => {
                self.strike_metrics
                    .suppressed_non_striking
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let Some(filer) = &self.strike_filer else {
            self.strike_metrics
                .suppressed_off
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        let issued_at_ms = (filer.clock)();
        let row = StrikeRow {
            issued_at_ms,
            weight_milli: MAJOR_STRIKE_WEIGHT_MILLI,
            kind,
            evidence_ref: evidence_ref(report),
            ruleset: report.bundle.ruleset,
            mode: filer.mode,
            expires_at_ms: issued_at_ms.saturating_add(STRIKE_RETENTION_MS),
        };
        match filer.ledger.file(target, offence, &row, episode.as_ref()) {
            Ok(StrikeFileOutcome::Filed { account }) => {
                filed_counter.fetch_add(1, Ordering::Relaxed);
                self.record_strike_ramp(
                    filer.mode,
                    Some(account),
                    "strike_filed",
                    Some(kind.as_str()),
                    issued_at_ms,
                    report.bundle.ruleset,
                );
            }
            Ok(StrikeFileOutcome::Duplicate { account }) => {
                self.strike_metrics
                    .duplicate
                    .fetch_add(1, Ordering::Relaxed);
                self.record_strike_ramp(
                    filer.mode,
                    Some(account),
                    "duplicate",
                    None,
                    issued_at_ms,
                    report.bundle.ruleset,
                );
            }
            Ok(StrikeFileOutcome::UnresolvedBinding) => {
                self.strike_metrics
                    .suppressed_unresolved
                    .fetch_add(1, Ordering::Relaxed);
                self.record_strike_ramp_unevaluated(
                    filer.mode,
                    "unresolved_binding",
                    issued_at_ms,
                    report.bundle.ruleset,
                );
            }
            Err(error) => {
                self.strike_metrics
                    .suppressed_error
                    .fetch_add(1, Ordering::Relaxed);
                self.record_strike_ramp_unevaluated(
                    filer.mode,
                    "strike_filing_error",
                    issued_at_ms,
                    report.bundle.ruleset,
                );
                tracing::warn!(%error, ?kind, "strike filing failed; verdict remains deliverable");
            }
        }
    }

    fn record_strike_ramp(
        &self,
        mode: StrikeMode,
        account: Option<AccountId>,
        verdict: &'static str,
        action: Option<&'static str>,
        observed_at_ms: u64,
        ruleset: RulesetId,
    ) {
        if mode != StrikeMode::Shadow {
            return;
        }
        let Some(meter) = self.strike_ramp_meter.as_deref() else {
            return;
        };
        meter.qualify(account);
        meter.observe(account, verdict, action, observed_at_ms);
        // D32 open question 6, resolved 2026-09-03: the window stamps the
        // rulesets it observed instead of resetting on a change, so a
        // reviewer sees a window that spanned one and judges it. The row the
        // control filed names the ruleset; the stamp rides the metering
        // entry beside the denominator.
        meter.observe_ruleset(ruleset);
    }

    fn record_strike_ramp_unevaluated(
        &self,
        mode: StrikeMode,
        reason: &'static str,
        observed_at_ms: u64,
        ruleset: RulesetId,
    ) {
        if mode != StrikeMode::Shadow {
            return;
        }
        let Some(meter) = self.strike_ramp_meter.as_deref() else {
            return;
        };
        meter.qualify(None);
        meter.observe_unevaluated(None, reason, observed_at_ms);
        meter.observe_ruleset(ruleset);
    }

    /// Append D33's executor-authorized compensating fact for an upheld
    /// appeal. The original fact remains immutable; this new `Appeal` row
    /// references its evidence and negates exactly its original weight.
    ///
    /// `offence` must be the same [`OffenceTime`] used when the original was
    /// filed, so a later NodeId rebind cannot redirect the reversal to a new
    /// account owner. Passing the original's own resolution is what makes the
    /// compensating fact land on the account that carries the conviction.
    pub fn uphold_appeal(
        &self,
        target: NodeId,
        offence: OffenceTime,
        appealed: &StrikeRow,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        if appealed.kind == StrikeKind::Appeal || appealed.weight_milli <= 0 {
            return Err(StrikeLedgerError(
                "an appeal must compensate one positive, non-appeal strike".into(),
            ));
        }
        let Some(filer) = &self.strike_filer else {
            return Err(StrikeLedgerError(
                "cannot uphold an appeal while strike filing is off".into(),
            ));
        };
        let issued_at_ms = (filer.clock)();
        let row = StrikeRow {
            issued_at_ms,
            weight_milli: -appealed.weight_milli,
            kind: StrikeKind::Appeal,
            evidence_ref: appealed.evidence_ref.clone(),
            ruleset: appealed.ruleset,
            mode: appealed.mode,
            expires_at_ms: issued_at_ms.saturating_add(STRIKE_RETENTION_MS),
        };
        // No episode key: an appeal is deduplicated by (evidence digest,
        // `Appeal`) instead, so upholding the same appeal twice cannot credit
        // the appellant twice.
        filer.ledger.file(target, offence, &row, None)
    }

    /// File the D33 clause (a) non-cooperation tier after the executor has
    /// established the existing proof threshold. This is a separate producer
    /// because a replay `Verdict` does not represent a log-gap finding.
    pub fn file_non_cooperation(
        &self,
        target: NodeId,
        offence: OffenceTime,
        evidence_ref: StrikeEvidenceRef,
        ruleset: RulesetId,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        self.file_executor_finding(
            target,
            offence,
            StrikeKind::NonCooperation,
            NON_COOPERATION_WEIGHT_MILLI,
            evidence_ref,
            ruleset,
        )
    }

    fn file_executor_finding(
        &self,
        target: NodeId,
        offence: OffenceTime,
        kind: StrikeKind,
        weight_milli: i32,
        evidence_ref: StrikeEvidenceRef,
        ruleset: RulesetId,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        let Some(filer) = &self.strike_filer else {
            return Err(StrikeLedgerError(
                "cannot file an executor finding while strike filing is off".into(),
            ));
        };
        let issued_at_ms = (filer.clock)();
        filer.ledger.file(
            target,
            offence,
            &StrikeRow {
                issued_at_ms,
                weight_milli,
                kind,
                evidence_ref,
                ruleset,
                mode: filer.mode,
                expires_at_ms: issued_at_ms.saturating_add(STRIKE_RETENTION_MS),
            },
            // An executor finding is not a replay divergence episode; its
            // dedup identity is (evidence digest, kind).
            None,
        )
    }

    #[cfg(test)]
    fn with_strike_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        if let Some(filer) = &mut self.strike_filer {
            filer.clock = Arc::new(clock);
        }
        self
    }

    /// D29 clause 7's second entry point: finalize one provisional intent by
    /// spot replay.
    ///
    /// # Why this cannot be `adjudicate` with a synthesised report
    ///
    /// [`Self::adjudicate`] opens by verifying the *reporter's* signature, and
    /// that check has no meaning here: a provisional finalization has no
    /// reporter and no accusation. There is nobody claiming anything — the
    /// cluster is checking its own outstanding work. Wrapping the bundle in a
    /// [`DiscrepancyReport`] would mean the cluster signing an accusation
    /// against a player in order to satisfy a check it wrote for a different
    /// situation, which is a worse fiction than a second entry point.
    ///
    /// What *is* shared is the executor itself, and deliberately:
    /// [`RETAINED_BUILDS`] is the scarce resource, and two registries would
    /// give two answers to "which build adjudicates this window" — the exact
    /// failure the version-keyed routing exists to prevent.
    ///
    /// # The commitment check comes first
    ///
    /// Before any replay, the fetched bundle must reproduce the
    /// [`EvidenceCommitment`] the intent was committed under. That is the only
    /// property clause 6 traded the attached evidence for: the submitter
    /// pinned the history before it knew what the cluster would ask, so a
    /// bundle that does not match is a substituted one. It is answered with
    /// `EvidenceForged` — and note that this verdict strikes the *submitter*
    /// here, where on the report path it protects an accused peer by striking
    /// the reporter. On this path the evidence's author and the intent's
    /// submitter are one account, which is the asymmetry D29 flags for review.
    ///
    /// # What spot replay proves, exactly
    ///
    /// That the submitter's claimed history across `[t₀, t_intent)` is
    /// self-consistent, correctly signed, chain-continuous, and reproduces its
    /// own state claims under the pinned build. It does **not** re-check the
    /// ledger invariant — that check already ran, inside the serializable
    /// transaction, and D11 keeps that transaction the sole authority over
    /// durable truth. The gap this closes is the one attestation would have
    /// closed: whether the intent was grafted onto a history nobody saw.
    ///
    /// Stating it narrowly matters, because a reader who believes finalization
    /// re-audits the economy will not build the conservation auditor that P5
    /// exit still owes.
    #[must_use]
    pub fn finalize_provisional(
        &self,
        subject: NodeId,
        commitment: &orrery_protocol::EvidenceCommitment,
        bundle: &orrery_protocol::EvidenceBundle,
        log_head: orrery_protocol::ChainHash,
    ) -> Verdict {
        if !crate::intent::provisional::commitment_matches(commitment, bundle, log_head) {
            return crate::intent::provisional::mismatch_verdict();
        }
        // Routed by the **commitment's** ruleset, not the bundle's. They are
        // equal — `commitment_matches` just proved it — and taking it from the
        // commitment is what makes that equality load-bearing rather than
        // incidental: the build that judges the window is the build the
        // submitter pinned at commit time, and no later-fetched artifact can
        // move it.
        let Some(registered) = self
            .builds
            .iter()
            .find(|registered| registered.id == commitment.ruleset)
        else {
            // Never a strike. `RETAINED_BUILDS` bounds both workloads, so an
            // intent pinning a build older than the last three is annulled
            // with nobody at fault — a new way for an honest player to lose an
            // item during a rules upgrade, and the same trade the report path
            // already accepted.
            return crate::intent::provisional::unknown_ruleset_verdict();
        };
        (registered.worker)(subject, bundle).verdict
    }
}

fn adjudicate_bundle<R: Ruleset>(
    factory: fn() -> R,
    seed: UniverseSeed,
    authority: NodeId,
    bundle: &orrery_protocol::EvidenceBundle,
) -> AdjudicationOutcome {
    let verdict = verify_bundle(factory(), seed, authority, bundle);
    let corrected = if matches!(verdict, Verdict::Confirms { .. }) {
        let mut replay = ReplayHarness::new(factory(), seed);
        replay
            .load_claimed_snapshot(&bundle.t0_claim, &bundle.t0_snapshot)
            .and_then(|()| {
                replay.replay(
                    &bundle.frames,
                    authority,
                    (bundle.window_start, bundle.window_end),
                    &bundle.sibling_heads,
                )
            })
            .ok()
            .and_then(|_| {
                bundle.window_end.0.checked_sub(1).and_then(|tick| {
                    replay.canonical_state().map(|canonical| AdjudicatedState {
                        tick: Tick::new(tick),
                        canonical,
                    })
                })
            })
    } else {
        None
    };
    AdjudicationOutcome { verdict, corrected }
}

fn system_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn evidence_ref(report: &DiscrepancyReport) -> StrikeEvidenceRef {
    let encoded = postcard::to_stdvec(report).unwrap_or_default();
    StrikeEvidenceRef {
        entity: report.bundle.entity,
        window_start: report.bundle.window_start,
        window_end: report.bundle.window_end,
        digest: *blake3::hash(&encoded).as_bytes(),
    }
}

fn episode_dedup_digest(ruleset: RulesetId, episode: &StrikeEpisodeRef) -> [u8; 32] {
    let encoded = postcard::to_stdvec(&(ruleset, episode)).expect("episode identity encodes");
    *blake3::hash(&encoded).as_bytes()
}

#[cfg(feature = "fdb")]
type RestoreHoldIndex = Arc<std::sync::RwLock<Option<NodeId>>>;

/// FoundationDB-backed executor-owned writer for D33's `ya` family.
#[cfg(feature = "fdb")]
#[derive(Clone)]
pub struct FdbStrikeLedger {
    db: Arc<foundationdb::Database>,
    restore_hold_index: RestoreHoldIndex,
}

#[cfg(feature = "fdb")]
impl FdbStrikeLedger {
    /// Open a bounded FoundationDB handle from `cluster_file`.
    pub fn connect(cluster_file: &str) -> Result<Self, StrikeLedgerError> {
        let context = crate::fdb::FdbContext::connect(cluster_file)
            .map_err(|error| StrikeLedgerError(error.to_string()))?;
        Ok(Self::from_database(context.database()))
    }

    /// Reuse a process-scoped, bounded database handle.
    #[must_use]
    pub fn from_database(db: Arc<foundationdb::Database>) -> Self {
        Self {
            db,
            restore_hold_index: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Attach the archive source whose restores this ledger's products hold.
    pub fn configure_restore_hold_index(&self, source_node: NodeId) {
        *self
            .restore_hold_index
            .write()
            .expect("restore-hold index lock poisoned") = Some(source_node);
    }

    /// Read one account's ledger rows in commit order.
    pub async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, StrikeLedgerError> {
        read_strike_rows(Arc::clone(&self.db), account).await
    }

    /// Delete every expired D33 product found in one off-path maintenance
    /// pass, and return how many keys were cleared.
    ///
    /// The scorer already excludes an expired row before this task reaches
    /// it, but that is not retention: this pass is what makes expiry a hard
    /// delete rather than unbounded dead data.
    ///
    /// All three families a filing writes are swept together, because two of
    /// them are indexes *into* the `ya` row and outliving it would be worse
    /// than never having been written. A `yc` restore hold in particular
    /// holds an entity's restore for as long as an adjudication product is
    /// retained, so an orphaned hold would block restore forever on the
    /// strength of a strike retention has already erased.
    ///
    /// The scan is in key order; production cadence and paging belong to the
    /// maintenance runner.
    pub async fn sweep_expired(&self, now_ms: u64) -> Result<usize, StrikeLedgerError> {
        use futures::TryStreamExt;

        async fn scan(
            trx: &foundationdb::RetryableTransaction,
            start: &[u8],
            end: &[u8],
        ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, foundationdb::FdbBindingError> {
            let mut stream = trx.get_ranges_keyvalues(
                foundationdb::RangeOption {
                    begin: foundationdb::KeySelector::first_greater_or_equal(start),
                    end: foundationdb::KeySelector::first_greater_or_equal(end),
                    ..foundationdb::RangeOption::default()
                },
                false,
            );
            let mut found = Vec::new();
            while let Some(kv) = stream.try_next().await? {
                found.push((kv.key().to_vec(), kv.value().to_vec()));
            }
            Ok(found)
        }

        self.db
            .run(|trx, _| async move {
                let mut doomed: Vec<Vec<u8>> = Vec::new();
                // `ya`: the strike rows themselves, and the identity of each
                // erased row, so its indexes can be found below.
                let mut erased = std::collections::HashSet::new();
                for (key, value) in scan(&trx, &strike_range_start(), &strike_range_end()).await? {
                    let row: StrikeRow = postcard::from_bytes(&value).map_err(|error| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(
                            StrikeLedgerError(format!("strike row decode: {error}")),
                        ))
                    })?;
                    if row.expires_at_ms <= now_ms {
                        if key.len() == 20 {
                            erased.insert((
                                AccountId::new(u64::from_be_bytes(
                                    key[2..10].try_into().expect("8 bytes"),
                                )),
                                <[u8; 10]>::try_from(&key[10..20]).expect("10 bytes"),
                            ));
                        }
                        doomed.push(key);
                    }
                }
                // `yb`: the episode-dedup marker, whose value is its own
                // expiry.
                for (key, value) in scan(
                    &trx,
                    &crate::keyspace::strike_episode_range_start(),
                    &crate::keyspace::strike_episode_range_end(),
                )
                .await?
                {
                    let expires_at_ms =
                        u64::from_be_bytes(value.as_slice().try_into().map_err(|_| {
                            foundationdb::FdbBindingError::new_custom_error(Box::new(
                                StrikeLedgerError(
                                    "strike episode index expiry has invalid width".into(),
                                ),
                            ))
                        })?);
                    if expires_at_ms <= now_ms {
                        doomed.push(key);
                    }
                }
                // `yc`: restore holds naming a strike this pass erased.
                if !erased.is_empty() {
                    for (key, _) in scan(
                        &trx,
                        &crate::keyspace::restore_hold_family_range_start(),
                        &crate::keyspace::restore_hold_family_range_end(),
                    )
                    .await?
                    {
                        if let Some((
                            _,
                            _,
                            crate::keyspace::RestoreHoldProduct::Strike {
                                account,
                                versionstamp,
                            },
                        )) = crate::keyspace::decode_restore_hold_key(&key)
                        {
                            if erased.contains(&(account, versionstamp)) {
                                doomed.push(key);
                            }
                        }
                    }
                }
                for key in &doomed {
                    trx.clear(key);
                }
                Ok(doomed.len())
            })
            .await
            .map_err(strike_fdb_error)
    }
}

#[cfg(feature = "fdb")]
fn strike_fdb_error(error: foundationdb::FdbBindingError) -> StrikeLedgerError {
    if let Some(fdb_error) = error.get_fdb_error() {
        return StrikeLedgerError(format!("fdb {}: {}", fdb_error.code(), fdb_error.message()));
    }
    StrikeLedgerError(format!("{error:?}"))
}

#[cfg(feature = "fdb")]
async fn read_strike_rows(
    db: Arc<foundationdb::Database>,
    account: AccountId,
) -> Result<Vec<StrikeRow>, StrikeLedgerError> {
    use futures::TryStreamExt;

    let start = strike_account_range_start(account);
    let end = strike_account_range_end(account);
    db.run(|trx, _| {
        let start = start.clone();
        let end = end.clone();
        async move {
            let mut stream = trx.get_ranges_keyvalues(
                foundationdb::RangeOption {
                    begin: foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                    end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                    ..foundationdb::RangeOption::default()
                },
                false,
            );
            let mut rows = Vec::new();
            while let Some(kv) = stream.try_next().await? {
                let row = postcard::from_bytes(kv.value()).map_err(|error| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(StrikeLedgerError(
                        format!("strike row decode: {error}"),
                    )))
                })?;
                rows.push(row);
            }
            Ok(rows)
        }
    })
    .await
    .map_err(strike_fdb_error)
}

#[cfg(feature = "fdb")]
impl StrikeLedger for FdbStrikeLedger {
    fn file(
        &self,
        target: NodeId,
        offence: OffenceTime,
        row: &StrikeRow,
        episode: Option<&StrikeEpisodeRef>,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        use foundationdb::options::MutationType;
        use futures::TryStreamExt;

        let db = Arc::clone(&self.db);
        let row = row.clone();
        let hold_location = self
            .restore_hold_index
            .read()
            .expect("restore-hold index lock poisoned")
            .as_ref()
            .copied();
        let episode = episode.map(|episode| episode_dedup_digest(row.ruleset, episode));
        futures::executor::block_on(async move {
            db.run(|trx, _| {
                let row = row.clone();
                async move {
                    let binding_key = crate::keyspace::binding_key(&target);
                    let current = trx
                        .get(&binding_key, false)
                        .await?
                        .map(|raw| {
                            postcard::from_bytes::<crate::keyspace::BindingRow>(&raw).map_err(
                                |error| {
                                    foundationdb::FdbBindingError::new_custom_error(Box::new(
                                        StrikeLedgerError(format!("binding row decode: {error}")),
                                    ))
                                },
                            )
                        })
                        .transpose()?;
                    let history_start = crate::keyspace::binding_history_node_range_start(&target);
                    let history_end = crate::keyspace::binding_history_node_range_end(&target);
                    let mut history_stream = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                history_start.as_slice(),
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(
                                history_end.as_slice(),
                            ),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    let mut history = Vec::new();
                    while let Some(kv) = history_stream.try_next().await? {
                        history.push(postcard::from_bytes(kv.value()).map_err(|error| {
                            foundationdb::FdbBindingError::new_custom_error(Box::new(
                                StrikeLedgerError(format!("binding history row decode: {error}")),
                            ))
                        })?);
                    }
                    drop(history_stream);
                    let Some(account) = binding_account_at(current.as_ref(), &history, offence)
                    else {
                        return Ok(StrikeFileOutcome::UnresolvedBinding);
                    };
                    // The selected account is historical, never merely the
                    // owner observed while this adjudication transaction ran.
                    if let Some(episode) = episode {
                        let episode_key = strike_episode_key(account, &episode);
                        if let Some(existing_expiry) = trx.get(&episode_key, false).await? {
                            let expires_at_ms = u64::from_be_bytes(
                                existing_expiry.as_ref().try_into().map_err(|_| {
                                    foundationdb::FdbBindingError::new_custom_error(Box::new(
                                        StrikeLedgerError(
                                            "strike episode index expiry has invalid width".into(),
                                        ),
                                    ))
                                })?,
                            );
                            if expires_at_ms > row.issued_at_ms {
                                return Ok(StrikeFileOutcome::Duplicate { account });
                            }
                        }
                    }
                    let start = strike_account_range_start(account);
                    let end = strike_account_range_end(account);
                    let mut stream = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                start.as_slice(),
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    while let Some(kv) = stream.try_next().await? {
                        let existing: StrikeRow =
                            postcard::from_bytes(kv.value()).map_err(|error| {
                                foundationdb::FdbBindingError::new_custom_error(Box::new(
                                    StrikeLedgerError(format!("strike row decode: {error}")),
                                ))
                            })?;
                        if episode.is_none()
                            && existing.evidence_ref.digest == row.evidence_ref.digest
                            && existing.kind == row.kind
                        {
                            return Ok(StrikeFileOutcome::Duplicate { account });
                        }
                    }
                    drop(stream);
                    let encoded = postcard::to_stdvec(&row).map_err(|error| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(
                            StrikeLedgerError(format!("strike row encode: {error}")),
                        ))
                    })?;
                    if let Some(episode) = episode {
                        trx.set(
                            &strike_episode_key(account, &episode),
                            &row.expires_at_ms.to_be_bytes(),
                        );
                    }
                    trx.atomic_op(
                        &strike_versionstamped_key(account),
                        &encoded,
                        MutationType::SetVersionstampedKey,
                    );
                    // D33 clause (e)'s "after every live filing" half. The
                    // notice is written in the *same* transaction as the row,
                    // so a strike that exists always has a pending evaluation
                    // and a notice never names a filing that did not commit.
                    //
                    // Written for every filed row, shadow-stamped ones
                    // included, because this ledger has no mode branch and
                    // must not grow one: the mode lives in the row and the
                    // scorer is what reads it. A shadow filing therefore
                    // queues an evaluation that finds nothing, which is what
                    // shadow means.
                    trx.set(
                        &crate::keyspace::filing_notice_key(account),
                        &row.issued_at_ms.to_be_bytes(),
                    );
                    if let Some(source_node) = hold_location {
                        trx.atomic_op(
                            &crate::keyspace::restore_hold_strike_versionstamped_key(
                                &source_node,
                                row.evidence_ref.entity,
                                account,
                            ),
                            b"",
                            MutationType::SetVersionstampedKey,
                        );
                    }
                    Ok(StrikeFileOutcome::Filed { account })
                }
            })
            .await
            .map_err(strike_fdb_error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::{CodecError, CoreCodec, OrderedInputs, Quantized, StateView, StepOutput};
    use orrery_protocol::{
        ChainHash, EntitySlice, EvidenceBundle, LogFrame, PersistId, StateClaim, Tick,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Empty;

    impl CoreCodec for Empty {
        fn encode(&self, _out: &mut Vec<u8>) {}
        fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
            Ok(Self)
        }
    }

    impl Quantized for Empty {
        fn quantize(&mut self) {}
    }

    macro_rules! build {
        ($name:ident, $version:expr) => {
            struct $name;
            impl Ruleset for $name {
                const OVERFLOW_IS_CANONICAL: bool = false;
                type CoreState = Empty;
                type CoreInput = Empty;
                type CoreEvent = Empty;
                fn id(&self) -> RulesetId {
                    RulesetId {
                        version: $version,
                        digest: [$version as u8; 32],
                    }
                }
                fn step(
                    &self,
                    _view: &mut StateView<'_, Empty>,
                    _inputs: &OrderedInputs<'_, Empty>,
                    _rng: &mut orrery_core::TickRng,
                ) -> StepOutput<Empty> {
                    StepOutput::default()
                }
            }
        };
    }

    build!(V1, 1);
    build!(V2, 2);
    build!(V3, 3);
    build!(V4, 4);

    fn key(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    /// One signed, record-free frame spanning the fixture's whole window.
    fn covering_frame(subject: &iroh_base::SecretKey, ruleset: RulesetId) -> LogFrame {
        let entity = PersistId::new(1);
        let slice = EntitySlice {
            entity,
            chain_epoch: 0,
            prev_head: ChainHash::EMPTY.rolling(),
            records: Vec::new(),
            head: ChainHash::EMPTY.rolling(),
        };
        let transitions = vec![orrery_core::log::HeadTransition {
            entity,
            prev_head: ChainHash::EMPTY,
            head: ChainHash::EMPTY,
        }];
        let preimage = orrery_core::log::frame_preimage(ruleset, Tick::new(0), 1, &transitions);
        LogFrame {
            ruleset,
            first_tick: Tick::new(0),
            tick_count: 1,
            entities: vec![slice],
            sig: subject.sign(&preimage),
        }
    }

    fn bundle(ruleset: RulesetId) -> EvidenceBundle {
        bundle_with_subject(ruleset, &key(1))
    }

    /// [`bundle`] with the subject this run chose, so the durable rows the
    /// FDB filing test writes are keyed by a node no other run claims.
    fn bundle_with_subject(ruleset: RulesetId, subject: &iroh_base::SecretKey) -> EvidenceBundle {
        let mut claim = StateClaim {
            entity: PersistId::new(1),
            chain_epoch: 0,
            tick: Tick::new(0),
            input_head: ChainHash::EMPTY,
            state_hash: *blake3::hash(&[]).as_bytes(),
            prev_claim: [0; 32],
            ruleset,
            sig: subject.sign(b"x"),
        };
        orrery_core::log::sign_claim(subject, &mut claim);
        EvidenceBundle {
            ruleset,
            entity: PersistId::new(1),
            window_start: Tick::new(0),
            window_end: Tick::new(1),
            t0_claim: claim,
            t0_snapshot: bytes::Bytes::new(),
            // A real signed frame covering [0, 1). An empty `frames` is not a
            // bundle any honest witness can build — `AuthorityLog::assemble_bundle`
            // refuses it with `BundleError::IncompleteFrames` — and the
            // adjudicator now agrees, because frames withheld wholesale are the
            // #874 omission attack at its limit: every tick would replay with no
            // inputs and diverge from what the authority signed.
            frames: vec![covering_frame(subject, ruleset)],
            sibling_heads: vec![Vec::new()],
            disputed_claims: Vec::new(),
            claimed_hashes: vec![[0; 32]],
            computed_hashes: vec![[0; 32]],
        }
    }

    fn executor() -> AdjudicationExecutor {
        let mut executor = AdjudicationExecutor::new(UniverseSeed([1; 32]));
        executor.register(|| V1);
        executor.register(|| V2);
        executor.register(|| V3);
        executor
    }

    fn report() -> DiscrepancyReport {
        orrery_witness::sign_report(&key(2), key(1).public(), bundle(V1.id()))
    }

    fn filing_executor<L: StrikeLedger + 'static>(
        ledger: Arc<L>,
        mode: StrikeMode,
    ) -> AdjudicationExecutor {
        executor()
            .with_strike_ledger(ledger, mode)
            .with_strike_clock(|| 1_700_000_000_000)
    }

    #[test]
    fn only_three_builds_stay_adjudicable() {
        let mut executor = executor();
        executor.register(|| V4);
        let retained: Vec<_> = executor.retained().map(|id| id.version).collect();
        assert_eq!(retained, vec![2, 3, 4], "the oldest build retires");
    }

    #[test]
    fn a_report_for_a_retired_build_is_undecidable_not_a_strike() {
        // Rules-version skew is the cluster's gap. Striking a reporter for it
        // would punish someone for an operator's release cadence.
        let mut executor = executor();
        executor.register(|| V4);
        let report = orrery_witness::sign_report(&key(2), key(1).public(), bundle(V1.id()));
        assert_eq!(
            executor.adjudicate(&report),
            Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
        );
    }

    #[test]
    fn a_report_is_routed_to_the_build_its_subject_ran() {
        // Judging an old window under current rules would manufacture
        // deviations out of a version bump.
        let executor = executor();
        for build in [V1.id(), V2.id(), V3.id()] {
            let report = orrery_witness::sign_report(&key(2), key(1).public(), bundle(build));
            assert!(
                !matches!(
                    executor.adjudicate(&report),
                    Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
                ),
                "build {build:?} should be adjudicable"
            );
        }
    }

    #[test]
    fn confirmed_replay_returns_the_canonical_adjudicated_state() {
        let mut evidence = bundle(V1.id());
        let mut disputed = StateClaim {
            entity: evidence.entity,
            chain_epoch: 0,
            tick: Tick::new(1),
            input_head: ChainHash::EMPTY,
            // `Empty` encodes as no bytes, so this is deliberately false.
            state_hash: [9; 32],
            prev_claim: orrery_core::log::claim_hash(&evidence.t0_claim),
            ruleset: evidence.ruleset,
            sig: key(1).sign(b"placeholder"),
        };
        orrery_core::log::sign_claim(&key(1), &mut disputed);
        evidence.disputed_claims.push(disputed);

        let outcome = adjudicate_bundle(|| V1, UniverseSeed([1; 32]), key(1).public(), &evidence);

        assert!(matches!(outcome.verdict, Verdict::Confirms { .. }));
        assert_eq!(
            outcome.corrected,
            Some(AdjudicatedState {
                tick: Tick::new(0),
                canonical: Vec::new(),
            })
        );
    }

    #[test]
    fn an_unsigned_report_is_malformed_rather_than_forged() {
        // `EvidenceForged` strikes the *named* reporter, and an unverifiable
        // signature is precisely the case where that name means nothing.
        let executor = executor();
        let mut report = orrery_witness::sign_report(&key(2), key(1).public(), bundle(V1.id()));
        report.reporter = key(9).public();
        assert_eq!(
            executor.adjudicate(&report),
            Verdict::Unadjudicable(UnadjudicableReason::Malformed)
        );
    }

    #[test]
    fn re_registering_a_build_does_not_consume_a_retention_slot() {
        // A restart re-registers the same builds; if each registration ate a
        // slot, a bounce would silently retire adjudicable history.
        let mut executor = executor();
        executor.register(|| V3);
        executor.register(|| V3);
        let retained: Vec<_> = executor.retained().map(|id| id.version).collect();
        assert_eq!(retained, vec![1, 2, 3]);
    }

    #[test]
    fn confirmed_verdict_files_against_subject_and_never_reporter() {
        let report = report();
        let subject_account = AccountId(41);
        let reporter_account = AccountId(42);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, subject_account);
        ledger.bind(report.reporter, reporter_account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        executor.file_report_verdict(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
        );

        let rows = ledger.rows(subject_account);
        assert_eq!(rows.len(), 1, "the judged subject receives one fact");
        assert_eq!(rows[0].kind, StrikeKind::Deviation);
        assert_eq!(rows[0].weight_milli, MAJOR_STRIKE_WEIGHT_MILLI);
        assert_eq!(rows[0].mode, StrikeMode::Live);
        assert_eq!(
            rows[0].expires_at_ms - rows[0].issued_at_ms,
            STRIKE_RETENTION_MS
        );
        assert!(
            ledger.rows(reporter_account).is_empty(),
            "the reporter must not be punished for a confirmed report"
        );
        assert_eq!(executor.strike_metrics().filed_subject, 1);
        assert_eq!(executor.strike_metrics().filed_reporter, 0);
    }

    #[test]
    fn c5_meter_attributes_shadow_filing_to_reporter_and_never_subject() {
        let report = report();
        let subject_account = AccountId(51);
        let reporter_account = AccountId(52);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, subject_account);
        ledger.bind(report.reporter, reporter_account);
        let meter = Arc::new(RampMeter::new("strikes"));
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Shadow)
            .with_strike_ramp_meter(Arc::clone(&meter));

        executor.file_report_verdict(
            &report,
            Verdict::EvidenceForged(orrery_protocol::ForgeryProof::ClaimSignatureInvalid),
        );

        assert!(
            ledger.rows(subject_account).is_empty(),
            "fabricated evidence exonerates the accused from this filing"
        );
        let rows = ledger.rows(reporter_account);
        assert_eq!(rows.len(), 1, "the attributable reporter receives the fact");
        assert_eq!(rows[0].kind, StrikeKind::EvidenceForged);
        assert_eq!(rows[0].mode, StrikeMode::Shadow);
        assert_eq!(executor.strike_metrics().filed_reporter, 1);
        assert_eq!(executor.strike_metrics().filed_subject, 0);
        let mut cohort = crate::intent::HonestCohort::new();
        cohort.arm(reporter_account);
        let ramp = meter.snapshot(&cohort);
        assert_eq!(ramp.qualifying, 1);
        assert_eq!(ramp.observed, 1);
        assert_eq!(ramp.would_act, 1);
        assert_eq!(ramp.by_cause.get("evidence_forged"), Some(&1));
        assert_eq!(ramp.cohort.fp_count, 1);
        assert_eq!(ramp.cohort.coverage, Some(1.0));
    }

    #[test]
    fn c5_meter_counts_zero_for_non_striking_verdicts() {
        let report = report();
        let subject_account = AccountId(61);
        let reporter_account = AccountId(62);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, subject_account);
        ledger.bind(report.reporter, reporter_account);
        let meter = Arc::new(RampMeter::new("strikes"));
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Shadow)
            .with_strike_ramp_meter(Arc::clone(&meter));

        for verdict in [
            Verdict::Exonerates,
            Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset),
            Verdict::Unadjudicable(UnadjudicableReason::Malformed),
        ] {
            executor.file_report_verdict(&report, verdict);
        }

        assert!(ledger.rows(subject_account).is_empty());
        assert!(ledger.rows(reporter_account).is_empty());
        assert_eq!(executor.strike_metrics().suppressed_non_striking, 3);
        let mut cohort = crate::intent::HonestCohort::new();
        cohort.arm(subject_account);
        cohort.arm(reporter_account);
        let ramp = meter.snapshot(&cohort);
        assert_eq!(ramp.qualifying, 0);
        assert_eq!(ramp.observed, 0);
        assert_eq!(ramp.would_act, 0);
        assert_eq!(ramp.cohort.fp_count, 0);
        assert_eq!(ramp.cohort.coverage, None);
    }

    /// The armed/natural split is what lets a promotion reviewer tell "would
    /// have refused 40 honest players" from "would have refused 40 cheats", so
    /// the C5 meter must score would-be filings against *either* half of the
    /// cohort — an armed-honest subject and a naturally-honest reporter alike
    /// — and none against an account outside it. A counter that only scored
    /// the armed half would certify harness traffic and stay silent about the
    /// players clause (e) actually protects.
    #[test]
    fn c5_meter_scores_the_natural_half_of_the_cohort_too() {
        let against_armed = report();
        let against_outside =
            orrery_witness::sign_report(&key(2), key(3).public(), bundle(V1.id()));
        let armed = AccountId(81);
        let natural = AccountId(82);
        let outside = AccountId(89);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(against_armed.subject, armed);
        // An `EvidenceForged` verdict files against the *reporter*, so binding
        // the reporter into the cohort's natural half makes that filing a
        // would-be action against a natural-honest member.
        ledger.bind(against_armed.reporter, natural);
        ledger.bind(against_outside.subject, outside);
        let meter = Arc::new(RampMeter::new("strikes"));
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Shadow)
            .with_strike_ramp_meter(Arc::clone(&meter));

        executor.file_report_verdict(
            &against_armed,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
        );
        executor.file_report_verdict(
            &against_armed,
            Verdict::EvidenceForged(orrery_protocol::ForgeryProof::ClaimSignatureInvalid),
        );
        executor.file_report_verdict(
            &against_outside,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
        );

        assert_eq!(ledger.rows(armed).len(), 1);
        assert_eq!(ledger.rows(natural).len(), 1);
        assert_eq!(ledger.rows(outside).len(), 1);

        let mut cohort = crate::intent::HonestCohort::new();
        cohort.arm(armed);
        cohort.sample(natural);
        let ramp = meter.snapshot(&cohort);
        assert_eq!(ramp.cohort.armed, 1, "the halves are reported separately");
        assert_eq!(ramp.cohort.natural, 1);
        assert_eq!(ramp.cohort.size, 2);
        assert_eq!(
            ramp.cohort.fp_count, 2,
            "a would-be filing against either half of H is a false positive; \
             the account outside H is not"
        );
        assert_eq!(ramp.cohort.accounts_would_act, 2);
        assert_eq!(ramp.cohort.by_cause.get("deviation"), Some(&1));
        assert_eq!(ramp.cohort.by_cause.get("evidence_forged"), Some(&1));
        assert_eq!(ramp.qualifying, 3);
        assert_eq!(
            ramp.would_act, 3,
            "the account outside H still counts fleet-wide"
        );
        assert_eq!(ramp.cohort.coverage, Some(1.0));
    }

    #[test]
    fn resubmitting_the_same_evidence_does_not_stack_weight() {
        let report = report();
        let account = AccountId(71);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);
        let verdict = Verdict::Confirms {
            at: Tick::new(1),
            kind: orrery_protocol::DeviationKind::DiscreteMismatch,
        };

        executor.file_report_verdict(&report, verdict);
        executor.file_report_verdict(&report, verdict);

        assert_eq!(ledger.rows(account).len(), 1);
        assert_eq!(executor.strike_metrics().filed_subject, 1);
        assert_eq!(executor.strike_metrics().duplicate, 1);
    }

    #[test]
    fn reslicing_one_divergence_files_one_strike() {
        // The reports have different reporter-selected bounds and therefore
        // different whole-report digests, but adjudication found the same
        // first divergence in the same signed chain epoch.
        let whole = report();
        let mut tail_bundle = whole.bundle.clone();
        tail_bundle.window_start = Tick::new(1);
        tail_bundle.window_end = Tick::new(2);
        let tail = orrery_witness::sign_report(&key(2), key(1).public(), tail_bundle);
        let first_divergence = Tick::new(7);
        assert_ne!(
            evidence_ref(&whole).digest,
            evidence_ref(&tail).digest,
            "the reporter-chosen slices still have distinct report digests"
        );

        let account = AccountId(72);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(whole.subject, account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);
        let verdict = Verdict::Confirms {
            at: first_divergence,
            kind: orrery_protocol::DeviationKind::DiscreteMismatch,
        };

        executor.file_report_verdict(&whole, verdict);
        executor.file_report_verdict(&tail, verdict);

        assert_eq!(ledger.rows(account).len(), 1, "one episode files one fact");
        assert_eq!(executor.strike_metrics().filed_subject, 1);
        assert_eq!(executor.strike_metrics().duplicate, 1);
    }

    #[test]
    fn distinct_divergence_episodes_file_distinct_strikes() {
        // Same subject, ruleset and signed chain epoch, but replay identified
        // different first-divergence ticks: these are two independent facts.
        let report = report();
        let account = AccountId(73);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        for at in [Tick::new(7), Tick::new(19)] {
            executor.file_report_verdict(
                &report,
                Verdict::Confirms {
                    at,
                    kind: orrery_protocol::DeviationKind::DiscreteMismatch,
                },
            );
        }

        assert_eq!(
            ledger.rows(account).len(),
            2,
            "distinct episodes remain visible"
        );
        assert_eq!(executor.strike_metrics().filed_subject, 2);
        assert_eq!(executor.strike_metrics().duplicate, 0);
    }

    #[test]
    fn legacy_strike_rows_stay_readable() {
        // Episode identity lives in a separate index, so the immutable D33
        // strike-row encoding remains exactly readable.
        #[derive(serde::Serialize)]
        struct LegacyEvidenceRef {
            entity: PersistId,
            window_start: Tick,
            window_end: Tick,
            digest: [u8; 32],
        }

        #[derive(serde::Serialize)]
        struct LegacyStrikeRow {
            issued_at_ms: u64,
            weight_milli: i32,
            kind: StrikeKind,
            evidence_ref: LegacyEvidenceRef,
            ruleset: RulesetId,
            mode: StrikeMode,
            expires_at_ms: u64,
        }

        let legacy = LegacyStrikeRow {
            issued_at_ms: 1,
            weight_milli: MAJOR_STRIKE_WEIGHT_MILLI,
            kind: StrikeKind::Deviation,
            evidence_ref: LegacyEvidenceRef {
                entity: PersistId::new(1),
                window_start: Tick::new(0),
                window_end: Tick::new(1),
                digest: [3; 32],
            },
            ruleset: V1.id(),
            mode: StrikeMode::Shadow,
            expires_at_ms: 2,
        };

        let encoded = postcard::to_stdvec(&legacy).expect("legacy row encodes");
        let decoded: StrikeRow = postcard::from_bytes(&encoded).expect("legacy row stays readable");
        assert_eq!(decoded.evidence_ref.digest, [3; 32]);
    }

    #[test]
    fn binding_time_attribution_keeps_a_rebound_node_from_punishing_its_new_owner() {
        let report = report();
        let original_owner = AccountId(72);
        let new_owner = AccountId(73);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind_at(report.subject, original_owner, 100);
        ledger.unbind_at(report.subject, original_owner, 200);
        ledger.bind_at(report.subject, new_owner, 300);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        executor.file_report_verdict_at(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
            OffenceTime::KnownMs(150),
        );

        assert_eq!(ledger.rows(original_owner).len(), 1);
        assert!(
            ledger.rows(new_owner).is_empty(),
            "the account bound only after the offence inherits no conviction"
        );
    }

    #[test]
    fn upheld_appeal_appends_a_negative_fact_without_rewriting_the_strike() {
        let report = report();
        let account = AccountId(74);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind_at(report.subject, account, 100);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);
        executor.file_report_verdict_at(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
            OffenceTime::KnownMs(150),
        );
        let original = ledger.rows(account).pop().expect("original strike");

        assert_eq!(
            executor
                .uphold_appeal(report.subject, OffenceTime::KnownMs(150), &original)
                .expect("executor-authorized appeal"),
            StrikeFileOutcome::Filed { account }
        );

        let rows = ledger.rows(account);
        assert_eq!(rows.len(), 2, "appeal is an additional immutable fact");
        assert_eq!(rows[0], original, "the conviction was not rewritten");
        assert_eq!(rows[1].kind, StrikeKind::Appeal);
        assert_eq!(rows[1].weight_milli, -original.weight_milli);
        assert_eq!(rows[1].evidence_ref, original.evidence_ref);
    }

    #[test]
    fn upholding_the_same_appeal_twice_credits_the_appellant_once() {
        let report = report();
        let account = AccountId(77);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind_at(report.subject, account, 100);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);
        executor.file_report_verdict_at(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
            OffenceTime::KnownMs(150),
        );
        let original = ledger.rows(account).pop().expect("original strike");
        let offence = OffenceTime::KnownMs(150);

        assert_eq!(
            executor.uphold_appeal(report.subject, offence, &original),
            Ok(StrikeFileOutcome::Filed { account })
        );
        assert_eq!(
            executor.uphold_appeal(report.subject, offence, &original),
            Ok(StrikeFileOutcome::Duplicate { account }),
            "an appeal is deduplicated by (evidence digest, Appeal)"
        );

        let rows = ledger.rows(account);
        assert_eq!(rows.len(), 2, "the reversal was credited exactly once");
        assert_eq!(
            rows.iter()
                .map(|row| i64::from(row.weight_milli))
                .sum::<i64>(),
            0,
            "the conviction is exactly cancelled, never over-credited"
        );
    }

    #[test]
    fn an_unknown_offence_instant_is_refused_rather_than_landed_on_the_new_owner() {
        // The default adjudication path has no authenticated wall clock. A
        // node that changed hands could have earned the strike under either
        // owner, so it is filed against neither.
        let report = report();
        let original_owner = AccountId(78);
        let new_owner = AccountId(79);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind_at(report.subject, original_owner, 100);
        ledger.unbind_at(report.subject, original_owner, 200);
        ledger.bind_at(report.subject, new_owner, 300);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        executor.file_report_verdict_at(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
            OffenceTime::Unknown,
        );

        assert!(
            ledger.rows(new_owner).is_empty(),
            "the current holder does not inherit an unattributable conviction"
        );
        assert!(ledger.rows(original_owner).is_empty());
        assert_eq!(executor.strike_metrics().suppressed_unresolved, 1);
    }

    #[test]
    fn an_unknown_offence_instant_still_files_against_a_sole_owner() {
        // Failing closed on ambiguity must not silently disable C5 for the
        // ordinary case of a node that never changed hands.
        let report = report();
        let account = AccountId(80);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind_at(report.subject, account, 100);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        executor.file_report_verdict_at(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
            OffenceTime::Unknown,
        );

        assert_eq!(ledger.rows(account).len(), 1);
    }

    #[test]
    fn expired_rows_are_hard_deleted_by_the_retention_sweep() {
        let report = report();
        let account = AccountId(75);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);
        executor.file_report_verdict(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
        );
        let expires_at_ms = ledger.rows(account)[0].expires_at_ms;

        assert_eq!(ledger.sweep_expired(expires_at_ms - 1), 0);
        assert_eq!(ledger.rows(account).len(), 1, "unexpired row remains");
        assert_eq!(ledger.sweep_expired(expires_at_ms), 1);
        assert!(ledger.rows(account).is_empty(), "expired row was deleted");
    }

    #[test]
    fn non_cooperation_uses_d33s_reachable_low_weight_tier() {
        let report = report();
        let account = AccountId(76);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        executor
            .file_non_cooperation(
                report.subject,
                OffenceTime::Unknown,
                evidence_ref(&report),
                report.bundle.ruleset,
            )
            .expect("proved log gap files a strike");

        let rows = ledger.rows(account);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, StrikeKind::NonCooperation);
        assert_eq!(rows[0].weight_milli, NON_COOPERATION_WEIGHT_MILLI);
        assert_ne!(rows[0].weight_milli, MAJOR_STRIKE_WEIGHT_MILLI);
    }

    #[test]
    fn unresolved_subject_binding_suppresses_instead_of_redirecting_to_reporter() {
        let report = report();
        let reporter_account = AccountId(82);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.reporter, reporter_account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

        executor.file_report_verdict(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
        );

        assert!(ledger.rows(reporter_account).is_empty());
        assert_eq!(executor.strike_metrics().suppressed_unresolved, 1);
    }

    #[test]
    fn strike_key_is_exactly_d33_ya_account_versionstamp() {
        let account = AccountId(0x0102_0304_0506_0708);
        let key = strike_key(account);
        assert_eq!(&key[..2], b"ya");
        assert_eq!(&key[2..10], &account.0.to_be_bytes());
        assert_eq!(&key[10..], &[0; 10]);
        let parameter = strike_versionstamped_key(account);
        assert_eq!(&parameter[..20], &key);
        assert_eq!(&parameter[20..], &10_u32.to_le_bytes());
        assert!(strike_account_range_start(account) < key.to_vec());
        assert!(key.to_vec() < strike_account_range_end(account));

        let episode = strike_episode_key(account, &[0xAB; 32]);
        assert_eq!(&episode[..2], b"yb");
        assert_eq!(&episode[2..10], &account.0.to_be_bytes());
        assert_eq!(&episode[10..], &[0xAB; 32]);
    }

    #[test]
    fn filing_failure_does_not_replace_the_adjudicated_verdict() {
        struct AlwaysFails;
        impl StrikeLedger for AlwaysFails {
            fn file(
                &self,
                _target: NodeId,
                _offence: OffenceTime,
                _row: &StrikeRow,
                _episode: Option<&StrikeEpisodeRef>,
            ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
                Err(StrikeLedgerError("injected write failure".into()))
            }
        }

        // The t0 claim is signed by key(1), while the report accuses key(9):
        // the report itself is valid and the evidence is provably fabricated.
        let report = orrery_witness::sign_report(&key(2), key(9).public(), bundle(V1.id()));
        let executor = executor().with_strike_ledger(Arc::new(AlwaysFails), StrikeMode::Live);
        let verdict = executor.adjudicate(&report);

        assert_eq!(
            verdict,
            Verdict::EvidenceForged(orrery_protocol::ForgeryProof::ClaimSignatureInvalid),
            "filing is a side effect; the reporter still receives adjudication's answer"
        );
        assert_eq!(executor.strike_metrics().suppressed_error, 1);
    }

    /// A node identity this run chose, not a fixed fixture.
    ///
    /// The dev cluster is shared, and a fixed fixture id turns a sibling
    /// lane's suite red — the reason the cooldown test's account id in
    /// `persistd_binary.rs` carries the pid. The seed byte names the role
    /// and the pid makes the identity this run's, so two concurrent runs
    /// claim disjoint binding rows and restore-hold spans, and a repeated
    /// run against the same cluster starts from spans its predecessor's
    /// cleanup already cleared.
    #[cfg(feature = "fdb")]
    fn run_key(role: u8) -> iroh_base::SecretKey {
        let mut bytes = [0u8; 32];
        bytes[0] = role;
        bytes[1..5].copy_from_slice(&std::process::id().to_le_bytes());
        iroh_base::SecretKey::from_bytes(&bytes)
    }

    /// The durable spans and rows one strike-filing test claims on the
    /// shared cluster, in the form both its setup and its cleanup clear.
    ///
    /// The cleanup that cleared only the `ya` span was #1000: one filing
    /// writes three families — the `ya` strike row, the `yb` episode-dedup
    /// marker, and the filing notice — and a `yb` survivor of the first
    /// run made the second run's filing deduplicate to `Duplicate`, so no
    /// `ya` row was written at all and the subject-row assertion saw zero.
    /// The claims are per-run (pid-scoped accounts, [`run_key`] nodes), so
    /// nothing a sibling run or lane wrote is ever inside them, and
    /// clearing them clears exactly what this test wrote and nothing else.
    #[cfg(feature = "fdb")]
    struct StrikeFixtureClaims {
        spans: Vec<(Vec<u8>, Vec<u8>)>,
        rows: Vec<Vec<u8>>,
    }

    #[cfg(feature = "fdb")]
    impl StrikeFixtureClaims {
        /// Everything one filing can write for two claimed accounts — the
        /// `ya` strike span, the `yb` episode-dedup span, the filing
        /// notice — plus the binding rows the filing resolves through and
        /// the restore-hold span it projects into.
        fn for_accounts(
            subject_node: &NodeId,
            reporter_node: &NodeId,
            source_node: &NodeId,
            entity: PersistId,
            subject_account: AccountId,
            reporter_account: AccountId,
        ) -> Self {
            let mut spans = Vec::new();
            let mut rows = Vec::new();
            for account in [subject_account, reporter_account] {
                spans.push((
                    crate::keyspace::strike_account_range_start(account),
                    crate::keyspace::strike_account_range_end(account),
                ));
                // The episode index is one `yb` row per (account, episode
                // digest), so the account's whole digest space is its span:
                // the all-zero digest opens it and one past the all-ff
                // digest closes it, both bounds inside this account's
                // prefix.
                let mut episodes_end =
                    crate::keyspace::strike_episode_key(account, &[0xff; 32]).to_vec();
                episodes_end.push(0);
                spans.push((
                    crate::keyspace::strike_episode_key(account, &[0; 32]).to_vec(),
                    episodes_end,
                ));
                rows.push(crate::keyspace::filing_notice_key(account).to_vec());
            }
            rows.push(crate::keyspace::binding_key(subject_node).to_vec());
            rows.push(crate::keyspace::binding_key(reporter_node).to_vec());
            spans.push((
                crate::keyspace::restore_hold_range_start(source_node, entity),
                crate::keyspace::restore_hold_range_end(source_node, entity),
            ));
            Self { spans, rows }
        }

        /// Clear every claimed span and row in one transaction.
        async fn clear(&self, db: &foundationdb::Database) {
            let spans = self.spans.clone();
            let rows = self.rows.clone();
            db.run(|trx, _| {
                let spans = spans.clone();
                let rows = rows.clone();
                async move {
                    for (start, end) in &spans {
                        trx.clear_range(start, end);
                    }
                    for row in &rows {
                        trx.clear(row);
                    }
                    Ok::<_, foundationdb::FdbBindingError>(())
                }
            })
            .await
            .expect("clear the claimed spans and rows");
        }
    }

    /// #1000's behaviour under test is D33 clause (e)'s durable half: a
    /// confirmed verdict files one strike row against the subject's
    /// account and nothing against the reporter's, and the filing projects
    /// a restore-hold index row for the strike it wrote — all through the
    /// real ledger, on a real cluster.
    #[cfg(feature = "fdb")]
    #[tokio::test]
    async fn fdb_verdict_files_subject_row_and_no_reporter_row() {
        let Some(cluster) = crate::fdb::discover_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set");
            return;
        };
        let ledger = Arc::new(FdbStrikeLedger::connect(&cluster).expect("connect strike ledger"));
        // An id in this issue's own band, with the pid above the slot: the
        // dev cluster is shared, and a fixed fixture id turns a sibling
        // lane's suite red — the same treatment the cooldown test's account
        // id gets in `persistd_binary.rs`.
        let subject_account =
            AccountId(0x0215_0000_0000_0000 | (u64::from(std::process::id()) << 4) | 1);
        let reporter_account =
            AccountId(0x0215_0000_0000_0000 | (u64::from(std::process::id()) << 4) | 2);
        let subject_key = run_key(1);
        let reporter_key = run_key(2);
        let source_node = run_key(7).public();
        let report = orrery_witness::sign_report(
            &reporter_key,
            subject_key.public(),
            bundle_with_subject(V1.id(), &subject_key),
        );
        let journal_dir = tempfile::tempdir().expect("journal tempdir");
        let journal = Arc::new(
            crate::journal::Journal::open(&crate::journal::JournalConfig {
                dir: journal_dir.path().to_path_buf(),
                ..crate::journal::JournalConfig::default()
            })
            .expect("journal opens"),
        );
        ledger.configure_restore_hold_index(source_node);
        let subject_binding = crate::keyspace::binding_key(&report.subject);
        let reporter_binding = crate::keyspace::binding_key(&report.reporter);
        let claims = StrikeFixtureClaims::for_accounts(
            &report.subject,
            &report.reporter,
            &source_node,
            report.bundle.entity,
            subject_account,
            reporter_account,
        );
        let db = Arc::clone(&ledger.db);
        let hold_start =
            crate::keyspace::restore_hold_range_start(&source_node, report.bundle.entity);
        let hold_end = crate::keyspace::restore_hold_range_end(&source_node, report.bundle.entity);
        // The pre-clear and the final cleanup are the same operation: the
        // spans this run claims, emptied, so the bindings below are written
        // into a keyspace this run owns outright.
        claims.clear(&db).await;
        db.run(|trx, _| async move {
            trx.set(
                &subject_binding,
                &postcard::to_stdvec(&crate::keyspace::BindingRow {
                    account: subject_account,
                    bound_at_ms: 1,
                })
                .expect("encode binding"),
            );
            trx.set(
                &reporter_binding,
                &postcard::to_stdvec(&crate::keyspace::BindingRow {
                    account: reporter_account,
                    bound_at_ms: 1,
                })
                .expect("encode binding"),
            );
            Ok(())
        })
        .await
        .expect("seed bindings");

        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);
        executor.file_report_verdict(
            &report,
            Verdict::Confirms {
                at: Tick::new(1),
                kind: orrery_protocol::DeviationKind::DiscreteMismatch,
            },
        );

        // The filing notice is the one claimed row nothing here asserts on,
        // and the `yd` family is drained fleet-wide by every reactor sweep
        // in the tree: retire it now, inside the same millisecond the
        // filing wrote it, rather than after the assertions, so a
        // concurrently-running suite's sweep cannot observe it.
        let notices = [
            crate::keyspace::filing_notice_key(subject_account).to_vec(),
            crate::keyspace::filing_notice_key(reporter_account).to_vec(),
        ];
        db.run(|trx, _| {
            let notices = notices.clone();
            async move {
                for notice in &notices {
                    trx.clear(notice);
                }
                Ok(())
            }
        })
        .await
        .expect("retire the filing notices");

        assert_eq!(
            ledger
                .rows(subject_account)
                .await
                .expect("subject rows")
                .len(),
            1
        );
        assert!(
            ledger
                .rows(reporter_account)
                .await
                .expect("reporter rows")
                .is_empty(),
            "the durable writer preserves the reporter/subject split"
        );
        let hold_start = hold_start.clone();
        let hold_end = hold_end.clone();
        let indexed = db
            .run(|trx, _| {
                let hold_start = hold_start.clone();
                let hold_end = hold_end.clone();
                async move {
                    use futures::TryStreamExt as _;

                    let mut rows = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                hold_start.as_slice(),
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(
                                hold_end.as_slice(),
                            ),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    Ok(rows.try_next().await?)
                }
            })
            .await
            .expect("restore-hold index read")
            .expect("strike transaction wrote restore-hold index");
        assert!(matches!(
            crate::keyspace::decode_restore_hold_key(indexed.key()),
            Some((source, entity, crate::keyspace::RestoreHoldProduct::Strike { account, .. }))
                if source == source_node
                    && entity == report.bundle.entity
                    && account == subject_account
        ));

        // Leave the cluster exactly as this run found it: every span and
        // row the setup and the filing claimed, cleared — the too-narrow
        // cleanup that stopped at the `ya` span was the defect (#1000).
        claims.clear(&db).await;
        journal.close().await.expect("journal closes");
    }
}
