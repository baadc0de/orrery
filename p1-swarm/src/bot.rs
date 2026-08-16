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

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_math::Vec3;
use bevy_time::{Real, Time};

use orrery_conformance::{Body, Reference};
use orrery_core::{Executor, QPos, QVel};
use orrery_net::budget::{UploadBudget, UploadMeter};
use orrery_net::peer_link::{
    forget_departed_links, receive_peer_packets, send_peer_packets, PeerLinkCounters, PeerPacket,
    SendPacket,
};
use orrery_net::plugin::Peer;
use orrery_net::{IslandMembership, IslandSource};
use orrery_protocol::coord::{IslandId, PeerEntry, TopologyRegime};
use orrery_protocol::{
    CellId, NodeId, PersistId, StateClaim, Tick, UniverseSeed, DEFAULT_CELL_EDGE_M,
};
use orrery_spatial::hysteresis::GridPosition;
use orrery_spatial::interest::{HighRate, InterestSelection, Proxy};
use orrery_spatial::plugin::{Cell, LocalPlayer};
use orrery_spatial::{OrrerySpatialPlugin, SpatialConfig};
use orrery_witness::plugin::{PublishClaim, PublishFrame, WitnessSet, WitnessState};
use orrery_witness::{Watch, Witness, WitnessConfig, WitnessPlugin, WitnessSignal, Witnessed};

use crate::chain::Chain;
use crate::profile::Profile;

/// The fixed simulation tick (VC-1).
pub const TICK: Duration = Duration::from_nanos(16_666_667);
/// Ticks per simulated second.
pub const TICK_HZ: u64 = 60;

/// Radius of the shared orbit, in metres.
///
/// A ~15.7 km circumference, so one lap traverses about 122 interest cells at
/// the 128 m edge — comfortably past the criterion's 64 without needing the
/// path to drift, and reached in roughly eight simulated minutes at cruise.
const ORBIT_RADIUS_M: f64 = 2_500.0;

/// The arc of the shared orbit the crowd is spread over, in radians.
///
/// 0.2 rad at this radius is ~500 m of crowd, or ~16 m between neighbours, so
/// roughly two dozen bots fall inside any one bot's 27-cell neighbourhood. That
/// is deliberately right at the 24-entity high-rate cap (D16): a looser crowd
/// would never exercise the cap, and a tighter one would sit so far past it
/// that the set never churns. At the boundary, ordinary movement pushes
/// entities in and out of the set, which is the case the cap exists for.
const CROWD_ARC_RAD: f64 = 0.08;

/// Fractional spread of orbit radii across the crowd.
///
/// Every bot cruises at the same speed, so a bot on a tighter orbit sweeps a
/// larger angle per tick (ω = v/r) and steadily overtakes the ones outside it.
/// Without that shear the crowd is a rigid formation: relative distances never
/// change, the interest set never reorders, and every clause about churn passes
/// by describing a world where nothing moves relative to anything else.
const RADIAL_SPREAD: f64 = 0.10;

/// Cruising speed in metres per second — a fast ground vehicle.
///
/// At a 128 m cell edge this is a crossing roughly every four simulated
/// seconds: fast enough to cover hundreds of cells in the criterion's hour,
/// slow enough that a bot dwells near a boundary long enough for the hysteresis
/// margin to matter. Anything much faster stops testing hysteresis and starts
/// testing whether cells can be skipped entirely.
const CRUISE_MPS: f64 = 32.0;

/// How soon a re-promotion counts as a pop, in ticks.
///
/// One simulated second. docs/03-replication.md §9.5 gives the proxy stream a
/// 5 s decay from 4 Hz down to its scored rate precisely so a *briefly*
/// demoted entity re-promotes without a visible stutter; anything that round
/// trips inside a second never got the benefit of that ramp.
const POP_WINDOW_TICKS: u64 = 60;

/// The rate a briefly demoted entity must stay above for the demotion to be
/// invisible: the midpoint of the 1–4 Hz proxy range.
///
/// Not the *top* of the range — the §9.5 ramp starts at 4 Hz and decays, so
/// demanding 4 Hz throughout would call every demotion longer than an instant a
/// pop, which is a tautology against the ramp rather than a test of it. What a
/// player actually sees is an entity that fell toward the 1 Hz floor and came
/// back; with the ramp, a sub-second round trip never gets near it.
const RAMP_FLOOR_HZ: f32 = 2.5;

/// A synthetic peer.
pub struct Bot {
    /// This peer's identity.
    pub node: NodeId,
    /// Its index in the swarm, used for routing.
    pub index: usize,
    /// The headless app running the real plugins.
    pub app: App,
    /// The core executor holding this bot's own body.
    executor: Executor<Reference>,
    /// The entity this bot authors.
    entity: PersistId,
    /// Heading change per tick, in micro-radians — its share of the circle.
    turn_urad: i64,
    /// Thrust magnitude in mm/s².
    accel_mmss: i64,
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
    /// subscriptions each time. The 10% hysteresis margin exists to make this
    /// number zero, so measuring total crossings would measure travel rather
    /// than the thing the margin fixes.
    pub boundary_flips: u64,
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
    /// This bot's behavioural profile.
    pub profile: Profile,
    /// The signed log this bot authors, when witnessing is on.
    pub chain: Option<Chain>,
    /// Witness signals raised against island-mates, by kind.
    pub signals: SignalTally,
}

/// Witness signals a peer raised, by kind.
///
/// Every bot in the swarm is honest, so anything here except a gap is a **false
/// positive** — that is what makes the count meaningful without a separate
/// oracle for who was cheating. Gaps are counted separately because a gap is a
/// question, not an accusation: it is the expected answer to a dropped datagram.
#[derive(Debug, Default, Clone, Copy)]
pub struct SignalTally {
    /// Chain gaps detected — repairs requested, not accusations.
    pub gaps: u64,
    /// Stage-1 invariant breaches. A false positive here.
    pub invariant_breaches: u64,
    /// Re-execution disagreeing with a signed claim. A false positive here.
    pub claim_mismatches: u64,
    /// Reports assembled. Always zero in shadow mode; a false positive here.
    pub reports: u64,
    /// Subjects reported as stalled — a hole never filled.
    ///
    /// A false positive in this swarm: every bot answers repairs, so a stall
    /// means the repair path could not keep up, not that a peer refused.
    pub stalled: u64,
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

/// The ECS entity mirroring another bot's body.
///
/// Without these the interest selector has nothing to rank: `update_interest_set`
/// queries the non-local entities with a position, and a harness that never
/// spawned any would report zero churn, never reach the 24-entity cap, and
/// certify an interest set it had not exercised.
#[derive(Component)]
pub struct Replica(pub NodeId);

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
    /// Replicas spawned.
    pub spawned: u64,
    /// Replicas updated.
    pub updated: u64,
}

/// Upserts a replica entity per peer from the state packets that arrive.
///
/// This is the receiving half of replication, reduced to what the criterion
/// measures: which entities a peer is tracking and where they are. Whether the
/// bytes arrived as a delta or a snapshot is `orrery_predict`'s concern; the
/// interest selector only needs a position and a cell.
pub fn apply_replicas(
    mut packets: MessageReader<PeerPacket>,
    mut commands: Commands,
    edge: Res<CellEdge>,
    tick: Res<SimTick>,
    mut counters: ResMut<ReplicaCounters>,
    existing: Query<(Entity, &Replica)>,
    witness: Option<ResMut<WitnessState<Reference>>>,
) {
    let mut witness = witness;
    for packet in packets.read() {
        counters.seen += 1;
        if packet.channel != orrery_net::channels::Channel::State {
            counters.wrong_channel += 1;
            continue;
        }
        // Witness records share this lane and are sub-tagged as such; skipping
        // them is not a decode failure. Counting them would make `undecodable`
        // — the guard that catches the harness measuring an empty world — fire
        // constantly and stop meaning anything.
        let Some((encoded, cell, entity, at)) = orrery_net::channels::decode_replication::<(
            Vec<u8>,
            CellId,
            orrery_protocol::PersistId,
            u64,
        )>(&packet.payload) else {
            if orrery_net::channels::decode_witness::<orrery_protocol::WitnessMsg>(&packet.payload)
                .is_none()
            {
                counters.undecodable += 1;
            }
            continue;
        };
        let Ok(body) = <Body as orrery_core::CoreCodec>::decode(&encoded) else {
            counters.bad_body += 1;
            continue;
        };
        // Stage 1, on everything received, witnessed or not (docs/06 §3) — and
        // the sample store a blind watch re-anchors from. The state is the
        // authority's own, stamped with the tick it belongs to, so a claim's
        // `state_hash` either matches it or does not; nothing here is inferred
        // from the receiver's clock.
        if let Some(witness) = witness.as_mut() {
            witness.0.observe(orrery_witness::Observation {
                entity,
                state: &body,
                tick: orrery_protocol::Tick::new(at),
            });
        }

        // Position is for distance ranking; the *cell* is the authority's own
        // committed value, never recomputed here (D2).
        let grid = grid_of(&body.pos, edge.0);
        match existing
            .iter()
            .find(|(_, replica)| replica.0 == packet.from)
        {
            Some((entity, _)) => {
                counters.updated += 1;
                commands
                    .entity(entity)
                    .insert((Cell(cell), GridPosition(grid), LastSeen(tick.0)));
            }
            None => {
                counters.spawned += 1;
                commands.spawn((
                    Replica(packet.from),
                    Cell(cell),
                    GridPosition(grid),
                    LastSeen(tick.0),
                ));
            }
        }
    }
}

impl Bot {
    /// Build a bot at `index` of `count`, spread around a ring.
    pub fn new(
        index: usize,
        count: usize,
        seed: UniverseSeed,
        cell_edge_m: f32,
        witnessing: bool,
    ) -> Self {
        let secret = bot_key(index);
        let node = secret.public();
        let entity = PersistId::new(index as u64 + 1);

        // The swarm is a *crowd*, not a scatter: every bot rides the same large
        // circle, spread over a short arc of it. Spacing that keeps neighbours
        // one or two cells apart is what makes interest sets overlap, churn as
        // the crowd moves, and actually hit the 24-entity cap with 32 peers. A
        // scatter would put every bot alone in its neighbourhood, and every
        // clause about interest would pass by being vacuous.
        let share = index as f64 / count.max(1) as f64;
        let radius_m = ORBIT_RADIUS_M * (1.0 + RADIAL_SPREAD * (share - 0.5));
        let arc = CROWD_ARC_RAD * share;
        let start = QPos::from_metres(libm::cos(arc) * radius_m, 0.0, libm::sin(arc) * radius_m);

        let mut executor = Executor::new(Reference, seed);
        executor.insert(
            entity,
            Body {
                pos: start,
                vel: QVel::default(),
                // Tangent to the ring, so the bot travels around it.
                heading_urad: ((arc + core::f64::consts::FRAC_PI_2) * 1_000_000.0) as i64,
                hp: 100,
                shield: 25,
                roll_fold: 0,
            },
        );

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
            app.add_plugins(WitnessPlugin::<Reference>::new())
                .insert_resource(WitnessState(Witness::<Reference>::new(
                    WitnessConfig::default(),
                    seed,
                    || Reference,
                )));
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
            executor,
            entity,
            // ω = v/r, so the bot actually follows the orbit it started on.
            // Picking a turn rate independently of the speed makes it spiral
            // into whatever radius the two happen to imply — which is how the
            // first version ended up circling 200 m and visiting ten cells.
            turn_urad: ((CRUISE_MPS / radius_m) / TICK_HZ as f64 * 1_000_000.0) as i64,
            accel_mmss: 60_000,
            visited: vec![cell_of(start_grid)],
            crossings: 0,
            boundary_flips: 0,
            previous_cell: None,
            current_cell: Some(cell_of(start_grid)),
            interest_churn: 0,
            proxy_pops: 0,
            profile: Profile::for_index(index, witnessing),
            chain: witnessing.then(|| {
                Chain::new(
                    secret.clone(),
                    entity,
                    orrery_conformance::REFERENCE_RULESET,
                    0,
                )
            }),
            signals: SignalTally::default(),
            last_high_rate: Vec::new(),
            demoted_at: Vec::new(),
            tick: 0,
        }
    }

    /// This bot's authored body.
    #[must_use]
    pub fn body(&self) -> &Body {
        self.executor.state(self.entity).expect("seeded")
    }

    /// The bot's current speed in metres per second.
    #[must_use]
    pub fn speed_mps(&self) -> f64 {
        let (vx, vy, vz) = self.body().vel.to_metres_per_sec();
        libm::sqrt(vx * vx + vy * vy + vz * vz)
    }

    /// Advance the core by one tick and mirror the result into the ECS.
    ///
    /// Thrust cuts out at [`CRUISE_MPS`]. The reference ruleset has no drag, so
    /// a constant thrust is a constant *acceleration* — left alone the bots
    /// reach several km/s and cross a cell every other tick, which is not
    /// roaming, it is teleporting, and it would make every hysteresis and
    /// interest-churn number meaningless.
    pub fn step_core(&mut self, tick: u64, cell_edge_m: f32) {
        let accel_mmss =
            self.profile
                .accel_mmss(tick, self.speed_mps(), self.accel_mmss, CRUISE_MPS);
        let command = orrery_conformance::Command::Thrust {
            accel_mmss,
            turn_urad: self.turn_urad,
        };
        // Log *before* executing, and log exactly what is about to be applied.
        // A log written from what happened rather than what was asked would
        // close the gap a cheat lives in by construction, and then the harness
        // could not tell an honest bot from a careful one.
        if let Some(chain) = &mut self.chain {
            chain.log_input(tick, &command);
        }
        let outcome = self
            .executor
            .step_entity(self.entity, Tick::new(tick), &[command])
            .expect("entity present");
        if let Some(chain) = &mut self.chain {
            chain.log_tick_hash(outcome.state_hash);
        }

        let grid = grid_of(&self.body().pos, cell_edge_m);
        let world = self.app.world_mut();
        let mut query = world.query_filtered::<(&mut GridPosition, &Cell), With<LocalPlayer>>();
        let Ok((mut position, _)) = query.single_mut(world) else {
            return;
        };
        position.0 = grid;
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

    /// Record what changed this tick: cell crossings and interest churn.
    pub fn sample(&mut self) {
        let world = self.app.world_mut();
        let mut cells = world.query_filtered::<&Cell, With<LocalPlayer>>();
        if let Ok(cell) = cells.single(world) {
            if self.current_cell != Some(cell.0) {
                self.crossings += 1;
                // Returning to the cell committed *before* the last one is the
                // boundary flip the hysteresis margin exists to prevent.
                if self.previous_cell == Some(cell.0) {
                    self.boundary_flips += 1;
                }
                self.previous_cell = self.current_cell;
                self.current_cell = Some(cell.0);
                if !self.visited.contains(&cell.0) {
                    self.visited.push(cell.0);
                }
            }
        }

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
        self.tick += 1;
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

    /// Attach a session for `peer`, so the send path has somewhere to write.
    pub fn link(&mut self, peer: NodeId, mtu: usize) {
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

    /// This bot's committed cell.
    #[must_use]
    pub fn cell(&mut self) -> Option<CellId> {
        let world = self.app.world_mut();
        let mut query = world.query_filtered::<&Cell, With<LocalPlayer>>();
        query.single(world).ok().map(|cell| cell.0)
    }

    /// Start watching `subject`'s `entity`, anchored at a signed claim.
    pub fn watch(&mut self, entity: PersistId, subject: NodeId, anchor: StateClaim, state: Body) {
        let Some(mut witness) = self
            .app
            .world_mut()
            .get_resource_mut::<WitnessState<Reference>>()
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
        let state = self.executor.state(self.entity).expect("seeded").clone();
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
    pub fn drain_signals(&mut self) {
        let Some(mut messages) = self
            .app
            .world_mut()
            .get_resource_mut::<bevy_ecs::message::Messages<Witnessed>>()
        else {
            return;
        };
        for witnessed in messages.drain() {
            match witnessed.signal {
                WitnessSignal::Gap(_) => self.signals.gaps += 1,
                // A subject that never fills a hole. Against this swarm that is
                // a false positive like any other — every bot answers repairs,
                // so a stall here means the repair path failed to keep up, not
                // that anyone refused.
                WitnessSignal::Stalled { .. } => self.signals.stalled += 1,
                WitnessSignal::InvariantBreach { .. } => self.signals.invariant_breaches += 1,
                WitnessSignal::ClaimMismatch { .. } => self.signals.claim_mismatches += 1,
                WitnessSignal::Report(_) => self.signals.reports += 1,
            }
        }
    }

    /// This bot's current core state, for seeding a watcher's anchor.
    #[must_use]
    pub fn state(&self) -> Body {
        self.executor.state(self.entity).expect("seeded").clone()
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

    /// This peer's witness counters, or zeroes if it witnesses nobody.
    #[must_use]
    pub fn witness_counters(&self) -> orrery_witness::WitnessCounters {
        self.app
            .world()
            .get_resource::<WitnessState<Reference>>()
            .map_or_else(Default::default, |state| state.0.counters())
    }

    /// Where this peer's inbound state packets went.
    #[must_use]
    pub fn replica_counters(&self) -> ReplicaCounters {
        *self.app.world().resource::<ReplicaCounters>()
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

/// A deterministic key per bot index.
#[must_use]
pub fn bot_key(index: usize) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    seed[31] = 0xB0;
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
