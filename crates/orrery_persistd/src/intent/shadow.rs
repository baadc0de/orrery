//! C1's shadow arm: what the K-of-N quorum *would* have done, recorded and
//! not acted on ([D32](../../../../docs/adr/0032-enforcement-ramp.md)).
//!
//! # The three obligations, and where each one is discharged
//!
//! D32 clause (b) defines shadow as a mode with three obligations rather than
//! as "off with a log line", and each is a separate piece of machinery:
//!
//! 1. **The predicate is evaluated in full**, including every sub-predicate
//!    live mode would evaluate — D30's standing conjunct among them. That is
//!    [`super::BaselineIntentValidator::check_at`] running the same quorum
//!    check under [`super::AttestationEnforcement::Shadow`] that it runs under
//!    `Required`.
//! 2. **The action live mode would take is recorded**, with identifiers fine
//!    enough to compute D32 clause (e)'s promotion evidence without re-running
//!    traffic. That is [`ShadowObservation`], handed to a [`ShadowObserver`]
//!    and emitted as a `tracing` event.
//! 3. **None of the control's actions is taken.** The refusal is the action,
//!    and it is suppressed: admission returns the same
//!    [`super::Admission::Attested`] the `Off` arm returns, and the executor
//!    skips the commit-time re-proof (D32 clause (d), [`super::fdb`]).
//!
//! On an internal error during evaluation the mode degrades to *record
//! unevaluated*, never to an action — [`ShadowVerdict::Unevaluated`].
//!
//! # Why the vocabulary is borrowed rather than invented
//!
//! D32 clause (b): a would-be refusal carries the exact
//! [`super::RejectionCause`] and its stable label
//! ([`super::RejectionCause::as_str`]) that `Required` would have returned, so
//! a shadow report joins against the rejection logs without a translation
//! table. Nothing here reaches the wire — observations are telemetry only,
//! matching the doctrine that causes are logged rather than sent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use orrery_protocol::{AccountId, CellEpoch, NodeId};

use super::RejectionCause;

/// The name D32 clause (c) gives control C1: the `control` field of every
/// observation this module emits, and the suffix of the `ramp/{control}`
/// posture row an operator writes to demote it.
pub const ATTESTATION_QUORUM_CONTROL: &str = "attestation_quorum";

/// The `tracing` target every shadow observation is emitted on.
///
/// A stable target rather than a stable message, so an out-of-process reader
/// — [#222]'s gate leg, or [#221]'s collector — filters on one string instead
/// of matching prose that a later edit can silently change.
///
/// [#221]: https://github.com/baadc0de/orrery/issues/221
/// [#222]: https://github.com/baadc0de/orrery/issues/222
pub const SHADOW_TARGET: &str = "orrery::ramp::shadow";

/// Why a shadow evaluation produced no verdict at all.
///
/// D32 clause (b)'s degraded arm. Both variants are misconfigurations rather
/// than properties of the intent, which is exactly why they must not be
/// recorded as would-be refusals: counting them into `fp_count` would let a
/// gateway that cannot evaluate anything look like a gateway that refuses
/// everything, and clause (e)'s promotion predicate reads that number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowUnevaluated {
    /// No witness-epoch cache is configured, so no announcement resolves.
    ///
    /// Under `Required` this is [`RejectionCause::UnknownEpoch`] and fails
    /// closed. Shadow has no closed direction to fail in — it never acts — so
    /// it records the absence instead of borrowing a cause that describes the
    /// *intent*.
    NoEpochAuthority,
    /// No interest authority is configured, so D30's standing conjunct cannot
    /// be answered for anybody.
    NoInterestAuthority,
}

impl ShadowUnevaluated {
    /// A short stable label for logs, in [`RejectionCause::as_str`]'s style.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoEpochAuthority => "no_epoch_authority",
            Self::NoInterestAuthority => "no_interest_authority",
        }
    }
}

/// What `Required` would have done with an intent this gateway admitted
/// anyway.
///
/// Three admitting outcomes and one refusing one, because live mode has
/// exactly that many: D29 clause 2's admission function is total, and a
/// shadow report that collapsed "would have committed provisionally" into
/// either "would have admitted" or "would have refused" would mis-state the
/// population D29's path serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowVerdict {
    /// D27 clause (d)'s predicate held: `Required` would have committed this
    /// intent with the cluster standing behind it.
    WouldAdmit,
    /// `|E(I)|` fell below `WITNESS_SET_FLOOR_N` and D29 clause 3's
    /// `reversible(i)` held: `Required` would have committed this intent
    /// provisionally, quarantined until spot replay.
    WouldCommitProvisionally,
    /// `Required` would have refused, with this cause.
    WouldRefuse(RejectionCause),
    /// The predicate could not be evaluated. Never an action.
    Unevaluated(ShadowUnevaluated),
}

impl ShadowVerdict {
    /// Whether live mode would have *acted* on this intent — that is, refused
    /// it.
    ///
    /// This is D32 clause (e)'s `o.would_act`, and it is the numerator of
    /// `fp_count(H, C, W)`. Provisional commit is deliberately not an action:
    /// it commits, and the quarantine it applies is D29's machinery rather
    /// than C1's refusal.
    #[must_use]
    pub const fn would_act(self) -> bool {
        matches!(self, Self::WouldRefuse(_))
    }

    /// The would-be action's stable label.
    ///
    /// A would-be refusal answers [`RejectionCause::as_str`] verbatim, which
    /// is what lets a shadow report join against the rejection logs with no
    /// translation table (D32 clause (b)). The other three answer names that
    /// cannot collide with a cause label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WouldAdmit => "would_admit",
            Self::WouldCommitProvisionally => "would_commit_provisionally",
            Self::WouldRefuse(cause) => cause.as_str(),
            Self::Unevaluated(reason) => reason.as_str(),
        }
    }

    /// The cause a would-be refusal carries, and `None` for every other
    /// verdict.
    #[must_use]
    pub const fn cause(self) -> Option<RejectionCause> {
        match self {
            Self::WouldRefuse(cause) => Some(cause),
            _ => None,
        }
    }
}

/// One shadow-mode observation: the dimensions D32 clause (d) requires of
/// every recorded would-be action.
///
/// "Recorded" is defined there as *would-be action, `RejectionCause` label
/// where one exists, subject account, cell-epoch handle where applicable,
/// timestamp* — and this struct is that list, plus the intent and issuer ids
/// that make an observation joinable against the intent's own row without
/// re-running traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowObservation {
    /// The intent the verdict is about, so the observation joins to
    /// `intent/{intent_id}` and — under shadow — to the `attest/{intent_id}`
    /// row the commit wrote with `enforced: false`.
    pub intent_id: u128,
    /// The submitting NodeId.
    pub issuer: NodeId,
    /// The subject account: the submitting session's own, when it has one.
    ///
    /// `None` is an unauthenticated submission, which is a real population
    /// rather than a gap — D32 clause (e)'s known-honest cohort `H` is a set
    /// of accounts, so an observation with no account is outside `H` by
    /// construction and must not be silently attributed to one.
    pub subject: Option<AccountId>,
    /// The cell-epoch handle the intent named.
    pub cell_epoch: CellEpoch,
    /// What `Required` would have done.
    pub verdict: ShadowVerdict,
    /// The clock `check_at` was called with, in the same units.
    pub observed_at_ms: u64,
}

/// Where a shadow observation goes.
///
/// A seam rather than a concrete sink because the two consumers want opposite
/// things: [#221]'s collector wants counters dimensioned by cause, account
/// cardinality and network-quality bucket, and a test or a gate leg wants the
/// individual observations back. Neither belongs in the admission path, which
/// is why this is one `record` call and no aggregation.
///
/// Implementations must not block: this runs inside
/// [`super::BaselineIntentValidator::check_at`], on the admission path D16
/// budgets at a 10 ms commit p99.
///
/// [#221]: https://github.com/baadc0de/orrery/issues/221
pub trait ShadowObserver: Send + Sync {
    /// Record one observation. Called once per intent evaluated in shadow.
    fn record(&self, observation: ShadowObservation);
}

/// A shared [`ShadowObserver`], the shape a validator holds.
pub type SharedShadowObserver = Arc<dyn ShadowObserver>;

/// The observer a validator built without one uses: counts, keeps nothing.
///
/// Not a no-op, and the difference matters to D32 clause (e)'s coverage
/// denominator: a deployment that discards the individual observations still
/// knows how many intents its shadow evaluated and how many of them would have
/// been refused, which is the pair a rate needs. The `tracing` events carry
/// the rest.
#[derive(Debug, Default)]
pub struct CountingShadowObserver {
    evaluated: AtomicU64,
    would_act: AtomicU64,
}

impl CountingShadowObserver {
    /// A fresh counter pair.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many intents this observer has seen a verdict for.
    #[must_use]
    pub fn evaluated(&self) -> u64 {
        self.evaluated.load(Ordering::Relaxed)
    }

    /// How many of them live mode would have refused.
    #[must_use]
    pub fn would_act(&self) -> u64 {
        self.would_act.load(Ordering::Relaxed)
    }
}

impl ShadowObserver for CountingShadowObserver {
    fn record(&self, observation: ShadowObservation) {
        self.evaluated.fetch_add(1, Ordering::Relaxed);
        if observation.verdict.would_act() {
            self.would_act.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A bounded in-memory log of the observations themselves, for tests and for
/// an in-process gate leg.
///
/// Bounded because it is reachable from the admission path and an unbounded
/// one is a memory leak with a shadow period's worth of traffic behind it. The
/// counters are not bounded: [`Self::evaluated`] keeps counting after the ring
/// stops keeping, so a reader can always tell a quiet log from a truncated
/// one.
#[derive(Debug)]
pub struct ShadowObservationLog {
    capacity: usize,
    counts: CountingShadowObserver,
    observations: std::sync::Mutex<Vec<ShadowObservation>>,
}

impl Default for ShadowObservationLog {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}

impl ShadowObservationLog {
    /// A log keeping at most `capacity` of the most recent observations.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            counts: CountingShadowObserver::new(),
            observations: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Every observation still held, oldest first.
    #[must_use]
    pub fn observations(&self) -> Vec<ShadowObservation> {
        self.observations
            .lock()
            .expect("shadow observation log poisoned")
            .clone()
    }

    /// The most recent observation, if any.
    #[must_use]
    pub fn last(&self) -> Option<ShadowObservation> {
        self.observations
            .lock()
            .expect("shadow observation log poisoned")
            .last()
            .copied()
    }

    /// How many intents were evaluated in shadow, including any the ring has
    /// since dropped.
    #[must_use]
    pub fn evaluated(&self) -> u64 {
        self.counts.evaluated()
    }

    /// How many of them live mode would have refused.
    #[must_use]
    pub fn would_act(&self) -> u64 {
        self.counts.would_act()
    }
}

impl ShadowObserver for ShadowObservationLog {
    fn record(&self, observation: ShadowObservation) {
        self.counts.record(observation);
        let mut held = self
            .observations
            .lock()
            .expect("shadow observation log poisoned");
        if held.len() == self.capacity {
            held.remove(0);
        }
        held.push(observation);
    }
}

/// Emit one observation: the `tracing` event first, then the observer.
///
/// # Why both, and why the levels differ
///
/// The event is the out-of-process surface — a process log is what a gate leg
/// or an operator has, and it needs no wiring at all. The observer is the
/// in-process one, where [#221]'s aggregation attaches. Obligation (2) is
/// discharged by whichever a deployment actually reads, so both carry the full
/// observation.
///
/// A would-be refusal is `info` and everything else is `debug`, and the split
/// is about volume rather than importance: shadow evaluates *every* intent, so
/// an unconditional `info` line per intent would put the admission path's
/// whole throughput into the default log. The observer sees both at the same
/// level regardless, which is where a rate's denominator must come from — a
/// denominator assembled by counting the log lines a level filter chose to
/// keep is not a denominator.
///
/// [#221]: https://github.com/baadc0de/orrery/issues/221
pub(super) fn emit(observer: Option<&dyn ShadowObserver>, observation: ShadowObservation) {
    if observation.verdict.would_act() {
        tracing::info!(
            target: SHADOW_TARGET,
            control = ATTESTATION_QUORUM_CONTROL,
            intent_id = %observation.intent_id,
            issuer = %observation.issuer,
            account = observation.subject.map(|account| account.0),
            cell_epoch = observation.cell_epoch.0,
            would_act = true,
            verdict = observation.verdict.as_str(),
            observed_at_ms = observation.observed_at_ms,
            "attestation quorum would have refused this intent; admitted in shadow"
        );
    } else {
        tracing::debug!(
            target: SHADOW_TARGET,
            control = ATTESTATION_QUORUM_CONTROL,
            intent_id = %observation.intent_id,
            issuer = %observation.issuer,
            account = observation.subject.map(|account| account.0),
            cell_epoch = observation.cell_epoch.0,
            would_act = false,
            verdict = observation.verdict.as_str(),
            observed_at_ms = observation.observed_at_ms,
            "attestation quorum observed in shadow"
        );
    }
    if let Some(observer) = observer {
        observer.record(observation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(verdict: ShadowVerdict) -> ShadowObservation {
        ShadowObservation {
            intent_id: 1,
            issuer: iroh_base::SecretKey::from_bytes(&[3; 32]).public(),
            subject: Some(AccountId::new(7)),
            cell_epoch: CellEpoch::new(9),
            verdict,
            observed_at_ms: 1_000,
        }
    }

    /// A would-be refusal answers the cause's own label, so a shadow report
    /// joins the rejection logs without a translation table (D32 clause (b)).
    #[test]
    fn a_would_be_refusal_reuses_the_rejection_cause_label() {
        assert_eq!(
            ShadowVerdict::WouldRefuse(RejectionCause::RequiredWitnessMissing).as_str(),
            RejectionCause::RequiredWitnessMissing.as_str()
        );
        assert_eq!(
            ShadowVerdict::WouldRefuse(RejectionCause::RequiredWitnessMissing).cause(),
            Some(RejectionCause::RequiredWitnessMissing)
        );
    }

    /// `would_act` is clause (e)'s `o.would_act`: only a refusal counts, and a
    /// degraded evaluation counts as nothing at all.
    #[test]
    fn only_a_would_be_refusal_counts_as_a_would_have_acted_event() {
        assert!(ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet).would_act());
        assert!(!ShadowVerdict::WouldAdmit.would_act());
        assert!(!ShadowVerdict::WouldCommitProvisionally.would_act());
        assert!(
            !ShadowVerdict::Unevaluated(ShadowUnevaluated::NoEpochAuthority).would_act(),
            "D32 clause (b): an internal error degrades to `record unevaluated`, \
             never to an action — and never to a false positive in clause (e)'s count"
        );
    }

    /// The ring drops the oldest observation and the counters do not, so a
    /// truncated log still reports its own denominator.
    #[test]
    fn the_bounded_log_keeps_counting_after_it_stops_keeping() {
        let log = ShadowObservationLog::with_capacity(2);
        log.record(observation(ShadowVerdict::WouldAdmit));
        log.record(observation(ShadowVerdict::WouldRefuse(
            RejectionCause::ThresholdNotMet,
        )));
        log.record(observation(ShadowVerdict::WouldCommitProvisionally));

        assert_eq!(log.evaluated(), 3);
        assert_eq!(log.would_act(), 1);
        let held = log.observations();
        assert_eq!(held.len(), 2, "the ring is bounded at its capacity");
        assert_eq!(held[0].verdict.as_str(), "threshold_not_met");
        assert_eq!(
            log.last().expect("a recorded observation").verdict,
            ShadowVerdict::WouldCommitProvisionally
        );
    }
}
