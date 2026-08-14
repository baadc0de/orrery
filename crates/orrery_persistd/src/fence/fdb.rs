//! FoundationDB-backed fence store (`fdb` feature, D11 §6, §3.4/§3.5).
//!
//! Maps the `actor/{grid}/{shard}` keyspace onto FDB as specified in
//! docs/08-persistence.md §6: `actor/{grid}/{shard_cell_id}` → `(owner, epoch,
//! status)`. Fencing and split are written in **serializable** `db.run`
//! transactions so the read-compare-set is atomic — a zombie actor's stale
//! checkpoint commit conflicts with the CAS, and a split's parent-status
//! change plus its eight child rows commit atomically.
//!
//! This adapter compiles only under the `fdb` feature (it links `libfdb_c`).
//! It needs a **live cluster** to actually run; tests that use it self-skip
//! when no cluster is reachable.

use std::sync::Arc;

use foundationdb::Database;

use orrery_protocol::{CellId, GridId};

use crate::fence::{FenceError, FenceOutcome, FenceRow, FenceStatus, FenceStore};
use crate::keyspace;
use crate::FdbContext;

/// Encode a fence row (postcard, matching the in-memory store's wire format).
fn encode_row(row: &FenceRow) -> Result<Vec<u8>, FenceError> {
    postcard::to_stdvec(row).map_err(FenceError::from)
}

/// Decode a fence row.
fn decode_row(bytes: &[u8]) -> Result<FenceRow, FenceError> {
    postcard::from_bytes(bytes).map_err(FenceError::from)
}

/// An FDB-backed fence store.
pub struct FdbFenceStore {
    db: Arc<Database>,
}

impl FdbFenceStore {
    /// Build a fence store using a process-scoped FDB context.
    #[must_use]
    pub fn from_context(context: &FdbContext) -> Self {
        Self {
            db: context.database(),
        }
    }

    /// Build a fence store from an already-open database handle.
    #[must_use]
    pub fn from_database(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Connect to the cluster at `cluster_file`.
    ///
    /// Prefer constructing one [`FdbContext`] and using [`Self::from_context`]
    /// when a process needs more than one FDB-backed adapter.
    pub fn connect(cluster_file: &str) -> Result<Self, FenceError> {
        let context =
            FdbContext::connect(cluster_file).map_err(|e| FenceError::Store(e.to_string()))?;
        Ok(Self::from_context(&context))
    }
}

#[async_trait::async_trait]
impl FenceStore for FdbFenceStore {
    async fn read(&self, grid: GridId, shard: CellId) -> Result<Option<FenceRow>, FenceError> {
        let db = Arc::clone(&self.db);
        let key = keyspace::fence_key(grid, shard);
        db.run(move |trx, _| async move {
            let raw = trx.get(&key, false).await?;
            match raw {
                Some(bytes) => {
                    let row = decode_row(bytes.as_ref()).map_err(|e| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                    })?;
                    Ok(Some(row))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| FenceError::Store(format!("read txn: {e}")))
    }

    async fn fence(
        &self,
        grid: GridId,
        shard: CellId,
        expected: Option<&FenceRow>,
        new: &FenceRow,
    ) -> Result<FenceOutcome, FenceError> {
        let db = Arc::clone(&self.db);
        let key = keyspace::fence_key(grid, shard);
        let expected = expected.copied();
        let new = *new;
        db.run(move |trx, _| async move {
            let raw = trx.get(&key, false).await?;
            let current =
                match raw {
                    Some(bytes) => Some(decode_row(bytes.as_ref()).map_err(|e| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                    })?),
                    None => None,
                };
            if current != expected {
                return Ok(FenceOutcome::Conflict { current });
            }
            let encoded = encode_row(&new)
                .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))?;
            trx.set(&key, &encoded);
            Ok(FenceOutcome::Fenced)
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| FenceError::Store(format!("fence txn: {e}")))
    }

    async fn begin_split(
        &self,
        grid: GridId,
        parent: CellId,
        parent_expected: &FenceRow,
        children: &[(CellId, FenceRow)],
    ) -> Result<FenceOutcome, FenceError> {
        let db = Arc::clone(&self.db);
        let parent_key = keyspace::fence_key(grid, parent);
        let parent_expected = *parent_expected;
        let children: Vec<(Vec<u8>, Vec<u8>)> = children
            .iter()
            .map(|(c, row)| {
                let key = keyspace::fence_key(grid, *c);
                let encoded = encode_row(row).expect("row encodes");
                (key.to_vec(), encoded)
            })
            .collect();
        db.run(move |trx, _| {
            let children = children.clone();
            async move {
                let raw = trx.get(&parent_key, false).await?;
                let current = match raw {
                    Some(bytes) => Some(decode_row(bytes.as_ref()).map_err(|e| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                    })?),
                    None => None,
                };
                if current != Some(parent_expected) {
                    return Ok(FenceOutcome::Conflict { current });
                }
                // Mark the parent Splitting (same owner/epoch, new status).
                let splitting = FenceRow {
                    status: FenceStatus::Splitting,
                    ..parent_expected
                };
                let encoded = encode_row(&splitting)
                    .map_err(|e| foundationdb::FdbBindingError::new_custom_error(Box::new(e)))?;
                trx.set(&parent_key, &encoded);
                for (key, encoded) in &children {
                    trx.set(key, encoded);
                }
                Ok(FenceOutcome::Fenced)
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| FenceError::Store(format!("split txn: {e}")))
    }

    async fn retire(&self, grid: GridId, shard: CellId) -> Result<(), FenceError> {
        let db = Arc::clone(&self.db);
        let key = keyspace::fence_key(grid, shard);
        db.run(move |trx, _| async move {
            trx.clear(&key);
            Ok(())
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| FenceError::Store(format!("retire txn: {e}")))
    }
}
