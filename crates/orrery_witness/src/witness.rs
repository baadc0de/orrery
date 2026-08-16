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
//!
//! # One frame, every entity it names
//!
//! A frame's single signature covers every entity the authority authored in
//! that send (docs/06 §6), so a witness watching several of a subject's
//! entities sees all of them in one frame. Every watched entity in a frame is
//! folded, re-executed and advanced — not just the first. A witness that
//! handled one and treated the rest as siblings would keep accepting frames
//! while quietly not watching most of what it was asked to watch, and would be
//! taking the *sender's* word for the head its own frame is checked against.

use std::collections::{BTreeMap, BTreeSet};

use orrery_core::log::{fold_all, verify_claim};
use orrery_core::replay::ReplayHarness;
use orrery_core::store::AuthorityLog;
use orrery_core::{evaluate, Executor, InvariantSample, InvariantViolation, Ruleset, TickOutcome};
use orrery_protocol::{
    ChainHash, DiscrepancyReport, EvidenceBundle, FrameHead, LogFrame, LogRangeRequest, NodeId,
    PersistId, RecordSource, StateClaim, Tick, MAX_ADJUDICATION_TICKS,
};

use crate::report::sign_report;

/// Witness tuning (docs/10-crates.md §8, D16 defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WitnessConfig {
    /// Audit window ceiling, in ticks. Anomalies longer than this are reported
    /// as consecutive windows rather than one oversized bundle.
    ///
    /// Read by [`Witness::audit_window`] and [`Witness::raise`], and clamped
    /// down to [`MAX_ADJUDICATION_TICKS`]: a longer window is refused by every
    /// adjudicator, so accepting one here would only defer the failure.
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
    /// Frames set aside because a repair for that chain is still outstanding.
    ///
    /// Not a rejection and not an acceptance: the frame was well-formed and the
    /// witness simply cannot chain it yet. Counted because a frame that vanishes
    /// into neither column is one an operator cannot account for — and this is
    /// the bucket a broken repair path fills up.
    pub frames_deferred: u64,
    /// Chain gaps detected, each producing a [`LogRangeRequest`].
    pub gaps_detected: u64,
    /// Stage-1 invariant breaches.
    pub invariant_breaches: u64,
    /// Re-execution mismatches against a claim. One per disputed claim, not one
    /// per packet that re-reveals it.
    pub claim_mismatches: u64,
    /// Reports that would have been filed. In shadow mode nothing leaves.
    pub reports_raised: u64,
    /// Reports actually filed, always zero in shadow mode.
    pub reports_filed: u64,
    /// Subjects that failed to fill a hole across repeated repairs.
    pub stalled: u64,
    /// Claim comparisons skipped because the witness was catching up.
    ///
    /// Not a fault on anyone's part — it is the count of judgements correctly
    /// *not* made, and a witness reporting many of them is one on a bad link.
    pub judgements_deferred: u64,
    /// Subject ticks this witness re-executed, and can therefore judge.
    ///
    /// The numerator of observation coverage.
    pub judged_ticks: u64,
    /// Subject ticks this witness was shown, judged or not.
    ///
    /// The denominator. Counted from the *advance* of each watch's newest seen
    /// tick, so a repair re-delivering a range is not counted twice, and
    /// counted whether the frame could be folded or was set aside — a frame the
    /// witness could not chain is still timeline it was shown and did not
    /// judge. That is what makes the ratio detect a watch which has quietly
    /// stopped: judging freezes while the subject keeps talking, so the two
    /// diverge. Measured against `judged_ticks` alone the same watch looks
    /// perfect, because a witness that judges nothing also misjudges nothing.
    pub shown_ticks: u64,
    /// Frames folded on a retry, after the hole in front of them closed.
    ///
    /// Timeline that used to be thrown away: a frame that could not chain when
    /// it arrived was dropped, so everything a subject sent while a repair was
    /// in flight had to be asked for all over again.
    pub frames_recovered: u64,
    /// Frames dropped because the deferral buffer for their subject was full.
    pub deferrals_overflowed: u64,
    /// Watches resumed at a later anchor after a hole was abandoned.
    ///
    /// Each one is a window this witness gave up on and a point at which it
    /// started judging again. A subject with a rising count is either on a bad
    /// link or exploiting one, and the two are told apart by
    /// [`Self::unjudged_ticks`] against the session length rather than by this
    /// count alone.
    pub reanchors: u64,
    /// Subject ticks abandoned unjudged by those re-anchors.
    ///
    /// The denominator of observation coverage. A witness that reports zero
    /// findings across a long session has proven nothing unless this is small
    /// beside the ticks it actually judged — which is the distinction between
    /// "saw nothing wrong" and "saw nothing".
    pub unjudged_ticks: u64,
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
    /// The window cannot be assembled from what this witness holds.
    ///
    /// Distinct from shadow mode, and deliberately so: "we chose not to file"
    /// and "we could not" are different facts, and returning the same value for
    /// both is how a witness that has gone structurally mute goes unnoticed.
    WindowUnservable,
}

impl core::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotWatched => f.write_str("entity is not being watched"),
            Self::NotTheSubject => f.write_str("frame is not from the watched authority"),
            Self::FrameRejected => f.write_str("frame failed verification"),
            Self::ClaimRejected => f.write_str("claim signature does not verify"),
            Self::InputMalformed => f.write_str("logged input is not a valid CoreInput"),
            Self::WindowUnservable => f.write_str("window cannot be assembled from what is held"),
        }
    }
}

impl core::error::Error for WitnessError {}

/// A subject whose chain has a hole the witness is waiting to have filled.
///
/// **A witness in this state does not judge.** Its own re-execution stopped at
/// the hole, so every tick after it is unknown; comparing a claim against a
/// trajectory the witness never computed is how an honest peer that dropped a
/// packet gets accused. The design says as much — a gap is a
/// `LogRangeRequest`, never an accusation — but leaving that to fall out of
/// "the hashes happen to be missing" makes it an accident rather than a rule.
///
/// The loophole this deliberately does *not* leave open: a cheat could stall
/// forever to stay unjudged. Three things close it. [`Catchup::attempts`] and
/// [`WitnessSignal::Stalled`] make persistent failure to fill a hole itself
/// reportable, so the state has a floor. [`Witness::sweep`] keeps that floor
/// under a subject that stops sending altogether. And [`Witness::try_reanchor`]
/// puts a *ceiling* on how long the state lasts: a hole that is never filled is
/// eventually abandoned and judging resumes at a later signed anchor, so the
/// most a stall can buy is one unjudged window rather than permanent silence.
/// The report alone was not enough for that, because in shadow mode nothing
/// acts on a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Catchup {
    /// Subject tick at which the hole was first noticed.
    pub since: u64,
    /// Repairs asked for and not yet answered.
    pub attempts: u32,
    /// Whether this episode has already been escalated.
    ///
    /// An escalation is a statement about a *situation*, not about the packet
    /// that happened to reveal it. Without this the witness re-raises on every
    /// subsequent frame that fails to chain — thirty-six times per hole in a
    /// measured run — and an operator counting reports would see the retry rate
    /// rather than the number of subjects in trouble.
    pub reported: bool,
}

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
    /// A subject has failed to fill a hole across repeated repairs.
    ///
    /// Distinct from [`WitnessSignal::Gap`], which is a question. This is the
    /// point at which not answering has itself become the finding — the only
    /// thing that stops "stall forever" being a way to avoid being judged.
    Stalled {
        /// The entity whose chain is stuck.
        entity: PersistId,
        /// How long it has been stuck, in the subject's own ticks.
        since: u64,
        /// Repairs asked for without the hole closing.
        attempts: u32,
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

/// What the repair machinery decided about one entity's chain on one frame.
enum GapCheck {
    /// The chain lines up; the frame can be folded.
    Clear,
    /// A hole is open and already being repaired. The frame cannot chain and
    /// asking again would only add load.
    Waiting,
    /// Something the caller has to act on: a repair request, or an escalation.
    Signal(WitnessSignal),
}

/// Where a held frame sits: its subject's key bytes, then its first tick.
///
/// The key is the subject's raw bytes rather than the `NodeId` itself because
/// the buffer is scanned by range — every frame held for one subject, in tick
/// order — and that needs a total order the key type does not provide.
type DeferredKey = ([u8; 32], u64);

/// A frame held behind a hole, with the sibling heads it arrived with.
///
/// The heads travel with it: they are the caller's, not something this witness
/// can reconstruct later, and re-offering the frame without them would fail
/// verification for a reason that has nothing to do with the frame.
type DeferredFrame = (LogFrame, Vec<(ChainHash, ChainHash)>);

/// One watched entity's running state.
///
/// The executor is **per entity**, not shared across the witness. See
/// [`Witness`] for why that is a correctness requirement rather than a layout
/// choice.
struct Watched<R: Ruleset> {
    subject: NodeId,
    /// Tick of the claim this watch was anchored at — the earliest point a
    /// repair could usefully start from.
    anchor_tick: u64,
    /// The chain epoch the last accepted frame declared, so a repair asks about
    /// the epoch the gap is actually in.
    chain_epoch: u32,
    /// Set while this subject's chain has a hole being repaired.
    catchup: Option<Catchup>,
    /// Newest subject tick this watch has already counted as shown, once any
    /// frame has arrived. `None` until the first one does, so that frame
    /// contributes the whole span it covers rather than only its advance —
    /// otherwise every watch judges one tick more than it was ever shown and
    /// coverage reads fractionally over 100%.
    newest_seen: Option<u64>,
    /// Newest subject tick actually folded into the chain, once one has been.
    folded_through: Option<u64>,
    head: ChainHash,
    /// Whether `head` reflects a verified fold, or is still the value a claim
    /// seeded. Until the first frame lands there is nothing to detect a gap
    /// against.
    anchored: bool,
    claims: BTreeMap<u64, StateClaim>,
    computed: BTreeMap<u64, [u8; 32]>,
    /// Recent re-executed states, canonical-encoded, so a claim arriving after
    /// the ticks it commits to can still be given a snapshot. Bounded by
    /// [`Witness::RECENT_SNAPSHOTS`].
    recent: BTreeMap<u64, Vec<u8>>,
    /// Claim ticks this witness holds a snapshot for — the ticks a bundle can
    /// open at.
    snapshotted: BTreeSet<u64>,
    /// Claim ticks already signalled as mismatched, so one divergence is one
    /// finding rather than one per packet that re-reveals it.
    reported: BTreeSet<u64>,
    executor: Executor<R>,
}

/// A witness over one universe, watching entities held by others.
///
/// Generic over the ruleset because re-execution *is* the witness signal: a
/// witness without the game's rules can check signatures and chains but cannot
/// tell whether an outcome was legal.
///
/// # One executor per watched entity
///
/// Not a shared world. `Executor::step_entity` exposes every *other* entity in
/// the same executor as a neighbour snapshot, and a witness advances each
/// entity as that entity's frames arrive — so a shared executor would present
/// neighbours sitting at whatever ticks they happened to have reached, which is
/// not the coherent set the authority stepped. Worse, the adjudicator that
/// decides the verdict loads exactly one entity
/// (`ReplayHarness::load_claimed_snapshot`), so its neighbour map is empty.
/// Isolating each entity here is what makes the witness compute the same
/// trajectory the adjudicator will.
///
/// The residual, stated because it cannot be fixed from this crate: a ruleset
/// whose `step` reads neighbours is outside what witnessing can adjudicate at
/// all, because the authority's live execution *does* see them and no replay
/// reproduces that. Core steps should not read neighbours.
pub struct Witness<R: Ruleset> {
    config: WitnessConfig,
    seed: orrery_protocol::UniverseSeed,
    ruleset_factory: fn() -> R,
    /// Kept for [`Ruleset::invariants`], which stage 1 reads on every sample.
    ruleset: R,
    watched: BTreeMap<PersistId, Watched<R>>,
    /// Previous stage-1 sample per entity, watched or not — stage 1 runs on
    /// everything a peer receives (docs/06 §3), not only on what it witnesses.
    samples: BTreeMap<PersistId, (R::CoreState, Tick)>,
    /// The subject's frames, retained so a window can be assembled from what
    /// was actually received rather than re-requested.
    log: AuthorityLog,
    counters: WitnessCounters,
    /// Frames that could not chain when they arrived, held until the hole in
    /// front of them closes. Keyed by subject key bytes and first tick, so one
    /// buffered frame serves every watched entity it covers and the drain runs
    /// in tick order.
    deferred: BTreeMap<DeferredKey, DeferredFrame>,
    /// Set while draining, so a frame that defers again cannot re-enter the
    /// drain it is being drained by.
    draining: bool,
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

    /// Frames held per subject while a hole in front of them is repaired.
    ///
    /// A repair round trip is [`Self::REPAIR_TIMEOUT_TICKS`] of the subject's
    /// timeline; at the three-tick frames the 20 Hz send cadence cuts that is
    /// twenty frames, and the cap is set above it so an answer that arrives on
    /// time never finds the buffer already full.
    const MAX_DEFERRED_FRAMES: usize = 32;

    /// Repairs asked for without the hole closing before the subject is
    /// reported as stalled rather than merely behind.
    ///
    /// The backoff is linear in attempts, so this is roughly fifteen seconds of
    /// the subject's timeline — comfortably longer than a rate-limited refill
    /// of a full retention window, and well inside it a peer that simply will
    /// not answer stops being given the benefit of the doubt.
    const MAX_REPAIR_ATTEMPTS: u32 = 5;

    /// Re-executed states kept per entity, in ticks.
    ///
    /// A witness is never sent state, so the only snapshot it can ever start a
    /// bundle from is one it computed itself — and it has to still be holding
    /// the right tick when the claim that commits to it arrives. Two seconds of
    /// the subject's timeline is several times the 2 Hz claim cadence, which
    /// leaves room for a claim that arrives late without keeping a copy of
    /// every tick in the retention window.
    const RECENT_SNAPSHOTS: usize = 128;

    /// A witness that builds a fresh `Ruleset` whenever it needs one.
    ///
    /// A factory rather than a value because every watched entity gets its own
    /// executor and re-executing a window needs its own harness, and a
    /// `Ruleset` is a pure value that is cheap to construct — cheaper than
    /// making it `Clone` and forcing that bound on every game.
    pub fn new(
        config: WitnessConfig,
        seed: orrery_protocol::UniverseSeed,
        ruleset_factory: fn() -> R,
    ) -> Self {
        Self {
            config,
            seed,
            ruleset_factory,
            ruleset: ruleset_factory(),
            watched: BTreeMap::new(),
            samples: BTreeMap::new(),
            log: AuthorityLog::default(),
            deferred: BTreeMap::new(),
            draining: false,
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

    /// The authority this witness holds responsible for an entity.
    ///
    /// The adapter needs this to address a repair. Sending it to whoever handed
    /// the frame over instead would let any peer that replays a subject's
    /// genuine frames collect the repair traffic those frames provoke — and
    /// then not answer it.
    #[must_use]
    pub fn subject(&self, entity: PersistId) -> Option<NodeId> {
        self.watched.get(&entity).map(|watched| watched.subject)
    }

    /// The chain epoch this witness last saw an entity's frames declare.
    #[must_use]
    pub fn chain_epoch(&self, entity: PersistId) -> Option<u32> {
        self.watched.get(&entity).map(|watched| watched.chain_epoch)
    }

    /// The audit window ceiling actually in force.
    fn window_ceiling(&self) -> u64 {
        self.config.window_ticks.clamp(1, MAX_ADJUDICATION_TICKS)
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
        // Retain the anchor with its snapshot: a bundle has to start from
        // committed state, and this is the one snapshot a witness is handed
        // rather than having to compute.
        self.log.record_claim(
            watch.anchor.clone(),
            orrery_core::CoreCodec::to_canonical(&watch.anchor_state),
        );
        let mut executor = Executor::new((self.ruleset_factory)(), self.seed);
        executor.insert(watch.entity, watch.anchor_state);
        let mut claims = BTreeMap::new();
        claims.insert(watch.anchor.tick.0, watch.anchor.clone());
        let mut snapshotted = BTreeSet::new();
        snapshotted.insert(watch.anchor.tick.0);
        self.watched.insert(
            watch.entity,
            Watched {
                subject: watch.subject,
                anchor_tick: watch.anchor.tick.0,
                chain_epoch: watch.anchor.chain_epoch,
                catchup: None,
                newest_seen: None,
                folded_through: None,
                head: watch.anchor.input_head,
                anchored: false,
                claims,
                computed: BTreeMap::new(),
                recent: BTreeMap::new(),
                snapshotted,
                reported: BTreeSet::new(),
                executor,
            },
        );
        Ok(())
    }

    /// Run stage-1 invariants on a received sample.
    ///
    /// Cheap and stateless beyond the previous sample, so every interested peer
    /// runs this on everything it receives — **including entities it does not
    /// witness** (docs/06 §3). Stage 1 is what peers outside the witness set
    /// contribute, and gating it on witness-set membership would leave most
    /// bulk-class state with no validation at all.
    pub fn observe(&mut self, observation: Observation<'_, R::CoreState>) -> Option<WitnessSignal>
    where
        R::CoreState: Clone,
    {
        let outcome = {
            let invariants = self.ruleset.invariants();
            let previous = self.samples.get(&observation.entity);
            let elapsed_ticks = previous
                .map(|(_, tick)| observation.tick.0.saturating_sub(tick.0) as u32)
                .unwrap_or(0);
            let sample = InvariantSample {
                entity: observation.entity,
                current: observation.state,
                tick: observation.tick,
                previous: previous.map(|(state, _)| state),
                elapsed_ticks,
            };
            evaluate(invariants, &sample)
        };
        self.samples.insert(
            observation.entity,
            (observation.state.clone(), observation.tick),
        );

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

    /// Whether this witness is waiting on a repair for `entity`, and since when.
    ///
    /// A witness in this state is knowingly behind and does not judge — see
    /// [`Catchup`].
    #[must_use]
    pub fn catching_up(&self, entity: PersistId) -> Option<Catchup> {
        self.watched
            .get(&entity)
            .and_then(|watched| watched.catchup)
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
    /// entities, and the two would drift.
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
        heads: &[FrameHead],
    ) -> Result<Vec<WitnessSignal>, WitnessError> {
        if !self.follows_anything_in(frame) {
            return Err(WitnessError::NotWatched);
        }
        let mut siblings = Vec::with_capacity(frame.entities.len());
        for slice in &frame.entities {
            if self.watched.contains_key(&slice.entity) {
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

    /// Ingest a run of frames served as one repair response.
    ///
    /// # Why this is not a loop over [`Self::ingest_wire_frame`]
    ///
    /// A range response carries **one head pair per entity for the whole
    /// answer** — the pair as it stood at the first frame that named it, since
    /// repeating it per frame would multiply the response's overhead. Feeding
    /// every frame the same pairs therefore checks every frame after the first
    /// against a sibling head that is one frame stale, the fold lands somewhere
    /// else, and a repair the authority served correctly and in full is refused
    /// frame by frame. The hole then never closes, and the subject is escalated
    /// as stalled for answering properly — D17 risk 3 arriving through the one
    /// path built to prevent it.
    ///
    /// The witness folds the siblings forward itself instead. It can: their
    /// records are in the frames it was just handed.
    ///
    /// # Errors
    ///
    /// As [`Self::ingest_frame`]. A frame that fails stops the run — the frames
    /// after it chain from the one that did not land.
    pub fn ingest_wire_frames(
        &mut self,
        frames: &[LogFrame],
        heads: &[FrameHead],
    ) -> Result<Vec<WitnessSignal>, WitnessError> {
        let mut carried: BTreeMap<PersistId, ChainHash> = heads
            .iter()
            .map(|head| (head.entity, head.prev_head))
            .collect();
        let mut signals = Vec::new();
        for frame in frames {
            if !self.follows_anything_in(frame) {
                return Err(WitnessError::NotWatched);
            }
            let mut siblings = Vec::with_capacity(frame.entities.len());
            let mut advanced = Vec::new();
            for slice in &frame.entities {
                if self.watched.contains_key(&slice.entity) {
                    continue;
                }
                let Some(prev) = carried.get(&slice.entity).copied() else {
                    self.counters.frames_rejected += 1;
                    return Err(WitnessError::FrameRejected);
                };
                let head = fold_all(prev, &slice.records);
                siblings.push((prev, head));
                advanced.push((slice.entity, head));
            }
            signals.extend(self.ingest_frame(frame, &siblings)?);
            carried.extend(advanced);
        }
        Ok(signals)
    }

    fn follows_anything_in(&self, frame: &LogFrame) -> bool {
        frame
            .entities
            .iter()
            .any(|slice| self.watched.contains_key(&slice.entity))
    }

    /// Ingest a signed frame and re-execute the ticks it covers, for **every**
    /// entity in it this witness follows.
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
        let entities: Vec<PersistId> = frame
            .entities
            .iter()
            .map(|slice| slice.entity)
            .filter(|entity| self.watched.contains_key(entity))
            .collect();
        let Some(first) = entities.first().copied() else {
            return Err(WitnessError::NotWatched);
        };
        let subject = self.watched[&first].subject;

        // Gap detection first, and it is *not* an accusation. The wire carries
        // truncated heads precisely so a receiver can notice a hole cheaply. A
        // hole in any followed entity blocks the whole frame: one signature
        // covers every slice, so a frame cannot be half-folded.
        let last_tick = frame.first_tick.0 + u64::from(frame.tick_count).saturating_sub(1);

        // A frame entirely behind the fold is a duplicate, not a hole. Repairs
        // answer in whole frames and the buffer re-offers what it held, so the
        // same frame legitimately arrives twice; without this the second copy
        // fails the `prev_head` check — the witness has moved past it — and is
        // read as missing chain, so a correct answer manufactures the gap it
        // was sent to close.
        if !entities.is_empty()
            && entities.iter().all(|entity| {
                self.watched[entity]
                    .folded_through
                    .is_some_and(|folded| last_tick <= folded)
            })
        {
            return Ok(Vec::new());
        }

        // Counted before anything can turn the frame away: what the witness was
        // *shown* is the denominator of its coverage, and a frame it sets aside
        // is exactly the case that has to land in it.
        for entity in &entities {
            if let Some(watched) = self.watched.get_mut(entity) {
                let advance = match watched.newest_seen {
                    Some(seen) => last_tick.saturating_sub(seen),
                    None => last_tick.saturating_sub(frame.first_tick.0) + 1,
                };
                watched.newest_seen = Some(match watched.newest_seen {
                    Some(seen) => seen.max(last_tick),
                    None => last_tick,
                });
                self.counters.shown_ticks += advance;
            }
        }

        let mut signals = Vec::new();
        let mut blocked = false;
        for entity in &entities {
            // A drain is *speculative*: the frame is being re-offered on the
            // chance that the hole in front of it has closed, and it was
            // already accounted for when it first arrived. Running it back
            // through the repair machinery would ask for a hole that is already
            // being repaired and, worse, spend one of the attempts the stall
            // threshold counts — so a witness would escalate a subject for the
            // retries the witness itself performed.
            if self.draining {
                blocked |= !self.chains(*entity, frame);
                continue;
            }
            match self.repair_check(*entity, frame) {
                GapCheck::Clear => {}
                GapCheck::Waiting => blocked = true,
                GapCheck::Signal(signal) => {
                    blocked = true;
                    signals.push(signal);
                }
            }
        }
        if blocked {
            if !self.draining {
                self.counters.frames_deferred += 1;
            }
            self.buffer_deferred(subject, frame, sibling_heads);
            return Ok(signals);
        }

        // Rebuild the signature preimage: our own heads from our own fold,
        // siblings from the caller. Anything followed uses the witness's fold,
        // never the sender's claim about it.
        let mut prev_heads = Vec::with_capacity(frame.entities.len());
        let mut sibling_cursor = 0usize;
        for slice in &frame.entities {
            if let Some(watched) = self.watched.get(&slice.entity) {
                prev_heads.push(watched.head);
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

        for entity in &entities {
            self.replay_entity(*entity, frame)?;
        }

        for entity in &entities {
            let Some(watched) = self.watched.get_mut(entity) else {
                continue;
            };
            for transition in &transitions {
                if transition.entity == *entity {
                    watched.head = transition.head;
                    watched.anchored = true;
                    // The chain moved, so whatever hole was being repaired is
                    // closed as far as this witness can tell, and judging can
                    // resume.
                    watched.catchup = None;
                }
            }
            if let Some(slice) = frame.entities.iter().find(|slice| slice.entity == *entity) {
                watched.chain_epoch = slice.chain_epoch;
            }
            watched.folded_through = Some(match watched.folded_through {
                Some(folded) => folded.max(last_tick),
                None => last_tick,
            });
        }
        self.log.record_frame(frame.clone(), transitions);

        // A claim may already have arrived for a tick this frame just computed.
        for entity in &entities {
            if let Some(signal) = self.check_pending_claims(*entity) {
                signals.push(signal);
            }
        }

        self.prune_if_due(Tick::new(last_tick));

        // The chain moved, so frames held behind the hole may chain now. This
        // is what stops a repair from leaving the witness behind by exactly the
        // round trip it took: without it, everything the subject sent while the
        // repair was in flight is gone and has to be asked for again, which is
        // how one hole becomes a queue of them.
        signals.extend(self.drain_deferred(subject));
        Ok(signals)
    }

    /// Hold a frame that could not chain, so the repair in front of it does not
    /// cost the timeline that arrived while it was outstanding.
    ///
    /// Bounded per subject. The buffer only has to span a repair round trip —
    /// [`Self::REPAIR_TIMEOUT_TICKS`] of the subject's timeline, which at the
    /// three-tick frames the send cadence produces is twenty frames — and a
    /// subject whose hole is not closing is one the escalation path already
    /// handles. Past the cap the oldest goes, because the oldest is the one the
    /// repair is most likely to have covered already.
    fn buffer_deferred(
        &mut self,
        subject: NodeId,
        frame: &LogFrame,
        sibling_heads: &[(ChainHash, ChainHash)],
    ) {
        let key = (*subject.as_bytes(), frame.first_tick.0);
        self.deferred
            .insert(key, (frame.clone(), sibling_heads.to_vec()));
        let held = self.held_for(subject);
        if held.len() > Self::MAX_DEFERRED_FRAMES {
            let oldest = held[0];
            self.deferred.remove(&oldest);
            self.counters.deferrals_overflowed += 1;
        }
    }

    /// Keys of the frames held for `subject`, in tick order.
    fn held_for(&self, subject: NodeId) -> Vec<DeferredKey> {
        let bytes = *subject.as_bytes();
        self.deferred
            .range((bytes, 0)..=(bytes, u64::MAX))
            .map(|(key, _)| *key)
            .collect()
    }

    /// Re-offer the frames held for `subject`, oldest first, until one of them
    /// still cannot chain.
    ///
    /// Stopping at the first refusal is not an optimisation: the frames are in
    /// tick order, so a frame that cannot chain means every frame behind it
    /// cannot either, and offering them would only re-open the same repair
    /// once per frame.
    fn drain_deferred(&mut self, subject: NodeId) -> Vec<WitnessSignal> {
        if self.draining {
            return Vec::new();
        }
        self.draining = true;
        let mut signals = Vec::new();
        for key in self.held_for(subject) {
            let Some((frame, siblings)) = self.deferred.remove(&key) else {
                continue;
            };
            match self.ingest_frame(&frame, &siblings) {
                Ok(produced) => signals.extend(produced),
                // Counted where it was decided; a frame that will not verify is
                // not a reason to hold on to the ones behind it.
                Err(_) => continue,
            }
            // `ingest_frame` puts it straight back when it still cannot chain.
            if self.deferred.contains_key(&key) {
                break;
            }
            self.counters.frames_recovered += 1;
        }
        self.draining = false;
        signals
    }

    /// Re-execute one entity's slice of a verified frame.
    ///
    /// Every tick the frame covers is stepped, including ticks that logged
    /// nothing: a silent tick still advances state and still draws from the
    /// RNG, so skipping it would put the witness on a different trajectory than
    /// the authority for reasons that have nothing to do with cheating.
    fn replay_entity(&mut self, entity: PersistId, frame: &LogFrame) -> Result<(), WitnessError> {
        let mut per_tick: BTreeMap<u64, Vec<R::CoreInput>> = BTreeMap::new();
        for slice in &frame.entities {
            if slice.entity != entity {
                continue;
            }
            for record in &slice.records {
                if !matches!(
                    record.source,
                    RecordSource::OwnPlayer { .. }
                        | RecordSource::Player { .. }
                        | RecordSource::InboundEvent { .. }
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

        let watched = self
            .watched
            .get_mut(&entity)
            .ok_or(WitnessError::NotWatched)?;
        let mut recorded = Vec::with_capacity(usize::from(frame.tick_count));
        for tick in frame.first_tick.0..frame.first_tick.0 + u64::from(frame.tick_count) {
            let inputs = per_tick.remove(&tick).unwrap_or_default();
            let Some(TickOutcome { state_hash, .. }) =
                watched
                    .executor
                    .step_entity(entity, Tick::new(tick), &inputs)
            else {
                return Err(WitnessError::NotWatched);
            };
            watched.computed.insert(tick, state_hash);
            if let Some(state) = watched.executor.state(entity) {
                watched
                    .recent
                    .insert(tick, orrery_core::CoreCodec::to_canonical(state));
            }
            recorded.push((tick, state_hash));
            self.counters.judged_ticks += 1;
        }
        while watched.recent.len() > Self::RECENT_SNAPSHOTS {
            let oldest = *watched.recent.keys().next().expect("len > 0");
            watched.recent.remove(&oldest);
        }

        for (tick, hash) in recorded {
            self.log.record_tick_hash(entity, Tick::new(tick), hash);
        }
        Ok(())
    }

    /// Whether one entity's slice of `frame` chains onto what this witness has
    /// folded. The plain chain question, with none of the repair bookkeeping
    /// [`Self::repair_check`] does on top of it.
    fn chains(&self, entity: PersistId, frame: &LogFrame) -> bool {
        let Some(watched) = self.watched.get(&entity) else {
            return true;
        };
        let Some(slice) = frame.entities.iter().find(|slice| slice.entity == entity) else {
            return true;
        };
        !watched.anchored || slice.prev_head == watched.head.rolling()
    }

    /// Decide what a frame that does not chain means for one entity.
    fn repair_check(&mut self, entity: PersistId, frame: &LogFrame) -> GapCheck {
        let Some(watched) = self.watched.get(&entity) else {
            return GapCheck::Clear;
        };
        let Some(slice) = frame.entities.iter().find(|slice| slice.entity == entity) else {
            return GapCheck::Clear;
        };
        if !watched.anchored || slice.prev_head == watched.head.rolling() {
            return GapCheck::Clear;
        }
        self.repair_step(
            entity,
            frame.first_tick.0,
            frame.first_tick.0,
            slice.chain_epoch,
        )
    }

    /// Advance one entity's repair state, returning what the caller must do.
    ///
    /// Shared by the frame path and [`Self::sweep`] so the backoff, the attempt
    /// count and the escalation threshold cannot drift apart between them.
    ///
    /// One outstanding repair at a time. A frame arriving mid-repair still
    /// fails to chain — that is what "mid-repair" means — and asking again for
    /// the same hole achieves nothing but load. The request is re-issued only
    /// once [`Self::REPAIR_TIMEOUT_TICKS`] of the subject's own timeline have
    /// passed without the hole closing, which is what keeps a *lost* repair
    /// from stalling the witness forever.
    fn repair_step(
        &mut self,
        entity: PersistId,
        now: u64,
        to_tick: u64,
        chain_epoch: u32,
    ) -> GapCheck {
        let Some(watched) = self.watched.get(&entity) else {
            return GapCheck::Clear;
        };

        // The backoff checks come before anything is computed: `sweep` calls
        // this every tick for every entity with an open hole, and the common
        // answer by far is "still waiting".
        if let Some(catchup) = watched.catchup {
            if now < catchup.since + Self::REPAIR_TIMEOUT_TICKS * u64::from(catchup.attempts) {
                return GapCheck::Waiting;
            }
            if catchup.attempts >= Self::MAX_REPAIR_ATTEMPTS {
                if catchup.reported {
                    // Already said. Repeating it on every frame adds nothing an
                    // operator can act on.
                    return GapCheck::Waiting;
                }
                if let Some(watched) = self.watched.get_mut(&entity) {
                    if let Some(catchup) = watched.catchup.as_mut() {
                        catchup.reported = true;
                    }
                }
                // Asking again has stopped being the answer. Not filling a hole
                // across this many chances is the finding — and is what keeps
                // "stall forever" from being a way to stay unjudged.
                self.counters.stalled += 1;
                return GapCheck::Signal(WitnessSignal::Stalled {
                    entity,
                    since: catchup.since,
                    attempts: catchup.attempts,
                });
            }
        }

        let resume = watched
            .computed
            .keys()
            .next_back()
            .map_or(watched.anchor_tick, |tick| tick + 1);

        self.counters.gaps_detected += 1;
        if let Some(watched) = self.watched.get_mut(&entity) {
            watched.catchup = Some(match watched.catchup {
                // Same hole, another attempt: the clock runs from when it was
                // *first* noticed, so a subject cannot reset its own deadline by
                // making the witness ask again.
                Some(catchup) => Catchup {
                    attempts: catchup.attempts + 1,
                    ..catchup
                },
                None => Catchup {
                    since: now,
                    attempts: 1,
                    reported: false,
                },
            });
        }
        // Ask from where this witness actually stopped, not from the beginning
        // of history. `from_tick: 0` looks harmless because retention bounds the
        // answer, but it makes *every* repair a full-history request: the
        // authority then scans and measures its whole retained window, the
        // response is truncated to one datagram, and the requester asks again —
        // so a peer that hitches once drags its witnesses through the entire
        // window repeatedly. A swarm with stalling peers took three orders of
        // magnitude longer than one without because of this line.
        GapCheck::Signal(WitnessSignal::Gap(LogRangeRequest {
            entity,
            chain_epoch,
            from_tick: Tick::new(resume.min(to_tick)),
            to_tick: Tick::new(to_tick.max(resume)),
        }))
    }

    /// Chase repairs that have gone unanswered, driven by the caller's clock.
    ///
    /// Every other repair check hangs off a frame arriving, which closes "stall
    /// forever to stay unjudged" only for a subject that keeps talking. A
    /// subject that simply stops sending would otherwise sit in `catchup`
    /// indefinitely, unjudged and unescalated, which is the cheaper version of
    /// the same trick. Call this from the tick loop with the local tick.
    ///
    /// Returns re-issued [`WitnessSignal::Gap`]s and [`WitnessSignal::Stalled`]
    /// escalations, exactly as the frame path would have.
    pub fn sweep(&mut self, now: Tick) -> Vec<WitnessSignal> {
        let waiting: Vec<(PersistId, u32)> = self
            .watched
            .iter()
            .filter(|(_, watched)| watched.catchup.is_some())
            .map(|(entity, watched)| (*entity, watched.chain_epoch))
            .collect();
        let mut signals = Vec::new();
        for (entity, chain_epoch) in waiting {
            if let GapCheck::Signal(signal) = self.repair_step(entity, now.0, now.0, chain_epoch) {
                signals.push(signal);
            }
        }
        signals
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
            watched.recent.retain(|tick, _| *tick >= floor);
            // One claim below the floor is kept: a claim at tick T is compared
            // against the hash for T-1, so evicting on the same boundary as the
            // hashes would drop the comparison a surviving claim still needs.
            watched.claims.retain(|tick, _| *tick + 1 >= floor);
            watched.snapshotted.retain(|tick| *tick + 1 >= floor);
            watched.reported.retain(|tick| *tick + 1 >= floor);
        }
        // A frame held behind a hole is only worth holding while a bundle could
        // still be assembled from it. Past the floor the per-subject cap is no
        // longer what bounds the buffer — a subject that went quiet mid-hole
        // would otherwise leave its frames there for the rest of the session.
        self.deferred
            .retain(|(_, first_tick), _| *first_tick >= floor);

        // Stage-1 samples are kept for entities this peer does not witness, so
        // nothing else bounds them.
        self.samples.retain(|_, (_, tick)| tick.0 + 1 >= floor);
    }

    /// Ingest a signed claim and compare it against what re-execution produced.
    ///
    /// # The snapshot a witness keeps, and the one it must not
    ///
    /// A claim at tick T commits to the state *before* T executes — the state
    /// this witness computed at T-1. Where the two agree, that state is a
    /// snapshot a bundle can open from, and it is the **only** way a witness
    /// ever gets one: a witness is never sent state, and the anchor it was
    /// handed ages out of retention long before the session does. Without this
    /// a witness that leaves shadow mode can file about its first window and
    /// nothing afterwards.
    ///
    /// Where the two disagree, no snapshot is kept, and that is deliberate: a
    /// t₀ snapshot that failed `load_claimed_snapshot` reads as
    /// `EvidenceForged` **against the reporter**. A bundle has to open at the
    /// last claim the two still shared.
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

        // A hole that will never be filled must not silence this watch for the
        // rest of the session. If this claim is a point the witness can resume
        // from, it resumes here, before anything else reads the watch.
        let resumed = self.try_reanchor(claim);

        let snapshot = resumed.or_else(|| {
            claim.tick.0.checked_sub(1).and_then(|previous| {
                let watched = self.watched.get(&claim.entity)?;
                if *watched.computed.get(&previous)? != claim.state_hash {
                    return None;
                }
                watched.recent.get(&previous).cloned()
            })
        });

        if let Some(watched) = self.watched.get_mut(&claim.entity) {
            watched.claims.insert(claim.tick.0, claim.clone());
            if snapshot.is_some() {
                watched.snapshotted.insert(claim.tick.0);
            }
        }
        self.log
            .record_claim(claim.clone(), snapshot.unwrap_or_default());
        Ok(self.check_pending_claims(claim.entity))
    }

    /// Resume a watch that gave up on a hole, at a later signed claim.
    ///
    /// # Why abandoning a window is better than waiting forever
    ///
    /// Once a hole has survived [`Self::MAX_REPAIR_ATTEMPTS`] the witness has
    /// no way to compute the ticks inside it: re-execution needs the inputs,
    /// and the inputs were in the frames that never arrived. Before this, that
    /// state was terminal — [`Self::repair_step`] returned
    /// [`GapCheck::Waiting`] on every subsequent tick without ever asking
    /// again, and [`Self::check_pending_claims`] declines to judge while a
    /// catchup is open. The subject was therefore never judged again, silently
    /// and for as long as the process lived.
    ///
    /// Measured in `p1-swarm --witness`, every watch reached that state within
    /// about twenty-five simulated seconds, after which the witness counters
    /// did not move again: identical gap, stall and overflow totals at 30 s and
    /// at 120 s of an eight-peer run. The escalation the design relies on to
    /// stop "stall forever to stay unjudged" ([`Catchup`]) does fire — but it
    /// only *reports*, and in shadow mode nothing acts on a report, so stalling
    /// was in fact a way to stay unjudged.
    ///
    /// Abandoning the window converts blind-forever into blind-for-one-window.
    /// The abandoned ticks are counted, not forgotten
    /// ([`WitnessCounters::unjudged_ticks`]), so coverage stays a number an
    /// operator can read rather than an assumption.
    ///
    /// # What makes the new anchor trustworthy
    ///
    /// Exactly what makes the original one trustworthy in [`Self::watch`]: a
    /// claim the subject signed, plus the state it commits to. The state comes
    /// from what replication delivered — stage-1 samples, which every
    /// interested peer already holds (docs/06 §3) — and is accepted only when
    /// [`orrery_core::state_hash`] over it equals the claim's `state_hash`. The
    /// witness is not taking the subject's word for its state: it is taking the
    /// subject's *signature*, checked against bytes it received independently.
    /// A subject cannot re-anchor a witness onto a state it did not commit to,
    /// because it would have to forge its own signature to do it.
    ///
    /// Returns the canonical anchor state when it resumes, so the caller can
    /// retain it as the snapshot a bundle opens at.
    fn try_reanchor(&mut self, claim: &StateClaim) -> Option<Vec<u8>> {
        let watched = self.watched.get(&claim.entity)?;
        let catchup = watched.catchup?;
        // Only a hole that has been given up on. A repair still in flight is
        // one the witness expects to fold, and skipping past it would throw
        // away judgeable ticks that were about to arrive.
        if catchup.attempts < Self::MAX_REPAIR_ATTEMPTS || !catchup.reported {
            return None;
        }
        // Forward only. A retained claim from inside the abandoned window would
        // move the anchor *backwards*, to a point the witness already could not
        // reach, and the next frame would not chain to it either.
        if claim.tick.0 <= watched.anchor_tick {
            return None;
        }
        let (state, _) = self.samples.get(&claim.entity)?;
        if orrery_core::state_hash(state) != claim.state_hash {
            return None;
        }
        let state = state.clone();
        let canonical = orrery_core::CoreCodec::to_canonical(&state);

        // A fresh executor, not a rewound one: the old one's trajectory stopped
        // at the hole, and every tick after it would otherwise be computed from
        // a state the subject left behind.
        let mut executor = Executor::new((self.ruleset_factory)(), self.seed);
        executor.insert(claim.entity, state);

        let abandoned = claim.tick.0.saturating_sub(catchup.since);
        self.counters.reanchors += 1;
        self.counters.unjudged_ticks += abandoned;

        let watched = self.watched.get_mut(&claim.entity)?;
        watched.anchor_tick = claim.tick.0;
        watched.chain_epoch = claim.chain_epoch;
        watched.catchup = None;
        watched.head = claim.input_head;
        // Anchored, unlike a fresh `watch`: this head is the subject's own
        // signed full head at this tick, so the very next frame is a thing the
        // witness can check rather than one it has to take on faith. If that
        // frame does not chain to it, ordinary gap detection re-opens a repair
        // — which is the right answer, because a frame that does not chain to a
        // signed head really is missing chain.
        watched.anchored = true;
        watched.folded_through = None;
        watched.executor = executor;
        watched.computed.clear();
        watched.recent.clear();
        watched.snapshotted.clear();
        watched.reported.clear();
        // Claims from inside the abandoned window can never be judged now —
        // their ticks will not be re-executed — and keeping them would have
        // `check_pending_claims` rescan them for the life of the watch.
        watched.claims.retain(|tick, _| *tick >= claim.tick.0);
        Some(canonical)
    }

    /// Compare every claim whose tick has been re-executed and not yet judged.
    ///
    /// A claim at tick T commits to the state *before* T executes, so it is
    /// compared against the hash computed for T-1. Getting that off by one
    /// would make every honest authority look like a cheat.
    ///
    /// A disputed claim is signalled **once**. Without that, every later frame
    /// rescans the same retained claim, finds the same divergence and raises it
    /// again — hundreds of times per real divergence at the default retention —
    /// so `claim_mismatches` would measure the ingest rate rather than count
    /// divergences, and P4 exists to produce exactly that count.
    fn check_pending_claims(&mut self, entity: PersistId) -> Option<WitnessSignal> {
        let watched = self.watched.get(&entity)?;
        // A witness with a hole in the chain has an incomplete trajectory, and
        // comparing a claim against ticks it never executed is precisely how an
        // honest peer that dropped a packet gets accused. It falls out of the
        // missing hashes anyway; saying it here makes it a rule rather than an
        // accident, and gives the deferral a name in the counters.
        if watched.catchup.is_some() {
            self.counters.judgements_deferred += 1;
            return None;
        }
        let mut diverged: Option<u64> = None;
        for (tick, claim) in &watched.claims {
            if watched.reported.contains(tick) {
                continue;
            }
            let Some(previous) = tick.checked_sub(1) else {
                continue;
            };
            let Some(computed) = watched.computed.get(&previous) else {
                continue;
            };
            if *computed != claim.state_hash {
                diverged = Some(*tick);
                break;
            }
        }
        let at = diverged?;
        if let Some(watched) = self.watched.get_mut(&entity) {
            watched.reported.insert(at);
        }
        self.counters.claim_mismatches += 1;
        Some(WitnessSignal::ClaimMismatch {
            entity,
            at: Tick::new(at),
        })
    }

    /// The window to raise for a divergence found at `at` (stage 2).
    ///
    /// Opens at the newest claim tick this witness holds a snapshot for — the
    /// last point at which witness and subject demonstrably agreed — and closes
    /// at the disputed claim, so the bundle contains something the subject
    /// signed. Bounded by [`WitnessConfig::window_ticks`].
    ///
    /// Returns `None` when no agreed claim is close enough, which is the honest
    /// answer: there is no window this witness can prove anything over.
    #[must_use]
    pub fn audit_window(&self, entity: PersistId, at: Tick) -> Option<(Tick, Tick)> {
        let watched = self.watched.get(&entity)?;
        let earliest = at.0.saturating_sub(self.window_ceiling());
        let start = *watched.snapshotted.range(earliest..at.0).next_back()?;
        Some((Tick::new(start), at))
    }

    /// Assemble a report for a window, honouring shadow mode.
    ///
    /// Returns `WitnessSignal::Report(None)` in shadow mode: the window was
    /// assembled and counted, and nothing is filed. That is the whole posture
    /// of P4 — measure first, enforce later.
    ///
    /// # Errors
    ///
    /// [`WitnessError::NotWatched`] when the entity is not watched, and
    /// [`WitnessError::WindowUnservable`] when the window is out of range or
    /// cannot be assembled from what is held. That is deliberately an error
    /// rather than a quiet `Report(None)`: returning shadow mode's own value
    /// for "could not" is how a witness that has gone structurally mute keeps
    /// looking like one that is merely being careful.
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

        let span = window.1 .0.saturating_sub(window.0 .0);
        if span == 0 || span > self.window_ceiling() {
            return Err(WitnessError::WindowUnservable);
        }
        let computed = self.computed_window(entity, window)?;
        let bundle = self
            .log
            .assemble_bundle(entity, window, computed)
            .map_err(|_| WitnessError::WindowUnservable)?;
        self.counters.reports_filed += 1;
        Ok(WitnessSignal::Report(Some(Box::new(sign_report(
            key, subject, bundle,
        )))))
    }

    /// This witness's own trajectory across a window.
    ///
    /// Every tick has to come from re-execution. Substituting zeros for ticks
    /// the witness never computed would put a run of fabricated hashes into the
    /// one artefact whose whole purpose is to explain itself — advisory or not.
    fn computed_window(
        &self,
        entity: PersistId,
        window: (Tick, Tick),
    ) -> Result<Vec<[u8; 32]>, WitnessError> {
        let watched = self.watched.get(&entity).ok_or(WitnessError::NotWatched)?;
        (window.0 .0..window.1 .0)
            .map(|tick| {
                watched
                    .computed
                    .get(&tick)
                    .copied()
                    .ok_or(WitnessError::WindowUnservable)
            })
            .collect()
    }

    /// Re-execute a window from retained frames, for a caller that wants the
    /// trajectory rather than a report.
    ///
    /// # Errors
    ///
    /// [`WitnessError::NotWatched`] when nothing is retained for the entity,
    /// [`WitnessError::WindowUnservable`] when the window cannot be assembled.
    pub fn replay_window(
        &self,
        entity: PersistId,
        window: (Tick, Tick),
    ) -> Result<EvidenceBundle, WitnessError> {
        let computed = self.computed_window(entity, window)?;
        self.log
            .assemble_bundle(entity, window, computed)
            .map_err(|_| WitnessError::WindowUnservable)
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
