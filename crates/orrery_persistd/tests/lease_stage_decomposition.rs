//! The renewal path's stage decomposition attributes time to the stage that
//! spent it, and its two denominators mean what they say.
//!
//! docs/08-persistence.md §2.2.3–§2.2.5 took the renewal path apart below the
//! `Router` boundary and left it near 1.9 us per renewal. Nothing measured
//! what a heartbeat costs *above* it: the peer-state lock, the resolve against
//! the session's own lease table, the second lock, the ack encode.
//! `lease::stages` splits that span. A split is only worth having if the
//! numbers land in the right buckets, and that is what this file guards:
//!
//! 1. **A slow router is billed to `route_us`, not to the unattributed gap.**
//!    The router below the gateway is deliberately delayed, and `route_us`
//!    must move by that delay while `gap_us` — the residual no stage claims —
//!    stays small. Remove the timer around `renew_session_leases` and the same
//!    time reappears in the gap, which is the whole point of the instrument.
//! 2. **The per-heartbeat identity holds.** `session + resolve + route +
//!    recheck + encode + gap` reconstructs `heartbeat_us`, so no stage can
//!    silently overlap another and nothing can hide between two of them.
//! 3. **`heartbeats` and `renewals` are different denominators.** One
//!    heartbeat carrying N pairs increments the first by one and the second by
//!    N, so dividing a per-message sum by a per-lease count is off by the
//!    batch width — the same class of error that made `JournalStageSnapshot`
//!    read ~30x low.
//!
//! Run in one test function against one gateway: the metrics are
//! process-global (like `RouteStageMetrics` and `IntentStageMetrics`, and for
//! the same reason), so two `#[tokio::test]`s in one binary would race for the
//! same counters.

mod lanes;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::cluster::{LeaseRenewal, Router};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::lease::stages::lease_stage_metrics;
use orrery_persistd::{
    CellRuntime, GatewayServer, JournalConfig, MemFenceStore, RuntimeConfig, GATEWAY_ALPN,
};
use orrery_protocol::channels::decode_stream_frame;
use orrery_protocol::{
    CellId, ClaimId, ClaimKind, Epoch, GatewayMsg, GatewayReply, GridId, JournalRecord, LeaseId,
    LeaseMsg, Lsn, NodeId, PersistId, RecordKind, Tick,
};
use tokio::sync::Mutex;

/// How long the router below the gateway is made to take per heartbeat batch.
///
/// Long enough that scheduling noise on a loaded box cannot account for it,
/// which is what makes the attribution falsifiable rather than merely
/// unproven.
const ROUTE_DELAY: Duration = Duration::from_millis(60);

/// The number of leases one heartbeat renews here.
///
/// More than one on purpose: the two denominators are only distinguishable
/// when a message is wider than a single pair, and the batch width is exactly
/// what a reader dividing by the wrong one gets wrong.
const BATCH: usize = 5;

fn secret(n: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed)
}

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        },
        node_id: 0,
        epoch: Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// Give an entity a world row, which a claim needs before it can be granted.
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
            author: secret(9).public(),
            kind: RecordKind::Spawn,
            crc: orrery_persistd::payload_crc(&payload),
            payload,
        })
        .await
        .expect("seed diff is accepted")
        .committed()
        .await
        .expect("seed diff commits");
}

/// A router that delays exactly the batched-renewal call and nothing else.
///
/// Scoped that tightly so the test cannot pass by accident: a delay applied to
/// every router method would also land inside the claim phase, and `route_us`
/// would then be large for a reason the assertion is not about.
struct SlowRenewalRouter {
    inner: Arc<dyn Router>,
    delay: Duration,
}

#[async_trait::async_trait]
impl Router for SlowRenewalRouter {
    async fn apply(
        &self,
        record: orrery_protocol::JournalRecord,
    ) -> Result<Arc<orrery_persistd::AppendHandle>, orrery_persistd::actor::Reject> {
        self.inner.apply(record).await
    }
    async fn apply_fenced(
        &self,
        record: orrery_protocol::JournalRecord,
        holder: NodeId,
        lease_id: LeaseId,
        authority_seq: orrery_protocol::SeqPair,
        now_ms: u64,
    ) -> Result<orrery_persistd::actor::FencedApply, orrery_persistd::actor::Reject> {
        self.inner
            .apply_fenced(record, holder, lease_id, authority_seq, now_ms)
            .await
    }
    async fn commit_rekey(
        &self,
        record: orrery_protocol::JournalRecord,
    ) -> Result<(), orrery_persistd::actor::RekeyError> {
        self.inner.commit_rekey(record).await
    }
    async fn claim_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        kind: ClaimKind,
        now_ms: u64,
    ) -> Result<orrery_persistd::ClaimResult, orrery_persistd::actor::Reject> {
        self.inner
            .claim_lease(grid, cell, entity, holder, kind, now_ms)
            .await
    }
    async fn heartbeat_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<orrery_protocol::Lease>, orrery_persistd::actor::Reject> {
        self.inner
            .heartbeat_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn heartbeat_leases(
        &self,
        grid: GridId,
        holder: NodeId,
        renew: &[LeaseRenewal],
        now_ms: u64,
    ) -> Result<Vec<Option<orrery_protocol::Lease>>, orrery_persistd::actor::Reject> {
        tokio::time::sleep(self.delay).await;
        self.inner
            .heartbeat_leases(grid, holder, renew, now_ms)
            .await
    }
    async fn validate_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
        now_ms: u64,
    ) -> Result<Option<orrery_protocol::Lease>, orrery_persistd::actor::Reject> {
        self.inner
            .validate_lease(grid, cell, entity, holder, lease_id, now_ms)
            .await
    }
    async fn park_lease(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
        holder: NodeId,
        lease_id: LeaseId,
    ) -> Result<Option<orrery_protocol::Lease>, orrery_persistd::actor::Reject> {
        self.inner
            .park_lease(grid, cell, entity, holder, lease_id)
            .await
    }
    async fn sweep_expired_leases(&self, now_ms: u64) -> Vec<orrery_persistd::lease::ParkedLease> {
        self.inner.sweep_expired_leases(now_ms).await
    }
    async fn read(
        &self,
        grid: GridId,
        cell: CellId,
    ) -> Result<orrery_persistd::actor::SnapshotPage, orrery_persistd::Reject> {
        self.inner.read(grid, cell).await
    }
    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        self.inner.has_actor(grid, cell).await
    }
    // Forwarded, not defaulted. `Router` defaults this to `Ok(None)`, and the
    // gateway treats an unresolvable committed cell as an implausible claim —
    // so a wrapper that forgets it denies every claim `NotEligible` and the
    // test never reaches the heartbeat it is about.
    async fn committed_entity_cell(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, orrery_persistd::actor::Reject> {
        self.inner.committed_entity_cell(grid, entity).await
    }
    async fn read_cold(
        &self,
        grid: GridId,
        cell: CellId,
    ) -> Result<Option<orrery_persistd::actor::SnapshotPage>, orrery_persistd::actor::Reject> {
        self.inner.read_cold(grid, cell).await
    }
    async fn inspect_lease(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<
        (
            Option<orrery_protocol::Lease>,
            Option<CellId>,
            Option<orrery_protocol::Lsn>,
        ),
        orrery_persistd::actor::Reject,
    > {
        self.inner.inspect_lease(grid, entity).await
    }
}

#[tokio::test]
async fn stages_attribute_the_time_they_spent_and_the_gap_is_visible() {
    let key = secret(3);
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new({
        let store: Arc<dyn orrery_persistd::checkpoint::CheckpointStore> =
            Arc::new(orrery_persistd::checkpoint::MemCheckpointStore::new());
        CellRuntime::open(&runtime_config(dir.path()), &store)
            .await
            .unwrap()
    }));
    let router: Arc<dyn Router> = Arc::new(SlowRenewalRouter {
        inner: runtime.clone(),
        delay: ROUTE_DELAY,
    });
    let server = GatewayServer::spawn(
        support::authority_config(key.public(), GridId::ROOT, vec![CellId::ROOT]),
        router,
    )
    .await
    .unwrap();
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

    // Claim the batch. These go through `claim_lease`, which the slow router
    // does not delay, so nothing here lands in `route_us`.
    let mut renew = Vec::new();
    for n in 0..BATCH {
        let entity = PersistId::new(500 + n as u64);
        seed_entity(&runtime, entity, CellId::ROOT).await;
        conn.send_control(&GatewayMsg::Lease {
            message: LeaseMsg::Claim {
                claim_id: ClaimId(n as u64 + 1),
                entity,
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                kind: ClaimKind::Weak,
                basis: orrery_protocol::ClaimBasis::Explicit,
                observed: Default::default(),
                tick: Tick::new(1),
            },
        })
        .await;
        // Not a loop: the very next control frame is this claim's answer, and
        // anything else is the failure worth reporting rather than skipping.
        // A `loop` here would spin past a `Deny` and time out saying nothing.
        let Some(pkt) = conn.next_payload(Duration::from_secs(5)).await else {
            panic!("no reply at all to the claim of {entity:?}");
        };
        let lease_id = match decode_stream_frame(&pkt) {
            Some(GatewayReply::Lease {
                message: LeaseMsg::Grant { lease_id, .. },
            }) => lease_id,
            other => panic!("claim of {entity:?} was not granted: {other:?}"),
        };
        renew.push((entity, lease_id));
    }

    // Everything above is setup; the window starts here so the claim phase
    // cannot contribute to any stage under test.
    let metrics = lease_stage_metrics();
    let before = metrics.snapshot();

    conn.send_control(&GatewayMsg::Lease {
        message: LeaseMsg::Heartbeat {
            renew: renew.clone(),
            tick: Tick::new(2),
        },
    })
    .await;
    let (leases, invalid) = loop {
        let pkt = conn.next_payload(Duration::from_secs(10)).await.unwrap();
        if let Some(GatewayReply::Lease {
            message: LeaseMsg::HeartbeatAck { leases, invalid },
        }) = decode_stream_frame(&pkt)
        {
            break (leases, invalid);
        }
    };
    assert_eq!(leases.len(), BATCH, "every claimed lease renews");
    assert!(invalid.is_empty(), "none of them is refused: {invalid:?}");

    let d = metrics.snapshot().delta(&before);

    // (3) The two denominators are different, and by exactly the batch width.
    assert_eq!(d.heartbeats, 1, "one heartbeat message was served");
    assert_eq!(
        d.renewals, BATCH as u64,
        "renewals counts pairs, not messages",
    );

    // (1) The router's time is billed to `route_us`, not to the gap.
    let delay_us = ROUTE_DELAY.as_micros() as u64;
    assert!(
        d.route_us_sum >= delay_us,
        "the router's own delay must land in route_us: {} us against a {} us delay",
        d.route_us_sum,
        delay_us,
    );
    assert!(
        d.gap_us_sum < delay_us / 2,
        "the router's delay must not reappear in the unattributed gap: gap {} us, route {} us",
        d.gap_us_sum,
        d.route_us_sum,
    );

    // (2) The identity holds: the stages plus the gap reconstruct the span.
    let claimed =
        d.session_us_sum + d.resolve_us_sum + d.route_us_sum + d.recheck_us_sum + d.encode_us_sum;
    assert!(
        claimed <= d.heartbeat_us_sum,
        "stages ({claimed} us) cannot exceed the span they decompose ({} us)",
        d.heartbeat_us_sum,
    );
    assert_eq!(
        claimed + d.gap_us_sum,
        d.heartbeat_us_sum,
        "stages plus gap must reconstruct the span exactly",
    );

    // And the span is dominated by the router, which is where the time went.
    assert!(
        d.heartbeat_us_sum >= delay_us,
        "the served span contains the router call: {} us",
        d.heartbeat_us_sum,
    );
}
