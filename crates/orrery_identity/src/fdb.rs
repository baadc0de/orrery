//! The durable account store: D31's `d` family, written by this service alone.
//!
//! # One transaction for bindings; five row kinds overall
//!
//! Every mutation here is a single `db.run` closure that stages `da`, `db`,
//! `dh` — and D36's `dw` window — together. D31 clause (b) is explicit that
//! this is the load-bearing half, not the byte layout:
//!
//! > A reverse index maintained in a second transaction has a window in which
//! > `db` names an account that `da` no longer binds — and under clause (f) a
//! > *wrong* answer is worse than a miss, because a miss excludes and a wrong
//! > answer admits.
//!
//! The `dh` append rides the same transaction for the reason D31's resolved
//! question 2 gives: [`AccountRow::binding_event_count`] and
//! [`AccountRow::first_event_ms`] are a **write-time fold** of the log, so
//! expiry can be a pure range delete with no read-modify-write. A fold
//! maintained in a different transaction than the row it folds is a counter
//! that drifts.
//!
//! FoundationDB gives all-or-nothing for free once the writes are staged
//! together, and that is exactly what
//! [`tests::binding_writes_are_all_or_nothing`] proves: a closure that stages
//! everything and then fails leaves *no* row changed — `da`, `db`, `dh`, and
//! since D36 the window too, whose refusal and append live in this same
//! transaction. Split the same work across two transactions and the first
//! one's rows survive the second one's failure, which is the observable form
//! of the window clause (b) forbids.
//!
//! # What is *not* here
//!
//! `BindingAuthority` (`orrery_persistd::gateway`) is deliberately not
//! implemented on this type. Its contract is synchronous and non-blocking —
//! "a tier-3 miss enqueues an asynchronous fill and answers `None` in the same
//! instant" — and a point read against FoundationDB is neither. D31 clause (e)
//! puts the durable read in tier 3, *off* the admission path, and an
//! `impl BindingAuthority for FdbAccountStore` would be a durable read on it
//! wearing the signature of a hash probe. [`crate::MemAccountStore`] implements
//! it because a lock-guarded map probe genuinely is tier 2.

use crate::store::{AccountStore, BindOutcome, CooldownEntry, CooldownRecord, IdentityError};
use crate::window::{admit_binding_event, rate_limited};
use async_trait::async_trait;
use foundationdb::options::MutationType;
use foundationdb::{Database, FdbBindingError};
use futures::TryStreamExt;
use orrery_persistd::adjudication::{
    strike_account_range_end, strike_account_range_start, StrikeRow,
};
use orrery_persistd::keyspace::{
    self, AccountRow, BindKind, BindingHistoryRow, BindingRow, MAX_BOUND_NODES_PER_ACCOUNT,
};
use orrery_protocol::AccountId;
use orrery_protocol::NodeId;
use std::sync::Arc;

/// Read-only identity view of the executor-owned `ya` strike family.
#[derive(Clone)]
pub struct FdbStrikeRowSource {
    db: Arc<Database>,
}

impl std::fmt::Debug for FdbStrikeRowSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FdbStrikeRowSource").finish_non_exhaustive()
    }
}

impl FdbStrikeRowSource {
    /// Open a bounded read-only source against `cluster_file`.
    pub fn connect(cluster_file: &str) -> Result<Self, IdentityError> {
        let context = orrery_persistd::fdb::FdbContext::connect(cluster_file)
            .map_err(|error| IdentityError::Store(error.to_string()))?;
        Ok(Self::from_database(context.database()))
    }

    /// Reuse a process-scoped, bounded database handle.
    #[must_use]
    pub fn from_database(db: Arc<Database>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl crate::standing::StrikeRowSource for FdbStrikeRowSource {
    async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, IdentityError> {
        let start = strike_account_range_start(account);
        let end = strike_account_range_end(account);
        self.db
            .run(|trx, _| {
                let start = start.clone();
                let end = end.clone();
                async move {
                    let mut stream = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                start.as_slice(),
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                            ..foundationdb::RangeOption::default()
                        },
                        true,
                    );
                    let mut rows = Vec::new();
                    while let Some(kv) = stream.try_next().await? {
                        let row = postcard::from_bytes(kv.value())
                            .map_err(decode_err("strike row decode"))?;
                        rows.push(row);
                    }
                    Ok(rows)
                }
            })
            .await
            .map_err(unwrap_binding_error)
    }
}

/// A FoundationDB-backed [`AccountStore`] over the `d` family.
#[derive(Clone)]
pub struct FdbAccountStore {
    db: Arc<Database>,
    /// Abort every mutation after staging its writes and before committing.
    ///
    /// The fault [`tests::binding_writes_are_all_or_nothing`] injects to make
    /// atomicity observable rather than asserted. Test-only: an operator has no
    /// use for a store that refuses to commit, and a public switch that turns
    /// one on is a foot-gun with a security consequence.
    #[cfg(test)]
    abort_before_commit: bool,
}

impl std::fmt::Debug for FdbAccountStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FdbAccountStore").finish_non_exhaustive()
    }
}

impl FdbAccountStore {
    /// Open a store against `cluster_file`.
    ///
    /// Goes through `orrery_persistd::fdb::FdbContext`, so this handle inherits
    /// the transaction timeout and retry bound every other durable adapter in
    /// this tree has. A handle opened straight from `Database::from_path`
    /// inherits the C client's retry-forever default instead, and an
    /// unreachable cluster then hangs the login path rather than failing it.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Store`] when the cluster file is unusable.
    pub fn connect(cluster_file: &str) -> Result<Self, IdentityError> {
        let context = orrery_persistd::fdb::FdbContext::connect(cluster_file)
            .map_err(|error| IdentityError::Store(error.to_string()))?;
        Ok(Self::from_database(context.database()))
    }

    /// Wrap a database handle this process already owns.
    ///
    /// The caller is responsible for having bounded it — see
    /// `orrery_persistd::fdb::bound_transactions` — which is why
    /// [`Self::connect`] is the path to prefer.
    #[must_use]
    pub fn from_database(db: Arc<Database>) -> Self {
        Self {
            db,
            #[cfg(test)]
            abort_before_commit: false,
        }
    }

    /// Whether a staged mutation should be aborted instead of committed.
    #[cfg(test)]
    const fn aborting(&self) -> bool {
        self.abort_before_commit
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    const fn aborting(&self) -> bool {
        false
    }
}

/// Smuggle an [`IdentityError`] out of a transaction closure.
///
/// `db.run` only carries `FdbBindingError`, so a typed refusal — an account at
/// its binding cap, a NodeId spoken for elsewhere — travels as a custom error
/// and is recovered by [`unwrap_binding_error`]. Custom errors are **not**
/// retried, which is the property that matters: a refusal must abort the
/// transaction once, not spin it.
fn custom(error: IdentityError) -> FdbBindingError {
    FdbBindingError::new_custom_error(Box::new(error))
}

fn decode_err(what: &'static str) -> impl Fn(postcard::Error) -> FdbBindingError {
    move |error| custom(IdentityError::Store(format!("{what}: {error}")))
}

/// Recover a smuggled [`IdentityError`], or map a raw FDB failure onto one.
fn unwrap_binding_error(error: FdbBindingError) -> IdentityError {
    if let FdbBindingError::CustomError(ref boxed) = error {
        if let Some(typed) = boxed.downcast_ref::<IdentityError>() {
            return typed.clone();
        }
    }
    if let Some(fdb_error) = error.get_fdb_error() {
        return IdentityError::Store(format!("fdb {}: {}", fdb_error.code(), fdb_error.message()));
    }
    IdentityError::Store(format!("{error:?}"))
}

/// Fold one binding event into the account row.
///
/// The same fold [`crate::mem`] applies, and it lives beside the `dh` append
/// for the reason D31's resolved question 2 requires: "maintained in the same
/// transaction that appends a `dh` row".
fn fold_event(row: &mut AccountRow, at_ms: u64) {
    row.binding_epoch = row.binding_epoch.saturating_add(1);
    if row.binding_event_count == 0 {
        row.first_event_ms = at_ms;
    }
    row.binding_event_count = row.binding_event_count.saturating_add(1);
}

/// Read and decode `da ‖ account` inside `trx`.
///
/// **Not a snapshot read.** The conflict range it registers is what serializes
/// two concurrent binds against one account, so the row a bind decides against
/// is the row its commit lands on.
async fn read_account(
    trx: &foundationdb::RetryableTransaction,
    account: AccountId,
) -> Result<Option<AccountRow>, FdbBindingError> {
    let key = keyspace::account_key(account);
    let Some(raw) = trx.get(&key, false).await? else {
        return Ok(None);
    };
    let row: AccountRow = postcard::from_bytes(&raw).map_err(decode_err("account row decode"))?;
    Ok(Some(row))
}

/// Read and decode `db ‖ node` inside `trx`, for the same reason and with the
/// same conflict range.
async fn read_binding(
    trx: &foundationdb::RetryableTransaction,
    node: &NodeId,
) -> Result<Option<BindingRow>, FdbBindingError> {
    let key = keyspace::binding_key(node);
    let Some(raw) = trx.get(&key, false).await? else {
        return Ok(None);
    };
    let row: BindingRow = postcard::from_bytes(&raw).map_err(decode_err("binding row decode"))?;
    Ok(Some(row))
}

/// Read and decode the account's `dw ‖ account` window inside `trx`.
///
/// **Not a snapshot read**, exactly like [`read_account`]: D36 (b) reads the
/// window with the same discipline as `da`, whose conflict range is what
/// serializes two concurrent binds on one account. The check must run before
/// anything is staged, and a versionstamped ordering is impossible pre-commit
/// — `at_ms` is the available time base (D36 §Alternatives).
async fn read_window(
    trx: &foundationdb::RetryableTransaction,
    account: AccountId,
) -> Result<Vec<u64>, FdbBindingError> {
    let key = keyspace::binding_window_key(account);
    let Some(raw) = trx.get(&key, false).await? else {
        return Ok(Vec::new());
    };
    postcard::from_bytes(&raw).map_err(decode_err("binding window decode"))
}

/// Key for D33's identity-owned cooldown entry: `dc ‖ account:u64-be`.
///
/// `dc` is the formerly unused sub-span immediately after D31's `db` range
/// end. This crate is the `d` family's sole writer; the key is local because
/// cooldown is derived identity state rather than a persistd concern.
fn cooldown_entry_key(account: AccountId) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = b'd';
    key[1] = b'c';
    key[2..].copy_from_slice(&account.0.to_be_bytes());
    key
}

/// Inclusive start of the whole `dc` family: every cooldown entry sorts at or
/// after `dc\0…`.
const COOLDOWN_RANGE_START: [u8; 2] = [b'd', b'c'];

/// Exclusive end of the whole `dc` family.
///
/// `dd` is not a key this crate writes; it is simply the successor of the two
/// byte `dc` prefix, so `[dc, dd)` contains every 10-byte `dc ‖ account` row
/// and nothing else. Deriving the bound from the prefix rather than from
/// `account = u64::MAX` keeps it correct if the row ever grows a suffix.
const COOLDOWN_RANGE_END: [u8; 2] = [b'd', b'd'];

/// Read the fixed-width `dc` timestamp inside one transaction.
async fn read_cooldown_entry(
    trx: &foundationdb::RetryableTransaction,
    account: AccountId,
) -> Result<Option<CooldownEntry>, FdbBindingError> {
    let key = cooldown_entry_key(account);
    let Some(raw) = trx.get(&key, false).await? else {
        return Ok(None);
    };
    let bytes: [u8; 8] = raw.as_ref().try_into().map_err(|_| {
        custom(IdentityError::Store(
            "cooldown entry decode: expected 8 bytes".into(),
        ))
    })?;
    Ok(Some(CooldownEntry {
        entered_at_ms: u64::from_be_bytes(bytes),
    }))
}

/// Stage the `dh` append.
///
/// `SetVersionstampedKey` with the parameter `keyspace` builds, so FDB
/// substitutes this transaction's commit versionstamp and a node's events are
/// ordered by commit order rather than by the writer's clock. One per
/// transaction — every versionstamped write in a transaction gets the same ten
/// bytes — which is why a bind and an unbind are separate transactions and not
/// a batch.
fn stage_history(
    trx: &foundationdb::RetryableTransaction,
    node: &NodeId,
    row: &BindingHistoryRow,
) -> Result<(), FdbBindingError> {
    let param = keyspace::binding_history_versionstamped_key(node);
    let encoded = postcard::to_stdvec(row).map_err(decode_err("binding history row encode"))?;
    trx.atomic_op(&param, &encoded, MutationType::SetVersionstampedKey);
    Ok(())
}

/// Stage the `dw` write-back: the pruned vector with this event's stamp
/// appended.
///
/// Rides the same transaction as `da`, `db` and `dh` — D36 clause (b)'s
/// atomicity requirement, the same property D31 clause (b) makes
/// load-bearing — so an abort leaves the window exactly as it was read, and a
/// refusal (which returns before this is reached) stages nothing at all.
fn stage_window(
    trx: &foundationdb::RetryableTransaction,
    account: AccountId,
    window: &[u64],
) -> Result<(), FdbBindingError> {
    let encoded = postcard::to_stdvec(window).map_err(decode_err("binding window encode"))?;
    trx.set(&keyspace::binding_window_key(account), &encoded);
    Ok(())
}

#[async_trait]
impl AccountStore for FdbAccountStore {
    async fn create_account(
        &self,
        account: AccountId,
        created_ms: u64,
    ) -> Result<(), IdentityError> {
        let aborting = self.aborting();
        self.db
            .run(|trx, _maybe_committed| async move {
                if read_account(&trx, account).await?.is_some() {
                    return Err(custom(IdentityError::AccountExists(account)));
                }
                let row = AccountRow {
                    created_ms,
                    ..AccountRow::default()
                };
                let encoded =
                    postcard::to_stdvec(&row).map_err(decode_err("account row encode"))?;
                trx.set(&keyspace::account_key(account), &encoded);
                if aborting {
                    return Err(custom(IdentityError::Store("injected abort".into())));
                }
                Ok(())
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn account(&self, account: AccountId) -> Result<Option<AccountRow>, IdentityError> {
        self.db
            .run(|trx, _maybe_committed| async move { read_account(&trx, account).await })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn binding(&self, node: &NodeId) -> Result<Option<BindingRow>, IdentityError> {
        let node = *node;
        self.db
            .run(|trx, _maybe_committed| async move { read_binding(&trx, &node).await })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn bind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<BindOutcome, IdentityError> {
        let node = *node;
        let aborting = self.aborting();
        self.db
            .run(|trx, _maybe_committed| async move {
                let Some(mut row) = read_account(&trx, account).await? else {
                    return Err(custom(IdentityError::UnknownAccount(account)));
                };
                if let Some(existing) = read_binding(&trx, &node).await? {
                    if existing.account == account {
                        // Not an event: re-asserting a binding that already
                        // holds appends nothing, so a caller cannot inflate the
                        // append-only log or the fold for free.
                        return Ok(BindOutcome::AlreadyBound);
                    }
                    return Err(custom(IdentityError::NodeBoundElsewhere {
                        node,
                        account: existing.account,
                    }));
                }
                if row.bound_nodes.len() >= MAX_BOUND_NODES_PER_ACCOUNT {
                    return Err(custom(IdentityError::TooManyBoundNodes {
                        account,
                        cap: MAX_BOUND_NODES_PER_ACCOUNT,
                    }));
                }
                // D36 (b): the rate check runs after the concurrency cap and
                // before anything is staged, so a refusal consumes nothing.
                // The 24 h window is evaluated first, so a double trip names
                // the shorter one.
                let window = match admit_binding_event(&read_window(&trx, account).await?, at_ms) {
                    Ok(next) => next,
                    Err(refusal) => return Err(custom(rate_limited(account, refusal))),
                };

                row.bound_nodes.push(node);
                fold_event(&mut row, at_ms);
                let account_bytes =
                    postcard::to_stdvec(&row).map_err(decode_err("account row encode"))?;
                let binding_bytes = postcard::to_stdvec(&BindingRow {
                    account,
                    bound_at_ms: at_ms,
                })
                .map_err(decode_err("binding row encode"))?;

                trx.set(&keyspace::account_key(account), &account_bytes);
                trx.set(&keyspace::binding_key(&node), &binding_bytes);
                stage_history(
                    &trx,
                    &node,
                    &BindingHistoryRow {
                        account,
                        kind: BindKind::Bind,
                        at_ms,
                    },
                )?;
                stage_window(&trx, account, &window)?;

                if aborting {
                    return Err(custom(IdentityError::Store("injected abort".into())));
                }
                Ok(BindOutcome::Bound)
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn unbind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<(), IdentityError> {
        let node = *node;
        let aborting = self.aborting();
        self.db
            .run(|trx, _maybe_committed| async move {
                let Some(mut row) = read_account(&trx, account).await? else {
                    return Err(custom(IdentityError::UnknownAccount(account)));
                };
                match read_binding(&trx, &node).await? {
                    Some(existing) if existing.account == account => {}
                    _ => return Err(custom(IdentityError::NotBound { node, account })),
                }
                // An unbind is an event too (D36 (b), property 2): refusing it
                // here aborts the whole transaction, so the binding stays in
                // place and removal waits for a window slide.
                let window = match admit_binding_event(&read_window(&trx, account).await?, at_ms) {
                    Ok(next) => next,
                    Err(refusal) => return Err(custom(rate_limited(account, refusal))),
                };

                row.bound_nodes.retain(|bound| bound != &node);
                fold_event(&mut row, at_ms);
                let account_bytes =
                    postcard::to_stdvec(&row).map_err(decode_err("account row encode"))?;

                trx.set(&keyspace::account_key(account), &account_bytes);
                // Cleared, not tombstoned: unbinding is immediate (docs/09 §8),
                // and the released NodeId's lookup becomes a miss — which
                // excludes, so shedding a NodeId before submitting buys
                // nothing.
                trx.clear(&keyspace::binding_key(&node));
                stage_history(
                    &trx,
                    &node,
                    &BindingHistoryRow {
                        account,
                        kind: BindKind::Unbind,
                        at_ms,
                    },
                )?;
                stage_window(&trx, account, &window)?;

                if aborting {
                    return Err(custom(IdentityError::Store("injected abort".into())));
                }
                Ok(())
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn binding_history(
        &self,
        node: &NodeId,
    ) -> Result<Vec<BindingHistoryRow>, IdentityError> {
        let start = keyspace::binding_history_node_range_start(node);
        let end = keyspace::binding_history_node_range_end(node);
        self.db
            .run(|trx, _maybe_committed| {
                let start = start.clone();
                let end = end.clone();
                async move {
                    let mut stream = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                start.as_slice(),
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    // The range is already in commit order: FDB returns keys
                    // ascending and the versionstamp is the key's tail.
                    let mut rows = Vec::new();
                    while let Some(kv) = stream.try_next().await? {
                        let row: BindingHistoryRow = postcard::from_bytes(kv.value())
                            .map_err(decode_err("binding history row decode"))?;
                        rows.push(row);
                    }
                    Ok(rows)
                }
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn observe_cooldown(
        &self,
        account: AccountId,
        observed_at_ms: u64,
        newest_live_strike_ms: Option<u64>,
    ) -> Result<CooldownEntry, IdentityError> {
        self.db
            .run(|trx, _maybe_committed| async move {
                if read_account(&trx, account).await?.is_none() {
                    return Err(custom(IdentityError::UnknownAccount(account)));
                }
                let current = read_cooldown_entry(&trx, account).await?;
                let entry = match current {
                    Some(entry)
                        if newest_live_strike_ms
                            .is_some_and(|issued_at_ms| issued_at_ms > entry.entered_at_ms) =>
                    {
                        CooldownEntry {
                            entered_at_ms: observed_at_ms,
                        }
                    }
                    Some(entry) => entry,
                    // Rollout rule: an account which was already in cooldown
                    // before `dc` existed starts a full dwell at first
                    // observation, never gets an unearned early release.
                    None => CooldownEntry {
                        entered_at_ms: observed_at_ms,
                    },
                };
                trx.set(
                    &cooldown_entry_key(account),
                    &entry.entered_at_ms.to_be_bytes(),
                );
                Ok(entry)
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn cooldown_entry(
        &self,
        account: AccountId,
    ) -> Result<Option<CooldownEntry>, IdentityError> {
        self.db
            .run(|trx, _maybe_committed| async move {
                if read_account(&trx, account).await?.is_none() {
                    return Err(custom(IdentityError::UnknownAccount(account)));
                }
                read_cooldown_entry(&trx, account).await
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn clear_cooldown_if(
        &self,
        account: AccountId,
        expected: CooldownEntry,
    ) -> Result<bool, IdentityError> {
        self.db
            .run(|trx, _maybe_committed| async move {
                if read_account(&trx, account).await?.is_none() {
                    return Err(custom(IdentityError::UnknownAccount(account)));
                }
                if read_cooldown_entry(&trx, account).await? == Some(expected) {
                    trx.clear(&cooldown_entry_key(account));
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .await
            .map_err(unwrap_binding_error)
    }

    async fn cooldown_entries(&self) -> Result<Vec<CooldownRecord>, IdentityError> {
        // A snapshot read. This is a reporting sweep, not an admission
        // decision: taking read conflict ranges over the entire family would
        // make every poll conflict with every concurrent `observe_cooldown`,
        // and the feed contract already tolerates a poll being one interval
        // stale. `FdbStrikeRowSource::rows` reads snapshot for the same
        // reason.
        self.db
            .run(|trx, _maybe_committed| async move {
                let mut stream = trx.get_ranges_keyvalues(
                    foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(
                            COOLDOWN_RANGE_START.as_slice(),
                        ),
                        end: foundationdb::KeySelector::first_greater_or_equal(
                            COOLDOWN_RANGE_END.as_slice(),
                        ),
                        ..foundationdb::RangeOption::default()
                    },
                    true,
                );
                let mut records = Vec::new();
                while let Some(kv) = stream.try_next().await? {
                    let key: [u8; 10] = kv.key().try_into().map_err(|_| {
                        custom(IdentityError::Store(
                            "cooldown entry key decode: expected 10 bytes".into(),
                        ))
                    })?;
                    let value: [u8; 8] = kv.value().try_into().map_err(|_| {
                        custom(IdentityError::Store(
                            "cooldown entry decode: expected 8 bytes".into(),
                        ))
                    })?;
                    let mut account = [0u8; 8];
                    account.copy_from_slice(&key[2..]);
                    records.push(CooldownRecord {
                        account: AccountId(u64::from_be_bytes(account)),
                        entry: CooldownEntry {
                            entered_at_ms: u64::from_be_bytes(value),
                        },
                    });
                }
                Ok(records)
            })
            .await
            .map_err(unwrap_binding_error)
    }
}

#[cfg(test)]
mod tests {
    //! FDB-gated tests for the durable store.
    //!
    //! Inline in the library target rather than under `tests/`, for one reason
    //! the `tests/` form cannot give: the atomicity proof needs to inject a
    //! fault *inside* the transaction, and a `#[cfg(test)]` field is the way to
    //! do that without putting an abort switch in the public API.
    //!
    //! Every test self-skips with a `skipping:` line when no cluster is
    //! reachable — right for a developer's `cargo test`, and a trap for CI,
    //! which is why `scripts/fdb-tests.sh` fails on that line rather than
    //! trusting the exit status.
    //!
    //! **Account ids are namespaced to this file** (`0x0210_…`), the same
    //! discipline `intent/fdb.rs` applies with its own grid: these suites run
    //! against a shared development cluster and a collided id turns an
    //! unrelated agent's run into a failure that reads like a bug in the
    //! mechanism.

    use super::*;
    use crate::window::{BINDING_RATE_CAP_24H, BINDING_RATE_WINDOW_24H_MS};
    use orrery_protocol::NodeId;

    fn cluster_file() -> Option<String> {
        orrery_persistd::fdb::discover_cluster_file()
    }

    fn node(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = 0x21;
        iroh_base::SecretKey::from_bytes(&bytes).public()
    }

    /// A distinct account per test, so two tests on one shared cluster cannot
    /// contend on a row.
    fn account(slot: u64) -> AccountId {
        AccountId(0x0210_0000_0000_0000 | slot)
    }

    /// Leave the subspace as it was found, so a shared cluster does not
    /// accumulate this suite's rows. The window row is this suite's too since
    /// D36 — a stale `dw` vector would rate-limit the next run's binds with
    /// events that no longer exist anywhere else.
    async fn wipe(store: &FdbAccountStore, account: AccountId, nodes: &[NodeId]) {
        let db = Arc::clone(&store.db);
        let nodes = nodes.to_vec();
        let _ = db
            .run(|trx, _| {
                let nodes = nodes.clone();
                async move {
                    trx.clear(&keyspace::account_key(account));
                    trx.clear(&keyspace::binding_window_key(account));
                    // D33's `dc` entry is this suite's row too: a leftover
                    // cooldown would publish an invalidation for an account
                    // the next run believes it just created clean.
                    trx.clear(&cooldown_entry_key(account));
                    for node in &nodes {
                        trx.clear(&keyspace::binding_key(node));
                        trx.clear_range(
                            &keyspace::binding_history_node_range_start(node),
                            &keyspace::binding_history_node_range_end(node),
                        );
                    }
                    Ok(())
                }
            })
            .await;
    }

    macro_rules! fdb_test {
        ($name:ident, $body:expr) => {
            #[tokio::test]
            async fn $name() {
                let Some(cluster) = cluster_file() else {
                    eprintln!(
                        "skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster"
                    );
                    return;
                };
                let store = FdbAccountStore::connect(&cluster).expect("connect identity store");
                let body: fn(FdbAccountStore) -> _ = $body;
                body(store).await;
            }
        };
    }

    fdb_test!(bind_writes_da_db_and_dh_together, |store| async move {
        let account = account(1);
        let node = node(1);
        wipe(&store, account, &[node]).await;

        store
            .create_account(account, 1_000)
            .await
            .expect("create account");
        assert_eq!(
            store.bind(account, &node, 2_000).await.expect("bind"),
            BindOutcome::Bound
        );

        let row = store
            .account(account)
            .await
            .expect("read account")
            .expect("account row");
        assert_eq!(row.bound_nodes, vec![node]);
        assert_eq!(row.binding_epoch, 1);
        // The write-time fold of D31's resolved question 2, maintained in the
        // transaction that appended the `dh` row.
        assert_eq!(row.binding_event_count, 1);
        assert_eq!(row.first_event_ms, 2_000);

        let binding = store
            .binding(&node)
            .await
            .expect("read binding")
            .expect("binding row");
        assert_eq!(binding.account, account);
        assert_eq!(binding.bound_at_ms, 2_000);

        let history = store.binding_history(&node).await.expect("read history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].kind, BindKind::Bind);
        assert_eq!(history[0].account, account);

        wipe(&store, account, &[node]).await;
    });

    fdb_test!(unbind_clears_db_and_appends_history, |store| async move {
        let account = account(2);
        let node = node(2);
        wipe(&store, account, &[node]).await;

        store.create_account(account, 1_000).await.expect("create");
        store.bind(account, &node, 2_000).await.expect("bind");
        store.unbind(account, &node, 3_000).await.expect("unbind");

        // Immediate, and a deletion rather than a tombstone (docs/09 §8).
        assert!(store.binding(&node).await.expect("read binding").is_none());
        let row = store
            .account(account)
            .await
            .expect("read account")
            .expect("row");
        assert!(row.bound_nodes.is_empty());
        assert_eq!(row.binding_event_count, 2);
        // The fold is never decremented, which is what makes history expiry a
        // pure range delete.
        assert_eq!(row.first_event_ms, 2_000);

        let history = store.binding_history(&node).await.expect("history");
        assert_eq!(
            history.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![BindKind::Bind, BindKind::Unbind],
            "the `dh` span is in commit order, oldest first"
        );

        wipe(&store, account, &[node]).await;
    });

    /// The account's stored `dw` vector, read straight off its raw key —
    /// not through any accessor, so the assertion is about the bytes.
    async fn window_of(store: &FdbAccountStore, account: AccountId) -> Vec<u64> {
        let db = Arc::clone(&store.db);
        let key = keyspace::binding_window_key(account);
        let raw = db
            .run(|trx, _| async move {
                trx.get(&key, false)
                    .await
                    .map_err(foundationdb::FdbBindingError::from)
            })
            .await
            .expect("read window row");
        raw.map(|value| postcard::from_bytes(&value[..]).expect("decode window row"))
            .unwrap_or_default()
    }

    // D31 clause (b), made observable — extended per D36 clause (d): a
    // mutation that stages `da`, `db`, `dh` **and the window** and then fails
    // leaves *no* row changed. Split the same work across two transactions
    // and the first one's rows survive — the window in which `db` names an
    // account `da` does not bind, which is the state clause (f) turns from a
    // miss into a wrong answer. One successful bind runs first, so "unchanged"
    // below means something: the window row exists with one stamp, and the
    // aborted transaction must leave it exactly that.
    fdb_test!(binding_writes_are_all_or_nothing, |store| async move {
        let account = account(3);
        let bound = node(3);
        let refused = node(0x63);
        let after = node(0x64);
        wipe(&store, account, &[bound, refused, after]).await;

        store.create_account(account, 1_000).await.expect("create");
        store.bind(account, &bound, 2_000).await.expect("bind");
        assert_eq!(window_of(&store, account).await, vec![2_000]);

        let mut aborting = store.clone();
        aborting.abort_before_commit = true;
        let error = aborting
            .bind(account, &refused, 3_000)
            .await
            .expect_err("the injected abort refuses the bind");
        assert!(matches!(error, IdentityError::Store(_)));

        // Nothing from the aborted transaction landed, and everything from
        // the committed one did: the half-applied mix is exactly the
        // inconsistency the single transaction exists to make unobservable.
        assert!(
            store
                .binding(&refused)
                .await
                .expect("read binding")
                .is_none(),
            "the aborted transaction must leave no `db` row"
        );
        let row = store
            .account(account)
            .await
            .expect("read account")
            .expect("row");
        assert_eq!(
            row.bound_nodes,
            vec![bound],
            "the aborted transaction must leave `da` unchanged"
        );
        assert_eq!(
            row.binding_event_count, 1,
            "and must leave the write-time fold unchanged with it"
        );
        assert!(
            store
                .binding_history(&refused)
                .await
                .expect("history")
                .is_empty(),
            "and must append no `dh` row"
        );
        assert_eq!(
            window_of(&store, account).await,
            vec![2_000],
            "and must leave the `dw` window unchanged across the injected abort"
        );

        // The store that did not abort still works, so the assertion above
        // is about atomicity and not about a store that writes nothing.
        store.bind(account, &after, 4_000).await.expect("bind");
        assert!(store.binding(&after).await.expect("binding").is_some());
        assert_eq!(window_of(&store, account).await, vec![2_000, 4_000]);

        wipe(&store, account, &[bound, refused, after]).await;
    });

    fdb_test!(a_node_binds_to_at_most_one_account, |store| async move {
        let first = account(4);
        let second = account(5);
        let node = node(4);
        wipe(&store, first, &[node]).await;
        wipe(&store, second, &[]).await;

        store.create_account(first, 1_000).await.expect("create");
        store.create_account(second, 1_000).await.expect("create");
        store.bind(first, &node, 2_000).await.expect("bind");

        let error = store
            .bind(second, &node, 3_000)
            .await
            .expect_err("a bound node is not silently re-pointed");
        assert_eq!(
            error,
            IdentityError::NodeBoundElsewhere {
                node,
                account: first
            }
        );
        // Re-asserting the binding that holds is idempotent and appends no
        // event.
        assert_eq!(
            store.bind(first, &node, 4_000).await.expect("rebind"),
            BindOutcome::AlreadyBound
        );
        assert_eq!(
            store
                .account(first)
                .await
                .expect("read")
                .expect("row")
                .binding_event_count,
            1
        );

        wipe(&store, first, &[node]).await;
        wipe(&store, second, &[]).await;
    });

    fdb_test!(an_account_binds_at_most_eight_nodes, |store| async move {
        let account = account(6);
        let nodes: Vec<NodeId> = (0..=u8::try_from(MAX_BOUND_NODES_PER_ACCOUNT).unwrap_or(8))
            .map(|index| node(0x40 + index))
            .collect();
        wipe(&store, account, &nodes).await;

        store.create_account(account, 1_000).await.expect("create");
        for node in nodes.iter().take(MAX_BOUND_NODES_PER_ACCOUNT) {
            store.bind(account, node, 2_000).await.expect("bind");
        }
        let error = store
            .bind(account, &nodes[MAX_BOUND_NODES_PER_ACCOUNT], 2_000)
            .await
            .expect_err("the ninth bind is refused");
        assert_eq!(
            error,
            IdentityError::TooManyBoundNodes {
                account,
                cap: MAX_BOUND_NODES_PER_ACCOUNT
            }
        );

        wipe(&store, account, &nodes).await;
    });

    // D36 clause (d)'s durable obligation: the ninth event inside one
    // trailing-24 h span is refused by the store itself, naming the window —
    // and nothing was consumed. Four bind/unbind pairs fill the short window
    // while keeping concurrency clear of its own cap, which is checked before
    // the rate cap by design; pure binds could never reach this refusal.
    fdb_test!(
        the_ninth_event_inside_24h_is_refused_at_the_durable_store,
        |store| async move {
            const DAY: u64 = BINDING_RATE_WINDOW_24H_MS;
            let t0 = DAY * 1_000;
            let account = account(7);
            let pairs: Vec<NodeId> = (0..4u8).map(|k| node(0x50 + k)).collect();
            let ninth = node(0x60);
            let mut all = pairs.clone();
            all.push(ninth);
            wipe(&store, account, &all).await;

            store.create_account(account, 1_000).await.expect("create");

            for (k, device) in pairs.iter().enumerate() {
                let base = t0 + 2 * k as u64;
                assert_eq!(
                    store.bind(account, device, base).await.expect("bind"),
                    BindOutcome::Bound
                );
                store
                    .unbind(account, device, base + 1)
                    .await
                    .expect("unbind");
            }
            assert_eq!(
                window_of(&store, account).await.len(),
                BINDING_RATE_CAP_24H,
                "eight admitted events sit in the durable window"
            );

            let error = store
                .bind(account, &ninth, t0 + 8)
                .await
                .expect_err("the ninth event inside 24 h refuses at the durable store");
            assert_eq!(
                error,
                IdentityError::BindingRateLimited {
                    account,
                    window_ms: DAY,
                    cap: BINDING_RATE_CAP_24H,
                }
            );

            // Refused, not deferred: no slot consumed, no `db` row, no `dh`
            // append, fold unmoved, window exactly as it was.
            assert!(store.binding(&ninth).await.expect("read").is_none());
            assert!(store
                .binding_history(&ninth)
                .await
                .expect("history")
                .is_empty());
            let row = store.account(account).await.expect("read").expect("row");
            assert!(row.bound_nodes.is_empty());
            assert_eq!(
                row.binding_event_count,
                u32::try_from(BINDING_RATE_CAP_24H).unwrap()
            );
            assert_eq!(window_of(&store, account).await.len(), BINDING_RATE_CAP_24H);

            wipe(&store, account, &all).await;
        }
    );

    // The durable half of D33 clause (e), against a real cluster: a cooldown
    // entry written through the store is published as an
    // `AccountInvalidation` by a separate reader that shares no memory with
    // the writer — only the `dc` rows.
    //
    // The in-memory suite in `crate::invalidation` proves the mapping; this
    // proves the key layout, the big-endian account decode and the range
    // bounds, none of which `MemAccountStore` exercises at all.
    //
    // Filtered to this suite's own account rather than compared as a whole
    // set: the range read covers the entire `dc` family, and the development
    // cluster is shared with every other agent's run.
    fdb_test!(
        a_durable_cooldown_entry_is_published_as_an_invalidation,
        |store| async move {
            use crate::invalidation::StandingInvalidationSource;
            use orrery_protocol::UnixMillis;

            let account = account(9);
            wipe(&store, account, &[]).await;
            store
                .create_account(account, 1_000)
                .await
                .expect("create account");

            let source = StandingInvalidationSource::new(Arc::new(store.clone()));
            let mine = |published: Vec<orrery_protocol::AccountInvalidation>| {
                published
                    .into_iter()
                    .filter(|entry| entry.account == account)
                    .collect::<Vec<_>>()
            };

            assert!(
                mine(source.current().await.expect("read the family")).is_empty(),
                "an account that has never crossed C publishes nothing"
            );

            store
                .observe_cooldown(account, 7_000, None)
                .await
                .expect("the account crosses C");

            assert_eq!(
                mine(source.current().await.expect("publish")),
                vec![orrery_protocol::AccountInvalidation {
                    account,
                    effective_from_ms: UnixMillis(7_000),
                }],
                "the durable `dc` row round-trips as the refusal watermark"
            );

            // Release, and the producer stops asserting a refusal it no longer
            // holds. (The consumers' retention rule is theirs, and is tested
            // where it lives.)
            let entry = store
                .cooldown_entry(account)
                .await
                .expect("read entry")
                .expect("entry exists");
            assert!(store
                .clear_cooldown_if(account, entry)
                .await
                .expect("release"));
            assert!(
                mine(source.current().await.expect("publish")).is_empty(),
                "a released account leaves the published set"
            );

            wipe(&store, account, &[]).await;
        }
    );

    // ---------------------------------------------------------------------
    // #862 acceptance box 1: the executor writes a `ya` row and a *subsequent
    // mint reads it*. Both halves are deployed —
    // `orrery_persistd/src/bin/persistd.rs` builds `FdbStrikeLedger`, and
    // `orrery_identity/src/bin/orrery-identity.rs` builds
    // `FdbStrikeRowSource` — but until now nothing exercised the seam
    // *between* them. The write was proven by a spawned binary
    // (`persistd_ruleset_registration::reference_ruleset_binary_strike_modes_reach_the_durable_ledger`)
    // and the refusal was proven over synthetic in-memory rows
    // (`orrery_identity/tests/served.rs`), which is exactly the "two unit
    // tests" the acceptance box refuses to accept. These two drive the real
    // writer and the real reader against one cluster, so a divergence in the
    // `ya` key layout or the postcard row encoding fails here rather than in
    // a deployment.

    /// The strike family is the executor's, so this suite clears its own
    /// `ya` range as well as the `d` rows `wipe` already owns.
    async fn wipe_strikes(store: &FdbAccountStore, account: AccountId) {
        let db = Arc::clone(&store.db);
        let start = strike_account_range_start(account);
        let end = strike_account_range_end(account);
        let _ = db
            .run(|trx, _| {
                let start = start.clone();
                let end = end.clone();
                async move {
                    trx.clear_range(&start, &end);
                    Ok(())
                }
            })
            .await;
    }

    /// One D33 clause (a) major finding, distinguished by `seed` so the
    /// ledger's evidence-digest dedup treats two calls as two facts.
    fn major_row(
        issued_at_ms: u64,
        seed: u8,
        mode: orrery_persistd::adjudication::StrikeMode,
    ) -> StrikeRow {
        StrikeRow {
            issued_at_ms,
            weight_milli: orrery_persistd::adjudication::MAJOR_STRIKE_WEIGHT_MILLI,
            kind: orrery_persistd::adjudication::StrikeKind::Deviation,
            evidence_ref: orrery_persistd::adjudication::StrikeEvidenceRef {
                entity: orrery_protocol::PersistId::new(862),
                window_start: orrery_protocol::Tick::new(1),
                window_end: orrery_protocol::Tick::new(2),
                digest: [seed; 32],
            },
            ruleset: orrery_protocol::RulesetId {
                version: 1,
                digest: [1; 32],
            },
            mode,
            expires_at_ms: issued_at_ms + orrery_persistd::adjudication::STRIKE_RETENTION_MS,
        }
    }

    /// Bind `node` to `account` through identity's own writer, then file
    /// `rows` through the executor's own writer, and return the reader the
    /// `orrery-identity` binary uses.
    async fn file_through_the_real_ledger(
        store: &FdbAccountStore,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
        rows: &[StrikeRow],
    ) -> FdbStrikeRowSource {
        use orrery_persistd::adjudication::{
            FdbStrikeLedger, OffenceTime, StrikeFileOutcome, StrikeLedger,
        };

        store
            .create_account(account, at_ms)
            .await
            .expect("create account");
        assert_eq!(
            store.bind(account, node, at_ms).await.expect("bind"),
            BindOutcome::Bound
        );

        let ledger = FdbStrikeLedger::from_database(Arc::clone(&store.db));
        for row in rows {
            assert_eq!(
                ledger
                    .file(*node, OffenceTime::KnownMs(at_ms + 1), row, None)
                    .expect("the executor files against the resolved binding"),
                StrikeFileOutcome::Filed { account },
                "attribution must resolve through the `db`/`dh` rows identity wrote"
            );
        }

        FdbStrikeRowSource::from_database(Arc::clone(&store.db))
    }

    fdb_test!(
        a_filed_ya_row_is_read_back_by_the_deployed_reader_and_refuses_the_mint,
        |store| async move {
            use crate::service::StandingSource;
            use crate::ComputedStanding;
            use orrery_persistd::adjudication::StrikeMode;

            let account = account(0x8621);
            let node = node(0x71);
            wipe(&store, account, &[node]).await;
            wipe_strikes(&store, account).await;

            let at_ms = 1_000_000;
            // Two majors is 6.0, over C (5.0) and under the ban band (7.0).
            let source = file_through_the_real_ledger(
                &store,
                account,
                &node,
                at_ms,
                &[
                    major_row(at_ms + 1, 1, StrikeMode::Live),
                    major_row(at_ms + 1, 2, StrikeMode::Live),
                ],
            )
            .await;

            // The reader decodes the writer's bytes: same count, same weights,
            // same mode. A keyspace or postcard drift dies on this assertion.
            let read = crate::standing::StrikeRowSource::rows(&source, account)
                .await
                .expect("the deployed reader reads the deployed writer's rows");
            assert_eq!(
                read.len(),
                2,
                "both filed `ya` rows are visible to identity"
            );
            assert!(
                read.iter().all(|row| row.mode == StrikeMode::Live
                    && row.weight_milli
                        == orrery_persistd::adjudication::MAJOR_STRIKE_WEIGHT_MILLI),
                "the round-trip preserves mode and weight: {read:?}"
            );

            // And the mint decision follows those rows, through the same
            // `CooldownStanding` the `orrery-identity` binary constructs.
            let store = Arc::new(store);
            let scorer = ComputedStanding::new(
                source,
                move || at_ms + 1,
                crate::standing::DEFAULT_STANDING_THRESHOLDS,
            )
            .expect("the default policy package is coherent");
            let standing = crate::CooldownStanding::new(Arc::clone(&store), scorer);
            assert!(
                matches!(
                    standing.standing(account, store.as_ref()).await,
                    Err(IdentityError::Cooldown(refused)) if refused == account
                ),
                "a mint after a real filing is refused, not stamped Good"
            );

            // The refusal is what #934's producer publishes, so the same
            // filing also reaches the coordinator's feed.
            assert!(
                store
                    .cooldown_entry(account)
                    .await
                    .expect("read entry")
                    .is_some(),
                "the refused mint left the durable `dc` row the feed reads"
            );

            wipe_strikes(&store, account).await;
            wipe(&store, account, &[node]).await;
        }
    );

    // #862 acceptance box 4, on the wired path rather than the scorer's own
    // fixtures: `standing.rs` proves shadow rows are inert against
    // hand-built rows, and the spawned-binary test proves `--strikes shadow`
    // stamps `Shadow` durably. This joins them — rows the executor really
    // wrote, read through the reader the binary really uses, changing no
    // standing.
    fdb_test!(
        shadow_ya_rows_read_through_the_deployed_reader_change_no_standing,
        |store| async move {
            use crate::service::StandingSource;
            use crate::ComputedStanding;
            use orrery_persistd::adjudication::StrikeMode;

            let account = account(0x8622);
            let node = node(0x72);
            wipe(&store, account, &[node]).await;
            wipe_strikes(&store, account).await;

            let at_ms = 1_000_000;
            // The same two majors that refused above, stamped shadow.
            let source = file_through_the_real_ledger(
                &store,
                account,
                &node,
                at_ms,
                &[
                    major_row(at_ms + 1, 3, StrikeMode::Shadow),
                    major_row(at_ms + 1, 4, StrikeMode::Shadow),
                ],
            )
            .await;

            let read = crate::standing::StrikeRowSource::rows(&source, account)
                .await
                .expect("read filed rows");
            assert_eq!(read.len(), 2, "shadow filings are durable rows, not drops");
            assert!(
                read.iter().all(|row| row.mode == StrikeMode::Shadow),
                "the mode stamp survives the round-trip: {read:?}"
            );

            let store = Arc::new(store);
            let scorer = ComputedStanding::new(
                source,
                move || at_ms + 1,
                crate::standing::DEFAULT_STANDING_THRESHOLDS,
            )
            .expect("the default policy package is coherent");
            let standing = crate::CooldownStanding::new(Arc::clone(&store), scorer);
            assert_eq!(
                standing
                    .standing(account, store.as_ref())
                    .await
                    .expect("a shadow ledger still mints"),
                orrery_protocol::SessionStanding::Good,
                "D32 C5 shadow files the fact and changes no standing"
            );
            assert!(
                store
                    .cooldown_entry(account)
                    .await
                    .expect("read entry")
                    .is_none(),
                "a shadow filing must not manufacture a `dc` row for the feed"
            );

            wipe_strikes(&store, account).await;
            wipe(&store, account, &[node]).await;
        }
    );
}
