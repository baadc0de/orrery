//! Offline invite-code minting; this binary has no network or issuer key input.

use clap::Parser;
use orrery_identity::{mint_invite, InviteLedger, OsInviteCodeGenerator};
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
    let minted = InviteLedger::update_locked(&arguments.ledger, |ledger| {
        mint_invite(ledger, arguments.label, &mut OsInviteCodeGenerator)
    })?;
    println!("account={}", minted.account.0);
    println!("invite_code={}", minted.code);
    Ok(())
}
