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

use orrery_core::verify_bundle;
use orrery_core::Ruleset;
use orrery_protocol::{
    AccountId, DiscrepancyReport, NodeId, PersistId, RulesetId, Tick, UnadjudicableReason,
    UniverseSeed, Verdict,
};
use serde::{Deserialize, Serialize};

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
    strike_account_range_end, strike_account_range_start, strike_key, strike_versionstamped_key,
    STRIKE_VERSIONSTAMP_OFFSET,
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
    /// Resolve `target` through D31's current binding and append `row` once.
    fn file(&self, target: NodeId, row: &StrikeRow)
        -> Result<StrikeFileOutcome, StrikeLedgerError>;
}

/// In-memory strike ledger for tests and harnesses.
#[derive(Debug, Default)]
pub struct MemStrikeLedger {
    state: Mutex<MemStrikeState>,
}

#[derive(Debug, Default)]
struct MemStrikeState {
    bindings: HashMap<NodeId, AccountId>,
    rows: HashMap<AccountId, Vec<StrikeRow>>,
}

impl MemStrikeLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one current D31 binding for a harness.
    pub fn bind(&self, node: NodeId, account: AccountId) {
        Self::lock(&self.state).bindings.insert(node, account);
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

    fn lock(state: &Mutex<MemStrikeState>) -> MutexGuard<'_, MemStrikeState> {
        state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl StrikeLedger for MemStrikeLedger {
    fn file(
        &self,
        target: NodeId,
        row: &StrikeRow,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        let mut state = Self::lock(&self.state);
        let Some(account) = state.bindings.get(&target).copied() else {
            return Ok(StrikeFileOutcome::UnresolvedBinding);
        };
        let rows = state.rows.entry(account).or_default();
        if rows.iter().any(|existing| {
            existing.evidence_ref.digest == row.evidence_ref.digest && existing.kind == row.kind
        }) {
            return Ok(StrikeFileOutcome::Duplicate { account });
        }
        rows.push(row.clone());
        Ok(StrikeFileOutcome::Filed { account })
    }
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
type Worker = Box<dyn Fn(NodeId, &orrery_protocol::EvidenceBundle) -> Verdict + Send + Sync>;

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
            Box::new(move |authority, bundle| verify_bundle(factory(), seed, authority, bundle));
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
        let verdict = if orrery_witness::verify_report(report).is_err() {
            // Unsigned or tampered-in-transit. Not `EvidenceForged`: that
            // verdict strikes the named reporter, and an unverifiable
            // signature is exactly the case where the name means nothing.
            Verdict::Unadjudicable(UnadjudicableReason::Malformed)
        } else if let Some(registered) = self
            .builds
            .iter()
            .find(|registered| registered.id == report.bundle.ruleset)
        {
            (registered.worker)(report.subject, &report.bundle)
        } else {
            Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
        };
        self.file_report_verdict(report, verdict);
        verdict
    }

    fn file_report_verdict(&self, report: &DiscrepancyReport, verdict: Verdict) {
        let (target, kind, filed_counter) = match verdict {
            Verdict::Confirms { .. } => (
                report.subject,
                StrikeKind::Deviation,
                &self.strike_metrics.filed_subject,
            ),
            Verdict::EvidenceForged(_) => (
                report.reporter,
                StrikeKind::EvidenceForged,
                &self.strike_metrics.filed_reporter,
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
        match filer.ledger.file(target, &row) {
            Ok(StrikeFileOutcome::Filed { .. }) => {
                filed_counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(StrikeFileOutcome::Duplicate { .. }) => {
                self.strike_metrics
                    .duplicate
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(StrikeFileOutcome::UnresolvedBinding) => {
                self.strike_metrics
                    .suppressed_unresolved
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                self.strike_metrics
                    .suppressed_error
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%error, ?kind, "strike filing failed; verdict remains deliverable");
            }
        }
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
        (registered.worker)(subject, bundle)
    }
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

/// FoundationDB-backed executor-owned writer for D33's `ya` family.
#[cfg(feature = "fdb")]
#[derive(Clone)]
pub struct FdbStrikeLedger {
    db: Arc<foundationdb::Database>,
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
        Self { db }
    }

    /// Read one account's ledger rows in commit order.
    pub async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, StrikeLedgerError> {
        read_strike_rows(Arc::clone(&self.db), account).await
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
        row: &StrikeRow,
    ) -> Result<StrikeFileOutcome, StrikeLedgerError> {
        use foundationdb::options::MutationType;
        use futures::TryStreamExt;

        let db = Arc::clone(&self.db);
        let row = row.clone();
        futures::executor::block_on(async move {
            db.run(|trx, _| {
                let row = row.clone();
                async move {
                    let binding_key = crate::keyspace::binding_key(&target);
                    let Some(raw_binding) = trx.get(&binding_key, false).await? else {
                        return Ok(StrikeFileOutcome::UnresolvedBinding);
                    };
                    let binding: crate::keyspace::BindingRow = postcard::from_bytes(&raw_binding)
                        .map_err(|error| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(
                            StrikeLedgerError(format!("binding row decode: {error}")),
                        ))
                    })?;
                    let account = binding.account;
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
                        if existing.evidence_ref.digest == row.evidence_ref.digest
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
                    trx.atomic_op(
                        &strike_versionstamped_key(account),
                        &encoded,
                        MutationType::SetVersionstampedKey,
                    );
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
    use orrery_protocol::{ChainHash, EvidenceBundle, PersistId, StateClaim, Tick};

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

    fn bundle(ruleset: RulesetId) -> EvidenceBundle {
        let subject = key(1);
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
        orrery_core::log::sign_claim(&subject, &mut claim);
        EvidenceBundle {
            ruleset,
            entity: PersistId::new(1),
            window_start: Tick::new(0),
            window_end: Tick::new(1),
            t0_claim: claim,
            t0_snapshot: bytes::Bytes::new(),
            frames: Vec::new(),
            sibling_heads: Vec::new(),
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
    fn forged_evidence_files_against_reporter_and_never_subject() {
        let report = report();
        let subject_account = AccountId(51);
        let reporter_account = AccountId(52);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, subject_account);
        ledger.bind(report.reporter, reporter_account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Shadow);

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
    }

    #[test]
    fn non_striking_verdicts_file_nothing_for_either_party() {
        let report = report();
        let subject_account = AccountId(61);
        let reporter_account = AccountId(62);
        let ledger = Arc::new(MemStrikeLedger::new());
        ledger.bind(report.subject, subject_account);
        ledger.bind(report.reporter, reporter_account);
        let executor = filing_executor(Arc::clone(&ledger), StrikeMode::Live);

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
    }

    #[test]
    fn filing_failure_does_not_replace_the_adjudicated_verdict() {
        struct AlwaysFails;
        impl StrikeLedger for AlwaysFails {
            fn file(
                &self,
                _target: NodeId,
                _row: &StrikeRow,
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

    #[cfg(feature = "fdb")]
    #[tokio::test]
    async fn fdb_verdict_files_subject_row_and_no_reporter_row() {
        let Some(cluster) = crate::fdb::discover_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set");
            return;
        };
        let ledger = Arc::new(FdbStrikeLedger::connect(&cluster).expect("connect strike ledger"));
        let report = report();
        let subject_account = AccountId(0x0215_0000_0000_0001);
        let reporter_account = AccountId(0x0215_0000_0000_0002);
        let subject_key = crate::keyspace::binding_key(&report.subject);
        let reporter_key = crate::keyspace::binding_key(&report.reporter);
        let subject_start = strike_account_range_start(subject_account);
        let subject_end = strike_account_range_end(subject_account);
        let reporter_start = strike_account_range_start(reporter_account);
        let reporter_end = strike_account_range_end(reporter_account);
        let db = Arc::clone(&ledger.db);
        db.run(|trx, _| {
            let subject_start = subject_start.clone();
            let subject_end = subject_end.clone();
            let reporter_start = reporter_start.clone();
            let reporter_end = reporter_end.clone();
            async move {
                trx.clear_range(&subject_start, &subject_end);
                trx.clear_range(&reporter_start, &reporter_end);
                trx.set(
                    &subject_key,
                    &postcard::to_stdvec(&crate::keyspace::BindingRow {
                        account: subject_account,
                        bound_at_ms: 1,
                    })
                    .expect("encode binding"),
                );
                trx.set(
                    &reporter_key,
                    &postcard::to_stdvec(&crate::keyspace::BindingRow {
                        account: reporter_account,
                        bound_at_ms: 1,
                    })
                    .expect("encode binding"),
                );
                Ok(())
            }
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

        let subject_start = strike_account_range_start(subject_account);
        let subject_end = strike_account_range_end(subject_account);
        let reporter_start = strike_account_range_start(reporter_account);
        let reporter_end = strike_account_range_end(reporter_account);
        db.run(|trx, _| {
            let subject_start = subject_start.clone();
            let subject_end = subject_end.clone();
            let reporter_start = reporter_start.clone();
            let reporter_end = reporter_end.clone();
            async move {
                trx.clear(&subject_key);
                trx.clear(&reporter_key);
                trx.clear_range(&subject_start, &subject_end);
                trx.clear_range(&reporter_start, &reporter_end);
                Ok(())
            }
        })
        .await
        .expect("wipe strike test rows");
    }
}
