//! Single-scanner archival of `ledger/receipt/` (#832, #837).
//!
//! Each pass captures the greatest receipt key once, then walks only up to
//! that fixed commit versionstamp in bounded pages. Every page is read in a
//! fresh snapshot-only transaction, encoded and verified, and finally marked
//! by one `rarchive/` row. The metadata row's last key is the restart cursor;
//! no separately persisted watermark can disagree with it.
//!
//! The scanner count is intentionally not configurable: it is one. #837 found
//! one continuous 4,096-row scanner indistinguishable from the no-scan intent
//! baseline, while four raised intent p50 by about 19% and p99 by 9–16% (to
//! 1.279 ms). If one scanner cannot keep up on deployment-shaped FDB, or intent
//! p99 approaches D11's 10 ms target, Shape B is reopened instead of silently
//! adding parallel scanners.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::archive::receipt_object::{encode_receipt_object, ReceiptArchiveRow};
use crate::archive::ArchiveStore;
use crate::checkpoint::CheckpointError;
use crate::keyspace::{self, RarchiveMetadata};

/// Number of FDB range scanners used by this path.
pub const RECEIPT_ARCHIVE_SCANNERS: u8 = 1;
/// Page size measured by #837's continuous single-scanner arm.
pub const DEFAULT_RECEIPT_ARCHIVE_PAGE_ROWS: usize = 4_096;

/// A receipt scanner or publication stage failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptArchiveError(pub String);

impl core::fmt::Display for ReceiptArchiveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for ReceiptArchiveError {}

impl From<CheckpointError> for ReceiptArchiveError {
    fn from(value: CheckpointError) -> Self {
        Self(value.to_string())
    }
}

/// Read-only access to the versionstamped receipt stream.
#[async_trait::async_trait]
pub trait ReceiptSource: Send + Sync {
    /// Greatest complete receipt key visible when a pass begins.
    async fn capture_upper(&self) -> Result<Option<[u8; 12]>, ReceiptArchiveError>;

    /// At most `limit` rows strictly after `after` and at or below `upper`.
    async fn read_page(
        &self,
        after: Option<[u8; 12]>,
        upper: [u8; 12],
        limit: usize,
    ) -> Result<Vec<ReceiptArchiveRow>, ReceiptArchiveError>;
}

/// Durable receipt-archive publication markers.
#[async_trait::async_trait]
pub trait ReceiptArchiveIndex: Send + Sync {
    /// Commit the marker for a verified page.
    async fn put(&self, metadata: &RarchiveMetadata) -> Result<(), CheckpointError>;
    /// Greatest committed marker, which is the restart cursor.
    async fn last(&self) -> Result<Option<RarchiveMetadata>, CheckpointError>;
}

/// In-memory source used by paging and fixed-upper tests.
#[derive(Debug, Default)]
pub struct MemReceiptSource {
    rows: std::sync::Mutex<std::collections::BTreeMap<[u8; 12], Vec<u8>>>,
}

impl MemReceiptSource {
    /// Empty receipt stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a fully versionstamped test row.
    pub fn insert(&self, key: [u8; 12], receipt: &keyspace::ReceiptRow) {
        assert_eq!(&key[..2], b"lr", "test source accepts only receipt keys");
        self.rows.lock().expect("receipt source lock").insert(
            key,
            keyspace::encode_receipt_row(receipt).expect("encode receipt"),
        );
    }
}

#[async_trait::async_trait]
impl ReceiptSource for MemReceiptSource {
    async fn capture_upper(&self) -> Result<Option<[u8; 12]>, ReceiptArchiveError> {
        Ok(self
            .rows
            .lock()
            .expect("receipt source lock")
            .last_key_value()
            .map(|(key, _)| *key))
    }

    async fn read_page(
        &self,
        after: Option<[u8; 12]>,
        upper: [u8; 12],
        limit: usize,
    ) -> Result<Vec<ReceiptArchiveRow>, ReceiptArchiveError> {
        let rows = self.rows.lock().expect("receipt source lock");
        rows.iter()
            .filter(|(key, _)| after.is_none_or(|cursor| **key > cursor) && **key <= upper)
            .take(limit)
            .map(|(key, value)| {
                let (receipt, encoding) = keyspace::decode_receipt_row(value)
                    .map_err(|e| ReceiptArchiveError(format!("decode receipt row: {e}")))?;
                Ok(ReceiptArchiveRow {
                    key: *key,
                    receipt,
                    encoding,
                })
            })
            .collect()
    }
}

/// In-memory publication index.
#[derive(Debug, Default)]
pub struct MemReceiptArchiveIndex {
    rows: std::sync::Mutex<std::collections::BTreeMap<[u8; 12], RarchiveMetadata>>,
}

impl MemReceiptArchiveIndex {
    /// Empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of page markers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.lock().expect("receipt index lock").len()
    }

    /// Whether no page marker exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl ReceiptArchiveIndex for MemReceiptArchiveIndex {
    async fn put(&self, metadata: &RarchiveMetadata) -> Result<(), CheckpointError> {
        let key = keyspace::rarchive_key(&metadata.last_receipt_key).ok_or_else(|| {
            CheckpointError::Store("rarchive metadata has a non-receipt cursor".to_owned())
        })?;
        self.rows
            .lock()
            .expect("receipt index lock")
            .insert(key, metadata.clone());
        Ok(())
    }

    async fn last(&self) -> Result<Option<RarchiveMetadata>, CheckpointError> {
        Ok(self
            .rows
            .lock()
            .expect("receipt index lock")
            .last_key_value()
            .map(|(_, metadata)| metadata.clone()))
    }
}

/// Retry, page and cadence settings for the one scanner.
#[derive(Debug, Clone)]
pub struct ReceiptArchiveConfig {
    /// Maximum rows returned by one short read-only transaction.
    pub page_rows: usize,
    /// Poll interval when no newer receipt exists.
    pub idle_interval: Duration,
    /// First retry delay after failure.
    pub backoff_initial: Duration,
    /// Maximum retry delay.
    pub backoff_max: Duration,
}

impl Default for ReceiptArchiveConfig {
    fn default() -> Self {
        Self {
            page_rows: DEFAULT_RECEIPT_ARCHIVE_PAGE_ROWS,
            idle_interval: Duration::from_secs(1),
            backoff_initial: Duration::from_millis(250),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Observable scanner state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptArchiveStatus {
    /// Fixed safe concurrency knob.
    pub scanners: u8,
    /// Configured page size.
    pub page_rows: usize,
    /// Last receipt key durably archived, if any.
    pub cursor: Option<[u8; 12]>,
    /// Fixed pass upper bound currently being walked, if any.
    pub pass_upper: Option<[u8; 12]>,
    /// Verified rows archived since startup.
    pub archived_rows: u64,
    /// Verified page objects archived since startup.
    pub archived_pages: u64,
    /// Completed bounded passes since startup.
    pub full_passes: u64,
    /// Most recent complete-pass duration.
    pub last_full_pass_ms: u64,
    /// Most recent complete-pass throughput.
    pub last_rows_per_second: u64,
}

/// Result of one page-sized unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptTailerPass {
    /// No receipt newer than the durable cursor.
    Idle,
    /// One verified page and marker were published.
    Published {
        /// Rows in the page.
        rows: u64,
        /// Encoded object bytes.
        bytes: u64,
        /// Whether this page reached the pass's fixed upper bound.
        completed_pass: bool,
    },
}

/// One sequential receipt archive scanner.
pub struct ReceiptArchiveTailer {
    source: Arc<dyn ReceiptSource>,
    store: Arc<dyn ArchiveStore>,
    index: Arc<dyn ReceiptArchiveIndex>,
    key_prefix: String,
    config: ReceiptArchiveConfig,
    cursor: Option<[u8; 12]>,
    pass_upper: Option<[u8; 12]>,
    pass_started: Option<Instant>,
    pass_rows: u64,
    archived_rows: AtomicU64,
    archived_pages: AtomicU64,
    full_passes: AtomicU64,
    last_full_pass_ms: AtomicU64,
    last_rows_per_second: AtomicU64,
}

impl ReceiptArchiveTailer {
    /// Open one scanner and recover its cursor from `rarchive/` metadata.
    ///
    /// # Errors
    ///
    /// Returns [`ReceiptArchiveError`] if recovery fails or `page_rows` is zero.
    pub async fn open(
        source: Arc<dyn ReceiptSource>,
        store: Arc<dyn ArchiveStore>,
        index: Arc<dyn ReceiptArchiveIndex>,
        key_prefix: String,
        config: ReceiptArchiveConfig,
    ) -> Result<Self, ReceiptArchiveError> {
        if config.page_rows == 0 {
            return Err(ReceiptArchiveError(
                "receipt archive page_rows must be nonzero".to_owned(),
            ));
        }
        let cursor = index
            .last()
            .await?
            .map(|metadata| metadata.last_receipt_key);
        Ok(Self {
            source,
            store,
            index,
            key_prefix,
            config,
            cursor,
            pass_upper: None,
            pass_started: None,
            pass_rows: 0,
            archived_rows: AtomicU64::new(0),
            archived_pages: AtomicU64::new(0),
            full_passes: AtomicU64::new(0),
            last_full_pass_ms: AtomicU64::new(0),
            last_rows_per_second: AtomicU64::new(0),
        })
    }

    /// Snapshot of scanner configuration and progress.
    #[must_use]
    pub fn status(&self) -> ReceiptArchiveStatus {
        ReceiptArchiveStatus {
            scanners: RECEIPT_ARCHIVE_SCANNERS,
            page_rows: self.config.page_rows,
            cursor: self.cursor,
            pass_upper: self.pass_upper,
            archived_rows: self.archived_rows.load(Ordering::Relaxed),
            archived_pages: self.archived_pages.load(Ordering::Relaxed),
            full_passes: self.full_passes.load(Ordering::Relaxed),
            last_full_pass_ms: self.last_full_pass_ms.load(Ordering::Relaxed),
            last_rows_per_second: self.last_rows_per_second.load(Ordering::Relaxed),
        }
    }

    /// Archive at most one page from a fixed-upper bounded pass.
    ///
    /// # Errors
    ///
    /// Returns [`ReceiptArchiveError`] without advancing the cursor when a
    /// read, encode, upload, verification or metadata commit fails.
    pub async fn pass(&mut self) -> Result<ReceiptTailerPass, ReceiptArchiveError> {
        if self.pass_upper.is_none() {
            let upper = self.source.capture_upper().await?;
            if upper.is_none() || upper <= self.cursor {
                return Ok(ReceiptTailerPass::Idle);
            }
            self.pass_upper = upper;
            self.pass_started = Some(Instant::now());
            self.pass_rows = 0;
        }
        let upper = self.pass_upper.expect("captured above");
        let rows = self
            .source
            .read_page(self.cursor, upper, self.config.page_rows)
            .await?;
        if rows.is_empty() {
            return Err(ReceiptArchiveError(format!(
                "receipt pass promised upper {} but returned no row after {}",
                key_hex(&upper),
                self.cursor
                    .as_ref()
                    .map_or_else(|| "start".to_owned(), |cursor| key_hex(cursor))
            )));
        }
        let first = rows.first().expect("nonempty").key;
        let last = rows.last().expect("nonempty").key;
        if last > upper {
            return Err(ReceiptArchiveError(
                "receipt source crossed its fixed pass upper bound".to_owned(),
            ));
        }
        let object_key = receipt_object_key(&self.key_prefix, &last);
        let row_count = u32::try_from(rows.len())
            .map_err(|_| ReceiptArchiveError("receipt page row count exceeds u32".to_owned()))?;
        let store = Arc::clone(&self.store);
        let publish_key = object_key.clone();
        let (bytes_len, checksum) = tokio::task::spawn_blocking(move || {
            publish_receipt_page(store.as_ref(), &publish_key, &rows)
        })
        .await
        .map_err(|e| ReceiptArchiveError(format!("receipt publisher task: {e}")))??;
        self.index
            .put(&RarchiveMetadata {
                object_key: object_key.clone(),
                first_receipt_key: first,
                last_receipt_key: last,
                rows: row_count,
                checksum,
            })
            .await?;

        self.cursor = Some(last);
        self.archived_rows
            .fetch_add(u64::from(row_count), Ordering::Relaxed);
        self.archived_pages.fetch_add(1, Ordering::Relaxed);
        self.pass_rows += u64::from(row_count);
        let completed_pass = last == upper;
        if completed_pass {
            let elapsed = self.pass_started.take().expect("pass start").elapsed();
            let elapsed_ms = elapsed.as_millis().max(1) as u64;
            self.last_full_pass_ms.store(elapsed_ms, Ordering::Relaxed);
            self.last_rows_per_second.store(
                self.pass_rows.saturating_mul(1_000) / elapsed_ms,
                Ordering::Relaxed,
            );
            self.full_passes.fetch_add(1, Ordering::Relaxed);
            self.pass_upper = None;
            tracing::info!(
                scanners = RECEIPT_ARCHIVE_SCANNERS,
                page_rows = self.config.page_rows,
                rows = self.pass_rows,
                full_pass_ms = elapsed_ms,
                rows_per_second = self.last_rows_per_second.load(Ordering::Relaxed),
                upper = %key_hex(&upper),
                "receipt archive completed a fixed-upper pass"
            );
        }
        Ok(ReceiptTailerPass::Published {
            rows: u64::from(row_count),
            bytes: bytes_len,
            completed_pass,
        })
    }
}

fn publish_receipt_page(
    store: &dyn ArchiveStore,
    object_key: &str,
    rows: &[ReceiptArchiveRow],
) -> Result<(u64, [u8; 32]), ReceiptArchiveError> {
    let bytes = encode_receipt_object(rows)
        .map_err(|e| ReceiptArchiveError(format!("encode receipt object: {e}")))?;
    let expected = blake3::hash(&bytes);
    store
        .put(object_key, &bytes)
        .map_err(|e| ReceiptArchiveError(format!("upload receipt object: {e}")))?;
    let stored = store
        .get(object_key)
        .map_err(|e| ReceiptArchiveError(format!("verify receipt object: {e}")))?
        .ok_or_else(|| {
            ReceiptArchiveError("receipt object absent after successful put".to_owned())
        })?;
    if blake3::hash(&stored) != expected {
        return Err(ReceiptArchiveError(
            "receipt object checksum differs on read-back".to_owned(),
        ));
    }
    Ok((bytes.len() as u64, *expected.as_bytes()))
}

/// Deterministic object key for one page, named by its final commit versionstamp.
#[must_use]
pub fn receipt_object_key(prefix: &str, last: &[u8; 12]) -> String {
    let prefix = prefix.trim_end_matches('/');
    let leaf = format!("{}.parquet", key_hex(&last[2..]));
    if prefix.is_empty() {
        format!("rarchive/{leaf}")
    } else {
        format!("{prefix}/rarchive/{leaf}")
    }
}

fn key_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Running receipt scanner handle.
pub struct ReceiptArchiveTailerHandle {
    shutdown: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

impl ReceiptArchiveTailerHandle {
    /// Stop the scanner and await its task.
    pub async fn shutdown(self) {
        self.shutdown.notify_waiters();
        let _ = self.join.await;
    }
}

/// Spawn the one sequential scanner with bounded retry backoff.
#[must_use]
pub fn spawn_receipt_archive_tailer(
    mut tailer: ReceiptArchiveTailer,
) -> ReceiptArchiveTailerHandle {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_task = Arc::clone(&shutdown);
    let join = tokio::spawn(async move {
        let mut backoff = tailer.config.backoff_initial;
        loop {
            let wait = match tailer.pass().await {
                Ok(ReceiptTailerPass::Idle) => {
                    backoff = tailer.config.backoff_initial;
                    tailer.config.idle_interval
                }
                Ok(ReceiptTailerPass::Published { .. }) => {
                    backoff = tailer.config.backoff_initial;
                    Duration::ZERO
                }
                Err(error) => {
                    tracing::warn!(error = %error, "receipt archive scanner stalled");
                    let wait = backoff;
                    backoff = (backoff * 2).min(tailer.config.backoff_max);
                    wait
                }
            };
            tokio::select! {
                () = shutdown_task.notified() => break,
                () = tokio::time::sleep(wait) => {}
            }
        }
    });
    ReceiptArchiveTailerHandle { shutdown, join }
}

/// FoundationDB receipt source and metadata index.
#[cfg(feature = "fdb")]
pub struct FdbReceiptArchive {
    db: Arc<foundationdb::Database>,
}

#[cfg(feature = "fdb")]
impl FdbReceiptArchive {
    /// Wrap the process-scoped database handle.
    #[must_use]
    pub fn new(db: Arc<foundationdb::Database>) -> Self {
        Self { db }
    }

    async fn read_range(
        &self,
        begin: foundationdb::KeySelector<'_>,
        end: foundationdb::KeySelector<'_>,
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ReceiptArchiveError> {
        use futures::TryStreamExt as _;
        let trx = self
            .db
            .create_trx()
            .map_err(|e| ReceiptArchiveError(format!("create receipt read transaction: {e}")))?;
        let range = foundationdb::RangeOption {
            begin,
            end,
            limit: Some(limit),
            reverse,
            ..foundationdb::RangeOption::default()
        };
        let mut stream = trx.get_ranges_keyvalues(range, true);
        let mut rows = Vec::new();
        while let Some(kv) = stream
            .try_next()
            .await
            .map_err(|e| ReceiptArchiveError(format!("receipt range read: {e}")))?
        {
            rows.push((kv.key().to_vec(), kv.value().to_vec()));
        }
        drop(stream);
        drop(trx);
        Ok(rows)
    }
}

#[cfg(feature = "fdb")]
#[async_trait::async_trait]
impl ReceiptSource for FdbReceiptArchive {
    async fn capture_upper(&self) -> Result<Option<[u8; 12]>, ReceiptArchiveError> {
        let start = keyspace::ledger_receipt_range_start();
        let end = keyspace::ledger_receipt_range_end();
        let rows = self
            .read_range(
                foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                1,
                true,
            )
            .await?;
        rows.first()
            .map(|(key, _)| {
                key.as_slice().try_into().map_err(|_| {
                    ReceiptArchiveError("receipt upper key is not 12 bytes".to_owned())
                })
            })
            .transpose()
    }

    async fn read_page(
        &self,
        after: Option<[u8; 12]>,
        upper: [u8; 12],
        limit: usize,
    ) -> Result<Vec<ReceiptArchiveRow>, ReceiptArchiveError> {
        let start = keyspace::ledger_receipt_range_start();
        let rows = self
            .read_range(
                after.as_ref().map_or_else(
                    || foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                    |cursor| foundationdb::KeySelector::first_greater_than(cursor.as_slice()),
                ),
                foundationdb::KeySelector::first_greater_than(upper.as_slice()),
                limit,
                false,
            )
            .await?;
        rows.into_iter()
            .map(|(key, value)| {
                let key: [u8; 12] = key.try_into().map_err(|_| {
                    ReceiptArchiveError("receipt page key is not 12 bytes".to_owned())
                })?;
                let (receipt, encoding) = keyspace::decode_receipt_row(&value)
                    .map_err(|e| ReceiptArchiveError(format!("decode receipt row: {e}")))?;
                Ok(ReceiptArchiveRow {
                    key,
                    receipt,
                    encoding,
                })
            })
            .collect()
    }
}

#[cfg(feature = "fdb")]
#[async_trait::async_trait]
impl ReceiptArchiveIndex for FdbReceiptArchive {
    async fn put(&self, metadata: &RarchiveMetadata) -> Result<(), CheckpointError> {
        let key = keyspace::rarchive_key(&metadata.last_receipt_key).ok_or_else(|| {
            CheckpointError::Store("rarchive metadata has a non-receipt cursor".to_owned())
        })?;
        let value = keyspace::encode_rarchive_metadata(metadata)?;
        self.db
            .run(|trx, _| {
                let value = value.clone();
                async move {
                    trx.set(&key, &value);
                    Ok(())
                }
            })
            .await
            .map_err(|e| CheckpointError::Store(format!("rarchive row commit: {e}")))
    }

    async fn last(&self) -> Result<Option<RarchiveMetadata>, CheckpointError> {
        let start = keyspace::rarchive_range_start();
        let end = keyspace::rarchive_range_end();
        let rows = self
            .read_range(
                foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                1,
                true,
            )
            .await
            .map_err(|e| CheckpointError::Store(e.to_string()))?;
        rows.first()
            .map(|(key, value)| {
                let cursor = keyspace::decode_rarchive_key(key)
                    .ok_or_else(|| CheckpointError::Store("undecodable rarchive key".to_owned()))?;
                let metadata = keyspace::decode_rarchive_metadata(value)?;
                if metadata.last_receipt_key != cursor {
                    return Err(CheckpointError::Store(
                        "rarchive key disagrees with metadata last_receipt_key".to_owned(),
                    ));
                }
                Ok(metadata)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{AccountId, AssetId};

    fn receipt_key(n: u64) -> [u8; 12] {
        let mut key = keyspace::ledger_receipt_key();
        key[4..12].copy_from_slice(&n.to_be_bytes());
        key
    }

    fn receipt(n: u64) -> keyspace::ReceiptRow {
        keyspace::ReceiptRow {
            intent_id: u128::from(n),
            parties: vec![AccountId::new(n)],
            ops: vec![0],
            balance_deltas: vec![keyspace::ReceiptBalanceDelta {
                account: AccountId::new(n),
                asset: AssetId::new(1),
                delta: n as i64,
            }],
            ownership: Vec::new(),
        }
    }

    #[tokio::test]
    async fn one_scanner_pages_to_a_fixed_upper_then_captures_new_writes() {
        let source = Arc::new(MemReceiptSource::new());
        for n in 1..=5 {
            source.insert(receipt_key(n), &receipt(n));
        }
        let store_dir = tempfile::tempdir().expect("store dir");
        let store =
            Arc::new(crate::archive::FsArchiveStore::open(store_dir.path()).expect("store"));
        let index = Arc::new(MemReceiptArchiveIndex::new());
        let mut tailer = ReceiptArchiveTailer::open(
            source.clone(),
            store,
            index.clone(),
            String::new(),
            ReceiptArchiveConfig {
                page_rows: 2,
                ..ReceiptArchiveConfig::default()
            },
        )
        .await
        .expect("open");

        assert!(matches!(
            tailer.pass().await.expect("page 1"),
            ReceiptTailerPass::Published {
                completed_pass: false,
                ..
            }
        ));
        assert_eq!(tailer.status().pass_upper, Some(receipt_key(5)));
        source.insert(receipt_key(6), &receipt(6));
        assert!(matches!(
            tailer.pass().await.expect("page 2"),
            ReceiptTailerPass::Published {
                completed_pass: false,
                ..
            }
        ));
        assert!(matches!(
            tailer.pass().await.expect("page 3"),
            ReceiptTailerPass::Published {
                completed_pass: true,
                rows: 1,
                ..
            }
        ));
        assert_eq!(tailer.status().cursor, Some(receipt_key(5)));
        assert_eq!(tailer.status().full_passes, 1);

        assert!(matches!(
            tailer.pass().await.expect("next bounded pass"),
            ReceiptTailerPass::Published {
                completed_pass: true,
                rows: 1,
                ..
            }
        ));
        assert_eq!(tailer.status().cursor, Some(receipt_key(6)));
        assert_eq!(index.len(), 4, "one durable marker per bounded page");
        assert_eq!(tailer.status().scanners, 1, "parallel scans are not a knob");
        assert_eq!(tailer.status().page_rows, 2, "page size is observable");
    }

    #[tokio::test]
    async fn restart_recovers_the_last_verified_page_cursor() {
        let source = Arc::new(MemReceiptSource::new());
        for n in 1..=3 {
            source.insert(receipt_key(n), &receipt(n));
        }
        let store_dir = tempfile::tempdir().expect("store dir");
        let store =
            Arc::new(crate::archive::FsArchiveStore::open(store_dir.path()).expect("store"));
        let index = Arc::new(MemReceiptArchiveIndex::new());
        let config = ReceiptArchiveConfig {
            page_rows: 2,
            ..ReceiptArchiveConfig::default()
        };
        let mut first = ReceiptArchiveTailer::open(
            source.clone(),
            store.clone(),
            index.clone(),
            String::new(),
            config.clone(),
        )
        .await
        .expect("open first");
        first.pass().await.expect("first page");
        assert_eq!(first.status().cursor, Some(receipt_key(2)));

        let restarted = ReceiptArchiveTailer::open(source, store, index, String::new(), config)
            .await
            .expect("restart");
        assert_eq!(restarted.status().cursor, Some(receipt_key(2)));
    }
}
