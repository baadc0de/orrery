//! Regolith's one grammar extension: damage identifies a weapon, never a raw reach.

use super::weapon::WeaponKind;
use orrery_core::{CodecError, CoreCodec, QPos, QVel};
use orrery_protocol::PersistId;

use super::state::RockTier;

fn encode_pos(pos: QPos, out: &mut Vec<u8>) {
    for value in [pos.x, pos.y, pos.z] {
        out.extend_from_slice(&value.to_le_bytes());
    }
}
fn decode_pos(bytes: &[u8]) -> QPos {
    let at = |o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    QPos {
        x: at(0),
        y: at(8),
        z: at(16),
    }
}
fn encode_vel(vel: QVel, out: &mut Vec<u8>) {
    for value in [vel.x, vel.y, vel.z] {
        out.extend_from_slice(&value.to_le_bytes());
    }
}
fn decode_vel(bytes: &[u8]) -> QVel {
    let at = |o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    QVel {
        x: at(0),
        y: at(8),
        z: at(16),
    }
}

/// One complete child description. The event is self-sufficient: executor
/// materialization never allocates an id or consults the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Derived child identifier.
    pub id: PersistId,
    /// Child tier.
    pub tier: RockTier,
    /// Child lattice position.
    pub pos: QPos,
    /// Child lattice velocity.
    pub vel: QVel,
}

/// An ordered core input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    /// Accelerate then turn using the inherited kinematics.
    Thrust {
        /// Requested acceleration.
        accel_mmss: i32,
        /// Yaw delta.
        yaw_urad: i32,
        /// Pitch delta (honest input is zero).
        pitch_urad: i32,
    },
    /// Fire the equipped weapon at a target.
    Fire {
        /// Target id.
        target: PersistId,
    },
    /// Damage delivered from a prior tick.
    Damage {
        /// Rolled amount.
        amount: i32,
        /// Shooter id.
        from: PersistId,
        /// Shooter origin.
        from_pos: QPos,
        /// Weapon from the shooter's hashed state.
        from_weapon: WeaponKind,
    },
}

impl CoreCodec for Order {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Thrust {
                accel_mmss,
                yaw_urad,
                pitch_urad,
            } => {
                out.push(0);
                out.extend_from_slice(&accel_mmss.to_le_bytes());
                out.extend_from_slice(&yaw_urad.to_le_bytes());
                out.extend_from_slice(&pitch_urad.to_le_bytes());
            }
            Self::Fire { target } => {
                out.push(1);
                out.extend_from_slice(&target.0.to_le_bytes());
            }
            Self::Damage {
                amount,
                from,
                from_pos,
                from_weapon,
            } => {
                out.push(2);
                out.extend_from_slice(&amount.to_le_bytes());
                out.extend_from_slice(&from.0.to_le_bytes());
                encode_pos(*from_pos, out);
                out.push(from_weapon.tag());
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes
            .split_first()
            .ok_or(CodecError("regolith order: empty"))?;
        match (tag, rest.len()) {
            (0, 12) => Ok(Self::Thrust {
                accel_mmss: i32::from_le_bytes(rest[0..4].try_into().unwrap()),
                yaw_urad: i32::from_le_bytes(rest[4..8].try_into().unwrap()),
                pitch_urad: i32::from_le_bytes(rest[8..12].try_into().unwrap()),
            }),
            (1, 8) => Ok(Self::Fire {
                target: PersistId::new(u64::from_le_bytes(rest.try_into().unwrap())),
            }),
            (2, 37) => Ok(Self::Damage {
                amount: i32::from_le_bytes(rest[0..4].try_into().unwrap()),
                from: PersistId::new(u64::from_le_bytes(rest[4..12].try_into().unwrap())),
                from_pos: decode_pos(&rest[12..36]),
                from_weapon: WeaponKind::from_tag(rest[36])?,
            }),
            _ => Err(CodecError("regolith order: bad tag or length")),
        }
    }
}

/// A deterministic outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A rolled shot for the target's next step.
    DamageDealt {
        /// Shooter.
        attacker: PersistId,
        /// Target.
        target: PersistId,
        /// Rolled amount.
        amount: i32,
        /// Firing position.
        attacker_pos: QPos,
        /// Equipped firing weapon.
        attacker_weapon: WeaponKind,
    },
    /// A hull reached zero.
    Destroyed {
        /// Last attacker.
        by: PersistId,
    },
    /// A dying parent produced its two ordered children.
    Split {
        /// The emitting parent.
        parent: PersistId,
        /// The parent generation, included in the id derivation.
        generation: u32,
        /// Slot-zero then slot-one children.
        children: [ChildSpec; 2],
    },
}

impl CoreCodec for Outcome {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::DamageDealt {
                attacker,
                target,
                amount,
                attacker_pos,
                attacker_weapon,
            } => {
                out.push(0);
                out.extend_from_slice(&attacker.0.to_le_bytes());
                out.extend_from_slice(&target.0.to_le_bytes());
                out.extend_from_slice(&amount.to_le_bytes());
                encode_pos(*attacker_pos, out);
                out.push(attacker_weapon.tag());
            }
            Self::Destroyed { by } => {
                out.push(1);
                out.extend_from_slice(&by.0.to_le_bytes());
            }
            Self::Split {
                parent,
                generation,
                children,
            } => {
                out.push(2);
                out.extend_from_slice(&parent.0.to_le_bytes());
                out.extend_from_slice(&generation.to_le_bytes());
                for child in children {
                    out.extend_from_slice(&child.id.0.to_le_bytes());
                    out.push(child.tier.tag());
                    encode_pos(child.pos, out);
                    encode_vel(child.vel, out);
                }
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes
            .split_first()
            .ok_or(CodecError("regolith outcome: empty"))?;
        match (tag, rest.len()) {
            (0, 45) => Ok(Self::DamageDealt {
                attacker: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                target: PersistId::new(u64::from_le_bytes(rest[8..16].try_into().unwrap())),
                amount: i32::from_le_bytes(rest[16..20].try_into().unwrap()),
                attacker_pos: decode_pos(&rest[20..44]),
                attacker_weapon: WeaponKind::from_tag(rest[44])?,
            }),
            (1, 8) => Ok(Self::Destroyed {
                by: PersistId::new(u64::from_le_bytes(rest.try_into().unwrap())),
            }),
            (2, 126) => {
                let child = |offset: usize| -> Result<ChildSpec, CodecError> {
                    Ok(ChildSpec {
                        id: PersistId::new(u64::from_le_bytes(
                            rest[offset..offset + 8].try_into().unwrap(),
                        )),
                        tier: RockTier::from_tag(rest[offset + 8])?,
                        pos: decode_pos(&rest[offset + 9..offset + 33]),
                        vel: decode_vel(&rest[offset + 33..offset + 57]),
                    })
                };
                Ok(Self::Split {
                    parent: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                    generation: u32::from_le_bytes(rest[8..12].try_into().unwrap()),
                    children: [child(12)?, child(69)?],
                })
            }
            _ => Err(CodecError("regolith outcome: bad tag or length")),
        }
    }
}
