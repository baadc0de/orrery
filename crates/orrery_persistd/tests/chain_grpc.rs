#![cfg(feature = "chain-grpc")]

use std::sync::Arc;
use std::time::Duration;

use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, spawn_adopted_chain, spawn_chain_grpc, ChainConfig, ChainTransport,
    DurableChainId, GrpcChainTransport, Journal, JournalConfig,
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

#[tokio::test]
async fn promoted_history_survives_restart_idempotently_and_seeds_new_follower() {
    let source_dir = tempfile::tempdir().unwrap();
    let mut source = Arc::new(Journal::open(&config(source_dir.path())).unwrap());
    let inbound = spawn_chain_grpc("127.0.0.1:0".parse().unwrap(), Arc::clone(&source), chain())
        .await
        .unwrap();
    let input = GrpcChainTransport::connect(inbound.addr(), chain())
        .await
        .unwrap();
    input.append(record(10)).await.unwrap();
    input.append(record(82)).await.unwrap();
    drop(input);
    inbound.shutdown().await;
    source.close().await.unwrap();
    drop(source);

    source = Arc::new(Journal::open(&config(source_dir.path())).unwrap());
    let adopted = source.adopt_chain_history(chain()).unwrap();
    assert_eq!(adopted.watermark(), Some(Lsn::new(0, 82)));
    assert_eq!(
        source.adopt_chain_history(chain()).unwrap().watermark(),
        adopted.watermark()
    );

    let output_dir = tempfile::tempdir().unwrap();
    let output = Arc::new(Journal::open(&config(output_dir.path())).unwrap());
    let new_chain = DurableChainId {
        primary_node: node(2),
        follower_node: node(3),
        shard_set: b"root/0-7".to_vec(),
        epoch: 5,
    };
    let outbound = spawn_chain_grpc(
        "127.0.0.1:0".parse().unwrap(),
        Arc::clone(&output),
        new_chain.clone(),
    )
    .await
    .unwrap();
    let transport: Arc<dyn ChainTransport> = Arc::new(
        GrpcChainTransport::connect(outbound.addr(), new_chain)
            .await
            .unwrap(),
    );
    let replicator = spawn_adopted_chain(
        Arc::clone(&source),
        adopted,
        transport,
        &ChainConfig::default(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while replicator.follower_watermark() != Some(Lsn::new(0, 82)) {
        assert!(
            std::time::Instant::now() < deadline,
            "adopted prefix did not reach new follower"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mirrored = output
        .scan_from(Lsn::new(0, 0))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        mirrored
            .iter()
            .map(|row| row.record.lsn)
            .collect::<Vec<_>>(),
        vec![Lsn::new(0, 10), Lsn::new(0, 82)]
    );
    replicator.shutdown().await;
    outbound.shutdown().await;
    source.close().await.unwrap();
    output.close().await.unwrap();
}

#[tokio::test]
async fn a_bumped_epoch_refuses_rather_than_forking_the_mirrored_namespace() {
    // `fence::activate_shards` bumps the ownership epoch on every activation,
    // an ordinary clean restart of the same owner included. The epoch is part
    // of `DurableChainId` and the dedupe index is keyed by it, so a restarted
    // follower at the new epoch used to rebuild an empty cursor and take a
    // full re-stream into a second physical copy of every record — at a
    // healthy zero-byte lag, and poisoning promotion afterwards.
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
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
    transport
        .append_batch(vec![record(10), record(82)])
        .await
        .unwrap();
    drop(transport);
    server.shutdown().await;
    journal.close().await.unwrap();
    drop(journal);

    // The same follower journal, reopened under the next ownership epoch.
    journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
    let next = DurableChainId {
        epoch: chain().epoch + 1,
        ..chain()
    };
    let refused = spawn_chain_grpc("127.0.0.1:0".parse().unwrap(), Arc::clone(&journal), next)
        .await
        .expect_err("a bumped epoch must not open a second mirrored namespace");
    assert!(
        refused.to_string().contains("restart handshake"),
        "refusal must name the missing handshake: {refused}"
    );

    // No re-stream happened, so the mirrored history is still one physical
    // copy per origin LSN and promotion stays unambiguous.
    let stored = journal
        .scan_from(Lsn::new(0, 0))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        stored.iter().map(|row| row.record.lsn).collect::<Vec<_>>(),
        vec![Lsn::new(0, 10), Lsn::new(0, 82)]
    );
    assert_eq!(
        journal.adopt_chain_history(chain()).unwrap().watermark(),
        Some(Lsn::new(0, 82)),
        "the surviving epoch must still adopt without an ambiguous identity"
    );
    journal.close().await.unwrap();
}
