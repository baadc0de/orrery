//! **Skirmish** — kinematic movement plus an integer combat core.
//!
//! The reference game P4 asks for (docs/11-roadmap.md §P4): small craft
//! manoeuvre continuously and shoot each other discretely, which puts one
//! rule on each side of the determinism contract and makes the two halves
//! separable in every measurement taken over it.
//!
//! | | Movement | Combat |
//! |---|---|---|
//! | Math | `libm` over f64, quantized each tick (VC-6, VC-7) | integers only (VC-5) |
//! | Compared | within the D16 bands — ε_pos 1 cm, ε_vel 1 cm/s | bit-exact |
//! | Cheap check | speed / acceleration / teleport caps | fire rate, value ranges |
//! | Drifts across platforms? | by up to a quantum, legitimately | never |
//!
//! # What makes it a game rather than a corpus
//!
//! `orrery_conformance`'s reference kernel is deliberately *not* a game — it is
//! the smallest thing that still exercises both halves of the contract. Three
//! things separate this from it, and each one exists because a measurement
//! needs it:
//!
//! - **Archetypes with published limits.** Stage-1 checks are "impossible
//!   value" checks, and impossible is undefinable without a declared ceiling.
//!   The corpus kernel has no caps, so nothing about it can be checked cheaply.
//! - **Rules that refuse.** Cooldowns, weapon reach, a death state, an
//!   acceleration clamp. A kernel that accepts every input can be replayed but
//!   never *disagreed with*, and the gap between what a client asks for and
//!   what the rules grant is where cheating lives.
//! - **Discrete outcomes recorded in the emitter's own state.** See
//!   [`state`] — without it, an inflated damage roll is unadjudicable.
//!
//! # The shape of a step
//!
//! 1. The weapon cools by one tick.
//! 2. Orders apply in log order (VC-2), never re-sorted: thrust along the
//!    facing *then* turn; fire if the weapon is ready; absorb damage into
//!    shield, then hull, if it was fired from inside its own reach.
//! 3. Speed is clamped to the archetype ceiling, drag applies, position
//!    integrates, everything continuous snaps back onto the lattice (VC-7).
//!
//! Step 2 splits a shot across two entities' steps on purpose. The attacker
//! decides *that* it fired and how hard; the target decides whether the shot
//! reached it. Neither reads the other's live state, which is what lets a
//! witness re-execute either one alone — see [`order`].

pub mod archetype;
pub mod invariants;
pub mod order;
pub mod pilot;
pub mod state;

use orrery_compose::{
    AmbiguityDetection, CanonicalSchedule, CompatibilityManifest, ComponentCapabilities,
    ComponentSchemaId, ComponentSchemaManifest, EventVocabularyId, ExecutorPolicy, GameId,
    InputVocabularyId, ManifestFormatVersion, ModuleId, ModuleManifest, ModuleVersion,
    PersistenceCapability, ProfileId, ProjectionVersion, ReplicationCapability, RollbackCapability,
    StateSectionId, WitnessCapability, WriteAuthorityCapability,
};
use orrery_core::{
    Invariant, OrderedInputs, QPos, QVel, Ruleset, StateView, StepOutput, TickRng, TICK_HZ,
};
use orrery_protocol::{PersistId, RulesetId, Tick};
use rand_core::RngCore;

use crate::game::{Game, GameMeta, Tamper};
use archetype::Archetype;
use order::{Order, Outcome};
use state::{Craft, PITCH_LIMIT_URAD, TAU_URAD};

/// The fixed tick duration in seconds (VC-1). A constant, never a measurement.
const DT: f64 = 1.0 / TICK_HZ as f64;

/// Velocity lost to drag per second, per mille. Small: enough that a craft
/// which stops thrusting visibly slows, little enough that the speed clamp
/// rather than drag is what bounds a thrusting craft.
///
/// Held as an integer because [`invariants`] needs the same number to compute
/// how much a velocity may legally change in a tick, and a stage-1 check that
/// derived its limit from a *different* drag constant than the rules use is a
/// false positive with a very long fuse.
pub const DRAG_PER_SEC_PER_MILLE: i64 = 50;

/// Drag as the fraction `step` multiplies by.
const DRAG_PER_SEC: f64 = DRAG_PER_SEC_PER_MILLE as f64 / 1_000.0;

/// Spawn ring radius, millimetres. Chosen against the archetype reaches: a
/// population spawned here and orbiting under [`pilot`] stays inside the
/// interceptor's 400 m weapon range for the whole window, so a combat scenario
/// does not quietly decay into a coasting one.
const SPAWN_RADIUS_MM: f64 = 150_000.0;

/// The golden angle in micro-radians, used to place spawns.
///
/// Any fixed angular step would place two craft on top of each other for some
/// population size; the golden angle is the step that never does.
const GOLDEN_ANGLE_URAD: i64 = 2_399_963;

/// Skirmish's build identity.
///
/// The digest is a placeholder pattern rather than a real build hash: nothing
/// in the tree computes one yet, and a fabricated-looking constant is more
/// honest than a plausible-looking one. **Bump `version` whenever the rules
/// change** — the committed golden chains are only meaningful against fixed
/// rules, and a silent rules change would present as a determinism failure
/// rather than as what it is.
///
/// # Why #758's snapshot isolation did *not* bump this
///
/// Regolith went v19 → v20 when neighbour reads moved to the tick-start
/// snapshot. Skirmish stayed at 2, and the reason is structural rather than
/// evidential: `Skirmish` declares `Ruleset::max_neighbor_reads() == 0` — it
/// does not override the default — and its `step` contains no
/// `StateView::neighbor` call at any depth. There is no input under which a
/// pre-#758 and a post-#758 Skirmish build reach the changed code, so no pair
/// of peers either side of it can disagree about any tick.
///
/// That is a different claim from "no golden moved", which is all the corpus
/// could have told us. A ruleset version is a claim about *these rules*, and
/// bumping it for an engine change no rule can observe would make the version
/// track the executor instead — after which every core refactor would owe a
/// bump on every game, and a real mismatch would be one more number in a
/// stream of them. `skirmish.rs`'s `skirmish_declares_no_neighbour_reads`
/// holds the premise, so the day Skirmish grows a neighbour read the decision
/// is revisited rather than inherited.
pub const SKIRMISH_RULESET: RulesetId = RulesetId {
    version: 2,
    digest: [0x5C; 32],
};

/// Component identifiers, for the §2 classification, aliased from the
/// reviewed ledger.
///
/// # Why these moved into `orrery_compose::registry` (#761)
///
/// #750 left these declared inline with an explicit note: Skirmish had no
/// [`orrery_compose::CompatibilityManifest`], so there was no
/// `component_schemas` field to populate and no reviewed allocation table for
/// one to agree with, and the ids were consumed only by
/// `Skirmish::classify_component`.
///
/// #761 retired that method. With it gone, Skirmish's three classification
/// facts have to be **declarations** or they cease to exist, so the note's
/// deferred work is now the work: the ids live in
/// [`orrery_compose::registry::skirmish`], and [`SKIRMISH_COMPOSITION`] states
/// them with their owning module and D45's five capability dimensions. The
/// values are unchanged — this is a move of where a fact is stated, not of
/// what it is.
pub mod components {
    use orrery_core::ComponentTypeId;

    /// The craft's verifiable state: position, velocity, hull, shield, counters.
    pub const CRAFT: ComponentTypeId = orrery_compose::registry::skirmish::CRAFT;
    /// Cumulative hull scarring — persisted so a ship looks fought-in across
    /// sessions, never adjudicated.
    pub const HULL_WEAR: ComponentTypeId = orrery_compose::registry::skirmish::HULL_WEAR;
    /// Engine trail. Never persisted, never verified.
    pub const ENGINE_TRAIL: ComponentTypeId = orrery_compose::registry::skirmish::ENGINE_TRAIL;
}

/// Skirmish's one statically linked rule domain.
///
/// One module, and the honesty of that is the reason Skirmish could be given a
/// manifest inside #761 rather than waiting for a composition-root lane: there
/// is no module split to get wrong. [`Ruleset::CoreState`] is a single
/// [`Craft`], every order and outcome in [`order`] concerns it, and
/// [`Ruleset::step`] is the sole executor entry point here exactly as it is
/// for Regolith.
pub const SKIRMISH_MODULES: &[ModuleManifest] = &[ModuleManifest {
    id: ModuleId("skirmish.craft"),
    version: ModuleVersion(1),
    dependencies: &[],
    state_sections: &[StateSectionId("craft")],
    inputs: &[InputVocabularyId("craft-control-and-damage")],
    events: &[EventVocabularyId("craft-damage-and-destruction")],
    schedule_stages: &[],
}];

/// Skirmish's component-schema table, stated from the reviewed ledger.
///
/// The derived half of the split #750 settled and #761 extended to this game:
/// [`orrery_compose::registry::skirmish`] is canonical for D45 clause (a)'s
/// `(ComponentTypeId, SchemaVersion)` pair, and this table restates that pair
/// with the two things a ledger does not carry — the owning module and the
/// five capability dimensions. The agreement is asserted, not assumed; see
/// [`composition_tests::the_manifest_schema_table_agrees_with_the_reviewed_registry`].
///
/// **These three rows are exactly the three classification facts the retired
/// `classify_component` stated**, restated as ADR-0045 clause (d) profiles:
///
/// - `CRAFT` is the `Core` profile — `P1` bulk persistence, `R1` rollback
///   membership, `W2` replay-adjudicated, `N1` interest-replicated, `A1`
///   lease-holder. Legal under every clause (e) prohibition that reaches it:
///   `W2` has its single fenced writer (IV-1) and its deterministic
///   [`orrery_core::CoreCodec`] (IV-2); `P1` is not the `P2` IV-3 and IV-5
///   constrain; the craft is a durable [`orrery_protocol::PersistId`], not an
///   ephemeral identity (IV-4); and `N1` is not paired with `A0` (IV-6).
/// - `HULL_WEAR` is the `Bulk` profile — persisted so a ship looks fought-in
///   across sessions, invariant-checked only, never replay-adjudicated.
/// - `ENGINE_TRAIL` is the `Cosmetic-local` profile, all zeros: the row is
///   inert-but-legal by clause (e)'s own note, and it is declared rather than
///   omitted so the ledger records that the id is permanently spent.
pub const SKIRMISH_COMPONENT_SCHEMAS: &[ComponentSchemaManifest] = &[
    ComponentSchemaManifest {
        owner: ModuleId("skirmish.craft"),
        id: ComponentSchemaId {
            component: components::CRAFT,
            version: orrery_protocol::atrest::SCHEMA_V0,
        },
        capabilities: ComponentCapabilities {
            persistence: PersistenceCapability::Bulk,
            rollback: RollbackCapability::Included,
            witness: WitnessCapability::ReplayAdjudicated,
            replication: ReplicationCapability::InterestReplicated,
            write_authority: WriteAuthorityCapability::LeaseHolder,
        },
    },
    ComponentSchemaManifest {
        owner: ModuleId("skirmish.craft"),
        id: ComponentSchemaId {
            component: components::HULL_WEAR,
            version: orrery_protocol::atrest::SCHEMA_V0,
        },
        capabilities: ComponentCapabilities {
            persistence: PersistenceCapability::Bulk,
            rollback: RollbackCapability::Excluded,
            witness: WitnessCapability::InvariantChecked,
            replication: ReplicationCapability::InterestReplicated,
            write_authority: WriteAuthorityCapability::LeaseHolder,
        },
    },
    ComponentSchemaManifest {
        owner: ModuleId("skirmish.craft"),
        id: ComponentSchemaId {
            component: components::ENGINE_TRAIL,
            version: orrery_protocol::atrest::SCHEMA_V0,
        },
        capabilities: ComponentCapabilities {
            persistence: PersistenceCapability::None,
            rollback: RollbackCapability::Excluded,
            witness: WitnessCapability::Unwatched,
            replication: ReplicationCapability::None,
            write_authority: WriteAuthorityCapability::Local,
        },
    },
];

/// The assembled, validated-at-registration composition manifest for Skirmish.
///
/// # Why this landed with #761 rather than in a lane of its own
///
/// #750's note deferred this because a manifest "carries the module split, the
/// schedule topology and the determinism-profile claim with it". That is still
/// true, and each of the three is answered here rather than waved past:
///
/// - **Module split** — [`SKIRMISH_MODULES`], one module. Skirmish is a single
///   `Craft` state with one order and one outcome vocabulary; there is no
///   split to get wrong.
/// - **Schedule topology** — empty stages, and the one rider that is genuinely
///   deferred rather than answered. Empty is not a placeholder: it is the
///   accurate statement that this build declares no stage decomposition, which
///   is true — [`Ruleset::step`] here is one undivided body, unlike Regolith's
///   since #745/#764. [`orrery_compose::validate`] holds the table to
///   [`AmbiguityDetection::Error`] and a single-threaded executor either way,
///   so nothing is claimed that is not run. Decomposing Skirmish into declared
///   systems with a pinned digest is D43 clause (g) work of the same shape
///   #764 did for Regolith, and it is its own lane; it does not block, and is
///   not blocked by, stating where this game's components are classified.
/// - **Profile claim** — [`ProfileId`]`("d9")`, the same D9 verifiable-core
///   envelope Skirmish has always run inside as a [`Ruleset`]: quantized
///   state, a hand-written [`orrery_core::CoreCodec`], no ambient inputs, and
///   `core-gates.sh` scanning this crate for exactly that.
///
/// What it buys is the reason it could not wait: with `classify_component`
/// retired, a schema table with no manifest around it would be a declaration
/// [`orrery_compose::validate`] never sees and no module table ever owns —
/// weaker than the method it replaces. Nothing hashes or admits on a manifest
/// today, so stating one moves no canonical byte.
pub const SKIRMISH_COMPOSITION: CompatibilityManifest = CompatibilityManifest {
    game_id: GameId("skirmish"),
    manifest_format_version: ManifestFormatVersion(1),
    protocol_version: 6,
    toolchain_stamp: "rust-2024",
    ruleset: SKIRMISH_RULESET,
    modules: SKIRMISH_MODULES,
    component_schemas: SKIRMISH_COMPONENT_SCHEMAS,
    schedule: CanonicalSchedule {
        stages: &[],
        ordering_edges: &[],
        ambiguities: &[],
        ambiguity_detection: AmbiguityDetection::Error,
        executor_policy: ExecutorPolicy::SingleThreaded,
    },
    canonical_constants: &[],
    projection_version: ProjectionVersion(1),
    profile_id: ProfileId("d9"),
    removed_components: &[],
};

/// The Skirmish rules.
///
/// `tamper` is `None` for the rules as shipped. A tampered build reports the
/// *honest* [`SKIRMISH_RULESET`] from [`Ruleset::id`], which is the point: a
/// modified client claims to be running the rules, and the claim is what a
/// witness holds it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Skirmish {
    tamper: Option<Tamper>,
}

impl Skirmish {
    /// The rules as shipped.
    #[must_use]
    pub const fn honest() -> Self {
        Self { tamper: None }
    }

    /// A build that breaks `tamper`.
    #[must_use]
    pub const fn cheating(tamper: Tamper) -> Self {
        Self {
            tamper: Some(tamper),
        }
    }

    /// Which cheat this build carries, if any.
    #[must_use]
    pub const fn tamper(self) -> Option<Tamper> {
        self.tamper
    }

    /// Movement ceilings, as this build enforces them.
    ///
    /// 1.5× is P4's demo criterion stated literally, and it is applied to both
    /// ceilings because raising one alone produces a craft that accelerates
    /// illegally into a legal speed — a strictly easier cheat to catch than
    /// the one the criterion names.
    const fn movement_cap(self, base: i64) -> i64 {
        match self.tamper {
            Some(Tamper::SpeedMultiplier) => base * 3 / 2,
            _ => base,
        }
    }

    /// A damage roll, as this build resolves it.
    const fn damage(self, rolled: i32) -> i32 {
        match self.tamper {
            Some(Tamper::DamageInflation) => rolled * 2,
            _ => rolled,
        }
    }

    /// Whether this build waits for the weapon to cool.
    const fn honours_cooldown(self) -> bool {
        !matches!(self.tamper, Some(Tamper::NoCooldown))
    }
}

impl Ruleset for Skirmish {
    type CoreState = Craft;
    type CoreInput = Order;
    type CoreEvent = Outcome;

    fn id(&self) -> RulesetId {
        SKIRMISH_RULESET
    }

    fn invariants(&self) -> &[Invariant<Craft>] {
        invariants::INVARIANTS
    }

    fn step(
        &self,
        view: &mut StateView<'_, Craft>,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> StepOutput<Outcome> {
        let mut events = Vec::new();

        // Whose tick this is. From the executor, not from the state, so a
        // craft cannot sign someone else's shot.
        let me = view.entity();
        let own = view.own();
        let archetype = own.archetype;
        let limits = archetype.limits();
        let origin = own.pos;
        // Read off the lattice into f64 for the continuous pass. The
        // conversion is exact — every quantum is representable — so the only
        // platform-dependent step below is `libm` itself.
        let (mut px, mut py, mut pz) = own.pos.to_metres();
        let (mut vx, mut vy, mut vz) = own.vel.to_metres_per_sec();
        let mut yaw = own.yaw_urad;
        let mut pitch = own.pitch_urad;
        let mut hull = own.hull;
        let mut shield = own.shield;
        let mut shots = own.shots;
        let mut damage_dealt = own.damage_dealt;
        // The weapon cools at the top of the tick, before this tick's orders
        // are read: a shot at tick T sets N, and T + N is the first tick that
        // may fire again.
        let mut cooldown = own.cooldown.saturating_sub(1);
        let mut disabled = !own.alive();

        for order in inputs.iter() {
            match order {
                // A wreck neither steers nor shoots. It still drifts, and it
                // still takes hits — which is why only the two action arms
                // check this.
                Order::Thrust { .. } | Order::Fire { .. } if disabled => {}

                Order::Thrust {
                    accel_mmss,
                    yaw_urad,
                    pitch_urad,
                } => {
                    let cap = self.movement_cap(limits.max_accel_mmss);
                    let accel = i64::from(*accel_mmss).clamp(0, cap) as f64 / 1_000.0;
                    let theta = f64::from(yaw) / 1_000_000.0;
                    let phi = f64::from(pitch) / 1_000_000.0;
                    // VC-6: libm, not std. These are the lines the determinism
                    // matrix exists to compare.
                    let horizontal = libm::cos(phi);
                    vx += accel * horizontal * libm::cos(theta) * DT;
                    vy += accel * libm::sin(phi) * DT;
                    vz += accel * horizontal * libm::sin(theta) * DT;
                    // Thrust along the old facing, then turn: the order is
                    // arbitrary but it has to be fixed, and this way a turn
                    // never redirects the same tick's acceleration.
                    yaw = yaw.wrapping_add(*yaw_urad).rem_euclid(TAU_URAD);
                    pitch = pitch
                        .saturating_add(*pitch_urad)
                        .clamp(-PITCH_LIMIT_URAD, PITCH_LIMIT_URAD);
                }

                Order::Fire { target } => {
                    if cooldown > 0 && self.honours_cooldown() {
                        continue;
                    }
                    // Nothing about the target is read. The adjudicator holds
                    // one entity — `ReplayHarness::load_claimed_snapshot`
                    // installs the state its claim commits to and no other —
                    // so a neighbour read here would resolve one way under
                    // play and another under replay, and an honest craft would
                    // hash-mismatch. Whether the shot connects is the target's
                    // question, answered on the `Damage` arm below from what
                    // this event carries.
                    //
                    // VC-3/VC-5: the roll is integer and seeded, so the damage
                    // a shot deals is bit-identical on every platform. Drawn
                    // whenever the weapon fires, which is now the only thing
                    // the attacker's side decides.
                    let roll = rng.next_u32() % limits.damage_spread.max(1);
                    let amount = self.damage(
                        i32::try_from(limits.damage_base.saturating_add(roll)).unwrap_or(i32::MAX),
                    );
                    shots = shots.saturating_add(1);
                    damage_dealt = damage_dealt.saturating_add(amount.unsigned_abs().into());
                    cooldown = limits.cooldown_ticks;
                    events.push(Outcome::DamageDealt {
                        attacker: me,
                        target: *target,
                        amount,
                        attacker_pos: origin,
                        attacker_archetype: archetype,
                    });
                }

                Order::Damage {
                    amount,
                    from,
                    from_pos,
                    from_archetype,
                } => {
                    // Reach, resolved where both sides of the comparison are
                    // own state: this craft's position at the top of its tick,
                    // and the attacker's, which arrived in the event. The
                    // reach itself is derived from the attacker's *archetype*
                    // rather than read off the wire, so a tampered build
                    // cannot grant itself a longer gun — the archetype is
                    // hashed into the attacker's own state and `value-range`
                    // refuses a craft that relabels it.
                    let reach = from_archetype.limits().range_mm;
                    if origin.distance_squared(*from_pos) > reach_sq(reach) {
                        continue;
                    }
                    // Integer only, shields absorbing first, hull floored at
                    // zero — a negative hull would be a value-range violation
                    // manufactured by the rules themselves.
                    let incoming = (*amount).max(0);
                    let absorbed = incoming.min(shield.max(0));
                    shield -= absorbed;
                    let through = incoming - absorbed;
                    if through > 0 && hull > 0 {
                        hull = (hull - through).max(0);
                        if hull == 0 {
                            disabled = true;
                            events.push(Outcome::Destroyed { by: *from });
                        }
                    }
                }
            }
        }

        // Clamp, then drag, then integrate. `sqrt` is the second transcendental
        // under test. Clamping before drag is what keeps an honest sample at or
        // below the ceiling: drag can only ever take speed further under it.
        let speed = libm::sqrt(vx * vx + vy * vy + vz * vz);
        let ceiling = self.movement_cap(limits.max_speed_mms) as f64 / 1_000.0;
        if speed > ceiling && speed > 0.0 {
            let scale = ceiling / speed;
            vx *= scale;
            vy *= scale;
            vz *= scale;
        }
        let retained = 1.0 - DRAG_PER_SEC * DT;
        vx *= retained;
        vy *= retained;
        vz *= retained;
        px += vx * DT;
        py += vy * DT;
        pz += vz * DT;

        let craft = view.own_mut();
        // Back onto the lattice (VC-7). The executor quantizes again after the
        // step; doing it here too means the value these rules believe and the
        // value that gets hashed are the same one.
        craft.pos = QPos::from_metres(px, py, pz);
        craft.vel = QVel::from_metres_per_sec(vx, vy, vz);
        craft.yaw_urad = yaw;
        craft.pitch_urad = pitch;
        craft.hull = hull;
        craft.shield = shield;
        craft.cooldown = cooldown;
        craft.shots = shots;
        craft.damage_dealt = damage_dealt;

        StepOutput { events }
    }
}

/// Squared weapon reach, in squared millimetres. `i128` for the same reason
/// [`QPos::distance_squared`] returns one: a squared `i64` millimetre distance
/// overflows `i64` at a few kilometres, and a reach check that silently
/// wrapped would let a shot land from anywhere.
const fn reach_sq(range_mm: i64) -> i128 {
    (range_mm as i128) * (range_mm as i128)
}

impl Game for Skirmish {
    const META: GameMeta = GameMeta {
        name: "skirmish",
        summary:
            "small craft: kinematic movement over libm, integer combat with cooldowns and reach",
        ruleset: SKIRMISH_RULESET,
    };

    const COMPOSITION: CompatibilityManifest = SKIRMISH_COMPOSITION;

    const GOLDEN_CHAINS: &'static [(&'static str, [u8; 32])] = &crate::golden::SKIRMISH;

    fn honest() -> Self {
        Skirmish::honest()
    }

    fn tampered(tamper: Tamper) -> Option<Self> {
        Some(Skirmish::cheating(tamper))
    }

    fn spawn(&self, _entity: PersistId, slot: u64) -> Craft {
        let archetype = Archetype::for_slot(slot);
        #[allow(clippy::cast_possible_wrap)]
        let angle_urad = (slot as i64).saturating_mul(GOLDEN_ANGLE_URAD) % i64::from(TAU_URAD);
        let angle = angle_urad as f64 / 1_000_000.0;
        let pos = QPos::from_metres(
            SPAWN_RADIUS_MM * libm::cos(angle) / 1_000.0,
            0.0,
            SPAWN_RADIUS_MM * libm::sin(angle) / 1_000.0,
        );
        // Facing along the ring, so thrusting turns into an orbit rather than
        // into an escape.
        let yaw = i32::try_from(angle_urad).unwrap_or(0) + TAU_URAD / 4;
        Craft::spawned(archetype, pos, yaw)
    }

    fn honest_inputs(
        &self,
        _entity: PersistId,
        slot: u64,
        tick: Tick,
        peers: &[PersistId],
        rng: &mut TickRng,
        out: &mut Vec<Order>,
    ) {
        pilot::honest_orders(slot, tick, peers, rng, out);
    }

    fn deliver(&self, event: &Outcome) -> Option<(PersistId, Order)> {
        match event {
            Outcome::DamageDealt {
                attacker,
                target,
                amount,
                attacker_pos,
                attacker_archetype,
            } => Some((
                *target,
                Order::Damage {
                    amount: *amount,
                    from: *attacker,
                    from_pos: *attacker_pos,
                    from_archetype: *attacker_archetype,
                },
            )),
            // A craft's death is news about itself. Nothing consumes it: the
            // durable half of a kill is a P5 intent, adjudicated against this
            // record rather than granted by it.
            Outcome::Destroyed { .. } => None,
        }
    }

    fn trajectory(state: &Craft) -> (QPos, QVel) {
        (state.pos, state.vel)
    }
}

#[cfg(test)]
mod composition_tests {
    use super::{SKIRMISH_COMPONENT_SCHEMAS, SKIRMISH_COMPOSITION};
    use orrery_compose::registry::skirmish::COMPONENT_TYPE_IDS;
    use orrery_compose::{profile_of, CapabilityProfile};
    use orrery_core::CoreClass;

    #[test]
    fn the_composition_manifest_validates() {
        assert_eq!(orrery_compose::validate(&SKIRMISH_COMPOSITION), Ok(()));
    }

    /// The two-direction guard #754 built for Regolith, for Skirmish.
    ///
    /// Without it, `SKIRMISH_COMPOSITION.component_schemas` could name a
    /// component or a schema version the reviewed ledger never allocated, or
    /// silently drop one it did — and since #761 that table is the only place
    /// Skirmish states classification at all.
    #[test]
    fn the_manifest_schema_table_agrees_with_the_reviewed_registry() {
        let reviewed: Vec<_> = COMPONENT_TYPE_IDS
            .iter()
            .map(|entry| (entry.id, entry.schema_version))
            .collect();
        let manifest: Vec<_> = SKIRMISH_COMPONENT_SCHEMAS
            .iter()
            .map(|schema| (schema.id.component, schema.id.version))
            .collect();
        assert_eq!(
            manifest, reviewed,
            "SKIRMISH_COMPOSITION.component_schemas must state exactly the \
             reviewed (ComponentTypeId, SchemaVersion) rows in \
             orrery_compose::registry::skirmish::COMPONENT_TYPE_IDS"
        );
    }

    /// Every declared row names one of ADR-0045 clause (d)'s profiles, and the
    /// three names are the three the retired `classify_component` returned.
    ///
    /// The tripwire A5 §6.2 asks for: a typo'd dimension lands on no profile
    /// and fails here by name rather than quietly re-filing a component.
    #[test]
    fn every_declared_row_names_a_known_profile() {
        let derived: Vec<Option<CoreClass>> = SKIRMISH_COMPONENT_SCHEMAS
            .iter()
            .map(|schema| {
                profile_of(schema.capabilities).unwrap_or_else(|| {
                    panic!(
                        "component {:?} declares a capability combination \
                             ADR-0045 clause (d) does not name",
                        schema.id.component
                    )
                })
            })
            .map(CapabilityProfile::core_class)
            .collect();
        assert_eq!(
            derived,
            vec![
                Some(CoreClass::Core),
                Some(CoreClass::Bulk),
                Some(CoreClass::Cosmetic),
            ],
            "CRAFT, HULL_WEAR and ENGINE_TRAIL must still derive the classes \
             the retired classify_component stated for them"
        );
    }
}
