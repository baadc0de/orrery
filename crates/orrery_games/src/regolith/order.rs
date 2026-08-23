//! Regolith's ordered combat, materialization and pickup grammar.

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
    /// Ask this craft to attempt a pickup grab.
    Grab {
        /// Pickup entity to contest.
        pickup: PersistId,
    },
    /// A craft's emitted attempt, delivered to the pickup next tick.
    GrabAttempt {
        /// Attempting craft.
        ship: PersistId,
        /// Craft position from its own hashed state.
        ship_pos: QPos,
    },
    /// A pickup's grant delivered back to its winner.
    PickupGranted {
        /// Weapon kind from the pickup's hashed state.
        kind: WeaponKind,
    },
    /// A pickup's denial delivered back to an unsuccessful craft.
    PickupDenied,
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
            Self::Grab { pickup } => {
                out.push(3);
                out.extend_from_slice(&pickup.0.to_le_bytes());
            }
            Self::GrabAttempt { ship, ship_pos } => {
                out.push(4);
                out.extend_from_slice(&ship.0.to_le_bytes());
                encode_pos(*ship_pos, out);
            }
            Self::PickupGranted { kind } => {
                out.extend_from_slice(&[5, kind.tag()]);
            }
            Self::PickupDenied => out.push(6),
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
            (3, 8) => Ok(Self::Grab {
                pickup: PersistId::new(u64::from_le_bytes(rest.try_into().unwrap())),
            }),
            (4, 32) => Ok(Self::GrabAttempt {
                ship: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                ship_pos: decode_pos(&rest[8..32]),
            }),
            (5, 1) => Ok(Self::PickupGranted {
                kind: WeaponKind::from_tag(rest[0])?,
            }),
            (6, 0) => Ok(Self::PickupDenied),
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
    /// A dying Small produced one fully described pickup.
    SpawnPickup {
        /// Derived pickup identifier.
        id: PersistId,
        /// Pickup lattice position.
        pos: QPos,
        /// Weapon kind to grant.
        kind: WeaponKind,
        /// Lifetime boundary in ticks after materialization.
        expires_at: u16,
    },
    /// A craft emitted a grab attempt for delivery to the pickup.
    GrabAttempted {
        /// Pickup being contested.
        pickup: PersistId,
        /// Attempting craft.
        ship: PersistId,
        /// Craft position from its own hashed state.
        ship_pos: QPos,
    },
    /// The first eligible craft won.
    Granted {
        /// Winning craft.
        ship: PersistId,
        /// Weapon kind from the pickup's own state.
        kind: WeaponKind,
    },
    /// A craft was ineligible or arrived after the winner.
    Denied {
        /// Denied craft.
        ship: PersistId,
    },
    /// An unclaimed pickup reached its TTL.
    Expired {
        /// Expired pickup.
        id: PersistId,
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
            Self::SpawnPickup {
                id,
                pos,
                kind,
                expires_at,
            } => {
                out.push(3);
                out.extend_from_slice(&id.0.to_le_bytes());
                encode_pos(*pos, out);
                out.push(kind.tag());
                out.extend_from_slice(&expires_at.to_le_bytes());
            }
            Self::GrabAttempted {
                pickup,
                ship,
                ship_pos,
            } => {
                out.push(4);
                out.extend_from_slice(&pickup.0.to_le_bytes());
                out.extend_from_slice(&ship.0.to_le_bytes());
                encode_pos(*ship_pos, out);
            }
            Self::Granted { ship, kind } => {
                out.push(5);
                out.extend_from_slice(&ship.0.to_le_bytes());
                out.push(kind.tag());
            }
            Self::Denied { ship } => {
                out.push(6);
                out.extend_from_slice(&ship.0.to_le_bytes());
            }
            Self::Expired { id } => {
                out.push(7);
                out.extend_from_slice(&id.0.to_le_bytes());
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
            (3, 35) => Ok(Self::SpawnPickup {
                id: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                pos: decode_pos(&rest[8..32]),
                kind: WeaponKind::from_tag(rest[32])?,
                expires_at: u16::from_le_bytes(rest[33..35].try_into().unwrap()),
            }),
            (4, 40) => Ok(Self::GrabAttempted {
                pickup: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                ship: PersistId::new(u64::from_le_bytes(rest[8..16].try_into().unwrap())),
                ship_pos: decode_pos(&rest[16..40]),
            }),
            (5, 9) => Ok(Self::Granted {
                ship: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                kind: WeaponKind::from_tag(rest[8])?,
            }),
            (6, 8) => Ok(Self::Denied {
                ship: PersistId::new(u64::from_le_bytes(rest.try_into().unwrap())),
            }),
            (7, 8) => Ok(Self::Expired {
                id: PersistId::new(u64::from_le_bytes(rest.try_into().unwrap())),
            }),
            _ => Err(CodecError("regolith outcome: bad tag or length")),
        }
    }
}
