//! The `actor/{grid}/{shard}` placement + fencing store (docs/08-persistence.md §3.4, §3.5).
//!
//! Every shard cell has a durable row recording who owns it, under which epoch
//! (the fencing token), and its placement status. On assuming shard `S`, a node
//! CASes `actor/{grid}/{S}` from `(old_node, e)` to `(self, e+1)` in one transaction;
//! every subsequent checkpoint transaction reads `actor/{grid}/{S}` and aborts if the
//! epoch moved — a zombie actor (network-partitioned former owner) can never
//! commit a stale checkpoint, because its commit would conflict with the CAS.
//!
//! The same row drives the hotspot split (§3.5): the split transaction writes
//! the eight child rows at epoch `e+1` and marks the parent `Splitting` in one
//! atomic step, so no gateway can observe a half-split.
//!
//! The [`FenceStore`] trait abstracts the durable tier. The default
//! [`MemFenceStore`] makes fencing/splitting testable with no external service;
//! [`FdbFenceStore`] (feature `fdb`) maps the same keyspace onto FoundationDB
//! exactly as D11 §6 specifies (`actor/{grid}/{shard_cell_id}`).

use std::collections::HashMap;
use std::sync::Mutex;

use orrery_protocol::{CellId, Epoch, GridId};

mod freshness;
pub use freshness::{FenceFreshnessConfig, FenceFreshnessError, FenceFreshnessMonitor};

#[cfg(feature = "fdb")]
pub mod fdb;

#[cfg(feature = "fdb")]
pub use fdb::FdbFenceStore;

/// The placement/fencing status of a shard cell (D11 §3.4, §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FenceStatus {
    /// The shard is actively owned and serving.
    Active,
    /// The shard is mid-split: the parent is quiesced and the children are
    /// being brought up at epoch `e+1`.
    Splitting,
}

/// The `actor/{grid}/{shard}` row: placement + fencing (D11 §6).
///
/// `owner` is the persistd node id (the same u64 used for rendezvous
/// placement); `epoch` is the fencing token. Every [`JournalRecord`] and
/// checkpoint carries the epoch it was written under, so recovery discards
/// records from a superseded epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FenceRow {
    /// The node that owns (or is splitting) the shard.
    pub owner: u64,
    /// The shard-ownership epoch (fencing token).
    pub epoch: Epoch,
    /// The placement/fencing status.
    pub status: FenceStatus,
}

/// The result of a fencing CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceOutcome {
    /// The CAS succeeded; the row is now the requested value.
    Fenced,
    /// The CAS failed because the current row differs from the expected one.
    Conflict {
        /// The row actually present (or `None` if the shard has no row).
        current: Option<FenceRow>,
    },
}

/// One shard participating in an atomic ownership activation.
///
/// `expected` is the row observed by the coordinator before activation.  It
/// is deliberately part of the request: bootstrap (`None`), restart (our
/// previous row), and promotion (the failed primary's row) all use the same
/// compare-and-set rule and cannot silently overwrite a newer owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardActivation {
    /// The grid-relative shard cell to activate.
    pub shard: CellId,
    /// The exact durable row expected before this activation.
    pub expected: Option<FenceRow>,
}

/// Result of atomically activating a set of shards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationOutcome {
    /// Every requested shard now has this node as its active owner.  Rows are
    /// returned in the request's canonical shard order.
    Activated {
        /// The newly durable active rows, in canonical shard order.
        rows: Vec<(CellId, FenceRow)>,
    },
    /// One request no longer matched its durable row; none of the set changed.
    Conflict {
        /// The request whose precondition no longer matched.
        shard: CellId,
        /// The current durable row for that shard.
        current: Option<FenceRow>,
    },
}

/// Errors from a [`FenceStore`].
#[derive(Debug)]
pub enum FenceError {
    /// The underlying store failed.
    Store(String),
    /// A fencing/split CAS failed because the live row differs from expected.
    Conflict {
        /// The row actually present (or `None` if the shard has no row).
        current: Option<FenceRow>,
    },
    /// An activation set was not in canonical order or contains overlapping
    /// subtrees.  Such a set is ambiguous and must never be committed.
    InvalidActivation(String),
}

impl core::fmt::Display for FenceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(s) => write!(f, "fence store error: {s}"),
            Self::Conflict { current } => write!(f, "fence conflict: current row {current:?}"),
            Self::InvalidActivation(s) => write!(f, "invalid shard activation: {s}"),
        }
    }
}

impl core::error::Error for FenceError {}

impl From<postcard::Error> for FenceError {
    fn from(e: postcard::Error) -> Self {
        Self::Store(format!("encode/decode: {e}"))
    }
}

/// A durable placement/fencing store for `actor/{grid}/{shard}` rows (D11 §3.4/§3.5).
///
/// Async because the FDB-backed implementation drives async transactions; the
/// in-memory default is trivially async. `#[async_trait]` keeps it object-safe
/// so the runtime can hold `&dyn FenceStore`.
#[async_trait::async_trait]
pub trait FenceStore: Send + Sync {
    /// Read the current fence row for `shard`, or `None` if none exists.
    async fn read(&self, grid: GridId, shard: CellId) -> Result<Option<FenceRow>, FenceError>;

    /// CAS `actor/{shard}` from `expected` to `new` in one transaction.
    ///
    /// `expected == None` means the shard is expected to have no row (cold
    /// start). Returns [`FenceOutcome::Conflict`] with the live row if the
    /// precondition does not hold.
    async fn fence(
        &self,
        grid: GridId,
        shard: CellId,
        expected: Option<&FenceRow>,
        new: &FenceRow,
    ) -> Result<FenceOutcome, FenceError>;

    /// Atomically activate a canonical, non-overlapping shard set for
    /// `owner`.  Each row advances its expected epoch by one and becomes
    /// [`FenceStatus::Active`].
    ///
    /// This is the durable ownership transition used for bootstrap, clean
    /// restart, and follower promotion.  A mismatch returns the offending
    /// row and leaves *every* shard unchanged.
    async fn activate_shards(
        &self,
        grid: GridId,
        owner: u64,
        shards: &[ShardActivation],
    ) -> Result<ActivationOutcome, FenceError>;

    /// Atomically mark `parent` `Splitting` and write the child rows (§3.5).
    ///
    /// The split transaction commits the parent's status change and all eight
    /// child rows together, so a gateway can never observe a half-split. Fails
    /// with [`FenceOutcome::Conflict`] if the parent row is not exactly
    /// `parent_expected`.
    async fn begin_split(
        &self,
        grid: GridId,
        parent: CellId,
        parent_expected: &FenceRow,
        children: &[(CellId, FenceRow)],
    ) -> Result<FenceOutcome, FenceError>;

    /// Retire `shard`: delete its `actor/{grid}/{shard}` row (§3.5).
    ///
    /// A retired shard has no row, so a later fence treats it as a cold start.
    async fn retire(&self, grid: GridId, shard: CellId) -> Result<(), FenceError>;
}

/// An in-process fence store, keyed by shard cell.
///
/// Used as the default so fencing/splitting is testable with no external
/// service. It is not durable across process death (that is FDB's job).
#[derive(Debug, Default)]
pub struct MemFenceStore {
    map: Mutex<HashMap<(GridId, CellId), FenceRow>>,
}

impl MemFenceStore {
    /// A new, empty in-process store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl FenceStore for MemFenceStore {
    async fn read(&self, grid: GridId, shard: CellId) -> Result<Option<FenceRow>, FenceError> {
        Ok(self
            .map
            .lock()
            .expect("mem fence lock")
            .get(&(grid, shard))
            .copied())
    }

    async fn fence(
        &self,
        grid: GridId,
        shard: CellId,
        expected: Option<&FenceRow>,
        new: &FenceRow,
    ) -> Result<FenceOutcome, FenceError> {
        let mut map = self.map.lock().expect("mem fence lock");
        let current = map.get(&(grid, shard)).copied();
        if current != expected.copied() {
            return Ok(FenceOutcome::Conflict { current });
        }
        map.insert((grid, shard), *new);
        Ok(FenceOutcome::Fenced)
    }

    async fn activate_shards(
        &self,
        grid: GridId,
        owner: u64,
        shards: &[ShardActivation],
    ) -> Result<ActivationOutcome, FenceError> {
        validate_activation_set(shards)?;
        let mut map = self.map.lock().expect("mem fence lock");
        for request in shards {
            let current = map.get(&(grid, request.shard)).copied();
            if current != request.expected {
                return Ok(ActivationOutcome::Conflict {
                    shard: request.shard,
                    current,
                });
            }
        }
        let rows = shards
            .iter()
            .map(|request| {
                let row = FenceRow {
                    owner,
                    epoch: Epoch::new(request.expected.map_or(0, |row| row.epoch.0) + 1),
                    status: FenceStatus::Active,
                };
                map.insert((grid, request.shard), row);
                (request.shard, row)
            })
            .collect();
        Ok(ActivationOutcome::Activated { rows })
    }

    async fn begin_split(
        &self,
        grid: GridId,
        parent: CellId,
        parent_expected: &FenceRow,
        children: &[(CellId, FenceRow)],
    ) -> Result<FenceOutcome, FenceError> {
        let mut map = self.map.lock().expect("mem fence lock");
        let current = map.get(&(grid, parent)).copied();
        if current != Some(*parent_expected) {
            return Ok(FenceOutcome::Conflict { current });
        }
        // Mark the parent Splitting (same owner/epoch, new status).
        map.insert(
            (grid, parent),
            FenceRow {
                status: FenceStatus::Splitting,
                ..*parent_expected
            },
        );
        for (child, row) in children {
            map.insert((grid, *child), *row);
        }
        Ok(FenceOutcome::Fenced)
    }

    async fn retire(&self, grid: GridId, shard: CellId) -> Result<(), FenceError> {
        self.map
            .lock()
            .expect("mem fence lock")
            .remove(&(grid, shard));
        Ok(())
    }
}

/// Validate the canonical ordering required to make a shard-set activation
/// unambiguous.  A parent and child overlap even if their raw Morton values
/// sort differently, so check prefix relation as well as strict ordering.
pub(crate) fn validate_activation_set(shards: &[ShardActivation]) -> Result<(), FenceError> {
    if shards.is_empty() {
        return Err(FenceError::InvalidActivation("empty shard set".into()));
    }
    for pair in shards.windows(2) {
        let previous = pair[0].shard;
        let next = pair[1].shard;
        if previous.to_bits() >= next.to_bits() {
            return Err(FenceError::InvalidActivation(
                "shards must be strictly sorted by CellId bits".into(),
            ));
        }
    }
    for (index, request) in shards.iter().enumerate() {
        if shards[index + 1..].iter().any(|other| {
            request.shard.is_prefix_of(other.shard) || other.shard.is_prefix_of(request.shard)
        }) {
            return Err(FenceError::InvalidActivation(
                "shards must not have overlapping subtrees".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(owner: u64, epoch: u64) -> FenceRow {
        FenceRow {
            owner,
            epoch: Epoch::new(epoch),
            status: FenceStatus::Active,
        }
    }

    #[tokio::test]
    async fn fence_from_absent_fences() {
        let store = MemFenceStore::new();
        let new = row(1, 5);
        assert_eq!(
            store
                .fence(GridId::ROOT, CellId::ROOT, None, &new)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        assert_eq!(
            store.read(GridId::ROOT, CellId::ROOT).await.unwrap(),
            Some(new)
        );
    }

    #[tokio::test]
    async fn fence_conflicts_on_mismatch() {
        let store = MemFenceStore::new();
        store
            .fence(GridId::ROOT, CellId::ROOT, None, &row(1, 5))
            .await
            .unwrap();
        // Wrong expected epoch -> conflict, row unchanged.
        let conflict = store
            .fence(GridId::ROOT, CellId::ROOT, Some(&row(1, 4)), &row(2, 6))
            .await
            .unwrap();
        assert_eq!(
            conflict,
            FenceOutcome::Conflict {
                current: Some(row(1, 5))
            }
        );
        assert_eq!(
            store.read(GridId::ROOT, CellId::ROOT).await.unwrap(),
            Some(row(1, 5))
        );
        // Correct expected -> fences.
        assert_eq!(
            store
                .fence(GridId::ROOT, CellId::ROOT, Some(&row(1, 5)), &row(2, 6))
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        assert_eq!(
            store.read(GridId::ROOT, CellId::ROOT).await.unwrap(),
            Some(row(2, 6))
        );
    }

    #[tokio::test]
    async fn begin_split_marks_parent_and_writes_children() {
        let store = MemFenceStore::new();
        let parent_row = row(0, 3);
        store
            .fence(GridId::ROOT, CellId::ROOT, None, &parent_row)
            .await
            .unwrap();

        let children = CellId::ROOT.children();
        let child_rows: Vec<(CellId, FenceRow)> =
            children.iter().map(|&c| (c, row(1, 4))).collect();

        assert_eq!(
            store
                .begin_split(GridId::ROOT, CellId::ROOT, &parent_row, &child_rows)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );

        // Parent is now Splitting.
        let parent = store
            .read(GridId::ROOT, CellId::ROOT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent.status, FenceStatus::Splitting);
        assert_eq!(parent.epoch, Epoch::new(3));

        // Children rows present.
        for (child, expected) in &child_rows {
            assert_eq!(
                store.read(GridId::ROOT, *child).await.unwrap(),
                Some(*expected)
            );
        }
    }

    #[tokio::test]
    async fn begin_split_conflicts_on_stale_parent() {
        let store = MemFenceStore::new();
        store
            .fence(GridId::ROOT, CellId::ROOT, None, &row(0, 3))
            .await
            .unwrap();
        // A stale expected row (epoch 2) must not split.
        let conflict = store
            .begin_split(GridId::ROOT, CellId::ROOT, &row(0, 2), &[])
            .await
            .unwrap();
        assert_eq!(
            conflict,
            FenceOutcome::Conflict {
                current: Some(row(0, 3))
            }
        );
        // Parent unchanged.
        assert_eq!(
            store.read(GridId::ROOT, CellId::ROOT).await.unwrap(),
            Some(row(0, 3))
        );
    }

    #[tokio::test]
    async fn retire_removes_row() {
        let store = MemFenceStore::new();
        store
            .fence(GridId::ROOT, CellId::ROOT, None, &row(1, 5))
            .await
            .unwrap();
        store.retire(GridId::ROOT, CellId::ROOT).await.unwrap();
        assert_eq!(store.read(GridId::ROOT, CellId::ROOT).await.unwrap(), None);
    }

    #[tokio::test]
    async fn activation_is_all_or_nothing() {
        let store = MemFenceStore::new();
        let children = CellId::ROOT.children();
        let mut shards = [children[0], children[1]];
        shards.sort_by_key(|cell| cell.to_bits());
        let requests = shards.map(|shard| ShardActivation {
            shard,
            expected: None,
        });
        let ActivationOutcome::Activated { rows } = store
            .activate_shards(GridId::ROOT, 8, &requests)
            .await
            .unwrap()
        else {
            panic!("bootstrap activation must succeed");
        };
        assert!(rows
            .iter()
            .all(|(_, row)| row.owner == 8 && row.epoch == Epoch::new(1)));

        let stale = [
            ShardActivation {
                shard: rows[0].0,
                expected: Some(rows[0].1),
            },
            ShardActivation {
                shard: rows[1].0,
                expected: None,
            },
        ];
        assert!(matches!(
            store.activate_shards(GridId::ROOT, 9, &stale).await.unwrap(),
            ActivationOutcome::Conflict { shard, .. } if shard == rows[1].0
        ));
        assert_eq!(
            store.read(GridId::ROOT, rows[0].0).await.unwrap(),
            Some(rows[0].1)
        );
    }
}
