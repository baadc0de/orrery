//! The witness engine: ingest, re-execute, compare, escalate (docs/07 §3).
//!
//! A witness holds no trust in the authority it watches. It re-executes that
//! authority's own signed log against that authority's own committed starting
//! state, and compares the result with what the authority claimed. Every input
//! to that judgement came from the subject, signed — which is why the output
//! can be checked by anyone and why the cluster never has to believe the
//! witness.
//!
//! Two things it deliberately does *not* do. It does not decide guilt: it
//! produces evidence and the adjudicator re-runs it. And it does not accuse on
//! a missing packet: a chain gap is a [`LogRangeRequest`], because datagram
//! loss is expected and treating it as fabrication would strike honest peers
//! on a lossy link.

use std::collections::BTreeMap;

use orrery_core::log::verify_claim;
use orrery_core::replay::ReplayHarness;
use orrery_core::store::AuthorityLog;
use orrery_core::{evaluate, Executor, InvariantSample, InvariantViolation, Ruleset, TickOutcome};
use orrery_protocol::{
    ChainHash, DiscrepancyReport, EvidenceBundle, LogFrame, LogRangeRequest, NodeId, PersistId,
    RecordSource, StateClaim, Tick, MAX_ADJUDICATION_TICKS,
};

use crate::report::sign_report;

/// Witness tuning (docs/10-crates.md §8, D16 defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WitnessConfig {
    /// Audit window ceiling, in ticks. Anomalies longer than this are reported
    /// as consecutive windows rather than one oversized bundle.
    pub window_ticks: u64,
    /// **Telemetry only: check everything, file nothing.**
    ///
    /// Default `true`, and it should stay true until P4's false-positive rate
    /// has been measured. D17 risk 3: false-positive strikes on honest players
    /// are the failure mode that kills witness-based trust, and no amount of
    /// correct detection logic substitutes for having measured the drift
    /// distribution on real hardware first.
    pub shadow_mode: bool,
}

impl Default for WitnessConfig {
    fn default() -> Self {
        Self {
            window_ticks: MAX_ADJUDICATION_TICKS,
            shadow_mode: true,
        }
    }
}

/// What a witness observed and, in shadow mode, only counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WitnessCounters {
    /// Frames accepted into a watched chain.
    pub frames_accepted: u64,
    /// Frames rejected — bad signature, broken chain, illegal order.
    pub frames_rejected: u64,
    /// Chain gaps detected, each producing a [`LogRangeRequest`].
    pub gaps_detected: u64,
    /// Stage-1 invariant breaches.
    pub invariant_breaches: u64,
    /// Re-execution mismatches against a claim.
    pub claim_mismatches: u64,
    /// Reports that would have been filed. In shadow mode nothing leaves.
    pub reports_raised: u64,
    /// Reports actually filed, always zero in shadow mode.
    pub reports_filed: u64,
}

/// Why an ingest was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessError {
    /// Nothing is being watched for this entity.
    NotWatched,
    /// The frame or claim is not signed by the subject.
    NotTheSubject,
    /// The frame does not verify: bad signature, broken chain, illegal order.
    FrameRejected,
    /// The claim's signature does not verify.
    ClaimRejected,
    /// A logged input payload is not a valid `CoreInput`.
    InputMalformed,
}

impl core::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotWatched => f.write_str("entity is not being watched"),
            Self::NotTheSubject => f.write_str("frame is not from the watched authority"),
            Self::FrameRejected => f.write_str("frame failed verification"),
            Self::ClaimRejected => f.write_str("claim signature does not verify"),
            Self::InputMalformed => f.write_str("logged input is not a valid CoreInput"),
        }
    }
}

impl core::error::Error for WitnessError {}

/// Something the caller must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitnessSignal {
    /// The chain has a hole; ask the authority to fill it.
    ///
    /// Not an accusation. Refusal or timeout is separately reportable, but a
    /// dropped datagram is the expected case on the lane logs ride.
    Gap(LogRangeRequest),
    /// A stage-1 check failed on received state.
    InvariantBreach {
        /// The entity that failed.
        entity: PersistId,
        /// Which check, and which validator reported it.
        violation: InvariantViolation,
    },
    /// Re-execution disagreed with a signed claim. The audit is armed.
    ClaimMismatch {
        /// The entity that diverged.
        entity: PersistId,
        /// The claim tick the disagreement was found at.
        at: Tick,
    },
    /// A report was assembled. `None` in shadow mode: the witness still did
    /// every check and still counted, but nothing leaves.
    Report(Option<Box<DiscrepancyReport>>),
}

/// One observation of received authoritative state, for stage-1 checks.
pub struct Observation<'a, S> {
    /// The entity the sample is for.
    pub entity: PersistId,
    /// The received state.
    pub state: &'a S,
    /// The tick it is stamped with.
    pub tick: Tick,
}

/// One watched entity's running state.
struct Watched<S> {
    subject: NodeId,
    /// Tick of the claim this watch was anchored at — the earliest point a
    /// repair could usefully start from.
    anchor_tick: u64,
    /// The tick a repair was last asked for, if one is outstanding.
    ///
    /// Without this a witness re-asks on *every* frame that fails to chain, and
    /// live frames keep arriving throughout the catch-up: one hitch turns into
    /// hundreds of overlapping requests, each making the authority scan and
    /// re-serialise its retained window. A swarm with stalling peers ran
    /// seventy times slower than one without, entirely here.
    repair_pending: Option<u64>,
    head: ChainHash,
    /// Whether `head` reflects a verified fold, or is still the value a claim
    /// seeded. Until the first frame lands there is nothing to detect a gap
    /// against.
    anchored: bool,
    last_sample: Option<(S, Tick)>,
    claims: BTreeMap<u64, StateClaim>,
    computed: BTreeMap<u64, [u8; 32]>,
}

/// A witness over one universe, watching entities held by others.
///
/// Generic over the ruleset because re-execution *is* the witness signal: a
/// witness without the game's rules can check signatures and chains but cannot
/// tell whether an outcome was legal.
pub struct Witness<R: Ruleset> {
    config: WitnessConfig,
    seed: orrery_protocol::UniverseSeed,
    ruleset_factory: fn() -> R,
    executor: Executor<R>,
    watched: BTreeMap<PersistId, Watched<R::CoreState>>,
    /// The subject's frames, retained so a window can be assembled from what
    /// was actually received rather than re-requested.
    log: AuthorityLog,
    counters: WitnessCounters,
    /// Newest tick at which retention was last enforced.
    last_pruned: u64,
}

/// A request to start watching an entity.
pub struct Watch<S> {
    /// The entity to watch.
    pub entity: PersistId,
    /// The authority that holds it — every frame and claim must be signed by
    /// this key.
    pub subject: NodeId,
    /// The claim to anchor re-execution at.
    pub anchor: StateClaim,
    /// The state that claim commits to.
    pub anchor_state: S,
}

impl<R: Ruleset> Witness<R> {
    /// Ticks of progress between retention sweeps.
    ///
    /// A quarter of the default 600-tick window: often enough that the window
    /// never grows much past its nominal size, rare enough that the sweep is
    /// amortised across hundreds of ingests.
    const PRUNE_EVERY: u64 = 150;

    /// Ticks of the subject's timeline before an unanswered repair is re-asked.
    ///
    /// Long enough that a multi-datagram refill completes without a second
    /// request piling on, short enough that a genuinely lost repair is retried
    /// well inside the 180-tick adjudication window.
    const REPAIR_TIMEOUT_TICKS: u64 = 60;

    /// A witness that builds a fresh `Ruleset` whenever it needs one.
    ///
    /// A factory rather than a value because re-executing a window needs its
    /// own harness with its own executor, and a `Ruleset` is a pure value that
    /// is cheap to construct — cheaper than making it `Clone` and forcing that
    /// bound on every game.
    pub fn new(
        config: WitnessConfig,
        seed: orrery_protocol::UniverseSeed,
        ruleset_factory: fn() -> R,
    ) -> Self {
        Self {
            config,
            seed,
            ruleset_factory,
            executor: Executor::new(ruleset_factory(), seed),
            watched: BTreeMap::new(),
            log: AuthorityLog::default(),
            last_pruned: 0,
            counters: WitnessCounters::default(),
        }
    }

    /// Everything observed so far.
    #[must_use]
    pub fn counters(&self) -> WitnessCounters {
        self.counters
    }

    /// Whether this witness would file, or only count.
    #[must_use]
    pub fn shadow_mode(&self) -> bool {
        self.config.shadow_mode
    }

    /// Start watching an entity from a signed anchor claim.
    ///
    /// The anchor is verified before anything is stored: a witness that
    /// anchored on an unsigned claim would re-execute from a starting point the
    /// subject never committed to, and every subsequent comparison would be
    /// against a trajectory nobody ran.
    ///
    /// # Errors
    ///
    /// [`WitnessError::ClaimRejected`] if the anchor is not signed by the
    /// subject.
    pub fn watch(&mut self, watch: Watch<R::CoreState>) -> Result<(), WitnessError> {
        verify_claim(&watch.anchor, watch.subject).map_err(|_| WitnessError::ClaimRejected)?;
        // Retain the anchor with its snapshot: it is the only claim a witness
        // ever holds state for, and a bundle has to start from committed state.
        self.log.record_claim(
            watch.anchor.clone(),
            orrery_core::CoreCodec::to_canonical(&watch.anchor_state),
        );
        self.executor.insert(watch.entity, watch.anchor_state);
        let mut claims = BTreeMap::new();
        claims.insert(watch.anchor.tick.0, watch.anchor.clone());
        self.watched.insert(
            watch.entity,
            Watched {
                subject: watch.subject,
                anchor_tick: watch.anchor.tick.0,
                repair_pending: None,
                head: watch.anchor.input_head,
                anchored: false,
                last_sample: None,
                claims,
                computed: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// Run stage-1 invariants on a received sample.
    ///
    /// Cheap and stateless beyond the previous sample, so every interested peer
    /// runs this regardless of witness-set membership.
    pub fn observe(&mut self, observation: Observation<'_, R::CoreState>) -> Option<WitnessSignal>
    where
        R::CoreState: Clone,
    {
        let ruleset = (self.ruleset_factory)();
        let invariants = ruleset.invariants();
        let watched = self.watched.get_mut(&observation.entity)?;

        let elapsed_ticks = watched
            .last_sample
            .as_ref()
            .map(|(_, previous_tick)| observation.tick.0.saturating_sub(previous_tick.0) as u32)
            .unwrap_or(0);
        let sample = InvariantSample {
            entity: observation.entity,
            current: observation.state,
            tick: observation.tick,
            previous: watched.last_sample.as_ref().map(|(state, _)| state),
            elapsed_ticks,
        };
        let outcome = evaluate(invariants, &sample);
        watched.last_sample = Some((observation.state.clone(), observation.tick));

        match outcome {
            Ok(()) => None,
            Err(violation) => {
                self.counters.invariant_breaches += 1;
                Some(WitnessSignal::InvariantBreach {
                    entity: observation.entity,
                    violation,
                })
            }
        }
    }

    /// Whether this witness is following `entity`.
    #[must_use]
    pub fn watches(&self, entity: PersistId) -> bool {
        self.watched.contains_key(&entity)
    }

    /// Ingest a frame as it arrived on the wire, with its full head pairs.
    ///
    /// [`Self::ingest_frame`] wants sibling heads positionally — the entities
    /// this witness does *not* follow, in slice order. A sender cannot produce
    /// that list, because it does not know what any given receiver follows, so
    /// on the wire every entity's pair travels and the selection happens here.
    /// Doing it in the caller would duplicate the rule that picks the watched
    /// entity, and the two would drift.
    ///
    /// Pairs for followed entities are ignored: this witness folded those
    /// itself, and taking the sender's word for them would let an authority
    /// choose the head its own frame is checked against.
    ///
    /// # Errors
    ///
    /// [`WitnessError::FrameRejected`] if a pair is missing for an entity this
    /// witness cannot fold — the preimage cannot be rebuilt, so the signature
    /// cannot be checked. Otherwise as [`Self::ingest_frame`].
    pub fn ingest_wire_frame(
        &mut self,
        frame: &LogFrame,
        heads: &[orrery_protocol::FrameHead],
    ) -> Result<Vec<WitnessSignal>, WitnessError> {
        let watched = frame
            .entities
            .iter()
            .map(|slice| slice.entity)
            .find(|entity| self.watched.contains_key(entity))
            .ok_or(WitnessError::NotWatched)?;

        let mut siblings = Vec::with_capacity(frame.entities.len().saturating_sub(1));
        for slice in &frame.entities {
            if slice.entity == watched {
                continue;
            }
            let Some(pair) = heads.iter().find(|head| head.entity == slice.entity) else {
                self.counters.frames_rejected += 1;
                return Err(WitnessError::FrameRejected);
            };
            siblings.push((pair.prev_head, pair.head));
        }
        self.ingest_frame(frame, &siblings)
    }

    /// Ingest a signed frame and re-execute the ticks it covers.
    ///
    /// # Errors
    ///
    /// [`WitnessError`] when the frame is not from the subject, does not
    /// verify, or carries an undecodable input.
    pub fn ingest_frame(
        &mut self,
        frame: &LogFrame,
        sibling_heads: &[(ChainHash, ChainHash)],
    ) -> Result<Vec<WitnessSignal>, WitnessError> {
        let entity = frame
            .entities
            .iter()
            .map(|slice| slice.entity)
            .find(|entity| self.watched.contains_key(entity))
            .ok_or(WitnessError::NotWatched)?;
        let watched = self.watched.get(&entity).ok_or(WitnessError::NotWatched)?;
        let subject = watched.subject;
        let expected_head = watched.head;
        let anchored = watched.anchored;

        // Gap detection first, and it is *not* an accusation. The wire carries
        // truncated heads precisely so a receiver can notice a hole cheaply.
        if let Some(slice) = frame.entities.iter().find(|slice| slice.entity == entity) {
            if anchored && slice.prev_head != expected_head.rolling() {
                // One outstanding repair at a time. A frame arriving mid-repair
                // still fails to chain — that is what "mid-repair" means — and
                // asking again for the same hole achieves nothing but load. The
                // request is re-issued only once `REPAIR_TIMEOUT_TICKS` of the
                // subject's own timeline have passed without the hole closing,
                // which is what keeps a *lost* repair from stalling the witness
                // forever.
                if let Some(asked_at) = watched.repair_pending {
                    if frame.first_tick.0 < asked_at + Self::REPAIR_TIMEOUT_TICKS {
                        return Ok(Vec::new());
                    }
                }
                self.counters.gaps_detected += 1;
                // Ask from where this witness actually stopped, not from the
                // beginning of history. `from_tick: 0` looks harmless because
                // retention bounds the answer, but it makes *every* repair a
                // full-history request: the authority then scans and measures
                // its whole retained window, the response is truncated to one
                // datagram, and the requester asks again — so a peer that
                // hitches once drags its witnesses through the entire window
                // repeatedly. A swarm with stalling peers took three orders of
                // magnitude longer than one without because of this line.
                let resume = watched
                    .computed
                    .keys()
                    .next_back()
                    .map_or(watched.anchor_tick, |tick| tick + 1);
                let asked_at = frame.first_tick.0;
                if let Some(watched) = self.watched.get_mut(&entity) {
                    watched.repair_pending = Some(asked_at);
                }
                return Ok(vec![WitnessSignal::Gap(LogRangeRequest {
                    entity,
                    chain_epoch: slice.chain_epoch,
                    from_tick: Tick::new(resume.min(frame.first_tick.0)),
                    to_tick: frame.first_tick,
                })]);
            }
        }

        let mut prev_heads = Vec::with_capacity(frame.entities.len());
        let mut sibling_cursor = 0usize;
        for slice in &frame.entities {
            if slice.entity == entity {
                prev_heads.push(expected_head);
            } else {
                let Some(pair) = sibling_heads.get(sibling_cursor) else {
                    self.counters.frames_rejected += 1;
                    return Err(WitnessError::FrameRejected);
                };
                sibling_cursor += 1;
                prev_heads.push(pair.0);
            }
        }

        let transitions =
            orrery_core::log::verify_frame(frame, subject, &prev_heads).map_err(|_| {
                self.counters.frames_rejected += 1;
                WitnessError::FrameRejected
            })?;
        self.counters.frames_accepted += 1;

        // Re-execute every tick the frame covers, including ticks that logged
        // nothing: a silent tick still advances state and still draws from the
        // RNG, so skipping it would put the witness on a different trajectory
        // than the authority for reasons that have nothing to do with cheating.
        let mut per_tick: BTreeMap<u64, Vec<R::CoreInput>> = BTreeMap::new();
        for slice in &frame.entities {
            if slice.entity != entity {
                continue;
            }
            for record in &slice.records {
                if !matches!(
                    record.source,
                    RecordSource::Player { .. } | RecordSource::InboundEvent { .. }
                ) {
                    continue;
                }
                let input = <R::CoreInput as orrery_core::CoreCodec>::decode(&record.payload)
                    .map_err(|_| WitnessError::InputMalformed)?;
                per_tick
                    .entry(frame.first_tick.0 + u64::from(record.tick_off))
                    .or_default()
                    .push(input);
            }
        }

        let mut signals = Vec::new();
        for tick in frame.first_tick.0..frame.first_tick.0 + u64::from(frame.tick_count) {
            let inputs = per_tick.remove(&tick).unwrap_or_default();
            let Some(TickOutcome { state_hash, .. }) =
                self.executor.step_entity(entity, Tick::new(tick), &inputs)
            else {
                return Err(WitnessError::NotWatched);
            };
            if let Some(watched) = self.watched.get_mut(&entity) {
                watched.computed.insert(tick, state_hash);
            }
            self.log
                .record_tick_hash(entity, Tick::new(tick), state_hash);
        }

        if let Some(watched) = self.watched.get_mut(&entity) {
            for transition in &transitions {
                if transition.entity == entity {
                    watched.head = transition.head;
                    watched.anchored = true;
                    // The chain moved, so whatever hole was being repaired is
                    // closed as far as this witness can tell.
                    watched.repair_pending = None;
                }
            }
        }
        self.log.record_frame(frame.clone(), transitions);

        // A claim may already have arrived for a tick this frame just computed.
        if let Some(signal) = self.check_pending_claims(entity) {
            signals.push(signal);
        }

        let last_tick = frame.first_tick.0 + u64::from(frame.tick_count).saturating_sub(1);
        self.prune_if_due(Tick::new(last_tick));
        Ok(signals)
    }

    /// Drop history older than the retention window.
    ///
    /// **Not an optimisation.** Without it a witness accumulates one state hash
    /// per tick per watched entity for as long as it watches — and
    /// [`Self::check_pending_claims`] rescans the retained claims on every
    /// ingest, so the cost is quadratic in session length as well as unbounded
    /// in memory. A 32-peer swarm watching seven neighbours each stalled inside
    /// two simulated minutes before this existed.
    ///
    /// The window is the log's own retention (docs/06 §6: floor is the 180-tick
    /// adjudication window, default 600). Anything older cannot be assembled
    /// into a bundle, so keeping it buys nothing but a slower scan.
    /// Prune at most once per [`Self::PRUNE_EVERY`] ticks of progress.
    ///
    /// Pruning walks the retained frames, so doing it on every ingest is itself
    /// quadratic — a witness watching seven peers ingests hundreds of frames a
    /// second and each one would rescan the whole window. Amortising keeps the
    /// window bounded without paying for the bound continuously.
    fn prune_if_due(&mut self, now: Tick) {
        if now.0 < self.last_pruned.saturating_add(Self::PRUNE_EVERY) {
            return;
        }
        self.last_pruned = now.0;
        self.prune(now);
    }

    fn prune(&mut self, now: Tick) {
        let floor = now.0.saturating_sub(self.log.retention().effective_ticks());
        self.log.prune(now);
        for watched in self.watched.values_mut() {
            watched.computed.retain(|tick, _| *tick >= floor);
            // One claim below the floor is kept: a claim at tick T is compared
            // against the hash for T-1, so evicting on the same boundary as the
            // hashes would drop the comparison a surviving claim still needs.
            watched.claims.retain(|tick, _| *tick + 1 >= floor);
        }
    }

    /// Ingest a signed claim and compare it against what re-execution produced.
    ///
    /// # Errors
    ///
    /// [`WitnessError`] when the claim is not from the watched subject or does
    /// not verify.
    pub fn ingest_claim(
        &mut self,
        claim: &StateClaim,
    ) -> Result<Option<WitnessSignal>, WitnessError> {
        let watched = self
            .watched
            .get(&claim.entity)
            .ok_or(WitnessError::NotWatched)?;
        verify_claim(claim, watched.subject).map_err(|_| WitnessError::ClaimRejected)?;
        if let Some(watched) = self.watched.get_mut(&claim.entity) {
            watched.claims.insert(claim.tick.0, claim.clone());
        }
        // Later claims are retained without a snapshot: a witness never holds
        // the subject's state, only the anchor it was given. That is enough —
        // a bundle starts from the anchor and the rest are what the subject is
        // held to.
        self.log.record_claim(claim.clone(), Vec::new());
        Ok(self.check_pending_claims(claim.entity))
    }

    /// Compare every claim whose tick has been re-executed.
    ///
    /// A claim at tick T commits to the state *before* T executes, so it is
    /// compared against the hash computed for T-1. Getting that off by one
    /// would make every honest authority look like a cheat.
    fn check_pending_claims(&mut self, entity: PersistId) -> Option<WitnessSignal> {
        let watched = self.watched.get(&entity)?;
        let mut diverged: Option<Tick> = None;
        for (tick, claim) in &watched.claims {
            let Some(previous) = tick.checked_sub(1) else {
                continue;
            };
            let Some(computed) = watched.computed.get(&previous) else {
                continue;
            };
            if *computed != claim.state_hash {
                diverged = Some(Tick::new(*tick));
                break;
            }
        }
        let at = diverged?;
        self.counters.claim_mismatches += 1;
        Some(WitnessSignal::ClaimMismatch { entity, at })
    }

    /// Assemble a report for a window, honouring shadow mode.
    ///
    /// Returns `WitnessSignal::Report(None)` in shadow mode: the window was
    /// assembled and counted, and nothing is filed. That is the whole posture
    /// of P4 — measure first, enforce later.
    ///
    /// # Errors
    ///
    /// [`WitnessError::NotWatched`] when the entity is not watched.
    pub fn raise(
        &mut self,
        key: &iroh_base::SecretKey,
        entity: PersistId,
        window: (Tick, Tick),
    ) -> Result<WitnessSignal, WitnessError> {
        let watched = self.watched.get(&entity).ok_or(WitnessError::NotWatched)?;
        let subject = watched.subject;
        self.counters.reports_raised += 1;

        if self.config.shadow_mode {
            return Ok(WitnessSignal::Report(None));
        }

        let computed: Vec<_> = (window.0 .0..window.1 .0)
            .map(|tick| watched.computed.get(&tick).copied().unwrap_or([0; 32]))
            .collect();
        let Ok(bundle) = self.log.assemble_bundle(entity, window, computed) else {
            // A window the witness cannot serve is not an accusation it can
            // make. Silence here is correct: the adjudicator would answer
            // `Unadjudicable`, which strikes nobody and costs a round trip.
            return Ok(WitnessSignal::Report(None));
        };
        self.counters.reports_filed += 1;
        Ok(WitnessSignal::Report(Some(Box::new(sign_report(
            key, subject, bundle,
        )))))
    }

    /// Re-execute a window from retained frames, for a caller that wants the
    /// trajectory rather than a report.
    ///
    /// # Errors
    ///
    /// [`WitnessError::NotWatched`] when nothing is retained for the entity.
    pub fn replay_window(
        &self,
        entity: PersistId,
        window: (Tick, Tick),
    ) -> Result<EvidenceBundle, WitnessError> {
        let watched = self.watched.get(&entity).ok_or(WitnessError::NotWatched)?;
        let computed: Vec<_> = (window.0 .0..window.1 .0)
            .map(|tick| watched.computed.get(&tick).copied().unwrap_or([0; 32]))
            .collect();
        self.log
            .assemble_bundle(entity, window, computed)
            .map_err(|_| WitnessError::NotWatched)
    }

    /// A fresh harness over this witness's ruleset build and universe seed.
    ///
    /// Exposed so a caller can check a bundle it assembled with exactly the
    /// build the witness used, rather than one it constructed separately and
    /// hoped matched.
    #[must_use]
    pub fn harness(&self) -> ReplayHarness<R> {
        ReplayHarness::new((self.ruleset_factory)(), self.seed)
    }
}
