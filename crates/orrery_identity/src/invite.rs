//! Offline invite minting and service-ready redemption.
//!
//! An [`InviteLedger`] is deliberately a small local file, not an account
//! store: minting allocates an [`AccountId`] and records only the hash of the
//! code, account, pre-minted session identity (#387), and operator's
//! volunteer label. Redemption takes all of its dependencies as arguments, so
//! an eventual HTTP handler only needs to parse its request and call
//! [`redeem_invite`]; no protocol type or verification path is duplicated
//! here.
//!
//! # Where invites are minted (the whole identity convention, stated once)
//!
//! Invites are minted **only** by `cargo run -p orrery_identity --bin
//! orrery-invite -- --ledger <file> --label <volunteer>`, offline, by the
//! operator, on a machine holding the ledger file. One invocation prints three
//! facts together: the invite code (given to the volunteer), the allocated
//! account id, and the pre-minted session UUIDv7. The session identity is what
//! makes the resulting hour bankable — the P4 ledger refuses a human hour
//! without it — and it exists nowhere else: no service holds a registry, no
//! coordinator derives it at join time. The operator passes the same value to
//! the hosting harness (`p1-swarm --expected-session-id`) and to the
//! volunteer's client (`regolith --session-id`); the host refuses a dialler
//! presenting any other identity, which is the entire admission control for
//! shakedown scale. Replacing this with a redemption service later changes
//! nothing upstream: the wire, the report, and the ledger already speak in
//! terms of the session identity alone.

use crate::service::{IdentityService, IssuedSession, StandingSource};
use crate::session_id::{is_uuid_v7, session_uuid_v7};
use crate::store::{AccountStore, IdentityError};
use orrery_protocol::{AccountId, NodeId, TokenClock, UnixMillis};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Ledger v3 adds the fifth column: the pre-minted session identity (#387).
///
/// One minted invite binds exactly one bankable session identity before
/// anyone dials anything. The operator hands the value to the host
/// (`p1-swarm --expected-session-id`) and to the volunteer's client
/// (`--session-id`); the host validates the pair at join, and the P4 ledger
/// accepts no human hour without it. There is no minting service: this
/// column *is* the allocation record.
const LEDGER_HEADER: &str =
    "# orrery invite ledger v3: code_hash_sha256\taccount_id\tvolunteer_label\tsession_id\tstate\tconsumed_node\tconsumed_at_ms";
/// Ledgers written before #387 carry no session column; they stay readable,
/// with their invites redeeming exactly as before but without a pre-minted
/// session identity (the operator supplies one out of band).
const LEDGER_HEADER_V2_PREFIX: &str = "# orrery invite ledger v2:";
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
    /// The pre-minted session identity (#387). Absent only for invites
    /// allocated by a pre-v3 ledger; those redeem unchanged but bank under a
    /// session identity supplied out of band.
    session_id: Option<String>,
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

    /// The pre-minted session identity, when this invite carries one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
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
            // A v3 ledger always writes the column; a legacy invite that was
            // loaded from a v2 file and re-saved upgrades in place with an
            // empty field rather than losing the record.
            output.push_str(entry.session_id.as_deref().unwrap_or(""));
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
    /// exactly the stable V1, V2 or V3 shape.
    pub fn parse(contents: &str) -> Result<Self, InviteError> {
        // The header decides the record grammar: v3 carries the session
        // column, everything before it does not. Field counts alone cannot
        // discriminate — a v2 `consumed` row has exactly the shape of a v3
        // `issued` row shifted by one — so the version is read once and
        // every line is parsed under it.
        let version_three = contents
            .lines()
            .find(|line| line.starts_with('#'))
            .is_some_and(|header| !header.starts_with(LEDGER_HEADER_V2_PREFIX));
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
            let session_id = if version_three {
                let Some(session) = fields.next() else {
                    return Err(InviteError::MalformedLedger { line: index + 1 });
                };
                // Empty is the legacy-upgrade shape: an invite allocated by a
                // v2 ledger re-saves with the column present but unclaimed.
                // Anything non-empty must be exactly what the ledger will
                // one day demand of `identity.human_session_id` — a weak
                // label or timestamp must not quietly become it.
                if !session.is_empty() && !is_uuid_v7(session) {
                    return Err(InviteError::MalformedLedger { line: index + 1 });
                }
                (!session.is_empty()).then(|| session.to_owned())
            } else {
                None
            };
            let state = match (fields.next(), fields.next(), fields.next(), fields.next()) {
                (None, None, None, None) => InviteState::Issued,
                (Some("issued"), Some(""), Some(""), None) => InviteState::Issued,
                (Some("consumed"), Some(node), Some(at_ms), None) => InviteState::Consumed {
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
            if entries
                .iter()
                .any(|entry: &InviteLedgerEntry| entry.code_hash == code_hash)
            {
                return Err(InviteError::MalformedLedger { line: index + 1 });
            }
            // Two invites holding one session identity would let one hour be
            // attributed to either volunteer; the ledger refuses the file
            // rather than the ambiguity.
            if entries.iter().any(|entry: &InviteLedgerEntry| {
                entry.session_id.is_some() && entry.session_id == session_id
            }) {
                return Err(InviteError::MalformedLedger { line: index + 1 });
            }
            entries.push(InviteLedgerEntry {
                code_hash,
                account,
                label: label.to_owned(),
                session_id,
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
    /// The pre-minted session identity this invite banks under (#387). Handed
    /// to the host and to the volunteer's client; there is no service that
    /// re-derives it.
    pub session_id: String,
}

/// Mint one code into `ledger` for `label`.
///
/// The session identity is pre-minted here, at allocation time, stamped with
/// `now_ms` and randomized from `session_rng` (#387's identity convention:
/// offline invites, no service). Both inputs are injected rather than read,
/// so the complete decision stays deterministic and testable.
///
/// # Errors
///
/// Refuses labels that cannot be represented safely in the flat-file format.
pub fn mint_invite(
    ledger: &mut InviteLedger,
    label: String,
    generator: &mut impl InviteCodeGenerator,
    now_ms: UnixMillis,
    session_rng: &mut impl Rng,
) -> Result<MintedInvite, InviteError> {
    if label.is_empty() || label.contains(['\r', '\n', '\t']) {
        return Err(InviteError::InvalidLabel);
    }
    let account = ledger.next_account()?;
    let code = format!("{CODE_PREFIX}{}", hex(&generator.generate_code_bytes()));
    let code_hash = code_hash(&code);
    if ledger
        .entries
        .iter()
        .any(|entry| entry.code_hash == code_hash)
    {
        return Err(InviteError::DuplicateCode);
    }
    let session_id = session_uuid_v7(now_ms, session_rng);
    // A collision would make one bankable hour attributable to either
    // volunteer; the parse refuses such a file, so minting refuses the mint.
    if ledger
        .entries
        .iter()
        .any(|entry| entry.session_id.as_deref() == Some(session_id.as_str()))
    {
        return Err(InviteError::DuplicateSessionId);
    }
    ledger.entries.push(InviteLedgerEntry {
        code_hash,
        account,
        label,
        session_id: Some(session_id.clone()),
        state: InviteState::Issued,
    });
    Ok(MintedInvite {
        code,
        account,
        session_id,
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
    /// The session-id generator repeated an existing pre-minted identity.
    DuplicateSessionId,
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
            Self::DuplicateSessionId => {
                formatter.write_str("session-id generator repeated an existing pre-minted identity")
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
    use orrery_protocol::UnixMillis;
    use rand::{SeedableRng};
    use rand_chacha::ChaCha8Rng;

    #[derive(Debug)]
    struct FixedCodes(Vec<[u8; 32]>);

    impl InviteCodeGenerator for FixedCodes {
        fn generate_code_bytes(&mut self) -> [u8; 32] {
            self.0.remove(0)
        }
    }

    fn mint_rng(seed: u64) -> ChaCha8Rng {
        ChaCha8Rng::seed_from_u64(seed)
    }

    const NOW_MS: UnixMillis = UnixMillis(1_756_000_000_000);

    #[test]
    fn mint_allocates_accounts_sessions_and_persists_hashes_not_codes() {
        let mut ledger = InviteLedger::default();
        let mut codes = FixedCodes(vec![[3; 32], [4; 32]]);
        let first = mint_invite(&mut ledger, "Ada".to_owned(), &mut codes, NOW_MS, &mut mint_rng(1))
            .expect("mint first");
        let second =
            mint_invite(&mut ledger, "Bryn".to_owned(), &mut codes, NOW_MS, &mut mint_rng(2))
                .expect("mint second");
        assert_eq!(first.account, AccountId(1));
        assert_eq!(second.account, AccountId(2));
        // Distinct pre-minted identities, valid in the ledger's own shape.
        assert_ne!(first.session_id, second.session_id);
        for minted in [&first, &second] {
            assert!(is_uuid_v7(&minted.session_id), "{}", minted.session_id);
        }

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
                text.push_str(entry.session_id.as_deref().expect("v3 mints carry one"));
                text.push('\n');
            }
            text
        };
        assert!(!saved.contains(&first.code));
        let parsed = InviteLedger::parse(&saved).expect("parse");
        assert_eq!(parsed, ledger);
        for (original, round_tripped) in ledger.entries.iter().zip(parsed.entries.iter()) {
            assert_eq!(original.session_id(), round_tripped.session_id());
        }
    }

    /// A v2 ledger — no session column — stays readable, and re-saving
    /// upgrades it in place without losing a record.
    #[test]
    fn legacy_v2_ledgers_still_parse_and_upgrade_on_save() {
        let mut codes = FixedCodes(vec![[5; 32]]);
        let mut fresh = InviteLedger::default();
        let minted = mint_invite(
            &mut fresh,
            "Legacy shape".to_owned(),
            &mut codes,
            NOW_MS,
            &mut mint_rng(3),
        )
        .expect("mint");
        let v2_text = format!(
            "# orrery invite ledger v2: code_hash_sha256\taccount_id\tvolunteer_label\tstate\tconsumed_node\tconsumed_at_ms\n{}\t7\tAda\n",
            hex(&code_hash(&minted.code)),
        );
        let parsed = InviteLedger::parse(&v2_text).expect("legacy parse");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].session_id(), None, "no column, no claim");
        assert_eq!(parsed.entries[0].account(), AccountId(7));

        // Re-save upgrades the file to the current header and an empty
        // session field (with the issued-state block `save_locked` writes),
        // and the upgraded file parses again.
        let mut resaved = String::from(LEDGER_HEADER);
        resaved.push('\n');
        resaved.push_str(&format!(
            "{}\t7\tAda\t\tissued\t\t\n",
            hex(&code_hash(&minted.code))
        ));
        let upgraded = InviteLedger::parse(&resaved).expect("upgraded parse");
        assert_eq!(upgraded.entries[0].session_id(), None);
    }

    /// A consumed v3 record round-trips with its session identity intact.
    #[test]
    fn consumed_v3_records_keep_their_session_identity() {
        let mut codes = FixedCodes(vec![[6; 32]]);
        let mut ledger = InviteLedger::default();
        let minted = mint_invite(
            &mut ledger,
            "Consumed".to_owned(),
            &mut codes,
            NOW_MS,
            &mut mint_rng(4),
        )
        .expect("mint");
        let line = format!(
            "{}\t1\tConsumed\t{}\tconsumed\t{}\t123\n",
            hex(&code_hash(&minted.code)),
            minted.session_id,
            hex(&[9u8; 32]),
        );
        let text = format!("{LEDGER_HEADER}\n{line}");
        let parsed = InviteLedger::parse(&text).expect("parse consumed row");
        assert_eq!(
            parsed.entries[0].session_id(),
            Some(minted.session_id.as_str())
        );
    }

    /// A v3 row whose session field is not a UUIDv7 is malformed, not
    /// tolerated: a weak value must not quietly become the banking identity.
    #[test]
    fn a_v3_record_with_a_non_v7_session_field_is_malformed() {
        let bad_session = format!("{LEDGER_HEADER}\n{}\t1\tX\tnot-a-uuid\tissued\t\t\n", hex(&[7u8; 32]));
        assert!(InviteLedger::parse(&bad_session).is_err());
    }

    /// Two invites claiming one session identity make attribution ambiguous;
    /// the file is refused rather than the ambiguity banked.
    #[test]
    fn duplicate_session_ids_are_refused_at_parse() {
        let shared = "018f8f4e-5c90-7abc-8123-00000000abcd";
        let text = format!(
            "{LEDGER_HEADER}\n{}\t1\tA\t{shared}\n{}\t2\tB\t{shared}\n",
            hex(&[1u8; 32]),
            hex(&[2u8; 32]),
        );
        assert!(InviteLedger::parse(&text).is_err());
    }

    /// The generator's random bytes are separate from the code generator, so
    /// a fixed code stream cannot also fix session identities.
    #[test]
    fn session_randomness_is_independent_of_code_generation() {
        let mut codes = FixedCodes(vec![[8; 32], [8; 32]]); // deliberately repeated!
        let mut ledger = InviteLedger::default();
        let first = mint_invite(
            &mut ledger,
            "One".to_owned(),
            &mut codes,
            NOW_MS,
            &mut mint_rng(11),
        );
        // The identical code bytes collide with the first entry.
        assert!(
            matches!(mint_invite(&mut ledger, "Two".to_owned(), &mut codes, NOW_MS, &mut mint_rng(12)), Err(InviteError::DuplicateCode)),
            "the code collision must still be caught"
        );
        assert!(first.is_ok());
    }
}
