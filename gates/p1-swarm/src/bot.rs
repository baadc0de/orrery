//! One synthetic peer: a headless Bevy app running the shipping plugin stack,
//! plus a scripted roam.
//!
//! # Time is simulated, not wall-clock
//!
//! The app's `Time<Real>` is advanced by exactly one 60 Hz tick per update
//! rather than by `TimePlugin` reading a clock. Everything downstream that
//! measures a rate — the upload meter above all — therefore measures *bytes per
//! simulated second*, which is the quantity the ≤ 1 Mbps budget is actually
//! about: the send cadence is 20 Hz of sim ticks, not of wall seconds.
//!
//! It also makes an hour of simulated play cost whatever it costs to compute
//! rather than an hour, and makes a run reproducible — both of which matter for
//! a criterion that wants hundreds of hours.
//!
//! # The roam
//!
//! Each bot thrusts along its heading and turns by a fixed rate, tracing a
//! circle whose radius is chosen so the path crosses cell boundaries steadily
//! instead of orbiting inside one. A straight line would cross cells too, but a
//! curve keeps re-entering the neighbourhood it just left, which is what
//! actually exercises hysteresis and interest-set churn.

use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aeronet_iroh::stream::{IrohStreamIo, RecvMessage, StreamMode};
use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_math::Vec3;
use bevy_time::{Real, Time};
use bytes::Bytes;

#[cfg(test)]
use orrery_core::Executor;
use orrery_core::{state_hash, tick_rng, CoreCodec, InputLogProducer, QPos};
use orrery_games::game::{Game, Tamper};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::order::{Order, Outcome};
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_games::regolith::weapon::WeaponKind;
use orrery_games::regolith::{
    collision_candidate, distance_mm, firing_arc_measurement, Regolith, REGOLITH_RULESET,
};
use orrery_net::budget::{datagram_wire_bytes, Bandwidth, UploadBudget, UploadMeter};
#[cfg(test)]
use orrery_net::channels::encode_replication;
use orrery_net::channels::{encode_replication_compressed, Channel};
use orrery_net::peer_link::{
    forget_departed_links, receive_peer_packets, send_peer_packets, PeerLinkCounters, PeerPacket,
    SendPacket,
};
use orrery_net::plugin::Peer;
use orrery_net::{IslandMembership, IslandSource};
use orrery_protocol::channels::{
    apply_delta_patch, decode_replication_delta, encode_delta_patch, encode_replication_delta,
    ReplicationDelta, TAG_REPLICATION_DELTA,
};
use orrery_protocol::coord::{IslandId, PeerEntry, TopologyRegime};
use orrery_protocol::{
    CellId, InterestCellCrossing, NodeId, PersistId, RecordSource, SeqPair, StateClaim, Tick,
    UniverseSeed, DEFAULT_CELL_EDGE_M,
};
use orrery_sim_host::{
    Delivery, InputOrigin, PredictionSet, RulesetAdapter, SealedInput, SimulationHost,
    SimulationHostConfig, TickCount, TickParticipant,
};
use orrery_spatial::hysteresis::GridPosition;
use orrery_spatial::interest::{HighRate, InterestSelection, Proxy};
use orrery_spatial::plugin::{Cell, LocalPlayer};
use orrery_spatial::{OrrerySpatialPlugin, SpatialConfig};
use orrery_witness::plugin::{
    PublishClaim, PublishFrame, ReportFiled, WitnessIdentity, WitnessLinkCounters, WitnessSet,
    WitnessState,
};
use orrery_witness::{Watch, Witness, WitnessConfig, WitnessPlugin, WitnessSignal, Witnessed};

use crate::delta_stats::DeltaStats;
use crate::profile::Profile;

/// Tick-zero claims repeat at 2 Hz (docs/06 §6).
const CLAIM_EVERY: u64 = 30;

/// Frame cadence derived from the witness lane's D16 budget.
const FRAME_TICKS: u16 = orrery_witness::plugin::frame_interval_ticks(
    1_000_000,
    orrery_witness::plugin::MAX_WITNESS_LINKS,
    TICK_HZ,
    CLAIM_EVERY,
);

/// The fixed simulation tick (VC-1).
pub const TICK: Duration = Duration::from_nanos(16_666_667);
/// Ticks per simulated second.
pub const TICK_HZ: u64 = 60;

/// Cruising speed in metres per second — a fast ground vehicle.
///
/// At a 128 m cell edge this is a crossing roughly every four simulated
/// seconds: fast enough to cover hundreds of cells in the criterion's hour,
/// slow enough that a bot dwells near a boundary long enough for the hysteresis
/// margin to matter. Anything much faster stops testing hysteresis and starts
/// testing whether cells can be skipped entirely.
const CRUISE_MPS: f64 = 32.0;

/// Default number of state sends between the 1 Hz keyframes.
///
/// [`Swarm`](crate::swarm::Swarm) replaces this with its configured send rate,
/// so lowering `--send-hz` never lowers the 1 Hz liveness heartbeat with it.
const DEFAULT_KEYFRAME_EVERY_SENDS: u64 = 20;

/// How soon a re-promotion counts as a pop, in ticks.
///
/// One simulated second. docs/03-replication.md §9.5 gives the proxy stream a
/// 5 s decay from 4 Hz down to its scored rate precisely so a *briefly*
/// demoted entity re-promotes without a visible stutter; anything that round
/// trips inside a second never got the benefit of that ramp.
const POP_WINDOW_TICKS: u64 = 60;

/// A newly observed committed-cell edge before its wire coverage is attached.
#[derive(Debug, Clone, Copy)]
struct CommittedCellCrossing {
    from: CellId,
    to: CellId,
}

/// One interest-refresh period, the interval over which boundary returns can
/// churn subscriptions and storage keys before the system observes them.
const BOUNDARY_RETURN_WINDOW_TICKS: u64 = TICK_HZ;

/// The rate a briefly demoted entity must stay above for the demotion to be
/// invisible: the midpoint of the 1–4 Hz proxy range.
///
/// Not the *top* of the range — the §9.5 ramp starts at 4 Hz and decays, so
/// demanding 4 Hz throughout would call every demotion longer than an instant a
/// pop, which is a tautology against the ramp rather than a test of it. What a
/// player actually sees is an entity that fell toward the 1 Hz floor and came
/// back; with the ramp, a sub-second round trip never gets near it.
const RAMP_FLOOR_HZ: f32 = 2.5;

/// How one bot is built.
///
/// Named rather than positional because two of these fields are what separate
/// a modified client from an honest one, and a bare `bool, Option<Tamper>` pair
/// at the end of a seven-argument call is how a leg of the gate silently ends
/// up fielding the wrong swarm.
#[derive(Debug, Clone, Copy)]
pub struct BotSpec {
    /// Index in the swarm.
    pub index: usize,
    /// Peers in the swarm.
    pub count: usize,
    /// The universe seed.
    pub seed: UniverseSeed,
    /// Interest-cell edge in metres.
    pub cell_edge_m: f32,
    /// Run the witness pipeline on this peer.
    pub witnessing: bool,
    /// The tamper this peer's **authority** executes with.
    ///
    /// `None` is the shipping build. `Some(_)` is P4's modified client: the
    /// rules that step this bot's own body are the tampered ones, while the
    /// [`Witness`] it runs over its island-mates stays honest. That asymmetry
    /// is deliberate — a tampered witness would re-execute *other* peers under
    /// raised ceilings and accuse the honest cruisers among them, which is a
    /// statement about the cheat rather than about the pipeline. What P4 asks
    /// is whether honest witnesses convict a modified authority.
    pub cheat: Option<Tamper>,
    /// Whether this peer's witness files what it raises, or only counts it.
    ///
    /// The inverse of [`WitnessConfig::shadow_mode`], which defaults to `true`
    /// and short-circuits `raise` — so a run left at the default measures
    /// detection and files nothing, which is P4's posture for the honest legs
    /// and useless for the conviction one.
    pub enforcing: bool,
}

/// One return to the cell committed immediately before the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoundaryReturn {
    tick: u64,
}

/// Sliding one-refresh-period measurement of returns to the same cell.
#[derive(Debug, Default)]
struct BoundaryReturnWindow {
    recent: VecDeque<BoundaryReturn>,
    max_returns: u64,
}

impl BoundaryReturnWindow {
    fn record(&mut self, tick: u64) {
        while self
            .recent
            .front()
            .is_some_and(|sample| tick.saturating_sub(sample.tick) >= BOUNDARY_RETURN_WINDOW_TICKS)
        {
            self.recent.pop_front();
        }
        self.recent.push_back(BoundaryReturn { tick });
        let returns = self.recent.len() as u64;
        self.max_returns = self.max_returns.max(returns);
    }
}

/// A synthetic peer.
pub struct Bot {
    /// This peer's identity.
    pub node: NodeId,
    /// Its index in the swarm, used for routing.
    pub index: usize,
    /// The headless app running the real plugins.
    pub app: App,
    /// The kernel-owned fixed-step host holding this bot's own craft, the
    /// extra authorities it hosts and the replicas it installs, under
    /// whichever build this peer runs — tampered or not.
    ///
    /// A18 S6.c: the host owns the clock, the sealed input buffer with its
    /// provenance, the named prediction set and the delivery-routing hook that
    /// [`Bot::step_core`] used to keep its own copy of. What is left in this
    /// driver is pilot authoring, replication ingest, the router and the
    /// witness log.
    host: SimulationHost<Regolith, BotAdapter>,
    /// A second executor running the *shipping* rules over the same logged
    /// orders, for a bot the harness modified.
    ///
    /// The dual-execution probe, and the only thing that can say *when* a cheat
    /// first changed anything. Without it "detected within one adjudication
    /// window" has no `t = 0`: `ReportFiled` carries no tick, the swarm knows
    /// which build it handed out but not which tick that build first mattered
    /// on, and a cheat that is inert at these parameters — which
    /// `Tamper::SpeedMultiplier` is on an interceptor slot, see
    /// [`BotSpec::cheat`] — would satisfy every conviction clause by producing
    /// byte-identical state and never being reported at all.
    honest_shadow: Option<SimulationHost<Regolith, BotAdapter>>,
    /// Prior-tick cross-entity inputs addressed to this authority, retained in
    /// reliable-stream arrival order for delivered-first composition.
    delivered_inbox: Vec<(PersistId, Order)>,
    /// Prior-tick inputs for additional canonical entities this peer hosts.
    hosted_inbox: BTreeMap<PersistId, Vec<(PersistId, Order)>>,
    /// Additional per-entity authorities held by this transport peer.
    hosted: BTreeSet<PersistId>,
    /// This tick's `(author, recipient, order)` products for the swarm router.
    delivered_outbox: Vec<(PersistId, PersistId, Order)>,
    /// Receipt ticks for canonical replicas installed as recorded neighbours.
    replica_seen_at: BTreeMap<PersistId, u64>,
    /// First authenticated transport identity seen authoring each replica.
    replica_authorities: BTreeMap<PersistId, NodeId>,
    /// Canonical keyframes used by the direct core-observation receive seam.
    replica_keyframes: ReplicaKeyframes,
    /// Target-authored shot verdicts retained only for an enabled measurement.
    resolved_shots: Option<Vec<Outcome>>,
    /// Envelopes routed to this peer but naming another entity.
    foreign_deliveries: u64,
    /// First tick at which the tampered build produced a different state hash
    /// than the shipping one would have from the same orders.
    first_tampered_tick: Option<u64>,
    /// The tamper this bot's authority runs, if any.
    tamper: Option<Tamper>,
    /// The entity this bot authors.
    entity: PersistId,
    /// Universe seed used by the shared pure pilot.
    seed: UniverseSeed,
    /// Scenario slot used by the shared pure pilot.
    slot: u64,
    /// Heading change per tick, in micro-radians — its share of the circle.
    turn_urad: i32,
    /// Thrust magnitude in mm/s².
    accel_mmss: i32,
    /// Cells this bot has committed to, in order of first visit.
    pub visited: Vec<CellId>,
    /// Times the committed cell changed.
    pub crossings: u64,
    /// Times a commitment returned to the cell it had just left (A→B→A).
    ///
    /// This — not the raw crossing count — is what "no entity thrashes cells at
    /// a boundary" means. A bot travelling in a straight line crosses a great
    /// many boundaries and thrashes none; a bot loitering on one boundary
    /// crosses the same one repeatedly, re-keying storage and churning
    /// subscriptions each time. A single return can be a genuine reversal
    /// after crossing the full hysteresis deadband, so the criterion judges
    /// the one-refresh-period maximum rather than this run-wide total.
    pub boundary_flips: u64,
    /// Most returns to one cell observed in any interest-refresh period.
    pub max_boundary_returns_in_window: u64,
    /// Recent returns used to measure the sliding refresh-period maximum.
    boundary_return_window: BoundaryReturnWindow,
    /// The cell committed before the current one.
    previous_cell: Option<CellId>,
    /// The cell committed now.
    current_cell: Option<CellId>,
    /// Times an entity entered or left the high-rate set.
    pub interest_churn: u64,
    /// Times an entity left the high-rate set and returned inside
    /// [`POP_WINDOW_TICKS`] — a *visible proxy pop*.
    ///
    /// Churn is expected and fine: a crowd moving past you constantly changes
    /// who your nearest two dozen entities are. What the criterion forbids is
    /// the *flap* — an entity demoted to a 1–4 Hz proxy and re-promoted a
    /// moment later, which a player sees as a stutter because the proxy's
    /// extrapolation and interpolation buffer are reset each time
    /// (docs/03-replication.md §9.5).
    pub proxy_pops: u64,
    /// Previous high-rate set, for churn accounting.
    last_high_rate: Vec<Entity>,
    /// Per entity demoted out of the high-rate set: when, and the lowest proxy
    /// rate it has been given since.
    demoted_at: Vec<(Entity, u64, f32)>,
    /// Ticks elapsed, for the pop window.
    tick: u64,
    /// Whether this seat emits the opt-in #699 receive/decision trace.
    ///
    /// Selected by `P1_SWARM_AUDIENCE_TRACE_SEAT=<stable seat>`. The probe is
    /// receiver-side only: it observes decoded replicas, delivered inputs and
    /// cell commitments without changing manifests, audiences or send order.
    audience_trace: bool,
    /// Opt-in copies of the exact audience diffs that feed the join rule.
    audience_changes: Option<Vec<AudienceChange>>,
    /// This bot's behavioural profile.
    pub profile: Profile,
    /// The signed log this bot authors, when witnessing is on.
    pub chain: Option<InputLogProducer>,
    /// What the last [`Self::step_core`] call stepped and emitted.
    ///
    /// Retained only for A18 S6.c's convergence detector
    /// (`the_converged_bot_tick_reproduces_the_hand_rolled_loop`), which has
    /// to compare the emitted event vector *and its order* against the loop
    /// the seam replaced. Nothing in the harness reads it.
    #[cfg(test)]
    last_step: SteppedTick,
    /// Witness signals raised against island-mates, by kind.
    pub signals: SignalTally,
    /// The peers the harness modified, so a signal against one of them is not
    /// counted as an accusation against an honest peer.
    ///
    /// Installed by the swarm, which is the only thing that knows: the bot
    /// itself must not be able to tell, or the false-positive count would be
    /// measuring the oracle instead of the witness.
    tampered_subjects: Vec<NodeId>,
    /// A19's opt-in observation of canonical changes at the existing send
    /// seam. It never influences the payload or recipient set.
    delta_stats: Option<DeltaStats>,
    /// Last authored keyframe and its byte-identical cached wire form.
    sender_keyframes: BTreeMap<PersistId, SenderKeyframe>,
    /// Previous anchors held until the newly built keyframe leaves the app.
    sender_keyframe_rollbacks: BTreeMap<PersistId, Option<SenderKeyframe>>,
    /// Audience seen on the preceding send, per authored entity.
    replication_audiences: BTreeMap<PersistId, BTreeSet<NodeId>>,
    /// Previous audiences held until the current send leaves the app.
    replication_audience_rollbacks: BTreeMap<PersistId, Option<BTreeSet<NodeId>>>,
    /// Number of state-send opportunities this authority has processed.
    replication_send_index: u64,
    /// State sends per 1 Hz keyframe interval.
    keyframe_every_sends: u64,
    /// Offered keyframe/delta traffic, split beside the shipping lane meter.
    replication_wire: ReplicationWireCounters,
}

/// One absolute anchor retained by a sender.
#[derive(Debug, Clone)]
struct SenderKeyframe {
    canonical: Vec<u8>,
    cell: CellId,
    tick: u64,
    payload: Vec<u8>,
}

/// One absolute anchor retained by a receiver.
#[derive(Debug, Clone)]
struct ReceiverKeyframe {
    canonical: Vec<u8>,
    cell: CellId,
    tick: u64,
}

/// Per-entity receiver anchors for keyframe-referenced deltas.
#[derive(Resource, Debug, Default, Clone)]
pub struct ReplicaKeyframes {
    anchors: BTreeMap<PersistId, ReceiverKeyframe>,
    /// Entities whose receiver holds an older anchor than an arrived delta
    /// names. A subsequent keyframe for the entity closes the window.
    missing_newer: BTreeSet<PersistId>,
}

/// One sender-side audience diff, captured without changing the join rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudienceChange {
    /// Entity whose audience changed.
    pub(crate) entity: PersistId,
    /// Newly interested recipients.
    pub(crate) joins: u64,
    /// Recipients no longer in the audience.
    pub(crate) leaves: u64,
    /// Half-open simulated one-second window containing the diff.
    pub(crate) window: u64,
}

/// A reconstructed absolute replication state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedReplica {
    pub(crate) canonical: Vec<u8>,
    pub(crate) cell: CellId,
    pub(crate) entity: PersistId,
    pub(crate) tick: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplicaDecodeError {
    NotReplication,
    UnanchoredDelta(UnanchoredDeltaCause),
    BadPatch,
}

/// Why a delta's exact sender-clocked anchor was unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnanchoredDeltaCause {
    /// No keyframe for this entity has ever reached the receiver.
    NoKeyframe,
    /// The receiver has an older anchor, so the referenced keyframe never arrived.
    MissingNewerKeyframe,
    /// A newer keyframe arrived first and replaced the referenced one.
    SupersededKeyframe,
    /// The delta's age points before tick zero.
    InvalidReference,
}

/// Keyframe/delta traffic offered to the shipping send path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplicationWireCounters {
    /// Absolute keyframe messages, counted once per recipient.
    pub keyframe_messages: u64,
    /// Delta messages, counted once per recipient.
    pub delta_messages: u64,
    /// Keyframe wire bytes including datagram overhead.
    pub keyframe_bytes: u64,
    /// Delta wire bytes including datagram overhead.
    pub delta_bytes: u64,
    /// Keyframe messages built and charged, then discarded by a simulated hitch.
    pub keyframes_discarded_while_stalled: u64,
    /// Delta messages built and charged, then discarded by a simulated hitch.
    pub deltas_discarded_while_stalled: u64,
}

impl ReplicationWireCounters {
    fn record(&mut self, kind: ReplicationKind, payload_len: usize) {
        // `send_peer_packets` adds the outer channel tag before charging the
        // datagram. Mirror that exact one-byte framing here.
        let wire = datagram_wire_bytes(payload_len.saturating_add(1));
        match kind {
            ReplicationKind::Keyframe => {
                self.keyframe_messages += 1;
                self.keyframe_bytes += wire;
            }
            ReplicationKind::Delta => {
                self.delta_messages += 1;
                self.delta_bytes += wire;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplicationKind {
    Keyframe,
    Delta,
}

/// Witness signals a peer raised, by kind and by whether the accused was one of
/// the harness's own modified clients.
///
/// Every bot in a run without `--cheat` is honest, so anything here except a gap
/// is a **false positive** — that is what makes the count meaningful without a
/// separate oracle for who was cheating. Gaps are counted separately because a
/// gap is a question, not an accusation: it is the expected answer to a dropped
/// datagram.
///
/// With `--cheat` the swarm *has* an oracle — it handed out the tampered build —
/// so the counters below split on it. The four that feed
/// [`false_positives`](Self::false_positives) keep their original meaning:
/// signals against a peer that did nothing wrong. Signals against a modified
/// client are the finding the run exists to produce and are counted apart.
#[derive(Debug, Default, Clone, Copy)]
pub struct SignalTally {
    /// Chain gaps detected — repairs requested, not accusations.
    pub gaps: u64,
    /// Stage-1 invariant breaches against an honest peer. A false positive.
    pub invariant_breaches: u64,
    /// Re-execution disagreeing with an honest peer's signed claim. A false
    /// positive.
    pub claim_mismatches: u64,
    /// Reports **filed** against an honest peer. A false positive.
    ///
    /// Counted from `Messages<ReportFiled>` rather than from `Witnessed`. The
    /// adapter's `route` intercepts `WitnessSignal::Report` and files it before
    /// the signal is written out (`orrery_witness::plugin`), so the `Witnessed`
    /// arm for it can never fire and this counter was structurally zero for as
    /// long as it existed.
    pub reports: u64,
    /// Honest subjects reported as stalled — a hole never filled.
    ///
    /// A false positive in this swarm: every bot answers repairs, so a stall
    /// means the repair path could not keep up, not that a peer refused.
    pub stalled: u64,
    /// Non-gap signals raised against a peer the harness modified. Findings,
    /// not false positives.
    pub signals_against_tampered: u64,
    /// Reports filed against a peer the harness modified.
    pub reports_against_tampered: u64,
}

impl SignalTally {
    /// Signals that would accuse an honest peer.
    #[must_use]
    pub fn false_positives(&self) -> u64 {
        self.invariant_breaches + self.claim_mismatches + self.reports + self.stalled
    }
}

/// How long a replica survives without an update, in ticks.
///
/// Two simulated seconds — twice the 1 Hz proxy floor, so an entity streaming at
/// the slowest legal rate is never mistaken for one that has gone.
///
/// A client drops an entity when it leaves interest; replicon does it through
/// `ScopeLifetime::WhileVisible`. Without an equivalent here a replica keeps the
/// last cell it was seen in *forever*, so a peer that walked out of the
/// neighbourhood stays tagged — and "a late joiner receives only its 27-cell
/// neighborhood" fails against ghosts rather than against anything real.
const REPLICA_TTL_TICKS: u64 = 120;

/// The ECS entity mirroring another peer's replicated body, keyed by the
/// canonical entity that body belongs to.
///
/// Without these the interest selector has nothing to rank: `update_interest_set`
/// queries the non-local entities with a position, and a harness that never
/// spawned any would report zero churn, never reach the 24-entity cap, and
/// certify an interest set it had not exercised.
///
/// The key is the *entity*, never the transport identity that carried it. A
/// transport identity is not an entity identity (D2): one peer may hold several
/// single-writer authorities, and campaign rocks are exactly that shape (see
/// [`Bot::host_entity`]). Keying by `PersistId` is what lets a peer's craft and
/// the rocks it hosts be separate bodies for the selector, rather than one row
/// whose position is whichever packet happened to arrive last.
///
/// This is the same shape the shipping client uses — `clients/regolith`'s
/// downlink installs by `PersistId` and filters on no variant at all — and the
/// same shape [`Bot::receive_inbound`] already uses for the executor half of
/// this receiver, where the streaming peer is tracked separately in
/// `replica_authorities` because that is the copy authority matching reads.
#[derive(Component)]
pub struct Replica(pub PersistId);

/// The tick a replica was last refreshed, for [`REPLICA_TTL_TICKS`].
#[derive(Component)]
pub struct LastSeen(pub u64);

/// The harness's simulated tick, so the replica systems can age entries.
#[derive(Resource, Default, Clone, Copy)]
pub struct SimTick(pub u64);

/// Despawns replicas whose authority has stopped streaming to this peer.
pub fn expire_replicas(
    mut commands: Commands,
    tick: Res<SimTick>,
    replicas: Query<(Entity, &LastSeen)>,
) {
    for (entity, seen) in &replicas {
        if tick.0.saturating_sub(seen.0) > REPLICA_TTL_TICKS {
            commands.entity(entity).despawn();
        }
    }
}

/// The cell edge the replica system converts positions against.
#[derive(Resource, Clone, Copy)]
pub struct CellEdge(pub f32);

/// Where inbound state packets went, so a silent decode failure cannot make
/// every interest clause pass by holding zero replicas.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ReplicaCounters {
    /// Packets seen by the replica system.
    pub seen: u64,
    /// Packets on a channel this system ignores.
    pub wrong_channel: u64,
    /// Packets whose datagram envelope did not decode.
    pub undecodable: u64,
    /// Packets whose body did not decode.
    pub bad_body: u64,
    /// Deltas whose referenced keyframe has not arrived on this link.
    pub deltas_unanchored: u64,
    /// Unanchored deltas seen before any keyframe for their entity.
    pub deltas_without_any_keyframe: u64,
    /// Unanchored deltas referring to a newer keyframe than the one retained.
    pub deltas_missing_newer_keyframe: u64,
    /// Unanchored deltas referring to an older, already-superseded keyframe.
    pub deltas_with_superseded_keyframe: u64,
    /// Unanchored deltas whose age underflowed their tick.
    pub deltas_with_invalid_reference: u64,
    /// Replicas spawned.
    pub spawned: u64,
    /// Replicas updated.
    pub updated: u64,
}

/// Mutable receiver state consumed together by [`apply_replicas`].
#[derive(SystemParam)]
pub struct ReplicaReceiveState<'w> {
    counters: ResMut<'w, ReplicaCounters>,
    keyframes: ResMut<'w, ReplicaKeyframes>,
}

pub(crate) fn decode_replica(
    payload: &[u8],
    keyframes: &mut ReplicaKeyframes,
) -> Result<DecodedReplica, ReplicaDecodeError> {
    if let Some((canonical, cell, entity, tick)) =
        orrery_net::channels::decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(payload)
    {
        keyframes.anchors.insert(
            entity,
            ReceiverKeyframe {
                canonical: canonical.clone(),
                cell,
                tick,
            },
        );
        keyframes.missing_newer.remove(&entity);
        return Ok(DecodedReplica {
            canonical,
            cell,
            entity,
            tick,
        });
    }

    let Some(delta) = decode_replication_delta(payload) else {
        return Err(ReplicaDecodeError::NotReplication);
    };
    let referenced_tick = delta
        .tick
        .checked_sub(u64::from(delta.keyframe_age))
        .ok_or(ReplicaDecodeError::UnanchoredDelta(
            UnanchoredDeltaCause::InvalidReference,
        ))?;
    let anchor =
        keyframes
            .anchors
            .get(&delta.entity)
            .ok_or(ReplicaDecodeError::UnanchoredDelta(
                UnanchoredDeltaCause::NoKeyframe,
            ))?;
    if anchor.tick < referenced_tick {
        keyframes.missing_newer.insert(delta.entity);
        return Err(ReplicaDecodeError::UnanchoredDelta(
            UnanchoredDeltaCause::MissingNewerKeyframe,
        ));
    }
    if anchor.tick > referenced_tick {
        return Err(ReplicaDecodeError::UnanchoredDelta(
            UnanchoredDeltaCause::SupersededKeyframe,
        ));
    }
    let canonical =
        apply_delta_patch(&anchor.canonical, &delta.patch).ok_or(ReplicaDecodeError::BadPatch)?;
    Ok(DecodedReplica {
        canonical,
        cell: delta.cell.unwrap_or(anchor.cell),
        entity: delta.entity,
        tick: delta.tick,
    })
}

/// Upserts a replica entity per replicated body from the state packets that arrive.
///
/// This is the receiving half of replication, reduced to what the criterion
/// measures: which entities a peer is tracking and where they are. Whether the
/// bytes arrived as a delta or a snapshot is `orrery_predict`'s concern; the
/// interest selector only needs a position and a cell.
///
/// One replica per *body*, not per peer (#961). `docs/03-replication.md` §4.1
/// scores "every replicable entity in its 27-cell AOI", and §9.3's remedy for
/// too many of them is to shed by relevance class or demote to a proxy — never
/// for the receiver to refuse the body. A peer keyed model could not represent
/// the multi-authority shape D2 permits and `Bot::host_entity` relies on, and
/// discarding the bodies it could not represent is what made this receiver
/// charge well-formed campaign rocks to `bad_body` — a counter whose contract
/// is bytes the receiver's own codec refuses.
pub fn apply_replicas(
    mut packets: MessageReader<PeerPacket>,
    mut commands: Commands,
    edge: Res<CellEdge>,
    tick: Res<SimTick>,
    mut receive: ReplicaReceiveState,
    existing: Query<(Entity, &Replica)>,
    witness: Option<ResMut<WitnessState<Regolith>>>,
) {
    let mut witness = witness;
    for packet in packets.read() {
        receive.counters.seen += 1;
        if packet.channel != orrery_net::channels::Channel::State {
            receive.counters.wrong_channel += 1;
            continue;
        }
        // Witness records share this lane and are sub-tagged as such; skipping
        // them is not a decode failure. Counting them would make `undecodable`
        // — the guard that catches the harness measuring an empty world — fire
        // constantly and stop meaning anything.
        let decoded = match decode_replica(&packet.payload, &mut receive.keyframes) {
            Ok(decoded) => decoded,
            Err(ReplicaDecodeError::UnanchoredDelta(cause)) => {
                receive.counters.deltas_unanchored += 1;
                if std::env::var_os("P1_SWARM_TRACE_UNANCHORED").is_some() {
                    if let Some(delta) = decode_replication_delta(&packet.payload) {
                        let referenced_tick = delta.tick.checked_sub(u64::from(delta.keyframe_age));
                        let retained_tick = receive
                            .keyframes
                            .anchors
                            .get(&delta.entity)
                            .map(|anchor| anchor.tick);
                        eprintln!(
                            "unanchored entity={} delta_tick={} referenced_tick={referenced_tick:?} retained_tick={retained_tick:?} cause={cause:?}",
                            delta.entity.0,
                            delta.tick,
                        );
                    }
                }
                match cause {
                    UnanchoredDeltaCause::NoKeyframe => {
                        receive.counters.deltas_without_any_keyframe += 1;
                    }
                    UnanchoredDeltaCause::MissingNewerKeyframe => {
                        receive.counters.deltas_missing_newer_keyframe += 1;
                    }
                    UnanchoredDeltaCause::SupersededKeyframe => {
                        receive.counters.deltas_with_superseded_keyframe += 1;
                    }
                    UnanchoredDeltaCause::InvalidReference => {
                        receive.counters.deltas_with_invalid_reference += 1;
                    }
                }
                continue;
            }
            Err(ReplicaDecodeError::BadPatch) => {
                receive.counters.bad_body += 1;
                continue;
            }
            Err(ReplicaDecodeError::NotReplication) => {
                if orrery_net::channels::decode_witness::<orrery_protocol::WitnessMsg>(
                    &packet.payload,
                )
                .is_none()
                {
                    receive.counters.undecodable += 1;
                }
                continue;
            }
        };
        let Ok(state) = <RegolithState as orrery_core::CoreCodec>::decode(&decoded.canonical)
        else {
            receive.counters.bad_body += 1;
            continue;
        };
        let entity = decoded.entity;
        // Stage 1, on everything received, witnessed or not (docs/06 §3) — and
        // the sample store a blind watch re-anchors from. The state is the
        // authority's own, stamped with the tick it belongs to, so a claim's
        // `state_hash` either matches it or does not; nothing here is inferred
        // from the receiver's clock.
        if let Some(witness) = witness.as_mut() {
            witness.0.observe(orrery_witness::Observation {
                entity,
                state: &state,
                tick: orrery_protocol::Tick::new(decoded.tick),
            });
        }

        // A scheduler occupies no point in the lattice, so it is not a spatial
        // replica and the selector has nothing to rank it by. Not a fault: the
        // body decoded, and the shipping client's own AOI code returns `None`
        // for the same variant (`clients/regolith/src/aoi.rs`).
        let Some(pos) = body_position(&state) else {
            continue;
        };
        // Position is for distance ranking; the *cell* is the authority's own
        // committed value, never recomputed here (D2).
        let grid = grid_of(&pos, edge.0);
        match existing.iter().find(|(_, replica)| replica.0 == entity) {
            Some((mirror, _)) => {
                receive.counters.updated += 1;
                commands.entity(mirror).insert((
                    Cell(decoded.cell),
                    GridPosition(grid),
                    LastSeen(tick.0),
                ));
            }
            None => {
                receive.counters.spawned += 1;
                commands.spawn((
                    Replica(entity),
                    Cell(decoded.cell),
                    GridPosition(grid),
                    LastSeen(tick.0),
                ));
            }
        }
    }
}

/// The deterministic crowd pose for swarm slot `index` of `count`.
///
/// Extracted from `Bot::new` so the host can know where an external peer will
/// spawn — and therefore which cell to put in every bot's roster — without
/// building a `Bot` to find out. Both sides of the #385 join derive this from
/// the slot number alone; neither tells the other.
///
/// The swarm is a *crowd*, not a scatter: every bot rides the same large
/// circle, spread over a short arc of it. Spacing that keeps neighbours one or
/// two cells apart is what makes interest sets overlap, churn as the crowd
/// moves, and actually hit the 24-entity cap with 32 peers. A scatter would
/// put every bot alone in its neighbourhood, and every clause about interest
/// would pass by being vacuous.
///
/// Returns the start position and a yaw tangent to the ring, so the bot
/// travels around it. `Craft::spawned` takes the yaw back into `[0, TAU)`,
/// which `regolith/value-range` requires of every sample.
#[must_use]
pub fn spawn_pose(index: usize, count: usize) -> (QPos, i32) {
    orrery_games::regolith::campaign_spawn_pose(index, count)
}

/// The orbit radius `spawn_pose`'s slot rides, for the turn-rate that keeps a
/// bot on it.
#[must_use]
pub fn orbit_radius_m(index: usize, count: usize) -> f64 {
    orrery_games::regolith::campaign_orbit_radius_m(index, count)
}

impl Bot {
    /// Build a bot from `spec`, spread around a ring with its swarm.
    pub fn new(spec: BotSpec) -> Self {
        let BotSpec {
            index,
            count,
            seed,
            cell_edge_m,
            witnessing,
            cheat,
            enforcing,
        } = spec;
        let secret = bot_key(index);
        let node = secret.public();
        let entity = PersistId::new(index as u64 + 1);
        // The crowd pose is slot-derived and shared with the host's view of an
        // external peer; see `spawn_pose`.
        let (start, yaw_urad) = spawn_pose(index, count);

        // **The cheat is inert on an interceptor, and that is the whole reason
        // this is not `Archetype::for_slot`.** `Tamper::SpeedMultiplier` raises
        // the archetype's ceilings by 1.5×, and the roam below asks for
        // `accel_mmss` 60 000 — exactly the interceptor's `max_accel_mmss`, so
        // `clamp(0, 60_000)` and `clamp(0, 90_000)` return the same number and
        // the tampered build produces byte-identical state. The speed ceiling
        // does not bind either: both archetypes cruise far under it, held there
        // by the profile rather than by the clamp. On a cruiser the same
        // request clamps to 20 000 honestly and to 30 000 tampered, so the
        // divergence is 167 mm/s of velocity per thrusting tick — sixteen D16
        // velocity bands, and unmistakable in a state hash. A modified client
        // pinned to the other slot would satisfy every conviction clause in
        // this harness by never diverging at all.
        let archetype = if cheat.is_some() {
            Archetype::Cruiser
        } else {
            Archetype::for_slot(index as u64)
        };
        let mut craft = Craft::spawned(archetype, start, yaw_urad);
        if cheat == Some(Tamper::DamageInflation) {
            // The tamper-parity scenario starts after a logged pickup grant:
            // both the modified authority and its honest shadow anchor from
            // the same Volley state. One held trigger then exposes three
            // independently inflated rolls, without coupling the pilot to hit
            // resolution (the seam #352 will replace).
            craft.weapon = WeaponKind::Volley;
        }
        let state = RegolithState::Craft(craft);

        let rules = cheat.map_or_else(Regolith::honest, Regolith::cheating);
        let mut host = new_host(rules, seed, Tick::new(0));
        host.install_state(entity, state.clone());
        let honest_shadow = cheat.map(|_| {
            let mut shadow = new_host(Regolith::honest(), seed, Tick::new(0));
            shadow.install_state(entity, state);
            shadow
        });
        let mut app = App::new();
        app.add_plugins(OrrerySpatialPlugin {
            config: SpatialConfig {
                cell_edge_m,
                ..SpatialConfig::default()
            },
        })
        // The send path and its budget, without the iroh endpoint: the router
        // stands in for the IO layer, and everything above it is shipping code.
        .init_resource::<PeerLinkCounters>()
        .init_resource::<UploadBudget>()
        .init_resource::<UploadMeter>()
        .init_resource::<IslandMembership>()
        .insert_resource(Time::<Real>::default())
        .insert_resource(CellEdge(cell_edge_m))
        .init_resource::<ReplicaCounters>()
        .init_resource::<ReplicaKeyframes>()
        .init_resource::<SimTick>()
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .add_systems(
            First,
            // Replicas land before the spatial stack runs, so the interest set
            // this frame reflects what arrived this frame rather than lagging
            // a tick behind the world.
            (receive_peer_packets, apply_replicas, expire_replicas).chain(),
        )
        .add_systems(Update, (send_peer_packets, forget_departed_links).chain());

        if witnessing {
            // The witness adapter drains the same peer lane, so it slots in
            // beside the send path rather than replacing anything.
            app.add_plugins(WitnessPlugin::<Regolith>::new())
                .insert_resource(WitnessState(Witness::<Regolith>::new(
                    WitnessConfig {
                        shadow_mode: !enforcing,
                        ..WitnessConfig::default()
                    },
                    seed,
                    // The **shipping** rules, on every peer including a
                    // modified one — see `BotSpec::cheat`. A witness re-executes
                    // the rules an authority *claims* to be running, and the
                    // claim is what it is held to.
                    Regolith::honest,
                )))
                // Filing is opt-in and this is the opt-in: without an identity
                // `escalate` counts `escalations_unidentified` and stops, which
                // is what the harness did for as long as it had no adjudicator
                // to hand a report to. The peer's own transport key, because a
                // report binds an accusation to an account and this peer has
                // exactly one.
                .insert_resource(WitnessIdentity(secret.clone()));
        }

        let start_grid = grid_of(&start, cell_edge_m);
        app.world_mut().spawn((
            LocalPlayer,
            Cell(cell_of(start_grid)),
            GridPosition(start_grid),
        ));

        Self {
            node,
            index,
            app,
            host,
            honest_shadow,
            delivered_inbox: Vec::new(),
            hosted_inbox: BTreeMap::new(),
            hosted: BTreeSet::new(),
            delivered_outbox: Vec::new(),
            replica_seen_at: BTreeMap::new(),
            replica_authorities: BTreeMap::new(),
            replica_keyframes: ReplicaKeyframes::default(),
            resolved_shots: None,
            foreign_deliveries: 0,
            first_tampered_tick: None,
            tamper: cheat,
            entity,
            seed,
            slot: index as u64,
            // ω = v/r, so the bot actually follows the orbit it started on.
            // Picking a turn rate independently of the speed makes it spiral
            // into whatever radius the two happen to imply — which is how the
            // first version ended up circling 200 m and visiting ten cells.
            //
            // `v` is `CRUISE_MPS` rather than a measured speed, and under
            // Regolith that is an approximation rather than an identity: these
            // rules apply drag, so a bot coasts *down* through the cutoff and
            // thrusts back over it instead of sitting at it exactly. The
            // sawtooth is one thrust tick's worth wide — under 1 m/s on a
            // cruiser — so the orbit it actually follows is within a few
            // percent of the one it started on, which is what this needs to be
            // right about.
            turn_urad: ((CRUISE_MPS / orbit_radius_m(index, count)) / TICK_HZ as f64 * 1_000_000.0)
                as i32,
            accel_mmss: 60_000,
            visited: vec![cell_of(start_grid)],
            crossings: 0,
            boundary_flips: 0,
            max_boundary_returns_in_window: 0,
            boundary_return_window: BoundaryReturnWindow::default(),
            previous_cell: None,
            current_cell: Some(cell_of(start_grid)),
            interest_churn: 0,
            proxy_pops: 0,
            profile: Profile::for_index(index, witnessing),
            // The *honest* ruleset id, even on a tampered build. That is the
            // point of `Regolith::id` reporting `REGOLITH_RULESET` whatever the
            // tamper: a modified client claims to be running the rules, and the
            // claim is what a witness holds it to. A cheat that announced
            // itself would be routed to no adjudicable build and resolve as
            // `UnknownRuleset` — never a strike.
            chain: witnessing.then(|| {
                InputLogProducer::new(
                    secret.clone(),
                    entity,
                    REGOLITH_RULESET,
                    0,
                    CLAIM_EVERY,
                    FRAME_TICKS,
                )
            }),
            #[cfg(test)]
            last_step: SteppedTick::default(),
            signals: SignalTally::default(),
            tampered_subjects: Vec::new(),
            delta_stats: None,
            sender_keyframes: BTreeMap::new(),
            sender_keyframe_rollbacks: BTreeMap::new(),
            replication_audiences: BTreeMap::new(),
            replication_audience_rollbacks: BTreeMap::new(),
            replication_send_index: 0,
            keyframe_every_sends: DEFAULT_KEYFRAME_EVERY_SENDS,
            replication_wire: ReplicationWireCounters::default(),
            last_high_rate: Vec::new(),
            demoted_at: Vec::new(),
            tick: 0,
            audience_trace: std::env::var("P1_SWARM_AUDIENCE_TRACE_SEAT")
                .ok()
                .and_then(|seat| seat.parse::<usize>().ok())
                == Some(index),
            audience_changes: None,
        }
    }

    /// Build a peer with a current craft snapshot but no receive history.
    ///
    /// The late-join fixture uses this for both sides of its isolated exchange:
    /// authorities retain their current simulated pose, while the arriving
    /// peer starts with an empty Bevy world and replica store. Constructing a
    /// new [`Bot`] here is the load-bearing part of that proof; copying a
    /// long-lived bot would also copy replicas that arrived before the join.
    pub(crate) fn from_craft_snapshot(spec: BotSpec, craft: Craft, committed_cell: CellId) -> Self {
        let cell_edge_m = spec.cell_edge_m;
        let mut bot = Self::new(spec);
        let grid = grid_of(&craft.pos, cell_edge_m);
        bot.host
            .install_state(bot.entity, RegolithState::Craft(craft.clone()));
        if let Some(shadow) = &mut bot.honest_shadow {
            shadow.install_state(bot.entity, RegolithState::Craft(craft));
        }
        let world = bot.app.world_mut();
        let mut query = world.query_filtered::<(&mut GridPosition, &mut Cell), With<LocalPlayer>>();
        let (mut position, mut committed) = query
            .single_mut(world)
            .expect("a bot has exactly one local player");
        position.0 = grid;
        committed.0 = committed_cell;
        bot.current_cell = Some(committed_cell);
        bot.previous_cell = None;
        bot.max_boundary_returns_in_window = 0;
        bot.boundary_return_window = BoundaryReturnWindow::default();
        bot.visited = vec![committed_cell];
        bot
    }

    /// This bot's authored craft.
    #[must_use]
    pub fn craft(&self) -> &Craft {
        let RegolithState::Craft(craft) = self.host.backend().state(self.entity).expect("seeded")
        else {
            unreachable!("a swarm bot always authors a craft")
        };
        craft
    }

    /// Install another canonical entity under this peer's authority.
    ///
    /// A transport identity is not an entity identity: D2 permits one peer to
    /// hold several single-writer authorities. Campaign rocks exercise that
    /// ordinary shape instead of pretending every world entity is a player.
    pub fn host_entity(&mut self, entity: PersistId, state: RegolithState) {
        assert_ne!(
            entity, self.entity,
            "the player authority is already installed"
        );
        assert!(
            !self.hosted.contains(&entity),
            "one peer cannot install an authority twice"
        );
        self.host.install_state(entity, state.clone());
        if let Some(shadow) = &mut self.honest_shadow {
            shadow.install_state(entity, state);
        }
        // The current producer cannot cut an empty-input frame. Autonomous
        // rocks would therefore accumulate tick hashes until their first mined
        // input, then publish them under one ten-tick frame. Do not manufacture
        // invalid evidence. These entities remain canonical and replicated,
        // but are not independently witnessed until the craft-shaped stage-1
        // adapter and producer gain a multi-entity/dynamic-anchor seam.
        self.hosted.insert(entity);
        self.hosted_inbox.insert(entity, Vec::new());
    }

    /// Whether this peer currently holds the named entity's authority.
    #[must_use]
    pub fn authors(&self, entity: PersistId) -> bool {
        entity == self.entity || self.hosted.contains(&entity)
    }

    /// Every entity this peer currently authors, in canonical id order after
    /// the player craft.
    #[must_use]
    pub fn authored_entities(&self) -> Vec<PersistId> {
        core::iter::once(self.entity)
            .chain(self.hosted.iter().copied())
            .collect()
    }

    /// Enable A19's changed-byte observation at this run's send cadence.
    pub fn enable_delta_stats(&mut self, send_hz: u64) {
        self.delta_stats = Some(DeltaStats::new(send_hz));
    }

    /// Set the sender-clocked keyframe interval in state-send opportunities.
    pub fn set_keyframe_every_sends(&mut self, sends: u64) {
        self.keyframe_every_sends = sends.max(1);
    }

    /// This authority's completed changed-byte observations.
    #[must_use]
    pub fn delta_stats(&self) -> Option<&DeltaStats> {
        self.delta_stats.as_ref()
    }

    /// Enable target-authored shot-verdict capture at the core step seam.
    pub fn enable_resolved_shot_capture(&mut self) {
        self.resolved_shots = Some(Vec::new());
    }

    /// The bot's current exact quantized speed in millimetres per second.
    #[must_use]
    pub fn speed_mms(&self) -> u64 {
        let velocity = self.craft().vel;
        let squared = u128::from(velocity.x.unsigned_abs()).pow(2)
            + u128::from(velocity.y.unsigned_abs()).pow(2)
            + u128::from(velocity.z.unsigned_abs()).pow(2);
        u64::try_from(squared.isqrt()).unwrap_or(u64::MAX)
    }

    /// The bot's current speed in metres per second.
    #[must_use]
    pub fn speed_mps(&self) -> f64 {
        let (vx, vy, vz) = self.craft().vel.to_metres_per_sec();
        libm::sqrt(vx * vx + vy * vy + vz * vz)
    }

    /// Advance the core by one tick and mirror the result into the ECS.
    ///
    /// Thrust cuts out at [`CRUISE_MPS`], which is far below either archetype's
    /// ceiling. Letting the rules' own speed clamp hold the bots instead would
    /// pin an interceptor at 120 m/s — a 128 m cell every other tick, which is
    /// teleporting rather than roaming and would make every hysteresis and
    /// interest-churn number meaningless.
    ///
    /// A modified build steps a second, honest executor over the same order, so
    /// the harness knows the first tick on which the cheat actually changed
    /// anything — see [`Bot::first_tampered_tick`].
    pub fn step_core(&mut self, tick: u64, cell_edge_m: f32) {
        let at = Tick::new(tick);
        // The host owns the clock; this driver is handed the tick by its
        // caller. Seating keeps the two the same value without giving the
        // driver a second opinion about what tick it is: a host that has never
        // stepped is entirely described by its population, so re-seating is
        // rebuilding it at `at` and re-installing what it held, stamp for
        // stamp. In a swarm run this happens once, at tick zero.
        self.seat_hosts_at(at);
        let stale: Vec<_> = self
            .replica_seen_at
            .iter()
            .filter_map(|(entity, seen)| {
                (tick.saturating_sub(*seen) > REPLICA_TTL_TICKS).then_some(*entity)
            })
            .collect();
        for entity in stale {
            self.replica_seen_at.remove(&entity);
            self.replica_authorities.remove(&entity);
            self.host.remove_state(entity);
            if let Some(shadow) = &mut self.honest_shadow {
                shadow.remove_state(entity);
            }
        }
        // Freeze every authored inbox at the tick boundary. An event emitted by
        // the player authority below is delivered to a hosted rock on the next
        // tick, even though both happen to share one process and host.
        let hosted_at_boundary: Vec<PersistId> = self.hosted.iter().copied().collect();
        let hosted_frozen: BTreeMap<PersistId, Vec<(PersistId, Order)>> = hosted_at_boundary
            .iter()
            .map(|entity| {
                (
                    *entity,
                    self.hosted_inbox.remove(entity).unwrap_or_default(),
                )
            })
            .collect();
        let mut rng = tick_rng(self.seed, self.entity, at);
        // D46 clause (d): deliveries from prior ticks precede this tick's
        // pilot orders. Submission order is what the host seals, so this
        // vector is composed here and queued in exactly this order below.
        let delivered = core::mem::take(&mut self.delivered_inbox);
        let traced_delivered = self.audience_trace.then(|| delivered.clone());
        let mut orders: Vec<_> = delivered.iter().map(|(_, order)| order.clone()).collect();
        if std::env::var_os("ORRERY_GEOMETRY_CAPTURE").is_some() {
            self.capture_adjudication_geometry(tick, &orders);
        }
        orders.reserve(4);
        // Where this bot's own pilot orders begin. `orders` already holds this
        // tick's *delivered* inbound orders (D46 clause (d) puts them first),
        // so `orders.first_mut()` is only the pilot's Thrust when the inbox
        // happened to be empty -- and the adaptation below must patch the
        // pilot's order, never a neighbour's.
        let authored_start = orders.len();
        orrery_games::regolith::pilot::honest_orders(
            self.entity,
            self.slot,
            at,
            &mut rng,
            &mut orders,
        );
        let collision_candidate = self.host.backend().state(self.entity).and_then(|own| {
            collision_candidate(
                self.entity,
                own,
                self.host
                    .backend()
                    .entities()
                    .filter(|candidate| **candidate != self.entity)
                    .filter_map(|candidate| {
                        self.host
                            .backend()
                            .state(*candidate)
                            .map(|state| (*candidate, state))
                    }),
            )
        });
        if let Some(other) = collision_candidate {
            if self.audience_trace {
                eprintln!(
                    "audience_trace kind=collision_candidate seat={} tick={tick} entity={} other={}",
                    self.index, self.entity.0, other.0,
                );
            }
            // Detection is an untrusted input-source concern. The ruleset reads
            // the installed snapshot as a recorded frame and alone decides
            // whether this nomination can apply either body's force.
            orders.push(Order::Collide { other });
        }
        let speed = self.speed_mps();
        let profile = self.profile;
        let turn_urad = self.turn_urad;
        let full_accel = self.accel_mmss;
        let speed_probe = self.tamper == Some(Tamper::SpeedMultiplier);
        // Input-source adaptation applies to the pilot's own Thrust orders --
        // never to delivered neighbour orders. The shared pilot currently
        // emits exactly one Thrust, but walking the authored suffix keeps the
        // boundary correct if it ever authors more than one.
        for order in orders.iter_mut().skip(authored_start) {
            if let Order::Thrust {
                accel_mmss,
                yaw_urad,
                pitch_urad,
            } = order
            {
                *pitch_urad = 0;
                *accel_mmss = if speed_probe {
                    // A modified cruiser must ask beyond its honest acceleration
                    // ceiling or a raised ceiling is inert at the 32 m/s roam.
                    full_accel
                } else {
                    profile.accel_mmss(tick, speed, *accel_mmss, CRUISE_MPS)
                };
                *yaw_urad = yaw_urad.signum() * turn_urad.abs();
            }
        }

        // ── A18 S6.c: one seam call, not a hand-rolled loop ───────────────
        //
        // The loop this replaced stepped the player craft, folded
        // `ruleset().deliver` over its events by hand, stepped every hosted
        // authority in a second loop and folded the same call again. All four
        // of those are `step_predicted`: submission order is D46 clause (d),
        // the prediction set is exactly the authorities this peer holds (so
        // every replica it also holds stays frozen), and `BotTick::route`
        // takes every delivery so the routing decision — own inbox, a hosted
        // inbox, or the router — stays this driver's, made once below.
        for ((from, _), order) in delivered.iter().zip(orders.iter()) {
            self.host
                .submit_delivered_input(self.entity, *from, order.clone());
        }
        for order in &orders[authored_start..] {
            self.host.submit_input(self.entity, order.clone());
        }
        for entity in &hosted_at_boundary {
            for (from, order) in hosted_frozen.get(entity).into_iter().flatten() {
                self.host
                    .submit_delivered_input(*entity, *from, order.clone());
            }
        }
        let prediction = PredictionSet::only(
            core::iter::once(self.entity).chain(hosted_at_boundary.iter().copied()),
        );
        let mut participant = BotTick::new(self.entity, tick);
        let report = self
            .host
            .step_predicted(TickCount::new(1), &prediction, &mut participant);

        // Log *before* executing was the old loop's rule, and it is still what
        // happens: `BotTick::sealed` runs at S0, after input became immutable
        // and before any rule ran, and what it hands back is the applied
        // vector with the provenance each input arrived under — not a second
        // vector derived beside it. A log written from what happened rather
        // than what was asked would close the gap a cheat lives in by
        // construction, and then the harness could not tell an honest bot from
        // a careful one.
        if let Some(chain) = &mut self.chain {
            chain.log_inputs_with_sources(tick, &participant.orders, &participant.sources);
        }
        let events: Vec<Outcome> = self
            .host
            .events()
            .iter()
            .filter(|emitted| emitted.source() == self.entity)
            .map(|emitted| emitted.event().clone())
            .collect();
        #[cfg(test)]
        {
            self.last_step = SteppedTick {
                predicted: report.state_hashes.len(),
                events: self
                    .host
                    .events()
                    .iter()
                    .map(|emitted| EmittedEvent {
                        source: emitted.source(),
                        event: emitted.event().clone(),
                    })
                    .collect(),
            };
        }
        // This driver is the only consumer of the host's event buffer and
        // takes a tick's worth per tick: nothing may survive into the next.
        self.host.clear_events();
        if self.audience_trace
            && (traced_delivered
                .as_ref()
                .is_some_and(|items| !items.is_empty())
                || events
                    .iter()
                    .any(|event| matches!(event, Outcome::Collision { .. })))
        {
            let traced_state = self
                .host
                .backend()
                .state(self.entity)
                .expect("entity remains present");
            let RegolithState::Craft(traced_craft) = traced_state else {
                unreachable!("the player entity is always a craft")
            };
            eprintln!(
                "audience_trace kind=canonical_step seat={} tick={tick} entity={} delivered={:?} events={:?} state_hash={:?} pos={:?} vel={:?}",
                self.index,
                self.entity.0,
                traced_delivered.unwrap_or_default(),
                events,
                state_hash(traced_state),
                traced_craft.pos,
                traced_craft.vel,
            );
        }
        if let Some(chain) = &mut self.chain {
            for stepped in &report.neighbor_frames {
                if stepped.entity != self.entity {
                    continue;
                }
                // The tick the reader was advanced on, from the report, so a
                // record set can never be filed under a tick that did not
                // produce it.
                chain.log_neighbor_frames(stepped.tick.0, &stepped.frames);
            }
        }
        if let Some(resolved_shots) = &mut self.resolved_shots {
            resolved_shots.extend(
                events
                    .iter()
                    .filter(|event| matches!(event, Outcome::ShotResolved { .. }))
                    .cloned(),
            );
        }
        // The routing decision the seam deliberately does not make. Applied in
        // the order the tick produced them — the player craft's, then each
        // hosted authority's in canonical id order — which is the order the
        // two deleted loops appended in.
        for routed in participant.routed {
            let RoutedDelivery {
                source,
                recipient,
                order,
            } = routed;
            if recipient == self.entity {
                self.delivered_inbox.push((source, order));
            } else if self.hosted.contains(&recipient) {
                self.hosted_inbox
                    .entry(recipient)
                    .or_default()
                    .push((source, order));
            } else {
                self.delivered_outbox.push((source, recipient, order));
            }
        }
        let own_hash = report
            .state_hashes
            .iter()
            .find(|hashed| hashed.entity == self.entity)
            .map(|hashed| hashed.hash)
            .expect("entity present");
        if let Some(shadow) = &mut self.honest_shadow {
            // The dual-execution probe: the *shipping* rules over the same
            // sealed vector, on their own host. Its prediction set is the
            // player craft alone — the deleted loop stepped nothing else on
            // the shadow either — and `ShadowTick` drops every delivery, so
            // an honest event can never queue an input the modified column
            // never saw.
            for ((from, _), order) in delivered.iter().zip(orders.iter()) {
                shadow.submit_delivered_input(self.entity, *from, order.clone());
            }
            for order in &orders[authored_start..] {
                shadow.submit_input(self.entity, order.clone());
            }
            let honest = shadow.step_predicted(
                TickCount::new(1),
                &PredictionSet::just(self.entity),
                &mut ShadowTick,
            );
            shadow.clear_events();
            let honest_hash = honest
                .state_hashes
                .iter()
                .find(|hashed| hashed.entity == self.entity)
                .map(|hashed| hashed.hash)
                .expect("entity present");
            if self.first_tampered_tick.is_none() && honest_hash != own_hash {
                self.first_tampered_tick = Some(tick);
            }
        }
        if let Some(chain) = &mut self.chain {
            chain.log_tick_hash(own_hash);
        }

        // Scenario content is canonical state, not decoration. Every
        // additional authority in the prediction set stepped beside the player
        // craft on this one tick; materialized children begin on the next one,
        // matching the reference scenario loop. The old loop read
        // `TickOutcome::materialized` per hosted emitter and ignored the
        // player craft's own; `StepReport::materialized` names the emitter, so
        // that stays exactly what it was.
        for born in &report.materialized {
            if born.source == self.entity || self.authors(born.entity) {
                continue;
            }
            let state = self
                .host
                .backend()
                .state(born.entity)
                .expect("materialization installed its canonical state")
                .clone();
            self.host_entity(born.entity, state);
        }
        for entity in hosted_at_boundary {
            self.hosted_inbox.entry(entity).or_default();
        }

        let grid = grid_of(&self.craft().pos, cell_edge_m);
        let world = self.app.world_mut();
        let mut query = world.query_filtered::<(&mut GridPosition, &Cell), With<LocalPlayer>>();
        let Ok((mut position, _)) = query.single_mut(world) else {
            return;
        };
        position.0 = grid;
    }

    /// Re-seat both hosts' clocks on `at`, if they are not already there.
    ///
    /// [`Self::step_core`] is handed its tick by the harness around it, and a
    /// host fixes its first tick at construction. A host that has never
    /// stepped is entirely described by its population and its stamps, so
    /// re-seating is: build a host at `at` and re-install what the old one
    /// held, byte for byte and stamp for stamp. Nothing else can be in flight
    /// — this driver submits, steps and drains its events inside one
    /// `step_core` call, so no input is ever queued and no event ever left
    /// undrained across one.
    ///
    /// In a swarm run this fires once, on tick zero. It exists because a test
    /// may start a fresh bot at an arbitrary tick, which the executor this
    /// replaced allowed because it had no clock of its own to disagree with.
    fn seat_hosts_at(&mut self, at: Tick) {
        if self.host.next_tick() == at {
            debug_assert!(
                self.honest_shadow
                    .as_ref()
                    .is_none_or(|shadow| shadow.next_tick() == at),
                "the shadow's clock never parts company with the modified column's"
            );
            return;
        }
        let rules = self
            .tamper
            .map_or_else(Regolith::honest, Regolith::cheating);
        self.host = reseat(&self.host, rules, self.seed, at);
        if let Some(shadow) = &self.honest_shadow {
            self.honest_shadow = Some(reseat(shadow, Regolith::honest(), self.seed, at));
        }
    }

    fn capture_adjudication_geometry(&self, tick: u64, orders: &[Order]) {
        for order in orders {
            let Order::Damage {
                from,
                from_pos,
                from_yaw_urad,
                from_archetype,
                from_weapon,
                flight_ticks: None,
                ..
            } = order
            else {
                continue;
            };
            let target = self.craft();
            let measurement =
                firing_arc_measurement(*from_archetype, *from_yaw_urad, *from_pos, target.pos);
            let range_sq = target.pos.distance_squared(*from_pos);
            let distance_mm = distance_mm(target.pos, *from_pos);
            let reach_mm = from_weapon
                .weapon()
                .optimal_mm
                .saturating_add(from_weapon.weapon().falloff_mm)
                .saturating_add(target.archetype.limits().radius_mm);
            eprintln!(
                "geometry_capture side=host tick={tick} attacker={} target={} attacker_pos={:?} \
                 target_pos={:?} attacker_yaw_urad={} archetype={:?} \
                 world_bearing_urad={:?} relative_urad={:?} inside={} \
                 distance_mm={} distance_sq_mm2={} reach_mm={}",
                from.0,
                self.entity.0,
                from_pos,
                target.pos,
                from_yaw_urad,
                from_archetype,
                measurement.world_bearing_urad,
                measurement.relative_urad,
                measurement.inside,
                distance_mm,
                range_sq,
                reach_mm,
            );
        }
    }

    /// Advance simulated time and run one frame of the plugin stack.
    pub fn update(&mut self) {
        self.app
            .world_mut()
            .resource_mut::<Time<Real>>()
            .advance_by(TICK);
        self.app.world_mut().resource_mut::<SimTick>().0 = self.tick;
        // The witness times its repairs in subject ticks, so it needs this
        // bot's tick rather than wall time. Without it a peer that goes quiet
        // inside an open hole is never escalated, because every other repair
        // check needs a frame to arrive before it runs.
        if let Some(mut clock) = self
            .app
            .world_mut()
            .get_resource_mut::<orrery_witness::WitnessClock>()
        {
            clock.0 = Tick::new(self.tick);
        }
        self.app.update();
    }

    /// Record what changed this tick without enabling host crossing emission.
    pub fn sample(&mut self) {
        let _ = self.sample_with_interest_crossing(false, 0.0);
    }

    /// Record what changed this tick and emit an ordered interest crossing.
    ///
    /// The feature flag deliberately gates both #653 mechanisms in the harness:
    /// the predictive swept margin covers where a craft can reach before the
    /// next bulk refresh, while this reactive event corrects the roster after
    /// the actual hysteresis commitment. Neither is redundant with the other,
    /// and the one-hertz bulk refresh remains the repair path for lost events.
    pub fn sample_with_interest_crossing(
        &mut self,
        swept_interest_margin: bool,
        refresh_period_s: f64,
    ) -> Option<InterestCellCrossing> {
        let committed_cell = {
            let world = self.app.world_mut();
            let mut cells = world.query_filtered::<&Cell, With<LocalPlayer>>();
            cells.single(world).ok().map(|cell| cell.0)
        };
        let committed_crossing = committed_cell.and_then(|cell| self.record_cell_commitment(cell));

        let world = self.app.world_mut();
        let high: Vec<Entity> = world.resource::<InterestSelection>().high_rate.clone();
        let entered: Vec<Entity> = high
            .iter()
            .copied()
            .filter(|e| !self.last_high_rate.contains(e))
            .collect();
        let left: Vec<Entity> = self
            .last_high_rate
            .iter()
            .copied()
            .filter(|e| !high.contains(e))
            .collect();
        self.interest_churn += entered.len() as u64 + left.len() as u64;

        // Churn itself is not a pop. docs/03-replication.md §9.5 answers
        // flapping with a *rate ramp*, not by preventing demotion: a
        // just-demoted entity keeps streaming at the top of the proxy range and
        // decays to its scored rate over five seconds, so a brief round trip is
        // invisible. What a player actually sees is a demoted entity whose
        // stream fell to the bottom of the range before it came back.
        //
        // So a pop is a re-promotion inside the window where the proxy rate had
        // already sagged below the ramp floor. Counting every round trip would
        // measure a crowd moving, which is the case the design accommodates
        // rather than the one it forbids.
        for entity in &entered {
            if let Some(index) = self.demoted_at.iter().position(|(e, _, _)| e == entity) {
                let (_, when, worst_rate) = self.demoted_at.remove(index);
                let brief = self.tick.saturating_sub(when) <= POP_WINDOW_TICKS;
                if brief && worst_rate < RAMP_FLOOR_HZ {
                    self.proxy_pops += 1;
                }
            }
        }
        // Track the worst rate each demoted entity is currently seeing, and
        // forget any that left the neighbourhood entirely.
        //
        // §9.5 is about entities *alternating in and out of the 24 slots* while
        // remaining in range. An entity that walked out of the AOI and back had
        // its stream interrupted by the coarse filter, which is the AOI diff's
        // business, not the interest set's — counting it here would blame the
        // eviction policy for something it never saw.
        let selection_rates: Vec<(Entity, f32)> =
            world.resource::<InterestSelection>().proxies.clone();
        self.demoted_at.retain(|(entity, _, _)| {
            selection_rates.iter().any(|(e, _)| e == entity) || high.contains(entity)
        });
        for (entity, _, worst) in &mut self.demoted_at {
            if let Some((_, rate)) = selection_rates.iter().find(|(e, _)| e == entity) {
                *worst = worst.min(*rate);
            }
        }
        for entity in &left {
            self.demoted_at.retain(|(e, _, _)| e != entity);
            self.demoted_at.push((*entity, self.tick, f32::INFINITY));
        }
        // Forget demotions too old to pop, so the list stays bounded.
        let horizon = self.tick.saturating_sub(POP_WINDOW_TICKS);
        self.demoted_at.retain(|(_, when, _)| *when >= horizon);

        self.last_high_rate = high;
        let crossing = if swept_interest_margin {
            committed_crossing.map(|committed| InterestCellCrossing {
                tick: Tick::new(self.tick),
                seq: SeqPair {
                    // The harness gives each bot one stable authority tenure.
                    own_seq: 1,
                    auth_seq: u32::try_from(self.crossings)
                        .expect("a harness run fits its crossing order in u32"),
                },
                from: committed.from,
                to: committed.to,
                covered_cells: self.swept_interest_cells(refresh_period_s),
            })
        } else {
            None
        };
        self.tick += 1;
        crossing
    }

    fn record_cell_commitment(&mut self, cell: CellId) -> Option<CommittedCellCrossing> {
        if self.current_cell == Some(cell) {
            return None;
        }
        let from = self.current_cell;
        self.crossings += 1;
        let is_return = self.previous_cell == Some(cell);
        if is_return {
            self.boundary_flips += 1;
            self.boundary_return_window.record(self.tick);
            self.max_boundary_returns_in_window = self.boundary_return_window.max_returns;
        }
        if self.audience_trace {
            eprintln!(
                "audience_trace kind=cell_commitment seat={} tick={} entity={} previous={:?} current={:?} next={:?} flip={is_return}",
                self.index,
                self.tick,
                self.entity.0,
                self.previous_cell,
                self.current_cell,
                cell,
            );
        }
        self.previous_cell = self.current_cell;
        self.current_cell = Some(cell);
        if !self.visited.contains(&cell) {
            self.visited.push(cell);
        }
        from.map(|from| CommittedCellCrossing { from, to: cell })
    }

    /// This peer's upload rate over the meter's window, in simulated time.
    pub fn upload_rate_bits(&mut self) -> u64 {
        let budget = *self.app.world().resource::<UploadBudget>();
        let now = self.app.world().resource::<Time<Real>>().elapsed();
        let mut meter = core::mem::take(&mut *self.app.world_mut().resource_mut::<UploadMeter>());
        let rate = meter.rate(budget, now).bits_per_sec();
        *self.app.world_mut().resource_mut::<UploadMeter>() = meter;
        rate
    }

    /// Override the shipping upload meter's sustained ceiling for a pressure run.
    ///
    /// The averaging window is deliberately retained. A20 varies the allowance,
    /// not the definition of "sustained", and changing both would make points on
    /// the pressure curve incomparable.
    pub fn set_upload_budget_bits(&mut self, bits_per_sec: u64) {
        self.app
            .world_mut()
            .resource_mut::<UploadBudget>()
            .sustained = Bandwidth::from_bits_per_sec(bits_per_sec);
    }

    /// The ceiling currently installed in this bot's real upload meter.
    #[must_use]
    pub fn upload_budget_bits(&self) -> u64 {
        self.app
            .world()
            .resource::<UploadBudget>()
            .sustained
            .bits_per_sec()
    }

    /// The directional one-refresh-period interest coverage #692 added.
    ///
    /// This is harness-only host wiring. The production coordinator/client seam
    /// remains a follow-up; the measurement nevertheless exercises the exact
    /// protocol primitive with the bot's committed cell, exact offset and
    /// canonical velocity rather than manufacturing an audience-size multiplier.
    #[must_use]
    pub fn swept_interest_cells(&mut self, refresh_period_s: f64) -> Vec<CellId> {
        let cell = self.cell().expect("a bot has one committed cell");
        let edge_m = f64::from(self.app.world().resource::<CellEdge>().0);
        let (cell_coords, _) = cell.coords();
        let (x, y, z) = self.craft().pos.to_metres();
        let (vx, vy, vz) = self.craft().vel.to_metres_per_sec();
        let minimum = glam::DVec3::new(
            f64::from(cell_coords.x) * edge_m,
            f64::from(cell_coords.y) * edge_m,
            f64::from(cell_coords.z) * edge_m,
        );
        let offset = glam::DVec3::new(x, y, z) - minimum;
        cell.swept_neighbors27(
            offset,
            glam::DVec3::new(vx, vy, vz),
            refresh_period_s,
            edge_m,
        )
    }

    /// How many entities are currently proxied rather than high-rate.
    #[must_use]
    pub fn proxies(&self) -> usize {
        self.app
            .world()
            .resource::<InterestSelection>()
            .proxies
            .len()
    }

    /// Packets the send path shed for want of budget.
    #[must_use]
    pub fn shed(&self) -> u64 {
        self.app.world().resource::<UploadMeter>().shed
    }

    /// What each lane spent over the run, in wire bytes.
    ///
    /// The split is what makes an overrun actionable: a peak of 1006 kbps says
    /// a peer is over its ceiling and says nothing about which dial to turn,
    /// and *which* was P4's whole question about the witness lane.
    #[must_use]
    pub fn lanes(&self) -> orrery_net::budget::LaneTally {
        self.app.world().resource::<UploadMeter>().lanes
    }

    /// What the peer link counted: what it moved, and what it could not.
    ///
    /// The fault counters in here — `no_session`, `oversized`, `untagged` —
    /// must all be zero on a healthy run. The report carries them so a
    /// non-zero value is seen by observation rather than found by reading
    /// `send_peer_packets`, which is how #953's whole-attempt replication
    /// failure was found (#954).
    #[must_use]
    pub fn peer_link_counters(&self) -> PeerLinkCounters {
        *self.app.world().resource::<PeerLinkCounters>()
    }

    /// Unsheddable packets sent while over budget (docs/03-replication.md
    /// §9.3) — the overrun the backstop could not shed away.
    #[must_use]
    pub fn unsheddable_over_budget(&self) -> u64 {
        self.app
            .world()
            .resource::<UploadMeter>()
            .unsheddable_over_budget
    }

    /// Install the island roster this bot believes it is in.
    ///
    /// Stands in for the coordinator's manifest: the harness knows the true
    /// membership, and island *formation* is P3's criterion rather than this
    /// one. What is under test here is what a peer does with a roster, not how
    /// it obtains one.
    pub fn set_island(&mut self, peers: Vec<PeerEntry>) {
        *self.app.world_mut().resource_mut::<IslandMembership>() = IslandMembership {
            island: Some(IslandId::new(1)),
            epoch: 1,
            peers,
            regime: TopologyRegime::Mesh,
            source: IslandSource::Coordinator,
        };
    }

    /// Apply one coordinator-ordered crossing to the roster this sender reads.
    ///
    /// Updating the existing entry in place is intentional: `peers` order feeds
    /// deterministic link and audience behaviour in this harness. Rebuilding it
    /// through a keyed collection would make an interest-only change perturb the
    /// flags-off simulation.
    /// A receiver that does not carry the author in its published roster has no
    /// interest coverage to correct, so the event is not addressed to it. That
    /// is a normal state for a peer in another island or one whose roster has
    /// not been published yet -- it must not be fatal on a live campaign host,
    /// where the same binary serves human seats.
    pub fn apply_interest_crossing(&mut self, node: NodeId, crossing: &InterestCellCrossing) {
        let membership = &mut *self.app.world_mut().resource_mut::<IslandMembership>();
        let Some(entry) = membership.peers.iter_mut().find(|entry| entry.node == node) else {
            return;
        };
        entry.cells.clone_from(&crossing.covered_cells);
    }

    /// Broadcasts every state this peer authors to island-mates whose interest
    /// cells contain that entity.
    ///
    /// Shared by the swarm loop and the external-peer runner (#385): both must
    /// replicate from the same function or the human path would send bytes no
    /// witness had reasoned about.
    ///
    /// The craft bytes plus the authority's *committed* cell. D2 makes the
    /// commitment a single-writer value emitted by the holder: a receiver that
    /// recomputed it from the position would get the raw geometric cell with
    /// no hysteresis, so a peer sitting on a boundary would flip cells on
    /// every packet and flap in and out of the receiver's AOI for reasons that
    /// have nothing to do with where it is. The entity and the tick travel
    /// with the state — prediction needs the tick and interest needs the
    /// identity — and without them a receiver holds bytes it cannot attribute
    /// to a subject or line up against a claim.
    pub fn broadcast_state(&mut self, tick: u64) {
        let player_cell = self.cell().expect("committed");
        let cell_edge_m = self.app.world().resource::<CellEdge>().0;
        let node = self.node;
        let send_index = self.replication_send_index;
        let keyframe_every = self.keyframe_every_sends;
        let membership = self.app.world().resource::<IslandMembership>();
        let audiences: BTreeMap<PersistId, BTreeSet<NodeId>> = self
            .authored_entities()
            .into_iter()
            .map(|entity| {
                let state = self.host.backend().state(entity).expect("authored entity");
                let cell = authored_cell(entity, self.entity, state, player_cell, cell_edge_m);
                let audience = membership
                    .peers
                    .iter()
                    .filter(|entry| entry.node != node && entry.cells.contains(&cell))
                    .map(|entry| entry.node)
                    .collect();
                (entity, audience)
            })
            .collect();
        let authored: Vec<_> = self
            .authored_entities()
            .into_iter()
            .filter_map(|entity| {
                let state = self.host.backend().state(entity)?.clone();
                let cell = authored_cell(entity, self.entity, &state, player_cell, cell_edge_m);
                let audience = audiences.get(&entity)?.clone();
                Some((entity, state, cell, audience))
            })
            .collect();
        let mut sends = Vec::new();
        for (entity, state, cell, audience) in authored {
            let canonical = state.to_canonical();
            if let Some(stats) = &mut self.delta_stats {
                stats.observe(entity, &state, &canonical);
            }

            let previous_audience = self
                .replication_audiences
                .get(&entity)
                .cloned()
                .unwrap_or_default();
            let added: BTreeSet<NodeId> =
                audience.difference(&previous_audience).copied().collect();
            let removed: BTreeSet<NodeId> =
                previous_audience.difference(&audience).copied().collect();
            if let Some(changes) = &mut self.audience_changes {
                // The first roster snapshot establishes a baseline. It is not
                // a roaming transition and would turn startup into a false
                // correlation burst.
                if self.replication_audiences.contains_key(&entity)
                    && (!added.is_empty() || !removed.is_empty())
                {
                    changes.push(AudienceChange {
                        entity,
                        joins: added.len() as u64,
                        leaves: removed.len() as u64,
                        window: tick / TICK_HZ,
                    });
                }
            }
            let existing: BTreeSet<NodeId> = audience.difference(&added).copied().collect();
            let observed_tick = tick + 1;
            let due = !self.sender_keyframes.contains_key(&entity)
                || send_index % keyframe_every == entity.0 % keyframe_every;

            if due {
                let payload = encode_replication_compressed(&(
                    canonical.clone(),
                    cell,
                    entity,
                    observed_tick,
                ));
                let previous = self.sender_keyframes.insert(
                    entity,
                    SenderKeyframe {
                        canonical,
                        cell,
                        tick: observed_tick,
                        payload: payload.clone(),
                    },
                );
                self.sender_keyframe_rollbacks.insert(entity, previous);
                sends.extend(
                    audience
                        .iter()
                        .copied()
                        .map(|peer| (peer, payload.clone(), ReplicationKind::Keyframe)),
                );
            } else if let Some(keyframe) = self.sender_keyframes.get(&entity) {
                // New subscribers are deliberately excluded from this tick's
                // delta audience. Datagram ordering is not a baseline protocol:
                // their cached keyframe must arrive before any delta is even
                // eligible for that link.
                sends.extend(
                    added
                        .iter()
                        .copied()
                        .map(|peer| (peer, keyframe.payload.clone(), ReplicationKind::Keyframe)),
                );

                let cell_changed = cell != keyframe.cell;
                if canonical != keyframe.canonical || cell_changed {
                    let keyframe_age = u16::try_from(observed_tick - keyframe.tick)
                        .expect("a 1 Hz keyframe age fits in u16 ticks");
                    let patch = encode_delta_patch(&keyframe.canonical, &canonical);
                    let delta = ReplicationDelta {
                        entity,
                        tick: observed_tick,
                        keyframe_age,
                        cell: cell_changed.then_some(cell),
                        patch,
                    };
                    let absolute = (canonical, cell, entity, observed_tick);
                    if let Some(payload) = encode_delta_if_smaller(&absolute, &delta) {
                        sends.extend(
                            existing
                                .iter()
                                .copied()
                                .map(|peer| (peer, payload.clone(), ReplicationKind::Delta)),
                        );
                    }
                    // A size fallback is an absolute keyframe. Emitting one on
                    // the arbitrary tick where entropy crossed the codec's size
                    // boundary would bypass the per-entity stagger and recreate
                    // the peak burst this cadence exists to remove. The state is
                    // therefore left for this entity's next staggered keyframe;
                    // the larger delta is never sent.
                }
            }
            let previous = self.replication_audiences.insert(entity, audience);
            self.replication_audience_rollbacks.insert(entity, previous);
        }
        self.replication_send_index += 1;

        let mut messages = self
            .app
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<SendPacket>>();
        for (peer, payload, kind) in sends {
            self.replication_wire.record(kind, payload.len());
            messages.write(SendPacket {
                to: peer,
                channel: Channel::State,
                payload: Bytes::from(payload),
                mode: StreamMode::Shared,
            });
        }
    }

    /// Attach a session for `peer`, so the send path has somewhere to write.
    ///
    /// Idempotent. `form_island` links the seats present when the run starts
    /// and `refresh_rosters` links every seat that binds afterwards, so both
    /// call this for peers already linked; spawning a second `Peer` for one
    /// node would give `send_peer_packets` two sessions to choose between.
    /// The check is a query of the world this spawns into rather than a
    /// second set kept alongside it, so it cannot disagree with reality.
    pub fn link(&mut self, peer: NodeId, mtu: usize) {
        if self.is_linked_to(peer) {
            return;
        }
        self.app.world_mut().spawn((
            Peer {
                id: peer,
                incoming: false,
            },
            aeronet_io::Session::new(bevy_platform::time::Instant::now(), mtu),
            // The reliable lane. Control rides QUIC streams (D3), so a peer
            // without this has nowhere to put a gap repair at all.
            aeronet_iroh::stream::IrohStreamIo::detached(),
        ));
    }

    /// Whether this bot holds a transport session for `peer`.
    ///
    /// `send_peer_packets` finds its session by exactly this predicate
    /// (`crates/orrery_net/src/peer_link.rs`), and a packet addressed to a
    /// node with no match is dropped against `no_session`, which nothing in
    /// the swarm report surfaces. So this is the difference between "the
    /// roster names the seat" and "the seat can be sent to".
    #[must_use]
    pub fn is_linked_to(&mut self, peer: NodeId) -> bool {
        self.sessions_to(peer) > 0
    }

    /// How many transport sessions this bot holds for `peer`.
    ///
    /// More than one is a defect, not a redundancy: `send_peer_packets` takes
    /// the `find` first match, so a duplicate silently halves nothing and
    /// hides which session a packet went down.
    #[must_use]
    pub fn sessions_to(&mut self, peer: NodeId) -> usize {
        self.app
            .world_mut()
            .query::<&Peer>()
            .iter(self.app.world())
            .filter(|existing| existing.id == peer)
            .count()
    }

    /// Drops everything this bot has queued to send, on both lanes.
    ///
    /// The harness's stand-in for a client stall: the packets were built and
    /// then never left, which is what an unserviced socket is. The peer's own
    /// log stays intact, so it can still answer for itself when its witnesses
    /// come asking. A stalling peer does not get to keep its reliable lane
    /// flowing while its datagrams stop.
    pub fn stall(&mut self) {
        let world = self.app.world_mut();
        let mut query = world.query::<(
            &orrery_net::plugin::Peer,
            &mut aeronet_io::Session,
            &mut IrohStreamIo,
        )>();
        let mut discarded_keyframes = 0;
        let mut discarded_deltas = 0;
        for (_, mut session, mut streams) in query.iter_mut(world) {
            for payload in &session.send {
                let Some((Channel::State, inner)) = orrery_net::channels::untag(payload) else {
                    continue;
                };
                if is_delta_payload(inner) {
                    discarded_deltas += 1;
                } else if orrery_net::channels::decode_replication::<(
                    Vec<u8>,
                    CellId,
                    PersistId,
                    u64,
                )>(inner)
                .is_some()
                {
                    discarded_keyframes += 1;
                }
            }
            session.send.clear();
            streams.send.clear();
        }
        self.replication_wire.keyframes_discarded_while_stalled += discarded_keyframes;
        self.replication_wire.deltas_discarded_while_stalled += discarded_deltas;
        for (entity, previous) in core::mem::take(&mut self.sender_keyframe_rollbacks) {
            // The sender clock advanced when `broadcast_state` built this
            // keyframe, but the simulated hitch proves no recipient could have
            // received it. Restore the last anchor that did leave the app, so
            // post-hitch deltas remain decodable without an off-phase keyframe
            // burst. With no predecessor, remove the phantom first anchor and
            // let the first post-hitch send be absolute.
            match previous {
                Some(previous) => {
                    self.sender_keyframes.insert(entity, previous);
                }
                None => {
                    self.sender_keyframes.remove(&entity);
                }
            }
        }
        for (entity, previous) in core::mem::take(&mut self.replication_audience_rollbacks) {
            // No packet from this send left the app, including the cached
            // keyframe offered to a newly interested peer. Restore the prior
            // audience so that peer remains "added" and is offered the anchor
            // again on the first post-hitch send.
            match previous {
                Some(audience) => {
                    self.replication_audiences.insert(entity, audience);
                }
                None => {
                    self.replication_audiences.remove(&entity);
                }
            }
        }
    }

    /// Drains everything queued to send into `(recipient, lane, payload)`
    /// triples, in queue order.
    ///
    /// Shared with the external-peer path (#385): whatever drains here goes
    /// into the router for an in-process bot and onto the wire for the remote
    /// one, from this one function, so the two paths cannot diverge.
    pub fn drain_outbound(&mut self) -> Vec<(NodeId, Option<StreamMode>, Bytes)> {
        let mut outbound = Vec::new();
        let world = self.app.world_mut();
        let mut query = world.query::<(
            &orrery_net::plugin::Peer,
            &mut aeronet_io::Session,
            &mut IrohStreamIo,
        )>();
        for (peer, mut session, mut streams) in query.iter_mut(world) {
            for packet in session.send.drain(..) {
                outbound.push((peer.id, None, packet));
            }
            for message in streams.send.drain(..) {
                outbound.push((peer.id, Some(message.mode), message.payload));
            }
        }
        // Keyframes in `outbound` have crossed the seam at which `stall` could
        // discard them. Router loss from here is the ordinary A19 §5.5 case;
        // the sender deliberately receives no per-link feedback for it.
        self.sender_keyframe_rollbacks.clear();
        self.replication_audience_rollbacks.clear();
        outbound
    }

    /// Drain ruleset deliveries in event-emission order for the authority
    /// router. Address resolution belongs to the swarm because it owns the
    /// entity-to-peer roster, including a token-authenticated exterior node
    /// whose transport identity cannot be derived from its slot.
    pub fn drain_delivered(&mut self) -> Vec<(PersistId, PersistId, Order)> {
        core::mem::take(&mut self.delivered_outbox)
    }

    #[cfg(test)]
    /// Replaces the authored craft with a captured live-state fixture.
    pub fn replace_craft_for_test(&mut self, craft: Craft) {
        self.host
            .install_state(self.entity, RegolithState::Craft(craft));
    }

    #[cfg(test)]
    /// Records a captured committed-cell transition at an explicit tick.
    pub fn record_cell_commitment_for_test(&mut self, cell: CellId, tick: u64) {
        self.tick = tick;
        let _ = self.record_cell_commitment(cell);
    }

    #[cfg(test)]
    /// Move the local player's spatial input without bypassing hysteresis.
    pub fn move_local_player_for_test(&mut self, grid_position: Vec3) {
        let world = self.app.world_mut();
        let mut positions = world.query_filtered::<&mut GridPosition, With<LocalPlayer>>();
        positions
            .single_mut(world)
            .expect("a bot has exactly one local player")
            .0 = grid_position;
    }

    #[cfg(test)]
    /// Place the authored player in a fixture cell before island formation.
    pub fn place_local_player_for_test(&mut self, cell: CellId) {
        let (coords, _) = cell.coords();
        let position = Vec3::new(
            coords.x as f32 + 0.5,
            coords.y as f32 + 0.5,
            coords.z as f32 + 0.5,
        );
        let world = self.app.world_mut();
        let mut local = world.query_filtered::<(&mut GridPosition, &mut Cell), With<LocalPlayer>>();
        let (mut grid, mut committed) = local
            .single_mut(world)
            .expect("a bot has exactly one local player");
        grid.0 = position;
        committed.0 = cell;
        self.current_cell = Some(cell);
        self.previous_cell = None;
        self.visited = vec![cell];
    }

    #[cfg(test)]
    /// Injects one prior-tick delivery without bypassing delivered-first composition.
    pub fn inject_delivered(&mut self, from: PersistId, order: Order) {
        self.delivered_inbox.push((from, order));
    }

    /// Drains target-authored shot verdicts observed at the real step boundary.
    pub fn take_resolved_shots(&mut self) -> Vec<Outcome> {
        self.resolved_shots
            .as_mut()
            .map(core::mem::take)
            .unwrap_or_default()
    }

    /// Hands one delivered message from `from` to this bot's receive buffers.
    ///
    /// The inverse of [`Self::drain_outbound`], shared for the same reason:
    /// the host's `deliver` and the external peer's receive pump must inject
    /// bytes identically or the witness lanes see different worlds.
    pub fn receive_inbound(
        &mut self,
        from: NodeId,
        from_entity: PersistId,
        stream: Option<StreamMode>,
        payload: Bytes,
    ) {
        if stream.is_none() {
            let tagged = orrery_net::channels::untag(&payload)
                .filter(|(channel, _)| *channel == Channel::State);
            let wire_kind = tagged.map_or("unknown", |(_, inner)| {
                if is_delta_payload(inner) {
                    "delta"
                } else {
                    "keyframe"
                }
            });
            let replication = tagged
                .and_then(|(_, inner)| decode_replica(inner, &mut self.replica_keyframes).ok());
            if let Some(replication) = replication {
                let entity = replication.entity;
                let authority_matches = self
                    .replica_authorities
                    .get(&entity)
                    .is_none_or(|authority| *authority == from);
                if entity != self.entity && !self.authors(entity) && authority_matches {
                    if let Ok(state) = RegolithState::decode(&replication.canonical) {
                        if self.audience_trace {
                            let (pos, vel) = match &state {
                                RegolithState::Craft(craft) => (Some(craft.pos), Some(craft.vel)),
                                _ => (None, None),
                            };
                            eprintln!(
                                "audience_trace kind=replica_arrival seat={} receive_tick={} sender_entity={} replica_entity={} state_tick={} wire_kind={wire_kind} cell={:?} state_hash={:?} pos={pos:?} vel={vel:?}",
                                self.index,
                                self.tick,
                                from_entity.0,
                                entity.0,
                                replication.tick,
                                replication.cell,
                                state_hash(&state),
                            );
                        }
                        self.replica_authorities.entry(entity).or_insert(from);
                        self.host.install_state_observed(
                            entity,
                            state.clone(),
                            Tick::new(replication.tick),
                        );
                        if let Some(shadow) = &mut self.honest_shadow {
                            shadow.install_state_observed(
                                entity,
                                state,
                                Tick::new(replication.tick),
                            );
                        }
                        self.replica_seen_at.insert(entity, self.tick);
                    }
                }
            }
        }
        let inner = orrery_net::channels::untag(&payload)
            .filter(|(channel, _)| *channel == orrery_net::channels::Channel::Control)
            .map(|(_, inner)| inner);
        let is_delivered = inner
            .and_then(orrery_protocol::channels::untag)
            .is_some_and(|(channel, body)| {
                channel == orrery_protocol::channels::Channel::Control
                    && body.first() == Some(&orrery_protocol::channels::TAG_DELIVERED_INPUT)
            });
        let delivered = inner.and_then(orrery_protocol::channels::decode_delivered_input);
        if let Some(delivered) = delivered {
            let sender_matches = delivered.from == from_entity
                || self.replica_authorities.get(&delivered.from) == Some(&from);
            if !sender_matches || !self.authors(delivered.recipient) {
                self.foreign_deliveries += 1;
            } else if let Ok(order) = Order::decode(&delivered.input) {
                if self.audience_trace && delivered.recipient == self.entity {
                    eprintln!(
                        "audience_trace kind=delivered_input seat={} receive_tick={} from_entity={} recipient={} order={order:?}",
                        self.index,
                        self.tick,
                        delivered.from.0,
                        delivered.recipient.0,
                    );
                }
                if delivered.recipient == self.entity {
                    self.delivered_inbox.push((delivered.from, order));
                } else {
                    self.hosted_inbox
                        .entry(delivered.recipient)
                        .or_default()
                        .push((delivered.from, order));
                }
            }
            // A delivered-input envelope is consumed at the authority seam,
            // including malformed or foreign ones. It must never fall through
            // into replication or witness consumers as foreign bytes.
            return;
        }
        if is_delivered {
            // A recognizable but truncated delivery is still consumed here.
            self.foreign_deliveries += 1;
            return;
        }
        let world = self.app.world_mut();
        let mut query = world.query::<(
            &orrery_net::plugin::Peer,
            &mut aeronet_io::Session,
            &mut IrohStreamIo,
        )>();
        for (peer, mut session, mut streams) in query.iter_mut(world) {
            // One session per linked peer; find the sender's.
            if peer.id != from {
                continue;
            }
            if stream.is_some() {
                streams.recv.push(RecvMessage {
                    payload,
                    recv_at: bevy_platform::time::Instant::now(),
                });
            } else {
                session.recv.push(aeronet_io::packet::RecvPacket {
                    payload,
                    recv_at: bevy_platform::time::Instant::now(),
                });
            }
            return;
        }
    }

    /// This bot's committed cell.
    #[must_use]
    pub fn cell(&mut self) -> Option<CellId> {
        let world = self.app.world_mut();
        let mut query = world.query_filtered::<&Cell, With<LocalPlayer>>();
        query.single(world).ok().map(|cell| cell.0)
    }

    /// Start watching `subject`'s `entity`, anchored at a signed claim.
    pub fn watch(
        &mut self,
        entity: PersistId,
        subject: NodeId,
        anchor: StateClaim,
        state: RegolithState,
    ) {
        let Some(mut witness) = self
            .app
            .world_mut()
            .get_resource_mut::<WitnessState<Regolith>>()
        else {
            return;
        };
        witness
            .0
            .watch(Watch {
                entity,
                subject,
                anchor,
                anchor_state: state,
            })
            .expect("the anchor is the subject's own signed claim");
    }

    /// Name the peers this bot streams its log to (docs/03 5.3, at most seven).
    pub fn set_witness_set(&mut self, members: Vec<NodeId>) {
        if let Some(mut set) = self.app.world_mut().get_resource_mut::<WitnessSet>() {
            set.members = members;
        }
    }

    /// Cut a claim for `tick` from the state *before* the tick executes.
    ///
    /// The ordering is the whole correctness of this: a `StateClaim` at tick T
    /// commits to the state before T runs, and a witness compares it against the
    /// hash it computed for T-1. Claiming post-step state instead labels every
    /// commitment one tick late, and every honest authority in the swarm reads
    /// as a cheat — which is exactly what this harness reported before the call
    /// moved ahead of `step_core`.
    pub fn publish_claim(&mut self, tick: u64) {
        let state = self
            .host
            .backend()
            .state(self.entity)
            .expect("seeded")
            .clone();
        let Some(chain) = &mut self.chain else {
            return;
        };
        let Some(claim) = chain.cut_claim(tick, &state) else {
            return;
        };
        self.app
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<PublishClaim>>()
            .write(PublishClaim {
                claim,
                snapshot: orrery_core::CoreCodec::to_canonical(&state),
            });
    }

    /// Publish this tick's frame, if one is due.
    ///
    /// A stalling profile still authors, retains and publishes: a client hitch
    /// is a *transport* failure, not a logging one. The peer keeps its log and
    /// can answer for it; what fails is the send, which the swarm models by
    /// dropping that peer's outbound packets for the window.
    ///
    /// Withholding the frame from the plugin instead — the harness's first
    /// attempt — left the authority unable to serve the very frames it had
    /// hidden, so twenty-eight repairs came back unservable and the witnesses
    /// escalated against a peer that had done nothing wrong.
    pub fn publish(&mut self, tick: u64) {
        let Some(chain) = &mut self.chain else {
            return;
        };
        let Some(authored) = chain.cut_frame(tick) else {
            return;
        };
        let entity = self.entity;
        self.app
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<PublishFrame>>()
            .write(PublishFrame {
                frame: authored.frame,
                transitions: authored.transitions,
                tick_hashes: vec![(entity, authored.tick_hashes)],
            });
    }

    /// Drain the witness signals this bot raised this tick.
    ///
    /// Every non-gap signal is attributed by *subject*: against an honest peer
    /// it is a false positive, against one the harness modified it is the
    /// finding the conviction leg exists to produce. Before `--cheat` there was
    /// no distinction to draw, because there was nobody to draw it against.
    pub fn drain_signals(&mut self) {
        let Some(mut messages) = self
            .app
            .world_mut()
            .get_resource_mut::<bevy_ecs::message::Messages<Witnessed>>()
        else {
            return;
        };
        let drained: Vec<Witnessed> = messages.drain().collect();
        for witnessed in drained {
            if let WitnessSignal::Gap(_) = witnessed.signal {
                // A question, not an accusation, whoever it is addressed to.
                self.signals.gaps += 1;
                continue;
            }
            if self.tampered_subjects.contains(&witnessed.subject) {
                self.signals.signals_against_tampered += 1;
                continue;
            }
            match witnessed.signal {
                // A subject that never fills a hole. Against an honest peer
                // that is a false positive like any other — every bot answers
                // repairs, so a stall means the repair path failed to keep up,
                // not that anyone refused.
                WitnessSignal::Stalled { .. } => self.signals.stalled += 1,
                WitnessSignal::InvariantBreach { .. } => self.signals.invariant_breaches += 1,
                WitnessSignal::ClaimMismatch { .. } => self.signals.claim_mismatches += 1,
                // Unreachable, and left as a no-op rather than folded into
                // `reports`. The adapter's `route` files a raised report and
                // `continue`s before it ever reaches `Witnessed`
                // (`orrery_witness::plugin`), so this arm counted nothing for
                // as long as it existed. Reports are counted where they
                // actually arrive — see `drain_reports`.
                WitnessSignal::Report(_) => {}
                WitnessSignal::Gap(_) => unreachable!("counted above"),
            }
        }
    }

    /// Drain the discrepancy reports this bot's witness filed this tick.
    ///
    /// Tallies them by subject and hands them back for adjudication. Nothing is
    /// here at all unless the peer was built `enforcing` *and* holds a
    /// [`WitnessIdentity`]: shadow mode short-circuits `raise`, and a missing
    /// identity stops `escalate` a step earlier still.
    pub fn drain_reports(&mut self) -> Vec<orrery_protocol::DiscrepancyReport> {
        let Some(mut messages) = self
            .app
            .world_mut()
            .get_resource_mut::<bevy_ecs::message::Messages<ReportFiled>>()
        else {
            return Vec::new();
        };
        let filed: Vec<ReportFiled> = messages.drain().collect();
        filed
            .into_iter()
            .map(|filed| {
                if self.tampered_subjects.contains(&filed.subject) {
                    self.signals.reports_against_tampered += 1;
                } else {
                    self.signals.reports += 1;
                }
                *filed.report
            })
            .collect()
    }

    /// Tell this bot which peers the harness modified.
    ///
    /// The oracle, and it is deliberately external: the witness engine is never
    /// shown it, so a signal is still raised or not raised on the evidence
    /// alone. All this changes is which column the harness counts it in.
    pub fn set_tampered_subjects(&mut self, subjects: Vec<NodeId>) {
        self.tampered_subjects = subjects;
    }

    /// The tamper this bot's authority runs, if it is a modified client.
    #[must_use]
    pub fn tamper(&self) -> Option<Tamper> {
        self.tamper
    }

    /// The first tick on which this bot's build produced state the shipping
    /// rules would not have, or `None` if it never did.
    ///
    /// `None` on an honest bot is the expected reading. `None` on a *modified*
    /// one is the finding that the cheat is inert at these parameters, and the
    /// swarm fails a clause on it rather than letting the conviction clauses
    /// pass over byte-identical state.
    #[must_use]
    pub fn first_tampered_tick(&self) -> Option<u64> {
        self.first_tampered_tick
    }

    /// What this peer's witness adapter did with the escalations it raised.
    #[must_use]
    pub fn link_counters(&self) -> WitnessLinkCounters {
        self.app
            .world()
            .get_resource::<WitnessLinkCounters>()
            .copied()
            .unwrap_or_default()
    }

    /// This bot's current core state, for seeding a watcher's anchor.
    #[must_use]
    pub fn state(&self) -> RegolithState {
        self.host
            .backend()
            .state(self.entity)
            .expect("seeded")
            .clone()
    }

    /// The entity this bot authors.
    #[must_use]
    pub fn entity(&self) -> PersistId {
        self.entity
    }

    /// Repair requests this peer dropped for want of queue space.
    #[must_use]
    pub fn repairs_overflowed(&self) -> u64 {
        self.app
            .world()
            .get_resource::<orrery_witness::plugin::PendingRepairs>()
            .map_or(0, |pending| pending.overflowed)
    }

    /// Repair requests this peer could not answer from its retained log.
    #[must_use]
    pub fn repairs_unservable(&self) -> u64 {
        self.app
            .world()
            .get_resource::<orrery_witness::plugin::WitnessLinkCounters>()
            .map_or(0, |c| c.repairs_unservable)
    }

    /// Whether this peer holds a watch on `entity` at all.
    #[must_use]
    pub fn watches(&self, entity: PersistId) -> bool {
        self.app
            .world()
            .get_resource::<WitnessState<Regolith>>()
            .is_some_and(|state| state.0.watches(entity))
    }

    /// Whether this peer holds a watch on `entity` that has seen no frame.
    ///
    /// Per subject, because the swarm-wide count averages the concentrated
    /// case away — see `Witness::watch_is_dark` (#1130). `false` when this peer
    /// does not watch the entity at all: not watching is not the same as
    /// watching blind, and only the second is a defect.
    #[must_use]
    pub fn watch_is_dark(&self, entity: PersistId) -> bool {
        self.app
            .world()
            .get_resource::<WitnessState<Regolith>>()
            .is_some_and(|state| state.0.watch_is_dark(entity))
    }

    /// This peer's witness counters, or zeroes if it witnesses nobody.
    #[must_use]
    pub fn witness_counters(&self) -> orrery_witness::WitnessCounters {
        self.app
            .world()
            .get_resource::<WitnessState<Regolith>>()
            .map_or_else(Default::default, |state| state.0.counters())
    }

    /// Where this peer's inbound state packets went.
    #[must_use]
    pub fn replica_counters(&self) -> ReplicaCounters {
        *self.app.world().resource::<ReplicaCounters>()
    }

    /// Keyframe/delta traffic offered to the real send path.
    #[must_use]
    pub fn replication_wire_counters(&self) -> ReplicationWireCounters {
        self.replication_wire
    }

    /// Current sender-side audience per authored entity.
    ///
    /// Exposed only to the harness's delivery-gap observer. The returned copy
    /// cannot affect recipient selection, which remains inside
    /// [`Self::broadcast_state`].
    #[must_use]
    pub fn replication_audience_snapshot(&self) -> Vec<(PersistId, Vec<NodeId>)> {
        self.replication_audiences
            .iter()
            .map(|(entity, audience)| (*entity, audience.iter().copied().collect()))
            .collect()
    }

    /// Enable capture of the sender's audience changes for a measurement run.
    pub fn enable_presence_stats(&mut self) {
        self.audience_changes = Some(Vec::new());
    }

    /// Drain the exact audience diffs observed at [`Self::broadcast_state`].
    pub(crate) fn drain_audience_changes(&mut self) -> Vec<AudienceChange> {
        self.audience_changes
            .as_mut()
            .map_or_else(Vec::new, core::mem::take)
    }

    /// Number of receiver links currently inside a missing-newer window.
    pub(crate) fn missing_newer_anchor_count(&self) -> usize {
        self.app
            .world()
            .resource::<ReplicaKeyframes>()
            .missing_newer
            .len()
    }

    /// Number of receiver-side replicated links with an anchor to judge.
    pub(crate) fn receiver_anchor_count(&self) -> usize {
        self.app
            .world()
            .resource::<ReplicaKeyframes>()
            .anchors
            .len()
    }

    /// Record the coverage installed for this seat when the #699 trace is on.
    ///
    /// Called after the swarm has selected the cells and observes only their
    /// count. `next_tick` is zero during island formation and otherwise the
    /// tick after the just-completed roster-refresh boundary.
    pub fn trace_interest_coverage(&self, cells: usize) {
        if self.audience_trace {
            eprintln!(
                "audience_trace kind=interest_coverage seat={} next_tick={} cells={cells}",
                self.index, self.tick,
            );
        }
    }

    /// Replica entities this peer holds, tagged or not.
    #[must_use]
    pub fn replicas(&mut self) -> usize {
        let world = self.app.world_mut();
        let mut query = world.query::<&Replica>();
        query.iter(world).count()
    }

    /// Entities currently tagged as replicas of other bots' bodies.
    #[must_use]
    pub fn tracked(&mut self) -> usize {
        let world = self.app.world_mut();
        let mut query = world.query_filtered::<Entity, Or<(With<HighRate>, With<Proxy>)>>();
        query.iter(world).count()
    }
}

fn authored_cell(
    entity: PersistId,
    primary: PersistId,
    state: &RegolithState,
    player_cell: CellId,
    cell_edge_m: f32,
) -> CellId {
    if entity == primary {
        return player_cell;
    }
    cell_of(grid_of(
        &body_position(state).unwrap_or_default(),
        cell_edge_m,
    ))
}

/// Where a replicated body sits in the lattice, if it sits anywhere.
///
/// `None` for the bloom scheduler, which occupies no point — the same call the
/// shipping client's `body_position_m` makes. Every other variant is an
/// ordinary spatial body: `docs/01-spatial-model.md` says of a drifting
/// asteroid that "it is an ordinary entity in its parent grid".
fn body_position(state: &RegolithState) -> Option<QPos> {
    match state {
        RegolithState::Craft(craft) => Some(craft.pos),
        RegolithState::Rock(rock) => Some(rock.pos),
        RegolithState::Pickup(pickup) => Some(pickup.pos),
        RegolithState::BloomDirector(_) => None,
    }
}

fn is_delta_payload(payload: &[u8]) -> bool {
    orrery_protocol::channels::untag(payload).is_some_and(|(channel, body)| {
        channel == orrery_protocol::channels::Channel::State
            && body.first() == Some(&TAG_REPLICATION_DELTA)
    })
}

fn encode_delta_if_smaller(
    absolute: &(Vec<u8>, CellId, PersistId, u64),
    delta: &ReplicationDelta,
) -> Option<Vec<u8>> {
    let payload = encode_replication_delta(absolute, delta);
    is_delta_payload(&payload).then_some(payload)
}

/// A deterministic key per bot index.
#[must_use]
pub fn bot_key(index: usize) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    seed[31] = 0xB0;
    iroh_base::SecretKey::from_bytes(&seed)
}

/// The exterior *host's* transport key.
///
/// Distinct namespace from [`bot_key`] on purpose: the host endpoint's node id
/// names the hosting process, not any island slot, and the two must never
/// collide — a dialler handed the host's id while holding the slot's key would
/// otherwise find itself connecting to itself (#385 found this live).
#[must_use]
pub fn host_key() -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&u64::MAX.to_le_bytes());
    seed[31] = 0xB1;
    iroh_base::SecretKey::from_bytes(&seed)
}

/// Grid-unit position from a quantized core position.
#[must_use]
pub fn grid_of(pos: &QPos, cell_edge_m: f32) -> Vec3 {
    let (x, y, z) = pos.to_metres();
    Vec3::new(x as f32, y as f32, z as f32) / cell_edge_m
}

/// The interest cell containing a grid-unit position.
#[must_use]
pub fn cell_of(grid: Vec3) -> CellId {
    CellId::from_coords(
        glam::IVec3::new(
            grid.x.floor() as i32,
            grid.y.floor() as i32,
            grid.z.floor() as i32,
        ),
        CellId::MAX_LEVEL,
    )
    .expect("the roam stays well inside the addressable volume")
}

/// The default cell edge, so the harness and the design agree.
#[must_use]
pub fn default_cell_edge_m() -> f32 {
    DEFAULT_CELL_EDGE_M as f32
}

/// Regolith's campaign cell edge, kept separate from the P1 gate default.
#[must_use]
pub fn campaign_cell_edge_m() -> f32 {
    orrery_games::regolith::CAMPAIGN_CELL_EDGE_M as f32
}

/// Routes Regolith's own outcome deliveries through the host's adapter seam.
///
/// The rule is `Regolith::deliver`, unchanged and not re-stated here: this is
/// only the shape adaptation between the ruleset's `(recipient, order)` pair
/// and the host's named [`Delivery`]. It carries the *build this peer runs* —
/// honest or tampered — because that is the ruleset the deleted loop called
/// `self.executor.ruleset().deliver` on, and a modified authority that routed
/// by the shipping rules would be a different peer than the one this harness
/// hands out.
#[derive(Debug, Clone, Copy)]
struct BotAdapter(Regolith);

impl RulesetAdapter<Regolith> for BotAdapter {
    fn deliver(&self, event: &Outcome) -> Option<Delivery<Order>> {
        Game::deliver(&self.0, event).map(|(recipient, order)| Delivery::new(recipient, order))
    }
}

/// Build one of this bot's hosts on `rules`, at the tick it executes first.
fn new_host(
    rules: Regolith,
    seed: UniverseSeed,
    first_tick: Tick,
) -> SimulationHost<Regolith, BotAdapter> {
    SimulationHost::new(
        SimulationHostConfig::new(seed).starting_at(first_tick),
        rules,
        BotAdapter(rules),
    )
}

/// Copy `held`'s population, stamp for stamp, into a fresh host clocked at
/// `first_tick`.
///
/// See [`Bot::seat_hosts_at`] for why a driver handed its tick from outside
/// needs this at all.
fn reseat(
    held: &SimulationHost<Regolith, BotAdapter>,
    rules: Regolith,
    seed: UniverseSeed,
    first_tick: Tick,
) -> SimulationHost<Regolith, BotAdapter> {
    let mut seated = new_host(rules, seed, first_tick);
    for entity in held.backend().entities().copied().collect::<Vec<_>>() {
        let observed = held.observed_tick(entity).unwrap_or_else(|| Tick::new(0));
        if let Some(state) = held.backend().state(entity) {
            seated.install_state_observed(entity, state.clone(), observed);
        }
    }
    seated
}

/// One delivery this tick's step produced, addressed and waiting for a route.
///
/// A named record, not a `(PersistId, PersistId, Order)` triple: two of the
/// three fields are entity ids, and a driver that swaps them credits the wrong
/// author or delivers to the wrong authority.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutedDelivery {
    /// The authority whose emitted event produced this delivery.
    source: PersistId,
    /// The authority the ruleset addressed it to.
    recipient: PersistId,
    /// The order that authority is being asked to apply.
    order: Order,
}

/// The source a witness record carries for one input the tick sealed.
///
/// The host knows only whether an input was handed to it, handed to it as
/// something another authority already delivered, or produced by its own
/// adapter; this harness's log speaks `RecordSource`. `authored_seq` counts
/// only the pilot-authored inputs of this tick, which is what the composed
/// vector's own numbering counted before the seam carried provenance.
fn record_source(origin: InputOrigin, tick: u64, authored_seq: u32) -> RecordSource {
    match origin {
        InputOrigin::Inbound { from } => RecordSource::InboundEvent { from },
        // A delivery this host's own adapter kept would name the entity whose
        // event produced it. `BotTick` takes every delivery, so this peer never
        // seals one; classifying it the same way as an arrival keeps the match
        // total rather than silently defaulting.
        InputOrigin::Delivered { source } => RecordSource::InboundEvent { from: source },
        InputOrigin::Submitted => RecordSource::OwnPlayer {
            // Five authored inputs are reachable when the four-order pilot row
            // also nominates a collision. Eight leaves each tick a disjoint,
            // power-of-two source-id range.
            input_seq: (tick as u32).wrapping_mul(8).wrapping_add(authored_seq),
        },
    }
}

/// This bot's participation in one host tick.
///
/// The two halves [`Bot::step_core`] owns and the host does not: the witness
/// column recorded at S0, and where each delivery goes. *Every* delivery is
/// taken here and none is queued back into the host, because the three-way
/// choice between this peer's own inbox, one of its hosted authorities' and
/// the router is the harness's own and the seam deliberately does not make it.
#[derive(Debug)]
struct BotTick {
    /// The entity whose sealed inputs the witness log records.
    entity: PersistId,
    /// The tick being sealed, for the authored-input sequence numbering.
    tick: u64,
    /// The sealed order vector for [`Self::entity`], in applied order.
    orders: Vec<Order>,
    /// Its provenance, one source per order, in the same total order.
    sources: Vec<RecordSource>,
    /// How many pilot-authored inputs have been classified this tick.
    authored_seq: u32,
    /// Deliveries taken for this driver to route.
    routed: Vec<RoutedDelivery>,
}

impl BotTick {
    fn new(entity: PersistId, tick: u64) -> Self {
        Self {
            entity,
            tick,
            orders: Vec::new(),
            sources: Vec::new(),
            authored_seq: 0,
            routed: Vec::new(),
        }
    }
}

impl TickParticipant<Regolith> for BotTick {
    fn sealed(&mut self, _tick: Tick, inputs: &[SealedInput<'_, Order>]) {
        for sealed in inputs {
            // The witness column is this peer's own authority's column. A
            // hosted rock's inputs are sealed by the same tick and are not in
            // it: `host_entity` explains why those entities are canonical and
            // replicated but not independently witnessed.
            if sealed.recipient() != self.entity {
                continue;
            }
            let source = record_source(sealed.origin(), self.tick, self.authored_seq);
            if matches!(sealed.origin(), InputOrigin::Submitted) {
                self.authored_seq = self.authored_seq.saturating_add(1);
            }
            self.orders.push(sealed.input().clone());
            self.sources.push(source);
        }
    }

    fn route(&mut self, source: PersistId, delivery: Delivery<Order>) -> Option<Delivery<Order>> {
        self.routed.push(RoutedDelivery {
            source,
            recipient: delivery.recipient(),
            order: delivery.into_input(),
        });
        None
    }
}

/// One event one authority emitted, kept in emission order.
///
/// A named record, not a `(PersistId, Outcome)` pair: the detector compares
/// these for equality and a swapped emitter would compare equal to nothing.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct EmittedEvent {
    /// The authority whose step emitted it.
    source: PersistId,
    /// The event itself.
    event: Outcome,
}

/// What one `step_core` call stepped and emitted.
#[cfg(test)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SteppedTick {
    /// Entities the host reports it advanced: D8's predicted set, counted.
    predicted: usize,
    /// Every event the tick raised, across every stepped authority, in
    /// emission order.
    events: Vec<EmittedEvent>,
}

/// The honest shadow's participation: watch nothing, keep nothing.
///
/// The deleted loop passed the shadow's `TickOutcome` nowhere but its
/// `state_hash`, so an event it emitted could never become an input. Dropping
/// every delivery is that, stated: the shadow's sealed vector is exactly what
/// [`Bot::step_core`] submits to it and never one order more.
#[derive(Debug, Clone, Copy)]
struct ShadowTick;

impl TickParticipant<Regolith> for ShadowTick {
    fn observes_seal(&self) -> bool {
        false
    }

    fn route(&mut self, _source: PersistId, _delivery: Delivery<Order>) -> Option<Delivery<Order>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_crowd_shares_one_turn_rate_inside_the_pairwise_aoi() {
        const PEERS: usize = 32;
        let turns: BTreeSet<i32> = (0..PEERS)
            .map(|slot| {
                ((CRUISE_MPS / orbit_radius_m(slot, PEERS)) / TICK_HZ as f64 * 1_000_000.0) as i32
            })
            .collect();
        assert_eq!(
            turns,
            BTreeSet::from([213]),
            "different integer turn rates shear neighboring campaign slots apart over the hour"
        );

        let poses: Vec<QPos> = (0..PEERS).map(|slot| spawn_pose(slot, PEERS).0).collect();
        let diameter_mm = poses
            .iter()
            .enumerate()
            .flat_map(|(slot, position)| {
                poses
                    .iter()
                    .skip(slot + 1)
                    .map(move |other| distance_mm(*position, *other))
            })
            .max()
            .expect("the campaign crowd has more than one peer");
        let pairwise_aoi_mm = (orrery_games::regolith::campaign_engagement_budget_m(
            orrery_games::regolith::CAMPAIGN_CELL_EDGE_M,
        ) * 1_000.0) as u128;
        assert!(
            diameter_mm <= pairwise_aoi_mm,
            "campaign diameter {diameter_mm} mm exceeds pairwise AOI {pairwise_aoi_mm} mm"
        );
    }

    #[test]
    fn boundary_return_window_measures_returns_in_a_sliding_second() {
        let mut window = BoundaryReturnWindow::default();

        window.record(0);
        window.record(20);
        window.record(59);
        assert_eq!(window.max_returns, 3);

        window.record(60);
        assert_eq!(
            window.recent.len(),
            3,
            "the half-open one-second window must evict a return exactly 60 ticks old"
        );
    }

    fn replication_test_bot(index: usize) -> Bot {
        Bot::new(BotSpec {
            index,
            count: 32,
            seed: UniverseSeed([0xA1; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: false,
            cheat: None,
            enforcing: false,
        })
    }

    #[test]
    fn input_adaptation_targets_authored_thrust_after_a_delivered_thrust() {
        let spec = BotSpec {
            index: 1,
            count: 32,
            seed: UniverseSeed([0xA1; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: true,
            cheat: None,
            enforcing: false,
        };
        let mut bot = Bot::new(spec);
        assert_eq!(
            bot.profile,
            Profile::Idle,
            "the fixture needs zero shaped thrust"
        );
        let turn_urad = bot.turn_urad.abs();
        let delivered = Order::Thrust {
            accel_mmss: 7_777,
            yaw_urad: 8_888,
            pitch_urad: 9_999,
        };
        bot.inject_delivered(PersistId::new(99), delivered.clone());

        bot.step_core(0, spec.cell_edge_m);

        let authored = bot
            .chain
            .as_mut()
            .expect("witnessing installed the live producer")
            .cut_frame(u64::from(FRAME_TICKS) - 1)
            .expect("the frame interval closes");
        let records = &authored.frame.entities[0].records;
        assert!(matches!(
            records[0].source,
            RecordSource::InboundEvent { from } if from == PersistId::new(99)
        ));
        assert_eq!(
            Order::decode(&records[0].payload).expect("delivered order decodes"),
            delivered,
            "input adaptation must not rewrite a preceding delivered Thrust"
        );
        assert!(matches!(records[1].source, RecordSource::OwnPlayer { .. }));
        let Order::Thrust {
            accel_mmss,
            yaw_urad,
            pitch_urad,
        } = Order::decode(&records[1].payload).expect("authored order decodes")
        else {
            panic!("the pilot's first authored order must be Thrust");
        };
        assert_eq!(
            accel_mmss, 0,
            "the idle profile shapes authored acceleration"
        );
        assert_eq!(
            yaw_urad.abs(),
            turn_urad,
            "the orbit rate shapes authored yaw"
        );
        assert_eq!(pitch_urad, 0, "the planar swarm shapes authored pitch");
    }

    /// A snapshot-built peer really starts with no replicas.
    ///
    /// The late-join clause reports `initial_replicas` and refuses to pass
    /// when it is non-zero, but that guard reads a number this constructor
    /// produces. Replacing the measurement with a literal `0` left the whole
    /// suite green -- the clause was asserting on its own input rather than
    /// on the world. The freshness has to be proved where it is created.
    #[test]
    fn a_snapshot_built_peer_starts_with_no_replicas() {
        let spec = BotSpec {
            index: 3,
            count: 32,
            seed: UniverseSeed([0xA1; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: false,
            cheat: None,
            enforcing: false,
        };
        let craft = Craft::spawned(
            orrery_games::regolith::archetype::Archetype::Interceptor,
            orrery_core::QPos::default(),
            0,
        );
        let cell = cell_of(grid_of(&craft.pos, spec.cell_edge_m));

        let mut fresh = Bot::from_craft_snapshot(spec, craft, cell);
        assert_eq!(
            fresh.replicas(),
            0,
            "a peer built from a craft snapshot carries no receive history; \
             a long-lived bot would bring replicas that arrived before the join"
        );
        assert_eq!(
            fresh.cell(),
            Some(cell),
            "and it keeps the committed cell the snapshot was taken at"
        );
    }

    #[test]
    fn swept_interest_cells_use_the_committed_cell_offset_and_canonical_velocity() {
        let mut bot = replication_test_bot(0);
        let cell = bot.cell().expect("committed cell");
        let (coords, _) = cell.coords();
        let edge = f64::from(default_cell_edge_m());
        let mut craft = bot.craft().clone();
        craft.pos = QPos::from_metres(
            f64::from(coords.x) * edge + edge - 1.0,
            f64::from(coords.y) * edge + edge / 2.0,
            f64::from(coords.z) * edge + edge / 2.0,
        );
        craft.vel = orrery_core::QVel::from_metres_per_sec(64.0, 0.0, 0.0);
        bot.replace_craft_for_test(craft);

        let baseline = cell.neighbors27();
        let swept = bot.swept_interest_cells(1.0);
        assert!(
            baseline.iter().all(|candidate| swept.contains(candidate)),
            "#692's primitive may widen but never narrow the baseline AOI"
        );
        assert!(
            swept.len() > baseline.len(),
            "a craft one metre from the positive-x boundary and moving 64 m/s \
             must add the next neighborhood face"
        );
    }

    fn audience_entry(sender: &mut Bot, peer: NodeId) -> PeerEntry {
        PeerEntry {
            node: peer,
            cells: sender.cell().expect("committed").neighbors27(),
        }
    }

    fn drain_replication(bot: &mut Bot) -> Vec<(NodeId, Vec<u8>)> {
        bot.drain_outbound()
            .into_iter()
            .filter_map(|(peer, stream, payload)| {
                if stream.is_some() {
                    return None;
                }
                let (channel, inner) = orrery_net::channels::untag(&payload)?;
                (channel == Channel::State).then(|| (peer, inner.to_vec()))
            })
            .collect()
    }

    fn step_and_send(bot: &mut Bot, tick: u64) -> Vec<(NodeId, Vec<u8>)> {
        bot.step_core(tick, default_cell_edge_m());
        if tick % 3 == 2 {
            bot.broadcast_state(tick);
        }
        bot.update();
        drain_replication(bot)
    }

    #[test]
    fn a_delta_stream_reconstructs_the_same_replica_states_as_the_snapshot_stream() {
        let mut delta_sender = replication_test_bot(19);
        let mut snapshot_control = replication_test_bot(19);
        let receiver = bot_key(31).public();
        delta_sender.link(receiver, 1_200);
        let receiver_entry = audience_entry(&mut delta_sender, receiver);
        delta_sender.set_island(vec![receiver_entry]);
        let mut keyframes = ReplicaKeyframes::default();

        for tick in 0..180 {
            let sent = step_and_send(&mut delta_sender, tick);
            snapshot_control.step_core(tick, default_cell_edge_m());
            snapshot_control.update();
            if tick % 3 != 2 {
                assert!(sent.is_empty());
                continue;
            }

            let control_canonical = snapshot_control.state().to_canonical();
            let control_payload = encode_replication_compressed(&(
                control_canonical,
                snapshot_control.cell().expect("committed"),
                snapshot_control.entity(),
                tick + 1,
            ));
            let (control, ..) =
                orrery_net::channels::decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(
                    &control_payload,
                )
                .expect("full-snapshot control decodes");
            assert_eq!(sent.len(), 1, "moving craft emits one state per send");
            let reconstructed = decode_replica(&sent[0].1, &mut keyframes)
                .expect("the receiver reconstructs every authored state");
            assert_eq!(
                reconstructed.canonical, control,
                "receiver trajectory diverged from the same-seed full-snapshot control at tick {tick}"
            );
        }
    }

    #[test]
    fn a_newly_interested_peer_receives_a_keyframe_before_any_delta() {
        let mut sender = replication_test_bot(19);
        let established = bot_key(30).public();
        let joining = bot_key(31).public();
        sender.link(established, 1_200);
        sender.link(joining, 1_200);
        let established_entry = audience_entry(&mut sender, established);
        sender.set_island(vec![established_entry]);
        for tick in 0..3 {
            let _ = step_and_send(&mut sender, tick);
        }

        let established_entry = audience_entry(&mut sender, established);
        let joining_entry = audience_entry(&mut sender, joining);
        sender.set_island(vec![established_entry, joining_entry]);
        let mut first_joiner_payloads = Vec::new();
        for tick in 3..6 {
            first_joiner_payloads.extend(
                step_and_send(&mut sender, tick)
                    .into_iter()
                    .filter(|(peer, _)| *peer == joining)
                    .map(|(_, payload)| payload),
            );
        }
        assert_eq!(
            first_joiner_payloads.len(),
            1,
            "a late joiner receives only its 27-cell neighborhood, but a newly interested peer did not receive exactly one immediate anchor"
        );
        assert!(
            orrery_net::channels::decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(
                &first_joiner_payloads[0]
            )
            .is_some(),
            "the first packet to a newly interested peer must be a keyframe"
        );
        assert!(!is_delta_payload(&first_joiner_payloads[0]));

        let next = (6..9)
            .flat_map(|tick| step_and_send(&mut sender, tick))
            .find(|(peer, _)| *peer == joining)
            .expect("the joining peer receives the following update");
        assert!(is_delta_payload(&next.1));
    }

    #[test]
    fn presence_stats_are_deterministic_over_a_boundary_join_with_an_existing_audience() {
        let measure = || {
            let mut sender = replication_test_bot(19);
            let first = bot_key(29).public();
            let second = bot_key(30).public();
            let crossing_joiner = bot_key(31).public();
            sender.enable_presence_stats();
            for peer in [first, second, crossing_joiner] {
                sender.link(peer, 1_200);
            }

            // This is the post-crossing roster shape: two peers already see
            // the author and one peer has just crossed into its audience.
            // The diff must be one, not the resulting audience size of three.
            let first_entry = audience_entry(&mut sender, first);
            let second_entry = audience_entry(&mut sender, second);
            sender.set_island(vec![first_entry, second_entry]);
            sender.broadcast_state(2);
            sender.update();
            let _ = sender.drain_outbound();
            let _ = sender.drain_audience_changes();

            let first_entry = audience_entry(&mut sender, first);
            let second_entry = audience_entry(&mut sender, second);
            let joiner_entry = audience_entry(&mut sender, crossing_joiner);
            sender.set_island(vec![first_entry, second_entry, joiner_entry]);
            sender.broadcast_state(5);
            sender.update();
            sender.drain_audience_changes()
        };

        let first = measure();
        let second = measure();
        assert_eq!(first, second, "one boundary-join seed must be reproducible");
        assert_eq!(first.len(), 1, "the changed audience yields one event");
        assert_eq!(first[0].joins, 1, "one crossing peer joined the audience");
        assert_eq!(first[0].leaves, 0, "the crossing did not remove a peer");
    }

    #[test]
    fn a_hitch_retries_the_new_subscribers_immediate_keyframe() {
        let mut sender = replication_test_bot(19);
        let established = bot_key(30).public();
        let joining = bot_key(31).public();
        sender.link(established, 1_200);
        sender.link(joining, 1_200);
        let established_entry = audience_entry(&mut sender, established);
        sender.set_island(vec![established_entry]);
        for tick in 0..3 {
            let _ = step_and_send(&mut sender, tick);
        }

        let established_entry = audience_entry(&mut sender, established);
        let joining_entry = audience_entry(&mut sender, joining);
        sender.set_island(vec![established_entry, joining_entry]);
        for tick in 3..6 {
            sender.step_core(tick, default_cell_edge_m());
            if tick % 3 == 2 {
                sender.broadcast_state(tick);
            }
            sender.update();
            if tick % 3 == 2 {
                sender.stall();
            }
        }

        let first_after_hitch = (6..9)
            .flat_map(|tick| step_and_send(&mut sender, tick))
            .find(|(peer, _)| *peer == joining)
            .expect("the newly interested peer is retried after the hitch");
        assert!(
            orrery_net::channels::decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(
                &first_after_hitch.1
            )
            .is_some(),
            "the retried first packet to a new subscriber must still be a keyframe"
        );
    }

    #[test]
    fn a_shed_or_lost_delta_is_fully_superseded_by_the_next() {
        let mut sender = replication_test_bot(19);
        let receiver = bot_key(31).public();
        sender.link(receiver, 1_200);
        let receiver_entry = audience_entry(&mut sender, receiver);
        sender.set_island(vec![receiver_entry]);

        let mut updates = Vec::new();
        let mut expected = Vec::new();
        for tick in 0..9 {
            let sent = step_and_send(&mut sender, tick);
            if tick % 3 == 2 {
                assert_eq!(sent.len(), 1);
                updates.push(sent[0].1.clone());
                expected.push(sender.state().to_canonical());
            }
        }
        assert!(!is_delta_payload(&updates[0]), "first send is an anchor");
        assert!(
            is_delta_payload(&updates[1]),
            "arbitrary dropped send is a delta"
        );
        assert!(
            is_delta_payload(&updates[2]),
            "following send is still before the keyframe"
        );

        let mut keyframes = ReplicaKeyframes::default();
        let anchor = decode_replica(&updates[0], &mut keyframes).expect("anchor decodes");
        assert_eq!(anchor.canonical, expected[0]);
        // `updates[1]` is lost or shed. Applying the following delta directly
        // to the retained keyframe must nevertheless produce the full state.
        let converged = decode_replica(&updates[2], &mut keyframes)
            .expect("the following delta remains independently decodable");
        assert_eq!(converged.canonical, expected[2]);
    }

    #[test]
    fn unanchored_delta_causes_distinguish_missing_from_superseded_anchors() {
        let entity = PersistId::new(7);
        let cell = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).expect("origin cell");
        let canonical = vec![0; 128];
        let keyframe =
            |tick: u64| encode_replication_compressed(&(canonical.clone(), cell, entity, tick));
        let delta = |tick: u64, referenced_tick: u64| {
            let payload = encode_replication_delta(
                &(canonical.clone(), cell, entity, tick),
                &ReplicationDelta {
                    entity,
                    tick,
                    keyframe_age: u16::try_from(tick - referenced_tick).expect("small age"),
                    cell: None,
                    patch: encode_delta_patch(&canonical, &canonical),
                },
            );
            assert!(
                is_delta_payload(&payload),
                "fixture must exercise the delta decoder"
            );
            payload
        };

        let mut none = ReplicaKeyframes::default();
        assert_eq!(
            decode_replica(&delta(23, 20), &mut none),
            Err(ReplicaDecodeError::UnanchoredDelta(
                UnanchoredDeltaCause::NoKeyframe
            ))
        );

        let mut older = ReplicaKeyframes::default();
        decode_replica(&keyframe(20), &mut older).expect("older anchor installs");
        assert_eq!(
            decode_replica(&delta(43, 40), &mut older),
            Err(ReplicaDecodeError::UnanchoredDelta(
                UnanchoredDeltaCause::MissingNewerKeyframe
            ))
        );

        let mut newer = ReplicaKeyframes::default();
        decode_replica(&keyframe(40), &mut newer).expect("newer anchor installs");
        assert_eq!(
            decode_replica(&delta(23, 20), &mut newer),
            Err(ReplicaDecodeError::UnanchoredDelta(
                UnanchoredDeltaCause::SupersededKeyframe
            ))
        );

        let invalid = ReplicationDelta {
            entity,
            tick: 1,
            keyframe_age: 2,
            cell: None,
            patch: encode_delta_patch(&canonical, &canonical),
        };
        let invalid = encode_replication_delta(&(canonical, cell, entity, 1), &invalid);
        assert_eq!(
            decode_replica(&invalid, &mut newer),
            Err(ReplicaDecodeError::UnanchoredDelta(
                UnanchoredDeltaCause::InvalidReference
            ))
        );
    }

    #[test]
    fn a_simulated_hitch_reanchors_after_discarding_a_keyframe() {
        let mut sender = replication_test_bot(19);
        let receiver = bot_key(31).public();
        sender.link(receiver, 1_200);
        let receiver_entry = audience_entry(&mut sender, receiver);
        sender.set_island(vec![receiver_entry]);

        for tick in 0..3 {
            sender.step_core(tick, default_cell_edge_m());
            if tick % 3 == 2 {
                sender.broadcast_state(tick);
            }
            sender.update();
            if tick % 3 == 2 {
                sender.stall();
            }
        }

        let wire = sender.replication_wire_counters();
        assert_eq!(wire.keyframes_discarded_while_stalled, 1);
        assert_eq!(wire.deltas_discarded_while_stalled, 0);

        let first_after_hitch = (3..6)
            .flat_map(|tick| step_and_send(&mut sender, tick))
            .find(|(peer, _)| *peer == receiver)
            .expect("the sender resumes on its next send opportunity");
        assert!(
            orrery_net::channels::decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(
                &first_after_hitch.1
            )
            .is_some(),
            "the first post-hitch packet must reanchor after a discarded keyframe"
        );
    }

    #[test]
    fn a_simulated_hitch_restores_the_last_delivered_anchor() {
        let mut sender = replication_test_bot(19);
        sender.set_keyframe_every_sends(2);
        let receiver = bot_key(31).public();
        sender.link(receiver, 1_200);
        let receiver_entry = audience_entry(&mut sender, receiver);
        sender.set_island(vec![receiver_entry]);
        let mut receiver_keyframes = ReplicaKeyframes::default();

        let first = (0..3)
            .flat_map(|tick| step_and_send(&mut sender, tick))
            .find(|(peer, _)| *peer == receiver)
            .expect("initial keyframe leaves the app");
        decode_replica(&first.1, &mut receiver_keyframes).expect("initial keyframe anchors");

        let delta = (3..6)
            .flat_map(|tick| step_and_send(&mut sender, tick))
            .find(|(peer, _)| *peer == receiver)
            .expect("the next state send leaves the app");
        decode_replica(&delta.1, &mut receiver_keyframes).expect("pre-hitch delta decodes");

        for tick in 6..9 {
            sender.step_core(tick, default_cell_edge_m());
            if tick % 3 == 2 {
                sender.broadcast_state(tick);
            }
            sender.update();
            if tick % 3 == 2 {
                sender.stall();
            }
        }
        assert_eq!(
            sender
                .replication_wire_counters()
                .keyframes_discarded_while_stalled,
            1,
            "the guarded stage must discard a replacement keyframe"
        );

        let resumed = (9..12)
            .flat_map(|tick| step_and_send(&mut sender, tick))
            .find(|(peer, _)| *peer == receiver)
            .expect("the sender resumes on its next send opportunity");
        decode_replica(&resumed.1, &mut receiver_keyframes)
            .expect("the first post-hitch delta must reference the last delivered anchor");
    }

    #[test]
    fn a_size_fallback_waits_for_the_staggered_keyframe_slot() {
        let entity = PersistId::new(7);
        let cell = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).expect("origin cell");
        let keyframe = vec![0; 128];
        let current = vec![1; 128];
        let absolute = (current.clone(), cell, entity, 6);
        let delta = ReplicationDelta {
            entity,
            tick: 6,
            keyframe_age: 3,
            cell: None,
            patch: encode_delta_patch(&keyframe, &current),
        };
        assert!(
            encode_delta_if_smaller(&absolute, &delta).is_none(),
            "the codec selected an absolute fallback; an off-phase sender must wait instead of bypassing the keyframe stagger"
        );
    }

    /// The loop A18 S6.c deleted from [`Bot::step_core`], kept verbatim here
    /// and nowhere else.
    ///
    /// This is a *reference column*, not a second implementation: every line
    /// below was in `step_core` before the conversion and is reproduced
    /// unchanged over its own [`Executor`], down to the two `ruleset().deliver`
    /// folds, the tick-boundary inbox freeze, the honest shadow's second
    /// execution and the order in which the witness log is fed. The
    /// convergence criterion for this stage is behavioural identity — the
    /// point is deleting a duplicated loop, not changing what it does — and in
    /// this crate a moved byte is a moved P4 golden, so the identity is
    /// detected rather than argued.
    struct ReferenceColumn {
        executor: Executor<Regolith>,
        honest_shadow: Option<Executor<Regolith>>,
        chain: Option<InputLogProducer>,
        delivered_inbox: Vec<(PersistId, Order)>,
        hosted_inbox: BTreeMap<PersistId, Vec<(PersistId, Order)>>,
        hosted: BTreeSet<PersistId>,
        delivered_outbox: Vec<(PersistId, PersistId, Order)>,
        replica_seen_at: BTreeMap<PersistId, u64>,
        replica_authorities: BTreeMap<PersistId, NodeId>,
        first_tampered_tick: Option<u64>,
        resolved_shots: Option<Vec<Outcome>>,
        entity: PersistId,
        seed: UniverseSeed,
        slot: u64,
        profile: Profile,
        turn_urad: i32,
        accel_mmss: i32,
        tamper: Option<Tamper>,
        last_step: SteppedTick,
    }

    impl ReferenceColumn {
        /// Start a reference column from the converged bot's own initial
        /// population, so the two differ in nothing but which code steps them.
        fn mirroring(bot: &Bot, index: usize) -> Self {
            let rules = bot.tamper.map_or_else(Regolith::honest, Regolith::cheating);
            let mut executor = Executor::new(rules, bot.seed);
            let mut honest_shadow = bot
                .tamper
                .map(|_| Executor::new(Regolith::honest(), bot.seed));
            for entity in bot.host.backend().entities().copied().collect::<Vec<_>>() {
                let state = bot
                    .host
                    .backend()
                    .state(entity)
                    .expect("the population it just listed")
                    .clone();
                executor.insert(entity, state.clone());
                if let Some(shadow) = &mut honest_shadow {
                    shadow.insert(entity, state);
                }
            }
            Self {
                executor,
                honest_shadow,
                chain: bot.chain.is_some().then(|| {
                    InputLogProducer::new(
                        bot_key(index),
                        bot.entity,
                        REGOLITH_RULESET,
                        0,
                        CLAIM_EVERY,
                        FRAME_TICKS,
                    )
                }),
                delivered_inbox: Vec::new(),
                hosted_inbox: bot.hosted_inbox.clone(),
                hosted: bot.hosted.clone(),
                delivered_outbox: Vec::new(),
                replica_seen_at: BTreeMap::new(),
                replica_authorities: BTreeMap::new(),
                first_tampered_tick: None,
                resolved_shots: bot.resolved_shots.clone(),
                entity: bot.entity,
                seed: bot.seed,
                slot: bot.slot,
                profile: bot.profile,
                turn_urad: bot.turn_urad,
                accel_mmss: bot.accel_mmss,
                tamper: bot.tamper,
                last_step: SteppedTick::default(),
            }
        }

        fn craft(&self) -> &Craft {
            let RegolithState::Craft(craft) = self.executor.state(self.entity).expect("seeded")
            else {
                unreachable!("a swarm bot always authors a craft")
            };
            craft
        }

        fn speed_mps(&self) -> f64 {
            let (vx, vy, vz) = self.craft().vel.to_metres_per_sec();
            libm::sqrt(vx * vx + vy * vy + vz * vz)
        }

        fn authors(&self, entity: PersistId) -> bool {
            entity == self.entity || self.hosted.contains(&entity)
        }

        fn host_entity(&mut self, entity: PersistId, state: RegolithState) {
            self.executor.insert(entity, state.clone());
            if let Some(shadow) = &mut self.honest_shadow {
                shadow.insert(entity, state);
            }
            self.hosted.insert(entity);
            self.hosted_inbox.insert(entity, Vec::new());
        }

        // ── the deleted loop, verbatim ────────────────────────────────────
        fn step_core(&mut self, tick: u64) {
            let at = Tick::new(tick);
            let stale: Vec<_> = self
                .replica_seen_at
                .iter()
                .filter_map(|(entity, seen)| {
                    (tick.saturating_sub(*seen) > REPLICA_TTL_TICKS).then_some(*entity)
                })
                .collect();
            for entity in stale {
                self.replica_seen_at.remove(&entity);
                self.replica_authorities.remove(&entity);
                self.executor.take_state(entity);
                if let Some(shadow) = &mut self.honest_shadow {
                    shadow.take_state(entity);
                }
            }
            let hosted_at_boundary: Vec<PersistId> = self.hosted.iter().copied().collect();
            let mut hosted_delivered: BTreeMap<PersistId, Vec<(PersistId, Order)>> =
                hosted_at_boundary
                    .iter()
                    .map(|entity| {
                        (
                            *entity,
                            self.hosted_inbox.remove(entity).unwrap_or_default(),
                        )
                    })
                    .collect();
            let mut rng = tick_rng(self.seed, self.entity, at);
            let delivered = core::mem::take(&mut self.delivered_inbox);
            let mut orders: Vec<_> = delivered.iter().map(|(_, order)| order.clone()).collect();
            let mut sources: Vec<_> = delivered
                .into_iter()
                .map(|(from, _)| RecordSource::InboundEvent { from })
                .collect();
            orders.reserve(4);
            let authored_start = orders.len();
            orrery_games::regolith::pilot::honest_orders(
                self.entity,
                self.slot,
                at,
                &mut rng,
                &mut orders,
            );
            let collision_candidate = self.executor.state(self.entity).and_then(|own| {
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
            });
            if let Some(other) = collision_candidate {
                orders.push(Order::Collide { other });
            }
            let authored = orders.len().saturating_sub(sources.len());
            sources.extend((0..authored).map(|seq| RecordSource::OwnPlayer {
                input_seq: (tick as u32).wrapping_mul(8).wrapping_add(seq as u32),
            }));
            let speed = self.speed_mps();
            let profile = self.profile;
            let turn_urad = self.turn_urad;
            let full_accel = self.accel_mmss;
            let speed_probe = self.tamper == Some(Tamper::SpeedMultiplier);
            for order in orders.iter_mut().skip(authored_start) {
                if let Order::Thrust {
                    accel_mmss,
                    yaw_urad,
                    pitch_urad,
                } = order
                {
                    *pitch_urad = 0;
                    *accel_mmss = if speed_probe {
                        full_accel
                    } else {
                        profile.accel_mmss(tick, speed, *accel_mmss, CRUISE_MPS)
                    };
                    *yaw_urad = yaw_urad.signum() * turn_urad.abs();
                }
            }
            if let Some(chain) = &mut self.chain {
                chain.log_inputs_with_sources(tick, &orders, &sources);
            }
            let mut emitted = Vec::new();
            let mut predicted = 0_usize;
            let outcome = self
                .executor
                .step_entity(self.entity, at, &orders)
                .expect("entity present");
            predicted += 1;
            emitted.extend(outcome.events.iter().map(|event| EmittedEvent {
                source: self.entity,
                event: event.clone(),
            }));
            if let Some(chain) = &mut self.chain {
                chain.log_neighbor_frames(tick, &outcome.neighbor_frames);
            }
            if let Some(resolved_shots) = &mut self.resolved_shots {
                resolved_shots.extend(
                    outcome
                        .events
                        .iter()
                        .filter(|event| matches!(event, Outcome::ShotResolved { .. }))
                        .cloned(),
                );
            }
            for event in &outcome.events {
                if let Some((recipient, order)) = self.executor.ruleset().deliver(event) {
                    if recipient == self.entity {
                        self.delivered_inbox.push((self.entity, order));
                    } else if self.hosted.contains(&recipient) {
                        self.hosted_inbox
                            .entry(recipient)
                            .or_default()
                            .push((self.entity, order));
                    } else {
                        self.delivered_outbox.push((self.entity, recipient, order));
                    }
                }
            }
            if let Some(shadow) = &mut self.honest_shadow {
                let honest = shadow
                    .step_entity(self.entity, at, &orders)
                    .expect("entity present");
                if self.first_tampered_tick.is_none() && honest.state_hash != outcome.state_hash {
                    self.first_tampered_tick = Some(tick);
                }
            }
            if let Some(chain) = &mut self.chain {
                chain.log_tick_hash(outcome.state_hash);
            }

            let mut materialized = Vec::new();
            for entity in hosted_at_boundary {
                let delivered = hosted_delivered.remove(&entity).unwrap_or_default();
                let orders: Vec<Order> = delivered.iter().map(|(_, order)| order.clone()).collect();
                let outcome = self
                    .executor
                    .step_entity(entity, at, &orders)
                    .expect("hosted campaign authority remains installed");
                predicted += 1;
                emitted.extend(outcome.events.iter().map(|event| EmittedEvent {
                    source: entity,
                    event: event.clone(),
                }));
                for event in &outcome.events {
                    if let Some((recipient, order)) = self.executor.ruleset().deliver(event) {
                        if recipient == self.entity {
                            self.delivered_inbox.push((entity, order));
                        } else if self.hosted.contains(&recipient) {
                            self.hosted_inbox
                                .entry(recipient)
                                .or_default()
                                .push((entity, order));
                        } else {
                            self.delivered_outbox.push((entity, recipient, order));
                        }
                    }
                }
                materialized.extend(outcome.materialized);
                self.hosted_inbox.entry(entity).or_default();
            }
            for entity in materialized {
                if !self.authors(entity) {
                    let state = self
                        .executor
                        .state(entity)
                        .expect("materialization installed its canonical state")
                        .clone();
                    self.host_entity(entity, state);
                }
            }
            self.last_step = SteppedTick {
                predicted,
                events: emitted,
            };
        }
    }

    /// A18 S6.c's own detector: the loop [`Bot::step_core`] deleted, run beside
    /// the seam call that replaced it.
    ///
    /// `gates/p1-swarm` is inside `PIPELINE_TREES` (`scripts/p4-ledger.sh`), so
    /// a behaviour change here does not merely move a golden — it changes what
    /// the P4 gate measures and would make every banked bot hour an hour of a
    /// different pipeline. The bar is therefore identity, every tick, and this
    /// is what checks it. Both columns run from the same seed and spec, over
    /// the same hosted campaign rocks, the same injected deliveries, the same
    /// replica refreshes and the same refresh *stop* (so the staleness sweep
    /// fires in both), and each tick they must agree on:
    ///
    /// - the canonical bytes of **every** entity either column holds — the
    ///   player craft, each hosted rock, each materialized child and the
    ///   frozen replica;
    /// - the predicted set's size;
    /// - the emitted event vector, with its emitter, *and its order*;
    /// - the `(author, recipient, order)` products handed to the router, in
    ///   order, and the two authored inboxes those products did not go to;
    /// - the witness column, compared as the signed frames it cuts — which
    ///   folds the input records with their sources, the neighbour records
    ///   with their observation stamps and read bytes, and the tick hashes.
    ///
    /// A control executor stepping only the player craft proves the freeze is
    /// not vacuous: the replica the prediction set does not name never moves,
    /// and the run asserts it really stepped, emitted, delivered, routed to
    /// the wire, sealed a delivered input, materialized a child and cut a
    /// frame.
    fn bot_convergence_run(cheat: Option<Tamper>) {
        const INDEX: usize = 3;
        const TICKS: u64 = 400;
        /// The ingest leg refreshes the replica on this cadence, until it
        /// stops — after which the staleness sweep must fire in both columns.
        const REFRESH_EVERY: u64 = 30;
        const REFRESH_UNTIL: u64 = 200;
        /// A remote authority delivers to this peer's craft on this cadence.
        const DELIVER_EVERY: u64 = 17;
        /// And to one of the rocks it hosts on this one, hard enough to split
        /// it — which is what makes the materialization column non-vacuous.
        const MINE_EVERY: u64 = 23;

        let spec = BotSpec {
            index: INDEX,
            count: 8,
            seed: UniverseSeed([0x6c; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: true,
            cheat,
            enforcing: true,
        };
        let mut bot = Bot::new(spec);
        bot.enable_resolved_shot_capture();
        let rocks: Vec<PersistId> = orrery_games::regolith::campaign_rock_seeds(spec.seed, 1)
            .into_iter()
            .map(|seeded| {
                bot.host_entity(seeded.entity, RegolithState::Rock(seeded.rock));
                seeded.entity
            })
            .collect();
        let mut reference = ReferenceColumn::mirroring(&bot, INDEX);

        // A neighbour placed inside the craft's collision radius and closing
        // on it, with a velocity of its own. Both halves are load-bearing:
        // Regolith reads a neighbour only to verify a claim its own input
        // named, so without a live collision candidate the neighbour-frame
        // column — the half of this conversion that was hardest to keep —
        // would be empty on every tick and prove nothing; and a body at rest
        // stays put whether the seam steps it or not, which would make the
        // freeze assertion vacuous.
        let remote = PersistId::new(97);
        let remote_authority = bot_key(41).public();
        let replica = {
            let own = bot.craft().clone();
            let RegolithState::Craft(mut craft) = Regolith::honest().spawn(remote, 1) else {
                panic!("a spawned craft is a craft");
            };
            craft.pos = QPos {
                x: own.pos.x + 1_000,
                y: own.pos.y,
                z: own.pos.z,
            };
            craft.vel = orrery_core::QVel {
                x: -12_000,
                y: -4_000,
                z: 2_500,
            };
            RegolithState::Craft(craft)
        };
        let installed_replica = replica.to_canonical();

        // The control: the same replica in an executor that is never asked to
        // step it either, so "frozen" is compared against a body that *can*
        // move rather than against a constant.
        let mut control = Executor::new(Regolith::honest(), spec.seed);
        control.insert(remote, replica.clone());

        let mut steps = 0_usize;
        let mut events_seen = 0_usize;
        let mut wired = 0_usize;
        let mut inboxed = 0_usize;
        let mut frames_cut = 0_usize;
        let mut materializations = 0_usize;
        let mut neighbour_reads = 0_usize;
        let mut expiries = 0_usize;

        for tick in 0..TICKS {
            // `apply_replicas`' ingest, as the receive seam leaves it: a
            // decoded claim installed verbatim under its observation tick.
            if tick <= REFRESH_UNTIL && tick.is_multiple_of(REFRESH_EVERY) {
                bot.replica_authorities
                    .entry(remote)
                    .or_insert(remote_authority);
                bot.host
                    .install_state_observed(remote, replica.clone(), Tick::new(tick));
                bot.replica_seen_at.insert(remote, tick);
                reference
                    .replica_authorities
                    .entry(remote)
                    .or_insert(remote_authority);
                reference
                    .executor
                    .insert_observed(remote, replica.clone(), Tick::new(tick));
                reference.replica_seen_at.insert(remote, tick);
            }
            // A remote authority's delivery, as `receive` leaves it.
            if tick.is_multiple_of(DELIVER_EVERY) {
                let order = Order::Lock { target: remote };
                bot.delivered_inbox.push((remote, order.clone()));
                reference.delivered_inbox.push((remote, order));
            }
            // And one addressed to a rock this peer hosts, which is the arm
            // that steps a second authority and materializes its children.
            if tick.is_multiple_of(MINE_EVERY) {
                // From this peer's *own* craft, which is what the rules do
                // when it fires at a rock it hosts: the rock's credit reply
                // is then addressed back to an authority this peer holds, so
                // the self-queue arm of the routing decision is exercised
                // rather than argued.
                let order = mining_damage(bot.entity, bot.craft());
                bot.hosted_inbox
                    .entry(rocks[0])
                    .or_default()
                    .push((bot.entity, order.clone()));
                reference
                    .hosted_inbox
                    .entry(rocks[0])
                    .or_default()
                    .push((bot.entity, order));
            }

            let before = bot.host.backend().entities().count();
            bot.publish_claim(tick);
            let reference_claim = reference
                .chain
                .as_mut()
                .zip(reference.executor.state(reference.entity).cloned())
                .and_then(|(chain, state)| chain.cut_claim(tick, &state));
            let _ = reference_claim;

            bot.step_core(tick, spec.cell_edge_m);
            reference.step_core(tick);

            // ── the comparison, every tick ───────────────────────────────
            assert_eq!(
                bot.last_step.predicted, reference.last_step.predicted,
                "the predicted set at tick {tick} must be the set the loop stepped"
            );
            assert_eq!(
                bot.last_step.events, reference.last_step.events,
                "emitted events must match in emitter, value and order at tick {tick}"
            );
            let converged_population: Vec<PersistId> =
                bot.host.backend().entities().copied().collect();
            let reference_population: Vec<PersistId> =
                reference.executor.entities().copied().collect();
            assert_eq!(
                converged_population, reference_population,
                "the two columns must hold the same population at tick {tick}"
            );
            for entity in &converged_population {
                assert_eq!(
                    bot.host.state_bytes(*entity),
                    reference
                        .executor
                        .state(*entity)
                        .map(CoreCodec::to_canonical),
                    "canonical state for {entity:?} diverged at tick {tick}"
                );
            }
            assert_eq!(
                bot.hosted, reference.hosted,
                "the set of authorities this peer holds must match at tick {tick}"
            );
            assert_eq!(
                bot.hosted_inbox, reference.hosted_inbox,
                "each hosted authority's inbox must match at tick {tick}"
            );
            assert_eq!(
                bot.delivered_inbox, reference.delivered_inbox,
                "this peer's own inbox must match at tick {tick}"
            );
            assert_eq!(
                bot.delivered_outbox, reference.delivered_outbox,
                "the products handed to the router must match in value and order at tick {tick}"
            );
            assert_eq!(
                bot.first_tampered_tick, reference.first_tampered_tick,
                "the honest shadow must first disagree on the same tick, at tick {tick}"
            );
            assert_eq!(
                bot.resolved_shots, reference.resolved_shots,
                "the captured shot verdicts must match at tick {tick}"
            );
            if bot.replica_seen_at.contains_key(&remote) {
                assert_eq!(
                    bot.host.state_bytes(remote),
                    Some(installed_replica.clone()),
                    "the replica outside the prediction set is frozen at tick {tick}: \
                     byte-identical to what the ingest leg installed"
                );
            } else {
                expiries = expiries.saturating_add(1);
                assert_eq!(
                    bot.host.state_bytes(remote),
                    None,
                    "an expired replica is gone from both columns at tick {tick}"
                );
            }
            let converged_frame = bot.chain.as_mut().and_then(|chain| chain.cut_frame(tick));
            let reference_frame = reference
                .chain
                .as_mut()
                .and_then(|chain| chain.cut_frame(tick));
            assert_eq!(
                converged_frame.is_some(),
                reference_frame.is_some(),
                "both columns must cut a frame on the same tick, at tick {tick}"
            );
            if let (Some(converged), Some(reference_cut)) = (&converged_frame, &reference_frame) {
                // The signed frame is the whole witness column for this
                // window: its input records with their sources, its neighbour
                // records with their observation stamps and read bytes, and
                // the signature over the folded chain. Comparing its bytes
                // compares all of that at once.
                assert_eq!(
                    serde_json::to_vec(&converged.frame).expect("a frame serializes"),
                    serde_json::to_vec(&reference_cut.frame).expect("a frame serializes"),
                    "the witness column must fold to the same signed frame at tick {tick}"
                );
                assert_eq!(converged.transitions, reference_cut.transitions);
                assert_eq!(converged.tick_hashes, reference_cut.tick_hashes);
                frames_cut = frames_cut.saturating_add(1);
            }

            steps = steps.saturating_add(bot.last_step.predicted);
            events_seen = events_seen.saturating_add(bot.last_step.events.len());
            wired = wired.saturating_add(bot.delivered_outbox.len());
            inboxed = inboxed
                .saturating_add(bot.delivered_inbox.len())
                .saturating_add(bot.hosted_inbox.values().map(Vec::len).sum::<usize>());
            materializations = materializations
                .saturating_add(bot.host.backend().entities().count().saturating_sub(before));
            neighbour_reads =
                neighbour_reads.saturating_add(reference_frame.as_ref().map_or(0, |cut| {
                    cut.frame
                        .entities
                        .iter()
                        .flat_map(|slice| slice.records.iter())
                        .filter(|record| {
                            matches!(record.source, RecordSource::NeighborFrame { .. })
                        })
                        .count()
                }));
            let _ = bot.drain_delivered();
            let _ = core::mem::take(&mut reference.delivered_outbox);
            control.step_entity(remote, Tick::new(tick), &[]);
        }

        // ── non-vacuity: this run really did all of it ────────────────────
        assert!(
            steps >= TICKS as usize,
            "the run must have stepped at least one authority per tick, not {steps}"
        );
        assert!(events_seen > 0, "the run emitted no events at all");
        assert!(wired > 0, "the run handed nothing to the router");
        assert!(inboxed > 0, "the run queued nothing into an authored inbox");
        assert!(frames_cut > 0, "the run cut no witness frame");
        assert!(materializations > 0, "the run materialized nothing");
        assert!(neighbour_reads > 0, "the run's witness column is empty");
        assert!(expiries > 0, "the staleness sweep never fired");
        assert_ne!(
            control.state(remote).map(CoreCodec::to_canonical),
            Some(installed_replica),
            "the freeze is not vacuous: the same replica, stepped, does move"
        );
    }

    /// A `Damage` order large enough to break the smallest campaign rock,
    /// shaped exactly as `Regolith::deliver` shapes one from `DamageDealt`.
    fn mining_damage(from: PersistId, shooter: &Craft) -> Order {
        Order::Damage {
            amount: 100_000,
            from,
            from_pos: shooter.pos,
            from_vel: shooter.vel,
            from_yaw_urad: shooter.yaw_urad,
            from_archetype: shooter.archetype,
            from_weapon: shooter.weapon,
            flight_ticks: None,
        }
    }

    #[test]
    fn the_converged_bot_tick_reproduces_the_hand_rolled_loop() {
        bot_convergence_run(None);
    }

    /// The same detector on a modified build: the tampered rules step the
    /// authority, the shipping rules step the shadow beside it, and both
    /// columns must first disagree on the same tick.
    #[test]
    fn the_converged_bot_tick_reproduces_the_hand_rolled_loop_on_a_modified_build() {
        for tamper in Tamper::ALL {
            bot_convergence_run(Some(*tamper));
        }
    }

    /// One tick of this harness's roam under `rules`, on `archetype`.
    fn hash_after_a_thrust(rules: Regolith, archetype: Archetype) -> [u8; 32] {
        let entity = PersistId::new(1);
        let mut executor = Executor::new(rules, UniverseSeed([7; 32]));
        executor.insert(
            entity,
            RegolithState::Craft(Craft::spawned(
                archetype,
                QPos::from_metres(1_000.0, 0.0, 0.0),
                0,
            )),
        );
        executor
            .step_entity(
                entity,
                Tick::new(0),
                &[Order::Thrust {
                    // Exactly what `step_core` asks for.
                    accel_mmss: 60_000,
                    yaw_urad: 213,
                    pitch_urad: 0,
                }],
            )
            .expect("seeded")
            .state_hash
    }

    #[test]
    fn the_speed_cheat_is_inert_on_an_interceptor_and_bites_on_a_cruiser() {
        // **The reason `Bot::new` pins a modified peer to the cruiser slot**,
        // asserted rather than argued for in a comment.
        //
        // `Tamper::SpeedMultiplier` raises an archetype's ceilings by 1.5×. The
        // interceptor's `max_accel_mmss` is 60 000 and this roam requests
        // 60 000, so `clamp(0, 60_000)` and `clamp(0, 90_000)` return the same
        // number: the tampered build produces byte-identical state, files
        // nothing, and every conviction clause would hold over a swarm in which
        // nothing happened. The cruiser's ceiling is 20 000, so the same request
        // clamps to 20 000 honestly and to 30 000 tampered.
        //
        // Neither *speed* ceiling binds at all — the bots cruise at 32 m/s
        // against 120 and 60 — so the acceleration clamp is the whole of this
        // cheat's effect at these parameters.
        let cheating = Regolith::cheating(Tamper::SpeedMultiplier);
        assert_eq!(
            hash_after_a_thrust(Regolith::honest(), Archetype::Interceptor),
            hash_after_a_thrust(cheating, Archetype::Interceptor),
            "the speed cheat is inert on an interceptor at this roam's requested \
             acceleration; if that ever stops being true, the archetype pin in `Bot::new` \
             is solving a problem that no longer exists and should go",
        );
        assert_ne!(
            hash_after_a_thrust(Regolith::honest(), Archetype::Cruiser),
            hash_after_a_thrust(cheating, Archetype::Cruiser),
            "the speed cheat must change a cruiser's state, or the conviction leg has \
             nothing to convict",
        );
    }

    #[test]
    fn replicated_contact_submits_the_live_collision_exchange() {
        let spec = BotSpec {
            index: 0,
            count: 2,
            seed: UniverseSeed([0x51; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: true,
            cheat: None,
            enforcing: false,
        };
        let mut bot = Bot::new(spec);
        let mut own = Craft::spawned(Archetype::Interceptor, QPos::default(), 0);
        own.vel.x = 20_000;
        bot.replace_craft_for_test(own);

        let other_id = PersistId::new(2);
        let mut other = Craft::spawned(
            Archetype::Cruiser,
            QPos {
                x: 5_000,
                y: 0,
                z: 0,
            },
            0,
        );
        other.vel.x = -10_000;
        let state = RegolithState::Craft(other);
        let inner = encode_replication(&(
            state.to_canonical(),
            CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).expect("origin cell"),
            other_id,
            1_u64,
        ));
        let payload = orrery_net::channels::tag(Channel::State, &inner);
        bot.receive_inbound(bot_key(1).public(), other_id, None, Bytes::from(payload));

        bot.step_core(1, spec.cell_edge_m);

        let authored = bot
            .chain
            .as_mut()
            .expect("witnessing installed the live producer")
            .cut_frame(u64::from(FRAME_TICKS) - 1)
            .expect("the frame interval closes");
        assert!(authored.frame.entities[0].records.iter().any(|record| {
            matches!(
                record.source,
                RecordSource::NeighborFrame { neighbor, .. } if neighbor == other_id
            )
        }));
        assert!(bot
            .drain_delivered()
            .into_iter()
            .any(|(_, recipient, order)| {
                recipient == other_id
                    && matches!(
                        order,
                        Order::CollisionResolved { from, .. } if from == bot.entity()
                    )
            }));
        assert_eq!(bot.craft().collisions, 1);
    }

    #[test]
    fn only_a_modified_bot_reports_a_tampered_tick_and_it_is_the_first_one() {
        // The dual-execution probe is what gives "convicted within one
        // adjudication window" a t = 0. Without it the harness knows which
        // build it handed out and not which tick that build first mattered on.
        let spec = BotSpec {
            index: 0,
            count: 8,
            seed: UniverseSeed([3; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: true,
            cheat: None,
            enforcing: false,
        };
        let mut honest = Bot::new(spec);
        let mut modified = Bot::new(BotSpec {
            cheat: Some(Tamper::SpeedMultiplier),
            enforcing: true,
            ..spec
        });
        for tick in 0..120 {
            honest.step_core(tick, spec.cell_edge_m);
            modified.step_core(tick, spec.cell_edge_m);
        }
        assert_eq!(honest.first_tampered_tick(), None);
        assert_eq!(
            modified.first_tampered_tick(),
            Some(0),
            "both builds start at rest, so the very first tick asks for full thrust and \
             the two clamps already disagree",
        );
    }

    #[test]
    fn every_regolith_tamper_is_live_under_the_shared_pilot() {
        let spec = BotSpec {
            index: 0,
            count: 8,
            seed: UniverseSeed([3; 32]),
            cell_edge_m: default_cell_edge_m(),
            witnessing: true,
            cheat: None,
            enforcing: false,
        };
        for tamper in Tamper::ALL {
            let mut modified = Bot::new(BotSpec {
                cheat: Some(*tamper),
                enforcing: true,
                ..spec
            });
            let target = PersistId::new(2);
            let game = Regolith::honest();
            let mut target_authority = Executor::new(game, spec.seed);
            let (target_pos, target_yaw) = spawn_pose(1, spec.count);
            target_authority.insert(
                target,
                RegolithState::Craft(Craft::spawned(
                    Archetype::for_slot(1),
                    target_pos,
                    target_yaw,
                )),
            );
            for tick in 0..=orrery_protocol::MAX_ADJUDICATION_TICKS {
                modified.step_core(tick, spec.cell_edge_m);
                for (_, recipient, order) in modified.drain_delivered() {
                    if recipient != target {
                        continue;
                    }
                    let outcome = target_authority
                        .step_entity(target, Tick::new(tick), &[order])
                        .expect("the test target owns its entity");
                    for event in &outcome.events {
                        if let Some((recipient, reply)) = game.deliver(event) {
                            if recipient == modified.entity {
                                modified.delivered_inbox.push((target, reply));
                            }
                        }
                    }
                }
                if modified.first_tampered_tick().is_some() {
                    break;
                }
            }
            assert!(
                modified
                    .first_tampered_tick()
                    .is_some_and(|tick| tick <= orrery_protocol::MAX_ADJUDICATION_TICKS),
                "{} is inert under the shared pilot for a whole adjudication window",
                tamper.name()
            );
            if *tamper == Tamper::DamageInflation {
                assert_eq!(modified.craft().weapon, WeaponKind::Volley);
                assert_eq!(modified.craft().shots, 1);
                assert!(
                    modified.craft().damage_dealt >= 3,
                    "Volley must leave all three inflated rolls in the own-state trace"
                );
            }
        }
    }
}
