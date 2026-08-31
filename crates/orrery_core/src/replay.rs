//! The headless replay harness and evidence self-verification (docs/06 §7).
//!
//! `verify_bundle` is the whole trust story in one function: check the
//! signatures against the authority's key, check chain continuity from the t₀
//! claim, check the snapshot hash, re-execute deterministically, compare. A
//! bundle either proves deviation to *anyone* holding the same `Ruleset` build,
//! or it proves nothing — which is why the cluster can adjudicate without
//! believing the witness that reported.
//!
//! The two failure verdicts are deliberately asymmetric. `EvidenceForged`
//! requires *proof* of fabrication and strikes the reporter; `Unadjudicable`
//! covers everything the adjudicator merely cannot decide and never strikes
//! anyone. Bogus-report pressure is meant to be absorbed by per-account rate
//! limits, not by punishing honest reporters for gaps the cluster owns.

use core::marker::PhantomData;
use std::collections::BTreeMap;

use orrery_protocol::{
    ChainHash, DeviationKind, EvidenceBundle, ForgeryProof, LogFrame, NodeId, PersistId,
    RecordSource, Tick, UnadjudicableReason, UniverseSeed, Verdict, MAX_ADJUDICATION_TICKS,
};

use crate::executor::{Executor, TickBackend};
use crate::log::{claim_hash, verify_claim, verify_frame, LogError};
use crate::ruleset::{CoreCodec, Ruleset};

type RecordedNeighbor<S> = (PersistId, Tick, Option<S>);

/// Why a replay could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The supplied snapshot does not hash to what the claim commits to.
    SnapshotHashMismatch,
    /// The snapshot bytes are not a valid `CoreState` encoding.
    SnapshotMalformed,
    /// A frame failed verification.
    Log(LogError),
    /// A logged input payload is not a valid `CoreInput` encoding.
    InputMalformed,
    /// Frames do not cover the window contiguously, or run backwards.
    NonContiguousFrames,
    /// The window is empty or longer than [`MAX_ADJUDICATION_TICKS`].
    WindowOutOfRange,
    /// Recorded neighbour inputs exceed the ruleset cap, are stale, malformed,
    /// or do not match the reads the replay performed.
    NeighborFramesMalformed,
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SnapshotHashMismatch => f.write_str("snapshot does not match the claim"),
            Self::SnapshotMalformed => f.write_str("snapshot is not a valid CoreState"),
            Self::Log(error) => write!(f, "log verification failed: {error}"),
            Self::InputMalformed => f.write_str("logged input is not a valid CoreInput"),
            Self::NonContiguousFrames => f.write_str("frames do not cover the window contiguously"),
            Self::WindowOutOfRange => f.write_str("window is empty or too long"),
            Self::NeighborFramesMalformed => f.write_str("recorded neighbour frames are malformed"),
        }
    }
}

impl core::error::Error for ReplayError {}

impl From<LogError> for ReplayError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

/// The per-tick trajectory a replay computed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayTrace {
    /// State hash after each executed tick, in tick order.
    pub hashes: Vec<(Tick, [u8; 32])>,
}

impl ReplayTrace {
    /// The computed hash at a tick, if the trace covers it.
    #[must_use]
    pub fn at(&self, tick: Tick) -> Option<[u8; 32]> {
        self.hashes
            .iter()
            .find(|(t, _)| *t == tick)
            .map(|(_, hash)| *hash)
    }
}

/// Re-executes a window from logged frames, on whatever substrate it is given.
///
/// The backend parameter is D43 clause (e)(5)'s adjudication half. A verdict
/// reached by replaying on the [`Executor`] is evidence about the `Executor`;
/// when the claims under suspicion were authored on some other
/// [`TickBackend`], the replay has to happen *there*, or "the schedule was
/// deterministic" has quietly been substituted for per-entity replay. The
/// default is `Executor` so that every existing caller keeps the substrate it
/// already had — this relocates where a replay runs and changes no canonical
/// byte.
pub struct ReplayHarness<R: Ruleset, B: TickBackend<R> = Executor<R>> {
    backend: B,
    entity: Option<PersistId>,
    /// The chain head the loaded claim commits to.
    ///
    /// A window rarely starts at the beginning of a chain, so folding from the
    /// empty head would put the replay on a different chain than the authority
    /// and report a head mismatch on the first frame. This is what
    /// `StateClaim::input_head` is *for*.
    start_head: Option<ChainHash>,
    ruleset: PhantomData<fn() -> R>,
}

impl<R: Ruleset> ReplayHarness<R, Executor<R>> {
    /// A harness over one ruleset build and universe seed, on the [`Executor`].
    pub fn new(ruleset: R, universe_seed: UniverseSeed) -> Self {
        Self::on(Executor::new(ruleset, universe_seed))
    }
}

impl<R: Ruleset, B: TickBackend<R>> ReplayHarness<R, B> {
    /// A harness over a substrate that already carries its ruleset and seed.
    ///
    /// `backend` must be empty: `load_claimed_snapshot` installs the one state
    /// the claim commits to, and a backend arriving with a population would
    /// give the replay neighbours the authority's own window did not have.
    pub fn on(backend: B) -> Self {
        Self {
            backend,
            entity: None,
            start_head: None,
            ruleset: PhantomData,
        }
    }

    /// Verify snapshot bytes against a claim, then load them.
    ///
    /// The hash check comes first and the load never happens without it: a
    /// bundle whose snapshot does not match its claim is rejected *before* any
    /// simulation runs, because everything downstream would be measuring
    /// against a starting point the authority never committed to.
    ///
    /// # Errors
    ///
    /// [`ReplayError::SnapshotHashMismatch`] or [`ReplayError::SnapshotMalformed`].
    pub fn load_claimed_snapshot(
        &mut self,
        claim: &orrery_protocol::StateClaim,
        snapshot_bytes: &[u8],
    ) -> Result<(), ReplayError> {
        if *blake3::hash(snapshot_bytes).as_bytes() != claim.state_hash {
            return Err(ReplayError::SnapshotHashMismatch);
        }
        let state =
            R::CoreState::decode(snapshot_bytes).map_err(|_| ReplayError::SnapshotMalformed)?;
        self.backend.insert(claim.entity, state);
        self.entity = Some(claim.entity);
        self.start_head = Some(claim.input_head);
        Ok(())
    }

    /// Canonical bytes for the currently installed replay state.
    ///
    /// This is the same encoding [`state_hash`](crate::state_hash) commits to.
    /// Adjudication uses it after a confirmed replay so an authority correction
    /// carries state the peer can install, rather than only a hash that proves
    /// the old authority was wrong.
    #[must_use]
    pub fn canonical_state(&self) -> Option<Vec<u8>> {
        self.entity
            .and_then(|entity| self.backend.state(entity))
            .map(CoreCodec::to_canonical)
    }

    /// Re-execute `window` from `frames`.
    ///
    /// Verifies signatures, chain continuity and input-order legality as it
    /// goes, then steps every tick in the window — including ticks with no
    /// inputs, because a tick that logged nothing still advances state and
    /// still draws from the RNG.
    ///
    /// # Errors
    ///
    /// Any [`ReplayError`]; a caller that wants a verdict rather than an error
    /// should use [`verify_bundle`].
    pub fn replay(
        &mut self,
        frames: &[LogFrame],
        authority: NodeId,
        window: (Tick, Tick),
        sibling_heads: &[Vec<(ChainHash, ChainHash)>],
    ) -> Result<ReplayTrace, ReplayError> {
        let (start, end) = window;
        let span = end.0.saturating_sub(start.0);
        if span == 0 || span > MAX_ADJUDICATION_TICKS {
            return Err(ReplayError::WindowOutOfRange);
        }
        let Some(entity) = self.entity else {
            return Err(ReplayError::SnapshotMalformed);
        };

        // Collect this entity's inputs per absolute tick, verifying each frame
        // as it is consumed. Ordering inside a tick is the log's, never ours.
        let mut per_tick: BTreeMap<u64, Vec<R::CoreInput>> = BTreeMap::new();
        let mut neighbors_per_tick: BTreeMap<u64, Vec<RecordedNeighbor<R::CoreState>>> =
            BTreeMap::new();
        // Start from the chain head the claim commits to, not from the empty
        // head: a window opened at a later claim is mid-chain, and folding from
        // zero would compare against a chain nobody has.
        let mut head = self.start_head.unwrap_or(ChainHash::EMPTY);
        let mut previous_end: Option<u64> = None;

        for (index, frame) in frames.iter().enumerate() {
            if let Some(previous) = previous_end {
                if frame.first_tick.0 != previous {
                    return Err(ReplayError::NonContiguousFrames);
                }
            }
            previous_end = Some(frame.first_tick.0 + u64::from(frame.tick_count));

            // Rebuild the frame's full incoming heads: ours from the running
            // fold, siblings from the bundle. A verifier that skipped this
            // could not check the signature at all.
            let mut prev_heads = Vec::with_capacity(frame.entities.len());
            let siblings = sibling_heads.get(index);
            let mut sibling_cursor = 0usize;
            for slice in &frame.entities {
                if slice.entity == entity {
                    prev_heads.push(head);
                } else {
                    let pair = siblings
                        .and_then(|pairs| pairs.get(sibling_cursor))
                        .ok_or(ReplayError::Log(LogError::MissingSiblingHeads))?;
                    sibling_cursor += 1;
                    prev_heads.push(pair.0);
                }
            }

            let transitions = verify_frame(frame, authority, &prev_heads)?;
            for transition in &transitions {
                if transition.entity == entity {
                    head = transition.head;
                }
            }

            for slice in &frame.entities {
                if slice.entity != entity {
                    continue;
                }
                for record in &slice.records {
                    if let RecordSource::NeighborFrame {
                        neighbor,
                        present,
                        observed_tick,
                    } = record.source
                    {
                        let reader_tick = frame.first_tick.0 + u64::from(record.tick_off);
                        let age = reader_tick.checked_sub(observed_tick.0);
                        if age.is_none_or(|age| {
                            age > self.backend.ruleset().max_neighbor_staleness_ticks()
                        }) {
                            return Err(ReplayError::NeighborFramesMalformed);
                        }
                        let state = if present {
                            Some(
                                R::CoreState::decode(&record.payload)
                                    .map_err(|_| ReplayError::NeighborFramesMalformed)?,
                            )
                        } else {
                            if !record.payload.is_empty() {
                                return Err(ReplayError::NeighborFramesMalformed);
                            }
                            None
                        };
                        let frames = neighbors_per_tick.entry(reader_tick).or_default();
                        if frames.len() >= self.backend.ruleset().max_neighbor_reads()
                            || frames.iter().any(|(found, _, _)| found == &neighbor)
                        {
                            return Err(ReplayError::NeighborFramesMalformed);
                        }
                        frames.push((neighbor, observed_tick, state));
                        continue;
                    }
                    // `NeighborFrame` and `AuthorityChange` records close the
                    // input set and mark epoch boundaries; they are not
                    // themselves `CoreInput`s to feed the step.
                    if !matches!(
                        record.source,
                        RecordSource::OwnPlayer { .. }
                            | RecordSource::Player { .. }
                            | RecordSource::InboundEvent { .. }
                    ) {
                        continue;
                    }
                    let input = R::CoreInput::decode(&record.payload)
                        .map_err(|_| ReplayError::InputMalformed)?;
                    let tick = frame.first_tick.0 + u64::from(record.tick_off);
                    per_tick.entry(tick).or_default().push(input);
                }
            }
        }

        let mut trace = ReplayTrace::default();
        for tick in start.0..end.0 {
            let inputs = per_tick.remove(&tick).unwrap_or_default();
            let neighbor_frames = neighbors_per_tick.remove(&tick).unwrap_or_default();
            for (neighbor, observed_tick, state) in &neighbor_frames {
                if *neighbor == entity {
                    if state.is_some() {
                        return Err(ReplayError::NeighborFramesMalformed);
                    }
                    continue;
                }
                if let Some(state) = state {
                    self.backend
                        .insert_observed(*neighbor, state.clone(), *observed_tick);
                }
            }
            let Some(outcome) = self.backend.step_entity(entity, Tick::new(tick), &inputs) else {
                return Err(ReplayError::SnapshotMalformed);
            };
            let expected_reads: Vec<_> = neighbor_frames
                .iter()
                .map(|(neighbor, _, _)| *neighbor)
                .collect();
            if outcome.neighbor_reads != expected_reads {
                return Err(ReplayError::NeighborFramesMalformed);
            }
            for (neighbor, _, state) in &neighbor_frames {
                if state.is_some() {
                    self.backend.take_state(*neighbor);
                }
            }
            // The adjudicator replays exactly one entity. Its emitted child
            // descriptions are reproduced by `step_entity`, but retaining
            // those children here would make them neighbours on the next tick
            // and quietly turn a closed window into a growing world.
            for materialized in &outcome.materialized {
                self.backend.take_state(*materialized);
            }
            trace.hashes.push((Tick::new(tick), outcome.state_hash));
        }
        Ok(trace)
    }
}

/// Adjudicate a bundle: a pure function of the bundle and the ruleset build.
///
/// `authority` is the peer whose signatures the bundle claims to carry. The §7
/// sketch omits it because the adjudicator always knows it from the lease row;
/// making it explicit keeps this function from having to guess which key a
/// signature was supposed to verify under, which is precisely the thing a
/// forged bundle would like it to guess.
#[must_use]
pub fn verify_bundle<R: Ruleset>(
    ruleset: R,
    universe_seed: UniverseSeed,
    authority: NodeId,
    bundle: &EvidenceBundle,
) -> Verdict {
    verify_bundle_on(Executor::new(ruleset, universe_seed), authority, bundle)
}

/// Adjudicate a bundle on a given substrate — D43 clause (e)(5)'s adjudication
/// half.
///
/// [`verify_bundle`] is this function on the [`Executor`], and that is the
/// right substrate exactly when the `Executor` is what authored the claims.
/// When something else did — an ECS world behind the seam, per D42 (d) — the
/// adjudicator that would convict it must re-execute *there*, because a
/// verdict reached on a different substrate than the one under suspicion is
/// evidence about the wrong substrate. Nothing about the rules moves with the
/// backend: the RNG, the rule call, quantization, hashing and neighbour
/// framing are all `canonical_step`, which every backend calls and none
/// reimplements, so this relocates a replay and changes no canonical byte.
///
/// `backend` carries the ruleset build and universe seed, and must be empty:
/// the bundle's own `t0_snapshot` is the only state a window may start from.
#[must_use]
pub fn verify_bundle_on<R: Ruleset, B: TickBackend<R>>(
    backend: B,
    authority: NodeId,
    bundle: &EvidenceBundle,
) -> Verdict {
    if backend.ruleset().id() != bundle.ruleset {
        return Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset);
    }
    let span = bundle.window_end.0.saturating_sub(bundle.window_start.0);
    if span == 0 || span > MAX_ADJUDICATION_TICKS {
        return Verdict::Unadjudicable(UnadjudicableReason::WindowOutOfRange);
    }
    // `claimed_hashes` is advisory (see the verdict section below), so its
    // length is a well-formedness check rather than something a verdict rests
    // on — a bundle whose hint does not describe its own window is malformed.
    if !bundle.claimed_hashes.is_empty() && bundle.claimed_hashes.len() as u64 != span {
        return Verdict::Unadjudicable(UnadjudicableReason::Malformed);
    }

    // Signature failures are *forgery*, not indecision: the reporter presented
    // these as the authority's, and they are checkably not.
    if verify_claim(&bundle.t0_claim, authority).is_err() {
        return Verdict::EvidenceForged(ForgeryProof::ClaimSignatureInvalid);
    }
    for claim in &bundle.disputed_claims {
        if verify_claim(claim, authority).is_err() {
            return Verdict::EvidenceForged(ForgeryProof::ClaimSignatureInvalid);
        }
    }

    let mut harness = ReplayHarness::on(backend);
    if let Err(error) = harness.load_claimed_snapshot(&bundle.t0_claim, &bundle.t0_snapshot) {
        return match error {
            ReplayError::SnapshotHashMismatch => {
                Verdict::EvidenceForged(ForgeryProof::SnapshotHashMismatch)
            }
            _ => Verdict::Unadjudicable(UnadjudicableReason::Malformed),
        };
    }

    let trace = match harness.replay(
        &bundle.frames,
        authority,
        (bundle.window_start, bundle.window_end),
        &bundle.sibling_heads,
    ) {
        Ok(trace) => trace,
        Err(ReplayError::Log(LogError::SignatureInvalid)) => {
            return Verdict::EvidenceForged(ForgeryProof::FrameSignatureInvalid)
        }
        Err(ReplayError::Log(_)) | Err(ReplayError::NonContiguousFrames) => {
            return Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain)
        }
        Err(ReplayError::WindowOutOfRange) => {
            return Verdict::Unadjudicable(UnadjudicableReason::WindowOutOfRange)
        }
        Err(_) => return Verdict::Unadjudicable(UnadjudicableReason::Malformed),
    };

    // A verdict may rest ONLY on what the subject signed.
    //
    // `claimed_hashes` is a per-tick trajectory the *reporter* supplies, and
    // the subject never signs it — it exists so an adjudicator can jump
    // straight to the first divergent tick instead of scanning. Reaching a
    // verdict from it would let a malicious reporter convict an honest peer by
    // inventing numbers, which is the exact failure a self-verifying bundle is
    // supposed to make impossible. The signed assertions are the `StateClaim`s,
    // so those are what the replay is judged against.
    //
    // This is why a window must end at a claim tick (docs/06 §7): a window with
    // no signed claim in it contains nothing the subject can be held to.
    if bundle.disputed_claims.is_empty() {
        return Verdict::Unadjudicable(UnadjudicableReason::Malformed);
    }

    let mut previous = claim_hash(&bundle.t0_claim);
    for claim in &bundle.disputed_claims {
        // Claims must chain, or the authority equivocated about its own
        // history even though every individual signature checks out.
        if claim.prev_claim != previous {
            return Verdict::Confirms {
                at: claim.tick,
                kind: DeviationKind::DiscreteMismatch,
            };
        }
        previous = claim_hash(claim);

        // A claim at tick T commits to the state *before* T executes, which is
        // the state after T-1. Comparing against T itself would accuse every
        // honest authority of being one tick ahead of itself.
        let Some(commits_to) = claim.tick.0.checked_sub(1) else {
            continue;
        };
        if commits_to < bundle.window_start.0 || claim.tick.0 > bundle.window_end.0 {
            continue;
        }
        let Some(computed) = trace.at(Tick::new(commits_to)) else {
            return Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain);
        };
        // State hashes are over quantized canonical bytes, so this is exact and
        // covers discrete state (VC-5) with no tolerance at all.
        if computed != claim.state_hash {
            return Verdict::Confirms {
                at: claim.tick,
                kind: DeviationKind::DiscreteMismatch,
            };
        }
    }

    Verdict::Exonerates
}
