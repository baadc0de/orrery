//! Hashed, own-state craft trace for Regolith.

use orrery_core::{CodecError, CoreCodec, QPos, QVel, Quantized};

use super::{archetype::Archetype, weapon::WeaponKind};

/// Full turn in micro-radians.
pub const TAU_URAD: i32 = 6_283_185;
/// The inherited schema limit; Regolith's pilot always supplies zero pitch.
pub const PITCH_LIMIT_URAD: i32 = 1_570_796;

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
    /// Heading.
    pub yaw_urad: i32,
    /// Retained schema field; input discipline locks it to zero.
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
}

const ENCODED_LEN: usize = 80;

impl Quantized for Craft {
    fn quantize(&mut self) {
        let (x, y, z) = self.pos.to_metres();
        self.pos = QPos::from_metres(x, y, z);
        let (x, y, z) = self.vel.to_metres_per_sec();
        self.vel = QVel::from_metres_per_sec(x, y, z);
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
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != ENCODED_LEN {
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
            yaw_urad: i32_at(50),
            pitch_urad: i32_at(54),
            hull: i32_at(58),
            shield: i32_at(62),
            cooldown: u16::from_le_bytes(bytes[66..68].try_into().unwrap()),
            shots: u32::from_le_bytes(bytes[68..72].try_into().unwrap()),
            damage_dealt: u64::from_le_bytes(bytes[72..80].try_into().unwrap()),
        })
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
            yaw_urad: yaw_urad.rem_euclid(TAU_URAD),
            pitch_urad: 0,
            hull: limits.max_hull,
            shield: limits.max_shield,
            cooldown: 0,
            shots: 0,
            damage_dealt: 0,
        }
    }
    /// Whether this craft is active.
    #[must_use]
    pub const fn alive(&self) -> bool {
        self.hull > 0
    }
}
