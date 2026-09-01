//! The account-store seam: D31's `da`, `db` and `dh` rows behind one trait.
//!
//! Two implementations back it — [`crate::mem::MemAccountStore`] for tests and
//! harnesses, and [`crate::fdb::FdbAccountStore`] behind the `fdb` feature —
//! and the trait exists so the default build needs no `libfdb_c`, the same
//! posture `orrery_persistd` and `orrery_seed` keep.
//!
//! # Row types are borrowed, not redefined
//!
//! [`AccountRow`], [`BindingRow`], [`BindingHistoryRow`] and [`BindKind`] come
//! from `orrery_persistd::keyspace`, where #209 landed them against D31. A
//! second definition here would be a second encoding of the same bytes, and
//! D31 clause (b) — `db` written in the transaction that writes `da`, so the
//! two are never observed disagreeing — is exactly the property two encodings
//! would eventually break.
//!
//! # Async, and object-safe
//!
//! `#[async_trait]` for the reason `orrery_persistd::fence::FenceStore` gives:
//! the FDB implementation drives async transactions, the in-memory one is
//! trivially async, and the attribute keeps the trait usable as
//! `&dyn AccountStore`.

use async_trait::async_trait;
use orrery_persistd::keyspace::{AccountRow, BindingHistoryRow, BindingRow};
use orrery_protocol::AccountId;
use orrery_protocol::NodeId;
use std::fmt;

/// What a [`AccountStore::bind`] call did.
///
/// A repeated bind of a pair that is already bound is
/// [`BindOutcome::AlreadyBound`] and appends **no** `dh` row: D31 clause (c)'s
/// history is a log of binding *events*, and re-asserting a binding that
/// already holds is not one. Making it one would let a caller inflate
/// [`AccountRow::binding_event_count`] and the append-only log for free, which
/// is the storage amplifier clause (g)'s rate cap exists to bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// The NodeId was not bound and now is; one `dh` row was appended.
    Bound,
    /// The NodeId was already bound to this account; nothing was written.
    AlreadyBound,
}

/// The identity-owned start instant of the current cooldown interval.
///
/// This is derived durable state, rather than part of the executor-owned
/// strike ledger: identity alone decides standing and admission, and this
/// value has to survive an identity restart. Its FDB representation is the
/// `dc ‖ account` row in the `d` family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CooldownEntry {
    /// Wall-clock instant at which identity entered or restarted cooldown.
    pub entered_at_ms: u64,
}

/// A typed failure from the identity store or the service above it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentityError {
    /// No `da ‖ account` row exists.
    ///
    /// D33 clause (f)'s first non-equivalent absence: "no `da` account row =>
    /// authentication fails; no token". It is never forgiven into a fresh
    /// account, because creating an account is the credentialed, costed
    /// operation D10 item 5 makes the Sybil anchor.
    UnknownAccount(AccountId),
    /// An account with this id already exists.
    AccountExists(AccountId),
    /// The NodeId is currently bound to a different account.
    ///
    /// A NodeId binds to at most one account at a time (D31 clause (b)), so
    /// this is refused rather than silently re-pointed: re-pointing would make
    /// `owner(n)` change under a reader that had cached it, and under clause
    /// (f) a *wrong* answer admits where a miss would have excluded.
    NodeBoundElsewhere {
        /// The NodeId that is already spoken for.
        node: NodeId,
        /// The account that currently holds it.
        account: AccountId,
    },
    /// The NodeId is not bound to this account, so there is nothing to release.
    NotBound {
        /// The NodeId that was asked about.
        node: NodeId,
        /// The account the caller claimed held it.
        account: AccountId,
    },
    /// The account already holds
    /// [`orrery_persistd::keyspace::MAX_BOUND_NODES_PER_ACCOUNT`] NodeIds.
    ///
    /// D31 clause (g)'s cap, and the reason [`AccountRow`] is safe to read
    /// whole: eight inline NodeIds bound the row at ~282 B.
    TooManyBoundNodes {
        /// The account at its cap.
        account: AccountId,
        /// The cap that was reached.
        cap: usize,
    },
    /// The account has filed too many binding events inside one rolling
    /// window, so this one is refused (D31 clause (g), enforced per D36).
    ///
    /// Both directions count — a bind and an unbind are both events — and the
    /// refusal stages nothing: the transaction aborts wholesale, so a refused
    /// unbind leaves the binding in place. When both windows would trip, the
    /// 24 h one is named, being checked first.
    BindingRateLimited {
        /// The account whose window is full.
        account: AccountId,
        /// The width of the window that tripped, in milliseconds —
        /// `BINDING_RATE_WINDOW_24H_MS` or `BINDING_RATE_WINDOW_30D_MS`.
        window_ms: u64,
        /// That window's cap — 8 or 64 events.
        cap: usize,
    },
    /// The requested session lifetime is longer than the one-hour policy cap.
    ///
    /// [`orrery_protocol::MAX_SESSION_TOKEN_TTL_MS`], enforced here
    /// so the cap is refused at issuance and not merely at verification.
    TtlAboveCap {
        /// The lifetime the caller asked for, in milliseconds.
        requested_ms: u64,
        /// The policy cap, in milliseconds.
        cap_ms: u64,
    },
    /// A zero-length session lifetime was requested.
    ///
    /// The verifier rejects `now − issued_at >= ttl`, so a zero TTL is a token
    /// that is expired in the instant it is signed.
    ZeroTtl,
    /// The account's standing could not be established.
    ///
    /// D33 clause (f): "a missing or unreadable ledger is never interpreted as
    /// `Good`: identity refuses to mint or refresh the token". The party able
    /// to make the lookup unavailable would otherwise select the branch that
    /// admits a ban.
    StandingUnavailable(AccountId),
    /// The live score is at or above the configured cooldown threshold.
    Cooldown(AccountId),
    /// The live score is at or above the configured ban threshold.
    Banned(AccountId),
    /// The durable store failed.
    Store(String),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAccount(account) => write!(f, "no account record for {}", account.0),
            Self::AccountExists(account) => write!(f, "account {} already exists", account.0),
            Self::NodeBoundElsewhere { node, account } => {
                write!(f, "node {node} is already bound to account {}", account.0)
            }
            Self::NotBound { node, account } => {
                write!(f, "node {node} is not bound to account {}", account.0)
            }
            Self::TooManyBoundNodes { account, cap } => {
                write!(f, "account {} already holds {cap} bound nodes", account.0)
            }
            Self::BindingRateLimited {
                account,
                window_ms,
                cap,
            } => write!(
                f,
                "account {} exceeded its cap of {cap} binding events per rolling {} ms",
                account.0, window_ms
            ),
            Self::TtlAboveCap {
                requested_ms,
                cap_ms,
            } => write!(
                f,
                "requested session lifetime {requested_ms} ms exceeds the {cap_ms} ms cap"
            ),
            Self::ZeroTtl => f.write_str("a zero-length session lifetime was requested"),
            Self::StandingUnavailable(account) => {
                write!(f, "standing for account {} is unavailable", account.0)
            }
            Self::Cooldown(account) => write!(f, "account {} is in cooldown", account.0),
            Self::Banned(account) => write!(f, "account {} is banned", account.0),
            Self::Store(message) => write!(f, "identity store: {message}"),
        }
    }
}

impl core::error::Error for IdentityError {}

impl From<postcard::Error> for IdentityError {
    fn from(error: postcard::Error) -> Self {
        Self::Store(format!("encode/decode: {error}"))
    }
}

/// The durable identity state D31 assigns to this service.
///
/// Every method here is a write or a read of the `d` family, and this crate is
/// its **sole writer** (D31 clause (d)). That is not a style preference: `db`
/// must be written in the same transaction as `da`, and a second writer would
/// have to be trusted to maintain an index whose staleness is a security
/// property under clause (f).
#[async_trait]
pub trait AccountStore: Send + Sync {
    /// Create an account record with no bindings.
    ///
    /// Fails with [`IdentityError::AccountExists`] rather than overwriting: an
    /// overwrite would silently drop every binding in the row while leaving the
    /// `db` rows that point at it, which is precisely the `da`/`db`
    /// disagreement clause (b) exists to prevent.
    async fn create_account(
        &self,
        account: AccountId,
        created_ms: u64,
    ) -> Result<(), IdentityError>;

    /// Read `da ‖ account`.
    async fn account(&self, account: AccountId) -> Result<Option<AccountRow>, IdentityError>;

    /// Read `db ‖ node` — the reverse index, and the only direction any
    /// consumer in this epic reads.
    ///
    /// `Ok(None)` is *unresolved*, never *resolved to nobody*: D31 clause (f)
    /// gives it exactly one reading, which is that a miss excludes.
    async fn binding(&self, node: &NodeId) -> Result<Option<BindingRow>, IdentityError>;

    /// Bind `node` to `account`, writing `da`, `db`, `dh` and the D36 window
    /// row together.
    ///
    /// `docs/09-services-and-ops.md` §8 requires credentials to bind. Proving
    /// them is the caller's; this is the durable half. The event is checked
    /// against the account's binding-rate window (D31 clause (g) via D36)
    /// inside the same transaction; a refusal stages nothing.
    async fn bind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<BindOutcome, IdentityError>;

    /// Release `node` from `account`, deleting `db ‖ node` and appending `dh`.
    ///
    /// Unbinding is **immediate** (`docs/09` §8) and the `db` row is deleted
    /// rather than tombstoned, so the released NodeId's lookup becomes a miss —
    /// and a miss excludes, which is what makes shedding a NodeId just before
    /// submitting buy an attacker nothing (D31 clause (g)).
    async fn unbind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<(), IdentityError>;

    /// Read one node's `dh` span in commit order, oldest first.
    ///
    /// The bounded, contiguous read D31 clause (b) keys the history by node to
    /// get: the audit's question is `owner_t(n)` for the ≤ 7 announced NodeIds.
    async fn binding_history(&self, node: &NodeId)
        -> Result<Vec<BindingHistoryRow>, IdentityError>;

    /// Observe a score at or above the cooldown boundary and return the
    /// durable entry instant that governs its dwell floor.
    ///
    /// Creates an entry at `observed_at_ms` when one is absent. That is the
    /// explicit rollout rule for an account already in cooldown when this row
    /// ships: no historical entry instant exists, so first observation begins
    /// a full conservative dwell rather than silently releasing it. A newer
    /// positive live strike restarts the entry at `observed_at_ms`, but an
    /// already-observed strike cannot repeatedly restart it.
    async fn observe_cooldown(
        &self,
        account: AccountId,
        observed_at_ms: u64,
        newest_live_strike_ms: Option<u64>,
    ) -> Result<CooldownEntry, IdentityError>;

    /// Read the current durable cooldown entry, if this account has one.
    async fn cooldown_entry(
        &self,
        account: AccountId,
    ) -> Result<Option<CooldownEntry>, IdentityError>;

    /// Clear one cooldown entry only if it is still the observation's entry.
    ///
    /// The boolean is false when a concurrent observation restarted the
    /// cooldown (or no longer finds the expected row); callers must refuse in
    /// that case rather than clear the newer interval.
    async fn clear_cooldown_if(
        &self,
        account: AccountId,
        expected: CooldownEntry,
    ) -> Result<bool, IdentityError>;
}

/// Forward through a shared handle, so one store can back several services.
///
/// `docs/09-services-and-ops.md` §3 puts identity behind "stateless replicas
/// (≥2) … FDB is the store of record", and within one process the same shape
/// appears whenever two things hold the store: a login path and an admin
/// binding path, or a test that rotates keys between two service values. Both
/// want the store shared rather than moved.
#[async_trait]
impl<T: AccountStore + ?Sized> AccountStore for std::sync::Arc<T> {
    async fn create_account(
        &self,
        account: AccountId,
        created_ms: u64,
    ) -> Result<(), IdentityError> {
        (**self).create_account(account, created_ms).await
    }

    async fn account(&self, account: AccountId) -> Result<Option<AccountRow>, IdentityError> {
        (**self).account(account).await
    }

    async fn binding(&self, node: &NodeId) -> Result<Option<BindingRow>, IdentityError> {
        (**self).binding(node).await
    }

    async fn bind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<BindOutcome, IdentityError> {
        (**self).bind(account, node, at_ms).await
    }

    async fn unbind(
        &self,
        account: AccountId,
        node: &NodeId,
        at_ms: u64,
    ) -> Result<(), IdentityError> {
        (**self).unbind(account, node, at_ms).await
    }

    async fn binding_history(
        &self,
        node: &NodeId,
    ) -> Result<Vec<BindingHistoryRow>, IdentityError> {
        (**self).binding_history(node).await
    }

    async fn observe_cooldown(
        &self,
        account: AccountId,
        observed_at_ms: u64,
        newest_live_strike_ms: Option<u64>,
    ) -> Result<CooldownEntry, IdentityError> {
        (**self)
            .observe_cooldown(account, observed_at_ms, newest_live_strike_ms)
            .await
    }

    async fn clear_cooldown_if(
        &self,
        account: AccountId,
        expected: CooldownEntry,
    ) -> Result<bool, IdentityError> {
        (**self).clear_cooldown_if(account, expected).await
    }

    async fn cooldown_entry(
        &self,
        account: AccountId,
    ) -> Result<Option<CooldownEntry>, IdentityError> {
        (**self).cooldown_entry(account).await
    }
}
