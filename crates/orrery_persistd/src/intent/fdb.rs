//! FoundationDB-backed [`IntentExecutor`] (`fdb` feature, D11 §2.2, §7).
//!
//! Executes an intent inside **one serializable `db.run` transaction**,
//! following the worked example of docs/08-persistence.md §7 step by step:
//!
//! 0. **Idempotency first.** Read `intent/{intent_id}` before anything else —
//!    the read registers the row's conflict range, and a present row means
//!    this intent already ran: return the recorded outcome unchanged. This is
//!    what converts at-least-once delivery (C-1: intents ride the packet lane
//!    with client retransmit) into exactly-once outcomes.
//! 1. **Reads register conflict ranges.** Every durable key the executor
//!    touches (the idempotency row and the ledger rows its ops name) is read
//!    before any write, so a concurrent commit
//!    intersecting the read set aborts this transaction with `not_committed`
//!    and the retry loop re-checks honestly (§7).
//! 2. **Writes.** `set`/`atomic_op` apply the ledger effects; balances are
//!    little-endian `MutationType::Add` so the credit side is a blind
//!    increment. `PersistId`s are drawn from durable, process-local block
//!    grants. Reserving a block serializes on `pid/next` only once per block;
//!    individual intent transactions never read that hot key.
//! 3. **The outcome row.** The `IntentOutcome` is written to
//!    `intent/{intent_id}` in the same transaction, so the ack the gateway
//!    sends after `db.run` resolves implies a durable commit (RPO 0).
//!
//! **The ownership fence** sits between steps 0 and 1 when the executor
//! carries one ([`IntentFence`]): every shard the node activated is re-read in
//! this same transaction and must still name this owner at this epoch, so a
//! superseded persistd that can still reach FDB is refused before any effect.
//! That it comes *after* the idempotency read is deliberate — see the comment
//! at the call site.
//!
//! **Bounded retries** (§7): `db.run` re-runs the closure on `not_committed`;
//! after [`MAX_CONFLICT_RETRIES`] attempts the executor gives up with
//! [`IntentError::ContentionExhausted`], which the gateway maps to a
//! definitive `Rejected` refusal.

use std::sync::Arc;

use foundationdb::options::MutationType;
use foundationdb::{Database, FdbBindingError};

use orrery_protocol::{
    AccountId, AssetId, CellId, Epoch, GridId, Intent, IntentOutcome, PersistId,
};

use crate::fence::{FenceRow, FenceStatus};
use crate::keyspace;
use crate::FdbContext;

use super::{IntentError, IntentExecutor};

/// The retry bound on `not_committed` conflicts (docs/08-persistence.md §7:
/// "after 5 conflict retries … the gateway returns a definitive refusal").
const MAX_CONFLICT_RETRIES: u32 = 5;

/// The FDB error code for a commit conflict (`not_committed`).
const NOT_COMMITTED: i32 = 1020;

/// Retention for the `intent/{intent_id}` row (docs/08-persistence.md §6:
/// default **1 h**, swept by the checkpoint GC pass).
const INTENT_ROW_RETENTION_MS: u64 = 60 * 60 * 1000;

/// Number of ids held by one executor after it must replenish its local
/// grant.  An executor may crash with unused ids in its grant; that is an
/// intentional permanent gap, which is the only safe outcome because a
/// crashed process could have committed an intent carrying one of those ids.
const PERSIST_ID_BLOCK_GRANT: u64 = 4_096;

/// A durable block has been reserved in FDB and may be handed out locally.
/// `next` and `end` are 1-based, with `end` exclusive.
#[derive(Debug, Default)]
struct PersistIdAllocator {
    next: u64,
    end: u64,
}

impl PersistIdAllocator {
    fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.next)
    }
}

/// An FDB-backed intent executor.
///
/// The grid scopes the `pid/next` counter the executor mints from
/// ([`keyspace::pid_next_key`]) — tests take their own grid id so the shared
/// dev cluster's counter is never contended.
pub struct FdbIntentExecutor {
    db: Arc<Database>,
    grid: GridId,
    fence: Option<IntentFence>,
    allocator: Arc<tokio::sync::Mutex<PersistIdAllocator>>,
}

/// The active shard ownership an intent executor is allowed to write under.
///
/// **Every** named shard is verified, not one of them. An [`IntentOp`] carries
/// no cell (`orrery_protocol::persist`), so an intent cannot be attributed to a
/// shard and a per-shard fence would have nothing to select on. The executor
/// therefore admits an intent only while the node still actively owns its
/// whole shard set at `epoch` — the same set `--shard` activated at startup,
/// which persistd already requires to share one epoch. Losing any one shard
/// fences the executor completely, which is the conservative direction: a node
/// that has been partially superseded is not a node that may still mint
/// durable ledger effects.
///
/// [`IntentOp`]: orrery_protocol::IntentOp
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentFence {
    /// The grid-relative shards that admit these intents, all of which must
    /// still be actively owned at `epoch`.
    pub shards: Vec<CellId>,
    /// Active persistd node id.
    pub owner: u64,
    /// Active fencing epoch.
    pub epoch: Epoch,
}

impl FdbIntentExecutor {
    /// Build an executor using a process-scoped FDB context.
    #[must_use]
    pub fn from_context(context: &FdbContext, grid: GridId) -> Self {
        Self::from_database(context.database(), grid)
    }

    /// Build an executor from an already-open database handle.
    #[must_use]
    pub fn from_database(db: Arc<Database>, grid: GridId) -> Self {
        Self {
            db,
            grid,
            fence: None,
            allocator: Arc::new(tokio::sync::Mutex::new(PersistIdAllocator::default())),
        }
    }

    /// Build an executor whose writes require this exact active shard fence.
    #[must_use]
    pub fn fenced_from_context(context: &FdbContext, grid: GridId, fence: IntentFence) -> Self {
        Self::fenced_from_database(context.database(), grid, fence)
    }

    /// Build a fenced executor from an already-open database handle.
    #[must_use]
    pub fn fenced_from_database(db: Arc<Database>, grid: GridId, fence: IntentFence) -> Self {
        Self {
            db,
            grid,
            fence: Some(fence),
            allocator: Arc::new(tokio::sync::Mutex::new(PersistIdAllocator::default())),
        }
    }

    /// Connect to the cluster at `cluster_file`, minting ids from `grid`'s
    /// `pid/next` counter.
    ///
    /// Prefer constructing one [`FdbContext`] and using [`Self::from_context`]
    /// when a process needs more than one FDB-backed adapter.
    pub fn connect(cluster_file: &str, grid: GridId) -> Result<Self, IntentError> {
        let context =
            FdbContext::connect(cluster_file).map_err(|e| IntentError::Store(e.to_string()))?;
        Ok(Self::from_context(&context, grid))
    }

    /// The underlying database handle, for tests that need to read rows back.
    #[must_use]
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    /// Allocate ids before entering the intent transaction.
    ///
    /// A lease is committed before any of its ids are exposed to an intent.
    /// Thus an executor crash or a failed intent can leave holes, but never
    /// reuse an id.  Keeping this counter read outside the per-intent
    /// transaction is essential: reading `pid/next` in every intent creates
    /// a shared read conflict range and makes otherwise independent intents
    /// repeatedly abort under load.
    async fn allocate_ids(&self, count: u64) -> Result<Vec<PersistId>, IntentError> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut allocator = self.allocator.lock().await;
        if allocator.remaining() < count {
            // Do not recycle an incomplete grant. Its ids remain permanently
            // reserved, including when this executor is dropped mid-flight.
            let len = PERSIST_ID_BLOCK_GRANT.max(count);
            let start = reserve_id_block(&self.db, self.grid, len).await?;
            allocator.next = start;
            allocator.end = start
                .checked_add(len)
                .ok_or_else(|| IntentError::Store("PersistId allocator overflow".to_owned()))?;
        }

        let start = allocator.next;
        allocator.next = start
            .checked_add(count)
            .ok_or_else(|| IntentError::Store("PersistId allocator overflow".to_owned()))?;
        Ok((start..allocator.next).map(PersistId::new).collect())
    }
}

/// Require every owned shard's active ownership row in the same transaction as
/// an intent's idempotency row and effects. A promotion changes those keys and
/// fences a zombie executor before it can commit.
///
/// The fence is entirely this read: the intent's own `cell_epoch` is a
/// witness-set epoch chosen peer-side ([`orrery_protocol::CellEpoch`]) and has
/// never been comparable with the shard-ownership epoch, so it takes no part.
async fn require_intent_fence(
    trx: &foundationdb::Transaction,
    grid: GridId,
    fence: &IntentFence,
) -> Result<(), FdbBindingError> {
    let expected = FenceRow {
        owner: fence.owner,
        epoch: fence.epoch,
        status: FenceStatus::Active,
    };
    // Issue every row's read **concurrently**, then check them in shard
    // order.
    //
    // This runs inside the intent's own transaction, on the critical path of
    // a commit D16 budgets at p99 < 10 ms, and the fence covers the node's
    // whole shard set — 128 rows in the deployment docs/11-roadmap.md §P2
    // describes. Awaiting each `get` in turn made that 128 *serialized*
    // round trips per intent, at ~0.1–1 ms each (docs/08-persistence.md §5),
    // which is the commit budget several times over before any effect is
    // written. FDB's client is built for exactly this: the reads are
    // independent, each registers its own conflict range whichever order it
    // resolves in, and the transaction's snapshot makes the result identical
    // either way.
    //
    // `join_all` keeps the results in shard order, so a superseded node still
    // fails with the *first* shard it no longer owns rather than whichever
    // read lost a race — the message is reproducible across retries.
    let reads = fence.shards.iter().map(|&shard| {
        let key = keyspace::fence_key(grid, shard);
        async move { (shard, trx.get(&key, false).await) }
    });
    for (shard, raw) in futures::future::join_all(reads).await {
        let current: Option<FenceRow> = raw?
            .map(|bytes| postcard::from_bytes(bytes.as_ref()))
            .transpose()
            .map_err(store_err("fence row decode"))?;
        if current != Some(expected) {
            return Err(FdbBindingError::new_custom_error(Box::new(
                IntentError::Store(format!(
                    "intent fence mismatch for {grid}/{shard}: expected owner {} epoch {}, got {current:?}",
                    fence.owner, fence.epoch
                )),
            )));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl IntentExecutor for FdbIntentExecutor {
    async fn execute(&self, intent: &Intent) -> Result<IntentOutcome, IntentError> {
        let intent = intent.clone();
        let grid = self.grid;
        let fence = self.fence.clone();
        // The block is durably reserved before the transaction. Reusing this
        // exact vector across FDB retries makes retries safe, while a failed
        // overall attempt merely leaves a permanent, harmless gap.
        let minted = self.allocate_ids(intent.ops.len() as u64).await?;
        // `db.run` is the retry loop (§7: "retry loop is db.run's"). We bound
        // the `not_committed` retries with an interior-mutable attempt counter:
        // past the limit the closure returns `ContentionExhausted` as a custom
        // (non-retryable) error, which `run` surfaces verbatim.
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        let result: Result<IntentOutcome, FdbBindingError> = self
            .db
            .run(|trx, _maybe_committed| {
                let intent = intent.clone();
                let minted = minted.clone();
                let fence = fence.clone();
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                async move {
                    if attempt > MAX_CONFLICT_RETRIES + 1 {
                        return Err(FdbBindingError::new_custom_error(Box::new(
                            IntentError::ContentionExhausted,
                        )));
                    }

                    // Step 0 (§7): the idempotency row, read FIRST so its
                    // conflict range registers. A present row is a replay —
                    // return the recorded outcome unchanged. This is also the
                    // `commit_unknown_result` recovery path: a retried commit
                    // that actually landed is observed here as a replay.
                    let ikey = keyspace::intent_key(intent.intent_id);
                    if let Some(prev) = trx.get(&ikey, false).await? {
                        let row: keyspace::IntentRow =
                            postcard::from_bytes(&prev).map_err(store_err("intent row decode"))?;
                        return Ok(row.outcome);
                    }

                    // The fence is checked AFTER the idempotency read, and
                    // that ordering is deliberate: a superseded executor
                    // replaying an intent it already committed returns the
                    // recorded outcome un-fenced. Returning a durable fact
                    // that is already in the database produces no new effect,
                    // and refusing it instead would turn a retransmit (C-1:
                    // intents ride the packet lane) into a spurious rejection
                    // of a commit that did happen. Everything that could add
                    // an effect is below this line.
                    if let Some(fence) = &fence {
                        require_intent_fence(&trx, grid, fence).await?;
                    }

                    // Step 1: reads register conflict ranges. The tick is
                    // derived from the transaction's read version — a
                    // cluster-issued, strictly-ordered stand-in, honest in a
                    // way the gateway's old AtomicU64 counter never was (the
                    // commit version cannot be read inside the transaction).
                    let read_version = trx.get_read_version().await?;
                    let tick = orrery_protocol::Tick::new(read_version as u64);

                    // Step 2: use ids from the executor's already-durable
                    // grant. The harness mints one id per op; a linked
                    // Ruleset names the real allocation.

                    // Step 3: apply the ops' ledger effects (see `apply_ops`).
                    apply_ops(&trx, &intent)?;

                    // Step 4 (§7 step 3): the outcome row, same transaction.
                    let outcome = IntentOutcome::Committed { tick, minted };
                    let row = keyspace::IntentRow {
                        outcome: outcome.clone(),
                        gc_deadline_ms: now_ms() + INTENT_ROW_RETENTION_MS,
                    };
                    let encoded =
                        postcard::to_stdvec(&row).map_err(store_err("intent row encode"))?;
                    trx.set(&ikey, &encoded);
                    Ok(outcome)
                }
            })
            .await;

        match result {
            Ok(outcome) => Ok(outcome),
            Err(e) => Err(unwrap_binding_error(e)),
        }
    }
}

/// Recover an [`IntentError`] smuggled out of the closure as a custom error,
/// or map a raw FDB failure (`not_committed` past `run`'s own retries, or any
/// other code) onto the executor's error type.
fn unwrap_binding_error(e: FdbBindingError) -> IntentError {
    if let FdbBindingError::CustomError(ref boxed) = e {
        if let Some(ie) = boxed.downcast_ref::<IntentError>() {
            return match ie {
                IntentError::ContentionExhausted => IntentError::ContentionExhausted,
                IntentError::Store(s) => IntentError::Store(s.clone()),
            };
        }
    }
    if let Some(fdb_err) = e.get_fdb_error() {
        if fdb_err.code() == NOT_COMMITTED {
            return IntentError::ContentionExhausted;
        }
        return IntentError::Store(format!("fdb {}: {}", fdb_err.code(), fdb_err.message()));
    }
    IntentError::Store(format!("{e:?}"))
}

/// Build a closure converting a postcard error into a custom binding error
/// carrying an [`IntentError::Store`].
fn store_err(what: &'static str) -> impl Fn(postcard::Error) -> FdbBindingError {
    move |e| FdbBindingError::new_custom_error(Box::new(IntentError::Store(format!("{what}: {e}"))))
}

/// Current unix time in milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reserve `count` ids from the global grid counter. This is the only path
/// that reads `pid/next`, so its conflict serialization is amortized over a
/// block rather than paid by every intent.
async fn reserve_id_block(db: &Database, grid: GridId, count: u64) -> Result<u64, IntentError> {
    debug_assert!(count > 0);
    let result: Result<u64, FdbBindingError> = db
        .run(|trx, _maybe_committed| async move {
            let key = keyspace::pid_next_key(grid);
            let base = match trx.get(&key, false).await? {
                Some(v) => decode_pid_counter(&v)?,
                None => 0,
            };
            let start = base.checked_add(1).ok_or_else(pid_overflow_error)?;
            base.checked_add(count).ok_or_else(pid_overflow_error)?;
            trx.atomic_op(&key, &count.to_le_bytes(), MutationType::Add);
            Ok(start)
        })
        .await;
    result.map_err(unwrap_binding_error)
}

fn decode_pid_counter(value: &[u8]) -> Result<u64, FdbBindingError> {
    if value.len() > 8 {
        return Err(FdbBindingError::new_custom_error(Box::new(
            IntentError::Store("pid/next counter is wider than u64".to_owned()),
        )));
    }
    let mut buf = [0u8; 8];
    buf[..value.len()].copy_from_slice(value);
    Ok(u64::from_le_bytes(buf))
}

fn pid_overflow_error() -> FdbBindingError {
    FdbBindingError::new_custom_error(Box::new(IntentError::Store(
        "PersistId allocator exhausted".to_owned(),
    )))
}

/// Apply the harness default op semantics.
///
/// Op id 0 is the ledger credit the tests use: `args` is
/// `account u64 LE ‖ asset u64 LE ‖ delta i64 LE` (24 bytes), applied as a
/// little-endian `MutationType::Add` on `ledger/bal/{account}/{asset}` — the
/// credit side of §7's worked trade, blind-incremented. Every other op id is
/// `Ruleset`-opaque and a no-op here by design (docs/08-persistence.md §2.2:
/// the wire type only carries the op id and its encoded arguments).
fn apply_ops(trx: &foundationdb::Transaction, intent: &Intent) -> Result<(), FdbBindingError> {
    for op in &intent.ops {
        if op.op != 0 {
            continue;
        }
        if op.args.len() != 24 {
            return Err(FdbBindingError::new_custom_error(Box::new(
                IntentError::Store(format!(
                    "op 0 args must be 24 bytes (account‖asset‖delta), got {}",
                    op.args.len()
                )),
            )));
        }
        let account = u64::from_le_bytes(op.args[0..8].try_into().expect("slice len"));
        let asset = u64::from_le_bytes(op.args[8..16].try_into().expect("slice len"));
        let delta = i64::from_le_bytes(op.args[16..24].try_into().expect("slice len"));
        let key = keyspace::ledger_bal_key(AccountId::new(account), AssetId::new(asset));
        // 16-byte little-endian delta so `Add` extends/keeps the i128 width.
        let mut param = [0u8; 16];
        param[..8].copy_from_slice(&delta.to_le_bytes());
        trx.atomic_op(&key, &param, MutationType::Add);
    }
    Ok(())
}
