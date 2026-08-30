//! The fixed corpus, and the digest a platform reports.
//!
//! Every case is generated from a constant seed by a ChaCha8 stream, so the
//! corpus is reproducible from this source alone — there is no recorded data
//! file to drift out of sync with the rules that consume it. When playtest and
//! adjudication windows start feeding the corpus (docs/06 §8), they land here
//! as additional cases; the digest format does not change.

use std::collections::BTreeMap;

use orrery_core::executor::{Executor, TickOutcome};
use orrery_core::quantize::{QPos, QVel};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::ruleset::{Body, Command, Outcome, Reference, REFERENCE_RULESET};

/// The tick a corpus window starts at. Non-zero on purpose: the RNG is seeded
/// from the *absolute* tick (VC-3), so starting at zero would hide an
/// off-by-one in tick derivation.
const T0: u64 = 1_000_000;

/// One case in the corpus.
pub struct Case {
    /// Stable name. Appears in the digest and in divergence reports.
    pub name: &'static str,
    /// How many entities the case simulates.
    pub entities: u64,
    /// How many ticks to run. 180 ticks is the 3 s adjudication window (D16).
    pub ticks: u64,
    /// The universe seed byte, expanded to the full 32-byte seed.
    pub seed_byte: u8,
    /// Whether entities attack each other, exercising the discrete path and
    /// the cross-entity event flow.
    pub combat: bool,
    /// The order in which the starting population is inserted into the
    /// executor. Canonical execution and projection must remain independent
    /// of this order (D48 WP-2).
    pub spawn_order: SpawnOrder,
    /// Execute each entity in its own single-entity [`Executor`], the shape
    /// `orrery_games::scenario::adjudicate_isolated` gives an adjudicator.
    ///
    /// The world, the input schedule and the event flow are identical either
    /// way; the only difference is that an isolated executor holds no
    /// neighbours, so `StateView::neighbor` always answers `None` — exactly as
    /// it does at replay, where `ReplayHarness::load_claimed_snapshot` installs
    /// one entity. A rule that read a neighbour's live state would therefore
    /// produce a *different chain* here than in the shared run, which is the
    /// divergence this axis exists to surface.
    ///
    pub isolated: bool,
}

/// How a case inserts its starting population into the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOrder {
    /// Insert entities in ascending [`PersistId`] order.
    Ascending,
    /// Insert the 16-entity projection fixture in its committed fixed
    /// permutation.
    ProjectionPermutation,
}

/// The committed insertion order for `projection-order-permuted`.
///
/// It is the same population as `kinematic-swarm`, but it is visibly not
/// sorted (`9` precedes `1`). Keeping the values explicit makes the fixture's
/// permutation reviewable rather than relying on a generator that could drift.
const PROJECTION_ORDER_PERMUTATION: [u64; 16] =
    [9, 1, 14, 3, 16, 6, 11, 2, 15, 8, 4, 13, 5, 12, 7, 10];

/// The corpus.
///
/// Deliberately small and fast — this runs on every commit, on every target.
/// Breadth comes from the cases differing in the axes that break determinism
/// (entity count, window length, whether cross-entity events flow, whether each
/// entity runs in its own executor), not from running the same case for longer.
pub const CASES: &[Case] = &[
    Case {
        name: "kinematic-single",
        entities: 1,
        ticks: 180,
        seed_byte: 0x11,
        combat: false,
        spawn_order: SpawnOrder::Ascending,
        isolated: false,
    },
    Case {
        name: "kinematic-swarm",
        entities: 16,
        ticks: 180,
        seed_byte: 0x22,
        combat: false,
        spawn_order: SpawnOrder::Ascending,
        isolated: false,
    },
    // The exact twin of `kinematic-swarm`, except that its starting population
    // is inserted in the committed fixed permutation above. D48 WP-2 requires
    // both projections to fold in ascending `PersistId` order regardless.
    Case {
        name: "projection-order-permuted",
        entities: 16,
        ticks: 180,
        seed_byte: 0x22,
        combat: false,
        spawn_order: SpawnOrder::ProjectionPermutation,
        isolated: false,
    },
    // A10 B-1's scale point: large enough to measure per-entity mirror and
    // fold cost while retaining the ordinary three-second corpus window.
    Case {
        name: "swarm-large",
        entities: 256,
        ticks: 180,
        seed_byte: 0x55,
        combat: false,
        spawn_order: SpawnOrder::Ascending,
        isolated: false,
    },
    Case {
        name: "combat-pair",
        entities: 2,
        ticks: 180,
        seed_byte: 0x33,
        combat: true,
        spawn_order: SpawnOrder::Ascending,
        isolated: false,
    },
    Case {
        name: "combat-island",
        entities: 8,
        ticks: 600,
        seed_byte: 0x44,
        combat: true,
        spawn_order: SpawnOrder::Ascending,
        isolated: false,
    },
    // Same axes as `combat-island`, executed one entity per executor. Its
    // chain is asserted equal to the shared run's in the crate's own tests:
    // that equality is the property, and this entry is what carries it across
    // the matrix and into the committed golden.
    Case {
        name: "combat-isolated",
        entities: 8,
        ticks: 600,
        seed_byte: 0x44,
        combat: true,
        spawn_order: SpawnOrder::Ascending,
        isolated: true,
    },
];

/// One entity's state at the end of a case, in canonical integer form.
///
/// Reported alongside the hashes because a bare hash mismatch says only "these
/// differ". These fields say *by how much*, which is what separates a libm
/// drift of one quantum from a genuine rules divergence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalState {
    /// The entity.
    pub entity: u64,
    /// Position in millimetres.
    pub pos_mm: [i64; 3],
    /// Velocity in millimetres per second.
    pub vel_mms: [i64; 3],
    /// Heading in micro-radians.
    pub heading_urad: i64,
    /// Hit points.
    pub hp: i32,
    /// Shield points.
    pub shield: i32,
    /// The folded RNG stream.
    pub roll_fold: u64,
}

/// One `(tick, entity)` state hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickHash {
    /// Absolute tick.
    pub tick: u64,
    /// The entity stepped.
    pub entity: u64,
    /// blake3 over the canonical encoding of the quantized state, hex.
    pub hash: String,
}

/// What one case produced on this platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseDigest {
    /// The case name.
    pub name: String,
    /// Entities simulated.
    pub entities: u64,
    /// Ticks run.
    pub ticks: u64,
    /// A blake3 chain over every per-tick state hash in execution order. This
    /// single value is what the cross-platform comparison and the committed
    /// golden compare on; the detail below only exists to localize a mismatch.
    pub chain: String,
    /// Final state per entity, in `PersistId` order.
    pub final_states: Vec<FinalState>,
    /// Every per-tick hash, in execution order. Present in emitted artifacts,
    /// omitted from the committed golden — it is diagnostic weight, not the
    /// thing under test.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tick_hashes: Vec<TickHash>,
}

/// A whole platform's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Format version, so a golden from an older layout fails loudly.
    pub schema: u32,
    /// The ruleset version this was produced under.
    pub ruleset_version: u32,
    /// The ruleset build digest, hex.
    pub ruleset_digest: String,
    /// Target triple, as reported by the build. Labelling only — it must never
    /// affect a hash.
    pub target: String,
    /// Per-case results, in corpus order.
    pub cases: Vec<CaseDigest>,
}

/// The report format version.
pub const SCHEMA: u32 = 1;

/// Build a case's starting world.
///
/// Deterministic in the case's seed alone: same seed, same world, on every
/// platform and every run.
fn seed_world(case: &Case) -> BTreeMap<PersistId, Body> {
    let mut rng = ChaCha8Rng::from_seed([case.seed_byte; 32]);
    let mut world = BTreeMap::new();
    for i in 0..case.entities {
        // Integer-derived starting values: the *starting* state must not itself
        // depend on float behaviour, or a drift in setup would be misread as a
        // drift in the rules.
        let pos = QPos {
            x: (rng.next_u32() % 100_000) as i64 - 50_000,
            y: (rng.next_u32() % 100_000) as i64 - 50_000,
            z: 0,
        };
        let vel = QVel {
            x: (rng.next_u32() % 4_000) as i64 - 2_000,
            y: (rng.next_u32() % 4_000) as i64 - 2_000,
            z: 0,
        };
        world.insert(
            PersistId::new(i + 1),
            Body {
                pos,
                vel,
                heading_urad: (rng.next_u32() % 6_283_185) as i64,
                hp: 1_000,
                shield: 250,
                roll_fold: 0,
            },
        );
    }
    world
}

/// The case's insertion order, separate from the canonical projection order.
fn spawn_order(case: &Case) -> Vec<PersistId> {
    match case.spawn_order {
        SpawnOrder::Ascending => (1..=case.entities).map(PersistId::new).collect(),
        SpawnOrder::ProjectionPermutation => {
            assert_eq!(
                case.entities,
                PROJECTION_ORDER_PERMUTATION.len() as u64,
                "the projection permutation is committed for exactly 16 entities"
            );
            PROJECTION_ORDER_PERMUTATION
                .into_iter()
                .map(PersistId::new)
                .collect()
        }
    }
}

/// The commands one entity issues on one tick.
///
/// Driven by its own ChaCha8 stream keyed on `(seed, entity, tick)`, so the
/// input schedule is reproducible without recording it, and identical inputs
/// reach every platform.
fn commands_for(case: &Case, entity: u64, tick: u64) -> Vec<Command> {
    let mut key = [0u8; 32];
    key[0] = case.seed_byte;
    key[1..9].copy_from_slice(&entity.to_le_bytes());
    key[9..17].copy_from_slice(&tick.to_le_bytes());
    let mut rng = ChaCha8Rng::from_seed(key);

    let mut out = Vec::new();
    // Two thrusts on some ticks, one on others: input *count* varies, so the
    // order-sensitivity of the rule is exercised rather than assumed.
    let thrusts = 1 + (rng.next_u32() % 2);
    for _ in 0..thrusts {
        out.push(Command::Thrust {
            accel_mmss: (rng.next_u32() % 8_000) as i64 - 4_000,
            turn_urad: (rng.next_u32() % 200_000) as i64 - 100_000,
        });
    }
    if case.combat && case.entities > 1 && rng.next_u32() % 3 == 0 {
        // Attack another entity, never self, so the cross-entity event path is
        // always the one exercised.
        let offset = 1 + u64::from(rng.next_u32()) % (case.entities - 1);
        let target = ((entity - 1 + offset) % case.entities) + 1;
        out.push(Command::Attack {
            target: PersistId::new(target),
            power: rng.next_u32() % 40,
        });
    }
    out
}

/// How a case's entities are held: all in one executor, or one executor each.
///
/// Both arms drive the same `Reference` ruleset over the same seed, the same
/// world and the same input schedule. What differs is what a step can *see*:
/// a shared executor exposes every other entity through `StateView::neighbor`,
/// an isolated one exposes nothing — which is what an adjudicator has, since
/// `ReplayHarness::load_claimed_snapshot` installs exactly one entity. Running
/// both and requiring the same chain is how a neighbour read stops being
/// invisible.
enum StageStorage {
    /// One executor over the whole world.
    Shared(Executor<Reference>),
    /// One single-entity executor per entity, in `PersistId` order.
    Isolated(BTreeMap<PersistId, Executor<Reference>>),
}

/// A case's executor storage and the order used to populate it.
struct Stage {
    storage: StageStorage,
    spawn_order: Vec<PersistId>,
}

impl Stage {
    /// Build the case's world into the requested arrangement.
    fn new(case: &Case) -> Self {
        let seed = UniverseSeed([case.seed_byte; 32]);
        let mut world = seed_world(case);
        let spawn_order = spawn_order(case);
        let storage = if case.isolated {
            let mut executors = BTreeMap::new();
            for id in &spawn_order {
                let body = world
                    .remove(id)
                    .expect("spawn order names every entity exactly once");
                let mut executor = Executor::new(Reference, seed);
                executor.insert(*id, body);
                executors.insert(*id, executor);
            }
            StageStorage::Isolated(executors)
        } else {
            let mut executor = Executor::new(Reference, seed);
            for id in &spawn_order {
                let body = world
                    .remove(id)
                    .expect("spawn order names every entity exactly once");
                executor.insert(*id, body);
            }
            StageStorage::Shared(executor)
        };
        assert!(
            world.is_empty(),
            "spawn order must name every entity exactly once"
        );
        Self {
            storage,
            spawn_order,
        }
    }

    /// Every entity, in `PersistId` order (VC-4: a BTreeMap walk either way).
    fn entities(&self) -> Vec<PersistId> {
        let entities: Vec<PersistId> = match &self.storage {
            StageStorage::Shared(executor) => executor.entities().copied().collect(),
            StageStorage::Isolated(executors) => executors.keys().copied().collect(),
        };
        debug_assert_eq!(entities.len(), self.spawn_order.len());
        entities
    }

    /// Advance one entity by one tick.
    fn step(&mut self, id: PersistId, tick: Tick, inputs: &[Command]) -> TickOutcome<Outcome> {
        match &mut self.storage {
            StageStorage::Shared(executor) => executor.step_entity(id, tick, inputs),
            StageStorage::Isolated(executors) => executors
                .get_mut(&id)
                .and_then(|executor| executor.step_entity(id, tick, inputs)),
        }
        .expect("entity is present for the whole case")
    }

    /// An entity's current state.
    fn state(&self, id: PersistId) -> &Body {
        match &self.storage {
            StageStorage::Shared(executor) => executor.state(id),
            StageStorage::Isolated(executors) => {
                executors.get(&id).and_then(|executor| executor.state(id))
            }
        }
        .expect("entity present")
    }
}

/// Run one case and digest it.
///
/// `detail` controls whether the per-tick hashes are retained. The chain value
/// is identical either way — retaining detail must never change the result.
pub fn run_case(case: &Case, detail: bool) -> CaseDigest {
    let mut stage = Stage::new(case);

    // Events emitted on tick N become inputs on tick N+1, which is what keeps
    // each entity's replay self-contained.
    let mut pending: BTreeMap<PersistId, Vec<Command>> = BTreeMap::new();
    let mut chain = [0u8; 32];
    let mut tick_hashes = Vec::new();

    for step in 0..case.ticks {
        let tick = Tick::new(T0 + step);
        let mut next_pending: BTreeMap<PersistId, Vec<Command>> = BTreeMap::new();

        // Entities step in `PersistId` order — a BTreeMap walk, never a hash
        // iteration (VC-4). Collected first because stepping borrows the
        // executor mutably.
        for id in stage.entities() {
            let mut inputs = commands_for(case, id.0, tick.0);
            // Inbound damage is appended after the entity's own commands, so
            // the total order is a property of the schedule rather than of map
            // iteration.
            if let Some(inbound) = pending.remove(&id) {
                inputs.extend(inbound);
            }

            let outcome = stage.step(id, tick, &inputs);

            chain = *blake3::Hasher::new()
                .update(&chain)
                .update(&id.0.to_le_bytes())
                .update(&tick.0.to_le_bytes())
                .update(&outcome.state_hash)
                .finalize()
                .as_bytes();

            if detail {
                tick_hashes.push(TickHash {
                    tick: tick.0,
                    entity: id.0,
                    hash: hex(&outcome.state_hash),
                });
            }

            for event in outcome.events {
                let Outcome::DamageApplied { target, amount } = event;
                next_pending
                    .entry(target)
                    .or_default()
                    .push(Command::Damage { amount });
            }
        }
        pending = next_pending;
    }

    let final_states = stage
        .entities()
        .into_iter()
        .map(|id| {
            let b = stage.state(id);
            FinalState {
                entity: id.0,
                pos_mm: [b.pos.x, b.pos.y, b.pos.z],
                vel_mms: [b.vel.x, b.vel.y, b.vel.z],
                heading_urad: b.heading_urad,
                hp: b.hp,
                shield: b.shield,
                roll_fold: b.roll_fold,
            }
        })
        .collect();

    CaseDigest {
        name: case.name.to_string(),
        entities: case.entities,
        ticks: case.ticks,
        chain: hex(&chain),
        final_states,
        tick_hashes,
    }
}

/// Run the whole corpus.
pub fn run_all(detail: bool) -> Report {
    Report {
        schema: SCHEMA,
        ruleset_version: REFERENCE_RULESET.version,
        ruleset_digest: hex(&REFERENCE_RULESET.digest),
        target: target_triple(),
        cases: CASES.iter().map(|c| run_case(c, detail)).collect(),
    }
}

/// The target this binary was built for. Label only — it must never reach a
/// hash, or every platform would trivially "agree" with itself.
///
/// Built from `std::env::consts` rather than a build script (`CARGO_CFG_*`
/// reaches build scripts, not `env!`), so the crate stays dependency-free.
pub fn target_triple() -> String {
    format!(
        "{}-{}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        std::env::consts::FAMILY,
    )
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str) -> &'static Case {
        CASES
            .iter()
            .find(|case| case.name == name)
            .unwrap_or_else(|| panic!("corpus case {name} is present"))
    }

    #[test]
    fn projection_order_permutation_is_complete_and_not_already_sorted() {
        let permuted = case("projection-order-permuted");
        let actual = spawn_order(permuted);
        let mut sorted = actual.clone();
        sorted.sort_unstable();
        sorted.dedup();

        let expected: Vec<_> = (1..=permuted.entities).map(PersistId::new).collect();
        assert_eq!(
            sorted, expected,
            "the permutation must cover the same population"
        );
        assert_ne!(
            actual, expected,
            "the permuted fixture must not already be PersistId-sorted"
        );
    }

    #[test]
    fn projection_order_permuted_matches_its_forward_twin() {
        let forward = run_case(case("kinematic-swarm"), false);
        let permuted = run_case(case("projection-order-permuted"), false);

        assert_eq!(
            permuted.chain, forward.chain,
            "projection-order-permuted diverged from its forward twin kinematic-swarm"
        );
        assert_eq!(permuted.final_states, forward.final_states);
    }
}
