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

use crate::fence::{
    validate_activation_set, ActivationOutcome, FenceError, FenceOutcome, FenceRow, FenceStatus,
    FenceStore, ShardActivation,
};
use futures::TryStreamExt as _;

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

    /// One range read in one transaction, whatever the shard count.
    ///
    /// The default in [`FenceStore`] is a loop over [`FenceStore::read`], and
    /// `read` here is a whole `db.run` — so the freshness monitor's
    /// once-a-second confirmation of a 128-shard node was 128 transactions a
    /// second, each with its own read version, for rows that are adjacent in
    /// the keyspace. docs/08-persistence.md §2.2.7 made exactly this change on
    /// the intent path's ownership fence; this is the same change on the
    /// background monitor that watches the same rows.
    ///
    /// The rows are returned positionally against `shards`, so the caller's
    /// comparison order is unchanged.
    async fn read_many(
        &self,
        grid: GridId,
        shards: &[CellId],
    ) -> Result<Vec<Option<FenceRow>>, FenceError> {
        let (Some(lo), Some(hi)) = (
            shards.iter().map(|s| s.to_bits()).min(),
            shards.iter().map(|s| s.to_bits()).max(),
        ) else {
            return Ok(Vec::new());
        };
        let db = Arc::clone(&self.db);
        let start = keyspace::fence_key(grid, CellId::from_bits(lo).expect("shard round-trips"));
        let mut end =
            keyspace::fence_key(grid, CellId::from_bits(hi).expect("shard round-trips")).to_vec();
        end.push(0);
        let wanted: Vec<u64> = shards.iter().map(|s| s.to_bits()).collect();
        db.run(move |trx, _| {
            let (start, end, wanted) = (start, end.clone(), wanted.clone());
            async move {
                let mut stream = trx.get_ranges_keyvalues(
                    foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                        end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                        ..foundationdb::RangeOption::default()
                    },
                    false,
                );
                let mut seen: std::collections::HashMap<u64, FenceRow> =
                    std::collections::HashMap::new();
                while let Some(kv) = stream.try_next().await? {
                    let key = kv.key();
                    if key.len() != 13 {
                        continue;
                    }
                    let bits =
                        u64::from_be_bytes(key[5..13].try_into().expect("13-byte fence key"));
                    let row = decode_row(kv.value()).map_err(|e| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                    })?;
                    seen.insert(bits, row);
                }
                Ok(wanted.iter().map(|bits| seen.get(bits).copied()).collect())
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

    async fn activate_shards(
        &self,
        grid: GridId,
        owner: u64,
        shards: &[ShardActivation],
    ) -> Result<ActivationOutcome, FenceError> {
        validate_activation_set(shards)?;
        let db = Arc::clone(&self.db);
        let requests: Vec<(CellId, Vec<u8>, Option<FenceRow>, FenceRow)> = shards
            .iter()
            .map(|request| {
                let row = FenceRow {
                    owner,
                    epoch: orrery_protocol::Epoch::new(
                        request.expected.map_or(0, |current| current.epoch.0) + 1,
                    ),
                    status: FenceStatus::Active,
                };
                (
                    request.shard,
                    keyspace::fence_key(grid, request.shard).to_vec(),
                    request.expected,
                    row,
                )
            })
            .collect();
        db.run(move |trx, _| {
            let requests = requests.clone();
            async move {
                // Read the complete compare set before writing any row. FDB
                // commits this transaction all-or-nothing; a conflicting fence
                // change makes `run` retry and then observe the mismatch.
                for (shard, key, expected, _) in &requests {
                    let raw = trx.get(key, false).await?;
                    let current = match raw {
                        Some(bytes) => Some(decode_row(bytes.as_ref()).map_err(|e| {
                            foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                        })?),
                        None => None,
                    };
                    if current != *expected {
                        return Ok(ActivationOutcome::Conflict {
                            shard: *shard,
                            current,
                        });
                    }
                }
                let mut rows = Vec::with_capacity(requests.len());
                for (shard, key, _, row) in &requests {
                    let encoded = encode_row(row).map_err(|e| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(e))
                    })?;
                    trx.set(key, &encoded);
                    rows.push((*shard, *row));
                }
                Ok(ActivationOutcome::Activated { rows })
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            FenceError::Store(format!("activation txn: {e}"))
        })
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
