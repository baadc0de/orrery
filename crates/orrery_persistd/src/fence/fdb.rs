#![allow(unsafe_code)] // `foundationdb::boot()` is the one unsafe call, gated behind the `fdb` feature.

//! FoundationDB-backed fence store (`fdb` feature, D11 §6, §3.4/§3.5).
//!
//! Maps the `actor/{shard}` keyspace onto FDB as specified in
//! docs/08-persistence.md §6: `actor/{shard_cell_id}` → `(owner, epoch,
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

use orrery_protocol::CellId;

use crate::fence::{FenceError, FenceOutcome, FenceRow, FenceStatus, FenceStore};

/// Boot the FoundationDB network once per process and leak the stop guard.
///
/// The FDB C API can only be initialized once per process (docs §5). We boot on
/// first use and intentionally leak the [`NetworkAutoStop`] so the network stays
/// alive for the process lifetime; the OS reclaims it at exit. Safe because this
/// is only ever called once (guarded by `OnceLock`).
fn fdb_network() -> Result<(), FenceError> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // `foundationdb::boot()` panics on failure, so once we get here the network
    // is up. We leak the guard so the network lives for the process lifetime.
    ONCE.call_once(|| {
        // SAFETY: boot() is called exactly once per process (Once), and the
        // returned guard is leaked so the network outlives every use.
        let guard = unsafe { foundationdb::boot() };
        std::mem::forget(guard);
    });
    Ok(())
}

/// Key for the fence row: `actor/{shard_cell_id}`.
///
/// `shard_cell_id` is the 8-byte big-endian Morton `CellId` (D11 §6).
fn fence_key(shard: CellId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'a';
    key[1..9].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

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
    /// Connect to the cluster at `cluster_file`.
    pub fn connect(cluster_file: &str) -> Result<Self, FenceError> {
        fdb_network()?;
        let db = Database::from_path(cluster_file)
            .map_err(|e| FenceError::Store(format!("connect: {e}")))?;
        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait::async_trait]
impl FenceStore for FdbFenceStore {
    async fn read(&self, shard: CellId) -> Result<Option<FenceRow>, FenceError> {
        let db = Arc::clone(&self.db);
        let key = fence_key(shard);
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
        shard: CellId,
        expected: Option<&FenceRow>,
        new: &FenceRow,
    ) -> Result<FenceOutcome, FenceError> {
        let db = Arc::clone(&self.db);
        let key = fence_key(shard);
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
        parent: CellId,
        parent_expected: &FenceRow,
        children: &[(CellId, FenceRow)],
    ) -> Result<FenceOutcome, FenceError> {
        let db = Arc::clone(&self.db);
        let parent_key = fence_key(parent);
        let parent_expected = *parent_expected;
        let children: Vec<(Vec<u8>, Vec<u8>)> = children
            .iter()
            .map(|(c, row)| {
                let key = fence_key(*c);
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

    async fn retire(&self, shard: CellId) -> Result<(), FenceError> {
        let db = Arc::clone(&self.db);
        let key = fence_key(shard);
        db.run(move |trx, _| async move {
            trx.clear(&key);
            Ok(())
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| FenceError::Store(format!("retire txn: {e}")))
    }
}
