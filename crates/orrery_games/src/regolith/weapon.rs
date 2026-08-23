//! The Regolith weapon table. A weapon kind is part of the shooter's hashed state.

use orrery_core::CodecError;

/// The single weapon slot's possible contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WeaponKind {
    /// Infinite spawn weapon.
    Stock,
    /// Three left-slot-first rolls.
    Volley,
    /// Slow long-range weapon.
    Heavy,
}

/// Integer weapon limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weapon {
    /// Damage before each roll.
    pub damage_base: u32,
    /// Uniform roll spread.
    pub damage_spread: u32,
    /// Rolls per trigger.
    pub rolls: u8,
    /// Decrement-first cadence.
    pub cooldown_ticks: u16,
    /// Target-side reach.
    pub reach_mm: i64,
}

impl WeaponKind {
    /// This kind's table entry.
    #[must_use]
    pub const fn weapon(self) -> Weapon {
        match self {
            Self::Stock => Weapon {
                damage_base: 10,
                damage_spread: 4,
                rolls: 1,
                cooldown_ticks: 20,
                reach_mm: 400_000,
            },
            Self::Volley => Weapon {
                damage_base: 5,
                damage_spread: 2,
                rolls: 3,
                cooldown_ticks: 30,
                reach_mm: 300_000,
            },
            Self::Heavy => Weapon {
                damage_base: 45,
                damage_spread: 12,
                rolls: 1,
                cooldown_ticks: 90,
                reach_mm: 900_000,
            },
        }
    }
    /// Wire tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Stock => 0,
            Self::Volley => 1,
            Self::Heavy => 2,
        }
    }
    /// Decodes a wire tag.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown weapon.
    pub const fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0 => Ok(Self::Stock),
            1 => Ok(Self::Volley),
            2 => Ok(Self::Heavy),
            _ => Err(CodecError("regolith: unknown weapon")),
        }
    }
}
