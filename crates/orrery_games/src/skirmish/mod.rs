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
//!    facing *then* turn; fire if the weapon is ready and the target is alive,
//!    present and in reach; absorb damage into shield, then hull.
//! 3. Speed is clamped to the archetype ceiling, drag applies, position
//!    integrates, everything continuous snaps back onto the lattice (VC-7).

pub mod archetype;
pub mod invariants;
pub mod order;
pub mod pilot;
pub mod state;

use orrery_core::{
    ComponentTypeId, CoreClass, Invariant, OrderedInputs, QPos, QVel, Ruleset, StateView,
    StepOutput, TickRng, TICK_HZ,
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
pub const SKIRMISH_RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0x5C; 32],
};

/// Component identifiers, for the §2 classification.
pub mod components {
    use orrery_core::ComponentTypeId;

    /// The craft's verifiable state: position, velocity, hull, shield, counters.
    pub const CRAFT: ComponentTypeId = ComponentTypeId(1);
    /// Cumulative hull scarring — persisted so a ship looks fought-in across
    /// sessions, never adjudicated.
    pub const HULL_WEAR: ComponentTypeId = ComponentTypeId(2);
    /// Engine trail. Never persisted, never verified.
    pub const ENGINE_TRAIL: ComponentTypeId = ComponentTypeId(3);
}

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

    fn classify_component(&self, component: ComponentTypeId) -> CoreClass {
        match component {
            components::CRAFT => CoreClass::Core,
            components::HULL_WEAR => CoreClass::Bulk,
            // Everything unnamed included: an unclassified component is
            // cosmetic, so forgetting to classify costs persistence rather
            // than silently admitting something to adjudication.
            _ => CoreClass::Cosmetic,
        }
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
                    // Reading the target closes the input set: the read is
                    // recorded, so a replay never needs the neighbour's live
                    // state, and an authority that fabricates neighbour state
                    // to justify a hit produces evidence against itself.
                    let Some(other) = view.neighbor(*target) else {
                        continue;
                    };
                    if !other.alive()
                        || origin.distance_squared(other.pos) > reach_sq(limits.range_mm)
                    {
                        continue;
                    }
                    // VC-3/VC-5: the roll is integer and seeded, so the damage
                    // a shot deals is bit-identical on every platform. Drawn
                    // only when the shot actually happens, so the RNG stream
                    // records what was fired rather than what was asked for.
                    let roll = rng.next_u32() % limits.damage_spread.max(1);
                    let amount = self.damage(
                        i32::try_from(limits.damage_base.saturating_add(roll)).unwrap_or(i32::MAX),
                    );
                    shots = shots.saturating_add(1);
                    damage_dealt = damage_dealt.saturating_add(amount.unsigned_abs().into());
                    cooldown = limits.cooldown_ticks;
                    events.push(Outcome::DamageDealt {
                        target: *target,
                        amount,
                    });
                }

                Order::Damage { amount } => {
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
                            events.push(Outcome::Destroyed);
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
            Outcome::DamageDealt { target, amount } => {
                Some((*target, Order::Damage { amount: *amount }))
            }
            Outcome::Destroyed => None,
        }
    }

    fn trajectory(state: &Craft) -> (QPos, QVel) {
        (state.pos, state.vel)
    }
}
