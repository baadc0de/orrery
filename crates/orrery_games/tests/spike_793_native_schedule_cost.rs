//! **SPIKE #793 — propose-only. Do not merge.**
//!
//! The half of the rollback axis `orrery_sim_host`'s measurement cannot reach.
//!
//! `spike_793_rollback_cost.rs` measures the ECS as *storage*: canonical state
//! lives in components, but the tick is still `canonical_step` called once per
//! entity out of a `for` loop. That is not what going native means. Native
//! means a **tick is a `Schedule` run over a `World`**, and D8's rollback
//! window means running that schedule nine more times.
//!
//! Porting the whole of Regolith's rules — `regolith/mod.rs` (2 232 lines),
//! `craft.rs` (513), `world.rs` (391), `order.rs` (940), `visibility.rs` (438)
//! — was out of scope, so what is measured here is a **floor**: the cost of
//! dispatching a five-system chained schedule over a populated world, nine
//! times, with everything else held out. A real native Regolith would pay this
//! plus its arithmetic. The number is useful precisely because it is the part
//! that is *new* — the arithmetic is the same arithmetic either way.
//!
//! It also measures the thing `bevy_ecs` has no answer for: **world
//! construction**, which is what a rollback restore becomes when state *is*
//! components. `bevy_ecs` 0.19.1 carries no `impl Clone for World`, so there is
//! no container copy to take; a snapshot is per-entity traffic and a restore is
//! per-entity spawning, both of which move archetypes.
//!
//! Run with:
//! `cargo test -p orrery_games --release --test spike_793_native_schedule_cost -- --nocapture`

use std::time::{Duration, Instant};

use orrery_core::{QPos, QVel};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::native::{NativeInvariants, Sample};
use orrery_games::regolith::state::{BloomDirector, Craft, Pickup, RegolithState, Rock, RockTier};
use orrery_games::regolith::weapon::WeaponKind;
use orrery_protocol::{PersistId, Tick};

/// D8's window (`orrery_predict::config::PredictConfig::rollback_ticks`).
const ROLLBACK_TICKS: usize = 9;
const POPULATIONS: [usize; 3] = [32, 128, 512];
const REPEATS: usize = 9;
const TICK: Tick = Tick::new(2_000);

/// A mixed population in the proportions Regolith actually runs: mostly rocks,
/// some craft, a few pickups, one director per island.
fn population(count: usize) -> Vec<RegolithState> {
    (0..count)
        .map(|slot| {
            let at = QPos {
                x: (slot as i64) * 1_000,
                y: 0,
                z: (slot as i64) * 7,
            };
            match slot % 8 {
                0 | 1 => {
                    let mut craft = Craft::spawned(Archetype::Interceptor, at, 0);
                    craft.vel = QVel {
                        x: 1_000,
                        y: 0,
                        z: 0,
                    };
                    RegolithState::Craft(craft)
                }
                7 => RegolithState::Pickup(Pickup::spawned(at, WeaponKind::Stock, 60)),
                6 => RegolithState::BloomDirector(BloomDirector::spawned()),
                _ => RegolithState::Rock(Rock::spawned(
                    RockTier::Large,
                    0,
                    at,
                    QVel { x: 500, y: 0, z: 0 },
                )),
            }
        })
        .collect()
}

fn build(states: &[RegolithState]) -> NativeInvariants {
    let mut native = NativeInvariants::new(TICK);
    for (slot, state) in states.iter().enumerate() {
        native.insert(&Sample {
            entity: PersistId::new(slot as u64 + 1),
            current: state,
            tick: TICK,
            previous: Some(state),
            elapsed_ticks: 1,
        });
    }
    native
}

fn best_of(mut run: impl FnMut() -> Duration) -> Duration {
    (0..REPEATS).map(|_| run()).min().expect("REPEATS non-zero")
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

#[test]
fn a_native_tick_is_a_schedule_and_this_is_what_nine_of_them_cost() {
    println!(
        "\n#793 native schedule dispatch — 5 chained systems, {ROLLBACK_TICKS}-tick window, \
         best of {REPEATS}, microseconds\n"
    );
    println!(
        "{:>6}  {:>16}  {:>16}  {:>16}",
        "N", "world build", "1 schedule run", "9 runs (window)"
    );
    for count in POPULATIONS {
        let states = population(count);

        // World construction: what a rollback restore costs when canonical
        // state *is* components and there is no `World::clone` to take.
        let build_cost = best_of(|| {
            let started = Instant::now();
            let mut native = build(&states);
            let elapsed = started.elapsed();
            assert_eq!(native.len(), count);
            elapsed
        });

        // One schedule run over a world already built — the marginal cost of a
        // tick once the population is in place.
        let mut native = build(&states);
        native.run();
        let single = best_of(|| {
            native.reset_findings();
            let started = Instant::now();
            native.run();
            started.elapsed()
        });

        // The window: nine runs back to back, which is what D8's rollback
        // becomes when a tick is a schedule.
        let window = best_of(|| {
            native.reset_findings();
            let started = Instant::now();
            for _ in 0..ROLLBACK_TICKS {
                native.run();
            }
            started.elapsed()
        });

        println!(
            "{count:>6}  {:>16.2}  {:>16.2}  {:>16.2}",
            micros(build_cost),
            micros(single),
            micros(window),
        );
    }
    println!();
}

/// The finding this file exists to make legible: a native rollback restore has
/// to rebuild the world, and rebuilding is more expensive than the ticks it
/// makes possible.
#[test]
fn rebuilding_the_world_costs_more_than_the_window_it_enables() {
    let states = population(512);
    let build_cost = best_of(|| {
        let started = Instant::now();
        let mut native = build(&states);
        let elapsed = started.elapsed();
        assert!(!native.is_empty());
        elapsed
    });
    let mut native = build(&states);
    native.run();
    let window = best_of(|| {
        native.reset_findings();
        let started = Instant::now();
        for _ in 0..ROLLBACK_TICKS {
            native.run();
        }
        started.elapsed()
    });
    println!(
        "\n#793 N=512: world rebuild {:.2} µs vs {ROLLBACK_TICKS}-tick window {:.2} µs \
         (rebuild is {:.2}× the window)\n",
        micros(build_cost),
        micros(window),
        build_cost.as_secs_f64() / window.as_secs_f64().max(f64::EPSILON),
    );
    // Deliberately not asserted as a threshold: the ratio is the evidence, and
    // a wall-clock assertion here would be a flake. What *is* asserted is that
    // both were measurable at all.
    assert!(build_cost > Duration::ZERO && window > Duration::ZERO);
}
