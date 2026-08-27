//! **Regolith** — planar combat, deterministic density, death loops and scoring.
//!
//! A split is described entirely by an ordered event. Its parent records the
//! monotone split counter in its own state, so materialization is adjudicable.

pub mod archetype;
pub mod invariants;
pub mod order;
pub mod pilot;
pub mod state;
mod visibility;
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
    BloomDirector, BloomMembership, Craft, LockClass, Pickup, RegolithState, Rock, RockTier,
    PITCH_LIMIT_URAD, TAU_URAD,
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
/// Held-lock ticks required to acquire a target lock.
pub const LOCK_ACQUISITION_TICKS: u16 = 30;
/// A held lock takes the same half-second premise to break as to acquire.
pub const LOCK_BREAK_TICKS: u16 = LOCK_ACQUISITION_TICKS;
/// Progress removed per occluded tick, derived from the acquisition and break windows.
pub const LOCK_DECAY_PER_TICK: u16 = LOCK_ACQUISITION_TICKS.div_ceil(LOCK_BREAK_TICKS);
const _: () = assert!(
    LOCK_BREAK_TICKS > 1,
    "lock breaking must span ticks or the decay acceptance test asserts nothing"
);
/// Visibility-transition claims are capped at four per second.
pub const COVER_CLAIM_INTERVAL_TICKS: u16 = (TICK_HZ / 4) as u16;
/// Visibility plus collision can read at most three distinct recorded frames.
pub const MAX_NEIGHBOR_READS: usize = 4;
/// Claims arrive at 2 Hz; one missed claim is tolerated before refusing a frame.
pub const MAX_NEIGHBOR_STALENESS_TICKS: u64 = TICK_HZ as u64;
/// Two-centimetre inward margin: twice VC-7's one-centimetre position epsilon.
pub const OCCLUSION_MARGIN_MM: i64 = 20;
const REFERENCE_SIGNATURE_RADIUS_MM: u128 = 3_000;
const CHANCE_SCALE: u128 = 1_000_000;
const CAMPAIGN_ORBIT_RADIUS_M: f64 = 2_500.0;
const CAMPAIGN_CROWD_ARC_RAD: f64 = 0.08;
const CAMPAIGN_RADIAL_SPREAD: f64 = 0.10;
/// Rocks present at campaign start: one Large, two Medium and three Small.
pub const CAMPAIGN_ROCK_COUNT: usize = 6;
const CAMPAIGN_ROCK_RADII_MM: [i64; CAMPAIGN_ROCK_COUNT] = [
    2_710_000, 2_790_000, 2_320_000, 2_840_000, 2_260_000, 2_890_000,
];
const CAMPAIGN_ROCK_TIERS: [RockTier; CAMPAIGN_ROCK_COUNT] = [
    RockTier::Large,
    RockTier::Medium,
    RockTier::Medium,
    RockTier::Small,
    RockTier::Small,
    RockTier::Small,
];
/// Campaign interest-cell edge, sized for Regolith's stock engagement reach.
///
/// The framework default is 128 m, but a 27-cell AOI at that edge can drop a
/// craft only 172 m from the observer. Every campaign craft starts with a
/// stock weapon whose 400 m reach is still live there. A 512 m edge preserves
/// the 10% commitment margin and keeps the whole stock interaction radius in
/// the coarse AOI without changing the 27-cell topology or the P1 gate's
/// framework-default exercise.
pub const CAMPAIGN_CELL_EDGE_M: f64 = 512.0;

/// Regolith v15's rules identity: collision forces compose in sealed input order.
pub const REGOLITH_RULESET: RulesetId = RulesetId {
    version: 15,
    digest: [0x66; 32],
};

/// One canonical rock installed by the campaign composition root.
///
/// `owner_slot` is content, not a transport guess: it gives every seeded
/// entity exactly one initial authority while allowing a peer to host more
/// than its player craft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignRockSeed {
    /// Stable persistent identity derived from the universe seed and slot.
    pub entity: PersistId,
    /// Headless host slot holding the rock's initial authority.
    pub owner_slot: usize,
    /// Complete canonical starting state.
    pub rock: Rock,
}

/// Build the rocks present at campaign start.
///
/// This is deliberately a direct seed, not a bloom director. The stock
/// director waits sixty seconds and draws sites inside 250 m of the origin;
/// the campaign crowd orbits at roughly 2.5 km, so that faithful bloom would
/// still be content nobody sees. Six rocks make every published tier present
/// without turning the crowd's orbit into a collision gauntlet: they sit in a
/// radial pocket just inside and outside the outer crowd, 110--340 m from its
/// player slot, but off every bot's initial flight line. A player can see,
/// lock and deliberately fly into the pocket; orbiting bots do not begin in it.
///
/// Identity, the small angular/radial variation, tier and owner are pure
/// functions of `(universe seed, campaign-rock slot, host peer count)`. No
/// clock, process RNG or other ambient input can enter replay.
#[must_use]
pub fn campaign_rock_seeds(
    seed: orrery_protocol::UniverseSeed,
    host_peer_count: usize,
) -> [CampaignRockSeed; CAMPAIGN_ROCK_COUNT] {
    core::array::from_fn(|slot| {
        let mut preimage = [0u8; 24];
        preimage[..20].copy_from_slice(b"regolith-campaign-v1");
        preimage[20..].copy_from_slice(&(slot as u32).to_le_bytes());
        let digest = blake3::keyed_hash(&seed.0, &preimage);
        let bytes = digest.as_bytes();

        // Keep the whole pocket close to the exterior slot for every seed;
        // variation makes the universe seed real content without allowing a
        // draw to move the rocks back to the empty central bloom region.
        let angular_jitter_urad =
            i64::from(u16::from_le_bytes([bytes[0], bytes[1]])) % 6_001 - 3_000;
        let radial_jitter_mm =
            (i64::from(u16::from_le_bytes([bytes[2], bytes[3]])) % 10_001) - 5_000;
        let angle = (68_000 + angular_jitter_urad) as f64 / 1_000_000.0;
        let radius_mm = CAMPAIGN_ROCK_RADII_MM[slot].saturating_add(radial_jitter_mm);
        let pos = QPos {
            x: (radius_mm as f64 * libm::cos(angle)) as i64,
            y: 0,
            z: (radius_mm as f64 * libm::sin(angle)) as i64,
        };

        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&bytes[8..16]);
        let entity = PersistId::new(
            0xC524_0000_0000_0000 | (u64::from_le_bytes(id_bytes) & 0x0000_FFFF_FFFF_FFFF),
        );
        CampaignRockSeed {
            entity,
            owner_slot: slot % host_peer_count.max(1),
            rock: Rock::spawned(CAMPAIGN_ROCK_TIERS[slot], 0, pos, QVel::default()),
        }
    })
}

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

/// Nominate the nearest contact that is worth submitting as [`Order::Collide`].
///
/// This is the deliberately untrusted broad phase shared by live input sources.
/// It reads replicated snapshots outside the canonical step and grants no state
/// change: [`visibility::verify_claims`] repeats the integer predicate against a
/// recorded neighbour frame before the rules apply either body's force.
#[must_use]
pub fn collision_candidate<'a>(
    entity: PersistId,
    own: &RegolithState,
    neighbors: impl IntoIterator<Item = (PersistId, &'a RegolithState)>,
) -> Option<PersistId> {
    visibility::broad_phase_collision_candidate(entity, own, neighbors)
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
    fn max_neighbor_reads(&self) -> usize {
        MAX_NEIGHBOR_READS
    }
    fn max_neighbor_staleness_ticks(&self) -> u64 {
        MAX_NEIGHBOR_STALENESS_TICKS
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
        let claims = visibility::verify_claims(view, inputs);
        let (mut state, mut events) = match view.own().clone() {
            RegolithState::Craft(craft) => {
                let (craft, events) = self.step_craft(me, craft, inputs, claims.collision, rng);
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
        if let (Some(Outcome::LockVisibility { occluded, .. }), RegolithState::Craft(craft)) =
            (&claims.visibility, &mut state)
        {
            craft.last_cover_occluded = *occluded;
        }
        if claims.arithmetic_overflowed {
            match &mut state {
                RegolithState::Craft(craft) => craft.arithmetic_overflowed = true,
                RegolithState::Rock(rock) => rock.arithmetic_overflowed = true,
                RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => {}
            }
        }
        *view.own_mut() = state;
        events.extend(claims.visibility);
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
        collision: Option<visibility::CollisionResolution>,
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
        let (mut lock_target, mut lock_class, mut lock_progress, mut locks_acquired) = (
            own.lock_target,
            own.lock_class,
            own.lock_progress,
            own.locks_acquired,
        );
        let mut collisions = own.collisions;
        let mut lock_reply = None;
        let mut lock_decay_progress = own.lock_decay_progress;
        let mut cover_claim_cooldown = own.cover_claim_cooldown.saturating_sub(1);
        if lock_decay_progress > 0 {
            lock_decay_progress = lock_decay_progress.saturating_add(LOCK_DECAY_PER_TICK);
            if lock_decay_progress >= LOCK_ACQUISITION_TICKS {
                lock_target = None;
                lock_class = None;
                lock_progress = 0;
                lock_decay_progress = 0;
            }
        }
        let was_alive = own.alive();
        let mut disabled = !was_alive;
        for order in inputs.iter() {
            match order {
                Order::Thrust { .. } | Order::Lock { .. } | Order::Fire | Order::Grab { .. }
                    if disabled => {}
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
                Order::Lock { target } => {
                    match lock_target {
                        None => {
                            lock_target = Some(*target);
                            lock_class = None;
                            lock_progress = 1;
                            lock_decay_progress = 0;
                            events.push(Outcome::LockRequested {
                                locker: me,
                                target: *target,
                            });
                        }
                        Some(current) if current == *target => {
                            if lock_progress < LOCK_ACQUISITION_TICKS {
                                lock_progress = lock_progress.saturating_add(1);
                                if lock_progress == LOCK_ACQUISITION_TICKS && lock_class.is_some() {
                                    locks_acquired = locks_acquired.saturating_add(1);
                                }
                            }
                        }
                        // A Lock naming a different target switches the lock,
                        // paying acquisition again from scratch: the switch is
                        // free to make but never cheaper than a fresh lock.
                        Some(_) => {
                            lock_target = Some(*target);
                            lock_class = None;
                            lock_progress = 1;
                            lock_decay_progress = 0;
                            events.push(Outcome::LockRequested {
                                locker: me,
                                target: *target,
                            });
                        }
                    }
                }
                Order::LockConfirmed { target, class } => {
                    lock_reply = Some((*target, Some(*class)));
                }
                Order::LockRefused { target } => {
                    lock_reply = Some((*target, None));
                }
                Order::Fire => {
                    // Orders are applied in their sealed order. A preceding Lock
                    // therefore switches first, while a preceding Fire consumes
                    // the lock that existed before a later switch.
                    let Some(target) = lock_target.filter(|_| {
                        lock_progress >= LOCK_ACQUISITION_TICKS && lock_class.is_some()
                    }) else {
                        events.push(Outcome::ShotRefused {
                            attacker: me,
                            result: ShotResult::NoLock,
                        });
                        continue;
                    };
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
                            target,
                            amount,
                            attacker_pos: origin,
                            attacker_vel: firing_vel,
                            attacker_yaw_urad: yaw,
                            attacker_archetype: own.archetype,
                            attacker_weapon: equipped,
                            flight_ticks: None,
                        });
                    }
                    shots = shots.saturating_add(1);
                    cooldown = weapon.cooldown_ticks;
                }
                Order::LockRequested { locker } => {
                    if hull > 0 {
                        events.push(Outcome::LockConfirmed {
                            locker: *locker,
                            target: me,
                            class: LockClass::Ship,
                        });
                    } else {
                        events.push(Outcome::LockRefused {
                            locker: *locker,
                            target: me,
                        });
                    }
                }
                Order::Damage {
                    amount,
                    from,
                    from_pos,
                    from_vel,
                    from_yaw_urad,
                    from_archetype,
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
                        *from_yaw_urad,
                        *from_archetype,
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
                                attacker_yaw_urad: *from_yaw_urad,
                                attacker_archetype: *from_archetype,
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
                        ProjectileResolution::OutOfArc => {
                            events.push(Outcome::ShotResolved {
                                attacker: *from,
                                target: me,
                                result: ShotResult::OutOfArc,
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
                        lock_class = None;
                        lock_progress = 0;
                        lock_decay_progress = 0;
                    }
                }
                Order::LockVisibility { target, occluded } => {
                    if lock_target == Some(*target) && lock_progress == LOCK_ACQUISITION_TICKS {
                        if *occluded {
                            if lock_decay_progress == 0 {
                                lock_decay_progress = LOCK_DECAY_PER_TICK;
                            }
                        } else {
                            lock_decay_progress = 0;
                            lock_progress = LOCK_ACQUISITION_TICKS;
                        }
                    }
                }
                Order::CollisionResolved { from: _, velocity } => {
                    if velocity_within_limit(*velocity, limits.max_speed_mms) {
                        vx = velocity.x as f64 / 1_000.0;
                        vy = velocity.y as f64 / 1_000.0;
                        vz = velocity.z as f64 / 1_000.0;
                        collisions = collisions.saturating_add(1);
                    }
                }
                Order::Collide { other }
                    if collision.is_some_and(|resolution| resolution.other == *other) =>
                {
                    let resolution = collision.expect("guarded by the matching resolution");
                    // One exchange is adjudicated twice, but its force is computed
                    // once: this step applies the resolver's own velocity and the
                    // event carries its counterparty's. Keeping this arm in the
                    // sealed-order loop is the physical meaning of D46(d). A
                    // prior-tick CollisionResolved is delivered first and applies
                    // before this authored contact; reversing host composition
                    // reverses which mutually applied force is observed last.
                    vx = resolution.own_velocity.x as f64 / 1_000.0;
                    vy = resolution.own_velocity.y as f64 / 1_000.0;
                    vz = resolution.own_velocity.z as f64 / 1_000.0;
                    collisions = collisions.saturating_add(1);
                    events.push(Outcome::Collision {
                        collider: me,
                        target: resolution.other,
                        target_velocity: resolution.target_velocity,
                    });
                }
                Order::ClaimCover { .. } => {
                    if cover_claim_cooldown == 0 {
                        cover_claim_cooldown = COVER_CLAIM_INTERVAL_TICKS;
                    }
                }
                Order::GrabAttempt { .. }
                | Order::BloomPopulationChanged { .. }
                | Order::ShotResolved { .. }
                | Order::Collide { .. } => {}
            }
        }
        if let Some((target, class)) = lock_reply {
            if lock_target == Some(target) {
                match class {
                    Some(class) if lock_class.is_none() => {
                        lock_class = Some(class);
                        if lock_progress == LOCK_ACQUISITION_TICKS {
                            locks_acquired = locks_acquired.saturating_add(1);
                        }
                    }
                    Some(_) => {}
                    None => {
                        lock_target = None;
                        lock_class = None;
                        lock_progress = 0;
                        lock_decay_progress = 0;
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
                lock_class = None;
                lock_progress = 0;
                lock_decay_progress = 0;
                cover_claim_cooldown = 0;
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
            lock_class,
            lock_progress,
            locks_acquired,
            lock_decay_progress,
            cover_claim_cooldown,
            collisions,
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
                match order {
                    Order::LockRequested { locker } => {
                        events.push(Outcome::LockConfirmed {
                            locker: *locker,
                            target: me,
                            class: LockClass::Rock,
                        });
                    }
                    Order::Damage {
                        amount,
                        from,
                        from_pos,
                        from_vel,
                        from_yaw_urad,
                        from_archetype,
                        from_weapon,
                        flight_ticks,
                    } => match projectile_resolution(
                        origin,
                        rock.vel,
                        rock.tier.limits().radius_mm,
                        rock.hull > 0,
                        *from_pos,
                        *from_vel,
                        *from_yaw_urad,
                        *from_archetype,
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
                                attacker_yaw_urad: *from_yaw_urad,
                                attacker_archetype: *from_archetype,
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
                        ProjectileResolution::OutOfArc => {
                            events.push(Outcome::ShotResolved {
                                attacker: *from,
                                target: me,
                                result: ShotResult::OutOfArc,
                            });
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
                    },
                    Order::CollisionResolved { from: _, velocity }
                        if velocity_within_limit(*velocity, rock.tier.limits().max_speed_mms) =>
                    {
                        rock.vel = *velocity;
                        rock.collisions = rock.collisions.saturating_add(1);
                    }
                    _ => {}
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
                match order {
                    Order::Damage { from, .. } => events.push(Outcome::LockBroken {
                        locker: *from,
                        target: me,
                        reason: LockBreakReason::TargetDestroyed,
                    }),
                    Order::LockRequested { locker } => events.push(Outcome::LockRefused {
                        locker: *locker,
                        target: me,
                    }),
                    _ => {}
                }
            }
        }
        if rock.hull > 0 {
            rock.pos.x = flagged_add(
                rock.pos.x,
                rock.vel.x / i64::from(TICK_HZ),
                &mut rock.arithmetic_overflowed,
            );
            rock.pos.y = flagged_add(
                rock.pos.y,
                rock.vel.y / i64::from(TICK_HZ),
                &mut rock.arithmetic_overflowed,
            );
            rock.pos.z = flagged_add(
                rock.pos.z,
                rock.vel.z / i64::from(TICK_HZ),
                &mut rock.arithmetic_overflowed,
            );
            if rock.pos.x.unsigned_abs() > ISLAND_BOUNDARY_MM as u64 {
                rock.vel.x = flagged_neg(rock.vel.x, &mut rock.arithmetic_overflowed);
            }
            if rock.pos.y.unsigned_abs() > ISLAND_BOUNDARY_MM as u64 {
                rock.vel.y = flagged_neg(rock.vel.y, &mut rock.arithmetic_overflowed);
            }
            if rock.pos.z.unsigned_abs() > ISLAND_BOUNDARY_MM as u64 {
                rock.vel.z = flagged_neg(rock.vel.z, &mut rock.arithmetic_overflowed);
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
            match order {
                Order::GrabAttempt { ship, ship_pos } => {
                    let eligible = pickup.claimed_by.is_none()
                        && !pickup.expired
                        && pickup.pos.distance_squared(*ship_pos) <= reach_sq(GRAB_RADIUS_MM);
                    if eligible {
                        pickup.claimed_by = Some(*ship);
                        pickup.claimed_at =
                            Some(pickup.expires_at.saturating_sub(pickup.ttl_remaining));
                        events.push(Outcome::Granted {
                            ship: *ship,
                            kind: pickup.kind,
                        });
                    } else {
                        events.push(Outcome::Denied { ship: *ship });
                    }
                }
                Order::LockRequested { locker } => events.push(Outcome::LockRefused {
                    locker: *locker,
                    target: me,
                }),
                _ => {}
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
        let mut events = Vec::new();
        for input in inputs {
            if let Order::LockRequested { locker } = input {
                events.push(Outcome::LockRefused {
                    locker: *locker,
                    target: me,
                });
                continue;
            }
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

        if director.clock_tick >= director.next_bloom_tick {
            let bloom_index = director.blooms_seeded;
            let site_pos = draw_bloom_site(rng);
            let active_until = director.clock_tick.saturating_add(BLOOM_LIFETIME_TICKS);
            let rocks = Box::new(core::array::from_fn(|slot| {
                bloom_spec(me, bloom_index, slot, site_pos, rng)
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
    OutOfArc,
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
    attacker_yaw_urad: i32,
    attacker_archetype: Archetype,
    weapon_kind: weapon::WeaponKind,
    flight_ticks: Option<u16>,
    rng: &mut TickRng,
) -> ProjectileResolution {
    if !target_alive {
        return ProjectileResolution::Break(LockBreakReason::TargetDestroyed);
    }
    // The initial delivery decides the firing-time fact. `Some` is a
    // target-authored continuation of that accepted projectile; rechecking
    // against the target's later position turns movement during flight into
    // a retroactive OutOfArc refusal before the hit roll.
    if flight_ticks.is_none()
        && !in_firing_arc(
            attacker_archetype,
            attacker_yaw_urad,
            attacker_pos,
            target_pos,
        )
    {
        return ProjectileResolution::OutOfArc;
    }
    let weapon = weapon_kind.weapon();
    // Range deliberately stays live for target-authored continuations. The
    // target's current position is compared with the attacker's firing-time
    // origin so that escaping beyond weapon reach breaks the lock before the
    // projectile resolves; that mixed-time frame is the outrunning mechanic.
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
            let ticks = projectile_flight_ticks(range_sq, weapon.projectile_speed_mms);
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

/// Whether `target_pos` lies in one of the shooter's chassis firing arcs.
///
/// The relative bearing is obtained with integer CORDIC vectoring. That keeps
/// this persistent-value decision bit-exact without a platform float result.
#[must_use]
pub fn in_firing_arc(
    attacker_archetype: Archetype,
    attacker_yaw_urad: i32,
    attacker_pos: QPos,
    target_pos: QPos,
) -> bool {
    firing_arc_measurement(
        attacker_archetype,
        attacker_yaw_urad,
        attacker_pos,
        target_pos,
    )
    .inside
}

/// The exact integer geometry used by firing-arc adjudication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiringArcMeasurement {
    /// Target bearing in world space, or `None` for coincident positions.
    pub world_bearing_urad: Option<i32>,
    /// Target bearing relative to the attacker, or `None` when coincident.
    pub relative_urad: Option<i32>,
    /// Whether at least one chassis arc accepts the relative bearing.
    pub inside: bool,
}

/// Measures the exact geometry used by [`in_firing_arc`].
#[must_use]
pub fn firing_arc_measurement(
    attacker_archetype: Archetype,
    attacker_yaw_urad: i32,
    attacker_pos: QPos,
    target_pos: QPos,
) -> FiringArcMeasurement {
    let dx = i128::from(target_pos.x) - i128::from(attacker_pos.x);
    let dz = i128::from(target_pos.z) - i128::from(attacker_pos.z);
    let Some(world_bearing) = integer_bearing_urad(dx, dz) else {
        return FiringArcMeasurement {
            world_bearing_urad: None,
            relative_urad: None,
            inside: true,
        };
    };
    let relative = world_bearing
        .saturating_sub(attacker_yaw_urad)
        .rem_euclid(TAU_URAD);
    let inside = attacker_archetype
        .firing_arcs()
        .iter()
        .any(|arc| arc.contains(relative));
    FiringArcMeasurement {
        world_bearing_urad: Some(world_bearing),
        relative_urad: Some(relative),
        inside,
    }
}

fn integer_bearing_urad(mut x: i128, mut y: i128) -> Option<i32> {
    const ATAN_URAD: [i32; 21] = [
        785_398, 463_648, 244_979, 124_355, 62_419, 31_240, 15_624, 7_812, 3_906, 1_953, 977, 488,
        244, 122, 61, 31, 15, 8, 4, 2, 1,
    ];
    if x == 0 && y == 0 {
        return None;
    }
    let mut angle = 0_i32;
    if x < 0 {
        x = -x;
        y = -y;
        angle = if y <= 0 {
            TAU_URAD / 2
        } else {
            -(TAU_URAD / 2)
        };
    }
    x <<= 32;
    y <<= 32;
    for (shift, turn) in ATAN_URAD.into_iter().enumerate() {
        if y == 0 {
            break;
        }
        let (old_x, old_y) = (x, y);
        if old_y > 0 {
            x = old_x.saturating_add(old_y >> shift);
            y = old_y.saturating_sub(old_x >> shift);
            angle = angle.saturating_add(turn);
        } else {
            x = old_x.saturating_sub(old_y >> shift);
            y = old_y.saturating_add(old_x >> shift);
            angle = angle.saturating_sub(turn);
        }
    }
    Some(angle)
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

/// Return the ruleset's flight duration for a squared range and projectile speed.
///
/// Presentation may use this to show the timing the ruleset will apply without
/// predicting the eventual shot result.
#[must_use]
pub fn projectile_flight_ticks(range_sq: u128, projectile_speed_mms: i64) -> u16 {
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

/// Straight-line separation in millimetres, rounded down exactly as projectile flight time is.
#[must_use]
pub fn distance_mm(a: QPos, b: QPos) -> u128 {
    integer_sqrt(nonnegative_distance_squared(a, b))
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

fn velocity_within_limit(velocity: QVel, max_speed_mms: i64) -> bool {
    let speed_sq = [velocity.x, velocity.y, velocity.z]
        .into_iter()
        .map(|value| i128::from(value).unsigned_abs().pow(2))
        .sum::<u128>();
    speed_sq <= square_i64(max_speed_mms)
}

pub(crate) fn integer_sqrt(value: u128) -> u128 {
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
fn bloom_spec(
    director: PersistId,
    bloom_index: u32,
    slot: usize,
    site_pos: QPos,
    rng: &mut TickRng,
) -> ChildSpec {
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
        vel: bloom_velocity(tier, rng),
        bloom: Some(BloomMembership {
            director,
            bloom_index,
        }),
    }
}
fn bloom_velocity(tier: RockTier, rng: &mut TickRng) -> QVel {
    // Eight planar directions in fixed-point /1024. The diagonal coefficient
    // is round(1024 / sqrt(2)); no float enters the rules predicate or state.
    const DIRECTIONS: [(i64, i64); 8] = [
        (1_024, 0),
        (724, 724),
        (0, 1_024),
        (-724, 724),
        (-1_024, 0),
        (-724, -724),
        (0, -1_024),
        (724, -724),
    ];
    let limits = tier.limits();
    let floor = limits.max_speed_mms / 4;
    let width = u32::try_from(limits.max_speed_mms / 4).unwrap_or(1).max(1);
    let speed = floor.saturating_add(i64::from(uniform_below(rng, width)));
    let direction = DIRECTIONS[uniform_below(rng, DIRECTIONS.len() as u32) as usize];
    QVel {
        x: speed.saturating_mul(direction.0) / 1_024,
        y: 0,
        z: speed.saturating_mul(direction.1) / 1_024,
    }
}

fn flagged_add(left: i64, right: i64, overflowed: &mut bool) -> i64 {
    left.checked_add(right).unwrap_or_else(|| {
        *overflowed = true;
        left.saturating_add(right)
    })
}

fn flagged_neg(value: i64, overflowed: &mut bool) -> i64 {
    value.checked_neg().unwrap_or_else(|| {
        *overflowed = true;
        value.saturating_neg()
    })
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
                attacker_yaw_urad,
                attacker_archetype,
                attacker_weapon,
                flight_ticks,
            } => Some((
                *target,
                Order::Damage {
                    amount: *amount,
                    from: *attacker,
                    from_pos: *attacker_pos,
                    from_vel: *attacker_vel,
                    from_yaw_urad: *attacker_yaw_urad,
                    from_archetype: *attacker_archetype,
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
            Outcome::LockRequested { locker, target } => {
                Some((*target, Order::LockRequested { locker: *locker }))
            }
            Outcome::LockConfirmed {
                locker,
                target,
                class,
            } => Some((
                *locker,
                Order::LockConfirmed {
                    target: *target,
                    class: *class,
                },
            )),
            Outcome::LockRefused { locker, target } => {
                Some((*locker, Order::LockRefused { target: *target }))
            }
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
            Outcome::ShotRefused { .. } => None,
            Outcome::LockVisibility {
                locker,
                target,
                occluded,
            } => Some((
                *locker,
                Order::LockVisibility {
                    target: *target,
                    occluded: *occluded,
                },
            )),
            Outcome::Collision {
                collider,
                target,
                target_velocity,
            } => Some((
                *target,
                Order::CollisionResolved {
                    from: *collider,
                    velocity: *target_velocity,
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

/// The campaign swarm's shared spawn pose for one slot.
///
/// Campaign participants must derive their initial canonical position from
/// the same function as the host's headless peers. A client using the compact
/// scenario ring instead starts kilometres outside the host crowd and cannot
/// put any target inside weapon range, even when its firing bearing is valid.
#[must_use]
pub fn campaign_spawn_pose(slot: usize, count: usize) -> (QPos, i32) {
    let share = slot as f64 / count.max(1) as f64;
    let radius_m = campaign_orbit_radius_m(slot, count);
    let arc = CAMPAIGN_CROWD_ARC_RAD * share;
    let pos = QPos::from_metres(libm::cos(arc) * radius_m, 0.0, libm::sin(arc) * radius_m);
    let yaw_urad = ((arc + core::f64::consts::FRAC_PI_2) * 1_000_000.0) as i32;
    (pos, yaw_urad)
}

/// The campaign orbit radius for one slot, in metres.
#[must_use]
pub fn campaign_orbit_radius_m(slot: usize, count: usize) -> f64 {
    let share = slot as f64 / count.max(1) as f64;
    CAMPAIGN_ORBIT_RADIUS_M * (1.0 + CAMPAIGN_RADIAL_SPREAD * (share - 0.5))
}
