//! The benchmark suite: A10 §8.2's B-1..B-7, over the instruments that exist.
//!
//! Every leg drives the conformance population itself — the corpus cases
//! ([`orrery_conformance::run_case`]) and the scenario battery
//! ([`orrery_games::scenario::play`]) — so a measured number is about a
//! workload the fixtures cover. What an instrument does not exist for is
//! recorded **absent** with its reason ([`absent_legs`]), never silently
//! skipped and never invented.
//!
//! Repetition counts are fixed constants, not time budgets, so a capture's
//! length is stable and statable. All timings are wall-clock
//! ([`std::time::Instant`]) around whole published calls; nothing inside the
//! measured crates is re-implemented here.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use orrery_conformance::CASES;
use orrery_conformance::corpus::{Case, run_case};
use orrery_core::{CoreCodec, Quantized, state_hash};
use orrery_games::game::{Game, GameVisitor, for_each_game};
use orrery_games::scenario::{SCENARIOS, play};

use crate::capacity::CapacityMirrorBench;

/// Whole-case runs per corpus leg before the timed repetitions.
const CORPUS_WARMUP: u32 = 3;
/// Timed whole-case runs per corpus leg. Odd, so p50 is a real sample.
const CORPUS_REPS: u32 = 31;
/// Whole-scenario plays per battery leg before the timed repetitions.
const BATTERY_WARMUP: u32 = 2;
/// Timed whole-scenario plays per battery leg. Odd, so p50 is a real sample.
const BATTERY_REPS: u32 = 11;
/// Corpus runs whose RSS delta is folded into the high-water mark (B-3).
const MEMORY_REPS: u32 = 9;

/// Everything the suite measured in one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuiteMetrics {
    /// B-1 over the committed corpus cases.
    pub b1_corpus: Vec<CaseBench>,
    /// B-1 over the committed scenario battery, per game per scenario.
    pub b1_battery: Vec<BatteryBench>,
    /// B-3, high-water RSS per entity. `None` off Linux — the field is
    /// recorded absent with that reason rather than faked.
    pub b3_memory: Option<MemoryBench>,
    /// B-4, canonical snapshot-encode cost over the battery's logged states.
    pub b4_snapshot: Vec<SnapshotBench>,
    /// B-5, claim assembly (quantize + encode + blake3) per logged state.
    pub b5_claim: Vec<ClaimBench>,
    /// B-5, capacity-scale canonical tick and presentation-byte extraction
    /// through the public simulation-host seam, on both admitted substrates.
    /// Default keeps pre-extraction schema-1 baselines readable: absence there
    /// is historical fact, not a zero measurement.
    #[serde(default)]
    pub b5_capacity_mirror: Vec<CapacityMirrorBench>,
}

/// B-1 for one corpus case: the tick cost of the conformance population.
///
/// A sample is one whole [`run_case`] call, the exact call the determinism
/// matrix runs. `tick_us` divides by the case's tick count (the executor's
/// 16.6 ms frame budget is per full-tick across the population);
/// `entity_tick_us` divides by entity-ticks. Both distributions are over
/// repetitions of the case, so they carry run-to-run stability; within-window
/// tick variance is not observable through the committed instruments and is
/// not claimed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseBench {
    /// The corpus case name.
    pub name: String,
    /// Entities simulated.
    pub entities: u64,
    /// Ticks per run.
    pub ticks: u64,
    /// Timed repetitions behind the percentiles.
    pub reps: u32,
    /// Full-population tick cost, µs.
    pub tick_us: Percentiles,
    /// Per entity-step cost, µs.
    pub entity_tick_us: Percentiles,
}

/// B-1 for one game over one scenario of the committed battery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryBench {
    /// The game's catalogue name.
    pub game: String,
    /// The scenario name.
    pub scenario: String,
    /// Entities spawned.
    pub entities: u64,
    /// Ticks per play.
    pub ticks: u64,
    /// Timed repetitions behind the percentiles.
    pub reps: u32,
    /// Cross-entity events emitted per play — workload shape, not cost: a
    /// candidate whose event count moved is not measuring the same work.
    pub events: u64,
    /// Full-population tick cost, µs, including log retention.
    pub tick_us: Percentiles,
    /// Per entity-step cost, µs, including log retention.
    pub entity_tick_us: Percentiles,
}

/// B-3 for one corpus case: high-water resident-memory delta across a run.
///
/// glibc reuses arenas, so most runs return a delta near zero; the high-water
/// mark over repetitions is the stable floor figure for "what holding and
/// simulating this population costs on top of an already-warm process".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryBench {
    /// The corpus case measured.
    pub case: String,
    /// Its entity count.
    pub entities: u64,
    /// Repetitions folded into the high-water mark.
    pub reps: u32,
    /// Largest RSS delta observed across a run, KiB.
    pub rss_delta_kib_high_water: i64,
    /// That delta per entity, bytes.
    pub bytes_per_entity: f64,
}

/// B-4 for one game over one scenario: canonical snapshot-encode cost over
/// the run's logged states — `CoreCodec::to_canonical` per entry, the encode
/// a checkpoint or claim commits to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotBench {
    /// The game's catalogue name.
    pub game: String,
    /// The scenario whose log was walked.
    pub scenario: String,
    /// States encoded.
    pub snapshots: u64,
    /// Encode cost, µs.
    pub encode_us: Percentiles,
    /// Canonical size, bytes (p50 across the log).
    pub bytes_p50: f64,
}

/// B-5 for one game over one scenario: claim assembly per entity-tick —
/// clone, quantize, canonical encode, blake3, the exact path a `StateClaim`
/// commits to. The capacity-scale extraction half is measured separately by
/// [`CapacityMirrorBench`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimBench {
    /// The game's catalogue name.
    pub game: String,
    /// The scenario whose log was walked.
    pub scenario: String,
    /// Claims assembled.
    pub claims: u64,
    /// Assembly cost, µs.
    pub claim_us: Percentiles,
}

/// A p50/p99 pair over one set of samples.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Percentiles {
    /// 50th percentile, nearest-rank.
    pub p50: f64,
    /// 99th percentile, nearest-rank.
    pub p99: f64,
}

/// One leg the suite could not drive, and why. Recorded in the baseline
/// document, so "absent" is a stated fact rather than a gap in a schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbsentLeg {
    /// The leg, named as A10 §8.2 names it.
    pub leg: String,
    /// Why this capture does not carry it.
    pub reason: String,
}

/// Run the whole suite. Wall-clock only; the caller times the call.
///
/// The battery visitor runs once and feeds B-1's battery leg and the B-4 and
/// B-5 legs from the same timed plays — one pass over the population, three
/// measurements out of it.
pub fn run() -> SuiteMetrics {
    let corpus = corpus_leg();
    let mut runner = BatteryRunner::default();
    for_each_game(&mut runner);
    let b1_battery = std::mem::take(&mut runner.battery);
    let b4_snapshot = std::mem::take(&mut runner.snapshot);
    let b5_claim = std::mem::take(&mut runner.claim);
    let b3_memory = memory_leg();
    let b5_capacity_mirror = crate::capacity::run();
    SuiteMetrics {
        b1_corpus: corpus,
        b1_battery,
        b3_memory,
        b4_snapshot,
        b5_claim,
        b5_capacity_mirror,
    }
}

/// The legs this build cannot drive, with the reason each is absent. Called
/// at capture time and recorded into the baseline document. B-3's entry
/// depends on the run: a platform where [`memory_leg`] returned `None` names
/// it absent rather than shipping a `null`.
pub fn absent_legs(metrics: &SuiteMetrics) -> Vec<AbsentLeg> {
    let mut legs = vec![
        AbsentLeg {
            leg: "b1.swarm-large".to_string(),
            reason: "the 256-entity corpus case is S1.c's fixture (F-5) and does not exist \
                     yet; B-1 records the five committed corpus cases"
                .to_string(),
        },
        AbsentLeg {
            leg: "b2.structural-change".to_string(),
            reason: "the instrument is a materialization-heavy Split-storm scenario; none \
                     exists in the committed battery, and the fixture's home is \
                     crates/orrery_games, outside this lane's file list"
                .to_string(),
        },
        AbsentLeg {
            leg: "b4.corpus-checkpoint".to_string(),
            reason: "run_case does not expose its final executor states, so a \
                     corpus-final checkpoint encode is not drivable without editing \
                     crates/orrery_conformance (outside this lane's file list); B-4 is \
                     measured over the battery's logged CoreStates instead — the same \
                     canonical encode, different population"
                .to_string(),
        },
        AbsentLeg {
            leg: "b4.feed-uplink-diff".to_string(),
            reason: "no committed feed_uplink-shaped diff instrument; the leg waits \
                     with its crate"
                .to_string(),
        },
        AbsentLeg {
            leg: "b6.startup".to_string(),
            reason: "composition-root assembly lands with Phase 2 (A10 §8.2); recorded \
                     absent, as the record itself specifies"
                .to_string(),
        },
        AbsentLeg {
            leg: "b7.compile-and-binary-size".to_string(),
            reason: "an honest clean-build timing needs an isolated, cold target \
                     directory; this box shares one kache cache and one disk with live \
                     lanes, so a warm-cache number would be about the cache. The leg \
                     waits for a quiet box and the --with-compile capture it needs"
                .to_string(),
        },
    ];
    if metrics.b3_memory.is_none() {
        legs.push(AbsentLeg {
            leg: "b3.memory".to_string(),
            reason: "RSS accounting is implemented for Linux only; recorded absent on \
                     this platform rather than faked"
                .to_string(),
        });
    }
    legs
}

/// B-1 over every committed corpus case.
fn corpus_leg() -> Vec<CaseBench> {
    CASES
        .iter()
        .map(|case| {
            for _ in 0..CORPUS_WARMUP {
                black_box(run_case(case, false));
            }
            let mut wall_us = Vec::with_capacity(CORPUS_REPS as usize);
            for _ in 0..CORPUS_REPS {
                let start = Instant::now();
                black_box(run_case(case, false));
                wall_us.push(start.elapsed().as_secs_f64() * 1e6);
            }
            let mut per_tick = scale(&wall_us, case.ticks as f64);
            let mut per_entity_tick = scale(&wall_us, (case.ticks * case.entities) as f64);
            CaseBench {
                name: case.name.to_string(),
                entities: case.entities,
                ticks: case.ticks,
                reps: CORPUS_REPS,
                tick_us: percentiles(&mut per_tick),
                entity_tick_us: percentiles(&mut per_entity_tick),
            }
        })
        .collect()
}

/// One observation per battery leg, keyed by game — the visitor lets the
/// generics stay inside [`for_each_game`].
#[derive(Default)]
struct BatteryRunner {
    battery: Vec<BatteryBench>,
    snapshot: Vec<SnapshotBench>,
    claim: Vec<ClaimBench>,
}

impl GameVisitor for BatteryRunner {
    fn visit<G: Game>(&mut self) {
        for scenario in SCENARIOS {
            for _ in 0..BATTERY_WARMUP {
                black_box(play(G::honest(), scenario));
            }
            let mut wall_us = Vec::with_capacity(BATTERY_REPS as usize);
            let mut events = 0;
            for _ in 0..BATTERY_REPS {
                let start = Instant::now();
                let run = play(G::honest(), scenario);
                wall_us.push(start.elapsed().as_secs_f64() * 1e6);
                events = run.events;
                black_box(&run);
            }
            self.battery.push(BatteryBench {
                game: G::META.name.to_string(),
                scenario: scenario.name.to_string(),
                entities: scenario.entities,
                ticks: scenario.ticks,
                reps: BATTERY_REPS,
                events,
                tick_us: percentiles(&mut scale(&wall_us, scenario.ticks as f64)),
                entity_tick_us: percentiles(&mut scale(
                    &wall_us,
                    (scenario.ticks * scenario.entities) as f64,
                )),
            });

            // One logged run, walked for B-4 and B-5: every logged state is a
            // real `G::CoreState`, so the encode and the claim path are the
            // committed ones, not stand-ins.
            let run = play(G::honest(), scenario);
            let mut encode_us = Vec::new();
            let mut byte_lens = Vec::new();
            let mut claim_us = Vec::new();
            for record in &run.log {
                for entry in &record.entries {
                    let start = Instant::now();
                    let bytes = entry.state.to_canonical();
                    encode_us.push(start.elapsed().as_secs_f64() * 1e6);
                    byte_lens.push(bytes.len() as f64);
                    black_box(bytes);

                    let start = Instant::now();
                    let mut state = entry.state.clone();
                    state.quantize();
                    let hash = state_hash(&state);
                    claim_us.push(start.elapsed().as_secs_f64() * 1e6);
                    black_box(hash);
                }
            }
            self.snapshot.push(SnapshotBench {
                game: G::META.name.to_string(),
                scenario: scenario.name.to_string(),
                snapshots: encode_us.len() as u64,
                encode_us: percentiles(&mut encode_us),
                bytes_p50: percentiles(&mut byte_lens).p50,
            });
            self.claim.push(ClaimBench {
                game: G::META.name.to_string(),
                scenario: scenario.name.to_string(),
                claims: claim_us.len() as u64,
                claim_us: percentiles(&mut claim_us),
            });
        }
    }
}

/// B-3 on Linux, `None` elsewhere. The largest committed corpus population is
/// the measured one; the 256-entity case will replace it when S1.c lands.
#[cfg(target_os = "linux")]
fn memory_leg() -> Option<MemoryBench> {
    let case: &Case = CASES.iter().max_by_key(|c| c.entities)?;
    black_box(run_case(case, false));
    let mut high_water: i64 = 0;
    for _ in 0..MEMORY_REPS {
        let before = rss_kib()?;
        black_box(run_case(case, false));
        let after = rss_kib()?;
        high_water = high_water.max(after as i64 - before as i64);
    }
    Some(MemoryBench {
        case: case.name.to_string(),
        entities: case.entities,
        reps: MEMORY_REPS,
        rss_delta_kib_high_water: high_water,
        bytes_per_entity: high_water as f64 * 1024.0 / case.entities as f64,
    })
}

#[cfg(not(target_os = "linux"))]
fn memory_leg() -> Option<MemoryBench> {
    None
}

/// Resident set size, KiB, from /proc/self/status — no page-size arithmetic.
#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches("kB").trim().parse().ok();
        }
    }
    None
}

/// Divide every wall-clock sample into units of `divisor` (ticks or
/// entity-ticks).
fn scale(wall_us: &[f64], divisor: f64) -> Vec<f64> {
    wall_us
        .iter()
        .map(|us| if divisor > 0.0 { us / divisor } else { 0.0 })
        .collect()
}

/// Nearest-rank p50 and p99 over one sample set. Sorts in place.
fn percentiles(samples: &mut [f64]) -> Percentiles {
    samples.sort_by(|a, b| a.total_cmp(b));
    Percentiles {
        p50: nearest_rank(samples, 50.0),
        p99: nearest_rank(samples, 99.0),
    }
}

fn nearest_rank(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[idx.saturating_sub(1).min(sorted.len() - 1)]
}

/// Flatten the suite to named metrics for the comparison report. Paths are
/// dotted and stable: `b1.corpus.<case>.tick_us_p99`,
/// `b1.battery.<game>.<scenario>.entity_tick_us_p50`, and so on.
pub fn metric_map(metrics: &SuiteMetrics) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for case in &metrics.b1_corpus {
        let prefix = format!("b1.corpus.{}.", case.name);
        out.insert(format!("{prefix}tick_us_p50"), case.tick_us.p50);
        out.insert(format!("{prefix}tick_us_p99"), case.tick_us.p99);
        out.insert(
            format!("{prefix}entity_tick_us_p50"),
            case.entity_tick_us.p50,
        );
        out.insert(
            format!("{prefix}entity_tick_us_p99"),
            case.entity_tick_us.p99,
        );
    }
    for leg in &metrics.b1_battery {
        let prefix = format!("b1.battery.{}.{}.", leg.game, leg.scenario);
        out.insert(format!("{prefix}tick_us_p50"), leg.tick_us.p50);
        out.insert(format!("{prefix}tick_us_p99"), leg.tick_us.p99);
        out.insert(
            format!("{prefix}entity_tick_us_p50"),
            leg.entity_tick_us.p50,
        );
        out.insert(
            format!("{prefix}entity_tick_us_p99"),
            leg.entity_tick_us.p99,
        );
        out.insert(format!("{prefix}events"), leg.events as f64);
    }
    if let Some(mem) = &metrics.b3_memory {
        let prefix = format!("b3.memory.{}.", mem.case);
        out.insert(format!("{prefix}bytes_per_entity"), mem.bytes_per_entity);
        out.insert(
            format!("{prefix}rss_delta_kib_high_water"),
            mem.rss_delta_kib_high_water as f64,
        );
    }
    for leg in &metrics.b4_snapshot {
        let prefix = format!("b4.snapshot.{}.{}.", leg.game, leg.scenario);
        out.insert(format!("{prefix}encode_us_p50"), leg.encode_us.p50);
        out.insert(format!("{prefix}encode_us_p99"), leg.encode_us.p99);
        out.insert(format!("{prefix}bytes_p50"), leg.bytes_p50);
    }
    for leg in &metrics.b5_claim {
        let prefix = format!("b5.claim.{}.{}.", leg.game, leg.scenario);
        out.insert(format!("{prefix}claim_us_p50"), leg.claim_us.p50);
        out.insert(format!("{prefix}claim_us_p99"), leg.claim_us.p99);
    }
    for leg in &metrics.b5_capacity_mirror {
        let prefix = format!("b5.capacity-mirror.{}.{}.", leg.backend, leg.entities);
        for (metric, spread) in [
            ("tick_us", leg.tick_us),
            ("collect_output_us", leg.collect_output_us),
            ("state_bytes_sweep_us", leg.state_bytes_sweep_us),
            ("state_bytes_aoi_us", leg.state_bytes_aoi_us),
        ] {
            out.insert(format!("{prefix}{metric}_min"), spread.min);
            out.insert(format!("{prefix}{metric}_p50"), spread.p50);
            out.insert(format!("{prefix}{metric}_p99"), spread.p99);
            out.insert(format!("{prefix}{metric}_max"), spread.max);
        }
        out.insert(format!("{prefix}output_bytes"), leg.output_bytes as f64);
        out.insert(
            format!("{prefix}state_payload_bytes"),
            leg.state_payload_bytes as f64,
        );
    }
    out
}
