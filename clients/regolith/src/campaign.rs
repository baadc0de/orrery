//! The joined campaign session (#386): dial the island host over slice 1's
//! exterior wire, run the human's orders through the same intent pipeline the
//! local session uses, apply replicated state off the same surface slice 1's
//! headless peer consumed, and measure the link from observed packet outcomes.
//!
//! # What a banked hour requires that [`crate::LocalSession`] cannot give
//!
//! A campaign hour banks only if the joined-session state machine ran: a real
//! iroh join completed against a host that verified this process's transport
//! identity, and every accumulated tick was driven through that link.
//! [`crate::LocalSession`] stays for offline and smoke use and produces no
//! campaign evidence — its [`crate::session::LiveProgress`] equivalent simply
//! never exists.
//!
//! # Where every measured number comes from (the point of this module)
//!
//! **Uplink loss** — the share of sequenced uplink datagrams the host's
//! *impaired router* reports as `Dropped`, per the #393 ack contract. Not QUIC
//! acknowledgments: the uplink rides a reliable stream whose transport acks
//! the write before the router decides anything, so a figure built there would
//! report success for exactly the frames impairment dropped.
//!
//! **Downlink loss** — gaps in received replication ticks. Each broadcast
//! carries the sender's absolute tick; bots broadcast on a fixed stride, so a
//! sender whose tick advances by more than one stride has had broadcasts lost
//! in between. Reordered arrivals (jitter delays whole packets) are not loss:
//! they carry already-seen-or-past ticks and only refresh timing.
//!
//! **Jitter** — the deviation of consecutive inter-arrival intervals of
//! downlink datagram frames, sampled once per arrival, pooled across senders.
//! Steady cadence under delay spikes produces large deviations; the p50/p99
//! of those deviations is what the F3 pane shows next to the configured
//! profile.
//!
//! Nothing here echoes configuration. The configured profile exists only to
//! be *compared against* the measurement, which is what
//! `SessionRecord::impairment_mismatch` is for.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::math::{DVec3, IVec3};
use bevy::prelude::*;
use bytes::Bytes;

use orrery_core::{CoreCodec, Executor, InputLogProducer};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::order::{Order, Outcome};
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_games::regolith::{campaign_spawn_pose, collision_candidate, REGOLITH_RULESET};
use orrery_games::{Game, Regolith};
use orrery_protocol::channels::{decode_delivered_input, decode_replication};
use orrery_protocol::UniverseSeed;
use orrery_protocol::{
    cell_id_from_metres, CellId, FrameHead, PersistId, RecordSource, Tick, WitnessMsg,
    DEFAULT_CELL_EDGE_M,
};

use crate::intent::{decode_packet, encode_orders, Controls, IntentPipeline, OrderPacket};
use crate::net::{
    self, CampaignLink, HostAddress, JoinRequest, Lane, UplinkAck, UplinkDatagram, UplinkOutcome,
};
use crate::session::{Actor, CampaignSession, ConfiguredImpairment, SessionRecord};

/// Broadcasts per second the harness runs (`send_hz`), mirrored so the
/// client's state cadence matches what a bot's `broadcast_state` does.
const SEND_HZ: u32 = 20;

/// Ticks between state broadcasts (`TICK_HZ / SEND_HZ`, floored at one).
const SEND_EVERY_TICKS: u64 = (orrery_core::TICK_HZ / SEND_HZ) as u64;

/// How long a remote state remains drawable without a replication refresh.
///
/// This matches the headless peer's two-second replica lifetime: twice the
/// one-hertz proxy floor, so a legal low-rate stream survives while a craft
/// that left this client's interest set does not remain as a frozen ghost.
const REPLICA_TTL_TICKS: u64 = 120;

/// One remote craft's receive-side lifetime and measurement history.
///
/// `installed` becomes false on expiry, but the last refresh is deliberately
/// retained so an eventual re-install can report the whole silent interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplicaFreshness {
    last_refresh_tick: u64,
    last_authoritative_tick: u64,
    installed: bool,
}

/// State claims are authored at the existing headless producer's 2 Hz cadence.
const CLAIM_EVERY_TICKS: u64 = 30;

/// Frames use the headless producer's D16-derived 10-tick cadence.
const WITNESS_FRAME_TICKS: u16 = 10;

/// P4's bounded witness fan-out (the same ring width as the host).
const MAX_WITNESS_LINKS: usize = 7;

/// How long the dial may take before the join attempt is declared failed.
/// The handshake itself bounds each read at ten seconds; this adds dial and
/// bind slack.
const JOIN_DEADLINE_SECS: u64 = 30;

/// Launch material for a joined campaign session.
///
/// The host NodeId, the slot this process occupies, the persistent client
/// transport identity, and the invite material (#387): the pre-minted session
/// UUIDv7 and, when the host demands one, the operator-signed session token.
#[derive(Debug, Clone)]
pub struct CampaignConfig {
    /// Hex node id of the hosting process, from its listening line.
    pub host_node_hex: String,
    /// Optional direct socket `<ip:port>`, for proofs without discovery.
    pub host_direct: Option<String>,
    /// The swarm slot this client occupies. It derives the entity id, but not
    /// the durable transport identity.
    pub slot: usize,
    /// Coordinator-issued session identity for the banking row, presented to
    /// the host at join (#345 §8). For a campaign session this is the
    /// pre-minted UUIDv7 the invite carries.
    pub session_id: String,
    /// Hex-encoded `SessionTokenV1` from the invite material, presented to
    /// the host at join. `None` joins hosts that do not require one.
    pub session_token_hex: Option<String>,
    /// UTC start stamp for the banking row.
    pub wall_start_utc: String,
    /// The impairment the operator says the host injects. Compared against
    /// the measurement; never substituted for it.
    pub configured: ConfiguredImpairment,
    /// Persistent client identity presented at admission, used for the QUIC
    /// handshake and the finished measurement signature.
    pub transport_secret: iroh_base::SecretKey,
}

impl CampaignConfig {
    /// The actor behind a campaign session built from this config.
    #[must_use]
    pub fn actor(&self) -> Actor {
        Actor::Human
    }
}

/// The joined-session state machine. A campaign hour banks only out of
/// [`JoinState::Joined`].
#[derive(Debug, Clone, PartialEq)]
pub enum JoinState {
    /// The dial thread is running.
    Dialing,
    /// The handshake completed; the link is live.
    Joined,
    /// The host answered with `Reject`; the reason names itself.
    Refused(String),
    /// Dial failure, timeout, or protocol error.
    Failed(String),
    /// The link ended after having been live.
    Closed {
        /// Whether the host sent its goodbye marker before the end.
        host_said_goodbye: bool,
    },
}

/// One downlink arrival's measurement yield.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrival {
    /// Broadcasts expected since the previous arrival that did not arrive.
    pub missing: u64,
    /// Deviation of this interval from the previous one, when there was a
    /// previous interval to deviate from.
    pub deviation_ms: Option<u64>,
}

/// Per-sender accounting for replication-tick gaps and arrival intervals.
///
/// Bots broadcast every stride ticks (20 Hz at 60 Hz sim). The stride is
/// *learned* from the first two arrivals rather than assumed: the criterion's
/// send cadence is harness configuration, and a measurement that assumed it
/// would be configuration echo by another door.
#[derive(Debug, Default)]
pub struct DownlinkTracker {
    senders: BTreeMap<u32, SenderTrack>,
    total_missing: u64,
}

#[derive(Debug, Default)]
struct SenderTrack {
    last_tick: Option<u64>,
    stride: Option<u64>,
    last_interval_ms: Option<f64>,
    last_arrival_ms: Option<f64>,
}

impl DownlinkTracker {
    fn last_tick(&self, sender: u32) -> Option<u64> {
        self.senders.get(&sender).and_then(|track| track.last_tick)
    }

    /// Account for one replication packet from `sender` carrying tick `at`,
    /// arriving `now_ms` milliseconds after session start.
    ///
    /// Reordered arrivals — an older tick than the newest seen — are counted
    /// as arrivals (they are timing samples) but never as gaps: their ticks
    /// were already accounted for when a later tick closed past them.
    pub fn record(&mut self, sender: u32, at: u64, now_ms: f64) -> Arrival {
        let track = self.senders.entry(sender).or_default();
        let mut missing = 0u64;
        match track.last_tick {
            None => {
                track.last_tick = Some(at);
            }
            Some(last) if at <= last => {
                // Late or duplicate: no gap arithmetic on stale ticks.
            }
            Some(last) => {
                let delta = at - last;
                // First delta IS the stride candidate unless zero (two
                // broadcasts sharing a tick cannot happen at a fixed cadence,
                // but refuse to divide by zero).
                let stride = *track.stride.get_or_insert_with(|| delta.max(1));
                // Broadcasts due between the two arrivals, rounded to the
                // nearest multiple of the learned stride; one of them is the
                // packet that just arrived, the rest are missing.
                let due = ((delta as f64 / stride as f64).round() as u64).max(1);
                missing = due - 1;
                track.last_tick = Some(at);
            }
        }
        // Interval/deviation bookkeeping. The very first arrival establishes
        // the baseline and yields no deviation; every later one deviates from
        // the interval before it.
        let timing = track.last_arrival_ms.map(|last| {
            let interval_ms = (now_ms - last).abs();
            let deviation = track
                .last_interval_ms
                .map(|previous| (interval_ms - previous).abs().round().max(0.0) as u64);
            (interval_ms, deviation)
        });
        match timing {
            Some((interval_ms, _deviation)) => {
                track.last_interval_ms = Some(interval_ms);
                track.last_arrival_ms = Some(now_ms);
            }
            None => {
                track.last_arrival_ms = Some(now_ms);
            }
        }
        self.total_missing += missing;
        Arrival {
            missing,
            deviation_ms: timing.and_then(|(_, deviation)| deviation),
        }
    }

    /// Broadcasts this tracker has judged lost, across all senders.
    #[must_use]
    pub fn total_missing(&self) -> u64 {
        self.total_missing
    }

    /// Distinct senders seen.
    #[must_use]
    pub fn senders(&self) -> usize {
        self.senders.len()
    }
}

/// What one driven tick produced, for the skin systems downstream.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Orders the shared pipeline emitted for this tick.
    pub intents: usize,
    /// Events the local prediction step raised (tracers, shot feedback).
    pub events: Vec<Outcome>,
    /// Authoritative inputs delivered to this entity and consumed this tick.
    pub delivered: Vec<DeliveredOrder>,
}

/// One accepted cross-authority delivery, preserved for presentation readers.
///
/// The order is still applied through the ordinary ruleset input path. Keeping
/// this copy in [`TickReport`] lets the skin read authoritative statements that
/// the recipient consumes without re-emitting, without reconstructing them
/// from player-authored intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredOrder {
    /// Authority whose outcome produced the delivery.
    pub from: PersistId,
    /// Local authority the envelope addressed.
    pub recipient: PersistId,
    /// Canonical order accepted from the delivery envelope.
    pub order: Order,
}

impl DeliveredOrder {
    /// Rebuild a terminal presentation statement that the local step consumes
    /// without re-emitting.
    ///
    /// The remaining delivered orders either mutate state that the skin reads
    /// from the executor (`LockConfirmed`, credits, pickup results, visibility
    /// and collision resolution), or are work requests whose recipient step
    /// emits its own current outcome (`Damage`, `GrabAttempt`, `LockRequested`).
    /// Player-authored orders have no delivered presentation meaning.
    #[must_use]
    pub fn feedback_outcome(&self) -> Option<Outcome> {
        match &self.order {
            Order::LockRefused { target } => Some(Outcome::LockRefused {
                locker: self.recipient,
                target: *target,
            }),
            Order::LockBroken { target, reason } => Some(Outcome::LockBroken {
                locker: self.recipient,
                target: *target,
                reason: *reason,
            }),
            Order::ShotResolved { target, result } => Some(Outcome::ShotResolved {
                attacker: self.recipient,
                target: *target,
                result: *result,
            }),
            Order::Thrust { .. }
            | Order::Lock { .. }
            | Order::LockRequested { .. }
            | Order::LockConfirmed { .. }
            | Order::Fire
            | Order::Damage { .. }
            | Order::Grab { .. }
            | Order::GrabAttempt { .. }
            | Order::PickupGranted { .. }
            | Order::PickupDenied
            | Order::KillCredit
            | Order::RockCredit { .. }
            | Order::BloomPopulationChanged { .. }
            | Order::ClaimCover { .. }
            | Order::LockVisibility { .. }
            | Order::Collide { .. }
            | Order::CollisionResolved { .. } => None,
        }
    }
}

/// What one dial attempt reports back to the render loop: the endpoint (kept
/// alive with the pumps) or why the join did not happen.
type PendingJoin = Arc<Mutex<Option<Result<(iroh::Endpoint, Arc<CampaignLink>), String>>>>;

/// The tokio runtime and endpoint keeping the pumps alive. Dropping either
/// closes the connection under the link (#385's joined-but-deaf lesson), so
/// both are held for the life of the session.
struct NetGuard {
    _runtime: Arc<tokio::runtime::Runtime>,
    _endpoint: Option<iroh::Endpoint>,
}

/// A joined campaign session: transport, local prediction, replicated view,
/// accumulator, and the measurements the banking row is built from.
pub struct CampaignRuntime {
    config: CampaignConfig,
    state: JoinState,
    launched_at: Instant,
    link: Option<Arc<CampaignLink>>,
    pending_join: PendingJoin,
    net: Option<NetGuard>,

    executor: Executor<Regolith>,
    pipeline: IntentPipeline,
    witness_log: InputLogProducer,
    entity: PersistId,
    cell_edge_m: f64,

    tick: Tick,
    started_at: Instant,
    uplink_sequence: u64,
    uplink_sent: u64,
    uplink_shed: u64,
    uplink_acks: u64,
    uplink_dropped: u64,
    downlink: DownlinkTracker,
    downlink_arrivals: u64,
    undecodable: u64,
    delivered_unroutable: u64,
    delivered_foreign: u64,
    pending_delivered: Vec<DeliveredOrder>,
    replica_freshness: BTreeMap<PersistId, ReplicaFreshness>,
    focus: Option<PersistId>,
    latest_cell: Option<CellId>,

    campaign: CampaignSession,
    record_written: bool,
}

impl CampaignRuntime {
    /// Build the session and start its dial thread. Bevy-facing systems then
    /// poll [`Self::poll_join`] until the state leaves [`JoinState::Dialing`].
    #[must_use]
    pub fn launch(config: CampaignConfig, seed: UniverseSeed) -> Self {
        let game = Regolith::honest();
        let mut executor = Executor::new(game, seed);
        let entity = PersistId::new(config.slot as u64 + 1);
        // The exterior is the last participant in the host's crowd. Its
        // signed tick-zero state must use the same campaign pose as the host's
        // headless peers; the compact scenario ring is a different world and
        // leaves every host-owned target kilometres outside weapon reach.
        let (pos, yaw_urad) = campaign_spawn_pose(config.slot, config.slot.saturating_add(1));
        executor.insert(
            entity,
            RegolithState::Craft(Craft::spawned(
                Archetype::for_slot(config.slot as u64),
                pos,
                yaw_urad,
            )),
        );
        let pipeline = IntentPipeline::new(seed, entity, config.slot as u64, Vec::new());
        let mut witness_log = InputLogProducer::new(
            config.transport_secret.clone(),
            entity,
            REGOLITH_RULESET,
            0,
            CLAIM_EVERY_TICKS,
            WITNESS_FRAME_TICKS,
        );
        let anchor_state = executor.state(entity).expect("spawn inserted").clone();
        let anchor_claim = witness_log.anchor(0, &anchor_state);
        let anchor = net::AnchorFrame {
            claim_json: serde_json::to_vec(&anchor_claim).expect("StateClaim serializes"),
            state: anchor_state.to_canonical(),
        };
        let campaign = CampaignSession::new(
            config.session_id.clone(),
            config.wall_start_utc.clone(),
            config.actor(),
            config.configured,
        );

        let pending_join: PendingJoin = Arc::new(Mutex::new(None));

        // Parse the dial target up front: a malformed node id is a named
        // failure surfaced through the state machine, not a thread that
        // silently never reports.
        let address = match HostAddress::parse(&config.host_node_hex, config.host_direct.as_deref())
        {
            Ok(address) => Some(address),
            Err(reason) => {
                *pending_join.lock().expect("pending lock") = Some(Err(reason));
                None
            }
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build();
        let runtime = match runtime {
            Ok(runtime) => Some(Arc::new(runtime)),
            Err(error) => {
                *pending_join.lock().expect("pending lock") =
                    Some(Err(format!("tokio runtime: {error}")));
                None
            }
        };

        if let (Some(address), Some(runtime)) = (&address, &runtime) {
            let thread_pending = Arc::clone(&pending_join);
            let thread_runtime = Arc::clone(runtime);
            let prefer = config
                .host_direct
                .as_deref()
                .and_then(|socket| socket.parse().ok());
            let slot = config.slot;
            let transport_secret = config.transport_secret.clone();
            let client_rev = crate::BUILD_REV.to_owned();
            let session_id = config.session_id.clone();
            let anchor = anchor.clone();
            // A malformed token hex is a named join failure, not a silently
            // tokenless join the host would refuse with a confusing reason.
            let token = match config.session_token_hex.as_deref().map(decode_hex) {
                None => Ok(None),
                Some(Ok(bytes)) => Ok(Some(bytes)),
                Some(Err(reason)) => Err(reason),
            };
            let address = address.clone();
            let _handle = std::thread::Builder::new()
                .name("regolith-campaign-dial".to_owned())
                .spawn(move || {
                    let joined = thread_runtime.block_on(async move {
                        let endpoint = net::bind(transport_secret).await?;
                        let addr = address.to_addr(prefer);
                        let request = JoinRequest {
                            client_rev,
                            session_id: Some(session_id),
                            token: token?,
                        };
                        let deadline = std::time::Duration::from_secs(JOIN_DEADLINE_SECS);
                        let link = tokio::time::timeout(
                            deadline,
                            net::remote_join(&endpoint, addr, &request, slot, Some(anchor)),
                        )
                        .await
                        .map_err(|_| format!("the join did not complete within {deadline:?}"))??;
                        Ok((endpoint, Arc::new(link)))
                    });
                    *thread_pending.lock().expect("pending lock") = Some(joined);
                });
        }

        let mut this = Self::assemble(
            config,
            executor,
            pipeline,
            witness_log,
            entity,
            campaign,
            pending_join,
            Instant::now(),
        );
        this.net = runtime.map(|runtime| NetGuard {
            _runtime: runtime,
            _endpoint: None,
        });
        this
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        config: CampaignConfig,
        executor: Executor<Regolith>,
        pipeline: IntentPipeline,
        witness_log: InputLogProducer,
        entity: PersistId,
        campaign: CampaignSession,
        pending_join: PendingJoin,
        launched_at: Instant,
    ) -> Self {
        Self {
            state: JoinState::Dialing,
            link: None,
            net: None,
            launched_at,
            cell_edge_m: DEFAULT_CELL_EDGE_M,
            tick: Tick::new(0),
            started_at: Instant::now(),
            uplink_sequence: 0,
            uplink_sent: 0,
            uplink_shed: 0,
            uplink_acks: 0,
            uplink_dropped: 0,
            downlink: DownlinkTracker::default(),
            downlink_arrivals: 0,
            undecodable: 0,
            delivered_unroutable: 0,
            delivered_foreign: 0,
            pending_delivered: Vec::new(),
            replica_freshness: BTreeMap::new(),
            focus: None,
            latest_cell: None,
            record_written: false,
            config,
            executor,
            pipeline,
            witness_log,
            entity,
            campaign,
            pending_join,
        }
    }

    /// Take the dial thread's result, if it has landed yet.
    pub fn poll_join(&mut self) {
        if !matches!(self.state, JoinState::Dialing) {
            return;
        }
        let result = self.pending_join.lock().expect("pending join lock").take();
        match result {
            None => {
                if self.launched_at.elapsed().as_secs() > JOIN_DEADLINE_SECS + 5 {
                    self.state =
                        JoinState::Failed("the dial thread never reported back".to_owned());
                }
            }
            Some(Ok((endpoint, link))) => {
                if let Some(net) = self.net.as_mut() {
                    net._endpoint = Some(endpoint);
                }
                self.link = Some(link);
                self.state = JoinState::Joined;
            }
            Some(Err(reason)) => {
                // A host refusal is a named outcome, not a malfunction; the
                // operator sees why. Anything else failed outright.
                if reason.contains("refused the join") {
                    let why = reason
                        .strip_prefix("the host refused the join: ")
                        .unwrap_or(&reason)
                        .to_owned();
                    self.state = JoinState::Refused(why);
                } else {
                    self.state = JoinState::Failed(reason);
                }
                // Release the guard: nothing will ever hold a connection.
                self.net = None;
            }
        }
    }

    /// The current join state.
    #[must_use]
    pub fn state(&self) -> &JoinState {
        &self.state
    }

    /// Ticks driven while joined — the accumulator's connected count in
    /// ticks. Zero until [`JoinState::Joined`].
    #[must_use]
    pub fn joined_ticks(&self) -> u64 {
        self.tick.0
    }

    /// Launch material, including the configured profile shown *beside* the
    /// measurement, never instead of it.
    #[must_use]
    pub fn config(&self) -> &CampaignConfig {
        &self.config
    }

    /// The live link, exactly when the state machine has reached Joined.
    #[must_use]
    pub fn link(&self) -> Option<&Arc<CampaignLink>> {
        if matches!(self.state, JoinState::Joined) {
            self.link.as_ref()
        } else {
            None
        }
    }

    /// This client's persistent id (slot-derived, as a bot's is).
    #[must_use]
    pub fn entity(&self) -> PersistId {
        self.entity
    }

    /// The remote craft the duel view follows: lowest replicated id seen.
    #[must_use]
    pub fn focus(&self) -> Option<PersistId> {
        self.focus
    }

    /// Read-only executor access for rendering and combat views.
    #[must_use]
    pub fn executor(&self) -> &Executor<Regolith> {
        &self.executor
    }

    /// The campaign accumulator this session feeds.
    #[must_use]
    pub fn accumulator(&self) -> &CampaignSession {
        &self.campaign
    }

    /// Loss measured from this session's own packet outcomes, so far.
    #[must_use]
    pub fn observed_loss_pct(&self) -> f64 {
        self.campaign.observed_loss_pct()
    }

    /// Jitter p50 measured from downlink arrival intervals, so far.
    #[must_use]
    pub fn observed_jitter_p50_ms(&self) -> u64 {
        self.campaign.observed_jitter_p50_ms()
    }

    /// Jitter p99 measured from downlink arrival intervals, so far.
    #[must_use]
    pub fn observed_jitter_p99_ms(&self) -> u64 {
        self.campaign.observed_jitter_p99_ms()
    }

    /// Uplink datagrams queued but refused by the bounded channel: visible
    /// backpressure, counted like the host counts its downlink drops.
    #[must_use]
    pub fn uplink_shed(&self) -> u64 {
        self.uplink_shed
    }

    /// Uplink datagrams sequenced out so far.
    #[must_use]
    pub fn uplink_sent(&self) -> u64 {
        self.uplink_sent
    }

    /// `(acks received, acks reporting Dropped)` — the router's settled
    /// decisions for this client's uplink (#393 contract).
    #[must_use]
    pub fn uplink_acks(&self) -> (u64, u64) {
        (self.uplink_acks, self.uplink_dropped)
    }

    /// `(downlink arrivals, broadcasts those arrivals found missing)`.
    #[must_use]
    pub fn downlink_accounting(&self) -> (u64, u64) {
        (self.downlink_arrivals, self.downlink.total_missing())
    }

    /// Newest replication tick accounted for from `sender`, if one arrived.
    ///
    /// This is the receiver-side progress marker fixtures use to establish a
    /// quiescent cut through an ordered downlink stream before reconciling the
    /// sender's ledger.
    #[must_use]
    pub fn downlink_last_tick(&self, sender: u32) -> Option<u64> {
        self.downlink.last_tick(sender)
    }

    /// Replication packets that decoded to nothing this session recognises.
    #[must_use]
    pub fn undecodable(&self) -> u64 {
        self.undecodable
    }

    /// The most recent cell this craft's position committed to.
    #[must_use]
    pub fn latest_cell(&self) -> Option<CellId> {
        self.latest_cell
    }

    /// Drive one simulation tick of the joined session.
    ///
    /// Order mirrors the runner: produce orders through the shared pipeline,
    /// step the local craft (prediction), broadcast canonical state on the
    /// window's last tick, drain inbound frames, then account the tick.
    /// The order packet is logged to the same JSONL stream the local session
    /// uses, so a human recording replays through the ordinary harness path
    /// (`recorded_human_order_log_replays_through_game_harness`).
    /// Returns what the skin needs; returns an empty report when not joined.
    pub fn advance(
        &mut self,
        controls: Controls,
        sink: &mut crate::telemetry::JsonlTelemetry,
    ) -> TickReport {
        self.poll_join();
        // Clone the handle, not a borrow: everything below mutates the
        // runtime while the pumps own the link's other half.
        let Some(link) = self.link.clone() else {
            return TickReport::default();
        };
        if !matches!(self.state, JoinState::Joined) {
            return TickReport::default();
        }
        let tick = self.tick;

        // The existing headless producer's order is load-bearing: a claim at
        // T commits to pre-step state, while T's inputs and resulting hash land
        // in the frame cut after the step.
        let claim = self
            .executor
            .state(self.entity)
            .and_then(|state| self.witness_log.cut_claim(tick.0, state));

        // ── Input: the exact pipeline the local session drives ────────────
        let authored = self.pipeline.human_packet(tick, controls);
        let mut intents = authored.orders.len();
        let mut report = TickReport {
            intents,
            events: Vec::new(),
            delivered: Vec::new(),
        };
        if let Ok(mut authored_orders) = decode_packet(&authored) {
            if let Some(other) = self.executor.state(self.entity).and_then(|own| {
                collision_candidate(
                    self.entity,
                    own,
                    self.executor
                        .entities()
                        .filter(|candidate| **candidate != self.entity)
                        .filter_map(|candidate| {
                            self.executor
                                .state(*candidate)
                                .map(|state| (*candidate, state))
                        }),
                )
            }) {
                authored_orders.push(Order::Collide { other });
                intents = intents.saturating_add(1);
                report.intents = intents;
            }
            // D46 clause (d): deliveries from prior ticks are canonical input
            // and precede this tick's player-authored orders. Record the
            // composed vector, not merely the keyboard half: replay and the
            // witness must execute the exact same order the authority did.
            let composed =
                compose_delivered_first(&mut self.pending_delivered, authored_orders, tick.0);
            report.delivered = composed.delivered;
            for delivered in &report.delivered {
                if let Order::CollisionResolved { from, velocity } = delivered.order {
                    if let Err(error) = sink.append_collision_resolved(
                        tick.0,
                        self.entity,
                        from,
                        velocity,
                        crate::telemetry::SessionScope::Campaign,
                    ) {
                        bevy::log::error!("cannot append Regolith collision result: {error}");
                    }
                }
            }
            let packet = OrderPacket {
                tick: tick.0,
                entity: self.entity.0,
                orders: encode_orders(&composed.orders),
            };
            if let Err(error) =
                sink.append_orders(&packet, crate::telemetry::SessionScope::Campaign)
            {
                bevy::log::error!("cannot append Regolith order packet: {error}");
            }
            self.witness_log
                .log_inputs_with_sources(tick.0, &composed.orders, &composed.sources);
            if let Some(outcome) = self
                .executor
                .step_entity(self.entity, tick, &composed.orders)
            {
                self.witness_log
                    .log_neighbor_frames(tick.0, &outcome.neighbor_frames);
                self.witness_log.log_tick_hash(outcome.state_hash);
                report.events.extend(outcome.events.iter().cloned());
                let deliveries: Vec<_> = outcome
                    .events
                    .iter()
                    .filter_map(|event| self.executor.ruleset().deliver(event))
                    .collect();
                for (recipient, order) in deliveries {
                    self.route_delivered_input(&link, recipient, order);
                }
            }
        }
        let authored = self.witness_log.cut_frame(tick.0);

        // The exterior slot is last in the host's deterministic witness ring;
        // its next peers wrap to slots 0..K. StreamShared matches the headless
        // plugin's reliable shared stream and is not subject to datagram loss.
        let witness_count = self.config.slot.min(MAX_WITNESS_LINKS);
        if let Some(claim) = claim {
            let payload = Bytes::from(orrery_protocol::channels::encode_witness(
                &WitnessMsg::Claim(claim),
            ));
            self.publish_witness_payload(&link, witness_count, payload);
        }
        if let Some(authored) = authored {
            let heads = authored
                .transitions
                .iter()
                .map(|transition| FrameHead {
                    entity: transition.entity,
                    prev_head: transition.prev_head,
                    head: transition.head,
                })
                .collect();
            let payload = Bytes::from(orrery_protocol::channels::encode_witness(
                &WitnessMsg::Frame {
                    frame: authored.frame,
                    heads,
                },
            ));
            self.publish_witness_payload(&link, witness_count, payload);
        }

        // ── Outbound: canonical state to every island-mate ────────────────
        // The external peer occupies slot N of N+1, so slots below it are
        // exactly the other members; the join reply pinned N.
        if tick.0 % SEND_EVERY_TICKS == SEND_EVERY_TICKS - 1 {
            let cell = self.committed_cell();
            self.latest_cell = Some(cell);
            let payload = encode_state_broadcast(&self.executor, self.entity, cell, tick.0 + 1);
            for recipient in 0..self.config.slot {
                let datagram = UplinkDatagram {
                    sequence: self.uplink_sequence,
                    payload: Bytes::clone(&payload),
                };
                self.uplink_sequence = self.uplink_sequence.wrapping_add(1);
                self.uplink_sent += 1;
                let frame = net::Frame {
                    peer: recipient as u32,
                    lane: Lane::Datagram,
                    payload: datagram.encode(),
                };
                if link.try_uplink(frame).is_err() {
                    self.uplink_shed += 1;
                }
            }
        }

        // Once per second, say where we are now (raw cell bits), like the
        // runner's meta report.
        if tick.0 % u64::from(orrery_core::TICK_HZ) == u64::from(orrery_core::TICK_HZ) - 1 {
            if let Some(cell) = self.latest_cell {
                let frame = net::Frame {
                    peer: u32::MAX,
                    lane: Lane::Meta,
                    payload: Bytes::from(cell.to_bits().to_le_bytes().to_vec()),
                };
                if link.try_uplink(frame).is_err() {
                    self.uplink_shed += 1;
                }
            }
        }

        // ── Inbound: replicated state, acks, liveness ─────────────────────
        for frame in link.drain_downlink() {
            match frame.lane {
                Lane::Meta => {
                    if let Some(ack) = UplinkAck::decode(&frame.payload) {
                        // THE uplink measurement: the router's settled
                        // decision, not a transport write.
                        let dropped = ack.outcome == UplinkOutcome::Dropped;
                        self.uplink_acks += 1;
                        self.uplink_dropped += u64::from(dropped);
                        self.campaign.observe_uplink_ack(dropped);
                    }
                    // Anything else on meta is not ours to interpret.
                }
                Lane::Datagram => {
                    let now_ms = self.started_at.elapsed().as_secs_f64() * 1_000.0;
                    // Strip the outer channel tag first (the harness wire is
                    // double-tagged; see `encode_state_broadcast`), then read
                    // the replication envelope from what remains.
                    let inner = orrery_protocol::channels::untag(&frame.payload)
                        .filter(|(channel, _)| {
                            *channel == orrery_protocol::channels::Channel::State
                        })
                        .map(|(_, rest)| rest.to_vec());
                    let Some(inner) = inner else {
                        self.undecodable += 1;
                        continue;
                    };
                    match decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(&inner) {
                        Some((encoded, _cell, entity, at)) => {
                            match <RegolithState as CoreCodec>::decode(&encoded) {
                                Ok(state) => {
                                    if entity != self.entity {
                                        let _ = refresh_replica(
                                            &mut self.replica_freshness,
                                            entity,
                                            tick.0,
                                            at,
                                        );
                                    }
                                    self.executor.insert(entity, state);
                                    // The duel view follows the first remote
                                    // craft that arrives, and stays with it.
                                    if entity != self.entity && self.focus.is_none() {
                                        self.focus = Some(entity);
                                    }
                                }
                                Err(_) => self.undecodable += 1,
                            }
                            let arrival = self.downlink.record(frame.peer, at, now_ms);
                            self.downlink_arrivals += 1;
                            self.campaign
                                .observe_arrival(arrival.missing, arrival.deviation_ms);
                        }
                        None => {
                            // Witness frames share this sub-tagged lane
                            // upstream; neither is replication we can read.
                            // Counted, so a silent empty world cannot hide.
                            self.undecodable += 1;
                        }
                    }
                }
                Lane::StreamShared => {
                    // The peer stack contributes the outer Control tag and
                    // the delivery envelope contributes the inner one, just
                    // as replication is double-tagged on the state lane.
                    let delivered = orrery_protocol::channels::untag(&frame.payload)
                        .filter(|(channel, _)| {
                            *channel == orrery_protocol::channels::Channel::Control
                        })
                        .and_then(|(_, inner)| decode_delivered_input(inner));
                    match delivered {
                        Some(delivered) => match accept_own_delivery(
                            self.entity,
                            delivered,
                            &mut self.pending_delivered,
                        ) {
                            Ok(()) => {}
                            Err(DeliveryRefusal::Foreign) => self.delivered_foreign += 1,
                            Err(DeliveryRefusal::Malformed) => self.undecodable += 1,
                        },
                        None => {
                            // Witness log frames and repairs also ride the
                            // reliable lane. This client authors its own
                            // stream but is not a watcher for other subjects.
                        }
                    }
                }
                Lane::StreamBulk => {
                    // Bulk witness repairs are not consumed by this client.
                }
            }
        }

        expire_stale_replicas(
            &mut self.executor,
            self.entity,
            tick.0,
            &mut self.replica_freshness,
            &mut self.focus,
        );

        if !link.is_connected() || link.host_said_goodbye() {
            self.state = JoinState::Closed {
                host_said_goodbye: link.host_said_goodbye(),
            };
        }

        // ── The accumulator, fed by reality ───────────────────────────────
        self.campaign.observe_tick(intents);
        self.tick = Tick::new(tick.0.saturating_add(1));
        report
    }

    /// The geometric cell of this craft's current position.
    ///
    /// The harness's commitment carries hysteresis via its spatial plugin;
    /// the client commits raw geometry for now, which flaps on boundaries by
    /// exactly the amount hysteresis exists to prevent. Accepted this slice:
    /// interest gating degrades gracefully and #387 owns the fix.
    fn committed_cell(&self) -> CellId {
        let metres = match self.executor.state(self.entity) {
            Some(RegolithState::Craft(craft)) => {
                let (x, y, z) = craft.pos.to_metres();
                DVec3::new(x, y, z)
            }
            _ => DVec3::ZERO,
        };
        cell_id_from_metres(metres, self.cell_edge_m).unwrap_or_else(|_| {
            CellId::from_coords(IVec3::ZERO, orrery_protocol::INTEREST_LEVEL)
                .expect("origin is representable")
        })
    }

    fn publish_witness_payload(
        &mut self,
        link: &CampaignLink,
        witness_count: usize,
        payload: Bytes,
    ) {
        for recipient in 0..witness_count {
            if link
                .try_uplink(net::Frame {
                    peer: recipient as u32,
                    lane: Lane::StreamShared,
                    payload: payload.clone(),
                })
                .is_err()
            {
                self.uplink_shed += 1;
            }
        }
    }

    /// Route a ruleset-produced delivery without ever applying foreign state
    /// locally. The exterior participant owns the last slot, so every lower
    /// participant slot is host-side; other ids name materialized authorities
    /// this campaign host does not yet publish a route for and remain counted.
    fn route_delivered_input(&mut self, link: &CampaignLink, recipient: PersistId, order: Order) {
        if recipient == self.entity {
            self.pending_delivered.push(DeliveredOrder {
                from: self.entity,
                recipient,
                order,
            });
            return;
        }
        let Some(slot) = recipient
            .0
            .checked_sub(1)
            .and_then(|slot| u32::try_from(slot).ok())
            .filter(|slot| (*slot as usize) < self.config.slot)
        else {
            self.delivered_unroutable += 1;
            return;
        };
        let inner = orrery_protocol::channels::encode_delivered_input(
            self.entity,
            recipient,
            &order.to_canonical(),
        );
        let payload = Bytes::from(orrery_protocol::channels::tag(
            orrery_protocol::channels::Channel::Control,
            &inner,
        ));
        if link
            .try_uplink(net::Frame {
                peer: slot,
                lane: Lane::StreamShared,
                payload,
            })
            .is_err()
        {
            self.uplink_shed += 1;
        }
    }

    /// Finish the banking row. Idempotent: the row is written once.
    ///
    /// A session that never reached [`JoinState::Joined`] produces **no row**
    /// — a refused or failed dial measured nothing and banks nothing, and a
    /// zero-minute placeholder would be indistinguishable from evidence
    /// downstream. `pipeline_digest` is coordinator-supplied provenance; a
    /// client-side session cannot know it yet (#387 assembles the report),
    /// so the field records that honestly instead of inventing a digest.
    pub fn finish_record(&mut self) -> Option<SessionRecord> {
        if self.record_written || self.joined_ticks() == 0 {
            return None;
        }
        self.record_written = true;
        let mut record = self.campaign.finish(
            utc_now_iso8601(),
            platform_triple(),
            crate::BUILD_REV.to_owned(),
            "unavailable-client-side".to_owned(),
        );
        record
            .sign(&self.config.transport_secret)
            .expect("SessionRecord signing payload serializes");
        Some(record)
    }

    /// End the session: goodbye marker, grace period, final row.
    pub fn shutdown(&mut self) -> Option<SessionRecord> {
        if let Some(link) = &self.link {
            link.close();
            self.state = JoinState::Closed {
                host_said_goodbye: false,
            };
        }
        self.finish_record()
    }

    /// Diagnostics for the F3 pane's session line.
    #[must_use]
    pub fn summary_line(&self) -> String {
        match &self.state {
            JoinState::Dialing => "campaign: dialing...".to_owned(),
            JoinState::Joined => format!(
                "campaign: joined as slot {} (entity {}), uplink sent {} shed {}, \
                 downlink missing {}",
                self.config.slot,
                self.entity.0,
                self.uplink_sent,
                self.uplink_shed,
                self.downlink.total_missing(),
            ),
            JoinState::Refused(reason) => format!("campaign: REFUSED - {reason}"),
            JoinState::Failed(reason) => format!("campaign: FAILED - {reason}"),
            JoinState::Closed { host_said_goodbye } => {
                format!("campaign: closed (host said goodbye: {host_said_goodbye})")
            }
        }
    }
}

fn expire_stale_replicas(
    executor: &mut Executor<Regolith>,
    own: PersistId,
    tick: u64,
    freshness: &mut BTreeMap<PersistId, ReplicaFreshness>,
    focus: &mut Option<PersistId>,
) {
    let expired: Vec<_> = freshness
        .iter()
        .filter_map(|(entity, replica)| {
            (replica.installed
                && tick.saturating_sub(replica.last_refresh_tick) > REPLICA_TTL_TICKS)
                .then_some((*entity, replica.last_refresh_tick))
        })
        .collect();
    for (entity, last_refresh_tick) in expired {
        freshness
            .get_mut(&entity)
            .expect("expired replica came from the freshness map")
            .installed = false;
        capture_replica_event(
            "expiry",
            entity,
            tick,
            None,
            Some(tick.saturating_sub(last_refresh_tick)),
            None,
        );
        if entity != own {
            executor.take_state(entity);
        }
        if *focus == Some(entity) {
            *focus = None;
        }
    }
}

fn refresh_replica(
    freshness: &mut BTreeMap<PersistId, ReplicaFreshness>,
    entity: PersistId,
    client_tick: u64,
    authoritative_tick: u64,
) -> (&'static str, Option<u64>, Option<u64>) {
    let previous = freshness.insert(
        entity,
        ReplicaFreshness {
            last_refresh_tick: client_tick,
            last_authoritative_tick: authoritative_tick,
            installed: true,
        },
    );
    let event = if previous.is_some_and(|replica| replica.installed) {
        "refresh"
    } else {
        "install"
    };
    let gap_ticks = previous.map(|replica| client_tick.saturating_sub(replica.last_refresh_tick));
    let authoritative_gap_ticks =
        previous.map(|replica| authoritative_tick.saturating_sub(replica.last_authoritative_tick));
    capture_replica_event(
        event,
        entity,
        client_tick,
        Some(authoritative_tick),
        gap_ticks,
        authoritative_gap_ticks,
    );
    (event, gap_ticks, authoritative_gap_ticks)
}

fn capture_replica_event(
    event: &str,
    entity: PersistId,
    client_tick: u64,
    authoritative_tick: Option<u64>,
    gap_ticks: Option<u64>,
    authoritative_gap_ticks: Option<u64>,
) {
    if std::env::var_os("ORRERY_REPLICA_CAPTURE").is_some() {
        eprintln!(
            "replica_capture event={event} entity={} client_tick={client_tick} \
             authoritative_tick={} gap_ticks={} authoritative_gap_ticks={}",
            entity.0,
            authoritative_tick.map_or_else(|| "none".to_owned(), |tick| tick.to_string()),
            gap_ticks.map_or_else(|| "none".to_owned(), |gap| gap.to_string()),
            authoritative_gap_ticks.map_or_else(|| "none".to_owned(), |gap| gap.to_string()),
        );
    }
}

impl Drop for CampaignRuntime {
    fn drop(&mut self) {
        // Best effort: mark the goodbye so a host-side gate sees a clean end
        // even when the window closes mid-run. The 200 ms grace happens
        // inside `close`.
        if matches!(self.state, JoinState::Joined) {
            self.shutdown();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryRefusal {
    Foreign,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposedInputs {
    orders: Vec<Order>,
    sources: Vec<RecordSource>,
    delivered: Vec<DeliveredOrder>,
}

/// D46's delivered-first composition. `pending` is already in reliable-stream
/// arrival order; draining it before appending authored orders makes the law
/// explicit at the authority boundary and leaves no later sort to drift.
fn compose_delivered_first(
    pending: &mut Vec<DeliveredOrder>,
    authored: Vec<Order>,
    tick: u64,
) -> ComposedInputs {
    let delivered = core::mem::take(pending);
    let mut orders = Vec::with_capacity(delivered.len() + authored.len());
    let mut sources = Vec::with_capacity(orders.capacity());
    for input in &delivered {
        orders.push(input.order.clone());
        sources.push(RecordSource::InboundEvent { from: input.from });
    }
    for (seq, order) in authored.into_iter().enumerate() {
        orders.push(order);
        sources.push(RecordSource::OwnPlayer {
            // Five authored inputs are reachable when the four-order pilot row
            // also nominates a collision. Eight leaves each tick a disjoint,
            // power-of-two source-id range.
            input_seq: (tick as u32).wrapping_mul(8).wrapping_add(seq as u32),
        });
    }
    ComposedInputs {
        orders,
        sources,
        delivered,
    }
}

/// Admit a delivered input only at the authority named by its envelope.
fn accept_own_delivery(
    own: PersistId,
    delivered: orrery_protocol::channels::DeliveredInput,
    pending: &mut Vec<DeliveredOrder>,
) -> Result<(), DeliveryRefusal> {
    if delivered.recipient != own {
        return Err(DeliveryRefusal::Foreign);
    }
    let order = Order::decode(&delivered.input).map_err(|_| DeliveryRefusal::Malformed)?;
    pending.push(DeliveredOrder {
        from: delivered.from,
        recipient: delivered.recipient,
        order,
    });
    Ok(())
}

fn encode_state_broadcast(
    executor: &Executor<Regolith>,
    entity: PersistId,
    cell: CellId,
    at: u64,
) -> Bytes {
    let encoded = match executor.state(entity) {
        Some(state) => state.to_canonical(),
        None => Vec::new(),
    };
    // The harness's exact wire bytes. `broadcast_state` builds
    // `encode_replication(...)` — `[State][TAG_REPLICATION][postcard]` — and
    // `send_peer_packets` then tags the whole payload again with its channel
    // (`tag(Channel::State, …)`), so what a bot's receive path unwraps is
    // `[State][State][TAG_REPLICATION][postcard]`: one channel tag stripped
    // by `receive_peer_packets`, the second by `decode_replication`. #386
    // sent the single-tagged form and every one of its broadcasts was
    // counted undecodable by every bot on the island (found by the first
    // real two-process run, #387); the fixture now pins the double tag.
    Bytes::from(orrery_protocol::channels::tag(
        orrery_protocol::channels::Channel::State,
        &orrery_protocol::channels::encode_replication(&(encoded, cell, entity, at)),
    ))
}

/// Decode lowercase/uppercase hex into bytes, with a reason on refusal.
fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("session token hex has an odd number of digits".to_owned());
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&hex[index..index + 2], 16)
                .map_err(|_| "session token is not hex".to_owned())
        })
        .collect()
}

/// Wall clock, RFC3339-ish UTC. Telemetry provenance, not ruleset input.
#[must_use]
pub fn utc_now_iso8601() -> String {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();
    // Days→Y-M-D by civil-from-days (Howard Hinnant's algorithm), so the
    // stamp needs no chrono dependency.
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((if m <= 2 { y + 1 } else { y }), m as u32, d as u32)
}

/// The Rust target triple this client was compiled for, stamped by build.rs.
///
/// The banking row's `platform_triple` is validated against the host report's
/// `identity.target` by `p4-ledger.sh`; both must therefore spell the same
/// triple. The previous `{os}-{arch}` spelling ("linux-x86_64") failed that
/// check on every row it ever produced.
fn platform_triple() -> String {
    env!("ORRERY_PLATFORM_TRIPLE").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Actor, ConfiguredImpairment};
    use orrery_games::regolith::state::LockClass;

    #[test]
    fn authoritative_deliveries_are_composed_before_this_ticks_human_orders() {
        let target = PersistId::new(1);
        let from = PersistId::new(7);
        let mut delivered = vec![DeliveredOrder {
            from,
            recipient: PersistId::new(2),
            order: Order::LockConfirmed {
                target,
                class: LockClass::Ship,
            },
        }];
        let composed = compose_delivered_first(
            &mut delivered,
            vec![Order::Lock { target }, Order::Fire],
            11,
        );
        assert_eq!(
            composed.orders,
            [
                Order::LockConfirmed {
                    target,
                    class: LockClass::Ship,
                },
                Order::Lock { target },
                Order::Fire,
            ],
            "D46 fixes prior-tick deliveries before this tick's authored orders"
        );
        assert!(matches!(
            composed.sources.as_slice(),
            [
                RecordSource::InboundEvent { from: source },
                RecordSource::OwnPlayer { .. },
                RecordSource::OwnPlayer { .. },
            ] if *source == from
        ));
        assert!(delivered.is_empty(), "each delivery is consumed once");
    }

    #[test]
    fn foreign_addressed_delivery_is_refused_before_local_step() {
        let own = PersistId::new(2);
        let target = PersistId::new(1);
        let delivered = orrery_protocol::channels::DeliveredInput {
            from: target,
            recipient: PersistId::new(99),
            input: Order::LockConfirmed {
                target,
                class: LockClass::Ship,
            }
            .to_canonical(),
        };
        let mut pending = Vec::new();
        assert_eq!(
            accept_own_delivery(own, delivered, &mut pending),
            Err(DeliveryRefusal::Foreign)
        );
        assert!(
            pending.is_empty(),
            "a foreign order must never enter this authority's input vector"
        );
    }

    /// Stride is learned from the first delta; afterwards every stride-
    /// sized advance is clean and every double-stride gap is one loss.
    #[test]
    fn gaps_are_counted_against_the_learned_stride() {
        let mut tracker = DownlinkTracker::default();
        let t = |n| f64::from(n) * 1_000.0;
        // First arrival: baseline, no deviation (nothing to deviate from).
        assert_eq!(
            tracker.record(7, 100, t(0)),
            Arrival {
                missing: 0,
                deviation_ms: None
            }
        );
        // Second: sets stride 3, interval established.
        assert_eq!(
            tracker.record(7, 103, t(50)),
            Arrival {
                missing: 0,
                deviation_ms: None
            }
        );
        // Clean strides: nothing missing, steady intervals → zero deviation.
        assert_eq!(tracker.record(7, 106, t(100)).missing, 0);
        assert_eq!(tracker.record(7, 109, t(150)).deviation_ms, Some(0));
        // A skipped broadcast: tick jumps two strides.
        let arrival = tracker.record(7, 115, t(200));
        assert_eq!(arrival.missing, 1, "ticks 112 was lost");
        assert_eq!(arrival.deviation_ms, Some(0), "interval held at 50 ms");
        // A long hole: four strides of jump = three losses.
        assert_eq!(tracker.record(7, 127, t(300)).missing, 3);
        assert_eq!(tracker.total_missing(), 4);
        assert_eq!(tracker.senders(), 1);
    }

    /// Reordered arrivals are timing samples, never losses: an older tick
    /// than the newest seen carries no gap arithmetic either way.
    #[test]
    fn reordered_arrivals_are_not_loss() {
        let mut tracker = DownlinkTracker::default();
        let _ = tracker.record(2, 30, 0.0);
        let _ = tracker.record(2, 33, 50.0);
        let _ = tracker.record(2, 36, 100.0);
        // A delayed packet from two broadcasts ago lands now.
        let arrival = tracker.record(2, 33, 160.0);
        assert_eq!(arrival.missing, 0);
        assert_eq!(tracker.total_missing(), 0, "no phantom loss from reorder");
        // The next fresh tick resumes counting from the newest seen (36):
        // broadcasts land every three ticks, so 42 arriving means 39 was lost.
        assert_eq!(tracker.record(2, 42, 210.0).missing, 1);
    }

    /// Jitter deviations come from consecutive inter-arrival intervals,
    /// pooled per sender independently so one chatty sender cannot invent
    /// another's cadence.
    #[test]
    fn jitter_deviations_follow_interval_changes() {
        let mut tracker = DownlinkTracker::default();
        // Sender A: perfectly steady 50 ms intervals.
        for index in 1..4u64 {
            let _ = tracker.record(0, index * 3, index as f64 * 50.0);
        }
        // Sender B arrives between A's frames with its own cadence.
        let first_b = tracker.record(1, 10, 25.0);
        assert_eq!(first_b.deviation_ms, None, "first arrival has no baseline");
        let second_b = tracker.record(1, 13, 90.0); // interval 65 after 25 → deviation vs baseline
        assert_eq!(
            second_b.deviation_ms, None,
            "second arrival still baselining"
        );
        let third_b = tracker.record(1, 16, 155.0); // interval 65 again
        assert_eq!(third_b.deviation_ms, Some(0));
        let spiky = tracker.record(1, 19, 280.0); // interval 125
        assert_eq!(spiky.deviation_ms, Some(60), "|125 − 65|");
    }

    #[test]
    fn campaign_replica_expires_instead_of_freezing_on_screen() {
        let game = Regolith::honest();
        let own = PersistId::new(9);
        let remote = PersistId::new(3);
        let mut executor = Executor::new(game, UniverseSeed([0x61; 32]));
        executor.insert(own, game.spawn(own, 8));
        executor.insert(remote, game.spawn(remote, 2));
        let mut freshness = BTreeMap::from([(
            remote,
            ReplicaFreshness {
                last_refresh_tick: 10,
                last_authoritative_tick: 9,
                installed: true,
            },
        )]);
        let mut focus = Some(remote);

        expire_stale_replicas(
            &mut executor,
            own,
            10 + REPLICA_TTL_TICKS,
            &mut freshness,
            &mut focus,
        );
        assert!(
            executor.state(remote).is_some(),
            "the legal one-hertz proxy grace remains drawable"
        );

        expire_stale_replicas(
            &mut executor,
            own,
            11 + REPLICA_TTL_TICKS,
            &mut freshness,
            &mut focus,
        );
        assert!(
            executor.state(remote).is_none(),
            "an out-of-interest replica must not freeze at its last position"
        );
        assert_eq!(focus, None, "the duel view must release the stale craft");
        assert!(executor.state(own).is_some(), "own authority never expires");
    }

    #[test]
    fn campaign_slow_but_live_replica_never_expires() {
        let game = Regolith::honest();
        let own = PersistId::new(9);
        let remote = PersistId::new(3);
        let mut executor = Executor::new(game, UniverseSeed([0x62; 32]));
        executor.insert(own, game.spawn(own, 8));
        executor.insert(remote, game.spawn(remote, 2));
        let mut freshness = BTreeMap::new();
        let mut focus = Some(remote);
        let _ = refresh_replica(&mut freshness, remote, 10, 10);

        for tick in [70, 130, 190] {
            expire_stale_replicas(&mut executor, own, tick, &mut freshness, &mut focus);
            assert!(
                executor.state(remote).is_some(),
                "a replica refreshed at the ruleset's one-second allowance stays drawable"
            );
            let _ = refresh_replica(&mut freshness, remote, tick, tick);
        }
    }

    #[test]
    fn replica_measurement_retains_the_gap_across_expiry_and_reinstall() {
        let remote = PersistId::new(3);
        let mut freshness = BTreeMap::new();

        let _ = refresh_replica(&mut freshness, remote, 10, 9);
        freshness.get_mut(&remote).expect("installed").installed = false;
        let measurement = refresh_replica(&mut freshness, remote, 211, 210);

        assert_eq!(
            measurement,
            ("install", Some(201), Some(201)),
            "a re-install reports the whole gap from its pre-expiry refresh"
        );
    }

    /// The config's actor is human by construction; the session id round-trips.
    #[test]
    fn config_names_a_human_session() {
        let config = CampaignConfig {
            host_node_hex: "61a71521afb8e193d0d0fc248f85ed20bc78efa1120c83334579129b4171405b"
                .to_owned(),
            host_direct: None,
            slot: 4,
            session_id: "s-1".to_owned(),
            session_token_hex: None,
            wall_start_utc: "2026-08-24T00:00:00Z".to_owned(),
            configured: ConfiguredImpairment {
                loss_pct: 3.0,
                jitter_p50_ms: 100,
                jitter_p99_ms: 100,
            },
            transport_secret: iroh_base::SecretKey::from_bytes(&[0x49; 32]),
        };
        assert_eq!(config.actor(), Actor::Human);
    }

    /// The client's broadcast must be the exact bytes a bot's receive path
    /// unwraps: outer channel tag (what `receive_peer_packets` strips), then
    /// the replication envelope (what `decode_replication` reads). #386 sent
    /// the single-tagged form and every bot on the island counted every one
    /// of this client's broadcasts undecodable.
    #[test]
    fn state_broadcasts_speak_the_harness_double_tagged_wire() {
        let game = Regolith::honest();
        let mut executor = Executor::new(game, UniverseSeed([9u8; 32]));
        let entity = PersistId::new(5);
        executor.insert(entity, game.spawn(entity, 4));
        let cell = CellId::from_coords(IVec3::ONE, orrery_protocol::INTEREST_LEVEL)
            .expect("representable cell");
        let bytes = encode_state_broadcast(&executor, entity, cell, 42);
        // Step 1: the bot-side channel untag.
        let (channel, inner) =
            orrery_protocol::channels::untag(&bytes).expect("outer channel tag present");
        assert_eq!(channel, orrery_protocol::channels::Channel::State);
        // Step 2: the bot-side replication decode of what remains.
        let (encoded, got_cell, got_entity, at) =
            decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(inner)
                .expect("inner replication envelope decodes exactly as a bot's");
        assert_eq!((got_cell, got_entity, at), (cell, entity, 42));
        assert!(
            <RegolithState as CoreCodec>::decode(&encoded).is_ok(),
            "the canonical state body round-trips"
        );
    }

    /// The banking row's platform must be the Rust *target triple*, because
    /// `p4-ledger.sh` refuses a row whose `platform_triple` differs from the
    /// host report's `identity.target`. The old `{os}-{arch}` spelling
    /// ("linux-x86_64") put the architecture last and the OS first; a triple
    /// leads with the architecture and names its vendor.
    #[test]
    fn platform_triple_is_a_rust_target_triple() {
        let triple = platform_triple();
        let parts: Vec<&str> = triple.split('-').collect();
        assert!(parts.len() >= 3, "not a triple: {triple}");
        assert!(
            ["x86_64", "aarch64", "i686", "arm64ec"].contains(&parts[0]),
            "a triple leads with its architecture: {triple}"
        );
        assert!(
            ["unknown", "pc", "apple"].contains(&parts[1]),
            "a triple's second component is its vendor: {triple}"
        );
    }

    /// A malformed session token is a named launch failure, not a silent
    /// tokenless join.
    #[test]
    fn token_hex_decodes_or_names_its_refusal() {
        assert_eq!(decode_hex("00ff10"), Ok(vec![0x00, 0xff, 0x10]));
        assert!(decode_hex("0f0").is_err(), "odd digits refused");
        assert!(decode_hex("zz").is_err(), "non-hex refused");
    }

    /// UTC stamps render as RFC3339 second precision.
    #[test]
    fn utc_stamps_render() {
        assert_eq!(utc_now_iso8601().len(), 20);
        assert!(utc_now_iso8601().ends_with('Z'));
    }
}
