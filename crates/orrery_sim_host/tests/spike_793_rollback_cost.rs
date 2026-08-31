//! **SPIKE #793 — propose-only. Do not merge.**
//!
//! The rollback axis, measured rather than predicted.
//!
//! D8 gives prediction a **9-tick rollback window** (`orrery_predict::config`
//! `rollback_ticks: 9`) guarded by `RollbackBudget`, whose `step_cost` is an
//! EWMA of one observed predicted-subset fixed step and whose default target is
//! ≈ 1 ms. `ResimPlan` spends at most `max_resim_per_frame` (5 ms) per render
//! frame over at most `max_amortize_frames` (2) frames.
//!
//! A rollback is three things, not one:
//!
//! 1. **restore** the world to the authoritative tick at the back of the window,
//! 2. **resimulate** forward up to nine ticks,
//! 3. keep a **snapshot** per tick so step 1 is possible at all.
//!
//! The current architecture has only ever had to answer for step 2. Steps 1 and
//! 3 are where an ECS substrate differs from a `BTreeMap`, because
//! `bevy_ecs::World` has no `Clone` — verified against `bevy_ecs` 0.19.1, which
//! carries no `impl Clone for World` — so a world snapshot is necessarily
//! per-entity traffic through the seam rather than a container copy.
//!
//! This file measures all three on both backends over identical inputs, and
//! asserts the two agree byte-for-byte after the resim. Timings are printed,
//! not asserted: a wall-clock threshold in a test is a flake, and the numbers
//! are evidence for the doc, not a gate.
//!
//! Run with:
//! `cargo test -p orrery_sim_host --release --test spike_793_rollback_cost -- --nocapture`

use std::time::{Duration, Instant};

use blake3::Hasher;
use orrery_core::{CoreCodec, Executor, SealedTickInputs, TickBackend};
use orrery_games::regolith::Regolith;
use orrery_games::Game;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;

/// D8's window (`orrery_predict::config::PredictConfig::rollback_ticks`).
const ROLLBACK_TICKS: u64 = 9;
/// Ticks run forward before the rollback, so the window has history behind it.
const WARMUP_TICKS: u64 = 30;
/// Populations measured. 512 is well past a plausible predicted subset and is
/// there to show the shape of the curve, not to claim a target.
const POPULATIONS: [u64; 3] = [32, 128, 512];
/// Repeats per measurement, so a single scheduler hiccup does not become the
/// reported number. The minimum is reported, not the mean: the minimum is the
/// one the machine can actually do.
const REPEATS: usize = 9;
const T0: u64 = 4_000_000;

fn seed() -> UniverseSeed {
    UniverseSeed([0x93; 32])
}

fn population(count: u64) -> Vec<(PersistId, orrery_games::regolith::state::RegolithState)> {
    (0..count)
        .map(|slot| {
            let entity = PersistId::new(slot + 1);
            (entity, Regolith::honest().spawn(entity, slot))
        })
        .collect()
}

/// A full canonical snapshot, as the seam permits one to be taken.
///
/// `TickBackend` exposes `state`, `entities`, `insert_observed` and
/// `take_state` and nothing else, so this is the *only* shape a snapshot can
/// have on either backend — which is itself part of the finding. Note what it
/// cannot capture: neither backend exposes a read of an entity's stored
/// observation tick, so a faithful restore has to be handed the tick from
/// outside. On the `Executor` that is an inconvenience; on the ECS it is the
/// same inconvenience, so the two are comparable.
type Snapshot = Vec<(PersistId, orrery_games::regolith::state::RegolithState)>;

fn snapshot<B: TickBackend<Regolith>>(backend: &B) -> Snapshot {
    backend
        .entities()
        .into_iter()
        .map(|entity| {
            (
                entity,
                backend
                    .state(entity)
                    .expect("entities() named an installed entity")
                    .clone(),
            )
        })
        .collect()
}

fn restore<B: TickBackend<Regolith>>(backend: &mut B, snapshot: &Snapshot, observed: Tick) {
    // Everything born since the snapshot has to leave, or the resimulated
    // population is not the one the authority had.
    for entity in backend.entities() {
        if !snapshot.iter().any(|(held, _)| *held == entity) {
            backend.take_state(entity);
        }
    }
    for (entity, state) in snapshot {
        backend.insert_observed(*entity, state.clone(), observed);
    }
}

/// The canonical chain over a run: `blake3(chain ‖ state_hash)` per entity-tick
/// in step order. Identical in shape to the fold in
/// `tier_h_projection_differential.rs`.
fn fold(chain: [u8; 32], hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(&chain);
    hasher.update(hash);
    *hasher.finalize().as_bytes()
}

/// A projection over the whole population in ascending `PersistId` order —
/// the only order clause (c)(4) lets anything leave the canonical context in.
fn project<B: TickBackend<Regolith>>(backend: &B) -> [u8; 32] {
    let mut hasher = Hasher::new();
    for entity in backend.entities() {
        let state = backend.state(entity).expect("installed");
        hasher.update(&entity.0.to_le_bytes());
        hasher.update(&state.to_canonical());
    }
    *hasher.finalize().as_bytes()
}

/// What one backend cost, and what it produced.
struct Measured {
    snapshot: Duration,
    restore: Duration,
    resim: Duration,
    chain: [u8; 32],
    projection: [u8; 32],
    entities: usize,
}

impl Measured {
    /// The whole rollback, as `RollbackBudget` would have to charge for it.
    fn total(&self) -> Duration {
        self.snapshot + self.restore + self.resim
    }
}

fn measure<B, N>(count: u64, new: N) -> Measured
where
    B: TickBackend<Regolith>,
    N: Fn() -> B,
{
    let mut best: Option<Measured> = None;
    for _ in 0..REPEATS {
        let mut backend = new();
        for (entity, state) in population(count) {
            backend.insert(entity, state);
        }
        for offset in 0..WARMUP_TICKS {
            backend.step_tick(Tick::new(T0 + offset), &SealedTickInputs::default());
        }

        // The back of the window. This is the state a late authoritative
        // update forces the predictor back to.
        let at = Tick::new(T0 + WARMUP_TICKS);
        let started = Instant::now();
        let held = snapshot(&backend);
        let snapshot_cost = started.elapsed();

        // Run forward through the window once, so the rollback below is a
        // genuine re-execution of ticks already executed.
        let mut forward = [0u8; 32];
        for offset in 0..ROLLBACK_TICKS {
            for stepped in backend.step_tick(Tick::new(at.0 + offset), &SealedTickInputs::default())
            {
                forward = fold(forward, &stepped.outcome.state_hash);
            }
        }
        let forward_projection = project(&backend);

        let started = Instant::now();
        restore(&mut backend, &held, at);
        let restore_cost = started.elapsed();

        let started = Instant::now();
        let mut chain = [0u8; 32];
        for offset in 0..ROLLBACK_TICKS {
            for stepped in backend.step_tick(Tick::new(at.0 + offset), &SealedTickInputs::default())
            {
                chain = fold(chain, &stepped.outcome.state_hash);
            }
        }
        let resim_cost = started.elapsed();

        assert_eq!(
            forward, chain,
            "the resimulated window did not reproduce the forward run's chain — rollback on this \
             backend is not deterministic, which is a correctness failure and not a cost one"
        );
        let projection = project(&backend);
        assert_eq!(
            forward_projection, projection,
            "the resimulated window ended in a different state than the forward run"
        );

        let entities = backend.entities().len();
        let candidate = Measured {
            snapshot: snapshot_cost,
            restore: restore_cost,
            resim: resim_cost,
            chain,
            projection,
            entities,
        };
        best = Some(match best {
            None => candidate,
            Some(held) if candidate.total() < held.total() => candidate,
            Some(held) => held,
        });
    }
    best.expect("REPEATS is non-zero")
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

/// The measurement. Prints a table; asserts only what is a correctness
/// property, never a wall-clock threshold.
#[test]
fn the_nine_tick_rollback_window_costs_this_much_on_each_backend() {
    println!(
        "\n#793 rollback cost — {ROLLBACK_TICKS}-tick window, best of {REPEATS}, \
         microseconds\n"
    );
    println!(
        "{:>6}  {:>7}  {:>26}  {:>26}  {:>8}",
        "N", "alive", "Executor (snap/rest/resim)", "EcsBackend (snap/rest/resim)", "ratio"
    );
    for count in POPULATIONS {
        let executor = measure(count, || Executor::new(Regolith::honest(), seed()));
        let ecs = measure(count, || EcsBackend::new(Regolith::honest(), seed()));

        assert_eq!(
            executor.chain, ecs.chain,
            "the two backends disagreed on the resimulated window's canonical chain at N={count}"
        );
        assert_eq!(
            executor.projection, ecs.projection,
            "the two backends disagreed on the post-rollback projection at N={count}"
        );
        assert_eq!(executor.entities, ecs.entities);

        println!(
            "{count:>6}  {:>7}  {:>8.1}{:>9.1}{:>9.1}  {:>8.1}{:>9.1}{:>9.1}  {:>8.2}",
            executor.entities,
            micros(executor.snapshot),
            micros(executor.restore),
            micros(executor.resim),
            micros(ecs.snapshot),
            micros(ecs.restore),
            micros(ecs.resim),
            ecs.total().as_secs_f64() / executor.total().as_secs_f64().max(f64::EPSILON),
        );
    }
    println!();
}

/// D8's guard reads a per-tick cost, so report the per-tick cost the guard
/// would actually see, against the 1 ms target `RollbackBudget::step_cost`
/// defaults to and the 5 ms `max_resim_per_frame`.
#[test]
fn the_per_tick_resim_cost_against_d8s_budget() {
    println!("\n#793 per-tick resim cost vs D8 budget (step_cost target 1 ms)\n");
    println!(
        "{:>6}  {:>16}  {:>16}  {:>22}",
        "N", "Executor µs/tick", "Ecs µs/tick", "full window / 5 ms frame"
    );
    for count in POPULATIONS {
        let executor = measure(count, || Executor::new(Regolith::honest(), seed()));
        let ecs = measure(count, || EcsBackend::new(Regolith::honest(), seed()));
        let ecs_per_tick = micros(ecs.resim) / ROLLBACK_TICKS as f64;
        println!(
            "{count:>6}  {:>16.2}  {:>16.2}  {:>22.3}",
            micros(executor.resim) / ROLLBACK_TICKS as f64,
            ecs_per_tick,
            ecs.total().as_secs_f64() / 0.005,
        );
    }
    println!();
}
