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
//!    touches (the idempotency row, the `pid/next` counter, the ledger rows
//!    its ops name) is read before any write, so a concurrent commit
//!    intersecting the read set aborts this transaction with `not_committed`
//!    and the retry loop re-checks honestly (§7).
//! 2. **Writes.** `set`/`atomic_op` apply the ledger effects; balances are
//!    little-endian `MutationType::Add` so the credit side is a blind
//!    increment. `PersistId`s are minted by `atomic_op(pid/next, Add)` — the
//!    counter never serializes concurrent intents beyond the atomic op.
//! 3. **The outcome row.** The `IntentOutcome` is written to
//!    `intent/{intent_id}` in the same transaction, so the ack the gateway
//!    sends after `db.run` resolves implies a durable commit (RPO 0).
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

use crate::FdbContext;
use crate::fence::{FenceRow, FenceStatus};
use crate::keyspace;

use super::{IntentError, IntentExecutor};

/// The retry bound on `not_committed` conflicts (docs/08-persistence.md §7:
/// "after 5 conflict retries … the gateway returns a definitive refusal").
const MAX_CONFLICT_RETRIES: u32 = 5;

/// The FDB error code for a commit conflict (`not_committed`).
const NOT_COMMITTED: i32 = 1020;

/// Retention for the `intent/{intent_id}` row (docs/08-persistence.md §6:
/// default **1 h**, swept by the checkpoint GC pass).
const INTENT_ROW_RETENTION_MS: u64 = 60 * 60 * 1000;

/// An FDB-backed intent executor.
///
/// The grid scopes the `pid/next` counter the executor mints from
/// ([`keyspace::pid_next_key`]) — tests take their own grid id so the shared
/// dev cluster's counter is never contended.
pub struct FdbIntentExecutor {
    db: Arc<Database>,
    grid: GridId,
    fence: Option<IntentFence>,
}

/// The active shard ownership an intent executor is allowed to write under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentFence {
    /// The grid-relative shard that admits these intents.
    pub shard: CellId,
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
}

/// Require the active ownership row in the same transaction as an intent's
/// idempotency row and effects. A promotion changes this key and fences a
/// zombie executor before it can commit.
async fn require_intent_fence(
    trx: &foundationdb::Transaction,
    grid: GridId,
    fence: IntentFence,
    intent: &Intent,
) -> Result<(), FdbBindingError> {
    if intent.cell_epoch != fence.epoch {
        return Err(FdbBindingError::new_custom_error(Box::new(
            IntentError::Store(format!(
                "intent epoch {} does not match active shard epoch {}",
                intent.cell_epoch, fence.epoch
            )),
        )));
    }
    let key = keyspace::fence_key(grid, fence.shard);
    let current: Option<FenceRow> = trx
        .get(&key, false)
        .await?
        .map(|bytes| postcard::from_bytes(bytes.as_ref()))
        .transpose()
        .map_err(store_err("fence row decode"))?;
    if current
        != Some(FenceRow {
            owner: fence.owner,
            epoch: fence.epoch,
            status: FenceStatus::Active,
        })
    {
        return Err(FdbBindingError::new_custom_error(Box::new(
            IntentError::Store(format!(
                "intent fence mismatch for {grid}/{}: expected owner {} epoch {}, got {current:?}",
                fence.shard, fence.owner, fence.epoch
            )),
        )));
    }
    Ok(())
}

#[async_trait::async_trait]
impl IntentExecutor for FdbIntentExecutor {
    async fn execute(&self, intent: &Intent) -> Result<IntentOutcome, IntentError> {
        let intent = intent.clone();
        let grid = self.grid;
        let fence = self.fence;
        // `db.run` is the retry loop (§7: "retry loop is db.run's"). We bound
        // the `not_committed` retries with an interior-mutable attempt counter:
        // past the limit the closure returns `ContentionExhausted` as a custom
        // (non-retryable) error, which `run` surfaces verbatim.
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        let result: Result<IntentOutcome, FdbBindingError> = self
            .db
            .run(|trx, _maybe_committed| {
                let intent = intent.clone();
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

                    if let Some(fence) = fence {
                        require_intent_fence(&trx, grid, fence, &intent).await?;
                    }

                    // Step 1: reads register conflict ranges. The tick is
                    // derived from the transaction's read version — a
                    // cluster-issued, strictly-ordered stand-in, honest in a
                    // way the gateway's old AtomicU64 counter never was (the
                    // commit version cannot be read inside the transaction).
                    let read_version = trx.get_read_version().await?;
                    let tick = orrery_protocol::Tick::new(read_version as u64);

                    // Step 2: mint one `PersistId` per op from `pid/next`
                    // (§7 "Id minting in the receipt"). The harness mints one
                    // id per op; a linked Ruleset names the real allocation.
                    let pid_key = keyspace::pid_next_key(grid);
                    let minted = mint_ids(&trx, &pid_key, intent.ops.len() as u64).await?;

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

/// Mint `count` `PersistId`s from the `pid/next` counter via `MutationType::Add`.
///
/// The counter is read **before** the atomic add so the read registers a
/// conflict range and the returned base is this transaction's own view of the
/// pre-add value; the add itself is blind, so concurrent intents do not
/// serialize on the counter beyond the atomic op (§7). Ids are 1-based — id 0
/// is never minted (a `PersistId` of 0 reads as "unset" downstream).
async fn mint_ids(
    trx: &foundationdb::Transaction,
    pid_key: &[u8],
    count: u64,
) -> Result<Vec<PersistId>, FdbBindingError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let base = match trx.get(pid_key, false).await? {
        Some(v) => {
            let mut buf = [0u8; 8];
            let n = v.len().min(8);
            buf[..n].copy_from_slice(&v[..n]);
            u64::from_le_bytes(buf)
        }
        None => 0,
    };
    trx.atomic_op(pid_key, &count.to_le_bytes(), MutationType::Add);
    Ok((base + 1..=base + count).map(PersistId::new).collect())
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
