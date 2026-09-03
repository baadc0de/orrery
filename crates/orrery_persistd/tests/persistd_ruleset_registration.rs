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
#[cfg(feature = "fdb")]
use orrery_conformance::Body;
use orrery_conformance::REFERENCE_RULESET;
#[cfg(feature = "fdb")]
use orrery_core::log::{claim_hash, sign_claim};
#[cfg(feature = "fdb")]
use orrery_core::{state_hash, CoreCodec};
#[cfg(feature = "fdb")]
use orrery_persistd::adjudication::{
    strike_account_range_end, strike_account_range_start, FdbStrikeLedger, StrikeMode, StrikeRow,
};
#[cfg(feature = "fdb")]
use orrery_persistd::FdbContext;
#[cfg(feature = "fdb")]
use orrery_protocol::{AccountId, GridId};
use orrery_protocol::{
    CellId, ChainHash, DiscrepancyReport, EntitySlice, EvidenceBundle, GatewayMsg, GatewayReply,
    LogFrame, NodeId, PersistId, RulesetId, StateClaim, Tick, UnadjudicableReason, Verdict,
    REPORT_ADJUDICATED, REPORT_REFUSED_NO_ADJUDICATOR,
};

const ENTITY: PersistId = PersistId::new(880);
const UNREGISTERED_RULESET: RulesetId = RulesetId {
    version: u32::MAX,
    digest: [0x88; 32],
};
const TEST_UNIVERSE_SEED: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// Serializes the tests in this file, because one of the things they share is
/// not theirs to partition.
///
/// `content/version` is a single global key (`[b'v']`), so the #947 fixture's
/// sealed row is visible to every other `persistd` this binary starts — and
/// since #947 a seal that contradicts the seed a process was handed is exactly
/// what makes that process refuse. Run in parallel, the fixture therefore
/// fails its neighbours rather than itself, which is the least useful shape a
/// test failure can take. There is no unique-id trick available: the key is a
/// single byte with no room for one.
static CLUSTER_FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// One signed, record-free frame spanning the guilty bundle's window.
#[cfg(feature = "fdb")]
fn covering_frame(subject: &iroh_base::SecretKey) -> LogFrame {
    let slice = EntitySlice {
        entity: ENTITY,
        chain_epoch: 0,
        prev_head: ChainHash::EMPTY.rolling(),
        records: Vec::new(),
        head: ChainHash::EMPTY.rolling(),
    };
    let transitions = vec![orrery_core::log::HeadTransition {
        entity: ENTITY,
        prev_head: ChainHash::EMPTY,
        head: ChainHash::EMPTY,
    }];
    let preimage =
        orrery_core::log::frame_preimage(REFERENCE_RULESET, Tick::new(10), 1, &transitions);
    LogFrame {
        ruleset: REFERENCE_RULESET,
        first_tick: Tick::new(10),
        tick_count: 1,
        entities: vec![slice],
        sig: subject.sign(&preimage),
    }
}

/// A valid one-tick Reference bundle whose closing signed claim lies about the
/// resulting state. Unlike [`thin_bundle`], this reaches `Verdict::Confirms`
/// and therefore exercises the deployed strike side effect.
#[cfg(feature = "fdb")]
fn guilty_report(reporter: &iroh_base::SecretKey) -> Box<DiscrepancyReport> {
    let subject = support::secret(2);
    let state = Body {
        pos: Default::default(),
        vel: Default::default(),
        heading_urad: 0,
        hp: 100,
        shield: 25,
        roll_fold: 0,
    };
    let mut anchor = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(10),
        input_head: ChainHash::EMPTY,
        state_hash: state_hash(&state),
        prev_claim: [0; 32],
        ruleset: REFERENCE_RULESET,
        sig: subject.sign(b"placeholder"),
    };
    sign_claim(&subject, &mut anchor);
    let mut disputed = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(11),
        input_head: ChainHash::EMPTY,
        state_hash: [0x86; 32],
        prev_claim: claim_hash(&anchor),
        ruleset: REFERENCE_RULESET,
        sig: subject.sign(b"placeholder"),
    };
    sign_claim(&subject, &mut disputed);
    let bundle = EvidenceBundle {
        ruleset: REFERENCE_RULESET,
        entity: ENTITY,
        window_start: Tick::new(10),
        window_end: Tick::new(11),
        t0_claim: anchor,
        t0_snapshot: Bytes::from(state.to_canonical()),
        // The window's input frames, which a bundle must carry in full: an
        // adjudicator that accepted a window with frames missing would convict
        // on ticks it could not see the inputs for (#874).
        frames: vec![covering_frame(&subject)],
        sibling_heads: vec![Vec::new()],
        disputed_claims: vec![disputed],
        claimed_hashes: Vec::new(),
        computed_hashes: Vec::new(),
    };
    Box::new(orrery_witness::sign_report(
        reporter,
        subject.public(),
        bundle,
    ))
}

#[cfg(feature = "fdb")]
fn strike_episode_range(account: AccountId) -> (Vec<u8>, Vec<u8>) {
    let mut start = b"yb".to_vec();
    start.extend_from_slice(&account.0.to_be_bytes());
    let mut end = b"yb".to_vec();
    end.extend_from_slice(
        &account
            .0
            .checked_add(1)
            .expect("test account has a successor")
            .to_be_bytes(),
    );
    (start, end)
}

#[cfg(feature = "fdb")]
async fn prepare_strike_account(context: &FdbContext, account: AccountId) {
    let db = context.database();
    let binding_key = orrery_persistd::keyspace::binding_key(&support::node(2));
    let binding = postcard::to_stdvec(&orrery_persistd::keyspace::BindingRow {
        account,
        bound_at_ms: 1,
    })
    .expect("binding encodes");
    let strike_start = strike_account_range_start(account);
    let strike_end = strike_account_range_end(account);
    let (episode_start, episode_end) = strike_episode_range(account);
    db.run(|trx, _| {
        let binding = binding.clone();
        let strike_start = strike_start.clone();
        let strike_end = strike_end.clone();
        let episode_start = episode_start.clone();
        let episode_end = episode_end.clone();
        async move {
            trx.clear_range(&strike_start, &strike_end);
            trx.clear_range(&episode_start, &episode_end);
            trx.set(&binding_key, &binding);
            Ok(())
        }
    })
    .await
    .expect("prepare strike account");
}

#[cfg(feature = "fdb")]
async fn clean_strike_account(context: &FdbContext, account: AccountId, shard: CellId) {
    let db = context.database();
    let binding_key = orrery_persistd::keyspace::binding_key(&support::node(2));
    let fence_key = orrery_persistd::keyspace::fence_key(GridId::ROOT, shard);
    let strike_start = strike_account_range_start(account);
    let strike_end = strike_account_range_end(account);
    let (episode_start, episode_end) = strike_episode_range(account);
    db.run(|trx, _| {
        let strike_start = strike_start.clone();
        let strike_end = strike_end.clone();
        let episode_start = episode_start.clone();
        let episode_end = episode_end.clone();
        async move {
            trx.clear(&binding_key);
            trx.clear(&fence_key);
            trx.clear_range(&strike_start, &strike_end);
            trx.clear_range(&episode_start, &episode_end);
            Ok(())
        }
    })
    .await
    .expect("clean strike fixture");
}

#[cfg(feature = "fdb")]
async fn run_strike_mode(
    cluster: &str,
    context: &FdbContext,
    account: AccountId,
    mode: Option<&str>,
) -> Vec<StrikeRow> {
    prepare_strike_account(context, account).await;
    let shard = CellId::ROOT.children()[7];
    let dir = tempfile::tempdir().expect("temp dir");
    let bind = free_loopback_addr();
    let mut command = Command::new(env!("CARGO_BIN_EXE_persistd"));
    command
        .arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg(bind.to_string())
        .arg("--node-id")
        .arg("862")
        .arg("--allow-volatile-leases")
        .arg("--fdb-cluster-file")
        .arg(cluster)
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        .arg("--shard")
        .arg(format!("0x{:x}", shard.to_bits()))
        .arg("--universe-seed")
        .arg(TEST_UNIVERSE_SEED)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(mode) = mode {
        command.arg("--strikes").arg(mode);
    }
    let mut child = command.spawn().expect("spawn FDB-backed persistd");

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

    let reporter = iroh::SecretKey::generate();
    let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![orrery_persistd::GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(reporter.clone())
        .bind()
        .await
        .expect("client endpoint");
    let raw = client
        .connect(
            iroh::EndpointAddr::new(gateway).with_ip_addr(bind),
            orrery_persistd::GATEWAY_ALPN,
        )
        .await
        .expect("connect to persistd");
    let mut admission = raw.accept_uni().await.expect("gateway admission");
    assert_eq!(admission.read_to_end(16).await.expect("admission"), vec![0]);
    let conn = lanes::GatewayLanes::attach(raw);
    conn.send_control(&GatewayMsg::VersionedHello {
        token: process_session_token(reporter.public()),
        node: reporter.public(),
        version: orrery_protocol::PROTOCOL_VERSION,
    })
    .await;
    lanes::expect_hello_ack(&conn).await;

    let (verdict, reason) = file(&conn, guilty_report(&reporter)).await;
    assert_eq!(reason, REPORT_ADJUDICATED);
    assert!(
        matches!(verdict, Some(Verdict::Confirms { .. })),
        "the deployed Reference worker must produce the guilty verdict that triggers filing: {verdict:?}"
    );

    conn.conn().close(0u32.into(), b"test complete");
    client.close().await;
    stop(&mut child);

    let ledger = FdbStrikeLedger::from_database(context.database());
    let rows = ledger.rows(account).await.expect("read filed strike rows");
    clean_strike_account(context, account, shard).await;
    rows
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
    let _serial = CLUSTER_FIXTURE.lock().await;
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

/// This is intentionally one named test: the mutation proof removes the
/// composition-root `with_strike_ledger` call and must fail this exact binary
/// path rather than merely exercising the library filer again.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn reference_ruleset_binary_strike_modes_reach_the_durable_ledger() {
    let _serial = CLUSTER_FIXTURE.lock().await;
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file opens");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos() as u64;
    let base = 0x0862_0000_0000_0000 | (nonce & 0x0000_ffff_ffff_fff0);

    let off = run_strike_mode(&cluster, &context, AccountId::new(base), None).await;
    assert!(
        off.is_empty(),
        "the default --strikes off deployment must install no filer"
    );

    let shadow =
        run_strike_mode(&cluster, &context, AccountId::new(base + 1), Some("shadow")).await;
    assert_eq!(shadow.len(), 1, "shadow must persist one durable ya row");
    assert_eq!(shadow[0].mode, StrikeMode::Shadow);
    assert_eq!(
        shadow
            .iter()
            .filter(|row| row.mode == StrikeMode::Live)
            .map(|row| i64::from(row.weight_milli))
            .sum::<i64>(),
        0,
        "D33 standing counts live rows only, so a shadow filing changes no standing"
    );

    let live = run_strike_mode(&cluster, &context, AccountId::new(base + 2), Some("live")).await;
    assert_eq!(live.len(), 1, "live must persist one durable ya row");
    assert_eq!(live[0].mode, StrikeMode::Live);
    assert_eq!(live[0].weight_milli, 3_000);
}

// ---------------------------------------------------------------------------
// #947: a seed that contradicts durable state must refuse, not adjudicate
// ---------------------------------------------------------------------------

/// The seed the fixture world is sealed to. Deliberately not
/// [`TEST_UNIVERSE_SEED`]: the defect being fixed is a *mistyped* seed, so the
/// two must differ.
#[cfg(feature = "fdb")]
const SEALED_UNIVERSE_SEED: &str =
    "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

#[cfg(feature = "fdb")]
fn seed_of(hex: &str) -> orrery_protocol::UniverseSeed {
    let mut seed = [0u8; 32];
    for (i, byte) in seed.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex seed");
    }
    orrery_protocol::UniverseSeed(seed)
}

/// Read `content/version` so the fixture can put it back.
///
/// The row is a single global key (`v`), so this test cannot isolate itself
/// with a unique id the way the strike fixtures do. It saves what it found and
/// restores it, which keeps a concurrent seeder's world stamp intact.
#[cfg(feature = "fdb")]
async fn take_content_version(context: &FdbContext) -> Option<Vec<u8>> {
    context
        .database()
        .run(|trx, _| async move {
            trx.get(&orrery_persistd::keyspace::content_version_key(), false)
                .await
                .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))
        })
        .await
        .expect("read content/version")
        .map(|v| v.to_vec())
}

#[cfg(feature = "fdb")]
async fn put_content_version(context: &FdbContext, value: Option<Vec<u8>>) {
    context
        .database()
        .run(|trx, _| {
            let value = value.clone();
            async move {
                match value {
                    Some(bytes) => {
                        trx.set(&orrery_persistd::keyspace::content_version_key(), &bytes)
                    }
                    None => trx.clear(&orrery_persistd::keyspace::content_version_key()),
                }
                Ok(())
            }
        })
        .await
        .expect("write content/version");
}

#[cfg(feature = "fdb")]
fn sealed_row(fingerprint: Option<orrery_protocol::UniverseSeedFingerprint>) -> Vec<u8> {
    orrery_persistd::content_version::encode(&orrery_persistd::ContentVersion {
        content_build: "seed-fingerprint-fixture-947".to_string(),
        manifest_digest: "0".repeat(64),
        scenario_seed: "947".to_string(),
        config_digest: "0".repeat(64),
        toolchain: "rustc 1.96.0".to_string(),
        seeded_at_ms: 947,
        universe_seed_fingerprint: fingerprint,
    })
    .expect("encode fixture content/version")
}

/// Start the binary against `cluster` with `seed` and report
/// `(exit succeeded, readiness line if any, stderr)`.
#[cfg(feature = "fdb")]
fn start_with_seed(cluster: &str, seed: &str) -> (bool, String, String) {
    use std::io::Read;

    let dir = tempfile::tempdir().expect("temp dir");
    let bind = free_loopback_addr();
    let mut child = Command::new(env!("CARGO_BIN_EXE_persistd"))
        .arg("--dir")
        .arg(dir.path())
        .arg("--bind")
        .arg(bind.to_string())
        .arg("--node-id")
        .arg("947")
        .arg("--allow-volatile-leases")
        .arg("--fdb-cluster-file")
        .arg(cluster)
        .arg("--issuer-key")
        .arg(issuer_key_arg())
        // A shard of this fixture's own: `ROOT` is durably owned by another
        // test's node, and startup would be rejected on activation before it
        // ever reached the adjudicator this test is about.
        .arg("--shard")
        .arg(format!("0x{:x}", CellId::ROOT.children()[3].to_bits()))
        .arg("--universe-seed")
        .arg(seed)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn persistd");

    let mut stdout = child.stdout.take().expect("stdout captured");
    let mut stderr = child.stderr.take().expect("stderr captured");
    // A refusing process exits, so a blocking read to EOF terminates. A
    // started process prints its readiness line and then runs forever, so read
    // exactly that line and kill it.
    let mut line = String::new();
    let read = std::io::BufReader::new(&mut stdout).read_line(&mut line);
    if read.is_ok() && !line.trim().is_empty() {
        let mut text = String::new();
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr.read_to_string(&mut text);
        return (true, line, text);
    }
    let status = child.wait().expect("wait for persistd");
    let mut text = String::new();
    let _ = stderr.read_to_string(&mut text);
    (status.success(), line, text)
}

/// #947: `--universe-seed` is a typed deployment input, and a mistyped one
/// used to be undetectable — the adjudicator replayed every disputed window
/// against the wrong keyed RNG stream and answered confidently about a world
/// that never existed.
///
/// The mutation this test exists for is the removal of
/// `check_universe_seed_against_cluster` from `configured_adjudicator`:
/// without it the contradicting process prints `"adjudicator": true` and
/// serves.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn a_seed_contradicting_the_sealed_universe_refuses_instead_of_adjudicating() {
    let _serial = CLUSTER_FIXTURE.lock().await;
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file opens");
    let saved = take_content_version(&context).await;

    let sealed_to = seed_of(SEALED_UNIVERSE_SEED).fingerprint();
    put_content_version(&context, Some(sealed_row(Some(sealed_to)))).await;

    // 1. The wrong seed: refuse before the readiness line exists.
    let (ok, readiness, stderr) = start_with_seed(&cluster, TEST_UNIVERSE_SEED);
    let mismatch_report = (ok, readiness.clone(), stderr.clone());

    // 2. The right seed: the same cluster, the same row, and it serves.
    let (matched_ok, matched_readiness, _) = start_with_seed(&cluster, SEALED_UNIVERSE_SEED);

    // 3. No fingerprint on the row: unsealed worlds must not be bricked.
    put_content_version(&context, Some(sealed_row(None))).await;
    let (unsealed_ok, unsealed_readiness, _) = start_with_seed(&cluster, TEST_UNIVERSE_SEED);

    // 4. No row at all: the same, for a cluster that was never seeded.
    put_content_version(&context, None).await;
    let (absent_ok, absent_readiness, _) = start_with_seed(&cluster, TEST_UNIVERSE_SEED);

    put_content_version(&context, saved).await;

    let (ok, readiness, stderr) = mismatch_report;
    assert!(
        !ok,
        "a persistd whose seed contradicts the sealed universe must exit non-zero, \
         not serve; readiness was {readiness:?}"
    );
    assert!(
        !readiness.contains("\"adjudicator\": true") && !readiness.contains("\"adjudicator\":true"),
        "the refusal must land before the readiness line: {readiness:?}"
    );
    assert!(
        stderr.contains("--universe-seed") && stderr.contains("content/version"),
        "the refusal must name the flag and the row: {stderr}"
    );
    assert!(
        stderr.contains(&sealed_to.to_hex())
            && stderr.contains(&seed_of(TEST_UNIVERSE_SEED).fingerprint().to_hex()),
        "the refusal must name both values so an operator can tell a typo from a \
         wrong cluster: {stderr}"
    );
    assert!(
        !stderr.contains(SEALED_UNIVERSE_SEED) && !stderr.contains(TEST_UNIVERSE_SEED),
        "no seed may appear in the refusal: it is a secret, and the fingerprint is \
         the published name: {stderr}"
    );

    assert!(
        matched_ok && matched_readiness.contains("\"adjudicator\":true")
            || matched_readiness.contains("\"adjudicator\": true"),
        "the seed the world is sealed to must start normally: {matched_readiness:?}"
    );
    assert!(
        unsealed_ok,
        "a row with no fingerprint is unsealed, not contradicted — warn and proceed: \
         {unsealed_readiness:?}"
    );
    assert!(
        absent_ok,
        "an absent content/version row must not brick a cluster: {absent_readiness:?}"
    );
}
