//! Escalation over the wire: a witness files a signed `DiscrepancyReport` at
//! the gateway and gets a `Verdict` back (docs/07-witnessing.md §3, stages 3
//! and 4).
//!
//! This is the middle of "detected, escalated, replay-adjudicated". The
//! detection half lives in `orrery_witness`; the adjudication half is
//! `AdjudicationExecutor`, which was complete, unit-tested and reachable from
//! nothing. What is proven here is the seam between them: that a report
//! crosses a real gateway connection, reaches a registered rules build, and
//! comes back as a verdict — and that every way it can *fail* to comes back as
//! a stable code rather than as silence.
//!
//! Bevy-free and raw iroh, like the rest of this crate's gateway tests (D15).
//! Signing a report needs nothing else: `orrery_persistd` already depends on
//! `orrery_witness` for `verify_report`, so `sign_report` is right here.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_core::log::{claim_hash, fold, sign_claim, sign_frame, HeadTransition};
use orrery_core::store::AuthorityLog;
use orrery_core::{
    state_hash, CodecError, CoreCodec, Executor, OrderedInputs, Quantized, Ruleset, StateView,
    StepOutput, TickRng,
};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    AdjudicationExecutor, CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore,
    Router, RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::{
    CellId, ChainHash, DiscrepancyReport, EntitySlice, Epoch, EvidenceBundle, GatewayMsg,
    GatewayReply, GridId, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick, UnadjudicableReason, UniverseSeed, Verdict, REPORT_ADJUDICATED,
    REPORT_REFUSED_NO_ADJUDICATOR, REPORT_REFUSED_NO_SESSION, REPORT_REFUSED_RATE_LIMITED,
    REPORT_REFUSED_REPORTER_MISMATCH,
};
use tokio::sync::Mutex;

// ── An honest authority, small enough to fit in a wire test ──────────────
//
// The point here is the transport, not the detection, so the ruleset is the
// smallest thing that still produces a bundle an adjudicator will actually
// replay: real signatures, a real input chain, real per-tick hashes. A stubbed
// bundle would come back `Unadjudicable(Malformed)` and prove only that the
// message arrived.

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tally(u64);

impl CoreCodec for Tally {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self(u64::from_le_bytes(
            bytes.try_into().map_err(|_| CodecError("8 bytes"))?,
        )))
    }
}

impl Quantized for Tally {
    fn quantize(&mut self) {}
}

#[derive(Debug, Clone, Copy)]
struct Bump(u64);

impl CoreCodec for Bump {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self(u64::from_le_bytes(
            bytes.try_into().map_err(|_| CodecError("8 bytes"))?,
        )))
    }
}

struct Nothing;

impl CoreCodec for Nothing {
    fn encode(&self, _out: &mut Vec<u8>) {}
    fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self)
    }
}

const RULESET: RulesetId = RulesetId {
    version: 3,
    digest: [0x33; 32],
};

/// The build nothing registers, for the `UnknownRuleset` case.
const RETIRED_RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0x11; 32],
};

struct Counting;

impl Ruleset for Counting {
    type CoreState = Tally;
    type CoreInput = Bump;
    type CoreEvent = Nothing;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn step(
        &self,
        view: &mut StateView<'_, Tally>,
        inputs: &OrderedInputs<'_, Bump>,
        _rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        let state = view.own_mut();
        for input in inputs.iter() {
            state.0 = state.0.wrapping_add(input.0);
        }
        StepOutput::default()
    }
}

const ENTITY: PersistId = PersistId::new(77);
const SEED: UniverseSeed = UniverseSeed([0x5A; 32]);
const T0: u64 = 1_200;
const SPAN: u64 = 30;

fn subject_key() -> iroh_base::SecretKey {
    support::secret(9)
}

/// One honest window, assembled the way a witness assembles one: an anchor
/// claim with its snapshot, a signed frame covering the window, per-tick
/// hashes, and a closing claim the subject signed.
fn honest_bundle(ruleset: RulesetId) -> EvidenceBundle {
    let key = subject_key();
    let mut executor = Executor::new(Counting, SEED);
    executor.insert(ENTITY, Tally(0));

    let anchor_state = executor.state(ENTITY).expect("seeded").clone();
    let mut anchor = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0),
        input_head: ChainHash::EMPTY,
        state_hash: state_hash(&anchor_state),
        prev_claim: [0; 32],
        ruleset,
        sig: key.sign(b"unsigned"),
    };
    sign_claim(&key, &mut anchor);

    let mut log = AuthorityLog::default();
    log.record_claim(anchor.clone(), anchor_state.to_canonical());

    let mut head = ChainHash::EMPTY;
    let mut records = Vec::with_capacity(SPAN as usize);
    let mut computed = Vec::with_capacity(SPAN as usize);
    for offset in 0..SPAN {
        let tick = T0 + offset;
        let record = InputRecord {
            tick_off: u16::try_from(offset).expect("window fits a u16 offset"),
            seq: 0,
            source: RecordSource::Player {
                node: key.public(),
                input_seq: u32::try_from(tick).expect("tick fits a u32 input seq"),
            },
            payload: Bytes::from(Bump(offset + 1).to_canonical()),
        };
        head = fold(head, &record);
        records.push(record);
        let outcome = executor
            .step_entity(ENTITY, Tick::new(tick), &[Bump(offset + 1)])
            .expect("entity present");
        log.record_tick_hash(ENTITY, Tick::new(tick), outcome.state_hash);
        computed.push(outcome.state_hash);
    }

    let transitions = vec![HeadTransition {
        entity: ENTITY,
        prev_head: ChainHash::EMPTY,
        head,
    }];
    let frame = LogFrame {
        ruleset,
        first_tick: Tick::new(T0),
        tick_count: u16::try_from(SPAN).expect("window fits a u16 tick count"),
        entities: vec![EntitySlice {
            entity: ENTITY,
            chain_epoch: 0,
            prev_head: ChainHash::EMPTY.rolling(),
            records,
            head: head.rolling(),
        }],
        sig: sign_frame(
            &key,
            ruleset,
            Tick::new(T0),
            u16::try_from(SPAN).expect("window fits a u16 tick count"),
            &transitions,
        ),
    };
    log.record_frame(frame, transitions);

    let closing_state = executor.state(ENTITY).expect("entity present").clone();
    let mut closing = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0 + SPAN),
        input_head: head,
        state_hash: state_hash(&closing_state),
        prev_claim: claim_hash(&anchor),
        ruleset,
        sig: key.sign(b"unsigned"),
    };
    sign_claim(&key, &mut closing);
    log.record_claim(closing, closing_state.to_canonical());

    log.assemble_bundle(ENTITY, (Tick::new(T0), Tick::new(T0 + SPAN)), computed)
        .expect("the window is exactly what was just recorded")
}

/// A report signed by `reporter` over an honest window.
fn honest_report(reporter: &iroh_base::SecretKey, ruleset: RulesetId) -> Box<DiscrepancyReport> {
    Box::new(orrery_witness::sign_report(
        reporter,
        subject_key().public(),
        honest_bundle(ruleset),
    ))
}

// ── The live gateway ─────────────────────────────────────────────────────

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

/// A live gateway plus a connected client. Everything is retained: dropping
/// the client endpoint locally-closes the connection.
struct Session {
    server: GatewayServer,
    conn: lanes::GatewayLanes,
    _client: iroh::Endpoint,
    _dir: tempfile::TempDir,
    _runtime: Arc<Mutex<CellRuntime>>,
}

/// Spawn a gateway and dial it as `key`, completing admission but not `Hello`.
async fn dial(config: GatewayConfig, key: &iroh_base::SecretKey) -> Session {
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
    Session {
        server,
        conn: lanes::GatewayLanes::attach(conn),
        _client: client,
        _dir: dir,
        _runtime: runtime,
    }
}

/// Dial and complete `Hello`, so the session has an account to bill.
async fn connect(config: GatewayConfig, key: &iroh_base::SecretKey) -> Session {
    let session = dial(config, key).await;
    session
        .conn
        .send_control(&GatewayMsg::VersionedHello {
            token: support::valid_session_token(key.public()),
            node: key.public(),
            version: orrery_protocol::PROTOCOL_VERSION,
        })
        .await;
    lanes::expect_hello_ack(&session.conn).await;
    session
}

/// How many unrelated replies a single-answer helper will drain before it
/// gives up. A *count*, not a deadline: exceeding it means the gateway is
/// talking and saying the wrong thing, which is a correctness failure and is
/// reported as one.
const UNRELATED_REPLY_BUDGET: usize = 8;

/// File `report` and read back its single answer.
async fn file(
    conn: &lanes::GatewayLanes,
    report: Box<DiscrepancyReport>,
) -> (Option<Verdict>, u16) {
    conn.send_control(&GatewayMsg::Report { report }).await;
    for _ in 0..UNRELATED_REPLY_BUDGET {
        match conn.next_reply(lanes::LIVENESS_CEILING).await {
            Some(GatewayReply::ReportVerdict {
                subject,
                entity,
                window_end,
                verdict,
                reason,
            }) => {
                assert_eq!(
                    subject,
                    subject_key().public(),
                    "the answer names the accused"
                );
                assert_eq!(entity, ENTITY);
                assert_eq!(window_end, Tick::new(T0 + SPAN));
                return (verdict, reason);
            }
            // Some other reply overtook the verdict on the wire; keep draining.
            Some(_) => continue,
            None => panic!(
                "timed out after {} s waiting for the report's verdict; this is \
                 a liveness failure, not evidence that the gateway answered the \
                 report with something other than a verdict",
                lanes::LIVENESS_CEILING.as_secs(),
            ),
        }
    }
    panic!(
        "{UNRELATED_REPLY_BUDGET} replies arrived on this connection and none was a \
         ReportVerdict"
    );
}

fn adjudicating_config(peer: orrery_protocol::NodeId) -> GatewayConfig {
    let mut adjudicator = AdjudicationExecutor::new(SEED);
    adjudicator.register(|| Counting);
    GatewayConfig {
        adjudicator: Some(Arc::new(adjudicator)),
        ..support::authority_config(peer, GridId::ROOT, vec![CellId::ROOT])
    }
}

// ── The tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_signed_report_is_adjudicated_over_the_wire() {
    // The seam this whole lane exists to close: a report leaves a reporter,
    // crosses a real connection, reaches the registered build, and comes back
    // judged. The window is honest, so the honest verdict is `Exonerates` —
    // which is the answer that protects players, and the one a cluster that
    // rubber-stamped accusations could never produce.
    let reporter = support::secret(1);
    let session = connect(adjudicating_config(reporter.public()), &reporter).await;

    let (verdict, reason) = file(&session.conn, honest_report(&reporter, RULESET)).await;
    assert_eq!(reason, REPORT_ADJUDICATED);
    assert_eq!(
        verdict,
        Some(Verdict::Exonerates),
        "the cluster re-ran the evidence and found nothing"
    );

    session.server.shutdown().await;
}

#[tokio::test]
async fn a_report_pinning_an_unretained_build_is_undecidable_not_a_strike() {
    // Rules-version skew is the cluster's gap, not the reporter's. It has to
    // reach the reporter as `Unadjudicable`, because the strike ledger reads
    // this verdict and `Unadjudicable` is the one that weighs nothing.
    let reporter = support::secret(1);
    let session = connect(adjudicating_config(reporter.public()), &reporter).await;

    let (verdict, reason) = file(&session.conn, honest_report(&reporter, RETIRED_RULESET)).await;
    assert_eq!(reason, REPORT_ADJUDICATED);
    assert_eq!(
        verdict,
        Some(Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)),
        "no registered build matches, and saying so is not an accusation"
    );

    session.server.shutdown().await;
}

#[tokio::test]
async fn a_gateway_with_no_adjudicator_refuses_rather_than_going_quiet() {
    // The stock configuration: no `Ruleset` linked, so nothing here can judge
    // evidence. Silence would be indistinguishable from an exoneration, and
    // the two call for opposite responses — so the refusal is explicit and
    // carries a stable code.
    let reporter = support::secret(1);
    let config = support::authority_config(reporter.public(), GridId::ROOT, vec![CellId::ROOT]);
    assert!(
        config.adjudicator.is_none(),
        "the default registers nothing"
    );
    let session = connect(config, &reporter).await;

    let (verdict, reason) = file(&session.conn, honest_report(&reporter, RULESET)).await;
    assert_eq!(reason, REPORT_REFUSED_NO_ADJUDICATOR);
    assert_eq!(verdict, None, "a refusal is not a verdict");

    session.server.shutdown().await;
}

#[tokio::test]
async fn a_report_filed_in_another_peers_name_is_refused() {
    // The binding that makes the per-account limit below mean anything:
    // without it a flooder simply spends somebody else's budget. Same rule
    // intents get from `REASON_ISSUER_MISMATCH`.
    let reporter = support::secret(1);
    let session = connect(adjudicating_config(reporter.public()), &reporter).await;

    let (verdict, reason) = file(&session.conn, honest_report(&support::secret(2), RULESET)).await;
    assert_eq!(reason, REPORT_REFUSED_REPORTER_MISMATCH);
    assert_eq!(verdict, None);

    session.server.shutdown().await;
}

#[tokio::test]
async fn a_report_before_hello_has_no_account_to_bill() {
    // Rate limiting is per account (docs/07 §7), and a connection that has not
    // completed `Hello` has none. Accepting reports there would leave exactly
    // one unmetered path into the adjudicator.
    let reporter = support::secret(1);
    let session = dial(adjudicating_config(reporter.public()), &reporter).await;

    let (verdict, reason) = file(&session.conn, honest_report(&reporter, RULESET)).await;
    assert_eq!(reason, REPORT_REFUSED_NO_SESSION);
    assert_eq!(verdict, None);

    session.server.shutdown().await;
}

#[tokio::test]
async fn a_flood_from_one_account_is_shed_rather_than_struck() {
    // The burst is legitimate — a witness re-anchoring seven watches can find
    // several stale divergences at once — so what is asserted is that the
    // burst is served and the flood past it is refused, with a code that is
    // explicitly not a verdict. A shed report costs the reporter nothing in
    // the strike ledger.
    let reporter = support::secret(1);
    let session = connect(adjudicating_config(reporter.public()), &reporter).await;

    let mut refusals = 0;
    let mut adjudicated = 0;
    for _ in 0..48 {
        let (verdict, reason) = file(&session.conn, honest_report(&reporter, RULESET)).await;
        match reason {
            REPORT_ADJUDICATED => {
                assert!(verdict.is_some());
                adjudicated += 1;
            }
            REPORT_REFUSED_RATE_LIMITED => {
                assert_eq!(verdict, None, "a shed report is not judged");
                refusals += 1;
            }
            other => panic!("unexpected report reason {other}"),
        }
    }
    assert!(
        adjudicated >= 16,
        "the documented burst must be served: {adjudicated} adjudicated"
    );
    assert!(
        refusals > 0,
        "a 48-report flood must be shed somewhere: {refusals} refused"
    );

    session.server.shutdown().await;
}
