//! What a launch directory the client cannot write to costs a volunteer.
//!
//! When the default artifact directory was the current working directory,
//! this was fatal (#766): the join artifact was written beside a telemetry
//! path resolved against wherever she launched from, and the admission reply
//! turned into `Could not save the join file: Access is denied (os error 5)`.
//! The 2026-09-02 owner decision moved the default to the directory holding
//! the executable (#942) — after a Finder double-click the working directory
//! is HOME, so a cwd-based default had put a volunteer's `session.jsonl` in
//! her home folder while she searched the extracted release for it — and
//! that changes what this file guards.
//!
//! This is its own test binary on purpose. It makes the process's working
//! directory read-only, and that is process-global: run beside the unit tests
//! it would change the ground under a parallel thread. One test, one process,
//! no sharing.
//!
//! Two properties are guarded here, both about **the client telling the truth
//! about where things are written**:
//!
//! 1. A read-only *launch* directory no longer costs the volunteer anything:
//!    the default lands beside the executable — the extracted release folder
//!    she is already looking in — so the join write succeeds from a launch
//!    directory she cannot write to. Under the cwd-based default this exact
//!    sequence refused the join, which is how this file earned its keep.
//! 2. An unwritable *artifact* directory (the executable's own directory, in
//!    the field: Program Files, a read-only mount, a quarantined macOS app)
//!    is reported through `open_or_unavailable`, never silently degraded into
//!    a stream that keeps nothing while claiming to record — a session that
//!    records nothing must say so (#769). The documented escape hatch,
//!    pointing `--telemetry-jsonl` (or the environment variable) somewhere
//!    writable, is proven to still rescue it.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use orrery_regolith_client::admission::{write_join_artifact, JoinObject};
use orrery_regolith_client::paths::resolve_telemetry_path;
use orrery_regolith_client::telemetry::JsonlTelemetry;

/// Drop `mode` bits and confirm the process really cannot create a file there.
///
/// Mode bits do not restrain uid 0, so a run that can still write is not a
/// pass with a different explanation — it is a run that cannot observe the
/// property at all, and it says so instead of reporting green.
fn make_unwritable(directory: &Path) {
    let mut permissions = std::fs::metadata(directory)
        .expect("the launch directory exists")
        .permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(directory, permissions).expect("drop the write bit");

    let probe = directory.join("write-probe");
    let writable = std::fs::write(&probe, b"probe").is_ok();
    let _ = std::fs::remove_file(&probe);
    assert!(
        !writable,
        "this process can still create files in a mode-0555 directory ({}), \
         so it cannot observe the read-only launch this test exists for — \
         run the suite as an unprivileged user",
        directory.display()
    );
}

fn restore_write_bit(directory: &Path) {
    let mut permissions = std::fs::metadata(directory)
        .expect("the directory still exists")
        .permissions();
    permissions.set_mode(0o755);
    let _ = std::fs::set_permissions(directory, permissions);
}

fn a_join_object() -> JoinObject {
    JoinObject {
        host_node: "9f".repeat(32),
        slot: 3,
        session_id: "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".to_owned(),
        session_token: "ab".repeat(48),
    }
}

#[test]
fn a_join_from_a_read_only_launch_directory_still_saves_its_join_file() {
    let launch = tempfile::tempdir().expect("launch directory");
    let application_data = tempfile::tempdir().expect("application data directory");

    // The volunteer's situation under the old default: the process's working
    // directory is the read-only place she launched from — inside the ZIP, or
    // Program Files, or a Controlled-Folder-Access-guarded Downloads.
    std::env::set_current_dir(launch.path()).expect("launch from the read-only directory");
    make_unwritable(launch.path());

    // The resolver takes flag and environment as values, so the process
    // environment is untouched: the default resolves beside the executable
    // and nothing else can move it.
    let telemetry_path = resolve_telemetry_path(&[], None);
    let exe_directory = std::env::current_exe()
        .expect("the test binary knows its own executable")
        .parent()
        .expect("the executable sits in a directory")
        .to_path_buf();
    assert_eq!(
        telemetry_path.parent().map(Path::to_path_buf),
        Some(exe_directory.clone()),
        "the default must sit beside the executable, not in the read-only \
         launch directory"
    );

    // The owner decision of 2026-09-02 in action: the read-only launch
    // directory costs her nothing. The join file goes beside the executable,
    // which is the extracted release folder she is already looking in.
    let join = a_join_object();
    let written = write_join_artifact(&telemetry_path, &join)
        .expect("a read-only launch directory must not stop the join any more");

    // Where it landed, and that the launch directory was left alone.
    assert!(
        written.starts_with(&exe_directory),
        "the join artifact went to {} instead of beside the executable at {}",
        written.display(),
        exe_directory.display()
    );
    assert!(
        !written.starts_with(launch.path()),
        "the join artifact was written under the read-only launch directory"
    );

    // The contents are unchanged by the move: still the strict named-field
    // join file, keyed by the session it grants.
    assert_eq!(
        written.file_name().and_then(|name| name.to_str()),
        Some("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.join.json")
    );
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&written).expect("the saved join file is readable"),
    )
    .expect("the saved join file is JSON");
    assert_eq!(saved["host_node"], join.host_node);
    assert_eq!(saved["slot"], 3);
    assert_eq!(saved["session_id"], join.session_id);
    assert_eq!(saved["session_token"], join.session_token);
    std::fs::remove_file(&written).expect("remove the join file written beside the executable");

    // The escape hatch PLAYTEST.md points at: name a writable stream and
    // everything — telemetry, join artifact, retry state — follows it there.
    // This is also what rescues an unwritable *executable* directory, the
    // field case this test cannot stage by moving its own binary.
    let redirected = resolve_telemetry_path(
        &[],
        Some(
            application_data
                .path()
                .join("session.jsonl")
                .into_os_string(),
        ),
    );
    let rescued = write_join_artifact(&redirected, &join)
        .expect("--telemetry-jsonl must rescue an unwritable artifact directory");
    assert!(
        rescued.starts_with(application_data.path()),
        "the join artifact went to {} instead of the redirected directory {}",
        rescued.display(),
        application_data.path().display()
    );

    // Restore the write bit so the temporary directory can be removed.
    restore_write_bit(launch.path());
    std::env::set_current_dir(application_data.path()).expect("leave the launch directory");
}

/// The honest-degradation half of the owner decision: when the resolved
/// artifact directory cannot be written — the executable's own directory, in
/// the field — the stream reports itself unavailable. It must never answer
/// with a sink that keeps nothing while the session looks recorded, and it
/// must never silently relocate to a directory the volunteer will never
/// think to look in (#769).
#[test]
fn an_unwritable_artifact_directory_is_reported_not_silently_dropped() {
    let recording = tempfile::tempdir().expect("artifact directory");
    make_unwritable(recording.path());

    let (sink, unavailable) =
        JsonlTelemetry::open_or_unavailable(&recording.path().join("session.jsonl"));
    assert!(
        !sink.is_recording(),
        "a stream that could not open must not pretend to be recording"
    );
    let detail = unavailable.expect("an unwritable artifact directory must be reported");
    assert!(
        detail.contains(recording.path().to_str().expect("temp paths are utf-8")),
        "the player-visible detail {detail:?} must name the directory that failed"
    );

    restore_write_bit(recording.path());
}
