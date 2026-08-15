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
use orrery_protocol::channels::{decode_stream_frame, encode_stream_frame};
use orrery_protocol::{
    CellId, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
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
        intent_id: id,
        issuer: key.public(),
        cell_epoch: Epoch::new(0),
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
    conn: iroh::endpoint::Connection,
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
    let mut admission = conn.accept_uni().await.unwrap();
    let msg = admission.read_to_end(16).await.unwrap();
    assert_eq!(msg, vec![0u8]);
    conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
        token: support::valid_session_token(key.public()),
        node: key.public(),
    })))
    .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
        .await
        .expect("hello reply")
        .expect("hello datagram");
    assert!(matches!(
        decode_stream_frame(&reply),
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
async fn submit(conn: &iroh::endpoint::Connection, intent: Intent) -> IntentOutcome {
    let intent_id = intent.intent_id;
    conn.send_datagram(Bytes::from(encode_stream_frame(
        &GatewayMsg::SubmitIntent { intent },
    )))
    .unwrap();
    for _ in 0..8 {
        let pkt = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
            .await
            .expect("timed out waiting for IntentAck")
            .expect("datagram");
        if let Some(GatewayReply::IntentAck {
            intent_id: got,
            outcome,
        }) = decode_stream_frame(&pkt)
        {
            assert_eq!(got, intent_id, "ack is for the submitted intent");
            return outcome;
        }
    }
    panic!("no IntentAck after 8 datagrams");
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

#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_fenced_intent_refuses_a_promoted_owner() {
    use orrery_persistd::{
        FdbIntentExecutor, FenceOutcome, FenceRow, FenceStatus, FenceStore, IntentExecutor,
        IntentFence,
    };

    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let grid = GridId::new(9_303);
    let shard = CellId::ROOT;
    let fences = orrery_persistd::fence::FdbFenceStore::connect(&cluster).unwrap();
    fences.retire(grid, shard).await.unwrap();
    let owner = FenceRow {
        owner: 73,
        epoch: Epoch::new(0),
        status: FenceStatus::Active,
    };
    assert_eq!(
        fences.fence(grid, shard, None, &owner).await.unwrap(),
        FenceOutcome::Fenced
    );
    let exec = FdbIntentExecutor::fenced_from_database(
        FdbIntentExecutor::connect(&cluster, grid)
            .unwrap()
            .database()
            .clone(),
        grid,
        IntentFence {
            shard,
            owner: 73,
            epoch: Epoch::new(0),
        },
    );
    let intent = signed_intent(0x9303_0001, &secret(33), 0, &credit_args(500_003, 3, 1));
    assert!(matches!(
        exec.execute(&intent).await,
        Ok(IntentOutcome::Committed { .. })
    ));

    // Promotion advances the fence. The old executor can no longer commit a
    // distinct intent even though it still has a live database handle.
    let promoted = FenceRow {
        owner: 74,
        epoch: Epoch::new(1),
        status: FenceStatus::Active,
    };
    assert_eq!(
        fences
            .fence(grid, shard, Some(&owner), &promoted)
            .await
            .unwrap(),
        FenceOutcome::Fenced
    );
    let stale = signed_intent(0x9303_0002, &secret(34), 0, &credit_args(500_004, 3, 1));
    assert!(exec.execute(&stale).await.is_err());
    fences.retire(grid, shard).await.unwrap();
}
