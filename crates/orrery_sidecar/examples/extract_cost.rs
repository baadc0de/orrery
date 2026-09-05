//! What the *real* extractor costs, on the world the shipped sidecar steps
//! (#898 step 3).
//!
//! # The gap this closes
//!
//! #920's harness (`orrery_ipc_transport`) measures the IPC round trip with
//! an `extract` column, and its own README is candid that the column is
//! shape-faithful rather than real: *"it is not a Bevy `App`"*. The extractor
//! that ships — [`orrery::ipc::export_ipc_frames`], installed by
//! [`OrreryIpcExportPlugin`](orrery::ipc::OrreryIpcExportPlugin) at
//! `crates/orrery_sidecar/src/lib.rs:285` — landed after that harness and has
//! never been timed. So the harness's headline `ipc_added` rests on an
//! `extract` column nothing has checked against the code it stands for.
//!
//! This example times the shipped system, on the shipped world, at the same
//! N and Hz the harness uses, and writes its percentiles in the same units.
//! The comparison is the point: if the two agree, #920's `ipc_added` carries
//! over to the real extractor; if they do not, the harness understates the
//! extraction term by the difference, and by exactly that much.
//!
//! # What is timed, precisely
//!
//! The app is the shipped [`sidecar`] composition — `MinimalPlugins`,
//! `StatesPlugin`, `OrreryClientPlugins<Synthetic>`, hit registration, the
//! IPC export plugin — with `N` predicted entities and Lightyear's real
//! prediction pipeline live under a declared `P2P` session, exactly as the
//! integration tests stand it up.
//!
//! Each tick runs `App::update()`, which includes the export plugin's own
//! run. The timed call is a *second* invocation of the same system,
//! immediately after. That is deliberate and it is the honest way to isolate
//! the system from a schedule that offers no per-system clock:
//!
//! - It is the same function, the same queries and the same world — nothing
//!   is reconstructed or approximated.
//! - In steady state the cursor diff is empty on *both* runs (membership
//!   stopped changing after the spawns, which the warmup excludes), so the
//!   second run does the same work as the first: iterate the predicted and
//!   interpolated sets, project each component, build `EntityFrame` values,
//!   write one `FrameBatch`.
//! - What it therefore does **not** measure is the spawn/despawn burst on a
//!   membership change. That is stated rather than hidden; the harness's
//!   churn column covers the transport side of it and the batch it builds is
//!   the same `Vec<PersistId>` push either way.
//!
//! **The system is registered once and re-run by id**, never through
//! `run_system_once`. This is load-bearing and was found the hard way: a
//! first cut used `run_system_once`, which calls `System::initialize` on
//! every invocation and so rebuilds the query state over every archetype in
//! the world each tick. That measures Bevy's system-initialization path, not
//! the extractor, and reported roughly an order of magnitude too much.
//! `World::register_system` caches the system between calls, which is what
//! the schedule does, so `run_system` is the comparable call.
//!
//! An **`empty_baseline`** column is reported beside it: an identically
//! registered do-nothing system, run by id the same way on the same world
//! every tick. It is the floor of this measurement technique — the
//! `run_system` dispatch and the `Instant` pair — and the extractor's real
//! cost is the difference. Reporting it means the reader never has to take
//! the harness overhead on trust.
//!
//! `Messages<IpcOutbound>` is drained every tick so the queue depth does not
//! grow into the measurement.
//!
//! # Usage
//!
//! ```text
//! cargo run --release -p orrery_sidecar --example extract_cost -- \
//!     --entities 24 --ticks 36000 --warmup 600 --report extract24.json
//! ```
//!
//! Take it on a quiet box and read the `loadavg` field before trusting the
//! number; a run taken under load measures the scheduler, not the extractor.

use std::fs;
use std::time::Instant;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use lightyear::prelude::{Interpolated, LocalTimelineSync, NetworkingMetadata, Predicted, P2P};

use orrery::ipc::{export_ipc_frames, IpcOutbound};
use orrery_protocol::PersistId;
use orrery_sidecar::{secret, sidecar, spawn_predicted, PredictedPosition};

/// The node seed. Deterministic, so a run is reproducible.
const NODE_SEED: u8 = 9;

/// Command-line configuration, with the harness's defaults.
struct Args {
    entities: u32,
    ticks: u32,
    warmup: u32,
    report: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        entities: 24,
        ticks: 3600,
        warmup: 600,
        report: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .unwrap_or_else(|| panic!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--entities" => args.entities = value().parse().expect("--entities is a number"),
            "--ticks" => args.ticks = value().parse().expect("--ticks is a number"),
            "--warmup" => args.warmup = value().parse().expect("--warmup is a number"),
            "--report" => args.report = Some(value()),
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(args.entities > 0, "--entities must be at least 1");
    assert!(args.ticks > 0, "--ticks must be at least 1");
    args
}

/// `/proc/loadavg`'s first three fields, or an empty string off Linux.
///
/// Recorded in the report because a measurement without its conditions is
/// not a measurement: #920's lie 7 is exactly this, and two earlier attempts
/// at this number were taken under load and thrown away.
fn loadavg() -> String {
    fs::read_to_string("/proc/loadavg")
        .map(|raw| raw.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// The percentile at `q` of an already-sorted sample, by nearest rank.
fn percentile(sorted: &[u128], q: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn main() {
    let args = parse_args();

    let key = secret(NODE_SEED);
    let authority = key.public();
    let mut app = sidecar(key, true);

    // A declared `P2P` session turns Lightyear's real prediction pipeline on
    // without a second peer; #896 already proved the iroh bridge and this
    // example is not re-proving it.
    app.world_mut().spawn(P2P);
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(0));
    app.update();
    app.update();
    assert!(
        app.world().resource::<NetworkingMetadata>().mode.is_p2p(),
        "the prediction pipeline is off: topology did not settle on P2P"
    );
    app.world_mut()
        .resource_mut::<LocalTimelineSync>()
        .set_synced(true);
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));

    for n in 0..args.entities {
        spawn_predicted(&mut app, authority, PersistId::new(u64::from(n) + 1));
    }

    // Registered once, so every timed call reuses the cached system state
    // exactly as the schedule does. See the module docs.
    let extractor = app
        .world_mut()
        .register_system(export_ipc_frames::<Predicted, Interpolated, PredictedPosition>);
    let noop = app.world_mut().register_system(|| {});

    let load_start = loadavg();
    let mut samples: Vec<u128> = Vec::with_capacity(args.ticks as usize);
    let mut baseline: Vec<u128> = Vec::with_capacity(args.ticks as usize);

    for tick in 0..(args.warmup + args.ticks) {
        app.update();

        let world = app.world_mut();

        let started = Instant::now();
        world
            .run_system(extractor)
            .expect("the shipped extractor runs on the shipped world");
        let elapsed = started.elapsed().as_nanos();

        let base_started = Instant::now();
        world.run_system(noop).expect("the empty baseline runs");
        let base_elapsed = base_started.elapsed().as_nanos();

        // Drain both this tick's plugin batch and the timed run's, so the
        // queue depth never grows into a later sample.
        let drained = world
            .resource_mut::<Messages<IpcOutbound>>()
            .drain()
            .count();
        assert!(
            drained > 0,
            "every extraction run emits at least a frames batch"
        );

        if tick >= args.warmup {
            samples.push(elapsed);
            baseline.push(base_elapsed);
        }
    }
    let load_end = loadavg();

    samples.sort_unstable();
    baseline.sort_unstable();
    let n = samples.len();
    let mean = samples.iter().sum::<u128>() as f64 / n as f64;
    let base_mean = baseline.iter().sum::<u128>() as f64 / n as f64;

    let report = format!(
        r#"{{
  "schema": "orrery-extract-cost/1",
  "what": "orrery::ipc::export_ipc_frames on the shipped orrery_sidecar app",
  "entities": {entities},
  "ticks": {n},
  "warmup": {warmup},
  "loadavg_start": "{load_start}",
  "loadavg_end": "{load_end}",
  "extract_ns": {{
    "n": {n},
    "mean_ns": {mean},
    "min_ns": {min},
    "p50_ns": {p50},
    "p99_ns": {p99},
    "p99_9_ns": {p999},
    "max_ns": {max}
  }},
  "empty_baseline_ns": {{
    "n": {n},
    "mean_ns": {base_mean},
    "min_ns": {base_min},
    "p50_ns": {base_p50},
    "p99_ns": {base_p99},
    "p99_9_ns": {base_p999},
    "max_ns": {base_max}
  }}
}}
"#,
        entities = args.entities,
        warmup = args.warmup,
        min = samples[0],
        p50 = percentile(&samples, 0.50),
        p99 = percentile(&samples, 0.99),
        p999 = percentile(&samples, 0.999),
        max = samples[n - 1],
        base_min = baseline[0],
        base_p50 = percentile(&baseline, 0.50),
        base_p99 = percentile(&baseline, 0.99),
        base_p999 = percentile(&baseline, 0.999),
        base_max = baseline[n - 1],
    );

    print!("{report}");
    if let Some(path) = args.report {
        fs::write(&path, &report).expect("the report is writable");
    }
}
