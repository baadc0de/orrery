//! The macro form, driven end to end: `Executor` → signed log → `ReplayHarness`.
//!
//! Two rulesets compute the same thing over the same window. `Macro` writes its
//! audited observation with [`recorded_reads!`]; `Hand` writes it the way
//! `crates/orrery_games/src/regolith/visibility.rs` does today — a slot enum, a
//! `.map` over it, and a `view.neighbor(id).cloned()` in the body. They share a
//! `RulesetId`, so a window authored by one can be adjudicated by the other.
//!
//! What the tests establish, in order:
//!
//! 1. The two forms are byte-identical through the whole evidence path — same
//!    recorded reads, same neighbour frames, same state hashes, and a window
//!    authored by the macro form replays clean on the hand-written one.
//! 2. The reader's own identifier is refused, recorded, and framed absent
//!    (#820's divergence (a), which `StateView` already prevents and the macro
//!    keeps inside that path by construction).
//! 3. An observation staler than the ruleset's cap is hidden and framed absent,
//!    and one exactly at the cap is delivered (#820's divergence (b); the bound
//!    is a bound, not a switch).
//! 4. The declared cap is the slot count, and the ruleset reads it off the type
//!    instead of restating it.
//! 5. Slot declaration order is recorded first-read order, whatever order the
//!    inputs arrived in.
//! 6. A slot resolving to `None` records no read at all.

use orrery_core::{
    run_schedule, AuthoredFrame, CodecError, CoreCodec, Executor, InputLogProducer, Observation,
    OrderedInputs, Quantized, ReplayHarness, Ruleset, Scheduled, StateView, StepCtx, StepOutput,
    TickRng,
};
use orrery_protocol::{LogFrame, NodeId, PersistId, RulesetId, Tick, UniverseSeed};
use orrery_rules_dsl::{canonical_schedule, recorded_reads, RecordedReads};

// ── the demo game ───────────────────────────────────────────────────────

/// Own state. Two integers, so every assertion below is exact rather than
/// within a band: `hp` accumulates what the reads delivered and `reads_seen`
/// counts how many slots were answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Body {
    hp: i64,
    reads_seen: i64,
}

impl CoreCodec for Body {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.hp.to_le_bytes());
        out.extend_from_slice(&self.reads_seen.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let raw: [u8; 16] = bytes.try_into().map_err(|_| CodecError("bad length"))?;
        Ok(Self {
            hp: i64::from_le_bytes(raw[..8].try_into().expect("eight bytes")),
            reads_seen: i64::from_le_bytes(raw[8..].try_into().expect("eight bytes")),
        })
    }
}

impl Quantized for Body {
    fn quantize(&mut self) {}
}

/// Regolith's two claim shapes, reduced to what the read set needs: a cover
/// claim naming two entities and a collision claim naming one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Order {
    Consult { locker: PersistId, rock: PersistId },
    Collide { other: PersistId },
}

impl CoreCodec for Order {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Consult { locker, rock } => {
                out.push(0);
                out.extend_from_slice(&locker.0.to_le_bytes());
                out.extend_from_slice(&rock.0.to_le_bytes());
            }
            Self::Collide { other } => {
                out.push(1);
                out.extend_from_slice(&other.0.to_le_bytes());
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let id = |slice: &[u8]| -> Result<PersistId, CodecError> {
            let raw: [u8; 8] = slice.try_into().map_err(|_| CodecError("bad id"))?;
            Ok(PersistId::new(u64::from_le_bytes(raw)))
        };
        match bytes.first() {
            Some(0) if bytes.len() == 17 => Ok(Self::Consult {
                locker: id(&bytes[1..9])?,
                rock: id(&bytes[9..])?,
            }),
            Some(1) if bytes.len() == 9 => Ok(Self::Collide {
                other: id(&bytes[1..])?,
            }),
            _ => Err(CodecError("bad order")),
        }
    }
}

/// What the fold produced, emitted so the tick has an event to order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tally(i64);

impl CoreCodec for Tally {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let raw: [u8; 8] = bytes.try_into().map_err(|_| CodecError("bad length"))?;
        Ok(Self(i64::from_le_bytes(raw)))
    }
}

/// Tick-scoped scratch, reset at the top of every entity's tick.
#[derive(Debug, Default, PartialEq, Eq)]
struct Locals {
    sum: i64,
    delivered: i64,
}

/// Ticks of replication lag this ruleset will consume. Two, so the tests can
/// stand on both sides of it.
const STALENESS_CAP: u64 = 2;

const RULES: RulesetId = RulesetId {
    version: 1,
    digest: [0x5D; 32],
};

// ── the macro form ──────────────────────────────────────────────────────

recorded_reads! {
    /// The audited claims read, declared rather than written.
    pub CLAIM_READS {
        rules:   Macro,
        locals:  Locals,
        system:  "verify-claims",
        targets: ClaimTargets,
        frames:  ClaimFrames,
        slots:   [
            /// The craft whose lock a cover claim challenges.
            cover_locker,
            /// The rock a cover claim names as occluder.
            cover_rock,
            /// The counterparty a collision claim names.
            collision,
        ],
        resolve: claim_targets,
        apply:   fold_claims,
    }
}

/// Who to read. No `StateView` in the signature, so this cannot read anything;
/// it can only name.
fn claim_targets(
    _reader: PersistId,
    _own: &Body,
    inputs: &OrderedInputs<'_, Order>,
) -> ClaimTargets {
    let mut targets = ClaimTargets::default();
    for order in inputs.iter() {
        match order {
            Order::Consult { locker, rock } if targets.cover_locker.is_none() => {
                targets.cover_locker = Some(*locker);
                targets.cover_rock = Some(*rock);
            }
            Order::Collide { other } if targets.collision.is_none() => {
                targets.collision = Some(*other);
            }
            _ => {}
        }
    }
    targets
}

/// What came back. Owns `Option<Body>` values and no handle to anything else,
/// so there is no second read to be had here either.
fn fold_claims(
    _reader: PersistId,
    _own: &Body,
    _targets: &ClaimTargets,
    frames: &ClaimFrames,
    _inputs: &OrderedInputs<'_, Order>,
    locals: &mut Locals,
) {
    for body in [&frames.cover_locker, &frames.cover_rock, &frames.collision]
        .into_iter()
        .flatten()
    {
        locals.sum += body.hp;
        locals.delivered += 1;
    }
}

/// The one ordinary system: fold the tick's reads into own state.
fn accrue(state: &mut Body, cx: &mut StepCtx<'_, Macro, Locals>) {
    let (sum, delivered) = (cx.locals.sum, cx.locals.delivered);
    state.hp += sum;
    state.reads_seen += delivered;
    cx.emit(Tally(sum));
}

canonical_schedule! {
    rules:    Macro,
    locals:   Locals,
    runnable: pub MACRO_SCHEDULE,
    declared: pub MACRO_CANONICAL,
    observe:  "observe" => [ "verify-claims" => CLAIM_READS ],
    stages:   [ "fold" => [ "accrue" => accrue ] ],
    edges:    [ "verify-claims" -> "accrue" ],
}

/// The ruleset whose audited read is declared.
#[derive(Debug, Clone, Copy)]
struct Macro;

impl Ruleset for Macro {
    type CoreState = Body;
    type CoreInput = Order;
    type CoreEvent = Tally;
    fn id(&self) -> RulesetId {
        RULES
    }
    /// **Derived, not restated.** The cap is the declared slot count, read off
    /// the generated frame type. It cannot drift from the reads.
    fn max_neighbor_reads(&self) -> usize {
        <ClaimFrames as RecordedReads>::MAX_NEIGHBOR_READS
    }
    fn max_neighbor_staleness_ticks(&self) -> u64 {
        STALENESS_CAP
    }
    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> StepOutput<Tally> {
        run_schedule(self, view, inputs, rng)
    }
}

impl Scheduled for Macro {
    type Locals = Locals;
    fn schedule(&self) -> &'static orrery_core::Schedule<Self, Locals> {
        &MACRO_SCHEDULE
    }
}

// ── the hand-written twin, in today's shape ─────────────────────────────

#[derive(Clone, Copy)]
enum Slot {
    CoverLocker,
    CoverRock,
    Collision,
}

const SLOTS: [Slot; 3] = [Slot::CoverLocker, Slot::CoverRock, Slot::Collision];

impl Slot {
    fn target(
        self,
        cover: Option<(PersistId, PersistId)>,
        collision: Option<PersistId>,
    ) -> Option<PersistId> {
        match self {
            Self::CoverLocker => cover.map(|(locker, _)| locker),
            Self::CoverRock => cover.map(|(_, rock)| rock),
            Self::Collision => collision,
        }
    }
}

/// `verify_claims`' shape, transcribed: the body holds the `StateView` and the
/// read is a line inside it.
fn hand_observe(
    view: &mut StateView<'_, Body>,
    inputs: &OrderedInputs<'_, Order>,
    locals: &mut Locals,
) {
    let mut cover: Option<(PersistId, PersistId)> = None;
    let mut collision: Option<PersistId> = None;
    for order in inputs.iter() {
        match order {
            Order::Consult { locker, rock } if cover.is_none() => cover = Some((*locker, *rock)),
            Order::Collide { other } if collision.is_none() => collision = Some(*other),
            _ => {}
        }
    }
    let frames = SLOTS.map(|slot| {
        slot.target(cover, collision)
            .and_then(|id| view.neighbor(id).cloned())
    });
    for body in frames.iter().flatten() {
        locals.sum += body.hp;
        locals.delivered += 1;
    }
}

fn hand_accrue(state: &mut Body, cx: &mut StepCtx<'_, Hand, Locals>) {
    let (sum, delivered) = (cx.locals.sum, cx.locals.delivered);
    state.hp += sum;
    state.reads_seen += delivered;
    cx.emit(Tally(sum));
}

static HAND_SCHEDULE: orrery_core::Schedule<Hand, Locals> = orrery_core::Schedule {
    observe_stage: orrery_core::StageName("observe"),
    observe: &[Observation {
        name: orrery_core::SystemName("verify-claims"),
        run: hand_observe,
    }],
    stages: &[orrery_core::Stage {
        name: orrery_core::StageName("fold"),
        systems: &[orrery_core::System {
            name: orrery_core::SystemName("accrue"),
            run: hand_accrue,
        }],
    }],
};

/// The same rules, written the way the tree writes them today.
#[derive(Debug, Clone, Copy)]
struct Hand;

impl Ruleset for Hand {
    type CoreState = Body;
    type CoreInput = Order;
    type CoreEvent = Tally;
    fn id(&self) -> RulesetId {
        RULES
    }
    /// Restated by hand — the number this spike is about. Nothing holds it to
    /// `SLOTS.len()` but the author's memory and a reviewer's eye.
    fn max_neighbor_reads(&self) -> usize {
        3
    }
    fn max_neighbor_staleness_ticks(&self) -> u64 {
        STALENESS_CAP
    }
    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> StepOutput<Tally> {
        run_schedule(self, view, inputs, rng)
    }
}

impl Scheduled for Hand {
    type Locals = Locals;
    fn schedule(&self) -> &'static orrery_core::Schedule<Self, Locals> {
        &HAND_SCHEDULE
    }
}

// ── fixtures ────────────────────────────────────────────────────────────

const SEED: UniverseSeed = UniverseSeed([0x2B; 32]);
const READER: PersistId = PersistId::new(1);
const LOCKER: PersistId = PersistId::new(2);
const ROCK: PersistId = PersistId::new(3);
const OTHER: PersistId = PersistId::new(4);
const FIRST_TICK: u64 = 10;
const WINDOW: u64 = 3;

fn authority_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[0x11; 32])
}

fn body(hp: i64) -> Body {
    Body { hp, reads_seen: 0 }
}

/// The inputs the reader is fed on each tick of the window.
fn window_inputs() -> Vec<Vec<Order>> {
    (0..WINDOW)
        .map(|_| {
            vec![
                Order::Consult {
                    locker: LOCKER,
                    rock: ROCK,
                },
                Order::Collide { other: OTHER },
            ]
        })
        .collect()
}

/// Populate an executor with the reader and three neighbours, each stamped
/// fresh at the window's first tick.
fn populate<R: Ruleset<CoreState = Body>>(executor: &mut Executor<R>) {
    executor.insert_observed(READER, body(100), Tick::new(FIRST_TICK));
    executor.insert_observed(LOCKER, body(5), Tick::new(FIRST_TICK));
    executor.insert_observed(ROCK, body(7), Tick::new(FIRST_TICK));
    executor.insert_observed(OTHER, body(11), Tick::new(FIRST_TICK));
}

/// What one authored window produced, per tick.
struct Window {
    reads: Vec<Vec<PersistId>>,
    present: Vec<Vec<bool>>,
    observed: Vec<Vec<Tick>>,
    hashes: Vec<[u8; 32]>,
    final_state: Body,
}

/// Author a window on `ruleset`, returning both the per-tick record and the
/// signed evidence a replay would be handed.
fn author<R: Ruleset<CoreState = Body, CoreInput = Order, CoreEvent = Tally>>(
    ruleset: R,
    inputs: &[Vec<Order>],
) -> (Window, Vec<LogFrame>, orrery_protocol::StateClaim, Vec<u8>) {
    let mut executor = Executor::new(ruleset, SEED);
    populate(&mut executor);
    let snapshot = executor
        .state(READER)
        .expect("the reader is installed")
        .to_canonical();

    let mut producer = InputLogProducer::new(
        authority_key(),
        READER,
        RULES,
        FIRST_TICK,
        WINDOW,
        u16::try_from(WINDOW).expect("a three-tick frame"),
    );
    let anchor = producer.anchor(FIRST_TICK, executor.state(READER).expect("installed"));

    let mut window = Window {
        reads: Vec::new(),
        present: Vec::new(),
        observed: Vec::new(),
        hashes: Vec::new(),
        final_state: Body::default(),
    };
    let mut frames = Vec::new();
    for (offset, tick_inputs) in inputs.iter().enumerate() {
        let tick = FIRST_TICK + offset as u64;
        producer.log_inputs(tick, tick_inputs);
        let outcome = executor
            .step_entity(READER, Tick::new(tick), tick_inputs)
            .expect("the reader is installed");
        producer.log_neighbor_frames(tick, &outcome.neighbor_frames);
        producer.log_tick_hash(outcome.state_hash);
        window.reads.push(outcome.neighbor_reads.clone());
        window.present.push(
            outcome
                .neighbor_frames
                .iter()
                .map(|f| f.state.is_some())
                .collect(),
        );
        window.observed.push(
            outcome
                .neighbor_frames
                .iter()
                .map(|f| f.observed_tick)
                .collect(),
        );
        window.hashes.push(outcome.state_hash);
        if let Some(AuthoredFrame { frame, .. }) = producer.cut_frame(tick) {
            frames.push(frame);
        }
    }
    window.final_state = executor.state(READER).expect("installed").clone();
    (window, frames, anchor, snapshot)
}

fn node_id() -> NodeId {
    authority_key().public()
}

// ── 1. the two forms are the same window ────────────────────────────────

#[test]
fn the_macro_form_and_the_hand_written_observation_produce_identical_windows() {
    // If these ever differ, the macro is not a rewrite of today's shape — it is
    // a different ruleset wearing the same name, and every other measurement
    // here would be about the wrong thing.
    let inputs = window_inputs();
    let (declared, _, _, _) = author(Macro, &inputs);
    let (hand, _, _, _) = author(Hand, &inputs);

    assert_eq!(declared.reads, hand.reads, "recorded read sequences");
    assert_eq!(declared.present, hand.present, "frame present bits");
    assert_eq!(declared.observed, hand.observed, "frame observation ticks");
    assert_eq!(declared.hashes, hand.hashes, "per-tick state hashes");
    assert_eq!(declared.final_state, hand.final_state, "final own state");
    // 100 + three ticks of (5 + 7 + 11).
    assert_eq!(declared.final_state.hp, 169);
    assert_eq!(declared.final_state.reads_seen, 9);
}

#[test]
fn a_window_authored_by_the_macro_form_replays_clean_on_the_hand_written_one() {
    // The end-to-end leg: the macro form's signed evidence, re-executed by an
    // adjudicator built from the hand-written ruleset. A `Confirms` here is the
    // statement that the two produce the same bytes through the whole path —
    // recording, framing, chain folding, signature and re-execution — and not
    // merely the same numbers in memory.
    let inputs = window_inputs();
    let (window, frames, anchor, snapshot) = author(Macro, &inputs);

    let mut harness = ReplayHarness::new(Hand, SEED);
    harness
        .load_claimed_snapshot(&anchor, &snapshot)
        .expect("the anchor commits to the snapshot it was cut from");
    let trace = harness
        .replay(
            &frames,
            node_id(),
            (Tick::new(FIRST_TICK), Tick::new(FIRST_TICK + WINDOW)),
            &[],
        )
        .expect("a well-formed window replays");

    for (offset, expected) in window.hashes.iter().enumerate() {
        let tick = Tick::new(FIRST_TICK + offset as u64);
        assert_eq!(
            trace.at(tick),
            Some(*expected),
            "tick {tick:?} reproduces the authority's hash",
        );
    }
}

#[test]
fn deleting_a_slot_changes_the_window() {
    // Not vacuous: if the reads did not reach the state, every assertion above
    // would hold for a ruleset that read nothing. Dropping the collision slot's
    // contribution moves the total by exactly that neighbour's hp per tick.
    let inputs: Vec<Vec<Order>> = (0..WINDOW)
        .map(|_| {
            vec![Order::Consult {
                locker: LOCKER,
                rock: ROCK,
            }]
        })
        .collect();
    let (window, _, _, _) = author(Macro, &inputs);
    assert_eq!(window.reads, vec![vec![LOCKER, ROCK]; WINDOW as usize]);
    // 100 + three ticks of (5 + 7): the 11 the collision slot carried is gone.
    assert_eq!(window.final_state.hp, 136);
}

// ── 2. the reader's own identifier (#820 divergence (a)) ────────────────

#[test]
fn the_readers_own_id_is_refused_recorded_and_framed_absent() {
    // `StateView::neighbor` refuses by identity (`ruleset.rs:176`) and
    // `canonical_step` frames it absent (`executor.rs:485-518`). The macro form
    // routes the read there, so the refusal is inherited rather than
    // reimplemented — which is exactly what #820's `OrderedQuery` could not do,
    // because it was never told who was reading.
    let inputs = vec![vec![
        Order::Consult {
            locker: READER,
            rock: ROCK,
        },
        Order::Collide { other: OTHER },
    ]];
    let (window, _, _, _) = author(Macro, &inputs);

    assert_eq!(
        window.reads[0],
        vec![READER, ROCK, OTHER],
        "the ask is recorded"
    );
    assert_eq!(
        window.present[0],
        vec![false, true, true],
        "the reader's own row is framed absent even though the snapshot holds it",
    );
    // 100 + 7 + 11. The reader's own 100 is not counted twice; #820 measured
    // 207 where an honest miss-then-hit gives 118.
    assert_eq!(window.final_state.hp, 118);
    assert_eq!(window.final_state.reads_seen, 2);
}

// ── 3. staleness (#820 divergence (b)) ──────────────────────────────────

/// Author one tick with `OTHER` stamped `age` ticks before the reader's tick.
fn stale_by(age: u64) -> Window {
    let mut executor = Executor::new(Macro, SEED);
    executor.insert_observed(READER, body(100), Tick::new(FIRST_TICK));
    executor.insert_observed(LOCKER, body(5), Tick::new(FIRST_TICK));
    executor.insert_observed(ROCK, body(7), Tick::new(FIRST_TICK));
    executor.insert_observed(OTHER, body(11), Tick::new(FIRST_TICK - age));

    let tick_inputs = vec![
        Order::Consult {
            locker: LOCKER,
            rock: ROCK,
        },
        Order::Collide { other: OTHER },
    ];
    let outcome = executor
        .step_entity(READER, Tick::new(FIRST_TICK), &tick_inputs)
        .expect("the reader is installed");
    Window {
        reads: vec![outcome.neighbor_reads.clone()],
        present: vec![outcome
            .neighbor_frames
            .iter()
            .map(|f| f.state.is_some())
            .collect()],
        observed: vec![outcome
            .neighbor_frames
            .iter()
            .map(|f| f.observed_tick)
            .collect()],
        hashes: vec![outcome.state_hash],
        final_state: executor.state(READER).expect("installed").clone(),
    }
}

#[test]
fn an_observation_past_the_staleness_cap_is_hidden_and_framed_absent() {
    // The serious half of #820. A query that did not know the tick returned the
    // hidden row while the log said it was never delivered: sequences matched,
    // hashes diverged, and a well-formed window convicted an honest authority.
    // Here the read *is* `StateView::neighbor`, so `ruleset.rs:192`'s
    // `checked_sub` is the one implementation and there is nothing to disagree
    // with it.
    let stale = stale_by(STALENESS_CAP + 1);
    assert_eq!(
        stale.reads[0],
        vec![LOCKER, ROCK, OTHER],
        "the ask is still recorded"
    );
    assert_eq!(stale.present[0], vec![true, true, false]);
    assert_eq!(
        stale.final_state.hp, 112,
        "100 + 5 + 7, without the hidden 11"
    );
    assert_eq!(stale.final_state.reads_seen, 2);
}

#[test]
fn an_observation_exactly_at_the_staleness_cap_is_delivered() {
    // The bound is a bound, not a switch: without this the test above would
    // also pass for a form that refused every lagged observation.
    let fresh = stale_by(STALENESS_CAP);
    assert_eq!(fresh.present[0], vec![true, true, true]);
    assert_eq!(fresh.final_state.hp, 123, "100 + 5 + 7 + 11");
    assert_eq!(fresh.final_state.reads_seen, 3);
}

// ── 4. the cap is derived ───────────────────────────────────────────────

#[test]
fn the_declared_cap_is_the_slot_count_and_the_ruleset_reads_it_off_the_type() {
    assert_eq!(<ClaimFrames as RecordedReads>::MAX_NEIGHBOR_READS, 3);
    assert_eq!(
        <ClaimFrames as RecordedReads>::SLOT_NAMES,
        ["cover_locker", "cover_rock", "collision"],
    );
    assert_eq!(
        Macro.max_neighbor_reads(),
        <ClaimFrames as RecordedReads>::MAX_NEIGHBOR_READS,
        "the ruleset's cap is the declaration, not a number beside it",
    );
    // The number the hand-written twin restates. Equal today; nothing but
    // review keeps it equal tomorrow, which is the whole point.
    assert_eq!(Hand.max_neighbor_reads(), 3);
}

#[test]
fn a_window_never_carries_more_frames_than_the_declared_cap() {
    // `replay.rs:275-278` refuses a window with more frames per tick than the
    // cap. With the cap derived from the slots, that check cannot be tripped by
    // a rule that reads more than it declared — there is no way to write one.
    let inputs = window_inputs();
    let (window, _, _, _) = author(Macro, &inputs);
    for reads in &window.reads {
        assert!(reads.len() <= Macro.max_neighbor_reads());
    }
}

// ── 5. slot order is read order ─────────────────────────────────────────

#[test]
fn slot_declaration_order_is_recorded_first_read_order() {
    // The inputs arrive collision-first; the recorded order is still the
    // declared slot order, because the generated struct literal fixes it. A
    // resolver cannot reorder the read set, only populate it.
    let inputs = vec![vec![
        Order::Collide { other: OTHER },
        Order::Consult {
            locker: LOCKER,
            rock: ROCK,
        },
    ]];
    let (window, _, _, _) = author(Macro, &inputs);
    assert_eq!(window.reads[0], vec![LOCKER, ROCK, OTHER]);
}

// ── 6. an unresolved slot reads nothing ─────────────────────────────────

#[test]
fn a_slot_resolving_to_none_records_no_read() {
    // A declared slot is capacity, not an obligation: an unnamed slot produces
    // no frame at all, so the cap stays an upper bound rather than a quota the
    // log has to fill.
    let inputs = vec![vec![Order::Collide { other: OTHER }]];
    let (window, _, _, _) = author(Macro, &inputs);
    assert_eq!(window.reads[0], vec![OTHER]);
    assert_eq!(window.present[0], vec![true]);
    assert_eq!(window.final_state.hp, 111);
}

// ── the derived manifest ────────────────────────────────────────────────

#[test]
fn the_declared_schedule_is_the_table_that_runs() {
    // `the_declared_schedule_matches_the_table_that_runs` in
    // `crates/orrery_games/src/regolith/mod.rs:1923` exists because the two
    // tables are written twice. Here they come from one list, so this test is
    // a restatement rather than a guard — which is the improvement.
    let running: Vec<(String, Vec<String>)> = MACRO_SCHEDULE
        .stages_with_systems()
        .into_iter()
        .map(|(stage, systems)| {
            (
                stage.0.to_owned(),
                systems.into_iter().map(|s| s.0.to_owned()).collect(),
            )
        })
        .collect();
    let declared: Vec<(String, Vec<String>)> = MACRO_CANONICAL
        .stages
        .iter()
        .map(|stage| {
            (
                stage.id.0.to_owned(),
                stage.systems.iter().map(|s| s.0.to_owned()).collect(),
            )
        })
        .collect();
    assert_eq!(running, declared);
    assert_eq!(MACRO_SCHEDULE.duplicate_system_name(), None);
    for edge in MACRO_CANONICAL.ordering_edges {
        let before = MACRO_SCHEDULE
            .position_of(orrery_core::SystemName(edge.before.0))
            .expect("a declared edge names a system that runs");
        let after = MACRO_SCHEDULE
            .position_of(orrery_core::SystemName(edge.after.0))
            .expect("a declared edge names a system that runs");
        assert!(before < after);
    }
}
