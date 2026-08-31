//! The harness: play a game, record what an authority would have logged, and
//! re-execute it the way a witness would.
//!
//! This is the whole P4 pipeline with the network taken out — and taking the
//! network out is the point. `gates/p1-swarm` runs the real witness over a real
//! (impaired, in-process) link and answers "does the pipeline hold up under
//! loss"; this answers the question underneath it, "do these *rules* produce
//! false positives, and are these *cheats* actually adjudicable", in a
//! millisecond, on four platforms, on every commit.
//!
//! One run produces three artifacts:
//!
//! 1. **A chain digest** over every per-tick state hash, in execution order —
//!    the same construction `orrery_conformance` uses, and for the same reason:
//!    a window can diverge at tick 40 and reconverge by tick 180, and a
//!    final-state comparison would call that a pass.
//! 2. **A log** — per tick, per entity, the inputs actually applied and the
//!    state hash they produced. This is what an authority streams and a
//!    witness re-executes, minus the signatures.
//! 3. **Stage-1 flags** — the game's own invariants, run over samples taken at
//!    the replication rate with deliberate gaps, exactly as a peer that is not
//!    in the witness set would run them.
//!
//! # Sample loss is a feature of the harness
//!
//! [`Scenario::sample_loss_pct`] drops invariant samples deterministically, so
//! `elapsed_ticks` between two samples is routinely 3, 6, 9 or more ticks.
//! Every stage-1 check that forgot to divide by it fails here rather than in
//! the field, which is the single most likely source of a false positive that
//! correlates with a player's packet loss rather than with their conduct
//! (D17 risk 3).

use std::collections::{BTreeMap, BTreeSet};

use orrery_core::{
    evaluate, state_hash, CoreCodec, Executor, InvariantSample, InvariantViolation, Tolerance,
};
use orrery_protocol::{DeviationKind, PersistId, Tick, UniverseSeed};
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::game::Game;

/// The tick every scenario starts at.
///
/// Non-zero on purpose: the RNG is seeded from the *absolute* tick (VC-3), so
/// starting at zero would hide an off-by-one in tick derivation behind a
/// zero that happens to work.
pub const T0: u64 = 1_000_000;

/// Ticks between stage-1 samples: 20 Hz at a 60 Hz tick, the D16 replication
/// rate. A witness sees state this often at best, never every tick.
pub const SAMPLE_PERIOD: u64 = 3;

/// One reproducible run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scenario {
    /// Stable name. Appears in golden tables and in failure output.
    pub name: &'static str,
    /// How many player entities to spawn, via [`Game::spawn`].
    pub entities: u64,
    /// How many *world-owned* entities to seed, via [`Game::spawn_world`].
    ///
    /// Separate from [`Self::entities`] because it is a different question: a
    /// player population is what an honest pilot drives, and a world
    /// population is what the game's non-player domain owns — Regolith's
    /// rocks, pickups and bloom director. Every scenario in [`SCENARIOS`]
    /// declares zero, which is why they step no world entity at all; that is
    /// a real gap in the corpus and is why [`WORLD_SCENARIO`] exists.
    pub world_entities: u64,
    /// How many ticks to run. 180 is the 3 s adjudication window (D16).
    pub ticks: u64,
    /// The universe seed byte, expanded to the full 32-byte seed.
    pub seed_byte: u8,
    /// Percentage of stage-1 samples to drop, simulating replication loss.
    pub sample_loss_pct: u32,
}

/// The absolute, half-open tick range a sealed scenario covers.
///
/// This is recorded with the inputs rather than inferred from a later
/// scenario table: a differential run must know exactly which ticks its
/// legacy input log sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickWindow {
    /// First tick in the run.
    pub first: Tick,
    /// First tick outside the run.
    pub end_exclusive: Tick,
}

/// One entity's inputs as sealed before it stepped.
#[derive(Clone)]
pub struct SealedEntry<I> {
    /// Entity receiving the inputs.
    pub entity: PersistId,
    /// Inputs in the authority's log order.
    pub inputs: Vec<I>,
}

/// The sealed inputs for one absolute tick.
#[derive(Clone)]
pub struct SealedTick<I> {
    /// Absolute tick the entries apply to.
    pub tick: Tick,
    /// Entries in ascending [`PersistId`] order.
    pub entries: Vec<SealedEntry<I>>,
}

/// A reproducible scenario input artifact.
///
/// It is deliberately the inputs, not a copy of the scenario recipe.  A
/// later implementation receives this seed, this tick window, and these
/// already-ordered inputs; it cannot silently replace the baseline's pilot or
/// delivery history while claiming a differential comparison.
pub struct SealedScenario<G: Game> {
    /// Universe seed used by the executor.
    pub seed: UniverseSeed,
    /// Exact tick range covered by [`Self::input_log`].
    pub tick_window: TickWindow,
    /// Player population installed before the first tick.
    pub initial_entities: u64,
    /// World population installed before the first tick.
    pub initial_world_entities: u64,
    /// Inputs sealed before each step, in tick and entity order.
    pub input_log: Vec<SealedTick<G::CoreInput>>,
}

impl<G: Game> Clone for SealedScenario<G> {
    fn clone(&self) -> Self {
        Self {
            seed: self.seed,
            tick_window: self.tick_window,
            initial_entities: self.initial_entities,
            initial_world_entities: self.initial_world_entities,
            input_log: self.input_log.clone(),
        }
    }
}

/// One routed event input, retained in delivery order for the outcome fold.
///
/// A named type makes the target/input relationship available to a future
/// differential consumer without relying on a positional tuple.
#[derive(Clone)]
pub struct DeliveryPair<I> {
    /// Entity that receives the input on the following tick.
    pub target: PersistId,
    /// Input produced by the game's delivery rule.
    pub input: I,
}

/// One stepped entity's outcome-facing artifacts for an outcome-chain tick.
#[derive(Clone)]
pub struct OutcomeEntry<I> {
    /// The emitter, used to impose WP-2 order across entities.
    pub entity: PersistId,
    /// Canonical event bytes in the order the step emitted them.
    pub events: Vec<Vec<u8>>,
    /// Successfully installed identifiers in executor installation order.
    pub materialized: Vec<PersistId>,
    /// Routed target/input pairs in delivery order.
    pub delivered: Vec<DeliveryPair<I>>,
}

/// The scenarios every game in the catalogue is run through.
///
/// Breadth comes from the axes that break things — population size, window
/// length, how lossy the observer's view is — rather than from running one
/// case for longer. `solo` is the control: one entity, no combat possible, no
/// loss, so a failure there is a movement bug and nothing else.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "solo",
        entities: 1,
        world_entities: 0,
        ticks: 180,
        seed_byte: 0x11,
        sample_loss_pct: 0,
    },
    Scenario {
        name: "duel",
        entities: 2,
        world_entities: 0,
        ticks: 180,
        seed_byte: 0x22,
        sample_loss_pct: 0,
    },
    Scenario {
        name: "island",
        entities: 8,
        world_entities: 0,
        ticks: 600,
        seed_byte: 0x33,
        sample_loss_pct: 5,
    },
    Scenario {
        name: "island-lossy",
        entities: 8,
        world_entities: 0,
        ticks: 600,
        seed_byte: 0x44,
        sample_loss_pct: 40,
    },
];

/// A scenario that actually steps a game's **world** domain.
///
/// Deliberately outside [`SCENARIOS`]. Every scenario in that table declares
/// `world_entities: 0`, so across the whole battery corpus Regolith's
/// `regolith.world` module — the `rock`, `pickup` and `bloom-director`
/// sections #737 split out — is **stepped zero times**. That is measured, not
/// assumed: `tests/world_scenario.rs` asserts it of `SCENARIOS` and asserts
/// the opposite of this scenario, because a green suite over a module no
/// scenario reaches is a suite that measured nothing about that module.
///
/// It is not added to [`SCENARIOS`] because that table is the battery's
/// per-game corpus with committed F-1/F-2 goldens for every game in it, and
/// this scenario asks for a world population that only Regolith has. Its own
/// digests live in [`crate::golden::REGOLITH_WORLD`] and
/// [`crate::golden::REGOLITH_WORLD_OUTCOMES`] instead; folding it into the
/// battery's per-game tables is fixture work in a digest tree, not this
/// change's.
///
/// 900 ticks so the seeded director's early bloom (`next_bloom_tick` well
/// inside the window) seeds a batch, its rocks live and are shot, and the
/// pickups they drop both get grabbed and time out.
pub const WORLD_SCENARIO: Scenario = Scenario {
    name: "world",
    entities: 4,
    world_entities: 8,
    ticks: 900,
    seed_byte: 0x55,
    sample_loss_pct: 0,
};

/// A stage-1 check that failed, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    /// The tick of the sample that failed.
    pub tick: Tick,
    /// The entity it described.
    pub entity: PersistId,
    /// What failed.
    pub violation: InvariantViolation,
}

/// One entity's tick, as an authority would have logged it.
pub struct Entry<G: Game> {
    /// Whose tick.
    pub entity: PersistId,
    /// The inputs applied, in the order they were applied (VC-2).
    pub inputs: Vec<G::CoreInput>,
    /// The state hash the authority would have claimed.
    pub hash: [u8; 32],
    /// The state itself, so a replay can say *how* it differs and not only
    /// that it does.
    pub state: G::CoreState,
}

/// One tick of the log.
pub struct TickRecord<G: Game> {
    /// The absolute tick.
    pub tick: Tick,
    /// Entities in `PersistId` order — the order they were stepped in (VC-4).
    pub entries: Vec<Entry<G>>,
}

/// What one run produced.
pub struct Play<G: Game> {
    /// The chain over every per-tick state hash, in execution order.
    pub chain: [u8; 32],
    /// The chain over outcomes that state hashes cannot observe.
    pub outcome_chain: [u8; 32],
    /// The exact inputs this run sealed before execution.
    pub sealed: SealedScenario<G>,
    /// The log an authority would have streamed, one record per tick.
    pub log: Vec<TickRecord<G>>,
    /// The per-tick outcome records, in tick order: the same material the
    /// outcome chain folds, retained unbinned so a differential harness can
    /// compare the bytes and not only the digest (F-4 class D-2, A10 §4.1).
    pub outcome_entries: Vec<Vec<OutcomeEntry<G::CoreInput>>>,
    /// Stage-1 checks that failed. Empty is the expected result for honest
    /// rules, and the reason this harness exists.
    pub flags: Vec<Flag>,
    /// Cross-entity events emitted, all told. A combat scenario that produced
    /// none is not measuring combat, however green its assertions are.
    pub events: u64,
}

impl<G: Game> Play<G> {
    /// The flags raised, by validator name, in first-seen order.
    #[must_use]
    pub fn flagged_validators(&self) -> Vec<&'static str> {
        let mut seen: Vec<&'static str> = Vec::new();
        for flag in &self.flags {
            if !seen.contains(&flag.violation.validator) {
                seen.push(flag.violation.validator);
            }
        }
        seen
    }
}

/// The world-owned entities a scenario seeds, in install order.
///
/// Their stable ids continue past the player block — player `slot` takes
/// `PersistId(slot + 1)`, world `slot` takes `PersistId(players + slot + 1)`
/// — so adding a world population cannot renumber a player, and a game with
/// no world domain ([`Game::spawn_world`]'s default) seeds nothing however
/// many the scenario asks for.
pub(crate) fn world_population<G: Game>(
    game: &G,
    players: u64,
    world_entities: u64,
) -> Vec<(PersistId, G::CoreState)> {
    (0..world_entities)
        .filter_map(|slot| {
            let entity = PersistId::new(players + slot + 1);
            game.spawn_world(entity, slot).map(|state| (entity, state))
        })
        .collect()
}

/// Play `scenario` under `game`'s rules.
///
/// The inputs come from [`Game::honest_inputs`] and never from game state, so
/// an honest build and a tampered one receive identical input streams and
/// every difference between their logs is the rules' doing.
pub fn play<G: Game>(game: G, scenario: &Scenario) -> Play<G> {
    let seed = UniverseSeed([scenario.seed_byte; 32]);
    let entities: Vec<PersistId> = (1..=scenario.entities).map(PersistId::new).collect();
    let player_slots: BTreeMap<PersistId, u64> = entities
        .iter()
        .enumerate()
        .map(|(slot, entity)| (*entity, slot as u64))
        .collect();

    // Spawn before the ruleset moves into the executor.
    let spawned: Vec<(PersistId, G::CoreState)> = entities
        .iter()
        .enumerate()
        .map(|(slot, entity)| (*entity, game.spawn(*entity, slot as u64)))
        .collect();
    let seeded_world = world_population(&game, scenario.entities, scenario.world_entities);

    let mut executor = Executor::new(game, seed);
    for (entity, state) in spawned {
        executor.insert(entity, state);
    }
    for (entity, state) in seeded_world {
        executor.insert(entity, state);
    }

    let mut chain = [0u8; 32];
    let mut outcome_chain = [0u8; 32];
    let mut log = Vec::with_capacity(scenario.ticks as usize);
    let mut input_log = Vec::with_capacity(scenario.ticks as usize);
    let mut outcome_material: Vec<Vec<OutcomeEntry<G::CoreInput>>> =
        Vec::with_capacity(scenario.ticks as usize);
    let mut flags = Vec::new();
    let mut events = 0u64;
    let mut pending: BTreeMap<PersistId, Vec<G::CoreInput>> = BTreeMap::new();
    let mut last_sample: BTreeMap<PersistId, (G::CoreState, Tick)> = BTreeMap::new();

    for offset in 0..scenario.ticks {
        let tick = Tick::new(T0 + offset);
        // Snapshot the population at the tick boundary. Entities materialized
        // while this tick runs begin stepping on the next tick, never halfway
        // through their birth tick.
        let tick_entities: Vec<PersistId> = executor.entities().copied().collect();
        let mut delivered: BTreeMap<PersistId, Vec<G::CoreInput>> = BTreeMap::new();
        let mut entries = Vec::with_capacity(tick_entities.len());
        let mut sealed_entries = Vec::with_capacity(tick_entities.len());
        let mut outcome_entries = Vec::with_capacity(tick_entities.len());

        for entity in &tick_entities {
            // Events delivered from the previous tick come first, then what
            // an initial scenario player asked for. Materialized entities are
            // autonomous and receive only delivered inputs. The order is
            // arbitrary but fixed, which is all VC-2 requires.
            let mut inputs = pending.remove(entity).unwrap_or_default();
            if let Some(slot) = player_slots.get(entity) {
                let peers: Vec<PersistId> = tick_entities
                    .iter()
                    .copied()
                    .filter(|peer| peer != entity)
                    .collect();
                let mut rng = pilot_rng(seed, *entity, tick);
                executor.ruleset().honest_inputs(
                    *entity,
                    *slot,
                    tick,
                    &peers,
                    &mut rng,
                    &mut inputs,
                );
            }

            let outcome = executor
                .step_entity(*entity, tick, &inputs)
                .expect("every scenario entity is installed in the executor");
            chain = fold(chain, &outcome.state_hash);
            events += outcome.events.len() as u64;
            let mut outcome_events = Vec::with_capacity(outcome.events.len());
            let mut delivery_pairs = Vec::new();
            for event in &outcome.events {
                outcome_events.push(event.to_canonical());
                if let Some((target, input)) = executor.ruleset().deliver(event) {
                    delivered.entry(target).or_default().push(input.clone());
                    delivery_pairs.push(DeliveryPair { target, input });
                }
            }
            outcome_entries.push(OutcomeEntry {
                entity: *entity,
                events: outcome_events,
                materialized: outcome.materialized,
                delivered: delivery_pairs,
            });
            sealed_entries.push(SealedEntry {
                entity: *entity,
                inputs: inputs.clone(),
            });
            entries.push(Entry::<G> {
                entity: *entity,
                inputs,
                hash: outcome.state_hash,
                state: executor
                    .state(*entity)
                    .expect("the entity was just stepped")
                    .clone(),
            });
        }

        outcome_material.push(outcome_entries);
        outcome_chain = fold_outcome_tick(
            outcome_chain,
            outcome_material
                .last()
                .expect("one outcome record per tick"),
        );
        pending = delivered;
        log.push(TickRecord::<G> { tick, entries });
        input_log.push(SealedTick {
            tick,
            entries: sealed_entries,
        });

        if offset.is_multiple_of(SAMPLE_PERIOD) {
            sample_stage_one(
                &executor,
                &tick_entities,
                tick,
                scenario,
                &mut last_sample,
                &mut flags,
            );
        }
    }

    Play {
        chain,
        outcome_chain,
        sealed: SealedScenario {
            seed,
            tick_window: TickWindow {
                first: Tick::new(T0),
                end_exclusive: Tick::new(T0 + scenario.ticks),
            },
            initial_entities: scenario.entities,
            initial_world_entities: scenario.world_entities,
            input_log,
        },
        log,
        outcome_entries: outcome_material,
        flags,
        events,
    }
}

/// Replay a previously sealed run under `game`.
///
/// This is the input side of differential parity: callers capture
/// [`Play::sealed`] immediately before a refactor, then give the changed
/// implementation precisely the same initial population, seed, tick window,
/// and log-ordered inputs.  Sampling is intentionally absent because it is an
/// observer concern, not one of the sealed simulation inputs.
#[must_use]
pub fn replay<G: Game>(game: G, sealed: &SealedScenario<G>) -> Play<G> {
    let entities: Vec<PersistId> = (1..=sealed.initial_entities).map(PersistId::new).collect();
    let spawned: Vec<(PersistId, G::CoreState)> = entities
        .iter()
        .enumerate()
        .map(|(slot, entity)| (*entity, game.spawn(*entity, slot as u64)))
        .collect();
    let seeded_world = world_population(
        &game,
        sealed.initial_entities,
        sealed.initial_world_entities,
    );
    let mut executor = Executor::new(game, sealed.seed);
    for (entity, state) in spawned {
        executor.insert(entity, state);
    }
    for (entity, state) in seeded_world {
        executor.insert(entity, state);
    }

    let expected_ticks = sealed.tick_window.end_exclusive.0 - sealed.tick_window.first.0;
    assert_eq!(
        sealed.input_log.len(),
        usize::try_from(expected_ticks).expect("scenario tick window fits usize"),
        "sealed input log does not fill its tick window"
    );

    let mut chain = [0u8; 32];
    let mut outcome_chain = [0u8; 32];
    let mut events = 0u64;
    let mut log = Vec::with_capacity(sealed.input_log.len());
    let mut outcome_entries_kept: Vec<Vec<OutcomeEntry<G::CoreInput>>> =
        Vec::with_capacity(sealed.input_log.len());
    for (offset, record) in sealed.input_log.iter().enumerate() {
        let expected_tick = Tick::new(
            sealed.tick_window.first.0 + u64::try_from(offset).expect("input log length fits u64"),
        );
        assert_eq!(
            record.tick, expected_tick,
            "sealed input log has a tick gap"
        );
        let installed: Vec<PersistId> = executor.entities().copied().collect();
        let sealed_entities: Vec<PersistId> =
            record.entries.iter().map(|entry| entry.entity).collect();
        assert_eq!(
            sealed_entities, installed,
            "sealed input log does not match the replay population at {expected_tick:?}"
        );

        let mut entries = Vec::with_capacity(record.entries.len());
        let mut outcome_entries = Vec::with_capacity(record.entries.len());
        for sealed_entry in &record.entries {
            let outcome = executor
                .step_entity(sealed_entry.entity, record.tick, &sealed_entry.inputs)
                .expect("sealed input names an installed entity");
            chain = fold(chain, &outcome.state_hash);
            events += outcome.events.len() as u64;
            let mut outcome_events = Vec::with_capacity(outcome.events.len());
            let mut delivered = Vec::new();
            for event in &outcome.events {
                outcome_events.push(event.to_canonical());
                if let Some((target, input)) = executor.ruleset().deliver(event) {
                    delivered.push(DeliveryPair { target, input });
                }
            }
            outcome_entries.push(OutcomeEntry {
                entity: sealed_entry.entity,
                events: outcome_events,
                materialized: outcome.materialized,
                delivered,
            });
            entries.push(Entry {
                entity: sealed_entry.entity,
                inputs: sealed_entry.inputs.clone(),
                hash: outcome.state_hash,
                state: executor
                    .state(sealed_entry.entity)
                    .expect("the entity was just stepped")
                    .clone(),
            });
        }
        outcome_chain = fold_outcome_tick(outcome_chain, &outcome_entries);
        outcome_entries_kept.push(outcome_entries);
        log.push(TickRecord {
            tick: record.tick,
            entries,
        });
    }

    Play {
        chain,
        outcome_chain,
        sealed: sealed.clone(),
        log,
        outcome_entries: outcome_entries_kept,
        flags: Vec::new(),
        events,
    }
}

/// Run the game's stage-1 invariants over this tick's samples, dropping some.
fn sample_stage_one<G: Game>(
    executor: &Executor<G>,
    entities: &[PersistId],
    tick: Tick,
    scenario: &Scenario,
    last_sample: &mut BTreeMap<PersistId, (G::CoreState, Tick)>,
    flags: &mut Vec<Flag>,
) {
    for entity in entities {
        if dropped(scenario, *entity, tick) {
            continue;
        }
        let current = executor
            .state(*entity)
            .expect("the entity is installed")
            .clone();
        let previous = last_sample.get(entity);
        let sample = InvariantSample {
            entity: *entity,
            current: &current,
            tick,
            previous: previous.map(|(state, _)| state),
            elapsed_ticks: previous
                .map(|(_, at)| u32::try_from(tick.0.saturating_sub(at.0)).unwrap_or(u32::MAX))
                .unwrap_or(0),
        };
        if let Err(violation) = evaluate(executor.ruleset().invariants(), &sample) {
            flags.push(Flag {
                tick,
                entity: *entity,
                violation,
            });
        }
        last_sample.insert(*entity, (current, tick));
    }
}

/// What a re-execution found, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    /// The first tick whose state hash did not match.
    pub tick: Tick,
    /// The entity it belonged to.
    pub entity: PersistId,
    /// Which half of the contract broke.
    ///
    /// [`DeviationKind::ContinuousOutOfBand`] when the replayed trajectory is
    /// outside the D16 bands, [`DeviationKind::DiscreteMismatch`] when the
    /// trajectories agree and the state hash still does not — which is the
    /// signature of a cheat that never moved illegally.
    pub kind: DeviationKind,
}

/// Re-execute a recorded log under `game`'s rules and compare, as a witness
/// re-executing a window it did not produce.
///
/// Returns the first `(tick, entity)` whose hash disagrees. `None` means the
/// log is exactly what these rules would have produced — which for an honest
/// log is the self-verification property, and for a tampered one would mean
/// the cheat is invisible to replay and the pipeline cannot touch it.
///
/// One simplification against the real path: this re-derives t₀ by calling
/// [`Game::spawn`] rather than loading it from an evidence bundle, so it
/// assumes a cheat did not also fabricate its starting state. The real
/// adjudicator has no such assumption — `t0_snapshot` travels in the bundle
/// and is committed to by the t₀ claim's signature — and a game whose `spawn`
/// consulted [`Game::tampered`] would silently defeat this harness.
pub fn adjudicate<G: Game>(game: G, scenario: &Scenario, play: &Play<G>) -> Option<Divergence> {
    let seed = UniverseSeed([scenario.seed_byte; 32]);
    let entities: Vec<PersistId> = (1..=scenario.entities).map(PersistId::new).collect();
    let spawned: Vec<(PersistId, G::CoreState)> = entities
        .iter()
        .enumerate()
        .map(|(slot, entity)| (*entity, game.spawn(*entity, slot as u64)))
        .collect();

    let mut executor = Executor::new(game, seed);
    for (entity, state) in spawned {
        executor.insert(entity, state);
    }

    let tolerance = Tolerance::default();
    for record in &play.log {
        for entry in &record.entries {
            let outcome = executor
                .step_entity(entry.entity, record.tick, &entry.inputs)
                .expect("the log names an entity the scenario spawned");
            if outcome.state_hash == entry.hash {
                continue;
            }
            let computed = executor
                .state(entry.entity)
                .expect("the entity was just stepped");
            return Some(divergence::<G>(record.tick, entry, computed, &tolerance));
        }
    }
    None
}

/// Re-execute a recorded log the way the *shipped* adjudicator does: one
/// entity per executor, with no neighbours installed at all.
///
/// [`adjudicate`] installs the whole population into a single executor, which
/// is convenient and is not the world a witness builds.
/// [`ReplayHarness::load_claimed_snapshot`](orrery_core::ReplayHarness::load_claimed_snapshot)
/// loads the one state its claim commits to, so the step that follows sees an
/// empty neighbour map. A rule that reads a neighbour therefore behaves
/// differently under adjudication than it did under play, and a harness that
/// never built that world would score the difference as a pass — a false
/// conviction of an honest peer, discovered in the field.
///
/// `make` builds a fresh ruleset per entity, because an [`Executor`] owns the
/// one it drives.
pub fn adjudicate_isolated<G: Game>(
    make: impl Fn() -> G,
    scenario: &Scenario,
    play: &Play<G>,
) -> Option<Divergence> {
    let seed = UniverseSeed([scenario.seed_byte; 32]);
    let mut executors: BTreeMap<PersistId, Executor<G>> = BTreeMap::new();
    let mut known_entities = BTreeSet::new();
    for slot in 0..scenario.entities {
        let entity = PersistId::new(slot + 1);
        let game = make();
        let state = game.spawn(entity, slot);
        let mut executor = Executor::new(game, seed);
        executor.insert(entity, state);
        executors.insert(entity, executor);
        known_entities.insert(entity);
    }

    let tolerance = Tolerance::default();
    for record in &play.log {
        for entry in &record.entries {
            let (outcome, materialized) = {
                let executor = executors
                    .get_mut(&entry.entity)
                    .expect("the log names an initial or materialized entity");
                let outcome = executor
                    .step_entity(entry.entity, record.tick, &entry.inputs)
                    .expect("each executor holds the entity it was built for");
                let materialized = outcome
                    .materialized
                    .iter()
                    .map(|entity| {
                        (
                            *entity,
                            executor
                                .take_state(*entity)
                                .expect("the executor just materialized this entity"),
                        )
                    })
                    .collect::<Vec<_>>();
                (outcome, materialized)
            };
            if outcome.state_hash == entry.hash {
                // First-writer-wins is global even though each source replays
                // alone. The emitted description remains source-local; only
                // the first log-ordered source creates the child's executor.
                for (entity, state) in materialized {
                    if !known_entities.insert(entity) {
                        continue;
                    }
                    let mut executor = Executor::new(make(), seed);
                    executor.insert(entity, state);
                    executors.insert(entity, executor);
                }
                continue;
            }
            let computed = executors
                .get(&entry.entity)
                .and_then(|executor| executor.state(entry.entity))
                .expect("the entity was just stepped");
            return Some(divergence::<G>(record.tick, entry, computed, &tolerance));
        }
    }
    None
}

/// Classify a hash mismatch as continuous or discrete.
fn divergence<G: Game>(
    tick: Tick,
    entry: &Entry<G>,
    computed: &G::CoreState,
    tolerance: &Tolerance,
) -> Divergence {
    let (claimed_pos, claimed_vel) = G::trajectory(&entry.state);
    let (computed_pos, computed_vel) = G::trajectory(computed);
    let sample = orrery_core::TrajectorySample {
        tick,
        claimed_pos,
        claimed_vel,
        computed_pos,
        computed_vel,
    };
    Divergence {
        tick,
        entity: entry.entity,
        kind: if tolerance.exceeds(&sample) {
            DeviationKind::ContinuousOutOfBand
        } else {
            DeviationKind::DiscreteMismatch
        },
    }
}

/// Re-encode and decode every state in a log, and report the first failure.
///
/// The canonical encoding is what gets hashed, so a codec that is not a
/// bijection would make two honest builds disagree about a state they both
/// hold. Exercised over real play rather than over hand-built values, which is
/// where the field combinations nobody thought to write down come from.
///
/// # Errors
///
/// Returns the entity whose state failed to round-trip, and why.
pub fn check_codec<G: Game>(play: &Play<G>) -> Result<usize, (PersistId, &'static str)> {
    let mut checked = 0;
    for record in &play.log {
        for entry in &record.entries {
            let bytes = entry.state.to_canonical();
            let decoded =
                G::CoreState::decode(&bytes).map_err(|_| (entry.entity, "state did not decode"))?;
            if state_hash(&decoded) != entry.hash {
                return Err((entry.entity, "state did not round-trip"));
            }
            for input in &entry.inputs {
                let bytes = input.to_canonical();
                G::CoreInput::decode(&bytes).map_err(|_| (entry.entity, "input did not decode"))?;
            }
            checked += 1;
        }
    }
    Ok(checked)
}

/// Fold one state hash into the chain.
fn fold(chain: [u8; 32], hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&chain);
    hasher.update(hash);
    *hasher.finalize().as_bytes()
}

/// Fold one tick's non-state outcomes into an outcome chain.
///
/// The function imposes WP-2's ascending [`PersistId`] order across emitters;
/// it preserves the executor's event and materialization order and the
/// harness's delivery order within each emitter.  Lengths are `u64`
/// little-endian, which makes empty and variable-width canonical payloads
/// unambiguous without giving the payloads a second encoding.
#[must_use]
pub fn fold_outcome_tick<I: CoreCodec>(chain: [u8; 32], entries: &[OutcomeEntry<I>]) -> [u8; 32] {
    let mut ordered: Vec<&OutcomeEntry<I>> = entries.iter().collect();
    ordered.sort_by_key(|entry| entry.entity);

    let mut hasher = blake3::Hasher::new();
    hasher.update(&chain);
    for entry in ordered {
        hasher.update(&entry.entity.0.to_le_bytes());
        fold_count(&mut hasher, entry.events.len());
        for event in &entry.events {
            fold_bytes(&mut hasher, event);
        }
        fold_count(&mut hasher, entry.materialized.len());
        for entity in &entry.materialized {
            hasher.update(&entity.0.to_le_bytes());
        }
        fold_count(&mut hasher, entry.delivered.len());
        for delivery in &entry.delivered {
            hasher.update(&delivery.target.0.to_le_bytes());
            fold_bytes(&mut hasher, &delivery.input.to_canonical());
        }
    }
    *hasher.finalize().as_bytes()
}

fn fold_count(hasher: &mut blake3::Hasher, count: usize) {
    hasher.update(
        &u64::try_from(count)
            .expect("scenario item count fits u64")
            .to_le_bytes(),
    );
}

fn fold_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    fold_count(hasher, bytes.len());
    hasher.update(bytes);
}

/// The pilot's RNG.
///
/// Deliberately *not* [`orrery_core::tick_rng`]: that stream belongs to the
/// rules, and a pilot drawing from it would couple what a player asks for to
/// what their damage rolls come out as. The domain separator is what keeps the
/// two apart.
fn pilot_rng(seed: UniverseSeed, entity: PersistId, tick: Tick) -> ChaCha8Rng {
    let mut preimage = [0u8; 32];
    preimage[..16].copy_from_slice(b"orrery.pilot.v1\0");
    preimage[16..24].copy_from_slice(&entity.0.to_le_bytes());
    preimage[24..].copy_from_slice(&tick.0.to_le_bytes());
    ChaCha8Rng::from_seed(*blake3::keyed_hash(&seed.0, &preimage).as_bytes())
}

/// Whether this entity's sample at this tick is lost.
///
/// A hash rather than a counter, so loss is bursty per entity instead of
/// striped across the population — a striped pattern would keep
/// `elapsed_ticks` almost constant and quietly stop testing the thing the
/// loss is here to test.
fn dropped(scenario: &Scenario, entity: PersistId, tick: Tick) -> bool {
    if scenario.sample_loss_pct == 0 {
        return false;
    }
    let mut preimage = [0u8; 16];
    preimage[..8].copy_from_slice(&entity.0.to_le_bytes());
    preimage[8..].copy_from_slice(&tick.0.to_le_bytes());
    let digest = blake3::keyed_hash(&[scenario.seed_byte; 32], &preimage);
    u32::from(digest.as_bytes()[0]) * 100 < scenario.sample_loss_pct * 256
}
