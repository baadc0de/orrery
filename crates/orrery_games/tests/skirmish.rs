//! Skirmish's own tests: the rules that refuse, and the detection table.
//!
//! The battery proves what must hold for any reference game. This proves the
//! two things that are specific to these rules and that the whole crate is
//! pointed at:
//!
//! - the rules **refuse** — cooldown, reach, and death are enforced, so there
//!   is a gap between what a client asks for and what it gets, which is where
//!   cheating lives;
//! - each tamper is caught by the stage it is supposed to be caught by, and
//!   `DamageInflation` is caught by *only* the expensive one.

use orrery_core::{evaluate, Executor, InvariantKind, InvariantSample, QPos, QVel, Ruleset};
use orrery_games::game::Tamper;
use orrery_games::scenario::{adjudicate, play, Scenario, SCENARIOS};
use orrery_games::skirmish::archetype::Archetype;
use orrery_games::skirmish::invariants::INVARIANTS;
use orrery_games::skirmish::order::{Order, Outcome};
use orrery_games::skirmish::state::Craft;
use orrery_games::skirmish::{Skirmish, SKIRMISH_RULESET};
use orrery_protocol::{DeviationKind, PersistId, Tick, UniverseSeed};

const T: u64 = 1_000;

fn craft_at(archetype: Archetype, x_mm: i64) -> Craft {
    Craft::spawned(
        archetype,
        QPos {
            x: x_mm,
            y: 0,
            z: 0,
        },
        0,
    )
}

fn world(states: &[(u64, Craft)]) -> Executor<Skirmish> {
    let mut executor = Executor::new(Skirmish::honest(), UniverseSeed([7; 32]));
    for (id, craft) in states {
        executor.insert(PersistId::new(*id), craft.clone());
    }
    executor
}

fn fire_at(target: u64) -> Order {
    Order::Fire {
        target: PersistId::new(target),
    }
}

fn island() -> &'static Scenario {
    SCENARIOS
        .iter()
        .find(|s| s.name == "island")
        .expect("the island scenario is in the table")
}

#[test]
fn a_tampered_build_still_claims_the_honest_ruleset() {
    // The whole model in one assertion: a cheat asserts it is running the
    // rules, and that claim is what a witness holds it to. A build that
    // announced its own cheating id would be adjudicated as an unknown
    // ruleset — unadjudicable, never a strike.
    for tamper in Tamper::ALL {
        let cheat = Skirmish::cheating(*tamper);
        assert_eq!(cheat.id(), SKIRMISH_RULESET, "{}", tamper.name());
    }
}

#[test]
fn the_weapon_will_not_fire_twice_in_one_tick() {
    let mut world = world(&[
        (1, craft_at(Archetype::Interceptor, 0)),
        (2, craft_at(Archetype::Interceptor, 100_000)),
    ]);
    let outcome = world
        .step_entity(
            PersistId::new(1),
            Tick::new(T),
            &[fire_at(2), fire_at(2), fire_at(2)],
        )
        .expect("entity 1 is installed");

    assert_eq!(outcome.events.len(), 1, "three orders, one shot");
    assert_eq!(world.state(PersistId::new(1)).unwrap().shots, 1);
}

#[test]
fn the_weapon_comes_back_exactly_on_its_cooldown() {
    let cooldown = u64::from(Archetype::Interceptor.limits().cooldown_ticks);
    let mut world = world(&[
        (1, craft_at(Archetype::Interceptor, 0)),
        (2, craft_at(Archetype::Interceptor, 100_000)),
    ]);

    for offset in 0..=cooldown {
        world
            .step_entity(PersistId::new(1), Tick::new(T + offset), &[fire_at(2)])
            .expect("entity 1 is installed");
        let shots = world.state(PersistId::new(1)).unwrap().shots;
        let expected = if offset < cooldown { 1 } else { 2 };
        assert_eq!(
            shots, expected,
            "at +{offset} ticks the craft should have fired {expected} times"
        );
    }
}

#[test]
fn a_shot_beyond_reach_is_not_a_shot() {
    let reach = Archetype::Interceptor.limits().range_mm;
    let mut world = world(&[
        (1, craft_at(Archetype::Interceptor, 0)),
        (2, craft_at(Archetype::Interceptor, reach + 1_000)),
    ]);
    let outcome = world
        .step_entity(PersistId::new(1), Tick::new(T), &[fire_at(2)])
        .expect("entity 1 is installed");

    assert!(
        outcome.events.is_empty(),
        "out of reach, so nothing happens"
    );
    let craft = world.state(PersistId::new(1)).unwrap();
    assert_eq!(craft.shots, 0);
    // And crucially the weapon is not on cooldown: a refused order costs
    // nothing, so a craft that keeps asking fires the instant it closes.
    assert_eq!(craft.cooldown, 0);
}

#[test]
fn a_wreck_neither_steers_nor_shoots_but_still_takes_hits() {
    let mut dead = craft_at(Archetype::Interceptor, 0);
    dead.hull = 0;
    dead.shield = 0;
    let mut world = world(&[(1, dead), (2, craft_at(Archetype::Interceptor, 100_000))]);

    let outcome = world
        .step_entity(
            PersistId::new(1),
            Tick::new(T),
            &[
                Order::Thrust {
                    accel_mmss: 60_000,
                    yaw_urad: 100_000,
                    pitch_urad: 0,
                },
                fire_at(2),
                Order::Damage { amount: 10 },
            ],
        )
        .expect("entity 1 is installed");

    assert!(outcome.events.is_empty());
    let craft = world.state(PersistId::new(1)).unwrap();
    assert_eq!(craft.vel, QVel::default(), "a wreck does not thrust");
    assert_eq!(craft.yaw_urad, 0, "a wreck does not steer");
    assert_eq!(craft.shots, 0, "a wreck does not shoot");
    assert_eq!(craft.hull, 0, "and it cannot be killed twice");
}

#[test]
fn shields_absorb_before_hull_and_hull_floors_at_zero() {
    let limits = Archetype::Interceptor.limits();
    let mut world = world(&[(1, craft_at(Archetype::Interceptor, 0))]);
    let entity = PersistId::new(1);

    world
        .step_entity(
            entity,
            Tick::new(T),
            &[Order::Damage {
                amount: limits.max_shield + 10,
            }],
        )
        .expect("entity 1 is installed");
    let craft = world.state(entity).unwrap();
    assert_eq!(craft.shield, 0);
    assert_eq!(craft.hull, limits.max_hull - 10);

    let outcome = world
        .step_entity(
            entity,
            Tick::new(T + 1),
            &[Order::Damage {
                amount: limits.max_hull * 10,
            }],
        )
        .expect("entity 1 is installed");
    assert_eq!(world.state(entity).unwrap().hull, 0, "never negative");
    assert_eq!(outcome.events, vec![Outcome::Destroyed]);
}

// --- the detection table (see `Tamper`) ---------------------------------

#[test]
fn the_speed_multiplier_is_caught_by_the_cheap_checks_and_by_replay() {
    // P4's demo criterion, minus the network: a 1.5× client is loud. It is
    // loud at stage 1, which is what lets a peer that is not in the witness
    // set escalate at all, and it is out of band on re-execution, which is
    // what makes the escalation adjudicable.
    let scenario = island();
    let cheated = play(Skirmish::cheating(Tamper::SpeedMultiplier), scenario);

    assert!(
        cheated.flagged_validators().contains(&"skirmish/speed-cap"),
        "stage 1 missed a 1.5× craft: {:?}",
        cheated.flagged_validators()
    );
    let divergence = adjudicate(Skirmish::honest(), scenario, &cheated)
        .expect("a 1.5× craft cannot re-execute as honest");
    assert_eq!(divergence.kind, DeviationKind::ContinuousOutOfBand);
}

#[test]
fn inflated_damage_is_invisible_to_the_cheap_checks_and_caught_only_by_replay() {
    // The case that justifies the whole replay apparatus. Nothing about a
    // doubled roll is *impossible*: the victim's hull drops by a legal amount
    // and the attacker's counters advance by legal steps, so no history-free
    // check can see it. Only re-executing the attacker's own window can — and
    // only because the roll is recorded in the attacker's own state, which is
    // what `damage_dealt` exists for.
    let scenario = island();
    let cheated = play(Skirmish::cheating(Tamper::DamageInflation), scenario);

    assert!(
        cheated.flags.is_empty(),
        "stage 1 should be blind to this, and claiming otherwise would \
         overstate what cheap checks can do: {:?}",
        cheated.flagged_validators()
    );
    let divergence = adjudicate(Skirmish::honest(), scenario, &cheated)
        .expect("an inflated roll cannot re-execute as honest");
    assert_eq!(
        divergence.kind,
        DeviationKind::DiscreteMismatch,
        "the trajectories agree and the state hash does not — that is the \
         signature of a cheat that never moved illegally"
    );
}

#[test]
fn ignoring_the_cooldown_is_caught_by_the_rate_limit_and_by_replay() {
    let scenario = island();
    let cheated = play(Skirmish::cheating(Tamper::NoCooldown), scenario);

    assert!(
        cheated.flagged_validators().contains(&"skirmish/fire-rate"),
        "stage 1 missed an unlimited fire rate: {:?}",
        cheated.flagged_validators()
    );
    let divergence = adjudicate(Skirmish::honest(), scenario, &cheated)
        .expect("an unlimited fire rate cannot re-execute as honest");
    assert_eq!(divergence.kind, DeviationKind::DiscreteMismatch);
}

// --- the invariants, on states the rules cannot produce ------------------

fn sample<'a>(
    previous: Option<&'a Craft>,
    current: &'a Craft,
    elapsed_ticks: u32,
) -> InvariantSample<'a, Craft> {
    InvariantSample {
        entity: PersistId::new(1),
        current,
        tick: Tick::new(T),
        previous,
        elapsed_ticks,
    }
}

fn kind(previous: Option<&Craft>, current: &Craft, elapsed: u32) -> Option<InvariantKind> {
    evaluate(INVARIANTS, &sample(previous, current, elapsed))
        .err()
        .map(|violation| violation.kind)
}

#[test]
fn a_first_sample_is_never_an_accusation() {
    // Except for the one check that needs no history. An entity entering
    // interest range brings no previous sample with it, and flagging that
    // would flag every peer that ever crossed a cell boundary.
    let far = craft_at(Archetype::Interceptor, 9_000_000_000);
    assert_eq!(kind(None, &far, 0), None);
}

#[test]
fn an_impossible_velocity_needs_no_history_at_all() {
    let mut speeding = craft_at(Archetype::Interceptor, 0);
    speeding.vel = QVel {
        x: Archetype::Interceptor.limits().max_speed_mms * 2,
        y: 0,
        z: 0,
    };
    assert_eq!(kind(None, &speeding, 0), Some(InvariantKind::SpeedCap));
}

#[test]
fn a_jump_is_a_teleport_only_relative_to_the_time_that_passed() {
    let here = craft_at(Archetype::Interceptor, 0);
    // 100 m in one sample interval: impossible at 120 m/s over 3 ticks.
    let there = craft_at(Archetype::Interceptor, 100_000);
    assert_eq!(kind(Some(&here), &there, 3), Some(InvariantKind::Teleport));
    // The same displacement after a two-second gap is ordinary travel, and a
    // check without the time term would accuse every peer coming back from a
    // loss burst.
    assert_eq!(kind(Some(&here), &there, 120), None);
}

#[test]
fn counters_may_not_run_backwards_and_hulls_may_not_go_negative() {
    let mut fired = craft_at(Archetype::Interceptor, 0);
    fired.shots = 10;
    let mut unfired = craft_at(Archetype::Interceptor, 0);
    unfired.shots = 9;
    assert_eq!(
        kind(Some(&fired), &unfired, 3),
        Some(InvariantKind::ValueRange)
    );

    let mut impossible = craft_at(Archetype::Interceptor, 0);
    impossible.hull = -1;
    assert_eq!(kind(None, &impossible, 0), Some(InvariantKind::ValueRange));
}

#[test]
fn an_entity_may_not_relabel_its_own_archetype() {
    // The cheat this catches is not "become a cruiser"; it is handing every
    // other check a limit table the authority never ran.
    let interceptor = craft_at(Archetype::Interceptor, 0);
    let cruiser = craft_at(Archetype::Cruiser, 0);
    assert_eq!(
        kind(Some(&interceptor), &cruiser, 3),
        Some(InvariantKind::ValueRange)
    );
}
