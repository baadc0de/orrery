//! The two halves of a live shard handover that only exist at the gateway:
//! the `Expire` a divested holder receives on its **own session** (D26 rule 3
//! step 3, invariant I2) and the closed claim admission that keeps a new
//! holder from appearing behind the drain (step 2).
//!
//! `tests/shard_handover.rs` proves the fence and registrar half with no
//! transport. This one needs a real session, because I2 is not "an `Expire`
//! was constructed" — it is "an `Expire` reached the holder's connection
//! before the ownership CAS", and a holder that never received one is exactly
//! the peer D26's Context describes writing into a void.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore,
    RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::{
    CellId, ClaimBasis, ClaimKind, Epoch, GatewayMsg, GatewayReply, GridId, JournalRecord,
    LeaseMsg, Lsn, PersistId, RecordKind, Tick,
};
use tokio::sync::Mutex;

const NODE_A: u64 = 1;
const NODE_B: u64 = 2;

fn node(n: u8) -> orrery_protocol::NodeId {
    support::node(n)
}

fn secret(n: u8) -> iroh_base::SecretKey {
    support::secret(n)
}

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
    let mut admission = connection.accept_uni().await.unwrap();
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0u8]);
    (endpoint, lanes::GatewayLanes::attach(connection))
}

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

async fn claim_reply(
    connection: &lanes::GatewayLanes,
    entity: PersistId,
    cell: CellId,
) -> LeaseMsg {
    connection
        .send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id: orrery_protocol::ClaimId(1),
                entity,
                grid: GridId::ROOT,
                cell,
                kind: ClaimKind::Weak,
                basis: ClaimBasis::Contact { tick: Tick::new(1) },
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

/// One gateway owning one shard, with `n` authenticated peers holding
/// coordinator interest over the entities' cell.
struct Fixture {
    _dir: tempfile::TempDir,
    _fence: Arc<MemFenceStore>,
    runtime: Arc<Mutex<CellRuntime>>,
    server: GatewayServer,
    _endpoints: Vec<iroh::Endpoint>,
    peers: Vec<lanes::GatewayLanes>,
    shard: CellId,
    cell: CellId,
}

impl Fixture {
    async fn open(peer_count: u8, entities: &[PersistId]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // A level-1 shard rather than the root, so "this node hosts no shard
        // over the cell" is a state this runtime can actually reach: with the
        // root as the shard there is no cell outside it.
        let shard = CellId::ROOT.children()[0];
        let cell = shard.children()[0];
        let fence = Arc::new(MemFenceStore::new());
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        let runtime = Arc::new(Mutex::new(
            CellRuntime::open(
                &RuntimeConfig {
                    shards: vec![shard],
                    grid: GridId::ROOT,
                    journal: JournalConfig {
                        dir: dir.path().to_path_buf(),
                        commit: GroupCommitConfig {
                            mode: AdaptiveCommitMode::AlwaysBatch,
                            batch_window: Duration::from_millis(10),
                            batch_max_records: 100_000,
                            batch_max_bytes: 1 << 20,
                        },
                    },
                    node_id: NODE_A,
                    epoch: Epoch::new(1),
                    fence: Arc::clone(&fence) as Arc<dyn orrery_persistd::FenceStore>,
                },
                &store,
            )
            .await
            .unwrap(),
        ));
        // The durable row that makes this node the owner (D26 rule 1).
        {
            use orrery_persistd::FenceStore as _;
            fence
                .fence(
                    GridId::ROOT,
                    shard,
                    None,
                    &orrery_persistd::FenceRow {
                        owner: NODE_A,
                        epoch: Epoch::new(1),
                        status: orrery_persistd::FenceStatus::Active,
                    },
                )
                .await
                .unwrap();
        }
        for entity in entities {
            seed_entity(&runtime, *entity, cell).await;
        }

        let issuer = secret(42);
        let snapshots: Vec<_> = (1..=peer_count)
            .map(|seed| support::interest_snapshot(node(seed), GridId::ROOT, vec![cell]))
            .collect();
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: support::interest_authority(snapshots),
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
        let mut peers = Vec::new();
        for seed in 1..=peer_count {
            let (endpoint, connection) = raw_connection(secret(seed), server.addr()).await;
            connection
                .send_control(&GatewayMsg::Hello {
                    token: support::session_token(
                        &issuer,
                        node(seed),
                        support::TOKEN_ISSUED_AT_MS,
                        support::TOKEN_TTL_MS,
                    ),
                    node: node(seed),
                })
                .await;
            let ack = connection.next_payload(Duration::from_secs(5)).await;
            assert!(matches!(
                ack.as_deref().and_then(decode_stream_frame),
                Some(GatewayReply::HelloAck { .. })
            ));
            endpoints.push(endpoint);
            peers.push(connection);
        }

        Self {
            _dir: dir,
            _fence: fence,
            runtime,
            server,
            _endpoints: endpoints,
            peers,
            shard,
            cell,
        }
    }

    fn peer(&self, seed: u8) -> &lanes::GatewayLanes {
        &self.peers[usize::from(seed) - 1]
    }
}

/// D26 invariant **I2**, measured: every lease live when the drain began got
/// an `Expire` on its holder's own connection before the ownership CAS, and no
/// holder was left heartbeating at a node that had stopped serving its shard.
#[test]
fn every_holder_on_a_moving_shard_receives_an_expire_before_the_cas() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let entities: Vec<PersistId> = (1..=3).map(PersistId::new).collect();
        let fixture = Fixture::open(3, &entities).await;

        // Given: three peers, each holding a live lease on this shard.
        let mut granted = Vec::new();
        for (index, entity) in entities.iter().enumerate() {
            let seed = u8::try_from(index + 1).unwrap();
            let reply = claim_reply(fixture.peer(seed), *entity, fixture.cell).await;
            let LeaseMsg::Grant { lease_id, .. } = reply else {
                panic!("peer {seed} must hold a lease before the drain: {reply:?}");
            };
            granted.push(lease_id);
        }
        let before = fixture.server.authority_metrics().snapshot();

        // When: the shard is marked draining and drained.
        fixture
            .runtime
            .lock()
            .await
            .begin_handover(fixture.shard, NODE_B, Some(node(200)))
            .await
            .expect("mark");
        let report = fixture
            .server
            .drain_shard_for_handover(GridId::ROOT, fixture.shard)
            .await;

        // Then: the drain accounted for every live row and delivered every
        // `Expire`, which is I2 with nothing left over.
        assert!(report.complete, "the drain finished inside its deadline");
        assert_eq!(report.live_at_start, 3);
        assert_eq!(
            report.live_at_start - report.expires_delivered,
            0,
            "I2: leases_live_at_drain_start - expires_delivered_before_cas == 0"
        );
        assert_eq!(report.expires_undeliverable, 0);

        // ...and each holder saw its own token withdrawn, as a *park* rather
        // than a timeout — the counter that tells a handover park from a crash
        // park.
        for (index, lease_id) in granted.iter().enumerate() {
            let seed = u8::try_from(index + 1).unwrap();
            let message = pushed_lease(fixture.peer(seed), Duration::from_secs(5))
                .await
                .unwrap_or_else(|| panic!("peer {seed} must be told its lease ended"));
            let LeaseMsg::Expire {
                lease_id: expired,
                reason,
                disposition,
                ..
            } = message
            else {
                panic!("peer {seed} must receive an Expire, got {message:?}");
            };
            assert_eq!(
                expired, *lease_id,
                "the Expire is addressed by the token the holder still has installed"
            );
            assert_eq!(reason, orrery_protocol::ExpireReason::Parked);
            assert_eq!(
                disposition,
                orrery_protocol::ExpireDisposition::Parked,
                "a drain never reassigns: a successor on this gateway would be a \
                 holder under a shard about to move away from it"
            );
        }

        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(after.handover_leases_live_at_drain_start, 3);
        assert_eq!(after.handover_expires_delivered_before_cas, 3);
        assert_eq!(after.handover_parks, 3);
        assert_eq!(
            after.parked_without_successor, before.parked_without_successor,
            "a handover park is not a crash park and must not alarm as one"
        );
        assert_eq!(after.duplicate_authority, 0);
        assert_eq!(
            after.heartbeats_rejected_wrong_owner, 0,
            "I2's second counter: nobody was left heartbeating at the old owner"
        );

        fixture.server.shutdown().await;
    });
}

/// Step 2: while the shard drains, a *new* claim under it is refused — and
/// refused as a redirect naming the successor, not as a statement about the
/// claimant.
#[test]
fn a_claim_under_a_draining_shard_is_redirected_to_the_successor() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let entity = PersistId::new(7);
        let fixture = Fixture::open(1, &[entity]).await;

        // Given: the claim would be granted at rest.
        let reply = claim_reply(fixture.peer(1), entity, fixture.cell).await;
        assert!(
            matches!(reply, LeaseMsg::Grant { .. }),
            "the claim is admissible before the drain: {reply:?}"
        );
        // Release it, so the only thing refusing the next claim is the drain.
        fixture
            .runtime
            .lock()
            .await
            .begin_handover(fixture.shard, NODE_B, Some(node(200)))
            .await
            .expect("mark");
        let before = fixture.server.authority_metrics().snapshot();

        // When: a second claim arrives under the draining subtree.
        let reply = claim_reply(fixture.peer(1), PersistId::new(8), fixture.cell).await;

        // Then: it is a redirect that names where to go.
        let LeaseMsg::Deny { reason, .. } = reply else {
            panic!("a claim under a draining shard must be denied, got {reply:?}");
        };
        let orrery_protocol::DenyReason::WrongOwner {
            grid,
            shard,
            epoch,
            owner,
        } = reason
        else {
            panic!(
                "the refusal is about the address, not the claimant: {reason:?}. \
                 A `NotEligible` here would send the peer backing off against a \
                 node that is about to stop answering for this cell entirely."
            );
        };
        assert_eq!(grid, GridId::ROOT);
        assert_eq!(shard, fixture.cell);
        assert_eq!(epoch, Epoch::new(1), "the shard's row epoch, not e+1 yet");
        assert_eq!(
            owner,
            Some(node(200)),
            "ADR-0026's `owner` field, filled: this is the first build in which \
             a WrongOwner can say who to ask instead"
        );

        let after = fixture.server.authority_metrics().snapshot();
        assert_eq!(
            after.claims_denied_draining - before.claims_denied_draining,
            1
        );

        fixture.server.shutdown().await;
    });
}
