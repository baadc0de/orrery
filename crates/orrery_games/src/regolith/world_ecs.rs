//! Native ECS execution for the `regolith.world` module (S7.4, #745).
//!
//! The host's migration frontier selects exactly this module: rock, pickup and
//! bloom-director own state is held as concrete components and the module's
//! named rules run as chained Bevy systems. `regolith.craft` stays on the
//! ordinary `Ruleset::step` path.
//!
//! The host still keeps the whole [`RegolithState`] beside a migrated entity.
//! That cache is required by `TickBackend::state -> &CoreState` and is the only
//! value canonical encoding reads. Before and after every native step this
//! module synchronizes the concrete component with that cache; quantization,
//! recorded-neighbour framing, hashing and materialization remain in
//! `orrery_core::canonical_step_with`.

use bevy_ecs::prelude::{
    Component, Entity, IntoScheduleConfigs, Query, ResMut, Resource, Schedule, With, World,
};
use bevy_ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use orrery_core::{run_system_as, OrderedInputs, StateView, StepOutput, TickRng};
use orrery_protocol::PersistId;

use super::order::{Order, Outcome};
use super::state::{BloomDirector, Pickup, RegolithState, Rock};
use super::{observe_claims, world, Regolith, RegolithLocals, CLAIMS_APPLY};

/// The ruleset-owned schedule run for one migrated entity.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WorldEntityTick;

/// Marks the one entity whose own state this per-entity replay advances.
#[derive(Component)]
struct ActiveStep;

/// One entity-tick's closed inputs and scratch, shared by the chained systems.
#[derive(Resource)]
struct NativeTick {
    entity: PersistId,
    rules: Regolith,
    inputs: Vec<Order>,
    rng: TickRng,
    locals: RegolithLocals,
    events: Vec<Outcome>,
}

/// The built module schedule, kept in the same dedicated world as its state.
#[derive(Resource)]
struct WorldSchedule(Schedule);

macro_rules! projected_adapter {
    ($adapter:ident, $component:ty, $variant:ident, $system:expr) => {
        fn $adapter(
            mut query: Query<&mut $component, With<ActiveStep>>,
            mut tick: ResMut<NativeTick>,
        ) {
            for mut component in &mut query {
                let mut state = RegolithState::$variant(component.clone());
                let NativeTick {
                    entity,
                    rules,
                    inputs,
                    rng,
                    locals,
                    events,
                } = tick.as_mut();
                let ordered = OrderedInputs::new(inputs.as_slice());
                let emitted =
                    run_system_as(*entity, rules, &mut state, &ordered, rng, locals, $system);
                let RegolithState::$variant(updated) = state else {
                    unreachable!("a projected system changed its state section")
                };
                *component = updated;
                events.extend(emitted);
            }
        }
    };
}

projected_adapter!(rock_load, Rock, Rock, &world::RESOLUTION[0]);
projected_adapter!(rock_resolve_orders, Rock, Rock, &world::RESOLUTION[1]);
projected_adapter!(rock_resolve_destruction, Rock, Rock, &world::RESOLUTION[2]);
projected_adapter!(rock_refuse_when_dead, Rock, Rock, &world::RESOLUTION[3]);
projected_adapter!(pickup_expire, Pickup, Pickup, &world::RESOLUTION[4]);
projected_adapter!(pickup_contest, Pickup, Pickup, &world::RESOLUTION[5]);
projected_adapter!(
    bloom_apply_population,
    BloomDirector,
    BloomDirector,
    &world::RESOLUTION[6]
);
projected_adapter!(rock_drift, Rock, Rock, &world::LIFECYCLE[0]);
projected_adapter!(
    bloom_advance_clock,
    BloomDirector,
    BloomDirector,
    &world::LIFECYCLE[1]
);
projected_adapter!(
    bloom_expire_site,
    BloomDirector,
    BloomDirector,
    &world::LIFECYCLE[2]
);
projected_adapter!(
    bloom_seed,
    BloomDirector,
    BloomDirector,
    &world::LIFECYCLE[3]
);

// `claims-apply` is shared by both manifest modules. The craft-only first
// system remains on the remainder path; the two whole-state systems are
// projected over each of this module's concrete component types.
projected_adapter!(rock_propagate_claim_overflow, Rock, Rock, &CLAIMS_APPLY[1]);
projected_adapter!(
    pickup_propagate_claim_overflow,
    Pickup,
    Pickup,
    &CLAIMS_APPLY[1]
);
projected_adapter!(
    bloom_propagate_claim_overflow,
    BloomDirector,
    BloomDirector,
    &CLAIMS_APPLY[1]
);
projected_adapter!(rock_emit_visibility, Rock, Rock, &CLAIMS_APPLY[2]);
projected_adapter!(pickup_emit_visibility, Pickup, Pickup, &CLAIMS_APPLY[2]);
projected_adapter!(
    bloom_emit_visibility,
    BloomDirector,
    BloomDirector,
    &CLAIMS_APPLY[2]
);

#[derive(Clone, Copy)]
enum Ordering {
    Chained,
    Unordered,
}

fn module_schedule(ordering: Ordering) -> Schedule {
    let mut schedule = Schedule::new(WorldEntityTick);
    let systems = (
        rock_load,
        rock_resolve_orders,
        rock_resolve_destruction,
        rock_refuse_when_dead,
        pickup_expire,
        pickup_contest,
        bloom_apply_population,
        rock_drift,
        bloom_advance_clock,
        bloom_expire_site,
        bloom_seed,
        rock_propagate_claim_overflow,
        pickup_propagate_claim_overflow,
        bloom_propagate_claim_overflow,
        rock_emit_visibility,
        pickup_emit_visibility,
        bloom_emit_visibility,
    );
    match ordering {
        Ordering::Chained => schedule.add_systems(systems.chain()),
        Ordering::Unordered => schedule.add_systems(systems),
    };
    schedule
}

fn ensure_schedule(world: &mut World) {
    if !world.contains_resource::<WorldSchedule>() {
        world.insert_resource(WorldSchedule(module_schedule(Ordering::Chained)));
    }
}

fn audit(mut schedule: Schedule) -> Result<(), String> {
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..ScheduleBuildSettings::default()
    });
    schedule
        .initialize(&mut World::new())
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

/// Check that the shipped `regolith.world` schedule composes unambiguously.
pub fn ambiguity_audit() -> Result<(), String> {
    audit(module_schedule(Ordering::Chained))
}

/// Check the ambiguity rejector against the same systems without their chain.
pub fn ambiguity_audit_of_the_unordered_mutant() -> Result<(), String> {
    audit(module_schedule(Ordering::Unordered))
}

/// Synchronize the host's migrated whole-state cache into concrete components.
///
/// `None` means the entity crossed back to the remainder and must shed every
/// `regolith.world` component. This is the native counterpart of the host
/// removing the stale `MigratedSection`/`RemainderSection` component.
pub fn sync_migrated(world: &mut World, entity: Entity, state: Option<&RegolithState>) {
    ensure_schedule(world);
    let mut held = world.entity_mut(entity);
    held.remove::<Rock>();
    held.remove::<Pickup>();
    held.remove::<BloomDirector>();
    held.remove::<ActiveStep>();
    match state {
        Some(RegolithState::Rock(rock)) => {
            held.insert(rock.clone());
        }
        Some(RegolithState::Pickup(pickup)) => {
            held.insert(pickup.clone());
        }
        Some(RegolithState::BloomDirector(director)) => {
            held.insert(director.clone());
        }
        Some(RegolithState::Craft(_)) => {
            unreachable!("the migrated module cannot contain a craft")
        }
        None => {}
    }
}

/// Run one migrated entity through the `regolith.world` ECS schedule.
///
/// The observation remains outside the component queries so every neighbour
/// lookup still passes through `StateView::neighbor`. The Bevy systems receive
/// only the active entity's own component and the closed per-tick resources.
pub fn step_migrated(
    world: &mut World,
    entity: Entity,
    rules: &Regolith,
    view: &mut StateView<'_, RegolithState>,
    inputs: &OrderedInputs<'_, Order>,
    rng: &mut TickRng,
) -> StepOutput<Outcome> {
    let starting = view.own().clone();
    sync_migrated(world, entity, Some(&starting));

    let mut locals = RegolithLocals::default();
    observe_claims(view, inputs, &mut locals);
    world.insert_resource(NativeTick {
        entity: view.entity(),
        rules: *rules,
        inputs: inputs.iter().cloned().collect(),
        rng: rng.clone(),
        locals,
        events: Vec::new(),
    });
    world.entity_mut(entity).insert(ActiveStep);

    let mut schedule = world
        .remove_resource::<WorldSchedule>()
        .expect("the migrated module installed its schedule");
    schedule.0.run(world);
    world.insert_resource(schedule);
    world.entity_mut(entity).remove::<ActiveStep>();

    let tick = world
        .remove_resource::<NativeTick>()
        .expect("the native tick context survives its schedule");
    *rng = tick.rng;
    let stepped = if let Some(rock) = world.get::<Rock>(entity) {
        RegolithState::Rock(rock.clone())
    } else if let Some(pickup) = world.get::<Pickup>(entity) {
        RegolithState::Pickup(pickup.clone())
    } else if let Some(director) = world.get::<BloomDirector>(entity) {
        RegolithState::BloomDirector(director.clone())
    } else {
        unreachable!("a migrated entity has no regolith.world component")
    };
    *view.own_mut() = stepped;

    StepOutput {
        events: tick.events,
    }
}
