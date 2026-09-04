//! A real C caller links the staticlib, walks the scripted scenes and rolls
//! back across every frame change against a stand-in authority — and the
//! shared header the Unreal module includes is compiled as C++ to prove it
//! can be.
//!
//! As spike #1052's `crates/orrery_unreal_direct/tests/c_consumer.rs`:
//! Linux-only by `cfg`, debug profile by default.

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
            "orrery-unreal-interiors-c-consumer-{}",
            std::process::id()
        ));
        fs::create_dir_all(&work_dir).expect("create C consumer directory");

        let profile =
            env::var("ORRERY_UNREAL_INTERIORS_PROFILE").unwrap_or_else(|_| "debug".into());
        let library_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| workspace_dir.join("target"), PathBuf::from)
            .join(&profile);
        let native_libs = build_staticlib(&library_dir, &profile);
        let executable = work_dir.join("interiors_consumer");
        compile_c_consumer(&manifest_dir, &library_dir, &native_libs, &executable);
        check_shared_header_as_cxx(&manifest_dir);
        Consumer { executable }
    })
}

fn build_staticlib(library_dir: &Path, profile: &str) -> Vec<String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut args = vec![
        "rustc",
        "-p",
        "orrery_unreal_interiors",
        "--lib",
        "--crate-type",
        "staticlib",
    ];
    if profile == "release" {
        args.push("--release");
    }
    args.extend(["--", "--print", "native-static-libs"]);
    let output = Command::new(&cargo)
        .args(&args)
        .output()
        .expect("run cargo rustc for the staticlib");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "build the staticlib:\n{stderr}");
    let library = library_dir.join("liborrery_unreal_interiors.a");
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

    let symbols = Command::new("nm")
        .args(["--defined-only"])
        .arg(&library)
        .output()
        .expect("nm reads the archive's symbols");
    let symbols = String::from_utf8_lossy(&symbols.stdout);
    for required in [
        "orrery_host_abi_version",
        "orrery_host_snapshot",
        "orrery_host_restore",
        "orrery_host_install_state",
        "orrery_host_step",
        "orrery_host_drain_state_hashes",
        "orrery_interiors_host_create",
        "orrery_interiors_scene_len",
        "orrery_interiors_scene_state",
    ] {
        assert!(
            symbols
                .lines()
                .any(|line| line.ends_with(&format!(" T {required}"))),
            "{required} is a defined text symbol in the staticlib"
        );
    }
    // The prong this is built on: no App, no runtime.
    for absent in ["bevy_app", "bevy_time", "lightyear", "iroh", "tokio"] {
        assert!(
            !symbols
                .lines()
                .any(|line| line.contains(&format!("{}{absent}", absent.len()))),
            "the archive carries no symbol from `{absent}`"
        );
    }
    native
}

fn include_dirs(manifest_dir: &Path) -> [PathBuf; 3] {
    [
        manifest_dir.join("include"),
        manifest_dir.join("examples/c"),
        manifest_dir
            .parent()
            .expect("crates dir")
            .join("orrery_sim_host/include"),
    ]
}

fn compile_c_consumer(
    manifest_dir: &Path,
    library_dir: &Path,
    native_libs: &[String],
    executable: &Path,
) {
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let mut command = Command::new(compiler);
    command.args(["-std=c11", "-Wall", "-Wextra", "-Werror"]);
    for dir in include_dirs(manifest_dir) {
        command.arg("-I").arg(dir);
    }
    let status = command
        .arg(manifest_dir.join("examples/c/interiors_consumer.c"))
        .arg(library_dir.join("liborrery_unreal_interiors.a"))
        .args(native_libs)
        .arg("-o")
        .arg(executable)
        .status()
        .expect("run the C compiler");
    assert!(status.success(), "the C consumer compiles and links");
}

/// The Unreal module is C++20 and includes the same header; a C-only
/// construct would fail there, at engine build time, on a shared box.
fn check_shared_header_as_cxx(manifest_dir: &Path) {
    let compiler = env::var_os("CXX").unwrap_or_else(|| "c++".into());
    let mut command = Command::new(compiler);
    command.args([
        "-std=c++20",
        "-x",
        "c++",
        "-fsyntax-only",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wno-unused-function",
    ]);
    for dir in include_dirs(manifest_dir) {
        command.arg("-I").arg(dir);
    }
    let status = command
        .arg(manifest_dir.join("examples/c/interiors_shared.h"))
        .status()
        .expect("run the C++ compiler");
    assert!(status.success(), "interiors_shared.h compiles as C++20");
}

fn run(args: &[&str]) -> Output {
    Command::new(&consumer().executable)
        .args(args)
        .output()
        .expect("run the C consumer")
}

fn field(stdout: &str, key: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} in output:\n{stdout}"))
        .to_owned()
}

#[test]
fn the_c_consumer_walks_the_rolling_ship_and_the_avatar_stays_in_ship_coordinates() {
    let output = run(&["smoke"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "smoke:\n{stdout}");
    assert_eq!(field(&stdout, "input_dropped"), "0");
    assert_eq!(field(&stdout, "snapshot_failed"), "0");
    // After 400 ticks: the ship undocked at 300 and has been cruising 99
    // ticks at 500 m/s rolling; the avatar boarded at 250 and is 150 ticks
    // (6 m) up the corridor, in frame 2, with frame-local numbers that never
    // saw the 500 m/s.
    let avatar = stdout
        .lines()
        .find(|line| line.starts_with("avatar "))
        .expect("avatar line");
    assert!(avatar.contains("frame=2"), "{avatar}");
    assert!(avatar.contains("pos=(0,6000,0)"), "{avatar}");
    assert!(avatar.contains("changes=1"), "{avatar}");
    let ship = stdout
        .lines()
        .find(|line| line.starts_with("ship "))
        .expect("ship line");
    assert!(ship.contains("frame=0"), "{ship}");
    assert!(ship.contains("vel=(500000,0,0)"), "{ship}");
}

#[test]
fn a_trace_is_deterministic_run_to_run() {
    let a = run(&["trace", "mech", "3000"]);
    let b = run(&["trace", "mech", "3000"]);
    let a = String::from_utf8_lossy(&a.stdout);
    let b = String::from_utf8_lossy(&b.stdout);
    assert_eq!(field(&a, "chain"), field(&b, "chain"));
}

#[test]
fn rollback_across_every_frame_change_is_hash_exact_against_the_authority() {
    // Four cycles of the transitions scene: 24 frame changes, one arranged
    // correction each, depths and shapes spread; plus the control
    // corrections away from any change.
    let output = run(&["rollback", "transitions", "2400"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rollback exit {:?}:\n{stdout}",
        output.status.code()
    );
    assert_eq!(field(&stdout, "transitions"), "24");
    assert_eq!(field(&stdout, "mismatch_window"), "0", "{stdout}");
    assert_eq!(field(&stdout, "mismatch_after"), "0", "{stdout}");
    assert_eq!(field(&stdout, "snap"), "0", "{stdout}");
    assert_eq!(field(&stdout, "restore_failed"), "0");
    assert_eq!(field(&stdout, "replay_step_failed"), "0");
    assert_eq!(field(&stdout, "lockstep_mismatch"), "0");
}

#[test]
fn the_mech_scene_rolls_back_across_mount_and_dismount() {
    let output = run(&["rollback", "mech", "2400", "--control-every", "0"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    // board, undock, mount, dismount (at 1800).
    assert_eq!(field(&stdout, "transitions"), "4");
    assert_eq!(field(&stdout, "mismatch_window"), "0", "{stdout}");
    assert_eq!(field(&stdout, "mismatch_after"), "0", "{stdout}");
}
