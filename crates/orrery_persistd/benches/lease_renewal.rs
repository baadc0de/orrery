//! Lease-renewal cost rig (P2 shape).
//!
//! The batched heartbeat path (`Router::heartbeat_leases`) is what a peer's
//! periodic renewal costs the server. At the P2 operating point that is 250
//! sessions x 40 entities renewed every 3 s -- ~3 333 renewals/s against
//! ~200 intents/s -- and every number about it so far has come from the
//! whole-rig sweep, where it is mixed with bulk, journal and intent work.
//! This isolates it: one `CellRuntime`, the P2 shard/entity/session shape, and
//! nothing running but renewals.
//!
//! Measure-only, like `journal_latency`: it asserts nothing.
//!
//! Run with:
//! ```sh
//! cargo bench -p orrery_persistd --bench lease_renewal
//! ```
//!
//! Options:
//! ```text
//! --shards <N>     level-18 shards hosted (default 128, the P2 lattice)
//! --entities <N>   leases claimed, spread over the shards (default 10000)
//! --sessions <N>   peers renewing; batch size is entities/sessions (default 250)
//! --rounds <N>     renewal passes measured (default 8)
//! --locate-us <U>  artificial delay inside `LeaseStore::locate`, to stand in
//!                  for FoundationDB's round trip (default 0, i.e. the
//!                  in-process store persistd runs without --fdb-cluster-file)
//! --dir <PATH>     journal backing directory (default: a tempdir)
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::LeaseRenewal;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    CellRuntime, ClaimResult, JournalConfig, LeasePut, LeaseStore, LeaseStoreError, MemLeaseStore,
    Router, RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, Lease, LeaseId, NodeId, PersistId, SHARD_LEVEL,
};

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

/// The in-process store, plus a call counter and an optional delay.
///
/// The counter is the point: "how many `LeaseStore::locate` calls does one
/// renewal pass make" is a property of the routing code, not of the store, so
/// it is the same number under FDB. The delay is what turns that count into
/// the latency it costs when the store is FoundationDB rather than a HashMap.
struct CountingLocateStore {
    inner: MemLeaseStore,
    locates: AtomicU64,
    delay: Duration,
}

#[async_trait::async_trait]
impl LeaseStore for CountingLocateStore {
    async fn load_cell(
        &self,
        grid: GridId,
        shard: CellId,
    ) -> Result<Vec<(CellId, Lease)>, LeaseStoreError> {
        self.inner.load_cell(grid, shard).await
    }
    async fn put(
        &self,
        grid: GridId,
        cell: CellId,
        lease: &Lease,
    ) -> Result<LeasePut, LeaseStoreError> {
        self.inner.put(grid, cell, lease).await
    }
    async fn locate(
        &self,
        grid: GridId,
        entity: PersistId,
    ) -> Result<Option<CellId>, LeaseStoreError> {
        self.locates.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.inner.locate(grid, entity).await
    }
    async fn remove(
        &self,
        grid: GridId,
        cell: CellId,
        entity: PersistId,
    ) -> Result<(), LeaseStoreError> {
        self.inner.remove(grid, cell, entity).await
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize - 1;
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

struct Args {
    shards: usize,
    entities: usize,
    sessions: usize,
    rounds: usize,
    locate_us: u64,
    blocked: bool,
    dir: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = Args {
        shards: 128,
        entities: 10_000,
        sessions: 250,
        rounds: 8,
        locate_us: 0,
        blocked: false,
        dir: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().expect("flag needs a value");
        match flag.as_str() {
            "--shards" => args.shards = value().parse().expect("--shards"),
            "--entities" => args.entities = value().parse().expect("--entities"),
            "--sessions" => args.sessions = value().parse().expect("--sessions"),
            "--rounds" => args.rounds = value().parse().expect("--rounds"),
            "--locate-us" => args.locate_us = value().parse().expect("--locate-us"),
            // Put each session's whole inventory in one shard instead of
            // striping it across the lattice. Not a workload P2 produces --
            // it is the ablation that separates "the batch fans out to N
            // actors" from "each actor turn is expensive".
            "--blocked" => args.blocked = true,
            "--dir" => args.dir = Some(PathBuf::from(value())),
            // `cargo bench` passes libtest's own flags through; this
            // harness has none of them, so they are ignored rather than
            // fatal (the same reason `--bench` shows up here at all).
            "--bench" | "--nocapture" => {}
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

/// 8x4x4 level-18 cells, the lattice the P2 kill-9 gate seeds.
fn shard_cells(count: usize) -> Vec<CellId> {
    (0..count)
        .map(|i| {
            let x = (i % 8) as i32;
            let y = ((i / 8) % 4) as i32;
            let z = (i / 32) as i32;
            CellId::from_coords(glam::IVec3::new(x, y, z), SHARD_LEVEL).expect("in range")
        })
        .collect()
}

/// A distinct leaf cell inside `shard`, so the deployment matches the measured
/// P2 one: entities sit in as many leaf cells as there are entities, which is
/// what makes grouping by leaf cell fold nothing.
fn leaf_in(shard: CellId, nth: usize) -> CellId {
    let a = shard.children()[nth % 8];
    let b = a.children()[(nth / 8) % 8];
    b.children()[(nth / 64) % 8]
}

fn main() {
    let args = parse_args();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run(args));
}

async fn run(args: Args) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = args.dir.clone().unwrap_or_else(|| tmp.path().to_path_buf());
    let shards = shard_cells(args.shards);
    let store = Arc::new(CountingLocateStore {
        inner: MemLeaseStore::new(),
        locates: AtomicU64::new(0),
        delay: Duration::from_micros(args.locate_us),
    });
    let config = RuntimeConfig {
        shards: shards.clone(),
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir,
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        },
        node_id: 1,
        epoch: Epoch::new(1),
        ..RuntimeConfig::default()
    };
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let runtime = Arc::new(
        CellRuntime::open_with_lease_store(
            &config,
            &checkpoints,
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .expect("runtime opens"),
    );
    let holder = test_node(7);

    // Claim phase: entity i lives in shard i % shards, in its own leaf cell.
    let claim_started = Instant::now();
    let mut held: Vec<(CellId, PersistId, LeaseId)> = Vec::with_capacity(args.entities);
    for i in 0..args.entities {
        let (shard, nth) = if args.blocked {
            let per = args.entities.div_ceil(shards.len());
            (shards[i / per], i % per)
        } else {
            (shards[i % shards.len()], i / shards.len())
        };
        let cell = leaf_in(shard, nth);
        let entity = PersistId::new(1_000_000 + i as u64);
        let ClaimResult::Granted(row) = Router::claim_lease(
            runtime.as_ref(),
            GridId::ROOT,
            cell,
            entity,
            holder,
            ClaimKind::Weak,
            0,
        )
        .await
        .expect("claim routes") else {
            panic!("claim {i} denied");
        };
        held.push((cell, entity, row.lease_id));
    }
    let claim_elapsed = claim_started.elapsed();

    // Session i holds every entity congruent to i mod sessions -- the same
    // "one session's inventory is spread across the shard lattice" shape the
    // rig's `scheduler_shard` produces.
    let mut batches: Vec<Vec<LeaseRenewal>> = vec![Vec::new(); args.sessions];
    for (index, (cell, entity, lease_id)) in held.iter().enumerate() {
        let session = if args.blocked {
            (index / args.entities.div_ceil(args.sessions)).min(args.sessions - 1)
        } else {
            index % args.sessions
        };
        batches[session].push(LeaseRenewal {
            cell: *cell,
            entity: *entity,
            lease_id: *lease_id,
        });
    }
    let batch_len = batches.iter().map(Vec::len).max().unwrap_or(0);
    let distinct_shards: usize = {
        let mut cells: Vec<CellId> = batches[0]
            .iter()
            .map(|renew| orrery_protocol::shard_of(renew.cell))
            .collect();
        cells.sort_by_key(|cell| format!("{cell:?}"));
        cells.dedup_by_key(|cell| format!("{cell:?}"));
        cells.len()
    };

    println!(
        "lease-renewal rig: {} shards, {} entities, {} sessions ({} per batch, spanning {} shards), \
         {} rounds, locate delay {} us",
        args.shards,
        args.entities,
        args.sessions,
        batch_len,
        distinct_shards,
        args.rounds,
        args.locate_us,
    );
    println!("claim phase: {:.1} ms", ms(claim_elapsed));

    let before_locates = store.locates.load(Ordering::Relaxed);
    let mut samples: Vec<Duration> = Vec::with_capacity(args.rounds * args.sessions);
    let wall = Instant::now();
    for round in 0..args.rounds {
        let now_ms = 1_000 + round as u64;
        for batch in &batches {
            if batch.is_empty() {
                continue;
            }
            let started = Instant::now();
            let rows =
                Router::heartbeat_leases(runtime.as_ref(), GridId::ROOT, holder, batch, now_ms)
                    .await
                    .expect("renewal routes");
            samples.push(started.elapsed());
            debug_assert_eq!(rows.len(), batch.len());
            assert!(
                rows.iter().all(Option::is_some),
                "every held pair must renew"
            );
        }
    }
    let wall = wall.elapsed();
    let locates = store.locates.load(Ordering::Relaxed) - before_locates;

    samples.sort_unstable();
    let renewals = (args.rounds * args.entities) as f64;
    println!(
        "per-batch latency: p50 {:.3} ms  p99 {:.3} ms  max {:.3} ms  (n={})",
        ms(percentile(&samples, 0.50)),
        ms(percentile(&samples, 0.99)),
        ms(percentile(&samples, 1.0)),
        samples.len(),
    );
    println!(
        "throughput: {:.0} renewals/s over {:.2} s wall ({:.1} us per renewal)",
        renewals / wall.as_secs_f64(),
        wall.as_secs_f64(),
        wall.as_secs_f64() * 1e6 / renewals,
    );
    println!(
        "store locates: {locates} ({:.3} per renewal)",
        locates as f64 / renewals,
    );

    Arc::try_unwrap(runtime)
        .ok()
        .expect("sole owner")
        .close()
        .await
        .expect("close");
}
