//! The swarm: N bots, a router between them, and the criterion they must meet.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use bytes::Bytes;
use serde::Serialize;

use orrery_core::CoreCodec;
use orrery_games::game::Tamper;
use orrery_games::regolith::order::Outcome;
use orrery_games::regolith::state::RegolithState;
use orrery_games::regolith::{campaign_rock_seeds, pilot::PILOT_SCENARIOS, REGOLITH_RULESET};
use orrery_net::peer_link::StreamMode;
use orrery_protocol::channels::{decode_replication, decode_replication_delta};
use orrery_protocol::coord::PeerEntry;
use orrery_protocol::{
    CellId, InterestCellCrossing, NodeId, PersistId, SeqPair, Tick, UniverseSeed,
    MAX_ADJUDICATION_TICKS,
};

use crate::adjudicate::{Adjudicator, Docket};
use crate::bot::{Bot, BotSpec, TICK_HZ};
use crate::delta_stats::{DeltaStats, DeltaStatsReport};
use crate::exterior::{
    ActiveSeat, Frame, HearsayContact, HearsayContacts, HearsaySource, Lane, StartManifest,
    UplinkAck, UplinkDatagram, UplinkOutcome,
};

use crate::router::{Impairment, Router, RouterCounters};
use crate::shot_interest::{ShotInterestReport, ShotInterestStats};

/// Five seconds at the fixed 60 Hz simulation cadence.
///
/// Serving the preceding buffer makes every delivered fold at least this old;
/// the current buffer is never a serving source.
const HEARSAY_FOLD_TICKS: u64 = 5 * TICK_HZ;

/// A broken transport is allowed this long to recover before its seat is freed.
/// Explicit goodbye bypasses the grace. No application-frame timer exists.
pub(crate) const TRANSPORT_CLOSE_GRACE_TICKS: u64 = 2 * TICK_HZ;

fn transport_close_grace_elapsed(first_closed_tick: u64, tick: u64) -> bool {
    tick.saturating_sub(first_closed_tick) >= TRANSPORT_CLOSE_GRACE_TICKS
}

/// One completed QUIC bind delivered by the asynchronous accept loop.
pub(crate) struct JoinedExternal {
    pub(crate) slot: usize,
    pub(crate) node: NodeId,
    pub(crate) session_id: String,
    pub(crate) anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
    pub(crate) link: crate::exterior::HostLink,
}

/// One active campaign seat's transport identity and release key.
pub(crate) struct LiveSeatBinding {
    pub(crate) node: NodeId,
    pub(crate) session_id: String,
}

/// Host-authored membership shared with admission and the live accept loop.
pub(crate) struct LiveMembership {
    pub(crate) attempt_id: String,
    pub(crate) active: BTreeMap<usize, LiveSeatBinding>,
    pub(crate) pending: BTreeSet<usize>,
    pub(crate) released_sessions: BTreeSet<String>,
    pub(crate) tick: u64,
    pub(crate) running: bool,
    pub(crate) path: Option<PathBuf>,
}

impl LiveMembership {
    /// Atomically republish the generation-bound binding feed.
    pub(crate) fn publish(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "attempt_id": self.attempt_id,
            "active_slots": self.active.keys().copied().collect::<Vec<_>>(),
            "released_sessions": self.released_sessions,
            "running": self.running,
        }))?;
        let temporary = path.with_extension("tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

struct LiveJoins {
    receiver: mpsc::Receiver<JoinedExternal>,
    membership: Arc<Mutex<LiveMembership>>,
    island_seats: usize,
}

/// One peer-authored crossing waiting for the host roster to apply it.
#[derive(Debug)]
struct HostInterestCrossing {
    node: NodeId,
    crossing: InterestCellCrossing,
}

/// How many times a freshly bound seat is re-sent its own membership.
///
/// One second apart, so a joiner that is still finishing its handshake when the
/// first copy goes out still gets one it can read.
const DEFERRED_MANIFEST_PUBLISHES: u8 = 5;

/// A newly bound seat's own membership, still owed to it.
#[derive(Debug, Clone, Copy)]
struct DeferredManifest {
    /// Copies still to send, including the one due at `next_tick`.
    publishes_left: u8,
    /// The tick the next copy is due at.
    next_tick: u64,
}

/// The last crossing order the host accepted for one stable bot seat.
#[derive(Debug, Clone, Copy)]
struct AppliedInterestCrossing {
    seq: SeqPair,
    tick: Tick,
    committed_cell: CellId,
}
/// The coordinator's bulk interest refresh remains 1 Hz (#653/#692).
const INTEREST_REFRESH_PERIOD_S: f64 = 1.0;

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
    /// State-send opportunities between sender-clocked keyframes.
    pub keyframe_every_sends: u64,
    /// Sustained upload ceiling installed in every real `UploadBudget` resource.
    pub upload_budget_bits: u64,
    /// Use #692's directional one-refresh-period interest coverage in manifests.
    pub swept_interest_margin: bool,
    /// Observe admitted per-(entity, link) replication delivery gaps.
    pub delivery_gap_instrumentation: bool,
    /// Link conditions.
    pub impairment: Impairment,
    /// Seed for impairment and the universe.
    pub seed: u64,
    /// Install Regolith's authored campaign content beside the player crowd.
    pub campaign: bool,
    /// Tick at which a late joiner appears, if any.
    pub late_join_tick: Option<u64>,
    /// Run the witness pipeline: every peer watches its witness set's entities
    /// and re-executes their signed logs (P4's input).
    pub witnessing: bool,
    /// Override whether bots use the four-profile witness stress mix.
    ///
    /// `None` preserves the shipping harness posture: witness runs are varied,
    /// unwitnessed runs are cruise-only. `Some` exists to separate those two
    /// variables in measurements.
    pub varied_profiles: Option<bool>,
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
    /// Emit the all-seat replica-scope diagnostic once per roster refresh.
    ///
    /// This is deliberately opt-in: eight active seats produce 56 directed,
    /// non-self decisions each second (3,360 lines per minute). The capture
    /// only observes the roster and must not affect either the roster or the
    /// replication path.
    pub replica_scope_capture: bool,
    /// Observe changed canonical bytes at the existing replication send seam.
    pub delta_stats: bool,
    /// Observe replica scope when target-authored shot verdicts resolve.
    pub shot_interest_stats: bool,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            peers: 32,
            seconds: 3_600,
            cell_edge_m: crate::bot::default_cell_edge_m(),
            send_hz: 20,
            keyframe_every_sends: 20,
            upload_budget_bits: 1_000_000,
            swept_interest_margin: false,
            delivery_gap_instrumentation: false,
            impairment: Impairment::default(),
            seed: 1,
            campaign: false,
            late_join_tick: None,
            witnessing: false,
            varied_profiles: None,
            cheats: None,
            enforcing: false,
            started_at_unix_secs: None,
            replica_scope_capture: false,
            delta_stats: false,
            shot_interest_stats: false,
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
    /// Commitments that returned to the cell just left, including real reversals.
    pub boundary_flips: u64,
    /// Most returns to one cell in any one-second interest-refresh period.
    pub max_boundary_returns_in_window: u64,
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
    /// Deltas dropped because their referenced keyframe was absent.
    pub deltas_unanchored: u64,
    /// Of those, deltas seen before any keyframe for their entity.
    pub deltas_without_any_keyframe: u64,
    /// Of those, deltas whose newer referenced keyframe never arrived.
    pub deltas_missing_newer_keyframe: u64,
    /// Of those, deltas whose anchor had already been superseded.
    pub deltas_with_superseded_keyframe: u64,
    /// Of those, deltas whose keyframe age underflowed their tick.
    pub deltas_with_invalid_reference: u64,
    /// Keyframes this sender built, charged, then discarded during a hitch.
    pub keyframes_discarded_while_stalled: u64,
    /// Deltas this sender built, charged, then discarded during a hitch.
    pub deltas_discarded_while_stalled: u64,
}

/// One bin in the distribution of per-seat boundary-return maxima.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BoundaryReturnHistogramBin {
    /// Returns to the same cell in one refresh period.
    pub returns_in_window: u64,
    /// Seats whose run-wide maximum had this value.
    pub seats: usize,
}

/// Boundary-return distribution for one behavioural profile.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BoundaryReturnProfileReport {
    /// Behavioural profile name.
    pub profile: &'static str,
    /// Highest one-second return count among seats with this profile.
    pub max_returns_in_window: u64,
    /// Distribution of per-seat maxima for this profile.
    pub histogram: Vec<BoundaryReturnHistogramBin>,
}

/// One sender/entity/recipient stream observed after the real upload meter.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGapPairReport {
    /// Authority transport identity.
    pub sender: NodeId,
    /// Canonical entity being replicated.
    pub entity: PersistId,
    /// Outgoing link recipient.
    pub recipient: NodeId,
    /// Replication messages admitted to this link.
    pub deliveries: u64,
    /// Largest completed gap between admitted messages, in simulation ticks.
    pub max_inter_delivery_gap_ticks: u64,
    /// Longest silence while this pair remained in the sender's audience.
    /// Includes leading and closing censored intervals.
    pub max_active_silence_ticks: u64,
    /// Right-censored gap from the final admitted message to the run end.
    ///
    /// Keeping this separate prevents a governor that goes silent after one
    /// delivery from disappearing from an inter-delivery-only statistic.
    pub trailing_gap_ticks: u64,
}

/// Degradation-honesty evidence for the sender-side governor lanes.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGapReport {
    /// Sender/entity/recipient streams that were active, including streams
    /// whose sender never admitted a message.
    pub pairs: Vec<DeliveryGapPairReport>,
    /// Completed inter-delivery intervals observed.
    pub completed_gaps: u64,
    /// Exact completed-gap distribution, keyed by simulation ticks.
    pub completed_gap_histogram: BTreeMap<u64, u64>,
    /// Median completed gap.
    pub p50_gap_ticks: u64,
    /// 95th-percentile completed gap.
    pub p95_gap_ticks: u64,
    /// 99th-percentile completed gap.
    pub p99_gap_ticks: u64,
    /// Worst completed or right-censored gap.
    pub max_gap_ticks: u64,
}

/// Cell-set size actually installed by #692's swept-margin primitive.
#[derive(Debug, Clone, Serialize)]
pub struct InterestMarginReport {
    /// Peer-refresh samples included.
    pub samples: u64,
    /// Smallest installed set.
    pub min_cells: usize,
    /// Arithmetic mean installed set size.
    pub mean_cells: f64,
    /// Largest installed set.
    pub max_cells: usize,
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
    /// State-send opportunities between keyframes in this run.
    pub keyframe_every_sends: u64,
    /// Sustained ceiling installed in the send path for this run.
    pub upload_budget_bits: u64,
    /// Whether #692's swept interest coverage fed the roster.
    pub swept_interest_margin: bool,
    /// Whether the four-profile witness stress mix was dealt.
    pub varied_profiles: bool,
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
    /// QUIC-authenticated transport identity admitted for this slot.
    pub node: NodeId,
    /// Whether the runner's clean end-of-run marker arrived.
    pub said_goodbye: bool,
    /// Whether the bridge believed the connection was alive at report time.
    pub connected: bool,
    /// Host ticks during which the bridge reported this slot connected.
    pub connected_ticks: u64,
    /// Frames forwarded from the remote into the router.
    pub uplink_frames: u64,
    /// Uplink datagrams retained by the impairment router.
    pub uplink_delivered: u64,
    /// Uplink datagrams discarded by the impairment router.
    pub uplink_dropped: u64,
    /// Frames queued for the remote out of router deliveries.
    pub downlink_frames: u64,
    /// Downlink frames refused because the queue was full. Zero at criterion
    /// rates; non-zero means the pump fell behind the swarm's clock.
    pub downlink_dropped: u64,
    /// Whether the peer shipped a witness anchor at its join tick. `false`
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
    /// Highest number of returns to one cell in any one-second window.
    pub max_boundary_returns_in_window: u64,
    /// Distribution of per-seat one-second boundary-return maxima.
    pub boundary_return_histogram: Vec<BoundaryReturnHistogramBin>,
    /// The same distribution split by behavioural profile.
    pub boundary_return_profiles: Vec<BoundaryReturnProfileReport>,
    /// A19 changed-byte measurements, present only with `--delta-stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_stats: Option<DeltaStatsReport>,
    /// Shot/interest measurements, present only with `--shot-interest-stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shot_interest_stats: Option<ShotInterestReport>,
    /// A20's post-meter degradation-honesty measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_gaps: Option<DeliveryGapReport>,
    /// Actual swept-cell sizes, present only on the swept-margin leg.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interest_margin: Option<InterestMarginReport>,
    /// Ceiling read back from every bot's real `UploadBudget` resource.
    pub meter_budget_bits: u64,
    /// Highest peak upload across the swarm, bits per simulated second.
    pub worst_peak_upload_bits: u64,
    /// Highest p99 upload across the swarm.
    pub worst_p99_upload_bits: u64,
    /// Fewest distinct cells any peer visited.
    pub min_cells_visited: usize,
    /// Total packets shed across the swarm.
    pub total_shed: u64,
    /// FIFO-shed replication anchors.
    pub shed_keyframes: u64,
    /// FIFO-shed keyframe-referenced deltas.
    pub shed_deltas: u64,
    /// Shed replication messages not recognized as either A19 wire form.
    pub shed_replication_other: u64,
    /// Total same-cell returns across the swarm, retained as travel evidence.
    pub total_boundary_flips: u64,
    /// Total visible proxy pops across the swarm.
    pub total_proxy_pops: u64,
    /// Total high-rate set entries and exits — the churn pops are judged against.
    pub total_interest_churn: u64,
    /// Packets still held by the link when the run ended.
    pub stranded_in_flight: usize,
    /// Inbound state packets no peer could decode.
    pub total_undecodable: u64,
    /// Deltas no receiver could apply for want of their exact keyframe.
    pub deltas_unanchored: u64,
    /// Unanchored deltas seen before any keyframe for their entity.
    pub deltas_without_any_keyframe: u64,
    /// Unanchored deltas whose newer referenced keyframe never arrived.
    pub deltas_missing_newer_keyframe: u64,
    /// Unanchored deltas whose referenced anchor had already been superseded.
    pub deltas_with_superseded_keyframe: u64,
    /// Unanchored deltas carrying an impossible keyframe reference.
    pub deltas_with_invalid_reference: u64,
    /// Keyframes built and charged, then discarded during simulated hitches.
    pub keyframes_discarded_while_stalled: u64,
    /// Deltas built and charged, then discarded during simulated hitches.
    pub deltas_discarded_while_stalled: u64,
    /// Replica entities held across the swarm at the end of the run.
    pub total_replicas: usize,
    /// Whether the witness pipeline ran.
    pub witnessing: bool,
    /// What each external peer did, ordered by swarm slot (#385, #571).
    pub external: Vec<ExteriorReport>,
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
    /// Mean replication-lane cost per peer over the run.
    pub replication_bits_per_sec: u64,
    /// Absolute keyframes offered to the send path, per recipient.
    pub keyframe_messages: u64,
    /// Keyframe-referenced deltas offered to the send path, per recipient.
    pub delta_messages: u64,
    /// Keyframe wire bytes offered to the send path, including overhead.
    pub keyframe_bytes: u64,
    /// Delta wire bytes offered to the send path, including overhead.
    pub delta_bytes: u64,
    /// Keyframes' share of replication messages, in 0.0–1.0.
    pub keyframe_message_share: f64,
    /// Keyframes' share of replication wire bytes, in 0.0–1.0.
    pub keyframe_byte_share: f64,
    /// Wire bytes the swarm spent on the verifiable-core lane: log frames and
    /// state claims (docs/03-replication.md §5.3a).
    pub witness_bytes: u64,
    /// Wire bytes the swarm spent on the reliable lane, gap repairs included.
    pub control_bytes: u64,
    /// Mean reliable-control-lane cost per peer over the run.
    pub control_bits_per_sec: u64,
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
    /// Replicas present before the join fixture delivers anything.
    ///
    /// Must be zero: a long-lived stand-in can retain replicas that a real
    /// arrival could never have received.
    pub initial_replicas: usize,
    /// Peers whose cell was inside that neighbourhood.
    pub in_neighbourhood: usize,
    /// Peers the joiner tracked — must not exceed `in_neighbourhood`.
    pub tracked: usize,
}

#[derive(Debug, Default)]
struct DeliveryGapPair {
    deliveries: u64,
    last_tick: Option<u64>,
    active_since: u64,
    active: bool,
    max_inter_delivery_gap_ticks: u64,
    max_active_silence_ticks: u64,
}

#[derive(Debug, Default)]
struct DeliveryGapTracker {
    pairs: BTreeMap<(NodeId, PersistId, NodeId), DeliveryGapPair>,
    completed_gap_histogram: BTreeMap<u64, u64>,
}

impl DeliveryGapTracker {
    fn set_active(&mut self, sender: NodeId, entity: PersistId, recipients: &[NodeId], tick: u64) {
        for ((pair_sender, pair_entity, pair_recipient), pair) in &mut self.pairs {
            if *pair_sender == sender
                && *pair_entity == entity
                && pair.active
                && !recipients.contains(pair_recipient)
            {
                let since = pair.last_tick.unwrap_or(pair.active_since);
                pair.max_active_silence_ticks = pair
                    .max_active_silence_ticks
                    .max(tick.saturating_sub(since));
                pair.active = false;
            }
        }
        for recipient in recipients {
            let pair = self.pairs.entry((sender, entity, *recipient)).or_default();
            if !pair.active {
                pair.active = true;
                pair.active_since = tick;
                pair.last_tick = None;
            }
        }
    }

    fn observe(&mut self, sender: NodeId, entity: PersistId, recipient: NodeId, tick: u64) {
        let pair = self.pairs.entry((sender, entity, recipient)).or_default();
        if !pair.active {
            // Defensive for traffic classes introduced without updating the
            // audience snapshot seam: count the delivery, but start an active
            // interval here rather than manufacturing a gap from tick zero.
            pair.active = true;
            pair.active_since = tick;
        }
        let since = pair.last_tick.unwrap_or(pair.active_since);
        let gap = tick.saturating_sub(since);
        pair.max_active_silence_ticks = pair.max_active_silence_ticks.max(gap);
        if pair.last_tick.is_some() {
            pair.max_inter_delivery_gap_ticks = pair.max_inter_delivery_gap_ticks.max(gap);
            *self.completed_gap_histogram.entry(gap).or_default() += 1;
        }
        pair.deliveries += 1;
        pair.last_tick = Some(tick);
    }

    fn report(self, end_tick: u64) -> DeliveryGapReport {
        let completed_gaps = self.completed_gap_histogram.values().sum();
        let p50_gap_ticks = histogram_percentile(&self.completed_gap_histogram, 50);
        let p95_gap_ticks = histogram_percentile(&self.completed_gap_histogram, 95);
        let p99_gap_ticks = histogram_percentile(&self.completed_gap_histogram, 99);
        let pairs = self
            .pairs
            .into_iter()
            .map(
                |((sender, entity, recipient), pair)| DeliveryGapPairReport {
                    sender,
                    entity,
                    recipient,
                    deliveries: pair.deliveries,
                    max_inter_delivery_gap_ticks: pair.max_inter_delivery_gap_ticks,
                    max_active_silence_ticks: pair.max_active_silence_ticks,
                    trailing_gap_ticks: if pair.active {
                        end_tick.saturating_sub(pair.last_tick.unwrap_or(pair.active_since))
                    } else {
                        0
                    },
                },
            )
            .collect::<Vec<_>>();
        let max_gap_ticks = pairs
            .iter()
            .map(|pair| {
                pair.max_inter_delivery_gap_ticks
                    .max(pair.max_active_silence_ticks)
                    .max(pair.trailing_gap_ticks)
            })
            .max()
            .unwrap_or(0);
        DeliveryGapReport {
            pairs,
            completed_gaps,
            completed_gap_histogram: self.completed_gap_histogram,
            p50_gap_ticks,
            p95_gap_ticks,
            p99_gap_ticks,
            max_gap_ticks,
        }
    }
}

#[derive(Debug, Default)]
struct InterestMarginStats {
    samples: u64,
    total_cells: u64,
    min_cells: usize,
    max_cells: usize,
}

impl InterestMarginStats {
    fn observe(&mut self, cells: usize) {
        if self.samples == 0 {
            self.min_cells = cells;
        } else {
            self.min_cells = self.min_cells.min(cells);
        }
        self.max_cells = self.max_cells.max(cells);
        self.total_cells += cells as u64;
        self.samples += 1;
    }

    fn report(self) -> InterestMarginReport {
        InterestMarginReport {
            samples: self.samples,
            min_cells: self.min_cells,
            mean_cells: if self.samples == 0 {
                0.0
            } else {
                self.total_cells as f64 / self.samples as f64
            },
            max_cells: self.max_cells,
        }
    }
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
    /// Connected external peers, keyed by their swarm slots (#385, #571).
    exteriors: BTreeMap<usize, ExteriorSlot>,
    /// Completed participants retained after their seat was released.
    departed_exteriors: Vec<ExteriorReport>,
    /// Continuous admission for a reservation-backed standing campaign.
    live_joins: Option<LiveJoins>,
    /// New links get a second manifest after their downlink writer is live.
    deferred_live_manifests: BTreeMap<usize, DeferredManifest>,
    /// Host-side watch edges already armed, `(watcher, subject)`.
    armed_external_watches: BTreeSet<(usize, usize)>,
    /// The two roster snapshots which enforce hearsay's age floor.
    hearsay_buffers: HearsayBuffers,
    /// Test seam for H2's required on/off replication diff.
    hearsay_fold_enabled: bool,
    /// The in-process cluster that re-runs filed reports.
    adjudicator: Adjudicator,
    /// Every verdict it reached.
    docket: Docket,
    /// Opt-in shot/interest accumulator.
    shot_interest_stats: Option<ShotInterestStats>,
    /// Directed scope from the roster most recently installed on the bots.
    shot_interest_scope: BTreeMap<(usize, usize), bool>,
    /// A20's admitted per-(entity, link) cadence measurement.
    delivery_gaps: Option<DeliveryGapTracker>,
    /// Actual cell counts installed by the swept-margin load point.
    interest_margin_stats: Option<InterestMarginStats>,
    /// Ordered crossing fence per stable bot seat.
    applied_interest_crossings: Vec<Option<AppliedInterestCrossing>>,
    /// Replication messages admitted by the meter, split by A19 wire kind.
    admitted_keyframes: u64,
    admitted_deltas: u64,
}

/// One joined external peer, as far as the host ever knows it.
///
/// Deliberately less than a [`Bot`]: no executor, no Bevy app, no pilot. The
/// remote process owns all of that; the host holds only what routing and
/// witnessing require — where to send its traffic, what it committed to at
/// its join tick, and which cell it last said it was in.
pub struct ExteriorSlot {
    /// The stable human-seat slot this peer occupies.
    pub index: usize,
    /// Transport identity, verified against the dial at join time.
    pub node: NodeId,
    /// The entity id derived from the slot, exactly as a bot's is.
    pub entity: PersistId,
    /// The interest cell from the slot's deterministic spawn pose. Updated by
    /// meta frames as the peer moves; starts honest by construction because
    /// both sides derive the pose from the slot alone (`bot::spawn_pose`).
    cell: CellId,
    /// Host tick at which the current cell fact was read from a Meta report.
    cell_fact_tick: u64,
    /// Admission session released when this binding ends.
    session_id: Option<String>,
    /// First host tick at which QUIC reported the connection closed.
    disconnected_at: Option<u64>,
    /// The join-tick claim the peer shipped after joining, with the state it
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
    /// Host ticks during which the bridge reported this slot connected.
    connected_ticks: u64,
    /// Uplink datagrams retained by the impairment router.
    uplink_delivered: u64,
    /// Uplink datagrams discarded by the impairment router.
    uplink_dropped: u64,
    /// Frames queued down, for the report.
    downlink_frames: u64,
    /// Downlink frames refused on a full queue.
    downlink_dropped: u64,
    /// True once the runner's clean end-of-run marker arrived. Shared with
    /// the bridge's reader task, which is what sees the marker first.
    pub goodbye: Arc<std::sync::atomic::AtomicBool>,
}

/// One crewed-roster snapshot retained by the hearsay fold.
#[derive(Debug)]
struct HearsaySnapshot {
    fold_tick: u64,
    contacts: Vec<HearsayContact>,
}

/// Current and preceding roster folds.
///
/// Both names are load-bearing: only `previous` may be encoded for delivery.
#[derive(Debug, Default)]
struct HearsayBuffers {
    previous: Option<HearsaySnapshot>,
    current: Option<HearsaySnapshot>,
}

/// Why a subject is or is not inside a receiver's replicated interest set.
///
/// The capture is diagnostic only. It is a rendering of the roster after the
/// coordinator-side refresh, not an input to any replication decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeReason {
    /// The subject's committed cell is one of the receiver's 27 interest cells.
    InInterest,
    /// The subject's committed cell is outside the receiver's interest cells.
    OutOfInterest,
}

impl ScopeReason {
    /// Stable text for the diagnostic line.
    const fn as_str(self) -> &'static str {
        match self {
            Self::InInterest => "in_interest",
            Self::OutOfInterest => "out_of_interest",
        }
    }
}

/// One directed replica-scope decision written by `replica_scope_capture`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScopeCapture {
    /// Stable slot of the state author.
    subject_seat: usize,
    /// Stable slot which would receive that author's state.
    receiver_seat: usize,
    /// The author's committed cell.
    subject_cell: CellId,
    /// The receiver's committed cell.
    receiver_cell: CellId,
    /// Whether the receiver's interest set contains the author.
    in_scope: bool,
    /// Readable classification of `in_scope` for operators.
    reason: ScopeReason,
}

/// Produce every directed, non-self replica-scope decision for active seats.
///
/// This is intentionally independent of the replication path: it reads the
/// already-selected cells and makes no change to recipients, router state, or
/// manifest contents.
fn scope_capture_records(roster: &[(usize, NodeId, CellId)]) -> Vec<ScopeCapture> {
    roster
        .iter()
        .flat_map(|(subject_seat, _, subject_cell)| {
            roster
                .iter()
                .filter_map(move |(receiver_seat, _, receiver_cell)| {
                    if subject_seat == receiver_seat {
                        return None;
                    }
                    let in_scope = receiver_cell.neighbors27().contains(subject_cell);
                    let reason = if in_scope {
                        ScopeReason::InInterest
                    } else {
                        ScopeReason::OutOfInterest
                    };
                    Some(ScopeCapture {
                        subject_seat: *subject_seat,
                        receiver_seat: *receiver_seat,
                        subject_cell: *subject_cell,
                        receiver_cell: *receiver_cell,
                        in_scope,
                        reason,
                    })
                })
        })
        .collect()
}

/// Classify one datagram after `send_peer_packets` admitted it to a session.
///
/// There are two channel envelopes here: the send path's outer tag and A19's
/// encoded replication payload. Looking through both at this seam is what makes
/// the shed split exact: offered minus stalled minus admitted, by wire kind.
fn admitted_replication(payload: &[u8]) -> Option<(PersistId, bool)> {
    let (channel, replication) = orrery_net::channels::untag(payload)?;
    if channel != orrery_net::channels::Channel::State {
        return None;
    }
    if let Some(delta) = decode_replication_delta(replication) {
        return Some((delta.entity, true));
    }
    decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(replication)
        .map(|(_, _, entity, _)| (entity, false))
}

/// Render the exact opt-in scope diagnostic bytes written by the host.
fn scope_capture_output(tick: u64, roster: &[(usize, NodeId, CellId)]) -> String {
    let mut output = String::new();
    for capture in scope_capture_records(roster) {
        writeln!(
            output,
            "replica_scope_capture host_tick={tick} subject_seat={} receiver_seat={} \
             in_scope={} scope_reason={} subject_cell={} receiver_cell={}",
            capture.subject_seat,
            capture.receiver_seat,
            capture.in_scope,
            capture.reason.as_str(),
            capture.subject_cell.to_bits(),
            capture.receiver_cell.to_bits(),
        )
        .expect("writing to a String cannot fail");
    }
    output
}

/// Apply the campaign's H5 reveal filter between snapshot and encoding.
///
/// Regolith is regime-1 public today, so every crewed contact passes. Keeping
/// this boundary named prevents a future hidden class from leaking merely
/// because it was added to the roster.
fn public_hearsay_record(snapshot: &HearsaySnapshot) -> HearsayContacts {
    HearsayContacts {
        source: HearsaySource::HostRosterFold,
        fold_tick: snapshot.fold_tick,
        contacts: snapshot.contacts.clone(),
    }
}

impl ExteriorSlot {
    fn report(&self) -> ExteriorReport {
        ExteriorReport {
            index: self.index,
            node: self.node,
            connected: self
                .link
                .connected
                .load(std::sync::atomic::Ordering::Relaxed),
            connected_ticks: self.connected_ticks,
            uplink_frames: self.uplink_frames,
            uplink_delivered: self.uplink_delivered,
            uplink_dropped: self.uplink_dropped,
            downlink_frames: self.downlink_frames,
            downlink_dropped: self.downlink_dropped,
            said_goodbye: self.goodbye.load(std::sync::atomic::Ordering::Relaxed),
            witness_anchored: self.witness_anchored,
        }
    }

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
                self.downlink_dropped += 1;
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
        if self
            .link
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.connected_ticks += 1;
        }
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
                        self.set_cell_from_bits(u64::from_le_bytes(raw), tick);
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
                    match disposition {
                        crate::router::DatagramDisposition::Delivered => {
                            self.uplink_delivered += 1;
                        }
                        crate::router::DatagramDisposition::Dropped => {
                            self.uplink_dropped += 1;
                        }
                    }
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

    /// Records a raw meta-lane cell report, refusing encodings that are not
    /// cells rather than storing them.
    fn set_cell_from_bits(&mut self, raw: u64, fact_tick: u64) {
        if let Some(cell) = CellId::from_bits(raw) {
            self.cell = cell;
            self.cell_fact_tick = fact_tick;
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

/// The deterministic measurement ring over frozen active seats.
///
/// Seat numbers need not be contiguous: an unoccupied human seat stays empty
/// at Start and is omitted without renumbering anybody after it.
pub(crate) fn witness_recipients(active_slots: &[usize], subject: usize) -> Vec<usize> {
    let Some(position) = active_slots.iter().position(|slot| *slot == subject) else {
        return Vec::new();
    };
    let width = orrery_witness::plugin::MAX_WITNESS_LINKS.min(active_slots.len().saturating_sub(1));
    (1..=width)
        .map(|offset| active_slots[(position + offset) % active_slots.len()])
        .collect()
}

impl Swarm {
    /// Build a swarm from `config`.
    #[must_use]
    pub fn new(config: SwarmConfig) -> Self {
        let bot_seats = config.peers;
        Self::new_for_island(config, bot_seats)
    }

    /// Build the bot cohort in the full stable seat namespace used by a
    /// campaign lobby. Human gaps still affect deterministic spawn geometry.
    #[must_use]
    pub(crate) fn new_for_island(config: SwarmConfig, island_seats: usize) -> Self {
        assert!(island_seats >= config.peers);
        let mut universe = [0u8; 32];
        universe[0..8].copy_from_slice(&config.seed.to_le_bytes());
        let seed = UniverseSeed(universe);

        let tampered = config
            .cheats
            .map(|cheats| tampered_indices(config.peers, cheats.count))
            .unwrap_or_default();

        let varied_profiles = config.varied_profiles.unwrap_or(config.witnessing);
        let mut bots: Vec<Bot> = (0..config.peers)
            .map(|index| {
                Bot::new(BotSpec {
                    index,
                    count: island_seats,
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
        for bot in &mut bots {
            bot.profile = crate::profile::Profile::for_index(bot.index, varied_profiles);
            bot.set_upload_budget_bits(config.upload_budget_bits);
        }
        if config.campaign && !bots.is_empty() {
            for seeded in campaign_rock_seeds(seed, bots.len()) {
                bots[seeded.owner_slot]
                    .host_entity(seeded.entity, RegolithState::Rock(seeded.rock));
            }
        }
        for bot in &mut bots {
            bot.set_keyframe_every_sends(config.keyframe_every_sends);
        }
        if config.delta_stats {
            for bot in &mut bots {
                bot.enable_delta_stats(config.send_hz);
            }
        }
        if config.shot_interest_stats {
            for bot in &mut bots {
                bot.enable_resolved_shot_capture();
            }
        }
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
            shot_interest_stats: config.shot_interest_stats.then(ShotInterestStats::default),
            shot_interest_scope: BTreeMap::new(),
            delivery_gaps: config
                .delivery_gap_instrumentation
                .then(DeliveryGapTracker::default),
            interest_margin_stats: config
                .swept_interest_margin
                .then(InterestMarginStats::default),
            applied_interest_crossings: vec![None; config.peers],
            admitted_keyframes: 0,
            admitted_deltas: 0,
            exteriors: BTreeMap::new(),
            departed_exteriors: Vec::new(),
            live_joins: None,
            deferred_live_manifests: BTreeMap::new(),
            armed_external_watches: BTreeSet::new(),
            hearsay_buffers: HearsayBuffers::default(),
            hearsay_fold_enabled: true,
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
        self,
        node: NodeId,
        anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
        link: crate::exterior::HostLink,
    ) -> Self {
        let index = self.bots.len();
        self.with_external_at(index, index + 1, node, anchor, link)
    }

    /// Attach a reserved exterior at its stable seat in the full configured
    /// island. Vacant human seats are gaps, not renumbering opportunities.
    #[must_use]
    pub fn with_external_at(
        mut self,
        index: usize,
        island_seats: usize,
        node: NodeId,
        anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
        link: crate::exterior::HostLink,
    ) -> Self {
        self.attach_external_at(index, island_seats, node, None, 0, anchor, link);
        self
    }

    /// Enable the receiver used by a standing host after the initial cohort starts.
    #[must_use]
    pub(crate) fn with_live_joins(
        mut self,
        receiver: mpsc::Receiver<JoinedExternal>,
        membership: Arc<Mutex<LiveMembership>>,
        island_seats: usize,
    ) -> Self {
        self.live_joins = Some(LiveJoins {
            receiver,
            membership,
            island_seats,
        });
        self
    }

    /// Attach an initial standing-campaign participant with its release key.
    #[must_use]
    pub(crate) fn with_external_session_at(
        mut self,
        index: usize,
        island_seats: usize,
        node: NodeId,
        session_id: String,
        anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
        link: crate::exterior::HostLink,
    ) -> Self {
        self.attach_external_at(index, island_seats, node, Some(session_id), 0, anchor, link);
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_external_at(
        &mut self,
        index: usize,
        island_seats: usize,
        node: NodeId,
        session_id: Option<String>,
        tick: u64,
        anchor: Option<(orrery_protocol::StateClaim, RegolithState)>,
        link: crate::exterior::HostLink,
    ) {
        assert!(
            index >= self.bots.len(),
            "an exterior cannot replace a bot seat"
        );
        assert!(
            index < island_seats,
            "exterior seat must be inside the island"
        );
        assert!(
            !self.exteriors.contains_key(&index),
            "exterior seat already occupied"
        );
        let (pos, _) = crate::bot::spawn_pose(index, island_seats);
        let start_grid = crate::bot::grid_of(&pos, self.config.cell_edge_m);
        let cell = crate::bot::cell_of(start_grid);
        let entity = PersistId::new(index as u64 + 1);
        self.index_of.insert(node, index);
        let goodbye_flag = link.goodbye.clone();
        let witness_anchored = anchor.is_some();
        self.exteriors.insert(
            index,
            ExteriorSlot {
                index,
                node,
                entity,
                cell,
                cell_fact_tick: tick,
                session_id,
                disconnected_at: None,
                anchor,
                witness_anchored,
                link,
                uplink_frames: 0,
                connected_ticks: 0,
                uplink_delivered: 0,
                uplink_dropped: 0,
                downlink_frames: 0,
                downlink_dropped: 0,
                goodbye: goodbye_flag,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn exterior_witness_anchored(&self) -> Option<bool> {
        self.exteriors
            .values()
            .next()
            .map(|exterior| exterior.witness_anchored)
    }

    /// A slot index's transport identity, bots and external alike.
    fn node_of(&self, index: usize) -> NodeId {
        self.exteriors.get(&index).map_or_else(
            || self.bots.get(index).expect("active slot has a node").node,
            |exterior| exterior.node,
        )
    }

    /// Total participants, external peer included: the number the report
    /// counts as peers and hours.
    fn total_peers(&self) -> usize {
        self.bots.len() + self.exteriors.len()
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
        let coverage = self.active_interest_coverage();

        for bot in &mut self.bots {
            let others: Vec<PeerEntry> = coverage
                .iter()
                .filter(|entry| entry.0 != bot.node)
                .map(|(node, cells)| PeerEntry {
                    node: *node,
                    cells: cells.clone(),
                })
                .collect();
            for entry in &others {
                bot.link(entry.node, 1_200);
            }
            bot.set_island(others);
        }
        if self.shot_interest_stats.is_some() {
            let roster = self.active_roster();
            self.replace_shot_interest_scope(&roster);
        }
    }

    /// Refresh every bot's roster with where the others actually are.
    ///
    /// Stands in for the coordinator re-broadcasting a manifest as peers move.
    /// Without it a bot's view of its island-mates' cells freezes at tick zero
    /// and the visibility gate stops reflecting the world.
    fn refresh_rosters(&mut self, tick: u64) -> String {
        // Pump the exterior's meta frames first, so today's roster carries the
        // cell it just reported rather than yesterday's.
        for exterior in self.exteriors.values_mut() {
            while let Ok(raw) = {
                let mut r = exterior.link.meta.lock().expect("meta lock");
                r.try_recv()
            } {
                exterior.set_cell_from_bits(raw, tick);
            }
        }
        let roster = self.active_roster();
        let coverage = self.active_interest_coverage();
        self.fold_hearsay_contacts(tick);
        let capture = if self.config.replica_scope_capture {
            scope_capture_output(tick, &roster)
        } else {
            String::new()
        };
        if !capture.is_empty() {
            eprint!("{capture}");
        }
        for bot in &mut self.bots {
            let others: Vec<PeerEntry> = coverage
                .iter()
                .filter(|entry| entry.0 != bot.node)
                .map(|(node, cells)| PeerEntry {
                    node: *node,
                    cells: cells.clone(),
                })
                .collect();
            bot.set_island(others);
        }
        self.replace_shot_interest_scope(&roster);
        capture
    }

    /// Apply immediate crossing coverage to the membership every sender reads.
    ///
    /// This is the host half of #653/#692. The swept set pages likely cells in
    /// before arrival; the ordered event closes the reactive tail between 1 Hz
    /// bulk repairs. Applying the event directly to `IslandMembership` is what
    /// lets `broadcast_state`'s existing audience diff offer a cached keyframe
    /// on the next send tick, with no additional presence-keyframe policy.
    fn apply_interest_crossings(&mut self, events: Vec<HostInterestCrossing>) {
        for event in events {
            let index = *self
                .index_of
                .get(&event.node)
                .expect("a crossing authority has one stable host seat");
            if let Some(applied) = self.applied_interest_crossings[index] {
                assert!(
                    event.crossing.seq.supersedes(applied.seq)
                        && event.crossing.tick > applied.tick
                        && event.crossing.from == applied.committed_cell,
                    "interest crossing for seat {index} did not chain after the host's ordered fence: previous {applied:?}, offered {:?}",
                    event.crossing,
                );
            } else {
                assert_eq!(
                    event.crossing.seq.auth_seq, 1,
                    "a seat's first emitted crossing must start its authority order at one"
                );
            }

            for bot in &mut self.bots {
                if bot.node != event.node {
                    bot.apply_interest_crossing(event.node, &event.crossing);
                }
            }
            self.applied_interest_crossings[index] = Some(AppliedInterestCrossing {
                seq: event.crossing.seq,
                tick: event.crossing.tick,
                committed_cell: event.crossing.to,
            });
        }
    }

    /// Retain the same directed decisions the replica-scope capture renders.
    fn replace_shot_interest_scope(&mut self, roster: &[(usize, NodeId, CellId)]) {
        if self.shot_interest_stats.is_none() {
            return;
        }
        self.shot_interest_scope = scope_capture_records(roster)
            .into_iter()
            .map(|capture| {
                (
                    (capture.subject_seat, capture.receiver_seat),
                    capture.in_scope,
                )
            })
            .collect();
    }

    /// Classify target-authored shot verdicts against the last installed roster.
    fn capture_shot_interest(&mut self) {
        if self.shot_interest_stats.is_none() {
            return;
        }
        let resolved: Vec<_> = self
            .bots
            .iter_mut()
            .enumerate()
            .flat_map(|(victim_index, bot)| {
                bot.take_resolved_shots()
                    .into_iter()
                    .map(move |event| (victim_index, event))
            })
            .collect();

        for (victim_index, event) in resolved {
            let Outcome::ShotResolved {
                attacker,
                target,
                result,
            } = event
            else {
                continue;
            };
            debug_assert_eq!(target, self.bots[victim_index].entity());
            let attacker_index = self.bots.iter().position(|bot| bot.entity() == attacker);
            let in_interest = attacker_index.and_then(|index| {
                self.shot_interest_scope
                    .get(&(index, victim_index))
                    .copied()
            });
            let attacker_profile = attacker_index.map(|index| self.bots[index].profile.name());
            let attacker_speed_mms = attacker_index.map(|index| self.bots[index].speed_mms());
            let victim_speed_mms = self.bots[victim_index].speed_mms();
            self.shot_interest_stats
                .as_mut()
                .expect("measurement enabled")
                .observe(
                    in_interest,
                    result,
                    attacker_profile,
                    attacker_speed_mms,
                    victim_speed_mms,
                );
        }
    }

    /// Rotate the five-second roster buffers and deliver only the preceding
    /// crewed snapshot. This is the H4 anti-wallhack boundary, not a cache.
    fn fold_hearsay_contacts(&mut self, tick: u64) {
        if self.exteriors.is_empty()
            || !self.hearsay_fold_enabled
            || !tick.saturating_add(1).is_multiple_of(HEARSAY_FOLD_TICKS)
        {
            return;
        }

        let contacts = self
            .exteriors
            .values()
            .map(|exterior| HearsayContact {
                seat: u8::try_from(exterior.index).expect("exterior seat fits the wire record"),
                cell: exterior.cell().to_bits(),
                fact_age_ticks: u16::try_from(tick.saturating_sub(exterior.cell_fact_tick))
                    .unwrap_or(u16::MAX),
            })
            .collect();
        let snapshot = HearsaySnapshot {
            fold_tick: tick,
            contacts,
        };
        self.hearsay_buffers.previous = self.hearsay_buffers.current.replace(snapshot);

        let Some(previous) = self.hearsay_buffers.previous.as_ref() else {
            return;
        };
        let payload = public_hearsay_record(previous).encode();
        for exterior in self.exteriors.values_mut() {
            exterior.queue_downlink(Frame {
                peer: u32::MAX,
                lane: Lane::Meta,
                payload: payload.clone(),
            });
        }
    }

    /// Every active seat's stable index, transport identity, and committed
    /// cell. This is read by the diagnostic and the normal bot-manifest
    /// refresh; neither reader may add or remove a recipient.
    fn active_roster(&mut self) -> Vec<(usize, NodeId, CellId)> {
        let mut roster: Vec<_> = self
            .bots
            .iter_mut()
            .map(|bot| (bot.index, bot.node, bot.cell().expect("committed")))
            .collect();
        roster.extend(
            self.exteriors
                .values()
                .map(|exterior| (exterior.index, exterior.node, exterior.cell())),
        );
        roster
    }

    /// Coverage each active recipient asks senders to use for audience gating.
    ///
    /// The ordinary arm is byte-for-byte the old `neighbors27()` roster. The
    /// swept arm is deliberately confined to this harness seam: #692 landed
    /// the primitive, while production host/client propagation remains a
    /// separate follow-up after this measurement says whether it is affordable.
    fn active_interest_coverage(&mut self) -> Vec<(NodeId, Vec<CellId>)> {
        let swept = self.config.swept_interest_margin;
        // Preserve the old manifest order: bots by stable seat, followed by
        // exteriors by stable seat. `set_island` installs links and audiences
        // in this order, so sorting by transport identity changes simulation
        // behaviour even when every measurement flag is off.
        let mut coverage = Vec::with_capacity(self.total_peers());
        for bot in &mut self.bots {
            let cells = if swept {
                bot.swept_interest_cells(INTEREST_REFRESH_PERIOD_S)
            } else {
                bot.cell().expect("committed").neighbors27()
            };
            bot.trace_interest_coverage(cells.len());
            if let Some(stats) = &mut self.interest_margin_stats {
                stats.observe(cells.len());
            }
            coverage.push((bot.node, cells));
        }
        for exterior in self.exteriors.values() {
            let cells = exterior.cell().neighbors27();
            if let Some(stats) = &mut self.interest_margin_stats {
                stats.observe(cells.len());
            }
            coverage.push((exterior.node, cells));
        }
        coverage
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
        if let Some(gaps) = &mut self.delivery_gaps {
            let sender = self.bots[index].node;
            for (entity, recipients) in self.bots[index].replication_audience_snapshot() {
                gaps.set_active(sender, entity, &recipients, tick + 1);
            }
        }
    }

    /// Drain what each bot's send path handed the IO layer into the router.
    fn collect_sends(&mut self, tick: u64) {
        // Disjoint field borrows: the exterior pumps into the router directly,
        // at this same tick, so its traffic is impaired like a bot's.
        let exteriors = &mut self.exteriors;
        let router = &mut self.router;
        for exterior in exteriors.values_mut() {
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
                if let Some(gaps) = &mut self.delivery_gaps {
                    let sender = self.bots[index].node;
                    for (entity, recipients) in self.bots[index].replication_audience_snapshot() {
                        gaps.set_active(sender, entity, &recipients, tick + 1);
                    }
                }
                continue;
            }
            for (to, stream, payload) in self.bots[index].drain_outbound() {
                if stream.is_none() {
                    if let Some((entity, is_delta)) = admitted_replication(&payload) {
                        if is_delta {
                            self.admitted_deltas += 1;
                        } else {
                            self.admitted_keyframes += 1;
                        }
                        if let Some(gaps) = &mut self.delivery_gaps {
                            // Wire ticks name the post-step state as `tick + 1`.
                            // Cached keyframes sent to a new audience carry an
                            // older state tick, so delivery time must come from
                            // this seam rather than from their payload.
                            gaps.observe(node, entity, to, tick + 1);
                        }
                    }
                }
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

    /// Route every `Game::deliver` product through the same reliable router as
    /// other peer traffic. The swarm owns the authority roster, so it is the
    /// only layer that can resolve an entity to a token-authenticated exterior
    /// node without guessing its transport identity from a slot.
    fn collect_delivered_inputs(&mut self, tick: u64) {
        for from in 0..self.bots.len() {
            let sender = self.bots[from].node;
            for (author, recipient, order) in self.bots[from].drain_delivered() {
                let target = self
                    .bots
                    .iter()
                    .position(|bot| bot.authors(recipient))
                    .or_else(|| {
                        self.exteriors
                            .values()
                            .find(|exterior| exterior.entity == recipient)
                            .map(|exterior| exterior.index)
                    });
                let Some(target) = target else {
                    continue;
                };
                let inner = orrery_protocol::channels::encode_delivered_input(
                    author,
                    recipient,
                    &order.to_canonical(),
                );
                let payload = Bytes::from(orrery_protocol::channels::tag(
                    orrery_protocol::channels::Channel::Control,
                    &inner,
                ));
                self.router
                    .accept_stream(tick, sender, target, StreamMode::Shared, payload);
            }
        }
    }

    /// Hand every due packet to its recipient's buffer, on the lane it came in on.
    fn deliver(&mut self, tick: u64) {
        for delivery in self.router.deliver_due(tick) {
            if let Some(exterior) = self.exteriors.get_mut(&delivery.to) {
                let from = self
                    .index_of
                    .get(&delivery.from)
                    .copied()
                    .unwrap_or(usize::MAX);
                exterior.deliver_from(from, delivery.stream, delivery.payload);
                continue;
            }
            // The router already carries the sender's identity verbatim.
            let from_entity = self
                .index_of
                .get(&delivery.from)
                .and_then(|index| self.bots.get(*index))
                .map(Bot::entity)
                .or_else(|| {
                    self.exteriors
                        .values()
                        .find(|exterior| exterior.node == delivery.from)
                        .map(|exterior| exterior.entity)
                })
                .unwrap_or(PersistId::new(0));
            let Some(bot) = self.bots.get_mut(delivery.to) else {
                // A packet already in flight for a seat that has since been
                // unbound is stale traffic, not a bot-vector index.
                continue;
            };
            bot.receive_inbound(
                delivery.from,
                from_entity,
                delivery.stream,
                delivery.payload,
            );
        }
    }

    fn membership_manifest(&self, subject: usize, tick: u64, attempt_id: &str) -> StartManifest {
        let mut active = self
            .bots
            .iter()
            .map(|bot| ActiveSeat {
                slot: bot.index,
                node: bot.node.to_string(),
                entity: bot.entity().0,
            })
            .collect::<Vec<_>>();
        active.extend(self.exteriors.values().map(|seat| ActiveSeat {
            slot: seat.index,
            node: seat.node.to_string(),
            entity: seat.entity.0,
        }));
        active.sort_by_key(|seat| seat.slot);
        let active_slots = active.iter().map(|seat| seat.slot).collect::<Vec<_>>();
        StartManifest {
            attempt_id: attempt_id.to_owned(),
            seed: self.config.seed,
            tick,
            island_seats: u16::try_from(
                self.live_joins
                    .as_ref()
                    .map_or(self.total_peers(), |live| live.island_seats),
            )
            .expect("campaign seat namespace fits u16"),
            active,
            witness_recipients: witness_recipients(&active_slots, subject),
            duration_ticks: self.config.seconds.saturating_mul(TICK_HZ),
        }
    }

    fn publish_live_manifests(&mut self, tick: u64) {
        let slots = self.exteriors.keys().copied().collect();
        self.publish_live_manifests_for(tick, slots);
    }

    fn publish_live_manifests_for(&mut self, tick: u64, slots: Vec<usize>) {
        let Some(live) = &self.live_joins else {
            return;
        };
        let attempt_id = live
            .membership
            .lock()
            .expect("membership lock")
            .attempt_id
            .clone();
        let manifests = slots
            .into_iter()
            .filter(|slot| self.exteriors.contains_key(slot))
            .map(|slot| (slot, self.membership_manifest(slot, tick, &attempt_id)))
            .collect::<Vec<_>>();
        for (slot, manifest) in manifests {
            if let Some(exterior) = self.exteriors.get_mut(&slot) {
                exterior.queue_downlink(Frame {
                    peer: u32::MAX,
                    lane: Lane::Meta,
                    // Live membership is the existing StartV1 JSON on the
                    // existing Meta lane; no second wire schema is invented.
                    payload: Bytes::from(
                        serde_json::to_vec(&manifest).expect("StartV1 serializes"),
                    ),
                });
            }
        }
    }

    fn refresh_live_witnesses(&mut self) {
        if !self.config.witnessing {
            return;
        }
        let active_slots: Vec<usize> = (0..self.bots.len())
            .chain(self.exteriors.keys().copied())
            .collect();
        for index in 0..self.bots.len() {
            let members = witness_recipients(&active_slots, index)
                .into_iter()
                .map(|slot| self.node_of(slot))
                .collect();
            self.bots[index].set_witness_set(members);
        }
        let subjects = self
            .exteriors
            .iter()
            .filter_map(|(slot, exterior)| {
                exterior
                    .anchor
                    .clone()
                    .map(|(claim, state)| (*slot, exterior.entity, exterior.node, claim, state))
            })
            .collect::<Vec<_>>();
        for (subject, entity, node, claim, state) in subjects {
            for watcher in witness_recipients(&active_slots, subject) {
                if watcher < self.bots.len()
                    && self.armed_external_watches.insert((watcher, subject))
                {
                    self.bots[watcher].watch(entity, node, claim.clone(), state.clone());
                }
            }
        }
    }

    fn process_live_membership(&mut self, tick: u64) {
        let due_manifest_slots = {
            let mut due = Vec::new();
            self.deferred_live_manifests.retain(|slot, deferred| {
                if tick < deferred.next_tick {
                    return true;
                }
                due.push(*slot);
                deferred.publishes_left -= 1;
                deferred.next_tick = tick + TICK_HZ;
                deferred.publishes_left > 0
            });
            due
        };
        let joined = self.live_joins.as_ref().map_or_else(Vec::new, |live| {
            let mut joined = Vec::new();
            while let Ok(seat) = live.receiver.try_recv() {
                joined.push(seat);
            }
            joined
        });
        let island_seats = self.live_joins.as_ref().map_or(0, |live| live.island_seats);
        let mut changed = false;
        for seat in joined {
            eprintln!(
                "gates/p1-swarm: live seat {} bound as {} at tick {}",
                seat.slot, seat.node, tick
            );
            self.attach_external_at(
                seat.slot,
                island_seats,
                seat.node,
                Some(seat.session_id),
                tick,
                seat.anchor,
                seat.link,
            );
            // The all-seat refresh below is for the peers already established;
            // the joiner cannot use it, because at this instant it is still
            // completing its own handshake and is not yet reading Meta.
            //
            // One retry is not enough: measured against the live campaign, the
            // last seat to bind received zero membership frames and kept a
            // bots-only roster for the whole session, because nothing joined
            // after it to trigger another all-seat publish. Repeat for a few
            // seconds so the joiner's own membership does not depend on a
            // stranger arriving later.
            self.deferred_live_manifests.insert(
                seat.slot,
                DeferredManifest {
                    publishes_left: DEFERRED_MANIFEST_PUBLISHES,
                    next_tick: tick + 1,
                },
            );
            changed = true;
        }

        let mut released = Vec::new();
        for (slot, exterior) in &mut self.exteriors {
            if exterior.goodbye.load(std::sync::atomic::Ordering::Relaxed) {
                released.push(*slot);
                continue;
            }
            let transport_close = exterior
                .link
                .transport_close
                .lock()
                .expect("transport-close lock")
                .clone();
            if transport_close.is_none() {
                exterior.disconnected_at = None;
            } else {
                let first = *exterior.disconnected_at.get_or_insert(tick);
                if transport_close_grace_elapsed(first, tick) {
                    released.push(*slot);
                }
            }
        }
        for slot in released {
            let exterior = self
                .exteriors
                .remove(&slot)
                .expect("release names a live seat");
            self.index_of.remove(&exterior.node);
            // A later session may reuse this entity id, but it is a new
            // authority and chain. Permit every bot to replace its old watch
            // with the new session's join-tick anchor.
            self.armed_external_watches
                .retain(|(_watcher, subject)| *subject != slot);
            let release_cause = if exterior.goodbye.load(std::sync::atomic::Ordering::Relaxed) {
                "explicit goodbye".to_owned()
            } else {
                let reason = exterior
                    .link
                    .transport_close
                    .lock()
                    .expect("transport-close lock")
                    .clone()
                    .expect("transport close triggered this release");
                format!("{reason}; transport close grace elapsed")
            };
            eprintln!("gates/p1-swarm: live seat {slot} released at tick {tick} ({release_cause})");
            if let (Some(live), Some(session)) = (&self.live_joins, exterior.session_id.as_ref()) {
                let mut membership = live.membership.lock().expect("membership lock");
                let binding = membership
                    .active
                    .remove(&slot)
                    .expect("release names an active membership binding");
                assert_eq!(
                    binding.session_id.as_str(),
                    session.as_str(),
                    "release session must match the active membership binding"
                );
                membership.released_sessions.insert(session.clone());
                membership
                    .publish()
                    .expect("republish active seats after unbind");
            }
            self.departed_exteriors.push(exterior.report());
            changed = true;
        }

        if let Some(live) = &self.live_joins {
            live.membership.lock().expect("membership lock").tick = tick;
        }
        if changed {
            let _ = self.refresh_rosters(tick);
            self.refresh_live_witnesses();
            self.publish_live_manifests(tick);
        }
        if !due_manifest_slots.is_empty() {
            self.publish_live_manifests_for(tick, due_manifest_slots);
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
        let real_time = !self.exteriors.is_empty();
        let tick_duration = std::time::Duration::from_nanos(1_000_000_000 / TICK_HZ);

        let mut phase = [0u128; 6];
        for tick in 0..ticks {
            let tick_start = std::time::Instant::now();
            self.process_live_membership(tick);
            self.tick_once(tick, send_every, &mut phase);

            // Once a simulated second, sample each peer's rate and re-publish
            // the roster — the coordinator's manifest cadence.
            if tick % TICK_HZ == TICK_HZ - 1 {
                for index in 0..self.bots.len() {
                    let rate = self.bots[index].upload_rate_bits();
                    self.samples[index].push(rate);
                }
                let _ = self.refresh_rosters(tick);
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
                eprintln!(
                    "gates/p1-swarm: phase {name:>11}: {:>8.2}s",
                    nanos as f64 / 1e9
                );
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
        self.capture_shot_interest();
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
        let mut interest_crossings = Vec::new();
        for bot in &mut self.bots {
            if let Some(crossing) = bot.sample_with_interest_crossing(
                self.config.swept_interest_margin,
                INTEREST_REFRESH_PERIOD_S,
            ) {
                interest_crossings.push(HostInterestCrossing {
                    node: bot.node,
                    crossing,
                });
            }
            bot.drain_signals();
        }
        self.apply_interest_crossings(interest_crossings);
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
        self.collect_delivered_inputs(tick);
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
        // The external peer is witnessed exactly like a bot: it holds a slot
        // in the ring and shipped its tick-zero anchor at join. What nobody
        // host-side can do is watch *through* it — its own observations live in
        // its process, and coverage counts what this run's watchers saw.
        let active_slots: Vec<usize> = (0..self.bots.len())
            .chain(self.exteriors.keys().copied())
            .collect();
        // Ring assignment: peer i is witnessed by the next `MAX_WITNESS_LINKS`
        // peers around the ring. Deterministic, uniform, and it gives every peer
        // both a witness set and a watch list without a central chooser.
        let sets: BTreeMap<usize, Vec<usize>> = active_slots
            .iter()
            .copied()
            .map(|slot| (slot, witness_recipients(&active_slots, slot)))
            .collect();

        // Anchors first: a watcher needs the subject's signed claim and the
        // state it commits to, and both have to be taken before anyone steps.
        let mut anchors: BTreeMap<
            usize,
            (
                PersistId,
                NodeId,
                orrery_protocol::StateClaim,
                RegolithState,
            ),
        > = (0..self.bots.len())
            .map(|index| {
                let state = self.bots[index].state();
                let entity = self.bots[index].entity();
                let node = self.bots[index].node;
                let anchor = self.bots[index]
                    .chain
                    .as_mut()
                    .expect("witnessing enabled")
                    .anchor(0, &state);
                (index, (entity, node, anchor, state))
            })
            .collect();
        for (slot, exterior) in &mut self.exteriors {
            let index = exterior.index;
            let entity = exterior.entity;
            let node = exterior.node;
            match exterior.anchor.clone() {
                Some((claim, state)) => {
                    anchors.insert(index, (entity, node, claim, state));
                }
                None => {
                    // A joiner with no witness log says so with an empty
                    // anchor at join. The slot seats
                    // unanchored: no watcher is armed against it, nothing of
                    // it is shown or judged, and the report carries
                    // `witness_anchored: false` so a human hour cannot be
                    // mistaken for an independently witnessed one. The
                    // rendered and headless producers ship a real anchor and
                    // take the armed path.
                    eprintln!(
                        "gates/p1-swarm: exterior slot {index} joined without a witness anchor;                          its own input stream is not independently witnessed this run"
                    );
                }
            }
            debug_assert_eq!(*slot, index, "the exterior map key is its ring slot");
        }

        for (index, witnesses) in &sets {
            let members: Vec<NodeId> = witnesses
                .iter()
                .map(|watcher| self.node_of(*watcher))
                .collect();
            if self.exteriors.contains_key(index) {
                // The external peer's witness set travels with it: nothing to
                // configure host-side. Its authored frames reach these same
                // watchers through the bridge.
                continue;
            }
            self.bots[*index].set_witness_set(members);
            // Each of those peers watches this one.
            for watcher in witnesses {
                if self.exteriors.contains_key(watcher) {
                    // A bot cannot be armed by a remote subject's anchor here —
                    // but the external peer is not watching anyone either in
                    // slice 1; both directions of that asymmetry close when the
                    // rendered client lands (#386).
                    continue;
                }
                let Some((entity, node, anchor, state)) = anchors.get(index).cloned() else {
                    continue;
                };
                self.bots[*watcher].watch(entity, node, anchor, state);
                self.armed_external_watches.insert((*watcher, *index));
            }
        }
    }

    /// Deliver every packet due by `tick`, discarding the payloads.
    fn deliver_due_all(&mut self, tick: u64) -> usize {
        self.router.deliver_due(tick).len()
    }

    /// A peer arriving mid-run must see only its 27-cell neighbourhood.
    fn check_late_join(&mut self) -> LateJoinReport {
        assert!(
            self.bots.len() >= 2,
            "a late-join scope check needs a joiner and a roster peer"
        );

        // Replace one population seat with a genuinely new peer, keeping the
        // criterion population unchanged. The joiner gets a new identity and
        // an empty receive world; the long-lived bot previously used here
        // could retain replicas from any earlier point in the run.
        let departing = self.bots.len() - 1;
        let retained_cells: Vec<CellId> = self
            .bots
            .iter_mut()
            .take(departing)
            .map(|bot| bot.cell().expect("committed"))
            .collect();
        let placement_peer = (0..departing)
            .max_by_key(|index| {
                let neighbourhood = retained_cells[*index].neighbors27();
                retained_cells
                    .iter()
                    .enumerate()
                    .filter(|(other, cell)| *other != *index && neighbourhood.contains(cell))
                    .count()
            })
            .expect("the retained roster is non-empty");
        let joiner_snapshot = self.bots[placement_peer].craft().clone();
        let mut universe = [0u8; 32];
        universe[0..8].copy_from_slice(&self.config.seed.to_le_bytes());
        let fixture_count = self.bots.len() + 1;
        let cell_edge_m = self.config.cell_edge_m;
        let fixture_spec = |index| BotSpec {
            index,
            count: fixture_count,
            seed: UniverseSeed(universe),
            cell_edge_m,
            witnessing: false,
            cheat: None,
            enforcing: false,
        };
        let mut joiner = Bot::from_craft_snapshot(
            fixture_spec(self.bots.len()),
            joiner_snapshot,
            retained_cells[placement_peer],
        );
        let joiner_cell = joiner.cell().expect("the fresh joiner is committed");
        let neighbourhood = joiner_cell.neighbors27();

        let elsewhere: Vec<(usize, NodeId, PersistId, CellId)> = self
            .bots
            .iter_mut()
            .enumerate()
            .filter(|(index, _)| *index != departing)
            .map(|(index, bot)| {
                (
                    index,
                    bot.node,
                    bot.entity(),
                    bot.cell().expect("committed"),
                )
            })
            .collect();
        let in_neighbourhood = elsewhere
            .iter()
            .filter(|(_, _, _, cell)| neighbourhood.contains(cell))
            .count();
        let roster: Vec<PeerEntry> = elsewhere
            .iter()
            .map(|(_, node, _, cell)| PeerEntry {
                node: *node,
                cells: cell.neighbors27(),
            })
            .collect();
        let roster_len = roster.len();
        for (_, node, _, _) in &elsewhere {
            joiner.link(*node, 1_200);
        }
        joiner.set_island(roster);
        let initial_replicas = joiner.replicas();

        // Exercise the same sender, peer-link and receiver systems as the hour,
        // but in an isolated exchange so this diagnostic does not advance the
        // live swarm an extra frame or change its budget counters. A new
        // subscription receives an absolute keyframe before deltas.
        for (index, node, entity, cell) in &elsewhere {
            let mut sender = Bot::from_craft_snapshot(
                fixture_spec(*index),
                self.bots[*index].craft().clone(),
                *cell,
            );
            sender.link(joiner.node, 1_200);
            sender.set_island(vec![PeerEntry {
                node: joiner.node,
                cells: neighbourhood.clone(),
            }]);
            sender.broadcast_state(0);
            sender.update();
            for (to, stream, payload) in sender.drain_outbound() {
                if to == joiner.node {
                    joiner.receive_inbound(*node, *entity, stream, payload);
                }
            }
        }
        joiner.update();

        LateJoinReport {
            neighbourhood: neighbourhood.len(),
            roster: roster_len,
            initial_replicas,
            in_neighbourhood,
            tracked: joiner.tracked(),
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
                let replica = bot.replica_counters();
                let replication_wire = bot.replication_wire_counters();
                let witness = bot.witness_counters();
                let links = bot.link_counters();
                let convicted_at_tick = docket.first_conviction(bot.node);
                PeerReport {
                    index,
                    cells_visited: bot.visited.len(),
                    crossings: bot.crossings,
                    boundary_flips: bot.boundary_flips,
                    max_boundary_returns_in_window: bot.max_boundary_returns_in_window,
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
                    deltas_unanchored: replica.deltas_unanchored,
                    deltas_without_any_keyframe: replica.deltas_without_any_keyframe,
                    deltas_missing_newer_keyframe: replica.deltas_missing_newer_keyframe,
                    deltas_with_superseded_keyframe: replica.deltas_with_superseded_keyframe,
                    deltas_with_invalid_reference: replica.deltas_with_invalid_reference,
                    keyframes_discarded_while_stalled: replication_wire
                        .keyframes_discarded_while_stalled,
                    deltas_discarded_while_stalled: replication_wire.deltas_discarded_while_stalled,
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
        let delta_stats = self.config.delta_stats.then(|| {
            let mut total = DeltaStats::new(self.config.send_hz);
            for stats in self.bots.iter().filter_map(Bot::delta_stats) {
                total.merge(stats);
            }
            total.report()
        });
        let shot_interest_stats = self
            .shot_interest_stats
            .take()
            .map(ShotInterestStats::report);
        let replication_wire = self.bots.iter().map(Bot::replication_wire_counters).fold(
            crate::bot::ReplicationWireCounters::default(),
            |mut total, peer| {
                total.keyframe_messages += peer.keyframe_messages;
                total.delta_messages += peer.delta_messages;
                total.keyframe_bytes += peer.keyframe_bytes;
                total.delta_bytes += peer.delta_bytes;
                total.keyframes_discarded_while_stalled += peer.keyframes_discarded_while_stalled;
                total.deltas_discarded_while_stalled += peer.deltas_discarded_while_stalled;
                total
            },
        );
        let replication_messages =
            replication_wire.keyframe_messages + replication_wire.delta_messages;
        let measured_replication_bytes =
            replication_wire.keyframe_bytes + replication_wire.delta_bytes;
        let total_shed: u64 = per_peer.iter().map(|peer| peer.shed).sum();
        let shed_keyframes = replication_wire
            .keyframe_messages
            .saturating_sub(replication_wire.keyframes_discarded_while_stalled)
            .saturating_sub(self.admitted_keyframes);
        let shed_deltas = replication_wire
            .delta_messages
            .saturating_sub(replication_wire.deltas_discarded_while_stalled)
            .saturating_sub(self.admitted_deltas);
        let shed_replication_other = total_shed.saturating_sub(shed_keyframes + shed_deltas);
        let mut meter_budgets = self
            .bots
            .iter()
            .map(Bot::upload_budget_bits)
            .collect::<Vec<_>>();
        meter_budgets.sort_unstable();
        meter_budgets.dedup();
        let meter_budget_bits = match meter_budgets.as_slice() {
            [] => self.config.upload_budget_bits,
            [budget] => *budget,
            _ => 0,
        };
        let delivery_gaps = self
            .delivery_gaps
            .take()
            .map(|tracker| tracker.report(ticks));
        let interest_margin = self
            .interest_margin_stats
            .take()
            .map(InterestMarginStats::report);
        let peer_seconds = self.total_peers() as u64 * self.config.seconds.max(1);

        let max_boundary_returns_in_window = per_peer
            .iter()
            .map(|peer| peer.max_boundary_returns_in_window)
            .max()
            .unwrap_or(0);
        let overall_boundary_return_histogram = boundary_return_histogram(
            per_peer
                .iter()
                .map(|peer| peer.max_boundary_returns_in_window),
        );
        let boundary_return_profiles = crate::profile::Profile::ALL
            .into_iter()
            .map(|profile| {
                let maxima = per_peer
                    .iter()
                    .filter(|peer| peer.profile == profile.name())
                    .map(|peer| peer.max_boundary_returns_in_window)
                    .collect::<Vec<_>>();
                BoundaryReturnProfileReport {
                    profile: profile.name(),
                    max_returns_in_window: maxima.iter().copied().max().unwrap_or(0),
                    histogram: boundary_return_histogram(maxima),
                }
            })
            .collect();

        SwarmReport {
            identity: RunIdentity {
                seed: self.config.seed,
                impairment: self.config.impairment,
                keyframe_every_sends: self.config.keyframe_every_sends,
                upload_budget_bits: self.config.upload_budget_bits,
                swept_interest_margin: self.config.swept_interest_margin,
                varied_profiles: self
                    .config
                    .varied_profiles
                    .unwrap_or(self.config.witnessing),
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
            delta_stats,
            shot_interest_stats,
            delivery_gaps,
            interest_margin,
            meter_budget_bits,
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
            total_shed,
            shed_keyframes,
            shed_deltas,
            shed_replication_other,
            total_boundary_flips: per_peer.iter().map(|p| p.boundary_flips).sum(),
            total_proxy_pops: per_peer.iter().map(|p| p.proxy_pops).sum(),
            total_interest_churn: per_peer.iter().map(|p| p.interest_churn).sum(),
            stranded_in_flight: self.router.in_flight(),
            total_undecodable: per_peer.iter().map(|p| p.undecodable).sum(),
            deltas_unanchored: per_peer.iter().map(|p| p.deltas_unanchored).sum(),
            deltas_without_any_keyframe: per_peer
                .iter()
                .map(|p| p.deltas_without_any_keyframe)
                .sum(),
            deltas_missing_newer_keyframe: per_peer
                .iter()
                .map(|p| p.deltas_missing_newer_keyframe)
                .sum(),
            deltas_with_superseded_keyframe: per_peer
                .iter()
                .map(|p| p.deltas_with_superseded_keyframe)
                .sum(),
            deltas_with_invalid_reference: per_peer
                .iter()
                .map(|p| p.deltas_with_invalid_reference)
                .sum(),
            keyframes_discarded_while_stalled: replication_wire.keyframes_discarded_while_stalled,
            deltas_discarded_while_stalled: replication_wire.deltas_discarded_while_stalled,
            total_replicas: per_peer.iter().map(|p| p.replicas).sum(),
            witnessing: self.config.witnessing,
            external: {
                let mut external = self
                    .departed_exteriors
                    .iter()
                    .cloned()
                    .chain(self.exteriors.values().map(ExteriorSlot::report))
                    .collect::<Vec<_>>();
                external.sort_by_key(|seat| seat.index);
                external
            },
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
            replication_bits_per_sec: lanes.replication_bytes * 8 / peer_seconds.max(1),
            keyframe_messages: replication_wire.keyframe_messages,
            delta_messages: replication_wire.delta_messages,
            keyframe_bytes: replication_wire.keyframe_bytes,
            delta_bytes: replication_wire.delta_bytes,
            keyframe_message_share: if replication_messages == 0 {
                0.0
            } else {
                replication_wire.keyframe_messages as f64 / replication_messages as f64
            },
            keyframe_byte_share: if measured_replication_bytes == 0 {
                0.0
            } else {
                replication_wire.keyframe_bytes as f64 / measured_replication_bytes as f64
            },
            witness_bytes: lanes.witness_bytes,
            control_bytes: lanes.control_bytes,
            control_bits_per_sec: lanes.control_bytes * 8 / peer_seconds.max(1),
            witness_lane_share: lanes.witness_share(),
            witness_lane_bits_per_sec: { lanes.witness_bytes * 8 / peer_seconds.max(1) },
            link: self.router.counters.into(),
            per_peer,
            max_boundary_returns_in_window,
            boundary_return_histogram: overall_boundary_return_histogram,
            boundary_return_profiles,
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

fn histogram_percentile(histogram: &BTreeMap<u64, u64>, p: u64) -> u64 {
    let count: u64 = histogram.values().sum();
    if count == 0 {
        return 0;
    }
    let rank = count.saturating_mul(p).div_ceil(100).max(1);
    let mut seen = 0u64;
    for (value, occurrences) in histogram {
        seen += occurrences;
        if seen >= rank {
            return *value;
        }
    }
    histogram.last_key_value().map_or(0, |(value, _)| *value)
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

/// Returns to one cell within a refresh period that constitute boundary thrash.
///
/// Five witnessed, impaired, swept-margin one-hour seeds measured 160 seats: the
/// maximum was one return (157 seats at zero, three at one), and all 40 Burst
/// seats were at zero. Three therefore preserves two legitimate direction
/// changes while making the first sustained A↔B oscillation fail.
const BOUNDARY_THRASH_RETURN_THRESHOLD: u64 = 3;

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

fn boundary_return_histogram(
    maxima: impl IntoIterator<Item = u64>,
) -> Vec<BoundaryReturnHistogramBin> {
    let mut maxima = maxima.into_iter().collect::<Vec<_>>();
    maxima.sort_unstable();
    let mut histogram: Vec<BoundaryReturnHistogramBin> = Vec::new();
    for returns_in_window in maxima {
        if let Some(bin) = histogram
            .last_mut()
            .filter(|bin| bin.returns_in_window == returns_in_window)
        {
            bin.seats += 1;
        } else {
            histogram.push(BoundaryReturnHistogramBin {
                returns_in_window,
                seats: 1,
            });
        }
    }
    histogram
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
        for external in &self.external {
            let clean_close = external.connected || external.said_goodbye;
            if !clean_close {
                failures.push(CriterionFailure {
                    clause: "the external peer stays connected",
                    detail: format!(
                        "slot {} bridge reported a disconnect; {} uplink / {} downlink frames \
                         before it dropped, {} downlink refused on a full queue",
                        external.index,
                        external.uplink_frames,
                        external.downlink_frames,
                        external.downlink_dropped,
                    ),
                });
            }
            if external.uplink_frames == 0 || external.downlink_frames == 0 {
                failures.push(CriterionFailure {
                    clause: "the external peer participates",
                    detail: format!(
                        "slot {} moved {} uplink / {} downlink frames; an island member that sends \
                         or receives nothing measures nothing",
                        external.index, external.uplink_frames, external.downlink_frames,
                    ),
                });
            }
            if external.downlink_dropped > 0 {
                failures.push(CriterionFailure {
                    clause: "the host keeps up with its own clock",
                    detail: format!(
                        "slot {} had {} downlink frames refused on a full queue; the pump fell \
                         behind the real-time tick",
                        external.index, external.downlink_dropped,
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
        if self.max_boundary_returns_in_window >= BOUNDARY_THRASH_RETURN_THRESHOLD {
            failures.push(CriterionFailure {
                clause: "no entity thrashes cells at a boundary",
                detail: format!(
                    "an entity returned to the same cell {} times within one 1 s interest-refresh \
                     period (thrash threshold {BOUNDARY_THRASH_RETURN_THRESHOLD}); zero such \
                     oscillations are allowed",
                    self.max_boundary_returns_in_window,
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
            if join.initial_replicas != 0 {
                failures.push(CriterionFailure {
                    clause: "the late joiner starts with no retained replicas",
                    detail: format!(
                        "{} replicas existed before any late-join delivery",
                        join.initial_replicas
                    ),
                });
            }
            if join.in_neighbourhood == 0 {
                failures.push(CriterionFailure {
                    clause: "the late-join check is not vacuous",
                    detail: "no peer was in the joiner's neighbourhood, so \
                             'receives only its neighborhood' held by receiving \
                             nothing"
                        .to_owned(),
                });
            }
            if join.tracked != join.in_neighbourhood {
                failures.push(CriterionFailure {
                    clause: "a late joiner receives only its 27-cell neighborhood",
                    detail: format!(
                        "tracked {} peers with {} in the neighbourhood",
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
    use orrery_games::regolith::{archetype::Archetype, CAMPAIGN_CELL_EDGE_M};

    fn next_membership(remote: &crate::exterior::RemoteLink) -> StartManifest {
        let frame = remote
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
            .expect("membership manifest queued");
        assert_eq!(frame.lane, Lane::Meta);
        serde_json::from_slice(&frame.payload).expect("Meta frame is a membership manifest")
    }

    #[test]
    fn a_peer_that_joins_a_running_attempt_is_given_every_seat_already_bound() {
        let (existing_host, _existing_remote) = crate::exterior::link_pair();
        let (joined_host, joined_remote) = crate::exterior::link_pair();
        let (joined_tx, joined_rx) = mpsc::channel();
        let membership = Arc::new(Mutex::new(LiveMembership {
            attempt_id: "attempt-live".to_owned(),
            active: BTreeMap::new(),
            pending: BTreeSet::new(),
            released_sessions: BTreeSet::new(),
            tick: 0,
            running: true,
            path: None,
        }));
        let mut swarm = Swarm::new_for_island(
            SwarmConfig {
                peers: 2,
                ..SwarmConfig::default()
            },
            4,
        )
        .with_external_session_at(
            2,
            4,
            crate::bot::bot_key(2).public(),
            "session-two".to_owned(),
            None,
            existing_host,
        )
        .with_live_joins(joined_rx, membership, 4);
        joined_tx
            .send(JoinedExternal {
                slot: 3,
                node: crate::bot::bot_key(3).public(),
                session_id: "session-three".to_owned(),
                anchor: None,
                link: joined_host,
            })
            .expect("join reaches the standing swarm");

        swarm.process_live_membership(120);
        let _early_manifest = next_membership(&joined_remote);
        swarm.process_live_membership(121);
        let corrected_manifest = next_membership(&joined_remote);

        assert_eq!(
            corrected_manifest
                .active
                .iter()
                .map(|seat| seat.slot)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "the joiner's post-link membership must name every already bound seat"
        );
    }

    #[test]
    fn ordinary_interest_coverage_preserves_stable_seat_order() {
        let mut swarm = Swarm::new(SwarmConfig {
            peers: 8,
            swept_interest_margin: false,
            ..SwarmConfig::default()
        });
        let expected = swarm.bots.iter().map(|bot| bot.node).collect::<Vec<_>>();

        let actual = swarm
            .active_interest_coverage()
            .into_iter()
            .map(|(node, _)| node)
            .collect::<Vec<_>>();

        assert_eq!(actual, expected, "flags-off manifests retain main's order");
        let mut sorted = expected.clone();
        sorted.sort_unstable();
        assert_ne!(
            expected, sorted,
            "fixture must detect accidental transport-key sorting"
        );
    }

    #[test]
    fn a_crossing_driven_roster_add_gets_its_keyframe_on_the_next_send_tick() {
        let mut swarm = Swarm::new(SwarmConfig {
            peers: 2,
            swept_interest_margin: true,
            witnessing: false,
            ..SwarmConfig::default()
        });
        let receiver = swarm.bots[1].node;
        let from = swarm.bots[1].cell().expect("receiver starts committed");
        let (from_coords, level) = from.coords();
        let to = CellId::from_coords(from_coords + glam::IVec3::X, level)
            .expect("adjacent crossing cell");
        let sender_cell = CellId::from_coords(from_coords + 2 * glam::IVec3::X, level)
            .expect("newly covered outer face");
        assert!(!from.neighbors27().contains(&sender_cell));
        assert!(to.neighbors27().contains(&sender_cell));
        swarm.bots[0].place_local_player_for_test(sender_cell);
        swarm.form_island();

        // Cache a sender anchor mid-keyframe interval while the receiver is
        // still outside the audience. Entity 1's stagger makes send index 1 a
        // keyframe slot; index 2 below is therefore an ordinary delta slot.
        swarm.bots[0].broadcast_state(2);
        swarm.bots[0].update();
        assert!(swarm.bots[0].drain_outbound().is_empty());
        swarm.bots[0].broadcast_state(5);
        swarm.bots[0].update();
        assert!(swarm.bots[0].drain_outbound().is_empty());

        // Drive the receiver through the real Bevy hysteresis system, then
        // deliver the ordered crossing without invoking the 1 Hz bulk repair.
        swarm.bots[1].move_local_player_for_test(glam::Vec3::new(
            from_coords.x as f32 + 1.2,
            from_coords.y as f32 + 0.5,
            from_coords.z as f32 + 0.5,
        ));
        swarm.bots[1].update();
        let crossing = swarm.bots[1]
            .sample_with_interest_crossing(true, INTEREST_REFRESH_PERIOD_S)
            .expect("hysteresis crossing emits the ordered host event");
        assert_eq!((crossing.from, crossing.to), (from, to));
        swarm.apply_interest_crossings(vec![HostInterestCrossing {
            node: receiver,
            crossing,
        }]);

        // The next send opportunity reads the corrected IslandMembership.
        // #671's existing `added` rule must supply the cached anchor and exclude
        // this link from the delta audience; A21 explicitly wants no other
        // presence keyframe mechanism.
        swarm.bots[0].broadcast_state(8);
        swarm.bots[0].update();
        let sent = swarm.bots[0]
            .drain_outbound()
            .into_iter()
            .filter_map(|(peer, stream, payload)| {
                (peer == receiver && stream.is_none()).then_some(payload)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sent.len(),
            1,
            "crossing-to-roster propagation must offer exactly one cached anchor on the next send tick"
        );
        let (channel, payload) =
            orrery_net::channels::untag(&sent[0]).expect("state datagram carries its channel tag");
        assert_eq!(channel, orrery_net::channels::Channel::State);
        assert!(
            decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(payload).is_some(),
            "the crossing-driven roster add's first eligible packet must be a keyframe"
        );
        assert!(
            decode_replication_delta(payload).is_none(),
            "the newly-added link must not receive a delta before its cached keyframe"
        );
    }

    #[test]
    fn three_returns_to_same_cell_inside_one_refresh_window_fail_boundary_thrash_clause() {
        let mut swarm = Swarm::new(SwarmConfig {
            peers: 1,
            ..SwarmConfig::default()
        });
        let cell_a = swarm.bots[0].cell().expect("the bot starts committed");
        let (coords, level) = cell_a.coords();
        let cell_b = CellId::from_coords(coords + glam::IVec3::X, level).unwrap();

        for (tick, cell) in [cell_b, cell_a, cell_b, cell_a].into_iter().enumerate() {
            swarm.bots[0].record_cell_commitment_for_test(cell, tick as u64);
        }

        let measured = swarm.bots[0].max_boundary_returns_in_window;
        assert_eq!(
            measured, 3,
            "the real one-second window accounting must measure three same-cell returns from A↔B oscillation"
        );
        let mut report = passing();
        report.max_boundary_returns_in_window = measured;
        let failure = report
            .against_criterion(STRICT)
            .into_iter()
            .find(|failure| failure.clause == "no entity thrashes cells at a boundary")
            .expect("three measured returns must fail the named boundary-thrash clause");
        assert!(
            failure.detail.contains("returned to the same cell 3 times"),
            "the failure must name the real violated quantity: {}",
            failure.detail
        );
    }

    #[test]
    fn two_returns_inside_one_refresh_window_are_not_boundary_thrash() {
        let mut report = passing();
        report.total_boundary_flips = 2;
        report.max_boundary_returns_in_window = 2;

        assert!(
            !clauses(&report, STRICT).contains(&"no entity thrashes cells at a boundary"),
            "a reversal and re-reversal cross the full hysteresis deadband and remain legitimate"
        );
    }

    #[test]
    fn transport_close_releases_only_after_the_two_second_grace() {
        assert!(
            !transport_close_grace_elapsed(700, 700 + TRANSPORT_CLOSE_GRACE_TICKS - 1),
            "a transient QUIC close report must keep the seat through the grace"
        );
        assert!(
            transport_close_grace_elapsed(700, 700 + TRANSPORT_CLOSE_GRACE_TICKS),
            "the real two-second transport-close boundary must release the seat"
        );
    }

    fn hearsay_test_swarm(
        bot_seats: usize,
        exterior_seats: &[usize],
        island_seats: usize,
    ) -> (Swarm, Vec<(usize, crate::exterior::RemoteLink)>) {
        use crate::bot::bot_key;
        use crate::exterior::link_pair;

        let mut swarm = Swarm::new_for_island(
            SwarmConfig {
                peers: bot_seats,
                cell_edge_m: crate::bot::campaign_cell_edge_m(),
                campaign: true,
                replica_scope_capture: true,
                ..SwarmConfig::default()
            },
            island_seats,
        );
        let mut remotes = Vec::new();
        for &seat in exterior_seats {
            let (host_link, remote_link) = link_pair();
            swarm =
                swarm.with_external_at(seat, island_seats, bot_key(seat).public(), None, host_link);
            remotes.push((seat, remote_link));
        }
        swarm.form_island();
        (swarm, remotes)
    }

    fn report_current_exterior_cells(
        swarm: &mut Swarm,
        remotes: &[(usize, crate::exterior::RemoteLink)],
        tick: u64,
    ) {
        for (seat, remote) in remotes {
            let cell = swarm.exteriors.get(seat).expect("exterior seat").cell();
            remote
                .uplink
                .try_send(Frame {
                    peer: u32::MAX,
                    lane: Lane::Meta,
                    payload: Bytes::copy_from_slice(&cell.to_bits().to_le_bytes()),
                })
                .expect("the host queue accepts a cell report");
            swarm
                .exteriors
                .get_mut(seat)
                .expect("exterior seat")
                .pump_uplink(tick, &mut swarm.router);
            assert_eq!(
                swarm
                    .exteriors
                    .get(seat)
                    .expect("exterior seat")
                    .cell_fact_tick,
                tick,
                "the fact tick is the host tick which read the Meta report"
            );
        }
    }

    fn receive_hearsay(remote: &crate::exterior::RemoteLink) -> HearsayContacts {
        loop {
            let frame = remote
                .downlink
                .lock()
                .expect("downlink lock")
                .try_recv()
                .expect("a hearsay record was queued");
            if let Some(contacts) = HearsayContacts::decode(&frame.payload) {
                assert_eq!(frame.lane, Lane::Meta);
                return contacts;
            }
        }
    }

    #[test]
    fn replica_scope_capture_covers_every_active_seat_pair() {
        use crate::bot::{bot_key, cell_of, grid_of};
        use crate::exterior::link_pair;
        use orrery_core::QPos;

        let cell_edge_m = crate::bot::campaign_cell_edge_m();
        let mut swarm = Swarm::new_for_island(
            SwarmConfig {
                peers: 3,
                cell_edge_m,
                campaign: true,
                replica_scope_capture: true,
                ..SwarmConfig::default()
            },
            6,
        );
        for seat in 3..6 {
            let (host_link, _remote_link) = link_pair();
            swarm = swarm.with_external_at(seat, 6, bot_key(seat).public(), None, host_link);
        }

        // Two humans occupy the same local encounter while the third is far
        // outside its 27-cell AOI. The log must make both classifications
        // explicit, rather than leaving an operator to infer distance from
        // raw cell ids.
        let near = swarm.bots[0].cell().expect("committed bot cell");
        swarm.exteriors.get_mut(&3).expect("first exterior").cell = near;
        swarm.exteriors.get_mut(&4).expect("second exterior").cell = near;
        swarm.exteriors.get_mut(&5).expect("third exterior").cell = cell_of(grid_of(
            &QPos::from_metres(900_000.0, 0.0, -900_000.0),
            cell_edge_m,
        ));

        let roster = swarm.active_roster();
        let captures = scope_capture_records(&roster);
        assert_eq!(captures.len(), 30, "six active seats have 6 * 5 pairs");

        for (subject_seat, _, subject_cell) in &roster {
            for (receiver_seat, _, receiver_cell) in &roster {
                if subject_seat == receiver_seat {
                    assert!(
                        !captures.iter().any(|capture| {
                            capture.subject_seat == *subject_seat
                                && capture.receiver_seat == *receiver_seat
                        }),
                        "a seat must never log a replica decision to itself"
                    );
                    continue;
                }
                let capture = captures
                    .iter()
                    .find(|capture| {
                        capture.subject_seat == *subject_seat
                            && capture.receiver_seat == *receiver_seat
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "missing capture for subject seat {subject_seat} to receiver seat {receiver_seat}"
                        )
                    });
                let expected_in_scope = receiver_cell.neighbors27().contains(subject_cell);
                assert_eq!(capture.subject_cell, *subject_cell);
                assert_eq!(capture.receiver_cell, *receiver_cell);
                assert_eq!(capture.in_scope, expected_in_scope);
                assert_eq!(
                    capture.reason,
                    if expected_in_scope {
                        ScopeReason::InInterest
                    } else {
                        ScopeReason::OutOfInterest
                    }
                );
            }
        }

        assert!(
            captures.iter().any(|capture| {
                capture.subject_seat == 3
                    && capture.receiver_seat == 4
                    && capture.reason == ScopeReason::InInterest
            }),
            "the first human must be logged as visible to the second human"
        );
        assert!(
            captures.iter().any(|capture| {
                capture.subject_seat == 3
                    && capture.receiver_seat == 5
                    && capture.reason == ScopeReason::OutOfInterest
            }),
            "the distant human pair must be logged as out of interest"
        );
    }

    #[test]
    fn hearsay_fold_never_delivers_a_contact_younger_than_the_v18_crossing_floor() {
        let (mut swarm, remotes) = hearsay_test_swarm(2, &[2, 3], 4);
        let cell_edge_m = CAMPAIGN_CELL_EDGE_M as u64;
        assert_eq!(
            cell_edge_m as f64, CAMPAIGN_CELL_EDGE_M,
            "campaign cell edge must be an exact whole metre for the H4 derivation"
        );
        let max_speed_mms = Archetype::ALL
            .iter()
            .map(|archetype| archetype.limits().max_speed_mms)
            .max()
            .expect("Regolith publishes at least one chassis");
        assert_eq!(
            max_speed_mms % 1_000,
            0,
            "maximum chassis speed must be an exact m/s for the H4 derivation"
        );
        let max_speed_mps = u64::try_from(max_speed_mms / 1_000).expect("speed is positive");
        let h4_floor_ticks = (cell_edge_m * TICK_HZ).div_ceil(max_speed_mps);
        assert_eq!(
            h4_floor_ticks, 64,
            "ceil(512 m / 480 m/s * 60 ticks/s) must be 64 ticks"
        );
        assert!(
            HEARSAY_FOLD_TICKS >= h4_floor_ticks,
            "the five-second hearsay fold ({HEARSAY_FOLD_TICKS} ticks) must be no younger than the v18 crossing floor ({h4_floor_ticks} ticks)"
        );
        let assert_old_enough = |record: HearsayContacts, delivery_tick: u64| {
            assert!(!record.contacts.is_empty(), "the age check is not vacuous");
            for contact in record.contacts {
                let delivered_age = delivery_tick
                    .saturating_sub(record.fold_tick)
                    .saturating_add(u64::from(contact.fact_age_ticks));
                assert!(
                    delivered_age >= h4_floor_ticks,
                    "seat {} was delivered at age {delivered_age}, below {h4_floor_ticks} ticks",
                    contact.seat
                );
            }
        };

        report_current_exterior_cells(&mut swarm, &remotes, 250);
        let first_fold_tick = HEARSAY_FOLD_TICKS - 1;
        let _ = swarm.refresh_rosters(first_fold_tick);
        for (_, remote) in &remotes {
            if let Ok(frame) = remote.downlink.lock().expect("downlink lock").try_recv() {
                let record = HearsayContacts::decode(&frame.payload)
                    .expect("a Meta downlink at the fold boundary is hearsay");
                assert_old_enough(record, first_fold_tick);
            }
        }

        report_current_exterior_cells(&mut swarm, &remotes, 550);
        let delivery_tick = 2 * HEARSAY_FOLD_TICKS - 1;
        let _ = swarm.refresh_rosters(delivery_tick);

        for (_, remote) in &remotes {
            assert_old_enough(receive_hearsay(remote), delivery_tick);
        }
    }

    #[test]
    fn hearsay_fold_does_not_change_replica_scope_capture_bytes() {
        fn deterministic_capture(fold_enabled: bool) -> (Vec<u8>, usize) {
            let (mut swarm, remotes) = hearsay_test_swarm(3, &[3, 4, 5], 6);
            swarm.hearsay_fold_enabled = fold_enabled;
            let mut capture = Vec::new();
            for second in 1..=10 {
                let tick = second * TICK_HZ - 1;
                report_current_exterior_cells(&mut swarm, &remotes, tick - 10);
                capture.extend_from_slice(swarm.refresh_rosters(tick).as_bytes());
            }
            let records = remotes
                .iter()
                .filter(|(_, remote)| {
                    remote
                        .downlink
                        .lock()
                        .expect("downlink lock")
                        .try_recv()
                        .ok()
                        .and_then(|frame| HearsayContacts::decode(&frame.payload))
                        .is_some()
                })
                .count();
            (capture, records)
        }

        let (with_fold, delivered) = deterministic_capture(true);
        let (without_fold, disabled_deliveries) = deterministic_capture(false);
        assert!(!with_fold.is_empty(), "the opt-in scope log ran");
        assert_eq!(delivered, 3, "the enabled run exercised fold delivery");
        assert_eq!(
            disabled_deliveries, 0,
            "the control really disabled the fold"
        );
        assert_eq!(
            with_fold, without_fold,
            "H2: hearsay must not change replica membership or rate"
        );
    }

    #[test]
    fn hearsay_fold_contains_only_crewed_seats() {
        let (mut swarm, remotes) = hearsay_test_swarm(5, &[5, 7], 8);
        report_current_exterior_cells(&mut swarm, &remotes, 250);
        let _ = swarm.refresh_rosters(HEARSAY_FOLD_TICKS - 1);
        report_current_exterior_cells(&mut swarm, &remotes, 550);
        let _ = swarm.refresh_rosters(2 * HEARSAY_FOLD_TICKS - 1);

        for (_, remote) in &remotes {
            let record = receive_hearsay(remote);
            let seats = record
                .contacts
                .iter()
                .map(|contact| usize::from(contact.seat))
                .collect::<Vec<_>>();
            assert_eq!(seats, vec![5, 7], "bots and the vacant seat stay absent");
        }
    }

    #[test]
    fn campaign_projectile_keeps_the_arc_verdict_it_had_when_fired() {
        use orrery_core::Executor;
        use orrery_games::game::Game;
        use orrery_games::regolith::archetype::Archetype;
        use orrery_games::regolith::firing_arc_measurement;
        use orrery_games::regolith::order::{Order, Outcome, ShotResult};
        use orrery_games::regolith::state::{Craft, LockClass};
        use orrery_games::regolith::LOCK_ACQUISITION_TICKS;
        use orrery_games::Regolith;
        use orrery_protocol::Tick;

        let mut universe = [0u8; 32];
        universe[..8].copy_from_slice(&0x61_u64.to_le_bytes());
        let seed = UniverseSeed(universe);
        let shooter = PersistId::new(9);
        let target = PersistId::new(3);
        let shooter_pos = orrery_core::QPos {
            x: 2_484_791,
            y: 0,
            z: 338_808,
        };
        let shooter_yaw = 3_079_384;
        let mut shooter_state = Craft::spawned(Archetype::Interceptor, shooter_pos, shooter_yaw);
        shooter_state.lock_target = Some(target);
        shooter_state.lock_class = Some(LockClass::Ship);
        shooter_state.lock_progress = LOCK_ACQUISITION_TICKS;
        let mut shooter_executor = Executor::new(Regolith::honest(), seed);
        shooter_executor.insert(shooter, RegolithState::Craft(shooter_state));
        let mut target_bot = Bot::new(BotSpec {
            index: 2,
            count: 8,
            seed,
            cell_edge_m: crate::bot::default_cell_edge_m(),
            witnessing: false,
            cheat: None,
            enforcing: false,
        });
        target_bot.enable_resolved_shot_capture();
        let mut target_state = Craft::spawned(
            Archetype::Interceptor,
            orrery_core::QPos {
                x: 2_335_587,
                y: 0,
                z: 489_809,
            },
            2_391_612,
        );
        target_state.vel = orrery_core::QVel {
            x: -13_623,
            y: 0,
            z: 50_517,
        };
        target_bot.replace_craft_for_test(target_state);

        let fired = shooter_executor
            .step_entity(shooter, Tick::new(505), &[Order::Fire])
            .expect("recorded shooter exists");
        let damage = fired
            .events
            .iter()
            .find_map(|event| shooter_executor.ruleset().deliver(event))
            .map(|(_, order)| order)
            .expect("the mature lock emits damage");
        let target_pos = target_bot.craft().pos;
        let measurement =
            firing_arc_measurement(Archetype::Interceptor, shooter_yaw, shooter_pos, target_pos);
        assert_eq!(
            target_pos,
            orrery_core::QPos {
                x: 2_335_587,
                y: 0,
                z: 489_809,
            },
            "the campaign target geometry must stay pinned"
        );
        assert_eq!(measurement.world_bearing_urad, Some(2_350_207));
        assert_eq!(measurement.relative_urad, Some(5_554_008));
        assert!(measurement.inside, "the shot starts inside the drawn arc");
        target_bot.inject_delivered(shooter, damage);

        let mut resolution = None;
        for tick in 505..600 {
            target_bot.step_core(tick, crate::bot::default_cell_edge_m());
            resolution =
                target_bot
                    .take_resolved_shots()
                    .into_iter()
                    .find_map(|event| match event {
                        Outcome::ShotResolved {
                            attacker,
                            target: resolved_target,
                            result,
                        } if attacker == shooter && resolved_target == target => Some(result),
                        _ => None,
                    });
            if resolution.is_some() {
                break;
            }
        }
        assert!(
            matches!(resolution, Some(ShotResult::Hit | ShotResult::Miss)),
            "an accepted in-flight shot must reach the damage roll, got {resolution:?}"
        );
    }

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
                keyframe_every_sends: 20,
                upload_budget_bits: 1_000_000,
                swept_interest_margin: false,
                varied_profiles: true,
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
            max_boundary_returns_in_window: 0,
            boundary_return_histogram: Vec::new(),
            boundary_return_profiles: Vec::new(),
            delta_stats: None,
            shot_interest_stats: None,
            delivery_gaps: None,
            interest_margin: None,
            meter_budget_bits: 1_000_000,
            worst_peak_upload_bits: 973_000,
            worst_p99_upload_bits: 906_000,
            min_cells_visited: 81,
            total_shed: 0,
            shed_keyframes: 0,
            shed_deltas: 0,
            shed_replication_other: 0,
            total_boundary_flips: 0,
            total_proxy_pops: 0,
            total_interest_churn: 8_426,
            stranded_in_flight: 0,
            total_undecodable: 0,
            deltas_unanchored: 0,
            deltas_without_any_keyframe: 0,
            deltas_missing_newer_keyframe: 0,
            deltas_with_superseded_keyframe: 0,
            deltas_with_invalid_reference: 0,
            keyframes_discarded_while_stalled: 0,
            deltas_discarded_while_stalled: 0,
            total_replicas: 992,
            witnessing: true,
            external: Vec::new(),
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
            replication_bits_per_sec: 0,
            keyframe_messages: 0,
            delta_messages: 0,
            keyframe_bytes: 0,
            delta_bytes: 0,
            keyframe_message_share: 0.0,
            keyframe_byte_share: 0.0,
            witness_bytes: 0,
            control_bytes: 0,
            control_bits_per_sec: 0,
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
                initial_replicas: 0,
                in_neighbourhood: 15,
                tracked: 15,
            }),
        }
    }

    #[test]
    fn profile_mix_can_be_measured_independently_of_witness_traffic() {
        let cruise = Swarm::new(SwarmConfig {
            peers: 4,
            witnessing: true,
            varied_profiles: Some(false),
            ..SwarmConfig::default()
        });
        assert!(
            cruise
                .bots
                .iter()
                .all(|bot| bot.profile == crate::profile::Profile::Cruise),
            "witness traffic could not be isolated from the stall profile"
        );

        let varied = Swarm::new(SwarmConfig {
            peers: 4,
            witnessing: false,
            varied_profiles: Some(true),
            ..SwarmConfig::default()
        });
        assert_eq!(
            varied
                .bots
                .iter()
                .map(|bot| bot.profile)
                .collect::<Vec<_>>(),
            crate::profile::Profile::ALL,
            "the profile load could not be measured without witness traffic"
        );
    }

    #[test]
    fn budget_override_reaches_every_peers_real_upload_meter() {
        let requested = 700_000;
        let swarm = Swarm::new(SwarmConfig {
            peers: 4,
            upload_budget_bits: requested,
            ..SwarmConfig::default()
        });
        assert!(
            swarm
                .bots
                .iter()
                .all(|bot| bot.upload_budget_bits() == requested),
            "A20 pressure was parsed but did not reach an UploadBudget resource"
        );
    }

    #[test]
    fn delivery_gap_report_counts_trailing_silence() {
        let sender = crate::bot::bot_key(0).public();
        let recipient = crate::bot::bot_key(1).public();
        let entity = PersistId::new(1);
        let mut tracker = DeliveryGapTracker::default();
        tracker.observe(sender, entity, recipient, 3);
        tracker.observe(sender, entity, recipient, 6);

        let report = tracker.report(60);
        assert_eq!(report.completed_gap_histogram.get(&3), Some(&1));
        assert_eq!(report.p99_gap_ticks, 3);
        assert_eq!(report.pairs[0].trailing_gap_ticks, 54);
        assert_eq!(
            report.max_gap_ticks, 54,
            "a sender that stops after one ordinary interval must not look healthy"
        );
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

    #[test]
    fn a_late_joiner_with_receive_history_fails_the_run() {
        let mut report = passing();
        report
            .late_join
            .as_mut()
            .expect("the passing fixture has a late join")
            .initial_replicas = 1;
        assert!(
            clauses(&report, STRICT).contains(&"the late joiner starts with no retained replicas")
        );
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
            .exteriors
            .get_mut(&1)
            .expect("external slot")
            .pump_uplink(7, &mut swarm.router);

        assert_eq!(swarm.router.counters.dropped, 1, "the router dropped it");
        let exterior = swarm.exteriors.get(&1).expect("external slot");
        assert_eq!(exterior.connected_ticks, 1);
        assert_eq!(exterior.uplink_delivered, 0);
        assert_eq!(exterior.uplink_dropped, 1);
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
            .exteriors
            .get_mut(&1)
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
        let exterior = swarm.exteriors.get(&1).expect("external slot");
        assert_eq!(
            exterior.uplink_delivered + exterior.uplink_dropped,
            SENT as u64
        );
        assert_eq!(exterior.uplink_dropped, client_dropped);
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
        if let Some(exterior) = swarm.exteriors.get_mut(&ext_index) {
            exterior.pump_uplink(0, &mut swarm.router);
        }
        if let Some(exterior) = swarm.exteriors.get_mut(&ext_index) {
            while let Ok(raw) = {
                let mut r = exterior.link.meta.lock().expect("meta lock");
                r.try_recv()
            } {
                exterior.set_cell_from_bits(raw, 0);
            }
            assert_eq!(exterior.cell(), moved, "meta updated the roster cell");
        } else {
            panic!("the exterior slot was attached");
        }
    }

    /// The live-client census reads canonical replication, so this assertion
    /// crosses the same host routing seam rather than merely inspecting seeds.
    #[test]
    fn campaign_broadcasts_seeded_rocks_to_the_exterior_slot() {
        use crate::bot::bot_key;
        use crate::exterior::link_pair;
        use orrery_games::regolith::state::RockTier;

        let mut swarm = Swarm::new(SwarmConfig {
            peers: 8,
            seconds: 1,
            cell_edge_m: crate::bot::campaign_cell_edge_m(),
            campaign: true,
            witnessing: false,
            ..SwarmConfig::default()
        });
        let (host_link, remote_link) = link_pair();
        swarm = swarm.with_external(bot_key(8).public(), None, host_link);
        swarm.form_island();

        let mut phase = [0u128; 6];
        swarm.tick_once(0, 1, &mut phase);

        let mut tiers = [0usize; 3];
        while let Ok(frame) = remote_link
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
        {
            if frame.lane != Lane::Datagram {
                continue;
            }
            let Some((_, inner)) = orrery_protocol::channels::untag(&frame.payload) else {
                continue;
            };
            let Some((encoded, _, _, _)) =
                orrery_protocol::channels::decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(
                    inner,
                )
            else {
                continue;
            };
            let Ok(RegolithState::Rock(rock)) = RegolithState::decode(&encoded) else {
                continue;
            };
            tiers[match rock.tier {
                RockTier::Large => 0,
                RockTier::Medium => 1,
                RockTier::Small => 2,
            }] += 1;
        }
        assert_eq!(
            tiers,
            [1, 2, 3],
            "the exterior state census must receive every seeded rock tier"
        );
    }

    #[test]
    fn campaign_rock_authority_accepts_collision_and_routes_lock_reply() {
        use crate::bot::bot_key;
        use crate::exterior::link_pair;
        use orrery_core::QVel;
        use orrery_games::regolith::campaign_rock_seeds;
        use orrery_games::regolith::order::Order;
        use orrery_games::regolith::state::Rock;

        let mut swarm = Swarm::new(SwarmConfig {
            peers: 8,
            seconds: 1,
            cell_edge_m: crate::bot::campaign_cell_edge_m(),
            campaign: true,
            witnessing: false,
            ..SwarmConfig::default()
        });
        let exterior_entity = PersistId::new(9);
        let exterior_node = bot_key(8).public();
        let mut universe = [0u8; 32];
        universe[..8].copy_from_slice(&1u64.to_le_bytes());
        let target = campaign_rock_seeds(UniverseSeed(universe), 8)[0].clone();
        let (host_link, remote_link) = link_pair();
        swarm = swarm.with_external(exterior_node, None, host_link);
        swarm.form_island();

        for order in [
            Order::LockRequested {
                locker: exterior_entity,
            },
            Order::CollisionResolved {
                from: exterior_entity,
                velocity: QVel {
                    x: 1_000,
                    y: 0,
                    z: 0,
                },
            },
        ] {
            let inner = orrery_protocol::channels::encode_delivered_input(
                exterior_entity,
                target.entity,
                &order.to_canonical(),
            );
            let payload = Bytes::from(orrery_protocol::channels::tag(
                orrery_protocol::channels::Channel::Control,
                &inner,
            ));
            swarm.bots[target.owner_slot].receive_inbound(
                exterior_node,
                exterior_entity,
                Some(StreamMode::Shared),
                payload,
            );
        }

        let mut phase = [0u128; 6];
        swarm.tick_once(0, 1, &mut phase);
        swarm.tick_once(1, 1, &mut phase);

        let mut confirmed = false;
        let mut collided: Option<Rock> = None;
        let mut keyframes = crate::bot::ReplicaKeyframes::default();
        while let Ok(frame) = remote_link
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
        {
            match frame.lane {
                Lane::StreamShared => {
                    let delivered =
                        orrery_protocol::channels::untag(&frame.payload).and_then(|(_, inner)| {
                            orrery_protocol::channels::decode_delivered_input(inner)
                        });
                    confirmed |= delivered.is_some_and(|delivered| {
                        delivered.from == target.entity
                            && delivered.recipient == exterior_entity
                            && matches!(
                                Order::decode(&delivered.input),
                                Ok(Order::LockConfirmed { target: locked, .. })
                                    if locked == target.entity
                            )
                    });
                }
                Lane::Datagram => {
                    let state = orrery_protocol::channels::untag(&frame.payload)
                        .and_then(|(_, inner)| {
                            crate::bot::decode_replica(inner, &mut keyframes).ok()
                        })
                        .and_then(|replication| {
                            (replication.entity == target.entity)
                                .then(|| RegolithState::decode(&replication.canonical).ok())
                                .flatten()
                        });
                    if let Some(RegolithState::Rock(rock)) = state {
                        collided = Some(rock);
                    }
                }
                Lane::StreamBulk | Lane::Meta => {}
            }
        }
        assert!(confirmed, "the rock authority answers the exterior lock");
        let collided = collided.expect("the changed rock state was replicated");
        assert_eq!(collided.collisions, 1);
        assert_eq!(
            collided.vel,
            QVel {
                x: 1_000,
                y: 0,
                z: 0
            }
        );
    }

    /// A rendered campaign starts with the exterior craft and the host crowd in
    /// one local encounter. Holding the player's craft still must not turn that
    /// encounter into a four-second fly-by merely because the P1 gate's bots
    /// normally roam across cells for an hour.
    #[test]
    fn stationary_campaign_player_keeps_initial_contacts_in_scope() {
        use crate::bot::bot_key;
        use crate::exterior::link_pair;

        let mut swarm = Swarm::new(SwarmConfig {
            peers: 8,
            seconds: 45,
            cell_edge_m: crate::bot::campaign_cell_edge_m(),
            campaign: true,
            witnessing: false,
            ..SwarmConfig::default()
        });
        let (host_link, _remote_link) = link_pair();
        swarm = swarm.with_external(bot_key(8).public(), None, host_link);
        swarm.form_island();

        let receiver_cell = swarm.exteriors.get(&8).expect("external slot").cell();
        let interest = receiver_cell.neighbors27();
        let initially_visible: Vec<PersistId> = swarm
            .bots
            .iter_mut()
            .filter_map(|bot| {
                interest
                    .contains(&bot.cell().expect("committed"))
                    .then(|| bot.entity())
            })
            .collect();
        assert!(
            !initially_visible.is_empty(),
            "the campaign fixture must begin with a visible contact"
        );

        // Stock's own envelope against an interceptor, read off the ruleset
        // table rather than restated: #545 cut Stock from 400 m to 320 m and a
        // hand-copied 406_000 would have quietly stopped meaning "stock reach".
        let stock_reach_mm = (orrery_games::regolith::weapon::WeaponKind::Stock
            .weapon()
            .reach_mm()
            + orrery_games::regolith::archetype::Archetype::Interceptor
                .limits()
                .radius_mm) as u128;
        let mut phase = [0u128; 6];
        let (receiver_pos, _) = crate::bot::spawn_pose(8, 9);
        let mut saw_far_departure = false;
        for tick in 0..45 * TICK_HZ {
            swarm.tick_once(tick, 3, &mut phase);
            if tick % TICK_HZ == TICK_HZ - 1 {
                let _ = swarm.refresh_rosters(tick);
                for bot in &mut swarm.bots {
                    let in_scope = interest.contains(&bot.cell().expect("committed"));
                    let distance =
                        orrery_games::regolith::distance_mm(bot.craft().pos, receiver_pos);
                    if distance <= stock_reach_mm {
                        assert!(
                            in_scope,
                            "contact {} left scope at tick {tick}, only {distance} mm from the stationary player",
                            bot.entity().0,
                        );
                    } else if !in_scope {
                        saw_far_departure = true;
                    }
                }
            }
        }
        assert!(
            saw_far_departure,
            "the test must still exercise a genuine AOI departure after interaction range"
        );
    }

    #[test]
    fn host_authoritative_lock_reply_routes_back_to_the_external_authority() {
        use crate::bot::bot_key;
        use crate::exterior::link_pair;
        use orrery_games::regolith::order::Order;

        let mut swarm = Swarm::new(SwarmConfig {
            peers: 1,
            seconds: 1,
            witnessing: false,
            ..SwarmConfig::default()
        });
        let exterior_index = 1;
        let exterior_node = bot_key(exterior_index).public();
        let exterior_entity = PersistId::new(2);
        let target = swarm.bots[0].entity();
        let (host_link, remote_link) = link_pair();
        swarm = swarm.with_external(exterior_node, None, host_link);
        swarm.form_island();

        let inner = orrery_protocol::channels::encode_delivered_input(
            exterior_entity,
            target,
            &Order::LockRequested {
                locker: exterior_entity,
            }
            .to_canonical(),
        );
        let payload = Bytes::from(orrery_protocol::channels::tag(
            orrery_protocol::channels::Channel::Control,
            &inner,
        ));
        swarm.bots[0].receive_inbound(
            exterior_node,
            exterior_entity,
            Some(StreamMode::Shared),
            payload,
        );

        let mut phase = [0u128; 6];
        swarm.tick_once(0, 20, &mut phase);
        swarm.tick_once(1, 20, &mut phase);

        let mut saw_confirmation = false;
        while let Ok(frame) = remote_link
            .downlink
            .lock()
            .expect("downlink lock")
            .try_recv()
        {
            let delivered = orrery_protocol::channels::untag(&frame.payload)
                .and_then(|(_, inner)| orrery_protocol::channels::decode_delivered_input(inner));
            let Some(delivered) = delivered else {
                continue;
            };
            saw_confirmation |= delivered.from == target
                && delivered.recipient == exterior_entity
                && matches!(
                    Order::decode(&delivered.input),
                    Ok(Order::LockConfirmed {
                        target: confirmed,
                        ..
                    }) if confirmed == target
                );
        }
        assert!(
            saw_confirmation,
            "the host target's authoritative step must route its reply to the exterior entity"
        );
    }
}
