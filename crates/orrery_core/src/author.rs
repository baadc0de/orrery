//! Shared authoring for a signed, hash-chained input log.
//!
//! Authorities and rendered clients must use one producer: an anchor, the
//! frames that follow it, and periodic claims are one protocol, not three
//! independently reconstructed byte streams.

use crate::log::{claim_hash, fold, neighbor_record, sign_claim, sign_frame, HeadTransition};
use crate::{state_hash, CoreCodec, NeighborFrame};
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, PersistId, RecordSource, RulesetId, StateClaim,
    Tick,
};

/// A frame cut by [`InputLogProducer`], plus the full data its publisher retains.
pub struct AuthoredFrame {
    /// The signed wire frame.
    pub frame: LogFrame,
    /// Full head pairs covered by the frame signature.
    pub transitions: Vec<HeadTransition>,
    /// State hash produced by each tick in the frame, in tick order.
    pub tick_hashes: Vec<[u8; 32]>,
}

/// One authority's signed input-log producer.
///
/// Inputs are folded in the exact order supplied. The caller owns simulation
/// ordering: cut a claim from pre-step state, log the inputs about to be
/// applied, execute them, retain the resulting tick hash, then cut a frame.
pub struct InputLogProducer {
    key: iroh_base::SecretKey,
    entity: PersistId,
    ruleset: RulesetId,
    head: ChainHash,
    previous_claim: [u8; 32],
    pending: Vec<InputRecord>,
    pending_hashes: Vec<[u8; 32]>,
    claimed_through: Option<u64>,
    frame_start: u64,
    frame_base: ChainHash,
    claim_every: u64,
    frame_ticks: u16,
}

impl InputLogProducer {
    /// Start a chain for `entity`, signed by `key`.
    ///
    /// `claim_every` and `frame_ticks` must both be non-zero, and the claim
    /// interval must be divisible by the frame interval so claims land on
    /// complete frame boundaries.
    #[must_use]
    pub fn new(
        key: iroh_base::SecretKey,
        entity: PersistId,
        ruleset: RulesetId,
        first_tick: u64,
        claim_every: u64,
        frame_ticks: u16,
    ) -> Self {
        assert!(claim_every > 0, "claim cadence must be non-zero");
        assert!(frame_ticks > 0, "frame cadence must be non-zero");
        assert!(
            claim_every.is_multiple_of(u64::from(frame_ticks)),
            "claim cadence must be divisible by frame cadence"
        );
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
            claim_every,
            frame_ticks,
        }
    }

    /// Sign the tick-zero (or handoff) claim a watcher starts from.
    pub fn anchor<S: CoreCodec>(&mut self, tick: u64, state: &S) -> StateClaim {
        let claim = self.sign_claim_at(tick, state);
        self.previous_claim = claim_hash(&claim);
        self.claimed_through = Some(tick);
        claim
    }

    /// Log exactly the inputs that will be applied at `tick`.
    pub fn log_inputs<I: CoreCodec>(&mut self, tick: u64, inputs: &[I]) {
        let sources: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(seq, _)| RecordSource::OwnPlayer {
                input_seq: (tick as u32).wrapping_mul(4).wrapping_add(seq as u32),
            })
            .collect();
        self.log_inputs_with_sources(tick, inputs, &sources);
    }

    /// Log exactly the inputs that will be applied at `tick`, with explicit
    /// provenance in the same total order.
    ///
    /// # Panics
    /// Panics when `inputs` and `sources` differ in length; an unclassified
    /// input or a source with no executed input would make replay ambiguous.
    pub fn log_inputs_with_sources<I: CoreCodec>(
        &mut self,
        tick: u64,
        inputs: &[I],
        sources: &[RecordSource],
    ) {
        assert_eq!(
            inputs.len(),
            sources.len(),
            "every logged input has exactly one source"
        );
        let offset = u16::try_from(tick.saturating_sub(self.frame_start))
            .expect("a frame is cut before its tick offset exceeds u16");
        for (seq, (input, source)) in inputs.iter().zip(sources).enumerate() {
            let record = InputRecord {
                tick_off: offset,
                seq: u16::try_from(seq).unwrap_or(u16::MAX),
                source: source.clone(),
                payload: bytes::Bytes::from(input.to_canonical()),
            };
            self.head = fold(self.head, &record);
            self.pending.push(record);
        }
    }

    /// Append the replay inputs produced by recorded neighbour reads at `tick`.
    ///
    /// A rule discovers these only while it executes, so they follow the sealed
    /// command records for the same tick. Replay separates the record classes,
    /// installs these snapshots before stepping, and verifies the performed read
    /// sequence against them.
    pub fn log_neighbor_frames(&mut self, tick: u64, frames: &[NeighborFrame]) {
        let offset = u16::try_from(tick.saturating_sub(self.frame_start))
            .expect("a frame is cut before its tick offset exceeds u16");
        let first_seq = self
            .pending
            .iter()
            .filter(|record| record.tick_off == offset)
            .count();
        for (index, frame) in frames.iter().enumerate() {
            let seq = u16::try_from(first_seq.saturating_add(index)).unwrap_or(u16::MAX);
            let record = neighbor_record(offset, seq, frame);
            self.head = fold(self.head, &record);
            self.pending.push(record);
        }
    }

    /// Retain the state hash produced immediately after one logged tick.
    pub fn log_tick_hash(&mut self, hash: [u8; 32]) {
        self.pending_hashes.push(hash);
    }

    /// Cut and sign a frame when the configured interval ends.
    pub fn cut_frame(&mut self, tick: u64) -> Option<AuthoredFrame> {
        if self.pending.is_empty() || (tick + 1 - self.frame_start) < u64::from(self.frame_ticks) {
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
            tick_count: self.frame_ticks,
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
                self.frame_ticks,
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

    /// Cut a periodic pre-step claim, unless this tick was already anchored.
    pub fn cut_claim<S: CoreCodec>(&mut self, tick: u64, state: &S) -> Option<StateClaim> {
        if !tick.is_multiple_of(self.claim_every) || self.claimed_through == Some(tick) {
            return None;
        }
        let claim = self.sign_claim_at(tick, state);
        self.previous_claim = claim_hash(&claim);
        self.claimed_through = Some(tick);
        Some(claim)
    }

    fn sign_claim_at<S: CoreCodec>(&self, tick: u64, state: &S) -> StateClaim {
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

    #[derive(Clone)]
    struct TestState(u8);

    impl CoreCodec for TestState {
        fn encode(&self, out: &mut Vec<u8>) {
            out.push(self.0);
        }

        fn decode(bytes: &[u8]) -> Result<Self, crate::CodecError> {
            bytes
                .first()
                .copied()
                .map(Self)
                .ok_or(crate::CodecError("empty test state"))
        }
    }

    #[test]
    fn an_anchor_is_not_duplicated_and_the_next_claim_chains_from_it() {
        const CLAIM_EVERY: u64 = 30;
        let key = iroh_base::SecretKey::from_bytes(&[9; 32]);
        let mut producer = InputLogProducer::new(
            key,
            PersistId::new(1),
            RulesetId {
                version: 1,
                digest: [7; 32],
            },
            0,
            CLAIM_EVERY,
            10,
        );
        let state = TestState(1);
        let anchor = producer.anchor(0, &state);
        assert!(producer.cut_claim(0, &state).is_none());
        let next = producer
            .cut_claim(CLAIM_EVERY, &state)
            .expect("claim cadence reached");
        assert_eq!(next.prev_claim, claim_hash(&anchor));
    }

    #[test]
    fn explicit_input_sources_survive_into_the_signed_frame_in_order() {
        let key = iroh_base::SecretKey::from_bytes(&[9; 32]);
        let mut producer = InputLogProducer::new(
            key,
            PersistId::new(1),
            RulesetId {
                version: 1,
                digest: [7; 32],
            },
            0,
            10,
            1,
        );
        let inputs = [TestState(3), TestState(5)];
        let sources = [
            RecordSource::InboundEvent {
                from: PersistId::new(2),
            },
            RecordSource::OwnPlayer { input_seq: 7 },
        ];
        producer.log_inputs_with_sources(0, &inputs, &sources);
        producer.log_tick_hash([0; 32]);
        let authored = producer.cut_frame(0).expect("one-tick frame closes");
        let records = &authored.frame.entities[0].records;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].source, sources[0]);
        assert_eq!(records[0].seq, 0);
        assert_eq!(records[1].source, sources[1]);
        assert_eq!(records[1].seq, 1);
    }

    #[test]
    fn executor_neighbor_frames_follow_commands_in_the_signed_tick() {
        let key = iroh_base::SecretKey::from_bytes(&[9; 32]);
        let mut producer = InputLogProducer::new(
            key,
            PersistId::new(1),
            RulesetId {
                version: 1,
                digest: [7; 32],
            },
            0,
            10,
            1,
        );
        producer.log_inputs(0, &[TestState(3)]);
        producer.log_neighbor_frames(
            0,
            &[NeighborFrame {
                neighbor: PersistId::new(2),
                observed_tick: Tick::new(0),
                state: Some(vec![8]),
            }],
        );
        producer.log_tick_hash([0; 32]);

        let authored = producer.cut_frame(0).expect("one-tick frame closes");
        let records = &authored.frame.entities[0].records;
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0].source, RecordSource::OwnPlayer { .. }));
        assert!(matches!(
            records[1].source,
            RecordSource::NeighborFrame {
                neighbor,
                present: true,
                observed_tick,
            } if neighbor == PersistId::new(2) && observed_tick == Tick::new(0)
        ));
        assert_eq!(records[1].payload.as_ref(), &[8]);
        assert_eq!(records[1].seq, 1);
    }
}
