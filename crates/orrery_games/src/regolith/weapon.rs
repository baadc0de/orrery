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
    /// Range at which the weapon has no range penalty.
    pub optimal_mm: i64,
    /// Additional range over which accuracy decays.
    pub falloff_mm: i64,
    /// Projectile speed, in millimetres per second.
    pub projectile_speed_mms: i64,
    /// Turret tracking speed, in micro-radians per second at reference signature.
    pub tracking_urad_per_sec: u32,
}

/// The longest reach any weapon in the table can resolve a hit at, in
/// millimetres.
///
/// Derived from the table rather than restated, so a weapon added below moves
/// this figure with it. `projectile_resolution` breaks the lock beyond
/// `optimal_mm + falloff_mm` (plus the target's own radius), so `falloff_mm`
/// genuinely extends reach — it does not merely scale damage.
pub const MAX_WEAPON_REACH_MM: i64 = {
    let mut longest = 0;
    let mut index = 0;
    while index < WeaponKind::ALL.len() {
        let reach = WeaponKind::ALL[index].weapon().reach_mm();
        if reach > longest {
            longest = reach;
        }
        index += 1;
    }
    longest
};

impl Weapon {
    /// The greatest range at which this weapon can still resolve a hit,
    /// before the target's own signature radius is added.
    ///
    /// This is the figure `projectile_resolution` compares the live range
    /// against before it breaks the lock with `RangeExceeded`.
    #[must_use]
    pub const fn reach_mm(&self) -> i64 {
        self.optimal_mm.saturating_add(self.falloff_mm)
    }
}

impl WeaponKind {
    /// Every weapon in the table, in wire-tag order.
    ///
    /// The AOI sizing and the reach guard iterate this rather than a
    /// hand-kept list, so a weapon added to the table cannot go unaccounted.
    pub const ALL: [Self; 3] = [Self::Stock, Self::Volley, Self::Heavy];

    /// This kind's table entry.
    #[must_use]
    pub const fn weapon(self) -> Weapon {
        match self {
            Self::Stock => Weapon {
                damage_base: 10,
                damage_spread: 4,
                rolls: 1,
                cooldown_ticks: 20,
                optimal_mm: 300_000,
                falloff_mm: 100_000,
                projectile_speed_mms: 300_000,
                tracking_urad_per_sec: 180_000,
            },
            Self::Volley => Weapon {
                damage_base: 5,
                damage_spread: 2,
                rolls: 3,
                cooldown_ticks: 30,
                optimal_mm: 200_000,
                falloff_mm: 100_000,
                projectile_speed_mms: 450_000,
                tracking_urad_per_sec: 300_000,
            },
            Self::Heavy => Weapon {
                damage_base: 45,
                damage_spread: 12,
                rolls: 1,
                cooldown_ticks: 90,
                optimal_mm: 700_000,
                falloff_mm: 200_000,
                projectile_speed_mms: 180_000,
                tracking_urad_per_sec: 60_000,
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
