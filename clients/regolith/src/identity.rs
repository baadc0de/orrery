//! Persistent client transport identity used by campaign admission and evidence.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use iroh_base::SecretKey;

const IDENTITY_FILE: &str = "transport-identity.key";

/// Resolve the persistent identity path as flag, environment, then the native
/// per-user data directory.
///
/// `ORRERY_IDENTITY_FILE` exists for packaged launches and test isolation;
/// `--identity-file` wins when both are present.
#[must_use]
pub fn resolve_identity_path(args: &[OsString], environment: Option<OsString>) -> PathBuf {
    args.windows(2)
        .find(|pair| pair[0] == "--identity-file")
        .map(|pair| PathBuf::from(&pair[1]))
        .or_else(|| environment.map(PathBuf::from))
        .unwrap_or_else(default_identity_path)
}

/// Load the durable client identity, generating it exactly once when absent.
///
/// On Unix the key is created and retained as mode `0600`. Existing symlinks
/// and non-files are refused so a launch cannot be redirected into disclosing
/// or overwriting another path.
pub fn load_or_create(path: &Path) -> io::Result<SecretKey> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("identity path {} is not a regular file", path.display()),
                ));
            }
            restrict_permissions(path)?;
            read_key(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_key(path),
        Err(error) => Err(error),
    }
}

fn default_identity_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(root) = std::env::var_os("APPDATA") {
        return PathBuf::from(root)
            .join("Orrery")
            .join("Regolith")
            .join(IDENTITY_FILE);
    }

    #[cfg(target_os = "macos")]
    if let Some(root) = std::env::var_os("HOME") {
        return PathBuf::from(root)
            .join("Library")
            .join("Application Support")
            .join("Orrery")
            .join("Regolith")
            .join(IDENTITY_FILE);
    }

    #[cfg(not(target_os = "windows"))]
    if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(root)
            .join("orrery")
            .join("regolith")
            .join(IDENTITY_FILE);
    }

    #[cfg(not(target_os = "windows"))]
    if let Some(root) = std::env::var_os("HOME") {
        return PathBuf::from(root)
            .join(".local")
            .join("share")
            .join("orrery")
            .join("regolith")
            .join(IDENTITY_FILE);
    }

    PathBuf::from(IDENTITY_FILE)
}

fn create_key(path: &Path) -> io::Result<SecretKey> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let key = SecretKey::generate();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(hex::encode(key.to_bytes()).as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            restrict_permissions(path)?;
            Ok(key)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => load_or_create(path),
        Err(error) => Err(error),
    }
}

fn read_key(path: &Path) -> io::Result<SecretKey> {
    let text = std::fs::read_to_string(path)?;
    let bytes = hex::decode(text.trim()).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("identity key is not lowercase hex: {error}"),
        )
    })?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "identity key must contain exactly 32 bytes",
        )
    })?;
    Ok(SecretKey::from_bytes(&bytes))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_generated_once_and_reused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("identity.key");
        let first = load_or_create(&path).expect("generate identity");
        let second = load_or_create(&path).expect("reload identity");
        assert_eq!(first.public(), second.public());
        assert_eq!(std::fs::read_to_string(&path).expect("key file").len(), 65);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn explicit_identity_path_wins_over_environment() {
        let args = [
            OsString::from("regolith"),
            OsString::from("--identity-file"),
            OsString::from("flag.key"),
        ];
        assert_eq!(
            resolve_identity_path(&args, Some(OsString::from("environment.key"))),
            PathBuf::from("flag.key")
        );
    }
}
