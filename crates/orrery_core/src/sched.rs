//! A canonical, declared, per-entity system schedule.
//!
//! # What this is, and what it deliberately is not
//!
//! An ECS query fuses two things: **selection** over a population, and
//! **projection** onto the components an operation actually touches. Orrery's
//! adjudication contract forbids the first — every cross-entity read is
//! recorded, capped ([`Ruleset::max_neighbor_reads`]) and replayed from the
//! log, so a rule that scanned a population would either emit O(N) frames per
//! entity-tick or read outside the log and break replay. It forbids nothing
//! about the second.
//!
//! So this module takes the projection and leaves the selection. A
//! [`System`] is a named function over **one entity's own state**, declared in
//! a `const` table; the runner walks stages in declared order and systems
//! within a stage in declared order. Selection is per-entity and free: a
//! system whose projection does not match the entity's state simply does not
//! run, which is exactly the hand-written `match own { .. }` dispatch a
//! [`Ruleset::step`] otherwise opens with.
//!
//! # The seam this puts in the type system
//!
//! Neighbour reads are the property the whole adjudication story rests on, and
//! today they are kept honest by a text scan (`scripts/core-gates.sh` clause
//! 5). Here they are kept honest by a signature: an [`Observation`] receives
//! the [`StateView`] and is the only thing that can, while a [`System`]
//! receives `&mut R::CoreState` and has no `StateView` to read a neighbour
//! from. The scan stays — it is cheap and it names the reviewer — but it is no
//! longer the only thing standing between a rules author and an unrecorded
//! read.
//!
//! # Determinism
//!
//! The schedule is a `const` ordered table. Iteration is over slices, never
//! over a map, so there is no VC-4 surface. Nothing here touches the RNG, the
//! quantizer, the hash or the neighbour framing — those stay in
//! [`crate::executor::canonical_step`], which this module never calls and
//! cannot reach. What a schedule changes is *the order rule bodies run in*,
//! and that order is now data a manifest can digest (D43 clause (g)) rather
//! than the statement order of one long function.
//!
//! # Why the contract did not have to change
//!
//! [`run_schedule`] is a plain function with exactly [`Ruleset::step`]'s
//! arguments and exactly its return type. A game adopting a schedule writes
//! `fn step(..) { sched::run_schedule(self, view, inputs, rng) }`. `Ruleset`
//! is untouched, every existing `impl` still compiles, and a ruleset that
//! wants none of this is unaffected.

use orrery_protocol::PersistId;

use crate::rng::TickRng;
use crate::ruleset::{OrderedInputs, Ruleset, StateView, StepOutput};

/// A stable canonical stage name.
///
/// Newtyped rather than a bare `&'static str` so a stage name cannot be passed
/// where a system name is wanted; both end up in the schedule digest, where
/// swapping them would be a silent topology change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StageName(pub &'static str);

/// A stable canonical system name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemName(pub &'static str);

/// Everything a canonical system may reach besides its own state.
///
/// Own state is deliberately *not* in here. It arrives as the first argument
/// of a [`SystemFn`], which is what lets a system be written against a
/// projected component (`&mut Craft`) rather than against the whole state
/// enum, and what keeps a system from holding a [`StateView`].
pub struct StepCtx<'a, R: Ruleset, L> {
    /// Which entity is being stepped, for event attribution.
    pub entity: PersistId,
    /// The ruleset, for the configuration a rule reads off it.
    pub rules: &'a R,
    /// This tick's inputs, in the authority's sealed order (VC-2).
    pub inputs: &'a OrderedInputs<'a, R::CoreInput>,
    /// The per-entity, per-tick RNG (VC-3).
    pub rng: &'a mut TickRng,
    /// Tick-scoped scratch shared between the systems of one entity's tick.
    ///
    /// This is where values that must **not** round between systems live —
    /// the unquantized carry a single long `step` keeps in local variables.
    /// Putting it here makes the tick's quantization boundary (VC-7) a visible
    /// declaration instead of an accident of where the last statement ran.
    pub locals: &'a mut L,
    events: &'a mut Vec<R::CoreEvent>,
}

impl<R: Ruleset, L> StepCtx<'_, R, L> {
    /// Emit an outcome event. Emission order is canonical.
    pub fn emit(&mut self, event: R::CoreEvent) {
        self.events.push(event);
    }

    /// The events emitted so far this tick, in emission order.
    pub fn emitted(&self) -> &[R::CoreEvent] {
        self.events
    }
}

/// The body of a canonical system: own state, plus everything else.
pub type SystemFn<R, L> = fn(&mut <R as Ruleset>::CoreState, &mut StepCtx<'_, R, L>);

/// The body of an observation system, the only place a neighbour may be read.
pub type ObservationFn<R, L> = fn(
    &mut StateView<'_, <R as Ruleset>::CoreState>,
    &OrderedInputs<'_, <R as Ruleset>::CoreInput>,
    &mut L,
);

/// One named canonical system.
pub struct System<R: Ruleset, L: 'static> {
    /// The name that appears in the manifest and the schedule digest.
    pub name: SystemName,
    /// What it does.
    pub run: SystemFn<R, L>,
}

/// One named observation system.
///
/// It runs before every [`System`], holds the [`StateView`], and writes what
/// it learned into the tick locals. Recorded neighbour reads are confined here
/// by construction.
pub struct Observation<R: Ruleset, L: 'static> {
    /// The name that appears in the manifest and the schedule digest.
    pub name: SystemName,
    /// What it does.
    pub run: ObservationFn<R, L>,
}

/// One named stage: an ordered list of systems.
pub struct Stage<R: Ruleset, L: 'static> {
    /// The name that appears in the manifest and the schedule digest.
    pub name: StageName,
    /// Systems in canonical execution order.
    pub systems: &'static [System<R, L>],
}

/// A game's whole canonical tick, as data.
pub struct Schedule<R: Ruleset, L: 'static> {
    /// The stage name reported for the observation systems.
    pub observe_stage: StageName,
    /// The audited neighbour-reading systems, in canonical order.
    pub observe: &'static [Observation<R, L>],
    /// Every other stage, in canonical order.
    pub stages: &'static [Stage<R, L>],
}

impl<R: Ruleset, L: 'static> Schedule<R, L> {
    /// Every stage name in canonical order, observation stage first.
    pub fn stage_names(&self) -> impl Iterator<Item = StageName> + '_ {
        core::iter::once(self.observe_stage).chain(self.stages.iter().map(|stage| stage.name))
    }

    /// Every system name in canonical execution order, across all stages.
    pub fn system_names(&self) -> impl Iterator<Item = SystemName> + '_ {
        self.observe
            .iter()
            .map(|observation| observation.name)
            .chain(
                self.stages
                    .iter()
                    .flat_map(|stage| stage.systems.iter().map(|system| system.name)),
            )
    }

    /// Each stage with its ordered system names, observation stage first.
    pub fn stages_with_systems(&self) -> Vec<(StageName, Vec<SystemName>)> {
        core::iter::once((
            self.observe_stage,
            self.observe.iter().map(|o| o.name).collect::<Vec<_>>(),
        ))
        .chain(self.stages.iter().map(|stage| {
            (
                stage.name,
                stage.systems.iter().map(|s| s.name).collect::<Vec<_>>(),
            )
        }))
        .collect()
    }

    /// The first system name that appears twice, if any.
    ///
    /// A duplicate name is not a harmless nicety: the schedule digest and the
    /// manifest address systems by name, so two systems sharing one makes an
    /// ordering edge ambiguous and the digest a statement about something that
    /// does not exist. Games assert this is `None`.
    pub fn duplicate_system_name(&self) -> Option<SystemName> {
        let mut seen: Vec<SystemName> = Vec::new();
        for name in self.system_names() {
            if seen.contains(&name) {
                return Some(name);
            }
            seen.push(name);
        }
        None
    }

    /// One system by name, wherever it sits in the table.
    ///
    /// Addressing a rule by name is what makes a rule *individually*
    /// reachable — to a test, to a tool, to a report. Inside one long `step`
    /// body a phase has no name and no handle, so the smallest thing anyone
    /// can run is the whole tick.
    pub fn system(&self, name: SystemName) -> Option<&'static System<R, L>> {
        self.stages
            .iter()
            .flat_map(|stage| stage.systems.iter())
            .find(|system| system.name == name)
    }

    /// The canonical position of a system, for checking a declared ordering
    /// edge against the table that actually runs.
    pub fn position_of(&self, name: SystemName) -> Option<usize> {
        self.system_names().position(|candidate| candidate == name)
    }
}

/// A ruleset whose tick is a declared schedule rather than one function body.
///
/// This is an *extension*, not a replacement: it is a supertrait of
/// [`Ruleset`], adds no required method to it, and a ruleset that does not
/// implement it is unaffected.
pub trait Scheduled: Ruleset + Sized {
    /// Tick-scoped scratch shared between this entity's systems.
    ///
    /// `Default` is the initial value at the top of every entity's tick. It is
    /// never carried between ticks or between entities — carrying it would be
    /// hidden state outside the closed input set.
    type Locals: Default + 'static;

    /// This ruleset's canonical schedule.
    fn schedule(&self) -> &'static Schedule<Self, Self::Locals>;
}

/// Run a ruleset's declared schedule for one entity-tick.
///
/// Drop-in for a [`Ruleset::step`] body: same arguments, same return type.
///
/// Order is: every observation system in declared order, then every stage in
/// declared order, and within each stage every system in declared order. There
/// is no other ordering rule and nothing is sorted at runtime.
pub fn run_schedule<R: Scheduled>(
    rules: &R,
    view: &mut StateView<'_, R::CoreState>,
    inputs: &OrderedInputs<'_, R::CoreInput>,
    rng: &mut TickRng,
) -> StepOutput<R::CoreEvent> {
    let entity = view.entity();
    let schedule = rules.schedule();
    let mut locals = R::Locals::default();
    for observation in schedule.observe {
        (observation.run)(view, inputs, &mut locals);
    }
    let mut events = Vec::new();
    let state = view.own_mut();
    for stage in schedule.stages {
        for system in stage.systems {
            let mut ctx = StepCtx {
                entity,
                rules,
                inputs,
                rng: &mut *rng,
                locals: &mut locals,
                events: &mut events,
            };
            (system.run)(&mut *state, &mut ctx);
        }
    }
    StepOutput { events }
}

/// Run one system on one state, outside a tick.
///
/// This is the entry point a rule's own test uses. It exists because a system
/// *has* a boundary: own state in, own state and events out, plus whatever the
/// caller chooses to put in `locals`. A phase buried in a long `step` body has
/// no such boundary, so the smallest thing its test could drive was a whole
/// entity-tick through an `Executor` — which is why rule tests in this tree
/// are written as scenarios even when the thing under test is four lines.
///
/// It is deliberately not part of [`run_schedule`]'s path: nothing canonical
/// calls it, so it cannot become a second way to run a tick.
pub fn run_system<R: Ruleset, L>(
    rules: &R,
    state: &mut R::CoreState,
    inputs: &OrderedInputs<'_, R::CoreInput>,
    rng: &mut TickRng,
    locals: &mut L,
    system: &System<R, L>,
) -> Vec<R::CoreEvent> {
    let mut events = Vec::new();
    let mut ctx = StepCtx {
        entity: PersistId::new(0),
        rules,
        inputs,
        rng,
        locals,
        events: &mut events,
    };
    (system.run)(state, &mut ctx);
    events
}

/// Run one system on one state as a named entity, outside a tick.
///
/// The [`run_system`] form with the entity stated, for rules that attribute
/// what they emit.
pub fn run_system_as<R: Ruleset, L>(
    entity: PersistId,
    rules: &R,
    state: &mut R::CoreState,
    inputs: &OrderedInputs<'_, R::CoreInput>,
    rng: &mut TickRng,
    locals: &mut L,
    system: &System<R, L>,
) -> Vec<R::CoreEvent> {
    let mut events = Vec::new();
    let mut ctx = StepCtx {
        entity,
        rules,
        inputs,
        rng,
        locals,
        events: &mut events,
    };
    (system.run)(state, &mut ctx);
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::TickRng;
    use crate::ruleset::{CodecError, CoreCodec};
    use orrery_protocol::{RulesetId, Tick, UniverseSeed};
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    struct Trace(Vec<u8>);

    impl CoreCodec for Trace {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.0);
        }
        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            Ok(Self(bytes.to_vec()))
        }
    }

    impl crate::quantize::Quantized for Trace {
        fn quantize(&mut self) {}
    }

    struct Scribe;

    impl Ruleset for Scribe {
        const OVERFLOW_IS_CANONICAL: bool = false;
        type CoreState = Trace;
        type CoreInput = Trace;
        type CoreEvent = Trace;
        fn id(&self) -> RulesetId {
            RulesetId {
                version: 1,
                digest: [0; 32],
            }
        }
        fn step(
            &self,
            view: &mut StateView<'_, Trace>,
            inputs: &OrderedInputs<'_, Trace>,
            rng: &mut TickRng,
        ) -> StepOutput<Trace> {
            run_schedule(self, view, inputs, rng)
        }
    }

    fn push_a(state: &mut Trace, ctx: &mut StepCtx<'_, Scribe, u8>) {
        state.0.push(b'a');
        *ctx.locals += 1;
    }
    fn push_b(state: &mut Trace, ctx: &mut StepCtx<'_, Scribe, u8>) {
        state.0.push(b'b');
        ctx.emit(Trace(vec![*ctx.locals]));
    }
    fn push_c(state: &mut Trace, _ctx: &mut StepCtx<'_, Scribe, u8>) {
        state.0.push(b'c');
    }

    static SCHEDULE: Schedule<Scribe, u8> = Schedule {
        observe_stage: StageName("observe"),
        observe: &[],
        stages: &[
            Stage {
                name: StageName("first"),
                systems: &[
                    System {
                        name: SystemName("push-a"),
                        run: push_a,
                    },
                    System {
                        name: SystemName("push-b"),
                        run: push_b,
                    },
                ],
            },
            Stage {
                name: StageName("second"),
                systems: &[System {
                    name: SystemName("push-c"),
                    run: push_c,
                }],
            },
        ],
    };

    impl Scheduled for Scribe {
        type Locals = u8;
        fn schedule(&self) -> &'static Schedule<Self, u8> {
            &SCHEDULE
        }
    }

    fn run() -> (Trace, StepOutput<Trace>) {
        let mut own = Trace::default();
        let neighbors = BTreeMap::new();
        let observation_ticks = BTreeMap::new();
        let mut view = StateView::new(
            PersistId::new(1),
            &mut own,
            &neighbors,
            &observation_ticks,
            Tick::new(0),
            0,
        );
        let inputs: [Trace; 0] = [];
        let ordered = OrderedInputs::new(&inputs);
        let mut rng = crate::rng::tick_rng(UniverseSeed([7; 32]), PersistId::new(1), Tick::new(0));
        let out = Scribe.step(&mut view, &ordered, &mut rng);
        (own, out)
    }

    #[test]
    fn systems_run_in_declared_order_across_stages() {
        // The whole point of a declared table is that this order is data. If
        // it were incidental, a reorder would be invisible to every golden.
        let (state, _) = run();
        assert_eq!(state.0, b"abc");
    }

    #[test]
    fn locals_carry_between_systems_and_reset_each_tick() {
        // `push_a` bumps the scratch and `push_b` emits it, so the emitted
        // byte is 1 on every tick — never 2 — or locals would be leaking
        // state outside the closed input set.
        let (_, first) = run();
        let (_, second) = run();
        assert_eq!(first.events, vec![Trace(vec![1])]);
        assert_eq!(second.events, first.events);
    }

    #[test]
    fn the_schedule_reports_its_own_topology() {
        assert_eq!(
            SCHEDULE.stage_names().collect::<Vec<_>>(),
            vec![
                StageName("observe"),
                StageName("first"),
                StageName("second")
            ]
        );
        assert_eq!(
            SCHEDULE.system_names().collect::<Vec<_>>(),
            vec![
                SystemName("push-a"),
                SystemName("push-b"),
                SystemName("push-c")
            ]
        );
        assert_eq!(SCHEDULE.duplicate_system_name(), None);
        assert_eq!(SCHEDULE.position_of(SystemName("push-c")), Some(2));
        assert_eq!(SCHEDULE.position_of(SystemName("absent")), None);
    }

    #[test]
    fn a_duplicate_system_name_is_reported() {
        // Two systems with one name make an ordering edge ambiguous and the
        // digest a statement about something that does not exist.
        static DUPLICATED: Schedule<Scribe, u8> = Schedule {
            observe_stage: StageName("observe"),
            observe: &[],
            stages: &[Stage {
                name: StageName("only"),
                systems: &[
                    System {
                        name: SystemName("same"),
                        run: push_a,
                    },
                    System {
                        name: SystemName("same"),
                        run: push_c,
                    },
                ],
            }],
        };
        assert_eq!(DUPLICATED.duplicate_system_name(), Some(SystemName("same")));
    }
}
