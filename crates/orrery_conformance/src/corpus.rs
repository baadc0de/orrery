//! The fixed corpus, and the digest a platform reports.
//!
//! Every case is generated from a constant seed by a ChaCha8 stream, so the
//! corpus is reproducible from this source alone — there is no recorded data
//! file to drift out of sync with the rules that consume it. When playtest and
//! adjudication windows start feeding the corpus (docs/06 §8), they land here
//! as additional cases; the digest format does not change.

use std::collections::BTreeMap;

use orrery_core::executor::Executor;
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
    /// the recorded neighbour reads.
    pub combat: bool,
}

/// The corpus.
///
/// Deliberately small and fast — this runs on every commit, on every target.
/// Breadth comes from the cases differing in the axes that break determinism
/// (entity count, window length, whether cross-entity events flow), not from
/// running the same case for longer.
pub const CASES: &[Case] = &[
    Case {
        name: "kinematic-single",
        entities: 1,
        ticks: 180,
        seed_byte: 0x11,
        combat: false,
    },
    Case {
        name: "kinematic-swarm",
        entities: 16,
        ticks: 180,
        seed_byte: 0x22,
        combat: false,
    },
    Case {
        name: "combat-pair",
        entities: 2,
        ticks: 180,
        seed_byte: 0x33,
        combat: true,
    },
    Case {
        name: "combat-island",
        entities: 8,
        ticks: 600,
        seed_byte: 0x44,
        combat: true,
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
        // Attack a neighbour that is never self, so a read is always recorded.
        let offset = 1 + u64::from(rng.next_u32()) % (case.entities - 1);
        let target = ((entity - 1 + offset) % case.entities) + 1;
        out.push(Command::Attack {
            target: PersistId::new(target),
            power: rng.next_u32() % 40,
        });
    }
    out
}

/// Run one case and digest it.
///
/// `detail` controls whether the per-tick hashes are retained. The chain value
/// is identical either way — retaining detail must never change the result.
pub fn run_case(case: &Case, detail: bool) -> CaseDigest {
    let mut executor = Executor::new(Reference, UniverseSeed([case.seed_byte; 32]));
    for (id, body) in seed_world(case) {
        executor.insert(id, body);
    }

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
        let ids: Vec<PersistId> = executor.entities().copied().collect();
        for id in ids {
            let mut inputs = commands_for(case, id.0, tick.0);
            // Inbound damage is appended after the entity's own commands, so
            // the total order is a property of the schedule rather than of map
            // iteration.
            if let Some(inbound) = pending.remove(&id) {
                inputs.extend(inbound);
            }

            let outcome = executor
                .step_entity(id, tick, &inputs)
                .expect("entity is present for the whole case");

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

    let final_states = executor
        .entities()
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .map(|id| {
            let b = executor.state(id).expect("entity present");
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
