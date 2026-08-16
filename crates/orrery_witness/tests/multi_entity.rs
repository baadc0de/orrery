//! Frames that carry more than one entity — the shape the whole log format is
//! built around, and the one nothing else here covers.
//!
//! One signature per *frame* covers every entity an authority authored in that
//! send (docs/06 §6); that is the reason a sender's signing cost is flat in the
//! number of entities rather than linear in records. So a witness watching
//! several of a subject's entities — which is what a cell-epoch witness set
//! does — sees all of them in one frame, and a repair for one of them arrives
//! carrying the others.
//!
//! Every fixture in the other two test files authors exactly one entity per
//! frame. That is a comfortable world where a great deal can be wrong without
//! anything failing: an engine can silently witness only the first entity it
//! finds, and a repair path can mis-thread sibling heads, and both look
//! perfectly healthy. These tests are the two-entity world.

use orrery_core::log::{claim_hash, sign_claim, sign_frame, HeadTransition};
use orrery_core::store::AuthorityLog;
use orrery_core::{
    state_hash, CodecError, CoreCodec, Executor, OrderedInputs, QPos, QVel, Quantized, Ruleset,
    StateView, StepOutput, TickRng,
};
use orrery_protocol::{
    ChainHash, EntitySlice, FrameHead, InputRecord, LogFrame, LogRangeRequest, PersistId,
    RecordSource, RulesetId, StateClaim, Tick, UniverseSeed,
};
use orrery_witness::{Watch, Witness, WitnessConfig, WitnessSignal};

// ── A ruleset small enough to keep this about the frame format ────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    pos: QPos,
    vel: QVel,
}

impl CoreCodec for Body {
    fn encode(&self, out: &mut Vec<u8>) {
        for value in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 48 {
            return Err(CodecError("body is 48 bytes"));
        }
        let read = |i: usize| i64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Ok(Self {
            pos: QPos {
                x: read(0),
                y: read(1),
                z: read(2),
            },
            vel: QVel {
                x: read(3),
                y: read(4),
                z: read(5),
            },
        })
    }
}

impl Quantized for Body {
    fn quantize(&mut self) {}
}

#[derive(Debug, Clone, Copy)]
struct Move(i64);

impl CoreCodec for Move {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self(i64::from_le_bytes(
            bytes.try_into().map_err(|_| CodecError("8 bytes"))?,
        )))
    }
}

struct Nothing;

impl CoreCodec for Nothing {
    fn encode(&self, _out: &mut Vec<u8>) {}
    fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self)
    }
}

struct Kinematic;

const RULESET: RulesetId = RulesetId {
    version: 7,
    digest: [0x7E; 32],
};

impl Ruleset for Kinematic {
    type CoreState = Body;
    type CoreInput = Move;
    type CoreEvent = Nothing;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        inputs: &OrderedInputs<'_, Move>,
        _rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        let mut requested = 0i64;
        for input in inputs.iter() {
            requested += input.0;
        }
        let state = view.own_mut();
        state.vel.x = requested;
        state.pos.x += state.vel.x;
        StepOutput::default()
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Two entities authored by one key. `A` sorts before `B`, so `A` is the one a
/// "find the first watched entity" implementation would pick.
const A: PersistId = PersistId::new(1);
const B: PersistId = PersistId::new(2);
const SEED: UniverseSeed = UniverseSeed([0x77; 32]);
const T0: u64 = 3_000;
const STEP: i64 = 40;

fn subject_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[21; 32])
}

fn body() -> Body {
    Body {
        pos: QPos::default(),
        vel: QVel::default(),
    }
}

/// An authority authoring both entities in every frame, under one signature.
struct Authority {
    executor: Executor<Kinematic>,
    heads: std::collections::BTreeMap<PersistId, ChainHash>,
    previous_claim: std::collections::BTreeMap<PersistId, [u8; 32]>,
    /// Entities whose claims lie about their trajectory.
    cheating: Vec<PersistId>,
}

impl Authority {
    fn new(cheating: Vec<PersistId>) -> Self {
        let mut executor = Executor::new(Kinematic, SEED);
        executor.insert(A, body());
        executor.insert(B, body());
        Self {
            executor,
            heads: [(A, ChainHash::EMPTY), (B, ChainHash::EMPTY)]
                .into_iter()
                .collect(),
            previous_claim: std::collections::BTreeMap::new(),
            cheating,
        }
    }

    fn anchor(&mut self, entity: PersistId) -> (StateClaim, Body) {
        let state = self.executor.state(entity).expect("seeded").clone();
        let claim = self.sign_claim(entity, T0, state_hash(&state));
        (claim, state)
    }

    fn sign_claim(&mut self, entity: PersistId, tick: u64, hash: [u8; 32]) -> StateClaim {
        let mut claim = StateClaim {
            entity,
            chain_epoch: 0,
            tick: Tick::new(tick),
            input_head: self.heads[&entity],
            state_hash: hash,
            prev_claim: self.previous_claim.get(&entity).copied().unwrap_or([0; 32]),
            ruleset: RULESET,
            sig: subject_key().sign(b"unsigned"),
        };
        sign_claim(&subject_key(), &mut claim);
        self.previous_claim.insert(entity, claim_hash(&claim));
        claim
    }

    /// One three-tick frame covering both entities.
    fn send(&mut self, first_tick: u64) -> (LogFrame, Vec<FrameHead>) {
        let key = subject_key();
        let mut slices = Vec::new();
        let mut transitions = Vec::new();

        for entity in [A, B] {
            let prev = self.heads[&entity];
            let mut records = Vec::new();
            for offset in 0..3u64 {
                let inputs = vec![Move(STEP)];
                let record = InputRecord {
                    tick_off: offset as u16,
                    seq: 0,
                    source: RecordSource::Player {
                        node: key.public(),
                        input_seq: (first_tick + offset) as u32,
                    },
                    payload: bytes::Bytes::from(inputs[0].to_canonical()),
                };
                self.heads
                    .insert(entity, orrery_core::log::fold(self.heads[&entity], &record));
                records.push(record);
                self.executor
                    .step_entity(entity, Tick::new(first_tick + offset), &inputs)
                    .expect("entity present");
            }
            let head = self.heads[&entity];
            slices.push(EntitySlice {
                entity,
                chain_epoch: 0,
                prev_head: prev.rolling(),
                records,
                head: head.rolling(),
            });
            transitions.push(HeadTransition {
                entity,
                prev_head: prev,
                head,
            });
        }

        let frame = LogFrame {
            ruleset: RULESET,
            first_tick: Tick::new(first_tick),
            tick_count: 3,
            entities: slices,
            sig: sign_frame(&key, RULESET, Tick::new(first_tick), 3, &transitions),
        };
        let heads = transitions
            .iter()
            .map(|transition| FrameHead {
                entity: transition.entity,
                prev_head: transition.prev_head,
                head: transition.head,
            })
            .collect();
        (frame, heads)
    }

    /// The claim for `entity` at `tick`, honest unless the entity is cheating.
    fn claim(&mut self, entity: PersistId, tick: u64) -> StateClaim {
        let honest = state_hash(self.executor.state(entity).expect("present"));
        let hash = if self.cheating.contains(&entity) {
            [0xAB; 32]
        } else {
            honest
        };
        self.sign_claim(entity, tick, hash)
    }
}

fn watching(
    config: WitnessConfig,
    authority: &mut Authority,
    of: &[PersistId],
) -> Witness<Kinematic> {
    let mut witness = Witness::new(config, SEED, || Kinematic);
    for entity in of {
        let (anchor, anchor_state) = authority.anchor(*entity);
        witness
            .watch(Watch {
                entity: *entity,
                subject: subject_key().public(),
                anchor,
                anchor_state,
            })
            .expect("the anchor is signed by the subject");
    }
    witness
}

// ── Tests ────────────────────────────────────────────────────────────────

#[test]
fn every_watched_entity_in_a_frame_is_re_executed_not_just_the_first() {
    // The failure this pins down is silent, which is what makes it bad: a
    // witness that folds only the first entity it recognises keeps accepting
    // frames, keeps its counters clean, and simply does not watch the rest of
    // what it was asked to watch. Nothing anywhere reports the omission — the
    // second entity's claims are never compared because there is no computed
    // hash to compare them against, and "no hash yet" is indistinguishable from
    // "not caught up".
    let mut authority = Authority::new(Vec::new());
    let mut witness = watching(WitnessConfig::default(), &mut authority, &[A, B]);

    for round in 0..10u64 {
        let (frame, heads) = authority.send(T0 + round * 3);
        let signals = witness
            .ingest_wire_frame(&frame, &heads)
            .expect("frames are signed and chained");
        assert!(signals.is_empty(), "round {round}: {signals:?}");
    }

    // Both trajectories exist, not just A's.
    for entity in [A, B] {
        witness
            .replay_window(entity, (Tick::new(T0), Tick::new(T0 + 30)))
            .unwrap_or_else(|error| panic!("{entity:?} was never re-executed: {error}"));
    }
}

#[test]
fn the_second_watched_entity_can_still_be_caught_cheating() {
    // The consequence, stated as the thing that actually matters: if only the
    // first entity is re-executed, an authority hides a cheat simply by
    // authoring a lower-numbered entity alongside it.
    let mut authority = Authority::new(vec![B]);
    let mut witness = watching(WitnessConfig::default(), &mut authority, &[A, B]);

    let mut signals = Vec::new();
    for round in 0..12u64 {
        let (frame, heads) = authority.send(T0 + round * 3);
        signals.extend(witness.ingest_wire_frame(&frame, &heads).expect("chained"));
        let next = T0 + (round + 1) * 3;
        if (next - T0).is_multiple_of(30) {
            for entity in [A, B] {
                let claim = authority.claim(entity, next);
                if let Some(signal) = witness.ingest_claim(&claim).expect("signed") {
                    signals.push(signal);
                }
            }
        }
    }

    let caught: Vec<PersistId> = signals
        .iter()
        .filter_map(|signal| match signal {
            WitnessSignal::ClaimMismatch { entity, .. } => Some(*entity),
            _ => None,
        })
        .collect();
    assert_eq!(caught, vec![B], "B lies; A does not — got {signals:?}");
}

#[test]
fn a_followed_entitys_head_is_the_witnesss_own_fold_not_the_senders() {
    // The wire carries a head pair for every entity because a receiver cannot
    // fold chains it does not follow. For the ones it *does* follow, taking the
    // sender's word would let an authority nominate the head its own frame is
    // checked against — which is the whole signature check, handed to the party
    // being checked.
    let mut authority = Authority::new(Vec::new());
    let mut witness = watching(WitnessConfig::default(), &mut authority, &[A, B]);

    let (frame, mut heads) = authority.send(T0);
    for head in &mut heads {
        head.prev_head = ChainHash([0x99; 32]);
        head.head = ChainHash([0x99; 32]);
    }

    // Both entities are followed, so every pair in `heads` is noise the witness
    // must ignore. It verifies from its own fold instead.
    let signals = witness
        .ingest_wire_frame(&frame, &heads)
        .expect("the sender's claims about followed heads are not used");
    assert!(signals.is_empty(), "{signals:?}");
    assert_eq!(witness.counters().frames_accepted, 1);
}

#[test]
fn a_multi_frame_repair_of_multi_entity_frames_closes_the_hole() {
    // A range response carries one head pair per entity for the *whole* answer,
    // since repeating it per frame would multiply the overhead. Replaying every
    // frame against that one snapshot checks each frame after the first against
    // a sibling head that is a frame stale — so the fold lands elsewhere, the
    // frame is refused, and a repair the authority served correctly and in full
    // is thrown away frame by frame. The hole never closes and the subject is
    // escalated as stalled for answering properly.
    let mut authority = Authority::new(Vec::new());
    // Watch only A, so B travels as a sibling and its head has to be threaded
    // forward across the response — the case that breaks.
    let mut witness = watching(WitnessConfig::default(), &mut authority, &[A]);

    let mut log = AuthorityLog::default();
    let mut live = Vec::new();
    for round in 0..6u64 {
        let (frame, heads) = authority.send(T0 + round * 3);
        let transitions: Vec<HeadTransition> = heads
            .iter()
            .map(|head| HeadTransition {
                entity: head.entity,
                prev_head: head.prev_head,
                head: head.head,
            })
            .collect();
        log.record_frame(frame.clone(), transitions);
        live.push((frame, heads));
    }

    // The first frame lands; rounds 1..6 are all lost.
    witness
        .ingest_wire_frame(&live[0].0, &live[0].1)
        .expect("the first frame chains from the anchor");

    // The next live frame opens a gap, and the authority answers it in full.
    let (frame, heads) = authority.send(T0 + 18);
    let signals = witness.ingest_wire_frame(&frame, &heads).expect("a gap");
    let [WitnessSignal::Gap(request)] = signals.as_slice() else {
        panic!("expected one gap, got {signals:?}");
    };
    let served = log.serve_range(request, usize::MAX);
    assert_eq!(
        served.response.frames.len(),
        5,
        "the authority holds the whole hole"
    );

    let repaired = witness
        .ingest_wire_frames(&served.response.frames, &served.heads)
        .expect("a correctly served repair is not a rejection");
    assert!(
        repaired.is_empty(),
        "a repair that closes the hole says nothing: {repaired:?}"
    );
    assert_eq!(
        witness.counters().frames_rejected,
        0,
        "not one frame of a correct answer may be refused"
    );
    assert_eq!(witness.counters().frames_accepted, 6);
    assert!(
        witness.catching_up(A).is_none(),
        "the hole is closed, so judgement resumes"
    );

    // And the chain is genuinely whole: the frame that revealed the gap now
    // chains onto it.
    let after = witness
        .ingest_wire_frame(&frame, &heads)
        .expect("the live frame chains once the hole is filled");
    assert!(after.is_empty(), "{after:?}");
}

#[test]
fn a_repair_answered_out_of_order_is_refused_rather_than_mis_folded() {
    // The other half of threading heads forward: the witness must not accept a
    // run it cannot chain. Feeding the same frames backwards has to fail, not
    // quietly produce a trajectory nobody ran.
    let mut authority = Authority::new(Vec::new());
    let mut witness = watching(WitnessConfig::default(), &mut authority, &[A]);

    let mut log = AuthorityLog::default();
    for round in 0..4u64 {
        let (frame, heads) = authority.send(T0 + round * 3);
        let transitions: Vec<HeadTransition> = heads
            .iter()
            .map(|head| HeadTransition {
                entity: head.entity,
                prev_head: head.prev_head,
                head: head.head,
            })
            .collect();
        log.record_frame(frame, transitions);
    }

    let served = log.serve_range(
        &LogRangeRequest {
            entity: A,
            chain_epoch: 0,
            from_tick: Tick::new(T0),
            to_tick: Tick::new(T0 + 12),
        },
        usize::MAX,
    );
    let mut backwards = served.response.frames.clone();
    backwards.reverse();

    assert!(
        witness
            .ingest_wire_frames(&backwards, &served.heads)
            .is_err(),
        "a run that does not chain must be refused, not folded anyway"
    );
}
