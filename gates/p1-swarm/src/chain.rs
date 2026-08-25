//! Authoring a real tamper-evident log for a bot (docs/06 §6).
//!
//! Every frame here is signed with the bot's own transport key, chained from the
//! previous head, and carries exactly the inputs the bot actually applied. That
//! is what makes a witness signal against one of these bots a *false positive*
//! by construction rather than by assumption: there is no gap between what was
//! logged and what was executed for a cheat to live in.
//!
//! The claim cadence is 2 Hz (every 30 ticks at 60 Hz), matching docs/06 §6.
//! The *frame* cadence is not a constant here: it is derived from the share of
//! the upload budget the witness lane may spend, because that is the thing it
//! was actually failing (docs/03-replication.md §5.3a).

use orrery_core::log::{claim_hash, fold, sign_claim, sign_frame, HeadTransition};
use orrery_core::{state_hash, CoreCodec};
use orrery_games::regolith::order::Order;
use orrery_games::regolith::state::RegolithState;
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick,
};

/// Ticks between state claims — 2 Hz at the 60 Hz sim rate (docs/06 §6).
pub const CLAIM_EVERY: u64 = 30;

/// Ticks a frame covers — **derived from the lane budget, not from the send
/// rate** (docs/03-replication.md §5.3a).
///
/// This used to be 3: one frame per 20 Hz send, so the chain a witness follows
/// lined up with the datagrams it already received. That alignment bought
/// nothing a witness uses and cost ~250 bytes of per-frame fixed overhead every
/// 50 ms, which at 32 peers put the lane at 384 kb/s against §5.3's 0.15–0.2
/// Mbps and took the peer over its 1 Mbps ceiling. See the module docs of
/// `gates/p1-swarm` for the measurement, and `orrery_witness::plugin` for the
/// arithmetic — at the D16 defaults it lands on **10 ticks, 6 Hz**.
pub const FRAME_TICKS: u16 = orrery_witness::plugin::frame_interval_ticks(
    1_000_000,
    orrery_witness::plugin::MAX_WITNESS_LINKS,
    TICK_HZ,
    CLAIM_EVERY,
);

/// The sim rate the cadence is derived against (D16).
const TICK_HZ: u64 = 60;

/// One bot's authored chain.
pub struct Chain {
    key: iroh_base::SecretKey,
    entity: PersistId,
    ruleset: RulesetId,
    head: ChainHash,
    previous_claim: [u8; 32],
    /// Records accumulated since the last frame was cut.
    pending: Vec<InputRecord>,
    /// State hashes the pending frame's ticks produced, in tick order.
    ///
    /// Retained alongside the frame so this bot can assemble a bundle to answer
    /// for *itself* — a log with frames but no per-tick hashes can serve a
    /// repair and still fail `IncompleteHashes` on every self-authored window.
    pending_hashes: Vec<[u8; 32]>,
    /// The newest tick a claim has already been cut for, so a tick cannot be
    /// claimed twice.
    ///
    /// **This is load-bearing for adjudication, not tidiness.** The swarm takes
    /// each bot's anchor at tick 0 and then runs `publish_claim(0)` on the very
    /// first tick, so tick 0 used to be signed *twice* — the same entity, head
    /// and state hash, but a different `prev_claim`, and therefore a different
    /// `claim_hash`. A witness retains the anchor and the duplicate both;
    /// `AuthorityLog::assemble_bundle` picks the first claim it holds at
    /// `window_start` (the anchor) while the claim at tick 30 chains from the
    /// *duplicate*. `verify_bundle` walks `disputed_claims` checking
    /// `claim.prev_claim == claim_hash(previous)` and would find the break —
    /// returning `Confirms { kind: DiscreteMismatch }` against an authority that
    /// had done nothing wrong. Nothing caught it because shadow mode meant no
    /// gates/p1-swarm bundle had ever been adjudicated.
    claimed_through: Option<u64>,
    /// Tick the pending frame starts at.
    frame_start: u64,
    /// The chain head as it stood before the pending records — the `prev_head`
    /// the next frame's signature commits to.
    frame_base: ChainHash,
}

/// A frame and the transitions its signature commits to.
pub struct AuthoredFrame {
    /// The signed frame.
    pub frame: LogFrame,
    /// Full head pairs the signature covers.
    pub transitions: Vec<HeadTransition>,
    /// State hash produced by each tick the frame covers, in tick order.
    pub tick_hashes: Vec<[u8; 32]>,
}

impl Chain {
    /// Start a chain for `entity`, signed by `key`.
    #[must_use]
    pub fn new(
        key: iroh_base::SecretKey,
        entity: PersistId,
        ruleset: RulesetId,
        first_tick: u64,
    ) -> Self {
        Self {
            key,
            entity,
            ruleset,
            head: ChainHash::EMPTY,
            previous_claim: [0; 32],
            pending: Vec::new(),
            pending_hashes: Vec::new(),
            claimed_through: None,
            frame_start: first_tick,
            frame_base: ChainHash::EMPTY,
        }
    }

    /// The anchor a watcher starts re-executing from.
    ///
    /// A witness holds state for exactly one claim — the anchor — and is held to
    /// the subject's own signature for everything after it.
    pub fn anchor(&mut self, tick: u64, state: &RegolithState) -> StateClaim {
        let claim = self.sign_claim_at(tick, state);
        self.previous_claim = claim_hash(&claim);
        self.claimed_through = Some(tick);
        claim
    }

    /// Log the inputs applied at `tick`, folding them into the chain in order.
    ///
    /// Called every tick, including ticks whose thrust is zero: the held
    /// trigger and scenario action still belong to that tick, and a log that
    /// skipped any order would put every witness on a different trajectory for
    /// reasons that have nothing to do with cheating.
    pub fn log_inputs(&mut self, tick: u64, commands: &[Order]) {
        let offset = (tick - self.frame_start) as u16;
        for (seq, command) in commands.iter().enumerate() {
            let record = InputRecord {
                tick_off: offset,
                seq: u16::try_from(seq).unwrap_or(u16::MAX),
                // The frame's own signer: no 32-byte key per record.
                source: RecordSource::OwnPlayer {
                    input_seq: (tick as u32).wrapping_mul(4).wrapping_add(seq as u32),
                },
                payload: bytes::Bytes::from(command.to_canonical()),
            };
            self.head = fold(self.head, &record);
            self.pending.push(record);
        }
    }

    /// Retain the state hash the tick just executed produced.
    ///
    /// Called right after the step, so the hash lands in the same frame as the
    /// input that produced it.
    pub fn log_tick_hash(&mut self, hash: [u8; 32]) {
        self.pending_hashes.push(hash);
    }

    /// Cut a frame if one is due at `tick`, returning it for publication.
    ///
    /// Frames are cut on the send cadence, so the chain a witness follows lines
    /// up with the datagrams it already receives.
    pub fn cut_frame(&mut self, tick: u64) -> Option<AuthoredFrame> {
        if self.pending.is_empty() || (tick + 1 - self.frame_start) < u64::from(FRAME_TICKS) {
            return None;
        }
        let before = self.frame_base;
        let transitions = vec![HeadTransition {
            entity: self.entity,
            prev_head: before,
            head: self.head,
        }];
        let frame = LogFrame {
            ruleset: self.ruleset,
            first_tick: Tick::new(self.frame_start),
            tick_count: FRAME_TICKS,
            entities: vec![EntitySlice {
                entity: self.entity,
                chain_epoch: 0,
                prev_head: before.rolling(),
                records: core::mem::take(&mut self.pending),
                head: self.head.rolling(),
            }],
            sig: sign_frame(
                &self.key,
                self.ruleset,
                Tick::new(self.frame_start),
                FRAME_TICKS,
                &transitions,
            ),
        };
        self.frame_start = tick + 1;
        self.frame_base = self.head;
        Some(AuthoredFrame {
            frame,
            transitions,
            tick_hashes: core::mem::take(&mut self.pending_hashes),
        })
    }

    /// Cut a claim if one is due at `tick`, and if this tick has not been
    /// claimed already — see the `claimed_through` field.
    pub fn cut_claim(&mut self, tick: u64, state: &RegolithState) -> Option<StateClaim> {
        if !tick.is_multiple_of(CLAIM_EVERY) || self.claimed_through == Some(tick) {
            return None;
        }
        let claim = self.sign_claim_at(tick, state);
        self.previous_claim = claim_hash(&claim);
        self.claimed_through = Some(tick);
        Some(claim)
    }

    fn sign_claim_at(&self, tick: u64, state: &RegolithState) -> StateClaim {
        let mut claim = StateClaim {
            entity: self.entity,
            chain_epoch: 0,
            tick: Tick::new(tick),
            input_head: self.head,
            state_hash: state_hash(state),
            prev_claim: self.previous_claim,
            ruleset: self.ruleset,
            sig: self.key.sign(b"unsigned"),
        };
        sign_claim(&self.key, &mut claim);
        claim
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::QPos;
    use orrery_games::regolith::archetype::Archetype;
    use orrery_games::regolith::state::Craft;

    fn craft() -> RegolithState {
        RegolithState::Craft(Craft::spawned(
            Archetype::Cruiser,
            QPos::from_metres(1_000.0, 0.0, 0.0),
            0,
        ))
    }

    #[test]
    fn a_tick_is_only_ever_claimed_once_and_the_next_claim_chains_to_that_one() {
        // The swarm anchors every watch at tick 0 and then runs
        // `publish_claim(0)` on the first tick, so tick 0 used to be signed
        // twice: same entity, head and state hash, different `prev_claim`, and
        // therefore a different `claim_hash`.
        //
        // A witness retains both. `AuthorityLog::assemble_bundle` takes the
        // *first* claim it holds at `window_start` — the anchor — while the
        // claim at tick 30 chains from the duplicate, so `verify_bundle`'s
        // `claim.prev_claim == claim_hash(previous)` walk finds a break and
        // returns `Confirms { kind: DiscreteMismatch }` against an authority
        // that had done nothing wrong. Shadow mode meant no gates/p1-swarm bundle had
        // ever been adjudicated, so nothing in the tree could see it.
        let key = iroh_base::SecretKey::from_bytes(&[9u8; 32]);
        let mut chain = Chain::new(
            key,
            PersistId::new(1),
            orrery_games::regolith::REGOLITH_RULESET,
            0,
        );
        let state = craft();

        let anchor = chain.anchor(0, &state);
        assert!(
            chain.cut_claim(0, &state).is_none(),
            "tick 0 was already claimed by the anchor",
        );

        for tick in 1..CLAIM_EVERY {
            chain.log_inputs(tick, &[orrery_games::regolith::order::Order::Fire]);
        }
        let next = chain
            .cut_claim(CLAIM_EVERY, &state)
            .expect("a claim is due");
        assert_eq!(
            next.prev_claim,
            claim_hash(&anchor),
            "the next claim must chain to the anchor an adjudicator will start from",
        );
    }
}
