//! A real C caller drives the generic ABI through the header alone.
//!
//! A Rust-only call of an `extern "C"` function does not test the ABI: it
//! tests that Rust can call Rust.  These tests build the reference `cdylib`
//! (`examples/synthetic_abi.rs`), compile `examples/c/synthetic_consumer.c`
//! with a C compiler against `include/orrery_sim_host.h`, and compare what
//! the C program prints against the same scenario driven from Rust.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use orrery_core::CoreCodec;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use synthetic::{Synthetic, SyntheticAdapter, SyntheticEvent, SyntheticInput, SyntheticState};

#[path = "support/synthetic.rs"]
mod synthetic;
use orrery_sim_host::{SimulationHost, SimulationHostConfig, TickCount};

struct Consumer {
    executable: PathBuf,
    work_dir: PathBuf,
}

fn consumer() -> &'static Consumer {
    static CONSUMER: OnceLock<Consumer> = OnceLock::new();
    CONSUMER.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_dir = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("crate has workspace parent");
        let work_dir =
            env::temp_dir().join(format!("orrery-sim-host-c-consumer-{}", std::process::id()));
        fs::create_dir_all(&work_dir).expect("create C consumer directory");

        let library_dir = env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_dir.join("target"))
            .join("release")
            .join("examples");
        build_cdylib(&library_dir);
        let executable = work_dir.join("synthetic_consumer");
        compile_c_consumer(&manifest_dir, &library_dir, &executable);
        Consumer {
            executable,
            work_dir,
        }
    })
}

fn build_cdylib(library_dir: &Path) {
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "build",
            "--release",
            "-p",
            "orrery_sim_host",
            "--example",
            "synthetic_abi",
        ])
        .status()
        .expect("run cargo build for the reference cdylib");
    assert!(status.success(), "build the reference cdylib");
    let library = library_dir.join("libsynthetic_abi.so");
    assert!(library.is_file(), "cdylib exists at {}", library.display());

    // The generic entry points live in the rlib; the cdylib must export them
    // or the header is a promise nothing keeps.
    let symbols = Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(&library)
        .output()
        .expect("nm reads the cdylib's dynamic symbols");
    let symbols = String::from_utf8_lossy(&symbols.stdout);
    for required in [
        "orrery_host_abi_version",
        "orrery_host_destroy",
        "orrery_host_ruleset_id",
        "orrery_host_next_tick",
        "orrery_host_submit_command",
        "orrery_host_install_state",
        "orrery_host_remove_state",
        "orrery_host_step",
        "orrery_host_drain_state_hashes",
        "orrery_host_drain_events",
        "orrery_host_collect_states",
        "orrery_host_state",
        "orrery_host_snapshot",
        "orrery_host_restore",
        "orrery_synthetic_host_create",
    ] {
        assert!(
            symbols
                .lines()
                .any(|line| line.ends_with(&format!(" T {required}"))),
            "{required} is exported from the cdylib"
        );
    }
}

fn compile_c_consumer(manifest_dir: &Path, library_dir: &Path, executable: &Path) {
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let rpath = format!("-Wl,-rpath,{}", library_dir.display());
    let status = Command::new(compiler)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(manifest_dir.join("include"))
        .arg(manifest_dir.join("examples/c/synthetic_consumer.c"))
        .arg("-L")
        .arg(library_dir)
        .arg("-lsynthetic_abi")
        .arg(rpath)
        .arg("-o")
        .arg(executable)
        .status()
        .expect("compile the C consumer");
    assert!(status.success(), "compile the C consumer with a C compiler");
}

fn run(args: &[&str]) -> Output {
    let output = Command::new(&consumer().executable)
        .args(args)
        .output()
        .expect("run the compiled C consumer");
    assert!(
        output.status.success(),
        "C consumer {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("C output is UTF-8")
}

fn state_line(entity: PersistId, state: &SyntheticState) -> String {
    format!(
        "entity={} pos=({},{},{}) vel=({},{},{}) health={} target={} sightings={}",
        entity.0,
        state.position_um[0],
        state.position_um[1],
        state.position_um[2],
        state.velocity_um_per_tick[0],
        state.velocity_um_per_tick[1],
        state.velocity_um_per_tick[2],
        state.health,
        state.target,
        state.sightings
    )
}

fn host(first_tick: u64) -> SimulationHost<Synthetic, SyntheticAdapter> {
    SimulationHost::new(
        SimulationHostConfig::new(UniverseSeed([0; 32])).starting_at(Tick::new(first_tick)),
        Synthetic,
        SyntheticAdapter,
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The determinism property re-proved on the generic path: a fixed-step
/// accumulator in a foreign variable-rate loop — steady, jittered, and a
/// forced 250 ms hitch — reproduces the host's arithmetic field-exactly.
/// The reference is the same 120 ticks issued from Rust in one call.
#[test]
fn a_c_frame_loop_with_a_hitch_reproduces_host_arithmetic_field_exactly() {
    let output = run(&["loop"]);
    let printed = stdout(&output);

    let mut reference = host(0);
    let entity = PersistId::new(7);
    reference.install_state(
        entity,
        SyntheticState {
            velocity_um_per_tick: [1_234, -567, 89],
            health: 100,
            ..SyntheticState::default()
        },
    );
    let mut command = entity.0.to_le_bytes().to_vec();
    command.extend(SyntheticInput::Impulse([10, 0, 0]).to_canonical());
    reference
        .submit_command_bytes(&command)
        .expect("canonical impulse decodes");
    let report = reference.step(TickCount::new(120));
    let state = SyntheticState::decode(&reference.state_bytes(entity).expect("entity is held"))
        .expect("state decodes");
    let last = report.state_hashes.last().expect("120 ticks hashed");

    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines.len(), 3, "loop output:\n{printed}");
    assert!(
        lines[0].ends_with(" ticks=120 next_tick=120"),
        "the accumulator issued exactly 120 ticks: {}",
        lines[0]
    );
    assert_eq!(lines[1], state_line(entity, &state));
    assert_eq!(
        lines[2],
        format!(
            "hashes=120 last_tick={} last_hash={}",
            last.tick.0,
            hex(&last.hash)
        )
    );
    assert_ne!(
        state.position_um, [0; 3],
        "the reference actually moved; a no-op host would pass a lazy equality"
    );
}

/// Snapshot, step forward, restore, step again: the C side asserts the
/// restored bytes equal the snapshotted bytes and that the replayed run's
/// states and hashes equal the first run's, and this side checks the
/// replayed states against a Rust host driven through the same history.
#[test]
fn a_c_caller_snapshots_steps_restores_and_replays_identically() {
    let output = run(&["rewind"]);
    let printed = stdout(&output);

    let mut reference = host(0);
    reference.install_state(
        PersistId::new(1),
        SyntheticState {
            velocity_um_per_tick: [1_000, 0, 0],
            health: 100,
            target: 2,
            ..SyntheticState::default()
        },
    );
    reference.install_state(
        PersistId::new(2),
        SyntheticState {
            position_um: [5_000, 0, 0],
            velocity_um_per_tick: [0, 1_000, 0],
            health: 5,
            target: 1,
            ..SyntheticState::default()
        },
    );
    reference.step(TickCount::new(3));
    let mut command = 1_u64.to_le_bytes().to_vec();
    command.extend(SyntheticInput::Impulse([0, 0, 333]).to_canonical());
    reference
        .submit_command_bytes(&command)
        .expect("canonical impulse decodes");
    let report = reference.step(TickCount::new(5));
    let expected: Vec<String> = [1, 2]
        .into_iter()
        .map(PersistId::new)
        .map(|entity| {
            let state =
                SyntheticState::decode(&reference.state_bytes(entity).expect("entity is held"))
                    .expect("state decodes");
            state_line(entity, &state)
        })
        .collect();

    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(
        lines,
        vec![
            "restore_exact=1",
            "extra_entity_after_restore=not_found",
            "next_tick_after_restore=3",
            "replay_states_equal=1",
            &format!("replay_hashes_equal=1 hashes={}", report.state_hashes.len()),
            expected[0].as_str(),
            expected[1].as_str(),
        ],
        "rewind output:\n{printed}"
    );
    let struck = SyntheticState::decode(
        &reference
            .state_bytes(PersistId::new(2))
            .expect("entity is held"),
    )
    .expect("state decodes");
    assert!(
        struck.health < 5,
        "the watcher's strikes reached the watched entity through the adapter"
    );
}

/// The event drain, from C.  The output half of the boundary is covered by
/// `collect_states` everywhere above; this covers the event half, which is
/// the one that consumes what it hands out.
///
/// It also pins the no-loss property the `peek`-then-`clear` split exists
/// for: a drain whose buffer is one byte short must report the size and
/// clear nothing, or a consumer that guessed low would silently lose a tick's
/// events.  The events themselves are decoded on the C side from the live
/// records and compared against the same host driven from Rust.
#[test]
fn a_c_caller_drains_events_and_a_short_buffer_drains_nothing() {
    let output = run(&["events"]);
    let printed = stdout(&output);

    let mut reference = host(0);
    reference.install_state(
        PersistId::new(1),
        SyntheticState {
            velocity_um_per_tick: [1_000, 0, 0],
            health: 100,
            target: 2,
            ..SyntheticState::default()
        },
    );
    reference.install_state(
        PersistId::new(2),
        SyntheticState {
            position_um: [5_000, 0, 0],
            velocity_um_per_tick: [0, 1_000, 0],
            health: 100,
            target: 1,
            ..SyntheticState::default()
        },
    );
    reference.step(TickCount::new(2));
    let bytes = reference
        .drain_event_bytes()
        .expect("events fit the buffer")
        .into_bytes();
    assert!(
        !bytes.is_empty(),
        "the scenario emitted no events, so the C side would pass on an empty drain"
    );

    // Decode the reference records the same way the C consumer does.
    let mut expected = vec![format!("required={}", bytes.len())];
    let mut decoded = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let source = u64::from_le_bytes(bytes[at..at + 8].try_into().expect("source"));
        let length =
            u32::from_le_bytes(bytes[at + 8..at + 12].try_into().expect("length")) as usize;
        at += 12;
        let SyntheticEvent::Struck { target, damage } =
            SyntheticEvent::decode(&bytes[at..at + length]).expect("event decodes");
        at += length;
        decoded.push(format!(
            "event source={source} target={} damage={damage}",
            target.0
        ));
    }
    expected.push(format!("events={}", decoded.len()));
    expected.extend(decoded);
    expected.push("events_after_drain=0".to_owned());

    assert_eq!(
        printed.lines().collect::<Vec<_>>(),
        expected,
        "events output:\n{printed}"
    );
    assert!(
        reference
            .drain_event_bytes()
            .expect("a second drain succeeds")
            .is_empty(),
        "the reference agrees that a successful drain empties the buffer"
    );
}

/// A panic inside a stepped ruleset reaches C as a result code, the handle
/// reports itself poisoned afterwards, and destroy still works.
#[test]
fn a_panic_inside_a_stepped_ruleset_crosses_the_boundary_as_an_error_code() {
    let output = run(&["panic"]);
    assert_eq!(stdout(&output), "step=7 after=6 destroy=0\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("boundary probe"),
        "the panic message was printed by the hook, not swallowed silently"
    );
}

/// A cross-language fixture that travels the live encoding path: the bytes
/// come from a real host's `snapshot().to_bytes()`, not a hand-built encoder,
/// and the C side restores them, steps, and prints what a Rust host stepping
/// from the same snapshot holds.
#[test]
fn a_snapshot_written_by_the_live_host_restores_and_steps_on_the_c_side() {
    let mut writer = host(40);
    writer.install_state_observed(
        PersistId::new(11),
        SyntheticState {
            position_um: [1_000, 2_000, 3_000],
            velocity_um_per_tick: [-1_500, 0, 250],
            health: 42,
            target: 12,
            ..SyntheticState::default()
        },
        Tick::new(40),
    );
    writer.install_state_observed(
        PersistId::new(12),
        SyntheticState {
            health: 7,
            ..SyntheticState::default()
        },
        Tick::new(39),
    );
    writer.step(TickCount::new(2));
    let snapshot = writer.snapshot();
    let fixture = consumer().work_dir.join("live-host.snapshot");
    fs::write(&fixture, snapshot.to_bytes().expect("snapshot encodes")).expect("write the fixture");

    let mut reader = host(0);
    reader
        .restore(&snapshot)
        .expect("the writer's snapshot restores");
    reader.step(TickCount::new(1));
    let expected: Vec<String> = [11, 12]
        .into_iter()
        .map(PersistId::new)
        .map(|entity| {
            let state =
                SyntheticState::decode(&reader.state_bytes(entity).expect("entity is held"))
                    .expect("state decodes");
            state_line(entity, &state)
        })
        .collect();

    let output = run(&["fixture", fixture.to_str().expect("fixture path is UTF-8")]);
    let printed = stdout(&output);
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(
        lines,
        vec![
            "restored_next_tick=42",
            "next_tick=43",
            expected[0].as_str(),
            expected[1].as_str(),
        ],
        "fixture output:\n{printed}"
    );
    assert!(
        expected[0].contains("sightings=3"),
        "the watcher saw its target on every tick, including the one after restore: {}",
        expected[0]
    );
}
