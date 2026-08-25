//! Regolith craft chassis limits. Weapons own all offensive limits.

use orrery_core::CodecError;

use super::state::TAU_URAD;

/// One chassis weapon arc in craft-local micro-radians.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiringArc {
    /// Stable presentation name for the arc.
    pub name: &'static str,
    /// Centre bearing from the craft's nose.
    pub centre_urad: i32,
    /// Half of the arc's total width.
    pub half_width_urad: i32,
}

impl FiringArc {
    /// Whether a craft-local bearing falls inside this arc, including its edges.
    #[must_use]
    pub fn contains(self, bearing_urad: i32) -> bool {
        let mut delta = bearing_urad
            .saturating_sub(self.centre_urad)
            .rem_euclid(TAU_URAD);
        if delta > TAU_URAD / 2 {
            delta -= TAU_URAD;
        }
        delta.abs() <= self.half_width_urad
    }
}

const INTERCEPTOR_ARCS: [FiringArc; 1] = [FiringArc {
    name: "arc_front",
    centre_urad: 0,
    half_width_urad: 785_398,
}];
const CRUISER_ARCS: [FiringArc; 2] = [
    FiringArc {
        name: "arc_starboard",
        centre_urad: 1_570_796,
        half_width_urad: 392_699,
    },
    FiringArc {
        name: "arc_port",
        centre_urad: -1_570_796,
        half_width_urad: 392_699,
    },
];

/// The two chassis Regolith fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Archetype {
    /// Fast, light chassis.
    Interceptor,
    /// Slow, durable chassis.
    Cruiser,
}

/// Integer limits derived from a craft's own hashed chassis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Speed ceiling, millimetres per second.
    pub max_speed_mms: i64,
    /// Acceleration ceiling, millimetres per second squared.
    pub max_accel_mmss: i64,
    /// Hull at spawn and its ceiling.
    pub max_hull: i32,
    /// Shield at spawn and its ceiling.
    pub max_shield: i32,
    /// Collision radius, millimetres, for the later rock resolver.
    pub radius_mm: i64,
}

impl Archetype {
    /// Every archetype, in encoding order.
    pub const ALL: &'static [Self] = &[Self::Interceptor, Self::Cruiser];

    /// Weapon arcs adjudicated for this chassis.
    #[must_use]
    pub const fn firing_arcs(self) -> &'static [FiringArc] {
        match self {
            Self::Interceptor => &INTERCEPTOR_ARCS,
            Self::Cruiser => &CRUISER_ARCS,
        }
    }

    /// This chassis's limits.
    #[must_use]
    pub const fn limits(self) -> Limits {
        match self {
            Self::Interceptor => Limits {
                max_speed_mms: 120_000,
                max_accel_mmss: 60_000,
                max_hull: 100,
                max_shield: 50,
                radius_mm: 3_000,
            },
            Self::Cruiser => Limits {
                max_speed_mms: 60_000,
                max_accel_mmss: 20_000,
                max_hull: 300,
                max_shield: 150,
                radius_mm: 6_000,
            },
        }
    }

    /// Wire tag.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Interceptor => 0,
            Self::Cruiser => 1,
        }
    }

    /// Decodes a wire tag.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown chassis.
    pub const fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0 => Ok(Self::Interceptor),
            1 => Ok(Self::Cruiser),
            _ => Err(CodecError("regolith: unknown archetype")),
        }
    }

    /// Alternates chassis by scenario slot.
    #[must_use]
    pub const fn for_slot(slot: u64) -> Self {
        if slot.is_multiple_of(2) {
            Self::Interceptor
        } else {
            Self::Cruiser
        }
    }
}
