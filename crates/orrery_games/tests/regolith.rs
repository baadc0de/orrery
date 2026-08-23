//! Regolith-specific checks for weapon state and planar input discipline.

use orrery_core::{evaluate, Executor, InvariantKind, InvariantSample, QPos};
use orrery_games::game::Game;
use orrery_games::regolith::{
    archetype::Archetype,
    invariants::INVARIANTS,
    order::{Order, Outcome},
    state::Craft,
    weapon::WeaponKind,
    Regolith, REGOLITH_RULESET,
};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use rand_chacha::rand_core::SeedableRng;

fn craft_at(x: i64) -> Craft {
    Craft::spawned(Archetype::Interceptor, QPos { x, y: 0, z: 0 }, 0)
}
fn sample<'a>(previous: Option<&'a Craft>, current: &'a Craft) -> InvariantSample<'a, Craft> {
    InvariantSample {
        entity: PersistId::new(1),
        current,
        tick: Tick::new(1),
        previous,
        elapsed_ticks: 3,
    }
}

#[test]
fn v1_weapon_table_and_ruleset_identity_are_pinned() {
    assert_eq!(REGOLITH_RULESET.version, 1);
    assert_eq!(WeaponKind::Stock.weapon().damage_base, 10);
    assert_eq!(WeaponKind::Volley.weapon().rolls, 3);
    assert_eq!(WeaponKind::Heavy.weapon().reach_mm, 900_000);
}

#[test]
fn relabelled_weapon_without_matching_hashed_state_fails_stage_one() {
    let honest = craft_at(0);
    let mut relabelled = honest.clone();
    // Mutate the guarded state field only. The invariant itself stays intact.
    relabelled.weapon = WeaponKind::Heavy;
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
    executor.insert(PersistId::new(1), shooter);
    executor.insert(PersistId::new(2), craft_at(1));
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
    assert_eq!(executor.state(PersistId::new(1)).unwrap().cooldown, 30);
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
