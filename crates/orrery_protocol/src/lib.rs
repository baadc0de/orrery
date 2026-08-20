//! Orrery wire and data types.
//!
//! Every serialized thing crosses this crate: [`CellId`], intents, leases,
//! attestations, evidence bundles, input-log records, and the protocol version
//! constant. It is engine-agnostic (glam for vector math, `iroh-base` for the
//! ed25519 identity/signature types — no Bevy, no tokio) so servers, tools, and
//! tests link it without an engine (D15, docs/10-crates.md §1).
//!
//! Normative source: [ADR index](https://github.com/baadc0de/orrery/blob/main/docs/DECISIONS.md)
//! D5 (CellId encoding), D7 (leases), D9 (input logs), D10 (evidence), D11
//! (intents), D15 (canonical scalars).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod authority;
pub mod cell;
pub mod channels;
pub mod coord;
mod gateway;
mod grid;
mod identity;
pub mod metrics;
mod persist;
mod protocol;
mod verifiable;

pub use authority::{
    ClaimBasis, ClaimId, ClaimKind, DenyReason, ExpireDisposition, ExpireReason, Lease, LeaseFlags,
    LeaseId, LeaseMsg, SeqPair,
};
pub use cell::{
    cell_id_from_metres, metres_from_cell_id, shard_of, CellId, CellRangeError,
    DEFAULT_CELL_EDGE_M, INTEREST_LEVEL, SHARD_LEVEL,
};
pub use coord::{
    verify_interest_grant, CoordMsg, CoordinatorInterestSnapshot, InterestGrantClaimsV1,
    InterestGrantV1, InterestGrantVerificationError, IslandId, IslandManifest, PeerEntry,
    TopologyRegime, COORD_ALPN, COORD_PROTOCOL_VERSION, INTEREST_GRANT_V1_DOMAIN,
    INTEREST_GRANT_V1_VERSION, MAX_INTEREST_GRANT_BYTES, MAX_INTEREST_GRANT_CELLS,
    MAX_INTEREST_GRANT_TTL_MS, MAX_PRESENCE_CELLS,
};
pub use gateway::{
    AreaPage, DiffUplink, GatewayMsg, GatewayReply, AREA_LOAD_ERR_COLD, AREA_LOAD_ERR_LIVE,
    AREA_LOAD_ERR_WRONG_OWNER, BULK_NACK_JOURNAL, BULK_NACK_REFUSED, BULK_NACK_WRONG_OWNER,
    INTEREST_ACK_BOUNDS, INTEREST_ACK_MALFORMED, INTEREST_ACK_OK, INTEREST_ACK_SUPERSEDED,
    INTEREST_ACK_UNSUPPORTED, INTEREST_ACK_UNTRUSTED, INTEREST_ACK_WRONG_PEER,
    MAX_AREA_PAGE_FRAME_BYTES,
};
pub use grid::GridId;
pub use identity::{
    FixedTokenClock, IssuerKey, IssuerKeyId, NodeId, SessionStanding, SessionTokenClaimsV1,
    SessionTokenTtlMs, SessionTokenV1, SessionTokenVerificationError, SessionTokenVerifier,
    Signature, TokenClock, UnixMillis, MAX_SESSION_TOKEN_BYTES, MAX_SESSION_TOKEN_TTL_MS,
    SESSION_TOKEN_V1_DOMAIN, SESSION_TOKEN_V1_VERSION,
};
pub use persist::{
    AccountId, AssetId, Attestation, CellEpoch, Checkpoint, EntityRekey, Epoch, Intent, IntentOp,
    IntentOutcome, ItemUid, JournalRecord, Lsn, PersistId, RecordKind, Tick, ENTITY_REKEY_VERSION,
    INTENT_PREIMAGE_TAG, REASON_BAD_SIGNATURE, REASON_CONTENTION_EXHAUSTED, REASON_EXECUTOR_ERROR,
    REASON_INSUFFICIENT_BALANCE, REASON_ISSUER_MISMATCH, REASON_ITEM_TRANSFER_TO_SELF,
    REASON_MALFORMED_OP, REASON_NOT_ITEM_OWNER, REASON_NO_EXECUTOR, REASON_NO_SUCH_ITEM,
    REASON_VALIDATION_FAILED, REPORT_ADJUDICATED, REPORT_REFUSED_NO_ADJUDICATOR,
    REPORT_REFUSED_NO_SESSION, REPORT_REFUSED_RATE_LIMITED, REPORT_REFUSED_REPORTER_MISMATCH,
};
pub use protocol::PROTOCOL_VERSION;
pub use verifiable::{
    ChainHash, DeviationKind, DiscrepancyReport, EntitySlice, EvidenceBundle, ForgeryProof,
    FrameHead, InputRecord, LogFrame, LogRangeRequest, LogRangeResponse, RecordSource, RollingHead,
    RulesetId, StateClaim, UnadjudicableReason, UniverseSeed, Verdict, WitnessMsg,
    MAX_ADJUDICATION_TICKS,
};
