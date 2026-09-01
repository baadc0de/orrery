//! D43 clause (e)(5): single-entity step semantics, exposed and honest.
//!
//! > exposes single-entity step semantics to witnesses and adjudication: the
//! > verdict must hold in a world of one, and "the schedule was deterministic"
//! > is never a substitute for per-entity replay.
//!
//! The clause has two halves and this file holds the host to both.
//!
//! **The verdict holds in a world of one.** A witness does not rebuild the
//! authority's population. `ReplayHarness::load_claimed_snapshot` loads the one
//! state its claim commits to, so the step that follows sees an empty
//! neighbour map — which is exactly what
//! [`orrery_games::scenario::adjudicate_isolated`] models on the `Executor`.
//! This file builds that same world **on the ECS**: one `EcsBackend` per
//! entity, holding that entity and nothing else, replaying that entity's
//! recorded inputs through `TickBackend::step_entity`, and reproducing every
//! recorded hash. That is per-entity replay on the substrate under test, not a
//! statement about the schedule.
//!
//! **The exposure is honest.** A backend that quietly advanced the whole
//! population under a single-entity call would return the right answer to its
//! caller and silently burn everyone else's tick — and in a world of one the
//! difference is invisible, which is precisely why this file also holds the
//! populated case.
//!
//! # What this file claims, and what its sibling does
//!
//! This file demonstrates the property *here*, in a harness: given the run's
//! recorded inputs, a world of one on the ECS reproduces every hash the full
//! population claimed. Until #763 that was the whole of it — the shipped
//! adjudicator still re-executed on the `Executor`, so on the ECS path the
//! D-4 *frames* were executor-authored while the *claim values* were
//! ECS-derived. Conviction power never depended on it (a diverging ECS fails
//! D-1/D-2/D-3 and the claim values independently of the frames), but the
//! adjudicator that would convict an ECS host was re-executing on a different
//! substrate than the one under suspicion.
//!
//! #763 closed that: `orrery_core::verify_bundle_on` and `ReplayHarness::on`
//! take the substrate, and `orrery_games::diff`'s `authored_bundles`,
//! `collect_witness_on` and `cross_replay_on` carry it through. The property
//! is now embodied in the adjudicator's own substrate, and
//! `tier_h_adjudication_substrate.rs` is what holds it there.

use std::collections::BTreeMap;

use orrery_core::{Executor, TickBackend};
use orrery_games::regolith::Regolith;
use orrery_games::scenario::{adjudicate_isolated, play_with, Scenario, SCENARIOS};
use orrery_games::Game;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;

fn regolith_ecs(game: Regolith, seed: UniverseSeed) -> EcsBackend<Regolith> {
    EcsBackend::new(game, seed).with_migrated_module(
        orrery_games::regolith::world_ecs::sync_migrated,
        orrery_games::regolith::world_ecs::step_migrated,
    )
}

/// Every scenario whose population is more than one entity — the ones where
/// "a world of one" is a *reduction* rather than a restatement of the run.
fn populated_scenarios() -> Vec<Scenario> {
    SCENARIOS
        .iter()
        .copied()
        .filter(|scenario| scenario.entities > 1)
        .collect()
}

/// D43 (e)(5). One named check, both halves.
#[test]
fn the_verdict_holds_in_a_world_of_one() {
    let scenarios = populated_scenarios();
    assert!(
        !scenarios.is_empty(),
        "no scenario has more than one entity, so 'a world of one' would be the run itself"
    );

    let mut replayed_entity_ticks = 0usize;
    for scenario in &scenarios {
        let seed = UniverseSeed([scenario.seed_byte; 32]);
        // The log under adjudication is authored on the ECS, not on the store.
        let played = play_with(Regolith::honest(), scenario, regolith_ecs);
        assert!(
            !played.log.is_empty(),
            "regolith/{}: the ECS authored an empty log",
            scenario.name
        );

        // ── Half one: one world per entity, on the ECS ───────────────────
        let mut worlds: BTreeMap<PersistId, EcsBackend<Regolith>> = BTreeMap::new();
        for slot in 0..scenario.entities {
            let entity = PersistId::new(slot + 1);
            let mut world = regolith_ecs(Regolith::honest(), seed);
            world.insert(entity, Regolith::honest().spawn(entity, slot));
            assert_eq!(
                TickBackend::entities(&world),
                vec![entity],
                "regolith/{}: {entity:?}'s adjudicating world is not a world of one",
                scenario.name
            );
            worlds.insert(entity, world);
        }

        for record in &played.log {
            for entry in &record.entries {
                let world = worlds.get_mut(&entry.entity).expect(
                    "the ECS-authored log names an entity outside the initial population — this \
                     harness is scoped to scenarios that materialize nothing",
                );
                let outcome = world
                    .step_entity(entry.entity, record.tick, &entry.inputs)
                    .expect("each world holds the entity it was built for");
                assert!(
                    outcome.materialized.is_empty(),
                    "regolith/{}: a world of one materialized an entity, which this harness \
                     does not model",
                    scenario.name
                );
                assert_eq!(
                    outcome.state_hash, entry.hash,
                    "regolith/{}: replaying {:?} alone in an ECS world of one at {:?} did not \
                     reproduce the hash it claimed under the full population — the verdict does \
                     not hold in a world of one (D43 (e)(5))",
                    scenario.name, entry.entity, record.tick
                );
                replayed_entity_ticks += 1;
            }
        }

        // The `Executor`'s isolated adjudication of the ECS-authored log must
        // reach the same verdict. Two substrates, one world-of-one verdict.
        assert_eq!(
            adjudicate_isolated(Regolith::honest, scenario, &played),
            None,
            "regolith/{}: the shipped isolated adjudicator convicted an honest ECS-authored log",
            scenario.name
        );
    }
    assert!(
        replayed_entity_ticks > 0,
        "no entity-tick was replayed in a world of one, so nothing above is a verdict"
    );

    // ── Half two: the exposure is honest in a populated world ────────────
    // In a world of one, a backend that advanced everyone would look correct.
    // This is where that lie is visible, and it is part of the same clause:
    // per-entity replay is only a substitute for the schedule if `step_entity`
    // really means one entity.
    let seed = UniverseSeed([0x5e; 32]);
    let mut ecs = regolith_ecs(Regolith::honest(), seed);
    let mut store = Executor::new(Regolith::honest(), seed);
    for slot in 0..6u64 {
        let entity = PersistId::new(slot + 1);
        let state = Regolith::honest().spawn(entity, slot);
        TickBackend::insert(&mut ecs, entity, state.clone());
        TickBackend::insert(&mut store, entity, state);
    }
    let before: Vec<_> = TickBackend::entities(&ecs)
        .into_iter()
        .map(|entity| {
            (
                entity,
                TickBackend::state(&ecs, entity)
                    .expect("the entity was just installed")
                    .clone(),
            )
        })
        .collect();
    assert!(
        before.len() > 1,
        "a populated world needs more than one entity"
    );

    let stepped = PersistId::new(3);
    let tick = Tick::new(1_000_000);
    let ecs_outcome = TickBackend::step_entity(&mut ecs, stepped, tick, &[])
        .expect("the ECS holds the stepped entity");
    let store_outcome = TickBackend::step_entity(&mut store, stepped, tick, &[])
        .expect("the store holds the stepped entity");
    assert_eq!(
        ecs_outcome.state_hash, store_outcome.state_hash,
        "the ECS and the store disagree on a single entity's tick, so per-entity replay is not a \
         shared unit (D43 (e)(5))"
    );
    let mut bystanders = 0usize;
    for (entity, was) in &before {
        let now = TickBackend::state(&ecs, *entity).expect("the entity is still installed");
        if *entity == stepped {
            continue;
        }
        bystanders += 1;
        assert_eq!(
            now, was,
            "{entity:?}: `step_entity` advanced an entity the caller did not ask for — the host \
             does not expose single-entity step semantics, so per-entity replay is a fiction \
             (D43 (e)(5))"
        );
    }
    assert!(
        bystanders > 0,
        "the populated world had no bystanders, so nothing checked single-entity semantics"
    );
}
