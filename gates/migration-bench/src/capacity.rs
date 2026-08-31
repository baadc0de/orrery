//! Capacity-scale B-5 measurements over the public simulation-host seam.
//!
//! A3's P4 copied `(PersistId, Pos)` rows into a `Vec`.  The path which exists
//! now is different: a host owns canonical state behind a substrate-neutral
//! seam and exports owned canonical bytes.  This leg therefore times the
//! actual [`SimulationHost`] operations a Bevy or Unreal mirror can call, and
//! times the same canonical tick on both admitted substrates.

use std::hint::black_box;
use std::time::Instant;

use orrery_core::TickBackend;
use orrery_games::Regolith;
use orrery_games::game::Game;
use orrery_games::regolith::state::{TRAIL_CAPACITY, TRAIL_SAMPLE_TICKS};
use orrery_protocol::{PersistId, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;
use orrery_sim_host::{NoEventRouting, SimulationHost, SimulationHostConfig, TickCount};
use serde::{Deserialize, Serialize};

/// The populations named by the repository's own sizing evidence:
///
/// - 10k is docs/14's comfortable point and the A3 reference population;
/// - 20k is docs/14's 2 Hz service-knee conversion;
/// - 24k is the upper end of docs/14's 4k-player entity implication.
const POPULATIONS: [u64; 3] = [10_000, 20_000, 24_000];
/// Four samples at 5 Hz fill Regolith's canonical trail at 60 Hz.
const STATE_WARMUP_TICKS: u64 = (TRAIL_CAPACITY as u64) * (TRAIL_SAMPLE_TICKS as u64);
/// Untimed calls before output timings. Each output call still allocates its
/// owned result; this warms code, allocator and cache state, not a retained
/// destination buffer.
const OUTPUT_WARMUP_REPS: u32 = 3;
/// Timed repetitions per operation and configuration.
const REPS: u32 = 11;
/// D16's bounded high-rate interest set, as converted in docs/14 §6.
const AOI_ENTITIES: usize = 24;
const SEED: UniverseSeed = UniverseSeed([0x42; 32]);

/// One backend/population configuration of the capacity-scale B-5 leg.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityMirrorBench {
    /// Canonical storage substrate (`executor` or `ecs`).
    pub backend: String,
    /// Canonical entities installed and visited by every measured operation.
    pub entities: u64,
    /// Workload shape. Kept in the report so a row cannot silently become a
    /// different state mix while retaining its metric name.
    pub workload: String,
    /// Untimed canonical ticks completed before measurement.
    pub state_warmup_ticks: u64,
    /// Untimed calls completed before each output-operation distribution.
    pub output_warmup_reps: u32,
    /// Timed samples behind each distribution.
    pub reps: u32,
    /// Bytes in one full stable-id-framed `collect_output_bytes` result.
    pub output_bytes: u64,
    /// Canonical payload bytes returned by a sweep of `state_bytes` over the
    /// same population (the full output additionally carries 12 framing bytes
    /// per entity).
    pub state_payload_bytes: u64,
    /// One `SimulationHost::step(1)`, including canonical arithmetic, hashing,
    /// the substrate, scheduling and the returned hash report.
    pub tick_us: Spread,
    /// Full-population `collect_output_bytes` cost.
    pub collect_output_us: Spread,
    /// Calling `state_bytes` once for every entity, in stable-id order.
    pub state_bytes_sweep_us: Spread,
    /// Entities selected by the AOI-sized targeted lookup.
    pub aoi_entities: u32,
    /// Calling `state_bytes` for 24 evenly spaced entities in the full backing
    /// population. This is the existing AOI-sized consumer primitive; it does
    /// not include AOI selection itself.
    pub state_bytes_aoi_us: Spread,
    /// Blake3 of one untimed full output. Matching hashes prove the two backend
    /// rows measured the same canonical bytes without creating a new golden.
    pub output_blake3: String,
}

/// Min/median/p99/max over repeated wall-clock samples.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    /// Smallest sample.
    pub min: f64,
    /// Nearest-rank median.
    pub p50: f64,
    /// Nearest-rank 99th percentile.
    pub p99: f64,
    /// Largest sample.
    pub max: f64,
}

/// Run every capacity configuration, executor and ECS paired at each size.
pub fn run() -> Vec<CapacityMirrorBench> {
    let mut rows = Vec::new();
    for entities in POPULATIONS {
        let executor = measure(
            "executor",
            entities,
            SimulationHost::new(
                SimulationHostConfig::new(SEED),
                Regolith::honest(),
                NoEventRouting,
            ),
        );
        let ecs_backend = EcsBackend::new(Regolith::honest(), SEED);
        let ecs = measure(
            "ecs",
            entities,
            SimulationHost::on_backend(
                SimulationHostConfig::new(SEED),
                ecs_backend,
                NoEventRouting,
            ),
        );
        assert_eq!(
            executor.output_blake3, ecs.output_blake3,
            "capacity rows must describe byte-identical canonical output"
        );
        assert_eq!(executor.output_bytes, ecs.output_bytes);
        assert_eq!(executor.state_payload_bytes, ecs.state_payload_bytes);
        rows.extend([executor, ecs]);
    }
    rows
}

fn measure<B>(
    backend: &str,
    entities: u64,
    mut host: SimulationHost<Regolith, NoEventRouting, B>,
) -> CapacityMirrorBench
where
    B: TickBackend<Regolith>,
{
    let game = Regolith::honest();
    let ids = (1..=entities).map(PersistId::new).collect::<Vec<_>>();
    for (slot, entity) in ids.iter().copied().enumerate() {
        host.install_state(entity, game.spawn(entity, slot as u64));
    }

    // This is a state warm-up as well as a cache warm-up: after 48 ticks every
    // live craft's fixed-capacity canonical trail is full, so output rows carry
    // the maximum trail payload instead of the cheaper spawn-state payload.
    for _ in 0..STATE_WARMUP_TICKS {
        black_box(host.step(TickCount::new(1)));
    }

    let mut tick_us = Vec::with_capacity(REPS as usize);
    for _ in 0..REPS {
        let started = Instant::now();
        let report = host.step(TickCount::new(1));
        black_box(&report);
        tick_us.push(started.elapsed().as_secs_f64() * 1e6);
    }

    for _ in 0..OUTPUT_WARMUP_REPS {
        black_box(
            host.collect_output_bytes()
                .expect("capacity output fits its fixed-width framing"),
        );
    }
    let mut collect_output_us = Vec::with_capacity(REPS as usize);
    let mut output_bytes = 0_u64;
    for _ in 0..REPS {
        let started = Instant::now();
        let output = host
            .collect_output_bytes()
            .expect("capacity output fits its fixed-width framing");
        let elapsed = started.elapsed().as_secs_f64() * 1e6;
        output_bytes = output.as_bytes().len() as u64;
        black_box(output.as_bytes());
        collect_output_us.push(elapsed);
    }

    for _ in 0..OUTPUT_WARMUP_REPS {
        black_box(state_bytes_sweep(&host, &ids));
    }
    let mut state_bytes_sweep_us = Vec::with_capacity(REPS as usize);
    let mut state_payload_bytes = 0_u64;
    for _ in 0..REPS {
        let started = Instant::now();
        state_payload_bytes = state_bytes_sweep(&host, &ids);
        state_bytes_sweep_us.push(started.elapsed().as_secs_f64() * 1e6);
        black_box(state_payload_bytes);
    }

    let aoi_ids = evenly_spaced_ids(&ids, AOI_ENTITIES);
    for _ in 0..OUTPUT_WARMUP_REPS {
        black_box(state_bytes_sweep(&host, &aoi_ids));
    }
    let mut state_bytes_aoi_us = Vec::with_capacity(REPS as usize);
    for _ in 0..REPS {
        let started = Instant::now();
        black_box(state_bytes_sweep(&host, &aoi_ids));
        state_bytes_aoi_us.push(started.elapsed().as_secs_f64() * 1e6);
    }

    let canonical = host
        .collect_output_bytes()
        .expect("capacity output fits its fixed-width framing");
    let output_blake3 = blake3::hash(canonical.as_bytes()).to_string();

    CapacityMirrorBench {
        backend: backend.to_string(),
        entities,
        workload: "regolith/all-craft/no-input/full-fixed-trail".to_string(),
        state_warmup_ticks: STATE_WARMUP_TICKS,
        output_warmup_reps: OUTPUT_WARMUP_REPS,
        reps: REPS,
        output_bytes,
        state_payload_bytes,
        tick_us: spread(&mut tick_us),
        collect_output_us: spread(&mut collect_output_us),
        state_bytes_sweep_us: spread(&mut state_bytes_sweep_us),
        aoi_entities: aoi_ids.len() as u32,
        state_bytes_aoi_us: spread(&mut state_bytes_aoi_us),
        output_blake3,
    }
}

fn evenly_spaced_ids(ids: &[PersistId], count: usize) -> Vec<PersistId> {
    let count = count.min(ids.len());
    if count == 0 {
        return Vec::new();
    }
    (0..count)
        .map(|index| ids[index * ids.len() / count])
        .collect()
}

fn state_bytes_sweep<B>(
    host: &SimulationHost<Regolith, NoEventRouting, B>,
    ids: &[PersistId],
) -> u64
where
    B: TickBackend<Regolith>,
{
    ids.iter()
        .map(|entity| {
            let bytes = host
                .state_bytes(*entity)
                .expect("every installed capacity entity remains present");
            let len = bytes.len() as u64;
            black_box(bytes);
            len
        })
        .sum()
}

fn spread(samples: &mut [f64]) -> Spread {
    samples.sort_by(|a, b| a.total_cmp(b));
    Spread {
        min: samples.first().copied().unwrap_or(0.0),
        p50: nearest_rank(samples, 50.0),
        p99: nearest_rank(samples, 99.0),
        max: samples.last().copied().unwrap_or(0.0),
    }
}

fn nearest_rank(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}
