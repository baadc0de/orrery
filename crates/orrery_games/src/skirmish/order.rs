//! What a craft is told to do, and what its rules produced.
//!
//! Cross-entity effects travel **only** as events: an attacker's step emits
//! [`Outcome::DamageDealt`], and the harness turns that into the target's
//! [`Order::Damage`] on the next tick. That is what keeps each entity's window
//! replayable on its own — a witness re-executing an entity never needs any
//! other entity's live state, only what the log says arrived.
//!
//! # An effect carries what resolving it needs
//!
//! The adjudicator installs exactly one entity
//! ([`ReplayHarness::load_claimed_snapshot`](orrery_core::ReplayHarness::load_claimed_snapshot)),
//! so a step that consulted a neighbour to decide whether a shot connects
//! would decide differently under replay than it did under play — and an
//! honest craft would hash-mismatch. So the attacker's step emits the shot
//! unconditionally and *whether it connects is the target's own question*,
//! answered from its own position against what the event carries.
//!
//! What the event carries is chosen so that a cheat cannot widen its own
//! reach. [`Outcome::DamageDealt`] names the attacker's **archetype**, not a
//! reach in millimetres: an archetype is adjudicated own state — the
//! `skirmish/value-range` invariant refuses a craft that relabels itself, and
//! `Craft::archetype` is in the hashed encoding — whereas a raw scalar is not
//! compared against anything. `crate::golden` says why that matters: the
//! chains cover state hashes only, and nothing regenerates events and
//! compares them to logged records, so an inflated scalar would travel
//! unchallenged.
//!
//! # Every effect names who caused it
//!
//! [`Outcome::DamageDealt`] carries its attacker and [`Outcome::Destroyed`]
//! carries the craft that landed the last hit, because
//! [`StateView::entity`](orrery_core::StateView::entity) tells a step which
//! entity it is. The identity comes from the executor rather than from the
//! state, so a rule cannot claim to be an entity it is not.
//!
//! This matters past flavour. A kill's *durable* consequences — credit, loot,
//! the ledger rows a P5 intent writes — attach to an account, and an
//! adjudicator asked to settle a disputed kill needs the log to say whose
//! window to re-execute. An anonymous effect is one nothing downstream can
//! act on.

use orrery_core::{CodecError, CoreCodec, QPos};
use orrery_protocol::PersistId;

use super::archetype::Archetype;

/// A lattice position's byte length, three little-endian `i64`.
const POS_ENCODED_LEN: usize = 24;

/// Append a lattice position in the same fixed field order [`Craft`] uses.
///
/// [`Craft`]: super::state::Craft
fn encode_pos(pos: QPos, out: &mut Vec<u8>) {
    for value in [pos.x, pos.y, pos.z] {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// Read a lattice position back. The slice length is checked by the caller's
/// arm, which is what makes the `try_into` here infallible.
fn decode_pos(bytes: &[u8]) -> QPos {
    debug_assert_eq!(bytes.len(), POS_ENCODED_LEN);
    let at = |o: usize| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    QPos {
        x: at(0),
        y: at(8),
        z: at(16),
    }
}

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
    /// Fire at another craft. Dropped by the rules only if the weapon is on
    /// cooldown or the craft is a wreck; whether the shot *connects* is
    /// resolved in the target's step, not here.
    Fire {
        /// Who is being shot at.
        target: PersistId,
    },
    /// Damage arriving from another craft's previous tick.
    ///
    /// Carries where it was fired from and what fired it, because reach is
    /// resolved here — see the module documentation.
    Damage {
        /// Amount, already rolled by the attacker.
        amount: i32,
        /// Who rolled it.
        from: PersistId,
        /// Where the attacker was when it rolled, on the lattice.
        from_pos: QPos,
        /// What the attacker was flying. The reach this shot gets is
        /// `from_archetype.limits().range_mm` and nothing the attacker says.
        from_archetype: Archetype,
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
            Order::Damage {
                amount,
                from,
                from_pos,
                from_archetype,
            } => {
                out.push(2);
                out.extend_from_slice(&amount.to_le_bytes());
                out.extend_from_slice(&from.0.to_le_bytes());
                encode_pos(*from_pos, out);
                out.push(from_archetype.tag());
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
            (2, 37) => Ok(Order::Damage {
                amount: i32::from_le_bytes(rest[0..4].try_into().unwrap()),
                from: PersistId::new(u64::from_le_bytes(rest[4..12].try_into().unwrap())),
                from_pos: decode_pos(&rest[12..36]),
                from_archetype: Archetype::from_tag(rest[36])?,
            }),
            _ => Err(CodecError("order: bad tag or length")),
        }
    }
}

/// A deterministic outcome of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Damage the target must consume on its next tick — *if* the target
    /// finds itself in reach of where this was fired from.
    DamageDealt {
        /// Who rolled it — [`StateView::entity`](orrery_core::StateView::entity),
        /// so it is the executor's answer and not the rule's.
        attacker: PersistId,
        /// Who it is aimed at.
        target: PersistId,
        /// How much.
        amount: i32,
        /// Where the attacker was, on the lattice, at the top of its tick.
        attacker_pos: QPos,
        /// What the attacker was flying, which is what its reach is derived
        /// from on the receiving side.
        attacker_archetype: Archetype,
    },
    /// The emitting craft's hull reached zero, on the tick it did.
    ///
    /// Naming the killer is not the same as awarding the kill: credit, loot
    /// and the rest of a kill's *durable* consequences are attested intents
    /// (P5), never something a peer's own step grants. This is the evidence
    /// such an intent would be adjudicated against.
    Destroyed {
        /// Who landed the last hit.
        by: PersistId,
    },
}

impl CoreCodec for Outcome {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Outcome::DamageDealt {
                attacker,
                target,
                amount,
                attacker_pos,
                attacker_archetype,
            } => {
                out.push(0);
                out.extend_from_slice(&attacker.0.to_le_bytes());
                out.extend_from_slice(&target.0.to_le_bytes());
                out.extend_from_slice(&amount.to_le_bytes());
                encode_pos(*attacker_pos, out);
                out.push(attacker_archetype.tag());
            }
            Outcome::Destroyed { by } => {
                out.push(1);
                out.extend_from_slice(&by.0.to_le_bytes());
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (tag, rest) = bytes.split_first().ok_or(CodecError("outcome: empty"))?;
        match (tag, rest.len()) {
            (0, 45) => Ok(Outcome::DamageDealt {
                attacker: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                target: PersistId::new(u64::from_le_bytes(rest[8..16].try_into().unwrap())),
                amount: i32::from_le_bytes(rest[16..20].try_into().unwrap()),
                attacker_pos: decode_pos(&rest[20..44]),
                attacker_archetype: Archetype::from_tag(rest[44])?,
            }),
            (1, 8) => Ok(Outcome::Destroyed {
                by: PersistId::new(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
            }),
            _ => Err(CodecError("outcome: bad tag or length")),
        }
    }
}
