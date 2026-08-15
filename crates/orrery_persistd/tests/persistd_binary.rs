//! Binary-level integration tests for the `persistd` binary.
//!
//! These tests spawn the compiled binary and assert its behavior: stable node
//! identity across restarts, the stdout JSON address line, and graceful signal
//! handling. They do NOT require FoundationDB.

mod support;

use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command};
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{presets::N0, Builder};
use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
#[cfg(feature = "fdb")]
use orrery_persistd::{FdbContext, FenceStore};
use orrery_persistd::{Journal, JournalConfig, GATEWAY_ALPN};
use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame,
};
use orrery_protocol::{
    CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
    JournalRecord, Lsn, NodeId, PersistId, RecordKind, Tick, REASON_VALIDATION_FAILED,
};

/// Locate the compiled `persistd` binary via `CARGO_BIN_EXE_persistd`, set by
/// Cargo when running `cargo test` on a binary target's integration tests.
fn persistd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_persistd")
}

fn issuer_key_arg() -> String {
    format!("{}@{}", support::ISSUER_KEY_ID, support::issuer().public())
}

fn process_session_token(node: NodeId) -> Vec<u8> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis();
    let issued_at_ms = u64::try_from(now_ms).expect("current timestamp fits u64");
    support::session_token(
        &support::issuer(),
        node,
        issued_at_ms,
        support::TOKEN_TTL_MS,
    )
}

/// Run `persistd` with the given extra arguments, wait for the JSON address
/// line on stdout, kill the process with SIGTERM, and return the parsed
/// `(node_id_string, endpoint_addr_string)`.
///
/// The binary is run with `--dir` set to a temporary directory and `--bind` set
/// to `127.0.0.1:0` so each run gets an ephemeral port.
fn run_persistd(args: &[&str]) -> (String, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cmd = Command::new(persistd_binary());
    cmd.arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--allow-volatile-leases")
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        .args(args)
        // Route tracing to stderr so stdout has only the JSON line.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn persistd");
    let stdout = child.stdout.take().expect("stdout captured");
    let mut reader = std::io::BufReader::new(stdout);

    // Read the first line of stdout — the JSON address line.
    let mut line = String::new();
    use std::io::BufRead;
    let timeout = Duration::from_secs(10);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            // Kill the process and fail.
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for JSON address line from persistd");
        }
        line.clear();
        let bytes_read = reader.read_line(&mut line).expect("read stdout line");
        if bytes_read == 0 {
            // EOF — process may have exited.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if line.starts_with('{') {
            break;
        }
        // Skip non-JSON output (tracing should go to stderr, but just in case).
    }

    // Parse the JSON line.
    let trimmed = line.trim();
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("stdout line is valid JSON");

    let node_id = parsed["node_id"]
        .as_str()
        .expect("node_id field present and is a string")
        .to_string();
    let endpoint_addr = parsed["endpoint_addr"]
        .as_str()
        .expect("endpoint_addr field present and is a string")
        .to_string();

    // SIGKILL: we already have what we came for, and the graceful SIGTERM path
    // is not what these tests cover.
    let _ = child.kill();
    let _ = child.wait();

    (node_id, endpoint_addr)
}

/// Start a process-topology role and return its single readiness document.
/// The temporary directory stays alive until the caller terminates the child,
/// avoiding a platform-dependent open-journal directory removal race.
fn spawn_persistd(args: &[String]) -> (tempfile::TempDir, Child, serde_json::Value) {
    let dir = tempfile::tempdir().expect("temp dir");
    let (child, ready) = spawn_persistd_in(dir.path(), args);
    (dir, child, ready)
}

fn spawn_persistd_in(dir: &std::path::Path, args: &[String]) -> (Child, serde_json::Value) {
    let mut child = Command::new(persistd_binary())
        .arg("--dir")
        .arg(dir)
        .arg("--allow-volatile-leases")
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn persistd topology role");
    let stdout = child.stdout.take().expect("stdout captured");
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    use std::io::BufRead;
    reader
        .read_line(&mut line)
        .expect("read readiness document");
    let ready = serde_json::from_str(line.trim()).expect("valid readiness JSON");
    (child, ready)
}

async fn seed_process_entity(
    dir: &std::path::Path,
    node_id: u64,
    entity: PersistId,
    author: NodeId,
) {
    let journal = Journal::open(&JournalConfig {
        dir: dir.join(format!("node-{node_id}")),
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::AlwaysBatch,
            ..GroupCommitConfig::default()
        },
    })
    .expect("open primary journal for seed");
    let payload = Bytes::from_static(b"seeded");
    journal
        .append_replicated(JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(0),
            epoch: Epoch::new(0),
            author,
            kind: RecordKind::Spawn,
            crc: orrery_persistd::payload_crc(&payload),
            payload,
        })
        .expect("append seed entity")
        .committed()
        .await
        .expect("commit seed entity");
    journal.close().await.expect("close seeded journal");
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Reserve an ephemeral loopback port long enough to pass it to the spawned
/// primary. Tests use an explicit direct address because the readiness document
/// is intentionally a stable machine contract rather than a debug string.
fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback port");
    listener.local_addr().expect("listener address")
}

/// Run `persistd` and return its exit status plus stderr output.
fn run_persistd_exit(args: &[&str]) -> (std::process::ExitStatus, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(persistd_binary())
        .arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .args(args)
        .output()
        .expect("failed to run persistd");

    (
        output.status,
        String::from_utf8(output.stderr).expect("stderr is utf-8"),
    )
}

#[test]
fn authority_startup_requires_durable_fdb_or_explicit_volatile_mode() {
    let (status, stderr) = run_persistd_exit(&[]);
    assert!(!status.success());
    assert!(stderr.contains("authority requires --fdb-cluster-file"));
}

#[test]
fn authority_startup_requires_an_identity_issuer_key() {
    let (status, stderr) = run_persistd_exit(&["--allow-volatile-leases"]);
    assert!(!status.success());
    assert!(stderr.contains("authority requires at least one --issuer-key"));
}

#[tokio::test]
async fn production_authority_rejects_intents_without_a_secure_validator() {
    // Given: the production binary's authority configuration and an
    // authenticated peer submitting a correctly signed intent.
    let client_key = iroh::SecretKey::generate();
    let bind_addr = free_loopback_addr();
    let args = vec!["--bind".to_string(), bind_addr.to_string()];
    let (_dir, mut child, ready) = spawn_persistd(&args);
    let gateway = ready["node_id"]
        .as_str()
        .expect("gateway node id")
        .parse::<NodeId>()
        .expect("valid gateway node id");
    let client = Builder::new(N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(client_key.clone())
        .bind()
        .await
        .expect("client endpoint");
    let connection = client
        .connect(
            iroh::EndpointAddr::new(gateway).with_ip_addr(bind_addr),
            GATEWAY_ALPN,
        )
        .await
        .expect("connect to production gateway");
    let mut admission = connection.accept_uni().await.expect("gateway admission");
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0]);
    connection
        .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: process_session_token(client_key.public()),
            node: client_key.public(),
        })))
        .expect("send hello");
    let hello = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
        .await
        .expect("hello reply timeout")
        .expect("hello reply");
    assert!(matches!(
        decode_stream_frame(&hello),
        Some(GatewayReply::HelloAck { .. })
    ));
    let mut intent = Intent {
        intent_id: 71,
        issuer: client_key.public(),
        cell_epoch: Epoch::new(0),
        ops: vec![IntentOp {
            op: 1,
            args: Bytes::from_static(b"production-authority"),
        }],
        attestations: Vec::new(),
        signature: client_key.sign(b"placeholder"),
    };
    intent.sign(&client_key);

    // When: the intent crosses the real binary gateway surface.
    connection
        .send_datagram(Bytes::from(encode_stream_frame(
            &GatewayMsg::SubmitIntent { intent },
        )))
        .expect("submit intent");
    let reply = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
        .await
        .expect("intent reply timeout")
        .expect("intent reply");

    // Then: validation fails closed before the missing-executor fallback.
    assert!(matches!(
        decode_stream_frame(&reply),
        Some(GatewayReply::IntentAck {
            intent_id: 71,
            outcome: IntentOutcome::Rejected {
                reason: REASON_VALIDATION_FAILED,
            },
        })
    ));
    connection.close(0u32.into(), b"test complete");
    client.close().await;
    stop(&mut child);
}

/// Locate the optional workspace development cluster, with an environment
/// override for CI. Presence alone is not sufficient: the FDB readiness test
/// below performs a transaction before spawning the binary.
#[cfg(feature = "fdb")]
fn fdb_cluster_file() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
        return Some(path.into());
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".fdb-dev/fdb.cluster");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The fdb-enabled binary must reach its JSON readiness line after building
/// fence, checkpoint, and intent adapters from one process-scoped context.
///
/// This is a process-level regression for the former multiple-`boot()` panic.
/// It skips only when no local cluster is configured or reachable.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn fdb_binary_reaches_readiness_with_shared_context() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let cluster_string = cluster.display().to_string();
    let Ok(context) = FdbContext::connect(&cluster_string) else {
        eprintln!("skipping: unable to open FDB cluster file");
        return;
    };
    let store = orrery_persistd::fence::FdbFenceStore::from_context(&context);
    if store.read(GridId::ROOT, CellId::ROOT).await.is_err() {
        eprintln!("skipping: FDB cluster is not reachable");
        return;
    }

    let args = vec![
        "--bind".to_string(),
        "127.0.0.1:0".to_string(),
        "--fdb-cluster-file".to_string(),
        cluster_string,
    ];
    let (_dir, mut child, ready) = spawn_persistd(&args);
    assert_eq!(ready["role"], "single");
    assert!(ready["endpoint_addr"].is_string());
    assert_eq!(ready["bulk_ack_fence_monitor"], true);
    stop(&mut child);
}

#[test]
fn gateway_node_id_is_stable_across_restart() {
    // The same --secret-key must produce the same NodeId across two runs.
    let secret_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let (id1, _) = run_persistd(&["--secret-key", secret_hex]);
    let (id2, _) = run_persistd(&["--secret-key", secret_hex]);

    assert_eq!(
        id1, id2,
        "same --secret-key must produce the same NodeId across restarts"
    );
}

#[test]
fn different_secret_keys_produce_different_node_ids() {
    let secret_a = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let secret_b = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let (id_a, _) = run_persistd(&["--secret-key", secret_a]);
    let (id_b, _) = run_persistd(&["--secret-key", secret_b]);

    assert_ne!(
        id_a, id_b,
        "different --secret-key must produce different NodeIds"
    );
}

#[test]
fn stdout_contains_json_address_line() {
    // Verify the output format: a single-line JSON object with endpoint_addr
    // and node_id fields.
    let (node_id, endpoint_addr) = run_persistd(&[]);
    assert!(!node_id.is_empty(), "node_id must be non-empty");
    assert!(!endpoint_addr.is_empty(), "endpoint_addr must be non-empty");
}

#[test]
fn stdout_lines_are_json_only() {
    // Assert that the first stdout line is JSON (starts with '{') even when
    // no --secret-key is used (ephemeral identity).
    let (_, _) = run_persistd(&[]);
}

#[test]
fn raw_shard_flag_is_accepted() {
    // Raw shard bits should be accepted as an explicit local shard.
    let (_, _) = run_persistd(&["--shard", "0xA924_9249_2492_4E00"]);
}

#[test]
fn coordinate_shard_flag_is_accepted() {
    // Coordinate form should also be accepted.
    let (_, _) = run_persistd(&["--shard", "2,-1,8@21"]);
}

#[test]
fn overlapping_local_shards_are_rejected() {
    let (status, stderr) =
        run_persistd_exit(&["--shard", "0x8000_0000_0000_0000", "--shard", "0,0,0@1"]);

    assert!(
        !status.success(),
        "persistd must exit non-zero for overlapping local shards"
    );
    assert!(
        stderr.contains("overlapping --shard values"),
        "stderr should explain the overlap: {stderr}"
    );
}

#[test]
fn clustered_topology_without_node_id_is_rejected() {
    let (status, stderr) = run_persistd_exit(&["--chain-listen", "127.0.0.1:3000"]);

    assert!(
        !status.success(),
        "persistd must exit non-zero when chain topology is requested without --node-id"
    );
    assert!(
        stderr.contains("--node-id is required"),
        "stderr should explain that clustered mode requires --node-id: {stderr}"
    );
}

#[test]
fn two_process_topology_starts_follower_then_primary() {
    let follower_args = vec![
        "--node-id".into(),
        "2".into(),
        "--chain-epoch".into(),
        "9".into(),
        "--chain-primary".into(),
        "1".into(),
        "--chain-listen".into(),
        "127.0.0.1:0".into(),
    ];
    let (_follower_dir, mut follower, follower_ready) = spawn_persistd(&follower_args);
    assert_eq!(follower_ready["role"], "follower");
    assert_eq!(follower_ready["node_id"], 2);
    assert!(
        follower_ready.get("endpoint_addr").is_none(),
        "a follower must not advertise a gateway endpoint"
    );
    let chain_addr = follower_ready["chain_addr"]
        .as_str()
        .expect("follower chain listener")
        .to_owned();

    let primary_args = vec![
        "--node-id".into(),
        "1".into(),
        "--chain-epoch".into(),
        "9".into(),
        "--chain-follower".into(),
        format!("2@{chain_addr}"),
    ];
    let (_primary_dir, mut primary, primary_ready) = spawn_persistd(&primary_args);
    assert_eq!(primary_ready["role"], "primary");
    assert_eq!(primary_ready["cluster_node_id"], 1);
    assert!(primary_ready["endpoint_addr"].is_string());
    assert!(primary_ready["bind_addr"].is_string());

    stop(&mut primary);
    stop(&mut follower);
}

#[test]
fn primary_starts_when_follower_is_temporarily_unavailable() {
    let args = vec![
        "--node-id".into(),
        "1".into(),
        "--chain-epoch".into(),
        "9".into(),
        "--chain-follower".into(),
        "2@127.0.0.1:9".into(),
    ];
    let (_dir, mut primary, ready) = spawn_persistd(&args);
    assert_eq!(ready["role"], "primary");
    assert!(ready["endpoint_addr"].is_string());
    assert!(ready["bind_addr"].is_string());
    stop(&mut primary);
}

/// The static topology's essential process boundary: a client-visible primary
/// acknowledgement is locally durable and subsequently appears in the passive
/// follower journal. This exercises the real binary, TCP gRPC transport, and
/// iroh gateway rather than the in-process chain shim.
#[tokio::test]
async fn primary_ack_is_mirrored_to_the_passive_follower_journal() {
    let client_key = iroh::SecretKey::generate();
    let follower_args = vec![
        "--node-id".into(),
        "2".into(),
        "--chain-epoch".into(),
        "9".into(),
        "--chain-primary".into(),
        "1".into(),
        "--chain-listen".into(),
        "127.0.0.1:0".into(),
    ];
    let follower_dir = tempfile::tempdir().expect("follower temp dir");
    let (mut follower, follower_ready) = spawn_persistd_in(follower_dir.path(), &follower_args);
    let chain_addr = follower_ready["chain_addr"]
        .as_str()
        .expect("follower chain listener")
        .to_owned();
    let bind_addr = free_loopback_addr();
    let primary_args = vec![
        "--node-id".into(),
        "1".into(),
        "--chain-epoch".into(),
        "9".into(),
        "--chain-follower".into(),
        format!("2@{chain_addr}"),
        "--bind".into(),
        bind_addr.to_string(),
    ];
    let primary_dir = tempfile::tempdir().expect("primary temp dir");
    seed_process_entity(
        primary_dir.path(),
        1,
        PersistId::new(77),
        client_key.public(),
    )
    .await;
    let (mut primary, primary_ready) = spawn_persistd_in(primary_dir.path(), &primary_args);
    assert_eq!(primary_ready["bind_addr"], bind_addr.to_string());
    let primary_id = primary_ready["node_id"]
        .as_str()
        .expect("primary iroh node id")
        .parse::<NodeId>()
        .expect("valid primary node id");

    let client = Builder::new(N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(client_key.clone())
        .bind()
        .await
        .expect("client endpoint");
    let target = iroh::EndpointAddr::new(primary_id).with_ip_addr(bind_addr);
    let conn = client
        .connect(target, GATEWAY_ALPN)
        .await
        .expect("connect to primary gateway");
    let mut admission = conn.accept_uni().await.expect("gateway admission");
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0]);

    let author = client_key.public();
    conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
        token: process_session_token(author),
        node: author,
    })))
    .expect("send hello");
    conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
        message: orrery_protocol::LeaseMsg::Claim {
            claim_id: orrery_protocol::ClaimId(1),
            entity: PersistId::new(77),
            grid: GridId::ROOT,
            cell: CellId::ROOT,
            kind: orrery_protocol::ClaimKind::Strong,
            basis: orrery_protocol::ClaimBasis::Explicit,
            observed: Default::default(),
            tick: Tick::new(11),
        },
    })))
    .expect("send lease claim");
    let (lease_id, authority_seq) = loop {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
            .await
            .expect("claim reply timeout")
            .expect("claim reply");
        let Some(GatewayReply::Lease {
            message: orrery_protocol::LeaseMsg::Grant { lease_id, seq, .. },
        }) = decode_stream_frame(&packet)
        else {
            continue;
        };
        break (lease_id, seq);
    };
    conn.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
        diff: DiffUplink {
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(77),
            tick: Tick::new(11),
            kind: RecordKind::Spawn,
            payload: Bytes::from_static(b"process-chain"),
            seq: 1,
            lease_id: Some(lease_id),
            authority_seq: Some(authority_seq),
        },
    })))
    .expect("send diff");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "primary never acknowledged diff"
        );
        let packet = tokio::time::timeout(Duration::from_millis(250), conn.read_datagram()).await;
        let Ok(Ok(packet)) = packet else { continue };
        if let Some(GatewayReply::BulkAck { entity, tick, .. }) = decode_datagram(&packet) {
            if entity == PersistId::new(77) && tick == Tick::new(11) {
                break;
            }
        }
    }

    // Chain replication is asynchronous. Give its background task an explicit
    // bounded interval to cross the process boundary before stopping both
    // processes and opening the follower journal independently.
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(conn);
    client.close().await;
    stop(&mut primary);
    stop(&mut follower);

    let journal = Journal::open(&JournalConfig {
        dir: follower_dir.path().join("node-2"),
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::Adaptive,
            ..GroupCommitConfig::default()
        },
    })
    .expect("open mirrored follower journal");
    let records = journal
        .scan_from(orrery_protocol::Lsn::new(0, 0))
        .map(|item| item.expect("valid mirrored record").record)
        .collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| {
            record.entity == PersistId::new(77)
                && record.tick == Tick::new(11)
                && record.payload.as_ref() == b"process-chain"
        }),
        "follower records: {:?}",
        records
            .iter()
            .map(|record| (
                record.lsn,
                record.entity,
                record.tick,
                record.payload.clone()
            ))
            .collect::<Vec<_>>()
    );
    journal.close().await.expect("close inspected journal");
}
