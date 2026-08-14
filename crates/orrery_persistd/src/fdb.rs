//! Process-scoped FoundationDB client context.
//!
//! FoundationDB's C client selects its API version and starts its network only
//! once per process.  A persistd process uses all three durable adapters, so
//! they must share this context rather than each independently calling
//! [`foundationdb::boot`].

#![allow(unsafe_code)] // This module contains the crate's sole FDB boot call.

use std::fmt;
use std::sync::Arc;

use foundationdb::Database;

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
    /// Start the FDB client network and open `cluster_file`.
    pub fn connect(cluster_file: &str) -> Result<Self, FdbContextError> {
        boot_network();
        let db = Database::from_path(cluster_file)
            .map_err(|e| FdbContextError(format!("connect: {e}")))?;
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
/// share the process-wide client bootstrap.
pub fn fdb_network() {
    boot_network();
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
