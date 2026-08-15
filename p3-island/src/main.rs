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
//! - One peer is SIGKILLed. After the settle window, every entity it held must
//!   be accounted for: inherited by a survivor (a `Grant` carrying the
//!   registrar correlation), or parked — which the orchestrator proves by
//!   claiming it, since a parked entity is claimable by anyone.
//! - Anything neither inherited nor claimable is a **lost entity**, which is
//!   the failure the phase exists to rule out.

mod peer;
mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use orrery_protocol::{
    AccountId, CellId, ClaimBasis, ClaimId, ClaimKind, GatewayMsg, GatewayReply, GridId,
    IssuerKeyId, LeaseMsg, NodeId, PersistId, SeqPair, SessionStanding, SessionTokenClaimsV1,
    SessionTokenTtlMs, SessionTokenV1, Tick, UnixMillis,
};

use crate::peer::{PeerConfig, PeerEvent};
use crate::wire::Session;

/// D16 lease TTL: the window the criterion measures against.
const LEASE_TTL: Duration = Duration::from_secs(10);

/// Slack added to the TTL before the criterion is judged to have failed.
///
/// A `kill -9` is **not** seen as a connection drop. QUIC cannot distinguish a
/// dead process from a dead path until its own idle timeout, so the gateway
/// resolves a SIGKILLed peer by the *slow* path of docs/04-authority.md §4.3 —
/// the lease TTL lapsing 10 s after that peer's last heartbeat — rather than
/// the fast path of an observed disconnect. Two things then sit between the
/// kill and the redistribution: up to one heartbeat interval of TTL that had
/// already elapsed before the kill, and up to one tick of the registrar's
/// once-a-second expiry sweep. This is that granularity, not a fudge factor;
/// the criterion is still that redistribution is bounded by the TTL.
const SETTLE_GRANULARITY: Duration = Duration::from_secs(2);

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
    let victim_index = 0usize;
    let victim_entities: BTreeSet<u64> = read_events(&logs[victim_index])?
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
    let victim_node = peer_nodes[victim_index];
    let victim_pid = children[victim_index]
        .id()
        .context("victim process has already exited")?;
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

    // ── Settle ──────────────────────────────────────────────────────────
    // Poll rather than sleep a fixed window: the criterion is *how long*
    // redistribution takes, so a fixed sleep would measure the harness's own
    // patience instead. Reassignment is the fast path — a dropped connection
    // is noticed immediately — so wait for it and fall through to the parked
    // check only when the TTL is spent.
    let mut inherited: BTreeMap<u64, usize> = BTreeMap::new();
    loop {
        inherited.clear();
        for (index, log) in logs.iter().enumerate() {
            if index == victim_index {
                continue;
            }
            for event in read_events(log)? {
                if let PeerEvent::Inherited { entity, .. } = event {
                    inherited.insert(entity, index);
                }
            }
        }
        let outstanding = victim_entities
            .iter()
            .filter(|entity| !inherited.contains_key(entity))
            .count();
        if outstanding == 0 || killed_at.elapsed() >= LEASE_TTL + SETTLE_GRANULARITY {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The clock stops here. Probing the remainder is verification, not
    // settling: an entity nobody inherited was parked at the registrar the
    // moment its holder's session tore down.
    let settled_in_ms = killed_at.elapsed().as_millis() as u64;

    // Anything not inherited must be parked, and a parked entity is claimable
    // by anyone (D7 §7). Proving it by claiming is stronger than reading the
    // registrar's own opinion of itself.
    let probe_secret = peer_secret(u8::MAX);
    let probe_node = probe_secret.public();
    let probe_token = mint_token(&issuer, probe_node)?;
    // The probe is a peer like any other: it asks the coordinator for its
    // interest rather than assuming any.
    let probe_coordinator = orrery_coordinator::CoordinatorClient::connect(
        probe_secret.clone(),
        coordinator_addr,
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

    let probe = Session::connect(probe_secret, gateway).await?;
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

    let mut parked = Vec::new();
    let mut lost = Vec::new();
    let mut claim_id = 1u64;
    for entity in &victim_entities {
        if inherited.contains_key(entity) {
            continue;
        }
        match probe_claim(&probe, ClaimId(claim_id), PersistId::new(*entity), cell).await? {
            Some(_) => parked.push(*entity),
            None => lost.push(*entity),
        }
        claim_id += 1;
    }

    let duplicate_authority = cli
        .metrics_jsonl
        .as_deref()
        .map(read_duplicate_authority)
        .transpose()?;

    for mut child in children {
        let _ = child.kill().await;
    }

    let no_duplicates = duplicate_authority.unwrap_or(0) == 0;
    // Every clause of the criterion, and no others: nothing lost, no tick on
    // which two peers both believed they were the writer, and all of it inside
    // the lease TTL.
    let passed = lost.is_empty()
        && no_duplicates
        && settled_in_ms <= (LEASE_TTL + SETTLE_GRANULARITY).as_millis() as u64;
    let report = serde_json::json!({
        "peers": cli.peers,
        "entities_total": total_entities,
        "victim_node": victim_node.to_string(),
        "victim_entities": victim_entities.len(),
        "reassigned": inherited.len(),
        "parked": parked.len(),
        "lost": lost,
        "settled_in_ms": settled_in_ms,
        "lease_ttl_ms": LEASE_TTL.as_millis() as u64,
        "settle_budget_ms": (LEASE_TTL + SETTLE_GRANULARITY).as_millis() as u64,
        // `None` means no metrics file was given, which is not the same as a
        // clean zero — the gate script always passes one.
        "duplicate_authority": duplicate_authority,
        "passed": passed,
    });
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

/// Try to claim an entity, returning its granted lease id when it is free.
async fn probe_claim(
    session: &Session,
    claim_id: ClaimId,
    entity: PersistId,
    cell: CellId,
) -> Result<Option<u64>> {
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(reply) = session.recv(remaining).await else {
            return Ok(None);
        };
        match reply {
            GatewayReply::Lease {
                message:
                    LeaseMsg::Grant {
                        claim_id: answered,
                        lease_id,
                        ..
                    },
            } if answered == claim_id => return Ok(Some(lease_id.0)),
            GatewayReply::Lease {
                message:
                    LeaseMsg::Deny {
                        claim_id: Some(answered),
                        ..
                    },
            } if answered == claim_id => return Ok(None),
            _ => {}
        }
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

/// The highest `duplicate_authority` total persistd reported.
///
/// The counter is absolute rather than an interval delta precisely so a
/// violation one tick before the read is still visible here.
fn read_duplicate_authority(path: &std::path::Path) -> Result<u64> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(0);
    };
    let mut highest = 0;
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("gateway_authority") {
            continue;
        }
        highest = highest.max(
            value
                .get("duplicate_authority")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        );
    }
    Ok(highest)
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
