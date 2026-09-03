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
pub mod adjudication;
pub mod archive;
pub mod audit;
pub mod census;
pub mod checkpoint;
pub mod cluster;
pub mod content_version;
mod crc;
#[cfg(feature = "fdb")]
pub mod fdb;
pub mod fence;
pub mod gateway;
pub mod intent;
pub mod journal;
pub mod keyspace;
pub mod lease;
pub mod migration;
pub mod placement;
pub mod reliable;
pub mod runtime;
pub mod schema;
pub mod witness_epoch;

pub use actor::{
    CellActorHandle, DivestOutcome, EntityRecord, FencedApply, Reject, RekeyError, SnapshotPage,
    Tombstone,
};
pub use adjudication::{AdjudicationExecutor, RETAINED_BUILDS};
#[cfg(feature = "fdb")]
pub use census::{scan_fdb as scan_world_census_fdb, DEFAULT_PAGE_ROWS};
pub use census::{GridWorldCensus, WorldCensus};
pub use checkpoint::{
    spawn_checkpoint_scheduler, spawn_checkpoint_scheduler_direct, CheckpointCause,
    CheckpointConfig, CheckpointData, CheckpointError, CheckpointScheduler, CheckpointStore,
    ColdCellReader, MemCheckpointStore, QuiesceSignal,
};
pub use cluster::{Cluster, ColdFallbackRouter, Router};
pub use content_version::{ContentVersion, CONTENT_VERSION_ENCODING_V1};
#[cfg(feature = "fdb")]
pub use fdb::{FdbContext, FdbContextError};
pub use fence::{
    overlapping_active_ownership, ActivationOutcome, FenceError, FenceFreshnessConfig,
    FenceFreshnessError, FenceFreshnessMonitor, FenceOutcome, FenceRow, FenceStatus, FenceStore,
    MemFenceStore, OwnedShard, ShardActivation,
};
pub use gateway::{
    interest_covers_read, AreaInterestScoping, AuthorityCorrectionEnforcement,
    AuthorityCorrectionMetrics, AuthorityCorrectionPosture, AuthorityCorrectionSnapshot,
    AuthorityMetrics, AuthoritySnapshot, BulkAckAdmission, BulkAckDisposition,
    CoordinatorHandoutAuthority, DrainReport, DuplicateAuthoritySample, FreshBulkAckAdmission,
    GatewayAreaMetrics, GatewayAreaSnapshot, GatewayBulkLatencySnapshot, GatewayBulkMetrics,
    GatewayBulkSample, GatewayBulkSnapshot, GatewayConfig, GatewayError, GatewayIntentMetrics,
    GatewayIntentSnapshot, GatewayMetrics, GatewayReportMetrics, GatewayReportSnapshot,
    GatewayServer, GatewayServerLatency, GatewayServerLatencySnapshot,
    NearestInterestSuccessorPolicy, ParkOnLossPolicy, RampMeters, RegistrarSweepClock,
    ShardDrainHandle, SharedAdjudicator, SharedBulkAckAdmission, SharedSuccessorPolicy,
    SuccessorCandidate, SuccessorPolicy, SuccessorRequest, AUTHORITY_CORRECTION_CONTROL,
    GATEWAY_ALPN, MAX_INTEREST_PEERS,
};
pub use intent::{
    item_transfer_verdict, IntentError, IntentExecutor, IntentPrecheck, IntentValidator,
    IntentVerdict, ItemTransferArgs, MemIntentExecutor, OpsVerdict, PermissiveValidator,
    LEDGER_CREDIT_OP, LEDGER_ITEM_TRANSFER_ARGS_BYTES, LEDGER_ITEM_TRANSFER_OP,
};
#[cfg(feature = "fdb")]
pub use intent::{FdbIntentExecutor, IntentFence};
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
pub use lease::stages::{
    lease_stage_metrics, HeartbeatTrace, LeaseStageMetrics, LeaseStageSnapshot,
};
#[cfg(feature = "fdb")]
pub use lease::FdbLeaseStore;
pub use lease::{
    ClaimResult, LeaseMigrate, LeasePut, LeaseRegistrar, LeaseStore, LeaseStoreError,
    MemLeaseStore, LEASE_TTL_MS,
};
pub use migration::{
    spawn_migration_sweep, MigratingStore, MigrationConfig, MigrationError, MigrationRegistry,
    MigrationSweepConfig, MigrationSweepTarget, MigrationSweeper,
};
pub use placement::{RendezvousHasher, RendezvousNode, RendezvousWeight};
pub use reliable::{Lane, ReliableSender};
pub use runtime::{payload_crc, CellRuntime, RuntimeConfig, ShardTransfer, TransferPhase};
pub use schema::{ComponentBag, ComponentSlot};
pub use witness_epoch::{AcceptedEpoch, WitnessEpochAuthority};
