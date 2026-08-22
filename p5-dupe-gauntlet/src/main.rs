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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use foundationdb::options::StreamingMode;
use foundationdb::{Database, FdbBindingError, KeySelector, RangeOption};
use futures::TryStreamExt;
use orrery_persistd::gateway::{
    InterestAuthority, SessionTokenV1Authorizer, SharedBindingAuthority, SnapshotBindingAuthority,
};
use orrery_persistd::intent::{BaselineIntentValidator, ItemTransferArgs, LEDGER_ITEM_TRANSFER_OP};
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

#[derive(Debug, Parser)]
#[command(name = "p5-dupe-gauntlet")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the enforcing persistence gateway until interrupted.
    Gateway {
        #[arg(long)]
        cluster_file: PathBuf,
        #[arg(long)]
        data_dir: PathBuf,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Gateway {
            cluster_file,
            data_dir,
        } => run_gateway(&cluster_file, &data_dir).await,
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

fn runtime_config(data_dir: &Path) -> RuntimeConfig {
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
        node_id: 151,
        epoch: Epoch::new(1),
        fence: Arc::new(MemFenceStore::new()),
    }
}

async fn run_gateway(cluster_file: &Path, data_dir: &Path) -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter("orrery_persistd=debug,p5_dupe_gauntlet=info")
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("install gateway tracing: {error}"))?;

    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create gateway data dir {}", data_dir.display()))?;
    let epochs = Arc::new(WitnessEpochAuthority::new([IssuerKey::new(
        IssuerKeyId::new(1),
        coordinator().public(),
    )]));
    let executor = FdbIntentExecutor::connect(
        cluster_file
            .to_str()
            .context("cluster-file path is not UTF-8")?,
        GRID,
    )?
    .recording_epochs(Arc::clone(&epochs), witness_bindings());

    let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
        Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
    let runtime = Arc::new(Mutex::new(
        CellRuntime::open(&runtime_config(data_dir), &store)
            .await
            .context("open gauntlet runtime")?,
    ));
    let router: Arc<dyn Router> = runtime;

    let config = GatewayConfig {
        secret_key: Some(secret(250)),
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
        validator: Arc::new(BaselineIntentValidator::enforcing(
            Arc::clone(&epochs),
            Arc::new(CoverAllInterest),
            witness_bindings(),
        )),
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
            "enforcement": "required",
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
