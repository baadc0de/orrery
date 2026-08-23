//! Regolith-specific checks for weapon state and planar input discipline.

use orrery_core::{evaluate, CoreCodec, Executor, InvariantKind, InvariantSample, QPos, QVel};
use orrery_games::game::Game;
use orrery_games::regolith::{
    archetype::Archetype,
    invariants::INVARIANTS,
    order::{Order, Outcome},
    state::{Craft, Pickup, RegolithState, Rock, RockTier},
    weapon::WeaponKind,
    Regolith, PICKUP_TTL_TICKS, REGOLITH_RULESET,
};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use rand_chacha::rand_core::SeedableRng;

fn craft_at(x: i64) -> Craft {
    Craft::spawned(Archetype::Interceptor, QPos { x, y: 0, z: 0 }, 0)
}
fn sample<'a>(
    previous: Option<&'a RegolithState>,
    current: &'a RegolithState,
) -> InvariantSample<'a, RegolithState> {
    InvariantSample {
        entity: PersistId::new(1),
        current,
        tick: Tick::new(1),
        previous,
        elapsed_ticks: 3,
    }
}

#[test]
fn v3_weapon_table_and_ruleset_identity_are_pinned() {
    assert_eq!(REGOLITH_RULESET.version, 3);
    assert_eq!(WeaponKind::Stock.weapon().damage_base, 10);
    assert_eq!(WeaponKind::Volley.weapon().rolls, 3);
    assert_eq!(WeaponKind::Heavy.weapon().reach_mm, 900_000);
}

#[test]
fn relabelled_weapon_without_matching_hashed_state_fails_stage_one() {
    let honest = RegolithState::Craft(craft_at(0));
    let mut relabelled = honest.clone();
    // Mutate the guarded state field only. The invariant itself stays intact.
    let RegolithState::Craft(craft) = &mut relabelled else {
        panic!("test constructed a craft")
    };
    craft.weapon = WeaponKind::Heavy;
    let violation = evaluate(INVARIANTS, &sample(Some(&honest), &relabelled))
        .expect_err("weapon relabel must fail");
    assert_eq!(violation.kind, InvariantKind::ValueRange);
    assert_eq!(violation.validator, "regolith/value-range");
}

#[test]
fn volley_is_three_left_slot_first_rolls_and_uses_its_own_cooldown() {
    let mut shooter = craft_at(0);
    shooter.weapon = WeaponKind::Volley;
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([9; 32]));
    executor.insert(PersistId::new(1), RegolithState::Craft(shooter));
    executor.insert(PersistId::new(2), RegolithState::Craft(craft_at(1)));
    let output = executor
        .step_entity(
            PersistId::new(1),
            Tick::new(1),
            &[Order::Fire {
                target: PersistId::new(2),
            }],
        )
        .unwrap();
    assert_eq!(output.events.len(), 3);
    assert!(output.events.iter().all(|event| matches!(
        event,
        Outcome::DamageDealt {
            attacker_weapon: WeaponKind::Volley,
            ..
        }
    )));
    assert!(matches!(
        executor.state(PersistId::new(1)),
        Some(RegolithState::Craft(Craft { cooldown: 30, .. }))
    ));
}

#[test]
fn large_split_is_slot_ordered_materialized_and_traced() {
    let parent = PersistId::new(77);
    let rock = Rock::spawned(
        RockTier::Large,
        0,
        QPos::default(),
        QVel {
            x: 20_000,
            y: 0,
            z: 0,
        },
    );
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([3; 32]));
    executor.insert(parent, RegolithState::Rock(rock));
    let output = executor
        .step_entity(
            parent,
            Tick::new(9),
            &[Order::Damage {
                amount: 40,
                from: PersistId::new(1),
                from_pos: QPos::default(),
                from_weapon: WeaponKind::Stock,
            }],
        )
        .expect("parent exists");
    let Outcome::Split {
        parent: emitted_parent,
        generation,
        children,
    } = &output.events[0]
    else {
        panic!("lethal large rock damage must split")
    };
    assert_eq!((*emitted_parent, *generation), (parent, 0));
    assert_eq!(children[0].tier, RockTier::Medium);
    assert_eq!(children[1].tier, RockTier::Medium);
    assert_ne!(children[0].id, children[1].id);
    assert_eq!(output.materialized, vec![children[0].id, children[1].id]);
    assert!(matches!(
        executor.state(parent),
        Some(RegolithState::Rock(Rock {
            hull: 0,
            splits_done: 1,
            ..
        }))
    ));
}

#[test]
fn split_replay_uses_derived_ids_not_creation_order() {
    let run = |filler: Option<PersistId>| {
        let parent = PersistId::new(77);
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([3; 32]));
        if let Some(filler) = filler {
            executor.insert(filler, RegolithState::Craft(craft_at(99)));
        }
        executor.insert(
            parent,
            RegolithState::Rock(Rock::spawned(
                RockTier::Large,
                0,
                QPos::default(),
                QVel {
                    x: 20_000,
                    y: 0,
                    z: 0,
                },
            )),
        );
        executor
            .step_entity(
                parent,
                Tick::new(9),
                &[Order::Damage {
                    amount: 40,
                    from: PersistId::new(1),
                    from_pos: QPos::default(),
                    from_weapon: WeaponKind::Stock,
                }],
            )
            .expect("parent exists")
    };
    let alone = run(None);
    let with_prior_creation = run(Some(PersistId::new(2)));
    assert_eq!(alone.events, with_prior_creation.events);
    assert_eq!(alone.materialized, with_prior_creation.materialized);
    assert_eq!(alone.state_hash, with_prior_creation.state_hash);
    let expected = |slot: u8| {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"regolith-rock");
        hasher.update(&77u64.to_le_bytes());
        hasher.update(&0u32.to_le_bytes());
        hasher.update(&[slot]);
        PersistId::new(u64::from_le_bytes(
            hasher.finalize().as_bytes()[..8]
                .try_into()
                .expect("digest prefix"),
        ))
    };
    assert_eq!(alone.materialized, vec![expected(0), expected(1)]);
}

#[test]
fn honest_pilot_keeps_pitch_locked_to_zero() {
    let game = Regolith::honest();
    let mut orders = Vec::new();
    let mut rng = orrery_core::TickRng::from_seed([1; 32]);
    game.honest_inputs(
        PersistId::new(1),
        0,
        Tick::new(1),
        &[],
        &mut rng,
        &mut orders,
    );
    assert!(matches!(
        orders.first(),
        Some(Order::Thrust { pitch_urad: 0, .. })
    ));
}

fn run_contest() -> (
    [orrery_core::TickOutcome<Outcome>; 3],
    Vec<Outcome>,
    [RegolithState; 3],
) {
    let game = Regolith::honest();
    // Higher id arrives first: sorting by entity id instead of preserving VC-2
    // log order would incorrectly give the pickup to `loser`.
    let winner = PersistId::new(2);
    let loser = PersistId::new(1);
    let pickup = PersistId::new(3);
    let mut executor = Executor::new(game, UniverseSeed([0xC3; 32]));
    executor.insert(winner, RegolithState::Craft(craft_at(0)));
    executor.insert(loser, RegolithState::Craft(craft_at(20_000)));
    executor.insert(
        pickup,
        RegolithState::Pickup(Pickup::spawned(
            QPos {
                x: 10_000,
                y: 0,
                z: 0,
            },
            WeaponKind::Heavy,
            PICKUP_TTL_TICKS,
        )),
    );

    let winner_attempt = executor
        .step_entity(winner, Tick::new(10), &[Order::Grab { pickup }])
        .expect("winner exists");
    let loser_attempt = executor
        .step_entity(loser, Tick::new(10), &[Order::Grab { pickup }])
        .expect("loser exists");
    let attempts = winner_attempt
        .events
        .iter()
        .chain(&loser_attempt.events)
        .map(|event| game.deliver(event).expect("attempt is delivered").1)
        .collect::<Vec<_>>();
    let pickup_outcome = executor
        .step_entity(pickup, Tick::new(11), &attempts)
        .expect("pickup exists");
    let resolutions = pickup_outcome
        .events
        .iter()
        .map(|event| game.deliver(event).expect("resolution is delivered"))
        .collect::<Vec<_>>();
    let winner_resolution = resolutions
        .iter()
        .find(|(target, _)| *target == winner)
        .expect("winner resolution")
        .1
        .clone();
    let loser_resolution = resolutions
        .iter()
        .find(|(target, _)| *target == loser)
        .expect("loser resolution")
        .1
        .clone();
    let winner_outcome = executor
        .step_entity(winner, Tick::new(12), &[winner_resolution])
        .expect("winner exists");
    let loser_outcome = executor
        .step_entity(loser, Tick::new(12), &[loser_resolution])
        .expect("loser exists");

    (
        [winner_outcome, loser_outcome, pickup_outcome],
        resolutions
            .into_iter()
            .map(|(_, input)| match input {
                Order::PickupGranted { kind } => Outcome::Granted { ship: winner, kind },
                Order::PickupDenied => Outcome::Denied { ship: loser },
                _ => unreachable!("only pickup resolutions"),
            })
            .collect(),
        [winner, loser, pickup].map(|entity| {
            executor
                .state(entity)
                .expect("contest entity exists")
                .clone()
        }),
    )
}

#[test]
fn contested_grab_replay_is_ordered_and_each_party_hashes_its_own_side() {
    let (first, first_events, states) = run_contest();
    let (replay, replay_events, replay_states) = run_contest();
    assert_eq!(first_events, replay_events);
    assert_eq!(
        first_events,
        vec![
            Outcome::Granted {
                ship: PersistId::new(2),
                kind: WeaponKind::Heavy,
            },
            Outcome::Denied {
                ship: PersistId::new(1),
            },
        ]
    );
    let hashes = first.map(|outcome| outcome.state_hash);
    assert_eq!(hashes, replay.map(|outcome| outcome.state_hash));
    assert_eq!(hashes, orrery_games::golden::REGOLITH_PICKUP_CONTEST);
    assert_eq!(states, replay_states);
    assert!(matches!(
        &states[0],
        RegolithState::Craft(Craft {
            weapon: WeaponKind::Heavy,
            grabs_attempted: 1,
            pickups_won: 1,
            grabs_lost: 0,
            ..
        })
    ));
    assert!(matches!(
        &states[1],
        RegolithState::Craft(Craft {
            weapon: WeaponKind::Stock,
            grabs_attempted: 1,
            pickups_won: 0,
            grabs_lost: 1,
            ..
        })
    ));
    assert!(matches!(
        &states[2],
        RegolithState::Pickup(Pickup {
            claimed_by: Some(entity),
            claimed_at: Some(1),
            ..
        }) if *entity == PersistId::new(2)
    ));
}

#[test]
fn ttl_expiry_replays_and_denies_a_late_grab() {
    let run = || {
        let pickup = PersistId::new(9);
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([7; 32]));
        executor.insert(
            pickup,
            RegolithState::Pickup(Pickup::spawned(QPos::default(), WeaponKind::Volley, 2)),
        );
        let first = executor
            .step_entity(pickup, Tick::new(20), &[])
            .expect("pickup exists");
        assert!(first.events.is_empty());
        executor
            .step_entity(
                pickup,
                Tick::new(21),
                &[Order::GrabAttempt {
                    ship: PersistId::new(1),
                    ship_pos: QPos::default(),
                }],
            )
            .expect("pickup exists")
    };
    let first = run();
    let replay = run();
    assert_eq!(first, replay);
    assert_eq!(
        first.events,
        vec![
            Outcome::Expired {
                id: PersistId::new(9),
            },
            Outcome::Denied {
                ship: PersistId::new(1),
            },
        ]
    );
}

#[test]
fn small_drop_is_derived_materialized_and_traced() {
    let rock_id = PersistId::new(77);
    let run = |tick| {
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([3; 32]));
        executor.insert(
            rock_id,
            RegolithState::Rock(Rock::spawned(
                RockTier::Small,
                2,
                QPos::default(),
                QVel::default(),
            )),
        );
        let outcome = executor
            .step_entity(
                rock_id,
                Tick::new(tick),
                &[Order::Damage {
                    amount: 5,
                    from: PersistId::new(1),
                    from_pos: QPos::default(),
                    from_weapon: WeaponKind::Stock,
                }],
            )
            .expect("rock exists");
        (executor, outcome)
    };
    let (executor, outcome) = (0..100)
        .map(run)
        .find(|(_, outcome)| !outcome.materialized.is_empty())
        .expect("the pinned RNG stream contains a normal 25% drop");
    let Outcome::SpawnPickup {
        id,
        pos,
        kind,
        expires_at,
    } = outcome.events[0]
    else {
        panic!("Small death must fully describe its drop")
    };
    assert_eq!(pos, QPos::default());
    assert!(matches!(kind, WeaponKind::Volley | WeaponKind::Heavy));
    assert_eq!(expires_at, PICKUP_TTL_TICKS);
    assert_eq!(outcome.materialized, vec![id]);
    assert!(matches!(
        executor.state(rock_id),
        Some(RegolithState::Rock(Rock {
            hull: 0,
            pickups_dropped: 1,
            ..
        }))
    ));
    assert!(matches!(
        executor.state(id),
        Some(RegolithState::Pickup(Pickup {
            ttl_remaining: PICKUP_TTL_TICKS,
            ..
        }))
    ));
}

#[test]
fn pickup_state_and_grammar_are_canonical() {
    let pickup = PersistId::new(9);
    let ship = PersistId::new(2);
    let pos = QPos {
        x: 10,
        y: -20,
        z: 30,
    };
    let states = [
        RegolithState::Craft(craft_at(0)),
        RegolithState::Rock(Rock::spawned(RockTier::Small, 2, pos, QVel::default())),
        RegolithState::Pickup(Pickup::spawned(pos, WeaponKind::Volley, PICKUP_TTL_TICKS)),
    ];
    for state in states {
        assert_eq!(RegolithState::decode(&state.to_canonical()).unwrap(), state);
    }
    let orders = [
        Order::Grab { pickup },
        Order::GrabAttempt {
            ship,
            ship_pos: pos,
        },
        Order::PickupGranted {
            kind: WeaponKind::Heavy,
        },
        Order::PickupDenied,
    ];
    for order in orders {
        assert_eq!(Order::decode(&order.to_canonical()).unwrap(), order);
    }
    let outcomes = [
        Outcome::SpawnPickup {
            id: pickup,
            pos,
            kind: WeaponKind::Volley,
            expires_at: PICKUP_TTL_TICKS,
        },
        Outcome::GrabAttempted {
            pickup,
            ship,
            ship_pos: pos,
        },
        Outcome::Granted {
            ship,
            kind: WeaponKind::Volley,
        },
        Outcome::Denied { ship },
        Outcome::Expired { id: pickup },
    ];
    for outcome in outcomes {
        assert_eq!(Outcome::decode(&outcome.to_canonical()).unwrap(), outcome);
    }
}
