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
//! **180 kbps**, worst peak upload **921 kbps**, **162 packets shed**, across
//! **32 accumulated player-hours with zero false positives and 100% coverage**
//! — and 172 shed at the 5% end of the band, also at zero and 100%.
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
use swarm::{CheatSpec, Criterion, Swarm, SwarmConfig};

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
    /// The gate's witnessed leg has passed 206, then 230, and now **162** — the
    /// measured number exactly, each time (docs/11-roadmap.md §P4). 206 → 230
    /// when watches stopped dying on their first lost frame, which is more
    /// repair traffic and so more of the cheap lane shed to pay for it. 230 →
    /// 162 when the bots moved from `orrery_conformance`'s corpus kernel to
    /// `orrery_games`' Regolith: drag and a per-archetype speed clamp move every
    /// trajectory in the swarm, and with it the crowd density that decides how
    /// much any peer has to send. 172 at the 5% end of the band. Both are
    /// identical at five simulated minutes and at one hour.
    #[arg(long, default_value_t = 0)]
    max_shed: u64,

    /// Seed for impairment and the universe.
    #[arg(long, default_value_t = 1)]
    seed: u64,

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

    /// Feed #692's swept one-refresh-period interest cells into bot manifests.
    ///
    /// This is intentionally minimal harness wiring. Production host/client
    /// propagation remains a follow-up after this run prices the feature.
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

#[allow(clippy::too_many_arguments)]
async fn accept_live_join(
    endpoint: &iroh::Endpoint,
    admission: &exterior::Admission,
    membership: &Arc<Mutex<swarm::LiveMembership>>,
    joined_tx: &mpsc::Sender<swarm::JoinedExternal>,
    seed: u64,
    seconds: u64,
    bot_seats: usize,
    island_seats: usize,
    witnessing: bool,
) -> Result<()> {
    let prepared = bridge::host_prepare(endpoint, None, admission).await?;
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
    let snapshot = {
        let mut live = membership.lock().expect("membership lock");
        if live.active.contains_key(&slot) || !live.pending.insert(slot) {
            None
        } else {
            Some((
                live.attempt_id.clone(),
                live.tick,
                live.active
                    .iter()
                    .map(|(slot, (node, _session))| ConnectedSeat {
                        slot: *slot,
                        node: *node,
                    })
                    .collect::<Vec<_>>(),
            ))
        }
    };
    let Some((attempt_id, tick, mut connected)) = snapshot else {
        let _ = prepared
            .refuse(format!(
                "reservation_slot_occupied: slot {slot} is already bound"
            ))
            .await;
        return Ok(());
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
            membership
                .lock()
                .expect("membership lock")
                .pending
                .remove(&slot);
            return Err(error);
        }
    };
    let finished = prepared.finish(Some(manifest), witnessing).await;
    let (link, anchor, joined_node, joined_slot) = match finished {
        Ok(joined) => joined,
        Err(error) => {
            membership
                .lock()
                .expect("membership lock")
                .pending
                .remove(&slot);
            return Err(error);
        }
    };
    let anchor = match decode_join_anchor(anchor) {
        Ok(anchor) => anchor,
        Err(error) => {
            membership
                .lock()
                .expect("membership lock")
                .pending
                .remove(&slot);
            return Err(error);
        }
    };
    {
        let mut live = membership.lock().expect("membership lock");
        live.pending.remove(&joined_slot);
        live.active
            .insert(joined_slot, (joined_node, session_id.clone()));
        if let Err(error) = live.publish() {
            live.active.remove(&joined_slot);
            return Err(error).context("republish active seats after bind");
        }
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

    let config = SwarmConfig {
        peers: args.peers,
        seconds: args.seconds,
        cell_edge_m: cell_edge_m_for_session(args.external, args.external_peer),
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
    };
    let _endpoint_guard;
    let _runtime_guard;
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
            let mut pending = Vec::new();
            let fixed_legacy_seat =
                (!standing).then(|| (config.peers, bot::bot_key(config.peers).public()));
            // A standing empty host waits indefinitely (#592). The initial
            // cohort delay begins only after the first authenticated arrival.
            loop {
                let prepared = if standing {
                    match bridge::host_prepare(&endpoint, fixed_legacy_seat, &admission).await {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            eprintln!("gates/p1-swarm: refused pending join: {error:#}");
                            continue;
                        }
                    }
                } else {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(args.join_timeout_secs.max(1)),
                        bridge::host_prepare(&endpoint, fixed_legacy_seat, &admission),
                    )
                    .await
                    {
                        Ok(Ok(prepared)) => prepared,
                        Ok(Err(error)) => {
                            eprintln!("gates/p1-swarm: refused pending join: {error:#}");
                            continue;
                        }
                        Err(_) => bail!("the lobby closed without an admitted human"),
                    }
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
            while pending.len() < args.external_slots {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let prepared = match tokio::time::timeout(
                    remaining,
                    bridge::host_prepare(&endpoint, fixed_legacy_seat, &admission),
                )
                .await
                {
                    Ok(Ok(prepared)) => prepared,
                    Ok(Err(error)) => {
                        eprintln!("gates/p1-swarm: refused pending join: {error:#}");
                        continue;
                    }
                    Err(_) => break,
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
                if pending
                    .iter()
                    .any(|joined: &bridge::PendingJoin| joined.index() == prepared.index())
                {
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

            let connected = pending
                .iter()
                .map(|join| ConnectedSeat {
                    slot: join.index(),
                    node: join.remote(),
                })
                .collect::<Vec<_>>();
            let manifests = args
                .attempt_id
                .as_deref()
                .map(|attempt_id| {
                    build_start_manifests(
                        attempt_id,
                        config.seed,
                        config.seconds,
                        config.peers,
                        island_seats,
                        &connected,
                        0,
                    )
                })
                .transpose()?;
            eprintln!(
                "gates/p1-swarm: StartV1 begins with {} active humans across {} seats",
                pending.len(),
                island_seats
            );

            let mut finished = Vec::with_capacity(pending.len());
            let membership = args.attempt_id.as_ref().map(|attempt_id| {
                Arc::new(Mutex::new(swarm::LiveMembership {
                    attempt_id: attempt_id.clone(),
                    active: BTreeMap::new(),
                    pending: BTreeSet::new(),
                    released_sessions: BTreeSet::new(),
                    tick: 0,
                    running: false,
                    path: args.active_seats_file.clone(),
                }))
            });
            for prepared in pending {
                let session_id = prepared.session_id().map(ToOwned::to_owned);
                let manifest = manifests
                    .as_ref()
                    .and_then(|by_slot| by_slot.get(&prepared.index()))
                    .cloned();
                let joined = prepared.finish(manifest, config.witnessing).await?;
                if let (Some(live), Some(session_id)) = (&membership, &session_id) {
                    let mut live = live.lock().expect("membership lock");
                    live.active.insert(joined.3, (joined.2, session_id.clone()));
                    live.publish()
                        .context("republish active seats after initial bind")?;
                }
                finished.push((joined, session_id));
            }
            let live_joins = if let Some(membership) = membership {
                {
                    let mut live = membership.lock().expect("membership lock");
                    live.running = true;
                    live.publish()
                        .context("publish the running membership boundary")?;
                }
                let (joined_tx, joined_rx) = mpsc::channel();
                let accept_endpoint = endpoint.clone();
                let accept_admission = admission.clone();
                let accept_membership = Arc::clone(&membership);
                tokio::spawn(async move {
                    loop {
                        if let Err(error) = accept_live_join(
                            &accept_endpoint,
                            &accept_admission,
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
            swarm = swarm.with_live_joins(receiver, membership, island_seats);
        }
    }

    let report = swarm.run();

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
}
