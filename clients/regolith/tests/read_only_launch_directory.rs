//! A volunteer who launched from a directory she could not write to could not
//! join at all (#766): the join artifact was written beside a telemetry path
//! that defaulted to `target/regolith-client/session.jsonl` *relative to the
//! current working directory*, and the admission reply turned into
//! `Could not save the join file: Access is denied (os error 5)`.
//!
//! This is its own test binary on purpose. It makes the process's working
//! directory read-only and points the data-directory environment somewhere
//! writable, and both of those are process-global: run beside the unit tests
//! they would change the ground under a parallel thread. One test, one
//! process, no sharing.
//!
//! The guarded stage is **the client writing where it is allowed to write**,
//! so the assertion is that the join write *succeeds* against a genuinely
//! unwritable launch directory — not that the resolved path is spelled a
//! particular way. A spelling test would not have caught this: the string was
//! always exactly what its author intended.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use orrery_regolith_client::admission::{write_join_artifact, JoinObject};
use orrery_regolith_client::paths::resolve_telemetry_path;

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

#[test]
fn a_join_from_a_read_only_launch_directory_still_saves_its_join_file() {
    let launch = tempfile::tempdir().expect("launch directory");
    let application_data = tempfile::tempdir().expect("application data directory");

    // The volunteer's situation: the process's working directory is the
    // read-only place she launched from — inside the ZIP, or Program Files,
    // or a Controlled-Folder-Access-guarded Downloads.
    std::env::set_current_dir(launch.path()).expect("launch from the read-only directory");
    make_unwritable(launch.path());

    std::env::set_var("XDG_DATA_HOME", application_data.path());
    std::env::set_var("HOME", application_data.path());
    std::env::remove_var("ORRERY_TELEMETRY_JSONL");

    let telemetry_path = resolve_telemetry_path(&[], None);

    let join = JoinObject {
        host_node: "9f".repeat(32),
        slot: 3,
        session_id: "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".to_owned(),
        session_token: "ab".repeat(48),
    };

    // The owner's decision of 2026-09-02: artifacts go to the working
    // directory, because a volunteer trusts files that appear where she
    // launched the game and can find them to send back. The read-only launch
    // is the documented cost of that, not a bug to engineer around.
    //
    // What this test now guards is that the cost stays *bounded*: the failure
    // is a plain I/O error the dialog can show, not a panic and not a silent
    // half-write, and the documented escape hatch actually works.
    let refused = write_join_artifact(&telemetry_path, &join);
    assert!(
        refused.is_err(),
        "a read-only launch directory must refuse, so the dialog can tell her to extract first"
    );

    // The escape hatch PLAYTEST.md points at: name a writable stream and
    // everything — telemetry, join artifact, retry state — follows it there.
    let redirected = resolve_telemetry_path(
        &[],
        Some(
            application_data
                .path()
                .join("session.jsonl")
                .into_os_string(),
        ),
    );
    let written = write_join_artifact(&redirected, &join)
        .expect("--telemetry-jsonl must rescue a read-only launch directory");

    // Where it landed, and that the launch directory was left alone.
    assert!(
        written.starts_with(application_data.path()),
        "the join artifact went to {} instead of the per-user data directory {}",
        written.display(),
        application_data.path().display()
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

    // Restore the write bit so the temporary directory can be removed.
    let mut permissions = std::fs::metadata(launch.path())
        .expect("the launch directory still exists")
        .permissions();
    permissions.set_mode(0o755);
    let _ = std::fs::set_permissions(launch.path(), permissions);
    std::env::set_current_dir(application_data.path()).expect("leave the launch directory");
}
