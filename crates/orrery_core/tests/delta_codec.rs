use orrery_core::{
    state_hash, CodecError, CoreCodec, Executor, OrderedInputs, Quantized, Ruleset, StateView,
    StepOutput, TickRng,
};
use orrery_protocol::channels::{apply_delta_patch, encode_delta_patch};
use orrery_protocol::{PersistId, RulesetId, Tick, UniverseSeed};

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrowingState(Vec<u8>);

impl CoreCodec for GrowingState {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self(bytes.to_vec()))
    }
}

impl Quantized for GrowingState {
    fn quantize(&mut self) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NoInput;

impl CoreCodec for NoInput {
    fn encode(&self, _out: &mut Vec<u8>) {}

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        bytes
            .is_empty()
            .then_some(Self)
            .ok_or(CodecError("unexpected input bytes"))
    }
}

#[derive(Debug)]
struct NoEvent;

impl CoreCodec for NoEvent {
    fn encode(&self, _out: &mut Vec<u8>) {}

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        bytes
            .is_empty()
            .then_some(Self)
            .ok_or(CodecError("unexpected event bytes"))
    }
}

#[derive(Debug, Clone, Copy)]
struct GrowingRuleset;

impl Ruleset for GrowingRuleset {
    const OVERFLOW_IS_CANONICAL: bool = false;
    type CoreState = GrowingState;
    type CoreInput = NoInput;
    type CoreEvent = NoEvent;

    fn id(&self) -> RulesetId {
        RulesetId {
            version: 1,
            digest: [0x65; 32],
        }
    }

    fn step(
        &self,
        view: &mut StateView<'_, Self::CoreState>,
        _inputs: &OrderedInputs<'_, Self::CoreInput>,
        _rng: &mut TickRng,
    ) -> StepOutput<Self::CoreEvent> {
        let state = view.own_mut();
        let index = state.0.len();
        state
            .0
            .push(u8::try_from(index).unwrap_or(u8::MAX).wrapping_add(1));
        if let Some(first) = state.0.first_mut() {
            *first = first.wrapping_add(3);
        }
        StepOutput::default()
    }
}

#[test]
fn executor_generated_delta_preserves_state_hash_equality() {
    let entity = PersistId::new(7);
    let mut executor = Executor::new(GrowingRuleset, UniverseSeed([0x65; 32]));
    executor.insert(entity, GrowingState(vec![0x40; 134]));
    let keyframe = executor
        .state(entity)
        .expect("installed state")
        .to_canonical();

    for tick in 0..24 {
        executor
            .step_entity(entity, Tick::new(tick), &[])
            .expect("installed entity steps");
    }
    let current = executor.state(entity).expect("stepped state");
    let patch = encode_delta_patch(&keyframe, &current.to_canonical());
    let reconstructed = apply_delta_patch(&keyframe, &patch).expect("valid authored patch");
    let decoded = GrowingState::decode(&reconstructed).expect("canonical state decodes");

    assert_eq!(state_hash(&decoded), state_hash(current));
    assert_eq!(reconstructed, current.to_canonical());
}
