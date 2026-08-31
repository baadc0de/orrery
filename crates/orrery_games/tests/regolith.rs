//! Regolith-specific checks for weapon state and planar input discipline.

use std::collections::BTreeMap;

use orrery_core::{
    assert_section_is_exact, evaluate, tick_rng, CoreCodec, Executor, InvariantKind,
    InvariantSample, QPos, QVel, TickBackend, TICK_HZ,
};
use orrery_games::game::Game;
use orrery_games::regolith::{
    archetype::Archetype,
    campaign_engagement_budget_m, campaign_guaranteed_aoi_radius_m, campaign_rock_seeds,
    campaign_spawn_pose, distance_mm,
    invariants::INVARIANTS,
    order::{ChildSpec, LockBreakReason, Order, Outcome, ShotResult},
    pilot::{scenario_at, PilotScenario, PILOT_SCENARIOS, SCENARIO_TICKS},
    projectile_flight_ticks,
    state::{
        BloomDirector, BloomDirectorSection, BloomMembership, Craft, CraftSection, LockClass,
        Pickup, PickupSection, RegolithState, Rock, RockSection, RockTier, PITCH_LIMIT_URAD,
        TRAIL_CAPACITY, TRAIL_MAX_ENCODED_BYTES, TRAIL_SAMPLE_TICKS,
    },
    weapon::{WeaponKind, MAX_WEAPON_REACH_MM},
    Regolith, BLOOM_CADENCE_TICKS, BLOOM_CENTRAL_RADIUS_MM, BLOOM_LIFETIME_TICKS, BLOOM_ROCK_COUNT,
    CAMPAIGN_CELL_EDGE_M, CAMPAIGN_MIN_CELL_EDGE_M, CAMPAIGN_ROCK_COUNT, DRAG_PER_SEC_PER_MILLE,
    ISLAND_CRAFT_BUDGET, ISLAND_DIRECTOR_BUDGET, ISLAND_PICKUP_BUDGET, ISLAND_ROCK_BUDGET,
    ISLAND_WINDOW_BUDGET, KILL_SCORE_POINTS, LOCK_ACQUISITION_TICKS, LOCK_BREAK_TICKS,
    LOCK_DECAY_PER_TICK, MAX_ENGAGEMENT_RANGE_MM, MAX_NEIGHBOR_READS, MAX_TARGET_RADIUS_MM,
    PICKUP_SCORE_POINTS, PICKUP_TTL_TICKS, REGOLITH_RULESET, RESPAWN_TICKS,
};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use rand_chacha::rand_core::SeedableRng;

fn craft_at(x: i64) -> Craft {
    Craft::spawned(Archetype::Interceptor, QPos { x, y: 0, z: 0 }, 0)
}

#[test]
fn campaign_seed_places_every_rock_tier_beside_but_not_on_the_crowd_orbit() {
    let seed = UniverseSeed([0x52; 32]);
    let rocks = campaign_rock_seeds(seed, 8);
    assert_eq!(rocks.len(), CAMPAIGN_ROCK_COUNT);
    assert_eq!(rocks, campaign_rock_seeds(seed, 8));
    assert_ne!(rocks, campaign_rock_seeds(UniverseSeed([0x53; 32]), 8));

    let tiers = rocks.iter().fold([0usize; 3], |mut counts, seeded| {
        counts[seeded.rock.tier.tag() as usize] += 1;
        counts
    });
    assert_eq!(tiers, [1, 2, 3], "the campaign publishes every visual tier");

    // This is the cross-platform commitment for scenario composition itself.
    // Same-process equality alone cannot catch a platform-specific libm drift
    // in the polar placement below; every determinism-matrix leg runs this
    // committed byte comparison.
    let mut digest = blake3::Hasher::new();
    for seeded in &rocks {
        digest.update(&seeded.entity.0.to_le_bytes());
        digest.update(&seeded.owner_slot.to_le_bytes());
        let mut encoded = Vec::new();
        seeded.rock.encode(&mut encoded);
        digest.update(&encoded);
    }
    assert_eq!(
        *digest.finalize().as_bytes(),
        [
            92, 13, 156, 174, 173, 149, 167, 28, 0, 143, 167, 231, 88, 139, 225, 13, 145, 226, 126,
            160, 130, 88, 154, 52, 63, 191, 160, 21, 198, 59, 255, 213,
        ]
    );

    let (player, _) = campaign_spawn_pose(8, 9);
    for seeded in &rocks {
        assert!(
            distance_mm(player, seeded.rock.pos) <= 350_000,
            "rock {} must start inside the player's stock encounter",
            seeded.entity.0,
        );
        for slot in 0..8 {
            let (craft, _) = campaign_spawn_pose(slot, 8);
            assert!(
                distance_mm(craft, seeded.rock.pos)
                    > u128::try_from(seeded.rock.tier.limits().radius_mm + 50_000).unwrap(),
                "rock {} must not seed a collision under bot slot {slot}",
                seeded.entity.0,
            );
        }
    }
}

#[test]
fn rock_position_integrates_velocity_on_all_axes_each_tick() {
    let rock_id = PersistId::new(448);
    let initial_pos = QPos {
        x: 1_000,
        y: -2_000,
        z: 3_000,
    };
    let velocity = QVel {
        x: 6_000,
        y: -12_000,
        z: 18_000,
    };
    let per_tick = QPos {
        x: velocity.x / i64::from(TICK_HZ),
        y: velocity.y / i64::from(TICK_HZ),
        z: velocity.z / i64::from(TICK_HZ),
    };
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x48; 32]));
    executor.insert(
        rock_id,
        RegolithState::Rock(Rock::spawned(RockTier::Small, 0, initial_pos, velocity)),
    );

    for tick in 1_u32..=3 {
        executor
            .step_entity(rock_id, Tick::new(u64::from(tick)), &[])
            .expect("rock exists");
        assert_eq!(
            executor.state(rock_id),
            Some(&RegolithState::Rock(Rock::spawned(
                RockTier::Small,
                0,
                QPos {
                    x: initial_pos.x + i64::from(tick) * per_tick.x,
                    y: initial_pos.y + i64::from(tick) * per_tick.y,
                    z: initial_pos.z + i64::from(tick) * per_tick.z,
                },
                velocity,
            ))),
            "rock motion at tick {tick}",
        );
    }
}

#[test]
fn ship_striking_a_rock_produces_a_recorded_adjudicated_collision() {
    let ship_id = PersistId::new(1);
    let rock_id = PersistId::new(2);
    let mut ship = craft_at(0);
    ship.vel = QVel {
        x: 60_000,
        y: 0,
        z: 0,
    };
    let rock = Rock::spawned(
        RockTier::Small,
        0,
        QPos {
            x: 10_000,
            y: 0,
            z: 0,
        },
        QVel::default(),
    );
    let game = Regolith::honest();
    let mut executor = Executor::new(game, UniverseSeed([0xC4; 32]));
    executor.insert(ship_id, RegolithState::Craft(ship));
    executor.insert(rock_id, RegolithState::Rock(rock));

    let resolved = executor
        .step_entity(ship_id, Tick::new(1), &[Order::Collide { other: rock_id }])
        .expect("ship exists");
    assert_eq!(resolved.neighbor_reads, [rock_id]);
    let collision = resolved
        .events
        .iter()
        .find(|event| matches!(event, Outcome::Collision { .. }))
        .expect("overlapping approaching bodies resolve");
    assert_eq!(
        Outcome::decode(&collision.to_canonical()).unwrap(),
        *collision
    );
    let (target, delivered) = game.deliver(collision).expect("collision routes to rock");
    assert_eq!(target, rock_id);
    assert_eq!(Order::decode(&delivered.to_canonical()).unwrap(), delivered);
    executor
        .step_entity(rock_id, Tick::new(2), &[delivered])
        .expect("rock applies its own collision result");

    assert!(matches!(
        executor.state(ship_id),
        Some(RegolithState::Craft(Craft { collisions: 1, .. }))
    ));
    assert!(matches!(
        executor.state(rock_id),
        Some(RegolithState::Rock(Rock { collisions: 1, .. }))
    ));
}

#[test]
fn combined_visibility_and_collision_read_exactly_the_declared_neighbor_bound() {
    let target = PersistId::new(1);
    let locker = PersistId::new(2);
    let cover = PersistId::new(3);
    let collider = PersistId::new(4);
    let mut target_state = craft_at(100_000);
    target_state.vel.x = 60_000;
    let mut locker_state = locked_craft(target);
    locker_state.pos = QPos::default();
    let cover_state = Rock::spawned(
        RockTier::Small,
        0,
        QPos {
            x: 50_000,
            y: 0,
            z: 0,
        },
        QVel::default(),
    );
    let collider_state = Rock::spawned(
        RockTier::Small,
        0,
        QPos {
            x: 110_000,
            y: 0,
            z: 0,
        },
        QVel::default(),
    );

    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xC8; 32]));
    executor.insert_observed(target, RegolithState::Craft(target_state), Tick::new(100));
    executor.insert_observed(locker, RegolithState::Craft(locker_state), Tick::new(99));
    executor.insert_observed(cover, RegolithState::Rock(cover_state), Tick::new(98));
    executor.insert_observed(collider, RegolithState::Rock(collider_state), Tick::new(97));

    let outcome = executor
        .step_entity(
            target,
            Tick::new(100),
            &[
                Order::ClaimCover {
                    locker,
                    rock: cover,
                },
                Order::Collide { other: collider },
            ],
        )
        .expect("target exists");

    assert_eq!(outcome.neighbor_reads, [locker, cover, collider]);
    assert_eq!(outcome.neighbor_reads.len(), MAX_NEIGHBOR_READS);
    assert_eq!(outcome.neighbor_frames.len(), MAX_NEIGHBOR_READS);
}

#[test]
fn two_ships_striking_each_other_resolve_once_by_stable_id() {
    let lower = PersistId::new(10);
    let higher = PersistId::new(11);
    let mut left = craft_at(0);
    left.vel.x = 60_000;
    let mut right = craft_at(5_000);
    right.vel.x = -60_000;
    let game = Regolith::honest();
    let mut executor = Executor::new(game, UniverseSeed([0xC5; 32]));
    executor.insert(lower, RegolithState::Craft(left));
    executor.insert(higher, RegolithState::Craft(right));

    let duplicate = executor
        .step_entity(higher, Tick::new(1), &[Order::Collide { other: lower }])
        .expect("higher-id ship exists");
    assert!(
        duplicate.events.is_empty(),
        "higher id cannot resolve the pair"
    );

    let resolved = executor
        .step_entity(lower, Tick::new(1), &[Order::Collide { other: higher }])
        .expect("lower-id ship exists");
    let collision = resolved
        .events
        .iter()
        .find(|event| matches!(event, Outcome::Collision { .. }))
        .expect("lower id resolves the pair");
    let (_, delivered) = game.deliver(collision).expect("collision routes");
    executor
        .step_entity(higher, Tick::new(2), &[delivered])
        .expect("higher-id ship applies its own result");

    assert!(matches!(
        executor.state(lower),
        Some(RegolithState::Craft(Craft { collisions: 1, .. }))
    ));
    assert!(matches!(
        executor.state(higher),
        Some(RegolithState::Craft(Craft { collisions: 1, .. }))
    ));
}

#[test]
fn delivered_collision_force_precedes_the_authored_collision_force() {
    let resolver = PersistId::new(30);
    let other = PersistId::new(31);
    let run = |orders: &[Order]| {
        let mut own = craft_at(0);
        own.vel.x = 20_000;
        let mut target = Craft::spawned(
            Archetype::Cruiser,
            QPos {
                x: 5_000,
                y: 0,
                z: 0,
            },
            0,
        );
        target.vel.x = -10_000;
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xC7; 32]));
        executor.insert(resolver, RegolithState::Craft(own));
        executor.insert(other, RegolithState::Craft(target));
        let output = executor
            .step_entity(resolver, Tick::new(4), orders)
            .expect("resolver exists");
        let RegolithState::Craft(state) = executor.state(resolver).expect("resolver remains")
        else {
            unreachable!("resolver remains a craft")
        };
        (state.vel.x, output.events)
    };
    let delivered = Order::CollisionResolved {
        from: PersistId::new(29),
        velocity: QVel {
            x: 40_000,
            y: 0,
            z: 0,
        },
    };
    let authored = Order::Collide { other };

    let (delivered_first, events) = run(&[delivered.clone(), authored.clone()]);
    let (authored_first, _) = run(&[authored, delivered]);

    assert!(delivered_first < 0, "the later authored contact wins");
    assert!(authored_first > 0, "the later delivered force wins");
    assert_ne!(delivered_first, authored_first);
    assert!(matches!(
        events.as_slice(),
        [Outcome::Collision { collider, target, .. }]
            if *collider == resolver && *target == other
    ));
}

#[test]
fn collision_beyond_contact_range_is_rejected() {
    let ship_id = PersistId::new(20);
    let rock_id = PersistId::new(21);
    let mut ship = craft_at(0);
    ship.vel.x = 60_000;
    let rock = Rock::spawned(
        RockTier::Small,
        0,
        QPos {
            x: 20_000,
            y: 0,
            z: 0,
        },
        QVel::default(),
    );
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xC6; 32]));
    executor.insert(ship_id, RegolithState::Craft(ship));
    executor.insert(rock_id, RegolithState::Rock(rock));

    let rejected = executor
        .step_entity(ship_id, Tick::new(1), &[Order::Collide { other: rock_id }])
        .expect("ship exists");
    assert_eq!(rejected.neighbor_reads, [rock_id]);
    assert!(
        rejected.events.is_empty(),
        "the overlap predicate must reject separated bodies"
    );
    assert!(matches!(
        executor.state(ship_id),
        Some(RegolithState::Craft(Craft { collisions: 0, .. }))
    ));
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

/// #545, and the class #520 settled only for the weapon that existed then.
///
/// A weapon out-ranging the AOI is a target that can be shot at while the
/// host is not obliged to replicate it. The bound is not the cell edge:
/// commitment is hysteretic and *both* bodies latch their own, so the
/// campaign budgets `edge - 2m` - 409.6 m at the 512 m edge.
///
/// The edge does not move to accommodate a weapon. A block wide enough to
/// hold the whole encounter would delete the interest-churn surface the
/// campaign exists to shake down, so this reads as a bound on the **weapon
/// table**: widen any weapon's `optimal_mm` or `falloff_mm` past the budget
/// and this fails, with `CAMPAIGN_MIN_CELL_EDGE_M` naming the edge that
/// weapon would have cost.
///
/// It asserts the relationship, not the constants - the derived minimum moves
/// with the table while the declared edge stays where it is put.
#[test]
fn every_weapons_reach_fits_inside_the_campaign_aoi_guarantee() {
    let guaranteed_m = campaign_guaranteed_aoi_radius_m(CAMPAIGN_CELL_EDGE_M);
    let budget_m = campaign_engagement_budget_m(CAMPAIGN_CELL_EDGE_M);

    assert!(
        budget_m < guaranteed_m,
        "the engagement budget must sit inside the guarantee, not on it: \
         {budget_m} m against {guaranteed_m} m"
    );

    for kind in WeaponKind::ALL {
        let envelope_mm = kind.weapon().reach_mm() + MAX_TARGET_RADIUS_MM;
        let envelope_m = envelope_mm as f64 / 1_000.0;
        assert!(
            envelope_m <= budget_m,
            "{kind:?} reaches {envelope_m} m against a {budget_m} m engagement \
             budget at the {CAMPAIGN_CELL_EDGE_M} m campaign edge - shorten the \
             weapon. Fitting it would cost a {CAMPAIGN_MIN_CELL_EDGE_M} m edge, \
             which is not the trade the campaign wants"
        );
    }

    // The declared edge is held; what must follow the table is the derived
    // minimum, and it may not overtake what is declared.
    let required_m = campaign_engagement_budget_m(CAMPAIGN_MIN_CELL_EDGE_M);
    assert!(
        budget_m >= required_m,
        "the weapon table now needs a {CAMPAIGN_MIN_CELL_EDGE_M} m edge, past the \
         declared {CAMPAIGN_CELL_EDGE_M} m - the table has outgrown the campaign"
    );

    // The derivation is the tightest edge that covers the table, not a slack
    // constant that happens to be large enough: one framework cell less would
    // not cover it.
    assert!(
        campaign_engagement_budget_m(CAMPAIGN_MIN_CELL_EDGE_M - 128.0)
            < MAX_ENGAGEMENT_RANGE_MM as f64 / 1_000.0,
        "CAMPAIGN_MIN_CELL_EDGE_M is not the smallest edge that covers the table"
    );

    // Heavy is the pickup a kill drops. It must still read as an upgrade, so
    // it stays the longest reach in the table - cutting it to fit is not
    // licence to make it just another gun.
    let heavy = WeaponKind::Heavy.weapon().reach_mm();
    for kind in WeaponKind::ALL {
        if kind != WeaponKind::Heavy {
            assert!(
                kind.weapon().reach_mm() < heavy,
                "{kind:?} now reaches as far as Heavy, which is the long gun"
            );
        }
    }

    // The longest reach is read from the table, not restated beside it.
    assert_eq!(
        MAX_WEAPON_REACH_MM,
        WeaponKind::ALL
            .into_iter()
            .map(|kind| kind.weapon().reach_mm())
            .max()
            .expect("the weapon table is not empty")
    );
    assert_eq!(
        MAX_ENGAGEMENT_RANGE_MM,
        MAX_WEAPON_REACH_MM + MAX_TARGET_RADIUS_MM
    );
}

#[test]
fn v20_snapshot_isolation_ruleset_identity_and_island_budget_are_pinned() {
    assert_eq!(REGOLITH_RULESET.version, 20);
    assert_eq!(
        PITCH_LIMIT_URAD, 1_570_796,
        "a quarter turn either side of level, on the micro-radian lattice"
    );
    assert_eq!(WeaponKind::Stock.weapon().damage_base, 10);
    assert_eq!(WeaponKind::Volley.weapon().rolls, 3);
    assert_eq!(WeaponKind::Stock.weapon().optimal_mm, 240_000);
    assert_eq!(WeaponKind::Volley.weapon().tracking_urad_per_sec, 300_000);
    assert_eq!(WeaponKind::Heavy.weapon().falloff_mm, 60_000);
    assert_eq!(WeaponKind::Heavy.weapon().projectile_speed_mms, 180_000);
    assert_eq!(
        [WeaponKind::Stock, WeaponKind::Volley, WeaponKind::Heavy].map(|kind| {
            let weapon = kind.weapon();
            weapon.optimal_mm + weapon.falloff_mm
        }),
        [320_000, 300_000, 360_000]
    );
    assert_eq!(
        MAX_TARGET_RADIUS_MM, 40_000,
        "a Large rock is the widest target"
    );
    assert_eq!(MAX_ENGAGEMENT_RANGE_MM, 400_000);
    assert_eq!(CAMPAIGN_MIN_CELL_EDGE_M, 512.0);
    assert_eq!(CAMPAIGN_CELL_EDGE_M, 512.0);
    assert_eq!(ISLAND_CRAFT_BUDGET, 8);
    assert_eq!(ISLAND_ROCK_BUDGET, 24);
    assert_eq!(ISLAND_PICKUP_BUDGET, 4);
    assert_eq!(ISLAND_DIRECTOR_BUDGET, 1);
    assert_eq!(ISLAND_WINDOW_BUDGET, 37);
    assert_eq!((KILL_SCORE_POINTS, PICKUP_SCORE_POINTS), (25, 5));
}

#[test]
fn v18_speed_caps_and_velocity_change_bounds_are_pinned() {
    let interceptor = Archetype::Interceptor.limits();
    let cruiser = Archetype::Cruiser.limits();
    assert_eq!(
        interceptor.max_speed_mms, 480_000,
        "interceptor speed ceiling must be 480,000 mm/s (four times v17)"
    );
    assert_eq!(
        cruiser.max_speed_mms, 120_000,
        "cruiser speed ceiling must be 120,000 mm/s (twice v17)"
    );

    let bound = |archetype: Archetype| {
        let limits = archetype.limits();
        limits.max_accel_mmss / TICK_HZ as i64
            + limits.max_speed_mms * DRAG_PER_SEC_PER_MILLE / (1_000 * TICK_HZ as i64)
            + 100
    };
    assert_eq!(
        bound(Archetype::Interceptor),
        1_500,
        "interceptor stage-1 velocity-change bound must include 1,000 mm/s thrust, \
         400 mm/s cap-speed drag, and the 100 mm/s margin"
    );
    assert_eq!(
        bound(Archetype::Cruiser),
        533,
        "cruiser stage-1 velocity-change bound must include 333 mm/s thrust, \
         100 mm/s cap-speed drag, and the 100 mm/s margin"
    );

    for archetype in Archetype::ALL {
        let limits = archetype.limits();
        let drag_equilibrium_mms = limits.max_accel_mmss * 1_000 / DRAG_PER_SEC_PER_MILLE;
        assert!(
            limits.max_speed_mms < drag_equilibrium_mms,
            "{archetype:?} ceiling {} mm/s must bind below its {} mm/s drag equilibrium",
            limits.max_speed_mms,
            drag_equilibrium_mms
        );
    }
}

#[test]
fn thrust_accumulates_delta_velocity_instead_of_replacing_velocity() {
    let entity = PersistId::new(1);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x18; 32]));
    executor.insert(entity, RegolithState::Craft(craft_at(0)));
    let thrust = Order::Thrust {
        accel_mmss: 60_000,
        yaw_urad: 0,
        pitch_urad: 0,
    };
    executor
        .step_entity(entity, Tick::new(1), core::slice::from_ref(&thrust))
        .expect("craft exists");
    let first = match executor.state(entity) {
        Some(RegolithState::Craft(craft)) => craft.vel.x,
        _ => panic!("craft remains a craft"),
    };
    executor
        .step_entity(entity, Tick::new(2), core::slice::from_ref(&thrust))
        .expect("craft exists");
    let second = match executor.state(entity) {
        Some(RegolithState::Craft(craft)) => craft.vel.x,
        _ => panic!("craft remains a craft"),
    };
    assert!(
        second > first,
        "thrust must accumulate delta velocity: first tick {first} mm/s, second tick {second} mm/s"
    );
    assert!(
        second < 2_100,
        "two ticks of 60,000 mm/s^2 thrust must add about 2,000 mm/s, not set an arbitrary speed; got {second} mm/s"
    );
}

#[test]
fn craft_trail_is_quantized_bounded_hashed_and_costs_25_bytes_when_full() {
    let entity = PersistId::new(1);
    let mut craft = craft_at(0);
    craft.vel = QVel {
        x: 480_000,
        y: 0,
        z: 0,
    };
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x19; 32]));
    executor.insert(entity, RegolithState::Craft(craft));
    for tick in 1..=u64::from(TRAIL_SAMPLE_TICKS) * (TRAIL_CAPACITY as u64 + 1) {
        executor
            .step_entity(entity, Tick::new(tick), &[])
            .expect("craft exists");
    }
    let RegolithState::Craft(craft) = executor.state(entity).expect("craft exists") else {
        panic!("craft remains a craft")
    };
    let points = craft.trail.points().collect::<Vec<_>>();
    assert_eq!(
        points.len(),
        TRAIL_CAPACITY,
        "trail retained {} points, expected hard capacity {TRAIL_CAPACITY}",
        points.len()
    );
    assert!(
        points.windows(2).all(|pair| pair[0].x_m < pair[1].x_m),
        "whole-metre trail points must remain oldest-first behind a +x craft: {points:?}"
    );
    assert!(
        points[0].x_m > 100,
        "the fifth sample must overwrite the oldest ring entry; oldest retained point was {:?}",
        points[0]
    );

    let encoded = craft.to_canonical();
    assert_eq!(
        encoded.len(),
        132 + TRAIL_MAX_ENCODED_BYTES,
        "a full four-point trail must add exactly {TRAIL_MAX_ENCODED_BYTES} canonical bytes to the 132-byte v17 craft"
    );
    assert_eq!(Craft::decode(&encoded), Ok(craft.clone()));

    let mut without_trail = craft.clone();
    without_trail.trail = Default::default();
    assert_ne!(
        orrery_core::state_hash(&RegolithState::Craft(craft.clone())),
        orrery_core::state_hash(&RegolithState::Craft(without_trail)),
        "trail points must participate in the replicated canonical bytes and state hash"
    );
}

#[test]
fn trail_quantization_is_integer_and_out_of_range_sets_the_hashed_overflow_flag() {
    let entity = PersistId::new(1);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x1A; 32]));
    executor.insert(
        entity,
        RegolithState::Craft(Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: 1_500,
                y: -1_500,
                z: 499,
            },
            0,
        )),
    );
    for tick in 1..=u64::from(TRAIL_SAMPLE_TICKS) {
        executor
            .step_entity(entity, Tick::new(tick), &[])
            .expect("craft exists");
    }
    let Some(RegolithState::Craft(craft)) = executor.state(entity) else {
        panic!("craft remains a craft")
    };
    assert_eq!(
        craft.trail.points().collect::<Vec<_>>(),
        [orrery_games::regolith::state::TrailPoint {
            x_m: 2,
            y_m: -2,
            z_m: 0,
        }],
        "trail positions must snap integer millimetres to whole metres, half away from zero"
    );

    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x1B; 32]));
    executor.insert(
        entity,
        RegolithState::Craft(Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: (i64::from(i16::MAX) + 1) * 1_000,
                y: 0,
                z: 0,
            },
            0,
        )),
    );
    for tick in 1..=u64::from(TRAIL_SAMPLE_TICKS) {
        executor
            .step_entity(entity, Tick::new(tick), &[])
            .expect("craft exists");
    }
    let Some(RegolithState::Craft(craft)) = executor.state(entity) else {
        panic!("craft remains a craft")
    };
    assert!(
        craft.arithmetic_overflowed,
        "a trail coordinate beyond signed 16-bit metres must set the canonical arithmetic-overflow flag"
    );
    assert!(
        craft.trail.is_empty(),
        "an unrepresentable trail coordinate must not be clamped into hashed state"
    );
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
                .filter(|order| matches!(order, Order::Fire))
                .count(),
            1,
            "{} must hold the trigger",
            scenario.name()
        );
        assert!(matches!(
            orders.first(),
            Some(Order::Thrust { pitch_urad, .. }) if pitch_urad.abs() <= PITCH_LIMIT_URAD
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
        assert_ne!(rock.vel, QVel::default(), "bloom rocks must visibly drift");
        let speed_sq = [rock.vel.x, rock.vel.y, rock.vel.z]
            .into_iter()
            .map(|value| i128::from(value).pow(2))
            .sum::<i128>();
        assert!(
            speed_sq <= i128::from(rock.tier.limits().max_speed_mms).pow(2),
            "bloom velocity stays inside its tier ceiling"
        );
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
    killer_start.lock_class = Some(LockClass::Ship);
    killer_start.lock_progress = LOCK_ACQUISITION_TICKS;
    killer_start.locks_acquired = 1;
    let mut victim_state = craft_at(0);
    victim_state.hull = 1;
    victim_state.shield = 0;
    let mut live = Executor::new(game, UniverseSeed([0xC1; 32]));
    live.insert(killer, RegolithState::Craft(killer_start.clone()));
    live.insert(victim, RegolithState::Craft(victim_state));

    let fired = live
        .step_entity(killer, Tick::new(1), &[Order::Fire])
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
        .step_entity(killer, Tick::new(1), &[Order::Fire])
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
        .step_entity(killer, Tick::new(1), &[Order::Fire])
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
                from_yaw_urad: 0,
                from_archetype: Archetype::Interceptor,
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
                from_yaw_urad: 0,
                from_archetype: Archetype::Interceptor,
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
    shooter.lock_class = Some(LockClass::Ship);
    shooter.lock_progress = LOCK_ACQUISITION_TICKS;
    shooter.locks_acquired = 1;
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([9; 32]));
    executor.insert(PersistId::new(1), RegolithState::Craft(shooter));
    executor.insert(PersistId::new(2), RegolithState::Craft(craft_at(1)));
    let output = executor
        .step_entity(PersistId::new(1), Tick::new(1), &[Order::Fire])
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
                from_yaw_urad: 0,
                from_archetype: Archetype::Interceptor,
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
                    from_yaw_urad: 0,
                    from_archetype: Archetype::Interceptor,
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

/// The honest pilot flies elevation, and stays inside the declared bound.
///
/// Pre-v19 this test asserted the opposite — `pitch_urad: 0` exactly. The
/// unlock is the whole point of the change, so the assertion inverts: what is
/// still forbidden is only leaving the bound.
#[test]
fn honest_pilot_flies_pitch_within_the_declared_limit() {
    let game = Regolith::honest();
    let mut rng = orrery_core::TickRng::from_seed([1; 32]);
    let mut pitched = false;
    for tick in 1..=64 {
        let mut orders = Vec::new();
        game.honest_inputs(
            PersistId::new(1),
            0,
            Tick::new(tick),
            &[],
            &mut rng,
            &mut orders,
        );
        let Some(Order::Thrust { pitch_urad, .. }) = orders.first() else {
            panic!("the pilot leads with a thrust");
        };
        assert!(
            pitch_urad.abs() <= PITCH_LIMIT_URAD,
            "tick {tick}: pilot asked for {pitch_urad} urad of pitch, past the limit"
        );
        pitched |= *pitch_urad != 0;
    }
    assert!(pitched, "the pilot must actually use the elevation axis");
}

/// The clamp lives in the canonical step, so it is inside `hash(e, t)`.
///
/// Drive one craft with a pitch delta far past the limit for many ticks and
/// the stored elevation must sit exactly on the stop, both signs. Removing the
/// `.clamp(-PITCH_LIMIT_URAD, PITCH_LIMIT_URAD)` from `Regolith::step` fails
/// this by name.
#[test]
fn pitch_saturates_at_the_declared_limit_in_the_canonical_step() {
    for (direction, expected) in [(1, PITCH_LIMIT_URAD), (-1, -PITCH_LIMIT_URAD)] {
        let entity = PersistId::new(1);
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x19; 32]));
        executor.insert(entity, RegolithState::Craft(craft_at(0)));
        let thrust = Order::Thrust {
            accel_mmss: 0,
            yaw_urad: 0,
            pitch_urad: direction * (PITCH_LIMIT_URAD / 4),
        };
        for tick in 1..=16 {
            executor
                .step_entity(entity, Tick::new(tick), core::slice::from_ref(&thrust))
                .expect("craft exists");
            let pitch = match executor.state(entity) {
                Some(RegolithState::Craft(craft)) => craft.pitch_urad,
                _ => panic!("craft remains a craft"),
            };
            assert!(
                pitch.abs() <= PITCH_LIMIT_URAD,
                "tick {tick}: pitch {pitch} left the +/-{PITCH_LIMIT_URAD} urad bound"
            );
        }
        let settled = match executor.state(entity) {
            Some(RegolithState::Craft(craft)) => craft.pitch_urad,
            _ => panic!("craft remains a craft"),
        };
        assert_eq!(
            settled, expected,
            "sustained pitch must settle on the stop, not past it"
        );
    }
}

/// The value-range invariant is the second guard, independent of the clamp.
#[test]
fn value_range_rejects_pitch_past_the_limit_and_admits_pitch_within_it() {
    let inside = {
        let mut craft = craft_at(0);
        craft.pitch_urad = PITCH_LIMIT_URAD;
        craft
    };
    let outside = {
        let mut craft = craft_at(0);
        craft.pitch_urad = PITCH_LIMIT_URAD + 1;
        craft
    };
    evaluate(INVARIANTS, &sample(None, &RegolithState::Craft(inside)))
        .expect("pitch exactly on the stop is legal");
    let violation = evaluate(INVARIANTS, &sample(None, &RegolithState::Craft(outside)))
        .expect_err("pitch past the stop must be refused");
    assert_eq!(violation.kind, InvariantKind::ValueRange);
    assert_eq!(violation.validator, "regolith/value-range");
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
                        from_yaw_urad: 0,
                        from_archetype: Archetype::Interceptor,
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
    // Both probes are the weapon's own band, not literals that were its band
    // when this was written: #545 moved Stock and these silently stopped
    // being the optimal edge and the falloff edge.
    let stock = WeaponKind::Stock.weapon();
    let optimal = resolved_hits(
        Archetype::Interceptor,
        stock.optimal_mm,
        QVel::default(),
        WeaponKind::Stock,
        SAMPLES,
    );
    let edge_of_falloff = resolved_hits(
        Archetype::Interceptor,
        stock.reach_mm(),
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
    // At Stock's optimal range: this fixture is about evasion during flight,
    // so it must not pick up a range penalty that would blur the result.
    let mut craft = craft_at(WeaponKind::Stock.weapon().optimal_mm);
    craft.yaw_urad = orrery_games::regolith::state::TAU_URAD / 4;
    let full_shield = craft.shield;
    let mut executor = Executor::new(game, UniverseSeed([seed_byte; 32]));
    executor.insert(target, RegolithState::Craft(craft));
    let mut projectile = Some(Order::Damage {
        amount: 1,
        from: PersistId::new(1),
        from_pos: QPos::default(),
        from_vel: QVel::default(),
        from_yaw_urad: 0,
        from_archetype: Archetype::Interceptor,
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
    // The flight time is the ruleset's own, for the range the fixture fires
    // at -- 48 ticks at Stock's 240 m optimal and 300 m/s projectile. Both
    // shots take exactly as long; only the evasion changes the verdict.
    let stock = WeaponKind::Stock.weapon();
    let flight = u64::from(projectile_flight_ticks(
        (stock.optimal_mm as u128).pow(2),
        stock.projectile_speed_mms,
    ));
    assert_eq!((stationary.0, stationary.1), (flight, true));
    assert_eq!((evasive.0, evasive.1), (flight, false));
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

fn fire_through_executor(archetype: Archetype, target_pos: QPos) -> (Craft, Craft, Outcome) {
    let game = Regolith::honest();
    let attacker = PersistId::new(1);
    let target = PersistId::new(2);
    let mut shooter = Craft::spawned(archetype, QPos::default(), 0);
    shooter.lock_target = Some(target);
    shooter.lock_class = Some(orrery_games::regolith::state::LockClass::Ship);
    shooter.lock_progress = LOCK_ACQUISITION_TICKS;
    shooter.locks_acquired = 1;
    let victim = Craft::spawned(Archetype::Interceptor, target_pos, 0);
    let mut executor = Executor::new(game, UniverseSeed([0xA4; 32]));
    executor.insert(attacker, RegolithState::Craft(shooter));
    executor.insert(target, RegolithState::Craft(victim));

    let fired = executor
        .step_entity(attacker, Tick::new(1), &[Order::Fire])
        .expect("the locked shooter exists");
    let damage = fired
        .events
        .iter()
        .map(|event| Outcome::decode(&event.to_canonical()).expect("outcome crosses the wire"))
        .find_map(|event| game.deliver(&event))
        .map(|(_, order)| order)
        .map(|order| Order::decode(&order.to_canonical()).expect("delivery crosses the wire"))
        .expect("a locked, cooled-down shooter emits its shot");
    let resolved = executor
        .step_entity(target, Tick::new(2), &[damage])
        .expect("the target resolves the delivered shot");
    let resolution = resolved
        .events
        .iter()
        .find(|event| matches!(event, Outcome::ShotResolved { .. }))
        .cloned()
        .expect("every non-breaking shot has a visible named resolution");
    let RegolithState::Craft(shooter) = executor.state(attacker).expect("shooter remains") else {
        panic!("shooter remains a craft")
    };
    let RegolithState::Craft(victim) = executor.state(target).expect("target remains") else {
        panic!("target remains a craft")
    };
    (shooter.clone(), victim.clone(), resolution)
}

#[test]
fn interceptor_cannot_fire_abeam_through_real_executor() {
    let target = PersistId::new(2);
    let (shooter, victim, resolution) = fire_through_executor(
        Archetype::Interceptor,
        QPos {
            x: 0,
            y: 0,
            z: 10_000,
        },
    );
    assert_eq!(
        resolution,
        Outcome::ShotResolved {
            attacker: PersistId::new(1),
            target,
            result: ShotResult::OutOfArc,
        }
    );
    assert_eq!(shooter.shots, 1, "the refused attempt wastes the shot");
    assert!(shooter.cooldown > 0, "the refused attempt pays cooldown");
    assert_eq!(
        shooter.lock_target,
        Some(target),
        "refusal preserves the lock"
    );
    assert_eq!(victim.shield, victim.archetype.limits().max_shield);
}

#[test]
fn cruiser_cannot_fire_forward_through_real_executor() {
    let target = PersistId::new(2);
    let (shooter, victim, resolution) = fire_through_executor(
        Archetype::Cruiser,
        QPos {
            x: 10_000,
            y: 0,
            z: 0,
        },
    );
    assert_eq!(
        resolution,
        Outcome::ShotResolved {
            attacker: PersistId::new(1),
            target,
            result: ShotResult::OutOfArc,
        }
    );
    assert_eq!(shooter.shots, 1, "the refused attempt wastes the shot");
    assert!(shooter.cooldown > 0, "the refused attempt pays cooldown");
    assert_eq!(
        shooter.lock_target,
        Some(target),
        "refusal preserves the lock"
    );
    assert_eq!(victim.shield, victim.archetype.limits().max_shield);
}

#[test]
fn craft_target_inside_drawn_arc_hits_through_emission_and_delivery() {
    for (archetype, target_pos) in [
        (
            Archetype::Interceptor,
            QPos {
                x: 1_000,
                y: 0,
                z: 0,
            },
        ),
        (
            Archetype::Cruiser,
            QPos {
                x: 0,
                y: 0,
                z: 1_000,
            },
        ),
    ] {
        let (_, _, resolution) = fire_through_executor(archetype, target_pos);
        assert!(
            matches!(
                resolution,
                Outcome::ShotResolved {
                    result: ShotResult::Hit,
                    ..
                }
            ),
            "{archetype:?}'s in-arc shot did not resolve through the existing hit path"
        );
    }
}

fn fire_at_rock_through_executor(target_pos: QPos) -> (Craft, Rock, Outcome) {
    let game = Regolith::honest();
    let attacker = PersistId::new(1);
    let target = PersistId::new(2);
    let mut shooter = Craft::spawned(Archetype::Interceptor, QPos::default(), 0);
    shooter.lock_target = Some(target);
    shooter.lock_class = Some(LockClass::Rock);
    shooter.lock_progress = LOCK_ACQUISITION_TICKS;
    shooter.locks_acquired = 1;
    let rock = Rock::spawned(RockTier::Small, 0, target_pos, QVel::default());
    let mut executor = Executor::new(game, UniverseSeed([0xA5; 32]));
    executor.insert(attacker, RegolithState::Craft(shooter));
    executor.insert(target, RegolithState::Rock(rock));

    let fired = executor
        .step_entity(attacker, Tick::new(1), &[Order::Fire])
        .expect("the rock-locked shooter exists");
    let damage = fired
        .events
        .iter()
        .map(|event| Outcome::decode(&event.to_canonical()).expect("outcome crosses the wire"))
        .find_map(|event| game.deliver(&event))
        .map(|(_, order)| order)
        .map(|order| Order::decode(&order.to_canonical()).expect("delivery crosses the wire"))
        .expect("a locked, cooled-down shooter emits its rock shot");
    let resolved = executor
        .step_entity(target, Tick::new(2), &[damage])
        .expect("the rock resolves the delivered shot");
    let resolution = resolved
        .events
        .iter()
        .find(|event| matches!(event, Outcome::ShotResolved { .. }))
        .cloned()
        .expect("the rock emits a named resolution");
    let RegolithState::Craft(shooter) = executor.state(attacker).expect("shooter remains") else {
        panic!("shooter remains a craft")
    };
    let RegolithState::Rock(rock) = executor.state(target).expect("rock remains") else {
        panic!("target remains a rock")
    };
    (shooter.clone(), rock.clone(), resolution)
}

#[test]
fn rock_target_inside_drawn_arc_hits_through_emission_and_delivery() {
    let target = PersistId::new(2);
    let (shooter, rock, resolution) = fire_at_rock_through_executor(QPos {
        x: 1_000,
        y: 0,
        z: 0,
    });
    assert_eq!(
        resolution,
        Outcome::ShotResolved {
            attacker: PersistId::new(1),
            target,
            result: ShotResult::Hit,
        }
    );
    assert_eq!(shooter.shots, 1);
    assert!(rock.hull < rock.tier.limits().max_hull);
}

#[test]
fn rock_target_outside_drawn_arc_refuses_through_emission_and_delivery() {
    let target = PersistId::new(2);
    let (_, rock, resolution) = fire_at_rock_through_executor(QPos {
        x: 0,
        y: 0,
        z: 1_000,
    });
    assert_eq!(
        resolution,
        Outcome::ShotResolved {
            attacker: PersistId::new(1),
            target,
            result: ShotResult::OutOfArc,
        }
    );
    assert_eq!(rock.hull, rock.tier.limits().max_hull);
}

#[test]
fn holding_lock_without_fire_produces_no_damage_over_many_ticks() {
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    let run = || {
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xAC; 32]));
        executor.insert(locker, RegolithState::Craft(craft_at(0)));
        let mut hashes = Vec::new();
        for tick in 1..=u64::from(LOCK_ACQUISITION_TICKS) * 4 {
            let output = executor
                .step_entity(locker, Tick::new(tick), &[Order::Lock { target }])
                .expect("locker exists");
            assert!(output.neighbor_reads.is_empty());
            assert!(
                output
                    .events
                    .iter()
                    .all(|event| matches!(event, Outcome::LockRequested { .. })),
                "lock alone produced a combat outcome at tick {tick}"
            );
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
            lock_class: None,
            locks_acquired: 0,
            ..
        }) if locked == target
    ));
    let RegolithState::Craft(live) = live else {
        panic!("locker remains a craft")
    };
    assert_eq!(live.shots, 0, "lock alone must never spend a shot");
    assert_eq!(live.damage_dealt, 0, "lock alone must never roll damage");
}

fn locked_craft(target: PersistId) -> Craft {
    let mut craft = craft_at(0);
    craft.lock_target = Some(target);
    craft.lock_class = Some(orrery_games::regolith::state::LockClass::Ship);
    craft.lock_progress = LOCK_ACQUISITION_TICKS;
    craft.locks_acquired = 1;
    craft
}

fn run_lock_round_trip(
    target_state: Option<RegolithState>,
    ticks: u64,
) -> (Executor<Regolith>, Vec<Outcome>) {
    let game = Regolith::honest();
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    let mut executor = Executor::new(game, UniverseSeed([0x42; 32]));
    executor.insert(locker, RegolithState::Craft(craft_at(0)));
    if let Some(state) = target_state {
        executor.insert(target, state);
    }
    let mut pending = BTreeMap::<PersistId, Vec<Order>>::new();
    let mut observed = Vec::new();
    for raw in 1..=ticks {
        let mut next = BTreeMap::<PersistId, Vec<Order>>::new();
        for entity in [locker, target] {
            if executor.state(entity).is_none() {
                continue;
            }
            let mut inputs = pending.remove(&entity).unwrap_or_default();
            if entity == locker {
                inputs.push(Order::Lock { target });
            }
            let output = executor
                .step_entity(entity, Tick::new(raw), &inputs)
                .expect("handshake entity exists");
            for event in output.events {
                if let Some((recipient, order)) = game.deliver(&event) {
                    next.entry(recipient).or_default().push(order);
                }
                observed.push(event);
            }
        }
        pending = next;
    }
    (executor, observed)
}

#[test]
fn a_rock_confirms_a_lock_and_the_reticle_class_is_rock() {
    let rock = Rock::spawned(
        RockTier::Large,
        0,
        QPos {
            x: 100_000,
            y: 0,
            z: 0,
        },
        QVel::default(),
    );
    let (executor, _) = run_lock_round_trip(
        Some(RegolithState::Rock(rock)),
        u64::from(LOCK_ACQUISITION_TICKS),
    );
    assert!(matches!(
        executor.state(PersistId::new(1)),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(target),
            lock_class: Some(LockClass::Rock),
            lock_progress: LOCK_ACQUISITION_TICKS,
            locks_acquired: 1,
            ..
        })) if *target == PersistId::new(2)
    ));
}

#[test]
fn a_pickup_refuses_a_lock_and_the_locker_clears_it() {
    let pickup = Pickup::spawned(QPos::default(), WeaponKind::Heavy, 90);
    let (executor, events) = run_lock_round_trip(Some(RegolithState::Pickup(pickup)), 3);
    assert!(events
        .iter()
        .any(|event| matches!(event, Outcome::LockRefused { .. })));
    assert!(matches!(
        executor.state(PersistId::new(1)),
        Some(RegolithState::Craft(Craft {
            lock_target: None,
            lock_class: None,
            lock_progress: 0,
            ..
        }))
    ));
}

#[test]
fn a_lock_on_a_missing_id_never_matures() {
    let (mut executor, _) = run_lock_round_trip(None, u64::from(LOCK_ACQUISITION_TICKS));
    let locker = PersistId::new(1);
    let output = executor
        .step_entity(locker, Tick::new(31), &[Order::Fire])
        .expect("locker exists");
    assert_eq!(
        output.events,
        [Outcome::ShotRefused {
            attacker: locker,
            result: ShotResult::NoLock,
        }]
    );
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(target),
            lock_class: None,
            lock_progress: LOCK_ACQUISITION_TICKS,
            locks_acquired: 0,
            ..
        })) if *target == PersistId::new(2)
    ));
}

#[test]
fn switching_between_classes_restarts_acquisition_and_reconfirms() {
    let rock = Rock::spawned(
        RockTier::Large,
        0,
        QPos {
            x: 100_000,
            y: 0,
            z: 0,
        },
        QVel::default(),
    );
    let (mut executor, _) = run_lock_round_trip(
        Some(RegolithState::Rock(rock)),
        u64::from(LOCK_ACQUISITION_TICKS),
    );
    let locker = PersistId::new(1);
    let ship = PersistId::new(3);
    executor.insert(ship, RegolithState::Craft(craft_at(200_000)));
    let switched = executor
        .step_entity(
            locker,
            Tick::new(31),
            &[
                Order::LockConfirmed {
                    target: PersistId::new(2),
                    class: LockClass::Rock,
                },
                Order::Lock { target: ship },
            ],
        )
        .expect("locker exists");
    assert!(matches!(
        switched.events.as_slice(),
        [Outcome::LockRequested { locker: who, target }]
            if *who == locker && *target == ship
    ));
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(target),
            lock_class: None,
            lock_progress: 1,
            ..
        })) if *target == ship
    ));
    executor
        .step_entity(
            locker,
            Tick::new(32),
            &[
                Order::Lock { target: ship },
                Order::LockConfirmed {
                    target: ship,
                    class: LockClass::Ship,
                },
            ],
        )
        .expect("locker exists");
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(target),
            lock_class: Some(LockClass::Ship),
            lock_progress: 2,
            ..
        })) if *target == ship
    ));
}

#[test]
fn rock_locks_do_not_yet_decay_behind_cover() {
    let target = PersistId::new(2);
    let locker = PersistId::new(1);
    let cover = PersistId::new(3);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x43; 32]));
    executor.insert_observed(
        target,
        RegolithState::Rock(Rock::spawned(
            RockTier::Large,
            0,
            QPos {
                x: 100_000,
                y: 0,
                z: 0,
            },
            QVel::default(),
        )),
        Tick::new(10),
    );
    let mut held = locked_craft(target);
    held.lock_class = Some(LockClass::Rock);
    executor.insert_observed(locker, RegolithState::Craft(held), Tick::new(10));
    executor.insert_observed(
        cover,
        RegolithState::Rock(Rock::spawned(
            RockTier::Small,
            0,
            QPos {
                x: 50_000,
                y: 0,
                z: 0,
            },
            QVel::default(),
        )),
        Tick::new(10),
    );
    let output = executor
        .step_entity(
            target,
            Tick::new(11),
            &[Order::ClaimCover {
                locker,
                rock: cover,
            }],
        )
        .expect("rock target exists");
    assert!(output.events.is_empty());
    assert!(output.neighbor_reads.is_empty());
}

#[test]
fn fire_without_a_mature_lock_is_a_named_refusal() {
    let shooter = PersistId::new(1);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xF1; 32]));
    executor.insert(shooter, RegolithState::Craft(craft_at(0)));

    let output = executor
        .step_entity(shooter, Tick::new(1), &[Order::Fire])
        .expect("shooter exists");

    assert_eq!(
        output.events,
        [Outcome::ShotRefused {
            attacker: shooter,
            result: ShotResult::NoLock,
        }]
    );
    assert!(matches!(
        executor.state(shooter),
        Some(RegolithState::Craft(Craft {
            shots: 0,
            damage_dealt: 0,
            ..
        }))
    ));
}

#[test]
fn fire_action_with_a_mature_lock_emits_the_existing_damage_path() {
    let shooter = PersistId::new(1);
    let target = PersistId::new(2);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xF2; 32]));
    executor.insert(shooter, RegolithState::Craft(locked_craft(target)));

    let output = executor
        .step_entity(shooter, Tick::new(1), &[Order::Fire])
        .expect("shooter exists");

    assert!(matches!(
        output.events.as_slice(),
        [Outcome::DamageDealt {
            attacker,
            target: fired_at,
            amount: 10..=13,
            ..
        }] if *attacker == shooter && *fired_at == target
    ));
    assert!(matches!(
        executor.state(shooter),
        Some(RegolithState::Craft(Craft {
            shots: 1,
            cooldown: 20,
            ..
        }))
    ));
}

#[test]
fn lock_switch_and_fire_are_applied_in_input_order() {
    let shooter = PersistId::new(1);
    let first_target = PersistId::new(2);
    let second_target = PersistId::new(3);
    let run = |orders: &[Order]| {
        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0xF3; 32]));
        executor.insert(shooter, RegolithState::Craft(locked_craft(first_target)));
        let output = executor
            .step_entity(shooter, Tick::new(1), orders)
            .expect("shooter exists");
        let state = executor.state(shooter).expect("shooter remains").clone();
        (state, output.events)
    };

    let (switch_then_fire, switch_then_fire_events) = run(&[
        Order::Lock {
            target: second_target,
        },
        Order::Fire,
    ]);
    assert!(matches!(
        switch_then_fire_events.as_slice(),
        [
            Outcome::LockRequested { locker, target },
            Outcome::ShotRefused {
                attacker,
                result: ShotResult::NoLock,
            }
        ] if *locker == shooter && *target == second_target && *attacker == shooter
    ));
    assert!(matches!(
        switch_then_fire,
        RegolithState::Craft(Craft {
            lock_target: Some(target),
            lock_progress: 1,
            shots: 0,
            ..
        }) if target == second_target
    ));

    let (fire_then_switch, fire_then_switch_events) = run(&[
        Order::Fire,
        Order::Lock {
            target: second_target,
        },
    ]);
    assert!(matches!(
        fire_then_switch_events.as_slice(),
        [
            Outcome::DamageDealt { target: fired_at, .. },
            Outcome::LockRequested { locker, target }
        ] if *fired_at == first_target && *locker == shooter && *target == second_target
    ));
    assert!(matches!(
        fire_then_switch,
        RegolithState::Craft(Craft {
            lock_target: Some(target),
            lock_progress: 1,
            shots: 1,
            ..
        }) if target == second_target
    ));
}

#[test]
fn lock_on_a_different_target_switches_and_restarts_acquisition() {
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
            &[Order::Lock {
                target: second_target,
            }],
        )
        .expect("locker exists");
    assert!(matches!(
        switched.events.as_slice(),
        [Outcome::LockRequested { locker: who, target }]
            if *who == locker && *target == second_target
    ));
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
                &[Order::Lock {
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
            &[Order::Lock {
                target: second_target,
            }],
        )
        .expect("locker exists");
    assert!(
        acquired.events.is_empty(),
        "re-acquired lock must not fire without an action"
    );
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: Some(locked),
            lock_progress: LOCK_ACQUISITION_TICKS,
            lock_class: None,
            locks_acquired: 0,
            ..
        })) if *locked == second_target
    ));
}

#[test]
fn lock_on_the_same_target_keeps_banked_acquisition() {
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x58; 32]));
    let mut start = craft_at(0);
    start.lock_target = Some(target);
    start.lock_progress = LOCK_ACQUISITION_TICKS / 2;
    executor.insert(locker, RegolithState::Craft(start));
    executor
        .step_entity(locker, Tick::new(1), &[Order::Lock { target }])
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
fn claimed_cover_behind_empty_space_is_rejected() {
    let target = PersistId::new(1);
    let locker = PersistId::new(2);
    let rock = PersistId::new(3);
    let mut target_state = craft_at(100_000);
    target_state.pos.y = 0;
    let mut locker_state = locked_craft(target);
    locker_state.pos = QPos::default();
    let mut off_axis = Rock::spawned(
        RockTier::Small,
        0,
        QPos {
            x: 50_000,
            y: 50_000,
            z: 0,
        },
        QVel::default(),
    );
    off_axis.hull = off_axis.tier.limits().max_hull;

    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x44; 32]));
    executor.insert_observed(target, RegolithState::Craft(target_state), Tick::new(100));
    executor.insert_observed(locker, RegolithState::Craft(locker_state), Tick::new(99));
    executor.insert_observed(rock, RegolithState::Rock(off_axis), Tick::new(98));
    let outcome = executor
        .step_entity(
            target,
            Tick::new(100),
            &[Order::ClaimCover { locker, rock }],
        )
        .expect("target exists");

    assert_eq!(outcome.neighbor_reads, vec![locker, rock]);
    assert_eq!(outcome.neighbor_frames[0].observed_tick, Tick::new(99));
    assert_eq!(outcome.neighbor_frames[1].observed_tick, Tick::new(98));
    assert!(outcome.events.contains(&Outcome::LockVisibility {
        locker,
        target,
        occluded: false,
    }));
    assert!(matches!(
        executor.state(target),
        Some(RegolithState::Craft(Craft {
            last_cover_occluded: false,
            ..
        }))
    ));
}

#[test]
fn occluded_lock_decays_over_time_and_visibility_restores_it() {
    let locker = PersistId::new(1);
    let target = PersistId::new(2);
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x45; 32]));
    executor.insert(locker, RegolithState::Craft(locked_craft(target)));

    executor
        .step_entity(
            locker,
            Tick::new(1),
            &[Order::LockVisibility {
                target,
                occluded: true,
            }],
        )
        .expect("locker exists");
    assert!(
        matches!(
            executor.state(locker),
            Some(RegolithState::Craft(Craft {
                lock_target: Some(found),
                lock_decay_progress: LOCK_DECAY_PER_TICK,
                ..
            })) if *found == target
        ),
        "one occluded tick must start decay, not drop the lock"
    );

    for tick in 2..u64::from(LOCK_BREAK_TICKS / 2) {
        executor
            .step_entity(locker, Tick::new(tick), &[])
            .expect("locker exists");
    }
    executor
        .step_entity(
            locker,
            Tick::new(u64::from(LOCK_BREAK_TICKS / 2)),
            &[Order::LockVisibility {
                target,
                occluded: false,
            }],
        )
        .expect("locker exists");
    assert!(
        matches!(
            executor.state(locker),
            Some(RegolithState::Craft(Craft {
                lock_target: Some(found),
                lock_progress: LOCK_ACQUISITION_TICKS,
                lock_decay_progress: 0,
                ..
            })) if *found == target
        ),
        "restored visibility must restore the held lock before decay completes"
    );

    executor
        .step_entity(
            locker,
            Tick::new(100),
            &[Order::LockVisibility {
                target,
                occluded: true,
            }],
        )
        .expect("locker exists");
    for tick in 101..100 + u64::from(LOCK_BREAK_TICKS) {
        executor
            .step_entity(locker, Tick::new(tick), &[])
            .expect("locker exists");
    }
    assert!(matches!(
        executor.state(locker),
        Some(RegolithState::Craft(Craft {
            lock_target: None,
            lock_progress: 0,
            lock_decay_progress: 0,
            ..
        }))
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
            .step_entity(locker, Tick::new(1), &[Order::Fire])
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

#[test]
fn target_outrunning_in_flight_projectile_breaks_lock_for_range() {
    let game = Regolith::honest();
    let shooter = PersistId::new(1);
    let target = PersistId::new(2);
    let mut target_state = craft_at(300_000);
    target_state.vel = QVel {
        x: 120_000,
        y: 0,
        z: 0,
    };
    let mut executor = Executor::new(game, UniverseSeed([0x51; 32]));
    executor.insert(shooter, RegolithState::Craft(locked_craft(target)));
    executor.insert(target, RegolithState::Craft(target_state));

    let fired = executor
        .step_entity(shooter, Tick::new(1), &[Order::Fire])
        .expect("shooter fires");
    let (_, mut continuation) = game
        .deliver(
            fired
                .events
                .first()
                .expect("locked fire emits a projectile"),
        )
        .expect("projectile is delivered");

    let mut range_break = None;
    for tick in 2..=100 {
        let target_outcome = executor
            .step_entity(target, Tick::new(tick), &[continuation])
            .expect("target adjudicates projectile delivery");
        if let Some(event) = target_outcome.events.iter().find(|event| {
            matches!(
                event,
                Outcome::LockBroken {
                    locker,
                    target: broken_target,
                    reason: LockBreakReason::RangeExceeded,
                } if *locker == shooter && *broken_target == target
            )
        }) {
            range_break = Some((tick, event.clone()));
            break;
        }
        let (recipient, next) = target_outcome
            .events
            .iter()
            .find_map(|event| game.deliver(event))
            .expect("in-flight projectile continues until range breaks");
        assert_eq!(recipient, target);
        continuation = next;
    }

    let (break_tick, break_event) = range_break.expect("target outruns the projectile range");
    assert!(break_tick > 2, "range break happens during flight");
    let (recipient, break_order) = game.deliver(&break_event).expect("break is delivered");
    assert_eq!(recipient, shooter);
    executor
        .step_entity(shooter, Tick::new(break_tick + 1), &[break_order])
        .expect("shooter consumes the range break");
    assert!(matches!(
        executor.state(shooter),
        Some(RegolithState::Craft(Craft {
            lock_target: None,
            lock_progress: 0,
            lock_decay_progress: 0,
            ..
        }))
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
                    from_vel: QVel::default(),
                    from_yaw_urad: 0,
                    from_archetype: Archetype::Interceptor,
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
        Order::Lock { target: pickup },
        Order::Fire,
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
            from_yaw_urad: 4,
            from_archetype: Archetype::Cruiser,
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
        Order::ClaimCover {
            locker: ship,
            rock: pickup,
        },
        Order::LockVisibility {
            target: ship,
            occluded: true,
        },
        Order::ShotResolved {
            target: pickup,
            result: ShotResult::OutOfArc,
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
            attacker_yaw_urad: 4,
            attacker_archetype: Archetype::Cruiser,
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
        Outcome::LockVisibility {
            locker: ship,
            target: pickup,
            occluded: true,
        },
        Outcome::ShotResolved {
            attacker: ship,
            target: pickup,
            result: ShotResult::OutOfArc,
        },
        Outcome::ShotRefused {
            attacker: ship,
            result: ShotResult::NoLock,
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

// ── the sections as types (#791) ────────────────────────────────────────────
//
// `Sectioned` (#789) told a decomposing host which component to file a value
// in. It could not change what any consumer was *handed*, because a
// `StateSection` is a string and a string cannot appear in a signature — so
// every consumer kept matching over all four sections, including the arms that
// existed only to yield a value the caller discarded. `Section` is the same
// four sections named as types, and these are the tests that hold it to the one
// property the whole thing rests on: a section hands out its own state and
// nothing else.

/// One value of every section, so the exactness law is tested in both
/// directions rather than only where it is convenient.
fn one_of_every_section() -> [RegolithState; 4] {
    [
        RegolithState::Craft(craft_at(0)),
        RegolithState::Rock(Rock::spawned(
            RockTier::Small,
            0,
            QPos::default(),
            QVel::default(),
        )),
        RegolithState::Pickup(Pickup::spawned(
            QPos::default(),
            WeaponKind::Stock,
            PICKUP_TTL_TICKS,
        )),
        RegolithState::BloomDirector(BloomDirector::spawned()),
    ]
}

#[test]
fn section_projection_is_exact_for_every_section() {
    // The `Some` half alone proves nothing: a projection that answered `Some`
    // for every value would pass it and would silently widen every check
    // written against it. `assert_section_is_exact` compares against
    // `Sectioned::section`, and this runs all four markers over all four
    // values, so the sixteen-cell cross product is covered rather than the
    // four cells on the diagonal.
    for state in &one_of_every_section() {
        assert_section_is_exact::<CraftSection>(state);
        assert_section_is_exact::<RockSection>(state);
        assert_section_is_exact::<PickupSection>(state);
        assert_section_is_exact::<BloomDirectorSection>(state);
    }
}

#[test]
fn the_section_accessor_hands_out_one_section_and_refuses_the_others() {
    // `TickBackend::section_state` is the seam #791 widened. What makes it
    // worth having is not that it can narrow, but that it *refuses*: an entity
    // filed under one section must read as absent through every other, or a
    // check written for crafts would start seeing rocks.
    let mut executor = Executor::new(Regolith::honest(), UniverseSeed([0x79; 32]));
    let craft = PersistId::new(1);
    let rock = PersistId::new(2);
    let pickup = PersistId::new(3);
    let director = PersistId::new(4);

    executor.insert(craft, RegolithState::Craft(craft_at(0)));
    executor.insert(
        rock,
        RegolithState::Rock(Rock::spawned(
            RockTier::Small,
            0,
            QPos::default(),
            QVel::default(),
        )),
    );
    executor.insert(
        pickup,
        RegolithState::Pickup(Pickup::spawned(
            QPos::default(),
            WeaponKind::Stock,
            PICKUP_TTL_TICKS,
        )),
    );
    executor.insert(
        director,
        RegolithState::BloomDirector(BloomDirector::spawned()),
    );

    assert!(
        TickBackend::<Regolith>::section_state::<CraftSection>(&executor, craft).is_some(),
        "the craft reads through its own section",
    );
    assert!(
        TickBackend::<Regolith>::section_state::<CraftSection>(&executor, rock).is_none(),
        "a rock must not read as a craft",
    );
    assert!(
        TickBackend::<Regolith>::section_state::<CraftSection>(&executor, pickup).is_none(),
        "a pickup must not read as a craft",
    );
    assert!(
        TickBackend::<Regolith>::section_state::<CraftSection>(&executor, director).is_none(),
        "a bloom director must not read as a craft",
    );
    assert!(
        TickBackend::<Regolith>::section_state::<RockSection>(&executor, rock).is_some(),
        "the rock reads through its own section",
    );
    assert!(
        TickBackend::<Regolith>::section_state::<RockSection>(&executor, craft).is_none(),
        "a craft must not read as a rock",
    );

    // Absent is absent in every section, not just the one that would have held
    // it. A section accessor that answered for an uninstalled entity would be
    // inventing a population.
    let never_installed = PersistId::new(99);
    assert!(
        TickBackend::<Regolith>::section_state::<CraftSection>(&executor, never_installed)
            .is_none()
    );
    assert!(
        TickBackend::<Regolith>::section_state::<RockSection>(&executor, never_installed).is_none()
    );
}

#[test]
fn a_section_lifted_check_runs_on_its_own_section_and_passes_every_other() {
    // The behavioural half of the win. `regolith/speed-cap` is registered once
    // per moving section, so it must still catch an impossible speed in both —
    // and the sections that used to reach the two zero-yielding arms must now
    // reach no check at all rather than a comparison against zero.
    let mut fast_craft = craft_at(0);
    fast_craft.vel = QVel {
        x: 10_000_000,
        y: 0,
        z: 0,
    };
    let fast_craft = RegolithState::Craft(fast_craft);
    assert_eq!(
        evaluate(INVARIANTS, &sample(None, &fast_craft))
            .expect_err("an impossible craft speed is still caught")
            .kind,
        InvariantKind::SpeedCap,
    );

    let fast_rock = RegolithState::Rock(Rock::spawned(
        RockTier::Small,
        0,
        QPos::default(),
        QVel {
            x: 10_000_000,
            y: 0,
            z: 0,
        },
    ));
    assert_eq!(
        evaluate(INVARIANTS, &sample(None, &fast_rock))
            .expect_err("an impossible rock speed is still caught")
            .kind,
        InvariantKind::SpeedCap,
    );

    // A pickup and a director are legal, and reach neither instantiation of
    // the speed cap: there is no `SpeedLimited` impl for either, so there is
    // nowhere left to write the zero the old comparison discarded.
    for state in &one_of_every_section()[2..] {
        assert_eq!(
            evaluate(INVARIANTS, &sample(None, state)),
            Ok(()),
            "a motionless section passes every lifted check",
        );
    }
}
