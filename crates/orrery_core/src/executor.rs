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

use std::collections::{btree_map::Entry, BTreeMap};

use orrery_protocol::{PersistId, Tick, UniverseSeed};

use crate::quantize::Quantized;
use crate::rng::tick_rng;
use crate::ruleset::{
    state_hash, CoreCodec, EntityMaterialization, OrderedInputs, Ruleset, StateView,
};

/// The fixed simulation rate (VC-1, D8).
pub const TICK_HZ: u32 = 60;

/// One tick's duration in nanoseconds. A constant, never a measurement.
pub const TICK_NANOS: u64 = 1_000_000_000 / TICK_HZ as u64;

/// What one executed tick produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome<E> {
    /// Events emitted, in emission order.
    pub events: Vec<E>,
    /// Entity identifiers installed from those events, in materialization
    /// order. Colliding later descriptions are absent: first writer wins.
    pub materialized: Vec<PersistId>,
    /// Neighbours the step actually read, in first-read order. These become
    /// `NeighborFrame` records.
    pub neighbor_reads: Vec<PersistId>,
    /// Canonical state and declared tick for every neighbour read.
    ///
    /// Log producers encode these as `RecordSource::NeighborFrame` records;
    /// retaining the tick closes the honest-replication-lag ambiguity.
    pub neighbor_frames: Vec<NeighborFrame>,
    /// blake3 over the canonical encoding of the quantized state, after the
    /// step — the value a [`StateClaim`](orrery_protocol::StateClaim) commits
    /// to.
    pub state_hash: [u8; 32],
}

/// One replayable neighbour observation produced by a live step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighborFrame {
    /// Neighbour whose state was read.
    pub neighbor: PersistId,
    /// Tick attached to the replicated state the reader actually held.
    pub observed_tick: Tick,
    /// Canonical quantized `CoreState` bytes, or `None` for an absent lookup.
    pub state: Option<Vec<u8>>,
}

/// Drives a `Ruleset` over entities at the fixed tick.
pub struct Executor<R: Ruleset> {
    ruleset: R,
    seed: UniverseSeed,
    states: BTreeMap<PersistId, R::CoreState>,
    state_ticks: BTreeMap<PersistId, Tick>,
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
            state_ticks: BTreeMap::new(),
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
    pub fn insert(&mut self, entity: PersistId, state: R::CoreState) {
        self.insert_observed(entity, state, Tick::new(0));
    }

    /// Install or replace state carrying the tick at which it was observed.
    ///
    /// Replication consumers use this form. The ordinary [`Self::insert`]
    /// remains useful for tick-zero spawns and starts the observation clock at
    /// zero.
    pub fn insert_observed(
        &mut self,
        entity: PersistId,
        mut state: R::CoreState,
        observed_tick: Tick,
    ) {
        state.quantize();
        self.states.insert(entity, state);
        self.state_ticks.insert(entity, observed_tick);
    }

    /// Read an entity's current state.
    pub fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.states.get(&entity)
    }

    /// Remove and return an entity's state.
    ///
    /// Replay consumers use this to move a materialized child into its own
    /// one-entity executor. A live world normally keeps materializations in
    /// this executor and has no reason to call it.
    pub fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        self.state_ticks.remove(&entity);
        self.states.remove(&entity)
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
        let mut view = StateView::new(entity, &mut own, neighbors);
        let ordered = OrderedInputs::new(inputs);
        let mut rng = tick_rng(self.seed, entity, tick);

        let output = self.ruleset.step(&mut view, &ordered, &mut rng);
        let neighbor_reads = view.recorded_reads().to_vec();
        let neighbor_frames = neighbor_reads
            .iter()
            .map(|neighbor| {
                let state = neighbors.get(neighbor);
                NeighborFrame {
                    neighbor: *neighbor,
                    observed_tick: self.state_ticks.get(neighbor).copied().unwrap_or(tick),
                    state: state.map(CoreCodec::to_canonical),
                }
            })
            .collect();

        // VC-7: snap before anything hashes or replicates it.
        own.quantize();
        let hash = state_hash(&own);
        self.states.insert(entity, own);
        // A claim at T commits to the state before T executes. The state just
        // produced by tick T is therefore the state whose claim tick is T+1.
        self.state_ticks
            .insert(entity, Tick::new(tick.0.saturating_add(1)));

        let mut descriptions = Vec::new();
        for event in &output.events {
            self.ruleset.materialize(event, &mut descriptions);
        }
        let materialized = self.install_materializations(descriptions, tick);

        Some(TickOutcome {
            events: output.events,
            materialized,
            neighbor_reads,
            neighbor_frames,
            state_hash: hash,
        })
    }

    fn install_materializations(
        &mut self,
        descriptions: Vec<EntityMaterialization<R::CoreState>>,
        tick: Tick,
    ) -> Vec<PersistId> {
        let mut materialized = Vec::with_capacity(descriptions.len());
        for EntityMaterialization { entity, mut state } in descriptions {
            if let Entry::Vacant(slot) = self.states.entry(entity) {
                state.quantize();
                slot.insert(state);
                self.state_ticks
                    .insert(entity, Tick::new(tick.0.saturating_add(1)));
                materialized.push(entity);
            }
        }
        materialized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantize::{QPos, QVel};
    use crate::ruleset::{CodecError, CoreCodec, EntityMaterialization, StepOutput};
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ChildSpec {
        entity: PersistId,
        state: Body,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SpawnBatch {
        children: Vec<ChildSpec>,
    }

    impl CoreCodec for SpawnBatch {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&(self.children.len() as u32).to_le_bytes());
            for child in &self.children {
                out.extend_from_slice(&child.entity.0.to_le_bytes());
                child.state.encode(out);
            }
        }

        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            let count = bytes
                .get(..4)
                .and_then(|raw| raw.try_into().ok())
                .map(u32::from_le_bytes)
                .ok_or(CodecError("spawn batch count is missing"))?
                as usize;
            let expected = 4usize
                .checked_add(
                    count
                        .checked_mul(60)
                        .ok_or(CodecError("spawn batch too large"))?,
                )
                .ok_or(CodecError("spawn batch too large"))?;
            if bytes.len() != expected {
                return Err(CodecError("spawn batch has the wrong length"));
            }
            let mut children = Vec::with_capacity(count);
            for index in 0..count {
                let offset = 4 + index * 60;
                let entity = PersistId::new(u64::from_le_bytes(
                    bytes[offset..offset + 8]
                        .try_into()
                        .expect("length checked above"),
                ));
                let state = Body::decode(&bytes[offset + 8..offset + 60])?;
                children.push(ChildSpec { entity, state });
            }
            Ok(Self { children })
        }
    }

    #[derive(Clone, Copy)]
    struct Materializer {
        children: usize,
    }

    impl Ruleset for Materializer {
        type CoreState = Body;
        type CoreInput = Nudge;
        type CoreEvent = SpawnBatch;

        fn id(&self) -> RulesetId {
            RulesetId {
                version: 1,
                digest: [8; 32],
            }
        }

        fn step(
            &self,
            view: &mut StateView<'_, Body>,
            _inputs: &OrderedInputs<'_, Nudge>,
            _rng: &mut crate::rng::TickRng,
        ) -> StepOutput<SpawnBatch> {
            if view.own().pos.x != 0 {
                return StepOutput::default();
            }
            let parent = view.entity();
            view.own_mut().pos.x = 1;
            let children = (0..self.children)
                .map(|slot| ChildSpec {
                    entity: derived_child_id(parent, 1, slot as u64),
                    state: Body {
                        pos: QPos {
                            x: 100 + slot as i64,
                            y: 0,
                            z: 0,
                        },
                        ..body()
                    },
                })
                .collect();
            StepOutput {
                events: vec![SpawnBatch { children }],
            }
        }

        fn materialize(&self, event: &SpawnBatch, out: &mut Vec<EntityMaterialization<Body>>) {
            out.extend(
                event
                    .children
                    .iter()
                    .map(|child| EntityMaterialization::new(child.entity, child.state.clone())),
            );
        }
    }

    fn derived_child_id(parent: PersistId, generation: u64, slot: u64) -> PersistId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"executor-materialization-test");
        hasher.update(&parent.0.to_le_bytes());
        hasher.update(&generation.to_le_bytes());
        hasher.update(&slot.to_le_bytes());
        PersistId::new(u64::from_le_bytes(
            hasher.finalize().as_bytes()[..8]
                .try_into()
                .expect("a digest has eight bytes"),
        ))
    }

    #[test]
    fn materialization_replay_is_isolated_from_world_creation_order() {
        let parent = PersistId::new(41);
        let expected = [
            derived_child_id(parent, 1, 0),
            derived_child_id(parent, 1, 1),
        ];

        let run = |with_unrelated_entity: bool| {
            let mut executor = Executor::new(Materializer { children: 2 }, UniverseSeed([5; 32]));
            if with_unrelated_entity {
                executor.insert(
                    PersistId::new(9_001),
                    Body {
                        pos: QPos { x: 9, y: 0, z: 0 },
                        ..body()
                    },
                );
            }
            executor.insert(parent, body());
            let outcome = executor
                .step_entity(parent, Tick::new(700), &[])
                .expect("the parent is installed");
            let children: Vec<(PersistId, Body)> = expected
                .iter()
                .map(|entity| {
                    (
                        *entity,
                        executor
                            .state(*entity)
                            .expect("the derived child was materialized")
                            .clone(),
                    )
                })
                .collect();
            (outcome.events, outcome.materialized, children)
        };

        let shared_world = run(true);
        let isolated_replay = run(false);
        assert_eq!(shared_world, isolated_replay);
        assert_eq!(shared_world.1, expected);
        assert_eq!(shared_world.0[0].children.len(), 2);
    }

    #[test]
    fn materialization_supports_pickup_split_and_director_batch_shapes() {
        for count in [1, 2, 10] {
            let parent = PersistId::new(50 + count as u64);
            let mut executor =
                Executor::new(Materializer { children: count }, UniverseSeed([6; 32]));
            executor.insert(parent, body());
            let outcome = executor
                .step_entity(parent, Tick::new(800), &[])
                .expect("the source is installed");
            assert_eq!(outcome.materialized.len(), count);
            assert_eq!(executor.entities().count(), count + 1);
        }
    }

    #[test]
    fn materialization_is_first_writer_wins_in_description_order() {
        let occupied = PersistId::new(70);
        let new = PersistId::new(71);
        let mut executor = executor();
        executor.insert(
            occupied,
            Body {
                pos: QPos { x: 7, y: 0, z: 0 },
                ..body()
            },
        );
        let accepted = executor.install_materializations(
            vec![
                EntityMaterialization::new(occupied, Body { rolls: 1, ..body() }),
                EntityMaterialization::new(new, Body { rolls: 2, ..body() }),
                EntityMaterialization::new(new, Body { rolls: 3, ..body() }),
            ],
            Tick::new(0),
        );

        assert_eq!(accepted, vec![new]);
        assert_eq!(executor.state(occupied).expect("kept").pos.x, 7);
        assert_eq!(executor.state(new).expect("created").rolls, 2);
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
    fn a_step_is_told_which_entity_it_is() {
        // Attribution depends on this: a rule emits cross-entity events that
        // are consumed elsewhere, and one that could not name its emitter
        // would produce a log of anonymous effects — damage nobody dealt.
        // It comes from the executor rather than from the state, so a rule
        // cannot claim to be an entity it is not.
        struct Introspect;
        impl Ruleset for Introspect {
            type CoreState = Body;
            type CoreInput = Nudge;
            type CoreEvent = ();
            fn id(&self) -> RulesetId {
                RulesetId {
                    version: 1,
                    digest: [3; 32],
                }
            }
            fn step(
                &self,
                view: &mut StateView<'_, Body>,
                _inputs: &OrderedInputs<'_, Nudge>,
                _rng: &mut crate::rng::TickRng,
            ) -> StepOutput<()> {
                // Recorded into state so the assertion is on what the rule
                // actually saw, not on a value the test handed it.
                let seen = view.entity().0 as i64;
                view.own_mut().pos.x = seen;
                StepOutput::default()
            }
        }

        let mut exec = Executor::new(Introspect, UniverseSeed([5; 32]));
        for id in [7, 9] {
            exec.insert(PersistId::new(id), body());
            exec.step_entity(PersistId::new(id), Tick::new(900), &[])
                .expect("entity present");
            assert_eq!(exec.state(PersistId::new(id)).unwrap().pos.x, id as i64);
        }
    }

    #[test]
    fn a_step_never_sees_itself_as_a_neighbour() {
        // Own state is reached through `own`/`own_mut`; the neighbour map has
        // the stepping entity removed for the duration. A rule that could
        // reach itself both ways would have two aliases for one value, and
        // only one of them recorded in the log.
        struct SelfPeek;
        impl Ruleset for SelfPeek {
            type CoreState = Body;
            type CoreInput = Nudge;
            type CoreEvent = ();
            fn id(&self) -> RulesetId {
                RulesetId {
                    version: 1,
                    digest: [4; 32],
                }
            }
            fn step(
                &self,
                view: &mut StateView<'_, Body>,
                _inputs: &OrderedInputs<'_, Nudge>,
                _rng: &mut crate::rng::TickRng,
            ) -> StepOutput<()> {
                let me = view.entity();
                assert!(view.neighbor(me).is_none(), "reached itself");
                StepOutput::default()
            }
        }

        let mut exec = Executor::new(SelfPeek, UniverseSeed([5; 32]));
        exec.insert(PersistId::new(1), body());
        exec.insert(PersistId::new(2), body());
        let outcome = exec
            .step_entity(PersistId::new(1), Tick::new(900), &[])
            .expect("entity present");
        assert_eq!(outcome.neighbor_reads, vec![PersistId::new(1)]);
        assert!(
            outcome.neighbor_frames[0].state.is_none(),
            "an attempted absent/self read is an explicit absent frame"
        );
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
        assert_eq!(outcome.neighbor_frames.len(), 1);
        assert_eq!(outcome.neighbor_frames[0].neighbor, PersistId::new(2));
        assert_eq!(outcome.neighbor_frames[0].observed_tick, Tick::new(0));
        assert!(outcome.neighbor_frames[0].state.is_some());
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
