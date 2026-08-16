//! The P4 pipeline end to end: a cheating authority is caught by a witness
//! that trusts nothing it says, and the resulting report is adjudicated.
//!
//! The cheat is D10's canonical one — a speed multiplier. The authority logs
//! honest inputs (it has to; the log is signed and chained) but claims a state
//! its own inputs do not produce. Nothing else about it is anomalous: the
//! signatures are real, the chain is intact, the claims are its own.
//!
//! That is exactly why continuous log re-execution is *the* witness signal for
//! entities nobody is interacting with. A cheat visible only in the gap between
//! logged inputs and claimed outcomes cannot be seen any other way.

use orrery_core::invariants::checks;
use orrery_core::log::{claim_hash, sign_claim, sign_frame, HeadTransition};
use orrery_core::{
    state_hash, CodecError, CoreCodec, Executor, Invariant, InvariantKind, InvariantSample,
    InvariantViolation, OrderedInputs, QPos, QVel, Quantized, Ruleset, StateView, StepOutput,
    TickRng,
};
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick, UniverseSeed, Verdict,
};
use orrery_witness::{Observation, Watch, Witness, WitnessConfig, WitnessSignal};
use rand_chacha::rand_core::RngCore;

// ── A ruleset with a speed cap it can be caught violating ────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    pos: QPos,
    vel: QVel,
    entropy: u32,
}

impl CoreCodec for Body {
    fn encode(&self, out: &mut Vec<u8>) {
        for value in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.entropy.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 52 {
            return Err(CodecError("body is 52 bytes"));
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
            entropy: u32::from_le_bytes(bytes[48..52].try_into().unwrap()),
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

/// Millimetres per tick an honest body can move: 3 m/s at 60 Hz.
const SPEED_CAP: i64 = 50;

fn speed_invariant(sample: &InvariantSample<'_, Body>) -> Result<(), InvariantViolation> {
    let Some(previous) = sample.previous else {
        return Ok(());
    };
    if checks::exceeds_speed(
        previous.pos,
        sample.current.pos,
        sample.elapsed_ticks,
        SPEED_CAP,
    ) {
        return Err(InvariantViolation::new(InvariantKind::SpeedCap, "speed"));
    }
    Ok(())
}

const INVARIANTS: &[Invariant<Body>] = &[Invariant {
    name: "speed",
    check: speed_invariant,
}];

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
        rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        let mut requested = 0i64;
        for input in inputs.iter() {
            requested += input.0;
        }
        let state = view.own_mut();
        // The rules clamp. A cheat is a *claim* that this clamp did not apply.
        state.vel.x = requested.clamp(-SPEED_CAP, SPEED_CAP);
        state.pos.x += state.vel.x;
        state.entropy = state.entropy.wrapping_add(rng.next_u32());
        StepOutput::default()
    }

    fn invariants(&self) -> &[Invariant<Body>] {
        INVARIANTS
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────────

const ENTITY: PersistId = PersistId::new(4_242);
const SEED: UniverseSeed = UniverseSeed([0x77; 32]);
const T0: u64 = 3_000;
const CLAIM_EVERY: u64 = 30;

fn subject_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[21; 32])
}

fn witness_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[22; 32])
}

fn body() -> Body {
    Body {
        pos: QPos::default(),
        vel: QVel::default(),
        entropy: 0,
    }
}

/// The honest input the authority logs every tick: hold full legal speed.
fn input_at(_tick: u64) -> Vec<Move> {
    vec![Move(SPEED_CAP)]
}

/// What the authority streams for one three-tick frame.
struct Sent {
    frame: LogFrame,
    claim: Option<StateClaim>,
}

/// An authority that logs honestly and, if `cheat_multiplier > 1`, claims a
/// trajectory faster than its own logged inputs can produce.
struct Authority {
    executor: Executor<Kinematic>,
    head: ChainHash,
    previous_claim: [u8; 32],
    cheat_multiplier: i64,
    cheat_state: Body,
}

impl Authority {
    fn new(cheat_multiplier: i64) -> Self {
        let mut executor = Executor::new(Kinematic, SEED);
        executor.insert(ENTITY, body());
        Self {
            executor,
            head: ChainHash::EMPTY,
            previous_claim: [0; 32],
            cheat_multiplier,
            cheat_state: body(),
        }
    }

    fn anchor(&mut self) -> (StateClaim, Body) {
        let state = self.executor.state(ENTITY).expect("seeded").clone();
        let mut claim = StateClaim {
            entity: ENTITY,
            chain_epoch: 0,
            tick: Tick::new(T0),
            input_head: self.head,
            state_hash: state_hash(&state),
            prev_claim: self.previous_claim,
            ruleset: RULESET,
            sig: subject_key().sign(b"unsigned"),
        };
        sign_claim(&subject_key(), &mut claim);
        self.previous_claim = claim_hash(&claim);
        (claim, state)
    }

    fn send(&mut self, first_tick: u64) -> Sent {
        let key = subject_key();
        let prev_head = self.head;
        let mut records = Vec::new();

        for offset in 0..3u64 {
            let tick = first_tick + offset;
            let inputs = input_at(tick);
            for (seq, input) in inputs.iter().enumerate() {
                let record = InputRecord {
                    tick_off: offset as u16,
                    seq: seq as u16,
                    source: RecordSource::Player {
                        node: key.public(),
                        input_seq: tick as u32,
                    },
                    payload: bytes::Bytes::from(input.to_canonical()),
                };
                self.head = orrery_core::log::fold(self.head, &record);
                records.push(record);
            }
            self.executor
                .step_entity(ENTITY, Tick::new(tick), &inputs)
                .expect("entity present");

            // The cheat: advance a private trajectory faster than the rules
            // allow, and claim *that* instead. The log stays honest because it
            // is signed and chained — lying there is caught immediately.
            self.cheat_state.vel.x = SPEED_CAP * self.cheat_multiplier;
            self.cheat_state.pos.x += self.cheat_state.vel.x;
            self.cheat_state.entropy = self.executor.state(ENTITY).expect("entity present").entropy;
        }

        let transitions = vec![HeadTransition {
            entity: ENTITY,
            prev_head,
            head: self.head,
        }];
        let frame = LogFrame {
            ruleset: RULESET,
            first_tick: Tick::new(first_tick),
            tick_count: 3,
            entities: vec![EntitySlice {
                entity: ENTITY,
                chain_epoch: 0,
                prev_head: prev_head.rolling(),
                records,
                head: self.head.rolling(),
            }],
            sig: sign_frame(&key, RULESET, Tick::new(first_tick), 3, &transitions),
        };

        let next_tick = first_tick + 3;
        let claim = ((next_tick - T0) % CLAIM_EVERY == 0).then(|| {
            let claimed = if self.cheat_multiplier > 1 {
                &self.cheat_state
            } else {
                self.executor.state(ENTITY).expect("entity present")
            };
            let mut claim = StateClaim {
                entity: ENTITY,
                chain_epoch: 0,
                tick: Tick::new(next_tick),
                input_head: self.head,
                state_hash: state_hash(claimed),
                prev_claim: self.previous_claim,
                ruleset: RULESET,
                sig: key.sign(b"unsigned"),
            };
            sign_claim(&key, &mut claim);
            self.previous_claim = claim_hash(&claim);
            claim
        });

        Sent { frame, claim }
    }

    /// What the authority tells the world its state is.
    fn advertised(&self) -> Body {
        if self.cheat_multiplier > 1 {
            self.cheat_state.clone()
        } else {
            self.executor.state(ENTITY).expect("entity present").clone()
        }
    }
}

fn watching(config: WitnessConfig, authority: &mut Authority) -> Witness<Kinematic> {
    let mut witness = Witness::new(config, SEED, || Kinematic);
    let (anchor, anchor_state) = authority.anchor();
    witness
        .watch(Watch {
            entity: ENTITY,
            subject: subject_key().public(),
            anchor,
            anchor_state,
        })
        .expect("the anchor is signed by the subject");
    witness
}

/// Stream `frames` three-tick frames into the witness, collecting signals.
fn stream(
    witness: &mut Witness<Kinematic>,
    authority: &mut Authority,
    frames: u64,
) -> Vec<WitnessSignal> {
    let mut signals = Vec::new();
    for index in 0..frames {
        let sent = authority.send(T0 + index * 3);
        signals.extend(
            witness
                .ingest_frame(&sent.frame, &[])
                .expect("frames are signed and chained"),
        );
        if let Some(claim) = sent.claim {
            if let Some(signal) = witness.ingest_claim(&claim).expect("claim is signed") {
                signals.push(signal);
            }
        }
    }
    signals
}

// ── The tests ────────────────────────────────────────────────────────────

#[test]
fn an_honest_authority_is_watched_without_a_single_signal() {
    // The baseline any detector has to clear before its accusations mean
    // anything: watch a peer doing nothing wrong for a hundred ticks and stay
    // silent. A detector that fires here has no false-positive budget left.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let signals = stream(&mut witness, &mut authority, 33);

    assert!(
        signals.is_empty(),
        "an honest authority produced signals: {signals:?}"
    );
    let counters = witness.counters();
    assert_eq!(counters.frames_accepted, 33);
    assert_eq!(counters.claim_mismatches, 0);
    assert_eq!(counters.invariant_breaches, 0);
}

#[test]
fn a_speed_cheat_is_caught_by_re_executing_its_own_log() {
    // The D10 canonical cheat. Every signature is real and the chain is
    // intact; the lie is only visible in the gap between the inputs it logged
    // and the state it claimed — which is exactly what continuous
    // re-execution is for.
    let mut authority = Authority::new(3);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let signals = stream(&mut witness, &mut authority, 20);

    let mismatch = signals.iter().find_map(|signal| match signal {
        WitnessSignal::ClaimMismatch { entity, at } => Some((*entity, *at)),
        _ => None,
    });
    let (entity, at) = mismatch.expect("the cheat must be detected");
    assert_eq!(entity, ENTITY);
    // The first claim covers ticks T0..T0+30, so that is where it surfaces.
    assert_eq!(at, Tick::new(T0 + CLAIM_EVERY));
    assert!(witness.counters().claim_mismatches >= 1);
}

#[test]
fn the_stage_one_speed_check_also_catches_it_without_any_log() {
    // Stage 1 is what peers *outside* the witness set contribute. It needs no
    // log and no re-execution — just two samples — so it is the cheap signal
    // that arms an audit before anyone re-executes anything.
    let mut authority = Authority::new(3);
    let mut witness = watching(WitnessConfig::default(), &mut authority);

    let mut breach = None;
    for index in 0..10u64 {
        authority.send(T0 + index * 3);
        let advertised = authority.advertised();
        if let Some(signal) = witness.observe(Observation {
            entity: ENTITY,
            state: &advertised,
            tick: Tick::new(T0 + index * 3 + 3),
        }) {
            breach = Some(signal);
            break;
        }
    }

    assert!(
        matches!(
            breach,
            Some(WitnessSignal::InvariantBreach {
                violation: InvariantViolation {
                    kind: InvariantKind::SpeedCap,
                    ..
                },
                ..
            })
        ),
        "stage 1 should catch a 3x speed multiplier, got {breach:?}"
    );
}

#[test]
fn shadow_mode_detects_everything_and_files_nothing() {
    // The P4 posture, and the whole reason the phase exists. Detection is on;
    // enforcement is not, until the false-positive rate has been measured on
    // real hardware (D17 risk 3).
    let mut authority = Authority::new(3);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    assert!(witness.shadow_mode());
    stream(&mut witness, &mut authority, 20);

    let raised = witness
        .raise(
            &witness_key(),
            ENTITY,
            (Tick::new(T0), Tick::new(T0 + CLAIM_EVERY)),
        )
        .expect("entity is watched");

    assert_eq!(
        raised,
        WitnessSignal::Report(None),
        "shadow mode files nothing"
    );
    let counters = witness.counters();
    assert_eq!(counters.reports_raised, 1, "but it is still counted");
    assert_eq!(counters.reports_filed, 0);
    assert!(counters.claim_mismatches >= 1, "and detection still ran");
}

#[test]
fn out_of_shadow_mode_the_report_survives_independent_adjudication() {
    // The end of the pipeline: a witness files, and a cluster that believes
    // nothing the witness says re-runs the evidence and reaches the same
    // verdict.
    let mut authority = Authority::new(3);
    let mut witness = watching(
        WitnessConfig {
            shadow_mode: false,
            ..WitnessConfig::default()
        },
        &mut authority,
    );
    stream(&mut witness, &mut authority, 20);

    let raised = witness
        .raise(
            &witness_key(),
            ENTITY,
            (Tick::new(T0), Tick::new(T0 + CLAIM_EVERY)),
        )
        .expect("entity is watched");
    let WitnessSignal::Report(Some(report)) = raised else {
        panic!("expected a filed report, got {raised:?}");
    };
    assert_eq!(report.subject, subject_key().public());
    assert_eq!(report.reporter, witness_key().public());
    assert!(orrery_witness::verify_report(&report).is_ok());

    let mut adjudicator = orrery_persistd::AdjudicationExecutor::new(SEED);
    adjudicator.register(|| Kinematic);
    assert!(
        matches!(adjudicator.adjudicate(&report), Verdict::Confirms { .. }),
        "the cluster must reach the same verdict from the evidence alone"
    );
}

#[test]
fn an_honest_authority_survives_adjudication_of_its_own_window() {
    // The other half of the same guarantee, and the one that protects players:
    // a filed report against an honest peer exonerates rather than convicts.
    let mut authority = Authority::new(1);
    let mut witness = watching(
        WitnessConfig {
            shadow_mode: false,
            ..WitnessConfig::default()
        },
        &mut authority,
    );
    stream(&mut witness, &mut authority, 20);

    let raised = witness
        .raise(
            &witness_key(),
            ENTITY,
            (Tick::new(T0), Tick::new(T0 + CLAIM_EVERY)),
        )
        .expect("entity is watched");
    let WitnessSignal::Report(Some(report)) = raised else {
        panic!("expected a filed report, got {raised:?}");
    };

    let mut adjudicator = orrery_persistd::AdjudicationExecutor::new(SEED);
    adjudicator.register(|| Kinematic);
    assert_eq!(adjudicator.adjudicate(&report), Verdict::Exonerates);
}

#[test]
fn a_frame_from_the_wrong_key_is_refused_before_it_is_replayed() {
    // A witness watches *one* authority. Accepting a frame signed by anyone
    // else would let a third party inject history into someone's chain.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);

    let mut sent = authority.send(T0);
    let impostor = iroh_base::SecretKey::from_bytes(&[99; 32]);
    let transitions = vec![HeadTransition {
        entity: ENTITY,
        prev_head: ChainHash::EMPTY,
        head: ChainHash::EMPTY,
    }];
    sent.frame.sig = sign_frame(&impostor, RULESET, Tick::new(T0), 3, &transitions);

    assert!(witness.ingest_frame(&sent.frame, &[]).is_err());
    assert_eq!(witness.counters().frames_rejected, 1);
    assert_eq!(witness.counters().frames_accepted, 0);
}

#[test]
fn a_dropped_frame_asks_for_a_refill_rather_than_accusing() {
    // Logs ride the unreliable lane, so loss is the expected case. A witness
    // that treated a hole as fabrication would strike honest peers on a lossy
    // link — the D17 risk-3 failure mode, arriving through the back door.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);

    let first = authority.send(T0);
    witness
        .ingest_frame(&first.frame, &[])
        .expect("first frame lands");
    let _dropped = authority.send(T0 + 3);
    let third = authority.send(T0 + 6);

    let signals = witness
        .ingest_frame(&third.frame, &[])
        .expect("a gap is a signal, not an error");
    assert!(
        matches!(
            signals.as_slice(),
            [WitnessSignal::Gap(request)] if request.entity == ENTITY && request.to_tick == Tick::new(T0 + 6)
        ),
        "expected a range request, got {signals:?}"
    );
    assert_eq!(witness.counters().gaps_detected, 1);
    assert_eq!(
        witness.counters().claim_mismatches,
        0,
        "a gap must never be counted as a mismatch"
    );
}

#[test]
fn an_anchor_the_subject_did_not_sign_is_refused() {
    // Anchoring on an unsigned claim would re-execute from a starting point
    // the subject never committed to, making every later comparison meaningless.
    let mut authority = Authority::new(1);
    let (mut anchor, anchor_state) = authority.anchor();
    sign_claim(&iroh_base::SecretKey::from_bytes(&[98; 32]), &mut anchor);

    let mut witness = Witness::new(WitnessConfig::default(), SEED, || Kinematic);
    assert!(witness
        .watch(Watch {
            entity: ENTITY,
            subject: subject_key().public(),
            anchor,
            anchor_state,
        })
        .is_err());
}
