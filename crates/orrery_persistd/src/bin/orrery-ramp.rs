//! `orrery-ramp` — the operator plane's authenticated writer for D32 clause
//! (c)'s runtime lever
//! ([D32](../../../docs/adr/0032-enforcement-ramp.md) clauses (c) and (i)).
//!
//! # What this tool is, and what it deliberately is not
//!
//! It is a *signer* that happens to write FoundationDB. It is not a privileged
//! writer service, and it holds no authority of its own: everything it produces
//! is checked again by every `persistd` on the next poll, against that
//! process's `--operator-key` set. Running this tool with no operator secret,
//! or with the wrong one, produces a row every gateway refuses. That is the
//! whole point of clause (i) — verification at the reader means the operator
//! plane is not a second trust root, and compromising this binary does not
//! silence enforcement.
//!
//! It follows that this tool's own checks are a *courtesy*, not a control. It
//! refuses a de-hardening write with no expiry before writing it, so the
//! operator learns at the terminal instead of from a fleet that ignored their
//! row; but the refusal that matters is the one in
//! [`orrery_persistd::intent::posture::admit`], which runs in every process and
//! does not care what wrote the bytes.
//!
//! # Key custody is D41's, and this tool says so by doing as little as possible
//!
//! `--operator-secret-file` names a file holding one hex-encoded Ed25519 secret
//! key. A *file* and not an argument, because a secret in `argv` is a secret in
//! every process listing on the host, and not a keyring, an HSM driver or an
//! agent protocol, because operator key custody, issuance and rotation are
//! [D41](../../../docs/adr/0041-offline-identity-issuer-custody-and-lifecycle.md)'s
//! lane. This tool takes the narrowest possible dependency on that lane — a
//! path — so D41 can decide what produces the file without amending this
//! binary.
//!
//! # Usage
//!
//! ```sh
//! # Promote C5 to live. A promotion needs no expiry.
//! orrery-ramp set --control strikes --mode live \
//!     --reason "clause (e) review 2026-09-02" \
//!     --operator-secret-file /run/secrets/operator.key
//!
//! # Demote C2 for four hours. Below its D32 default, so the expiry is
//! # mandatory and the tool refuses the write without it.
//! orrery-ramp set --control quarantine_validation --mode shadow \
//!     --reason "incident 4471" --expires-in-ms 14400000 \
//!     --operator-secret-file /run/secrets/operator.key
//!
//! # Restore the CLI startup default everywhere.
//! orrery-ramp clear --control strikes
//!
//! # Read what the fleet would see, verified as a poller verifies it.
//! orrery-ramp show --control strikes --operator-key 1@<public-key>
//! ```

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};

use orrery_persistd::intent::posture::{self, SignedRampPosture};
use orrery_persistd::intent::{FdbRampPostureStore, PostureSource, RampMode, RampPosture};

/// The longest `reason` this tool will sign.
///
/// [`RampPosture::reason`] documents "limited to 256 bytes by writers"; this is
/// the writer that limit was waiting for. The bound is enforced here rather
/// than at the reader because a reader that truncated a signed field would
/// break the signature, and one that refused an over-long row would hand
/// whoever can write FoundationDB a way to make a control unreadable.
const MAX_REASON_BYTES: usize = 256;

#[derive(Parser)]
#[command(
    name = "orrery-ramp",
    about = "Write, clear and inspect D32 clause (c)'s durable ramp posture rows"
)]
struct Cli {
    /// FoundationDB cluster file to write through.
    ///
    /// Named to match `world-census`, the tree's other operator-plane binary.
    /// Holding this file is what lets the tool *reach* the row; it is not what
    /// lets the row take effect, which is the whole of clause (i).
    #[arg(long, value_name = "PATH")]
    fdb_cluster_file: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign and store one control's posture.
    Set {
        /// One of D32 clause (c)'s stable control names.
        #[arg(long, value_name = "CONTROL")]
        control: String,
        /// The posture to select.
        #[arg(long, value_enum)]
        mode: ModeArg,
        /// Why. Recorded in the row, signed, and shown by `show`.
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// Auto-suspend incident this write clears, as 32 hex characters.
        #[arg(long, value_name = "HEX32")]
        incident_id: Option<String>,
        /// How long this posture may last, in milliseconds from now.
        ///
        /// Mandatory when the write leaves the control below its D32 clause (c)
        /// default — today that is `quarantine_validation` alone. Refused as
        /// meaningless on a promotion, so an operator cannot believe they set
        /// an expiry that the fleet will not honour.
        #[arg(long, value_name = "MS")]
        expires_in_ms: Option<u64>,
        /// File holding the operator's hex-encoded Ed25519 secret key.
        #[arg(long, value_name = "PATH")]
        operator_secret_file: PathBuf,
    },
    /// Remove one control's posture row, restoring every process's CLI default.
    Clear {
        /// One of D32 clause (c)'s stable control names.
        #[arg(long, value_name = "CONTROL")]
        control: String,
    },
    /// Show what a poller holding these keys would make of the stored row.
    Show {
        /// One of D32 clause (c)'s stable control names.
        #[arg(long, value_name = "CONTROL")]
        control: String,
        /// Trusted operator key in `<key-id>@<public-key>` form, repeatable —
        /// the same value a `persistd` would be given.
        #[arg(long, value_name = "KEY_ID@PUBLIC_KEY")]
        operator_key: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Off,
    Shadow,
    Live,
}

impl From<ModeArg> for RampMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Off => Self::Off,
            ModeArg::Shadow => Self::Shadow,
            ModeArg::Live => Self::Live,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn read_secret(path: &PathBuf) -> anyhow::Result<iroh::SecretKey> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    let trimmed = text.trim();
    let mut bytes = [0u8; 32];
    if trimmed.len() != 64 {
        anyhow::bail!(
            "{} must hold 64 hex characters (one Ed25519 secret key), found {}",
            path.display(),
            trimmed.len()
        );
    }
    for (index, pair) in trimmed.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).unwrap_or_default();
        bytes[index] = u8::from_str_radix(text, 16)
            .map_err(|error| anyhow::anyhow!("{} is not hex: {error}", path.display()))?;
    }
    Ok(iroh::SecretKey::from_bytes(&bytes))
}

fn parse_operator_key(spec: &str) -> anyhow::Result<orrery_protocol::NodeId> {
    let (_, public_key) = spec
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("expected operator key as <key-id>@<public-key>"))?;
    orrery_protocol::NodeId::from_str(public_key)
        .map_err(|error| anyhow::anyhow!("invalid operator public key `{public_key}`: {error}"))
}

fn parse_incident(hex: &str) -> anyhow::Result<[u8; 16]> {
    if hex.len() != 32 {
        anyhow::bail!("--incident-id must be 32 hex characters");
    }
    let mut id = [0u8; 16];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).unwrap_or_default();
        id[index] = u8::from_str_radix(text, 16)
            .map_err(|error| anyhow::anyhow!("--incident-id is not hex: {error}"))?;
    }
    Ok(id)
}

/// Build the row this invocation would sign, and refuse the combinations D32
/// clause (i) refuses.
///
/// Split out from [`main`] so the argument-shaped rules are testable without a
/// cluster; the *authorisation* rules are not tested here at all, because they
/// are not this binary's — they are `posture::admit`'s, and they run in every
/// gateway.
fn compose(
    control: &str,
    mode: RampMode,
    reason: String,
    incident_id: Option<[u8; 16]>,
    expires_in_ms: Option<u64>,
    now_ms: u64,
    key: &iroh::SecretKey,
) -> anyhow::Result<SignedRampPosture> {
    if reason.len() > MAX_REASON_BYTES {
        anyhow::bail!("--reason is limited to {MAX_REASON_BYTES} bytes");
    }
    let de_hardening = posture::is_de_hardening(control, mode);
    match (de_hardening, expires_in_ms) {
        (true, None) => anyhow::bail!(
            "{control} = {mode:?} is below D32's default for this control, so it is a \
             de-hardening write and needs --expires-in-ms. Clause (i): an incident \
             demotion may not outlive its incident by inattention, and nothing alerts \
             on a posture row that is simply still there."
        ),
        (false, Some(_)) => anyhow::bail!(
            "{control} = {mode:?} is not below D32's default, so it is a promotion and \
             carries no expiry. Clause (f)'s asymmetry: hardening is permanent, \
             weakening expires."
        ),
        _ => {}
    }
    let row = posture::sign_posture(
        control,
        RampPosture {
            mode,
            source: PostureSource::Operator,
            set_at_ms: now_ms,
            reason,
            incident_id,
        },
        expires_in_ms.map(|delta| now_ms.saturating_add(delta)),
        key,
    );

    // Sign, then verify against our own key before writing. A row this tool
    // cannot admit is a row no gateway will admit, and finding that out at the
    // terminal is better than finding it out from a fleet that did not move.
    posture::admit(control, &row, &[key.public()], now_ms).map_err(|refusal| {
        anyhow::anyhow!("this row would be refused by every poller: {refusal}")
    })?;
    Ok(row)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let context = orrery_persistd::FdbContext::connect(&cli.fdb_cluster_file.display().to_string())
        .map_err(|error| anyhow::anyhow!("open FoundationDB cluster: {error}"))?;
    let store = FdbRampPostureStore::from_context(&context);

    match cli.command {
        Command::Set {
            control,
            mode,
            reason,
            incident_id,
            expires_in_ms,
            operator_secret_file,
        } => {
            let key = read_secret(&operator_secret_file)?;
            let incident = incident_id.as_deref().map(parse_incident).transpose()?;
            let row = compose(
                &control,
                mode.into(),
                reason,
                incident,
                expires_in_ms,
                now_ms(),
                &key,
            )?;
            store.write(&control, &row).await?;
            println!(
                "ramp/{control} = {:?}, signed by {}, expires {}",
                row.posture.mode,
                key.public(),
                row.expires_at_ms
                    .map_or_else(|| "never".to_string(), |at| format!("at {at} ms")),
            );
            println!(
                "every persistd trusting this key applies it within D32 clause (c)'s 2 s bound"
            );
        }
        Command::Clear { control } => {
            store.clear(&control).await?;
            println!("ramp/{control} removed; every process reverts to its CLI startup default");
        }
        Command::Show {
            control,
            operator_key,
        } => {
            let keys = operator_key
                .iter()
                .map(|spec| parse_operator_key(spec))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let store = store.with_operator_keys(keys);
            match store.read(&control).await? {
                None => {
                    println!("ramp/{control}: no admissible row; the CLI startup default applies")
                }
                Some(posture) => println!(
                    "ramp/{control}: mode={:?} source={:?} set_at_ms={} reason={:?}",
                    posture.mode, posture.source, posture.set_at_ms, posture.reason
                ),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_persistd::gateway::STRIKES_CONTROL;
    use orrery_persistd::intent::QUARANTINE_VALIDATION_CONTROL;

    fn key() -> iroh::SecretKey {
        iroh::SecretKey::from_bytes(&[3; 32])
    }

    #[test]
    fn the_writer_refuses_a_permanent_de_hardening_write() {
        let error = compose(
            QUARANTINE_VALIDATION_CONTROL,
            RampMode::Shadow,
            "incident 4471".to_string(),
            None,
            None,
            1_000,
            &key(),
        )
        .expect_err("a de-hardening write with no expiry is not writable");
        assert!(format!("{error}").contains("--expires-in-ms"));
    }

    #[test]
    fn the_writer_refuses_an_expiry_on_a_promotion() {
        let error = compose(
            STRIKES_CONTROL,
            RampMode::Live,
            "clause (e) review".to_string(),
            None,
            Some(3_600_000),
            1_000,
            &key(),
        )
        .expect_err("clause (f)'s asymmetry is not negotiable at the writer either");
        assert!(format!("{error}").contains("promotion"));
    }

    #[test]
    fn the_writer_refuses_c2s_closed_off_arm() {
        let error = compose(
            QUARANTINE_VALIDATION_CONTROL,
            RampMode::Off,
            "mass-quarantine bug".to_string(),
            None,
            Some(3_600_000),
            1_000,
            &key(),
        )
        .expect_err("D32 open question 3 is closed in the negative");
        assert!(format!("{error}").contains("refused by every poller"));
    }

    #[test]
    fn a_signed_promotion_round_trips_through_the_verifier() {
        let row = compose(
            STRIKES_CONTROL,
            RampMode::Live,
            "clause (e) review 2026-09-02".to_string(),
            None,
            None,
            1_000,
            &key(),
        )
        .expect("a promotion signs");
        let value = posture::encode(&row).expect("encode");
        assert_eq!(
            posture::verdict(STRIKES_CONTROL, Some(&value), &[key().public()], 2_000),
            posture::PostureVerdict::Admitted(row.posture),
        );
        assert!(
            matches!(
                posture::verdict(STRIKES_CONTROL, Some(&value), &[], 2_000),
                posture::PostureVerdict::Refused(_)
            ),
            "a process trusting no operator key admits nothing this tool writes"
        );
    }

    #[test]
    fn a_reason_longer_than_the_documented_bound_is_refused() {
        let error = compose(
            STRIKES_CONTROL,
            RampMode::Live,
            "x".repeat(MAX_REASON_BYTES + 1),
            None,
            None,
            1_000,
            &key(),
        )
        .expect_err("RampPosture::reason documents a 256-byte writer bound");
        assert!(format!("{error}").contains("256"));
    }
}
