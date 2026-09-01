//! S7.4: Regolith's migrated modules, stored in their own components (#745).
//!
//! `tests/ecs_differential.rs` proves the ECS-backed host *behaves* identically
//! to the store. This file proves the thing that differential cannot see: that
//! the migrated modules are genuinely stored past the frontier, that the
//! substrate — not the caller — knows which entities belong there, and that the
//! hazards a decomposition introduces are actually closed.
//!
//! Every assertion here is about storage. None of it may move a canonical byte,
//! and the differential is what says it did not.

use orrery_core::Section;
use orrery_core::{SealedTickInputs, Sectioned, TickBackend};
use orrery_games::regolith::state::{
    BloomDirectorSection, CraftSection, PickupSection, RegolithState, RockSection, SECTION_CRAFT,
    SECTION_PICKUP, SECTION_ROCK,
};
use orrery_games::regolith::Regolith;
use orrery_games::scenario::WORLD_SCENARIO;
use orrery_games::Game;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;

fn seed() -> UniverseSeed {
    UniverseSeed([WORLD_SCENARIO.seed_byte; 32])
}

fn regolith_ecs(game: Regolith, seed: UniverseSeed) -> EcsBackend<Regolith> {
    EcsBackend::new(game, seed).with_migrated_module(
        orrery_games::regolith::native_ecs::sync_migrated,
        orrery_games::regolith::native_ecs::step_migrated,
    )
}

/// The `world` scenario's population: four crafts, then eight world seeds.
fn seeded_world() -> EcsBackend<Regolith> {
    let game = Regolith::honest();
    let mut backend = regolith_ecs(game, seed());
    for slot in 0..WORLD_SCENARIO.entities {
        let entity = PersistId::new(slot + 1);
        TickBackend::insert(&mut backend, entity, game.spawn(entity, slot));
    }
    for slot in 0..WORLD_SCENARIO.world_entities {
        let entity = PersistId::new(WORLD_SCENARIO.entities + slot + 1);
        let state = game
            .spawn_world(entity, slot)
            .expect("Regolith seeds every world slot");
        TickBackend::insert(&mut backend, entity, state);
    }
    backend
}

/// Which entities the *ruleset's own declaration* puts past the frontier,
/// computed the way a caller of the `BTreeMap` store has to compute it: scan
/// the whole population and ask each state what it is.
fn declared_migrated(backend: &EcsBackend<Regolith>) -> Vec<PersistId> {
    TickBackend::entities(backend)
        .into_iter()
        .filter(|entity| {
            TickBackend::state(backend, *entity).is_some_and(orrery_core::Sectioned::is_migrated)
        })
        .collect()
}

/// The migrated modules' population comes out of the storage layout, and it
/// agrees with the ruleset's declaration.
///
/// Lane two intentionally advances every Regolith section past the frontier,
/// so the expected population is the entire seeded store. The section counts
/// are the anti-vacuity half: both declared modules and multiple world-owned
/// variants must actually be present.
#[test]
fn both_migrated_modules_are_selected_by_the_storage_layout() {
    let mut backend = seeded_world();
    let population = TickBackend::entities(&backend);
    let migrated = backend.migrated_population();

    assert_eq!(
        migrated,
        declared_migrated(&backend),
        "the migrated archetypes' population disagrees with RegolithState::is_migrated"
    );
    assert!(
        !migrated.is_empty(),
        "no entity is past the migration frontier, so nothing above was compared"
    );
    assert_eq!(
        migrated, population,
        "lane two moved every declared Regolith module, so no Regolith entity \
         may remain on the generic remainder side"
    );
    assert_eq!(
        migrated.len(),
        (WORLD_SCENARIO.entities + WORLD_SCENARIO.world_entities) as usize,
        "the migrated population must contain both crafts and world seeds"
    );
    // A section is a *set* of state variants, not a synonym for one enum arm:
    // `regolith.world` owns `rock`, `pickup` and `bloom-director`, and the
    // world seeds carry at least two of them. A frontier that happened to line
    // up one-to-one with a variant would not have shown that.
    let mut sections: Vec<_> = migrated
        .iter()
        .map(|entity| {
            TickBackend::state(&backend, *entity)
                .expect("a migrated entity has state")
                .section()
        })
        .collect();
    for section in &sections {
        assert!(
            RegolithState::MIGRATED_SECTIONS.contains(section),
            "{section:?} is in the migrated archetype but not past the frontier"
        );
    }
    assert!(sections.contains(&SECTION_CRAFT));
    assert!(sections.contains(&SECTION_ROCK));
    sections.sort_unstable();
    sections.dedup();
    assert!(
        sections.len() > 2,
        "the seeded population did not exercise both modules and more than one \
         world-owned section"
    );
}

/// Every entity the tick *materialized* was placed in the migrated module's
/// archetype by `host.install-materializations`.
///
/// This is the clause that made `regolith.world` the module to migrate first.
/// A spawn is the only moment the substrate chooses an archetype from a state
/// value it has never seen before, and `regolith.craft` — whose population is
/// fixed at seeding — would never have reached it.
#[test]
fn a_materialized_entity_is_born_into_the_migrated_archetype() {
    let mut backend = seeded_world();
    let before = backend.migrated_population().len();
    let empty = SealedTickInputs::new();
    for tick in 0..WORLD_SCENARIO.ticks {
        backend.step_tick(Tick::new(tick), &empty);
    }
    let after = backend.migrated_population();

    assert!(
        after.len() > before,
        "no entity was materialized in {} ticks, so this test asserts nothing \
         about the spawn path",
        WORLD_SCENARIO.ticks
    );
    assert_eq!(
        after,
        declared_migrated(&backend),
        "an entity materialized mid-tick was placed on the wrong side of the \
         migration frontier"
    );
}

/// An install that crosses native modules replaces the entity; it does not
/// duplicate or double-step it.
///
/// The hazard is specific to the decomposition and invisible to the
/// differential over the corpus, because the corpus never reuses an id across
/// sections. Both sections are now past the generic frontier, but they have
/// different concrete game components and different module schedules. A stale
/// component or duplicate host entity would make one stable id step twice.
#[test]
fn an_install_across_native_modules_replaces_the_entity_rather_than_duplicating_it() {
    let game = Regolith::honest();
    let mut backend = regolith_ecs(game, seed());
    let entity = PersistId::new(1);

    TickBackend::insert(&mut backend, entity, game.spawn(entity, 0));
    assert_eq!(
        TickBackend::state(&backend, entity).map(RegolithState::section),
        Some(SECTION_CRAFT),
    );
    assert_eq!(
        backend.migrated_population(),
        vec![entity],
        "craft is now in the migrated craft module"
    );

    let rock = game
        .spawn_world(entity, 0)
        .expect("Regolith seeds every world slot");
    let world_section = rock.section();
    TickBackend::insert(&mut backend, entity, rock);

    assert_eq!(
        backend.migrated_population(),
        vec![entity],
        "the replacing install lost the entity from the migrated frontier"
    );
    assert_eq!(
        TickBackend::entities(&backend),
        vec![entity],
        "the replacing install spawned a second world entity"
    );
    let stepped = backend.step_tick(Tick::new(0), &SealedTickInputs::new());
    assert_eq!(
        stepped.len(),
        1,
        "the entity was duplicated or selected by both native module schedules"
    );
    assert_eq!(
        TickBackend::state(&backend, entity).map(RegolithState::section),
        Some(world_section),
        "the craft-to-world replacement ran the wrong native module"
    );
}

// ── the section accessor, answered from the storage layout (#791, #793) ──
//
// `TickBackend::section_state` has a provided default —
// `self.state(entity).and_then(S::project)` — which fetches a whole state and
// only then asks whether it was the section the caller named. This backend
// overrides it, because its storage already answers half of that question:
// every declared Regolith section is now filed past the frontier.
//
// These two tests are the pair the override needs. The first pins the answers
// themselves — the `None` half over the full cross product, not just the
// `Some` half. The second pins the invariant the override *rests* on: that the
// index's recorded side and the component an entity actually carries never
// drift apart. `state_at` reads exactly one component now, chosen by the index,
// so a drifted row is an entity whose canonical state silently disappears.

/// Every section, over every entity past the frontier.
#[test]
fn the_section_accessor_answers_every_section_over_every_entity() {
    let mut backend = seeded_world();
    let empty = SealedTickInputs::new();
    // Step far enough to materialize: a spawn is the only place the substrate
    // picks a side for a state value it has never seen, and the override reads
    // the side that path recorded.
    for tick in 0..WORLD_SCENARIO.ticks {
        backend.step_tick(Tick::new(tick), &empty);
    }

    let mut seen = [0_usize; 4];
    for entity in TickBackend::entities(&backend) {
        let whole = TickBackend::state(&backend, entity)
            .expect("every indexed entity's state is reachable through its slot")
            .clone();
        // One of the four is `Some` and exactly three are `None`, for every
        // entity — which is the exactness law of `Section` read through the
        // backend rather than through a value.
        let craft = TickBackend::section_state::<CraftSection>(&backend, entity).is_some();
        let rock = TickBackend::section_state::<RockSection>(&backend, entity).is_some();
        let pickup = TickBackend::section_state::<PickupSection>(&backend, entity).is_some();
        let director =
            TickBackend::section_state::<BloomDirectorSection>(&backend, entity).is_some();
        let answers = [craft, rock, pickup, director];
        assert_eq!(
            answers.iter().filter(|held| **held).count(),
            1,
            "{entity:?} in section {:?} answered {answers:?} across the four \
             declared sections; exactly one must be Some",
            whole.section(),
        );
        let expected = match whole.section() {
            SECTION_CRAFT => 0,
            SECTION_ROCK => 1,
            SECTION_PICKUP => 2,
            _ => 3,
        };
        assert!(
            answers[expected],
            "{entity:?} declares section {:?} but that section's accessor said None",
            whole.section(),
        );
        seen[expected] += 1;
        // The accessor and the default must agree. The override is an
        // optimisation over the storage layout, never a different answer.
        assert_eq!(
            TickBackend::section_state::<CraftSection>(&backend, entity),
            TickBackend::state(&backend, entity).and_then(CraftSection::project),
            "the override disagrees with `state().and_then(project)` for {entity:?}",
        );
    }
    // Anti-vacuity: a population that exercised only one module would pass
    // every assertion above while never asking the accessor about the other.
    assert!(
        seen[0] > 0,
        "no craft: the second migrated module was never asked"
    );
    assert!(
        seen[1] > 0,
        "no rock: the first migrated module was never asked"
    );
    assert!(
        seen[1] + seen[2] + seen[3] > seen[1],
        "the migrated module contributed only rocks, so the accessor was never \
         asked to tell two migrated sections apart"
    );
}

/// The index's recorded side and the entity's actual component agree, for
/// every entity, across installs, module replacements and materializations.
///
/// This is the invariant `state_at` now trusts: it reads the one component the
/// slot names instead of probing both. If a row drifted, `state()` would answer
/// `None` for an entity the index still lists — so the check is that every
/// indexed entity has reachable state, and that the archetype-driven
/// `migrated_population` still agrees with the state-driven declaration.
#[test]
fn the_index_and_the_archetypes_agree_on_every_entity() {
    let game = Regolith::honest();
    let mut backend = seeded_world();
    let empty = SealedTickInputs::new();
    for tick in 0..WORLD_SCENARIO.ticks {
        backend.step_tick(Tick::new(tick), &empty);
    }
    // Replacing installs across native modules in both directions exercise the
    // concrete-component synchronizers without changing the generic side.
    let crossing = PersistId::new(1);
    let rock = game
        .spawn_world(crossing, 0)
        .expect("Regolith seeds every world slot");
    TickBackend::insert(&mut backend, crossing, rock);
    TickBackend::insert(&mut backend, crossing, game.spawn(crossing, 0));

    let population = TickBackend::entities(&backend);
    assert!(!population.is_empty());
    for entity in &population {
        assert!(
            TickBackend::state(&backend, *entity).is_some(),
            "{entity:?} is indexed but its slot names a component it does not \
             carry: the recorded side and the archetype have drifted"
        );
    }
    assert_eq!(
        backend.migrated_population(),
        declared_migrated(&backend),
        "the archetype-driven migrated population disagrees with the \
         state-driven declaration"
    );
    assert_eq!(
        TickBackend::state(&backend, crossing).map(RegolithState::section),
        Some(SECTION_CRAFT),
        "the second module replacement did not restore craft state"
    );
    assert!(
        backend.migrated_population().contains(&crossing),
        "the craft module is past the migration frontier but its entity was lost"
    );
}
