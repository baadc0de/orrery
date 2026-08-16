//! Authoring a real tamper-evident log for a bot (docs/06 §6).
//!
//! Every frame here is signed with the bot's own transport key, chained from the
//! previous head, and carries exactly the inputs the bot actually applied. That
//! is what makes a witness signal against one of these bots a *false positive*
//! by construction rather than by assumption: there is no gap between what was
//! logged and what was executed for a cheat to live in.
//!
//! The claim cadence is 2 Hz (every 30 ticks at 60 Hz), matching docs/06 §6.

use orrery_conformance::{Body, Command};
use orrery_core::log::{claim_hash, fold, sign_claim, sign_frame, HeadTransition};
use orrery_core::{state_hash, CoreCodec};
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick,
};

/// Ticks between state claims — 2 Hz at the 60 Hz sim rate (docs/06 §6).
pub const CLAIM_EVERY: u64 = 30;
/// Ticks a frame covers — one 20 Hz send.
pub const FRAME_TICKS: u16 = 3;

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
            frame_start: first_tick,
            frame_base: ChainHash::EMPTY,
        }
    }

    /// The anchor a watcher starts re-executing from.
    ///
    /// A witness holds state for exactly one claim — the anchor — and is held to
    /// the subject's own signature for everything after it.
    pub fn anchor(&mut self, tick: u64, state: &Body) -> StateClaim {
        let claim = self.sign_claim_at(tick, state);
        self.previous_claim = claim_hash(&claim);
        claim
    }

    /// Log the input applied at `tick`, folding it into the chain.
    ///
    /// Called every tick, including ticks whose input is a zero thrust: a silent
    /// tick still advances state and still draws from the RNG, so a log that
    /// skipped it would put every witness on a different trajectory for reasons
    /// that have nothing to do with cheating.
    pub fn log_input(&mut self, tick: u64, command: &Command) {
        let offset = (tick - self.frame_start) as u16;
        let record = InputRecord {
            tick_off: offset,
            seq: 0,
            // The frame's own signer: no 32-byte key per record.
            source: RecordSource::OwnPlayer {
                input_seq: tick as u32,
            },
            payload: bytes::Bytes::from(command.to_canonical()),
        };
        self.head = fold(self.head, &record);
        self.pending.push(record);
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

    /// Cut a claim if one is due at `tick`.
    pub fn cut_claim(&mut self, tick: u64, state: &Body) -> Option<StateClaim> {
        if !tick.is_multiple_of(CLAIM_EVERY) {
            return None;
        }
        let claim = self.sign_claim_at(tick, state);
        self.previous_claim = claim_hash(&claim);
        Some(claim)
    }

    fn sign_claim_at(&self, tick: u64, state: &Body) -> StateClaim {
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
