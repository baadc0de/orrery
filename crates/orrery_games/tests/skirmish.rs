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
use orrery_games::game::{Game, Tamper};
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

/// Damage as the [`Skirmish::deliver`] bridge would produce it: the attacker's
/// identity, and the position and archetype the target derives reach from.
fn damage_from(from: u64, archetype: Archetype, x_mm: i64, amount: i32) -> Order {
    Order::Damage {
        amount,
        from: PersistId::new(from),
        from_pos: QPos {
            x: x_mm,
            y: 0,
            z: 0,
        },
        from_archetype: archetype,
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
fn a_shot_beyond_reach_lands_on_nobody() {
    // Reach is enforced in the *target's* step, not the shooter's. The shot
    // is real — the round left the barrel, the roll is in the shooter's own
    // state, the weapon is hot — and it simply never arrives. Resolving it
    // the other way would mean the shooter read the target's live position,
    // which the single-entity world an adjudicator builds cannot supply.
    let reach = Archetype::Interceptor.limits().range_mm;
    let mut world = world(&[
        (1, craft_at(Archetype::Interceptor, 0)),
        (2, craft_at(Archetype::Interceptor, reach + 1_000)),
    ]);
    let outcome = world
        .step_entity(PersistId::new(1), Tick::new(T), &[fire_at(2)])
        .expect("entity 1 is installed");

    let shooter = world.state(PersistId::new(1)).unwrap();
    assert_eq!(shooter.shots, 1, "the shooter fired");
    assert_eq!(
        shooter.cooldown,
        Archetype::Interceptor.limits().cooldown_ticks,
        "and paid the cooldown for it, in reach or not"
    );

    let (target, damage) = Skirmish::honest()
        .deliver(outcome.events.first().expect("a shot was emitted"))
        .expect("damage is addressed to its target");
    let before = world.state(target).unwrap().clone();
    world
        .step_entity(target, Tick::new(T + 1), &[damage])
        .expect("entity 2 is installed");
    let after = world.state(target).unwrap();
    assert_eq!(
        (after.hull, after.shield),
        (before.hull, before.shield),
        "out of reach, so nothing arrives"
    );
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
                damage_from(2, Archetype::Interceptor, 100_000, 10),
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
            &[damage_from(
                2,
                Archetype::Interceptor,
                100_000,
                limits.max_shield + 10,
            )],
        )
        .expect("entity 1 is installed");
    let craft = world.state(entity).unwrap();
    assert_eq!(craft.shield, 0);
    assert_eq!(craft.hull, limits.max_hull - 10);

    let outcome = world
        .step_entity(
            entity,
            Tick::new(T + 1),
            &[damage_from(
                2,
                Archetype::Interceptor,
                100_000,
                limits.max_hull * 10,
            )],
        )
        .expect("entity 1 is installed");
    assert_eq!(world.state(entity).unwrap().hull, 0, "never negative");
    assert_eq!(
        outcome.events,
        vec![Outcome::Destroyed {
            by: PersistId::new(2)
        }]
    );
}

#[test]
fn every_effect_names_who_caused_it() {
    // The executor tells a step which entity it is, so a shot can be
    // attributed to its shooter and a kill to its killer. Without that, a
    // disputed kill would have no window to re-execute and a P5 kill-credit
    // intent would have nobody to attach to.
    let mut world = world(&[
        (1, craft_at(Archetype::Cruiser, 0)),
        (2, craft_at(Archetype::Interceptor, 100_000)),
    ]);

    let shot = world
        .step_entity(PersistId::new(1), Tick::new(T), &[fire_at(2)])
        .expect("entity 1 is installed");
    let Some(
        event @ Outcome::DamageDealt {
            attacker,
            target,
            amount,
            attacker_pos,
            attacker_archetype,
        },
    ) = shot.events.first().cloned()
    else {
        panic!("a cruiser should have fired: {:?}", shot.events)
    };
    assert_eq!(
        attacker,
        PersistId::new(1),
        "the shooter signs its own shot"
    );
    assert_eq!(target, PersistId::new(2));
    // Where it was fired from and what fired it, because that is what the
    // target resolves reach against — and deriving reach from the archetype
    // rather than trusting a scalar is what keeps the attacker from granting
    // itself a longer gun.
    assert_eq!(attacker_pos, QPos { x: 0, y: 0, z: 0 });
    assert_eq!(attacker_archetype, Archetype::Cruiser);

    // And the damage arrives carrying the same name, which is what lets the
    // victim's own log say who killed it.
    let delivered = Skirmish::honest()
        .deliver(&event)
        .expect("damage is delivered to its target");
    assert_eq!(
        delivered,
        (
            PersistId::new(2),
            Order::Damage {
                amount,
                from: PersistId::new(1),
                from_pos: QPos { x: 0, y: 0, z: 0 },
                from_archetype: Archetype::Cruiser,
            }
        )
    );
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

/// The premise of #758's decision not to bump `SKIRMISH_RULESET`.
///
/// Snapshot isolation changed what a neighbour read returns. Skirmish declares
/// it makes none — `max_neighbor_reads()` is the trait default of zero, and
/// the executor's budget is what a rule is held to — so no Skirmish tick can
/// reach the changed path and no two builds either side of #758 can disagree.
/// If this ever fails, Skirmish acquired neighbour reads and the version
/// decision is due for review, not for inheritance.
#[test]
fn skirmish_declares_no_neighbour_reads() {
    assert_eq!(
        Skirmish::honest().max_neighbor_reads(),
        0,
        "Skirmish now reads neighbours, so #758's argument for leaving \
         SKIRMISH_RULESET at v{} no longer holds",
        SKIRMISH_RULESET.version
    );
}
