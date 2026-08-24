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
    AccountId, NodeId, SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, TokenClock as _,
    MAX_SESSION_TOKEN_TTL_MS,
};
use std::path::PathBuf;
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
        /// external slot, the slot key the client prints
        /// (`orrery-regolith --print-slot-key <n>`).
        #[arg(long)]
        node: String,
        /// Requested lifetime; the protocol caps it at one hour.
        #[arg(long, default_value_t = MAX_SESSION_TOKEN_TTL_MS)]
        ttl_ms: u64,
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
            println!("issuer_key_id={}", keyring.active_key_id().0);
            println!(
                "issuer_public_key={}",
                keyring.published_keys()[0].public_key
            );
            println!("session_token={}", hex(&token.encode()?));
        }
    }
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
