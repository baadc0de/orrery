//! P1's replication harness (docs/11-roadmap.md §P1).
//!
//! The phase's demo criterion: **32 synthetic peers, scripted roaming across
//! ≥64 interest cells, run for one hour — every peer's sustained upload stays
//! ≤ 1 Mbps; interest-set churn is absorbed without visible proxy pops; no
//! entity thrashes cells at a boundary; a late-joining peer receives only its
//! 27-cell neighborhood.**
//!
//! Like the P2 and P3 gates this is a *proof harness*, not a convenience
//! script: it exits non-zero unless every clause holds, and prints which one
//! did not.
//!
//! # What is real here and what is not
//!
//! Real: `orrery_spatial`'s hysteresis, AOI and bounded interest selection;
//! `orrery_net`'s send path, channel policy and upload meter with wire-byte
//! accounting; `orrery_games`' Regolith driving the motion. Every number the
//! report prints is produced by shipping code.
//!
//! The ruleset used to be `orrery_conformance`'s reference kernel, and swapping
//! it was not a rename. The corpus kernel is deliberately *not* a game: it
//! publishes no archetype limits, so `Ruleset::invariants()` falls through to
//! `orrery_core`'s `&[]` default and **every player-hour this harness had ever
//! accumulated ran stage 1 against an empty slice**. `SignalTally`'s
//! `invariant_breaches` was a dead term in the false-positive sum, and "no
//! false-positive discrepancy signal against an honest peer" was measuring
//! log re-execution alone. Regolith publishes the movement, fire-rate,
//! equipment and score checks, and they run on every sample every peer
//! receives. It also ships its own cheats, which is what
//! makes the conviction leg below possible at all.
//!
//! Not real: the socket. Peers are coupled by an in-process router. Transport
//! is P0's criterion and is already met; what P1 asks about is what a peer
//! *decides* to send and how it reacts to what arrives. Making the link a
//! parameter also buys the thing P4 needs next — seeded 3–5% loss and 100 ms
//! jitter, reproducible enough that a false positive can be replayed.
//!
//! Also not real: island *formation*. The harness installs the roster directly,
//! because forming one is P3's criterion and is separately proven; what is
//! under test here is what a peer does with a roster.
//!
//! # Witnessing (`--witness`) is one half of P4's criterion; `--cheat` is the
//! other
//!
//! With `--witness` every peer streams a real signed log to its witness set and
//! re-executes what it is streamed. Every bot is honest by construction — each
//! logs exactly the inputs it applied — so any signal beyond a chain gap is a
//! false positive, which is what makes the count meaningful without a separate
//! oracle for who was cheating.
//!
//! That measures the false-positive rate and says nothing about whether the
//! pipeline catches anybody, and the two are only evidence together: a witness
//! tuned until it accuses nobody passes the honest legs perfectly. `--cheat`
//! closes it. It hands N peers a tampered `Regolith` build, arms every witness
//! (`--enforce`, implied), and re-runs each filed `DiscrepancyReport` through
//! an in-process adjudicator built on `orrery_witness::verify_report` and
//! `orrery_core::verify_bundle` — which believes nothing the reporter said.
//!
//! Measured at the criterion's own population, `--peers 8 --seconds 300
//! --impaired --witness --cheat speed`: the modified peer diverges on **tick 0**
//! and is confirmed on **tick 32**, against a 180-tick adjudication window. 41
//! reports are filed against it, every one of them `Verdict::Confirms` under
//! independent re-execution, and **zero** against any of the seven honest peers.
//! The same island with every witness armed and nobody modified (`--witness
//! --enforce`) files nothing at all.
//!
//! Three things about that number are worth keeping.
//!
//! **The cheat had to be aimed.** `Tamper::SpeedMultiplier` raises an
//! archetype's ceilings by 1.5×, and this roam requests `accel_mmss` 60 000 —
//! *exactly* the interceptor's `max_accel_mmss`. On that slot both builds clamp
//! to the same number and the tampered peer is byte-identical to an honest one:
//! nothing to detect, nothing filed, and every conviction clause passing over a
//! swarm in which nothing happened. Modified peers are pinned to the cruiser
//! slot, whose ceiling is 20 000, and `bot::tests` asserts both halves of that.
//! Neither speed ceiling ever binds — 32 m/s against 120 and 60 — so the
//! acceleration clamp is the whole of this cheat at these parameters.
//!
//! **It stops filing after one window, and that is correct.** A subject that
//! diverges permanently never agrees with its witness again, so `audit_window`
//! runs out of agreed claims 180 ticks past the anchor and everything after is
//! `escalations_unservable` — 4026 of them over five minutes at eight peers.
//! The convictions all happen in the first 32 ticks.
//!
//! **Stage 1 never fires on it.** The cheat is worth 167 mm/s of velocity per
//! *thrusting* tick, and a cruising bot thrusts about one tick in nineteen, so
//! across a 20 Hz sample gap the change stays well inside
//! `regolith/acceleration-cap`'s per-tick allowance. It is caught by
//! re-execution and only by re-execution — which is the argument for stage 1
//! being a filter rather than a verdict, arriving from the direction the
//! `DamageInflation` cheat was supposed to make it.
//!
//! ## The honest half
//!
//! `--witness` works and it found real defects. Both of the reasons it could not
//! accumulate P4's 500 honest player-hours are now closed: the repair budget
//! bounded the traffic that reached 8.7 Mbps against a 1 Mbps allowance, and
//! the cost that grew faster than the peer count is linear again — 32 peers
//! over 60 simulated seconds runs in about ten wall seconds.
//!
//! It holds the criterion at eight, sixteen and thirty-two peers: **zero false
//! positives at 100% observation coverage** on a clean link, and zero at 100%
//! across the whole of the criterion's 3–5% loss band. Both numbers are printed
//! together because neither is readable alone — a witness that has stopped
//! watching also reports zero. That pairing is what caught the last coverage
//! defect: the impaired band read 96.0% at 3% and 93.8% at 5% while reporting
//! nothing, and the deficit turned out to be whole watches killed by one lost
//! frame each rather than anything about the repairs.
//!
//! Since it holds, it gates: `scripts/p1-swarm-gate.sh` runs the impaired hour
//! with `--witness` as its third leg, then the conviction and control islands as
//! its fourth and fifth — nightly and blocking, and the only place in the tree
//! the witness pipeline runs at all. Every clause guarded by
//! `SwarmConfig.witnessing` was dead code before the third leg existed, and the
//! six guarded by `SwarmReport::conviction` were dead code before the fourth.
//!
//! # What thirty-two peers cost, and which dial paid for it
//!
//! Thirty-two used to fail, and not as a bandwidth nuisance: peak upload reached
//! 1006 kbps against the 1 Mbps allowance, ~15 000 replication packets were
//! shed, and because the backstop shed log frames and replication updates
//! indifferently, every shed frame opened a chain hole, every hole became a
//! repair on the unsheddable control lane, coverage fell to 81% and 582 signals
//! were raised against bots that are honest by construction. A false-positive
//! count taken at 81% coverage is a statement about which frames arrived.
//!
//! The lane was at 384 kbps per peer against docs/03-replication.md §5.3's
//! 0.15–0.2 Mbps, and the cause was neither the seven links nor the 2 Hz claims
//! but the **frame cadence**: one frame per 20 Hz send, of which ~250 of 316
//! wire bytes — signature, ruleset digest, head pair, datagram framing — is paid
//! per *frame* rather than per tick. Frames now cover ten ticks at 6 Hz, derived
//! from a declared 20% share of the peer budget (docs/03-replication.md §5.3a),
//! which is still four times faster than the 250 ms sustained-violation window
//! that has to elapse before any signal is actionable.
//!
//! | 32 peers, 300 simulated seconds, clean link | 20 Hz frames | 6 Hz frames |
//! |---|---|---|
//! | Witness lane per peer | 384 kbps wanted | **190 kbps** (share: 200) |
//! | Worst peak upload | **1006 kbps** | 973 kbps |
//! | Replication packets shed | 14 630 | 200 |
//! | Observation coverage | 81.3% | **100.0%** |
//! | False positives | 582 | **0** |
//!
//! That table was measured on `orrery_conformance`'s corpus kernel. Regolith
//! applies drag and a per-archetype speed clamp where the kernel applied
//! neither, so every trajectory moved and the seeded figures moved with them:
//! over the criterion's hour under the impairment profile the lane sits at
//! **177 kbps**, worst peak upload **907 kbps**, **278 packets shed**, across
//! **32 accumulated player-hours with zero false positives and 100% coverage**
//! — and 94 shed at the 5% end of the band, also at zero and 100%.
//!
//! The residual shed packets are replication bytes belonging to the stalling
//! peers in the densest part of the crowd, and the count is *identical* at five
//! simulated minutes and at one hour — a transient at island formation rather
//! than a sustained overrun. What produces it is the preference order working: a peer
//! recovering from a client hitch serves its witnesses' repair burst on the
//! control lane, which is never shed, and sheds the cheap lane to afford it.
//! Which is why the report prints the per-lane split — so the next overrun names
//! its own cause instead of being attributed to whichever lane was suspected.
//!
//! Coverage is printed at all because of what it caught. Before re-anchoring
//! existed, a watch that gave up on a hole stopped judging its subject
//! permanently — and nothing in the report said so, because a witness that
//! judges nothing also accuses nobody. Every witness counter froze at about
//! twenty-five simulated seconds and stayed frozen: identical gap, stall and
//! overflow totals at 30 s and at 120 s. The hours would have accumulated and
//! the false-positive count would have been zero for the worst possible reason.
//!
//! Note what the profiles do to the roaming figures: `--witness` runs assign
//! idle, burst and stall behaviours where the plain run is all cruise, so the
//! least-travelled peer legitimately visits one cell. The interest clauses are
//! measured on the cruise-only run; these runs are about the witness.
//!
//! And one it structurally cannot do: every bot here shares a binary and a
//! `libm`, so re-execution is bit-identical by construction and the
//! cross-platform divergence false positive — the one D9's whole apparatus
//! exists to prevent — cannot occur. That belongs to the determinism matrix,
//! extended to exchange logs *between* platform legs.
//!
//! # Time
//!
//! Simulated. Each peer's clock advances exactly one 60 Hz tick per frame, so
//! rates are bytes per *simulated* second — which is what the budget is about,
//! since the send cadence is 20 Hz of sim ticks. An hour of play costs what it
//! costs to compute, and a run is reproducible.

mod adjudicate;
mod bot;
mod bridge;
mod delta_stats;
mod exterior;
mod peer_runner;
mod profile;
mod router;
mod shot_interest;
mod swarm;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use orrery_core::CoreCodec as _;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{mpsc, Arc, Mutex};

use orrery_games::game::Tamper;
use orrery_games::regolith::state::RegolithState;
use orrery_protocol::NodeId;
use router::Impairment;
use swarm::{CheatSpec, Criterion, SeatReclaim, Swarm, SwarmConfig};

/// The D6/D16 peer upload budget.
const BUDGET_BITS: u64 = 1_000_000;

/// Which honest bot profiles a measurement deals.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProfileMode {
    /// Preserve the gate posture: varied with witnessing, cruise otherwise.
    Auto,
    /// Deal only smooth cruising bots, even while witnessing.
    Cruise,
    /// Deal cruise, idle, burst and stall profiles, even without witnessing.
    Varied,
}

#[derive(Parser, Debug)]
#[command(
    name = "p1-swarm",
    about = "P1 replication harness: roaming peers against the phase criterion"
)]
struct Args {
    /// Peers in the swarm. The criterion says 32.
    #[arg(long, default_value_t = 32)]
    peers: usize,

    /// Simulated seconds to run. The criterion says one hour.
    #[arg(long, default_value_t = 3_600)]
    seconds: u64,

    /// Distinct interest cells the least-travelled peer must visit.
    #[arg(long, default_value_t = 64)]
    min_cells: usize,

    /// Apply P4's impairment profile: 3% loss, 100 ms jitter spikes.
    #[arg(long)]
    impaired: bool,

    /// Move the impaired profile's loss within the criterion's 3–5% band.
    ///
    /// The band had only ever been run at its 3% floor, and running the other
    /// end is what found the coverage defect: at 5% the witnessed hour judged
    /// **93.8%** of the timeline it was shown against the 95% floor, and the
    /// per-peer figures came out at exact sevenths — whole watches that never
    /// folded a frame, not repairs arriving late. Both ends now judge
    /// **100.0%** with zero false positives (docs/11-roadmap.md §P4). The gate
    /// runs the floor nightly; this flag is what keeps the other end from
    /// going unexercised again.
    ///
    /// Ignored without `--impaired`, which is the flag that selects the profile
    /// at all.
    #[arg(long)]
    loss: Option<f64>,

    /// Proxy pops tolerated before the clause fails.
    ///
    /// Zero on a clean link — that is P1's criterion. Under loss a peer ranks
    /// its neighbours on positions that are one or more updates stale, so a
    /// small number of evictions are decided on old information; that is a
    /// consequence of the loss, not of the eviction policy, and §9.1/§9.2 own
    /// it. The allowance stays far below a real regression: removing the
    /// demotion ramp produced 2870 pops in a five-minute run.
    #[arg(long, default_value_t = 0)]
    max_pops: u64,

    /// Packets the send path may shed for want of budget before the clause
    /// fails.
    ///
    /// Zero is the criterion, and zero is what the cruise-only runs hold on
    /// both links. The witness lane makes a small transient real: at island
    /// formation a peer recovering from a hitch serves its witnesses' repair
    /// burst on the unsheddable control lane and sheds the cheap lane to afford
    /// it. What says that is a transient and not an overrun is that the count
    /// is the same at five simulated minutes as at one hour.
    ///
    /// The gate's witnessed leg passed this as an exact per-seed ratchet — 206,
    /// then 230, then 162, then 278, the measured number each time — on the
    /// premise that at a fixed seed and loss point the count is a single number,
    /// so a run that moved it had found something. #974 measured the premise and
    /// it is false: holding the simulation entirely fixed and re-rolling only
    /// the impairment realisation, the count ranges 32–420. The legs now pass a
    /// **band**, derived in `scripts/p1-swarm-gate.sh`; see
    /// `swarm::Criterion::max_shed` for why, and
    /// `--max-unsheddable-over-budget` for the bound §9.3 actually names.
    #[arg(long, default_value_t = 0)]
    max_shed: u64,

    /// Unsheddable sends charged while over budget before the clause fails.
    ///
    /// docs/03-replication.md §9.3's own overrun signal, judged from #974 —
    /// before it, `unsheddable_over_budget` was measured, printed on every run,
    /// and looked at by nothing. Zero is the criterion and zero is what every
    /// leg but the 32-peer witnessed ones measures; those pass a derived
    /// formation allowance, because the counter reads 1–42 on healthy runs
    /// there and never 0.
    #[arg(long, default_value_t = 0)]
    max_unsheddable_over_budget: u64,

    /// Seed for impairment and the universe.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Override the interest-cell edge for an in-process measurement leg.
    ///
    /// `128` is the gate default and Regolith's campaign uses `512`; the
    /// presence observer needs both without turning a measurement into an
    /// external-peer session.
    #[arg(long)]
    cell_edge_m: Option<f32>,

    /// Simulated second at which to run the late-join check.
    #[arg(long)]
    late_join_at: Option<u64>,

    /// Bot profile mix, independently of whether witness traffic is enabled.
    #[arg(long, value_enum, default_value_t = ProfileMode::Auto)]
    profile_mode: ProfileMode,

    /// State-send opportunities between sender-clocked keyframes.
    #[arg(long, default_value_t = 20)]
    keyframe_every_sends: u64,

    /// Override the send path's sustained allowance, in decimal kilobits/s.
    ///
    /// A20's pressure sweep changes only this meter input. The P1 criterion
    /// remains fixed at 1 Mbps, so a failed clause is reported as a finding.
    #[arg(long)]
    budget_kbps: Option<u64>,

    /// Enable #653's two-part interest coverage: #692's swept one-refresh-period
    /// cells in every bot manifest, plus the ordered crossing event that
    /// corrects the host roster on the commitment instead of at the next 1 Hz
    /// refresh.
    ///
    /// Both halves are deliberate and neither subsumes the other. At v18's
    /// 480 m/s interceptor ceiling a craft clears the 460.8 m one-body AOI
    /// guarantee in 0.96 s, inside one refresh period: the swept set covers
    /// where it can get to, the event covers the tail when it gets somewhere
    /// else, and the bulk refresh stays the repair path for a lost event.
    #[arg(long)]
    swept_interest_margin: bool,

    /// Report post-meter per-(entity, link) replication delivery gaps.
    #[arg(long)]
    delivery_gaps: bool,

    /// Write the full report as JSON to this path.
    #[arg(long)]
    json: Option<String>,

    /// Measure canonical changed bytes against the previous send and a modeled
    /// 1 Hz keyframe, grouped by body type in the JSON report.
    #[arg(long)]
    delta_stats: bool,

    /// Measure whether each shot's attacker was in the victim's replicated
    /// interest scope at the resolution tick, including attacker-speed stats.
    #[arg(long)]
    shot_interest_stats: bool,

    /// Measure roaming audience churn and missing-newer anchor windows.
    ///
    /// This observer changes no presence policy or criterion.
    #[arg(long)]
    presence_stats: bool,

    /// Print the report and exit zero even if a clause failed.
    #[arg(long)]
    report_only: bool,

    /// Run the witness pipeline: every peer re-executes its witness set's
    /// signed logs, and any signal against an honest peer is a false positive.
    ///
    /// Also deals the awkward behavioural profiles (idle / burst / stall), since
    /// they exist to stress the witness rather than the spatial stack. **This is
    /// P4's input and does not yet meet P4's criterion** — see the module docs.
    #[arg(long)]
    witness: bool,

    /// Field modified clients: `<tamper>[:count]`, e.g. `speed` or `speed:2`.
    ///
    /// P4's demo criterion, and the half `--witness` alone cannot reach. It
    /// hands `count` peers a tampered `Regolith` build and re-runs each filed
    /// report through an in-process adjudicator that believes nothing the
    /// reporter said. Implies `--witness` and `--enforce`: without a witness
    /// there is nobody to detect anything, and in shadow mode nobody files.
    ///
    /// The tampers are `orrery_games`' three. The shared Regolith pilot holds
    /// the trigger, so damage inflation and cooldown bypass are live alongside
    /// the speed-ceiling probe. A cheat that turns out to be inert fails the
    /// "actually diverges" clause rather than passing over identical state.
    #[arg(long, value_name = "TAMPER[:COUNT]")]
    cheat: Option<String>,

    /// Take every witness out of shadow mode: file what it raises.
    ///
    /// Shadow mode is P4's posture and the default (D17 risk 3) — check
    /// everything, file nothing, until the false-positive rate has been
    /// measured. That makes "an unmodified swarm files no report at all" a
    /// tautology on every honest leg, which is what this flag exists to break:
    /// an entirely honest swarm with every witness *armed* filing zero is a
    /// measurement, and it is the control the conviction leg is only evidence
    /// against.
    #[arg(long)]
    enforce: bool,

    /// Stamp the report with the wall-clock second the run started.
    ///
    /// Off by default, and outside the report's identity block when on: a run
    /// is a function of its seed, and a timestamp inside the reproducible body
    /// would make two identical runs compare unequal. Evidence uploads want it;
    /// a developer comparing two seeds does not.
    #[arg(long)]
    stamp_wall_clock: bool,

    /// Emit one directed replica-scope decision for every active non-self seat
    /// pair at each one-second roster refresh.
    ///
    /// Eight active seats produce 3,360 lines/minute, so this diagnostic is
    /// opt-in and changes neither the island roster nor replicated traffic.
    #[arg(long)]
    replica_scope_capture: bool,

    /// Structural self-check for CI images with no time to run a swarm.
    #[arg(long)]
    self_test: bool,

    /// Host an external peer slot (#385): bind a real iroh endpoint, wait for
    /// the dial, and run the swarm with the remote as an island member.
    ///
    /// Without token admission the external slot keeps the deterministic test
    /// identity. With `--issuer-key`, the signed transport identity is used.
    /// The run paces itself in real time — see the runner — so `--seconds`
    /// here measures wall-clock seconds.
    #[arg(long)]
    external_peer: bool,

    /// Number of stable human seats offered during the campaign. The full seat
    /// namespace is `peers + external_slots`; any unbound human seat may join.
    #[arg(long, default_value_t = 1)]
    external_slots: usize,

    /// Initial cohort-formation seconds after the first authenticated arrival.
    /// Admission remains open after the run starts; a full cohort starts now.
    #[arg(long, default_value_t = 90)]
    lobby_seconds: u64,

    /// Seconds to wait for the external peer's dial before giving up
    /// (`--external-peer` only).
    #[arg(long, default_value_t = 60)]
    join_timeout_secs: u64,

    /// Write the exterior listening address to this file, one line:
    /// `<node id hex> <ip:port>` (`--external-peer` only). Lets a launcher or
    /// test hand the runner its dial target without parsing streams.
    #[arg(long)]
    listening_file: Option<String>,

    /// Atomically publish the attempt id and currently bound human seats.
    /// The co-located admission service reads this host-authored liveness fact;
    /// every bind and release replaces it through a temporary file + rename.
    #[arg(long, value_name = "PATH")]
    active_seats_file: Option<std::path::PathBuf>,

    /// Bind the exterior endpoint to this exact socket (`--external-peer`
    /// only). Omit it to retain iroh's wildcard, ephemeral-port default.
    #[arg(long, value_name = "IP:PORT")]
    external_bind: Option<std::net::SocketAddr>,

    /// Refuse a join whose client build is not exactly this revision
    /// (#345 §8's version pinning; `--external-peer` only). The refusal
    /// reason tells the volunteer to download the current build.
    #[arg(long)]
    require_client_rev: Option<String>,

    /// Refuse a join that does not present exactly this pre-minted invite
    /// session id (UUIDv7, minted by `orrery-invite mint`;
    /// `--external-peer` only).
    #[arg(long)]
    require_session: Option<String>,

    /// Trusted issuer key as `<key_id>:<public key hex>`, from
    /// `orrery-invite session-token`'s output (`--external-peer` only).
    /// When set, a join must carry a session token that verifies under this
    /// key for the dialler's own transport identity.
    #[arg(long, value_name = "KEYID:PUBKEY")]
    issuer_key: Option<String>,

    /// Admission's authoritative `slots.json` on storage visible to this
    /// host (`--external-peer` only). Required with a token-gated standing
    /// host (issuer configured, no single `--require-session`).
    #[arg(long, value_name = "PATH")]
    reservation_journal: Option<std::path::PathBuf>,

    /// Supervisor-owned attempt generation used to reject stale reservation
    /// rows. Required whenever `--reservation-journal` is set.
    #[arg(long)]
    attempt_id: Option<String>,

    // ── The external runner's own mode ──────────────────────────────────────
    /// Run as the external peer process instead of hosting a swarm (#385).
    ///
    /// Takes the same `--peers/--seconds/--seed/--witness` values the host was
    /// given: both sides must derive the same island. Impairment is not set
    /// here — it lives in the *host's* router, where every other packet goes.
    #[arg(long)]
    external: bool,

    /// Host node id (hex), required with `--external`.
    #[arg(long)]
    host_node: Option<String>,

    /// Host direct socket address, for proofs without a relay. Optional; the
    /// first bound socket is used when omitted.
    #[arg(long)]
    host_direct: Option<String>,

    /// Admission-reserved human seat (`--external` only). Defaults to the
    /// legacy single exterior seat immediately after the bots.
    #[arg(long)]
    slot: Option<usize>,

    /// Admission session id (`--external` only).
    #[arg(long)]
    session_id: Option<String>,

    /// Hex-encoded `SessionTokenV1` (`--external` only).
    #[arg(long)]
    session_token: Option<String>,
}

/// Parse `--cheat <tamper>[:count]`.
///
/// The names are the ones `orrery_games` prints, shortened at the first hyphen:
/// a flag value and a `Tamper::name` that drift apart is how a gate ends up
/// fielding a different cheat than its comment says it does.
fn parse_cheat(spec: &str) -> Result<CheatSpec> {
    let (name, count) = match spec.split_once(':') {
        Some((name, count)) => (
            name,
            count
                .parse::<usize>()
                .with_context(|| format!("{count:?} is not a peer count"))?,
        ),
        None => (spec, 1),
    };
    let tamper = Tamper::ALL
        .iter()
        .copied()
        .find(|tamper| tamper.name() == name || tamper.name().starts_with(&format!("{name}-")))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown tamper {name:?}; known: {}",
                Tamper::ALL
                    .iter()
                    .map(|tamper| tamper.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    if count == 0 {
        bail!("a cheat count of zero fields no modified client at all");
    }
    Ok(CheatSpec { tamper, count })
}

/// Parse `--issuer-key <key_id>:<public key hex>` into a trusted issuer entry.
fn parse_issuer_key(spec: &str) -> Result<orrery_protocol::IssuerKey> {
    let (key_id, public) = spec
        .split_once(':')
        .context("expected <key_id>:<public key hex>")?;
    let key_id = key_id
        .parse::<u32>()
        .with_context(|| format!("{key_id:?} is not an issuer key id"))?;
    let public = NodeId::from_str(public).context("issuer public key is not hex")?;
    Ok(orrery_protocol::IssuerKey::new(
        orrery_protocol::IssuerKeyId::new(key_id),
        public,
    ))
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        bail!("session token hex has odd length");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).context("session token is not ASCII hex")?;
            u8::from_str_radix(text, 16).with_context(|| "session token is not hex")
        })
        .collect()
}

fn cell_edge_m_for_session(external: bool, external_peer: bool) -> f32 {
    if external || external_peer {
        bot::campaign_cell_edge_m()
    } else {
        bot::default_cell_edge_m()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConnectedSeat {
    slot: usize,
    node: NodeId,
}

/// Build one current roster snapshot and personalize its witness recipients.
///
/// `connected` is the set bound at this instant. This function never compacts
/// or fills a gap in the configured seat namespace; later calls may bind a
/// previously absent seat or omit one that has departed.
fn build_start_manifests(
    attempt_id: &str,
    seed: u64,
    seconds: u64,
    bot_seats: usize,
    island_seats: usize,
    connected: &[ConnectedSeat],
    tick: u64,
) -> Result<BTreeMap<usize, exterior::StartManifest>> {
    if bot_seats > island_seats {
        bail!("bot seats exceed the configured island seat namespace");
    }
    let island_seats = u16::try_from(island_seats).context("island seat count exceeds u16")?;
    let mut occupied = BTreeSet::new();
    let mut active = (0..bot_seats)
        .map(|slot| exterior::ActiveSeat {
            slot,
            node: bot::bot_key(slot).public().to_string(),
            entity: u64::try_from(slot).unwrap_or(u64::MAX).saturating_add(1),
        })
        .collect::<Vec<_>>();
    occupied.extend(0..bot_seats);

    // Load-bearing membership stage: every admitted human is copied into the
    // active roster snapshot. The mutation proof removes one entry here and
    // `start_manifest_names_all_three_connected_humans` must fail.
    for seat in connected {
        if seat.slot < bot_seats || seat.slot >= usize::from(island_seats) {
            bail!(
                "connected human seat {} is outside the human seat range",
                seat.slot
            );
        }
        if !occupied.insert(seat.slot) {
            bail!("seat {} was connected more than once", seat.slot);
        }
        active.push(exterior::ActiveSeat {
            slot: seat.slot,
            node: seat.node.to_string(),
            entity: u64::try_from(seat.slot)
                .context("seat does not fit the entity namespace")?
                .checked_add(1)
                .context("seat entity overflow")?,
        });
    }
    active.sort_by_key(|seat| seat.slot);
    let active_slots = active.iter().map(|seat| seat.slot).collect::<Vec<_>>();
    let duration_ticks = seconds
        .checked_mul(bot::TICK_HZ)
        .context("attempt duration overflows ticks")?;

    connected
        .iter()
        .map(|seat| {
            let manifest = exterior::StartManifest {
                attempt_id: attempt_id.to_owned(),
                seed,
                tick,
                island_seats,
                active: active.clone(),
                witness_recipients: swarm::witness_recipients(&active_slots, seat.slot),
                duration_ticks,
            };
            Ok((seat.slot, manifest))
        })
        .collect()
}

/// The wall clock in whole seconds, which is admission's clock too: both sides
/// of the reservation contract — `slots.json` and `active-seats.json` — are
/// stamped in Unix seconds by co-located processes on one box.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn decode_join_anchor(
    anchor: Option<exterior::AnchorFrame>,
) -> Result<Option<(orrery_protocol::StateClaim, RegolithState)>> {
    anchor
        .map(|bytes| {
            let claim = serde_json::from_slice(&bytes.claim_json)
                .context("external anchor claim did not decode")?;
            let state = RegolithState::decode(&bytes.state)
                .context("external anchor state did not decode")?;
            Ok((claim, state))
        })
        .transpose()
}

/// What the live-join path learned when it asked to hold a seat.
///
/// Named rather than an `Option`, because the two refusals are different
/// facts and only one of them used to exist. #1053: a seat that is free in an
/// attempt whose run is over is not a seat worth handing out, and answering
/// "yes, held" for it is what let a joiner adopt a `StartV1` for a dead run.
#[derive(Debug)]
enum LiveJoinHold {
    /// The window is open and this seat is now held for the joiner.
    Held {
        attempt_id: String,
        tick: u64,
        connected: Vec<ConnectedSeat>,
    },
    /// Another connection already holds or has bound this seat.
    SeatTaken,
    /// The attempt is not inside its run window. Nothing was held.
    OutsideWindow { reason: String },
}

/// Hold the seat a live joiner asked for, or say it is already taken.
///
/// The hold is published, so admission counts the seat taken for as long as
/// this host is holding a connection for it rather than for the arrival lease
/// alone (#1016). Every way out of the handshake that is not a bind therefore
/// has to give the hold back — see [`abandon_live_join`].
fn reserve_live_join(
    live: &mut swarm::LiveMembership,
    slot: usize,
    now_s: u64,
) -> Result<LiveJoinHold> {
    // The window is read under the same lock that takes the hold, so a run
    // that ends between the two cannot leave a seat held for a joiner the
    // next line would have refused.
    if !live.window.admits(now_s) {
        return Ok(LiveJoinHold::OutsideWindow {
            reason: live.window.refusal(now_s),
        });
    }
    if !live
        .hold_pending(slot)
        .context("publish the held seat for a live join")?
    {
        return Ok(LiveJoinHold::SeatTaken);
    }
    Ok(LiveJoinHold::Held {
        attempt_id: live.attempt_id.clone(),
        tick: live.tick,
        connected: live
            .active
            .iter()
            .map(|(slot, binding)| ConnectedSeat {
                slot: *slot,
                node: binding.node,
            })
            .collect(),
    })
}

/// Give back a seat held for a live join that will never bind.
///
/// The caller is already returning the reason the join failed, and that reason
/// is the one worth propagating: a republication that also fails is an
/// operator's line, not a replacement diagnosis. What must not happen is the
/// hold outliving the handshake — admission would keep the seat for a
/// connection that is gone.
fn abandon_live_join(membership: &Arc<Mutex<swarm::LiveMembership>>, slot: usize) {
    if let Err(error) = membership
        .lock()
        .expect("membership lock")
        .drop_pending(slot)
    {
        eprintln!(
            "gates/p1-swarm: seat {slot} could not be given back after a failed live join: \
             {error:#}"
        );
    }
}

/// Seat one lobby arrival, or say its slot is already taken.
///
/// Two readers have to agree about a lobby seat, and this is where they are
/// made to: `pending`, which is what `StartV1` will be authored from, and the
/// published feed, which is what admission offers seats out of. Before #1016
/// only the first of them knew about a seat waiting in the lobby, so admission
/// re-offered it once its arrival lease ran out and the host then refused the
/// second dialler with `reservation_slot_occupied` — correctly, and for a
/// reason neither volunteer could act on.
fn seat_lobby_arrival<P: LobbyPeer>(
    pending: &[P],
    slot: usize,
    membership: Option<&Arc<Mutex<swarm::LiveMembership>>>,
) -> Result<bool> {
    if pending.iter().any(|peer| peer.index() == slot) {
        return Ok(false);
    }
    let Some(live) = membership else {
        return Ok(true);
    };
    let held = live
        .lock()
        .expect("membership lock")
        .hold_pending(slot)
        .context("publish the held seat for a lobby arrival")?;
    Ok(held)
}

/// Bind one seat, and stop calling its session released if it was.
///
/// The second half is what makes a reissued reservation (#1001) safe: while a
/// session is named released, admission counts its seat free and will offer it
/// to the next volunteer. A peer that redialled inside its grace and bound the
/// seat again is *in* it, so the feed must stop saying otherwise in the same
/// publication that names the binding.
fn record_live_binding(
    live: &mut swarm::LiveMembership,
    slot: usize,
    node: NodeId,
    session_id: String,
) -> Result<()> {
    live.pending.remove(&slot);
    let reclaimed = live.rebind_released(&session_id);
    live.active.insert(
        slot,
        swarm::LiveSeatBinding {
            node,
            session_id: session_id.clone(),
        },
    );
    if let Err(error) = live.publish() {
        live.active.remove(&slot);
        if let Some(reclaim) = reclaimed {
            live.released_sessions.insert(session_id, reclaim);
        }
        return Err(error).context("republish active seats after bind");
    }
    Ok(())
}

/// One completed start handshake: the host end of the link, the peer's join
/// anchor, and the transport identity and seat it bound.
type JoinedSeat = (
    exterior::HostLink,
    Option<exterior::AnchorFrame>,
    NodeId,
    usize,
);

/// The frozen-lobby shape [`finish_start_joins`] needs from one prepared peer.
///
/// [`bridge::PendingJoin`] is the production implementor. The trait exists so
/// the tests can inject a peer whose `finish` fails: the live failure is a
/// ten-second `EXTERIOR_MAX_IDLE_TIMEOUT` expiring on a lobby connection that
/// went quiet minutes earlier (#994), and waiting that out in a test would buy
/// a slow flaky test rather than coverage.
trait StartJoin: Sized {
    /// Admission-authoritative seat this peer holds.
    fn index(&self) -> usize;
    /// QUIC-authenticated transport identity.
    fn remote(&self) -> NodeId;
    /// Admission session whose reservation authorized the connection.
    fn session_id(&self) -> Option<&str>;
    /// Send this peer its `StartV1` and arm its pumps.
    fn finish(
        self,
        manifest: Option<exterior::StartManifest>,
        wants_anchor: bool,
    ) -> impl std::future::Future<Output = Result<JoinedSeat>>;
}

impl StartJoin for bridge::PendingJoin {
    fn index(&self) -> usize {
        Self::index(self)
    }

    fn remote(&self) -> NodeId {
        Self::remote(self)
    }

    fn session_id(&self) -> Option<&str> {
        Self::session_id(self)
    }

    async fn finish(
        self,
        manifest: Option<exterior::StartManifest>,
        wants_anchor: bool,
    ) -> Result<JoinedSeat> {
        Self::finish(self, manifest, wants_anchor).await
    }
}

/// What a seat is told when the host gives it back mid-lobby, in the words the
/// volunteer reads off their own screen.
///
/// It has to answer one question — retry, or not? — for somebody who is not
/// reading a stack trace, and it has to be short: the wire carries this reason
/// behind a `u8` length.
///
/// Since #1001's reissue window the answer is "retry, right now, from this
/// machine": admission hands the same reservation back to the same transport
/// identity for its grace, so rejoining is a click and not a new invite. The
/// install is named because the identity *is* the client's durable transport
/// key — the same install is the same volunteer, and a fresh one is a stranger.
const LOBBY_LOST_CONTACT: &str = "the host lost contact while the run was filling; \
     your seat is held for you for a short while — rejoin this campaign now, from \
     this same install, and you get it back";

/// The lobby-sweep shape needed from one peer that is still waiting.
///
/// [`bridge::PendingJoin`] is the production implementor. As with
/// [`StartJoin`], the trait exists so the tests can inject a peer whose
/// heartbeat fails: the live failure is a connection that lapsed minutes
/// before anyone looked at it (#994), and waiting one out would buy a slow
/// flaky test rather than coverage.
trait LobbyPeer: Sized {
    /// Admission-authoritative seat this peer holds.
    fn index(&self) -> usize;
    /// Admission session whose reservation authorized the connection.
    fn session_id(&self) -> Option<&str>;
    /// Tell the peer the lobby is still filling and it is still in it.
    fn lobby_heartbeat(
        &mut self,
        seated: u16,
        needed: u16,
    ) -> impl std::future::Future<Output = Result<()>>;
    /// Give the seat back, naming the reason to the peer on the way out.
    fn evict(self, reason: &str) -> impl std::future::Future<Output = ()>;
}

impl LobbyPeer for bridge::PendingJoin {
    fn index(&self) -> usize {
        Self::index(self)
    }

    fn session_id(&self) -> Option<&str> {
        Self::session_id(self)
    }

    async fn lobby_heartbeat(&mut self, seated: u16, needed: u16) -> Result<()> {
        Self::lobby_heartbeat(self, seated, needed).await
    }

    async fn evict(self, reason: &str) {
        Self::evict(self, reason).await;
    }
}

/// Beat once on every seat still waiting for the lobby to fill, and give back
/// the ones that no longer answer (#994).
///
/// The heartbeat is the news and the probe at once. A peer that hears it knows
/// it is queued; a peer that stops hearing it knows it is not. And the write
/// itself is what tells the *host* that a connection has lapsed — before this,
/// nothing looked at a lobby connection between the handshake and `StartV1`,
/// so a peer that died at 12:54 was discovered at 12:57, when discovering it
/// was most expensive.
///
/// A lost seat is released through the one release path
/// ([`swarm::LiveMembership::release_seat`]) so admission reopens it, exactly
/// as a start-path drop is. Then, and only then, the peer is told: the notice
/// is best effort by construction, because the usual reason a heartbeat failed
/// is that nothing can be written to that peer any more. The client's own
/// heartbeat grace is what makes the outcome intelligible when the notice
/// cannot land.
///
/// A seat lost here is released as [`SeatReclaim::LostAt`] `now_s`: the seat
/// frees, and admission may hand the same reservation back to the same
/// transport identity until its own grace runs out (#1001). `now_s` is the
/// wall-clock second of this sweep, injected for the same reason the trait
/// above is — a test must be able to name the instant without waiting one out.
///
/// Returns how many seats were given back.
async fn sweep_lobby<P: LobbyPeer>(
    pending: &mut Vec<P>,
    wanted: usize,
    membership: Option<&Arc<Mutex<swarm::LiveMembership>>>,
    now_s: u64,
) -> Result<usize> {
    let seated = u16::try_from(pending.len()).unwrap_or(u16::MAX);
    let needed = u16::try_from(wanted).unwrap_or(u16::MAX);
    let mut lost = Vec::new();
    for (position, peer) in pending.iter_mut().enumerate() {
        if let Err(error) = peer.lobby_heartbeat(seated, needed).await {
            eprintln!(
                "gates/p1-swarm: seat {} lost in the lobby: {error:#}",
                peer.index()
            );
            lost.push(position);
        }
    }
    // Back to front: removing a low position first would shift every higher
    // one out from under the index that named it.
    for position in lost.iter().rev().copied() {
        let peer = pending.remove(position);
        let slot = peer.index();
        let session_id = peer.session_id().map(ToOwned::to_owned);
        if let (Some(live), Some(session_id)) = (membership, &session_id) {
            let mut live = live.lock().expect("membership lock");
            live.release_seat(
                slot,
                session_id,
                SeatReclaim::LostAt {
                    released_at_s: now_s,
                },
            )
            .context("republish active seats after a lobby eviction")?;
        }
        peer.evict(LOBBY_LOST_CONTACT).await;
    }
    Ok(lost.len())
}

/// Everything the start path needs to author a `StartV1` for a given roster.
struct StartRoster<'a> {
    attempt_id: Option<&'a str>,
    seed: u64,
    seconds: u64,
    bot_seats: usize,
    island_seats: usize,
    witnessing: bool,
}

impl StartRoster<'_> {
    /// The per-seat `StartV1` set for exactly these connected humans, or
    /// `None` when this run is not attempt-bound and sends no manifest at all.
    fn manifests(
        &self,
        connected: &[ConnectedSeat],
    ) -> Result<Option<BTreeMap<usize, exterior::StartManifest>>> {
        self.attempt_id
            .map(|attempt_id| {
                build_start_manifests(
                    attempt_id,
                    self.seed,
                    self.seconds,
                    self.bot_seats,
                    self.island_seats,
                    connected,
                    0,
                )
            })
            .transpose()
    }
}

/// Bind every prepared seat, dropping the ones that cannot finish rather than
/// taking the attempt down with them (#994).
///
/// A lobby peer may sit for minutes waiting for the run to fill while
/// `EXTERIOR_MAX_IDLE_TIMEOUT` is ten seconds, so a stale connection is an
/// expected outcome here, not an exceptional one. The old loop propagated the
/// first `finish` error out of `main`: one dead peer dropped every healthy
/// player, exited the process, and measured nothing for anyone.
///
/// Three things follow from dropping a seat instead:
///
/// * **The seat is released**, through the one release path
///   ([`swarm::LiveMembership::release_seat`]), so admission reopens it and the
///   published feed never names a seat the transport did not admit (#954).
/// * **The roster is re-authored.** Peers still to be bound receive a `StartV1`
///   without the dropped seat; peers already bound are sent the corrected
///   membership on the Meta lane, the same wire shape the live path uses. A
///   survivor would otherwise spend the whole run broadcasting to — and
///   expecting witness coverage from — a seat nobody is in.
/// * **The failure is named.** `seat N dropped at start: <reason>` is what an
///   operator reads back to the volunteer whose session ended.
///
/// The viability rule is "at least one seat bound". The criterion judges the
/// peers that *attached* — every clause in `Swarm::judge` iterates
/// `self.external` — so a run with fewer humans is measured by exactly the same
/// rules, and a seat that never attached is neither a pass nor a failure, it is
/// absent. What the criterion cannot express is a run with no external peer at
/// all: it would bank an attempt while measuring nothing about human play,
/// which is the shape #375 exists to refuse. So that case, and only that case,
/// fails hard and lets the supervisor open a fresh lobby.
async fn finish_start_joins<J: StartJoin>(
    pending: Vec<J>,
    roster: &StartRoster<'_>,
    membership: Option<&Arc<Mutex<swarm::LiveMembership>>>,
    now_s: u64,
) -> Result<Vec<(JoinedSeat, Option<String>)>> {
    let prepared_seats = pending.len();
    let mut connected = pending
        .iter()
        .map(|join| ConnectedSeat {
            slot: join.index(),
            node: join.remote(),
        })
        .collect::<Vec<_>>();
    let mut manifests = roster.manifests(&connected)?;
    let mut finished: Vec<(JoinedSeat, Option<String>)> = Vec::with_capacity(prepared_seats);
    let mut dropped = 0usize;

    for prepared in pending {
        let slot = prepared.index();
        let session_id = prepared.session_id().map(ToOwned::to_owned);
        let manifest = manifests
            .as_ref()
            .and_then(|by_slot| by_slot.get(&slot))
            .cloned();
        let joined = match prepared.finish(manifest, roster.witnessing).await {
            Ok(joined) => joined,
            Err(error) => {
                eprintln!("gates/p1-swarm: seat {slot} dropped at start: {error:#}");
                dropped += 1;
                connected.retain(|seat| seat.slot != slot);
                manifests = roster.manifests(&connected)?;
                if let (Some(live), Some(session_id)) = (membership, &session_id) {
                    let mut live = live.lock().expect("membership lock");
                    live.release_seat(
                        slot,
                        session_id,
                        SeatReclaim::LostAt {
                            released_at_s: now_s,
                        },
                    )
                    .context("republish active seats after a start-path drop")?;
                }
                continue;
            }
        };
        if let (Some(live), Some(session_id)) = (membership, &session_id) {
            let mut live = live.lock().expect("membership lock");
            record_live_binding(&mut live, joined.3, joined.2, session_id.clone())
                .context("republish active seats after initial bind")?;
        }
        finished.push((joined, session_id));
    }

    if finished.is_empty() {
        bail!(
            "no seat completed its start handshake: all {prepared_seats} prepared peers were \
             dropped, so the attempt would measure no human play at all"
        );
    }
    if dropped > 0 {
        eprintln!(
            "gates/p1-swarm: StartV1 continues with {} of {prepared_seats} prepared seats \
             ({dropped} dropped)",
            finished.len()
        );
        republish_start_roster(&finished, manifests.as_ref()).await;
    }
    Ok(finished)
}

/// Re-send the corrected `StartV1` to seats bound before a peer was dropped.
///
/// Same lane and same JSON as `Swarm::publish_live_manifests_for`; the client
/// adopts it through the live-membership path it already has. Only the seats
/// bound *before* the first drop hold a stale roster, but re-sending to all of
/// them is idempotent and cheaper than tracking which.
async fn republish_start_roster(
    finished: &[(JoinedSeat, Option<String>)],
    manifests: Option<&BTreeMap<usize, exterior::StartManifest>>,
) {
    let Some(by_slot) = manifests else {
        return;
    };
    for ((link, _, _, slot), _) in finished {
        let Some(manifest) = by_slot.get(slot) else {
            continue;
        };
        let frame = exterior::Frame {
            peer: u32::MAX,
            lane: exterior::Lane::Meta,
            payload: bytes::Bytes::from(serde_json::to_vec(manifest).expect("StartV1 serializes")),
        };
        if link.downlink.send(frame).await.is_err() {
            eprintln!(
                "gates/p1-swarm: seat {slot} could not be sent the corrected start roster; its \
                 downlink is already gone"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn accept_live_join(
    prepared: bridge::PendingJoin,
    membership: &Arc<Mutex<swarm::LiveMembership>>,
    joined_tx: &mpsc::Sender<swarm::JoinedExternal>,
    seed: u64,
    seconds: u64,
    bot_seats: usize,
    island_seats: usize,
    witnessing: bool,
) -> Result<()> {
    let slot = prepared.index();
    let node = prepared.remote();
    let Some(session_id) = prepared.session_id().map(ToOwned::to_owned) else {
        let _ = prepared
            .refuse("reservation_missing_session: no invite session id was presented".to_owned())
            .await;
        return Ok(());
    };
    if slot < bot_seats || slot >= island_seats {
        let _ = prepared
            .refuse(format!(
                "reservation_slot_out_of_range: requested slot {slot}, human seats are {bot_seats}..{island_seats}"
            ))
            .await;
        return Ok(());
    }
    let hold = {
        let mut live = membership.lock().expect("membership lock");
        reserve_live_join(&mut live, slot, unix_seconds())?
    };
    let (attempt_id, tick, mut connected) = match hold {
        LiveJoinHold::Held {
            attempt_id,
            tick,
            connected,
        } => (attempt_id, tick, connected),
        LiveJoinHold::SeatTaken => {
            let _ = prepared
                .refuse(format!(
                    "reservation_slot_occupied: slot {slot} is already bound"
                ))
                .await;
            return Ok(());
        }
        // The refusal is the whole point of #1053: the joiner is told by name
        // that there is nothing to join, instead of adopting a `StartV1` for
        // a finished attempt and banking the second before its downlink dies.
        LiveJoinHold::OutsideWindow { reason } => {
            eprintln!("gates/p1-swarm: refused live join into seat {slot}: {reason}");
            let _ = prepared.refuse(reason).await;
            return Ok(());
        }
    };
    connected.push(ConnectedSeat { slot, node });
    connected.sort_by_key(|seat| seat.slot);
    let manifest = build_start_manifests(
        &attempt_id,
        seed,
        seconds,
        bot_seats,
        island_seats,
        &connected,
        tick,
    )
    .and_then(|mut manifests| {
        manifests
            .remove(&slot)
            .context("live join manifest did not include its subject")
    });
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(error) => {
            abandon_live_join(membership, slot);
            return Err(error);
        }
    };
    let finished = prepared.finish(Some(manifest), witnessing).await;
    let (link, anchor, joined_node, joined_slot) = match finished {
        Ok(joined) => joined,
        Err(error) => {
            abandon_live_join(membership, slot);
            return Err(error);
        }
    };
    let anchor = match decode_join_anchor(anchor) {
        Ok(anchor) => anchor,
        Err(error) => {
            abandon_live_join(membership, slot);
            return Err(error);
        }
    };
    {
        let mut live = membership.lock().expect("membership lock");
        record_live_binding(&mut live, joined_slot, joined_node, session_id.clone())?;
    }
    joined_tx
        .send(swarm::JoinedExternal {
            slot: joined_slot,
            node: joined_node,
            session_id,
            anchor,
            link,
        })
        .map_err(|_| anyhow::anyhow!("swarm stopped before the joined seat was installed"))
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.self_test {
        return self_test();
    }

    let cheats = args
        .cheat
        .as_deref()
        .map(parse_cheat)
        .transpose()
        .context("--cheat")?;

    let late_join_tick = args.late_join_at.map(|second| second * bot::TICK_HZ);
    let upload_budget_bits = args
        .budget_kbps
        .unwrap_or(BUDGET_BITS / 1_000)
        .checked_mul(1_000)
        .context("--budget-kbps overflows bits/s")?;
    if upload_budget_bits == 0 {
        bail!("--budget-kbps must be greater than zero");
    }
    if args
        .cell_edge_m
        .is_some_and(|edge| !edge.is_finite() || edge <= 0.0)
    {
        bail!("--cell-edge-m must be finite and greater than zero");
    }

    let config = SwarmConfig {
        peers: args.peers,
        seconds: args.seconds,
        cell_edge_m: args
            .cell_edge_m
            .unwrap_or_else(|| cell_edge_m_for_session(args.external, args.external_peer)),
        send_hz: 20,
        keyframe_every_sends: args.keyframe_every_sends.max(1),
        upload_budget_bits,
        swept_interest_margin: args.swept_interest_margin,
        delivery_gap_instrumentation: args.delivery_gaps,
        impairment: if args.impaired {
            args.loss
                .map_or_else(Impairment::p4_profile, Impairment::p4_profile_at_loss)
        } else {
            Impairment::default()
        },
        seed: args.seed,
        campaign: args.external_peer,
        late_join_tick,
        // `--cheat` implies `--witness`: a modified client in a swarm with no
        // witness is a peer nobody is re-executing, and every conviction clause
        // would fail for want of a detector rather than for want of a
        // conviction.
        witnessing: args.witness || cheats.is_some(),
        varied_profiles: match args.profile_mode {
            ProfileMode::Auto => None,
            ProfileMode::Cruise => Some(false),
            ProfileMode::Varied => Some(true),
        },
        cheats,
        // Implied by `--cheat` for the same reason `--witness` is: a modified
        // client every witness is forbidden to file against is detected and
        // never convicted.
        enforcing: args.enforce || cheats.is_some(),
        started_at_unix_secs: args.stamp_wall_clock.then(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs())
        }),
        replica_scope_capture: args.replica_scope_capture,
        delta_stats: args.delta_stats,
        shot_interest_stats: args.shot_interest_stats,
        presence_stats: args.presence_stats,
    };

    eprintln!(
        "gates/p1-swarm: {} peers, {} simulated seconds ({} ticks), link {}",
        config.peers,
        config.seconds,
        config.seconds * bot::TICK_HZ,
        if config.impairment.is_clean() {
            "clean".to_owned()
        } else {
            format!(
                "{:.0}% loss, {} tick jitter",
                config.impairment.loss * 100.0,
                config.impairment.jitter_ticks
            )
        }
    );

    // The external runner replaces everything below the config build: it is
    // one Bot against a socket, not a swarm host.
    if args.external {
        let host_node = args
            .host_node
            .as_deref()
            .context("--external needs --host-node")?;
        let node = NodeId::from_str(host_node).context("host node id is not hex")?;
        let direct = match &args.host_direct {
            Some(socket) => Some(
                socket
                    .parse()
                    .context("host direct address is not ip:port")?,
            ),
            None => None,
        };
        let run = peer_runner::ExternalRun {
            peers: config.peers,
            slot: args.slot.unwrap_or(config.peers),
            island_seats: config
                .peers
                .checked_add(args.external_slots)
                .context("island seat count overflow")?,
            seconds: config.seconds,
            seed: config.seed,
            witnessing: config.witnessing,
            session_id: args.session_id.clone(),
            session_token: args.session_token.as_deref().map(decode_hex).transpose()?,
            host: bridge::HostAddress {
                node,
                direct: Vec::new(),
            },
            direct,
        };
        return peer_runner::run(&run);
    }

    let configured_island_seats = config
        .peers
        .checked_add(args.external_slots)
        .context("island seat count overflow")?;
    let mut swarm = if args.external_peer {
        Swarm::new_for_island(config, configured_island_seats)
    } else {
        Swarm::new(config)
    }
    // The id the operator named this generation by, carried into the report so
    // a derived ledger row has an attempt to bind to. Before this it reached
    // the start manifest, the active-seats file and the reservation journal and
    // stopped there, which is why no human hour could ever be assembled (#960).
    .with_attempt_id(args.attempt_id.clone());
    let _endpoint_guard;
    let _runtime_guard;
    // Kept past the run so the window can be closed the instant `run()`
    // returns. The live accept loop outlives the swarm -- it is spawned on a
    // runtime held for the whole of `main` -- so without this the report tail
    // is a window in which every joiner is handed a `StartV1` for a run that
    // has already ended (#1053).
    let mut live_membership: Option<Arc<Mutex<swarm::LiveMembership>>> = None;
    if args.external_peer {
        // The host endpoint's identity is the hosting process's, not the
        // slot's: the slot key belongs to the dialler and is what accept()
        // verifies against.
        let secret = bot::host_key();
        let admission = exterior::Admission {
            require_client_rev: args.require_client_rev.clone(),
            require_session: args.require_session.clone(),
            issuer: args
                .issuer_key
                .as_deref()
                .map(parse_issuer_key)
                .transpose()
                .context("--issuer-key")?,
            reservation_journal: match (&args.reservation_journal, &args.attempt_id) {
                (Some(path), Some(attempt_id)) => Some(exterior::ReservationJournal {
                    path: path.clone(),
                    attempt_id: attempt_id.clone(),
                }),
                (None, None) => None,
                _ => bail!("--reservation-journal and --attempt-id must be supplied together"),
            },
        };
        if admission.issuer.is_some()
            && admission.require_session.is_none()
            && admission.reservation_journal.is_none()
        {
            bail!(
                "a standing token-gated host requires --reservation-journal and --attempt-id; refusing to run with unverified seats"
            );
        }
        let standing = admission.reservation_journal.is_some();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("tokio runtime")?;
        let island_seats = configured_island_seats;
        if args.external_slots == 0 {
            bail!("--external-peer requires at least one --external-slot");
        }
        // Admission retains the pre-lobby eight-bot/one-human shape only for
        // campaigns without an explicit `humans` key. New multi-human
        // campaigns obey #563's eight-seat cap.
        if args.external_slots > 1 && island_seats > 8 {
            bail!("the P1 campaign lobby is capped at eight total seats");
        }
        let (endpoint, joined, live_joins) = rt.block_on(async {
            let endpoint = bridge::bind(secret, args.external_bind).await?;
            eprintln!(
                "gates/p1-swarm: exterior seats {}..{} listening, node {}, direct {:?}",
                config.peers,
                island_seats,
                endpoint.id(),
                endpoint.bound_sockets(),
            );
            if let Some(path) = &args.listening_file {
                let line = format!(
                    "{} {}\n",
                    endpoint.id(),
                    endpoint
                        .bound_sockets()
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                );
                std::fs::write(path, line).context("cannot write the exterior listening file")?;
            }
            // Hoisted above the lobby so a seat lost while the lobby is still
            // filling can be released through the one release path (#994).
            // Constructing it publishes nothing; only a release or a bind does.
            let membership = args.attempt_id.as_ref().map(|attempt_id| {
                Arc::new(Mutex::new(swarm::LiveMembership {
                    attempt_id: attempt_id.clone(),
                    active: BTreeMap::new(),
                    pending: BTreeSet::new(),
                    released_sessions: BTreeMap::new(),
                    tick: 0,
                    window: swarm::AttemptWindow::Forming,
                    path: args.active_seats_file.clone(),
                }))
            });
            let mut pending = Vec::new();
            let fixed_legacy_seat =
                (!standing).then(|| (config.peers, bot::bot_key(config.peers).public()));
            // One acceptor for the whole lobby phase. It keeps
            // `endpoint.accept()` polled while handshakes run, so a peer that
            // connects and then says nothing costs only its own connection
            // instead of every later dial (#1144). What comes out of it is
            // already authenticated; the seat bookkeeping below stays serial.
            let mut acceptor =
                bridge::JoinAcceptor::new(endpoint.clone(), fixed_legacy_seat, admission.clone());
            // A standing empty host waits indefinitely (#592). The initial
            // cohort delay begins only after the first authenticated arrival.
            loop {
                let arrival = if standing {
                    acceptor.next().await
                } else {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(args.join_timeout_secs.max(1)),
                        acceptor.next(),
                    )
                    .await
                    {
                        Ok(arrival) => arrival,
                        Err(_) => bail!("the lobby closed without an admitted human"),
                    }
                };
                let prepared = match arrival {
                    Some(Ok(prepared)) => prepared,
                    Some(Err(error)) => {
                        eprintln!("gates/p1-swarm: refused pending join: {error:#}");
                        continue;
                    }
                    None => bail!("the exterior endpoint closed before any human was admitted"),
                };
                if prepared.index() < config.peers || prepared.index() >= island_seats {
                    let reason =
                        format!(
                        "reservation_slot_out_of_range: requested slot {}, human seats are {}..{}",
                        prepared.index(), config.peers, island_seats
                    );
                    let _ = prepared.refuse(reason).await;
                    continue;
                }
                if !seat_lobby_arrival(&pending, prepared.index(), membership.as_ref())? {
                    let reason = format!(
                        "reservation_slot_occupied: slot {} is already bound",
                        prepared.index()
                    );
                    let _ = prepared.refuse(reason).await;
                    continue;
                }
                eprintln!(
                    "gates/p1-swarm: reservation seat {} connected as {}",
                    prepared.index(),
                    prepared.remote()
                );
                pending.push(prepared);
                break;
            }
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(args.lobby_seconds);
            // The seats already in `pending` wait here for as long as the
            // lobby takes to fill — minutes, live — so this loop is also the
            // only place that can keep them alive and watched (#994). The
            // `select!` used to hold a bare `host_prepare` future across
            // ticks, because dropping it every two seconds would have
            // abandoned whichever handshake was in flight. The acceptor owns
            // the handshakes now, and `next()` is a cancel-safe channel read,
            // so the heartbeat arm abandons nothing (#1144).
            let mut heartbeat = tokio::time::interval(bridge::LOBBY_HEARTBEAT_INTERVAL);
            heartbeat.tick().await;
            while pending.len() < args.external_slots {
                let mut arrived = None;
                let mut lobby_closed = false;
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(deadline) => lobby_closed = true,
                    _ = heartbeat.tick() => {}
                    result = acceptor.next() => arrived = Some(result),
                }
                if lobby_closed {
                    break;
                }
                let Some(result) = arrived else {
                    sweep_lobby(
                        &mut pending,
                        args.external_slots,
                        membership.as_ref(),
                        unix_seconds(),
                    )
                    .await?;
                    continue;
                };
                let prepared = match result {
                    Some(Ok(prepared)) => prepared,
                    Some(Err(error)) => {
                        eprintln!("gates/p1-swarm: refused pending join: {error:#}");
                        continue;
                    }
                    None => {
                        eprintln!(
                            "gates/p1-swarm: the exterior endpoint closed while the lobby filled"
                        );
                        break;
                    }
                };
                if prepared.index() < config.peers || prepared.index() >= island_seats {
                    let reason = format!(
                        "reservation_slot_out_of_range: requested slot {}, human seats are {}..{}",
                        prepared.index(),
                        config.peers,
                        island_seats
                    );
                    let _ = prepared.refuse(reason).await;
                    continue;
                }
                if !seat_lobby_arrival(&pending, prepared.index(), membership.as_ref())? {
                    let reason = format!(
                        "reservation_slot_occupied: slot {} is already bound",
                        prepared.index()
                    );
                    let _ = prepared.refuse(reason).await;
                    continue;
                }
                eprintln!(
                    "gates/p1-swarm: reservation seat {} connected as {}",
                    prepared.index(),
                    prepared.remote()
                );
                pending.push(prepared);
            }

            // The acceptor accepts on a clone of the endpoint the run is about
            // to take ownership of, and the live loop opens its own; the lobby
            // is closed, so stop this one before it competes.
            drop(acceptor);

            if pending.is_empty() {
                bail!(
                    "every seat that reached this lobby was lost before StartV1; there is no \
                     human left to measure, so the supervisor opens a fresh lobby"
                );
            }
            eprintln!(
                "gates/p1-swarm: StartV1 begins with {} active humans across {} seats",
                pending.len(),
                island_seats
            );

            let finished = finish_start_joins(
                pending,
                &StartRoster {
                    attempt_id: args.attempt_id.as_deref(),
                    seed: config.seed,
                    seconds: config.seconds,
                    bot_seats: config.peers,
                    island_seats,
                    witnessing: config.witnessing,
                },
                membership.as_ref(),
                unix_seconds(),
            )
            .await?;
            let live_joins = if let Some(membership) = membership {
                {
                    let mut live = membership.lock().expect("membership lock");
                    // The run's wall-clock life starts here, at the freeze,
                    // and this is the only place that knows it. Everything
                    // downstream -- the live accept loop's refusal, the
                    // supervisor's `window_closed` read -- hangs off it.
                    live.window = swarm::AttemptWindow::opened_at(unix_seconds(), config.seconds);
                    live.publish()
                        .context("publish the running membership boundary")?;
                }
                let (joined_tx, joined_rx) = mpsc::channel();
                let accept_endpoint = endpoint.clone();
                let accept_admission = admission.clone();
                let accept_membership = Arc::clone(&membership);
                tokio::spawn(async move {
                    // The rejoin door a disconnected tester comes back
                    // through. Same acceptor, same reason: a silent dialler
                    // here used to make every later rejoin's dial fail as if
                    // the host had gone (#1144).
                    let mut acceptor =
                        bridge::JoinAcceptor::new(accept_endpoint, None, accept_admission);
                    while let Some(arrival) = acceptor.next().await {
                        let prepared = match arrival {
                            Ok(prepared) => prepared,
                            Err(error) => {
                                eprintln!("gates/p1-swarm: refused live join: {error:#}");
                                continue;
                            }
                        };
                        if let Err(error) = accept_live_join(
                            prepared,
                            &accept_membership,
                            &joined_tx,
                            config.seed,
                            config.seconds,
                            config.peers,
                            island_seats,
                            config.witnessing,
                        )
                        .await
                        {
                            eprintln!("gates/p1-swarm: refused live join: {error:#}");
                        }
                    }
                    eprintln!(
                        "gates/p1-swarm: the exterior endpoint closed; live rejoins are over"
                    );
                });
                Some((joined_rx, membership))
            } else {
                None
            };
            anyhow::Ok((endpoint, finished, live_joins))
        })?;
        // Held for their lifetime: dropping the endpoint closes the
        // connection, and dropping the runtime waits on the pump tasks.
        _endpoint_guard = endpoint;
        _runtime_guard = rt;

        for ((link, anchor_bytes, joined_node, slot), session_id) in joined {
            let anchor = decode_join_anchor(anchor_bytes)?;
            swarm = if let Some(session_id) = session_id {
                swarm.with_external_session_at(
                    slot,
                    island_seats,
                    joined_node,
                    session_id,
                    anchor,
                    link,
                )
            } else {
                swarm.with_external_at(slot, island_seats, joined_node, anchor, link)
            };
        }
        if let Some((receiver, membership)) = live_joins {
            live_membership = Some(Arc::clone(&membership));
            swarm = swarm.with_live_joins(receiver, membership, island_seats);
        }
    }

    let report = swarm.run();

    // The attempt is over the moment the swarm stops, whatever else this
    // process still has to write. Closing the window here is what turns a
    // late joiner from a silent one-second measurement into a named refusal,
    // and republishing it is what tells the supervisor to stop advertising a
    // lobby for a generation that has finished (#1053).
    if let Some(membership) = &live_membership {
        let mut live = membership.lock().expect("membership lock");
        live.window = swarm::AttemptWindow::Closed;
        if let Err(error) = live.publish() {
            eprintln!(
                "gates/p1-swarm: could not publish the closed attempt window: {error:#}; \
                 admission may keep advertising this finished attempt until the child exits"
            );
        }
    }

    // Mutation guard for A20 lane 1: parsing a pressure value without replacing
    // every bot's real resource must fail by name, not merely produce a
    // suspiciously flat curve an operator has to notice.
    if report.meter_budget_bits != upload_budget_bits {
        bail!(
            "pressure override did not reach every UploadBudget meter: requested {} bits/s, meters report {}",
            upload_budget_bits,
            report.meter_budget_bits,
        );
    }

    // Printed as well as serialized: a nightly log is often all that survives a
    // failed job, and a figure that cannot be traced to a seed and a commit is
    // not evidence.
    eprintln!(
        "gates/p1-swarm: {} v{}, scenarios {}, seed {}, target {}, commit {}, witness {}",
        report.game,
        report.ruleset_version,
        report.scenarios.join(","),
        report.identity.seed,
        report.identity.target,
        report.identity.commit,
        if report.witnessing { "on" } else { "off" },
    );

    eprintln!(
        "gates/p1-swarm: worst peak upload {} kbps (meter {} kbps; criterion {} kbps), worst p99 {} kbps",
        report.worst_peak_upload_bits / 1_000,
        report.meter_budget_bits / 1_000,
        BUDGET_BITS / 1_000,
        report.worst_p99_upload_bits / 1_000,
    );
    eprintln!(
        "gates/p1-swarm: mean lane cost per peer: replication {} kbps, witness {} kbps, control {} kbps; \
         witness share target {} kbps ({:.0}% of all bytes sent); totals replication {} kB, witness {} kB, control {} kB",
        report.replication_bits_per_sec / 1_000,
        report.witness_lane_bits_per_sec / 1_000,
        report.control_bits_per_sec / 1_000,
        BUDGET_BITS * orrery_witness::plugin::WITNESS_LANE_SHARE_PCT / 100 / 1_000,
        report.witness_lane_share * 100.0,
        report.replication_bytes / 1_000,
        report.witness_bytes / 1_000,
        report.control_bytes / 1_000,
    );
    eprintln!(
        "gates/p1-swarm: replication wire {} keyframes / {} deltas, keyframes {:.1}% of messages and {:.1}% of bytes; {} deltas_unanchored ({} no anchor, {} missing newer, {} superseded, {} invalid)",
        report.keyframe_messages,
        report.delta_messages,
        report.keyframe_message_share * 100.0,
        report.keyframe_byte_share * 100.0,
        report.deltas_unanchored,
        report.deltas_without_any_keyframe,
        report.deltas_missing_newer_keyframe,
        report.deltas_with_superseded_keyframe,
        report.deltas_with_invalid_reference,
    );
    if let Some(presence) = &report.presence_stats {
        eprintln!(
            "gates/p1-swarm: presence stats: {} entities, {} joiner-cluster bins; stranded anchors mean {:.3}% (max {:.3}%)",
            presence.entities.len(),
            presence.joiner_cluster_size_distribution.len(),
            presence.stranded_anchor_fraction.mean_fraction * 100.0,
            presence.stranded_anchor_fraction.max_fraction * 100.0,
        );
    }
    eprintln!(
        "gates/p1-swarm: simulated hitches discarded {} keyframes / {} deltas after sender accounting",
        report.keyframes_discarded_while_stalled,
        report.deltas_discarded_while_stalled,
    );
    eprintln!(
        "gates/p1-swarm: least-travelled peer visited {} cells; {} packets shed ({} keyframes, {} deltas, {} other replication); link carried {} delivered / {} dropped",
        report.min_cells_visited,
        report.total_shed,
        report.shed_keyframes,
        report.shed_deltas,
        report.shed_replication_other,
        report.link.delivered,
        report.link.dropped,
    );
    // The reliable lane, printed even at zero, for the same reason the fault
    // counters below are: all three of these were incremented on every run
    // since they landed and read by nothing outside `router.rs`'s own unit
    // tests (#1133). The head-of-line tax is the costly one — it is excluded
    // from the drain horizon by design (`Router::max_delivery_delay_ticks`),
    // so it is the single number that explains a failed "the link drains"
    // clause, and it was being thrown away at the very last tick of the runs
    // that needed it.
    eprintln!(
        "gates/p1-swarm: stream lane — {} messages delivered, {} retransmissions, {} ticks of \
         head-of-line blocking on shared streams",
        report.link.stream_delivered,
        report.link.stream_retransmits,
        report.link.stream_head_of_line_ticks,
    );
    // The fault counters, printed even at zero. A counter that is incremented
    // but never surfaced is worse than no counter, because it creates the
    // impression the condition is monitored — which is how `no_session` hid a
    // whole-attempt replication failure behind a clean report (#954). One line
    // per run is the whole cost of the zeros; the non-zero lines below name
    // what each counter exists to catch.
    eprintln!(
        "gates/p1-swarm: fault counters — no_session {}, oversized {}, untagged {}, bad_body {}, unsheddable_over_budget {}, misaddressed {}",
        report.total_no_session_sends,
        report.total_oversized_sends,
        report.total_untagged_inbound,
        report.total_bad_body,
        report.total_unsheddable_over_budget,
        report.link.misaddressed,
    );
    if report.total_no_session_sends > 0 {
        let seats = report
            .per_peer
            .iter()
            .filter(|peer| peer.no_session_sends > 0)
            .map(|peer| format!("{}:{}", peer.index, peer.no_session_sends))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "gates/p1-swarm: no_session > 0 — {} send(s) dropped because the addressed peer had no \
             session (seat:count {seats}); a rostered peer without a link loses every packet built \
             for it, keyframes included, while every host-side signal stays clean (#953, #954)",
            report.total_no_session_sends,
        );
    }
    if report.total_oversized_sends > 0 {
        eprintln!(
            "gates/p1-swarm: oversized > 0 — {} send(s) refused over a lane's size limit; a caller \
             exceeded a budget it was sized against, which is a defect at the call site (#954)",
            report.total_oversized_sends,
        );
    }
    if report.total_untagged_inbound > 0 {
        eprintln!(
            "gates/p1-swarm: untagged > 0 — {} inbound packet(s) carried no channel tag; a sender's \
             framing has drifted from this crate's (#954)",
            report.total_untagged_inbound,
        );
    }
    if report.total_bad_body > 0 {
        eprintln!(
            "gates/p1-swarm: bad_body > 0 — {} state packet(s) decoded at the envelope but not as \
             state; the sender and this receiver disagree about the rules (#954)",
            report.total_bad_body,
        );
    }
    if report.total_unsheddable_over_budget > 0 {
        eprintln!(
            "gates/p1-swarm: unsheddable_over_budget > 0 — {} control/witness/hit packet(s) sent \
             while over budget; the overrun was real, not an artefact of shedding (docs/03 §9.3, #954)",
            report.total_unsheddable_over_budget,
        );
    }
    if report.link.misaddressed > 0 {
        eprintln!(
            "gates/p1-swarm: misaddressed > 0 — {} packet(s) addressed to a peer the router does not \
             know; a roster names a seat the transport never admitted (#954)",
            report.link.misaddressed,
        );
    }
    if let Some(margin) = &report.interest_margin {
        eprintln!(
            "gates/p1-swarm: swept interest margin installed {:.2} cells mean ({} min, {} max) across {} peer-refresh samples",
            margin.mean_cells, margin.min_cells, margin.max_cells, margin.samples,
        );
    }
    if let Some(gaps) = &report.delivery_gaps {
        eprintln!(
            "gates/p1-swarm: delivery gaps across {} entity-links / {} completed intervals: p50 {} ticks, p95 {}, p99 {}, max {} including trailing silence",
            gaps.pairs.len(),
            gaps.completed_gaps,
            gaps.p50_gap_ticks,
            gaps.p95_gap_ticks,
            gaps.p99_gap_ticks,
            gaps.max_gap_ticks,
        );
    }
    eprintln!(
        "gates/p1-swarm: {} same-cell returns, {} proxy pops out of {} churn events",
        report.total_boundary_flips, report.total_proxy_pops, report.total_interest_churn,
    );
    let seat_distribution = report
        .per_peer
        .iter()
        .map(|peer| {
            format!(
                "{}/{}={}",
                peer.index, peer.profile, peer.max_boundary_returns_in_window
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let histogram = report
        .boundary_return_histogram
        .iter()
        .map(|bin| format!("{}:{}", bin.returns_in_window, bin.seats))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "gates/p1-swarm: boundary returns per 1 s refresh window: max {}; seats [{seat_distribution}]; histogram returns:seats [{histogram}]",
        report.max_boundary_returns_in_window,
    );
    for profile in &report.boundary_return_profiles {
        let histogram = profile
            .histogram
            .iter()
            .map(|bin| format!("{}:{}", bin.returns_in_window, bin.seats))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "gates/p1-swarm: boundary returns profile {}: max {}; histogram returns:seats [{histogram}]",
            profile.profile, profile.max_returns_in_window,
        );
    }
    if let Some(shots) = &report.shot_interest_stats {
        eprintln!(
            "gates/p1-swarm: shot interest — {} of {} resolved shots out of interest ({:.3}%); \
             {} of {} against a slower victim ({:.3}%); {} scope unknown",
            shots.attacker_out_of_interest,
            shots.resolved_shots,
            shots.out_of_interest_rate * 100.0,
            shots.out_of_interest_against_slower_victim,
            shots.resolved_against_slower_victim,
            shots.slower_victim_out_of_interest_rate * 100.0,
            shots.scope_unknown,
        );
    }
    if report.witnessing {
        eprintln!(
            "gates/p1-swarm: witness ran over {:.0} player-hours: {} chain gaps repaired, {} false positives",
            report.player_hours, report.total_gaps, report.total_false_positives,
        );
        // Printed beside the false-positive count, never apart from it: the one
        // is only readable against the other.
        eprintln!(
            "gates/p1-swarm: witness judged {:.1}% of the timeline it was shown ({} of {} ticks, \
             {} abandoned across {} re-anchors); {} frames deferred, {} judgements deferred",
            report.observation_coverage * 100.0,
            report.total_judged_ticks,
            report.total_shown_ticks,
            report.total_unjudged_ticks,
            report.total_reanchors,
            report.total_frames_deferred,
            report.total_judgements_deferred,
        );
        // The two ways a witness loses timeline, printed apart because they are
        // not the same failure. A watch that never anchored loses its subject's
        // *whole* run and asks for nothing while it does; a deferral loses at
        // most the frames a repair did not overtake. Reading the deficit
        // against the wrong one is how it stayed unattributed.
        eprintln!(
            "gates/p1-swarm: {} watches never folded a frame ({} frames refused, {} of them by a \
             watch with no anchor); each is its subject's whole timeline shown and none judged",
            report.total_watches_unanchored,
            report.total_frames_rejected,
            report.total_frames_rejected_unanchored,
        );
        // Printed immediately under the coverage line, because it is what the
        // coverage line's denominator now contains (#1130). Until this figure
        // existed a dark watch moved neither term of that ratio, so a report
        // could say 100% over a population that was mostly not observed and
        // nothing anywhere in it disagreed.
        eprintln!(
            "gates/p1-swarm: {} of {} armed watches were shown nothing at all, each charged its \
             subject's whole run in the coverage above",
            report.total_watches_dark, report.total_watches_armed,
        );
        // Coverage is one minus what is in flight through repair, so the line
        // above is only half a finding without this one: it says how much was
        // missed, and this says where it went. A deficit that cannot be spent
        // against these seven columns is a deficit with an unnamed cause.
        eprintln!(
            "gates/p1-swarm: of {} deferred frames — {} recovered, {} stale, {} overflowed, \
             {} pruned, {} dropped in drain, {} replaced, {} still held; ledger {}",
            report.total_frames_deferred,
            report.total_frames_recovered,
            report.total_deferrals_stale,
            report.total_deferrals_overflowed,
            report.total_deferrals_pruned,
            report.total_deferrals_dropped_in_drain,
            report.total_deferrals_replaced,
            report.total_deferrals_held,
            if report.deferral_ledger_balances {
                "balances"
            } else {
                "DOES NOT BALANCE"
            },
        );
    }
    if let Some(conviction) = &report.conviction {
        eprintln!(
            "gates/p1-swarm: cheat {} on {} peer(s): {} diverged, {} convicted on replay; \
             worst detection {} ticks (window {})",
            conviction.tamper,
            conviction.tampered_peers,
            conviction.tampered_peers_that_diverged,
            conviction.tampered_peers_convicted,
            conviction
                .worst_detection_ticks
                .map_or_else(|| "n/a".to_owned(), |ticks| ticks.to_string()),
            orrery_protocol::MAX_ADJUDICATION_TICKS,
        );
        // The two report counts side by side, for the same reason coverage is
        // printed beside the false-positive count: a pipeline that convicts by
        // accusing everybody produces a fine number in the first column.
        eprintln!(
            "gates/p1-swarm: {} reports filed against the modified peer(s), {} against honest peers; \
             {} adjudicated — {} confirms, {} exonerates, {} evidence-forged, {} unadjudicable",
            conviction.reports_against_tampered,
            conviction.reports_against_honest,
            conviction.adjudicated,
            conviction.confirms,
            conviction.exonerates,
            conviction.evidence_forged,
            conviction.unadjudicable,
        );
    }
    if report.witnessing {
        eprintln!(
            "gates/p1-swarm: escalations — {} filed, {} shadowed, {} unservable, {} unidentified",
            report.total_escalations_filed,
            report.total_escalations_shadowed,
            report.total_escalations_unservable,
            report.total_escalations_unidentified,
        );
    }
    if let Some(join) = &report.late_join {
        eprintln!(
            "gates/p1-swarm: fresh late joiner started with {} replicas, then tracked {} of {} roster peers, {} of which were in its neighbourhood",
            join.initial_replicas, join.tracked, join.roster, join.in_neighbourhood,
        );
    }

    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
        eprintln!("gates/p1-swarm: report written to {path}");
    }

    let failures = report.against_criterion(Criterion {
        budget_bits: BUDGET_BITS,
        min_cells: args.min_cells,
        max_pops: args.max_pops,
        max_shed: args.max_shed,
        max_unsheddable_over_budget: args.max_unsheddable_over_budget,
        ..Criterion::default()
    });
    if failures.is_empty() {
        eprintln!("gates/p1-swarm: every clause of the P1 criterion holds");
        return Ok(());
    }

    for failure in &failures {
        eprintln!(
            "gates/p1-swarm: FAILED [{}] — {}",
            failure.clause, failure.detail
        );
    }
    if args.report_only {
        eprintln!("gates/p1-swarm: --report-only, exiting zero anyway");
        return Ok(());
    }
    bail!(
        "{} clause(s) of the P1 criterion did not hold",
        failures.len()
    );
}

/// Structural self-check: the harness still proves what it claims to.
///
/// Deliberately about *coverage of the criterion*, not about the numbers — it
/// catches a harness that has been reduced to a smoke test without pretending
/// to run a swarm on an image that has no time for one.
fn self_test() -> Result<()> {
    let source = include_str!("swarm.rs");
    for clause in [
        "sustained upload ≤ 1 Mbps",
        "no load shed to stay within budget",
        // §9.3's normative overrun signal. It was measured and printed for
        // four issues before #974 wired it to a failure; the clause above,
        // which is a harness-local convention rather than a restatement of the
        // document, was doing the whole job alone.
        "no unsheddable overrun beyond island formation",
        "the harness observes what it sends",
        "no false-positive discrepancy signal against an honest peer",
        "the witness sees the stream it is judging",
        // The third witnessing clause, and the one the other two are unreadable
        // without: a witness that has stopped watching also accuses nobody.
        "the witness keeps watching for the whole run",
        // P4's demo criterion, the conviction half. The first is the
        // anti-vacuity guard: `Tamper::SpeedMultiplier` is inert on an
        // interceptor slot at this roam's requested acceleration, so without it
        // the three that follow would hold over byte-identical state.
        "the modified client actually diverges from the shipping rules",
        "a modified client is convicted on replay",
        "no report is filed against an honest peer",
        "a modified client is convicted within one adjudication window",
        "an unmodified swarm files no report at all",
        "every witness can sign what it raises",
        "the link drains",
        // #1132: the one clause that reads a *human* seat's interest
        // crossings. Bots emitted them from the day the flag landed; the seat
        // the flag was bought for emitted none.
        "the swept interest margin fires for a human seat",
        "the late joiner starts with no retained replicas",
        "the late-join check is not vacuous",
        "interest churn absorbed without visible proxy pops",
        "no entity thrashes cells at a boundary",
        "roaming across ≥64 interest cells",
        "a late joiner receives only its 27-cell neighborhood",
    ] {
        // Matched at the push site — `clause: "…"` — rather than anywhere in
        // the file. The unit tests in that module name every clause they
        // exercise, so a bare substring search would be satisfied by the test
        // that was meant to be protecting it, and deleting the clause itself
        // would go unnoticed. This form also asserts the stronger thing: the
        // string is wired to a failure, not merely mentioned.
        if !source.contains(&format!("clause: {clause:?}")) {
            bail!("self-test: criterion clause absent: {clause}");
        }
    }
    let bot = include_str!("bot.rs");
    if !bot.contains("OrrerySpatialPlugin") {
        bail!("self-test: the harness no longer runs the real spatial stack");
    }
    if !bot.contains("send_peer_packets") {
        bail!("self-test: the harness no longer runs the real send path");
    }
    // The four wires the conviction half hangs off, each of which fails
    // *silently* if it goes: a swarm on the corpus kernel runs stage 1 against
    // an empty slice, a witness with no identity counts
    // `escalations_unidentified` and files nothing, one left in shadow mode
    // assembles windows and files nothing, and a filed report nobody re-runs is
    // an accusation rather than a verdict. None of them turns a green run red
    // on its own, which is why they are asserted structurally.
    if !bot.contains("Regolith") {
        bail!("self-test: the bots no longer play a ruleset that publishes stage-1 invariants");
    }
    if !bot.contains("regolith::pilot::honest_orders") {
        bail!("self-test: the swarm bypasses the pilot shared with the human client");
    }
    let pilot = include_str!("../../../crates/orrery_games/src/regolith/pilot.rs");
    for scenario in ["Combat", "Mining", "ContestedGrab", "BloomConvergence"] {
        if !pilot.contains(&format!("PilotScenario::{scenario}")) {
            bail!("self-test: Regolith pilot scenario is absent: {scenario}");
        }
    }
    if !bot.contains("WitnessIdentity") {
        bail!("self-test: no witness can sign a report; escalation stops at `unidentified`");
    }
    if !bot.contains("shadow_mode: !enforcing") {
        bail!(
            "self-test: shadow mode is no longer a parameter; a conviction leg would file nothing"
        );
    }
    if !bot.contains("honest_shadow") {
        bail!("self-test: the dual-execution probe is gone; detection latency has no t = 0");
    }
    if !include_str!("adjudicate.rs").contains("verify_bundle") {
        bail!(
            "self-test: filed reports are no longer re-executed; a verdict would be the \
               reporter's own word"
        );
    }
    if !bot.contains("Archetype::Cruiser") {
        bail!(
            "self-test: modified peers are no longer pinned to the low-limit archetype; the \
               speed cheat is inert on the other one"
        );
    }
    eprintln!(
        "gates/p1-swarm: self-test passed — every criterion clause present, real stack wired"
    );
    Ok(())
}

#[cfg(test)]
mod session_geometry_tests {
    use super::*;

    #[test]
    fn exterior_campaigns_use_regoliths_interaction_sized_cells() {
        assert_eq!(
            cell_edge_m_for_session(false, true),
            orrery_games::regolith::CAMPAIGN_CELL_EDGE_M as f32
        );
        assert_eq!(
            cell_edge_m_for_session(true, false),
            orrery_games::regolith::CAMPAIGN_CELL_EDGE_M as f32
        );
        assert_eq!(
            cell_edge_m_for_session(false, false),
            orrery_protocol::DEFAULT_CELL_EDGE_M as f32,
            "the P1 gate continues to exercise the framework default"
        );
    }

    #[test]
    fn start_manifest_names_all_three_connected_humans() {
        // Reservation order is deliberately the opposite of connection order:
        // stable seats, not accept order, determine the frozen roster.
        let connected = [
            ConnectedSeat {
                slot: 7,
                node: bot::bot_key(7).public(),
            },
            ConnectedSeat {
                slot: 5,
                node: bot::bot_key(5).public(),
            },
            ConnectedSeat {
                slot: 6,
                node: bot::bot_key(6).public(),
            },
        ];
        let manifests = build_start_manifests("attempt-1", 17, 20, 5, 8, &connected, 0)
            .expect("valid frozen roster");

        assert_eq!(manifests.keys().copied().collect::<Vec<_>>(), vec![5, 6, 7]);
        for (subject, manifest) in &manifests {
            assert_eq!(manifest.attempt_id, "attempt-1");
            assert_eq!(manifest.seed, 17);
            assert_eq!(manifest.tick, 0);
            assert_eq!(manifest.island_seats, 8);
            assert_eq!(manifest.duration_ticks, 20 * bot::TICK_HZ);
            assert_eq!(
                manifest
                    .active
                    .iter()
                    .map(|seat| seat.slot)
                    .collect::<Vec<_>>(),
                vec![0, 1, 2, 3, 4, 5, 6, 7],
                "StartV1 must name all three humans connected before the freeze"
            );
            assert_eq!(
                manifest
                    .active
                    .iter()
                    .find(|seat| seat.slot == *subject)
                    .map(|seat| seat.node.clone()),
                Some(bot::bot_key(*subject).public().to_string())
            );
        }
    }

    /// #1053. The standing `shakedown` host handed two clients, ninety
    /// seconds apart, a `StartV1` for an attempt whose run window had passed,
    /// then closed their downlink about sixty milliseconds later. Both banked
    /// a valid, signed 0.017-minute row: a worthless measurement that was
    /// indistinguishable from a good one.
    ///
    /// The seat was free, and "free" was the only question the live-join path
    /// ever asked. It now asks whether the attempt is still inside its own
    /// window first, and takes no hold when it is not -- a seat held for a
    /// refused joiner would be an immortal seat.
    #[test]
    fn an_attempt_past_its_window_is_not_adoptable() {
        let ends_at_s = 1_757_000_000;
        let mut membership = swarm::LiveMembership {
            attempt_id: "01a06bb4-f42b-7a9a-93ec-621fcafccb6f".to_owned(),
            active: BTreeMap::new(),
            pending: BTreeSet::new(),
            released_sessions: BTreeMap::new(),
            tick: 18_000,
            window: swarm::AttemptWindow::Running { ends_at_s },
            path: None,
        };

        let inside = reserve_live_join(&mut membership, 5, ends_at_s - 1)
            .expect("holding a seat inside the window");
        assert!(
            matches!(inside, LiveJoinHold::Held { .. }),
            "a joiner inside the run window is still admitted, got {inside:?}"
        );
        membership.drop_pending(5).expect("giving the seat back");

        // One hour thirty-eight minutes past the end, which is what the two
        // clients in #1053 actually arrived at.
        for (label, now_s) in [
            ("on the closing second", ends_at_s),
            ("1h38m past the end", ends_at_s + 5_880),
        ] {
            let refused = reserve_live_join(&mut membership, 5, now_s)
                .expect("the window check itself never fails");
            let LiveJoinHold::OutsideWindow { reason } = &refused else {
                panic!("an attempt {label} must not be adoptable, got {refused:?}");
            };
            assert!(
                reason.contains("attempt_window_closed"),
                "the joiner is refused by name, got {reason}"
            );
            assert!(
                membership.pending.is_empty(),
                "a refused joiner {label} must leave no held seat behind"
            );
        }

        // And once the swarm has stopped, nothing is adoptable at any clock.
        membership.window = swarm::AttemptWindow::Closed;
        assert!(
            matches!(
                reserve_live_join(&mut membership, 5, ends_at_s - 1)
                    .expect("the window check itself never fails"),
                LiveJoinHold::OutsideWindow { .. }
            ),
            "the report-writing tail after run() must admit nobody"
        );
        assert!(membership.pending.is_empty());
    }

    #[test]
    fn a_live_bound_seat_is_recorded_so_the_next_joiner_inherits_it() {
        let mut membership = swarm::LiveMembership {
            attempt_id: "attempt-live".to_owned(),
            active: BTreeMap::new(),
            pending: BTreeSet::new(),
            released_sessions: BTreeMap::new(),
            tick: 240,
            window: swarm::AttemptWindow::Running {
                ends_at_s: u64::MAX,
            },
            path: None,
        };
        record_live_binding(
            &mut membership,
            5,
            bot::bot_key(5).public(),
            "session-five".to_owned(),
        )
        .expect("live seat binding records");

        let LiveJoinHold::Held {
            connected: inherited,
            ..
        } = reserve_live_join(&mut membership, 6, 0).expect("holding the slot publishes")
        else {
            panic!("next live join reserves its slot");
        };
        assert_eq!(
            inherited.iter().map(|seat| seat.slot).collect::<Vec<_>>(),
            vec![5],
            "the next joiner's manifest snapshot must inherit the already live seat"
        );
        assert_eq!(
            membership.active[&5].session_id, "session-five",
            "the active binding retains the release key for the live seat"
        );
    }

    #[test]
    fn a_reclaimed_seat_stops_being_named_released_the_moment_it_binds() {
        // The other half of #1001's reissue window. Admission counts a
        // released session's seat as free once the window passes, and hands it
        // to the next volunteer; a peer that redialled inside its window and
        // bound the seat again would then be flying a seat admission was still
        // offering. The feed has to stop saying released in the very
        // publication that names the binding.
        let mut membership = swarm::LiveMembership {
            attempt_id: "attempt-live".to_owned(),
            active: BTreeMap::new(),
            pending: BTreeSet::new(),
            released_sessions: BTreeMap::new(),
            tick: 240,
            window: swarm::AttemptWindow::Running {
                ends_at_s: u64::MAX,
            },
            path: None,
        };
        membership
            .release_seat(
                5,
                "session-five",
                swarm::SeatReclaim::LostAt {
                    released_at_s: 1_756_900_000,
                },
            )
            .expect("a seat with no feed path releases in memory");
        assert!(
            membership.released_sessions.contains_key("session-five"),
            "the lapse is published so admission can hold the reservation"
        );

        record_live_binding(
            &mut membership,
            5,
            bot::bot_key(5).public(),
            "session-five".to_owned(),
        )
        .expect("the redial binds the same seat");

        assert!(
            membership.released_sessions.is_empty(),
            "a seat the transport is holding must never read as released"
        );
        assert_eq!(membership.active[&5].session_id, "session-five");
    }
}

#[cfg(test)]
mod start_join_tests {
    use super::*;

    /// A prepared peer whose handshake outcome the test chooses.
    ///
    /// The live failure is `EXTERIOR_MAX_IDLE_TIMEOUT` expiring on a lobby
    /// connection that went quiet minutes ago (#994). Reproducing that for real
    /// would cost ten seconds of wall clock per case and still be timing
    /// dependent, so the seam is [`StartJoin`] and the failure is injected.
    struct FakeJoin {
        slot: usize,
        session_id: Option<String>,
        /// `Some` if this peer cannot finish, carrying the transport reason.
        failure: Option<&'static str>,
        /// Lobby beats this peer still answers before its connection is gone.
        /// `None` is a reachable peer, which answers for as long as it is
        /// asked — the case that must survive an arbitrarily long lobby.
        beats_left: Option<usize>,
        /// Every beat any peer answered: slot, and the lobby it was told about.
        beats: Arc<Mutex<Vec<(usize, u16, u16)>>>,
        /// Every eviction notice sent, in the words the peer would read.
        evictions: Arc<Mutex<Vec<(usize, String)>>>,
        /// The `StartV1` each peer was handed at `Accept`, in bind order.
        accepted: Arc<Mutex<Vec<AcceptedSeat>>>,
        /// Remote ends of the bound links, kept alive so the corrected roster
        /// the host republishes is readable rather than dropped on the floor.
        remotes: Arc<Mutex<Vec<(usize, exterior::RemoteLink)>>>,
    }

    impl StartJoin for FakeJoin {
        fn index(&self) -> usize {
            self.slot
        }

        fn remote(&self) -> NodeId {
            bot::bot_key(self.slot).public()
        }

        fn session_id(&self) -> Option<&str> {
            self.session_id.as_deref()
        }

        async fn finish(
            self,
            manifest: Option<exterior::StartManifest>,
            _wants_anchor: bool,
        ) -> Result<JoinedSeat> {
            self.accepted
                .lock()
                .expect("accepted lock")
                .push(AcceptedSeat {
                    slot: self.slot,
                    manifest,
                });
            if let Some(reason) = self.failure {
                bail!("connection lost: {reason}");
            }
            let node = bot::bot_key(self.slot).public();
            let (host, remote) = exterior::link_pair();
            self.remotes
                .lock()
                .expect("remotes lock")
                .push((self.slot, remote));
            Ok((host, None, node, self.slot))
        }
    }

    impl LobbyPeer for FakeJoin {
        fn index(&self) -> usize {
            self.slot
        }

        fn session_id(&self) -> Option<&str> {
            self.session_id.as_deref()
        }

        async fn lobby_heartbeat(&mut self, seated: u16, needed: u16) -> Result<()> {
            if let Some(left) = self.beats_left.as_mut() {
                if *left == 0 {
                    // What the live write returns once the connection has
                    // lapsed: the classified QUIC close, not a stream error.
                    bail!("idle timeout");
                }
                *left -= 1;
            }
            self.beats
                .lock()
                .expect("beats lock")
                .push((self.slot, seated, needed));
            Ok(())
        }

        async fn evict(self, reason: &str) {
            self.evictions
                .lock()
                .expect("evictions lock")
                .push((self.slot, reason.to_owned()));
        }
    }

    /// The wall-clock second the lobby tests release seats at. Fixed, so the
    /// published reissue deadline is an equality and not a window (#1001).
    const SWEPT_AT: u64 = 1_756_900_000;

    /// One `Accept` the fake join recorded: which seat, and the `StartV1` it
    /// was handed.
    ///
    /// Named rather than a bare pair. `clippy::type_complexity` on the two
    /// `Arc<Mutex<Vec<..>>>` fields it lives in was the symptom (#1140); the
    /// cause is that nothing at a use site could say which half of the pair
    /// was the slot.
    struct AcceptedSeat {
        /// The seat that was accepted.
        slot: usize,
        /// The membership manifest it was handed, when it was handed one.
        manifest: Option<exterior::StartManifest>,
    }

    struct StartHarness {
        accepted: Arc<Mutex<Vec<AcceptedSeat>>>,
        remotes: Arc<Mutex<Vec<(usize, exterior::RemoteLink)>>>,
        beats: Arc<Mutex<Vec<(usize, u16, u16)>>>,
        evictions: Arc<Mutex<Vec<(usize, String)>>>,
        membership: Arc<Mutex<swarm::LiveMembership>>,
    }

    impl StartHarness {
        fn new() -> Self {
            Self {
                accepted: Arc::new(Mutex::new(Vec::new())),
                remotes: Arc::new(Mutex::new(Vec::new())),
                beats: Arc::new(Mutex::new(Vec::new())),
                evictions: Arc::new(Mutex::new(Vec::new())),
                membership: Arc::new(Mutex::new(swarm::LiveMembership {
                    attempt_id: "attempt-994".to_owned(),
                    active: BTreeMap::new(),
                    pending: BTreeSet::new(),
                    released_sessions: BTreeMap::new(),
                    tick: 0,
                    window: swarm::AttemptWindow::Forming,
                    // No feed file: `publish` is a no-op and the assertions
                    // read the in-memory bookkeeping the feed is written from.
                    path: None,
                })),
            }
        }

        /// A harness whose membership publishes to a real file, so a test can
        /// read the same feed `scripts/admission.py` reads.
        fn publishing() -> (Self, std::path::PathBuf) {
            let directory = std::env::temp_dir().join(format!(
                "p1-lobby-feed-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("test clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&directory).expect("temp dir");
            let path = directory.join("active-seats.json");
            let harness = Self::new();
            harness
                .membership
                .lock()
                .expect("membership lock")
                .path
                .clone_from(&Some(path.clone()));
            (harness, path)
        }

        /// Slots the published feed names as held-but-unbound, and as bound.
        fn feed(path: &std::path::Path) -> (Vec<usize>, Vec<usize>) {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).expect("feed written"))
                    .expect("feed parses");
            let slots = |key: &str| -> Vec<usize> {
                value[key]
                    .as_array()
                    .expect("the feed names a slot list")
                    .iter()
                    .map(|slot| slot.as_u64().expect("a slot is a number") as usize)
                    .collect()
            };
            (slots("pending_slots"), slots("active_slots"))
        }

        fn peer(&self, slot: usize, failure: Option<&'static str>) -> FakeJoin {
            FakeJoin {
                slot,
                session_id: Some(format!("session-{slot}")),
                failure,
                beats_left: None,
                accepted: Arc::clone(&self.accepted),
                remotes: Arc::clone(&self.remotes),
                beats: Arc::clone(&self.beats),
                evictions: Arc::clone(&self.evictions),
            }
        }

        /// A peer whose connection lapses after `beats_left` more heartbeats.
        fn fading_peer(&self, slot: usize, beats_left: usize) -> FakeJoin {
            FakeJoin {
                beats_left: Some(beats_left),
                ..self.peer(slot, None)
            }
        }

        /// How many beats a given seat was sent.
        fn beats_for(&self, slot: usize) -> usize {
            self.beats
                .lock()
                .expect("beats lock")
                .iter()
                .filter(|(seat, _, _)| *seat == slot)
                .count()
        }

        /// Slots the given peer's `Accept` manifest named as active.
        fn accepted_slots(&self, slot: usize) -> Vec<usize> {
            self.accepted
                .lock()
                .expect("accepted lock")
                .iter()
                .find(|accepted| accepted.slot == slot)
                .and_then(|accepted| accepted.manifest.as_ref())
                .expect("the peer was handed a StartV1")
                .active
                .iter()
                .map(|seat| seat.slot)
                .collect()
        }

        /// Slots named by the last membership frame pushed to a bound seat, or
        /// `None` when the host sent it no correction.
        fn republished_slots(&self, slot: usize) -> Option<Vec<usize>> {
            let remotes = self.remotes.lock().expect("remotes lock");
            let (_, remote) = remotes
                .iter()
                .find(|(seat, _)| *seat == slot)
                .expect("the seat bound");
            let mut downlink = remote.downlink.lock().expect("downlink lock");
            let mut latest = None;
            while let Ok(frame) = downlink.try_recv() {
                assert_eq!(frame.lane, exterior::Lane::Meta);
                let manifest: exterior::StartManifest =
                    serde_json::from_slice(&frame.payload).expect("a StartV1 on the Meta lane");
                latest = Some(manifest.active.iter().map(|seat| seat.slot).collect());
            }
            latest
        }
    }

    fn roster() -> StartRoster<'static> {
        StartRoster {
            attempt_id: Some("attempt-994"),
            seed: 17,
            seconds: 20,
            bot_seats: 5,
            island_seats: 8,
            witnessing: false,
        }
    }

    /// How many sweeps stand in for a lobby wait far past the idle timeout.
    ///
    /// The host beats every `bridge::LOBBY_HEARTBEAT_INTERVAL`, so ninety of
    /// them is three minutes — the wait #994's tester actually sat through,
    /// and eighteen `EXTERIOR_MAX_IDLE_TIMEOUT` windows. It costs microseconds
    /// here because the seam is the beat, not the clock.
    const A_THREE_MINUTE_LOBBY: usize = 90;

    #[tokio::test]
    async fn a_volunteer_who_waits_out_the_arrival_lease_keeps_their_seat_and_starts() {
        // #1016. A lobby runs `lobby_seconds` (180 s) and admission's arrival
        // lease is 45 s, so the ordinary case -- arriving early and waiting for
        // the run to fill -- used to make the seat invisible to admission a
        // quarter of the way in. The bound is a parameter of the sweep here,
        // not a clock: `A_THREE_MINUTE_LOBBY` beats is four arrival leases,
        // and it costs microseconds because the seam is the beat.
        let (harness, path) = StartHarness::publishing();
        let mut lobby = vec![harness.peer(5, None)];
        assert!(
            seat_lobby_arrival(&[] as &[FakeJoin], 5, Some(&harness.membership))
                .expect("the held seat publishes"),
            "the first arrival takes the seat admission reserved for it"
        );
        assert_eq!(
            StartHarness::feed(&path),
            (vec![5], vec![]),
            "the seat is held from the moment the host has a connection for it"
        );

        for _ in 0..A_THREE_MINUTE_LOBBY {
            assert_eq!(
                sweep_lobby(&mut lobby, 2, Some(&harness.membership), SWEPT_AT)
                    .await
                    .expect("the lobby sweep publishes"),
                0,
                "a peer that keeps answering is never given back"
            );
        }
        assert_eq!(
            StartHarness::feed(&path),
            (vec![5], vec![]),
            "four arrival leases into the lobby the seat is still visibly held"
        );

        // The defect's second victim: while the first volunteer holds the
        // seat, the next dialler must be offered a different one by admission
        // rather than sent at this one and refused by the host.
        assert!(
            !seat_lobby_arrival(&lobby, 5, Some(&harness.membership))
                .expect("a refused arrival publishes nothing"),
            "a held seat is not re-offered while its volunteer is sitting in it"
        );

        let finished = finish_start_joins(lobby, &roster(), Some(&harness.membership), SWEPT_AT)
            .await
            .expect("the waiting volunteer starts");
        assert_eq!(finished.len(), 1, "the volunteer who waited is in the run");
        assert_eq!(
            StartHarness::feed(&path),
            (vec![], vec![5]),
            "a bound seat leaves the held set in the same publication that binds it"
        );
    }

    #[tokio::test]
    async fn a_held_seat_whose_volunteer_stops_answering_is_given_back() {
        // The other direction, and the reason the fix is not "hold the seat
        // longer". A seat is held for exactly as long as the host has a
        // connection for it; the moment the heartbeat stops landing the seat
        // goes back to admission, which is what #996 and #1001 exist to keep
        // true. A reservation the host never saw a connection for is never
        // held here at all, and its arrival lease frees it as it always did.
        let (harness, path) = StartHarness::publishing();
        assert!(
            seat_lobby_arrival(&[] as &[FakeJoin], 6, Some(&harness.membership))
                .expect("the held seat publishes")
        );
        let mut lobby = vec![harness.fading_peer(6, 2)];

        let mut lost = 0;
        for _ in 0..A_THREE_MINUTE_LOBBY {
            lost += sweep_lobby(&mut lobby, 2, Some(&harness.membership), SWEPT_AT)
                .await
                .expect("the lobby sweep publishes");
        }
        assert_eq!(lost, 1, "the peer that stopped answering was given back");
        assert!(lobby.is_empty());
        assert_eq!(
            StartHarness::feed(&path),
            (vec![], vec![]),
            "a seat nobody is arriving to is held by nothing at all"
        );
        assert!(
            seat_lobby_arrival(&lobby, 6, Some(&harness.membership))
                .expect("the reopened seat publishes"),
            "the freed seat is available to the next arrival"
        );
    }

    #[tokio::test]
    async fn a_reachable_peer_waits_out_a_long_lobby_and_still_starts() {
        // The defect's healthy half: nothing exercised a peer that waits
        // longer than `EXTERIOR_MAX_IDLE_TIMEOUT`, because before the lobby
        // heartbeat nothing happened on that connection at all.
        let harness = StartHarness::new();
        let mut pending = vec![harness.peer(5, None), harness.peer(6, None)];

        for _ in 0..A_THREE_MINUTE_LOBBY {
            let lost = sweep_lobby(&mut pending, 3, Some(&harness.membership), SWEPT_AT)
                .await
                .expect("a reachable lobby releases no seat");
            assert_eq!(lost, 0);
        }

        assert_eq!(harness.beats_for(5), A_THREE_MINUTE_LOBBY);
        assert_eq!(
            harness.beats.lock().expect("beats lock")[0],
            (5, 2, 3),
            "each beat carries the lobby's progress, which is what the client shows"
        );
        assert!(
            harness.evictions.lock().expect("evictions lock").is_empty(),
            "a reachable player is never evicted for waiting"
        );

        // And it still starts: the wait cost it nothing.
        let finished = finish_start_joins(pending, &roster(), Some(&harness.membership), SWEPT_AT)
            .await
            .expect("both peers survived the lobby");
        assert_eq!(
            finished
                .iter()
                .map(|((_, _, _, slot), _)| *slot)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
    }

    #[tokio::test]
    async fn an_unreachable_lobby_peer_is_evicted_told_why_and_gives_its_seat_back() {
        // Seat 6's connection lapses after two beats; seat 5's does not. The
        // host must learn during the lobby, not at `StartV1` minutes later.
        let harness = StartHarness::new();
        let mut pending = vec![harness.peer(5, None), harness.fading_peer(6, 2)];

        let mut lost = 0;
        for _ in 0..4 {
            lost += sweep_lobby(&mut pending, 3, Some(&harness.membership), SWEPT_AT)
                .await
                .expect("losing one seat is survivable");
        }

        assert_eq!(lost, 1);
        assert_eq!(
            pending.iter().map(LobbyPeer::index).collect::<Vec<_>>(),
            vec![5],
            "only the lost seat leaves the lobby"
        );
        assert_eq!(
            *harness.evictions.lock().expect("evictions lock"),
            vec![(6, LOBBY_LOST_CONTACT.to_owned())],
            "the peer is told why, in words that say whether to retry"
        );
        assert!(
            LOBBY_LOST_CONTACT.contains("rejoin this campaign now"),
            "and the words have to be actionable by somebody who is not reading a log: \
             since #1001 the action is rejoining, not asking for a new invite"
        );
        assert!(
            !LOBBY_LOST_CONTACT.contains("new invite"),
            "telling a volunteer to get a new invite when their reservation is being \
             held for them sends them the long way round"
        );
        assert!(
            LOBBY_LOST_CONTACT.len() <= usize::from(u8::MAX),
            "the reject reason is u8-length-prefixed on the wire"
        );

        let live = harness.membership.lock().expect("membership lock");
        assert_eq!(
            live.released_sessions.keys().cloned().collect::<Vec<_>>(),
            vec!["session-6".to_owned()],
            "the seat goes back through the one release path, so admission reopens it (#954)"
        );
        assert_eq!(
            live.released_sessions.get("session-6").copied(),
            Some(swarm::SeatReclaim::LostAt {
                released_at_s: SWEPT_AT
            }),
            "a lobby lapse is lost, not spent: admission is told when, so it can hand the \
             same reservation back to the same transport identity (#1001)"
        );
        assert!(
            live.active.is_empty() && live.pending.is_empty(),
            "and nothing the transport did not admit is left named"
        );
    }

    #[tokio::test]
    async fn a_lobby_peer_without_a_session_is_still_evicted() {
        // The legacy single-peer path presents no invite session, so there is
        // no reservation to spend. The eviction must still happen.
        let harness = StartHarness::new();
        let mut stale = harness.fading_peer(6, 0);
        stale.session_id = None;
        let mut pending = vec![stale];

        assert_eq!(
            sweep_lobby(&mut pending, 2, Some(&harness.membership), SWEPT_AT)
                .await
                .expect("a session-less peer is evicted the same way"),
            1
        );
        assert!(pending.is_empty());
        assert!(
            harness
                .membership
                .lock()
                .expect("membership lock")
                .released_sessions
                .is_empty(),
            "there was no reservation to spend"
        );
    }

    #[tokio::test]
    async fn a_stale_peer_is_dropped_and_the_attempt_starts_with_the_rest() {
        // Seat 6 fails in the middle of the bind order on purpose: seat 5 is
        // already bound and holding a roster that names 6, and seat 7 has not
        // been handed one yet. Both halves of the correction are live here.
        let harness = StartHarness::new();
        let pending = vec![
            harness.peer(5, None),
            harness.peer(6, Some("timed out")),
            harness.peer(7, None),
        ];

        let finished = finish_start_joins(pending, &roster(), Some(&harness.membership), SWEPT_AT)
            .await
            .expect("one stale peer must not take the attempt down");

        assert_eq!(
            finished
                .iter()
                .map(|((_, _, _, slot), _)| *slot)
                .collect::<Vec<_>>(),
            vec![5, 7],
            "the healthy peers must start"
        );

        let live = harness.membership.lock().expect("membership lock");
        assert_eq!(
            live.active.keys().copied().collect::<Vec<_>>(),
            vec![5, 7],
            "the dropped seat must not be bound"
        );
        assert!(
            live.pending.is_empty(),
            "the dropped seat must not be left reserved"
        );
        assert_eq!(
            live.released_sessions.keys().cloned().collect::<Vec<_>>(),
            vec!["session-6".to_owned()],
            "the dropped seat's session is released, so admission reopens the seat"
        );
        assert_eq!(
            live.released_sessions.get("session-6").copied(),
            Some(swarm::SeatReclaim::LostAt {
                released_at_s: SWEPT_AT
            }),
            "a peer that could not finish its start handshake lapsed the same way a \
             swept lobby peer did, and gets the same reissue window"
        );
        drop(live);

        assert_eq!(
            harness.accepted_slots(7),
            vec![0, 1, 2, 3, 4, 5, 7],
            "a peer bound after the drop is handed a roster without the dropped seat"
        );
        assert_eq!(
            harness.accepted_slots(5),
            vec![0, 1, 2, 3, 4, 5, 6, 7],
            "the peer bound before the drop was handed the pre-drop roster"
        );
        assert_eq!(
            harness.republished_slots(5),
            Some(vec![0, 1, 2, 3, 4, 5, 7]),
            "so it must be sent the corrected membership before the run starts (#954)"
        );
        assert_eq!(
            harness.republished_slots(7),
            Some(vec![0, 1, 2, 3, 4, 5, 7]),
            "the correction goes to every bound seat, idempotently"
        );
    }

    #[tokio::test]
    async fn no_drop_means_no_correction_frame() {
        let harness = StartHarness::new();
        let pending = vec![harness.peer(5, None), harness.peer(6, None)];

        let finished = finish_start_joins(pending, &roster(), Some(&harness.membership), SWEPT_AT)
            .await
            .expect("a clean lobby starts");

        assert_eq!(finished.len(), 2);
        assert_eq!(
            harness.republished_slots(5),
            None,
            "an uncorrected roster is not resent; the Accept manifest already named everyone"
        );
    }

    #[tokio::test]
    async fn losing_every_prepared_peer_fails_the_attempt() {
        // The viability boundary. One of several dropping is survivable; the
        // last one dropping is not, because a run with no external peer banks
        // an attempt while measuring nothing about human play — the shape #375
        // refuses. It still releases the seat, so the supervisor's fresh lobby
        // finds it free.
        let harness = StartHarness::new();
        let pending = vec![
            harness.peer(5, Some("timed out")),
            harness.peer(6, Some("timed out")),
        ];

        let error = finish_start_joins(pending, &roster(), Some(&harness.membership), SWEPT_AT)
            .await
            .expect_err("an attempt with no bound seat is not viable");
        assert!(
            format!("{error:#}").contains("no seat completed its start handshake"),
            "the failure must name itself: {error:#}"
        );

        let live = harness.membership.lock().expect("membership lock");
        assert!(
            live.active.is_empty() && live.pending.is_empty(),
            "no seat may be left named by the feed"
        );
        assert_eq!(
            live.released_sessions.keys().cloned().collect::<Vec<_>>(),
            vec!["session-5".to_owned(), "session-6".to_owned()],
            "both seats are released for the next lobby"
        );
    }

    #[tokio::test]
    async fn a_dropped_seat_without_a_session_still_leaves_the_attempt_running() {
        // The legacy single-peer path presents no invite session id, so there
        // is no reservation to release. The drop must still be survivable.
        let harness = StartHarness::new();
        let mut stale = harness.peer(6, Some("timed out"));
        stale.session_id = None;
        let pending = vec![harness.peer(5, None), stale];

        let finished = finish_start_joins(pending, &roster(), Some(&harness.membership), SWEPT_AT)
            .await
            .expect("a session-less peer dropping must not take the attempt down");

        assert_eq!(finished.len(), 1);
        let live = harness.membership.lock().expect("membership lock");
        assert_eq!(live.active.keys().copied().collect::<Vec<_>>(), vec![5]);
        assert!(
            live.released_sessions.is_empty(),
            "there was no reservation to spend"
        );
    }
}
