//! **Regolith** — planar combat plus materialized, replayable rock splits.
//!
//! A split is described entirely by an ordered event. Its parent records the
//! monotone split counter in its own state, so materialization is adjudicable.

pub mod archetype;
pub mod invariants;
pub mod order;
pub mod pilot;
pub mod state;
pub mod weapon;

use crate::game::{Game, GameMeta, Tamper};
use archetype::Archetype;
use order::{ChildSpec, Order, Outcome};
use orrery_core::{
    ComponentTypeId, CoreClass, EntityMaterialization, Invariant, OrderedInputs, QPos, QVel,
    Ruleset, StateView, StepOutput, TickRng, TICK_HZ,
};
use orrery_protocol::{PersistId, RulesetId, Tick};
use rand_core::RngCore;
use state::{Craft, RegolithState, Rock, RockTier, PITCH_LIMIT_URAD, TAU_URAD};

const DT: f64 = 1.0 / TICK_HZ as f64;
/// Drag shared by craft rules and stage-1 acceleration bounds.
pub const DRAG_PER_SEC_PER_MILLE: i64 = 50;
const DRAG_PER_SEC: f64 = DRAG_PER_SEC_PER_MILLE as f64 / 1_000.0;
const SPAWN_RADIUS_MM: f64 = 150_000.0;
const GOLDEN_ANGLE_URAD: i64 = 2_399_963;
/// Rocks reflect from this square island edge with integer velocity negation.
pub const ISLAND_BOUNDARY_MM: i64 = 1_000_000;
const JITTER_MIN_URAD: u32 = 785_398;
const JITTER_MAX_URAD: u32 = 1_308_997;

/// Regolith v2's rules identity. Rocks change rules and golden chains.
pub const REGOLITH_RULESET: RulesetId = RulesetId {
    version: 2,
    digest: [0x52; 32],
};

/// Component classifications.
pub mod components {
    use orrery_core::ComponentTypeId;
    /// Verifiable Regolith state, whether craft or rock.
    pub const STATE: ComponentTypeId = ComponentTypeId(1);
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
    type CoreState = RegolithState;
    type CoreInput = Order;
    type CoreEvent = Outcome;
    fn id(&self) -> RulesetId {
        REGOLITH_RULESET
    }
    fn classify_component(&self, component: ComponentTypeId) -> CoreClass {
        if component == components::STATE {
            CoreClass::Core
        } else {
            CoreClass::Cosmetic
        }
    }
    fn invariants(&self) -> &[Invariant<RegolithState>] {
        invariants::INVARIANTS
    }
    fn step(
        &self,
        view: &mut StateView<'_, RegolithState>,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> StepOutput<Outcome> {
        let me = view.entity();
        let (state, events) = match view.own().clone() {
            RegolithState::Craft(craft) => {
                let (craft, events) = self.step_craft(me, craft, inputs, rng);
                (RegolithState::Craft(craft), events)
            }
            RegolithState::Rock(rock) => {
                let (rock, events) = self.step_rock(me, rock, inputs, rng);
                (RegolithState::Rock(rock), events)
            }
        };
        *view.own_mut() = state;
        StepOutput { events }
    }
    fn materialize(&self, event: &Outcome, out: &mut Vec<EntityMaterialization<RegolithState>>) {
        if let Outcome::Split {
            generation,
            children,
            ..
        } = event
        {
            for child in children {
                out.push(EntityMaterialization::new(
                    child.id,
                    RegolithState::Rock(Rock::spawned(
                        child.tier,
                        generation.saturating_add(1),
                        child.pos,
                        child.vel,
                    )),
                ));
            }
        }
    }
}

impl Regolith {
    fn step_craft(
        &self,
        me: PersistId,
        own: Craft,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> (Craft, Vec<Outcome>) {
        let mut events = Vec::new();
        let limits = own.archetype.limits();
        let weapon = own.weapon.weapon();
        let origin = own.pos;
        let (mut px, mut py, mut pz) = own.pos.to_metres();
        let (mut vx, mut vy, mut vz) = own.vel.to_metres_per_sec();
        let (mut yaw, mut pitch, mut hull, mut shield) =
            (own.yaw_urad, own.pitch_urad, own.hull, own.shield);
        let (mut shots, mut damage_dealt, mut cooldown) =
            (own.shots, own.damage_dealt, own.cooldown.saturating_sub(1));
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
                    let reach = from_weapon
                        .weapon()
                        .reach_mm
                        .saturating_add(limits.radius_mm);
                    if origin.distance_squared(*from_pos) > reach_sq(reach) {
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
        (
            Craft {
                pos: QPos::from_metres(px, py, pz),
                vel: QVel::from_metres_per_sec(vx, vy, vz),
                yaw_urad: yaw,
                pitch_urad: pitch,
                hull,
                shield,
                cooldown,
                shots,
                damage_dealt,
                ..own
            },
            events,
        )
    }
    fn step_rock(
        &self,
        me: PersistId,
        mut rock: Rock,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> (Rock, Vec<Outcome>) {
        let mut events = Vec::new();
        let origin = rock.pos;
        if rock.hull > 0 {
            for order in inputs.iter() {
                if let Order::Damage {
                    amount,
                    from_pos,
                    from_weapon,
                    ..
                } = order
                {
                    let reach = from_weapon
                        .weapon()
                        .reach_mm
                        .saturating_add(rock.tier.limits().radius_mm);
                    if origin.distance_squared(*from_pos) <= reach_sq(reach) {
                        rock.hull = (rock.hull - (*amount).max(0)).max(0);
                    }
                }
            }
            if rock.hull == 0 {
                if let Some(child_tier) = rock.tier.child() {
                    let children = split_children(me, &rock, child_tier, rng);
                    events.push(Outcome::Split {
                        parent: me,
                        generation: rock.generation,
                        children,
                    });
                    rock.splits_done = rock.splits_done.saturating_add(1);
                }
            }
        }
        if rock.hull > 0 {
            rock.pos.x = rock.pos.x.saturating_add(rock.vel.x / i64::from(TICK_HZ));
            rock.pos.y = rock.pos.y.saturating_add(rock.vel.y / i64::from(TICK_HZ));
            rock.pos.z = rock.pos.z.saturating_add(rock.vel.z / i64::from(TICK_HZ));
            if rock.pos.x.abs() > ISLAND_BOUNDARY_MM {
                rock.vel.x = rock.vel.x.saturating_neg();
            }
            if rock.pos.y.abs() > ISLAND_BOUNDARY_MM {
                rock.vel.y = rock.vel.y.saturating_neg();
            }
            if rock.pos.z.abs() > ISLAND_BOUNDARY_MM {
                rock.vel.z = rock.vel.z.saturating_neg();
            }
        }
        (rock, events)
    }
}

fn split_children(
    parent: PersistId,
    rock: &Rock,
    tier: RockTier,
    rng: &mut TickRng,
) -> [ChildSpec; 2] {
    let jitter0 = uniform_jitter(rng);
    let jitter1 = uniform_jitter(rng);
    [
        child_spec(parent, rock, tier, 0, i64::from(jitter0)),
        child_spec(parent, rock, tier, 1, -i64::from(jitter1)),
    ]
}
fn uniform_jitter(rng: &mut TickRng) -> u32 {
    let width = JITTER_MAX_URAD - JITTER_MIN_URAD + 1;
    let limit = u32::MAX - u32::MAX % width;
    loop {
        let draw = rng.next_u32();
        if draw < limit {
            return JITTER_MIN_URAD + draw % width;
        }
    }
}
fn child_spec(
    parent: PersistId,
    rock: &Rock,
    tier: RockTier,
    slot: u8,
    signed_angle_urad: i64,
) -> ChildSpec {
    let angle = signed_angle_urad as f64 / 1_000_000.0;
    let (vx, vy, vz) = rock.vel.to_metres_per_sec();
    let scale = 1.4_f64;
    let (mut x, mut z) = (
        (vx * libm::cos(angle) - vz * libm::sin(angle)) * scale,
        (vx * libm::sin(angle) + vz * libm::cos(angle)) * scale,
    );
    let mut y = vy * scale;
    let speed = libm::sqrt(x * x + y * y + z * z);
    let ceiling = tier.limits().max_speed_mms as f64 / 1_000.0;
    if speed > ceiling && speed > 0.0 {
        let cap = ceiling / speed;
        x *= cap;
        y *= cap;
        z *= cap;
    }
    let vel = QVel::from_metres_per_sec(x, y, z);
    let speed =
        libm::sqrt((vel.x as f64).powi(2) + (vel.y as f64).powi(2) + (vel.z as f64).powi(2));
    let radius = rock.tier.limits().radius_mm as f64;
    let pos = if speed > 0.0 {
        QPos {
            x: rock
                .pos
                .x
                .saturating_add((vel.x as f64 * radius / speed) as i64),
            y: rock
                .pos
                .y
                .saturating_add((vel.y as f64 * radius / speed) as i64),
            z: rock
                .pos
                .z
                .saturating_add((vel.z as f64 * radius / speed) as i64),
        }
    } else {
        rock.pos
    };
    ChildSpec {
        id: child_id(parent, rock.generation, slot),
        tier,
        pos,
        vel,
    }
}
fn child_id(parent: PersistId, generation: u32, slot: u8) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regolith-rock");
    hasher.update(&parent.0.to_le_bytes());
    hasher.update(&generation.to_le_bytes());
    hasher.update(&[slot]);
    PersistId::new(u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("digest prefix"),
    ))
}
const fn reach_sq(range_mm: i64) -> i128 {
    (range_mm as i128) * (range_mm as i128)
}

impl Game for Regolith {
    const META: GameMeta = GameMeta {
        name: "regolith",
        summary: "planar craft and replayable three-tier rock splits",
        ruleset: REGOLITH_RULESET,
    };
    const GOLDEN_CHAINS: &'static [(&'static str, [u8; 32])] = &crate::golden::REGOLITH;
    fn honest() -> Self {
        Self::honest()
    }
    fn tampered(tamper: Tamper) -> Option<Self> {
        Some(Self::cheating(tamper))
    }
    fn spawn(&self, _entity: PersistId, slot: u64) -> RegolithState {
        let archetype = Archetype::for_slot(slot);
        let angle_urad = (slot as i64).saturating_mul(GOLDEN_ANGLE_URAD) % i64::from(TAU_URAD);
        let angle = angle_urad as f64 / 1_000_000.0;
        let pos = QPos::from_metres(
            SPAWN_RADIUS_MM * libm::cos(angle) / 1_000.0,
            0.0,
            SPAWN_RADIUS_MM * libm::sin(angle) / 1_000.0,
        );
        let yaw = i32::try_from(angle_urad).unwrap_or(0) + TAU_URAD / 4;
        RegolithState::Craft(Craft::spawned(archetype, pos, yaw))
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
            Outcome::Destroyed { .. } | Outcome::Split { .. } => None,
        }
    }
    fn trajectory(state: &RegolithState) -> (QPos, QVel) {
        match state {
            RegolithState::Craft(craft) => (craft.pos, craft.vel),
            RegolithState::Rock(rock) => (rock.pos, rock.vel),
        }
    }
}
