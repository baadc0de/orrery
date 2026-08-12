//! Orrery wire and data types.
//!
//! Every serialized thing crosses this crate: [`CellId`], intents, leases,
//! attestations, evidence bundles, input-log records, and the protocol version
//! constant. It is engine-agnostic (glam for vector math, `iroh-base` for the
//! ed25519 identity/signature types — no Bevy, no tokio) so servers, tools, and
//! tests link it without an engine (D15, docs/10-crates.md §1).
//!
//! Normative source: [DECISIONS.md](https://github.com/baadc0de/orrery/blob/main/docs/DECISIONS.md)
//! D5 (CellId encoding), D7 (leases), D9 (input logs), D10 (evidence), D11
//! (intents), D15 (canonical scalars).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cell;
pub mod coord;
mod grid;
mod identity;
mod persist;
mod protocol;

pub use cell::{CellId, CellRangeError};
pub use coord::{CoordMsg, IslandId, IslandManifest, PeerEntry, TopologyRegime};
pub use grid::GridId;
pub use identity::{NodeId, Signature};
pub use persist::{
    Attestation, Checkpoint, Epoch, Intent, IntentOp, IntentOutcome, JournalRecord, Lsn, PersistId,
    RecordKind, Tick,
};
pub use protocol::PROTOCOL_VERSION;
