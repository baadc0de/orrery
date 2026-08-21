//! The courier path for coordinator witness-set announcements
//! ([D28](../../../docs/adr/0028-witness-set-seeding.md) clause (a)).
//!
//! A gateway has no connection to the coordinator, by design, so the only way
//! a witness set reaches one is inside bytes a peer cannot forge. This file is
//! the end-to-end proof of that hop: a real client presents a real
//! coordinator-signed envelope over loopback iroh, and the gateway verifies
//! it, caches it, and says what it did.
//!
//! The verification *rules* are the cache's own unit tests — this asserts the
//! wiring, which is the half that unit tests structurally cannot reach: that
//! the message is decoded, that the presenting peer's identity (not a claimed
//! one) is what step 6 tests, that the ack carries a code a peer can act on,
//! and that a gateway with no cache says so instead of dropping the message.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::witness_epoch::WitnessEpochAuthority;
use orrery_persistd::{
    CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router, RuntimeConfig,
    GATEWAY_ALPN,
};
use orrery_protocol::{
    CellId, Epoch, GatewayMsg, GatewayReply, GridId, IssuerKey, IssuerKeyId, NodeId,
    WitnessEpochClaimsV1, WitnessEpochV1, WITNESS_EPOCH_ACK_OK, WITNESS_EPOCH_ACK_UNSUPPORTED,
    WITNESS_EPOCH_ACK_UNTRUSTED,
};
use tokio::sync::Mutex;

const HANDLE: u64 = 0x0001_0000_0000_0007;

fn secret(n: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed)
}

fn coordinator() -> iroh_base::SecretKey {
    secret(200)
}

fn witnesses() -> Vec<NodeId> {
    (0..7).map(|i| secret(100 + i).public()).collect()
}

/// A signed announcement for `(ROOT, ROOT, epoch)` under `signer`.
fn announcement(epoch: u32, signer: &iroh_base::SecretKey) -> Vec<u8> {
    let selected = witnesses();
    let mut candidates = selected.clone();
    candidates.sort_by_key(|node| *node.as_bytes());
    let claims = WitnessEpochClaimsV1::new(
        GridId::ROOT,
        CellId::ROOT,
        epoch,
        HANDLE,
        30_000,
        30_000,
        candidates,
        selected,
        orrery_protocol::witness_epoch_commitment(GridId::ROOT, CellId::ROOT, epoch, &[7u8; 32]),
        None,
        IssuerKeyId::new(1),
    );
    WitnessEpochV1::sign(claims, signer)
        .expect("claims encode")
        .encode()
        .expect("envelope encodes")
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

struct Session {
    server: GatewayServer,
    conn: lanes::GatewayLanes,
    _client: iroh::Endpoint,
    _dir: tempfile::TempDir,
    _runtime: Arc<Mutex<CellRuntime>>,
}

async fn connect(config: GatewayConfig, key: &iroh_base::SecretKey) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new({
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
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
    Session {
        server,
        conn,
        _client: client,
        _dir: dir,
        _runtime: runtime,
    }
}

/// Present `bytes` and read back the ack.
async fn present(conn: &lanes::GatewayLanes, bytes: Vec<u8>) -> (Option<u32>, u8) {
    conn.send_control(&GatewayMsg::WitnessEpoch {
        announcement: bytes,
    })
    .await;
    for _ in 0..8 {
        if let Some(GatewayReply::WitnessEpochAck { epoch, reason }) =
            conn.next_reply(Duration::from_secs(5)).await
        {
            return (epoch, reason);
        }
    }
    panic!("no WitnessEpochAck after 8 inbound messages");
}

#[tokio::test]
async fn a_couriered_announcement_is_verified_cached_and_acknowledged() {
    let key = secret(1);
    let epochs = Arc::new(WitnessEpochAuthority::new([IssuerKey::new(
        IssuerKeyId::new(1),
        coordinator().public(),
    )]));
    let config = GatewayConfig {
        witness_epochs: Some(Arc::clone(&epochs)),
        ..support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT])
    };
    let session = connect(config, &key).await;

    assert_eq!(
        present(&session.conn, announcement(1, &coordinator())).await,
        (Some(1), WITNESS_EPOCH_ACK_OK)
    );

    let cached = session
        .server
        .witness_epochs()
        .expect("configured")
        .resolve(HANDLE)
        .expect("the announcement is on file");
    assert_eq!(cached.snapshot.selected, witnesses());
    assert_eq!(
        cached.draw_commit,
        orrery_protocol::attestation_draw_commitment(
            GridId::ROOT,
            CellId::ROOT,
            1,
            cached.draw_key()
        ),
        "the gateway minted a draw key for the epoch and committed to it the \
         moment it accepted the announcement — before any intent under the \
         epoch could have been submitted"
    );

    // A peer is a courier and not an authority: an envelope signed by anyone
    // else is refused, and the refusal is a code the peer can act on rather
    // than silence. Without this the whole scheme collapses to "the gateway
    // believes whichever witness set a submitter hands it".
    let impostor = present(&session.conn, announcement(2, &secret(201))).await;
    assert_eq!(impostor, (None, WITNESS_EPOCH_ACK_UNTRUSTED));
    assert_eq!(
        epochs.len(),
        1,
        "a refused announcement leaves the cache exactly as it was"
    );
}

#[tokio::test]
async fn a_gateway_with_no_epoch_cache_says_so_rather_than_dropping_the_message() {
    let key = secret(2);
    let config = support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT]);
    assert!(
        config.witness_epochs.is_none(),
        "None is the default, and it is the enforcement switch's off position \
         at the transport layer"
    );
    let session = connect(config, &key).await;

    assert_eq!(
        present(&session.conn, announcement(1, &coordinator())).await,
        (None, WITNESS_EPOCH_ACK_UNSUPPORTED),
        "silence would leave a peer couriering announcements into a void and \
         discovering it only when its attested intents started failing"
    );
    assert!(session.server.witness_epochs().is_none());
}
