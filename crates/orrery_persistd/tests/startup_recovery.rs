//! Startup recovery over a large shard set (docs/11-roadmap.md §P2).
//!
//! `CellRuntime::open` seeds every shard from the durable tier before it folds
//! the journal tail. It did so **one shard at a time**, so recovery cost one
//! full store round trip per shard: 128 of them in the deployment the P2
//! criterion describes, and linearly more beyond it. These tests hold the two
//! properties that change and the one that must not: the loads overlap, they
//! stay bounded, and a failing load still produces one deterministic error
//! naming the first failing shard in shard order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orrery_persistd::checkpoint::{CheckpointData, CheckpointError, CheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{CellRuntime, JournalConfig, RuntimeConfig};
use orrery_protocol::{CellId, Epoch, GridId, SHARD_LEVEL};

/// A checkpoint store whose `load` takes measurable time, records the peak
/// number of concurrent loads, and can be told to fail for specific shards.
struct ObservedCheckpointStore {
    delay: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    loads: AtomicUsize,
    /// Shards whose load fails, each with the delay it takes to do so — so a
    /// test can make a *later* shard fail *sooner*.
    failures: Vec<(CellId, Duration)>,
}

impl ObservedCheckpointStore {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            loads: AtomicUsize::new(0),
            failures: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl CheckpointStore for ObservedCheckpointStore {
    async fn checkpoint(&self, _data: &CheckpointData) -> Result<(), CheckpointError> {
        Ok(())
    }

    async fn load(
        &self,
        shard: CellId,
        _grid: GridId,
    ) -> Result<Option<CheckpointData>, CheckpointError> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let failure = self
            .failures
            .iter()
            .find(|(failing, _)| *failing == shard)
            .map(|(_, delay)| *delay);
        tokio::time::sleep(failure.unwrap_or(self.delay)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if failure.is_some() {
            return Err(CheckpointError::Store(format!("unreadable shard {shard}")));
        }
        Ok(None)
    }

    async fn delete(&self, _shard: CellId, _grid: GridId) -> Result<(), CheckpointError> {
        Ok(())
    }
}

/// A disjoint level-`SHARD_LEVEL` shard set of `count` cells, in the canonical
/// (sorted) order startup uses.
fn shard_set(count: usize) -> Vec<CellId> {
    let mut shards: Vec<CellId> = (0..count)
        .map(|i| {
            let x = (i % 16) as i32;
            let y = ((i / 16) % 16) as i32;
            let z = (i / 256) as i32;
            CellId::from_coords(glam::IVec3::new(x, y, z), SHARD_LEVEL).expect("in range")
        })
        .collect();
    shards.sort_unstable_by_key(|shard| shard.to_bits());
    shards
}

fn config(dir: &std::path::Path, shards: Vec<CellId>) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(10),
                ..GroupCommitConfig::default()
            },
        },
        node_id: 1,
        epoch: Epoch::new(1),
        fence: Arc::new(orrery_persistd::MemFenceStore::new()),
    }
}

/// Recovery must seed the shard set from the durable tier concurrently.
///
/// The guarded property is overlap, observed rather than timed: with a 20 ms
/// load, a sequential recovery of 64 shards can never have two loads in flight
/// and cannot finish in under 1.28 s. The elapsed bound is a generous backstop
/// for a shared, loaded box — the peak-concurrency assertion is the one that
/// fails the moment the loop goes back to being sequential.
#[tokio::test]
async fn recovery_loads_checkpoints_concurrently() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shards = shard_set(64);
    let store = Arc::new(ObservedCheckpointStore::new(Duration::from_millis(20)));
    let as_store: Arc<dyn CheckpointStore> = store.clone();

    let started = Instant::now();
    let runtime = CellRuntime::open(&config(dir.path(), shards.clone()), &as_store)
        .await
        .expect("recovery opens");
    let elapsed = started.elapsed();

    assert_eq!(
        store.loads.load(Ordering::SeqCst),
        64,
        "every shard is still loaded exactly once"
    );
    assert!(
        store.max_in_flight.load(Ordering::SeqCst) > 1,
        "the checkpoint loads are sequential: never more than one in flight"
    );
    assert!(
        store.max_in_flight.load(Ordering::SeqCst) <= 32,
        "the loads must stay bounded, got {}",
        store.max_in_flight.load(Ordering::SeqCst)
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "64 sequential 20 ms loads would take 1.28 s; took {elapsed:?}"
    );
    for shard in shards {
        assert!(
            runtime.actor(GridId::ROOT, shard).is_some(),
            "{shard} hosted"
        );
    }
    runtime.close().await.expect("close runtime");
}

/// Concurrency must not turn one deterministic recovery failure into a race
/// between several: the error names the **first failing shard in shard
/// order**, even when a later shard fails sooner in wall-clock time.
#[tokio::test]
async fn recovery_reports_the_first_failing_shard_in_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shards = shard_set(16);
    let mut store = ObservedCheckpointStore::new(Duration::from_millis(5));
    // Shard 9 fails immediately; shard 3 — earlier in shard order — fails
    // only after 60 ms. A first-error-wins implementation would report 9.
    store.failures = vec![
        (shards[3], Duration::from_millis(60)),
        (shards[9], Duration::from_millis(0)),
    ];
    let as_store: Arc<dyn CheckpointStore> = Arc::new(store);

    let opened = CellRuntime::open(&config(dir.path(), shards.clone()), &as_store).await;
    let message = match opened {
        Ok(_) => panic!("an unreadable checkpoint must fail recovery"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains(&shards[3].to_string()),
        "recovery must name the first failing shard in order ({}), got: {message}",
        shards[3]
    );
    assert!(
        !message.contains(&shards[9].to_string()),
        "a later shard's failure must not win the race: {message}"
    );
}
