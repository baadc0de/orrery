//! Startup surviving a telemetry path the process cannot write, and saying so.
//!
//! `RegolithSkinPlugin::build` used to `panic!` when the telemetry file could
//! not be opened. That runs during plugin registration, before any UI exists,
//! so a volunteer whose artifact directory blocked the *create* — a ZIP preview
//! path, an elevated install directory, a Controlled Folder Access denial —
//! got a raw Rust panic and a dead process instead of the dialog #766
//! produces. Same root cause, one step earlier, strictly worse (#772).
//!
//! An unwritable telemetry file is degradable: a game that cannot record can
//! still be played. What the player is owed is being told — and told the part
//! she cares about, which is that nothing is being banked (#773, #769).
//!
//! Its own test binary, and no `set_current_dir` or environment mutation
//! anywhere in it: every path here is passed explicitly, so these two tests
//! are safe to run in parallel with each other and with everything else.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use orrery_predict::OrreryPredictPlugin;
use orrery_regolith_client::{ActiveSession, RegolithSkinPlugin, SessionNotices};

/// A directory the process genuinely cannot create files in.
///
/// Mode bits do not restrain uid 0, so a probe confirms the denial rather than
/// assuming it: a run that can still write cannot observe this property at all
/// and says so instead of reporting green.
struct ReadOnlyDir(tempfile::TempDir);

impl ReadOnlyDir {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut permissions = std::fs::metadata(directory.path())
            .expect("the directory exists")
            .permissions();
        permissions.set_mode(0o555);
        std::fs::set_permissions(directory.path(), permissions).expect("drop the write bit");

        let probe = directory.path().join("write-probe");
        let writable = std::fs::write(&probe, b"probe").is_ok();
        let _ = std::fs::remove_file(&probe);
        assert!(
            !writable,
            "this process can still create files in a mode-0555 directory ({}), \
             so it cannot observe the unwritable telemetry path these tests exist for — \
             run the suite as an unprivileged user",
            directory.path().display()
        );
        Self(directory)
    }

    /// A telemetry path inside the unwritable directory, which therefore can
    /// be neither created nor appended to.
    fn telemetry_path(&self) -> PathBuf {
        self.0.path().join("session.jsonl")
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::metadata(self.0.path()) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755);
            let _ = std::fs::set_permissions(self.0.path(), permissions);
        }
    }
}

/// The client's non-graphics composition over `telemetry_path`, exactly as
/// `--smoke-test` assembles it.
fn compose(telemetry_path: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(bevy::state::app::StatesPlugin)
        .add_plugins(OrreryPredictPlugin::default())
        .add_plugins(RegolithSkinPlugin::new(telemetry_path.to_path_buf()));
    app.finish();
    app
}

/// #772. The guarded stage is startup surviving an unwritable telemetry path.
#[test]
fn a_client_that_cannot_record_still_reaches_its_ui() {
    let unwritable = ReadOnlyDir::new();
    let app = compose(&unwritable.telemetry_path());

    assert!(
        app.world().contains_resource::<ActiveSession>(),
        "the client did not finish composing over an unwritable telemetry path at {}, \
         so a volunteer whose folder blocks the create gets no game at all",
        unwritable.path().display()
    );
    assert!(
        !unwritable.telemetry_path().exists(),
        "the test did not actually deny the write it is asserting about"
    );
}

/// #773 and #769. The guarded stage is the player learning that nothing she
/// flies now is being recorded — and therefore that nothing is being banked.
///
/// Told at plugin build, on the banner, for the whole session. The condition
/// is one directory's writability: the telemetry stream, the campaign banking
/// record and the upload state all live there, so a directory that refuses the
/// stream refuses the record too. Detecting it here rather than at `AppExit`
/// is the whole point — at exit there is no UI left to tell anyone anything.
#[test]
fn a_client_that_cannot_record_tells_the_player_nothing_is_being_banked() {
    let unwritable = ReadOnlyDir::new();
    let app = compose(&unwritable.telemetry_path());

    let notices = app
        .world()
        .get_resource::<SessionNotices>()
        .expect("the skin installs its notices");
    let lines = notices.lines();
    assert!(
        !lines.is_empty(),
        "a session that records nothing told the player nothing"
    );
    let notice = lines.join("\n");
    assert!(
        notice.contains("NOT BEING RECORDED"),
        "{notice:?} does not say the session is unrecorded"
    );
    assert!(
        notice.contains("NOTHING YOU FLY NOW WILL BE SAVED"),
        "{notice:?} does not say the consequence a volunteer cares about: \
         that her time is not being saved"
    );

    // And a writable path says nothing, so the notice means something.
    let writable = tempfile::tempdir().expect("temporary directory");
    let quiet = compose(&writable.path().join("session.jsonl"));
    assert!(
        quiet
            .world()
            .get_resource::<SessionNotices>()
            .expect("the skin installs its notices")
            .lines()
            .is_empty(),
        "a session that is recording normally warned the player anyway"
    );
}
