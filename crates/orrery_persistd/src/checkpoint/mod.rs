//! Checkpoint/restore for cell actors (docs/08-persistence.md §8, §3.4).
//!
//! A checkpoint is the durable base of a shard cell's state: the entity bag
//! plus the journal watermark it covers. Restore loads the checkpoint and then
//! replays the journal tail (`lsn > watermark`) — the checkpoint is the base,
//! the journal is the delta, so recovery is zero-loss by construction.
//!
//! The [`CheckpointStore`] trait abstracts the durable tier. The default
//! [`MemCheckpointStore`] makes checkpoint/restore testable with no external
//! service; [`FdbCheckpointStore`] (feature `fdb`) maps the same keyspace onto
//! FoundationDB exactly as D11 §6 specifies.

use std::collections::HashMap;
use std::sync::Mutex;

use orrery_protocol::{CellId, Epoch, Lsn, PersistId};

use crate::actor::EntityRecord;

#[cfg(feature = "fdb")]
pub mod fdb;

#[cfg(feature = "fdb")]
pub use fdb::FdbCheckpointStore;

/// The durable payload of one shard cell's checkpoint (§8, `ckpt/{shard}` row).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckpointData {
    /// The shard cell this checkpoint covers.
    pub shard: CellId,
    /// The shard-ownership epoch the checkpoint was taken under (§3.4 fence).
    pub epoch: Epoch,
    /// The journal LSN covered by this checkpoint (recovery replays `> this`).
    pub watermark: Lsn,
    /// The entity bag (dirty set) at checkpoint time.
    pub entities: HashMap<PersistId, EntityRecord>,
    /// The cell each entity lives in at checkpoint time (split partitioning, §3.5).
    pub by_cell: HashMap<PersistId, CellId>,
    /// Wall-clock time the checkpoint was taken, as unix milliseconds.
    pub taken_at_ms: u64,
}

/// A durable checkpoint store (the system of record for bulk state, D11).
///
/// Async because the FDB-backed implementation drives async transactions; the
/// in-memory default is trivially async. `#[async_trait]` keeps it object-safe
/// so the runtime can hold `&dyn CheckpointStore`.
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Persist a checkpoint for `shard`, overwriting any prior one.
    async fn checkpoint(&self, data: &CheckpointData) -> Result<(), CheckpointError>;

    /// Load the checkpoint for `shard`, or `None` if none exists.
    async fn load(&self, shard: CellId) -> Result<Option<CheckpointData>, CheckpointError>;

    /// Delete the checkpoint for `shard`.
    async fn delete(&self, shard: CellId) -> Result<(), CheckpointError>;
}

/// Errors from a [`CheckpointStore`].
#[derive(Debug)]
pub enum CheckpointError {
    /// The underlying store failed.
    Store(String),
}

impl core::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(s) => write!(f, "checkpoint store error: {s}"),
        }
    }
}

impl core::error::Error for CheckpointError {}

impl From<postcard::Error> for CheckpointError {
    fn from(e: postcard::Error) -> Self {
        Self::Store(format!("encode/decode: {e}"))
    }
}

/// An in-process checkpoint store, keyed by shard cell.
///
/// Used as the default so checkpoint/restore is testable with no external
/// service. It is not durable across process death (that is FDB's job).
#[derive(Debug, Default)]
pub struct MemCheckpointStore {
    map: Mutex<HashMap<CellId, Vec<u8>>>,
}

impl MemCheckpointStore {
    /// A new, empty in-process store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl CheckpointStore for MemCheckpointStore {
    async fn checkpoint(&self, data: &CheckpointData) -> Result<(), CheckpointError> {
        let bytes = postcard::to_stdvec(data)?;
        self.map
            .lock()
            .expect("mem store lock")
            .insert(data.shard, bytes);
        Ok(())
    }

    async fn load(&self, shard: CellId) -> Result<Option<CheckpointData>, CheckpointError> {
        let map = self.map.lock().expect("mem store lock");
        match map.get(&shard) {
            Some(bytes) => Ok(Some(postcard::from_bytes(bytes)?)),
            None => Ok(None),
        }
    }

    async fn delete(&self, shard: CellId) -> Result<(), CheckpointError> {
        self.map.lock().expect("mem store lock").remove(&shard);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(shard: CellId, n: u64) -> CheckpointData {
        let mut entities = HashMap::new();
        for i in 0..n {
            entities.insert(
                PersistId::new(i),
                EntityRecord {
                    components: bytes::Bytes::copy_from_slice(&i.to_le_bytes()),
                    dirty: true,
                },
            );
        }
        CheckpointData {
            shard,
            epoch: Epoch::new(3),
            watermark: Lsn::new(2, 4096),
            entities,
            by_cell: HashMap::new(),
            taken_at_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn mem_store_roundtrips() {
        let store = MemCheckpointStore::new();
        let d = data(CellId::ROOT, 10);
        store.checkpoint(&d).await.unwrap();
        let loaded = store.load(CellId::ROOT).await.unwrap().unwrap();
        assert_eq!(loaded, d);
    }

    #[tokio::test]
    async fn mem_store_missing_is_none() {
        let store = MemCheckpointStore::new();
        assert!(store.load(CellId::ROOT).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mem_store_delete() {
        let store = MemCheckpointStore::new();
        store.checkpoint(&data(CellId::ROOT, 1)).await.unwrap();
        store.delete(CellId::ROOT).await.unwrap();
        assert!(store.load(CellId::ROOT).await.unwrap().is_none());
    }
}
