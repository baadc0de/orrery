//! D43 clause (e)(4): the ambiguity canary and the projection differential.
//!
//! Two halves of one clause, and the record is precise about both.
//!
//! **The canary** is clause (c)(1)'s: "the real schedule initializes Ok, a
//! deliberately un-ordered mutant initializes Err; both directions in CI".
//! One direction alone proves nothing — a rejector that rejects nothing
//! returns `Ok` for the real schedule too, and A3's probe ran an ambiguous
//! schedule 200/200 identical, so observed stability is not evidence either.
//!
//! **The differential** is the harder half:
//!
//! > permuted insertion orders must yield equal sorted-by-`PersistId`
//! > projection hashes matching the executor-computed chain, while agreement
//! > of naive query-order folds is deliberately *not* asserted (their
//! > agreement would be luck, not a property).
//!
//! Three obligations, and the file discharges each explicitly:
//!
//! 1. **Permute.** `bevy_ecs` is built here without `multi_threaded` and
//!    archetype iteration follows spawn history, so the permutation is applied
//!    to the *insertion* order — the order `TickBackend::insert` spawns
//!    entities into the `World`.
//! 2. **Assert the sorted projection.** Every permutation must produce the
//!    same per-tick sorted-by-`PersistId` projection hash, and the same
//!    canonical chain as the `Executor`.
//! 3. **Do not assert the naive fold.** No assertion in this file says two
//!    permutations' query-order folds agree. The file asserts the opposite
//!    where it can: that at least one pair of permutations *disagrees* on the
//!    naive fold, which is what proves the permutation actually reached the
//!    substrate. A differential that permuted nothing, or that folded both
//!    sides in the same accidental order, would pass every assertion in
//!    obligation 2 while measuring nothing — the exact "agreement would be
//!    luck" failure the record names.

use blake3::Hasher;
use orrery_core::{CoreCodec, Executor, TickBackend};
use orrery_games::regolith::Regolith;
use orrery_games::Game;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;

/// How many entities the permuted population holds.
const ENTITIES: u64 = 6;
/// How many ticks each permutation runs.
const TICKS: u64 = 24;
/// The first tick of a run.
const T0: u64 = 1_000_000;

fn seed() -> UniverseSeed {
    UniverseSeed([0x7c; 32])
}

fn regolith_ecs(game: Regolith, seed: UniverseSeed) -> EcsBackend<Regolith> {
    EcsBackend::new(game, seed).with_migrated_module(
        orrery_games::regolith::world_ecs::sync_migrated,
        orrery_games::regolith::world_ecs::step_migrated,
    )
}

/// The insertion orders under test, as slot indices.
///
/// More than one, and provably distinct — a single-row table would make every
/// "equal across permutations" assertion below a tautology.
fn permutations() -> Vec<Vec<u64>> {
    let ascending: Vec<u64> = (0..ENTITIES).collect();
    let descending: Vec<u64> = (0..ENTITIES).rev().collect();
    // A fixed interleave, not a shuffle: a random permutation would make a
    // failure unreproducible, which is the wrong property for a gate.
    let interleaved: Vec<u64> = (0..ENTITIES)
        .map(|slot| {
            if slot.is_multiple_of(2) {
                slot / 2
            } else {
                ENTITIES - 1 - slot / 2
            }
        })
        .collect();
    vec![ascending, descending, interleaved]
}

/// The population in ascending `PersistId` order — the projection order clause
/// (c)(4) fixes for anything leaving the canonical context.
fn ascending_population() -> Vec<PersistId> {
    (0..ENTITIES).map(|slot| PersistId::new(slot + 1)).collect()
}

/// What one run produced.
struct Run {
    /// The canonical chain: `blake3(chain ‖ state_hash)` folded over every
    /// entity-tick in step order. Identical in shape to
    /// `orrery_games::scenario`'s fold.
    chain: [u8; 32],
    /// Per tick, the projection hash over the population **sorted by
    /// `PersistId`**. This is the quantity clause (e)(4) binds.
    sorted_projections: Vec<[u8; 32]>,
    /// Per tick, the same fold taken in the substrate's own storage order.
    /// **Nothing asserts two runs agree on this**; it exists only to prove the
    /// permutation reached the substrate.
    naive_projections: Vec<[u8; 32]>,
    /// The substrate's archetype iteration order at the end of the run.
    storage_order: Vec<PersistId>,
}

/// Fold one entity-tick's state hash into the chain.
fn fold(chain: [u8; 32], hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(&chain);
    hasher.update(hash);
    *hasher.finalize().as_bytes()
}

/// Hash a projection: `(PersistId, canonical state bytes)` in the order given.
///
/// Order-sensitive on purpose — a projection hash that ignored order could not
/// distinguish the sorted projection from the naive one, and the whole clause
/// is about the difference between them.
fn project<B: TickBackend<Regolith>>(backend: &B, order: &[PersistId]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    for entity in order {
        let state = backend
            .state(*entity)
            .expect("the projection names an installed entity");
        hasher.update(&entity.0.to_le_bytes());
        hasher.update(&state.to_canonical());
    }
    *hasher.finalize().as_bytes()
}

/// Run the population on an `EcsBackend`, inserting in `insertion_order`.
fn run_on_ecs(insertion_order: &[u64]) -> Run {
    let mut backend = regolith_ecs(Regolith::honest(), seed());
    for slot in insertion_order {
        let entity = PersistId::new(slot + 1);
        backend.insert(entity, Regolith::honest().spawn(entity, *slot));
    }

    let mut chain = [0u8; 32];
    let mut sorted_projections = Vec::with_capacity(TICKS as usize);
    let mut naive_projections = Vec::with_capacity(TICKS as usize);
    for offset in 0..TICKS {
        let stepped = backend.step_tick(Tick::new(T0 + offset), &Default::default());
        for entry in &stepped {
            chain = fold(chain, &entry.outcome.state_hash);
        }
        // The sorted order is computed *here*, from the population, and not
        // taken from the backend: a host that reported its archetype order out
        // of `entities()` would otherwise have its lie followed into the
        // "sorted" projection and the clause would check nothing. The trait's
        // ascending-`PersistId` promise is then checked against it.
        let sorted = ascending_population();
        assert_eq!(
            TickBackend::entities(&backend),
            sorted,
            "the backend did not report its population in ascending PersistId order"
        );
        let naive = backend.storage_order_probe();
        assert_eq!(
            naive.len(),
            sorted.len(),
            "the storage-order probe lost or invented an entity"
        );
        sorted_projections.push(project(&backend, &sorted));
        naive_projections.push(project(&backend, &naive));
    }

    Run {
        chain,
        sorted_projections,
        naive_projections,
        storage_order: backend.storage_order_probe(),
    }
}

/// The same run on the `Executor` — the chain clause (e)(4) requires the ECS's
/// sorted projection to match.
fn run_on_executor() -> ([u8; 32], Vec<[u8; 32]>) {
    let mut backend = Executor::new(Regolith::honest(), seed());
    for slot in 0..ENTITIES {
        let entity = PersistId::new(slot + 1);
        TickBackend::insert(&mut backend, entity, Regolith::honest().spawn(entity, slot));
    }
    let mut chain = [0u8; 32];
    let mut sorted_projections = Vec::with_capacity(TICKS as usize);
    for offset in 0..TICKS {
        let stepped = backend.step_tick(Tick::new(T0 + offset), &Default::default());
        for entry in &stepped {
            chain = fold(chain, &entry.outcome.state_hash);
        }
        sorted_projections.push(project(&backend, &ascending_population()));
    }
    (chain, sorted_projections)
}

/// D43 (c)(1) / (e)(4), first half: the ambiguity rejector is awake, proven in
/// **both** directions against the schedule that actually ships.
#[test]
fn the_canonical_schedule_composes_unambiguously_and_the_unordered_mutant_does_not() {
    let real = EcsBackend::<Regolith>::ambiguity_audit(Regolith::honest(), seed());
    assert!(
        real.is_ok(),
        "the canonical tick schedule did not compose with ambiguity promoted to an error: {real:?}"
    );

    let mutant =
        EcsBackend::<Regolith>::ambiguity_audit_of_the_unordered_mutant(Regolith::honest(), seed());
    let Err(rendered) = mutant else {
        panic!(
            "the un-ordered canary mutant composed cleanly — `bevy_ecs`'s ambiguity rejector is \
             asleep, so the real schedule's clean composition is worth nothing"
        );
    };
    assert!(
        rendered.contains("Ambiguity"),
        "the canary mutant failed for a reason other than ambiguity, so this proves the wrong \
         rejector is awake: {rendered}"
    );

    let native = orrery_games::regolith::world_ecs::ambiguity_audit();
    assert!(
        native.is_ok(),
        "the migrated module's native schedule did not compose cleanly: {native:?}"
    );
    let native_mutant =
        orrery_games::regolith::world_ecs::ambiguity_audit_of_the_unordered_mutant();
    let Err(rendered) = native_mutant else {
        panic!("the migrated module's unordered native schedule escaped ambiguity detection");
    };
    assert!(
        rendered.contains("Ambiguity"),
        "the native canary failed for the wrong reason: {rendered}"
    );
}

/// D43 (e)(4), second half: the projection differential over permuted
/// insertion orders.
#[test]
fn permuted_insertion_orders_agree_on_the_sorted_projection_and_the_executor_chain() {
    let orders = permutations();
    assert!(
        orders.len() >= 2,
        "one insertion order is not a permutation table — every comparison below would be a \
         tautology"
    );
    for (index, order) in orders.iter().enumerate() {
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..ENTITIES).collect::<Vec<_>>(),
            "permutation {index} is not a permutation of the population"
        );
    }
    assert!(
        orders.windows(2).any(|pair| pair[0] != pair[1]),
        "every insertion order in the table is the same order — nothing is permuted"
    );

    let runs: Vec<Run> = orders.iter().map(|order| run_on_ecs(order)).collect();
    let (executor_chain, executor_projections) = run_on_executor();

    // ── Non-vacuity, before any agreement is claimed ─────────────────────
    assert_eq!(
        runs[0].sorted_projections.len(),
        TICKS as usize,
        "the run produced no per-tick projections"
    );
    assert!(
        runs[0].sorted_projections.first() != runs[0].sorted_projections.last(),
        "the population's projection never changed across {TICKS} ticks, so the differential \
         compares a constant"
    );
    // The permutation must have reached the substrate. If every run ends in
    // the same archetype order, the insertion permutation was absorbed
    // somewhere and this whole test is measuring nothing.
    assert!(
        runs.iter()
            .any(|run| run.storage_order != runs[0].storage_order),
        "every permutation ended in the same archetype iteration order {:?} — the permutation \
         never reached the substrate, so agreement below would be luck, not a property",
        runs[0].storage_order
    );
    // And it must be *observable* in a naive query-order fold. This is the
    // guard the record's "agreement would be luck" sentence is about: it
    // asserts the naive folds DISAGREE. Nothing here asserts they agree, and
    // nothing may be added that does.
    assert!(
        runs.iter()
            .any(|run| run.naive_projections != runs[0].naive_projections),
        "no permutation moved the naive query-order fold, so the sorted projection's agreement \
         is untested against storage-order dependence"
    );

    // ── The clause ───────────────────────────────────────────────────────
    for (index, run) in runs.iter().enumerate() {
        assert_eq!(
            run.sorted_projections, runs[0].sorted_projections,
            "permutation {index} produced a different sorted-by-PersistId projection than \
             permutation 0 — canonical output depends on insertion order (D43 (c)(4), (e)(4))"
        );
        assert_eq!(
            run.chain, executor_chain,
            "permutation {index}'s canonical chain does not match the executor-computed chain \
             (D43 (e)(4))"
        );
        assert_eq!(
            run.sorted_projections, executor_projections,
            "permutation {index}'s sorted-by-PersistId projection does not match the \
             executor's (D43 (e)(4))"
        );
    }
}
