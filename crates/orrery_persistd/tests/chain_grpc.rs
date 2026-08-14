#![cfg(feature = "chain-grpc")]

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, spawn_chain_grpc, ChainTransport, DurableChainId, GrpcChainTransport, Journal,
    JournalConfig,
};
use orrery_protocol::{
    CellId, Epoch, GridId, JournalRecord, Lsn, NodeId, PersistId, RecordKind, Tick,
};

fn node(n: u8) -> NodeId {
    let mut seed = [0; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

fn config(path: &std::path::Path) -> JournalConfig {
    JournalConfig {
        dir: path.to_path_buf(),
        commit: GroupCommitConfig {
            mode: AdaptiveCommitMode::AlwaysBatch,
            batch_window: Duration::from_millis(1),
            batch_max_records: 128,
            batch_max_bytes: 1 << 20,
        },
    }
}

fn record(origin: u64) -> JournalRecord {
    let payload = origin.to_le_bytes();
    JournalRecord {
        lsn: Lsn::new(0, origin),
        cell: CellId::ROOT,
        grid: GridId::ROOT,
        entity: PersistId::new(origin),
        tick: Tick::new(origin),
        epoch: Epoch::new(0),
        author: node(1),
        kind: RecordKind::Spawn,
        payload: bytes::Bytes::copy_from_slice(&payload),
        crc: payload_crc(&payload),
    }
}

fn chain() -> DurableChainId {
    DurableChainId {
        primary_node: node(1),
        follower_node: node(2),
        shard_set: b"root/0-7".to_vec(),
        epoch: 4,
    }
}

#[tokio::test]
async fn grpc_batch_ack_and_restart_probe_are_durable() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().unwrap(),
        Arc::clone(&journal),
        chain(),
    )
    .await
    .unwrap();
    let addr = server.addr();
    let transport = GrpcChainTransport::connect(addr, chain()).await.unwrap();
    assert_eq!(
        transport
            .append_batch(vec![record(10), record(82), record(154)])
            .await
            .unwrap(),
        Lsn::new(0, 154)
    );
    assert_eq!(
        server.sessions_opened(),
        1,
        "append must reuse the open stream"
    );
    assert_eq!(
        transport.follower_watermark().await,
        Some(Lsn::new(0, 154)),
        "watermark call must perform a fresh reconnect probe"
    );
    assert_eq!(server.sessions_opened(), 2);
    drop(transport);
    server.shutdown().await;
    journal.close().await.unwrap();
    drop(journal);

    journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
    let server = spawn_chain_grpc(addr, Arc::clone(&journal), chain())
        .await
        .unwrap();
    let transport = GrpcChainTransport::connect(server.addr(), chain())
        .await
        .unwrap();
    assert_eq!(transport.reconnect().await.unwrap(), Some(Lsn::new(0, 154)));
    assert_eq!(
        transport.append(record(226)).await.unwrap(),
        Lsn::new(0, 226)
    );
    assert_eq!(
        journal
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .len(),
        4
    );
    drop(transport);
    server.shutdown().await;
    journal.close().await.unwrap();
}

#[tokio::test]
async fn stream_loss_opens_one_fresh_session_and_resumes_from_remote_progress() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().unwrap(),
        Arc::clone(&journal),
        chain(),
    )
    .await
    .unwrap();
    let transport = GrpcChainTransport::connect(server.addr(), chain())
        .await
        .unwrap();
    transport.append(record(10)).await.unwrap();
    assert_eq!(server.sessions_opened(), 1);

    server.close_connections().await;
    assert_eq!(transport.append(record(82)).await.unwrap(), Lsn::new(0, 82));
    assert_eq!(server.sessions_opened(), 2);
    assert_eq!(
        journal
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .len(),
        2
    );

    drop(transport);
    server.shutdown().await;
    journal.close().await.unwrap();
}

#[tokio::test]
async fn durable_append_lost_ack_replays_without_duplicate_record() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
    let server = spawn_chain_grpc(
        "127.0.0.1:0".parse().unwrap(),
        Arc::clone(&journal),
        chain(),
    )
    .await
    .unwrap();
    let transport = GrpcChainTransport::connect(server.addr(), chain())
        .await
        .unwrap();

    server.fail_next_ack();
    assert_eq!(transport.append(record(10)).await.unwrap(), Lsn::new(0, 10));
    assert_eq!(server.sessions_opened(), 2);
    assert_eq!(
        journal
            .scan_from(Lsn::new(0, 0))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .len(),
        1,
        "ambiguous append must be recovered by durable watermark, not duplicated"
    );

    drop(transport);
    server.shutdown().await;
    journal.close().await.unwrap();
}
