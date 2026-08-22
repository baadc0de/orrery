//! An in-process [`AccountStore`], for tests and for harnesses with no cluster.
//!
//! It keeps the same maps the `d` family keeps as sub-spans — accounts,
//! bindings, history and D36's per-account rate window as a fourth — and it
//! mutates all of them under **one** lock, which is the in-memory reading of
//! D31 clause (b): a reader can never observe `da` and `db` disagreeing,
//! because there is no instant between the two writes at which a reader runs.
//! A refusal (an account at its binding cap, its rate window full, a NodeId
//! spoken for elsewhere) happens before any map is touched, so a rejected
//! bind leaves nothing behind.
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
use crate::window::{admit_binding_event, rate_limited};
use async_trait::async_trait;
use orrery_persistd::gateway::BindingAuthority;
use orrery_persistd::keyspace::{
    AccountRow, BindKind, BindingHistoryRow, BindingRow, MAX_BOUND_NODES_PER_ACCOUNT,
};
use orrery_protocol::AccountId;
use orrery_protocol::NodeId;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The sub-spans, as maps under one lock.
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
    /// `dw ‖ account` — the binding-rate window (D36), the ascending stamps of
    /// every event this account filed inside its trailing 30 days. Written in
    /// the same critical section as `da`, `db` and `dh`, never before the
    /// refusal checks.
    windows: HashMap<AccountId, Vec<u64>>,
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
        // D36 (b): the rate check runs after the concurrency cap and before
        // anything is staged. Computed against the window's current contents;
        // only the admitted vector is carried into the mutations below.
        let window = {
            let stamps = state
                .windows
                .get(&account)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            match admit_binding_event(stamps, at_ms) {
                Ok(next) => next,
                Err(refusal) => return Err(rate_limited(account, refusal)),
            }
        };

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
        state.windows.insert(account, window);
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
        // An unbind is an event too (D36 (b), property 2): refusing it here
        // aborts the whole mutation, so the binding stays in place and device
        // removal waits for a window slide.
        let window = {
            let stamps = state
                .windows
                .get(&account)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            match admit_binding_event(stamps, at_ms) {
                Ok(next) => next,
                Err(refusal) => return Err(rate_limited(account, refusal)),
            }
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
        state.windows.insert(account, window);
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

#[cfg(test)]
mod tests {
    //! Enforcement wiring: the window logic's semantics have their own unit
    //! tests in [`crate::window`]; what is proven here is that the store
    //! consults them on every event path, in D36 (b)'s check order, and that a
    //! refusal — like every refusal here — happens before any map is mutated.
    //!
    //! One accounting note the scenarios below lean on: pure binds can never
    //! reach the rate cap, because nine concurrent binds hit
    //! `TooManyBoundNodes` first (it is checked earlier by design), so the
    //! short window is filled with bind/unbind pairs — two events per pair,
    //! and concurrency back to zero at the end of each.

    use super::*;
    use crate::window::{BINDING_RATE_CAP_24H, BINDING_RATE_CAP_30D, BINDING_RATE_WINDOW_30D_MS};

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const ACCOUNT: AccountId = AccountId(0x0255_0000_0000_0001);

    fn node(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = 0x21;
        iroh_base::SecretKey::from_bytes(&bytes).public()
    }

    /// The account's stored `dw` vector, read whole.
    fn window_of(store: &MemAccountStore) -> Vec<u64> {
        MemAccountStore::lock(&store.state)
            .windows
            .get(&ACCOUNT)
            .cloned()
            .unwrap_or_default()
    }

    async fn fresh_store() -> MemAccountStore {
        let store = MemAccountStore::new();
        store.create_account(ACCOUNT, 1_000).await.expect("create");
        store
    }

    /// Two binding events: bind `device`, then release it again. Concurrency
    /// ends where it started; the window grows by two.
    async fn bind_unbind_pair(store: &MemAccountStore, device: &NodeId, t_bind: u64) {
        assert_eq!(
            store.bind(ACCOUNT, device, t_bind).await.expect("bind"),
            BindOutcome::Bound
        );
        store
            .unbind(ACCOUNT, device, t_bind + 1)
            .await
            .expect("unbind");
    }

    #[tokio::test]
    async fn the_ninth_event_inside_24h_is_refused_and_consumes_nothing() {
        let t0 = 1_000 * DAY;
        let store = fresh_store().await;

        // Eight admitted events inside one trailing-24 h span.
        for k in 0..4u8 {
            bind_unbind_pair(&store, &node(0x30 + k), t0 + 2 * u64::from(k)).await;
        }
        assert_eq!(window_of(&store).len(), BINDING_RATE_CAP_24H);

        // The ninth — a fresh bind, concurrency zero — is refused, naming the
        // tripped window and its cap.
        let ninth = node(0x40);
        let error = store
            .bind(ACCOUNT, &ninth, t0 + 8)
            .await
            .expect_err("the ninth event inside 24 h is refused");
        assert_eq!(
            error,
            IdentityError::BindingRateLimited {
                account: ACCOUNT,
                window_ms: DAY,
                cap: BINDING_RATE_CAP_24H,
            }
        );

        // Nothing was consumed: no slot, no `db` row, no `dh` append, no fold.
        assert_eq!(window_of(&store).len(), BINDING_RATE_CAP_24H);
        assert!(store.binding(&ninth).await.expect("read").is_none());
        assert!(
            store
                .binding_history(&ninth)
                .await
                .expect("history")
                .is_empty(),
            "a refused bind appends no history row"
        );
        let row = store.account(ACCOUNT).await.expect("read").expect("row");
        assert!(row.bound_nodes.is_empty());
        assert_eq!(
            row.binding_event_count,
            u32::try_from(BINDING_RATE_CAP_24H).unwrap()
        );
    }

    #[tokio::test]
    async fn the_65th_event_inside_30d_is_refused_while_each_days_8_stay_admitted() {
        // Eight events per slot for eight slots, two days apart — every
        // octet under the short cap (and fully out of its trailing-24 h count
        // before the next begins), so all sixty-four admissions prove nothing
        // was over-counted into an early refusal.
        let t0 = 1_000 * DAY;
        let store = fresh_store().await;
        for day in 0..8u64 {
            let day_start = t0 + 2 * day * DAY;
            for k in 0..4u8 {
                let device = node(k);
                store
                    .bind(ACCOUNT, &device, day_start + 2 * u64::from(k))
                    .await
                    .expect("each day's binds stay admitted");
                store
                    .unbind(ACCOUNT, &device, day_start + 2 * u64::from(k) + 1)
                    .await
                    .expect("each day's unbinds stay admitted");
            }
        }
        assert_eq!(window_of(&store).len(), BINDING_RATE_CAP_30D);

        // Two days past the last octet: the trailing-24 h count there is
        // zero, so only the long window can refuse — and it does, sitting
        // exactly at its 64.
        let error = store
            .bind(ACCOUNT, &node(0x40), t0 + 16 * DAY)
            .await
            .expect_err("the 65th in-window event refuses");
        assert_eq!(
            error,
            IdentityError::BindingRateLimited {
                account: ACCOUNT,
                window_ms: BINDING_RATE_WINDOW_30D_MS,
                cap: BINDING_RATE_CAP_30D,
            },
            "refused even though the 24 h cap is satisfied"
        );
        assert_eq!(window_of(&store).len(), BINDING_RATE_CAP_30D);
    }

    #[tokio::test]
    async fn stamps_older_than_a_window_stop_counting() {
        // Prune correctness through the store itself: saturate the short
        // window, slide past it and watch admission resume; then slide past
        // both windows and watch the stored vector empty down to one stamp.
        let t0 = 1_000 * DAY;
        let store = fresh_store().await;
        for k in 0..4u8 {
            bind_unbind_pair(&store, &node(0x30 + k), t0 + 2 * u64::from(k)).await;
        }
        let error = store.bind(ACCOUNT, &node(0x40), t0 + 8).await.unwrap_err();
        assert!(matches!(error, IdentityError::BindingRateLimited { .. }));

        // One ms past the short edge the same shape of bind is fine: seven
        // old stamps still sit inside the trailing-24 h count, under its cap
        // of eight. The stored vector keeps every stamp still inside 30 days
        // — all eight old ones plus the new one; pruning is against the long
        // horizon only.
        store
            .bind(ACCOUNT, &node(0x41), t0 + DAY + 1)
            .await
            .expect("the short window slid");
        assert_eq!(window_of(&store).len(), 9);

        // Thirty-one days on, nothing survives either window.
        let late = t0 + 31 * DAY + 2;
        store
            .bind(ACCOUNT, &node(0x42), late)
            .await
            .expect("everything aged out of both windows");
        assert_eq!(window_of(&store), vec![late]);
    }

    #[tokio::test]
    async fn already_bound_consumes_nothing_an_unbind_consumes_one_slot() {
        let t0 = 1_000 * DAY;
        let store = fresh_store().await;
        let device = node(0x50);

        assert_eq!(
            store.bind(ACCOUNT, &device, t0).await.expect("bind"),
            BindOutcome::Bound
        );
        assert_eq!(window_of(&store).len(), 1);
        // Re-asserting a binding that already holds appends no `dh` row, so it
        // must consume no slot either (D36 (b), property 2).
        assert_eq!(
            store.bind(ACCOUNT, &device, t0 + 1).await.expect("rebind"),
            BindOutcome::AlreadyBound
        );
        assert_eq!(window_of(&store).len(), 1);

        // Six more events fill the short window to its eight: three pairs,
        // then one bind that stays — so a live binding exists for the refused
        // unbind below.
        for k in 1..4u8 {
            bind_unbind_pair(&store, &node(0x50 + k), t0 + 2 * u64::from(k)).await;
        }
        let bound_now = node(0x54);
        store
            .bind(ACCOUNT, &bound_now, t0 + 8)
            .await
            .expect("the eighth event stays admitted");
        assert_eq!(window_of(&store).len(), 8);
        assert_eq!(
            store
                .binding(&bound_now)
                .await
                .expect("read")
                .map(|b| b.account),
            Some(ACCOUNT)
        );

        // Refused bind consumes nothing …
        let error = store.bind(ACCOUNT, &node(0x60), t0 + 9).await.unwrap_err();
        assert!(matches!(error, IdentityError::BindingRateLimited { .. }));
        assert_eq!(window_of(&store).len(), 8);

        // … and a refused unbind leaves the binding standing: the transaction
        // aborts wholesale, so removal waits for a window slide (D36 (b)).
        let error = store
            .unbind(ACCOUNT, &bound_now, t0 + 10)
            .await
            .unwrap_err();
        assert!(matches!(error, IdentityError::BindingRateLimited { .. }));
        assert_eq!(
            store
                .binding(&bound_now)
                .await
                .expect("read")
                .map(|b| b.account),
            Some(ACCOUNT),
            "a rate-limited unbind must not release the node"
        );

        // One ms past the edge, the pending removal goes through.
        store
            .unbind(ACCOUNT, &bound_now, t0 + DAY + 11)
            .await
            .expect("the window slid and the unbind lands");
        assert!(store.binding(&bound_now).await.expect("read").is_none());
    }
}
