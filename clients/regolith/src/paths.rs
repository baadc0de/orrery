//! Where the shipped client keeps the files it writes.
//!
//! A released binary has no `target/` and no reason to invent one: `target/`
//! is Cargo's build-output convention, meaningful inside a checkout and
//! meaningless beside a downloaded `.exe`. Defaulting the writable artifacts
//! to `target/regolith-client/` *relative to the current working directory*
//! meant the client wrote wherever it happened to be launched from, and on
//! Windows that is routinely a place the process may not write: run from
//! inside the ZIP (Explorer's read-only temp), extracted into `Program
//! Files`, or under Controlled Folder Access guarding `Downloads` and
//! `Documents`. The join artifact is written beside the telemetry stream, so
//! an unwritable launch directory did not degrade telemetry — it stopped a
//! volunteer at the door with `Could not save the join file: Access is denied
//! (os error 5)` (#766).
//!
//! The first fix (#766) resolved everything to one per-user application-data
//! directory chosen by platform convention. That solved writability but hid
//! the files: a per-user application-data path is invisible to most people
//! and turns "send me the log" into a support conversation. An owner
//! decision of 2026-09-02 therefore moved the default to the *current
//! working directory*, on the reasoning that a volunteer trusts files that
//! appear where she launched the game and can find them to send back. The
//! reasoning was right; the mechanism was not. On macOS a Finder double-click
//! runs the binary with the working directory set to the user's HOME, not to
//! the folder holding it (#942): a volunteer's `session.jsonl` landed in
//! `/Users/<user>/` while he looked in the extracted release folder and found
//! only the four shipped files.
//!
//! The owner decision of 2026-09-02 that supersedes it is this: **artifacts
//! are written to the directory containing the executable, on every
//! operating system.** The executable's own directory is where the volunteer
//! is already looking, on every platform — and it is the one location that
//! needs no explanation of how the process was started.
//!
//! Everything the client writes therefore resolves to the executable's
//! directory, with `--telemetry-jsonl` (or `ORRERY_TELEMETRY_JSONL`) still
//! overriding it. The join artifact and the upload-retry state continue to
//! live beside whatever that resolves to: one directory for both is the
//! property that made the `--telemetry-jsonl` workaround work on the night
//! this was found.
//!
//! The executable's directory can be unwritable — extracted into `Program
//! Files`, a read-only mount, a quarantined macOS app — and that is
//! documented rather than engineered around: the stream refuses to open and
//! the scope banner says so for the whole session. Nothing is silently
//! redirected to a directory the volunteer will never think to look in,
//! because a session that records nothing must say so (#769).
//!
//! File *contents* are unchanged by any of this. Only their location moves.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The telemetry stream's file name inside the data directory.
pub const TELEMETRY_FILE: &str = "session.jsonl";

/// The composition smoke stream's file name inside the data directory.
pub const SMOKE_FILE: &str = "smoke.jsonl";

/// Which platform's per-user data-directory convention to resolve against.
///
/// Named rather than `cfg!`-ed at every use so all three conventions are
/// testable from one host: the bug this module exists for was a Windows bug
/// found by a Windows volunteer, and nothing that only runs on Linux can
/// assert the Windows answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// `%LOCALAPPDATA%\Orrery\Regolith\`, falling back to `%APPDATA%`.
    Windows,
    /// `~/Library/Application Support/Orrery/Regolith/`.
    MacOs,
    /// `$XDG_DATA_HOME/orrery/regolith/`, else `~/.local/share/orrery/regolith/`.
    Unix,
}

impl Platform {
    /// The convention this build's target platform follows.
    #[must_use]
    pub const fn host() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Unix
        }
    }
}

/// The environment values a data directory is resolved from.
///
/// Taken as a value rather than read inline so resolution is a pure function
/// of `(environment, platform)` and can be asserted for all three platforms
/// without mutating the process environment out from under a parallel test.
#[derive(Debug, Default, Clone)]
pub struct DataDirEnv {
    /// `%LOCALAPPDATA%`.
    pub local_app_data: Option<OsString>,
    /// `%APPDATA%`, the roaming profile, used only when `%LOCALAPPDATA%` is absent.
    pub app_data: Option<OsString>,
    /// `$XDG_DATA_HOME`.
    pub xdg_data_home: Option<OsString>,
    /// `$HOME`.
    pub home: Option<OsString>,
}

impl DataDirEnv {
    /// Read the four variables resolution consults from this process.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            local_app_data: std::env::var_os("LOCALAPPDATA"),
            app_data: std::env::var_os("APPDATA"),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            home: std::env::var_os("HOME"),
        }
    }
}

/// The per-user application-data directory for `platform`, when `environment`
/// names one.
///
/// `None` means the environment said nothing this platform's convention can be
/// built from — a stripped service account, say. Nothing resolves against this
/// by default any more: since #766 it has been superseded twice as the
/// artifact location (see the module notes), but it is kept because
/// `--telemetry-jsonl` users and any future opt-in need it, and because a
/// Windows answer must stay testable from a Linux host.
#[must_use]
pub fn data_dir_from(environment: &DataDirEnv, platform: Platform) -> Option<PathBuf> {
    match platform {
        Platform::Windows => environment
            .local_app_data
            .as_ref()
            .or(environment.app_data.as_ref())
            .map(|root| PathBuf::from(root).join("Orrery").join("Regolith")),
        Platform::MacOs => environment.home.as_ref().map(|root| {
            PathBuf::from(root)
                .join("Library")
                .join("Application Support")
                .join("Orrery")
                .join("Regolith")
        }),
        Platform::Unix => environment
            .xdg_data_home
            .as_ref()
            .map(|root| PathBuf::from(root).join("orrery").join("regolith"))
            .or_else(|| {
                environment.home.as_ref().map(|root| {
                    PathBuf::from(root)
                        .join(".local")
                        .join("share")
                        .join("orrery")
                        .join("regolith")
                })
            }),
    }
}

/// The directory containing `exe`, when one can be named.
///
/// The pure half of [`data_dir`]: taken as a value rather than read inline so
/// the executable-directory answer is assertable without moving the running
/// binary, mirroring how [`data_dir_from`] keeps the per-user conventions
/// testable per platform.
#[must_use]
pub fn data_dir_from_exe(exe: Option<&Path>) -> Option<PathBuf> {
    exe.and_then(Path::parent).map(Path::to_path_buf)
}

/// The directory this launch writes its artifacts into.
///
/// **The directory containing the running executable**, by owner decision
/// (2026-09-02, superseding the same-day decision that named the current
/// working directory): a volunteer trusts files that appear where she is
/// looking and can find them to send back. `cwd` was the wrong mechanism for
/// that intent — after a macOS Finder double-click the working directory is
/// the user's HOME, so the session log landed in `/Users/<user>/` while the
/// volunteer looked in the extracted release folder she had just unarchived
/// (#942). The executable's own directory is beside the files she was
/// already shown, on every platform.
///
/// The trade is documented rather than engineered around. The executable's
/// directory can be a place the process may not write — run from inside the
/// ZIP via Explorer's read-only temp, extracted into `Program Files`, a
/// read-only mount, a quarantined macOS app — and then the artifacts cannot
/// be saved. Nothing is silently redirected to a directory the volunteer
/// will never think to look in: the stream refuses to open and the client
/// says so for the whole session, because a session that records nothing
/// must say so (#769). The escape hatches are `PLAYTEST.md`'s instruction to
/// extract somewhere you own, plus `--telemetry-jsonl` (or
/// `ORRERY_TELEMETRY_JSONL`) to point everything somewhere writable.
///
/// If the process cannot name its own executable at all — essentially
/// unreachable — resolution falls back to the working directory as a last
/// resort, where an unwritable result surfaces as the recording-unavailable
/// notice at startup rather than a panic.
///
/// [`data_dir_from`] still resolves the platform conventions; it is kept
/// because `--telemetry-jsonl` users and any future opt-in need it, and
/// because a Windows answer must stay testable from a Linux host.
#[must_use]
pub fn data_dir() -> PathBuf {
    data_dir_from_exe(std::env::current_exe().ok().as_deref()).unwrap_or_else(|| PathBuf::from("."))
}

/// The default telemetry stream, which the join artifact and the upload-retry
/// state are written beside.
#[must_use]
pub fn default_telemetry_path() -> PathBuf {
    data_dir().join(TELEMETRY_FILE)
}

/// The default stream for `--smoke-test`'s non-rendering composition.
#[must_use]
pub fn default_smoke_path() -> PathBuf {
    data_dir().join(SMOKE_FILE)
}

/// Resolve the telemetry path as flag, environment, then the per-user data
/// directory.
///
/// `--telemetry-jsonl` is the documented override and wins over everything;
/// `ORRERY_TELEMETRY_JSONL` exists for packaged launches and test isolation,
/// mirroring `ORRERY_IDENTITY_FILE`.
#[must_use]
pub fn resolve_telemetry_path(args: &[OsString], environment: Option<OsString>) -> PathBuf {
    args.windows(2)
        .find(|pair| pair[0] == "--telemetry-jsonl")
        .map(|pair| PathBuf::from(&pair[1]))
        .or_else(|| environment.map(PathBuf::from))
        .unwrap_or_else(default_telemetry_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn environment() -> DataDirEnv {
        DataDirEnv {
            local_app_data: Some(OsString::from(r"C:\Users\vol\AppData\Local")),
            app_data: Some(OsString::from(r"C:\Users\vol\AppData\Roaming")),
            xdg_data_home: Some(OsString::from("/home/vol/.data")),
            home: Some(OsString::from("/home/vol")),
        }
    }

    #[test]
    fn each_platform_resolves_its_own_per_user_convention() {
        assert_eq!(
            data_dir_from(&environment(), Platform::Windows),
            Some(
                PathBuf::from(r"C:\Users\vol\AppData\Local")
                    .join("Orrery")
                    .join("Regolith")
            )
        );
        assert_eq!(
            data_dir_from(&environment(), Platform::MacOs),
            Some(
                Path::new("/home/vol")
                    .join("Library")
                    .join("Application Support")
                    .join("Orrery")
                    .join("Regolith")
            )
        );
        assert_eq!(
            data_dir_from(&environment(), Platform::Unix),
            Some(Path::new("/home/vol/.data").join("orrery").join("regolith"))
        );
    }

    #[test]
    fn windows_falls_back_to_the_roaming_profile_and_unix_to_the_home_default() {
        let mut without_local = environment();
        without_local.local_app_data = None;
        assert_eq!(
            data_dir_from(&without_local, Platform::Windows),
            Some(
                PathBuf::from(r"C:\Users\vol\AppData\Roaming")
                    .join("Orrery")
                    .join("Regolith")
            )
        );

        let mut without_xdg = environment();
        without_xdg.xdg_data_home = None;
        assert_eq!(
            data_dir_from(&without_xdg, Platform::Unix),
            Some(
                Path::new("/home/vol")
                    .join(".local")
                    .join("share")
                    .join("orrery")
                    .join("regolith")
            )
        );
    }

    #[test]
    fn an_empty_environment_names_no_data_directory() {
        let empty = DataDirEnv::default();
        for platform in [Platform::Windows, Platform::MacOs, Platform::Unix] {
            assert_eq!(data_dir_from(&empty, platform), None);
        }
    }

    /// No default artifact path may be relative. A relative path is exactly
    /// how a read-only launch directory once became an unwritable artifact
    /// directory (#766), and it is still how the degenerate no-executable
    /// fallback would reintroduce the hazard if it ever fired.
    #[test]
    fn no_default_artifact_path_is_relative() {
        for path in [default_telemetry_path(), default_smoke_path(), data_dir()] {
            assert!(
                path.is_absolute(),
                "{} is relative, so it resolves against wherever the volunteer launched",
                path.display()
            );
            assert!(
                !path.starts_with("target"),
                "{} is Cargo's build directory, which a shipped binary does not have",
                path.display()
            );
        }
    }

    /// The owner decision of 2026-09-02 (#942): the default is the directory
    /// holding the executable, not the working directory. A Finder
    /// double-click runs with the working directory set to HOME, so a
    /// cwd-based default put a volunteer's `session.jsonl` in her home
    /// directory while she searched the extracted release folder for it.
    #[test]
    fn the_resolved_default_sits_beside_the_executable_not_in_the_working_directory() {
        let exe = std::env::current_exe().expect("every test binary knows its own executable");
        let expected = exe
            .parent()
            .expect("the executable itself sits in a directory")
            .to_path_buf();
        assert_eq!(
            data_dir(),
            expected,
            "the default data directory must be the executable's directory"
        );

        let working = std::env::current_dir().expect("the test runs with a working directory");
        assert_ne!(
            data_dir(),
            working,
            "the default data directory must not be the working directory"
        );
    }

    /// The pure half of the resolution, per platform shape: whatever the
    /// process names as its executable, the data directory is the folder
    /// holding it — a release archive's folder, an install prefix, an app
    /// bundle's `MacOS` directory.
    #[test]
    fn the_data_directory_is_always_the_executables_own_directory() {
        assert_eq!(
            data_dir_from_exe(Some(Path::new("/opt/orrery/regolith/regolith-client"))),
            Some(PathBuf::from("/opt/orrery/regolith"))
        );
        assert_eq!(
            data_dir_from_exe(Some(Path::new(
                "/Applications/Orrery.app/Contents/MacOS/regolith"
            ))),
            Some(PathBuf::from("/Applications/Orrery.app/Contents/MacOS"))
        );
        assert_eq!(
            data_dir_from_exe(Some(Path::new("C:/Games/Regolith/regolith-client.exe"))),
            Some(PathBuf::from("C:/Games/Regolith"))
        );

        // A process that cannot name an executable names no data directory,
        // which is [`data_dir`]'s cue for its last-resort fallback.
        assert_eq!(data_dir_from_exe(None), None);
    }

    /// Precedence is unchanged by the new default: `--telemetry-jsonl` wins
    /// over everything, `ORRERY_TELEMETRY_JSONL` wins over the default, and
    /// the default — now beside the executable — is what both fall back to.
    #[test]
    fn flag_then_environment_then_the_executables_directory_in_that_order() {
        assert_eq!(
            resolve_telemetry_path(&[], None),
            default_telemetry_path(),
            "flag and environment must fall back to the executable-side default"
        );
        assert_eq!(
            resolve_telemetry_path(&[], Some(OsString::from("environment.jsonl"))),
            PathBuf::from("environment.jsonl")
        );
        let args = [
            OsString::from("regolith"),
            OsString::from("--telemetry-jsonl"),
            OsString::from("flag.jsonl"),
        ];
        assert_eq!(
            resolve_telemetry_path(&args, Some(OsString::from("environment.jsonl"))),
            PathBuf::from("flag.jsonl")
        );
    }

    #[test]
    fn explicit_telemetry_path_wins_over_environment() {
        let args = [
            OsString::from("regolith"),
            OsString::from("--telemetry-jsonl"),
            OsString::from("flag.jsonl"),
        ];
        assert_eq!(
            resolve_telemetry_path(&args, Some(OsString::from("environment.jsonl"))),
            PathBuf::from("flag.jsonl")
        );
        assert_eq!(
            resolve_telemetry_path(&[], Some(OsString::from("environment.jsonl"))),
            PathBuf::from("environment.jsonl")
        );
    }
}
