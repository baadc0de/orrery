//! An in-process [`AccountStore`], for tests and for harnesses with no cluster.
//!
//! It keeps the same three maps the `d` family keeps as three sub-spans, and it
//! mutates all of them under **one** lock — which is the in-memory reading of
//! D31 clause (b): a reader can never observe `da` and `db` disagreeing,
//! because there is no instant between the two writes at which a reader runs.
//! A refusal (an account at its binding cap, a NodeId spoken for elsewhere)
//! happens before any map is touched, so a rejected bind leaves nothing behind.
//!
//! # It answers `BindingAuthority` directly
//!
//! [`orrery_persistd::gateway::BindingAuthority`] is the seam #211 introduced
//! for `owner(n)`, and its in-tree default `UnboundBindingAuthority` resolves
//! nothing "until `orrery_identity` exists". The trait's two load-bearing
//! properties are that `owner` is **synchronous** and **never blocks**, which
//! is why the durable store does *not* implement it — an FDB point read is
//! neither — and why this one does: a lock-guarded `HashMap` probe is exactly
//! the tier-2 shape clause (e) describes. [`MemAccountStore::bindings`] serves
//! the other direction, feeding
//! `orrery_persistd::gateway::SnapshotBindingAuthority` from a store rather
//! than from a table somebody typed.

use crate::store::{AccountStore, BindOutcome, IdentityError};
use async_trait::async_trait;
use orrery_persistd::gateway::BindingAuthority;
use orrery_persistd::keyspace::{
    AccountRow, BindKind, BindingHistoryRow, BindingRow, MAX_BOUND_NODES_PER_ACCOUNT,
};
use orrery_protocol::AccountId;
use orrery_protocol::NodeId;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The three sub-spans, as three maps under one lock.
#[derive(Debug, Default)]
struct State {
    /// `da ‖ account`.
    accounts: HashMap<AccountId, AccountRow>,
    /// `db ‖ node`.
    bindings: HashMap<NodeId, BindingRow>,
    /// `dh ‖ node ‖ versionstamp`, in append order — which stands in for the
    /// commit order a versionstamp gives, and is the same order for the same
    /// reason: one event per mutation, appended under the lock that serializes
    /// them.
    history: HashMap<NodeId, Vec<BindingHistoryRow>>,
}

/// An in-process account store.
#[derive(Debug, Default)]
pub struct MemAccountStore {
    state: Mutex<State>,
}

impl MemAccountStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every current `(node, account)` binding.
    ///
    /// Ready to hand to
    /// `orrery_persistd::gateway::SnapshotBindingAuthority::from_bindings`.
    #[must_use]
    pub fn bindings(&self) -> Vec<(NodeId, AccountId)> {
        Self::lock(&self.state)
            .bindings
            .iter()
            .map(|(node, row)| (*node, row.account))
            .collect()
    }

    /// Take the lock, treating poisoning as "read what is there".
    ///
    /// Nothing in this type panics while holding the lock — every refusal is
    /// computed before the first mutation — so a poisoned lock can only come
    /// from a panic in *another* thread's unrelated code, and the maps are
    /// still internally consistent. Propagating a poison error here would turn
    /// an unrelated test failure into an identity outage.
    fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
        state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Fold one binding event into the account row, exactly as D31's resolved
/// question 2 requires: in the same critical section that appends the `dh` row.
///
/// `binding_event_count` is a lifetime count and is never decremented, which is
/// what makes history expiry a pure range delete; `first_event_ms` is set once
/// and then left alone, so the pair still says "this account has rebound N
/// times since T" after every row that proved it has been deleted.
fn fold_event(row: &mut AccountRow, at_ms: u64) {
    row.binding_epoch = row.binding_epoch.saturating_add(1);
    if row.binding_event_count == 0 {
        row.first_event_ms = at_ms;
    }
    row.binding_event_count = row.binding_event_count.saturating_add(1);
}

#[async_trait]
impl AccountStore for MemAccountStore {
    async fn create_account(
        &self,
        account: AccountId,
        created_ms: u64,
    ) -> Result<(), IdentityError> {
        let mut state = Self::lock(&self.state);
        if state.accounts.contains_key(&account) {
            return Err(IdentityError::AccountExists(account));
        }
        state.accounts.insert(
            account,
            AccountRow {
                created_ms,
                ..AccountRow::default()
            },
        );
        Ok(())
    }

    async fn account(&self, account: AccountId) -> Result<Option<AccountRow>, IdentityError> {
        Ok(Self::lock(&self.state).accounts.get(&account).cloned())
    }

    async fn binding(&self, node: &NodeId) -> Result<Option<BindingRow>, IdentityError> {
        Ok(Self::lock(&self.state).bindings.get(node).cloned())
    }

    async fn bind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<BindOutcome, IdentityError> {
        let mut state = Self::lock(&self.state);

        // Every refusal is decided before the first mutation, so a rejected
        // bind writes nothing at all — the in-memory reading of "one
        // transaction".
        if !state.accounts.contains_key(&account) {
            return Err(IdentityError::UnknownAccount(account));
        }
        if let Some(existing) = state.bindings.get(node) {
            if existing.account == account {
                return Ok(BindOutcome::AlreadyBound);
            }
            return Err(IdentityError::NodeBoundElsewhere {
                node: *node,
                account: existing.account,
            });
        }
        let Some(row) = state.accounts.get(&account) else {
            return Err(IdentityError::UnknownAccount(account));
        };
        if row.bound_nodes.len() >= MAX_BOUND_NODES_PER_ACCOUNT {
            return Err(IdentityError::TooManyBoundNodes {
                account,
                cap: MAX_BOUND_NODES_PER_ACCOUNT,
            });
        }

        let mut row = row.clone();
        row.bound_nodes.push(*node);
        fold_event(&mut row, at_ms);
        state.accounts.insert(account, row);
        state.bindings.insert(
            *node,
            BindingRow {
                account,
                bound_at_ms: at_ms,
            },
        );
        state
            .history
            .entry(*node)
            .or_default()
            .push(BindingHistoryRow {
                account,
                kind: BindKind::Bind,
                at_ms,
            });
        Ok(BindOutcome::Bound)
    }

    async fn unbind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<(), IdentityError> {
        let mut state = Self::lock(&self.state);

        if !state.accounts.contains_key(&account) {
            return Err(IdentityError::UnknownAccount(account));
        }
        match state.bindings.get(node) {
            Some(existing) if existing.account == account => {}
            _ => {
                return Err(IdentityError::NotBound {
                    node: *node,
                    account,
                })
            }
        }
        let Some(row) = state.accounts.get(&account) else {
            return Err(IdentityError::UnknownAccount(account));
        };

        let mut row = row.clone();
        row.bound_nodes.retain(|bound| bound != node);
        fold_event(&mut row, at_ms);
        state.accounts.insert(account, row);
        // Deleted, not tombstoned: unbinding is immediate (docs/09 §8) and the
        // released NodeId's lookup becomes a miss, which excludes.
        state.bindings.remove(node);
        state
            .history
            .entry(*node)
            .or_default()
            .push(BindingHistoryRow {
                account,
                kind: BindKind::Unbind,
                at_ms,
            });
        Ok(())
    }

    async fn binding_history(
        &self,
        node: &NodeId,
    ) -> Result<Vec<BindingHistoryRow>, IdentityError> {
        Ok(Self::lock(&self.state)
            .history
            .get(node)
            .cloned()
            .unwrap_or_default())
    }
}

impl BindingAuthority for MemAccountStore {
    fn owner(&self, node: &NodeId) -> Option<AccountId> {
        Self::lock(&self.state)
            .bindings
            .get(node)
            .map(|row| row.account)
    }
}
