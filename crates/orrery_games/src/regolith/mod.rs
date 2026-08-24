//! **Regolith** — planar combat, deterministic density, death loops and scoring.
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
use order::{ChildSpec, LockBreakReason, Order, Outcome, ShotResult};
use orrery_core::{
    ComponentTypeId, CoreClass, EntityMaterialization, Invariant, OrderedInputs, QPos, QVel,
    Ruleset, StateView, StepOutput, TickRng, TICK_HZ,
};
use orrery_protocol::{PersistId, RulesetId, Tick};
use rand_core::RngCore;
use state::{
    BloomDirector, BloomMembership, Craft, Pickup, RegolithState, Rock, RockTier, PITCH_LIMIT_URAD,
    TAU_URAD,
};

const DT: f64 = 1.0 / TICK_HZ as f64;
/// Drag shared by craft rules and stage-1 acceleration bounds.
pub const DRAG_PER_SEC_PER_MILLE: i64 = 50;
const DRAG_PER_SEC: f64 = DRAG_PER_SEC_PER_MILLE as f64 / 1_000.0;
const SPAWN_RADIUS_MM: f64 = 150_000.0;
const GOLDEN_ANGLE_URAD: i64 = 2_399_963;
/// Bloom cadence: 60 seconds at 60 Hz.
pub const BLOOM_CADENCE_TICKS: u64 = 3_600;
/// Maximum bloom-site lifetime: 90 seconds at 60 Hz.
pub const BLOOM_LIFETIME_TICKS: u64 = 5_400;
/// Rocks seeded by one bloom: 2 Large, 3 Medium and 5 Small.
pub const BLOOM_ROCK_COUNT: u16 = 10;
/// Largest live descendant population reachable from one bloom batch.
pub const BLOOM_MAX_LIVE_ROCKS: u16 = 19;
/// Half-width of the square central region used for bloom site draws.
pub const BLOOM_CENTRAL_RADIUS_MM: i64 = 250_000;
/// Wreck countdown: two seconds at 60 Hz.
pub const RESPAWN_TICKS: u16 = 120;
/// Maximum craft windows in one island.
pub const ISLAND_CRAFT_BUDGET: u16 = 8;
/// Steady-state rock-window target in one island.
pub const ISLAND_ROCK_BUDGET: u16 = 24;
/// Outstanding pickup-window target in one island.
pub const ISLAND_PICKUP_BUDGET: u16 = 4;
/// BloomDirector windows in one island.
pub const ISLAND_DIRECTOR_BUDGET: u16 = 1;
/// Published total island-window budget.
pub const ISLAND_WINDOW_BUDGET: u16 =
    ISLAND_CRAFT_BUDGET + ISLAND_ROCK_BUDGET + ISLAND_PICKUP_BUDGET + ISLAND_DIRECTOR_BUDGET;
/// Score value of one delivered craft kill.
pub const KILL_SCORE_POINTS: u64 = 25;
/// Score value of one delivered pickup win.
pub const PICKUP_SCORE_POINTS: u64 = 5;
/// Rocks reflect from this square island edge with integer velocity negation.
pub const ISLAND_BOUNDARY_MM: i64 = 1_000_000;
const JITTER_MIN_URAD: u32 = 785_398;
const JITTER_MAX_URAD: u32 = 1_308_997;
/// Pickup lifetime: 30 seconds at 60 Hz.
pub const PICKUP_TTL_TICKS: u16 = 1_800;
/// Maximum eligible grab distance, in millimetres.
pub const GRAB_RADIUS_MM: i64 = 25_000;
/// Held-fire ticks required to acquire a target lock.
pub const LOCK_ACQUISITION_TICKS: u16 = 30;
const REFERENCE_SIGNATURE_RADIUS_MM: u128 = 3_000;
const CHANCE_SCALE: u128 = 1_000_000;

/// Regolith v8's rules identity: authoritative shot-resolution feedback.
pub const REGOLITH_RULESET: RulesetId = RulesetId {
    version: 8,
    digest: [0x63; 32],
};

/// Component classifications.
pub mod components {
    use orrery_core::ComponentTypeId;
    /// Verifiable Regolith state for every entity-window variant.
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
            RegolithState::Pickup(pickup) => {
                let (pickup, events) = Self::step_pickup(me, pickup, inputs);
                (RegolithState::Pickup(pickup), events)
            }
            RegolithState::BloomDirector(director) => {
                let (director, events) = Self::step_director(me, director, inputs, rng);
                (RegolithState::BloomDirector(director), events)
            }
        };
        *view.own_mut() = state;
        StepOutput { events }
    }
    fn materialize(&self, event: &Outcome, out: &mut Vec<EntityMaterialization<RegolithState>>) {
        match event {
            Outcome::Split {
                generation,
                children,
                ..
            } => {
                for child in children {
                    let mut rock = Rock::spawned(
                        child.tier,
                        generation.saturating_add(1),
                        child.pos,
                        child.vel,
                    );
                    rock.bloom = child.bloom;
                    rock.born_in_bloom = child.bloom.is_some();
                    out.push(EntityMaterialization::new(
                        child.id,
                        RegolithState::Rock(rock),
                    ));
                }
            }
            Outcome::SpawnPickup {
                id,
                pos,
                kind,
                expires_at,
            } => out.push(EntityMaterialization::new(
                *id,
                RegolithState::Pickup(Pickup::spawned(*pos, *kind, *expires_at)),
            )),
            Outcome::BloomSeeded {
                director,
                bloom_index,
                rocks,
                ..
            } => {
                for rock in rocks.iter() {
                    out.push(EntityMaterialization::new(
                        rock.id,
                        RegolithState::Rock(Rock::spawned_in_bloom(
                            rock.tier,
                            rock.pos,
                            rock.vel,
                            *director,
                            *bloom_index,
                        )),
                    ));
                }
            }
            _ => {}
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
        let mut equipped = own.weapon;
        let origin = own.pos;
        let firing_vel = own.vel;
        let (mut px, mut py, mut pz) = own.pos.to_metres();
        let (mut vx, mut vy, mut vz) = own.vel.to_metres_per_sec();
        let (mut yaw, mut pitch, mut hull, mut shield) =
            (own.yaw_urad, own.pitch_urad, own.hull, own.shield);
        let (mut shots, mut damage_dealt, mut cooldown) =
            (own.shots, own.damage_dealt, own.cooldown.saturating_sub(1));
        let (mut grabs_attempted, mut pickups_won, mut grabs_lost) =
            (own.grabs_attempted, own.pickups_won, own.grabs_lost);
        let (mut respawn_in, mut score_rock_points, mut kills) =
            (own.respawn_in, own.score_rock_points, own.kills);
        let (mut lock_target, mut lock_progress, mut locks_acquired) =
            (own.lock_target, own.lock_progress, own.locks_acquired);
        let was_alive = own.alive();
        let mut disabled = !was_alive;
        for order in inputs.iter() {
            match order {
                Order::Thrust { .. } | Order::Fire { .. } | Order::Grab { .. } if disabled => {}
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
                    match lock_target {
                        None => {
                            lock_target = Some(*target);
                            lock_progress = 1;
                        }
                        Some(current) if current == *target => {
                            if lock_progress < LOCK_ACQUISITION_TICKS {
                                lock_progress = lock_progress.saturating_add(1);
                                if lock_progress == LOCK_ACQUISITION_TICKS {
                                    locks_acquired = locks_acquired.saturating_add(1);
                                }
                            }
                        }
                        // A Fire naming a different target switches the lock,
                        // paying acquisition again from scratch: the switch is
                        // free to make but never cheaper than a fresh lock.
                        Some(_) => {
                            lock_target = Some(*target);
                            lock_progress = 1;
                        }
                    }
                    if lock_progress < LOCK_ACQUISITION_TICKS {
                        continue;
                    }
                    let weapon = equipped.weapon();
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
                            attacker_vel: firing_vel,
                            attacker_weapon: equipped,
                            flight_ticks: None,
                        });
                    }
                    shots = shots.saturating_add(1);
                    cooldown = weapon.cooldown_ticks;
                }
                Order::Damage {
                    amount,
                    from,
                    from_pos,
                    from_vel,
                    from_weapon,
                    flight_ticks,
                } => {
                    match projectile_resolution(
                        origin,
                        own.vel,
                        limits.radius_mm,
                        was_alive && !disabled,
                        *from_pos,
                        *from_vel,
                        *from_weapon,
                        *flight_ticks,
                        rng,
                    ) {
                        ProjectileResolution::InFlight(ticks) => {
                            events.push(Outcome::DamageDealt {
                                attacker: *from,
                                target: me,
                                amount: *amount,
                                attacker_pos: *from_pos,
                                attacker_vel: *from_vel,
                                attacker_weapon: *from_weapon,
                                flight_ticks: Some(ticks),
                            });
                            continue;
                        }
                        ProjectileResolution::Miss => {
                            events.push(Outcome::ShotResolved {
                                attacker: *from,
                                target: me,
                                result: ShotResult::Miss,
                            });
                            continue;
                        }
                        ProjectileResolution::Break(reason) => {
                            events.push(Outcome::LockBroken {
                                locker: *from,
                                target: me,
                                reason,
                            });
                            continue;
                        }
                        ProjectileResolution::Hit => {
                            events.push(Outcome::ShotResolved {
                                attacker: *from,
                                target: me,
                                result: ShotResult::Hit,
                            });
                        }
                    }
                    let incoming = (*amount).max(0);
                    let absorbed = incoming.min(shield.max(0));
                    shield -= absorbed;
                    let through = incoming - absorbed;
                    if through > 0 && hull > 0 {
                        hull = (hull - through).max(0);
                        if hull == 0 {
                            disabled = true;
                            respawn_in = RESPAWN_TICKS;
                            events.push(Outcome::Destroyed { by: *from });
                            events.push(Outcome::LockBroken {
                                locker: *from,
                                target: me,
                                reason: LockBreakReason::TargetDestroyed,
                            });
                        }
                    }
                }
                Order::Grab { pickup } => {
                    grabs_attempted = grabs_attempted.saturating_add(1);
                    events.push(Outcome::GrabAttempted {
                        pickup: *pickup,
                        ship: me,
                        ship_pos: origin,
                    });
                }
                Order::PickupGranted { kind } => {
                    // This write is the durable inventory trace: the pickup
                    // decided the outcome, then delivery brought it home.
                    equipped = *kind;
                    pickups_won = pickups_won.saturating_add(1);
                }
                Order::PickupDenied => {
                    grabs_lost = grabs_lost.saturating_add(1);
                }
                Order::KillCredit => kills = kills.saturating_add(1),
                Order::RockCredit { points } => {
                    score_rock_points = score_rock_points.saturating_add(u64::from(*points));
                }
                Order::LockBroken { target, reason: _ } => {
                    if lock_target == Some(*target) {
                        lock_target = None;
                        lock_progress = 0;
                    }
                }
                Order::GrabAttempt { .. }
                | Order::BloomPopulationChanged { .. }
                | Order::ShotResolved { .. } => {}
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
        if !was_alive && hull == 0 && respawn_in > 0 {
            respawn_in -= 1;
            if respawn_in == 0 {
                let (spawn_pos, spawn_yaw) = spawn_pose(me.0.saturating_sub(1));
                px = spawn_pos.x as f64 / 1_000.0;
                py = spawn_pos.y as f64 / 1_000.0;
                pz = spawn_pos.z as f64 / 1_000.0;
                vx = 0.0;
                vy = 0.0;
                vz = 0.0;
                yaw = spawn_yaw;
                pitch = 0;
                hull = limits.max_hull;
                shield = limits.max_shield;
                cooldown = 0;
                equipped = weapon::WeaponKind::Stock;
                lock_target = None;
                lock_progress = 0;
            }
        }
        let next = Craft {
            weapon: equipped,
            pos: QPos::from_metres(px, py, pz),
            vel: QVel::from_metres_per_sec(vx, vy, vz),
            yaw_urad: yaw,
            pitch_urad: pitch,
            hull,
            shield,
            cooldown,
            shots,
            damage_dealt,
            grabs_attempted,
            pickups_won,
            grabs_lost,
            respawn_in,
            score_rock_points,
            kills,
            lock_target,
            lock_progress,
            locks_acquired,
            ..own
        };
        (next, events)
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
        let mut killer = None;
        if rock.hull > 0 {
            for order in inputs.iter() {
                if let Order::Damage {
                    amount,
                    from,
                    from_pos,
                    from_vel,
                    from_weapon,
                    flight_ticks,
                } = order
                {
                    match projectile_resolution(
                        origin,
                        rock.vel,
                        rock.tier.limits().radius_mm,
                        rock.hull > 0,
                        *from_pos,
                        *from_vel,
                        *from_weapon,
                        *flight_ticks,
                        rng,
                    ) {
                        ProjectileResolution::InFlight(ticks) => {
                            events.push(Outcome::DamageDealt {
                                attacker: *from,
                                target: me,
                                amount: *amount,
                                attacker_pos: *from_pos,
                                attacker_vel: *from_vel,
                                attacker_weapon: *from_weapon,
                                flight_ticks: Some(ticks),
                            });
                        }
                        ProjectileResolution::Hit => {
                            events.push(Outcome::ShotResolved {
                                attacker: *from,
                                target: me,
                                result: ShotResult::Hit,
                            });
                            rock.hull = (rock.hull - (*amount).max(0)).max(0);
                            if rock.hull == 0 {
                                killer = Some(*from);
                            }
                        }
                        ProjectileResolution::Break(reason) => {
                            events.push(Outcome::LockBroken {
                                locker: *from,
                                target: me,
                                reason,
                            });
                        }
                        ProjectileResolution::Miss => {
                            events.push(Outcome::ShotResolved {
                                attacker: *from,
                                target: me,
                                result: ShotResult::Miss,
                            });
                        }
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
                    if let Some(bloom) = rock.bloom {
                        events.push(Outcome::BloomPopulationChanged {
                            director: bloom.director,
                            bloom_index: bloom.bloom_index,
                            delta: 1,
                        });
                    }
                } else {
                    let threshold = if rock.born_in_bloom { 50 } else { 25 };
                    if uniform_percent(rng) < threshold {
                        let kind = if rng.next_u32() & 1 == 0 {
                            weapon::WeaponKind::Volley
                        } else {
                            weapon::WeaponKind::Heavy
                        };
                        events.push(Outcome::SpawnPickup {
                            id: pickup_id(me),
                            pos: rock.pos,
                            kind,
                            expires_at: PICKUP_TTL_TICKS,
                        });
                        rock.pickups_dropped = rock.pickups_dropped.saturating_add(1);
                    }
                    if let Some(bloom) = rock.bloom {
                        events.push(Outcome::BloomPopulationChanged {
                            director: bloom.director,
                            bloom_index: bloom.bloom_index,
                            delta: -1,
                        });
                    }
                }
                if let Some(by) = killer {
                    events.push(Outcome::RockDestroyed {
                        by,
                        points: rock.tier.limits().points,
                    });
                    events.push(Outcome::LockBroken {
                        locker: by,
                        target: me,
                        reason: LockBreakReason::TargetDestroyed,
                    });
                }
            }
        } else {
            for order in inputs.iter() {
                if let Order::Damage { from, .. } = order {
                    events.push(Outcome::LockBroken {
                        locker: *from,
                        target: me,
                        reason: LockBreakReason::TargetDestroyed,
                    });
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

    fn step_pickup(
        me: PersistId,
        mut pickup: Pickup,
        inputs: &OrderedInputs<'_, Order>,
    ) -> (Pickup, Vec<Outcome>) {
        let mut events = Vec::new();
        if pickup.claimed_by.is_none() && !pickup.expired {
            pickup.ttl_remaining = pickup.ttl_remaining.saturating_sub(1);
            if pickup.ttl_remaining == 0 {
                pickup.expired = true;
                events.push(Outcome::Expired { id: me });
            }
        }
        for order in inputs.iter() {
            let Order::GrabAttempt { ship, ship_pos } = order else {
                continue;
            };
            let eligible = pickup.claimed_by.is_none()
                && !pickup.expired
                && pickup.pos.distance_squared(*ship_pos) <= reach_sq(GRAB_RADIUS_MM);
            if eligible {
                pickup.claimed_by = Some(*ship);
                pickup.claimed_at = Some(pickup.expires_at.saturating_sub(pickup.ttl_remaining));
                events.push(Outcome::Granted {
                    ship: *ship,
                    kind: pickup.kind,
                });
            } else {
                events.push(Outcome::Denied { ship: *ship });
            }
        }
        (pickup, events)
    }

    fn step_director(
        me: PersistId,
        mut director: BloomDirector,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> (BloomDirector, Vec<Outcome>) {
        for input in inputs {
            let Order::BloomPopulationChanged { bloom_index, delta } = input else {
                continue;
            };
            let current_index = director.blooms_seeded.checked_sub(1);
            if current_index != Some(*bloom_index) || director.site_pos.is_none() {
                continue;
            }
            director.site_rocks_alive = if *delta < 0 {
                director
                    .site_rocks_alive
                    .saturating_sub(delta.unsigned_abs().into())
            } else {
                director
                    .site_rocks_alive
                    .saturating_add(u16::try_from(*delta).unwrap_or(0))
                    .min(BLOOM_MAX_LIVE_ROCKS)
            };
            if director.site_rocks_alive == 0 {
                director.site_pos = None;
                director.site_active_until = None;
            }
        }

        director.clock_tick = director.clock_tick.saturating_add(1);
        if director
            .site_active_until
            .is_some_and(|until| director.clock_tick >= until)
        {
            director.site_pos = None;
            director.site_active_until = None;
            director.site_rocks_alive = 0;
        }

        let mut events = Vec::new();
        if director.clock_tick >= director.next_bloom_tick {
            let bloom_index = director.blooms_seeded;
            let site_pos = draw_bloom_site(rng);
            let active_until = director.clock_tick.saturating_add(BLOOM_LIFETIME_TICKS);
            let rocks = Box::new(core::array::from_fn(|slot| {
                bloom_spec(me, bloom_index, slot, site_pos)
            }));
            events.push(Outcome::BloomSeeded {
                director: me,
                bloom_index,
                site_pos,
                active_until,
                rocks,
            });
            director.blooms_seeded = director.blooms_seeded.saturating_add(1);
            director.next_bloom_tick = director.next_bloom_tick.saturating_add(BLOOM_CADENCE_TICKS);
            director.site_pos = Some(site_pos);
            director.site_active_until = Some(active_until);
            director.site_rocks_alive = BLOOM_ROCK_COUNT;
        }
        (director, events)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectileResolution {
    InFlight(u16),
    Hit,
    Miss,
    Break(LockBreakReason),
}

#[allow(clippy::too_many_arguments)]
fn projectile_resolution(
    target_pos: QPos,
    target_vel: QVel,
    target_radius_mm: i64,
    target_alive: bool,
    attacker_pos: QPos,
    attacker_vel: QVel,
    weapon_kind: weapon::WeaponKind,
    flight_ticks: Option<u16>,
    rng: &mut TickRng,
) -> ProjectileResolution {
    if !target_alive {
        return ProjectileResolution::Break(LockBreakReason::TargetDestroyed);
    }
    let weapon = weapon_kind.weapon();
    let range_sq = nonnegative_distance_squared(target_pos, attacker_pos);
    let reach = weapon
        .optimal_mm
        .saturating_add(weapon.falloff_mm)
        .saturating_add(target_radius_mm);
    if range_sq > square_i64(reach) {
        return ProjectileResolution::Break(LockBreakReason::RangeExceeded);
    }

    match flight_ticks {
        None => {
            let ticks = flight_ticks_for(range_sq, weapon.projectile_speed_mms);
            if ticks > 1 {
                return ProjectileResolution::InFlight(ticks - 1);
            }
        }
        Some(ticks) if ticks > 1 => return ProjectileResolution::InFlight(ticks - 1),
        Some(_) => {}
    }

    let chance = hit_chance_ppm(
        target_pos,
        target_vel,
        target_radius_mm,
        attacker_pos,
        attacker_vel,
        weapon,
    );
    if uniform_below(rng, CHANCE_SCALE as u32) < chance {
        ProjectileResolution::Hit
    } else {
        ProjectileResolution::Miss
    }
}

fn hit_chance_ppm(
    target_pos: QPos,
    target_vel: QVel,
    target_radius_mm: i64,
    attacker_pos: QPos,
    attacker_vel: QVel,
    weapon: weapon::Weapon,
) -> u32 {
    let rx = i128::from(target_pos.x).saturating_sub(i128::from(attacker_pos.x));
    let ry = i128::from(target_pos.y).saturating_sub(i128::from(attacker_pos.y));
    let rz = i128::from(target_pos.z).saturating_sub(i128::from(attacker_pos.z));
    let vx = i128::from(target_vel.x).saturating_sub(i128::from(attacker_vel.x));
    let vy = i128::from(target_vel.y).saturating_sub(i128::from(attacker_vel.y));
    let vz = i128::from(target_vel.z).saturating_sub(i128::from(attacker_vel.z));
    let range_sq = sum_squares([rx, ry, rz]);
    let range_mm = integer_sqrt(range_sq);

    let cross = [
        ry.saturating_mul(vz).saturating_sub(rz.saturating_mul(vy)),
        rz.saturating_mul(vx).saturating_sub(rx.saturating_mul(vz)),
        rx.saturating_mul(vy).saturating_sub(ry.saturating_mul(vx)),
    ];
    let cross_magnitude = integer_sqrt(sum_squares(cross));
    let angular_urad_per_sec = cross_magnitude
        .saturating_mul(1_000_000)
        .checked_div(range_sq)
        .unwrap_or(0);
    let tracking_denominator =
        u128::from(weapon.tracking_urad_per_sec).saturating_mul(target_radius_mm.max(1) as u128);
    let tracking_ratio = angular_urad_per_sec
        .saturating_mul(REFERENCE_SIGNATURE_RADIUS_MM)
        .saturating_mul(CHANCE_SCALE)
        / tracking_denominator.max(1);

    let optimal = weapon.optimal_mm.max(0) as u128;
    let range_ratio = range_mm
        .saturating_sub(optimal)
        .saturating_mul(CHANCE_SCALE)
        / (weapon.falloff_mm.max(1) as u128);
    let penalty = tracking_ratio
        .saturating_mul(tracking_ratio)
        .saturating_add(range_ratio.saturating_mul(range_ratio));
    let denominator = CHANCE_SCALE
        .saturating_mul(CHANCE_SCALE)
        .saturating_add(penalty);
    let chance = CHANCE_SCALE
        .saturating_mul(CHANCE_SCALE)
        .saturating_mul(CHANCE_SCALE)
        / denominator.max(1);
    u32::try_from(chance.min(CHANCE_SCALE)).unwrap_or(CHANCE_SCALE as u32)
}

fn flight_ticks_for(range_sq: u128, projectile_speed_mms: i64) -> u16 {
    let distance = integer_sqrt(range_sq);
    let numerator = distance.saturating_mul(u128::from(TICK_HZ));
    let speed = projectile_speed_mms.max(1) as u128;
    let ticks = numerator.saturating_add(speed - 1) / speed;
    u16::try_from(ticks.max(1)).unwrap_or(u16::MAX)
}

fn nonnegative_distance_squared(a: QPos, b: QPos) -> u128 {
    sum_squares([
        i128::from(a.x).saturating_sub(i128::from(b.x)),
        i128::from(a.y).saturating_sub(i128::from(b.y)),
        i128::from(a.z).saturating_sub(i128::from(b.z)),
    ])
}

fn square_i64(value: i64) -> u128 {
    let value = value.max(0) as u128;
    value.saturating_mul(value)
}

fn sum_squares(values: [i128; 3]) -> u128 {
    values.into_iter().fold(0, |sum, value| {
        let magnitude = value.unsigned_abs();
        sum.saturating_add(magnitude.saturating_mul(magnitude))
    })
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut estimate = 1u128 << (value.ilog2() / 2 + 1);
    loop {
        let next = (estimate + value / estimate) / 2;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
}

fn uniform_below(rng: &mut TickRng, bound: u32) -> u32 {
    let limit = u32::MAX - u32::MAX % bound;
    loop {
        let draw = rng.next_u32();
        if draw < limit {
            return draw % bound;
        }
    }
}

fn uniform_percent(rng: &mut TickRng) -> u32 {
    rng.next_u32() % 100
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
        bloom: rock.bloom,
    }
}
fn bloom_spec(director: PersistId, bloom_index: u32, slot: usize, site_pos: QPos) -> ChildSpec {
    let tier = match slot {
        0..=1 => RockTier::Large,
        2..=4 => RockTier::Medium,
        _ => RockTier::Small,
    };
    let slot = u8::try_from(slot).expect("ten bloom slots fit in u8");
    ChildSpec {
        id: bloom_rock_id(director, bloom_index, slot),
        tier,
        pos: site_pos,
        vel: QVel::default(),
        bloom: Some(BloomMembership {
            director,
            bloom_index,
        }),
    }
}
fn bloom_rock_id(director: PersistId, bloom_index: u32, slot: u8) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regolith-bloom");
    hasher.update(&director.0.to_le_bytes());
    hasher.update(&bloom_index.to_le_bytes());
    hasher.update(&[slot]);
    PersistId::new(u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("digest prefix"),
    ))
}
fn draw_bloom_site(rng: &mut TickRng) -> QPos {
    let span = u64::try_from(BLOOM_CENTRAL_RADIUS_MM.saturating_mul(2).saturating_add(1))
        .expect("positive central-region span");
    let coordinate = |draw: u64| i64::try_from(draw % span).unwrap_or(0) - BLOOM_CENTRAL_RADIUS_MM;
    QPos {
        x: coordinate(rng.next_u64()),
        y: 0,
        z: coordinate(rng.next_u64()),
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
fn pickup_id(rock: PersistId) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regolith-pickup");
    hasher.update(&rock.0.to_le_bytes());
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
        summary: "planar combat, deterministic bloom density and logged scoring",
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
        let (pos, yaw) = spawn_pose(slot);
        RegolithState::Craft(Craft::spawned(archetype, pos, yaw))
    }
    fn honest_inputs(
        &self,
        entity: PersistId,
        slot: u64,
        tick: Tick,
        _peers: &[PersistId],
        rng: &mut TickRng,
        out: &mut Vec<Order>,
    ) {
        pilot::honest_orders(entity, slot, tick, rng, out);
    }
    fn deliver(&self, event: &Outcome) -> Option<(PersistId, Order)> {
        match event {
            Outcome::DamageDealt {
                attacker,
                target,
                amount,
                attacker_pos,
                attacker_vel,
                attacker_weapon,
                flight_ticks,
            } => Some((
                *target,
                Order::Damage {
                    amount: *amount,
                    from: *attacker,
                    from_pos: *attacker_pos,
                    from_vel: *attacker_vel,
                    from_weapon: *attacker_weapon,
                    flight_ticks: *flight_ticks,
                },
            )),
            Outcome::GrabAttempted {
                pickup,
                ship,
                ship_pos,
            } => Some((
                *pickup,
                Order::GrabAttempt {
                    ship: *ship,
                    ship_pos: *ship_pos,
                },
            )),
            Outcome::Granted { ship, kind } => Some((*ship, Order::PickupGranted { kind: *kind })),
            Outcome::Denied { ship } => Some((*ship, Order::PickupDenied)),
            Outcome::Destroyed { by } => Some((*by, Order::KillCredit)),
            Outcome::RockDestroyed { by, points } => {
                Some((*by, Order::RockCredit { points: *points }))
            }
            Outcome::BloomPopulationChanged {
                director,
                bloom_index,
                delta,
            } => Some((
                *director,
                Order::BloomPopulationChanged {
                    bloom_index: *bloom_index,
                    delta: *delta,
                },
            )),
            Outcome::LockBroken {
                locker,
                target,
                reason,
            } => Some((
                *locker,
                Order::LockBroken {
                    target: *target,
                    reason: *reason,
                },
            )),
            Outcome::ShotResolved {
                attacker,
                target,
                result,
            } => Some((
                *attacker,
                Order::ShotResolved {
                    target: *target,
                    result: *result,
                },
            )),
            Outcome::Split { .. }
            | Outcome::SpawnPickup { .. }
            | Outcome::Expired { .. }
            | Outcome::BloomSeeded { .. } => None,
        }
    }
    fn trajectory(state: &RegolithState) -> (QPos, QVel) {
        match state {
            RegolithState::Craft(craft) => (craft.pos, craft.vel),
            RegolithState::Rock(rock) => (rock.pos, rock.vel),
            RegolithState::Pickup(pickup) => (pickup.pos, QVel::default()),
            RegolithState::BloomDirector(_) => (QPos::default(), QVel::default()),
        }
    }
}

fn spawn_pose(slot: u64) -> (QPos, i32) {
    let angle_urad = (slot as i64).saturating_mul(GOLDEN_ANGLE_URAD) % i64::from(TAU_URAD);
    let angle = angle_urad as f64 / 1_000_000.0;
    let pos = QPos::from_metres(
        SPAWN_RADIUS_MM * libm::cos(angle) / 1_000.0,
        0.0,
        SPAWN_RADIUS_MM * libm::sin(angle) / 1_000.0,
    );
    let yaw = i32::try_from(angle_urad).unwrap_or(0) + TAU_URAD / 4;
    (pos, yaw.rem_euclid(TAU_URAD))
}
