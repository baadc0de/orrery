//! The intent execution path, end to end (D11 §2.2, §7; docs/11-roadmap.md
//! §P2).
//!
//! Two layers of coverage:
//!
//! - **Gateway rejections** (always on): an unsigned intent, an intent whose
//!   `issuer` is not the connection's authenticated id, and an intent arriving
//!   at a gateway with no executor configured are all `Rejected` with their
//!   reason codes — never `Committed`. These run against a live gateway over
//!   loopback iroh.
//! - **FDB durability** (`fdb` feature, live cluster): a replayed intent
//!   returns the recorded outcome (same tick, same minted ids) with the ledger
//!   effect applied exactly once, and a committed intent survives an unclean
//!   cluster drop (the idempotency row and the ledger row are both readable
//!   after reopening).
//! - **The item ownership transfer** (`fdb` feature, live cluster): one trade
//!   moves `ledger/item/{uid}` and both balances and banks a versionstamped
//!   receipt in one transaction; each durable refusal names itself and leaves
//!   every row untouched; a replay moves the item once; and two concurrent
//!   transfers of one item leave exactly one owner. That last one is the
//!   anti-dupe invariant of D11 §7, and the read that produces it is
//!   `trx.get(ledger/item/{uid})` inside the intent transaction.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, MemIntentExecutor,
    Router, RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::{
    CellEpoch, CellId, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
    REASON_BAD_SIGNATURE, REASON_ISSUER_MISMATCH, REASON_NO_EXECUTOR,
};
use tokio::sync::Mutex;

/// A deterministic ed25519 key from a one-byte discriminant.
fn secret(n: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed)
}

/// Build a signed intent from `key` with `id` and one op.
fn signed_intent(id: u128, key: &iroh_base::SecretKey, op: u16, args: &[u8]) -> Intent {
    let mut intent = Intent {
        evidence: None,
        intent_id: id,
        issuer: key.public(),
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op,
            args: Bytes::copy_from_slice(args),
        }],
        attestations: Vec::new(),
        signature: key.sign(b"placeholder"),
    };
    intent.sign(key);
    intent
}

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// A live gateway + client session. Holds the tempdir, client endpoint and
/// router so nothing is dropped mid-test (dropping the client endpoint
/// locally-closes the connection).
struct Session {
    server: GatewayServer,
    conn: lanes::GatewayLanes,
    _client: iroh::Endpoint,
    _dir: tempfile::TempDir,
    _runtime: Arc<Mutex<CellRuntime>>,
}

/// Spawn a gateway (with the given config) and connect a client using `key`'s
/// identity, completing admission.
async fn connect(config: GatewayConfig, key: &iroh_base::SecretKey) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new({
        let store: std::sync::Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            std::sync::Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        CellRuntime::open(&runtime_config(dir.path()), &store)
            .await
            .unwrap()
    }));
    let router: Arc<dyn Router> = runtime.clone();
    let server = GatewayServer::spawn(config, router).await.unwrap();
    let addr = server.addr();

    let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key.clone())
        .bind()
        .await
        .unwrap();
    let conn = client.connect(addr, GATEWAY_ALPN).await.unwrap();
    // Admission: the gateway streams [ACCEPTED] on a uni stream.
    // Read admission before attaching, or the lane reader consumes it.
    let mut admission = conn.accept_uni().await.unwrap();
    let msg = admission.read_to_end(16).await.unwrap();
    assert_eq!(msg, vec![0u8]);
    let conn = lanes::GatewayLanes::attach(conn);
    conn.send_control(&GatewayMsg::VersionedHello {
        token: support::valid_session_token(key.public()),
        node: key.public(),
        version: orrery_protocol::PROTOCOL_VERSION,
    })
    .await;
    assert!(matches!(
        conn.next_reply(Duration::from_secs(5)).await,
        Some(GatewayReply::HelloAck { .. })
    ));
    Session {
        server,
        conn,
        _client: client,
        _dir: dir,
        _runtime: runtime,
    }
}

/// Send `intent` and read back the `IntentAck` outcome.
async fn submit(conn: &lanes::GatewayLanes, intent: Intent) -> IntentOutcome {
    let intent_id = intent.intent_id;
    conn.send_control(&GatewayMsg::SubmitIntent { intent })
        .await;
    for _ in 0..8 {
        let pkt = conn
            .next_payload(Duration::from_secs(5))
            .await
            .expect("timed out waiting for IntentAck");
        if let Some(GatewayReply::IntentAck {
            intent_id: got,
            outcome,
        }) = decode_stream_frame(&pkt)
        {
            assert_eq!(got, intent_id, "ack is for the submitted intent");
            return outcome;
        }
    }
    panic!("no IntentAck after 8 inbound messages");
}

#[tokio::test]
async fn unsigned_intent_is_rejected() {
    let key = secret(1);
    // An executor IS configured — the rejection must come from the signature
    // check, not the missing-executor path.
    let config = GatewayConfig {
        executor: Some(Arc::new(MemIntentExecutor::new())),
        ..support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT])
    };
    let session = connect(config, &key).await;

    // An intent whose signature was made over a different preimage (here: a
    // valid signature of the literal message b"other", not the intent's
    // preimage) fails verification.
    let mut intent = signed_intent(1, &key, 1, b"trade");
    intent.signature = key.sign(b"other");
    let outcome = submit(&session.conn, intent).await;
    assert_eq!(
        outcome,
        IntentOutcome::Rejected {
            reason: REASON_BAD_SIGNATURE
        },
        "a signature that does not cover the intent preimage is rejected"
    );

    // And a correctly-signed intent from the same connection commits — the
    // rejection above was the signature check, not a blanket refusal.
    let good = signed_intent(2, &key, 1, b"trade");
    let outcome = submit(&session.conn, good).await;
    assert!(
        matches!(outcome, IntentOutcome::Committed { .. }),
        "a signed intent commits when an executor is configured"
    );

    session.server.shutdown().await;
}

#[tokio::test]
async fn issuer_mismatch_is_rejected() {
    let key = secret(1);
    let other = secret(2);
    let config = GatewayConfig {
        executor: Some(Arc::new(MemIntentExecutor::new())),
        ..support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT])
    };
    let session = connect(config, &key).await;

    // Signed by `other`, but submitted over `key`'s connection: the gateway
    // binds issuer to the authenticated id and refuses.
    let intent = signed_intent(3, &other, 1, b"trade");
    let outcome = submit(&session.conn, intent).await;
    assert_eq!(
        outcome,
        IntentOutcome::Rejected {
            reason: REASON_ISSUER_MISMATCH
        },
        "an intent in another peer's name is rejected"
    );

    session.server.shutdown().await;
}

#[tokio::test]
async fn intent_without_executor_is_rejected() {
    let key = secret(1);
    // No executor: the honest reply is a rejection, never a fake commit.
    let session = connect(
        support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT]),
        &key,
    )
    .await;

    let intent = signed_intent(4, &key, 1, b"trade");
    let outcome = submit(&session.conn, intent).await;
    assert_eq!(
        outcome,
        IntentOutcome::Rejected {
            reason: REASON_NO_EXECUTOR
        },
        "with no executor the gateway rejects rather than acking a fake commit"
    );

    session.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// FDB-backed durability (live cluster; self-skips when unconfigured)
// ---------------------------------------------------------------------------

/// The cluster file for the FDB-gated tests, or `None` if not configured.
///
/// Honors `ORRERY_FDB_CLUSTER_FILE`; otherwise walks up from the crate dir to
/// find the workspace-root `.fdb-dev/fdb.cluster` (tests run with CWD = the
/// crate dir, not the workspace root).
#[cfg(feature = "fdb")]
fn fdb_cluster_file() -> Option<String> {
    if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
        return Some(path);
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".fdb-dev/fdb.cluster");
        if candidate.exists() {
            return Some(candidate.display().to_string());
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The op-0 args encoding the FDB executor's harness credit op uses:
/// `account u64 LE ‖ asset u64 LE ‖ delta i64 LE` (24 bytes).
#[cfg(feature = "fdb")]
fn credit_args(account: u64, asset: u64, delta: i64) -> Vec<u8> {
    let mut v = Vec::with_capacity(24);
    v.extend_from_slice(&account.to_le_bytes());
    v.extend_from_slice(&asset.to_le_bytes());
    v.extend_from_slice(&delta.to_le_bytes());
    v
}

/// Read the little-endian i64 balance at `ledger/bal/{account}/{asset}`.
#[cfg(feature = "fdb")]
async fn read_balance(db: &foundationdb::Database, account: u64, asset: u64) -> Option<i64> {
    let key = orrery_persistd::keyspace::ledger_bal_key(
        orrery_protocol::AccountId::new(account),
        orrery_protocol::AssetId::new(asset),
    );
    let value: Option<foundationdb::future::FdbSlice> = db
        .run(|trx, _| async move { Ok(trx.get(&key, false).await?) })
        .await
        .unwrap();
    value.map(|v| {
        let mut buf = [0u8; 8];
        let n = v.len().min(8);
        buf[..n].copy_from_slice(&v[..n]);
        i64::from_le_bytes(buf)
    })
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_replayed_intent_returns_recorded_outcome() {
    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // This test's own grid (brief: 9301..9315), so its ledger/pid rows never
    // touch another test's namespace on the shared dev cluster.
    let grid = GridId::new(9301);
    let exec = FdbIntentExecutor::connect(&cluster, grid).unwrap();

    let key = secret(11);
    let account = 500_001u64;
    let asset = 1u64;
    let intent = signed_intent(0x9301_0001, &key, 0, &credit_args(account, asset, 500));

    // First submission commits.
    let first = exec.execute(&intent).await.unwrap();
    let IntentOutcome::Committed {
        tick: t1,
        minted: m1,
    } = &first
    else {
        panic!("expected Committed, got {first:?}");
    };
    assert_eq!(m1.len(), 1, "one op mints one PersistId");

    // The ledger effect applied once.
    let bal = read_balance(exec.database(), account, asset).await;
    assert_eq!(bal, Some(500), "the credit applied exactly once");

    // A replay with the same intent_id returns the FIRST outcome unchanged.
    let second = exec.execute(&intent).await.unwrap();
    assert_eq!(
        second, first,
        "replay returns the recorded outcome (same tick, same minted ids)"
    );
    let IntentOutcome::Committed {
        tick: t2,
        minted: m2,
    } = &second
    else {
        unreachable!();
    };
    assert_eq!(t2, t1);
    assert_eq!(m2, m1);

    // And the ledger effect did NOT apply a second time.
    let bal = read_balance(exec.database(), account, asset).await;
    assert_eq!(
        bal,
        Some(500),
        "the replayed intent did not double-apply the credit"
    );
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_concurrent_independent_intents_use_unique_block_granted_ids() {
    use std::collections::BTreeSet;

    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // One shared executor is the P2 gateway shape: many concurrently arriving
    // independent intents must not all read-conflict on pid/next. This grid is
    // deliberately isolated from the durability tests above and from seeded
    // content in the shared development cluster.
    let exec = Arc::new(FdbIntentExecutor::connect(&cluster, GridId::new(9310)).unwrap());
    let key = secret(13);
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..32u64 {
        let exec = Arc::clone(&exec);
        let intent = signed_intent(
            0x9310_0000 + u128::from(n),
            &key,
            0,
            &credit_args(510_000 + n, 1, 1),
        );
        tasks.spawn(async move { exec.execute(&intent).await });
    }

    let mut minted = BTreeSet::new();
    while let Some(result) = tasks.join_next().await {
        let outcome = result
            .expect("intent task did not panic")
            .expect("independent intents must not exhaust conflict retries on pid/next");
        let IntentOutcome::Committed { minted: ids, .. } = outcome else {
            panic!("concurrent signed intent was not committed");
        };
        assert_eq!(ids.len(), 1);
        assert!(minted.insert(ids[0]), "PersistId must not be reused");
    }
    assert_eq!(minted.len(), 32, "every committed intent received one id");
}

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_committed_intent_survives_restart() {
    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let grid = GridId::new(9302);
    let key = secret(12);
    let account = 500_002u64;
    let asset = 2u64;
    let intent = signed_intent(0x9302_0001, &key, 0, &credit_args(account, asset, 750));

    // Commit against one executor, then drop it WITHOUT a clean shutdown —
    // the FDB client has no graceful-close requirement; durability is the
    // cluster's, so a kill -9 of the gateway loses nothing committed.
    let committed = {
        let exec = FdbIntentExecutor::connect(&cluster, grid).unwrap();
        let outcome = exec.execute(&intent).await.unwrap();
        assert!(matches!(outcome, IntentOutcome::Committed { .. }));
        outcome
        // `exec` dropped here, no shutdown — the kill -9 analogue.
    };

    // Reopen with a fresh executor (a new Database handle on the same
    // cluster) and read both the effect and the idempotency row back.
    let exec = FdbIntentExecutor::connect(&cluster, grid).unwrap();

    // The ledger effect survived.
    let bal = read_balance(exec.database(), account, asset).await;
    assert_eq!(
        bal,
        Some(750),
        "the committed credit survived the unclean drop"
    );

    // The idempotency row survived: a replay after "restart" returns the
    // recorded outcome, not a fresh commit.
    let replayed = exec.execute(&intent).await.unwrap();
    assert_eq!(
        replayed, committed,
        "the idempotency row survived and returns the recorded outcome"
    );

    // And it still did not double-apply.
    let bal = read_balance(exec.database(), account, asset).await;
    assert_eq!(bal, Some(750), "no double-apply across the restart");
}

/// The ledger fence, driven the way persistd drives it: ownership comes from a
/// real `activate_shards` (so the epoch is cluster-minted and ≥ 1, never a
/// hand-written 0), and the client keeps sending the default `cell_epoch` it
/// has always sent — the two are different namespaces now and the fence does
/// not read the intent at all.
///
/// Two shards, because `IntentFence` verifies the whole activated set: an
/// `IntentOp` names no cell, so there is nothing to select a single shard by.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_fenced_intent_refuses_a_promoted_owner() {
    use orrery_persistd::fence::{ActivationOutcome, ShardActivation};
    use orrery_persistd::{
        FdbIntentExecutor, FenceOutcome, FenceRow, FenceStatus, FenceStore, IntentExecutor,
        IntentFence,
    };

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let grid = GridId::new(9_303);
    let children = CellId::ROOT.children();
    let shards = vec![children[0], children[1]];
    let fences = orrery_persistd::fence::FdbFenceStore::connect(&cluster).unwrap();
    for &shard in &shards {
        fences.retire(grid, shard).await.unwrap();
    }

    let db = FdbIntentExecutor::connect(&cluster, grid)
        .unwrap()
        .database()
        .clone();
    // Idempotency rows outlive the test by an hour (the GC deadline), and a
    // replay is answered before the fence is ever consulted. Clearing this
    // test's three rows is what keeps a repeat run honest rather than
    // vacuously green.
    for intent_id in [0x9303_0001u128, 0x9303_0002, 0x9303_0003] {
        let key = orrery_persistd::keyspace::intent_key(intent_id);
        db.run(move |trx, _| async move {
            trx.clear(&key);
            Ok(())
        })
        .await
        .unwrap();
    }

    // Ownership as persistd acquires it at startup: one atomic activation over
    // the whole shard set, minting the epoch itself.
    let requests: Vec<ShardActivation> = shards
        .iter()
        .map(|&shard| ShardActivation {
            shard,
            expected: None,
        })
        .collect();
    let ActivationOutcome::Activated { rows } =
        fences.activate_shards(grid, 73, &requests).await.unwrap()
    else {
        panic!("bootstrap activation must succeed");
    };
    let epoch = rows[0].1.epoch;
    assert_eq!(
        epoch,
        Epoch::new(1),
        "a cluster-minted ownership epoch starts at 1, which no client ever sends"
    );

    let exec = FdbIntentExecutor::fenced_from_database(
        db.clone(),
        grid,
        IntentFence {
            shards: shards.clone(),
            owner: 73,
            epoch,
        },
    );
    // `signed_intent` ships `CellEpoch::new(0)`, exactly as every production
    // issuer does. Under the old conflated type this was rejected outright.
    let intent = signed_intent(0x9303_0001, &secret(33), 0, &credit_args(500_003, 3, 1));
    let committed = exec.execute(&intent).await.unwrap();
    assert!(matches!(committed, IntentOutcome::Committed { .. }));
    // Ledger rows are grid-independent, so compare against what this run
    // observed rather than an absolute figure a repeat run would move.
    let credited = read_balance(exec.database(), 500_003, 3).await;
    let uncredited = read_balance(exec.database(), 500_004, 3).await;

    // Promotion advances the fence on one shard only. The old executor can no
    // longer commit a distinct intent even though it still has a live database
    // handle and still owns the other shard.
    let previous = FenceRow {
        owner: 73,
        epoch,
        status: FenceStatus::Active,
    };
    let promoted = FenceRow {
        owner: 74,
        epoch: Epoch::new(epoch.0 + 1),
        status: FenceStatus::Active,
    };
    assert_eq!(
        fences
            .fence(grid, shards[1], Some(&previous), &promoted)
            .await
            .unwrap(),
        FenceOutcome::Fenced
    );
    let stale = signed_intent(0x9303_0002, &secret(34), 0, &credit_args(500_004, 3, 1));
    assert!(
        exec.execute(&stale).await.is_err(),
        "a superseded executor may not mint a new ledger effect"
    );
    assert_eq!(
        read_balance(exec.database(), 500_004, 3).await,
        uncredited,
        "the refused intent left no effect behind"
    );

    // Replay is deliberately not fenced: the idempotency row is read before
    // the fence, so a superseded executor answering a retransmit returns the
    // outcome it already committed rather than a spurious refusal. It is a
    // durable fact, not a new effect.
    assert_eq!(
        exec.execute(&intent).await.unwrap(),
        committed,
        "a replayed intent returns its recorded outcome even after fencing"
    );
    assert_eq!(
        read_balance(exec.database(), 500_003, 3).await,
        credited,
        "the replay did not re-apply the credit"
    );

    // Control: the refusal is the fence's doing and nothing else's. An
    // equivalent intent, on the same database and past the same promotion,
    // commits through an executor that carries no fence — exactly the state
    // the reference binary shipped in before it was wired to
    // `fenced_from_context`. It needs its own id: `stale` must stay
    // uncommitted, or a repeat run would meet its idempotency row and be
    // answered before ever reaching the fence.
    let unfenced = FdbIntentExecutor::from_database(exec.database().clone(), grid);
    let control = signed_intent(0x9303_0003, &secret(35), 0, &credit_args(500_005, 3, 1));
    assert!(matches!(
        unfenced.execute(&control).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));

    for &shard in &shards {
        fences.retire(grid, shard).await.unwrap();
    }
}

/// A 128-shard ownership fence is verified inside **every** intent
/// transaction, on the critical path of a commit D16 budgets at p99 < 10 ms.
///
/// This measures that cost at the deployment size the P2 criterion describes
/// (docs/11-roadmap.md §P2). `#[ignore]` because it needs a cluster it can
/// hammer and because a wall-clock figure is evidence for a human, not a gate
/// on a shared box. Run with:
///
/// ```text
/// ORRERY_FDB_CLUSTER_FILE=... cargo test -p orrery_persistd --features fdb \
///   --test intent_commit -- --ignored --nocapture fdb_fenced_intent_latency
/// ```
#[cfg(feature = "fdb")]
#[tokio::test]
#[ignore = "measurement against a live cluster, not a gate"]
async fn fdb_fenced_intent_latency_at_128_shards() {
    use orrery_persistd::fence::{ActivationOutcome, ShardActivation};
    use orrery_persistd::{FdbIntentExecutor, FenceStore, IntentExecutor, IntentFence};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let grid = GridId::new(9_401);
    // 128 disjoint level-18 cells, the shard set the kill-9 gate derives from
    // the demo scenario, in the sorted order activation requires.
    let mut shards: Vec<CellId> = (0..128)
        .map(|i| {
            let x = i % 8;
            let y = (i / 8) % 4;
            let z = i / 32;
            CellId::from_coords(glam::IVec3::new(x, y, z), orrery_protocol::SHARD_LEVEL)
                .expect("in range")
        })
        .collect();
    shards.sort_unstable_by_key(|shard| shard.to_bits());
    let fences = orrery_persistd::fence::FdbFenceStore::connect(&cluster).unwrap();
    for &shard in &shards {
        fences.retire(grid, shard).await.unwrap();
    }
    let requests: Vec<ShardActivation> = shards
        .iter()
        .map(|&shard| ShardActivation {
            shard,
            expected: None,
        })
        .collect();
    let ActivationOutcome::Activated { rows } =
        fences.activate_shards(grid, 91, &requests).await.unwrap()
    else {
        panic!("bootstrap activation must succeed");
    };
    let epoch = rows[0].1.epoch;

    let exec = FdbIntentExecutor::fenced_from_context(
        &orrery_persistd::FdbContext::connect(&cluster).unwrap(),
        grid,
        IntentFence {
            shards: shards.clone(),
            owner: 91,
            epoch,
        },
    );

    let mut samples = Vec::new();
    for i in 0..200u128 {
        // A fresh id per iteration: a replay is answered from the idempotency
        // row before the fence is read at all, which would measure nothing.
        let id = 0x9401_0000_0000u128 + (std::process::id() as u128) * 1_000_000 + i;
        let intent = signed_intent(id, &secret(41), 0, &credit_args(590_001, 4, 1));
        let started = std::time::Instant::now();
        let outcome = exec.execute(&intent).await.expect("commit");
        samples.push(started.elapsed());
        assert!(matches!(outcome, IntentOutcome::Committed { .. }));
    }
    samples.sort_unstable();
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "fenced intent commit, {} shards, n={}: p50 {:?} p99 {:?} max {:?}",
        shards.len(),
        samples.len(),
        p(0.50),
        p(0.99),
        samples[samples.len() - 1]
    );

    for &shard in &shards {
        fences.retire(grid, shard).await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// The item ownership transfer (`fdb` feature, live cluster)
// ---------------------------------------------------------------------------
//
// These write to fixed keys, so every test resets the rows it owns before it
// runs. Reusing a shared development cluster otherwise makes the *second* run
// of a suite assert something different from the first: an item already
// transferred, a balance already spent, an `intent/` row already recorded.
// The account, item and intent-id spaces below are disjoint per test for the
// same reason two tests must not share a grid.

/// Build the 40-byte `LEDGER_ITEM_TRANSFER_OP` args through the same
/// definition the executor decodes them with.
#[cfg(feature = "fdb")]
fn transfer_args(item: u64, seller: u64, buyer: u64, asset: u64, price: i64) -> Vec<u8> {
    orrery_persistd::ItemTransferArgs {
        item: orrery_protocol::ItemUid::new(item),
        seller: orrery_protocol::AccountId::new(seller),
        buyer: orrery_protocol::AccountId::new(buyer),
        asset: orrery_protocol::AssetId::new(asset),
        price,
    }
    .encode()
    .to_vec()
}

/// A signed transfer intent.
#[cfg(feature = "fdb")]
fn transfer_intent(
    id: u128,
    key: &iroh_base::SecretKey,
    item: u64,
    seller: u64,
    buyer: u64,
    asset: u64,
    price: i64,
) -> Intent {
    signed_intent(
        id,
        key,
        orrery_persistd::LEDGER_ITEM_TRANSFER_OP,
        &transfer_args(item, seller, buyer, asset, price),
    )
}

/// Put the durable rows a trade test needs into a known state: item owners,
/// exact balances, and no `intent/` row for the ids the test will submit.
///
/// Balances are `set` rather than `Add`ed, so a re-run starts from the same
/// number instead of accumulating. The `intent/` deletes are what let a test
/// reuse a fixed `intent_id` — without them the second run replays the first
/// run's recorded outcome and asserts nothing.
#[cfg(feature = "fdb")]
async fn reset_ledger(
    db: &foundationdb::Database,
    items: &[(u64, u64)],
    balances: &[(u64, u64, i64)],
    intents: &[u128],
) {
    let items = items.to_vec();
    let balances = balances.to_vec();
    let intents = intents.to_vec();
    db.run(|trx, _| {
        let items = items.clone();
        let balances = balances.clone();
        let intents = intents.clone();
        async move {
            for (item, owner) in items {
                let key =
                    orrery_persistd::keyspace::ledger_item_key(orrery_protocol::ItemUid::new(item));
                let row = orrery_persistd::keyspace::ItemRow {
                    owner: orrery_protocol::AccountId::new(owner),
                    state: b"fixture".to_vec(),
                };
                trx.set(&key, &postcard::to_stdvec(&row).unwrap());
            }
            for (account, asset, value) in balances {
                let key = orrery_persistd::keyspace::ledger_bal_key(
                    orrery_protocol::AccountId::new(account),
                    orrery_protocol::AssetId::new(asset),
                );
                trx.set(&key, &i128::from(value).to_le_bytes());
            }
            for id in &intents {
                trx.clear(&orrery_persistd::keyspace::intent_key(*id));
            }
            // Receipts are keyed by commit versionstamp, so a re-run cannot
            // overwrite the previous run's row the way every other key here
            // does — it appends a second one. Clear the ones these intent ids
            // banked, and only those: the `lr` span is global (no grid in the
            // key) and other tests' rows live in it.
            {
                use futures::TryStreamExt as _;
                let mut stale = Vec::new();
                let mut stream = trx.get_ranges_keyvalues(
                    foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(b"lr".as_slice()),
                        end: foundationdb::KeySelector::first_greater_or_equal(b"ls".as_slice()),
                        ..foundationdb::RangeOption::default()
                    },
                    false,
                );
                while let Some(kv) = stream.try_next().await? {
                    if let Ok(row) =
                        postcard::from_bytes::<orrery_persistd::keyspace::ReceiptRow>(kv.value())
                    {
                        if intents.contains(&row.intent_id) {
                            stale.push(kv.key().to_vec());
                        }
                    }
                }
                for key in stale {
                    trx.clear(&key);
                }
            }
            Ok(())
        }
    })
    .await
    .unwrap();
}

/// The account named by `ledger/item/{item}`, or `None` if the row is absent.
#[cfg(feature = "fdb")]
async fn read_item_owner(db: &foundationdb::Database, item: u64) -> Option<u64> {
    let key = orrery_persistd::keyspace::ledger_item_key(orrery_protocol::ItemUid::new(item));
    let value: Option<foundationdb::future::FdbSlice> = db
        .run(|trx, _| async move { Ok(trx.get(&key, false).await?) })
        .await
        .unwrap();
    value.map(|v| {
        let row: orrery_persistd::keyspace::ItemRow = postcard::from_bytes(&v).unwrap();
        row.owner.0
    })
}

/// Every `ledger/receipt/` row this intent banked.
///
/// The receipt family is not grid-scoped — its key is nothing but the commit
/// versionstamp — so the whole span is scanned and filtered by `intent_id`
/// rather than counted. On a shared cluster the span holds other tests' rows.
#[cfg(feature = "fdb")]
async fn receipts_for(
    db: &foundationdb::Database,
    intent_id: u128,
) -> Vec<orrery_persistd::keyspace::ReceiptRow> {
    use futures::TryStreamExt as _;

    db.run(|trx, _| async move {
        let mut found = Vec::new();
        let mut stream = trx.get_ranges_keyvalues(
            foundationdb::RangeOption {
                begin: foundationdb::KeySelector::first_greater_or_equal(b"lr".as_slice()),
                end: foundationdb::KeySelector::first_greater_or_equal(b"ls".as_slice()),
                ..foundationdb::RangeOption::default()
            },
            false,
        );
        while let Some(kv) = stream.try_next().await? {
            if let Ok(row) =
                postcard::from_bytes::<orrery_persistd::keyspace::ReceiptRow>(kv.value())
            {
                if row.intent_id == intent_id {
                    // The versionstamp is 10 bytes at offset 2, so a receipt
                    // key is 12 bytes and the placeholder is gone.
                    assert_eq!(kv.key().len(), 12, "receipt key carries a versionstamp");
                    assert_ne!(
                        &kv.key()[2..12],
                        &[0u8; 10],
                        "FDB substituted the commit versionstamp for the placeholder"
                    );
                    found.push(row);
                }
            }
        }
        Ok(found)
    })
    .await
    .unwrap()
}

/// The happy path of docs/08-persistence.md §7: one transaction moves the
/// ownership row, debits the buyer, credits the seller, and banks a receipt.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_item_transfer_moves_the_row_and_both_balances() {
    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(9320)).unwrap();
    let (item, seller, buyer, asset, price) = (0x9320_0001u64, 520_001u64, 520_002u64, 9u64, 500);
    let id = 0x9320_0001u128;
    reset_ledger(
        exec.database(),
        &[(item, seller)],
        &[(buyer, asset, 500), (seller, asset, 0)],
        &[id],
    )
    .await;

    let outcome = exec
        .execute(&transfer_intent(
            id,
            &secret(20),
            item,
            seller,
            buyer,
            asset,
            price,
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, IntentOutcome::Committed { .. }),
        "the trade must commit, got {outcome:?}"
    );

    assert_eq!(
        read_item_owner(exec.database(), item).await,
        Some(buyer),
        "the single ownership row now names the buyer"
    );
    assert_eq!(read_balance(exec.database(), buyer, asset).await, Some(0));
    assert_eq!(
        read_balance(exec.database(), seller, asset).await,
        Some(500)
    );

    let receipts = receipts_for(exec.database(), id).await;
    assert_eq!(receipts.len(), 1, "one trade banks one receipt");
    assert_eq!(
        receipts[0].parties,
        vec![
            orrery_protocol::AccountId::new(seller),
            orrery_protocol::AccountId::new(buyer)
        ]
    );
    assert_eq!(
        receipts[0].ops,
        vec![orrery_persistd::LEDGER_ITEM_TRANSFER_OP]
    );
}

/// Each durable refusal names itself, and none of them writes anything.
///
/// The distinctness is the assertion. An opaque `REASON_EXECUTOR_ERROR` for
/// all four would leave "the anti-dupe invariant held" and "the cluster fell
/// over" reading identically — which is exactly what the dupe gauntlet has to
/// tell apart.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_each_transfer_refusal_names_itself_and_writes_nothing() {
    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(9321)).unwrap();
    let (item, absent, seller, buyer, asset) =
        (0x9321_0001u64, 0x9321_00FFu64, 521_001u64, 521_002u64, 9u64);

    // (intent_id, seller, buyer, item, price, expected reason, why)
    let cases: [(u128, u64, u64, u64, i64, u16, &str); 4] = [
        (
            0x9321_0001,
            buyer,
            seller,
            item,
            0,
            orrery_protocol::REASON_NOT_ITEM_OWNER,
            "the named seller does not own the row",
        ),
        (
            0x9321_0002,
            seller,
            buyer,
            absent,
            0,
            orrery_protocol::REASON_NO_SUCH_ITEM,
            "the item has no ownership row",
        ),
        (
            0x9321_0003,
            seller,
            seller,
            item,
            0,
            orrery_protocol::REASON_ITEM_TRANSFER_TO_SELF,
            "one account cannot be both parties",
        ),
        (
            0x9321_0004,
            seller,
            buyer,
            item,
            501,
            orrery_protocol::REASON_INSUFFICIENT_BALANCE,
            "the buyer holds 500 and the price is 501",
        ),
    ];

    for (id, from, to, uid, price, reason, why) in cases {
        reset_ledger(
            exec.database(),
            &[(item, seller)],
            &[(buyer, asset, 500), (seller, asset, 0)],
            &[id],
        )
        .await;
        // The absent item must actually be absent, whatever a previous run
        // left behind.
        exec.database()
            .run(|trx, _| async move {
                trx.clear(&orrery_persistd::keyspace::ledger_item_key(
                    orrery_protocol::ItemUid::new(absent),
                ));
                Ok(())
            })
            .await
            .unwrap();

        let outcome = exec
            .execute(&transfer_intent(
                id,
                &secret(21),
                uid,
                from,
                to,
                asset,
                price,
            ))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            IntentOutcome::Rejected { reason },
            "{why}: expected its own reason code"
        );
        assert_eq!(
            read_item_owner(exec.database(), item).await,
            Some(seller),
            "{why}: the ownership row is untouched"
        );
        assert_eq!(
            read_balance(exec.database(), buyer, asset).await,
            Some(500),
            "{why}: the debit side is untouched"
        );
        assert_eq!(
            read_balance(exec.database(), seller, asset).await,
            Some(0),
            "{why}: the credit side is untouched"
        );
        assert!(
            receipts_for(exec.database(), id).await.is_empty(),
            "{why}: no receipt is banked"
        );
    }
}

/// Replay: the same `intent_id` submitted twice transfers the item once.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_replayed_transfer_moves_the_item_once() {
    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(9322)).unwrap();
    let (item, seller, buyer, asset) = (0x9322_0001u64, 522_001u64, 522_002u64, 9u64);
    let id = 0x9322_0001u128;
    reset_ledger(
        exec.database(),
        &[(item, seller)],
        &[(buyer, asset, 500), (seller, asset, 0)],
        &[id],
    )
    .await;

    let intent = transfer_intent(id, &secret(22), item, seller, buyer, asset, 500);
    let first = exec.execute(&intent).await.unwrap();
    assert!(matches!(first, IntentOutcome::Committed { .. }));
    let second = exec.execute(&intent).await.unwrap();
    assert_eq!(second, first, "the replay returns the recorded outcome");

    assert_eq!(read_item_owner(exec.database(), item).await, Some(buyer));
    assert_eq!(
        read_balance(exec.database(), seller, asset).await,
        Some(500),
        "the seller was paid once, not twice"
    );
    assert_eq!(read_balance(exec.database(), buyer, asset).await, Some(0));
    assert_eq!(
        receipts_for(exec.database(), id).await.len(),
        1,
        "one receipt: the replay committed no second trade"
    );
}

/// **The anti-dupe invariant, in one test.** Two transfers of the same item
/// run concurrently against a live cluster; exactly one commits, the other is
/// refused with `REASON_NOT_ITEM_OWNER`, and the item ends with exactly one
/// owner who is not the seller.
///
/// The mechanism is the `trx.get` on `ledger/item/{uid}` that both
/// transactions perform before writing. That read registers the row's
/// serializable read conflict range, so the resolver aborts whichever
/// transaction tries to commit second with `not_committed`; `db.run` re-runs
/// its closure, the item row is read again, and the owner check now fails
/// against the winner. Double-spend would require two commits over that one
/// read — which FDB does not allow (docs/08-persistence.md §7).
///
/// In-process, deliberately: the two-*process* form is the P5 gauntlet's arm
/// (b) and belongs to its own harness. What is proved here is that the
/// conflict range exists and that losing it produces an honest refusal rather
/// than a second transfer.
#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fdb_two_transfers_of_one_item_leave_one_owner() {
    use orrery_persistd::{FdbIntentExecutor, IntentExecutor};

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let exec = Arc::new(FdbIntentExecutor::connect(&cluster, GridId::new(9323)).unwrap());
    let (item, seller, asset, price) = (0x9323_0001u64, 523_001u64, 9u64, 500i64);
    let buyers = [523_002u64, 523_003u64];
    let ids = [0x9323_0001u128, 0x9323_0002u128];
    reset_ledger(
        exec.database(),
        &[(item, seller)],
        &[
            (buyers[0], asset, 500),
            (buyers[1], asset, 500),
            (seller, asset, 0),
        ],
        &ids,
    )
    .await;

    // A barrier, so the two transactions genuinely overlap. Without it the
    // first can finish before the second takes its read version, and the test
    // would assert the same outcome without ever exercising the conflict.
    let gate = Arc::new(tokio::sync::Barrier::new(ids.len()));
    let mut tasks = tokio::task::JoinSet::new();
    for (id, buyer) in ids.into_iter().zip(buyers) {
        let exec = Arc::clone(&exec);
        let gate = Arc::clone(&gate);
        tasks.spawn(async move {
            gate.wait().await;
            exec.execute(&transfer_intent(
                id,
                &secret(23),
                item,
                seller,
                buyer,
                asset,
                price,
            ))
            .await
        });
    }

    let mut committed = 0;
    let mut refused = 0;
    while let Some(result) = tasks.join_next().await {
        match result
            .expect("transfer task did not panic")
            .expect("a losing transfer is a Rejected outcome, never an executor error")
        {
            IntentOutcome::Committed { .. } => committed += 1,
            // `execute` is the attested path and never produces this arm;
            // named rather than wildcarded so a change that let it do so
            // fails here instead of going uncounted.
            IntentOutcome::Provisional { .. } => {
                panic!("execute never commits provisionally")
            }
            IntentOutcome::Rejected { reason } => {
                assert_eq!(
                    reason,
                    orrery_protocol::REASON_NOT_ITEM_OWNER,
                    "the loser re-reads the winner's owner and fails its check honestly"
                );
                refused += 1;
            }
        }
    }
    assert_eq!(
        (committed, refused),
        (1, 1),
        "exactly one of two transfers of one item commits"
    );

    let owner = read_item_owner(exec.database(), item)
        .await
        .expect("the item still has exactly one ownership row");
    assert!(
        buyers.contains(&owner),
        "the item ended with one of the two buyers, got {owner}"
    );
    assert_ne!(owner, seller, "the seller divested exactly once");
    assert_eq!(
        read_balance(exec.database(), seller, asset).await,
        Some(price),
        "the seller was paid for one sale, not two"
    );
    let winner = usize::from(owner == buyers[1]);
    assert_eq!(
        read_balance(exec.database(), buyers[winner], asset).await,
        Some(0),
        "the winning buyer paid"
    );
    assert_eq!(
        read_balance(exec.database(), buyers[1 - winner], asset).await,
        Some(500),
        "the refused buyer's balance is untouched"
    );
}

/// The conflict, named: FDB reports `ledger/item/{uid}` as the range that
/// caused the second commit to fail.
///
/// `fdb_two_transfers_of_one_item_leave_one_owner` asserts the *outcome* — one
/// owner, one honest refusal — and that outcome is also what a purely
/// serialized pair of transfers would produce, so on its own it does not
/// distinguish "the resolver aborted the loser" from "the loser simply ran
/// second". This one removes the ambiguity by asking FDB directly. Two
/// transactions read the exact key [`plan_ops`] reads
/// (`keyspace::ledger_item_key`), both write it, the first commits, and the
/// second is refused with `not_committed` (1020) — with
/// `ReportConflictingKeys` set, so the reported range can be checked to be the
/// item row and not something incidental.
///
/// [`plan_ops`]: orrery_persistd
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_the_item_row_read_is_what_registers_the_conflict() {
    use foundationdb::options::TransactionOption;
    use orrery_persistd::FdbIntentExecutor;

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(9324)).unwrap();
    let item = 0x9324_0001u64;
    reset_ledger(exec.database(), &[(item, 524_001)], &[], &[]).await;

    let key = orrery_persistd::keyspace::ledger_item_key(orrery_protocol::ItemUid::new(item));
    let row = |owner: u64| {
        postcard::to_stdvec(&orrery_persistd::keyspace::ItemRow {
            owner: orrery_protocol::AccountId::new(owner),
            state: Vec::new(),
        })
        .unwrap()
    };

    let first = exec.database().create_trx().unwrap();
    let second = exec.database().create_trx().unwrap();
    second
        .set_option(TransactionOption::ReportConflictingKeys)
        .unwrap();

    // Both take a read version and register the row's read conflict range,
    // exactly as the executor's transfer does before it writes.
    first.get(&key, false).await.unwrap();
    second.get(&key, false).await.unwrap();

    first.set(&key, &row(524_002));
    second.set(&key, &row(524_003));

    first.commit().await.expect("the first transfer commits");
    let Err(refused) = second.commit().await else {
        panic!("the second commit must be refused: both read the same item row");
    };
    assert_eq!(
        refused.code(),
        1020,
        "not_committed is what makes double-spend impossible; got {refused}"
    );

    let ranges = refused.conflicting_keys().await.unwrap();
    assert!(
        ranges
            .iter()
            .any(|r| r.begin() <= key.as_slice() && key.as_slice() < r.end()),
        "the conflicting range must cover ledger/item/{item:#x}, got {:?}",
        ranges.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}
