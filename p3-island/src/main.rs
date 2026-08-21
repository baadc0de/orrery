//! P3's permanent authority regression harness (docs/11-roadmap.md §P3).
//!
//! The phase's demo criterion is an 8-peer island with contested physics
//! objects: `kill -9` one peer holding entities, and **every** entity must be
//! reassigned or parked within the 10 s lease TTL, with no duplicate-authority
//! tick recorded and no entity lost. This tool is that proof, and — like the
//! P2 kill-9 gate it is modelled on — it is a proof harness rather than a
//! convenience script: it exits non-zero unless every clause holds.
//!
//! Shape:
//!
//! - The orchestrator mints one identity token and one coordinator interest
//!   grant per peer, then spawns each peer as its **own process**. The
//!   criterion says `kill -9`; a dropped task would prove only that the
//!   harness can stop calling the gateway, not that the registrar notices a
//!   torn connection.
//! - Peers claim their entities, uplink fenced diffs, and heartbeat. Each
//!   writes a JSONL event log the orchestrator reads back.
//! - One peer is SIGKILLed. Every entity it held must then reach a
//!   *disposition*: inherited by a survivor (a `Grant` carrying the registrar
//!   correlation), or parked at the registrar. The criterion counts both, so
//!   the clock has to stop on either.
//! - Anything with no disposition of its own is probed, and the probe's answer
//!   is three facts rather than one: the row was free to take, the row is
//!   **reserved** for the peer that died, or the row is neither. Only the last
//!   is a **lost entity** — the failure the phase exists to rule out.
//!
//! ## How each half of the criterion is observed, and why
//!
//! **Reassignment** is observed on the survivor: the inheriting peer logs the
//! grant with the wall-clock instant it arrived, so the settle time is the
//! registrar's latency and not the orchestrator's poll interval.
//!
//! **Parking became observable from a peer on the strong leg, and only
//! there.** It used to be observable from nowhere: the registrar told only the
//! *previous* holder that a lease parked (`Expire`), and that peer is the one
//! that was killed; nothing went to anybody else, and the heartbeat read path
//! returns rows only for leases the asking session already holds, so a
//! survivor could not poll for it either.
//!
//! D25 fans a verbatim copy of that same `Expire` out to
//! `A(G, grid, cell, t) = { p ∈ Sessions(G,t) : allows(p, grid, cell, t) }` —
//! every peer with a live session on this gateway whose coordinator interest
//! covers the entity's committed cell. Survivors therefore record
//! `PeerEvent::Observed`, and the orchestrator checks it. Two scoping clauses
//! are load-bearing, and neither is an implementation detail:
//!
//! - **Strong leg only.** With `--victim-claim-kind strong` the victim's rows
//!   are `STRONG_HELD`, so `Redistributor::place` parks each one *before* it
//!   computes any candidate (D7 §5: only weak authority is redistributed
//!   without consent), and the seven survivors are a live, covered audience.
//!   On the weak leg the same rows *reassign*, which D25 rule 7 deliberately
//!   keeps holder-only: INV-4 already converges every observer on the
//!   successor's first replicated envelope, so an advisory would buy one 20 Hz
//!   send interval and nothing else. Asserting observation on the weak leg
//!   would assert something unreachable by construction.
//! - **Best-effort by design.** D25 permits the gateway to *drop* an advisory
//!   rather than queue it behind a `Grant` on the same lane. So the clause is
//!   conditioned on the registrar's own `expire_fanout_dropped` reading zero
//!   for the window: a run that dropped is a run where non-delivery is
//!   correct, and the gate says nothing about it rather than failing it.
//!
//! What this does **not** change is which instrument times the settle. An
//! advisory is not a disposition; the registrar's counters remain the witness
//! that a row parked, for the reasons below.
//!
//! That leaves two instruments, and only one of them is honest:
//!
//! - *Claiming the entity.* A claim is not an observation — it changes what it
//!   measures. A `Weak` claim over a weak-held row is **granted even when the
//!   holder is live and unexpired** (`lease.rs::claim`), so a successful weak
//!   probe cannot tell "this was parked" from "I just stole it from the dying
//!   victim". A `Strong` claim is worse rather than better: the gateway turns
//!   a strong claim against a live holder into a cooperative *handoff request*
//!   (D7 §4.2, `gateway.rs`'s `contested` branch), and when the holder never
//!   answers — a SIGKILLed peer never answers — the weak-tier deadline rule
//!   hands the entity to the claimant unconditionally. A harness probing that
//!   way would be *performing* the redistribution it claims to be measuring.
//!   So no claim may be issued while the victim's lease could still be live.
//! - *The registrar's own disposition counters*, exported to persistd's
//!   `--metrics-jsonl` (`reassigned`, `parked_without_successor`). These are
//!   incremented by `Redistributor::place` at the moment it decides, they are
//!   registrar-attested rather than harness-inferred, and reading them
//!   disturbs nothing. Their cost is granularity: the exporter appends at
//!   1 Hz and its record carries no timestamp, so a park is *observed* up to
//!   one export interval — plus the harness's own poll interval — after it
//!   happened. That is a bounded overstatement of the settle time, which is
//!   the safe direction, and the settle budget below names both as terms.
//!
//! So: reassignment stops the clock per entity, parking stops it per cohort,
//! and the claim probe is demoted to what it can actually prove — that an
//! entity with no disposition still exists and still answers, i.e. that it is
//! not lost.
//!
//! ## A denied probe is not a lost entity (#129)
//!
//! The probe used to answer `Option<lease_id>`, and every `Deny` — whatever
//! the registrar's reason — collapsed into the `None` this report calls
//! `lost`. Two outcomes, where the registrar distinguishes four.
//!
//! On the weak leg that was invisible, because the victim's rows reassign and
//! are never probed at all. On the strong leg it is wrong for **every** row,
//! and the leg could not pass: `LeaseStore::claim` refuses a foreign claimant
//! on a parked strong row whose `bound_to` names its dead owner and whose
//! reservation has not lapsed (`lease.rs`, the `row.holder.is_none() &&
//! STRONG_HELD` branch), which is D7 §4.3 and docs/04-authority.md §7 working
//! exactly as specified — "a strong-owned entity whose owner crashed re-parks
//! with `own_seq` intact rather than being regranted". So all fifty probes
//! were denied, and the gate reported all fifty lost.
//!
//! The probe therefore reports the registrar's *reason*, and the four answers
//! are counted as the four different facts they are:
//!
//! - a **grant** — the row exists, answers, and no reservation stood over it;
//! - `Deny{Parked}` — **parked and reserved**. `park()` writes `bound_to` from
//!   the row's holder, and the only holder these rows ever had is the peer
//!   that was killed, so a reservation refusing this probe is the victim's
//!   reservation. A reserved row is accounted for; that reservation is the
//!   whole point of the disposition, and counting it as a loss asserts the
//!   opposite of the ADR;
//! - **any other `Deny`** — the registrar answered, but with something that is
//!   neither a grant nor a reservation. The probe has established nothing
//!   about the row's disposition, so this counts as lost rather than as an
//!   unexplained pass;
//! - **no answer at all** — unreachable, and unreserved. This is the literal
//!   lost entity.
//!
//! What is emphatically *not* the fix is making a parked strong row grantable.
//! That would delete the §7 reservation the registrar exists to defend and
//! would turn this gate green by breaking the thing it measures.

mod peer;
mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use orrery_protocol::{
    AccountId, CellId, ClaimBasis, ClaimId, ClaimKind, DenyReason, GatewayMsg, GatewayReply,
    GridId, IssuerKeyId, LeaseMsg, NodeId, PersistId, SeqPair, SessionStanding,
    SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, Tick, UnixMillis,
};

use crate::peer::{PeerConfig, PeerEvent};
use crate::wire::Session;

/// The peer the criterion kills. Its rows are the ones whose disposition is
/// measured, and the survivors are what a successor policy can choose from.
const VICTIM_INDEX: usize = 0;

/// D16 lease TTL: the window the criterion measures against.
const LEASE_TTL: Duration = Duration::from_secs(10);

/// The registrar's expiry sweep period (`gateway.rs`'s accept loop): a lease
/// that lapses just after a sweep waits up to this long to be noticed.
const REGISTRAR_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// persistd's `METRICS_REPORT_INTERVAL`: how long a disposition can sit in the
/// registrar's counters before it appears in `--metrics-jsonl`. This is the
/// lag of the only instrument that can see a park, so it is measurement error
/// in the *late* direction — never early.
const METRICS_EXPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Slack added to the TTL before the criterion is judged to have failed.
///
/// A `kill -9` is **not** seen as a connection drop. QUIC cannot distinguish a
/// dead process from a dead path until its own idle timeout, so the gateway
/// resolves a SIGKILLed peer by the *slow* path of docs/04-authority.md §4.3 —
/// the lease TTL lapsing 10 s after that peer's last heartbeat — rather than
/// the fast path of an observed disconnect.
///
/// The budget is the sum of exactly three terms, and every one of them is
/// granularity rather than fudge — each is a quantum some instrument between
/// the disposition and this harness rounds up to:
///
/// 1. the registrar's once-a-second expiry sweep, which is when a lapsed lease
///    is *noticed*;
/// 2. persistd's once-a-second metrics export, which is when a park becomes
///    *visible*, there being no other witness to one;
/// 3. this loop's own poll interval, which is when a visible park is *read*.
///    The record carries no timestamp of its own (`write_gateway_authority` in
///    persistd emits counters only), so a park can be timed no more finely
///    than the read that first sees it. Leaving this term out would make the
///    gate reject a park that happened inside the TTL purely because the
///    harness looked 50 ms later — stricter than the criterion, and by an
///    accident of the harness rather than a property of the registrar.
///
/// The heartbeat interval is deliberately **not** a term — a lease expires one
/// TTL after the last heartbeat, and the last heartbeat is at or before the
/// kill, so heartbeat age moves the expiry *earlier* and needs no budget. The
/// criterion is still that the disposition is bounded by the TTL; these three
/// are what stand between the disposition and the harness learning of it.
const SETTLE_GRANULARITY: Duration = Duration::from_millis(
    REGISTRAR_SWEEP_INTERVAL.as_millis() as u64
        + METRICS_EXPORT_INTERVAL.as_millis() as u64
        + SETTLE_POLL_INTERVAL.as_millis() as u64,
);

/// How long the harness waits, *after* the clock has stopped, for the
/// registrar's exported counters to account for every one of the victim's
/// rows. The clock stops on the first evidence, which is usually earlier than
/// the export that corroborates it, so a plain read here would almost always
/// predate it. Nothing measured depends on this wait; the cross-check does.
const ATTESTATION_WAIT: Duration = Duration::from_secs(5);

/// How often the settle loop re-reads the peer logs and the metrics file.
///
/// It bounds one half of the measurement and not the other. A reassignment is
/// timestamped by the peer that received the grant, so no amount of polling
/// changes that number. A park has no timestamp anywhere — only the counter
/// that moved — so it is timed by the read that first sees it, and this
/// interval is the error in that reading. It is therefore a term of
/// `SETTLE_GRANULARITY`, not a free parameter: shortening it tightens the
/// budget rather than loosening the measurement.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A harness watchdog, not a drain timing budget.
///
/// The drain verdict is counter-based and carries elapsed time as evidence
/// only. This ceiling keeps a dead registrar from hanging the proof forever;
/// reaching it means the required disposition evidence never arrived, not
/// that a latency target was missed.
const DRAIN_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Two complete registrar sweep periods with no disposition-counter movement.
///
/// One quiet sweep can straddle a change. Two consecutive periods establish
/// that both the sweep which could observe it and the following sweep were
/// quiet, without introducing a peer that could become a successor.
const DRAIN_QUIESCENCE_WINDOW: Duration =
    Duration::from_millis(REGISTRAR_SWEEP_INTERVAL.as_millis() as u64 * 2);

/// The claim tier a peer asks for, as a CLI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ClaimTier {
    Weak,
    Strong,
}

impl From<ClaimTier> for ClaimKind {
    fn from(tier: ClaimTier) -> Self {
        match tier {
            ClaimTier::Weak => ClaimKind::Weak,
            ClaimTier::Strong => ClaimKind::Strong,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "p3-island",
    about = "P3 authority gate: 8-peer island, kill -9, reassign-or-park proof"
)]
struct Cli {
    /// The gateway's `bind_addr` from persistd's readiness line.
    #[arg(long, value_name = "IP:PORT")]
    gateway_addr: String,

    /// The gateway's `node_id` from persistd's readiness line.
    #[arg(long, value_name = "NODE_ID")]
    gateway_node: String,

    /// Hex-encoded identity issuer secret; its public half must be persistd's
    /// `--issuer-key`.
    #[arg(long, value_name = "HEX")]
    issuer_secret: String,

    /// The coordinator's `bind_addr` from its readiness line.
    #[arg(long, value_name = "IP:PORT")]
    coordinator_addr: String,

    /// The coordinator's `node_id` from its readiness line.
    #[arg(long, value_name = "NODE_ID")]
    coordinator_node: String,

    /// Peers in the island. The criterion's number is 8.
    #[arg(long, default_value_t = 8)]
    peers: u8,

    /// Entities each peer claims. The criterion's victim holds ~50.
    #[arg(long, default_value_t = 50)]
    entities_per_peer: u32,

    /// The tier the victim claims its entities at.
    ///
    /// `weak` is the criterion's contested-physics case, and it redistributes.
    /// `strong` is the case that *parks*: D7 §5 refuses to redistribute strong
    /// ownership without consent, so `Redistributor::place` returns `Parked`
    /// for every one of the victim's entities. Both are correct registrar
    /// behaviour and the criterion accepts both, which is exactly why the gate
    /// has to be able to run — and stop its clock — on either.
    #[arg(long, value_enum, default_value_t = ClaimTier::Weak)]
    victim_claim_kind: ClaimTier,

    /// The cell the island occupies.
    #[arg(long, default_value = "0x8000000000000000")]
    cell: String,

    /// How long peers keep simulating. Must outlast the settle window with
    /// margin, so survivors are still logging while the proof is collected.
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,

    /// Where to put peer logs and the report.
    #[arg(long, value_name = "DIR", default_value = "p3-island-out")]
    out: PathBuf,

    /// persistd's `--metrics-jsonl` file, read for `duplicate_authority`.
    #[arg(long, value_name = "PATH")]
    metrics_jsonl: Option<PathBuf>,

    /// Run the D24 island-drain proof after the kill-9 criterion settles.
    ///
    /// The proof has every survivor end its coordinator presence and gateway
    /// session. The verdict is the registrar's own parking counter, not
    /// whether a coordinator advisory was delivered.
    #[arg(long)]
    drain: bool,

    /// Print the public half of the identity issuer secret as JSON and exit.
    ///
    /// persistd and the coordinator must be configured to trust it, so
    /// deriving it here keeps the harness from needing ed25519 in shell.
    #[arg(long)]
    print_keys: bool,

    /// Internal: run as one peer rather than the orchestrator.
    #[arg(long, hide = true)]
    peer_spec: Option<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if cli.print_keys {
        let issuer = iroh::SecretKey::from_bytes(&decode_key(&cli.issuer_secret)?);
        println!(
            "{}",
            serde_json::json!({ "issuer_public": issuer.public().to_string() })
        );
        return Ok(());
    }
    let runtime = tokio::runtime::Runtime::new()?;
    if let Some(spec) = cli.peer_spec.clone() {
        return runtime.block_on(run_peer(spec));
    }
    let outcome = runtime.block_on(orchestrate(cli))?;
    println!("{}", serde_json::to_string_pretty(&outcome.report)?);
    if outcome.passed {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// The material one spawned peer needs, handed over as a file rather than
/// argv: tokens and grants are long, and a process listing is not the place
/// for credentials.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PeerSpec {
    gateway_addr: String,
    gateway_node: String,
    coordinator_addr: String,
    coordinator_node: String,
    secret: String,
    token: String,
    cell: u64,
    entities: Vec<u64>,
    kind: ClaimKind,
    duration_secs: u64,
    log: PathBuf,
}

async fn run_peer(spec_path: PathBuf) -> Result<()> {
    let spec: PeerSpec = serde_json::from_slice(&std::fs::read(&spec_path)?)
        .with_context(|| format!("read peer spec {}", spec_path.display()))?;
    peer::run(PeerConfig {
        gateway: endpoint_addr(&spec.gateway_node, &spec.gateway_addr)?,
        coordinator: endpoint_addr(&spec.coordinator_node, &spec.coordinator_addr)?,
        secret: iroh::SecretKey::from_bytes(&decode_key(&spec.secret)?),
        token: decode_hex(&spec.token)?,
        cell: CellId::from_bits(spec.cell).context("peer spec cell is not a valid CellId")?,
        entities: spec.entities,
        kind: spec.kind,
        duration: Duration::from_secs(spec.duration_secs),
        log: spec.log,
    })
    .await
}

struct Outcome {
    passed: bool,
    report: serde_json::Value,
}

async fn orchestrate(cli: Cli) -> Result<Outcome> {
    std::fs::create_dir_all(&cli.out)
        .with_context(|| format!("create output directory {}", cli.out.display()))?;
    let cell = parse_cell(&cli.cell)?;
    let issuer = iroh::SecretKey::from_bytes(&decode_key(&cli.issuer_secret)?);
    let gateway = endpoint_addr(&cli.gateway_node, &cli.gateway_addr)?;
    let coordinator_addr = endpoint_addr(&cli.coordinator_node, &cli.coordinator_addr)?;

    anyhow::ensure!(cli.peers >= 2, "redistribution needs at least two peers");
    // Half the criterion is unmeasurable without the registrar's own counters:
    // nothing tells a third party that one specific row parked. A harness that
    // ran without them could only ever prove the reassignment half, which is
    // stricter than the criterion and silently so.
    let metrics_jsonl = cli
        .metrics_jsonl
        .as_deref()
        .context("--metrics-jsonl is required: the parked half of the criterion is observable only in persistd's authority counters")?;
    anyhow::ensure!(
        cli.drain,
        "--drain is required: the P3 gate proves both the kill-9 and island-drain legs"
    );
    // Survivors must still be running while the proof is collected: a peer
    // that exits mid-window stops writing the log the accounting reads, and
    // its own leases park, which would look like the victim's redistribution
    // failing.
    let settle_budget = LEASE_TTL + SETTLE_GRANULARITY;
    anyhow::ensure!(
        Duration::from_secs(cli.duration_secs) > settle_budget + Duration::from_secs(5),
        "--duration-secs must exceed the {}s settle budget with margin",
        settle_budget.as_secs()
    );
    let total_entities = u64::from(cli.entities_per_peer) * u64::from(cli.peers);

    // ── Peers ───────────────────────────────────────────────────────────
    // Entity ids are the dev-seeded range 1..=N. The orchestrator partitions
    // them so each peer's holdings are disjoint, which is what makes "the
    // victim's entities" a well-defined set after the kill.
    let mut children = Vec::new();
    let mut logs = Vec::new();
    let mut peer_nodes = Vec::new();
    for index in 0..cli.peers {
        let secret = peer_secret(index);
        let node = secret.public();
        let entities: Vec<u64> = (0..u64::from(cli.entities_per_peer))
            .map(|offset| u64::from(index) * u64::from(cli.entities_per_peer) + offset + 1)
            .collect();
        let log = cli.out.join(format!("peer-{index}.jsonl"));
        let spec_path = cli.out.join(format!("peer-{index}.json"));
        let spec = PeerSpec {
            gateway_addr: cli.gateway_addr.clone(),
            gateway_node: cli.gateway_node.clone(),
            coordinator_addr: cli.coordinator_addr.clone(),
            coordinator_node: cli.coordinator_node.clone(),
            secret: encode_hex(&secret.to_bytes()),
            token: encode_hex(&mint_token(&issuer, node)?),
            cell: cell.to_bits(),
            entities,
            // Only the victim's tier is a parameter: the survivors are the
            // island's contested physics objects either way, and it is the
            // victim's rows whose disposition the criterion is about.
            kind: if usize::from(index) == VICTIM_INDEX {
                cli.victim_claim_kind.into()
            } else {
                ClaimKind::Weak
            },
            duration_secs: cli.duration_secs,
            log: log.clone(),
        };
        std::fs::write(&spec_path, serde_json::to_vec(&spec)?)?;

        let child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("--peer-spec")
            .arg(&spec_path)
            // The orchestrator's own required flags are irrelevant in peer
            // mode but still parsed, so echo them through.
            .args(["--gateway-addr", &cli.gateway_addr])
            .args(["--gateway-node", &cli.gateway_node])
            .args(["--coordinator-addr", &cli.coordinator_addr])
            .args(["--coordinator-node", &cli.coordinator_node])
            .args(["--issuer-secret", &cli.issuer_secret])
            .args(["--out", &cli.out.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(std::fs::File::create(
                cli.out.join(format!("peer-{index}.log")),
            )?))
            .kill_on_drop(false)
            .spawn()
            .context("spawn island peer")?;
        children.push(child);
        logs.push(log);
        peer_nodes.push(node);
    }

    // Wait for every peer to finish claiming before disturbing anything: a
    // kill during the claim storm would measure a different property.
    let expected_per_peer = usize::try_from(cli.entities_per_peer).unwrap_or(usize::MAX);
    for (index, log) in logs.iter().enumerate() {
        wait_for_claims(log, expected_per_peer, Duration::from_secs(60))
            .await
            .with_context(|| format!("peer {index} never finished claiming"))?;
    }
    tracing::info!(peers = cli.peers, total_entities, "island formed");

    // ── The kill ────────────────────────────────────────────────────────
    let victim_entities: BTreeSet<u64> = read_events(&logs[VICTIM_INDEX])?
        .into_iter()
        .filter_map(|event| match event {
            PeerEvent::Claimed { entity, .. } | PeerEvent::Inherited { entity, .. } => Some(entity),
            _ => None,
        })
        .collect();
    anyhow::ensure!(
        !victim_entities.is_empty(),
        "the victim held nothing; there is no redistribution to prove"
    );
    let victim_node = peer_nodes[VICTIM_INDEX];
    let victim_pid = children[VICTIM_INDEX]
        .id()
        .context("victim process has already exited")?;
    // The registrar's disposition counters are absolute totals, so the deltas
    // that belong to this kill are measured from a baseline taken before it.
    let baseline = read_authority_counters(metrics_jsonl)?;
    // SIGKILL, not a graceful shutdown: the criterion is about a peer that
    // never gets to say goodbye.
    unsafe {
        libc_kill(victim_pid as i32);
    }
    tracing::warn!(
        pid = victim_pid,
        entities = victim_entities.len(),
        "kill -9 sent to the victim peer"
    );
    let killed_at = tokio::time::Instant::now();
    // The peer that inherits a lease timestamps the grant off the same system
    // clock, so the two readings are directly subtractable.
    let killed_at_unix_ms = unix_ms();

    // ── Settle ──────────────────────────────────────────────────────────
    // Poll rather than sleep a fixed window: the criterion is *how long* the
    // registrar takes to dispose of the victim's rows, so a fixed sleep would
    // measure the harness's own patience instead. Two dispositions count, and
    // they are observed differently (see the module header):
    //
    //   reassigned — per entity, timestamped by the survivor that got the
    //                grant, so the poll interval is not in the number;
    //   parked     — per cohort, from the registrar's own
    //                `parked_without_successor` counter, because nothing tells
    //                a third party that one specific row parked.
    //
    // The loop therefore stops as soon as *both* halves together account for
    // every entity the victim held — not, as it did before, only when every
    // one of them was inherited, which no run that legitimately parks can ever
    // satisfy and which is stricter than the criterion it cites.
    let mut inherited: BTreeMap<u64, u64> = BTreeMap::new();
    let mut inherited_by: BTreeMap<u64, usize> = BTreeMap::new();
    let mut parked_observed_in_ms: Option<u64> = None;
    let mut parked_delta = 0u64;
    // Aggregate counters can only be attributed to the victim while the
    // survivors keep their own leases; a survivor that lost one during the
    // window would be counted here as one of the victim's rows settling.
    // Checked rather than assumed.
    let mut survivor_losses: Vec<u64> = Vec::new();
    // D25's advisory, as observed by the survivors. Keyed by entity, valued by
    // the set of peers that saw a disposition for it — the second half is what
    // makes "*every* surviving peer observed it" checkable rather than "at
    // least somebody did".
    let mut observed_by: BTreeMap<u64, BTreeSet<usize>> = BTreeMap::new();
    let settled = loop {
        survivor_losses.clear();
        for (index, log) in logs.iter().enumerate() {
            if index == VICTIM_INDEX {
                continue;
            }
            for event in read_events(log)? {
                match event {
                    PeerEvent::Inherited { entity, at_ms, .. }
                        if victim_entities.contains(&entity) =>
                    {
                        inherited
                            .entry(entity)
                            .or_insert_with(|| at_ms.saturating_sub(killed_at_unix_ms));
                        inherited_by.insert(entity, index);
                    }
                    PeerEvent::Observed { entity, .. } if victim_entities.contains(&entity) => {
                        observed_by.entry(entity).or_default().insert(index);
                    }
                    PeerEvent::Lost { entity, .. } => survivor_losses.push(entity),
                    _ => {}
                }
            }
        }
        let outstanding = victim_entities
            .iter()
            .filter(|entity| !inherited.contains_key(entity))
            .count();
        if outstanding == 0 {
            break true;
        }
        // Whatever was not inherited can only have parked, and the registrar's
        // counter is the only honest witness to that. Reading it costs the
        // entities nothing — a probe claim would consume them.
        parked_delta = read_authority_counters(metrics_jsonl)?
            .parked
            .saturating_sub(baseline.parked);
        if parked_delta >= outstanding as u64 {
            parked_observed_in_ms = Some(killed_at.elapsed().as_millis() as u64);
            break true;
        }
        if killed_at.elapsed() >= settle_budget {
            break false;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    };
    // The clock stops here. What follows is accounting, not settling.
    //
    // A settled run is timed by its evidence, not by the loop: the last
    // inherit's own timestamp, and — when anything parked — the poll at which
    // the export showed the cohort covered. An unsettled run is timed by the
    // deadline it ran into, which is the only number that can be true about it.
    let reassigned_in_ms = inherited.values().copied().max().unwrap_or(0);
    let settled_in_ms = if settled {
        reassigned_in_ms.max(parked_observed_in_ms.unwrap_or(0))
    } else {
        killed_at.elapsed().as_millis() as u64
    };

    // The registrar's own account of the same window, waited for rather than
    // snatched: it must record a disposition — reassigned or parked — for every
    // row the victim held. This is the criterion restated in the registrar's
    // numbers instead of the harness's, and it is a pass clause of its own, so
    // a run cannot be settled by the harness's reading of the peer logs alone.
    let attestation_deadline = tokio::time::Instant::now() + ATTESTATION_WAIT;
    let final_counters = loop {
        let counters = read_authority_counters(metrics_jsonl)?;
        let attested = counters.reassigned.saturating_sub(baseline.reassigned)
            + counters.parked.saturating_sub(baseline.parked);
        if attested >= victim_entities.len() as u64
            || tokio::time::Instant::now() >= attestation_deadline
        {
            break counters;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    };
    let parked_attested = final_counters.parked.saturating_sub(baseline.parked);
    let fanout_sent = final_counters
        .fanout_sent
        .saturating_sub(baseline.fanout_sent);
    let fanout_dropped = final_counters
        .fanout_dropped
        .saturating_sub(baseline.fanout_dropped);

    // ── The park, observed ──────────────────────────────────────────────
    // Re-read the survivors' logs once the registrar's own account has caught
    // up. The settle loop stops on the *first* evidence a cohort was disposed
    // of, which is by construction at or before the last advisory lands, so a
    // reading taken there would undercount for reasons that have nothing to do
    // with the registrar.
    for (index, log) in logs.iter().enumerate() {
        if index == VICTIM_INDEX {
            continue;
        }
        for event in read_events(log)? {
            if let PeerEvent::Observed { entity, .. } = event {
                if victim_entities.contains(&entity) {
                    observed_by.entry(entity).or_default().insert(index);
                }
            }
        }
    }
    let survivors = usize::from(cli.peers).saturating_sub(1);
    // **This clause is a leg, not a blanket** (D25's third accepted caveat).
    //
    // It is reachable on the strong leg and only there. With
    // `--victim-claim-kind strong` the victim's rows are `STRONG_HELD`, so
    // `Redistributor::place` parks each one *before* any candidate is computed
    // — D7 §5: only weak authority is redistributed without consent — and the
    // seven survivors are a live audience with coordinator interest covering
    // the cell. On the weak leg those same rows reassign, and D25 rule 7 keeps
    // a reassignment holder-only because INV-4 already converges every
    // observer on the successor's first replicated envelope. Asserting
    // observation there would assert something unreachable by construction,
    // so the clause is scoped rather than universal.
    let expects_advisory = matches!(ClaimKind::from(cli.victim_claim_kind), ClaimKind::Strong);
    // And it is conditioned on the bound, because the advisory is best-effort
    // by design: D25 permits the gateway to drop one rather than queue it
    // behind a `Grant`. If the registrar reports having dropped any in this
    // window, the run is in the regime where non-delivery is correct and this
    // clause has nothing to say about it.
    let unobserved: Vec<u64> = if expects_advisory && fanout_dropped == 0 {
        victim_entities
            .iter()
            .copied()
            .filter(|entity| {
                observed_by
                    .get(entity)
                    .is_none_or(|seen| seen.len() < survivors)
            })
            .collect()
    } else {
        Vec::new()
    };

    // Anything with no disposition of its own is checked entity by entity, and
    // the check is deliberately narrow about what it proves. A claim is not an
    // observation: a weak claim over a weak-held row is granted even against a
    // live holder, so a grant here says "this entity still exists at the
    // registrar and still answers" — that it is **not lost**, which is the
    // clause the phase exists to rule out — and nothing at all about whether
    // it was parked. The parked figure in the report is the registrar's own
    // counter for that reason.
    //
    // It runs only after the clock has stopped, because a claim issued while
    // the victim's lease might still be live would take the entity and settle
    // it by hand.
    //
    // What it *does* distinguish, since #129, is the registrar's reason for
    // refusing. A `Deny{Parked}` is the answer D7 §4.3 specifies for a
    // strong-owned row whose owner crashed, and it is the answer every one of
    // the victim's rows gives on the strong leg; reading it as a loss made
    // that leg unpassable. See [`ProbeOutcome`].
    //
    // The probe is a stranger to those rows on purpose. The reservation only
    // refuses claimants other than the one it names, so a probe wearing the
    // victim's own identity would be *granted* every row and would report the
    // reservation as an ordinary free entity — the returning owner's reclaim
    // (`lease.rs`'s `a_crashed_strong_owner_is_not_regranted_to_whoever_claims_next`),
    // performed rather than observed. Its `peer_secret(u8::MAX)` identity is
    // therefore load-bearing and not just a spare key.
    //
    // It also has to run inside the reservation's own grace window
    // (`STRONG_PARK_GRACE_MS`, three lease TTLs from the park), because past
    // it §7's ordinary "the first `Claim` by anyone unparks it" resumes and
    // the same row answers with a grant. That is not a failure — a granted row
    // is accounted for too — but it would stop the strong leg from
    // demonstrating the reservation. Measured on this box: the rows park about
    // 10.7 s after the kill and the first probe is sent about 16 s after it,
    // against a 30 s grace.
    let probe_secret = peer_secret(u8::MAX);
    let probe_node = probe_secret.public();
    let probe_token = mint_token(&issuer, probe_node)?;
    // The probe is a peer like any other: it asks the coordinator for its
    // interest rather than assuming any.
    let probe_coordinator = orrery_coordinator::CoordinatorClient::connect(
        probe_secret.clone(),
        coordinator_addr.clone(),
        probe_token.clone(),
        Duration::from_secs(10),
    )
    .await
    .map_err(|error| anyhow::anyhow!("probe coordinator session: {error}"))?;
    probe_coordinator
        .report_presence(vec![cell])
        .map_err(|error| anyhow::anyhow!("probe presence: {error}"))?;
    let probe_grant = probe_coordinator
        .next_grant(Duration::from_secs(10))
        .await
        .map_err(|error| anyhow::anyhow!("probe interest grant: {error}"))?;

    let probe = Session::connect(probe_secret, gateway.clone()).await?;
    probe.send_control(&GatewayMsg::Hello {
        token: probe_token,
        node: probe_node,
    })?;
    anyhow::ensure!(
        matches!(
            probe.recv(Duration::from_secs(10)).await,
            Some(GatewayReply::HelloAck { .. })
        ),
        "probe session was refused"
    );
    probe.send_control(&GatewayMsg::InterestGrant { grant: probe_grant })?;
    anyhow::ensure!(
        matches!(
            probe.recv(Duration::from_secs(10)).await,
            Some(GatewayReply::InterestAck { epoch: Some(_), .. })
        ),
        "probe interest grant was refused"
    );

    // Four buckets, one per answer the registrar can give. `lost` is the union
    // of the two that account for nothing — refused for some reason that is
    // not a reservation, or never answered — and it is the only one that fails
    // the gate.
    let mut claimable = Vec::new();
    let mut reserved = Vec::new();
    let mut refused: Vec<serde_json::Value> = Vec::new();
    let mut unreachable: Vec<u64> = Vec::new();
    let mut lost = Vec::new();
    let mut claim_id = 1u64;
    for entity in &victim_entities {
        if inherited.contains_key(entity) {
            continue;
        }
        match probe_claim(&probe, ClaimId(claim_id), PersistId::new(*entity), cell).await? {
            ProbeOutcome::Granted(_) => claimable.push(*entity),
            ProbeOutcome::Reserved => reserved.push(*entity),
            ProbeOutcome::Refused(reason) => {
                // Carried with its reason rather than as a bare id: a run that
                // fails here has to say what the registrar actually said, or
                // the next reader repeats this issue's investigation.
                refused.push(serde_json::json!({ "entity": entity, "reason": reason }));
                lost.push(*entity);
            }
            ProbeOutcome::Unreachable => {
                unreachable.push(*entity);
                lost.push(*entity);
            }
        }
        claim_id += 1;
    }

    let reassigned_attested = final_counters
        .reassigned
        .saturating_sub(baseline.reassigned);

    // ── Drain ─────────────────────────────────────────────────────────
    // The probe joined the original island only to acquire its interest grant.
    // It holds no lease, and no replacement observer is introduced during the
    // drain: an instrument that joins now could become a redistribution
    // successor and would be participating rather than observing.
    probe_coordinator.leave().await;
    drop(probe);

    let drain_leases_held = total_entities.saturating_sub(parked_attested);
    let drain_baseline = read_authority_counters(metrics_jsonl)?;

    // Reuse the harness's process teardown: stop every survivor without
    // relocating it or opening a replacement session. This is deliberately
    // the uncooperative shape D24 must survive. A process can disappear before
    // QUIC flushes CONNECTION_CLOSE, so the registrar's own silence deadline
    // has to turn the nominally-open session into path 2 (session teardown).
    // The advisory Drain notice cannot be a clause: a stopped peer has no live
    // recipient, while the expiry/silence backstop still has to park its rows.
    for (index, child) in children.iter_mut().enumerate() {
        if index != VICTIM_INDEX {
            child
                .start_kill()
                .with_context(|| format!("tear down survivor peer {index}"))?;
        }
    }
    for (index, child) in children.iter_mut().enumerate() {
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .with_context(|| format!("peer {index} did not finish teardown"))??;
    }
    let drain_departed_at = tokio::time::Instant::now();
    let mut drain_counters = read_authority_counters(metrics_jsonl)?;
    let mut stable_since = tokio::time::Instant::now();
    let mut drain_last_change_in_ms = 0;
    let mut drain_counter_series = vec![authority_counter_sample(
        drain_departed_at.elapsed().as_millis() as u64,
        drain_counters,
        drain_baseline,
    )];
    // This is a harness watchdog, not a correctness bound. The registrar's
    // counters have no event timestamps, and this process cannot observe the
    // registrar's own `T_last_peer_gone`, so elapsed wall time is evidence and
    // never part of the verdict.
    let drain_observation_deadline =
        drain_departed_at + DRAIN_OBSERVATION_TIMEOUT + DRAIN_QUIESCENCE_WINDOW;
    let mut drain_quiesced = false;
    loop {
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        let next = read_authority_counters(metrics_jsonl)?;
        if next != drain_counters {
            drain_counters = next;
            stable_since = tokio::time::Instant::now();
            drain_last_change_in_ms = drain_departed_at.elapsed().as_millis() as u64;
            drain_counter_series.push(authority_counter_sample(
                drain_last_change_in_ms,
                drain_counters,
                drain_baseline,
            ));
        }
        let accounting = drain_accounting(drain_leases_held, next, drain_baseline);
        // Quiet is meaningful only after every starting lease has a D7
        // disposition. In particular, an intermediate reassignment is an
        // accounted disposition rather than a failed park; a later park may
        // move the counters again without invalidating either transition.
        if accounting.outstanding == 0 && stable_since.elapsed() >= DRAIN_QUIESCENCE_WINDOW {
            drain_quiesced = true;
            break;
        }
        if tokio::time::Instant::now() >= drain_observation_deadline {
            break;
        }
    }
    let drain_parked = drain_counters.parked.saturating_sub(drain_baseline.parked);
    let drain_reassigned = drain_counters
        .reassigned
        .saturating_sub(drain_baseline.reassigned);
    let drain_accounting = drain_accounting(drain_leases_held, drain_counters, drain_baseline);
    let drain_quiescence_observed_in_ms = drain_departed_at.elapsed().as_millis() as u64;
    let drain_last_disposition_in_ms = if drain_quiesced {
        drain_last_change_in_ms
    } else {
        drain_quiescence_observed_in_ms
    };
    drain_counter_series.push(authority_counter_sample(
        drain_quiescence_observed_in_ms,
        drain_counters,
        drain_baseline,
    ));
    let drain_passed = drain_passes(drain_quiesced, drain_accounting, drain_counters.duplicate);
    let duplicate_authority = drain_counters.duplicate;

    // Every clause of the criterion, and no others: every entity disposed of —
    // reassigned *or* parked — inside the budget, nothing lost, and no tick on
    // which two peers both believed they were the writer. `settled` is carried
    // separately from the elapsed comparison on purpose: a loop that ran out of
    // deadline reports exactly the budget in milliseconds, and `<=` alone would
    // let that read as a pass.
    let attribution_sound = survivor_losses.is_empty();
    let dispositions_attested = reassigned_attested + parked_attested;
    // The new category is corroborated rather than taken on the probe's word.
    // "Parked and reserved" is a claim about a *park*, and the registrar's own
    // `parked_without_successor` delta is what witnesses a park in this
    // harness — every other number in this report defers to it for the same
    // reason. Without this clause the reserved bucket would be a category the
    // gate accepts on the strength of a refusal alone, which is how a bucket
    // added to stop miscounting becomes somewhere to put anything awkward.
    let reservations_attested = reserved.len() as u64 <= parked_attested;
    let passed = settled
        && dispositions_attested >= victim_entities.len() as u64
        && settled_in_ms <= settle_budget.as_millis() as u64
        && lost.is_empty()
        && reservations_attested
        && duplicate_authority == 0
        && attribution_sound
        && unobserved.is_empty()
        && drain_passed;
    let drain_report = serde_json::json!({
        // No observer joins during this window: the series is the registrar's
        // non-invasive account of every D7 disposition. Reassignment and
        // parking are both valid; two quiet sweep periods after every starting
        // lease is accounted for establish quiescence.
        "drain_leases_held_at_start": drain_leases_held,
        "drain_parked_at_quiescence": drain_parked,
        "drain_reassigned_during_close": drain_reassigned,
        "drain_accounted_at_quiescence": drain_accounting.accounted,
        "drain_outstanding_at_quiescence": drain_accounting.outstanding,
        "drain_counter_series": drain_counter_series,
        "drain_quiesced": drain_quiesced,
        "drain_last_disposition_in_ms": drain_last_disposition_in_ms,
        "drain_quiescence_observed_in_ms": drain_quiescence_observed_in_ms,
        "drain_quiescence_window_ms": DRAIN_QUIESCENCE_WINDOW.as_millis() as u64,
        "drain_observation_timeout_ms": DRAIN_OBSERVATION_TIMEOUT.as_millis() as u64,
        "drain_passed": drain_passed,
    });
    let mut report = serde_json::json!({
        "peers": cli.peers,
        "entities_total": total_entities,
        "victim_node": victim_node.to_string(),
        "victim_claim_kind": format!("{:?}", ClaimKind::from(cli.victim_claim_kind)),
        "victim_entities": victim_entities.len(),
        // Reassignment is counted per entity off the survivors' logs;
        // `reassigned_attested` is the registrar's own count of the same thing
        // over the same window, carried so the two can be compared.
        "reassigned": inherited.len(),
        "reassigned_attested": reassigned_attested,
        "successors": inherited_by.values().collect::<BTreeSet<_>>().len(),
        // Parking is the registrar's counter, never the probe's grant count.
        // `parked` is that counter once it has caught up; `parked_delta` is
        // what it read at the poll that stopped the clock.
        "parked": parked_attested,
        "parked_when_clock_stopped": parked_delta,
        "dispositions_attested": dispositions_attested,
        // What the probe actually proves, split by the four answers the
        // registrar can give to it. `claimable_after_settle` is a grant: the
        // row exists, answers, and nothing stood over it.
        "claimable_after_settle": claimable.len(),
        // `Deny{Parked}`: the row is parked and reserved for the peer that was
        // killed (D7 §4.3; docs/04-authority.md §7). The specified answer on
        // the strong leg, and neither a reassignment nor a loss.
        "parked_and_reserved": reserved.len(),
        // The registrar's own park count, repeated here as the thing
        // `parked_and_reserved` is checked against; the pass clause is
        // `parked_and_reserved <= parked`.
        "reservations_attested": reservations_attested,
        // Any other `Deny`, with the reason, and no answer at all. Both are
        // lost — the row is not accounted for — and both are named separately
        // so the failure says which kind it was.
        "refused_after_settle": refused,
        "unreachable_after_settle": unreachable.len(),
        // Unreachable *and* unreserved: the union of the two above.
        "lost": lost,
        "settled": settled,
        "settled_in_ms": settled_in_ms,
        "reassigned_in_ms": reassigned_in_ms,
        "parked_observed_in_ms": parked_observed_in_ms,
        // How late the parked half of the measurement can be, by construction:
        // the export that makes a park visible, plus the poll that reads it.
        "park_observation_lag_ms": (METRICS_EXPORT_INTERVAL + SETTLE_POLL_INTERVAL).as_millis()
            as u64,
        "lease_ttl_ms": LEASE_TTL.as_millis() as u64,
        "settle_budget_ms": settle_budget.as_millis() as u64,
        "duplicate_authority": duplicate_authority,
        // A survivor losing a lease inside the window would put dispositions
        // that are not the victim's into the counters the clock reads.
        "survivor_leases_lost": survivor_losses.len(),
        // D25's `Expire` fan-out. `observed_entities` is how many of the
        // victim's rows at least one survivor saw a disposition for, and
        // `fully_observed_entities` how many were seen by *every* survivor —
        // the number the strong-leg clause is written against.
        "fanout_expected": expects_advisory,
        "fanout_sent_attested": fanout_sent,
        "fanout_dropped_attested": fanout_dropped,
        "observed_entities": observed_by.len(),
        "fully_observed_entities": observed_by
            .values()
            .filter(|seen| seen.len() >= survivors)
            .count(),
        "unobserved_entities": unobserved.len(),
        "passed": passed,
    });
    if let (Some(report), Some(drain)) = (report.as_object_mut(), drain_report.as_object()) {
        report.extend(drain.clone());
    }
    Ok(Outcome { passed, report })
}

/// SIGKILL by pid, without pulling in a libc dependency for one call.
unsafe fn libc_kill(pid: i32) {
    // `kill(2)` is the syscall the criterion names. `std::process::Child::kill`
    // is also SIGKILL, but the victim is owned by a `tokio::process::Child`
    // whose `kill` is async and would need the handle back out of the vector.
    std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .ok();
}

/// What the post-settle probe learned about one of the victim's rows.
///
/// Four variants because the registrar answers four different things, and the
/// two-valued `Option` this replaced could not tell the specified answer from
/// the failure the phase exists to rule out (#129, and the module header's
/// "A denied probe is not a lost entity").
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeOutcome {
    /// The registrar granted the claim, carrying the fencing token it issued.
    ///
    /// The row exists, still answers, and nothing stood over it — *not* that
    /// it was parked: `lease.rs::claim` grants a weak claim over a weak-held
    /// row whose holder is live and unexpired.
    Granted(u64),
    /// `Deny{Parked}`: the row is parked with a reservation that has not
    /// lapsed and that names somebody other than this probe.
    ///
    /// The only somebody it can name here is the peer that was killed — the
    /// reservation is written by `park()` out of the row's holder, and these
    /// rows were held by the victim and by nobody else. This is a disposition,
    /// not a loss.
    Reserved,
    /// Any other `Deny`, carrying the registrar's reason verbatim.
    ///
    /// Held by someone, not eligible, rate-limited, addressed to the wrong
    /// owner: the registrar answered, but with nothing that accounts for the
    /// row. Kept apart from [`ProbeOutcome::Reserved`] rather than folded into
    /// it, because folding it in is precisely how a new category becomes a
    /// place for failures to hide.
    Refused(String),
    /// The probe deadline passed with no answer naming this claim. Unreachable
    /// and unreserved: the lost entity, in the criterion's own words.
    Unreachable,
}

/// How long the probe waits for the registrar's answer about one row.
///
/// Nothing measured depends on it — the clock stopped before the first probe
/// was sent — so it is sized to be far longer than a loopback round trip and
/// short enough that fifty unanswered rows do not outlast the gate.
const PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// Ask the registrar about one entity and report which of the four answers it
/// gave.
///
/// The tier stays `Weak` deliberately: a `Strong` claim against a live holder
/// is turned into a cooperative handoff request whose unanswered deadline
/// hands the entity over, which is a harness resolving the redistribution
/// rather than observing it.
async fn probe_claim(
    session: &Session,
    claim_id: ClaimId,
    entity: PersistId,
    cell: CellId,
) -> Result<ProbeOutcome> {
    session.send_control(&GatewayMsg::Lease {
        message: LeaseMsg::Claim {
            claim_id,
            entity,
            grid: GridId::ROOT,
            cell,
            kind: ClaimKind::Weak,
            basis: ClaimBasis::Contact { tick: Tick::new(0) },
            observed: SeqPair::default(),
            tick: Tick::new(0),
        },
    })?;
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    Ok(probe_outcome(claim_id, deadline, |within| session.recv(within)).await)
}

/// The probe's reply loop, over a receiver rather than over a live session.
///
/// Split out from [`probe_claim`] for one reason: the mapping from replies to
/// outcomes is the whole of #129, and a mapping that can only be exercised by
/// standing up eight peers and a registrar is a mapping nothing checks. Driven
/// by the tests below with a scripted receiver, all four arms included.
async fn probe_outcome<F, R>(
    claim_id: ClaimId,
    deadline: tokio::time::Instant,
    mut recv: F,
) -> ProbeOutcome
where
    F: FnMut(Duration) -> R,
    R: std::future::Future<Output = Option<GatewayReply>>,
{
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        // A receiver handed a zero timeout is not obliged to block, so the
        // deadline is enforced here as well: without this a chatty gateway
        // whose traffic is all for other claims spins forever rather than
        // reporting the row unreachable.
        if remaining.is_zero() {
            return ProbeOutcome::Unreachable;
        }
        let Some(reply) = recv(remaining).await else {
            return ProbeOutcome::Unreachable;
        };
        if let Some(outcome) = classify_probe_reply(claim_id, &reply) {
            return outcome;
        }
    }
}

/// Map one gateway reply to the probe's verdict, or `None` when the reply is
/// not this claim's answer.
///
/// `None` means "keep listening", and it has to stay distinguishable from
/// every verdict: the probe shares its session with whatever else the gateway
/// pushes — advisories, another claim's `Deny` — and treating one of those as
/// this row's answer would report a fact about the wrong entity.
fn classify_probe_reply(claim_id: ClaimId, reply: &GatewayReply) -> Option<ProbeOutcome> {
    match reply {
        GatewayReply::Lease {
            message:
                LeaseMsg::Grant {
                    claim_id: answered,
                    lease_id,
                    ..
                },
        } if *answered == claim_id => Some(ProbeOutcome::Granted(lease_id.0)),
        GatewayReply::Lease {
            message:
                LeaseMsg::Deny {
                    claim_id: Some(answered),
                    reason,
                    ..
                },
        } if *answered == claim_id => Some(match reason {
            // The specified answer on the strong leg, and the one the gate
            // used to call a loss. See the module header.
            DenyReason::Parked => ProbeOutcome::Reserved,
            other => ProbeOutcome::Refused(format!("{other:?}")),
        }),
        _ => None,
    }
}

/// Block until a peer's log shows it has answered for every entity.
async fn wait_for_claims(log: &PathBuf, expected: usize, within: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let answered = read_events(log)
            .unwrap_or_default()
            .into_iter()
            .filter(|event| matches!(event, PeerEvent::Claimed { .. } | PeerEvent::Denied { .. }))
            .count();
        if answered >= expected {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "only {answered}/{expected} claims answered"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn read_events(path: &PathBuf) -> Result<Vec<PeerEvent>> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// The registrar's own authority counters, as last exported.
///
/// These are the harness's only non-invasive window into what the registrar
/// decided: `parked` is the sole witness that an entity parked rather than
/// being reassigned, and `duplicate_authority` is the single-writer invariant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AuthorityCounters {
    /// Ticks on which two peers both believed they held authority.
    duplicate: u64,
    /// Lost leases handed to a successor.
    reassigned: u64,
    /// Lost leases parked because no successor was eligible — or because the
    /// tier forbids redistributing them at all (D7 §5).
    parked: u64,
    /// Non-holder `Expire` advisories the registrar pushed (D25 rule 1).
    fanout_sent: u64,
    /// Advisories a bound refused — over the per-expiry recipient cap, or
    /// past a recipient's own egress bucket (D25 rules 8 and 9).
    ///
    /// Read for one reason only: the advisory is *best-effort by design*, and
    /// the bound explicitly permits the gateway to drop one. A gate that
    /// asserted delivery unconditionally would be asserting something the
    /// architecture allows to be false. This counter is what makes the
    /// observability clause below conditional on the regime the run was
    /// actually in rather than on hope.
    fanout_dropped: u64,
}

/// The starting leases for which the registrar has reported a D7 disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DrainAccounting {
    accounted: u64,
    outstanding: u64,
}

fn drain_accounting(
    held_at_start: u64,
    counters: AuthorityCounters,
    baseline: AuthorityCounters,
) -> DrainAccounting {
    let dispositions = counters
        .reassigned
        .saturating_sub(baseline.reassigned)
        .saturating_add(counters.parked.saturating_sub(baseline.parked));
    let accounted = dispositions.min(held_at_start);
    DrainAccounting {
        accounted,
        outstanding: held_at_start.saturating_sub(accounted),
    }
}

const fn drain_passes(
    quiesced: bool,
    accounting: DrainAccounting,
    duplicate_authority: u64,
) -> bool {
    quiesced && accounting.outstanding == 0 && duplicate_authority == 0
}

/// One change point in the drain quiescence series, relative to its baseline.
fn authority_counter_sample(
    elapsed_ms: u64,
    counters: AuthorityCounters,
    baseline: AuthorityCounters,
) -> serde_json::Value {
    serde_json::json!({
        "elapsed_ms": elapsed_ms,
        "reassigned": counters.reassigned.saturating_sub(baseline.reassigned),
        "parked_without_successor": counters.parked.saturating_sub(baseline.parked),
        "expire_fanout_sent": counters.fanout_sent.saturating_sub(baseline.fanout_sent),
        "expire_fanout_dropped": counters.fanout_dropped.saturating_sub(baseline.fanout_dropped),
        "duplicate_authority": counters.duplicate.saturating_sub(baseline.duplicate),
    })
}

/// Read the highest total persistd has reported for each authority counter.
///
/// Every counter is an absolute total rather than an interval delta, precisely
/// so an event one tick before the read is still visible; taking the maximum
/// rather than the last line is the same statement, and is robust to a record
/// being appended while this read is in flight.
fn read_authority_counters(path: &std::path::Path) -> Result<AuthorityCounters> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(AuthorityCounters::default());
    };
    let mut counters = AuthorityCounters::default();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("gateway_authority") {
            continue;
        }
        let field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        counters.duplicate = counters.duplicate.max(field("duplicate_authority"));
        counters.reassigned = counters.reassigned.max(field("reassigned"));
        counters.parked = counters.parked.max(field("parked_without_successor"));
        counters.fanout_sent = counters.fanout_sent.max(field("expire_fanout_sent"));
        counters.fanout_dropped = counters.fanout_dropped.max(field("expire_fanout_dropped"));
    }
    Ok(counters)
}

/// Wall-clock milliseconds, matching the stamp a peer puts on an inherit.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

fn mint_token(issuer: &iroh::SecretKey, node: NodeId) -> Result<Vec<u8>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    Ok(SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            AccountId::new(1),
            node,
            UnixMillis::new(now),
            SessionTokenTtlMs::new(3_600_000),
            SessionStanding::Good,
            IssuerKeyId::new(1),
        ),
        issuer,
    )?
    .encode()?)
}

/// Deterministic per-peer identity, so a rerun is diffable against the last.
fn peer_secret(index: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = index;
    bytes[1] = 0xA7;
    iroh::SecretKey::from_bytes(&bytes)
}

fn endpoint_addr(node: &str, socket: &str) -> Result<iroh::EndpointAddr> {
    let node = NodeId::from_str(node).context("gateway node id")?;
    let socket: std::net::SocketAddr = socket.parse().context("gateway socket address")?;
    Ok(iroh::EndpointAddr::from_parts(
        node,
        [iroh::TransportAddr::Ip(socket)],
    ))
}

fn parse_cell(value: &str) -> Result<CellId> {
    let bits = if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).context("cell id hex")?
    } else {
        value.parse::<u64>().context("cell id")?
    };
    CellId::from_bits(bits).context("value is not a valid CellId")
}

fn decode_key(value: &str) -> Result<[u8; 32]> {
    let bytes = decode_hex(value)?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret keys are 32 bytes"))
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    data_encoding::HEXLOWER_PERMISSIVE
        .decode(value.as_bytes())
        .context("decode hex")
}

fn encode_hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::LeaseId;

    /// The parked half of the criterion rides entirely on one field name in
    /// persistd's export. If that name ever moves, `parked` reads zero for
    /// every run, and the gate silently becomes the reassign-only gate this
    /// harness was fixed to stop being — while still passing the weak run,
    /// which is what would keep the regression invisible. So the wire name is
    /// pinned here rather than trusted.
    #[test]
    fn parked_is_read_from_the_registrars_own_counter() {
        let dir = std::env::temp_dir().join(format!("p3-counters-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"gateway_bulk_latency\",\"parked_without_successor\":99}\n",
                "{\"type\":\"gateway_authority\",\"duplicate_authority\":0,",
                "\"reassigned\":3,\"parked_without_successor\":7}\n",
                // A later record with lower totals cannot exist, but a torn
                // trailing line can: the read takes the maximum, so a partial
                // append never walks a counter backwards.
                "{\"type\":\"gateway_authority\",\"reassigned\":1}\n",
                "{\"type\":\"gateway_auth\n",
            ),
        )
        .unwrap();
        let counters = read_authority_counters(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(counters.parked, 7, "parked_without_successor not read");
        assert_eq!(counters.reassigned, 3, "reassigned walked backwards");
        assert_eq!(counters.duplicate, 0);
    }

    /// The budget must cover every instrument between a disposition and this
    /// harness learning of it, or a park that happened inside the TTL fails
    /// the gate on the harness's own reading cadence. The report publishes
    /// that lag as `park_observation_lag_ms`; this ties the two together, so
    /// dropping a term from one without the other cannot go unnoticed.
    #[test]
    fn the_settle_budget_covers_the_park_observation_lag() {
        let published_lag = METRICS_EXPORT_INTERVAL + SETTLE_POLL_INTERVAL;
        assert!(
            SETTLE_GRANULARITY >= REGISTRAR_SWEEP_INTERVAL + published_lag,
            "settle budget {SETTLE_GRANULARITY:?} does not cover sweep + {published_lag:?}"
        );
    }

    /// Reassignment is a valid D7 disposition, not a failed attempt to park.
    ///
    /// The hosted-runner shape from #218 is deliberately represented here: a
    /// lease can be reassigned during concurrent close and park later, so the
    /// cumulative park count need not equal the number held at drain start.
    #[test]
    fn reassigned_then_parked_leases_are_accounted_once() {
        let accounting = drain_accounting(
            400,
            AuthorityCounters {
                reassigned: 378,
                parked: 589,
                ..AuthorityCounters::default()
            },
            AuthorityCounters::default(),
        );
        assert_eq!(accounting.accounted, 400);
        assert_eq!(accounting.outstanding, 0);
        assert!(drain_passes(true, accounting, 0));
        assert!(!drain_passes(
            true,
            DrainAccounting {
                accounted: 399,
                outstanding: 1,
            },
            0,
        ));
        assert!(!drain_passes(false, accounting, 0));
        assert!(!drain_passes(true, accounting, 1));
    }

    /// One `Deny` for the probe to be answered with.
    fn deny(claim_id: Option<ClaimId>, reason: DenyReason) -> GatewayReply {
        GatewayReply::Lease {
            message: LeaseMsg::Deny {
                claim_id,
                entity: PersistId::new(1),
                reason,
                retry_after_ms: 0,
            },
        }
    }

    /// One `Grant` for the probe to be answered with.
    fn grant(claim_id: ClaimId, lease_id: u64) -> GatewayReply {
        GatewayReply::Lease {
            message: LeaseMsg::Grant {
                claim_id,
                entity: PersistId::new(1),
                lease_id: LeaseId(lease_id),
                seq: SeqPair::default(),
                ttl_ms: 10_000,
                prev_holder: None,
            },
        }
    }

    /// The mapping #129 is about, arm by arm.
    ///
    /// Every one of these was `None` before, and `None` is what the report
    /// calls `lost`: the grant and the reservation were indistinguishable from
    /// the loss. Written against the classifier rather than against a live
    /// island, because a mapping that needs eight peers and a registrar to
    /// exercise is a mapping nothing exercises.
    #[test]
    fn the_probe_tells_the_registrars_four_answers_apart() {
        let ours = ClaimId(7);
        assert_eq!(
            classify_probe_reply(ours, &grant(ours, 42)),
            Some(ProbeOutcome::Granted(42)),
            "a grant is not reported as a grant"
        );
        assert_eq!(
            classify_probe_reply(ours, &deny(Some(ours), DenyReason::Parked)),
            Some(ProbeOutcome::Reserved),
            "the parked reservation — the specified answer on the strong leg — is not reported as one"
        );
        let Some(ProbeOutcome::Refused(why)) =
            classify_probe_reply(ours, &deny(Some(ours), DenyReason::NotEligible))
        else {
            panic!("a denial that is not the reservation must not read as one");
        };
        assert!(
            why.contains("NotEligible"),
            "the refusal does not carry the registrar's reason: {why}"
        );
        // And the fourth: nothing arrives at all. Driven through the reply
        // loop, because "no answer" is a property of the loop rather than of
        // any one reply.
        let timed_out = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(probe_outcome(
                ours,
                tokio::time::Instant::now() + Duration::from_millis(50),
                |_| async { None },
            ));
        assert_eq!(
            timed_out,
            ProbeOutcome::Unreachable,
            "a probe nobody answered is not reported unreachable"
        );
    }

    /// Somebody else's answer is not this row's answer.
    ///
    /// The probe shares one session with everything else the gateway pushes,
    /// and the four verdicts are only worth anything if the reply that carries
    /// one is the reply to *this* claim. A classifier that matched on shape
    /// alone would report another entity's denial as this entity's, which is
    /// the same class of mistake as #129 one level down.
    #[test]
    fn a_reply_for_another_claim_is_not_this_rows_answer() {
        let ours = ClaimId(7);
        let theirs = ClaimId(8);
        assert_eq!(classify_probe_reply(ours, &grant(theirs, 42)), None);
        assert_eq!(
            classify_probe_reply(ours, &deny(Some(theirs), DenyReason::Parked)),
            None
        );
        // A `Deny` with no correlation at all answers no claim in particular
        // (`claim_id: None` is the protocol's non-claim rejection).
        assert_eq!(
            classify_probe_reply(ours, &deny(None, DenyReason::Parked)),
            None
        );
    }

    /// The reply loop keeps listening past replies that are not its own, and
    /// still stops at the deadline.
    ///
    /// Both halves in one test on purpose: a loop that gave up on the first
    /// foreign reply would report a live row unreachable, and a loop that
    /// never gave up would hang the gate on a chatty gateway rather than
    /// failing it.
    #[test]
    fn the_reply_loop_skips_foreign_replies_without_spinning_forever() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let ours = ClaimId(7);

        let mut scripted = vec![
            grant(ClaimId(8), 1),
            deny(Some(ClaimId(9)), DenyReason::RateLimited),
            deny(Some(ours), DenyReason::Parked),
        ]
        .into_iter();
        let found = runtime.block_on(probe_outcome(
            ours,
            tokio::time::Instant::now() + Duration::from_secs(5),
            |_| {
                let next = scripted.next();
                async move { next }
            },
        ));
        assert_eq!(found, ProbeOutcome::Reserved);

        // A receiver that answers instantly and never for us: the deadline is
        // the only thing that can end this.
        let chatty = runtime.block_on(probe_outcome(
            ours,
            tokio::time::Instant::now() + Duration::from_millis(50),
            |_| async { Some(grant(ClaimId(8), 1)) },
        ));
        assert_eq!(chatty, ProbeOutcome::Unreachable);
    }
}
