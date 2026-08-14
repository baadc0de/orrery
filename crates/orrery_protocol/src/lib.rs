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

pub mod cell;
pub mod channels;
pub mod coord;
mod gateway;
mod grid;
mod identity;
mod persist;
mod protocol;

pub use cell::{
    cell_id_from_metres, metres_from_cell_id, shard_of, CellId, CellRangeError,
    DEFAULT_CELL_EDGE_M, INTEREST_LEVEL, SHARD_LEVEL,
};
pub use coord::{CoordMsg, IslandId, IslandManifest, PeerEntry, TopologyRegime};
pub use gateway::{
    AreaPage, DiffUplink, GatewayMsg, GatewayReply, AREA_LOAD_ERR_COLD, AREA_LOAD_ERR_LIVE,
    MAX_AREA_PAGE_FRAME_BYTES,
};
pub use grid::GridId;
pub use identity::{NodeId, Signature};
pub use persist::{
    AccountId, AssetId, Attestation, Checkpoint, Epoch, Intent, IntentOp, IntentOutcome, ItemUid,
    JournalRecord, Lsn, PersistId, RecordKind, Tick, INTENT_PREIMAGE_TAG, REASON_BAD_SIGNATURE,
    REASON_CONTENTION_EXHAUSTED, REASON_EXECUTOR_ERROR, REASON_ISSUER_MISMATCH, REASON_NO_EXECUTOR,
    REASON_VALIDATION_FAILED,
};
pub use protocol::PROTOCOL_VERSION;
