//! End-to-end test of the persistd gateway against a client speaking the same
//! wire surface the `aeronet_iroh` gateway client uses: the admission
//! uni-stream, then tagged datagrams carrying `GatewayMsg`s (docs/11-roadmap.md
//! §P2).
//!
//! This closes the loop the client-side pipeline test (docs/10-crates.md §9,
//! `tests/client_pipeline.rs`) proves from the other side — a real
//! client → gateway → cell-actor path — but Bevy-free, using the raw iroh
//! endpoint directly (D15).

mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::gateway::{ClaimClock, GatewayClock, IdentityHealth};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, GatewayConfig, GatewayServer, JournalConfig,
    MemFenceStore, Router, RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame,
};
use orrery_protocol::{
    Attestation, CellId, ClaimBasis, ClaimKind, CoordinatorInterestSnapshot, DiffUplink,
    EntityRekey, Epoch, GatewayMsg, GatewayReply, GridId, Intent, IntentOp, IntentOutcome,
    JournalRecord, Lease, LeaseFlags, LeaseId, LeaseMsg, Lsn, PersistId, RecordKind, SeqPair, Tick,
    UnixMillis, ENTITY_REKEY_VERSION,
};
use tokio::sync::Mutex;

fn node(n: u8) -> orrery_protocol::NodeId {
    support::node(n)
}

fn secret(n: u8) -> iroh_base::SecretKey {
    support::secret(n)
}

#[derive(Debug)]
struct AtomicGatewayClock(AtomicU64);

impl GatewayClock for AtomicGatewayClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Debug)]
struct AtomicClaimClock(AtomicU64);

impl ClaimClock for AtomicClaimClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct SwitchIdentityHealth(AtomicBool);

impl IdentityHealth for SwitchIdentityHealth {
    fn is_available(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct BlockingClaimRouter {
    entered: tokio::sync::mpsc::Sender<()>,
    claim_release: tokio::sync::Notify,
    park_entered: tokio::sync::mpsc::Sender<()>,
    park_release: tokio::sync::Notify,
    block_claim_once: AtomicBool,
    entity: PersistId,
    holder: orrery_protocol::NodeId,
    lease_id: LeaseId,
    live: Mutex<Option<Lease>>,
    parked: Mutex<Vec<LeaseId>>,
}

#[async_trait::async_trait]
impl Router for BlockingClaimRouter {
    async fn apply(
        &self,
        _record: JournalRecord,
    ) -> Result<Arc<orrery_persistd::journal::AppendHandle>, orrery_persistd::Reject> {
        Err(orrery_persistd::Reject::JournalClosed)
    }

    async fn read(
        &self,
        _grid: GridId,
        _cell: CellId,
    ) -> Result<orrery_persistd::SnapshotPage, orrery_persistd::Reject> {
        Err(orrery_persistd::Reject::JournalClosed)
    }

    async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
        true
    }

    async fn committed_entity_cell(
        &self,
        _grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, orrery_persistd::Reject> {
        assert_eq!(entity, self.entity);
        Ok(Some(CellId::ROOT))
    }

    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: orrery_protocol::NodeId,
        _kind: ClaimKind,
        _now_ms: u64,
    ) -> Result<ClaimResult, orrery_persistd::Reject> {
        assert_eq!(
            (grid, cell, entity, holder),
            (GridId::ROOT, CellId::ROOT, self.entity, self.holder)
        );
        if self.block_claim_once.swap(false, Ordering::SeqCst) {
            self.entered
                .send(())
                .await
                .map_err(|_| orrery_persistd::Reject::JournalClosed)?;
            self.claim_release.notified().await;
        }
        let lease = Lease {
            entity,
            holder: Some(holder),
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
            lease_id: self.lease_id,
            expires_at: u64::MAX,
            flags: LeaseFlags::default(),
        };
        *self.live.lock().await = Some(lease.clone());
        Ok(ClaimResult::Granted(lease))
    }

    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: orrery_protocol::NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<Lease>, orrery_persistd::Reject> {
        assert_eq!(
            (grid, cell, entity, holder, lease_id),
            (
                GridId::ROOT,
                CellId::ROOT,
                self.entity,
                self.holder,
                self.lease_id,
            )
        );
        self.park_entered
            .send(())
            .await
            .map_err(|_| orrery_persistd::Reject::JournalClosed)?;
        self.park_release.notified().await;
        let mut live = self.live.lock().await;
        let parked = live.as_ref().is_some_and(|lease| {
            lease.entity == entity && lease.holder == Some(holder) && lease.lease_id == lease_id
        });
        if parked {
            *live = None;
            self.parked.lock().await.push(lease_id);
        }
        Ok(parked.then(|| Lease {
            entity,
            holder: None,
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
            lease_id,
            expires_at: 0,
            flags: LeaseFlags::default(),
        }))
    }

    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: orrery_protocol::NodeId,
        lease_id: LeaseId,
        _now_ms: u64,
    ) -> Result<Option<Lease>, orrery_persistd::Reject> {
        assert_eq!(
            (grid, cell, entity, holder, lease_id),
            (
                GridId::ROOT,
                CellId::ROOT,
                self.entity,
                self.holder,
                self.lease_id,
            )
        );
        Ok(self.live.lock().await.clone())
    }
}

struct BlockingHeartbeatRouter {
    entered: tokio::sync::mpsc::Sender<()>,
    release: tokio::sync::Notify,
    block_once: AtomicBool,
    entity: PersistId,
    holder: orrery_protocol::NodeId,
    lease_id: LeaseId,
}

impl BlockingHeartbeatRouter {
    fn lease(&self) -> Lease {
        Lease {
            entity: self.entity,
            holder: Some(self.holder),
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
            lease_id: self.lease_id,
            expires_at: u64::MAX,
            flags: LeaseFlags::default(),
        }
    }
}

#[async_trait::async_trait]
impl Router for BlockingHeartbeatRouter {
    async fn apply(
        &self,
        _record: JournalRecord,
    ) -> Result<Arc<orrery_persistd::journal::AppendHandle>, orrery_persistd::Reject> {
        Err(orrery_persistd::Reject::JournalClosed)
    }

    async fn read(
        &self,
        _grid: GridId,
        _cell: CellId,
    ) -> Result<orrery_persistd::SnapshotPage, orrery_persistd::Reject> {
        Err(orrery_persistd::Reject::JournalClosed)
    }

    async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
        true
    }

    async fn committed_entity_cell(
        &self,
        _grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, orrery_persistd::Reject> {
        assert_eq!(entity, self.entity);
        Ok(Some(CellId::ROOT))
    }

    async fn claim_lease(
        &self,
        _grid: GridId,
        _cell: CellId,
        entity: PersistId,
        holder: orrery_protocol::NodeId,
        _kind: ClaimKind,
        _now_ms: u64,
    ) -> Result<ClaimResult, orrery_persistd::Reject> {
        assert_eq!((entity, holder), (self.entity, self.holder));
        Ok(ClaimResult::Granted(self.lease()))
    }

    async fn heartbeat_lease(
        &self,
        _grid: GridId,
        _cell: CellId,
        entity: PersistId,
        holder: orrery_protocol::NodeId,
        lease_id: LeaseId,
        _now_ms: u64,
    ) -> Result<Option<Lease>, orrery_persistd::Reject> {
        assert_eq!(
            (entity, holder, lease_id),
            (self.entity, self.holder, self.lease_id)
        );
        if self.block_once.swap(false, Ordering::SeqCst) {
            self.entered
                .send(())
                .await
                .map_err(|_| orrery_persistd::Reject::JournalClosed)?;
            self.release.notified().await;
        }
        Ok(Some(self.lease()))
    }
}

fn session_token(
    issuer: &iroh_base::SecretKey,
    bound_node: orrery_protocol::NodeId,
    issued_at_ms: u64,
    ttl_ms: u64,
) -> Vec<u8> {
    support::session_token(issuer, bound_node, issued_at_ms, ttl_ms)
}

async fn raw_connection(
    key: iroh_base::SecretKey,
    address: iroh::EndpointAddr,
) -> (iroh::Endpoint, iroh::endpoint::Connection) {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key)
        .bind()
        .await
        .unwrap();
    let connection = endpoint.connect(address, GATEWAY_ALPN).await.unwrap();
    let mut admission = connection.accept_uni().await.unwrap();
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0u8]);
    (endpoint, connection)
}

async fn seed_entity(runtime: &Arc<Mutex<CellRuntime>>, entity: PersistId, cell: CellId) {
    let actor = runtime
        .lock()
        .await
        .actor(GridId::ROOT, cell)
        .expect("actor for seeded entity")
        .clone();
    let payload = Bytes::from_static(b"seeded");
    actor
        .start_diff(JournalRecord {
            lsn: Lsn::new(0, 0),
            cell,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(0),
            epoch: Epoch::new(0),
            author: node(9),
            kind: RecordKind::Spawn,
            crc: payload_crc(&payload),
            payload,
        })
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();
}

async fn claim_reply(
    connection: &iroh::endpoint::Connection,
    entity: PersistId,
    grid: GridId,
    cell: CellId,
    kind: ClaimKind,
    basis: ClaimBasis,
) -> LeaseMsg {
    connection
        .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id: orrery_protocol::ClaimId(1),
                entity,
                grid,
                cell,
                kind,
                basis,
                observed: Default::default(),
                tick: Tick::new(1),
            },
        })))
        .unwrap();
    loop {
        let packet = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
            .await
            .unwrap()
            .unwrap();
        if let Some(GatewayReply::Lease { message }) = decode_stream_frame(&packet) {
            return message;
        }
    }
}

async fn receives_hello_ack(connection: &iroh::endpoint::Connection) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(150), connection.read_datagram()).await,
        Ok(Ok(packet))
            if matches!(decode_stream_frame(&packet), Some(GatewayReply::HelloAck { .. }))
    )
}

async fn heartbeat_reply(
    connection: &iroh::endpoint::Connection,
    lease_ids: Vec<orrery_protocol::LeaseId>,
    tick: Tick,
) -> (Vec<orrery_protocol::Lease>, Vec<orrery_protocol::LeaseId>) {
    connection
        .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat { lease_ids, tick },
        })))
        .unwrap();
    loop {
        let packet = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
            .await
            .unwrap()
            .unwrap();
        if let Some(GatewayReply::Lease {
            message: LeaseMsg::HeartbeatAck { leases, invalid },
        }) = decode_stream_frame(&packet)
        {
            return (leases, invalid);
        }
    }
}

async fn diff_reply(connection: &iroh::endpoint::Connection, diff: DiffUplink) -> GatewayReply {
    connection
        .send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff { diff })))
        .unwrap();
    loop {
        let packet = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
            .await
            .unwrap()
            .unwrap();
        if let Some(reply @ (GatewayReply::BulkAck { .. } | GatewayReply::BulkNack { .. })) =
            decode_datagram(&packet)
        {
            return reply;
        }
    }
}

async fn journal_len(runtime: &Arc<Mutex<CellRuntime>>) -> usize {
    runtime
        .lock()
        .await
        .journal()
        .scan_from(Lsn::new(0, 0))
        .count()
}

async fn assert_diff_denied_without_mutation(
    connection: &iroh::endpoint::Connection,
    actor: &orrery_persistd::CellActorHandle,
    runtime: &Arc<Mutex<CellRuntime>>,
    entity: PersistId,
) {
    let journal_appends_before = runtime
        .lock()
        .await
        .journal()
        .commit_metrics()
        .snapshot()
        .total();
    connection
        .send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(2),
                kind: RecordKind::Spawn,
                payload: Bytes::from_static(b"unauthorized"),
                seq: 1,
                lease_id: None,
                authority_seq: None,
            },
        })))
        .unwrap();
    let packet = tokio::time::timeout(Duration::from_secs(1), connection.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        decode_datagram(&packet),
        Some(GatewayReply::BulkNack {
            entity: rejected,
            tick,
            lease: None,
            ..
        }) if rejected == entity && tick == Tick::new(2)
    ));
    assert!(!actor
        .read_snapshot(vec![CellId::ROOT])
        .await
        .unwrap()
        .entities
        .contains_key(&entity));
    assert_eq!(
        runtime
            .lock()
            .await
            .journal()
            .commit_metrics()
            .snapshot()
            .total(),
        journal_appends_before
    );
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
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap()
        }));
        seed_entity(&runtime, PersistId::new(1), CellId::ROOT).await;

        // Coerce the single-node runtime into the routing surface the gateway
        // uses. `Mutex<CellRuntime>` implements `Router`.
        let router: Arc<dyn Router> = runtime.clone();
        let interest_snapshot =
            support::interest_snapshot(node(2), GridId::ROOT, vec![CellId::ROOT]);
        let interest_authority = support::interest_authority([
            interest_snapshot.clone(),
            support::interest_snapshot(node(1), GridId::ROOT, vec![CellId::ROOT]),
        ]);
        let issuer = secret(42);
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority,
                ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
            },
            router,
        )
        .await
        .unwrap();
        let server_addr = server.addr();

        // A raw iroh client mirroring aeronet_iroh's outgoing session.
        let client_key = secret(1);
        let client = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![GATEWAY_ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .secret_key(client_key.clone())
            .bind()
            .await
            .unwrap();
        let conn = client.connect(server_addr, GATEWAY_ALPN).await.unwrap();

        // Admission: the gateway streams [ACCEPTED] (byte 0) on a uni stream.
        let mut admission = conn.accept_uni().await.unwrap();
        let msg = admission.read_to_end(16).await.unwrap();
        assert_eq!(msg, vec![0u8]);

        // A claimed NodeId that does not equal the iroh-authenticated remote
        // identity must not activate lease traffic.
        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: b"bad-node".to_vec(),
            node: node(2),
        })))
        .unwrap();
        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: orrery_protocol::LeaseMsg::Claim {
                claim_id: orrery_protocol::ClaimId(1),
                entity: PersistId::new(999),
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                kind: orrery_protocol::ClaimKind::Weak,
                basis: orrery_protocol::ClaimBasis::Explicit,
                observed: Default::default(),
                tick: Tick::new(0),
            },
        })))
        .unwrap();

        // A matching Hello binds the session and activates control traffic.
        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: session_token(&issuer, node(1), 900, 200),
            node: node(1),
        })))
        .unwrap();

        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: orrery_protocol::LeaseMsg::Claim {
                claim_id: orrery_protocol::ClaimId(2),
                entity: PersistId::new(1),
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                kind: orrery_protocol::ClaimKind::Weak,
                basis: orrery_protocol::ClaimBasis::Explicit,
                observed: Default::default(),
                tick: Tick::new(1),
            },
        })))
        .unwrap();
        let grant = loop {
            let pkt = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
                .await
                .unwrap()
                .unwrap();
            let Some(GatewayReply::Lease {
                message: orrery_protocol::LeaseMsg::Grant { lease_id, seq, .. },
            }) = decode_stream_frame(&pkt)
            else {
                continue;
            };
            break (lease_id, seq);
        };
        let ignored = {
            let actor = runtime
                .lock()
                .await
                .actor(GridId::ROOT, CellId::ROOT)
                .expect("root actor")
                .clone();
            actor
                .validate_lease(PersistId::new(999), node(1), orrery_protocol::LeaseId(0), 0)
                .await
                .unwrap()
        };
        assert!(ignored.is_none(), "mismatched Hello must not admit a claim");

        // Session-indexed heartbeats fan back to the owning actor and return
        // the renewed row on the typed reliable-control reply.
        conn.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: orrery_protocol::LeaseMsg::Heartbeat {
                lease_ids: vec![grant.0, orrery_protocol::LeaseId(999)],
                tick: Tick::new(2),
            },
        })))
        .unwrap();
        let renewed = loop {
            let pkt = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
                .await
                .unwrap()
                .unwrap();
            let Some(GatewayReply::Lease {
                message: orrery_protocol::LeaseMsg::HeartbeatAck { leases, invalid },
            }) = decode_stream_frame(&pkt)
            else {
                continue;
            };
            break (leases, invalid);
        };
        assert_eq!(renewed.1, vec![orrery_protocol::LeaseId(999)]);
        assert_eq!(renewed.0.len(), 1);
        assert_eq!(renewed.0[0].lease_id, grant.0);
        assert_eq!(renewed.0[0].seq, grant.1);
        assert_eq!(renewed.0[0].holder, Some(node(1)));

        // A stale pair is rejected before reaching the actor and carries the
        // current registrar row so the holder can recover without a lookup.
        conn.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(1),
                tick: Tick::new(0),
                kind: RecordKind::Spawn,
                payload: bytes::Bytes::from_static(b"stale"),
                seq: 0,
                lease_id: Some(orrery_protocol::LeaseId(0)),
                authority_seq: Some(Default::default()),
            },
        })))
        .unwrap();
        let pkt = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
            .await
            .unwrap()
            .unwrap();
        let Some(GatewayReply::BulkNack {
            lease: Some(current),
            ..
        }) = decode_datagram(&pkt)
        else {
            panic!("stale lease must receive lease-bearing BulkNack");
        };
        assert_eq!(current.lease_id, grant.0);
        assert_eq!(current.seq, grant.1);

        // Missing fencing fields are also lease failures, not a generic
        // malformed-diff response: the actor returns its current row so a
        // client can recover the pair without an out-of-band lookup.
        conn.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(1),
                tick: Tick::new(99),
                kind: RecordKind::Spawn,
                payload: bytes::Bytes::from_static(b"missing-fence"),
                seq: 99,
                lease_id: None,
                authority_seq: None,
            },
        })))
        .unwrap();
        let pkt = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram())
            .await
            .unwrap()
            .unwrap();
        let Some(GatewayReply::BulkNack {
            lease: Some(current),
            ..
        }) = decode_datagram(&pkt)
        else {
            panic!("missing fence must receive a lease-bearing BulkNack");
        };
        assert_eq!(current.lease_id, grant.0);
        assert_eq!(current.seq, grant.1);

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
                lease_id: Some(grant.0),
                authority_seq: Some(grant.1),
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
        assert_eq!(
            server.interest_authority().snapshot_for(node(2)),
            Some(interest_snapshot),
            "GatewayMsg::Subscribe must not alter the running gateway's coordinator-owned interest"
        );
        assert!(
            server
                .interest_authority()
                .allows(node(1), GridId::ROOT, CellId::ROOT, 0),
            "Subscribe must not replace the coordinator-provided interest"
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

        // Closing the authenticated session immediately parks every lease in
        // its session index. The stale grant remains observable only as the
        // parked current row for recovery/NACK purposes.
        drop(conn);
        client.close().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let actor = {
                runtime
                    .lock()
                    .await
                    .actor(GridId::ROOT, CellId::ROOT)
                    .expect("root actor")
                    .clone()
            };
            let current = actor
                .validate_lease(PersistId::new(1), node(1), grant.0, 0)
                .await
                .unwrap()
                .expect("current lease row");
            if current.holder.is_none() {
                assert!(current.lease_id > grant.0);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gateway did not park the disconnected session's lease"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        server.shutdown().await;
    });
}

#[test]
fn gateway_gates_client_claims_against_committed_location_and_authoritative_interest() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: committed targets in covered, uncovered, and moved cells.
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new({
            let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
                Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap()
        }));
        let cells = CellId::ROOT.children();
        let covered = PersistId::new(301);
        let uncovered = PersistId::new(302);
        let moved = PersistId::new(303);
        let orphan = PersistId::new(304);
        let stale = PersistId::new(305);
        seed_entity(&runtime, covered, cells[0]).await;
        seed_entity(&runtime, uncovered, cells[1]).await;
        seed_entity(&runtime, moved, cells[0]).await;
        seed_entity(&runtime, moved, cells[1]).await;
        seed_entity(&runtime, orphan, cells[0]).await;
        seed_entity(&runtime, stale, cells[0]).await;

        let issuer = secret(42);
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: support::interest_authority([
                    CoordinatorInterestSnapshot {
                        peer: node(1),
                        epoch: Epoch::new(2),
                        grid: GridId::ROOT,
                        covered_cells: vec![cells[0]],
                        valid_until_ms: 10_000,
                    },
                    CoordinatorInterestSnapshot {
                        peer: node(2),
                        epoch: Epoch::new(2),
                        grid: GridId::ROOT,
                        covered_cells: vec![cells[0]],
                        valid_until_ms: 0,
                    },
                ]),
                ..support::authority_config(node(1), GridId::ROOT, vec![cells[0]])
            },
            runtime.clone(),
        )
        .await
        .unwrap();
        let (_client, connection) = raw_connection(secret(1), server.addr()).await;
        connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&connection).await);

        // When: a weak contact targets its exact committed, covered cell.
        let granted = claim_reply(
            &connection,
            covered,
            GridId::ROOT,
            cells[0],
            ClaimKind::Weak,
            ClaimBasis::Contact { tick: Tick::new(1) },
        )
        .await;

        // Then: the registrar grants it.
        let LeaseMsg::Grant {
            claim_id,
            entity: granted_entity,
            lease_id: covered_lease_id,
            ..
        } = granted
        else {
            panic!("covered weak claim must be granted");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId(1));
        assert_eq!(granted_entity, covered);

        // When: invalid locations, interest, or client-owned basis are submitted.
        connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Subscribe {
                grid: GridId::ROOT,
                cells: vec![cells[1]],
            })))
            .unwrap();
        let denied = [
            (
                PersistId::new(399),
                GridId::ROOT,
                cells[0],
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            ),
            (
                moved,
                GridId::ROOT,
                cells[0],
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            ),
            (
                uncovered,
                GridId::ROOT,
                cells[1],
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            ),
            (
                orphan,
                GridId::ROOT,
                cells[0],
                ClaimKind::Weak,
                ClaimBasis::Orphan,
            ),
            (
                covered,
                GridId::new(9),
                cells[0],
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            ),
        ];
        for (entity, grid, cell, kind, basis) in denied {
            let reply = claim_reply(&connection, entity, grid, cell, kind, basis).await;
            assert!(matches!(
                reply,
                LeaseMsg::Deny {
                    claim_id: Some(orrery_protocol::ClaimId(1)),
                    entity: denied_entity,
                    reason: orrery_protocol::DenyReason::NotEligible,
                    ..
                } if denied_entity == entity
            ));
        }

        let (_stale_client, stale_connection) = raw_connection(secret(2), server.addr()).await;
        stale_connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(2), 900, 200),
                node: node(2),
            })))
            .unwrap();
        assert!(receives_hello_ack(&stale_connection).await);
        let stale_reply = claim_reply(
            &stale_connection,
            stale,
            GridId::ROOT,
            cells[0],
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await;
        assert!(matches!(
            stale_reply,
            LeaseMsg::Deny {
                entity,
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            } if entity == stale
        ));

        // Then: denied entities have no registrar row, while strong eligibility stays actor-owned.
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, CellId::ROOT)
            .unwrap()
            .clone();
        for entity in [PersistId::new(399), moved, uncovered, orphan, stale] {
            assert!(actor
                .validate_lease(entity, node(1), orrery_protocol::LeaseId(0), 0)
                .await
                .unwrap()
                .is_none());
        }
        let covered_after_denials = actor
            .validate_lease(covered, node(1), covered_lease_id, 0)
            .await
            .unwrap()
            .expect("covered lease remains current");
        assert_eq!(covered_after_denials.lease_id, covered_lease_id);
        assert_eq!(covered_after_denials.holder, Some(node(1)));
        let strong = claim_reply(
            &connection,
            uncovered,
            GridId::ROOT,
            cells[1],
            ClaimKind::Strong,
            ClaimBasis::Explicit,
        )
        .await;
        assert!(matches!(strong, LeaseMsg::Grant { entity, .. } if entity == uncovered));

        server.shutdown().await;
    });
}

#[test]
fn gateway_rejects_unverified_raw_iroh_hellos_before_authority_activation() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new({
            let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
                Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap()
        }));
        let router: Arc<dyn Router> = runtime.clone();
        let issuer = secret(42);
        let server = GatewayServer::spawn(
            GatewayConfig {
                authorizer: support::authorizer(&issuer),
                ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
            },
            router,
        )
        .await
        .unwrap();
        let (_client, connection) = raw_connection(secret(1), server.addr()).await;
        let actor = {
            let runtime = runtime.lock().await;
            runtime.actor(GridId::ROOT, CellId::ROOT).unwrap().clone()
        };
        let invalid_tokens = [
            Vec::new(),
            vec![1, 2, 3],
            session_token(&secret(43), node(1), 900, 200),
            session_token(&issuer, node(2), 900, 200),
            session_token(&issuer, node(1), 1_001, 200),
            session_token(&issuer, node(1), 800, 200),
        ];

        for (index, token) in invalid_tokens.into_iter().enumerate() {
            connection
                .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                    token,
                    node: node(1),
                })))
                .unwrap();
            assert!(!receives_hello_ack(&connection).await);

            let entity = PersistId::new(100 + u64::try_from(index).unwrap());
            connection
                .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
                    message: orrery_protocol::LeaseMsg::Claim {
                        claim_id: orrery_protocol::ClaimId(1),
                        entity,
                        grid: GridId::ROOT,
                        cell: CellId::ROOT,
                        kind: orrery_protocol::ClaimKind::Weak,
                        basis: orrery_protocol::ClaimBasis::Explicit,
                        observed: Default::default(),
                        tick: Tick::new(1),
                    },
                })))
                .unwrap();
            assert!(!receives_hello_ack(&connection).await);
            assert!(actor
                .validate_lease(entity, node(1), orrery_protocol::LeaseId(0), 0)
                .await
                .unwrap()
                .is_none());

            assert_diff_denied_without_mutation(&connection, &actor, &runtime, entity).await;
        }

        connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(2),
            })))
            .unwrap();
        assert!(!receives_hello_ack(&connection).await);
        assert_diff_denied_without_mutation(&connection, &actor, &runtime, PersistId::new(198))
            .await;
        server.shutdown().await;

        let router: Arc<dyn Router> = runtime.clone();
        let deny_server = GatewayServer::spawn(GatewayConfig::default(), router)
            .await
            .unwrap();
        let (_client, deny_connection) = raw_connection(secret(1), deny_server.addr()).await;
        deny_connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(1),
            })))
            .unwrap();
        assert!(!receives_hello_ack(&deny_connection).await);
        assert_diff_denied_without_mutation(
            &deny_connection,
            &actor,
            &runtime,
            PersistId::new(199),
        )
        .await;
        deny_server.shutdown().await;
    });
}

#[test]
fn gateway_graces_only_the_established_matching_token_during_identity_outage() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap(),
        ));
        let router: Arc<dyn Router> = runtime.clone();
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, CellId::ROOT)
            .unwrap()
            .clone();
        let issuer = secret(42);
        let clock = Arc::new(AtomicGatewayClock(AtomicU64::new(1_000)));
        let health = Arc::new(SwitchIdentityHealth(AtomicBool::new(true)));
        let token = session_token(&issuer, node(1), 900, 200);
        let server = GatewayServer::spawn(
            GatewayConfig {
                authorizer: support::authorizer(&issuer),
                identity_clock: clock.clone(),
                identity_health: health.clone(),
                ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
            },
            router,
        )
        .await
        .unwrap();

        let (_first_client, first) = raw_connection(secret(1), server.addr()).await;
        first
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: token.clone(),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&first).await);

        clock.0.store(1_200, Ordering::SeqCst);
        health.0.store(false, Ordering::SeqCst);
        let (_grace_client, grace) = raw_connection(secret(1), server.addr()).await;
        grace
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: token.clone(),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&grace).await);

        let (_new_client, newly_expired) = raw_connection(secret(2), server.addr()).await;
        newly_expired
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(2), 900, 200),
                node: node(2),
            })))
            .unwrap();
        assert!(!receives_hello_ack(&newly_expired).await);
        assert_diff_denied_without_mutation(&newly_expired, &actor, &runtime, PersistId::new(200))
            .await;

        let (_changed_client, changed_token) = raw_connection(secret(1), server.addr()).await;
        changed_token
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 901, 200),
                node: node(1),
            })))
            .unwrap();
        assert!(!receives_hello_ack(&changed_token).await);
        assert_diff_denied_without_mutation(&changed_token, &actor, &runtime, PersistId::new(201))
            .await;

        health.0.store(true, Ordering::SeqCst);
        let (_healthy_client, healthy) = raw_connection(secret(1), server.addr()).await;
        healthy
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token,
                node: node(1),
            })))
            .unwrap();
        assert!(!receives_hello_ack(&healthy).await);
        assert_diff_denied_without_mutation(&healthy, &actor, &runtime, PersistId::new(202)).await;
        server.shutdown().await;
    });
}

#[test]
fn stale_inflight_claim_grant_is_parked_without_indexing_replacement() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a raw gateway claim is waiting for committed actor state.
        let entity = PersistId::new(490);
        let lease_id = LeaseId(490);
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
        let (park_entered_tx, mut park_entered_rx) = tokio::sync::mpsc::channel(1);
        let router = Arc::new(BlockingClaimRouter {
            entered: entered_tx,
            claim_release: tokio::sync::Notify::new(),
            park_entered: park_entered_tx,
            park_release: tokio::sync::Notify::new(),
            block_claim_once: AtomicBool::new(true),
            entity,
            holder: node(1),
            lease_id,
            live: Mutex::new(None),
            parked: Mutex::new(Vec::new()),
        });
        let server = GatewayServer::spawn(
            support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT]),
            router.clone(),
        )
        .await
        .unwrap();
        let (old_client, old) = raw_connection(secret(1), server.addr()).await;
        old.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })))
        .unwrap();
        assert!(receives_hello_ack(&old).await);
        let old_claim = tokio::spawn(async move {
            let reply = claim_reply(
                &old,
                entity,
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await;
            (old, reply)
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("claim reaches the controlled router")
            .expect("claim route reports entry");

        // When: a replacement for the same peer authenticates while that route is blocked.
        let (replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();

        // Then: registry access remains responsive, and the exact stale grant is compensated.
        assert!(receives_hello_ack(&replacement).await);
        router.claim_release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), park_entered_rx.recv())
            .await
            .expect("stale grant reaches compensation")
            .expect("compensation route reports entry");
        assert!(matches!(
            claim_reply(
                &replacement,
                entity,
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                entity: denied,
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            } if denied == entity
        ));
        router.park_release.notify_one();
        let (old, stale_reply) = tokio::time::timeout(Duration::from_secs(5), old_claim)
            .await
            .expect("released claim returns")
            .expect("claim task does not panic");
        assert!(matches!(
            stale_reply,
            LeaseMsg::Deny {
                entity: denied,
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            } if denied == entity
        ));
        assert!(router.live.lock().await.is_none());
        assert_eq!(router.parked.lock().await.as_slice(), &[lease_id]);
        let replacement_heartbeat =
            heartbeat_reply(&replacement, vec![lease_id], Tick::new(2)).await;
        assert!(replacement_heartbeat.0.is_empty());
        assert_eq!(replacement_heartbeat.1, vec![lease_id]);
        assert!(matches!(
            claim_reply(
                &replacement,
                entity,
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Grant {
                entity: granted,
                lease_id: granted_lease,
                ..
            } if granted == entity && granted_lease == lease_id
        ));
        assert!(router.live.lock().await.as_ref().is_some_and(|lease| {
            lease.entity == entity && lease.holder == Some(node(1)) && lease.lease_id == lease_id
        }));
        let replacement_heartbeat =
            heartbeat_reply(&replacement, vec![lease_id], Tick::new(3)).await;
        assert_eq!(
            replacement_heartbeat.0.first().map(|lease| (
                lease.entity,
                lease.holder,
                lease.lease_id
            )),
            Some((entity, Some(node(1)), lease_id))
        );
        assert!(replacement_heartbeat.1.is_empty());

        drop(old);
        old_client.close().await;
        router.park_release.notify_one();
        drop(replacement);
        replacement_client.close().await;
        server.shutdown().await;
    });
}

#[test]
fn pending_heartbeat_releases_peer_state_and_stale_session_gets_no_current_rows() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: an authenticated raw gateway session owns a lease whose heartbeat blocks.
        let entity = PersistId::new(491);
        let lease_id = LeaseId(491);
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
        let router = Arc::new(BlockingHeartbeatRouter {
            entered: entered_tx,
            release: tokio::sync::Notify::new(),
            block_once: AtomicBool::new(true),
            entity,
            holder: node(1),
            lease_id,
        });
        let server = GatewayServer::spawn(
            support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT]),
            router.clone(),
        )
        .await
        .unwrap();
        let (old_client, old) = raw_connection(secret(1), server.addr()).await;
        old.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })))
        .unwrap();
        assert!(receives_hello_ack(&old).await);
        assert!(matches!(
            claim_reply(
                &old,
                entity,
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Grant { lease_id: granted, .. } if granted == lease_id
        ));
        let old_heartbeat = tokio::spawn(async move {
            let reply = heartbeat_reply(&old, vec![lease_id], Tick::new(2)).await;
            (old, reply)
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("heartbeat reaches the controlled router")
            .expect("heartbeat route reports entry");

        // When: a replacement for the same peer authenticates while renewal is blocked.
        let (replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();

        // Then: it promptly owns the registry, while the released stale reply is invalidated.
        assert!(receives_hello_ack(&replacement).await);
        router.release.notify_one();
        let (old, stale_heartbeat) = tokio::time::timeout(Duration::from_secs(5), old_heartbeat)
            .await
            .expect("released heartbeat returns")
            .expect("heartbeat task does not panic");
        assert!(stale_heartbeat.0.is_empty());
        assert_eq!(stale_heartbeat.1, vec![lease_id]);
        let replacement_heartbeat =
            heartbeat_reply(&replacement, vec![lease_id], Tick::new(3)).await;
        assert_eq!(
            replacement_heartbeat.0.first().map(|row| row.lease_id),
            Some(lease_id)
        );
        assert!(replacement_heartbeat.1.is_empty());

        drop(old);
        old_client.close().await;
        drop(replacement);
        replacement_client.close().await;
        server.shutdown().await;
    });
}

#[test]
fn gateway_rate_limits_claims_by_retained_node_id() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap(),
        ));
        for entity in 1..=86 {
            seed_entity(&runtime, PersistId::new(entity), CellId::ROOT).await;
        }
        let claim_clock = Arc::new(AtomicClaimClock(AtomicU64::new(0)));
        let server = GatewayServer::spawn(
            GatewayConfig {
                claim_clock: claim_clock.clone(),
                interest_authority: support::interest_authority([
                    support::interest_snapshot(node(1), GridId::ROOT, vec![CellId::ROOT]),
                    support::interest_snapshot(node(2), GridId::ROOT, vec![CellId::ROOT]),
                ]),
                ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
            },
            runtime.clone(),
        )
        .await
        .unwrap();

        let (first_client, first) = raw_connection(secret(1), server.addr()).await;
        first
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&first).await);
        for entity in 1..=32 {
            assert!(matches!(
                claim_reply(
                    &first,
                    PersistId::new(entity),
                    GridId::ROOT,
                    CellId::ROOT,
                    ClaimKind::Weak,
                    ClaimBasis::Explicit,
                )
                .await,
                LeaseMsg::Grant { .. }
            ));
        }

        let (replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&replacement).await);
        for entity in 33..=64 {
            assert!(matches!(
                claim_reply(
                    &replacement,
                    PersistId::new(entity),
                    GridId::ROOT,
                    CellId::ROOT,
                    ClaimKind::Weak,
                    ClaimBasis::Explicit,
                )
                .await,
                LeaseMsg::Grant { .. }
            ));
        }
        assert!(matches!(
            claim_reply(
                &replacement,
                PersistId::new(65),
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                reason: orrery_protocol::DenyReason::RateLimited,
                retry_after_ms: 50,
                ..
            }
        ));

        claim_clock.0.store(1_000, Ordering::SeqCst);
        for entity in 65..=84 {
            assert!(matches!(
                claim_reply(
                    &replacement,
                    PersistId::new(entity),
                    GridId::ROOT,
                    CellId::ROOT,
                    ClaimKind::Weak,
                    ClaimBasis::Explicit,
                )
                .await,
                LeaseMsg::Grant { .. }
            ));
        }
        assert!(matches!(
            claim_reply(
                &replacement,
                PersistId::new(85),
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                reason: orrery_protocol::DenyReason::RateLimited,
                retry_after_ms: 50,
                ..
            }
        ));

        let (other_client, other) = raw_connection(secret(2), server.addr()).await;
        other
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(2)),
                node: node(2),
            })))
            .unwrap();
        assert!(receives_hello_ack(&other).await);
        assert!(matches!(
            claim_reply(
                &other,
                PersistId::new(86),
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Grant { .. }
        ));

        drop(first);
        first_client.close().await;
        drop(replacement);
        replacement_client.close().await;
        drop(other);
        other_client.close().await;
        server.shutdown().await;
    });
}

#[test]
fn gateway_replacement_session_exclusively_owns_inherited_leases() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: one authenticated peer has a live lease on the raw gateway.
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap(),
        ));
        let entity = PersistId::new(501);
        seed_entity(&runtime, entity, CellId::ROOT).await;
        let server = GatewayServer::spawn(
            support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT]),
            runtime.clone(),
        )
        .await
        .unwrap();
        let (old_client, old) = raw_connection(secret(1), server.addr()).await;
        old.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })))
        .unwrap();
        assert!(receives_hello_ack(&old).await);
        let LeaseMsg::Grant { lease_id, seq, .. } = claim_reply(
            &old,
            entity,
            GridId::ROOT,
            CellId::ROOT,
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await
        else {
            panic!("initial session must receive a lease");
        };

        // When: a second connection with the same verified NodeId replaces it.
        let (new_client, new) = raw_connection(secret(1), server.addr()).await;
        new.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })))
        .unwrap();
        assert!(receives_hello_ack(&new).await);
        new.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat {
                lease_ids: vec![lease_id],
                tick: Tick::new(2),
            },
        })))
        .unwrap();
        let renewed = loop {
            let packet = tokio::time::timeout(Duration::from_secs(5), new.read_datagram())
                .await
                .unwrap()
                .unwrap();
            if let Some(GatewayReply::Lease {
                message: LeaseMsg::HeartbeatAck { leases, invalid },
            }) = decode_stream_frame(&packet)
            {
                break (leases, invalid);
            }
        };
        assert!(renewed.1.is_empty());
        assert_eq!(renewed.0.first().map(|row| row.lease_id), Some(lease_id));

        // Then: the superseded generation can neither renew nor write.
        old.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat {
                lease_ids: vec![lease_id],
                tick: Tick::new(3),
            },
        })))
        .unwrap();
        let old_heartbeat = loop {
            let packet = tokio::time::timeout(Duration::from_secs(5), old.read_datagram())
                .await
                .unwrap()
                .unwrap();
            if let Some(GatewayReply::Lease {
                message: LeaseMsg::HeartbeatAck { leases, invalid },
            }) = decode_stream_frame(&packet)
            {
                break (leases, invalid);
            }
        };
        assert!(old_heartbeat.0.is_empty());
        assert_eq!(old_heartbeat.1, vec![lease_id]);
        assert!(matches!(
            claim_reply(
                &old,
                entity,
                GridId::ROOT,
                CellId::ROOT,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                entity: denied,
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            } if denied == entity
        ));
        old.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(3),
                kind: RecordKind::Spawn,
                payload: Bytes::from_static(b"superseded"),
                seq: 3,
                lease_id: Some(lease_id),
                authority_seq: Some(seq),
            },
        })))
        .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(5), old.read_datagram())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            decode_datagram(&packet),
            Some(GatewayReply::BulkNack { entity: denied, .. }) if denied == entity
        ));

        // Closing the old generation must not park the lease transferred to the new one.
        drop(old);
        old_client.close().await;
        new.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat {
                lease_ids: vec![lease_id],
                tick: Tick::new(4),
            },
        })))
        .unwrap();
        let post_close_renewal = loop {
            let packet = tokio::time::timeout(Duration::from_secs(5), new.read_datagram())
                .await
                .unwrap()
                .unwrap();
            if let Some(GatewayReply::Lease {
                message: LeaseMsg::HeartbeatAck { leases, invalid },
            }) = decode_stream_frame(&packet)
            {
                break (leases, invalid);
            }
        };
        assert!(post_close_renewal.1.is_empty());
        assert_eq!(
            post_close_renewal
                .0
                .first()
                .map(|row| (row.lease_id, row.seq)),
            Some((lease_id, seq))
        );
        new.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(5),
                kind: RecordKind::Spawn,
                payload: Bytes::from_static(b"replacement"),
                seq: 5,
                lease_id: Some(lease_id),
                authority_seq: Some(seq),
            },
        })))
        .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(5), new.read_datagram())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            decode_datagram(&packet),
            Some(GatewayReply::BulkAck { entity: accepted, .. }) if accepted == entity
        ));

        // Closing the current generation parks its inherited lease exactly once.
        drop(new);
        new_client.close().await;
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, CellId::ROOT)
            .unwrap()
            .clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let parked = loop {
            let row = actor
                .validate_lease(entity, node(1), lease_id, 0)
                .await
                .unwrap()
                .expect("parked row remains observable");
            if row.holder.is_none() {
                break row;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        let unchanged = actor
            .validate_lease(entity, node(1), lease_id, 0)
            .await
            .unwrap()
            .expect("parked row remains observable");
        assert_eq!(unchanged.lease_id, parked.lease_id);
        assert_eq!(unchanged.seq, parked.seq);
        server.shutdown().await;
    });
}

#[test]
fn gateway_peer_registry_rejects_capacity_then_evicts_expired_idle_peer() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: the bounded registry is full with one established peer.
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap(),
        ));
        let clock = Arc::new(AtomicGatewayClock(AtomicU64::new(1_000)));
        let issuer = secret(42);
        let server = GatewayServer::spawn(
            GatewayConfig {
                peer_registry_capacity: 1,
                peer_idle_retention_ms: 10,
                identity_clock: clock.clone(),
                authorizer: support::authorizer(&issuer),
                interest_authority: support::interest_authority([
                    support::interest_snapshot(node(1), GridId::ROOT, vec![CellId::ROOT]),
                    support::interest_snapshot(node(2), GridId::ROOT, vec![CellId::ROOT]),
                ]),
                ..GatewayConfig::default()
            },
            runtime,
        )
        .await
        .unwrap();
        let (first_client, first) = raw_connection(secret(1), server.addr()).await;
        first
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&first).await);
        let (_second_client, second) = raw_connection(secret(2), server.addr()).await;

        // When: another NodeId authenticates while the sole slot is occupied.
        second
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&issuer, node(2), 900, 200),
                node: node(2),
            })))
            .unwrap();

        // Then: capacity rejects it without replacing the established peer.
        assert!(!receives_hello_ack(&second).await);

        // When: the established peer disconnects and its idle retention elapses.
        drop(first);
        first_client.close().await;
        clock.0.store(1_011, Ordering::SeqCst);

        // Then: a later authentication evicts the stale entry and takes the slot.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            second
                .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                    token: session_token(&issuer, node(2), 900, 200),
                    node: node(2),
                })))
                .unwrap();
            if receives_hello_ack(&second).await {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        server.shutdown().await;
    });
}

#[test]
fn gateway_lease_capacity_denies_before_actor_mutation_after_reconnect() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: one NodeId owns the only live-lease slot through a real gateway and actor.
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap(),
        ));
        let first_entity = PersistId::new(8_901);
        let rejected_entity = PersistId::new(8_902);
        seed_entity(&runtime, first_entity, CellId::ROOT).await;
        seed_entity(&runtime, rejected_entity, CellId::ROOT).await;
        let server = GatewayServer::spawn(
            GatewayConfig {
                peer_lease_capacity: 1,
                ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
            },
            runtime.clone(),
        )
        .await
        .unwrap();
        let (old_client, old) = raw_connection(secret(1), server.addr()).await;
        old.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })))
        .unwrap();
        assert!(receives_hello_ack(&old).await);
        let LeaseMsg::Grant { lease_id, .. } = claim_reply(
            &old,
            first_entity,
            GridId::ROOT,
            CellId::ROOT,
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await
        else {
            panic!("first live lease must be granted");
        };

        // When: a replacement generation reconnects and claims another eligible entity.
        let (_replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&replacement).await);
        let inherited = heartbeat_reply(&replacement, vec![lease_id], Tick::new(1)).await;
        assert!(inherited.1.is_empty());
        assert_eq!(inherited.0.first().map(|row| row.lease_id), Some(lease_id));
        let journal_before = journal_len(&runtime).await;
        let denied = claim_reply(
            &replacement,
            rejected_entity,
            GridId::ROOT,
            CellId::ROOT,
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await;

        // Then: capacity refuses before the actor creates a second lease or journal mutation.
        assert!(matches!(
            denied,
            LeaseMsg::Deny {
                entity,
                reason: orrery_protocol::DenyReason::NotEligible,
                retry_after_ms: 0,
                ..
            } if entity == rejected_entity
        ));
        assert_eq!(journal_len(&runtime).await, journal_before);
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, CellId::ROOT)
            .unwrap()
            .clone();
        assert!(actor
            .validate_lease(first_entity, node(1), lease_id, 0)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            actor
                .validate_lease(rejected_entity, node(1), LeaseId(0), 0)
                .await
                .unwrap(),
            None
        );

        drop(old);
        old_client.close().await;
        server.shutdown().await;
    });
}

#[test]
fn gateway_rejects_client_rekey_without_mutation() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: an authenticated holder, a committed source location, and a
        // durable actor record reached through the real raw gateway surface.
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap(),
        ));
        let cells = CellId::ROOT.children();
        let source = cells[0];
        let destination = cells[1];
        let entity = PersistId::new(8_811);
        seed_entity(&runtime, entity, source).await;
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: support::interest_authority([support::interest_snapshot(
                    node(1),
                    GridId::ROOT,
                    vec![source],
                )]),
                ..support::authority_config(node(1), GridId::ROOT, vec![source])
            },
            runtime.clone(),
        )
        .await
        .unwrap();
        let (_client, connection) = raw_connection(secret(1), server.addr()).await;
        connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&connection).await);
        let LeaseMsg::Grant { lease_id, seq, .. } = claim_reply(
            &connection,
            entity,
            GridId::ROOT,
            source,
            ClaimKind::Strong,
            ClaimBasis::Explicit,
        )
        .await
        else {
            panic!("seeded entity must receive its source lease");
        };
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, CellId::ROOT)
            .unwrap()
            .clone();
        let snapshot_before = actor.read_snapshot(vec![CellId::ROOT]).await.unwrap();
        let journal_count_before = runtime
            .lock()
            .await
            .journal()
            .scan_from(Lsn::new(0, 0))
            .count();

        // When: the client repeatedly sends the legacy control-lane rekey.
        for _ in 0..3 {
            connection
                .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Lease {
                    message: LeaseMsg::Rekey {
                        entity,
                        old_cell: source,
                        new_cell: destination,
                    },
                })))
                .unwrap();
        }

        // Then: the gateway does not activate movement authority, and every
        // durable/hot observable remains exactly as it was.
        for _ in 0..3 {
            let packet = tokio::time::timeout(Duration::from_secs(1), connection.read_datagram())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                decode_stream_frame(&packet),
                Some(GatewayReply::Lease {
                    message: LeaseMsg::Deny {
                        entity: denied,
                        reason: orrery_protocol::DenyReason::NotEligible,
                        ..
                    }
                }) if denied == entity
            ));
        }
        let rekey_payload = postcard::to_allocvec(&EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity,
            source_grid: GridId::ROOT,
            source_cell: source,
            destination_grid: GridId::ROOT,
            destination_cell: destination,
            expected_lease_id: lease_id,
            source_record: snapshot_before.entities[&entity].components.clone(),
        })
        .unwrap();
        connection
            .send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
                diff: DiffUplink {
                    cell: source,
                    grid: GridId::ROOT,
                    entity,
                    tick: Tick::new(9),
                    kind: RecordKind::Rekey,
                    payload: Bytes::from(rekey_payload),
                    seq: 9,
                    lease_id: Some(lease_id),
                    authority_seq: Some(seq),
                },
            })))
            .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(1), connection.read_datagram())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            decode_datagram(&packet),
            Some(GatewayReply::BulkNack {
                entity: denied,
                tick,
                lease: None,
                ..
            }) if denied == entity && tick == Tick::new(9)
        ));
        assert_eq!(
            runtime.lock().await.lease_location(entity).await.unwrap(),
            Some(source)
        );
        let current = actor
            .validate_lease(entity, node(1), lease_id, 0)
            .await
            .unwrap()
            .expect("source lease remains indexed");
        assert_eq!((current.lease_id, current.seq), (lease_id, seq));
        assert_eq!(
            actor
                .read_snapshot(vec![CellId::ROOT])
                .await
                .unwrap()
                .entities,
            snapshot_before.entities
        );
        assert_eq!(
            runtime
                .lock()
                .await
                .journal()
                .scan_from(Lsn::new(0, 0))
                .count(),
            journal_count_before
        );

        server.shutdown().await;
    });
}

#[test]
fn rekeyed_entity_rejects_stale_presented_cell_with_current_lease() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: an authenticated strong holder whose entity is server-rekeyed after claim.
        let dir = tempfile::tempdir().unwrap();
        let checkpoints: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &checkpoints)
                .await
                .unwrap(),
        ));
        let cells = CellId::ROOT.children();
        let source = cells[0];
        let destination = cells[1];
        let entity = PersistId::new(8_812);
        seed_entity(&runtime, entity, source).await;
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: support::interest_authority([support::interest_snapshot(
                    node(1),
                    GridId::ROOT,
                    vec![source, destination],
                )]),
                ..support::authority_config(node(1), GridId::ROOT, vec![source, destination])
            },
            runtime.clone(),
        )
        .await
        .unwrap();
        let (_client, connection) = raw_connection(secret(1), server.addr()).await;
        connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&connection).await);
        let LeaseMsg::Grant { lease_id, seq, .. } = claim_reply(
            &connection,
            entity,
            GridId::ROOT,
            source,
            ClaimKind::Strong,
            ClaimBasis::Explicit,
        )
        .await
        else {
            panic!("seeded entity must receive its source lease");
        };
        let rekey = EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity,
            source_grid: GridId::ROOT,
            source_cell: source,
            destination_grid: GridId::ROOT,
            destination_cell: destination,
            expected_lease_id: lease_id,
            source_record: Bytes::from_static(b"seeded"),
        };
        let payload = Bytes::from(postcard::to_allocvec(&rekey).unwrap());
        Router::commit_rekey(
            runtime.as_ref(),
            JournalRecord {
                lsn: Lsn::new(0, 0),
                cell: source,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(7),
                epoch: Epoch::new(0),
                author: node(9),
                kind: RecordKind::Rekey,
                crc: payload_crc(&payload),
                payload,
            },
        )
        .await
        .unwrap();
        let journal_before = runtime
            .lock()
            .await
            .journal()
            .scan_from(Lsn::new(0, 0))
            .count();

        // When: the same holder presents its current fence with the obsolete source cell.
        connection
            .send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
                diff: DiffUplink {
                    cell: source,
                    grid: GridId::ROOT,
                    entity,
                    tick: Tick::new(8),
                    kind: RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"must-not-journal"),
                    seq: 8,
                    lease_id: Some(lease_id),
                    authority_seq: Some(seq),
                },
            })))
            .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
            .await
            .unwrap()
            .unwrap();

        // Then: the wire NACK carries the destination lease and actor/journal state is unchanged.
        let Some(GatewayReply::BulkNack {
            entity: rejected,
            tick,
            lease: Some(current),
            ..
        }) = decode_datagram(&packet)
        else {
            panic!("stale cell must receive a lease-bearing BulkNack");
        };
        assert_eq!((rejected, tick), (entity, Tick::new(8)));
        assert_eq!((current.lease_id, current.seq), (lease_id, seq));
        assert_eq!(
            runtime.lock().await.lease_location(entity).await.unwrap(),
            Some(destination)
        );
        let actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, destination)
            .unwrap()
            .clone();
        assert!(!actor
            .read_snapshot(vec![source])
            .await
            .unwrap()
            .entities
            .contains_key(&entity));
        assert_eq!(
            actor
                .read_snapshot(vec![destination])
                .await
                .unwrap()
                .entities[&entity]
                .components
                .as_ref(),
            b"seeded"
        );
        assert_eq!(
            runtime
                .lock()
                .await
                .journal()
                .scan_from(Lsn::new(0, 0))
                .count(),
            journal_before
        );

        // When: the holder retries with the same current fence at the committed destination.
        connection
            .send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
                diff: DiffUplink {
                    cell: destination,
                    grid: GridId::ROOT,
                    entity,
                    tick: Tick::new(9),
                    kind: RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"destination-applied"),
                    seq: 9,
                    lease_id: Some(lease_id),
                    authority_seq: Some(seq),
                },
            })))
            .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
            .await
            .unwrap()
            .unwrap();

        // Then: the live gateway acknowledges durability and only the destination advances.
        assert!(matches!(
            decode_datagram(&packet),
            Some(GatewayReply::BulkAck {
                entity: accepted,
                tick,
                ..
            }) if accepted == entity && tick == Tick::new(9)
        ));
        assert!(!actor
            .read_snapshot(vec![source])
            .await
            .unwrap()
            .entities
            .contains_key(&entity));
        assert_eq!(
            actor
                .read_snapshot(vec![destination])
                .await
                .unwrap()
                .entities[&entity]
                .components
                .as_ref(),
            b"destination-applied"
        );
        assert_eq!(
            runtime
                .lock()
                .await
                .journal()
                .scan_from(Lsn::new(0, 0))
                .count(),
            journal_before + 1
        );

        server.shutdown().await;
    });
}

#[test]
fn reviewed_authority_narrative() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: committed entities and one coordinator interest snapshot on a real iroh gateway.
        let dir = tempfile::tempdir().unwrap();
        let checkpoints: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(&runtime_config(dir.path()), &checkpoints)
                .await
                .unwrap(),
        ));
        let cells = CellId::ROOT.children();
        let source = cells[0];
        let destination = cells[1];
        let outside_interest = cells[2];
        let entity = PersistId::new(10_000);
        let stale_token_entity = PersistId::new(10_001);
        let wrong_interest_entity = PersistId::new(10_002);
        let missing_interest_entity = PersistId::new(10_003);
        for (seeded, cell) in [
            (entity, source),
            (stale_token_entity, source),
            (wrong_interest_entity, outside_interest),
            (missing_interest_entity, source),
        ] {
            seed_entity(&runtime, seeded, cell).await;
        }
        for id in 10_100..=10_162 {
            seed_entity(&runtime, PersistId::new(id), source).await;
        }
        let claim_clock = Arc::new(AtomicClaimClock(AtomicU64::new(0)));
        let server = GatewayServer::spawn(
            GatewayConfig {
                claim_clock,
                interest_authority: support::interest_authority([support::interest_snapshot(
                    node(1),
                    GridId::ROOT,
                    vec![source, destination],
                )]),
                ..support::authority_config(node(1), GridId::ROOT, vec![source, destination])
            },
            runtime.clone(),
        )
        .await
        .unwrap();

        // When: an expired signed credential tries to activate and write.
        let (stale_client, stale_connection) = raw_connection(secret(3), server.addr()).await;
        let stale_journal = journal_len(&runtime).await;
        stale_connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: session_token(&secret(42), node(3), 1, 1),
                node: node(3),
            })))
            .unwrap();
        assert!(!receives_hello_ack(&stale_connection).await);
        assert!(matches!(
            diff_reply(
                &stale_connection,
                DiffUplink {
                    cell: source,
                    grid: GridId::ROOT,
                    entity: stale_token_entity,
                    tick: Tick::new(1),
                    kind: RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"stale-token"),
                    seq: 1,
                    lease_id: Some(orrery_protocol::LeaseId(1)),
                    authority_seq: Some(Default::default()),
                },
            )
            .await,
            GatewayReply::BulkNack {
                entity: denied,
                lease: None,
                ..
            } if denied == stale_token_entity
        ));
        assert_eq!(journal_len(&runtime).await, stale_journal);

        // When: a valid signed token is paired with missing coordinator interest.
        let (missing_client, missing_connection) = raw_connection(secret(2), server.addr()).await;
        missing_connection
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(2)),
                node: node(2),
            })))
            .unwrap();
        assert!(receives_hello_ack(&missing_connection).await);
        let missing_journal = journal_len(&runtime).await;
        assert!(matches!(
            claim_reply(
                &missing_connection,
                missing_interest_entity,
                GridId::ROOT,
                source,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                entity: denied,
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            } if denied == missing_interest_entity
        ));
        assert_eq!(journal_len(&runtime).await, missing_journal);

        // When: the reviewed holder presents a valid token but the wrong interest cell.
        let (old_client, old) = raw_connection(secret(1), server.addr()).await;
        old.send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })))
        .unwrap();
        assert!(receives_hello_ack(&old).await);
        let wrong_interest_journal = journal_len(&runtime).await;
        assert!(matches!(
            claim_reply(
                &old,
                wrong_interest_entity,
                GridId::ROOT,
                outside_interest,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                entity: denied,
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            } if denied == wrong_interest_entity
        ));
        assert_eq!(journal_len(&runtime).await, wrong_interest_journal);

        // When: the holder claims, writes with its fence, and heartbeats.
        let LeaseMsg::Grant { lease_id, seq, .. } = claim_reply(
            &old,
            entity,
            GridId::ROOT,
            source,
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await
        else {
            panic!("reviewed claim must grant");
        };
        assert!(matches!(
            diff_reply(
                &old,
                DiffUplink {
                    cell: source,
                    grid: GridId::ROOT,
                    entity,
                    tick: Tick::new(2),
                    kind: RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"source-write"),
                    seq: 2,
                    lease_id: Some(lease_id),
                    authority_seq: Some(seq),
                },
            )
            .await,
            GatewayReply::BulkAck {
                entity: accepted,
                tick,
                ..
            } if accepted == entity && tick == Tick::new(2)
        ));
        let journal_after_fenced_write = journal_len(&runtime).await;
        let heartbeat = heartbeat_reply(&old, vec![lease_id], Tick::new(3)).await;
        assert!(heartbeat.1.is_empty());
        assert_eq!(heartbeat.0.first().map(|row| row.lease_id), Some(lease_id));
        assert_eq!(journal_len(&runtime).await, journal_after_fenced_write);

        // When: a replacement connection inherits the lease generation.
        let (replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_datagram(Bytes::from(encode_stream_frame(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })))
            .unwrap();
        assert!(receives_hello_ack(&replacement).await);
        let replacement_heartbeat =
            heartbeat_reply(&replacement, vec![lease_id], Tick::new(4)).await;
        assert!(replacement_heartbeat.1.is_empty());
        assert_eq!(
            replacement_heartbeat
                .0
                .first()
                .map(|row| (row.lease_id, row.seq)),
            Some((lease_id, seq))
        );

        // Then: the old generation gets definitive control and bulk denials with no append.
        let superseded_journal = journal_len(&runtime).await;
        let old_heartbeat = heartbeat_reply(&old, vec![lease_id], Tick::new(5)).await;
        assert!(old_heartbeat.0.is_empty());
        assert_eq!(old_heartbeat.1, vec![lease_id]);
        assert!(matches!(
            diff_reply(
                &old,
                DiffUplink {
                    cell: source,
                    grid: GridId::ROOT,
                    entity,
                    tick: Tick::new(5),
                    kind: RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"superseded"),
                    seq: 5,
                    lease_id: Some(lease_id),
                    authority_seq: Some(seq),
                },
            )
            .await,
            GatewayReply::BulkNack {
                entity: denied,
                ..
            } if denied == entity
        ));
        assert_eq!(journal_len(&runtime).await, superseded_journal);

        // Then: the D16 burst budget remains aggregate across the replacement generation.
        for id in 10_100..=10_161 {
            assert!(matches!(
                claim_reply(
                    &replacement,
                    PersistId::new(id),
                    GridId::ROOT,
                    source,
                    ClaimKind::Weak,
                    ClaimBasis::Explicit,
                )
                .await,
                LeaseMsg::Grant { .. }
            ));
        }
        assert!(matches!(
            claim_reply(
                &replacement,
                PersistId::new(10_162),
                GridId::ROOT,
                source,
                ClaimKind::Weak,
                ClaimBasis::Explicit,
            )
            .await,
            LeaseMsg::Deny {
                entity: denied,
                reason: orrery_protocol::DenyReason::RateLimited,
                retry_after_ms: 50,
                ..
            } if denied == PersistId::new(10_162)
        ));

        // When: trusted persistence commits a cross-cell rekey for the current fence.
        let rekey = EntityRekey {
            version: ENTITY_REKEY_VERSION,
            entity,
            source_grid: GridId::ROOT,
            source_cell: source,
            destination_grid: GridId::ROOT,
            destination_cell: destination,
            expected_lease_id: lease_id,
            source_record: Bytes::from_static(b"source-write"),
        };
        let rekey_payload = Bytes::from(postcard::to_allocvec(&rekey).unwrap());
        Router::commit_rekey(
            runtime.as_ref(),
            JournalRecord {
                lsn: Lsn::new(0, 0),
                cell: source,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(6),
                epoch: Epoch::new(0),
                author: node(9),
                kind: RecordKind::Rekey,
                crc: payload_crc(&rekey_payload),
                payload: rekey_payload,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            runtime.lock().await.lease_location(entity).await.unwrap(),
            Some(destination)
        );

        // Then: the old cell is lease-specifically NACKed without an append.
        let old_cell_journal = journal_len(&runtime).await;
        let old_cell_reply = diff_reply(
            &replacement,
            DiffUplink {
                cell: source,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(7),
                kind: RecordKind::ComponentDiff,
                payload: Bytes::from_static(b"old-cell"),
                seq: 7,
                lease_id: Some(lease_id),
                authority_seq: Some(seq),
            },
        )
        .await;
        assert!(matches!(
            old_cell_reply,
            GatewayReply::BulkNack {
                entity: denied,
                lease: Some(ref current),
                ..
            } if denied == entity && current.lease_id == lease_id && current.seq == seq
        ));
        assert_eq!(journal_len(&runtime).await, old_cell_journal);

        // When: the same fence writes to the committed destination.
        assert!(matches!(
            diff_reply(
                &replacement,
                DiffUplink {
                    cell: destination,
                    grid: GridId::ROOT,
                    entity,
                    tick: Tick::new(8),
                    kind: RecordKind::ComponentDiff,
                    payload: Bytes::from_static(b"destination-write"),
                    seq: 8,
                    lease_id: Some(lease_id),
                    authority_seq: Some(seq),
                },
            )
            .await,
            GatewayReply::BulkAck {
                entity: accepted,
                tick,
                ..
            } if accepted == entity && tick == Tick::new(8)
        ));

        // When: the holder goes silent through lease expiry, then retries its old fence.
        Router::sweep_expired_leases(runtime.as_ref(), u64::MAX).await;
        let expired_journal = journal_len(&runtime).await;
        let expired_reply = diff_reply(
            &replacement,
            DiffUplink {
                cell: destination,
                grid: GridId::ROOT,
                entity,
                tick: Tick::new(9),
                kind: RecordKind::ComponentDiff,
                payload: Bytes::from_static(b"expired"),
                seq: 9,
                lease_id: Some(lease_id),
                authority_seq: Some(seq),
            },
        )
        .await;
        let GatewayReply::BulkNack {
            entity: denied,
            lease: Some(parked),
            ..
        } = expired_reply
        else {
            panic!("silent expiry must return the parked current lease");
        };
        assert_eq!(denied, entity);
        assert!(parked.holder.is_none());
        assert!(parked.lease_id > lease_id);
        assert_eq!(journal_len(&runtime).await, expired_journal);
        let destination_actor = runtime
            .lock()
            .await
            .actor(GridId::ROOT, destination)
            .unwrap()
            .clone();
        assert_eq!(
            destination_actor
                .read_snapshot(vec![destination])
                .await
                .unwrap()
                .entities[&entity]
                .components
                .as_ref(),
            b"destination-write"
        );

        drop(old);
        old_client.close().await;
        drop(replacement);
        replacement_client.close().await;
        drop(stale_connection);
        stale_client.close().await;
        drop(missing_connection);
        missing_client.close().await;
        server.shutdown().await;
    });
}
