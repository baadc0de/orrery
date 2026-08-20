//! Every lease an island held reaches `PARKED` when the whole island leaves
//! at once (D24 §(a), issue #157).
//!
//! D24's drain bound — `T_drain ≤ T_last_peer_gone + TTL + S` — rests on one
//! premise: once the last peer is gone there is nobody left to redistribute
//! to, so the expiry sweep parks every row and nothing restarts a TTL after
//! that instant. This asserts the premise rather than the bound, because the
//! bound is a statement about a clock and the premise is a statement about the
//! registrar's candidate set, and it is the premise that broke.
//!
//! The failure this pins is not a slow drain. With every peer of an island
//! departing together, each departing peer's rows were parked and then handed
//! straight back out to another peer that was *also* departing, on a fresh
//! 10 s lease. The counters then went quiet with the island empty and several
//! hundred rows still held by sessions that would never renew or release them.
//! So the assertion here is a conjunction of three things, and dropping any
//! one of them would let that state pass:
//!
//! 1. every row held at drain start is parked in the registrar's own rows —
//!    not "eventually", but before the departure's own dispositions stop
//!    moving;
//! 2. the dispositions **reconcile**: `parked_without_successor + reassigned`
//!    accounts for every row, with nothing left over. A drain that parks
//!    everything by reassigning it round the island four times first is not
//!    the same event, and only the arithmetic tells them apart;
//! 3. `duplicate_authority` stays at zero throughout, because the cheapest
//!    way to make (1) and (2) hold would be to park rows out from under a
//!    holder that still believes it writes them.
//!
//! The end-to-end *timing* against `TTL + S` is measured by the P3 island
//! gate, which has real peer processes and a real registrar; this is the
//! crate-level regression that runs per commit.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router,
    RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::{
    CellId, ClaimBasis, ClaimKind, Epoch, GatewayMsg, GatewayReply, GridId, JournalRecord,
    LeaseMsg, Lsn, PersistId, RecordKind, Tick,
};
use tokio::sync::Mutex;

/// Eight peers, matching the P3 island gate's population.
const PEERS: u8 = 8;
/// Enough rows per peer that a redistribution cascade is visible in the
/// counters rather than being lost in a handful of leases.
const PER_PEER: u64 = 25;

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(10),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
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
    // Read admission by hand before attaching the lane readers, which would
    // otherwise consume the stream this handshake is waiting on.
    let mut admission = connection.accept_uni().await.unwrap();
    assert_eq!(admission.read_to_end(16).await.unwrap(), vec![0u8]);
    (endpoint, lanes::GatewayLanes::attach(connection))
}

async fn seed_entity(runtime: &Arc<Mutex<CellRuntime>>, entity: PersistId, cell: CellId) {
    let actor = runtime.lock().await.actor(GridId::ROOT, cell).unwrap();
    let payload = Bytes::from_static(b"seeded");
    actor
        .start_diff(JournalRecord {
            lsn: Lsn::new(0, 0),
            cell,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(0),
            epoch: Epoch::new(0),
            author: support::node(9),
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

/// Count the rows in `entities` that still name a holder.
async fn still_held(runtime: &Arc<Mutex<CellRuntime>>, entities: &[PersistId]) -> usize {
    let mut held = 0;
    for entity in entities {
        let (row, _, _) = runtime
            .lock()
            .await
            .inspect_lease(GridId::ROOT, *entity)
            .await
            .unwrap();
        if row.as_ref().and_then(|row| row.holder).is_some() {
            held += 1;
        }
    }
    held
}

#[test]
fn an_island_departing_at_once_parks_every_lease_it_held() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Given: an island of eight peers, every one of them holding leases in
        // the cell every other one covers — so each is an eligible successor
        // for every other's rows, which is the arrangement that produced the
        // defect.
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new({
            let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
                Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
            CellRuntime::open(&runtime_config(dir.path()), &store)
                .await
                .unwrap()
        }));
        let cell = CellId::ROOT;
        let mut entities = Vec::new();
        for index in 0..(u64::from(PEERS) * PER_PEER) {
            let entity = PersistId::new(1_000 + index);
            seed_entity(&runtime, entity, cell).await;
            entities.push(entity);
        }

        let issuer = support::issuer();
        let server = GatewayServer::spawn(
            GatewayConfig {
                interest_authority: support::interest_authority(
                    (1..=PEERS)
                        .map(|seed| {
                            support::interest_snapshot(
                                support::node(seed),
                                GridId::ROOT,
                                vec![cell],
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
                authorizer: support::authorizer(&issuer),
                identity_clock: support::fixed_clock(support::TOKEN_NOW_MS),
                identity_health: support::available_identity_health(),
                ..GatewayConfig::default()
            },
            runtime.clone() as Arc<dyn Router>,
        )
        .await
        .unwrap();

        let mut endpoints = Vec::new();
        let mut connections = Vec::new();
        for seed in 1..=PEERS {
            let (endpoint, connection) = raw_connection(support::secret(seed), server.addr()).await;
            connection
                .send_control(&GatewayMsg::Hello {
                    token: support::session_token(
                        &issuer,
                        support::node(seed),
                        support::TOKEN_ISSUED_AT_MS,
                        support::TOKEN_TTL_MS,
                    ),
                    node: support::node(seed),
                })
                .await;
            loop {
                let packet = connection
                    .next_payload(Duration::from_secs(5))
                    .await
                    .unwrap();
                if matches!(
                    decode_stream_frame(&packet),
                    Some(GatewayReply::HelloAck { .. })
                ) {
                    break;
                }
            }
            endpoints.push(endpoint);
            connections.push(connection);
        }

        for (index, connection) in connections.iter().enumerate() {
            for offset in 0..PER_PEER {
                let entity = entities[index * PER_PEER as usize + offset as usize];
                connection
                    .send_control(&GatewayMsg::Lease {
                        message: LeaseMsg::Claim {
                            claim_id: orrery_protocol::ClaimId(1),
                            entity,
                            grid: GridId::ROOT,
                            cell,
                            kind: ClaimKind::Weak,
                            basis: ClaimBasis::Explicit,
                            observed: orrery_protocol::SeqPair::default(),
                            tick: Tick::new(1),
                        },
                    })
                    .await;
                loop {
                    let packet = connection
                        .next_payload(Duration::from_secs(5))
                        .await
                        .unwrap();
                    let Some(GatewayReply::Lease { message }) = decode_stream_frame(&packet) else {
                        continue;
                    };
                    match message {
                        LeaseMsg::Grant {
                            entity: granted, ..
                        } if granted == entity => break,
                        LeaseMsg::Deny {
                            entity: denied,
                            reason,
                            ..
                        } if denied == entity => panic!("seeded claim refused: {reason:?}"),
                        _ => {}
                    }
                }
            }
        }

        let held_at_drain_start = still_held(&runtime, &entities).await;
        assert_eq!(
            held_at_drain_start,
            entities.len(),
            "the island must hold every seeded row before it departs"
        );
        let before = server.authority_metrics().snapshot();

        // When: every peer leaves at the same instant.
        for connection in &connections {
            connection.conn().close(0u32.into(), b"island drain");
        }

        // Then: every one of those rows parks. The deadline is far inside the
        // lease TTL on purpose — a drain that only completes once the leases
        // expire is the very thing being ruled out, so waiting `TTL + S` here
        // would make the test pass on the defect.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut outstanding = held_at_drain_start;
        while tokio::time::Instant::now() < deadline && outstanding > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            outstanding = still_held(&runtime, &entities).await;
        }
        assert_eq!(
            outstanding, 0,
            "{outstanding} of {held_at_drain_start} rows were still held after the island left"
        );

        // ...and the dispositions reconcile against what was held, exactly.
        //
        // The registrar issues one disposition per parked row it redistributes
        // — `Reassigned` or `Parked`, never both — so a row that changes hands
        // `h` times inside the drain contributes `h` reassignments and one
        // final park, and the totals are
        //
        //     dispositions = parked + reassigned,   parked = rows,
        //     reassigned   = hops.
        //
        // `parked == rows` is therefore the whole reconciliation: it says
        // every row ended a chain, and it is exactly what fails when a chain
        // ends on a peer that is gone — that row is reassigned and never
        // parked, so `parked` comes up short by however many rows were left
        // out there waiting on a lease TTL nothing would ever renew.
        let after = server.authority_metrics().snapshot();
        let parked = after.parked_without_successor - before.parked_without_successor;
        let reassigned = after.reassigned - before.reassigned;
        assert_eq!(
            parked, held_at_drain_start as u64,
            "every row held at drain start must end a chain in a park: \
             parked={parked} reassigned={reassigned} held={held_at_drain_start}"
        );
        assert_eq!(
            after.duplicate_authority, 0,
            "no row may be taken from a peer that still believes it writes it"
        );

        server.shutdown().await;
        drop(connections);
        drop(endpoints);
    });
}
