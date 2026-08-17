//! `Craft` — the verifiable state of one ship.
//!
//! Two decisions here carry more weight than they look like they do.
//!
//! **Continuous fields are held on the lattice.** `QPos`/`QVel` are integer
//! millimetre quanta, so the f64 excursion inside `step` never survives a tick
//! boundary (VC-7) and platform drift cannot accumulate across a window. The
//! angles are integer micro-radians for the same reason from the other side:
//! the *input* to `libm::cos` is bit-identical everywhere, so the matrix tests
//! libm rather than testing whatever the previous tick's float happened to be.
//!
//! **Discrete outcomes leave a trace in the emitter's own state.** [`shots`]
//! and [`damage_dealt`] are running counters that no gameplay rule reads back.
//! They exist because a cross-entity effect travels as an event, the event
//! becomes the *target's* logged input, and the target's replay then reproduces
//! it faithfully — an inflated damage roll is perfectly self-consistent on the
//! victim's side. It is only visible where it was *decided*, and it is only
//! visible there if the decision changed the decider's own state hash. A game
//! that emitted damage without recording it would have written itself an
//! unadjudicable cheat.
//!
//! [`shots`]: Craft::shots
//! [`damage_dealt`]: Craft::damage_dealt

use orrery_core::{CodecError, CoreCodec, QPos, QVel, Quantized};

use super::archetype::Archetype;

/// Half a turn in micro-radians, the pitch clamp.
pub const PITCH_LIMIT_URAD: i32 = 1_570_796;

/// A full turn in micro-radians. Yaw is kept inside `[0, TAU)` so the argument
/// handed to `libm` stays small: a yaw allowed to wander to ±2000 radians
/// would still be deterministic, but it would be testing argument reduction
/// rather than the rules.
pub const TAU_URAD: i32 = 6_283_185;

/// One craft's verifiable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Craft {
    /// Which limit table this craft is held to. Never changes.
    pub archetype: Archetype,
    /// Position, on the millimetre lattice.
    pub pos: QPos,
    /// Velocity, on the millimetre-per-second lattice.
    pub vel: QVel,
    /// Heading about the vertical axis, micro-radians in `[0, TAU_URAD)`.
    pub yaw_urad: i32,
    /// Elevation, micro-radians, clamped to ±[`PITCH_LIMIT_URAD`].
    pub pitch_urad: i32,
    /// Hull. Zero is destroyed; it never goes negative (VC-5, discrete).
    pub hull: i32,
    /// Shield, absorbed before hull.
    pub shield: i32,
    /// Ticks until the weapon may fire again.
    pub cooldown: u16,
    /// Shots fired, ever. Monotone: the fire-rate check reads it.
    ///
    /// Rolls, not landings. Whether a shot reached anything is resolved in the
    /// *target's* step, which this craft's own replay never runs, so counting
    /// hits here would be counting something this entity cannot know alone.
    pub shots: u32,
    /// Damage rolled, ever. Monotone: this is what makes an inflated roll
    /// adjudicable at the attacker. Rolled rather than landed, for the same
    /// reason [`shots`](Craft::shots) is.
    pub damage_dealt: u64,
}

/// The canonical encoding's byte length.
const CRAFT_ENCODED_LEN: usize = 1 + 24 + 24 + 4 + 4 + 4 + 4 + 2 + 4 + 8;

impl Quantized for Craft {
    fn quantize(&mut self) {
        // Idempotent — `step` already wrote lattice points. Re-snapping is
        // what makes a state loaded from a checkpoint or an evidence bundle
        // start the first tick from a point the authority actually occupied.
        let (x, y, z) = self.pos.to_metres();
        self.pos = QPos::from_metres(x, y, z);
        let (vx, vy, vz) = self.vel.to_metres_per_sec();
        self.vel = QVel::from_metres_per_sec(vx, vy, vz);
    }
}

impl CoreCodec for Craft {
    fn encode(&self, out: &mut Vec<u8>) {
        // Fixed field order, little-endian, no map iteration (VC-4): two
        // builds that encoded this differently would manufacture a deviation
        // out of nothing.
        out.push(self.archetype.tag());
        for v in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.yaw_urad.to_le_bytes());
        out.extend_from_slice(&self.pitch_urad.to_le_bytes());
        out.extend_from_slice(&self.hull.to_le_bytes());
        out.extend_from_slice(&self.shield.to_le_bytes());
        out.extend_from_slice(&self.cooldown.to_le_bytes());
        out.extend_from_slice(&self.shots.to_le_bytes());
        out.extend_from_slice(&self.damage_dealt.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != CRAFT_ENCODED_LEN {
            return Err(CodecError("craft: wrong length"));
        }
        let i64_at = |o: usize| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        Ok(Self {
            archetype: Archetype::from_tag(bytes[0])?,
            pos: QPos {
                x: i64_at(1),
                y: i64_at(9),
                z: i64_at(17),
            },
            vel: QVel {
                x: i64_at(25),
                y: i64_at(33),
                z: i64_at(41),
            },
            yaw_urad: i32_at(49),
            pitch_urad: i32_at(53),
            hull: i32_at(57),
            shield: i32_at(61),
            cooldown: u16::from_le_bytes(bytes[65..67].try_into().unwrap()),
            shots: u32::from_le_bytes(bytes[67..71].try_into().unwrap()),
            damage_dealt: u64::from_le_bytes(bytes[71..79].try_into().unwrap()),
        })
    }
}

impl Craft {
    /// A craft at spawn: full hull and shield, at rest, weapon ready.
    #[must_use]
    pub fn spawned(archetype: Archetype, pos: QPos, yaw_urad: i32) -> Self {
        let limits = archetype.limits();
        Self {
            archetype,
            pos,
            vel: QVel::default(),
            yaw_urad: yaw_urad.rem_euclid(TAU_URAD),
            pitch_urad: 0,
            hull: limits.max_hull,
            shield: limits.max_shield,
            cooldown: 0,
            shots: 0,
            damage_dealt: 0,
        }
    }

    /// Whether this craft is still in the fight.
    #[must_use]
    pub const fn alive(&self) -> bool {
        self.hull > 0
    }
}
