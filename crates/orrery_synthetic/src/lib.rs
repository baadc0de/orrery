//! The synthetic ruleset (#871): the smallest canonical rules a hit claim can
//! be adjudicated against.
//!
//! Hit registration is platform code, and #871 says so explicitly: build it
//! against a synthetic two-entity ruleset, not against Regolith and not
//! against the eventual FPS. Following `orrery_sim_host`'s `OffLatticeRuleset`
//! precedent, this is deliberately smaller than a game and implements only the
//! canonical contract — one integer of state, advanced by one per tick, which
//! is enough for a pose ring to be filled, looked up, and disagreed with.
//!
//! It is Bevy-free by construction and by gate: `orrery_sidecar` is what turns
//! these rules into a running app.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use orrery_core::{
    CodecError, CoreCodec, OrderedInputs, Quantized, Ruleset, StateView, StepOutput, TickRng,
};
use orrery_protocol::RulesetId;

/// The canonical state: one position on one axis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntheticState {
    /// Position along x, in lattice units.
    pub position_mm: i64,
}

impl Quantized for SyntheticState {
    fn quantize(&mut self) {}
}

impl CoreCodec for SyntheticState {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.position_mm.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let raw: [u8; 8] = bytes
            .try_into()
            .map_err(|_| CodecError("synthetic state is 8 bytes"))?;
        Ok(Self {
            position_mm: i64::from_le_bytes(raw),
        })
    }
}

/// The uninhabited input and event type: this ruleset takes neither.
#[derive(Clone)]
pub enum SyntheticNever {}

impl CoreCodec for SyntheticNever {
    fn encode(&self, _out: &mut Vec<u8>) {
        match *self {}
    }

    fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
        Err(CodecError("synthetic input/event is uninhabited"))
    }
}

/// The sidecar's ruleset: the entity advances one millimetre per tick.
pub struct Synthetic;

impl Ruleset for Synthetic {
    const OVERFLOW_IS_CANONICAL: bool = false;
    type CoreState = SyntheticState;
    type CoreInput = SyntheticNever;
    type CoreEvent = SyntheticNever;

    fn id(&self) -> RulesetId {
        RulesetId {
            version: 1,
            digest: [0x87; 32],
        }
    }

    fn step(
        &self,
        view: &mut StateView<'_, Self::CoreState>,
        _inputs: &OrderedInputs<'_, Self::CoreInput>,
        _rng: &mut TickRng,
    ) -> StepOutput<Self::CoreEvent> {
        view.own_mut().position_mm += 1;
        StepOutput::default()
    }
}
