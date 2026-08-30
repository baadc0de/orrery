//! Hashed, own-state traces for Regolith entities.

use orrery_core::{CodecError, CoreCodec, QPos, QVel, Quantized};

use super::{archetype::Archetype, weapon::WeaponKind};

/// Target class confirmed by the target's own rules step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockClass {
    /// A live craft.
    Ship,
    /// A live rock in the mining economy.
    Rock,
}

impl LockClass {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Ship => 1,
            Self::Rock => 2,
        }
    }

    pub(crate) const fn from_tag(tag: u8) -> Result<Option<Self>, CodecError> {
        match tag {
            0 => Ok(None),
            1 => Ok(Some(Self::Ship)),
            2 => Ok(Some(Self::Rock)),
            _ => Err(CodecError("regolith: unknown lock class")),
        }
    }
}

/// Full turn in micro-radians.
pub const TAU_URAD: i32 = 6_283_185;
/// Elevation limit, micro-radians: a quarter turn either side of level.
///
/// Symmetric by construction, and π/2 rather than any smaller angle because
/// ±π/2 *is* the whole elevation range — straight up to straight down. A
/// larger bound would not buy a new direction, it would alias: pitch beyond a
/// quarter turn re-enters the same range with the yaw flipped by π, so the
/// same heading would have two encodings and the canonical state would stop
/// being canonical.
///
/// The value is π/2 truncated to the micro-radian lattice, 1.5707960 rad
/// against a true 1.5707963. That truncation is load-bearing rather than
/// sloppy: it leaves `cos(PITCH_LIMIT_URAD / 1e6) ≈ 3.3e-7`, strictly
/// positive, so the horizontal thrust factor never collapses to exactly zero
/// and yaw stays observable even at full elevation.
///
/// Shared with Skirmish's limit of the same name and value; both games feed
/// one client control mapping, and a player who pitches to the stop in one
/// must not find a different stop in the other.
pub const PITCH_LIMIT_URAD: i32 = 1_570_796;

/// Maximum number of sampled positions retained behind a craft.
pub const TRAIL_CAPACITY: usize = 4;
/// One trail point every twelve simulation ticks (5 Hz at the fixed 60 Hz tick).
pub const TRAIL_SAMPLE_TICKS: u8 = 12;
/// Trail-position lattice, in millimetres per encoded metre.
pub const TRAIL_QUANTUM_MM: i64 = 1_000;
/// Maximum canonical bytes added to a craft by its full trail.
pub const TRAIL_MAX_ENCODED_BYTES: usize = 1 + TRAIL_CAPACITY * 3 * core::mem::size_of::<i16>();

/// One whole-metre, grid-relative trail position.
///
/// Regolith's furthest campaign content is below 3 km and its reflecting
/// island edge is 1 km, so signed 16-bit metres leave more than a tenfold
/// range margin while halving each coordinate compared with an `i32` point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrailPoint {
    /// Whole metres along x.
    pub x_m: i16,
    /// Whole metres along y.
    pub y_m: i16,
    /// Whole metres along z.
    pub z_m: i16,
}

impl TrailPoint {
    fn from_pos(pos: QPos, overflowed: &mut bool) -> Option<Self> {
        Some(Self {
            x_m: quantize_trail_axis(pos.x, overflowed)?,
            y_m: quantize_trail_axis(pos.y, overflowed)?,
            z_m: quantize_trail_axis(pos.z, overflowed)?,
        })
    }
}

/// A bounded, oldest-first trail with overwrite-oldest ring semantics.
///
/// The backing array never grows. Once full, each 5 Hz sample removes the
/// oldest point and appends the newest. The sample phase is part of canonical
/// state so replay takes samples on exactly the same ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trail {
    points: [TrailPoint; TRAIL_CAPACITY],
    len: u8,
    sample_phase: u8,
}

impl Default for Trail {
    fn default() -> Self {
        Self {
            points: [TrailPoint::default(); TRAIL_CAPACITY],
            len: 0,
            sample_phase: 0,
        }
    }
}

impl Trail {
    /// Number of retained points.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether no point has been sampled yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Retained points from oldest to newest.
    pub fn points(&self) -> impl ExactSizeIterator<Item = TrailPoint> + '_ {
        self.points[..self.len()].iter().copied()
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn advance(&mut self, pos: QPos, overflowed: &mut bool) {
        let Some(next_phase) = self.sample_phase.checked_add(1) else {
            *overflowed = true;
            self.sample_phase = 0;
            return;
        };
        if next_phase < TRAIL_SAMPLE_TICKS {
            self.sample_phase = next_phase;
            return;
        }
        self.sample_phase = 0;
        let Some(point) = TrailPoint::from_pos(pos, overflowed) else {
            return;
        };
        if self.len() < TRAIL_CAPACITY {
            self.points[self.len()] = point;
            self.len += 1;
        } else {
            self.points.rotate_left(1);
            self.points[TRAIL_CAPACITY - 1] = point;
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        // Four points need three length bits; a phase in 0..12 needs four.
        out.push((self.sample_phase << 3) | self.len);
        for point in self.points() {
            out.extend_from_slice(&point.x_m.to_le_bytes());
            out.extend_from_slice(&point.y_m.to_le_bytes());
            out.extend_from_slice(&point.z_m.to_le_bytes());
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let Some(meta) = bytes.first().copied() else {
            return Err(CodecError("regolith trail: missing metadata"));
        };
        let len = meta & 0b111;
        let sample_phase = meta >> 3;
        if usize::from(len) > TRAIL_CAPACITY || sample_phase >= TRAIL_SAMPLE_TICKS {
            return Err(CodecError("regolith trail: invalid length or sample phase"));
        }
        let expected = 1 + usize::from(len) * 3 * core::mem::size_of::<i16>();
        if bytes.len() != expected {
            return Err(CodecError("regolith trail: wrong length"));
        }
        let mut trail = Self {
            len,
            sample_phase,
            ..Self::default()
        };
        for (index, chunk) in bytes[1..].chunks_exact(6).enumerate() {
            trail.points[index] = TrailPoint {
                x_m: i16::from_le_bytes([chunk[0], chunk[1]]),
                y_m: i16::from_le_bytes([chunk[2], chunk[3]]),
                z_m: i16::from_le_bytes([chunk[4], chunk[5]]),
            };
        }
        Ok(trail)
    }
}

fn quantize_trail_axis(mm: i64, overflowed: &mut bool) -> Option<i16> {
    let whole = mm / TRAIL_QUANTUM_MM;
    let remainder = mm % TRAIL_QUANTUM_MM;
    let adjustment = i64::from(remainder >= TRAIL_QUANTUM_MM / 2)
        - i64::from(remainder <= -(TRAIL_QUANTUM_MM / 2));
    let Some(rounded) = whole.checked_add(adjustment) else {
        *overflowed = true;
        return None;
    };
    i16::try_from(rounded).ok().or_else(|| {
        *overflowed = true;
        None
    })
}

/// A craft's verifiable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Craft {
    /// Immutable chassis.
    pub archetype: Archetype,
    /// Equipped weapon; this self-grants the weapon kind carried by a shot.
    pub weapon: WeaponKind,
    /// Lattice position.
    pub pos: QPos,
    /// Lattice velocity.
    pub vel: QVel,
    /// Bounded, quantized positions sampled behind this craft.
    pub trail: Trail,
    /// Heading.
    pub yaw_urad: i32,
    /// Elevation, micro-radians, clamped to ±[`PITCH_LIMIT_URAD`].
    pub pitch_urad: i32,
    /// Hull, floored at zero.
    pub hull: i32,
    /// Shield, absorbed first.
    pub shield: i32,
    /// Ticks until the weapon may fire.
    pub cooldown: u16,
    /// Triggers fired, ever.
    pub shots: u32,
    /// Damage rolled, ever, not landed. This monotone own-state trace makes
    /// `DamageInflation` adjudicable at the attacker.
    pub damage_dealt: u64,
    /// Pickup grabs emitted, ever. The attempt is knowable from this craft's
    /// own input even though the pickup decides its outcome.
    pub grabs_attempted: u32,
    /// Pickup grants consumed, ever. Monotone and state-hashed.
    pub pickups_won: u32,
    /// Pickup denials consumed, ever. Monotone and state-hashed.
    pub grabs_lost: u32,
    /// Ticks remaining before a wreck respawns. Zero while alive.
    pub respawn_in: u16,
    /// Rock points delivered to this craft, ever. Monotone and state-hashed.
    pub score_rock_points: u64,
    /// Craft kill credits delivered to this craft, ever. Monotone and state-hashed.
    pub kills: u32,
    /// Target being acquired or held; only own inputs and logged breaks change it.
    pub lock_target: Option<orrery_protocol::PersistId>,
    /// Target-owned class confirmation for the current acquisition.
    pub lock_class: Option<LockClass>,
    /// Monotone acquisition progress while `lock_target` remains unchanged.
    pub lock_progress: u16,
    /// Locks acquired, ever. Monotone and state-hashed.
    pub locks_acquired: u32,
    /// Accumulated occluded progress; zero means the held lock is not decaying.
    pub lock_decay_progress: u16,
    /// Ticks until another two-neighbour visibility claim may be checked.
    pub cover_claim_cooldown: u16,
    /// Result of the last verified cover claim, retained in hashed own state.
    pub last_cover_occluded: bool,
    /// Verified collisions applied to this craft, ever.
    pub collisions: u32,
    /// Canonical arithmetic overflow has occurred in this entity's rules.
    pub arithmetic_overflowed: bool,
}

/// A rock's published tier. Its limits are derived from this hashed value,
/// never from a damage event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RockTier {
    /// The first, 40 m tier.
    Large,
    /// The second, 20 m tier.
    Medium,
    /// The terminal, 8 m tier.
    Small,
}

/// Immutable limits for one [`RockTier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RockLimits {
    /// Resolver-owned radius, in millimetres.
    pub radius_mm: i64,
    /// Hull at creation and its ceiling.
    pub max_hull: i32,
    /// Score value routed to a killer through logged delivery.
    pub points: u8,
    /// Velocity ceiling, in millimetres per second.
    pub max_speed_mms: i64,
    /// Relative collision mass. Only ratios between bodies are meaningful.
    pub mass_units: i64,
}

impl RockTier {
    /// Every tier, largest first.
    ///
    /// The campaign's AOI sizing iterates this to find the widest body a
    /// weapon can be locked onto, so a new tier cannot go unaccounted.
    pub const ALL: [Self; 3] = [Self::Large, Self::Medium, Self::Small];

    /// Published limits for this tier.
    #[must_use]
    pub const fn limits(self) -> RockLimits {
        match self {
            Self::Large => RockLimits {
                radius_mm: 40_000,
                max_hull: 40,
                points: 4,
                max_speed_mms: 40_000,
                mass_units: 16,
            },
            Self::Medium => RockLimits {
                radius_mm: 20_000,
                max_hull: 15,
                points: 2,
                max_speed_mms: 56_000,
                mass_units: 8,
            },
            Self::Small => RockLimits {
                radius_mm: 8_000,
                max_hull: 5,
                points: 1,
                max_speed_mms: 78_400,
                mass_units: 2,
            },
        }
    }

    /// The tier produced by a split, if this tier splits.
    #[must_use]
    pub const fn child(self) -> Option<Self> {
        match self {
            Self::Large => Some(Self::Medium),
            Self::Medium => Some(Self::Small),
            Self::Small => None,
        }
    }

    /// Canonical wire tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Large => 0,
            Self::Medium => 1,
            Self::Small => 2,
        }
    }
    /// Decode a canonical wire tag.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown tier.
    pub const fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0 => Ok(Self::Large),
            1 => Ok(Self::Medium),
            2 => Ok(Self::Small),
            _ => Err(CodecError("regolith: unknown rock tier")),
        }
    }
}

/// A drifting rock. `splits_done` is the monotone, own-state record of a
/// materialized split: it is knowable from this rock's damage input and RNG
/// alone, without observing either child or any other entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rock {
    /// The resolver-owned limits source.
    pub tier: RockTier,
    /// Split depth used in derived child identifiers.
    pub generation: u32,
    /// Lattice position.
    pub pos: QPos,
    /// Constant lattice velocity.
    pub vel: QVel,
    /// Hull, floored at zero.
    pub hull: i32,
    /// Parent splits emitted, ever. Monotone and state-hashed.
    pub splits_done: u32,
    /// Whether this rock was seeded by a bloom. Hashed here for the later
    /// director rule; pickup drops use it as their own-state probability bit.
    pub born_in_bloom: bool,
    /// Pickup materializations emitted, ever. Monotone and state-hashed.
    pub pickups_dropped: u32,
    /// Bloom lineage, propagated through splits so site liveness is log-routed.
    pub bloom: Option<BloomMembership>,
    /// Verified collisions applied to this rock, ever.
    pub collisions: u32,
    /// Canonical arithmetic overflow has occurred in this entity's rules.
    pub arithmetic_overflowed: bool,
}

/// The director and bloom generation that own a seeded rock lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BloomMembership {
    /// Director receiving population-change events.
    pub director: orrery_protocol::PersistId,
    /// Director-local bloom index.
    pub bloom_index: u32,
}

/// A materialized weapon pickup with its own adjudicable window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pickup {
    /// Lattice position used to resolve grabs.
    pub pos: QPos,
    /// Weapon granted to the first eligible craft.
    pub kind: WeaponKind,
    /// Lifetime boundary, in ticks after materialization.
    pub expires_at: u16,
    /// Ticks before expiry. Its countdown makes expiry state-hash visible.
    pub ttl_remaining: u16,
    /// First eligible claimant, if any.
    pub claimed_by: Option<orrery_protocol::PersistId>,
    /// Pickup-local age at the claim, if any.
    pub claimed_at: Option<u16>,
    /// Whether the TTL elapsed before a claim.
    pub expired: bool,
}

/// One island's deterministic bloom schedule and in-band site announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomDirector {
    /// Island-local core tick advanced by this entity's own step.
    pub clock_tick: u64,
    /// Island-local tick at which the next bloom is seeded.
    pub next_bloom_tick: u64,
    /// Bloom batches emitted, ever. This is the monotone own-state seed trace.
    pub blooms_seeded: u32,
    /// Latest active site position, replicated in-band.
    pub site_pos: Option<QPos>,
    /// Absolute expiry tick for the latest active site.
    pub site_active_until: Option<u64>,
    /// Live rock lineages in the latest site, including split descendants.
    pub site_rocks_alive: u16,
}

/// The complete core-state sum: every Regolith window shares one ruleset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegolithState {
    /// A player craft.
    Craft(Craft),
    /// An autonomous rock.
    Rock(Rock),
    /// A contested weapon pickup.
    Pickup(Pickup),
    /// One island's bloom scheduler and replicated site announcement.
    BloomDirector(BloomDirector),
}

const CRAFT_BASE_ENCODED_LEN: usize = 132;

impl Quantized for Craft {
    fn quantize(&mut self) {
        let (x, y, z) = self.pos.to_metres();
        self.pos = QPos::from_metres(x, y, z);
        let (x, y, z) = self.vel.to_metres_per_sec();
        self.vel = QVel::from_metres_per_sec(x, y, z);
    }
}

impl Quantized for Rock {
    fn quantize(&mut self) {
        let (x, y, z) = self.pos.to_metres();
        self.pos = QPos::from_metres(x, y, z);
        let (x, y, z) = self.vel.to_metres_per_sec();
        self.vel = QVel::from_metres_per_sec(x, y, z);
    }
}

impl Quantized for Pickup {
    fn quantize(&mut self) {
        let (x, y, z) = self.pos.to_metres();
        self.pos = QPos::from_metres(x, y, z);
    }
}

impl Quantized for BloomDirector {
    fn quantize(&mut self) {
        if let Some(pos) = self.site_pos {
            let (x, y, z) = pos.to_metres();
            self.site_pos = Some(QPos::from_metres(x, y, z));
        }
    }
}

impl Quantized for RegolithState {
    fn quantize(&mut self) {
        match self {
            Self::Craft(craft) => craft.quantize(),
            Self::Rock(rock) => rock.quantize(),
            Self::Pickup(pickup) => pickup.quantize(),
            Self::BloomDirector(director) => director.quantize(),
        }
    }
}

impl CoreCodec for Craft {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[self.archetype.tag(), self.weapon.tag()]);
        for value in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.yaw_urad.to_le_bytes());
        out.extend_from_slice(&self.pitch_urad.to_le_bytes());
        out.extend_from_slice(&self.hull.to_le_bytes());
        out.extend_from_slice(&self.shield.to_le_bytes());
        out.extend_from_slice(&self.cooldown.to_le_bytes());
        out.extend_from_slice(&self.shots.to_le_bytes());
        out.extend_from_slice(&self.damage_dealt.to_le_bytes());
        out.extend_from_slice(&self.grabs_attempted.to_le_bytes());
        out.extend_from_slice(&self.pickups_won.to_le_bytes());
        out.extend_from_slice(&self.grabs_lost.to_le_bytes());
        out.extend_from_slice(&self.respawn_in.to_le_bytes());
        out.extend_from_slice(&self.score_rock_points.to_le_bytes());
        out.extend_from_slice(&self.kills.to_le_bytes());
        match self.lock_target {
            Some(target) => {
                out.push(1);
                out.extend_from_slice(&target.0.to_le_bytes());
            }
            None => out.extend_from_slice(&[0; 9]),
        }
        out.push(self.lock_class.map_or(0, LockClass::tag));
        out.extend_from_slice(&self.lock_progress.to_le_bytes());
        out.extend_from_slice(&self.locks_acquired.to_le_bytes());
        out.extend_from_slice(&self.lock_decay_progress.to_le_bytes());
        out.extend_from_slice(&self.cover_claim_cooldown.to_le_bytes());
        out.push(u8::from(self.last_cover_occluded));
        out.extend_from_slice(&self.collisions.to_le_bytes());
        out.push(u8::from(self.arithmetic_overflowed));
        self.trail.encode(out);
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() <= CRAFT_BASE_ENCODED_LEN
            || bytes[106] > 1
            || bytes[126] > 1
            || bytes[131] > 1
        {
            return Err(CodecError("regolith craft: wrong length"));
        }
        let i64_at = |o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        let i32_at = |o| i32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        Ok(Self {
            archetype: Archetype::from_tag(bytes[0])?,
            weapon: WeaponKind::from_tag(bytes[1])?,
            pos: QPos {
                x: i64_at(2),
                y: i64_at(10),
                z: i64_at(18),
            },
            vel: QVel {
                x: i64_at(26),
                y: i64_at(34),
                z: i64_at(42),
            },
            trail: Trail::decode(&bytes[CRAFT_BASE_ENCODED_LEN..])?,
            yaw_urad: i32_at(50),
            pitch_urad: i32_at(54),
            hull: i32_at(58),
            shield: i32_at(62),
            cooldown: u16::from_le_bytes(bytes[66..68].try_into().unwrap()),
            shots: u32::from_le_bytes(bytes[68..72].try_into().unwrap()),
            damage_dealt: u64::from_le_bytes(bytes[72..80].try_into().unwrap()),
            grabs_attempted: u32::from_le_bytes(bytes[80..84].try_into().unwrap()),
            pickups_won: u32::from_le_bytes(bytes[84..88].try_into().unwrap()),
            grabs_lost: u32::from_le_bytes(bytes[88..92].try_into().unwrap()),
            respawn_in: u16::from_le_bytes(bytes[92..94].try_into().unwrap()),
            score_rock_points: u64::from_le_bytes(bytes[94..102].try_into().unwrap()),
            kills: u32::from_le_bytes(bytes[102..106].try_into().unwrap()),
            lock_target: (bytes[106] == 1).then(|| {
                orrery_protocol::PersistId::new(u64::from_le_bytes(
                    bytes[107..115].try_into().unwrap(),
                ))
            }),
            lock_class: LockClass::from_tag(bytes[115])?,
            lock_progress: u16::from_le_bytes(bytes[116..118].try_into().unwrap()),
            locks_acquired: u32::from_le_bytes(bytes[118..122].try_into().unwrap()),
            lock_decay_progress: u16::from_le_bytes(bytes[122..124].try_into().unwrap()),
            cover_claim_cooldown: u16::from_le_bytes(bytes[124..126].try_into().unwrap()),
            last_cover_occluded: bytes[126] == 1,
            collisions: u32::from_le_bytes(bytes[127..131].try_into().unwrap()),
            arithmetic_overflowed: bytes[131] == 1,
        })
    }
}

impl CoreCodec for Rock {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.tier.tag());
        out.extend_from_slice(&self.generation.to_le_bytes());
        for value in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.hull.to_le_bytes());
        out.extend_from_slice(&self.splits_done.to_le_bytes());
        out.push(u8::from(self.born_in_bloom));
        out.extend_from_slice(&self.pickups_dropped.to_le_bytes());
        match self.bloom {
            Some(bloom) => {
                out.push(1);
                out.extend_from_slice(&bloom.director.0.to_le_bytes());
                out.extend_from_slice(&bloom.bloom_index.to_le_bytes());
            }
            None => out.extend_from_slice(&[0; 13]),
        }
        out.extend_from_slice(&self.collisions.to_le_bytes());
        out.push(u8::from(self.arithmetic_overflowed));
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 84 || bytes[61] > 1 || bytes[66] > 1 || bytes[83] > 1 {
            return Err(CodecError("regolith rock: wrong length"));
        }
        let i64_at = |o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        Ok(Self {
            tier: RockTier::from_tag(bytes[0])?,
            generation: u32::from_le_bytes(bytes[1..5].try_into().unwrap()),
            pos: QPos {
                x: i64_at(5),
                y: i64_at(13),
                z: i64_at(21),
            },
            vel: QVel {
                x: i64_at(29),
                y: i64_at(37),
                z: i64_at(45),
            },
            hull: i32::from_le_bytes(bytes[53..57].try_into().unwrap()),
            splits_done: u32::from_le_bytes(bytes[57..61].try_into().unwrap()),
            born_in_bloom: bytes[61] == 1,
            pickups_dropped: u32::from_le_bytes(bytes[62..66].try_into().unwrap()),
            bloom: (bytes[66] == 1).then(|| BloomMembership {
                director: orrery_protocol::PersistId::new(u64::from_le_bytes(
                    bytes[67..75].try_into().unwrap(),
                )),
                bloom_index: u32::from_le_bytes(bytes[75..79].try_into().unwrap()),
            }),
            collisions: u32::from_le_bytes(bytes[79..83].try_into().unwrap()),
            arithmetic_overflowed: bytes[83] == 1,
        })
    }
}

impl CoreCodec for Pickup {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.kind.tag());
        for value in [self.pos.x, self.pos.y, self.pos.z] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.expires_at.to_le_bytes());
        out.extend_from_slice(&self.ttl_remaining.to_le_bytes());
        match self.claimed_by {
            Some(entity) => {
                out.push(1);
                out.extend_from_slice(&entity.0.to_le_bytes());
            }
            None => out.extend_from_slice(&[0; 9]),
        }
        match self.claimed_at {
            Some(at) => {
                out.push(1);
                out.extend_from_slice(&at.to_le_bytes());
            }
            None => out.extend_from_slice(&[0; 3]),
        }
        out.push(u8::from(self.expired));
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 42 || bytes[29] > 1 || bytes[38] > 1 || bytes[41] > 1 {
            return Err(CodecError("regolith pickup: bad length or option tag"));
        }
        let i64_at = |o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        Ok(Self {
            kind: WeaponKind::from_tag(bytes[0])?,
            pos: QPos {
                x: i64_at(1),
                y: i64_at(9),
                z: i64_at(17),
            },
            expires_at: u16::from_le_bytes(bytes[25..27].try_into().unwrap()),
            ttl_remaining: u16::from_le_bytes(bytes[27..29].try_into().unwrap()),
            claimed_by: (bytes[29] == 1).then(|| {
                orrery_protocol::PersistId::new(u64::from_le_bytes(
                    bytes[30..38].try_into().unwrap(),
                ))
            }),
            claimed_at: (bytes[38] == 1)
                .then(|| u16::from_le_bytes(bytes[39..41].try_into().unwrap())),
            expired: bytes[41] == 1,
        })
    }
}

impl CoreCodec for BloomDirector {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.clock_tick.to_le_bytes());
        out.extend_from_slice(&self.next_bloom_tick.to_le_bytes());
        out.extend_from_slice(&self.blooms_seeded.to_le_bytes());
        match self.site_pos {
            Some(pos) => {
                out.push(1);
                for value in [pos.x, pos.y, pos.z] {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
            None => out.extend_from_slice(&[0; 25]),
        }
        match self.site_active_until {
            Some(tick) => {
                out.push(1);
                out.extend_from_slice(&tick.to_le_bytes());
            }
            None => out.extend_from_slice(&[0; 9]),
        }
        out.extend_from_slice(&self.site_rocks_alive.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 56 || bytes[20] > 1 || bytes[45] > 1 {
            return Err(CodecError(
                "regolith bloom director: bad length or option tag",
            ));
        }
        let i64_at = |o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        Ok(Self {
            clock_tick: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            next_bloom_tick: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            blooms_seeded: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            site_pos: (bytes[20] == 1).then(|| QPos {
                x: i64_at(21),
                y: i64_at(29),
                z: i64_at(37),
            }),
            site_active_until: (bytes[45] == 1)
                .then(|| u64::from_le_bytes(bytes[46..54].try_into().unwrap())),
            site_rocks_alive: u16::from_le_bytes(bytes[54..56].try_into().unwrap()),
        })
    }
}

impl CoreCodec for RegolithState {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Craft(craft) => {
                out.push(0);
                craft.encode(out);
            }
            Self::Rock(rock) => {
                out.push(1);
                rock.encode(out);
            }
            Self::Pickup(pickup) => {
                out.push(2);
                pickup.encode(out);
            }
            Self::BloomDirector(director) => {
                out.push(3);
                director.encode(out);
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes
            .split_first()
            .ok_or(CodecError("regolith state: empty"))?;
        match tag {
            0 => Ok(Self::Craft(Craft::decode(rest)?)),
            1 => Ok(Self::Rock(Rock::decode(rest)?)),
            2 => Ok(Self::Pickup(Pickup::decode(rest)?)),
            3 => Ok(Self::BloomDirector(BloomDirector::decode(rest)?)),
            _ => Err(CodecError("regolith state: unknown tag")),
        }
    }
}

impl Craft {
    /// Full, equipped spawn state.
    #[must_use]
    pub fn spawned(archetype: Archetype, pos: QPos, yaw_urad: i32) -> Self {
        let limits = archetype.limits();
        Self {
            archetype,
            weapon: WeaponKind::Stock,
            pos,
            vel: QVel::default(),
            trail: Trail::default(),
            yaw_urad: yaw_urad.rem_euclid(TAU_URAD),
            pitch_urad: 0,
            hull: limits.max_hull,
            shield: limits.max_shield,
            cooldown: 0,
            shots: 0,
            damage_dealt: 0,
            grabs_attempted: 0,
            pickups_won: 0,
            grabs_lost: 0,
            respawn_in: 0,
            score_rock_points: 0,
            kills: 0,
            lock_target: None,
            lock_class: None,
            lock_progress: 0,
            locks_acquired: 0,
            lock_decay_progress: 0,
            cover_claim_cooldown: 0,
            last_cover_occluded: false,
            collisions: 0,
            arithmetic_overflowed: false,
        }
    }
    /// Whether this craft is active.
    #[must_use]
    pub const fn alive(&self) -> bool {
        self.hull > 0
    }

    /// Session score derived from the three monotone hashed counters.
    #[must_use]
    pub const fn score(&self) -> u64 {
        self.score_rock_points
            .saturating_add((self.kills as u64).saturating_mul(super::KILL_SCORE_POINTS))
            .saturating_add((self.pickups_won as u64).saturating_mul(super::PICKUP_SCORE_POINTS))
    }
}

impl Rock {
    /// A fully described initial rock state.
    #[must_use]
    pub const fn spawned(tier: RockTier, generation: u32, pos: QPos, vel: QVel) -> Self {
        Self {
            tier,
            generation,
            pos,
            vel,
            hull: tier.limits().max_hull,
            splits_done: 0,
            born_in_bloom: false,
            pickups_dropped: 0,
            bloom: None,
            collisions: 0,
            arithmetic_overflowed: false,
        }
    }

    /// A fully described rock seeded by a bloom director.
    #[must_use]
    pub const fn spawned_in_bloom(
        tier: RockTier,
        pos: QPos,
        vel: QVel,
        director: orrery_protocol::PersistId,
        bloom_index: u32,
    ) -> Self {
        let mut rock = Self::spawned(tier, 0, pos, vel);
        rock.born_in_bloom = true;
        rock.bloom = Some(BloomMembership {
            director,
            bloom_index,
        });
        rock
    }
}

impl Pickup {
    /// A fully described initial pickup state.
    #[must_use]
    pub const fn spawned(pos: QPos, kind: WeaponKind, expires_at: u16) -> Self {
        Self {
            pos,
            kind,
            expires_at,
            ttl_remaining: expires_at,
            claimed_by: None,
            claimed_at: None,
            expired: false,
        }
    }
}

impl BloomDirector {
    /// A fresh island director whose first bloom starts after one cadence.
    #[must_use]
    pub const fn spawned() -> Self {
        Self {
            clock_tick: 0,
            next_bloom_tick: super::BLOOM_CADENCE_TICKS,
            blooms_seeded: 0,
            site_pos: None,
            site_active_until: None,
            site_rocks_alive: 0,
        }
    }
}
