use orrery_core::{
    CodecError, CoreCodec, EntityMaterialization, OrderedInputs, QPos, QVel, Quantized, Ruleset,
    StateView, StepOutput, TickRng,
};
use orrery_games::game::{Game, GameMeta, Tamper};
use orrery_games::scenario::{adjudicate, adjudicate_isolated, play, Scenario, T0};
use orrery_protocol::{PersistId, RulesetId, Tick};

const RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0xA4; 32],
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    ticks: u64,
    has_materialized: bool,
}

impl Quantized for State {
    fn quantize(&mut self) {}
}

impl CoreCodec for State {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ticks.to_le_bytes());
        out.push(u8::from(self.has_materialized));
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 9 || bytes[8] > 1 {
            return Err(CodecError("materialization state is nine canonical bytes"));
        }
        Ok(Self {
            ticks: u64::from_le_bytes(bytes[..8].try_into().expect("length checked")),
            has_materialized: bytes[8] == 1,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Input;

impl CoreCodec for Input {
    fn encode(&self, _out: &mut Vec<u8>) {}

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.is_empty() {
            Ok(Self)
        } else {
            Err(CodecError("materialization input is empty"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Spawn {
    entity: PersistId,
    state: State,
}

impl CoreCodec for Spawn {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.entity.0.to_le_bytes());
        self.state.encode(out);
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 17 {
            return Err(CodecError("materialization event is seventeen bytes"));
        }
        Ok(Self {
            entity: PersistId::new(u64::from_le_bytes(
                bytes[..8].try_into().expect("length checked"),
            )),
            state: State::decode(&bytes[8..])?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Growing;

impl Ruleset for Growing {
    type CoreState = State;
    type CoreInput = Input;
    type CoreEvent = Spawn;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn step(
        &self,
        view: &mut StateView<'_, State>,
        _inputs: &OrderedInputs<'_, Input>,
        _rng: &mut TickRng,
    ) -> StepOutput<Spawn> {
        let entity = view.entity();
        let state = view.own_mut();
        state.ticks += 1;
        if state.has_materialized {
            return StepOutput::default();
        }
        state.has_materialized = true;
        StepOutput {
            events: vec![Spawn {
                entity: child_id(entity, 0),
                state: State {
                    ticks: 0,
                    has_materialized: true,
                },
            }],
        }
    }

    fn materialize(&self, event: &Spawn, out: &mut Vec<EntityMaterialization<State>>) {
        out.push(EntityMaterialization::new(
            event.entity,
            event.state.clone(),
        ));
    }
}

impl Game for Growing {
    const META: GameMeta = GameMeta {
        name: "materialization-fixture",
        summary: "a source creates one autonomous child",
        ruleset: RULESET,
    };

    fn honest() -> Self {
        Self
    }

    fn tampered(_tamper: Tamper) -> Option<Self> {
        None
    }

    fn spawn(&self, _entity: PersistId, _slot: u64) -> State {
        State {
            ticks: 0,
            has_materialized: false,
        }
    }

    fn honest_inputs(
        &self,
        _entity: PersistId,
        _slot: u64,
        _tick: Tick,
        _peers: &[PersistId],
        _rng: &mut TickRng,
        _out: &mut Vec<Input>,
    ) {
    }

    fn deliver(&self, _event: &Spawn) -> Option<(PersistId, Input)> {
        None
    }

    fn trajectory(_state: &State) -> (QPos, QVel) {
        (QPos::default(), QVel::default())
    }
}

fn child_id(parent: PersistId, slot: u64) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"orrery-games-materialization-test");
    hasher.update(&parent.0.to_le_bytes());
    hasher.update(&slot.to_le_bytes());
    PersistId::new(u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("a digest has eight bytes"),
    ))
}

#[test]
fn scenario_steps_materialized_entities_on_next_tick_and_replays_them_in_isolation() {
    let scenario = Scenario {
        name: "materialization",
        entities: 1,
        world_entities: 0,
        ticks: 3,
        seed_byte: 0xA4,
        sample_loss_pct: 0,
    };
    let child = child_id(PersistId::new(1), 0);
    let played = play(Growing, &scenario);

    assert_eq!(played.events, 1);
    assert_eq!(played.log[0].tick, Tick::new(T0));
    assert_eq!(played.log[0].entries.len(), 1);
    assert_eq!(played.log[1].entries.len(), 2);
    assert_eq!(played.log[2].entries.len(), 2);
    let child_ticks: Vec<u64> = played
        .log
        .iter()
        .flat_map(|record| &record.entries)
        .filter(|entry| entry.entity == child)
        .map(|entry| entry.state.ticks)
        .collect();
    assert_eq!(child_ticks, [1, 2]);
    assert_eq!(adjudicate(Growing, &scenario, &played), None);
    assert_eq!(adjudicate_isolated(|| Growing, &scenario, &played), None);
}
