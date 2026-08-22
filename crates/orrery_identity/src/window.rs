//! The binding-rate window: D36's `dw` row, as one shared prune/check/append.
//!
//! D31 clause (g) caps binding events at 8 per account per rolling 24 h and
//! 64 per rolling 30 d; D36 makes that answerable by giving each account a
//! window row — the ascending vector of `at_ms` stamps of every event it
//! filed within its trailing 30 days — written inside the transaction that
//! already stages `da`, `db` and `dh`. Both stores enforce from *this* logic,
//! because a harness store that enforces less than the durable one lies to
//! every gate that uses it (D36 clause (d)); [`crate::mem`] keeps the vector
//! in a fourth map under its lock and [`crate::fdb`] reads and writes the
//! `dw ‖ account` row non-snapshot in the bind/unbind transaction.
//!
//! # Exactness, not buckets
//!
//! Rolling windows computed from stored stamps are exact; hourly or daily
//! buckets would admit boundary-straddled bursts of up to twice the intended
//! size (D36 §Alternatives). At ≤ 64 × 8 B there is no size argument for
//! approximation, so there is none here.
//!
//! # The boundary is inclusive by one reading and pinned by a test
//!
//! An entry counts toward a window of width `W` evaluated at `at_ms` iff
//! `t >= at_ms − W`: D36 says the row "prunes entries older than now − 30 d",
//! which drops strictly older stamps and keeps one exactly at the cutoff. An
//! event exactly 24 h old still refuses the 9th; one millisecond later it no
//! longer does. [`window_boundary_is_inclusive_then_slides_off`] pins this so
//! the choice is recorded rather than accidental.

use crate::store::IdentityError;
use orrery_protocol::AccountId;

/// Binding events allowed per account per rolling 24 h (D31 (g), D36 (f)).
///
/// Accepted clause-(g) policy; D36 does not reopen it and adds no tunable —
/// the log's horizon *is* the longer window, and everything else derives.
pub const BINDING_RATE_CAP_24H: usize = 8;

/// Binding events allowed per account per rolling 30 days (D31 (g), D36 (f)).
pub const BINDING_RATE_CAP_30D: usize = 64;

/// Width of the short rate window, in milliseconds.
pub const BINDING_RATE_WINDOW_24H_MS: u64 = 24 * 60 * 60 * 1000;

/// Width of the long rate window, in milliseconds — also the vector's whole
/// retention horizon: anything older is pruned at write time, which is what
/// bounds the row at [`BINDING_RATE_CAP_30D`] entries without a sweep.
pub const BINDING_RATE_WINDOW_30D_MS: u64 = 30 * BINDING_RATE_WINDOW_24H_MS;

/// Which cap refused an event, and the width of the window it belongs to.
///
/// Consumed into [`IdentityError::BindingRateLimited`]; split out as its own
/// type because the pure check below must not know the error taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRefusal {
    /// The width of the window that tripped, in milliseconds. When both
    /// windows would refuse, this names the 24 h one — checked first (D36
    /// (b), property 4).
    pub window_ms: u64,
    /// That window's cap.
    pub cap: usize,
}

/// Prune, check, append — D36 clause (b)'s window semantics in one function.
///
/// `stamps` is the account's stored vector (ascending event times in ms;
/// sorted again here so the invariant does not depend on the caller). The
/// returned vector is what the caller writes back: over-window entries
/// dropped, `at_ms` appended, still ascending. `Err` names the tripped window
/// and means **nothing was consumed**: both stores compute this before staging
/// any write, so a refused bind or unbind leaves the vector — and everything
/// else — untouched.
///
/// Both directions count: a *binding event* is every staged `dh` row, bind or
/// unbind alike (D36 (b), property 2). Callers reach this function only on
/// paths that will append a `dh` row — a re-bind of a pair that already holds
/// is `BindOutcome::AlreadyBound`, appends nothing, and never gets here.
///
/// The 24 h cap is evaluated before the 30 d one, so when both would trip the
/// refusal names the shorter window.
///
/// Clock note, so nobody over-trusts it: window arithmetic runs on the
/// `at_ms` the caller supplies, the same trust level D31's resolved question 2
/// accepted for `first_event_ms`. Skew between replicas is bounded by fleet
/// NTP discipline and measured in milliseconds against hour-long and
/// month-long windows (D36 (b)).
pub fn admit_binding_event(stamps: &[u64], at_ms: u64) -> Result<Vec<u64>, RateRefusal> {
    let mut live: Vec<u64> = stamps
        .iter()
        .copied()
        .filter(|t| *t >= at_ms.saturating_sub(BINDING_RATE_WINDOW_30D_MS))
        .collect();
    live.sort_unstable();

    let short_cutoff = at_ms.saturating_sub(BINDING_RATE_WINDOW_24H_MS);
    let recent = live.partition_point(|t| *t < short_cutoff);
    if live.len() - recent >= BINDING_RATE_CAP_24H {
        return Err(RateRefusal {
            window_ms: BINDING_RATE_WINDOW_24H_MS,
            cap: BINDING_RATE_CAP_24H,
        });
    }
    if live.len() >= BINDING_RATE_CAP_30D {
        return Err(RateRefusal {
            window_ms: BINDING_RATE_WINDOW_30D_MS,
            cap: BINDING_RATE_CAP_30D,
        });
    }

    // Insert at position, not append: `at_ms` comes from the caller's clock,
    // and a call that lands one millisecond behind the newest stored stamp
    // must come back ascending all the same. The vector's ordering is part
    // of its documented shape (D36 §Decision (a)), so the function defends
    // it against the caller rather than trusting the wall clock to move.
    let position = live.partition_point(|t| *t <= at_ms);
    live.insert(position, at_ms);
    Ok(live)
}

/// Map a [`RateRefusal`] onto the named error variant.
///
/// Shared by both stores so the refusal taxonomy has exactly one spelling.
pub(crate) fn rate_limited(account: AccountId, refusal: RateRefusal) -> IdentityError {
    IdentityError::BindingRateLimited {
        account,
        window_ms: refusal.window_ms,
        cap: refusal.cap,
    }
}

#[cfg(test)]
mod tests {
    //! Window semantics against the pure function; enforcement wiring is
    //! proven where it lives — [`crate::mem::tests`] through the in-memory
    //! store, `crate::fdb::tests` against a real transaction.

    use super::*;

    /// A day, and a bit more than a day, in ms — the spacing the scenarios
    /// below reason in.
    const DAY: u64 = BINDING_RATE_WINDOW_24H_MS;

    #[test]
    fn the_ninth_event_inside_24h_is_refused() {
        // The issue's criterion verbatim: eight events land, all inside one
        // trailing 24 h span; the ninth — still inside it — is refused with
        // the short window named.
        let t0 = 1_000 * DAY;
        let mut stamps: Vec<u64> = Vec::new();
        for k in 0..8u64 {
            stamps = admit_binding_event(&stamps, t0 + k).expect("each of the first eight admits");
        }
        assert_eq!(stamps.len(), BINDING_RATE_CAP_24H);

        let error = admit_binding_event(&stamps, t0 + 8).expect_err("the ninth refuses");
        assert_eq!(
            error,
            RateRefusal {
                window_ms: BINDING_RATE_WINDOW_24H_MS,
                cap: BINDING_RATE_CAP_24H,
            },
            "the refusal names the 24 h window and its cap"
        );
        assert_eq!(stamps.len(), BINDING_RATE_CAP_24H, "nothing was consumed");
    }

    #[test]
    fn the_65th_event_inside_30d_is_refused_while_each_days_8_stay_admitted() {
        // Sixty-four admitted events, eight per slot across eight slots two
        // days apart — every octet under the 24 h cap (and each one fully
        // aged out of the trailing-24 h count before the next begins), all 64
        // still inside the 30-day horizon. The 65th attempt comes when the
        // trailing-24 h count is zero, so only the long window can refuse.
        let t0 = 1_000 * DAY;
        let mut stamps = Vec::new();
        for day in 0..8u64 {
            let base = t0 + 2 * day * DAY;
            for k in 0..8u64 {
                stamps =
                    admit_binding_event(&stamps, base + k).expect("each day's 8 stay admitted");
            }
        }
        assert_eq!(stamps.len(), BINDING_RATE_CAP_30D);

        let error =
            admit_binding_event(&stamps, t0 + 16 * DAY).expect_err("the 65th in-window refuses");
        assert_eq!(
            error,
            RateRefusal {
                window_ms: BINDING_RATE_WINDOW_30D_MS,
                cap: BINDING_RATE_CAP_30D,
            },
            "a full 30-day window refuses even with the 24 h cap satisfied"
        );

        // And the refused call consumed nothing: the vector is unchanged.
        assert_eq!(stamps.len(), BINDING_RATE_CAP_30D);
    }

    #[test]
    fn stamps_older_than_a_window_stop_counting() {
        // Eight events on day 0 saturate the short window; past its slide the
        // same schedule admits freely again. Past the long horizon the vector
        // itself empties — pruning, not just discounting.
        let t0 = 1_000 * DAY;
        let stamps: Vec<u64> = (0..8).map(|k| t0 + k).collect();
        let refused = admit_binding_event(&stamps, t0 + DAY - 1);
        assert_eq!(
            refused.unwrap_err(),
            RateRefusal {
                window_ms: BINDING_RATE_WINDOW_24H_MS,
                cap: BINDING_RATE_CAP_24H,
            },
            "the 9th inside 24 h"
        );

        // One ms past the short window's edge, the same ninth is admitted:
        // seven of the eight stamps still sit inside the trailing-24 h count
        // (under its cap), and all eight survive the 30-day prune — so the
        // vector comes back with nine.
        let admitted = admit_binding_event(&stamps, t0 + DAY + 1).expect("the window slid");
        assert_eq!(admitted.len(), 9);

        // Thirty-two days on, nothing is inside either window any more: the
        // slide stamp itself has crossed the long horizon.
        let much_later = t0 + 32 * DAY;
        let fresh = admit_binding_event(&admitted, much_later).expect("everything aged out");
        assert_eq!(fresh, vec![much_later], "the pruned vector holds only now");
    }

    #[test]
    fn window_boundary_is_inclusive_then_slides_off() {
        // Pins the boundary reading: an entry exactly `W` old still counts
        // ("prunes entries older than now − W" keeps the cutoff itself); one
        // ms later it does not. Recorded rather than accidental — see the
        // module doc.
        let t0 = 1_000 * DAY;
        let stamps = [t0; 8];

        let at_edge = admit_binding_event(&stamps, t0 + DAY);
        assert_eq!(
            at_edge.unwrap_err(),
            RateRefusal {
                window_ms: BINDING_RATE_WINDOW_24H_MS,
                cap: BINDING_RATE_CAP_24H,
            },
            "an entry exactly one window old still counts against it"
        );

        // One ms past the edge none of them *counts* any more — but the
        // stored vector prunes on the 30-day horizon, not on this one, so
        // they all ride along under the new stamp.
        let slid = admit_binding_event(&stamps, t0 + DAY + 1)
            .expect("one ms past the edge nothing counts");
        assert_eq!(slid.len(), 9);
        assert_eq!(*slid.last().expect("non-empty"), t0 + DAY + 1);

        // Past the long horizon they leave the stored vector entirely.
        let gone = admit_binding_event(&stamps, t0 + 31 * DAY).expect("past both windows");
        assert_eq!(gone, vec![t0 + 31 * DAY]);
    }

    #[test]
    fn the_vector_never_exceeds_the_long_cap_and_stays_ascending() {
        // Drive the function across many out-of-order calls; the invariant
        // D36 (c) prices the row by — ≤ 64 entries, ascending — must hold
        // whatever order the stamps arrive in.
        let t0 = 1_000 * DAY;
        let mut stamps: Vec<u64> = Vec::new();
        let mut step: u64 = 0;
        for i in 0..200 {
            step = step
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(i | 1);
            let at = t0 + (step % (45 * DAY));
            stamps = admit_binding_event(&stamps, at).unwrap_or(stamps);
            assert!(stamps.len() <= BINDING_RATE_CAP_30D);
            assert!(stamps.windows(2).all(|pair| pair[0] <= pair[1]));
        }
    }
}
