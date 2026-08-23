//! Regolith's ordered combat, materialization and pickup grammar.

use super::weapon::WeaponKind;
use orrery_core::{CodecError, CoreCodec, QPos, QVel};
use orrery_protocol::PersistId;

use super::state::{BloomMembership, RockTier};

/// A target-side fact that clears a lock without a live neighbour read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockBreakReason {
    /// The projectile was outside optimal plus falloff range.
    RangeExceeded,
    /// The target was destroyed before or by the projectile.
    TargetDestroyed,
}

impl LockBreakReason {
    const fn tag(self) -> u8 {
        match self {
            Self::RangeExceeded => 0,
            Self::TargetDestroyed => 1,
        }
    }

    const fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0 => Ok(Self::RangeExceeded),
            1 => Ok(Self::TargetDestroyed),
            _ => Err(CodecError("regolith: unknown lock-break reason")),
        }
    }
}

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
    /// Bloom lineage inherited from the parent, if any.
    pub bloom: Option<BloomMembership>,
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
        /// Shooter velocity at firing.
        from_vel: QVel,
        /// Weapon from the shooter's hashed state.
        from_weapon: WeaponKind,
        /// Remaining target-owned flight ticks; `None` marks first arrival.
        flight_ticks: Option<u16>,
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
    /// A destroyed craft's logged credit delivered to its killer.
    KillCredit,
    /// A destroyed rock's logged point value delivered to its killer.
    RockCredit {
        /// Resolver-owned points from the dead rock's hashed tier.
        points: u8,
    },
    /// A bloom rock lineage changed size, delivered to its director.
    BloomPopulationChanged {
        /// Director-local bloom generation.
        bloom_index: u32,
        /// Net live-lineage change: `+1` for a split, `-1` for a terminal death.
        delta: i8,
    },
    /// A target-side range or destruction fact delivered to the locker.
    LockBroken {
        /// Target whose lock ended.
        target: PersistId,
        /// Resolver-owned reason.
        reason: LockBreakReason,
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
                from_vel,
                from_weapon,
                flight_ticks,
            } => {
                out.push(2);
                out.extend_from_slice(&amount.to_le_bytes());
                out.extend_from_slice(&from.0.to_le_bytes());
                encode_pos(*from_pos, out);
                encode_vel(*from_vel, out);
                out.push(from_weapon.tag());
                match flight_ticks {
                    Some(ticks) => {
                        out.push(1);
                        out.extend_from_slice(&ticks.to_le_bytes());
                    }
                    None => out.extend_from_slice(&[0; 3]),
                }
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
            Self::KillCredit => out.push(7),
            Self::RockCredit { points } => out.extend_from_slice(&[8, *points]),
            Self::BloomPopulationChanged { bloom_index, delta } => {
                out.push(9);
                out.extend_from_slice(&bloom_index.to_le_bytes());
                out.push(delta.to_le_bytes()[0]);
            }
            Self::LockBroken { target, reason } => {
                out.push(10);
                out.extend_from_slice(&target.0.to_le_bytes());
                out.push(reason.tag());
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
            (2, 64) if rest[61] <= 1 => Ok(Self::Damage {
                amount: i32::from_le_bytes(rest[0..4].try_into().unwrap()),
                from: PersistId::new(u64::from_le_bytes(rest[4..12].try_into().unwrap())),
                from_pos: decode_pos(&rest[12..36]),
                from_vel: decode_vel(&rest[36..60]),
                from_weapon: WeaponKind::from_tag(rest[60])?,
                flight_ticks: (rest[61] == 1)
                    .then(|| u16::from_le_bytes(rest[62..64].try_into().unwrap())),
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
            (7, 0) => Ok(Self::KillCredit),
            (8, 1) => Ok(Self::RockCredit { points: rest[0] }),
            (9, 5) => Ok(Self::BloomPopulationChanged {
                bloom_index: u32::from_le_bytes(rest[0..4].try_into().unwrap()),
                delta: i8::from_le_bytes([rest[4]]),
            }),
            (10, 9) => Ok(Self::LockBroken {
                target: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                reason: LockBreakReason::from_tag(rest[8])?,
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
        /// Firing velocity.
        attacker_vel: QVel,
        /// Equipped firing weapon.
        attacker_weapon: WeaponKind,
        /// Remaining target-owned flight ticks; `None` marks first arrival.
        flight_ticks: Option<u16>,
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
    /// One director seeded ten fully described rocks in slot order.
    BloomSeeded {
        /// Emitting director.
        director: PersistId,
        /// Director-local bloom generation.
        bloom_index: u32,
        /// In-band site announcement position.
        site_pos: QPos,
        /// Absolute site expiry tick.
        active_until: u64,
        /// Two Large, three Medium, then five Small rocks.
        rocks: Box<[ChildSpec; 10]>,
    },
    /// A rock death earned resolver-owned points.
    RockDestroyed {
        /// Last attacker whose logged damage reduced hull to zero.
        by: PersistId,
        /// Points derived from the dead rock's own hashed tier.
        points: u8,
    },
    /// A bloom lineage split or ended, routed to the owning director.
    BloomPopulationChanged {
        /// Owning director.
        director: PersistId,
        /// Director-local bloom generation.
        bloom_index: u32,
        /// Net live-lineage change.
        delta: i8,
    },
    /// A target-side fact that clears a lock at its owner.
    LockBroken {
        /// Locker receiving the fact.
        locker: PersistId,
        /// Target whose lock ended.
        target: PersistId,
        /// Resolver-owned reason.
        reason: LockBreakReason,
    },
}

fn encode_bloom(bloom: Option<BloomMembership>, out: &mut Vec<u8>) {
    match bloom {
        Some(bloom) => {
            out.push(1);
            out.extend_from_slice(&bloom.director.0.to_le_bytes());
            out.extend_from_slice(&bloom.bloom_index.to_le_bytes());
        }
        None => out.extend_from_slice(&[0; 13]),
    }
}

fn decode_bloom(bytes: &[u8]) -> Result<Option<BloomMembership>, CodecError> {
    match bytes[0] {
        0 => Ok(None),
        1 => Ok(Some(BloomMembership {
            director: PersistId::new(u64::from_le_bytes(bytes[1..9].try_into().unwrap())),
            bloom_index: u32::from_le_bytes(bytes[9..13].try_into().unwrap()),
        })),
        _ => Err(CodecError("regolith outcome: bad bloom tag")),
    }
}

fn encode_child(child: &ChildSpec, out: &mut Vec<u8>) {
    out.extend_from_slice(&child.id.0.to_le_bytes());
    out.push(child.tier.tag());
    encode_pos(child.pos, out);
    encode_vel(child.vel, out);
    encode_bloom(child.bloom, out);
}

fn decode_child(bytes: &[u8]) -> Result<ChildSpec, CodecError> {
    Ok(ChildSpec {
        id: PersistId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap())),
        tier: RockTier::from_tag(bytes[8])?,
        pos: decode_pos(&bytes[9..33]),
        vel: decode_vel(&bytes[33..57]),
        bloom: decode_bloom(&bytes[57..70])?,
    })
}

impl CoreCodec for Outcome {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::DamageDealt {
                attacker,
                target,
                amount,
                attacker_pos,
                attacker_vel,
                attacker_weapon,
                flight_ticks,
            } => {
                out.push(0);
                out.extend_from_slice(&attacker.0.to_le_bytes());
                out.extend_from_slice(&target.0.to_le_bytes());
                out.extend_from_slice(&amount.to_le_bytes());
                encode_pos(*attacker_pos, out);
                encode_vel(*attacker_vel, out);
                out.push(attacker_weapon.tag());
                match flight_ticks {
                    Some(ticks) => {
                        out.push(1);
                        out.extend_from_slice(&ticks.to_le_bytes());
                    }
                    None => out.extend_from_slice(&[0; 3]),
                }
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
                    encode_child(child, out);
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
            Self::BloomSeeded {
                director,
                bloom_index,
                site_pos,
                active_until,
                rocks,
            } => {
                out.push(8);
                out.extend_from_slice(&director.0.to_le_bytes());
                out.extend_from_slice(&bloom_index.to_le_bytes());
                encode_pos(*site_pos, out);
                out.extend_from_slice(&active_until.to_le_bytes());
                for rock in rocks.iter() {
                    encode_child(rock, out);
                }
            }
            Self::RockDestroyed { by, points } => {
                out.push(9);
                out.extend_from_slice(&by.0.to_le_bytes());
                out.push(*points);
            }
            Self::BloomPopulationChanged {
                director,
                bloom_index,
                delta,
            } => {
                out.push(10);
                out.extend_from_slice(&director.0.to_le_bytes());
                out.extend_from_slice(&bloom_index.to_le_bytes());
                out.push(delta.to_le_bytes()[0]);
            }
            Self::LockBroken {
                locker,
                target,
                reason,
            } => {
                out.push(11);
                out.extend_from_slice(&locker.0.to_le_bytes());
                out.extend_from_slice(&target.0.to_le_bytes());
                out.push(reason.tag());
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes
            .split_first()
            .ok_or(CodecError("regolith outcome: empty"))?;
        match (tag, rest.len()) {
            (0, 72) if rest[69] <= 1 => Ok(Self::DamageDealt {
                attacker: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                target: PersistId::new(u64::from_le_bytes(rest[8..16].try_into().unwrap())),
                amount: i32::from_le_bytes(rest[16..20].try_into().unwrap()),
                attacker_pos: decode_pos(&rest[20..44]),
                attacker_vel: decode_vel(&rest[44..68]),
                attacker_weapon: WeaponKind::from_tag(rest[68])?,
                flight_ticks: (rest[69] == 1)
                    .then(|| u16::from_le_bytes(rest[70..72].try_into().unwrap())),
            }),
            (1, 8) => Ok(Self::Destroyed {
                by: PersistId::new(u64::from_le_bytes(rest.try_into().unwrap())),
            }),
            (2, 152) => Ok(Self::Split {
                parent: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                generation: u32::from_le_bytes(rest[8..12].try_into().unwrap()),
                children: [decode_child(&rest[12..82])?, decode_child(&rest[82..152])?],
            }),
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
            (8, 744) => {
                let rocks: [Result<ChildSpec, CodecError>; 10] = core::array::from_fn(|slot| {
                    let offset = 44 + slot * 70;
                    decode_child(&rest[offset..offset + 70])
                });
                let rocks = rocks
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?
                    .try_into()
                    .map_err(|_| CodecError("regolith outcome: bloom rock count"))?;
                Ok(Self::BloomSeeded {
                    director: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                    bloom_index: u32::from_le_bytes(rest[8..12].try_into().unwrap()),
                    site_pos: decode_pos(&rest[12..36]),
                    active_until: u64::from_le_bytes(rest[36..44].try_into().unwrap()),
                    rocks: Box::new(rocks),
                })
            }
            (9, 9) => Ok(Self::RockDestroyed {
                by: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                points: rest[8],
            }),
            (10, 13) => Ok(Self::BloomPopulationChanged {
                director: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                bloom_index: u32::from_le_bytes(rest[8..12].try_into().unwrap()),
                delta: i8::from_le_bytes([rest[12]]),
            }),
            (11, 17) => Ok(Self::LockBroken {
                locker: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                target: PersistId::new(u64::from_le_bytes(rest[8..16].try_into().unwrap())),
                reason: LockBreakReason::from_tag(rest[16])?,
            }),
            _ => Err(CodecError("regolith outcome: bad tag or length")),
        }
    }
}
