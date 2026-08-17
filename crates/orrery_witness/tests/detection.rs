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
use orrery_core::store::AuthorityLog;
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
/// A second subject's entity, for the one counter a single subject cannot
/// reach — see [`Authority::for_subject`].
const OTHER: PersistId = PersistId::new(4_243);
const SEED: UniverseSeed = UniverseSeed([0x77; 32]);
const T0: u64 = 3_000;
const CLAIM_EVERY: u64 = 30;

fn subject_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[21; 32])
}

fn second_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[0x2c; 32])
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
    /// The key every frame and claim is signed with, and the entity it holds.
    ///
    /// Parameters rather than constants because one counter — frames swept out
    /// of the deferral buffer by retention — is only reachable with a *second*
    /// subject: a witness prunes on the ingest path, so a subject whose frames
    /// are stranded needs some other subject still feeding the witness ticks.
    key: iroh_base::SecretKey,
    entity: PersistId,
    executor: Executor<Kinematic>,
    head: ChainHash,
    previous_claim: [u8; 32],
    cheat_multiplier: i64,
    cheat_state: Body,
}

impl Authority {
    fn new(cheat_multiplier: i64) -> Self {
        Self::for_subject(subject_key(), ENTITY, cheat_multiplier)
    }

    fn for_subject(key: iroh_base::SecretKey, entity: PersistId, cheat_multiplier: i64) -> Self {
        let mut executor = Executor::new(Kinematic, SEED);
        executor.insert(entity, body());
        Self {
            key,
            entity,
            executor,
            head: ChainHash::EMPTY,
            previous_claim: [0; 32],
            cheat_multiplier,
            cheat_state: body(),
        }
    }

    fn anchor(&mut self) -> (StateClaim, Body) {
        let state = self.executor.state(self.entity).expect("seeded").clone();
        let mut claim = StateClaim {
            entity: self.entity,
            chain_epoch: 0,
            tick: Tick::new(T0),
            input_head: self.head,
            state_hash: state_hash(&state),
            prev_claim: self.previous_claim,
            ruleset: RULESET,
            sig: self.key.sign(b"unsigned"),
        };
        sign_claim(&self.key, &mut claim);
        self.previous_claim = claim_hash(&claim);
        (claim, state)
    }

    fn send(&mut self, first_tick: u64) -> Sent {
        let key = self.key.clone();
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
                .step_entity(self.entity, Tick::new(tick), &inputs)
                .expect("entity present");

            // The cheat: advance a private trajectory faster than the rules
            // allow, and claim *that* instead. The log stays honest because it
            // is signed and chained — lying there is caught immediately.
            self.cheat_state.vel.x = SPEED_CAP * self.cheat_multiplier;
            self.cheat_state.pos.x += self.cheat_state.vel.x;
            self.cheat_state.entropy = self
                .executor
                .state(self.entity)
                .expect("entity present")
                .entropy;
        }

        let transitions = vec![HeadTransition {
            entity: self.entity,
            prev_head,
            head: self.head,
        }];
        let frame = LogFrame {
            ruleset: RULESET,
            first_tick: Tick::new(first_tick),
            tick_count: 3,
            entities: vec![EntitySlice {
                entity: self.entity,
                chain_epoch: 0,
                prev_head: prev_head.rolling(),
                records,
                head: self.head.rolling(),
            }],
            sig: sign_frame(&key, RULESET, Tick::new(first_tick), 3, &transitions),
        };

        let next_tick = first_tick + 3;
        let claim = (next_tick - T0).is_multiple_of(CLAIM_EVERY).then(|| {
            let claimed = if self.cheat_multiplier > 1 {
                &self.cheat_state
            } else {
                self.executor.state(self.entity).expect("entity present")
            };
            let mut claim = StateClaim {
                entity: self.entity,
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

    /// Start cheating from wherever the honest trajectory currently is.
    ///
    /// Used to prove a witness is judging *now* rather than to set up a cheat
    /// from the beginning: a divergence introduced after some event is only
    /// caught if the watch survived that event.
    fn start_cheating(&mut self, multiplier: i64) {
        self.cheat_multiplier = multiplier;
        self.cheat_state = self
            .executor
            .state(self.entity)
            .expect("entity present")
            .clone();
    }

    /// What the authority tells the world its state is.
    fn advertised(&self) -> Body {
        if self.cheat_multiplier > 1 {
            self.cheat_state.clone()
        } else {
            self.executor
                .state(self.entity)
                .expect("entity present")
                .clone()
        }
    }
}

fn watching(config: WitnessConfig, authority: &mut Authority) -> Witness<Kinematic> {
    let mut witness = Witness::new(config, SEED, || Kinematic);
    watch_also(&mut witness, authority);
    witness
}

/// Add `authority`'s entity to an existing witness, anchored at a fresh claim.
fn watch_also(witness: &mut Witness<Kinematic>, authority: &mut Authority) {
    let entity = authority.entity;
    let subject = authority.key.public();
    let (anchor, anchor_state) = authority.anchor();
    witness
        .watch(Watch {
            entity,
            subject,
            anchor,
            anchor_state,
        })
        .expect("the anchor is signed by the subject");
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

#[test]
fn a_disputed_claim_is_raised_once_not_once_per_packet() {
    // A divergence is a fact about a claim, not about the frame that happened
    // to arrive next. Re-scanning the retained claims on every ingest finds the
    // same disagreement again and again — at the default 600-tick retention,
    // hundreds of times per real divergence — so `claim_mismatches` would
    // measure the ingest rate rather than count divergences. P4 exists to
    // produce exactly that count, which makes this a measurement bug and not
    // only a noise one. It is the same defect `Catchup::reported` fixed for
    // gaps.
    let mut authority = Authority::new(3);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let signals = stream(&mut witness, &mut authority, 20);

    let mut at: Vec<Tick> = signals
        .iter()
        .filter_map(|signal| match signal {
            WitnessSignal::ClaimMismatch { at, .. } => Some(*at),
            _ => None,
        })
        .collect();
    let raised = at.len();
    at.sort_unstable();
    at.dedup();
    assert_eq!(
        raised,
        at.len(),
        "the same claim was disputed twice: {at:?}"
    );
    assert_eq!(
        at,
        vec![Tick::new(T0 + CLAIM_EVERY), Tick::new(T0 + 2 * CLAIM_EVERY)],
        "one finding per signed claim that does not hold up"
    );
    assert_eq!(witness.counters().claim_mismatches, raised as u64);
}

#[test]
fn a_report_can_be_filed_from_a_window_later_than_the_anchor() {
    // A witness is never sent state, so the only snapshot it can open a bundle
    // from is one it computed itself. Retaining only the anchor it was handed
    // makes `[anchor, anchor+180)` the sole window it can ever serve — and the
    // anchor ages out of retention long before the session does, so past a few
    // seconds of watching the witness is structurally unable to file anything
    // at all. Every other test here files from the anchor, which is exactly why
    // that went unnoticed.
    let mut authority = Authority::new(1);
    let mut witness = watching(
        WitnessConfig {
            shadow_mode: false,
            ..WitnessConfig::default()
        },
        &mut authority,
    );
    stream(&mut witness, &mut authority, 40);

    let later = (
        Tick::new(T0 + 2 * CLAIM_EVERY),
        Tick::new(T0 + 3 * CLAIM_EVERY),
    );
    let raised = witness
        .raise(&witness_key(), ENTITY, later)
        .expect("a window opening at an agreed claim is servable");
    let WitnessSignal::Report(Some(report)) = raised else {
        panic!("expected a filed report, got {raised:?}");
    };
    assert_eq!(report.bundle.window_start, later.0);
    assert!(orrery_witness::verify_report(&report).is_ok());

    // And it is real evidence, not merely well-formed: an honest peer's own
    // window exonerates it.
    let mut adjudicator = orrery_persistd::AdjudicationExecutor::new(SEED);
    adjudicator.register(|| Kinematic);
    assert_eq!(adjudicator.adjudicate(&report), Verdict::Exonerates);
}

#[test]
fn the_audit_window_opens_at_the_last_claim_the_two_agreed_on() {
    // Stage 2 has to pick a t0 the *subject* still stands behind. Opening at a
    // claim the witness already disagrees with would fail the snapshot hash
    // check at the adjudicator — which is read as `EvidenceForged` against the
    // reporter, so a witness that got this wrong would convict itself for
    // catching a cheat.
    let mut authority = Authority::new(3);
    let mut witness = watching(
        WitnessConfig {
            shadow_mode: false,
            ..WitnessConfig::default()
        },
        &mut authority,
    );
    stream(&mut witness, &mut authority, 20);

    let at = Tick::new(T0 + CLAIM_EVERY);
    let window = witness
        .audit_window(ENTITY, at)
        .expect("the anchor is still the last agreed point");
    assert_eq!(window, (Tick::new(T0), at));

    let raised = witness
        .raise(&witness_key(), ENTITY, window)
        .expect("filed");
    let WitnessSignal::Report(Some(report)) = raised else {
        panic!("expected a filed report, got {raised:?}");
    };
    let mut adjudicator = orrery_persistd::AdjudicationExecutor::new(SEED);
    adjudicator.register(|| Kinematic);
    assert!(
        matches!(adjudicator.adjudicate(&report), Verdict::Confirms { .. }),
        "the window stage 2 chose must be one the cluster can adjudicate"
    );
}

#[test]
fn a_window_the_witness_cannot_serve_is_an_error_not_silence() {
    // `Report(None)` is shadow mode's answer, and it means "we chose not to
    // file". Returning it for "we could not" as well is how a witness that has
    // gone structurally mute keeps looking like one that is being careful.
    let mut authority = Authority::new(1);
    let mut witness = watching(
        WitnessConfig {
            shadow_mode: false,
            ..WitnessConfig::default()
        },
        &mut authority,
    );
    stream(&mut witness, &mut authority, 20);

    // Mid-claim: no committed state to open from.
    assert_eq!(
        witness.raise(
            &witness_key(),
            ENTITY,
            (Tick::new(T0 + 5), Tick::new(T0 + 35))
        ),
        Err(orrery_witness::WitnessError::WindowUnservable)
    );
    // Past everything re-executed: the missing ticks are missing, not zero.
    assert_eq!(
        witness.raise(
            &witness_key(),
            ENTITY,
            (Tick::new(T0), Tick::new(T0 + 4 * CLAIM_EVERY))
        ),
        Err(orrery_witness::WitnessError::WindowUnservable)
    );
    assert_eq!(
        witness.counters().reports_filed,
        0,
        "nothing was filed, and nothing pretended to be"
    );
}

#[test]
fn a_subject_that_goes_quiet_inside_a_hole_is_still_escalated() {
    // Every other repair check hangs off a frame arriving. A subject that stops
    // sending therefore sits in `catchup` forever — unjudged, unescalated, and
    // costing nothing to maintain. That is the cheap version of the stall the
    // escalation threshold exists to close, so the clock has to come from
    // somewhere other than the subject.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);

    witness
        .ingest_frame(&authority.send(T0).frame, &[])
        .expect("the first frame lands");
    let _lost = authority.send(T0 + 3);
    let third = authority.send(T0 + 6);
    let opened = witness.ingest_frame(&third.frame, &[]).expect("a gap");
    assert!(matches!(opened.as_slice(), [WitnessSignal::Gap(_)]));

    // Now silence. Only the local clock advances.
    let mut stalled = 0;
    let mut now = T0 + 6;
    for _ in 0..40 {
        now += 30;
        for signal in witness.sweep(Tick::new(now)) {
            if matches!(signal, WitnessSignal::Stalled { .. }) {
                stalled += 1;
            }
        }
    }
    assert_eq!(stalled, 1, "escalated once, on a subject that said nothing");
    assert_eq!(witness.counters().stalled, 1);
}

/// Drive `witness` to the point where it has given up on a hole it opened.
///
/// Returns the local tick the sweeps reached, so a caller can carry on from a
/// clock that has already advanced.
fn abandoned_hole(witness: &mut Witness<Kinematic>, authority: &mut Authority) -> u64 {
    witness
        .ingest_frame(&authority.send(T0).frame, &[])
        .expect("the first frame lands");
    let _lost = authority.send(T0 + 3);
    let third = authority.send(T0 + 6);
    let opened = witness.ingest_frame(&third.frame, &[]).expect("a gap");
    assert!(matches!(opened.as_slice(), [WitnessSignal::Gap(_)]));

    // Nobody ever answers the repair.
    let mut now = T0 + 6;
    for _ in 0..40 {
        now += 30;
        witness.sweep(Tick::new(now));
    }
    assert_eq!(witness.counters().stalled, 1, "the hole was given up on");
    now
}

#[test]
fn a_hole_that_never_fills_is_abandoned_so_the_subject_is_judged_again() {
    // The failure this closes: a witness that gave up on a hole used to stop
    // judging the subject *permanently*. `repair_step` never asked again once
    // the escalation was reported, and `check_pending_claims` declines to judge
    // while a catchup is open, so the watch was finished — silently, for the
    // life of the process. Measured in `p1-swarm --witness`, every watch
    // reached that state inside about twenty-five simulated seconds and the
    // counters never moved again, which made "zero false positives over 500
    // player-hours" a statement about a witness that had stopped looking.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    abandoned_hole(&mut witness, &mut authority);

    // The subject keeps talking. A claim it signs, over a state replication
    // independently delivered, is a point the witness can resume from.
    let mut tick = T0 + 9;
    let resumed = loop {
        let sent = authority.send(tick);
        witness
            .ingest_frame(&sent.frame, &[])
            .expect("deferred while blind, never refused");
        tick += 3;
        if let Some(claim) = sent.claim {
            let advertised = authority.advertised();
            witness.observe(Observation {
                entity: ENTITY,
                state: &advertised,
                tick: claim.tick,
            });
            witness.ingest_claim(&claim).expect("the claim is signed");
            if witness.counters().reanchors == 1 {
                break claim.tick.0;
            }
        }
        assert!(tick < T0 + 400, "the witness never resumed");
    };
    assert!(
        witness.catching_up(ENTITY).is_none(),
        "resumed, rather than still waiting on a hole nobody will fill"
    );
    assert!(
        witness.counters().unjudged_ticks > 0,
        "the abandoned window is counted, not quietly forgotten"
    );

    // And it is really judging again, not merely accepting bytes: a cheat that
    // starts *after* the resume is caught.
    let accepted_before = witness.counters().frames_accepted;
    authority.start_cheating(3);
    let mut signals = Vec::new();
    for index in 0..=(CLAIM_EVERY / 3) {
        let sent = authority.send(resumed + index * 3);
        signals.extend(
            witness
                .ingest_frame(&sent.frame, &[])
                .expect("frames chain to the resumed anchor"),
        );
        if let Some(claim) = sent.claim {
            if let Some(signal) = witness.ingest_claim(&claim).expect("the claim is signed") {
                signals.push(signal);
            }
        }
    }
    assert!(
        witness.counters().frames_accepted > accepted_before,
        "the chain picked up from the new anchor"
    );
    assert!(
        signals
            .iter()
            .any(|signal| matches!(signal, WitnessSignal::ClaimMismatch { .. })),
        "a cheat after the resume is caught: {signals:?}"
    );
}

#[test]
fn a_resume_is_refused_unless_the_state_matches_what_the_subject_signed() {
    // The anchor is the one thing a witness cannot check by re-execution, so it
    // is checked by signature and hash instead. A state the subject never
    // committed to must not move the anchor, however genuine the claim beside
    // it — otherwise stalling would be a way to hand a witness a starting point
    // of the subject's choosing, which is worse than the stall.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    abandoned_hole(&mut witness, &mut authority);

    let mut tick = T0 + 9;
    let claim = loop {
        let sent = authority.send(tick);
        witness.ingest_frame(&sent.frame, &[]).expect("deferred");
        tick += 3;
        if let Some(claim) = sent.claim {
            break claim;
        }
        assert!(tick < T0 + 400, "no claim was ever cut");
    };

    // One millimetre off what the claim commits to.
    let mut wrong = authority.advertised();
    wrong.pos.x += 1;
    witness.observe(Observation {
        entity: ENTITY,
        state: &wrong,
        tick: claim.tick,
    });
    witness.ingest_claim(&claim).expect("the claim is signed");

    assert_eq!(witness.counters().reanchors, 0, "the anchor did not move");
    assert!(
        witness.catching_up(ENTITY).is_some(),
        "still blind, which is the honest state to be in"
    );
}

#[test]
fn coverage_counts_the_timeline_shown_not_the_timeline_judged() {
    // The trap this avoids: measuring a witness against its own output. A watch
    // that has stopped judging has also stopped abandoning, disputing and
    // repairing, so every ratio built from what it *did* scores it perfectly at
    // the moment it stops being worth anything. The denominator has to be what
    // the subject put in front of it.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    abandoned_hole(&mut witness, &mut authority);

    let judged_while_blind = witness.counters().judged_ticks;
    let shown_while_blind = witness.counters().shown_ticks;

    // The subject keeps streaming into a witness that cannot fold any of it.
    // No re-anchor is possible: replication delivered nothing, so there is no
    // state to check a claim against.
    let mut tick = T0 + 9;
    for _ in 0..20 {
        let sent = authority.send(tick);
        witness.ingest_frame(&sent.frame, &[]).expect("deferred");
        if let Some(claim) = sent.claim {
            witness.ingest_claim(&claim).expect("the claim is signed");
        }
        tick += 3;
    }

    assert_eq!(
        witness.counters().judged_ticks,
        judged_while_blind,
        "a blind watch judges nothing further"
    );
    assert!(
        witness.counters().shown_ticks > shown_while_blind,
        "but it is still being shown timeline, and that is what it is measured against"
    );
    assert_eq!(witness.counters().reanchors, 0, "nothing to resume from");
}

#[test]
fn frames_that_arrive_during_a_repair_are_kept_rather_than_re_requested() {
    // A repair takes a round trip, and the subject does not stop sending for
    // it. Every frame that arrives while the answer is in flight fails to chain
    // — that is what "mid-repair" means — and used to be dropped on the spot.
    // So a hole cost not just the frames that were lost but everything sent
    // while it was being filled, and closing it left the witness behind by
    // exactly the round trip, which opened the next hole. That is the
    // amplification the repair budget's own notes describe: more repairs in
    // flight crowd the state lane, which opens more holes.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let mut log = AuthorityLog::default();

    let record = |authority: &mut Authority, log: &mut AuthorityLog, tick: u64| {
        let sent = authority.send(tick);
        log.record_frame(
            sent.frame.clone(),
            vec![HeadTransition {
                entity: ENTITY,
                prev_head: ChainHash::EMPTY,
                head: ChainHash::EMPTY,
            }],
        );
        sent
    };

    let first = record(&mut authority, &mut log, T0);
    witness
        .ingest_frame(&first.frame, &[])
        .expect("the first frame chains from the anchor");
    let _lost = record(&mut authority, &mut log, T0 + 3);

    // The frame that reveals the hole, and four more behind it while the repair
    // is outstanding. None of them can chain yet.
    let revealing = record(&mut authority, &mut log, T0 + 6);
    let signals = witness.ingest_frame(&revealing.frame, &[]).expect("a gap");
    let [WitnessSignal::Gap(request)] = signals.as_slice() else {
        panic!("expected one gap, got {signals:?}");
    };
    let request = request.clone();
    for round in 3..7u64 {
        let sent = record(&mut authority, &mut log, T0 + round * 3);
        let quiet = witness
            .ingest_frame(&sent.frame, &[])
            .expect("held, not refused");
        assert!(
            quiet.is_empty(),
            "one hole is one repair, however many frames pile up behind it: {quiet:?}"
        );
    }
    let accepted_before = witness.counters().frames_accepted;
    assert_eq!(
        witness.counters().gaps_detected,
        1,
        "the frames behind the hole must not each ask for it again"
    );

    // The answer covers only what was lost. Everything held behind it should
    // fold off the back of it, with no second request.
    let served = log.serve_range(&request, usize::MAX);
    witness
        .ingest_wire_frames(&served.response.frames, &served.heads)
        .expect("a correctly served repair is not a rejection");

    assert!(
        witness.catching_up(ENTITY).is_none(),
        "the hole is closed and judgement has resumed"
    );
    assert_eq!(
        witness.counters().gaps_detected,
        1,
        "closing the hole must not open another"
    );
    assert_eq!(
        witness.counters().frames_recovered,
        5,
        "the frame that revealed the hole and the four behind it were kept"
    );
    assert_eq!(
        witness.counters().frames_accepted - accepted_before,
        6,
        "one repaired frame plus the five that were held"
    );
}

// ── The deferral ledger ──────────────────────────────────────────────────
//
// Coverage is one minus whatever is in flight through repair: `shown_ticks` is
// charged the moment a frame arrives, ahead of the branch that sets it aside,
// while `judged_ticks` only moves inside a fold. So the coverage deficit P4
// measures *is* the deferral buffer, and attributing it means every frame that
// went in came out of a named door. There are six: recovered, overflowed,
// pruned, dropped by a drain, replaced by a later copy, or still held. The
// tests below each drive one of them, and each asserts the ledger closes —
// because a counter that is merely non-zero attributes nothing.

/// Every frame set aside has since left by one of the six named doors, or is
/// still in the buffer.
fn ledger_balances(witness: &Witness<Kinematic>) -> bool {
    let counters = witness.counters();
    counters.frames_deferred
        == counters.frames_recovered
            + counters.deferrals_overflowed
            + counters.deferrals_pruned
            + counters.deferrals_dropped_in_drain
            + counters.deferrals_replaced
            + counters.deferrals_stale
            + counters.deferrals_held
}

/// Open a hole and leave it open: the first frame folds, the second is lost,
/// and the third reveals the gap and is held behind it.
fn open_hole(witness: &mut Witness<Kinematic>, authority: &mut Authority) {
    witness
        .ingest_frame(&authority.send(T0).frame, &[])
        .expect("the first frame chains from the anchor");
    let _lost = authority.send(T0 + 3);
    let revealing = authority.send(T0 + 6);
    let opened = witness.ingest_frame(&revealing.frame, &[]).expect("a gap");
    assert!(matches!(opened.as_slice(), [WitnessSignal::Gap(_)]));
}

#[test]
fn frames_dropped_for_want_of_buffer_space_are_counted_as_such() {
    // The buffer is bounded per subject and the oldest goes when it is full.
    // That is the right call — the oldest is the one the repair in flight is
    // likeliest to have covered — but it is still timeline the witness was
    // shown and will not judge, and until this counter reached the report it
    // was the one loss an operator could not see.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    open_hole(&mut witness, &mut authority);

    // Thirty-nine more behind the one that revealed it: forty deferrals against
    // a buffer that holds thirty-two.
    for round in 3..42u64 {
        witness
            .ingest_frame(&authority.send(T0 + round * 3).frame, &[])
            .expect("held, not refused");
    }

    let counters = witness.counters();
    assert_eq!(counters.frames_deferred, 40, "forty frames were set aside");
    assert_eq!(
        counters.deferrals_held, 32,
        "the buffer holds its cap and no more"
    );
    assert_eq!(
        counters.deferrals_overflowed, 8,
        "the eight the cap displaced are named rather than merely absent"
    );
    assert!(ledger_balances(&witness));
}

#[test]
fn a_frame_re_delivered_while_its_own_copy_is_held_displaces_it() {
    // A repair can re-serve a frame that is already sitting in the buffer. The
    // buffer keys on the frame's identity, so the second copy takes the first
    // one's place: one deferral in, one frame out, and no other counter moves.
    // Small, and it has to be counted anyway — an unaccounted frame is exactly
    // what makes an attribution a lower bound instead of an answer.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    witness
        .ingest_frame(&authority.send(T0).frame, &[])
        .expect("the first frame chains from the anchor");
    let _lost = authority.send(T0 + 3);
    let revealing = authority.send(T0 + 6);

    witness.ingest_frame(&revealing.frame, &[]).expect("a gap");
    witness
        .ingest_frame(&revealing.frame, &[])
        .expect("the same frame again, still behind the same hole");

    let counters = witness.counters();
    assert_eq!(counters.frames_deferred, 2);
    assert_eq!(counters.deferrals_replaced, 1);
    assert_eq!(counters.deferrals_held, 1, "one frame, held once");
    assert!(ledger_balances(&witness));
}

#[test]
fn a_held_frame_that_will_not_verify_leaves_the_ledger_by_its_own_door() {
    // The drain takes a frame out of the buffer before re-offering it, so a
    // frame the second attempt refuses is gone. `frames_rejected` records the
    // refusal; nothing recorded the *deferral* that ended with it, so a frame
    // could be set aside and never appear in any column again.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let mut log = AuthorityLog::default();

    witness
        .ingest_frame(&authority.send(T0).frame, &[])
        .expect("the first frame chains from the anchor");
    let lost = authority.send(T0 + 3);
    log.record_frame(
        lost.frame.clone(),
        vec![HeadTransition {
            entity: ENTITY,
            prev_head: ChainHash::EMPTY,
            head: ChainHash::EMPTY,
        }],
    );

    // The frame behind the hole chains perfectly and is signed by nobody. Gap
    // detection runs first, so it is held without its signature ever being
    // looked at — the refusal comes on the retry.
    let mut tampered = authority.send(T0 + 6).frame;
    tampered.sig = subject_key().sign(b"not this frame");
    let opened = witness.ingest_frame(&tampered, &[]).expect("a gap");
    let [WitnessSignal::Gap(request)] = opened.as_slice() else {
        panic!("expected one gap, got {opened:?}");
    };
    let request = request.clone();
    assert_eq!(witness.counters().deferrals_held, 1);

    let served = log.serve_range(&request, usize::MAX);
    witness
        .ingest_wire_frames(&served.response.frames, &served.heads)
        .expect("the repair itself is well formed");

    let counters = witness.counters();
    assert_eq!(
        counters.deferrals_dropped_in_drain, 1,
        "the held frame ended here and the ledger says so"
    );
    assert_eq!(counters.frames_recovered, 0, "it was not recovered");
    assert_eq!(counters.deferrals_held, 0, "and it is not still held");
    assert_eq!(counters.frames_rejected, 1);
    assert!(ledger_balances(&witness));
}

#[test]
fn a_repair_that_lands_closes_the_ledger_with_nothing_dropped() {
    // The healthy case, and the reason the other four are readable: when the
    // repair arrives in time every deferral comes back out as a fold. A run
    // whose ledger is all `frames_recovered` has no coverage deficit to
    // attribute, which is what makes a run that does one worth reading.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let mut log = AuthorityLog::default();

    let record = |authority: &mut Authority, log: &mut AuthorityLog, tick: u64| {
        let sent = authority.send(tick);
        log.record_frame(
            sent.frame.clone(),
            vec![HeadTransition {
                entity: ENTITY,
                prev_head: ChainHash::EMPTY,
                head: ChainHash::EMPTY,
            }],
        );
        sent
    };

    let first = record(&mut authority, &mut log, T0);
    witness
        .ingest_frame(&first.frame, &[])
        .expect("the first frame chains from the anchor");
    let _lost = record(&mut authority, &mut log, T0 + 3);
    let revealing = record(&mut authority, &mut log, T0 + 6);
    let opened = witness.ingest_frame(&revealing.frame, &[]).expect("a gap");
    let [WitnessSignal::Gap(request)] = opened.as_slice() else {
        panic!("expected one gap, got {opened:?}");
    };
    let request = request.clone();
    for round in 3..7u64 {
        let sent = record(&mut authority, &mut log, T0 + round * 3);
        witness.ingest_frame(&sent.frame, &[]).expect("held");
    }

    let served = log.serve_range(&request, usize::MAX);
    witness
        .ingest_wire_frames(&served.response.frames, &served.heads)
        .expect("a correctly served repair is not a rejection");

    let counters = witness.counters();
    assert_eq!(counters.frames_deferred, 5);
    assert_eq!(counters.frames_recovered, 5);
    assert_eq!(counters.deferrals_held, 0);
    assert_eq!(counters.deferrals_pruned, 0);
    assert_eq!(counters.deferrals_dropped_in_drain, 0);
    assert_eq!(counters.deferrals_overflowed, 0);
    assert_eq!(counters.deferrals_replaced, 0);
    assert!(ledger_balances(&witness));
}

#[test]
fn frames_stranded_by_a_re_anchor_are_not_reported_as_recovered() {
    // A watch that gives up on a hole and resumes at a later anchor leaves
    // every frame it was holding behind the new anchor. The drain accepts them
    // as duplicates — their ticks are behind the fold — and they leave the
    // buffer without a single one being re-executed. Counting that as recovery
    // would report the *most* expensive loss in the path as the healthy case:
    // those ticks are in `unjudged_ticks`, which is the opposite column.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    abandoned_hole(&mut witness, &mut authority);

    let mut tick = T0 + 9;
    let resumed = loop {
        let sent = authority.send(tick);
        witness.ingest_frame(&sent.frame, &[]).expect("deferred");
        tick += 3;
        if let Some(claim) = sent.claim {
            let advertised = authority.advertised();
            witness.observe(Observation {
                entity: ENTITY,
                state: &advertised,
                tick: claim.tick,
            });
            witness.ingest_claim(&claim).expect("the claim is signed");
            if witness.counters().reanchors == 1 {
                break claim.tick.0;
            }
        }
        assert!(tick < T0 + 400, "the witness never resumed");
    };
    let stranded = witness.counters().deferrals_held;
    assert!(stranded > 0, "the resume left frames behind it");

    // One frame off the new anchor is enough to run the drain over them.
    witness
        .ingest_frame(&authority.send(resumed).frame, &[])
        .expect("chains to the resumed anchor");

    let counters = witness.counters();
    assert_eq!(
        counters.deferrals_stale, stranded,
        "every stranded frame is named as stale, not as recovered"
    );
    assert_eq!(
        counters.frames_recovered, 0,
        "nothing was folded on a retry — the hole was abandoned, not filled"
    );
    assert!(
        counters.unjudged_ticks > 0,
        "and the ticks are counted lost"
    );
    assert!(ledger_balances(&witness));
}

#[test]
fn frames_held_for_a_subject_that_went_quiet_are_swept_and_counted() {
    // The quietest door in the path, and the only one that needs two subjects
    // to reach: retention runs on the ingest path, so a subject whose frames
    // are stranded behind a hole nobody will fill is swept by the ticks some
    // *other* subject keeps delivering. In a swarm every witness watches
    // several peers, so this is the ordinary case rather than a corner — and a
    // frame that leaves this way was counted as timeline shown and will never
    // be judged.
    let mut quiet = Authority::for_subject(second_key(), OTHER, 1);
    let mut talkative = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut talkative);
    watch_also(&mut witness, &mut quiet);

    // The quiet subject opens a hole and then says nothing more.
    witness
        .ingest_frame(&quiet.send(T0).frame, &[])
        .expect("the first frame chains from the anchor");
    let _lost = quiet.send(T0 + 3);
    for round in 2..6u64 {
        witness
            .ingest_frame(&quiet.send(T0 + round * 3).frame, &[])
            .expect("held behind the hole");
    }
    let stranded = witness.counters().deferrals_held;
    assert_eq!(stranded, 4, "four frames held for a subject that stopped");

    // The other subject carries the witness past the retention window. Nothing
    // in that traffic touches the quiet subject's buffer except the sweep.
    let mut tick = T0;
    for _ in 0..300 {
        witness
            .ingest_frame(&talkative.send(tick).frame, &[])
            .expect("an unbroken chain from the other subject");
        tick += 3;
    }

    let counters = witness.counters();
    assert_eq!(
        counters.deferrals_pruned, stranded,
        "the stranded frames left through retention and the ledger says so"
    );
    assert_eq!(counters.deferrals_held, 0, "the buffer is empty again");
    assert_eq!(counters.frames_recovered, 0, "none of them was ever folded");
    assert!(ledger_balances(&witness));
}

#[test]
fn a_watch_whose_first_frame_is_lost_repairs_rather_than_going_blind() {
    // The whole of P4's coverage deficit, and it is not the deferral path: at
    // 32 peers under the criterion's impairment profile every peer's coverage
    // came out at exactly k/7 of the timeline it was shown, k whole watches out
    // of the seven each peer keeps. A watch judges its subject's entire hour or
    // none of it, and what decides which is whether the *first* frame after the
    // anchor arrived.
    //
    // Why it used to be none of it: until a frame lands there is no verified
    // head, so the signature preimage is rebuilt from the anchor claim's head.
    // A frame that does not chain to it therefore fails its signature check
    // rather than its chain check — a rejection, not a gap. No repair is asked
    // for, the head never moves, and every frame after it fails identically for
    // the rest of the session. `try_reanchor` cannot save it either: resuming
    // needs a `Catchup`, and no `Catchup` was ever opened.
    //
    // The anchor's head is signed by the subject, which is exactly the argument
    // `try_reanchor` already makes for trusting the head it resumes on. A watch
    // can therefore be checked from its first frame, and a first frame that
    // does not chain is what it looks like: a hole.
    let mut authority = Authority::new(1);
    let mut witness = watching(WitnessConfig::default(), &mut authority);
    let mut log = AuthorityLog::default();

    // The frame that would have chained off the anchor never arrives.
    let lost = authority.send(T0);
    log.record_frame(
        lost.frame.clone(),
        vec![HeadTransition {
            entity: ENTITY,
            prev_head: ChainHash::EMPTY,
            head: ChainHash::EMPTY,
        }],
    );

    let second = authority.send(T0 + 3);
    let opened = witness
        .ingest_frame(&second.frame, &[])
        .expect("a frame that does not chain to a signed anchor is a hole, not a forgery");
    let [WitnessSignal::Gap(request)] = opened.as_slice() else {
        panic!("expected one gap, got {opened:?}");
    };
    let request = request.clone();
    assert_eq!(
        request.from_tick.0, T0,
        "the repair starts at the anchor, which is the earliest point it can"
    );
    assert_eq!(
        witness.counters().frames_rejected,
        0,
        "and it was not refused: refusing it is what used to end the watch"
    );
    assert_eq!(witness.counters().watches_unanchored, 1, "still blind");

    let served = log.serve_range(&request, usize::MAX);
    witness
        .ingest_wire_frames(&served.response.frames, &served.heads)
        .expect("the repair is well formed");

    assert_eq!(
        witness.counters().watches_unanchored,
        0,
        "the watch anchored off the repair rather than spending the run blind"
    );
    assert!(
        witness.catching_up(ENTITY).is_none(),
        "and the hole is closed"
    );

    // And it judges: the frame that revealed the hole was held, not thrown
    // away, so both it and the repaired one were re-executed.
    assert_eq!(
        witness.counters().judged_ticks,
        6,
        "two three-tick frames, neither of them lost to the anchor"
    );
    assert!(ledger_balances(&witness));
}
