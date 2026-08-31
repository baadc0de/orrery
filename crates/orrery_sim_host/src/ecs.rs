//! An ECS storage-and-scheduling substrate for the seam (S7.4, #745).
//!
//! [`EcsBackend`] is a [`TickBackend`] whose canonical state lives in a
//! dedicated `bevy_ecs::World` and whose tick is driven by a `Schedule` of
//! named stages, rather than in `Executor`'s `BTreeMap` and a `for` loop.
//! [`SimulationHost::on_backend`](crate::SimulationHost::on_backend) accepts
//! it wherever it accepts the executor.
//!
//! # The unit of migration: one module's state sections
//!
//! Canonical state is not one component. Entities whose declared state section
//! is past the ruleset's migration frontier
//! ([`orrery_core::Sectioned::MIGRATED_SECTIONS`]) are stored in
//! [`MigratedSection`]; everything else is stored in [`RemainderSection`].
//! Those are two Rust types and therefore two `bevy_ecs` components, so the
//! migrated module's population is a set of archetypes and a query for it
//! visits no other entity.
//!
//! Regolith's frontier is `regolith.world` — the `rock`, `pickup` and
//! `bloom-director` sections of #737's split. `regolith.craft` is the
//! remainder. Moving the next module across is a one-line edit to
//! `MIGRATED_SECTIONS` and a field on [`SectionStore`]; no stage signature
//! changes, and the whole blast radius is measured by
//! `tests/ecs_differential.rs`.
//!
//! **What this does not buy, stated plainly.** [`TickBackend::state`] returns
//! `&R::CoreState`, so the migrated component holds the whole state enum and
//! not the narrower `Rock`/`Pickup`/`BloomDirector` its section names. The
//! decomposition separates *storage*, not types. Narrowing the payload would
//! need the seam's contract to change, and that is not a thing S7.4 is allowed
//! to do.
//!
//! # Why this is legal where the previous attempt was not
//!
//! ADR-0042 clause (d) admits a dedicated `bevy_ecs::World` as "a legal future
//! host implementation … behind the seam", and refuses `bevy_ecs` inside a
//! gated crate. The seam is *this* crate, which `core-gates.sh` clause 1 does
//! not gate: its only `impl Ruleset` is under `#[cfg(test)]`, which the gate's
//! role discovery strips before deciding. So this file needs no ADR amendment
//! and no gate change, and `./scripts/core-gates.sh` still exits 0 with
//! `bevy_ecs` a first-class dependency here.
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
//! [`orrery_core::canonical_step`], which both backends call and neither
//! copies. This file owns *where the state was before the call and where it
//! goes after*, plus the order the calls happen in. That is the entire claim
//! being made, and it is a structural one: there is no expression in this file
//! that can compute a hash.

use std::collections::{btree_map, BTreeMap};
use std::marker::PhantomData;

use bevy_ecs::prelude::{
    Component, Entity, IntoScheduleConfigs, Query, Res, ResMut, Resource, Schedule, World,
};
use bevy_ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use orrery_core::{
    canonical_step, CanonicalOutcome, CanonicalStep, NeighborSnapshot, Quantized, Ruleset,
    SealedTickInputs, Sectioned, SteppedEntity, TickBackend, TickOutcome,
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
/// migration frontier** — for Regolith, the `regolith.world` module's `rock`,
/// `pickup` and `bloom-director` sections (S7.4, #745).
///
/// This is a different Rust type from [`RemainderSection`] and therefore a
/// different `bevy_ecs` component, which is the whole of the decomposition:
/// the migrated module's entities live in their own archetype, and a query for
/// them visits no other entity's memory and needs no discriminant test. The
/// *payload* is still the whole `R::CoreState` and cannot be narrowed here —
/// see the module note on `TickBackend::state`.
#[derive(Component, Debug, Clone)]
struct MigratedSection<R: EcsHostable>(R::CoreState);

/// One entity's canonical core state when its section has **not** been
/// migrated. The undivided remainder: for Regolith, `regolith.craft`.
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

/// `PersistId` → `Entity`. The ECS's own identifier is not canonical, so the
/// substrate must carry this index; it is also what gives [`TickBackend::entities`]
/// its ascending-`PersistId` order, which archetype iteration does not have.
#[derive(Resource, Debug, Default)]
struct Index(BTreeMap<PersistId, Entity>);

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
/// inherited from the iteration order. Materialization first-writer-wins
/// remains the one ordering effect still decided by execution order.
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

/// Canonical state on both sides of the migration frontier, as one system
/// param.
///
/// The decomposition's second ergonomic effect, after the archetype: a system
/// that needs to advance an entity asks for [`SectionStore`] and does not have
/// to know that "canonical state" is two components today and will be three
/// when the next module crosses. Migrating a further module adds a field here
/// and an arm to [`SectionStore::own_mut`]; no stage signature changes.
#[derive(bevy_ecs::system::SystemParam)]
struct SectionStore<'w, 's, R: EcsHostable> {
    migrated: Query<'w, 's, &'static mut MigratedSection<R>>,
    remainder: Query<'w, 's, &'static mut RemainderSection<R>>,
    observed_at: Query<'w, 's, &'static mut ObservedAt>,
}

impl<R: EcsHostable> SectionStore<'_, '_, R> {
    /// One sealed entity's canonical state, from the side its slot names.
    fn own_mut(&mut self, slot: &StepSlot) -> Option<&mut R::CoreState> {
        match slot.side {
            Side::Migrated => self
                .migrated
                .get_mut(slot.entity)
                .ok()
                .map(|held| &mut held.into_inner().0),
            Side::Remainder => self
                .remainder
                .get_mut(slot.entity)
                .ok()
                .map(|held| &mut held.into_inner().0),
        }
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
fn advance_population<R: EcsHostable>(
    rules: Res<Rules<R>>,
    plan: Res<TickPlan<R>>,
    index: Res<Index>,
    neighborhood: Res<Neighborhood<R>>,
    mut results: ResMut<TickResults<R>>,
    mut store: SectionStore<R>,
) {
    let tick = plan.tick;
    let settled = Tick::new(tick.0.saturating_add(1));
    results.stepped.clear();
    results.spawned.clear();

    for slot in &plan.order {
        if plan.only.is_some_and(|only| only != slot.persist) {
            continue;
        }
        // Own state is mutated in place in its component, and stays in the
        // sealed view for everyone else to read at its tick-start value. It is
        // hidden from *this* step by identity, inside `StateView`, not by
        // taking the row out of a map that other steps also read.
        // The migrated module's state comes out of its own component, and the
        // slot says which. Both arms yield `&mut R::CoreState` because the seam
        // hands the host an opaque state type; what the decomposition buys is
        // the archetype, not a narrower payload.
        let Some(own) = store.own_mut(slot) else {
            continue;
        };
        let CanonicalOutcome {
            mut outcome,
            materializations,
        } = canonical_step(
            CanonicalStep {
                ruleset: &rules.ruleset,
                seed: rules.seed,
                entity: slot.persist,
                tick,
                inputs: plan.inputs.for_entity(slot.persist),
            },
            own,
            NeighborSnapshot {
                states: &neighborhood.states,
                observed_ticks: &neighborhood.observed,
            },
        );
        // The write has already happened, in the component; the stamp goes
        // beside it rather than at the end of the tick, so a T+1 stamp is
        // never paired with a pre-step state.
        if let Ok(mut observed) = store.observed_at.get_mut(slot.entity) {
            observed.0 = settled;
        }

        // First writer wins, against the population and against anything an
        // earlier entity in this same tick already materialized. The pending
        // spawns are the second half of that test now that the neighbour view
        // is read-only: `Index` covers everything alive at the tick boundary,
        // `results.spawned` covers everything claimed since.
        let mut installed = Vec::with_capacity(materializations.len());
        for description in materializations {
            if index.0.contains_key(&description.entity)
                || results
                    .spawned
                    .iter()
                    .any(|pending| pending.entity == description.entity)
            {
                continue;
            }
            let mut state = description.state;
            state.quantize();
            installed.push(description.entity);
            // Deliberately not added to the sealed view: an entity born
            // during tick T had no state at the start of T, so no
            // later-stepping entity can have observed one. It becomes a
            // neighbour on T+1, the tick it starts stepping on.
            results.spawned.push(Materialized {
                entity: description.entity,
                state,
            });
        }
        outcome.materialized = installed;

        results.stepped.push(SteppedEntity {
            entity: slot.persist,
            outcome,
        });
    }
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
        // by a split lands directly in the migrated module's archetype and is
        // never briefly a member of the remainder. This is the only place in the
        // substrate where a spawn's archetype is chosen, and it is why
        // `regolith.world` — the module whose population changes mid-tick — is
        // the one migrated first.
        let mut spawn = world.spawn((Identity(materialized.entity), ObservedAt(settled)));
        match Side::of(&materialized.state) {
            Side::Migrated => spawn.insert(MigratedSection::<R>(materialized.state)),
            Side::Remainder => spawn.insert(RemainderSection::<R>(materialized.state)),
        };
        let entity = spawn.id();
        world
            .resource_mut::<Index>()
            .0
            .insert(materialized.entity, entity);
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

    fn entity_for(&self, entity: PersistId) -> Option<Entity> {
        self.world.resource::<Index>().0.get(&entity).copied()
    }

    /// One entity's canonical state, from whichever side of the frontier holds
    /// it.
    ///
    /// The two-arm lookup is the decomposition's cost, and it is charged here
    /// exactly once. `TickBackend::state` returns `&R::CoreState`, so the
    /// migrated component has to hold the whole state enum rather than the
    /// narrower `Rock`/`Pickup`/`BloomDirector` the section names — the seam's
    /// contract, not the substrate, is what stops the payload being narrowed.
    fn state_at(&self, handle: Entity) -> Option<&R::CoreState> {
        match self.world.get::<MigratedSection<R>>(handle) {
            Some(held) => Some(&held.0),
            None => self
                .world
                .get::<RemainderSection<R>>(handle)
                .map(|held| &held.0),
        }
    }

    /// Every entity of the **migrated module**, in ascending `PersistId` order.
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
        let handle = match self.entity_for(entity) {
            Some(existing) => existing,
            None => {
                let spawned = self
                    .world
                    .spawn((Identity(entity), ObservedAt(observed_tick)))
                    .id();
                self.world.resource_mut::<Index>().0.insert(entity, spawned);
                spawned
            }
        };
        let mut held = self.world.entity_mut(handle);
        held.insert(ObservedAt(observed_tick));
        // A replacing install may cross the frontier — a replay harness reuses
        // an id, and nothing in the `Ruleset` contract says a stable id keeps
        // its section. The stale component is removed rather than left behind:
        // an entity carrying both would be sealed twice, once per query, and
        // the population would silently double.
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

    /// Despawn the entity outright rather than only dropping its index row:
    /// an entity left in the `World` without its index row would still be
    /// sealed by `seal_population` — which queries components, not the index —
    /// and would go on being a neighbour nobody could see.
    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        let handle = self.world.resource_mut::<Index>().0.remove(&entity)?;
        let state = self.state_at(handle).cloned();
        self.world.despawn(handle);
        state
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.state_at(self.entity_for(entity)?)
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
        self.entity_for(entity)?;
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
        stepped.sort_by_key(|entry| entry.entity);
        stepped
    }
}
