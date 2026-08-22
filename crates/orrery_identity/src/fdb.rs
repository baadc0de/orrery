//! The durable account store: D31's `d` family, written by this service alone.
//!
//! # One transaction, three rows
//!
//! Every mutation here is a single `db.run` closure that stages `da`, `db` and
//! `dh` together. D31 clause (b) is explicit that this is the load-bearing
//! half, not the byte layout:
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
//! everything and then fails leaves *no* row changed. Split the same work
//! across two transactions and the first one's rows survive the second one's
//! failure, which is the observable form of the window clause (b) forbids.
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

use crate::store::{AccountStore, BindOutcome, IdentityError};
use async_trait::async_trait;
use foundationdb::options::MutationType;
use foundationdb::{Database, FdbBindingError};
use futures::TryStreamExt;
use orrery_persistd::keyspace::{
    self, AccountRow, BindKind, BindingHistoryRow, BindingRow, MAX_BOUND_NODES_PER_ACCOUNT,
};
use orrery_protocol::AccountId;
use orrery_protocol::NodeId;
use std::sync::Arc;

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
    /// accumulate this suite's rows.
    async fn wipe(store: &FdbAccountStore, account: AccountId, nodes: &[NodeId]) {
        let db = Arc::clone(&store.db);
        let nodes = nodes.to_vec();
        let _ = db
            .run(|trx, _| {
                let nodes = nodes.clone();
                async move {
                    trx.clear(&keyspace::account_key(account));
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

    // D31 clause (b), made observable: a mutation that stages `da`, `db` and
    // `dh` and then fails leaves **no** row changed. Split the same work across
    // two transactions and the first one's rows survive — the window in which
    // `db` names an account `da` does not bind, which is the state clause (f)
    // turns from a miss into a wrong answer.
    fdb_test!(binding_writes_are_all_or_nothing, |store| async move {
        let account = account(3);
        let node = node(3);
        wipe(&store, account, &[node]).await;

        store.create_account(account, 1_000).await.expect("create");

        let mut aborting = store.clone();
        aborting.abort_before_commit = true;
        let error = aborting
            .bind(account, &node, 2_000)
            .await
            .expect_err("the injected abort refuses the bind");
        assert!(matches!(error, IdentityError::Store(_)));

        // Neither half landed, and that is the assertion: a `db` row
        // without its `da` row, or the reverse, is the inconsistency the
        // single transaction exists to make unobservable.
        assert!(
            store.binding(&node).await.expect("read binding").is_none(),
            "the aborted transaction must leave no `db` row"
        );
        let row = store
            .account(account)
            .await
            .expect("read account")
            .expect("row");
        assert!(
            row.bound_nodes.is_empty(),
            "the aborted transaction must leave `da` unchanged"
        );
        assert_eq!(
            row.binding_event_count, 0,
            "and must leave the write-time fold unchanged with it"
        );
        assert!(
            store
                .binding_history(&node)
                .await
                .expect("history")
                .is_empty(),
            "and must append no `dh` row"
        );

        // The store that did not abort still works, so the assertion above
        // is about atomicity and not about a store that writes nothing.
        store.bind(account, &node, 4_000).await.expect("bind");
        assert!(store.binding(&node).await.expect("binding").is_some());

        wipe(&store, account, &[node]).await;
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
}
