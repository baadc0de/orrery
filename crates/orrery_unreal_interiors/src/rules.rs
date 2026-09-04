//! The throwaway nested-frame ruleset.
//!
//! Four kinds of body share one state shape, [`Body`]: a **station** (the
//! mothership fixture, G11.1 — it never moves), a **ship** (a frame under
//! way: velocity and angular rates at its root), a **mech** (a frame inside
//! the ship) and an **avatar** (a walker; never a frame). Every body names
//! the frame it steps in, [`Body::frame`], as the `PersistId` of the carrying
//! body or [`UNIVERSE`] for the root grid, and holds its pose in **that
//! frame's** millimetre lattice — D5's rule that the carrier's velocity lives
//! at the grid root and never in its contents (`docs/01-spatial-model.md`
//! §13, first paragraph).
//!
//! # The frame transform
//!
//! A frame's rotation is `R = Rz(yaw) · Rx(roll)`, yaw and roll in integer
//! micro-radians so the argument handed to `libm` is bit-identical on every
//! platform (VC-6; the same reasoning as `orrery_games::skirmish::state`).
//! Contents-to-parent is
//!
//! ```text
//! parent_pos = frame.pos + R · local_pos
//! parent_vel = frame.vel + R · local_vel          (§13.3 step 1, verbatim)
//! parent_yaw = frame.yaw + local_yaw
//! ```
//!
//! and parent-to-contents is the exact inverse, `R^T` applied to the
//! differences. Both are one `f64` pass over integer inputs, snapped back to
//! the lattice (VC-7). D5 says "integer cell math plus one f32 compose at the
//! leaf"; this ruleset has no cells, so the whole transform is the one
//! compose, in `f64`. **Teleport-class and continuous-class crossings are the
//! same code**: the teleport-class case (boarding a docked ship) is the one
//! where `frame.vel` and the relative velocity happen to be zero.
//!
//! # What a frame change is, to the host
//!
//! [`Intent::Enter`] and [`Intent::Leave`] read the frame body through
//! [`StateView::neighbor`] — a recorded read, so the executor's
//! `NeighborFrame` names the frame state the transform used — and rewrite the
//! entity's own `frame`, `pos`, `vel` and `yaw` in one step. The count of
//! changes and the tick of the last one are in the entity's own state, so the
//! discrete outcome is in its hash (the `shots` argument of
//! `skirmish/state.rs`). Nothing else changes: no second entity is written,
//! no record is appended to any log. Whether D47's per-entity snapshot set
//! captures the frame relation is therefore exactly the question of whether
//! `frame` plus the frame body's own snapshot is enough — and the C
//! consumer's `rollback` mode asks it hash for hash.

use orrery_core::{
    CodecError, CoreCodec, OrderedInputs, QPos, QVel, Quantized, Ruleset, StateView, StepOutput,
    TickRng, TICK_HZ,
};
use orrery_protocol::{PersistId, RulesetId};

/// The fixed tick duration in seconds (VC-1).
const DT: f64 = 1.0 / TICK_HZ as f64;

/// A full turn in micro-radians. Angles are kept in `[0, TAU_URAD)`.
pub const TAU_URAD: i32 = 6_283_185;

/// The root grid, `GridId` 0 in `docs/01-spatial-model.md` §13.1.
pub const UNIVERSE: u64 = 0;

/// This spike's rules identity. The digest is a fixed spike constant, not a
/// build-script digest: nothing adjudicates these rules, and a version bump
/// is the only thing a consumer checks before decoding.
pub const INTERIORS_RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0x45; 32],
};

/// What kind of body this is. The kind fixes which intents apply and whether
/// the body may carry others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// The mothership fixture. Never moves; carries ships and walkers.
    Station = 0,
    /// A ship: a frame under way. Carries mechs and walkers.
    Ship = 1,
    /// A mech: a frame inside a ship. Carries a walker.
    Mech = 2,
    /// A walker. Never a frame.
    Avatar = 3,
}

impl Kind {
    const fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0 => Ok(Self::Station),
            1 => Ok(Self::Ship),
            2 => Ok(Self::Mech),
            3 => Ok(Self::Avatar),
            _ => Err(CodecError("body: unknown kind")),
        }
    }

    /// Whether other bodies may step in this body's frame.
    #[must_use]
    pub const fn can_carry(self) -> bool {
        !matches!(self, Self::Avatar)
    }
}

/// One body's verifiable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    /// Which rules apply. Never changes.
    pub kind: Kind,
    /// The frame this body steps in: a carrying body's id, or [`UNIVERSE`].
    pub frame: u64,
    /// Position in the frame's millimetre lattice.
    pub pos: QPos,
    /// Velocity in the frame's lattice, mm/s.
    pub vel: QVel,
    /// Heading about the frame's vertical axis, micro-radians in `[0, TAU)`.
    pub yaw_urad: i32,
    /// Roll about the body's own forward axis, micro-radians in `[0, TAU)`.
    pub roll_urad: i32,
    /// Yaw rate, micro-radians per tick. Integer so a rotating frame stays on
    /// the lattice without a float accumulator (VC-5).
    pub yaw_rate_urad_tick: i32,
    /// Roll rate, micro-radians per tick.
    pub roll_rate_urad_tick: i32,
    /// Frame migrations performed, ever. The discrete trace of a crossing:
    /// a `StateView` does not expose the tick, so the trace is a count, and
    /// the tick of a change is what the consumer reads off the
    /// `FrameChanged` event beside `orrery_host_step`'s `out_first_tick`.
    pub frame_changes: u32,
}

/// The canonical encoding's byte length.
pub const BODY_ENCODED_LEN: usize = 1 + 8 + 24 + 24 + 4 + 4 + 4 + 4 + 4;

impl Body {
    /// A body at rest in `frame` at `pos`, facing `yaw_urad`.
    #[must_use]
    pub const fn at_rest(kind: Kind, frame: u64, pos: QPos, yaw_urad: i32) -> Self {
        Self {
            kind,
            frame,
            pos,
            vel: QVel { x: 0, y: 0, z: 0 },
            yaw_urad,
            roll_urad: 0,
            yaw_rate_urad_tick: 0,
            roll_rate_urad_tick: 0,
            frame_changes: 0,
        }
    }

    /// The frame's rotation, `Rz(yaw) · Rx(roll)`, as a row-major matrix.
    #[must_use]
    pub fn rotation(&self) -> [[f64; 3]; 3] {
        rotation(self.yaw_urad, self.roll_urad)
    }
}

/// `Rz(yaw) · Rx(roll)` over integer micro-radians.
#[must_use]
pub fn rotation(yaw_urad: i32, roll_urad: i32) -> [[f64; 3]; 3] {
    let yaw = f64::from(yaw_urad) * 1e-6;
    let roll = f64::from(roll_urad) * 1e-6;
    let (sy, cy) = (libm::sin(yaw), libm::cos(yaw));
    let (sr, cr) = (libm::sin(roll), libm::cos(roll));
    // Rz(yaw) = [[cy,-sy,0],[sy,cy,0],[0,0,1]]; Rx(roll) = [[1,0,0],[0,cr,-sr],[0,sr,cr]].
    [
        [cy, -sy * cr, sy * sr],
        [sy, cy * cr, -cy * sr],
        [0.0, sr, cr],
    ]
}

fn mul(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn mul_transposed(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ]
}

fn wrap_angle(urad: i64) -> i32 {
    // Exact: the rem is in [0, TAU) which fits an i32.
    urad.rem_euclid(i64::from(TAU_URAD)) as i32
}

/// Express `body` (in `frame`'s parent) in `frame`'s own lattice.
///
/// The exact inverse of [`to_parent`]: `R^T (pos − frame.pos)`, the same for
/// velocity, yaw differenced. Integer differences first, so the `f64` pass
/// sees the small relative numbers, never the 100 km absolute ones.
#[must_use]
pub fn to_local(body: &Body, frame: &Body) -> (QPos, QVel, i32) {
    let r = frame.rotation();
    let dp = QPos {
        x: body.pos.x.wrapping_sub(frame.pos.x),
        y: body.pos.y.wrapping_sub(frame.pos.y),
        z: body.pos.z.wrapping_sub(frame.pos.z),
    };
    let dv = QVel {
        x: body.vel.x.wrapping_sub(frame.vel.x),
        y: body.vel.y.wrapping_sub(frame.vel.y),
        z: body.vel.z.wrapping_sub(frame.vel.z),
    };
    let (px, py, pz) = dp.to_metres();
    let (vx, vy, vz) = dv.to_metres_per_sec();
    let p = mul_transposed(&r, [px, py, pz]);
    let v = mul_transposed(&r, [vx, vy, vz]);
    (
        QPos::from_metres(p[0], p[1], p[2]),
        QVel::from_metres_per_sec(v[0], v[1], v[2]),
        wrap_angle(i64::from(body.yaw_urad) - i64::from(frame.yaw_urad)),
    )
}

/// Express `body` (in `frame`'s lattice) in `frame`'s parent lattice.
#[must_use]
pub fn to_parent(body: &Body, frame: &Body) -> (QPos, QVel, i32) {
    let r = frame.rotation();
    let (px, py, pz) = body.pos.to_metres();
    let (vx, vy, vz) = body.vel.to_metres_per_sec();
    let p = mul(&r, [px, py, pz]);
    let v = mul(&r, [vx, vy, vz]);
    let rp = QPos::from_metres(p[0], p[1], p[2]);
    let rv = QVel::from_metres_per_sec(v[0], v[1], v[2]);
    (
        QPos {
            x: frame.pos.x.wrapping_add(rp.x),
            y: frame.pos.y.wrapping_add(rp.y),
            z: frame.pos.z.wrapping_add(rp.z),
        },
        QVel {
            x: frame.vel.x.wrapping_add(rv.x),
            y: frame.vel.y.wrapping_add(rv.y),
            z: frame.vel.z.wrapping_add(rv.z),
        },
        wrap_angle(i64::from(body.yaw_urad) + i64::from(frame.yaw_urad)),
    )
}

impl Quantized for Body {
    fn quantize(&mut self) {
        let (x, y, z) = self.pos.to_metres();
        self.pos = QPos::from_metres(x, y, z);
        let (vx, vy, vz) = self.vel.to_metres_per_sec();
        self.vel = QVel::from_metres_per_sec(vx, vy, vz);
        self.yaw_urad = wrap_angle(i64::from(self.yaw_urad));
        self.roll_urad = wrap_angle(i64::from(self.roll_urad));
    }
}

fn i64_at(bytes: &[u8], at: usize) -> i64 {
    let mut raw = [0; 8];
    raw.copy_from_slice(&bytes[at..at + 8]);
    i64::from_le_bytes(raw)
}

fn i32_at(bytes: &[u8], at: usize) -> i32 {
    let mut raw = [0; 4];
    raw.copy_from_slice(&bytes[at..at + 4]);
    i32::from_le_bytes(raw)
}

impl CoreCodec for Body {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.kind as u8);
        out.extend_from_slice(&self.frame.to_le_bytes());
        for v in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            self.yaw_urad,
            self.roll_urad,
            self.yaw_rate_urad_tick,
            self.roll_rate_urad_tick,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.frame_changes.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != BODY_ENCODED_LEN {
            return Err(CodecError("body: wrong length"));
        }
        Ok(Self {
            kind: Kind::from_tag(bytes[0])?,
            frame: i64_at(bytes, 1) as u64,
            pos: QPos {
                x: i64_at(bytes, 9),
                y: i64_at(bytes, 17),
                z: i64_at(bytes, 25),
            },
            vel: QVel {
                x: i64_at(bytes, 33),
                y: i64_at(bytes, 41),
                z: i64_at(bytes, 49),
            },
            yaw_urad: i32_at(bytes, 57),
            roll_urad: i32_at(bytes, 61),
            yaw_rate_urad_tick: i32_at(bytes, 65),
            roll_rate_urad_tick: i32_at(bytes, 69),
            frame_changes: i32_at(bytes, 73) as u32,
        })
    }
}

/// One input to a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A walker sets its frame-local velocity and heading. Kinematic: there
    /// is no acceleration model, the spike is about frames.
    Move {
        /// Frame-local velocity, mm/s.
        vel: QVel,
        /// Heading in the frame, micro-radians.
        yaw_urad: i32,
    },
    /// Enter a frame whose parent is this body's current frame.
    Enter {
        /// The carrying body to enter.
        frame: u64,
    },
    /// Leave the current frame for its parent.
    Leave,
    /// A ship sets its velocity and angular rates in its current frame.
    Cruise {
        /// Velocity, mm/s.
        vel: QVel,
        /// Yaw rate, micro-radians per tick.
        yaw_rate_urad_tick: i32,
        /// Roll rate, micro-radians per tick.
        roll_rate_urad_tick: i32,
    },
}

impl CoreCodec for Intent {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Move { vel, yaw_urad } => {
                out.push(0);
                for v in [vel.x, vel.y, vel.z] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(&yaw_urad.to_le_bytes());
            }
            Self::Enter { frame } => {
                out.push(1);
                out.extend_from_slice(&frame.to_le_bytes());
            }
            Self::Leave => out.push(2),
            Self::Cruise {
                vel,
                yaw_rate_urad_tick,
                roll_rate_urad_tick,
            } => {
                out.push(3);
                for v in [vel.x, vel.y, vel.z] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(&yaw_rate_urad_tick.to_le_bytes());
                out.extend_from_slice(&roll_rate_urad_tick.to_le_bytes());
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        match (bytes.first(), bytes.len()) {
            (Some(0), 29) => Ok(Self::Move {
                vel: QVel {
                    x: i64_at(bytes, 1),
                    y: i64_at(bytes, 9),
                    z: i64_at(bytes, 17),
                },
                yaw_urad: i32_at(bytes, 25),
            }),
            (Some(1), 9) => Ok(Self::Enter {
                frame: i64_at(bytes, 1) as u64,
            }),
            (Some(2), 1) => Ok(Self::Leave),
            (Some(3), 33) => Ok(Self::Cruise {
                vel: QVel {
                    x: i64_at(bytes, 1),
                    y: i64_at(bytes, 9),
                    z: i64_at(bytes, 17),
                },
                yaw_rate_urad_tick: i32_at(bytes, 25),
                roll_rate_urad_tick: i32_at(bytes, 29),
            }),
            _ => Err(CodecError("intent: unknown tag or wrong length")),
        }
    }
}

/// Why an intent was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Refusal {
    /// The intent does not apply to this kind of body.
    WrongKind = 0,
    /// The named frame is not a neighbour this tick, or cannot carry.
    NoSuchFrame = 1,
    /// The named frame's parent is not this body's frame (§13.4: interaction
    /// requires frame coincidence).
    NotCoincident = 2,
    /// Already in the root grid; nothing to leave to.
    AtRoot = 3,
}

/// A deterministic outcome event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Happening {
    /// The body migrated between frames on this tick.
    FrameChanged {
        /// Who migrated.
        entity: u64,
        /// The frame left.
        from: u64,
        /// The frame entered.
        to: u64,
    },
    /// An intent was refused.
    Refused {
        /// Whose intent.
        entity: u64,
        /// Why.
        reason: Refusal,
    },
}

impl CoreCodec for Happening {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::FrameChanged { entity, from, to } => {
                out.push(0);
                out.extend_from_slice(&entity.to_le_bytes());
                out.extend_from_slice(&from.to_le_bytes());
                out.extend_from_slice(&to.to_le_bytes());
            }
            Self::Refused { entity, reason } => {
                out.push(1);
                out.extend_from_slice(&entity.to_le_bytes());
                out.push(*reason as u8);
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        match (bytes.first(), bytes.len()) {
            (Some(0), 25) => Ok(Self::FrameChanged {
                entity: i64_at(bytes, 1) as u64,
                from: i64_at(bytes, 9) as u64,
                to: i64_at(bytes, 17) as u64,
            }),
            (Some(1), 10) => Ok(Self::Refused {
                entity: i64_at(bytes, 1) as u64,
                reason: match bytes[9] {
                    0 => Refusal::WrongKind,
                    1 => Refusal::NoSuchFrame,
                    2 => Refusal::NotCoincident,
                    3 => Refusal::AtRoot,
                    _ => return Err(CodecError("happening: unknown refusal")),
                },
            }),
            _ => Err(CodecError("happening: unknown tag or wrong length")),
        }
    }
}

/// The rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct Interiors;

impl Ruleset for Interiors {
    type CoreState = Body;
    type CoreInput = Intent;
    type CoreEvent = Happening;

    fn id(&self) -> RulesetId {
        INTERIORS_RULESET
    }

    /// One read per `Enter`/`Leave`; a tick with several is malformed, not
    /// merely expensive.
    fn max_neighbor_reads(&self) -> usize {
        4
    }

    /// Every body steps every tick, so a neighbour is at most one tick old
    /// once the population has stepped once. Installed state is observed at
    /// tick 0 (`orrery_host_install_state`'s "use 0 for a fresh spawn"), so
    /// a wider cap lets a frame be entered before it has stepped.
    fn max_neighbor_staleness_ticks(&self) -> u64 {
        u64::from(TICK_HZ)
    }

    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        inputs: &OrderedInputs<'_, Intent>,
        _rng: &mut TickRng,
    ) -> StepOutput<Happening> {
        let mut events = Vec::new();
        let me = view.entity();
        let mut next = view.own().clone();
        let kind = next.kind;

        for intent in inputs.iter() {
            match intent {
                Intent::Move { vel, yaw_urad } if matches!(kind, Kind::Mech | Kind::Avatar) => {
                    next.vel = *vel;
                    next.yaw_urad = wrap_angle(i64::from(*yaw_urad));
                }
                Intent::Cruise {
                    vel,
                    yaw_rate_urad_tick,
                    roll_rate_urad_tick,
                } if kind == Kind::Ship => {
                    next.vel = *vel;
                    next.yaw_rate_urad_tick = *yaw_rate_urad_tick;
                    next.roll_rate_urad_tick = *roll_rate_urad_tick;
                }
                Intent::Enter { frame } if kind != Kind::Station => {
                    let target = *frame;
                    match view.neighbor(PersistId::new(target)) {
                        Some(f) if f.kind.can_carry() && target != me.0 => {
                            if f.frame != next.frame {
                                events.push(Happening::Refused {
                                    entity: me.0,
                                    reason: Refusal::NotCoincident,
                                });
                                continue;
                            }
                            let from = next.frame;
                            let (pos, vel, yaw) = to_local(&next, f);
                            next.pos = pos;
                            next.vel = vel;
                            next.yaw_urad = yaw;
                            next.frame = target;
                            next.frame_changes = next.frame_changes.wrapping_add(1);
                            events.push(Happening::FrameChanged {
                                entity: me.0,
                                from,
                                to: target,
                            });
                        }
                        _ => events.push(Happening::Refused {
                            entity: me.0,
                            reason: Refusal::NoSuchFrame,
                        }),
                    }
                }
                Intent::Leave if kind != Kind::Station => {
                    if next.frame == UNIVERSE {
                        events.push(Happening::Refused {
                            entity: me.0,
                            reason: Refusal::AtRoot,
                        });
                        continue;
                    }
                    let from = next.frame;
                    match view.neighbor(PersistId::new(from)) {
                        Some(f) => {
                            let (pos, vel, yaw) = to_parent(&next, f);
                            next.pos = pos;
                            next.vel = vel;
                            next.yaw_urad = yaw;
                            next.frame = f.frame;
                            next.frame_changes = next.frame_changes.wrapping_add(1);
                            events.push(Happening::FrameChanged {
                                entity: me.0,
                                from,
                                to: f.frame,
                            });
                        }
                        None => events.push(Happening::Refused {
                            entity: me.0,
                            reason: Refusal::NoSuchFrame,
                        }),
                    }
                }
                _ => events.push(Happening::Refused {
                    entity: me.0,
                    reason: Refusal::WrongKind,
                }),
            }
        }

        if kind != Kind::Station {
            // Integrate in the body's own frame. Exact reads off the lattice,
            // one f64 pass, snapped back (VC-7).
            let (mut px, mut py, mut pz) = next.pos.to_metres();
            let (vx, vy, vz) = next.vel.to_metres_per_sec();
            // Plain multiply-then-add, as `skirmish/mod.rs:522-524`: a fused
            // multiply-add rounds differently per platform, which is the
            // drift VC-6's bands exist to absorb, not to invite.
            px += vx * DT;
            py += vy * DT;
            pz += vz * DT;
            next.pos = QPos::from_metres(px, py, pz);
            next.yaw_urad =
                wrap_angle(i64::from(next.yaw_urad) + i64::from(next.yaw_rate_urad_tick));
            next.roll_urad =
                wrap_angle(i64::from(next.roll_urad) + i64::from(next.roll_rate_urad_tick));
        }

        *view.own_mut() = next;
        StepOutput { events }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(kind: Kind, frame: u64, x: i64, y: i64, yaw: i32) -> Body {
        Body::at_rest(kind, frame, QPos { x, y, z: 0 }, yaw)
    }

    #[test]
    fn body_codec_round_trips() {
        let mut b = body(Kind::Ship, 1, 100_000_000, -5, 123_456);
        b.vel = QVel {
            x: 500_000,
            y: 0,
            z: 7,
        };
        b.roll_urad = 99;
        b.roll_rate_urad_tick = 1454;
        b.frame_changes = 3;
        let bytes = b.to_canonical();
        assert_eq!(bytes.len(), BODY_ENCODED_LEN);
        assert_eq!(Body::decode(&bytes).expect("decodes"), b);
    }

    #[test]
    fn intent_codec_round_trips() {
        for intent in [
            Intent::Move {
                vel: QVel {
                    x: 2000,
                    y: -1,
                    z: 0,
                },
                yaw_urad: 7,
            },
            Intent::Enter { frame: 2 },
            Intent::Leave,
            Intent::Cruise {
                vel: QVel {
                    x: 0,
                    y: 500_000,
                    z: 0,
                },
                yaw_rate_urad_tick: 0,
                roll_rate_urad_tick: 1454,
            },
        ] {
            assert_eq!(
                Intent::decode(&intent.to_canonical()).expect("decodes"),
                intent
            );
        }
    }

    #[test]
    fn to_local_then_to_parent_round_trips_within_one_quantum_at_100_km() {
        // A rolled, yawed ship 100 km from the origin; a walker 12.345 m
        // along it. A rotated lattice is not a lattice, so the round trip is
        // exact to one quantum (1 mm), never worse: the f64 pass sees only
        // the relative numbers, and the 100 km is integer arithmetic.
        let mut ship = body(Kind::Ship, UNIVERSE, 100_000_000, 25_000, 1_234_567);
        ship.roll_urad = 654_321;
        ship.vel = QVel {
            x: 500_000,
            y: 0,
            z: 0,
        };
        let mut walker = body(Kind::Avatar, UNIVERSE, 100_012_345, 25_678, 0);
        walker.vel = QVel {
            x: 501_000,
            y: 2000,
            z: 0,
        };
        let (lp, lv, ly) = to_local(&walker, &ship);
        let mut local = walker.clone();
        local.pos = lp;
        local.vel = lv;
        local.yaw_urad = ly;
        let (pp, pv, py) = to_parent(&local, &ship);
        for (a, b) in [
            (pp.x, walker.pos.x),
            (pp.y, walker.pos.y),
            (pp.z, walker.pos.z),
            (pv.x, walker.vel.x),
            (pv.y, walker.vel.y),
            (pv.z, walker.vel.z),
        ] {
            assert!((a - b).abs() <= 1, "{a} vs {b}");
        }
        assert_eq!(py, walker.yaw_urad);
    }
}
