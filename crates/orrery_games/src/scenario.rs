//! The harness: play a game, record what an authority would have logged, and
//! re-execute it the way a witness would.
//!
//! This is the whole P4 pipeline with the network taken out — and taking the
//! network out is the point. `p1-swarm` runs the real witness over a real
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

use std::collections::BTreeMap;

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
    /// How many entities to spawn.
    pub entities: u64,
    /// How many ticks to run. 180 is the 3 s adjudication window (D16).
    pub ticks: u64,
    /// The universe seed byte, expanded to the full 32-byte seed.
    pub seed_byte: u8,
    /// Percentage of stage-1 samples to drop, simulating replication loss.
    pub sample_loss_pct: u32,
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
        ticks: 180,
        seed_byte: 0x11,
        sample_loss_pct: 0,
    },
    Scenario {
        name: "duel",
        entities: 2,
        ticks: 180,
        seed_byte: 0x22,
        sample_loss_pct: 0,
    },
    Scenario {
        name: "island",
        entities: 8,
        ticks: 600,
        seed_byte: 0x33,
        sample_loss_pct: 5,
    },
    Scenario {
        name: "island-lossy",
        entities: 8,
        ticks: 600,
        seed_byte: 0x44,
        sample_loss_pct: 40,
    },
];

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
    /// The log an authority would have streamed, one record per tick.
    pub log: Vec<TickRecord<G>>,
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

/// Play `scenario` under `game`'s rules.
///
/// The inputs come from [`Game::honest_inputs`] and never from game state, so
/// an honest build and a tampered one receive identical input streams and
/// every difference between their logs is the rules' doing.
pub fn play<G: Game>(game: G, scenario: &Scenario) -> Play<G> {
    let seed = UniverseSeed([scenario.seed_byte; 32]);
    let entities: Vec<PersistId> = (1..=scenario.entities).map(PersistId::new).collect();
    let peers: BTreeMap<PersistId, Vec<PersistId>> = entities
        .iter()
        .map(|entity| {
            (
                *entity,
                entities.iter().copied().filter(|e| e != entity).collect(),
            )
        })
        .collect();

    // Spawn before the ruleset moves into the executor.
    let spawned: Vec<(PersistId, G::CoreState)> = entities
        .iter()
        .enumerate()
        .map(|(slot, entity)| (*entity, game.spawn(*entity, slot as u64)))
        .collect();

    let mut executor = Executor::new(game, seed);
    for (entity, state) in spawned {
        executor.insert(entity, state);
    }

    let mut chain = [0u8; 32];
    let mut log = Vec::with_capacity(scenario.ticks as usize);
    let mut flags = Vec::new();
    let mut events = 0u64;
    let mut pending: BTreeMap<PersistId, Vec<G::CoreInput>> = BTreeMap::new();
    let mut last_sample: BTreeMap<PersistId, (G::CoreState, Tick)> = BTreeMap::new();

    for offset in 0..scenario.ticks {
        let tick = Tick::new(T0 + offset);
        let mut delivered: BTreeMap<PersistId, Vec<G::CoreInput>> = BTreeMap::new();
        let mut entries = Vec::with_capacity(entities.len());

        for (slot, entity) in entities.iter().enumerate() {
            // Events delivered from the previous tick come first, then what
            // the player asked for. The order is arbitrary but it is fixed,
            // which is all VC-2 requires of the authority that sets it.
            let mut inputs = pending.remove(entity).unwrap_or_default();
            let mut rng = pilot_rng(seed, *entity, tick);
            executor.ruleset().honest_inputs(
                *entity,
                slot as u64,
                tick,
                &peers[entity],
                &mut rng,
                &mut inputs,
            );

            let outcome = executor
                .step_entity(*entity, tick, &inputs)
                .expect("every scenario entity is installed in the executor");
            chain = fold(chain, &outcome.state_hash);
            events += outcome.events.len() as u64;
            for event in &outcome.events {
                if let Some((target, input)) = executor.ruleset().deliver(event) {
                    delivered.entry(target).or_default().push(input);
                }
            }
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

        pending = delivered;
        log.push(TickRecord::<G> { tick, entries });

        if offset.is_multiple_of(SAMPLE_PERIOD) {
            sample_stage_one(
                &executor,
                &entities,
                tick,
                scenario,
                &mut last_sample,
                &mut flags,
            );
        }
    }

    Play {
        chain,
        log,
        flags,
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
    for slot in 0..scenario.entities {
        let entity = PersistId::new(slot + 1);
        let game = make();
        let state = game.spawn(entity, slot);
        let mut executor = Executor::new(game, seed);
        executor.insert(entity, state);
        executors.insert(entity, executor);
    }

    let tolerance = Tolerance::default();
    for record in &play.log {
        for entry in &record.entries {
            let executor = executors
                .get_mut(&entry.entity)
                .expect("the log names an entity the scenario spawned");
            let outcome = executor
                .step_entity(entry.entity, record.tick, &entry.inputs)
                .expect("each executor holds the entity it was built for");
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
