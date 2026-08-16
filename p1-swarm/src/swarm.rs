//! The swarm: N bots, a router between them, and the criterion they must meet.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde::Serialize;

use orrery_core::CoreCodec;
use orrery_net::channels::{encode_replication, Channel};
use orrery_net::peer_link::SendPacket;
use orrery_protocol::coord::PeerEntry;
use orrery_protocol::{CellId, NodeId, PersistId, UniverseSeed};

use crate::bot::{Bot, TICK_HZ};
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

/// The whole run.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmReport {
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
    fn broadcast(&mut self, index: usize) {
        let (node, cell, payload) = {
            let bot = &mut self.bots[index];
            let cell = bot.cell().expect("committed");
            let body = bot.body();
            // The body bytes plus the authority's *committed* cell. D2 makes
            // the commitment a single-writer value emitted by the holder: a
            // receiver that recomputed it from the position would get the raw
            // geometric cell with no hysteresis, so a peer sitting on a
            // boundary would flip cells on every packet and flap in and out of
            // the receiver's AOI for reasons that have nothing to do with where
            // it is.
            (
                bot.node,
                cell,
                encode_replication(&(body.to_canonical(), cell)),
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
                let world = self.bots[index].app.world_mut();
                let mut query =
                    world.query::<(&orrery_net::plugin::Peer, &mut aeronet_io::Session)>();
                for (_, mut session) in query.iter_mut(world) {
                    session.send.clear();
                }
                continue;
            }
            let mut outbound: Vec<(NodeId, Bytes)> = Vec::new();
            {
                let world = self.bots[index].app.world_mut();
                let mut query =
                    world.query::<(&orrery_net::plugin::Peer, &mut aeronet_io::Session)>();
                for (peer, mut session) in query.iter_mut(world) {
                    for packet in session.send.drain(..) {
                        outbound.push((peer.id, packet));
                    }
                }
            }
            for (to, payload) in outbound {
                let Some(target) = self.index_of.get(&to).copied() else {
                    self.router.counters.misaddressed += 1;
                    continue;
                };
                self.router.accept(tick, node, target, payload);
            }
        }
    }

    /// Hand every due packet to its recipient's session buffer.
    fn deliver(&mut self, tick: u64) {
        for (to, from, payload) in self.router.deliver_due(tick) {
            let world = self.bots[to].app.world_mut();
            let mut query = world.query::<(&orrery_net::plugin::Peer, &mut aeronet_io::Session)>();
            for (peer, mut session) in query.iter_mut(world) {
                if peer.id == from {
                    session.recv.push(aeronet_io::packet::RecvPacket {
                        payload,
                        recv_at: bevy_platform::time::Instant::now(),
                    });
                    break;
                }
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
            if tick % send_every == 0 {
                for index in 0..self.bots.len() {
                    self.broadcast(index);
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
                    replicas,
                    tagged,
                    proxied,
                    undecodable,
                }
            })
            .collect();

        SwarmReport {
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

impl SwarmReport {
    /// Check the report against the P1 demo criterion.
    ///
    /// Every clause, or the run does not count — the phase gate is the whole
    /// sentence, not the convenient half of it.
    #[must_use]
    pub fn against_criterion(
        &self,
        budget_bits: u64,
        min_cells: usize,
        max_pops: u64,
    ) -> Vec<CriterionFailure> {
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
        if self.total_shed > 0 {
            // Shedding means the budget was reached, which the criterion treats
            // as a failure rather than a success of the backstop: a peer that
            // had to drop state did not stay *within* budget, it was held to it.
            failures.push(CriterionFailure {
                clause: "no load shed to stay within budget",
                detail: format!("{} packets shed", self.total_shed),
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
