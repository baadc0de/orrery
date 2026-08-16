//! Craft archetypes, and the limits that make cheap checks possible at all.
//!
//! Stage-1 invariants are "impossible value" checks, and *impossible* is only
//! definable against a declared limit. A game whose craft have no published
//! ceiling on speed or fire rate cannot be checked cheaply by anybody — the
//! only recourse is replay, on every entity, forever. So the limits live here,
//! in core state, where a witness reads them off the sample it is judging.
//!
//! The numbers are invented defaults. D16 fixes the *bands* (ε_pos 1 cm, ε_vel
//! 1 cm/s) and the lattice is 1 mm; nothing in the architecture fixes how fast
//! a fictional interceptor flies. They are chosen so that the two archetypes
//! differ on every axis a check reads — speed, acceleration, reach, cadence,
//! durability — because a suite where both archetypes share a limit would pass
//! with the archetype lookup wired to a constant.

use orrery_core::CodecError;

/// The two craft this game fields.
///
/// An entity's archetype never changes, which is itself checkable: see
/// [`crate::skirmish::invariants`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Archetype {
    /// Fast, fragile, short-ranged, high cadence.
    Interceptor,
    /// Slow, durable, long-ranged, slow cadence.
    Cruiser,
}

/// What an archetype may do. Every field is integer: these are read by
/// integer-only checks (VC-5), so a limit expressed as a float would drag the
/// tolerance question into a place it does not belong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Speed ceiling, millimetres per second.
    pub max_speed_mms: i64,
    /// Acceleration ceiling, millimetres per second squared.
    pub max_accel_mmss: i64,
    /// Hull at spawn, and its ceiling. Integer — this is persistent value.
    pub max_hull: i32,
    /// Shield at spawn, and its ceiling.
    pub max_shield: i32,
    /// Weapon reach, millimetres.
    pub range_mm: i64,
    /// Ticks between shots.
    pub cooldown_ticks: u16,
    /// Damage before the seeded roll.
    pub damage_base: u32,
    /// The roll is uniform over `[0, damage_spread)`.
    pub damage_spread: u32,
}

impl Archetype {
    /// Every archetype, in encoding order.
    pub const ALL: &'static [Archetype] = &[Archetype::Interceptor, Archetype::Cruiser];

    /// This archetype's limits.
    #[must_use]
    pub const fn limits(self) -> Limits {
        match self {
            Archetype::Interceptor => Limits {
                max_speed_mms: 120_000,
                max_accel_mmss: 60_000,
                max_hull: 100,
                max_shield: 50,
                range_mm: 400_000,
                cooldown_ticks: 30,
                damage_base: 12,
                damage_spread: 8,
            },
            Archetype::Cruiser => Limits {
                max_speed_mms: 60_000,
                max_accel_mmss: 20_000,
                max_hull: 300,
                max_shield: 150,
                range_mm: 900_000,
                cooldown_ticks: 90,
                damage_base: 40,
                damage_spread: 16,
            },
        }
    }

    /// The wire tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Archetype::Interceptor => 0,
            Archetype::Cruiser => 1,
        }
    }

    /// Decode a wire tag.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] for a tag this build does not know. A decoder
    /// that fell back to a default would hand the invariants a limit table the
    /// authority never used.
    pub const fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0 => Ok(Archetype::Interceptor),
            1 => Ok(Archetype::Cruiser),
            _ => Err(CodecError("craft: unknown archetype")),
        }
    }

    /// The archetype flown in scenario slot `slot`.
    ///
    /// Alternating rather than random: a population that is half of each by
    /// construction keeps every scenario exercising both limit tables, at any
    /// entity count down to two.
    #[must_use]
    pub const fn for_slot(slot: u64) -> Self {
        if slot.is_multiple_of(2) {
            Archetype::Interceptor
        } else {
            Archetype::Cruiser
        }
    }
}
