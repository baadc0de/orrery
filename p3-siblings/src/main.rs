//! Two sibling gateways over disjoint shards, and the single-writer invariant
//! stated across both of them (issue #118, D26).
//!
//! Nothing in this repository had ever run two active persistd gateways at
//! once. `persistd`'s own module header calls the binary a **single-node**
//! harness (`crates/orrery_persistd/src/bin/persistd.rs:3`), `Cluster` is an
//! in-process fixture with no node-to-node transport
//! (`crates/orrery_persistd/src/cluster.rs:4`), and the only two-process
//! topology is a primary plus a passive journal follower over the *same* shard
//! set, where `TopologyRole::Follower` is documented as "never a gateway"
//! (`bin/persistd.rs:1634`). This tool is the observable baseline that was
//! missing: two persistd processes, disjoint `--shard` subtrees of one grid,
//! one coordinator, one FoundationDB cluster.
//!
//! ## What it takes no position on
//!
//! **Placement.** Under D26 rule 1 the durable `actor/{grid}/{shard}` row is
//! the single ownership rule and `--shard` is verbatim; HRW is a non-binding
//! planner and appears nowhere in a serving or routing path. Here the shard
//! split is two lists of flags handed to two processes by the gate script —
//! which is exactly why this slice did not have to wait on the ownership
//! decision — and the routing this harness does is read back from what each
//! gateway *activated* and printed on its readiness line, never from the flags
//! the script hoped it would take.
//!
//! **Cross-gateway succession.** D26 rule 4: a successor is never selected on
//! a sibling gateway. Each registrar redistributes only among the sessions it
//! can see, so an entity whose only remaining interested peers were on the
//! other gateway *parks*, and that is the specified behaviour rather than a
//! defect. The report counts a park and a reassignment alike, as the P3
//! criterion does.
//!
//! ## Why the summed counter is the point
//!
//! `observe_fencing_rejection` (`gateway.rs`) and `AuthorityMetrics` are
//! per-`GatewayServer`, and nothing in the tree aggregates them. Two
//! independent per-process zeroes are not the fleet-wide statement the
//! invariant needs, so `duplicate_authority` in this report is the **sum**
//! over both gateways' `--metrics-jsonl` exports, and the per-gateway halves
//! are carried beside it so the sum can be checked.
//!
//! ## The two kills
//!
//! 1. **A peer.** One peer is SIGKILLed and every row it held — on either
//!    gateway — must be reassigned or parked inside the settle budget, with
//!    nothing lost and the summed duplicate counter at zero. This is P3's
//!    criterion restated over a fleet.
//! 2. **A gateway.** One of the two persistd processes is SIGKILLed and the
//!    *survivor's* rows must be untouched: no lease of its expires, none is
//!    duplicated, and its disposition counters do not move. This is the case
//!    that distinguishes a real partition of authority from two processes that
//!    merely coexist — with a shared fence and a shared lease tier, a gateway
//!    that answered for its sibling's shards, or a registrar whose expiry
//!    sweep swept rows it did not own, would show up here and nowhere else.

// `serde_json::json!` expands one nested macro call per key, and the report
// below is deliberately wide: every clause of both criteria, plus the
// per-gateway halves of the summed invariant. The default 128 is not enough
// for it.
#![recursion_limit = "512"]

mod peer;
mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use orrery_protocol::{
    shard_of, AccountId, CellId, ClaimBasis, ClaimId, ClaimKind, DenyReason, GatewayMsg,
    GatewayReply, GridId, IssuerKeyId, LeaseMsg, NodeId, PersistId, SeqPair, SessionStanding,
    SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, Tick, UnixMillis,
};

use crate::peer::{PeerEvent, PeerSpec, Row, Side};
use crate::wire::Session;

/// The peer the criterion kills. Its rows straddle both gateways by
/// construction, which is what makes the settle clause a fleet-wide one.
const VICTIM_INDEX: usize = 0;

/// D16 lease TTL: the window both criteria are measured against.
const LEASE_TTL: Duration = Duration::from_secs(10);
/// The registrar's expiry sweep period (`gateway.rs`'s accept loop).
const REGISTRAR_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// persistd's `METRICS_REPORT_INTERVAL`: the lag of the only instrument that
/// can see a park.
const METRICS_EXPORT_INTERVAL: Duration = Duration::from_secs(1);
/// How often the settle loop re-reads the peer logs and both metrics files.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Slack added to the TTL before either criterion is judged to have failed.
///
/// The same three granularity terms as the single-gateway island gate, for the
/// same reasons: the once-a-second expiry sweep is when a lapsed lease is
/// *noticed*, the once-a-second metrics export is when a park becomes
/// *visible*, and this loop's poll is when a visible park is *read*. Nothing
/// here is fudge — each is a quantum some instrument between the disposition
/// and this process rounds up to.
const SETTLE_GRANULARITY: Duration = Duration::from_millis(
    REGISTRAR_SWEEP_INTERVAL.as_millis() as u64
        + METRICS_EXPORT_INTERVAL.as_millis() as u64
        + SETTLE_POLL_INTERVAL.as_millis() as u64,
);

/// How long the harness waits, after the clock has stopped, for both
/// registrars' exported counters to account for every one of the victim's rows.
const ATTESTATION_WAIT: Duration = Duration::from_secs(5);

/// The claim tier a peer asks for, as a CLI value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ClaimTier {
    /// The contested-physics case, which redistributes.
    Weak,
    /// The case D7 §5 refuses to redistribute without consent, which parks.
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
    name = "p3-siblings",
    about = "Two sibling persistd gateways over disjoint shards: the single-writer invariant, summed"
)]
struct Cli {
    /// Gateway A's `bind_addr` from its readiness line.
    #[arg(long, value_name = "IP:PORT")]
    gateway_a_addr: String,
    /// Gateway A's `node_id` from its readiness line.
    #[arg(long, value_name = "NODE_ID")]
    gateway_a_node: String,
    /// Gateway A's `--metrics-jsonl` file.
    #[arg(long, value_name = "PATH")]
    metrics_a: Option<PathBuf>,
    /// The shard cells gateway A activated, one per line.
    ///
    /// The gate script writes this from gateway A's own readiness line, not
    /// from the `--shard` flags it passed: D26 rule 1 makes the durable
    /// `actor/{grid}/{shard}` row the ownership rule, and a process's flags
    /// and its activation are two different facts.
    #[arg(long, value_name = "PATH")]
    shards_a: Option<PathBuf>,

    /// Gateway B's `bind_addr`.
    #[arg(long, value_name = "IP:PORT")]
    gateway_b_addr: String,
    /// Gateway B's `node_id`.
    #[arg(long, value_name = "NODE_ID")]
    gateway_b_node: String,
    /// Gateway B's `--metrics-jsonl` file.
    #[arg(long, value_name = "PATH")]
    metrics_b: Option<PathBuf>,
    /// The shard cells gateway B activated, one per line.
    #[arg(long, value_name = "PATH")]
    shards_b: Option<PathBuf>,
    /// Gateway B's process id: the second `kill -9` this harness issues.
    ///
    /// The kill is issued here rather than by the gate script because the
    /// clause is about *when* the survivor's leases did not end, and the only
    /// process that can subtract two instants from one clock is this one.
    #[arg(long, value_name = "PID")]
    gateway_b_pid: Option<u32>,

    /// The coordinator's `bind_addr`.
    #[arg(long, value_name = "IP:PORT")]
    coordinator_addr: String,
    /// The coordinator's `node_id`.
    #[arg(long, value_name = "NODE_ID")]
    coordinator_node: String,

    /// Hex-encoded identity issuer secret; its public half must be both
    /// gateways' `--issuer-key`.
    #[arg(long, value_name = "HEX")]
    issuer_secret: String,

    /// The seeder manifest (`orrery-seed verify --emit-manifest`).
    ///
    /// A durable world is not optional here: two gateways sharing one fence
    /// and one lease tier is the whole topology, `--dev-seed` refuses to run
    /// with `--fdb-cluster-file` set (`bin/persistd.rs`), and a registrar only
    /// grants a lease for an entity whose committed cell it can resolve.
    #[arg(long, value_name = "PATH")]
    manifest: Option<PathBuf>,

    /// Peers in the island. The criterion's number is 8.
    #[arg(long, default_value_t = 8)]
    peers: u8,

    /// The tier the victim claims its rows at.
    #[arg(long, value_enum, default_value_t = ClaimTier::Weak)]
    victim_claim_kind: ClaimTier,

    /// How long peers keep simulating. Must outlast both settle windows.
    #[arg(long, default_value_t = 75)]
    duration_secs: u64,

    /// Where to put peer logs and the report.
    #[arg(long, value_name = "DIR", default_value = "p3-sibling-out")]
    out: PathBuf,

    /// Print the public half of the identity issuer secret as JSON and exit.
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
    if let Some(path) = cli.peer_spec.clone() {
        let spec: PeerSpec = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("read peer spec {}", path.display()))?;
        return runtime.block_on(peer::run(spec));
    }
    let outcome = runtime.block_on(orchestrate(cli))?;
    println!("{}", serde_json::to_string_pretty(&outcome.report)?);
    if outcome.passed {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

struct Outcome {
    passed: bool,
    report: serde_json::Value,
}

/// The registrar authority counters one gateway last exported.
#[derive(Debug, Clone, Copy, Default)]
struct AuthorityCounters {
    duplicate: u64,
    reassigned: u64,
    parked: u64,
}

impl AuthorityCounters {
    /// The fleet-wide reading: two per-process registrars, one statement.
    fn sum(a: Self, b: Self) -> Self {
        Self {
            duplicate: a.duplicate + b.duplicate,
            reassigned: a.reassigned + b.reassigned,
            parked: a.parked + b.parked,
        }
    }

    fn minus(self, baseline: Self) -> Self {
        Self {
            duplicate: self.duplicate.saturating_sub(baseline.duplicate),
            reassigned: self.reassigned.saturating_sub(baseline.reassigned),
            parked: self.parked.saturating_sub(baseline.parked),
        }
    }
}

/// Read the highest total one persistd has reported for each authority counter.
///
/// Every counter is an absolute total rather than an interval delta, so taking
/// the maximum is the same statement as taking the last line — and is robust
/// to a record being appended while this read is in flight, and to the file of
/// a gateway that has been `kill -9`ed simply stopping.
fn read_authority_counters(path: &Path) -> AuthorityCounters {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return AuthorityCounters::default();
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
    }
    counters
}

/// One entry of the seeder manifest (docs/12-world-seeding.md §9.3).
#[derive(Debug, serde::Deserialize)]
struct ManifestEntry {
    persist_id: u64,
    cell: u64,
    grid: Option<u32>,
}

/// The `(entity, cell)` inventory the seeder committed, root grid only.
fn read_manifest(path: &Path) -> Result<Vec<(u64, u64)>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read manifest {}", path.display()))?;
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The `content/version` header line has no `persist_id`; a failed
        // decode is how it is skipped, and it is the only line that does.
        let Ok(entry) = serde_json::from_str::<ManifestEntry>(line) else {
            continue;
        };
        if entry.grid.is_some_and(|grid| grid != 0) {
            continue;
        }
        rows.push((entry.persist_id, entry.cell));
    }
    anyhow::ensure!(
        !rows.is_empty(),
        "manifest {} named no rows",
        path.display()
    );
    rows.sort_unstable();
    Ok(rows)
}

/// The shard cells one gateway reported activating.
///
/// Read rather than re-derived, and that is load-bearing twice over. A harness
/// that computed its own split would agree with itself and disagree with the
/// processes, and every misrouted claim would come back `WrongOwner` from a
/// gateway configured exactly as asked. And the list comes from each gateway's
/// readiness line rather than from the flags it was given, because D26 rule 1
/// makes the durable `actor/{grid}/{shard}` row the ownership rule — the flags
/// are a request, the activation is the answer.
fn read_shards(path: &Path) -> Result<BTreeSet<u64>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read shards {}", path.display()))?;
    let mut shards = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let bits = if let Some(hex) = line.strip_prefix("0x").or_else(|| line.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).with_context(|| format!("shard hex `{line}`"))?
        } else {
            line.parse::<u64>()
                .with_context(|| format!("shard `{line}`"))?
        };
        shards.insert(bits);
    }
    anyhow::ensure!(!shards.is_empty(), "{} listed no shards", path.display());
    Ok(shards)
}

async fn orchestrate(cli: Cli) -> Result<Outcome> {
    std::fs::create_dir_all(&cli.out)
        .with_context(|| format!("create output directory {}", cli.out.display()))?;
    let issuer = iroh::SecretKey::from_bytes(&decode_key(&cli.issuer_secret)?);
    anyhow::ensure!(
        cli.peers >= 4,
        "redistribution needs survivors on both sides"
    );

    let manifest = cli
        .manifest
        .as_deref()
        .context("--manifest is required: a durable world is what two gateways share")?;
    let metrics_a = cli
        .metrics_a
        .as_deref()
        .context("--metrics-a is required: half the criterion lives in gateway A's counters")?;
    let metrics_b = cli
        .metrics_b
        .as_deref()
        .context("--metrics-b is required: the summed invariant needs gateway B's counters too")?;
    let shards_a = read_shards(
        cli.shards_a
            .as_deref()
            .context("--shards-a is required: routing must come from the flags A was given")?,
    )?;
    let shards_b = read_shards(
        cli.shards_b
            .as_deref()
            .context("--shards-b is required: routing must come from the flags B was given")?,
    )?;
    anyhow::ensure!(
        shards_a.is_disjoint(&shards_b),
        "the two shard sets overlap; this harness is about *disjoint* subtrees of one grid"
    );

    let settle_budget = LEASE_TTL + SETTLE_GRANULARITY;
    // Two settle windows, an attestation wait, and the claim storm all have to
    // fit inside the peers' lifetime: a peer that exits mid-window stops
    // writing the log the accounting reads, and its own leases park.
    let needed = settle_budget * 2 + ATTESTATION_WAIT + Duration::from_secs(15);
    anyhow::ensure!(
        Duration::from_secs(cli.duration_secs) > needed,
        "--duration-secs must exceed {}s: two settle budgets, the attestation wait and the claim storm",
        needed.as_secs()
    );

    // ── Routing ─────────────────────────────────────────────────────────
    // Every seeded row is addressed to the gateway whose shard set covers the
    // *shard* its cell collapses to. `shard_of` is the canonical collapse
    // (`orrery_protocol::cell`), never re-implemented here.
    let mut rows = Vec::new();
    let mut unrouted = 0usize;
    for (entity, cell_bits) in read_manifest(manifest)? {
        let Some(cell) = CellId::from_bits(cell_bits) else {
            continue;
        };
        let shard = shard_of(cell).to_bits();
        let side = if shards_a.contains(&shard) {
            Side::A
        } else if shards_b.contains(&shard) {
            Side::B
        } else {
            unrouted += 1;
            continue;
        };
        rows.push(Row {
            entity,
            cell: cell_bits,
            side,
        });
    }
    anyhow::ensure!(
        unrouted == 0,
        "{unrouted} seeded rows collapse to a shard neither gateway was given; the split does not cover the world"
    );
    let rows_a = rows.iter().filter(|row| row.side == Side::A).count();
    let rows_b = rows.len() - rows_a;
    anyhow::ensure!(
        rows_a > 0 && rows_b > 0,
        "the shard split put every row on one gateway ({rows_a}/{rows_b}); there is no sibling topology to prove"
    );

    // ── Interest zones ──────────────────────────────────────────────────
    // Two overlapping zones, each interleaved through the sorted row order so
    // that **both** zones straddle **both** shard sets. Two things ride on
    // this and neither is cosmetic:
    //
    //   - a peer's rows must span both gateways, or the settle clause is two
    //     single-gateway statements again;
    //   - a lost lease can only be reassigned to a peer with
    //     coordinator-confirmed interest in its cell, so the victim's zone
    //     must contain survivors. Disjoint per-peer interest would park
    //     everything — a correct outcome the criterion accepts, and a much
    //     weaker run than the one this gate is for.
    //
    // `MAX_INTEREST_GRANT_CELLS` is 64 (`orrery_protocol::coord`), which is
    // the ceiling on a zone and therefore on the world this harness can drive
    // at a given peer count.
    let zones = 2usize;
    anyhow::ensure!(
        usize::from(cli.peers) >= zones * 2,
        "each zone needs a victim and at least one survivor"
    );
    let mut zone_rows: Vec<Vec<Row>> = vec![Vec::new(); zones];
    for (index, row) in rows.iter().enumerate() {
        zone_rows[index % zones].push(*row);
    }
    for (index, zone) in zone_rows.iter().enumerate() {
        let cells: BTreeSet<u64> = zone.iter().map(|row| row.cell).collect();
        anyhow::ensure!(
            cells.len() <= orrery_protocol::MAX_INTEREST_GRANT_CELLS,
            "zone {index} spans {} cells, over the {} an interest grant may cover; seed a smaller world or raise --peers",
            cells.len(),
            orrery_protocol::MAX_INTEREST_GRANT_CELLS
        );
        anyhow::ensure!(
            zone.iter().any(|row| row.side == Side::A)
                && zone.iter().any(|row| row.side == Side::B),
            "zone {index} does not straddle both gateways"
        );
    }

    // ── Peers ───────────────────────────────────────────────────────────
    let mut children = Vec::new();
    let mut logs = Vec::new();
    let mut peer_nodes = Vec::new();
    let mut peer_zone = Vec::new();
    for index in 0..cli.peers {
        let zone = usize::from(index) % zones;
        let within = usize::from(index) / zones;
        let peers_in_zone = (usize::from(cli.peers) + zones - 1 - zone) / zones;
        let claimed: Vec<Row> = zone_rows[zone]
            .iter()
            .enumerate()
            .filter(|(position, _)| position % peers_in_zone == within)
            .map(|(_, row)| *row)
            .collect();
        let secret = peer_secret(index);
        let node = secret.public();
        let log = cli.out.join(format!("peer-{index}.jsonl"));
        let spec_path = cli.out.join(format!("peer-{index}.json"));
        let spec = PeerSpec {
            gateway_a_addr: cli.gateway_a_addr.clone(),
            gateway_a_node: cli.gateway_a_node.clone(),
            gateway_b_addr: cli.gateway_b_addr.clone(),
            gateway_b_node: cli.gateway_b_node.clone(),
            coordinator_addr: cli.coordinator_addr.clone(),
            coordinator_node: cli.coordinator_node.clone(),
            secret: encode_hex(&secret.to_bytes()),
            token: encode_hex(&mint_token(&issuer, node)?),
            zone_rows: zone_rows[zone].clone(),
            rows: claimed,
            kind: if usize::from(index) == VICTIM_INDEX {
                cli.victim_claim_kind.into()
            } else {
                ClaimKind::Weak
            },
            duration_secs: cli.duration_secs,
            log: log.clone(),
        };
        let expected = spec.rows.len();
        std::fs::write(&spec_path, serde_json::to_vec(&spec)?)?;

        let child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("--peer-spec")
            .arg(&spec_path)
            // The orchestrator's own required flags are irrelevant in peer
            // mode but still parsed, so echo them through.
            .args(["--gateway-a-addr", &cli.gateway_a_addr])
            .args(["--gateway-a-node", &cli.gateway_a_node])
            .args(["--gateway-b-addr", &cli.gateway_b_addr])
            .args(["--gateway-b-node", &cli.gateway_b_node])
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
            .context("spawn sibling peer")?;
        children.push(child);
        logs.push((log, expected));
        peer_nodes.push(node);
        peer_zone.push(zone);
    }

    for (index, (log, expected)) in logs.iter().enumerate() {
        wait_for_claims(log, *expected, Duration::from_secs(90))
            .await
            .with_context(|| format!("peer {index} never finished claiming"))?;
    }
    // A claim refused as `WrongOwner` is a misrouted harness, and it is the
    // one failure #117 exists to make legible: it says the address was wrong,
    // not that the claimant was ineligible. Counted before anything is killed,
    // because a run that started misrouted measures nothing afterwards.
    let mut misrouted = 0usize;
    let mut refused: Vec<String> = Vec::new();
    for (log, _) in &logs {
        for event in read_events(log) {
            if let PeerEvent::Denied { entity, reason, .. } = event {
                if reason.starts_with("WrongOwner") {
                    misrouted += 1;
                }
                refused.push(format!("{entity}:{reason}"));
            }
        }
    }
    tracing::info!(
        peers = cli.peers,
        rows = rows.len(),
        rows_a,
        rows_b,
        "sibling island formed"
    );

    // ── The misroute probe ──────────────────────────────────────────────
    // A deliberately wrong address, so the instrument the rest of the run
    // relies on is shown to be live *in this run*. Without it, "no claim came
    // back WrongOwner" is equally consistent with perfect routing and with a
    // gateway that lost the check entirely.
    //
    // Its interest is **one cell**, and its session is dropped the moment it
    // has its answer. Both are corrections rather than caution. A probe is a
    // peer like any other from the registrar's side, so a probe holding
    // coordinator-confirmed interest over a whole zone is an eligible
    // successor for every row in it — and in the first run of this gate it won
    // twelve of the victim's thirteen rows, none of which it could report,
    // leaving the harness reading one reassignment against thirteen the
    // registrars had attested. An instrument that can be picked as a successor
    // is not observing the redistribution; it is participating in it.
    let mut claim_id = 1u64;
    let misroute_target = rows
        .iter()
        .find(|row| row.side == Side::B)
        .copied()
        .context("no B-side row to misaddress")?;
    let misroute_cell = CellId::from_bits(misroute_target.cell).context("probe cell")?;
    let misroute_reply = {
        let (_coordinator, session) = probe_session(
            &issuer,
            0xFE,
            &[misroute_cell],
            &cli.coordinator_node,
            &cli.coordinator_addr,
            &cli.gateway_a_node,
            &cli.gateway_a_addr,
        )
        .await
        .context("misroute probe session")?;
        let reply = probe_claim(
            &session,
            ClaimId(claim_id),
            PersistId::new(misroute_target.entity),
            misroute_cell,
        )
        .await;
        claim_id += 1;
        reply
    };
    let wrong_owner_probe = matches!(
        misroute_reply,
        ProbeReply::Denied(DenyReason::WrongOwner { .. })
    );
    tracing::info!(
        entity = misroute_target.entity,
        ?misroute_reply,
        wrong_owner_probe,
        "misroute probe: a B-side row addressed to gateway A"
    );

    // ── Kill 1: a peer ──────────────────────────────────────────────────
    let victim_rows: BTreeMap<u64, Side> = read_events(&logs[VICTIM_INDEX].0)
        .into_iter()
        .filter_map(|event| match event {
            PeerEvent::Claimed { entity, side, .. } | PeerEvent::Inherited { entity, side, .. } => {
                Some((entity, side))
            }
            _ => None,
        })
        .collect();
    anyhow::ensure!(
        !victim_rows.is_empty(),
        "the victim held nothing; there is no redistribution to prove"
    );
    let victim_a = victim_rows.values().filter(|s| **s == Side::A).count();
    let victim_b = victim_rows.len() - victim_a;
    anyhow::ensure!(
        victim_a > 0 && victim_b > 0,
        "the victim's rows ({victim_a}/{victim_b}) do not straddle both gateways"
    );
    let victim_node = peer_nodes[VICTIM_INDEX];
    let victim_pid = children[VICTIM_INDEX]
        .id()
        .context("victim process has already exited")?;

    let baseline = AuthorityCounters::sum(
        read_authority_counters(metrics_a),
        read_authority_counters(metrics_b),
    );
    sigkill(victim_pid);
    tracing::warn!(
        pid = victim_pid,
        rows = victim_rows.len(),
        "kill -9 sent to the victim peer"
    );
    let killed_at = tokio::time::Instant::now();
    let killed_at_unix_ms = unix_ms();

    let mut inherited: BTreeMap<u64, u64> = BTreeMap::new();
    let mut inherited_by: BTreeMap<u64, usize> = BTreeMap::new();
    let mut parked_observed_in_ms: Option<u64> = None;
    let mut parked_delta = 0u64;
    let mut survivor_losses: Vec<u64> = Vec::new();
    let settled = loop {
        survivor_losses.clear();
        for (index, (log, _)) in logs.iter().enumerate() {
            if index == VICTIM_INDEX {
                continue;
            }
            for event in read_events(log) {
                match event {
                    PeerEvent::Inherited { entity, at_ms, .. }
                        if victim_rows.contains_key(&entity) =>
                    {
                        inherited
                            .entry(entity)
                            .or_insert_with(|| at_ms.saturating_sub(killed_at_unix_ms));
                        inherited_by.insert(entity, index);
                    }
                    PeerEvent::Lost { entity, .. } if !victim_rows.contains_key(&entity) => {
                        survivor_losses.push(entity);
                    }
                    _ => {}
                }
            }
        }
        let outstanding = victim_rows
            .keys()
            .filter(|entity| !inherited.contains_key(entity))
            .count();
        if outstanding == 0 {
            break true;
        }
        parked_delta = AuthorityCounters::sum(
            read_authority_counters(metrics_a),
            read_authority_counters(metrics_b),
        )
        .minus(baseline)
        .parked;
        if parked_delta >= outstanding as u64 {
            parked_observed_in_ms = Some(killed_at.elapsed().as_millis() as u64);
            break true;
        }
        if killed_at.elapsed() >= settle_budget {
            break false;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    };
    let reassigned_in_ms = inherited.values().copied().max().unwrap_or(0);
    let settled_in_ms = if settled {
        reassigned_in_ms.max(parked_observed_in_ms.unwrap_or(0))
    } else {
        killed_at.elapsed().as_millis() as u64
    };

    // Both registrars' account of the same window, waited for rather than
    // snatched: between them they must record a disposition for every row the
    // victim held, on whichever gateway held it.
    let attestation_deadline = tokio::time::Instant::now() + ATTESTATION_WAIT;
    let final_counters = loop {
        let counters = AuthorityCounters::sum(
            read_authority_counters(metrics_a),
            read_authority_counters(metrics_b),
        );
        let attested = counters.minus(baseline);
        if attested.reassigned + attested.parked >= victim_rows.len() as u64
            || tokio::time::Instant::now() >= attestation_deadline
        {
            break counters;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    };
    let disposed = final_counters.minus(baseline);

    // Anything with no disposition of its own still has to exist and still
    // answer. A weak claim over a weak-held row is granted even against a live
    // holder (`lease.rs::claim`), so a grant proves existence and nothing
    // about parking — which is why the parked figure above is the registrars'
    // counter and never this probe's grant count. It runs only now, after the
    // clock has stopped, because a claim issued while the victim's lease could
    // still be live would settle by hand what it claims to observe.
    //
    // **It runs in two halves, and the split is not cosmetic.** A probe claim
    // takes a real lease that the probe never heartbeats, so it lapses one TTL
    // later and the registrar parks it — a disposition this harness caused,
    // landing in exactly the counters the gateway-kill clause below reads. The
    // first run of this gate reported six such parks against the survivor and
    // failed itself for them. So the B-side rows are probed *before* gateway B
    // is killed, because afterwards nothing can answer for them at all, and
    // the A-side rows are probed *after* the survivor's observation window has
    // closed, so no lease this harness took can move the numbers that window
    // is about.
    let mut claimable = Vec::new();
    let mut lost = Vec::new();
    let undisposed = |side: Side| -> Vec<(u64, CellId)> {
        victim_rows
            .iter()
            .filter(|(entity, at)| **at == side && !inherited.contains_key(entity))
            .filter_map(|(entity, _)| {
                rows.iter()
                    .find(|row| row.entity == *entity)
                    .and_then(|row| CellId::from_bits(row.cell))
                    .map(|cell| (*entity, cell))
            })
            .collect()
    };
    probe_for_loss(
        &issuer,
        0xFD,
        &undisposed(Side::B),
        &cli.coordinator_node,
        &cli.coordinator_addr,
        &cli.gateway_b_node,
        &cli.gateway_b_addr,
        &mut claim_id,
        &mut claimable,
        &mut lost,
    )
    .await?;

    // ── Kill 2: a gateway ───────────────────────────────────────────────
    // The clause that distinguishes a partition of authority from coexistence.
    // Gateway B is SIGKILLed; gateway A's rows must be untouched — no lease of
    // its expires, none is duplicated, and its disposition counters do not
    // move for a full settle budget, which is longer than the TTL any of those
    // leases is renewed against.
    //
    // Note what the peers do *not* do here, because it is what gives the
    // clause teeth rather than a flaw in it: QUIC cannot distinguish a dead
    // process from a dead path until its own idle timeout, so a peer whose
    // gateway was `kill -9`ed goes on heartbeating into the void for longer
    // than this window lasts. `peers_that_noticed` is therefore usually zero,
    // and it is reported rather than asserted. The survivor is under no less
    // load for it, which is the point: nothing about losing a sibling may
    // reach the rows it owns.
    let survivor = Side::A;
    let doomed = survivor.other();
    let held_before = held_on(&logs, survivor, VICTIM_INDEX);
    let a_before = read_authority_counters(metrics_a);
    let gateway_killed_at_unix_ms = unix_ms();
    let gateway_kill_issued = if let Some(pid) = cli.gateway_b_pid {
        sigkill(pid);
        tracing::warn!(pid, ?doomed, "kill -9 sent to the sibling gateway");
        true
    } else {
        false
    };
    if gateway_kill_issued {
        tokio::time::sleep(settle_budget).await;
    }
    let held_after = held_on(&logs, survivor, VICTIM_INDEX);
    let a_after = read_authority_counters(metrics_a);
    let a_moved = a_after.minus(a_before);
    let survivor_expiries_after_kill: Vec<u64> = logs
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != VICTIM_INDEX)
        .flat_map(|(_, (log, _))| read_events(log))
        .filter_map(|event| match event {
            PeerEvent::Lost {
                entity,
                side,
                at_ms,
                ..
            } if side == survivor && at_ms >= gateway_killed_at_unix_ms => Some(entity),
            _ => None,
        })
        .collect();
    let noticed_gateway_gone = logs
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != VICTIM_INDEX)
        .filter(|(_, (log, _))| {
            read_events(log)
                .into_iter()
                .any(|event| matches!(event, PeerEvent::GatewayGone { side, .. } if side == doomed))
        })
        .count();

    probe_for_loss(
        &issuer,
        0xFC,
        &undisposed(Side::A),
        &cli.coordinator_node,
        &cli.coordinator_addr,
        &cli.gateway_a_node,
        &cli.gateway_a_addr,
        &mut claim_id,
        &mut claimable,
        &mut lost,
    )
    .await?;

    let duplicate_a = read_authority_counters(metrics_a).duplicate;
    let duplicate_b = read_authority_counters(metrics_b).duplicate;
    let duplicate_authority = duplicate_a + duplicate_b;

    for mut child in children {
        let _ = child.kill().await;
    }

    // Every clause, and no others.
    let gateway_kill_clean = gateway_kill_issued
        && survivor_expiries_after_kill.is_empty()
        && a_moved.reassigned == 0
        && a_moved.parked == 0
        && a_moved.duplicate == 0
        && held_after >= held_before;
    let passed = settled
        && misrouted == 0
        && wrong_owner_probe
        && disposed.reassigned + disposed.parked >= victim_rows.len() as u64
        && settled_in_ms <= settle_budget.as_millis() as u64
        && lost.is_empty()
        && duplicate_authority == 0
        && survivor_losses.is_empty()
        && gateway_kill_clean;

    let report = serde_json::json!({
        "peers": cli.peers,
        "entities_total": rows.len(),
        "entities_gateway_a": rows_a,
        "entities_gateway_b": rows_b,
        "shards_gateway_a": shards_a.len(),
        "shards_gateway_b": shards_b.len(),
        "victim_node": victim_node.to_string(),
        "victim_claim_kind": format!("{:?}", ClaimKind::from(cli.victim_claim_kind)),
        "victim_entities": victim_rows.len(),
        "victim_entities_gateway_a": victim_a,
        "victim_entities_gateway_b": victim_b,
        "reassigned": inherited.len(),
        "reassigned_attested": disposed.reassigned,
        "successors": inherited_by.values().collect::<BTreeSet<_>>().len(),
        "parked": disposed.parked,
        "parked_when_clock_stopped": parked_delta,
        "dispositions_attested": disposed.reassigned + disposed.parked,
        "claimable_after_settle": claimable.len(),
        "lost": lost,
        "settled": settled,
        "settled_in_ms": settled_in_ms,
        "reassigned_in_ms": reassigned_in_ms,
        "parked_observed_in_ms": parked_observed_in_ms,
        "park_observation_lag_ms": (METRICS_EXPORT_INTERVAL + SETTLE_POLL_INTERVAL).as_millis() as u64,
        "lease_ttl_ms": LEASE_TTL.as_millis() as u64,
        "settle_budget_ms": settle_budget.as_millis() as u64,
        // The fleet-wide statement: one number, summed over both registrars,
        // with the halves beside it so the sum can be checked rather than
        // trusted. Nothing in the tree aggregates `AuthorityMetrics` today —
        // it is per-`GatewayServer` — which is exactly why this is here.
        "duplicate_authority": duplicate_authority,
        "duplicate_authority_gateway_a": duplicate_a,
        "duplicate_authority_gateway_b": duplicate_b,
        "survivor_leases_lost": survivor_losses.len(),
        // #117's instrument, exercised in this run rather than assumed: a
        // deliberately misaddressed claim must come back `WrongOwner`, and no
        // routed claim may.
        "misrouted_claims": misrouted,
        "wrong_owner_probe": wrong_owner_probe,
        "claim_refusals": refused,
        "gateway_kill": {
            "issued": gateway_kill_issued,
            "killed": format!("{doomed:?}"),
            "surviving": format!("{survivor:?}"),
            "observation_ms": if gateway_kill_issued { settle_budget.as_millis() as u64 } else { 0 },
            "peers_that_noticed": noticed_gateway_gone,
            "survivor_leases_held_before": held_before,
            "survivor_leases_held_after": held_after,
            "survivor_leases_expired_after": survivor_expiries_after_kill.len(),
            "survivor_reassigned_after": a_moved.reassigned,
            "survivor_parked_after": a_moved.parked,
            "survivor_duplicate_after": a_moved.duplicate,
            "clean": gateway_kill_clean,
        },
        "passed": passed,
    });
    Ok(Outcome { passed, report })
}

/// How many leases the survivors currently hold on one gateway, from their
/// own logs: claimed or inherited there, and not since withdrawn.
fn held_on(logs: &[(PathBuf, usize)], side: Side, skip: usize) -> usize {
    let mut held: BTreeSet<(usize, u64)> = BTreeSet::new();
    for (index, (log, _)) in logs.iter().enumerate() {
        if index == skip {
            continue;
        }
        for event in read_events(log) {
            match event {
                PeerEvent::Claimed {
                    entity, side: at, ..
                }
                | PeerEvent::Inherited {
                    entity, side: at, ..
                } if at == side => {
                    held.insert((index, entity));
                }
                PeerEvent::Lost {
                    entity, side: at, ..
                } if at == side => {
                    held.remove(&(index, entity));
                }
                _ => {}
            }
        }
    }
    held.len()
}

/// Open one short-lived probe identity: a coordinator session, the interest it
/// grants over exactly `cells`, and a gateway session presenting it.
///
/// Short-lived and narrow on purpose. A probe is an ordinary peer as far as
/// the registrar is concerned, so one that is connected while a lease is being
/// redistributed, over interest wider than the row it is asking about, is an
/// eligible successor — and a successor that reports nothing. Both halves are
/// returned so the caller can drop them together the moment it has its answer;
/// the coordinator client is otherwise unused, and dropping it first would
/// take the island membership the grant was minted against with it.
#[allow(clippy::too_many_arguments)]
async fn probe_session(
    issuer: &iroh::SecretKey,
    seed: u8,
    cells: &[CellId],
    coordinator_node: &str,
    coordinator_addr: &str,
    gateway_node: &str,
    gateway_addr: &str,
) -> Result<(orrery_coordinator::CoordinatorClient, Session)> {
    let secret = peer_secret(seed);
    let node = secret.public();
    let token = mint_token(issuer, node)?;
    let coordinator = orrery_coordinator::CoordinatorClient::connect(
        secret.clone(),
        endpoint_addr(coordinator_node, coordinator_addr)?,
        token.clone(),
        Duration::from_secs(10),
    )
    .await
    .map_err(|error| anyhow::anyhow!("probe coordinator session: {error}"))?;
    coordinator
        .report_presence(cells.to_vec())
        .map_err(|error| anyhow::anyhow!("probe presence: {error}"))?;
    let grant = coordinator
        .next_grant(Duration::from_secs(15))
        .await
        .map_err(|error| anyhow::anyhow!("probe interest grant: {error}"))?;

    let session = Session::connect(secret, endpoint_addr(gateway_node, gateway_addr)?).await?;
    session.send_control(&GatewayMsg::Hello { token, node })?;
    anyhow::ensure!(
        matches!(
            session.recv(Duration::from_secs(10)).await,
            Some(GatewayReply::HelloAck { .. })
        ),
        "probe session was refused"
    );
    session.send_control(&GatewayMsg::InterestGrant { grant })?;
    anyhow::ensure!(
        matches!(
            session.recv(Duration::from_secs(10)).await,
            Some(GatewayReply::InterestAck { epoch: Some(_), .. })
        ),
        "probe interest grant was refused"
    );
    Ok((coordinator, session))
}

/// Ask one gateway whether each undisposed row still exists and still answers.
///
/// A grant proves existence and nothing about parking, which is why the parked
/// figure in the report is the registrars' own counter. A row that answers
/// nothing is the **lost entity** the phase exists to rule out.
#[allow(clippy::too_many_arguments)]
async fn probe_for_loss(
    issuer: &iroh::SecretKey,
    seed: u8,
    targets: &[(u64, CellId)],
    coordinator_node: &str,
    coordinator_addr: &str,
    gateway_node: &str,
    gateway_addr: &str,
    claim_id: &mut u64,
    claimable: &mut Vec<u64>,
    lost: &mut Vec<u64>,
) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    let cells: Vec<CellId> = targets
        .iter()
        .map(|(_, cell)| *cell)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (_coordinator, session) = probe_session(
        issuer,
        seed,
        &cells,
        coordinator_node,
        coordinator_addr,
        gateway_node,
        gateway_addr,
    )
    .await?;
    for (entity, cell) in targets {
        let reply = probe_claim(&session, ClaimId(*claim_id), PersistId::new(*entity), *cell).await;
        *claim_id += 1;
        match reply {
            ProbeReply::Granted => claimable.push(*entity),
            _ => lost.push(*entity),
        }
    }
    Ok(())
}

/// What one probe claim came back as.
#[derive(Debug)]
enum ProbeReply {
    /// The registrar granted a lease: the row exists and still answers.
    Granted,
    /// The registrar refused, and said why.
    Denied(DenyReason),
    /// Nothing came back inside the deadline.
    Silent,
}

/// Try to claim an entity, and report what the registrar said.
///
/// The tier stays `Weak` deliberately: a `Strong` claim against a live holder
/// is turned into a cooperative handoff request whose unanswered deadline
/// hands the entity over (D7 §4.2), which is a harness resolving the
/// redistribution rather than observing it.
async fn probe_claim(
    session: &Session,
    claim_id: ClaimId,
    entity: PersistId,
    cell: CellId,
) -> ProbeReply {
    if session
        .send_control(&GatewayMsg::Lease {
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
        })
        .is_err()
    {
        return ProbeReply::Silent;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(reply) = session.recv(remaining).await else {
            return ProbeReply::Silent;
        };
        match reply {
            GatewayReply::Lease {
                message:
                    LeaseMsg::Grant {
                        claim_id: answered, ..
                    },
            } if answered == claim_id => return ProbeReply::Granted,
            GatewayReply::Lease {
                message:
                    LeaseMsg::Deny {
                        claim_id: Some(answered),
                        reason,
                        ..
                    },
            } if answered == claim_id => return ProbeReply::Denied(reason),
            _ => {}
        }
    }
}

/// SIGKILL by pid, without pulling in a libc dependency for one call.
fn sigkill(pid: u32) {
    std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .ok();
}

/// Block until a peer's log shows it has answered for every row it was given.
async fn wait_for_claims(log: &Path, expected: usize, within: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let answered = read_events(log)
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

fn read_events(path: &Path) -> Vec<PeerEvent> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Wall-clock milliseconds, matching the stamp a peer puts on its events.
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

/// Build a dial document from a readiness line's `node_id` and `bind_addr`.
pub fn endpoint_addr(node: &str, socket: &str) -> Result<iroh::EndpointAddr> {
    let node = NodeId::from_str(node).context("node id")?;
    let socket: std::net::SocketAddr = socket.parse().context("socket address")?;
    Ok(iroh::EndpointAddr::from_parts(
        node,
        [iroh::TransportAddr::Ip(socket)],
    ))
}

/// Decode a 32-byte hex secret key.
pub fn decode_key(value: &str) -> Result<[u8; 32]> {
    decode_hex(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret keys are 32 bytes"))
}

/// Decode a hex string, permissively about case.
pub fn decode_hex(value: &str) -> Result<Vec<u8>> {
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

    /// The fleet-wide invariant is a **sum**, and the whole reason this
    /// harness exists is that nothing else computes one: `AuthorityMetrics`
    /// is per-`GatewayServer`, so two gateways reporting zero separately is
    /// not the same statement as the fleet reporting zero. A regression that
    /// read only one file would still pass every single-gateway assertion in
    /// this tree, which is exactly why the sum is pinned here.
    #[test]
    fn the_invariant_is_summed_over_both_gateways() {
        let a = AuthorityCounters {
            duplicate: 1,
            reassigned: 3,
            parked: 2,
        };
        let b = AuthorityCounters {
            duplicate: 0,
            reassigned: 4,
            parked: 5,
        };
        let sum = AuthorityCounters::sum(a, b);
        assert_eq!(
            sum.duplicate, 1,
            "a duplicate on one gateway is a duplicate"
        );
        assert_eq!(sum.reassigned, 7);
        assert_eq!(sum.parked, 7);
    }

    /// The parked half of the criterion rides one field name in persistd's
    /// export, per gateway. If that name moves, `parked` reads zero for every
    /// run and the gate silently becomes the reassign-only gate the P3 harness
    /// was fixed to stop being — while still passing a weak run, which is what
    /// would keep the regression invisible.
    #[test]
    fn counters_come_from_the_registrars_own_export() {
        let dir = std::env::temp_dir().join(format!("p3-sibling-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"gateway_bulk_latency\",\"parked_without_successor\":99}\n",
                "{\"type\":\"gateway_authority\",\"duplicate_authority\":0,",
                "\"reassigned\":3,\"parked_without_successor\":7}\n",
                // A gateway that was `kill -9`ed simply stops appending, and a
                // torn trailing line is what that looks like on disk. The read
                // takes the maximum, so neither walks a counter backwards.
                "{\"type\":\"gateway_authority\",\"reassigned\":1}\n",
                "{\"type\":\"gateway_auth\n",
            ),
        )
        .unwrap();
        let counters = read_authority_counters(&path);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(counters.parked, 7, "parked_without_successor not read");
        assert_eq!(counters.reassigned, 3, "reassigned walked backwards");
        assert_eq!(counters.duplicate, 0);
    }

    /// The budget must cover every instrument between a disposition and this
    /// harness learning of it, or a park that happened inside the TTL fails
    /// the gate on the harness's own reading cadence.
    #[test]
    fn the_settle_budget_covers_the_park_observation_lag() {
        let published_lag = METRICS_EXPORT_INTERVAL + SETTLE_POLL_INTERVAL;
        assert!(
            SETTLE_GRANULARITY >= REGISTRAR_SWEEP_INTERVAL + published_lag,
            "settle budget {SETTLE_GRANULARITY:?} does not cover sweep + {published_lag:?}"
        );
    }

    /// Routing is read from what the gateways reported activating, never
    /// re-derived: a harness that computed its own split would agree with
    /// itself and disagree with the processes. Both spellings the readiness
    /// line and the `--shard` flag accept must parse.
    #[test]
    fn shard_lists_parse_both_spellings() {
        let dir = std::env::temp_dir().join(format!("p3-sibling-shards-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shards.txt");
        std::fs::write(&path, "0x1FFFFFFFFFFF4A00\n\n2305843009213693952\n").unwrap();
        let shards = read_shards(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(shards.len(), 2);
        assert!(shards.contains(&0x1FFF_FFFF_FFFF_4A00));
        assert!(shards.contains(&2_305_843_009_213_693_952));
    }
}
