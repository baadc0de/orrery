//! The authority-side retained log (docs/06 §6, "Retention").
//!
//! [`replay`](crate::replay) can verify a bundle; this is what *produces* one.
//! Without it an authority has a hash chain and no memory of it, and every
//! `EvidenceBundle` has to be assembled by hand — which is fine for a test and
//! useless for a peer that has just been asked to justify the last three
//! seconds.
//!
//! What is retained, per entity: the chain records, the frames actually sent
//! (with their signatures and the full head transitions they commit to), the
//! claims, the quantized snapshot at each claim tick, and the per-tick state
//! hashes. That set is exactly what an [`EvidenceBundle`] needs, which is not a
//! coincidence — anything less and a window becomes unservable, anything more
//! is memory a peer spends for nothing.
//!
//! **Retention floor is the adjudication window** (180 ticks / 3 s, D16); the
//! default is 600 ticks (10 s), giving slack for dispute round trips and claim
//! alignment. Order of magnitude: tens of KB per entity.

use std::collections::{BTreeMap, VecDeque};

use orrery_protocol::{
    ChainHash, EvidenceBundle, FrameHead, InputRecord, LogFrame, LogRangeRequest, LogRangeResponse,
    PersistId, StateClaim, Tick, MAX_ADJUDICATION_TICKS,
};

use crate::log::{claim_hash, fold, HeadTransition};

/// Postcard cost of one [`FrameHead`]: two 32-byte hashes plus a varint id.
///
/// Deliberately an over-estimate, so a budget is never overshot by rounding.
const SERVED_HEAD_BYTES: usize = 32 + 32 + 10;

/// Distinct claims retained per tick.
///
/// One is the honest case. Two is what it takes to *hold* a producer to a
/// double-signed tick — an equivocation is proven by a conflicting pair, and a
/// third claim adds nothing a pair does not already establish. The cap is what
/// keeps a peer that streams claims at a witness from growing its retention
/// without bound: a witness retains whatever a subject signs, so "keep them
/// all" is an allocation a stranger controls.
const MAX_CLAIMS_PER_TICK: usize = 2;

/// What retaining a claim did — see [`AuthorityLog::record_claim`].
///
/// Returned rather than silently absorbed because a store that quietly
/// resolves a duplicate is a store that hides a producer bug. gates/p1-swarm signed
/// tick 0 twice for the whole of P4 and it was found by accident; the fix
/// there was in the producer, and a producer that repeats the mistake should
/// hear about it from the first component that can see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRecord {
    /// Nothing was retained at this tick; the claim is now.
    Recorded,
    /// The identical claim was already retained. Idempotent: re-recording a
    /// claim (a witness re-anchoring on one it already holds) is not a fault.
    Repeated,
    /// A *different* claim is already retained at this tick: the producer
    /// signed one tick twice. Both are kept — see [`MAX_CLAIMS_PER_TICK`] —
    /// and `held` is the [`claim_hash`] of the one that was already there, so
    /// a caller can name the pair without digging it back out of the store.
    Conflict {
        /// Hash of the claim already retained at this tick.
        held: [u8; 32],
    },
}

/// What an authority could serve of a [`LogRangeRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedRange {
    /// The frames, in tick order.
    pub response: LogRangeResponse,
    /// Full head pairs for every entity appearing in those frames.
    pub heads: Vec<FrameHead>,
    /// First tick still missing, if the answer was cut short by the budget.
    pub resume_from: Option<Tick>,
}

/// How much history an authority keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Ticks of history retained. Clamped up to [`MAX_ADJUDICATION_TICKS`]:
    /// retaining less than one adjudication window would make an authority
    /// structurally unable to answer for itself.
    pub ticks: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { ticks: 600 }
    }
}

impl Retention {
    /// The effective window, never below the adjudication floor.
    #[must_use]
    pub fn effective_ticks(self) -> u64 {
        self.ticks.max(MAX_ADJUDICATION_TICKS)
    }
}

/// Why a window could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleError {
    /// The window is empty, or longer than [`MAX_ADJUDICATION_TICKS`].
    WindowOutOfRange,
    /// No claim sits exactly at the window start. A bundle has to begin from a
    /// committed state, not from whatever the authority happens to remember.
    NoClaimAtStart,
    /// The snapshot for the starting claim has aged out of retention.
    SnapshotEvicted,
    /// Frames covering the window are missing or non-contiguous — usually the
    /// window has simply aged out.
    IncompleteFrames,
    /// The per-tick hashes for the window are not all retained.
    IncompleteHashes,
}

impl core::fmt::Display for BundleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WindowOutOfRange => f.write_str("window is empty or too long"),
            Self::NoClaimAtStart => f.write_str("no state claim at the window start"),
            Self::SnapshotEvicted => f.write_str("the starting claim's snapshot has aged out"),
            Self::IncompleteFrames => f.write_str("frames for the window are missing"),
            Self::IncompleteHashes => f.write_str("per-tick hashes for the window are missing"),
        }
    }
}

impl core::error::Error for BundleError {}

/// A claim together with the snapshot it commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RetainedClaim {
    claim: StateClaim,
    snapshot: Vec<u8>,
}

/// One entity's retained history.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntityHistory {
    head: ChainHash,
    hashes: BTreeMap<u64, [u8; 32]>,
    claims: VecDeque<RetainedClaim>,
}

impl Default for EntityHistory {
    fn default() -> Self {
        Self {
            // A chain starts at the empty head, not at a zeroed hash that
            // happens to equal it — the constant is the contract.
            head: ChainHash::EMPTY,
            hashes: BTreeMap::new(),
            claims: VecDeque::new(),
        }
    }
}

impl EntityHistory {
    /// Index range of the claims retained at `tick`, in retention order.
    ///
    /// `claims` is kept sorted by tick (see [`AuthorityLog::record_claim`]),
    /// which is what makes this a bisection rather than a scan and what lets
    /// [`Self::chain_from`] walk tick groups in order.
    fn range_at(&self, tick: u64) -> (usize, usize) {
        (
            self.claims.partition_point(|held| held.claim.tick.0 < tick),
            self.claims
                .partition_point(|held| held.claim.tick.0 <= tick),
        )
    }

    /// The claims following `t0` that chain from it, one per tick.
    ///
    /// Returns how long the chaining prefix is, and the claims themselves. At
    /// a tick with two retained claims the one whose `prev_claim` continues
    /// the chain is taken; where neither does, the earliest-retained one is —
    /// which is what a single-claim tick would have produced, so a genuine
    /// break is reported exactly as it was before this selection existed.
    fn follow(&self, t0: usize, window: (Tick, Tick)) -> (usize, Vec<StateClaim>) {
        let (start, end) = window;
        let mut previous = claim_hash(&self.claims[t0].claim);
        let mut chained = 0usize;
        let mut broken = false;
        let mut disputed = Vec::new();
        let mut index = self.range_at(start.0).1;
        while index < self.claims.len() {
            let tick = self.claims[index].claim.tick.0;
            if tick > end.0 {
                break;
            }
            let (group_start, group_end) = self.range_at(tick);
            let pick = (group_start..group_end)
                .find(|candidate| self.claims[*candidate].claim.prev_claim == previous)
                .unwrap_or(group_start);
            if broken || self.claims[pick].claim.prev_claim != previous {
                broken = true;
            } else {
                chained += 1;
            }
            previous = claim_hash(&self.claims[pick].claim);
            disputed.push(self.claims[pick].claim.clone());
            index = group_end;
        }
        (chained, disputed)
    }

    /// The claim to open a window at, with the claims that follow it.
    ///
    /// # Why selection, and not just "the first claim at the tick"
    ///
    /// A bundle whose `t0_claim` and `disputed_claims` do not chain is a
    /// conviction: `verify_bundle` reads the break as the authority having
    /// equivocated about its own history. So when a tick holds two claims, the
    /// choice of which one opens the window decides a verdict — and taking the
    /// first retained one convicted gates/p1-swarm's honest bots, because the anchor
    /// was retained first while every later claim chained from the run loop's
    /// duplicate.
    ///
    /// The candidate is therefore chosen by what the *rest of the retained
    /// history* says: the one the following claims actually chain from, so a
    /// break survives into the bundle only when no retained claim at the tick
    /// explains it. A snapshot outranks chaining, because a window that cannot
    /// be opened at all silences a witness, and a break with an openable
    /// window is adjudicable evidence rather than a mute report.
    fn chain_from(&self, window: (Tick, Tick)) -> Option<(&RetainedClaim, Vec<StateClaim>)> {
        let (first, last) = self.range_at(window.0 .0);
        let mut best: Option<((bool, usize), usize, Vec<StateClaim>)> = None;
        for candidate in first..last {
            let (chained, disputed) = self.follow(candidate, window);
            let score = (!self.claims[candidate].snapshot.is_empty(), chained);
            // Strictly better wins, so a tie keeps the earliest retained
            // candidate and selection stays a pure function of the log.
            if best.as_ref().is_none_or(|(held, _, _)| score > *held) {
                best = Some((score, candidate, disputed));
            }
        }
        let (_, candidate, disputed) = best?;
        Some((&self.claims[candidate], disputed))
    }
}

/// An authority's retained log across every entity it holds.
///
/// Frames are stored once for all entities rather than per entity: one frame
/// covers everything the authority authored in that send, and a bundle needs
/// the *other* entities' head transitions to rebuild the signature preimage.
/// Splitting them per entity would throw away exactly the data a sibling
/// reconstruction needs.
#[derive(Debug, Clone)]
pub struct AuthorityLog {
    retention: Retention,
    entities: BTreeMap<PersistId, EntityHistory>,
    frames: VecDeque<(LogFrame, Vec<HeadTransition>)>,
    conflicting_claims: u64,
    conflicting_tick_hashes: u64,
}

impl Default for AuthorityLog {
    fn default() -> Self {
        Self::new(Retention::default())
    }
}

impl AuthorityLog {
    /// An empty log with the given retention.
    #[must_use]
    pub fn new(retention: Retention) -> Self {
        Self {
            retention,
            entities: BTreeMap::new(),
            frames: VecDeque::new(),
            conflicting_claims: 0,
            conflicting_tick_hashes: 0,
        }
    }

    /// The retention policy in force.
    #[must_use]
    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// An entity's current chain head.
    #[must_use]
    pub fn head(&self, entity: PersistId) -> ChainHash {
        self.entities
            .get(&entity)
            .map_or(ChainHash::EMPTY, |history| history.head)
    }

    /// Fold a record into an entity's chain, returning the new head.
    ///
    /// The record itself is not retained here — it travels in a frame, and the
    /// frame is what gets retained, because a bundle serves *sent* frames with
    /// their signatures rather than a reconstruction of them.
    pub fn append(&mut self, entity: PersistId, record: &InputRecord) -> ChainHash {
        let history = self.entities.entry(entity).or_default();
        history.head = fold(history.head, record);
        history.head
    }

    /// Record the per-tick state hash an entity's execution produced.
    ///
    /// Last write wins, and a write that *changes* a retained hash is counted
    /// in [`Self::conflicting_tick_hashes`]. Unlike a claim, a tick hash is
    /// never judged — `claimed_hashes` is reporter-supplied and
    /// [`verify_bundle`](crate::verify_bundle) reaches a verdict only from
    /// signed claims — so overwriting one cannot convict anybody. It can still
    /// only happen if the same tick was executed twice to different ends,
    /// which is a producer bug worth being able to see.
    pub fn record_tick_hash(&mut self, entity: PersistId, tick: Tick, hash: [u8; 32]) {
        let history = self.entities.entry(entity).or_default();
        let conflict = history
            .hashes
            .insert(tick.0, hash)
            .is_some_and(|held| held != hash);
        if conflict {
            self.conflicting_tick_hashes += 1;
        }
    }

    /// Retain a frame the authority sent, with the transitions it commits to.
    ///
    /// A frame identical to one already retained is dropped. Frames are
    /// checked for contiguity at assembly, so a repeat would not corrupt a
    /// bundle — it would make the window *unservable*
    /// ([`BundleError::IncompleteFrames`], because the repeat does not start
    /// where its predecessor ended) and mute a witness that had done nothing
    /// wrong. A byte-identical frame carries no information either way.
    pub fn record_frame(&mut self, frame: LogFrame, transitions: Vec<HeadTransition>) {
        if self
            .frames
            .iter()
            .any(|(held, _)| held.first_tick == frame.first_tick && *held == frame)
        {
            return;
        }
        self.frames.push_back((frame, transitions));
    }

    /// Ticks at which a producer signed two different claims.
    ///
    /// Non-zero means a producer bug (or an equivocating subject, when this log
    /// is a witness's): the store resolves the duplicate so it cannot
    /// manufacture a verdict, and counts it so the resolution is not the same
    /// thing as the fault going unnoticed.
    #[must_use]
    pub fn conflicting_claims(&self) -> u64 {
        self.conflicting_claims
    }

    /// Ticks whose retained state hash was overwritten with a different one.
    #[must_use]
    pub fn conflicting_tick_hashes(&self) -> u64 {
        self.conflicting_tick_hashes
    }

    /// Retain a claim and the quantized snapshot it commits to.
    ///
    /// The snapshot is kept because a claim is a hash, not state: an
    /// adjudicator needs somewhere to start replaying from, and the claim only
    /// proves that the served bytes are the ones committed to.
    ///
    /// # One tick, two claims
    ///
    /// Claims are retained sorted by tick, at most [`MAX_CLAIMS_PER_TICK`] per
    /// tick, and a repeat of a claim already held is idempotent. Sorting is not
    /// tidiness: [`Self::assemble_bundle`] walks tick groups in order, and a
    /// witness retains claims as they arrive from a subject that owes it no
    /// ordering.
    ///
    /// A *conflicting* pair — same tick, different claim — is kept rather than
    /// resolved here, because at record time the log cannot know which of the
    /// two the producer went on to chain from, and picking wrong is what
    /// manufactures a false conviction. The choice belongs where the evidence
    /// to make it exists: assembly, which can see what the following claims
    /// chain to. The return value reports the conflict so a caller need not
    /// discover its own double-signing by adjudication.
    ///
    /// A claim recorded without a snapshot inherits a sibling's when both
    /// commit to the same `state_hash` at that tick. The snapshot is a
    /// commitment to *state*, not to a chain position, and
    /// [`verify_bundle`](crate::verify_bundle) checks it against the claim's
    /// own `state_hash` — so sharing it is sound, and refusing to would leave
    /// the chaining claim unopenable and the window unservable. This is the
    /// witness case exactly: it is handed a snapshot with the anchor and none
    /// with the streamed claims that follow.
    pub fn record_claim(&mut self, claim: StateClaim, snapshot: Vec<u8>) -> ClaimRecord {
        let tick = claim.tick.0;
        let history = self.entities.entry(claim.entity).or_default();
        let (first, last) = history.range_at(tick);

        if let Some(index) = (first..last).find(|held| history.claims[*held].claim == claim) {
            if history.claims[index].snapshot.is_empty() && !snapshot.is_empty() {
                history.claims[index].snapshot = snapshot;
            }
            return ClaimRecord::Repeated;
        }

        let snapshot = if snapshot.is_empty() {
            (first..last)
                .find(|held| {
                    let sibling = &history.claims[*held];
                    sibling.claim.state_hash == claim.state_hash && !sibling.snapshot.is_empty()
                })
                .map_or(snapshot, |held| history.claims[held].snapshot.clone())
        } else {
            snapshot
        };

        let outcome = if first == last {
            ClaimRecord::Recorded
        } else {
            ClaimRecord::Conflict {
                held: claim_hash(&history.claims[first].claim),
            }
        };
        history
            .claims
            .insert(last, RetainedClaim { claim, snapshot });
        if last + 1 - first > MAX_CLAIMS_PER_TICK {
            // Keep the earliest and the newest: those are the two a following
            // claim can chain from, and the pair that proves the conflict.
            history.claims.remove(first + 1);
        }
        if matches!(outcome, ClaimRecord::Conflict { .. }) {
            self.conflicting_claims += 1;
        }
        outcome
    }

    /// Answer a [`LogRangeRequest`] with as many retained frames as fit.
    ///
    /// # Partial by design
    ///
    /// A 180-tick window does not fit in one datagram, so this serves frames in
    /// tick order until `budget` bytes are spent and reports the first tick
    /// still missing. The requester asks again from there. Truncating without
    /// saying so would leave a witness believing the gap was unfillable — which
    /// is the one witness input that *is* reportable, so silence there would
    /// manufacture accusations out of a packet-size limit.
    ///
    /// An empty response with `resume_from: None` is the honest "cannot serve
    /// it": the range fell out of retention, or nothing was ever logged for it.
    ///
    /// Head pairs accompany the frames because a requester that is missing part
    /// of a chain cannot fold the entities it does not follow, and without every
    /// entity's full pair it cannot rebuild the signature preimage.
    #[must_use]
    pub fn serve_range(&self, request: &LogRangeRequest, budget: usize) -> ServedRange {
        let mut frames = Vec::new();
        let mut heads: Vec<FrameHead> = Vec::new();
        let mut spent = 0usize;
        let mut resume_from = None;

        for (frame, transitions) in &self.frames {
            let first = frame.first_tick.0;
            let end = first + u64::from(frame.tick_count);
            // Half-open on both sides: a frame overlapping the request at all
            // is part of the answer, because a chain cannot be folded from a
            // partial frame — the signature covers the whole thing.
            if end <= request.from_tick.0 || first >= request.to_tick.0 {
                continue;
            }
            if !frame
                .entities
                .iter()
                .any(|slice| slice.entity == request.entity)
            {
                continue;
            }

            let new_heads: Vec<FrameHead> = transitions
                .iter()
                .filter(|transition| !heads.iter().any(|held| held.entity == transition.entity))
                .map(|transition| FrameHead {
                    entity: transition.entity,
                    prev_head: transition.prev_head,
                    head: transition.head,
                })
                .collect();

            let cost = postcard::to_stdvec(frame).map_or(usize::MAX, |bytes| bytes.len())
                + new_heads.len() * SERVED_HEAD_BYTES;
            if spent + cost > budget && !frames.is_empty() {
                // Out of room, and something is already going out: stop here
                // and tell the requester where to resume.
                resume_from = Some(Tick::new(first));
                break;
            }
            if spent + cost > budget {
                // The very first frame does not fit. Sending nothing with a
                // resume point would loop forever; report it as unservable so
                // the requester escalates instead of retrying.
                break;
            }
            spent += cost;
            heads.extend(new_heads);
            frames.push(frame.clone());
        }

        ServedRange {
            response: LogRangeResponse {
                entity: request.entity,
                frames,
            },
            heads,
            resume_from,
        }
    }

    /// Drop history older than the retention window relative to `now`.
    pub fn prune(&mut self, now: Tick) {
        let floor = now.0.saturating_sub(self.retention.effective_ticks());
        self.frames
            .retain(|(frame, _)| frame.first_tick.0 + u64::from(frame.tick_count) > floor);
        for history in self.entities.values_mut() {
            history.hashes.retain(|tick, _| *tick >= floor);
            // A claim is kept while its *snapshot* can still start a servable
            // window, so the retained set is claims at or after the floor plus
            // the one immediately before it — which is the claim any window
            // opening at the floor would have to begin from.
            while history.claims.len() > 1 && history.claims[1].claim.tick.0 <= floor {
                history.claims.pop_front();
            }
        }
    }

    /// Assemble a self-verifying bundle for `entity` over `window`.
    ///
    /// `window` is half-open and must start exactly at a retained claim. The
    /// caller supplies the reporter's own re-execution as `computed_hashes`;
    /// an authority answering for itself passes its claimed trajectory for
    /// both, and a witness reporting a dispute passes what it computed.
    ///
    /// # Errors
    ///
    /// [`BundleError`] when the window is out of range or has aged out.
    pub fn assemble_bundle(
        &self,
        entity: PersistId,
        window: (Tick, Tick),
        computed_hashes: Vec<[u8; 32]>,
    ) -> Result<EvidenceBundle, BundleError> {
        let (start, end) = window;
        let span = end.0.saturating_sub(start.0);
        if span == 0 || span > MAX_ADJUDICATION_TICKS {
            return Err(BundleError::WindowOutOfRange);
        }
        let history = self
            .entities
            .get(&entity)
            .ok_or(BundleError::NoClaimAtStart)?;

        // The claim that opens the window is the one the rest of the retained
        // claims chain from — not merely the first one held at that tick. See
        // `EntityHistory::chain_from`: with one claim per tick these are the
        // same claim, and with two the difference is a verdict.
        let (t0, disputed_claims) = history
            .chain_from((start, end))
            .ok_or(BundleError::NoClaimAtStart)?;
        if t0.snapshot.is_empty() {
            return Err(BundleError::SnapshotEvicted);
        }

        let mut claimed_hashes = Vec::with_capacity(span as usize);
        for tick in start.0..end.0 {
            claimed_hashes.push(
                *history
                    .hashes
                    .get(&tick)
                    .ok_or(BundleError::IncompleteHashes)?,
            );
        }

        // Frames intersecting the window, in order, checked for contiguity as
        // they are collected: a gap here means the window cannot be replayed,
        // and discovering that at the adjudicator is worse than here.
        let mut frames = Vec::new();
        let mut sibling_heads = Vec::new();
        let mut expected_next: Option<u64> = None;
        for (frame, transitions) in &self.frames {
            let frame_end = frame.first_tick.0 + u64::from(frame.tick_count);
            if frame_end <= start.0 || frame.first_tick.0 >= end.0 {
                continue;
            }
            if !frame.entities.iter().any(|slice| slice.entity == entity) {
                continue;
            }
            if let Some(expected) = expected_next {
                if frame.first_tick.0 != expected {
                    return Err(BundleError::IncompleteFrames);
                }
            }
            expected_next = Some(frame_end);

            let siblings = frame
                .entities
                .iter()
                .filter(|slice| slice.entity != entity)
                .map(|slice| {
                    transitions
                        .iter()
                        .find(|transition| transition.entity == slice.entity)
                        .map(|transition| (transition.prev_head, transition.head))
                        .ok_or(BundleError::IncompleteFrames)
                })
                .collect::<Result<Vec<_>, _>>()?;
            frames.push(frame.clone());
            sibling_heads.push(siblings);
        }
        if frames.is_empty() {
            return Err(BundleError::IncompleteFrames);
        }

        Ok(EvidenceBundle {
            ruleset: t0.claim.ruleset,
            entity,
            window_start: start,
            window_end: end,
            t0_claim: t0.claim.clone(),
            t0_snapshot: bytes::Bytes::from(t0.snapshot.clone()),
            frames,
            sibling_heads,
            disputed_claims,
            claimed_hashes,
            computed_hashes,
        })
    }

    /// The claimed per-tick hashes the log retains for a window, for a caller
    /// that wants to answer for itself without re-executing.
    #[must_use]
    pub fn claimed_hashes(&self, entity: PersistId, window: (Tick, Tick)) -> Option<Vec<[u8; 32]>> {
        let history = self.entities.get(&entity)?;
        (window.0 .0..window.1 .0)
            .map(|tick| history.hashes.get(&tick).copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{EntitySlice, RecordSource, RulesetId};

    const ENTITY: PersistId = PersistId::new(1);

    fn ruleset() -> RulesetId {
        RulesetId {
            version: 1,
            digest: [3; 32],
        }
    }

    fn record(seq: u16) -> InputRecord {
        InputRecord {
            tick_off: 0,
            seq,
            source: RecordSource::InboundEvent {
                from: PersistId::new(9),
            },
            payload: bytes::Bytes::from_static(b"x"),
        }
    }

    fn claim(tick: u64) -> StateClaim {
        StateClaim {
            entity: ENTITY,
            chain_epoch: 0,
            tick: Tick::new(tick),
            input_head: ChainHash::EMPTY,
            state_hash: [0; 32],
            prev_claim: [0; 32],
            ruleset: ruleset(),
            sig: iroh_base::SecretKey::from_bytes(&[1; 32]).sign(b"x"),
        }
    }

    fn frame(first_tick: u64, entities: Vec<PersistId>) -> (LogFrame, Vec<HeadTransition>) {
        let slices: Vec<_> = entities
            .iter()
            .map(|entity| EntitySlice {
                entity: *entity,
                chain_epoch: 0,
                prev_head: ChainHash::EMPTY.rolling(),
                records: vec![record(0)],
                head: ChainHash::EMPTY.rolling(),
            })
            .collect();
        let transitions = entities
            .iter()
            .map(|entity| HeadTransition {
                entity: *entity,
                prev_head: ChainHash::EMPTY,
                head: ChainHash([7; 32]),
            })
            .collect();
        (
            LogFrame {
                ruleset: ruleset(),
                first_tick: Tick::new(first_tick),
                tick_count: 3,
                entities: slices,
                sig: iroh_base::SecretKey::from_bytes(&[1; 32]).sign(b"x"),
            },
            transitions,
        )
    }

    fn populated() -> AuthorityLog {
        let mut log = AuthorityLog::default();
        log.record_claim(claim(100), vec![1, 2, 3]);
        for tick in 100..106 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [tick as u8; 32]);
        }
        for first in [100, 103] {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        log
    }

    #[test]
    fn appending_advances_the_head_deterministically() {
        let mut a = AuthorityLog::default();
        let mut b = AuthorityLog::default();
        a.append(ENTITY, &record(0));
        a.append(ENTITY, &record(1));
        b.append(ENTITY, &record(0));
        b.append(ENTITY, &record(1));
        assert_eq!(a.head(ENTITY), b.head(ENTITY));
        assert_ne!(a.head(ENTITY), ChainHash::EMPTY);
    }

    #[test]
    fn a_window_is_served_from_a_claim_with_its_snapshot() {
        let log = populated();
        let bundle = log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .expect("window is retained");
        assert_eq!(bundle.entity, ENTITY);
        assert_eq!(bundle.t0_claim.tick, Tick::new(100));
        assert_eq!(bundle.t0_snapshot.as_ref(), &[1, 2, 3]);
        assert_eq!(bundle.claimed_hashes.len(), 6);
        assert_eq!(bundle.frames.len(), 2);
    }

    /// A claim at `tick` chaining from `previous`, otherwise like [`claim`].
    fn chained_claim(tick: u64, previous: [u8; 32], state: u8) -> StateClaim {
        let mut built = claim(tick);
        built.prev_claim = previous;
        built.state_hash = [state; 32];
        crate::log::sign_claim(&iroh_base::SecretKey::from_bytes(&[1; 32]), &mut built);
        built
    }

    #[test]
    fn a_tick_claimed_twice_is_reported_rather_than_silently_resolved() {
        // A store that quietly picks one of two claims hides a producer bug:
        // gates/p1-swarm signed tick 0 twice for the whole of P4 and it surfaced by
        // accident. Recording the same claim again is not a fault; recording a
        // different one at the same tick is.
        let mut log = AuthorityLog::default();
        let first = chained_claim(100, [0; 32], 1);
        assert_eq!(
            log.record_claim(first.clone(), vec![1]),
            ClaimRecord::Recorded
        );
        assert_eq!(
            log.record_claim(first.clone(), vec![1]),
            ClaimRecord::Repeated
        );
        assert_eq!(log.conflicting_claims(), 0);

        let second = chained_claim(100, claim_hash(&first), 1);
        assert_eq!(
            log.record_claim(second, Vec::new()),
            ClaimRecord::Conflict {
                held: claim_hash(&first)
            }
        );
        assert_eq!(log.conflicting_claims(), 1);
    }

    #[test]
    fn a_second_claim_at_a_tick_inherits_the_snapshot_of_the_state_it_shares() {
        // The witness case: it is handed a snapshot with the anchor and none
        // with the claims that stream in after. A snapshot commits to a state
        // hash, not to a chain position, so the two claims at one tick share
        // it — and without that the chaining claim could not open a window at
        // all, which silences a witness instead of convicting anyone.
        let mut log = AuthorityLog::default();
        let anchor = chained_claim(100, [0; 32], 1);
        log.record_claim(anchor.clone(), vec![1, 2, 3]);
        log.record_claim(chained_claim(100, claim_hash(&anchor), 1), Vec::new());
        for tick in 100..106 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [tick as u8; 32]);
        }
        for first in [100, 103] {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        let bundle = log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .expect("the shared snapshot makes the window servable");
        assert_eq!(bundle.t0_snapshot.as_ref(), &[1, 2, 3]);

        // A differing state at the same tick is a different commitment; there
        // is nothing to share, and the window opens at the claim that can.
        let mut other = AuthorityLog::default();
        other.record_claim(anchor.clone(), vec![1, 2, 3]);
        other.record_claim(chained_claim(100, claim_hash(&anchor), 9), Vec::new());
        for tick in 100..106 {
            other.record_tick_hash(ENTITY, Tick::new(tick), [tick as u8; 32]);
        }
        for first in [100, 103] {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            other.record_frame(frame, transitions);
        }
        let bundle = other
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .expect("openable at the claim that has a snapshot");
        assert_eq!(bundle.t0_claim.state_hash, [1; 32]);
    }

    #[test]
    fn a_window_opens_at_the_claim_the_later_claims_chain_from() {
        // The false-conviction shape, at the unit level: two claims at the
        // window start, and the claim inside the window chains from the second.
        // Taking the first would put a break into the bundle that the retained
        // history does not contain.
        let mut log = AuthorityLog::default();
        let anchor = chained_claim(100, [0; 32], 1);
        let duplicate = chained_claim(100, claim_hash(&anchor), 1);
        let later = chained_claim(103, claim_hash(&duplicate), 2);
        log.record_claim(anchor, vec![1, 2, 3]);
        log.record_claim(duplicate.clone(), Vec::new());
        log.record_claim(later.clone(), vec![4]);
        for tick in 100..106 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [tick as u8; 32]);
        }
        for first in [100, 103] {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }

        let bundle = log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .expect("window is servable");
        assert_eq!(bundle.t0_claim, duplicate);
        assert_eq!(bundle.disputed_claims, vec![later]);
    }

    #[test]
    fn a_break_no_retained_claim_explains_still_reaches_the_bundle() {
        // Selection must not launder a real discrepancy. When nothing retained
        // at the tick chains, the bundle carries the break exactly as it did
        // before selection existed — this is evidence, not a bug.
        let mut log = AuthorityLog::default();
        let anchor = chained_claim(100, [0; 32], 1);
        let orphan = chained_claim(103, [0x11; 32], 2);
        log.record_claim(anchor.clone(), vec![1]);
        log.record_claim(orphan.clone(), vec![2]);
        for tick in 100..106 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [0; 32]);
        }
        for first in [100, 103] {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        let bundle = log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .expect("window is servable");
        assert_eq!(bundle.t0_claim, anchor);
        assert_eq!(bundle.disputed_claims, vec![orphan]);
    }

    #[test]
    fn claims_are_retained_in_tick_order_however_they_arrive() {
        // A witness retains what a subject sends it, and a subject owes it no
        // ordering. `assemble_bundle` walks the claims in order, so arrival
        // order must not decide what a bundle contains.
        let mut log = AuthorityLog::default();
        let first = chained_claim(100, [0; 32], 1);
        let second = chained_claim(103, claim_hash(&first), 2);
        log.record_claim(second.clone(), vec![2]);
        log.record_claim(first.clone(), vec![1]);
        for tick in 100..106 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [0; 32]);
        }
        for start in [100, 103] {
            let (frame, transitions) = frame(start, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        let bundle = log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .expect("window is servable");
        assert_eq!(bundle.t0_claim, first);
        assert_eq!(bundle.disputed_claims, vec![second]);
    }

    #[test]
    fn a_third_claim_at_one_tick_keeps_the_first_and_the_newest() {
        // Retention a stranger controls has to be bounded: a witness retains
        // whatever its subject signs. Two is what proves a conflict; the pair
        // kept is the earliest and the newest, which are the two a following
        // claim can chain from.
        let mut log = AuthorityLog::default();
        let first = chained_claim(100, [0; 32], 1);
        let second = chained_claim(100, claim_hash(&first), 1);
        let third = chained_claim(100, claim_hash(&second), 1);
        log.record_claim(first.clone(), vec![1]);
        log.record_claim(second, vec![2]);
        log.record_claim(third.clone(), vec![3]);
        let history = log.entities.get(&ENTITY).expect("entity retained");
        assert_eq!(history.claims.len(), MAX_CLAIMS_PER_TICK);
        assert_eq!(history.claims[0].claim, first);
        assert_eq!(history.claims[1].claim, third);
        assert_eq!(log.conflicting_claims(), 2);
    }

    #[test]
    fn a_repeated_frame_does_not_make_the_window_unservable() {
        // Frames are checked for contiguity at assembly, so a byte-identical
        // repeat would fail `IncompleteFrames` and mute a witness rather than
        // corrupt a bundle. It carries no information; it is dropped.
        let mut log = AuthorityLog::default();
        log.record_claim(claim(100), vec![1]);
        for tick in 100..106 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [0; 32]);
        }
        for start in [100, 100, 103] {
            let (frame, transitions) = frame(start, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        assert!(log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(106)), vec![[0; 32]; 6])
            .is_ok());
    }

    #[test]
    fn a_tick_hash_overwritten_with_a_different_one_is_counted() {
        let mut log = AuthorityLog::default();
        log.record_tick_hash(ENTITY, Tick::new(100), [1; 32]);
        log.record_tick_hash(ENTITY, Tick::new(100), [1; 32]);
        assert_eq!(log.conflicting_tick_hashes(), 0);
        log.record_tick_hash(ENTITY, Tick::new(100), [2; 32]);
        assert_eq!(log.conflicting_tick_hashes(), 1);
    }

    #[test]
    fn a_window_that_does_not_start_at_a_claim_is_refused() {
        // A bundle has to begin from a committed state. Starting from whatever
        // the authority happens to remember would let it choose its own
        // starting point after the fact.
        let log = populated();
        assert_eq!(
            log.assemble_bundle(ENTITY, (Tick::new(101), Tick::new(105)), vec![[0; 32]; 4]),
            Err(BundleError::NoClaimAtStart)
        );
    }

    #[test]
    fn an_oversized_window_is_refused_before_anything_is_collected() {
        let log = populated();
        assert_eq!(
            log.assemble_bundle(ENTITY, (Tick::new(100), Tick::new(100 + 181)), Vec::new()),
            Err(BundleError::WindowOutOfRange)
        );
        assert_eq!(
            log.assemble_bundle(ENTITY, (Tick::new(100), Tick::new(100)), Vec::new()),
            Err(BundleError::WindowOutOfRange)
        );
    }

    #[test]
    fn a_gap_in_the_frames_is_caught_here_rather_than_at_the_adjudicator() {
        let mut log = AuthorityLog::default();
        log.record_claim(claim(100), vec![1]);
        for tick in 100..109 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [0; 32]);
        }
        // 100..103 and 106..109 — the middle frame never arrived.
        for first in [100, 106] {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        assert_eq!(
            log.assemble_bundle(ENTITY, (Tick::new(100), Tick::new(109)), vec![[0; 32]; 9]),
            Err(BundleError::IncompleteFrames)
        );
    }

    #[test]
    fn sibling_head_transitions_are_carried_for_multi_entity_frames() {
        // One signature covers every authored entity, so a bundle without the
        // siblings' full head pairs cannot rebuild the preimage.
        let mut log = AuthorityLog::default();
        log.record_claim(claim(100), vec![1]);
        for tick in 100..103 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [0; 32]);
        }
        let (frame, transitions) = frame(100, vec![ENTITY, PersistId::new(2)]);
        log.record_frame(frame, transitions);

        let bundle = log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(103)), vec![[0; 32]; 3])
            .expect("retained");
        assert_eq!(bundle.sibling_heads.len(), 1);
        assert_eq!(bundle.sibling_heads[0].len(), 1);
        assert_eq!(bundle.sibling_heads[0][0].1, ChainHash([7; 32]));
    }

    #[test]
    fn retention_never_drops_below_one_adjudication_window() {
        // An authority retaining less than a window could not answer for
        // itself at all — every dispute would be unadjudicable by construction.
        assert_eq!(
            Retention { ticks: 10 }.effective_ticks(),
            MAX_ADJUDICATION_TICKS
        );
        assert_eq!(Retention::default().effective_ticks(), 600);
    }

    #[test]
    fn pruning_drops_aged_history_but_keeps_the_window_openable() {
        let mut log = AuthorityLog::new(Retention { ticks: 180 });
        log.record_claim(claim(100), vec![1]);
        log.record_claim(claim(400), vec![2]);
        for tick in 100..500 {
            log.record_tick_hash(ENTITY, Tick::new(tick), [0; 32]);
        }
        for first in (100..500).step_by(3) {
            let (frame, transitions) = frame(first, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }

        log.prune(Tick::new(500));

        // The aged claim is gone; the one a window at the floor would open
        // from is kept.
        assert!(log
            .assemble_bundle(ENTITY, (Tick::new(100), Tick::new(160)), vec![[0; 32]; 60])
            .is_err());
        assert!(log
            .assemble_bundle(ENTITY, (Tick::new(400), Tick::new(460)), vec![[0; 32]; 60])
            .is_ok());
    }

    /// A log holding `count` frames of 3 ticks each, starting at tick 0.
    fn log_with_frames(count: u64) -> AuthorityLog {
        let mut log = AuthorityLog::default();
        for index in 0..count {
            let (frame, transitions) = frame(index * 3, vec![ENTITY]);
            log.record_frame(frame, transitions);
        }
        log
    }

    fn request(from: u64, to: u64) -> LogRangeRequest {
        LogRangeRequest {
            entity: ENTITY,
            chain_epoch: 0,
            from_tick: Tick::new(from),
            to_tick: Tick::new(to),
        }
    }

    #[test]
    fn a_range_is_served_with_the_head_pairs_needed_to_verify_it() {
        // A requester missing part of a chain cannot fold entities it does not
        // follow, and without every entity's full pair it cannot rebuild the
        // frame preimage — so frames alone are unverifiable.
        let log = log_with_frames(4);
        let served = log.serve_range(&request(0, 12), usize::MAX);
        assert_eq!(served.response.frames.len(), 4);
        assert_eq!(served.resume_from, None);
        assert!(served.heads.iter().any(|head| head.entity == ENTITY));
    }

    #[test]
    fn only_frames_overlapping_the_request_are_served() {
        let log = log_with_frames(10);
        let served = log.serve_range(&request(6, 12), usize::MAX);
        let ticks: Vec<u64> = served
            .response
            .frames
            .iter()
            .map(|frame| frame.first_tick.0)
            .collect();
        assert_eq!(ticks, vec![6, 9], "frames 0 and 3 end before the request");
    }

    #[test]
    fn a_frame_straddling_the_start_is_served_whole() {
        // A chain cannot be folded from part of a frame: the signature covers
        // the whole thing, so half a frame is worth nothing to the requester.
        let log = log_with_frames(4);
        let served = log.serve_range(&request(4, 8), usize::MAX);
        let ticks: Vec<u64> = served
            .response
            .frames
            .iter()
            .map(|frame| frame.first_tick.0)
            .collect();
        assert_eq!(ticks, vec![3, 6], "the frame covering ticks 3..6 comes too");
    }

    #[test]
    fn a_budget_that_runs_out_says_where_to_resume() {
        // 180 ticks does not fit in a datagram. Truncating silently would leave
        // a witness believing the gap was unfillable — and an authority that
        // will not fill a gap *is* reportable, so silence here manufactures
        // accusations out of a packet-size limit.
        let log = log_with_frames(8);
        let full = log.serve_range(&request(0, 24), usize::MAX);
        let one_frame = postcard::to_stdvec(&full.response.frames[0]).unwrap().len();

        let served = log.serve_range(&request(0, 24), one_frame * 2 + SERVED_HEAD_BYTES);
        assert!(
            served.response.frames.len() < 8,
            "the budget must actually bite"
        );
        assert!(!served.response.frames.is_empty());
        let resume = served.resume_from.expect("a cut-short answer says where");
        let last = served.response.frames.last().unwrap();
        assert_eq!(
            resume.0,
            last.first_tick.0 + u64::from(last.tick_count),
            "resume at the first tick not covered"
        );
    }

    #[test]
    fn resuming_from_where_it_stopped_eventually_serves_everything() {
        // The loop a requester runs. If `resume_from` ever pointed at a tick
        // already served, this would not terminate.
        let log = log_with_frames(8);
        let one_frame =
            postcard::to_stdvec(&log.serve_range(&request(0, 24), usize::MAX).response.frames[0])
                .unwrap()
                .len();
        let budget = one_frame * 2 + SERVED_HEAD_BYTES;

        let mut collected = Vec::new();
        let mut from = 0u64;
        for _ in 0..16 {
            let served = log.serve_range(&request(from, 24), budget);
            collected.extend(served.response.frames.iter().map(|f| f.first_tick.0));
            match served.resume_from {
                Some(next) => {
                    assert!(next.0 > from, "resume must advance");
                    from = next.0;
                }
                None => break,
            }
        }
        assert_eq!(collected, (0..8).map(|i| i * 3).collect::<Vec<_>>());
    }

    #[test]
    fn a_single_frame_too_large_for_the_budget_is_unservable_not_a_stall() {
        // Answering "nothing, resume at 0" would loop the requester forever.
        // An empty answer with no resume point is the honest "cannot serve it",
        // which escalates instead of retrying.
        let log = log_with_frames(4);
        let served = log.serve_range(&request(0, 12), 1);
        assert!(served.response.frames.is_empty());
        assert_eq!(served.resume_from, None);
    }

    #[test]
    fn a_range_outside_retention_is_empty_rather_than_wrong() {
        let log = log_with_frames(4);
        let served = log.serve_range(&request(900, 960), usize::MAX);
        assert!(served.response.frames.is_empty());
        assert_eq!(served.resume_from, None);
        assert_eq!(served.response.entity, ENTITY);
    }

    #[test]
    fn frames_for_other_entities_are_not_served() {
        let mut log = AuthorityLog::default();
        let (frame, transitions) = frame(0, vec![PersistId::new(99)]);
        log.record_frame(frame, transitions);
        assert!(log
            .serve_range(&request(0, 12), usize::MAX)
            .response
            .frames
            .is_empty());
    }

    #[test]
    fn a_head_pair_is_sent_once_however_many_frames_name_the_entity() {
        // The pair is per entity per response, not per frame. Repeating it
        // would multiply the response's overhead by the frame count.
        let mut log = AuthorityLog::default();
        for index in 0..4 {
            let (frame, transitions) = frame(index * 3, vec![ENTITY, PersistId::new(2)]);
            log.record_frame(frame, transitions);
        }
        let served = log.serve_range(&request(0, 12), usize::MAX);
        assert_eq!(served.response.frames.len(), 4);
        assert_eq!(served.heads.len(), 2, "one pair per entity");
    }
}
