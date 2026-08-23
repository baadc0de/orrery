//! Production custody and portability for the identity issuer signing key.
//!
//! Escrows are passphrase-encrypted `age` files. `age` supplies a reviewed,
//! interoperable scrypt-based passphrase recipient, authenticated file
//! encryption, and framing; this module intentionally does not compose a KDF
//! and cipher itself. The construction assumes a strong, unique passphrase
//! held separately from the escrow. Encryption cannot rescue a guessable or
//! reused passphrase.
//!
//! Runtime credentials are deliberately plain because the current signer must
//! receive the Ed25519 secret in-process. They are created mode `0600` on Unix
//! and must be placed in a volatile, service-private location. Restrictive file
//! modes are necessary but not sufficient: host root can read the file and the
//! service's memory, so with the landed signer host-root compromise remains
//! issuer-key compromise. Secret-bearing outputs are refused inside Git work
//! trees; operators must also keep them out of container build contexts, image
//! layers, and invite-ledger backup paths, which no file writer can detect
//! after the fact.

use age::secrecy::{ExposeSecret, SecretString};
use orrery_protocol::{IssuerKeyId, NodeId};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const CREDENTIAL_MAGIC: &[u8; 16] = b"orrery-issuer-v1";
const CREDENTIAL_LEN: usize = CREDENTIAL_MAGIC.len() + 4 + 32;
const MAX_ESCROW_BYTES: u64 = 1024 * 1024;

/// A failure to generate, escrow, restore, or load an issuer key.
#[derive(Debug)]
#[non_exhaustive]
pub enum IssuerKeyLifecycleError {
    /// A filesystem operation failed.
    Io(io::Error),
    /// The escrow was malformed, corrupted, or opened with the wrong passphrase.
    Decrypt(age::DecryptError),
    /// The decrypted bytes are not a supported issuer credential.
    InvalidCredential,
    /// A secret-bearing file would be written inside a source repository.
    RepositoryPath(PathBuf),
    /// The restored public key differs from the ceremony's recorded public key.
    PublicKeyMismatch {
        /// The public key recorded when the key was generated.
        expected: NodeId,
        /// The public key derived by [`IssuerSigningKey::public_key`].
        actual: NodeId,
    },
    /// A passphrase was empty.
    EmptyPassphrase,
    /// An input credential has permissions broader than owner-only on Unix.
    InsecurePermissions(PathBuf),
    /// An input was unexpectedly large.
    OversizedInput,
}

impl fmt::Display for IssuerKeyLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "issuer-key file operation: {error}"),
            Self::Decrypt(_) => formatter.write_str(
                "decrypt issuer-key escrow: wrong passphrase, corruption, or invalid age file",
            ),
            Self::InvalidCredential => {
                formatter.write_str("invalid or unsupported issuer runtime credential")
            }
            Self::RepositoryPath(path) => write!(
                formatter,
                "refusing to write issuer-key material inside repository: {}",
                path.display()
            ),
            Self::PublicKeyMismatch { expected, actual } => write!(
                formatter,
                "restored issuer public key mismatch: expected {expected}, got {actual}"
            ),
            Self::EmptyPassphrase => formatter.write_str("issuer escrow passphrase is empty"),
            Self::InsecurePermissions(path) => write!(
                formatter,
                "issuer-key input is accessible beyond its owner: {}",
                path.display()
            ),
            Self::OversizedInput => formatter.write_str("issuer-key input exceeds its size bound"),
        }
    }
}

impl std::error::Error for IssuerKeyLifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            // Do not expose age's detailed decryption error through ordinary
            // operator output; wrong passphrases and corrupt files fail alike.
            Self::Decrypt(_) => None,
            _ => None,
        }
    }
}

impl From<io::Error> for IssuerKeyLifecycleError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Generate a fresh Ed25519 issuer key using the operating-system RNG path
/// provided by `iroh_base`.
#[must_use]
pub fn generate_issuer_key(key_id: IssuerKeyId) -> crate::IssuerSigningKey {
    crate::IssuerSigningKey::new(key_id, iroh_base::SecretKey::generate())
}

/// Write a plain runtime credential for the identity service.
///
/// The file is created without overwriting an existing path and with mode
/// `0600` on Unix. Run the loader as the service UID and place this output in a
/// volatile service-private directory so only that UID and root can reach it.
///
/// # Errors
///
/// Fails for repository paths, existing outputs, or filesystem errors.
pub fn write_runtime_credential(
    path: &Path,
    key: &crate::IssuerSigningKey,
) -> Result<(), IssuerKeyLifecycleError> {
    let plaintext = zeroize::Zeroizing::new(encode_credential(key));
    write_secret_file(path, plaintext.as_ref())
}

/// Encrypt a runtime credential into a portable `age` passphrase escrow.
///
/// # Errors
///
/// Fails closed for an empty passphrase, invalid/insecure input credential,
/// repository output path, encryption error, or filesystem error.
pub fn escrow_issuer_key(
    credential_path: &Path,
    escrow_path: &Path,
    passphrase: SecretString,
) -> Result<crate::IssuerSigningKey, IssuerKeyLifecycleError> {
    validate_passphrase(&passphrase)?;
    let key = load_runtime_credential(credential_path)?;
    let plaintext = zeroize::Zeroizing::new(encode_credential(&key));
    let encryptor = age::Encryptor::with_user_passphrase(passphrase);
    let mut encrypted = Vec::new();
    {
        let mut writer = encryptor.wrap_output(&mut encrypted)?;
        writer.write_all(plaintext.as_ref())?;
        writer.finish()?;
    }
    write_secret_file(escrow_path, &encrypted)?;
    Ok(key)
}

/// Restore an encrypted escrow and require it to reproduce the recorded public
/// key from the generation ceremony.
///
/// # Errors
///
/// Fails closed for a wrong passphrase, ciphertext corruption, invalid
/// credential, insecure input permissions, or public-key mismatch.
pub fn restore_issuer_key(
    escrow_path: &Path,
    passphrase: SecretString,
    expected_public_key: NodeId,
) -> Result<crate::IssuerSigningKey, IssuerKeyLifecycleError> {
    validate_passphrase(&passphrase)?;
    check_owner_only(escrow_path)?;
    let metadata = fs::metadata(escrow_path)?;
    if metadata.len() > MAX_ESCROW_BYTES {
        return Err(IssuerKeyLifecycleError::OversizedInput);
    }
    let encrypted = fs::read(escrow_path)?;
    let decryptor =
        age::Decryptor::new_buffered(&encrypted[..]).map_err(IssuerKeyLifecycleError::Decrypt)?;
    let identity = age::scrypt::Identity::new(passphrase);
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(IssuerKeyLifecycleError::Decrypt)?;
    let mut plaintext = zeroize::Zeroizing::new(Vec::with_capacity(CREDENTIAL_LEN));
    reader
        .by_ref()
        .take((CREDENTIAL_LEN + 1) as u64)
        .read_to_end(&mut plaintext)
        .map_err(age::DecryptError::Io)
        .map_err(IssuerKeyLifecycleError::Decrypt)?;
    let key = decode_credential(&plaintext)?;
    compare_public_key(&key, expected_public_key)?;
    Ok(key)
}

/// Decrypt an escrow into the identity service's runtime credential.
///
/// This is the boot-time handoff: run it as the identity service UID, then
/// construct the service keyring from [`load_runtime_credential`].
///
/// # Errors
///
/// Returns any restore, comparison, or secure-file creation failure.
pub fn load_issuer_key(
    escrow_path: &Path,
    runtime_path: &Path,
    passphrase: SecretString,
    expected_public_key: NodeId,
) -> Result<crate::IssuerSigningKey, IssuerKeyLifecycleError> {
    let key = restore_issuer_key(escrow_path, passphrase, expected_public_key)?;
    write_runtime_credential(runtime_path, &key)?;
    Ok(key)
}

/// Read a restrictive plain runtime credential for construction of an
/// [`crate::IssuerKeyring`] in the identity service.
///
/// # Errors
///
/// Fails when the file is accessible beyond its owner on Unix, has an invalid
/// format, or cannot be read.
pub fn load_runtime_credential(
    path: &Path,
) -> Result<crate::IssuerSigningKey, IssuerKeyLifecycleError> {
    check_owner_only(path)?;
    let bytes = zeroize::Zeroizing::new(fs::read(path)?);
    decode_credential(bytes.as_ref())
}

fn compare_public_key(
    key: &crate::IssuerSigningKey,
    expected: NodeId,
) -> Result<(), IssuerKeyLifecycleError> {
    // The comparison deliberately uses the same API printed by the CLI. It is
    // the rehearsal guard, not a second derivation of the Ed25519 public key.
    let actual = key.public_key();
    if actual != expected {
        return Err(IssuerKeyLifecycleError::PublicKeyMismatch { expected, actual });
    }
    Ok(())
}

fn validate_passphrase(passphrase: &SecretString) -> Result<(), IssuerKeyLifecycleError> {
    if passphrase.expose_secret().is_empty() {
        return Err(IssuerKeyLifecycleError::EmptyPassphrase);
    }
    Ok(())
}

fn encode_credential(key: &crate::IssuerSigningKey) -> [u8; CREDENTIAL_LEN] {
    let mut encoded = [0; CREDENTIAL_LEN];
    encoded[..CREDENTIAL_MAGIC.len()].copy_from_slice(CREDENTIAL_MAGIC);
    let id_start = CREDENTIAL_MAGIC.len();
    encoded[id_start..id_start + 4].copy_from_slice(&key.key_id().0.to_be_bytes());
    let secret = zeroize::Zeroizing::new(key.secret_bytes());
    encoded[id_start + 4..].copy_from_slice(secret.as_ref());
    encoded
}

fn decode_credential(encoded: &[u8]) -> Result<crate::IssuerSigningKey, IssuerKeyLifecycleError> {
    if encoded.len() != CREDENTIAL_LEN || &encoded[..CREDENTIAL_MAGIC.len()] != CREDENTIAL_MAGIC {
        return Err(IssuerKeyLifecycleError::InvalidCredential);
    }
    let id_start = CREDENTIAL_MAGIC.len();
    let mut id = [0; 4];
    id.copy_from_slice(&encoded[id_start..id_start + 4]);
    let mut secret = zeroize::Zeroizing::new([0; 32]);
    secret.copy_from_slice(&encoded[id_start + 4..]);
    Ok(crate::IssuerSigningKey::new(
        IssuerKeyId::new(u32::from_be_bytes(id)),
        iroh_base::SecretKey::from_bytes(&secret),
    ))
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), IssuerKeyLifecycleError> {
    refuse_repository_path(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(IssuerKeyLifecycleError::Io(error));
    }
    Ok(())
}

fn refuse_repository_path(path: &Path) -> Result<(), IssuerKeyLifecycleError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = parent.canonicalize()?;
    if canonical_parent.ancestors().any(|ancestor| {
        let marker = ancestor.join(".git");
        marker.is_file() || marker.join("HEAD").is_file()
    }) {
        return Err(IssuerKeyLifecycleError::RepositoryPath(path.to_owned()));
    }
    Ok(())
}

#[cfg(unix)]
fn check_owner_only(path: &Path) -> Result<(), IssuerKeyLifecycleError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.mode() & 0o077 != 0 {
        return Err(IssuerKeyLifecycleError::InsecurePermissions(
            path.to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_owner_only(path: &Path) -> Result<(), IssuerKeyLifecycleError> {
    if !fs::metadata(path)?.is_file() {
        return Err(IssuerKeyLifecycleError::InsecurePermissions(
            path.to_owned(),
        ));
    }
    Ok(())
}
