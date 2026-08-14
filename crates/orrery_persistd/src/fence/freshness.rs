//! Bounded-staleness ownership monitoring for bulk gateway acknowledgements.
//!
//! A successful journal append is only a durable client acknowledgement while
//! this process still owns the relevant `actor/{grid}/{shard}` fence row.  The
//! monitor continuously re-reads the exact rows acquired during activation.
//! It fails closed: a different row is stale immediately, and an unavailable
//! durable store becomes stale after the configured confirmation window.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use orrery_protocol::{CellId, GridId};

use crate::gateway::{BulkAckAdmission, BulkAckDisposition};

use super::{FenceRow, FenceStore};

/// Timing policy for [`FenceFreshnessMonitor`].
#[derive(Debug, Clone, Copy)]
pub struct FenceFreshnessConfig {
    /// How often to read the active fence rows from the durable store.
    pub poll_interval: Duration,
    /// Maximum age of the last successful full confirmation.
    pub max_staleness: Duration,
}

impl Default for FenceFreshnessConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            max_staleness: Duration::from_secs(3),
        }
    }
}

/// Why a freshness monitor could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceFreshnessError {
    /// A zero interval cannot drive a monitor safely.
    ZeroPollInterval,
    /// A zero confirmation window would make every acknowledgement stale.
    ZeroMaxStaleness,
    /// There is no shard whose ownership can be assessed.
    EmptyShardSet,
    /// Two expected shards overlap, making cell-to-shard acknowledgement
    /// admission ambiguous.
    OverlappingShards,
    /// The durable fence store could not supply the active row snapshot used
    /// to start monitoring.
    FenceRead(String),
    /// A local actor has no matching active row owned by this runtime.
    InactiveShard(CellId),
}

impl core::fmt::Display for FenceFreshnessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroPollInterval => write!(f, "fence freshness poll interval must be non-zero"),
            Self::ZeroMaxStaleness => write!(f, "fence freshness window must be non-zero"),
            Self::EmptyShardSet => write!(f, "fence freshness monitor requires at least one shard"),
            Self::OverlappingShards => write!(f, "fence freshness shards must not overlap"),
            Self::FenceRead(error) => write!(f, "read active fence rows: {error}"),
            Self::InactiveShard(shard) => write!(f, "shard {shard:?} is not actively owned"),
        }
    }
}

impl core::error::Error for FenceFreshnessError {}

struct FreshnessState {
    /// `None` only after shutdown.  At construction this is deliberately
    /// initialized to now: installation has a bounded grace window to make
    /// its first durable confirmation, but never remains fresh indefinitely.
    last_confirmation: Option<Instant>,
    /// An observed row mismatch is an immediate split-brain signal.  Store
    /// errors are different: they consume the bounded grace window instead.
    mismatch: bool,
}

/// A running bounded-staleness monitor over one grid's active fence rows.
///
/// The expected rows are an activation result, not merely an owner id: an
/// epoch advance, status change, disappearance, or owner change all make the
/// local gateway provisional.  It implements [`BulkAckAdmission`] directly,
/// so callers inject an `Arc<Self>` into [`crate::gateway::GatewayConfig`].
pub struct FenceFreshnessMonitor {
    grid: GridId,
    rows: Vec<(CellId, FenceRow)>,
    max_staleness: Duration,
    state: RwLock<FreshnessState>,
    shutdown: watch::Sender<bool>,
}

impl FenceFreshnessMonitor {
    /// Start polling `store` for the exact active `rows` owned in `grid`.
    ///
    /// The first check is performed immediately in the task.  Until it
    /// succeeds the monitor has only `max_staleness` of provisional startup
    /// grace; a mismatch observed by that first check fails closed at once.
    pub fn start(
        store: Arc<dyn FenceStore>,
        grid: GridId,
        mut rows: Vec<(CellId, FenceRow)>,
        config: FenceFreshnessConfig,
    ) -> Result<Arc<Self>, FenceFreshnessError> {
        if config.poll_interval.is_zero() {
            return Err(FenceFreshnessError::ZeroPollInterval);
        }
        if config.max_staleness.is_zero() {
            return Err(FenceFreshnessError::ZeroMaxStaleness);
        }
        if rows.is_empty() {
            return Err(FenceFreshnessError::EmptyShardSet);
        }
        rows.sort_by_key(|(shard, _)| *shard);
        if rows
            .windows(2)
            .any(|pair| pair[0].0.is_prefix_of(pair[1].0) || pair[1].0.is_prefix_of(pair[0].0))
        {
            return Err(FenceFreshnessError::OverlappingShards);
        }

        let (shutdown, mut stopped) = watch::channel(false);
        let monitor = Arc::new(Self {
            grid,
            rows,
            max_staleness: config.max_staleness,
            state: RwLock::new(FreshnessState {
                last_confirmation: Some(Instant::now()),
                mismatch: false,
            }),
            shutdown,
        });
        let task_monitor = Arc::clone(&monitor);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(config.poll_interval);
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    _ = interval.tick() => task_monitor.poll_once(store.as_ref()).await,
                }
            }
        });
        Ok(monitor)
    }

    /// Stop polling and make all future bulk acknowledgements provisional.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        let mut state = self.state.write().expect("fence freshness lock poisoned");
        state.last_confirmation = None;
        state.mismatch = true;
    }

    async fn poll_once(&self, store: &dyn FenceStore) {
        for &(shard, expected) in &self.rows {
            match store.read(self.grid, shard).await {
                Ok(Some(actual)) if actual == expected => {}
                Ok(_) => {
                    self.state
                        .write()
                        .expect("fence freshness lock poisoned")
                        .mismatch = true;
                    return;
                }
                Err(_) => return,
            }
        }
        let mut state = self.state.write().expect("fence freshness lock poisoned");
        state.last_confirmation = Some(Instant::now());
        state.mismatch = false;
    }

    fn owns_cell(&self, cell: CellId) -> bool {
        self.rows.iter().any(|(shard, _)| shard.is_prefix_of(cell))
    }
}

impl BulkAckAdmission for FenceFreshnessMonitor {
    fn assess(&self, grid: GridId, cell: CellId) -> BulkAckDisposition {
        if grid != self.grid || !self.owns_cell(cell) {
            return BulkAckDisposition::Provisional;
        }
        let state = self.state.read().expect("fence freshness lock poisoned");
        if state.mismatch
            || state
                .last_confirmation
                .is_none_or(|then| then.elapsed() > self.max_staleness)
        {
            BulkAckDisposition::Provisional
        } else {
            BulkAckDisposition::Durable
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use orrery_protocol::{CellId, Epoch, GridId};

    use crate::fence::{FenceOutcome, FenceStatus, FenceStore, MemFenceStore};
    use crate::gateway::{BulkAckAdmission, BulkAckDisposition};

    use super::{FenceFreshnessConfig, FenceFreshnessMonitor};

    fn row() -> crate::fence::FenceRow {
        crate::fence::FenceRow {
            owner: 7,
            epoch: Epoch::new(3),
            status: FenceStatus::Active,
        }
    }

    async fn wait_for(monitor: &FenceFreshnessMonitor, expected: BulkAckDisposition) {
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if monitor.assess(GridId::ROOT, CellId::ROOT) == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("monitor reached expected disposition");
    }

    #[tokio::test]
    async fn mismatch_is_immediately_provisional_and_matching_row_recovers() {
        let store = Arc::new(MemFenceStore::new());
        let expected = row();
        assert_eq!(
            store
                .fence(GridId::ROOT, CellId::ROOT, None, &expected)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        let monitor = FenceFreshnessMonitor::start(
            store.clone(),
            GridId::ROOT,
            vec![(CellId::ROOT, expected)],
            FenceFreshnessConfig {
                poll_interval: Duration::from_millis(5),
                max_staleness: Duration::from_millis(80),
            },
        )
        .unwrap();
        wait_for(&monitor, BulkAckDisposition::Durable).await;

        let changed = crate::fence::FenceRow {
            owner: 8,
            epoch: Epoch::new(4),
            status: FenceStatus::Active,
        };
        assert_eq!(
            store
                .fence(GridId::ROOT, CellId::ROOT, Some(&expected), &changed)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        wait_for(&monitor, BulkAckDisposition::Provisional).await;

        assert_eq!(
            store
                .fence(GridId::ROOT, CellId::ROOT, Some(&changed), &expected)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        );
        wait_for(&monitor, BulkAckDisposition::Durable).await;
        monitor.shutdown();
    }

    #[tokio::test]
    async fn lack_of_confirmation_expires_startup_grace() {
        let store = Arc::new(MemFenceStore::new());
        let monitor = FenceFreshnessMonitor::start(
            store,
            GridId::ROOT,
            vec![(CellId::ROOT, row())],
            FenceFreshnessConfig {
                poll_interval: Duration::from_secs(10),
                max_staleness: Duration::from_millis(15),
            },
        )
        .unwrap();
        assert_eq!(
            monitor.assess(GridId::ROOT, CellId::ROOT),
            BulkAckDisposition::Durable
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            monitor.assess(GridId::ROOT, CellId::ROOT),
            BulkAckDisposition::Provisional
        );
        monitor.shutdown();
    }
}
