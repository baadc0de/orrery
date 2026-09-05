//! A real C caller links the staticlib and drives both handles.
//!
//! As `crates/orrery_sim_host/tests/c_consumer.rs`, with the lesson D53 §"What
//! this record could not establish" item 3 records applied: this file is
//! Linux-only by `cfg`, so on any other target it compiles to nothing rather
//! than failing on `nm`, `.a` naming and a Unix-only C program. The Windows
//! half of #1043's C-consumer proof needs a Windows host and is not here.
//!
//! The profile defaults to `debug` so `cargo test --workspace` pays one crate's
//! debug build, not a release build of the whole net/predict graph; the
//! measurement run in `spike.sh` builds `--release` and is not a test.

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
            "orrery-unreal-host-c-consumer-{}",
            std::process::id()
        ));
        fs::create_dir_all(&work_dir).expect("create C consumer directory");

        let profile = env::var("ORRERY_UNREAL_HOST_PROFILE").unwrap_or_else(|_| "debug".into());
        let library_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| workspace_dir.join("target"), PathBuf::from)
            .join(&profile);
        let native_libs = build_staticlib(&library_dir, &profile);
        let executable = work_dir.join("spike_consumer");
        compile_c_consumer(&manifest_dir, &library_dir, &native_libs, &executable);
        Consumer { executable }
    })
}

/// Builds the staticlib and returns the system libraries rustc says it needs
/// (`--print native-static-libs`): the Linux analogue of #1043's output 4,
/// recorded from the linker's own answer rather than assumed.
fn build_staticlib(library_dir: &Path, profile: &str) -> Vec<String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut args = vec![
        "rustc",
        "-p",
        "orrery_unreal_host",
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
    let library = library_dir.join("liborrery_unreal_host.a");
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
    // cargo may reuse a cached compile and print no note; the set is then the
    // one every Linux staticlib needs. Printed either way so the test log
    // carries what the link actually used.
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

    // The generic entry points live in orrery_sim_host's rlib and the App's
    // in this crate; the archive must carry both sets as defined text symbols
    // or the header is a promise nothing keeps.
    let symbols = Command::new("nm")
        .args(["--defined-only"])
        .arg(&library)
        .output()
        .expect("nm reads the archive's symbols");
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
        "orrery_skirmish_host_create",
        "orrery_skirmish_spawn_state",
        "orrery_skirmish_honest_commands",
        "orrery_app_abi_version",
        "orrery_app_create",
        "orrery_app_update",
        "orrery_app_timeline_read",
        "orrery_app_on_creating_thread",
        "orrery_app_request_panic",
        "orrery_app_destroy",
    ] {
        assert!(
            symbols
                .lines()
                .any(|line| line.ends_with(&format!(" T {required}"))),
            "{required} is a defined text symbol in the staticlib"
        );
    }
    native
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
        .arg(manifest_dir.join("examples/c/spike_consumer.c"))
        .arg(library_dir.join("liborrery_unreal_host.a"))
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

/// `key=value` fields of the one-line smoke summary.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} in {line}"))
}

/// The lockstep property the `App` prong has to have, and what it actually
/// has: 120 ticks issued by the C accumulator, the host counts 120, and
/// lightyear's timeline and Bevy's fixed schedule count 119 — with the manual
/// clock, the mechanism by which a foreign accumulator would own Bevy's clock.
/// The missing one is Bevy's zero-delta startup frame (see `app.rs`'s unit
/// test): a constant one-tick offset, not drift.
#[test]
fn a_c_frame_loop_drives_the_host_and_the_app_in_lockstep() {
    let output = run(&["smoke", "--clock", "manual"]);
    let printed = stdout(&output);
    let line = printed.lines().next().expect("one summary line");
    assert_eq!(field(line, "ticks"), "120", "{line}");
    assert_eq!(field(line, "host_next_tick"), "120", "{line}");
    assert_eq!(field(line, "lightyear_tick"), "119", "{line}");
    assert_eq!(field(line, "fixed_steps"), "119", "{line}");
    assert_eq!(field(line, "frames"), "120", "{line}");
    assert_eq!(field(line, "samples"), "120", "{line}");
    assert_eq!(field(line, "input_dropped"), "0", "{line}");
    assert_eq!(field(line, "step_failed"), "0", "{line}");
    assert_eq!(field(line, "app_update_failed"), "0", "{line}");
    assert_eq!(field(line, "decode_failures"), "0", "{line}");
    let events: u64 = field(line, "events").parse().expect("events count");
    assert!(
        events > 0,
        "the honest pilots fired and the adapter routed damage: {line}"
    );
    let before: u32 = field(line, "threads_before").parse().expect("thread count");
    let after: u32 = field(line, "threads_after_app")
        .parse()
        .expect("thread count");
    assert!(
        after > before,
        "creating the App spawned threads into the C process ({before} -> {after})"
    );
}

/// The host alone, no `App`: the control the bench compares against.
#[test]
fn the_host_runs_without_the_app_beside_it() {
    let output = run(&["smoke", "--no-app"]);
    let line = stdout(&output);
    let line = line.lines().next().expect("one summary line");
    assert_eq!(field(line, "host_next_tick"), "120", "{line}");
    assert_eq!(field(line, "samples"), "120", "{line}");
    assert_eq!(
        field(line, "threads_before"),
        field(line, "threads_after_app"),
        "no App, no threads: {line}"
    );
}

/// A panic inside a Bevy system reaches C as `ORRERY_HOST_PANIC`, the handle
/// reports itself poisoned afterwards, and destroy still works.
#[test]
fn a_system_panic_inside_app_update_crosses_the_boundary_as_a_code() {
    let output = run(&["panic"]);
    assert_eq!(stdout(&output), "update=7 after=6 destroy=0\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("boundary probe"),
        "the panic message was printed by the hook, not swallowed silently"
    );
}

/// Updating the `App` from a thread other than the one that created it: the
/// result codes are the finding, the assertion is only that the process
/// survives, the handle reports something, and destroy on the creating thread
/// succeeds.
#[test]
fn an_update_from_another_thread_is_reported_not_crashed() {
    let output = run(&["threadhop"]);
    let printed = stdout(&output);
    let line = printed.lines().next().expect("one summary line");
    assert!(line.starts_with("threadhop "), "{line}");
    assert_eq!(field(line, "on_creating_thread"), "0", "{line}");
    assert_eq!(field(line, "destroy"), "0", "{line}");
    eprintln!("{line}");
}
