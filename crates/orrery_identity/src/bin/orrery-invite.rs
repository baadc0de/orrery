//! Offline invite-code minting; this binary has no network or issuer key input.
//!
//! The three printed facts travel to different places (see `invite`'s module
//! docs): the code to the volunteer, the session identity to the hosting
//! harness and the client, the account id to the operator's records.

use clap::Parser;
use orrery_identity::{mint_invite, InviteLedger, OsInviteCodeGenerator};
use orrery_protocol::UnixMillis;
use std::path::PathBuf;

/// Mint one operator-issued invite code into a local hash-only ledger.
#[derive(Debug, Parser)]
struct Arguments {
    /// Local tab-separated invite ledger; created if it does not exist.
    #[arg(long)]
    ledger: PathBuf,
    /// Operator's volunteer label, stored alongside the allocated account id.
    #[arg(long)]
    label: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    let now_ms = UnixMillis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0),
    );
    let minted = InviteLedger::update_locked(&arguments.ledger, |ledger| {
        // The thread RNG is the same entropy class the code generator uses
        // (OS-seeded); it is drawn separately so the two draws cannot be
        // confused with the deterministic code stream.
        mint_invite(
            ledger,
            arguments.label.clone(),
            &mut OsInviteCodeGenerator,
            now_ms,
            &mut rand::rng(),
        )
    })?;
    println!("account={}", minted.account.0);
    println!("invite_code={}", minted.code);
    println!("session_id={}", minted.session_id);
    Ok(())
}
