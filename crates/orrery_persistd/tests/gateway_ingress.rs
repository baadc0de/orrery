//! Bulk ingress admission: what a connection does with diffs it cannot route
//! in time (docs/11-roadmap.md §P2, docs/08-persistence.md §2.1).
//!
//! The defect this file pins is a *hidden queue*. The datagram reader pushes
//! into an unbounded channel and never blocks; the receive loop used to
//! `await` a route permit before spawning each diff's route, so a connection
//! at its concurrency cap stopped draining its own queue while the reader
//! kept filling it. The delay was real and invisible: the gateway stamped
//! `received_at` when it finally picked a message up, so its own span read
//! 30 ms while the client observed 2 s (`gateway_ingress_queue_ms` p99
//! 2 045 ms against a 5 ms D16 budget).
//!
//! Two claims, and the second is the one that makes the first honest:
//!
//! 1. **A saturated bulk lane refuses instead of queueing.** The excess is
//!    dropped — legitimate on this lane, which is unreliable QUIC datagrams
//!    with idempotent `(entity, tick)` records and a client that resends
//!    until acked — and the connection keeps answering the traffic behind it.
//! 2. **Every refusal is counted.** A silent drop would be exactly as
//!    dishonest as the hidden queue it replaced, so overload has to read as a
//!    number. `GatewayIngressMetrics` is that number, collected with no flag
//!    set, and this asserts it moves.

mod lanes;
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::actor::{Reject, SnapshotPage};
use orrery_persistd::journal::AppendHandle;
use orrery_persistd::{GatewayConfig, GatewayServer, Router, GATEWAY_ALPN};
use orrery_protocol::{
    CellId, DiffUplink, GatewayMsg, GatewayReply, GridId, JournalRecord, PersistId, RecordKind,
    Tick,
};

/// The rekey entity, chosen distinct from every flooded one: its reply is how
/// the test observes that the receive loop is still alive.
const PROBE: PersistId = PersistId::new(9_999_999);

/// A gateway with the downstream route valve **off**.
///
/// The saturation test below holds a connection at its route cap by parking
/// every route in the router, and the valve exists precisely to stop a route
/// from parking. With it on at its default 25 ms the parked routes are shed,
/// the permits come back, and the cap is never reached — so this states the
/// policy the test is about instead of inheriting one that contradicts it.
fn gateway_config() -> GatewayConfig {
    GatewayConfig {
        route_admission_wait_us: 0,
        ..support::authority_config(support::node(7), GridId::ROOT, vec![CellId::ROOT])
    }
}

/// The same gateway with the valve on, at a budget short enough that a test
/// does not have to sleep through the shipped one.
fn valved_gateway_config(route_admission_wait_us: u64) -> GatewayConfig {
    GatewayConfig {
        route_admission_wait_us,
        ..support::authority_config(support::node(7), GridId::ROOT, vec![CellId::ROOT])
    }
}

/// A router whose every append parks forever, counting arrivals.
///
/// One parked `apply` holds one route permit for the life of the test, which
/// is the whole point: it is the only way to put a connection at its
/// concurrency cap deterministically, without depending on how fast a journal
/// happens to be on the machine running this.
struct ParkingRouter {
    entered: AtomicUsize,
}

#[async_trait::async_trait]
impl Router for ParkingRouter {
    async fn apply(&self, _record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("pending never resolves")
    }

    async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
        Ok(SnapshotPage::default())
    }

    async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
        true
    }
}

/// A router that answers immediately, with a completed handle.
struct AnsweringRouter;

#[async_trait::async_trait]
impl Router for AnsweringRouter {
    async fn apply(&self, _record: JournalRecord) -> Result<Arc<AppendHandle>, Reject> {
        Ok(AppendHandle::completed(orrery_protocol::Lsn::new(1, 1)))
    }

    async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
        Err(Reject::JournalClosed)
    }

    async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
        false
    }
}

struct Client {
    _endpoint: iroh::Endpoint,
    conn: lanes::GatewayLanes,
}

/// Dial `server`, complete admission and `Hello`, and return both lanes.
async fn dial(server: &GatewayServer) -> Client {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(support::secret(7))
        .bind()
        .await
        .expect("bind client endpoint");
    let node = endpoint.id();
    let conn = endpoint
        .connect(server.addr(), GATEWAY_ALPN)
        .await
        .expect("connect to gateway");
    let mut admission = conn.accept_uni().await.expect("admission stream");
    assert_eq!(
        admission.read_to_end(16).await.expect("admission byte"),
        vec![0u8]
    );
    let conn = lanes::GatewayLanes::attach(conn);
    conn.send_control(&GatewayMsg::VersionedHello {
        token: support::valid_session_token(node),
        node,
        version: orrery_protocol::PROTOCOL_VERSION,
    })
    .await;
    assert!(matches!(
        conn.next_reply(Duration::from_secs(5)).await,
        Some(GatewayReply::HelloAck { .. })
    ));
    Client {
        _endpoint: endpoint,
        conn,
    }
}

fn diff(entity: PersistId, kind: RecordKind) -> GatewayMsg {
    GatewayMsg::Diff {
        diff: DiffUplink {
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(1),
            kind,
            payload: Bytes::from_static(b"state"),
            seq: 1,
            // No lease: `apply_fenced` falls through to `apply`, which parks.
            // What is under test is the admission decision in front of the
            // route, not the fence behind it.
            lease_id: None,
            authority_seq: None,
        },
    }
}

#[tokio::test]
async fn a_saturated_bulk_lane_sheds_the_excess_and_keeps_serving_the_connection() {
    let router = Arc::new(ParkingRouter {
        entered: AtomicUsize::new(0),
    });
    let server = GatewayServer::spawn(gateway_config(), router.clone())
        .await
        .expect("spawn gateway");
    let metrics = Arc::clone(server.metrics());
    let client = dial(&server).await;

    // Flood until the connection's route cap is saturated and the gateway
    // starts refusing. Sent in small batches with a yield between them: these
    // are datagrams, and a burst large enough to overrun the local send
    // buffer would be lost before the gateway ever made an admission
    // decision — which is a different experiment.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut sent = 0u64;
    while metrics.ingress.snapshot().shed_saturated == 0 {
        assert!(
            Instant::now() < deadline,
            "no diff was ever shed after {sent} sends: either the cap is not \
             reached or the loop is parked waiting for a permit"
        );
        for _ in 0..32 {
            sent += 1;
            client
                .conn
                .send_state(&diff(PersistId::new(sent), RecordKind::ComponentDiff));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let shed = metrics.ingress.snapshot();
    assert!(
        shed.admitted > 0,
        "the connection routed before it refused: {shed:?}"
    );
    assert_eq!(
        shed.shed_stale, 0,
        "nothing waited past the ingress deadline here; the refusal under \
         test is the route cap: {shed:?}"
    );
    assert!(
        router.entered.load(Ordering::SeqCst) > 0,
        "the admitted diffs reached the router and are parked in it"
    );

    // The claim this whole change exists for: with every route slot occupied,
    // the receive loop is still draining its queue. A rekey diff is answered
    // inline, before any permit is considered, so its `BulkNack` proves the
    // loop reached a message that arrived *after* the ones being refused.
    // Under the old inline `acquire_owned().await` the loop is parked on the
    // first excess diff and this reply never comes.
    client.conn.send_state(&diff(PROBE, RecordKind::Rekey));
    let reply = client.conn.next_reply(Duration::from_secs(10)).await;
    match reply {
        Some(GatewayReply::BulkNack { entity, .. }) => assert_eq!(
            entity, PROBE,
            "the only reply on this connection is the probe's: a shed diff is \
             answered with silence, not with a nack that would tell the peer \
             to discard the write"
        ),
        other => panic!("saturated connection stopped serving: {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_router_that_will_not_answer_is_shed_downstream_and_counted_separately() {
    // The regression this pins is the one the lease-lane offload created.
    // `MAX_INGRESS_QUEUE_WAIT_US` is evaluated on arrival age at the instant
    // the receive loop dequeues a message. Once the loop went instant
    // (`gateway_ingress_queue_ms` p99 0.05 ms) that check passed for
    // everything and bounded nothing, while the standing queue moved
    // downstream of it — into the entity gate inside `Router::apply_fenced`.
    // A deadline only bounds a wait it is evaluated *after*, so here the same
    // budget is applied around the router round trip, and the refusal is its
    // own counter because it is its own queue.
    //
    // `ParkingRouter` never answers, so every admitted diff must come back as
    // `shed_slow_route` and none as an ack. Ten milliseconds rather than the
    // shipped 25 so this does not become a sleep test.
    let router = Arc::new(ParkingRouter {
        entered: AtomicUsize::new(0),
    });
    let server = GatewayServer::spawn(valved_gateway_config(10_000), router.clone())
        .await
        .expect("spawn gateway");
    let metrics = Arc::clone(server.metrics());
    let client = dial(&server).await;

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut sent = 0u64;
    while metrics.ingress.snapshot().shed_slow_route == 0 {
        assert!(
            Instant::now() < deadline,
            "no diff was ever shed downstream after {sent} sends, against a \
             router that never answers: the route-admission budget is not \
             being applied around the router round trip"
        );
        for _ in 0..32 {
            sent += 1;
            client
                .conn
                .send_state(&diff(PersistId::new(sent), RecordKind::ComponentDiff));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let shed = metrics.ingress.snapshot();
    assert!(
        router.entered.load(Ordering::SeqCst) > 0,
        "the diffs reached the router: this is a refusal downstream of \
         admission, not the ingress check firing again: {shed:?}"
    );
    assert!(
        shed.admitted > 0,
        "a downstream refusal is preceded by an admission, and the two \
         overlap by design: {shed:?}"
    );
    assert_eq!(
        (shed.shed_stale, shed.shed_saturated),
        (0, 0),
        "and it is counted apart from the two ingress refusals, because an \
         operator reading one number must not have to guess which queue \
         grew: {shed:?}"
    );

    // The silence convention, unchanged from the ingress refusals: an
    // un-acked diff stays pending in the peer's scheduler and is re-offered,
    // where a `BulkNack` would tell it to discard the write. The rekey probe
    // is answered inline before any routing, so its reply is the only one
    // this connection can produce.
    client.conn.send_state(&diff(PROBE, RecordKind::Rekey));
    match client.conn.next_reply(Duration::from_secs(10)).await {
        Some(GatewayReply::BulkNack { entity, .. }) => assert_eq!(
            entity, PROBE,
            "a diff shed downstream is answered with silence, not with a nack"
        ),
        other => panic!("shedding connection stopped serving: {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_gateway_that_is_keeping_up_sheds_nothing_downstream() {
    // The mutation check for the test above. `ParkingRouter` is what makes
    // that one fire; against a router that answers, the same budget must
    // never refuse anything — a valve that trips on a healthy gateway is a
    // throughput bug wearing a metric.
    let router = Arc::new(AnsweringRouter);
    let server = GatewayServer::spawn(valved_gateway_config(10_000), router)
        .await
        .expect("spawn gateway");
    let metrics = Arc::clone(server.metrics());
    let client = dial(&server).await;

    for entity in 1..=64u64 {
        client
            .conn
            .send_state(&diff(PersistId::new(entity), RecordKind::ComponentDiff));
    }
    // Every reply, then a full budget of quiet on top, so a late shed still
    // lands inside the observation window.
    let mut acked = 0;
    while acked < 64 {
        match client.conn.next_reply(Duration::from_secs(10)).await {
            Some(GatewayReply::BulkAck { .. }) => acked += 1,
            other => panic!("an answering router must acknowledge: {other:?}"),
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    let shed = metrics.ingress.snapshot();
    assert_eq!(
        (shed.shed_slow_route, shed.shed_stale, shed.shed_saturated),
        (0, 0, 0),
        "nothing is refused on a gateway that is keeping up: {shed:?}"
    );
    assert_eq!(shed.admitted, 64, "{shed:?}");

    server.shutdown().await;
}
