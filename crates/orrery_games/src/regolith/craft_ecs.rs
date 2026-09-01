//! Native ECS execution for the `regolith.craft` module (S7.4, #745).
//!
//! `Craft` own state is held as a concrete component and the module's named
//! rules run as a chained Bevy schedule. Observation remains outside the
//! component queries, and the host still enters through
//! `orrery_core::canonical_step_with`; RNG derivation, recorded-neighbour
//! framing, quantization, hashing and materialization attribution never enter
//! this module.

use bevy_ecs::prelude::{
    Component, Entity, IntoScheduleConfigs, Query, ResMut, Resource, Schedule, With, World,
};
use bevy_ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use orrery_core::{run_system_as, OrderedInputs, StateView, StepOutput, TickRng};
use orrery_protocol::PersistId;

use super::order::{Order, Outcome};
use super::state::{Craft, RegolithState};
use super::{craft, observe_claims, Regolith, RegolithLocals, CLAIMS_APPLY};

/// The ruleset-owned schedule run for one craft entity.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CraftEntityTick;

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
struct CraftSchedule(Schedule);

macro_rules! craft_adapter {
    ($adapter:ident, $system:expr) => {
        fn $adapter(mut query: Query<&mut Craft, With<ActiveStep>>, mut tick: ResMut<NativeTick>) {
            for mut craft in &mut query {
                let mut state = RegolithState::Craft(craft.clone());
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
                let RegolithState::Craft(updated) = state else {
                    unreachable!("a craft system changed its state section")
                };
                *craft = updated;
                events.extend(emitted);
            }
        }
    };
}

craft_adapter!(craft_tick_cooldowns, &craft::CONTROL[0]);
craft_adapter!(craft_decay_lock, &craft::CONTROL[1]);
craft_adapter!(craft_load_kinematics, &craft::CONTROL[2]);
craft_adapter!(craft_apply_orders, &craft::CONTROL[3]);
craft_adapter!(craft_resolve_lock_reply, &craft::CONTROL[4]);
craft_adapter!(craft_clamp_speed, &craft::MOTION[0]);
craft_adapter!(craft_apply_drag, &craft::MOTION[1]);
craft_adapter!(craft_integrate, &craft::MOTION[2]);
craft_adapter!(craft_respawn, &craft::MOTION[3]);
craft_adapter!(craft_store_kinematics, &craft::MOTION[4]);
craft_adapter!(craft_advance_trail, &craft::MOTION[5]);
craft_adapter!(craft_apply_cover_claim, &CLAIMS_APPLY[0]);
craft_adapter!(craft_propagate_claim_overflow, &CLAIMS_APPLY[1]);
craft_adapter!(craft_emit_visibility, &CLAIMS_APPLY[2]);

#[derive(Clone, Copy)]
enum Ordering {
    Chained,
    Unordered,
}

fn module_schedule(ordering: Ordering) -> Schedule {
    let mut schedule = Schedule::new(CraftEntityTick);
    let systems = (
        craft_tick_cooldowns,
        craft_decay_lock,
        craft_load_kinematics,
        craft_apply_orders,
        craft_resolve_lock_reply,
        craft_clamp_speed,
        craft_apply_drag,
        craft_integrate,
        craft_respawn,
        craft_store_kinematics,
        craft_advance_trail,
        craft_apply_cover_claim,
        craft_propagate_claim_overflow,
        craft_emit_visibility,
    );
    match ordering {
        Ordering::Chained => schedule.add_systems(systems.chain()),
        Ordering::Unordered => schedule.add_systems(systems),
    };
    schedule
}

fn ensure_schedule(world: &mut World) {
    if !world.contains_resource::<CraftSchedule>() {
        world.insert_resource(CraftSchedule(module_schedule(Ordering::Chained)));
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

/// Check that the shipped `regolith.craft` schedule composes unambiguously.
pub fn ambiguity_audit() -> Result<(), String> {
    audit(module_schedule(Ordering::Chained))
}

/// Check the ambiguity rejector against the same systems without their chain.
pub fn ambiguity_audit_of_the_unordered_mutant() -> Result<(), String> {
    audit(module_schedule(Ordering::Unordered))
}

/// Synchronize the whole-state cache into the concrete `Craft` component.
///
/// A non-craft state means the entity belongs to another migrated module, so
/// any stale craft component is removed.
pub fn sync_migrated(world: &mut World, entity: Entity, state: Option<&RegolithState>) {
    ensure_schedule(world);
    let mut held = world.entity_mut(entity);
    held.remove::<Craft>();
    held.remove::<ActiveStep>();
    if let Some(RegolithState::Craft(craft)) = state {
        held.insert(craft.clone());
    }
}

/// Run one craft entity through the `regolith.craft` ECS schedule.
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
        .remove_resource::<CraftSchedule>()
        .expect("the craft module installed its schedule");
    schedule.0.run(world);
    world.insert_resource(schedule);
    world.entity_mut(entity).remove::<ActiveStep>();

    let tick = world
        .remove_resource::<NativeTick>()
        .expect("the native tick context survives its schedule");
    *rng = tick.rng;
    let craft = world
        .get::<Craft>(entity)
        .expect("a migrated craft entity has a Craft component")
        .clone();
    *view.own_mut() = RegolithState::Craft(craft);

    StepOutput {
        events: tick.events,
    }
}
