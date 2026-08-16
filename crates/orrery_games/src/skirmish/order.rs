//! What a craft is told to do, and what its rules produced.
//!
//! Cross-entity effects travel **only** as events: an attacker's step emits
//! [`Outcome::DamageDealt`], and the harness turns that into the target's
//! [`Order::Damage`] on the next tick. That is what keeps each entity's window
//! replayable on its own — a witness re-executing an entity never needs any
//! other entity's live state, only what the log says arrived.
//!
//! # Neither type names its emitter, and that is not an oversight
//!
//! [`Outcome::Destroyed`] would obviously rather say *who* landed the last hit,
//! and [`Order::Damage`] would rather say who rolled it. A step cannot fill
//! either field: [`StateView`](orrery_core::StateView) hands a rule its own
//! state and its neighbours' but never its own
//! [`PersistId`](orrery_protocol::PersistId), so a rule literally cannot
//! attribute an event to itself. Attribution is therefore left to the layer
//! that does know — the executor knows which entity it is stepping — rather
//! than faked here. See the crate docs for the follow-on this suggests for
//! `orrery_core`.

use orrery_core::{CodecError, CoreCodec};
use orrery_protocol::PersistId;

/// One input to a core rule, in the authority's total order (VC-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    /// Accelerate along the current facing, then turn.
    ///
    /// The magnitude is what the *client asked for*; the rules clamp it to the
    /// archetype's ceiling. An honest client asks for something legal, so the
    /// clamp is normally inert — which is exactly why a cheat has to change
    /// the rules rather than the packet, and why an inflated request coming
    /// out of an honest build changes nothing at all.
    Thrust {
        /// Requested acceleration, millimetres per second squared.
        accel_mmss: i32,
        /// Yaw change applied after the thrust, micro-radians.
        yaw_urad: i32,
        /// Pitch change applied after the thrust, micro-radians.
        pitch_urad: i32,
    },
    /// Fire at another craft. Dropped by the rules if the weapon is on
    /// cooldown, or the target is absent, destroyed, or out of reach.
    Fire {
        /// Who is being shot at.
        target: PersistId,
    },
    /// Damage arriving from another craft's previous tick.
    Damage {
        /// Amount, already rolled by the attacker.
        amount: i32,
    },
}

impl CoreCodec for Order {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Order::Thrust {
                accel_mmss,
                yaw_urad,
                pitch_urad,
            } => {
                out.push(0);
                out.extend_from_slice(&accel_mmss.to_le_bytes());
                out.extend_from_slice(&yaw_urad.to_le_bytes());
                out.extend_from_slice(&pitch_urad.to_le_bytes());
            }
            Order::Fire { target } => {
                out.push(1);
                out.extend_from_slice(&target.0.to_le_bytes());
            }
            Order::Damage { amount } => {
                out.push(2);
                out.extend_from_slice(&amount.to_le_bytes());
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes.split_first().ok_or(CodecError("order: empty"))?;
        match (tag, rest.len()) {
            (0, 12) => Ok(Order::Thrust {
                accel_mmss: i32::from_le_bytes(rest[0..4].try_into().unwrap()),
                yaw_urad: i32::from_le_bytes(rest[4..8].try_into().unwrap()),
                pitch_urad: i32::from_le_bytes(rest[8..12].try_into().unwrap()),
            }),
            (1, 8) => Ok(Order::Fire {
                target: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
            }),
            (2, 4) => Ok(Order::Damage {
                amount: i32::from_le_bytes(rest[0..4].try_into().unwrap()),
            }),
            _ => Err(CodecError("order: bad tag or length")),
        }
    }
}

/// A deterministic outcome of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Damage the target must consume on its next tick.
    DamageDealt {
        /// Who takes it.
        target: PersistId,
        /// How much.
        amount: i32,
    },
    /// The emitting craft's hull reached zero, on the tick it did.
    ///
    /// It goes nowhere: kill credit, loot and the rest of a kill's *durable*
    /// consequences are attested intents (P5), never something a peer's own
    /// step awards itself. The event exists so the moment is in the log.
    Destroyed,
}

impl CoreCodec for Outcome {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Outcome::DamageDealt { target, amount } => {
                out.push(0);
                out.extend_from_slice(&target.0.to_le_bytes());
                out.extend_from_slice(&amount.to_le_bytes());
            }
            Outcome::Destroyed => out.push(1),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes.split_first().ok_or(CodecError("outcome: empty"))?;
        match (tag, rest.len()) {
            (0, 12) => Ok(Outcome::DamageDealt {
                target: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                amount: i32::from_le_bytes(rest[8..12].try_into().unwrap()),
            }),
            (1, 0) => Ok(Outcome::Destroyed),
            _ => Err(CodecError("outcome: bad tag or length")),
        }
    }
}
