//! The intent path's stage decomposition attributes time to the stage that
//! spent it, and its two denominators mean what they say.
//!
//! `intent_commit_ms` failed D16 at p99 with a p50 inside budget and nothing
//! able to say which of the eight waits on the path produced the difference.
//! `crate::intent::stages` splits it. A split is only worth having if the
//! numbers land in the right buckets, and *that* is what this file guards:
//!
//! 1. **Executor time is billed to `exec_us`, not to the unattributed gap.** A
//!    deliberately slow executor must move `exec_us` by its whole sleep and
//!    leave `server_gap_us` — the residual no stage claims — near zero. If the
//!    `stages::timed` wrapper around `IntentExecutor::execute` is removed, the
//!    same time reappears in the gap and this test fails. That is the point of
//!    the instrument: an unattributed remainder is visible as a remainder.
//! 2. **The per-intent identity holds.** `admit + spawn_wait + exec + gap`
//!    reconstructs `server_us` exactly, so no stage can silently overlap
//!    another and nothing can hide between two of them.
//! 3. **`intents` and `executed` are different denominators.** An intent
//!    refused before the executor increments the first and not the second, so
//!    dividing an FDB stage by `intents` would understate it — the same class
//!    of error that made `JournalStageSnapshot` read ~30x low.
//! 4. **The `slow` accumulator holds the tail and only the tail**, because a
//!    mean over every intent cannot answer a question about a p99.
//!
//! Run in one test function against one gateway: the metrics are
//! process-global (like `RouteStageMetrics`, and for the same reason), so two
//! `#[tokio::test]`s in one binary would race for the same counters.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::intent::stages::{intent_stage_metrics, slow_threshold_us};
use orrery_persistd::intent::{IntentError, IntentExecutor};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router, RuntimeConfig,
    GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::{
    CellEpoch, CellId, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
    PersistId, Tick,
};
use tokio::sync::Mutex;

/// An executor that takes a known, controllable amount of wall time.
///
/// The sleep is what makes the attribution falsifiable: the test knows exactly
/// how many microseconds the executor owns, so a stage split that bills them
/// anywhere else is detectable rather than merely unproven.
struct SlowExecutor {
    delay: Duration,
}

#[async_trait::async_trait]
impl IntentExecutor for SlowExecutor {
    async fn execute(&self, _intent: &Intent) -> Result<IntentOutcome, IntentError> {
        tokio::time::sleep(self.delay).await;
        Ok(IntentOutcome::Committed {
            tick: Tick::new(1),
            minted: vec![PersistId::new(1)],
        })
    }
}

fn secret(n: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed)
}

fn signed_intent(id: u128, key: &iroh_base::SecretKey) -> Intent {
    let mut intent = Intent {
        evidence: None,
        intent_id: id,
        issuer: key.public(),
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op: 1,
            args: Bytes::from_static(b"stage"),
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
            assert_eq!(got, intent_id);
            return outcome;
        }
    }
    panic!("no IntentAck after 8 inbound messages");
}

#[tokio::test]
async fn stages_attribute_the_time_they_spent_and_the_gap_is_visible() {
    // Comfortably above the 20 ms slow cut, and long enough that scheduling
    // noise on a loaded CI box cannot account for it.
    let delay = Duration::from_millis(slow_threshold_us() / 1000 + 40);
    let key = secret(3);

    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new({
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        CellRuntime::open(&runtime_config(dir.path()), &store)
            .await
            .unwrap()
    }));
    let router: Arc<dyn Router> = runtime.clone();
    let config = GatewayConfig {
        executor: Some(Arc::new(SlowExecutor { delay })),
        ..support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT])
    };
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
    let mut admission = conn.accept_uni().await.unwrap();
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0u8]);
    let conn = lanes::GatewayLanes::attach(conn);
    conn.send_control(&GatewayMsg::Hello {
        token: support::valid_session_token(key.public()),
        node: key.public(),
    })
    .await;
    assert!(matches!(
        conn.next_reply(Duration::from_secs(5)).await,
        Some(GatewayReply::HelloAck { .. })
    ));

    let metrics = intent_stage_metrics();
    let all_before = metrics.all.snapshot();
    let slow_before = metrics.slow.snapshot();

    // ── One intent that reaches the executor ──────────────────────────────
    assert!(matches!(
        submit(&conn, signed_intent(1, &key)).await,
        IntentOutcome::Committed { .. }
    ));

    let all = metrics.all.snapshot().delta(all_before);
    assert_eq!(all.intents, 1, "one reply, one stage sample");
    assert_eq!(all.executed, 1, "it reached the executor");

    let delay_us = delay.as_micros() as u64;
    // Claim 1: the executor's own time is billed to `exec_us`.
    assert!(
        all.exec_us_sum >= delay_us,
        "the executor slept {delay_us} us but exec_us_sum is {}: executor time is \
         not being attributed to the exec stage",
        all.exec_us_sum
    );
    // ...and therefore is NOT sitting in the unattributed remainder. The
    // budget is generous because the gap legitimately holds the reply encode
    // and any scheduler hop after the executor returns; what it must not hold
    // is the sleep.
    assert!(
        all.server_gap_us_sum < delay_us / 2,
        "server_gap_us_sum is {} us against a {delay_us} us executor: the exec stage \
         is not claiming the time it spent, so the gap has absorbed it",
        all.server_gap_us_sum
    );

    // Claim 2: the stages reconstruct the span they decompose, exactly.
    let exemplar = metrics.take_exemplar().expect("an intent was replied to");
    assert_eq!(
        exemplar.admit_us + exemplar.spawn_wait_us + exemplar.exec_us + exemplar.server_gap_us(),
        exemplar.server_us,
        "the per-intent stage identity must hold exactly: {exemplar:?}"
    );
    assert!(
        exemplar.exec_us >= delay_us,
        "the exemplar is the slowest intent and must carry the slow executor: {exemplar:?}"
    );

    // Claim 4: the tail accumulator caught it, because it exceeded the cut.
    let slow = metrics.slow.snapshot().delta(slow_before);
    assert_eq!(
        slow.intents,
        1,
        "a {delay_us} us intent is over the {} us slow cut and must be in the tail view",
        slow_threshold_us()
    );

    // ── One intent refused before the executor ────────────────────────────
    // Its signature is invalid, so `admit_intent` refuses it on the receive
    // loop and it never reaches an executor.
    let all_mid = metrics.all.snapshot();
    let slow_mid = metrics.slow.snapshot();
    let mut unsigned = signed_intent(2, &key);
    unsigned.signature = key.sign(b"not the canonical bytes");
    assert!(matches!(
        submit(&conn, unsigned).await,
        IntentOutcome::Rejected { .. }
    ));

    let refused = metrics.all.snapshot().delta(all_mid);
    // Claim 3: the two denominators are not the same number.
    assert_eq!(refused.intents, 1, "a refusal is still a reply");
    assert_eq!(
        refused.executed, 0,
        "a refusal never reached the executor, so it must not count in the \
         denominator the FDB stages divide by"
    );
    assert_eq!(refused.exec_us_sum, 0, "a refusal has no execution time");
    // Claim 4, the other half: a fast intent stays out of the tail view.
    let slow_after = metrics.slow.snapshot().delta(slow_mid);
    assert_eq!(
        slow_after.intents, 0,
        "an admission refusal is microseconds and must not enter the tail view"
    );

    drop(conn);
    drop(client);
    server.shutdown().await;
}
