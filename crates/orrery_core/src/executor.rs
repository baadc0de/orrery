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
    /// The tick-start slot: what every neighbour read of the current tick is
    /// served from (D43 (b), #758).
    ///
    /// Two slots rather than one map plus a per-read pre-image, because a
    /// read has to answer for entities that have *already* stepped this tick,
    /// and their tick-start value is gone from `states` by then. The slot is
    /// refilled once per tick, in one pass, overwriting its rows in place —
    /// see [`Self::fill_tick_start_slot`] for why that is not `clone_from`.
    tick_start_states: BTreeMap<PersistId, R::CoreState>,
    /// The observation stamps that go with [`Self::tick_start_states`].
    tick_start_ticks: BTreeMap<PersistId, Tick>,
    /// Which tick the two slots above were filled for, or `None` when
    /// something changed the population out of band and they must be refilled.
    snapshot_tick: Option<Tick>,
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
            tick_start_states: BTreeMap::new(),
            tick_start_ticks: BTreeMap::new(),
            snapshot_tick: None,
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
        // Installing state from outside the stepping path moves the world the
        // tick-start slot describes, so the slot stops describing it. The next
        // step refills it rather than serving a read from a population that no
        // longer exists — which is exactly what the adjudicator does when it
        // installs a bundle's neighbour frames before replaying one tick.
        self.snapshot_tick = None;
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
        self.snapshot_tick = None;
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
    /// entity's replay self-contained. That is ADR-0043 clause (b)'s S2 row,
    /// and since #758 it is true of this code and not only of the record.
    ///
    /// Mechanically: reads are served from [`Self::tick_start_states`], which
    /// is filled once per tick and never written by a step. Own state is
    /// mutated in place in `states` and is therefore *present* in the
    /// tick-start slot as well — a rule that reads its own identifier is
    /// refused by [`StateView`], by identity, rather than by the executor
    /// removing the row.
    ///
    /// The canonical arithmetic — RNG derivation, the rule call, neighbour
    /// framing, VC-7 quantization and the state hash — is [`canonical_step`],
    /// which this method owns no copy of. Everything left here is *storage*:
    /// filling the tick-start slot, handing the rule its own state, stamping
    /// the observation tick, installing materializations. That split is what lets
    /// an alternative backend (S7.4, #745) replace the storage without being
    /// able to produce different canonical bytes: there is one implementation
    /// of the canonical stage in the workspace and both backends call it.
    ///
    /// Returns `None` for an entity this executor does not hold.
    pub fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        if !self.states.contains_key(&entity) {
            return None;
        }
        self.fill_tick_start_slot(tick);
        let Self {
            ruleset,
            seed,
            states,
            state_ticks,
            tick_start_states,
            tick_start_ticks,
            ..
        } = self;
        // The write happens in place, and the stamp happens beside it: a
        // buffered write paired with a T+1 stamp would frame a pre-step state
        // as a post-step observation.
        let own = states
            .get_mut(&entity)
            .expect("the entity was present a statement ago");
        let produced = canonical_step(
            CanonicalStep {
                ruleset,
                seed: *seed,
                entity,
                tick,
                inputs,
            },
            own,
            NeighborSnapshot {
                states: tick_start_states,
                observed_ticks: tick_start_ticks,
            },
        );
        // A claim at T commits to the state before T executes. The state just
        // produced by tick T is therefore the state whose claim tick is T+1.
        state_ticks.insert(entity, Tick::new(tick.0.saturating_add(1)));

        let CanonicalOutcome {
            mut outcome,
            materializations,
        } = produced;
        outcome.materialized = self.install_materializations(materializations, tick);
        Some(outcome)
    }

    /// Fill the tick-start slot for `tick`, unless it already holds it.
    ///
    /// Values are overwritten *in place* where the slot already holds the key,
    /// which is every key on every tick of a world whose population is not
    /// changing. So the slot keeps its nodes and its rows keep their own
    /// buffers across ticks — deliberately not `clone_from`, whose `BTreeMap`
    /// implementation is the unspecialized `*self = source.clone()` and would
    /// drop and rebuild the whole map once per tick.
    ///
    /// What remains is one value copy per entity per tick, and that is
    /// irreducible while `Ruleset::step` mutates own state in place: the
    /// pre-image has to survive for the entities that step later, and the
    /// post-image has to exist at the same time. No arrangement of maps
    /// removes it; what an arrangement can remove is the *second* copy and the
    /// per-tick allocation, and this one does.
    fn fill_tick_start_slot(&mut self, tick: Tick) {
        if self.snapshot_tick == Some(tick) {
            return;
        }
        let Self {
            states,
            state_ticks,
            tick_start_states,
            tick_start_ticks,
            ..
        } = self;
        tick_start_states.retain(|entity, _| states.contains_key(entity));
        for (entity, state) in states.iter() {
            match tick_start_states.entry(*entity) {
                Entry::Occupied(mut held) => held.get_mut().clone_from(state),
                Entry::Vacant(slot) => {
                    slot.insert(state.clone());
                }
            }
        }
        tick_start_ticks.retain(|entity, _| state_ticks.contains_key(entity));
        tick_start_ticks.extend(state_ticks.iter().map(|(entity, at)| (*entity, *at)));
        self.snapshot_tick = Some(tick);
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
                // Deliberately not added to the tick-start slot: an entity
                // born during tick T had no state at the start of T, so there
                // is nothing for a later-stepping entity to have observed. It
                // becomes a neighbour on T+1, the same tick it starts
                // stepping on.
                self.state_ticks
                    .insert(entity, Tick::new(tick.0.saturating_add(1)));
                materialized.push(entity);
            }
        }
        materialized
    }
}

/// Everything one canonical per-entity step needs that is not storage.
///
/// A named record rather than five positional parameters: the call site is the
/// boundary between "where state lives" and "what the rules compute", and a
/// reader of a backend should be able to see at a glance that it supplies the
/// ruleset, the universe seed, the identity, the absolute tick and the sealed
/// inputs — and nothing else.
pub struct CanonicalStep<'a, R: Ruleset> {
    /// The rules to run.
    pub ruleset: &'a R,
    /// The universe seed VC-3 derives the tick RNG from.
    pub seed: UniverseSeed,
    /// The entity being advanced.
    pub entity: PersistId,
    /// The absolute tick being executed.
    pub tick: Tick,
    /// This entity's sealed inputs, in applied order (VC-2).
    pub inputs: &'a [R::CoreInput],
}

/// The neighbour states a canonical step may observe, and when each was
/// observed.
///
/// **Tick-start states, not live ones.** Every entity in a tick reads the same
/// map, so what a step observes does not depend on where in the tick it ran
/// (D43 (b), #758). A backend that served this from storage it was
/// concurrently writing would reintroduce execution order into the rules.
///
/// The stepping entity's own state *may* be present here, and a backend is
/// free to leave it: a rule reads its own state through [`StateView::own`],
/// and reading it by identifier is refused by the view and framed absent by
/// [`canonical_step`]. Exclusion is by query shape, not by map surgery —
/// removing the row would also remove it from every *other* entity's read,
/// which is the one thing it must not do.
pub struct NeighborSnapshot<'a, S> {
    /// Every entity's canonical state as of the start of this tick, in
    /// `PersistId` order.
    pub states: &'a BTreeMap<PersistId, S>,
    /// The tick each of those states was observed at.
    pub observed_ticks: &'a BTreeMap<PersistId, Tick>,
}

/// What the canonical stage produced, before any of it is stored.
///
/// [`TickOutcome::materialized`] is empty here and filled in by whichever
/// backend installs the descriptions: which identifiers actually materialize
/// is a first-writer-wins property of the *store*, not of the rules.
pub struct CanonicalOutcome<R: Ruleset> {
    /// The tick outcome, with `materialized` not yet resolved.
    pub outcome: TickOutcome<R::CoreEvent>,
    /// Entities the emitted events asked to materialize, in emission order.
    pub materializations: Vec<EntityMaterialization<R::CoreState>>,
}

/// Run the canonical stage of one entity-tick: derive the RNG (VC-3), call the
/// rule, frame the neighbour reads, quantize (VC-7) and hash.
///
/// This is the whole of what D43 calls canonical about a tick, and the only
/// implementation of it in the workspace. It touches no store: `own` is a
/// borrow the caller owns, neighbours are read-only, and materializations are
/// handed back undecided. That is what makes an alternative storage backend a
/// storage change and nothing more — it cannot reach the bytes.
pub fn canonical_step<R: Ruleset>(
    step: CanonicalStep<'_, R>,
    own: &mut R::CoreState,
    neighbors: NeighborSnapshot<'_, R::CoreState>,
) -> CanonicalOutcome<R> {
    let CanonicalStep {
        ruleset,
        seed,
        entity,
        tick,
        inputs,
    } = step;
    let NeighborSnapshot {
        states,
        observed_ticks,
    } = neighbors;

    let staleness_cap = ruleset.max_neighbor_staleness_ticks();
    let mut view = StateView::observed(entity, own, states, observed_ticks, tick, staleness_cap);
    let ordered = OrderedInputs::new(inputs);
    let mut rng = tick_rng(seed, entity, tick);

    let output = ruleset.step(&mut view, &ordered, &mut rng);
    let neighbor_reads = view.recorded_reads().to_vec();
    let neighbor_frames = neighbor_reads
        .iter()
        .map(|neighbor| {
            // Own state is not a neighbour observation. It is reachable
            // through `StateView::own`, and the view returns `None` for it;
            // the frame the log carries has to say the same.
            let observed_tick = (*neighbor != entity)
                .then(|| observed_ticks.get(neighbor).copied())
                .flatten();
            let fresh = observed_tick.is_some_and(|observed_tick| {
                // Checked, not saturating: with reads served from the
                // tick-start slot, the stepping path can no longer produce an
                // observation stamped ahead of the reader. One arriving by
                // replication is state from the reader's future, which a tick
                // T step may not consult — and which `ReplayHarness` already
                // refuses as a malformed frame. Hiding it here is what makes
                // live execution and replay agree about the same bytes.
                tick.0
                    .checked_sub(observed_tick.0)
                    .is_some_and(|age| age <= staleness_cap)
            });
            let state = fresh.then(|| states.get(neighbor)).flatten();
            NeighborFrame {
                neighbor: *neighbor,
                observed_tick: if fresh {
                    observed_tick.unwrap_or(tick)
                } else {
                    tick
                },
                state: state.map(CoreCodec::to_canonical),
            }
        })
        .collect();

    // VC-7: snap before anything hashes or replicates it.
    own.quantize();
    let hash = state_hash(own);

    let mut materializations = Vec::new();
    for event in &output.events {
        ruleset.materialize(event, &mut materializations);
    }

    CanonicalOutcome {
        outcome: TickOutcome {
            events: output.events,
            materialized: Vec::new(),
            neighbor_reads,
            neighbor_frames,
            state_hash: hash,
        },
        materializations,
    }
}

/// One tick's sealed inputs, by recipient.
///
/// A newtype rather than a bare map so a backend cannot be handed "some
/// map of inputs" whose keying is ambiguous: the key is always the *recipient*
/// `PersistId`, and an entity absent from it steps with no inputs.
#[derive(Debug, Clone)]
pub struct SealedTickInputs<I> {
    by_recipient: BTreeMap<PersistId, Vec<I>>,
}

impl<I> Default for SealedTickInputs<I> {
    fn default() -> Self {
        Self {
            by_recipient: BTreeMap::new(),
        }
    }
}

impl<I> SealedTickInputs<I> {
    /// An empty sealed set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seal one more input for `recipient`, after everything already sealed
    /// for it.
    pub fn push(&mut self, recipient: PersistId, input: I) {
        self.by_recipient.entry(recipient).or_default().push(input);
    }

    /// Seal a whole ordered run of inputs for `recipient`.
    pub fn extend(&mut self, recipient: PersistId, inputs: impl IntoIterator<Item = I>) {
        self.by_recipient
            .entry(recipient)
            .or_default()
            .extend(inputs);
    }

    /// What `recipient` steps with this tick, in applied order.
    #[must_use]
    pub fn for_entity(&self, recipient: PersistId) -> &[I] {
        self.by_recipient
            .get(&recipient)
            .map_or(&[][..], Vec::as_slice)
    }
}

/// One entity's completed tick, as a backend reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteppedEntity<E> {
    /// Whose tick.
    pub entity: PersistId,
    /// What it produced.
    pub outcome: TickOutcome<E>,
}

/// The storage and scheduling substrate a fixed-step driver advances.
///
/// [`Executor`] is the reference implementation and the canonical store. The
/// trait exists so the seam (`orrery_sim_host`) can be handed a different
/// *substrate* — an ECS world, per D42 (d) — while the canonical stage stays
/// [`canonical_step`] and the canonical bytes stay identical by construction
/// rather than by comparison.
///
/// # What an implementor owns, and what it must not touch
///
/// It owns: where states live, the iteration order of [`Self::entities`], and
/// what schedules [`Self::step_tick`]. It must not own: the RNG, the rule
/// call, quantization, hashing, or neighbour framing — all of those are
/// `canonical_step`, and an implementor that reimplements any of them has left
/// the seam and is a second ruleset engine.
///
/// # What within-tick order is still for
///
/// Since #758 it is no longer for *reads*: neighbours are served from the
/// tick-start snapshot, so an entity observes the same world wherever in the
/// tick it ran, and D43 (b)'s S2 row ("steps are independent — snapshot
/// isolation") describes this code.
///
/// Ascending `PersistId` remains canonical because two output properties are
/// now *established* on the vector [`Self::step_tick`] returns, rather than
/// inherited from the order entities happened to step in:
///
/// - **Result reporting order.** [`Self::step_tick`] returns one entry per
///   entity, sorted by ascending `PersistId`.
/// - **Event collection order.** [`TickOutcome::events`] are folded in the
///   order those sorted entries are consumed, so the committed event order is
///   the sorted order, not the execution order.
///
/// Materialization first-writer-wins is the remaining ordering obligation:
/// two entities describing the same identifier in one tick are still resolved
/// by which one ran first, so the winner is still a fact about execution
/// order. A backend that stepped entities in a different order would still be
/// a different simulation until that is also given a deterministic rule.
///
/// What *has* changed is the reason: the order no longer reaches inside a
/// step, only around it — and for result reporting and event collection it
/// is now established explicitly on output.
pub trait TickBackend<R: Ruleset> {
    /// The rules this backend drives.
    fn ruleset(&self) -> &R;

    /// Install or replace an entity's state, quantizing it first (VC-7).
    ///
    /// The observation clock starts at zero, which is what a tick-zero spawn
    /// wants; a replicated or replayed state that was observed later carries
    /// its own tick through [`Self::insert_observed`].
    fn insert(&mut self, entity: PersistId, state: R::CoreState) {
        self.insert_observed(entity, state, Tick::new(0));
    }

    /// Install or replace state carrying the tick at which it was observed.
    ///
    /// Adjudication needs this, not just [`Self::insert`]: a bundle's
    /// neighbour records name the tick each neighbour state was observed at,
    /// and a replay that installed them at tick zero would fail the ruleset's
    /// staleness bound instead of reproducing the read. Quantizes first,
    /// exactly as `insert` does.
    fn insert_observed(&mut self, entity: PersistId, state: R::CoreState, observed_tick: Tick);

    /// Remove and return an entity's state.
    ///
    /// The adjudicator's window is closed: a neighbour installed for one
    /// tick's read, and a child materialized inside a single-entity replay,
    /// both have to leave again, or the next tick would find a population the
    /// authority never had. See [`ReplayHarness::replay`](crate::ReplayHarness).
    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState>;

    /// Read an entity's current canonical state.
    fn state(&self, entity: PersistId) -> Option<&R::CoreState>;

    /// Every installed entity, in ascending `PersistId` order.
    fn entities(&self) -> Vec<PersistId>;

    /// Advance one entity by one tick, or `None` if it is not installed.
    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>>;

    /// Advance every entity installed at the tick boundary and report each
    /// one's outcome, with the returned vector sorted by ascending
    /// [`PersistId`].
    ///
    /// Entities materialized while the tick runs begin stepping on the *next*
    /// tick, never halfway through their birth tick, so the population is
    /// snapshotted before the first entity steps.
    fn step_tick(
        &mut self,
        tick: Tick,
        inputs: &SealedTickInputs<R::CoreInput>,
    ) -> Vec<SteppedEntity<R::CoreEvent>> {
        let population = self.entities();
        let mut stepped = Vec::with_capacity(population.len());
        for entity in population {
            let Some(outcome) = self.step_entity(entity, tick, inputs.for_entity(entity)) else {
                continue;
            };
            stepped.push(SteppedEntity { entity, outcome });
        }
        stepped.sort_by_key(|entry| entry.entity);
        stepped
    }
}

impl<R: Ruleset> TickBackend<R> for Executor<R> {
    fn ruleset(&self) -> &R {
        Executor::ruleset(self)
    }

    fn insert(&mut self, entity: PersistId, state: R::CoreState) {
        Executor::insert(self, entity, state);
    }

    fn insert_observed(&mut self, entity: PersistId, state: R::CoreState, observed_tick: Tick) {
        Executor::insert_observed(self, entity, state, observed_tick);
    }

    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        Executor::take_state(self, entity)
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        Executor::state(self, entity)
    }

    fn entities(&self) -> Vec<PersistId> {
        Executor::entities(self).copied().collect()
    }

    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        Executor::step_entity(self, entity, tick, inputs)
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
            fn max_neighbor_reads(&self) -> usize {
                1
            }
            fn max_neighbor_staleness_ticks(&self) -> u64 {
                1
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
    fn executor_hides_neighbor_observations_older_than_ruleset_cap() {
        struct FreshnessPeeker;
        impl Ruleset for FreshnessPeeker {
            type CoreState = Body;
            type CoreInput = Nudge;
            type CoreEvent = ();

            fn id(&self) -> RulesetId {
                RulesetId {
                    version: 1,
                    digest: [8; 32],
                }
            }

            fn max_neighbor_reads(&self) -> usize {
                1
            }

            fn max_neighbor_staleness_ticks(&self) -> u64 {
                1
            }

            fn step(
                &self,
                view: &mut StateView<'_, Body>,
                _inputs: &OrderedInputs<'_, Nudge>,
                _rng: &mut crate::rng::TickRng,
            ) -> StepOutput<()> {
                if view.neighbor(PersistId::new(2)).is_some() {
                    view.own_mut().pos.x = 1;
                }
                StepOutput::default()
            }
        }

        let mut exec = Executor::new(FreshnessPeeker, UniverseSeed([5; 32]));
        exec.insert_observed(PersistId::new(1), body(), Tick::new(5));
        exec.insert_observed(PersistId::new(2), body(), Tick::new(5));
        let outcome = exec
            .step_entity(PersistId::new(1), Tick::new(7), &[])
            .expect("reader is installed");

        assert_eq!(
            exec.state(PersistId::new(1)).unwrap().pos.x,
            0,
            "neighbor age 2 exceeded the ruleset maximum of 1 tick but remained visible"
        );
        assert_eq!(outcome.neighbor_frames.len(), 1);
        assert!(
            outcome.neighbor_frames[0].state.is_none(),
            "an expired neighbor must be logged as absent for deterministic replay"
        );
        assert_eq!(outcome.neighbor_frames[0].observed_tick, Tick::new(7));
    }

    /// A neighbour stamped *ahead* of the reader's tick is refused.
    ///
    /// This test previously asserted the opposite, and its premise was the
    /// stepping path: `step` stamped T+1 on each entity as it finished, so a
    /// neighbour already stepped this tick carried an observation ahead of its
    /// reader, and hiding it removed same-tick neighbours from every read.
    ///
    /// Snapshot isolation (#758) removes that premise entirely — reads come
    /// from the tick-start slot, and nothing in the stepping path can stamp
    /// ahead of a reader. What survives is the *replication* case:
    /// `insert_observed` takes an arbitrary tick and a peer can hand over an
    /// observation from the reader's future. That is state a tick T step may
    /// not consult, and `ReplayHarness` already refuses such a frame as
    /// malformed (`replay.rs`, `checked_sub` on the reader tick). Live
    /// execution has to agree, or the authority would step on bytes its own
    /// adjudicator rejects.
    #[test]
    fn a_neighbor_stamped_after_the_readers_tick_is_refused() {
        struct FreshnessPeeker;
        impl Ruleset for FreshnessPeeker {
            type CoreState = Body;
            type CoreInput = Nudge;
            type CoreEvent = ();

            fn id(&self) -> RulesetId {
                RulesetId {
                    version: 1,
                    digest: [0; 32],
                }
            }
            fn max_neighbor_reads(&self) -> usize {
                1
            }
            fn max_neighbor_staleness_ticks(&self) -> u64 {
                1
            }
            fn step(
                &self,
                view: &mut StateView<'_, Body>,
                _inputs: &OrderedInputs<'_, Nudge>,
                _rng: &mut crate::rng::TickRng,
            ) -> StepOutput<()> {
                if view.neighbor(PersistId::new(2)).is_some() {
                    view.own_mut().pos.x = 1;
                }
                StepOutput::default()
            }
        }

        let mut exec = Executor::new(FreshnessPeeker, UniverseSeed([5; 32]));
        exec.insert_observed(PersistId::new(1), body(), Tick::new(7));
        // Only replication can produce this now: an observation stamped one
        // tick into the reader's future.
        exec.insert_observed(PersistId::new(2), body(), Tick::new(8));
        let outcome = exec
            .step_entity(PersistId::new(1), Tick::new(7), &[])
            .expect("reader is installed");

        assert_eq!(
            exec.state(PersistId::new(1)).unwrap().pos.x,
            0,
            "a replicated observation from the reader's future was readable; \
             the adjudicator refuses that frame, so live execution would be \
             stepping on bytes its own replay rejects"
        );
        assert!(
            outcome.neighbor_frames[0].state.is_none(),
            "a refused observation must be logged as an explicit absent frame"
        );
        assert_eq!(outcome.neighbor_frames[0].observed_tick, Tick::new(7));
    }

    /// The name, earned: a read *and* a write.
    ///
    /// Entity 2 steps first and its state moves. Entity 1 then steps in the
    /// same tick, reads entity 2 as a neighbour and copies what it saw. Under
    /// live within-tick visibility the copy is entity 2's post-step value;
    /// under snapshot isolation (D43 (b), #758) it is entity 2's tick-start
    /// value, and execution order is not observable to a rule.
    ///
    /// The second assertion is the one this test used to make alone — that
    /// stepping entity 1 does not *mutate* entity 2. It holds under either
    /// semantics, which is why it was never evidence for the name.
    #[test]
    fn a_step_cannot_see_another_entitys_mutation_from_the_same_tick() {
        /// Entity 2 moves itself; entity 1 reads entity 2 and copies its
        /// position into `rolls`, which nothing else in this ruleset writes.
        struct Copier;
        impl Ruleset for Copier {
            type CoreState = Body;
            type CoreInput = Nudge;
            type CoreEvent = ();

            fn id(&self) -> RulesetId {
                RulesetId {
                    version: 1,
                    digest: [9; 32],
                }
            }
            fn max_neighbor_reads(&self) -> usize {
                1
            }
            /// One tick, because both entities are installed at tick 0 and the
            /// read happens at tick 1. Zero — the default — would hide the
            /// neighbour on age and the test would prove nothing.
            fn max_neighbor_staleness_ticks(&self) -> u64 {
                1
            }
            fn step(
                &self,
                view: &mut StateView<'_, Body>,
                _inputs: &OrderedInputs<'_, Nudge>,
                _rng: &mut crate::rng::TickRng,
            ) -> StepOutput<()> {
                if view.entity() == PersistId::new(2) {
                    view.own_mut().pos.x += 1;
                    return StepOutput::default();
                }
                let seen = view
                    .neighbor(PersistId::new(2))
                    .map(|neighbor| neighbor.pos.x)
                    .expect("entity 2 is installed and freshly observed");
                view.own_mut().rolls = u32::try_from(seen).expect("fixture positions are small");
                StepOutput::default()
            }
        }

        let mut exec = Executor::new(Copier, UniverseSeed([5; 32]));
        exec.insert(PersistId::new(1), body());
        exec.insert(
            PersistId::new(2),
            Body {
                pos: QPos { x: 50, y: 0, z: 0 },
                ..body()
            },
        );

        exec.step_entity(PersistId::new(2), Tick::new(1), &[])
            .expect("entity 2 is installed");
        assert_eq!(
            exec.state(PersistId::new(2)).unwrap().pos.x,
            51,
            "the fixture is vacuous unless entity 2's state actually moved"
        );

        let before = exec.state(PersistId::new(2)).cloned();
        exec.step_entity(PersistId::new(1), Tick::new(1), &[])
            .expect("entity 1 is installed");

        assert_eq!(
            exec.state(PersistId::new(1)).unwrap().rolls,
            50,
            "entity 1 read entity 2's post-step state from the same tick; \
             within-tick execution order is observable to the rules"
        );
        assert_eq!(
            exec.state(PersistId::new(2)).cloned(),
            before,
            "stepping entity 1 wrote to entity 2"
        );
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
