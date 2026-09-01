//! D43 clause (e)(5), adjudication half: the verdict is reached *on the
//! substrate that authored the claims* (#763).
//!
//! > exposes single-entity step semantics to witnesses and adjudication: the
//! > verdict must hold in a world of one, and "the schedule was deterministic"
//! > is never a substitute for per-entity replay.
//!
//! #762 closed the host half — `tier_h_world_of_one.rs` replays one entity at
//! a time on the ECS and reproduces every hash it claimed. The half this file
//! closes is the other one: until #763, `orrery_core::verify_bundle` built its
//! harness around `Executor::new` and `orrery_games::diff`'s
//! `authored_bundles` re-executed every side's signed log through an
//! `Executor`, whatever authored it. On the ECS path that made the D-4
//! *frames* executor-authored while the *claim values* were ECS-derived: an
//! adjudicator re-executing on a different substrate than the one under
//! suspicion, which is exactly the distance the clause's own sentence is
//! about.
//!
//! # What is and is not claimed here
//!
//! This is not a hole a cheat walks through, and nothing below pretends it
//! was. A diverging ECS fails D-1, D-2 and D-3 and the claim values
//! independently of the frames, so conviction power never depended on it.
//! What these tests hold is the structural property: the adjudicator's
//! substrate is the authoring substrate, observably so.
//!
//! # Why the guard has to be observational rather than differential
//!
//! The two substrates are byte-identical by construction — every canonical
//! byte comes from `orrery_core::canonical_step`, which both call and neither
//! copies — so no comparison of *outputs* can tell you which one ran. That is
//! the point of the seam and it is also why "the goldens still pass" is no
//! evidence at all here. So the guard watches the substrate itself:
//! [`Counting`] records the `step_entity` calls the adjudicator made on it,
//! and [`Swapped`] proves those calls are what the verdict actually rests on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use orrery_core::{Ruleset, SealedTickInputs, SteppedEntity, TickBackend, TickOutcome};
use orrery_games::diff::{collect_witness_on, cross_replay_on, Backends, WitnessArtifact};
use orrery_games::regolith::Regolith;
use orrery_games::scenario::{play_with, Play, Scenario, SCENARIOS};
use orrery_games::Game;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::ecs::EcsBackend;

fn regolith_ecs(game: Regolith, seed: UniverseSeed) -> EcsBackend<Regolith> {
    EcsBackend::new(game, seed).with_migrated_module(
        orrery_games::regolith::native_ecs::sync_migrated,
        orrery_games::regolith::native_ecs::step_migrated,
    )
}

/// A substrate that counts the single-entity steps taken on it, and otherwise
/// is exactly the substrate it wraps.
///
/// Nothing canonical is derived from the count: it is written by
/// `step_entity` and read only by a test assertion, and the wrapper forwards
/// every other call untouched.
struct Counting<B> {
    inner: B,
    steps: Arc<AtomicUsize>,
}

impl<R: Ruleset, B: TickBackend<R>> TickBackend<R> for Counting<B> {
    fn ruleset(&self) -> &R {
        self.inner.ruleset()
    }

    fn insert_observed(&mut self, entity: PersistId, state: R::CoreState, observed_tick: Tick) {
        self.inner.insert_observed(entity, state, observed_tick);
    }

    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        self.inner.take_state(entity)
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.inner.state(entity)
    }

    fn entities(&self) -> Vec<PersistId> {
        self.inner.entities()
    }

    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        self.steps.fetch_add(1, Ordering::Relaxed);
        self.inner.step_entity(entity, tick, inputs)
    }

    fn step_tick(
        &mut self,
        tick: Tick,
        inputs: &SealedTickInputs<R::CoreInput>,
    ) -> Vec<SteppedEntity<R::CoreEvent>> {
        let stepped = self.inner.step_tick(tick, inputs);
        self.steps.fetch_add(stepped.len(), Ordering::Relaxed);
        stepped
    }
}

/// A substrate that installs a *different* state than the one it was handed.
///
/// This is the anti-vacuity companion to [`Counting`]. A counter that only
/// counts would still tick over if the adjudicator called a backend it then
/// ignored; `Swapped` answers differently, so a verdict that does not move
/// when it is handed in is a verdict that never consulted the substrate at
/// all. It changes nothing about the rules — it stores an ordinary legal
/// spawn state — which is what makes the resulting `Confirms` a statement
/// about *where* the replay ran.
struct Swapped<B> {
    inner: B,
}

impl<R: Game, B: TickBackend<R>> TickBackend<R> for Swapped<B> {
    fn ruleset(&self) -> &R {
        self.inner.ruleset()
    }

    fn insert_observed(&mut self, entity: PersistId, _state: R::CoreState, observed_tick: Tick) {
        // A legal state, freshly spawned from a slot the run never used, in
        // place of the one the bundle committed to.
        let substitute = self.inner.ruleset().spawn(entity, 0xD43);
        self.inner
            .insert_observed(entity, substitute, observed_tick);
    }

    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        self.inner.take_state(entity)
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.inner.state(entity)
    }

    fn entities(&self) -> Vec<PersistId> {
        self.inner.entities()
    }

    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        self.inner.step_entity(entity, tick, inputs)
    }

    fn step_tick(
        &mut self,
        tick: Tick,
        inputs: &SealedTickInputs<R::CoreInput>,
    ) -> Vec<SteppedEntity<R::CoreEvent>> {
        self.inner.step_tick(tick, inputs)
    }
}

/// The scenario every test below runs: the smallest one that still has a
/// population to be reduced to a world of one.
fn scenario() -> Scenario {
    *SCENARIOS
        .iter()
        .find(|scenario| scenario.name == "duel")
        .expect("the battery corpus declares a duel scenario")
}

/// One honest Regolith run, played on the ECS — so the claims under
/// adjudication really are ECS-authored.
fn played_on_the_ecs(scenario: &Scenario) -> Play<Regolith> {
    play_with(Regolith::honest(), scenario, regolith_ecs)
}

fn witness_on_the_ecs(played: &Play<Regolith>) -> WitnessArtifact {
    collect_witness_on(&Regolith::honest(), played, regolith_ecs)
        .expect("an honest ECS run authors D-4 evidence")
}

/// **The guarded stage.** With ECS-authored claims in front of it, the
/// adjudicator re-executes on the ECS: the substrate it was handed records
/// the per-entity steps the verdict was reached from, and the verdict is
/// clean.
///
/// A zero count is the failure this test exists for. It is what an
/// adjudicator that quietly rebuilt an `Executor` for itself would leave
/// behind — the same verdict, reached somewhere else.
#[test]
fn adjudication_re_executes_on_the_substrate_that_authored_the_claims() {
    let scenario = scenario();
    let played = played_on_the_ecs(&scenario);
    let witness = witness_on_the_ecs(&played);
    assert!(
        !witness.bundles.is_empty(),
        "no ECS-authored bundle to adjudicate, so nothing below is a test"
    );

    let steps = Arc::new(AtomicUsize::new(0));
    let ecs = |game, seed| Counting {
        inner: regolith_ecs(game, seed),
        steps: Arc::clone(&steps),
    };
    let cross = cross_replay_on(
        &Regolith::honest(),
        &Regolith::honest(),
        &witness,
        &witness,
        played.sealed.seed,
        &Backends {
            legacy: &ecs,
            candidate: &ecs,
        },
    );

    assert!(
        !cross.verdicts.is_empty(),
        "the cross-replay reached no verdict at all"
    );
    assert!(
        cross.unclean().is_empty(),
        "an honest ECS run was not exonerated on its own substrate: {:?}",
        cross.unclean()
    );
    assert!(
        steps.load(Ordering::Relaxed) > 0,
        "the adjudicator reached its verdicts without taking a single step on \
         the substrate that authored the claims"
    );
}

/// The other half of the acceptance: the ECS path's **D-4 frames are authored
/// on the ECS**.
///
/// `authored_bundles` re-executes the sealed inputs to produce the signed log
/// a witness would adjudicate. Until #763 it did that through an `Executor`
/// whatever ran the scenario, which is how ECS-derived claim values ended up
/// in front of executor-authored frames. There is nothing in the *bytes* that
/// can tell you which one ran — that is the seam working — so this watches the
/// substrate instead.
#[test]
fn the_d4_evidence_is_authored_on_the_substrate_that_ran_the_scenario() {
    let scenario = scenario();
    let played = played_on_the_ecs(&scenario);

    let steps = Arc::new(AtomicUsize::new(0));
    let witness = collect_witness_on(&Regolith::honest(), &played, |game, seed| Counting {
        inner: regolith_ecs(game, seed),
        steps: Arc::clone(&steps),
    })
    .expect("an honest ECS run authors D-4 evidence");

    assert!(
        !witness.bundles.is_empty(),
        "no bundles were authored, so a zero count below would be trivially true"
    );
    assert!(
        steps.load(Ordering::Relaxed) > 0,
        "the D-4 evidence was authored without a single step on the substrate \
         the scenario ran on"
    );
}

/// The counter is not inert: the substrate handed to the adjudicator is what
/// the verdict rests on.
///
/// Same ECS-authored evidence, same adjudicator, one thing changed — the
/// substrate stores a different (still perfectly legal) state than the bundle
/// committed to. The verdict must move. If it does not, then the backend
/// parameter is decorative and the test above is counting calls nobody used.
#[test]
fn the_substrate_the_adjudicator_was_handed_is_what_the_verdict_rests_on() {
    let scenario = scenario();
    let played = played_on_the_ecs(&scenario);
    let witness = witness_on_the_ecs(&played);

    let swapped = |game, seed| Swapped {
        inner: regolith_ecs(game, seed),
    };
    let cross = cross_replay_on(
        &Regolith::honest(),
        &Regolith::honest(),
        &witness,
        &witness,
        played.sealed.seed,
        &Backends {
            legacy: &swapped,
            candidate: &swapped,
        },
    );

    assert!(
        !cross.verdicts.is_empty(),
        "the cross-replay reached no verdict at all"
    );
    assert!(
        !cross.unclean().is_empty(),
        "a substrate that answers differently produced the same clean verdicts, \
         so the adjudicator never consulted it"
    );
}

/// And the relocation moved no canonical byte: the evidence the ECS authors is
/// the evidence the store authors, bundle for bundle, byte for byte.
///
/// This is the constraint #763 was run under, checked at the artifact rather
/// than inferred from a green golden — the bundles carry signatures over
/// frames and claims, so equality here is equality of every hash, head and
/// input the two substrates committed to.
#[test]
fn the_ecs_authors_byte_identical_evidence_to_the_store() {
    let scenario = scenario();
    let played = played_on_the_ecs(&scenario);

    let on_the_ecs = witness_on_the_ecs(&played);
    let on_the_store = collect_witness_on(&Regolith::honest(), &played, orrery_core::Executor::new)
        .expect("the store authors D-4 evidence from the same sealed run");

    assert!(
        !on_the_ecs.bundles.is_empty(),
        "no bundles were authored, so nothing below is a comparison"
    );
    assert_eq!(
        on_the_ecs.bundles.keys().collect::<Vec<_>>(),
        on_the_store.bundles.keys().collect::<Vec<_>>(),
        "the two substrates authored evidence for different entities"
    );
    assert_eq!(
        on_the_ecs, on_the_store,
        "the ECS and the store authored different D-4 evidence from one sealed run"
    );
}

/// The world-of-one reduction, now on the adjudicator's own path.
///
/// `ReplayHarness::load_claimed_snapshot` installs exactly one state, so every
/// step the assertion above counted happened in a world of one — on the ECS.
/// This pins that the population really was one, rather than the substrate
/// having quietly carried the whole run's population into the replay.
#[test]
fn the_adjudicating_substrate_holds_a_world_of_one() {
    let scenario = scenario();
    let played = played_on_the_ecs(&scenario);
    let witness = witness_on_the_ecs(&played);

    let widest = Arc::new(AtomicUsize::new(0));
    let ecs = |game, seed| Widest {
        inner: regolith_ecs(game, seed),
        widest: Arc::clone(&widest),
    };
    let cross = cross_replay_on(
        &Regolith::honest(),
        &Regolith::honest(),
        &witness,
        &witness,
        played.sealed.seed,
        &Backends {
            legacy: &ecs,
            candidate: &ecs,
        },
    );
    assert!(
        cross.unclean().is_empty(),
        "an honest ECS run was not exonerated: {:?}",
        cross.unclean()
    );
    let widest = widest.load(Ordering::Relaxed);
    assert!(widest > 0, "the adjudicator never stepped anything");
    assert_eq!(
        widest, 1,
        "the adjudicating substrate stepped a population of {widest}; a bundle \
         commits to one entity's state, so a wider world is one the authority \
         never had"
    );
}

/// Records the widest population the substrate ever held at a step.
struct Widest<B> {
    inner: B,
    widest: Arc<AtomicUsize>,
}

impl<R: Ruleset, B: TickBackend<R>> TickBackend<R> for Widest<B> {
    fn ruleset(&self) -> &R {
        self.inner.ruleset()
    }

    fn insert_observed(&mut self, entity: PersistId, state: R::CoreState, observed_tick: Tick) {
        self.inner.insert_observed(entity, state, observed_tick);
    }

    fn take_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        self.inner.take_state(entity)
    }

    fn state(&self, entity: PersistId) -> Option<&R::CoreState> {
        self.inner.state(entity)
    }

    fn entities(&self) -> Vec<PersistId> {
        self.inner.entities()
    }

    fn step_entity(
        &mut self,
        entity: PersistId,
        tick: Tick,
        inputs: &[R::CoreInput],
    ) -> Option<TickOutcome<R::CoreEvent>> {
        let held = self.inner.entities().len();
        self.widest.fetch_max(held, Ordering::Relaxed);
        self.inner.step_entity(entity, tick, inputs)
    }

    fn step_tick(
        &mut self,
        tick: Tick,
        inputs: &SealedTickInputs<R::CoreInput>,
    ) -> Vec<SteppedEntity<R::CoreEvent>> {
        let held = self.inner.entities().len();
        self.widest.fetch_max(held, Ordering::Relaxed);
        self.inner.step_tick(tick, inputs)
    }
}
