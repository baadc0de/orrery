//! Binary-level integration tests for the `persistd` binary.
//!
//! These tests spawn the compiled binary and assert its behavior: stable node
//! identity across restarts, the stdout JSON address line, and graceful signal
//! handling. They do NOT require FoundationDB.

mod lanes;
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
use orrery_protocol::channels::{decode_datagram, decode_stream_frame};
use orrery_protocol::{
    CellEpoch, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp,
    IntentOutcome, JournalRecord, Lsn, NodeId, PersistId, RecordKind, Tick, REASON_NO_EXECUTOR,
    REASON_VALIDATION_FAILED,
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

    // Snapshot the command line before spawning. A failure report that cannot
    // say what was run leaves the reader guessing at the one thing the harness
    // knows for certain.
    let command = format!("{cmd:?}");
    let mut child = cmd.spawn().expect("failed to spawn persistd");
    let parsed = match await_readiness(&mut child, &command) {
        Ok(parsed) => parsed,
        Err(why) => panic!("{why}"),
    };

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
    match try_spawn_persistd_in(dir, args) {
        Ok(spawned) => spawned,
        // `panic!` rather than `.expect()` on the `Result`: the message is
        // already a multi-line report, and `expect` would bury it inside an
        // `Err(..)` debug rendering with the newlines escaped.
        Err(why) => panic!("{why}"),
    }
}

/// The fallible half of [`spawn_persistd_in`], split out so the harness's own
/// diagnostics can be asserted on without catching a panic. Every test uses the
/// panicking wrapper; only the regression test for issue #139 calls this.
fn try_spawn_persistd_in(
    dir: &std::path::Path,
    args: &[String],
) -> Result<(Child, serde_json::Value), String> {
    let mut cmd = Command::new(persistd_binary());
    cmd.arg("--dir")
        .arg(dir)
        .arg("--allow-volatile-leases")
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let command = format!("{cmd:?}");
    let mut child = cmd.spawn().expect("spawn persistd topology role");
    let ready = await_readiness(&mut child, &command)?;
    Ok((child, ready))
}

/// How long a freshly spawned `persistd` gets to print its readiness line.
///
/// The same ten seconds `run_persistd` already allowed itself for the address
/// line, now applied to every spawn in this file rather than one of them.
/// Startup on an idle box is milliseconds; the bound exists to turn a hang into
/// a report, not to pace a slow machine, so a value generous enough to survive
/// a loaded shared runner is the right one to reuse.
const READINESS_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a spawned child never produced a readiness document.
///
/// Each variant is a distinct thing the harness observed, and each earns its
/// own sentence: "it died", "it went quiet" and "it said something else" send
/// an investigation to three different places.
enum ReadinessProblem {
    /// The child closed stdout without printing anything — it is dead.
    Died,
    /// The child was still silent when [`READINESS_TIMEOUT`] expired.
    Silent,
    /// Reading the child's stdout failed outright.
    Unreadable(String),
    /// A line arrived, but it was not a readiness document.
    NotJson {
        /// The line as read, quoted verbatim.
        line: String,
        /// What `serde_json` made of it.
        error: String,
    },
}

/// Wait, with a bound, for a spawned child's readiness document.
///
/// Issue #139. The body this replaces was `read_line(..).expect(..)` followed
/// by `from_str(..).expect("valid readiness JSON")`. A child that died during
/// startup — a failed bind, a resource limit, an OOM on this shared box — made
/// `read_line` return zero bytes, and the test then failed with
/// `Error("EOF while parsing a value", line: 1, column: 0)`. That describes the
/// shape of an empty read. It says nothing about the child, whose exit status
/// was never checked and whose stderr was captured and then dropped on the
/// floor, at exactly the moment the cause was knowable. Two investigations
/// ended in "probably contention" for want of those two facts.
///
/// So: never parse a short read. Reap the child, join its stderr, and report
/// the status, the output and the command line together. There is deliberately
/// no retry here — retrying a failure you cannot yet read converts a visible
/// flake into an invisible one, which is the worse of the two.
fn await_readiness(child: &mut Child, command: &str) -> Result<serde_json::Value, String> {
    use std::io::{BufRead, Read};
    use std::sync::mpsc::{self, RecvTimeoutError};

    let stdout = child.stdout.take().expect("stdout captured");
    let mut stderr = child.stderr.take().expect("stderr captured");

    // Drain stderr on a thread of its own, for the child's whole life. Two
    // reasons, and this file is exposed to both: a pipe nobody reads fills at
    // 64 KiB and blocks the child mid-startup, and the one diagnostic worth
    // having when a child dies is precisely what it wrote there. The thread
    // ends when the child's stderr closes, which is when the child exits, so a
    // healthy spawn leaks nothing.
    let stderr_drain = std::thread::spawn(move || {
        let mut raw = Vec::new();
        let _ = stderr.read_to_end(&mut raw);
        // Lossy on purpose: one mangled byte must not cost the whole message.
        String::from_utf8_lossy(&raw).into_owned()
    });

    // Read stdout on another thread, so that the wait below can be bounded at
    // all. `read_line` on a pipe blocks indefinitely, so a child that hangs
    // before printing would otherwise hang the test along with it.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            let sent = match reader.read_line(&mut line) {
                Ok(0) => tx.send(Err(ReadinessProblem::Died)),
                Ok(_) if line.trim_start().starts_with('{') => tx.send(Ok(line)),
                // Tracing is configured onto stderr, but a stray line that
                // reaches stdout anyway must not be mistaken for the readiness
                // document — skip it and keep reading.
                Ok(_) => continue,
                Err(err) => tx.send(Err(ReadinessProblem::Unreadable(err.to_string()))),
            };
            let _ = sent;
            return;
        }
    });

    let start = std::time::Instant::now();
    let outcome = rx.recv_timeout(READINESS_TIMEOUT);
    let waited = start.elapsed();
    let problem = match outcome {
        Ok(Ok(line)) => match serde_json::from_str(line.trim()) {
            Ok(ready) => return Ok(ready),
            Err(error) => ReadinessProblem::NotJson {
                line,
                error: error.to_string(),
            },
        },
        Ok(Err(problem)) => problem,
        Err(RecvTimeoutError::Timeout) => ReadinessProblem::Silent,
        // The reader thread drops its sender only after sending, so a
        // disconnect means it died before it could say anything at all.
        Err(RecvTimeoutError::Disconnected) => {
            ReadinessProblem::Unreadable("the stdout reader thread died".to_string())
        }
    };
    Err(readiness_failure(
        child,
        command,
        stderr_drain,
        waited,
        problem,
    ))
}

/// Reap a child that failed to become ready and render the whole story as one
/// message: what was observed, how the child ended, how long it was given, what
/// was run, and everything it wrote to stderr.
fn readiness_failure(
    child: &mut Child,
    command: &str,
    stderr_drain: std::thread::JoinHandle<String>,
    waited: Duration,
    problem: ReadinessProblem,
) -> String {
    // Kill first, then reap, then join. Signalling a child that has already
    // exited is a no-op that leaves its recorded status alone, and signalling
    // one that has hung is the only way to close its stderr so the drain can
    // reach EOF — joining before the kill would deadlock on exactly the case
    // this function exists for.
    let _ = child.kill();
    let status = match child.wait() {
        Ok(status) => status.to_string(),
        Err(err) => format!("unavailable ({err})"),
    };
    let stderr = stderr_drain
        .join()
        .unwrap_or_else(|_| "<the stderr drain thread panicked>".to_string());

    let head = match problem {
        ReadinessProblem::Died => {
            format!("persistd exited with status `{status}` before printing its readiness line")
        }
        ReadinessProblem::Silent => format!(
            "persistd printed no readiness line before the bound expired; \
             it was killed and reaped as `{status}`"
        ),
        ReadinessProblem::Unreadable(err) => {
            format!("reading persistd's stdout failed ({err}); it was reaped as `{status}`")
        }
        ReadinessProblem::NotJson { line, error } => format!(
            "persistd printed {line:?}, which is not a readiness document ({error}); \
             it was reaped as `{status}`"
        ),
    };

    let stderr = stderr.trim_end();
    let stderr = if stderr.is_empty() {
        "    <the child wrote nothing to stderr>".to_string()
    } else {
        stderr
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "{head}\n  waited: {waited:?} of {READINESS_TIMEOUT:?}\n  command: {command}\n  stderr:\n{stderr}"
    )
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

/// The deployed binary's admission filter is a *filter*, not a blanket
/// refusal.
///
/// It used to be the latter: `ProductionIntentValidator` ignored its argument
/// and answered `Reject { REASON_VALIDATION_FAILED }` to everything, which
/// made D16's "intent commit p99 < 10 ms" unmeasurable by construction — the
/// P2 gate's own run recorded `committed: 0, rejected: 1024`. So the assertion
/// here is two-sided on purpose: a well-formed intent must get *past*
/// validation (this node has no executor, so the honest answer beyond it is
/// `REASON_NO_EXECUTOR`), and a malformed one must still be refused. Either
/// half alone is satisfied by a validator that is a constant function.
#[tokio::test]
async fn production_authority_admits_well_formed_intents_and_refuses_malformed_ones() {
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
    // Read admission before attaching, or the lane reader consumes it.
    let mut admission = connection.accept_uni().await.expect("gateway admission");
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0]);
    let connection = lanes::GatewayLanes::attach(connection);
    connection
        .send_control(&GatewayMsg::Hello {
            token: process_session_token(client_key.public()),
            node: client_key.public(),
        })
        .await;
    assert!(matches!(
        connection.next_reply(Duration::from_secs(5)).await,
        Some(GatewayReply::HelloAck { .. })
    ));
    let signed = |intent_id: u128, ops: Vec<IntentOp>| {
        let mut intent = Intent {
            evidence: None,
            intent_id,
            issuer: client_key.public(),
            cell_epoch: CellEpoch::new(0),
            ops,
            attestations: Vec::new(),
            signature: client_key.sign(b"placeholder"),
        };
        intent.sign(&client_key);
        intent
    };

    // When: a well-formed, `Ruleset`-opaque intent crosses the real binary
    // gateway surface.
    connection
        .send_control(&GatewayMsg::SubmitIntent {
            intent: signed(
                71,
                vec![IntentOp {
                    op: 1,
                    args: Bytes::from_static(b"production-authority"),
                }],
            ),
        })
        .await;
    let reply = connection
        .next_payload(Duration::from_secs(5))
        .await
        .expect("intent reply timeout");

    // Then: it passes admission and reaches the executor seam, which this
    // node (started without --fdb-cluster-file) does not have.
    let ack: Option<GatewayReply> = decode_stream_frame(&reply);
    assert!(
        matches!(
            ack,
            Some(GatewayReply::IntentAck {
                intent_id: 71,
                outcome: IntentOutcome::Rejected {
                    reason: REASON_NO_EXECUTOR,
                },
            })
        ),
        "a well-formed intent must get past validation: {ack:?}"
    );

    // And: an intent with nothing to commit is still refused by the filter,
    // before the executor seam is consulted at all.
    connection
        .send_control(&GatewayMsg::SubmitIntent {
            intent: signed(72, Vec::new()),
        })
        .await;
    let reply = connection
        .next_payload(Duration::from_secs(5))
        .await
        .expect("intent reply timeout");
    let ack: Option<GatewayReply> = decode_stream_frame(&reply);
    assert!(
        matches!(
            ack,
            Some(GatewayReply::IntentAck {
                intent_id: 72,
                outcome: IntentOutcome::Rejected {
                    reason: REASON_VALIDATION_FAILED,
                },
            })
        ),
        "a malformed intent must still be refused: {ack:?}"
    );
    connection.conn().close(0u32.into(), b"test complete");
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
fn readiness_reports_whether_the_cluster_can_adjudicate() {
    // Whether a report can be judged here is not otherwise visible from
    // outside the process: without an adjudicator every escalation comes back
    // `REPORT_REFUSED_NO_ADJUDICATOR`, which the witness sees and the operator
    // does not. `false` is the correct answer for a stock build — registering
    // a rules build means linking a `Ruleset` into the deployed binary
    // (docs/09-services-and-ops.md §1) — so what is asserted is that the field
    // is present and honest, not that it is set.
    let (_dir, mut child, ready) =
        spawn_persistd(&["--bind".to_string(), "127.0.0.1:0".to_string()]);
    assert_eq!(
        ready["adjudicator"], false,
        "a stock persistd links no Ruleset and must say so"
    );
    stop(&mut child);
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
fn a_child_that_dies_at_startup_reports_its_exit_status_and_stderr() {
    // Issue #139, and the reason this test exists at all: a child that died
    // during startup used to surface as
    // `valid readiness JSON: Error("EOF while parsing a value", line: 1,
    // column: 0)`, which describes the shape of an empty read and nothing about
    // the child. Twice that cost an investigation that ended in "probably
    // contention" because the exit status and stderr had been discarded.
    //
    // Force a real startup failure — overlapping `--shard` values, the same
    // rejection `overlapping_local_shards_are_rejected` relies on — and assert
    // that the harness reports what the child said on its way out. Asserting on
    // the message rather than on a panic is why `try_spawn_persistd_in` exists.
    let dir = tempfile::tempdir().expect("temp dir");
    let why = match try_spawn_persistd_in(
        dir.path(),
        &[
            "--bind".to_string(),
            "127.0.0.1:0".to_string(),
            "--shard".to_string(),
            "0x8000_0000_0000_0000".to_string(),
            "--shard".to_string(),
            "0,0,0@1".to_string(),
        ],
    ) {
        Ok((mut child, _ready)) => {
            stop(&mut child);
            panic!(
                "persistd accepted overlapping --shard values; this test needs another way to \
                 make startup fail"
            );
        }
        Err(why) => why,
    };

    assert!(
        why.contains("persistd exited with status"),
        "a dead child must be reported as dead, with its status: {why}"
    );
    assert!(
        why.contains("overlapping --shard values"),
        "the child's stderr must be surfaced, not discarded: {why}"
    );
    assert!(
        why.contains(persistd_binary()),
        "the report must say what was run: {why}"
    );
    assert!(
        !why.contains("EOF while parsing"),
        "a short read must never be handed to the JSON parser: {why}"
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
    // Read admission before attaching, or the lane reader consumes it.
    let mut admission = conn.accept_uni().await.expect("gateway admission");
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0]);
    let conn = lanes::GatewayLanes::attach(conn);

    let author = client_key.public();
    conn.send_control(&GatewayMsg::Hello {
        token: process_session_token(author),
        node: author,
    })
    .await;
    conn.send_control(&GatewayMsg::Lease {
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
    })
    .await;
    let (lease_id, authority_seq) = loop {
        let packet = conn
            .next_payload(Duration::from_secs(5))
            .await
            .expect("claim reply timeout");
        let Some(GatewayReply::Lease {
            message: orrery_protocol::LeaseMsg::Grant { lease_id, seq, .. },
        }) = decode_stream_frame(&packet)
        else {
            continue;
        };
        break (lease_id, seq);
    };
    conn.send_state(&GatewayMsg::Diff {
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
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "primary never acknowledged diff"
        );
        let Some(packet) = conn.next_payload(Duration::from_millis(250)).await else {
            continue;
        };
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

/// The other half of the mirroring contract, and the one the P2 kill-9 gate
/// tripped over: a diff the strict-authority fence refuses is never appended,
/// so it is never mirrored either. An empty follower mirror after a load is
/// therefore evidence about *the writes*, not about chain replication.
///
/// `scripts/p2-kill9-gate.sh` reached `prove_epoch_fork_refused` and reported
/// that the follower held no mirrored record at all. The chain was healthy:
/// the rig (`p2-load`) sends every `DiffUplink` with `lease_id: None` — see the
/// comment at `p2-load/src/main.rs:1401` — while `route_session_diff` sets
/// `strict_authority: true` unconditionally (`gateway.rs:3930`), so
/// `route_diff` substitutes the never-granted `LeaseId(0)` and `apply_fenced`
/// rejects every uplink before the journal sees a byte. Nothing was
/// acknowledged, nothing was committed, and the mirror was correctly empty.
///
/// This is the same topology as
/// `primary_ack_is_mirrored_to_the_passive_follower_journal` with exactly one
/// difference — the lease claim is gone — so the pair localizes the emptiness
/// to the fence rather than to the chain.
#[tokio::test]
async fn an_unleased_diff_is_nacked_and_never_reaches_the_follower_mirror() {
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
        PersistId::new(78),
        client_key.public(),
    )
    .await;
    let (mut primary, primary_ready) = spawn_persistd_in(primary_dir.path(), &primary_args);
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
    let conn = lanes::GatewayLanes::attach(conn);

    let author = client_key.public();
    conn.send_control(&GatewayMsg::Hello {
        token: process_session_token(author),
        node: author,
    })
    .await;

    // The rig's exact uplink shape: no lease, no authority sequence.
    conn.send_state(&GatewayMsg::Diff {
        diff: DiffUplink {
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(78),
            tick: Tick::new(12),
            kind: RecordKind::ComponentDiff,
            payload: Bytes::from_static(b"unleased-diff"),
            seq: 1,
            lease_id: None,
            authority_seq: None,
        },
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "primary neither acked nor nacked an unleased diff"
        );
        let Some(packet) = conn.next_payload(Duration::from_millis(250)).await else {
            continue;
        };
        match decode_datagram(&packet) {
            Some(GatewayReply::BulkNack { entity, tick, .. })
                if entity == PersistId::new(78) && tick == Tick::new(12) =>
            {
                break;
            }
            Some(GatewayReply::BulkAck { entity, tick, .. })
                if entity == PersistId::new(78) && tick == Tick::new(12) =>
            {
                panic!("an unleased diff was acknowledged; the authority fence did not run")
            }
            _ => continue,
        }
    }

    // The same bounded interval the mirroring test gives the chain task, so a
    // record that *would* have crossed the process boundary had time to.
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
    let mirrored = journal
        .scan_from(Lsn::new(0, 0))
        .map(|item| item.expect("valid mirrored record").record)
        .collect::<Vec<_>>();
    assert!(
        mirrored.is_empty(),
        "a refused diff must not be mirrored; follower holds {:?}",
        mirrored
            .iter()
            .map(|record| (record.lsn, record.entity, record.tick))
            .collect::<Vec<_>>()
    );
    journal.close().await.expect("close inspected journal");
}

#[test]
fn readiness_reports_the_owned_shard_set_with_per_shard_epochs() {
    // A node can now tell a peer "you are talking to the wrong owner"
    // (`DenyReason::WrongOwner`, docs/08-persistence.md §3.5). Nothing outside
    // the process could check that claim: which shards were owned here was
    // knowable only from the `--shard` flags the harness itself passed, which
    // is not evidence — it is the input restated. This publishes the answer
    // the node actually activated.
    //
    // Two shards, not one, so an implementation that reported a single shard
    // or collapsed the list would fail rather than pass by arity.
    let mut octants = CellId::ROOT.children();
    octants.sort_unstable();
    let (first, second) = (octants[0].to_bits(), octants[1].to_bits());
    let (_dir, mut child, ready) = spawn_persistd(&[
        "--bind".to_string(),
        "127.0.0.1:0".to_string(),
        "--shard".to_string(),
        format!("{first:#x}"),
        "--shard".to_string(),
        format!("{second:#x}"),
    ]);

    let shards = ready["shards"]
        .as_array()
        .expect("readiness line carries the owned shard set");
    let cells: Vec<u64> = shards
        .iter()
        .map(|entry| entry["cell"].as_u64().expect("shard cell is raw bits"))
        .collect();
    assert_eq!(
        cells,
        vec![first, second],
        "the published set must be exactly the --shard flags, in canonical order"
    );

    // Per shard, not one figure for the node: activation refuses a mixed set
    // today, and showing that rather than asking a reader to know it is the
    // point of the field.
    let epoch = ready["ownership_epoch"]
        .as_u64()
        .expect("readiness line carries the ownership epoch");
    for entry in shards {
        assert_eq!(
            entry["epoch"].as_u64().expect("shard epoch present"),
            epoch,
            "every shard's own row must agree with the node's activation epoch"
        );
    }

    stop(&mut child);
}
