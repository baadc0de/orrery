//! Regolith-specific checks for weapon state and planar input discipline.

use orrery_core::{evaluate, Executor, InvariantKind, InvariantSample, QPos, QVel};
use orrery_games::game::Game;
use orrery_games::regolith::{
    archetype::Archetype,
    invariants::INVARIANTS,
    order::{Order, Outcome},
    state::{Craft, RegolithState, Rock, RockTier},
    weapon::WeaponKind,
    Regolith, REGOLITH_RULESET,
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
fn v2_weapon_table_and_ruleset_identity_are_pinned() {
    assert_eq!(REGOLITH_RULESET.version, 2);
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
