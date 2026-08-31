//! The object-store seam, and the filesystem implementation that stands in for
//! a real one (docs/08-persistence.md §11.6).
//!
//! **Why a trait and a filesystem backend rather than an S3 client.** #808 is
//! the tailer: sealed-segment consumption, the re-sort, verified publication,
//! retry, and the watermark. Every one of those is testable against any store
//! that can `put`, `get` and `delete` by key, and none of them is *about* S3.
//! A real client (`object_store`, or the AWS SDK) brings its own async runtime,
//! its own credential chain, its own retry layer competing with this module's,
//! and a region/endpoint configuration surface — a second reviewable change
//! that would arrive fused to this one. So the seam lands here with the one
//! backend that needs no configuration, and the S3 implementation is a later
//! change against a trait whose contract the tailer's tests already pin.
//!
//! The contract the trait states is the one the crash-safety argument depends
//! on, and it is stated here because a filesystem gives it for free while an
//! object store gives it only if asked:
//!
//! - **`put` is idempotent by key.** Writing the same key twice with the same
//!   bytes is indistinguishable from writing it once. This is what makes the
//!   retry after a crash a no-op rather than a duplicate.
//! - **`put` is atomic.** A reader never observes a partially written object.
//!   `FsArchiveStore` gets this from write-to-temp-then-rename; S3 gets it from
//!   single-part PUT semantics, and a multipart implementation must complete
//!   the upload before the key is visible.
//! - **`get` reads back what the store holds**, not a cache of what was just
//!   written. §11.3 is explicit that "a checksum nobody re-reads is not a
//!   verification", so a `get` that could be served from the writer's own
//!   buffer would void the whole check.

use std::path::{Path, PathBuf};

/// Why an object-store operation failed.
///
/// One variant, carrying the store's own message: the tailer's retry policy
/// does not branch on the kind of failure — an unreachable store, a refused
/// credential and a full disk all mean "the object is not durably there", and
/// all three must hold the watermark. What the operator needs is the message,
/// which is why it is carried rather than discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveStoreError(pub String);

impl core::fmt::Display for ArchiveStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for ArchiveStoreError {}

/// The object store the archive tailer publishes to.
///
/// Deliberately synchronous. The tailer calls it from a `spawn_blocking` task
/// rather than from the runtime that carries the bulk write path, because a
/// segment's worth of Parquet encoding plus an upload and a read-back is
/// hundreds of milliseconds and D16's journal-commit budget is 2 ms. A sync
/// trait is also what keeps this module free of a second async abstraction
/// layer (`async_trait` boxing per object) for a call that happens once per
/// 128 MiB — and it is what a future S3 implementation will have to respect by
/// blocking on its own client rather than leaking a runtime into this seam.
pub trait ArchiveStore: Send + Sync {
    /// Write `bytes` under `key`, atomically and idempotently.
    ///
    /// # Errors
    ///
    /// [`ArchiveStoreError`] if the object is not durably stored under `key`.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), ArchiveStoreError>;

    /// Read the object stored under `key` back from the store.
    ///
    /// `Ok(None)` when no such object exists — which is a normal answer during
    /// verification of an upload that did not land, not an error.
    ///
    /// # Errors
    ///
    /// [`ArchiveStoreError`] if the store could not be reached or read.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ArchiveStoreError>;
}

/// An [`ArchiveStore`] over a local directory tree.
///
/// The development and single-node backend, and the one every test in this
/// crate runs against. Object keys are treated as `/`-separated relative
/// paths under `root`; a key that escapes the root is refused rather than
/// resolved, because the key is derived from a `NodeId` and a segment number
/// and a key that walks upward means the derivation is wrong.
#[derive(Debug, Clone)]
pub struct FsArchiveStore {
    root: PathBuf,
}

impl FsArchiveStore {
    /// Open (creating if needed) a filesystem archive store rooted at `root`.
    ///
    /// # Errors
    ///
    /// [`ArchiveStoreError`] if the root cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ArchiveStoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| {
            ArchiveStoreError(format!("create archive root {}: {e}", root.display()))
        })?;
        Ok(Self { root })
    }

    fn resolve(&self, key: &str) -> Result<PathBuf, ArchiveStoreError> {
        if key.is_empty() {
            return Err(ArchiveStoreError("empty object key".into()));
        }
        let mut path = self.root.clone();
        for part in key.split('/') {
            if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
                return Err(ArchiveStoreError(format!(
                    "object key {key} is not a plain relative path"
                )));
            }
            path.push(part);
        }
        Ok(path)
    }
}

impl ArchiveStore for FsArchiveStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), ArchiveStoreError> {
        let path = self.resolve(key)?;
        let parent: &Path = path
            .parent()
            .ok_or_else(|| ArchiveStoreError(format!("object key {key} has no parent")))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| ArchiveStoreError(format!("create {}: {e}", parent.display())))?;
        // Write-then-rename, so a crash mid-write leaves the key either absent
        // or complete and never truncated. A truncated object that verified
        // against a truncated re-read is exactly the failure §11.3's
        // "re-read the stored object" is guarding against, and atomicity is
        // what keeps that guard from having to also range-check the length.
        let temp = path.with_extension("part");
        std::fs::write(&temp, bytes)
            .map_err(|e| ArchiveStoreError(format!("write {}: {e}", temp.display())))?;
        std::fs::rename(&temp, &path)
            .map_err(|e| ArchiveStoreError(format!("publish {}: {e}", path.display())))?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ArchiveStoreError> {
        let path = self.resolve(key)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ArchiveStoreError(format!("read {}: {e}", path.display()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filesystem_store_round_trips_and_overwrites_a_key_idempotently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsArchiveStore::open(dir.path()).expect("open");
        assert_eq!(store.get("jarchive/a/0.parquet").expect("get"), None);
        store.put("jarchive/a/0.parquet", b"first").expect("put");
        store.put("jarchive/a/0.parquet", b"first").expect("re-put");
        assert_eq!(
            store.get("jarchive/a/0.parquet").expect("get"),
            Some(b"first".to_vec()),
            "the same key written twice holds one object"
        );
    }

    #[test]
    fn a_key_that_escapes_the_root_is_refused_rather_than_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsArchiveStore::open(dir.path()).expect("open");
        assert!(store.put("../escape", b"x").is_err());
        assert!(store.put("jarchive/../../escape", b"x").is_err());
        assert!(store.put("", b"x").is_err());
    }
}
