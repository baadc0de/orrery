#![allow(unsafe_code)] // `foundationdb::boot()` is the one unsafe call, gated behind the `fdb` feature.

//! FoundationDB-backed checkpoint store (`fdb` feature, D11 §6, §5).
//!
//! Maps the checkpoint keyspace onto FDB as specified in docs/08-persistence.md
//! §6: `world/{cell_id}/{entity_id}` → component bag, `ckpt/{shard}` → the
//! watermark row. Written in serializable `db.run` transactions so a checkpoint
//! batch and its watermark commit atomically.
//!
//! This adapter compiles only under the `fdb` feature (it links `libfdb_c`).
//! It needs a **live cluster** to actually run; tests that use it self-skip when
//! no cluster is reachable.

use std::sync::Arc;

use foundationdb::Database;

use orrery_protocol::{CellId, PersistId};

use crate::checkpoint::{CheckpointData, CheckpointError, CheckpointStore};

/// Boot the FoundationDB network once per process and leak the stop guard.
///
/// The FDB C API can only be initialized once per process (docs §5). We boot on
/// first use and intentionally leak the [`NetworkAutoStop`] so the network stays
/// alive for the process lifetime; the OS reclaims it at exit. Safe because this
/// is only ever called once (guarded by `OnceLock`).
fn fdb_network() -> Result<(), CheckpointError> {
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

/// Key prefix for entity rows: `world/{cell_id}/{entity_id}`.
///
/// `cell_id` and `entity_id` are 8-byte big-endian so range scans inherit
/// Morton order (§6: a shard cell's subtree is one contiguous range).
fn world_key(cell: CellId, entity: PersistId) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[0] = b'w';
    key[1..9].copy_from_slice(&cell.to_bits().to_be_bytes());
    key[9..17].copy_from_slice(&entity.0.to_be_bytes());
    key
}

/// Key prefix for the checkpoint watermark: `ckpt/{shard}`.
fn ckpt_key(shard: CellId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'c';
    key[1..9].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

/// The first key of the `world/{cell_id}/…` range for a shard (the shard's
/// subtree start).
fn world_range_start(shard: CellId) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = b'w';
    key[1..9].copy_from_slice(&shard.to_bits().to_be_bytes());
    key
}

/// An FDB-backed checkpoint store.
pub struct FdbCheckpointStore {
    db: Arc<Database>,
}

impl FdbCheckpointStore {
    /// Connect to the cluster at `cluster_file`.
    pub fn connect(cluster_file: &str) -> Result<Self, CheckpointError> {
        fdb_network()?;
        let db = Database::from_path(cluster_file)
            .map_err(|e| CheckpointError::Store(format!("connect: {e}")))?;
        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait::async_trait]
impl CheckpointStore for FdbCheckpointStore {
    async fn checkpoint(&self, data: &CheckpointData) -> Result<(), CheckpointError> {
        // One serializable transaction: write all entity rows + the watermark.
        // Rows are idempotent overwrites, so a partially applied checkpoint
        // (e.g. interrupted before commit) is simply re-run (§5).
        let db = Arc::clone(&self.db);
        let data = data.clone();
        db.run(|trx, _| {
            let data = data.clone();
            async move {
                for (entity, record) in &data.entities {
                    let key = world_key(data.shard, *entity);
                    trx.set(&key, &record.components);
                }
                let encoded = postcard::to_stdvec(&data).map_err(|e| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(
                        CheckpointError::Store(format!("encode: {e}")),
                    ))
                })?;
                trx.set(&ckpt_key(data.shard), &encoded);
                Ok(())
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("checkpoint txn: {e}"))
        })
    }

    async fn load(&self, shard: CellId) -> Result<Option<CheckpointData>, CheckpointError> {
        let db = Arc::clone(&self.db);
        db.run(|trx, _| async move {
            let raw = trx.get(&ckpt_key(shard), false).await?;
            match raw {
                Some(bytes) => {
                    let data: CheckpointData =
                        postcard::from_bytes(bytes.as_ref()).map_err(|e| {
                            foundationdb::FdbBindingError::new_custom_error(Box::new(
                                CheckpointError::Store(format!("decode: {e}")),
                            ))
                        })?;
                    Ok(Some(data))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("load txn: {e}"))
        })
    }

    async fn delete(&self, shard: CellId) -> Result<(), CheckpointError> {
        let db = Arc::clone(&self.db);
        db.run(|trx, _| async move {
            trx.clear(&ckpt_key(shard));
            // Also clear the entity rows for this shard's subtree.
            let start = world_range_start(shard);
            let end = {
                let mut k = start;
                // Exclusive end: one past the 9-byte prefix.
                k[8] = k[8].wrapping_add(1);
                k
            };
            trx.clear_range(&start, &end);
            Ok(())
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            CheckpointError::Store(format!("delete txn: {e}"))
        })
    }
}
