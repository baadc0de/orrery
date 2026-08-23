//! P5's single-gateway dupe gauntlet (issue #151).
//!
//! The gate launches this binary twice. `gateway` is its own OS process and
//! assembles the production `GatewayServer`, enforcing validator, witness
//! epoch cache, and FDB executor. `run` speaks the real iroh gateway wire and
//! then reads the durable rows back from FoundationDB. Keeping those roles in
//! separate processes prevents an in-memory executor or direct validator call
//! from masquerading as an end-to-end proof.

mod wire;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use foundationdb::options::StreamingMode;
use foundationdb::{Database, FdbBindingError, KeySelector, RangeOption};
use futures::TryStreamExt;
use orrery_persistd::gateway::{
    InterestAuthority, SessionTokenV1Authorizer, SharedBindingAuthority, SnapshotBindingAuthority,
};
use orrery_persistd::intent::{
    AttestationEnforcement, AttestationPosture, BaselineIntentValidator, ItemTransferArgs,
    LEDGER_ITEM_TRANSFER_OP, SHADOW_TARGET,
};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::witness_epoch::WitnessEpochAuthority;
use orrery_persistd::{
    keyspace, CellRuntime, FdbIntentExecutor, GatewayConfig, GatewayServer, JournalConfig,
    MemFenceStore, Router, RuntimeConfig,
};
use orrery_protocol::{
    required_witnesses, AccountId, AssetId, CellEpoch, CellId, Epoch, GatewayMsg, GatewayReply,
    GridId, Intent, IntentOp, IntentOutcome, IssuerKey, IssuerKeyId, ItemUid, NodeId,
    SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, UnixMillis,
    WitnessEpochClaimsV1, WitnessEpochV1, WITNESS_EPOCH_ACK_OK, WITNESS_QUORUM_K,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::wire::Session;

const GRID: GridId = GridId(151);
const CELL: CellId = CellId::ROOT;
const EPOCH: u32 = 1;
const HANDLE: u64 = 0x0097_0000_0000_0001;
const BUYER: AccountId = AccountId(151_001);
const SELLER: AccountId = AccountId(151_002);
const REPLAY_ASSET: AssetId = AssetId(151_101);
const CONTROL_ASSET: AssetId = AssetId(151_102);
const STARTING_BALANCE: i128 = 1_000;
const PRICE: i64 = 100;

const REPLAY_INTENT: u128 = 151_000_001;
const CONTROL_INTENT: u128 = 151_000_002;
const LEGACY_INTENT: u128 = 151_000_003;
const SELF_WITNESS_INTENT: u128 = 151_000_004;
const OUTSIDE_SET_INTENT: u128 = 151_000_005;
const WRONG_SUBSET_INTENT: u128 = 151_000_006;
const GOOD_ORDERING_CONTROL_INTENT: u128 = 151_000_007;
const QUARANTINED_INTENT: u128 = 151_000_008;

// ── The ramp arm's own ids (issue #222) ────────────────────────────────────
//
// Disjoint from every id above, and from the dupe gauntlet's ledger items, so
// the two gates can share a cluster without either one's seed check refusing
// the other's rows.
const RAMP_ASSET: AssetId = AssetId(151_103);
/// The honest trade that makes each gateway's epoch cache adopt the durable
/// draw key before any subset is drawn against it.
const RAMP_ENFORCING_WARMUP_INTENT: u128 = 151_000_101;
const RAMP_SHADOW_WARMUP_INTENT: u128 = 151_000_102;
/// The synthetic offender, submitted once to each gateway. Two ids because the
/// intent id is the idempotency key and both gateways write the same
/// FoundationDB keyspace — one id would make the second submission a replay of
/// the first rather than a second judgement of the same traffic.
const RAMP_ENFORCING_OFFENDER_INTENT: u128 = 151_000_103;
const RAMP_SHADOW_OFFENDER_INTENT: u128 = 151_000_104;
/// The offender submitted to the *enforcing* process after it is demoted, and
/// the one submitted after it is promoted back.
const RAMP_DEMOTED_OFFENDER_INTENT: u128 = 151_000_105;
const RAMP_REPROMOTED_OFFENDER_INTENT: u128 = 151_000_106;

/// D32 clause (c)'s bound on an operator's decision reaching a running fleet:
/// one poll interval plus apply, 2 s wall clock.
const RAMP_APPLY_BOUND_MS: u64 = 2_000;

#[derive(Debug, Parser)]
#[command(name = "p5-dupe-gauntlet")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the persistence gateway until interrupted.
    Gateway {
        #[arg(long)]
        cluster_file: PathBuf,
        #[arg(long)]
        data_dir: PathBuf,
        /// D32 clause (c)'s startup default for control C1, which seeds the
        /// posture cell and pins this process's identity.
        ///
        /// `required` is the default so every existing caller — the dupe
        /// gauntlet's gate among them — keeps the process it already launches.
        #[arg(long, default_value = "required")]
        enforcement: String,
        /// D32 clause (c)'s runtime lever, stood in for by a file.
        ///
        /// The record's lever is the durable `ramp/{control}` row polled on
        /// the maintenance sweep, and that row does not exist in this tree
        /// yet. What the ramp gate has to prove is the property the row is a
        /// transport for — that a control demoted while the process runs stops
        /// acting, within clause (c)'s bound — so the transport is a file this
        /// process polls on the same schedule. Every byte downstream of
        /// [`AttestationPosture::set`] is the production path.
        #[arg(long)]
        posture_file: Option<PathBuf>,
    },
    /// Exercise all requested arms and write their durable evidence.
    Run {
        #[arg(long)]
        gateway_addr: String,
        #[arg(long)]
        gateway_node: String,
        #[arg(long)]
        cluster_file: PathBuf,
        #[arg(long)]
        audit_log: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[arg(long)]
        replay: bool,
        #[arg(long)]
        attestation: bool,
        #[arg(long)]
        quarantine: bool,
    },
    /// D32's enforcement ramp, against one shadow and one enforcing gateway
    /// (issue #222).
    Ramp {
        #[arg(long)]
        enforcing_addr: String,
        #[arg(long)]
        enforcing_node: String,
        #[arg(long)]
        shadow_addr: String,
        #[arg(long)]
        shadow_node: String,
        #[arg(long)]
        cluster_file: PathBuf,
        /// The enforcing gateway's own process log: refusal causes, and the
        /// shadow observations it starts emitting once it is demoted.
        #[arg(long)]
        enforcing_log: PathBuf,
        /// The shadow gateway's process log, which is where the observation
        /// half of this gate is read from.
        #[arg(long)]
        shadow_log: PathBuf,
        /// The file the enforcing gateway polls for its posture.
        #[arg(long)]
        posture_file: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Measure paired honest trade commits with and without gateway-side
    /// attestation verification (issue #153). This is not a gauntlet arm and
    /// does not change the nightly gate's assertions or report.
    Measure {
        #[arg(long)]
        control_addr: String,
        #[arg(long)]
        control_node: String,
        #[arg(long)]
        attested_addr: String,
        #[arg(long)]
        attested_node: String,
        #[arg(long)]
        cluster_file: PathBuf,
        #[arg(long)]
        control_stages: PathBuf,
        #[arg(long)]
        attested_stages: PathBuf,
        #[arg(long)]
        report: PathBuf,
        /// Samples in each population. Ten thousand leaves 100 observations
        /// in the upper one percent instead of presenting a tiny tail as p99.
        #[arg(long, default_value_t = 10_000)]
        samples: usize,
        /// Simultaneous submissions per population. Each worker owns one
        /// session to each gateway and submits its pair concurrently.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Gateway {
            cluster_file,
            data_dir,
            enforcement,
            posture_file,
        } => {
            run_gateway(
                &cluster_file,
                &data_dir,
                AttestationEnforcement::from_str(&enforcement)
                    .map_err(|_| anyhow::anyhow!("unknown --enforcement {enforcement:?}"))?,
                posture_file.as_deref(),
            )
            .await
        }
        Command::Run {
            gateway_addr,
            gateway_node,
            cluster_file,
            audit_log,
            report,
            replay,
            attestation,
            quarantine,
        } => {
            anyhow::ensure!(replay, "the replay arm was not requested");
            anyhow::ensure!(attestation, "the attestation arm was not requested");
            anyhow::ensure!(quarantine, "the quarantine arm was not requested");
            run_gauntlet(
                &gateway_addr,
                &gateway_node,
                &cluster_file,
                &audit_log,
                &report,
            )
            .await
        }
        Command::Ramp {
            enforcing_addr,
            enforcing_node,
            shadow_addr,
            shadow_node,
            cluster_file,
            enforcing_log,
            shadow_log,
            posture_file,
            report,
        } => {
            run_ramp(RampArgs {
                enforcing: endpoint_addr(&enforcing_node, &enforcing_addr)?,
                shadow: endpoint_addr(&shadow_node, &shadow_addr)?,
                cluster_file,
                enforcing_log,
                shadow_log,
                posture_file,
                report,
            })
            .await
        }
        Command::Measure {
            control_addr,
            control_node,
            attested_addr,
            attested_node,
            cluster_file,
            control_stages,
            attested_stages,
            report,
            samples,
            concurrency,
        } => {
            run_measurement(MeasurementArgs {
                control: endpoint_addr(&control_node, &control_addr)?,
                attested: endpoint_addr(&attested_node, &attested_addr)?,
                cluster_file,
                control_stages,
                attested_stages,
                report,
                samples,
                concurrency,
            })
            .await
        }
    }
}

fn secret(seed: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[1] = 0x51;
    iroh::SecretKey::from_bytes(&bytes)
}

fn identity_issuer() -> iroh::SecretKey {
    secret(201)
}

fn coordinator() -> iroh::SecretKey {
    secret(200)
}

fn witnesses() -> Vec<iroh::SecretKey> {
    (100..107).map(secret).collect()
}

/// D31 clause (e)'s `owner(n)` for this harness: one account per announced
/// witness, none of them [`BUYER`] or [`SELLER`].
///
/// Two properties are load-bearing and neither is decoration. **Distinct**
/// accounts, so no pair of announced witnesses is refused as one account
/// attesting twice. **Non-party** accounts, so `E(I)` is the full announced
/// set and arm (c.4)'s draw runs over the vector this file computes as
/// `selected.clone()`. And they must resolve *at all*: D31 clause (f) excludes
/// a candidate whose binding is unknown, so an unbound witness set would leave
/// `E(I)` empty and demote every arm to the low-population path.
fn witness_bindings() -> SharedBindingAuthority {
    Arc::new(SnapshotBindingAuthority::from_bindings(
        witnesses()
            .iter()
            .enumerate()
            .map(|(index, key)| (key.public(), AccountId(151_100 + index as u64))),
    ))
}

/// The gateway's wire identity, keyed on its startup posture.
///
/// The ramp gate runs a shadow and an enforcing gateway **at the same time**
/// against one cluster, so the two cannot share a NodeId. Deriving it from the
/// startup mode rather than from a flag keeps the enforcing process byte-for-
/// byte the one the dupe gauntlet's gate already launches, and the identity is
/// fixed at startup: a demotion changes what a process does, never who it is.
const fn gateway_seed(enforcement: AttestationEnforcement) -> u8 {
    match enforcement {
        AttestationEnforcement::Shadow => 251,
        _ => 250,
    }
}

/// D32 clause (c)'s runtime lever, transported by a file.
///
/// The record's lever is `ramp/attestation_quorum`, a durable row every
/// process polls on its 1 s maintenance sweep. That row is not in the tree, and
/// inventing it here would put a keyspace allocation inside a gate. What the
/// gate has to prove is downstream of the transport — that a control demoted
/// in a *running* process stops acting, within the bound — so the transport is
/// a polled file and everything after [`AttestationPosture::set`] is the
/// production path.
///
/// Polled at a quarter of clause (c)'s 1 s sweep so the gate's measurement of
/// apply latency is not dominated by this stand-in's own period.
fn spawn_posture_poller(path: PathBuf, posture: AttestationPosture) {
    tokio::spawn(async move {
        let mut last = posture.get();
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mode) = AttestationEnforcement::from_str(text.trim()) else {
                continue;
            };
            if mode != last {
                posture.set(mode);
                last = mode;
                tracing::info!(
                    target: "p5_dupe_gauntlet",
                    control = "attestation_quorum",
                    mode = mode.as_str(),
                    "ramp posture applied"
                );
            }
        }
    });
}

fn runtime_config(data_dir: &Path, enforcement: AttestationEnforcement) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GRID,
        journal: JournalConfig {
            dir: data_dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(2),
                batch_max_records: 128,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: u64::from(gateway_seed(enforcement)) - 99,
        epoch: Epoch::new(1),
        fence: Arc::new(MemFenceStore::new()),
    }
}

async fn run_gateway(
    cluster_file: &Path,
    data_dir: &Path,
    enforcement: AttestationEnforcement,
    posture_file: Option<&Path>,
) -> Result<()> {
    // `SHADOW_TARGET` is `orrery::ramp::shadow`, which is **not** a prefix of
    // `orrery_persistd` — a filter naming only the crate drops every shadow
    // observation, and the gate reading that log would see an inert control as
    // a silent one. Named explicitly, and at `debug`, because the
    // `would_act = false` observations are the coverage denominator D32 clause
    // (e) calls the difference between a rate and blindness.
    let filter = format!("orrery_persistd=debug,p5_dupe_gauntlet=info,{SHADOW_TARGET}=debug");
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("install gateway tracing: {error}"))?;

    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create gateway data dir {}", data_dir.display()))?;
    let epochs = Arc::new(WitnessEpochAuthority::new([IssuerKey::new(
        IssuerKeyId::new(1),
        coordinator().public(),
    )]));
    // D30/D31's three authorities, built before the executor because the two
    // halves of control C1 share them — and, since #222, share the posture
    // cell as well: `tracking_posture` hands the executor the *same* cell the
    // validator reads, so one write moves the admission refusal and the
    // commit-time re-proof together. Two cells would let a demoted gateway go
    // on refusing at commit what it now admits at admission.
    let validator = match enforcement {
        AttestationEnforcement::Required => BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            Arc::new(CoverAllInterest),
            witness_bindings(),
        ),
        AttestationEnforcement::Shadow => BaselineIntentValidator::shadow(
            Arc::clone(&epochs),
            Arc::new(CoverAllInterest),
            witness_bindings(),
        ),
        AttestationEnforcement::Off => {
            anyhow::bail!("`off` evaluates nothing and calibrates nothing (D32 clause (b))")
        }
    };
    let posture = validator.posture();
    let executor = FdbIntentExecutor::connect(
        cluster_file
            .to_str()
            .context("cluster-file path is not UTF-8")?,
        GRID,
    )?
    .tracking_posture(Arc::clone(&epochs), witness_bindings(), posture.clone());

    if let Some(path) = posture_file {
        spawn_posture_poller(path.to_path_buf(), posture);
    }

    let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
        Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
    let runtime = Arc::new(Mutex::new(
        CellRuntime::open(&runtime_config(data_dir, enforcement), &store)
            .await
            .context("open gauntlet runtime")?,
    ));
    let router: Arc<dyn Router> = runtime;

    let config = GatewayConfig {
        secret_key: Some(secret(gateway_seed(enforcement))),
        executor: Some(Arc::new(executor)),
        // D30 (#197) made the validator resolve a cell-epoch only where the
        // issuer has standing, so `enforcing` now takes the interest authority
        // too. The gauntlet hands it the same `CoverAllInterest` the gateway
        // gets: cell coverage is orthogonal to what these arms prove, and
        // holding it open is what stops a standing refusal masquerading as an
        // attestation result.
        //
        // D31 (#211) added the third authority for the same reason and with
        // the same discipline: `E(I)` is now derived through `owner(n)`, and
        // the executor below records the vector through the **same** resolver
        // it is handed here. Two resolvers would let the recorded vector and
        // the admitted one disagree by construction.
        validator: Arc::new(validator),
        witness_epochs: Some(epochs),
        authorizer: Arc::new(SessionTokenV1Authorizer::new([IssuerKey::new(
            IssuerKeyId::new(1),
            identity_issuer().public(),
        )])),
        // The announcement signature is the authority under test here. Cell
        // coverage is orthogonal and is held open so it cannot mask an
        // attestation result as a courier refusal.
        interest_authority: Arc::new(CoverAllInterest),
        ..GatewayConfig::default()
    };
    let server = GatewayServer::spawn(config, router)
        .await
        .context("spawn enforcing gateway")?;
    let addr = server.addr();
    let bind_addr = addr
        .ip_addrs()
        .next()
        .context("gateway published no direct address")?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "bind_addr": bind_addr.to_string(),
            "node_id": server.id().to_string(),
            "enforcement": enforcement.as_str(),
            "posture_file": posture_file.map(|path| path.display().to_string()),
            "fdb": true
        }))?
    );

    tokio::signal::ctrl_c()
        .await
        .context("wait for gateway stop")?;
    server.shutdown().await;
    Ok(())
}

#[derive(Debug, Default)]
struct CoverAllInterest;

impl InterestAuthority for CoverAllInterest {
    fn snapshot_for(&self, _peer: NodeId) -> Option<orrery_protocol::CoordinatorInterestSnapshot> {
        None
    }

    fn allows(&self, _peer: NodeId, _grid: GridId, _cell: CellId, _now_ms: u64) -> bool {
        true
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn session_token(node: NodeId, standing: SessionStanding) -> Result<Vec<u8>> {
    SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            BUYER,
            node,
            UnixMillis::new(unix_ms()),
            SessionTokenTtlMs::new(60_000),
            standing,
            IssuerKeyId::new(1),
            false,
        ),
        &identity_issuer(),
    )?
    .encode()
    .map_err(Into::into)
}

fn endpoint_addr(node: &str, socket: &str) -> Result<iroh::EndpointAddr> {
    let node = NodeId::from_str(node).context("gateway node id")?;
    let socket = socket.parse().context("gateway socket address")?;
    Ok(iroh::EndpointAddr::from_parts(
        node,
        [iroh::TransportAddr::Ip(socket)],
    ))
}

async fn connect_session(
    key: &iroh::SecretKey,
    standing: SessionStanding,
    address: iroh::EndpointAddr,
) -> Result<Session> {
    let session = Session::connect(key.clone(), address).await?;
    session.send(&GatewayMsg::VersionedHello {
        token: session_token(key.public(), standing)?,
        node: key.public(),
        version: orrery_protocol::PROTOCOL_VERSION,
    })?;
    anyhow::ensure!(
        matches!(
            session.recv(Duration::from_secs(10)).await,
            Some(GatewayReply::HelloAck { .. })
        ),
        "gateway did not accept the {standing:?} session"
    );
    Ok(session)
}

fn announcement(selected: &[NodeId]) -> Result<Vec<u8>> {
    let mut candidates = selected.to_vec();
    candidates.sort_by_key(|node| *node.as_bytes());
    let claims = WitnessEpochClaimsV1::new(
        GRID,
        CELL,
        EPOCH,
        HANDLE,
        30_000,
        30_000,
        candidates,
        selected.to_vec(),
        orrery_protocol::witness_epoch_commitment(GRID, CELL, EPOCH, &[7u8; 32]),
        None,
        IssuerKeyId::new(1),
    );
    Ok(WitnessEpochV1::sign(claims, &coordinator())?.encode()?)
}

fn transfer_intent(
    issuer: &iroh::SecretKey,
    intent_id: u128,
    item: ItemUid,
    asset: AssetId,
) -> Intent {
    let args = ItemTransferArgs {
        item,
        seller: SELLER,
        buyer: BUYER,
        asset,
        price: PRICE,
    };
    let mut intent = Intent {
        evidence: None,
        intent_id,
        issuer: issuer.public(),
        cell_epoch: CellEpoch::new(HANDLE),
        ops: vec![IntentOp {
            op: LEDGER_ITEM_TRANSFER_OP,
            args: Bytes::from(args.encode().to_vec()),
        }],
        attestations: Vec::new(),
        signature: issuer.sign(b"placeholder"),
    };
    intent.sign(issuer);
    intent
}

fn fully_attest(intent: &mut Intent, keys: &[iroh::SecretKey]) {
    intent.attestations = keys.iter().map(|key| intent.attest(key)).collect();
}

async fn present_epoch(session: &Session, bytes: Vec<u8>) -> Result<()> {
    session.send(&GatewayMsg::WitnessEpoch {
        announcement: bytes,
    })?;
    loop {
        match session.recv(Duration::from_secs(10)).await {
            Some(GatewayReply::WitnessEpochAck {
                epoch: Some(EPOCH),
                reason: WITNESS_EPOCH_ACK_OK,
            }) => return Ok(()),
            Some(_) => {}
            None => anyhow::bail!("gateway did not accept the witness epoch"),
        }
    }
}

async fn submit(session: &Session, intent: Intent) -> Result<IntentOutcome> {
    let intent_id = intent.intent_id;
    session.send(&GatewayMsg::SubmitIntent { intent })?;
    loop {
        match session.recv(Duration::from_secs(20)).await {
            Some(GatewayReply::IntentAck {
                intent_id: answered,
                outcome,
            }) if answered == intent_id => return Ok(outcome),
            Some(_) => {}
            None => anyhow::bail!("no IntentAck for {intent_id}"),
        }
    }
}

async fn run_gauntlet(
    gateway_addr: &str,
    gateway_node: &str,
    cluster_file: &Path,
    audit_log: &Path,
    report_path: &Path,
) -> Result<()> {
    let db = FdbIntentExecutor::connect(
        cluster_file
            .to_str()
            .context("cluster-file path is not UTF-8")?,
        GRID,
    )?
    .database()
    .clone();
    seed_ledger(&db).await?;

    let address = endpoint_addr(gateway_node, gateway_addr)?;
    let good_key = secret(1);
    let good = connect_session(&good_key, SessionStanding::Good, address.clone()).await?;
    let witness_keys = witnesses();
    let selected: Vec<NodeId> = witness_keys.iter().map(iroh::SecretKey::public).collect();
    present_epoch(&good, announcement(&selected)?).await?;

    // Arm (a): byte-for-byte the same signed intent enters the live gateway
    // twice. The durable row and receipt scan decide whether it transferred
    // once; matching acknowledgements alone are not accepted as evidence.
    let mut replay = transfer_intent(
        &good_key,
        REPLAY_INTENT,
        ItemUid::new(151_201),
        REPLAY_ASSET,
    );
    fully_attest(&mut replay, &witness_keys);
    let replay_first = submit(&good, replay.clone()).await?;
    let replay_second = submit(&good, replay).await?;

    // Negative control for the attestation arm: an independently keyed,
    // honestly attested trade in the same epoch must commit.
    let mut control = transfer_intent(
        &good_key,
        CONTROL_INTENT,
        ItemUid::new(151_202),
        CONTROL_ASSET,
    );
    fully_attest(&mut control, &witness_keys);
    let control_outcome = submit(&good, control).await?;

    let epoch_row: keyspace::EpochRow = decode_required(
        &db,
        keyspace::epoch_key(GRID, CELL, EPOCH).to_vec(),
        "durable epoch row",
    )
    .await?;
    // `E(I)` is the announced set: the issuer is not announced, no announced
    // witness is bound to `BUYER` or `SELLER`, and every one of them resolves
    // (`witness_bindings`), so neither half of D10 item 4's party exclusion
    // removes anybody here.
    let eligible = selected.clone();

    // Arm (c.1): real witness keys sign the old issuer preimage. Those
    // signatures are cryptographically genuine over those bytes and count as
    // no D27 attestation.
    let mut legacy = transfer_intent(
        &good_key,
        LEGACY_INTENT,
        ItemUid::new(151_203),
        CONTROL_ASSET,
    );
    let legacy_preimage = legacy.signing_preimage();
    legacy.attestations = witness_keys
        .iter()
        .take(WITNESS_QUORUM_K)
        .map(|key| orrery_protocol::Attestation {
            witness: key.public(),
            signature: key.sign(&legacy_preimage),
        })
        .collect();
    let legacy_crypto_valid = legacy.attestations.iter().all(|attestation| {
        attestation
            .witness
            .verify(&legacy_preimage, &attestation.signature)
            .is_ok()
    });
    let legacy_outcome = submit(&good, legacy).await?;

    // Arm (c.2): the issuer makes a structurally correct witness signature.
    // Domain separation alone cannot stop this; party exclusion must.
    let mut self_witnessed = transfer_intent(
        &good_key,
        SELF_WITNESS_INTENT,
        ItemUid::new(151_204),
        CONTROL_ASSET,
    );
    let self_attestation = self_witnessed.attest(&good_key);
    let self_crypto_valid = self_attestation.verify(&self_witnessed);
    self_witnessed.attestations.push(self_attestation);
    let self_witness_outcome = submit(&good, self_witnessed).await?;

    // Arm (c.3): a valid signature from a self-chosen key outside the
    // coordinator announcement is still not a witness for this epoch.
    let outsider = secret(150);
    let mut outside = transfer_intent(
        &good_key,
        OUTSIDE_SET_INTENT,
        ItemUid::new(151_205),
        CONTROL_ASSET,
    );
    let outside_attestation = outside.attest(&outsider);
    let outside_crypto_valid = outside_attestation.verify(&outside);
    outside.attestations.push(outside_attestation);
    for key in witness_keys.iter().take(WITNESS_QUORUM_K - 1) {
        outside.attestations.push(outside.attest(key));
    }
    let outside_outcome = submit(&good, outside).await?;

    // Arm (c.4): exactly K valid, announced co-signatures, deliberately a
    // different subset from the one the durable draw key names.
    let required = required_witnesses(&epoch_row.draw_key, WRONG_SUBSET_INTENT, &eligible);
    let mut wrong_members: Vec<NodeId> = eligible.iter().copied().take(WITNESS_QUORUM_K).collect();
    if wrong_members == required {
        wrong_members[WITNESS_QUORUM_K - 1] = eligible[WITNESS_QUORUM_K];
    }
    let mut wrong_subset = transfer_intent(
        &good_key,
        WRONG_SUBSET_INTENT,
        ItemUid::new(151_206),
        CONTROL_ASSET,
    );
    for node in &wrong_members {
        let key = witness_keys
            .iter()
            .find(|key| key.public() == *node)
            .context("wrong-subset member is not announced")?;
        wrong_subset.attestations.push(wrong_subset.attest(key));
    }
    let wrong_subset_crypto_valid = wrong_subset
        .attestations
        .iter()
        .all(|attestation| attestation.verify(&wrong_subset));
    let wrong_subset_outcome = submit(&good, wrong_subset).await?;

    // Arm (d): the same cheap shape rejection with a forged attestation.
    // Good standing stops at `no_ops`; quarantined standing must validate the
    // attestation first and therefore attributes the refusal to
    // `bad_attestation` in the gateway audit log.
    let good_ordering = forged_empty_intent(&good_key, GOOD_ORDERING_CONTROL_INTENT);
    let good_ordering_outcome = submit(&good, good_ordering).await?;
    let quarantined_key = secret(2);
    let quarantined =
        connect_session(&quarantined_key, SessionStanding::Quarantined, address).await?;
    let quarantine_outcome = submit(
        &quarantined,
        forged_empty_intent(&quarantined_key, QUARANTINED_INTENT),
    )
    .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let audit = read_refusal_audit(audit_log)?;
    let cause = |intent_id| audit.get(&intent_id).cloned();
    let receipts = read_receipts(&db).await?;

    let replay_owner = read_item_owner(&db, ItemUid::new(151_201)).await?;
    let replay_intent_row = read_raw(&db, keyspace::intent_key(REPLAY_INTENT).to_vec())
        .await?
        .is_some();
    let replay_attest_row = read_raw(&db, keyspace::attest_key(REPLAY_INTENT).to_vec())
        .await?
        .is_some();
    let replay_receipts = receipts
        .iter()
        .filter(|row| row.intent_id == REPLAY_INTENT)
        .count();
    let replay_buyer_balance = read_balance(&db, BUYER, REPLAY_ASSET).await?;
    let replay_seller_balance = read_balance(&db, SELLER, REPLAY_ASSET).await?;
    let replay_passed = matches!(replay_first, IntentOutcome::Committed { .. })
        && replay_first == replay_second
        && replay_owner == Some(BUYER)
        && replay_intent_row
        && replay_attest_row
        && replay_receipts == 1
        && replay_buyer_balance == STARTING_BALANCE - i128::from(PRICE)
        && replay_seller_balance == i128::from(PRICE);

    let honest_receipts = receipts
        .iter()
        .filter(|row| row.intent_id == CONTROL_INTENT)
        .count();
    let honest_passed = matches!(control_outcome, IntentOutcome::Committed { .. })
        && read_item_owner(&db, ItemUid::new(151_202)).await? == Some(BUYER)
        && honest_receipts == 1;

    let legacy_durable = rejected_without_state(&db, &receipts, LEGACY_INTENT).await?;
    let self_durable = rejected_without_state(&db, &receipts, SELF_WITNESS_INTENT).await?;
    let outside_durable = rejected_without_state(&db, &receipts, OUTSIDE_SET_INTENT).await?;
    let wrong_durable = rejected_without_state(&db, &receipts, WRONG_SUBSET_INTENT).await?;
    let quarantine_durable = rejected_without_state(&db, &receipts, QUARANTINED_INTENT).await?;

    let legacy_passed = legacy_crypto_valid
        && rejected_as(&legacy_outcome, orrery_protocol::REASON_VALIDATION_FAILED)
        && cause(LEGACY_INTENT).as_deref() == Some("bad_attestation")
        && legacy_durable == (true, true, 0);
    let self_passed = self_crypto_valid
        && rejected_as(&self_witness_outcome, orrery_protocol::REASON_SELF_WITNESS)
        && cause(SELF_WITNESS_INTENT).as_deref() == Some("self_witness")
        && self_durable == (true, true, 0);
    let outside_passed = outside_crypto_valid
        && rejected_as(&outside_outcome, orrery_protocol::REASON_ATTESTATION_QUORUM)
        && cause(OUTSIDE_SET_INTENT).as_deref() == Some("witness_outside_announced_set")
        && outside_durable == (true, true, 0);
    let wrong_subset_passed = wrong_subset_crypto_valid
        && wrong_members.len() == WITNESS_QUORUM_K
        && wrong_members != required
        && wrong_members.iter().all(|node| eligible.contains(node))
        && rejected_as(
            &wrong_subset_outcome,
            orrery_protocol::REASON_ATTESTATION_QUORUM,
        )
        && cause(WRONG_SUBSET_INTENT).as_deref() == Some("required_witness_missing")
        && wrong_durable == (true, true, 0);
    let attestation_passed =
        honest_passed && legacy_passed && self_passed && outside_passed && wrong_subset_passed;

    let quarantine_passed = rejected_as(
        &good_ordering_outcome,
        orrery_protocol::REASON_VALIDATION_FAILED,
    ) && rejected_as(
        &quarantine_outcome,
        orrery_protocol::REASON_VALIDATION_FAILED,
    ) && cause(GOOD_ORDERING_CONTROL_INTENT).as_deref() == Some("no_ops")
        && cause(QUARANTINED_INTENT).as_deref() == Some("bad_attestation")
        && quarantine_durable == (true, true, 0);

    let passed = replay_passed && attestation_passed && quarantine_passed;
    let node_strings = |nodes: &[NodeId]| nodes.iter().map(ToString::to_string).collect::<Vec<_>>();
    let report = json!({
        "schema": "orrery.p5-dupe-gauntlet/1",
        "result": if passed { "pass" } else { "fail" },
        "gateway": {
            "process_boundary": true,
            "wire": "iroh",
            "fdb": true,
            "attestation_enforcement": "required"
        },
        "epoch": {
            "grid": GRID.0,
            "cell": CELL.to_bits(),
            "epoch": EPOCH,
            "handle": HANDLE,
            "announced_witnesses": node_strings(&selected),
            "draw_commit": hex(&epoch_row.draw_commit),
            "draw_key_audited_from_fdb": true
        },
        "arms": {
            "replay": {
                "passed": replay_passed,
                "intent_id": REPLAY_INTENT.to_string(),
                "submissions": 2,
                "first_outcome": format!("{replay_first:?}"),
                "second_outcome": format!("{replay_second:?}"),
                "outcomes_identical": replay_first == replay_second,
                "durable_item_owner": replay_owner.map(|owner| owner.0),
                "intent_rows": usize::from(replay_intent_row),
                "attest_rows": usize::from(replay_attest_row),
                "ledger_receipts": replay_receipts,
                "buyer_balance": replay_buyer_balance.to_string(),
                "seller_balance": replay_seller_balance.to_string()
            },
            "attestation": {
                "passed": attestation_passed,
                "honest_control": {
                    "passed": honest_passed,
                    "intent_id": CONTROL_INTENT.to_string(),
                    "outcome": format!("{control_outcome:?}"),
                    "ledger_receipts": honest_receipts
                },
                "legacy_preimage": refusal_json(
                    legacy_passed,
                    LEGACY_INTENT,
                    &legacy_outcome,
                    cause(LEGACY_INTENT),
                    legacy_durable,
                    json!({"valid_over_legacy_preimage": legacy_crypto_valid})
                ),
                "issuer_as_witness": refusal_json(
                    self_passed,
                    SELF_WITNESS_INTENT,
                    &self_witness_outcome,
                    cause(SELF_WITNESS_INTENT),
                    self_durable,
                    json!({"valid_attestation_signature": self_crypto_valid})
                ),
                "outside_announced_set": refusal_json(
                    outside_passed,
                    OUTSIDE_SET_INTENT,
                    &outside_outcome,
                    cause(OUTSIDE_SET_INTENT),
                    outside_durable,
                    json!({"valid_attestation_signature": outside_crypto_valid})
                ),
                "non_required_subset": refusal_json(
                    wrong_subset_passed,
                    WRONG_SUBSET_INTENT,
                    &wrong_subset_outcome,
                    cause(WRONG_SUBSET_INTENT),
                    wrong_durable,
                    json!({
                        "cryptographically_valid": wrong_subset_crypto_valid,
                        "announced": wrong_members.iter().all(|node| eligible.contains(node)),
                        "submitted": node_strings(&wrong_members),
                        "required": node_strings(&required)
                    })
                )
            },
            "quarantine": {
                "passed": quarantine_passed,
                "intent_id": QUARANTINED_INTENT.to_string(),
                "standing": "quarantined",
                "outcome": format!("{quarantine_outcome:?}"),
                "audit_cause": cause(QUARANTINED_INTENT),
                "good_standing_control": {
                    "intent_id": GOOD_ORDERING_CONTROL_INTENT.to_string(),
                    "outcome": format!("{good_ordering_outcome:?}"),
                    "audit_cause": cause(GOOD_ORDERING_CONTROL_INTENT)
                },
                "full_validation_path_proved_by_ordering":
                    cause(GOOD_ORDERING_CONTROL_INTENT).as_deref() == Some("no_ops")
                    && cause(QUARANTINED_INTENT).as_deref() == Some("bad_attestation"),
                "intent_rows": usize::from(!quarantine_durable.0),
                "attest_rows": usize::from(!quarantine_durable.1),
                "ledger_receipts": quarantine_durable.2
            }
        }
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    std::fs::write(report_path, &encoded)
        .with_context(|| format!("write report {}", report_path.display()))?;
    println!("{}", String::from_utf8_lossy(&encoded));
    anyhow::ensure!(passed, "one or more gauntlet arms failed");
    Ok(())
}

// ── Honest trade verification-overhead measurement (issue #153) ────────
//
// This deliberately lives beside, not inside, `run_gauntlet`. The nightly
// gate's commands, arms, report schema and fixed ids remain unchanged. The
// only shared pieces are the production gateway constructor and the honest
// epoch/trade helpers whose exact shape the measurement must reuse.

const MEASURE_ASSET_BASE: u64 = 153_000_000;
const MEASURE_ITEM_BASE: u64 = 153_000_000;
const MEASURE_CONTROL_INTENT_BASE: u128 = 153_000_000_000;
const MEASURE_ATTESTED_INTENT_BASE: u128 = 153_100_000_000;
const MEASURE_CONTROL_WARMUP_INTENT: u128 = 153_900_000_001;
const MEASURE_ATTESTED_WARMUP_INTENT: u128 = 153_900_000_002;
const MEASURE_CONTROL_CONVERGENCE_INTENT: u128 = 153_900_000_003;
const MEASURE_ATTESTED_CONVERGENCE_INTENT: u128 = 153_900_000_004;
const MEASURE_MIN_SAMPLES: usize = 10_000;

struct MeasurementArgs {
    control: iroh::EndpointAddr,
    attested: iroh::EndpointAddr,
    cluster_file: PathBuf,
    control_stages: PathBuf,
    attested_stages: PathBuf,
    report: PathBuf,
    samples: usize,
    concurrency: usize,
}

#[derive(Debug)]
struct WorkerSamples {
    control: Vec<(usize, u64)>,
    attested: Vec<(usize, u64)>,
    attestations_verified: usize,
}

async fn run_measurement(args: MeasurementArgs) -> Result<()> {
    anyhow::ensure!(
        args.samples >= MEASURE_MIN_SAMPLES,
        "--samples must be at least {MEASURE_MIN_SAMPLES}; a smaller population gives p99 too few tail observations"
    );
    anyhow::ensure!(
        (1..=64).contains(&args.concurrency),
        "--concurrency must be in 1..=64"
    );
    let db = FdbIntentExecutor::connect(
        args.cluster_file
            .to_str()
            .context("cluster-file path is not UTF-8")?,
        GRID,
    )?
    .database()
    .clone();
    seed_measurement_ledger(&db, args.samples).await?;

    let witness_keys = Arc::new(witnesses());
    let selected: Vec<NodeId> = witness_keys.iter().map(iroh::SecretKey::public).collect();
    let mut control_sessions = Vec::with_capacity(args.concurrency);
    let mut attested_sessions = Vec::with_capacity(args.concurrency);
    for worker in 0..args.concurrency {
        let key = secret(u8::try_from(worker + 1).context("worker key seed")?);
        control_sessions
            .push(connect_session(&key, SessionStanding::Good, args.control.clone()).await?);
        attested_sessions
            .push(connect_session(&key, SessionStanding::Good, args.attested.clone()).await?);
    }
    // The enforcing gateway owns epoch initialization. Its first full-N
    // warmup makes *its own* draw key durable before the control gateway can
    // race a different key into the row. This ordering is the precondition
    // exact-K samples need; a simultaneous first commit would deliberately
    // refuse the loser once while it adopts the durable key.
    present_epoch(&attested_sessions[0], announcement(&selected)?).await?;

    // Both gateways commit the same fully-attested shape used by the live
    // gauntlet. The enforcing gateway goes first to establish the durable key;
    // the shadow control then adopts it without racing initialization.
    let warmup_key = secret(1);
    let mut attested_warmup = transfer_intent(
        &warmup_key,
        MEASURE_ATTESTED_WARMUP_INTENT,
        ItemUid::new(MEASURE_ITEM_BASE),
        AssetId(MEASURE_ASSET_BASE),
    );
    fully_attest(&mut attested_warmup, &witness_keys);
    anyhow::ensure!(
        matches!(
            submit(&attested_sessions[0], attested_warmup).await?,
            IntentOutcome::Committed { .. }
        ),
        "enforcing gateway epoch-initialization warmup did not commit"
    );
    present_epoch(&control_sessions[0], announcement(&selected)?).await?;
    let mut control_warmup = transfer_intent(
        &warmup_key,
        MEASURE_CONTROL_WARMUP_INTENT,
        ItemUid::new(MEASURE_ITEM_BASE + 1),
        AssetId(MEASURE_ASSET_BASE + 1),
    );
    fully_attest(&mut control_warmup, &witness_keys);
    anyhow::ensure!(
        matches!(
            submit(&control_sessions[0], control_warmup).await?,
            IntentOutcome::Committed { .. }
        ),
        "control gateway epoch-adoption warmup did not commit"
    );
    // A second full-N commit on each process proves both caches are on the
    // already-durable key before exactly-K samples are constructed. This is
    // event-ordered, not a fixed-time assertion.
    let mut control_convergence = transfer_intent(
        &warmup_key,
        MEASURE_CONTROL_CONVERGENCE_INTENT,
        ItemUid::new(MEASURE_ITEM_BASE + 2),
        AssetId(MEASURE_ASSET_BASE + 2),
    );
    fully_attest(&mut control_convergence, &witness_keys);
    let mut attested_convergence = transfer_intent(
        &warmup_key,
        MEASURE_ATTESTED_CONVERGENCE_INTENT,
        ItemUid::new(MEASURE_ITEM_BASE + 3),
        AssetId(MEASURE_ASSET_BASE + 3),
    );
    fully_attest(&mut attested_convergence, &witness_keys);
    anyhow::ensure!(
        matches!(
            submit(&control_sessions[0], control_convergence).await?,
            IntentOutcome::Committed { .. }
        ) && matches!(
            submit(&attested_sessions[0], attested_convergence).await?,
            IntentOutcome::Committed { .. }
        ),
        "post-race epoch convergence commits did not both succeed"
    );
    let epoch_row: keyspace::EpochRow = decode_required(
        &db,
        keyspace::epoch_key(GRID, CELL, EPOCH).to_vec(),
        "durable measurement epoch row",
    )
    .await?;

    // Let the 250 ms gateway reporter consume the warmups, then remove their
    // already-drained records. O_APPEND makes subsequent interval deltas land
    // at the new EOF; the reporter's in-memory cursor remains advanced.
    tokio::time::sleep(Duration::from_millis(600)).await;
    std::fs::write(&args.control_stages, [])
        .with_context(|| format!("clear control stages {}", args.control_stages.display()))?;
    std::fs::write(&args.attested_stages, [])
        .with_context(|| format!("clear attested stages {}", args.attested_stages.display()))?;

    let mut tasks = tokio::task::JoinSet::new();
    for (worker, (control, attested)) in control_sessions
        .into_iter()
        .zip(attested_sessions)
        .enumerate()
    {
        let witness_keys = Arc::clone(&witness_keys);
        let selected = selected.clone();
        let draw_key = epoch_row.draw_key;
        let samples = args.samples;
        let concurrency = args.concurrency;
        tasks.spawn(async move {
            let issuer = secret(u8::try_from(worker + 1).context("worker key seed")?);
            let mut result = WorkerSamples {
                control: Vec::new(),
                attested: Vec::new(),
                attestations_verified: 0,
            };
            for index in (worker..samples).step_by(concurrency) {
                let offset = u64::try_from(index).context("sample item offset")?;
                let control_id = MEASURE_CONTROL_INTENT_BASE
                    + u128::try_from(index).context("control intent offset")?;
                let attested_id = MEASURE_ATTESTED_INTENT_BASE
                    + u128::try_from(index).context("attested intent offset")?;
                let control_intent = transfer_intent(
                    &issuer,
                    control_id,
                    ItemUid::new(MEASURE_ITEM_BASE + 4 + offset),
                    AssetId(MEASURE_ASSET_BASE + 4 + offset),
                );
                let mut attested_intent = transfer_intent(
                    &issuer,
                    attested_id,
                    ItemUid::new(MEASURE_ITEM_BASE + 4 + samples as u64 + offset),
                    AssetId(MEASURE_ASSET_BASE + 4 + samples as u64 + offset),
                );
                let required = required_witnesses(&draw_key, attested_id, &selected);
                anyhow::ensure!(
                    required.len() == WITNESS_QUORUM_K,
                    "required subset has {} members, expected {WITNESS_QUORUM_K}",
                    required.len()
                );
                for node in required {
                    let key = witness_keys
                        .iter()
                        .find(|key| key.public() == node)
                        .context("required witness is not in the announced key set")?;
                    attested_intent
                        .attestations
                        .push(attested_intent.attest(key));
                }
                anyhow::ensure!(
                    attested_intent.attestations.len() == WITNESS_QUORUM_K
                        && attested_intent
                            .attestations
                            .iter()
                            .all(|attestation| attestation.verify(&attested_intent)),
                    "sample {index} does not carry exactly K valid attestations"
                );
                result.attestations_verified += attested_intent.attestations.len();

                // One paired observation: both populations see the same
                // moment and the same offered concurrency, without asserting
                // that either finishes inside a fixed wall-clock window.
                let (control_sample, attested_sample) = tokio::join!(
                    submit_timed(&control, control_intent),
                    submit_timed(&attested, attested_intent)
                );
                let (control_outcome, control_us) = control_sample?;
                let (attested_outcome, attested_us) = attested_sample?;
                anyhow::ensure!(
                    matches!(control_outcome, IntentOutcome::Committed { .. }),
                    "unattested control sample {index} did not commit: {control_outcome:?}"
                );
                anyhow::ensure!(
                    matches!(attested_outcome, IntentOutcome::Committed { .. }),
                    "attested sample {index} did not commit: {attested_outcome:?}"
                );
                result.control.push((index, control_us));
                result.attested.push((index, attested_us));
            }
            Ok::<_, anyhow::Error>(result)
        });
    }

    let mut control = Vec::with_capacity(args.samples);
    let mut attested = Vec::with_capacity(args.samples);
    let mut attestations_verified = 0usize;
    while let Some(joined) = tasks.join_next().await {
        let worker = joined.context("measurement worker panicked")??;
        control.extend(worker.control);
        attested.extend(worker.attested);
        attestations_verified += worker.attestations_verified;
    }
    control.sort_by_key(|sample| sample.0);
    attested.sort_by_key(|sample| sample.0);
    anyhow::ensure!(
        control.len() == args.samples && attested.len() == args.samples,
        "measurement populations are incomplete: control={} attested={} expected={}",
        control.len(),
        attested.len(),
        args.samples
    );
    anyhow::ensure!(
        attestations_verified == args.samples * WITNESS_QUORUM_K,
        "verified-attestation count is {attestations_verified}, expected {}",
        args.samples * WITNESS_QUORUM_K
    );

    // Drain the final interval before reading the gateway-owned stage totals.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let control_stage = stage_report(&args.control_stages, args.samples)?;
    let attested_stage = stage_report(&args.attested_stages, args.samples)?;
    let receipts = read_receipts(&db).await?;
    let control_receipts = receipts
        .iter()
        .filter(|row| {
            (MEASURE_CONTROL_INTENT_BASE..MEASURE_CONTROL_INTENT_BASE + args.samples as u128)
                .contains(&row.intent_id)
        })
        .count();
    let attested_receipts = receipts
        .iter()
        .filter(|row| {
            (MEASURE_ATTESTED_INTENT_BASE..MEASURE_ATTESTED_INTENT_BASE + args.samples as u128)
                .contains(&row.intent_id)
        })
        .count();
    anyhow::ensure!(
        control_receipts == args.samples && attested_receipts == args.samples,
        "durable receipt populations differ: control={control_receipts} attested={attested_receipts} expected={}",
        args.samples
    );

    let control_values: Vec<u64> = control.iter().map(|sample| sample.1).collect();
    let attested_values: Vec<u64> = attested.iter().map(|sample| sample.1).collect();
    let control_summary = latency_summary(&control_values);
    let attested_summary = latency_summary(&attested_values);
    let control_p99 = control_summary["p99_us"].as_u64().context("control p99")?;
    let attested_p99 = attested_summary["p99_us"]
        .as_u64()
        .context("attested p99")?;
    let budget_met = attested_p99 < 10_000;
    let report = json!({
        "schema": "orrery.p5-honest-trade-verification-overhead/1",
        "measurement_valid": true,
        "method": {
            "attestations": "pre-built",
            "claim": "verification overhead",
            "not_end_to_end": [
                "witness discovery",
                "request/response latency",
                "witness execution",
                "witness signing",
                "retries",
                "quorum collection"
            ],
            "paired": true,
            "fresh_item_per_sample": true,
            "control_posture": "shadow",
            "attested_posture": "required",
            "attestations_per_attested_intent": WITNESS_QUORUM_K,
            "cryptographically_verified_attestations": attestations_verified,
            "distinct_non_party_witness_accounts": witness_keys.len(),
            "concurrency_per_population": args.concurrency,
            "combined_inflight_pairs": args.concurrency,
        },
        "populations": {
            "control": {
                "series": "honest_trade_unattested_control_commit_ms",
                "samples": args.samples,
                "committed": control.len(),
                "durable_receipts": control_receipts,
                "latency": control_summary,
                "gateway_stages": control_stage,
            },
            "attested": {
                "series": "honest_trade_attested_verification_commit_ms",
                "samples": args.samples,
                "committed": attested.len(),
                "durable_receipts": attested_receipts,
                "latency": attested_summary,
                "gateway_stages": attested_stage,
            }
        },
        "delta": {
            "p99_us": i128::from(attested_p99) - i128::from(control_p99),
            "admit_mean_us": signed_stage_mean_delta(
                &args.control_stages,
                &args.attested_stages,
                "admit_us_sum"
            )?,
        },
        "budget": {
            "intent_commit_p99_us": 10_000,
            "comparison": "strictly_less_than",
            "attested_p99_us": attested_p99,
            "result": if budget_met { "pass" } else { "miss" },
        },
        "recorded_at_unix_ms": unix_ms(),
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    std::fs::write(&args.report, &encoded)
        .with_context(|| format!("write measurement report {}", args.report.display()))?;
    println!("{}", String::from_utf8_lossy(&encoded));
    Ok(())
}

async fn submit_timed(session: &Session, intent: Intent) -> Result<(IntentOutcome, u64)> {
    let started = Instant::now();
    let outcome = submit(session, intent).await?;
    Ok((outcome, elapsed_us(started)))
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn latency_summary(samples: &[u64]) -> Value {
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let percentile = |numerator: usize, denominator: usize| {
        let rank = (ordered.len() * numerator).div_ceil(denominator);
        ordered[rank.saturating_sub(1)]
    };
    json!({
        "unit": "microseconds",
        "min_us": ordered[0],
        "mean_us": ordered.iter().sum::<u64>() / ordered.len() as u64,
        "p50_us": percentile(50, 100),
        "p90_us": percentile(90, 100),
        "p99_us": percentile(99, 100),
        "max_us": ordered[ordered.len() - 1],
    })
}

fn read_stage_totals(path: &Path, scope: &str) -> Result<std::collections::BTreeMap<String, u64>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read stage metrics {}", path.display()))?;
    let mut totals = std::collections::BTreeMap::<String, u64>::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some("gateway_intent_stage")
            || record.get("scope").and_then(Value::as_str) != Some(scope)
        {
            continue;
        }
        let Some(fields) = record.as_object() else {
            continue;
        };
        for (key, value) in fields {
            let Some(value) = value.as_u64() else {
                continue;
            };
            if key.ends_with("_max") || key == "fence_read_max_us" {
                totals
                    .entry(key.clone())
                    .and_modify(|current| *current = (*current).max(value))
                    .or_insert(value);
            } else {
                *totals.entry(key.clone()).or_default() += value;
            }
        }
    }
    Ok(totals)
}

fn stage_report(path: &Path, expected: usize) -> Result<Value> {
    let all = read_stage_totals(path, "all")?;
    let intents = all.get("intents").copied().unwrap_or(0);
    let executed = all.get("executed").copied().unwrap_or(0);
    anyhow::ensure!(
        intents == expected as u64 && executed == expected as u64,
        "stage population in {} is intents={intents} executed={executed}, expected={expected}",
        path.display()
    );
    let mean = |key: &str, denominator: u64| all.get(key).copied().unwrap_or(0) / denominator;
    Ok(json!({
        "intents": intents,
        "executed": executed,
        "attempts": all.get("attempts").copied().unwrap_or(0),
        "mean_us": {
            "ingress": mean("ingress_us_sum", intents),
            "admit": mean("admit_us_sum", intents),
            "spawn_wait": mean("spawn_wait_us_sum", intents),
            "exec": mean("exec_us_sum", intents),
            "server": mean("server_us_sum", intents),
            "reply": mean("reply_us_sum", intents),
            "server_gap": mean("server_gap_us_sum", intents),
            "alloc_wait": mean("alloc_wait_us_sum", executed),
            "alloc_refill": mean("alloc_refill_us_sum", executed),
            "grv": mean("grv_us_sum", executed),
            "idem_read": mean("idem_read_us_sum", executed),
            "fence": mean("fence_us_sum", executed),
            "commit": mean("commit_us_sum", executed),
            "backoff": mean("backoff_us_sum", executed),
            "fdb_gap": mean("fdb_gap_us_sum", executed),
        },
        "max_us": {
            "admit": all.get("admit_us_max").copied().unwrap_or(0),
            "exec": all.get("exec_us_max").copied().unwrap_or(0),
            "server": all.get("server_us_max").copied().unwrap_or(0),
            "commit": all.get("commit_us_max").copied().unwrap_or(0),
        }
    }))
}

fn signed_stage_mean_delta(control: &Path, attested: &Path, key: &str) -> Result<i128> {
    let control = read_stage_totals(control, "all")?;
    let attested = read_stage_totals(attested, "all")?;
    let control_n = control
        .get("intents")
        .copied()
        .context("control stage intents")?;
    let attested_n = attested
        .get("intents")
        .copied()
        .context("attested stage intents")?;
    Ok(
        i128::from(attested.get(key).copied().unwrap_or(0) / attested_n)
            - i128::from(control.get(key).copied().unwrap_or(0) / control_n),
    )
}

async fn seed_measurement_ledger(db: &Database, samples: usize) -> Result<()> {
    anyhow::ensure!(
        read_receipts(db).await?.is_empty(),
        "measurement requires a fresh throwaway cluster: ledger receipts already exist"
    );
    for key in [
        keyspace::ledger_bal_key(BUYER, AssetId(MEASURE_ASSET_BASE)).to_vec(),
        keyspace::ledger_bal_key(BUYER, AssetId(MEASURE_ASSET_BASE + 1)).to_vec(),
    ] {
        anyhow::ensure!(
            read_raw(db, key).await?.is_none(),
            "measurement balance rows already exist; cluster is not fresh"
        );
    }
    let count = samples
        .checked_mul(2)
        .and_then(|count| count.checked_add(4))
        .context("measurement item count overflow")?;
    let rows = (0..count)
        .map(|offset| {
            Ok::<_, anyhow::Error>((
                keyspace::ledger_item_key(ItemUid::new(
                    MEASURE_ITEM_BASE + u64::try_from(offset).context("item offset")?,
                ))
                .to_vec(),
                postcard::to_stdvec(&keyspace::ItemRow {
                    owner: SELLER,
                    state: b"p5-honest-trade-measurement".to_vec(),
                })?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let balance = i128::from(PRICE);
    db.run(|trx, _| {
        let rows = rows.clone();
        async move {
            for (key, value) in rows {
                trx.set(&key, &value);
            }
            for offset in 0..count {
                let asset = AssetId(MEASURE_ASSET_BASE + offset as u64);
                trx.set(
                    &keyspace::ledger_bal_key(BUYER, asset),
                    &balance.to_le_bytes(),
                );
            }
            Ok(())
        }
    })
    .await
    .map_err(|error: FdbBindingError| anyhow::anyhow!("seed measurement ledger: {error}"))
}

// ── The ramp arm (issue #222, D32 clause (b) and clause (e)'s sensitivity leg) ─
//
// Three claims, and the middle one is the one that rots quietly:
//
//   1. **Enforcing acts.** A synthetic offender — K cryptographically valid
//      co-signatures from announced witnesses, deliberately not the subset the
//      durable draw key names — is refused by the enforcing process with
//      `required_witness_missing`, and leaves no durable trace.
//   2. **Shadow observes.** The same offender, submitted to a gateway launched
//      in shadow, produces a `would_act = true` observation carrying that
//      *same* verdict label on the stable `orrery::ramp::shadow` target.
//   3. **Shadow does not act.** That intent's ack comes back `Committed`, its
//      `attest/` row carries `enforced: false`, the item moved, and across the
//      whole shadow run the count of refusals is zero.
//
// A gate that proved only (1) and (2) is the one #222 exists to prevent: a
// shadow arm that had quietly started refusing would pass it.
//
// Plus reversibility, which is the claim that cannot be made by launching a
// second process: the *enforcing* gateway is demoted while it runs, and the
// offender it refused a moment ago commits — inside D32 clause (c)'s 2 s bound
// — and then, promoted back, is refused again.
//
// ## Why the process log and not the in-process observer
//
// #217 left three surfaces. The in-process `CountingShadowObserver` is the
// cheapest and proves the least: it would live in *this* process, and a gate
// that instantiates its own validator has proved something about a validator
// rather than about a gateway anyone could deploy. The two used here are the
// out-of-process ones. The `tracing` events are what an operator actually has,
// need no wiring, and are read here through the same JSON log the dupe
// gauntlet already reads refusal causes from. The durable `attest/` row is the
// inertness half, and it is durable evidence rather than a log line: a shadow
// arm that had started acting could not fake a committed transfer.

struct RampArgs {
    enforcing: iroh::EndpointAddr,
    shadow: iroh::EndpointAddr,
    cluster_file: PathBuf,
    enforcing_log: PathBuf,
    shadow_log: PathBuf,
    posture_file: PathBuf,
    report: PathBuf,
}

/// One shadow observation, as an out-of-process reader recovers it.
#[derive(Debug, Clone)]
struct ObservedShadow {
    intent_id: u128,
    would_act: bool,
    verdict: String,
}

/// Every `orrery::ramp::shadow` event in a gateway's JSON process log.
///
/// Filtered on the **target**, which is why #217 made it a constant: a reader
/// matching the human-readable message would pass on the day somebody reworded
/// it, and would report an observing control as a silent one.
fn read_shadow_observations(path: &Path) -> Result<Vec<ObservedShadow>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read gateway log {}", path.display()))?;
    let mut seen = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("target").and_then(Value::as_str) != Some(SHADOW_TARGET) {
            continue;
        }
        let Some(fields) = value.get("fields") else {
            continue;
        };
        let intent_id = fields
            .get("intent_id")
            .and_then(|id| {
                id.as_u64()
                    .map(u128::from)
                    .or_else(|| id.as_str()?.parse::<u128>().ok())
            })
            .context("shadow observation has no intent_id")?;
        seen.push(ObservedShadow {
            intent_id,
            would_act: fields
                .get("would_act")
                .and_then(Value::as_bool)
                .context("shadow observation has no would_act")?,
            verdict: fields
                .get("verdict")
                .and_then(Value::as_str)
                .context("shadow observation has no verdict")?
                .to_owned(),
        });
    }
    Ok(seen)
}

/// An honestly attested trade, which is what makes a gateway's epoch cache
/// adopt the durable draw key.
///
/// Load-bearing rather than warm-up ceremony: the draw is keyed, and a
/// validator drawing over a locally generated key would refuse — or admit — a
/// different subset from the one the durable row names. Both gateways must
/// have converged before either judges the offender, or the two verdicts this
/// gate compares would be answers to two different questions.
fn ramp_warmup(
    issuer: &iroh::SecretKey,
    intent_id: u128,
    item: ItemUid,
    witness_keys: &[iroh::SecretKey],
) -> Intent {
    let mut intent = transfer_intent(issuer, intent_id, item, RAMP_ASSET);
    fully_attest(&mut intent, witness_keys);
    intent
}

/// The synthetic offender: exactly K valid, announced co-signatures, chosen to
/// be a *different* subset from the one the durable draw key names for this
/// intent id.
///
/// The same shape the dupe gauntlet's arm (c.4) uses, and deliberately so:
/// D32 clause (b) says a shadow verdict carries the exact `RejectionCause`
/// label `Required` returns, so the gate can only check that by refusing the
/// intent one way and observing it the other.
/// Choose the offender's witness subset: `WITNESS_QUORUM_K` announced
/// witnesses that do **not** cover `required`.
///
/// Split out from [`ramp_offender`] so the one thing that can silently
/// un-offend the offender is directly testable.
///
/// The comparison is by **set**. What makes the intent an offender is that a
/// required witness is absent from the submitted members; the quorum check is
/// membership and does not care about order. `required_witnesses` returns its
/// draw in draw order while the leading-K slice is in eligible order, so an
/// ordered `==` is false for every permutation of a colliding set: the guard
/// would not fire, the check below would pass, and the harness would submit a
/// fully-witnessed intent as its offender. That intent commits, the
/// `enforcing_acts` arm fails, and the report blames a gate that is behaving
/// correctly.
///
/// Rate, so this is not dismissed as theoretical: the draw takes
/// `WITNESS_QUORUM_K` = 3 of the 7 announced witnesses, so it collides with the
/// leading 3 once in `C(7,3)` = 35 runs, and 5 of the 6 orderings of a
/// colliding set are permutations rather than the identity — 5/210, about one
/// run in 42. That is what failed run 32614843047.
fn offender_members(eligible: &[NodeId], required: &[NodeId]) -> Result<Vec<NodeId>> {
    fn same_set(a: &[NodeId], b: &[NodeId]) -> bool {
        let (mut a, mut b) = (a.to_vec(), b.to_vec());
        a.sort_unstable();
        b.sort_unstable();
        a == b
    }

    anyhow::ensure!(
        eligible.len() > WITNESS_QUORUM_K,
        "need more than WITNESS_QUORUM_K announced witnesses to build a \
         non-covering subset"
    );
    let mut members: Vec<NodeId> = eligible.iter().copied().take(WITNESS_QUORUM_K).collect();
    if same_set(&members, required) {
        members[WITNESS_QUORUM_K - 1] = eligible[WITNESS_QUORUM_K];
    }
    anyhow::ensure!(
        !same_set(&members, required) && members.len() == WITNESS_QUORUM_K,
        "the offender's subset is the required one; it would not offend"
    );
    Ok(members)
}

fn ramp_offender(
    issuer: &iroh::SecretKey,
    intent_id: u128,
    item: ItemUid,
    draw_key: &[u8; 32],
    eligible: &[NodeId],
    witness_keys: &[iroh::SecretKey],
) -> Result<(Intent, Vec<NodeId>, Vec<NodeId>)> {
    let required = required_witnesses(draw_key, intent_id, eligible);
    let members = offender_members(eligible, &required)?;
    let mut intent = transfer_intent(issuer, intent_id, item, RAMP_ASSET);
    for node in &members {
        let key = witness_keys
            .iter()
            .find(|key| key.public() == *node)
            .context("offender member is not announced")?;
        intent.attestations.push(intent.attest(key));
    }
    anyhow::ensure!(
        intent
            .attestations
            .iter()
            .all(|attestation| attestation.verify(&intent)),
        "the offender's co-signatures must be cryptographically valid, or the \
         refusal proves signature checking rather than the quorum"
    );
    Ok((intent, members, required))
}

/// Submit `intent` until the outcome the caller is waiting for arrives, and
/// report how long the wait was.
///
/// The measurement D32 clause (c) is bounded on. A refused intent burns no
/// idempotency row, so resubmitting the same id is a fresh judgement rather
/// than a replay — which is what makes this a poll rather than a sleep, and
/// the difference is that a sleep would report the bound instead of measuring
/// against it.
async fn poll_until_committed(
    session: &Session,
    intent: &Intent,
    budget: Duration,
) -> Result<(IntentOutcome, u64)> {
    let started = std::time::Instant::now();
    loop {
        let outcome = submit(session, intent.clone()).await?;
        let elapsed = started.elapsed();
        // Exhausting the budget is a *failed arm*, not a harness error: the
        // report is the evidence, and a gate that died here would name the
        // timeout instead of naming the control that went on acting. The
        // caller's assertion — committed, inside the bound — is what fails.
        if matches!(outcome, IntentOutcome::Committed { .. }) || elapsed >= budget {
            return Ok((outcome, elapsed.as_millis() as u64));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for a gateway to record that it applied a posture, and report how long
/// it took.
///
/// The promotion direction cannot be polled the way the demotion direction is,
/// and the asymmetry is the mechanism rather than an inconvenience: a refused
/// intent burns no idempotency row, so resubmitting it under `required` is a
/// fresh judgement — but a *committed* one is durable, so an offender polled
/// across the promotion boundary would commit on its first attempt and then be
/// unusable as a probe. So the latency comes from the process's own record and
/// the *effect* is proved by the single submission after it, which must be
/// refused with no durable trace.
fn wait_for_posture(log: &Path, mode: &str, budget: Duration) -> Result<u64> {
    let started = std::time::Instant::now();
    loop {
        let text = std::fs::read_to_string(log)
            .with_context(|| format!("read gateway log {}", log.display()))?;
        let mut applied = text.lines().filter_map(|line| {
            let value = serde_json::from_str::<Value>(line).ok()?;
            let fields = value.get("fields")?;
            (fields.get("message").and_then(Value::as_str) == Some("ramp posture applied")).then(
                || {
                    fields
                        .get("mode")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                },
            )?
        });
        // Same discipline as `poll_until_committed`: a posture that never
        // applied returns a latency past the bound, which fails the arm and
        // says so in the report, rather than killing the harness.
        if applied.next_back().as_deref() == Some(mode) || started.elapsed() >= budget {
            return Ok(started.elapsed().as_millis() as u64);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

async fn run_ramp(args: RampArgs) -> Result<()> {
    let db = FdbIntentExecutor::connect(
        args.cluster_file
            .to_str()
            .context("cluster-file path is not UTF-8")?,
        GRID,
    )?
    .database()
    .clone();
    seed_ramp_ledger(&db).await?;
    std::fs::write(&args.posture_file, "required\n")
        .with_context(|| format!("seed the posture file {}", args.posture_file.display()))?;

    let issuer = secret(1);
    let witness_keys = witnesses();
    let selected: Vec<NodeId> = witness_keys.iter().map(iroh::SecretKey::public).collect();

    let enforcing = connect_session(&issuer, SessionStanding::Good, args.enforcing.clone()).await?;
    present_epoch(&enforcing, announcement(&selected)?).await?;
    let shadow = connect_session(&issuer, SessionStanding::Good, args.shadow.clone()).await?;
    present_epoch(&shadow, announcement(&selected)?).await?;

    // Both caches converge on the durable draw key before anything is drawn
    // against it, and the shadow warm-up doubles as the `would_act = false`
    // observation that keeps this gate's "shadow refuses nothing" from being a
    // claim about a control that observed nothing either.
    let enforcing_warmup = submit(
        &enforcing,
        ramp_warmup(
            &issuer,
            RAMP_ENFORCING_WARMUP_INTENT,
            ItemUid::new(151_301),
            &witness_keys,
        ),
    )
    .await?;
    let shadow_warmup = submit(
        &shadow,
        ramp_warmup(
            &issuer,
            RAMP_SHADOW_WARMUP_INTENT,
            ItemUid::new(151_302),
            &witness_keys,
        ),
    )
    .await?;

    let epoch_row: keyspace::EpochRow = decode_required(
        &db,
        keyspace::epoch_key(GRID, CELL, EPOCH).to_vec(),
        "durable epoch row",
    )
    .await?;
    let eligible = selected.clone();

    // (1) Enforcing acts.
    let (offender, submitted, required) = ramp_offender(
        &issuer,
        RAMP_ENFORCING_OFFENDER_INTENT,
        ItemUid::new(151_303),
        &epoch_row.draw_key,
        &eligible,
        &witness_keys,
    )?;
    let enforcing_outcome = submit(&enforcing, offender).await?;

    // (2) and (3): the same traffic, judged by the shadow process.
    let (shadow_offender, _, _) = ramp_offender(
        &issuer,
        RAMP_SHADOW_OFFENDER_INTENT,
        ItemUid::new(151_304),
        &epoch_row.draw_key,
        &eligible,
        &witness_keys,
    )?;
    let shadow_outcome = submit(&shadow, shadow_offender).await?;

    // (4) Reversibility, on the process that was acting a moment ago. The
    // posture write is the operator's act; everything after it is the fleet's.
    let (demoted_offender, _, _) = ramp_offender(
        &issuer,
        RAMP_DEMOTED_OFFENDER_INTENT,
        ItemUid::new(151_305),
        &epoch_row.draw_key,
        &eligible,
        &witness_keys,
    )?;
    std::fs::write(&args.posture_file, "shadow\n").context("demote the enforcing gateway")?;
    let (demoted_outcome, demote_ms) = poll_until_committed(
        &enforcing,
        &demoted_offender,
        Duration::from_millis(RAMP_APPLY_BOUND_MS * 5),
    )
    .await?;

    // And back, because a lever that only moves one way is a collapse rather
    // than a ramp: D32 clause (f) makes promotion an operator act precisely
    // because demotion is automatable, and a gate that never promoted could
    // not tell a reversible control from a broken one.
    let (repromoted_offender, _, _) = ramp_offender(
        &issuer,
        RAMP_REPROMOTED_OFFENDER_INTENT,
        ItemUid::new(151_306),
        &epoch_row.draw_key,
        &eligible,
        &witness_keys,
    )?;
    std::fs::write(&args.posture_file, "required\n").context("promote the gateway back")?;
    let promote_ms = wait_for_posture(
        &args.enforcing_log,
        "required",
        Duration::from_millis(RAMP_APPLY_BOUND_MS * 5),
    )?;
    let repromoted_outcome = submit(&enforcing, repromoted_offender).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let shadow_observations = read_shadow_observations(&args.shadow_log)?;
    let enforcing_observations = read_shadow_observations(&args.enforcing_log)?;
    let shadow_refusals = read_refusal_audit(&args.shadow_log)?;
    let enforcing_refusals = read_refusal_audit(&args.enforcing_log)?;

    let observation_of =
        |seen: &[ObservedShadow], id: u128| seen.iter().find(|row| row.intent_id == id).cloned();

    // ── (1) ─────────────────────────────────────────────────────────────────
    let enforcing_durable = rejected_without_state(
        &db,
        &read_receipts(&db).await?,
        RAMP_ENFORCING_OFFENDER_INTENT,
    )
    .await?;
    let enforcing_cause = enforcing_refusals
        .get(&RAMP_ENFORCING_OFFENDER_INTENT)
        .cloned();
    let enforcing_acts = rejected_as(
        &enforcing_outcome,
        orrery_protocol::REASON_ATTESTATION_QUORUM,
    ) && enforcing_cause.as_deref() == Some("required_witness_missing")
        && enforcing_durable == (true, true, 0)
        && matches!(enforcing_warmup, IntentOutcome::Committed { .. });

    // ── (2) ─────────────────────────────────────────────────────────────────
    let shadow_offender_seen = observation_of(&shadow_observations, RAMP_SHADOW_OFFENDER_INTENT);
    let shadow_warmup_seen = observation_of(&shadow_observations, RAMP_SHADOW_WARMUP_INTENT);
    let shadow_observes = shadow_offender_seen
        .as_ref()
        .is_some_and(|row| row.would_act && row.verdict == "required_witness_missing")
        // The predicate ran in full on honest traffic too, which is what
        // separates a shadow that is observing from one that only wakes up for
        // the traffic a gate injects.
        && shadow_warmup_seen
            .as_ref()
            .is_some_and(|row| !row.would_act);

    // ── (3) ─────────────────────────────────────────────────────────────────
    let receipts = read_receipts(&db).await?;
    let shadow_receipts = receipts
        .iter()
        .filter(|row| row.intent_id == RAMP_SHADOW_OFFENDER_INTENT)
        .count();
    let shadow_attest = read_attest_enforced(&db, RAMP_SHADOW_OFFENDER_INTENT).await?;
    let shadow_would_act = shadow_observations
        .iter()
        .filter(|row| row.would_act)
        .count();
    let shadow_does_not_act = matches!(shadow_outcome, IntentOutcome::Committed { .. })
        && matches!(shadow_warmup, IntentOutcome::Committed { .. })
        && shadow_attest == Some(false)
        && read_item_owner(&db, ItemUid::new(151_304)).await? == Some(BUYER)
        && shadow_receipts == 1
        // The pair a single counter cannot express: nothing refused, and
        // something that would have been. Either half alone is satisfied by a
        // control that is simply off.
        && shadow_refusals.is_empty()
        && shadow_would_act > 0;

    // ── (4) ─────────────────────────────────────────────────────────────────
    let demoted_attest = read_attest_enforced(&db, RAMP_DEMOTED_OFFENDER_INTENT).await?;
    let demoted_seen = observation_of(&enforcing_observations, RAMP_DEMOTED_OFFENDER_INTENT);
    let repromoted_durable =
        rejected_without_state(&db, &receipts, RAMP_REPROMOTED_OFFENDER_INTENT).await?;
    let reversible = matches!(demoted_outcome, IntentOutcome::Committed { .. })
        && demote_ms <= RAMP_APPLY_BOUND_MS
        // The commit half of the control moved with the admission half. A
        // demotion that reached only the validator would have committed this
        // intent with `enforced: true`, or refused it at commit.
        && demoted_attest == Some(false)
        && demoted_seen
            .as_ref()
            .is_some_and(|row| row.would_act && row.verdict == "required_witness_missing")
        && rejected_as(
            &repromoted_outcome,
            orrery_protocol::REASON_ATTESTATION_QUORUM,
        )
        && promote_ms <= RAMP_APPLY_BOUND_MS
        && repromoted_durable == (true, true, 0);

    let passed = enforcing_acts && shadow_observes && shadow_does_not_act && reversible;
    let node_strings = |nodes: &[NodeId]| nodes.iter().map(ToString::to_string).collect::<Vec<_>>();
    let observed_json = |row: &Option<ObservedShadow>| match row {
        Some(row) => json!({
            "intent_id": row.intent_id.to_string(),
            "would_act": row.would_act,
            "verdict": row.verdict
        }),
        None => Value::Null,
    };
    let report = json!({
        "schema": "orrery.p5-ramp-shadow/2",
        "result": if passed { "pass" } else { "fail" },
        "control": "attestation_quorum",
        "observation_surface": {
            "tracing_target": SHADOW_TARGET,
            "durable_row": "attest/{intent_id}.enforced",
            "in_process_observer": false
        },
        "gateways": {
            "process_boundary": true,
            "wire": "iroh",
            "fdb": true,
            "enforcing_started_as": "required",
            "shadow_started_as": "shadow"
        },
        "epoch": {
            "grid": GRID.0,
            "cell": CELL.to_bits(),
            "epoch": EPOCH,
            "handle": HANDLE,
            "announced_witnesses": node_strings(&selected),
            "draw_key_audited_from_fdb": true
        },
        "arms": {
            "enforcing_acts": {
                "passed": enforcing_acts,
                "intent_id": RAMP_ENFORCING_OFFENDER_INTENT.to_string(),
                "outcome": format!("{enforcing_outcome:?}"),
                "audit_cause": enforcing_cause,
                "submitted_witnesses": node_strings(&submitted),
                "required_witnesses": node_strings(&required),
                "intent_rows": usize::from(!enforcing_durable.0),
                "attest_rows": usize::from(!enforcing_durable.1),
                "ledger_receipts": enforcing_durable.2
            },
            "shadow_observes": {
                "passed": shadow_observes,
                // Everything below is explanatory evidence, not another gate
                // predicate. Keep it structurally separate from `passed`: this
                // cross-gateway comparison is useful when diagnosing a failure,
                // but it must not be mistaken for the arm's verdict.
                "diagnostics": {
                    "intent_id": RAMP_SHADOW_OFFENDER_INTENT.to_string(),
                    "offender_observation": observed_json(&shadow_offender_seen),
                    "honest_observation": observed_json(&shadow_warmup_seen),
                    "cross_gateway_verdict_matches_enforcing_audit_cause":
                        shadow_offender_seen.as_ref().map(|row| row.verdict.clone())
                            == enforcing_refusals.get(&RAMP_ENFORCING_OFFENDER_INTENT).cloned()
                }
            },
            "shadow_does_not_act": {
                "passed": shadow_does_not_act,
                "intent_id": RAMP_SHADOW_OFFENDER_INTENT.to_string(),
                "outcome": format!("{shadow_outcome:?}"),
                "attest_row_enforced": shadow_attest,
                "durable_item_owner": read_item_owner(&db, ItemUid::new(151_304))
                    .await?
                    .map(|owner| owner.0),
                "ledger_receipts": shadow_receipts,
                "refusals_in_shadow_run": shadow_refusals.len(),
                "would_act_observations": shadow_would_act,
                "observations": shadow_observations.len()
            },
            "reversibility": {
                "passed": reversible,
                "apply_bound_ms": RAMP_APPLY_BOUND_MS,
                "demotion": {
                    "intent_id": RAMP_DEMOTED_OFFENDER_INTENT.to_string(),
                    "apply_ms": demote_ms,
                    "outcome": format!("{demoted_outcome:?}"),
                    "attest_row_enforced": demoted_attest,
                    "observation": observed_json(&demoted_seen)
                },
                "promotion": {
                    "intent_id": RAMP_REPROMOTED_OFFENDER_INTENT.to_string(),
                    "apply_ms": promote_ms,
                    "outcome": format!("{repromoted_outcome:?}"),
                    "intent_rows": usize::from(!repromoted_durable.0),
                    "attest_rows": usize::from(!repromoted_durable.1),
                    "ledger_receipts": repromoted_durable.2
                }
            }
        },
        // D32 clause (c)'s inventory has five controls and only C1 and C2
        // exist; C3, C4 and C5 "do not exist yet" and their flags "gate
        // nothing". #222's acceptance list asks this gate to prove no lease was
        // revoked and no authority correction broadcast, and there is no such
        // control in the tree to prove it of. Named here rather than silently
        // skipped, so the hole is visible to whoever promotes.
        "controls_not_yet_built": ["write_annulment", "authority_correction", "strikes"]
    });
    let encoded = serde_json::to_vec_pretty(&report)?;
    std::fs::write(&args.report, &encoded)
        .with_context(|| format!("write report {}", args.report.display()))?;
    println!("{}", String::from_utf8_lossy(&encoded));
    anyhow::ensure!(passed, "one or more ramp arms failed");
    Ok(())
}

/// The `enforced` marker on an intent's durable attestation row, and `None`
/// when there is no row at all.
///
/// The two are different facts and the gate needs both: `off` writes no row,
/// `shadow` writes one saying the cluster did not stand behind the quorum, and
/// `required` writes one saying it did. Collapsing the absent row into `false`
/// would let an `off` gateway pass the shadow arm.
async fn read_attest_enforced(db: &Database, intent_id: u128) -> Result<Option<bool>> {
    let Some(raw) = read_raw(db, keyspace::attest_key(intent_id).to_vec()).await? else {
        return Ok(None);
    };
    let row: keyspace::AttestRow = postcard::from_bytes(&raw).context("decode attest row")?;
    Ok(Some(row.enforced))
}

async fn seed_ramp_ledger(db: &Database) -> Result<()> {
    let items = [151_301u64, 151_302, 151_303, 151_304, 151_305, 151_306];
    for item in items {
        anyhow::ensure!(
            read_raw(db, keyspace::ledger_item_key(ItemUid::new(item)).to_vec())
                .await?
                .is_none(),
            "ledger item {item} already exists; the gate requires a fresh throwaway cluster"
        );
    }
    let rows = items
        .into_iter()
        .map(|item| {
            Ok::<_, anyhow::Error>((
                keyspace::ledger_item_key(ItemUid::new(item)).to_vec(),
                postcard::to_stdvec(&keyspace::ItemRow {
                    owner: SELLER,
                    state: b"p5-ramp-shadow".to_vec(),
                })?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    db.run(|trx, _| {
        let rows = rows.clone();
        async move {
            for (key, value) in rows {
                trx.set(&key, &value);
            }
            trx.set(
                &keyspace::ledger_bal_key(BUYER, RAMP_ASSET),
                &STARTING_BALANCE.to_le_bytes(),
            );
            Ok(())
        }
    })
    .await
    .map_err(|error: FdbBindingError| anyhow::anyhow!("seed ramp ledger: {error}"))
}

fn forged_empty_intent(issuer: &iroh::SecretKey, intent_id: u128) -> Intent {
    let witness = secret(149);
    let mut intent = Intent {
        evidence: None,
        intent_id,
        issuer: issuer.public(),
        cell_epoch: CellEpoch::new(HANDLE),
        ops: Vec::new(),
        attestations: Vec::new(),
        signature: issuer.sign(b"placeholder"),
    };
    intent.sign(issuer);
    intent.attestations.push(orrery_protocol::Attestation {
        witness: witness.public(),
        signature: witness.sign(b"forged attestation"),
    });
    intent
}

fn rejected_as(outcome: &IntentOutcome, expected: u16) -> bool {
    matches!(outcome, IntentOutcome::Rejected { reason } if *reason == expected)
}

fn refusal_json(
    passed: bool,
    intent_id: u128,
    outcome: &IntentOutcome,
    cause: Option<String>,
    durable: (bool, bool, usize),
    proof: Value,
) -> Value {
    json!({
        "passed": passed,
        "intent_id": intent_id.to_string(),
        "outcome": format!("{outcome:?}"),
        "audit_cause": cause,
        "intent_rows": usize::from(!durable.0),
        "attest_rows": usize::from(!durable.1),
        "ledger_receipts": durable.2,
        "proof": proof
    })
}

async fn seed_ledger(db: &Database) -> Result<()> {
    let items = [151_201u64, 151_202, 151_203, 151_204, 151_205, 151_206];
    for item in items {
        anyhow::ensure!(
            read_raw(db, keyspace::ledger_item_key(ItemUid::new(item)).to_vec())
                .await?
                .is_none(),
            "ledger item {item} already exists; the gate requires a fresh throwaway cluster"
        );
    }
    let rows = items
        .into_iter()
        .map(|item| {
            Ok::<_, anyhow::Error>((
                keyspace::ledger_item_key(ItemUid::new(item)).to_vec(),
                postcard::to_stdvec(&keyspace::ItemRow {
                    owner: SELLER,
                    state: b"p5-dupe-gauntlet".to_vec(),
                })?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    db.run(|trx, _| {
        let rows = rows.clone();
        async move {
            for (key, value) in rows {
                trx.set(&key, &value);
            }
            for asset in [REPLAY_ASSET, CONTROL_ASSET] {
                trx.set(
                    &keyspace::ledger_bal_key(BUYER, asset),
                    &STARTING_BALANCE.to_le_bytes(),
                );
            }
            Ok(())
        }
    })
    .await
    .map_err(|error: FdbBindingError| anyhow::anyhow!("seed gauntlet ledger: {error}"))
}

async fn read_raw(db: &Database, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
    db.run(|trx, _| {
        let key = key.clone();
        async move { Ok(trx.get(&key, false).await?.map(|value| value.to_vec())) }
    })
    .await
    .map_err(|error: FdbBindingError| anyhow::anyhow!("read FDB key: {error}"))
}

async fn rejected_without_state(
    db: &Database,
    receipts: &[keyspace::ReceiptRow],
    intent_id: u128,
) -> Result<(bool, bool, usize)> {
    let intent = read_raw(db, keyspace::intent_key(intent_id).to_vec()).await?;
    let attest = read_raw(db, keyspace::attest_key(intent_id).to_vec()).await?;
    let receipt_count = receipts
        .iter()
        .filter(|row| row.intent_id == intent_id)
        .count();
    Ok((intent.is_none(), attest.is_none(), receipt_count))
}

async fn decode_required<T>(db: &Database, key: Vec<u8>, label: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let raw = read_raw(db, key)
        .await?
        .with_context(|| format!("{label} is absent"))?;
    postcard::from_bytes(&raw).with_context(|| format!("decode {label}"))
}

async fn read_item_owner(db: &Database, item: ItemUid) -> Result<Option<AccountId>> {
    let Some(raw) = read_raw(db, keyspace::ledger_item_key(item).to_vec()).await? else {
        return Ok(None);
    };
    let row: keyspace::ItemRow = postcard::from_bytes(&raw).context("decode item row")?;
    Ok(Some(row.owner))
}

async fn read_balance(db: &Database, account: AccountId, asset: AssetId) -> Result<i128> {
    let Some(raw) = read_raw(db, keyspace::ledger_bal_key(account, asset).to_vec()).await? else {
        return Ok(0);
    };
    anyhow::ensure!(raw.len() <= 16, "balance row is wider than i128");
    let mut bytes = [0u8; 16];
    bytes[..raw.len()].copy_from_slice(&raw);
    Ok(i128::from_le_bytes(bytes))
}

async fn read_receipts(db: &Database) -> Result<Vec<keyspace::ReceiptRow>> {
    let begin = keyspace::ledger_receipt_key().to_vec();
    let mut end = begin.clone();
    end[1] = b's';
    let values = db
        .run(|trx, _| {
            let begin = begin.clone();
            let end = end.clone();
            async move {
                let range = RangeOption {
                    begin: KeySelector::first_greater_or_equal(&begin),
                    end: KeySelector::first_greater_or_equal(&end),
                    mode: StreamingMode::WantAll,
                    ..RangeOption::default()
                };
                let mut stream = trx.get_ranges_keyvalues(range, false);
                let mut values = Vec::new();
                while let Some(kv) = stream.try_next().await? {
                    values.push(kv.value().to_vec());
                }
                Ok(values)
            }
        })
        .await
        .map_err(|error: FdbBindingError| anyhow::anyhow!("scan ledger receipts: {error}"))?;
    values
        .into_iter()
        .map(|value| postcard::from_bytes(&value).context("decode receipt row"))
        .collect()
}

fn read_refusal_audit(path: &Path) -> Result<std::collections::BTreeMap<u128, String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read gateway audit log {}", path.display()))?;
    let mut audit = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(fields) = value.get("fields") else {
            continue;
        };
        if fields.get("message").and_then(Value::as_str) != Some("intent admission refused") {
            continue;
        }
        let Some(cause) = fields.get("cause").and_then(Value::as_str) else {
            continue;
        };
        let intent_id = fields
            .get("intent_id")
            .and_then(|id| {
                id.as_u64()
                    .map(u128::from)
                    .or_else(|| id.as_str()?.parse::<u128>().ok())
            })
            .context("refusal audit record has no intent_id")?;
        audit.insert(intent_id, cause.to_owned());
    }
    Ok(audit)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(seed: u8) -> NodeId {
        secret(seed).public()
    }

    /// The regression from run 32614843047: `required` was a *permutation* of
    /// the leading `WITNESS_QUORUM_K` eligible witnesses, not a different set.
    ///
    /// An ordered `members == required` is false here, so the old guard did not
    /// fire and the old `ensure!` did not trip. The harness then submitted an
    /// intent carrying every required witness as its "offender", the gateway
    /// correctly committed it, and `enforcing_acts` failed — blaming a gate
    /// that was right.
    #[test]
    fn a_permuted_required_set_still_forces_a_non_covering_subset() {
        let eligible: Vec<NodeId> = (100..107).map(node).collect();
        let required = vec![eligible[0], eligible[2], eligible[1]];

        // The precondition that made the old comparison useless.
        assert_ne!(
            eligible[..WITNESS_QUORUM_K].to_vec(),
            required,
            "this case is only interesting when an ordered comparison says \
             they differ"
        );

        let members = offender_members(&eligible, &required).expect("subset");

        let mut got = members.clone();
        let mut want = required.clone();
        got.sort_unstable();
        want.sort_unstable();
        assert_ne!(
            got, want,
            "the offender must omit a required witness, or it does not offend"
        );
        assert_eq!(members.len(), WITNESS_QUORUM_K);
    }

    /// The ordinary case: the draw genuinely differs, and the leading slice is
    /// already a valid offender, so nothing is substituted.
    #[test]
    fn a_disjoint_required_set_leaves_the_leading_subset_alone() {
        let eligible: Vec<NodeId> = (100..107).map(node).collect();
        let required = vec![eligible[3], eligible[4], eligible[5]];

        let members = offender_members(&eligible, &required).expect("subset");
        assert_eq!(members, eligible[..WITNESS_QUORUM_K].to_vec());
    }

    #[test]
    fn latency_summary_uses_nearest_rank_over_a_real_p99_population() {
        let samples: Vec<u64> = (1..=10_000).collect();
        let summary = latency_summary(&samples);

        assert_eq!(summary["p50_us"], 5_000);
        assert_eq!(summary["p90_us"], 9_000);
        assert_eq!(summary["p99_us"], 9_900);
        assert_eq!(summary["max_us"], 10_000);
    }

    #[test]
    fn stage_report_refuses_a_population_with_missing_samples() {
        let path = std::env::temp_dir().join(format!(
            "orrery-p5-stage-report-{}-{}.jsonl",
            std::process::id(),
            unix_ms()
        ));
        std::fs::write(
            &path,
            r#"{"type":"gateway_intent_stage","scope":"all","intents":9999,"executed":9999,"admit_us_sum":9999}"#,
        )
        .expect("write stage fixture");

        let error = stage_report(&path, 10_000).expect_err("missing sample must fail");
        assert!(error.to_string().contains("expected=10000"));
        std::fs::remove_file(path).expect("remove stage fixture");
    }
}
