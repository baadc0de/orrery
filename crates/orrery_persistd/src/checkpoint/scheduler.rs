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
/// One timer task per shard cell, each with an independent jitter, so
/// checkpoints spread across the interval rather than synchronizing cluster-wide
/// (D16). A quiesce request for a cell triggers an immediate checkpoint.
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
        // Snapshot the shard set once; splits add actors later, which the
        // runtime's own checkpoint() covers on the next pass. We schedule one
        // timer per shard here.
        let shards: Vec<CellId> = {
            let rt = runtime.lock().await;
            rt.shards().copied().collect()
        };

        let mut timers: Vec<(CellId, tokio::time::Instant)> = shards
            .iter()
            .map(|&shard| {
                let delay = jittered(interval, jitter);
                (shard, tokio::time::Instant::now() + delay)
            })
            .collect();

        loop {
            let now = tokio::time::Instant::now();
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
                    let _ = checkpoint_cell(&runtime, &store, cell).await;
                    continue;
                }
            }

            // Fire any timers that are due.
            let now = tokio::time::Instant::now();
            for (shard, due) in timers.iter_mut() {
                if *due <= now {
                    let _ = checkpoint_cell(&runtime, &store, *shard).await;
                    *due = now + jittered(interval, jitter);
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
    let rt = runtime.lock().await;
    rt.checkpoint_shard(shard, store.as_ref()).await
}

/// A jittered delay in `[interval - jitter, interval + jitter]`.
fn jittered(interval: Duration, jitter: Duration) -> Duration {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // A cheap deterministic-ish jitter from the clock; not security-relevant.
    let frac = (seed % 1_000_000) as f64 / 1_000_000.0;
    let offset = (frac * 2.0 - 1.0) * jitter.as_secs_f64();
    interval.saturating_add(Duration::from_secs_f64(offset.max(0.0)))
}
