//! An ECS storage-and-scheduling substrate for the seam (S7.4, #745).
//!
//! [`EcsBackend`] is a [`TickBackend`] whose canonical state lives in a
//! dedicated `bevy_ecs::World` and whose tick is driven by a `Schedule` of
//! named stages, rather than in `Executor`'s `BTreeMap` and a `for` loop.
//! [`SimulationHost::on_backend`](crate::SimulationHost::on_backend) accepts
//! it wherever it accepts the executor.
//!
//! # The unit of migration: complete modules' state sections
//!
//! Canonical state is not one component. Entities whose declared state section
//! is past the ruleset's migration frontier
//! ([`orrery_core::Sectioned::MIGRATED_SECTIONS`]) are stored in
//! [`MigratedSection`]; everything else is stored in [`RemainderSection`].
//! Those are two Rust types and therefore two `bevy_ecs` components, so the
//! migrated module's population is a set of archetypes and a query for it
//! visits no other entity.
//!
//! Regolith moved `regolith.world` first — it has no module dependencies while
//! craft depends on it, and its materialized rocks and pickups exercise the
//! only path that adds archetypes during a tick. Lane two advances the frontier
//! by the remaining `regolith.craft` module. The generic migrated/remainder
//! split remains even though every current Regolith section is now past it.
//! The whole blast radius is measured by `tests/ecs_differential.rs`.
//!
//! # Native own-state components, with the whole-state seam intact
//!
//! [`TickBackend::state`] remains declared as
//!
//! ```text
//! fn state(&self, entity: PersistId) -> Option<&R::CoreState>;
//! ```
//!
//! so [`MigratedSection`] still holds the whole `R::CoreState` as the seam's
//! readable cache. S7.4 adds the narrower game-owned components beside it and
//! runs only those components through [`MigratedStep`]. [`MigratedSync`] holds
//! the two representations equal on insert, materialization and post-step
//! quantization. The cache is the only value canonical encoding reads; the
//! concrete component is the own-state projection the rules mutate.
//!
//! # Why this is legal where the previous attempt was not
//!
//! ADR-0042 clause (a) and ADR-0043 clause (e)(1) were amended under the owner
//! acceptance recorded on #793: `orrery_games` may own ECS components and rule
//! systems while `orrery_core` stays Bevy-free. This crate remains the declared
//! Tier-H host and owns the dedicated world; the game receives it only for the
//! duration of one closed, per-entity step.
//!
//! # D42 (a): canonical truth is not in an application world
//!
//! This `World` is *dedicated*: it is created by, owned by and reachable only
//! through one `EcsBackend`. No `bevy_app::App` touches it, no plugin
//! registers against it, no renderer reads it. Nothing outside this crate can
//! obtain a `&World` from it, and a `bevy_ecs::Entity` never escapes — the
//! canonical identity everything outside sees is [`PersistId`], and everything
//! outside reads canonical *bytes* through the host's flat buffers.
//!
//! # And the canonical bytes are not this file's to produce
//!
//! Every byte a tick commits to — the RNG stream (VC-3), the rule call, the
//! neighbour framing, the VC-7 quantization, the state hash — is
//! [`orrery_core::canonical_step`] or its callback form
//! [`orrery_core::canonical_step_with`], which both backends call and neither
//! copies. This file owns *where the state was before the call and where it
//! goes after*, plus the order the calls happen in. There is no expression in
//! this file that can compute a hash.

use std::collections::{btree_map, BTreeMap};
use std::marker::PhantomData;

use bevy_ecs::prelude::{
    Component, Entity, IntoScheduleConfigs, Query, ResMut, Resource, Schedule, World,
};
use bevy_ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use orrery_core::{
    canonical_step, canonical_step_with, sort_materialization_candidates, sort_stepped_entities,
    CanonicalOutcome, CanonicalStep, NeighborSnapshot, OrderedInputs, Quantized, Ruleset,
    SealedTickInputs, Section, Sectioned, StateView, StepOutput, SteppedEntity, TickBackend,
    TickOutcome, TickRng,
};
use orrery_protocol::{PersistId, Tick, UniverseSeed};

/// The associated types a ruleset must be able to put in a `World`.
///
/// `bevy_ecs` requires everything it stores to be `Send + Sync + 'static`.
/// `Ruleset` itself is, but its three associated types are only bounded by the
/// codec traits, so the substrate asks for the rest here rather than sprinkling
/// the bound across every item. A ruleset that does not satisfy it cannot be
/// hosted on an ECS at all — which is itself worth knowing, and is a real
/// restriction the `BTreeMap` store does not impose.
pub trait EcsHostable:
    Ruleset<
    CoreState: Send + Sync + 'static + Sectioned,
    CoreInput: Send + Sync + 'static,
    CoreEvent: Send + Sync + 'static,
>
{
}

impl<R> EcsHostable for R
where
    R: Ruleset,
    R::CoreState: Send + Sync + 'static + Sectioned,
    R::CoreInput: Send + Sync + 'static,
    R::CoreEvent: Send + Sync + 'static,
{
}

/// Synchronize a migrated entity's whole-state cache into game-owned ECS
/// components. `None` removes those components when the entity crosses back to
/// the remainder.
pub type MigratedSync<R> = fn(&mut World, Entity, Option<&<R as Ruleset>::CoreState>);

/// Run one migrated entity's own-state rules as game-owned ECS systems.
///
/// The callback receives the same closed inputs as [`Ruleset::step`]. It runs
/// inside [`orrery_core::canonical_step_with`], so it cannot replace RNG
/// derivation, neighbour framing, quantization, hashing, or materialization.
pub type MigratedStep<R> = for<'state, 'input> fn(
    &mut World,
    Entity,
    &R,
    &mut StateView<'state, <R as Ruleset>::CoreState>,
    &OrderedInputs<'input, <R as Ruleset>::CoreInput>,
    &mut TickRng,
) -> StepOutput<<R as Ruleset>::CoreEvent>;

/// The ruleset-owned adapter for every module currently past the frontier.
#[derive(Resource)]
struct MigratedModule<R: EcsHostable> {
    sync: MigratedSync<R>,
    step: MigratedStep<R>,
}

impl<R: EcsHostable> Clone for MigratedModule<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R: EcsHostable> Copy for MigratedModule<R> {}

/// The one schedule a tick runs.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CanonicalTick;

/// The stages [`CanonicalTick`] chains, in the order it chains them.
///
/// Named and asserted rather than implied by the `.chain()` call below, so a
/// reordering is a diff a reviewer sees. The order is not a preference: stage
/// `seal` fixes the population and the neighbour snapshot *before* any entity
/// steps, which is what makes a materialization land on the next tick and not
/// halfway through its birth tick.
pub const CANONICAL_TICK_STAGES: [&str; 3] = [
    "host.seal-population",
    "host.advance-population",
    "host.install-materializations",
];

/// The stepping entity's stable canonical identity.
///
/// A `bevy_ecs::Entity` is an index into this world and is meaningless outside
/// it; the identity every canonical artifact is keyed by is this one.
#[derive(Component, Debug, Clone, Copy)]
struct Identity(PersistId);

/// One entity's canonical core state when its state section is **past the
/// migration frontier** — for Regolith after S7.4 lane two, all four sections.
///
/// This is a different Rust type from [`RemainderSection`] and therefore a
/// different `bevy_ecs` component, which is the whole of the decomposition:
/// the migrated module's entities live in their own archetype, and a query for
/// them visits no other entity's memory and needs no discriminant test. The
/// *payload* is still the whole `R::CoreState`. Since #791 a **caller** can
/// narrow — [`TickBackend::section_state`] hands out one section's own type —
/// but the stored bytes here are still the sum, and this backend therefore
/// takes that method's provided default. See the module note.
#[derive(Component, Debug, Clone)]
struct MigratedSection<R: EcsHostable>(R::CoreState);

/// One entity's canonical core state when its section has **not** been
/// migrated. The generic undivided remainder; Regolith currently has none.
#[derive(Component, Debug, Clone)]
struct RemainderSection<R: EcsHostable>(R::CoreState);

/// Which side of the migration frontier one entity's state sits on.
///
/// Named rather than a `bool`, because the two sides are two component types
/// and a boolean at a call site says which one only by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// Stored in [`MigratedSection`].
    Migrated,
    /// Stored in [`RemainderSection`].
    Remainder,
}

impl Side {
    /// The side a value's declared section puts it on.
    fn of<S: Sectioned>(state: &S) -> Self {
        if state.is_migrated() {
            Self::Migrated
        } else {
            Self::Remainder
        }
    }
}

/// The tick at which this entity's canonical state was observed.
#[derive(Component, Debug, Clone, Copy)]
struct ObservedAt(Tick);

/// The rules this world runs, and the universe seed VC-3 derives from.
#[derive(Resource)]
struct Rules<R: EcsHostable> {
    ruleset: R,
    seed: UniverseSeed,
}

/// Where one canonical entity lives in this world.
///
/// A newtype rather than a bare `(Entity, Side)`, because the two fields are
/// two different kinds of fact — one is a handle into this world, the other is
/// which component type holds the state — and a tuple at a call site says
/// which is which only by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    /// This world's handle for the entity.
    entity: Entity,
    /// The component type its canonical state is filed in.
    side: Side,
}

/// `PersistId` → [`Slot`]. The ECS's own identifier is not canonical, so the
/// substrate must carry this index; it is also what gives [`TickBackend::entities`]
/// its ascending-`PersistId` order, which archetype iteration does not have.
///
/// Carrying the [`Side`] here rather than re-deriving it is what makes a
/// canonical read *one* component lookup. Before, [`EcsBackend::state_at`]
/// probed [`MigratedSection`] and fell back to [`RemainderSection`], so every
/// entity on the remainder side — which for Regolith is every craft, and a
/// craft population is what the capacity leg measures — paid a miss before its
/// hit. The index already had to exist and already had to be maintained at
/// exactly the three places the side changes, so the side rides along for
/// free.
#[derive(Resource, Debug, Default)]
struct Index(BTreeMap<PersistId, Slot>);

/// What the seal stage fixed for this tick.
#[derive(Resource)]
struct TickPlan<R: EcsHostable> {
    tick: Tick,
    /// The population at the tick boundary, in canonical order.
    order: Vec<StepSlot>,
    /// When set, only this entity advances; everyone else is sealed as a
    /// neighbour and left where they are. This is
    /// [`TickBackend::step_entity`]'s shape, which the seam does not use but
    /// the trait exposes — a backend that quietly advanced the whole
    /// population under it would be a very expensive surprise.
    only: Option<PersistId>,
    inputs: SealedTickInputs<R::CoreInput>,
}

/// One entity scheduled to step this tick, under both identities.
#[derive(Debug, Clone, Copy)]
struct StepSlot {
    persist: PersistId,
    entity: Entity,
    side: Side,
}

/// The tick-start neighbour snapshot, filled by `seal` and read-only after it.
///
/// This is the substrate's least comfortable object and the honest place to
/// say why it exists: `StateView` — frozen, and rightly so — reads neighbours
/// out of a `&BTreeMap<PersistId, S>`. An ECS that stores state in components
/// therefore has to reconstitute the very map it replaced before it can call a
/// rule. See the crate report for what that costs a rules author.
///
/// Since #758 the advance stage never writes to it. That is the whole of the
/// backend's half of snapshot isolation, and it is also what took the *second*
/// copy off this path: a stepped entity's new state is now written straight
/// into its component and mirrored nowhere.
#[derive(Resource)]
struct Neighborhood<R: EcsHostable> {
    states: BTreeMap<PersistId, R::CoreState>,
    observed: BTreeMap<PersistId, Tick>,
}

/// What the tick produced, in canonical order.
#[derive(Resource)]
struct TickResults<R: EcsHostable> {
    stepped: Vec<SteppedEntity<R::CoreEvent>>,
    /// Materializations the advance stage decided on, awaiting their spawn.
    spawned: Vec<Materialized<R>>,
}

/// One entity the tick decided to materialize, and its initial state.
struct Materialized<R: EcsHostable> {
    entity: PersistId,
    state: R::CoreState,
}

/// Stage 1 — `host.seal-population`.
///
/// Fixes the population and snapshots every entity's canonical state and
/// observation tick — including the state of each entity that is about to
/// step, because *other* entities must go on reading its tick-start value all
/// tick. Archetype iteration order is not `PersistId` order, so the population
/// is sorted here. The returned [`TickResults::stepped`] vector is sorted
/// again before it leaves the backend, so result reporting and event
/// collection order are established properties of the output rather than
/// inherited from the iteration order. Materialization candidates are likewise
/// sorted by their explicit winner key after every step completes.
fn seal_population<R: EcsHostable>(
    migrated: Query<(Entity, &Identity, &MigratedSection<R>, &ObservedAt)>,
    remainder: Query<(Entity, &Identity, &RemainderSection<R>, &ObservedAt)>,
    mut plan: ResMut<TickPlan<R>>,
    mut neighborhood: ResMut<Neighborhood<R>>,
) {
    let mut order = Vec::new();
    let neighborhood = neighborhood.as_mut();
    {
        let mut seal = |entity: Entity,
                        identity: &Identity,
                        state: &R::CoreState,
                        observed: &ObservedAt,
                        side: Side| {
            order.push(StepSlot {
                persist: identity.0,
                entity,
                side,
            });
            // Overwritten in place, not cleared and rebuilt: the population is the
            // same set on almost every tick, so the view keeps its nodes and each
            // row keeps its own buffer. Entities that left are dropped below,
            // against the order that was just sealed.
            match neighborhood.states.entry(identity.0) {
                btree_map::Entry::Occupied(mut held) => held.get_mut().clone_from(state),
                btree_map::Entry::Vacant(slot) => {
                    slot.insert(state.clone());
                }
            }
            neighborhood.observed.insert(identity.0, observed.0);
        };
        // Two queries, one per side of the frontier. Each visits exactly its own
        // archetypes: the migrated module's population is selected by the storage
        // layout, not by testing a discriminant on every entity in the store.
        for (entity, identity, canonical, observed) in &migrated {
            seal(entity, identity, &canonical.0, observed, Side::Migrated);
        }
        for (entity, identity, canonical, observed) in &remainder {
            seal(entity, identity, &canonical.0, observed, Side::Remainder);
        }
    }
    order.sort_by_key(|slot| slot.persist);
    let held = |entity: &PersistId| {
        order
            .binary_search_by_key(entity, |slot| slot.persist)
            .is_ok()
    };
    neighborhood.states.retain(|entity, _| held(entity));
    neighborhood.observed.retain(|entity, _| held(entity));
    plan.order = order;
}

/// Canonical whole-state storage on both sides of the migration frontier.
///
/// #789 introduced this boundary as a `SystemParam`. Native game systems now
/// need an exclusive borrow of the same dedicated world, so S7.4 keeps the
/// boundary but makes its operations short-lived: take the seam cache before
/// the nested module schedule runs, then restore the post-quantized cache
/// afterwards. Callers still do not know which component type holds a side.
struct SectionStore<R: EcsHostable>(PhantomData<fn() -> R>);

impl<R: EcsHostable> SectionStore<R> {
    /// Take one sealed entity's whole-state cache from the side its slot names.
    fn take(world: &mut World, slot: &StepSlot) -> Option<R::CoreState> {
        match slot.side {
            Side::Migrated => world
                .entity_mut(slot.entity)
                .take::<MigratedSection<R>>()
                .map(|held| held.0),
            Side::Remainder => world
                .entity_mut(slot.entity)
                .take::<RemainderSection<R>>()
                .map(|held| held.0),
        }
    }

    /// Restore the post-step cache and observation stamp on its declared side.
    fn restore(world: &mut World, slot: &StepSlot, state: R::CoreState, observed: Tick) {
        let mut held = world.entity_mut(slot.entity);
        match slot.side {
            Side::Migrated => held.insert(MigratedSection::<R>(state)),
            Side::Remainder => held.insert(RemainderSection::<R>(state)),
        };
        held.insert(ObservedAt(observed));
    }
}

/// Stage 2 — `host.advance-population`.
///
/// Runs the canonical stage for each sealed entity, in order, mutating its
/// canonical component in place. The sealed neighbour view is **not** touched:
/// every entity in the tick reads the state the world had when the tick began
/// (D43 (b), #758), so what a rule observes does not depend on where in the
/// tick it ran. The vector written to [`TickResults::stepped`] is sorted by
/// ascending `PersistId` before it is returned, establishing the output order
/// regardless of the iteration order used here.
///
/// This is exclusive because a migrated module's own systems run over this
/// same dedicated `World`. The whole-state component is taken out while those
/// systems execute, then restored after `canonical_step_with` quantizes and
/// hashes it; no aliased world borrow and no second byte path exists.
fn advance_population<R: EcsHostable>(world: &mut World) {
    let rules = world
        .remove_resource::<Rules<R>>()
        .expect("the canonical world holds its rules");
    let plan = world
        .remove_resource::<TickPlan<R>>()
        .expect("the canonical world holds its tick plan");
    let neighborhood = world
        .remove_resource::<Neighborhood<R>>()
        .expect("the canonical world holds its sealed neighborhood");
    let mut results = world
        .remove_resource::<TickResults<R>>()
        .expect("the canonical world holds its tick results");
    let migrated_module = world.get_resource::<MigratedModule<R>>().copied();

    let tick = plan.tick;
    let settled = Tick::new(tick.0.saturating_add(1));
    results.stepped.clear();
    results.spawned.clear();
    let mut candidates = Vec::new();

    for slot in &plan.order {
        if plan.only.is_some_and(|only| only != slot.persist) {
            continue;
        }
        // The sealed view is a separate tick-start snapshot, so taking the live
        // whole-state component out does not hide this entity from any reader.
        let own = SectionStore::<R>::take(world, slot);
        let Some(mut own) = own else {
            continue;
        };
        let canonical = CanonicalStep {
            ruleset: &rules.ruleset,
            seed: rules.seed,
            entity: slot.persist,
            tick,
            inputs: plan.inputs.for_entity(slot.persist),
        };
        let neighbors = NeighborSnapshot {
            states: &neighborhood.states,
            observed_ticks: &neighborhood.observed,
        };
        let CanonicalOutcome {
            outcome,
            materializations,
        } = match (slot.side, migrated_module) {
            (Side::Migrated, Some(module)) => canonical_step_with(
                canonical,
                &mut own,
                neighbors,
                |ruleset, view, inputs, rng| {
                    (module.step)(world, slot.entity, ruleset, view, inputs, rng)
                },
            ),
            _ => canonical_step(canonical, &mut own, neighbors),
        };

        if let (Side::Migrated, Some(module)) = (slot.side, migrated_module) {
            (module.sync)(world, slot.entity, Some(&own));
        }
        // The stamp goes beside the post-step state, so a T+1 stamp is never
        // paired with a pre-step value.
        SectionStore::<R>::restore(world, slot, own, settled);

        candidates.extend(materializations);
        results.stepped.push(SteppedEntity {
            entity: slot.persist,
            outcome,
        });
    }

    // Establish lowest-emitter, then emission-index order after every
    // canonical step. `Index` refuses identifiers occupied at the tick
    // boundary; `results.spawned` retains the first candidate for each new
    // identifier. Neither half depends on the loop order above.
    sort_materialization_candidates(&mut candidates);
    for candidate in candidates {
        let emitter = candidate.emitter;
        let description = candidate.description;
        if world
            .resource::<Index>()
            .0
            .contains_key(&description.entity)
            || results
                .spawned
                .iter()
                .any(|pending| pending.entity == description.entity)
        {
            continue;
        }
        let mut state = description.state;
        state.quantize();
        let outcome = results
            .stepped
            .iter_mut()
            .find(|stepped| stepped.entity == emitter)
            .expect("a materialization candidate came from a stepped entity");
        outcome.outcome.materialized.push(description.entity);
        // Deliberately not added to the sealed view: an entity born during
        // tick T had no state at the start of T, so no entity in T can have
        // observed one. It becomes a neighbour on T+1, the tick it starts
        // stepping on.
        results.spawned.push(Materialized {
            entity: description.entity,
            state,
        });
    }

    world.insert_resource(rules);
    world.insert_resource(plan);
    world.insert_resource(neighborhood);
    world.insert_resource(results);
}

/// Stage 3 — `host.install-materializations`.
///
/// Gives every entity the advance stage decided on a real `World` entity and
/// an index row. It is exclusive because a spawn must be reflected in the
/// index before the next tick seals, and `Commands` would defer it past the
/// end of the schedule.
fn install_materializations<R: EcsHostable>(world: &mut World) {
    let spawned = {
        let mut results = world.resource_mut::<TickResults<R>>();
        std::mem::take(&mut results.spawned)
    };
    let settled = world.resource::<TickPlan<R>>().tick;
    let settled = Tick::new(settled.0.saturating_add(1));
    for materialized in spawned {
        // A newborn is placed by its own declared section, so a rock materialized
        // by a split lands directly past the migration frontier and is never
        // briefly a member of the remainder. This is the only place in the
        // substrate where a spawn's archetype is chosen, and it is why
        // `regolith.world` — the module whose population changes mid-tick — was
        // migrated first.
        let side = Side::of(&materialized.state);
        let native_state = (side == Side::Migrated).then(|| materialized.state.clone());
        let mut spawn = world.spawn((Identity(materialized.entity), ObservedAt(settled)));
        match side {
            Side::Migrated => spawn.insert(MigratedSection::<R>(materialized.state)),
            Side::Remainder => spawn.insert(RemainderSection::<R>(materialized.state)),
        };
        let entity = spawn.id();
        world
            .resource_mut::<Index>()
            .0
            .insert(materialized.entity, Slot { entity, side });
        if let Some(module) = world.get_resource::<MigratedModule<R>>().copied() {
            (module.sync)(world, entity, native_state.as_ref());
        }
    }
}

/// Whether [`canonical_schedule`] chains its stages or leaves them unordered.
///
/// The unordered arm exists only for D43 (c)(1)'s canary mutant and is never
/// reachable from [`EcsBackend::new`]; a backend is always built `Chained`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ordering {
    Chained,
    Unordered,
}

/// The dedicated world every `EcsBackend` runs on, with its resources installed.
///
/// Factored out of `EcsBackend::new` so the ambiguity canary composes the
/// schedule against *this* world rather than an approximation of it: a canary
/// that initializes against a different resource set is not a canary for the
/// schedule that ships.
fn canonical_world<R: EcsHostable>(ruleset: R, seed: UniverseSeed) -> World {
    let mut world = World::new();
    world.insert_resource(Rules { ruleset, seed });
    world.insert_resource(Index::default());
    world.insert_resource(TickPlan::<R> {
        tick: Tick::new(0),
        order: Vec::new(),
        only: None,
        inputs: SealedTickInputs::new(),
    });
    world.insert_resource(Neighborhood::<R> {
        states: BTreeMap::new(),
        observed: BTreeMap::new(),
    });
    world.insert_resource(TickResults::<R> {
        stepped: Vec::new(),
        spawned: Vec::new(),
    });
    world
}

/// The canonical tick schedule.
///
/// `Ordering::Chained` is the shipped one: explicitly chained, because the
/// canonical schedule declares no ambiguity, so neither may this one.
/// `bevy_ecs` is built here without `multi_threaded`, so single-threaded
/// execution is a property of the build and not a request this schedule makes.
fn canonical_schedule<R: EcsHostable>(ordering: Ordering) -> Schedule {
    let mut schedule = Schedule::new(CanonicalTick);
    let stages = (
        seal_population::<R>,
        advance_population::<R>,
        install_materializations::<R>,
    );
    match ordering {
        Ordering::Chained => schedule.add_systems(stages.chain()),
        Ordering::Unordered => schedule.add_systems(stages),
    };
    // The declared stage list is not decoration: a fourth system added to the
    // chain without a row in `CANONICAL_TICK_STAGES` is a stage the published
    // order does not mention, and that is how a schedule starts running
    // something nobody reviewed.
    assert_eq!(
        schedule.systems_len(),
        CANONICAL_TICK_STAGES.len(),
        "the canonical tick schedule runs a different number of stages than it declares"
    );
    schedule
}

/// Initialize `schedule` against `world` with ambiguity promoted to an error.
fn audit_ambiguity(mut schedule: Schedule, mut world: World) -> Result<(), String> {
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..ScheduleBuildSettings::default()
    });
    schedule
        .initialize(&mut world)
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

/// Canonical state in a dedicated `bevy_ecs::World`, stepped by a `Schedule`.
///
/// See the module documentation for what this does and does not own. It is
/// interchangeable with `Executor` at
/// [`SimulationHost::on_backend`](crate::SimulationHost::on_backend), and the
/// S7.4 differential is the two of them driven from identical sealed inputs.
pub struct EcsBackend<R: EcsHostable> {
    world: World,
    schedule: Schedule,
    ruleset: PhantomData<fn() -> R>,
}

impl<R: EcsHostable> EcsBackend<R> {
    /// Build a dedicated world for `ruleset` under `seed`.
    #[must_use]
    pub fn new(ruleset: R, seed: UniverseSeed) -> Self {
        Self {
            world: canonical_world(ruleset, seed),
            schedule: canonical_schedule::<R>(Ordering::Chained),
            ruleset: PhantomData,
        }
    }

    /// Install the game-owned ECS implementation for the migrated module.
    ///
    /// The migration remains explicit at construction: [`EcsBackend::new`]
    /// continues to host any [`EcsHostable`] ruleset through its ordinary
    /// `Ruleset::step`, while this builder opts one declared module into native
    /// components and systems. Existing entities are synchronized immediately,
    /// so the builder is correct before or after seeding.
    #[must_use]
    pub fn with_migrated_module(mut self, sync: MigratedSync<R>, step: MigratedStep<R>) -> Self {
        let module = MigratedModule { sync, step };
        self.world.insert_resource(module);
        let migrated: Vec<(Entity, R::CoreState)> = self
            .world
            .resource::<Index>()
            .0
            .values()
            .filter(|slot| slot.side == Side::Migrated)
            .filter_map(|slot| {
                self.state_at(*slot)
                    .cloned()
                    .map(|state| (slot.entity, state))
            })
            .collect();
        for (entity, state) in migrated {
            (sync)(&mut self.world, entity, Some(&state));
        }
        self
    }

    /// D43 (c)(1)'s ambiguity canary, arming direction.
    ///
    /// Builds the *real* canonical schedule against a real canonical world,
    /// turns `bevy_ecs`'s ambiguity detection up from its default `Ignore` to
    /// `Error`, and initializes. `Ok(())` means the shipped schedule carries an
    /// explicit ordering edge between every pair of systems with conflicting
    /// data access; an `Err` means the schedule this backend actually runs has
    /// an ambiguity in it — which the record refuses at composition rather than
    /// logs.
    ///
    /// Neither direction is evidence on its own: a rejector that rejects
    /// nothing returns `Ok` here too. The other direction is
    /// [`EcsBackend::ambiguity_audit_of_the_unordered_mutant`], and the record
    /// requires both in CI.
    ///
    /// # Errors
    ///
    /// The `bevy_ecs` schedule build error, rendered, when the canonical
    /// schedule does not compose unambiguously.
    pub fn ambiguity_audit(ruleset: R, seed: UniverseSeed) -> Result<(), String> {
        audit_ambiguity(
            canonical_schedule::<R>(Ordering::Chained),
            canonical_world(ruleset, seed),
        )
    }

    /// D43 (c)(1)'s ambiguity canary, refuting direction.
    ///
    /// The same three systems over the same world with the `.chain()` taken
    /// off, and nothing else changed. `seal_population` and
    /// `advance_population` both take `ResMut<Neighborhood<R>>`, and
    /// `install_materializations` is exclusive, so with no ordering edges this
    /// schedule is ambiguous by construction. If it initializes `Ok`, the
    /// rejector is asleep and every `Ok` from [`EcsBackend::ambiguity_audit`]
    /// is worth nothing.
    ///
    /// A3's probe ran an ambiguous schedule 200/200 identical, which is why
    /// this is a composition-time check and not a repetition count.
    ///
    /// # Errors
    ///
    /// The `bevy_ecs` schedule build error, rendered — which is the outcome
    /// this function exists to obtain.
    pub fn ambiguity_audit_of_the_unordered_mutant(
        ruleset: R,
        seed: UniverseSeed,
    ) -> Result<(), String> {
        audit_ambiguity(
            canonical_schedule::<R>(Ordering::Unordered),
            canonical_world(ruleset, seed),
        )
    }

    /// The population in **archetype iteration order** — the order `bevy_ecs`
    /// happens to hold it in, which follows spawn history.
    ///
    /// Nothing canonical may be derived from this, and nothing is: no caller
    /// inside this crate reads it, and [`TickBackend::entities`] remains the
    /// `PersistId`-ordered index. It exists for D43 (e)(4)'s projection
    /// differential, which has to *prove* that permuting insertion order
    /// really moved the substrate's storage order — a differential over
    /// permutations that never permuted anything passes for the wrong reason,
    /// and "agreement would be luck, not a property" is the failure the record
    /// names. See `tests/tier_h_projection_differential.rs`.
    #[doc(hidden)]
    #[must_use]
    pub fn storage_order_probe(&mut self) -> Vec<PersistId> {
        let mut query = self.world.query::<&Identity>();
        query.iter(&self.world).map(|identity| identity.0).collect()
    }

    fn slot_for(&self, entity: PersistId) -> Option<Slot> {
        self.world.resource::<Index>().0.get(&entity).copied()
    }

    /// One entity's canonical state, read from the one component its slot
    /// names.
    ///
    /// The decomposition used to charge a probe here — try [`MigratedSection`],
    /// fall back to [`RemainderSection`] — so a remainder entity paid a miss
    /// before its hit. The [`Slot`] says which component holds it, so this is
    /// one lookup on either side. The index is the authority on that, and
    /// `the_index_and_the_archetypes_agree_on_every_entity` is what holds the
    /// two to each other.
    ///
    /// `TickBackend::state` returns `&R::CoreState`, so the component has to
    /// hold the whole state enum rather than the narrower
    /// `Rock`/`Pickup`/`BloomDirector` the section names — the seam's
    /// contract, not the substrate, is what stops the payload being narrowed.
    /// See the module note for the proof.
    fn state_at(&self, slot: Slot) -> Option<&R::CoreState> {
        match slot.side {
            Side::Migrated => self
                .world
                .get::<MigratedSection<R>>(slot.entity)
                .map(|held| &held.0),
            Side::Remainder => self
                .world
                .get::<RemainderSection<R>>(slot.entity)
                .map(|held| &held.0),
        }
    }

    /// Every entity past the **migration frontier**, in ascending `PersistId` order.
    ///
    /// The point of the decomposition, in one signature. On the `BTreeMap`
    /// store the same question is `entities()` — the whole population — filtered
    /// by a predicate the caller writes over `state()`, which is a scan of every
    /// entity in the world and a discriminant test the store cannot check. Here
    /// the query visits the migrated archetypes and nothing else, and the
    /// predicate is the ruleset's own `Sectioned` declaration rather than a
    /// `matches!` at the call site.
    #[must_use]
    pub fn migrated_population(&mut self) -> Vec<PersistId> {
        let mut query = self
            .world
            .query_filtered::<&Identity, bevy_ecs::prelude::With<MigratedSection<R>>>();
        let mut population: Vec<PersistId> =
            query.iter(&self.world).map(|identity| identity.0).collect();
        population.sort_unstable();
        population
    }
}

impl<R: EcsHostable> TickBackend<R> for EcsBackend<R> {
    fn ruleset(&self) -> &R {
        &self.world.resource::<Rules<R>>().ruleset
    }

    fn insert_observed(&mut self, entity: PersistId, mut state: R::CoreState, observed_tick: Tick) {
        // VC-7 on install, exactly as `Executor::insert_observed` does it: a
        // snapshot loaded from a bundle must sit on the lattice before the
        // first tick reads it.
        state.quantize();
        let side = Side::of(&state);
        let native_state = (side == Side::Migrated).then(|| state.clone());
        let handle = match self.slot_for(entity) {
            Some(existing) => existing.entity,
            None => self
                .world
                .spawn((Identity(entity), ObservedAt(observed_tick)))
                .id(),
        };
        // The index row is written after the side is known, so a replacing
        // install that crosses the frontier updates the side in the same
        // statement that moves the component.
        self.world.resource_mut::<Index>().0.insert(
            entity,
            Slot {
                entity: handle,
                side,
            },
        );
        {
            let mut held = self.world.entity_mut(handle);
            held.insert(ObservedAt(observed_tick));
            // A replacing install may cross the frontier — a replay harness
            // reuses an id, and nothing in the `Ruleset` contract says a stable
            // id keeps its section. The stale component is removed rather than
            // left behind: an entity carrying both would be sealed twice, once
            // per query, and the population would silently double.
            match side {
                Side::Migrated => {
                    held.remove::<RemainderSection<R>>();
                    held.insert(MigratedSection::<R>(state));
                }
                Side::Remainder => {
                    held.remove::<MigratedSection<R>>();
                    held.insert(RemainderSection::<R>(state));
                }
            };
        }
        if let Some(module) = self.world.get_resource::<MigratedModule<R>>().copied() {
            (module.sync)(&mut self.world, handle, native_state.as_ref());
        }
    }

    /// Despawn the entity outright rather than only dropping its index row:
    /// an entity left in the `World` without its index row would still be
    /// sealed by `seal_population` — which queries components, not the index —
    /// and would go on being a neighbour nobody could see.
    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        let slot = self.world.resource_mut::<Index>().0.remove(&entity)?;
        let state = self.state_at(slot).cloned();
        self.world.despawn(slot.entity);
        state
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.state_at(self.slot_for(entity)?)
    }

    /// The frontier decides, and only then is any state read.
    ///
    /// The provided default is `self.state(entity).and_then(S::project)` — it
    /// fetches a whole state and *then* asks whether it was the section the
    /// caller named. A decomposing host already knows the answer to half of
    /// that question from its storage layout: a section past the migration
    /// frontier cannot be held by an entity filed on the remainder side, and a
    /// remainder section cannot be held by one filed past it. Those two cases
    /// are answered here from the [`Slot`] alone, and the entity's canonical
    /// state is never touched.
    ///
    /// What is *not* answered from this generic layout is which concrete
    /// section a migrated entity occupies. Regolith keeps those own-state
    /// projections as concrete game components for its rules, but this read API
    /// returns a borrow tied to the whole-state cache, so the generic accessor
    /// still projects here. See the module note.
    fn section_state<S>(&self, entity: PersistId) -> Option<&S::State>
    where
        S: Section<Root = R::CoreState>,
    {
        let slot = self.slot_for(entity)?;
        let wanted = if R::CoreState::MIGRATED_SECTIONS.contains(&S::SECTION) {
            Side::Migrated
        } else {
            Side::Remainder
        };
        if slot.side != wanted {
            return None;
        }
        self.state_at(slot).and_then(S::project)
    }

    fn entities(&self) -> Vec<PersistId> {
        self.world.resource::<Index>().0.keys().copied().collect()
    }

    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        self.slot_for(entity)?;
        let mut sealed = SealedTickInputs::new();
        sealed.extend(entity, inputs.iter().cloned());
        let mut stepped = self.run_tick(tick, &sealed, Some(entity));
        let position = stepped.iter().position(|step| step.entity == entity)?;
        Some(stepped.swap_remove(position).outcome)
    }

    fn step_tick(
        &mut self,
        tick: Tick,
        inputs: &SealedTickInputs<R::CoreInput>,
    ) -> Vec<SteppedEntity<R::CoreEvent>> {
        self.run_tick(tick, inputs, None)
    }
}

impl<R: EcsHostable> EcsBackend<R> {
    /// Run one tick's schedule, optionally restricted to a single entity.
    fn run_tick(
        &mut self,
        tick: Tick,
        inputs: &SealedTickInputs<R::CoreInput>,
        only: Option<PersistId>,
    ) -> Vec<SteppedEntity<R::CoreEvent>> {
        {
            let mut plan = self.world.resource_mut::<TickPlan<R>>();
            plan.tick = tick;
            plan.only = only;
            plan.inputs = inputs.clone();
        }
        // A single-entity step still *seals* the whole population, because the
        // neighbour view the rule reads has to be the whole population. It
        // advances only the one entity.
        self.schedule.run(&mut self.world);
        let mut results = self.world.resource_mut::<TickResults<R>>();
        let mut stepped = std::mem::take(&mut results.stepped);
        sort_stepped_entities(&mut stepped);
        stepped
    }
}
