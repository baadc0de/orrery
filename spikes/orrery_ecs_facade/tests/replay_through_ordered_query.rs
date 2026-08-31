//! Spike (#793), the half #815 could not close: a `Ruleset` whose `step`
//! reaches neighbours through [`OrderedQuery`] instead of
//! `StateView::neighbor`, driven by `Executor` and adjudicated by
//! `ReplayHarness`.
//!
//! #815 stated its own limit plainly — *"the `get` compatibility is a
//! demonstrated shape match against quoted code, not a replay that ran"*. This
//! file is the replay that ran, and it both confirms and refutes.
//!
//! # The wiring, and why it is shaped like this
//!
//! `Ruleset::step` (`crates/orrery_core/src/ruleset.rs:338`) hands the rule a
//! `&mut StateView<'_, CoreState>` and nothing else. There is no channel in
//! that signature for a system param, and `StateView`'s neighbour snapshot is
//! a private `&BTreeMap` reachable only through `StateView::neighbor`
//! (`ruleset.rs:176`), which is itself the recording call. So a query cannot
//! be *sourced from* a `StateView`. What can be done without touching
//! `orrery_core` is the inverse:
//!
//! * the rules object owns the `World` (interior-mutable, because `step` takes
//!   `&self`), and a [`MirrorBackend`] keeps it equal to the executor's store
//!   by mirroring every `TickBackend` write into it;
//! * `step` runs the game's rule through `OrderedQuery` against that `World`;
//! * `step` then **drains** `AccessLog::neighbor_reads()` into
//!   `StateView::neighbor`, one call per id, in order.
//!
//! `StateView` stays the ledger; `OrderedQuery` becomes the read path. That is
//! the arrangement under test, and it is the most favourable one available —
//! every divergence below survives it.
//!
//! # What this is not
//!
//! Propose-only. Nothing shipped depends on this file. `Ruleset` is
//! implemented in `tests/`, not in `src/`, so `scripts/core-gates.sh`'s role
//! discovery — which reads library sources only — does not gate this crate,
//! and no gate is edited. Regolith is not migrated and `orrery_core` is not
//! amended.

use std::sync::Mutex;

use bevy_ecs::prelude::*;
use bevy_ecs::system::RunSystemOnce;
use facade_game::Rock;
use orrery_core::log::{fold_all, sign_claim, sign_frame, HeadTransition};
use orrery_core::{
    CodecError, CoreCodec, Executor, OrderedInputs, ReplayError, ReplayHarness, Ruleset, StateView,
    StepOutput, TickBackend, TickOutcome, TickRng,
};
use orrery_ecs_facade::{AccessLog, KeyIndex, ObservedAt, OrderedQuery, PersistKey, ReadWindow};
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick, UniverseSeed,
};

// ── The ruleset ─────────────────────────────────────────────────────────────

const RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0xEC; 32],
};

/// One logged input: the id this tick asks the rule to consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Consult(PersistId);

impl CoreCodec for Consult {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0 .0.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let raw: [u8; 8] = bytes
            .try_into()
            .map_err(|_| CodecError("a consult is eight bytes"))?;
        Ok(Self(PersistId(u64::from_le_bytes(raw))))
    }
}

/// This ruleset emits nothing; cross-entity effects are out of scope here.
struct NoEvent;

impl CoreCodec for NoEvent {
    fn encode(&self, _out: &mut Vec<u8>) {}
    fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self)
    }
}

/// This tick's consult list, handed to the game rule as plain data.
#[derive(Resource, Default)]
struct Targets(Vec<PersistId>);

/// The stepping entity's own state, in and out of the `World`.
///
/// A resource rather than a component read through the query, deliberately:
/// own state is not a neighbour observation, and routing it through the query
/// would be the very confusion the `neighbor`/`own` split exists to prevent.
#[derive(Resource)]
struct Own(Rock);

/// The system. Written **here**, on the facade's side, because `Res` and
/// `ResMut` are not on the curated surface and `#[derive(SystemParam)]` does
/// not compile in a game crate at all (`facade_game::system_param_is_refused`).
/// The game crate contributes `erode`, a plain function.
fn rule_system(
    mut rocks: OrderedQuery<&'static Rock>,
    targets: Res<Targets>,
    mut own: ResMut<Own>,
) {
    facade_game::erode(&mut own.0, &mut rocks, &targets.0);
}

/// The rules object, which owns the `World` the query reads.
///
/// `Ruleset: Send + Sync + 'static` and `step` takes `&self`, so the `World`
/// is behind a `Mutex`. That is a spike's answer to a signature problem, and
/// it is also a finding: on the bevy-native path the `World` *is* the store
/// and `step` is a system over it, at which point no mutex is involved. Here
/// it is the only way to get a system param into a `&mut StateView`-shaped
/// call without amending `orrery_core`.
struct OrderedQueryRules {
    world: Mutex<World>,
    enforcement: Enforcement,
}

/// Whether the host stamps a [`ReadWindow`] before each step.
///
/// `Off` is #815's `OrderedQuery` exactly: no own-identity refusal and no
/// staleness bound. It is kept runnable so the two refutations below stay
/// measurements rather than recollections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enforcement {
    Off,
    On,
}

impl OrderedQueryRules {
    fn new(enforcement: Enforcement) -> Self {
        let mut world = World::new();
        world.insert_resource(AccessLog::default());
        world.insert_resource(KeyIndex::default());
        world.insert_resource(Targets::default());
        world.insert_resource(ReadWindow::open());
        Self {
            world: Mutex::new(world),
            enforcement,
        }
    }

    /// Stamp the window this entity-tick reads through.
    ///
    /// The host does this and not the rule, because `Ruleset::step`
    /// (`crates/orrery_core/src/ruleset.rs:338`) is handed no tick — the one
    /// fact both of `StateView::neighbor`'s refusals need.
    fn stamp(&self, reader: PersistId, tick: Tick) {
        let window = match self.enforcement {
            Enforcement::Off => ReadWindow::open(),
            Enforcement::On => {
                ReadWindow::observed(reader, tick, self.max_neighbor_staleness_ticks())
            }
        };
        let mut world = self.world.lock().expect("no panics inside the lock");
        world.insert_resource(window);
    }

    /// Spawn or update the row a `PersistId` names, with its observation tick.
    fn upsert(&self, key: PersistId, state: Rock, observed: Tick) {
        let mut world = self.world.lock().expect("no panics inside the lock");
        match world.resource::<KeyIndex>().entity(key) {
            Some(entity) if world.get_entity(entity).is_ok() => {
                world
                    .entity_mut(entity)
                    .insert((state, ObservedAt(observed)));
            }
            _ => {
                let entity = world
                    .spawn((PersistKey(key), ObservedAt(observed), state))
                    .id();
                world.resource_mut::<KeyIndex>().insert(key, entity);
            }
        }
    }

    /// Remove the row a `PersistId` names.
    fn forget(&self, key: PersistId) {
        let mut world = self.world.lock().expect("no panics inside the lock");
        if let Some(entity) = world.resource::<KeyIndex>().entity(key) {
            world.despawn(entity);
        }
        world.resource_mut::<KeyIndex>().remove(key);
    }
}

impl Ruleset for OrderedQueryRules {
    type CoreState = Rock;
    type CoreInput = Consult;
    type CoreEvent = NoEvent;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn max_neighbor_reads(&self) -> usize {
        4
    }

    fn max_neighbor_staleness_ticks(&self) -> u64 {
        5
    }

    fn step(
        &self,
        view: &mut StateView<'_, Rock>,
        inputs: &OrderedInputs<'_, Consult>,
        _rng: &mut TickRng,
    ) -> StepOutput<NoEvent> {
        let targets: Vec<PersistId> = inputs.iter().map(|consult| consult.0).collect();
        let (own_after, reads) = {
            let mut world = self.world.lock().expect("no panics inside the lock");
            let world = &mut *world;
            // One entity-tick, one log — the lifetime `StateView` itself has
            // (`crates/orrery_core/src/executor.rs:382` builds a fresh one per
            // entity-tick).
            world.resource_mut::<AccessLog>().clear();
            world.insert_resource(Targets(targets));
            world.insert_resource(Own(*view.own()));
            world
                .run_system_once(rule_system)
                .expect("the rule system is well-formed");
            let own_after = world.resource::<Own>().0;
            let reads = world.resource::<AccessLog>().neighbor_reads();
            (own_after, reads)
        };
        *view.own_mut() = own_after;
        // The drain. `StateView` remains the ledger the executor reads
        // (`executor.rs:387`, `view.recorded_reads()`), and the query supplies
        // what goes into it — deduplicated, first-mention order, produced by
        // `AccessLog::neighbor_reads`.
        for key in reads {
            let _ = view.neighbor(key);
        }
        StepOutput::default()
    }
}

// ── The backend that keeps the two stores equal ─────────────────────────────

/// `Executor`, plus a mirror of its store into the rules object's `World`.
///
/// Every `TickBackend` write is applied twice: once to the executor, whose
/// `BTreeMap` is what `canonical_step` frames neighbours from, and once to the
/// `World`, which is what the query reads. If the two ever disagreed the
/// experiment would be measuring the mirror rather than the query, so the
/// mirror is deliberately total: `insert_observed`, `take_state` and the
/// post-step own-state write-back.
struct MirrorBackend {
    inner: Executor<OrderedQueryRules>,
}

impl MirrorBackend {
    fn new(seed: UniverseSeed, enforcement: Enforcement) -> Self {
        Self {
            inner: Executor::new(OrderedQueryRules::new(enforcement), seed),
        }
    }
}

impl TickBackend<OrderedQueryRules> for MirrorBackend {
    fn ruleset(&self) -> &OrderedQueryRules {
        self.inner.ruleset()
    }

    fn insert_observed(&mut self, entity: PersistId, state: Rock, observed_tick: Tick) {
        self.inner.insert_observed(entity, state, observed_tick);
        self.inner.ruleset().upsert(entity, state, observed_tick);
    }

    fn take_state(&mut self, entity: PersistId) -> Option<Rock> {
        self.inner.ruleset().forget(entity);
        self.inner.take_state(entity)
    }

    fn state(&self, entity: PersistId) -> Option<&Rock> {
        self.inner.state(entity)
    }

    fn entities(&self) -> Vec<PersistId> {
        self.inner.entities().copied().collect()
    }

    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[Consult],
    ) -> Option<TickOutcome<NoEvent>> {
        // The host stamps the window, because the rule cannot: the tick is not
        // in `Ruleset::step`'s signature.
        self.inner.ruleset().stamp(entity, tick);
        let outcome = self.inner.step_entity(entity, tick, inputs)?;
        if let Some(after) = self.inner.state(entity).copied() {
            // `Executor::step_entity` stamps the post-step state at T+1
            // (`executor.rs:228`); the mirror says the same, or the two stores
            // would disagree about provenance.
            self.inner
                .ruleset()
                .upsert(entity, after, Tick::new(tick.0.saturating_add(1)));
        }
        Some(outcome)
    }
}

// ── The window ──────────────────────────────────────────────────────────────

const SUBJECT: PersistId = PersistId::new(77);
const NEIGHBOUR: PersistId = PersistId::new(78);
/// Never installed anywhere. The absent case, in the log and in the frames.
const GHOST: PersistId = PersistId::new(999);
const SEED: UniverseSeed = UniverseSeed([0x5A; 32]);
const T0: u64 = 6_000;
const TICKS: u64 = 3;

fn key(seed: u8) -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[seed; 32])
}

/// This tick's consults: a hit, a miss, and the hit again.
///
/// Three inputs, two distinct ids, in that order. The repeat is what makes the
/// dedup in `AccessLog::neighbor_reads` load-bearing rather than decorative,
/// and the miss is what makes the absent case travel through the frames.
fn consults() -> Vec<Consult> {
    vec![Consult(NEIGHBOUR), Consult(GHOST), Consult(NEIGHBOUR)]
}

/// How the neighbour was observed at a given tick, for the staleness cases.
#[derive(Clone, Copy)]
enum Observation {
    /// Stamped at the reader's own tick: fresh under any cap.
    Fresh,
    /// Stamped ten ticks back, against a cap of five: hidden by `StateView`.
    Stale,
}

/// What the authority produced over the window.
struct Produced {
    t0_claim: StateClaim,
    t0_snapshot: Vec<u8>,
    frames: Vec<LogFrame>,
    claimed_hashes: Vec<[u8; 32]>,
    /// The `neighbor_reads` sequence the query produced, per tick.
    reads_per_tick: Vec<Vec<PersistId>>,
    /// The `NeighborFrame` ids the authority logged, per tick — the sequence
    /// `replay.rs:325` will compare against.
    logged_per_tick: Vec<Vec<PersistId>>,
    /// Whether each logged frame carried state, per tick.
    presence_per_tick: Vec<Vec<bool>>,
}

/// Run the authority side: step the window on a live `MirrorBackend`, and
/// write down everything a signed log would carry.
fn produce(
    authority: &iroh_base::SecretKey,
    consults_at: &dyn Fn(u64) -> Vec<Consult>,
    observed: Observation,
    enforcement: Enforcement,
) -> Produced {
    let mut backend = MirrorBackend::new(SEED, enforcement);
    backend.insert(SUBJECT, Rock { hp: 100 });

    let t0_snapshot = backend.state(SUBJECT).expect("seeded").to_canonical();
    let mut t0_claim = StateClaim {
        entity: SUBJECT,
        chain_epoch: 0,
        tick: Tick::new(T0),
        input_head: ChainHash::EMPTY,
        state_hash: *blake3::hash(&t0_snapshot).as_bytes(),
        prev_claim: [0; 32],
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(authority, &mut t0_claim);

    let mut records = Vec::new();
    let mut claimed_hashes = Vec::new();
    let mut reads_per_tick = Vec::new();
    let mut logged_per_tick = Vec::new();
    let mut presence_per_tick = Vec::new();

    for offset in 0..TICKS {
        let tick = T0 + offset;
        // The neighbour is installed for the tick that reads it and removed
        // afterwards, which is exactly the shape `ReplayHarness::replay`
        // imposes on itself (`replay.rs:315`, `replay.rs:330`). Producing any
        // other way would make the two paths incomparable.
        let observed_tick = match observed {
            Observation::Fresh => Tick::new(tick),
            Observation::Stale => Tick::new(tick - 10),
        };
        backend.insert_observed(NEIGHBOUR, Rock { hp: 7 }, observed_tick);

        let inputs = consults_at(tick);
        for (seq, consult) in inputs.iter().enumerate() {
            records.push(InputRecord {
                tick_off: offset as u16,
                seq: seq as u16,
                source: RecordSource::Player {
                    node: authority.public(),
                    input_seq: (tick * 10 + seq as u64) as u32,
                },
                payload: bytes::Bytes::from(consult.to_canonical()),
            });
        }

        let outcome = backend
            .step_entity(SUBJECT, Tick::new(tick), &inputs)
            .expect("the subject is installed");

        reads_per_tick.push(outcome.neighbor_reads.clone());
        logged_per_tick.push(
            outcome
                .neighbor_frames
                .iter()
                .map(|frame| frame.neighbor)
                .collect(),
        );
        presence_per_tick.push(
            outcome
                .neighbor_frames
                .iter()
                .map(|frame| frame.state.is_some())
                .collect(),
        );

        for (index, frame) in outcome.neighbor_frames.iter().enumerate() {
            records.push(InputRecord {
                tick_off: offset as u16,
                seq: (inputs.len() + index) as u16,
                source: RecordSource::NeighborFrame {
                    neighbor: frame.neighbor,
                    present: frame.state.is_some(),
                    observed_tick: frame.observed_tick,
                },
                payload: frame
                    .state
                    .clone()
                    .map(bytes::Bytes::from)
                    .unwrap_or_default(),
            });
        }

        claimed_hashes.push(outcome.state_hash);
        backend.take_state(NEIGHBOUR);
    }

    let head = fold_all(ChainHash::EMPTY, &records);
    let transitions = [HeadTransition {
        entity: SUBJECT,
        prev_head: ChainHash::EMPTY,
        head,
    }];
    let frames = vec![LogFrame {
        ruleset: RULESET,
        first_tick: Tick::new(T0),
        tick_count: TICKS as u16,
        entities: vec![EntitySlice {
            entity: SUBJECT,
            chain_epoch: 0,
            prev_head: ChainHash::EMPTY.rolling(),
            records,
            head: head.rolling(),
        }],
        sig: sign_frame(
            authority,
            RULESET,
            Tick::new(T0),
            TICKS as u16,
            &transitions,
        ),
    }];

    Produced {
        t0_claim,
        t0_snapshot,
        frames,
        claimed_hashes,
        reads_per_tick,
        logged_per_tick,
        presence_per_tick,
    }
}

/// Adjudicate what `produce` produced, on a fresh substrate of the same kind.
fn adjudicate(
    produced: &Produced,
    authority: &iroh_base::SecretKey,
    enforcement: Enforcement,
) -> Result<Vec<[u8; 32]>, ReplayError> {
    let mut harness: ReplayHarness<OrderedQueryRules, MirrorBackend> =
        ReplayHarness::on(MirrorBackend::new(SEED, enforcement));
    harness.load_claimed_snapshot(&produced.t0_claim, &produced.t0_snapshot)?;
    let trace = harness.replay(
        &produced.frames,
        authority.public(),
        (Tick::new(T0), Tick::new(T0 + TICKS)),
        &vec![Vec::new(); produced.frames.len()],
    )?;
    Ok(trace.hashes.into_iter().map(|(_, hash)| hash).collect())
}

// ── 1. The claim under test ─────────────────────────────────────────────────

/// The sequence `OrderedQuery` produces is the sequence `replay.rs:325`
/// accepts.
///
/// This is the assertion #815 could not make. Everything about the window is
/// real: signed frames, chain fold, a `ReplayHarness` on a fresh substrate, and
/// a per-tick hash comparison. Nothing in `orrery_core` is adjusted to admit
/// it.
#[test]
fn an_ordered_query_window_replays_and_the_sequences_match() {
    let authority = key(1);
    let produced = produce(
        &authority,
        &|_| consults(),
        Observation::Fresh,
        Enforcement::On,
    );

    // Side by side, per tick. The rule consulted NEIGHBOUR, GHOST, NEIGHBOUR;
    // what reached the log is two ids in first-mention order.
    for tick in 0..TICKS as usize {
        assert_eq!(
            produced.reads_per_tick[tick],
            vec![NEIGHBOUR, GHOST],
            "the query's `neighbor_reads` deduplicates and keeps first mention"
        );
        assert_eq!(
            produced.logged_per_tick[tick], produced.reads_per_tick[tick],
            "and `canonical_step` frames exactly that sequence"
        );
        assert_eq!(
            produced.presence_per_tick[tick],
            vec![true, false],
            "the hit carries state and the miss carries none — and both are logged"
        );
    }

    let replayed = adjudicate(&produced, &authority, Enforcement::On).expect("the window replays");
    assert_eq!(
        replayed, produced.claimed_hashes,
        "every tick reproduces the hash the authority claimed"
    );
}

/// The anti-vacuity companion: the check at `replay.rs:325` is elementwise and
/// ordered, so a window whose frames are permuted must be refused. If this
/// passed, the test above would be proving nothing about ordering.
#[test]
fn a_permuted_frame_order_is_refused() {
    let authority = key(1);
    let mut produced = produce(
        &authority,
        &|_| consults(),
        Observation::Fresh,
        Enforcement::On,
    );
    let records = &mut produced.frames[0].entities[0].records;
    let neighbour_frames: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| matches!(record.source, RecordSource::NeighborFrame { .. }))
        .map(|(index, _)| index)
        .collect();
    assert!(neighbour_frames.len() >= 2, "there is an order to permute");
    // Swap the *contents* of the first tick's two neighbour records, leaving
    // `tick_off` and `seq` alone. Swapping whole records instead trips the
    // earlier `InputOrderIllegal` check on `seq` and the test would be about
    // sequence numbers rather than about `replay.rs:325`.
    let (first, second) = (neighbour_frames[0], neighbour_frames[1]);
    let carried = (
        records[first].source.clone(),
        records[first].payload.clone(),
    );
    records[first].source = records[second].source.clone();
    records[first].payload = records[second].payload.clone();
    records[second].source = carried.0;
    records[second].payload = carried.1;
    // The fold covers the records, so the frame has to be re-signed for the
    // test to be about ordering rather than about a broken signature.
    let head = fold_all(ChainHash::EMPTY, records);
    produced.frames[0].entities[0].head = head.rolling();
    produced.frames[0].sig = sign_frame(
        &authority,
        RULESET,
        Tick::new(T0),
        TICKS as u16,
        &[HeadTransition {
            entity: SUBJECT,
            prev_head: ChainHash::EMPTY,
            head,
        }],
    );

    assert_eq!(
        adjudicate(&produced, &authority, Enforcement::On),
        Err(ReplayError::NeighborFramesMalformed),
        "the comparison is ordered, so the green test above is about order"
    );
}

// ── 2. The refutations ──────────────────────────────────────────────────────

/// **Refutation.** `OrderedQuery` has no staleness check, and the difference is
/// not academic: an honest authority that reads a stale neighbour through the
/// query produces a window that replays to a *different hash*.
///
/// `StateView::neighbor` (`ruleset.rs:176`) hides an observation older than
/// `max_neighbor_staleness_ticks` — the read is recorded, the state is not
/// returned — and `canonical_step` (`executor.rs:394`) logs the frame as
/// `present: false` with the *reader's* tick. `OrderedQuery::get` does neither
/// check: the row is in the `World`, so it is returned.
///
/// So the authority computes with a state its own log says it never received,
/// and the adjudicator — which installs only present frames — computes without
/// it. The read sequence still matches at `replay.rs:325`; the hashes do not.
/// That is worse than a refusal, because the window is well-formed: it convicts
/// an honest authority.
#[test]
fn a_stale_read_through_the_query_replays_to_a_different_hash() {
    let authority = key(1);
    let produced = produce(
        &authority,
        &|_| consults(),
        Observation::Stale,
        Enforcement::Off,
    );

    // The log is well-formed and the sequence still matches — the divergence
    // is not caught by the sequence check.
    for tick in 0..TICKS as usize {
        assert_eq!(produced.logged_per_tick[tick], vec![NEIGHBOUR, GHOST]);
        assert_eq!(
            produced.presence_per_tick[tick],
            vec![false, false],
            "the executor logs the stale read as absent, exactly as `StateView` saw it"
        );
    }

    let replayed =
        adjudicate(&produced, &authority, Enforcement::Off).expect("the window is well-formed");
    assert_ne!(
        replayed, produced.claimed_hashes,
        "the query consumed state the log says was never delivered, and replay disagrees"
    );
}

/// The control for the test above: with the same rule and the same window, a
/// *fresh* observation replays clean. So the divergence is the staleness rule,
/// not the wiring.
#[test]
fn the_same_window_with_a_fresh_observation_does_not_diverge() {
    let authority = key(1);
    let produced = produce(
        &authority,
        &|_| consults(),
        Observation::Fresh,
        Enforcement::Off,
    );
    assert_eq!(
        adjudicate(&produced, &authority, Enforcement::Off).expect("replays"),
        produced.claimed_hashes
    );
}

/// **Refutation.** The query answers for the stepping entity's own id;
/// `StateView` refuses it by identity (`ruleset.rs:176`, `id != self.entity`).
///
/// The read *is* recorded either way, so `replay.rs:325` is satisfied and the
/// window is accepted. What the log then says is false: the frame for the own
/// id is written `present: false` with an empty payload
/// (`executor.rs:394`, `(*neighbor != entity).then(..)`), while the rule
/// actually consumed the entity's own `hp` a second time, through a path the
/// evidence does not describe.
///
/// Here the replay happens to agree, because the adjudicator runs the *same*
/// query implementation and reads the same own row out of its own mirror. That
/// agreement is the finding, not a reassurance: it means the divergence is
/// invisible to adjudication, and would surface only against a substrate that
/// honoured `StateView`'s own-identity rule.
#[test]
fn the_query_answers_for_the_stepping_entity_and_the_log_calls_it_absent() {
    let authority = key(1);
    let produced = produce(
        &authority,
        &|_| vec![Consult(SUBJECT), Consult(NEIGHBOUR)],
        Observation::Fresh,
        Enforcement::Off,
    );

    assert_eq!(
        produced.logged_per_tick[0],
        vec![SUBJECT, NEIGHBOUR],
        "the own read is recorded, so the sequence check cannot object"
    );
    assert_eq!(
        produced.presence_per_tick[0],
        vec![false, true],
        "and the frame for the own id says the state was never delivered"
    );

    // The state proves the rule consumed it anyway: an absent read subtracts
    // one (`facade_game::erode`), a present one adds the target's `hp`. Own hp
    // starts at 100, so a genuine miss on SUBJECT plus a hit on NEIGHBOUR
    // would give 100 - 1 + 7 = 106; reading own gives 100 + 100 + 7 = 207.
    let mut backend = MirrorBackend::new(SEED, Enforcement::Off);
    backend.insert(SUBJECT, Rock { hp: 100 });
    backend.insert_observed(NEIGHBOUR, Rock { hp: 7 }, Tick::new(T0));
    backend
        .step_entity(
            SUBJECT,
            Tick::new(T0),
            &[Consult(SUBJECT), Consult(NEIGHBOUR)],
        )
        .expect("installed");
    assert_eq!(
        backend.state(SUBJECT).expect("installed").hp,
        207,
        "own state was read through the query, though the log records absence"
    );

    // And it still adjudicates, which is the uncomfortable half.
    assert!(adjudicate(&produced, &authority, Enforcement::Off).is_ok());
}

// ── 3. The repair, and what it cost ─────────────────────────────────────────

/// Both refutations close when the host stamps a [`ReadWindow`], and the fix
/// is the one `StateView` already makes: `checked_sub` against the ruleset's
/// `max_neighbor_staleness_ticks` (`ruleset.rs:192`).
///
/// The same stale window that diverged above now replays to the same hashes,
/// because the query hides what the log calls absent.
#[test]
fn a_stamped_read_window_closes_the_staleness_gap() {
    let authority = key(1);
    let produced = produce(
        &authority,
        &|_| consults(),
        Observation::Stale,
        Enforcement::On,
    );

    for tick in 0..TICKS as usize {
        assert_eq!(
            produced.logged_per_tick[tick],
            vec![NEIGHBOUR, GHOST],
            "the stale read is still recorded — hidden, not unasked"
        );
        assert_eq!(produced.presence_per_tick[tick], vec![false, false]);
    }

    assert_eq!(
        adjudicate(&produced, &authority, Enforcement::On).expect("replays"),
        produced.claimed_hashes,
        "the query and the log now agree about what was delivered"
    );
}

/// And the own id is refused by identity, recorded, and priced as a miss —
/// which is what `StateView::neighbor` does with it.
#[test]
fn a_stamped_read_window_refuses_the_readers_own_id() {
    let authority = key(1);
    let produced = produce(
        &authority,
        &|_| vec![Consult(SUBJECT), Consult(NEIGHBOUR)],
        Observation::Fresh,
        Enforcement::On,
    );

    assert_eq!(produced.logged_per_tick[0], vec![SUBJECT, NEIGHBOUR]);
    assert_eq!(produced.presence_per_tick[0], vec![false, true]);

    let mut backend = MirrorBackend::new(SEED, Enforcement::On);
    backend.insert(SUBJECT, Rock { hp: 100 });
    backend.insert_observed(NEIGHBOUR, Rock { hp: 7 }, Tick::new(T0));
    backend
        .step_entity(
            SUBJECT,
            Tick::new(T0),
            &[Consult(SUBJECT), Consult(NEIGHBOUR)],
        )
        .expect("installed");
    assert_eq!(
        backend.state(SUBJECT).expect("installed").hp,
        106,
        "100 - 1 for the refused own read, + 7 for the neighbour — the miss `StateView` would have produced"
    );

    assert_eq!(
        adjudicate(&produced, &authority, Enforcement::On).expect("replays"),
        produced.claimed_hashes
    );
}

/// The bound is a bound, not a switch: an observation *inside* the cap is still
/// delivered. Without this the two tests above would pass for a query that
/// simply never returned anything.
#[test]
fn a_read_window_still_admits_an_observation_inside_the_cap() {
    let mut backend = MirrorBackend::new(SEED, Enforcement::On);
    backend.insert(SUBJECT, Rock { hp: 100 });
    // Four ticks back, against a cap of five.
    backend.insert_observed(NEIGHBOUR, Rock { hp: 7 }, Tick::new(T0 - 4));
    backend
        .step_entity(SUBJECT, Tick::new(T0), &[Consult(NEIGHBOUR)])
        .expect("installed");
    assert_eq!(
        backend.state(SUBJECT).expect("installed").hp,
        107,
        "a four-tick-old observation is inside a five-tick cap and must be read"
    );
}
