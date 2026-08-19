//! Stage decomposition for the lease-renewal path above the router.
//!
//! # Why this exists
//!
//! docs/08-persistence.md §2.2.3–§2.2.5 took the renewal path apart *below*
//! the [`Router`](crate::cluster::Router) boundary and left it at about 1.9 us
//! per renewal. Nothing measures what happens above that boundary. One
//! heartbeat carries a peer's whole lease set — 80 entries at the P2 operating
//! point — and before it reaches the router it takes the peer-state lock,
//! resolves every pair against the session's own table, and afterwards takes
//! that lock a second time and encodes a row per renewed lease. Those are five
//! waits nobody has ever seen a number for, which is precisely the position
//! `router_apply` and `gateway_intent_server_ms` were in before
//! [`RouteStageMetrics`](crate::cluster::RouteStageMetrics) and
//! [`IntentStageMetrics`](crate::intent::stages::IntentStageMetrics) split
//! them. This is the same split for renewals, modelled on both deliberately.
//!
//! # Denominators — read this before dividing anything
//!
//! There are **two** and they are not interchangeable:
//!
//! * [`LeaseStageSnapshot::heartbeats`] — one per `LeaseMsg::Heartbeat`
//!   served. Every stage below is summed once per heartbeat, so
//!   `<stage>_us_sum / heartbeats` is a mean **over messages**.
//! * [`LeaseStageSnapshot::renewals`] — the summed width of those messages,
//!   i.e. how many `(entity, lease_id)` pairs they carried. A per-lease cost
//!   is `<stage>_us_sum / renewals`, and it is the number to compare against
//!   the bench's per-renewal figure.
//!
//! Dividing by the wrong one is off by the batch width, which is ~80 at P2 and
//! is *not* constant — it is however many leases a peer happens to hold. The
//! intent path carries the same warning for the same reason, and this project
//! has already published one set of numbers that divided a per-flush sum by a
//! per-record count and understated every stage ~30x.
//!
//! # The gap is a stage too
//!
//! ```text
//! gap_us = heartbeat_us - (session_us + resolve_us + route_us
//!                          + recheck_us + encode_us)
//! ```
//!
//! Time inside the served span that no stage claims — scheduler delay between
//! the lease lane being woken and a worker polling it, mostly. It is emitted
//! rather than left to be subtracted, because an unattributed gap is a finding
//! and this project has been bitten by one before: a location audit whose cost
//! was excluded from every stage timer and reappeared as the next diff's gate
//! wait (docs/08 §2.1.3).
//!
//! # Why there is no slow/all split here
//!
//! [`IntentStageMetrics`](crate::intent::stages::IntentStageMetrics) keeps its
//! field set twice because `intent_commit_ms` is a **D16 series with a p99
//! budget**, and a p99 cannot be read out of a sum and a max. Renewals have no
//! such budget — no D16 series measures a heartbeat — so the question here is
//! "where does the time go", which sums and maxima answer. If a renewal tail
//! ever becomes a target, this grows the same second accumulator; it does not
//! have one yet because nothing would read it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// One served heartbeat's stage times, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeartbeatTrace {
    /// Pairs the heartbeat asked to renew, before de-duplication.
    pub entries: u64,
    /// Wait for the peer-state lock that guards the session's lease table.
    pub session_us: u64,
    /// `resolve_renewals`: de-duplicate the batch and check every pair against
    /// the session's own table. Synchronous, and runs with the lock held.
    pub resolve_us: u64,
    /// `renew_session_leases`: grouping plus the whole `Router` call. This is
    /// the part docs/08 §2.2.3–§2.2.5 measured from below.
    pub route_us: u64,
    /// The second peer-state lock acquisition, which re-checks that the
    /// session survived the router call.
    pub recheck_us: u64,
    /// Encoding the `HeartbeatAck` and handing it to the send closure.
    pub encode_us: u64,
    /// The whole arm, so the stage sum can be checked against it in one trace.
    pub heartbeat_us: u64,
}

impl HeartbeatTrace {
    /// Served-span time claimed by a named stage.
    #[must_use]
    pub fn claimed_us(&self) -> u64 {
        self.session_us + self.resolve_us + self.route_us + self.recheck_us + self.encode_us
    }

    /// Served-span time no stage claims. See the module docs.
    #[must_use]
    pub fn gap_us(&self) -> u64 {
        self.heartbeat_us.saturating_sub(self.claimed_us())
    }
}

/// Process-wide renewal stage counters.
#[derive(Debug, Default)]
pub struct LeaseStageMetrics {
    heartbeats: AtomicU64,
    renewals: AtomicU64,
    session_us_sum: AtomicU64,
    session_us_max: AtomicU64,
    resolve_us_sum: AtomicU64,
    resolve_us_max: AtomicU64,
    route_us_sum: AtomicU64,
    route_us_max: AtomicU64,
    recheck_us_sum: AtomicU64,
    recheck_us_max: AtomicU64,
    encode_us_sum: AtomicU64,
    encode_us_max: AtomicU64,
    heartbeat_us_sum: AtomicU64,
    heartbeat_us_max: AtomicU64,
    gap_us_sum: AtomicU64,
    gap_us_max: AtomicU64,
    /// The widest batch seen. A mean over `renewals` is only interpretable
    /// next to the spread of the widths it averages.
    entries_max: AtomicU64,
}

fn bump(sum: &AtomicU64, max: &AtomicU64, value: u64) {
    sum.fetch_add(value, Ordering::Relaxed);
    max.fetch_max(value, Ordering::Relaxed);
}

impl LeaseStageMetrics {
    /// Fold one served heartbeat in.
    pub fn record(&self, trace: &HeartbeatTrace) {
        self.heartbeats.fetch_add(1, Ordering::Relaxed);
        self.renewals.fetch_add(trace.entries, Ordering::Relaxed);
        self.entries_max.fetch_max(trace.entries, Ordering::Relaxed);
        for (sum, max, value) in [
            (&self.session_us_sum, &self.session_us_max, trace.session_us),
            (&self.resolve_us_sum, &self.resolve_us_max, trace.resolve_us),
            (&self.route_us_sum, &self.route_us_max, trace.route_us),
            (&self.recheck_us_sum, &self.recheck_us_max, trace.recheck_us),
            (&self.encode_us_sum, &self.encode_us_max, trace.encode_us),
            (
                &self.heartbeat_us_sum,
                &self.heartbeat_us_max,
                trace.heartbeat_us,
            ),
            (&self.gap_us_sum, &self.gap_us_max, trace.gap_us()),
        ] {
            bump(sum, max, value);
        }
    }

    /// Read every counter at once.
    #[must_use]
    pub fn snapshot(&self) -> LeaseStageSnapshot {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        LeaseStageSnapshot {
            heartbeats: get(&self.heartbeats),
            renewals: get(&self.renewals),
            entries_max: get(&self.entries_max),
            session_us_sum: get(&self.session_us_sum),
            session_us_max: get(&self.session_us_max),
            resolve_us_sum: get(&self.resolve_us_sum),
            resolve_us_max: get(&self.resolve_us_max),
            route_us_sum: get(&self.route_us_sum),
            route_us_max: get(&self.route_us_max),
            recheck_us_sum: get(&self.recheck_us_sum),
            recheck_us_max: get(&self.recheck_us_max),
            encode_us_sum: get(&self.encode_us_sum),
            encode_us_max: get(&self.encode_us_max),
            heartbeat_us_sum: get(&self.heartbeat_us_sum),
            heartbeat_us_max: get(&self.heartbeat_us_max),
            gap_us_sum: get(&self.gap_us_sum),
            gap_us_max: get(&self.gap_us_max),
        }
    }
}

/// A read of every [`LeaseStageMetrics`] counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs, reason = "each field mirrors its HeartbeatTrace stage")]
pub struct LeaseStageSnapshot {
    /// Heartbeat messages served. The denominator for a per-message mean.
    pub heartbeats: u64,
    /// Pairs those messages carried. The denominator for a per-lease mean.
    pub renewals: u64,
    /// The widest single batch, so a per-lease mean can be read in context.
    pub entries_max: u64,
    pub session_us_sum: u64,
    pub session_us_max: u64,
    pub resolve_us_sum: u64,
    pub resolve_us_max: u64,
    pub route_us_sum: u64,
    pub route_us_max: u64,
    pub recheck_us_sum: u64,
    pub recheck_us_max: u64,
    pub encode_us_sum: u64,
    pub encode_us_max: u64,
    pub heartbeat_us_sum: u64,
    pub heartbeat_us_max: u64,
    pub gap_us_sum: u64,
    pub gap_us_max: u64,
}

impl LeaseStageSnapshot {
    /// This snapshot minus `earlier`, for a reporter emitting intervals.
    ///
    /// Sums subtract; maxima do not — a max over an interval cannot be
    /// recovered from two cumulative maxima, so those are carried through as
    /// the run-to-date value and named that way by the reporter.
    #[must_use]
    pub fn delta(&self, earlier: &Self) -> Self {
        Self {
            heartbeats: self.heartbeats.saturating_sub(earlier.heartbeats),
            renewals: self.renewals.saturating_sub(earlier.renewals),
            entries_max: self.entries_max,
            session_us_sum: self.session_us_sum.saturating_sub(earlier.session_us_sum),
            session_us_max: self.session_us_max,
            resolve_us_sum: self.resolve_us_sum.saturating_sub(earlier.resolve_us_sum),
            resolve_us_max: self.resolve_us_max,
            route_us_sum: self.route_us_sum.saturating_sub(earlier.route_us_sum),
            route_us_max: self.route_us_max,
            recheck_us_sum: self.recheck_us_sum.saturating_sub(earlier.recheck_us_sum),
            recheck_us_max: self.recheck_us_max,
            encode_us_sum: self.encode_us_sum.saturating_sub(earlier.encode_us_sum),
            encode_us_max: self.encode_us_max,
            heartbeat_us_sum: self
                .heartbeat_us_sum
                .saturating_sub(earlier.heartbeat_us_sum),
            heartbeat_us_max: self.heartbeat_us_max,
            gap_us_sum: self.gap_us_sum.saturating_sub(earlier.gap_us_sum),
            gap_us_max: self.gap_us_max,
        }
    }
}

static LEASE_STAGE: std::sync::LazyLock<std::sync::Arc<LeaseStageMetrics>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(LeaseStageMetrics::default()));

/// The process-wide renewal stage decomposition.
#[must_use]
pub fn lease_stage_metrics() -> std::sync::Arc<LeaseStageMetrics> {
    std::sync::Arc::clone(&LEASE_STAGE)
}

/// Microseconds since `since`, saturating.
#[must_use]
pub fn elapsed_us(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gap_is_the_span_minus_its_stages() {
        let trace = HeartbeatTrace {
            entries: 80,
            session_us: 10,
            resolve_us: 20,
            route_us: 300,
            recheck_us: 5,
            encode_us: 40,
            heartbeat_us: 500,
        };
        assert_eq!(trace.claimed_us(), 375);
        assert_eq!(trace.gap_us(), 125);
    }

    /// A span shorter than its stages is a clock artifact, not a negative gap.
    #[test]
    fn a_gap_never_goes_negative() {
        let trace = HeartbeatTrace {
            heartbeat_us: 10,
            route_us: 40,
            ..HeartbeatTrace::default()
        };
        assert_eq!(trace.gap_us(), 0);
    }

    #[test]
    fn both_denominators_are_counted_and_are_not_the_same() {
        let metrics = LeaseStageMetrics::default();
        metrics.record(&HeartbeatTrace {
            entries: 80,
            route_us: 100,
            heartbeat_us: 100,
            ..HeartbeatTrace::default()
        });
        metrics.record(&HeartbeatTrace {
            entries: 4,
            route_us: 10,
            heartbeat_us: 10,
            ..HeartbeatTrace::default()
        });
        let s = metrics.snapshot();
        assert_eq!(s.heartbeats, 2, "one per message");
        assert_eq!(s.renewals, 84, "summed batch width, not message count");
        assert_eq!(s.entries_max, 80);
        assert_eq!(s.route_us_sum, 110);
        assert_eq!(s.route_us_max, 100);
        // The two denominators differ by the batch width, which is the whole
        // reason the module docs warn about them.
        assert_ne!(s.route_us_sum / s.heartbeats, s.route_us_sum / s.renewals);
    }

    #[test]
    fn a_delta_subtracts_sums_and_carries_maxima() {
        let metrics = LeaseStageMetrics::default();
        metrics.record(&HeartbeatTrace {
            entries: 10,
            route_us: 90,
            heartbeat_us: 90,
            ..HeartbeatTrace::default()
        });
        let first = metrics.snapshot();
        metrics.record(&HeartbeatTrace {
            entries: 3,
            route_us: 5,
            heartbeat_us: 5,
            ..HeartbeatTrace::default()
        });
        let d = metrics.snapshot().delta(&first);
        assert_eq!(d.heartbeats, 1);
        assert_eq!(d.renewals, 3);
        assert_eq!(d.route_us_sum, 5, "sums subtract");
        assert_eq!(
            d.route_us_max, 90,
            "an interval max is not recoverable from two cumulative maxima, so \
             the run-to-date value is carried through",
        );
    }
}
