//! Offline generation, encrypted escrow, restore rehearsal, and boot loading
//! for the identity issuer key required by D41 clause (d).

use age::secrecy::SecretString;
use clap::{Parser, Subcommand};
use orrery_identity::{
    escrow_issuer_key, generate_issuer_key, load_issuer_key, restore_issuer_key,
    write_runtime_credential,
};
use orrery_protocol::{IssuerKeyId, NodeId};
use std::io::{self, BufRead};
use std::path::PathBuf;
use zeroize::Zeroize;

/// Give the in-process identity issuer key a portable production lifecycle.
#[derive(Debug, Parser)]
#[command(name = "orrery-issuer-key")]
struct Arguments {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Generate a fresh key and a restrictive plain staging credential.
    Generate {
        /// Rotation identifier stamped into tokens signed by this key.
        #[arg(long)]
        key_id: u32,
        /// New staging credential; must be outside every repository.
        #[arg(long)]
        output: PathBuf,
    },
    /// Encrypt a staging credential into a portable age escrow.
    Escrow {
        /// Restrictive plain credential produced by `generate` or `load`.
        #[arg(long)]
        credential: PathBuf,
        /// New encrypted age file; must be outside every repository.
        #[arg(long)]
        output: PathBuf,
        /// Read one newline-terminated passphrase from a protected stdin fd;
        /// never put a literal passphrase in the shell command.
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// Rehearse recovery and compare against the generation public key.
    Restore {
        /// Encrypted age escrow to recover.
        #[arg(long)]
        escrow: PathBuf,
        /// Public key printed by `generate`; mismatch fails closed.
        #[arg(long)]
        expect_public_key: NodeId,
        /// Read one newline-terminated passphrase from a protected stdin fd;
        /// never put a literal passphrase in the shell command.
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// Decrypt a boot-time runtime credential for the identity service.
    Load {
        /// Encrypted age escrow to load.
        #[arg(long)]
        escrow: PathBuf,
        /// New service-private runtime credential, normally on volatile storage.
        #[arg(long)]
        output: PathBuf,
        /// Public key printed by `generate`; mismatch fails closed.
        #[arg(long)]
        expect_public_key: NodeId,
        /// Read one newline-terminated passphrase from a protected stdin fd;
        /// never put a literal passphrase in the shell command.
        #[arg(long)]
        passphrase_stdin: bool,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Arguments::parse().operation {
        Operation::Generate { key_id, output } => {
            let key = generate_issuer_key(IssuerKeyId::new(key_id));
            write_runtime_credential(&output, &key)?;
            print_public_key(&key);
        }
        Operation::Escrow {
            credential,
            output,
            passphrase_stdin,
        } => {
            let passphrase = read_passphrase(passphrase_stdin, true)?;
            let key = escrow_issuer_key(&credential, &output, passphrase)?;
            print_public_key(&key);
        }
        Operation::Restore {
            escrow,
            expect_public_key,
            passphrase_stdin,
        } => {
            let passphrase = read_passphrase(passphrase_stdin, false)?;
            let key = restore_issuer_key(&escrow, passphrase, expect_public_key)?;
            print_public_key(&key);
        }
        Operation::Load {
            escrow,
            output,
            expect_public_key,
            passphrase_stdin,
        } => {
            let passphrase = read_passphrase(passphrase_stdin, false)?;
            let key = load_issuer_key(&escrow, &output, passphrase, expect_public_key)?;
            print_public_key(&key);
        }
    }
    Ok(())
}

fn print_public_key(key: &orrery_identity::IssuerSigningKey) {
    // D41's comparison value must come from the landed signing-key API, not a
    // parallel public-key derivation in this tool.
    println!("issuer_key_id={}", key.key_id().0);
    println!("public_key={}", key.public_key());
}

fn read_passphrase(from_stdin: bool, confirm: bool) -> io::Result<SecretString> {
    let mut passphrase = if from_stdin {
        let mut value = String::new();
        io::stdin().lock().read_line(&mut value)?;
        while value.ends_with(['\n', '\r']) {
            value.pop();
        }
        value
    } else {
        rpassword::prompt_password("Issuer escrow passphrase: ")?
    };
    if confirm && !from_stdin {
        let mut confirmation = rpassword::prompt_password("Confirm issuer escrow passphrase: ")?;
        let matches = passphrase == confirmation;
        confirmation.zeroize();
        if !matches {
            passphrase.zeroize();
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "passphrase confirmation does not match",
            ));
        }
    }
    Ok(SecretString::from(passphrase))
}
