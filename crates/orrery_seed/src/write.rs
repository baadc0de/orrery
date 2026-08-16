//! Batch writing and commit telemetry for the seeder.
//!
//! The writer stays deliberately simple at the row level:
//! `(key, value)` pairs are packed into 768 KiB transactions, shuffled by a
//! deterministic permutation, and committed with blind `set` operations so no
//! transaction needs to read the rows it writes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "fdb")]
use std::time::Instant;

use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

#[cfg(feature = "fdb")]
use crate::seedtree::SeedRoot;

/// The transaction budget, in bytes.
pub const TXN_BUDGET_BYTES: usize = 768 * 1024;

/// One encoded row to write.
#[derive(Debug, Clone)]
pub struct EncodedRow {
    /// The full key bytes.
    pub key: Vec<u8>,
    /// The full value bytes.
    pub value: Vec<u8>,
}

impl EncodedRow {
    /// Approximate landed size for batching: key + value + framing.
    #[must_use]
    pub fn budget_bytes(&self) -> usize {
        self.key.len() + self.value.len() + 40
    }
}

/// One packed batch.
pub type Batch = Vec<EncodedRow>;

/// Commit latency histogram.
#[derive(Debug, Default, Clone)]
pub struct CommitHistogram {
    buckets: BTreeMap<u64, u64>,
}

impl CommitHistogram {
    /// Record one commit duration.
    pub fn record(&mut self, duration: Duration) {
        let ms = duration.as_millis() as u64;
        *self.buckets.entry(ms).or_insert(0) += 1;
    }

    /// Commit p99 in milliseconds.
    #[must_use]
    pub fn p99_ms(&self) -> f64 {
        let total: u64 = self.buckets.values().sum();
        if total == 0 {
            return 0.0;
        }
        let threshold = ((total as f64) * 0.99).ceil() as u64;
        let mut seen = 0u64;
        for (bucket, count) in &self.buckets {
            seen += *count;
            if seen >= threshold {
                return *bucket as f64;
            }
        }
        self.buckets.keys().next_back().copied().unwrap_or(0) as f64
    }
}

/// Writer telemetry.
#[derive(Debug, Clone)]
pub struct WriteStats {
    /// Committed rows.
    pub written_rows: u64,
    /// Batches committed.
    pub batches: u64,
    /// Commit latency histogram.
    pub histogram: CommitHistogram,
    /// Wall-clock elapsed.
    pub elapsed: Duration,
}

impl WriteStats {
    /// Commit p99 as measured from the histogram.
    #[must_use]
    pub fn commit_p99_ms(&self) -> f64 {
        self.histogram.p99_ms()
    }
}

/// Pack rows into contiguous transactions under the landed budget.
#[must_use]
pub fn pack_batches(rows: Vec<EncodedRow>) -> Vec<Batch> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0usize;
    for row in rows {
        let row_bytes = row.budget_bytes();
        if !current.is_empty() && current_bytes + row_bytes > TXN_BUDGET_BYTES {
            out.push(current);
            current = Vec::new();
            current_bytes = 0;
        }
        current_bytes += row_bytes;
        current.push(row);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Shuffle batch order with a deterministic root-key-derived permutation.
pub fn shuffle_batches(root: &[u8; 32], batches: &mut [Batch]) {
    let mut rng = ChaCha8Rng::from_seed(*root);
    batches.shuffle(&mut rng);
}

#[cfg(feature = "fdb")]
mod fdb {
    use super::*;
    use foundationdb::{Database, FdbBindingError};
    use tokio::task::JoinSet;

    /// Commit all batches with a small AIMD controller.
    pub async fn commit_batches(
        db: Arc<Database>,
        mut batches: Vec<Batch>,
    ) -> Result<WriteStats, String> {
        shuffle_batches(
            &SeedRoot::derive("orrery.seeder.v1", b"seed.write.shuffle").layer_key("write"),
            &mut batches,
        );
        let started = Instant::now();
        let mut histogram = CommitHistogram::default();
        let mut batches_done = 0u64;
        let mut rows_done = 0u64;
        let mut in_flight = JoinSet::new();
        let mut next = 0usize;
        let mut window = 8usize;

        while next < batches.len() || !in_flight.is_empty() {
            while next < batches.len() && in_flight.len() < window {
                let batch = batches[next].clone();
                let db = Arc::clone(&db);
                next += 1;
                in_flight.spawn(async move {
                    let started = Instant::now();
                    let rows = batch.len() as u64;
                    db.run(|trx, _| {
                        let batch = batch.clone();
                        async move {
                            for row in &batch {
                                trx.set(&row.key, &row.value);
                            }
                            Ok(())
                        }
                    })
                    .await
                    .map_err(|e: FdbBindingError| format!("commit batch: {e}"))?;
                    Ok::<(Duration, u64), String>((started.elapsed(), rows))
                });
            }

            if let Some(joined) = in_flight.join_next().await {
                let (elapsed, rows) = joined.map_err(|e| format!("join batch: {e}"))??;
                histogram.record(elapsed);
                rows_done += rows;
                batches_done += 1;
                if histogram.p99_ms() < 20.0 {
                    window = window.saturating_add(1);
                } else if histogram.p99_ms() > 20.0 {
                    window = (window / 2).max(1);
                }
            }
        }

        Ok(WriteStats {
            written_rows: rows_done,
            batches: batches_done,
            histogram,
            elapsed: started.elapsed(),
        })
    }
}

#[cfg(feature = "fdb")]
pub use fdb::commit_batches;

#[cfg(not(feature = "fdb"))]
/// Stub when the `fdb` feature is off.
pub async fn commit_batches(_db: Arc<()>, _batches: Vec<Batch>) -> Result<WriteStats, String> {
    Err("write path requires the `fdb` feature".to_string())
}
