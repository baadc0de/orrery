//! End-to-end test of the persistd gateway against a client speaking the same
//! wire surface the `aeronet_iroh` gateway client uses: the admission
//! uni-stream, then tagged datagrams carrying `GatewayMsg`s (docs/11-roadmap.md
//! §P2).
//!
//! This closes the loop the client-side pipeline test (docs/10-crates.md §9,
//! `tests/client_pipeline.rs`) proves from the other side — a real
//! client → gateway → cell-actor path — but Bevy-free, using the raw iroh
//! endpoint directly (D15).

mod lanes;
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
use orrery_protocol::channels::{decode_datagram, decode_stream_frame};
use orrery_protocol::{
    Attestation, CellEpoch, CellId, ClaimBasis, ClaimKind, CoordinatorInterestSnapshot, DiffUplink,
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
            bound_to: None,
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
            bound_to: None,
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
            bound_to: None,
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

/// Dial the gateway, complete admission, and start draining both lanes.
///
/// The endpoint is returned alongside because dropping it closes the
/// connection.
async fn raw_connection(
    key: iroh_base::SecretKey,
    address: iroh::EndpointAddr,
) -> (iroh::Endpoint, lanes::GatewayLanes) {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(key)
        .bind()
        .await
        .unwrap();
    let connection = endpoint.connect(address, GATEWAY_ALPN).await.unwrap();
    // Read admission before attaching: the lane reader accepts every inbound
    // stream from that point on, and would otherwise consume it.
    let mut admission = connection.accept_uni().await.unwrap();
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0u8]);
    (endpoint, lanes::GatewayLanes::attach(connection))
}

async fn seed_entity(runtime: &Arc<Mutex<CellRuntime>>, entity: PersistId, cell: CellId) {
    let actor = runtime
        .lock()
        .await
        .actor(GridId::ROOT, cell)
        .expect("actor for seeded entity");
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
    connection: &lanes::GatewayLanes,
    entity: PersistId,
    grid: GridId,
    cell: CellId,
    kind: ClaimKind,
    basis: ClaimBasis,
) -> LeaseMsg {
    connection
        .send_control(&GatewayMsg::Lease {
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
        })
        .await;
    loop {
        let packet = connection
            .next_payload(Duration::from_secs(5))
            .await
            .unwrap();
        if let Some(GatewayReply::Lease { message }) = decode_stream_frame(&packet) {
            return message;
        }
    }
}

async fn receives_hello_ack(connection: &lanes::GatewayLanes) -> bool {
    matches!(
        connection.next_payload(Duration::from_millis(150)).await,
        Some(packet)
            if matches!(decode_stream_frame(&packet), Some(GatewayReply::HelloAck { .. }))
    )
}

async fn heartbeat_reply(
    connection: &lanes::GatewayLanes,
    renew: Vec<(PersistId, orrery_protocol::LeaseId)>,
    tick: Tick,
) -> (
    Vec<orrery_protocol::Lease>,
    Vec<(PersistId, orrery_protocol::LeaseId)>,
) {
    connection
        .send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat { renew, tick },
        })
        .await;
    loop {
        let packet = connection
            .next_payload(Duration::from_secs(5))
            .await
            .unwrap();
        if let Some(GatewayReply::Lease {
            message: LeaseMsg::HeartbeatAck { leases, invalid },
        }) = decode_stream_frame(&packet)
        {
            return (leases, invalid);
        }
    }
}

async fn diff_reply(connection: &lanes::GatewayLanes, diff: DiffUplink) -> GatewayReply {
    connection.send_state(&GatewayMsg::Diff { diff });
    loop {
        let packet = connection
            .next_payload(Duration::from_secs(5))
            .await
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
    connection: &lanes::GatewayLanes,
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
    connection.send_state(&GatewayMsg::Diff {
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
    });
    let packet = connection
        .next_payload(Duration::from_secs(1))
        .await
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
        cell_epoch: CellEpoch::new(0),
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
        // Read it before attaching, or the lane reader consumes it.
        let mut admission = conn.accept_uni().await.unwrap();
        let msg = admission.read_to_end(16).await.unwrap();
        assert_eq!(msg, vec![0u8]);
        let conn = lanes::GatewayLanes::attach(conn);

        // A claimed NodeId that does not equal the iroh-authenticated remote
        // identity must not activate lease traffic.
        conn.send_control(&GatewayMsg::Hello {
            token: b"bad-node".to_vec(),
            node: node(2),
        })
        .await;
        conn.send_control(&GatewayMsg::Lease {
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
        })
        .await;

        // A matching Hello binds the session and activates control traffic.
        conn.send_control(&GatewayMsg::Hello {
            token: session_token(&issuer, node(1), 900, 200),
            node: node(1),
        })
        .await;

        conn.send_control(&GatewayMsg::Lease {
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
        })
        .await;
        let grant = loop {
            let pkt = conn.next_payload(Duration::from_secs(5)).await.unwrap();
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
                .expect("root actor");
            actor
                .validate_lease(PersistId::new(999), node(1), orrery_protocol::LeaseId(0), 0)
                .await
                .unwrap()
        };
        assert!(ignored.is_none(), "mismatched Hello must not admit a claim");

        // Session-indexed heartbeats fan back to the owning actor and return
        // the renewed row on the typed reliable-control reply.
        conn.send_control(&GatewayMsg::Lease {
            message: orrery_protocol::LeaseMsg::Heartbeat {
                renew: vec![
                    (PersistId::new(1), grant.0),
                    // Never held: refused by entity, with its token echoed.
                    (PersistId::new(999), orrery_protocol::LeaseId(999)),
                ],
                tick: Tick::new(2),
            },
        })
        .await;
        let renewed = loop {
            let pkt = conn.next_payload(Duration::from_secs(5)).await.unwrap();
            let Some(GatewayReply::Lease {
                message: orrery_protocol::LeaseMsg::HeartbeatAck { leases, invalid },
            }) = decode_stream_frame(&pkt)
            else {
                continue;
            };
            break (leases, invalid);
        };
        assert_eq!(
            renewed.1,
            vec![(PersistId::new(999), orrery_protocol::LeaseId(999))]
        );
        assert_eq!(renewed.0.len(), 1);
        assert_eq!(renewed.0[0].lease_id, grant.0);
        assert_eq!(renewed.0[0].seq, grant.1);
        assert_eq!(renewed.0[0].holder, Some(node(1)));

        // A stale pair is rejected before reaching the actor and carries the
        // current registrar row so the holder can recover without a lookup.
        conn.send_state(&GatewayMsg::Diff {
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
        });
        let pkt = conn.next_payload(Duration::from_secs(5)).await.unwrap();
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
        conn.send_state(&GatewayMsg::Diff {
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
        });
        let pkt = conn.next_payload(Duration::from_secs(5)).await.unwrap();
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
        conn.send_state(&GatewayMsg::Diff {
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
        });

        // Intent — signed by the gateway's own identity (the fixture has no
        // executor configured, so the honest outcome is a rejection, never a
        // fake commit; the commit path is covered by tests/intent_commit.rs).
        let intent = signed_intent(7, &secret(1));
        conn.send_control(&GatewayMsg::SubmitIntent {
            intent: intent.clone(),
        })
        .await;

        // Subscribe to the (single) shard cell.
        conn.send_control(&GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![CellId::ROOT],
        })
        .await;

        // Collect the replies: expect a bulk ack, an area page, and an intent ack.
        let mut got_ack = false;
        let mut got_page = false;
        let mut got_intent = false;
        for _ in 0..8 {
            if got_ack && got_page && got_intent {
                break;
            }
            let Some(pkt) = conn.next_payload(Duration::from_secs(5)).await else {
                break;
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
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(1),
            })
            .await;
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
            .send_control(&GatewayMsg::Subscribe {
                grid: GridId::ROOT,
                cells: vec![cells[1]],
            })
            .await;
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

        // A claim in a grid this node serves no shard of is refused for a
        // different reason, and the difference matters. P-7 makes the same raw
        // cell id under another grid a different entity universe, so this is
        // not an ineligible claimant — it is a claimant at the wrong node
        // (docs/08-persistence.md §3.5), and telling it to back off would have
        // it wait out a node that can never answer.
        let misaddressed = claim_reply(
            &connection,
            covered,
            GridId::new(9),
            cells[0],
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await;
        assert!(
            matches!(
                misaddressed,
                LeaseMsg::Deny {
                    claim_id: Some(orrery_protocol::ClaimId(1)),
                    entity: denied_entity,
                    reason: orrery_protocol::DenyReason::WrongOwner {
                        grid,
                        shard,
                        owner: None,
                        ..
                    },
                    retry_after_ms: 0,
                } if denied_entity == covered && grid == GridId::new(9) && shard == cells[0]
            ),
            "an unserved grid must be answered as a wrong owner, got {misaddressed:?}"
        );

        let (_stale_client, stale_connection) = raw_connection(secret(2), server.addr()).await;
        stale_connection
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(2), 900, 200),
                node: node(2),
            })
            .await;
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
            .unwrap();
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
            runtime.actor(GridId::ROOT, CellId::ROOT).unwrap()
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
                .send_control(&GatewayMsg::Hello {
                    token,
                    node: node(1),
                })
                .await;
            assert!(!receives_hello_ack(&connection).await);

            let entity = PersistId::new(100 + u64::try_from(index).unwrap());
            connection
                .send_control(&GatewayMsg::Lease {
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
                })
                .await;
            assert!(!receives_hello_ack(&connection).await);
            assert!(actor
                .validate_lease(entity, node(1), orrery_protocol::LeaseId(0), 0)
                .await
                .unwrap()
                .is_none());

            assert_diff_denied_without_mutation(&connection, &actor, &runtime, entity).await;
        }

        connection
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(2),
            })
            .await;
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
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(1),
            })
            .await;
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
            .unwrap();
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
            .send_control(&GatewayMsg::Hello {
                token: token.clone(),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&first).await);

        clock.0.store(1_200, Ordering::SeqCst);
        health.0.store(false, Ordering::SeqCst);
        let (_grace_client, grace) = raw_connection(secret(1), server.addr()).await;
        grace
            .send_control(&GatewayMsg::Hello {
                token: token.clone(),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&grace).await);

        let (_new_client, newly_expired) = raw_connection(secret(2), server.addr()).await;
        newly_expired
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(2), 900, 200),
                node: node(2),
            })
            .await;
        assert!(!receives_hello_ack(&newly_expired).await);
        assert_diff_denied_without_mutation(&newly_expired, &actor, &runtime, PersistId::new(200))
            .await;

        let (_changed_client, changed_token) = raw_connection(secret(1), server.addr()).await;
        changed_token
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 901, 200),
                node: node(1),
            })
            .await;
        assert!(!receives_hello_ack(&changed_token).await);
        assert_diff_denied_without_mutation(&changed_token, &actor, &runtime, PersistId::new(201))
            .await;

        health.0.store(true, Ordering::SeqCst);
        let (_healthy_client, healthy) = raw_connection(secret(1), server.addr()).await;
        healthy
            .send_control(&GatewayMsg::Hello {
                token,
                node: node(1),
            })
            .await;
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
        old.send_control(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })
        .await;
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;

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
            heartbeat_reply(&replacement, vec![(entity, lease_id)], Tick::new(2)).await;
        assert!(replacement_heartbeat.0.is_empty());
        assert_eq!(replacement_heartbeat.1, vec![(entity, lease_id)]);
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
            heartbeat_reply(&replacement, vec![(entity, lease_id)], Tick::new(3)).await;
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
        old.send_control(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })
        .await;
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
            let reply = heartbeat_reply(&old, vec![(entity, lease_id)], Tick::new(2)).await;
            (old, reply)
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("heartbeat reaches the controlled router")
            .expect("heartbeat route reports entry");

        // When: a replacement for the same peer authenticates while renewal is blocked.
        let (replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;

        // Then: it promptly owns the registry, while the released stale reply is invalidated.
        assert!(receives_hello_ack(&replacement).await);
        router.release.notify_one();
        let (old, stale_heartbeat) = tokio::time::timeout(Duration::from_secs(5), old_heartbeat)
            .await
            .expect("released heartbeat returns")
            .expect("heartbeat task does not panic");
        assert!(stale_heartbeat.0.is_empty());
        assert_eq!(stale_heartbeat.1, vec![(entity, lease_id)]);
        let replacement_heartbeat =
            heartbeat_reply(&replacement, vec![(entity, lease_id)], Tick::new(3)).await;
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(2)),
                node: node(2),
            })
            .await;
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
        old.send_control(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })
        .await;
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
        new.send_control(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })
        .await;
        assert!(receives_hello_ack(&new).await);
        new.send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat {
                renew: vec![(entity, lease_id)],
                tick: Tick::new(2),
            },
        })
        .await;
        let renewed = loop {
            let packet = new.next_payload(Duration::from_secs(5)).await.unwrap();
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
        old.send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat {
                renew: vec![(entity, lease_id)],
                tick: Tick::new(3),
            },
        })
        .await;
        let old_heartbeat = loop {
            let packet = old.next_payload(Duration::from_secs(5)).await.unwrap();
            if let Some(GatewayReply::Lease {
                message: LeaseMsg::HeartbeatAck { leases, invalid },
            }) = decode_stream_frame(&packet)
            {
                break (leases, invalid);
            }
        };
        assert!(old_heartbeat.0.is_empty());
        assert_eq!(old_heartbeat.1, vec![(entity, lease_id)]);
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
        old.send_state(&GatewayMsg::Diff {
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
        });
        let packet = old.next_payload(Duration::from_secs(5)).await.unwrap();
        assert!(matches!(
            decode_datagram(&packet),
            Some(GatewayReply::BulkNack { entity: denied, .. }) if denied == entity
        ));

        // Closing the old generation must not park the lease transferred to the new one.
        drop(old);
        old_client.close().await;
        new.send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Heartbeat {
                renew: vec![(entity, lease_id)],
                tick: Tick::new(4),
            },
        })
        .await;
        let post_close_renewal = loop {
            let packet = new.next_payload(Duration::from_secs(5)).await.unwrap();
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
        new.send_state(&GatewayMsg::Diff {
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
        });
        let packet = new.next_payload(Duration::from_secs(5)).await.unwrap();
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
            .unwrap();
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
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 10_000),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&first).await);
        let (_second_client, second) = raw_connection(secret(2), server.addr()).await;

        // When: another NodeId authenticates while the sole slot is occupied.
        second
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(2), 900, 10_000),
                node: node(2),
            })
            .await;

        // Then: capacity rejects it without replacing the established peer.
        assert!(!receives_hello_ack(&second).await);

        // When: the established peer disconnects and its idle retention elapses.
        drop(first);
        first_client.close().await;

        // Then: a later authentication evicts the stale entry and takes the slot.
        // Advance logical time before each already-existing admission probe. If
        // disconnect cleanup has not run yet, that probe is still rejected; when
        // cleanup does run, its `idle_since` uses the current logical instant and
        // the next probe is necessarily 11 ms later. A one-shot store raced the
        // cleanup on a loaded runner and could leave both instants at 1,011 ms,
        // after which five seconds of wall time changed nothing about this clock.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            clock.0.fetch_add(11, Ordering::SeqCst);
            second
                .send_control(&GatewayMsg::Hello {
                    token: session_token(&issuer, node(2), 900, 10_000),
                    node: node(2),
                })
                .await;
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
        old.send_control(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })
        .await;
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&replacement).await);
        let inherited =
            heartbeat_reply(&replacement, vec![(first_entity, lease_id)], Tick::new(1)).await;
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
            .unwrap();
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;
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
            .unwrap();
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
                .send_control(&GatewayMsg::Lease {
                    message: LeaseMsg::Rekey {
                        entity,
                        old_cell: source,
                        new_cell: destination,
                    },
                })
                .await;
        }

        // Then: the gateway does not activate movement authority, and every
        // durable/hot observable remains exactly as it was.
        for _ in 0..3 {
            let packet = connection
                .next_payload(Duration::from_secs(1))
                .await
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
        connection.send_state(&GatewayMsg::Diff {
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
        });
        let packet = connection
            .next_payload(Duration::from_secs(1))
            .await
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;
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
        connection.send_state(&GatewayMsg::Diff {
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
        });
        let packet = connection
            .next_payload(Duration::from_secs(5))
            .await
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
            .unwrap();
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
        connection.send_state(&GatewayMsg::Diff {
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
        });
        let packet = connection
            .next_payload(Duration::from_secs(5))
            .await
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
            .send_control(&GatewayMsg::Hello {
                token: session_token(&secret(42), node(3), 1, 1),
                node: node(3),
            })
            .await;
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
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(2)),
                node: node(2),
            })
            .await;
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
        old.send_control(&GatewayMsg::Hello {
            token: support::valid_session_token(node(1)),
            node: node(1),
        })
        .await;
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
        let heartbeat = heartbeat_reply(&old, vec![(entity, lease_id)], Tick::new(3)).await;
        assert!(heartbeat.1.is_empty());
        assert_eq!(heartbeat.0.first().map(|row| row.lease_id), Some(lease_id));
        assert_eq!(journal_len(&runtime).await, journal_after_fenced_write);

        // When: a replacement connection inherits the lease generation.
        let (replacement_client, replacement) = raw_connection(secret(1), server.addr()).await;
        replacement
            .send_control(&GatewayMsg::Hello {
                token: support::valid_session_token(node(1)),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&replacement).await);
        let replacement_heartbeat =
            heartbeat_reply(&replacement, vec![(entity, lease_id)], Tick::new(4)).await;
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
        let old_heartbeat = heartbeat_reply(&old, vec![(entity, lease_id)], Tick::new(5)).await;
        assert!(old_heartbeat.0.is_empty());
        assert_eq!(old_heartbeat.1, vec![(entity, lease_id)]);
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
            .unwrap();
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

/// Read the next registrar-pushed lease control message on `connection`.
///
/// Unlike [`claim_reply`] this sends nothing: reassignment grants and expiry
/// notices arrive unprompted, which is the whole point of the push path.
async fn pushed_lease(connection: &lanes::GatewayLanes, within: Duration) -> Option<LeaseMsg> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let packet = connection.next_payload(remaining).await?;
        if let Some(GatewayReply::Lease { message }) = decode_stream_frame(&packet) {
            return Some(message);
        }
    }
}

/// A two-peer authority fixture: one seeded entity, both peers authenticated
/// with coordinator interest covering its cell.
struct HandoffFixture {
    _dir: tempfile::TempDir,
    runtime: Arc<Mutex<CellRuntime>>,
    server: GatewayServer,
    _endpoints: Vec<iroh::Endpoint>,
    holder: lanes::GatewayLanes,
    successor: lanes::GatewayLanes,
    entity: PersistId,
    cell: CellId,
}

async fn handoff_fixture(config: GatewayConfig) -> HandoffFixture {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new({
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        CellRuntime::open(&runtime_config(dir.path()), &store)
            .await
            .unwrap()
    }));
    let cell = CellId::ROOT.children()[0];
    let entity = PersistId::new(770);
    seed_entity(&runtime, entity, cell).await;

    let issuer = secret(42);
    let server = GatewayServer::spawn(
        GatewayConfig {
            interest_authority: support::interest_authority([
                support::interest_snapshot(node(1), GridId::ROOT, vec![cell]),
                support::interest_snapshot(node(2), GridId::ROOT, vec![cell]),
            ]),
            authorizer: support::authorizer(&issuer),
            identity_clock: support::fixed_clock(support::TOKEN_NOW_MS),
            identity_health: support::available_identity_health(),
            ..config
        },
        runtime.clone(),
    )
    .await
    .unwrap();

    let mut endpoints = Vec::new();
    let mut connections = Vec::new();
    for seed in [1u8, 2] {
        let (endpoint, connection) = raw_connection(secret(seed), server.addr()).await;
        connection
            .send_control(&GatewayMsg::Hello {
                token: session_token(
                    &issuer,
                    node(seed),
                    support::TOKEN_ISSUED_AT_MS,
                    support::TOKEN_TTL_MS,
                ),
                node: node(seed),
            })
            .await;
        assert!(receives_hello_ack(&connection).await);
        endpoints.push(endpoint);
        connections.push(connection);
    }
    let successor = connections.pop().expect("successor connection");
    let holder = connections.pop().expect("holder connection");

    HandoffFixture {
        _dir: dir,
        runtime,
        server,
        _endpoints: endpoints,
        holder,
        successor,
        entity,
        cell,
    }
}

/// Claim the fixture's entity for the holder and return its grant.
async fn claim_for_holder(fixture: &HandoffFixture) -> (LeaseId, SeqPair) {
    let granted = claim_reply(
        &fixture.holder,
        fixture.entity,
        GridId::ROOT,
        fixture.cell,
        ClaimKind::Weak,
        ClaimBasis::Contact { tick: Tick::new(1) },
    )
    .await;
    let LeaseMsg::Grant { lease_id, seq, .. } = granted else {
        panic!("holder's covered weak claim must be granted, got {granted:?}");
    };
    (lease_id, seq)
}

fn holder_diff(fixture: &HandoffFixture, lease_id: LeaseId, seq: SeqPair, tick: u64) -> DiffUplink {
    DiffUplink {
        cell: fixture.cell,
        grid: GridId::ROOT,
        entity: fixture.entity,
        tick: Tick::new(tick),
        kind: RecordKind::ComponentDiff,
        payload: Bytes::from_static(b"held"),
        seq: tick,
        lease_id: Some(lease_id),
        authority_seq: Some(seq),
    }
}

#[test]
fn disconnected_holder_lease_is_reassigned_to_an_interested_peer() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: one peer holds a lease and a second interested peer is live.
        let fixture = handoff_fixture(GatewayConfig::default()).await;
        let (lease_id, _) = claim_for_holder(&fixture).await;

        // When: the holder's session dies without divesting — the `kill -9`
        // case the phase exists to survive.
        fixture.holder.conn().close(0u32.into(), b"gone");

        // Then: the registrar pushes the lease to the interested peer, on a
        // freshly advanced fence, naming who it succeeded.
        let inherited = pushed_lease(&fixture.successor, Duration::from_secs(5))
            .await
            .expect("successor is told it inherited the lease");
        let LeaseMsg::Grant {
            claim_id,
            entity,
            lease_id: successor_lease_id,
            prev_holder,
            ..
        } = inherited
        else {
            panic!("successor must receive a grant, got {inherited:?}");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId::REGISTRAR);
        assert_eq!(entity, fixture.entity);
        assert!(successor_lease_id > lease_id);
        assert_eq!(prev_holder, Some(node(1)));
        assert_eq!(
            fixture.server.authority_metrics().snapshot().reassigned,
            1,
            "the reassignment is counted"
        );

        // Then: the successor can actually write, and the dead holder's token
        // never can.
        let seq = fixture
            .runtime
            .lock()
            .await
            .inspect_lease(GridId::ROOT, fixture.entity)
            .await
            .unwrap()
            .0
            .expect("registrar row survives the handoff")
            .seq;
        assert!(matches!(
            diff_reply(
                &fixture.successor,
                DiffUplink {
                    lease_id: Some(successor_lease_id),
                    authority_seq: Some(seq),
                    ..holder_diff(&fixture, successor_lease_id, seq, 11)
                },
            )
            .await,
            GatewayReply::BulkAck { .. }
        ));

        fixture.server.shutdown().await;
    });
}

#[test]
fn expired_holder_lease_is_reassigned_and_the_silent_holder_is_told() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a sweep clock a test can advance past the 10 s TTL, so a
        // silent-holder expiry does not need a ten-second sleep.
        let sweep_clock = Arc::new(AtomicClaimClock(AtomicU64::new(0)));
        let fixture = handoff_fixture(GatewayConfig {
            lease_sweep_clock: sweep_clock.clone(),
            ..GatewayConfig::default()
        })
        .await;
        let (lease_id, _) = claim_for_holder(&fixture).await;

        // When: the holder goes silent past its TTL while staying connected —
        // the zombie case peer clocks must never be trusted to resolve.
        sweep_clock.0.store(u64::MAX, Ordering::SeqCst);

        // Then: the lease moves to the interested peer...
        let inherited = pushed_lease(&fixture.successor, Duration::from_secs(5))
            .await
            .expect("successor is told it inherited the expired lease");
        let LeaseMsg::Grant {
            claim_id,
            lease_id: successor_lease_id,
            ..
        } = inherited
        else {
            panic!("successor must receive a grant, got {inherited:?}");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId::REGISTRAR);
        assert!(successor_lease_id > lease_id);

        // ...and the zombie is told, addressed by the token it still believes
        // it holds, so it stops writing without consulting its own clock.
        let expired = pushed_lease(&fixture.holder, Duration::from_secs(5))
            .await
            .expect("the silent holder is told its lease ended");
        assert_eq!(
            expired,
            LeaseMsg::Expire {
                entity: fixture.entity,
                lease_id,
                last_holder: Some(node(1)),
                reason: orrery_protocol::ExpireReason::Timeout,
                disposition: orrery_protocol::ExpireDisposition::Reassigned { to: node(2) },
            }
        );
        sweep_clock.0.store(0, Ordering::SeqCst);

        fixture.server.shutdown().await;
    });
}

// ── D25: `Expire` fan-out to the cell's interest set ────────────────────────
//
// The registrar used to tell only the losing holder that a lease ended, which
// made a park observable from no peer at all — and on a disconnect, where the
// holder is by definition gone, observable from nobody whatsoever. D25 fans a
// verbatim copy of the same `LeaseMsg::Expire` out to
// `A(G, grid, cell, t) = { p ∈ Sessions(G,t) : allows(p, grid, cell, t) }`,
// for the two dispositions that have no self-healing path of their own.
//
// The fixture below is deliberately four peers rather than the two
// `handoff_fixture` uses, because three distinct exclusion reasons have to be
// told apart in one run: a peer that covers the cell, a peer whose grant
// covers a *different* cell, and a peer whose grant covers this cell but has
// lapsed by the gateway clock. Only the first is a recipient, and all three
// answer the same `allows` seam a live `Claim` does.

/// A fan-out fixture: one holder plus three observers with different interest.
struct FanoutFixture {
    _dir: tempfile::TempDir,
    server: GatewayServer,
    _endpoints: Vec<iroh::Endpoint>,
    /// Connections for `node(1)..=node(n)`, in that order.
    peers: Vec<lanes::GatewayLanes>,
    entity: PersistId,
    cell: CellId,
}

impl FanoutFixture {
    /// The connection for `node(seed)`.
    fn peer(&self, seed: u8) -> &lanes::GatewayLanes {
        &self.peers[usize::from(seed) - 1]
    }
}

/// Bring up a gateway with one seeded entity and one session per snapshot.
///
/// `snapshots` is indexed by peer: entry `i` is `node(i + 1)`'s coordinator
/// interest, so a test says what each peer covers by constructing its snapshot
/// rather than by threading flags through here.
async fn fanout_fixture(
    config: GatewayConfig,
    snapshots: Vec<CoordinatorInterestSnapshot>,
) -> FanoutFixture {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new({
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        CellRuntime::open(&runtime_config(dir.path()), &store)
            .await
            .unwrap()
    }));
    let cell = CellId::ROOT.children()[0];
    let entity = PersistId::new(771);
    seed_entity(&runtime, entity, cell).await;

    let issuer = secret(42);
    let peer_count = snapshots.len();
    let server = GatewayServer::spawn(
        GatewayConfig {
            interest_authority: support::interest_authority(snapshots),
            authorizer: support::authorizer(&issuer),
            identity_clock: support::fixed_clock(support::TOKEN_NOW_MS),
            identity_health: support::available_identity_health(),
            ..config
        },
        runtime.clone(),
    )
    .await
    .unwrap();

    let mut endpoints = Vec::new();
    let mut peers = Vec::new();
    for seed in 1..=u8::try_from(peer_count).expect("peer count fits a seed") {
        let (endpoint, connection) = raw_connection(secret(seed), server.addr()).await;
        connection
            .send_control(&GatewayMsg::Hello {
                token: session_token(
                    &issuer,
                    node(seed),
                    support::TOKEN_ISSUED_AT_MS,
                    support::TOKEN_TTL_MS,
                ),
                node: node(seed),
            })
            .await;
        assert!(receives_hello_ack(&connection).await);
        endpoints.push(endpoint);
        peers.push(connection);
    }

    FanoutFixture {
        _dir: dir,
        server,
        _endpoints: endpoints,
        peers,
        entity,
        cell,
    }
}

/// A coordinator snapshot that lapsed before the gateway ever read it.
///
/// `allows` requires `valid_until_ms > now_ms` and the registrar clock is
/// process uptime, so zero is unconditionally in the past. That is the point
/// of the case: interest is a lifetime the gateway stamps, and a peer between
/// refreshes still *renders* the cell while being invisible to `allows` — D25
/// rule 2 names it as one of the three exclusions from D5's true interest set.
fn lapsed_interest_snapshot(
    peer: orrery_protocol::NodeId,
    cell: CellId,
) -> CoordinatorInterestSnapshot {
    CoordinatorInterestSnapshot {
        valid_until_ms: 0,
        ..support::interest_snapshot(peer, GridId::ROOT, vec![cell])
    }
}

/// Assert nothing more arrives on `connection` within `within`.
///
/// Deliberately not `pushed_lease(..).is_none()` inline at every call site: a
/// negative here is the load-bearing half of the recipient set, and it has to
/// name what it saw when it fails.
async fn no_pushed_lease(connection: &lanes::GatewayLanes, within: Duration, who: &str) {
    if let Some(message) = pushed_lease(connection, within).await {
        panic!("{who} must receive nothing, got {message:?}");
    }
}

/// Claim the fan-out fixture's entity for `node(1)`, strongly.
///
/// Strong is what makes the row park rather than reassign: `place` returns
/// `Parked` for a `STRONG_HELD` row *before* any candidate is computed (D7
/// §5 — only weak authority is redistributed without consent), which is
/// exactly the disposition with no successor stream to converge observers, and
/// therefore the one D25 fans out.
async fn strong_claim_for_holder(fixture: &FanoutFixture) -> LeaseId {
    let granted = claim_reply(
        fixture.peer(1),
        fixture.entity,
        GridId::ROOT,
        fixture.cell,
        ClaimKind::Strong,
        ClaimBasis::Explicit,
    )
    .await;
    let LeaseMsg::Grant { lease_id, .. } = granted else {
        panic!("holder's covered strong claim must be granted, got {granted:?}");
    };
    lease_id
}

/// The `Expire` a non-holder should see for a parked entity.
fn expected_park_advisory(
    entity: PersistId,
    lease_id: LeaseId,
    reason: orrery_protocol::ExpireReason,
) -> LeaseMsg {
    LeaseMsg::Expire {
        entity,
        lease_id,
        last_holder: Some(node(1)),
        reason,
        disposition: orrery_protocol::ExpireDisposition::Parked,
    }
}

/// Peers 2 and 3 cover the entity's cell; peer 4 covers a different one.
fn fanout_snapshots(cell: CellId) -> Vec<CoordinatorInterestSnapshot> {
    let elsewhere = CellId::ROOT.children()[1];
    vec![
        support::interest_snapshot(node(1), GridId::ROOT, vec![cell]),
        support::interest_snapshot(node(2), GridId::ROOT, vec![cell]),
        support::interest_snapshot(node(3), GridId::ROOT, vec![cell]),
        support::interest_snapshot(node(4), GridId::ROOT, vec![elsewhere]),
    ]
}

#[test]
fn ttl_expiry_of_a_parked_lease_fans_the_expire_out_to_the_cells_interest_set() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a strong holder and three onlookers — two covering the
        // entity's cell, one covering a different one — and a sweep clock a
        // test can push past the 10 s TTL.
        let sweep_clock = Arc::new(AtomicClaimClock(AtomicU64::new(0)));
        let cell = CellId::ROOT.children()[0];
        let fixture = fanout_fixture(
            GatewayConfig {
                lease_sweep_clock: sweep_clock.clone(),
                ..GatewayConfig::default()
            },
            fanout_snapshots(cell),
        )
        .await;
        let lease_id = strong_claim_for_holder(&fixture).await;
        let before = fixture.server.authority_metrics().snapshot();

        // When: the holder goes silent past its TTL.
        sweep_clock.0.store(u64::MAX, Ordering::SeqCst);

        // Then: the holder is told, as it always was...
        let expected = expected_park_advisory(
            fixture.entity,
            lease_id,
            orrery_protocol::ExpireReason::Timeout,
        );
        assert_eq!(
            pushed_lease(fixture.peer(1), Duration::from_secs(5))
                .await
                .expect("the silent holder is told its lease ended"),
            expected,
        );

        // ...and so is every peer whose live coordinator interest covers the
        // cell, with the *same* entity, token and disposition. Identical by
        // construction rather than by copying: one `LeaseMsg` value is cloned
        // per recipient, which is what makes the wire format unchanged and the
        // rollout one-sided (D25 rule 4).
        for observer in [2u8, 3] {
            assert_eq!(
                pushed_lease(fixture.peer(observer), Duration::from_secs(5))
                    .await
                    .unwrap_or_else(|| panic!("covering peer {observer} receives the advisory")),
                expected,
                "peer {observer}'s copy must be the holder's copy verbatim",
            );
        }

        // The peer whose grant covers another cell is not in `A` and hears
        // nothing. Checked after the two positives have landed, so this is a
        // real absence rather than a race with delivery.
        no_pushed_lease(
            fixture.peer(4),
            Duration::from_millis(500),
            "a peer whose interest covers a different cell",
        )
        .await;

        // The fan-out is purely additive: the disposition counters read
        // exactly as they did before D25, and nothing about it can produce two
        // live writers.
        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(after.expire_fanout_sent - before.expire_fanout_sent, 2);
        assert_eq!(after.expire_fanout_dropped, before.expire_fanout_dropped);
        assert_eq!(after.expire_fanout_skipped, before.expire_fanout_skipped);
        assert_eq!(
            after.parked_without_successor - before.parked_without_successor,
            1,
            "one park, whatever the audience size"
        );
        assert_eq!(after.reassigned, before.reassigned);
        assert_eq!(after.duplicate_authority, 0);

        sweep_clock.0.store(0, Ordering::SeqCst);
        fixture.server.shutdown().await;
    });
}

#[test]
fn a_disconnected_holders_park_is_observed_by_the_cells_interest_set() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: the same island, resolved by the *fast* path this time —
        // `cleanup_peer_session` on an observed disconnect rather than the TTL
        // sweep.
        let cell = CellId::ROOT.children()[0];
        let fixture = fanout_fixture(GatewayConfig::default(), fanout_snapshots(cell)).await;
        let lease_id = strong_claim_for_holder(&fixture).await;
        let before = fixture.server.authority_metrics().snapshot();

        // When: the holder's session dies. Its own copy of the `Expire` is
        // undeliverable by construction — this is the case the harness used to
        // record as "parking is not observable from any peer".
        fixture.peer(1).conn().close(0u32.into(), b"gone");

        // Then: the survivors that cover the cell observe the disposition.
        let expected = expected_park_advisory(
            fixture.entity,
            lease_id,
            orrery_protocol::ExpireReason::Disconnect,
        );
        for observer in [2u8, 3] {
            assert_eq!(
                pushed_lease(fixture.peer(observer), Duration::from_secs(5))
                    .await
                    .unwrap_or_else(|| panic!("covering peer {observer} receives the advisory")),
                expected,
            );
        }
        no_pushed_lease(
            fixture.peer(4),
            Duration::from_millis(500),
            "a peer whose interest covers a different cell",
        )
        .await;

        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(after.expire_fanout_sent - before.expire_fanout_sent, 2);
        assert_eq!(
            after.parked_without_successor - before.parked_without_successor,
            1
        );
        assert_eq!(after.reassigned, before.reassigned);
        assert_eq!(after.duplicate_authority, 0);

        fixture.server.shutdown().await;
    });
}

#[test]
fn a_peer_whose_interest_grant_lapsed_receives_no_expire_copy() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: two onlookers whose grants name the *same* cell, one of them
        // lapsed by the gateway's own clock. Fan-out eligibility is claim
        // admission, through the same seam, so the lapsed peer must be as
        // invisible to the advisory as it is to a successor nomination.
        let sweep_clock = Arc::new(AtomicClaimClock(AtomicU64::new(0)));
        let cell = CellId::ROOT.children()[0];
        let fixture = fanout_fixture(
            GatewayConfig {
                lease_sweep_clock: sweep_clock.clone(),
                ..GatewayConfig::default()
            },
            vec![
                support::interest_snapshot(node(1), GridId::ROOT, vec![cell]),
                support::interest_snapshot(node(2), GridId::ROOT, vec![cell]),
                lapsed_interest_snapshot(node(3), cell),
            ],
        )
        .await;
        let lease_id = strong_claim_for_holder(&fixture).await;
        let before = fixture.server.authority_metrics().snapshot();

        // When: the lease lapses.
        sweep_clock.0.store(u64::MAX, Ordering::SeqCst);

        // Then: the live grant is served and the lapsed one is not.
        assert_eq!(
            pushed_lease(fixture.peer(2), Duration::from_secs(5))
                .await
                .expect("the peer with live interest receives the advisory"),
            expected_park_advisory(
                fixture.entity,
                lease_id,
                orrery_protocol::ExpireReason::Timeout
            ),
        );
        no_pushed_lease(
            fixture.peer(3),
            Duration::from_millis(500),
            "a peer whose interest grant has lapsed",
        )
        .await;

        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(after.expire_fanout_sent - before.expire_fanout_sent, 1);
        assert_eq!(after.duplicate_authority, 0);

        sweep_clock.0.store(0, Ordering::SeqCst);
        fixture.server.shutdown().await;
    });
}

#[test]
fn a_reassigned_expiry_is_holder_only_and_the_successor_gets_only_its_grant() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a *weak* holder — the disposition that redistributes — and
        // two peers that both cover the cell, so one inherits and the other is
        // a live, covered, non-holder onlooker. Exactly the peer D25 rule 7
        // declines to tell, because INV-4 converges it on the successor's
        // first replicated envelope anyway.
        let sweep_clock = Arc::new(AtomicClaimClock(AtomicU64::new(0)));
        let cell = CellId::ROOT.children()[0];
        let fixture = fanout_fixture(
            GatewayConfig {
                lease_sweep_clock: sweep_clock.clone(),
                ..GatewayConfig::default()
            },
            vec![
                support::interest_snapshot(node(1), GridId::ROOT, vec![cell]),
                support::interest_snapshot(node(2), GridId::ROOT, vec![cell]),
                support::interest_snapshot(node(3), GridId::ROOT, vec![cell]),
            ],
        )
        .await;
        let granted = claim_reply(
            fixture.peer(1),
            fixture.entity,
            GridId::ROOT,
            fixture.cell,
            ClaimKind::Weak,
            ClaimBasis::Contact { tick: Tick::new(1) },
        )
        .await;
        let LeaseMsg::Grant { lease_id, .. } = granted else {
            panic!("holder's covered weak claim must be granted, got {granted:?}");
        };
        let before = fixture.server.authority_metrics().snapshot();

        // When: the lease lapses and the registrar places it with a successor.
        sweep_clock.0.store(u64::MAX, Ordering::SeqCst);

        // Then: the holder is told where authority went...
        let LeaseMsg::Expire {
            disposition: orrery_protocol::ExpireDisposition::Reassigned { to: successor },
            lease_id: expired_lease_id,
            ..
        } = pushed_lease(fixture.peer(1), Duration::from_secs(5))
            .await
            .expect("the silent holder is told its lease ended")
        else {
            panic!("a weak lease with covered candidates must reassign");
        };
        assert_eq!(expired_lease_id, lease_id);

        // ...the successor receives a `Grant`, and specifically **not** an
        // `Expire` telling it that it lost what it just gained...
        let inherited = pushed_lease(
            fixture.peer(if successor == node(2) { 2 } else { 3 }),
            Duration::from_secs(5),
        )
        .await
        .expect("the successor is told it inherited the lease");
        let LeaseMsg::Grant {
            claim_id,
            lease_id: successor_lease_id,
            ..
        } = inherited
        else {
            panic!("successor must receive a grant, got {inherited:?}");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId::REGISTRAR);
        assert!(successor_lease_id > lease_id);

        // ...and the other covered onlooker receives nothing at all. This is
        // the clause that makes D25's arithmetic work: fanning `Reassigned`
        // out would restore the `L × |A|` burst in the exact scenario the
        // system is designed around — a field host disconnecting — to save one
        // 20 Hz send interval in the case INV-4 already handles.
        let bystander = if successor == node(2) { 3 } else { 2 };
        no_pushed_lease(
            fixture.peer(bystander),
            Duration::from_millis(500),
            "a covered non-holder on a reassignment",
        )
        .await;

        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(
            after.expire_fanout_sent, before.expire_fanout_sent,
            "a reassignment fans out nothing"
        );
        assert_eq!(after.expire_fanout_dropped, before.expire_fanout_dropped);
        assert_eq!(after.reassigned - before.reassigned, 1);
        assert_eq!(
            after.parked_without_successor, before.parked_without_successor,
            "reassignment must not be miscounted as a park"
        );
        assert_eq!(after.duplicate_authority, 0);

        sweep_clock.0.store(0, Ordering::SeqCst);
        fixture.server.shutdown().await;
    });
}

#[test]
fn a_consented_release_is_advertised_to_the_cells_interest_set() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a holder about to put an entity down deliberately. `to: None`
        // parks it, and a park is the disposition with no successor stream —
        // without the advisory every observer extrapolates a proxy of a body
        // nobody writes.
        let cell = CellId::ROOT.children()[0];
        let fixture = fanout_fixture(GatewayConfig::default(), fanout_snapshots(cell)).await;
        let granted = claim_reply(
            fixture.peer(1),
            fixture.entity,
            GridId::ROOT,
            fixture.cell,
            ClaimKind::Weak,
            ClaimBasis::Contact { tick: Tick::new(1) },
        )
        .await;
        let LeaseMsg::Grant { lease_id, seq, .. } = granted else {
            panic!("holder's covered weak claim must be granted, got {granted:?}");
        };
        let before = fixture.server.authority_metrics().snapshot();

        // When: it releases rather than handing off.
        fixture
            .peer(1)
            .send_control(&GatewayMsg::Lease {
                message: LeaseMsg::Divest {
                    entity: fixture.entity,
                    lease_id,
                    to: None,
                    final_seq: seq,
                    cursor: None,
                },
            })
            .await;

        // Then: its own reply is the `Expire`, and the covering peers get the
        // same message.
        let expected = expected_park_advisory(
            fixture.entity,
            lease_id,
            orrery_protocol::ExpireReason::Parked,
        );
        assert_eq!(
            pushed_lease(fixture.peer(1), Duration::from_secs(5))
                .await
                .expect("the divesting holder receives its expiry"),
            expected,
        );
        for observer in [2u8, 3] {
            assert_eq!(
                pushed_lease(fixture.peer(observer), Duration::from_secs(5))
                    .await
                    .unwrap_or_else(|| panic!("covering peer {observer} receives the advisory")),
                expected,
            );
        }
        no_pushed_lease(
            fixture.peer(4),
            Duration::from_millis(500),
            "a peer whose interest covers a different cell",
        )
        .await;

        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(after.expire_fanout_sent - before.expire_fanout_sent, 2);
        assert_eq!(after.divested - before.divested, 1);
        assert_eq!(
            after.parked_without_successor, before.parked_without_successor,
            "a consented release is not a park for want of a successor"
        );
        assert_eq!(after.duplicate_authority, 0);

        fixture.server.shutdown().await;
    });
}

#[test]
fn negotiated_divestiture_hands_the_lease_to_the_named_successor() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a holder that has uplinked and knows its acked journal cursor
        // — the "player A grabs, throws to B" flow.
        let fixture = handoff_fixture(GatewayConfig::default()).await;
        let (lease_id, seq) = claim_for_holder(&fixture).await;
        let GatewayReply::BulkAck { lsn: cursor, .. } =
            diff_reply(&fixture.holder, holder_diff(&fixture, lease_id, seq, 5)).await
        else {
            panic!("the holder's own fenced write must be acked");
        };

        // When: it consents to hand the lease to the named peer.
        fixture
            .holder
            .send_control(&GatewayMsg::Lease {
                message: LeaseMsg::Divest {
                    entity: fixture.entity,
                    lease_id,
                    to: Some(node(2)),
                    final_seq: seq,
                    cursor: Some(cursor),
                },
            })
            .await;

        // Then: the successor is granted the lease...
        let inherited = pushed_lease(&fixture.successor, Duration::from_secs(5))
            .await
            .expect("named successor receives the handoff");
        let LeaseMsg::Grant {
            claim_id,
            lease_id: successor_lease_id,
            prev_holder,
            ..
        } = inherited
        else {
            panic!("successor must receive a grant, got {inherited:?}");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId::REGISTRAR);
        assert!(successor_lease_id > lease_id);
        assert_eq!(prev_holder, Some(node(1)));

        // ...and the divesting holder is told where authority went.
        let released = pushed_lease(&fixture.holder, Duration::from_secs(5))
            .await
            .expect("the divesting holder receives its expiry");
        assert_eq!(
            released,
            LeaseMsg::Expire {
                entity: fixture.entity,
                lease_id,
                last_holder: Some(node(1)),
                reason: orrery_protocol::ExpireReason::Revoked,
                disposition: orrery_protocol::ExpireDisposition::Reassigned { to: node(2) },
            }
        );

        let metrics = fixture.server.authority_metrics().snapshot();
        assert_eq!(metrics.divested, 1);
        assert_eq!(metrics.reassigned, 1);
        assert_eq!(
            metrics.duplicate_authority, 0,
            "a consented handoff has no overlap of its own"
        );

        // Then: the old token is dead on the wire. A real client stops writing
        // the moment it sends a divest, so this write is deliberately
        // misbehaved — and the invariant checker flags it, because on the wire
        // a lying ex-holder is indistinguishable from a zombie.
        assert!(matches!(
            diff_reply(&fixture.holder, holder_diff(&fixture, lease_id, seq, 6)).await,
            GatewayReply::BulkNack { .. }
        ));
        assert_eq!(
            fixture
                .server
                .authority_metrics()
                .snapshot()
                .duplicate_authority,
            1
        );

        fixture.server.shutdown().await;
    });
}

#[test]
fn divestiture_without_a_committed_cursor_is_refused_and_the_holder_keeps_writing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a holder mid-uplink.
        let fixture = handoff_fixture(GatewayConfig::default()).await;
        let (lease_id, seq) = claim_for_holder(&fixture).await;
        let GatewayReply::BulkAck { .. } =
            diff_reply(&fixture.holder, holder_diff(&fixture, lease_id, seq, 5)).await
        else {
            panic!("the holder's own fenced write must be acked");
        };

        // When: it offers a handoff naming a journal position the cluster
        // never wrote, and then one with no cursor at all.
        for cursor in [Some(Lsn::new(u64::MAX, u64::MAX)), None] {
            fixture
                .holder
                .send_control(&GatewayMsg::Lease {
                    message: LeaseMsg::Divest {
                        entity: fixture.entity,
                        lease_id,
                        to: Some(node(2)),
                        final_seq: seq,
                        cursor,
                    },
                })
                .await;

            // Then: the registrar refuses, definitively.
            let refused = pushed_lease(&fixture.holder, Duration::from_secs(5))
                .await
                .expect("an unsatisfiable divest gets a definitive reply");
            assert_eq!(
                refused,
                LeaseMsg::Deny {
                    claim_id: None,
                    entity: fixture.entity,
                    reason: orrery_protocol::DenyReason::NotEligible,
                    retry_after_ms: 0,
                }
            );
        }

        // When: the same peer floods refused divestitures.
        for _ in 0..80 {
            fixture
                .holder
                .send_control(&GatewayMsg::Lease {
                    message: LeaseMsg::Divest {
                        entity: fixture.entity,
                        lease_id,
                        to: Some(node(2)),
                        final_seq: seq,
                        cursor: None,
                    },
                })
                .await;
        }
        // Then: the NodeId-scoped budget cuts it off — a refused divest still
        // costs an actor round trip, so it is not an unmetered path in.
        let mut rate_limited = 0;
        while let Some(reply) = pushed_lease(&fixture.holder, Duration::from_millis(500)).await {
            if matches!(
                reply,
                LeaseMsg::Deny {
                    reason: orrery_protocol::DenyReason::RateLimited,
                    ..
                }
            ) {
                rate_limited += 1;
            }
        }
        assert!(
            rate_limited > 0,
            "a divest flood must hit the claim budget, not the registrar"
        );

        // Then: nothing moved — the holder is still the writer, and the
        // successor was never offered anything.
        assert!(matches!(
            diff_reply(&fixture.holder, holder_diff(&fixture, lease_id, seq, 6)).await,
            GatewayReply::BulkAck { .. }
        ));
        assert_eq!(
            pushed_lease(&fixture.successor, Duration::from_millis(250)).await,
            None
        );
        let metrics = fixture.server.authority_metrics().snapshot();
        assert!(metrics.divest_rejected >= 2);
        assert_eq!(metrics.divested, 0);
        assert_eq!(metrics.reassigned, 0);

        fixture.server.shutdown().await;
    });
}

#[test]
fn single_writer_invariant_checker_flags_a_fenced_out_second_writer() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a weak lease that a second interested peer takes over — the
        // legitimate contested-object case.
        let fixture = handoff_fixture(GatewayConfig::default()).await;
        let (first_lease_id, first_seq) = claim_for_holder(&fixture).await;
        // Past the claim-herd window: a *second* claim inside it is the tail
        // of a herd racing for a just-unparked entity and is refused, which
        // the herd test covers. This one is the later, deliberate takeover.
        tokio::time::sleep(Duration::from_millis(
            orrery_persistd::lease::CLAIM_HERD_DAMPER_MS + 20,
        ))
        .await;
        let stolen = claim_reply(
            &fixture.successor,
            fixture.entity,
            GridId::ROOT,
            fixture.cell,
            ClaimKind::Weak,
            ClaimBasis::Contact { tick: Tick::new(2) },
        )
        .await;
        let LeaseMsg::Grant { .. } = stolen else {
            panic!("a weak lease is supersedable, got {stolen:?}");
        };
        assert_eq!(
            fixture
                .server
                .authority_metrics()
                .snapshot()
                .duplicate_authority,
            0,
            "an orderly weak transfer is not a duplicate-authority event"
        );

        // When: the superseded peer keeps writing on its stale token, still
        // believing it is the authority.
        assert!(matches!(
            diff_reply(
                &fixture.holder,
                holder_diff(&fixture, first_lease_id, first_seq, 7)
            )
            .await,
            GatewayReply::BulkNack { .. }
        ));

        // Then: the invariant checker records exactly that overlap, naming
        // both peers.
        let metrics = fixture.server.authority_metrics();
        assert_eq!(metrics.snapshot().duplicate_authority, 1);
        let sample = metrics
            .last_duplicate_authority()
            .expect("the observation is retained for diagnosis");
        assert_eq!(sample.entity, fixture.entity);
        assert_eq!(sample.tick, Tick::new(7));
        assert_eq!(sample.rejected_writer, node(1));
        assert_eq!(sample.current_holder, node(2));
        assert_eq!(sample.rejected_lease_id, first_lease_id);

        fixture.server.shutdown().await;
    });
}

/// Present a coordinator-signed interest grant and return the gateway's answer.
async fn present_grant(connection: &lanes::GatewayLanes, grant: Vec<u8>) -> (Option<Epoch>, u8) {
    connection
        .send_control(&GatewayMsg::InterestGrant { grant })
        .await;
    loop {
        let packet = connection
            .next_payload(Duration::from_secs(5))
            .await
            .unwrap();
        if let Some(GatewayReply::InterestAck { epoch, reason }) = decode_stream_frame(&packet) {
            return (epoch, reason);
        }
    }
}

fn coordinator_grant(
    coordinator: &iroh_base::SecretKey,
    key_id: u32,
    peer: orrery_protocol::NodeId,
    epoch: u64,
    cells: Vec<CellId>,
) -> Vec<u8> {
    orrery_protocol::InterestGrantV1::sign(
        orrery_protocol::InterestGrantClaimsV1::new(
            peer,
            Epoch::new(epoch),
            GridId::ROOT,
            cells,
            60_000,
            orrery_protocol::IssuerKeyId::new(key_id),
        ),
        coordinator,
    )
    .expect("sign grant")
    .encode()
    .expect("encode grant")
}

#[test]
fn a_peer_carrying_its_coordinator_grant_unlocks_claims_and_redistribution() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a gateway that trusts a coordinator key but has been handed
        // no snapshots out of band — the production posture, where interest
        // arrives only because peers carry it.
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new({
            let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
                Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap()
        }));
        let cell = CellId::ROOT.children()[0];
        let entity = PersistId::new(880);
        seed_entity(&runtime, entity, cell).await;

        let issuer = secret(42);
        let coordinator = secret(77);
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: Arc::new(orrery_persistd::CoordinatorHandoutAuthority::new([
                    orrery_protocol::IssuerKey::new(
                        orrery_protocol::IssuerKeyId::new(5),
                        coordinator.public(),
                    ),
                ])),
                authorizer: support::authorizer(&issuer),
                identity_clock: support::fixed_clock(support::TOKEN_NOW_MS),
                identity_health: support::available_identity_health(),
                ..GatewayConfig::default()
            },
            runtime.clone(),
        )
        .await
        .unwrap();

        let mut endpoints = Vec::new();
        let mut connections = Vec::new();
        for seed in [1u8, 2] {
            let (endpoint, connection) = raw_connection(secret(seed), server.addr()).await;
            connection
                .send_control(&GatewayMsg::Hello {
                    token: session_token(
                        &issuer,
                        node(seed),
                        support::TOKEN_ISSUED_AT_MS,
                        support::TOKEN_TTL_MS,
                    ),
                    node: node(seed),
                })
                .await;
            assert!(receives_hello_ack(&connection).await);
            endpoints.push(endpoint);
            connections.push(connection);
        }
        let successor = connections.pop().unwrap();
        let holder = connections.pop().unwrap();

        // Then: with no grant presented, a weak claim has no interest backing
        // it and is refused.
        assert!(matches!(
            claim_reply(
                &holder,
                entity,
                GridId::ROOT,
                cell,
                ClaimKind::Weak,
                ClaimBasis::Contact { tick: Tick::new(1) },
            )
            .await,
            LeaseMsg::Deny {
                reason: orrery_protocol::DenyReason::NotEligible,
                ..
            }
        ));

        // When: a peer forges its own grant.
        let forged = coordinator_grant(&secret(1), 5, node(1), 1, vec![cell]);
        assert_eq!(
            present_grant(&holder, forged).await,
            (None, orrery_protocol::INTEREST_ACK_UNTRUSTED),
            "self-declared interest is self-granted authority; it must not verify"
        );

        // When: both peers present genuine coordinator grants.
        for (connection, seed) in [(&holder, 1u8), (&successor, 2u8)] {
            assert_eq!(
                present_grant(
                    connection,
                    coordinator_grant(&coordinator, 5, node(seed), 1, vec![cell])
                )
                .await,
                (Some(Epoch::new(1)), orrery_protocol::INTEREST_ACK_OK)
            );
        }

        // Then: the same claim now succeeds — interest is what gated it.
        let granted = claim_reply(
            &holder,
            entity,
            GridId::ROOT,
            cell,
            ClaimKind::Weak,
            ClaimBasis::Contact { tick: Tick::new(2) },
        )
        .await;
        let LeaseMsg::Grant { lease_id, .. } = granted else {
            panic!("a grant-backed weak claim must be granted, got {granted:?}");
        };

        // And: redistribution now has a candidate, so a lost holder's lease
        // moves instead of parking. This is the whole point of the transport:
        // the mechanism was already built, and interest is what switches it on.
        holder.conn().close(0u32.into(), b"gone");
        let inherited = pushed_lease(&successor, Duration::from_secs(5))
            .await
            .expect("the interested peer inherits the lease");
        let LeaseMsg::Grant {
            claim_id,
            lease_id: successor_lease_id,
            prev_holder,
            ..
        } = inherited
        else {
            panic!("successor must receive a grant, got {inherited:?}");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId::REGISTRAR);
        assert!(successor_lease_id > lease_id);
        assert_eq!(prev_holder, Some(node(1)));
        assert_eq!(server.authority_metrics().snapshot().reassigned, 1);

        server.shutdown().await;
    });
}

#[test]
fn an_interest_grant_is_refused_with_a_reason_the_peer_can_act_on() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a gateway trusting one coordinator key.
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new({
            let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
                Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap()
        }));
        let issuer = secret(42);
        let coordinator = secret(77);
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: Arc::new(orrery_persistd::CoordinatorHandoutAuthority::new([
                    orrery_protocol::IssuerKey::new(
                        orrery_protocol::IssuerKeyId::new(5),
                        coordinator.public(),
                    ),
                ])),
                authorizer: support::authorizer(&issuer),
                identity_clock: support::fixed_clock(support::TOKEN_NOW_MS),
                identity_health: support::available_identity_health(),
                ..GatewayConfig::default()
            },
            runtime.clone(),
        )
        .await
        .unwrap();
        let (_client, connection) = raw_connection(secret(1), server.addr()).await;
        connection
            .send_control(&GatewayMsg::Hello {
                token: session_token(
                    &issuer,
                    node(1),
                    support::TOKEN_ISSUED_AT_MS,
                    support::TOKEN_TTL_MS,
                ),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&connection).await);
        let cell = CellId::ROOT.children()[0];

        // Every refusal is distinguishable, so a misconfiguration is
        // diagnosable instead of surfacing later as unexplained claim denials.
        assert_eq!(
            present_grant(&connection, b"not a grant".to_vec()).await,
            (None, orrery_protocol::INTEREST_ACK_MALFORMED)
        );
        assert_eq!(
            present_grant(
                &connection,
                coordinator_grant(&coordinator, 99, node(1), 1, vec![cell])
            )
            .await,
            (None, orrery_protocol::INTEREST_ACK_UNTRUSTED),
            "an unknown key id is a rotation gap, not a bad signature"
        );
        assert_eq!(
            present_grant(
                &connection,
                coordinator_grant(&coordinator, 5, node(2), 1, vec![cell])
            )
            .await,
            (None, orrery_protocol::INTEREST_ACK_WRONG_PEER),
            "relaying someone else's genuine grant must not work"
        );
        assert_eq!(
            present_grant(
                &connection,
                coordinator_grant(&coordinator, 5, node(1), 1, Vec::new())
            )
            .await,
            (None, orrery_protocol::INTEREST_ACK_BOUNDS)
        );

        // A newer epoch lands; replaying the older one is then refused.
        assert_eq!(
            present_grant(
                &connection,
                coordinator_grant(&coordinator, 5, node(1), 9, vec![cell])
            )
            .await,
            (Some(Epoch::new(9)), orrery_protocol::INTEREST_ACK_OK)
        );
        assert_eq!(
            present_grant(
                &connection,
                coordinator_grant(&coordinator, 5, node(1), 8, vec![cell, CellId::ROOT])
            )
            .await,
            (None, orrery_protocol::INTEREST_ACK_SUPERSEDED)
        );

        server.shutdown().await;
    });
}

/// Send a claim without waiting for its reply, so the caller can watch what
/// the *other* peer is asked to do first.
async fn send_claim(
    connection: &lanes::GatewayLanes,
    claim_id: orrery_protocol::ClaimId,
    entity: PersistId,
    cell: CellId,
    kind: ClaimKind,
) {
    connection
        .send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id,
                entity,
                grid: GridId::ROOT,
                cell,
                kind,
                basis: ClaimBasis::Explicit,
                observed: Default::default(),
                tick: Tick::new(1),
            },
        })
        .await;
}

#[test]
fn a_strong_claim_asks_the_holder_to_divest_instead_of_refusing_the_claimant() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: one peer holds the entity and another grabs it — "B grabs an
        // object A is holding", the half of §4.2 a claimant drives.
        let fixture = handoff_fixture(GatewayConfig::default()).await;
        let (lease_id, seq) = claim_for_holder(&fixture).await;
        let GatewayReply::BulkAck { lsn: cursor, .. } =
            diff_reply(&fixture.holder, holder_diff(&fixture, lease_id, seq, 5)).await
        else {
            panic!("the holder's own fenced write must be acked");
        };

        // When: the claimant sends its explicit strong claim.
        let claim_id = orrery_protocol::ClaimId(42);
        send_claim(
            &fixture.successor,
            claim_id,
            fixture.entity,
            fixture.cell,
            ClaimKind::Strong,
        )
        .await;

        // Then: the registrar asks the holder rather than answering the
        // claimant, naming the claimant as the successor to hand it to.
        let asked = pushed_lease(&fixture.holder, Duration::from_secs(5))
            .await
            .expect("the holder is asked to divest");
        assert_eq!(
            asked,
            LeaseMsg::Divest {
                entity: fixture.entity,
                lease_id,
                to: Some(node(2)),
                final_seq: SeqPair::default(),
                cursor: None,
            }
        );
        assert_eq!(
            fixture
                .server
                .authority_metrics()
                .snapshot()
                .divest_requested,
            1
        );

        // When: the holder consents.
        fixture
            .holder
            .send_control(&GatewayMsg::Lease {
                message: LeaseMsg::Divest {
                    entity: fixture.entity,
                    lease_id,
                    to: Some(node(2)),
                    final_seq: seq,
                    cursor: Some(cursor),
                },
            })
            .await;

        // Then: the claimant's *own* claim is what gets answered — its pending
        // correlation resolves rather than looking like an unsolicited grant.
        let granted = pushed_lease(&fixture.successor, Duration::from_secs(5))
            .await
            .expect("the claimant receives its grant");
        let LeaseMsg::Grant {
            claim_id: answered,
            lease_id: claimant_lease_id,
            prev_holder,
            ..
        } = granted
        else {
            panic!("claimant must receive a grant, got {granted:?}");
        };
        assert_eq!(answered, claim_id);
        assert!(claimant_lease_id > lease_id);
        assert_eq!(prev_holder, Some(node(1)));

        // And: the old holder's token is dead.
        assert!(matches!(
            diff_reply(&fixture.holder, holder_diff(&fixture, lease_id, seq, 6)).await,
            GatewayReply::BulkNack { .. }
        ));

        fixture.server.shutdown().await;
    });
}

#[test]
fn an_unanswered_request_takes_weak_authority_but_never_strong_ownership() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a deadline short enough to observe, and a holder that will
        // simply not answer.
        let fixture = handoff_fixture(GatewayConfig {
            handoff_deadline_ms: 120,
            ..GatewayConfig::default()
        })
        .await;
        let (weak_lease_id, _) = claim_for_holder(&fixture).await;

        // When: the claimant grabs it and the holder stays silent.
        send_claim(
            &fixture.successor,
            orrery_protocol::ClaimId(7),
            fixture.entity,
            fixture.cell,
            ClaimKind::Strong,
        )
        .await;
        assert!(matches!(
            pushed_lease(&fixture.holder, Duration::from_secs(5)).await,
            Some(LeaseMsg::Divest { .. })
        ));

        // Then: weak authority converts to unconditional divestiture past the
        // deadline — an interaction must not stall on an unresponsive peer.
        let granted = pushed_lease(&fixture.successor, Duration::from_secs(5))
            .await
            .expect("the claimant is granted after the deadline");
        let LeaseMsg::Grant {
            claim_id,
            lease_id: taken,
            ..
        } = granted
        else {
            panic!("weak authority must be taken, got {granted:?}");
        };
        assert_eq!(claim_id, orrery_protocol::ClaimId(7));
        assert!(taken > weak_lease_id);
        let metrics = fixture.server.authority_metrics().snapshot();
        assert_eq!(metrics.handoff_timed_out, 1);
        assert_eq!(metrics.reassigned, 1);

        // The dispossessed holder is told, so it stops writing without
        // consulting its own clock.
        let told = pushed_lease(&fixture.holder, Duration::from_secs(5))
            .await
            .expect("the silent holder is told its lease ended");
        assert!(matches!(
            told,
            LeaseMsg::Expire {
                disposition: orrery_protocol::ExpireDisposition::Reassigned { to },
                ..
            } if to == node(2)
        ));

        // When: the new holder now owns it *strongly* and a third claim
        // arrives that it also ignores.
        send_claim(
            &fixture.holder,
            orrery_protocol::ClaimId(8),
            fixture.entity,
            fixture.cell,
            ClaimKind::Strong,
        )
        .await;
        assert!(matches!(
            pushed_lease(&fixture.successor, Duration::from_secs(5)).await,
            Some(LeaseMsg::Divest { .. })
        ));

        // Then: nothing is taken. Only expiry breaks strong ownership; a
        // missed deadline is not a theft licence.
        let refused = pushed_lease(&fixture.holder, Duration::from_secs(5))
            .await
            .expect("the claimant gets a definitive refusal");
        assert_eq!(
            refused,
            LeaseMsg::Deny {
                claim_id: Some(orrery_protocol::ClaimId(8)),
                entity: fixture.entity,
                reason: orrery_protocol::DenyReason::StrongHeld,
                retry_after_ms: 0,
            }
        );

        fixture.server.shutdown().await;
    });
}

#[test]
fn a_holder_that_parks_instead_of_handing_over_still_answers_the_claimant() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: a claimant waiting on a request the holder will refuse by
        // releasing the entity rather than handing it over.
        let fixture = handoff_fixture(GatewayConfig::default()).await;
        let (lease_id, seq) = claim_for_holder(&fixture).await;
        send_claim(
            &fixture.successor,
            orrery_protocol::ClaimId(11),
            fixture.entity,
            fixture.cell,
            ClaimKind::Strong,
        )
        .await;
        assert!(matches!(
            pushed_lease(&fixture.holder, Duration::from_secs(5)).await,
            Some(LeaseMsg::Divest { .. })
        ));

        // When: the holder releases it to nobody.
        fixture
            .holder
            .send_control(&GatewayMsg::Lease {
                message: LeaseMsg::Divest {
                    entity: fixture.entity,
                    lease_id,
                    to: None,
                    final_seq: seq,
                    cursor: None,
                },
            })
            .await;

        // Then: the claimant is told, rather than waiting out a deadline for a
        // request that has already been resolved.
        let answered = pushed_lease(&fixture.successor, Duration::from_secs(5))
            .await
            .expect("the claimant is answered");
        assert_eq!(
            answered,
            LeaseMsg::Deny {
                claim_id: Some(orrery_protocol::ClaimId(11)),
                entity: fixture.entity,
                reason: orrery_protocol::DenyReason::NotEligible,
                retry_after_ms: 0,
            }
        );

        // And: because this claimant is also a peer whose coordinator interest
        // covers the cell, the same release reaches it a second time as a D25
        // advisory. It arrives *after* the `Deny` — `divest_lease` answers a
        // waiting claimant before it fans anything out — and it is drained
        // here rather than tolerated, because the retry below reads the next
        // lease frame on this lane and would otherwise read this one.
        assert_eq!(
            pushed_lease(&fixture.successor, Duration::from_secs(5))
                .await
                .expect("the claimant observes the park it asked for"),
            LeaseMsg::Expire {
                entity: fixture.entity,
                lease_id,
                last_holder: Some(node(1)),
                reason: orrery_protocol::ExpireReason::Parked,
                disposition: orrery_protocol::ExpireDisposition::Parked,
            },
        );

        // And: the entity parked, so the claimant's retry can unpark it.
        let parked = claim_reply(
            &fixture.successor,
            fixture.entity,
            GridId::ROOT,
            fixture.cell,
            ClaimKind::Strong,
            ClaimBasis::Explicit,
        )
        .await;
        assert!(
            matches!(parked, LeaseMsg::Grant { .. }),
            "a parked entity is claimable by anyone, got {parked:?}"
        );

        fixture.server.shutdown().await;
    });
}

/// The version a refused [`GatewayMsg::VersionedHello`] came back with, or
/// `None` if the gateway answered something else (an ack included).
async fn hello_refusal(connection: &lanes::GatewayLanes) -> Option<(u16, u8)> {
    match connection.next_payload(Duration::from_millis(250)).await {
        Some(packet) => match decode_stream_frame(&packet) {
            Some(GatewayReply::HelloRefused {
                protocol, reason, ..
            }) => Some((protocol, reason)),
            _ => None,
        },
        None => None,
    }
}

#[test]
fn versioned_hello_is_accepted_across_the_rolling_window_and_refused_outside_it() {
    // The gateway's own version is per-instance, so this drives all four cases
    // against one server without touching `PROTOCOL_VERSION`.
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
        let issuer = support::issuer();
        let server = GatewayServer::spawn(
            GatewayConfig {
                protocol_version: 4,
                ..support::authority_config(node(1), GridId::ROOT, vec![CellId::ROOT])
            },
            router,
        )
        .await
        .unwrap();
        let (_client, connection) = raw_connection(secret(1), server.addr()).await;

        // V and V−1 are the rolling-upgrade window: a cluster deploys ahead of
        // its clients, so the version below the gateway's own is accepted.
        for version in [4u16, 3] {
            connection
                .send_control(&GatewayMsg::VersionedHello {
                    token: session_token(&issuer, node(1), 900, 200),
                    node: node(1),
                    version,
                })
                .await;
            assert!(
                receives_hello_ack(&connection).await,
                "version {version} is inside the window"
            );
        }

        // Anything else is refused with a typed reply naming the gateway's own
        // version, not dropped: silence here is indistinguishable from a slow
        // gateway, and the client would re-offer the same version forever.
        for version in [2u16, 5, 0] {
            connection
                .send_control(&GatewayMsg::VersionedHello {
                    token: session_token(&issuer, node(1), 900, 200),
                    node: node(1),
                    version,
                })
                .await;
            assert_eq!(
                hello_refusal(&connection).await,
                Some((4, GatewayReply::HELLO_REFUSED_PROTOCOL)),
                "version {version} is outside the window"
            );
        }

        // The unversioned bootstrap is still accepted unchecked, which is what
        // makes enforcement opt-in until `GatewayMsg::Hello` is retired.
        connection
            .send_control(&GatewayMsg::Hello {
                token: session_token(&issuer, node(1), 900, 200),
                node: node(1),
            })
            .await;
        assert!(receives_hello_ack(&connection).await);

        server.shutdown().await;
    });
}

/// A runtime config hosting exactly `shards`, at `epoch`.
///
/// The default [`runtime_config`] hosts `CellId::ROOT`, which is a prefix of
/// every cell — nothing is ever outside it, so nothing in this file could have
/// exercised a wrong-owner route before. Naming a narrower set is what makes
/// "a cell this node does not own" expressible at all.
fn sharded_runtime_config(
    dir: &std::path::Path,
    shards: Vec<CellId>,
    epoch: Epoch,
) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        epoch,
        ..runtime_config(dir)
    }
}

/// A Diff, a Claim and an area Read for a cell outside this node's `--shard`
/// set are each refused **by name**, and the refusal carries the cell and this
/// node's fence epoch.
///
/// The defect: all three used to come back as `Reject::JournalClosed`, which
/// the wire reported as a generic `BulkNack`/`Deny`. A peer could not tell
/// "this node is broken" from "you are talking to the wrong node", and those
/// call for opposite responses — one is a reason to stop writing, the other a
/// reason to write somewhere else (docs/08-persistence.md §3.5).
///
/// Every leg asserts the in-shard control in the same run, because a refusal
/// that fired for *every* cell would satisfy the wrong-cell assertions and be
/// a total outage.
#[test]
fn a_cell_outside_this_nodes_shards_is_refused_as_a_wrong_owner_not_as_a_broken_journal() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let roots = CellId::ROOT.children();
        let (mine, theirs) = (roots[0], roots[1]);
        let in_shard = mine.children()[0];
        let out_of_shard = theirs.children()[0];
        // Not zero, so a reported epoch cannot pass by coincidence with the
        // `Epoch::new(0)` a forgotten field would carry.
        let epoch = Epoch::new(9);

        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(
                &sharded_runtime_config(dir.path(), vec![mine], epoch),
                &store,
            )
            .await
            .unwrap(),
        ));
        let entity = PersistId::new(4_117);
        seed_entity(&runtime, entity, in_shard).await;

        let router: Arc<dyn Router> = runtime.clone();
        let issuer = secret(42);
        // Interest covers both cells: the refusal under test must come from
        // shard ownership, not from a peer that was never allowed to ask.
        let server = GatewayServer::spawn(
            support::authority_config(node(1), GridId::ROOT, vec![in_shard, out_of_shard]),
            router,
        )
        .await
        .unwrap();
        let (_client, conn) = raw_connection(secret(1), server.addr()).await;
        conn.send_control(&GatewayMsg::Hello {
            token: session_token(&issuer, node(1), 900, 200),
            node: node(1),
        })
        .await;
        assert!(receives_hello_ack(&conn).await);

        // -- Diff --------------------------------------------------------
        let wrong_cell_diff = DiffUplink {
            cell: out_of_shard,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(3),
            kind: RecordKind::ComponentDiff,
            payload: Bytes::from_static(b"misaddressed"),
            seq: 1,
            lease_id: None,
            authority_seq: None,
        };
        match diff_reply(&conn, wrong_cell_diff).await {
            GatewayReply::BulkNack { reason, lease, .. } => {
                assert_eq!(
                    reason,
                    orrery_protocol::BULK_NACK_WRONG_OWNER,
                    "a diff for an unowned cell must not read as a journal failure"
                );
                assert!(
                    lease.is_none(),
                    "a routing refusal carries no registrar row: there is none here to carry"
                );
            }
            other => panic!("expected a wrong-owner BulkNack, got {other:?}"),
        }

        // -- Claim -------------------------------------------------------
        let denied = claim_reply(
            &conn,
            entity,
            GridId::ROOT,
            out_of_shard,
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await;
        match denied {
            LeaseMsg::Deny {
                reason, entity: e, ..
            } => {
                assert_eq!(e, entity);
                assert_eq!(
                    reason,
                    orrery_protocol::DenyReason::WrongOwner {
                        grid: GridId::ROOT,
                        shard: out_of_shard,
                        epoch,
                        owner: None,
                    },
                    "the refusal must name the cell and this node's fence epoch, \
                     not report the claimant as ineligible"
                );
            }
            other => panic!("expected a wrong-owner Deny, got {other:?}"),
        }

        // The control: the same claim shape at a cell this node *does* own is
        // arbitrated normally, so the refusal above is about the address.
        let granted = claim_reply(
            &conn,
            entity,
            GridId::ROOT,
            in_shard,
            ClaimKind::Weak,
            ClaimBasis::Explicit,
        )
        .await;
        assert!(
            matches!(granted, LeaseMsg::Grant { .. }),
            "an in-shard claim must still be granted, got {granted:?}"
        );

        // -- Area read ---------------------------------------------------
        conn.send_control(&GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![out_of_shard],
        })
        .await;
        let area = loop {
            let packet = conn.next_payload(Duration::from_secs(5)).await.unwrap();
            match decode_stream_frame(&packet) {
                Some(
                    reply @ (GatewayReply::AreaPage { .. } | GatewayReply::AreaLoadError { .. }),
                ) => break reply,
                _ => continue,
            }
        };
        match area {
            GatewayReply::AreaLoadError { cell, kind } => {
                assert_eq!(cell, out_of_shard);
                assert_eq!(
                    kind,
                    orrery_protocol::AREA_LOAD_ERR_WRONG_OWNER,
                    "an unowned cell must not be answered as an empty page: a node \
                     hosting no shard over it cannot claim it is empty"
                );
            }
            other => panic!("expected a wrong-owner AreaLoadError, got {other:?}"),
        }

        // The control: an owned cell still pages, so the error above is not
        // the area path failing outright.
        conn.send_control(&GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![in_shard],
        })
        .await;
        let owned_area = loop {
            let packet = conn.next_payload(Duration::from_secs(5)).await.unwrap();
            match decode_stream_frame(&packet) {
                Some(
                    reply @ (GatewayReply::AreaPage { .. } | GatewayReply::AreaLoadError { .. }),
                ) => break reply,
                _ => continue,
            }
        };
        assert!(
            matches!(owned_area, GatewayReply::AreaPage { cell, .. } if cell == in_shard),
            "an in-shard read must still page, got {owned_area:?}"
        );

        server.shutdown().await;
    });
}

/// A Heartbeat routed to a cell outside this node's shard set is refused as a
/// wrong owner, at the router surface the gateway renews through.
///
/// Asserted here rather than over the wire because the wire cannot reach it:
/// `resolve_renewals` only renews pairs this session was *granted*, and a
/// grant for an unowned cell is exactly what the claim leg above proves is
/// impossible. The reachable production shape is a shard that moves off this
/// node while a session still holds leases in it, and that arrives at the
/// router through these same two methods.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heartbeat_for_a_cell_outside_this_nodes_shards_is_refused_as_a_wrong_owner() {
    let roots = CellId::ROOT.children();
    let (mine, theirs) = (roots[0], roots[1]);
    let in_shard = mine.children()[0];
    let out_of_shard = theirs.children()[0];
    let epoch = Epoch::new(9);

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
        Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
    let runtime = CellRuntime::open(
        &sharded_runtime_config(dir.path(), vec![mine], epoch),
        &store,
    )
    .await
    .unwrap();

    let entity = PersistId::new(4_118);
    let refused = Router::heartbeat_lease(
        &runtime,
        GridId::ROOT,
        out_of_shard,
        entity,
        node(1),
        LeaseId(1),
        0,
    )
    .await;
    assert_eq!(
        refused,
        Err(orrery_persistd::Reject::WrongOwner {
            grid: GridId::ROOT,
            shard: out_of_shard,
            epoch,
        }),
        "a renewal for an unowned cell must name the cell and the epoch"
    );

    // The batched path the gateway actually calls degrades an unroutable entry
    // to a per-entry `None` rather than failing a batch that may straddle
    // owned and unowned cells, and that is deliberate — so the *reason* is
    // recovered from `Router::wrong_owner_epoch`, which is what
    // `renew_session_leases` consults for exactly the pairs that did not
    // renew. Both halves are asserted here so the pair cannot drift apart.
    let batched = Router::heartbeat_leases(
        &runtime,
        GridId::ROOT,
        node(1),
        &[
            orrery_persistd::cluster::LeaseRenewal {
                cell: in_shard,
                entity,
                lease_id: LeaseId(1),
            },
            orrery_persistd::cluster::LeaseRenewal {
                cell: out_of_shard,
                entity: PersistId::new(4_119),
                lease_id: LeaseId(1),
            },
        ],
        0,
    )
    .await;
    assert_eq!(
        batched,
        Ok(vec![None, None]),
        "the batch answers per entry; it must not fail the owned entry too"
    );
    assert_eq!(
        Router::wrong_owner_epoch(&runtime, GridId::ROOT, out_of_shard).await,
        Some(epoch),
        "the refused entry's cell must be reportable as this node's wrong owner"
    );
    assert_eq!(
        Router::wrong_owner_epoch(&runtime, GridId::ROOT, in_shard).await,
        None,
        "a cell this node does own must never be reported as misaddressed"
    );

    // The control: the same renewal at an owned cell routes and answers (with
    // no row, because nothing granted one) rather than being refused.
    let owned = Router::heartbeat_lease(
        &runtime,
        GridId::ROOT,
        in_shard,
        entity,
        node(1),
        LeaseId(1),
        0,
    )
    .await;
    assert_eq!(
        owned,
        Ok(None),
        "an in-shard renewal must still route to its actor"
    );

    runtime.close().await.unwrap();
}
