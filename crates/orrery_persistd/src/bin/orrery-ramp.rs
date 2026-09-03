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
//!
//! # The cohort subcommand: D32 clause (e)'s sampling path
//!
//! ```sh
//! # Sample a natural-honest member. The store verifies the durable facts —
//! # the account row exists, probation is past, and the strike-ledger span is
//! # empty — in the same transaction that records the decision, and refuses
//! # with the reason when one fails.
//! orrery-ramp cohort sample --account 4242 --half natural \
//!     --reason "owner-sampled 2026-09-02" --fdb-cluster-file /etc/orrery/fdb.cluster
//!
//! # Record an armed-honest (operator harness) account. The decision is the
//! # fact: no cluster-side check can re-derive "this account is harness-driven".
//! orrery-ramp cohort sample --account 4243 --half armed \
//!     --reason "p1 swarm operator #3" --fdb-cluster-file /etc/orrery/fdb.cluster
//!
//! # Inspect the cohort a promotion review would be handed.
//! orrery-ramp cohort show --fdb-cluster-file /etc/orrery/fdb.cluster
//! ```
//!
//! # The window subcommand: D32 clause (e)'s `W`
//!
//! ```sh
//! # What the promotion review's time term actually stands at, with clause
//! # (e)'s armed/natural split reported apart.
//! orrery-ramp window show --control attestation_quorum \
//!     --fdb-cluster-file /etc/orrery/fdb.cluster
//!
//! # Retire the window because a semantic change invalidated what it saw.
//! # The counters are gone afterwards; read them first if you need them.
//! orrery-ramp window reset --control attestation_quorum \
//!     --reason "ruleset v9 rewrote the quorum predicate" \
//!     --fdb-cluster-file /etc/orrery/fdb.cluster
//! ```
//!
//! There is no `window set`, and the absence is the design. Counters are
//! measurements; an operator who could write them could write clause (e)'s
//! evidence directly, which would make the whole ramp ceremonial. The two
//! verbs an operator has are *read* and *start again*.
//!
//! Resetting is not automatic on a ruleset change. Whether it should be is a
//! policy question for the owner rather than a gap in this tool — see
//! `orrery_persistd::intent::window`'s module docs.
//!
//! Cohort rows carry no signature, unlike the posture rows above, and the
//! asymmetry is deliberate: a posture row commands enforcement, a cohort row
//! names a member of a measurement population. A forged membership row cannot
//! manufacture a clean `fp_count` or promote anything — see
//! `orrery_persistd::intent::cohort`'s module docs for the full argument.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};

use orrery_persistd::intent::cohort::{CohortHalf, CohortMemberRow, FdbHonestCohortStore};
use orrery_persistd::intent::posture::{self, SignedRampPosture};
use orrery_persistd::intent::window::{FdbRampWindowStore, RampWindowRow};
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
    /// Record, remove and inspect D32 clause (e)'s known-honest cohort.
    #[command(subcommand)]
    Cohort(CohortCommand),
    /// Inspect and reset D32 clause (e)'s durable measurement window `W`.
    #[command(subcommand)]
    Window(WindowCommand),
}

/// The measurement window's two verbs.
#[derive(Subcommand)]
enum WindowCommand {
    /// Show one control's durable window: its generation, `W` in days, and
    /// clause (e)'s counters with the armed/natural split intact.
    Show {
        /// One of D32 clause (c)'s stable control names.
        #[arg(long, value_name = "CONTROL")]
        control: String,
    },
    /// Retire the current window and open a fresh one.
    ///
    /// The deliberate reset. Use it when a semantic change — a ruleset
    /// version, a rewritten predicate — invalidates what the window already
    /// observed: clause (e) evidence spanning such a change would be worse
    /// than a short window. The retired counters are **not** archived, so read
    /// them with `window show` first if you need them.
    Reset {
        /// One of D32 clause (c)'s stable control names.
        #[arg(long, value_name = "CONTROL")]
        control: String,
        /// Why the prior observations no longer apply. Recorded in the row,
        /// bounded at 256 bytes.
        #[arg(long, value_name = "TEXT")]
        reason: String,
    },
}

/// The cohort subcommand's three verbs.
#[derive(Subcommand)]
enum CohortCommand {
    /// Record one sample decision. For a `natural` member the store verifies
    /// the durable facts — account row present, probation past, strike-ledger
    /// span empty — in the transaction that writes the row, and refuses with
    /// the failing fact when one does not hold.
    Sample {
        /// The account being sampled.
        #[arg(long, value_name = "ID")]
        account: u64,
        /// Which of clause (e)'s two halves this decision names.
        #[arg(long, value_enum)]
        half: HalfArg,
        /// Why. Recorded in the row, bounded at 256 bytes.
        #[arg(long, value_name = "TEXT")]
        reason: String,
    },
    /// Remove one account's membership row, the explicit correction path.
    Remove {
        /// The account to remove.
        #[arg(long, value_name = "ID")]
        account: u64,
    },
    /// List every recorded decision and the cohort totals a promotion
    /// reviewer reads.
    Show,
}

/// The two halves, on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum HalfArg {
    /// Operator-controlled accounts acting honestly under automation.
    Armed,
    /// Real players past probation with a clean archive, sampled in by hand.
    Natural,
}

impl From<HalfArg> for CohortHalf {
    fn from(value: HalfArg) -> Self {
        match value {
            HalfArg::Armed => Self::Armed,
            HalfArg::Natural => Self::Natural,
        }
    }
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
        Command::Cohort(cohort) => run_cohort(cohort, &context).await?,
        Command::Window(window) => run_window(window, &context).await?,
    }
    Ok(())
}

/// The summary line `cohort show` leads with.
fn cohort_summary(total: usize, armed: usize, natural: usize) -> String {
    format!("cohort: |H| = {total} ({armed} armed, {natural} natural)")
}

/// One member line of `cohort show`.
fn cohort_member_line(account: u64, row: &CohortMemberRow) -> String {
    format!(
        "  {} {} decided_at_ms={} reason={:?}",
        account,
        row.half.as_str(),
        row.decided_at_ms,
        row.reason
    )
}

/// Run one `cohort` verb against the durable cohort store.
///
/// Refusals are the tool working, not it failing: a sample that fails a
/// durable fact is the store saying *why this account cannot be natural
/// honest*, which is exactly what the operator at the terminal needs to see.
/// They still exit non-zero, so a scripted sampling run stops on the first
/// refusal rather than assembling a cohort someone later assumes was checked.
async fn run_cohort(
    command: CohortCommand,
    context: &orrery_persistd::FdbContext,
) -> anyhow::Result<()> {
    let store = FdbHonestCohortStore::from_context(context);
    match command {
        CohortCommand::Sample {
            account,
            half,
            reason,
        } => {
            let half = CohortHalf::from(half);
            store
                .sample(
                    orrery_protocol::AccountId::new(account),
                    half,
                    &reason,
                    now_ms(),
                    now_ms(),
                )
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            println!(
                "cohort: sampled account {account} as {}; the decision is durable and verified",
                half.as_str()
            );
        }
        CohortCommand::Remove { account } => {
            store
                .remove(orrery_protocol::AccountId::new(account))
                .await?;
            println!(
                "cohort: removed account {account}; the CLI startup default of nothing applies"
            );
        }
        CohortCommand::Show => {
            let members = store.members().await?;
            let armed = members
                .iter()
                .filter(|(_, row)| matches!(row.half, CohortHalf::Armed))
                .count();
            let natural = members.len() - armed;
            println!("{}", cohort_summary(members.len(), armed, natural));
            for (account, row) in members {
                println!("{}", cohort_member_line(account.0, &row));
            }
            println!(
                "clause (e)'s floor is |H| ≥ 100 with the split reported separately; \
                 an artifact's `active` count is the one that makes the size mean something"
            );
        }
    }
    Ok(())
}

/// The lines `window show` renders for one control's durable window.
///
/// Split out from [`run_window`] so the shape is testable without a cluster.
/// Every figure comes from the row; nothing is re-derived here, for the reason
/// `RampMeter::snapshot` gives about `scripts/ramp-report.py` — a second
/// implementation of a gate figure disagrees with the first exactly when it
/// matters. The one computed value is `window_days`, and it is
/// [`RampWindowRow::window_days`] rather than an arithmetic expression typed
/// out again here.
fn window_lines(control: &str, row: &RampWindowRow) -> Vec<String> {
    let counts = &row.counts;
    let mut lines = vec![
        format!(
            "rampw/{control}: window {} opened at {} ms{}",
            row.window_id,
            row.opened_at_ms,
            row.reset_reason
                .as_ref()
                .map_or_else(String::new, |reason| format!(" ({reason:?})"))
        ),
        format!(
            "  W = {:.3} days, from {} to {} ms, over {} flush(es)",
            row.window_days(),
            counts
                .first_ms
                .map_or_else(|| "—".to_owned(), |ms| ms.to_string()),
            counts
                .last_ms
                .map_or_else(|| "—".to_owned(), |ms| ms.to_string()),
            row.flushes
        ),
        format!(
            "  fleet: qualifying={} observed={} unevaluated={} would_act={}",
            counts.fleet.qualifying,
            counts.fleet.observed,
            counts.fleet.unevaluated,
            counts.fleet.would_act
        ),
        // The two halves on their own lines, never summed: a reviewer reading
        // this is deciding whether the control would have refused players or
        // bots, and one number cannot answer that.
        format!(
            "  armed:   qualifying={} observed={} would_act={} active_members={} would_act_members={}",
            counts.armed.qualifying,
            counts.armed.observed,
            counts.armed.would_act,
            counts.armed_active.len(),
            counts.armed_would_act.len()
        ),
        format!(
            "  natural: qualifying={} observed={} would_act={} active_members={} would_act_members={}",
            counts.natural.qualifying,
            counts.natural.observed,
            counts.natural.would_act,
            counts.natural_active.len(),
            counts.natural_would_act.len()
        ),
        format!(
            "  unattributed: qualifying={} observed={} would_act={}",
            counts.unattributed.qualifying,
            counts.unattributed.observed,
            counts.unattributed.would_act
        ),
    ];
    if counts.cohort_accounts_truncated > 0 {
        lines.push(format!(
            "  WARNING: {} cohort account id(s) did not fit the row; the \
             active and would-act member counts above are understated by at \
             most that much",
            counts.cohort_accounts_truncated
        ));
    }
    if counts.fleet_truncation_seen {
        lines.push(
            "  WARNING: this window folded traffic from the meter's \
             past-capacity truncation bucket; fleet account spread and the \
             cohort denominator are both understated by an unknown amount"
                .to_owned(),
        );
    }
    lines.push(
        "clause (e) needs W >= 30 days and |H| >= 100 with fp_count = 0; \
         `cohort show` is where |H| lives, and distinct-account counts \
         fleet-wide are per-process and are not in this row"
            .to_owned(),
    );
    lines
}

/// Run one `window` verb against the durable measurement window.
async fn run_window(
    command: WindowCommand,
    context: &orrery_persistd::FdbContext,
) -> anyhow::Result<()> {
    let store = FdbRampWindowStore::from_context(context);
    match command {
        WindowCommand::Show { control } => match store.load(&control).await? {
            None => println!(
                "rampw/{control}: no window row; no process has flushed one yet, \
                 so clause (e)'s W is zero"
            ),
            Some(row) => {
                for line in window_lines(&control, &row) {
                    println!("{line}");
                }
            }
        },
        WindowCommand::Reset { control, reason } => {
            let row = store.reset(&control, &reason, now_ms()).await?;
            println!(
                "rampw/{control}: window {} opened at {} ms; the previous \
                 window's counters are gone",
                row.window_id, row.opened_at_ms
            );
            println!(
                "every persistd metering this control discards the delta it \
                 was holding on its next flush, so at most one flush interval \
                 of post-reset observations is lost — which is the trade: no \
                 observation from before this reset can enter the new window"
            );
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

    /// The cohort subcommand parses, and its half argument maps onto D32
    /// clause (e)'s two halves in the order the record names them.
    #[test]
    fn the_cohort_subcommand_parses_with_both_halves() {
        let parsed = Cli::try_parse_from([
            "orrery-ramp",
            "--fdb-cluster-file",
            "/tmp/fdb.cluster",
            "cohort",
            "sample",
            "--account",
            "4242",
            "--half",
            "natural",
            "--reason",
            "owner-sampled",
        ])
        .expect("cohort sample parses");
        let Command::Cohort(CohortCommand::Sample { account, half, .. }) = parsed.command else {
            panic!("expected the cohort sample subcommand");
        };
        assert_eq!(account, 4242);
        assert!(matches!(CohortHalf::from(half), CohortHalf::Natural));

        let parsed = Cli::try_parse_from([
            "orrery-ramp",
            "--fdb-cluster-file",
            "/tmp/fdb.cluster",
            "cohort",
            "sample",
            "--account",
            "7",
            "--half",
            "armed",
            "--reason",
            "harness",
        ])
        .expect("armed sample parses");
        let Command::Cohort(CohortCommand::Sample { half, .. }) = parsed.command else {
            panic!("expected the cohort sample subcommand");
        };
        assert!(matches!(CohortHalf::from(half), CohortHalf::Armed));
    }

    /// `cohort show` renders the split a promotion reviewer reads: the
    /// halves separately, never folded into one number.
    #[test]
    fn the_cohort_summary_reports_the_halves_separately() {
        assert_eq!(
            cohort_summary(120, 40, 80),
            "cohort: |H| = 120 (40 armed, 80 natural)"
        );
        let row = CohortMemberRow {
            half: CohortHalf::Natural,
            decided_at_ms: 2_000,
            reason: "handed a day pass by the owner".to_owned(),
        };
        let line = cohort_member_line(4242, &row);
        assert!(line.contains("4242 natural"), "{line}");
        assert!(line.contains("decided_at_ms=2000"), "{line}");
    }

    /// `window show` renders the halves apart, which is the same rule
    /// `cohort show` follows and for the same reason: the number a reviewer
    /// needs is *which* forty accounts would have been refused.
    #[test]
    fn the_window_summary_reports_the_halves_separately() {
        let mut row = RampWindowRow::opened(2, 900, Some("ruleset v9".to_owned()));
        row.counts.observe_at(0);
        row.counts.observe_at(86_400_000 * 31);
        row.counts.armed.qualifying = 400;
        row.counts.armed.would_act = 1;
        row.counts.natural.qualifying = 600;
        row.counts.natural.would_act = 40;
        row.counts.fleet.qualifying = 1_000;
        row.flushes = 12;

        let rendered = window_lines("attestation_quorum", &row).join("\n");
        assert!(rendered.contains("window 2 opened at 900 ms"), "{rendered}");
        assert!(rendered.contains("\"ruleset v9\""), "{rendered}");
        assert!(rendered.contains("W = 31.000 days"), "{rendered}");
        assert!(rendered.contains("over 12 flush(es)"), "{rendered}");
        assert!(
            rendered.contains("armed:   qualifying=400 observed=0 would_act=1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("natural: qualifying=600 observed=0 would_act=40"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("would_act=41"),
            "the halves are never summed into one figure: {rendered}"
        );
    }

    /// Truncation is reported rather than absorbed, in both of the two ways a
    /// window can be understated.
    #[test]
    fn the_window_summary_warns_about_every_understated_figure() {
        let mut row = RampWindowRow::opened(0, 0, None);
        row.counts.cohort_accounts_truncated = 3;
        row.counts.fleet_truncation_seen = true;
        let rendered = window_lines("strikes", &row).join("\n");
        assert!(
            rendered.contains("3 cohort account id(s) did not fit"),
            "{rendered}"
        );
        assert!(rendered.contains("truncation bucket"), "{rendered}");
    }

    #[test]
    fn a_window_with_no_observations_renders_absent_bounds_rather_than_zero() {
        let row = RampWindowRow::opened(0, 5_000, None);
        let rendered = window_lines("strikes", &row).join("\n");
        assert!(
            rendered.contains("from — to — ms"),
            "an unobserved window has no bounds, and printing 0 would read as \
             an observation at the epoch: {rendered}"
        );
    }

    #[test]
    fn the_window_reset_verb_parses_with_its_reason() {
        let parsed = Cli::try_parse_from([
            "orrery-ramp",
            "--fdb-cluster-file",
            "/tmp/fdb.cluster",
            "window",
            "reset",
            "--control",
            "attestation_quorum",
            "--reason",
            "ruleset v9",
        ])
        .expect("window reset parses");
        let Command::Window(WindowCommand::Reset { control, reason }) = parsed.command else {
            panic!("expected the window reset subcommand");
        };
        assert_eq!(control, "attestation_quorum");
        assert_eq!(reason, "ruleset v9");
    }
}
