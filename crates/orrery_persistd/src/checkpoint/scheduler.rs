//! The checkpoint scheduler (docs/08-persistence.md §8, D16).
//!
//! Cell actors checkpoint **copy-on-update** on a **20 s, jittered per shard**
//! cadence (spreads FDB write load; prevents cluster-wide checkpoint
//! synchronization), and **immediately on cell quiesce** — a flush pulled
//! forward ahead of the cell's next jittered tick, after which the cell's
//! rows may be parked by the ordinary per-entity lease paths.
//!
//! This module owns that cadence. A [`CheckpointScheduler`] runs one timer per
//! shard cell, each jittered independently, and fires a checkpoint to the
//! runtime's [`CheckpointStore`]. A [`QuiesceSignal`] lets *this process*
//! request an immediate quiesce-flush for a cell; see its docs for why that
//! is not a request anything outside the process can make.
//!
//! The scheduler re-reads its runtime's shard set every loop iteration and
//! reconciles the timer vector against it, so shards that appear after the
//! scheduler starts (e.g. post-split children) are picked up and retired
//! shards are dropped.

use std::sync::Arc;
use std::time::Duration;

use orrery_protocol::{CellId, Lsn};

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
    /// Whether to release journal segments the checkpoints have made
    /// redundant (D20). Default **on**: with it off a node's journal, and the
    /// index rebuilt from it at every open, grow without bound for as long as
    /// the node runs.
    pub retention: bool,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(20),
            jitter: Duration::from_secs(5),
            retention: true,
        }
    }
}

/// Tracks each hosted shard's last durable checkpoint watermark and turns them
/// into the journal retention floor (D20).
///
/// **The floor is the minimum, and a shard that has never checkpointed has no
/// floor at all.** Both halves matter. The minimum, because a journal record
/// is releasable only once *every* shard that could still need it has folded
/// it — the shard that checkpointed most recently says nothing about the one
/// that has not checkpointed since. And the abstention, because a hosted shard
/// with no watermark yet is a shard whose whole history is still delta: taking
/// the minimum over the shards that *have* reported would release records it
/// has not folded.
#[derive(Debug, Default)]
struct ReleaseFloor {
    watermarks: std::collections::HashMap<CellId, Lsn>,
}

impl ReleaseFloor {
    fn record(&mut self, shard: CellId, watermark: Lsn) {
        // Monotone per shard: a checkpoint never covers less than the last one,
        // and a stale reply that said otherwise must not lower the floor.
        let entry = self.watermarks.entry(shard).or_insert(watermark);
        *entry = (*entry).max(watermark);
    }

    /// The floor for `shards`, or `None` while any of them has yet to report.
    fn floor(&mut self, shards: &[CellId]) -> Option<Lsn> {
        self.watermarks.retain(|shard, _| shards.contains(shard));
        if shards.is_empty() {
            return None;
        }
        shards
            .iter()
            .map(|shard| self.watermarks.get(shard).copied())
            .min()
            .flatten()
    }
}

/// Ask the journal to release everything below the floor these checkpoints
/// established, and log what it did.
///
/// The floor the scheduler *proposes* is the journal's own
/// [`Journal::retention_floor`] rather than the checkpoint floor directly,
/// because the two halves of a node's journal answer to different authorities
/// (D23): locally originated records to these checkpoints, mirrored records to
/// the floor their primary has itself reached. A passive follower holds only
/// the second kind, and reading its actors' empty watermark as a floor of
/// `0:0` is what kept every mirror unbounded.
///
/// [`Journal::retention_floor`]: crate::journal::Journal::retention_floor
fn release_journal(journal: &crate::journal::Journal, floor: Lsn) {
    match journal.release_before(floor) {
        Ok(release) => match release.blocked {
            // At `info`, deliberately: a release is at most one event per
            // checkpoint round, and it is the only line that says the journal
            // is being bounded at all. An operator reading a node's log should
            // be able to see retention working — or, from the `trace` arm
            // below plus a journal that keeps growing, see it not working and
            // why.
            None => tracing::info!(
                floor = %release.floor,
                records_dropped = release.records_dropped,
                bytes_before = release.bytes_before,
                bytes_after = release.bytes_after,
                "released journal below the checkpoint floor"
            ),
            // Every blocked release but one is routine and stays at `trace`.
            // `ArchiveLag` is the exception, because it is the only reason
            // whose cost *grows*: the other clamps are held by a peer that is
            // catching up on the same journal, while an unreachable archive
            // holds the floor while records keep arriving. See
            // [`report_archive_lag`].
            Some(crate::journal::ReleaseBlocked::ArchiveLag) => {
                report_archive_lag(journal, floor);
            }
            Some(reason) => tracing::trace!(floor = %floor, %reason, "journal release did nothing"),
        },
        Err(error) => tracing::warn!(floor = %floor, %error, "journal release failed"),
    }
}

/// Whether an archive gap has passed the point where a blocked release stops
/// being routine and becomes the §15 alarm.
///
/// Split out from [`report_archive_lag`]'s logging so the escalation itself is
/// assertable: a log level is not something a test can read back, and "the
/// alarm fires" is the property, not "a `warn!` macro was reached".
///
/// A gap with no verified watermark at all always alarms. A tailer that has
/// registered and verified nothing is either starting up — in which case the
/// journal has nothing to release yet and this costs one line — or it has
/// never once succeeded, which is the worst version of the countdown rather
/// than a lesser one.
#[must_use]
fn archive_lag_alarms(gap: &crate::journal::JournalArchiveGap) -> bool {
    use crate::journal::{ArchiveClaimState, ARCHIVE_LAG_ALARM_SEGMENTS};
    match gap.claim {
        ArchiveClaimState::Unregistered => false,
        ArchiveClaimState::Registered => true,
        ArchiveClaimState::Verified { .. } => gap.segments_behind >= ARCHIVE_LAG_ALARM_SEGMENTS,
    }
}

/// The §15 alarm: a release blocked on the archive, escalated by how far
/// behind the archive is.
///
/// docs/08-persistence.md §15 promises "alarm before shed via watermark
/// telemetry" for the journal-disk-full failure mode, and #806's clamp is what
/// makes an unreachable archive a path to it: the floor stops advancing, the
/// journal stops reclaiming, and it grows at the arrival rate the P2 gate
/// measures (~18 000 records/s, ~26 MB/s — D20). Logging that at `trace` like
/// any other blocked release would be shipping a silent countdown.
///
/// The escalation is a pure function of the gap rather than a rate limiter,
/// which is what lets it live in this stateless helper: below
/// [`ARCHIVE_LAG_ALARM_SEGMENTS`] the tailer is merely behind — one slow
/// upload on a 20 s cadence — and above it the journal is holding more than
/// half a gigabyte it cannot let go of. Either way this runs at most once per
/// checkpoint round, which is the same budget the `info` arm above is written
/// to.
///
/// [`ARCHIVE_LAG_ALARM_SEGMENTS`]: crate::journal::ARCHIVE_LAG_ALARM_SEGMENTS
fn report_archive_lag(journal: &crate::journal::Journal, floor: Lsn) {
    use crate::journal::ArchiveClaimState;

    let Some(gap) = journal.archive_gap(floor) else {
        // No archive claim, so `ArchiveLag` cannot be the reason. Unreachable
        // via `release_before`, which only returns the variant under a
        // registered claim; reported rather than assumed away.
        tracing::trace!(floor = %floor, "journal release did nothing");
        return;
    };
    let watermark = match gap.claim {
        ArchiveClaimState::Verified { watermark } => Some(watermark),
        ArchiveClaimState::Registered | ArchiveClaimState::Unregistered => None,
    };
    if archive_lag_alarms(&gap) {
        tracing::warn!(
            floor = %floor,
            watermark = ?watermark.map(|w| w.to_string()),
            segments_behind = gap.segments_behind,
            bytes_behind = gap.bytes_behind,
            reason = %crate::journal::ReleaseBlocked::ArchiveLag,
            "the archive is not keeping up; the journal cannot reclaim and will fill              (docs/09-services-and-ops.md §10, \"archive unreachable\")"
        );
    } else {
        tracing::trace!(
            floor = %floor,
            watermark = ?watermark.map(|w| w.to_string()),
            segments_behind = gap.segments_behind,
            bytes_behind = gap.bytes_behind,
            reason = %crate::journal::ReleaseBlocked::ArchiveLag,
            "journal release did nothing"
        );
    }
}

/// Retention on a node with no actors to checkpoint: a passive chain follower
/// (D23).
///
/// `run_follower` opens no runtime, no scheduler and no gateway — mirrored
/// records are its only writes — so the checkpoint cadence that drives
/// retention everywhere else does not exist there, and its mirror grew with
/// its uptime for exactly that reason. This is the same release call on the
/// same cadence, driven by a timer instead of by checkpoints, because on this
/// node there is nothing to checkpoint: the floor comes from the primary
/// ([`Journal::retention_floor`] with no local floor at all).
///
/// [`Journal::retention_floor`]: crate::journal::Journal::retention_floor
pub struct MirrorRetention {
    shutdown: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

impl MirrorRetention {
    /// Stop the driver and await its task.
    pub async fn shutdown(self) {
        self.shutdown.notify_waiters();
        let _ = self.join.await;
    }
}

/// Spawn the passive-follower retention driver described by [`MirrorRetention`].
#[must_use]
pub fn spawn_mirror_retention(
    journal: Arc<crate::journal::Journal>,
    interval: Duration,
) -> MirrorRetention {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_task = Arc::clone(&shutdown);
    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = shutdown_task.notified() => break,
            }
            if let Some(floor) = journal.retention_floor(None) {
                release_journal(&journal, floor);
            }
        }
    });
    MirrorRetention { shutdown, join }
}

/// An in-process request to checkpoint one cell immediately (§8).
///
/// This is a handle on a channel into the scheduler's own task and nothing
/// more. It has no wire representation, and no component outside this process
/// can raise it. No coordinator can call it: D24 (a) rules out a
/// coordinator→gateway control edge. The gateway holds only coordinator
/// **public** keys, and peers courier the signed facts the persistence tier
/// acts on ([`crate::gateway::CoordinatorHandoutAuthority`]). D24 (c) settles
/// what "checkpoint and quiesce" means today: ordinary per-entity paths park
/// affected lease rows, and the cell reaches durability on the ordinary 20 s
/// jittered cadence (D16). This signal only pulls that flush forward.
///
/// **A flush does not bound hot memory.** A checkpoint writes the cell's state
/// to durable storage and the actor goes on holding it; there is no cell-state
/// eviction path anywhere in this crate. The only implemented eviction is the
/// gateway's idle-*peer* registry, which is unrelated despite the shared word.
/// Bounding the hot tier by *populated* cells rather than universe size is the
/// **intent of a path that is not built** — issue #124 Part 2, which must
/// settle the trigger, the durability precondition, the interaction with §3.4
/// fencing, and the write amplification D23 measured, in an ADR before any of
/// it exists.
///
/// **Why this stays `pub` with no production caller.** Every caller in the
/// tree is test code (including `tests/checkpoint_restore.rs`). It is kept
/// public deliberately, as the request half of the seam issue #124 Part 2
/// builds on: an eviction path needs this "flush this cell now" entry point,
/// followed by a completion result proving which watermark committed, before
/// it may drop the cell from memory. Removing the type now only to re-add it
/// then would churn D21's frozen public surface for no gain. If #124 is ever
/// closed without an eviction path, this is dead surface and should go with
/// it.
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
    /// The quiesce-flush signal: an in-process handle for requesting an
    /// immediate checkpoint of a cell, ahead of its jittered cadence.
    ///
    /// No coordinator can call it (D24 (a)), and it is not a memory bound.
    /// See [`QuiesceSignal`] for both, and for why the type stays public with
    /// no production caller.
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
    let retention = config.retention;
    let shutdown_task = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        // Timer vector: each entry is a shard and its next checkpoint deadline.
        let mut timers: Vec<(CellId, tokio::time::Instant)> = Vec::new();
        let mut floor = ReleaseFloor::default();

        loop {
            // Re-read the shard set each iteration to pick up splits.
            let shards: Vec<CellId> = {
                let rt = runtime.lock().await;
                rt.shards().collect()
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
                    match checkpoint_cell(&runtime, &store, cell).await {
                        Ok(watermark) => floor.record(cell, watermark),
                        Err(e) => {
                            tracing::warn!(shard = %cell, error = %e, "quiesce checkpoint failed");
                        }
                    }
                    continue;
                }
            }

            // Fire any timers that are due.
            let now = tokio::time::Instant::now();
            for (shard, due) in timers.iter_mut() {
                if *due <= now {
                    match checkpoint_cell(&runtime, &store, *shard).await {
                        Ok(watermark) => floor.record(*shard, watermark),
                        Err(e) => {
                            tracing::warn!(shard = %shard, error = %e, "scheduled checkpoint failed");
                        }
                    }
                    *due = now + jittered(interval, jitter, *shard);
                }
            }

            // Retention runs after the round rather than after each shard: the
            // floor can only move when the shard that was holding it lowest
            // checkpoints, so per-shard attempts would be one useful call and
            // `shards - 1` blocked ones.
            if retention {
                let journal = {
                    let rt = runtime.lock().await;
                    Arc::clone(rt.journal())
                };
                if let Some(release) = journal.retention_floor(floor.floor(&shards)) {
                    release_journal(&journal, release);
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
    let retention = config.retention;
    let shutdown_task = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        let mut timers: Vec<(CellId, tokio::time::Instant)> = Vec::new();
        let mut floor = ReleaseFloor::default();

        loop {
            let shards: Vec<CellId> = runtime.shards().collect();

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
                        match target.checkpoint(store.as_ref()).await {
                            Ok(watermark) => floor.record(cell, watermark),
                            Err(e) => {
                                tracing::warn!(shard = %cell, error = %e, "quiesce checkpoint failed");
                            }
                        }
                    }
                    continue;
                }
            }

            let now = tokio::time::Instant::now();
            for (shard, due) in timers.iter_mut() {
                if *due <= now {
                    if let Ok(target) = runtime.checkpoint_target(*shard) {
                        match target.checkpoint(store.as_ref()).await {
                            Ok(watermark) => floor.record(*shard, watermark),
                            Err(e) => {
                                tracing::warn!(shard = %shard, error = %e, "scheduled checkpoint failed");
                            }
                        }
                    }
                    *due = now + jittered(interval, jitter, *shard);
                }
            }

            if retention {
                let journal = runtime.journal();
                if let Some(release) = journal.retention_floor(floor.floor(&shards)) {
                    release_journal(journal, release);
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
) -> Result<Lsn, CheckpointError> {
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

    /// The §15 alarm's escalation rule, asserted directly rather than through
    /// a log level nothing can read back.
    ///
    /// Landing the tailer without this alarm is landing a silent countdown
    /// (#808), so the boundary is pinned: a routine lag stays quiet, a gap of
    /// [`ARCHIVE_LAG_ALARM_SEGMENTS`] alarms, and a claim that has verified
    /// nothing at all alarms immediately.
    ///
    /// [`ARCHIVE_LAG_ALARM_SEGMENTS`]: crate::journal::ARCHIVE_LAG_ALARM_SEGMENTS
    #[test]
    fn the_archive_lag_alarm_fires_at_the_stated_gap_and_not_before() {
        use crate::journal::{ArchiveClaimState, JournalArchiveGap, ARCHIVE_LAG_ALARM_SEGMENTS};

        let gap = |claim, segments_behind| JournalArchiveGap {
            proposed: Lsn::new(segments_behind, 0),
            claim,
            segments_behind,
            bytes_behind: segments_behind * crate::journal::DEFAULT_SEGMENT_SIZE,
        };
        let verified = ArchiveClaimState::Verified {
            watermark: Lsn::new(0, 0),
        };

        assert!(
            !archive_lag_alarms(&gap(verified, 0)),
            "a caught-up archive is not an alarm"
        );
        assert!(
            !archive_lag_alarms(&gap(verified, ARCHIVE_LAG_ALARM_SEGMENTS - 1)),
            "one round behind is routine and stays at trace"
        );
        assert!(
            archive_lag_alarms(&gap(verified, ARCHIVE_LAG_ALARM_SEGMENTS)),
            "at the stated gap the journal is holding half a gigabyte it cannot release"
        );
        assert!(
            archive_lag_alarms(&gap(ArchiveClaimState::Registered, 0)),
            "a claim that has verified nothing is the worst case, not a lesser one"
        );
        assert!(
            !archive_lag_alarms(&gap(ArchiveClaimState::Unregistered, 99)),
            "with no claim the archive is not what is holding the floor"
        );
    }

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
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store))
                .await
                .unwrap(),
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
                retention: true,
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
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&seed_store))
                .await
                .unwrap(),
        ));
        let blocking_store = Arc::new(BlockingCheckpointStore::default());
        let scheduler = spawn_checkpoint_scheduler(
            Arc::clone(&runtime),
            blocking_store.clone(),
            &CheckpointConfig {
                interval: Duration::from_secs(60),
                jitter: Duration::ZERO,
                retention: true,
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
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store))
                .await
                .unwrap(),
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
            retention: true,
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

    #[test]
    fn a_release_floor_abstains_until_every_shard_has_reported() {
        let a = CellId::ROOT;
        let b = CellId::from_coords(glam::IVec3::new(-1, -1, -1), 1).expect("valid cell");
        let mut floor = ReleaseFloor::default();

        assert_eq!(floor.floor(&[]), None, "no shards, no floor");
        floor.record(a, Lsn::new(0, 900));
        assert_eq!(
            floor.floor(&[a, b]),
            None,
            "a shard that has never checkpointed has no floor to contribute"
        );

        floor.record(b, Lsn::new(0, 400));
        assert_eq!(
            floor.floor(&[a, b]),
            Some(Lsn::new(0, 400)),
            "the floor is the minimum, not the latest"
        );

        // A stale reply must not lower a floor already established.
        floor.record(b, Lsn::new(0, 100));
        assert_eq!(floor.floor(&[a, b]), Some(Lsn::new(0, 400)));

        // A retired shard stops holding the floor down, and stops being tracked.
        assert_eq!(floor.floor(&[a]), Some(Lsn::new(0, 900)));
        assert!(!floor.watermarks.contains_key(&b));
    }

    /// The scheduler's checkpoints are what bound the journal (D20): a running
    /// node's retention floor advances without anyone asking it to.
    ///
    /// Raw-backend only, because the floor advancing is the thing asserted and
    /// the Fjall fallback deliberately does not implement retention (D19, D20
    /// §7). The driver runs identically under both; only one of them reclaims.
    #[cfg(feature = "journal-raw")]
    #[tokio::test]
    async fn the_scheduler_releases_the_journal_behind_its_checkpoints() {
        use orrery_protocol::{JournalRecord, PersistId, RecordKind, Tick};

        use crate::payload_crc;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemCheckpointStore::new());
        let runtime = Arc::new(tokio::sync::Mutex::new(
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store))
                .await
                .unwrap(),
        ));

        {
            let rt = runtime.lock().await;
            for entity in 0..8u64 {
                let rec = JournalRecord {
                    lsn: Lsn::new(0, 0),
                    cell: CellId::ROOT,
                    grid: GridId::ROOT,
                    entity: PersistId::new(entity),
                    tick: Tick::new(1),
                    epoch: orrery_protocol::Epoch::new(0),
                    author: NodeId::from_bytes(&[0u8; 32]).expect("valid node id"),
                    kind: RecordKind::Spawn,
                    payload: bytes::Bytes::from_static(b"test"),
                    crc: payload_crc(b"test"),
                };
                rt.apply(rec).await.unwrap();
            }
            assert_eq!(rt.journal().released_floor(), Lsn::new(0, 0));
        }

        let scheduler = spawn_checkpoint_scheduler(
            Arc::clone(&runtime),
            store.clone(),
            &CheckpointConfig {
                interval: Duration::from_millis(50),
                jitter: Duration::from_millis(5),
                retention: true,
            },
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let floor = runtime.lock().await.journal().released_floor();
            if floor > Lsn::new(0, 0) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the scheduler never released the journal behind its checkpoints"
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
            CellRuntime::open(&test_runtime_config(dir.path()), &ckpt_store(&store))
                .await
                .unwrap(),
        ));

        let interval = Duration::from_millis(50);
        let config = CheckpointConfig {
            interval,
            jitter: Duration::from_millis(5),
            retention: true,
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

    /// The prose in this module twice claimed something the tree does not do:
    /// that the *coordinator* raises the quiesce signal, and that a flush
    /// therefore bounds hot memory by populated cells. Both were repaired for
    /// #124, and both are the kind of claim that regresses silently — nothing
    /// compiles differently when a doc comment starts lying again.
    ///
    /// So the check is the module reading itself. A flush persists the cell
    /// and the actor goes on holding it; until an eviction path exists (#124
    /// Part 2, proposed as D39), any sentence here asserting a memory bound is
    /// false. If that path lands, delete this test in the same change that
    /// makes the claim true — do not weaken it to keep it green.
    #[test]
    fn module_prose_claims_no_memory_bound_and_no_coordinator_signal() {
        const SCHEDULER_SOURCE: &str = include_str!("scheduler.rs");

        // Everything below the test module is this test's own text, which of
        // course contains the very phrases it forbids.
        let prose = SCHEDULER_SOURCE
            .split_once("mod tests {")
            .expect("scheduler.rs has a test module")
            .0;

        for forbidden in [
            "hot memory is bounded by",
            "bounded by *populated*",
            "the coordinator asks",
            "coordinator signal",
        ] {
            assert!(
                !prose.contains(forbidden),
                "scheduler.rs prose contains {forbidden:?}: a checkpoint does not \
                 evict, so it bounds nothing, and D24 (a) leaves no \
                 coordinator->gateway edge that could raise this signal"
            );
        }
    }
}
