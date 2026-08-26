//! Offline invite-code minting and offline session-token signing.
//!
//! This binary is where campaign invites are minted (#387): on the operator's
//! own machine, against a local hash-only ledger, with no network, no minting
//! service and no webpage. `mint` allocates the account, the invite code and
//! the **pre-minted campaign session id** — the UUIDv7 a banked human hour
//! carries as `identity.human_session_id`, unique under this ledger.
//!
//! `session-token` signs a `SessionTokenClaimsV1` for one session, still
//! offline, against the D41 issuer credential. It is a separate step from
//! `mint` because `MAX_SESSION_TOKEN_TTL_MS` is one hour: a token signed at
//! invite time would be expired before the session it was minted for, so the
//! operator signs it shortly before hosting and the host verifies it at join
//! (#345 §8).

use clap::{Parser, Subcommand};
use orrery_identity::{
    load_runtime_credential, mint_invite, InviteLedger, IssuerKeyring, OsInviteCodeGenerator,
};
use orrery_protocol::{
    AccountId, CampaignJoinFileV1, NodeId, SessionStanding, SessionTokenClaimsV1,
    SessionTokenTtlMs, TokenClock as _, MAX_SESSION_TOKEN_TTL_MS,
};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Offline campaign identity material: invites and session tokens.
#[derive(Debug, Parser)]
#[command(name = "orrery-invite")]
struct Arguments {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Mint one invite code into a local hash-only ledger.
    Mint {
        /// Local tab-separated invite ledger; created if it does not exist.
        #[arg(long)]
        ledger: PathBuf,
        /// Operator's volunteer label, stored alongside the allocated account id.
        #[arg(long)]
        label: String,
    },
    /// Sign one session token, offline, against the issuer credential.
    SessionToken {
        /// Plain runtime credential from `orrery-issuer-key generate`/`load`;
        /// must be owner-readable only and outside every repository.
        #[arg(long)]
        issuer_credential: PathBuf,
        /// The account the invite allocated (`mint` printed it).
        #[arg(long)]
        account: u64,
        /// The transport identity the token authorizes — for a campaign
        /// external slot, the persistent key the client prints
        /// (`orrery-regolith --print-slot-key <n>`).
        #[arg(long)]
        node: String,
        /// Requested lifetime; the protocol caps it at one hour.
        #[arg(long, default_value_t = MAX_SESSION_TOKEN_TTL_MS)]
        ttl_ms: u64,
        /// Write a complete, named-field join file instead of requiring the
        /// volunteer to transcribe its launch material.
        #[arg(long)]
        join_file: Option<PathBuf>,
        /// Hosting process NodeId included in `--join-file`.
        #[arg(long)]
        host_node: Option<String>,
        /// Client slot included in `--join-file`; the token is independently
        /// bound to the persistent transport identity supplied by `--node`.
        #[arg(long)]
        slot: Option<usize>,
        /// Pre-minted session ID included in `--join-file`.
        #[arg(long)]
        session_id: Option<String>,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Arguments::parse().operation {
        Operation::Mint { ledger, label } => {
            let now = orrery_identity::SystemClock.now_ms();
            let minted = InviteLedger::update_locked(&ledger, |ledger| {
                mint_invite(ledger, label, &mut OsInviteCodeGenerator, now)
            })?;
            println!("account={}", minted.account.0);
            println!("invite_code={}", minted.code);
            println!("session_id={}", minted.session);
        }
        Operation::SessionToken {
            issuer_credential,
            account,
            node,
            ttl_ms,
            join_file,
            host_node,
            slot,
            session_id,
        } => {
            let key = load_runtime_credential(&issuer_credential)?;
            let node = NodeId::from_str(&node)?;
            let keyring = IssuerKeyring::new(key);
            let claims = SessionTokenClaimsV1::new(
                AccountId(account),
                node,
                orrery_identity::SystemClock.now_ms(),
                SessionTokenTtlMs(ttl_ms.min(MAX_SESSION_TOKEN_TTL_MS)),
                SessionStanding::Good,
                keyring.active_key_id(),
                // A shakedown volunteer is inside D33 clause (d)'s probation
                // window by construction; the closed direction is the truth.
                true,
            );
            let token = keyring.sign(claims)?;
            let token_hex = hex(&token.encode()?);
            if let Some(path) = join_file {
                let host_node = host_node.ok_or("--join-file needs --host-node <NodeId>")?;
                let slot = slot.ok_or("--join-file needs --slot <n>")?;
                let session_id = session_id.ok_or("--join-file needs --session-id <UUID>")?;
                write_join_file(
                    &path,
                    &CampaignJoinFileV1::new(host_node, slot, session_id, token_hex),
                )?;
                println!("join_file={}", path.display());
            } else {
                // Compatibility path: existing automation uses this argv-ready
                // value. Prefer --join-file or --session-token @path for new
                // sessions, because argv exposes the token to shell history and ps.
                println!("session_token={token_hex}");
            }
            println!("issuer_key_id={}", keyring.active_key_id().0);
            println!(
                "issuer_public_key={}",
                keyring.published_keys()[0].public_key
            );
        }
    }
    Ok(())
}

/// Write one new owner-readable join file without replacing an existing secret.
fn write_join_file(
    path: &Path,
    join: &CampaignJoinFileV1,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(join.to_json()?.as_bytes())?;
    Ok(())
}

/// Lowercase hex, so the token can travel through a flag or an env var.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{hex, write_join_file};
    use orrery_identity::{generate_issuer_key, IssuerKeyring};
    use orrery_protocol::{
        AccountId, CampaignJoinFileV1, FixedTokenClock, IssuerKeyId, SessionStanding,
        SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenVerificationError,
        SessionTokenVerifier, UnixMillis,
    };

    #[test]
    fn signed_token_join_file_round_trips_through_the_shared_client_format() {
        let keyring = IssuerKeyring::new(generate_issuer_key(IssuerKeyId::new(47)));
        let client_node = iroh_base::SecretKey::from_bytes(&[7; 32]).public();
        let token = keyring
            .sign(SessionTokenClaimsV1::new(
                AccountId::new(8),
                client_node,
                UnixMillis::new(1_700_000_000_000),
                SessionTokenTtlMs(60_000),
                SessionStanding::Good,
                keyring.active_key_id(),
                true,
            ))
            .expect("sign token");
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("volunteer.join.json");
        let expected = CampaignJoinFileV1::new(
            "host-node".to_owned(),
            3,
            "018f0000-0000-7000-8000-000000000047".to_owned(),
            hex(&token.encode().expect("encode token")),
        );

        write_join_file(&path, &expected).expect("emit join file");
        let consumed = CampaignJoinFileV1::from_json(
            &std::fs::read_to_string(&path).expect("read emitted join file"),
        )
        .expect("client join format accepts emitted file");
        assert_eq!(consumed, expected);
        let verifier = SessionTokenVerifier::new(
            FixedTokenClock::new(UnixMillis::new(1_700_000_000_001)),
            keyring.published_keys(),
        );
        assert_eq!(
            consumed.session_token,
            hex(&token.encode().expect("encode signed token")),
            "the emitted file preserves the signed token bytes"
        );
        let encoded = token.encode().expect("encode signed token");
        assert!(verifier.verify(&encoded, &client_node).is_ok());
        assert_eq!(
            verifier.verify(
                &encoded,
                &iroh_base::SecretKey::from_bytes(&[8; 32]).public()
            ),
            Err(SessionTokenVerificationError::WrongNode),
            "the join file carries the same token; it cannot authorize another transport key"
        );
    }
}
