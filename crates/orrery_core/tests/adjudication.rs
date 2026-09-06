//! End-to-end adjudication: an authority produces a signed window, and a
//! second party holding only the same `Ruleset` build reaches a verdict.
//!
//! This is the property the whole crate exists for. Each test drives the
//! *complete* path — execute, log, sign, claim, bundle, replay, compare —
//! rather than any single stage, because the failure that matters is two
//! honest parties disagreeing, and no unit test of a stage can show that.

use std::collections::BTreeMap;

use orrery_core::log::{claim_hash, fold_all, sign_claim, sign_frame, HeadTransition};
use orrery_core::{
    verify_bundle, CodecError, CoreCodec, Executor, OrderedInputs, QPos, QVel, Quantized, Ruleset,
    StateView, StepOutput, TickRng,
};
use orrery_protocol::{
    ChainHash, DeviationKind, DiscrepancyReport, EntitySlice, EvidenceBundle, ForgeryProof,
    InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim, Tick,
    UnadjudicableReason, UniverseSeed, Verdict,
};
use rand_chacha::rand_core::RngCore;

// ── A minimal ruleset ────────────────────────────────────────────────────
//
// Kinematic movement plus one integer counter fed by the seeded RNG: enough
// to exercise VC-2 (input order matters), VC-3 (randomness is reproducible)
// and VC-7 (state is quantized) without inventing a game.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

struct Peeking;

const RULESET: RulesetId = RulesetId {
    version: 1,
    digest: [0xAB; 32],
};

impl Ruleset for Kinematic {
    const OVERFLOW_IS_CANONICAL: bool = false;
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

impl Ruleset for Peeking {
    const OVERFLOW_IS_CANONICAL: bool = false;
    type CoreState = Body;
    type CoreInput = Thrust;
    type CoreEvent = Nothing;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn max_neighbor_reads(&self) -> usize {
        1
    }

    fn max_neighbor_staleness_ticks(&self) -> u64 {
        5
    }

    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        _inputs: &OrderedInputs<'_, Thrust>,
        _rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        if let Some(neighbor) = view.neighbor(PersistId::new(78)) {
            let entropy = neighbor.entropy;
            view.own_mut().entropy = entropy;
        }
        StepOutput::default()
    }
}

// ── The authority side ───────────────────────────────────────────────────

const ENTITY: PersistId = PersistId::new(77);
const SEED: UniverseSeed = UniverseSeed([0x5A; 32]);
const T0: u64 = 6_000;
const WINDOW: u64 = 12;

fn key(seed: u8) -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[seed; 32])
}

fn body() -> Body {
    Body {
        pos: QPos::default(),
        vel: QVel::default(),
        entropy: 0,
    }
}

/// One tick's inputs, as the authority scheduled them.
fn inputs_at(tick: u64) -> Vec<Thrust> {
    match tick % 4 {
        0 => vec![Thrust(3)],
        2 => vec![Thrust(-1), Thrust(2)],
        _ => Vec::new(),
    }
}

/// What an authority produces over a window: the t₀ snapshot and claim, the
/// signed frames, and its asserted per-tick trajectory.
struct Produced {
    t0_claim: StateClaim,
    t0_snapshot: Vec<u8>,
    frames: Vec<LogFrame>,
    frame_heads: Vec<ChainHash>,
    claimed_hashes: Vec<[u8; 32]>,
    tick_snapshots: Vec<Vec<u8>>,
    end_claim: StateClaim,
}

fn produce(authority: &iroh_base::SecretKey) -> Produced {
    let mut executor = Executor::new(Kinematic, SEED);
    executor.insert(ENTITY, body());

    let t0_snapshot = executor.state(ENTITY).expect("seeded").to_canonical();
    let mut t0_claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0),
        input_head: ChainHash::EMPTY,
        state_hash: *blake3::hash(&t0_snapshot).as_bytes(),
        prev_claim: [0; 32],
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(authority, &mut t0_claim);

    // One frame per three ticks: 60 Hz simulation, 20 Hz send.
    let mut frames = Vec::new();
    let mut frame_heads = Vec::new();
    let mut claimed_hashes = Vec::new();
    let mut tick_snapshots = Vec::new();
    let mut head = ChainHash::EMPTY;

    for frame_index in 0..(WINDOW / 3) {
        let first_tick = T0 + frame_index * 3;
        let mut records = Vec::new();
        for offset in 0..3u64 {
            let tick = first_tick + offset;
            for (seq, thrust) in inputs_at(tick).into_iter().enumerate() {
                records.push(InputRecord {
                    tick_off: offset as u16,
                    seq: seq as u16,
                    source: RecordSource::Player {
                        node: authority.public(),
                        input_seq: (tick * 10 + seq as u64) as u32,
                    },
                    payload: bytes::Bytes::from(thrust.to_canonical()),
                });
            }
            let outcome = executor
                .step_entity(ENTITY, Tick::new(tick), &inputs_at(tick))
                .expect("entity present");
            claimed_hashes.push(outcome.state_hash);
            tick_snapshots.push(
                executor
                    .state(ENTITY)
                    .expect("entity present")
                    .to_canonical(),
            );
        }

        let prev_head = head;
        head = fold_all(prev_head, &records);
        let slice = EntitySlice {
            entity: ENTITY,
            chain_epoch: 0,
            prev_head: prev_head.rolling(),
            records,
            head: head.rolling(),
        };
        let transitions = [HeadTransition {
            entity: ENTITY,
            prev_head,
            head,
        }];
        frames.push(LogFrame {
            ruleset: RULESET,
            first_tick: Tick::new(first_tick),
            tick_count: 3,
            entities: vec![slice],
            sig: sign_frame(authority, RULESET, Tick::new(first_tick), 3, &transitions),
        });
        frame_heads.push(head);
    }

    let mut end_claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0 + WINDOW),
        input_head: head,
        state_hash: *blake3::hash(
            &executor
                .state(ENTITY)
                .expect("entity present")
                .to_canonical(),
        )
        .as_bytes(),
        prev_claim: claim_hash(&t0_claim),
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(authority, &mut end_claim);

    Produced {
        t0_claim,
        t0_snapshot,
        frames,
        frame_heads,
        claimed_hashes,
        tick_snapshots,
        end_claim,
    }
}

fn claim_at(
    produced: &Produced,
    authority: &iroh_base::SecretKey,
    offset: u64,
    previous: &StateClaim,
) -> StateClaim {
    assert!(offset > 0 && offset <= WINDOW && offset.is_multiple_of(3));
    let mut claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0 + offset),
        input_head: produced.frame_heads[(offset / 3 - 1) as usize],
        state_hash: produced.claimed_hashes[(offset - 1) as usize],
        prev_claim: claim_hash(previous),
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(authority, &mut claim);
    claim
}

fn bundle(produced: &Produced) -> EvidenceBundle {
    EvidenceBundle {
        ruleset: RULESET,
        entity: ENTITY,
        window_start: Tick::new(T0),
        window_end: Tick::new(T0 + WINDOW),
        t0_claim: produced.t0_claim.clone(),
        t0_snapshot: bytes::Bytes::from(produced.t0_snapshot.clone()),
        frames: produced.frames.clone(),
        // One authored entity, so no siblings to reconstruct.
        sibling_heads: vec![Vec::new(); produced.frames.len()],
        disputed_claims: vec![produced.end_claim.clone()],
        claimed_hashes: produced.claimed_hashes.clone(),
        computed_hashes: produced.claimed_hashes.clone(),
    }
}

fn signed_report(
    reporter: &iroh_base::SecretKey,
    subject: &iroh_base::SecretKey,
    bundle: EvidenceBundle,
) -> DiscrepancyReport {
    // The report signature contract from orrery_witness: domain, subject,
    // reporter, then the canonical bundle digest. Keeping the probe here avoids
    // a dependency cycle from core's tests back through witness.
    let reporter_id = reporter.public();
    let encoded = postcard::to_stdvec(&bundle).expect("bundle encodes");
    let mut preimage = Vec::new();
    preimage.extend_from_slice(b"orrery/discrepancy-report/v1");
    preimage.extend_from_slice(subject.public().as_bytes());
    preimage.extend_from_slice(reporter_id.as_bytes());
    preimage.extend_from_slice(blake3::hash(&encoded).as_bytes());
    DiscrepancyReport {
        subject: subject.public(),
        bundle,
        reporter: reporter_id,
        reporter_sig: reporter.sign(&preimage),
    }
}

// ── The tests ────────────────────────────────────────────────────────────

#[test]
fn an_honest_window_exonerates() {
    // The baseline that has to hold before any accusation means anything: a
    // second party, given only the bundle and the same ruleset build,
    // re-executes and agrees.
    let authority = key(1);
    let produced = produce(&authority);
    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle(&produced)),
        Verdict::Exonerates
    );
}

#[test]
fn an_adjudicator_is_a_pure_function_of_the_bundle() {
    // Same bundle, same verdict, every time and for anyone. If this were not
    // so, the cluster could not adjudicate without trusting the reporter.
    let authority = key(1);
    let produced = produce(&authority);
    let bundle = bundle(&produced);
    let once = verify_bundle(Kinematic, SEED, authority.public(), &bundle);
    let twice = verify_bundle(Kinematic, SEED, authority.public(), &bundle);
    assert_eq!(once, twice);
    assert_eq!(once, Verdict::Exonerates);
}

#[test]
fn a_signed_claim_the_inputs_do_not_produce_is_confirmed() {
    // The accusation case, and note what it takes: the authority has to have
    // *signed* a state its own logged inputs do not produce. Nothing weaker
    // convicts, because nothing weaker is attributable to it.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    let mut falsified = bundle.disputed_claims[0].clone();
    falsified.state_hash = [0xFF; 32];
    sign_claim(&authority, &mut falsified);
    bundle.disputed_claims = vec![falsified];

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Confirms {
            at: Tick::new(T0 + WINDOW),
            kind: DeviationKind::DiscreteMismatch,
        }
    );
}

#[test]
fn a_reporter_cannot_convict_an_honest_peer_by_inventing_a_trajectory() {
    // The property that makes a bundle self-verifying rather than merely
    // detailed. `claimed_hashes` and `computed_hashes` are the reporter's own
    // numbers; the subject never signs them. A verdict that rested on them
    // would let anyone strike anyone.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    for hash in &mut bundle.claimed_hashes {
        *hash = [0xAB; 32];
    }
    for hash in &mut bundle.computed_hashes {
        *hash = [0xCD; 32];
    }

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Exonerates,
        "an honest authority must survive a reporter that fabricated the hint"
    );
}

#[test]
fn a_window_with_no_signed_claim_proves_nothing() {
    // A window that ends nowhere near a claim contains no assertion the
    // subject can be held to. That is undecidable, not exoneration and not
    // guilt — and it is why docs/06 §7 requires a window to end at a claim.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    bundle.disputed_claims.clear();

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}

#[test]
fn dropping_an_input_from_the_log_breaks_the_chain_rather_than_changing_the_answer() {
    // An authority cannot quietly rewrite what it was told: removing a record
    // moves the fold, so the frame signature no longer covers the heads.
    // Crucially this is *not* a deviation verdict — the evidence is incomplete,
    // and an adjudicator that guessed here would be inventing a fact.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    bundle.frames[0].entities[0].records.pop();

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}

#[test]
fn a_reporter_that_forges_a_signature_is_the_one_struck() {
    // The asymmetry that keeps the pipeline honest in both directions: proof
    // of fabrication strikes the reporter, and nothing else does.
    let authority = key(1);
    let impostor = key(2);
    let produced = produce(&impostor);
    let bundle = bundle(&produced);

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::EvidenceForged(ForgeryProof::ClaimSignatureInvalid)
    );
}

#[test]
fn a_snapshot_that_does_not_match_its_claim_is_forgery_not_deviation() {
    // Replay from an unclaimed starting point would measure nothing real, so
    // the check happens before any simulation runs.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    // Not all-zeroes: the honest starting body encodes to 52 zero bytes, so
    // that "tampered" snapshot would be the real one and the test vacuous.
    bundle.t0_snapshot = bytes::Bytes::from(vec![0xFFu8; 52]);

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::EvidenceForged(ForgeryProof::SnapshotHashMismatch)
    );
}

#[test]
fn a_bundle_for_another_rules_build_is_undecidable_not_a_strike() {
    // Rules-version skew is a cluster-side gap, never the reporter's fault
    // (D11 retains three builds; older bundles simply cannot be judged).
    struct OtherBuild;
    impl Ruleset for OtherBuild {
        const OVERFLOW_IS_CANONICAL: bool = false;
        type CoreState = Body;
        type CoreInput = Thrust;
        type CoreEvent = Nothing;
        fn id(&self) -> RulesetId {
            RulesetId {
                version: 2,
                digest: [0xCD; 32],
            }
        }
        fn step(
            &self,
            _view: &mut StateView<'_, Body>,
            _inputs: &OrderedInputs<'_, Thrust>,
            _rng: &mut TickRng,
        ) -> StepOutput<Nothing> {
            StepOutput::default()
        }
    }

    let authority = key(1);
    let produced = produce(&authority);
    assert_eq!(
        verify_bundle(OtherBuild, SEED, authority.public(), &bundle(&produced)),
        Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
    );
}

#[test]
fn an_oversized_window_is_refused_before_it_is_replayed() {
    // The 3 s ceiling is what bounds adjudication cost; a bundle asking for
    // more is refused rather than served.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    bundle.window_end = Tick::new(T0 + 5_000);

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::WindowOutOfRange)
    );
}

#[test]
fn omitting_an_intermediate_signed_claim_cannot_convict() {
    // Every presented signature is genuine. The missing predecessor could be
    // either reporter omission or authority equivocation, and the bundle alone
    // cannot distinguish them, so a verdict about the authority is impossible.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    let first = claim_at(&produced, &authority, 3, &produced.t0_claim);
    let omitted = claim_at(&produced, &authority, 6, &first);
    let end = claim_at(&produced, &authority, WINDOW, &omitted);
    bundle.disputed_claims = vec![first, end];

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}

#[test]
fn two_distinct_signed_claims_at_one_tick_convict_the_equivocating_authority() {
    // Unlike a lone broken link, this pair needs no missing history to explain
    // it: the authority signed two incompatible assertions for one claim tick.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    let first = claim_at(&produced, &authority, WINDOW, &produced.t0_claim);
    let mut conflicting = first.clone();
    conflicting.state_hash = [0xE1; 32];
    sign_claim(&authority, &mut conflicting);
    bundle.disputed_claims = vec![first, conflicting];

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Confirms {
            at: Tick::new(T0 + WINDOW),
            kind: DeviationKind::DiscreteMismatch,
        }
    );
}

#[test]
fn the_swarm_duplicate_anchor_shape_is_incomplete_at_the_adjudicator() {
    // gates/p1-swarm once signed the anchor tick twice, then chained later
    // claims from the run-loop duplicate. Presenting the older anchor while
    // withholding that duplicate creates the exact broken link that used to
    // convict honest bots. This drives verify_bundle directly, bypassing the
    // store's chain-aware assembler fix.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    let mut duplicate = produced.t0_claim.clone();
    duplicate.prev_claim = claim_hash(&produced.t0_claim);
    sign_claim(&authority, &mut duplicate);
    let end = claim_at(&produced, &authority, WINDOW, &duplicate);
    bundle.disputed_claims = vec![end];

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}

#[test]
fn reslicing_one_divergence_stacks_strike_dedup_keys() {
    // One bad end claim can be replayed from either of two genuine anchors.
    // Both reports therefore convict the same signed divergence. Persistd's
    // duplicate key is (BLAKE3(postcard(report)), StrikeKind); if these report
    // digests differ, the current ledger admits both major-strike rows.
    let authority = key(1);
    let reporter = key(2);
    let produced = produce(&authority);
    let middle = claim_at(&produced, &authority, 3, &produced.t0_claim);
    let mut bad_end = claim_at(&produced, &authority, WINDOW, &middle);
    bad_end.state_hash = [0xD1; 32];
    sign_claim(&authority, &mut bad_end);

    let mut whole = bundle(&produced);
    whole.disputed_claims = vec![middle.clone(), bad_end.clone()];

    let mut tail = whole.clone();
    tail.window_start = middle.tick;
    tail.t0_claim = middle;
    tail.t0_snapshot = bytes::Bytes::from(produced.tick_snapshots[2].clone());
    tail.frames.remove(0);
    tail.sibling_heads.remove(0);
    tail.disputed_claims = vec![bad_end];
    tail.claimed_hashes.drain(..3);
    tail.computed_hashes.drain(..3);

    let expected = Verdict::Confirms {
        at: Tick::new(T0 + WINDOW),
        kind: DeviationKind::DiscreteMismatch,
    };
    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &whole),
        expected
    );
    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &tail),
        expected
    );

    let whole = signed_report(&reporter, &authority, whole);
    let tail = signed_report(&reporter, &authority, tail);
    let digest = |report: &DiscrepancyReport| {
        let encoded = postcard::to_stdvec(report).expect("report encodes");
        *blake3::hash(&encoded).as_bytes()
    };
    let whole_key = (digest(&whole), "major");
    let tail_key = (digest(&tail), "major");
    assert_ne!(
        whole_key, tail_key,
        "reporter-chosen slices bypass (whole-report digest, strike kind) deduplication"
    );
}

#[test]
fn replaying_with_a_different_universe_seed_diverges() {
    // VC-3 in the negative: the seed is load-bearing. An adjudicator using the
    // wrong universe seed must not silently exonerate.
    let authority = key(1);
    let produced = produce(&authority);
    assert!(matches!(
        verify_bundle(
            Kinematic,
            UniverseSeed([0x99; 32]),
            authority.public(),
            &bundle(&produced)
        ),
        Verdict::Confirms { .. }
    ));
}

#[test]
fn two_independent_executions_of_the_same_window_agree_exactly() {
    // §8's golden-state rule at window scale: a full re-run in the same process
    // that diverged would be an instant VC-4/VC-8 violation — hash iteration
    // order, address hashing, or an ambient read.
    let authority = key(1);
    let first = produce(&authority);
    let second = produce(&authority);
    assert_eq!(first.claimed_hashes, second.claimed_hashes);
    assert_eq!(first.t0_snapshot, second.t0_snapshot);
    assert_eq!(first.end_claim.state_hash, second.end_claim.state_hash);
}

#[test]
fn a_recorded_neighbour_frame_replays_without_a_live_world() {
    let mut executor = Executor::new(Kinematic, SEED);
    executor.insert(ENTITY, body());
    executor.insert(PersistId::new(78), body());

    let mut neighbor = body();
    neighbor.entropy = 42;
    let record = InputRecord {
        tick_off: 0,
        seq: 0,
        source: RecordSource::NeighborFrame {
            neighbor: PersistId::new(78),
            present: true,
            observed_tick: Tick::new(T0),
        },
        payload: bytes::Bytes::from(neighbor.to_canonical()),
    };
    let head = fold_all(ChainHash::EMPTY, std::slice::from_ref(&record));
    let authority = key(1);
    let transitions = [HeadTransition {
        entity: ENTITY,
        prev_head: ChainHash::EMPTY,
        head,
    }];
    let frame = LogFrame {
        ruleset: RULESET,
        first_tick: Tick::new(T0),
        tick_count: 1,
        entities: vec![EntitySlice {
            entity: ENTITY,
            chain_epoch: 0,
            prev_head: ChainHash::EMPTY.rolling(),
            records: vec![record],
            head: head.rolling(),
        }],
        sig: sign_frame(&authority, RULESET, Tick::new(T0), 1, &transitions),
    };

    let snapshot = body().to_canonical();
    let mut t0_claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0),
        input_head: ChainHash::EMPTY,
        state_hash: *blake3::hash(&snapshot).as_bytes(),
        prev_claim: [0; 32],
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(&authority, &mut t0_claim);

    let mut reference = Executor::new(Peeking, SEED);
    reference.insert(ENTITY, body());
    reference.insert_observed(PersistId::new(78), neighbor, Tick::new(T0));
    let expected = reference
        .step_entity(ENTITY, Tick::new(T0), &[])
        .expect("entity present")
        .state_hash;

    // A claim at T0+1 commits to the state after T0 executed. Without one the
    // window carries nothing signed, and nothing signed means nothing to judge.
    let mut end_claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0 + 1),
        input_head: head,
        state_hash: expected,
        prev_claim: claim_hash(&t0_claim),
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(&authority, &mut end_claim);

    let verdict = verify_bundle(
        Peeking,
        SEED,
        authority.public(),
        &EvidenceBundle {
            ruleset: RULESET,
            entity: ENTITY,
            window_start: Tick::new(T0),
            window_end: Tick::new(T0 + 1),
            t0_claim,
            t0_snapshot: bytes::Bytes::from(snapshot),
            frames: vec![frame],
            sibling_heads: vec![Vec::new()],
            disputed_claims: vec![end_claim],
            claimed_hashes: vec![expected],
            computed_hashes: vec![expected],
        },
    );
    assert_eq!(verdict, Verdict::Exonerates);
}

#[test]
fn a_multi_entity_frame_needs_its_sibling_heads_to_verify() {
    // One signature covers every authored entity, so a bundle that omits the
    // siblings cannot rebuild the preimage. The honest answer is "incomplete",
    // not a verdict about the subject.
    let authority = key(1);
    let other = PersistId::new(78);

    let subject_record = InputRecord {
        tick_off: 0,
        seq: 0,
        source: RecordSource::Player {
            node: authority.public(),
            input_seq: 1,
        },
        payload: bytes::Bytes::from(Thrust(1).to_canonical()),
    };
    let sibling_record = InputRecord {
        tick_off: 0,
        seq: 0,
        source: RecordSource::Player {
            node: authority.public(),
            input_seq: 2,
        },
        payload: bytes::Bytes::from(Thrust(9).to_canonical()),
    };
    let subject_head = fold_all(ChainHash::EMPTY, std::slice::from_ref(&subject_record));
    let sibling_head = fold_all(ChainHash::EMPTY, std::slice::from_ref(&sibling_record));

    let mut slices = BTreeMap::new();
    slices.insert(
        ENTITY,
        EntitySlice {
            entity: ENTITY,
            chain_epoch: 0,
            prev_head: ChainHash::EMPTY.rolling(),
            records: vec![subject_record],
            head: subject_head.rolling(),
        },
    );
    slices.insert(
        other,
        EntitySlice {
            entity: other,
            chain_epoch: 0,
            prev_head: ChainHash::EMPTY.rolling(),
            records: vec![sibling_record],
            head: sibling_head.rolling(),
        },
    );
    let transitions: Vec<_> = slices
        .values()
        .map(|slice| HeadTransition {
            entity: slice.entity,
            prev_head: ChainHash::EMPTY,
            head: if slice.entity == ENTITY {
                subject_head
            } else {
                sibling_head
            },
        })
        .collect();
    let frame = LogFrame {
        ruleset: RULESET,
        first_tick: Tick::new(T0),
        tick_count: 1,
        entities: slices.into_values().collect(),
        sig: sign_frame(&authority, RULESET, Tick::new(T0), 1, &transitions),
    };

    let snapshot = body().to_canonical();
    let mut t0_claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0),
        input_head: ChainHash::EMPTY,
        state_hash: *blake3::hash(&snapshot).as_bytes(),
        prev_claim: [0; 32],
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(&authority, &mut t0_claim);

    let mut end_claim = StateClaim {
        entity: ENTITY,
        chain_epoch: 0,
        tick: Tick::new(T0 + 1),
        input_head: subject_head,
        // Deliberately wrong: this test is about whether the frame can be
        // verified at all, so the window is judgeable and lands on a verdict.
        state_hash: [0; 32],
        prev_claim: claim_hash(&t0_claim),
        ruleset: RULESET,
        sig: authority.sign(b"unsigned"),
    };
    sign_claim(&authority, &mut end_claim);

    let make = |sibling_heads: Vec<Vec<(ChainHash, ChainHash)>>| EvidenceBundle {
        ruleset: RULESET,
        entity: ENTITY,
        window_start: Tick::new(T0),
        window_end: Tick::new(T0 + 1),
        t0_claim: t0_claim.clone(),
        t0_snapshot: bytes::Bytes::from(snapshot.clone()),
        frames: vec![frame.clone()],
        sibling_heads,
        disputed_claims: vec![end_claim.clone()],
        claimed_hashes: vec![[0; 32]],
        computed_hashes: vec![[0; 32]],
    };

    // Without the sibling pair the preimage cannot be rebuilt.
    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &make(vec![Vec::new()])),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );

    // With it, the frame verifies and the window is judged on its merits —
    // here a deviation, because the claimed hash is a placeholder.
    assert!(matches!(
        verify_bundle(
            Kinematic,
            SEED,
            authority.public(),
            &make(vec![vec![(ChainHash::EMPTY, sibling_head)]])
        ),
        Verdict::Confirms { .. }
    ));
}

#[test]
fn omitting_a_trailing_input_frame_cannot_convict() {
    // The claim chain is complete and every signature is genuine; what the
    // reporter withheld is the *input* frame covering the last three ticks.
    // Replaying those ticks with no inputs diverges from what the subject
    // signed, so an adjudicator that does not require the frames to cover the
    // window convicts an honest authority with its own signatures.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Exonerates,
        "the untouched bundle must exonerate, or the probe proves nothing"
    );

    bundle.frames.pop().expect("frames present");
    bundle.sibling_heads.pop().expect("heads present");

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}

#[test]
fn omitting_a_leading_input_frame_cannot_convict() {
    // The same withholding at the other end of the window.
    let authority = key(1);
    let produced = produce(&authority);
    let mut bundle = bundle(&produced);
    bundle.frames.remove(0);
    bundle.sibling_heads.remove(0);

    assert_eq!(
        verify_bundle(Kinematic, SEED, authority.public(), &bundle),
        Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
    );
}
