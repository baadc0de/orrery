//! The swarm: N bots, a router between them, and the criterion they must meet.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;

use orrery_games::game::Tamper;
use orrery_games::regolith::state::RegolithState;
use orrery_games::regolith::{pilot::PILOT_SCENARIOS, REGOLITH_RULESET};
use orrery_net::peer_link::StreamMode;
use orrery_protocol::coord::PeerEntry;
use orrery_protocol::{CellId, NodeId, PersistId, UniverseSeed, MAX_ADJUDICATION_TICKS};

use crate::adjudicate::{Adjudicator, Docket};
use crate::bot::{Bot, BotSpec, TICK_HZ};
use crate::exterior::{Frame, Lane, UplinkAck, UplinkDatagram, UplinkOutcome};

use crate::router::{Impairment, Router, RouterCounters};

/// The harness's modified clients: which cheat, and how many peers run it.
///
/// P4's demo criterion names one — "a modified client applying a 1.5× speed
/// multiplier joins an 8-peer island" — but the count is a parameter because a
/// single cheat proves detection and says nothing about whether the honest
/// peers around it stay unaccused as the population of cheats grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheatSpec {
    /// The tamper the modified peers' authority runs.
    pub tamper: Tamper,
    /// How many peers run it.
    pub count: usize,
}

/// How the swarm is configured for a run.
#[derive(Debug, Clone, Copy)]
pub struct SwarmConfig {
    /// Number of peers. The P1 criterion says 32.
    pub peers: usize,
    /// Simulated seconds to run.
    pub seconds: u64,
    /// Interest-cell edge in metres.
    pub cell_edge_m: f32,
    /// Send cadence in Hz (D8: 20 Hz default against the 60 Hz sim tick).
    pub send_hz: u64,
    /// Link conditions.
    pub impairment: Impairment,
    /// Seed for impairment and the universe.
    pub seed: u64,
    /// Tick at which a late joiner appears, if any.
    pub late_join_tick: Option<u64>,
    /// Run the witness pipeline: every peer watches its witness set's entities
    /// and re-executes their signed logs (P4's input).
    pub witnessing: bool,
    /// The modified clients to field, if any.
    pub cheats: Option<CheatSpec>,
    /// Take every peer's witness out of shadow mode, so a raised window is
    /// actually filed.
    ///
    /// Implied by [`SwarmConfig::cheats`] — a conviction leg that files nothing
    /// convicts nobody — but separable from it, and the separation is what makes
    /// the control leg worth running. Shadow mode files nothing *by
    /// construction*, so "an unmodified swarm files no report at all" is a
    /// tautology under it and a real assertion under this: every witness in an
    /// entirely honest swarm is armed, and still files zero.
    ///
    /// P4's own posture is shadow mode, and the honest legs keep it (D17
    /// risk 3): enforcement stays off until the false-positive rate has been
    /// measured, and these two legs are what measure it.
    pub enforcing: bool,
    /// Wall-clock stamp for the report, when one was asked for.
    ///
    /// Read by the caller rather than by the swarm: nothing inside the run may
    /// consult a clock, or the run stops being a function of its seed.
    pub started_at_unix_secs: Option<u64>,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            peers: 32,
            seconds: 3_600,
            cell_edge_m: crate::bot::default_cell_edge_m(),
            send_hz: 20,
            impairment: Impairment::default(),
            seed: 1,
            late_join_tick: None,
            witnessing: false,
            cheats: None,
            enforcing: false,
            started_at_unix_secs: None,
        }
    }
}

/// What one peer did over the run.
#[derive(Debug, Clone, Serialize)]
pub struct PeerReport {
    /// Index in the swarm.
    pub index: usize,
    /// Distinct interest cells visited.
    pub cells_visited: usize,
    /// Committed-cell changes.
    pub crossings: u64,
    /// Commitments that returned to the cell just left — hysteresis failures.
    pub boundary_flips: u64,
    /// Entries and exits from the bounded high-rate set.
    pub interest_churn: u64,
    /// Demotions that re-promoted within a second — visible proxy pops.
    pub proxy_pops: u64,
    /// Peak upload rate in bits per simulated second.
    pub peak_upload_bits: u64,
    /// Upload rate at the 99th percentile of samples.
    pub p99_upload_bits: u64,
    /// Packets the send path shed for want of budget.
    pub shed: u64,
    /// This peer's behavioural profile.
    pub profile: &'static str,
    /// The cheat this peer's authority runs, or `null` for the shipping build.
    pub tamper: Option<&'static str>,
    /// First tick this peer's build produced state the shipping rules would
    /// not have. `null` on an honest peer — and on a modified one whose cheat
    /// turned out to be inert, which is a finding rather than a pass.
    pub first_tampered_tick: Option<u64>,
    /// Simulated tick at which an independent re-run of a filed report first
    /// returned `Verdict::Confirms` against this peer.
    pub convicted_at_tick: Option<u64>,
    /// Chain gaps this peer detected — repairs, not accusations.
    pub gaps: u64,
    /// Signals this peer raised that would accuse an honest island-mate.
    pub false_positives: u64,
    /// Of those, stage-1 invariant breaches.
    pub invariant_breaches: u64,
    /// Of those, re-execution disagreeing with a signed claim.
    pub claim_mismatches: u64,
    /// Of those, subjects whose hole was never filled.
    pub stalled: u64,
    /// Of those, reports actually **filed** against an honest island-mate.
    ///
    /// Zero on every leg that leaves shadow mode on, which is every leg but the
    /// conviction one — and zero on that one too, or the demo criterion is not
    /// met.
    pub reports: u64,
    /// Non-gap signals this peer raised against a subject the harness modified.
    /// Findings, not false positives.
    pub signals_against_tampered: u64,
    /// Reports this peer filed against a subject the harness modified.
    pub reports_against_tampered: u64,
    /// Reports this peer assembled, signed and handed to the transport.
    pub escalations_filed: u64,
    /// Windows it raised while in shadow mode, and therefore did not file.
    pub escalations_shadowed: u64,
    /// Mismatches it could not turn into a provable window.
    ///
    /// The expected tail of a conviction leg rather than a defect: a subject
    /// that diverges permanently never agrees with its witness again, so after
    /// the last claim inside `MAX_ADJUDICATION_TICKS` of the anchor there is no
    /// agreed point left to open a window at. The convictions have already
    /// happened by then.
    pub escalations_unservable: u64,
    /// Mismatches left unescalated for want of a `WitnessIdentity`.
    ///
    /// **Must be zero when witnessing.** Every peer is handed its own transport
    /// key as an identity; a non-zero count here means the harness went back to
    /// detecting without being able to file, which is what it did before this
    /// lane and which no other counter distinguishes from honest silence.
    pub escalations_unidentified: u64,
    /// Repair requests this peer dropped for want of queue space.
    pub repairs_overflowed: u64,
    /// Repair requests this peer could not answer from its retained log.
    pub repairs_unservable: u64,
    /// Frames it folded on a retry, after the hole in front of them closed.
    pub frames_recovered: u64,
    /// Watches this peer resumed at a later anchor after abandoning a hole.
    pub reanchors: u64,
    /// Subject ticks it gave up on doing so — the blind half of coverage.
    pub unjudged_ticks: u64,
    /// Subject ticks it actually re-executed and could judge against.
    pub judged_ticks: u64,
    /// Subject ticks it was shown, judged or not — the coverage denominator.
    pub shown_ticks: u64,
    /// Frames it refused: bad signature, broken chain, illegal order.
    pub frames_rejected: u64,
    /// Of those, ones refused by a watch that had never folded anything —
    /// a watch that will refuse every frame it is shown for the rest of the
    /// run, and asks for no repair while it does.
    pub frames_rejected_unanchored: u64,
    /// Watches that were shown frames and never folded one.
    ///
    /// The unit the coverage deficit comes in: a watch judges its subject's
    /// whole timeline or none of it, so this times the run length is the
    /// deficit, to the tick.
    pub watches_unanchored: u64,
    /// Frames it could not chain because a repair was outstanding.
    pub frames_deferred: u64,
    /// Of those, ones dropped because the subject's deferral buffer was full.
    pub deferrals_overflowed: u64,
    /// Of those, ones the retention sweep evicted before the hole closed.
    pub deferrals_pruned: u64,
    /// Of those, ones that failed verification when the drain re-offered them.
    pub deferrals_dropped_in_drain: u64,
    /// Of those, ones displaced by a later copy of the same frame.
    pub deferrals_replaced: u64,
    /// Of those, ones discarded because their ticks were already behind the
    /// fold — judged by the repair that overtook them, or abandoned with the
    /// hole the watch re-anchored past. Read beside `reanchors`.
    pub deferrals_stale: u64,
    /// Of those, ones still held behind an open hole when the run ended.
    ///
    /// The balance of the deferral ledger: with the five counters above and
    /// `frames_recovered`, every frame this peer set aside is accounted for,
    /// which is what turns a coverage deficit into a named cause.
    pub deferrals_held: u64,
    /// Claim comparisons it correctly declined to make while catching up.
    pub judgements_deferred: u64,
    /// Replica entities held at the end of the run.
    pub replicas: usize,
    /// Of those, how many carried an interest tag.
    pub tagged: usize,
    /// Of those tagged, how many were proxied rather than high-rate.
    pub proxied: usize,
    /// Inbound state packets this peer could not decode.
    ///
    /// **Must be zero.** A silent decode failure holds every peer at zero
    /// replicas, and then every clause about interest passes by describing an
    /// empty world — the most expensive kind of green, and one this harness
    /// produced for real before this counter existed.
    pub undecodable: u64,
}

/// Everything needed to reproduce a run, and to say which code produced it.
///
/// Deliberately free of wall clock. The harness's whole claim is that a run is
/// a function of its seed (see the module docs on time), and a timestamp folded
/// in here would make two identical runs compare unequal — which is exactly the
/// comparison an accumulated ledger of player-hours will want to make. A caller
/// that wants the clock asks for it, and it lands in
/// [`SwarmReport::started_at_unix_secs`] beside the body rather than inside it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RunIdentity {
    /// Seed for the impairment RNG and the universe.
    pub seed: u64,
    /// The link conditions the run was carried over, in full.
    pub impairment: Impairment,
    /// Target triple the harness was compiled for. P4's 500 hours are counted
    /// *across* platforms, so a report that does not name its own is unusable
    /// for the gate it is evidence for.
    pub target: &'static str,
    /// Commit the harness was built from, or `unknown` outside a git checkout.
    pub commit: &'static str,
}

/// What the host observed about the external peer over one run (#385).
#[derive(Debug, Clone, Serialize)]
pub struct ExteriorReport {
    /// The swarm slot the external peer occupied.
    pub index: usize,
    /// Whether the runner's clean end-of-run marker arrived.
    pub said_goodbye: bool,
    /// Whether the bridge believed the connection was alive at report time.
    pub connected: bool,
    /// Frames forwarded from the remote into the router.
    pub uplink_frames: u64,
    /// Frames queued for the remote out of router deliveries.
    pub downlink_frames: u64,
    /// Downlink frames refused because the queue was full. Zero at criterion
    /// rates; non-zero means the pump fell behind the swarm's clock.
    pub downlink_dropped: u64,
    /// Whether the peer shipped a tick-zero witness anchor at join. `false`
    /// for a rendered client (#387): its slot is seated unwitnessed, and the
    /// witnessed clauses of this report cover the bot cohort only.
    pub witness_anchored: bool,
}

/// The whole run.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmReport {
    /// What produced this run, and what it would take to produce it again.
    pub identity: RunIdentity,
    /// Game whose hours this report banks.
    pub game: &'static str,
    /// Exact Regolith rules version executed and witnessed.
    pub ruleset_version: u32,
    /// Input-diversity surfaces exercised during the session.
    pub scenarios: [&'static str; 4],
    /// Unix seconds at which the run started, when the caller asked for a
    /// stamp (`--stamp-wall-clock`).
    ///
    /// Outside [`RunIdentity`] on purpose: it is the one field that makes two
    /// runs of the same seed differ.
    pub started_at_unix_secs: Option<u64>,
    /// Peers in the swarm.
    pub peers: usize,
    /// Simulated seconds run.
    pub seconds: u64,
    /// Simulated ticks run.
    pub ticks: u64,
    /// Per-peer results.
    pub per_peer: Vec<PeerReport>,
    /// Highest peak upload across the swarm, bits per simulated second.
    pub worst_peak_upload_bits: u64,
    /// Highest p99 upload across the swarm.
    pub worst_p99_upload_bits: u64,
    /// Fewest distinct cells any peer visited.
    pub min_cells_visited: usize,
    /// Total packets shed across the swarm.
    pub total_shed: u64,
    /// Total boundary flips across the swarm. The criterion wants zero.
    pub total_boundary_flips: u64,
    /// Total visible proxy pops across the swarm.
    pub total_proxy_pops: u64,
    /// Total high-rate set entries and exits — the churn pops are judged against.
    pub total_interest_churn: u64,
    /// Packets still held by the link when the run ended.
    pub stranded_in_flight: usize,
    /// Inbound state packets no peer could decode.
    pub total_undecodable: u64,
    /// Replica entities held across the swarm at the end of the run.
    pub total_replicas: usize,
    /// Whether the witness pipeline ran.
    pub witnessing: bool,
    /// What the external peer did, when one joined (#385).
    pub external: Option<ExteriorReport>,
    /// Player-hours accumulated: peers times simulated seconds.
    pub player_hours: f64,
    /// Chain gaps detected across the swarm — expected under loss.
    pub total_gaps: u64,
    /// Signals raised against honest peers. **Every one is a false positive.**
    pub total_false_positives: u64,
    /// The conviction half of P4's demo criterion, when a modified client was
    /// fielded. `null` on the honest legs.
    pub conviction: Option<ConvictionReport>,
    /// Reports assembled, signed and handed to the transport, swarm-wide.
    pub total_escalations_filed: u64,
    /// Windows raised in shadow mode and therefore not filed, swarm-wide.
    pub total_escalations_shadowed: u64,
    /// Mismatches no witness could turn into a provable window, swarm-wide.
    pub total_escalations_unservable: u64,
    /// Mismatches left unescalated for want of an identity, swarm-wide.
    pub total_escalations_unidentified: u64,
    /// Frames folded on a retry rather than dropped and re-requested.
    pub total_frames_recovered: u64,
    /// Watches resumed at a later anchor after a hole was abandoned.
    pub total_reanchors: u64,
    /// Subject ticks abandoned unjudged by those resumes.
    pub total_unjudged_ticks: u64,
    /// Subject ticks re-executed and available to judge against.
    pub total_judged_ticks: u64,
    /// Subject ticks shown to a witness, judged or not.
    pub total_shown_ticks: u64,
    /// Frames refused across the swarm.
    pub total_frames_rejected: u64,
    /// Of those, ones refused by a watch that had never folded anything.
    pub total_frames_rejected_unanchored: u64,
    /// Watches across the swarm that were shown frames and never folded one.
    pub total_watches_unanchored: u64,
    /// Frames set aside across the swarm because a repair was outstanding.
    pub total_frames_deferred: u64,
    /// Claim comparisons correctly declined while catching up.
    pub total_judgements_deferred: u64,
    /// Deferred frames dropped for want of buffer space.
    pub total_deferrals_overflowed: u64,
    /// Deferred frames the retention sweep evicted before the hole closed.
    pub total_deferrals_pruned: u64,
    /// Deferred frames that failed verification when the drain re-offered them.
    pub total_deferrals_dropped_in_drain: u64,
    /// Deferred frames displaced by a later copy of themselves.
    pub total_deferrals_replaced: u64,
    /// Deferred frames discarded because their ticks were already behind the
    /// fold.
    pub total_deferrals_stale: u64,
    /// Deferred frames still held behind an open hole when the run ended.
    pub total_deferrals_held: u64,
    /// Whether the deferral ledger balances: every frame set aside was
    /// recovered, overflowed, pruned, dropped by a drain, replaced, discarded
    /// as stale, or is still held.
    ///
    /// **The clause that makes the attribution evidence rather than a guess.**
    /// A coverage deficit is only attributable to the deferral path if that
    /// path's own arithmetic closes; if it does not, some frame left by a door
    /// this report does not name and the named causes are a lower bound.
    pub deferral_ledger_balances: bool,
    /// Share of watched ticks this swarm actually judged, 0.0–1.0.
    ///
    /// **The number that makes a false-positive count mean anything.** A
    /// witness that has stopped observing reports zero findings for the same
    /// reason an honest island does, and nothing else in this report tells the
    /// two apart. Before re-anchoring existed every watch went permanently
    /// blind within about twenty-five simulated seconds and this figure decayed
    /// towards zero while the run kept accumulating player-hours.
    pub observation_coverage: f64,
    /// Wire bytes the swarm spent on replicated entity state.
    pub replication_bytes: u64,
    /// Wire bytes the swarm spent on the verifiable-core lane: log frames and
    /// state claims (docs/03-replication.md §5.3a).
    pub witness_bytes: u64,
    /// Wire bytes the swarm spent on the reliable lane, gap repairs included.
    pub control_bytes: u64,
    /// The witness lane's share of everything sent, 0.0–1.0.
    ///
    /// A share of *traffic*, not of budget — informative about the mix, and not
    /// the number the cadence is chosen against. Use
    /// [`Self::witness_lane_bits_per_sec`] for that.
    pub witness_lane_share: f64,
    /// The witness lane's sustained cost, in bits per simulated second **per
    /// peer**.
    ///
    /// The figure `orrery_witness::plugin::WITNESS_LANE_SHARE_PCT` bounds, and
    /// the one docs/03-replication.md §5.3 states as `≈ 0.15–0.2 Mbps`. Printing
    /// it beside the peak upload is what turns "a peer went over budget" into
    /// "this lane went over its share" — or, when it has not, points the
    /// finding at the other lane.
    pub witness_lane_bits_per_sec: u64,
    /// Link statistics.
    pub link: LinkReport,
    /// The late-join check, if one ran.
    pub late_join: Option<LateJoinReport>,
}

/// Link statistics, flattened for the report.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LinkReport {
    /// Packets carried.
    pub delivered: u64,
    /// Packets dropped by the impairment model.
    pub dropped: u64,
    /// Packets delayed by it.
    pub delayed: u64,
    /// Wire bytes carried.
    pub bytes: u64,
}

impl From<RouterCounters> for LinkReport {
    fn from(counters: RouterCounters) -> Self {
        Self {
            delivered: counters.delivered,
            dropped: counters.dropped,
            delayed: counters.delayed,
            bytes: counters.bytes,
        }
    }
}

/// What happened to the modified clients the harness fielded.
///
/// P4's demo criterion in one struct: *a modified client applying a 1.5× speed
/// multiplier joins an 8-peer island — detected, escalated, replay-adjudicated
/// with a deviation verdict within one adjudication window of the violation*,
/// while the honest peers beside it are accused of nothing.
#[derive(Debug, Clone, Serialize)]
pub struct ConvictionReport {
    /// The cheat that was fielded.
    pub tamper: &'static str,
    /// Peers running it.
    pub tampered_peers: usize,
    /// Of those, how many diverged from the shipping rules at all.
    ///
    /// **The anti-vacuity number.** `Tamper::SpeedMultiplier` is inert on an
    /// interceptor slot at this roam's requested acceleration, and a cheat that
    /// never changes a byte is never reported — so every clause below would
    /// hold over a swarm in which nothing happened.
    pub tampered_peers_that_diverged: usize,
    /// Of those, how many an independent re-run of a filed report convicted.
    pub tampered_peers_convicted: usize,
    /// Earliest tick any modified peer's build diverged.
    pub first_tampered_tick: Option<u64>,
    /// The worst gap, in ticks, between a modified peer diverging and a
    /// `Verdict::Confirms` being reached against it.
    ///
    /// Measured on the tick the swarm drained the report, not on the bundle's
    /// window end: the window end is when the *evidence* stops, and the
    /// criterion is about how long a deviation survives in the running system,
    /// which includes the frames and claims still crossing the link.
    pub worst_detection_ticks: Option<u64>,
    /// Reports filed against a modified peer.
    pub reports_against_tampered: u64,
    /// Reports filed against a peer that was not modified. **Must be zero.**
    pub reports_against_honest: u64,
    /// Reports re-run by the in-process adjudicator.
    pub adjudicated: u64,
    /// Of those, verdicts that proved a deviation.
    pub confirms: u64,
    /// Of those, verdicts that cleared the accused.
    pub exonerates: u64,
    /// Of those, verdicts that struck the reporter instead.
    pub evidence_forged: u64,
    /// Of those, verdicts that decided nothing.
    pub unadjudicable: u64,
}

/// What a late joiner could see on arrival.
#[derive(Debug, Clone, Serialize)]
pub struct LateJoinReport {
    /// Cells in the joiner's 27-cell neighbourhood.
    pub neighbourhood: usize,
    /// Peers the joiner's island roster named.
    pub roster: usize,
    /// Peers whose cell was inside that neighbourhood.
    pub in_neighbourhood: usize,
    /// Peers the joiner tracked — must not exceed `in_neighbourhood`.
    pub tracked: usize,
}

/// Runs the swarm and produces its report.
pub struct Swarm {
    config: SwarmConfig,
    bots: Vec<Bot>,
    router: Router,
    /// Per-peer upload samples, one per simulated second.
    samples: Vec<Vec<u64>>,
    /// NodeId → swarm index, for routing.
    index_of: BTreeMap<NodeId, usize>,
    /// The connected external peer, when the run has one (#385).
    exterior: Option<ExteriorSlot>,
    /// The in-process cluster that re-runs filed reports.
    adjudicator: Adjudicator,
    /// Every verdict it reached.
    docket: Docket,
}

/// One joined external peer, as far as the host ever knows it.
///
/// Deliberately less than a [`Bot`]: no executor, no Bevy app, no pilot. The
/// remote process owns all of that; the host holds only what routing and
/// witnessing require — where to send its traffic, what it committed to at
/// tick zero, and which cell it last said it was in.
pub struct ExteriorSlot {
    /// The swarm slot this peer occupies: always `bots.len()`.
    pub index: usize,
    /// Transport identity, verified against the dial at join time.
    pub node: NodeId,
    /// The entity id derived from the slot, exactly as a bot's is.
    pub entity: PersistId,
    /// The interest cell from the slot's deterministic spawn pose. Updated by
    /// meta frames as the peer moves; starts honest by construction because
    /// both sides derive the pose from the slot alone (`bot::spawn_pose`).
    cell: CellId,
    /// The tick-zero claim the peer shipped after joining, with the state it
    /// commits to — what watchers arm against instead of reading a local
    /// `Chain`. Present when witnessing is on and the peer authors a witness
    /// log (the headless runner does; a rendered client does not, #387).
    pub anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
    /// Whether an anchor was shipped at all, kept after `anchor` is taken.
    pub witness_anchored: bool,
    /// Queues to and from the connection pump.
    pub link: crate::exterior::HostLink,
    /// Frames forwarded up, for the report.
    uplink_frames: u64,
    /// Frames queued down, for the report.
    downlink_frames: u64,
    /// Downlink frames refused on a full queue.
    downlink_dropped: u64,
    /// True once the runner's clean end-of-run marker arrived. Shared with
    /// the bridge's reader task, which is what sees the marker first.
    pub goodbye: Arc<std::sync::atomic::AtomicBool>,
}

impl ExteriorSlot {
    /// Queues one frame on the established synchronous side of the bridge.
    /// `try_send` performs the send immediately; unlike `Sender::send`, it
    /// creates no future that could be dropped without being polled.
    fn queue_downlink(&mut self, frame: Frame) {
        match self.link.downlink.try_send(frame) {
            Ok(()) => self.downlink_frames += 1,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.downlink_dropped += 1;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.link
                    .connected
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Queues one settled router decision on the existing host→remote Meta
    /// lane. A full queue is visible as the same backpressure failure as any
    /// other downlink frame; at criterion rates this must remain zero.
    fn acknowledge_uplink(&mut self, ack: UplinkAck) {
        let frame = Frame {
            peer: u32::MAX,
            lane: Lane::Meta,
            payload: ack.encode(),
        };
        self.queue_downlink(frame);
    }

    /// Queues one router delivery for the remote, naming its sender's slot.
    fn deliver_from(&mut self, from: usize, stream: Option<StreamMode>, payload: Bytes) {
        let lane = match stream {
            None => Lane::Datagram,
            Some(StreamMode::Shared) => Lane::StreamShared,
            Some(StreamMode::Bulk) => Lane::StreamBulk,
        };
        // Downlink frames name their sender: that is the index the remote
        // needs to pick the right linked session (see `exterior`'s docs).
        let frame = Frame {
            peer: u32::try_from(from).unwrap_or(u32::MAX),
            lane,
            payload,
        };
        self.queue_downlink(frame);
    }

    /// Forwards every queued uplink frame into the router, and meta frames
    /// into this slot's own cell.
    ///
    /// Uplink traffic is *impaired like any other*: frames enter through
    /// `router.accept` at the swarm's own tick, never around it, which is the
    /// whole point of the bridge (#385's module docs).
    fn pump_uplink(&mut self, tick: u64, router: &mut Router) {
        let debug = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
        if debug && tick.is_multiple_of(60) {
            eprintln!(
                "bridge[host][{}]: pump_uplink tick {}",
                std::process::id(),
                tick
            );
        }
        while let Ok(frame) = {
            let mut r = self.link.uplink.lock().expect("uplink lock");
            r.try_recv()
        } {
            if debug && self.uplink_frames == 0 {
                eprintln!("bridge[host]: first uplink frame routed into the router");
            }
            // GOODBYE sentinel: the runner's clean end-of-run marker.
            if frame.payload.as_ref() == [0xFFu8] {
                self.goodbye
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            match frame.lane {
                Lane::Meta => {
                    // One u64: the sender's current interest cell, raw bits.
                    if let Ok(raw) = <[u8; 8]>::try_from(frame.payload.as_ref()) {
                        self.set_cell_from_bits(u64::from_le_bytes(raw));
                    }
                    continue;
                }
                Lane::Datagram => {
                    let Some(datagram) = UplinkDatagram::decode(frame.payload) else {
                        continue;
                    };
                    // This call is the load-bearing ordering boundary: only
                    // the router can say whether impairment retained or
                    // discarded the logical packet. The Meta ACK is queued
                    // strictly after that decision has been returned.
                    let disposition = router.accept(
                        tick,
                        self.node,
                        usize::try_from(frame.peer).unwrap_or(usize::MAX),
                        datagram.payload,
                    );
                    self.acknowledge_uplink(UplinkAck {
                        sequence: datagram.sequence,
                        outcome: match disposition {
                            crate::router::DatagramDisposition::Delivered => {
                                UplinkOutcome::Delivered
                            }
                            crate::router::DatagramDisposition::Dropped => UplinkOutcome::Dropped,
                        },
                    });
                }
                Lane::StreamShared => router.accept_stream(
                    tick,
                    self.node,
                    usize::try_from(frame.peer).unwrap_or(usize::MAX),
                    StreamMode::Shared,
                    frame.payload,
                ),
                Lane::StreamBulk => router.accept_stream(
                    tick,
                    self.node,
                    usize::try_from(frame.peer).unwrap_or(usize::MAX),
                    StreamMode::Bulk,
                    frame.payload,
                ),
            }
            self.uplink_frames += 1;
        }
    }

    /// The cell this peer last reported, for roster broadcasts.
    fn cell(&self) -> CellId {
        self.cell
    }

    /// Records a meta-lane cell report.
    /// Records a raw meta-lane cell report, refusing encodings that are not
    /// cells rather than storing them.
    fn set_cell_from_bits(&mut self, raw: u64) {
        if let Some(cell) = CellId::from_bits(raw) {
            self.cell = cell;
        }
    }
}

/// Which swarm indices run the modified build.
///
/// **Only cruising slots.** An idling bot never asks for thrust, and a cheat
/// that raises an acceleration ceiling changes nothing for a peer that never
/// accelerates — so dealing the modified build to `Profile::Idle` would field a
/// cheater whose state is byte-identical to an honest one's and let the
/// conviction clauses pass over a swarm in which nothing happened. The profiles
/// are dealt round-robin over `Profile::ALL`, so every fourth index cruises.
fn tampered_indices(peers: usize, count: usize) -> Vec<usize> {
    (0..peers)
        .filter(|index| {
            crate::profile::Profile::for_index(*index, true) == crate::profile::Profile::Cruise
        })
        .take(count)
        .collect()
}

impl Swarm {
    /// Build a swarm from `config`.
    #[must_use]
    pub fn new(config: SwarmConfig) -> Self {
        let mut universe = [0u8; 32];
        universe[0..8].copy_from_slice(&config.seed.to_le_bytes());
        let seed = UniverseSeed(universe);

        let tampered = config
            .cheats
            .map(|cheats| tampered_indices(config.peers, cheats.count))
            .unwrap_or_default();

        let bots: Vec<Bot> = (0..config.peers)
            .map(|index| {
                Bot::new(BotSpec {
                    index,
                    count: config.peers,
                    seed,
                    cell_edge_m: config.cell_edge_m,
                    witnessing: config.witnessing,
                    cheat: config
                        .cheats
                        .filter(|_| tampered.contains(&index))
                        .map(|cheats| cheats.tamper),
                    // Enforcement is a property of the *run*, not of the peer:
                    // a swarm in which only the cheater's watchers filed would
                    // prove nothing about honest peers being left alone, since
                    // nobody else was in a position to accuse anyone.
                    enforcing: config.enforcing,
                })
            })
            .collect();
        let index_of = bots
            .iter()
            .map(|bot| (bot.node, bot.index))
            .collect::<BTreeMap<_, _>>();

        Self {
            samples: vec![Vec::new(); config.peers],
            config,
            bots,
            router: Router::new(config.impairment, config.seed),
            index_of,
            adjudicator: Adjudicator::new(seed),
            docket: Docket::default(),
            exterior: None,
        }
    }

    /// Attaches an external peer to this swarm, occupying the next slot.
    ///
    /// Must be called before [`Self::run`]: the slot joins at island
    /// formation, its anchor arms the witnesses, and a run with a peer
    /// attached paces itself in real time — see `run`.
    ///
    /// The non-test caller is the host CLI mode, which lands with the tokio
    /// bridge in the next commit of #385; until then only the routing test
    /// drives this.
    #[allow(dead_code)]
    #[must_use]
    pub fn with_external(
        mut self,
        node: NodeId,
        anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
        link: crate::exterior::HostLink,
    ) -> Self {
        let index = self.bots.len();
        let (pos, _) = crate::bot::spawn_pose(index, index + 1);
        let start_grid = crate::bot::grid_of(&pos, self.config.cell_edge_m);
        let cell = crate::bot::cell_of(start_grid);
        let entity = PersistId::new(index as u64 + 1);
        self.index_of.insert(node, index);
        let goodbye_flag = link.goodbye.clone();
        let witness_anchored = anchor.is_some();
        self.exterior = Some(ExteriorSlot {
            index,
            node,
            entity,
            cell,
            anchor,
            witness_anchored,
            link,
            uplink_frames: 0,
            downlink_frames: 0,
            downlink_dropped: 0,
            goodbye: goodbye_flag,
        });
        // One more sample bucket than bots; the exterior's own upload is not
        // measured host-side (slice 3's problem), so it stays empty and is
        // never read.
        self.samples.push(Vec::new());
        self
    }

    /// A slot index's transport identity, bots and external alike.
    fn node_of(&self, index: usize) -> NodeId {
        match &self.exterior {
            Some(exterior) if exterior.index == index => exterior.node,
            _ => self.bots[index].node,
        }
    }

    /// Total participants, external peer included: the number the report
    /// counts as peers and hours.
    fn total_peers(&self) -> usize {
        self.bots.len() + usize::from(self.exterior.is_some())
    }

    /// Tell every peer which of its island-mates the harness modified.
    ///
    /// The oracle is the harness's, never the witness's: the engine is shown
    /// nothing, and all this decides is which column a raised signal is counted
    /// in. Without it a run with `--cheat` would report the convictions it is
    /// meant to produce as false positives and fail its own honest clause.
    fn declare_tampered(&mut self) {
        let tampered: Vec<NodeId> = self
            .bots
            .iter()
            .filter(|bot| bot.tamper().is_some())
            .map(|bot| bot.node)
            .collect();
        if tampered.is_empty() {
            return;
        }
        for bot in &mut self.bots {
            bot.set_tampered_subjects(tampered.clone());
        }
    }

    /// Wire every bot to every other: the mesh regime the criterion runs in.
    fn form_island(&mut self) {
        let mut roster: Vec<(NodeId, CellId)> = self
            .bots
            .iter_mut()
            .map(|bot| (bot.node, bot.cell().expect("seeded")))
            .collect();
        // The external peer is an island-mate like any other: it takes the
        // slot's deterministic spawn pose (`with_external`), so the host knows
        // its starting cell without asking.
        if let Some(exterior) = &self.exterior {
            roster.push((exterior.node, exterior.cell()));
        }

        for bot in &mut self.bots {
            let others: Vec<PeerEntry> = roster
                .iter()
                .filter(|(node, _)| *node != bot.node)
                .map(|(node, cell)| PeerEntry {
                    node: *node,
                    cells: cell.neighbors27(),
                })
                .collect();
            for entry in &others {
                bot.link(entry.node, 1_200);
            }
            bot.set_island(others);
        }
    }

    /// Refresh every bot's roster with where the others actually are.
    ///
    /// Stands in for the coordinator re-broadcasting a manifest as peers move.
    /// Without it a bot's view of its island-mates' cells freezes at tick zero
    /// and the visibility gate stops reflecting the world.
    fn refresh_rosters(&mut self) {
        // Pump the exterior's meta frames first, so today's roster carries the
        // cell it just reported rather than yesterday's.
        if let Some(exterior) = &mut self.exterior {
            while let Ok(raw) = {
                let mut r = exterior.link.meta.lock().expect("meta lock");
                r.try_recv()
            } {
                exterior.set_cell_from_bits(raw);
            }
        }
        let mut roster: Vec<(NodeId, CellId)> = self
            .bots
            .iter_mut()
            .map(|bot| (bot.node, bot.cell().expect("committed")))
            .collect();
        if let Some(exterior) = &self.exterior {
            roster.push((exterior.node, exterior.cell()));
        }
        for bot in &mut self.bots {
            let others: Vec<PeerEntry> = roster
                .iter()
                .filter(|(node, _)| *node != bot.node)
                .map(|(node, cell)| PeerEntry {
                    node: *node,
                    cells: cell.neighbors27(),
                })
                .collect();
            bot.set_island(others);
        }
    }

    /// Queue this bot's state to the island-mates whose interest covers it.
    ///
    /// **Gated by the manifest, not broadcast.** An authority sends to a peer
    /// only when that peer's declared cells contain the authority's own cell —
    /// the same predicate `orrery_spatial`'s visibility gate applies, evaluated
    /// from the roster the coordinator hands out. Sending to everyone would
    /// measure the naive mesh instead of what the bounded interest set buys,
    /// and would let a broken interest set pass the budget clause: at 32 peers
    /// even an ungated mesh fits under 1 Mbps, so the number alone proves
    /// nothing about interest management.
    ///
    /// The payload is the ruleset's own canonical encoding, so the wire cost is
    /// the size the real thing would be rather than a guess.
    fn broadcast(&mut self, index: usize, tick: u64) {
        self.bots[index].broadcast_state(tick);
    }

    /// Drain what each bot's send path handed the IO layer into the router.
    fn collect_sends(&mut self, tick: u64) {
        // Disjoint field borrows: the exterior pumps into the router directly,
        // at this same tick, so its traffic is impaired like a bot's.
        let exterior = self.exterior.as_mut();
        let router = &mut self.router;
        if let Some(exterior) = exterior {
            exterior.pump_uplink(tick, router);
        }
        for index in 0..self.bots.len() {
            let node = self.bots[index].node;
            if !self.bots[index].profile.is_sending(tick) {
                // The peer is hitching. Its packets are built and then never
                // leave — which is what a client stall actually is, and it
                // leaves the peer's own log intact so it can still answer for
                // itself when its witnesses come asking.
                self.bots[index].stall();
                continue;
            }
            for (to, stream, payload) in self.bots[index].drain_outbound() {
                let Some(target) = self.index_of.get(&to).copied() else {
                    self.router.counters.misaddressed += 1;
                    continue;
                };
                match stream {
                    Some(mode) => self.router.accept_stream(tick, node, target, mode, payload),
                    None => {
                        self.router.accept(tick, node, target, payload);
                    }
                }
            }
        }
    }

    /// Hand every due packet to its recipient's buffer, on the lane it came in on.
    fn deliver(&mut self, tick: u64) {
        for delivery in self.router.deliver_due(tick) {
            if let Some(exterior) = &mut self.exterior {
                if delivery.to == exterior.index {
                    let from = self
                        .index_of
                        .get(&delivery.from)
                        .copied()
                        .unwrap_or(usize::MAX);
                    exterior.deliver_from(from, delivery.stream, delivery.payload);
                    continue;
                }
            }
            // The router already carries the sender's identity verbatim.
            self.bots[delivery.to].receive_inbound(
                delivery.from,
                delivery.stream,
                delivery.payload,
            );
        }
    }

    /// Run the configured number of simulated seconds.
    #[must_use]
    pub fn run(mut self) -> SwarmReport {
        self.form_island();
        self.declare_tampered();
        if self.config.witnessing {
            self.seed_witnesses();
        }
        let ticks = self.config.seconds * TICK_HZ;
        let send_every = (TICK_HZ / self.config.send_hz.max(1)).max(1);
        let mut late_join = None;

        // A run with a connected external peer paces itself in **real time**:
        // the remote process steps at wall clock — a human plays in real time,
        // and bankable hours need real connected time — so the host may not
        // outrun it. Pure-bot runs keep their faster-than-real-time pacing and
        // every nightly number that depends on it (#385).
        let real_time = self.exterior.is_some();
        let tick_duration = std::time::Duration::from_nanos(1_000_000_000 / TICK_HZ);

        let mut phase = [0u128; 6];
        for tick in 0..ticks {
            let tick_start = std::time::Instant::now();
            self.tick_once(tick, send_every, &mut phase);

            // Once a simulated second, sample each peer's rate and re-publish
            // the roster — the coordinator's manifest cadence.
            if tick % TICK_HZ == TICK_HZ - 1 {
                for index in 0..self.bots.len() {
                    let rate = self.bots[index].upload_rate_bits();
                    self.samples[index].push(rate);
                }
                self.refresh_rosters();
            }

            if self.config.late_join_tick == Some(tick) {
                late_join = Some(self.check_late_join());
            }

            // The external run's metronome: hold each tick to its wall-clock
            // slice so the remote process can keep step. See `real_time` above.
            if real_time {
                let spent = tick_start.elapsed();
                if spent < tick_duration {
                    std::thread::sleep(tick_duration - spent);
                }
            }
        }

        // Drain anything the jitter model is still holding. A packet due after
        // the last tick is an artefact of stopping the clock mid-flight, not a
        // link that fails to deliver — but a link that is *still* holding work
        // after the drain is the latter, which is why the clause survives.
        if std::env::var_os("P1_SWARM_PHASES").is_some() {
            let names = [
                "step",
                "publish",
                "app.update",
                "sample",
                "collect",
                "deliver",
            ];
            for (name, nanos) in names.iter().zip(phase) {
                eprintln!("gates/p1-swarm: phase {name:>11}: {:>8.2}s", nanos as f64 / 1e9);
            }
        }
        let _ = self.deliver_due_all(ticks + u64::from(self.config.impairment.jitter_ticks) + 1);
        self.report(ticks, late_join)
    }

    /// One tick of the swarm, in run order: claims, steps, broadcasts,
    /// publishes, app updates, samples, adjudication, then the send/deliver
    /// cycle that carries it all — including the exterior's uplink and
    /// downlink, when a peer is attached.
    ///
    /// Split from [`Self::run`] so tests can drive single ticks against a
    /// live bridge without committing to a whole simulated hour.
    fn tick_once(&mut self, tick: u64, send_every: u64, phase: &mut [u128; 6]) {
        if self.config.witnessing {
            // Before the tick runs: a claim commits to pre-step state.
            for bot in &mut self.bots {
                bot.publish_claim(tick);
            }
        }
        for index in 0..self.bots.len() {
            self.bots[index].step_core(tick, self.config.cell_edge_m);
        }
        let mut mark = std::time::Instant::now();
        phase[0] += mark.elapsed().as_nanos();
        // The last tick of each send window, not the first. Broadcasting at
        // `t` ships the state *after* `t` stepped, which is the state a claim
        // at `t + 1` commits to — so sending on the window's last tick is what
        // makes the replicated state and the signed claim the same object. On
        // the window's first tick the two are one tick apart forever, and no
        // receiver can check a claim against a state it was actually sent,
        // which is the corroboration stage 1 and re-anchoring both rest on.
        // Same cadence and same number of sends; only the phase moves.
        if tick % send_every == send_every - 1 {
            for index in 0..self.bots.len() {
                self.broadcast(index, tick);
            }
        }
        if self.config.witnessing {
            for bot in &mut self.bots {
                bot.publish(tick);
            }
        }
        phase[1] += mark.elapsed().as_nanos();
        mark = std::time::Instant::now();
        for bot in &mut self.bots {
            bot.update();
        }
        phase[2] += mark.elapsed().as_nanos();
        mark = std::time::Instant::now();
        for bot in &mut self.bots {
            bot.sample();
            bot.drain_signals();
        }
        // Stage 4, in the same tick the report was filed. The cluster is
        // in-process and believes nothing the reporter said: it checks the
        // reporter's signature, then re-runs the bundle under the shipping
        // rules. Adjudicating here rather than at the end of the run is what
        // makes the *tick* meaningful — the criterion bounds how long a
        // deviation survives, and a verdict reached in a batch after the hour
        // would have no time in it at all.
        //
        // Unconditional, not gated on `--cheat`: a leg that files nothing
        // costs nothing here, and gating it would make "an unmodified swarm
        // files nothing" a clause the harness could not have observed being
        // broken.
        for index in 0..self.bots.len() {
            for report in self.bots[index].drain_reports() {
                self.docket.record(&self.adjudicator, &report, tick);
            }
        }
        phase[3] += mark.elapsed().as_nanos();
        mark = std::time::Instant::now();
        self.collect_sends(tick);
        phase[4] += mark.elapsed().as_nanos();
        mark = std::time::Instant::now();
        self.deliver(tick);
        phase[5] += mark.elapsed().as_nanos();
    }

    /// Seed every peer's witness set and start it watching.
    ///
    /// Each bot streams its log to at most seven island-mates and watches
    /// exactly those it is streamed to — the reciprocal arrangement a
    /// coordinator-seeded cell-epoch set would produce, built here because
    /// seeding is P5's. Every anchor is the subject's own signed claim at tick
    /// zero, which is the only state a witness ever holds for someone else.
    fn seed_witnesses(&mut self) {
        use orrery_witness::plugin::MAX_WITNESS_LINKS;

        // The external peer is witnessed exactly like a bot: it holds a slot
        // in the ring and shipped its tick-zero anchor at join. What nobody
        // host-side can do is watch *through* it — its own observations live in
        // its process, and coverage counts what this run's watchers saw.
        let count = self.bots.len() + usize::from(self.exterior.is_some());
        // Ring assignment: peer i is witnessed by the next `MAX_WITNESS_LINKS`
        // peers around the ring. Deterministic, uniform, and it gives every peer
        // both a witness set and a watch list without a central chooser.
        let sets: Vec<Vec<usize>> = (0..count)
            .map(|index| {
                (1..=MAX_WITNESS_LINKS.min(count.saturating_sub(1)))
                    .map(|offset| (index + offset) % count)
                    .collect()
            })
            .collect();

        let exterior_index = self.exterior.as_ref().map(|exterior| exterior.index);
        // Anchors first: a watcher needs the subject's signed claim and the
        // state it commits to, and both have to be taken before anyone steps.
        let mut anchors: Vec<(
            PersistId,
            NodeId,
            orrery_protocol::StateClaim,
            RegolithState,
        )> = (0..self.bots.len())
            .map(|index| {
                let state = self.bots[index].state();
                let entity = self.bots[index].entity();
                let node = self.bots[index].node;
                let anchor = self.bots[index]
                    .chain
                    .as_mut()
                    .expect("witnessing")
                    .anchor(0, &state);
                (entity, node, anchor, state)
            })
            .collect();
        if let Some(exterior) = &mut self.exterior {
            let index = exterior.index;
            let entity = exterior.entity;
            let node = exterior.node;
            match exterior.anchor.take() {
                Some((claim, state)) => anchors.push((entity, node, claim, state)),
                None => {
                    // A rendered client (#387) authors no witness log and
                    // says so with an empty anchor at join. The slot seats
                    // unanchored: no watcher is armed against it, nothing of
                    // it is shown or judged, and the report carries
                    // `witness_anchored: false` so a human hour cannot be
                    // mistaken for an independently witnessed one. The
                    // headless runner still ships a real anchor and keeps
                    // the armed path.
                    eprintln!(
                        "gates/p1-swarm: exterior slot {index} joined without a witness anchor;                          its own input stream is not independently witnessed this run"
                    );
                }
            }
            debug_assert_eq!(index, count - 1, "the exterior takes the last ring slot");
        }

        for (index, witnesses) in sets.iter().enumerate() {
            let members: Vec<NodeId> = witnesses
                .iter()
                .map(|watcher| self.node_of(*watcher))
                .collect();
            if Some(index) == exterior_index {
                // The external peer's witness set travels with it: nothing to
                // configure host-side. Its authored frames reach these same
                // watchers through the bridge.
                continue;
            }
            self.bots[index].set_witness_set(members);
            // Each of those peers watches this one.
            let (entity, node, anchor, state) = anchors[index].clone();
            for watcher in witnesses {
                if Some(*watcher) == exterior_index {
                    // A bot cannot be armed by a remote subject's anchor here —
                    // but the external peer is not watching anyone either in
                    // slice 1; both directions of that asymmetry close when the
                    // rendered client lands (#386).
                    continue;
                }
                self.bots[*watcher].watch(entity, node, anchor.clone(), state.clone());
            }
        }
    }

    /// Deliver every packet due by `tick`, discarding the payloads.
    fn deliver_due_all(&mut self, tick: u64) -> usize {
        self.router.deliver_due(tick).len()
    }

    /// A peer arriving mid-run must see only its 27-cell neighbourhood.
    fn check_late_join(&mut self) -> LateJoinReport {
        // Whichever peer currently has the most island-mates in its
        // neighbourhood stands in for the joiner. Always picking the last bot
        // made the check vacuous once the crowd sheared apart: `tracked ≤
        // in_neighbourhood` is trivially true when both are zero, and the run
        // reported a pass having proven nothing.
        let cells: Vec<CellId> = (0..self.bots.len())
            .map(|index| self.bots[index].cell().expect("committed"))
            .collect();
        let joiner = (0..self.bots.len())
            .max_by_key(|index| {
                let neighbourhood = cells[*index].neighbors27();
                cells
                    .iter()
                    .enumerate()
                    .filter(|(other, cell)| other != index && neighbourhood.contains(cell))
                    .count()
            })
            .expect("a non-empty swarm");
        let neighbourhood: Vec<CellId> = self.bots[joiner].cell().expect("committed").neighbors27();

        let elsewhere: Vec<(NodeId, CellId)> = (0..self.bots.len())
            .filter(|index| *index != joiner)
            .map(|index| {
                let node = self.bots[index].node;
                (node, self.bots[index].cell().expect("committed"))
            })
            .collect();

        let in_neighbourhood = elsewhere
            .iter()
            .filter(|(_, cell)| neighbourhood.contains(cell))
            .count();

        let roster: Vec<PeerEntry> = elsewhere
            .iter()
            .map(|(node, cell)| PeerEntry {
                node: *node,
                cells: vec![*cell],
            })
            .collect();
        let roster_len = roster.len();
        self.bots[joiner].set_island(roster);
        self.bots[joiner].update();

        LateJoinReport {
            neighbourhood: neighbourhood.len(),
            roster: roster_len,
            in_neighbourhood,
            tracked: self.bots[joiner].tracked(),
        }
    }

    fn report(mut self, ticks: u64, late_join: Option<LateJoinReport>) -> SwarmReport {
        let docket = core::mem::take(&mut self.docket);
        let per_peer: Vec<PeerReport> = (0..self.bots.len())
            .map(|index| {
                let mut samples = self.samples[index].clone();
                samples.sort_unstable();
                let peak = samples.last().copied().unwrap_or(0);
                let p99 = percentile(&samples, 99);
                let bot = &mut self.bots[index];
                let replicas = bot.replicas();
                let tagged = bot.tracked();
                let proxied = bot.proxies();
                let undecodable = bot.replica_counters().undecodable;
                let witness = bot.witness_counters();
                let links = bot.link_counters();
                let convicted_at_tick = docket.first_conviction(bot.node);
                PeerReport {
                    index,
                    cells_visited: bot.visited.len(),
                    crossings: bot.crossings,
                    boundary_flips: bot.boundary_flips,
                    interest_churn: bot.interest_churn,
                    proxy_pops: bot.proxy_pops,
                    peak_upload_bits: peak,
                    p99_upload_bits: p99,
                    shed: bot.shed(),
                    profile: bot.profile.name(),
                    tamper: bot.tamper().map(Tamper::name),
                    first_tampered_tick: bot.first_tampered_tick(),
                    convicted_at_tick,
                    gaps: bot.signals.gaps,
                    false_positives: bot.signals.false_positives(),
                    invariant_breaches: bot.signals.invariant_breaches,
                    claim_mismatches: bot.signals.claim_mismatches,
                    stalled: bot.signals.stalled,
                    reports: bot.signals.reports,
                    signals_against_tampered: bot.signals.signals_against_tampered,
                    reports_against_tampered: bot.signals.reports_against_tampered,
                    escalations_filed: links.escalations_filed,
                    escalations_shadowed: links.escalations_shadowed,
                    escalations_unservable: links.escalations_unservable,
                    escalations_unidentified: links.escalations_unidentified,
                    repairs_overflowed: bot.repairs_overflowed(),
                    repairs_unservable: bot.repairs_unservable(),
                    frames_recovered: witness.frames_recovered,
                    reanchors: witness.reanchors,
                    unjudged_ticks: witness.unjudged_ticks,
                    judged_ticks: witness.judged_ticks,
                    shown_ticks: witness.shown_ticks,
                    frames_rejected: witness.frames_rejected,
                    frames_rejected_unanchored: witness.frames_rejected_unanchored,
                    watches_unanchored: witness.watches_unanchored,
                    frames_deferred: witness.frames_deferred,
                    deferrals_overflowed: witness.deferrals_overflowed,
                    deferrals_pruned: witness.deferrals_pruned,
                    deferrals_dropped_in_drain: witness.deferrals_dropped_in_drain,
                    deferrals_replaced: witness.deferrals_replaced,
                    deferrals_stale: witness.deferrals_stale,
                    deferrals_held: witness.deferrals_held,
                    judgements_deferred: witness.judgements_deferred,
                    replicas,
                    tagged,
                    proxied,
                    undecodable,
                }
            })
            .collect();

        // Summed across the swarm rather than taken from the worst peer: the
        // share is a property of the traffic mix, and one peer's mix is noisy
        // when profiles differ.
        let lanes = self.bots.iter().map(Bot::lanes).fold(
            orrery_net::budget::LaneTally::default(),
            |mut total, peer| {
                total.replication_bytes += peer.replication_bytes;
                total.witness_bytes += peer.witness_bytes;
                total.control_bytes += peer.control_bytes;
                total.replication_shed += peer.replication_shed;
                total
            },
        );

        SwarmReport {
            identity: RunIdentity {
                seed: self.config.seed,
                impairment: self.config.impairment,
                target: env!("P1_SWARM_TARGET"),
                commit: env!("P1_SWARM_COMMIT"),
            },
            game: "regolith",
            ruleset_version: REGOLITH_RULESET.version,
            scenarios: PILOT_SCENARIOS.map(|scenario| scenario.name()),
            started_at_unix_secs: self.config.started_at_unix_secs,
            peers: self.total_peers(),
            seconds: self.config.seconds,
            ticks,
            worst_peak_upload_bits: per_peer
                .iter()
                .map(|p| p.peak_upload_bits)
                .max()
                .unwrap_or(0),
            worst_p99_upload_bits: per_peer
                .iter()
                .map(|p| p.p99_upload_bits)
                .max()
                .unwrap_or(0),
            min_cells_visited: per_peer.iter().map(|p| p.cells_visited).min().unwrap_or(0),
            total_shed: per_peer.iter().map(|p| p.shed).sum(),
            total_boundary_flips: per_peer.iter().map(|p| p.boundary_flips).sum(),
            total_proxy_pops: per_peer.iter().map(|p| p.proxy_pops).sum(),
            total_interest_churn: per_peer.iter().map(|p| p.interest_churn).sum(),
            stranded_in_flight: self.router.in_flight(),
            total_undecodable: per_peer.iter().map(|p| p.undecodable).sum(),
            total_replicas: per_peer.iter().map(|p| p.replicas).sum(),
            witnessing: self.config.witnessing,
            external: self.exterior.as_ref().map(|exterior| ExteriorReport {
                index: exterior.index,
                connected: exterior
                    .link
                    .connected
                    .load(std::sync::atomic::Ordering::Relaxed),
                uplink_frames: exterior.uplink_frames,
                downlink_frames: exterior.downlink_frames,
                downlink_dropped: exterior.downlink_dropped,
                said_goodbye: exterior.goodbye.load(std::sync::atomic::Ordering::Relaxed),
                witness_anchored: exterior.witness_anchored,
            }),
            player_hours: self.total_peers() as f64 * self.config.seconds as f64 / 3_600.0,
            total_gaps: per_peer.iter().map(|p| p.gaps).sum(),
            total_false_positives: per_peer.iter().map(|p| p.false_positives).sum(),
            conviction: self.config.cheats.map(|cheats| ConvictionReport {
                tamper: cheats.tamper.name(),
                tampered_peers: per_peer.iter().filter(|p| p.tamper.is_some()).count(),
                tampered_peers_that_diverged: per_peer
                    .iter()
                    .filter(|p| p.tamper.is_some() && p.first_tampered_tick.is_some())
                    .count(),
                tampered_peers_convicted: per_peer
                    .iter()
                    .filter(|p| p.tamper.is_some() && p.convicted_at_tick.is_some())
                    .count(),
                first_tampered_tick: per_peer.iter().filter_map(|p| p.first_tampered_tick).min(),
                worst_detection_ticks: per_peer
                    .iter()
                    .filter_map(|p| {
                        Some(p.convicted_at_tick?.saturating_sub(p.first_tampered_tick?))
                    })
                    .max(),
                reports_against_tampered: per_peer.iter().map(|p| p.reports_against_tampered).sum(),
                // `reports` on an honest subject and nothing else: the same
                // counter the false-positive clause reads, surfaced here
                // because the conviction leg is where it can be non-zero for
                // an interesting reason.
                reports_against_honest: per_peer.iter().map(|p| p.reports).sum(),
                adjudicated: docket.adjudicated,
                confirms: docket.confirms,
                exonerates: docket.exonerates,
                evidence_forged: docket.evidence_forged,
                unadjudicable: docket.unadjudicable,
            }),
            total_escalations_filed: per_peer.iter().map(|p| p.escalations_filed).sum(),
            total_escalations_shadowed: per_peer.iter().map(|p| p.escalations_shadowed).sum(),
            total_escalations_unservable: per_peer.iter().map(|p| p.escalations_unservable).sum(),
            total_escalations_unidentified: per_peer
                .iter()
                .map(|p| p.escalations_unidentified)
                .sum(),
            total_frames_recovered: per_peer.iter().map(|p| p.frames_recovered).sum(),
            total_reanchors: per_peer.iter().map(|p| p.reanchors).sum(),
            total_unjudged_ticks: per_peer.iter().map(|p| p.unjudged_ticks).sum(),
            total_judged_ticks: per_peer.iter().map(|p| p.judged_ticks).sum(),
            total_shown_ticks: per_peer.iter().map(|p| p.shown_ticks).sum(),
            total_frames_rejected: per_peer.iter().map(|p| p.frames_rejected).sum(),
            total_frames_rejected_unanchored: per_peer
                .iter()
                .map(|p| p.frames_rejected_unanchored)
                .sum(),
            total_watches_unanchored: per_peer.iter().map(|p| p.watches_unanchored).sum(),
            total_frames_deferred: per_peer.iter().map(|p| p.frames_deferred).sum(),
            total_judgements_deferred: per_peer.iter().map(|p| p.judgements_deferred).sum(),
            total_deferrals_overflowed: per_peer.iter().map(|p| p.deferrals_overflowed).sum(),
            total_deferrals_pruned: per_peer.iter().map(|p| p.deferrals_pruned).sum(),
            total_deferrals_dropped_in_drain: per_peer
                .iter()
                .map(|p| p.deferrals_dropped_in_drain)
                .sum(),
            total_deferrals_replaced: per_peer.iter().map(|p| p.deferrals_replaced).sum(),
            total_deferrals_stale: per_peer.iter().map(|p| p.deferrals_stale).sum(),
            total_deferrals_held: per_peer.iter().map(|p| p.deferrals_held).sum(),
            deferral_ledger_balances: per_peer.iter().all(|p| {
                p.frames_deferred
                    == p.frames_recovered
                        + p.deferrals_overflowed
                        + p.deferrals_pruned
                        + p.deferrals_dropped_in_drain
                        + p.deferrals_replaced
                        + p.deferrals_stale
                        + p.deferrals_held
            }),
            observation_coverage: {
                let judged: u64 = per_peer.iter().map(|p| p.judged_ticks).sum();
                let shown: u64 = per_peer.iter().map(|p| p.shown_ticks).sum();
                // Shown, not judged-plus-abandoned. A witness that stops
                // judging also stops abandoning, so measuring against its own
                // abandonments scores a watch that died at 100% — which is the
                // reading this clause exists to refuse.
                if shown == 0 {
                    0.0
                } else {
                    judged as f64 / shown as f64
                }
            },
            replication_bytes: lanes.replication_bytes,
            witness_bytes: lanes.witness_bytes,
            control_bytes: lanes.control_bytes,
            witness_lane_share: lanes.witness_share(),
            witness_lane_bits_per_sec: {
                let peer_seconds = self.total_peers() as u64 * self.config.seconds.max(1);
                lanes.witness_bytes * 8 / peer_seconds.max(1)
            },
            link: self.router.counters.into(),
            per_peer,
            late_join,
        }
    }
}

/// The `p`-th percentile of a sorted slice.
#[must_use]
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() * p).div_ceil(100).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

/// Why a run failed the P1 criterion (docs/11-roadmap.md §P1).
#[derive(Debug, Clone, Serialize)]
pub struct CriterionFailure {
    /// Which clause failed.
    pub clause: &'static str,
    /// What was observed.
    pub detail: String,
}

/// The thresholds a run is judged against.
///
/// Named rather than positional because the legs do not share them: the
/// witnessed run deals behavioural profiles the cruise-only run does not, and
/// three of these numbers move with it (see `scripts/p1-swarm-gate.sh`). Four
/// bare scalars at a call site is how a leg silently gets judged against
/// another leg's allowances.
#[derive(Debug, Clone, Copy)]
pub struct Criterion {
    /// The D6/D16 per-peer upload budget, bits per simulated second.
    pub budget_bits: u64,
    /// Distinct interest cells the least-travelled peer must have visited.
    pub min_cells: usize,
    /// Visible proxy pops tolerated.
    pub max_pops: u64,
    /// Packets the send path may shed for want of budget.
    ///
    /// Zero is the criterion. It is a knob only because the witness lane makes
    /// the transient at island formation real: a peer recovering from a hitch
    /// serves its witnesses' repair burst on the unsheddable control lane and
    /// sheds the cheap lane to afford it (docs/03-replication.md §5.3a). That
    /// count is flat between five minutes and an hour, which is what
    /// distinguishes it from an overrun.
    pub max_shed: u64,
    /// Ticks a deviation may survive between first changing the subject's state
    /// and an independent re-run of a filed report confirming it.
    ///
    /// P4's demo criterion says *within one adjudication window*, and the window
    /// is [`MAX_ADJUDICATION_TICKS`] — 180 ticks, 3 s. From `orrery_protocol`
    /// rather than a number chosen here: it is the wire-level bound every
    /// adjudicator enforces, and a harness judging against its own constant
    /// would be measuring itself.
    pub max_detection_ticks: u64,
}

impl Default for Criterion {
    fn default() -> Self {
        Self {
            budget_bits: 1_000_000,
            min_cells: 64,
            max_pops: 0,
            max_shed: 0,
            max_detection_ticks: MAX_ADJUDICATION_TICKS,
        }
    }
}

impl SwarmReport {
    /// Check the report against the P1 demo criterion.
    ///
    /// Every clause, or the run does not count — the phase gate is the whole
    /// sentence, not the convenient half of it.
    #[must_use]
    pub fn against_criterion(&self, criterion: Criterion) -> Vec<CriterionFailure> {
        let Criterion {
            budget_bits,
            min_cells,
            max_pops,
            max_shed,
            max_detection_ticks,
        } = criterion;
        let mut failures = Vec::new();

        if self.worst_peak_upload_bits > budget_bits {
            failures.push(CriterionFailure {
                clause: "sustained upload ≤ 1 Mbps",
                detail: format!(
                    "peak {} bits/s across {} peers exceeds {budget_bits}",
                    self.worst_peak_upload_bits, self.peers
                ),
            });
        }
        if self.total_shed > max_shed {
            // Shedding means the budget was reached, which the criterion treats
            // as a failure rather than a success of the backstop: a peer that
            // had to drop state did not stay *within* budget, it was held to it.
            failures.push(CriterionFailure {
                clause: "no load shed to stay within budget",
                detail: format!("{} packets shed (allowance {max_shed})", self.total_shed),
            });
        }
        // Guard against the harness measuring an empty world. Every clause
        // below is about interest, and interest over zero replicas is
        // vacuously perfect.
        if self.total_undecodable > 0 {
            failures.push(CriterionFailure {
                clause: "the harness observes what it sends",
                detail: format!(
                    "{} inbound state packets did not decode",
                    self.total_undecodable
                ),
            });
        }
        if self.total_replicas == 0 {
            failures.push(CriterionFailure {
                clause: "the harness observes what it sends",
                detail: "no peer holds a single replica; every interest clause \
                         below would pass by describing an empty world"
                    .to_owned(),
            });
        }
        // P4's criterion, when the witness is running: every peer here is
        // honest by construction — each logs exactly the inputs it applied — so
        // any signal beyond a gap accuses someone who did nothing wrong. A gap
        // is a question and is counted separately.
        if self.witnessing && self.total_false_positives > 0 {
            failures.push(CriterionFailure {
                clause: "no false-positive discrepancy signal against an honest peer",
                detail: format!(
                    "{} signals raised across {:.0} player-hours ({} gaps, which are repairs \
                     rather than accusations)",
                    self.total_false_positives, self.player_hours, self.total_gaps
                ),
            });
        }
        // The clause above is only worth reading if the witness was still
        // watching. A count of zero findings proves nothing about a witness
        // that went blind, and going blind is what used to happen: a watch that
        // gave up on a hole never asked again and never judged again, so within
        // about twenty-five simulated seconds every watch that had met loss was
        // finished and the run accumulated player-hours against a witness that
        // had stopped looking.
        //
        // The signature of that failure is decay. Measured at eight peers over
        // 30/120/300 simulated seconds it ran 82.2% → 76.8% → 75.8%, falling as
        // more watches died; with re-anchoring the same runs hold 83.4% → 82.9%
        // → 83.0%, which is a steady state rather than a slope.
        //
        // Those figures predate the frame cadence being derived from the lane's
        // budget share (docs/03-replication.md §5.3a, docs/11-roadmap.md §P4).
        // At the criterion population the clause now holds rather than fails,
        // and at both ends of the criterion's 3–5% loss band: **100.0% clean,
        // 100.0% at 3% loss, 100.0% at 5%**, all at 32 peers with zero false
        // positives.
        //
        // It read 96.0% and 93.8% under loss until watches stopped dying on
        // their first lost frame. What that deficit was is worth keeping here,
        // because the plausible reading was wrong: it was not timeline the
        // repair failed to recover — the deferral ledger balances at
        // essentially 100% recovered at both ends — it was whole *watches*, 9
        // of 224 at 3% and 14 at 5%, each shown its subject's entire hour and
        // judging none of it. Coverage is the only figure in this report that
        // could see them.
        //
        // Raising the threshold to today's number, or lowering it to
        // accommodate a run that misses, are both measurements rather than
        // edits; the number here is the phase's target and stays put.
        const MIN_COVERAGE: f64 = 0.95;
        if self.witnessing && self.observation_coverage < MIN_COVERAGE {
            failures.push(CriterionFailure {
                clause: "the witness keeps watching for the whole run",
                detail: format!(
                    "{:.1}% of the timeline shown to a witness was judged ({} of {} ticks, \
                     {} abandoned across {} re-anchors); below {:.0}% a false-positive count \
                     of zero says more about the witness than about the swarm",
                    self.observation_coverage * 100.0,
                    self.total_judged_ticks,
                    self.total_shown_ticks,
                    self.total_unjudged_ticks,
                    self.total_reanchors,
                    MIN_COVERAGE * 100.0,
                ),
            });
        }
        // A run that attached an external peer is only evidence if the peer
        // stayed joined and traffic actually flowed both ways. A silent slot
        // would otherwise bank its player-hour while measuring nothing — the
        // exact shape #375 exists to refuse.
        if let Some(external) = &self.external {
            let clean_close = external.connected || external.said_goodbye;
            if !clean_close {
                failures.push(CriterionFailure {
                    clause: "the external peer stays connected",
                    detail: format!(
                        "the bridge reported a disconnect; {} uplink / {} downlink frames \
                         before it dropped, {} downlink refused on a full queue",
                        external.uplink_frames, external.downlink_frames, external.downlink_dropped,
                    ),
                });
            }
            if external.uplink_frames == 0 || external.downlink_frames == 0 {
                failures.push(CriterionFailure {
                    clause: "the external peer participates",
                    detail: format!(
                        "{} uplink / {} downlink frames moved; an island member that sends \
                         or receives nothing measures nothing",
                        external.uplink_frames, external.downlink_frames,
                    ),
                });
            }
            if external.downlink_dropped > 0 {
                failures.push(CriterionFailure {
                    clause: "the host keeps up with its own clock",
                    detail: format!(
                        "{} downlink frames were refused on a full queue; the pump fell \
                         behind the real-time tick",
                        external.downlink_dropped,
                    ),
                });
            }
        }
        if self.witnessing && self.total_gaps == 0 && self.link.dropped > 0 {
            failures.push(CriterionFailure {
                clause: "the witness sees the stream it is judging",
                detail: "packets were dropped and no peer detected a single chain gap; \
                         the witness is not following the logs it was given"
                    .to_owned(),
            });
        }
        // P4's *other* half — the demo criterion proper. Only alive when the
        // harness fielded a modified client, and every clause below exists
        // because a plausible-looking pass could be reached without it.
        if let Some(conviction) = &self.conviction {
            // Vacuity first, because everything after it is measured against a
            // cheat that had to have done something. `Tamper::SpeedMultiplier`
            // raises an archetype's ceilings by 1.5×, and this roam requests
            // exactly the interceptor's `max_accel_mmss` — so on that slot both
            // builds clamp to the same number and produce byte-identical state.
            // The modified peers are pinned to the cruiser slot for that
            // reason; this clause is what says so out loud, and what catches
            // the next cheat that turns out to be inert at these parameters.
            if conviction.tampered_peers_that_diverged < conviction.tampered_peers {
                failures.push(CriterionFailure {
                    clause: "the modified client actually diverges from the shipping rules",
                    detail: format!(
                        "{} of {} modified peers produced state the shipping rules would not have; \
                         the rest ran a cheat that is inert at these parameters, and every clause \
                         below would hold over a swarm in which nothing happened",
                        conviction.tampered_peers_that_diverged, conviction.tampered_peers,
                    ),
                });
            }
            if conviction.tampered_peers_convicted < conviction.tampered_peers {
                failures.push(CriterionFailure {
                    clause: "a modified client is convicted on replay",
                    detail: format!(
                        "{} of {} modified peers reached Verdict::Confirms under independent \
                         re-execution ({} reports adjudicated: {} confirms, {} exonerates, \
                         {} evidence-forged, {} unadjudicable)",
                        conviction.tampered_peers_convicted,
                        conviction.tampered_peers,
                        conviction.adjudicated,
                        conviction.confirms,
                        conviction.exonerates,
                        conviction.evidence_forged,
                        conviction.unadjudicable,
                    ),
                });
            }
            // The other side of the same coin, and the one D17 risk 3 is
            // about. A pipeline that convicts the cheat by accusing everybody
            // has not met this criterion, it has failed the other one.
            if conviction.reports_against_honest > 0 {
                failures.push(CriterionFailure {
                    clause: "no report is filed against an honest peer",
                    detail: format!(
                        "{} reports were filed against peers the harness did not modify, \
                         alongside {} against peers it did",
                        conviction.reports_against_honest, conviction.reports_against_tampered,
                    ),
                });
            }
            if let Some(ticks) = conviction.worst_detection_ticks {
                if ticks > max_detection_ticks {
                    failures.push(CriterionFailure {
                        clause: "a modified client is convicted within one adjudication window",
                        detail: format!(
                            "the worst deviation survived {ticks} ticks between first changing \
                             the subject's state and being confirmed, against a window of \
                             {max_detection_ticks}",
                        ),
                    });
                }
            }
        } else if self.witnessing && self.total_escalations_filed > 0 {
            // Nobody was modified and something was filed anyway. Only reachable
            // under `--enforce`, which is exactly the configuration in which
            // "files nothing at all" is a claim worth checking rather than a
            // tautology of shadow mode.
            failures.push(CriterionFailure {
                clause: "an unmodified swarm files no report at all",
                detail: format!(
                    "{} reports were filed across a swarm in which every peer runs the shipping \
                     rules",
                    self.total_escalations_filed
                ),
            });
        }
        // Filing is opt-in and the opt-in is a `WitnessIdentity` per peer. A
        // non-zero count here is the harness silently back in the posture it
        // spent this whole lane leaving: detecting, and unable to file.
        if self.witnessing && self.total_escalations_unidentified > 0 {
            failures.push(CriterionFailure {
                clause: "every witness can sign what it raises",
                detail: format!(
                    "{} escalations stopped for want of a WitnessIdentity; the peer detected a \
                     deviation and had no key to file it under",
                    self.total_escalations_unidentified
                ),
            });
        }
        if self.total_proxy_pops > max_pops {
            failures.push(CriterionFailure {
                clause: "interest churn absorbed without visible proxy pops",
                detail: format!(
                    "{} entities were demoted and re-promoted inside one second \
                     (allowance {max_pops}) out of {} churn events",
                    self.total_proxy_pops, self.total_interest_churn
                ),
            });
        }
        if self.stranded_in_flight > 0 {
            failures.push(CriterionFailure {
                clause: "the link drains",
                detail: format!(
                    "{} packets still in flight when the run ended",
                    self.stranded_in_flight
                ),
            });
        }
        if self.total_boundary_flips > 0 {
            failures.push(CriterionFailure {
                clause: "no entity thrashes cells at a boundary",
                detail: format!(
                    "{} commitments returned to the cell just left; the 10% \
                     hysteresis margin should make this zero",
                    self.total_boundary_flips
                ),
            });
        }
        if self.min_cells_visited < min_cells {
            failures.push(CriterionFailure {
                clause: "roaming across ≥64 interest cells",
                detail: format!(
                    "the least-travelled peer visited {} cells",
                    self.min_cells_visited
                ),
            });
        }
        if let Some(join) = &self.late_join {
            if join.in_neighbourhood == 0 {
                failures.push(CriterionFailure {
                    clause: "the late-join check is not vacuous",
                    detail: "no peer was in the joiner's neighbourhood, so \
                             'receives only its neighborhood' held by receiving \
                             nothing"
                        .to_owned(),
                });
            }
            if join.tracked > join.in_neighbourhood {
                failures.push(CriterionFailure {
                    clause: "a late joiner receives only its 27-cell neighborhood",
                    detail: format!(
                        "tracked {} peers with only {} in the neighbourhood",
                        join.tracked, join.in_neighbourhood
                    ),
                });
            }
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The P1 criterion as the gate's clean leg states it.
    const STRICT: Criterion = Criterion {
        budget_bits: 1_000_000,
        min_cells: 64,
        max_pops: 0,
        max_shed: 0,
        max_detection_ticks: MAX_ADJUDICATION_TICKS,
    };

    /// A report in which every clause holds, witnessing included.
    ///
    /// The tests below each break exactly one thing and assert which clause
    /// notices, which is only meaningful against a baseline that raises
    /// nothing. Built by hand rather than by running a swarm: what is under
    /// test is the judgement, and a judgement that can only be exercised by a
    /// forty-second simulation is a judgement nobody exercises.
    fn passing() -> SwarmReport {
        SwarmReport {
            identity: RunIdentity {
                seed: 1,
                impairment: Impairment::p4_profile(),
                target: "test",
                commit: "test",
            },
            game: "regolith",
            ruleset_version: REGOLITH_RULESET.version,
            scenarios: PILOT_SCENARIOS.map(|scenario| scenario.name()),
            started_at_unix_secs: None,
            peers: 32,
            seconds: 3_600,
            ticks: 3_600 * TICK_HZ,
            per_peer: Vec::new(),
            worst_peak_upload_bits: 973_000,
            worst_p99_upload_bits: 906_000,
            min_cells_visited: 81,
            total_shed: 0,
            total_boundary_flips: 0,
            total_proxy_pops: 0,
            total_interest_churn: 8_426,
            stranded_in_flight: 0,
            total_undecodable: 0,
            total_replicas: 992,
            witnessing: true,
            external: None,
            player_hours: 32.0,
            total_gaps: 13_009,
            total_false_positives: 0,
            conviction: None,
            total_escalations_filed: 0,
            total_escalations_shadowed: 0,
            total_escalations_unservable: 0,
            total_escalations_unidentified: 0,
            total_frames_recovered: 0,
            total_reanchors: 0,
            total_unjudged_ticks: 0,
            total_judged_ticks: 3_864_390,
            total_shown_ticks: 4_026_190,
            total_frames_rejected: 0,
            total_frames_rejected_unanchored: 0,
            total_watches_unanchored: 0,
            total_frames_deferred: 0,
            total_judgements_deferred: 0,
            total_deferrals_overflowed: 0,
            total_deferrals_pruned: 0,
            total_deferrals_dropped_in_drain: 0,
            total_deferrals_replaced: 0,
            total_deferrals_stale: 0,
            total_deferrals_held: 0,
            deferral_ledger_balances: true,
            observation_coverage: 0.96,
            replication_bytes: 0,
            witness_bytes: 0,
            control_bytes: 0,
            witness_lane_share: 0.45,
            witness_lane_bits_per_sec: 194_000,
            link: LinkReport {
                delivered: 2_147_904,
                dropped: 66_520,
                delayed: 0,
                bytes: 0,
            },
            late_join: Some(LateJoinReport {
                neighbourhood: 27,
                roster: 31,
                in_neighbourhood: 15,
                tracked: 15,
            }),
        }
    }

    /// Clauses `report` raises, by name.
    fn clauses(report: &SwarmReport, criterion: Criterion) -> Vec<&'static str> {
        report
            .against_criterion(criterion)
            .into_iter()
            .map(|failure| failure.clause)
            .collect()
    }

    #[test]
    fn a_witnessed_run_that_holds_raises_nothing() {
        assert!(clauses(&passing(), STRICT).is_empty());
    }

    // Clause: no false-positive discrepancy signal against an honest peer.

    #[test]
    fn a_signal_against_an_honest_peer_fails_the_run() {
        let mut report = passing();
        report.total_false_positives = 1;
        assert!(clauses(&report, STRICT)
            .contains(&"no false-positive discrepancy signal against an honest peer"));
    }

    #[test]
    fn the_false_positive_clause_is_silent_when_the_witness_did_not_run() {
        // Not merely untested: without `--witness` the counter is structurally
        // zero, and a clause that fired on it would be judging a run that never
        // made a claim.
        let mut report = passing();
        report.witnessing = false;
        report.total_false_positives = 7;
        assert!(clauses(&report, STRICT).is_empty());
    }

    // Clauses: P4's demo criterion, the conviction half.

    /// A conviction leg in which every clause holds: one modified peer, it
    /// diverged, it was convicted, and nobody else was accused.
    fn convicting() -> SwarmReport {
        SwarmReport {
            conviction: Some(ConvictionReport {
                tamper: Tamper::SpeedMultiplier.name(),
                tampered_peers: 1,
                tampered_peers_that_diverged: 1,
                tampered_peers_convicted: 1,
                first_tampered_tick: Some(0),
                worst_detection_ticks: Some(32),
                reports_against_tampered: 42,
                reports_against_honest: 0,
                adjudicated: 42,
                confirms: 42,
                exonerates: 0,
                evidence_forged: 0,
                unadjudicable: 0,
            }),
            total_escalations_filed: 42,
            ..passing()
        }
    }

    #[test]
    fn a_conviction_leg_that_holds_raises_nothing() {
        assert!(clauses(&convicting(), STRICT).is_empty());
    }

    #[test]
    fn a_cheat_that_never_diverged_fails_the_run() {
        // The vacuity trap this clause exists for, and it is not hypothetical:
        // `Tamper::SpeedMultiplier` clamps to the same number as the shipping
        // build on an interceptor slot at this roam's requested acceleration.
        // Fielded there, the modified peer is byte-identical to an honest one —
        // nothing to report, nothing filed, and "no report against an honest
        // peer" and "convicted within one window" both hold over a swarm in
        // which nothing happened.
        let mut report = convicting();
        let conviction = report.conviction.as_mut().expect("a conviction leg");
        conviction.tampered_peers_that_diverged = 0;
        conviction.tampered_peers_convicted = 0;
        conviction.first_tampered_tick = None;
        conviction.worst_detection_ticks = None;
        conviction.reports_against_tampered = 0;
        conviction.adjudicated = 0;
        conviction.confirms = 0;
        report.total_escalations_filed = 0;
        let raised = clauses(&report, STRICT);
        assert!(raised.contains(&"the modified client actually diverges from the shipping rules"));
        assert!(raised.contains(&"a modified client is convicted on replay"));
    }

    #[test]
    fn a_modified_peer_nobody_convicted_fails_the_run() {
        let mut report = convicting();
        report
            .conviction
            .as_mut()
            .expect("a conviction leg")
            .tampered_peers_convicted = 0;
        assert!(clauses(&report, STRICT).contains(&"a modified client is convicted on replay"));
    }

    #[test]
    fn convicting_the_cheat_by_accusing_everybody_fails_the_run() {
        // D17 risk 3 in one assertion: a pipeline that reaches the right
        // verdict about the modified peer and files against honest ones has
        // failed the criterion it was measuring, not met the one it was aiming
        // at.
        let mut report = convicting();
        report
            .conviction
            .as_mut()
            .expect("a conviction leg")
            .reports_against_honest = 3;
        assert!(clauses(&report, STRICT).contains(&"no report is filed against an honest peer"));
    }

    #[test]
    fn a_conviction_past_the_adjudication_window_fails_the_run() {
        let mut report = convicting();
        report
            .conviction
            .as_mut()
            .expect("a conviction leg")
            .worst_detection_ticks = Some(MAX_ADJUDICATION_TICKS + 1);
        assert!(clauses(&report, STRICT)
            .contains(&"a modified client is convicted within one adjudication window"));
    }

    #[test]
    fn a_conviction_at_the_window_holds() {
        // The comparison is `>`, so the window itself passes. Worth pinning for
        // the same reason the coverage floor is: the measured leg runs a long
        // way inside it, which is not much room in which to notice an
        // off-by-one.
        let mut report = convicting();
        report
            .conviction
            .as_mut()
            .expect("a conviction leg")
            .worst_detection_ticks = Some(MAX_ADJUDICATION_TICKS);
        assert!(clauses(&report, STRICT).is_empty());
    }

    #[test]
    fn an_unmodified_swarm_that_files_anything_fails_the_run() {
        let mut report = passing();
        report.total_escalations_filed = 1;
        assert!(clauses(&report, STRICT).contains(&"an unmodified swarm files no report at all"));
    }

    #[test]
    fn the_conviction_clauses_are_silent_on_an_honest_leg() {
        // `conviction` is `None` without `--cheat`, and a clause that fired on
        // it would be judging a run that never fielded a modified client.
        assert!(clauses(&passing(), STRICT).is_empty());
    }

    #[test]
    fn a_witness_that_cannot_sign_fails_the_run() {
        // The posture this whole lane exists to leave. Nothing else in the
        // report distinguishes "detected and could not file" from "found
        // nothing to file": both read as zero reports.
        let mut report = passing();
        report.total_escalations_unidentified = 5;
        assert!(clauses(&report, STRICT).contains(&"every witness can sign what it raises"));
    }

    #[test]
    fn the_signing_clause_is_silent_when_the_witness_did_not_run() {
        let mut report = passing();
        report.witnessing = false;
        report.total_escalations_unidentified = 5;
        assert!(clauses(&report, STRICT).is_empty());
    }

    // Clause: the witness keeps watching for the whole run.

    #[test]
    fn coverage_below_the_floor_fails_the_run() {
        let mut report = passing();
        report.observation_coverage = 0.949;
        assert!(clauses(&report, STRICT).contains(&"the witness keeps watching for the whole run"));
    }

    #[test]
    fn coverage_at_the_floor_holds() {
        // The comparison is `<`, so the floor itself passes. Worth pinning: the
        // impaired leg runs four points above it, which is not much room in
        // which to discover an off-by-one.
        let mut report = passing();
        report.observation_coverage = 0.95;
        assert!(clauses(&report, STRICT).is_empty());
    }

    #[test]
    fn a_blind_witness_is_caught_even_with_zero_findings() {
        // The failure this clause exists for: a watch that gave up reports no
        // findings for the same reason an honest swarm does.
        let mut report = passing();
        report.observation_coverage = 0.0;
        report.total_judged_ticks = 0;
        report.total_false_positives = 0;
        assert_eq!(
            clauses(&report, STRICT),
            vec!["the witness keeps watching for the whole run"]
        );
    }

    #[test]
    fn the_coverage_clause_is_silent_when_the_witness_did_not_run() {
        let mut report = passing();
        report.witnessing = false;
        report.observation_coverage = 0.0;
        report.total_judged_ticks = 0;
        report.total_shown_ticks = 0;
        assert!(clauses(&report, STRICT).is_empty());
    }

    // Clause: the witness sees the stream it is judging.

    #[test]
    fn a_lossy_link_with_no_detected_gap_fails_the_run() {
        let mut report = passing();
        report.total_gaps = 0;
        assert!(clauses(&report, STRICT).contains(&"the witness sees the stream it is judging"));
    }

    #[test]
    fn no_gaps_on_a_clean_link_is_not_a_finding() {
        // Zero gaps is the *expected* reading when nothing was dropped; the
        // clause is about a witness that missed the drops, not about the drops.
        let mut report = passing();
        report.identity.impairment = Impairment::default();
        report.total_gaps = 0;
        report.link.dropped = 0;
        report.observation_coverage = 1.0;
        assert!(clauses(&report, STRICT).is_empty());
    }

    #[test]
    fn the_stream_clause_is_silent_when_the_witness_did_not_run() {
        let mut report = passing();
        report.witnessing = false;
        report.total_gaps = 0;
        assert!(clauses(&report, STRICT).is_empty());
    }

    // The witnessed leg's own thresholds.

    #[test]
    fn the_witnessed_legs_thresholds_do_not_reach_the_witnessing_clauses() {
        // The relaxations `scripts/p1-swarm-gate.sh` gives the witnessed leg are
        // about roaming and about the island-formation shed transient. If either
        // silenced a witnessing clause the leg would be theatre, so pin that
        // neither does: the run below is judged with every allowance open and
        // still fails on the witness.
        let witnessed = Criterion {
            min_cells: 1,
            max_pops: 64,
            max_shed: 512,
            ..STRICT
        };
        let mut report = passing();
        report.min_cells_visited = 1;
        report.total_shed = 206;
        report.total_proxy_pops = 3;
        assert!(clauses(&report, witnessed).is_empty());

        report.total_false_positives = 1;
        report.observation_coverage = 0.5;
        report.total_gaps = 0;
        assert_eq!(
            clauses(&report, witnessed),
            vec![
                "no false-positive discrepancy signal against an honest peer",
                "the witness keeps watching for the whole run",
                "the witness sees the stream it is judging",
            ]
        );
    }

    #[test]
    fn the_shed_allowance_is_a_ceiling_not_an_exemption() {
        let mut report = passing();
        report.total_shed = 513;
        let witnessed = Criterion {
            max_shed: 512,
            ..STRICT
        };
        assert!(clauses(&report, witnessed).contains(&"no load shed to stay within budget"));
    }

    #[test]
    fn the_modified_build_is_only_ever_dealt_to_a_cruising_slot() {
        // An idling bot never asks for thrust, so a cheat that raises an
        // acceleration ceiling changes nothing about it: dealt there, the
        // modified peer would be byte-identical to an honest one and the whole
        // conviction leg would pass over a swarm in which nothing happened.
        // The vacuity clause would catch it — but catching it as a failed
        // nightly is worse than never dealing it.
        for count in 1..=8 {
            for index in tampered_indices(32, count) {
                assert_eq!(
                    crate::profile::Profile::for_index(index, true),
                    crate::profile::Profile::Cruise,
                    "index {index} runs a profile that may never thrust",
                );
            }
        }
    }

    #[test]
    fn the_demo_criterions_eight_peer_island_can_field_a_cheat() {
        // "A modified client applying a 1.5× speed multiplier joins an 8-peer
        // island" — stated literally, so the population the criterion names has
        // to have a cruising slot to deal it to.
        assert_eq!(tampered_indices(8, 1), vec![0]);
    }

    #[test]
    fn the_identity_block_carries_no_wall_clock() {
        // The reproducible body must be a function of the seed alone: two runs
        // of one seed may differ only in the sidecar.
        let stamped = SwarmReport {
            started_at_unix_secs: Some(1_755_300_000),
            ..passing()
        };
        assert_eq!(
            serde_json::to_string(&stamped.identity).unwrap(),
            serde_json::to_string(&passing().identity).unwrap()
        );
    }

    #[test]
    fn a_dropped_uplink_is_acknowledged_only_after_the_router_decides() {
        use crate::bot::bot_key;
        use crate::exterior::{link_pair, UplinkAck};

        let mut swarm = Swarm::new(SwarmConfig {
            peers: 1,
            impairment: Impairment {
                loss: 1.0,
                ..Impairment::default()
            },
            ..SwarmConfig::default()
        });
        let (host_link, remote_link) = link_pair();
        swarm = swarm.with_external(bot_key(1).public(), None, host_link);
        remote_link
            .uplink
            .try_send(Frame {
                peer: 0,
                lane: Lane::Datagram,
                payload: UplinkDatagram {
                    sequence: 41,
                    payload: Bytes::from_static(b"must be dropped"),
                }
                .encode(),
            })
            .expect("uplink queue accepts the datagram");

        swarm
            .exterior
            .as_mut()
            .expect("external slot")
            .pump_uplink(7, &mut swarm.router);

        assert_eq!(swarm.router.counters.dropped, 1, "the router dropped it");
        let frame = remote_link
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
            .expect("one settled ACK");
        assert_eq!(frame.lane, Lane::Meta);
        assert_eq!(
            UplinkAck::decode(&frame.payload),
            Some(UplinkAck {
                sequence: 41,
                outcome: UplinkOutcome::Dropped,
            }),
            "a pre-decision success ACK would lie about the discarded frame"
        );
    }

    #[test]
    fn client_side_ack_loss_tracks_the_router_actual_drops() {
        use crate::bot::bot_key;
        use crate::exterior::{link_pair, UplinkAck};

        const SENT: usize = 1_000;
        let mut swarm = Swarm::new(SwarmConfig {
            peers: 1,
            impairment: Impairment {
                loss: 0.30,
                ..Impairment::default()
            },
            seed: 7,
            ..SwarmConfig::default()
        });
        let (host_link, remote_link) = link_pair();
        swarm = swarm.with_external(bot_key(1).public(), None, host_link);
        for sequence in 0..SENT as u64 {
            remote_link
                .uplink
                .try_send(Frame {
                    peer: 0,
                    lane: Lane::Datagram,
                    payload: UplinkDatagram {
                        sequence,
                        payload: Bytes::from_static(b"measured datagram"),
                    }
                    .encode(),
                })
                .expect("criterion-rate uplink fits the queue");
        }

        swarm
            .exterior
            .as_mut()
            .expect("external slot")
            .pump_uplink(11, &mut swarm.router);

        let mut acknowledged = 0usize;
        let mut client_dropped = 0u64;
        let mut seen = vec![false; SENT];
        while let Ok(frame) = remote_link
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
        {
            let ack = UplinkAck::decode(&frame.payload).expect("Meta frame is an uplink ACK");
            let index = usize::try_from(ack.sequence).expect("test sequence fits");
            assert!(index < SENT, "ACK identifies a sent datagram");
            assert!(!seen[index], "each datagram is acknowledged once");
            seen[index] = true;
            acknowledged += 1;
            client_dropped += u64::from(ack.outcome == UplinkOutcome::Dropped);
        }

        assert_eq!(acknowledged, SENT, "every decision produced one ACK");
        assert!(seen.into_iter().all(|settled| settled));
        assert_eq!(
            client_dropped, swarm.router.counters.dropped,
            "the remote's ACK-derived loss figure must equal actual router drops"
        );
    }

    /// The bridge is only worth its name if an exterior slot behaves like a
    /// bot slot: uplink frames enter the impaired router attributed to the
    /// external node, downlink frames carry the sender's index, and meta
    /// updates move the roster cell. Driven through `link_pair` — no sockets,
    /// no runtime.
    #[test]
    fn an_external_slot_routes_like_a_bot_slot() {
        use crate::bot::{bot_key, grid_of};
        use crate::exterior::{link_pair, Frame, Lane};

        let peers = 2usize;
        let mut swarm = Swarm::new(SwarmConfig {
            peers,
            seconds: 1,
            witnessing: false,
            ..SwarmConfig::default()
        });
        // Attach before formation: an island-mate that joins gets a slot in
        // the roster and a link from every bot, exactly as run() would order
        // it. Formed first, the bots would have no session to receive on.
        let ext_index = peers;
        let ext_node = bot_key(ext_index).public();
        let (host_link, remote_link) = link_pair();
        swarm = swarm.with_external(ext_node, None, host_link);
        swarm.form_island();

        // Up: the remote sends a datagram addressed to bot 0. The bridge must
        // feed it into the *router* — not around it — so it arrives at bot 0
        // attributed to the external node, exactly like any peer's traffic.
        // The payload is deliberately raw bytes with no channel tag: the link
        // layer counts exactly that as `untagged`, which makes it the cleanest
        // observable that the frame crossed router → session → receive drain
        // and stopped where a malformed packet should.
        remote_link
            .uplink
            .try_send(Frame {
                peer: 0,
                lane: Lane::Datagram,
                payload: UplinkDatagram {
                    sequence: 0,
                    payload: bytes::Bytes::from_static(b"not a canonical craft"),
                }
                .encode(),
            })
            .expect("the host queue accepts");
        let mut phase = [0u128; 6];
        swarm.tick_once(7, 20, &mut phase);
        swarm.tick_once(8, 20, &mut phase);
        let untagged = swarm.bots[0]
            .app
            .world()
            .resource::<orrery_net::peer_link::PeerLinkCounters>()
            .untagged;
        assert!(
            untagged >= 1,
            "the uplink frame never reached its addressed bot"
        );
        let ack_frame = remote_link
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
            .expect("the clean router decision was acknowledged");
        assert_eq!(
            UplinkAck::decode(&ack_frame.payload),
            Some(UplinkAck {
                sequence: 0,
                outcome: UplinkOutcome::Delivered,
            })
        );

        // Down: a bot's packet routed to the exterior slot lands on the
        // downlink queue naming the sender's slot.
        swarm.router.accept(
            9,
            swarm.bots[0].node,
            ext_index,
            bytes::Bytes::from_static(b"state for you"),
        );
        swarm.deliver(9);
        let mut downlink = None;
        for _ in 0..50 {
            let attempt = {
                let mut r = remote_link.downlink.lock().expect("downlink lock");
                r.try_recv()
            };
            if let Ok(frame) = attempt {
                downlink = Some(frame);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let downlink = downlink.expect("a routed packet landed on the downlink");
        assert_eq!(downlink.peer, 0, "downlink frames name the sender's slot");
        assert_eq!(downlink.lane, Lane::Datagram);

        // Meta: the remote reports a new cell; the roster view follows.
        let moved = crate::bot::cell_of(grid_of(
            &orrery_core::QPos::from_metres(900_000.0, 0.0, -900_000.0),
            crate::bot::default_cell_edge_m(),
        ));
        remote_link
            .uplink
            .try_send(crate::exterior::Frame {
                peer: u32::MAX,
                lane: Lane::Meta,
                payload: bytes::Bytes::from(moved.to_bits().to_le_bytes().to_vec()),
            })
            .expect("the uplink queue accepts meta frames");
        // The uplink pump is what turns meta frames into roster cells; drain
        // it the way collect_sends would.
        if let Some(exterior) = &mut swarm.exterior {
            exterior.pump_uplink(0, &mut swarm.router);
        }
        if let Some(exterior) = &mut swarm.exterior {
            while let Ok(raw) = {
                let mut r = exterior.link.meta.lock().expect("meta lock");
                r.try_recv()
            } {
                exterior.set_cell_from_bits(raw);
            }
            assert_eq!(exterior.cell(), moved, "meta updated the roster cell");
        } else {
            panic!("the exterior slot was attached");
        }
    }
}
