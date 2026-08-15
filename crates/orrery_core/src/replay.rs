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

use std::collections::BTreeMap;

use orrery_protocol::{
    ChainHash, DeviationKind, EvidenceBundle, ForgeryProof, LogFrame, NodeId, PersistId,
    RecordSource, Tick, UnadjudicableReason, UniverseSeed, Verdict, MAX_ADJUDICATION_TICKS,
};

use crate::executor::Executor;
use crate::log::{claim_hash, verify_claim, verify_frame, LogError};
use crate::ruleset::{CoreCodec, Ruleset};

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

/// Re-executes a window from logged frames.
pub struct ReplayHarness<R: Ruleset> {
    executor: Executor<R>,
    entity: Option<PersistId>,
    /// The chain head the loaded claim commits to.
    ///
    /// A window rarely starts at the beginning of a chain, so folding from the
    /// empty head would put the replay on a different chain than the authority
    /// and report a head mismatch on the first frame. This is what
    /// `StateClaim::input_head` is *for*.
    start_head: Option<ChainHash>,
}

impl<R: Ruleset> ReplayHarness<R> {
    /// A harness over one ruleset build and universe seed.
    pub fn new(ruleset: R, universe_seed: UniverseSeed) -> Self {
        Self {
            executor: Executor::new(ruleset, universe_seed),
            entity: None,
            start_head: None,
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
        self.executor.insert(claim.entity, state);
        self.entity = Some(claim.entity);
        self.start_head = Some(claim.input_head);
        Ok(())
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
                    // `NeighborFrame` and `AuthorityChange` records close the
                    // input set and mark epoch boundaries; they are not
                    // themselves `CoreInput`s to feed the step.
                    if !matches!(
                        record.source,
                        RecordSource::Player { .. } | RecordSource::InboundEvent { .. }
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
            let Some(outcome) = self.executor.step_entity(entity, Tick::new(tick), &inputs) else {
                return Err(ReplayError::SnapshotMalformed);
            };
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
    if ruleset.id() != bundle.ruleset {
        return Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset);
    }
    let span = bundle.window_end.0.saturating_sub(bundle.window_start.0);
    if span == 0 || span > MAX_ADJUDICATION_TICKS {
        return Verdict::Unadjudicable(UnadjudicableReason::WindowOutOfRange);
    }
    if bundle.claimed_hashes.len() as u64 != span {
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

    let mut harness = ReplayHarness::new(ruleset, universe_seed);
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

    // State hashes are over quantized canonical bytes, so this comparison is
    // exact and covers discrete state (VC-5) with no tolerance at all. The
    // continuous-state band comparison in `tolerance` applies to trajectories
    // the harness can decode into positions; hash equality is the stronger
    // check and the one a bundle carries the data for.
    for (offset, claimed) in bundle.claimed_hashes.iter().enumerate() {
        let tick = Tick::new(bundle.window_start.0 + offset as u64);
        let Some(computed) = trace.at(tick) else {
            return Verdict::Unadjudicable(UnadjudicableReason::IncompleteChain);
        };
        if computed != *claimed {
            return Verdict::Confirms {
                at: tick,
                kind: DeviationKind::DiscreteMismatch,
            };
        }
    }

    // Claims inside the window must chain, or the authority equivocated about
    // its own history even though every individual signature checks out.
    let mut previous = claim_hash(&bundle.t0_claim);
    for claim in &bundle.disputed_claims {
        if claim.prev_claim != previous {
            return Verdict::Confirms {
                at: claim.tick,
                kind: DeviationKind::DiscreteMismatch,
            };
        }
        previous = claim_hash(claim);
    }

    Verdict::Exonerates
}
