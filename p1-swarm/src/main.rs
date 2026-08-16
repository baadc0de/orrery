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
//! # Time
//!
//! Simulated. Each peer's clock advances exactly one 60 Hz tick per frame, so
//! rates are bytes per *simulated* second — which is what the budget is about,
//! since the send cadence is 20 Hz of sim ticks. An hour of play costs what it
//! costs to compute, and a run is reproducible.

mod bot;
mod router;
mod swarm;

use anyhow::{bail, Result};
use clap::Parser;

use router::Impairment;
use swarm::{Swarm, SwarmConfig};

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
            Impairment::p4_profile()
        } else {
            Impairment::default()
        },
        seed: args.seed,
        late_join_tick,
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

    eprintln!(
        "p1-swarm: worst peak upload {} kbps (budget {} kbps), worst p99 {} kbps",
        report.worst_peak_upload_bits / 1_000,
        BUDGET_BITS / 1_000,
        report.worst_p99_upload_bits / 1_000,
    );
    eprintln!(
        "p1-swarm: least-travelled peer visited {} cells; {} packets shed; link carried {} delivered / {} dropped",
        report.min_cells_visited, report.total_shed, report.link.delivered, report.link.dropped,
    );
    eprintln!(
        "p1-swarm: {} boundary flips, {} proxy pops out of {} churn events",
        report.total_boundary_flips, report.total_proxy_pops, report.total_interest_churn,
    );
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

    let failures = report.against_criterion(BUDGET_BITS, args.min_cells, args.max_pops);
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
        "the late-join check is not vacuous",
        "interest churn absorbed without visible proxy pops",
        "no entity thrashes cells at a boundary",
        "roaming across ≥64 interest cells",
        "a late joiner receives only its 27-cell neighborhood",
    ] {
        if !source.contains(clause) {
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
