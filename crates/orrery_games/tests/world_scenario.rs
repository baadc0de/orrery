//! The scenario that actually steps `regolith.world`, and what it is worth.
//!
//! Every scenario in [`SCENARIOS`] declares `world_entities: 0`. Measured on
//! this tree: across all four, the `regolith.world` module — the `rock`,
//! `pickup` and `bloom-director` sections #737 split out — is stepped
//! **zero** times. Nothing in the battery, the goldens or the differential
//! corpus reached that module at all, so any suite run over that corpus was
//! green about code that never executed: `0 passed; N filtered out` wearing a
//! verdict's clothes.
//!
//! [`WORLD_SCENARIO`] closes that gap, and these tests are what keep it
//! closed. Salvaged out of S7.4 (#745), which needed the coverage to prove a
//! change to that module was safe and found there was none.
//!
//! | Test | The failure it names |
//! |---|---|
//! | the corpus does not step the world module | the premise above silently changing, leaving `WORLD_SCENARIO` redundant and unexamined |
//! | the world scenario steps every section | a scenario named "world" that seeds a population and still never steps it |
//! | honest world play raises no stage-1 flag | a false positive in the one domain a Regolith tester actually plays in (#1124) |
//! | the seeded director satisfies the invariant | the fixture itself going offside, as it did in #1124, and taking the check above down with it |
//! | the world scenario matches its goldens | an unintended change to the three sections nothing else pins |
//! | the goldens name the scenario | a rename pointing the fixture rows at nothing |
//! | the cross-replay leg's reach | D-4's adjudication half being credited with coverage it does not have |
//! | D-4 skips only the silent | `authored_bundles` dropping an entity that *did* author evidence |

use orrery_games::diff::{collect_artifacts, Side, VersionAxes};
use orrery_games::golden;
use orrery_games::regolith::state::RegolithState;
use orrery_games::regolith::{Regolith, BLOOM_CADENCE_TICKS, REGOLITH_COMPOSITION};
use orrery_games::scenario::{play, Play, Scenario, SCENARIOS, WORLD_SCENARIO};
use orrery_games::Game;
use orrery_protocol::atrest::SchemaVersion;
use orrery_protocol::PersistId;
use orrery_protocol::MAX_ADJUDICATION_TICKS;
use std::collections::{BTreeMap, BTreeSet};

/// The same axes `tests/differential.rs` declares, for the same reasons, and
/// now from the same source: the manifest's schema table is populated (#750),
/// so the persisted component is read from it rather than reached for in the
/// registry, and D-3 frames a real slot per entity-tick.
const DECLARED_COMPONENT: orrery_core::ComponentTypeId =
    REGOLITH_COMPOSITION.component_schemas[0].id.component;

fn regolith_axes() -> VersionAxes {
    let schema_versions: BTreeMap<orrery_core::ComponentTypeId, SchemaVersion> =
        REGOLITH_COMPOSITION
            .component_schemas
            .iter()
            .map(|schema| (schema.id.component, schema.id.version))
            .collect();
    assert!(
        schema_versions.contains_key(&DECLARED_COMPONENT),
        "the manifest must declare the persisted component these axes are framed on"
    );
    VersionAxes {
        ruleset_version: Regolith::META.ruleset.version,
        projection_version: REGOLITH_COMPOSITION.projection_version.0,
        schema_versions,
    }
}

/// How many times each `regolith.world` section was stepped in a run.
#[derive(Debug, Default, PartialEq, Eq)]
struct WorldSteps {
    rock: u64,
    pickup: u64,
    director: u64,
}

impl WorldSteps {
    fn of(played: &Play<Regolith>) -> Self {
        let mut steps = Self::default();
        for record in &played.log {
            for entry in &record.entries {
                match entry.state {
                    RegolithState::Craft(_) => {}
                    RegolithState::Rock(_) => steps.rock += 1,
                    RegolithState::Pickup(_) => steps.pickup += 1,
                    RegolithState::BloomDirector(_) => steps.director += 1,
                }
            }
        }
        steps
    }

    fn total(&self) -> u64 {
        self.rock + self.pickup + self.director
    }
}

/// The vacuity self-check, measured rather than argued: the battery's corpus
/// steps the world module zero times, so nothing taken over it is evidence
/// about that module.
#[test]
fn the_battery_corpus_never_steps_the_world_module() {
    for scenario in SCENARIOS {
        let steps = WorldSteps::of(&play(Regolith::honest(), scenario));
        assert_eq!(
            steps.total(),
            0,
            "{}: the corpus scenario stepped the world module ({steps:?}); \
             this test's premise — and the reason WORLD_SCENARIO exists — has changed",
            scenario.name
        );
    }
}

/// And the opposite, of the one scenario that is supposed to reach it: all
/// three sections stepped, in quantity, including the bloom the director
/// seeds and the expiry the pickup reaches.
#[test]
fn the_world_scenario_steps_every_section_of_the_module() {
    let played = play(Regolith::honest(), &WORLD_SCENARIO);
    let steps = WorldSteps::of(&played);
    assert!(
        steps.rock > 900,
        "rock sections stepped {} times; the seeded rocks alone would give 6 per tick, \
         so this is below even the un-bloomed floor",
        steps.rock
    );
    assert_eq!(
        steps.pickup, WORLD_SCENARIO.ticks,
        "the seeded pickup did not step on every tick of the window"
    );
    assert_eq!(
        steps.director, WORLD_SCENARIO.ticks,
        "the seeded bloom director did not step on every tick of the window"
    );
    // The bloom fired: its rocks are materialized entities, so the rock step
    // count exceeds what the seeded rocks alone could produce.
    let seeded_rocks = WORLD_SCENARIO.world_entities - 2;
    assert!(
        steps.rock > seeded_rocks * WORLD_SCENARIO.ticks,
        "no rock was materialized: {} rock steps is exactly the {seeded_rocks} seeded rocks \
         drifting, so the bloom, split and materialization paths never ran",
        steps.rock
    );
}

/// **The false-positive check, for the domain a tester actually plays in.**
///
/// `battery.rs`'s `honest_play_raises_no_stage_one_flag` is the measurement P4
/// exists for in miniature, and it iterates [`SCENARIOS`] — every member of
/// which declares `world_entities: 0`. So until #1124 the one check that asks
/// "did honest play accuse anybody" had never seen a rock, a pickup or a bloom
/// director: the whole domain a Regolith playtest happens in had zero
/// false-positive coverage, and the gate was asserting a rate it had not
/// measured there.
///
/// The scenario could not simply be handed to that loop, because the fixture
/// seeding it was itself in permanent violation of `regolith/value-range` —
/// 36,000 flags over 30 simulated minutes, every one of them the seed's own
/// off-cadence `next_bloom_tick`. That is fixed at the fixture (see
/// `regolith::world_seed`); this is the check that keeps it fixed.
///
/// Run past a *second* bloom as well as the shipped window, because the
/// re-seed is where the cadence invariant is most easily broken: `seed_bloom`
/// advancing `next_bloom_tick` by anything but a whole cadence would leave the
/// short window green and fail here.
#[test]
fn honest_world_play_raises_no_stage_one_flag() {
    // Long enough for the seeded director's second bloom: its clock starts one
    // cadence short of `WORLD_SEED_FIRST_BLOOM_TICK`, so blooms land 60 ticks
    // in and one whole cadence after that.
    let two_blooms = Scenario {
        name: "world-two-blooms",
        ticks: BLOOM_CADENCE_TICKS + 400,
        ..WORLD_SCENARIO
    };
    for scenario in [&WORLD_SCENARIO, &two_blooms] {
        let played = play(Regolith::honest(), scenario);
        assert!(
            played.flags.is_empty(),
            "{}: honest play in the rock/pickup/bloom domain raised {} stage-1 flags {:?}; \
             the first is at {:?} on entity {:?}",
            scenario.name,
            played.flags.len(),
            played.flagged_validators(),
            played.flags[0].tick,
            played.flags[0].entity,
        );
    }

    // Non-vacuity: the longer run really did re-seed, so the clause above
    // covered `seed_bloom`'s advance and not merely the seeded values.
    let played = play(Regolith::honest(), &two_blooms);
    let blooms = played
        .log
        .last()
        .expect("the run logged at least one tick")
        .entries
        .iter()
        .find_map(|entry| match entry.state {
            RegolithState::BloomDirector(ref director) => Some(director.blooms_seeded),
            _ => None,
        })
        .expect("the seeded director is still installed at the end of the run");
    assert!(
        blooms >= 2,
        "the longer window seeded {blooms} blooms, so `seed_bloom`'s advance of \
         `next_bloom_tick` was never exercised under the flag check"
    );
}

/// **The fixture is on the cadence the ruleset publishes.**
///
/// Stated against the seed directly, so the failure names the fixture rather
/// than arriving as an opaque wall of value-range flags the way #1124 did.
/// `regolith/value-range` requires a director's `next_bloom_tick` to be a
/// multiple of [`BLOOM_CADENCE_TICKS`] and strictly ahead of its clock, and
/// production upholds it: `BloomDirector::spawned` seeds one whole cadence and
/// `seed_bloom` only ever adds another. A scenario seed is held to the same
/// rule, because a seed that is not is a seed no honest-play check can run on.
#[test]
fn the_seeded_bloom_director_satisfies_the_cadence_invariant() {
    let game = Regolith::honest();
    let mut seen = 0;
    for slot in 0..WORLD_SCENARIO.world_entities {
        let entity = PersistId::new(WORLD_SCENARIO.entities + slot + 1);
        let Some(RegolithState::BloomDirector(director)) = game.spawn_world(entity, slot) else {
            continue;
        };
        seen += 1;
        assert_eq!(
            director.next_bloom_tick % BLOOM_CADENCE_TICKS,
            0,
            "world slot {slot} seeds next_bloom_tick {} which is off the {BLOOM_CADENCE_TICKS} \
             cadence; the value-range invariant rejects it on every tick from the first",
            director.next_bloom_tick
        );
        assert!(
            director.next_bloom_tick > director.clock_tick,
            "world slot {slot} seeds a bloom at {} that is not ahead of its clock at {}",
            director.next_bloom_tick,
            director.clock_tick
        );
    }
    assert_eq!(
        seen, 1,
        "the world population no longer seeds exactly one bloom director; \
         this test and `world_seed`'s slot 0 have drifted apart"
    );
}

/// The committed pin. Before this scenario existed no digest anywhere covered
/// the three sections, so a change to any of them moved nothing.
#[test]
fn the_world_scenario_matches_its_golden_chains() {
    let played = play(Regolith::honest(), &WORLD_SCENARIO);
    assert_eq!(
        (WORLD_SCENARIO.name, played.chain),
        golden::REGOLITH_WORLD[0],
        "the world scenario's state chain moved"
    );
    assert_eq!(
        (WORLD_SCENARIO.name, played.outcome_chain),
        golden::REGOLITH_WORLD_OUTCOMES[0],
        "the world scenario's outcome chain moved"
    );
}

/// The fixture rows this scenario pins, restated so a rename cannot silently
/// point them at nothing — and so the scenario cannot drift into the battery
/// corpus without its goldens following it there.
#[test]
fn the_world_scenario_is_the_one_the_goldens_name() {
    let named: Vec<&str> = golden::REGOLITH_WORLD
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(named, vec![WORLD_SCENARIO.name]);
    let named: Vec<&str> = golden::REGOLITH_WORLD_OUTCOMES
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(named, vec![WORLD_SCENARIO.name]);
    assert!(
        !SCENARIOS
            .iter()
            .any(|scenario: &Scenario| scenario.name == WORLD_SCENARIO.name),
        "WORLD_SCENARIO joined the battery corpus; its goldens now need to be in the battery's tables"
    );
}

/// **The reach of D-4's strongest leg, measured rather than assumed.**
///
/// The cross-replay half re-executes each side's signed log against the
/// other's claims through `orrery_core::verify_bundle`. It can only do that
/// for entities that *authored* evidence, and `InputLogProducer::cut_frame`
/// authors no frame for an entity with no pending input records. Under the
/// shipped honest pilot no `regolith.world` entity ever receives an input —
/// `Order::Lock` and `Order::Grab` are player-sent and `pilot::honest_orders`
/// sends neither — so the world module is covered by D-4's *claim-value* half
/// and not by its adjudication half.
///
/// That is a real limit on the evidence, so it is pinned here: if a later
/// scenario or pilot does put inputs in front of the world module, this test
/// fails and the limitation is retired deliberately rather than forgotten.
#[test]
fn the_cross_replay_leg_reaches_the_players_and_not_the_world_module() {
    let played = play(Regolith::honest(), &WORLD_SCENARIO);
    let artifacts = collect_artifacts(&Regolith::honest(), &played, Side::Legacy, regolith_axes());
    let witness = artifacts.d4.expect("the legacy side produced D-4");

    let bundled: Vec<u64> = witness.bundles.keys().map(|entity| entity.0).collect();
    let players: Vec<u64> = (1..=WORLD_SCENARIO.entities).collect();
    assert_eq!(
        bundled, players,
        "the set of entities the adjudicator can be run on changed; \
         if the world module now authors evidence, the stated \
         cross-replay limitation no longer holds and should be retired"
    );

    // The claim-value half is the one that does reach the module: every
    // world entity-tick has a claim.
    let world_claims = witness
        .claims
        .keys()
        .filter(|key| key.entity.0 > WORLD_SCENARIO.entities)
        .count();
    assert!(
        world_claims as u64 > WORLD_SCENARIO.ticks,
        "only {world_claims} world-entity claims: D-4's claim half does not reach the module either"
    );
}

/// **`authored_bundles` skips the silent and only the silent.**
///
/// The bundle loop drops an entity whose adjudication window is not covered
/// claim-to-claim. That is the right answer *only* for an entity that
/// genuinely authored nothing; dropping one that did author evidence would be
/// a hole in the very class that exists to convict, and it would be invisible
/// — a smaller `bundles` map still compares equal to a smaller `bundles` map.
///
/// So the skip is checked against its cause rather than against itself: an
/// entity authors frames exactly when it has input records inside the
/// adjudication window, because that is what `InputLogProducer::cut_frame`
/// requires. This asserts the two sets are the same set — every
/// initially-installed entity with an input in the window has a bundle, and
/// every entity without one does not. A skip with a different cause fails
/// here.
#[test]
fn d4_bundles_exactly_the_entities_that_received_inputs_in_the_window() {
    let played = play(Regolith::honest(), &WORLD_SCENARIO);
    let artifacts = collect_artifacts(&Regolith::honest(), &played, Side::Legacy, regolith_axes());
    let witness = artifacts.d4.expect("the legacy side produced D-4");

    // The population `authored_bundles` reconstructs: players, then the
    // world seeds, exactly as `scenario::play` installed them.
    let installed: BTreeSet<PersistId> = (1..=WORLD_SCENARIO.entities
        + WORLD_SCENARIO.world_entities)
        .map(PersistId::new)
        .collect();

    let window_end = played.sealed.tick_window.first.0
        + (played.sealed.tick_window.end_exclusive.0 - played.sealed.tick_window.first.0)
            .min(MAX_ADJUDICATION_TICKS);
    let mut inputs_in_window: BTreeSet<PersistId> = BTreeSet::new();
    for record in &played.sealed.input_log {
        if record.tick.0 >= window_end {
            break;
        }
        for entry in &record.entries {
            if !entry.inputs.is_empty() && installed.contains(&entry.entity) {
                inputs_in_window.insert(entry.entity);
            }
        }
    }

    let bundled: BTreeSet<PersistId> = witness.bundles.keys().copied().collect();
    assert!(
        !inputs_in_window.is_empty(),
        "no initially-installed entity received an input in the window; \
         this test would pass vacuously"
    );
    assert_eq!(
        bundled, inputs_in_window,
        "D-4 bundled a set of entities that is not the set with inputs in the \
         adjudication window; an entity that authored evidence was skipped, or \
         one that authored none was bundled"
    );

    // And the skipped ones are skipped for the stated reason, not merely
    // absent: they are installed, they are claimed about, and they received
    // nothing.
    let skipped: Vec<PersistId> = installed.difference(&bundled).copied().collect();
    assert!(
        !skipped.is_empty(),
        "nothing was skipped; this test's subject does not occur in this scenario"
    );
    for entity in skipped {
        assert!(
            !inputs_in_window.contains(&entity),
            "entity {} was skipped despite receiving inputs in the window",
            entity.0
        );
        assert!(
            witness.claims.keys().any(|key| key.entity == entity),
            "entity {} was skipped and is not even claimed about; it was never installed",
            entity.0
        );
    }
}
