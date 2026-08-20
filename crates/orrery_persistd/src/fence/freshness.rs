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
    /// The exact rows being watched.
    ///
    /// Mutable because a live shard handover (D26 rule 3) moves a row *out*
    /// of this node's ownership on purpose. Left immutable, the outgoing
    /// owner would watch a row it had itself handed away, see a permanent
    /// mismatch on the next poll, and make **every** bulk acknowledgement on
    /// every one of its remaining shards provisional forever — a correct
    /// handover taking the node's whole write path down with it. See
    /// [`Self::forget`] and [`Self::adopt`].
    rows: RwLock<Vec<(CellId, FenceRow)>>,
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
            rows: RwLock::new(rows),
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

    /// Update the row expected for a shard this monitor already watches.
    ///
    /// Called when this node itself changes the row — which happens exactly
    /// once outside activation, at a live handover's step 1, where the status
    /// goes `Active → Draining{B}` under the same owner and epoch (D26 rule
    /// 3). Without this the monitor would see its own deliberate write as a
    /// mismatch and make **every** bulk acknowledgement on **every** shard of
    /// this node provisional for the length of the drain — a drain D26 calls
    /// invisible to gameplay, visibly degrading the write path.
    ///
    /// A no-op on a shard that is not watched: this never starts watching one,
    /// because what may be watched is an activation result.
    pub fn rewatch(&self, shard: CellId, row: FenceRow) {
        let mut rows = self.rows.write().expect("fence freshness lock poisoned");
        if let Some(watched) = rows.iter_mut().find(|(watched, _)| *watched == shard) {
            watched.1 = row;
        }
    }

    /// Stop watching `shard`: this node no longer owns it (D26 rule 3 step 6).
    ///
    /// Called by the outgoing owner *after* its handover CAS commits. A
    /// mismatch already recorded is deliberately not cleared — a mismatch
    /// means some row moved under this node without its consent, and forgetting
    /// one shard is not evidence about the others. What this prevents is the
    /// *next* poll finding a row that is missing by design.
    pub fn forget(&self, shard: CellId) {
        self.rows
            .write()
            .expect("fence freshness lock poisoned")
            .retain(|(watched, _)| *watched != shard);
    }

    /// Start watching `shard` at `row`: this node has just adopted it
    /// (D26 rule 3 step 7).
    ///
    /// Refused, and reported as such, if `shard` overlaps a row already
    /// watched — the same non-overlap precondition [`Self::start`] enforces,
    /// for the same reason: `assess` picks a row by prefix, so an overlapping
    /// pair makes cell-to-shard admission ambiguous.
    pub fn adopt(&self, shard: CellId, row: FenceRow) -> Result<(), FenceFreshnessError> {
        let mut rows = self.rows.write().expect("fence freshness lock poisoned");
        if rows
            .iter()
            .any(|(watched, _)| watched.is_prefix_of(shard) || shard.is_prefix_of(*watched))
        {
            return Err(FenceFreshnessError::OverlappingShards);
        }
        rows.push((shard, row));
        rows.sort_by_key(|(shard, _)| *shard);
        Ok(())
    }

    /// Stop polling and make all future bulk acknowledgements provisional.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        let mut state = self.state.write().expect("fence freshness lock poisoned");
        state.last_confirmation = None;
        state.mismatch = true;
    }

    /// Confirm every watched row, in one batched read.
    ///
    /// This used to be a `for` loop over `FenceStore::read`, and on the
    /// durable tier each of those is a whole transaction — so confirming a
    /// 128-shard node once a second was 128 transactions a second, each with
    /// its own read version, over rows that are adjacent in the keyspace.
    /// `read_many` is one range read (docs/08-persistence.md §2.2.7 made the
    /// same change on the intent path's fence, which watches the same rows).
    ///
    /// The decision is unchanged. The rows come back positionally, they are
    /// compared against the same expectations in the same order, and the two
    /// early exits are preserved exactly: any mismatch marks the monitor
    /// mismatched and does not refresh the confirmation, and a store error
    /// refreshes nothing at all. The only difference is that a mismatch no
    /// longer *stops* the reads — they have already happened — which costs
    /// nothing, because a mismatched monitor is a terminal state the node does
    /// not poll its way out of.
    async fn poll_once(&self, store: &dyn FenceStore) {
        // Snapshot the watch set rather than holding the lock across the
        // store round trip: `forget` and `adopt` are called from a handover
        // that must not block on FoundationDB, and a poll that raced one
        // simply confirms the set as it was a moment ago.
        let watched = self
            .rows
            .read()
            .expect("fence freshness lock poisoned")
            .clone();
        if watched.is_empty() {
            // A node that owns nothing has nothing to confirm. Refreshing the
            // confirmation here would be a claim about rows that do not
            // exist, so the window simply runs down and acks go provisional —
            // which is correct, because there is no shard left to ack for.
            return;
        }
        let shards: Vec<CellId> = watched.iter().map(|&(shard, _)| shard).collect();
        let Ok(actual) = store.read_many(self.grid, &shards).await else {
            return;
        };
        if actual.len() != watched.len() {
            return;
        }
        for (&(_, expected), got) in watched.iter().zip(actual) {
            if got != Some(expected) {
                self.state
                    .write()
                    .expect("fence freshness lock poisoned")
                    .mismatch = true;
                return;
            }
        }
        let mut state = self.state.write().expect("fence freshness lock poisoned");
        state.last_confirmation = Some(Instant::now());
        state.mismatch = false;
    }

    fn owns_cell(&self, cell: CellId) -> bool {
        self.rows
            .read()
            .expect("fence freshness lock poisoned")
            .iter()
            .any(|(shard, _)| shard.is_prefix_of(cell))
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

    /// A store that counts how many times the monitor asked it for rows, and
    /// whether it asked one at a time or in a batch.
    ///
    /// Both counters exist because only their *ratio* is the property under
    /// test: a poll that reads its rows individually is what this change
    /// removed, and a poll that batches them is what replaced it. Counting
    /// only rows would pass either way.
    #[derive(Default)]
    struct CountingFenceStore {
        inner: MemFenceStore,
        single_reads: std::sync::atomic::AtomicUsize,
        batched_calls: std::sync::atomic::AtomicUsize,
        rows_returned: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl FenceStore for CountingFenceStore {
        async fn read(
            &self,
            grid: GridId,
            shard: CellId,
        ) -> Result<Option<crate::fence::FenceRow>, crate::fence::FenceError> {
            self.single_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.read(grid, shard).await
        }
        async fn read_many(
            &self,
            grid: GridId,
            shards: &[CellId],
        ) -> Result<Vec<Option<crate::fence::FenceRow>>, crate::fence::FenceError> {
            self.batched_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.rows_returned
                .fetch_add(shards.len(), std::sync::atomic::Ordering::SeqCst);
            // Deliberately *not* delegating to `inner.read_many`: that would
            // take the trait default, which loops over `read` and would make
            // `single_reads` count this batch's rows too.
            let mut out = Vec::with_capacity(shards.len());
            for &shard in shards {
                out.push(self.inner.read(grid, shard).await?);
            }
            Ok(out)
        }
        async fn fence(
            &self,
            grid: GridId,
            shard: CellId,
            expected: Option<&crate::fence::FenceRow>,
            new: &crate::fence::FenceRow,
        ) -> Result<FenceOutcome, crate::fence::FenceError> {
            self.inner.fence(grid, shard, expected, new).await
        }
        async fn activate_shards(
            &self,
            grid: GridId,
            owner: u64,
            shards: &[crate::fence::ShardActivation],
        ) -> Result<crate::fence::ActivationOutcome, crate::fence::FenceError> {
            self.inner.activate_shards(grid, owner, shards).await
        }
        async fn begin_split(
            &self,
            grid: GridId,
            parent: CellId,
            parent_expected: &crate::fence::FenceRow,
            children: &[(CellId, crate::fence::FenceRow)],
        ) -> Result<FenceOutcome, crate::fence::FenceError> {
            self.inner
                .begin_split(grid, parent, parent_expected, children)
                .await
        }
        async fn retire(
            &self,
            grid: GridId,
            shard: CellId,
        ) -> Result<(), crate::fence::FenceError> {
            self.inner.retire(grid, shard).await
        }
    }

    /// The monitor confirms its whole shard set with one batched read, not one
    /// read per shard.
    ///
    /// On the durable tier a `FenceStore::read` is a whole transaction, so the
    /// per-shard loop this replaced was one transaction per shard per second.
    /// Counting rows alone would not notice the difference; counting *calls*
    /// is the property.
    #[tokio::test]
    async fn a_poll_reads_its_shard_set_in_one_batch() {
        const SHARDS: usize = 8;
        let store = Arc::new(CountingFenceStore::default());
        let shards: Vec<CellId> = CellId::ROOT.children().into_iter().take(SHARDS).collect();
        let mut rows = Vec::new();
        for &shard in &shards {
            assert!(matches!(
                store
                    .fence(GridId::ROOT, shard, None, &row())
                    .await
                    .unwrap(),
                FenceOutcome::Fenced
            ));
            rows.push((shard, row()));
        }
        // The rows the monitor starts from are read through `read`, so the
        // window that matters starts after it is running.
        let monitor = FenceFreshnessMonitor::start(
            Arc::clone(&store) as Arc<dyn FenceStore>,
            GridId::ROOT,
            rows,
            FenceFreshnessConfig {
                poll_interval: Duration::from_millis(5),
                max_staleness: Duration::from_secs(3),
            },
        )
        .unwrap();
        let singles_before = store.single_reads.load(std::sync::atomic::Ordering::SeqCst);
        let batches_before = store
            .batched_calls
            .load(std::sync::atomic::Ordering::SeqCst);
        // Asked about a cell the monitor actually owns: its rows are ROOT's
        // children here, and `owns_cell` is a prefix test, so ROOT itself is
        // not one of them.
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if monitor.assess(GridId::ROOT, shards[0]) == BulkAckDisposition::Durable {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("monitor confirms its rows");
        tokio::time::sleep(Duration::from_millis(40)).await;
        let batches = store
            .batched_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            - batches_before;
        let rows_seen = store
            .rows_returned
            .load(std::sync::atomic::Ordering::SeqCst);
        let singles = store.single_reads.load(std::sync::atomic::Ordering::SeqCst) - singles_before;
        assert!(batches >= 1, "the monitor must have polled at least once");
        assert_eq!(
            rows_seen,
            batches * SHARDS,
            "every poll must ask for the whole shard set at once",
        );
        // The counting store's `read_many` reaches `inner.read`, not `self.read`,
        // so a batched poll leaves this at zero. A per-shard loop would not.
        assert_eq!(
            singles, 0,
            "a poll must not read its shards one at a time: {singles} single reads across \
             {batches} polls of {SHARDS} shards",
        );
    }

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
