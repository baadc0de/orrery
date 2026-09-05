//! A real C caller links the staticlib and drives the one handle — and the
//! archive is proven to carry none of the runtime the `App` prong ships.
//!
//! As `crates/orrery_sim_host/tests/c_consumer.rs` and spike #1043's
//! `crates/orrery_unreal_host/tests/c_consumer.rs`: Linux-only by `cfg`, so on
//! any other target it compiles to nothing rather than failing on `nm`, `.a`
//! naming and a Unix-only C program.
//!
//! The profile defaults to `debug` so `cargo test --workspace` pays one crate's
//! debug build; the measurement run in `spike.sh` builds `--release` and is
//! not a test.

#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

struct Consumer {
    executable: PathBuf,
}

fn consumer() -> &'static Consumer {
    static CONSUMER: OnceLock<Consumer> = OnceLock::new();
    CONSUMER.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_dir = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("crate has workspace parent");
        let work_dir = env::temp_dir().join(format!(
            "orrery-unreal-direct-c-consumer-{}",
            std::process::id()
        ));
        fs::create_dir_all(&work_dir).expect("create C consumer directory");

        let profile = env::var("ORRERY_UNREAL_DIRECT_PROFILE").unwrap_or_else(|_| "debug".into());
        let library_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| workspace_dir.join("target"), PathBuf::from)
            .join(&profile);
        let native_libs = build_staticlib(&library_dir, &profile);
        let executable = work_dir.join("direct_consumer");
        compile_c_consumer(&manifest_dir, &library_dir, &native_libs, &executable);
        Consumer { executable }
    })
}

/// Builds the staticlib, asserts the exported ABI, and asserts the archive's
/// *purity*: no symbol from `bevy_app`, `bevy_time`, `bevy_state`, lightyear,
/// aeronet, iroh or tokio. That absence is the prong's claim, and Rust symbol mangling
/// carries the crate name, so `nm` can check it rather than the README assert
/// it.
fn build_staticlib(library_dir: &Path, profile: &str) -> Vec<String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut args = vec![
        "rustc",
        "-p",
        "orrery_unreal_direct",
        "--lib",
        "--crate-type",
        "staticlib",
    ];
    if profile == "release" {
        args.push("--release");
    }
    args.extend(["--", "--print", "native-static-libs"]);
    // The link line is *parsed* out of this stderr, so it must not be
    // decorated. The workflows set `CARGO_TERM_COLOR: always`
    // (`.github/workflows/nightly.yml:144`), which survives the pipe: rustc
    // then ends its `native-static-libs` note with a reset, the last token
    // parses as `-lc\x1b[0m`, and the C driver is asked for a library whose
    // name carries an escape -- reported as the invisible
    // `/usr/bin/ld: cannot find -lc: No such file or directory`. Forced off
    // here, as `scripts/fdb-tests.sh:508` does for the same reason.
    let output = Command::new(&cargo)
        .args(&args)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("run cargo rustc for the staticlib");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "build the staticlib:\n{stderr}");
    let library = library_dir.join("liborrery_unreal_direct.a");
    assert!(
        library.is_file(),
        "staticlib exists at {}",
        library.display()
    );

    let native: Vec<String> = stderr
        .lines()
        .filter_map(|line| line.split("native-static-libs:").nth(1))
        .next_back()
        .map(|libs| libs.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let native = if native.is_empty() {
        [
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    } else {
        native
    };
    eprintln!("native-static-libs: {}", native.join(" "));

    let symbols = Command::new("nm")
        .args(["--defined-only"])
        .arg(&library)
        .output()
        .expect("nm reads the archive's symbols");
    let symbols = String::from_utf8_lossy(&symbols.stdout);
    assert_exports(&symbols);
    assert_archive_purity(&symbols);
    native
}

/// The header is a promise the archive keeps: every entry point is a defined
/// text symbol.
fn assert_exports(symbols: &str) {
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
        "orrery_skirmish_host_create",
        "orrery_skirmish_spawn_state",
        "orrery_skirmish_honest_commands",
    ] {
        assert!(
            symbols
                .lines()
                .any(|line| line.ends_with(&format!(" T {required}"))),
            "{required} is a defined text symbol in the staticlib"
        );
    }
}

/// The prong's claim, checked. Legacy mangling spells a crate as
/// `_ZN<len><crate>`; v0 as `_R...<len><crate>`. Both put the crate name
/// in the symbol, and `bevy_ecs`' prefix is `bevy_ecs`, not any of these.
///
/// `bevy_tasks` is deliberately NOT on this list: `bevy_ecs` depends on it
/// and the archive carries its single-threaded pool and the unused
/// `ComputeTaskPool::get` statics as code reachable from `bevy_ecs`'
/// parallel-iteration paths, which this host never calls. Whether a pool is
/// *spawned* is a runtime fact, and the smoke test reads it from
/// `/proc/self/task` instead.
fn assert_archive_purity(symbols: &str) {
    for absent in [
        "bevy_app",
        "bevy_time",
        "bevy_state",
        "lightyear",
        "iroh",
        "tokio",
        "aeronet",
    ] {
        let hits: Vec<&str> = symbols
            .lines()
            .filter(|line| {
                line.contains(&format!("{}{absent}", absent.len()))
                    || line.contains(&format!("{absent}.."))
            })
            .take(3)
            .collect();
        assert!(
            hits.is_empty(),
            "the archive carries no symbol from `{absent}`; found {hits:?}"
        );
    }
}

fn compile_c_consumer(
    manifest_dir: &Path,
    library_dir: &Path,
    native_libs: &[String],
    executable: &Path,
) {
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let sim_host_include = manifest_dir
        .parent()
        .expect("crates dir")
        .join("orrery_sim_host/include");
    let status = Command::new(compiler)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(manifest_dir.join("include"))
        .arg("-I")
        .arg(sim_host_include)
        .arg(manifest_dir.join("examples/c/direct_consumer.c"))
        .arg(library_dir.join("liborrery_unreal_direct.a"))
        .args(native_libs)
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

/// `key=value` fields of the one-line summary.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} in {line}"))
}

/// The lockstep property the `App` prong had to prime for, here for free:
/// 120 ticks issued by the C accumulator, the host counts 120, and there is
/// no second counter to be one behind. Handle creation spawns no thread.
#[test]
fn a_c_frame_loop_drives_the_host_with_one_clock_and_no_threads() {
    let output = run(&["smoke"]);
    let printed = stdout(&output);
    let line = printed.lines().next().expect("one summary line");
    assert_eq!(field(line, "ticks"), "120", "{line}");
    assert_eq!(field(line, "host_next_tick"), "120", "{line}");
    assert_eq!(field(line, "authority_next_tick"), "120", "{line}");
    assert_eq!(field(line, "samples"), "120", "{line}");
    assert_eq!(field(line, "input_dropped"), "0", "{line}");
    assert_eq!(field(line, "step_failed"), "0", "{line}");
    assert_eq!(field(line, "snapshot_failed"), "0", "{line}");
    assert_eq!(field(line, "restore_failed"), "0", "{line}");
    assert_eq!(field(line, "rollback_failed"), "0", "{line}");
    assert_eq!(field(line, "decode_failures"), "0", "{line}");
    let events: u64 = field(line, "events").parse().expect("events count");
    assert!(
        events > 0,
        "the honest pilots fired and the adapter routed damage: {line}"
    );
    let corrections: u64 = field(line, "corrections").parse().expect("count");
    let rollbacks: u64 = field(line, "rollbacks").parse().expect("count");
    assert!(corrections > 0 && rollbacks == corrections, "{line}");
    assert_eq!(
        field(line, "threads_before"),
        field(line, "threads_after_create"),
        "creating the host spawned no thread: {line}"
    );
    assert_eq!(
        field(line, "threads_before"),
        field(line, "threads_end"),
        "and none appeared during the loop: {line}"
    );
}

/// The host alone, no ring, no authority: the control that matches #1043's
/// `--no-app` run shape exactly.
#[test]
fn the_host_runs_without_the_ring() {
    let output = run(&["smoke", "--no-ring"]);
    let line = stdout(&output);
    let line = line.lines().next().expect("one summary line");
    assert_eq!(field(line, "host_next_tick"), "120", "{line}");
    assert_eq!(field(line, "samples"), "120", "{line}");
    assert_eq!(field(line, "corrections"), "0", "{line}");
    assert_eq!(
        field(line, "threads_before"),
        field(line, "threads_end"),
        "no threads: {line}"
    );
}

/// The falsifier #1052 names, answered: the rollback path IS drivable through
/// the existing ABI. An identity correction at depth 9 replays hash for hash;
/// a divergent one changes hashes; the same divergent one again changes none,
/// so the ring now holds the corrected timeline.
#[test]
fn rollback_through_the_abi_is_hash_exact_and_the_ring_follows_the_correction() {
    let output = run(&["rollback"]);
    let printed = stdout(&output);
    let line = printed.lines().next().expect("one summary line");
    assert_eq!(field(line, "depth"), "9", "{line}");
    assert_eq!(field(line, "host_next_tick"), "60", "{line}");
    assert_eq!(field(line, "identity_ok"), "1", "{line}");
    assert_eq!(field(line, "identity_hashes_changed"), "0", "{line}");
    assert_eq!(field(line, "identity_residual_mm"), "0", "{line}");
    assert_eq!(field(line, "divergent_ok"), "1", "{line}");
    let changed: u64 = field(line, "divergent_hashes_changed")
        .parse()
        .expect("count");
    assert!(
        changed > 0,
        "the authority's bytes changed the timeline: {line}"
    );
    assert_eq!(field(line, "repeat_ok"), "1", "{line}");
    assert_eq!(field(line, "repeat_hashes_changed"), "0", "{line}");
    assert_eq!(field(line, "repeat_residual_mm"), "0", "{line}");
    assert_eq!(field(line, "restore_failed"), "0", "{line}");
    assert_eq!(field(line, "replay_step_failed"), "0", "{line}");
    eprintln!("{line}");
}
