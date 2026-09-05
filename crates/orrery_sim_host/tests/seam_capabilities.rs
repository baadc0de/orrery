//! The three seam capabilities S6.b needs, each proved against what the
//! campaign driver actually does (#1108).
//!
//! `clients/regolith`'s `CampaignRuntime::advance` steps **one** entity while
//! the same store holds replicas of remote craft, feeds that step's neighbour
//! frames to a witness log, and routes every delivery the ruleset produced
//! *over the wire* to the authority that owns the recipient — never into its
//! own input buffer. Until #1108 the seam could express none of the three, so
//! converging the driver onto it would have changed behaviour.
//!
//! Every test here drives only the public API, from outside the crate, exactly
//! as that driver would. Where a capability could pass vacuously — a freeze
//! test on a body that was not going to move anyway — a control host running
//! the *existing* call proves the difference is real.

use orrery_core::{CoreCodec, NeighborFrame};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::{
    Delivery, HostRoutedTick, InputOrigin, PredictionSet, SealedInput, SimulationHost,
    SimulationHostConfig, TickCount, TickParticipant,
};
use synthetic::{Synthetic, SyntheticAdapter, SyntheticInput, SyntheticState};

#[path = "support/synthetic.rs"]
mod synthetic;

/// The craft this client predicts: `advance`'s `self.entity`.
const LOCAL: PersistId = PersistId(1);
/// A replica of a remote craft, installed verbatim from a decoded claim and
/// frozen between refreshes.
const REMOTE: PersistId = PersistId(2);

/// The tick a replica's state was observed at, as an authority's claim named
/// it. Deliberately behind `FIRST_TICK` and inside the ruleset's staleness
/// bound, so the replica is readable and its frame carries this stamp.
const OBSERVED_TICK: u64 = 99;
const FIRST_TICK: u64 = 100;

type Host = SimulationHost<Synthetic, SyntheticAdapter>;

fn local() -> SyntheticState {
    SyntheticState {
        velocity_um_per_tick: [7_500, 0, 0],
        health: 100,
        target: REMOTE.0,
        ..SyntheticState::default()
    }
}

/// A replica with a velocity of its own. That velocity is the whole point:
/// if the seam stepped it, it would move, and the freeze test could not pass
/// by accident.
fn replica() -> SyntheticState {
    SyntheticState {
        position_um: [40_000, 0, 0],
        velocity_um_per_tick: [-3_250, 1_000, 0],
        health: 100,
        target: LOCAL.0,
        ..SyntheticState::default()
    }
}

fn joined_session() -> Host {
    let mut host = SimulationHost::new(
        SimulationHostConfig::new(UniverseSeed([11; 32])).starting_at(Tick::new(FIRST_TICK)),
        Synthetic,
        SyntheticAdapter,
    );
    // Both craft are installed with an observation stamp inside the ruleset's
    // staleness bound, so each is readable by the other: a stale neighbour is
    // still *recorded* as a read but yields no state, and this scenario needs
    // real reads to produce real deliveries.
    host.install_state_observed(LOCAL, local(), Tick::new(OBSERVED_TICK));
    // The ingest path installs a decoded claim verbatim, carrying the tick the
    // authority observed it at.
    host.install_state_observed(REMOTE, replica(), Tick::new(OBSERVED_TICK));
    host
}

fn state(host: &Host, entity: PersistId) -> SyntheticState {
    SyntheticState::decode(&host.state_bytes(entity).expect("entity is held"))
        .expect("state decodes")
}

/// What the campaign driver does with one tick, as a participant.
///
/// The two hooks stand in for the two halves `advance` owns and the seam did
/// not: the witness column it logs before the step
/// (`log_inputs_with_sources`), and the wire leg it routes deliveries down
/// (`route_delivered_input`).
#[derive(Debug, Default)]
struct CampaignTick {
    /// One entry per input the host sealed, in applied order: what
    /// `log_inputs_with_sources` would fold, with the provenance it needs to
    /// classify each record.
    witnessed: Vec<WitnessedInput>,
    /// Deliveries this driver took off the host and sent to the authority
    /// that owns the recipient.
    wire: Vec<RoutedOverWire>,
    /// Ticks the `sealed` hook was called for, in order.
    sealed_ticks: Vec<Tick>,
}

/// One sealed input as a witness column would record it. A named record, not a
/// `(SyntheticInput, InputOrigin)` pair, for the reason the log cares about:
/// a mis-paired input and source is an unreplayable frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WitnessedInput {
    recipient: PersistId,
    input: SyntheticInput,
    origin: InputOrigin,
}

impl WitnessedInput {
    fn of(sealed: &SealedInput<'_, SyntheticInput>) -> Self {
        Self {
            recipient: sealed.recipient(),
            input: *sealed.input(),
            origin: sealed.origin(),
        }
    }
}

/// One delivery this client sent to a remote authority instead of applying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoutedOverWire {
    source: PersistId,
    recipient: PersistId,
    input: SyntheticInput,
}

impl TickParticipant<Synthetic> for CampaignTick {
    fn sealed(&mut self, tick: Tick, inputs: &[SealedInput<'_, SyntheticInput>]) {
        self.sealed_ticks.push(tick);
        self.witnessed.extend(inputs.iter().map(WitnessedInput::of));
    }

    fn route(
        &mut self,
        source: PersistId,
        delivery: Delivery<SyntheticInput>,
    ) -> Option<Delivery<SyntheticInput>> {
        // A joined client is the authority for its own craft and for nothing
        // else. Anything addressed elsewhere leaves over the wire; the remote
        // authority decides whether it becomes input there.
        if delivery.recipient() == LOCAL {
            return Some(delivery);
        }
        self.wire.push(RoutedOverWire {
            source,
            recipient: delivery.recipient(),
            input: *delivery.input(),
        });
        None
    }
}

// ── Capability 1: stepping a subset of the population ─────────────────────

#[test]
fn a_named_prediction_set_freezes_every_replica_it_does_not_name() {
    let installed = replica();

    let mut predicting = joined_session();
    let mut whole_population = joined_session();

    let mut driver = CampaignTick::default();
    for _ in 0..4 {
        predicting.step_predicted(TickCount::new(1), &PredictionSet::just(LOCAL), &mut driver);
        whole_population.step(TickCount::new(1));
    }

    assert_eq!(
        state(&predicting, REMOTE),
        installed,
        "a replica outside the prediction set is frozen: byte-identical to the state the \
         ingest path installed, four ticks later"
    );
    assert_eq!(
        predicting.observed_tick(REMOTE),
        Some(Tick::new(OBSERVED_TICK)),
        "and it is still as old as the claim said, so a replica-age readout stays honest"
    );
    assert_ne!(
        state(&whole_population, REMOTE),
        installed,
        "non-vacuity: the existing whole-population step does advance that same replica by \
         its own velocity, which is exactly the behaviour change converging `advance` would \
         otherwise have made"
    );

    assert_ne!(
        state(&predicting, LOCAL),
        local(),
        "the named entity did step"
    );
    assert_eq!(
        predicting.observed_tick(LOCAL),
        Some(Tick::new(FIRST_TICK + 4)),
        "and it alone carries the post-tick observation stamp"
    );
    assert_eq!(predicting.next_tick(), Tick::new(FIRST_TICK + 4));
}

#[test]
fn a_prediction_set_naming_the_whole_population_matches_the_existing_step() {
    let mut named = joined_session();
    let mut whole = joined_session();

    let named_report = named.step_predicted(
        TickCount::new(3),
        &PredictionSet::only([LOCAL, REMOTE]),
        &mut HostRoutedTick,
    );
    let whole_report = whole.step(TickCount::new(3));

    assert_eq!(
        named_report.state_hashes, whole_report.state_hashes,
        "naming every entity steps them in the same canonical order, tick ascending then \
         PersistId ascending, and produces the same hashes"
    );
    assert_eq!(named_report.neighbor_frames, whole_report.neighbor_frames);
    assert_eq!(named.collect_output_bytes(), whole.collect_output_bytes());
    assert_eq!(named.next_tick(), whole.next_tick());
    assert_eq!(
        named.observed_tick(REMOTE),
        whole.observed_tick(REMOTE),
        "an entity the set names is stamped exactly as the whole-population path stamps it"
    );
}

#[test]
fn a_prediction_set_naming_nobody_advances_only_the_clock() {
    let mut host = joined_session();
    let before = host.collect_output_bytes();

    let report = host.step_predicted(
        TickCount::new(2),
        &PredictionSet::only([]),
        &mut HostRoutedTick,
    );

    assert!(report.state_hashes.is_empty());
    assert_eq!(host.collect_output_bytes(), before);
    assert_eq!(
        host.next_tick(),
        Tick::new(FIRST_TICK + 2),
        "an empty naming is an instruction, not the absence of one: `PredictionSet::only([])` \
         steps nobody, where `PredictionSet::everything()` steps all"
    );
}

#[test]
fn a_named_entity_the_host_does_not_hold_is_skipped() {
    let mut host = joined_session();
    let absent = PersistId::new(404);

    let report = host.step_predicted(
        TickCount::new(1),
        &PredictionSet::only([LOCAL, absent]),
        &mut HostRoutedTick,
    );

    assert_eq!(report.state_hashes.len(), 1);
    assert_eq!(report.state_hashes[0].entity, LOCAL);
    assert_eq!(host.state_bytes(absent), None);
}

// ── Capability 2: neighbour frames surviving the seam ─────────────────────

#[test]
fn a_step_report_carries_the_neighbour_frames_the_witness_log_needs() {
    let mut host = joined_session();

    let report = host.step_predicted(
        TickCount::new(1),
        &PredictionSet::just(LOCAL),
        &mut CampaignTick::default(),
    );

    assert_eq!(
        report.neighbor_frames.len(),
        1,
        "one stepped entity read one neighbour, so one record set"
    );
    let stepped = &report.neighbor_frames[0];
    assert_eq!(stepped.entity, LOCAL);
    assert_eq!(stepped.tick, Tick::new(FIRST_TICK));
    assert_eq!(
        stepped.frames,
        vec![NeighborFrame {
            neighbor: REMOTE,
            observed_tick: Tick::new(OBSERVED_TICK),
            state: Some(replica().to_canonical()),
        }],
        "the frame names the neighbour, the tick the reader actually held it at — not the \
         reading tick — and its canonical bytes. Retaining the stamp is what closes the \
         honest-replication-lag ambiguity, so a driver logging from the seam folds the same \
         records as one logging from `TickOutcome::neighbor_frames` directly"
    );
}

#[test]
fn neighbour_frames_survive_a_multi_tick_call_in_execution_order() {
    let mut host = joined_session();

    let report = host.step(TickCount::new(2));

    let addressed: Vec<_> = report
        .neighbor_frames
        .iter()
        .map(|stepped| (stepped.entity, stepped.tick))
        .collect();
    assert_eq!(
        addressed,
        vec![
            (LOCAL, Tick::new(FIRST_TICK)),
            (REMOTE, Tick::new(FIRST_TICK)),
            (LOCAL, Tick::new(FIRST_TICK + 1)),
            (REMOTE, Tick::new(FIRST_TICK + 1)),
        ],
        "tick ascending, then PersistId ascending within a tick: the same canonical order \
         `state_hashes` is in"
    );
    assert!(
        report
            .neighbor_frames
            .iter()
            .all(|stepped| !stepped.frames.is_empty()),
        "an entity that read nothing contributes no record set, so every set carries reads"
    );
}

// ── Capability 3: intercepting deliveries, and the sealed order vector ────

#[test]
fn a_participant_diverts_a_delivery_addressed_to_a_remote_authority() {
    // Both hosts step the whole population, so the *only* difference under
    // test is where the delivery went.
    let mut diverting = joined_session();
    let mut self_routing = joined_session();

    let mut driver = CampaignTick::default();
    diverting.step_predicted(TickCount::new(1), &PredictionSet::everything(), &mut driver);
    self_routing.step(TickCount::new(1));

    assert_eq!(
        driver.wire,
        vec![RoutedOverWire {
            source: LOCAL,
            recipient: REMOTE,
            input: SyntheticInput::Damage(1),
        }],
        "the driver saw both deliveries this tick produced and took exactly the one \
         addressed elsewhere. It reaches the participant named — who emitted it, who it is \
         addressed to, and the input itself — which is what `route_delivered_input` needs \
         to address an envelope"
    );

    // The driver kept the one addressed to itself and took the other. Step
    // once more so the queued half becomes input.
    diverting.step_predicted(TickCount::new(1), &PredictionSet::everything(), &mut driver);
    self_routing.step(TickCount::new(1));

    assert_eq!(
        state(&diverting, REMOTE).health,
        100,
        "the diverted delivery never entered this host's own buffer, so the replica took no \
         damage from it here — the remote authority owns that decision"
    );
    assert_eq!(
        state(&self_routing, REMOTE).health,
        99,
        "non-vacuity: the existing unconditional self-queue does apply it"
    );
    assert_eq!(
        state(&diverting, LOCAL).health,
        99,
        "and a delivery the participant handed back is queued exactly as before"
    );
    assert_eq!(state(&self_routing, LOCAL).health, 99);
}

#[test]
fn the_sealed_order_vector_reaches_the_driver_with_its_provenance() {
    let mut host = joined_session();

    // D46 clause (d): what another authority delivered is canonical input for
    // this tick and precedes the orders the player authored. The driver
    // submits in that order and the host seals in that order.
    host.submit_delivered_input(LOCAL, REMOTE, SyntheticInput::Damage(3));
    host.submit_input(LOCAL, SyntheticInput::Impulse([1, 0, 0]));

    let mut driver = CampaignTick::default();
    host.step_predicted(TickCount::new(1), &PredictionSet::just(LOCAL), &mut driver);

    assert_eq!(
        driver.sealed_ticks,
        vec![Tick::new(FIRST_TICK)],
        "the seal is observed once per tick, at S0"
    );
    assert_eq!(
        driver.witnessed,
        vec![
            WitnessedInput {
                recipient: LOCAL,
                input: SyntheticInput::Damage(3),
                origin: InputOrigin::Inbound { from: REMOTE },
            },
            WitnessedInput {
                recipient: LOCAL,
                input: SyntheticInput::Impulse([1, 0, 0]),
                origin: InputOrigin::Submitted,
            },
        ],
        "the exact vector the tick applied, in the order it applied it, each input paired \
         with the source a witness record would classify it under — delivered-first, then \
         the player's own"
    );
}

#[test]
fn an_adapter_delivery_the_host_kept_reads_back_as_delivered() {
    let mut host = joined_session();
    let mut driver = CampaignTick::default();

    // Tick one emits the strikes; the delivery addressed to LOCAL is handed
    // back and queued. Tick two seals it.
    host.step_predicted(TickCount::new(1), &PredictionSet::everything(), &mut driver);
    let sealed_by_tick_one = driver.witnessed.len();
    host.step_predicted(TickCount::new(1), &PredictionSet::everything(), &mut driver);

    assert_eq!(
        sealed_by_tick_one, 0,
        "an event's delivery cannot be input on the tick that emitted it (D43)"
    );
    assert_eq!(
        driver.witnessed,
        vec![WitnessedInput {
            recipient: LOCAL,
            input: SyntheticInput::Damage(1),
            origin: InputOrigin::Delivered { source: REMOTE },
        }],
        "a delivery this host's own adapter produced and kept names the entity whose event \
         produced it, which is a different provenance from one that arrived over a wire"
    );
}

// ── The three together, shaped like `advance` ─────────────────────────────

#[test]
fn one_call_expresses_what_the_campaign_driver_does_with_a_tick() {
    let mut host = joined_session();
    let mut driver = CampaignTick::default();
    let installed = replica();

    let mut predicted_per_tick = Vec::new();
    let mut witness_column = Vec::new();
    let mut refreshes = Vec::new();
    for tick in 0..6 {
        // `advance`'s ingest leg: a decoded claim is installed verbatim under
        // the tick the authority observed it at. Installing is the *only* way
        // this replica's bytes ever change.
        let refresh = Tick::new(FIRST_TICK + tick);
        host.install_state_observed(REMOTE, installed, refresh);
        refreshes.push(refresh);
        // `advance`'s own leg: the player's authored orders for this tick.
        host.submit_input(LOCAL, SyntheticInput::Impulse([0, 0, tick as i64]));

        let report =
            host.step_predicted(TickCount::new(1), &PredictionSet::just(LOCAL), &mut driver);

        // What `advance` counts as `report.predicted`.
        predicted_per_tick.push(report.state_hashes.len());
        // What `advance` feeds to `log_neighbor_frames` and `log_tick_hash`.
        for stepped in &report.neighbor_frames {
            witness_column.push(stepped.frames.clone());
        }
        assert_eq!(report.state_hashes.len(), report.neighbor_frames.len());
    }

    assert_eq!(
        predicted_per_tick,
        vec![1; 6],
        "one entity stepped per tick, counted from what the host reports rather than stated \
         as a constant — a tick that stepped nothing must read as zero"
    );
    assert_eq!(witness_column.len(), 6, "and six neighbour record sets");
    let expected: Vec<_> = refreshes
        .iter()
        .map(|refresh| {
            vec![NeighborFrame {
                neighbor: REMOTE,
                observed_tick: *refresh,
                state: Some(installed.to_canonical()),
            }]
        })
        .collect();
    assert_eq!(
        witness_column, expected,
        "each read carries the tick the reader actually held the replica at — the refresh \
         stamp, not the reading tick — which is the record replay verifies against"
    );

    assert_eq!(
        driver.witnessed.len(),
        6,
        "one authored order sealed per tick"
    );
    assert!(driver
        .witnessed
        .iter()
        .all(|witnessed| witnessed.origin == InputOrigin::Submitted));
    assert_eq!(
        driver.wire.len(),
        6,
        "and one delivery per tick left over the wire instead of entering this host"
    );
    assert!(driver
        .wire
        .iter()
        .all(|routed| routed.source == LOCAL && routed.recipient == REMOTE));

    assert_eq!(
        state(&host, REMOTE),
        installed,
        "and through all of it the replica never moved"
    );
}

#[test]
fn step_is_step_predicted_over_everything_routed_by_the_host() {
    let mut existing = joined_session();
    let mut spelled_out = joined_session();

    existing.submit_input(LOCAL, SyntheticInput::Impulse([0, 5, 0]));
    spelled_out.submit_input(LOCAL, SyntheticInput::Impulse([0, 5, 0]));

    let existing_report = existing.step(TickCount::new(5));
    let spelled_out_report = spelled_out.step_predicted(
        TickCount::new(5),
        &PredictionSet::everything(),
        &mut HostRoutedTick,
    );

    assert_eq!(existing_report, spelled_out_report);
    assert_eq!(
        existing.collect_output_bytes(),
        spelled_out.collect_output_bytes()
    );
    assert_eq!(existing.peek_event_bytes(), spelled_out.peek_event_bytes());
    assert!(
        !existing_report.state_hashes.is_empty()
            && !existing.peek_event_bytes().expect("events fit").is_empty(),
        "non-vacuity: this run stepped entities and emitted events"
    );
}

#[test]
fn a_prediction_set_says_which_entities_it_names() {
    assert!(PredictionSet::everything().is_everything());
    assert!(PredictionSet::everything().contains(REMOTE));
    assert!(!PredictionSet::just(LOCAL).is_everything());
    assert!(PredictionSet::just(LOCAL).contains(LOCAL));
    assert!(!PredictionSet::just(LOCAL).contains(REMOTE));
    assert!(PredictionSet::only([LOCAL, REMOTE]).contains(REMOTE));
    assert_eq!(PredictionSet::default(), PredictionSet::everything());
}
