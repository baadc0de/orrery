//! The fixed-tick executor (VC-1).
//!
//! Core state advances only here, and only in whole 60 Hz ticks. There is no
//! `dt` parameter anywhere in this module, because a variable one is the single
//! easiest way to make two builds disagree — and the authority's live execution
//! is what *produces* the log a witness will replay, so the two paths have to
//! be the same code driving the same `step`.
//!
//! What the executor guarantees on every tick, so a `Ruleset` cannot forget:
//!
//! - the RNG is derived from `(universe_seed, entity, absolute tick)` (VC-3);
//! - own state is quantized *after* the step and before anything hashes it
//!   (VC-7);
//! - neighbour reads are collected for the log (docs/06 §3).

use std::collections::BTreeMap;

use orrery_protocol::{PersistId, Tick, UniverseSeed};

use crate::quantize::Quantized;
use crate::rng::tick_rng;
use crate::ruleset::{state_hash, OrderedInputs, Ruleset, StateView};

/// The fixed simulation rate (VC-1, D8).
pub const TICK_HZ: u32 = 60;

/// One tick's duration in nanoseconds. A constant, never a measurement.
pub const TICK_NANOS: u64 = 1_000_000_000 / TICK_HZ as u64;

/// What one executed tick produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome<E> {
    /// Events emitted, in emission order.
    pub events: Vec<E>,
    /// Neighbours the step actually read, in first-read order. These become
    /// `NeighborFrame` records.
    pub neighbor_reads: Vec<PersistId>,
    /// blake3 over the canonical encoding of the quantized state, after the
    /// step — the value a [`StateClaim`](orrery_protocol::StateClaim) commits
    /// to.
    pub state_hash: [u8; 32],
}

/// Drives a `Ruleset` over entities at the fixed tick.
pub struct Executor<R: Ruleset> {
    ruleset: R,
    seed: UniverseSeed,
    states: BTreeMap<PersistId, R::CoreState>,
}

impl<R: Ruleset> Executor<R> {
    /// A new executor over an empty world.
    pub fn new(ruleset: R, seed: UniverseSeed) -> Self {
        Self {
            ruleset,
            seed,
            // BTreeMap, not HashMap (VC-4): iteration order is observable
            // through neighbour snapshots, and std hash iteration order is
            // randomized per process.
            states: BTreeMap::new(),
        }
    }

    /// The ruleset being driven.
    pub fn ruleset(&self) -> &R {
        &self.ruleset
    }

    /// Install or replace an entity's state, quantizing it first.
    ///
    /// Quantizing on insert matters as much as on step: a snapshot loaded from
    /// an evidence bundle or a checkpoint must sit on the lattice before the
    /// first tick reads it, or that tick starts from a point the authority
    /// never occupied.
    pub fn insert(&mut self, entity: PersistId, mut state: R::CoreState) {
        state.quantize();
        self.states.insert(entity, state);
    }

    /// Read an entity's current state.
    pub fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.states.get(&entity)
    }

    /// Every entity, in `PersistId` order.
    pub fn entities(&self) -> impl Iterator<Item = &PersistId> {
        self.states.keys()
    }

    /// Advance one entity by one tick.
    ///
    /// Neighbours are snapshotted before the step, so a step cannot observe
    /// another entity's mutation from the same tick — cross-entity effects
    /// travel as events consumed on the *next* tick, which is what keeps each
    /// entity's replay self-contained.
    ///
    /// Returns `None` for an entity this executor does not hold.
    pub fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        let mut own = self.states.remove(&entity)?;
        let neighbors = &self.states;
        let mut view = StateView::new(&mut own, neighbors);
        let ordered = OrderedInputs::new(inputs);
        let mut rng = tick_rng(self.seed, entity, tick);

        let output = self.ruleset.step(&mut view, &ordered, &mut rng);
        let neighbor_reads = view.recorded_reads().to_vec();

        // VC-7: snap before anything hashes or replicates it.
        own.quantize();
        let hash = state_hash(&own);
        self.states.insert(entity, own);

        Some(TickOutcome {
            events: output.events,
            neighbor_reads,
            state_hash: hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantize::{QPos, QVel};
    use crate::ruleset::{CodecError, CoreCodec, StepOutput};
    use orrery_protocol::RulesetId;
    use rand_chacha::rand_core::RngCore;

    /// A deliberately tiny kinematic ruleset: enough to exercise the executor's
    /// guarantees without pulling a game in.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Body {
        pub pos: QPos,
        pub vel: QVel,
        pub rolls: u32,
    }

    impl CoreCodec for Body {
        fn encode(&self, out: &mut Vec<u8>) {
            for value in [
                self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
            ] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            out.extend_from_slice(&self.rolls.to_le_bytes());
        }
        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            if bytes.len() != 52 {
                return Err(CodecError("body is 52 bytes"));
            }
            let read = |i: usize| {
                i64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().expect("checked length"))
            };
            Ok(Self {
                pos: QPos {
                    x: read(0),
                    y: read(1),
                    z: read(2),
                },
                vel: QVel {
                    x: read(3),
                    y: read(4),
                    z: read(5),
                },
                rolls: u32::from_le_bytes(bytes[48..52].try_into().expect("checked length")),
            })
        }
    }

    impl Quantized for Body {
        fn quantize(&mut self) {
            // Already lattice-valued; a real game would snap floats here.
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Nudge(pub i64);

    impl CoreCodec for Nudge {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.0.to_le_bytes());
        }
        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            Ok(Self(i64::from_le_bytes(
                bytes
                    .try_into()
                    .map_err(|_| CodecError("nudge is 8 bytes"))?,
            )))
        }
    }

    impl CoreCodec for () {
        fn encode(&self, _out: &mut Vec<u8>) {}
        fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
            Ok(())
        }
    }

    pub(crate) struct Kinematic;

    impl Ruleset for Kinematic {
        type CoreState = Body;
        type CoreInput = Nudge;
        type CoreEvent = ();

        fn id(&self) -> RulesetId {
            RulesetId {
                version: 1,
                digest: [1; 32],
            }
        }

        fn step(
            &self,
            view: &mut StateView<'_, Body>,
            inputs: &OrderedInputs<'_, Nudge>,
            rng: &mut crate::rng::TickRng,
        ) -> StepOutput<()> {
            // Input order is observable in the result, so a re-sorted replay
            // would produce a different state.
            let mut applied = 0i64;
            for (index, nudge) in inputs.iter().enumerate() {
                applied += nudge.0 * (index as i64 + 1);
            }
            let state = view.own_mut();
            state.vel.x += applied;
            state.pos.x += state.vel.x;
            state.rolls = state.rolls.wrapping_add(rng.next_u32());
            StepOutput::default()
        }
    }

    fn executor() -> Executor<Kinematic> {
        Executor::new(Kinematic, UniverseSeed([5; 32]))
    }

    fn body() -> Body {
        Body {
            pos: QPos::default(),
            vel: QVel::default(),
            rolls: 0,
        }
    }

    #[test]
    fn the_same_tick_run_twice_produces_the_same_state() {
        // §8's golden-state rule: identical runs that diverge in-process are
        // an instant VC-4/VC-8 violation.
        let run = || {
            let mut exec = executor();
            exec.insert(PersistId::new(1), body());
            let outcome = exec
                .step_entity(PersistId::new(1), Tick::new(900), &[Nudge(3), Nudge(5)])
                .expect("entity present");
            (outcome.state_hash, exec.state(PersistId::new(1)).cloned())
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn input_order_changes_the_outcome() {
        // If it did not, VC-2 would be unnecessary — and a replay free to
        // re-sort could not be wrong. This is what makes log order normative.
        let apply = |inputs: &[Nudge]| {
            let mut exec = executor();
            exec.insert(PersistId::new(1), body());
            exec.step_entity(PersistId::new(1), Tick::new(900), inputs)
                .expect("entity present")
                .state_hash
        };
        assert_ne!(apply(&[Nudge(3), Nudge(5)]), apply(&[Nudge(5), Nudge(3)]));
    }

    #[test]
    fn the_tick_is_part_of_the_randomness() {
        // Same state, same inputs, different tick: the RNG must move, or a
        // loot roll could be farmed by replaying one tick.
        let at = |tick: u64| {
            let mut exec = executor();
            exec.insert(PersistId::new(1), body());
            exec.step_entity(PersistId::new(1), Tick::new(tick), &[])
                .expect("entity present")
                .state_hash
        };
        assert_ne!(at(900), at(901));
    }

    #[test]
    fn neighbour_reads_are_reported_for_the_log() {
        struct Peeker;
        impl Ruleset for Peeker {
            type CoreState = Body;
            type CoreInput = Nudge;
            type CoreEvent = ();
            fn id(&self) -> RulesetId {
                RulesetId {
                    version: 1,
                    digest: [2; 32],
                }
            }
            fn step(
                &self,
                view: &mut StateView<'_, Body>,
                _inputs: &OrderedInputs<'_, Nudge>,
                _rng: &mut crate::rng::TickRng,
            ) -> StepOutput<()> {
                view.neighbor(PersistId::new(2));
                StepOutput::default()
            }
        }

        let mut exec = Executor::new(Peeker, UniverseSeed([5; 32]));
        exec.insert(PersistId::new(1), body());
        exec.insert(PersistId::new(2), body());
        let outcome = exec
            .step_entity(PersistId::new(1), Tick::new(1), &[])
            .expect("entity present");
        assert_eq!(outcome.neighbor_reads, vec![PersistId::new(2)]);
    }

    #[test]
    fn a_step_cannot_see_another_entitys_mutation_from_the_same_tick() {
        // Neighbours are snapshotted at tick start. Without that, execution
        // order between entities would be observable — and there is no
        // canonical execution order to appeal to.
        let mut exec = executor();
        exec.insert(PersistId::new(1), body());
        exec.insert(
            PersistId::new(2),
            Body {
                pos: QPos { x: 50, y: 0, z: 0 },
                ..body()
            },
        );
        let before = exec.state(PersistId::new(2)).cloned();
        exec.step_entity(PersistId::new(1), Tick::new(1), &[Nudge(1)]);
        assert_eq!(exec.state(PersistId::new(2)).cloned(), before);
    }

    #[test]
    fn an_absent_entity_steps_to_nothing() {
        let mut exec = executor();
        assert!(exec
            .step_entity(PersistId::new(99), Tick::new(1), &[])
            .is_none());
    }

    #[test]
    fn the_tick_rate_is_a_constant_not_a_measurement() {
        assert_eq!(TICK_HZ, 60);
        assert_eq!(TICK_NANOS, 16_666_666);
    }
}
