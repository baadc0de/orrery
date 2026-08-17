//! Producer meets verifier: an authority runs, retains, and serves a window
//! that an adjudicator then judges — with no hand-assembled fixtures anywhere.
//!
//! `adjudication.rs` builds bundles by hand to isolate each verdict path. That
//! is the right shape for testing the *verifier*, and the wrong shape for
//! testing whether the two halves agree: a fixture proves the verifier accepts
//! what the test author believed an authority produces. This file removes the
//! author from the loop. Everything the adjudicator sees comes out of
//! [`AuthorityLog::assemble_bundle`].

use orrery_core::log::{claim_hash, fold_all, sign_claim, sign_frame, HeadTransition};
use orrery_core::{
    state_hash, verify_bundle, AuthorityLog, BundleError, CodecError, CoreCodec, Executor,
    OrderedInputs, QPos, QVel, Quantized, Retention, Ruleset, StateView, StepOutput, TickRng,
};
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick, UnadjudicableReason, UniverseSeed, Verdict,
};
use rand_chacha::rand_core::RngCore;

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
struct Thrust(i64);

impl CoreCodec for Thrust {
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
    version: 4,
    digest: [0x1D; 32],
};

impl Ruleset for Kinematic {
    type CoreState = Body;
    type CoreInput = Thrust;
    type CoreEvent = Nothing;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        inputs: &OrderedInputs<'_, Thrust>,
        rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        let mut applied = 0i64;
        for (index, thrust) in inputs.iter().enumerate() {
            applied += thrust.0 * (index as i64 + 1);
        }
        let state = view.own_mut();
        state.vel.x += applied;
        state.pos.x += state.vel.x;
        state.entropy = state.entropy.wrapping_add(rng.next_u32());
        StepOutput::default()
    }
}

const ENTITY: PersistId = PersistId::new(1_001);
const SEED: UniverseSeed = UniverseSeed([0x33; 32]);
const T0: u64 = 900;
/// Claims every 30 ticks (2 Hz), per docs/06 §6.
const CLAIM_EVERY: u64 = 30;

fn key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[11; 32])
}

fn body() -> Body {
    Body {
        pos: QPos::default(),
        vel: QVel::default(),
        entropy: 0,
    }
}

fn inputs_at(tick: u64) -> Vec<Thrust> {
    match tick % 5 {
        0 => vec![Thrust(2)],
        3 => vec![Thrust(-1), Thrust(4)],
        _ => Vec::new(),
    }
}

/// Run an authority for `ticks` ticks, retaining everything a dispute needs.
///
/// This is the shape a real authority has: execute at 60 Hz, batch three ticks
/// into a frame at 20 Hz, claim every 30 ticks, retain.
fn run_authority(ticks: u64, retention: Retention) -> (AuthorityLog, Executor<Kinematic>) {
    run_authority_inner(ticks, retention, false)
}

/// As [`run_authority`], but the producer signs `T0` twice before it starts —
/// an anchor claim and then the run loop's first claim, the p1-swarm shape.
fn run_authority_double_signing_t0(
    ticks: u64,
    retention: Retention,
) -> (AuthorityLog, Executor<Kinematic>) {
    run_authority_inner(ticks, retention, true)
}

fn run_authority_inner(
    ticks: u64,
    retention: Retention,
    double_sign_t0: bool,
) -> (AuthorityLog, Executor<Kinematic>) {
    let authority = key();
    let mut executor = Executor::new(Kinematic, SEED);
    executor.insert(ENTITY, body());
    let mut log = AuthorityLog::new(retention);

    // Claims chain: each commits to the hash of the one before it, so an
    // authority cannot rewrite its own history one claim at a time even when
    // every individual signature is valid.
    let mut previous_claim = [0u8; 32];
    let claim_at = |log: &mut AuthorityLog,
                    executor: &Executor<Kinematic>,
                    tick: u64,
                    previous: &mut [u8; 32]| {
        let state = executor.state(ENTITY).expect("entity present");
        let snapshot = state.to_canonical();
        let mut claim = StateClaim {
            entity: ENTITY,
            chain_epoch: 0,
            tick: Tick::new(tick),
            input_head: log.head(ENTITY),
            state_hash: state_hash(state),
            prev_claim: *previous,
            ruleset: RULESET,
            sig: authority.sign(b"unsigned"),
        };
        sign_claim(&authority, &mut claim);
        *previous = claim_hash(&claim);
        log.record_claim(claim, snapshot);
    };

    claim_at(&mut log, &executor, T0, &mut previous_claim);
    if double_sign_t0 {
        // The second claim commits to the same state at the same tick — the
        // executor has not stepped — and differs only in `prev_claim`, which
        // now names the anchor. A witness receives it with no snapshot of its
        // own, because it already holds the anchor's.
        let state = executor.state(ENTITY).expect("entity present");
        let mut duplicate = StateClaim {
            entity: ENTITY,
            chain_epoch: 0,
            tick: Tick::new(T0),
            input_head: log.head(ENTITY),
            state_hash: state_hash(state),
            prev_claim: previous_claim,
            ruleset: RULESET,
            sig: authority.sign(b"unsigned"),
        };
        sign_claim(&authority, &mut duplicate);
        previous_claim = claim_hash(&duplicate);
        log.record_claim(duplicate, Vec::new());
    }

    let mut tick = T0;
    while tick < T0 + ticks {
        let first_tick = tick;
        let mut records = Vec::new();
        let prev_head = log.head(ENTITY);

        for offset in 0..3u64 {
            let at = first_tick + offset;
            let inputs = inputs_at(at);
            for (seq, thrust) in inputs.iter().enumerate() {
                let record = InputRecord {
                    tick_off: offset as u16,
                    seq: seq as u16,
                    source: RecordSource::Player {
                        node: authority.public(),
                        input_seq: (at * 8 + seq as u64) as u32,
                    },
                    payload: bytes::Bytes::from(thrust.to_canonical()),
                };
                log.append(ENTITY, &record);
                records.push(record);
            }
            let outcome = executor
                .step_entity(ENTITY, Tick::new(at), &inputs)
                .expect("entity present");
            log.record_tick_hash(ENTITY, Tick::new(at), outcome.state_hash);
        }

        let head = log.head(ENTITY);
        let transitions = vec![HeadTransition {
            entity: ENTITY,
            prev_head,
            head,
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
                head: head.rolling(),
            }],
            sig: sign_frame(&authority, RULESET, Tick::new(first_tick), 3, &transitions),
        };
        log.record_frame(frame, transitions);

        tick += 3;
        if (tick - T0).is_multiple_of(CLAIM_EVERY) {
            claim_at(&mut log, &executor, tick, &mut previous_claim);
        }
    }
    (log, executor)
}

#[test]
fn an_authority_can_answer_for_itself_without_a_hand_built_bundle() {
    // The property that makes the crate usable rather than merely correct: an
    // authority that has been running normally can be asked to justify a
    // window, and the bundle it produces satisfies the verifier.
    let (log, _) = run_authority(90, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 60));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Exonerates
    );
}

#[test]
fn a_window_starting_at_a_later_claim_is_also_servable() {
    // Disputes do not politely start at the beginning of history. Any retained
    // claim has to be a usable t₀, or most real windows would be unservable.
    let (log, _) = run_authority(120, Retention::default());
    let window = (Tick::new(T0 + 60), Tick::new(T0 + 90));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    assert_eq!(bundle.t0_claim.tick, Tick::new(T0 + 60));
    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Exonerates
    );
}

#[test]
fn a_witness_that_replays_the_same_window_reaches_the_same_hashes() {
    // The witness side of the same coin: a second party, given only the served
    // bundle and the same ruleset build, computes the authority's trajectory
    // rather than having to be told it.
    let (log, _) = run_authority(60, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 30));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");

    // Independent re-execution, sharing nothing with the authority but the
    // ruleset, the seed, and the logged inputs.
    let mut witness = Executor::new(Kinematic, SEED);
    witness.insert(ENTITY, body());
    let recomputed: Vec<_> = (T0..T0 + 30)
        .map(|tick| {
            witness
                .step_entity(ENTITY, Tick::new(tick), &inputs_at(tick))
                .expect("entity present")
                .state_hash
        })
        .collect();

    assert_eq!(claimed, recomputed);
    let bundle = log
        .assemble_bundle(ENTITY, window, recomputed)
        .expect("window is servable");
    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Exonerates
    );
}

#[test]
fn a_forged_trajectory_in_a_real_bundle_is_confirmed() {
    // The accusation path, end to end. Everything is genuine except the state
    // the authority *signed* for the closing claim — which is the only thing
    // that can convict it, since `claimed_hashes` and `computed_hashes` are
    // the reporter's own numbers and carry no signature.
    let (log, _) = run_authority(60, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 30));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let mut bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    let mut falsified = bundle.disputed_claims[0].clone();
    falsified.state_hash = [0xEE; 32];
    sign_claim(&key(), &mut falsified);
    let at = falsified.tick;
    bundle.disputed_claims = vec![falsified];

    assert!(matches!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Confirms { at: first, .. } if first == at
    ));
}

#[test]
fn fabricating_the_advisory_hashes_convicts_nobody() {
    // The same bundle, with the reporter's advisory numbers replaced wholesale.
    // An honest authority has to survive that, or a witness could strike anyone
    // it disliked by filing well-formed nonsense.
    let (log, _) = run_authority(60, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 30));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let mut bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");
    for hash in &mut bundle.claimed_hashes {
        *hash = [0x11; 32];
    }
    for hash in &mut bundle.computed_hashes {
        *hash = [0x22; 32];
    }

    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Exonerates
    );
}

#[test]
fn a_window_older_than_retention_is_unservable_rather_than_wrong() {
    // The retention edge is where a naive store would quietly serve a partial
    // window and produce a verdict about a trajectory it did not have. Refusing
    // is the only honest answer, and it maps to `Unadjudicable` — never a
    // strike, because the gap is the cluster's, not the reporter's.
    let (mut log, _) = run_authority(600, Retention { ticks: 180 });
    log.prune(Tick::new(T0 + 600));

    assert!(matches!(
        log.assemble_bundle(
            ENTITY,
            (Tick::new(T0), Tick::new(T0 + 30)),
            vec![[0; 32]; 30]
        ),
        Err(BundleError::NoClaimAtStart | BundleError::IncompleteFrames)
    ));
}

#[test]
fn a_retained_window_still_verifies_after_pruning() {
    // Pruning must not corrupt what it keeps. A store that dropped a frame the
    // remaining window still needed would turn honest history into an
    // unadjudicable gap.
    let (mut log, _) = run_authority(600, Retention { ticks: 180 });
    let now = T0 + 600;
    log.prune(Tick::new(now));

    let window = (Tick::new(now - 30), Tick::new(now));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("recent window is retained");
    let bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("recent window is servable");
    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Exonerates
    );
}

#[test]
fn the_bundle_carries_the_claims_inside_its_window_and_they_chain_forward() {
    let (log, _) = run_authority(90, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 60));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    // Claims at +30 and +60 fall inside; the t₀ claim is carried separately.
    assert_eq!(bundle.disputed_claims.len(), 2);
    assert!(bundle
        .disputed_claims
        .iter()
        .all(|claim| claim.tick > window.0 && claim.tick <= window.1));
}

#[test]
fn a_tampered_frame_in_a_served_bundle_is_caught_by_the_chain() {
    // A bundle is only as good as its chain. Editing a record after the fact
    // moves the fold, and the frame signature no longer covers the heads —
    // which is incompleteness, not a deviation verdict about the subject.
    let (log, _) = run_authority(60, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 30));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let mut bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    let slice = &mut bundle.frames[0].entities[0];
    if let Some(record) = slice.records.first_mut() {
        record.payload = bytes::Bytes::from(Thrust(9_999).to_canonical());
    }

    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}

#[test]
fn the_retained_head_matches_folding_the_frames_that_were_sent() {
    // The store's running head and the frames it emitted have to describe the
    // same chain; if they drifted, every bundle would be self-inconsistent.
    let (log, _) = run_authority(30, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 30));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    let mut head = ChainHash::EMPTY;
    for frame in &bundle.frames {
        for slice in &frame.entities {
            head = fold_all(head, &slice.records);
        }
    }
    assert_eq!(head, log.head(ENTITY));
}

#[test]
fn a_tick_signed_twice_does_not_convict_the_authority_that_signed_it() {
    // p1-swarm's own bug, seen from the store side: the anchor at T0 and the
    // run loop's first claim at T0 are two signed claims at one tick, and every
    // later claim chains from the second. `assemble_bundle` used to take the
    // *first* claim it held at `window_start`, so the claim at T0+30 chained
    // from a hash the bundle never mentions, `verify_bundle` found the break,
    // and an authority that had executed the window correctly was convicted of
    // a `DiscreteMismatch` it did not commit. The producer bug is real and
    // fixed in p1-swarm; the store must not turn it into a false conviction for
    // the next producer that makes it.
    let (log, _) = run_authority_double_signing_t0(90, Retention::default());
    let window = (Tick::new(T0), Tick::new(T0 + 60));
    let claimed = log
        .claimed_hashes(ENTITY, window)
        .expect("window is retained");
    let bundle = log
        .assemble_bundle(ENTITY, window, claimed)
        .expect("window is servable");

    assert_eq!(
        verify_bundle(Kinematic, SEED, key().public(), &bundle),
        Verdict::Exonerates,
        "an honest trajectory must not be convicted by its own evidence store"
    );

    // Chain-consistent by construction: the t0 claim the bundle carries is the
    // one the rest of the retained claims actually chain from.
    let mut previous = claim_hash(&bundle.t0_claim);
    for claim in &bundle.disputed_claims {
        assert_eq!(claim.prev_claim, previous, "bundle claims must chain");
        previous = claim_hash(claim);
    }
}
