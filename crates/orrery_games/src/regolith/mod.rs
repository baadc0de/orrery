//! **Regolith** — planar combat over a weapon table.
//!
//! This is intentionally a sibling, not an edit, of `skirmish`. Kinematics are
//! inherited exactly; the honest input source locks pitch to zero. The only
//! grammar change is that a shot names `WeaponKind`, whose legitimacy is
//! established by the shooter's own hashed `Craft.weapon` state.

pub mod archetype;
pub mod invariants;
pub mod order;
pub mod pilot;
pub mod state;
pub mod weapon;

use crate::game::{Game, GameMeta, Tamper};
use archetype::Archetype;
use order::{Order, Outcome};
use orrery_core::{
    ComponentTypeId, CoreClass, Invariant, OrderedInputs, QPos, QVel, Ruleset, StateView,
    StepOutput, TickRng, TICK_HZ,
};
use orrery_protocol::{PersistId, RulesetId, Tick};
use rand_core::RngCore;
use state::{Craft, PITCH_LIMIT_URAD, TAU_URAD};

const DT: f64 = 1.0 / TICK_HZ as f64;
/// Drag shared by rules and stage-1 acceleration bounds.
pub const DRAG_PER_SEC_PER_MILLE: i64 = 50;
const DRAG_PER_SEC: f64 = DRAG_PER_SEC_PER_MILLE as f64 / 1_000.0;
const SPAWN_RADIUS_MM: f64 = 150_000.0;
const GOLDEN_ANGLE_URAD: i64 = 2_399_963;

/// Regolith v1's fixed rules identity.
pub const REGOLITH_RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0x52; 32],
};

/// Component classifications.
pub mod components {
    use orrery_core::ComponentTypeId;

    /// Verifiable craft state.
    pub const CRAFT: ComponentTypeId = ComponentTypeId(1);
}

/// Regolith rules, optionally carrying one deliberate P4 tamper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Regolith {
    tamper: Option<Tamper>,
}
impl Regolith {
    /// Honest rules.
    #[must_use]
    pub const fn honest() -> Self {
        Self { tamper: None }
    }
    /// A modified build which still claims the honest identity.
    #[must_use]
    pub const fn cheating(tamper: Tamper) -> Self {
        Self {
            tamper: Some(tamper),
        }
    }
    const fn movement_cap(self, base: i64) -> i64 {
        match self.tamper {
            Some(Tamper::SpeedMultiplier) => base * 3 / 2,
            _ => base,
        }
    }
    const fn damage(self, roll: i32) -> i32 {
        match self.tamper {
            Some(Tamper::DamageInflation) => roll * 2,
            _ => roll,
        }
    }
    const fn honours_cooldown(self) -> bool {
        !matches!(self.tamper, Some(Tamper::NoCooldown))
    }
}
impl Ruleset for Regolith {
    type CoreState = Craft;
    type CoreInput = Order;
    type CoreEvent = Outcome;
    fn id(&self) -> RulesetId {
        REGOLITH_RULESET
    }
    fn classify_component(&self, component: ComponentTypeId) -> CoreClass {
        if component == components::CRAFT {
            CoreClass::Core
        } else {
            CoreClass::Cosmetic
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
        let me = view.entity();
        let own = view.own();
        let limits = own.archetype.limits();
        let weapon = own.weapon.weapon();
        let origin = own.pos;
        let (mut px, mut py, mut pz) = own.pos.to_metres();
        let (mut vx, mut vy, mut vz) = own.vel.to_metres_per_sec();
        let mut yaw = own.yaw_urad;
        let mut pitch = own.pitch_urad;
        let mut hull = own.hull;
        let mut shield = own.shield;
        let mut shots = own.shots;
        let mut damage_dealt = own.damage_dealt;
        let mut cooldown = own.cooldown.saturating_sub(1);
        let mut disabled = !own.alive();
        for order in inputs.iter() {
            match order {
                Order::Thrust { .. } | Order::Fire { .. } if disabled => {}
                Order::Thrust {
                    accel_mmss,
                    yaw_urad,
                    pitch_urad,
                } => {
                    let accel = i64::from(*accel_mmss)
                        .clamp(0, self.movement_cap(limits.max_accel_mmss))
                        as f64
                        / 1_000.0;
                    let theta = f64::from(yaw) / 1_000_000.0;
                    let phi = f64::from(pitch) / 1_000_000.0;
                    let horizontal = libm::cos(phi);
                    vx += accel * horizontal * libm::cos(theta) * DT;
                    vy += accel * libm::sin(phi) * DT;
                    vz += accel * horizontal * libm::sin(theta) * DT;
                    yaw = yaw.wrapping_add(*yaw_urad).rem_euclid(TAU_URAD);
                    pitch = pitch
                        .saturating_add(*pitch_urad)
                        .clamp(-PITCH_LIMIT_URAD, PITCH_LIMIT_URAD);
                }
                Order::Fire { target } => {
                    if cooldown > 0 && self.honours_cooldown() {
                        continue;
                    }
                    for _ in 0..weapon.rolls {
                        let roll = rng.next_u32() % weapon.damage_spread.max(1);
                        let amount = self.damage(
                            i32::try_from(weapon.damage_base.saturating_add(roll))
                                .unwrap_or(i32::MAX),
                        );
                        damage_dealt =
                            damage_dealt.saturating_add(u64::from(amount.unsigned_abs()));
                        events.push(Outcome::DamageDealt {
                            attacker: me,
                            target: *target,
                            amount,
                            attacker_pos: origin,
                            attacker_weapon: own.weapon,
                        });
                    }
                    shots = shots.saturating_add(1);
                    cooldown = weapon.cooldown_ticks;
                }
                Order::Damage {
                    amount,
                    from,
                    from_pos,
                    from_weapon,
                } => {
                    if origin.distance_squared(*from_pos) > reach_sq(from_weapon.weapon().reach_mm)
                    {
                        continue;
                    }
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
const fn reach_sq(range_mm: i64) -> i128 {
    (range_mm as i128) * (range_mm as i128)
}
impl Game for Regolith {
    const META: GameMeta = GameMeta {
        name: "regolith",
        summary: "planar craft: inherited kinematics and weapon-table combat",
        ruleset: REGOLITH_RULESET,
    };
    const GOLDEN_CHAINS: &'static [(&'static str, [u8; 32])] = &crate::golden::REGOLITH;
    fn honest() -> Self {
        Self::honest()
    }
    fn tampered(tamper: Tamper) -> Option<Self> {
        Some(Self::cheating(tamper))
    }
    fn spawn(&self, _entity: PersistId, slot: u64) -> Craft {
        let archetype = Archetype::for_slot(slot);
        let angle_urad = (slot as i64).saturating_mul(GOLDEN_ANGLE_URAD) % i64::from(TAU_URAD);
        let angle = angle_urad as f64 / 1_000_000.0;
        let pos = QPos::from_metres(
            SPAWN_RADIUS_MM * libm::cos(angle) / 1_000.0,
            0.0,
            SPAWN_RADIUS_MM * libm::sin(angle) / 1_000.0,
        );
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
                attacker_weapon,
            } => Some((
                *target,
                Order::Damage {
                    amount: *amount,
                    from: *attacker,
                    from_pos: *attacker_pos,
                    from_weapon: *attacker_weapon,
                },
            )),
            Outcome::Destroyed { .. } => None,
        }
    }
    fn trajectory(state: &Craft) -> (QPos, QVel) {
        (state.pos, state.vel)
    }
}
