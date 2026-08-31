//! Stage-1 invariant checks (docs/06 §3, D10 stage 1).
//!
//! These live on the `Ruleset` rather than in `orrery_witness` for a reason
//! worth stating plainly: **every interested peer runs them on received
//! authoritative state, regardless of witness-set membership**, and cell actors
//! run them on inbound bulk diffs. They are the only validation most bulk-class
//! state ever gets, so they have to travel with the rules, not with the witness.
//!
//! The contract is deliberately narrow — pure, cheap, `O(received state)`, no
//! history beyond the previous sample. A check that needed a trajectory would
//! be doing the replay harness's job at 20 Hz on every peer's hot path.
//!
//! A violation here is a *signal*, never a verdict. Stage 1 escalates; only
//! replay adjudication (§7) proves anything, and only that can strike.

use orrery_protocol::{PersistId, Tick};

use crate::quantize::{QPos, QVel};
use crate::ruleset::Section;

/// What a stage-1 check found.
///
/// Kinds are coarse on purpose: the escalation path only needs to know *what
/// kind of impossible* was observed, and a finer taxonomy would be a promise
/// about attribution that stage 1 cannot keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InvariantKind {
    /// Displacement implies a speed the archetype cannot reach.
    SpeedCap,
    /// Velocity changed faster than the archetype can accelerate.
    AccelerationCap,
    /// Position jumped further than any continuous motion allows.
    Teleport,
    /// An action repeated faster than its rate limit.
    RateLimit,
    /// A field left its legal range — negative health, impossible currency.
    ValueRange,
}

/// One failed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvariantViolation {
    /// Which check failed.
    pub kind: InvariantKind,
    /// The validator that reported it, for telemetry and triage.
    pub validator: &'static str,
}

impl InvariantViolation {
    /// A violation of `kind`, attributed to `validator`.
    #[must_use]
    pub const fn new(kind: InvariantKind, validator: &'static str) -> Self {
        Self { kind, validator }
    }
}

impl core::fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?} reported by {}", self.kind, self.validator)
    }
}

/// What a check gets to look at: this sample, and at most the previous one.
///
/// `previous` is `None` for the first sample of an entity a peer has just
/// started watching — a check that cannot decide without history must pass
/// rather than accuse, because "I only just met this entity" is not evidence.
pub struct InvariantSample<'a, S> {
    /// The entity being checked.
    pub entity: PersistId,
    /// The state just received, and the tick it is stamped with.
    pub current: &'a S,
    /// The tick `current` is stamped with.
    pub tick: Tick,
    /// The previous sample this peer holds, if any.
    pub previous: Option<&'a S>,
    /// Ticks between `previous` and `current`. Zero when there is no previous.
    ///
    /// Samples arrive at the replication rate, not the simulation rate, and
    /// under loss the gap widens — so every rate-derived check must divide by
    /// this rather than assume adjacency.
    pub elapsed_ticks: u32,
}

impl<'a, S> InvariantSample<'a, S> {
    /// Narrow this sample to one declared section, or `None` when the entity
    /// does not occupy it.
    ///
    /// This is the whole mechanism behind [`section_invariant!`]: it turns a
    /// check that would have opened with a match over every section into a
    /// check whose *signature* names the one section it is about.
    ///
    /// # `previous` narrows independently, and that is deliberate
    ///
    /// `current` decides whether there is a sample at all — a value in another
    /// section yields `None` and the check never runs. `previous` is narrowed
    /// separately, so an entity whose *previous* sample was in a different
    /// section arrives with `previous: None`.
    ///
    /// That is not a hole. It is what the whole-state form already did: a
    /// pair-matching check like `acceleration_cap` opened with
    /// `(Some(Craft(_)), Craft(_)) else { return Ok(()) }`, so a
    /// rock-then-craft pair passed there too. What makes it safe in both forms
    /// is that changing section between two samples is *itself* a violation,
    /// caught by the discriminant arm of `regolith/value-range` — which stays
    /// a whole-state check precisely because it is the one that asks about the
    /// pair rather than about a section.
    #[must_use]
    pub fn project<Sec>(&self) -> Option<InvariantSample<'a, Sec::State>>
    where
        Sec: Section<Root = S>,
    {
        Some(InvariantSample {
            entity: self.entity,
            current: Sec::project(self.current)?,
            tick: self.tick,
            previous: self.previous.and_then(Sec::project),
            elapsed_ticks: self.elapsed_ticks,
        })
    }
}

/// Register a per-section check as a whole-state [`Invariant`].
///
/// The check is written against [`Section::State`] — the payload of one
/// section — and this lifts it to the `Invariant<CoreState>` that
/// [`Ruleset::invariants`](crate::Ruleset::invariants) publishes. An entity in
/// any other section passes without the check running, which is exactly what
/// the discarding arms of a hand-written match did, written once here instead
/// of once per check.
///
/// The lift is a plain `fn` item, so the result is still a function pointer and
/// still const-constructible: an `INVARIANTS` slice keeps being a `const`.
///
/// ```ignore
/// pub const INVARIANTS: &[Invariant<RegolithState>] = &[
///     section_invariant!("regolith/speed-cap", CraftSection, speed_cap::<Craft>),
///     section_invariant!("regolith/acceleration-cap", CraftSection, acceleration_cap),
/// ];
/// ```
///
/// Passing a check written for a *different* section is a type error at the
/// macro's expansion site, not a test failure: the `check` local is annotated
/// with the section's own `State`, so `CraftSection` will not accept a check
/// over `Rock`.
#[macro_export]
macro_rules! section_invariant {
    ($name:expr, $section:ty, $check:expr $(,)?) => {
        $crate::Invariant {
            name: $name,
            check: {
                fn lifted(
                    sample: &$crate::InvariantSample<'_, <$section as $crate::Section>::Root>,
                ) -> ::core::result::Result<(), $crate::InvariantViolation> {
                    // Annotated rather than inferred: this is the line that
                    // refuses a check written against another section.
                    let check: fn(
                        &$crate::InvariantSample<'_, <$section as $crate::Section>::State>,
                    )
                        -> ::core::result::Result<(), $crate::InvariantViolation> = $check;
                    match sample.project::<$section>() {
                        ::core::option::Option::Some(narrowed) => check(&narrowed),
                        ::core::option::Option::None => ::core::result::Result::Ok(()),
                    }
                }
                lifted
            },
        }
    };
}

/// One registered stateless check.
///
/// A plain function pointer, not a trait object: these run on every interested
/// peer for every received sample, so the contract is that they are pure and
/// cheap enough to have no allocation and no dynamic dispatch cost worth
/// naming. It also makes `Send + Sync` automatic rather than a bound a game
/// has to satisfy.
pub struct Invariant<S> {
    /// Name reported in a violation, for telemetry and triage.
    pub name: &'static str,
    /// The check. `Ok(())` passes.
    pub check: fn(&InvariantSample<'_, S>) -> Result<(), InvariantViolation>,
}

impl<S> Clone for Invariant<S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S> Copy for Invariant<S> {}

impl<S> core::fmt::Debug for Invariant<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Invariant")
            .field("name", &self.name)
            .finish()
    }
}

/// Run every check against one sample, returning the first failure.
///
/// First failure, not all of them: stage 1 exists to decide whether to
/// escalate, and one impossibility is as sufficient as five.
pub fn evaluate<S>(
    invariants: &[Invariant<S>],
    sample: &InvariantSample<'_, S>,
) -> Result<(), InvariantViolation> {
    for invariant in invariants {
        (invariant.check)(sample)?;
    }
    Ok(())
}

/// Building blocks for the checks docs/06 §3 names.
///
/// Each takes lattice values and integer limits, so a game's `Invariant` is a
/// thin wrapper that pulls fields out of its own state. Keeping them integer
/// keeps the answer identical on every platform — a check that disagreed
/// between two honest peers would generate escalations out of nothing.
pub mod checks {
    use super::{QPos, QVel};

    /// Whether displacement over `elapsed_ticks` implies more than
    /// `max_mm_per_tick`.
    ///
    /// A zero gap passes: two samples stamped with the same tick describe one
    /// instant, and no speed can be derived from it.
    #[must_use]
    pub fn exceeds_speed(
        previous: QPos,
        current: QPos,
        elapsed_ticks: u32,
        max_mm_per_tick: i64,
    ) -> bool {
        if elapsed_ticks == 0 {
            return false;
        }
        let budget = i128::from(max_mm_per_tick).saturating_mul(i128::from(elapsed_ticks));
        current.distance_squared(previous) > budget.saturating_mul(budget)
    }

    /// Whether the velocity change over `elapsed_ticks` exceeds
    /// `max_mm_per_tick_squared`.
    #[must_use]
    pub fn exceeds_acceleration(
        previous: QVel,
        current: QVel,
        elapsed_ticks: u32,
        max_mm_per_tick_squared: i64,
    ) -> bool {
        if elapsed_ticks == 0 {
            return false;
        }
        let budget = i128::from(max_mm_per_tick_squared).saturating_mul(i128::from(elapsed_ticks));
        current.difference_squared(previous) > budget.saturating_mul(budget)
    }

    /// Whether a single step moved further than `max_jump_mm`, regardless of
    /// how much time passed.
    ///
    /// Distinct from the speed cap: a long enough gap makes any displacement
    /// speed-legal, which is exactly the loophole a teleport rides. This one
    /// has no time term at all.
    #[must_use]
    pub fn is_teleport(previous: QPos, current: QPos, max_jump_mm: i64) -> bool {
        let limit = i128::from(max_jump_mm);
        current.distance_squared(previous) > limit.saturating_mul(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::checks::{exceeds_acceleration, exceeds_speed, is_teleport};
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Body {
        pos: QPos,
        vel: QVel,
        health: i32,
    }

    const WALK: i64 = 100; // mm per tick — 6 m/s at 60 Hz

    fn speed_check(sample: &InvariantSample<'_, Body>) -> Result<(), InvariantViolation> {
        let Some(previous) = sample.previous else {
            return Ok(());
        };
        if exceeds_speed(previous.pos, sample.current.pos, sample.elapsed_ticks, WALK) {
            return Err(InvariantViolation::new(InvariantKind::SpeedCap, "speed"));
        }
        Ok(())
    }

    fn health_check(sample: &InvariantSample<'_, Body>) -> Result<(), InvariantViolation> {
        if sample.current.health < 0 {
            return Err(InvariantViolation::new(InvariantKind::ValueRange, "health"));
        }
        Ok(())
    }

    fn body(x: i64, health: i32) -> Body {
        Body {
            pos: QPos { x, y: 0, z: 0 },
            vel: QVel::default(),
            health,
        }
    }

    fn sample<'a>(
        previous: Option<&'a Body>,
        current: &'a Body,
        elapsed_ticks: u32,
    ) -> InvariantSample<'a, Body> {
        InvariantSample {
            entity: PersistId::new(1),
            current,
            tick: Tick::new(100),
            previous,
            elapsed_ticks,
        }
    }

    const INVARIANTS: &[Invariant<Body>] = &[
        Invariant {
            name: "speed",
            check: speed_check,
        },
        Invariant {
            name: "health",
            check: health_check,
        },
    ];

    #[test]
    fn a_first_sample_cannot_be_accused() {
        // "I only just met this entity" is not evidence. A check that failed
        // here would flag every entity entering interest range.
        let current = body(1_000_000, 100);
        assert_eq!(evaluate(INVARIANTS, &sample(None, &current, 0)), Ok(()));
    }

    #[test]
    fn legal_motion_passes_and_impossible_motion_does_not() {
        let previous = body(0, 100);
        let legal = body(300, 100);
        let impossible = body(3_000, 100);
        assert_eq!(
            evaluate(INVARIANTS, &sample(Some(&previous), &legal, 3)),
            Ok(())
        );
        assert_eq!(
            evaluate(INVARIANTS, &sample(Some(&previous), &impossible, 3)),
            Err(InvariantViolation::new(InvariantKind::SpeedCap, "speed"))
        );
    }

    #[test]
    fn a_wider_sample_gap_widens_the_budget() {
        // Samples arrive at the replication rate and stretch further under
        // loss. A check that assumed adjacency would accuse honest players
        // every time a packet dropped — the D17 risk-3 failure mode.
        let previous = body(0, 100);
        let moved = body(1_000, 100);
        assert!(evaluate(INVARIANTS, &sample(Some(&previous), &moved, 3)).is_err());
        assert_eq!(
            evaluate(INVARIANTS, &sample(Some(&previous), &moved, 30)),
            Ok(())
        );
    }

    #[test]
    fn the_first_failing_check_is_the_one_reported() {
        // Stage 1 decides whether to escalate; one impossibility is as
        // sufficient as five, and running the rest would be work for nothing.
        let previous = body(0, 100);
        let both_wrong = body(3_000, -5);
        assert_eq!(
            evaluate(INVARIANTS, &sample(Some(&previous), &both_wrong, 3)),
            Err(InvariantViolation::new(InvariantKind::SpeedCap, "speed"))
        );
    }

    #[test]
    fn a_check_needing_no_history_still_runs_on_a_first_sample() {
        let current = body(0, -1);
        assert_eq!(
            evaluate(INVARIANTS, &sample(None, &current, 0)),
            Err(InvariantViolation::new(InvariantKind::ValueRange, "health"))
        );
    }

    #[test]
    fn teleport_detection_is_not_subsumed_by_the_speed_cap() {
        // Given a long enough gap any displacement becomes speed-legal, which
        // is precisely the loophole a teleport rides. The jump check has no
        // time term, so it closes it.
        let previous = QPos::default();
        let across_the_map = QPos {
            x: 1_000_000,
            y: 0,
            z: 0,
        };
        assert!(!exceeds_speed(previous, across_the_map, 100_000, WALK));
        assert!(is_teleport(previous, across_the_map, 5_000));
    }

    #[test]
    fn a_zero_tick_gap_derives_no_rate() {
        // Two samples stamped with the same tick describe one instant. Deriving
        // a speed from it would be dividing by zero and calling the result a
        // cheat.
        let previous = QPos::default();
        let far = QPos {
            x: 1_000_000,
            y: 0,
            z: 0,
        };
        assert!(!exceeds_speed(previous, far, 0, WALK));
        assert!(!exceeds_acceleration(
            QVel::default(),
            QVel {
                x: 1_000_000,
                y: 0,
                z: 0
            },
            0,
            10
        ));
    }

    #[test]
    fn acceleration_is_judged_on_the_velocity_delta() {
        let slow = QVel { x: 100, y: 0, z: 0 };
        let sudden = QVel { x: 900, y: 0, z: 0 };
        assert!(exceeds_acceleration(slow, sudden, 1, 100));
        assert!(!exceeds_acceleration(slow, sudden, 10, 100));
    }
}
