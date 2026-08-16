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
//! accounting; `orrery_conformance`'s reference ruleset driving the motion.
//! Every number the report prints is produced by shipping code.
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
//! # Witnessing (`--witness`) is P4's input, not yet P4's criterion
//!
//! With `--witness` every peer streams a real signed log to its witness set and
//! re-executes what it is streamed. Every bot is honest by construction — each
//! logs exactly the inputs it applied — so any signal beyond a chain gap is a
//! false positive, which is what makes the count meaningful without a separate
//! oracle for who was cheating.
//!
//! It works and it found real defects. Both of the reasons it could not
//! accumulate P4's 500 honest player-hours are now closed: the repair budget
//! bounded the traffic that reached 8.7 Mbps against a 1 Mbps allowance, and
//! the cost that grew faster than the peer count is linear again — 32 peers
//! over 60 simulated seconds runs in about ten wall seconds.
//!
//! It holds the criterion at eight, sixteen and thirty-two peers: **zero false
//! positives at 100% observation coverage** on a clean link, and zero at 96%
//! under the 3% loss / 100 ms jitter profile. Both numbers are printed together
//! because neither is readable alone — a witness that has stopped watching also
//! reports zero.
//!
//! Since it holds, it gates: `scripts/p1-swarm-gate.sh` runs the impaired hour
//! with `--witness` as its third leg, nightly and blocking, and that is the only
//! place in the tree the witness pipeline runs at all. Every clause guarded by
//! `SwarmConfig.witnessing` was dead code before it existed.
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
//! Over the criterion's full simulated hour the lane sits at 194 kbps across
//! **32 accumulated player-hours, zero false positives, 100% coverage**.
//!
//! The residual 200 shed packets are replication bytes belonging to the four
//! stalling peers in the densest part of the crowd, and the count is *identical*
//! at five minutes and at one hour — a transient at island formation rather than
//! a sustained overrun. What produces it is the preference order working: a peer
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

mod bot;
mod chain;
mod profile;
mod router;
mod swarm;

use anyhow::{bail, Result};
use clap::Parser;

use router::Impairment;
use swarm::{Criterion, Swarm, SwarmConfig};

/// The D6/D16 peer upload budget.
const BUDGET_BITS: u64 = 1_000_000;

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
    /// The band has only ever been run at its 3% floor. This is how the other
    /// end gets exercised — on demand rather than nightly, because a second
    /// witnessed hour would not fit the job's timeout. Ignored without
    /// `--impaired`, which is the flag that selects the profile at all.
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
    #[arg(long, default_value_t = 0)]
    max_shed: u64,

    /// Seed for impairment and the universe.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Simulated second at which to run the late-join check.
    #[arg(long)]
    late_join_at: Option<u64>,

    /// Write the full report as JSON to this path.
    #[arg(long)]
    json: Option<String>,

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

    /// Stamp the report with the wall-clock second the run started.
    ///
    /// Off by default, and outside the report's identity block when on: a run
    /// is a function of its seed, and a timestamp inside the reproducible body
    /// would make two identical runs compare unequal. Evidence uploads want it;
    /// a developer comparing two seeds does not.
    #[arg(long)]
    stamp_wall_clock: bool,

    /// Structural self-check for CI images with no time to run a swarm.
    #[arg(long)]
    self_test: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.self_test {
        return self_test();
    }

    let late_join_tick = args
        .late_join_at
        .or(Some(args.seconds / 2))
        .map(|second| second * bot::TICK_HZ);

    let config = SwarmConfig {
        peers: args.peers,
        seconds: args.seconds,
        cell_edge_m: bot::default_cell_edge_m(),
        send_hz: 20,
        impairment: if args.impaired {
            args.loss
                .map_or_else(Impairment::p4_profile, Impairment::p4_profile_at_loss)
        } else {
            Impairment::default()
        },
        seed: args.seed,
        late_join_tick,
        witnessing: args.witness,
        started_at_unix_secs: args.stamp_wall_clock.then(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs())
        }),
    };

    eprintln!(
        "p1-swarm: {} peers, {} simulated seconds ({} ticks), link {}",
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

    let report = Swarm::new(config).run();

    // Printed as well as serialized: a nightly log is often all that survives a
    // failed job, and a figure that cannot be traced to a seed and a commit is
    // not evidence.
    eprintln!(
        "p1-swarm: seed {}, target {}, commit {}, witness {}",
        report.identity.seed,
        report.identity.target,
        report.identity.commit,
        if report.witnessing { "on" } else { "off" },
    );

    eprintln!(
        "p1-swarm: worst peak upload {} kbps (budget {} kbps), worst p99 {} kbps",
        report.worst_peak_upload_bits / 1_000,
        BUDGET_BITS / 1_000,
        report.worst_p99_upload_bits / 1_000,
    );
    eprintln!(
        "p1-swarm: witness lane {} kbps per peer against its {} kbps share ({:.0}% of all bytes sent); \
         replication {} kB, witness {} kB, control {} kB",
        report.witness_lane_bits_per_sec / 1_000,
        BUDGET_BITS * orrery_witness::plugin::WITNESS_LANE_SHARE_PCT / 100 / 1_000,
        report.witness_lane_share * 100.0,
        report.replication_bytes / 1_000,
        report.witness_bytes / 1_000,
        report.control_bytes / 1_000,
    );
    eprintln!(
        "p1-swarm: least-travelled peer visited {} cells; {} packets shed; link carried {} delivered / {} dropped",
        report.min_cells_visited, report.total_shed, report.link.delivered, report.link.dropped,
    );
    eprintln!(
        "p1-swarm: {} boundary flips, {} proxy pops out of {} churn events",
        report.total_boundary_flips, report.total_proxy_pops, report.total_interest_churn,
    );
    if report.witnessing {
        eprintln!(
            "p1-swarm: witness ran over {:.0} player-hours: {} chain gaps repaired, {} false positives",
            report.player_hours, report.total_gaps, report.total_false_positives,
        );
        // Printed beside the false-positive count, never apart from it: the one
        // is only readable against the other.
        eprintln!(
            "p1-swarm: witness judged {:.1}% of the timeline it was shown ({} of {} ticks, \
             {} abandoned across {} re-anchors); {} frames deferred, {} judgements deferred",
            report.observation_coverage * 100.0,
            report.total_judged_ticks,
            report.total_shown_ticks,
            report.total_unjudged_ticks,
            report.total_reanchors,
            report
                .per_peer
                .iter()
                .map(|p| p.frames_deferred)
                .sum::<u64>(),
            report
                .per_peer
                .iter()
                .map(|p| p.judgements_deferred)
                .sum::<u64>(),
        );
    }
    if let Some(join) = &report.late_join {
        eprintln!(
            "p1-swarm: late joiner tracked {} of {} roster peers, {} of which were in its neighbourhood",
            join.tracked, join.roster, join.in_neighbourhood,
        );
    }

    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
        eprintln!("p1-swarm: report written to {path}");
    }

    let failures = report.against_criterion(Criterion {
        budget_bits: BUDGET_BITS,
        min_cells: args.min_cells,
        max_pops: args.max_pops,
        max_shed: args.max_shed,
    });
    if failures.is_empty() {
        eprintln!("p1-swarm: every clause of the P1 criterion holds");
        return Ok(());
    }

    for failure in &failures {
        eprintln!("p1-swarm: FAILED [{}] — {}", failure.clause, failure.detail);
    }
    if args.report_only {
        eprintln!("p1-swarm: --report-only, exiting zero anyway");
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
        "the link drains",
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
    if !include_str!("bot.rs").contains("OrrerySpatialPlugin") {
        bail!("self-test: the harness no longer runs the real spatial stack");
    }
    if !include_str!("bot.rs").contains("send_peer_packets") {
        bail!("self-test: the harness no longer runs the real send path");
    }
    eprintln!("p1-swarm: self-test passed — every criterion clause present, real stack wired");
    Ok(())
}
