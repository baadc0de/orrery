//! Operator-visible gateway telemetry: the server-side spans, the report
//! outcome split, and the promise that none of it depends on a flag.
//!
//! Three claims are proven here, and the first is the one a naive reading gets
//! backwards.
//!
//! 1. **A running `persistd` emits the two new server-side spans, under names
//!    D16 does not gate.** `gateway_intent_server_ms` and
//!    `gateway_area_first_page_server_ms` measure receipt-through-send inside
//!    the process; `intent_commit_ms` and `area_first_page_ms` measure client
//!    round trips and carry the D16 targets. `gates/p2-dashboard` folds by series
//!    name into one histogram per name with no source field, and
//!    `scripts/p2-kill9-gate.sh` concatenates the client rig's JSONL with
//!    persistd's into that fold — so a server span written under a gated name
//!    would *lower* the gated p99 and pass a gate it never measured. The test
//!    asserts the new names are present and the gated ones are absent from
//!    persistd's own file.
//! 2. **Every report outcome moves its own counter**, refusals included.
//! 3. **A gateway with no metrics sink still counts.** Collection is
//!    unconditional; only the JSONL sink is optional. The honest limit is
//!    stated in `GatewayMetrics`' own documentation and repeated at the test
//!    that proves the counting: there is no scrape or admin surface on
//!    `persistd`, so on a node started without `--metrics-jsonl` these
//!    counters are correct, warm, and reachable by nothing until the D12 OTel
//!    bridge lands.

mod lanes;
mod support;

use std::io::BufRead;
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    AdjudicationExecutor, CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore,
    Router, RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::metrics::{
    GATED_SERIES, SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER, SERIES_GATEWAY_INTENT_SERVER,
};
use orrery_protocol::{
    CellEpoch, CellId, ChainHash, ClaimBasis, ClaimId, ClaimKind, DiffUplink, DiscrepancyReport,
    Epoch, EvidenceBundle, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
    LeaseMsg, NodeId, PersistId, RecordKind, RulesetId, StateClaim, Tick, Verdict,
    REASON_NO_EXECUTOR, REPORT_ADJUDICATED, REPORT_REFUSED_NO_ADJUDICATOR,
    REPORT_REFUSED_RATE_LIMITED,
};
use tokio::sync::Mutex;

// ── Fixtures ─────────────────────────────────────────────────────────────

/// A rules build nothing registers, so an adjudicator that has registered
/// nothing answers `Unadjudicable(UnknownRuleset)`.
///
/// That is a real adjudication — `REPORT_ADJUDICATED`, a verdict on the wire —
/// and it is all this lane needs. Re-executing a window is
/// `report_escalation.rs`'s subject; what is asserted here is which counter
/// moves.
const UNREGISTERED_RULESET: RulesetId = RulesetId {
    version: 9,
    digest: [0x99; 32],
};

const ENTITY: PersistId = PersistId::new(4242);

fn thin_bundle() -> EvidenceBundle {
    let subject = support::secret(2);
    EvidenceBundle {
        ruleset: UNREGISTERED_RULESET,
        entity: ENTITY,
        window_start: Tick::new(10),
        window_end: Tick::new(20),
        t0_claim: StateClaim {
            entity: ENTITY,
            chain_epoch: 0,
            tick: Tick::new(10),
            input_head: ChainHash([0; 32]),
            state_hash: [0; 32],
            prev_claim: [0; 32],
            ruleset: UNREGISTERED_RULESET,
            sig: subject.sign(b"t0"),
        },
        t0_snapshot: Bytes::new(),
        frames: Vec::new(),
        sibling_heads: Vec::new(),
        disputed_claims: Vec::new(),
        claimed_hashes: Vec::new(),
        computed_hashes: Vec::new(),
    }
}

fn signed_report(reporter: &iroh_base::SecretKey) -> Box<DiscrepancyReport> {
    Box::new(orrery_witness::sign_report(
        reporter,
        support::node(2),
        thin_bundle(),
    ))
}

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(20),
                ..GroupCommitConfig::default()
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// A live in-process gateway plus a connected, `Hello`-completed client.
struct Session {
    server: GatewayServer,
    conn: lanes::GatewayLanes,
    _client: iroh::Endpoint,
    _dir: tempfile::TempDir,
    runtime: Arc<Mutex<CellRuntime>>,
}

/// Commit a `Spawn` for `entity` in the root cell.
///
/// A player-basis claim is only plausible for an entity the cluster has
/// already committed somewhere (D7 §4.2), so a lease — and therefore a fenced
/// write, and therefore a bulk acknowledgement — needs this first.
async fn seed_entity(runtime: &Arc<Mutex<CellRuntime>>, entity: PersistId) {
    let actor = runtime
        .lock()
        .await
        .actor(GridId::ROOT, CellId::ROOT)
        .expect("actor for the hosted root cell");
    let payload = Bytes::from_static(b"seeded");
    actor
        .start_diff(orrery_protocol::JournalRecord {
            lsn: orrery_protocol::Lsn::new(0, 0),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(0),
            epoch: Epoch::new(0),
            author: support::node(9),
            kind: RecordKind::Spawn,
            crc: orrery_persistd::payload_crc(&payload),
            payload,
        })
        .await
        .expect("seed append")
        .committed()
        .await
        .expect("seed commit");
}

async fn connect(config: GatewayConfig, key: &iroh_base::SecretKey) -> Session {
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = Arc::new(Mutex::new({
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        CellRuntime::open(&runtime_config(dir.path()), &store)
            .await
            .expect("open runtime")
    }));
    let router: Arc<dyn Router> = runtime.clone();
    let server = GatewayServer::spawn(config, router)
        .await
        .expect("spawn gateway");
    let addr = server.addr();

    let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key.clone())
        .bind()
        .await
        .expect("bind client endpoint");
    let conn = client
        .connect(addr, GATEWAY_ALPN)
        .await
        .expect("connect to gateway");
    // Read admission before attaching, or the lane reader consumes it.
    let mut admission = conn.accept_uni().await.expect("admission stream");
    assert_eq!(
        admission.read_to_end(16).await.expect("admission"),
        vec![0u8]
    );
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
        runtime,
    }
}

/// File one report and read back its single answer.
async fn file(
    conn: &lanes::GatewayLanes,
    report: Box<DiscrepancyReport>,
) -> (Option<Verdict>, u16) {
    conn.send_control(&GatewayMsg::Report { report }).await;
    for _ in 0..8 {
        if let Some(GatewayReply::ReportVerdict {
            verdict, reason, ..
        }) = conn.next_reply(Duration::from_secs(5)).await
        {
            return (verdict, reason);
        }
    }
    panic!("no ReportVerdict after 8 inbound replies");
}

fn adjudicating_config(peer: NodeId) -> GatewayConfig {
    // Registers no build: the verdict is `Unadjudicable(UnknownRuleset)`,
    // which is an adjudication and not a refusal — exactly the distinction the
    // counters have to keep.
    let adjudicator = AdjudicationExecutor::new(orrery_protocol::UniverseSeed([0x5A; 32]));
    GatewayConfig {
        adjudicator: Some(Arc::new(adjudicator)),
        ..support::authority_config(peer, GridId::ROOT, vec![CellId::ROOT])
    }
}

// ── The report split ─────────────────────────────────────────────────────

#[tokio::test]
async fn every_report_outcome_moves_its_own_counter() {
    let reporter = support::secret(1);
    let session = connect(adjudicating_config(reporter.public()), &reporter).await;
    let metrics = Arc::clone(session.server.metrics());

    // One adjudicated.
    let (verdict, reason) = file(&session.conn, signed_report(&reporter)).await;
    assert_eq!(reason, REPORT_ADJUDICATED);
    assert!(verdict.is_some(), "an adjudicated report carries a verdict");
    let after_one = metrics.report.snapshot();
    assert_eq!(after_one.verdicts, 1);
    assert_eq!(after_one.adjudicated, 1);
    assert_eq!(after_one.unadjudicable, 1);
    assert_eq!(after_one.refused_rate_limited, 0);
    assert_eq!(after_one.refused_no_adjudicator, 0);
    assert_eq!(after_one.refused_other, 0);

    // One rate-limited: the documented burst is served, the flood past it is
    // shed (docs/07 §7), and a shed report is not a verdict.
    let mut shed = false;
    for _ in 0..48 {
        let (verdict, reason) = file(&session.conn, signed_report(&reporter)).await;
        if reason == REPORT_REFUSED_RATE_LIMITED {
            assert_eq!(verdict, None, "a shed report is not judged");
            shed = true;
            break;
        }
    }
    assert!(shed, "a 48-report flood must be shed somewhere");
    let after_flood = metrics.report.snapshot();
    assert_eq!(after_flood.refused_rate_limited, 1);
    assert!(after_flood.adjudicated > after_one.adjudicated);
    assert_eq!(after_flood.refused_no_adjudicator, 0);
    assert_eq!(after_flood.refused_other, 0);
    assert_eq!(
        after_flood.verdicts,
        after_flood.adjudicated + after_flood.refused_rate_limited,
        "every reply is accounted for exactly once"
    );
    session.server.shutdown().await;

    // One refused for no adjudicator — a stock build's answer to every report,
    // and the reason it needs a counter of its own rather than an error
    // bucket: from the witness side it is indistinguishable from a cluster
    // that exonerates everybody.
    let stock = support::authority_config(reporter.public(), GridId::ROOT, vec![CellId::ROOT]);
    assert!(stock.adjudicator.is_none(), "a stock build judges nothing");
    let session = connect(stock, &reporter).await;
    let metrics = Arc::clone(session.server.metrics());
    let (verdict, reason) = file(&session.conn, signed_report(&reporter)).await;
    assert_eq!(reason, REPORT_REFUSED_NO_ADJUDICATOR);
    assert_eq!(verdict, None);
    let refused = metrics.report.snapshot();
    assert_eq!(refused.verdicts, 1);
    assert_eq!(refused.refused_no_adjudicator, 1);
    assert_eq!(refused.adjudicated, 0);
    assert_eq!(refused.unadjudicable, 0);
    session.server.shutdown().await;
}

// ── Counting without a sink ──────────────────────────────────────────────

#[tokio::test]
async fn a_gateway_with_no_metrics_sink_still_accumulates_every_counter() {
    // The library has no sink to configure: `--metrics-jsonl` opens a reporter
    // in the binary and nothing else. So a gateway built here is exactly a
    // node started without the flag, and the counters below are what such a
    // node accumulates.
    //
    // What this does *not* prove, because it is not true: that an operator can
    // read them. `persistd` has no scrape or admin surface, so on a real node
    // these are reachable by nothing until the D12 OTel bridge lands. Keeping
    // them warm and correct is the precondition for that bridge, not a
    // substitute for it.
    let peer = support::secret(1);
    let session = connect(
        support::authority_config(peer.public(), GridId::ROOT, vec![CellId::ROOT]),
        &peer,
    )
    .await;
    let metrics = Arc::clone(session.server.metrics());

    // Bulk: seed the entity, claim a lease, then write under its fence.
    seed_entity(&session.runtime, ENTITY).await;
    session
        .conn
        .send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id: ClaimId(1),
                entity: ENTITY,
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                kind: ClaimKind::Weak,
                basis: ClaimBasis::Explicit,
                observed: Default::default(),
                tick: Tick::new(1),
            },
        })
        .await;
    let (lease_id, seq) = loop {
        match session.conn.next_reply(Duration::from_secs(5)).await {
            Some(GatewayReply::Lease {
                message: LeaseMsg::Grant { lease_id, seq, .. },
            }) => break (lease_id, seq),
            Some(GatewayReply::Lease { message }) => panic!("claim was not granted: {message:?}"),
            Some(_) => continue,
            None => panic!("no answer to the lease claim"),
        }
    };
    session.conn.send_state(&GatewayMsg::Diff {
        diff: DiffUplink {
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: ENTITY,
            tick: Tick::new(2),
            kind: RecordKind::ComponentDiff,
            payload: Bytes::from_static(b"state"),
            seq: 2,
            lease_id: Some(lease_id),
            authority_seq: Some(seq),
        },
    });
    assert!(
        matches!(
            session.conn.next_reply(Duration::from_secs(5)).await,
            Some(GatewayReply::BulkAck { .. })
        ),
        "the fenced write must be acknowledged"
    );

    // Area: one subscribe, one page.
    session
        .conn
        .send_control(&GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![CellId::ROOT],
        })
        .await;
    assert!(
        matches!(
            session.conn.next_reply(Duration::from_secs(5)).await,
            Some(GatewayReply::AreaPage { .. })
        ),
        "a subscribed cell must answer with a page"
    );

    // Intent: no executor is configured, so the honest answer is a rejection —
    // which is still one definitive reply, and still one measured span.
    let mut intent = Intent {
        evidence: None,
        intent_id: 71,
        issuer: peer.public(),
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op: 1,
            args: Bytes::from_static(b"metrics"),
        }],
        attestations: Vec::new(),
        signature: peer.sign(b"placeholder"),
    };
    intent.sign(&peer);
    session
        .conn
        .send_control(&GatewayMsg::SubmitIntent { intent })
        .await;
    assert!(matches!(
        session.conn.next_reply(Duration::from_secs(5)).await,
        Some(GatewayReply::IntentAck {
            intent_id: 71,
            outcome: IntentOutcome::Rejected {
                reason: REASON_NO_EXECUTOR
            },
        })
    ));

    // Report: refused, and counted.
    let (_, reason) = file(&session.conn, signed_report(&peer)).await;
    assert_eq!(reason, REPORT_REFUSED_NO_ADJUDICATOR);

    let bulk = metrics.bulk.snapshot();
    assert_eq!(bulk.acknowledgements, 1);
    assert!(bulk.total_us_sum > 0, "the bulk span was measured");

    let intent = metrics.intent.snapshot();
    assert_eq!(intent.replies, 1);
    assert_eq!(intent.rejected, 1);
    assert_eq!(intent.rejected_no_executor, 1);
    assert_eq!(intent.committed, 0);
    assert_eq!(intent.lane_saturated, 0);
    assert_eq!(metrics.intent.latency().total(), 1);

    let area = metrics.area.snapshot();
    assert_eq!(area.subscribes, 1);
    assert_eq!(area.first_pages, 1);
    assert_eq!(area.frames, 1);
    assert_eq!(area.cell_read_errors, 0);
    assert_eq!(metrics.area.latency().total(), 1);

    assert_eq!(metrics.report.snapshot().verdicts, 1);

    session.server.shutdown().await;
}

// ── The binary's artifact ────────────────────────────────────────────────

fn persistd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_persistd")
}

fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback port");
    listener.local_addr().expect("listener address")
}

fn issuer_key_arg() -> String {
    format!("{}@{}", support::ISSUER_KEY_ID, support::issuer().public())
}

/// A session token stamped from the real clock: the binary runs the system
/// clock verifier, not the fixture's fixed one.
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

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Every `sample_batch` record in the artifact so far, as
/// `(series, value_us, count)`.
fn sample_batches(path: &std::path::Path) -> Vec<(String, u64, u64)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(&line).ok()?;
            if value.get("type")?.as_str()? != "sample_batch" {
                return None;
            }
            Some((
                value.get("series")?.as_str()?.to_string(),
                value.get("value_us")?.as_u64()?,
                value.get("count")?.as_u64()?,
            ))
        })
        .collect()
}

#[tokio::test]
async fn a_persistd_run_emits_the_two_server_spans_and_never_a_gated_name() {
    let dir = tempfile::tempdir().expect("temp dir");
    let artifact = dir.path().join("metrics.jsonl");
    let bind = free_loopback_addr();
    let mut child = Command::new(persistd_binary())
        .arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg(bind.to_string())
        .arg("--allow-volatile-leases")
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        // Host the root cell locally so the subscribe reads a live actor
        // rather than a cold tier this run has not configured.
        .arg("--shard")
        .arg(format!("0x{:x}", CellId::ROOT.to_bits()))
        .arg("--metrics-jsonl")
        .arg(&artifact)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn persistd");
    let stdout = child.stdout.take().expect("stdout captured");
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read readiness document");
    let ready: serde_json::Value = serde_json::from_str(line.trim()).expect("readiness JSON");
    let gateway: NodeId = ready["node_id"]
        .as_str()
        .expect("gateway node id")
        .parse()
        .expect("valid gateway node id");

    let client_key = iroh::SecretKey::generate();
    let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(client_key.clone())
        .bind()
        .await
        .expect("client endpoint");
    let conn = client
        .connect(
            iroh::EndpointAddr::new(gateway).with_ip_addr(bind),
            GATEWAY_ALPN,
        )
        .await
        .expect("connect to persistd");
    let mut admission = conn.accept_uni().await.expect("gateway admission");
    assert_eq!(admission.read_to_end(16).await.expect("admission"), vec![0]);
    let conn = lanes::GatewayLanes::attach(conn);
    conn.send_control(&GatewayMsg::VersionedHello {
        token: process_session_token(client_key.public()),
        node: client_key.public(),
        version: orrery_protocol::PROTOCOL_VERSION,
    })
    .await;
    assert!(matches!(
        conn.next_reply(Duration::from_secs(5)).await,
        Some(GatewayReply::HelloAck { .. })
    ));

    conn.send_control(&GatewayMsg::Subscribe {
        grid: GridId::ROOT,
        cells: vec![CellId::ROOT],
    })
    .await;
    assert!(
        matches!(
            conn.next_reply(Duration::from_secs(5)).await,
            Some(GatewayReply::AreaPage { .. })
        ),
        "the hosted root cell must answer with a page"
    );

    let mut intent = Intent {
        evidence: None,
        intent_id: 91,
        issuer: client_key.public(),
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op: 1,
            args: Bytes::from_static(b"server-span"),
        }],
        attestations: Vec::new(),
        signature: client_key.sign(b"placeholder"),
    };
    intent.sign(&client_key);
    conn.send_control(&GatewayMsg::SubmitIntent { intent })
        .await;
    let reply = conn
        .next_payload(Duration::from_secs(5))
        .await
        .expect("intent reply");
    assert!(matches!(
        decode_stream_frame(&reply),
        Some(GatewayReply::IntentAck { intent_id: 91, .. })
    ));

    // The reporter exports on a one-second cadence, so poll rather than sleep
    // a fixed span.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let batches = loop {
        let batches = sample_batches(&artifact);
        let has = |series: &str| batches.iter().any(|(name, ..)| name == series);
        if has(SERIES_GATEWAY_INTENT_SERVER) && has(SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER) {
            break batches;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no server-span batches after 20 s: {batches:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    for series in [
        SERIES_GATEWAY_INTENT_SERVER,
        SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER,
    ] {
        let mine: Vec<_> = batches.iter().filter(|(name, ..)| name == series).collect();
        assert!(!mine.is_empty(), "{series} produced no batch");
        assert_eq!(
            mine.iter().map(|(_, _, count)| count).sum::<u64>(),
            1,
            "one request, one sample: {mine:?}"
        );
        for (_, value_us, _) in &mine {
            // Plausible rather than exact: a bucket upper bound on the shared
            // lattice, and a server span that took a whole second on loopback
            // is a defect whatever the number says.
            assert!(
                orrery_protocol::metrics::LATENCY_BOUNDARIES_US.contains(value_us),
                "{series} reported {value_us} µs, which is not a lattice bound"
            );
            assert!(*value_us <= 1_000_000, "{series} took {value_us} µs");
        }
    }

    // The gated names are the client rig's to produce. persistd writing one
    // would silently deflate the P2 gate's p99, because the kill-9 harness
    // concatenates both files into one fold.
    for (series, ..) in &batches {
        assert!(
            series != orrery_protocol::metrics::SERIES_INTENT_COMMIT
                && series != orrery_protocol::metrics::SERIES_AREA_FIRST_PAGE,
            "persistd minted the gated series {series}"
        );
    }
    // `journal_commit_ms` is the one gated series persistd legitimately owns —
    // it *is* the server-internal measurement D16 targets — and this run
    // commits nothing, so its absence here is the contract working, not a gap.
    assert!(GATED_SERIES.contains(&orrery_protocol::metrics::SERIES_JOURNAL_COMMIT));

    conn.conn().close(0u32.into(), b"test complete");
    client.close().await;
    stop(&mut child);
}
