//! The swarm: N bots, a router between them, and the criterion they must meet.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde::Serialize;

use orrery_core::CoreCodec;
use orrery_net::channels::{encode_replication, Channel};
use orrery_net::peer_link::{SendPacket, StreamMode};
use orrery_protocol::coord::PeerEntry;
use orrery_protocol::{CellId, NodeId, PersistId, UniverseSeed};

use crate::bot::{Bot, TICK_HZ};
use aeronet_iroh::stream::{IrohStreamIo, RecvMessage};

use crate::router::{Impairment, Router, RouterCounters};

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

/// The whole run.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmReport {
    /// What produced this run, and what it would take to produce it again.
    pub identity: RunIdentity,
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
    /// Player-hours accumulated: peers times simulated seconds.
    pub player_hours: f64,
    /// Chain gaps detected across the swarm — expected under loss.
    pub total_gaps: u64,
    /// Signals raised against honest peers. **Every one is a false positive.**
    pub total_false_positives: u64,
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
}

impl Swarm {
    /// Build a swarm from `config`.
    #[must_use]
    pub fn new(config: SwarmConfig) -> Self {
        let mut universe = [0u8; 32];
        universe[0..8].copy_from_slice(&config.seed.to_le_bytes());
        let seed = UniverseSeed(universe);

        let bots: Vec<Bot> = (0..config.peers)
            .map(|index| {
                Bot::new(
                    index,
                    config.peers,
                    seed,
                    config.cell_edge_m,
                    config.witnessing,
                )
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
        }
    }

    /// Wire every bot to every other: the mesh regime the criterion runs in.
    fn form_island(&mut self) {
        let roster: Vec<(NodeId, CellId)> = self
            .bots
            .iter_mut()
            .map(|bot| (bot.node, bot.cell().expect("seeded")))
            .collect();

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
        let roster: Vec<(NodeId, CellId)> = self
            .bots
            .iter_mut()
            .map(|bot| (bot.node, bot.cell().expect("committed")))
            .collect();
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
        let (node, cell, payload) = {
            let bot = &mut self.bots[index];
            let cell = bot.cell().expect("committed");
            let entity = bot.entity();
            let body = bot.body();
            // The body bytes plus the authority's *committed* cell. D2 makes
            // the commitment a single-writer value emitted by the holder: a
            // receiver that recomputed it from the position would get the raw
            // geometric cell with no hysteresis, so a peer sitting on a
            // boundary would flip cells on every packet and flap in and out of
            // the receiver's AOI for reasons that have nothing to do with where
            // it is.
            // The entity and the tick travel with the state. Both are things
            // real replication carries anyway — prediction needs the tick and
            // interest needs the identity — and without them a receiver holds
            // bytes it cannot attribute to a subject or line up against a
            // claim.
            (
                bot.node,
                cell,
                encode_replication(&(body.to_canonical(), cell, entity, tick + 1)),
            )
        };
        let peers: Vec<NodeId> = self.bots[index]
            .app
            .world()
            .resource::<orrery_net::IslandMembership>()
            .peers
            .iter()
            .filter(|entry| entry.node != node && entry.cells.contains(&cell))
            .map(|entry| entry.node)
            .collect();
        let bot = &mut self.bots[index];
        let mut messages = bot
            .app
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<SendPacket>>();
        for peer in peers {
            messages.write(SendPacket {
                to: peer,
                channel: Channel::State,
                payload: Bytes::from(payload.clone()),
                mode: StreamMode::Shared,
            });
        }
    }

    /// Drain what each bot's send path handed the IO layer into the router.
    fn collect_sends(&mut self, tick: u64) {
        for index in 0..self.bots.len() {
            let node = self.bots[index].node;
            if !self.bots[index].profile.is_sending(tick) {
                // The peer is hitching. Its packets are built and then never
                // leave — which is what a client stall actually is, and it
                // leaves the peer's own log intact so it can still answer for
                // itself when its witnesses come asking.
                // Both lanes: a client hitch is the socket going unserviced,
                // and a stalling peer does not get to keep its reliable lane
                // flowing while its datagrams stop.
                let world = self.bots[index].app.world_mut();
                let mut query = world.query::<(
                    &orrery_net::plugin::Peer,
                    &mut aeronet_io::Session,
                    &mut IrohStreamIo,
                )>();
                for (_, mut session, mut streams) in query.iter_mut(world) {
                    session.send.clear();
                    streams.send.clear();
                }
                continue;
            }
            let mut outbound: Vec<(NodeId, Option<StreamMode>, Bytes)> = Vec::new();
            {
                let world = self.bots[index].app.world_mut();
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
            }
            for (to, stream, payload) in outbound {
                let Some(target) = self.index_of.get(&to).copied() else {
                    self.router.counters.misaddressed += 1;
                    continue;
                };
                match stream {
                    Some(mode) => self.router.accept_stream(tick, node, target, mode, payload),
                    None => self.router.accept(tick, node, target, payload),
                }
            }
        }
    }

    /// Hand every due packet to its recipient's buffer, on the lane it came in on.
    fn deliver(&mut self, tick: u64) {
        for delivery in self.router.deliver_due(tick) {
            let world = self.bots[delivery.to].app.world_mut();
            let mut query = world.query::<(
                &orrery_net::plugin::Peer,
                &mut aeronet_io::Session,
                &mut IrohStreamIo,
            )>();
            for (peer, mut session, mut streams) in query.iter_mut(world) {
                if peer.id != delivery.from {
                    continue;
                }
                if delivery.stream.is_some() {
                    streams.recv.push(RecvMessage {
                        payload: delivery.payload,
                        recv_at: bevy_platform::time::Instant::now(),
                    });
                } else {
                    session.recv.push(aeronet_io::packet::RecvPacket {
                        payload: delivery.payload,
                        recv_at: bevy_platform::time::Instant::now(),
                    });
                }
                break;
            }
        }
    }

    /// Run the configured number of simulated seconds.
    #[must_use]
    pub fn run(mut self) -> SwarmReport {
        self.form_island();
        if self.config.witnessing {
            self.seed_witnesses();
        }
        let ticks = self.config.seconds * TICK_HZ;
        let send_every = (TICK_HZ / self.config.send_hz.max(1)).max(1);
        let mut late_join = None;

        let mut phase = [0u128; 6];
        for tick in 0..ticks {
            let mut mark = std::time::Instant::now();
            if self.config.witnessing {
                // Before the tick runs: a claim commits to pre-step state.
                for bot in &mut self.bots {
                    bot.publish_claim(tick);
                }
            }
            for index in 0..self.bots.len() {
                self.bots[index].step_core(tick, self.config.cell_edge_m);
            }
            phase[0] += mark.elapsed().as_nanos();
            mark = std::time::Instant::now();
            // The last tick of each send window, not the first. Broadcasting
            // at `t` ships the state *after* `t` stepped, which is the state a
            // claim at `t + 1` commits to — so sending on the window's last
            // tick is what makes the replicated state and the signed claim the
            // same object. On the window's first tick the two are one tick
            // apart forever, and no receiver can check a claim against a state
            // it was actually sent, which is the corroboration stage 1 and
            // re-anchoring both rest on. Same cadence and same number of
            // sends; only the phase moves.
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
            phase[3] += mark.elapsed().as_nanos();
            mark = std::time::Instant::now();
            self.collect_sends(tick);
            phase[4] += mark.elapsed().as_nanos();
            mark = std::time::Instant::now();
            self.deliver(tick);
            phase[5] += mark.elapsed().as_nanos();

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
                eprintln!("p1-swarm: phase {name:>11}: {:>8.2}s", nanos as f64 / 1e9);
            }
        }
        let _ = self.deliver_due_all(ticks + u64::from(self.config.impairment.jitter_ticks) + 1);
        self.report(ticks, late_join)
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

        let count = self.bots.len();
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

        // Anchors first: a watcher needs the subject's signed claim and the
        // state it commits to, and both have to be taken before anyone steps.
        let anchors: Vec<(PersistId, NodeId, orrery_protocol::StateClaim, _)> = (0..count)
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

        for (index, witnesses) in sets.iter().enumerate() {
            let members: Vec<NodeId> = witnesses.iter().map(|w| self.bots[*w].node).collect();
            self.bots[index].set_witness_set(members);
            // Each of those peers watches this one.
            let (entity, node, anchor, state) = anchors[index].clone();
            for watcher in witnesses {
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
                    gaps: bot.signals.gaps,
                    false_positives: bot.signals.false_positives(),
                    invariant_breaches: bot.signals.invariant_breaches,
                    claim_mismatches: bot.signals.claim_mismatches,
                    stalled: bot.signals.stalled,
                    repairs_overflowed: bot.repairs_overflowed(),
                    repairs_unservable: bot.repairs_unservable(),
                    frames_recovered: witness.frames_recovered,
                    reanchors: witness.reanchors,
                    unjudged_ticks: witness.unjudged_ticks,
                    judged_ticks: witness.judged_ticks,
                    shown_ticks: witness.shown_ticks,
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
            started_at_unix_secs: self.config.started_at_unix_secs,
            peers: self.bots.len(),
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
            player_hours: self.config.peers as f64 * self.config.seconds as f64 / 3_600.0,
            total_gaps: per_peer.iter().map(|p| p.gaps).sum(),
            total_false_positives: per_peer.iter().map(|p| p.false_positives).sum(),
            total_frames_recovered: per_peer.iter().map(|p| p.frames_recovered).sum(),
            total_reanchors: per_peer.iter().map(|p| p.reanchors).sum(),
            total_unjudged_ticks: per_peer.iter().map(|p| p.unjudged_ticks).sum(),
            total_judged_ticks: per_peer.iter().map(|p| p.judged_ticks).sum(),
            total_shown_ticks: per_peer.iter().map(|p| p.shown_ticks).sum(),
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
                let peer_seconds = self.config.peers as u64 * self.config.seconds.max(1);
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
        // At the criterion population the clause now holds rather than fails:
        // 100.0% on a clean link and 96.0% under the 3% loss / 100 ms jitter
        // profile, both at 32 peers. The residual under loss is timeline shown
        // to a witness while a hole was open that the repair which followed did
        // not recover — four points of it, and the margin over this threshold
        // is only one. Raising the threshold to today's number, or lowering it
        // to accommodate a run that misses, are both measurements rather than
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
        if self.witnessing && self.total_gaps == 0 && self.link.dropped > 0 {
            failures.push(CriterionFailure {
                clause: "the witness sees the stream it is judging",
                detail: "packets were dropped and no peer detected a single chain gap; \
                         the witness is not following the logs it was given"
                    .to_owned(),
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
            player_hours: 32.0,
            total_gaps: 13_009,
            total_false_positives: 0,
            total_frames_recovered: 0,
            total_reanchors: 0,
            total_unjudged_ticks: 0,
            total_judged_ticks: 3_864_390,
            total_shown_ticks: 4_026_190,
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
}
