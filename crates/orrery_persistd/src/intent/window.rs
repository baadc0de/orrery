//! D32 clause (e)'s measurement window `W` and the counters observed inside
//! it, durable
//! ([D32](../../../../docs/adr/0032-enforcement-ramp.md)).
//!
//! # Why this exists
//!
//! [`super::cohort`] gave `|H|` a durable form, so cohort *membership* now
//! survives a restart. `W` did not get one, and neither did the counters
//! scored over `H`. Both were fields of [`RampMeter`](super::ramp::RampMeter)'s
//! in-process `Mutex<Tallies>`: `first_ms` and `last_ms` were reset to `None`
//! by every `persistd` restart, and the qualifying / observed / would-act
//! counts went with them.
//!
//! A routine deploy is enough to do that. So clause (e)'s `W ≥ 30 days` term
//! was unreachable no matter how long the fleet ran — not traffic-limited,
//! structurally unreachable — and `|H|` surviving while the counts observed
//! over `H` did not was only half a fix.
//!
//! # The split is carried, not summed away
//!
//! The counters are stored **per clause (e) half**: an armed tally, a natural
//! tally, and the fleet-wide tally they are drawn from. That is the
//! non-negotiable part. A promotion reviewer's actual question is whether the
//! control *"would have refused 40 honest players"* or *"would have refused 40
//! cheats"*, and a durable counter that folded the two halves together would
//! answer neither. [`super::cohort`] keeps the split per member; this keeps it
//! per count, and the two meet in
//! [`RampMeter::snapshot`](super::ramp::RampMeter::snapshot).
//!
//! The join to `H` happens at **flush** time and not at count time, because
//! the meter is on the admission path and the cohort is an operator-written
//! roster: asking the hot path which half an account is in would put a second
//! lookup where D16 budgets none. The flusher already holds the drained
//! per-account map and the loaded cohort, so the join costs one pass over
//! accounts that produced traffic since the last flush.
//!
//! # What is stored, and what deliberately is not
//!
//! | Figure | Durable | Why |
//! |---|---|---|
//! | `first_ms` / `last_ms` — clause (e)'s `W` | yes | the whole point |
//! | fleet, armed, natural, unattributed volumes | yes | clause (e)'s `coverage` and `fp_count` |
//! | verdict and cause histograms | yes | the outcome split, exhaustive |
//! | `H` accounts that were active / would-have-acted | yes, as **sets** | a cardinality cannot be folded from counts, and `\|H\|` is bounded by an operator's sampling |
//! | the `RulesetId`s the window observed | yes, as a **set** | the stamp, below — a window that spanned a ruleset change says so instead of resetting |
//! | fleet-wide distinct-account cardinalities | **no** | see below |
//!
//! Set cardinalities cannot be added across flushes or across processes — the
//! same account appearing in two flushes is one account, and only the ids
//! themselves can say so. For the cohort side that is affordable: `H` is a
//! hand-sampled roster whose floor is `|H| ≥ 100`, and the ids fit in a row
//! (bounded by [`MAX_DURABLE_COHORT_ACCOUNTS`], with the overflow *reported*
//! rather than absorbed). For the fleet side it is not: the meter tracks up to
//! [`DEFAULT_ACCOUNT_CAPACITY`](super::ramp::DEFAULT_ACCOUNT_CAPACITY) =
//! 100_000 accounts, and 100_000 ids do not fit in a FoundationDB value.
//!
//! So [`RampSnapshot::accounts_qualifying`](super::ramp::RampSnapshot::accounts_qualifying),
//! `accounts_observed` and `accounts_would_act` remain **per-process** figures
//! that a restart resets, and this module states that rather than inventing a
//! number for them. It is a real gap and it is named as one; what it is not is
//! load-bearing. Clause (e)'s own terms — `|H|`, `W`, `coverage`, `fp_count`
//! and the cohort's `active` count — are all durable here. And clause (f)'s
//! `spread`, the one of those cardinalities a decision hangs on, is computed
//! by the auto-suspend breaker itself in
//! [`SuspendMonitor`](super::autosuspend::SuspendMonitor) over its own
//! [`WINDOW_MS`](super::autosuspend::WINDOW_MS) rolling window, and never read
//! out of a snapshot.
//!
//! # Windows are generations, and a reset opens a new one
//!
//! Every row carries a [`RampWindowRow::window_id`]. A deliberate reset —
//! `orrery-ramp window reset` — writes a fresh row with the next id, a new
//! `opened_at_ms`, zeroed counters and the operator's reason.
//!
//! The generation is what makes the reset *reach a running fleet*. Each
//! process flushes a [`RampWindowDelta`] stamped with the window it believes
//! it is measuring; the flush transaction compares that stamp against the
//! stored row and, when they differ, **discards the delta** and hands the
//! process the new window instead. A reset therefore costs at most one flush
//! interval of post-reset observations per live process, and buys the property
//! it exists for: no observation taken before a semantic change can be counted
//! into the window opened after it. A semantic change the operator *does*
//! reset for is excluded that way; one they do not is the stamp's business,
//! in the next section.
//!
//! # The ruleset stamp, and why a ruleset change does not reset the window
//!
//! Whether a ruleset change should reset the window *automatically* was D32
//! open question 6, and the owner decided it on 2026-09-03: **no**. A reset
//! discards evidence irreversibly — this store keeps no history — and clause
//! (f) scopes `RulesetId` to the verdict-driven controls C3/C4/C5 only, so an
//! automatic reset would fire for some controls and not others, an
//! inconsistency worse than the problem it solved. `orrery-ramp window
//! reset` remains the deliberate operator act, unchanged.
//!
//! What landed instead is the stamp: [`WindowCounts::ruleset_ids`] records
//! the `RulesetId`s the window's counters observed, folding by union exactly
//! like the account sets and bounded by [`MAX_DURABLE_RULESET_IDS`] with the
//! overflow reported rather than absorbed. A window that spanned a ruleset
//! change therefore carries both ids, and
//! [`super::report`]'s `provenance.windows` publishes them, so a reviewer
//! sees the span and judges the evidence instead of the fleet deciding for
//! them. The stamp rides the same counting point as the denominator:
//! `RampMeter::observe_ruleset` is called where the meter first sees the
//! ruleset, which for C4 is every metered report and for C5 every
//! shadow-filed strike.
//!
//! # Why these rows carry no signature
//!
//! The argument is [`super::cohort`]'s, unchanged: a `ramp/{control}` posture
//! row commands the fleet's enforcement and is authenticated at the reader per
//! clause (i); a measurement row commands nothing. It is worth restating one
//! difference, though. A forged *cohort* row cannot manufacture a clean
//! `fp_count`, because the counters are where every figure comes from. A
//! forged *window* row can — it is the counters. Possession of the cluster
//! file is therefore a strictly larger capability here than it is for the
//! cohort, and the mitigation is the one the record already relies on: the
//! artifact producer, the flusher and the operator tool all reach this row
//! through the same cluster file, and custody of that file is the boundary.
//! Signing measurement rows would need a writer identity every `persistd`
//! holds, which is a different trust root from D32 clause (i)'s operator key
//! and is not one this change introduces.
//!
//! # Keyspace
//!
//! `rampw/{control}` — the `b"vm"` sub-span of the registered `v` family, per
//! D32 clause (c)'s allocation rule. See
//! [`crate::keyspace::ramp_window_key`] for the discriminator argument.

use std::collections::{BTreeMap, BTreeSet};

use orrery_protocol::{AccountId, RulesetId};
use serde::{Deserialize, Serialize};

/// How many `H` account ids one window row records per set.
///
/// Clause (e)'s floor is `|H| ≥ 100`; this is an order of magnitude above it,
/// and the four sets together stay far inside FoundationDB's 100 KiB value
/// bound. Overflow is *reported* through
/// [`WindowCounts::cohort_accounts_truncated`] rather than absorbed, on the
/// same principle as
/// [`RampSnapshot::accounts_truncated`](super::ramp::RampSnapshot::accounts_truncated):
/// a cohort cardinality that is silently understated is worse than one that
/// says it is understated.
pub const MAX_DURABLE_COHORT_ACCOUNTS: usize = 1_024;

/// The longest `reason` a window reset records.
///
/// The same 256-byte writer bound
/// [`RampPosture::reason`](super::ramp::RampPosture::reason) and
/// [`MAX_COHORT_REASON_BYTES`](super::cohort::MAX_COHORT_REASON_BYTES)
/// document, for the same reason: the one free-text field an operator controls
/// must not become a storage amplifier.
pub const MAX_WINDOW_REASON_BYTES: usize = 256;

/// How many distinct `RulesetId`s one window row records.
///
/// Distinct rulesets are build identities, not per-account cardinalities: a
/// fleet that is not churning its rules observes a handful over even a
/// thirty-day window, and 64 leaves an order of magnitude over the most
/// churned plausible one. The ids are not operator- or meter-chosen, though —
/// they arrive inside evidence bundles — so the set is bounded anyway, and
/// 64 × 36 B keeps it far inside FoundationDB's 100 KiB value bound even
/// alongside four full cohort account sets. Overflow is *reported* through
/// [`WindowCounts::rulesets_truncated`] rather than absorbed, on the same
/// principle as [`MAX_DURABLE_COHORT_ACCOUNTS`]: a window's ruleset span that
/// is silently understated is exactly the fact this stamp exists to surface.
pub const MAX_DURABLE_RULESET_IDS: usize = 64;

/// The spelling an observed `RulesetId` takes in the artifact and in
/// `orrery-ramp window show`: `v<version>:<64 hex>`.
///
/// The full digest is kept, not shortened to a prefix: the label is evidence,
/// and a truncated one could not distinguish two builds of the same version —
/// which is the exact situation a window spanning a ruleset change presents.
#[must_use]
pub fn ruleset_stamp_label(ruleset: &RulesetId) -> String {
    let digest: String = ruleset
        .digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("v{}:{digest}", ruleset.version)
}

/// One population's volumes inside the window.
///
/// The shape mirrors the meter's in-process per-account tally, minus the
/// account identity: these are sums over a population, and the population is
/// named by which field of [`WindowCounts`] holds them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTally {
    /// Admission decisions — clause (e)'s coverage denominator.
    pub qualifying: u64,
    /// Of those, the ones the control produced a verdict for.
    pub observed: u64,
    /// Recorded evaluations that produced no verdict (clause (b)'s degraded
    /// arm), which are neither numerator nor denominator.
    pub unevaluated: u64,
    /// Of the observed ones, the ones live mode would have acted on.
    pub would_act: u64,
    /// The would-be actions by stable action label, never a parallel
    /// vocabulary (D32 clause (b)).
    pub causes: BTreeMap<String, u64>,
}

impl DurableTally {
    /// Add `other`'s volumes into this one.
    pub fn fold(&mut self, other: &Self) {
        self.qualifying = self.qualifying.saturating_add(other.qualifying);
        self.observed = self.observed.saturating_add(other.observed);
        self.unevaluated = self.unevaluated.saturating_add(other.unevaluated);
        self.would_act = self.would_act.saturating_add(other.would_act);
        for (cause, count) in &other.causes {
            let slot = self.causes.entry(cause.clone()).or_default();
            *slot = slot.saturating_add(*count);
        }
    }

    /// Whether nothing has been counted into this tally.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.qualifying == 0
            && self.observed == 0
            && self.unevaluated == 0
            && self.would_act == 0
            && self.causes.is_empty()
    }
}

/// Everything one window has observed, with clause (e)'s halves kept apart.
///
/// Folding is the only way this grows, and every field folds *associatively
/// and idempotently enough to be safe under concurrent flushers*: volumes add,
/// bounds take min and max, and account sets union. That is what lets several
/// `persistd` processes contribute to one row through independent
/// read-modify-write transactions without a coordinator.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowCounts {
    /// Earliest observation timestamp in the window — `W`'s lower bound.
    pub first_ms: Option<u64>,
    /// Latest observation timestamp in the window — `W`'s upper bound.
    pub last_ms: Option<u64>,
    /// Fleet-wide volumes: every attributed account plus the meter's
    /// truncation bucket, matching what the in-process snapshot calls `all`.
    pub fleet: DurableTally,
    /// Submissions on a connection with no established session, which clause
    /// (e) puts outside `H` by construction. Kept apart rather than dropped:
    /// their size is a fact about the traffic a reviewer should see.
    pub unattributed: DurableTally,
    /// Volumes produced by accounts in `H_armed` — operator-driven automation.
    pub armed: DurableTally,
    /// Volumes produced by accounts in `H_natural` — sampled real players.
    pub natural: DurableTally,
    /// Armed members with any qualifying activity. A set and not a count,
    /// because the same member reappears in later flushes and only the id can
    /// say it is the same one.
    pub armed_active: BTreeSet<AccountId>,
    /// Natural members with any qualifying activity.
    pub natural_active: BTreeSet<AccountId>,
    /// Armed members with at least one would-have-acted event.
    pub armed_would_act: BTreeSet<AccountId>,
    /// Natural members with at least one would-have-acted event — the half a
    /// promotion review reads first, because these are real players the
    /// control would have refused.
    pub natural_would_act: BTreeSet<AccountId>,
    /// Every `RulesetId` this window's counters observed — D32 open question
    /// 6's stamp, decided 2026-09-03.
    ///
    /// A window that spanned a ruleset change carries every id it saw, so a
    /// reviewer judges the span instead of the fleet resetting for them. The
    /// set folds by union exactly like the account sets above, is bounded by
    /// [`MAX_DURABLE_RULESET_IDS`], and its overflow is counted in
    /// [`Self::rulesets_truncated`] rather than dropped. Protocol-level
    /// controls (C1, C2) carry an empty set by construction: clause (f)
    /// scopes the dimension to the verdict-driven C3/C4/C5, and their meters
    /// are never handed a ruleset.
    pub ruleset_ids: BTreeSet<RulesetId>,
    /// Every recorded verdict by its label, admitting ones included.
    pub by_verdict: BTreeMap<String, u64>,
    /// Distinct `H` accounts a set could not record because it was already at
    /// [`MAX_DURABLE_COHORT_ACCOUNTS`]. Nonzero means the cohort's `active`
    /// and `accounts_would_act` figures are understated by at most this much.
    pub cohort_accounts_truncated: u64,
    /// Distinct `RulesetId`s [`Self::ruleset_ids`] could not record because it
    /// was already at [`MAX_DURABLE_RULESET_IDS`]. Nonzero means the window's
    /// ruleset span is understated — there were rulesets it saw that the row
    /// cannot name.
    pub rulesets_truncated: u64,
    /// Whether any flush into this window carried traffic from the meter's
    /// past-capacity truncation bucket.
    ///
    /// A flag and not a count, and that is the honest shape rather than a
    /// convenient one. The figure a reviewer would want is *how many distinct
    /// accounts* were folded away, and distinct-account counts do not add
    /// across flushes — see the module docs. What can be carried without
    /// inventing anything is whether it happened at all, which is enough to
    /// tell a reviewer that this window's fleet-wide account spread and cohort
    /// denominator are both understated by an unknown amount.
    pub fleet_truncation_seen: bool,
}

/// Union `from` into `into`, counting the ids that did not fit `cap` into
/// `truncated` rather than dropping them silently. One body for the cohort
/// account sets and the ruleset stamp, at their own bounds.
fn fold_set<T: Copy + Ord>(
    into: &mut BTreeSet<T>,
    from: &BTreeSet<T>,
    cap: usize,
    truncated: &mut u64,
) {
    for item in from {
        if into.contains(item) {
            continue;
        }
        if into.len() >= cap {
            *truncated = truncated.saturating_add(1);
            continue;
        }
        into.insert(*item);
    }
}

impl WindowCounts {
    /// Add `other` into this window's totals.
    pub fn fold(&mut self, other: &Self) {
        self.first_ms = match (self.first_ms, other.first_ms) {
            (Some(mine), Some(theirs)) => Some(mine.min(theirs)),
            (mine, None) => mine,
            (None, theirs) => theirs,
        };
        self.last_ms = match (self.last_ms, other.last_ms) {
            (Some(mine), Some(theirs)) => Some(mine.max(theirs)),
            (mine, None) => mine,
            (None, theirs) => theirs,
        };
        self.fleet.fold(&other.fleet);
        self.unattributed.fold(&other.unattributed);
        self.armed.fold(&other.armed);
        self.natural.fold(&other.natural);
        let mut truncated = self.cohort_accounts_truncated;
        fold_set(
            &mut self.armed_active,
            &other.armed_active,
            MAX_DURABLE_COHORT_ACCOUNTS,
            &mut truncated,
        );
        fold_set(
            &mut self.natural_active,
            &other.natural_active,
            MAX_DURABLE_COHORT_ACCOUNTS,
            &mut truncated,
        );
        fold_set(
            &mut self.armed_would_act,
            &other.armed_would_act,
            MAX_DURABLE_COHORT_ACCOUNTS,
            &mut truncated,
        );
        fold_set(
            &mut self.natural_would_act,
            &other.natural_would_act,
            MAX_DURABLE_COHORT_ACCOUNTS,
            &mut truncated,
        );
        self.cohort_accounts_truncated = truncated.saturating_add(other.cohort_accounts_truncated);
        let mut rulesets_truncated = self.rulesets_truncated;
        fold_set(
            &mut self.ruleset_ids,
            &other.ruleset_ids,
            MAX_DURABLE_RULESET_IDS,
            &mut rulesets_truncated,
        );
        self.rulesets_truncated = rulesets_truncated.saturating_add(other.rulesets_truncated);
        self.fleet_truncation_seen |= other.fleet_truncation_seen;
        for (verdict, count) in &other.by_verdict {
            let slot = self.by_verdict.entry(verdict.clone()).or_default();
            *slot = slot.saturating_add(*count);
        }
    }

    /// Note one observation's timestamp against the window's bounds.
    pub fn observe_at(&mut self, at_ms: u64) {
        self.first_ms = Some(self.first_ms.map_or(at_ms, |first| first.min(at_ms)));
        self.last_ms = Some(self.last_ms.map_or(at_ms, |last| last.max(at_ms)));
    }

    /// Whether nothing at all has been counted into this window.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.first_ms.is_none()
            && self.last_ms.is_none()
            && self.fleet.is_empty()
            && self.unattributed.is_empty()
            && self.armed.is_empty()
            && self.natural.is_empty()
            && self.by_verdict.is_empty()
    }

    /// `|H_armed ∪ H_natural|` restricted to members that produced qualifying
    /// activity — clause (e)'s `active` count, over the durable window.
    #[must_use]
    pub fn active_members(&self) -> usize {
        self.armed_active.union(&self.natural_active).count()
    }

    /// Distinct `H` members with at least one would-have-acted event.
    #[must_use]
    pub fn would_act_members(&self) -> usize {
        self.armed_would_act.union(&self.natural_would_act).count()
    }
}

/// One control's durable measurement window: the value stored at
/// `rampw/{control}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RampWindowRow {
    /// Which generation of the window these counters belong to. Bumped only
    /// by a deliberate [`FdbRampWindowStore::reset`].
    pub window_id: u64,
    /// Unix milliseconds at which this generation was opened. Distinct from
    /// [`WindowCounts::first_ms`], which is the first *observation*: a window
    /// opened and then silent for a week has an `opened_at_ms` a week before
    /// its `first_ms`, and a reviewer needs both.
    pub opened_at_ms: u64,
    /// The operator's reason for opening this generation, absent for the one
    /// the first flush creates implicitly.
    pub reset_reason: Option<String>,
    /// How many flushes have folded into this row. An operational figure: a
    /// window whose counters are large and whose `flushes` is 1 was written by
    /// one process once, which is not a fleet measurement.
    pub flushes: u64,
    /// The counters.
    pub counts: WindowCounts,
}

impl RampWindowRow {
    /// A fresh, empty generation opened at `opened_at_ms`.
    #[must_use]
    pub fn opened(window_id: u64, opened_at_ms: u64, reset_reason: Option<String>) -> Self {
        Self {
            window_id,
            opened_at_ms,
            reset_reason,
            flushes: 0,
            counts: WindowCounts::default(),
        }
    }

    /// `W` in days, from the observation bounds.
    ///
    /// Zero for a window with no observations, which is what `W = 0` means:
    /// the window exists and nothing has been seen in it. It is deliberately
    /// not derived from `opened_at_ms`, because clause (e) measures a window
    /// *of observations* and a control nobody exercised for thirty days has
    /// not observed for thirty days.
    #[must_use]
    pub fn window_days(&self) -> f64 {
        let (Some(first), Some(last)) = (self.counts.first_ms, self.counts.last_ms) else {
            return 0.0;
        };
        // Both are event timestamps; a millisecond count that exceeds 2^53 is
        // not a measurement window.
        #[allow(clippy::cast_precision_loss)]
        {
            last.saturating_sub(first) as f64 / 86_400_000.0
        }
    }
}

/// Counters one process accumulated since its last successful flush, stamped
/// with the window generation it believes it is measuring.
///
/// The stamp is the whole reason this is a distinct type from
/// [`WindowCounts`]. A delta whose `window_id` no longer matches the stored
/// row was measured under a retired window, and folding it in would put
/// pre-reset observations into a post-reset window — the exact thing the reset
/// exists to prevent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RampWindowDelta {
    /// The generation this delta was measured under.
    pub window_id: u64,
    /// What was measured.
    pub counts: WindowCounts,
}

/// The outcome of one flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    /// The delta was folded into the stored row, which now reads as given.
    Applied(Box<RampWindowRow>),
    /// The stored window is a later generation than the delta's: the delta was
    /// discarded and the caller must adopt the row returned here. See the
    /// module docs on why a reset is allowed to cost one flush interval.
    WindowChanged(Box<RampWindowRow>),
}

/// Failure reading or writing a durable measurement window.
#[derive(Debug)]
pub enum RampWindowError {
    /// A reset reason exceeds [`MAX_WINDOW_REASON_BYTES`].
    ReasonTooLong(usize),
    /// A FoundationDB transaction failed, or a row did not decode.
    Store(String),
}

impl std::fmt::Display for RampWindowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReasonTooLong(len) => write!(
                formatter,
                "the reset reason is {len} bytes; the writer bound is \
                 {MAX_WINDOW_REASON_BYTES}"
            ),
            Self::Store(what) => write!(formatter, "ramp window store: {what}"),
        }
    }
}

impl std::error::Error for RampWindowError {}

/// Durable home of D32 clause (e)'s window and counters: one `rampw/{control}`
/// row, folded into by every process that meters the control.
///
/// [`Self::load`] is the startup half — a restart *continues* the window it
/// finds instead of opening a new one — and [`Self::flush`] is the periodic
/// half. Neither runs in the coordinator: `orrery_coordinator` declares no
/// `foundationdb` dependency at all, and ADR-0031 clause (d) keeps it that way
/// ("the coordinator does not read"). The flusher runs in `persistd`, which is
/// the gateway, which is the process clause (d) names as the reader.
#[cfg(feature = "fdb")]
pub struct FdbRampWindowStore {
    db: std::sync::Arc<foundationdb::Database>,
}

#[cfg(feature = "fdb")]
impl FdbRampWindowStore {
    /// Construct from the process-scoped FDB context.
    #[must_use]
    pub fn from_context(context: &crate::FdbContext) -> Self {
        Self {
            db: context.database(),
        }
    }

    /// The window this control is currently being measured over, or `None`
    /// when no flush has ever opened one.
    ///
    /// # Errors
    ///
    /// [`RampWindowError::Store`] for transaction or decode failures.
    pub async fn load(&self, control: &str) -> Result<Option<RampWindowRow>, RampWindowError> {
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_window_key(control);
        let value: Option<Vec<u8>> = db
            .run(move |trx, _| {
                let key = key.clone();
                async move {
                    Ok(trx
                        .get(&key, false)
                        .await
                        .map_err(store_err("read ramp window row"))?
                        .map(|bytes| bytes.as_ref().to_vec()))
                }
            })
            .await
            .map_err(|error: foundationdb::FdbBindingError| {
                RampWindowError::Store(format!("read ramp window transaction: {error}"))
            })?;
        value
            .map(|bytes| {
                postcard::from_bytes(&bytes).map_err(|error| {
                    RampWindowError::Store(format!("decode ramp window row: {error}"))
                })
            })
            .transpose()
    }

    /// Fold one process's delta into the stored window, in one transaction.
    ///
    /// Read-modify-write rather than an absolute overwrite, because several
    /// `persistd` processes meter the same control and an absolute write would
    /// silently drop every other writer's contribution. Every field of
    /// [`WindowCounts`] folds monotonically — volumes add, bounds take min and
    /// max, account and ruleset sets union — so the transaction needs no
    /// coordination beyond FoundationDB's own serializability, and a retried
    /// transaction applies the same delta once.
    ///
    /// A row absent altogether is *opened* here, at generation 0, with
    /// `opened_at_ms = now_ms`. The implicit open is deliberate: a fleet that
    /// has never been reset should not need an operator to press start before
    /// clause (e) begins accumulating.
    ///
    /// Returns [`FlushOutcome::WindowChanged`] without applying anything when
    /// the stored generation is not the delta's; the caller adopts the
    /// returned row and starts measuring the new window.
    ///
    /// # Errors
    ///
    /// [`RampWindowError::Store`] for transaction or decode failures.
    pub async fn flush(
        &self,
        control: &str,
        delta: &RampWindowDelta,
        now_ms: u64,
    ) -> Result<FlushOutcome, RampWindowError> {
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_window_key(control);
        let delta = delta.clone();
        db.run(move |trx, _| {
            let (key, delta) = (key.clone(), delta.clone());
            async move {
                let stored = trx
                    .get(&key, false)
                    .await
                    .map_err(store_err("read ramp window row"))?;
                let mut row = match stored {
                    Some(bytes) => postcard::from_bytes::<RampWindowRow>(&bytes)
                        .map_err(store_err("decode ramp window row"))?,
                    None => RampWindowRow::opened(0, now_ms, None),
                };
                if row.window_id != delta.window_id {
                    return Ok(FlushOutcome::WindowChanged(Box::new(row)));
                }
                row.counts.fold(&delta.counts);
                row.flushes = row.flushes.saturating_add(1);
                let encoded =
                    postcard::to_allocvec(&row).map_err(store_err("encode ramp window row"))?;
                trx.set(&key, &encoded);
                Ok(FlushOutcome::Applied(Box::new(row)))
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampWindowError::Store(format!("flush ramp window transaction: {error}"))
        })
    }

    /// Open a fresh generation, retiring everything the current one observed.
    ///
    /// The deliberate reset. It is an operator act and not an automatic one:
    /// the owner declined the automatic trigger for D32 open question 6 on
    /// 2026-09-03 — the window stamps the rulesets it observed instead — so
    /// this stays the only way a ruleset change retires a window. See the
    /// module docs.
    ///
    /// Returns the row it wrote. The previous generation's counters are
    /// **gone** — this store keeps no history, because D32 open question 2's
    /// journal shadow is where append-only measurement history lands and
    /// inventing a second archive here would pre-empt it. An operator who
    /// needs the retired numbers reads them before resetting.
    ///
    /// # Errors
    ///
    /// [`RampWindowError::ReasonTooLong`] past [`MAX_WINDOW_REASON_BYTES`];
    /// [`RampWindowError::Store`] for transaction or decode failures.
    pub async fn reset(
        &self,
        control: &str,
        reason: &str,
        now_ms: u64,
    ) -> Result<RampWindowRow, RampWindowError> {
        if reason.len() > MAX_WINDOW_REASON_BYTES {
            return Err(RampWindowError::ReasonTooLong(reason.len()));
        }
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_window_key(control);
        let reason = reason.to_owned();
        db.run(move |trx, _| {
            let (key, reason) = (key.clone(), reason.clone());
            async move {
                let stored = trx
                    .get(&key, false)
                    .await
                    .map_err(store_err("read ramp window row"))?;
                let next_id = match stored {
                    Some(bytes) => postcard::from_bytes::<RampWindowRow>(&bytes)
                        .map_err(store_err("decode ramp window row"))?
                        .window_id
                        .saturating_add(1),
                    // No row yet: the reset opens generation 0 rather than 1,
                    // so an operator resetting before any traffic gets the
                    // same window the first flush would have opened, with
                    // their reason on it.
                    None => 0,
                };
                let row = RampWindowRow::opened(next_id, now_ms, Some(reason));
                let encoded =
                    postcard::to_allocvec(&row).map_err(store_err("encode ramp window row"))?;
                trx.set(&key, &encoded);
                Ok(row)
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampWindowError::Store(format!("reset ramp window transaction: {error}"))
        })
    }

    /// Remove one control's window row entirely.
    ///
    /// Distinct from [`Self::reset`]: a reset keeps the generation counter
    /// climbing, so a live process's stale delta is recognised and discarded.
    /// Clearing restarts generations at 0, which a process still holding a
    /// generation-0 delta would match — so this is a teardown verb for tests
    /// and decommissioning, not an operator's reset.
    ///
    /// # Errors
    ///
    /// [`RampWindowError::Store`] for transaction failures.
    pub async fn clear(&self, control: &str) -> Result<(), RampWindowError> {
        let db = std::sync::Arc::clone(&self.db);
        let key = crate::keyspace::ramp_window_key(control);
        db.run(move |trx, _| {
            let key = key.clone();
            async move {
                trx.clear(&key);
                Ok(())
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            RampWindowError::Store(format!("clear ramp window transaction: {error}"))
        })
    }
}

/// Lift a step description into the mapper every `db.run` closure needs at
/// each fallible step, matching [`super::cohort`]'s convention.
#[cfg(feature = "fdb")]
fn store_err<E: std::fmt::Display>(
    what: &'static str,
) -> impl Fn(E) -> foundationdb::FdbBindingError {
    move |error| {
        foundationdb::FdbBindingError::new_custom_error(Box::new(WindowStepError(format!(
            "{what}: {error}"
        ))))
    }
}

/// The error wrapper `new_custom_error` needs.
#[cfg(feature = "fdb")]
#[derive(Debug)]
struct WindowStepError(String);

#[cfg(feature = "fdb")]
impl std::fmt::Display for WindowStepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(feature = "fdb")]
impl std::error::Error for WindowStepError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: u64) -> AccountId {
        AccountId::new(id)
    }

    fn tally(qualifying: u64, observed: u64, would_act: u64) -> DurableTally {
        DurableTally {
            qualifying,
            observed,
            unevaluated: 0,
            would_act,
            causes: BTreeMap::new(),
        }
    }

    #[test]
    fn folding_takes_the_widest_bounds_and_never_restarts_the_window() {
        let mut into = WindowCounts::default();
        into.observe_at(5_000);
        into.observe_at(9_000);
        let mut from = WindowCounts::default();
        from.observe_at(1_000);
        from.observe_at(3_000);
        into.fold(&from);
        assert_eq!(
            into.first_ms,
            Some(1_000),
            "an earlier flush widens `first`"
        );
        assert_eq!(into.last_ms, Some(9_000), "and never moves `last` back");
    }

    #[test]
    fn folding_an_empty_delta_leaves_the_bounds_alone() {
        let mut into = WindowCounts::default();
        into.observe_at(7_000);
        into.fold(&WindowCounts::default());
        assert_eq!(into.first_ms, Some(7_000));
        assert_eq!(into.last_ms, Some(7_000));
    }

    #[test]
    fn the_halves_stay_apart_through_a_fold() {
        let mut into = WindowCounts {
            armed: tally(10, 8, 1),
            natural: tally(4, 4, 0),
            ..WindowCounts::default()
        };
        into.fold(&WindowCounts {
            armed: tally(1, 1, 0),
            natural: tally(6, 5, 3),
            ..WindowCounts::default()
        });
        assert_eq!(into.armed.qualifying, 11);
        assert_eq!(into.armed.would_act, 1, "armed would-acts stay armed");
        assert_eq!(into.natural.qualifying, 10);
        assert_eq!(
            into.natural.would_act, 3,
            "a reviewer can still tell three refused players from one refused bot"
        );
    }

    #[test]
    fn account_sets_union_rather_than_double_counting_a_returning_member() {
        let mut into = WindowCounts {
            natural_active: [account(1), account(2)].into_iter().collect(),
            ..WindowCounts::default()
        };
        into.fold(&WindowCounts {
            natural_active: [account(2), account(3)].into_iter().collect(),
            ..WindowCounts::default()
        });
        assert_eq!(
            into.active_members(),
            3,
            "account 2 appearing in two flushes is one active member"
        );
    }

    #[test]
    fn the_cohort_set_bound_reports_its_overflow_instead_of_absorbing_it() {
        let mut into = WindowCounts::default();
        let wide: BTreeSet<AccountId> = (0..u64::try_from(MAX_DURABLE_COHORT_ACCOUNTS).unwrap()
            + 7)
            .map(account)
            .collect();
        into.fold(&WindowCounts {
            armed_active: wide,
            ..WindowCounts::default()
        });
        assert_eq!(into.armed_active.len(), MAX_DURABLE_COHORT_ACCOUNTS);
        assert_eq!(
            into.cohort_accounts_truncated, 7,
            "the seven that did not fit are reported, not silently dropped"
        );
    }

    fn ruleset(version: u32, byte: u8) -> RulesetId {
        RulesetId {
            version,
            digest: [byte; 32],
        }
    }

    /// The owner's 2026-09-03 decision on D32 open question 6, at the row's
    /// level: a window that observed two rulesets stamps both, and the set
    /// unions across folds exactly the way the account sets do — a ruleset
    /// seen by two flushes is one entry, and a change mid-window leaves both
    /// ids on the row for a reviewer to judge.
    #[test]
    fn a_window_that_observed_two_rulesets_carries_both_and_unions_across_folds() {
        let mut into = WindowCounts {
            ruleset_ids: [ruleset(9, 0xAA)].into_iter().collect(),
            ..WindowCounts::default()
        };
        into.fold(&WindowCounts {
            ruleset_ids: [ruleset(9, 0xAA), ruleset(10, 0xBB)].into_iter().collect(),
            ..WindowCounts::default()
        });
        assert_eq!(
            into.ruleset_ids,
            [ruleset(9, 0xAA), ruleset(10, 0xBB)].into_iter().collect(),
            "the shared ruleset is one entry and the change is visible as both"
        );
        assert_eq!(into.rulesets_truncated, 0);
    }

    #[test]
    fn the_ruleset_set_bound_reports_its_overflow_instead_of_absorbing_it() {
        let mut into = WindowCounts::default();
        let wide: BTreeSet<RulesetId> = (0..u32::try_from(MAX_DURABLE_RULESET_IDS).unwrap() + 3)
            .map(|version| ruleset(version, 0x11))
            .collect();
        into.fold(&WindowCounts {
            ruleset_ids: wide,
            ..WindowCounts::default()
        });
        assert_eq!(into.ruleset_ids.len(), MAX_DURABLE_RULESET_IDS);
        assert_eq!(
            into.rulesets_truncated, 3,
            "the three that did not fit are reported, not silently dropped"
        );
    }

    #[test]
    fn window_days_is_zero_for_a_window_with_no_observations() {
        let row = RampWindowRow::opened(3, 1_000, Some("ruleset v9".to_owned()));
        assert!(
            (row.window_days() - 0.0).abs() < f64::EPSILON,
            "an open window with nothing in it has observed for zero days, \
             whatever its age"
        );
    }

    #[test]
    fn window_days_spans_the_observations_and_not_the_generation_age() {
        let mut row = RampWindowRow::opened(0, 0, None);
        row.counts.observe_at(86_400_000);
        row.counts.observe_at(86_400_000 * 31);
        assert!((row.window_days() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_row_round_trips_through_postcard() {
        let mut row = RampWindowRow::opened(2, 4_242, Some("ruleset change".to_owned()));
        row.counts.observe_at(9);
        row.counts.armed = tally(3, 2, 1);
        row.counts.armed_would_act = [account(77)].into_iter().collect();
        row.counts.by_verdict.insert("would_refuse".to_owned(), 1);
        row.counts.ruleset_ids = [ruleset(9, 0xAA), ruleset(10, 0xBB)].into_iter().collect();
        let bytes = postcard::to_allocvec(&row).expect("encode");
        let back: RampWindowRow = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(row, back);
    }

    /// The stamp's spelling, as the artifact and `window show` render it.
    /// The full digest is kept: two builds of one version must be
    /// distinguishable, which is the situation a spanning window presents.
    #[test]
    fn the_stamp_label_carries_the_version_and_the_whole_digest() {
        assert_eq!(
            ruleset_stamp_label(&ruleset(9, 0xAB)),
            format!("v9:{}", "ab".repeat(32))
        );
        assert_ne!(
            ruleset_stamp_label(&ruleset(9, 0xAB)),
            ruleset_stamp_label(&ruleset(9, 0xCD)),
            "same version, different build — a shortened digest could not tell them apart"
        );
    }
}

#[cfg(all(test, feature = "fdb"))]
mod fdb_tests {
    use super::*;
    use crate::intent::ramp::{HonestCohort, RampMeter};
    use crate::intent::shadow::{ShadowObservation, ShadowVerdict};
    use crate::intent::{NetworkQuality, RejectionCause};
    use orrery_protocol::CellEpoch;

    /// The dev-cluster convention `intent/fdb.rs` established and
    /// [`super::super::cohort`] follows: skip without a cluster, and let
    /// `scripts/fdb-tests.sh` turn the skip into a gate.
    fn fdb_cluster_file() -> Option<String> {
        if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
            return Some(path);
        }
        let local = std::path::Path::new(".fdb-dev/fdb.cluster");
        local.exists().then(|| local.display().to_string())
    }

    fn store() -> FdbRampWindowStore {
        let cluster = fdb_cluster_file().expect("cluster file for ramp window tests");
        let context = crate::FdbContext::connect(&cluster).expect("connect");
        FdbRampWindowStore::from_context(&context)
    }

    fn account(id: u64) -> AccountId {
        AccountId::new(id)
    }

    fn node() -> orrery_protocol::NodeId {
        iroh_base::SecretKey::from_bytes(&[11; 32]).public()
    }

    fn obs(subject: u64, verdict: ShadowVerdict, at_ms: u64) -> ShadowObservation {
        ShadowObservation {
            intent_id: u128::from(at_ms) * 1_000 + u128::from(subject),
            issuer: node(),
            subject: Some(account(subject)),
            cell_epoch: CellEpoch::new(9),
            verdict,
            observed_at_ms: at_ms,
            network: NetworkQuality::Unknown,
        }
    }

    fn cohort() -> HonestCohort {
        let mut cohort = HonestCohort::new();
        cohort.arm(account(9_001));
        cohort.sample(account(9_002));
        cohort
    }

    /// Drive one process's worth of traffic into a meter.
    fn run_traffic(meter: &RampMeter, days: std::ops::Range<u64>) {
        const DAY_MS: u64 = 86_400_000;
        for day in days {
            for subject in [9_001_u64, 9_002] {
                meter.record_qualifying(Some(account(subject)));
                meter.record(obs(subject, ShadowVerdict::WouldAdmit, day * DAY_MS));
            }
        }
    }

    /// #990's headline property, against a real cluster: a `persistd` restart
    /// **continues** the measurement window instead of opening a new one.
    ///
    /// The window bounds and both halves' counters are read back out of
    /// FoundationDB by a meter that never saw the traffic, and the resulting
    /// `W` spans the whole run rather than the segment since the restart.
    #[tokio::test]
    async fn a_window_survives_a_simulated_persistd_restart() {
        const DAY_MS: u64 = 86_400_000;
        let Some(_) = fdb_cluster_file() else {
            return;
        };
        let control = "test_window_restart";
        let store = store();
        store.clear(control).await.expect("clear");
        let cohort = cohort();

        // ── The process that ran for the first twenty days ────────────────
        let before = RampMeter::new("attestation_quorum");
        if let Some(row) = store.load(control).await.expect("load") {
            before.restore(row);
        }
        run_traffic(&before, 0..20);
        // One would-have-acted event per half — the distinction a promotion
        // review turns on.
        for subject in [9_001_u64, 9_002] {
            before.record_qualifying(Some(account(subject)));
            before.record(obs(
                subject,
                ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet),
                19 * DAY_MS,
            ));
        }

        let delta = before.take_delta(&cohort);
        let FlushOutcome::Applied(row) = store.flush(control, &delta, 1_000).await.expect("flush")
        else {
            panic!("nothing else writes this control's window");
        };
        before.commit_flush(*row);
        let observed_before = before.snapshot(&cohort);

        // ── The deploy ────────────────────────────────────────────────────
        drop(before);

        // ── The process that came up after it ─────────────────────────────
        let after = RampMeter::new("attestation_quorum");
        let reloaded = store
            .load(control)
            .await
            .expect("load")
            .expect("the window the previous process flushed");
        assert_eq!(reloaded.window_id, 0, "no reset happened, so no new window");
        assert_eq!(reloaded.flushes, 1);
        after.restore(reloaded);

        let continued = after.snapshot(&cohort);
        assert_eq!(continued.observed_from_ms, 0);
        assert_eq!(continued.observed_to_ms, 19 * DAY_MS);
        assert!(
            (continued.window_days - 19.0).abs() < 1e-9,
            "the window did not restart: it is {} days",
            continued.window_days
        );
        assert_eq!(continued.qualifying, observed_before.qualifying);
        assert_eq!(continued.observed, observed_before.observed);
        assert_eq!(
            continued.cohort.qualifying,
            observed_before.cohort.qualifying
        );
        assert_eq!(continued.cohort.coverage, observed_before.cohort.coverage);

        // Both halves came back apart, which is the whole point of storing
        // them apart.
        let carried = after.durable_window();
        assert_eq!(carried.counts.armed.would_act, 1);
        assert_eq!(carried.counts.natural.would_act, 1);
        assert_eq!(continued.cohort.fp_count, 2);
        assert_eq!(continued.cohort.accounts_would_act, 2);
        assert_eq!(continued.cohort.active, 2);

        // ── Eleven more days, and clause (e)'s `W ≥ 30 days` is reached ───
        run_traffic(&after, 20..31);
        let delta = after.take_delta(&cohort);
        let FlushOutcome::Applied(row) = store.flush(control, &delta, 2_000).await.expect("flush")
        else {
            panic!("still nothing else writes it");
        };
        after.commit_flush(*row);

        let whole = after.snapshot(&cohort);
        assert!(
            whole.window_days >= 30.0,
            "a thirty-day window across a restart is what #990 said was \
             structurally unreachable; it is {} days",
            whole.window_days
        );
        assert_eq!(whole.cohort.fp_count, 2);

        // And it is durable, not merely in this process.
        let final_row = store.load(control).await.expect("load").expect("row");
        assert!((final_row.window_days() - 30.0).abs() < 1e-9);
        assert_eq!(final_row.counts.armed.would_act, 1);
        assert_eq!(final_row.counts.natural.would_act, 1);
        assert_eq!(final_row.flushes, 2);

        store.clear(control).await.expect("clear");
    }

    /// A deliberate reset opens a new generation, and a delta measured under
    /// the old one is refused rather than folded in — which is what makes the
    /// reset mean "prior observations do not count".
    #[tokio::test]
    async fn a_reset_retires_the_window_and_refuses_an_in_flight_delta() {
        let Some(_) = fdb_cluster_file() else {
            return;
        };
        let control = "test_window_reset";
        let store = store();
        store.clear(control).await.expect("clear");
        let cohort = cohort();

        let meter = RampMeter::new("attestation_quorum");
        run_traffic(&meter, 0..5);
        let first = meter.take_delta(&cohort);
        let FlushOutcome::Applied(row) = store.flush(control, &first, 10).await.expect("flush")
        else {
            panic!("first flush opens generation 0");
        };
        assert_eq!(row.window_id, 0);
        meter.commit_flush(*row);

        // More traffic, drained but not yet written.
        run_traffic(&meter, 5..8);
        let in_flight = meter.take_delta(&cohort);
        assert_eq!(in_flight.window_id, 0);

        // The operator resets, because the ruleset changed underneath.
        let fresh = store
            .reset(control, "ruleset v9 invalidates prior observations", 99_000)
            .await
            .expect("reset");
        assert_eq!(fresh.window_id, 1, "generations climb, they do not restart");
        assert_eq!(fresh.opened_at_ms, 99_000);
        assert!(fresh.counts.is_empty());
        assert_eq!(
            fresh.reset_reason.as_deref(),
            Some("ruleset v9 invalidates prior observations")
        );

        // The in-flight delta is refused, and the process is handed the new
        // window instead.
        let outcome = store
            .flush(control, &in_flight, 100_000)
            .await
            .expect("flush");
        let FlushOutcome::WindowChanged(row) = outcome else {
            panic!("a delta from a retired generation must not be applied");
        };
        assert_eq!(row.window_id, 1);
        assert!(
            row.counts.is_empty(),
            "pre-reset observations did not leak into the window opened to \
             exclude them"
        );
        meter.adopt_window(*row);
        assert_eq!(meter.window_id(), 1);
        let after_reset = meter.snapshot(&cohort);
        assert_eq!(after_reset.cohort.qualifying, 0);
        assert!(after_reset.window_days.abs() < f64::EPSILON);

        // And a reset before any traffic opens generation 0 with the reason
        // on it, rather than skipping a generation nothing observed.
        store.clear(control).await.expect("clear");
        let virgin = store.reset(control, "bring-up", 5).await.expect("reset");
        assert_eq!(virgin.window_id, 0);

        store.clear(control).await.expect("clear");
    }

    /// Two processes metering the same control fold into one row, which is why
    /// the flush is a read-modify-write and not an absolute overwrite.
    #[tokio::test]
    async fn two_processes_fold_into_one_window() {
        const DAY_MS: u64 = 86_400_000;
        let Some(_) = fdb_cluster_file() else {
            return;
        };
        let control = "test_window_two_writers";
        let store = store();
        store.clear(control).await.expect("clear");
        let cohort = cohort();

        let left = RampMeter::new("attestation_quorum");
        let right = RampMeter::new("attestation_quorum");
        run_traffic(&left, 0..3);
        run_traffic(&right, 10..13);

        for meter in [&left, &right] {
            let delta = meter.take_delta(&cohort);
            let FlushOutcome::Applied(row) = store.flush(control, &delta, 1).await.expect("flush")
            else {
                panic!("both write the same generation");
            };
            meter.commit_flush(*row);
        }

        let row = store.load(control).await.expect("load").expect("row");
        assert_eq!(row.flushes, 2);
        assert_eq!(row.counts.first_ms, Some(0), "the earlier writer's bound");
        assert_eq!(
            row.counts.last_ms,
            Some(12 * DAY_MS),
            "and the later writer's, rather than whichever wrote last"
        );
        assert_eq!(row.counts.armed.qualifying, 6);
        assert_eq!(row.counts.natural.qualifying, 6);
        assert_eq!(
            row.counts.armed_active.len(),
            1,
            "one armed member seen by both processes is one active member"
        );

        // The second writer's snapshot now reports the fleet's window, not its
        // own share of it.
        let seen = right.snapshot(&cohort);
        assert_eq!(seen.cohort.qualifying, 12);
        assert!((seen.window_days - 12.0).abs() < 1e-9);

        store.clear(control).await.expect("clear");
    }

    /// The ruleset stamp folds across flushes the way every other window
    /// field does: two processes, each observing a different pair of
    /// rulesets, leave one row naming all three — so a window that spanned a
    /// change says so durably, per the owner's 2026-09-03 decision on D32
    /// open question 6.
    #[tokio::test]
    async fn a_ruleset_stamp_folds_across_flushes_like_the_other_fields() {
        let Some(_) = fdb_cluster_file() else {
            return;
        };
        let control = "test_window_rulesets";
        let store = store();
        store.clear(control).await.expect("clear");
        let cohort = cohort();

        let v9 = RulesetId {
            version: 9,
            digest: [0xAA; 32],
        };
        let v10 = RulesetId {
            version: 10,
            digest: [0xBB; 32],
        };
        let v11 = RulesetId {
            version: 11,
            digest: [0xCC; 32],
        };

        let before = RampMeter::new("attestation_quorum");
        before.observe_ruleset(v9);
        before.observe_ruleset(v10);
        run_traffic(&before, 0..2);
        let first = before.take_delta(&cohort);
        let FlushOutcome::Applied(row) = store.flush(control, &first, 1).await.expect("flush")
        else {
            panic!("nothing else writes this control's window");
        };
        before.commit_flush(*row);

        // The change: the second flush observes v10 again — which must stay
        // one entry — and a new v11, which must join it.
        let after = RampMeter::new("attestation_quorum");
        after.restore(store.load(control).await.expect("load").expect("row"));
        after.observe_ruleset(v10);
        after.observe_ruleset(v11);
        run_traffic(&after, 2..4);
        let second = after.take_delta(&cohort);
        let FlushOutcome::Applied(row) = store.flush(control, &second, 2).await.expect("flush")
        else {
            panic!("same generation, so the delta applies");
        };
        assert_eq!(row.flushes, 2);
        assert_eq!(
            row.counts.ruleset_ids,
            [v9, v10, v11].into_iter().collect(),
            "the row names every ruleset the window observed, across both flushes"
        );
        assert_eq!(row.counts.rulesets_truncated, 0);
        after.commit_flush(*row);

        store.clear(control).await.expect("clear");
    }
}
