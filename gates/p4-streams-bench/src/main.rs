//! P4 transport benchmark: what a shared control stream, a stream per message,
//! and the datagram status quo each cost the repair path.
//!
//! # The question
//!
//! `orrery_net`'s control lane now rides QUIC streams. A stream is ordered
//! within itself and independent of every other stream, so *which* stream a
//! message takes decides what can block it. One shared stream is cheap and
//! totally ordered; a stream per message cannot head-of-line block, at the cost
//! of a stream. Neither is obviously right, and the traffic mix is what
//! decides — so this measures the mix rather than arguing about it.
//!
//! # What is real here
//!
//! Real: two `aeronet_iroh` endpoints, two QUIC connections' worth of loss
//! detection, congestion control and — the subject — stream scheduling;
//! `orrery_net`'s send path, channel policy and upload meter. Not real: the
//! game above it, and the wire, which is [`impaired`]'s in-process link with
//! seeded loss, delay and jitter. The peers cannot get around it: they are
//! built with no IP transport at all, so it is the only path there is.
//!
//! # Reading the output
//!
//! Latency is to a *whole* message. A repair that arrives in thirty-four pieces
//! is complete when the last one lands, because that is when a witness can fold
//! it. Completion rate matters as much as latency on the datagram rows: an
//! unreliable lane can post a fine p50 by losing the samples that would have
//! been slow.

mod impaired;
mod link;
mod transport;
mod workload;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bevy_ecs::message::Messages;
use clap::Parser;

use orrery_net::peer_link::{PeerPacket, SendPacket};

use crate::impaired::Impairment;
use crate::transport::{Inbound, Outcome, Receiver, Sender, Transport};
use crate::workload::{Class, Samples};

/// How often the run loop steps. Fine enough to place a 20 Hz send within a
/// couple of milliseconds of where it belongs.
const STEP: Duration = Duration::from_millis(2);

#[derive(Debug, Parser)]
#[command(about = "Measure what each control-lane transport costs under loss")]
struct Args {
    /// Which transports to run. Repeat, or omit for all four.
    #[arg(long, value_enum)]
    transport: Vec<Transport>,
    /// Seconds of traffic per transport.
    #[arg(long, default_value_t = 30)]
    seconds: u64,
    /// Packet loss, 0.0–1.0. P4's criterion is 3%.
    #[arg(long, default_value_t = 0.03)]
    loss: f64,
    /// One-way delay in milliseconds. The round trip is twice this.
    #[arg(long, default_value_t = 20)]
    delay_ms: u64,
    /// Repairs per second. The default very nearly fills the link at 3% loss,
    /// which is a real operating point and a bad one for isolating head-of-line
    /// blocking — at saturation every transport is queueing. Sweep it.
    #[arg(long, default_value_t = workload::REPAIR_HZ)]
    repair_hz: u32,
    /// Seed for the link's loss and jitter, so a run can be repeated.
    #[arg(long, default_value_t = 0xB1A5)]
    seed: u64,
    /// Emit the results as JSON rather than a table.
    #[arg(long)]
    json: bool,
    /// Connect, exchange a little traffic, and report what the link did to it
    /// — without running the measurement.
    #[arg(long)]
    check_link: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let args = Args::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;
    let handle = runtime.handle().clone();

    let impairment = Impairment {
        loss: args.loss,
        delay: Duration::from_millis(args.delay_ms),
        ..Impairment::p4_profile()
    };

    if args.check_link {
        return check_link(&handle, impairment, args.seed);
    }

    let transports = if args.transport.is_empty() {
        vec![
            Transport::Datagram,
            Transport::Shared,
            Transport::Bulk,
            Transport::Split,
        ]
    } else {
        args.transport.clone()
    };

    let mut results = Vec::new();
    for transport in transports {
        // A fresh link per transport: a congestion window warmed by the last
        // run would flatter whichever came second.
        let outcome = run_one(
            &handle,
            transport,
            impairment,
            args.seed,
            args.seconds,
            args.repair_hz,
        )?;
        results.push((transport, outcome));
    }

    if args.json {
        report_json(&results);
    } else {
        report_table(&results, impairment, args.seconds, args.repair_hz);
    }
    Ok(())
}

/// Check the link is doing what it was told, before trusting any measurement.
///
/// The peers cannot route around it — they have no IP transport — so this is
/// not the path check an earlier proxy-based version needed. It is the
/// *model* check: that the observed drop rate is the configured one, so a
/// figure quoted at "3% loss" was measured at 3% loss.
fn check_link(handle: &tokio::runtime::Handle, impairment: Impairment, seed: u64) -> Result<()> {
    let mut link = link::establish(handle, impairment, seed)?;
    let witness = link.witness;

    let start = Instant::now();
    let mut seq = 0u32;
    while start.elapsed() < Duration::from_secs(5) {
        link.app
            .world_mut()
            .resource_mut::<Messages<SendPacket>>()
            .write(SendPacket::state(
                witness,
                workload::encode(
                    Class::State,
                    seq,
                    Instant::now(),
                    start,
                    workload::STATE_BYTES,
                ),
            ));
        seq += 1;
        link.app.update();
        std::thread::sleep(STEP);
    }

    let (delivered, dropped, overflowed, bytes) = link.wire.stats().read();
    let offered = delivered + dropped + overflowed;
    let observed = if offered == 0 {
        0.0
    } else {
        dropped as f64 / offered as f64
    };
    println!("subject:    {}", link.subject);
    println!("witness:    {witness}");
    println!("offered:    {offered} packets, {} kB", bytes / 1_000);
    println!("delivered:  {delivered}");
    println!(
        "dropped:    {dropped} ({:.2}% observed, {:.2}% configured)",
        observed * 100.0,
        impairment.loss * 100.0
    );
    println!("overflowed: {overflowed}");

    anyhow::ensure!(
        offered > 100,
        "the link carried almost nothing; the peers never really talked"
    );
    anyhow::ensure!(
        (observed - impairment.loss).abs() < 0.01,
        "observed loss {observed:.3} is not the configured {:.3} — the impairment model is not \
         doing what the figures will claim it did",
        impairment.loss
    );
    anyhow::ensure!(
        overflowed * 20 < delivered,
        "{overflowed} packets were dropped for a full inbox rather than by the loss model, which \
         would show up as extra loss the seed does not account for"
    );
    println!("\nthe link is impairing exactly what it was told to");
    Ok(())
}

/// Run one transport for `seconds` and return what it cost.
fn run_one(
    handle: &tokio::runtime::Handle,
    transport: Transport,
    impairment: Impairment,
    seed: u64,
    seconds: u64,
    repair_hz: u32,
) -> Result<Outcome> {
    let mut link = link::establish(handle, impairment, seed)?;
    let subject = link.subject;
    let witness = link.witness;

    let mut sender = Sender::default();
    let mut receiver = Receiver::default();
    let mut outcome = Outcome::default();
    let mut queued: Vec<SendPacket> = Vec::new();

    let origin = Instant::now();
    let deadline = origin + Duration::from_secs(seconds);

    // Next due send per class, as a count of elapsed periods rather than a
    // drifting accumulator.
    let mut due = [0u64; 3];

    while Instant::now() < deadline {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(origin);

        for (index, (class, hz)) in [
            (Class::State, workload::STATE_HZ),
            (Class::Sparse, workload::SPARSE_HZ),
            (Class::Repair, repair_hz),
        ]
        .into_iter()
        .enumerate()
        {
            let periods = elapsed.as_millis() as u64 * u64::from(hz) / 1_000;
            while due[index] <= periods {
                sender.emit(
                    transport,
                    class,
                    witness,
                    now,
                    origin,
                    &mut queued,
                    &mut outcome,
                );
                due[index] += 1;
            }
        }

        receiver.retry_stalled(now, subject, &mut queued, &mut outcome);

        {
            let mut messages = link.app.world_mut().resource_mut::<Messages<SendPacket>>();
            for packet in queued.drain(..) {
                messages.write(packet);
            }
        }

        link.app.update();

        // Drain what arrived. A packet whose `from` is the subject was carried
        // to the witness, and vice versa — one world, two peers, and the peer
        // id is what tells them apart.
        let inbound: Vec<(orrery_protocol::NodeId, bytes::Bytes)> = {
            let mut messages = link.app.world_mut().resource_mut::<Messages<PeerPacket>>();
            messages
                .drain()
                .map(|packet| (packet.from, packet.payload))
                .collect()
        };
        let arrived = Instant::now();
        for (from, payload) in inbound {
            if from == witness {
                // A chunk request travelling back to the subject.
                if let Inbound::Request { seq, offset } = receiver.accept(
                    transport,
                    &payload,
                    arrived,
                    origin,
                    subject,
                    &mut queued,
                    &mut outcome,
                ) {
                    sender.serve_request(seq, offset, witness, &mut queued);
                }
                continue;
            }
            match receiver.accept(
                transport,
                &payload,
                arrived,
                origin,
                subject,
                &mut queued,
                &mut outcome,
            ) {
                Inbound::Complete(received) => outcome
                    .samples
                    .entry(received.class.name())
                    .or_insert_with(Samples::default)
                    .record(received.latency),
                Inbound::Request { seq, offset } => {
                    sender.serve_request(seq, offset, witness, &mut queued);
                }
                Inbound::Partial => {}
            }
        }

        std::thread::sleep(STEP);
    }

    // A grace period: a repair started in the last second is still in flight,
    // and counting it as lost would charge every transport for the deadline.
    let grace = Instant::now() + Duration::from_secs(2);
    while Instant::now() < grace {
        {
            let mut messages = link.app.world_mut().resource_mut::<Messages<SendPacket>>();
            for packet in queued.drain(..) {
                messages.write(packet);
            }
        }
        link.app.update();
        let inbound: Vec<(orrery_protocol::NodeId, bytes::Bytes)> = {
            let mut messages = link.app.world_mut().resource_mut::<Messages<PeerPacket>>();
            messages
                .drain()
                .map(|packet| (packet.from, packet.payload))
                .collect()
        };
        let arrived = Instant::now();
        for (from, payload) in inbound {
            let reply_to = if from == witness { subject } else { witness };
            match receiver.accept(
                transport,
                &payload,
                arrived,
                origin,
                reply_to,
                &mut queued,
                &mut outcome,
            ) {
                Inbound::Complete(received) => outcome
                    .samples
                    .entry(received.class.name())
                    .or_insert_with(Samples::default)
                    .record(received.latency),
                Inbound::Request { seq, offset } => {
                    sender.serve_request(seq, offset, witness, &mut queued);
                }
                Inbound::Partial => {}
            }
        }
        std::thread::sleep(STEP);
    }

    let (delivered, dropped, overflowed, bytes) = link.wire.stats().read();
    outcome.link_packets = delivered + dropped + overflowed;
    outcome.link_bytes = bytes;
    anyhow::ensure!(
        outcome.link_packets > 0,
        "the {} run carried nothing over the link",
        transport.name()
    );
    anyhow::ensure!(
        overflowed * 20 < delivered.max(1),
        "the {} run lost {overflowed} packets to a full inbox rather than to the loss model, so \
         its figures include impairment the seed does not account for",
        transport.name()
    );
    Ok(outcome)
}

fn ms(duration: Option<Duration>) -> String {
    duration.map_or_else(
        || "—".to_owned(),
        |d| format!("{:.1}", d.as_secs_f64() * 1e3),
    )
}

fn report_table(
    results: &[(Transport, Outcome)],
    impairment: Impairment,
    seconds: u64,
    repair_hz: u32,
) {
    println!(
        "\n{} s per transport · {:.0}% loss · {} ms RTT · {} ms jitter on {:.0}% of packets \
         · {} kB of repair/s\n",
        seconds,
        impairment.loss * 100.0,
        impairment.delay.as_millis() * 2,
        impairment.jitter.as_millis(),
        impairment.jitter_rate * 100.0,
        u64::from(repair_hz) * workload::REPAIR_BYTES as u64 / 1_000,
    );
    println!(
        "{:<10} {:<8} {:>7} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "transport", "class", "n", "done %", "p50 ms", "p95 ms", "p99 ms", "max ms"
    );
    for (transport, outcome) in results {
        for class in [Class::State, Class::Sparse, Class::Repair] {
            let empty = Samples::default();
            let samples = outcome.samples.get(class.name()).unwrap_or(&empty);
            println!(
                "{:<10} {:<8} {:>7} {:>8.1}% {:>9} {:>9} {:>9} {:>8}",
                transport.name(),
                class.name(),
                samples.count(),
                outcome.completion(class) * 100.0,
                ms(samples.quantile(0.50)),
                ms(samples.quantile(0.95)),
                ms(samples.quantile(0.99)),
                ms(samples.max()),
            );
        }
    }

    println!(
        "\n{:<10} {:>14} {:>14} {:>14}",
        "transport", "link kB", "packets", "chunk retries"
    );
    for (transport, outcome) in results {
        println!(
            "{:<10} {:>14} {:>14} {:>14}",
            transport.name(),
            outcome.link_bytes / 1_000,
            outcome.link_packets,
            outcome.chunk_retries,
        );
    }
    println!();
}

fn report_json(results: &[(Transport, Outcome)]) {
    let rows: Vec<serde_json::Value> = results
        .iter()
        .flat_map(|(transport, outcome)| {
            [Class::State, Class::Sparse, Class::Repair]
                .into_iter()
                .map(|class| {
                    let empty = Samples::default();
                    let samples = outcome.samples.get(class.name()).unwrap_or(&empty);
                    serde_json::json!({
                        "transport": transport.name(),
                        "class": class.name(),
                        "n": samples.count(),
                        "completion": outcome.completion(class),
                        "p50_ms": samples.quantile(0.50).map(|d| d.as_secs_f64() * 1e3),
                        "p95_ms": samples.quantile(0.95).map(|d| d.as_secs_f64() * 1e3),
                        "p99_ms": samples.quantile(0.99).map(|d| d.as_secs_f64() * 1e3),
                        "max_ms": samples.max().map(|d| d.as_secs_f64() * 1e3),
                        "link_bytes": outcome.link_bytes,
                        "chunk_retries": outcome.chunk_retries,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_owned())
    );
}
