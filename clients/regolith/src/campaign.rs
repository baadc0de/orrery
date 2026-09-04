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
//! **Downlink loss** — send slots that produced no delivery. Every
//! broadcast lands on the send-slot grid (`SEND_EVERY_TICKS`), so a slot
//! the client never received was dropped by the link, or has not arrived
//! yet (the impaired profile reorders a tenth of all datagrams by 100 ms,
//! two slots), or was never sent (interest gating scopes a peer out; an
//! unchanged state skips a delta). The tracker separates the three: a gap
//! stays open for the reorder window so a late arrival can retract its
//! slot, and a settled gap counts as loss only when its width matches the
//! cadence the sender had been exhibiting — dense replication every slot,
//! or the one-hertz keyframe heartbeat. Gaps matching neither (interest
//! churn, mid-stride silences, outages) are indistinguishable from loss
//! in a tick stream: they are counted in
//! [`DownlinkTracker::unattributed_slots`] and never scored. The exact
//! count they hide is what a sequence number on the replication envelope
//! would make countable; the wire does not carry one today.
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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::math::{DVec3, IVec3};
use bevy::prelude::*;
use bytes::Bytes;

use orrery_core::{CoreCodec, Executor, InputLogProducer};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::order::{Order, Outcome};
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_games::regolith::{
    campaign_spawn_pose, collision_candidate, CAMPAIGN_CELL_EDGE_M, REGOLITH_RULESET,
};
use orrery_games::{Game, Regolith};
use orrery_protocol::channels::{decode_delivered_input, decode_replication};
use orrery_protocol::UniverseSeed;
use orrery_protocol::{
    cell_id_from_metres, CellId, FrameHead, PersistId, RecordSource, Tick, WitnessMsg,
};

use crate::intent::{decode_packet, encode_orders, Controls, IntentPipeline, OrderPacket};
use crate::net::{
    self, CampaignLink, HearsayContacts, HostAddress, JoinRequest, Lane, UplinkAck, UplinkDatagram,
    UplinkOutcome,
};
use crate::session::{Actor, CampaignSession, ConfiguredImpairment, PlayerActivity, SessionRecord};

/// Broadcasts per second the harness runs (`send_hz`), mirrored so the
/// client's state cadence matches what a bot's `broadcast_state` does.
const SEND_HZ: u32 = 20;

/// Ticks between state broadcasts (`TICK_HZ / SEND_HZ`, floored at one).
const SEND_EVERY_TICKS: u64 = (orrery_core::TICK_HZ / SEND_HZ) as u64;

/// State broadcasts between absolute replication keyframes.
///
/// The gateway's sender-clocked path uses a one-hertz absolute heartbeat and
/// patches every intervening canonical state against it. This client has one
/// authored entity, but keeps the same cadence so a human upload obeys the
/// arithmetic the swarm gate measures.
const KEYFRAME_EVERY_SENDS: u64 = SEND_HZ as u64;

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
    /// Exterior sender slot that authored the latest installed state.
    authority_slot: u32,
    installed: bool,
}

/// State claims are authored at the existing headless producer's 2 Hz cadence.
const CLAIM_EVERY_TICKS: u64 = 30;

/// Frames use the headless producer's D16-derived 10-tick cadence.
const WITNESS_FRAME_TICKS: u16 = 10;

/// P4's bounded witness fan-out (the same ring width as the host).
const MAX_WITNESS_LINKS: usize = 7;

/// How long the dial may take before the join attempt is declared failed.
/// The handshake waits out the host's lobby; this adds dial and bind slack
/// around that inner bound. See `CAMPAIGN_LOBBY_HOLD`.
const JOIN_DEADLINE_SECS: u64 = crate::JOIN_DEADLINE.as_secs();

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
    /// This client's display label, when admission granted one in the same
    /// reply as [`Self::slot`].
    ///
    /// `None` is the honest state for an older admission service and for a
    /// join-file launch: neither source asserted an own label. The public
    /// roster must never fill this field by matching a locally held slot.
    pub own_label: Option<String>,
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
    /// How many seats the host's island has, when admission said.
    ///
    /// The spawn pose is a function of `(slot, island_seats)`
    /// (`orrery_games::regolith::campaign_spawn_pose`), so a client that
    /// guesses this number starts somewhere the host did not put it. Before
    /// #573 the client used `slot + 1`, which is right only for the sole
    /// human in the last seat and wrong for the first of several. `None`
    /// keeps that derivation, because a service that does not publish
    /// `humans` is a one-human service and the two agree there.
    pub island_seats: Option<u16>,
    /// Where to fetch this campaign's nickname roster, when one is reachable.
    ///
    /// `None` for a session that never spoke to an admission service — the
    /// join-from-file path — and every ship then stays unlabelled. A label is
    /// only ever a label (#484), so its absence costs nothing but the label.
    pub roster_url: Option<String>,
}

impl CampaignConfig {
    /// How the scope banner names this campaign (#769).
    ///
    /// The campaign id when the roster URL names one, and otherwise the head
    /// of the coordinator-issued session id — a join-from-file launch without
    /// `--roster-campaign` knows a host and a seat but not a campaign name,
    /// and the session it was granted is then the only identity it honestly
    /// has. Never empty: a player in campaign scope must be able to tell the
    /// two states apart positively.
    #[must_use]
    pub fn campaign_label(&self) -> String {
        self.roster_url
            .as_deref()
            .and_then(crate::admission::campaign_id_of_roster_url)
            .unwrap_or_else(|| {
                let head = self.session_id.get(..8).unwrap_or(&self.session_id);
                format!("session {head}")
            })
    }

    /// The island size the spawn pose is computed against.
    ///
    /// The host's number when admission published one, and otherwise the
    /// pre-#573 derivation, which is exactly right for the single-human
    /// campaign that is the only thing a service without `humans` can run.
    #[must_use]
    pub fn island_seats(&self) -> u16 {
        let derived = u16::try_from(self.slot.saturating_add(1)).unwrap_or(u16::MAX);
        self.island_seats
            .map_or(derived, |seats| seats.max(derived))
    }

    /// What the host's `StartV1` manifest must agree with before this client
    /// will play. Every field is spent on the join-tick anchor.
    #[must_use]
    pub fn start_expectation(&self) -> crate::lobby::StartExpectation {
        crate::lobby::StartExpectation {
            slot: self.slot,
            entity: self.slot as u64 + 1,
            node_hex: self.transport_secret.public().to_string(),
            island_seats: self.island_seats(),
        }
    }

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
    /// The host gave this client's seat back while it waited in the lobby, and
    /// said so (#994).
    ///
    /// Distinct from [`Self::Refused`], which is the host declining a join
    /// that never got a seat, and from [`Self::Failed`], which is a
    /// malfunction. This one is neither: the seat was real, the wait was real,
    /// and the seat is now somebody else's — the only thing the player needs
    /// is a new invite.
    Evicted(String),
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
    /// Loss newly attributed by this arrival: the settled gaps it closed
    /// out. A gap opens when an arrival's tick skips send slots and
    /// settles once it is too old for a delayed packet to still fill it,
    /// so a loss is reported an arrival or two after the hole — late,
    /// because a packet that is merely reordered must not be scored.
    pub missing: u64,
    /// Deviation of this interval from the previous one, when there was a
    /// previous interval to deviate from.
    pub deviation_ms: Option<u64>,
}

/// Send slots a delayed datagram may arrive late before waiting for it is
/// pointless.
///
/// The impaired profile's jitter holds a packet for 100 ms
/// (`Impairment::p4_profile` in `gates/p1-swarm`), six ticks — two send
/// slots at the broadcast cadence — so roughly a tenth of all datagrams
/// arrive two slots behind their place on the grid. A gap stays open this
/// long past its end; a tick that lands inside it retracts one missing.
/// Whatever is still missing when the window closes was dropped or never
/// sent, not delayed.
///
/// This is also the settlement horizon, and it is public because a harness
/// cannot place an honest cut through the accounting without it. A gap only
/// settles on a *later* arrival this far past its end
/// ([`DownlinkTracker::record`]), so gaps opened by a stream's final arrivals
/// stay open forever and are never scored. A fixture reconciling
/// [`CampaignRuntime::downlink_accounting`] against a sender ledger must
/// therefore stop the sender at least `REORDER_WINDOW_SLOTS + 1` delivered
/// broadcasts after its last skipped one; a cut taken sooner reads the
/// tracker's deliberate settlement lag as a lost frame.
pub const REORDER_WINDOW_SLOTS: u64 = 2;

/// How many recent advancing gaps (in send slots) define a sender's
/// exhibited cadence. Dense replication advances one slot per datagram;
/// an idle craft advances twenty (the one-hertz keyframe heartbeat). The
/// mode of the last few widths is which of those the sender has been
/// doing, and it is the only thing that lets a gap be read as loss
/// rather than as a change in the sender's cadence.
const CADENCE_HISTORY: usize = 8;

/// Open gaps held per sender. At the broadcast cadence a gap closes two
/// slots after it opens, so more than a couple can only pile up if a
/// future impairment profile reorders far wider than
/// [`REORDER_WINDOW_SLOTS`] assumes; the oldest is then force-settled
/// under the usual rule rather than growing the queue without bound.
const MAX_OPEN_GAPS: usize = 4;

/// Per-sender accounting for replication send slots and arrival intervals.
///
/// Every broadcast lands on the send-slot grid (`SEND_EVERY_TICKS`), so the
/// ticks a sender's datagrams carry measure its cadence in slots. A slot
/// the client never received was dropped by the link, or has not arrived
/// yet (reordering), or was never sent (interest gating scopes the peer
/// out; an unchanged state skips a delta). The tracker separates the three:
/// a gap stays open for [`REORDER_WINDOW_SLOTS`] so a late arrival can
/// retract its slot, and a settled gap counts as loss only when its width
/// matches the cadence the sender had been exhibiting. Gaps matching
/// neither cadence — interest churn, mid-stride silences, outages — are
/// indistinguishable from loss in a tick stream, so they are counted in
/// [`DownlinkTracker::unattributed_slots`] and never scored.
#[derive(Debug, Default)]
pub struct DownlinkTracker {
    senders: BTreeMap<u32, SenderTrack>,
    total_missing: u64,
    unattributed_slots: u64,
    unattributed_gaps: u64,
}

#[derive(Debug, Default)]
struct SenderTrack {
    /// The newest tick seen from this sender: its replication frontier.
    last_tick: Option<u64>,
    last_interval_ms: Option<f64>,
    last_arrival_ms: Option<f64>,
    /// Gaps waiting out the reorder window before they settle.
    open_gaps: Vec<OpenGap>,
    /// Widths (in send slots) of the most recent advancing gaps.
    cadence: VecDeque<u64>,
}

/// A stretch of send slots between two arrivals, not yet settled.
#[derive(Debug)]
struct OpenGap {
    start_tick: u64,
    end_tick: u64,
    /// Stale ticks already credited to this gap, so a retransmitted
    /// keyframe retracts its slot once, however often it repeats. The
    /// slots it spans minus the one that closed it, less this set, is
    /// what settlement still owes.
    filled: BTreeSet<u64>,
}

/// The sender's exhibited cadence: the mode of its recent gap widths,
/// ties resolved to the narrower width. An empty history reads as dense
/// replication, the default for a craft in scope.
fn cadence_mode(history: &VecDeque<u64>) -> u64 {
    let mut best_width = 1u64;
    let mut best_count = 0u64;
    for &width in history {
        let count = history.iter().filter(|&&w| w == width).count() as u64;
        if count > best_count || (count == best_count && width < best_width) {
            best_width = width;
            best_count = count;
        }
    }
    best_width
}

/// What one settled gap of `slots` send slots means, given the cadence the
/// sender had been exhibiting and the fills it already absorbed.
///
/// A gap whose width matches the exhibited cadence reads as missing
/// broadcasts: dense replication (`cadence` 1) sends every slot, so each
/// unfilled slot is a drop; the keyframe heartbeat (`cadence` 20) sends
/// once every twenty slots, so only a multiple beyond the first is a
/// drop. A gap matching neither — interest churn, a mid-stride silence,
/// an outage — is a change in the sender's cadence, which a tick stream
/// cannot tell from loss: `None`, and the caller counts the slots as
/// unattributable rather than scoring them.
fn attribute_gap(slots: u64, fills: u64, cadence: u64) -> Option<u64> {
    let window = 2u64.max(cadence / 2);
    if slots.abs_diff(cadence) > window {
        return None;
    }
    let due = (slots as f64 / cadence as f64).round() as u64;
    Some(due.saturating_sub(1 + fills))
}

impl DownlinkTracker {
    fn last_tick(&self, sender: u32) -> Option<u64> {
        self.senders.get(&sender).and_then(|track| track.last_tick)
    }

    /// Account for one replication packet from `sender` carrying tick `at`,
    /// arriving `now_ms` milliseconds after session start.
    ///
    /// Reordered arrivals — an older tick than the newest seen — are
    /// deliveries, never losses: one landing strictly inside an open gap
    /// retracts one missing from it (the packet was delayed by the link,
    /// not dropped by it), and a repeat of an already-credited tick
    /// retracts nothing.
    pub fn record(&mut self, sender: u32, at: u64, now_ms: f64) -> Arrival {
        let track = self.senders.entry(sender).or_default();
        let mut missing = 0u64;
        let mut unattributed_slots = 0u64;
        let mut unattributed_gaps = 0u64;
        match track.last_tick {
            None => {
                track.last_tick = Some(at);
            }
            Some(last) if at <= last => {
                // Late or duplicate: a delivery, never a loss.
                for gap in &mut track.open_gaps {
                    if gap.start_tick < at && at < gap.end_tick && gap.filled.insert(at) {
                        break;
                    }
                }
            }
            Some(last) => {
                // Settle every gap old enough that a delayed packet can no
                // longer fill it: whatever is still missing after the fills
                // is loss, but only if the gap's width matches the cadence
                // the sender has been exhibiting.
                let cadence = cadence_mode(&track.cadence);
                track.open_gaps.retain(|gap| {
                    if at < gap.end_tick + REORDER_WINDOW_SLOTS * SEND_EVERY_TICKS {
                        return true;
                    }
                    let slots = (gap.end_tick - gap.start_tick) / SEND_EVERY_TICKS;
                    match attribute_gap(slots, gap.filled.len() as u64, cadence) {
                        Some(owed) => missing += owed,
                        None => {
                            unattributed_gaps += 1;
                            unattributed_slots += slots - 1;
                        }
                    }
                    false
                });
                let slots = (at - last) / SEND_EVERY_TICKS;
                if slots > 1 {
                    track.open_gaps.push(OpenGap {
                        start_tick: last,
                        end_tick: at,
                        filled: BTreeSet::new(),
                    });
                    while track.open_gaps.len() > MAX_OPEN_GAPS {
                        let oldest = track.open_gaps.first().expect("len checked above");
                        let slots = (oldest.end_tick - oldest.start_tick) / SEND_EVERY_TICKS;
                        match attribute_gap(slots, oldest.filled.len() as u64, cadence) {
                            Some(owed) => missing += owed,
                            None => {
                                unattributed_gaps += 1;
                                unattributed_slots += slots - 1;
                            }
                        }
                        track.open_gaps.remove(0);
                    }
                }
                track.cadence.push_back(slots);
                while track.cadence.len() > CADENCE_HISTORY {
                    track.cadence.pop_front();
                }
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
        self.unattributed_slots += unattributed_slots;
        self.unattributed_gaps += unattributed_gaps;
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

    /// Send slots whose silence could not be attributed to loss.
    ///
    /// Interest gating, state-unchanged slots and outages all look exactly
    /// like loss in a tick stream; the cadence check keeps them out of the
    /// loss figure at the cost of this honest hole in the measurement. The
    /// exact count they hide is what a sequence number on the replication
    /// envelope would make countable.
    #[must_use]
    pub fn unattributed_slots(&self) -> u64 {
        self.unattributed_slots
    }

    /// Gaps whose silence could not be attributed to loss.
    #[must_use]
    pub fn unattributed_gaps(&self) -> u64 {
        self.unattributed_gaps
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
    /// Entities this tick actually stepped: D8's predicted set, counted.
    ///
    /// One on a tick that stepped this client's craft, zero on a tick that
    /// did not — not joined, no link, or an order packet that failed to
    /// decode and skipped the step. The skin reports this to
    /// `OverlayMetrics::prediction_set_size`, so a stepless tick is visible
    /// in the JSONL rather than inferred from a constant (#1029).
    pub predicted: usize,
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
type PendingJoin = Arc<
    Mutex<
        Option<
            Result<
                (
                    iroh::Endpoint,
                    Arc<CampaignLink>,
                    Option<crate::lobby::AcceptedStart>,
                ),
                String,
            >,
        >,
    >,
>;

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
    /// The host's latest membership, once checked against what this client
    /// anchored. `None` against a host that sends no
    /// manifest, which is every host until #574 lands.
    start: Option<crate::lobby::AcceptedStart>,

    executor: Executor<Regolith>,
    pipeline: IntentPipeline,
    witness_log: InputLogProducer,
    entity: PersistId,
    cell_edge_m: f64,

    tick: Tick,
    /// Number of ticks this process actually drove, independent of join tick.
    ticks_driven: u64,
    started_at: Instant,
    uplink_sequence: u64,
    uplink_sent: u64,
    uplink_shed: u64,
    uplink_acks: u64,
    uplink_dropped: u64,
    /// State datagrams awaiting the host router's settled decision, by
    /// connection-local sequence and addressed swarm slot.
    pending_broadcast_recipients: BTreeMap<u64, usize>,
    /// Swarm slots to which the host router has retained at least one state
    /// broadcast from this client.
    settled_broadcast_recipients: BTreeSet<usize>,
    downlink: DownlinkTracker,
    downlink_arrivals: u64,
    /// Received-traffic decode failures: frames the downlink delivered that
    /// this client could not decode into anything it recognises (#1034).
    ///
    /// Split from the own-packet counter so a number here can only ever mean
    /// "the downlink side failed", and split again in #1039 from the four
    /// delta-application failures, which moved to their own counters
    /// ([`Self::deltas_without_keyframe`] and siblings). Witness records from
    /// the bot cohort ride this same datagram lane (every `Channel::State`
    /// send does, `orrery_net`'s `send_peer_packets`) and land in the
    /// neither-replication arm; like the bot cohort's receiver
    /// (`gates/p1-swarm/src/bot.rs`, the `ReplicaDecodeError::NotReplication`
    /// arm) this client now exempts them by their sub-tag. A healthy island's
    /// steady witness cadence — the dead-flat ~39/s that filled the
    /// 2026-09-04 session's 28 740 — therefore no longer reports here at all.
    downlink_undecodable: u64,
    /// Deltas that arrived for an entity this client holds no keyframe for.
    ///
    /// The client's form of the bot cohort's `deltas_without_any_keyframe`
    /// (`gates/p1-swarm/src/bot.rs`). A delta applies only to the keyframe it
    /// was patched against, so with no retained keyframe there is nothing to
    /// apply it to and the frame is dropped — usually because the keyframe
    /// carrying its anchor was lost and never repaired. A real diagnostic,
    /// split from [`Self::downlink_undecodable`] in #1039: it is not
    /// unintelligible bytes, and counting it there buried both readings.
    deltas_without_keyframe: u64,
    /// Deltas whose tick and keyframe age do not anchor them to the keyframe
    /// this client retains.
    ///
    /// [`delta_is_anchored`] refused the frame: the keyframe the delta names
    /// is not the one held — superseded by a newer one, already replaced by
    /// an older one, or an age that underflows its tick. One coarse counter
    /// where the bot cohort keeps three (`deltas_missing_newer_keyframe`,
    /// `deltas_with_superseded_keyframe`, `deltas_with_invalid_reference`):
    /// this client retains one keyframe per entity and checks once. Split
    /// from [`Self::downlink_undecodable`] in #1039.
    deltas_unanchored: u64,
    /// Deltas whose skip/write patch did not apply to the retained keyframe.
    ///
    /// The bot cohort's `BadPatch`. The bytes parsed as a delta but describe
    /// a program the retained keyframe cannot execute — malformation at the
    /// source, or corruption the envelope check still accepted. Split from
    /// [`Self::downlink_undecodable`] in #1039.
    delta_patch_failures: u64,
    /// Deltas whose patch applied but whose resulting body did not decode as
    /// a [`RegolithState`].
    ///
    /// The bot cohort's `bad_body`: bytes the receiver's own codec refuses.
    /// The patch applied, so sender and receiver agreed on the keyframe; the
    /// produced body is still not a state this client accepts, which is a
    /// codec-contract break rather than loss. Split from
    /// [`Self::downlink_undecodable`] in #1039.
    delta_bodies_undecodable: u64,
    /// This client's own authored order packet failing to decode (#1034).
    ///
    /// Formerly conflated with [`Self::downlink_undecodable`], which made a
    /// 28 740-count session uninterpretable: the two are different defects
    /// with different fixes. The own packet failing to decode skips the
    /// tick's step — a literal one-tick freeze of this craft — so this
    /// counter is the one a turning-freeze report is checked against.
    own_orders_undecodable: u64,
    delivered_unroutable: u64,
    delivered_foreign: u64,
    pending_delivered: Vec<DeliveredOrder>,
    pending_routing: Vec<(PersistId, Order)>,
    replica_freshness: BTreeMap<PersistId, ReplicaFreshness>,
    /// Last absolute sample for each remote entity, used to reconstruct its
    /// keyframe-referenced deltas.
    replica_keyframes: BTreeMap<PersistId, ReplicationKeyframe>,
    focus: Option<PersistId>,
    latest_cell: Option<CellId>,
    /// This client's most recently emitted absolute sample. Every outbound
    /// delta patches these canonical bytes, never the preceding delta.
    broadcast_keyframe: Option<ReplicationKeyframe>,
    /// Number of authored state-send opportunities so keyframes use the same
    /// per-entity stagger as the swarm gate.
    broadcast_send_index: u64,
    /// Client-only received hearsay. This stays outside the executor and
    /// reaches only the crate-private rendering view.
    hearsay: crate::hearsay::HearsayState,

    campaign: CampaignSession,
    record_written: bool,
    /// Where the finished row is appended the instant it is minted.
    ///
    /// The row used to be minted here and written by a Bevy system that read
    /// it back. Any teardown that did not run that system — a panic unwinding
    /// through `Drop`, a window closed before `AppExit` was observed — signed
    /// a row and dropped it on the floor (#947). Durability now belongs to
    /// the same call that mints it, so no later step can lose it.
    record_path: Option<PathBuf>,
    record_disposition: RecordDisposition,
}

/// How far this session's banking row got, for the exit writer's diagnostics.
///
/// The exit writer used to treat "no row" as one silent case. It is two, and
/// they mean opposite things: a session that measured nothing legitimately
/// has no row, while a row that was minted and never reached disk is the
/// exact loss #947 was opened for. Only naming them apart makes the second
/// one visible in a log a volunteer can send back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordDisposition {
    /// No row has been minted yet; the session may still be measuring.
    Unfinished,
    /// The session reached no joined tick, so there is nothing to bank and
    /// deliberately no row (`finish_record`'s zero-tick refusal).
    NothingMeasured,
    /// A row was minted, signed and appended to the record file.
    Persisted,
    /// A row was minted, signed and appended — and is below the measurement
    /// floor, so it is a record of a failure to seat rather than of play
    /// (#1053). Named apart from [`Self::Persisted`] because the two used to
    /// be indistinguishable: a session dropped a second after `StartV1`
    /// produced a valid, signed, uploadable row and said nothing about it.
    PersistedBelowFloor,
    /// A row was minted and signed but never reached durable storage.
    Lost,
}

/// Canonical bytes and metadata carried by an absolute replication keyframe.
#[derive(Clone)]
struct ReplicationKeyframe {
    canonical: Vec<u8>,
    cell: CellId,
    at: u64,
}

/// One inbound Meta-lane payload, classified by the client's grammar.
///
/// Extracted from `advance` so the classification is reachable by test: the
/// branch itself needs a live `CampaignLink` and a dial thread, and an
/// untested branch here fails silently -- hearsay simply never arrives, which
/// looks exactly like a host that sent none.
#[derive(Debug)]
enum MetaFrame {
    /// The router's settled uplink decision.
    Ack(UplinkAck),
    /// A host hearsay fold (#608).
    Hearsay(HearsayContacts),
    /// Host-authored live membership, using the existing StartV1 shape.
    Membership(crate::lobby::StartManifest),
    /// Some other member of the Meta grammar, not ours to interpret.
    Ignored,
}

/// Reads one Meta payload. Ack first: the two tags are disjoint, and this
/// order is what keeps a fold from being consumed as a malformed ack.
fn classify_meta(payload: &[u8]) -> MetaFrame {
    if let Some(ack) = UplinkAck::decode(payload) {
        MetaFrame::Ack(ack)
    } else if let Some(contacts) = HearsayContacts::decode(payload) {
        MetaFrame::Hearsay(contacts)
    } else if let Ok(manifest) = crate::lobby::StartManifest::decode(payload) {
        MetaFrame::Membership(manifest)
    } else {
        MetaFrame::Ignored
    }
}

/// Whether a delta references the keyframe this client actually holds.
///
/// A delta is a patch against one specific keyframe. Applying it to a
/// *different* one produces bytes that decode successfully and are silently
/// wrong -- a corrupt replica with no error anywhere, which is the worst
/// failure shape available here. The age field is the sender's claim about
/// which keyframe it patched; this is where that claim is checked against
/// what we hold.
///
/// Extracted from `advance` so the rule is reachable by test: the branch
/// itself needs a live `CampaignLink` and a dial thread, and severing it left
/// the whole suite green.
fn delta_is_anchored(keyframe_at: u64, delta_tick: u64, keyframe_age: u16) -> bool {
    delta_tick.checked_sub(u64::from(keyframe_age)) == Some(keyframe_at)
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
        // signed join snapshot must use the same campaign pose as the host's
        // headless peers; the compact scenario ring is a different world and
        // leaves every host-owned target kilometres outside weapon reach.
        let (pos, yaw_urad) = campaign_spawn_pose(config.slot, usize::from(config.island_seats()));
        executor.insert(
            entity,
            RegolithState::Craft(Craft::spawned(
                Archetype::for_slot(config.slot as u64),
                pos,
                yaw_urad,
            )),
        );
        let pipeline = IntentPipeline::new(seed, entity, config.slot as u64, Vec::new());
        let witness_log = InputLogProducer::new(
            config.transport_secret.clone(),
            entity,
            REGOLITH_RULESET,
            0,
            CLAIM_EVERY_TICKS,
            WITNESS_FRAME_TICKS,
        );
        let anchor_state = executor.state(entity).expect("spawn inserted").clone();
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
            // #583: the seat admission reserved, sent so the host can check it
            // against its reservation journal rather than trusting the client.
            let slot = config.slot;
            let expectation = config.start_expectation();
            let transport_secret = config.transport_secret.clone();
            let client_rev = crate::BUILD_REV.to_owned();
            let session_id = config.session_id.clone();
            let anchor_secret = transport_secret.clone();
            let anchor_state = anchor_state.clone();
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
                            slot: Some(slot),
                        };
                        let deadline = std::time::Duration::from_secs(JOIN_DEADLINE_SECS);
                        let (link, start) = tokio::time::timeout(
                            deadline,
                            net::remote_join(
                                &endpoint,
                                addr,
                                &request,
                                &expectation,
                                Some(move |tick| {
                                    let mut log = InputLogProducer::new(
                                        anchor_secret,
                                        entity,
                                        REGOLITH_RULESET,
                                        tick,
                                        CLAIM_EVERY_TICKS,
                                        WITNESS_FRAME_TICKS,
                                    );
                                    let claim = log.anchor(tick, &anchor_state);
                                    net::AnchorFrame {
                                        claim_json: serde_json::to_vec(&claim)
                                            .expect("StateClaim serializes"),
                                        state: anchor_state.to_canonical(),
                                    }
                                }),
                            ),
                        )
                        .await
                        .map_err(|_| format!("the join did not complete within {deadline:?}"))??;
                        Ok((endpoint, Arc::new(link), start))
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
            cell_edge_m: CAMPAIGN_CELL_EDGE_M,
            tick: Tick::new(0),
            ticks_driven: 0,
            started_at: Instant::now(),
            uplink_sequence: 0,
            uplink_sent: 0,
            uplink_shed: 0,
            uplink_acks: 0,
            uplink_dropped: 0,
            pending_broadcast_recipients: BTreeMap::new(),
            settled_broadcast_recipients: BTreeSet::new(),
            downlink: DownlinkTracker::default(),
            downlink_arrivals: 0,
            downlink_undecodable: 0,
            deltas_without_keyframe: 0,
            deltas_unanchored: 0,
            delta_patch_failures: 0,
            delta_bodies_undecodable: 0,
            own_orders_undecodable: 0,
            delivered_unroutable: 0,
            delivered_foreign: 0,
            pending_delivered: Vec::new(),
            pending_routing: Vec::new(),
            replica_freshness: BTreeMap::new(),
            replica_keyframes: BTreeMap::new(),
            focus: None,
            latest_cell: None,
            broadcast_keyframe: None,
            broadcast_send_index: 0,
            hearsay: crate::hearsay::HearsayState::default(),
            record_written: false,
            record_path: None,
            record_disposition: RecordDisposition::Unfinished,
            config,
            executor,
            pipeline,
            witness_log,
            entity,
            campaign,
            pending_join,
            start: None,
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
            Some(Ok((endpoint, link, start))) => {
                if let Some(net) = self.net.as_mut() {
                    net._endpoint = Some(endpoint);
                }
                if let Some(start) = &start {
                    bevy::log::info!(
                        "campaign: adopted StartV1 attempt {} - {} seats, {} active, witnesses {:?}",
                        start.attempt_id,
                        start.island_seats,
                        start.active_slots.len(),
                        start.witness_recipients
                    );
                }
                let join_tick = start.as_ref().map_or(0, |start| start.tick);
                self.tick = Tick::new(join_tick);
                self.witness_log = InputLogProducer::new(
                    self.config.transport_secret.clone(),
                    self.entity,
                    REGOLITH_RULESET,
                    join_tick,
                    CLAIM_EVERY_TICKS,
                    WITNESS_FRAME_TICKS,
                );
                let state = self
                    .executor
                    .state(self.entity)
                    .expect("joining entity remains installed")
                    .clone();
                // Reproduce the exact signed anchor sent after StartV1. The
                // network thread could not hand its mutable producer across;
                // Ed25519 signing is deterministic, so this producer begins
                // from the same claim hash and continues that witnessed chain.
                let _ = self.witness_log.anchor(join_tick, &state);
                self.start = start;
                self.link = Some(link);
                self.state = JoinState::Joined;
            }
            Some(Err(reason)) => {
                // A host refusal is a named outcome, not a malfunction; the
                // operator sees why. Anything else failed outright.
                if let Some(why) = reason.strip_prefix(net::LOBBY_EVICTION_PREFIX) {
                    // Not a malfunction and not a refusal: this client was
                    // queued, the host lost it, and the seat went back to
                    // admission (#994). Saying that is the whole point — the
                    // volunteer who waited three minutes for
                    // `handshake closed mid-length` could not act on it.
                    bevy::log::warn!("campaign: the host gave this seat back: {why}");
                    self.state = JoinState::Evicted(why.to_owned());
                } else if reason.contains("refused the join") {
                    let why = reason
                        .strip_prefix("the host refused the join: ")
                        .unwrap_or(&reason)
                        .to_owned();
                    bevy::log::warn!("campaign: the host refused the join: {why}");
                    self.state = JoinState::Refused(why);
                } else {
                    // Until now this reason existed only inside the F3 pane, so
                    // a dial that failed in the field left nothing in the log
                    // and nothing in the telemetry — the operator saw a client
                    // that silently stayed offline. Say it once, out loud.
                    bevy::log::error!("campaign: the dial failed: {reason}");
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
        self.ticks_driven
    }

    /// Launch material, including the configured profile shown *beside* the
    /// measurement, never instead of it.
    #[must_use]
    pub fn config(&self) -> &CampaignConfig {
        &self.config
    }

    /// The host's most recently accepted membership snapshot.
    #[must_use]
    pub fn accepted_start(&self) -> Option<&crate::lobby::AcceptedStart> {
        self.start.as_ref()
    }

    /// Who canonical state is broadcast to.
    ///
    /// The manifest's active set minus this client when there is one, because
    /// a later human is not a lower-numbered slot and `0..slot` would silently
    /// stop replicating to it. Without a manifest this is the pre-#574
    /// derivation: the sole exterior occupies the last seat, so every slot
    /// below it is exactly the rest of the island.
    #[must_use]
    fn broadcast_recipients(&self) -> Vec<usize> {
        broadcast_recipients(self.start.as_ref(), self.config.slot)
    }

    /// Whether replication with one installed remote is proven in both
    /// directions for the release preflight.
    ///
    /// The installed replica proves that peer's state reached this client.
    /// The settled recipient proves the host's impaired router retained this
    /// client's state addressed to the authenticated slot that authored the
    /// replica. Waiting for both keeps a successful headless probe alive until
    /// its half of the mutual exchange is beyond the async client writer.
    #[must_use]
    pub fn replication_is_mutual_with(&self, entity: PersistId) -> bool {
        replication_is_mutual(
            &self.replica_freshness,
            &self.settled_broadcast_recipients,
            entity,
        )
    }

    /// Who witness claims and frames are addressed to.
    ///
    /// The host chooses the ring (D28 reserves witness-set choice to the
    /// coordinator), so a manifest's `witness_recipients` is adopted whole
    /// rather than intersected with anything derived here. Without a manifest
    /// this is the harness's deterministic ring, bounded at
    /// [`MAX_WITNESS_LINKS`] exactly as before.
    #[must_use]
    fn witness_recipients(&self) -> Vec<usize> {
        match &self.start {
            Some(start) => start.witness_recipients.clone(),
            None => (0..self.config.slot.min(MAX_WITNESS_LINKS)).collect(),
        }
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

    /// The interest cell edge this session commits its cell against, metres.
    ///
    /// Exposed so the skin's AOI fade (#533) measures against the same number
    /// [`Self::committed_cell`] divides by, rather than a second copy.
    #[must_use]
    pub const fn cell_edge_m(&self) -> f64 {
        self.cell_edge_m
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

    /// `(downlink arrivals, send slots those arrivals found missing)`.
    ///
    /// The missing count is settled loss only — gaps that outlived the
    /// reorder window and matched the sender's cadence — so an arrival
    /// from moments ago may still be waiting out its window.
    ///
    /// Consequently `arrivals + missing` equals what the sender produced
    /// only at a cut taken [`REORDER_WINDOW_SLOTS`] `+ 1` delivered
    /// broadcasts past the sender's last missing one. Taken sooner it is
    /// short by the trailing gaps still inside their window, which is this
    /// accounting working as designed rather than loss.
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

    /// Received-traffic decode failures: downlink frames this client could
    /// not decode into anything it recognises.
    ///
    /// Split from the own-packet failure in #1034, and from the four
    /// delta-application failures in #1039; see the field note for what a
    /// steady rate here can and can no longer include.
    #[must_use]
    pub fn downlink_undecodable(&self) -> u64 {
        self.downlink_undecodable
    }

    /// Deltas that arrived for an entity this client holds no keyframe for;
    /// the split #1039 made so real diagnostics stay visible.
    #[must_use]
    pub fn deltas_without_keyframe(&self) -> u64 {
        self.deltas_without_keyframe
    }

    /// Deltas whose tick and keyframe age do not anchor them to the retained
    /// keyframe; the split #1039 made so real diagnostics stay visible.
    #[must_use]
    pub fn deltas_unanchored(&self) -> u64 {
        self.deltas_unanchored
    }

    /// Deltas whose skip/write patch did not apply to the retained keyframe;
    /// the split #1039 made so real diagnostics stay visible.
    #[must_use]
    pub fn delta_patch_failures(&self) -> u64 {
        self.delta_patch_failures
    }

    /// Deltas whose patch applied but whose body did not decode as a
    /// [`RegolithState`]; the split #1039 made so real diagnostics stay
    /// visible.
    #[must_use]
    pub fn delta_bodies_undecodable(&self) -> u64 {
        self.delta_bodies_undecodable
    }

    /// This client's own order packet failing to decode.
    ///
    /// Zero is the healthy reading. Any non-zero count is ticks this client
    /// skipped its own step on.
    #[must_use]
    pub fn own_orders_undecodable(&self) -> u64 {
        self.own_orders_undecodable
    }

    /// Deliveries this ruleset produced for an entity this client cannot
    /// route to. Incremented on a real failure path and, until #947, read by
    /// nothing at all — so a session that lost deliveries looked identical to
    /// one that did not.
    #[must_use]
    pub fn delivered_unroutable(&self) -> u64 {
        self.delivered_unroutable
    }

    /// Delivered inputs addressed to some other authority, refused at this
    /// one. Also had no reader before #947.
    #[must_use]
    pub fn delivered_foreign(&self) -> u64 {
        self.delivered_foreign
    }

    /// The most recent cell this craft's position committed to.
    #[must_use]
    pub fn latest_cell(&self) -> Option<CellId> {
        self.latest_cell
    }

    /// The latest client-only hearsay view, for the rendering skin.
    #[must_use]
    #[allow(
        dead_code,
        reason = "A16 piece 4 is the first render consumer of this isolated view"
    )]
    pub(crate) fn hearsay_view(
        &self,
        roster: &crate::roster::ShipRoster,
    ) -> crate::hearsay::HearsayRenderView {
        self.hearsay.render_view(roster, self.tick.0)
    }

    /// How many client ticks ago this entity's replicated state last
    /// refreshed, or [`None`] for an entity this session holds no replica of
    /// (a local craft, or one never installed).
    ///
    /// A replicated body is **frozen** between refreshes: the ingest path
    /// installs decoded state verbatim and nothing advances it by its own
    /// velocity, so its position is exactly as old as this number says. The
    /// skin needs that number for the same reason every hearsay arrow prints
    /// its age: without it the readout states an exact separation to a body
    /// that may not have been there for two seconds (#940).
    ///
    /// Zero means the state refreshed on the current tick. The value is
    /// bounded by [`REPLICA_TTL_TICKS`] while a replica is installed, because
    /// `expire_stale_replicas` removes it past that.
    #[must_use]
    pub(crate) fn replica_age_ticks(&self, entity: PersistId) -> Option<u64> {
        replica_age_ticks(&self.replica_freshness, self.tick.0, entity)
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
            predicted: 0,
            events: Vec::new(),
            delivered: Vec::new(),
        };
        // The offline path asserts on this same condition (`.expect("the
        // local codec produced valid orders")`). Here a failure used to skip
        // telemetry, the witness log, the executor step and routing for the
        // tick with no `else` and no log on either branch — sixty-eight lines
        // quietly not happening. The campaign path is now at least as loud as
        // the offline one, without taking the process down mid-session (#947).
        let decoded = decode_packet(&authored);
        if let Err(error) = &decoded {
            self.count_own_packet_decode_failure(tick, error);
        }
        if let Ok(mut authored_orders) = decoded {
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
                // The predicted set is whatever this tick stepped, and this
                // is the only place a campaign tick steps anything: one
                // entity, this client's own craft. Counted here rather than
                // stated as a constant downstream, so a tick that stepped
                // nothing reads as zero (#1029).
                report.predicted += 1;
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

        // The host names this subject's frozen ring in `StartV1`; without a
        // manifest the exterior slot is last in the harness's deterministic
        // ring and its next peers wrap to slots 0..K. StreamShared matches the
        // headless plugin's reliable shared stream and is not subject to
        // datagram loss.
        let witness_count = self.witness_recipients();
        if let Some(claim) = claim {
            let payload = Bytes::from(orrery_protocol::channels::encode_witness(
                &WitnessMsg::Claim(claim),
            ));
            self.publish_witness_payload(&link, &witness_count, payload);
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
            self.publish_witness_payload(&link, &witness_count, payload);
        }

        // ── Outbound: canonical state to every island-mate ────────────────
        // The manifest's active set minus this client, when the host sent one.
        // Without a manifest the external peer occupies slot N of N+1, so
        // slots below it are exactly the other members; the join reply pins N.
        if tick.0 % SEND_EVERY_TICKS == SEND_EVERY_TICKS - 1 {
            let cell = self.committed_cell();
            self.latest_cell = Some(cell);
            let payload = encode_state_broadcast(
                &self.executor,
                self.entity,
                cell,
                tick.0 + 1,
                &mut self.broadcast_keyframe,
                &mut self.broadcast_send_index,
            );
            if let Some(payload) = payload {
                for recipient in self.broadcast_recipients() {
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
                    match link.try_uplink(frame) {
                        Ok(()) => {
                            self.pending_broadcast_recipients
                                .insert(datagram.sequence, recipient);
                        }
                        Err(_) => self.uplink_shed += 1,
                    }
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
            self.accept_frame(frame, tick);
        }

        expire_stale_replicas(
            &mut self.executor,
            self.entity,
            tick.0,
            &mut self.replica_freshness,
            &mut self.focus,
        );
        self.hearsay.expire(tick.0);
        self.flush_pending_routes(&link);

        if !link.is_connected() || link.host_said_goodbye() {
            self.state = JoinState::Closed {
                host_said_goodbye: link.host_said_goodbye(),
            };
        }

        // ── The accumulator, fed by reality ───────────────────────────────
        // `intents` counts orders, and the pilot pushes Thrust/Lock/Fire on
        // every tick whether or not a key is down — the skin's idle gate only
        // zeroes the acceleration and yaw inside `Thrust`. Feeding that count
        // here made `local_intents != 0` true forever, so idle ticks never
        // accrued, `banked_ticks` always equalled `connected_ticks` and
        // `afk_capped` was unreachable: twenty idle minutes banked twenty
        // minutes and the row claimed `afk_seconds: 0` (#947).
        //
        // The offline path had this right all along (`controls ==
        // Controls::default()`); the mode that banks nothing was the mode
        // that measured correctly. This is the same question, asked the same
        // way, in the mode that banks.
        self.campaign.observe_tick(player_activity(controls));
        self.tick = Tick::new(tick.0.saturating_add(1));
        self.ticks_driven = self.ticks_driven.saturating_add(1);
        report
    }

    /// Accept one frame the downlink delivered, on the lane it arrived on.
    ///
    /// Extracted from `advance`'s inbound loop so the counting rules are
    /// reachable by test without a live `CampaignLink` and a dial thread —
    /// the same seam [`delta_is_anchored`] was cut for. Every failure
    /// counted here is a *received*-traffic failure: the four
    /// delta-application causes feed their dedicated counters (the split
    /// #1039 made, mirroring the bot cohort's four causes), and everything
    /// else — bytes that are neither a keyframe, a delta nor a decodable
    /// witness record — feeds [`Self::downlink_undecodable`]. This client's
    /// own order packet is counted by
    /// [`Self::count_own_packet_decode_failure`] instead (#1034).
    fn accept_frame(&mut self, frame: net::Frame, tick: Tick) {
        match frame.lane {
            Lane::Meta => match classify_meta(&frame.payload) {
                MetaFrame::Ack(ack) => {
                    // THE uplink measurement: the router's settled
                    // decision, not a transport write.
                    let dropped = ack.outcome == UplinkOutcome::Dropped;
                    self.uplink_acks += 1;
                    self.uplink_dropped += u64::from(dropped);
                    self.campaign.observe_uplink_ack(dropped);
                    settle_broadcast_ack(
                        &mut self.pending_broadcast_recipients,
                        &mut self.settled_broadcast_recipients,
                        ack,
                    );
                }
                MetaFrame::Hearsay(contacts) => self.hearsay.accept(contacts, tick.0),
                MetaFrame::Membership(manifest) => {
                    if let Err(reason) = self.adopt_live_membership(&manifest) {
                        bevy::log::error!("campaign: refusing live membership: {reason}");
                        self.state = JoinState::Failed(reason);
                    }
                }
                // Anything else on meta is not ours to interpret.
                MetaFrame::Ignored => {}
            },
            Lane::Datagram => {
                let now_ms = self.started_at.elapsed().as_secs_f64() * 1_000.0;
                // Strip the outer channel tag first (the harness wire is
                // double-tagged; see `encode_state_broadcast`), then read
                // the replication envelope from what remains.
                let inner = orrery_protocol::channels::untag(&frame.payload)
                    .filter(|(channel, _)| *channel == orrery_protocol::channels::Channel::State)
                    .map(|(_, rest)| rest.to_vec());
                let Some(inner) = inner else {
                    self.downlink_undecodable += 1;
                    return;
                };
                match decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(&inner) {
                    Some((encoded, _cell, entity, at)) => {
                        self.replica_keyframes.insert(
                            entity,
                            ReplicationKeyframe {
                                canonical: encoded.clone(),
                                cell: _cell,
                                at,
                            },
                        );
                        match <RegolithState as CoreCodec>::decode(&encoded) {
                            Ok(state) => {
                                if entity != self.entity {
                                    let _ = refresh_replica(
                                        &mut self.replica_freshness,
                                        entity,
                                        frame.peer,
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
                            Err(_) => self.downlink_undecodable += 1,
                        }
                        let arrival = self.downlink.record(frame.peer, at, now_ms);
                        self.downlink_arrivals += 1;
                        self.campaign
                            .observe_arrival(arrival.missing, arrival.deviation_ms);
                    }
                    None => match orrery_protocol::channels::decode_replication_delta(&inner) {
                        Some(delta) => {
                            let Some(keyframe) = self.replica_keyframes.get(&delta.entity) else {
                                self.deltas_without_keyframe += 1;
                                return;
                            };
                            if !delta_is_anchored(keyframe.at, delta.tick, delta.keyframe_age) {
                                self.deltas_unanchored += 1;
                                return;
                            }
                            let Some(encoded) = orrery_protocol::channels::apply_delta_patch(
                                &keyframe.canonical,
                                &delta.patch,
                            ) else {
                                self.delta_patch_failures += 1;
                                return;
                            };
                            let _cell = delta.cell.unwrap_or(keyframe.cell);
                            match <RegolithState as CoreCodec>::decode(&encoded) {
                                Ok(state) => {
                                    if delta.entity != self.entity {
                                        let _ = refresh_replica(
                                            &mut self.replica_freshness,
                                            delta.entity,
                                            frame.peer,
                                            tick.0,
                                            delta.tick,
                                        );
                                    }
                                    self.executor.insert(delta.entity, state);
                                    if delta.entity != self.entity && self.focus.is_none() {
                                        self.focus = Some(delta.entity);
                                    }
                                    let arrival =
                                        self.downlink.record(frame.peer, delta.tick, now_ms);
                                    self.downlink_arrivals += 1;
                                    self.campaign
                                        .observe_arrival(arrival.missing, arrival.deviation_ms);
                                }
                                Err(_) => self.delta_bodies_undecodable += 1,
                            }
                        }
                        None => {
                            // Witness records share this lane and are
                            // sub-tagged as such; recognising one is not a
                            // decode failure. Counting them would make
                            // `downlink_undecodable` — the guard that catches
                            // a client receiving nothing it can read — fire
                            // constantly and stop meaning anything, which is
                            // what saturated it at ~39/s across the whole
                            // 2026-09-04 session (#1039). The same exemption
                            // the bot cohort's receiver makes (bot.rs, the
                            // `ReplicaDecodeError::NotReplication` arm), by
                            // the same sub-tag mechanism. The sub-tag routes,
                            // the type check decides: a witness tag over
                            // bytes no `WitnessMsg` accepts still counts.
                            if orrery_protocol::channels::decode_witness::<WitnessMsg>(&inner)
                                .is_none()
                            {
                                self.downlink_undecodable += 1;
                            }
                        }
                    },
                }
            }
            Lane::StreamShared => {
                // The peer stack contributes the outer Control tag and
                // the delivery envelope contributes the inner one, just
                // as replication is double-tagged on the state lane.
                let delivered = orrery_protocol::channels::untag(&frame.payload)
                    .filter(|(channel, _)| *channel == orrery_protocol::channels::Channel::Control)
                    .and_then(|(_, inner)| decode_delivered_input(inner));
                match delivered {
                    Some(delivered) => match accept_own_delivery(
                        self.entity,
                        delivered,
                        &mut self.pending_delivered,
                    ) {
                        Ok(()) => {}
                        Err(DeliveryRefusal::Foreign) => self.delivered_foreign += 1,
                        Err(DeliveryRefusal::Malformed) => self.downlink_undecodable += 1,
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

    /// The own-packet half of the undecodable split (#1034).
    ///
    /// This client's authored order packet did not decode: no step, no
    /// witness entry and no telemetry for this tick. Formerly this fed the
    /// same counter the received-downlink failures fed, which is what made
    /// the 2026-09-04 session's 28 740 uninterpretable — the own-packet
    /// failure freezes this craft for a tick, the downlink failure ages a
    /// replica, and neither number can audit the other.
    fn count_own_packet_decode_failure(&mut self, tick: Tick, error: &orrery_core::CodecError) {
        self.own_orders_undecodable += 1;
        bevy::log::error!(
            "campaign tick {}: this client's own order packet did not decode ({error}); \
             no step, no witness entry and no telemetry for this tick",
            tick.0
        );
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

    /// Publishes one already-`encode_witness`d record to every witness
    /// recipient, adding the transport frame tag the peer stack would.
    ///
    /// The outer tag is not decoration. A bot publishes witness traffic as
    /// `SendPacket { channel: State, mode: Shared }` carrying
    /// `encode_witness(..)`, whose own bytes already open with the inner
    /// `[TAG_STATE]` (`orrery_witness::plugin::publish_authored`), and
    /// `send_peer_packets` then prepends the *transport's* tag before the
    /// bytes leave. The host's `receive_peer_packets` strips exactly that one
    /// byte and hands the rest to `ingest_peer_traffic`, which calls
    /// `decode_witness` — so what must reach the wire is the double-tagged
    /// `[State][State][TAG_WITNESS][postcard]`, and this client bypasses the
    /// peer stack, so the outer tag is ours to add.
    ///
    /// #964 shipped the single-tagged form: every one of the human seat's
    /// claims and frames arrived one byte short, `decode_witness` returned
    /// `None`, and the witness plugin `continue`d past all of them while the
    /// seat still reported `witness_anchored = true`. That is the identical
    /// mistake #386 made on the replication lane and #387 fixed — see
    /// `encode_state_broadcast`, which pins the same double tag — and
    /// `send_remote_delivery` already had it right on the delivered-input
    /// lane. This is the third and last uplink producer to agree with them.
    fn publish_witness_payload(
        &mut self,
        link: &CampaignLink,
        recipients: &[usize],
        payload: Bytes,
    ) {
        let framed = Bytes::from(orrery_protocol::channels::tag(
            orrery_protocol::channels::Channel::State,
            &payload,
        ));
        for recipient in recipients {
            if link
                .try_uplink(net::Frame {
                    peer: *recipient as u32,
                    lane: Lane::StreamShared,
                    payload: framed.clone(),
                })
                .is_err()
            {
                self.uplink_shed += 1;
            }
        }
    }

    fn adopt_live_membership(
        &mut self,
        manifest: &crate::lobby::StartManifest,
    ) -> Result<(), String> {
        let accepted = crate::lobby::accept_start(manifest, &self.config.start_expectation())
            .map_err(|mismatch| mismatch.player_sentence())?;
        if let Some(current) = &self.start {
            if accepted.attempt_id != current.attempt_id {
                return Err("the host changed attempt generation on a live connection".to_owned());
            }
            if accepted.island_seats != current.island_seats
                || accepted.duration_ticks != current.duration_ticks
            {
                return Err("the host changed the live attempt shape".to_owned());
            }
            if accepted.tick < current.tick {
                return Err("the host sent an older membership snapshot".to_owned());
            }
            let (arrived, departed) =
                membership_delta(&current.active_slots, &accepted.active_slots);
            // Say it out loud, because until #1003 nothing did. `poll_join`
            // logs the StartV1 this client joined on and the live path logged
            // nothing at all, so a session in which a late joiner was — or was
            // not — adopted read identically in the log, and the only way to
            // tell the two apart was to guess. A roster change is rare, so
            // this is one line per change, not per frame.
            if !arrived.is_empty() || !departed.is_empty() {
                bevy::log::info!(
                    "campaign: adopted live membership at host tick {} - {} active, \
                     arrived {arrived:?}, departed {departed:?}",
                    accepted.tick,
                    accepted.active_slots.len()
                );
            }
            for slot in departed {
                let entity = PersistId::new(slot as u64 + 1);
                if entity != self.entity {
                    self.executor.take_state(entity);
                }
                self.replica_freshness.remove(&entity);
                self.replica_keyframes.remove(&entity);
                self.settled_broadcast_recipients.remove(&slot);
                self.pending_broadcast_recipients
                    .retain(|_, recipient| *recipient != slot);
                if self.focus == Some(entity) {
                    self.focus = None;
                }
            }
        }
        self.start = Some(accepted);
        Ok(())
    }

    /// Route a ruleset-produced delivery without ever applying foreign state
    /// locally.
    ///
    /// The replication frame already names the authenticated exterior sender,
    /// so each installed replica carries its route. Keeping that slot in the
    /// freshness row deliberately makes replica presence and routability one
    /// lifetime: expiry invalidates both, while a later install (including an
    /// authority move) replaces both. An entity id is opaque and must never be
    /// decoded as a transport location; one peer may author many entities.
    fn route_delivered_input(&mut self, link: &CampaignLink, recipient: PersistId, order: Order) {
        if recipient == self.entity {
            self.pending_delivered.push(DeliveredOrder {
                from: self.entity,
                recipient,
                order,
            });
            return;
        }
        let Some(slot) = replica_authority_slot(&self.replica_freshness, recipient) else {
            // The simulation runs before this tick's inbound drain. Hold the
            // delivery only until that drain completes so a replica arriving
            // on this tick can supply its route. `flush_pending_routes` runs
            // after expiry, hence this never revives or outlives a replica.
            self.pending_routing.push((recipient, order));
            return;
        };
        self.send_remote_delivery(link, slot, recipient, order);
    }

    fn flush_pending_routes(&mut self, link: &CampaignLink) {
        for (recipient, order) in std::mem::take(&mut self.pending_routing) {
            if let Some(slot) = replica_authority_slot(&self.replica_freshness, recipient) {
                self.send_remote_delivery(link, slot, recipient, order);
            } else {
                self.delivered_unroutable += 1;
            }
        }
    }

    fn send_remote_delivery(
        &mut self,
        link: &CampaignLink,
        slot: u32,
        recipient: PersistId,
        order: Order,
    ) {
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
        if self.record_written {
            return None;
        }
        if self.joined_ticks() == 0 {
            self.record_disposition = RecordDisposition::NothingMeasured;
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
        // Durable here, not in whatever runs next. `Drop` reaches this
        // function during a panic unwind and during a window close that never
        // produced an `AppExit`; both used to sign a row and discard it,
        // because the only writer was a Bevy system reading the return value
        // (#947). Nothing downstream can now lose what this call measured.
        self.record_disposition = match &self.record_path {
            Some(path) => match append_session_record(path, &record) {
                Ok(()) if !record.is_measurement() => {
                    // Loud, and to the volunteer rather than only to a log
                    // file: this is the case where everything downstream is
                    // valid and nothing downstream is worth anything (#1053).
                    bevy::log::error!(
                        "campaign session {} lasted {:.3} min, below the {} min measurement \
                         floor: recorded to {} as evidence of a failed seating, not as play",
                        record.session_id,
                        record.distinct_play_minutes,
                        crate::session::MIN_MEASURED_MINUTES,
                        path.display()
                    );
                    eprintln!(
                        "regolith: this campaign session lasted {:.3} minutes, which is below \
                         the {} minute measurement floor. The host dropped you almost \
                         immediately, so nothing was measured and nothing will bank. The row \
                         was still written to {} — please send it with your report.",
                        record.distinct_play_minutes,
                        crate::session::MIN_MEASURED_MINUTES,
                        path.display()
                    );
                    RecordDisposition::PersistedBelowFloor
                }
                Ok(()) => {
                    bevy::log::info!(
                        "campaign session {} recorded to {} ({} recorded min)",
                        record.session_id,
                        path.display(),
                        record.banked_minutes
                    );
                    RecordDisposition::Persisted
                }
                Err(error) => {
                    bevy::log::error!(
                        "cannot write campaign record {}: {error}; upload not attempted so local evidence remains authoritative",
                        path.display()
                    );
                    eprintln!(
                        "regolith: your campaign record could not be saved to {}: {error}. \
                         The minutes from this attempt were not recorded.",
                        path.display()
                    );
                    RecordDisposition::Lost
                }
            },
            None => {
                // No path was ever named. A headless fixture is entitled to
                // this; a shipped client is not, and says so.
                bevy::log::warn!(
                    "campaign session {} finished with no record path configured; \
                     {} banked minutes exist only in memory",
                    record.session_id,
                    record.banked_minutes
                );
                RecordDisposition::Lost
            }
        };
        Some(record)
    }

    /// Name the file finished rows are appended to, before any tick is flown.
    ///
    /// Called once at session construction. Without it a finished row has
    /// nowhere durable to go and `finish_record` reports [`RecordDisposition::Lost`].
    pub fn set_record_path(&mut self, path: PathBuf) {
        self.record_path = Some(path);
    }

    /// How far this session's banking row got. See [`RecordDisposition`].
    #[must_use]
    pub fn record_disposition(&self) -> RecordDisposition {
        self.record_disposition
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

    #[cfg(test)]
    pub(crate) fn finished_for_test(config: CampaignConfig, seed: UniverseSeed) -> Self {
        let mut runtime = Self::launch(config, seed);
        runtime.campaign.observe_tick(PlayerActivity::Active);
        runtime.ticks_driven = 1;
        runtime
    }

    /// A session that drove joined ticks and then lost its host, which is what
    /// seat 6 of the 2026-09-02 attempt was for its last seven seconds (#942).
    #[cfg(test)]
    pub(crate) fn close_for_test(&mut self) {
        self.state = JoinState::Closed {
            host_said_goodbye: false,
        };
    }

    /// A session that adopted the host's `StartV1` manifest (#942).
    #[cfg(test)]
    pub(crate) fn adopt_start_for_test(&mut self, start: crate::lobby::AcceptedStart) {
        self.start = Some(start);
    }

    /// A session the host admitted, without a live link behind it. The state
    /// `Drop` keys its teardown on (#947).
    #[cfg(test)]
    pub(crate) fn join_for_test(&mut self) {
        self.state = JoinState::Joined;
    }

    /// Install one replicated peer exactly as the downlink keyframe arm does.
    ///
    /// The two lines that matter are copied from `advance`'s `Lane::Datagram`
    /// keyframe branch and must stay copied: the executor insert, and the
    /// first-write-wins `focus` latch. A test that seeded `focus` by hand
    /// could not tell the difference between "the client draws every peer"
    /// and "the client draws the peer the test happened to point at".
    #[cfg(test)]
    pub(crate) fn install_replica_for_test(&mut self, entity: PersistId, state: RegolithState) {
        self.executor.insert(entity, state);
        if entity != self.entity && self.focus.is_none() {
            self.focus = Some(entity);
        }
    }

    /// A dial the host turned away: no joined tick, nothing measured (#942).
    #[cfg(test)]
    pub(crate) fn refuse_for_test(&mut self, why: &str) {
        self.state = JoinState::Refused(why.to_owned());
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
            JoinState::Evicted(reason) => format!("campaign: SEAT GIVEN BACK - {reason}"),
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

fn replica_authority_slot(
    freshness: &BTreeMap<PersistId, ReplicaFreshness>,
    entity: PersistId,
) -> Option<u32> {
    freshness
        .get(&entity)
        .filter(|replica| replica.installed)
        .map(|replica| replica.authority_slot)
}

fn broadcast_recipients(
    start: Option<&crate::lobby::AcceptedStart>,
    own_slot: usize,
) -> Vec<usize> {
    match start {
        Some(start) => start
            .active_slots
            .iter()
            .copied()
            .filter(|slot| *slot != own_slot)
            .collect(),
        None => (0..own_slot).collect(),
    }
}

/// What one adopted membership changed, as `(arrived, departed)` seat slots.
///
/// Both halves matter to this client for different reasons: a departed seat
/// has replica, route and focus state to retire, and an arrived seat is a
/// craft it must start addressing. Naming them together is also the only
/// observability the live-membership path has — see `adopt_live_membership`.
fn membership_delta(previous: &[usize], adopted: &[usize]) -> (Vec<usize>, Vec<usize>) {
    (
        adopted
            .iter()
            .copied()
            .filter(|slot| !previous.contains(slot))
            .collect(),
        previous
            .iter()
            .copied()
            .filter(|slot| !adopted.contains(slot))
            .collect(),
    )
}

fn settle_broadcast_ack(
    pending: &mut BTreeMap<u64, usize>,
    settled: &mut BTreeSet<usize>,
    ack: UplinkAck,
) {
    let Some(recipient) = pending.remove(&ack.sequence) else {
        return;
    };
    if ack.outcome == UplinkOutcome::Delivered {
        settled.insert(recipient);
    }
}

/// Client ticks since `entity`'s replicated state last refreshed.
///
/// [`None`] for an entity with no *installed* replica: one never received, or
/// one `expire_stale_replicas` has retired. An expired entry deliberately
/// keeps its `last_refresh_tick` so a re-install can report the whole silent
/// interval, so `installed` — not the presence of the entry — is what decides
/// whether there is a live replica to age at all.
fn replica_age_ticks(
    freshness: &BTreeMap<PersistId, ReplicaFreshness>,
    tick: u64,
    entity: PersistId,
) -> Option<u64> {
    let replica = freshness.get(&entity)?;
    replica
        .installed
        .then(|| tick.saturating_sub(replica.last_refresh_tick))
}

fn replication_is_mutual(
    freshness: &BTreeMap<PersistId, ReplicaFreshness>,
    settled: &BTreeSet<usize>,
    entity: PersistId,
) -> bool {
    replica_authority_slot(freshness, entity).is_some_and(|slot| {
        usize::try_from(slot)
            .ok()
            .is_some_and(|slot| settled.contains(&slot))
    })
}

fn refresh_replica(
    freshness: &mut BTreeMap<PersistId, ReplicaFreshness>,
    entity: PersistId,
    authority_slot: u32,
    client_tick: u64,
    authoritative_tick: u64,
) -> (&'static str, Option<u64>, Option<u64>) {
    let previous = freshness.insert(
        entity,
        ReplicaFreshness {
            last_refresh_tick: client_tick,
            last_authoritative_tick: authoritative_tick,
            authority_slot,
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

/// Append one finished row to the campaign record file, durably.
///
/// `sync_all` is the point: a row buffered in the page cache when the process
/// dies is not evidence. The file is append-only across every session this
/// binary plays, so a partial write would corrupt earlier attempts too.
///
/// # Errors
/// The record directory cannot be created, the file cannot be opened or
/// appended to, or the flush to stable storage fails.
pub fn append_session_record(path: &Path, record: &SessionRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut writer = std::io::BufWriter::new(file);
    crate::session::CampaignSession::write_record(&mut writer, record)?;
    use std::io::Write as _;
    writer.flush()?;
    writer.get_ref().sync_all()
}

impl Drop for CampaignRuntime {
    fn drop(&mut self) {
        // Best effort: mark the goodbye so a host-side gate sees a clean end
        // even when the window closes mid-run. The 200 ms grace happens
        // inside `close`.
        if !matches!(self.state, JoinState::Joined) {
            return;
        }
        // `shutdown` mints, signs *and now persists* the row, so the value
        // dropped here is a copy of something already on disk. Before #947
        // this line was the whole loss: a joined session torn down by a panic
        // or by a window close that never reached `AppExit` signed its record
        // and discarded it, silently, on both branches.
        let record = self.shutdown();
        match (record, self.record_disposition) {
            (Some(record), RecordDisposition::PersistedBelowFloor) => bevy::log::error!(
                "campaign session {} was torn down holding {:.3} min, below the measurement \
                 floor: this attempt measured nothing",
                record.session_id,
                record.distinct_play_minutes
            ),
            (Some(record), RecordDisposition::Persisted) => bevy::log::info!(
                "campaign session {} banked {} minutes and was recorded during teardown",
                record.session_id,
                record.banked_minutes
            ),
            (Some(record), _) => bevy::log::error!(
                "campaign session {} was torn down holding {} banked minutes that \
                 could not be written: this session's evidence is lost",
                record.session_id,
                record.banked_minutes
            ),
            (None, _) => bevy::log::info!(
                "campaign session torn down with no row to write ({:?})",
                self.record_disposition
            ),
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

/// Does this tick's control state mean the player is flying, or resting?
///
/// The one question the banking accumulator needs, asked of the player's
/// input rather than of the order vector the codec produced from it. See
/// [`PlayerActivity`] for why the difference decided whether every banked
/// hour in P4 was honest.
fn player_activity(controls: Controls) -> PlayerActivity {
    if controls == Controls::default() {
        PlayerActivity::Idle
    } else {
        PlayerActivity::Active
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
    keyframe: &mut Option<ReplicationKeyframe>,
    send_index: &mut u64,
) -> Option<Bytes> {
    let encoded = match executor.state(entity) {
        Some(state) => state.to_canonical(),
        None => Vec::new(),
    };
    // The harness's exact wire bytes. Its keyframes and deltas both carry the
    // inner State channel tag, then `send_peer_packets` adds the outer tag.
    // What a bot's receive path unwraps is therefore
    // `[State][State][replication envelope]`: one channel tag stripped by
    // `receive_peer_packets`, the second by the replication decoder. #386
    // sent the single-tagged form and every broadcast was counted undecodable
    // by every bot on the island (found by the first real two-process run,
    // #387); the fixture now pins the double tag.
    encode_keyframe_or_delta(encoded, cell, entity, at, keyframe, send_index).map(|payload| {
        Bytes::from(orrery_protocol::channels::tag(
            orrery_protocol::channels::Channel::State,
            &payload,
        ))
    })
}

/// Encode one client state broadcast with the same keyframe-referenced delta
/// arithmetic as the swarm bot.
fn encode_keyframe_or_delta(
    encoded: Vec<u8>,
    cell: CellId,
    entity: PersistId,
    at: u64,
    keyframe: &mut Option<ReplicationKeyframe>,
    send_index: &mut u64,
) -> Option<Vec<u8>> {
    let absolute = (encoded.clone(), cell, entity, at);
    let keyframe_due = keyframe.as_ref().is_none()
        || *send_index % KEYFRAME_EVERY_SENDS == entity.0 % KEYFRAME_EVERY_SENDS;
    *send_index = (*send_index).wrapping_add(1);
    if keyframe_due {
        *keyframe = Some(ReplicationKeyframe {
            canonical: encoded,
            cell,
            at,
        });
        return Some(orrery_protocol::channels::encode_replication_compressed(
            &absolute,
        ));
    }

    let previous = keyframe.as_ref().expect("keyframe checked above");
    let cell_changed = cell != previous.cell;
    if encoded == previous.canonical && !cell_changed {
        return None;
    }
    let delta = orrery_protocol::channels::ReplicationDelta {
        entity,
        tick: at,
        keyframe_age: u16::try_from(at.saturating_sub(previous.at))
            .expect("keyframe age fits between one-hertz broadcasts"),
        cell: cell_changed.then_some(cell),
        patch: orrery_protocol::channels::encode_delta_patch(&previous.canonical, &encoded),
    };
    let wire = orrery_protocol::channels::encode_replication_delta(&absolute, &delta);
    let is_delta = orrery_protocol::channels::untag(&wire).is_some_and(|(_, body)| {
        body.first() == Some(&orrery_protocol::channels::TAG_REPLICATION_DELTA)
    });
    // The codec's smaller-only fallback is an absolute keyframe. Sending it
    // here would bypass the per-entity stagger and recreate a peak burst, so
    // leave this state for the next scheduled keyframe like the swarm gate.
    is_delta.then_some(wire)
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
    /// #947 defect 2: the accumulator was asked the wrong question.
    ///
    /// `observe_tick` was fed `authored.orders.len()`, and
    /// `pilot::honest_orders` pushes `Thrust`, `Lock` and `Fire` every single
    /// tick — the skin's idle gate only zeroes `accel_mmss` and the yaw inside
    /// `Thrust`, it never drops the order. So the count could not be zero,
    /// `idle_ticks` was permanently zero, `banked_ticks` always equalled
    /// `connected_ticks`, and `afk_capped` was unreachable.
    ///
    /// The first assertion drives the *real* authoring path with the controls
    /// a resting player produces, so it pins the cause rather than restating
    /// the fix.
    #[test]
    fn a_resting_player_still_authors_orders_but_is_not_active() {
        use crate::intent::{decode_packet, Controls, IntentPipeline};
        use crate::session::PlayerActivity;
        use orrery_protocol::{PersistId, Tick, UniverseSeed};

        let pipeline = IntentPipeline::new(
            UniverseSeed([7; 32]),
            PersistId::new(1),
            0,
            vec![PersistId::new(2)],
        );
        let resting = pipeline.human_packet(Tick::new(0), Controls::default());
        let orders = decode_packet(&resting).expect("the codec produced valid orders");
        assert!(
            !orders.is_empty(),
            "the pilot authors orders even at rest; this is why an order count \
             could never mean 'idle', and why counting them banked every AFK minute"
        );
        assert_eq!(
            super::player_activity(Controls::default()),
            PlayerActivity::Idle,
            "a player touching nothing is idle, whatever the codec emitted"
        );
        assert_eq!(
            super::player_activity(Controls {
                thrust: true,
                ..Controls::default()
            }),
            PlayerActivity::Active,
        );
    }

    /// #947 defect 2, the consequence: the AFK cap must be reachable.
    ///
    /// Composes the real decision the campaign tick makes with the real
    /// accumulator, over the controls a genuinely idle tester produces for
    /// eleven minutes. Under the order-count signal this session banked all
    /// eleven minutes and reported `afk_seconds: 0`.
    #[test]
    fn eleven_idle_minutes_do_not_all_bank() {
        use crate::intent::Controls;
        use crate::session::{Actor, CampaignSession, ConfiguredImpairment};

        let mut session = CampaignSession::new(
            "018f0f8a-0000-7000-8000-000000000001".to_owned(),
            "2026-09-02T17:24:00Z".to_owned(),
            Actor::Human,
            ConfiguredImpairment {
                loss_pct: 3.0,
                jitter_p50_ms: 100,
                jitter_p99_ms: 100,
            },
        );
        let ticks = 11 * 60 * u64::from(orrery_core::TICK_HZ);
        for _ in 0..ticks {
            session.observe_tick(super::player_activity(Controls::default()));
        }
        let record = session.finish(
            "2026-09-02T17:35:00Z".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
            "rev".to_owned(),
            "pipeline".to_owned(),
        );
        assert!(
            record.afk_capped,
            "eleven idle minutes must exhaust the ten-minute idle allowance"
        );
        assert_eq!(
            record.banked_minutes, 10.0,
            "an idle session banks the allowance and no more"
        );
        assert_eq!(record.afk_seconds, 11 * 60);
        assert!(
            record.distinct_play_minutes > record.banked_minutes,
            "connected time exceeds banked time once idling is measured at all"
        );
    }

    /// The client's own broadcast goes out compressed, like the harness's.
    ///
    /// #649 taught `decode_replication` both tags and switched the harness
    /// (`gates/p1-swarm/src/bot.rs`), but left this encoder plain -- so every
    /// byte a real player sent stayed uncompressed while the measured bots'
    /// did not, and the 32-peer gate numbers were quietly better than what a
    /// human client puts on the wire.
    ///
    /// This drives `encode_state_broadcast` itself rather than the codec it
    /// calls: the receiver accepts both forms by design, so the only thing
    /// that pins the sender's choice is an assertion on the sender.
    #[test]
    fn the_clients_own_broadcast_goes_out_compressed() {
        use orrery_protocol::channels::{
            untag, Channel, TAG_REPLICATION, TAG_REPLICATION_COMPRESSED,
        };

        let mut executor = Executor::new(Regolith::honest(), UniverseSeed([3; 32]));
        let entity = PersistId::new(2);
        executor.insert(
            entity,
            RegolithState::Craft(Craft::spawned(
                Archetype::Cruiser,
                orrery_core::QPos::default(),
                0,
            )),
        );

        let mut keyframe = None;
        let mut send_index = 0;
        let wire = encode_state_broadcast(
            &executor,
            entity,
            CellId::from_bits(7).expect("a cell"),
            9,
            &mut keyframe,
            &mut send_index,
        )
        .expect("first broadcast is a keyframe");

        let (channel, inner) = untag(&wire).expect("the outer channel tag");
        assert_eq!(channel, Channel::State);
        let (doubled, body) = untag(inner).expect("the doubled channel tag");
        assert_eq!(
            doubled,
            Channel::State,
            "the doubled tag #387 pinned must survive"
        );
        assert_eq!(
            body.first().copied(),
            Some(TAG_REPLICATION_COMPRESSED),
            "canonical craft state compresses, so the broadcast must leave as \
             {TAG_REPLICATION_COMPRESSED:#x} rather than plain {TAG_REPLICATION:#x}"
        );
    }

    /// A delta may only be applied to the keyframe it was patched against.
    ///
    /// Severing this check left every test green, and the consequence is not
    /// a decode error: `apply_delta_patch` against the wrong keyframe returns
    /// bytes that decode into a *plausible, wrong* state. A silently corrupt
    /// replica is worse than a dropped frame, because nothing counts it.
    #[test]
    fn a_delta_may_only_be_applied_to_the_keyframe_it_names() {
        assert!(
            delta_is_anchored(600, 640, 40),
            "tick 640 minus an age of 40 is the keyframe at 600"
        );
        assert!(
            !delta_is_anchored(600, 640, 39),
            "an off-by-one age names a keyframe this client does not hold"
        );
        assert!(
            !delta_is_anchored(600, 640, 41),
            "and so does an age one too large"
        );
        assert!(
            !delta_is_anchored(600, 10, 40),
            "a delta older than its own age underflows rather than anchoring"
        );
    }

    /// Bytes as the host's `exterior::HearsayContacts::encode` lays them out.
    fn hearsay_wire(fold_tick: u64, seat: u8) -> Vec<u8> {
        let mut out = vec![0xa2, 0x01];
        out.extend_from_slice(&fold_tick.to_le_bytes());
        out.push(1);
        out.push(seat);
        out.extend_from_slice(&7u64.to_le_bytes());
        out.extend_from_slice(&300u16.to_le_bytes());
        out
    }

    /// The branch `advance` takes for every inbound Meta frame. Without this,
    /// severing the client's only wire-to-state feed leaves the suite green
    /// and hearsay simply never arrives in a live game.
    #[test]
    fn a_host_fold_on_the_meta_lane_is_classified_as_hearsay() {
        let classified = classify_meta(&hearsay_wire(600, 3));
        let MetaFrame::Hearsay(contacts) = classified else {
            panic!("a host fold must reach the hearsay arm, got {classified:?}");
        };
        assert_eq!(contacts.fold_tick, 600);
        assert_eq!(contacts.contacts.len(), 1);
        assert_eq!(contacts.contacts[0].seat, 3);
    }

    /// Ack precedence, both directions: the two tags are disjoint, so neither
    /// decoder may consume the other's frame.
    #[test]
    fn an_ack_is_still_an_ack_and_never_a_fold() {
        // [tag][outcome][sequence u64 LE] -- outcome precedes the sequence.
        let mut ack = vec![0xa1, 0];
        ack.extend_from_slice(&42u64.to_le_bytes());
        assert!(
            matches!(classify_meta(&ack), MetaFrame::Ack(_)),
            "the ack arm keeps priority over hearsay"
        );
        assert!(
            matches!(classify_meta(&[0xa3, 0x01]), MetaFrame::Ignored),
            "an unknown Meta member is ignored, not guessed at"
        );
    }

    use super::*;
    use crate::session::{Actor, ConfiguredImpairment};
    use orrery_games::regolith::state::LockClass;

    #[test]
    fn three_active_humans_are_pairwise_broadcast_recipients_regardless_of_seat_order() {
        let start = crate::lobby::AcceptedStart {
            attempt_id: "attempt-601".to_owned(),
            island_seats: 8,
            tick: 0,
            active_slots: vec![0, 1, 2, 3, 4, 5, 6, 7],
            witness_recipients: Vec::new(),
            duration_ticks: 216_000,
        };
        let human_slots = [5, 6, 7];

        for subject in human_slots {
            let recipients = broadcast_recipients(Some(&start), subject);
            for peer in human_slots {
                assert_eq!(
                    recipients.contains(&peer),
                    peer != subject,
                    "human slot {subject} must broadcast to every other human, including slot 7"
                );
            }
        }
    }

    #[test]
    fn live_membership_adds_and_removes_outbound_recipients_in_both_directions() {
        let manifest = |tick, human_slots: &[usize], subject| {
            let active_slots = (0..4)
                .chain(human_slots.iter().copied())
                .collect::<Vec<_>>();
            crate::lobby::AcceptedStart {
                attempt_id: "attempt-681".to_owned(),
                island_seats: 8,
                tick,
                active_slots: active_slots.clone(),
                witness_recipients: active_slots
                    .iter()
                    .copied()
                    .filter(|slot| *slot != subject)
                    .take(MAX_WITNESS_LINKS)
                    .collect(),
                duration_ticks: 216_000,
            }
        };

        let before_join = manifest(600, &[4], 4);
        assert!(!broadcast_recipients(Some(&before_join), 4).contains(&5));

        let after_join = manifest(601, &[4, 5], 4);
        assert!(
            broadcast_recipients(Some(&after_join), 4).contains(&5),
            "an already-connected player must start sending to the late joiner"
        );

        let after_departure = manifest(602, &[5], 5);
        assert!(
            !broadcast_recipients(Some(&after_departure), 5).contains(&4),
            "a remaining player must stop sending to the departed slot"
        );
    }

    #[test]
    fn a_membership_delta_names_the_arrival_as_well_as_the_departure() {
        // The arrival half is what #1003 needed and did not have: a session
        // that adopted a late joiner and one that never did produced
        // byte-identical logs, so neither could be told from the other.
        assert_eq!(
            membership_delta(&[0, 1, 2, 5, 6], &[0, 1, 2, 5, 6, 7]),
            (vec![7], vec![]),
            "a late joiner is an arrival and nothing else"
        );
        assert_eq!(
            membership_delta(&[0, 1, 2, 5, 6], &[0, 1, 2, 6]),
            (vec![], vec![5]),
            "a seat that left is a departure and nothing else"
        );
        assert_eq!(
            membership_delta(&[0, 1, 2, 5], &[0, 1, 2, 7]),
            (vec![7], vec![5]),
            "a seat reused inside one attempt is both at once"
        );
        assert_eq!(
            membership_delta(&[0, 1, 2, 5, 6], &[0, 1, 2, 5, 6]),
            (vec![], vec![]),
            "an unchanged roster is silent, so the log carries changes only"
        );
    }

    #[test]
    fn mutual_observation_waits_for_our_broadcast_to_that_same_peer_to_settle() {
        let peer = PersistId::new(7);
        let freshness = BTreeMap::from([(
            peer,
            ReplicaFreshness {
                last_refresh_tick: 10,
                last_authoritative_tick: 9,
                authority_slot: 6,
                installed: true,
            },
        )]);
        let mut pending = BTreeMap::from([(41, 6)]);
        let mut settled = BTreeSet::new();

        assert!(
            !replication_is_mutual(&freshness, &settled, peer),
            "receiving the peer does not prove our async writer sent back"
        );
        settle_broadcast_ack(
            &mut pending,
            &mut settled,
            UplinkAck {
                sequence: 41,
                outcome: UplinkOutcome::Dropped,
            },
        );
        assert!(
            !replication_is_mutual(&freshness, &settled, peer),
            "the impaired router dropping our reply is still one-way"
        );

        pending.insert(42, 6);
        settle_broadcast_ack(
            &mut pending,
            &mut settled,
            UplinkAck {
                sequence: 42,
                outcome: UplinkOutcome::Delivered,
            },
        );
        assert!(
            replication_is_mutual(&freshness, &settled, peer),
            "the peer arrived here and our state is retained for its exact slot"
        );
    }

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

    /// Dense replication is exact: every unfilled send slot inside the
    /// reorder window is a drop, counted once the window has passed — not
    /// on the arrival that opened the gap, because a packet the link only
    /// delayed could still land and retract it.
    #[test]
    fn dense_streaming_counts_real_loss_exactly() {
        let mut tracker = DownlinkTracker::default();
        let t = |n| f64::from(n) * 1_000.0;
        assert_eq!(
            tracker.record(7, 100, t(0)),
            Arrival {
                missing: 0,
                deviation_ms: None
            }
        );
        assert_eq!(
            tracker.record(7, 103, t(50)),
            Arrival {
                missing: 0,
                deviation_ms: None
            }
        );
        assert_eq!(tracker.record(7, 106, t(100)).missing, 0);
        assert_eq!(tracker.record(7, 109, t(150)).deviation_ms, Some(0));
        // Tick 112 is dropped: the gap opens here, reporting nothing yet.
        assert_eq!(tracker.record(7, 115, t(200)).missing, 0);
        assert_eq!(tracker.record(7, 118, t(250)).missing, 0);
        // …and settles as one loss only once the reorder window has passed.
        assert_eq!(tracker.record(7, 121, t(300)).missing, 1);
        // Two consecutive drops (127, 130) settle as two.
        assert_eq!(tracker.record(7, 124, t(350)).missing, 0);
        assert_eq!(tracker.record(7, 133, t(400)).missing, 0);
        assert_eq!(tracker.record(7, 139, t(450)).missing, 2);
        assert_eq!(tracker.total_missing(), 3);
        assert_eq!(tracker.unattributed_slots(), 0);
        assert_eq!(tracker.senders(), 1);
    }

    /// A gap opened by a stream's *final* arrivals never settles, so a cut
    /// taken there under-reports loss by exactly those gaps. This is the
    /// price of not scoring a merely-delayed packet, and it is the reason a
    /// harness reconciling [`CampaignRuntime::downlink_accounting`] against a
    /// sender ledger must stop the sender [`REORDER_WINDOW_SLOTS`] `+ 1`
    /// delivered broadcasts past its last skipped one (#1024).
    ///
    /// Without the margin the fixture's conservation check reads this lag as
    /// a lost frame; with it, the same check holds exactly.
    #[test]
    fn a_trailing_gap_settles_only_after_the_reorder_window_passes() {
        let slot = SEND_EVERY_TICKS;
        // Broadcasts on the send-slot grid, one skipped, then `margin` more.
        // The skip is conserved only once the margin reaches the horizon.
        let ledger = |margin: u64| {
            let mut tracker = DownlinkTracker::default();
            let mut arrivals = 0u64;
            let mut deliver = |tracker: &mut DownlinkTracker, index: u64| {
                let _ = tracker.record(0, index * slot, (index * 50) as f64);
                arrivals += 1;
            };
            for index in 1..=6 {
                deliver(&mut tracker, index);
            }
            // Broadcast 7 is skipped; 8.. are delivered.
            for index in 8..8 + margin {
                deliver(&mut tracker, index);
            }
            // Sender ledger: `7 + margin` broadcasts produced, one skipped.
            (arrivals + tracker.total_missing(), 7 + margin)
        };
        // A cut one or two delivered broadcasts past the skip is short by it.
        assert_eq!(ledger(1), (7, 8), "the skip is still inside its window");
        assert_eq!(ledger(2), (8, 9), "still inside its window");
        // One slot further — REORDER_WINDOW_SLOTS + 1 past the skip — and the
        // gap settles, so arrivals plus loss equal what the sender produced.
        for margin in REORDER_WINDOW_SLOTS + 1..=6 {
            let (accounted, produced) = ledger(margin);
            assert_eq!(
                accounted, produced,
                "conservation holds {margin} delivered broadcasts past the skip"
            );
        }
    }

    /// A gap wider than the reorder window matches no cadence the sender
    /// exhibited, so it is counted as unattributable rather than scored as
    /// loss. This is the stated limit of what a tick stream can decide —
    /// three consecutive drops and three send slots of silence look
    /// identical from the client — and the reason an exact count wants a
    /// sequence number on the replication envelope.
    #[test]
    fn a_gap_wider_than_the_reorder_window_is_silence_not_loss() {
        let mut tracker = DownlinkTracker::default();
        let _ = tracker.record(1, 100, 0.0);
        let _ = tracker.record(1, 103, 50.0);
        let _ = tracker.record(1, 106, 100.0);
        // Ticks 109, 112 and 115 never arrive.
        let _ = tracker.record(1, 118, 200.0);
        let _ = tracker.record(1, 121, 250.0);
        let _ = tracker.record(1, 124, 300.0);
        assert_eq!(tracker.total_missing(), 0);
        assert_eq!(tracker.unattributed_gaps(), 1);
        assert_eq!(tracker.unattributed_slots(), 3);
    }

    /// The impaired profile delays a tenth of all datagrams by two send
    /// slots, so the overtaken packet arrives after the one behind it. The
    /// gap the overtake seemed to open must be retracted when the delayed
    /// packet lands: the estimator this replaced scored every such gap as
    /// loss, which is how a link measurably running at 3% read as 13-30%.
    #[test]
    fn a_reordered_packet_retracts_the_gap_it_fills() {
        let mut tracker = DownlinkTracker::default();
        let _ = tracker.record(2, 30, 0.0);
        let _ = tracker.record(2, 33, 50.0);
        let _ = tracker.record(2, 36, 100.0);
        // Tick 39 is two slots late; tick 42 overtakes it.
        let _ = tracker.record(2, 42, 150.0);
        let _ = tracker.record(2, 39, 200.0);
        let _ = tracker.record(2, 45, 250.0);
        assert_eq!(
            tracker.record(2, 48, 300.0).missing,
            0,
            "the late packet was delivered, not lost"
        );
        assert_eq!(tracker.total_missing(), 0);
    }

    /// Heartbeat and churn silences are cadence, not loss. A craft that
    /// goes idle still sends its one-hertz keyframe (twenty slots apart);
    /// interest gating takes a peer out of scope entirely. Both read as
    /// wide gaps in the tick stream, and the estimator this replaced
    /// scored every silent slot as a lost broadcast — the part of #972
    /// that grew with session length.
    #[test]
    fn heartbeat_and_churn_silences_are_not_scored_as_loss() {
        let mut tracker = DownlinkTracker::default();
        // Dense replication: a datagram every send slot.
        for tick in (300..=321u32).step_by(3) {
            let _ = tracker.record(3, u64::from(tick), f64::from(tick));
        }
        // Idle: the one-hertz keyframe heartbeat, twenty slots apart. The
        // first of these is a cadence change and lands in the
        // unattributable bucket; once heartbeats dominate the history, a
        // heartbeat gap settles as zero missing through the ordinary rule.
        for beat in 1..=7u64 {
            let tick = 321 + beat * 60;
            let _ = tracker.record(3, tick, f64::from(u32::try_from(tick).expect("small")));
        }
        let unattributed_heartbeats = tracker.unattributed_gaps();
        let _ = tracker.record(3, 801, 801.0);
        let _ = tracker.record(3, 861, 861.0);
        assert_eq!(
            tracker.unattributed_gaps(),
            unattributed_heartbeats,
            "a steady heartbeat gap is cadence, not silence"
        );
        assert_eq!(tracker.total_missing(), 0);
        // Interest churn: the peer leaves scope for three seconds and
        // comes back. Nothing about that is loss.
        let _ = tracker.record(3, 1041, 1041.0);
        let _ = tracker.record(3, 1044, 1044.0);
        let _ = tracker.record(3, 1047, 1047.0);
        assert_eq!(tracker.total_missing(), 0);
        assert!(
            tracker.unattributed_slots() > 0,
            "the churn is counted honestly as unattributable, not hidden"
        );
    }

    /// The stride is the send-slot grid, not a learned value: a first
    /// interval that happens to span a loss must not poison every later
    /// gap. The estimator this replaced locked its stride onto the first
    /// delta, so a loss-straddling first interval turned every later
    /// real loss into zero.
    #[test]
    fn a_loss_straddling_first_interval_does_not_poison_the_grid() {
        let mut tracker = DownlinkTracker::default();
        let _ = tracker.record(4, 100, 0.0);
        // First interval spans a loss (tick 103 never arrives)...
        let _ = tracker.record(4, 106, 50.0);
        let _ = tracker.record(4, 109, 100.0);
        let _ = tracker.record(4, 112, 150.0);
        let _ = tracker.record(4, 115, 200.0);
        // ...but once dense replication re-establishes the cadence, both
        // the straddled loss and a later one (tick 118) are counted.
        let _ = tracker.record(4, 121, 250.0);
        assert_eq!(tracker.record(4, 124, 300.0).missing, 0);
        assert_eq!(tracker.record(4, 127, 350.0).missing, 1);
        assert_eq!(tracker.total_missing(), 2);
    }

    /// Stale and duplicate ticks are deliveries. A retransmitted keyframe
    /// (an old tick, re-sent when a peer re-enters scope) retracts the
    /// slot it carries — once, however often it repeats — and a duplicate
    /// of the newest tick retracts nothing.
    #[test]
    fn stale_and_duplicate_arrivals_are_deliveries_not_loss() {
        let mut tracker = DownlinkTracker::default();
        let _ = tracker.record(5, 100, 0.0);
        let _ = tracker.record(5, 103, 50.0);
        let _ = tracker.record(5, 106, 100.0);
        // Tick 109 is dropped; a retransmission of it arrives late.
        let _ = tracker.record(5, 112, 150.0);
        let _ = tracker.record(5, 109, 175.0);
        let _ = tracker.record(5, 109, 180.0);
        let _ = tracker.record(5, 112, 200.0);
        assert_eq!(
            tracker.record(5, 118, 250.0).missing,
            0,
            "the retransmission retracted the slot, exactly once"
        );
        // Tick 115 is genuinely dropped.
        assert_eq!(tracker.record(5, 124, 300.0).missing, 1);
        assert_eq!(tracker.total_missing(), 1);
    }

    /// The whole estimator, replayed against a captured run with the
    /// host's own per-directed-link counters as ground truth (#965's
    /// `per_link_impairment`).
    ///
    /// The fixture is the client-side arrival stream (sender, tick,
    /// arrival time) of a real 30 s witnessed impaired attempt driven for
    /// #972: the shipped client dialling a `p1-swarm --external-peer`
    /// host over QUIC under the 3% / 100 ms profile. The host counted
    /// 5642 datagrams on the links toward the client's slot and dropped
    /// 169 of them — 2.995%. The estimator this replaced reported 20.5%
    /// from this same stream. No clamping pins the answer: the replayed
    /// missing count is asserted exactly, and its rate must sit beside
    /// the host's, not merely inside the criterion's 2-point tolerance.
    #[test]
    fn captured_impaired_run_replays_beside_the_host_per_link_truth() {
        const CAPTURE: &str = include_str!("../tests/fixtures/downlink-capture-972.csv");
        // From the attempt report the host wrote for this run.
        const HOST_DELIVERED: u64 = 5473;
        const HOST_DROPPED: u64 = 169;

        let mut tracker = DownlinkTracker::default();
        let mut arrivals = 0u64;
        for row in CAPTURE.lines().filter(|line| !line.is_empty()) {
            let mut fields = row.split(',');
            let sender: u32 = fields.next().expect("sender").parse().expect("sender");
            let tick: u64 = fields.next().expect("tick").parse().expect("tick");
            let now_ms: f64 = fields.next().expect("now_ms").parse().expect("now_ms");
            let _ = tracker.record(sender, tick, now_ms);
            arrivals += 1;
        }
        // Pinned exactly: any change to the estimator's arithmetic moves
        // this number and must be re-validated against ground truth. The
        // session's final gap per sender is deliberately absent: once the
        // stream ends, nothing can distinguish its drops from its late
        // packets, so a settled count is the only count.
        assert_eq!(tracker.total_missing(), 145);
        assert_eq!(arrivals, 3758);
        let rate =
            tracker.total_missing() as f64 * 100.0 / (tracker.total_missing() + arrivals) as f64;
        let host_rate = HOST_DROPPED as f64 * 100.0 / (HOST_DELIVERED + HOST_DROPPED) as f64;
        assert!(
            (rate - host_rate).abs() <= 1.0,
            "client measured {rate:.2}% but the host counted {host_rate:.2}% on the same links"
        );
        // The honest hole stays visible and bounded: unattributable
        // silence exists in every real session, but must never swallow
        // the measurement.
        assert!(tracker.unattributed_slots() > 0);
        assert!(
            tracker.unattributed_slots() < arrivals,
            "unattributable silence exceeded the observed stream"
        );
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

    /// #940: a replicated craft is frozen between refreshes — the ingest path
    /// installs decoded state verbatim and nothing dead-reckons it — so the
    /// skin's exact metre reading is only as good as the replica's age. This
    /// pins that the age the skin can ask for is the real elapsed gap, taken
    /// through the same `refresh_replica` / `expire_stale_replicas` path
    /// production uses, at ages production actually produces (0 through the
    /// TTL, since expiry retires anything older).
    #[test]
    fn a_replicas_age_is_readable_for_every_tick_it_stays_installed() {
        let remote = PersistId::new(3);
        let mut freshness = BTreeMap::new();

        assert_eq!(
            replica_age_ticks(&freshness, 500, remote),
            None,
            "an entity that has never been replicated has no age to report"
        );

        let install_tick = 1_000;
        refresh_replica(&mut freshness, remote, 2, install_tick, 998);
        for age in 0..=REPLICA_TTL_TICKS {
            assert_eq!(
                replica_age_ticks(&freshness, install_tick + age, remote),
                Some(age),
                "a replica installed at {install_tick} must read {age} ticks old"
            );
        }

        // The staleness this discloses is not theoretical. At the
        // interceptor's ceiling the frozen body has moved this far by the
        // time the TTL retires it.
        let ceiling_mms = orrery_games::regolith::archetype::Archetype::Interceptor
            .limits()
            .max_speed_mms;
        let drift_m =
            ceiling_mms * REPLICA_TTL_TICKS as i64 / i64::from(orrery_core::TICK_HZ) / 1_000;
        assert!(
            drift_m > 500,
            "a full-TTL replica can be {drift_m} m from where it is drawn; \
             an unqualified metre reading is not a measurement"
        );

        // A retired replica reports no age at all rather than a growing one:
        // there is no live replica left to be stale.
        let game = Regolith::honest();
        let own = PersistId::new(9);
        let mut executor = Executor::new(game, UniverseSeed([0x61; 32]));
        executor.insert(own, game.spawn(own, 8));
        executor.insert(remote, game.spawn(remote, 2));
        let mut focus = Some(remote);
        expire_stale_replicas(
            &mut executor,
            own,
            install_tick + REPLICA_TTL_TICKS + 1,
            &mut freshness,
            &mut focus,
        );
        assert_eq!(
            replica_age_ticks(&freshness, install_tick + REPLICA_TTL_TICKS + 1, remote),
            None,
            "an expired replica is not a very stale one, it is absent"
        );
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
                authority_slot: 2,
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
        let _ = refresh_replica(&mut freshness, remote, 2, 10, 10);

        for tick in [70, 130, 190] {
            expire_stale_replicas(&mut executor, own, tick, &mut freshness, &mut focus);
            assert!(
                executor.state(remote).is_some(),
                "a replica refreshed at the ruleset's one-second allowance stays drawable"
            );
            let _ = refresh_replica(&mut freshness, remote, 2, tick, tick);
        }
    }

    #[test]
    fn materialised_replica_route_has_exactly_the_replica_lifetime() {
        let game = Regolith::honest();
        let own = PersistId::new(9);
        let materialised = PersistId::new(0xC524_1234_5678_9ABC);
        let mut executor = Executor::new(game, UniverseSeed([0x63; 32]));
        executor.insert(own, game.spawn(own, 8));
        executor.insert(materialised, game.spawn(materialised, 2));
        let mut freshness = BTreeMap::new();
        let mut focus = Some(materialised);

        let _ = refresh_replica(&mut freshness, materialised, 4, 10, 9);
        assert_eq!(replica_authority_slot(&freshness, materialised), Some(4));

        expire_stale_replicas(
            &mut executor,
            own,
            11 + REPLICA_TTL_TICKS,
            &mut freshness,
            &mut focus,
        );
        assert_eq!(
            replica_authority_slot(&freshness, materialised),
            None,
            "an expired replica must not leave a stale delivery route"
        );

        let _ = refresh_replica(&mut freshness, materialised, 6, 132, 130);
        assert_eq!(
            replica_authority_slot(&freshness, materialised),
            Some(6),
            "a reinstalled replica learns its current authority from its packet"
        );
    }

    #[test]
    fn replica_measurement_retains_the_gap_across_expiry_and_reinstall() {
        let remote = PersistId::new(3);
        let mut freshness = BTreeMap::new();

        let _ = refresh_replica(&mut freshness, remote, 2, 10, 9);
        freshness.get_mut(&remote).expect("installed").installed = false;
        let measurement = refresh_replica(&mut freshness, remote, 2, 211, 210);

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
            own_label: None,
            session_id: "s-1".to_owned(),
            session_token_hex: None,
            wall_start_utc: "2026-08-24T00:00:00Z".to_owned(),
            configured: ConfiguredImpairment {
                loss_pct: 3.0,
                jitter_p50_ms: 100,
                jitter_p99_ms: 100,
            },
            transport_secret: iroh_base::SecretKey::from_bytes(&[0x49; 32]),
            island_seats: None,
            roster_url: None,
        };
        assert_eq!(config.actor(), Actor::Human);
    }

    /// The spawn pose is a function of `(slot, island_seats)`, and the host
    /// computes it over the *configured* island. A human in seat 4 of 8 whose
    /// client derived `slot + 1 = 5` would spawn on a different orbit than the
    /// host put it on, so the number must come from the host when the host
    /// states it and only fall back where the two provably agree.
    #[test]
    fn the_spawn_pose_uses_the_island_the_host_configured() {
        let mut config = CampaignConfig {
            host_node_hex: "61a71521afb8e193d0d0fc248f85ed20bc78efa1120c83334579129b4171405b"
                .to_owned(),
            host_direct: None,
            slot: 4,
            own_label: None,
            session_id: "s-1".to_owned(),
            session_token_hex: None,
            wall_start_utc: "2026-08-24T00:00:00Z".to_owned(),
            configured: ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
            transport_secret: iroh_base::SecretKey::from_bytes(&[0x49; 32]),
            island_seats: Some(8),
            roster_url: None,
        };
        assert_eq!(config.island_seats(), 8);
        assert_eq!(
            campaign_spawn_pose(4, 8),
            campaign_spawn_pose(config.slot, usize::from(config.island_seats())),
        );
        assert_ne!(
            campaign_spawn_pose(4, 8),
            campaign_spawn_pose(4, 5),
            "the pre-#573 derivation is a different orbit, which is the bug"
        );

        // A service that never says how big the island is is a one-human
        // service, where the sole human sits in the last seat and the two
        // derivations agree exactly.
        config.island_seats = None;
        config.slot = 7;
        assert_eq!(config.island_seats(), 8);

        // The expectation the manifest is checked against is built from the
        // same numbers, so it cannot drift from the pose that was anchored.
        let expectation = config.start_expectation();
        assert_eq!((expectation.slot, expectation.entity), (7, 8));
        assert_eq!(expectation.island_seats, 8);
        assert_eq!(
            expectation.node_hex,
            config.transport_secret.public().to_string()
        );
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
        let keyframe_state = game.spawn(entity, 4);
        executor.insert(entity, keyframe_state.clone());
        let keyframe_canonical = executor
            .state(entity)
            .expect("inserted keyframe state")
            .to_canonical();
        let cell = CellId::from_coords(IVec3::ONE, orrery_protocol::INTEREST_LEVEL)
            .expect("representable cell");
        let mut keyframe = None;
        let mut send_index = 0;
        let bytes =
            encode_state_broadcast(&executor, entity, cell, 42, &mut keyframe, &mut send_index)
                .expect("first broadcast is a keyframe");
        let expected_keyframe = orrery_protocol::channels::tag(
            orrery_protocol::channels::Channel::State,
            &orrery_protocol::channels::encode_replication_compressed(&(
                keyframe_canonical,
                cell,
                entity,
                42_u64,
            )),
        );
        assert_eq!(
            bytes.as_ref(),
            expected_keyframe,
            "the client's keyframe bytes must be the gate bot's double-tagged wire"
        );
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

        let RegolithState::Craft(mut current_state) = keyframe_state else {
            unreachable!("Regolith spawn is a craft")
        };
        current_state.pos.x += 1;
        let current_state = RegolithState::Craft(current_state);
        executor.insert(entity, current_state);
        let current = executor
            .state(entity)
            .expect("inserted changed state")
            .to_canonical();
        let delta =
            encode_state_broadcast(&executor, entity, cell, 45, &mut keyframe, &mut send_index)
                .expect("changed state broadcasts a delta");
        let expected_delta = orrery_protocol::channels::tag(
            orrery_protocol::channels::Channel::State,
            &orrery_protocol::channels::encode_replication_delta(
                &(current.clone(), cell, entity, 45_u64),
                &orrery_protocol::channels::ReplicationDelta {
                    entity,
                    tick: 45,
                    keyframe_age: 3,
                    cell: None,
                    patch: orrery_protocol::channels::encode_delta_patch(&encoded, &current),
                },
            ),
        );
        assert_eq!(
            delta.as_ref(),
            expected_delta,
            "the client's delta bytes must be the gate bot's double-tagged wire"
        );
        let (_, delta_inner) =
            orrery_protocol::channels::untag(&delta).expect("delta outer channel tag");
        let (_, delta_body) =
            orrery_protocol::channels::untag(delta_inner).expect("delta inner channel tag");
        assert_eq!(
            delta_body.first().copied(),
            Some(orrery_protocol::channels::TAG_REPLICATION_DELTA),
            "the changed state pair must exercise the delta branch"
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

    /// A campaign config for the undecodable-split tests: a bogus host the
    /// dial thread can fail against in its own time, since these tests never
    /// poll it.
    fn undecodable_test_config(session: &str) -> CampaignConfig {
        CampaignConfig {
            host_node_hex: "61a71521afb8e193d0d0fc248f85ed20bc78efa1120c83334579129b4171405b"
                .to_owned(),
            host_direct: None,
            slot: 4,
            own_label: None,
            session_id: session.to_owned(),
            session_token_hex: None,
            wall_start_utc: "2026-08-24T00:00:00Z".to_owned(),
            configured: ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
            transport_secret: iroh_base::SecretKey::from_bytes(&[0x49; 32]),
            island_seats: Some(8),
            roster_url: None,
        }
    }

    /// #1034, the own side driven alone: a real order packet that really
    /// fails to decode, through the same method `advance` calls. Only the
    /// own counter may move — that the downlink counter stays at zero is
    /// the property that lets a reader tell a frozen tick from a stale
    /// replica.
    #[test]
    fn an_own_order_packet_that_fails_to_decode_counts_only_the_own_side() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("undecodable-split-own"),
            UniverseSeed([0xB3; 32]),
        );
        runtime.join_for_test();

        let packet = OrderPacket {
            tick: 1_000_000,
            entity: 1,
            orders: vec![vec![0xFF, 0x00]],
        };
        let error =
            decode_packet(&packet).expect_err("a garbage order vector cannot decode an Order");
        runtime.count_own_packet_decode_failure(Tick::new(1_000_000), &error);

        assert_eq!(runtime.own_orders_undecodable(), 1);
        assert_eq!(
            runtime.downlink_undecodable(),
            0,
            "an own-packet failure must not borrow the downlink counter"
        );
    }

    /// #1034, the downlink side driven alone: frames the downlink delivered
    /// that decode to nothing this client recognises, through the same
    /// method `advance`'s inbound loop calls. The first frame has no State
    /// outer tag at all; the second carries one but is neither a replication
    /// keyframe, nor a delta, nor a decodable witness record — the arm a
    /// *witness-tagged but malformed* body lands in since #1039 exempted
    /// well-formed witness records from it. The 2026-09-04 session's steady
    /// 39/s used to land here too, before that exemption; only the downlink
    /// counter may move.
    #[test]
    fn a_downlink_frame_that_decodes_to_nothing_counts_only_the_downlink_side() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("undecodable-split-downlink"),
            UniverseSeed([0xB4; 32]),
        );
        runtime.join_for_test();

        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: Bytes::from_static(b"no channel tag at all"),
            },
            Tick::new(1_000),
        );
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: Bytes::from(orrery_protocol::channels::tag(
                    orrery_protocol::channels::Channel::State,
                    b"neither replication nor a delta",
                )),
            },
            Tick::new(1_001),
        );

        assert_eq!(runtime.downlink_undecodable(), 2);
        assert_eq!(
            runtime.own_orders_undecodable(),
            0,
            "a downlink failure must not borrow the own-packet counter"
        );
    }

    /// A remote craft the way `Game::spawn` mints it, for the delta-cause
    /// and witness fixtures below.
    fn remote_craft(entity: PersistId) -> RegolithState {
        Regolith::honest().spawn(entity, 2)
    }

    /// The same craft one step further on, the way the wire fixtures below
    /// produce a delta worth applying.
    fn advanced_craft(state: &RegolithState) -> RegolithState {
        let RegolithState::Craft(mut craft) = state.clone() else {
            unreachable!("spawn mints a craft")
        };
        craft.pos.x += 1;
        RegolithState::Craft(craft)
    }

    /// One received keyframe as the harness wire carries it: the sender's
    /// replication envelope already opens with the inner `[TAG_STATE]`, then
    /// the transport tag `receive_peer_packets` strips — and this client's
    /// downlink delivers the bytes with that transport tag intact, so the
    /// frame here is the double-tagged form #387 pinned.
    fn keyframe_wire(state: &RegolithState, entity: PersistId, cell: CellId, at: u64) -> Bytes {
        use orrery_protocol::channels::{encode_replication_compressed, tag, Channel};
        Bytes::from(tag(
            Channel::State,
            &encode_replication_compressed(&(state.to_canonical(), cell, entity, at)),
        ))
    }

    /// One received delta in the same double-tagged form, with the caller's
    /// patch, age and tick — every knob a delta-cause fixture needs to move.
    ///
    /// Asserts the delta form itself: `encode_replication_delta` falls back
    /// to an absolute keyframe whenever the delta would not be smaller, and
    /// a fixture that silently exercised the keyframe arm would prove
    /// nothing about the counters it names.
    fn delta_wire(
        current: &RegolithState,
        entity: PersistId,
        cell: CellId,
        at: u64,
        keyframe_age: u16,
        patch: Vec<u8>,
    ) -> Bytes {
        use orrery_protocol::channels::{
            encode_replication_delta, tag, untag, Channel, ReplicationDelta, TAG_REPLICATION_DELTA,
        };
        let delta = ReplicationDelta {
            entity,
            tick: at,
            keyframe_age,
            cell: None,
            patch,
        };
        let wire = tag(
            Channel::State,
            &encode_replication_delta(&(current.to_canonical(), cell, entity, at), &delta),
        );
        let (_, inner) = untag(&wire).expect("the transport channel tag");
        let (_, body) = untag(inner).expect("the replication channel tag");
        assert_eq!(
            body.first().copied(),
            Some(TAG_REPLICATION_DELTA),
            "the fixture must exercise the delta arm, not the keyframe fallback"
        );
        Bytes::from(wire)
    }

    /// Which delta-cause counter a fixture asserts to be moving.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum DeltaCause {
        DeltasWithoutKeyframe,
        DeltasUnanchored,
        DeltaPatchFailures,
        DeltaBodiesUndecodable,
    }

    use DeltaCause::*;

    /// The four delta-cause counters at zero, each spelled out so a test
    /// that means "nothing moved" cannot pass while one of the four quietly
    /// did.
    fn assert_no_delta_causes(runtime: &CampaignRuntime) {
        for (name, count) in [
            ("deltas_without_keyframe", runtime.deltas_without_keyframe()),
            ("deltas_unanchored", runtime.deltas_unanchored()),
            ("delta_patch_failures", runtime.delta_patch_failures()),
            (
                "delta_bodies_undecodable",
                runtime.delta_bodies_undecodable(),
            ),
        ] {
            assert_eq!(count, 0, "{name} moved where nothing should have");
        }
    }

    /// Every delta-cause counter other than `cause` at zero. The named
    /// cause's own count is asserted explicitly at its call site, so a
    /// fixture cannot pass by asserting only the absence of the others.
    fn assert_no_delta_causes_except(runtime: &CampaignRuntime, cause: DeltaCause) {
        let checks = [
            (
                DeltaCause::DeltasWithoutKeyframe,
                "deltas_without_keyframe",
                runtime.deltas_without_keyframe(),
            ),
            (
                DeltaCause::DeltasUnanchored,
                "deltas_unanchored",
                runtime.deltas_unanchored(),
            ),
            (
                DeltaCause::DeltaPatchFailures,
                "delta_patch_failures",
                runtime.delta_patch_failures(),
            ),
            (
                DeltaCause::DeltaBodiesUndecodable,
                "delta_bodies_undecodable",
                runtime.delta_bodies_undecodable(),
            ),
        ];
        for (variant, name, count) in checks {
            if variant == cause {
                continue;
            }
            assert_eq!(count, 0, "{name} moved where only {cause:?} should have");
        }
    }

    /// The whole healthy reading: no delta cause and no unintelligible
    /// bytes.
    fn assert_no_delta_causes_and_no_undecodable(runtime: &CampaignRuntime) {
        assert_no_delta_causes(runtime);
        assert_eq!(runtime.downlink_undecodable(), 0);
    }

    /// #1039, the exemption the bot cohort already had (`gates/p1-swarm/
    /// src/bot.rs`, the `ReplicaDecodeError::NotReplication` arm): a witness
    /// record on the lossy state lane is routine traffic, recognised by the
    /// same sub-tag mechanism the bot uses — `decode_witness` — and counted
    /// by nothing. A witness *tag* over bytes no `WitnessMsg` accepts still
    /// counts, because the sub-tag routes and the type check decides.
    ///
    /// The frame bytes are the shape the wire actually delivers: the bot's
    /// `encode_witness` payload is itself channel-tagged, and the transport
    /// tag wraps it, so this client strips one tag and `decode_witness`
    /// strips the second.
    #[test]
    fn a_witness_record_on_the_state_lane_is_exempt_and_counts_nowhere() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("witness-exemption"),
            UniverseSeed([0xB5; 32]),
        );
        runtime.join_for_test();

        let signer = iroh_base::SecretKey::from_bytes(&[0x71; 32]);
        let claim = orrery_protocol::StateClaim {
            entity: PersistId::new(9),
            chain_epoch: 0,
            tick: Tick::new(600),
            input_head: orrery_protocol::ChainHash::EMPTY,
            state_hash: [2; 32],
            prev_claim: [0; 32],
            ruleset: orrery_protocol::RulesetId {
                version: 1,
                digest: [1; 32],
            },
            sig: signer.sign(b"claim"),
        };
        let witness_payload = orrery_protocol::channels::encode_witness(&WitnessMsg::Claim(claim));
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: Bytes::from(orrery_protocol::channels::tag(
                    orrery_protocol::channels::Channel::State,
                    &witness_payload,
                )),
            },
            Tick::new(1_000),
        );
        assert_no_delta_causes_and_no_undecodable(&runtime);

        let mut tagged = vec![orrery_protocol::channels::TAG_WITNESS];
        tagged.extend_from_slice(b"not postcard for any WitnessMsg");
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: Bytes::from(orrery_protocol::channels::tag(
                    orrery_protocol::channels::Channel::State,
                    &tagged,
                )),
            },
            Tick::new(1_001),
        );

        assert_eq!(
            runtime.downlink_undecodable(),
            1,
            "the exemption follows the bot's: a decodable witness record is \
             exempt, witness-tagged garbage is still undecodable"
        );
        assert_no_delta_causes(&runtime);
    }

    /// #1039, delta cause 1: a delta for an entity no keyframe has arrived
    /// for. Still counted — it says the keyframe carrying the anchor was
    /// lost — but in its own counter, not `downlink_undecodable`.
    #[test]
    fn a_delta_without_a_retained_keyframe_counts_in_its_own_counter() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("delta-cause-without-keyframe"),
            UniverseSeed([0xB6; 32]),
        );
        runtime.join_for_test();

        let entity = PersistId::new(9);
        let cell = CellId::from_bits(7).expect("a cell");
        let state = remote_craft(entity);
        let changed = advanced_craft(&state);
        let patch = orrery_protocol::channels::encode_delta_patch(
            &state.to_canonical(),
            &changed.to_canonical(),
        );
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: delta_wire(&changed, entity, cell, 45, 3, patch),
            },
            Tick::new(1_000),
        );

        assert_eq!(runtime.deltas_without_keyframe(), 1);
        assert_eq!(
            runtime.downlink_undecodable(),
            0,
            "a delta failure must not borrow the unintelligible-byte counter"
        );
        assert_no_delta_causes_except(&runtime, DeltasWithoutKeyframe);
    }

    /// #1039, delta cause 2: a delta whose tick and keyframe age do not
    /// anchor it to the retained keyframe (`delta_is_anchored` refuses).
    /// The bot cohort keeps three causes here; this client checks once and
    /// counts once.
    #[test]
    fn a_delta_that_fails_its_anchor_check_counts_in_its_own_counter() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("delta-cause-unanchored"),
            UniverseSeed([0xB7; 32]),
        );
        runtime.join_for_test();

        let entity = PersistId::new(9);
        let cell = CellId::from_bits(7).expect("a cell");
        let state = remote_craft(entity);
        let canonical = state.to_canonical();
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: keyframe_wire(&state, entity, cell, 42),
            },
            Tick::new(1_000),
        );

        let changed = advanced_craft(&state);
        let patch =
            orrery_protocol::channels::encode_delta_patch(&canonical, &changed.to_canonical());
        // The keyframe is at 42; a delta at 45 naming an age of 4 references
        // tick 41, which this client does not hold.
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: delta_wire(&changed, entity, cell, 45, 4, patch),
            },
            Tick::new(1_001),
        );

        assert_eq!(runtime.deltas_unanchored(), 1);
        assert_eq!(
            runtime.downlink_undecodable(),
            0,
            "a delta failure must not borrow the unintelligible-byte counter"
        );
        assert_no_delta_causes_except(&runtime, DeltasUnanchored);
    }

    /// #1039, delta cause 3: a delta whose patch does not apply. The bytes
    /// parsed as a delta — the envelope is well-formed — but the skip/write
    /// program is malformed, so `apply_delta_patch` refuses.
    #[test]
    fn a_delta_whose_patch_fails_to_apply_counts_in_its_own_counter() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("delta-cause-patch-failure"),
            UniverseSeed([0xB8; 32]),
        );
        runtime.join_for_test();

        let entity = PersistId::new(9);
        let cell = CellId::from_bits(7).expect("a cell");
        let state = remote_craft(entity);
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: keyframe_wire(&state, entity, cell, 42),
            },
            Tick::new(1_000),
        );

        let changed = advanced_craft(&state);
        // 0xFF repeated is a varint that never terminates canonically: the
        // envelope parses, the program does not.
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: delta_wire(&changed, entity, cell, 42, 0, vec![0xFF; 9]),
            },
            Tick::new(1_001),
        );

        assert_eq!(runtime.delta_patch_failures(), 1);
        assert_eq!(
            runtime.downlink_undecodable(),
            0,
            "a delta failure must not borrow the unintelligible-byte counter"
        );
        assert_no_delta_causes_except(&runtime, DeltaPatchFailures);
    }

    /// #1039, delta cause 4: a delta whose patch applies but whose produced
    /// body is not a state this client's codec accepts. The bot cohort
    /// counts this as `bad_body` — bytes the receiver's own codec refuses —
    /// and the client keeps it visible under its own name.
    #[test]
    fn a_delta_whose_patched_body_does_not_decode_counts_in_its_own_counter() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("delta-cause-body-undecodable"),
            UniverseSeed([0xB9; 32]),
        );
        runtime.join_for_test();

        let entity = PersistId::new(9);
        let cell = CellId::from_bits(7).expect("a cell");
        let state = remote_craft(entity);
        let canonical = state.to_canonical();
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: keyframe_wire(&state, entity, cell, 42),
            },
            Tick::new(1_000),
        );

        let changed = advanced_craft(&state);
        // A well-formed program that rewrites the body to nothing: the patch
        // applies, the empty result decodes as no `RegolithState`.
        let patch = orrery_protocol::channels::encode_delta_patch(&canonical, b"");
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: delta_wire(&changed, entity, cell, 42, 0, patch),
            },
            Tick::new(1_001),
        );

        assert_eq!(runtime.delta_bodies_undecodable(), 1);
        assert_eq!(
            runtime.downlink_undecodable(),
            0,
            "a delta failure must not borrow the unintelligible-byte counter"
        );
        assert_no_delta_causes_except(&runtime, DeltaBodiesUndecodable);
    }

    /// The control the four cause tests imply: a well-formed keyframe and a
    /// well-anchored delta still land, and no counter moves. The exemption
    /// and the split must not swallow replication itself, and the delta
    /// counters must not fire on the healthy cadence they were split away
    /// from.
    #[test]
    fn a_well_formed_keyframe_and_delta_still_land_and_count_nowhere() {
        let mut runtime = CampaignRuntime::launch(
            undecodable_test_config("delta-cause-control"),
            UniverseSeed([0xBA; 32]),
        );
        runtime.join_for_test();

        let entity = PersistId::new(9);
        let cell = CellId::from_bits(7).expect("a cell");
        let state = remote_craft(entity);
        let canonical = state.to_canonical();
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: keyframe_wire(&state, entity, cell, 42),
            },
            Tick::new(1_000),
        );
        assert_eq!(
            runtime.downlink_last_tick(2),
            Some(42),
            "the keyframe landed and was accounted"
        );

        let changed = advanced_craft(&state);
        let patch =
            orrery_protocol::channels::encode_delta_patch(&canonical, &changed.to_canonical());
        runtime.accept_frame(
            net::Frame {
                peer: 2,
                lane: Lane::Datagram,
                payload: delta_wire(&changed, entity, cell, 45, 3, patch),
            },
            Tick::new(1_001),
        );
        assert_eq!(
            runtime.downlink_last_tick(2),
            Some(45),
            "the anchored delta landed and was accounted"
        );
        assert_no_delta_causes_and_no_undecodable(&runtime);
    }
}
