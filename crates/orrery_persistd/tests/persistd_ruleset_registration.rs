//! The deployed-binary proof for the Ruleset registration seam (#880).
//!
//! The library already tests an in-process gateway with a registered worker.
//! This test deliberately starts the `persistd` binary compiled with the
//! `reference-ruleset` feature: it proves the composition root actually
//! constructs, registers, and installs that worker before accepting reports.

#![cfg(feature = "reference-ruleset")]

mod lanes;
mod support;

use std::io::BufRead;
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};

use bytes::Bytes;
use iroh::RelayMode;
use orrery_conformance::REFERENCE_RULESET;
use orrery_protocol::{
    CellId, ChainHash, DiscrepancyReport, EvidenceBundle, GatewayMsg, GatewayReply, NodeId,
    PersistId, RulesetId, StateClaim, Tick, UnadjudicableReason, Verdict, REPORT_ADJUDICATED,
    REPORT_REFUSED_NO_ADJUDICATOR,
};

const ENTITY: PersistId = PersistId::new(880);
const UNREGISTERED_RULESET: RulesetId = RulesetId {
    version: u32::MAX,
    digest: [0x88; 32],
};
const TEST_UNIVERSE_SEED: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback port");
    listener.local_addr().expect("listener address")
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

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn thin_bundle(ruleset: RulesetId) -> EvidenceBundle {
    let subject = support::secret(2);
    EvidenceBundle {
        ruleset,
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
            ruleset,
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

fn signed_report(reporter: &iroh_base::SecretKey, ruleset: RulesetId) -> Box<DiscrepancyReport> {
    Box::new(orrery_witness::sign_report(
        reporter,
        support::node(2),
        thin_bundle(ruleset),
    ))
}

async fn file(
    conn: &lanes::GatewayLanes,
    report: Box<DiscrepancyReport>,
) -> (Option<Verdict>, u16) {
    conn.send_control(&GatewayMsg::Report { report }).await;
    for _ in 0..8 {
        match conn.next_reply(lanes::LIVENESS_CEILING).await {
            Some(GatewayReply::ReportVerdict {
                verdict, reason, ..
            }) => return (verdict, reason),
            Some(_) => continue,
            None => panic!("the binary did not return a report verdict"),
        }
    }
    panic!("the binary sent eight replies without a report verdict")
}

#[tokio::test]
async fn reference_ruleset_binary_adjudicates_registered_builds_and_marks_unknown_builds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bind = free_loopback_addr();
    let mut child = Command::new(env!("CARGO_BIN_EXE_persistd"))
        .arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg(bind.to_string())
        .arg("--allow-volatile-leases")
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        .arg("--shard")
        .arg(format!("0x{:x}", CellId::ROOT.to_bits()))
        .arg("--universe-seed")
        .arg(TEST_UNIVERSE_SEED)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn feature-enabled persistd");

    let stdout = child.stdout.take().expect("stdout captured");
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read readiness document");
    let ready: serde_json::Value = serde_json::from_str(line.trim()).expect("readiness JSON");
    assert_eq!(
        ready["adjudicator"], true,
        "the compiled Ruleset is installed"
    );
    assert_eq!(
        ready["adjudicator_rulesets"],
        serde_json::to_value([REFERENCE_RULESET]).expect("RulesetId is JSON"),
        "the binary's executor retained the build it linked"
    );
    let gateway: NodeId = ready["node_id"]
        .as_str()
        .expect("gateway node id")
        .parse()
        .expect("valid gateway node id");

    let client_key = iroh::SecretKey::generate();
    let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![orrery_persistd::GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(client_key.clone())
        .bind()
        .await
        .expect("client endpoint");
    let conn = client
        .connect(
            iroh::EndpointAddr::new(gateway).with_ip_addr(bind),
            orrery_persistd::GATEWAY_ALPN,
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
    lanes::expect_hello_ack(&conn).await;

    let (verdict, reason) = file(&conn, signed_report(&client_key, REFERENCE_RULESET)).await;
    assert_eq!(reason, REPORT_ADJUDICATED);
    assert_ne!(
        reason, REPORT_REFUSED_NO_ADJUDICATOR,
        "the registered build reaches the executor through the binary gateway"
    );
    assert!(
        verdict.is_some(),
        "the registered Reference worker, not the no-adjudicator refusal path, judged the bundle"
    );

    let (verdict, reason) = file(&conn, signed_report(&client_key, UNREGISTERED_RULESET)).await;
    assert_eq!(reason, REPORT_ADJUDICATED);
    assert_eq!(
        verdict,
        Some(Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)),
        "an unregistered build reaches the executor, files no strike, and is not refused"
    );

    conn.conn().close(0u32.into(), b"test complete");
    client.close().await;
    stop(&mut child);
}
