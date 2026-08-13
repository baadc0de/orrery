//! End-to-end test of the persistd gateway against a client speaking the same
//! wire surface the `aeronet_iroh` gateway client uses: the admission
//! uni-stream, then tagged datagrams carrying `GatewayMsg`s (docs/11-roadmap.md
//! §P2).
//!
//! This closes the loop the client-side pipeline test (docs/10-crates.md §9,
//! `tests/client_pipeline.rs`) proves from the other side — a real
//! client → gateway → cell-actor path — but Bevy-free, using the raw iroh
//! endpoint directly (D15).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router, RuntimeConfig,
    GATEWAY_ALPN,
};
use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame,
};
use orrery_protocol::{
    Attestation, CellId, DiffUplink, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp,
    IntentOutcome, PersistId, RecordKind, Tick,
};
use tokio::sync::Mutex;

fn node(n: u8) -> orrery_protocol::NodeId {
    secret(n).public()
}

fn secret(n: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed)
}

/// A properly-signed intent from `key` — the gateway now verifies the issuer
/// signature and binds it to the connection's id (D11 §2.2), so the old
/// `b"test"`-signed fixture no longer commits.
fn signed_intent(id: u128, key: &iroh_base::SecretKey) -> Intent {
    let mut intent = Intent {
        intent_id: id,
        issuer: key.public(),
        cell_epoch: Epoch::new(0),
        ops: vec![IntentOp {
            op: 1,
            args: bytes::Bytes::from_static(b"trade"),
        }],
        attestations: vec![Attestation {
            witness: node(2),
            signature: secret(2).sign(b"test"),
        }],
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

#[test]
fn gateway_closes_the_client_to_actor_path() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new({
            let store: std::sync::Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
                std::sync::Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
            CellRuntime::open(&runtime_config(dir.path()), &store).unwrap()
        }));

        // Coerce the single-node runtime into the routing surface the gateway
        // uses. `Mutex<CellRuntime>` implements `Router`.
        let router: Arc<dyn Router> = runtime.clone();
        let server = GatewayServer::spawn(GatewayConfig::default(), router)
            .await
            .unwrap();
        let server_addr = server.addr();

        // A raw iroh client mirroring aeronet_iroh's outgoing session.
        let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![GATEWAY_ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let conn = client.connect(server_addr, GATEWAY_ALPN).await.unwrap();

        // Admission: the gateway streams [ACCEPTED] (byte 0) on a uni stream.
        let mut admission = conn.accept_uni().await.unwrap();
        let msg = admission.read_to_end(16).await.unwrap();
        assert_eq!(msg, vec![0u8]);

        // Hello.
        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: b"tok".to_vec(),
            node: node(1),
        })))
        .unwrap();

        // Bulk diff.
        conn.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(1),
                tick: Tick::new(1),
                kind: RecordKind::Spawn,
                payload: bytes::Bytes::from_static(b"hp=100"),
                seq: 1,
            },
        })))
        .unwrap();

        // Intent — signed by the gateway's own identity (the fixture has no
        // executor configured, so the honest outcome is a rejection, never a
        // fake commit; the commit path is covered by tests/intent_commit.rs).
        let intent = signed_intent(7, &secret(1));
        conn.send_datagram(Bytes::from(encode_stream_frame(
            &GatewayMsg::SubmitIntent {
                intent: intent.clone(),
            },
        )))
        .unwrap();

        // Subscribe to the (single) shard cell.
        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![CellId::ROOT],
        })))
        .unwrap();

        // Collect the replies: expect a bulk ack, an area page, and an intent ack.
        let mut got_ack = false;
        let mut got_page = false;
        let mut got_intent = false;
        for _ in 0..8 {
            if got_ack && got_page && got_intent {
                break;
            }
            let pkt = match tokio::time::timeout(Duration::from_secs(5), conn.read_datagram()).await
            {
                Ok(Ok(p)) => p,
                _ => break,
            };
            if let Some(GatewayReply::BulkAck { entity, tick, .. }) = decode_datagram(&pkt) {
                got_ack = entity == PersistId::new(1) && tick == Tick::new(1);
            }
            if let Some(GatewayReply::AreaPage { cell, page }) = decode_stream_frame(&pkt) {
                got_page = cell == CellId::ROOT
                    && page.total_chunks == 1
                    && page.chunk_index == 0
                    && page.entities == vec![PersistId::new(1)];
            }
            if let Some(GatewayReply::IntentAck { intent_id, outcome }) = decode_stream_frame(&pkt)
            {
                // The gateway has no executor configured: the honest ack is a
                // rejection (never a fake commit). The commit path is covered
                // by tests/intent_commit.rs against a configured executor.
                got_intent = intent_id == 7 && matches!(outcome, IntentOutcome::Rejected { .. });
            }
        }
        assert!(got_ack, "expected a bulk ack for the diff");
        assert!(got_page, "expected an area page for the subscribe");
        assert!(
            got_intent,
            "expected an intent ack for the submitted intent"
        );

        // The diff reached the actor: a snapshot reflects the journaled entity.
        {
            let rt = runtime.lock().await;
            let page = rt.read(GridId::ROOT, CellId::ROOT).await.unwrap();
            let entity = page
                .entities
                .get(&PersistId::new(1))
                .expect("entity journaled");
            assert_eq!(entity.components.as_ref(), b"hp=100");
        }

        server.shutdown().await;
    });
}
