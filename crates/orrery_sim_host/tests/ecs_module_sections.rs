//! S7.4: the `regolith.world` module, stored in its own components (#745).
//!
//! `tests/ecs_differential.rs` proves the ECS-backed host *behaves* identically
//! to the store. This file proves the thing that differential cannot see: that
//! the migrated module is genuinely stored apart, that the substrate — not the
//! caller — is what knows which entities belong to it, and that the two
//! hazards a decomposition introduces are actually closed.
//!
//! Every assertion here is about storage. None of it may move a canonical byte,
//! and the differential is what says it did not.

use orrery_core::{SealedTickInputs, Sectioned, TickBackend};
use orrery_games::regolith::state::{RegolithState, SECTION_CRAFT, SECTION_ROCK};
use orrery_games::regolith::Regolith;
use orrery_games::scenario::WORLD_SCENARIO;
use orrery_games::Game;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;

fn seed() -> UniverseSeed {
    UniverseSeed([WORLD_SCENARIO.seed_byte; 32])
}

/// The `world` scenario's population: four crafts, then eight world seeds.
fn seeded_world() -> EcsBackend<Regolith> {
    let game = Regolith::honest();
    let mut backend = EcsBackend::new(game, seed());
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

/// The migrated module's population comes out of the storage layout, and it
/// agrees with the ruleset's declaration.
///
/// The second assertion is the anti-vacuity twin of the first: a
/// `migrated_population` that returned the entire store would agree with a
/// `declared_migrated` that was also wrong, so the test also pins that the two
/// are a *strict* subset relation with both sides non-empty. Four crafts stay
/// out; eight world seeds go in.
#[test]
fn the_migrated_module_is_selected_by_the_storage_layout() {
    let mut backend = seeded_world();
    let population = TickBackend::entities(&backend);
    let migrated = backend.migrated_population();

    assert_eq!(
        migrated,
        declared_migrated(&backend),
        "the migrated archetype's population disagrees with RegolithState::is_migrated"
    );
    assert!(
        !migrated.is_empty(),
        "no entity is in the migrated module, so nothing above was compared"
    );
    assert!(
        migrated.len() < population.len(),
        "every entity is in the migrated module, so a query that returned the \
         whole store would pass this file"
    );
    assert_eq!(
        migrated.len(),
        WORLD_SCENARIO.world_entities as usize,
        "the world scenario's eight world seeds are the migrated population"
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
    assert!(sections.contains(&SECTION_ROCK));
    sections.sort_unstable();
    sections.dedup();
    assert!(
        sections.len() > 1,
        "every migrated entity has the same section, so this migration is \
         indistinguishable from migrating one enum variant"
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

/// An install that crosses the frontier moves the entity; it does not leave it
/// carrying both components.
///
/// The hazard is specific to the decomposition and invisible to the
/// differential over the corpus, because the corpus never reuses an id across
/// sections. An entity holding `MigratedSection` and `RemainderSection` at once
/// is matched by *both* of `seal_population`'s queries, so it would be sealed
/// twice, step twice in one tick, and draw its per-entity RNG stream twice —
/// with the second step's neighbour read served from a map the first step
/// already overwrote.
#[test]
fn an_install_across_the_frontier_moves_the_entity_rather_than_duplicating_it() {
    let game = Regolith::honest();
    let mut backend = EcsBackend::new(game, seed());
    let entity = PersistId::new(1);

    TickBackend::insert(&mut backend, entity, game.spawn(entity, 0));
    assert_eq!(
        TickBackend::state(&backend, entity).map(RegolithState::section),
        Some(SECTION_CRAFT),
    );
    assert!(
        backend.migrated_population().is_empty(),
        "a craft is not in the migrated module"
    );

    let rock = game
        .spawn_world(entity, 0)
        .expect("Regolith seeds every world slot");
    TickBackend::insert(&mut backend, entity, rock);

    assert_eq!(
        backend.migrated_population(),
        vec![entity],
        "the replacing install did not move the entity into the migrated module"
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
        "the entity was sealed by both section queries and stepped twice in one tick"
    );
}
