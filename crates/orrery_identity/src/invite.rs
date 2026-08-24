//! Offline invite minting and service-ready redemption.
//!
//! An [`InviteLedger`] is deliberately a small local file, not an account
//! store: minting allocates an [`AccountId`] and records only the hash of the
//! code, account, and operator's volunteer label. Redemption takes all of its
//! dependencies as arguments, so an eventual HTTP handler only needs to parse
//! its request and call [`redeem_invite`]; no protocol type or verification
//! path is duplicated here.
//!
//! # Where invites are minted (#387 — the identity convention, no service)
//!
//! Invites are minted **on the operator's own machine, offline**, by the
//! `orrery-invite` binary in this crate — there is no minting service, no
//! webpage, and nothing listening on a socket. The mint allocates three
//! things into the operator's local hash-only ledger:
//!
//!   * the invite code (never stored; only its hash is),
//!   * the [`AccountId`], and
//!   * a **pre-minted campaign session id** — a UUIDv7 satisfying
//!     `scripts/p4-ledger.sh`'s `identity.human_session_id` constraint. The
//!     ledger enforces the coordinator's uniqueness constraint offline: it
//!     refuses to mint or parse a duplicate session id, so the same id cannot
//!     be issued twice and two volunteers cannot share one.
//!
//! What is deliberately *not* pre-minted here is the session **token**:
//! [`orrery_protocol::MAX_SESSION_TOKEN_TTL_MS`] is one hour, so a token
//! signed at invite time would be expired before the session it was minted
//! for. The operator signs the token (`orrery-invite session-token`, still
//! offline, against the D41 issuer credential) shortly before hosting, and
//! the host verifies it at join (#345 §8).

use crate::service::{IdentityService, IssuedSession, StandingSource};
use crate::store::{AccountStore, IdentityError};
use orrery_protocol::{AccountId, NodeId, TokenClock, UnixMillis};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

const LEDGER_HEADER: &str =
    "# orrery invite ledger v3: code_hash_sha256\taccount_id\tvolunteer_label\tsession_id\tstate\tconsumed_node\tconsumed_at_ms";
const CODE_PREFIX: &str = "orrery-invite-v1-";

/// A source of new invite-code bytes.
pub trait InviteCodeGenerator {
    /// Produce 32 unpredictable bytes for one invite code.
    fn generate_code_bytes(&mut self) -> [u8; 32];
}

/// The operating-system-backed generator used by the offline mint CLI.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsInviteCodeGenerator;

impl InviteCodeGenerator for OsInviteCodeGenerator {
    fn generate_code_bytes(&mut self) -> [u8; 32] {
        rand::rng().random()
    }
}

/// One hashed invite allocation in the operator's offline ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteLedgerEntry {
    code_hash: [u8; 32],
    account: AccountId,
    label: String,
    /// Pre-minted campaign session id (UUIDv7). `None` on rows minted before
    /// the v3 ledger format existed; those invites predate campaign banking
    /// and cannot bank a human hour without a re-mint.
    session: Option<String>,
    state: InviteState,
}

/// The terminal state tracked for one invite allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InviteState {
    Issued,
    Consumed { node: [u8; 32], at_ms: u64 },
}

impl InviteLedgerEntry {
    /// The allocated account identity.
    #[must_use]
    pub const fn account(&self) -> AccountId {
        self.account
    }

    /// The operator's volunteer label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The pre-minted campaign session id, absent on pre-v3 rows.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }
}

/// The local, hash-only collection of invite allocations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InviteLedger {
    entries: Vec<InviteLedgerEntry>,
}

impl InviteLedger {
    /// Read an invite ledger from `path`; an absent file is an empty ledger.
    ///
    /// # Errors
    ///
    /// Returns [`InviteError::Io`] for a filesystem error and
    /// [`InviteError::MalformedLedger`] for an invalid record.
    pub fn load(path: &Path) -> Result<Self, InviteError> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse(&contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(InviteError::Io(error)),
        }
    }

    /// Atomically write the ledger to `path`. Invite codes themselves are never written.
    ///
    /// # Errors
    ///
    /// Returns [`InviteError::Io`] if the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), InviteError> {
        Self::with_lock(path, |_| self.save_locked(path))
    }

    /// Lock `path`, reload it, apply `update`, and atomically replace it.
    ///
    /// The exclusive lock covers the entire read-modify-write sequence, so two
    /// local mint or consume operations cannot lose one another's updates.
    ///
    /// # Errors
    ///
    /// Returns [`InviteError::Io`] for lock or filesystem failures, or an error
    /// returned by `update`.
    pub fn update_locked<T>(
        path: &Path,
        update: impl FnOnce(&mut Self) -> Result<T, InviteError>,
    ) -> Result<T, InviteError> {
        Self::with_lock(path, |ledger| {
            let result = update(ledger)?;
            ledger.save_locked(path)?;
            Ok(result)
        })
    }

    fn with_lock<T>(
        path: &Path,
        update: impl FnOnce(&mut Self) -> Result<T, InviteError>,
    ) -> Result<T, InviteError> {
        let lock_path = ledger_lock_path(path);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(InviteError::Io)?;
        lock.lock().map_err(InviteError::Io)?;
        let mut ledger = Self::load(path)?;
        let result = update(&mut ledger);
        lock.unlock().map_err(InviteError::Io)?;
        result
    }

    fn save_locked(&self, path: &Path) -> Result<(), InviteError> {
        let mut output = String::from(LEDGER_HEADER);
        output.push('\n');
        for entry in &self.entries {
            output.push_str(&hex(&entry.code_hash));
            output.push('\t');
            output.push_str(&entry.account.0.to_string());
            output.push('\t');
            output.push_str(&entry.label);
            output.push('\t');
            output.push_str(entry.session.as_deref().unwrap_or(""));
            output.push('\t');
            match entry.state {
                InviteState::Issued => output.push_str("issued\t\t\n"),
                InviteState::Consumed { node, at_ms } => {
                    output.push_str("consumed\t");
                    output.push_str(&hex(&node));
                    output.push('\t');
                    output.push_str(&at_ms.to_string());
                    output.push('\n');
                }
            }
        }
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        let mut temporary = NamedTempFile::new_in(directory).map_err(InviteError::Io)?;
        temporary
            .write_all(output.as_bytes())
            .map_err(InviteError::Io)?;
        temporary.as_file().sync_all().map_err(InviteError::Io)?;
        temporary
            .persist(path)
            .map_err(|error| InviteError::Io(error.error))?;
        fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(InviteError::Io)
    }

    /// Decode a ledger file from its text representation.
    ///
    /// # Errors
    ///
    /// Returns [`InviteError::MalformedLedger`] when a record does not have
    /// exactly the stable V1 or V2 shape.
    pub fn parse(contents: &str) -> Result<Self, InviteError> {
        let mut entries = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            let Some(hash) = fields.next() else {
                unreachable!("split always has one field")
            };
            let Some(account) = fields.next() else {
                return Err(InviteError::MalformedLedger { line: index + 1 });
            };
            let Some(label) = fields.next() else {
                return Err(InviteError::MalformedLedger { line: index + 1 });
            };
            // Rows have carried three shapes: v1 (three fields, always
            // issued), v2 (state directly after the label), and v3 (the
            // pre-minted session id between label and state). The session
            // column is recognised positionally by the total field count, so
            // a v2 row keeps reading exactly as it always did.
            let tail: Vec<&str> = fields.collect();
            let (session, state_fields): (Option<String>, &[&str]) = match tail.len() {
                0 | 3 => (None, tail.as_slice()),
                4 => {
                    let session = match tail[0] {
                        "" => None,
                        session if is_uuid_v7(session) => Some(session.to_owned()),
                        _ => return Err(InviteError::MalformedLedger { line: index + 1 }),
                    };
                    (session, &tail[1..])
                }
                _ => return Err(InviteError::MalformedLedger { line: index + 1 }),
            };
            let state = match state_fields {
                [] => InviteState::Issued,
                ["issued", "", ""] => InviteState::Issued,
                ["consumed", node, at_ms] => InviteState::Consumed {
                    node: decode_hash(node)
                        .ok_or(InviteError::MalformedLedger { line: index + 1 })?,
                    at_ms: at_ms
                        .parse()
                        .map_err(|_| InviteError::MalformedLedger { line: index + 1 })?,
                },
                _ => return Err(InviteError::MalformedLedger { line: index + 1 }),
            };
            if label.is_empty() || label.contains(['\r', '\n', '\t']) {
                return Err(InviteError::MalformedLedger { line: index + 1 });
            }
            let code_hash =
                decode_hash(hash).ok_or(InviteError::MalformedLedger { line: index + 1 })?;
            let account = account
                .parse::<u64>()
                .map(AccountId)
                .map_err(|_| InviteError::MalformedLedger { line: index + 1 })?;
            if entries.iter().any(|entry: &InviteLedgerEntry| {
                entry.code_hash == code_hash
                    || (entry.session.is_some() && entry.session == session)
            }) {
                return Err(InviteError::MalformedLedger { line: index + 1 });
            }
            entries.push(InviteLedgerEntry {
                code_hash,
                account,
                label: label.to_owned(),
                session,
                state,
            });
        }
        Ok(Self { entries })
    }

    fn issued_account_for_code(&self, code: &str) -> Result<AccountId, InviteError> {
        let code_hash = code_hash(code);
        self.entries
            .iter()
            .find(|entry| constant_time_eq(&entry.code_hash, &code_hash))
            .ok_or(InviteError::InvalidCode)
            .and_then(|entry| match entry.state {
                InviteState::Issued => Ok(entry.account),
                InviteState::Consumed { .. } => Err(InviteError::AlreadyConsumed),
            })
    }

    fn consume(
        &mut self,
        code: &str,
        node: &NodeId,
        at_ms: UnixMillis,
    ) -> Result<AccountId, InviteError> {
        let code_hash = code_hash(code);
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| constant_time_eq(&entry.code_hash, &code_hash))
            .ok_or(InviteError::InvalidCode)?;
        match entry.state {
            InviteState::Issued => {
                entry.state = InviteState::Consumed {
                    node: *node.as_bytes(),
                    at_ms: at_ms.0,
                };
                Ok(entry.account)
            }
            InviteState::Consumed { .. } => Err(InviteError::AlreadyConsumed),
        }
    }

    /// Allocates the next account id in this ledger.
    fn next_account(&self) -> Result<AccountId, InviteError> {
        self.entries
            .iter()
            .map(|entry| entry.account.0)
            .max()
            .map_or(Ok(AccountId(1)), |account| {
                account
                    .checked_add(1)
                    .map(AccountId)
                    .ok_or(InviteError::AccountExhausted)
            })
    }
}

/// A freshly minted code and its offline allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedInvite {
    /// The code displayed by the mint command. It is absent from [`InviteLedger`].
    pub code: String,
    /// The account allocated to the code.
    pub account: AccountId,
    /// The pre-minted campaign session id: a UUIDv7 allocated once, here,
    /// under this ledger's uniqueness constraint. It is what a banked human
    /// hour carries as `identity.human_session_id`.
    pub session: String,
}

/// Render one RFC 9562 UUIDv7 from a Unix-millisecond timestamp and 74 bits
/// of caller-supplied entropy (the top 6 bits of `entropy[0]` are discarded).
///
/// Layout: 48-bit big-endian milliseconds, then the version nibble `7` over
/// 12 bits of entropy, then the `10` variant bits over the remaining 62.
/// This is exactly the shape `scripts/p4-ledger.sh` insists on for
/// `identity.human_session_id`:
/// `^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`.
#[must_use]
pub fn uuid_v7(now_ms: u64, entropy: [u8; 10]) -> String {
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&now_ms.to_be_bytes()[2..8]);
    bytes[6] = 0x70 | (entropy[0] & 0x0f);
    bytes[7] = entropy[1];
    bytes[8] = 0x80 | (entropy[2] & 0x3f);
    bytes[9] = entropy[3];
    bytes[10..16].copy_from_slice(&entropy[4..10]);
    let hex = hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Whether `candidate` is a well-formed lowercase UUIDv7 — the same predicate
/// `scripts/p4-ledger.sh` applies to `identity.human_session_id`.
#[must_use]
pub fn is_uuid_v7(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            14 => {
                if *byte != b'7' {
                    return false;
                }
            }
            19 => {
                if !matches!(byte, b'8' | b'9' | b'a' | b'b') {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

/// Mint one code into `ledger` for `label`, pre-minting the campaign session
/// id at `now_ms` (#387's identity convention).
///
/// # Errors
///
/// Refuses labels that cannot be represented safely in the flat-file format,
/// and a generator that repeats an existing code or session id.
pub fn mint_invite(
    ledger: &mut InviteLedger,
    label: String,
    generator: &mut impl InviteCodeGenerator,
    now_ms: UnixMillis,
) -> Result<MintedInvite, InviteError> {
    if label.is_empty() || label.contains(['\r', '\n', '\t']) {
        return Err(InviteError::InvalidLabel);
    }
    let account = ledger.next_account()?;
    let code = format!("{CODE_PREFIX}{}", hex(&generator.generate_code_bytes()));
    let code_hash = code_hash(&code);
    // Fresh entropy for the session id, drawn separately from the code bytes:
    // the code is a secret and the session id is not, so deriving one from
    // the other would leak code bits into every banked row.
    let session_entropy: [u8; 10] = generator.generate_code_bytes()[..10]
        .try_into()
        .expect("ten of thirty-two bytes");
    let session = uuid_v7(now_ms.0, session_entropy);
    if ledger.entries.iter().any(|entry| {
        entry.code_hash == code_hash || entry.session.as_deref() == Some(session.as_str())
    }) {
        return Err(InviteError::DuplicateCode);
    }
    ledger.entries.push(InviteLedgerEntry {
        code_hash,
        account,
        label,
        session: Some(session.clone()),
        state: InviteState::Issued,
    });
    Ok(MintedInvite {
        code,
        account,
        session,
    })
}

/// Redeem one invite: verify its code, create and bind its account, then mint.
///
/// `at_ms` is injected rather than read here, making the complete decision
/// deterministic and ready for a thin endpoint to supply its request clock.
/// The resulting token is issued through [`IdentityService::issue`] and is
/// therefore verified by the existing protocol/coordinator verifier unchanged.
///
/// # Errors
///
/// Returns [`InviteRedemptionError::InvalidCode`] before any account-store
/// mutation when the code is absent or wrong. Store and issuance failures are
/// propagated without being softened.
pub async fn redeem_invite<S, T, C>(
    ledger_path: &Path,
    code: &str,
    node: &NodeId,
    at_ms: UnixMillis,
    identity: &IdentityService<S, T, C>,
    requested_ttl_ms: Option<u64>,
) -> Result<IssuedSession, InviteRedemptionError>
where
    S: AccountStore,
    T: StandingSource,
    C: TokenClock,
{
    let account = InviteLedger::update_locked(ledger_path, |ledger| {
        ledger.issued_account_for_code(code)?;
        // This commits the single-use transition before the account store is
        // touched. A local ledger cannot atomically commit with AccountStore:
        // a later store failure leaves this code consumed. Nor is this global:
        // independent operator ledgers can still collide on AccountId and only
        // enforce single-use for the ledger they share.
        ledger.consume(code, node, at_ms)
    })
    .map_err(redemption_from_invite_error)?;
    if identity.store().account(account).await?.is_none() {
        identity.store().create_account(account, at_ms.0).await?;
    }
    identity.store().bind(account, node, at_ms.0).await?;
    identity
        .issue(account, node, requested_ttl_ms)
        .await
        .map_err(InviteRedemptionError::Identity)
}

/// Errors from minting or parsing an invite ledger.
#[derive(Debug)]
#[non_exhaustive]
pub enum InviteError {
    /// The label cannot safely occupy one tab-separated field.
    InvalidLabel,
    /// The account id sequence cannot advance further.
    AccountExhausted,
    /// A deterministic generator repeated an existing code.
    DuplicateCode,
    /// No invite record matches the presented code.
    InvalidCode,
    /// The invite was already consumed by an earlier redemption.
    AlreadyConsumed,
    /// A ledger record is not valid V1 input.
    MalformedLedger {
        /// One-based source line.
        line: usize,
    },
    /// Local ledger I/O failed.
    Io(std::io::Error),
}

impl fmt::Display for InviteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLabel => {
                formatter.write_str("invite label must be nonempty single-line text")
            }
            Self::AccountExhausted => formatter.write_str("invite account ids are exhausted"),
            Self::DuplicateCode => {
                formatter.write_str("invite-code generator repeated an existing code")
            }
            Self::InvalidCode => formatter.write_str("invalid invite code"),
            Self::AlreadyConsumed => formatter.write_str("invite code already consumed"),
            Self::MalformedLedger { line } => {
                write!(formatter, "malformed invite ledger line {line}")
            }
            Self::Io(error) => write!(formatter, "invite ledger I/O: {error}"),
        }
    }
}

impl std::error::Error for InviteError {}

/// Errors from redemption after a code is presented.
#[derive(Debug)]
#[non_exhaustive]
pub enum InviteRedemptionError {
    /// No invite record matches this code.
    InvalidCode,
    /// The invite was already consumed by an earlier redemption.
    AlreadyConsumed,
    /// The local invite ledger could not be read or durably updated.
    Ledger(InviteError),
    /// The durable account/binding operation failed.
    Identity(IdentityError),
}

impl fmt::Display for InviteRedemptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCode => formatter.write_str("invalid invite code"),
            Self::AlreadyConsumed => formatter.write_str("invite code already consumed"),
            Self::Ledger(error) => write!(formatter, "redeem invite ledger: {error}"),
            Self::Identity(error) => write!(formatter, "redeem invite: {error}"),
        }
    }
}

impl std::error::Error for InviteRedemptionError {}

impl From<IdentityError> for InviteRedemptionError {
    fn from(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}

fn redemption_from_invite_error(error: InviteError) -> InviteRedemptionError {
    match error {
        InviteError::InvalidCode => InviteRedemptionError::InvalidCode,
        InviteError::AlreadyConsumed => InviteRedemptionError::AlreadyConsumed,
        error => InviteRedemptionError::Ledger(error),
    }
}

fn code_hash(code: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"orrery/invite-code/v1\0");
    hasher.update(code.as_bytes());
    hasher.finalize().into()
}

fn ledger_lock_path(path: &Path) -> PathBuf {
    let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut different = 0u8;
    for (left, right) in left.iter().zip(right) {
        different |= left ^ right;
    }
    different == 0
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        hash[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(hash)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedCodes(Vec<[u8; 32]>);

    impl InviteCodeGenerator for FixedCodes {
        fn generate_code_bytes(&mut self) -> [u8; 32] {
            self.0.remove(0)
        }
    }

    const T0: u64 = 1_756_000_000_000;

    #[test]
    fn mint_allocates_accounts_and_persists_hashes_not_codes() {
        let mut ledger = InviteLedger::default();
        let mut codes = FixedCodes(vec![[3; 32], [13; 32], [4; 32], [14; 32]]);
        let first = mint_invite(
            &mut ledger,
            "Ada".to_owned(),
            &mut codes,
            UnixMillis::new(T0),
        )
        .expect("mint first");
        let second = mint_invite(
            &mut ledger,
            "Bryn".to_owned(),
            &mut codes,
            UnixMillis::new(T0),
        )
        .expect("mint second");
        assert_eq!(first.account, AccountId(1));
        assert_eq!(second.account, AccountId(2));
        assert_ne!(first.session, second.session);

        let saved = {
            let mut text = String::from(LEDGER_HEADER);
            text.push('\n');
            for entry in &ledger.entries {
                text.push_str(&hex(&entry.code_hash));
                text.push('\t');
                text.push_str(&entry.account.0.to_string());
                text.push('\t');
                text.push_str(&entry.label);
                text.push('\t');
                text.push_str(entry.session.as_deref().unwrap_or(""));
                text.push_str("\tissued\t\t\n");
            }
            text
        };
        assert!(!saved.contains(&first.code));
        assert_eq!(InviteLedger::parse(&saved).expect("parse"), ledger);
    }

    /// A pre-v3 row (no session column) still parses, carrying no session id.
    #[test]
    fn legacy_rows_without_a_session_column_still_parse() {
        let text = format!("{}\t1\tAda\n", hex(&[9u8; 32]));
        let ledger = InviteLedger::parse(&text).expect("v1 row parses");
        assert_eq!(ledger.entries[0].session_id(), None);
        let v2 = format!("{}\t2\tBryn\tissued\t\t\n", hex(&[8u8; 32]));
        let ledger = InviteLedger::parse(&v2).expect("v2 row parses");
        assert_eq!(ledger.entries[0].session_id(), None);
    }

    /// The ledger is the offline stand-in for the coordinator's unique
    /// session-id constraint: a duplicated id must refuse to parse.
    #[test]
    fn duplicate_session_ids_refuse_to_parse() {
        let session = uuid_v7(T0, [7; 10]);
        let text = format!(
            "{}\t1\tAda\t{session}\tissued\t\t\n{}\t2\tBryn\t{session}\tissued\t\t\n",
            hex(&[1u8; 32]),
            hex(&[2u8; 32]),
        );
        assert!(matches!(
            InviteLedger::parse(&text),
            Err(InviteError::MalformedLedger { line: 2 })
        ));
    }

    /// The mint refuses to allocate one session id twice: the ledger is the
    /// offline stand-in for the coordinator's unique session-id constraint.
    #[test]
    fn a_repeated_session_entropy_refuses_to_mint() {
        let mut ledger = InviteLedger::default();
        // Codes [1] and [3] differ; session entropy is [2] both times, so the
        // second mint would repeat the first session id at the same instant.
        let mut codes = FixedCodes(vec![[1; 32], [2; 32], [3; 32], [2; 32]]);
        mint_invite(
            &mut ledger,
            "Ada".to_owned(),
            &mut codes,
            UnixMillis::new(T0),
        )
        .expect("first mint");
        assert!(matches!(
            mint_invite(
                &mut ledger,
                "Bryn".to_owned(),
                &mut codes,
                UnixMillis::new(T0)
            ),
            Err(InviteError::DuplicateCode)
        ));
    }

    /// The minted id satisfies exactly the ledger script's regex shape.
    #[test]
    fn minted_session_ids_are_uuid_v7() {
        let mut ledger = InviteLedger::default();
        let mut codes = FixedCodes(vec![[5; 32], [0xff; 32]]);
        let minted = mint_invite(
            &mut ledger,
            "Ada".to_owned(),
            &mut codes,
            UnixMillis::new(T0),
        )
        .expect("mint");
        assert!(is_uuid_v7(&minted.session), "got {}", minted.session);
        // Version and variant nibbles are forced even from all-ones entropy.
        let forced = uuid_v7(T0, [0xff; 10]);
        assert!(is_uuid_v7(&forced), "got {forced}");
        assert_eq!(&forced[14..15], "7");
        assert_eq!(&forced[19..20], "b");
        // The timestamp occupies the first 48 bits, big-endian.
        assert!(forced.starts_with("0198d9c1-9800"), "got {forced}");
        // And the checker refuses near-misses.
        assert!(!is_uuid_v7(&forced.to_uppercase()));
        assert!(!is_uuid_v7(&forced.replace('-', "")));
        let v4 = format!("{}4{}", &forced[..14], &forced[15..]);
        assert!(!is_uuid_v7(&v4), "version nibble must be 7");
    }
}
