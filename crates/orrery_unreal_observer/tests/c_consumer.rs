//! A real C caller links the archive and renders a real stream — and the
//! archive is proven to carry neither an ECS nor a network stack.
//!
//! Same shape as `crates/orrery_sim_host/tests/c_consumer.rs` and spike
//! #1043's: Linux-only by `cfg`, so on any other target it compiles to
//! nothing rather than failing on `nm`, `.a` naming and a Unix-only C
//! program. The profile defaults to `debug` so `cargo test --workspace` pays
//! one crate's debug build.
//!
//! # Why the sidecar is not here
//!
//! The stream this test serves is produced by a plain `TcpListener` and the
//! real codec, not by `orrery_sidecar`. That is not convenience: this crate's
//! claim is that an engine linking it gets a socket and a codec and no
//! simulation, and a dev-dependency on the sidecar would put Bevy, lightyear
//! and iroh into the graph that the archive-purity assertion below is written
//! to be read against. The shipped sidecar's own `tests/observer_kill.rs`
//! drives a real one against a real observer process; this file's job is the
//! *C boundary*.

#![cfg(target_os = "linux")]

use core::net::SocketAddr;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use orrery_ipc::{EntityFrame, FrameBatch, QuantizedTransform, SidecarToEngine};
use orrery_ipc_transport::FrameWriter;
use orrery_protocol::{InterpBasis, LatticePoint, PersistId, QuantizedDir, Tick, UNorm16};

/// The predicted capsule's id, and the interpolated one's.
const LOCAL: PersistId = PersistId::new(1);
const REMOTE: PersistId = PersistId::new(2);

fn consumer() -> &'static PathBuf {
    static CONSUMER: OnceLock<PathBuf> = OnceLock::new();
    CONSUMER.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_dir = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("crate has workspace parent");
        // `CARGO_TARGET_TMPDIR` rather than `/tmp`, and a fixed name wiped on
        // the way in rather than a PID-stamped one: #1087 found 95 leaked
        // directories and 9.4 GB from the second arrangement.
        let work_dir =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("orrery-unreal-observer-c-consumer");
        if work_dir.exists() {
            fs::remove_dir_all(&work_dir).expect("clear the previous C consumer directory");
        }
        fs::create_dir_all(&work_dir).expect("create C consumer directory");

        let profile = env::var("ORRERY_UNREAL_OBSERVER_PROFILE").unwrap_or_else(|_| "debug".into());
        let library_dir = env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| workspace_dir.join("target"), PathBuf::from)
            .join(&profile);
        let native = build_staticlib(&library_dir, &profile);
        let executable = work_dir.join("observer_consumer");
        compile(&manifest_dir, &library_dir, &native, &executable);
        executable
    })
}

/// Build the archive, check the header's promises, and check what the archive
/// does *not* contain.
fn build_staticlib(library_dir: &Path, profile: &str) -> Vec<String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut args = vec![
        "rustc",
        "-p",
        "orrery_unreal_observer",
        "--lib",
        "--crate-type",
        "staticlib",
    ];
    if profile == "release" {
        args.push("--release");
    }
    args.extend(["--", "--print", "native-static-libs"]);
    // The link line is parsed out of this stderr, so colour must be off: with
    // `CARGO_TERM_COLOR: always` the last token parses as `-lc\x1b[0m` and the
    // C driver reports `cannot find -lc` with the escape invisible.
    let output = Command::new(&cargo)
        .args(&args)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("run cargo rustc for the staticlib");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "build the staticlib:\n{stderr}");

    let library = library_dir.join("liborrery_unreal_observer.a");
    assert!(library.is_file(), "staticlib at {}", library.display());

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

    for required in [
        "orrery_observer_abi_version",
        "orrery_observer_entity_size",
        "orrery_observer_connect",
        "orrery_observer_poll",
        "orrery_observer_snapshot",
        "orrery_observer_destroy",
    ] {
        assert!(
            symbols
                .lines()
                .any(|line| line.ends_with(&format!(" T {required}"))),
            "{required} is a defined text symbol in the staticlib"
        );
    }

    // The observer's claim, checked rather than asserted in prose: an engine
    // linking this gets a socket reader and a codec, not a simulation. Rust
    // mangling carries the crate name — legacy spells it `_ZN<len><crate>`,
    // v0 `_R…<len><crate>` — so `nm` can see the absence.
    for absent in [
        "bevy_ecs",
        "bevy_app",
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
            "the observer archive carries no symbol from `{absent}`; found {hits:?}"
        );
    }
    native
}

fn compile(manifest_dir: &Path, library_dir: &Path, native: &[String], executable: &Path) {
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let status = Command::new(compiler)
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(manifest_dir.join("include"))
        .arg(manifest_dir.join("examples/c/observer_consumer.c"))
        .arg(library_dir.join("liborrery_unreal_observer.a"))
        .args(native)
        .arg("-o")
        .arg(executable)
        .status()
        .expect("run the C compiler");
    assert!(status.success(), "the C consumer compiles and links");
}

/// One batch: a predicted capsule that moves, an interpolated one on a real
/// bracket. The same shape `orrery::ipc::export_ipc_frames` emits.
fn batch(tick: u64) -> Vec<u8> {
    let axis = |x: i64| QuantizedTransform {
        translation: LatticePoint::new(x, 0, 0),
        forward: QuantizedDir::new(1, 0, 0),
        up: QuantizedDir::new(0, 1, 0),
    };
    SidecarToEngine::Frames(FrameBatch {
        extracted_at: Tick::new(tick),
        predicted: vec![EntityFrame {
            persist_id: LOCAL,
            transform: axis(i64::try_from(tick).expect("tick fits")),
            basis: InterpBasis::exact(Tick::new(tick)),
        }],
        interpolated: vec![EntityFrame {
            persist_id: REMOTE,
            transform: axis(500),
            basis: InterpBasis {
                from: Tick::new(tick - 3),
                to: Tick::new(tick),
                alpha: UNorm16(16_384),
            },
        }],
    })
    .encode()
    .expect("batch encodes")
}

/// A listener that serves `batches` frames to one dialler and then closes.
fn serving(batches: u64) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
    let addr = listener.local_addr().expect("bound address");
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = FrameWriter::new(stream);
        for tick in 100..(100 + batches) {
            if writer.write_frame(&batch(tick)).is_err() || writer.flush().is_err() {
                return;
            }
            std::thread::sleep(core::time::Duration::from_millis(2));
        }
    });
    addr
}

/// The whole C boundary, end to end: the archive links, the header agrees
/// with it, and a C program renders both timeline classes off a real socket.
#[test]
fn a_c_program_renders_both_classes_off_a_real_link() {
    let addr = serving(200);
    let mut child = Command::new(consumer())
        .arg(addr.to_string())
        .arg("400")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the C consumer runs");

    let stdout = child.stdout.take().expect("piped stdout");
    let lines: Vec<String> = BufReader::new(stdout)
        .lines()
        .map(|line| line.expect("stdout is readable"))
        .collect();
    let status = child.wait().expect("the C consumer is reaped");

    let joined = lines.join("\n");
    assert!(status.success(), "the C consumer exits cleanly:\n{joined}");
    assert!(
        lines.iter().any(|line| line.contains("class=predicted")),
        "a predicted capsule reached C:\n{joined}"
    );
    assert!(
        lines.iter().any(|line| line.contains("class=interpolated")),
        "an interpolated capsule reached C:\n{joined}"
    );
    // The bracket survives the crossing rather than collapsing to an exact
    // tick — a renderer that lost it could not build a later hit claim.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("class=interpolated") && line.contains("@16384")),
        "the interpolated capsule kept its alpha across the ABI:\n{joined}"
    );
    assert!(
        lines.iter().any(|line| line.contains("link ended with 2")),
        "a sidecar closing the stream reaches C as LINK_CLOSED, not as a crash:\n{joined}"
    );
}
