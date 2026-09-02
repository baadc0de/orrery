//! A synthetic reference ruleset for exercising the host and its C ABI.
//!
//! This lives under `tests/support`, not in the library, on purpose: a
//! `Ruleset` impl in `src/` would make `core-gates.sh` discover
//! `orrery_sim_host` as a canonical rules crate, which it is not — it is the
//! host around one.  The C ABI test, the rewind test and the reference
//! `cdylib` example each include this file by path.
//!
//! This is not a game and names nothing from one.  It exists so that the
//! generic seam can be driven end to end — install, command, step, event
//! routing through an adapter, neighbour reads under a staleness bound,
//! quantization, snapshot and restore — by tests and by a foreign caller,
//! without a game's types anywhere in the path.  A ruleset that carries no
//! neighbour read, no adapter delivery and no off-lattice quantization could
//! pass a rewind test while leaving every interesting path unproven.
//!
//! The state is deliberately shaped like something a first-person game would
//! carry (a position, a velocity, hit points, a target) so the flat encoding a
//! C++ consumer mirrors is a realistic one, but every field is generic.

#![allow(dead_code)]

use orrery_core::{
    CodecError, CoreCodec, OrderedInputs, Quantized, Ruleset, StateSection, StateView, StepOutput,
    TickRng,
};
use orrery_protocol::{PersistId, RulesetId};

use orrery_sim_host::{Delivery, RulesetAdapter};

/// Micrometres per millimetre: positions live in micrometres and quantize to
/// the millimetre lattice, so a velocity that is not a multiple of 1 000 µm
/// per tick moves the state off-lattice every tick and VC-7 snaps it back.
pub const MICROMETRES_PER_MILLIMETRE: i64 = 1_000;

/// The ruleset identity every [`Synthetic`] host reports.
pub const SYNTHETIC_RULESET_ID: RulesetId = RulesetId {
    version: 1,
    digest: [0x5A; 32],
};

/// How stale, in ticks, a neighbour observation may be and still be read.
pub const SYNTHETIC_NEIGHBOR_STALENESS_TICKS: u64 = 2;

const STATE_BYTES: usize = 3 * 8 + 3 * 8 + 4 + 8 + 4;

/// One entity's verifiable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntheticState {
    /// Position in micrometres, quantized to the millimetre lattice at every
    /// tick boundary.
    pub position_um: [i64; 3],
    /// Velocity in micrometres per tick.  Not quantized.
    pub velocity_um_per_tick: [i64; 3],
    /// Hit points; [`SyntheticInput::Damage`] reduces them, saturating at zero.
    pub health: i32,
    /// The neighbour this entity watches, or zero for none.
    pub target: u64,
    /// How many ticks the target was readable under the staleness bound.
    pub sightings: u32,
}

impl Quantized for SyntheticState {
    fn quantize(&mut self) {
        for axis in &mut self.position_um {
            let magnitude = (axis.abs() + MICROMETRES_PER_MILLIMETRE / 2)
                / MICROMETRES_PER_MILLIMETRE
                * MICROMETRES_PER_MILLIMETRE;
            *axis = magnitude * axis.signum();
        }
    }
}

impl CoreCodec for SyntheticState {
    fn encode(&self, out: &mut Vec<u8>) {
        for axis in self.position_um {
            out.extend_from_slice(&axis.to_le_bytes());
        }
        for axis in self.velocity_um_per_tick {
            out.extend_from_slice(&axis.to_le_bytes());
        }
        out.extend_from_slice(&self.health.to_le_bytes());
        out.extend_from_slice(&self.target.to_le_bytes());
        out.extend_from_slice(&self.sightings.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != STATE_BYTES {
            return Err(CodecError("synthetic state is 64 bytes"));
        }
        let mut reader = Reader { bytes, at: 0 };
        let position_um = [reader.i64()?, reader.i64()?, reader.i64()?];
        let velocity_um_per_tick = [reader.i64()?, reader.i64()?, reader.i64()?];
        let health = reader.i32()?;
        let target = reader.u64()?;
        let sightings = reader.u32()?;
        Ok(Self {
            position_um,
            velocity_um_per_tick,
            health,
            target,
            sightings,
        })
    }
}

/// One command to an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticInput {
    /// Add to the velocity, in micrometres per tick.
    Impulse([i64; 3]),
    /// Reduce hit points.
    Damage(i32),
    /// A deliberately faulty input whose step panics.
    ///
    /// The kernel forbids panics in rules; this one exists so the ABI's
    /// boundary can be shown to turn an unwind into an error code from a
    /// real foreign caller, which no well-behaved ruleset lets a test do.
    Poison,
}

const INPUT_IMPULSE: u8 = 1;
const INPUT_DAMAGE: u8 = 2;
const INPUT_POISON: u8 = 3;

impl CoreCodec for SyntheticInput {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Impulse(delta) => {
                out.push(INPUT_IMPULSE);
                for axis in delta {
                    out.extend_from_slice(&axis.to_le_bytes());
                }
            }
            Self::Damage(amount) => {
                out.push(INPUT_DAMAGE);
                out.extend_from_slice(&amount.to_le_bytes());
            }
            Self::Poison => out.push(INPUT_POISON),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (&tag, rest) = bytes
            .split_first()
            .ok_or(CodecError("synthetic input needs a tag"))?;
        let mut reader = Reader { bytes: rest, at: 0 };
        let input = match tag {
            INPUT_IMPULSE => Self::Impulse([reader.i64()?, reader.i64()?, reader.i64()?]),
            INPUT_DAMAGE => Self::Damage(reader.i32()?),
            INPUT_POISON => Self::Poison,
            _ => return Err(CodecError("unknown synthetic input tag")),
        };
        if reader.at != rest.len() {
            return Err(CodecError("trailing synthetic input bytes"));
        }
        Ok(input)
    }
}

/// One emitted outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticEvent {
    /// The watcher saw its target this tick and struck it; the adapter turns
    /// this into a [`SyntheticInput::Damage`] delivered to `target` next tick.
    Struck {
        /// Who was struck.
        target: PersistId,
        /// How hard.
        damage: i32,
    },
}

const EVENT_STRUCK: u8 = 1;

impl CoreCodec for SyntheticEvent {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Struck { target, damage } => {
                out.push(EVENT_STRUCK);
                out.extend_from_slice(&target.0.to_le_bytes());
                out.extend_from_slice(&damage.to_le_bytes());
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (&tag, rest) = bytes
            .split_first()
            .ok_or(CodecError("synthetic event needs a tag"))?;
        let mut reader = Reader { bytes: rest, at: 0 };
        let event = match tag {
            EVENT_STRUCK => Self::Struck {
                target: PersistId::new(reader.u64()?),
                damage: reader.i32()?,
            },
            _ => return Err(CodecError("unknown synthetic event tag")),
        };
        if reader.at != rest.len() {
            return Err(CodecError("trailing synthetic event bytes"));
        }
        Ok(event)
    }
}

/// The synthetic rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct Synthetic;

impl Ruleset for Synthetic {
    type CoreState = SyntheticState;
    type CoreInput = SyntheticInput;
    type CoreEvent = SyntheticEvent;

    fn id(&self) -> RulesetId {
        SYNTHETIC_RULESET_ID
    }

    fn max_neighbor_reads(&self) -> usize {
        1
    }

    fn max_neighbor_staleness_ticks(&self) -> u64 {
        SYNTHETIC_NEIGHBOR_STALENESS_TICKS
    }

    fn step(
        &self,
        view: &mut StateView<'_, Self::CoreState>,
        inputs: &OrderedInputs<'_, Self::CoreInput>,
        _rng: &mut TickRng,
    ) -> StepOutput<Self::CoreEvent> {
        let mut output = StepOutput::default();
        for input in inputs.iter() {
            match *input {
                SyntheticInput::Impulse(delta) => {
                    let own = view.own_mut();
                    for (axis, change) in own.velocity_um_per_tick.iter_mut().zip(delta) {
                        *axis = axis.saturating_add(change);
                    }
                }
                SyntheticInput::Damage(amount) => {
                    let own = view.own_mut();
                    own.health = own.health.saturating_sub(amount).max(0);
                }
                SyntheticInput::Poison => panic!("synthetic poison input: the boundary probe"),
            }
        }

        let target = view.own().target;
        let target = (target != 0).then(|| PersistId::new(target));
        let sighted = target.is_some_and(|target| view.neighbor(target).is_some());

        let own = view.own_mut();
        for (axis, velocity) in own.position_um.iter_mut().zip(own.velocity_um_per_tick) {
            *axis = axis.saturating_add(velocity);
        }
        if sighted {
            own.sightings = own.sightings.saturating_add(1);
            if let Some(target) = target {
                output
                    .events
                    .push(SyntheticEvent::Struck { target, damage: 1 });
            }
        }
        output
    }
}

/// Routes [`SyntheticEvent::Struck`] to its target as next-tick damage.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyntheticAdapter;

impl RulesetAdapter<Synthetic> for SyntheticAdapter {
    fn deliver(&self, event: &SyntheticEvent) -> Option<Delivery<SyntheticInput>> {
        match *event {
            SyntheticEvent::Struck { target, damage } => {
                Some(Delivery::new(target, SyntheticInput::Damage(damage)))
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let end = self
            .at
            .checked_add(N)
            .ok_or(CodecError("synthetic record overflows"))?;
        let raw: [u8; N] = self
            .bytes
            .get(self.at..end)
            .ok_or(CodecError("synthetic record truncated"))?
            .try_into()
            .map_err(|_| CodecError("synthetic record truncated"))?;
        self.at = end;
        Ok(raw)
    }

    fn i64(&mut self) -> Result<i64, CodecError> {
        self.take().map(i64::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        self.take().map(u64::from_le_bytes)
    }

    fn i32(&mut self) -> Result<i32, CodecError> {
        self.take().map(i32::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        self.take().map(u32::from_le_bytes)
    }
}

/// The section every synthetic entity occupies: the state has one shape, so
/// a decomposing host files it as one component.
pub const SYNTHETIC_SECTION: StateSection = StateSection("synthetic");

impl orrery_core::Sectioned for SyntheticState {
    const MIGRATED_SECTIONS: &'static [StateSection] = &[];

    fn section(&self) -> StateSection {
        SYNTHETIC_SECTION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_input_and_event_round_trip() {
        let state = SyntheticState {
            position_um: [1_000, -2_000, 3_000],
            velocity_um_per_tick: [1_234, -5, 0],
            health: 90,
            target: 7,
            sightings: 3,
        };
        assert_eq!(
            SyntheticState::decode(&state.to_canonical()),
            Ok(state),
            "state round-trips"
        );
        for input in [
            SyntheticInput::Impulse([1, -2, 3]),
            SyntheticInput::Damage(4),
            SyntheticInput::Poison,
        ] {
            assert_eq!(SyntheticInput::decode(&input.to_canonical()), Ok(input));
        }
        let event = SyntheticEvent::Struck {
            target: PersistId::new(9),
            damage: 2,
        };
        assert_eq!(SyntheticEvent::decode(&event.to_canonical()), Ok(event));
        assert!(SyntheticState::decode(&[0; STATE_BYTES - 1]).is_err());
        assert!(SyntheticInput::decode(&[INPUT_DAMAGE, 1]).is_err());
    }

    #[test]
    fn quantize_snaps_to_the_millimetre_lattice_and_is_idempotent() {
        let mut state = SyntheticState {
            position_um: [1_499, -1_500, 2_501],
            ..SyntheticState::default()
        };
        state.quantize();
        assert_eq!(state.position_um, [1_000, -2_000, 3_000]);
        let once = state;
        state.quantize();
        assert_eq!(state, once);
    }
}
