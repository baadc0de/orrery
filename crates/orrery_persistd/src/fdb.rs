//! Process-scoped FoundationDB client context.
//!
//! FoundationDB's C client selects its API version and starts its network only
//! once per process.  A persistd process uses all three durable adapters, so
//! they must share this context rather than each independently calling
//! [`foundationdb::boot`].
//!
//! # Every transaction is bounded
//!
//! The C client's default is to retry forever: when the cluster is
//! unreachable, or the cluster file names a host that does not resolve, a
//! `db.run` never resolves at all. A hang is the worst failure a durable tier
//! can have — nothing upstream can queue behind a call that does not return,
//! and no failure-mode test can assert on one. So [`FdbContext::connect`] sets
//! two database options on every handle it opens, and both are load-bearing:
//!
//! - [`DEFAULT_TRANSACTION_TIMEOUT_MS`] caps the wall time a transaction may
//!   spend, and it is the load-bearing half. Set on the *database* under API
//!   version 610 or later it covers the whole `db.run` — every retry included
//!   — and it is the only one of the two that bounds an unreachable cluster,
//!   because waiting for a first connection is not an error the retry loop
//!   ever sees. Measured against a cluster file nothing is listening on, a
//!   durable read returns in 10.0 s with this option and does not return at
//!   all without it.
//!
//!   Ten seconds, not five: FoundationDB's own five-second limit on the age of
//!   a read version means no legitimate attempt can outlive five, so double
//!   that cannot cut short the longest transaction this codebase issues — a
//!   checkpoint's final metadata commit, which follows the row chunks and may
//!   retry on `not_committed` under shard contention.
//! - [`DEFAULT_TRANSACTION_RETRY_LIMIT`] caps how many times `onError` re-runs
//!   the closure. It is a backstop against a *fast* retryable-error spin — a
//!   `not_committed` storm, where each attempt costs milliseconds and the
//!   timeout would be the only thing to stop it — and not part of the
//!   unreachable-cluster bound: with the limit set to `-1` (unlimited) the
//!   measurement above is unchanged at 10.0 s. The default sits well above the
//!   intent executor's own five-attempt conflict budget, so a legitimate
//!   conflict retry still reaches its own typed
//!   `IntentError::ContentionExhausted` first.
//!
//! Both are overridable per process by
//! [`TRANSACTION_TIMEOUT_ENV`] and [`TRANSACTION_RETRY_LIMIT_ENV`]; an
//! unparseable value is a hard error rather than a silent fall back to the
//! default, because a misconfigured bound is indistinguishable from no bound.
//! Both accept FoundationDB's own disabling values — `0` for the timeout,
//! `-1` for the retry limit — which is how the regression test in
//! `tests/area_load.rs` demonstrates that removing the timeout restores the
//! hang.
//!
//! The resulting failure is an ordinary `…Error::Store` at every adapter —
//! the same class those call sites already surface for a decode failure or a
//! rejected commit — so bounding the calls introduced no new error type to
//! handle. What it did introduce is a new *cause*: a store error can now mean
//! "the cluster did not answer in time" and not only "the cluster refused".

#![allow(unsafe_code)] // This module contains the crate's sole FDB boot call.

use std::fmt;
use std::sync::Arc;

use foundationdb::options::DatabaseOption;
use foundationdb::Database;

/// Wall-clock budget for one FoundationDB transaction, in milliseconds.
///
/// See the module documentation for why ten seconds and not five.
pub const DEFAULT_TRANSACTION_TIMEOUT_MS: i32 = 10_000;

/// Maximum number of `onError` retries for one FoundationDB transaction.
///
/// See the module documentation for why this is required alongside the
/// timeout rather than an alternative to it.
pub const DEFAULT_TRANSACTION_RETRY_LIMIT: i32 = 20;

/// Environment override for [`DEFAULT_TRANSACTION_TIMEOUT_MS`].
pub const TRANSACTION_TIMEOUT_ENV: &str = "ORRERY_FDB_TRANSACTION_TIMEOUT_MS";

/// Environment override for [`DEFAULT_TRANSACTION_RETRY_LIMIT`].
pub const TRANSACTION_RETRY_LIMIT_ENV: &str = "ORRERY_FDB_TRANSACTION_RETRY_LIMIT";

/// An error while opening the process's FoundationDB client.
#[derive(Debug, Clone)]
pub struct FdbContextError(String);

impl fmt::Display for FdbContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FdbContextError {}

/// The shared FoundationDB network and database handle for one process.
///
/// Construct this once at process startup and pass it to every FDB-backed
/// adapter. The network guard is intentionally process-lifetime: the client
/// must outlive all database handles, and the operating system reclaims it at
/// exit.
#[derive(Clone)]
pub struct FdbContext {
    db: Arc<Database>,
}

impl FdbContext {
    /// Start the FDB client network and open the platform's default cluster
    /// configuration.
    ///
    /// Services without a cluster-file CLI surface use FoundationDB's normal
    /// `FDB_CLUSTER_FILE`/platform-default resolution while retaining the same
    /// transaction bounds as [`Self::connect`].
    pub fn connect_default() -> Result<Self, FdbContextError> {
        boot_network();
        let db = Database::default().map_err(|e| FdbContextError(format!("connect: {e}")))?;
        bound_transactions(&db)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Start the FDB client network and open `cluster_file`, with every
    /// transaction on the resulting handle bounded.
    ///
    /// The signature is deliberately stable: four adapters, the persistd
    /// binary and the `gates/p2-load` rig all funnel through it, and they inherit
    /// the bounds without a call-site change.
    ///
    /// Opening is lazy — `Database::from_path` succeeds against a cluster
    /// nothing is listening on, because the client connects on first use — so
    /// a failure here means the cluster *file* is unusable, and an unreachable
    /// cluster surfaces later as a bounded transaction error.
    pub fn connect(cluster_file: &str) -> Result<Self, FdbContextError> {
        boot_network();
        let db = Database::from_path(cluster_file)
            .map_err(|e| FdbContextError(format!("connect: {e}")))?;
        bound_transactions(&db)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Clone the database handle shared by the adapters in this process.
    #[must_use]
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }
}

/// Start the FoundationDB client network without opening a database.
///
/// This is for callers that own their own database handle but still need to
/// share the process-wide client bootstrap. Such a caller must apply
/// [`bound_transactions`] itself; prefer [`FdbContext::connect`], which does
/// both.
pub fn fdb_network() {
    boot_network();
}

/// Apply this process's transaction timeout and retry limit to `db`.
///
/// [`FdbContext::connect`] calls this. It is public for the one caller that
/// cannot: a handle opened directly from [`foundationdb::Database::from_path`]
/// must be bounded the same way or it inherits the C client's retry-forever
/// default.
pub fn bound_transactions(db: &Database) -> Result<(), FdbContextError> {
    let timeout_ms = env_i32(TRANSACTION_TIMEOUT_ENV, DEFAULT_TRANSACTION_TIMEOUT_MS)?;
    let retry_limit = env_i32(TRANSACTION_RETRY_LIMIT_ENV, DEFAULT_TRANSACTION_RETRY_LIMIT)?;
    db.set_option(DatabaseOption::TransactionTimeout(timeout_ms))
        .map_err(|e| FdbContextError(format!("set transaction timeout {timeout_ms}ms: {e}")))?;
    db.set_option(DatabaseOption::TransactionRetryLimit(retry_limit))
        .map_err(|e| FdbContextError(format!("set transaction retry limit {retry_limit}: {e}")))?;
    Ok(())
}

/// Read an `i32` bound from the environment, or fall back to `default`.
fn env_i32(key: &str, default: i32) -> Result<i32, FdbContextError> {
    match std::env::var(key) {
        Ok(raw) => raw
            .trim()
            .parse::<i32>()
            .map_err(|e| FdbContextError(format!("{key}={raw:?} is not a transaction bound: {e}"))),
        Err(_) => Ok(default),
    }
}

/// The cluster file a development or test process should use, if any.
///
/// Honors `ORRERY_FDB_CLUSTER_FILE`; otherwise walks up from the working
/// directory for a `.fdb-dev/fdb.cluster`, because a `cargo test` runs with
/// the crate directory as its CWD rather than the workspace root
/// (AGENTS.md documents the upward walk's reach as intended).
///
/// This is the one discovery rule for both crates' FDB-gated test tiers; a
/// per-file copy is how they drifted into an env-only variant and a walking
/// variant that disagreed about when a suite runs.
#[must_use]
pub fn discover_cluster_file() -> Option<String> {
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

/// Boot FoundationDB exactly once for this Rust process.
fn boot_network() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once` enforces the FoundationDB C client's one-boot-per-
        // process requirement. Leaking the guard keeps its network alive until
        // process exit, after all contexts and database handles are gone.
        let guard = unsafe { foundationdb::boot() };
        std::mem::forget(guard);
    });
}
