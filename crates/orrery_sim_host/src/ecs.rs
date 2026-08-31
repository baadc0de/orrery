//! An ECS storage-and-scheduling substrate for the seam (S7.4, #745).
//!
//! [`EcsBackend`] is a [`TickBackend`] whose canonical state lives in a
//! dedicated `bevy_ecs::World` and whose tick is driven by a `Schedule` of
//! named stages, rather than in `Executor`'s `BTreeMap` and a `for` loop.
//! [`SimulationHost::on_backend`](crate::SimulationHost::on_backend) accepts
//! it wherever it accepts the executor.
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

use std::collections::BTreeMap;
use std::marker::PhantomData;

use bevy_ecs::prelude::{
    Component, Entity, IntoScheduleConfigs, Query, Res, ResMut, Resource, Schedule, World,
};
use bevy_ecs::schedule::ScheduleLabel;
use orrery_core::{
    canonical_step, CanonicalOutcome, CanonicalStep, NeighborSnapshot, Quantized, Ruleset,
    SealedTickInputs, SteppedEntity, TickBackend, TickOutcome,
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
    CoreState: Send + Sync + 'static,
    CoreInput: Send + Sync + 'static,
    CoreEvent: Send + Sync + 'static,
>
{
}

impl<R> EcsHostable for R
where
    R: Ruleset,
    R::CoreState: Send + Sync + 'static,
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

/// One entity's canonical core state, as a component. The store of record.
#[derive(Component, Debug, Clone)]
struct Canonical<R: EcsHostable>(R::CoreState);

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
}

/// The live neighbour view, maintained across the advance stage.
///
/// This is the substrate's least comfortable object and the honest place to
/// say why it exists: `StateView` — frozen, and rightly so — reads neighbours
/// out of a `&BTreeMap<PersistId, S>`. An ECS that stores state in components
/// therefore has to reconstitute the very map it replaced before it can call a
/// rule. See the crate report for what that costs a rules author.
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
/// observation tick. Archetype iteration order is not `PersistId` order, so
/// the population is sorted here: the order entities step in is canonical
/// (a neighbour read at tick T sees whatever an earlier-stepped neighbour
/// wrote at T), and leaving it to the archetype layout would be exactly the
/// unordered-iteration hazard VC-4 exists for.
fn seal_population<R: EcsHostable>(
    population: Query<(Entity, &Identity, &Canonical<R>, &ObservedAt)>,
    mut plan: ResMut<TickPlan<R>>,
    mut neighborhood: ResMut<Neighborhood<R>>,
) {
    let mut order = Vec::new();
    neighborhood.states.clear();
    neighborhood.observed.clear();
    for (entity, identity, canonical, observed) in &population {
        order.push(StepSlot {
            persist: identity.0,
            entity,
        });
        neighborhood.states.insert(identity.0, canonical.0.clone());
        neighborhood.observed.insert(identity.0, observed.0);
    }
    order.sort_by_key(|slot| slot.persist);
    plan.order = order;
}

/// Stage 2 — `host.advance-population`.
///
/// Runs the canonical stage for each sealed entity, in order, writing the
/// result back into its components. The neighbour view is kept live as it
/// goes, because an entity stepped later in a tick can observe an
/// earlier-stepped neighbour's new state — that is the executor's behaviour
/// and therefore the canonical one.
fn advance_population<R: EcsHostable>(
    rules: Res<Rules<R>>,
    plan: Res<TickPlan<R>>,
    index: Res<Index>,
    mut neighborhood: ResMut<Neighborhood<R>>,
    mut results: ResMut<TickResults<R>>,
    mut states: Query<(&mut Canonical<R>, &mut ObservedAt)>,
) {
    let tick = plan.tick;
    let settled = Tick::new(tick.0.saturating_add(1));
    results.stepped.clear();
    results.spawned.clear();

    for slot in &plan.order {
        if plan.only.is_some_and(|only| only != slot.persist) {
            continue;
        }
        // Own state leaves the neighbour view for the duration of its own
        // step, exactly as it leaves the executor's map.
        let Some(mut own) = neighborhood.states.remove(&slot.persist) else {
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
            &mut own,
            NeighborSnapshot {
                states: &neighborhood.states,
                observed_ticks: &neighborhood.observed,
            },
        );

        // First writer wins, against the population and against anything an
        // earlier entity in this same tick already materialized.
        let mut installed = Vec::with_capacity(materializations.len());
        for description in materializations {
            if index.0.contains_key(&description.entity)
                || neighborhood.states.contains_key(&description.entity)
            {
                continue;
            }
            let mut state = description.state;
            state.quantize();
            neighborhood
                .states
                .insert(description.entity, state.clone());
            neighborhood.observed.insert(description.entity, settled);
            installed.push(description.entity);
            results.spawned.push(Materialized {
                entity: description.entity,
                state,
            });
        }
        outcome.materialized = installed;

        // The component is the store of record: write the stepped state back
        // to it, and mirror it into the live neighbour view.
        let (mut canonical, mut observed) = states
            .get_mut(slot.entity)
            .expect("a sealed entity still holds its canonical components");
        canonical.0 = own.clone();
        observed.0 = settled;
        neighborhood.states.insert(slot.persist, own);
        neighborhood.observed.insert(slot.persist, settled);

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
        let entity = world
            .spawn((
                Identity(materialized.entity),
                Canonical::<R>(materialized.state),
                ObservedAt(settled),
            ))
            .id();
        world
            .resource_mut::<Index>()
            .0
            .insert(materialized.entity, entity);
    }
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

        let mut schedule = Schedule::new(CanonicalTick);
        // Explicitly chained: the canonical schedule declares no ambiguity, so
        // neither may this one. `bevy_ecs` is built here without
        // `multi_threaded`, so single-threaded execution is a property of the
        // build and not a request this schedule makes.
        schedule.add_systems(
            (
                seal_population::<R>,
                advance_population::<R>,
                install_materializations::<R>,
            )
                .chain(),
        );
        // The declared stage list is not decoration: a fourth system added to
        // the chain without a row in `CANONICAL_TICK_STAGES` is a stage the
        // published order does not mention, and that is how a schedule starts
        // running something nobody reviewed.
        assert_eq!(
            schedule.systems_len(),
            CANONICAL_TICK_STAGES.len(),
            "the canonical tick schedule runs a different number of stages than it declares"
        );
        Self {
            world,
            schedule,
            ruleset: PhantomData,
        }
    }

    fn entity_for(&self, entity: PersistId) -> Option<Entity> {
        self.world.resource::<Index>().0.get(&entity).copied()
    }
}

impl<R: EcsHostable> TickBackend<R> for EcsBackend<R> {
    fn ruleset(&self) -> &R {
        &self.world.resource::<Rules<R>>().ruleset
    }

    fn insert(&mut self, entity: PersistId, mut state: R::CoreState) {
        // VC-7 on install, exactly as `Executor::insert` does it: a snapshot
        // loaded from a bundle must sit on the lattice before the first tick
        // reads it.
        state.quantize();
        match self.entity_for(entity) {
            Some(existing) => {
                self.world
                    .entity_mut(existing)
                    .insert((Canonical::<R>(state), ObservedAt(Tick::new(0))));
            }
            None => {
                let spawned = self
                    .world
                    .spawn((
                        Identity(entity),
                        Canonical::<R>(state),
                        ObservedAt(Tick::new(0)),
                    ))
                    .id();
                self.world.resource_mut::<Index>().0.insert(entity, spawned);
            }
        }
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        let handle = self.entity_for(entity)?;
        self.world.get::<Canonical<R>>(handle).map(|held| &held.0)
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
        std::mem::take(&mut results.stepped)
    }
}
