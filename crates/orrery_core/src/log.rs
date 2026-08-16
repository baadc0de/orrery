//! The tamper-evident input log (docs/06 §6).
//!
//! Each core entity gets one hash chain per authority epoch:
//! `h_i = blake3(h_{i-1} ‖ encode(record_i))`. Records are never individually
//! signed — one signature per *frame* covers every entity's
//! `(prev_head, head)` transition, so a sender's signing cost is flat in the
//! number of entities it authors rather than linear in records.
//!
//! Verification therefore has an order to it: **fold first, then check the
//! signature.** The wire carries truncated 8-byte heads for gap detection, and
//! the signature preimage carries the full 32-byte ones — so a verifier that
//! trusted the wire heads would be checking a signature over numbers it never
//! derived. Folding is what turns eight bytes of hint into thirty-two bytes of
//! evidence.

use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, NodeId, PersistId, RollingHead, RulesetId,
    Signature, StateClaim, Tick,
};

/// Domain separator for frame signatures, so a frame preimage can never be
/// mistaken for a claim preimage under the same key.
const FRAME_DOMAIN: &[u8] = b"orrery/log-frame/v1";
/// Domain separator for claim signatures.
const CLAIM_DOMAIN: &[u8] = b"orrery/state-claim/v1";

/// Fold one record into a chain.
///
/// The record is hashed in its canonical postcard encoding, so two peers that
/// encode the same record identically fold to the same head.
#[must_use]
pub fn fold(previous: ChainHash, record: &InputRecord) -> ChainHash {
    let encoded = postcard::to_stdvec(record).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&previous.0);
    hasher.update(&encoded);
    ChainHash(*hasher.finalize().as_bytes())
}

/// Fold a whole run of records.
#[must_use]
pub fn fold_all(previous: ChainHash, records: &[InputRecord]) -> ChainHash {
    records.iter().fold(previous, fold)
}

/// One entity's live chain, as the authority maintains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityChain {
    /// The entity this chain covers.
    pub entity: PersistId,
    /// Increments on authority handoff; adjudication windows never span one.
    pub chain_epoch: u32,
    /// The current head.
    pub head: ChainHash,
}

impl EntityChain {
    /// Start a chain at the empty head.
    #[must_use]
    pub fn new(entity: PersistId, chain_epoch: u32) -> Self {
        Self {
            entity,
            chain_epoch,
            head: ChainHash::EMPTY,
        }
    }

    /// Append one record, advancing the head.
    pub fn append(&mut self, record: &InputRecord) -> ChainHash {
        self.head = fold(self.head, record);
        self.head
    }
}

/// The full `(prev_head, head)` pair a frame's signature commits to for one
/// entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadTransition {
    /// The entity.
    pub entity: PersistId,
    /// Head before this frame's records.
    pub prev_head: ChainHash,
    /// Head after them.
    pub head: ChainHash,
}

/// Build the frame signature preimage.
///
/// `ruleset ‖ tick range ‖ every entity's full (prev_head, head)` — the full
/// heads, which is why a verifier has to fold before it can check anything.
#[must_use]
pub fn frame_preimage(
    ruleset: RulesetId,
    first_tick: Tick,
    tick_count: u16,
    transitions: &[HeadTransition],
) -> Vec<u8> {
    let mut preimage = Vec::with_capacity(FRAME_DOMAIN.len() + 48 + transitions.len() * 72);
    preimage.extend_from_slice(FRAME_DOMAIN);
    preimage.extend_from_slice(&ruleset.version.to_le_bytes());
    preimage.extend_from_slice(&ruleset.digest);
    preimage.extend_from_slice(&first_tick.0.to_le_bytes());
    preimage.extend_from_slice(&tick_count.to_le_bytes());
    for transition in transitions {
        preimage.extend_from_slice(&transition.entity.0.to_le_bytes());
        preimage.extend_from_slice(&transition.prev_head.0);
        preimage.extend_from_slice(&transition.head.0);
    }
    preimage
}

/// Sign a frame with the authority's transport key (D3).
#[must_use]
pub fn sign_frame(
    key: &iroh_base::SecretKey,
    ruleset: RulesetId,
    first_tick: Tick,
    tick_count: u16,
    transitions: &[HeadTransition],
) -> Signature {
    key.sign(&frame_preimage(
        ruleset,
        first_tick,
        tick_count,
        transitions,
    ))
}

/// Why a frame or claim failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogError {
    /// A slice's recomputed head does not match the rolling head it carries —
    /// a gap, a reorder, or a tampered record.
    HeadMismatch {
        /// The entity whose chain broke.
        entity: PersistId,
    },
    /// A record names a tick outside the frame's declared range.
    TickOutOfRange {
        /// The entity carrying the offending record.
        entity: PersistId,
    },
    /// Per-tick sequence numbers are not strictly increasing (VC-2 legality).
    InputOrderIllegal {
        /// The entity carrying the offending record.
        entity: PersistId,
    },
    /// The signature does not verify under the authority's key.
    SignatureInvalid,
    /// A frame declares entities whose sibling heads were not supplied, so the
    /// preimage cannot be rebuilt.
    MissingSiblingHeads,
}

impl core::fmt::Display for LogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::HeadMismatch { entity } => write!(f, "chain head mismatch for {entity:?}"),
            Self::TickOutOfRange { entity } => write!(f, "record tick out of range for {entity:?}"),
            Self::InputOrderIllegal { entity } => write!(f, "illegal input order for {entity:?}"),
            Self::SignatureInvalid => f.write_str("frame signature does not verify"),
            Self::MissingSiblingHeads => f.write_str("sibling heads missing for frame preimage"),
        }
    }
}

impl core::error::Error for LogError {}

/// Check one slice's records: tick range, input-order legality, and that
/// folding `prev_head` through them reaches `head`.
///
/// `prev_head` is the caller's **full** head — from the previous frame's fold
/// or a `StateClaim` — because the slice only carries a truncated one.
pub fn verify_slice(
    slice: &EntitySlice,
    frame_first_tick: Tick,
    frame_tick_count: u16,
    prev_head: ChainHash,
) -> Result<ChainHash, LogError> {
    if prev_head.rolling() != slice.prev_head {
        return Err(LogError::HeadMismatch {
            entity: slice.entity,
        });
    }

    let mut last: Option<(u16, u16)> = None;
    for record in &slice.records {
        if record.tick_off >= frame_tick_count {
            return Err(LogError::TickOutOfRange {
                entity: slice.entity,
            });
        }
        // Legality, not re-sorting (VC-2): ticks never go backwards, and within
        // a tick the authority's sequence numbers strictly increase. A replay
        // that reordered would manufacture deviations out of a legal log.
        if let Some((last_tick, last_seq)) = last {
            let ordered = record.tick_off > last_tick
                || (record.tick_off == last_tick && record.seq > last_seq);
            if !ordered {
                return Err(LogError::InputOrderIllegal {
                    entity: slice.entity,
                });
            }
        }
        last = Some((record.tick_off, record.seq));
    }
    let _ = frame_first_tick;

    let head = fold_all(prev_head, &slice.records);
    if head.rolling() != slice.head {
        return Err(LogError::HeadMismatch {
            entity: slice.entity,
        });
    }
    Ok(head)
}

/// Verify a whole frame: fold every slice, rebuild the preimage, check the
/// signature.
///
/// `prev_heads` supplies the full incoming head for each entity in the frame,
/// in slice order. For the entity under dispute that comes from the previous
/// frame's fold; for its siblings it comes from the bundle, which is why an
/// [`EvidenceBundle`](orrery_protocol::EvidenceBundle) carries them at all.
pub fn verify_frame(
    frame: &LogFrame,
    authority: NodeId,
    prev_heads: &[ChainHash],
) -> Result<Vec<HeadTransition>, LogError> {
    if prev_heads.len() != frame.entities.len() {
        return Err(LogError::MissingSiblingHeads);
    }
    let mut transitions = Vec::with_capacity(frame.entities.len());
    for (slice, prev) in frame.entities.iter().zip(prev_heads) {
        let head = verify_slice(slice, frame.first_tick, frame.tick_count, *prev)?;
        transitions.push(HeadTransition {
            entity: slice.entity,
            prev_head: *prev,
            head,
        });
    }
    let preimage = frame_preimage(
        frame.ruleset,
        frame.first_tick,
        frame.tick_count,
        &transitions,
    );
    authority
        .verify(&preimage, &frame.sig)
        .map_err(|_| LogError::SignatureInvalid)?;
    Ok(transitions)
}

/// Build the claim signature preimage.
#[must_use]
pub fn claim_preimage(claim: &StateClaim) -> Vec<u8> {
    let mut preimage = Vec::with_capacity(CLAIM_DOMAIN.len() + 128);
    preimage.extend_from_slice(CLAIM_DOMAIN);
    preimage.extend_from_slice(&claim.entity.0.to_le_bytes());
    preimage.extend_from_slice(&claim.chain_epoch.to_le_bytes());
    preimage.extend_from_slice(&claim.tick.0.to_le_bytes());
    preimage.extend_from_slice(&claim.input_head.0);
    preimage.extend_from_slice(&claim.state_hash);
    preimage.extend_from_slice(&claim.prev_claim);
    preimage.extend_from_slice(&claim.ruleset.version.to_le_bytes());
    preimage.extend_from_slice(&claim.ruleset.digest);
    preimage
}

/// Sign a claim, filling in its signature.
pub fn sign_claim(key: &iroh_base::SecretKey, claim: &mut StateClaim) {
    claim.sig = key.sign(&claim_preimage(claim));
}

/// Verify a claim's signature under the authority's key.
///
/// # Errors
///
/// [`LogError::SignatureInvalid`] when it does not verify.
pub fn verify_claim(claim: &StateClaim, authority: NodeId) -> Result<(), LogError> {
    authority
        .verify(&claim_preimage(claim), &claim.sig)
        .map_err(|_| LogError::SignatureInvalid)
}

/// The hash a following claim puts in its `prev_claim`, chaining claims as
/// well as inputs.
#[must_use]
pub fn claim_hash(claim: &StateClaim) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&claim_preimage(claim));
    hasher.update(&claim.sig.to_bytes());
    *hasher.finalize().as_bytes()
}

/// A rolling head as it appears on the wire, for gap detection only.
#[must_use]
pub fn rolling(head: ChainHash) -> RollingHead {
    head.rolling()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::RecordSource;

    fn key(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    fn ruleset() -> RulesetId {
        RulesetId {
            version: 1,
            digest: [7; 32],
        }
    }

    fn record(tick_off: u16, seq: u16, payload: &'static [u8]) -> InputRecord {
        InputRecord {
            tick_off,
            seq,
            source: RecordSource::InboundEvent {
                from: PersistId::new(3),
            },
            payload: bytes::Bytes::from_static(payload),
        }
    }

    fn slice(entity: PersistId, prev: ChainHash, records: Vec<InputRecord>) -> EntitySlice {
        let head = fold_all(prev, &records);
        EntitySlice {
            entity,
            chain_epoch: 0,
            prev_head: prev.rolling(),
            records,
            head: head.rolling(),
        }
    }

    fn frame(
        authority: &iroh_base::SecretKey,
        slices: Vec<EntitySlice>,
        prevs: &[ChainHash],
    ) -> LogFrame {
        let transitions: Vec<_> = slices
            .iter()
            .zip(prevs)
            .map(|(s, prev)| HeadTransition {
                entity: s.entity,
                prev_head: *prev,
                head: fold_all(*prev, &s.records),
            })
            .collect();
        LogFrame {
            ruleset: ruleset(),
            first_tick: Tick::new(100),
            tick_count: 3,
            entities: slices,
            sig: sign_frame(authority, ruleset(), Tick::new(100), 3, &transitions),
        }
    }

    #[test]
    fn a_frame_verifies_only_after_folding_to_the_signed_heads() {
        let authority = key(1);
        let prev = ChainHash::EMPTY;
        let slices = vec![slice(
            PersistId::new(1),
            prev,
            vec![record(0, 0, b"a"), record(1, 0, b"b")],
        )];
        let frame = frame(&authority, slices, &[prev]);
        assert!(verify_frame(&frame, authority.public(), &[prev]).is_ok());
    }

    #[test]
    fn tampering_with_a_record_breaks_the_chain_before_the_signature_is_reached() {
        // The point of the chain: a record cannot be edited, added or removed
        // without the fold landing somewhere else.
        let authority = key(1);
        let prev = ChainHash::EMPTY;
        let mut frame = frame(
            &authority,
            vec![slice(
                PersistId::new(1),
                prev,
                vec![record(0, 0, b"a"), record(1, 0, b"b")],
            )],
            &[prev],
        );
        frame.entities[0].records[1].payload = bytes::Bytes::from_static(b"tampered");
        assert_eq!(
            verify_frame(&frame, authority.public(), &[prev]),
            Err(LogError::HeadMismatch {
                entity: PersistId::new(1)
            })
        );
    }

    #[test]
    fn a_frame_signed_by_someone_else_is_rejected() {
        let authority = key(1);
        let impostor = key(2);
        let prev = ChainHash::EMPTY;
        let frame = frame(
            &impostor,
            vec![slice(PersistId::new(1), prev, vec![record(0, 0, b"a")])],
            &[prev],
        );
        assert_eq!(
            verify_frame(&frame, authority.public(), &[prev]),
            Err(LogError::SignatureInvalid)
        );
    }

    #[test]
    fn a_sibling_entitys_heads_are_part_of_the_signature() {
        // One signature covers every authored entity. If a verifier could drop
        // a sibling slice and still verify, an authority could disown entities
        // after the fact.
        let authority = key(1);
        let prev = ChainHash::EMPTY;
        let slices = vec![
            slice(PersistId::new(1), prev, vec![record(0, 0, b"a")]),
            slice(PersistId::new(2), prev, vec![record(0, 0, b"b")]),
        ];
        let mut frame = frame(&authority, slices, &[prev, prev]);
        frame.entities.pop();
        assert_eq!(
            verify_frame(&frame, authority.public(), &[prev]),
            Err(LogError::SignatureInvalid)
        );
    }

    #[test]
    fn a_gap_is_detected_as_a_head_mismatch() {
        // The receiver's previous head disagrees with the slice's declared
        // prev_head: exactly the case that triggers a LogRangeRequest.
        let authority = key(1);
        let prev = ChainHash::EMPTY;
        let frame = frame(
            &authority,
            vec![slice(PersistId::new(1), prev, vec![record(0, 0, b"a")])],
            &[prev],
        );
        let wrong_prev = ChainHash([9; 32]);
        assert_eq!(
            verify_frame(&frame, authority.public(), &[wrong_prev]),
            Err(LogError::HeadMismatch {
                entity: PersistId::new(1)
            })
        );
    }

    #[test]
    fn out_of_order_or_out_of_range_records_are_illegal() {
        let prev = ChainHash::EMPTY;
        let backwards = slice(
            PersistId::new(1),
            prev,
            vec![record(2, 0, b"a"), record(1, 0, b"b")],
        );
        assert_eq!(
            verify_slice(&backwards, Tick::new(100), 3, prev),
            Err(LogError::InputOrderIllegal {
                entity: PersistId::new(1)
            })
        );

        let duplicate_seq = slice(
            PersistId::new(1),
            prev,
            vec![record(0, 1, b"a"), record(0, 1, b"b")],
        );
        assert_eq!(
            verify_slice(&duplicate_seq, Tick::new(100), 3, prev),
            Err(LogError::InputOrderIllegal {
                entity: PersistId::new(1)
            })
        );

        let past_end = slice(PersistId::new(1), prev, vec![record(3, 0, b"a")]);
        assert_eq!(
            verify_slice(&past_end, Tick::new(100), 3, prev),
            Err(LogError::TickOutOfRange {
                entity: PersistId::new(1)
            })
        );
    }

    #[test]
    fn claims_verify_and_chain() {
        let authority = key(1);
        let mut first = StateClaim {
            entity: PersistId::new(1),
            chain_epoch: 0,
            tick: Tick::new(30),
            input_head: ChainHash([1; 32]),
            state_hash: [2; 32],
            prev_claim: [0; 32],
            ruleset: ruleset(),
            sig: authority.sign(b"placeholder"),
        };
        sign_claim(&authority, &mut first);
        assert!(verify_claim(&first, authority.public()).is_ok());
        assert_eq!(
            verify_claim(&first, key(2).public()),
            Err(LogError::SignatureInvalid)
        );

        let mut second = StateClaim {
            tick: Tick::new(60),
            prev_claim: claim_hash(&first),
            ..first.clone()
        };
        sign_claim(&authority, &mut second);
        assert!(verify_claim(&second, authority.public()).is_ok());
        assert_ne!(second.prev_claim, [0; 32]);
    }

    #[test]
    fn a_frame_preimage_cannot_be_replayed_as_a_claim() {
        // Domain separation: the same key signs both, so without distinct
        // prefixes a frame signature could be presented as a claim signature.
        let ruleset = ruleset();
        let preimage = frame_preimage(ruleset, Tick::new(0), 1, &[]);
        assert!(preimage.starts_with(FRAME_DOMAIN));
        assert!(!preimage.starts_with(CLAIM_DOMAIN));
    }
}
