//! FoundationDB-backed durable registrar rows.
use std::sync::Arc;

use foundationdb::{Database, KeySelector, RangeOption};
use futures::TryStreamExt;
use orrery_protocol::{CellId, GridId, Lease, PersistId};

use crate::{keyspace, FdbContext};

use super::{LeaseMigrate, LeasePut, LeaseStore, LeaseStoreError};

/// FDB implementation of [`LeaseStore`].
pub struct FdbLeaseStore {
    db: Arc<Database>,
}
impl FdbLeaseStore {
    /// Build from the process-scoped FDB context.
    #[must_use]
    pub fn from_context(context: &FdbContext) -> Self {
        Self {
            db: context.database(),
        }
    }
    /// Build from an opened database.
    #[must_use]
    pub fn from_database(db: Arc<Database>) -> Self {
        Self { db }
    }
}
#[async_trait::async_trait]
impl LeaseStore for FdbLeaseStore {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        let db = Arc::clone(&self.db);
        let start = keyspace::lease_cell_range_start(grid, shard);
        let end = keyspace::lease_cell_range_end(grid, shard);
        db.run(move |trx, _| {
            let start = start.clone();
            let end = end.clone();
            async move {
                let mut stream = trx.get_ranges_keyvalues(
                    RangeOption {
                        begin: KeySelector::first_greater_or_equal(start.as_slice()),
                        end: KeySelector::first_greater_or_equal(end.as_slice()),
                        ..RangeOption::default()
                    },
                    false,
                );
                let mut rows = Vec::new();
                while let Some(kv) = stream.try_next().await? {
                    let key = kv.key();
                    if key.len() != 21 {
                        continue;
                    }
                    let cell = CellId::from_bits(u64::from_be_bytes(
                        key[5..13].try_into().expect("length checked"),
                    ))
                    .ok_or_else(|| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                            "invalid lease cell key".into(),
                        )))
                    })?;
                    let entity = PersistId::new(u64::from_be_bytes(
                        key[13..21].try_into().expect("length checked"),
                    ));
                    let Some(value) = trx.get(&keyspace::lease_key(grid, entity), false).await?
                    else {
                        continue;
                    };
                    let row: Lease = postcard::from_bytes(value.as_ref()).map_err(|e| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                            format!("decode lease: {e}"),
                        )))
                    })?;
                    rows.push((cell, row));
                }
                Ok(rows)
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            LeaseStoreError(format!("lease load transaction: {e}"))
        })
    }
    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        let db = Arc::clone(&self.db);
        let row = lease.clone();
        db.run(move |trx, _| {
            let row = row.clone();
            async move {
                let location_key = keyspace::lease_location_key(grid, row.entity);
                if let Some(value) = trx.get(&location_key, false).await? {
                    let bits: [u8; 8] = value.as_ref().try_into().map_err(|_| {
                        foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                            "invalid lease location value".into(),
                        )))
                    })?;
                    let committed =
                        CellId::from_bits(u64::from_be_bytes(bits)).ok_or_else(|| {
                            foundationdb::FdbBindingError::new_custom_error(Box::new(
                                LeaseStoreError("invalid lease location cell".into()),
                            ))
                        })?;
                    if committed != cell {
                        return Ok(LeasePut::LocationConflict(committed));
                    }
                }
                let value = postcard::to_stdvec(&row).map_err(|e| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                        format!("encode lease: {e}"),
                    )))
                })?;
                // Clearing the old index by entity would require a reverse index;
                // actors reject cross-cell claims in this slice, so this is an
                // overwrite at the committed location.
                trx.set(&keyspace::lease_key(grid, row.entity), &value);
                trx.set(&keyspace::lease_cell_key(grid, cell, row.entity), &[]);
                trx.set(&location_key, &cell.to_bits().to_be_bytes());
                Ok(LeasePut::Stored)
            }
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            LeaseStoreError(format!("lease put transaction: {e}"))
        })
    }
    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        let db = Arc::clone(&self.db);
        db.run(move |trx, _| async move {
            let Some(value) = trx
                .get(&keyspace::lease_location_key(grid, entity), false)
                .await?
            else {
                return Ok(None);
            };
            let bits: [u8; 8] = value.as_ref().try_into().map_err(|_| {
                foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                    "invalid lease location value".into(),
                )))
            })?;
            CellId::from_bits(u64::from_be_bytes(bits))
                .map(Some)
                .ok_or_else(|| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                        "invalid lease location cell".into(),
                    )))
                })
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            LeaseStoreError(format!("lease locate transaction: {e}"))
        })
    }
    async fn migrate(
        &self,
        grid: GridId,
        entity: PersistId,
        from: CellId,
        to: CellId,
        expected_lease_id: orrery_protocol::LeaseId,
    ) -> Result<LeaseMigrate, LeaseStoreError> {
        let db = Arc::clone(&self.db);
        db.run(move |trx, _| async move {
            let location_key = keyspace::lease_location_key(grid, entity);
            let Some(location_value) = trx.get(&location_key, false).await? else {
                return Ok(LeaseMigrate::SourceMismatch { actual: None });
            };
            let location_bits: [u8; 8] = location_value.as_ref().try_into().map_err(|_| {
                foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                    "invalid lease location value".into(),
                )))
            })?;
            let actual = CellId::from_bits(u64::from_be_bytes(location_bits)).ok_or_else(|| {
                foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(
                    "invalid lease location cell".into(),
                )))
            })?;
            if actual != from {
                return Ok(LeaseMigrate::SourceMismatch {
                    actual: Some(actual),
                });
            }

            let row_key = keyspace::lease_key(grid, entity);
            let source_key = keyspace::lease_cell_key(grid, from, entity);
            let destination_key = keyspace::lease_cell_key(grid, to, entity);
            let Some(row_value) = trx.get(&row_key, false).await? else {
                return Ok(LeaseMigrate::IndexConflict);
            };
            let row: Lease = postcard::from_bytes(row_value.as_ref()).map_err(|e| {
                foundationdb::FdbBindingError::new_custom_error(Box::new(LeaseStoreError(format!(
                    "decode lease: {e}"
                ))))
            })?;
            if row.entity != entity {
                return Ok(LeaseMigrate::IndexConflict);
            }
            if row.lease_id != expected_lease_id {
                return Ok(LeaseMigrate::LeaseIdMismatch {
                    actual: row.lease_id,
                });
            }
            if trx.get(&source_key, false).await?.is_none() {
                return Ok(LeaseMigrate::IndexConflict);
            }
            if from != to && trx.get(&destination_key, false).await?.is_some() {
                return Ok(LeaseMigrate::IndexConflict);
            }

            trx.clear(&source_key);
            trx.set(&destination_key, &[]);
            trx.set(&location_key, &to.to_bits().to_be_bytes());
            trx.set(&row_key, row_value.as_ref());
            Ok(LeaseMigrate::Migrated)
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            LeaseStoreError(format!("lease migrate transaction: {e}"))
        })
    }
    async fn remove(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        let db = Arc::clone(&self.db);
        db.run(move |trx, _| async move {
            trx.clear(&keyspace::lease_key(grid, entity));
            trx.clear(&keyspace::lease_cell_key(grid, cell, entity));
            trx.clear(&keyspace::lease_location_key(grid, entity));
            Ok(())
        })
        .await
        .map_err(|e: foundationdb::FdbBindingError| {
            LeaseStoreError(format!("lease remove transaction: {e}"))
        })
    }
}
