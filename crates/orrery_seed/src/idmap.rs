//! `pid/next` allocation and `seedmap/` lookups for the seeder.
//!
//! The seeder keeps designed content stable across reruns by mapping each
//! `ContentKey` to its minted `PersistId` and first-seen metadata. Fresh ids
//! are leased from `pid/next` in blocks so one worker does not serialize on
//! the counter for every row.

use std::collections::BTreeMap;

use orrery_persistd::keyspace;
use orrery_protocol::{CellId, GridId, PersistId};
use serde::{Deserialize, Serialize};

use crate::content::ContentKey;

/// The default size of a block grant from `pid/next`.
pub const DEFAULT_BLOCK_GRANT: u64 = 4096;

/// One block grant of contiguous `PersistId`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGrant {
    /// The first minted id in the block.
    pub start: PersistId,
    /// The number of ids in the block.
    pub len: u64,
}

/// A cursor over a grant that hands out ids one by one.
#[derive(Debug, Clone)]
pub struct BlockGrantCursor {
    next: u64,
    end: u64,
}

impl BlockGrantCursor {
    /// Create a cursor over `grant`.
    #[must_use]
    pub fn new(grant: BlockGrant) -> Self {
        Self {
            next: grant.start.0,
            end: grant.start.0.saturating_add(grant.len),
        }
    }

    /// Mint one id from the grant.
    #[must_use]
    pub fn next_id(&mut self) -> Option<PersistId> {
        if self.next >= self.end {
            return None;
        }
        let id = self.next;
        self.next = self.next.saturating_add(1);
        Some(PersistId::new(id))
    }
}

/// One row in `seedmap/{content_key}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedMapRow {
    /// The stable persisted id.
    pub persist_id: PersistId,
    /// The entity's grid.
    pub grid: GridId,
    /// The entity's cell.
    pub cell: CellId,
    /// The first content build that minted this row.
    pub first_seen_build: String,
}

/// Decode a seedmap value.
pub fn decode_seedmap_value(value: &[u8]) -> Result<SeedMapRow, String> {
    postcard::from_bytes(value).map_err(|e| format!("seedmap decode: {e}"))
}

/// Encode a seedmap value.
#[must_use]
pub fn encode_seedmap_value(row: &SeedMapRow) -> Vec<u8> {
    postcard::to_stdvec(row).expect("seedmap row encodes")
}

/// Build the logical `seedmap/{content_key}` key from the seeder's
/// `ContentKey` newtype.
#[must_use]
pub fn seedmap_key(content_key: &ContentKey) -> [u8; 17] {
    keyspace::seedmap_key(content_key.0)
}

/// Build the logical `seedprog/{emit}/{grid}/{cell}` resume key from the emit
/// name.
#[must_use]
pub fn seedprog_key(emit: &str, grid: GridId, cell: CellId) -> [u8; 21] {
    let hash = blake3::hash(emit.as_bytes());
    let mut emit_hash = [0u8; 8];
    emit_hash.copy_from_slice(&hash.as_bytes()[..8]);
    keyspace::seedprog_key(emit_hash, grid, cell)
}

/// A seedmap index in memory, keyed by `ContentKey`.
pub type SeedMap = BTreeMap<ContentKey, SeedMapRow>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_yields_exactly_the_reserved_block() {
        let mut cursor = BlockGrantCursor::new(BlockGrant {
            start: PersistId::new(41),
            len: 2,
        });

        assert_eq!(cursor.next_id(), Some(PersistId::new(41)));
        assert_eq!(cursor.next_id(), Some(PersistId::new(42)));
        assert_eq!(cursor.next_id(), None);
    }
}

#[cfg(feature = "fdb")]
mod fdb {
    use super::*;
    use foundationdb::options::MutationType;
    use foundationdb::{Database, FdbBindingError, KeySelector, RangeOption};
    use futures::stream::TryStreamExt;

    use orrery_protocol::PersistId;

    /// Reserve a block from `pid/next` with an FDB atomic add.
    pub async fn reserve_block(
        db: &Database,
        grid: GridId,
        len: u64,
    ) -> Result<BlockGrant, String> {
        db.run(|trx, _| async move {
            let key = keyspace::pid_next_key(grid);
            let base = match trx.get(&key, false).await? {
                Some(v) => {
                    let mut buf = [0u8; 8];
                    let n = v.len().min(8);
                    buf[..n].copy_from_slice(&v[..n]);
                    u64::from_le_bytes(buf)
                }
                None => 0,
            };
            trx.atomic_op(&key, &len.to_le_bytes(), MutationType::Add);
            Ok(BlockGrant {
                start: PersistId::new(base + 1),
                len,
            })
        })
        .await
        .map_err(|e: FdbBindingError| format!("pid/next reserve: {e}"))
    }

    /// Read the whole seedmap from FDB.
    pub async fn read_seedmap(db: &Database) -> Result<SeedMap, String> {
        db.run(|trx, _| async move {
            let begin = keyspace::seedmap_range_start();
            let end = keyspace::seedmap_range_end();
            let opt = RangeOption {
                begin: KeySelector::first_greater_or_equal(begin.as_slice()),
                end: KeySelector::first_greater_or_equal(end.as_slice()),
                ..RangeOption::default()
            };
            let mut out = SeedMap::new();
            let mut stream = trx.get_ranges_keyvalues(opt, false);
            while let Some(kv) = stream.try_next().await? {
                let key = kv.key();
                if key.len() != 17 || key[0] != b's' {
                    continue;
                }
                let mut content = [0u8; 16];
                content.copy_from_slice(&key[1..]);
                let content_key = ContentKey(content);
                let row = decode_seedmap_value(kv.value()).map_err(|e| {
                    FdbBindingError::new_custom_error(Box::new(std::io::Error::other(e)))
                })?;
                out.insert(content_key, row);
            }
            Ok(out)
        })
        .await
        .map_err(|e: FdbBindingError| format!("seedmap scan: {e}"))
    }
}

#[cfg(feature = "fdb")]
pub use fdb::{read_seedmap, reserve_block};
