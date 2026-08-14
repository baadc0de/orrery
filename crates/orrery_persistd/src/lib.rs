//! Orrery persistence cluster harness (D11, docs/10-crates.md §11).
//!
//! Slice 2 (P2) implements the durable bulk tier in isolation, testable without
//! FoundationDB: a **single-writer cell-actor runtime** whose writes go through
//! a **per-node segmented append-only journal** with **adaptive group commit**.
//! The ack is the durability contract: an `apply_diff` resolves only once its
//! journal record is group-fsynced (docs/08-persistence.md §2.1, §4).
//!
//! Slice 2 continuation adds the **`actor/{shard}` fencing CAS** (§3.4) and the
//! **hotspot split** (§3.5): a [`FenceStore`] (in-memory by default, FDB-backed
//! behind the `fdb` feature) holds the placement/fencing rows, the runtime
//! fences shards on assumption and splits hot shards into their eight children
//! atomically, and the actor tracks each entity's cell so splits partition
//! correctly (including after a checkpoint/restore).
//!
//! Targets (D16): journal commit **< 2 ms server-internal**; client-observed
//! bulk ack **p99 < 5 ms in-region**. This slice measures the former; the
//! latter is the latency rig's job later.
//!
//! Later P2 slices: the gateway/iroh/tonic surface, the `Ruleset`-linked
//! intent validator and adjudication executor.

#![deny(unsafe_code)]
// `deny`, not `forbid`, so the `fdb`-feature module can allow the one unsafe call in `foundationdb::boot()`.
#![warn(missing_docs)]

pub mod actor;
pub mod checkpoint;
pub mod cluster;
mod crc;
#[cfg(feature = "fdb")]
pub mod fdb;
pub mod fence;
pub mod gateway;
pub mod intent;
pub mod journal;
pub mod keyspace;
pub mod placement;
pub mod runtime;

pub use actor::{CellActorHandle, CellMsg, EntityRecord, Reject, SnapshotPage, Tombstone};
pub use checkpoint::{
    spawn_checkpoint_scheduler, CheckpointConfig, CheckpointData, CheckpointError,
    CheckpointScheduler, CheckpointStore, ColdCellReader, MemCheckpointStore, QuiesceSignal,
};
pub use cluster::{Cluster, ColdFallbackRouter, Router};
#[cfg(feature = "fdb")]
pub use fdb::{FdbContext, FdbContextError};
pub use fence::{
    ActivationOutcome, FenceError, FenceFreshnessConfig, FenceFreshnessError,
    FenceFreshnessMonitor, FenceOutcome, FenceRow, FenceStatus, FenceStore, MemFenceStore,
    ShardActivation,
};
pub use gateway::{
    BulkAckAdmission, BulkAckDisposition, FreshBulkAckAdmission, GatewayConfig, GatewayError,
    GatewayServer, SharedBulkAckAdmission, GATEWAY_ALPN,
};
#[cfg(feature = "fdb")]
pub use intent::{FdbIntentExecutor, IntentFence};
pub use intent::{
    IntentError, IntentExecutor, IntentPrecheck, IntentValidator, IntentVerdict, MemIntentExecutor,
    PermissiveValidator,
};
#[cfg(feature = "chain-grpc")]
pub use journal::{
    spawn_adopted_chain, spawn_chain_grpc, AdoptedChainHistory, ChainGrpcServer, DurableChainId,
    GrpcChainTransport,
};
pub use journal::{
    spawn_chain, AppendHandle, ChainConfig, ChainReplicator, ChainSink, ChainTransport, Journal,
    JournalChainSink, JournalCommitMetrics, JournalCommitSample, JournalCommitSnapshot,
    JournalConfig, JournalError, JournalScan, JournalStageSnapshot, MemChainTransport,
    StoredRecord,
};
pub use placement::{RendezvousHasher, RendezvousNode, RendezvousWeight};
pub use runtime::{payload_crc, CellRuntime, RuntimeConfig};
