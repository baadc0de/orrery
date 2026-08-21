//! An issuer may not witness its own intent, and the refusal happens before
//! the executor — the only thing on this path that reads durable state — is
//! ever reached.
//!
//! D10 item 4 seeds the witness set "excluding **all parties to the intent**";
//! `docs/07-witnessing.md` §4.2 makes the gateway enforce it independently of
//! whoever selected the set. `crates/orrery_persistd/src/intent/mod.rs` holds
//! the unit-level coverage of the rule itself. What *this* file pins is the
//! part a unit test on a pure function cannot: the position of the check in
//! the real path.
//!
//! The instrument is a tripwire executor. `IntentExecutor::execute` is the
//! sole entry to the durable rows (the `intent/{intent_id}` idempotency read
//! is its first act), so an executor that records every call answers "did this
//! refusal cost a durable read?" without the test having to model the ordering
//! itself. A refactor that moved the party check behind the executor — or that
//! let a self-witnessed intent through to it at all — flips the tripwire and
//! fails here, which is exactly the regression #162 exists to prevent once
//! K-of-N enforcement (#147) makes attestations load-bearing.
//!
//! Two arms, because "refuses self-witnessing" is otherwise satisfied by a
//! gateway that refuses everything: the self-witnessed intent must be refused
//! with `REASON_SELF_WITNESS` and leave the tripwire cold, and the
//! independently witnessed one must commit through the same gateway, in the
//! same connection, with the tripwire fired.

mod lanes;
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::intent::{BaselineIntentValidator, IntentError, IntentExecutor};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router, RuntimeConfig,
    GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::{
    CellEpoch, CellId, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
    PersistId, Tick, REASON_SELF_WITNESS,
};
use tokio::sync::Mutex;

/// An executor that records that it ran.
///
/// It stands in for "any durable read at all": the real executors open their
/// transaction with the `intent/{intent_id}` idempotency read before doing
/// anything else, so reaching `execute` and reaching FDB are the same event
/// from the admission path's point of view.
struct Tripwire {
    executions: AtomicU64,
}

#[async_trait::async_trait]
impl IntentExecutor for Tripwire {
    async fn execute(&self, _intent: &Intent) -> Result<IntentOutcome, IntentError> {
        self.executions.fetch_add(1, Ordering::Relaxed);
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

/// A signed intent carrying one `Ruleset`-opaque op, so the baseline validator
/// has nothing to object to but the attestations.
fn signed_intent(id: u128, key: &iroh_base::SecretKey) -> Intent {
    let mut intent = Intent {
        evidence: None,
        intent_id: id,
        issuer: key.public(),
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op: 57_019,
            args: Bytes::from_static(b"trade"),
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
async fn a_self_witnessed_intent_is_refused_before_the_executor_is_reached() {
    let key = secret(3);
    let tripwire = Arc::new(Tripwire {
        executions: AtomicU64::new(0),
    });

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
        executor: Some(tripwire.clone()),
        // The library default is `PermissiveValidator`, which admits
        // everything; the check under test lives in the baseline validator a
        // deployed node runs, so the test must configure it or assert nothing.
        validator: Arc::new(BaselineIntentValidator::permissive()),
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

    // ── Arm 1: the issuer witnesses itself ────────────────────────────────
    //
    // The attestation is a *correct* co-signature made by the issuer over D27
    // clause (a)'s witness preimage. It is deliberately not the copied issuer
    // signature this arm used before the preimage switch: that variant now
    // fails as `BadAttestation` and would prove only that the domain tag
    // works. A domain tag cannot stop an issuer from correctly signing the
    // witness preimage too, which is exactly why the party check is a separate
    // rule — and this arm is what holds it to that.
    let mut self_witnessed = signed_intent(1, &key);
    let self_attestation = self_witnessed.attest(&key);
    assert!(
        self_attestation.verify(&self_witnessed),
        "arm 1 must present a co-signature that verifies, or it proves nothing \
         beyond the signature check"
    );
    self_witnessed.attestations.push(self_attestation);
    assert_eq!(
        submit(&conn, self_witnessed).await,
        IntentOutcome::Rejected {
            reason: REASON_SELF_WITNESS
        },
        "an issuer must not witness its own intent, and the refusal must say so \
         on the wire rather than as an opaque validation failure"
    );
    assert_eq!(
        tripwire.executions.load(Ordering::Relaxed),
        0,
        "the refusal must precede every durable read: the executor is the only \
         thing on this path that opens a transaction, and it was reached"
    );

    // ── Arm 2: a genuinely independent witness ────────────────────────────
    //
    // Same gateway, same connection, same op — only the witness identity
    // differs. Without this arm the assertion above is satisfied by a gateway
    // that refuses every attested intent.
    let witness = secret(9);
    assert_ne!(witness.public(), key.public());
    let mut attested = signed_intent(2, &key);
    let attestation = attested.attest(&witness);
    attested.attestations.push(attestation);
    assert!(
        matches!(
            submit(&conn, attested).await,
            IntentOutcome::Committed { .. }
        ),
        "an independent co-signature is what D10 asks for and must be admitted"
    );
    assert_eq!(
        tripwire.executions.load(Ordering::Relaxed),
        1,
        "and it must reach the executor, or arm 1 proved only that this gateway \
         refuses attestations"
    );

    drop(conn);
    drop(client);
    server.shutdown().await;
}
