//! **SPIKE #793 — propose-only. Do not merge.**
//!
//! Regolith's stage-1 checks written the way anyone would write them in
//! `bevy_ecs`: concrete components, and a `Query` that names what it wants.
//!
//! This module exists to answer one question and no other:
//!
//! > Does Regolith's *own game code* become more comfortable to write when
//! > components and queries are available?
//!
//! It is deliberately **not** a reusable engine over an abstract trait, and it
//! deliberately does not replace [`super::invariants`]. Both live in the tree
//! at once so the before and the after can be read side by side and, more
//! importantly, so [`crate::regolith::native`]'s answers can be differenced
//! against the shipped ones on the same corpus.
//!
//! # What this violates, stated out loud
//!
//! Compiling this file puts `bevy_ecs` in `orrery_games`'s dependency graph,
//! which `scripts/core-gates.sh` clause 1 refuses by name (`orrery_games has
//! Bevy in its dependency graph`, exit 1). That refusal is D42 (a) and
//! D43 (e)(1), both Accepted and both owner-reserved. **This spike does not
//! amend them and does not edit the gate.** It names the amendment as owed and
//! leaves the gate failing, the way #745 named D42 (d).
//!
//! # The three sites, and why these three
//!
//! `speed_cap` and `acceleration_cap` are the sites the owner named. `teleport`
//! is included because it is the one that *does not* get better, and a spike
//! that only showed its wins would be an argument rather than a measurement.
//!
//! # Determinism is not inherited here — see [`Findings`]
//!
//! `Query` iteration follows archetype and spawn order, which is stable for one
//! insertion history and **not** stable across permuted ones. The shipped
//! checks never met this because the host iterates in `PersistId` order on the
//! rules' behalf; a rule that iterates for itself has to canonicalize. This
//! module therefore sorts, and [`NativeInvariants::unsorted_findings`] exists
//! only so a test can prove the sort is load-bearing rather than decorative.

use bevy_ecs::prelude::{
    Component, IntoScheduleConfigs, Query, Res, ResMut, Resource, Schedule, With, Without, World,
};
use bevy_ecs::schedule::ScheduleLabel;
use orrery_core::invariants::checks;
use orrery_core::{
    Invariant, InvariantKind, InvariantSample, InvariantViolation, QPos, QVel, TICK_HZ,
};
use orrery_protocol::{PersistId, Tick};

use super::state::RegolithState;
use super::DRAG_PER_SEC_PER_MILLE;

/// Velocity slack allowed before a speed reading is called impossible.
/// Same constant and same value as [`super::invariants`]'s private one.
const VEL_MARGIN_MMS: i64 = 100;
/// Position slack allowed before a displacement is called a teleport.
const POS_MARGIN_MM: i64 = 100;
const TICKS_PER_SEC: i64 = TICK_HZ as i64;

// ── the components ──────────────────────────────────────────────────────────
//
// Four of these carry the entire argument. `Vel` is present on craft and rocks
// and absent from pickups and directors; `SpeedLimit` is present on everything
// that has a published ceiling. So `Query<(&Vel, &SpeedLimit)>` *is* the set of
// entities the speed cap applies to, and the two arms of the shipped `match`
// that exist only to yield a zero the comparison then discards have nowhere to
// be written, because there is nothing to write them about.

/// The canonical identity a violation is attributed to.
///
/// A `bevy_ecs::Entity` is an index into one world; this is the identity every
/// report outside it is keyed by, and the key the findings are sorted on.
#[derive(Component, Debug, Clone, Copy)]
pub struct Subject(pub PersistId);

/// The tick the sampled state is stamped with.
#[derive(Component, Debug, Clone, Copy)]
pub struct SampledAt(pub Tick);

/// Ticks between the previous sample and this one.
///
/// Samples arrive at the replication rate, so every rate-derived check divides
/// by this rather than assuming adjacency — the same contract
/// [`InvariantSample::elapsed_ticks`] states.
#[derive(Component, Debug, Clone, Copy)]
pub struct Elapsed(pub u32);

/// Lattice position, this sample.
#[derive(Component, Debug, Clone, Copy)]
pub struct Pos(pub QPos);

/// Lattice position, previous sample.
///
/// **Absent when there is no previous sample.** That absence is the whole of
/// what `Option<&S>` and its `let Some(previous) = ... else { return Ok(()) }`
/// preamble do in the shipped checks, expressed as a thing the query simply
/// does not match.
#[derive(Component, Debug, Clone, Copy)]
pub struct PrevPos(pub QPos);

/// Lattice velocity, this sample. Absent on pickups and bloom directors,
/// which have no velocity to cap.
#[derive(Component, Debug, Clone, Copy)]
pub struct Vel(pub QVel);

/// Lattice velocity, previous sample. Absent when there is no previous sample.
#[derive(Component, Debug, Clone, Copy)]
pub struct PrevVel(pub QVel);

/// Published speed ceiling, millimetres per second.
///
/// Carried by anything with a ceiling worth checking — craft from their
/// archetype, rocks from their tier, pickups as a literal zero because a
/// pickup that moved at all has moved impossibly. Bloom directors have no
/// position and carry neither this nor [`Pos`].
#[derive(Component, Debug, Clone, Copy)]
pub struct SpeedLimit(pub i64);

/// Published acceleration ceiling, millimetres per second squared.
///
/// **Only craft carry this.** `acceleration_cap`'s shipped body opens by
/// discarding every non-craft pair with a `let-else`; here the discard is the
/// component's absence and there is no opening line at all.
#[derive(Component, Debug, Clone, Copy)]
pub struct AccelLimit(pub i64);

/// This sample is the first tick of a respawn: the previous sample was a wreck
/// and this one is not.
///
/// A marker, so the exemption every kinematic check shares becomes
/// `Without<Respawned>` in the query signature — a fact about *which entities a
/// check applies to*, stated where the check says what it applies to, instead
/// of an early `return Ok(())` repeated in three bodies.
#[derive(Component, Debug, Clone, Copy)]
pub struct Respawned;

/// The entity kind changed between the two samples.
///
/// Reported, not checked, because in a native world it is not a thing a query
/// can express: the two samples of one entity are one entity, and nothing in
/// the component set records what kind it *used to* be. See [`Findings`] and
/// the module's verdict in `docs/spikes/ecs-native-game-code.md`.
#[derive(Component, Debug, Clone, Copy)]
pub struct KindChanged;

// ── the collected report ────────────────────────────────────────────────────

/// One check's answer about one entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// Who.
    pub subject: PersistId,
    /// The validator name, as the shipped checks report it.
    pub validator: &'static str,
    /// What kind of impossible.
    pub kind: InvariantKind,
}

impl Finding {
    /// The violation this finding would be reported as.
    #[must_use]
    pub const fn violation(&self) -> InvariantViolation {
        InvariantViolation::new(self.kind, self.validator)
    }
}

/// Everything the pass found, in the order the queries happened to visit.
///
/// Push order here is archetype order, which is insertion-history dependent.
/// It is canonicalized once, at the boundary, by
/// [`NativeInvariants::findings`]. Keeping the raw vector reachable is
/// deliberate: `tests/ecs_native_invariants.rs` asserts that two permutations
/// of the same population *disagree* on this, which is what proves the sort is
/// doing work rather than decorating an order that was already canonical.
#[derive(Resource, Debug, Default)]
pub struct Findings(pub Vec<Finding>);

impl Findings {
    fn report(&mut self, subject: Subject, validator: &'static str, kind: InvariantKind) {
        self.0.push(Finding {
            subject: subject.0,
            validator,
            kind,
        });
    }
}

/// The tick the pass is evaluating, for parity with `InvariantSample::tick`.
#[derive(Resource, Debug, Clone, Copy)]
pub struct EvaluatingTick(pub Tick);

// ── the checks, as game code ────────────────────────────────────────────────

/// `regolith/speed-cap`.
///
/// **Before** ([`super::invariants`] `speed_cap`, `invariants.rs:42-52`): two
/// four-arm `match`es over `RegolithState`, enumerating the same four variants
/// twice, of which two arms exist only to produce a `0` limit and a
/// `QVel::default()` that the comparison then discards.
///
/// **After**: one query. Pickups and bloom directors carry no [`Vel`], so they
/// are not visited, and there is no zero to discard because there is no arm to
/// write it in.
fn speed_cap(mut findings: ResMut<Findings>, query: Query<(&Subject, &Vel, &SpeedLimit)>) {
    for (subject, vel, limit) in &query {
        let limit = limit.0 + VEL_MARGIN_MMS;
        if vel.0.difference_squared(QVel::default()) > i128::from(limit) * i128::from(limit) {
            findings.report(*subject, "regolith/speed-cap", InvariantKind::SpeedCap);
        }
    }
}

/// `regolith/acceleration-cap`.
///
/// **Before** ([`super::invariants`] `acceleration_cap`, `invariants.rs:62`):
/// a `let-else` that pattern-matches `(Some(Craft), Craft)` out of the sample
/// pair and discards everything else, followed by an early return for the
/// respawn edge.
///
/// **After**: the `let-else` is [`AccelLimit`]'s absence on non-craft, the
/// `Option` is [`PrevVel`]'s absence on a first sample, and the respawn edge is
/// `Without<Respawned>`. The body is the arithmetic and nothing else.
///
/// **And here is the friction, recorded rather than smoothed over.** The
/// workspace lints at `-D warnings`, so `clippy::type_complexity` refuses this
/// signature: `error: very complex type used`, at the exact line that carries
/// the ergonomic win. The `allow` below is the standard `bevy_ecs` answer and
/// it is not free — the lint that would catch a genuinely tangled type in
/// Regolith is now switched off for this function. Every native system with
/// more than a few components needs it, which means adopting this style means
/// either an `allow` per system or turning the lint off crate-wide.
#[allow(clippy::type_complexity)]
fn acceleration_cap(
    mut findings: ResMut<Findings>,
    query: Query<
        (&Subject, &PrevVel, &Vel, &Elapsed, &AccelLimit, &SpeedLimit),
        Without<Respawned>,
    >,
) {
    for (subject, previous, current, elapsed, accel, speed) in &query {
        let per_tick = accel.0 / TICKS_PER_SEC
            + speed.0 * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC)
            + VEL_MARGIN_MMS;
        if checks::exceeds_acceleration(previous.0, current.0, elapsed.0, per_tick) {
            findings.report(
                *subject,
                "regolith/acceleration-cap",
                InvariantKind::AccelerationCap,
            );
        }
    }
}

/// `regolith/teleport` — the site that does **not** get better.
///
/// The shipped body's five-arm `match (previous, current)` is not really four
/// kinds of check; it is one check with three different caps plus a
/// discriminant-mismatch arm. The three caps collapse into [`SpeedLimit`]
/// exactly as they do above. The mismatch arm does not collapse: it asks a
/// question about the *pair* of samples that a component set cannot hold, so
/// the native form has to be told the answer by the projection
/// ([`KindChanged`]) rather than deriving it. See the doc's verdict.
///
/// Second `clippy::type_complexity` site; see `acceleration_cap` above for what
/// the `allow` costs.
#[allow(clippy::type_complexity)]
fn teleport(
    mut findings: ResMut<Findings>,
    query: Query<
        (&Subject, &PrevPos, &Pos, &Elapsed, &SpeedLimit),
        (Without<Respawned>, Without<KindChanged>),
    >,
) {
    for (subject, previous, current, elapsed, limit) in &query {
        if checks::exceeds_speed(
            previous.0,
            current.0,
            elapsed.0,
            limit.0 / TICKS_PER_SEC + POS_MARGIN_MM,
        ) {
            findings.report(*subject, "regolith/teleport", InvariantKind::Teleport);
        }
    }
}

/// The mismatch arm `teleport` above cannot express, kept honest as its own
/// system rather than hidden.
fn kind_changed(mut findings: ResMut<Findings>, query: Query<&Subject, With<KindChanged>>) {
    for subject in &query {
        findings.report(*subject, "regolith/value-range", InvariantKind::ValueRange);
    }
}

/// Parity with `InvariantSample::tick`: the checks above never read it, and a
/// system that takes the resource proves the read is available rather than
/// merely claimed.
fn assert_tick_available(tick: Res<EvaluatingTick>, query: Query<&SampledAt>) {
    for sampled in &query {
        debug_assert!(sampled.0 .0 <= tick.0 .0 || tick.0 .0 == 0);
    }
}

/// The one schedule a stage-1 pass runs.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Stage1;

/// The stages [`Stage1`] chains, in the order it chains them.
///
/// Chained rather than left to `bevy_ecs` for the same reason
/// `orrery_sim_host::ecs` chains its three: the canonical schedule declares no
/// ambiguity, so neither may this one.
pub const STAGE1_SYSTEMS: [&str; 5] = [
    "regolith/speed-cap",
    "regolith/acceleration-cap",
    "regolith/teleport",
    "regolith/kind-changed",
    "regolith/tick-available",
];

fn stage1_schedule() -> Schedule {
    let mut schedule = Schedule::new(Stage1);
    schedule.add_systems(
        (
            speed_cap,
            acceleration_cap,
            teleport,
            kind_changed,
            assert_tick_available,
        )
            .chain(),
    );
    assert_eq!(
        schedule.systems_len(),
        STAGE1_SYSTEMS.len(),
        "the stage-1 schedule runs a different number of systems than it declares"
    );
    schedule
}

// ── the projection: RegolithState → components ──────────────────────────────

/// One sample pair, as the shipped checks receive it.
///
/// The spike's boundary is here on purpose. Everything above this line is the
/// game code being evaluated; everything below is the adapter that exists only
/// because the rest of the tree is not native. In a genuinely native tree there
/// would be no projection at all — the components *would be* the state, and
/// this whole section would be deleted rather than written.
pub struct Sample<'a> {
    /// Who.
    pub entity: PersistId,
    /// The state just received.
    pub current: &'a RegolithState,
    /// The tick `current` is stamped with.
    pub tick: Tick,
    /// The previous sample, if any.
    pub previous: Option<&'a RegolithState>,
    /// Ticks between them; zero when there is no previous.
    pub elapsed_ticks: u32,
}

impl<'a> Sample<'a> {
    /// The same sample in the shipped checks' shape, so one corpus can drive
    /// both arms of the differential.
    #[must_use]
    pub fn as_invariant_sample(&self) -> InvariantSample<'a, RegolithState> {
        InvariantSample {
            entity: self.entity,
            current: self.current,
            tick: self.tick,
            previous: self.previous,
            elapsed_ticks: self.elapsed_ticks,
        }
    }
}

/// Kinematics a sample publishes, or `None` when it publishes none.
struct Kinematics {
    pos: QPos,
    vel: Option<QVel>,
    speed_limit: i64,
    accel_limit: Option<i64>,
}

fn kinematics(state: &RegolithState) -> Option<Kinematics> {
    match state {
        RegolithState::Craft(craft) => {
            let limits = craft.archetype.limits();
            Some(Kinematics {
                pos: craft.pos,
                vel: Some(craft.vel),
                speed_limit: limits.max_speed_mms,
                accel_limit: Some(limits.max_accel_mmss),
            })
        }
        RegolithState::Rock(rock) => Some(Kinematics {
            pos: rock.pos,
            vel: Some(rock.vel),
            speed_limit: rock.tier.limits().max_speed_mms,
            accel_limit: None,
        }),
        RegolithState::Pickup(pickup) => Some(Kinematics {
            pos: pickup.pos,
            vel: None,
            speed_limit: 0,
            accel_limit: None,
        }),
        RegolithState::BloomDirector(_) => None,
    }
}

/// Whether this pair is the first tick of a respawn.
fn respawned(previous: Option<&RegolithState>, current: &RegolithState) -> bool {
    matches!(
        (previous, current),
        (Some(RegolithState::Craft(previous)), RegolithState::Craft(current))
            if previous.hull == 0 && current.hull > 0
    )
}

/// A world holding one stage-1 pass's population, and the schedule over it.
///
/// Built fresh per pass. That is the honest shape for a stage-1 check — it runs
/// on received state on every interested peer, and there is no persistent world
/// on that path — and it is also where the native form's cost shows up. See the
/// rollback findings in `docs/spikes/ecs-native-game-code.md`.
pub struct NativeInvariants {
    world: World,
    schedule: Schedule,
}

impl NativeInvariants {
    /// An empty pass evaluating `tick`.
    #[must_use]
    pub fn new(tick: Tick) -> Self {
        let mut world = World::new();
        world.insert_resource(Findings::default());
        world.insert_resource(EvaluatingTick(tick));
        Self {
            world,
            schedule: stage1_schedule(),
        }
    }

    /// Project one sample into components and spawn it.
    ///
    /// Insertion order is the caller's, and is *not* canonicalized here — the
    /// permutation the differential applies has to reach the archetype layout
    /// or it measures nothing.
    pub fn insert(&mut self, sample: &Sample<'_>) {
        let Some(current) = kinematics(sample.current) else {
            // A bloom director publishes no kinematics, so it carries no
            // kinematic components and no kinematic query visits it. In the
            // shipped form it is two `match` arms and a discarded zero.
            self.world
                .spawn((Subject(sample.entity), SampledAt(sample.tick)));
            return;
        };
        let mut entity = self.world.spawn((
            Subject(sample.entity),
            SampledAt(sample.tick),
            Elapsed(sample.elapsed_ticks),
            Pos(current.pos),
            SpeedLimit(current.speed_limit),
        ));
        if let Some(vel) = current.vel {
            entity.insert(Vel(vel));
        }
        if let Some(accel) = current.accel_limit {
            entity.insert(AccelLimit(accel));
        }
        if let Some(previous) = sample.previous {
            if core::mem::discriminant(previous) != core::mem::discriminant(sample.current) {
                entity.insert(KindChanged);
            } else if let Some(before) = kinematics(previous) {
                entity.insert(PrevPos(before.pos));
                if let Some(vel) = before.vel {
                    entity.insert(PrevVel(vel));
                }
            }
        }
        if respawned(sample.previous, sample.current) {
            entity.insert(Respawned);
        }
    }

    /// Run the stage-1 schedule once over everything inserted.
    pub fn run(&mut self) {
        self.schedule.run(&mut self.world);
    }

    /// Drop everything found so far, keeping the population and the schedule.
    ///
    /// Exists for `tests/spike_793_native_schedule_cost.rs`, which re-runs the
    /// same schedule over the same world nine times to measure what D8's
    /// rollback window costs when a tick is a schedule rather than a loop.
    pub fn reset_findings(&mut self) {
        self.world.resource_mut::<Findings>().0.clear();
    }

    /// How many samples the pass holds.
    ///
    /// Counted by querying [`Subject`], not by asking the `World` how many
    /// entities it has: `bevy_ecs` 0.19 backs component and resource ids with
    /// entities of its own, so `World::entities().len()` is roughly twice the
    /// population and is not the number a caller means.
    #[must_use]
    pub fn len(&mut self) -> usize {
        self.world.query::<&Subject>().iter(&self.world).count()
    }

    /// Whether the pass holds no samples.
    #[must_use]
    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    /// Every finding, canonicalized.
    ///
    /// The sort is the obligation the native form creates and the shipped form
    /// never had: the host used to iterate in `PersistId` order on the rules'
    /// behalf, and a rule that iterates for itself inherits archetype order,
    /// which is stable per insertion history and not across permuted ones.
    #[must_use]
    pub fn findings(&self) -> Vec<Finding> {
        let mut findings = self.unsorted_findings();
        findings.sort_unstable();
        findings
    }

    /// The same findings in query-visit order.
    ///
    /// Exposed for the differential only. Nothing may depend on this order;
    /// `tests/ecs_native_invariants.rs` asserts that some pair of permutations
    /// *disagrees* on it, which is what makes [`Self::findings`]'s sort a
    /// measured necessity rather than a precaution.
    #[must_use]
    pub fn unsorted_findings(&self) -> Vec<Finding> {
        self.world.resource::<Findings>().0.clone()
    }
}

// ── the control arm: the same win without `bevy_ecs` ────────────────────────

/// The Bevy-free counterfactual, so the spike measures ECS rather than
/// measuring *projection*.
///
/// `regolith/mod.rs:14-31` already carries `projected_system!` — "the
/// projection half of an ECS query, and deliberately only that half" — which
/// deleted the four-way `match own { .. }` that used to open `Ruleset::step`,
/// with no `bevy_ecs` anywhere. `invariants.rs` never got the same treatment.
/// This module asks what happens if it does, because if most of the comfort is
/// available for a macro and no dependency, that is the answer to the owner's
/// question and not a footnote to it.
pub mod control {
    use super::{
        respawned, InvariantKind, InvariantSample, InvariantViolation, QVel, RegolithState,
        DRAG_PER_SEC_PER_MILLE, TICKS_PER_SEC, VEL_MARGIN_MMS,
    };

    /// What a kinematic check needs off any state that publishes kinematics.
    ///
    /// This is the control arm's whole mechanism, and it is worth naming what
    /// it is: the trait that a native form does not need, because there
    /// `Vel` and `SpeedLimit` are components and "publishes kinematics" is a
    /// query rather than an `impl`.
    pub trait Moving {
        /// Lattice velocity.
        fn vel(&self) -> QVel;
        /// Published speed ceiling, millimetres per second.
        fn speed_limit(&self) -> i64;
    }

    impl Moving for crate::regolith::state::Craft {
        fn vel(&self) -> QVel {
            self.vel
        }
        fn speed_limit(&self) -> i64 {
            self.archetype.limits().max_speed_mms
        }
    }

    impl Moving for crate::regolith::state::Rock {
        fn vel(&self) -> QVel {
            self.vel
        }
        fn speed_limit(&self) -> i64 {
            self.tier.limits().max_speed_mms
        }
    }

    /// Project a sample onto the arms that carry `$trait`, or pass.
    ///
    /// The direct analogue of `projected_system!`: the enumeration of variants
    /// moves out of every check body and into one macro, written once.
    macro_rules! over_moving {
        ($sample:expr, |$bound:ident| $body:block) => {
            match $sample.current {
                RegolithState::Craft($bound) => $body,
                RegolithState::Rock($bound) => $body,
                RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => Ok(()),
            }
        };
    }

    /// `regolith/speed-cap`, Bevy-free, with the projection factored out.
    ///
    /// Two arms instead of eight, and no discarded zero. What it still cannot
    /// do is what the macro's own shape shows: the variant list is written
    /// *here*, in the rules crate, once per projection rather than once per
    /// check — so adding a fifth entity kind is still an edit to a `match`,
    /// where in the native form it is a decision about which components the
    /// kind carries.
    pub fn speed_cap(
        sample: &InvariantSample<'_, RegolithState>,
    ) -> Result<(), InvariantViolation> {
        over_moving!(sample, |moving| {
            let limit = moving.speed_limit() + VEL_MARGIN_MMS;
            if moving.vel().difference_squared(QVel::default())
                > i128::from(limit) * i128::from(limit)
            {
                Err(InvariantViolation::new(
                    InvariantKind::SpeedCap,
                    "regolith/speed-cap",
                ))
            } else {
                Ok(())
            }
        })
    }

    /// `regolith/acceleration-cap`, Bevy-free.
    ///
    /// The honest result: **this one barely improves at all.** The check
    /// applies to exactly one variant *pair*, and a pair is not something a
    /// projection macro over `current` can select on — so the `let-else`
    /// survives almost verbatim. Compare the native version, where the same
    /// selection is `&AccelLimit` plus `&PrevVel` plus `Without<Respawned>` in
    /// the signature and nothing at all in the body.
    pub fn acceleration_cap(
        sample: &InvariantSample<'_, RegolithState>,
    ) -> Result<(), InvariantViolation> {
        let (Some(RegolithState::Craft(previous)), RegolithState::Craft(current)) =
            (sample.previous, sample.current)
        else {
            return Ok(());
        };
        if respawned(sample.previous, sample.current) {
            return Ok(());
        }
        let limits = current.archetype.limits();
        let per_tick = limits.max_accel_mmss / TICKS_PER_SEC
            + limits.max_speed_mms * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC)
            + VEL_MARGIN_MMS;
        if super::checks::exceeds_acceleration(
            previous.vel,
            current.vel,
            sample.elapsed_ticks,
            per_tick,
        ) {
            Err(InvariantViolation::new(
                InvariantKind::AccelerationCap,
                "regolith/acceleration-cap",
            ))
        } else {
            Ok(())
        }
    }
}

/// The control arm's checks, registered the way [`super::invariants::INVARIANTS`]
/// registers the shipped ones, so the differential can run all three arms off
/// one table.
pub const CONTROL_INVARIANTS: &[Invariant<RegolithState>] = &[
    Invariant {
        name: "regolith/speed-cap",
        check: control::speed_cap,
    },
    Invariant {
        name: "regolith/acceleration-cap",
        check: control::acceleration_cap,
    },
];
