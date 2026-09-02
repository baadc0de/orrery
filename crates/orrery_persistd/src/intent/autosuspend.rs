//! D32 clause (f)'s circuit breaker: the verdict-rate monitor, and the
//! asymmetry that makes it safe to run unattended
//! ([D32](../../../../docs/adr/0032-enforcement-ramp.md)).
//!
//! # What this is protecting against
//!
//! [docs/07:237](../../../../docs/07-witnessing.md) names the scenario this
//! machinery exists for: *"a `Ruleset` bug that makes honest clients fail
//! replay would strike everyone"*. A control that has started misfiring does
//! not look like an attack from inside the control — every individual verdict
//! is exactly what the predicate says it should be. It looks like a *rate*:
//! suddenly many unrelated accounts are being acted on at once. So the
//! detector is a rate detector, and its terms are chosen to separate that
//! shape from the shapes that are not bugs.
//!
//! # The predicate, verbatim from the record
//!
//! ```text
//! suspend(C [, v]) ⟺ spread ≥ 8 distinct accounts
//!                   ∧ rate  ≥ max(10 × median₇d(C [, v]),  25 events/h)
//!    over a sliding 60-minute window
//!    spread  = distinct accounts with a would-have-acted (or acted) event
//!    rate    = events per hour, same window, same scope
//!    median₇d= trailing 7-day hourly median of the same counter
//! ```
//!
//! Each term is a filter against a specific false positive:
//!
//! - **Spread ≥ 8 accounts** rejects the flood. A ruleset bug strikes
//!   everyone; an attacker concentrates. Because the counter is *cardinality*,
//!   one account submitting a million bad intents contributes exactly one to
//!   it — "floods are what per-account rate limits are for".
//! - **Rate ≥ 10 × the control's own trailing median** rejects the busy
//!   afternoon: the baseline moves with real traffic, so a control that
//!   normally acts 200 times an hour is not tripped by 400.
//! - **Floored at 25 events/h** rejects the quiet Tuesday, where `10 ×
//!   median` is `10 × 0`. Without the floor a single-digit trickle on a dead
//!   cluster would trip the breaker every time.
//! - **The 60-minute window** is what makes all three a rate rather than a
//!   total.
//!
//! The two constants have no derivation and the record says so, in D29's
//! `C = 8` tradition — set low enough that a genuine bug trips within minutes,
//! high enough that a quiet period's noise does not.
//!
//! # The asymmetry, and why it is a type and not a habit
//!
//! Clause (f): *automation may make the fleet safer without asking, never less
//! safe.* Anything that can demote a control fleet-wide can, if it is wrong or
//! captured, also **promote** one — and a promotion nobody reviewed is exactly
//! what D32 clause (e)'s whole review gate exists to prevent. Worse, a trigger
//! that could write `off` would be a denial-of-service lever *against
//! enforcement*: induce a spike, blind the cluster, act freely in the dark.
//!
//! So the asymmetry is enforced at three points, and no two of them are the
//! same kind of check:
//!
//! 1. **The target mode is not an input.** [`Demotion::new`] takes the mode
//!    the control is *leaving* and returns `None` unless that mode is
//!    [`RampMode::Live`]. There is no parameter, anywhere in this module's
//!    API, that names where a control ends up — [`Demotion::to`] is a
//!    constant. A monitor cannot ask for a promotion incorrectly because it
//!    cannot phrase one.
//! 2. **The row is checked at the reader.** [`RampPosture::admissible`]
//!    refuses any durable row whose `source` is
//!    [`PostureSource::AutoSuspend`] and whose `mode` is not
//!    [`RampMode::Shadow`], and every poller calls it before applying a row.
//!    This is the check that survives a compromised monitor: it does not
//!    matter which process wrote the row or how, because the *consumer*
//!    re-derives the constraint. It is the same reader-side placement
//!    [#932](https://github.com/baadc0de/orrery/pull/932) argues for on the
//!    operator's row, and it needs no signature — a forged demotion is still
//!    a demotion, and clause (f) permits those without asking.
//! 3. **The in-process cell refuses too.**
//!    [`super::AttestationPosture::auto_suspend`] is a compare-exchange from
//!    `Required`, so even a caller holding the cell cannot drive it upward.
//!
//! Point 2 is the load-bearing one. Points 1 and 3 constrain *this* code;
//! point 2 constrains every writer that will ever exist, including one that
//! bypasses this module entirely and writes FoundationDB by hand.
//!
//! # Why `Off` is refused as hard as `Live`
//!
//! Falling to `off` is not "extra safe". Shadow keeps evaluating and keeps
//! recording, and *the incident is the calibration data* — the traffic that
//! tripped the breaker is the most informative traffic the control will ever
//! see. Falling to `off` throws away the evidence that explains the outage,
//! during the outage. [`Demotion::to`] is `Shadow` and there is no path to
//! any other value.
//!
//! # What this deliberately does not do
//!
//! - **No cross-process aggregation.** D32 open question 4 defers it pending
//!   [#221]'s per-gateway numbers, so every [`SuspendMonitor`] counts only
//!   what its own process observed. The *effect* is still fleet-wide, which
//!   the record accepts in as many words ("a single noisy gateway can demote a
//!   control for every gateway… falling to shadow is safe and observable").
//! - **No promotion, and no timer.** Nothing here re-arms enforcement.
//!   Returning a suspended control to live is an operator act, reviewed under
//!   clause (e) like any first promotion.
//! - **No second enforcement point.** This module computes a verdict and
//!   yields a [`Demotion`]; it never refuses an intent, never ends a session,
//!   and never consults a posture in order to act on a peer. The bug
//!   [#934](https://github.com/baadc0de/orrery/pull/934) fixed — an
//!   enforcement arm that skipped the posture check — is not reachable from a
//!   type that cannot enforce anything.
//!
//! [#221]: https://github.com/baadc0de/orrery/issues/221

use std::collections::{BTreeSet, VecDeque};

use orrery_protocol::{AccountId, RulesetId};
use serde::{Deserialize, Serialize};

use super::ramp::{PostureSource, RampMode, RampPosture};
use super::shadow::NetworkQuality;

/// Clause (f)'s account-cardinality term: distinct accounts required.
pub const SPREAD_THRESHOLD: usize = 8;

/// Clause (f)'s absolute floor, in events per hour.
pub const RATE_FLOOR_PER_HOUR: u64 = 25;

/// Clause (f)'s multiple of the trailing baseline.
pub const MEDIAN_MULTIPLE: u64 = 10;

/// Clause (f)'s sliding window: 60 minutes.
pub const WINDOW_MS: u64 = 60 * 60 * 1_000;

/// Clause (f)'s baseline horizon: a trailing 7 days of hourly counts.
pub const BASELINE_HOURS: usize = 7 * 24;

/// The most events one window retains.
///
/// Bounded for the reason [`super::ramp::DEFAULT_ACCOUNT_CAPACITY`] is
/// bounded: this queue is fed from the admission path, and an unbounded
/// structure keyed on peer-driven traffic is a memory leak with an incident
/// behind it. Overflow is *reported* — [`WindowStats::truncated`] is nonzero
/// exactly when the rate is understated — and understating the rate can only
/// fail to trip the breaker, never trip it wrongly.
pub const DEFAULT_WINDOW_CAPACITY: usize = 100_000;

/// A trip's incident handle.
///
/// A newtype rather than a bare `[u8; 16]` so it cannot be swapped with any
/// other 16-byte identifier at a call site, and so the durable row and the log
/// line provably carry the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IncidentId(pub [u8; 16]);

impl IncidentId {
    /// Derive the handle from the trip's own facts.
    ///
    /// Deterministic rather than random: two processes that observed the same
    /// trip should name it the same way, and a test must be able to assert an
    /// incident id without a seam for a clock or an RNG.
    #[must_use]
    pub fn of(scope: &TriggerScope, at_ms: u64) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"orrery.autosuspend.incident/1");
        hasher.update(scope.control().as_bytes());
        match scope.ruleset() {
            Some(ruleset) => {
                hasher.update(&[1]);
                hasher.update(&ruleset.version.to_le_bytes());
                hasher.update(&ruleset.digest);
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(&at_ms.to_le_bytes());
        let mut id = [0_u8; 16];
        id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        Self(id)
    }

    /// Lowercase hex, for the row's reason string and the log line.
    #[must_use]
    pub fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// The scope one monitor counts over.
///
/// Clause (f): the trigger is per control C, *and* per `RulesetId` v "where
/// the control is verdict-driven — C3, C4, C5"; "C1 and C2 are protocol-level
/// and suspend globally". A newtype rather than a `(&str, Option<RulesetId>)`
/// tuple so the two cases are named where they are used and cannot be
/// assembled in the wrong order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TriggerScope {
    control: &'static str,
    ruleset: Option<RulesetId>,
}

impl TriggerScope {
    /// A protocol-level control, which suspends globally (C1, C2).
    #[must_use]
    pub const fn global(control: &'static str) -> Self {
        Self {
            control,
            ruleset: None,
        }
    }

    /// A verdict-driven control, scoped to one rule version (C3, C4, C5).
    #[must_use]
    pub const fn per_ruleset(control: &'static str, ruleset: RulesetId) -> Self {
        Self {
            control,
            ruleset: Some(ruleset),
        }
    }

    /// The control this scope names.
    #[must_use]
    pub const fn control(&self) -> &'static str {
        self.control
    }

    /// The rule version, for a verdict-driven control.
    #[must_use]
    pub const fn ruleset(&self) -> Option<RulesetId> {
        self.ruleset
    }
}

/// One would-have-acted (or acted) event, as clause (f) counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlEvent {
    /// The subject account, or `None` for an unauthenticated submission.
    ///
    /// `None` contributes to the rate and **not** to the spread, because
    /// clause (f) defines spread as *distinct accounts* and an unattributed
    /// event names none. Counting it toward cardinality would let one
    /// unauthenticated flood manufacture the account spread the term exists to
    /// require.
    pub subject: Option<AccountId>,
    /// The network-quality bucket of the connection the event arrived on.
    pub network: NetworkQuality,
    /// When it was observed, in the monitor's clock units.
    pub at_ms: u64,
}

/// Why a trip fired, as recorded in the incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TripReason {
    /// The rate and spread terms breached against ordinary-looking traffic.
    RateSpike,
    /// The same breach, with the events concentrated on impaired paths well
    /// beyond that population's share of the traffic the control evaluated.
    ///
    /// R-6's early warning, and the reason clause (f) calls the RTT/loss
    /// dimension required: *"that shape is packet loss wearing a cheat
    /// costume, exactly the false positive this machinery exists to
    /// prevent"*. It fires the same trip — the discriminant tells the operator
    /// which incident they are reading, and points the post-mortem at the
    /// transport rather than at the accounts.
    NetworkCorrelated,
}

impl TripReason {
    /// The stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateSpike => "rate_spike",
            Self::NetworkCorrelated => "network_correlated",
        }
    }
}

/// What the window currently holds, as the predicate reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowStats {
    /// Distinct accounts with an event in the window — clause (f)'s `spread`.
    pub spread: usize,
    /// Events in the window — clause (f)'s `rate`, the window being one hour.
    pub rate: u64,
    /// The trailing 7-day hourly median this rate was compared against.
    pub median_7d: u64,
    /// The threshold `rate` had to reach: `max(10 × median, 25)`.
    pub rate_threshold: u64,
    /// Events on a measured, impaired path.
    pub impaired: u64,
    /// Events on any measured path, impaired or not.
    pub measured: u64,
    /// Events dropped for capacity, which understate `rate` and `spread`.
    pub truncated: u64,
}

impl WindowStats {
    /// Whether both of clause (f)'s terms are satisfied.
    #[must_use]
    pub const fn breaches(&self) -> bool {
        self.spread >= SPREAD_THRESHOLD && self.rate >= self.rate_threshold
    }

    /// Whether the breach concentrates on impaired paths.
    ///
    /// A majority of the *measured* events on impaired paths. Deliberately
    /// only a label on a trip that has already fired on clause (f)'s own two
    /// terms, never a third firing condition: an independent network arm would
    /// need a correlation threshold and a minimum-sample threshold, and D32
    /// supplies numbers for neither. P4's primary tunable is the false
    /// positive rate ([D17](../../../../docs/adr/0017-risks-and-open-questions.md)
    /// risk 3), so a term that can only *add* trips does not get invented here.
    #[must_use]
    pub const fn network_correlated(&self) -> bool {
        self.measured > 0 && self.impaired * 2 > self.measured
    }

    /// The reason to record for a trip with these statistics.
    #[must_use]
    pub const fn reason(&self) -> TripReason {
        if self.network_correlated() {
            TripReason::NetworkCorrelated
        } else {
            TripReason::RateSpike
        }
    }
}

/// A posture move automation is permitted to make.
///
/// **The destination is not a field.** [`Self::new`] accepts the mode being
/// left and nothing else, [`Self::to`] is a constant, and there is no
/// constructor, setter or deserialisation path that names a different target.
/// This is what makes clause (f)'s asymmetry a property of the type rather
/// than of the care taken at each call site: a caller cannot write the wrong
/// thing, because the wrong thing is unspellable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demotion {
    control: String,
    ruleset: Option<RulesetId>,
    from: RampMode,
    incident: IncidentId,
    reason: TripReason,
    stats: WindowStats,
    at_ms: u64,
}

impl Demotion {
    /// The only constructor.
    ///
    /// `None` unless `from` is [`RampMode::Live`], which is the whole
    /// asymmetry in one line:
    ///
    /// - `from == Live` → the control is acting, and suspending it is the
    ///   safer direction. Permitted.
    /// - `from == Shadow` → already suspended; there is no action left to
    ///   suspend, and re-writing the row would restamp an older incident with
    ///   a newer one.
    /// - `from == Off` → the control evaluates nothing. Moving it to `Shadow`
    ///   would be automation *starting* an evaluation nobody asked for, which
    ///   is a promotion in every sense that matters: it begins spending the
    ///   admission path's budget on a control an operator switched off.
    #[must_use]
    pub fn new(
        scope: &TriggerScope,
        from: RampMode,
        stats: WindowStats,
        at_ms: u64,
    ) -> Option<Self> {
        if !matches!(from, RampMode::Live) {
            return None;
        }
        Some(Self {
            control: scope.control().to_owned(),
            ruleset: scope.ruleset(),
            from,
            incident: IncidentId::of(scope, at_ms),
            reason: stats.reason(),
            stats,
            at_ms,
        })
    }

    /// Where the control ends up. Always [`RampMode::Shadow`].
    ///
    /// An associated constant in function's clothing. It takes `&self` only so
    /// it reads as a property of the demotion at the call sites that pair it
    /// with [`Self::from`].
    #[must_use]
    pub const fn to(&self) -> RampMode {
        RampMode::Shadow
    }

    /// The mode the control is leaving.
    #[must_use]
    pub const fn from(&self) -> RampMode {
        self.from
    }

    /// The control being suspended.
    #[must_use]
    pub fn control(&self) -> &str {
        &self.control
    }

    /// The rule version, for a verdict-driven control.
    #[must_use]
    pub const fn ruleset(&self) -> Option<RulesetId> {
        self.ruleset
    }

    /// The incident handle an operator's return-to-live clears.
    #[must_use]
    pub const fn incident(&self) -> IncidentId {
        self.incident
    }

    /// Why it tripped.
    #[must_use]
    pub const fn reason(&self) -> TripReason {
        self.reason
    }

    /// The statistics the predicate read.
    #[must_use]
    pub const fn stats(&self) -> WindowStats {
        self.stats
    }

    /// The human-readable reason string for the durable row.
    ///
    /// Every number the predicate branched on, so an operator reading the row
    /// can re-check the arithmetic without the process that wrote it.
    #[must_use]
    pub fn reason_text(&self) -> String {
        let text = format!(
            "autosuspend {}: spread {} >= {}, rate {}/h >= {} (10x median {} floored at {}), \
             impaired {}/{} measured, incident {}",
            self.reason.as_str(),
            self.stats.spread,
            SPREAD_THRESHOLD,
            self.stats.rate,
            self.stats.rate_threshold,
            self.stats.median_7d,
            RATE_FLOOR_PER_HOUR,
            self.stats.impaired,
            self.stats.measured,
            self.incident.to_hex(),
        );
        // The row's `reason` is capped at 256 bytes by every writer, and a
        // truncation must not split a UTF-8 sequence.
        truncate_utf8(&text, 256)
    }

    /// The durable row this demotion writes.
    ///
    /// Constructed here rather than by the caller so `mode` and `source`
    /// cannot be paired wrongly: this is the only place in the tree that
    /// builds a [`PostureSource::AutoSuspend`] row, and it hardcodes
    /// [`Self::to`].
    #[must_use]
    pub fn posture(&self) -> RampPosture {
        RampPosture {
            mode: self.to(),
            source: PostureSource::AutoSuspend,
            set_at_ms: self.at_ms,
            reason: self.reason_text(),
            incident_id: Some(self.incident.0),
        }
    }
}

/// Truncate to at most `limit` bytes without splitting a character.
fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Clause (f)'s per-control monitor: a sliding window, a trailing baseline,
/// and a predicate.
///
/// # Cost
///
/// One `VecDeque` push and an amortised prune per observed event, plus a
/// `BTreeSet` build per *verdict*. The verdict is computed on the poll tick,
/// not on the admission path — clause (c) bounds the fleet-wide effect at a
/// poll interval, so there is nothing to gain from evaluating the predicate
/// per intent and a `O(window)` set build per intent would be a real cost
/// against D16's 10 ms commit p99.
///
/// # Clock
///
/// Every entry point takes the time explicitly. The monitor holds no clock,
/// which is what lets a test drive a synthetic 60-minute burst in
/// microseconds, and what keeps the 7-day baseline from depending on wall time
/// the process cannot reproduce.
#[derive(Debug)]
pub struct SuspendMonitor {
    scope: TriggerScope,
    capacity: usize,
    /// Events inside the sliding window, oldest first.
    window: VecDeque<ControlEvent>,
    /// Events dropped for capacity since construction.
    truncated: u64,
    /// Completed hourly counts, oldest first, capped at [`BASELINE_HOURS`].
    baseline: VecDeque<u64>,
    /// The hour index currently accumulating, and its count so far.
    current_hour: Option<u64>,
    current_hour_count: u64,
    /// Qualifying traffic by measured/impaired, for the correlation label.
    qualifying_measured: u64,
    qualifying_impaired: u64,
}

impl SuspendMonitor {
    /// A monitor for `scope` with the default window capacity.
    #[must_use]
    pub fn new(scope: TriggerScope) -> Self {
        Self::with_capacity(scope, DEFAULT_WINDOW_CAPACITY)
    }

    /// A monitor keeping at most `capacity` events in its window.
    #[must_use]
    pub fn with_capacity(scope: TriggerScope, capacity: usize) -> Self {
        Self {
            scope,
            capacity: capacity.max(1),
            window: VecDeque::new(),
            truncated: 0,
            baseline: VecDeque::new(),
            current_hour: None,
            current_hour_count: 0,
            qualifying_measured: 0,
            qualifying_impaired: 0,
        }
    }

    /// The scope this monitor counts over.
    #[must_use]
    pub const fn scope(&self) -> &TriggerScope {
        &self.scope
    }

    /// Count one admission decision the control evaluated, for the
    /// correlation label's denominator.
    ///
    /// Separate from [`Self::observe`] for the reason
    /// [`super::ramp::RampMeter`] counts at two points: "what fraction of
    /// would-be actions were on bad paths" is only meaningful against "what
    /// fraction of *all* traffic was on bad paths", and a ratio whose
    /// numerator and denominator come from one call site is not a ratio.
    pub fn qualify(&mut self, network: NetworkQuality) {
        if network.is_measured() {
            self.qualifying_measured += 1;
            if network.is_impaired() {
                self.qualifying_impaired += 1;
            }
        }
    }

    /// Record one would-have-acted (or acted) event.
    pub fn observe(&mut self, event: ControlEvent) {
        self.advance_to(event.at_ms);
        self.current_hour_count += 1;
        self.window.push_back(event);
        if self.window.len() > self.capacity {
            self.window.pop_front();
            self.truncated += 1;
        }
        self.prune(event.at_ms);
    }

    /// The window as the predicate reads it at `now_ms`.
    #[must_use]
    pub fn stats(&self, now_ms: u64) -> WindowStats {
        let floor = now_ms.saturating_sub(WINDOW_MS);
        let live = self.window.iter().filter(|event| event.at_ms > floor);
        let mut spread = BTreeSet::new();
        let (mut rate, mut impaired, mut measured) = (0_u64, 0_u64, 0_u64);
        for event in live {
            rate += 1;
            if let Some(account) = event.subject {
                spread.insert(account);
            }
            if event.network.is_measured() {
                measured += 1;
                if event.network.is_impaired() {
                    impaired += 1;
                }
            }
        }
        let median = self.median_7d();
        WindowStats {
            spread: spread.len(),
            rate,
            median_7d: median,
            rate_threshold: median
                .saturating_mul(MEDIAN_MULTIPLE)
                .max(RATE_FLOOR_PER_HOUR),
            impaired,
            measured,
            truncated: self.truncated,
        }
    }

    /// Clause (f)'s trailing 7-day hourly median.
    ///
    /// Over *completed* hours only: the hour in progress is a partial count
    /// and folding it in would drag the median down on every poll, inflating
    /// `10 × median`'s discrimination exactly when the window is busiest.
    ///
    /// A fresh process has no history, so the median is `0` and the floor
    /// governs. That is the intended behaviour and not a gap — it is why
    /// clause (f) has a floor at all.
    #[must_use]
    pub fn median_7d(&self) -> u64 {
        if self.baseline.is_empty() {
            return 0;
        }
        let mut hours: Vec<u64> = self.baseline.iter().copied().collect();
        hours.sort_unstable();
        let middle = hours.len() / 2;
        if hours.len() % 2 == 1 {
            hours[middle]
        } else {
            // The lower of the two middles rather than their mean: the median
            // is a multiplier for a threshold, it must stay an integer, and
            // rounding down makes the threshold lower, which is the direction
            // that trips sooner. Automation tripping sooner is the safe error.
            hours[middle - 1]
        }
    }

    /// The demotion clause (f) calls for, given the control's current mode.
    ///
    /// `None` when the predicate does not breach **or** when `from` is not
    /// [`RampMode::Live`]. Both refusals go through [`Demotion::new`], so
    /// there is exactly one place in the tree that decides an auto-suspend is
    /// permitted.
    #[must_use]
    pub fn verdict(&self, from: RampMode, now_ms: u64) -> Option<Demotion> {
        let stats = self.stats(now_ms);
        if !stats.breaches() {
            return None;
        }
        Demotion::new(&self.scope, from, stats, now_ms)
    }

    /// Roll completed hours into the baseline up to `at_ms`.
    fn advance_to(&mut self, at_ms: u64) {
        let hour = at_ms / (60 * 60 * 1_000);
        match self.current_hour {
            None => self.current_hour = Some(hour),
            Some(current) if hour > current => {
                self.push_baseline(self.current_hour_count);
                // Every whole hour between the two saw no events at all, and a
                // silent hour is a real `0` in the baseline. Skipping them
                // would make the median describe only the busy hours, which is
                // the median of a different population.
                let gap = (hour - current - 1).min(BASELINE_HOURS as u64);
                for _ in 0..gap {
                    self.push_baseline(0);
                }
                self.current_hour_count = 0;
                self.current_hour = Some(hour);
            }
            Some(_) => {}
        }
    }

    fn push_baseline(&mut self, count: u64) {
        self.baseline.push_back(count);
        while self.baseline.len() > BASELINE_HOURS {
            self.baseline.pop_front();
        }
    }

    /// Drop events that have fallen out of the sliding window.
    fn prune(&mut self, now_ms: u64) {
        let floor = now_ms.saturating_sub(WINDOW_MS);
        while self
            .window
            .front()
            .is_some_and(|event| event.at_ms <= floor)
        {
            self.window.pop_front();
        }
    }
}

/// The runtime lever clause (f)'s trip pulls.
///
/// A trait rather than a concrete type because D32 clause (c) gives *each*
/// control a lever and they are not the same object: C1's is
/// [`super::AttestationPosture`], C5's is the gateway's `StrikesPosture`, and
/// a durable `ramp/{control}` writer is a third. The monitor is written once
/// against the shape they share.
///
/// # The implementor's obligation
///
/// [`Self::suspend`] must move the control from acting to *shadow* and must be
/// incapable of any other transition. Implementations discharge this with a
/// compare-exchange from the acting code, not with a store — a store would let
/// a racing operator promotion be silently reverted by a monitor that read the
/// mode a moment earlier, and clause (f) gives automation no authority to undo
/// an operator's act.
pub trait ControlLever {
    /// The control's mode right now.
    fn mode(&self) -> RampMode;

    /// Demote an acting control to shadow, returning whether it moved.
    ///
    /// Returns `false` for a control that is not acting. That is the ordinary
    /// answer, not an error: two monitors seeing the same incident, or one
    /// monitor polling twice, must not restamp the incident.
    fn suspend(&self) -> bool;
}

impl SuspendMonitor {
    /// Evaluate the predicate against `lever` and pull it if it breaches.
    ///
    /// The mode is read from the lever rather than passed in, so the decision
    /// and the action see one value: a caller that read `Live`, waited, and
    /// then demoted could suspend a control an operator had meanwhile switched
    /// off. Returns the [`Demotion`] only when the posture actually moved, so
    /// a caller logging the incident cannot log one that did not happen.
    ///
    /// This is the *whole* action. It writes no row, refuses no intent and
    /// ends no session — the trip's only effect is that a control stops
    /// acting, which is the one effect clause (f) authorises.
    #[must_use]
    pub fn trip(&self, lever: &dyn ControlLever, now_ms: u64) -> Option<Demotion> {
        let demotion = self.verdict(lever.mode(), now_ms)?;
        if !lever.suspend() {
            // The mode moved between the read and the pull — an operator
            // demoted or switched it off first. Nothing to report.
            return None;
        }
        tracing::warn!(
            target: super::shadow::SHADOW_TARGET,
            control = demotion.control(),
            incident = %demotion.incident().to_hex(),
            reason = demotion.reason().as_str(),
            spread = demotion.stats().spread,
            rate = demotion.stats().rate,
            rate_threshold = demotion.stats().rate_threshold,
            median_7d = demotion.stats().median_7d,
            impaired = demotion.stats().impaired,
            measured = demotion.stats().measured,
            from = ?demotion.from(),
            to = ?demotion.to(),
            "D32 clause (f): auto-suspend demoted an acting control to shadow"
        );
        Some(demotion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::shadow::{PathSample, ATTESTATION_QUORUM_CONTROL};

    const HOUR_MS: u64 = 60 * 60 * 1_000;

    /// A realistic wall clock for the tests.
    ///
    /// Not zero: the sliding window is half-open — an event is in it when
    /// `at_ms > now_ms - WINDOW_MS` — and near the epoch that lower bound
    /// saturates at `0`, which would silently drop an event stamped `0`. Real
    /// callers pass Unix milliseconds, so the tests do too rather than
    /// asserting against a boundary no deployment can reach.
    const T0: u64 = 1_735_689_600_000;

    fn scope() -> TriggerScope {
        TriggerScope::global(ATTESTATION_QUORUM_CONTROL)
    }

    fn monitor() -> SuspendMonitor {
        SuspendMonitor::new(scope())
    }

    /// `count` events spread over `accounts` distinct accounts, one per
    /// millisecond from `start_ms`, all on clean paths.
    fn burst(monitor: &mut SuspendMonitor, start_ms: u64, count: u64, accounts: u64) {
        burst_on(monitor, start_ms, count, accounts, NetworkQuality::Clean);
    }

    fn burst_on(
        monitor: &mut SuspendMonitor,
        start_ms: u64,
        count: u64,
        accounts: u64,
        network: NetworkQuality,
    ) {
        for index in 0..count {
            monitor.observe(ControlEvent {
                subject: (accounts > 0).then(|| AccountId(index % accounts.max(1))),
                network,
                at_ms: start_ms + index,
            });
        }
    }

    // -----------------------------------------------------------------------
    // Clause (f)'s predicate: every term earns its place
    //
    // The mutation check the issue asks for is structural rather than
    // narrative: each of the next four tests holds two terms at a passing
    // value and puts the third one increment short. Break `SPREAD_THRESHOLD`
    // and `a_burst_one_account_short_of_the_spread_term_does_not_trip` fails;
    // break `RATE_FLOOR_PER_HOUR` and
    // `a_burst_one_event_short_of_the_floor_does_not_trip` fails; break
    // `MEDIAN_MULTIPLE` and `a_burst_under_ten_times_the_median_does_not_trip`
    // fails; break `WINDOW_MS` and `events_that_left_the_window_do_not_count`
    // fails.
    // -----------------------------------------------------------------------

    /// The whole predicate, satisfied.
    #[test]
    fn a_burst_meeting_every_term_demotes_the_control() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 40, 10);
        let stats = monitor.stats(T0 + 1_000);
        assert_eq!(stats.spread, 10);
        assert_eq!(stats.rate, 40);
        assert_eq!(stats.median_7d, 0, "a fresh process has no baseline");
        assert_eq!(
            stats.rate_threshold, RATE_FLOOR_PER_HOUR,
            "so the absolute floor governs, which is why clause (f) has one"
        );
        assert!(stats.breaches());

        let demotion = monitor
            .verdict(RampMode::Live, 1_000)
            .expect("40 events over 10 accounts in one hour breaches both terms");
        assert_eq!(demotion.from(), RampMode::Live);
        assert_eq!(demotion.to(), RampMode::Shadow);
        assert_eq!(demotion.control(), ATTESTATION_QUORUM_CONTROL);
    }

    /// Spread is cardinality, and seven accounts is not eight.
    #[test]
    fn a_burst_one_account_short_of_the_spread_term_does_not_trip() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 400, 7);
        let stats = monitor.stats(T0 + 1_000);
        assert_eq!(stats.spread, SPREAD_THRESHOLD - 1);
        assert!(
            stats.rate >= stats.rate_threshold,
            "the rate term is satisfied many times over"
        );
        assert!(!stats.breaches(), "and the trigger still refuses to fire");
        assert!(monitor.verdict(RampMode::Live, T0 + 1_000).is_none());
    }

    /// One account cannot flood its way to a fleet-wide demotion.
    ///
    /// D32: "one account flooding the path cannot trip it — floods are what
    /// per-account rate limits are for".
    #[test]
    fn one_account_flooding_cannot_trip_the_breaker() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 50_000, 1);
        let stats = monitor.stats(T0 + 1_000);
        assert_eq!(stats.spread, 1);
        assert!(!stats.breaches());
        assert!(monitor.verdict(RampMode::Live, T0 + 1_000).is_none());
    }

    /// Unattributed submissions count toward the rate and never toward spread.
    #[test]
    fn unattributed_events_cannot_manufacture_account_spread() {
        let mut monitor = monitor();
        for index in 0..1_000 {
            monitor.observe(ControlEvent {
                subject: None,
                network: NetworkQuality::Clean,
                at_ms: T0 + index,
            });
        }
        let stats = monitor.stats(T0 + 2_000);
        assert_eq!(stats.rate, 1_000, "they are real traffic and are counted");
        assert_eq!(stats.spread, 0, "but they name no account");
        assert!(!stats.breaches());
    }

    /// The floor: 24 events in an hour is under 25.
    #[test]
    fn a_burst_one_event_short_of_the_floor_does_not_trip() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, RATE_FLOOR_PER_HOUR - 1, 24);
        let stats = monitor.stats(T0 + 1_000);
        assert!(
            stats.spread >= SPREAD_THRESHOLD,
            "the spread term is satisfied"
        );
        assert_eq!(stats.rate, RATE_FLOOR_PER_HOUR - 1);
        assert!(!stats.breaches());

        // And the very next event, in the same window, does trip it.
        monitor.observe(ControlEvent {
            subject: Some(AccountId(999)),
            network: NetworkQuality::Clean,
            at_ms: T0 + 500,
        });
        assert!(monitor.stats(T0 + 1_000).breaches());
    }

    /// The baseline: ten times a busy control's own median, not an absolute.
    #[test]
    fn a_burst_under_ten_times_the_median_does_not_trip() {
        let mut monitor = monitor();
        // Seven days of hourly history at 100 events/h, so the median is 100
        // and the threshold becomes 1_000 — far above the 25/h floor.
        for hour in 0..BASELINE_HOURS as u64 {
            burst(&mut monitor, T0 + hour * HOUR_MS, 100, 20);
        }
        let now = T0 + BASELINE_HOURS as u64 * HOUR_MS;
        burst(&mut monitor, now, 900, 20);
        let stats = monitor.stats(now + 1_000);
        assert_eq!(stats.median_7d, 100);
        assert_eq!(stats.rate_threshold, 100 * MEDIAN_MULTIPLE);
        assert_eq!(stats.rate, 900);
        assert!(
            !stats.breaches(),
            "900/h is a busy hour for a control whose median hour is 100, not an incident"
        );

        // 10x the median is: the same control, one order of magnitude out.
        burst(&mut monitor, now + 2_000, 200, 20);
        assert!(monitor.stats(now + 4_000).breaches());
    }

    /// The window is 60 minutes, so an old burst is not a current rate.
    #[test]
    fn events_that_left_the_window_do_not_count() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 400, 20);
        assert!(monitor.stats(T0 + 1_000).breaches());
        let later = T0 + WINDOW_MS + 10_000;
        let stats = monitor.stats(later);
        assert_eq!(stats.rate, 0);
        assert_eq!(stats.spread, 0);
        assert!(!stats.breaches());
        assert!(monitor.verdict(RampMode::Live, later).is_none());
    }

    /// A silent hour is a zero in the baseline, not an absent sample.
    #[test]
    fn silent_hours_are_zeroes_in_the_median_and_not_gaps() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 100, 20);
        // Nothing at all for ten hours, then one event, which closes them.
        monitor.observe(ControlEvent {
            subject: Some(AccountId(1)),
            network: NetworkQuality::Clean,
            at_ms: T0 + 11 * HOUR_MS,
        });
        assert_eq!(
            monitor.median_7d(),
            0,
            "one busy hour and ten silent ones has a median of zero, so the \
             25/h floor governs rather than 10x a hand-picked busy hour"
        );
    }

    // -----------------------------------------------------------------------
    // Clause (f)'s asymmetry, proven rather than asserted
    // -----------------------------------------------------------------------

    /// Automation may demote, and only from an acting control.
    ///
    /// This is the exhaustive statement of the asymmetry over `RampMode`: the
    /// match has three arms and only one of them yields a `Demotion`.
    #[test]
    fn automation_may_demote_and_may_do_nothing_else() {
        let stats = WindowStats {
            spread: 100,
            rate: 10_000,
            median_7d: 0,
            rate_threshold: RATE_FLOOR_PER_HOUR,
            impaired: 0,
            measured: 10_000,
            truncated: 0,
        };
        assert!(stats.breaches(), "a breach so large no term is in doubt");

        let demotion = Demotion::new(&scope(), RampMode::Live, stats, 1)
            .expect("an acting control is the one case automation may change");
        assert_eq!(demotion.from(), RampMode::Live);
        assert_eq!(demotion.to(), RampMode::Shadow);

        assert!(
            Demotion::new(&scope(), RampMode::Shadow, stats, 1).is_none(),
            "a suspended control has no action left to suspend"
        );
        assert!(
            Demotion::new(&scope(), RampMode::Off, stats, 1).is_none(),
            "and automation does not start an evaluation nobody asked for"
        );
    }

    /// There is no reachable value of the demotion's target but `Shadow`.
    ///
    /// The destination is not a constructor parameter, not a field, and not
    /// settable, so this test is a statement about the API's shape: every way
    /// of obtaining a `Demotion` produces the same target, whatever the window
    /// looked like.
    #[test]
    fn the_demotion_target_is_shadow_and_is_never_an_input() {
        for (spread, rate, impaired) in [(8, 25, 0), (10_000, 1_000_000, 1_000_000)] {
            let stats = WindowStats {
                spread,
                rate,
                median_7d: 0,
                rate_threshold: RATE_FLOOR_PER_HOUR,
                impaired,
                measured: rate,
                truncated: 0,
            };
            let demotion = Demotion::new(&scope(), RampMode::Live, stats, 7).expect("breaches");
            assert_eq!(demotion.to(), RampMode::Shadow);
            let row = demotion.posture();
            assert_eq!(row.mode, RampMode::Shadow, "never `Live`: no promotion");
            assert_eq!(
                row.mode,
                RampMode::Shadow,
                "and never `Off`: shadow keeps observing, and blinding the \
                 cluster is the censorship lever clause (f) names"
            );
            assert_eq!(row.source, PostureSource::AutoSuspend);
            assert_eq!(row.incident_id, Some(demotion.incident().0));
            assert!(row.admissible());
        }
    }

    /// The predicate that holds even if the monitor is compromised.
    ///
    /// This is the check a poller runs on a row it did not write. It is what
    /// makes the asymmetry a property of the *fleet* rather than of this
    /// module: a forged row, a buggy future writer and a raw FoundationDB
    /// write by hand are all refused by the same line, because the refusal
    /// reads the row's own claim rather than trusting its author.
    #[test]
    fn a_reader_refuses_an_autosuspend_row_that_promotes_or_blinds() {
        let row = |mode, source| RampPosture {
            mode,
            source,
            set_at_ms: 1,
            reason: String::new(),
            incident_id: None,
        };

        assert!(
            row(RampMode::Shadow, PostureSource::AutoSuspend).admissible(),
            "the one thing automation may say"
        );
        assert!(
            !row(RampMode::Live, PostureSource::AutoSuspend).admissible(),
            "a promotion by automation is refused at the reader, however it \
             reached the row"
        );
        assert!(
            !row(RampMode::Off, PostureSource::AutoSuspend).admissible(),
            "and so is blinding the cluster during the incident that tripped it"
        );

        // The operator's lever is unconstrained by this predicate: promotion
        // is an operator act, and authenticating that writer is D32 open
        // question 1's business, not this check's.
        for mode in [RampMode::Off, RampMode::Shadow, RampMode::Live] {
            assert!(row(mode, PostureSource::Operator).admissible());
            assert!(row(mode, PostureSource::Default).admissible());
        }
    }

    /// The transition rule, and why rank alone is not clause (f).
    ///
    /// The accepted D32 open-question-1 spike states the automation arm as a
    /// rank comparison (`rank(row.mode) < rank(current)`). That is necessary
    /// and **not sufficient**: `off` ranks below `live`, so a rank-only rule
    /// admits the one fallback clause (f) forbids in as many words. The row's
    /// own constraint and the transition's constraint are both required, and
    /// this pins the two cases that separate them.
    #[test]
    fn the_transition_rule_is_a_conjunction_and_not_a_rank_comparison() {
        let auto = |mode| RampPosture {
            mode,
            source: PostureSource::AutoSuspend,
            set_at_ms: 1,
            reason: String::new(),
            incident_id: None,
        };

        // The one permitted move.
        assert!(auto(RampMode::Shadow).admissible_from(RampMode::Live));

        // Ranks below `live`, and is still refused: shadow keeps observing,
        // and falling to `off` is the censorship lever.
        assert!(!auto(RampMode::Off).admissible_from(RampMode::Live));

        // Passes the row-local check and still refused: it raises the rank.
        assert!(!auto(RampMode::Shadow).admissible_from(RampMode::Off));

        // Idempotence: re-applying a demotion to an already-shadow control is
        // not a transition, so it does not strictly lower anything.
        assert!(!auto(RampMode::Shadow).admissible_from(RampMode::Shadow));

        // A promotion fails both halves.
        assert!(!auto(RampMode::Live).admissible_from(RampMode::Shadow));

        // The operator's lever is unconstrained in both predicates.
        for mode in [RampMode::Off, RampMode::Shadow, RampMode::Live] {
            for current in [RampMode::Off, RampMode::Shadow, RampMode::Live] {
                let operator = RampPosture {
                    mode,
                    source: PostureSource::Operator,
                    set_at_ms: 1,
                    reason: String::new(),
                    incident_id: None,
                };
                assert!(operator.admissible_from(current));
            }
        }
    }

    /// The monitor never yields a demotion without a breach, in either
    /// direction.
    #[test]
    fn no_breach_means_no_demotion_even_from_live() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 10, 10);
        assert!(!monitor.stats(T0 + 1_000).breaches());
        assert!(monitor.verdict(RampMode::Live, T0 + 1_000).is_none());
    }

    /// The reason string carries every number the predicate branched on.
    #[test]
    fn the_incident_reason_lets_an_operator_recheck_the_arithmetic() {
        let mut monitor = monitor();
        burst(&mut monitor, T0, 40, 10);
        let demotion = monitor
            .verdict(RampMode::Live, T0 + 1_000)
            .expect("breaches");
        let reason = demotion.reason_text();
        assert!(reason.contains("spread 10 >= 8"), "{reason}");
        assert!(reason.contains("rate 40/h >= 25"), "{reason}");
        assert!(reason.contains(&demotion.incident().to_hex()), "{reason}");
        assert!(
            demotion.posture().reason.len() <= 256,
            "writers cap the row's reason at 256 bytes"
        );
    }

    /// The same trip, named the same way, by any process that saw it.
    #[test]
    fn incident_ids_are_derived_and_not_drawn() {
        let global = IncidentId::of(&scope(), 1_000);
        assert_eq!(global, IncidentId::of(&scope(), 1_000));
        assert_ne!(global, IncidentId::of(&scope(), 1_001));

        let versioned = TriggerScope::per_ruleset(
            ATTESTATION_QUORUM_CONTROL,
            RulesetId {
                version: 3,
                digest: [7; 32],
            },
        );
        assert_ne!(
            global,
            IncidentId::of(&versioned, 1_000),
            "a per-rule-version trip is a different incident from a global one"
        );
        assert_eq!(global.to_hex().len(), 32);
    }

    // -----------------------------------------------------------------------
    // The RTT/loss dimension
    // -----------------------------------------------------------------------

    /// The bucket takes the worst of the three signals.
    #[test]
    fn network_quality_takes_the_worst_signal() {
        let clean = PathSample {
            rtt_ms: 20,
            loss_ppm: 0,
            relayed: false,
        };
        assert_eq!(NetworkQuality::of(clean), NetworkQuality::Clean);

        assert_eq!(
            NetworkQuality::of(PathSample {
                relayed: true,
                ..clean
            }),
            NetworkQuality::Degraded,
            "docs/07 §5 groups relay-path connections with measured loss"
        );
        assert_eq!(
            NetworkQuality::of(PathSample {
                rtt_ms: crate::intent::RTT_DEGRADED_MS,
                ..clean
            }),
            NetworkQuality::Degraded
        );
        assert_eq!(
            NetworkQuality::of(PathSample {
                rtt_ms: crate::intent::RTT_BAD_MS,
                ..clean
            }),
            NetworkQuality::Bad,
            "D8's 250 ms high-latency band, borrowed rather than reinvented"
        );
        assert_eq!(
            NetworkQuality::of(PathSample {
                loss_ppm: crate::intent::LOSS_BAD_PPM,
                ..clean
            }),
            NetworkQuality::Bad
        );
        assert_eq!(
            NetworkQuality::of(PathSample {
                rtt_ms: 10,
                loss_ppm: crate::intent::LOSS_DEGRADED_PPM,
                relayed: false,
            }),
            NetworkQuality::Degraded,
            "a fast path losing one packet in a hundred is still impaired"
        );
    }

    /// An unmeasured path is `Unknown`, and `Unknown` is not evidence of health.
    #[test]
    fn an_unmeasured_path_is_never_counted_as_a_good_one() {
        assert!(!NetworkQuality::Unknown.is_measured());
        assert!(
            !NetworkQuality::Unknown.is_impaired(),
            "it must not inflate the impaired numerator either"
        );
        assert_eq!(NetworkQuality::default(), NetworkQuality::Unknown);

        let mut monitor = monitor();
        burst_on(&mut monitor, T0, 40, 10, NetworkQuality::Unknown);
        let stats = monitor.stats(T0 + 1_000);
        assert_eq!(stats.measured, 0);
        assert_eq!(stats.impaired, 0);
        assert!(
            !stats.network_correlated(),
            "a gateway that measures nothing cannot manufacture a correlation"
        );
        assert_eq!(stats.reason(), TripReason::RateSpike);
    }

    /// R-6's shape: the same trip, labelled so the post-mortem starts at the
    /// transport rather than at the accounts.
    #[test]
    fn a_breach_concentrated_on_bad_paths_is_labelled_network_correlated() {
        let mut monitor = monitor();
        burst_on(&mut monitor, T0, 40, 10, NetworkQuality::Bad);
        let stats = monitor.stats(T0 + 1_000);
        assert_eq!(stats.impaired, 40);
        assert_eq!(stats.measured, 40);
        assert!(stats.breaches());
        assert_eq!(stats.reason(), TripReason::NetworkCorrelated);

        let demotion = monitor
            .verdict(RampMode::Live, T0 + 1_000)
            .expect("breaches");
        assert_eq!(demotion.reason(), TripReason::NetworkCorrelated);
        assert!(demotion.reason_text().contains("network_correlated"));
        assert_eq!(
            demotion.to(),
            RampMode::Shadow,
            "the label changes the incident's story, never its direction"
        );
    }

    /// The correlation labels trips; it does not create them.
    ///
    /// P4's primary tunable is the false-positive rate (D17 risk 3), so a
    /// dimension with no threshold in the record does not get to add trips of
    /// its own.
    #[test]
    fn the_network_label_never_lowers_the_bar_for_tripping() {
        let mut monitor = monitor();
        // Every event on the worst possible path, and still short on spread.
        burst_on(&mut monitor, T0, 10_000, 3, NetworkQuality::Bad);
        let stats = monitor.stats(T0 + 1_000);
        assert!(stats.network_correlated());
        assert!(
            !stats.breaches(),
            "correlation is a reason, not a term: clause (f)'s two terms still govern"
        );
        assert!(monitor.verdict(RampMode::Live, T0 + 1_000).is_none());
    }

    // -----------------------------------------------------------------------
    // End to end, against the real C1 lever
    // -----------------------------------------------------------------------

    /// The acceptance case: a synthetic burst demotes an acting control.
    ///
    /// Against [`crate::intent::AttestationPosture`] itself, not a stub, so
    /// what is proven is that the trip reaches the byte the admission path
    /// reads.
    #[test]
    fn a_qualifying_burst_demotes_the_live_control_and_stops_there() {
        use crate::intent::{AttestationEnforcement, AttestationPosture};

        let posture = AttestationPosture::new(AttestationEnforcement::Required);
        let mut monitor = monitor();

        // Below the floor: nothing moves.
        burst(&mut monitor, T0, RATE_FLOOR_PER_HOUR - 1, 24);
        assert!(monitor.trip(&posture, T0 + 1_000).is_none());
        assert_eq!(
            posture.get(),
            AttestationEnforcement::Required,
            "a control that has not breached keeps acting"
        );

        // Past it: the control stops acting.
        burst(&mut monitor, T0 + 1_000, 100, 20);
        let demotion = monitor
            .trip(&posture, T0 + 2_000)
            .expect("the burst breaches both terms against a live control");
        assert_eq!(demotion.from(), RampMode::Live);
        assert_eq!(demotion.to(), RampMode::Shadow);
        assert_eq!(
            posture.get(),
            AttestationEnforcement::Shadow,
            "clause (f): the acting control is suspended"
        );

        // And it stops there. The window still breaches, and polling again
        // must neither fall to `off` nor restamp the incident.
        assert!(monitor.stats(T0 + 2_000).breaches());
        assert!(
            monitor.trip(&posture, T0 + 2_000).is_none(),
            "a suspended control has no action left to suspend"
        );
        assert_eq!(
            posture.get(),
            AttestationEnforcement::Shadow,
            "shadow keeps observing: the incident is the calibration data"
        );
    }

    /// Automation never reverts an operator, and never starts a control.
    #[test]
    fn a_breaching_window_cannot_promote_or_wake_a_control() {
        use crate::intent::{AttestationEnforcement, AttestationPosture};

        let mut monitor = monitor();
        burst(&mut monitor, T0, 400, 20);
        assert!(monitor.stats(T0 + 1_000).breaches());

        // An operator has switched the control off. The most extreme breach
        // the window can hold does not wake it: `Off -> Shadow` is automation
        // starting an evaluation nobody asked for.
        let off = AttestationPosture::new(AttestationEnforcement::Off);
        assert!(monitor.trip(&off, T0 + 1_000).is_none());
        assert_eq!(off.get(), AttestationEnforcement::Off);

        // And an already-suspended control is left alone rather than promoted
        // back to acting or dropped to off.
        let shadow = AttestationPosture::new(AttestationEnforcement::Shadow);
        assert!(monitor.trip(&shadow, T0 + 1_000).is_none());
        assert_eq!(shadow.get(), AttestationEnforcement::Shadow);
    }

    /// Capacity overflow understates the rate, which can only fail to trip.
    #[test]
    fn window_overflow_is_reported_and_errs_toward_not_tripping() {
        let mut monitor = SuspendMonitor::with_capacity(scope(), 10);
        burst(&mut monitor, T0, 100, 20);
        let stats = monitor.stats(T0 + 1_000);
        assert_eq!(stats.truncated, 90);
        assert_eq!(stats.rate, 10, "the retained window, not the true rate");
        assert!(
            !stats.breaches(),
            "an understated rate fails to trip; it never trips wrongly"
        );
    }
}
