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
//!
//!    **Every read happens before every write**, across the whole intent and
//!    not merely within one op: [`plan_ops`] reads and checks all the ops,
//!    then [`apply_plan`] writes. Two reasons, and the second is a
//!    correctness bug rather than a style preference. It is §7's order; and
//!    `db.run` commits whatever the closure staged, so an executor that wrote
//!    as it went and returned early on the third op's refusal would commit the
//!    first two ops' effects along with the refusal.
//! 3. **The outcome row.** The `IntentOutcome` is written to
//!    `intent/{intent_id}` in the same transaction, so the ack the gateway
//!    sends after `db.run` resolves implies a durable commit (RPO 0).
//!
//! **The anti-dupe invariant.** [`LEDGER_ITEM_TRANSFER_OP`] is the op that
//! makes any of this matter. It reads `ledger/item/{item_uid}` before writing
//! it, and *that read* is what registers the conflict range two concurrent
//! transfers of the same item share: at most one of them can commit, and the
//! loser's `db.run` retry re-reads the winner's owner and refuses with
//! `REASON_NOT_ITEM_OWNER` — §7's "fails the check honestly". A durable
//! refusal returns before [`apply_plan`] runs, so it writes nothing at all,
//! not even an `intent/` row: a rejected intent is not a durable fact and a
//! later resubmission must be free to succeed if the ledger has moved.
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

use futures::TryStreamExt as _;

use crate::fence::{FenceRow, FenceStatus};
use crate::keyspace;
use crate::FdbContext;

use super::provisional::{self, ProvisionalStore};
use super::stages;
use super::{
    IntentError, IntentExecutor, IntentPlan, ItemTransferArgs, OpsVerdict, PlannedWrite,
    LEDGER_CREDIT_OP, LEDGER_ITEM_TRANSFER_OP,
};

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
    /// The shard set every intent transaction re-reads before committing.
    ///
    /// Behind a lock because a live shard handover (D26 rule 3) changes it on
    /// a running node. Left fixed at what `--shard` activated, a node that
    /// correctly handed one shard to a sibling would find *every* subsequent
    /// intent refused — the fence verifies the whole set, and one row now
    /// naming the successor fails it — so a planned shard move would take the
    /// node's entire intent ledger down with it. See
    /// [`FdbIntentExecutor::refence`].
    fence: std::sync::RwLock<Option<IntentFence>>,
    allocator: Arc<tokio::sync::Mutex<PersistIdAllocator>>,
    /// The epoch cache this executor records against, when the gateway
    /// enforces K-of-N.
    ///
    /// `None` is the enforcement-off build and writes no `epoch/` or `attest/`
    /// row at all — an executor that recorded an eligible vector nobody
    /// enforced would be banking evidence about a check that did not happen.
    witness_epochs: Option<Arc<crate::witness_epoch::WitnessEpochAuthority>>,
    /// The `owner(n)` resolver `E(I)` is derived through when the recorded
    /// vector is written (D31 clause (e)).
    ///
    /// Set and cleared together with [`Self::witness_epochs`], because a
    /// recorded vector derived through a *different* resolver from the one
    /// admission used is a recorded vector the audit cannot read as evidence
    /// of the decision that was actually made.
    witness_bindings: Option<crate::gateway::SharedBindingAuthority>,
    /// The posture the gateway's validator admits under, which decides two
    /// things here and nothing else (D32 clause (d)): the `enforced` marker on
    /// the [`keyspace::AttestRow`] this executor writes, and whether the
    /// commit-time required-subset re-proof is armed.
    ///
    /// Wired through [`Self::recording_epochs`], [`Self::shadowing_epochs`]
    /// and [`Self::tracking_posture`] rather than read from the validator,
    /// because the two seams do not know each other:
    /// [`super::IntentExecutor`] is implemented outside this crate and takes
    /// no validator.
    ///
    /// # Why it is a posture cell and not a mode
    ///
    /// D32 clause (c) makes the mode *runtime-settable* and bounds the time
    /// from an operator's decision to a stopped control at one poll interval
    /// plus apply. A frozen copy here cannot honour that bound, and the way it
    /// fails is the worst available one: demoting the validator
    /// `Required -> Shadow` would move the refusal from admission to commit
    /// rather than removing it, because the re-proof below would still be
    /// armed. The control would go on acting under a mode that says it does
    /// not — clause (b)'s second corollary, and exactly what [#222]'s gate leg
    /// exists to catch. Sharing the validator's own cell
    /// ([`super::BaselineIntentValidator::posture`]) is what makes the two
    /// halves of one control move together.
    ///
    /// [`Self::recording_epochs`] and [`Self::shadowing_epochs`] keep their
    /// meaning: each installs a private cell pinned to that mode, which is the
    /// right answer for a deployment with no runtime lever wired up.
    ///
    /// [#222]: https://github.com/baadc0de/orrery/issues/222
    witness_enforcement: super::AttestationPosture,
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
    /// Replace the shard set this executor admits intents under.
    ///
    /// Called after a live shard handover commits (either side of it): the
    /// outgoing owner drops the shard it handed away, the successor adds the
    /// one it adopted, at the epoch its row now carries. An executor with no
    /// fence stays unfenced — this never *installs* one, because whether a
    /// deployment fences its ledger at all is a startup decision.
    pub fn refence(&self, shards: Vec<CellId>, owner: u64, epoch: Epoch) {
        let mut fence = self.fence.write().expect("intent fence lock poisoned");
        if fence.is_some() {
            *fence = Some(IntentFence {
                shards,
                owner,
                epoch,
            });
        }
    }

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
            fence: std::sync::RwLock::new(None),
            allocator: Arc::new(tokio::sync::Mutex::new(PersistIdAllocator::default())),
            witness_epochs: None,
            witness_bindings: None,
            witness_enforcement: super::AttestationPosture::new(super::AttestationEnforcement::Off),
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
            fence: std::sync::RwLock::new(Some(fence)),
            allocator: Arc::new(tokio::sync::Mutex::new(PersistIdAllocator::default())),
            witness_epochs: None,
            witness_bindings: None,
            witness_enforcement: super::AttestationPosture::new(super::AttestationEnforcement::Off),
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

    /// Record this executor's committed intents against `epochs`, deriving
    /// the recorded eligible vector through `bindings`.
    ///
    /// Wire the **same** cache and the **same** resolver the gateway's
    /// validator enforces against. The two are not independent: admission
    /// draws the required subset from a cache entry, and this executor writes
    /// the draw commitment that entry carries. A second, differently-seeded
    /// cache here would publish a commitment to a key no admission decision
    /// was ever made under, which is the one way this scheme can be broken
    /// from the inside.
    ///
    /// # Why the resolver is now the second half of the same argument
    ///
    /// `E(I)` used to be a pure function of the announced set and the issuer,
    /// which is why this executor re-derived it instead of threading a vector
    /// through [`super::IntentExecutor::execute`]. With D10 item 4's account
    /// half enforced it is **not** pure any more: it depends on `owner(n)`,
    /// which is a live view. Re-deriving it through a different resolver would
    /// let the recorded vector and the admitted one disagree by construction,
    /// so the two sides share one authority instead.
    ///
    /// They can still disagree by *time* — a binding that moved between
    /// admission and commit — and that residual is D31 clause (h)'s and is
    /// bounded by `T_stale`: "a mismatch inside `T_stale` is not evidence of
    /// anything, and a mismatch outside `T_stale` is". Collapsing it to zero
    /// means recording the binding view alongside the vector, which is that
    /// record's Open question 1 and not this executor's to answer.
    #[must_use]
    pub fn recording_epochs(
        mut self,
        epochs: Arc<crate::witness_epoch::WitnessEpochAuthority>,
        bindings: crate::gateway::SharedBindingAuthority,
    ) -> Self {
        self.witness_epochs = Some(epochs);
        self.witness_bindings = Some(bindings);
        self.witness_enforcement =
            super::AttestationPosture::new(super::AttestationEnforcement::Required);
        self
    }

    /// [`Self::recording_epochs`] for a gateway admitting in
    /// [`super::AttestationEnforcement::Shadow`].
    ///
    /// Two differences, both D32 clause (d)'s, and both necessary rather than
    /// cosmetic:
    ///
    /// - The [`keyspace::AttestRow`] is written with `enforced: false`. The row
    ///   still has to be written — a shadow-period commit is an attested
    ///   commit as far as D27 clause (f)'s audit is concerned, and omitting it
    ///   would leave the whole observation period unauditable — but a row that
    ///   looked enforced and was not is a false audit trail, which is worse
    ///   than none.
    /// - The commit-time required-subset re-proof is **disarmed**. That check
    ///   exists to stop a stale-key intent committing below quorum, and shadow
    ///   commits below quorum on purpose; leaving it armed would make shadow
    ///   refuse at commit what it admitted at admission — acting after failing
    ///   to act, which violates clause (b) from the far side and is the worst
    ///   of both modes.
    ///
    /// The draw-key adoption is *not* one of the differences: the cache must
    /// converge on the durable key regardless of mode, or a promotion to
    /// `required` would begin against a key this gateway never adopted.
    #[must_use]
    pub fn shadowing_epochs(
        mut self,
        epochs: Arc<crate::witness_epoch::WitnessEpochAuthority>,
        bindings: crate::gateway::SharedBindingAuthority,
    ) -> Self {
        self.witness_epochs = Some(epochs);
        self.witness_bindings = Some(bindings);
        self.witness_enforcement =
            super::AttestationPosture::new(super::AttestationEnforcement::Shadow);
        self
    }

    /// [`Self::recording_epochs`] against a posture cell the caller also
    /// holds, rather than one pinned at startup.
    ///
    /// The constructor a gateway that wires D32 clause (c)'s runtime lever
    /// uses: hand this the *same* [`super::AttestationPosture`] the
    /// [`super::BaselineIntentValidator`] reads
    /// ([`super::BaselineIntentValidator::posture`]) and one write moves both
    /// halves of control C1 together — the admission refusal and the
    /// commit-time re-proof, the `enforced` marker with them.
    ///
    /// Two cells would be two controls wearing one name, and the failure is
    /// silent in the dangerous direction: a demotion that reached only the
    /// validator would leave this executor refusing at commit what admission
    /// admitted, so a control an operator believes is observing would still be
    /// acting.
    #[must_use]
    pub fn tracking_posture(
        mut self,
        epochs: Arc<crate::witness_epoch::WitnessEpochAuthority>,
        bindings: crate::gateway::SharedBindingAuthority,
        posture: super::AttestationPosture,
    ) -> Self {
        self.witness_epochs = Some(epochs);
        self.witness_bindings = Some(bindings);
        self.witness_enforcement = posture;
        self
    }

    /// The posture cell this executor reads at the top of every commit.
    ///
    /// The read side of [`Self::tracking_posture`], and the reason it is a
    /// cell rather than a mode: a caller that cached this answer would be
    /// reporting the posture at startup and calling it the posture now, which
    /// is the mistake [`super::BaselineIntentValidator::posture`] documents on
    /// the admission half of the same control.
    #[must_use]
    pub fn attestation_posture(&self) -> super::AttestationPosture {
        self.witness_enforcement.clone()
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

        // Two stages, not one, and the split is the whole reason to measure
        // here: `alloc_wait` is the queue behind a process-wide mutex, and
        // `alloc_refill` is the FDB transaction that mutex is held **across**
        // roughly every `PERSIST_ID_BLOCK_GRANT` ids. A refill that lands in
        // the tail stalls every concurrent intent, and averaged into one
        // number it is invisible.
        let mut allocator = stages::timed(|t| &mut t.alloc_wait_us, self.allocator.lock()).await;
        if allocator.remaining() < count {
            // Do not recycle an incomplete grant. Its ids remain permanently
            // reserved, including when this executor is dropped mid-flight.
            let len = PERSIST_ID_BLOCK_GRANT.max(count);
            let start = stages::timed(
                |t| &mut t.alloc_refill_us,
                reserve_id_block(&self.db, self.grid, len),
            )
            .await?;
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
    // D26 rule 1's ownership function: `owner(g, s)` is the row's owner when
    // its status is `Active` **or** `Draining`. This fence asks whether this
    // node is still the single writer for its shard set, which a live handover
    // does not change until its second CAS — so a shard being drained
    // (D26 rule 3 steps 1–5) is still this node's, and the intents it is
    // fenced by must keep committing. Refusing them would take the node's
    // whole intent ledger down for the length of every planned shard move,
    // because the fence covers the *whole* set and one non-`Active` row fails
    // it. The same reading is applied on the checkpoint path
    // (`checkpoint/fdb.rs`), where getting it wrong refused the `PreHandover`
    // checkpoint outright.
    let owned_here = |current: Option<FenceRow>| {
        current.is_some_and(|row| {
            row.owner == fence.owner
                && row.epoch == fence.epoch
                && matches!(
                    row.status,
                    FenceStatus::Active | FenceStatus::Draining { .. }
                )
        })
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
    //
    // Each read is timed individually as well as the fan-out as a whole,
    // because those two numbers answer different questions and only their
    // *difference* is diagnostic. The fan-out completes when the slowest read
    // does, so `fence_us` large **with** `fence_read_max_us` large means the
    // cluster served one read slowly; `fence_us` large with every individual
    // read fast means the time went between the reads resolving and this task
    // being polled again — a scheduler symptom wearing an FDB costume.
    // **One range read, not one point read per shard.** The fence keys are
    // `'a' || grid || shard_bits` (`keyspace::fence_key`), so a grid's rows
    // are contiguous and the whole set arrives in a single operation.
    //
    // Measured on the P2 gate before this: 128.0 reads per intent, 15 867
    // intents in 30 s — **67 699 FDB reads/s**, more than 20x the renewal
    // locates docs/08 §2.2.4 took off the same `libfdb_c` network thread that
    // docs/14-capacity.md §5.1 measured as one box's whole capacity — and
    // 22.5% of an intent's mean server span.
    //
    // Nothing about the fence's meaning changes. The same rows are read
    // inside the same transaction, so they register the same read conflict
    // ranges and a superseded node still cannot commit; the same values are
    // compared against the same expected row; and the check still runs in
    // shard order, so the error still names the first shard the node no
    // longer owns rather than whichever read lost a race. A range read's
    // conflict range spans the whole span rather than 128 points, which also
    // conflicts on a row *inserted* into it — strictly stricter, and in the
    // conservative direction this fence already argues for.
    let (Some(lo), Some(hi)) = (
        fence.shards.iter().map(|s| s.to_bits()).min(),
        fence.shards.iter().map(|s| s.to_bits()).max(),
    ) else {
        return Ok(());
    };
    let start = keyspace::fence_key(grid, CellId::from_bits(lo).expect("shard round-trips"));
    let mut end =
        keyspace::fence_key(grid, CellId::from_bits(hi).expect("shard round-trips")).to_vec();
    end.push(0); // exclusive end just past the last shard's key
    let fence_started = std::time::Instant::now();
    let mut stream = trx.get_ranges_keyvalues(
        foundationdb::RangeOption {
            begin: foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
            end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
            ..foundationdb::RangeOption::default()
        },
        false,
    );
    let mut seen: std::collections::HashMap<u64, FenceRow> = std::collections::HashMap::new();
    while let Some(kv) = stream.try_next().await? {
        let key = kv.key();
        if key.len() != 13 {
            continue;
        }
        let bits = u64::from_be_bytes(key[5..13].try_into().expect("13-byte fence key"));
        let row: FenceRow =
            postcard::from_bytes(kv.value()).map_err(store_err("fence row decode"))?;
        seen.insert(bits, row);
    }
    let fence_us = fence_started.elapsed().as_micros() as u64;
    stages::trace(|t| {
        t.fence_us += fence_us;
        // One FDB operation now, whatever the shard count. The rows verified
        // are `fence.shards.len()`; the reads issued are what this counts,
        // and the difference between the two is the change.
        t.fence_reads += 1;
        t.fence_read_max_us = t.fence_read_max_us.max(fence_us);
    });
    let results: Vec<(CellId, Option<FenceRow>)> = fence
        .shards
        .iter()
        .map(|&shard| (shard, seen.get(&shard.to_bits()).copied()))
        .collect();
    for (shard, current) in results {
        if !owned_here(current) {
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
        self.run(intent, None).await
    }

    async fn execute_provisional(
        &self,
        intent: &Intent,
        account: AccountId,
    ) -> Result<IntentOutcome, IntentError> {
        self.run(intent, Some(account)).await
    }
}

impl FdbIntentExecutor {
    /// §7's transaction, with D29's low-population path folded into it.
    ///
    /// `provisional` is `Some(account)` for a commit admitted to D29's
    /// low-population path. The reads, the checks and the ledger writes are
    /// identical either way — a provisional commit is a real commit, and the
    /// reply after `db.run` resolves is an RPO-0 statement exactly as
    /// `Committed` is. Three things differ, and all three are recorded rather
    /// than applied differently: the finality on the `intent/` row, the
    /// deadline beside it, and a hold in `provisional/{account}` naming every
    /// balance this intent wrote.
    async fn run(
        &self,
        intent: &Intent,
        provisional: Option<AccountId>,
    ) -> Result<IntentOutcome, IntentError> {
        let intent_handle = intent.cell_epoch.0;
        let intent = intent.clone();
        let grid = self.grid;
        let fence = self
            .fence
            .read()
            .expect("intent fence lock poisoned")
            .clone();
        // The block is durably reserved before the transaction. Reusing this
        // exact vector across FDB retries makes retries safe, while a failed
        // overall attempt merely leaves a permanent, harmless gap.
        let minted = self.allocate_ids(intent.ops.len() as u64).await?;
        let epochs = self.witness_epochs.clone();
        let bindings = self.witness_bindings.clone();
        // Read once, here, and used for the whole of this intent's commit.
        // D32 clause (c): "intents already past validation complete under the
        // prior mode" — so a posture write landing mid-transaction must not
        // arm the re-proof for an intent shadow already admitted, nor mark its
        // row `enforced` because the flag moved after the fact.
        let enforcement = self.witness_enforcement.get();
        // Set by the closure only on an attempt that wrote or verified the
        // `epoch/` row. It is **not** the same question as "did this intent
        // commit": the idempotency replay path below returns a recorded
        // `Committed` outcome without running step 2c at all, and marking the
        // cache on that would tell every later intent in the cell-epoch that a
        // row exists which nothing ever wrote — leaving the draw commitment
        // undurable while intents commit under the epoch, which is exactly the
        // ordering rule D27 clause (d) exists to enforce.
        let epoch_row_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Derived once, outside the retry loop: it is a pure function of the
        // intent's ops and re-deriving it per attempt would be work the
        // conflict did not ask for.
        let named = provisional::named_balances(&intent);
        // `db.run` is the retry loop (§7: "retry loop is db.run's"). We bound
        // the `not_committed` retries with an interior-mutable attempt counter:
        // past the limit the closure returns `ContentionExhausted` as a custom
        // (non-retryable) error, which `run` surfaces verbatim.
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        // The retry loop's own instrumentation. Before this, `attempts` was
        // maintained only to enforce the retry bound and was dropped when
        // `execute` returned, so "does `db.run` retry at all?" had no answer
        // available from any artifact in this repo — and "conflicts are zero"
        // does not answer it, because `on_error` also fires on 1007, 1009,
        // 1021, 1037 and 1213.
        let hooks = std::sync::Arc::new(IntentRunnerHooks::default());

        let result: Result<IntentOutcome, FdbBindingError> = self
            .db
            .run_with_hooks(hooks.as_ref(), |trx, _maybe_committed| {
                let intent = intent.clone();
                let minted = minted.clone();
                let fence = fence.clone();
                let epochs = epochs.clone();
                let bindings = bindings.clone();
                let epoch_row_seen = std::sync::Arc::clone(&epoch_row_seen);
                let named = named.clone();
                let hooks = std::sync::Arc::clone(&hooks);
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
                    // The read version, taken explicitly and first.
                    //
                    // It is not an extra round trip: the idempotency read
                    // below cannot return without one, so libfdb_c would fetch
                    // it here anyway. Taking it by name turns GRV — which sits
                    // on the critical path of every intent including a pure
                    // replay, and which confirms liveness against the tlogs,
                    // i.e. against the same md2 array the journal fsyncs to —
                    // into a stage that can be blamed, instead of an invisible
                    // prefix of the read that follows it.
                    let read_version =
                        stages::timed(|t| &mut t.grv_us, trx.get_read_version()).await?;

                    let ikey = keyspace::intent_key(intent.intent_id);
                    let previous =
                        stages::timed(|t| &mut t.idem_read_us, trx.get(&ikey, false)).await?;
                    if let Some(prev) = previous {
                        let row: keyspace::IntentRow =
                            postcard::from_bytes(&prev).map_err(store_err("intent row decode"))?;
                        hooks.mark_closure_end();
                        // D29 clause 8. An annulled intent is the one replay
                        // that does not return its recorded outcome, because
                        // its recorded outcome is no longer true: the effects
                        // were reversed by a forward-written inverse. The row
                        // is still here — clause 9(c)'s GC interlock keeps it
                        // here, restamped from the annulment — so the replay
                        // applies nothing and is told what happened rather
                        // than being handed a `Committed` for value that is
                        // gone.
                        if row.finality == keyspace::IntentFinality::Annulled {
                            return Ok(IntentOutcome::Rejected {
                                reason: orrery_protocol::REASON_INTENT_ANNULLED,
                            });
                        }
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

                    // D29 clause 4, and the reason it is *here* rather than
                    // at admission: the quarantine is a fact about a durable
                    // row, and D11 keeps the serializable transaction the sole
                    // authority over those. An admission-time cache would be a
                    // second answer to a question this read answers exactly.
                    //
                    // **The cost, stated.** One point read per distinct
                    // account the intent names a balance for — at most two for
                    // the reference trade, and *zero* for an intent of nothing
                    // but `Ruleset`-opaque ops, which is what every existing
                    // load harness in this repository submits. The read
                    // registers a conflict range on `provisional/{account}`,
                    // so an intent spending an account's balance and a
                    // provisional commit into that same account are ordered by
                    // the resolver rather than racing. That is the correct
                    // ordering and it is not free: two intents naming one
                    // account's balances now conflict where previously only
                    // their item rows did.
                    let mut holds = std::collections::HashMap::new();
                    for (account, asset) in &named {
                        let row = match holds.entry(*account) {
                            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                            std::collections::hash_map::Entry::Vacant(e) => {
                                e.insert(read_provisional_row(&trx, *account).await?)
                            }
                        };
                        if row.holds_balance(*account, *asset).is_some() {
                            hooks.mark_closure_end();
                            return Ok(IntentOutcome::Rejected {
                                reason: orrery_protocol::REASON_PROVISIONAL_INPUT,
                            });
                        }
                    }

                    // D29 clause 9(b): the per-account outstanding cap, read
                    // in the same transaction that will extend it. An intent
                    // of purely opaque ops names no balance, so the row may
                    // not have been read above; reading it here is what makes
                    // the cap cover every provisional intent rather than only
                    // the ones that move currency.
                    let submitter_row = match provisional {
                        None => None,
                        Some(account) => {
                            let row = match holds.remove(&account) {
                                Some(row) => row,
                                None => read_provisional_row(&trx, account).await?,
                            };
                            if row.holds.len() >= orrery_protocol::PROVISIONAL_OUTSTANDING_CAP {
                                hooks.mark_closure_end();
                                return Ok(IntentOutcome::Rejected {
                                    reason: orrery_protocol::REASON_PROVISIONAL_CAP,
                                });
                            }
                            Some((account, row))
                        }
                    };

                    // Step 1: reads register conflict ranges. The tick is
                    // derived from the transaction's read version — a
                    // cluster-issued, strictly-ordered stand-in, honest in a
                    // way the gateway's old AtomicU64 counter never was (the
                    // commit version cannot be read inside the transaction).
                    // It is the version taken at the top of this closure: the
                    // binding caches it for the transaction's whole life, so
                    // re-reading it here only ever returned the same number.
                    let tick = orrery_protocol::Tick::new(read_version as u64);

                    // Step 2: use ids from the executor's already-durable
                    // grant. The harness mints one id per op; a linked
                    // Ruleset names the real allocation.

                    // Steps 1-2 continued: read every durable row the ops
                    // name and check them, staging the writes but applying
                    // none of them. This is where the item row's conflict
                    // range registers.
                    let plan = match plan_ops(&trx, &intent).await? {
                        PlanOutcome::Rejected(reason) => {
                            // A durable invariant refused this intent. Nothing
                            // has been staged and nothing is written — not the
                            // effects, and deliberately not the `intent/` row
                            // either. The transaction commits empty, which is
                            // free, and the refusal travels back as an
                            // ordinary outcome rather than an executor error.
                            hooks.mark_closure_end();
                            return Ok(IntentOutcome::Rejected { reason });
                        }
                        PlanOutcome::Planned(plan) => plan,
                    };

                    // Step 2c (D27 clauses (d) and (f), D28 clause (f)): the
                    // witness-epoch record and this intent's eligible vector,
                    // in the transaction the intent already runs.
                    //
                    // Both obligations land here rather than on a path of
                    // their own, and the placement is the point: the draw
                    // commitment must be durable before the cell-epoch admits
                    // anything, and the `epoch/` row must cost the intent p99
                    // no extra round trip. Writing it inside the *first*
                    // intent's own transaction satisfies both at once —
                    // atomicity means no intent's effects become durable
                    // before the commitment does, so the gateway cannot have
                    // chosen the draw key after seeing which attestations
                    // arrived. Every later intent in the epoch skips even the
                    // index read (`is_committed`), so the steady-state cost is
                    // exactly zero operations.
                    //
                    // **Above `apply_plan`, and that is not cosmetic.** This
                    // step can refuse (a draw key that was never this
                    // cell-epoch's, see `record_witness_epoch`), and `db.run`
                    // commits whatever a closure staged whether or not the
                    // closure decided it wanted it — the exact trap
                    // `PlannedWrite`'s doc names. Deciding before writing is
                    // what keeps a refused intent from banking its effects.
                    match record_witness_epoch(
                        &trx,
                        epochs.as_deref(),
                        bindings.as_deref(),
                        enforcement,
                        &intent,
                    )
                    .await?
                    {
                        EpochRecord::Refused(reason) => {
                            hooks.mark_closure_end();
                            return Ok(IntentOutcome::Rejected { reason });
                        }
                        EpochRecord::Recorded => {
                            epoch_row_seen.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        EpochRecord::NotApplicable => {}
                    }

                    // Step 3: apply the ops' ledger effects (see `apply_plan`).
                    apply_plan(&trx, &plan, &intent)?;

                    // Step 4 (§7 step 3): the outcome row, same transaction —
                    // and, on the low-population path, the hold row beside it.
                    // Both in this transaction, because a provisional commit
                    // whose hold did not land would be quarantined value that
                    // nothing quarantines.
                    let (outcome, finality, finalize_by_ms) = match submitter_row {
                        None => (
                            IntentOutcome::Committed { tick, minted },
                            keyspace::IntentFinality::Final,
                            0,
                        ),
                        Some((account, mut row)) => {
                            let committed_ms = now_ms();
                            let finalize_by = provisional::finalize_by(committed_ms);
                            row.holds.push(keyspace::ProvisionalHold {
                                intent_id: intent.intent_id,
                                account,
                                writes: provisional::provisional_writes(plan.writes()),
                                committed_ms,
                                finalize_by_ms: finalize_by,
                                // Present by construction: admission refuses a
                                // provisional intent that carries no
                                // commitment, because one that did could never
                                // be finalized and would only expire.
                                commitment: intent.evidence.ok_or_else(|| {
                                    FdbBindingError::new_custom_error(Box::new(IntentError::Store(
                                        "provisional intent reached the executor with no evidence commitment"
                                            .to_owned(),
                                    )))
                                })?,
                                subject: intent.issuer,
                            });
                            let encoded = postcard::to_stdvec(&row)
                                .map_err(store_err("provisional row encode"))?;
                            trx.set(&keyspace::provisional_key(account), &encoded);
                            (
                                IntentOutcome::Provisional {
                                    tick,
                                    minted,
                                    finalize_by,
                                },
                                keyspace::IntentFinality::Provisional,
                                finalize_by,
                            )
                        }
                    };
                    let row = keyspace::IntentRow {
                        outcome: outcome.clone(),
                        gc_deadline_ms: now_ms() + INTENT_ROW_RETENTION_MS,
                        finality,
                        finalize_by_ms,
                    };
                    let encoded =
                        postcard::to_stdvec(&row).map_err(store_err("intent row encode"))?;
                    trx.set(&ikey, &encoded);
                    // The closure is done; everything after this point inside
                    // `run_with_hooks` is the commit. Stamping here is what
                    // lets `on_commit_success` report the commit in
                    // microseconds — the hook's own `commit_duration_ms` is
                    // milliseconds, too coarse against a 6-8 ms median.
                    hooks.mark_closure_end();
                    Ok(outcome)
                }
            })
            .await;

        stages::trace(|t| {
            t.attempts += u64::from(attempts.load(std::sync::atomic::Ordering::Relaxed));
        });
        hooks.fold_into_trace();

        match result {
            Ok(outcome) => {
                // Only now is the `epoch/` row durable, so only now may the
                // cache stop re-reading the index. Doing this inside the
                // closure would mark a row durable that a later conflict retry
                // discarded, and every subsequent intent in the epoch would
                // then skip a write that never landed.
                //
                // Both conditions are load-bearing. `Committed` alone is not
                // enough — the replay path returns it without touching the row
                // — and the flag alone is not enough, because an attempt can
                // record and then lose its commit to a conflict.
                if matches!(
                    outcome,
                    IntentOutcome::Committed { .. } | IntentOutcome::Provisional { .. }
                ) && epoch_row_seen.load(std::sync::atomic::Ordering::Relaxed)
                {
                    if let Some(epochs) = self.witness_epochs.as_ref() {
                        if let Some(epoch) = epochs.resolve(intent_handle) {
                            epoch.mark_committed();
                        }
                    }
                }
                Ok(outcome)
            }
            Err(e) => Err(unwrap_binding_error(e)),
        }
    }
}

/// What one intent's pass over the witness-epoch record decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpochRecord {
    /// No epoch cache, or an intent naming a cell-epoch this gateway cannot
    /// resolve. Nothing was written and nothing is claimed.
    NotApplicable,
    /// The `epoch/` row was written or verified in this transaction, and this
    /// intent's eligible vector was recorded.
    Recorded,
    /// The intent must be refused with this wire reason.
    Refused(u16),
}

/// Make this cell-epoch's draw commitment durable, re-derive the required
/// subset under the **durable** draw key, and record the eligible vector the
/// intent was judged against.
///
/// # The one refusal, and why it is checked here rather than only at admission
///
/// The `epoch/` row already exists and carries a different draw key from the
/// one this gateway holds. That is the D26 sibling handover: another gateway
/// owned the shard when the epoch was accepted and minted the key every
/// outstanding co-signature was solicited under. The durable key wins.
///
/// Refusing on the *key* alone is not sufficient, and the gap is narrow enough
/// to be worth spelling out. When the mismatch is discovered the cache adopts
/// the durable key — but intents already admitted under the stale key are
/// still in flight, and by the time they arrive the cache and the row agree,
/// so a key comparison passes them. Their attestations satisfy the subset
/// drawn under the *old* key. So this re-derives `required(I)` from
/// `row.draw_key` and checks it against the attestations actually carried,
/// which is a check on the authoritative key no matter when the intent was
/// admitted. It costs K keyed hashes and only runs while the row is being
/// read at all — never on the steady-state path.
///
/// # Errors
///
/// Only FoundationDB and encoding failures; every semantic outcome is an
/// [`EpochRecord`].
async fn record_witness_epoch(
    trx: &foundationdb::Transaction,
    epochs: Option<&crate::witness_epoch::WitnessEpochAuthority>,
    bindings: Option<&dyn crate::gateway::BindingAuthority>,
    enforcement: super::AttestationEnforcement,
    intent: &Intent,
) -> Result<EpochRecord, FdbBindingError> {
    // No cache is the enforcement-off build, and an unresolvable handle is an
    // intent this executor was handed without admission having enforced
    // anything (the permissive validator, or an epoch that aged out between
    // admission and commit). Neither is an error and neither writes a row:
    // there is no eligible vector to record, because no draw was made.
    let (Some(epochs), Some(epoch)) = (epochs, epochs.and_then(|e| e.resolve(intent.cell_epoch.0)))
    else {
        return Ok(EpochRecord::NotApplicable);
    };

    // D27 clause (f) item 5, derived once and used for both the durable
    // re-derivation below and the recorded vector. It is derived here rather
    // than carried from admission because both sides read the same cache entry
    // *and* the same `owner(n)` resolver — see `recording_epochs`, which is
    // where the pairing is argued and where the residual (a binding that moved
    // between admission and commit, bounded by D31 clause (h)'s `T_stale`) is
    // named. Threading a vector through the executor seam would have widened
    // `IntentExecutor::execute`, which is implemented outside this crate.
    //
    // A resolver is *required* once epochs are: `recording_epochs` takes both,
    // so `None` here is only reachable through a hand-built executor. It is
    // read as D31 clause (f) reads every miss — nothing resolves, so nothing
    // is eligible — rather than as "skip the account half", because a recorded
    // vector wider than the admitted one is the audit going quietly wrong in
    // the direction that convicts nobody.
    static UNBOUND: crate::gateway::UnboundBindingAuthority =
        crate::gateway::UnboundBindingAuthority;
    let bindings = bindings.unwrap_or(&UNBOUND);
    // The issuer's account through the resolver rather than a session context,
    // which the executor seam does not carry. For both ops this cluster
    // interprets the ops themselves already name it, so the two derivations
    // agree wherever "party" means anything — `super::party_accounts` writes
    // that argument out.
    let parties = super::party_accounts(intent, bindings.owner(&intent.issuer));
    let eligible =
        super::eligible_after_party_exclusion(&epoch.snapshot.selected, intent, &parties, bindings);

    let handle_key = keyspace::epoch_handle_key(intent.cell_epoch.0);
    let row_key = keyspace::epoch_key(
        epoch.snapshot.grid,
        epoch.snapshot.cell,
        epoch.snapshot.epoch,
    );
    if !epoch.is_committed() {
        let fresh = |draw_key, draw_commit| keyspace::EpochRow {
            announcement: epoch.announcement.clone(),
            first_seen_ms: epoch.snapshot.first_seen_ms,
            draw_commit,
            draw_key,
            revealed_key: None,
            gc_deadline_ms: now_ms() + INTENT_ROW_RETENTION_MS,
        };
        // A handle index pointing at nothing is a torn write this executor
        // cannot have produced — both keys are set in one transaction — so it
        // is repaired by writing the pair, not trusted and not skipped.
        // Skipping would commit the intent with no recorded `E(I)` and under
        // an unverified key, which is the audit going quietly vacuous.
        let existing = match trx.get(&handle_key, false).await? {
            None => None,
            Some(existing_key) => trx.get(&existing_key, false).await?,
        };
        match existing {
            None => {
                let encoded = postcard::to_stdvec(&fresh(*epoch.draw_key(), epoch.draw_commit))
                    .map_err(store_err("witness epoch row encode"))?;
                trx.set(&row_key, &encoded);
                // The index carries the row's key rather than a copy of the
                // row, for the reason `lease_location_key` carries a cell:
                // one fact, one writer, and a second copy is a second thing
                // that can disagree.
                trx.set(&handle_key, &row_key);
            }
            Some(value) => {
                let row: keyspace::EpochRow =
                    postcard::from_bytes(&value).map_err(store_err("witness epoch row decode"))?;
                if &row.draw_key != epoch.draw_key() {
                    epochs.adopt_draw_key(intent.cell_epoch.0, row.draw_key, row.draw_commit);
                }
                // The authoritative check: the subset the *durable* key names
                // must be among the attestations this intent actually carries.
                // Under a matching key this re-proves what admission already
                // decided, for K hashes; under an adopted one it is the only
                // thing standing between a stale-key intent and a commit.
                //
                // **Disarmed in shadow** (D32 clause (d)). The adoption above
                // still runs — the cache must converge on the durable key in
                // every mode — but the re-proof does not, because the thing it
                // refuses is a below-quorum commit and shadow commits below
                // quorum deliberately. Left armed it would refuse at commit
                // what admission admitted, which is the control acting after
                // the mode said it would not.
                if enforcement.acts() {
                    for required in orrery_protocol::required_witnesses(
                        &row.draw_key,
                        intent.intent_id,
                        &eligible,
                    ) {
                        if !intent
                            .attestations
                            .iter()
                            .any(|attestation| attestation.witness == required)
                        {
                            return Ok(EpochRecord::Refused(
                                orrery_protocol::REASON_ATTESTATION_QUORUM,
                            ));
                        }
                    }
                }
            }
        }
    }

    // D32 clause (d)'s marker. The row is written in every mode that resolved
    // an epoch at all; what the mode decides is whether it claims the cluster
    // stood behind the quorum. `off` never reaches this line — it resolves no
    // epoch and returns `NotApplicable` above.
    let row = keyspace::AttestRow {
        epoch_handle: intent.cell_epoch.0,
        eligible,
        gc_deadline_ms: now_ms() + INTENT_ROW_RETENTION_MS,
        enforced: enforcement.acts(),
    };
    let encoded = postcard::to_stdvec(&row).map_err(store_err("attest row encode"))?;
    trx.set(&keyspace::attest_key(intent.intent_id), &encoded);
    Ok(EpochRecord::Recorded)
}

/// Retry-loop instrumentation for one intent's `db.run`.
///
/// `foundationdb`'s runner already offers exactly the observations this path
/// was missing ([`foundationdb::Database::run_with_hooks`]); the only thing
/// added here is microsecond resolution, which the hooks' own millisecond
/// durations do not have. The commit is timed from a stamp the closure takes
/// as its last act, so `commit_us` is the true gap between "the closure
/// finished" and "the commit resolved" — the phase that fsyncs on the same
/// md2 array as the journal.
///
/// One instance per `execute` call, so no field needs to be reset. All fields
/// are `Mutex`-guarded rather than atomic because `run_with_hooks` takes
/// `&H` across an await and therefore requires `Sync`; the locks are
/// uncontended by construction (one intent, one task) and never held across
/// an await.
#[derive(Debug, Default)]
struct IntentRunnerHooks {
    closure_end: std::sync::Mutex<Option<std::time::Instant>>,
    commit_us: std::sync::Mutex<u64>,
    backoff_us: std::sync::Mutex<u64>,
    last_err_code: std::sync::Mutex<u64>,
}

impl IntentRunnerHooks {
    /// Stamp the moment the closure finished, on every exit path it has.
    fn mark_closure_end(&self) {
        if let Ok(mut slot) = self.closure_end.lock() {
            *slot = Some(std::time::Instant::now());
        }
    }

    /// Fold what the retry loop observed into the task's intent trace.
    fn fold_into_trace(&self) {
        let commit_us = self.commit_us.lock().map(|v| *v).unwrap_or(0);
        let backoff_us = self.backoff_us.lock().map(|v| *v).unwrap_or(0);
        let code = self.last_err_code.lock().map(|v| *v).unwrap_or(0);
        stages::trace(|t| {
            t.commit_us += commit_us;
            t.backoff_us += backoff_us;
            if code != 0 {
                t.last_err_code = code;
            }
        });
    }

    fn note_error(&self, code: i32) {
        if let Ok(mut slot) = self.last_err_code.lock() {
            *slot = code.unsigned_abs().into();
        }
    }
}

impl foundationdb::RunnerHooks for IntentRunnerHooks {
    async fn on_commit_error(
        &self,
        err: &foundationdb::TransactionCommitError,
    ) -> foundationdb::FdbResult<()> {
        self.note_error(err.code());
        Ok(())
    }

    fn on_closure_error(&self, err: &foundationdb::FdbError) {
        self.note_error(err.code());
    }

    fn on_error_duration(&self, duration_ms: u64) {
        if let Ok(mut slot) = self.backoff_us.lock() {
            *slot += duration_ms * 1_000;
        }
    }

    fn on_commit_success(
        &self,
        _committed: &foundationdb::TransactionCommitted,
        _commit_duration_ms: u64,
    ) {
        let started = self.closure_end.lock().ok().and_then(|slot| *slot);
        if let (Some(started), Ok(mut slot)) = (started, self.commit_us.lock()) {
            *slot += started.elapsed().as_micros() as u64;
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

/// What [`plan_ops`] decided.
enum PlanOutcome {
    /// Every op's durable checks passed; these are the writes they earned.
    Planned(IntentPlan),
    /// A durable invariant refused the intent, with the wire reason code.
    Rejected(u16),
}

/// Read and check every op the cluster interprets, staging the writes.
///
/// This is §7 steps 1 and 2 for the whole intent. Two op ids are interpreted
/// and every other is `Ruleset`-opaque and a no-op by design
/// (docs/08-persistence.md §2.2: the wire type only carries the op id and its
/// encoded arguments):
///
/// - [`LEDGER_CREDIT_OP`] — `account ‖ asset ‖ delta` (24 bytes), a blind
///   little-endian `MutationType::Add` on `ledger/bal/{account}/{asset}`. It
///   reads nothing, and that is exactly why it cannot be double-spent *or*
///   prevented from minting value from nothing.
/// - [`LEDGER_ITEM_TRANSFER_OP`] — the §7 trade. Reads
///   `ledger/item/{item_uid}` and the buyer's balance, checks owner and
///   sufficiency, and stages the ownership move plus both balance adds.
///
/// **The reads are the point.** `trx.get` inside a serializable transaction
/// registers the key's read conflict range, so any concurrent commit that
/// writes `ledger/item/{item_uid}` between this transaction's read version and
/// its commit aborts it with `not_committed`. `db.run` then re-runs the whole
/// closure, the item row is read again, and the check refuses honestly against
/// the new owner. Two commits over one item row cannot both happen — which is
/// D11's anti-duplication mechanism, stated as one `get`.
async fn plan_ops(
    trx: &foundationdb::Transaction,
    intent: &Intent,
) -> Result<PlanOutcome, FdbBindingError> {
    let mut plan = IntentPlan::default();
    for op in &intent.ops {
        let verdict = match op.op {
            LEDGER_CREDIT_OP => {
                if op.args.len() != 24 {
                    OpsVerdict::Rejected(orrery_protocol::REASON_MALFORMED_OP)
                } else {
                    let field = |i: usize| {
                        u64::from_le_bytes(op.args[i..i + 8].try_into().expect("slice len"))
                    };
                    let account = AccountId::new(field(0));
                    let asset = AssetId::new(field(8));
                    let delta = i64::from_le_bytes(op.args[16..24].try_into().expect("slice len"));
                    // Deliberately *not* read: a blind `Add` has no read
                    // conflict range, which is what keeps independent credits
                    // from serializing on each other. The plan's view of this
                    // balance stays unseeded, so a later op in the same intent
                    // that needs the value reads it then.
                    plan.credit(account, asset, delta);
                    OpsVerdict::Applied
                }
            }
            LEDGER_ITEM_TRANSFER_OP => match ItemTransferArgs::decode(&op.args) {
                Err(_) => OpsVerdict::Rejected(orrery_protocol::REASON_MALFORMED_OP),
                Ok(transfer) => {
                    if !plan.has_item(transfer.item) {
                        let row = read_item_row(trx, transfer.item).await?;
                        plan.load_item(transfer.item, row);
                    }
                    if !plan.has_balance(transfer.buyer, transfer.asset) {
                        let value = read_balance(trx, transfer.buyer, transfer.asset).await?;
                        plan.load_balance(transfer.buyer, transfer.asset, value);
                    }
                    plan.transfer(&transfer)
                }
            },
            _ => OpsVerdict::Applied,
        };
        if let OpsVerdict::Rejected(reason) = verdict {
            return Ok(PlanOutcome::Rejected(reason));
        }
    }
    Ok(PlanOutcome::Planned(plan))
}

/// Apply a checked plan's writes, then bank the trade receipt.
///
/// Called only after [`plan_ops`] has accepted every op, so nothing here can
/// fail a durable check — which is the property that makes an early return
/// impossible past this line, and therefore makes a partially applied intent
/// impossible too.
fn apply_plan(
    trx: &foundationdb::Transaction,
    plan: &IntentPlan,
    intent: &Intent,
) -> Result<(), FdbBindingError> {
    for write in plan.writes() {
        match write {
            PlannedWrite::BalanceAdd {
                account,
                asset,
                delta,
            } => {
                let key = keyspace::ledger_bal_key(*account, *asset);
                // 16-byte little-endian delta so `Add` extends/keeps the i128
                // width. Sign-extended, not zero-padded: a debit is a negative
                // delta and `Add` is two's-complement, so the high bytes must
                // carry the sign or `-500` would arrive as a very large
                // positive number.
                let param = i128::from(*delta).to_le_bytes();
                trx.atomic_op(&key, &param, MutationType::Add);
            }
            PlannedWrite::ItemOwner { item, row } => {
                let key = keyspace::ledger_item_key(*item);
                let encoded = postcard::to_stdvec(row).map_err(store_err("item row encode"))?;
                trx.set(&key, &encoded);
            }
        }
    }
    if let Some(receipt) = plan.receipt(intent) {
        // `ledger/receipt/{versionstamp}`: the key's 10-byte placeholder is
        // replaced with this transaction's commit versionstamp, so the audit
        // trail is ordered by commit order itself and needs no clock. One per
        // intent — every versionstamped write in a transaction gets the same
        // 10 bytes, so a second would be the same key written twice.
        let key = keyspace::ledger_receipt_versionstamped_key();
        let encoded = postcard::to_stdvec(&receipt).map_err(store_err("receipt row encode"))?;
        trx.atomic_op(&key, &encoded, MutationType::SetVersionstampedKey);
    }
    Ok(())
}

/// Read and decode `provisional/{account}` inside `trx`; an absent row is an
/// account with nothing outstanding.
///
/// **Not a snapshot read.** The conflict range this registers is the point:
/// clause 4's quarantine has to hold against a provisional commit landing
/// concurrently, and a snapshot read would let an intent observe an empty hold
/// set a microsecond before the commit that fills it.
async fn read_provisional_row(
    trx: &foundationdb::Transaction,
    account: AccountId,
) -> Result<keyspace::ProvisionalRow, FdbBindingError> {
    let key = keyspace::provisional_key(account);
    let Some(value) = trx.get(&key, false).await? else {
        return Ok(keyspace::ProvisionalRow::default());
    };
    postcard::from_bytes(&value).map_err(store_err("provisional row decode"))
}

/// D29 clauses 7 and 8 against FoundationDB.
///
/// Each method is one serializable transaction. [`Self::annul`] being one
/// transaction is the load-bearing part and the thing only this tier can
/// prove: the inverse writes, the finality flip, the restamped GC deadline,
/// the compensating receipt and the hold release either all become durable or
/// none of them do. An annulment that lost half of itself to a crash would
/// leave a row saying `Annulled` over ledger effects that were never reversed,
/// which is worse than either outcome it sits between.
#[async_trait::async_trait]
impl ProvisionalStore for FdbIntentExecutor {
    async fn outstanding(&self) -> Result<Vec<keyspace::ProvisionalHold>, IntentError> {
        let result: Result<Vec<keyspace::ProvisionalHold>, FdbBindingError> = self
            .db
            .run(|trx, _maybe_committed| async move {
                // One range read over a family that is **empty in the steady
                // state**: a row exists only while an account has unfinalized
                // provisional work, and `release_hold`'s FDB counterpart
                // clears the row rather than leaving a zero-length one. So the
                // sweep costs one operation proportional to outstanding work,
                // not to history — which is what makes running it at a
                // sampling rate of 1 affordable.
                let mut holds = Vec::new();
                let start = keyspace::provisional_range_start();
                let end = keyspace::provisional_range_end();
                let mut stream = trx.get_ranges_keyvalues(
                    foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                        end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                        ..foundationdb::RangeOption::default()
                    },
                    false,
                );
                while let Some(kv) = stream.try_next().await? {
                    let row: keyspace::ProvisionalRow = postcard::from_bytes(kv.value())
                        .map_err(store_err("provisional row decode"))?;
                    holds.extend(row.holds);
                }
                Ok(holds)
            })
            .await;
        let mut holds = result.map_err(unwrap_binding_error)?;
        // Oldest first (D29 clause 7), with `intent_id` breaking ties so two
        // intents committed in the same millisecond sweep in a defined order.
        holds.sort_by_key(|hold| (hold.committed_ms, hold.intent_id));
        Ok(holds)
    }

    async fn finalize(&self, hold: &keyspace::ProvisionalHold) -> Result<(), IntentError> {
        let hold = hold.clone();
        let result: Result<(), FdbBindingError> = self
            .db
            .run(|trx, _maybe_committed| {
                let hold = hold.clone();
                async move {
                    let ikey = keyspace::intent_key(hold.intent_id);
                    let Some(value) = trx.get(&ikey, false).await? else {
                        // The row is gone. Under clause 9(c) that cannot
                        // happen to a provisional row — it is not sweepable
                        // while unresolved — so this is a torn state rather
                        // than a race, and the honest response is to release
                        // the hold and stop rather than to write a finality
                        // onto a row that is not there.
                        release_hold(&trx, &hold).await?;
                        return Ok(());
                    };
                    let mut row: keyspace::IntentRow =
                        postcard::from_bytes(&value).map_err(store_err("intent row decode"))?;
                    // Only a provisional row is promotable. A row that has
                    // already been annulled by a concurrent sweep stays
                    // annulled: reversal is not undone by a later agreeing
                    // replay, because the value is already back and the player
                    // has already been told.
                    if row.finality == keyspace::IntentFinality::Provisional {
                        row.finality = keyspace::IntentFinality::Final;
                        row.finalize_by_ms = 0;
                        let encoded =
                            postcard::to_stdvec(&row).map_err(store_err("intent row encode"))?;
                        trx.set(&ikey, &encoded);
                    }
                    release_hold(&trx, &hold).await?;
                    Ok(())
                }
            })
            .await;
        result.map_err(unwrap_binding_error)
    }

    async fn annul(&self, hold: &keyspace::ProvisionalHold) -> Result<(), IntentError> {
        let hold = hold.clone();
        let result: Result<(), FdbBindingError> = self
            .db
            .run(|trx, _maybe_committed| {
                let hold = hold.clone();
                async move {
                    let ikey = keyspace::intent_key(hold.intent_id);
                    let previous = trx.get(&ikey, false).await?;
                    let Some(value) = previous else {
                        release_hold(&trx, &hold).await?;
                        return Ok(());
                    };
                    let mut row: keyspace::IntentRow =
                        postcard::from_bytes(&value).map_err(store_err("intent row decode"))?;
                    // Idempotent by the row, not by the caller. Two sweeps
                    // racing on one hold must not apply the inverse twice, and
                    // the read above registers the conflict range that makes
                    // at most one of them commit.
                    if row.finality != keyspace::IntentFinality::Provisional {
                        release_hold(&trx, &hold).await?;
                        return Ok(());
                    }

                    // The forward-written inverse. `MutationType::Add` with a
                    // negated delta, in the same 16-byte sign-extended
                    // little-endian form `apply_plan` writes, so the reversal
                    // is arithmetically the commit's mirror and not a
                    // recomputation of what the balance "should" be.
                    for write in &hold.writes {
                        let key = keyspace::ledger_bal_key(write.account, write.asset);
                        let param = i128::from(write.delta).wrapping_neg().to_le_bytes();
                        trx.atomic_op(&key, &param, MutationType::Add);
                    }

                    // The compensating receipt. Appended, never substituted:
                    // `ledger/receipt/{versionstamp}` is a strictly-ordered
                    // history, and a reader of it sees the commit and then the
                    // reversal — which is what an auditor needs and what a
                    // player owed an explanation needs.
                    let receipt = keyspace::ReceiptRow {
                        intent_id: hold.intent_id,
                        parties: hold.writes.iter().map(|write| write.account).collect(),
                        ops: Vec::new(),
                    };
                    let rkey = keyspace::ledger_receipt_versionstamped_key();
                    let encoded =
                        postcard::to_stdvec(&receipt).map_err(store_err("receipt row encode"))?;
                    trx.atomic_op(&rkey, &encoded, MutationType::SetVersionstampedKey);

                    row.finality = keyspace::IntentFinality::Annulled;
                    row.finalize_by_ms = 0;
                    // Restamped from the annulment, so an annulled row retains
                    // for an hour from *this* moment rather than from the
                    // commit (D29 clause 9(c)). That is what keeps it able to
                    // answer a replay out of a client's offline queue, whose
                    // TTL docs/08 §6 already requires to be shorter than the
                    // retention.
                    row.gc_deadline_ms = now_ms() + INTENT_ROW_RETENTION_MS;
                    let encoded =
                        postcard::to_stdvec(&row).map_err(store_err("intent row encode"))?;
                    trx.set(&ikey, &encoded);

                    release_hold(&trx, &hold).await?;
                    Ok(())
                }
            })
            .await;
        result.map_err(unwrap_binding_error)
    }
}

/// Drop `hold` from its account's `provisional/{account}` row, clearing the
/// row entirely once nothing is outstanding.
///
/// Clearing rather than storing an empty vector keeps
/// [`ProvisionalStore::outstanding`]'s range scan proportional to outstanding
/// work instead of to the set of accounts that have ever committed
/// provisionally.
async fn release_hold(
    trx: &foundationdb::Transaction,
    hold: &keyspace::ProvisionalHold,
) -> Result<(), FdbBindingError> {
    let key = keyspace::provisional_key(hold.account);
    let Some(value) = trx.get(&key, false).await? else {
        return Ok(());
    };
    let mut row: keyspace::ProvisionalRow =
        postcard::from_bytes(&value).map_err(store_err("provisional row decode"))?;
    row.holds.retain(|held| held.intent_id != hold.intent_id);
    if row.holds.is_empty() {
        trx.clear(&key);
    } else {
        let encoded = postcard::to_stdvec(&row).map_err(store_err("provisional row encode"))?;
        trx.set(&key, &encoded);
    }
    Ok(())
}

/// Read and decode `ledger/item/{item}` inside `trx`.
///
/// **The read that makes the anti-dupe invariant work.** It registers the
/// row's serializable read conflict range; everything else about the transfer
/// is bookkeeping around it.
async fn read_item_row(
    trx: &foundationdb::Transaction,
    item: orrery_protocol::ItemUid,
) -> Result<Option<keyspace::ItemRow>, FdbBindingError> {
    let key = keyspace::ledger_item_key(item);
    let Some(value) = trx.get(&key, false).await? else {
        return Ok(None);
    };
    let row: keyspace::ItemRow =
        postcard::from_bytes(&value).map_err(store_err("item row decode"))?;
    Ok(Some(row))
}

/// Read `ledger/bal/{account}/{asset}` inside `trx`, as the little-endian
/// integer `MutationType::Add` maintains it. An absent row is zero.
///
/// Values are written as 16-byte little-endian `i128`s by [`apply_plan`], and
/// FDB's `Add` zero-extends a shorter stored value to the parameter's width,
/// so a row this cluster wrote is always 16 bytes. A shorter one — a row from
/// an older writer, or a seeded fixture — is zero-extended here to match what
/// `Add` would do to it, which keeps the value this read sees and the value a
/// subsequent `Add` produces in agreement.
async fn read_balance(
    trx: &foundationdb::Transaction,
    account: AccountId,
    asset: AssetId,
) -> Result<i128, FdbBindingError> {
    let key = keyspace::ledger_bal_key(account, asset);
    let Some(value) = trx.get(&key, false).await? else {
        return Ok(0);
    };
    let mut buf = [0u8; 16];
    let n = value.len().min(16);
    buf[..n].copy_from_slice(&value[..n]);
    Ok(i128::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    //! The `fdb` tier for D29's low-population path.
    //!
    //! These are the assertions the in-memory tier cannot make. That tier
    //! serialises on a mutex, so it proves the *logic* of the quarantine, the
    //! sweep and the inverse, and proves nothing about the property those three
    //! actually depend on: that each of them is **one serializable
    //! transaction**, and that the reads inside it register conflict ranges a
    //! concurrent commit collides with.
    //!
    //! Each test opens with the same guard every `fdb`-tier test in this
    //! repository opens with — `eprintln!("skipping: …")` and return — which is
    //! right for a developer's `cargo test` and a trap for CI;
    //! `scripts/fdb-tests.sh` is what turns it back into a real gate, failing
    //! on any `skipping:` line and asserting a floor on tests executed.
    //!
    //! **One account per test.** libtest runs these concurrently and every one
    //! of them writes a `provisional/{account}` row and one balance row.
    //! Neither key carries a grid discriminator (they are flat ledger keys), so
    //! the isolation has to come from the id — and a shared account made these
    //! four a race whose failures read as bugs in the mechanism rather than in
    //! the fixture.

    use super::*;
    use crate::intent::{IntentExecutor, LEDGER_CREDIT_OP};
    use orrery_protocol::{
        CellEpoch, ChainHash, EvidenceCommitment, IntentOp, PersistId, RulesetId, Tick,
    };

    /// This file's own grid, so its `pid/next` counter never contends with
    /// another suite's on the shared dev cluster.
    const GRID: u32 = 9602;
    const GOLD: u64 = 0x9602_0000_0000_00f0;

    fn fdb_cluster_file() -> Option<String> {
        if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
            return Some(path);
        }
        let mut dir = std::env::current_dir().ok()?;
        loop {
            let candidate = dir.join(".fdb-dev/fdb.cluster");
            if candidate.exists() {
                return Some(candidate.display().to_string());
            }
            if !dir.pop() {
                return None;
            }
        }
    }

    /// D32 clause (c)'s runtime lever reaches the *commit* half of control C1,
    /// not only the admission half.
    ///
    /// The regression this pins is the one #222's gate leg exists to catch,
    /// and it is invisible from the admission side: with a frozen mode here, a
    /// validator demoted `Required -> Shadow` would go on refusing at commit
    /// what it now admits at admission — the control acting under a mode that
    /// says it does not. Sharing the validator's own cell is what makes one
    /// write move both halves, so this asserts the *identity* of the cells
    /// rather than a mode either of them happened to start in.
    #[test]
    fn the_executor_and_the_validator_share_one_posture_cell() {
        let Some(cluster) = fdb_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
            return;
        };
        let epochs = Arc::new(crate::witness_epoch::WitnessEpochAuthority::new([]));
        let bindings: crate::gateway::SharedBindingAuthority =
            Arc::new(crate::gateway::SnapshotBindingAuthority::from_bindings([]));
        let validator = crate::intent::BaselineIntentValidator::shadow(
            Arc::clone(&epochs),
            Arc::new(crate::gateway::DenyAllInterestAuthority),
            Arc::clone(&bindings),
        );
        let executor = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
            .expect("connect")
            .tracking_posture(epochs, bindings, validator.posture());

        assert_eq!(
            executor.attestation_posture().get(),
            crate::intent::AttestationEnforcement::Shadow
        );

        // The operator's promotion, written through the validator's handle —
        // the seam a `ramp/attestation_quorum` poller holds.
        validator
            .posture()
            .set(crate::intent::AttestationEnforcement::Required);
        assert_eq!(
            executor.attestation_posture().get(),
            crate::intent::AttestationEnforcement::Required,
            "a promotion that reached only the validator would leave the \
             commit-time re-proof disarmed under `required`"
        );

        // And clause (f)'s trip, which is the direction that matters: the
        // demotion has to disarm the re-proof, or auto-suspend suspends half a
        // control.
        assert!(validator.posture().auto_suspend());
        assert_eq!(
            executor.attestation_posture().get(),
            crate::intent::AttestationEnforcement::Shadow
        );
    }

    /// The two pinned constructors keep their meaning: each installs a cell of
    /// its own, so a deployment with no runtime lever is unchanged.
    #[test]
    fn the_pinned_constructors_install_private_cells() {
        let Some(cluster) = fdb_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
            return;
        };
        let epochs = Arc::new(crate::witness_epoch::WitnessEpochAuthority::new([]));
        let bindings: crate::gateway::SharedBindingAuthority =
            Arc::new(crate::gateway::SnapshotBindingAuthority::from_bindings([]));
        let recording = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
            .expect("connect")
            .recording_epochs(Arc::clone(&epochs), Arc::clone(&bindings));
        let shadowing = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
            .expect("connect")
            .shadowing_epochs(epochs, bindings);

        assert_eq!(
            recording.attestation_posture().get(),
            crate::intent::AttestationEnforcement::Required
        );
        assert_eq!(
            shadowing.attestation_posture().get(),
            crate::intent::AttestationEnforcement::Shadow
        );
        shadowing
            .attestation_posture()
            .set(crate::intent::AttestationEnforcement::Off);
        assert_eq!(
            recording.attestation_posture().get(),
            crate::intent::AttestationEnforcement::Required,
            "two pinned executors must not share one cell"
        );
    }

    fn commitment() -> EvidenceCommitment {
        EvidenceCommitment {
            ruleset: RulesetId {
                version: 1,
                digest: [7; 32],
            },
            entity: PersistId::new(11),
            window_start: Tick::new(100),
            window_end: Tick::new(160),
            t0_claim_hash: [9; 32],
            log_head: ChainHash([5; 32]),
        }
    }

    fn intent_with(id: u128, ops: Vec<IntentOp>) -> Intent {
        let mut seed = [0u8; 32];
        seed[0] = 42;
        let key = iroh_base::SecretKey::from_bytes(&seed);
        let mut intent = Intent {
            intent_id: id,
            issuer: key.public(),
            cell_epoch: CellEpoch::new(0),
            ops,
            attestations: Vec::new(),
            evidence: Some(commitment()),
            signature: key.sign(b"placeholder"),
        };
        intent.sign(&key);
        intent
    }

    /// One self-credit of `delta` gold into `account`.
    fn credit(account: u64, id: u128, delta: i64) -> Intent {
        let mut args = Vec::with_capacity(24);
        args.extend_from_slice(&account.to_le_bytes());
        args.extend_from_slice(&GOLD.to_le_bytes());
        args.extend_from_slice(&delta.to_le_bytes());
        intent_with(
            id,
            vec![IntentOp {
                op: LEDGER_CREDIT_OP,
                args: bytes::Bytes::from(args),
            }],
        )
    }

    /// A `Ruleset`-opaque op: provisional-eligible, and it names no ledger row,
    /// so the quarantine — which is stricter per balance row than the cap is
    /// per account — is not what refuses it.
    fn opaque(id: u128) -> Intent {
        intent_with(
            id,
            vec![IntentOp {
                op: 100,
                args: bytes::Bytes::from_static(b"opaque"),
            }],
        )
    }

    /// Clear every row one test writes, so a rerun against the same cluster
    /// starts from the same state. The balance is *cleared* rather than
    /// negated: `MutationType::Add` has no identity a test can compute without
    /// reading, and reading it back to subtract it would make the fixture
    /// depend on the thing under test.
    async fn reset(db: &Database, account: u64, ids: &[u128]) {
        let ids = ids.to_vec();
        db.run(|trx, _| {
            let ids = ids.clone();
            async move {
                for id in &ids {
                    trx.clear(&keyspace::intent_key(*id));
                }
                trx.clear(&keyspace::provisional_key(AccountId::new(account)));
                trx.clear(&keyspace::ledger_bal_key(
                    AccountId::new(account),
                    AssetId::new(GOLD),
                ));
                Ok(())
            }
        })
        .await
        .expect("reset");
    }

    async fn balance(db: &Database, account: u64) -> i128 {
        db.run(|trx, _| async move {
            read_balance(&trx, AccountId::new(account), AssetId::new(GOLD)).await
        })
        .await
        .expect("balance")
    }

    async fn intent_row(db: &Database, id: u128) -> Option<keyspace::IntentRow> {
        db.run(|trx, _| async move {
            let Some(value) = trx.get(&keyspace::intent_key(id), false).await? else {
                return Ok(None);
            };
            let row: keyspace::IntentRow =
                postcard::from_bytes(&value).map_err(store_err("intent row decode"))?;
            Ok(Some(row))
        })
        .await
        .expect("intent row")
    }

    /// This account's outstanding holds, filtered out of the family-wide scan
    /// so concurrent tests' rows are invisible to each other.
    async fn holds_of(exec: &FdbIntentExecutor, account: u64) -> Vec<keyspace::ProvisionalHold> {
        exec.outstanding()
            .await
            .expect("outstanding")
            .into_iter()
            .filter(|hold| hold.account == AccountId::new(account))
            .collect()
    }

    #[tokio::test]
    async fn a_provisional_commit_writes_the_row_the_hold_and_the_effect_together() {
        const ALICE: u64 = 0x9602_0000_0000_0001;
        let Some(cluster) = fdb_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
            return;
        };
        let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID)).expect("connect");
        let db = Arc::clone(exec.database());
        reset(&db, ALICE, &[0xd29_0001]).await;

        let outcome = exec
            .execute_provisional(&credit(ALICE, 0xd29_0001, 100), AccountId::new(ALICE))
            .await
            .expect("no executor error");
        let IntentOutcome::Provisional { finalize_by, .. } = outcome else {
            panic!("expected Provisional, got {outcome:?}");
        };

        // All three in one transaction, so all three are here or none is.
        assert_eq!(balance(&db, ALICE).await, 100);
        let row = intent_row(&db, 0xd29_0001).await.expect("durable row");
        assert_eq!(row.finality, keyspace::IntentFinality::Provisional);
        assert_eq!(row.finalize_by_ms, finalize_by);
        assert!(
            !keyspace::sweepable(&row, u64::MAX),
            "D29 clause 9(c): a provisional row is not sweepable at any clock"
        );
        let holds = holds_of(&exec, ALICE).await;
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].intent_id, 0xd29_0001);
        assert_eq!(holds[0].commitment, commitment());
        assert_eq!(holds[0].writes.len(), 1);
        assert_eq!(holds[0].writes[0].delta, 100);

        exec.annul(&holds[0]).await.expect("annul");
        reset(&db, ALICE, &[0xd29_0001]).await;
    }

    #[tokio::test]
    async fn a_held_balance_row_is_an_input_to_nothing() {
        // D29 clause 4 against the durable rows, which is where it has to hold:
        // the refusal is decided by a `get` inside the intent's own
        // serializable transaction, so it registers the conflict range a
        // concurrent provisional commit collides with rather than consulting a
        // cache a commit can outrun.
        const BOB: u64 = 0x9602_0000_0000_0002;
        let Some(cluster) = fdb_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
            return;
        };
        let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID)).expect("connect");
        let db = Arc::clone(exec.database());
        reset(&db, BOB, &[0xd29_0002, 0xd29_0003]).await;

        exec.execute_provisional(&credit(BOB, 0xd29_0002, 100), AccountId::new(BOB))
            .await
            .expect("no executor error");
        assert_eq!(
            exec.execute(&credit(BOB, 0xd29_0003, 5))
                .await
                .expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_PROVISIONAL_INPUT
            },
        );
        assert_eq!(balance(&db, BOB).await, 100, "the refusal applied nothing");
        assert!(
            intent_row(&db, 0xd29_0003).await.is_none(),
            "and burned no idempotency row, so the same id works after finalization"
        );

        let holds = holds_of(&exec, BOB).await;
        exec.finalize(&holds[0]).await.expect("finalize");
        assert!(matches!(
            exec.execute(&credit(BOB, 0xd29_0003, 5))
                .await
                .expect("no executor error"),
            IntentOutcome::Committed { .. }
        ));
        assert_eq!(balance(&db, BOB).await, 105);
        assert_eq!(
            intent_row(&db, 0xd29_0002).await.expect("row").finality,
            keyspace::IntentFinality::Final
        );
        reset(&db, BOB, &[0xd29_0002, 0xd29_0003]).await;
    }

    #[tokio::test]
    async fn annulment_is_one_transaction_and_a_replay_of_it_reapplies_nothing() {
        const CAROL: u64 = 0x9602_0000_0000_0003;
        let Some(cluster) = fdb_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
            return;
        };
        let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID)).expect("connect");
        let db = Arc::clone(exec.database());
        reset(&db, CAROL, &[0xd29_0004]).await;

        exec.execute_provisional(&credit(CAROL, 0xd29_0004, 100), AccountId::new(CAROL))
            .await
            .expect("no executor error");
        let hold = holds_of(&exec, CAROL).await.remove(0);
        exec.annul(&hold).await.expect("annul");

        assert_eq!(
            balance(&db, CAROL).await,
            0,
            "the forward-written inverse: +100 then -100, never a deletion"
        );
        let row = intent_row(&db, 0xd29_0004).await.expect("row");
        assert_eq!(row.finality, keyspace::IntentFinality::Annulled);
        assert!(
            row.gc_deadline_ms >= now_ms(),
            "restamped from the annulment, so it outlives a client offline queue"
        );
        assert!(!keyspace::sweepable(&row, row.gc_deadline_ms - 1));

        // A replay is answered, not re-applied. The row survives its reversal
        // precisely so this question has an answer.
        assert_eq!(
            exec.execute_provisional(&credit(CAROL, 0xd29_0004, 100), AccountId::new(CAROL))
                .await
                .expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_INTENT_ANNULLED
            }
        );
        assert_eq!(balance(&db, CAROL).await, 0);

        // Annulling twice is idempotent by the row, which is what makes two
        // sweeps racing on one hold safe.
        exec.annul(&hold).await.expect("second annul");
        assert_eq!(balance(&db, CAROL).await, 0);
        reset(&db, CAROL, &[0xd29_0004]).await;
    }

    #[tokio::test]
    async fn the_per_account_cap_is_enforced_against_the_durable_row() {
        const DAVE: u64 = 0x9602_0000_0000_0004;
        let Some(cluster) = fdb_cluster_file() else {
            eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
            return;
        };
        let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID)).expect("connect");
        let db = Arc::clone(exec.database());
        let base = 0xd29_1000_u128;
        let cap = orrery_protocol::PROVISIONAL_OUTSTANDING_CAP as u128;
        let ids: Vec<u128> = (0..=cap).map(|i| base + i).collect();
        reset(&db, DAVE, &ids).await;

        for id in ids.iter().take(cap as usize) {
            assert!(matches!(
                exec.execute_provisional(&opaque(*id), AccountId::new(DAVE))
                    .await
                    .expect("no executor error"),
                IntentOutcome::Provisional { .. }
            ));
        }
        assert_eq!(
            exec.execute_provisional(&opaque(base + cap), AccountId::new(DAVE))
                .await
                .expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_PROVISIONAL_CAP
            },
            "the cluster refuses new provisional intents rather than annulling old ones"
        );
        assert!(
            intent_row(&db, base + cap).await.is_none(),
            "a refused intent is not a durable fact"
        );

        for hold in holds_of(&exec, DAVE).await {
            exec.annul(&hold).await.expect("annul");
        }
        reset(&db, DAVE, &ids).await;
    }
}
