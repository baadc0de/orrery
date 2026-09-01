//! Synthetic rules used by facade integration tests.
//!
//! This follows `orrery_sim_host`'s `OffLatticeRuleset` precedent: the fixture
//! is deliberately smaller than a game and implements only the canonical
//! contract the facade's generic witness plugin requires.

use orrery_core::{
    CodecError, CoreCodec, OrderedInputs, Quantized, Ruleset, StateView, StepOutput, TickRng,
};
use orrery_protocol::RulesetId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntheticState {
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

pub struct Synthetic;

impl Ruleset for Synthetic {
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
