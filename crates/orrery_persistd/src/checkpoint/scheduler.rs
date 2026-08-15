//! The checkpoint scheduler (docs/08-persistence.md §8, D16).
//!
//! Cell actors checkpoint **copy-on-update** on a **20 s, jittered per shard**
//! cadence (spreads FDB write load; prevents cluster-wide checkpoint
//! synchronization), and **immediately on cell quiesce** — when a cell's last
//! player leaves (coordinator signal), the actor checkpoints and may be parked.
//!
//! This module owns that cadence. A [`CheckpointScheduler`] runs one timer per
//! shard cell, each jittered independently, and fires a checkpoint to the
//! runtime's [`CheckpointStore`]. A [`QuiesceSignal`] lets the coordinator
//! request an immediate quiesce-flush for a cell.
//!
//! The scheduler re-reads its runtime's shard set every loop iteration and
//! reconciles the timer vector against it, so shards that appear after the
//! scheduler starts (e.g. post-split children) are picked up and retired
//! shards are dropped.

use std::sync::Arc;
use std::time::Duration;

use orrery_protocol::CellId;

use crate::checkpoint::{CheckpointError, CheckpointStore};
use crate::runtime::CellRuntime;

/// Configuration for the checkpoint scheduler (D16: 20 s jittered).
#[derive(Debug, Clone)]
pub struct CheckpointConfig {
    /// The base checkpoint interval. Default 20 s (D16).
    pub interval: Duration,
    /// The maximum jitter added to (or subtracted from) the interval per shard,
    /// so shards do not checkpoint in lockstep. Default ±5 s.
    pub jitter: Duration,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(20),
            jitter: Duration::from_secs(5),
        }
    }
}

/// A request to quiesce-flush a cell (coordinator signal, §8).
///
/// When a cell's last player leaves, the coordinator asks the scheduler to
/// checkpoint that cell immediately, so hot memory is bounded by *populated*
/// cells, not universe size.
#[derive(Debug, Clone)]
pub struct QuiesceSignal {
    tx: tokio::sync::mpsc::Sender<CellId>,
}

impl QuiesceSignal {
    /// Request an immediate quiesce-flush of `cell`.
    ///
    /// Returns `false` if the scheduler has shut down.
    pub async fn quiesce(&self, cell: CellId) -> bool {
        self.tx.send(cell).await.is_ok()
    }
}

/// A running checkpoint scheduler.
///
/// Drives one jittered timer per shard cell against the runtime's
/// [`CheckpointStore`], plus an immediate quiesce-flush channel.
pub struct CheckpointScheduler {
    shutdown: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
    quiesce: QuiesceSignal,
}

impl CheckpointScheduler {
    /// The quiesce-flush signal, for the coordinator to request immediate
    /// checkpoints of drained cells.
    #[must_use]
    pub fn quiesce_signal(&self) -> QuiesceSignal {
        self.quiesce.clone()
    }

    /// Stop the scheduler, awaiting its exit.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        let _ = self.join.await;
    }
}

/// Spawn a checkpoint scheduler over `runtime`'s shard actors, writing to
/// `store` on each jittered interval.
///
/// One timer task per shard cell, each with an independent jitter (seeded from
/// the shard's `CellId::to_bits()`), so checkpoints spread across the interval
/// rather than synchronizing cluster-wide (D16). A quiesce request for a cell
/// triggers an immediate checkpoint.
///
/// The scheduler re-reads the runtime's shard set every loop iteration and
/// reconciles its timer vector, so shards created after spawn (post-split
/// children) are armed and retired shards are dropped within one interval.
pub fn spawn_checkpoint_scheduler(
    runtime: Arc<tokio::sync::Mutex<CellRuntime>>,
    store: Arc<dyn CheckpointStore>,
    config: &CheckpointConfig,
) -> CheckpointScheduler {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let (quiesce_tx, mut quiesce_rx) = tokio::sync::mpsc::channel(64);

    let interval = config.interval;
    let jitter = config.jitter;
    let shutdown_task = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        // Timer vector: each entry is a shard and its next checkpoint deadline.
        let mut timers: Vec<(CellId, tokio::time::Instant)> = Vec::new();

        loop {
            // Re-read the shard set each iteration to pick up splits.
            let shards: Vec<CellId> = {
                let rt = runtime.lock().await;
                rt.shards().copied().collect()
            };

            // Reconcile: add timers for shards not yet tracked, remove timers
            // for shards that have been retired.
            let now = tokio::time::Instant::now();

            // Remove timers for shards that no longer exist.
            timers.retain(|(shard, _)| shards.contains(shard));

            // Add timers for new shards that appeared after the last loop.
            for &shard in &shards {
                if !timers.iter().any(|(s, _)| *s == shard) {
                    let delay = jittered(interval, jitter, shard);
                    timers.push((shard, now + delay));
                }
            }

            // Find the earliest timer.
            let next = timers
                .iter()
                .map(|(_, t)| *t)
                .min()
                .unwrap_or(now + interval);

            let sleep = tokio::time::sleep_until(next);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                _ = shutdown_task.notified() => break,
                Some(cell) = quiesce_rx.recv() => {
                    if let Err(e) = checkpoint_cell(&runtime, &store, cell).await {
                        tracing::warn!(shard = %cell, error = %e, "quiesce checkpoint failed");
                    }
                    continue;
                }
            }

            // Fire any timers that are due.
            let now = tokio::time::Instant::now();
            for (shard, due) in timers.iter_mut() {
                if *due <= now {
                    if let Err(e) = checkpoint_cell(&runtime, &store, *shard).await {
                        tracing::warn!(shard = %shard, error = %e, "scheduled checkpoint failed");
                    }
                    *due = now + jittered(interval, jitter, *shard);
                }
            }
        }
    });

    CheckpointScheduler {
        shutdown,
        join,
        quiesce: QuiesceSignal { tx: quiesce_tx },
    }
}

/// Spawn a checkpoint scheduler for an unlocked `Arc<CellRuntime>`.
pub fn spawn_checkpoint_scheduler_direct(
    runtime: Arc<CellRuntime>,
    store: Arc<dyn CheckpointStore>,
    config: &CheckpointConfig,
) -> CheckpointScheduler {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let (quiesce_tx, mut quiesce_rx) = tokio::sync::mpsc::channel(64);

    let interval = config.interval;
    let jitter = config.jitter;
    let shutdown_task = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        let mut timers: Vec<(CellId, tokio::time::Instant)> = Vec::new();

        loop {
            let shards: Vec<CellId> = runtime.shards().copied().collect();

            let now = tokio::time::Instant::now();
            timers.retain(|(shard, _)| shards.contains(shard));

            for &shard in &shards {
                if !timers.iter().any(|(s, _)| *s == shard) {
                    let delay = jittered(interval, jitter, shard);
                    timers.push((shard, now + delay));
                }
            }

            let next = timers
                .iter()
                .map(|(_, t)| *t)
                .min()
                .unwrap_or(now + interval);

            let sleep = tokio::time::sleep_until(next);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                _ = shutdown_task.notified() => break,
                Some(cell) = quiesce_rx.recv() => {
                    if let Ok(target) = runtime.checkpoint_target(cell) {
                        if let Err(e) = target.checkpoint(store.as_ref()).await {
                            tracing::warn!(shard = %cell, error = %e, "quiesce checkpoint failed");
                        }
                    }
                    continue;
                }
            }

            let now = tokio::time::Instant::now();
            for (shard, due) in timers.iter_mut() {
                if *due <= now {
                    if let Ok(target) = runtime.checkpoint_target(*shard) {
                        if let Err(e) = target.checkpoint(store.as_ref()).await {
                            tracing::warn!(shard = %shard, error = %e, "scheduled checkpoint failed");
                        }
                    }
                    *due = now + jittered(interval, jitter, *shard);
                }
            }
        }
    });

    CheckpointScheduler {
        shutdown,
        join,
        quiesce: QuiesceSignal { tx: quiesce_tx },
    }
}

/// Checkpoint a single shard cell via the runtime.
async fn checkpoint_cell(
    runtime: &Arc<tokio::sync::Mutex<CellRuntime>>,
    store: &Arc<dyn CheckpointStore>,
    shard: CellId,
) -> Result<(), CheckpointError> {
    // The runtime mutex protects the actor topology, not checkpoint I/O.
    // Resolve a cloneable target under it, then release it before the actor
    // snapshot, durable-store transaction, and tombstone-pruning awaits. A
    // root checkpoint can take seconds under load; holding this mutex would
    // block every Router::apply from resolving its actor for that whole time.
    let target = {
        let rt = runtime.lock().await;
        rt.checkpoint_target(shard)?
    };
    target.checkpoint(store.as_ref()).await
}

/// A jittered delay in `[interval - jitter, interval + jitter]`, seeded from
/// `shard`'s [`CellId::to_bits()`] so every shard has a stable, unique offset
/// within the interval.
///
/// Uses a simple LCG seeded by the shard cell's raw bits. The jitter is a
/// SIGNED offset so the period spans [interval - jitter, interval + jitter],
/// spreading checkpoints evenly around the base cadence rather than delaying
/// every shard by at least zero.
fn jittered(interval: Duration, jitter: Duration, shard: CellId) -> Duration {
    // A cheap deterministic LCG seeded from the shard cell id, so every shard
    // gets a stable jitter that is (almost certainly) unique to it. Not
    // security-relevant.
    let seed = shard.to_bits();
    // LCG constants from Numerical Recipes (Park-Miller).
    let state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let frac = (state >> 33) as f64 / (1u64 << 31) as f64;
    let signed_secs = (frac * 2.0 - 1.0) * jitter.as_secs_f64();
    let total_secs = interval.as_secs_f64() + signed_secs;
    Duration::from_secs_f64(total_secs.max(0.0))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use orrery_protocol::{CellId, GridId, NodeId};

    use crate::checkpoint::{CheckpointData, CheckpointError, CheckpointStore, MemCheckpointStore};
    use crate::cluster::Router;
    use crate::journal::{AdaptiveCommitMode, GroupCommitConfig};
    use crate::{CellRuntime, JournalConfig, RuntimeConfig};

    use super::*;

    fn test_runtime_config(dir: &std::path::Path) -> RuntimeConfig {
        RuntimeConfig {
            shards: vec![CellId::ROOT],
            grid: GridId::ROOT,
            journal: JournalConfig {
                dir: dir.to_path_buf(),
                commit: GroupCommitConfig {
                    mode: AdaptiveCommitMode::AlwaysBatch,
                    batch_window: Duration::from_millis(100),
                    batch_max_records: 100_000,
                    batch_max_bytes: 1 << 20,
                },
            },
            node_id: 0,
            epoch: orrery_protocol::Epoch::new(0),
            fence: std::sync::Arc::new(crate::fence::MemFenceStore::new()),
        }
    }

    /// The checkpoint store as the trait object `CellRuntime::open` takes.
    fn ckpt_store(store: &Arc<MemCheckpointStore>) -> Arc<dyn CheckpointStore> {
        store.clone()
    }

    /// A store that announces entry into the durable write, then waits until
    /// the test releases it. This models a slow FDB checkpoint transaction
    /// without making the regression depend on a live FDB cluster.
    #[derive(Default)]
    struct BlockingCheckpointStore {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl CheckpointStore for BlockingCheckpointStore {
        async fn checkpoint(&self, _data: &CheckpointData) -> Result<(), CheckpointError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(())
        }

        async fn load(
            &self,
            _shard: CellId,
            _grid: GridId,
        ) -> Result<Option<CheckpointData>, CheckpointError> {
            Ok(None)
        }

        async fn delete(&self, _shard: CellId, _grid: GridId) -> Result<(), CheckpointError> {
            Ok(())
        }
    }

    #[test]
    fn jitter_is_two_sided() {
        // The jitter must be a SIGNED offset: at least one draw must be
        // strictly below `interval` and at least one strictly above, and every
        // draw must be within [interval - jitter, interval + jitter].
        let interval = Duration::from_secs(20);
        let jitter = Duration::from_secs(5);
        let lower = interval - jitter;
        let upper = interval + jitter;

        let mut below = false;
        let mut above = false;

        // Use many distinct cell IDs across levels 1-3 to exercise the
        // per-shard seeding with diverse bit patterns.
        let shards: Vec<CellId> = {
            let mut cells = Vec::new();
            // Level 1: the 8 octants.
            for x in -1..=0 {
                for y in -1..=0 {
                    for z in -1..=0 {
                        if let Ok(c) = CellId::from_coords(glam::IVec3::new(x, y, z), 1) {
                            cells.push(c);
                        }
                    }
                }
            }
            // Level 2: a wider spread.
            for x in -2..2 {
                for y in -2..2 {
                    for z in -2..2 {
                        if let Ok(c) = CellId::from_coords(glam::IVec3::new(x, y, z), 2) {
                            cells.push(c);
                        }
                    }
                }
            }
            assert!(
                cells.len() >= 64,
                "need at least 64 distinct cell ids for jitter test, got {}",
                cells.len()
            );
            cells
        };

        // For each distinct CellId the jitter is deterministic, so we iterate
        // over all shards once and check the spread.
        for (i, &shard) in shards.iter().enumerate() {
            let d = jittered(interval, jitter, shard);
            assert!(
                d >= lower,
                "jittered delay {d:?} is below {lower:?} (seed shard={shard:?})"
            );
            assert!(
                d <= upper,
                "jittered delay {d:?} is above {upper:?} (seed shard={shard:?})"
            );
            if d < interval {
                below = true;
            }
            if d > interval {
                above = true;
            }
            if below && above {
                break;
            }
            if i >= 1000 {
                break;
            }
        }

        assert!(
            below,
            "no sample fell strictly below interval={interval:?}; jitter may be biased"
        );
        assert!(
            above,
            "no sample fell strictly above interval={interval:?}; jitter may be biased"
        );
    }

    #[tokio::test]
    async fn scheduler_checkpoints_on_cadence_and_quiesce() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemCheckpointStore::new());
        let runtime = Arc::new(tokio::sync::Mutex::new(
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store)).unwrap(),
        ));

        // Write an entity so there is something to checkpoint.
        {
            let rt = runtime.lock().await;
            use crate::payload_crc;
            use orrery_protocol::{JournalRecord, Lsn, PersistId, RecordKind, Tick};
            let rec = JournalRecord {
                lsn: Lsn::new(0, 0),
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(1),
                tick: Tick::new(1),
                epoch: orrery_protocol::Epoch::new(0),
                author: NodeId::from_bytes(&[0u8; 32]).expect("valid node id"),
                kind: RecordKind::Spawn,
                payload: bytes::Bytes::from_static(b"hp=100"),
                crc: payload_crc(b"hp=100"),
            };
            rt.apply(rec).await.unwrap();
        }

        // A fast cadence so the test is not slow.
        let scheduler = spawn_checkpoint_scheduler(
            Arc::clone(&runtime),
            store.clone(),
            &CheckpointConfig {
                interval: Duration::from_millis(50),
                jitter: Duration::from_millis(10),
            },
        );

        // Quiesce-flush: an immediate checkpoint on demand.
        scheduler.quiesce_signal().quiesce(CellId::ROOT).await;

        // Wait for the cadence timer to fire a checkpoint too.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if store
                .load(CellId::ROOT, GridId::ROOT)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "scheduler never checkpointed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The checkpoint reflects the entity.
        let ckpt = store
            .load(CellId::ROOT, GridId::ROOT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ckpt.entities.len(), 1);
        assert_eq!(
            ckpt.entities[&orrery_protocol::PersistId::new(1)]
                .components
                .as_ref(),
            b"hp=100"
        );

        scheduler.shutdown().await;
        // After shutdown the scheduler no longer holds the runtime Arc; take it
        // back so we can close the journal cleanly.
        let rt = Arc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("scheduler released the runtime"))
            .into_inner();
        rt.close().await.unwrap();
    }

    #[tokio::test]
    async fn checkpoint_store_wait_does_not_block_router_apply() {
        let dir = tempfile::tempdir().unwrap();
        let seed_store = Arc::new(MemCheckpointStore::new());
        let runtime = Arc::new(tokio::sync::Mutex::new(
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&seed_store)).unwrap(),
        ));
        let blocking_store = Arc::new(BlockingCheckpointStore::default());
        let scheduler = spawn_checkpoint_scheduler(
            Arc::clone(&runtime),
            blocking_store.clone(),
            &CheckpointConfig {
                interval: Duration::from_secs(60),
                jitter: Duration::ZERO,
            },
        );

        // Park the checkpoint task inside durable storage. By this point its
        // actor snapshot and watermark have already been captured.
        assert!(scheduler.quiesce_signal().quiesce(CellId::ROOT).await);
        tokio::time::timeout(Duration::from_secs(2), blocking_store.started.notified())
            .await
            .expect("checkpoint never entered durable store");

        // Actor routing must remain available while the checkpoint store is
        // stalled. Before checkpoint-target isolation this timed out waiting
        // for the scheduler's runtime mutex.
        use crate::payload_crc;
        use orrery_protocol::{JournalRecord, Lsn, PersistId, RecordKind, Tick};
        let rec = JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(99),
            tick: Tick::new(1),
            epoch: orrery_protocol::Epoch::new(0),
            author: NodeId::from_bytes(&[0u8; 32]).expect("valid node id"),
            kind: RecordKind::Spawn,
            payload: bytes::Bytes::from_static(b"concurrent"),
            crc: payload_crc(b"concurrent"),
        };
        let append =
            tokio::time::timeout(Duration::from_secs(2), Router::apply(runtime.as_ref(), rec))
                .await
                .expect("Router::apply blocked behind checkpoint storage")
                .expect("concurrent apply succeeds");
        let lsn = append.committed().await.expect("concurrent append durable");
        assert_eq!(lsn.segment, 0);

        blocking_store.release.notify_one();
        scheduler.shutdown().await;
        let rt = Arc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("scheduler released the runtime"))
            .into_inner();
        rt.close().await.unwrap();
    }

    #[tokio::test]
    async fn scheduler_arms_timers_for_shards_created_after_spawn() {
        // This test starts the scheduler with CellId::ROOT, then splits the
        // shard (creating 8 children), and asserts that each of the 8 children
        // receives a checkpoint within 2x the interval.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemCheckpointStore::new());
        let runtime = Arc::new(tokio::sync::Mutex::new(
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store)).unwrap(),
        ));

        // Write entities at child-cell positions so they partition cleanly
        // into the split children. The children of CellId::ROOT are level-1
        // cells like (-1,-1,-1); write an entity at one such cell.
        use crate::payload_crc;
        use orrery_protocol::{JournalRecord, Lsn, PersistId, RecordKind, Tick};
        {
            let rt = runtime.lock().await;
            // Use a level-1 child of ROOT: (-1, -1, -1) is a valid cell.
            let child = CellId::from_coords(glam::IVec3::new(-1, -1, -1), 1).unwrap();
            let rec = JournalRecord {
                lsn: Lsn::new(0, 0),
                cell: child,
                grid: GridId::ROOT,
                entity: PersistId::new(42),
                tick: Tick::new(1),
                epoch: orrery_protocol::Epoch::new(0),
                author: NodeId::from_bytes(&[0u8; 32]).expect("valid node id"),
                kind: RecordKind::Spawn,
                payload: bytes::Bytes::from_static(b"test"),
                crc: payload_crc(b"test"),
            };
            rt.apply(rec).await.unwrap();
        }

        // Fence ROOT first so the fence store has a row for it, then split.
        {
            let mut rt = runtime.lock().await;
            rt.fence_shard(CellId::ROOT, None, store.as_ref())
                .await
                .unwrap();
            let root_row = rt.fence().read(rt.grid(), CellId::ROOT).await.unwrap();
            let parent_row = root_row.unwrap();
            let children = rt.split(CellId::ROOT, &parent_row).await.unwrap();
            assert_eq!(children.len(), 8, "root splits into 8 children");
        }

        // A short interval so the test completes quickly.
        let interval = Duration::from_millis(100);
        let config = CheckpointConfig {
            interval,
            jitter: Duration::from_millis(10),
        };

        let scheduler = spawn_checkpoint_scheduler(Arc::clone(&runtime), store.clone(), &config);

        // Give the scheduler time to discover the 8 children and checkpoint
        // each at least once. The scheduler re-reads shards every loop
        // iteration; the loop is driven by timer expiry, so the worst-case
        // discovery delay is the jittered interval (~1x). Then the children
        // each get their own timer and fire within ~1x. Total: ≤ 2x.
        let deadline = std::time::Instant::now() + interval * 2 + Duration::from_millis(200);
        loop {
            let mut all_children_checkpointed = true;
            for child in CellId::ROOT.children() {
                let ckpt = store.load(child, GridId::ROOT).await.unwrap();
                if ckpt.is_none() {
                    all_children_checkpointed = false;
                    break;
                }
            }
            if all_children_checkpointed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "not all 8 children received a checkpoint within 2x interval"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        scheduler.shutdown().await;
        let rt = Arc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("scheduler released the runtime"))
            .into_inner();
        rt.close().await.unwrap();
    }

    #[tokio::test]
    async fn scheduler_retires_shards_not_in_runtime() {
        // Start a runtime with shard ROOT, create a scheduler, then simulate
        // that ROOT is no longer in the shard set (as after a split + retire).
        // The scheduler's reconcile loop should stop arming timers for ROOT.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemCheckpointStore::new());

        // We use a custom approach: create a runtime with ROOT, spawn the
        // scheduler, then replace the runtime with one that has no shards to
        // simulate retirement. However, the runtime is behind Arc<Mutex<>> so
        // we cannot replace it. Instead we verify the reconcile path by
        // splitting and then asserting that only the children are tracked.
        //
        // See scheduler_arms_timers_for_shards_created_after_spawn for the
        // full split path. This test checks that a retired shard (not in
        // rt.shards()) loses its timer after reconcile by checking that it
        // is no longer checkpointed.
        let runtime = Arc::new(tokio::sync::Mutex::new(
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store)).unwrap(),
        ));

        let interval = Duration::from_millis(50);
        let config = CheckpointConfig {
            interval,
            jitter: Duration::from_millis(5),
        };

        let scheduler = spawn_checkpoint_scheduler(Arc::clone(&runtime), store.clone(), &config);

        // Split ROOT into children, retiring the parent.
        {
            let mut rt = runtime.lock().await;
            rt.fence_shard(CellId::ROOT, None, store.as_ref())
                .await
                .unwrap();
            let root_row = rt.fence().read(rt.grid(), CellId::ROOT).await.unwrap();
            let parent_row = root_row.unwrap();
            rt.split(CellId::ROOT, &parent_row).await.unwrap();
        }

        // Wait long enough for a full cadence of the children but not so long
        // that the test is slow. The root should NOT be checkpointed anymore.
        tokio::time::sleep(interval * 3).await;

        // Root should have no checkpoint (it was retired; the scheduler's
        // timer for it was dropped on reconcile).
        let root_ckpt = store.load(CellId::ROOT, GridId::ROOT).await.unwrap();
        assert!(
            root_ckpt.is_none(),
            "retired shard ROOT should not be checkpointed"
        );

        // The children (now tracked by the scheduler) should eventually get
        // checkpoints — but we only verify that root is not checkpointed.
        scheduler.shutdown().await;
        let rt = Arc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("scheduler released the runtime"))
            .into_inner();
        rt.close().await.unwrap();
    }
}
