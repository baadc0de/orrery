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
//! Everything the client writes therefore resolves to one per-user
//! application-data directory, chosen by platform convention, with
//! `--telemetry-jsonl` (or `ORRERY_TELEMETRY_JSONL`) still overriding it. The
//! join artifact and the upload-retry state continue to live beside whatever
//! that resolves to: one directory for both is the property that made the
//! `--telemetry-jsonl` workaround work on the night this was found.
//!
//! File *contents* are unchanged by any of this. Only their location moves.

use std::ffi::OsString;
use std::path::PathBuf;

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
/// built from — a stripped service account, say. Callers that must write
/// somewhere use [`data_dir`], which falls back to the temporary directory
/// rather than to a relative path.
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

/// The directory this launch writes its artifacts into.
///
/// **The current working directory**, by owner decision (2026-09-02): a
/// volunteer trusts files that appear where they launched the game, and can
/// find them to send back. A per-user application-data path is invisible to
/// most people and turns "send me the log" into a support conversation.
///
/// The trade is documented rather than engineered around. If the client is
/// launched from a place the process may not write — from inside the ZIP via
/// Explorer's read-only temp, or extracted into `Program Files` — the join
/// artifact cannot be saved and the volunteer is stopped at the door. The
/// answer is `PLAYTEST.md`'s instruction to extract first, plus
/// `--telemetry-jsonl` (or `ORRERY_TELEMETRY_JSONL`) to point everything
/// somewhere writable.
///
/// [`data_dir_from`] still resolves the platform conventions; it is kept
/// because `--telemetry-jsonl` users and any future opt-in need it, and
/// because a Windows answer must stay testable from a Linux host.
#[must_use]
pub fn data_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

    /// The launch directory must never appear in a resolved default: that is
    /// the whole of #766. A relative path is exactly how a read-only launch
    /// directory becomes an unwritable artifact directory.
    #[test]
    fn no_default_artifact_path_is_relative_to_the_launch_directory() {
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
