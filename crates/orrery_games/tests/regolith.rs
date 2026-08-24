//! Regolith-specific checks for weapon state and planar input discipline.

use orrery_core::{
    evaluate, tick_rng, CoreCodec, Executor, InvariantKind, InvariantSample, QPos, QVel,
};
use orrery_games::game::Game;
use orrery_games::regolith::{
    archetype::Archetype,
    invariants::INVARIANTS,
    order::{ChildSpec, LockBreakReason, Order, Outcome, ShotResult},
    pilot::{scenario_at, PilotScenario, PILOT_SCENARIOS, SCENARIO_TICKS},
    state::{BloomDirector, BloomMembership, Craft, Pickup, RegolithState, Rock, RockTier},
    weapon::WeaponKind,
    Regolith, BLOOM_CADENCE_TICKS, BLOOM_CENTRAL_RADIUS_MM, BLOOM_LIFETIME_TICKS, BLOOM_ROCK_COUNT,
    ISLAND_CRAFT_BUDGET, ISLAND_DIRECTOR_BUDGET, ISLAND_PICKUP_BUDGET, ISLAND_ROCK_BUDGET,
    ISLAND_WINDOW_BUDGET, KILL_SCORE_POINTS, LOCK_ACQUISITION_TICKS, PICKUP_SCORE_POINTS,
    PICKUP_TTL_TICKS, REGOLITH_RULESET, RESPAWN_TICKS,
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
fn v8_weapon_table_ruleset_identity_and_island_budget_are_pinned() {
    assert_eq!(REGOLITH_RULESET.version, 8);
    assert_eq!(WeaponKind::Stock.weapon().damage_base, 10);
    assert_eq!(WeaponKind::Volley.weapon().rolls, 3);
    assert_eq!(WeaponKind::Stock.weapon().optimal_mm, 300_000);
    assert_eq!(WeaponKind::Volley.weapon().tracking_urad_per_sec, 300_000);
    assert_eq!(WeaponKind::Heavy.weapon().falloff_mm, 200_000);
    assert_eq!(WeaponKind::Heavy.weapon().projectile_speed_mms, 180_000);
    assert_eq!(
        [WeaponKind::Stock, WeaponKind::Volley, WeaponKind::Heavy].map(|kind| {
            let weapon = kind.weapon();
            weapon.optimal_mm + weapon.falloff_mm
        }),
        [400_000, 300_000, 900_000]
    );
    assert_eq!(ISLAND_CRAFT_BUDGET, 8);
    assert_eq!(ISLAND_ROCK_BUDGET, 24);
    assert_eq!(ISLAND_PICKUP_BUDGET, 4);
    assert_eq!(ISLAND_DIRECTOR_BUDGET, 1);
    assert_eq!(ISLAND_WINDOW_BUDGET, 37);
    assert_eq!((KILL_SCORE_POINTS, PICKUP_SCORE_POINTS), (25, 5));
}

fn pilot_orders(seed: u8, entity: u64, slot: u64, tick: u64) -> Vec<Order> {
    let seed = UniverseSeed([seed; 32]);
    let entity = PersistId::new(entity);
    let at = Tick::new(tick);
    let peers = [PersistId::new(11), PersistId::new(12), PersistId::new(13)];
    let mut rng = tick_rng(seed, entity, at);
    let mut orders = Vec::new();
    Regolith::honest().honest_inputs(entity, slot, at, &peers, &mut rng, &mut orders);
    orders
}

#[test]
fn pilot_scenario_table_covers_the_four_durable_surfaces() {
    assert_eq!(
        PILOT_SCENARIOS.map(PilotScenario::name),
        ["combat", "mining", "contested-grab", "bloom-convergence"]
    );
    for (index, scenario) in PILOT_SCENARIOS.into_iter().enumerate() {
        let tick = Tick::new(index as u64 * SCENARIO_TICKS);
        assert_eq!(scenario_at(tick), scenario);
        let orders = pilot_orders(0x61, 1, 0, tick.0);
        assert_eq!(
            orders
                .iter()
                .filter(|order| matches!(order, Order::Fire { .. }))
                .count(),
            1,
            "{} must hold the trigger",
            scenario.name()
        );
        assert!(matches!(
            orders.first(),
            Some(Order::Thrust { pitch_urad: 0, .. })
        ));
        for order in orders {
            assert_eq!(Order::decode(&order.to_canonical()).unwrap(), order);
        }
    }
}

#[test]
fn adjacent_slots_deliberately_contest_one_pickup() {
    let tick = 2 * SCENARIO_TICKS;
    let pickup = |slot| {
        pilot_orders(0x61, slot + 1, slot, tick)
            .into_iter()
            .find_map(|order| match order {
                Order::Grab { pickup } => Some(pickup),
                _ => None,
            })
            .expect("the contested-grab row emits a grab")
    };
    assert_eq!(pickup(0), pickup(1));
    assert_ne!(pickup(1), pickup(2));
}

#[test]
fn pilot_is_pure_in_seed_entity_slot_and_tick() {
    let first = pilot_orders(0x61, 7, 3, 4 * SCENARIO_TICKS + 17);
    let second = pilot_orders(0x61, 7, 3, 4 * SCENARIO_TICKS + 17);
    assert_eq!(first, second);
}

#[test]
fn bloom_director_replays_in_isolation_without_neighbor_reads() {
    let director_id = PersistId::new(700);
    let neighbor_id = PersistId::new(99);
    let ready = BloomDirector {
        clock_tick: BLOOM_CADENCE_TICKS - 1,
        ..BloomDirector::spawned()
    };
    let run = |with_neighbor: bool| {
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xB1; 32]));
        executor.insert(director_id, RegolithState::BloomDirector(ready.clone()));
        if with_neighbor {
            executor.insert(neighbor_id, RegolithState::Craft(craft_at(123)));
        }
        let outcome = executor
            .step_entity(director_id, Tick::new(9_000), &[])
            .expect("director exists");
        let state = executor
            .state(director_id)
            .expect("director remains")
            .clone();
        (outcome, state, executor)
    };

    let (populated, populated_state, populated_executor) = run(true);
    let (isolated, isolated_state, _) = run(false);
    assert_eq!(populated, isolated);
    assert_eq!(populated_state, isolated_state);
    assert!(populated.neighbor_reads.is_empty());
    assert_eq!(populated.materialized.len(), usize::from(BLOOM_ROCK_COUNT));

    let Outcome::BloomSeeded {
        director,
        bloom_index,
        site_pos,
        active_until,
        rocks,
    } = &populated.events[0]
    else {
        panic!("the cadence tick must seed one bloom")
    };
    assert_eq!((*director, *bloom_index), (director_id, 0));
    assert_eq!(*active_until, BLOOM_CADENCE_TICKS + BLOOM_LIFETIME_TICKS);
    assert_eq!(site_pos.y, 0);
    assert!(site_pos.x.abs() <= BLOOM_CENTRAL_RADIUS_MM);
    assert!(site_pos.z.abs() <= BLOOM_CENTRAL_RADIUS_MM);
    assert_eq!(
        rocks.iter().map(|rock| rock.tier).collect::<Vec<_>>(),
        vec![
            RockTier::Large,
            RockTier::Large,
            RockTier::Medium,
            RockTier::Medium,
            RockTier::Medium,
            RockTier::Small,
            RockTier::Small,
            RockTier::Small,
            RockTier::Small,
            RockTier::Small,
        ]
    );
    for rock in rocks.iter() {
        assert!(matches!(
            populated_executor.state(rock.id),
            Some(RegolithState::Rock(Rock {
                born_in_bloom: true,
                bloom: Some(_),
                ..
            }))
        ));
    }
}

#[test]
fn kill_credit_is_log_delivered_and_replays_from_the_killers_input() {
    let game = Regolith::honest();
    let killer = PersistId::new(1);
    let victim = PersistId::new(2);
    let mut killer_start = craft_at(0);
    killer_start.lock_target = Some(victim);
    killer_start.lock_progress = LOCK_ACQUISITION_TICKS;
    killer_start.locks_acquired = 1;
    let mut victim_state = craft_at(0);
    victim_state.hull = 1;
    victim_state.shield = 0;
    let mut live = Executor::new(game, UniverseSeed([0xC1; 32]));
    live.insert(killer, RegolithState::Craft(killer_start.clone()));
    live.insert(victim, RegolithState::Craft(victim_state));

    let fired = live
        .step_entity(killer, Tick::new(1), &[Order::Fire { target: victim }])
        .expect("killer exists");
    let damage = game
        .deliver(&fired.events[0])
        .expect("damage is delivered")
        .1;
    live.step_entity(killer, Tick::new(2), &[])
        .expect("killer advances each tick");
    let destroyed = live
        .step_entity(victim, Tick::new(2), &[damage])
        .expect("victim exists");
    let destroyed = destroyed
        .events
        .iter()
        .find(|event| matches!(event, Outcome::Destroyed { .. }))
        .expect("lethal logged damage emits Destroyed");
    let (credit_target, credit_input) = game
        .deliver(destroyed)
        .expect("Destroyed credit must enter the killer's log through deliver");
    assert_eq!(credit_target, killer);
    assert_eq!(credit_input, Order::KillCredit);
    let credited = live
        .step_entity(killer, Tick::new(3), core::slice::from_ref(&credit_input))
        .expect("killer exists");

    let mut replay = Executor::new(game, UniverseSeed([0xC1; 32]));
    replay.insert(killer, RegolithState::Craft(killer_start.clone()));
    replay
        .step_entity(killer, Tick::new(1), &[Order::Fire { target: victim }])
        .expect("isolated killer exists");
    replay
        .step_entity(killer, Tick::new(2), &[])
        .expect("isolated killer advances each tick");
    let replayed = replay
        .step_entity(killer, Tick::new(3), &[credit_input])
        .expect("isolated killer consumes its logged credit");
    assert_eq!(credited.state_hash, replayed.state_hash);

    let mut replay_without_delivery = Executor::new(game, UniverseSeed([0xC1; 32]));
    replay_without_delivery.insert(killer, RegolithState::Craft(killer_start));
    replay_without_delivery
        .step_entity(killer, Tick::new(1), &[Order::Fire { target: victim }])
        .expect("isolated killer exists");
    replay_without_delivery
        .step_entity(killer, Tick::new(2), &[])
        .expect("isolated killer advances each tick");
    let missing_credit = replay_without_delivery
        .step_entity(killer, Tick::new(3), &[])
        .expect("replay without the delivery still runs");
    assert_ne!(credited.state_hash, missing_credit.state_hash);
    assert!(matches!(
        replay_without_delivery.state(killer),
        Some(RegolithState::Craft(Craft { kills: 0, .. }))
    ));
    assert!(matches!(
        live.state(killer),
        Some(RegolithState::Craft(Craft { kills: 1, .. }))
    ));
    let RegolithState::Craft(craft) = replay.state(killer).expect("replayed killer") else {
        panic!("killer is a craft")
    };
    assert_eq!(craft.kills, 1);
    assert_eq!(craft.score(), KILL_SCORE_POINTS);
}

#[test]
fn wreck_respawns_after_120_own_ticks_with_stock_and_score_intact() {
    let craft_id = PersistId::new(2);
    let mut craft = craft_at(80_000);
    craft.weapon = WeaponKind::Heavy;
    craft.hull = 1;
    craft.shield = 0;
    craft.kills = 2;
    craft.pickups_won = 3;
    let expected_score = 2 * KILL_SCORE_POINTS + 3 * PICKUP_SCORE_POINTS;
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xD1; 32]));
    executor.insert(craft_id, RegolithState::Craft(craft));
    executor
        .step_entity(
            craft_id,
            Tick::new(1),
            &[Order::Damage {
                amount: 1,
                from: PersistId::new(8),
                from_pos: QPos {
                    x: 80_000,
                    y: 0,
                    z: 0,
                },
                from_weapon: WeaponKind::Stock,
                from_vel: QVel::default(),
                flight_ticks: Some(1),
            }],
        )
        .expect("craft exists");
    assert!(matches!(
        executor.state(craft_id),
        Some(RegolithState::Craft(Craft {
            hull: 0,
            respawn_in: RESPAWN_TICKS,
            ..
        }))
    ));
    for tick in 2..=120 {
        executor
            .step_entity(craft_id, Tick::new(tick), &[])
            .expect("wreck owns its countdown");
    }
    assert!(matches!(
        executor.state(craft_id),
        Some(RegolithState::Craft(Craft {
            hull: 0,
            respawn_in: 1,
            ..
        }))
    ));
    executor
        .step_entity(craft_id, Tick::new(121), &[])
        .expect("120th wreck tick respawns");
    let expected = Regolith::honest().spawn(craft_id, 1);
    let Some(RegolithState::Craft(respawned)) = executor.state(craft_id) else {
        panic!("respawned state is a craft")
    };
    let RegolithState::Craft(expected) = expected else {
        unreachable!()
    };
    assert_eq!(respawned.pos, expected.pos);
    assert_eq!(respawned.vel, QVel::default());
    assert_eq!(respawned.weapon, WeaponKind::Stock);
    let limits = respawned.archetype.limits();
    assert_eq!(
        (respawned.hull, respawned.shield),
        (limits.max_hull, limits.max_shield)
    );
    assert_eq!(respawned.score(), expected_score);
}

#[test]
fn rock_credit_is_log_delivered_with_resolver_owned_points() {
    let game = Regolith::honest();
    let killer = PersistId::new(1);
    let rock_id = PersistId::new(20);
    let mut executor = Executor::new(game, UniverseSeed([0xA4; 32]));
    executor.insert(killer, RegolithState::Craft(craft_at(0)));
    executor.insert(
        rock_id,
        RegolithState::Rock(Rock::spawned(
            RockTier::Small,
            2,
            QPos::default(),
            QVel::default(),
        )),
    );
    let destroyed = executor
        .step_entity(
            rock_id,
            Tick::new(1),
            &[Order::Damage {
                amount: 5,
                from: killer,
                from_pos: QPos::default(),
                from_weapon: WeaponKind::Stock,
                from_vel: QVel::default(),
                flight_ticks: Some(1),
            }],
        )
        .expect("rock exists");
    let credit = destroyed
        .events
        .iter()
        .filter(|event| matches!(event, Outcome::RockDestroyed { .. }))
        .find_map(|event| game.deliver(event))
        .expect("RockDestroyed is delivered");
    assert_eq!(credit, (killer, Order::RockCredit { points: 1 }));
    executor
        .step_entity(killer, Tick::new(2), &[credit.1])
        .expect("killer consumes rock credit");
    let Some(RegolithState::Craft(killer)) = executor.state(killer) else {
        panic!("killer remains a craft")
    };
    assert_eq!(killer.score_rock_points, 1);
    assert_eq!(killer.score(), 1);
}

#[test]
fn score_rate_and_director_population_ceilings_are_stage_one_checked() {
    let previous = RegolithState::Craft(craft_at(0));
    let mut impossible_score = previous.clone();
    let RegolithState::Craft(craft) = &mut impossible_score else {
        unreachable!()
    };
    craft.kills = u32::from(ISLAND_CRAFT_BUDGET) * 3 + 1;
    let violation = evaluate(INVARIANTS, &sample(Some(&previous), &impossible_score))
        .expect_err("more than eight kill deliveries per tick is impossible");
    assert_eq!(violation.kind, InvariantKind::RateLimit);
    assert_eq!(violation.validator, "regolith/score-rate");

    let impossible_population = RegolithState::BloomDirector(BloomDirector {
        clock_tick: BLOOM_CADENCE_TICKS,
        next_bloom_tick: BLOOM_CADENCE_TICKS * 2,
        blooms_seeded: 1,
        site_pos: Some(QPos::default()),
        site_active_until: Some(BLOOM_CADENCE_TICKS + BLOOM_LIFETIME_TICKS),
        site_rocks_alive: 20,
    });
    let violation = evaluate(INVARIANTS, &sample(None, &impossible_population))
        .expect_err("one bloom cannot have twenty live descendants");
    assert_eq!(violation.kind, InvariantKind::ValueRange);
    assert_eq!(violation.validator, "regolith/value-range");
}

#[test]
fn site_closes_from_logged_population_delivery_without_neighbor_reads() {
    let game = Regolith::honest();
    let director_id = PersistId::new(700);
    let population_event = Outcome::BloomPopulationChanged {
        director: director_id,
        bloom_index: 0,
        delta: -1,
    };
    let (target, input) = game
        .deliver(&population_event)
        .expect("rock population events route through the log");
    assert_eq!(target, director_id);
    let mut executor = Executor::new(game, UniverseSeed([0xE1; 32]));
    executor.insert(
        director_id,
        RegolithState::BloomDirector(BloomDirector {
            clock_tick: BLOOM_CADENCE_TICKS,
            next_bloom_tick: BLOOM_CADENCE_TICKS * 2,
            blooms_seeded: 1,
            site_pos: Some(QPos::default()),
            site_active_until: Some(BLOOM_CADENCE_TICKS + BLOOM_LIFETIME_TICKS),
            site_rocks_alive: BLOOM_ROCK_COUNT,
        }),
    );
    let inputs = vec![input; usize::from(BLOOM_ROCK_COUNT)];
    let outcome = executor
        .step_entity(director_id, Tick::new(1), &inputs)
        .expect("director consumes logged population changes");
    assert!(outcome.neighbor_reads.is_empty());
    assert!(matches!(
        executor.state(director_id),
        Some(RegolithState::BloomDirector(BloomDirector {
            site_pos: None,
            site_active_until: None,
            site_rocks_alive: 0,
            ..
        }))
    ));
}

#[test]
fn bloom_site_expires_after_5400_director_ticks() {
    let director_id = PersistId::new(700);
    let active_until = BLOOM_CADENCE_TICKS + BLOOM_LIFETIME_TICKS;
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xE2; 32]));
    executor.insert(
        director_id,
        RegolithState::BloomDirector(BloomDirector {
            clock_tick: active_until - 1,
            next_bloom_tick: BLOOM_CADENCE_TICKS * 3,
            blooms_seeded: 2,
            site_pos: Some(QPos::default()),
            site_active_until: Some(active_until),
            site_rocks_alive: BLOOM_ROCK_COUNT,
        }),
    );
    let outcome = executor
        .step_entity(director_id, Tick::new(active_until), &[])
        .expect("director owns site expiry");
    assert!(outcome.events.is_empty());
    assert!(outcome.neighbor_reads.is_empty());
    assert!(matches!(
        executor.state(director_id),
        Some(RegolithState::BloomDirector(BloomDirector {
            site_pos: None,
            site_active_until: None,
            site_rocks_alive: 0,
            ..
        }))
    ));
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
    shooter.lock_target = Some(PersistId::new(2));
    shooter.lock_progress = LOCK_ACQUISITION_TICKS;
    shooter.locks_acquired = 1;
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
                from_vel: QVel::default(),
                flight_ticks: Some(1),
            }],
        )
        .expect("parent exists");
    let Outcome::Split {
        parent: emitted_parent,
        generation,
        children,
    } = output
        .events
        .iter()
        .find(|event| matches!(event, Outcome::Split { .. }))
        .expect("lethal large rock damage emits a split")
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
                    from_vel: QVel::default(),
                    flight_ticks: Some(1),
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

fn resolved_hits(
    archetype: Archetype,
    target_x: i64,
    target_vel: QVel,
    weapon: WeaponKind,
    samples: u64,
) -> u64 {
    let target = PersistId::new(2);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x35; 32]));
    (0..samples)
        .filter(|sample| {
            let mut craft = Craft::spawned(
                archetype,
                QPos {
                    x: target_x,
                    y: 0,
                    z: 0,
                },
                0,
            );
            craft.vel = target_vel;
            let full_shield = craft.shield;
            executor.insert(target, RegolithState::Craft(craft));
            executor
                .step_entity(
                    target,
                    Tick::new(10_000 + sample),
                    &[Order::Damage {
                        amount: 1,
                        from: PersistId::new(1),
                        from_pos: QPos::default(),
                        from_vel: QVel::default(),
                        from_weapon: weapon,
                        flight_ticks: Some(1),
                    }],
                )
                .expect("target exists");
            matches!(
                executor.state(target),
                Some(RegolithState::Craft(Craft { shield, .. })) if *shield < full_shield
            )
        })
        .count() as u64
}

#[test]
fn fast_orbit_is_measurably_harder_to_hit_over_10000_shots() {
    const SAMPLES: u64 = 10_000;
    let slow_hits = resolved_hits(
        Archetype::Interceptor,
        200_000,
        QVel {
            x: 0,
            y: 0,
            z: 12_000,
        },
        WeaponKind::Stock,
        SAMPLES,
    );
    let fast_hits = resolved_hits(
        Archetype::Interceptor,
        200_000,
        QVel {
            x: 0,
            y: 0,
            z: 120_000,
        },
        WeaponKind::Stock,
        SAMPLES,
    );
    println!(
        "tracking hit rates over {SAMPLES} shots: slow={slow_hits}/{SAMPLES} ({:.2}%), fast={fast_hits}/{SAMPLES} ({:.2}%)",
        slow_hits as f64 * 100.0 / SAMPLES as f64,
        fast_hits as f64 * 100.0 / SAMPLES as f64,
    );
    assert!(slow_hits > fast_hits.saturating_mul(5));
}

#[test]
fn signature_radius_and_accuracy_falloff_are_live_inputs() {
    const SAMPLES: u64 = 10_000;
    let fast_interceptor = resolved_hits(
        Archetype::Interceptor,
        200_000,
        QVel {
            x: 0,
            y: 0,
            z: 120_000,
        },
        WeaponKind::Stock,
        SAMPLES,
    );
    let fast_cruiser = resolved_hits(
        Archetype::Cruiser,
        200_000,
        QVel {
            x: 0,
            y: 0,
            z: 120_000,
        },
        WeaponKind::Stock,
        SAMPLES,
    );
    let optimal = resolved_hits(
        Archetype::Interceptor,
        300_000,
        QVel::default(),
        WeaponKind::Stock,
        SAMPLES,
    );
    let edge_of_falloff = resolved_hits(
        Archetype::Interceptor,
        400_000,
        QVel::default(),
        WeaponKind::Stock,
        SAMPLES,
    );
    assert!(fast_cruiser > fast_interceptor);
    assert_eq!(optimal, SAMPLES);
    assert!(edge_of_falloff < optimal && edge_of_falloff > 4_500);
}

fn fly_one_stock_shot(seed_byte: u8, evade: bool) -> (u64, bool, Outcome) {
    let target = PersistId::new(2);
    let game = Regolith::honest();
    let mut craft = craft_at(300_000);
    craft.yaw_urad = orrery_games::regolith::state::TAU_URAD / 4;
    let full_shield = craft.shield;
    let mut executor = Executor::new(game, UniverseSeed([seed_byte; 32]));
    executor.insert(target, RegolithState::Craft(craft));
    let mut projectile = Some(Order::Damage {
        amount: 1,
        from: PersistId::new(1),
        from_pos: QPos::default(),
        from_vel: QVel::default(),
        from_weapon: WeaponKind::Stock,
        flight_ticks: None,
    });
    for tick in 1..=100 {
        let mut inputs = projectile.take().into_iter().collect::<Vec<_>>();
        if evade && tick > 1 {
            inputs.push(Order::Thrust {
                accel_mmss: 60_000,
                yaw_urad: 0,
                pitch_urad: 0,
            });
        }
        let output = executor
            .step_entity(target, Tick::new(tick), &inputs)
            .expect("target exists");
        projectile = output
            .events
            .iter()
            .find_map(|event| game.deliver(event))
            .map(|(_, order)| order)
            .filter(|order| matches!(order, Order::Damage { .. }));
        if projectile.is_none() {
            let hit = matches!(
                executor.state(target),
                Some(RegolithState::Craft(Craft { shield, .. })) if *shield < full_shield
            );
            let resolution = output
                .events
                .iter()
                .find(|event| matches!(event, Outcome::ShotResolved { .. }))
                .cloned()
                .expect("resolved shot emits its authoritative result");
            return (tick, hit, resolution);
        }
    }
    panic!("projectile did not resolve within its bounded flight time")
}

#[test]
fn course_change_after_fire_avoids_a_fixed_seed_projectile() {
    let stationary = fly_one_stock_shot(0x6A, false);
    let evasive = fly_one_stock_shot(0x6A, true);
    println!("fixed seed 0x6a time-of-flight: stationary={stationary:?}, evasive={evasive:?}");
    assert_eq!((stationary.0, stationary.1), (60, true));
    assert_eq!((evasive.0, evasive.1), (60, false));
}

#[test]
fn shot_resolution_is_emitted_for_hit_and_miss_and_delivered_to_attacker() {
    let game = Regolith::honest();
    let attacker = PersistId::new(1);
    let target = PersistId::new(2);
    for (evade, expected) in [(false, ShotResult::Hit), (true, ShotResult::Miss)] {
        let (_, hit, resolution) = fly_one_stock_shot(0x6A, evade);
        assert_eq!(hit, expected == ShotResult::Hit);
        assert_eq!(
            resolution,
            Outcome::ShotResolved {
                attacker,
                target,
                result: expected,
            }
        );
        assert_eq!(
            game.deliver(&resolution),
            Some((
                attacker,
                Order::ShotResolved {
                    target,
                    result: expected,
                },
            )),
            "the target's result must be routed back to the attacker"
        );
    }
}

#[test]
fn lock_acquisition_replays_from_the_locker_alone() {
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    let run = || {
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xAC; 32]));
        executor.insert(locker, RegolithState::Craft(craft_at(0)));
        let mut hashes = Vec::new();
        for tick in 1..=u64::from(LOCK_ACQUISITION_TICKS) {
            let output = executor
                .step_entity(locker, Tick::new(tick), &[Order::Fire { target }])
                .expect("locker exists");
            assert!(output.neighbor_reads.is_empty());
            if tick < u64::from(LOCK_ACQUISITION_TICKS) {
                assert!(output.events.is_empty(), "lock fired early at tick {tick}");
            } else {
                assert!(!output.events.is_empty(), "acquired lock did not fire");
            }
            hashes.push(output.state_hash);
        }
        (
            executor.state(locker).expect("locker remains").clone(),
            hashes,
        )
    };
    let (live, live_hashes) = run();
    let (replay, replay_hashes) = run();
    assert_eq!(live_hashes, replay_hashes);
    assert_eq!(live, replay);
    assert!(matches!(
        live,
        RegolithState::Craft(Craft {
            lock_target: Some(locked),
            lock_progress: LOCK_ACQUISITION_TICKS,
            locks_acquired: 1,
            ..
        }) if locked == target
    ));
}

fn locked_craft(target: PersistId) -> Craft {
    let mut craft = craft_at(0);
    craft.lock_target = Some(target);
    craft.lock_progress = LOCK_ACQUISITION_TICKS;
    craft.locks_acquired = 1;
    craft
}

#[test]
fn fire_on_a_different_target_switches_the_lock_and_restarts_acquisition() {
    let locker = PersistId::new(1);
    let first_target = PersistId::new(2);
    let second_target = PersistId::new(3);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x57; 32]));
    let mut start = craft_at(0);
    // Mid-acquisition on the old target: progress banked but not yet locked.
    start.lock_target = Some(first_target);
    start.lock_progress = LOCK_ACQUISITION_TICKS - 10;
    executor.insert(locker, RegolithState::Craft(start));

    // The switch tick: B replaces A and acquisition restarts from one. The
    // banked progress must not survive, and nothing may fire on this tick.
    let switched = executor
        .step_entity(
            locker,
            Tick::new(1),
            &[Order::Fire {
                target: second_target,
            }],
        )
        .expect("locker exists");
    assert!(
        switched.events.is_empty(),
        "a switched lock must not fire on its switch tick"
    );
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(locked),
            lock_progress: 1,
            locks_acquired: 0,
            ..
        })) if *locked == second_target
    ));

    // The switched lock then pays the full acquisition again: no shot until
    // LOCK_ACQUISITION_TICKS ticks have named the new target.
    for tick in 2..u64::from(LOCK_ACQUISITION_TICKS) {
        let output = executor
            .step_entity(
                locker,
                Tick::new(tick),
                &[Order::Fire {
                    target: second_target,
                }],
            )
            .expect("locker exists");
        assert!(
            output.events.is_empty(),
            "switched lock fired early at tick {tick}"
        );
    }
    let acquired = executor
        .step_entity(
            locker,
            Tick::new(u64::from(LOCK_ACQUISITION_TICKS)),
            &[Order::Fire {
                target: second_target,
            }],
        )
        .expect("locker exists");
    assert!(!acquired.events.is_empty(), "re-acquired lock did not fire");
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(locked),
            lock_progress: LOCK_ACQUISITION_TICKS,
            locks_acquired: 1,
            ..
        })) if *locked == second_target
    ));
}

#[test]
fn fire_on_the_locked_target_keeps_banked_acquisition() {
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x58; 32]));
    let mut start = craft_at(0);
    start.lock_target = Some(target);
    start.lock_progress = LOCK_ACQUISITION_TICKS / 2;
    executor.insert(locker, RegolithState::Craft(start));
    executor
        .step_entity(locker, Tick::new(1), &[Order::Fire { target }])
        .expect("locker exists");
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(locked),
            lock_progress,
            ..
        })) if *locked == target && *lock_progress == LOCK_ACQUISITION_TICKS / 2 + 1
    ));
}

#[test]
fn range_exceeded_and_target_destroyed_break_logged_locks() {
    let game = Regolith::honest();
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    for (reason, target_state) in [
        (LockBreakReason::RangeExceeded, craft_at(500_000)),
        (LockBreakReason::TargetDestroyed, {
            let mut destroyed = craft_at(0);
            destroyed.hull = 0;
            destroyed.shield = 0;
            destroyed.respawn_in = RESPAWN_TICKS;
            destroyed
        }),
    ] {
        let mut executor = Executor::new(game, UniverseSeed([0xB4; 32]));
        executor.insert(locker, RegolithState::Craft(locked_craft(target)));
        executor.insert(target, RegolithState::Craft(target_state));
        let fired = executor
            .step_entity(locker, Tick::new(1), &[Order::Fire { target }])
            .expect("locker fires");
        let projectile = game
            .deliver(
                fired
                    .events
                    .first()
                    .expect("locked fire emits a projectile"),
            )
            .expect("projectile is delivered")
            .1;
        let resolved = executor
            .step_entity(target, Tick::new(2), &[projectile])
            .expect("target resolves");
        let break_event = resolved
            .events
            .iter()
            .find(|event| matches!(event, Outcome::LockBroken { reason: found, .. } if *found == reason))
            .expect("target emits the scoped lock break");
        let (recipient, break_order) = game.deliver(break_event).expect("break is delivered");
        assert_eq!(recipient, locker);
        executor
            .step_entity(locker, Tick::new(3), &[break_order])
            .expect("locker consumes break");
        assert!(matches!(
            executor.state(locker),
            Some(RegolithState::Craft(Craft {
                lock_target: None,
                lock_progress: 0,
                locks_acquired: 1,
                ..
            }))
        ));
    }
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
                    from_vel: QVel::default(),
                    flight_ticks: Some(1),
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
    } = outcome
        .events
        .iter()
        .find(|event| matches!(event, Outcome::SpawnPickup { .. }))
        .expect("Small death emits its materialized drop")
    else {
        panic!("Small death must fully describe its drop")
    };
    assert_eq!(*pos, QPos::default());
    assert!(matches!(kind, WeaponKind::Volley | WeaponKind::Heavy));
    assert_eq!(*expires_at, PICKUP_TTL_TICKS);
    assert_eq!(outcome.materialized, vec![*id]);
    assert!(matches!(
        executor.state(rock_id),
        Some(RegolithState::Rock(Rock {
            hull: 0,
            pickups_dropped: 1,
            ..
        }))
    ));
    assert!(matches!(
        executor.state(*id),
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
        RegolithState::BloomDirector(BloomDirector::spawned()),
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
        Order::KillCredit,
        Order::RockCredit { points: 4 },
        Order::BloomPopulationChanged {
            bloom_index: 3,
            delta: -1,
        },
        Order::Damage {
            amount: 7,
            from: ship,
            from_pos: pos,
            from_vel: QVel { x: 1, y: 2, z: 3 },
            from_weapon: WeaponKind::Stock,
            flight_ticks: Some(12),
        },
        Order::LockBroken {
            target: pickup,
            reason: LockBreakReason::RangeExceeded,
        },
        Order::ShotResolved {
            target: pickup,
            result: ShotResult::Miss,
        },
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
        Outcome::RockDestroyed {
            by: ship,
            points: 2,
        },
        Outcome::BloomPopulationChanged {
            director: PersistId::new(700),
            bloom_index: 3,
            delta: 1,
        },
        Outcome::DamageDealt {
            attacker: ship,
            target: pickup,
            amount: 7,
            attacker_pos: pos,
            attacker_vel: QVel { x: 1, y: 2, z: 3 },
            attacker_weapon: WeaponKind::Stock,
            flight_ticks: Some(12),
        },
        Outcome::LockBroken {
            locker: ship,
            target: pickup,
            reason: LockBreakReason::TargetDestroyed,
        },
        Outcome::ShotResolved {
            attacker: ship,
            target: pickup,
            result: ShotResult::Hit,
        },
    ];
    for outcome in outcomes {
        assert_eq!(Outcome::decode(&outcome.to_canonical()).unwrap(), outcome);
    }
    let bloom = BloomMembership {
        director: PersistId::new(700),
        bloom_index: 3,
    };
    let bloom_outcome = Outcome::BloomSeeded {
        director: bloom.director,
        bloom_index: bloom.bloom_index,
        site_pos: pos,
        active_until: 9_000,
        rocks: Box::new(core::array::from_fn(|slot| ChildSpec {
            id: PersistId::new(100 + slot as u64),
            tier: RockTier::Small,
            pos,
            vel: QVel::default(),
            bloom: Some(bloom),
        })),
    };
    assert_eq!(
        Outcome::decode(&bloom_outcome.to_canonical()).unwrap(),
        bloom_outcome
    );
}
