//! The `Ruleset` contract (docs/06 §3).
//!
//! A game implements this once; the same build links into peers, field hosts
//! and `persistd`. Every method here is pure — no I/O, no clocks, no globals
//! (VC-8) — because the whole adjudication story is "re-run it somewhere else
//! and get the same answer".
//!
//! **Scoped to what the executor, the replay harness and stage-1 checking
//! need.** The §3 sketch also carries `validate_intent`, `park_tick` and
//! `catch_up`. Each belongs to a consumer that does not exist yet — the intent
//! path, the field host's parked-cell catch-up — and each needs types
//! (`Intent`, `HotStateRead`) that would be invented here and re-invented
//! there. They are additive on this trait when their consumers land.

use std::collections::BTreeMap;

use orrery_protocol::{PersistId, RulesetId, Tick};

use crate::invariants::Invariant;
use crate::quantize::Quantized;
use crate::rng::TickRng;

/// Canonical encoding: `encode` is a pure function of the value, and it is what
/// gets hashed.
///
/// "Canonical" is the whole requirement. Two builds that encode the same state
/// differently produce different state hashes and therefore a false deviation,
/// so field order is fixed and nothing may depend on map iteration order
/// (VC-4).
pub trait CoreCodec: Sized {
    /// Append the canonical encoding to `out`.
    fn encode(&self, out: &mut Vec<u8>);

    /// Decode from canonical bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the bytes are not a valid encoding.
    fn decode(bytes: &[u8]) -> Result<Self, CodecError>;

    /// The canonical bytes as a fresh vector.
    fn to_canonical(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

/// A canonical decoding failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub &'static str);

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl core::error::Error for CodecError {}

/// Where a replicated component sits in the §2 classification — **derived
/// vocabulary, never an authored datum**.
///
/// ADR-0045 clause (g) and A5 §6.2: "Core", "Bulk" and "Cosmetic" stay the
/// names of the load-bearing macro-profiles the documentation set speaks, but
/// the enum stopped being a source of truth when #761 retired
/// `Ruleset::classify_component`. Classification is now declared as data in a
/// build's `orrery_compose::CompatibilityManifest::component_schemas` —
/// five independent capability dimensions per `(ComponentTypeId,
/// SchemaVersion)` — and a value of this enum is *computed* from those five
/// by `orrery_compose::profile_of(..).and_then(CapabilityProfile::core_class)`.
///
/// **Nothing in the tree authors, persists, hashes or routes on this enum.**
/// D-3 persistence, for instance, reads the declared `P` dimension directly
/// (`ComponentCapabilities::is_persisted`) rather than deriving a class and
/// branching on it, because one three-valued name cannot carry five
/// independent dimensions and two of clause (d)'s five profiles have no
/// `CoreClass` value at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoreClass {
    /// Outcomes touch persistent value: full determinism rules, logged,
    /// replayable, adjudicable.
    Core,
    /// Persisted but not adjudicated: quantized replication, bulk writes,
    /// invariant validators only.
    Bulk,
    /// Never persisted, never verified. Nondeterminism welcome.
    Cosmetic,
}

/// A game-assigned identifier for a replicated component type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentTypeId(pub u32);

/// The only state `step` may read or write.
///
/// Own state is mutable; neighbours are read-only, served from the tick-start
/// snapshot (D43 (b)), and **every read is recorded**. Recording is what closes the input set: a
/// replay never needs the neighbour's live state, and an authority that feeds
/// itself fabricated neighbour state to justify an outcome produces checkable
/// evidence against itself.
pub struct StateView<'a, S> {
    entity: PersistId,
    own: &'a mut S,
    neighbors: &'a BTreeMap<PersistId, S>,
    observation_ticks: Option<(&'a BTreeMap<PersistId, Tick>, Tick, u64)>,
    reads: Vec<PersistId>,
}

impl<'a, S> StateView<'a, S> {
    /// Build a view over one entity's own state and its neighbour snapshot.
    pub fn new(entity: PersistId, own: &'a mut S, neighbors: &'a BTreeMap<PersistId, S>) -> Self {
        Self {
            entity,
            own,
            neighbors,
            observation_ticks: None,
            reads: Vec::new(),
        }
    }

    /// Build a live view that hides observations older than the ruleset's
    /// declared staleness cap.
    pub(crate) fn observed(
        entity: PersistId,
        own: &'a mut S,
        neighbors: &'a BTreeMap<PersistId, S>,
        observation_ticks: &'a BTreeMap<PersistId, Tick>,
        tick: Tick,
        staleness_cap: u64,
    ) -> Self {
        Self {
            entity,
            own,
            neighbors,
            observation_ticks: Some((observation_ticks, tick, staleness_cap)),
            reads: Vec::new(),
        }
    }

    /// Which entity is being stepped.
    ///
    /// Supplied by the executor, never by the state, so a rule cannot claim to
    /// be an entity it is not. Rules need it to *attribute* what they emit: a
    /// cross-entity event travels to its target and is consumed there, so an
    /// event that could not name its emitter would arrive anonymous — a game
    /// could resolve damage but never say who dealt it, and the durable
    /// consequences of a kill (credit, loot, the ledger rows a P5 intent
    /// writes) have nobody to attach to.
    pub fn entity(&self) -> PersistId {
        self.entity
    }

    /// This entity's state.
    pub fn own(&self) -> &S {
        self.own
    }

    /// This entity's state, mutably. The only writable state in a step.
    pub fn own_mut(&mut self) -> &mut S {
        self.own
    }

    /// Read a neighbour, recording the read.
    ///
    /// Takes `&mut self` precisely because reading has a side effect on the
    /// log. A view that let neighbours be read without recording would produce
    /// windows that cannot be replayed.
    ///
    /// The stepping entity's own identifier reads as `None`, whether or not
    /// the neighbour map holds a row for it. Own state is reachable through
    /// [`Self::own`] alone; a rule able to reach it both ways would hold two
    /// aliases for one value under two different staleness rules, and only one
    /// of them recorded. The read is still recorded, so the log says the rule
    /// asked.
    pub fn neighbor(&mut self, id: PersistId) -> Option<&S> {
        let readable = id != self.entity;
        let fresh = readable
            && self
                .observation_ticks
                .is_none_or(|(observed, tick, staleness_cap)| {
                    observed.get(&id).is_some_and(|observed_tick| {
                        // Checked, not saturating. Since #758 neighbours are
                        // served from the tick-start slot, so the stepping
                        // path cannot stamp an observation ahead of its
                        // reader; one that is ahead arrived by replication and
                        // is state from the reader's future, which
                        // `ReplayHarness` already refuses as a malformed
                        // frame. Live execution refuses it too, or the two
                        // would disagree about the same log.
                        tick.0
                            .checked_sub(observed_tick.0)
                            .is_some_and(|age| age <= staleness_cap)
                    })
                });
        let found = fresh.then(|| self.neighbors.get(&id)).flatten();
        if !self.reads.contains(&id) {
            self.reads.push(id);
        }
        found
    }

    /// The neighbours read this tick, in first-read order.
    ///
    /// The executor turns these into `NeighborFrame` records. Order is
    /// first-read rather than sorted so the log reflects what the rules
    /// actually did.
    pub fn recorded_reads(&self) -> &[PersistId] {
        &self.reads
    }
}

/// This entity's inputs for this tick, in the total order the authority fixed
/// (VC-2).
///
/// Iteration is log order, always. Replay applies records in log sequence;
/// validators check the order is *legal* but never re-sort — a replay that
/// sorted differently from the authority would manufacture deviations.
pub struct OrderedInputs<'a, I> {
    inputs: &'a [I],
}

impl<'a, I> OrderedInputs<'a, I> {
    /// Wrap a slice already in log order.
    #[must_use]
    pub fn new(inputs: &'a [I]) -> Self {
        Self { inputs }
    }

    /// Iterate in log order.
    pub fn iter(&self) -> core::slice::Iter<'_, I> {
        self.inputs.iter()
    }

    /// How many inputs this tick carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    /// Whether the tick has no inputs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }
}

impl<'a, I> IntoIterator for &'a OrderedInputs<'a, I> {
    type Item = &'a I;
    type IntoIter = core::slice::Iter<'a, I>;

    fn into_iter(self) -> Self::IntoIter {
        self.inputs.iter()
    }
}

/// What one step produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutput<E> {
    /// Emission order is part of determinism — a `Vec`, never a set.
    ///
    /// Cross-entity effects travel only as events: an attacker's step emits
    /// `DamageApplied(target)`; the target consumes it as an input at the next
    /// tick. That is what keeps each entity's replay self-contained.
    pub events: Vec<E>,
}

impl<E> Default for StepOutput<E> {
    fn default() -> Self {
        Self { events: Vec::new() }
    }
}

/// A fully described entity a core event asks the executor to install.
///
/// The ruleset supplies the identifier; the executor deliberately has no
/// allocator. That makes identity a pure function of the emitting entity's
/// replayable inputs (for example `(parent, generation, slot)`) instead of a
/// function of which other entities happened to be created first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityMaterialization<S> {
    /// The game-derived persistent identifier.
    pub entity: PersistId,
    /// The complete initial core state, quantized by the executor on install.
    pub state: S,
}

impl<S> EntityMaterialization<S> {
    /// Describe one entity for deterministic materialization.
    #[must_use]
    pub const fn new(entity: PersistId, state: S) -> Self {
        Self { entity, state }
    }
}

/// The game's deterministic kernel.
pub trait Ruleset: Send + Sync + 'static {
    /// Per-entity verifiable state — the only state `step` may touch.
    /// Discrete fields are integers or fixed-point; continuous fields are
    /// quantized at tick boundaries (VC-7).
    type CoreState: CoreCodec + Clone + Quantized;

    /// One input to a core rule: a player command, or an inbound event from
    /// another entity's previous tick.
    type CoreInput: CoreCodec + Clone;

    /// A deterministic outcome event.
    type CoreEvent: CoreCodec;

    /// This build's version identity, pinned into every frame, claim and
    /// bundle.
    fn id(&self) -> RulesetId;

    /// Maximum recorded neighbour reads one entity may perform in one tick.
    ///
    /// Zero is fail-closed for rulesets that do not use spatial claims. A
    /// replay carrying more frames than this is malformed rather than merely
    /// expensive.
    fn max_neighbor_reads(&self) -> usize {
        0
    }

    /// Oldest neighbour observation this ruleset will consume, in ticks.
    ///
    /// Zero admits only observations stamped with the reader's tick. Games
    /// that accept replication lag must pin a finite non-zero bound.
    fn max_neighbor_staleness_ticks(&self) -> u64 {
        0
    }

    /// Advance one 60 Hz tick for one entity.
    ///
    /// Pure: no I/O, no clocks, no globals; all reads through `view`, all
    /// randomness through `rng`. Re-executing with the same state, inputs and
    /// RNG **must** reproduce the same mutation and the same events in the
    /// same order — that equality is the entire basis on which a window can be
    /// adjudicated.
    fn step(
        &self,
        view: &mut StateView<'_, Self::CoreState>,
        inputs: &OrderedInputs<'_, Self::CoreInput>,
        rng: &mut TickRng,
    ) -> StepOutput<Self::CoreEvent>;

    /// Project one emitted event into fully described entities to install.
    ///
    /// The executor calls this immediately after [`Ruleset::step`], once per
    /// event in emission order, and installs appended entities in append
    /// order. The first description of an identifier wins; later descriptions
    /// are dropped. Existing rulesets need no materialization channel and use
    /// the empty default.
    ///
    /// This projection must be pure and must take every identifier and state
    /// field from the event. In particular, identifiers are derived by the
    /// emitting step from its own replayable inputs; they are never allocated
    /// from executor population or creation order. An isolated replay of the
    /// emitter therefore reproduces the same descriptions even though it does
    /// not hold the rest of the world. Whether a colliding description wins is
    /// an executor concern and does not feed back into the emitter's step.
    ///
    /// Materialization descriptions are not part of [`state_hash`]. A game
    /// whose materialization matters to adjudication must also record an
    /// own-state trace in the emitter (a monotone split/seed/drop counter, for
    /// example); an event-only effect is invisible to state-hash goldens and
    /// adjudication.
    fn materialize(
        &self,
        event: &Self::CoreEvent,
        out: &mut Vec<EntityMaterialization<Self::CoreState>>,
    ) {
        let _ = (event, out);
    }

    /// The stateless stage-1 checks (D10 stage 1, docs/06 §3).
    ///
    /// These live here rather than in `orrery_witness` because *every*
    /// interested peer runs them on received authoritative state, regardless of
    /// witness-set membership, and cell actors run them on inbound bulk diffs.
    /// They are the only validation most bulk-class state ever gets, so they
    /// have to travel with the rules.
    ///
    /// The default is none, which is a real choice rather than a placeholder: a
    /// game with no cheap invariants gets replay adjudication and nothing else,
    /// which is correct but slower to notice. It should supply some.
    fn invariants(&self) -> &[Invariant<Self::CoreState>] {
        &[]
    }
}

/// The canonical state hash committed to by a [`StateClaim`].
///
/// blake3 over the canonical encoding of the **quantized** state (VC-7), so a
/// claim commits to exactly what replication and persistence saw.
#[must_use]
pub fn state_hash<S: CoreCodec>(state: &S) -> [u8; 32] {
    *blake3::hash(&state.to_canonical()).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Bag(u32);

    impl CoreCodec for Bag {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.0.to_le_bytes());
        }
        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            let raw: [u8; 4] = bytes.try_into().map_err(|_| CodecError("bad length"))?;
            Ok(Self(u32::from_le_bytes(raw)))
        }
    }

    #[test]
    fn reading_a_neighbour_records_it_once() {
        // The record is what closes replay over neighbours. Recording twice
        // would inflate the log; recording zero times would break replay.
        let mut own = Bag(1);
        let mut neighbors = BTreeMap::new();
        neighbors.insert(PersistId::new(5), Bag(2));
        neighbors.insert(PersistId::new(6), Bag(3));
        let mut view = StateView::new(PersistId::new(1), &mut own, &neighbors);

        assert_eq!(view.neighbor(PersistId::new(5)), Some(&Bag(2)));
        assert_eq!(view.neighbor(PersistId::new(5)), Some(&Bag(2)));
        assert_eq!(view.recorded_reads(), &[PersistId::new(5)]);

        view.neighbor(PersistId::new(6));
        assert_eq!(
            view.recorded_reads(),
            &[PersistId::new(5), PersistId::new(6)]
        );
    }

    #[test]
    fn a_missing_neighbour_attempt_is_recorded() {
        // Recording a read that returned nothing would make replay demand a
        // neighbour frame the authority never actually consulted.
        let mut own = Bag(1);
        let neighbors = BTreeMap::new();
        let mut view = StateView::new(PersistId::new(1), &mut own, &neighbors);
        assert!(view.neighbor(PersistId::new(9)).is_none());
        assert_eq!(view.recorded_reads(), &[PersistId::new(9)]);
    }

    #[test]
    fn state_hash_is_over_canonical_bytes() {
        assert_eq!(state_hash(&Bag(7)), state_hash(&Bag(7)));
        assert_ne!(state_hash(&Bag(7)), state_hash(&Bag(8)));
    }

    #[test]
    fn ordered_inputs_iterate_in_log_order() {
        // Never sorted: the authority's order is normative (VC-2).
        let inputs = [Bag(3), Bag(1), Bag(2)];
        let ordered = OrderedInputs::new(&inputs);
        assert_eq!(
            ordered.iter().cloned().collect::<Vec<_>>(),
            vec![Bag(3), Bag(1), Bag(2)]
        );
    }
}

// ── module state sections (S7.4, #745) ──────────────────────────────────

/// The stable name of one module-owned section of a game's `CoreState`.
///
/// The same string the composition manifest carries as
/// `orrery_compose::StateSectionId`. It is newtyped here rather than shared
/// because `orrery_compose` depends on this crate and not the other way round;
/// the two are held together by a test in the game that declares both, which
/// is the only place that knows they are the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateSection(pub &'static str);

/// A `CoreState` that says, per value, which declared module section it
/// occupies — and which of those sections a decomposing host stores apart.
///
/// # Why the game declares this and not the host
///
/// A [`TickBackend`](crate::TickBackend) sees an opaque `R::CoreState`. It
/// cannot look inside, so it cannot know that a Regolith entity is a craft
/// rather than a rock, and a store that cannot know that has exactly one
/// archetype: "entity". This trait is the smallest thing a game can say that
/// lets a host keep one module's population in its own component instead.
///
/// # `MIGRATED_SECTIONS` is a migration frontier, not a property of the state
///
/// S7.4 migrates modules to the ECS **one at a time**, each landing only with
/// the four-class differential green across it. The frontier therefore has to
/// be nameable, and this is where it is named: the sections listed here are the
/// ones a decomposing host stores in their own component, and every other
/// section stays in the undivided remainder. Moving a module across the
/// frontier is a one-line edit whose whole blast radius is measured by F-4.
pub trait Sectioned {
    /// The sections a decomposing host stores apart from the remainder.
    ///
    /// Must be a subset of the sections [`Self::section`] can return, and
    /// should be exactly the state sections of the migrated module as the
    /// composition manifest declares them.
    const MIGRATED_SECTIONS: &'static [StateSection];

    /// Which declared section this value occupies.
    fn section(&self) -> StateSection;

    /// Whether this value belongs to a section past the migration frontier.
    fn is_migrated(&self) -> bool {
        Self::MIGRATED_SECTIONS.contains(&self.section())
    }
}
