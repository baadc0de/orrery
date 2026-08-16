//! The lightyear ↔ universe tick bridge (docs/05-prediction-rollback.md §6, D8).
//!
//! lightyear counts ticks in a `u32` that starts wherever a session starts.
//! Orrery counts them in a `u64` anchored to a coordinator-issued universe
//! epoch, shared by every island, and that number is what signed input logs,
//! RNG seeds, witness epochs and journal records all reference. The two must
//! never be confused, and neither can simply become the other: widening
//! lightyear's tick would be a fork, and narrowing Orrery's would make island
//! merges re-base history — the exact thing D8 forbids.
//!
//! So the boundary gets an offset map, and it lives here because this is the
//! only crate allowed to see both sides. Outside `orrery_predict` there is one
//! kind of tick: [`orrery_protocol::Tick`].
//!
//! **The base is still the universe origin, and that is a real cost.**
//! [`OrreryPredictPlugin`](crate::plugin::OrreryPredictPlugin) anchors the
//! bridge at `(Tick(0), 0)` and calls [`TickBridge::advance`] every fixed tick,
//! so the wrap epoch is maintained — but nothing re-anchors it. `orrery_net`
//! does not: the coordinator's universe epoch and the converged clock offset
//! reach no caller of [`TickBridge::anchor`] anywhere in the tree. Until they
//! do, `base` is zero and [`TickBridge::resolve`] returns lightyear's own
//! session-relative tick unchanged. What that costs is bounded and specific:
//! the ticks [`ReconciliationMonitor`](crate::ReconciliationMonitor) stamps
//! onto reconciliation residuals are session-relative, so a residual run is
//! measured correctly *within* a session — the monitor only ever compares
//! residuals to each other — while the tick it reports is not the universe
//! tick a witness report or a journal record would name. Re-anchoring is the
//! sync phase's job (docs/05 §6) and is not implemented; anchoring earlier
//! than convergence would bake the offset error into every tick the session
//! ever stamps, which is why the resource is not simply seeded from the
//! coordinator's epoch on arrival.
//!
//! Wraparound is handled rather than assumed away. A `u32` at 60 Hz wraps after
//! about 828 days, which is longer than any session and shorter than a
//! universe; a bridge that ignored it would be a bug with a two-year fuse.
//! [`TickBridge::advance`] carries the epoch forward on the wrap, and
//! [`TickBridge::resolve`] reads *backwards* across it by serial-number
//! comparison — which is what rollback does, every time it looks at history
//! that may sit on the far side of the boundary.

use bevy_ecs::prelude::*;
use orrery_protocol::Tick;

/// Half the `u32` space: the serial-number comparison horizon.
///
/// A lightyear tick more than this far from the last observed one is read as
/// being on the other side of a wrap rather than as a jump of two years.
const HORIZON: u32 = 1 << 31;

/// Maps lightyear's session-relative `u32` tick onto the universe-global
/// [`Tick`] (docs/05 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource)]
pub struct TickBridge {
    /// The universe tick that lightyear tick `0` of the current wrap epoch
    /// corresponds to.
    base: Tick,
    /// The most recent lightyear tick handed to [`Self::advance`], the origin
    /// for serial-number comparison.
    last: u32,
}

impl TickBridge {
    /// Anchor the bridge: at this instant, lightyear says `ly_now` and the
    /// universe says `universe_now`.
    ///
    /// Called once per session, from the coordinator-issued epoch plus the
    /// converged clock offset (docs/05 §6's sync phase). Anchoring before the
    /// offset converges would bake the error into every tick the session ever
    /// stamps.
    #[must_use]
    pub fn anchor(universe_now: Tick, ly_now: u32) -> Self {
        Self {
            base: Tick(universe_now.0.wrapping_sub(u64::from(ly_now))),
            last: ly_now,
        }
    }

    /// The universe tick that lightyear tick `0` currently maps to.
    #[must_use]
    pub const fn base(&self) -> Tick {
        self.base
    }

    /// The last lightyear tick the bridge was advanced to.
    #[must_use]
    pub const fn last_seen(&self) -> u32 {
        self.last
    }

    /// Advance to the session's current lightyear tick, returning its universe
    /// tick.
    ///
    /// Call once per fixed tick. Crossing the `u32` wrap here — and only here —
    /// is what keeps [`Self::resolve`] pure: rollback queries look backwards
    /// many times per frame and must not be able to move the epoch.
    pub fn advance(&mut self, ly_now: u32) -> Tick {
        if ly_now < self.last && self.last.wrapping_sub(ly_now) > HORIZON {
            // Forward across the wrap: 0xFFFF_FFFE → 0x0000_0001.
            self.base = Tick(self.base.0.wrapping_add(u64::from(u32::MAX) + 1));
        }
        self.last = ly_now;
        Tick(self.base.0.wrapping_add(u64::from(ly_now)))
    }

    /// Map a lightyear tick to its universe tick, relative to the last
    /// [`Self::advance`].
    ///
    /// Pure, and correct across the wrap in both directions: a tick that is
    /// "ahead" by more than half the space is read as being behind, which is
    /// how a 9-tick rollback window straddling the boundary resolves to nine
    /// consecutive universe ticks instead of to four billion.
    #[must_use]
    pub fn resolve(&self, ly: u32) -> Tick {
        let span = u64::from(u32::MAX) + 1;
        if ly > self.last && ly.wrapping_sub(self.last) > HORIZON {
            // Behind the last observed tick, on the far side of the wrap.
            return Tick(self.base.0.wrapping_sub(span).wrapping_add(u64::from(ly)));
        }
        Tick(self.base.0.wrapping_add(u64::from(ly)))
    }

    /// The inverse map, for stamping an Orrery tick onto lightyear's timeline.
    ///
    /// `None` when the universe tick predates the session anchor: lightyear
    /// has no representation for a tick from before it was counting, and
    /// silently wrapping one into range would hand its prediction machinery a
    /// tick from the far future.
    #[must_use]
    pub fn to_lightyear(&self, universe: Tick) -> Option<u32> {
        let delta = universe.0.checked_sub(self.base.0)?;
        u32::try_from(delta).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor is the whole contract: whatever the universe said at the
    /// moment lightyear said `ly_now` must round-trip exactly, or every signed
    /// log entry in the session is stamped with someone else's tick.
    #[test]
    fn anchor_round_trips_the_anchoring_instant() {
        let b = TickBridge::anchor(Tick(1_000_000), 4_242);
        assert_eq!(b.resolve(4_242), Tick(1_000_000));
        assert_eq!(b.to_lightyear(Tick(1_000_000)), Some(4_242));
    }

    /// Ordinary forward progress: universe ticks advance one for one with
    /// lightyear's.
    #[test]
    fn advancing_tracks_one_for_one() {
        let mut b = TickBridge::anchor(Tick(500), 10);
        assert_eq!(b.advance(11), Tick(501));
        assert_eq!(b.advance(12), Tick(502));
        assert_eq!(b.resolve(9), Tick(499));
    }

    /// The two-year fuse: crossing `u32::MAX` must produce consecutive
    /// universe ticks, not a four-billion-tick jump backwards.
    #[test]
    fn crossing_the_u32_wrap_stays_consecutive() {
        let mut b = TickBridge::anchor(Tick(9_000_000_000), u32::MAX - 1);
        let before = b.advance(u32::MAX - 1);
        let at = b.advance(u32::MAX);
        let after = b.advance(0);
        let then = b.advance(1);
        assert_eq!(at.0, before.0 + 1);
        assert_eq!(after.0, at.0 + 1);
        assert_eq!(then.0, after.0 + 1);
    }

    /// Rollback looks backwards, and a 9-tick window that straddles the wrap
    /// must resolve to nine consecutive universe ticks. This is the case that
    /// makes `resolve` a serial-number comparison rather than an addition.
    #[test]
    fn a_rollback_window_straddling_the_wrap_resolves_consecutively() {
        let mut b = TickBridge::anchor(Tick(9_000_000_000), u32::MAX - 4);
        for step in 0..5u32 {
            b.advance(u32::MAX - 4 + step);
        }
        let now = b.advance(3);
        let window: Vec<u64> = [
            u32::MAX - 4,
            u32::MAX - 3,
            u32::MAX - 2,
            u32::MAX - 1,
            u32::MAX,
            0,
            1,
            2,
            3,
        ]
        .iter()
        .map(|ly| b.resolve(*ly).0)
        .collect();
        assert_eq!(*window.last().unwrap(), now.0);
        for pair in window.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "window was {window:?}");
        }
    }

    /// `resolve` must not move the epoch. Rollback calls it many times per
    /// frame, in arbitrary order, and a query that advanced state would make
    /// the answer depend on how many times history had been consulted.
    #[test]
    fn resolve_does_not_move_the_epoch() {
        let mut b = TickBridge::anchor(Tick(9_000_000_000), u32::MAX);
        b.advance(u32::MAX);
        let snapshot = b;
        for ly in [0u32, 1, u32::MAX, u32::MAX - 5, 7] {
            let _ = b.resolve(ly);
        }
        assert_eq!(b, snapshot);
    }

    /// A tick from before the session anchor has no lightyear representation,
    /// and inventing one would hand lightyear a tick from the far future.
    #[test]
    fn ticks_before_the_anchor_have_no_lightyear_tick() {
        let b = TickBridge::anchor(Tick(1_000), 10);
        assert_eq!(b.to_lightyear(Tick(990)), Some(0));
        assert_eq!(b.to_lightyear(Tick(989)), None);
    }
}
