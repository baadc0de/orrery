//! S7.4: the ECS behind the seam, held to four-class F-4 parity (#745).
//!
//! The claim this file makes, and the only one it makes: with canonical state
//! stored in a dedicated `bevy_ecs::World` and the tick driven by a
//! `bevy_ecs::Schedule`, every canonical artifact of a Regolith run is
//! byte-identical to the same run on the `BTreeMap`-backed executor — state
//! chain, outcome chain, at-rest slots and journal, witness claims, and the
//! verdicts the *shipped* adjudicator returns when each side's log is replayed
//! against the other side's claims.
//!
//! # Why the test lives here and not in `orrery_games`
//!
//! `orrery_games` is on `core-gates.sh` clause 1's Bevy-free list and ADR-0042
//! clause (d) refuses proposals to change that. The seam is not on it, and
//! `bevy_ecs` is a first-class dependency of this crate — verified: with it
//! declared under `[dependencies]` here, `./scripts/core-gates.sh` exits 0 and
//! role discovery still reports exactly `orrery_conformance orrery_core
//! orrery_games`. So the harness had to learn to accept a backend it cannot
//! name (`orrery_games::diff::run_differential_on`), and the call site that
//! names both backends is this file.
//!
//! # The unit of migration
//!
//! At the seam there is no such thing as "one module at a time". The host sees
//! an opaque `R: Ruleset` and a `PersistId`-keyed store; #737's Regolith module
//! split is a concept *inside* the gated crate and has no expression here. So
//! the unit is the whole entity store, taken at once, and the safety that a
//! module-at-a-time migration would have bought is bought instead by the two
//! backends being simultaneously live and differentially compared on every run
//! of this file. What the schedule's three named stages give is the *place*
//! module structure would land if the `Ruleset` contract ever handed a backend
//! a whole tick's population.

use std::collections::BTreeMap;

use orrery_core::{ComponentTypeId, Executor, Ruleset, TickBackend};
use orrery_games::diff::{
    run_differential_on, Backends, Baseline, Class, Subject, Verdict, VersionAxes,
};
use orrery_games::golden;
use orrery_games::regolith::{Regolith, REGOLITH_COMPOSITION};
use orrery_games::scenario::{play_with, Scenario, SCENARIOS, WORLD_SCENARIO};
use orrery_games::Game;
use orrery_protocol::atrest::SchemaVersion;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;
use orrery_sim_host::{Delivery, RulesetAdapter, SimulationHost, SimulationHostConfig, TickCount};

/// The version axes both sides declare. Identical by construction: this lane
/// changes storage, so a bumped axis would mean the lane did something else.
fn regolith_axes() -> VersionAxes {
    let schema_versions: BTreeMap<ComponentTypeId, SchemaVersion> = REGOLITH_COMPOSITION
        .component_schemas
        .iter()
        .map(|schema| (schema.id.component, schema.id.version))
        .collect();
    assert!(
        !schema_versions.is_empty(),
        "the manifest must declare the persisted component these axes are framed on"
    );
    VersionAxes {
        ruleset_version: Regolith::META.ruleset.version,
        projection_version: REGOLITH_COMPOSITION.projection_version.0,
        schema_versions,
    }
}

/// The committed baseline, covering the battery corpus **and**
/// [`WORLD_SCENARIO`].
///
/// The world scenario is not optional here. Every scenario in [`SCENARIOS`]
/// declares `world_entities: 0`, so across the whole battery corpus Regolith's
/// `regolith.world` module is stepped zero times — and with it, so is the ECS
/// substrate's entire materialization path: no bloom director means no rock is
/// ever spawned mid-tick, and stage `host.install-materializations` would run
/// 720 times without once being asked to do anything. A parity claim over
/// `SCENARIOS` alone would be a claim about a third of this file.
fn regolith_baseline() -> Baseline {
    let mut chains = Regolith::GOLDEN_CHAINS.to_vec();
    chains.extend_from_slice(&golden::REGOLITH_WORLD);
    let mut outcome_chains = golden::REGOLITH_OUTCOMES.to_vec();
    outcome_chains.extend_from_slice(&golden::REGOLITH_WORLD_OUTCOMES);
    Baseline {
        commit: "3c2ddef",
        axes: regolith_axes(),
        chains,
        outcome_chains,
    }
}

/// Every scenario the differential runs: the battery corpus plus the one that
/// actually populates a world domain.
fn scenarios() -> Vec<Scenario> {
    let mut scenarios = SCENARIOS.to_vec();
    scenarios.push(WORLD_SCENARIO);
    scenarios
}

fn subject(label: &'static str) -> Subject<Regolith> {
    Subject {
        label,
        game: Regolith::honest(),
        axes: regolith_axes(),
    }
}

/// The four-class parity verdict, with nothing left uncompared.
fn four_class_parity() -> Verdict {
    Verdict::Parity {
        compared: vec![
            Class::D1State,
            Class::D2Outcome,
            Class::D3Persistence,
            Class::D4Witness,
        ],
        not_compared: vec![],
    }
}

/// Run the F-4 differential for one scenario with the store-backed host as
/// legacy and the ECS-backed host as candidate.
fn differential(scenario: &Scenario) -> Verdict {
    run_differential_on(
        subject("store-backed host"),
        subject("ecs-backed host"),
        scenario,
        Some(&regolith_baseline()),
        Backends {
            legacy: Executor::new,
            candidate: EcsBackend::new,
        },
    )
}

/// **The exit criterion.** Every scenario in the table, four classes, nothing
/// not compared, legacy pinned to its committed goldens.
#[test]
fn the_ecs_backend_reaches_four_class_parity_with_the_store() {
    let scenarios = scenarios();
    assert!(
        scenarios.iter().any(|scenario| scenario.world_entities > 0),
        "no scenario populates a world domain, so the substrate's materialization stage is never exercised"
    );
    for scenario in &scenarios {
        assert_eq!(
            differential(scenario),
            four_class_parity(),
            "regolith/{}: the ECS-backed host did not reach four-class parity",
            scenario.name
        );
    }
}

/// The differential is not vacuous about the ECS: the candidate side really
/// stored its population in a `World`, really grew that population mid-tick,
/// and really agreed with the store about every entity's canonical state.
///
/// The middle clause is the one worth having. `host.install-materializations`
/// is the substrate's only stage that changes the shape of the world rather
/// than the contents of a component, and a green differential over a corpus
/// that never materializes anything would say nothing about it.
#[test]
fn the_candidate_side_stored_and_grew_its_population_in_a_world() {
    let scenario = &WORLD_SCENARIO;
    let seed = UniverseSeed([scenario.seed_byte; 32]);
    let seeded = scenario.entities + scenario.world_entities;

    let mut backend = EcsBackend::new(Regolith::honest(), seed);
    let mut store = Executor::new(Regolith::honest(), seed);
    for slot in 0..scenario.entities {
        let entity = PersistId::new(slot + 1);
        let state = Regolith::honest().spawn(entity, slot);
        TickBackend::insert(&mut backend, entity, state.clone());
        TickBackend::insert(&mut store, entity, state);
    }
    assert_eq!(
        TickBackend::entities(&backend),
        TickBackend::entities(&store),
        "the world's population index is not the store's, in the same order"
    );
    assert!(
        !TickBackend::entities(&backend).is_empty(),
        "the world holds no entities, so nothing below is a comparison"
    );
    for entity in TickBackend::entities(&backend) {
        assert_eq!(
            TickBackend::state(&backend, entity),
            TickBackend::state(&store, entity),
            "{entity:?}: the component and the store hold different canonical state"
        );
    }

    // Now play the whole world scenario on the ECS and count what the
    // materialization stage was actually asked to do.
    let played = play_with(Regolith::honest(), scenario, EcsBackend::new);
    let materialized: usize = played
        .outcome_entries
        .iter()
        .flat_map(|tick| tick.iter())
        .map(|entry| entry.materialized.len())
        .sum();
    assert!(
        materialized > 0,
        "the world scenario materialized nothing on the ECS, so \
         `host.install-materializations` never spawned an entity"
    );
    let final_population = played
        .log
        .last()
        .expect("the world scenario ran at least one tick")
        .entries
        .len();
    assert!(
        final_population > usize::try_from(seeded).expect("seeded population fits usize"),
        "the ECS population never grew past the {seeded} seeded entities, so no \
         materialized entity was ever stepped out of a `World` spawn"
    );
}

/// `TickBackend::step_entity` advances **one** entity on the ECS, not the
/// population it had to seal to build the neighbour view.
///
/// The seam drives `step_tick`, so nothing in this lane would have noticed if
/// the substrate had advanced everyone under a single-entity call — it would
/// have produced the right answer for the caller and silently burned the rest
/// of the population's tick. The trait promises one entity; this holds the ECS
/// to it, against the store.
#[test]
fn a_single_entity_step_on_the_ecs_advances_only_that_entity() {
    let seed = UniverseSeed([0x33; 32]);
    let mut ecs = EcsBackend::new(Regolith::honest(), seed);
    let mut store = Executor::new(Regolith::honest(), seed);
    for slot in 0..4u64 {
        let entity = PersistId::new(slot + 1);
        let state = Regolith::honest().spawn(entity, slot);
        TickBackend::insert(&mut ecs, entity, state.clone());
        TickBackend::insert(&mut store, entity, state);
    }

    let stepped = PersistId::new(2);
    let tick = Tick::new(1_000_000);
    let ecs_outcome = TickBackend::step_entity(&mut ecs, stepped, tick, &[])
        .expect("the ECS holds the stepped entity");
    let store_outcome = TickBackend::step_entity(&mut store, stepped, tick, &[])
        .expect("the store holds the stepped entity");
    assert_eq!(
        ecs_outcome.state_hash, store_outcome.state_hash,
        "the two backends disagree on a single entity's tick"
    );
    for entity in TickBackend::entities(&ecs) {
        assert_eq!(
            TickBackend::state(&ecs, entity),
            TickBackend::state(&store, entity),
            "{entity:?}: the ECS advanced an entity the caller did not ask for"
        );
    }
}

/// Regolith's event routing, as the seam's adapter.
#[derive(Debug, Clone, Copy, Default)]
struct RegolithAdapter;

impl RulesetAdapter<Regolith> for RegolithAdapter {
    fn deliver(
        &self,
        event: &<Regolith as Ruleset>::CoreEvent,
    ) -> Option<Delivery<<Regolith as Ruleset>::CoreInput>> {
        Regolith::honest()
            .deliver(event)
            .map(|delivery| Delivery::new(delivery.0, delivery.1))
    }
}

/// Literally at the host: two `SimulationHost`s differing only in substrate
/// produce the same step report, the same event bytes and the same state
/// bytes. `run_differential_on` compares the substrates; this compares the
/// thing D42 (d) actually names — the host.
#[test]
fn the_two_hosts_agree_on_report_events_and_state_bytes() {
    const TICKS: u64 = 120;
    let seed = UniverseSeed([0x55; 32]);
    let config = SimulationHostConfig::new(seed).starting_at(Tick::new(40));

    let mut store_host = SimulationHost::new(config, Regolith::honest(), RegolithAdapter);
    let mut ecs_host = SimulationHost::on_backend(
        config,
        EcsBackend::new(Regolith::honest(), seed),
        RegolithAdapter,
    );
    for slot in 0..4u64 {
        let entity = PersistId::new(slot + 1);
        store_host.install_state(entity, Regolith::honest().spawn(entity, slot));
        ecs_host.install_state(entity, Regolith::honest().spawn(entity, slot));
    }

    let store_report = store_host.step(TickCount::new(TICKS));
    let ecs_report = ecs_host.step(TickCount::new(TICKS));
    assert!(
        !store_report.state_hashes.is_empty(),
        "the store-backed host executed no entity-ticks, so nothing is compared"
    );
    assert_eq!(
        store_report, ecs_report,
        "the two hosts disagree on the step report"
    );
    assert_eq!(
        store_host.drain_event_bytes(),
        ecs_host.drain_event_bytes(),
        "the two hosts disagree on the emitted event bytes"
    );
    assert_eq!(
        store_host.collect_output_bytes(),
        ecs_host.collect_output_bytes(),
        "the two hosts disagree on the canonical state bytes"
    );
}
