//! The rollback budget guard (docs/05-prediction-rollback.md §3, D8).
//!
//! Rollback is the one part of prediction whose cost is not bounded by the
//! design: an authoritative update for a tick inside the window forces a replay
//! of every tick since, and the replay itself takes wall-clock time that the
//! frame did not budget for. SnapNet's arithmetic is the reason a guard is
//! mandatory rather than nice: a 60 Hz game absorbing 300 ms of rollback has
//! ~1.1 ms/frame of simulation budget left, and once resim makes a frame late,
//! the next resim is longer — the spiral of death.
//!
//! So the guard never asks "can we afford this replay?" and answers no. It
//! answers with a *plan* that always fits: replay now, or spread over two
//! frames, or shrink the predicted set until it fits, or — the floor — snap the
//! own player and smooth the error. Something always renders.

use std::time::Duration;

use bevy_ecs::prelude::*;

/// How the guard decided a pending replay should be paid for.
///
/// Each variant is a decision the caller can execute without further
/// arithmetic: `ticks_now` is always how many fixed steps to run this frame,
/// and it is always affordable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResimPlan {
    /// The whole replay fits in one frame's resim budget.
    Immediate {
        /// Ticks to replay this frame — all of them.
        ticks_now: u16,
    },
    /// The replay is split across `frames` render frames (ladder step 1).
    ///
    /// Rendering continues from the last completed predicted state and newly
    /// arriving inputs queue; the remainder is replayed on subsequent frames.
    Amortize {
        /// How many render frames the replay is spread over. Never exceeds
        /// [`RollbackBudget::max_amortize_frames`].
        frames: u8,
        /// Ticks to replay this frame.
        ticks_now: u16,
    },
    /// Even the multi-frame budget is exceeded: demote the lowest-priority
    /// members of the predicted set until the projected cost fits (ladder
    /// step 2).
    ///
    /// The caller chooses *which* entities by [`PredictPriority`]; the guard
    /// only knows how many must go.
    Evict {
        /// How many predicted entities to demote to Interpolated.
        demote: u16,
        /// Ticks to replay this frame, after the demotion.
        ticks_now: u16,
    },
    /// The floor (ladder step 3): even the own player alone overruns. Snap it
    /// to the authoritative state and let presentation-side error smoothing
    /// carry the visuals.
    SnapOwnPlayer,
}

impl ResimPlan {
    /// Ticks to replay on this render frame.
    #[must_use]
    pub const fn ticks_now(&self) -> u16 {
        match *self {
            Self::Immediate { ticks_now }
            | Self::Amortize { ticks_now, .. }
            | Self::Evict { ticks_now, .. } => ticks_now,
            Self::SnapOwnPlayer => 0,
        }
    }
}

/// Eviction order for the predicted set (docs/05 §3, ladder step 2).
///
/// Ordered so that `Ord::cmp` puts the *first to be evicted* first: an
/// interaction-predicted crate is a cheaper thing to lose than the entity the
/// player is steering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PredictPriority {
    /// Predicted because a local interaction started (D7 optimistic claim).
    Interaction,
    /// Predicted because this peer holds a weak-authority claim.
    WeakAuthority,
    /// Predicted because this peer strongly owns it.
    StrongOwned,
    /// The own player. Never evicted — the ladder's floor snaps it instead.
    OwnPlayer,
}

/// The rollback budget guard (D8; docs/05 §3).
///
/// Cost is measured, not assumed: `step_cost` is an EWMA of one observed
/// predicted-subset fixed step, so a machine that is genuinely slower gets a
/// smaller predicted set rather than a longer frame.
#[derive(Debug, Clone, Resource)]
pub struct RollbackBudget {
    /// EWMA of the measured cost of one predicted-subset fixed step. D8's
    /// target is ≈ 1 ms; the default seeds the average at that target so the
    /// first frame after startup plans against the design figure rather than
    /// against zero.
    pub step_cost: Duration,
    /// Maximum resim time to spend on a single render frame. Default 5 ms —
    /// under a third of a 60 fps frame, leaving the rest for the ordinary
    /// step, rendering and slack.
    pub max_resim_per_frame: Duration,
    /// Maximum render frames one replay may be spread over. D8: 2.
    pub max_amortize_frames: u8,
    /// Smoothing factor for `step_cost`, as the reciprocal of the weight given
    /// to a new sample: 8 means `ewma += (sample - ewma) / 8`. Integer, so the
    /// EWMA cannot drift between platforms.
    pub cost_smoothing: u32,
    /// The current cap on predicted-set size, halved by hysteresis after two
    /// consecutive overruns and restored after [`Self::recovery_period`] of
    /// clean frames. `None` means uncapped.
    predicted_cap: Option<u16>,
    /// Consecutive frames that planned worse than [`ResimPlan::Immediate`].
    consecutive_overruns: u8,
    /// Clean time accumulated since the last overrun.
    clean_for: Duration,
    /// How long the cap must go unprovoked before it is released. D8's
    /// hysteresis: 5 s.
    pub recovery_period: Duration,
}

impl Default for RollbackBudget {
    fn default() -> Self {
        Self {
            step_cost: Duration::from_micros(1000),
            max_resim_per_frame: Duration::from_millis(5),
            max_amortize_frames: 2,
            cost_smoothing: 8,
            predicted_cap: None,
            consecutive_overruns: 0,
            clean_for: Duration::ZERO,
            recovery_period: Duration::from_secs(5),
        }
    }
}

impl RollbackBudget {
    /// Fold one measured predicted-subset step into the cost EWMA.
    ///
    /// Cheap enough to call every fixed tick, which is the point: the guard's
    /// arithmetic is only as good as its estimate of what a step costs on
    /// *this* machine right now.
    pub fn observe_step(&mut self, measured: Duration) {
        let n = u32::from(self.cost_smoothing.max(1) as u16).max(1);
        let old = self.step_cost.as_nanos();
        let new = measured.as_nanos();
        // Integer EWMA: ewma += (sample - ewma) / n, without going negative.
        let next = if new >= old {
            old + (new - old) / u128::from(n)
        } else {
            old - (old - new) / u128::from(n)
        };
        self.step_cost = Duration::from_nanos(u64::try_from(next).unwrap_or(u64::MAX));
    }

    /// The cap currently imposed on the predicted set, if hysteresis has one.
    #[must_use]
    pub const fn predicted_cap(&self) -> Option<u16> {
        self.predicted_cap
    }

    /// Consecutive frames that could not be paid for immediately.
    #[must_use]
    pub const fn consecutive_overruns(&self) -> u8 {
        self.consecutive_overruns
    }

    /// Account for a render frame during which no replay was needed.
    ///
    /// Calling this is what lets the hysteresis cap expire: without clean
    /// frames the cap is permanent, which would be a machine that never gets
    /// its predicted set back after one bad second.
    pub fn observe_clean_frame(&mut self, frame_time: Duration) {
        self.consecutive_overruns = 0;
        self.clean_for = self.clean_for.saturating_add(frame_time);
        if self.clean_for >= self.recovery_period {
            self.predicted_cap = None;
        }
    }

    /// Plan how to pay for `pending_ticks` of replay across a predicted set of
    /// `predicted_len` entities.
    ///
    /// This is the D8 degradation ladder, evaluated before each resim. It
    /// mutates the guard: the hysteresis counters are part of the decision, so
    /// planning twice for one frame would double-count.
    pub fn plan(&mut self, pending_ticks: u16, predicted_len: u16) -> ResimPlan {
        if pending_ticks == 0 {
            return ResimPlan::Immediate { ticks_now: 0 };
        }

        let frame_budget = self.max_resim_per_frame;
        let total = self.step_cost.saturating_mul(u32::from(pending_ticks));

        // Ladder step 0: it fits in one frame.
        if total <= frame_budget {
            self.consecutive_overruns = 0;
            self.clean_for = Duration::ZERO;
            return ResimPlan::Immediate {
                ticks_now: pending_ticks,
            };
        }

        self.register_overrun();

        let frames = u32::from(self.max_amortize_frames.max(1));
        let amortized_budget = frame_budget.saturating_mul(frames);

        // Ladder step 1: split across up to `max_amortize_frames`.
        if total <= amortized_budget {
            let per_frame = self.ticks_affordable(frame_budget).max(1);
            return ResimPlan::Amortize {
                frames: self.max_amortize_frames.max(1),
                ticks_now: per_frame.min(pending_ticks),
            };
        }

        // Ladder step 2: shrink the predicted set until the projected cost
        // fits the amortized budget. Step cost is taken to scale with set
        // size, which is what makes eviction the lever that works — the
        // alternative (dropping ticks) would desynchronise the prediction.
        //
        // The arithmetic is in nanoseconds because per-entity cost divided out
        // of a millisecond-scale step is sub-microsecond, and `Duration`
        // division would round it to nothing on a large predicted set.
        let len = predicted_len.max(1);
        let per_entity_ns = (self.step_cost.as_nanos() / u128::from(len)).max(1);
        let replay_ns = per_entity_ns.saturating_mul(u128::from(pending_ticks));
        let keep = amortized_budget.as_nanos() / replay_ns;

        if keep == 0 {
            // Ladder step 3, the floor: not even one entity's replay fits, so
            // there is nothing left to evict. Snap the own player and let
            // presentation-side smoothing carry the visuals.
            return ResimPlan::SnapOwnPlayer;
        }

        let keep = u16::try_from(keep).unwrap_or(u16::MAX);
        if keep >= len {
            // The whole set fits the amortized budget after all — the frame
            // budget was what it exceeded, which is the amortize rung.
            return ResimPlan::Amortize {
                frames: self.max_amortize_frames.max(1),
                ticks_now: self
                    .ticks_affordable(frame_budget)
                    .max(1)
                    .min(pending_ticks),
            };
        }

        // Ticks the *kept* set can replay on this frame.
        let kept_step_ns = per_entity_ns.saturating_mul(u128::from(keep)).max(1);
        let ticks_now = u16::try_from(frame_budget.as_nanos() / kept_step_ns)
            .unwrap_or(u16::MAX)
            .max(1)
            .min(pending_ticks);

        ResimPlan::Evict {
            demote: len - keep,
            ticks_now,
        }
    }

    /// How many ticks of the current predicted set fit in `budget`.
    fn ticks_affordable(&self, budget: Duration) -> u16 {
        let step = self.step_cost.as_nanos();
        if step == 0 {
            return u16::MAX;
        }
        u16::try_from(budget.as_nanos() / step).unwrap_or(u16::MAX)
    }

    /// Record that this frame could not pay immediately, and apply D8's
    /// hysteresis: two consecutive overruns halve the predicted-set cap.
    fn register_overrun(&mut self) {
        self.clean_for = Duration::ZERO;
        self.consecutive_overruns = self.consecutive_overruns.saturating_add(1);
        if self.consecutive_overruns >= 2 {
            let current = self.predicted_cap.unwrap_or(crate::config::HIGH_RATE_SET);
            self.predicted_cap = Some((current / 2).max(1));
            self.consecutive_overruns = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap replay must not be amortized: splitting a 3-tick rollback over
    /// two frames would add a frame of latency for no reason.
    #[test]
    fn short_replay_runs_in_one_frame() {
        let mut b = RollbackBudget::default();
        assert_eq!(b.plan(3, 8), ResimPlan::Immediate { ticks_now: 3 });
    }

    /// The worst case D8 sizes for — a full 9-tick window at the 1 ms step
    /// target, 9 ms against a 5 ms frame budget — must land on the amortize
    /// rung and not on eviction. If this fails, an ordinary bad-network moment
    /// starts demoting entities the player is touching.
    #[test]
    fn full_window_at_target_cost_amortizes_rather_than_evicts() {
        let mut b = RollbackBudget::default();
        let plan = b.plan(9, 8);
        match plan {
            ResimPlan::Amortize { frames, ticks_now } => {
                assert_eq!(frames, 2);
                assert_eq!(ticks_now, 5, "5 ms / 1 ms per tick");
            }
            other => panic!("expected amortize, got {other:?}"),
        }
    }

    /// Beyond the two-frame budget the ladder must shed entities, and it must
    /// shed enough that the *projected* cost fits — a guard that evicts one
    /// entity per frame while overrunning is the spiral it exists to prevent.
    #[test]
    fn overlong_replay_evicts_enough_to_fit() {
        // 4 ms per step over 24 entities: a 9-tick replay is 36 ms against a
        // 10 ms two-frame budget.
        let mut b = RollbackBudget {
            step_cost: Duration::from_millis(4),
            ..RollbackBudget::default()
        };
        let plan = b.plan(9, 24);
        match plan {
            ResimPlan::Evict { demote, ticks_now } => {
                let kept = 24 - demote;
                let per_entity = Duration::from_millis(4) / 24;
                let projected = per_entity * u32::from(kept) * 9;
                assert!(
                    projected <= Duration::from_millis(10),
                    "kept {kept} entities projects {projected:?} against a 10 ms budget"
                );
                assert!(ticks_now >= 1);
            }
            other => panic!("expected evict, got {other:?}"),
        }
    }

    /// The floor: when a single entity's replay overruns even the two-frame
    /// budget there is nothing left to evict, and the guard must say so rather
    /// than return an eviction the caller cannot perform.
    #[test]
    fn pathological_cost_snaps_the_own_player() {
        let mut b = RollbackBudget {
            step_cost: Duration::from_millis(40),
            ..RollbackBudget::default()
        };
        assert_eq!(b.plan(9, 1), ResimPlan::SnapOwnPlayer);
    }

    /// Two consecutive overruns halve the predicted-set cap, and only clean
    /// frames release it. A cap that expired on a timer regardless of load
    /// would re-provoke the overrun it was installed to stop.
    #[test]
    fn hysteresis_halves_the_cap_and_only_clean_frames_release_it() {
        let mut b = RollbackBudget {
            step_cost: Duration::from_millis(2),
            ..RollbackBudget::default()
        };
        assert_eq!(b.predicted_cap(), None);
        b.plan(9, 24);
        b.plan(9, 24);
        assert_eq!(b.predicted_cap(), Some(crate::config::HIGH_RATE_SET / 2));

        // Four seconds of calm is not yet five.
        for _ in 0..4 {
            b.observe_clean_frame(Duration::from_secs(1));
        }
        assert_eq!(b.predicted_cap(), Some(crate::config::HIGH_RATE_SET / 2));
        b.observe_clean_frame(Duration::from_secs(1));
        assert_eq!(b.predicted_cap(), None);
    }

    /// A single overrun must not halve the cap: one late packet is not a slow
    /// machine, and D8 asks for *two consecutive* overruns.
    #[test]
    fn single_overrun_does_not_cap() {
        let mut b = RollbackBudget {
            step_cost: Duration::from_millis(2),
            ..RollbackBudget::default()
        };
        b.plan(9, 24);
        assert_eq!(b.predicted_cap(), None);
        b.plan(1, 24);
        assert_eq!(b.consecutive_overruns(), 0);
    }

    /// The EWMA must converge toward observed cost from both directions; a
    /// one-sided estimator would let the guard stay pessimistic forever after
    /// one slow frame.
    #[test]
    fn step_cost_ewma_converges_both_ways() {
        let mut b = RollbackBudget::default();
        for _ in 0..64 {
            b.observe_step(Duration::from_millis(4));
        }
        assert!(
            b.step_cost > Duration::from_micros(3500),
            "{:?}",
            b.step_cost
        );
        for _ in 0..128 {
            b.observe_step(Duration::from_micros(500));
        }
        assert!(
            b.step_cost < Duration::from_micros(700),
            "{:?}",
            b.step_cost
        );
    }

    /// Eviction order is the D8 priority order, expressed so that sorting a
    /// predicted set ascending yields the demotion queue.
    #[test]
    fn eviction_order_demotes_interactions_before_the_own_player() {
        let mut set = vec![
            PredictPriority::OwnPlayer,
            PredictPriority::Interaction,
            PredictPriority::StrongOwned,
            PredictPriority::WeakAuthority,
        ];
        set.sort_unstable();
        assert_eq!(
            set,
            vec![
                PredictPriority::Interaction,
                PredictPriority::WeakAuthority,
                PredictPriority::StrongOwned,
                PredictPriority::OwnPlayer,
            ]
        );
    }
}
