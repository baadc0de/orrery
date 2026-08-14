//! Chain replication of the journal to one async follower (docs/08-persistence.md
//! §4, D11).
//!
//! Each node streams its journal to exactly one async follower — the next node
//! in HRW order over node ids (ops-overridable, placed in a different AZ). The
//! follower persists segments verbatim. Replication is **async**: it is not in
//! the ack path, so it never adds to client-observed latency. Lag is monitored
//! and alarmed above [`ChainConfig::lag_alarm`] (default 100 ms, D11).
//!
//! The transport is intentionally pluggable: a [`ChainTransport`] carries
//! committed records to the follower and reports the follower's durable
//! watermark. The default [`MemChainTransport`] is an in-process shim that
//! writes into a follower's [`ChainSink`] directly, so the replication logic is
//! testable without a network. A real deployment supplies a transport that
//! pushes over the node-to-node link (tonic/gRPC per D12) into the follower's
//! sink.
//!
//! The follower's [`ChainSink`] is the durable landing point: it appends each
//! received record to the follower's own journal (so the follower can serve as
//! the recovery source if the primary's disk is lost) and tracks the highest
//! *origin* LSN it has durably persisted — the follower watermark that bounds
//! RPO.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use orrery_protocol::JournalRecord;
use orrery_protocol::Lsn;

use crate::journal::{Journal, JournalError};

/// A durably accepted follower history that a promoted node may export to its
/// new follower. Mirrored rows remain non-outbound unless explicitly adopted.
#[cfg(feature = "chain-grpc")]
#[derive(Clone, Debug)]
pub struct AdoptedChainHistory {
    source: crate::journal::chain_grpc::DurableChainId,
    watermark: Option<Lsn>,
}

#[cfg(feature = "chain-grpc")]
impl AdoptedChainHistory {
    pub(crate) fn new(
        source: crate::journal::chain_grpc::DurableChainId,
        watermark: Option<Lsn>,
    ) -> Self {
        Self { source, watermark }
    }

    /// The source chain whose record identities this history preserves.
    #[must_use]
    pub fn source(&self) -> &crate::journal::chain_grpc::DurableChainId {
        &self.source
    }

    /// Highest accepted source LSN, if the mirrored chain was non-empty.
    #[must_use]
    pub fn watermark(&self) -> Option<Lsn> {
        self.watermark
    }
}

pub(crate) enum ChainSource {
    Originated,
    #[cfg(feature = "chain-grpc")]
    Adopted(AdoptedChainHistory),
}

/// Configuration for chain replication (D11 §4).
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// The node id of this node's follower (the next node in HRW order).
    pub follower: u64,
    /// The lag (primary committed LSN vs follower durable LSN) above which an
    /// alarm fires. Default 100 ms of journal time; the exact mapping from LSN
    /// to wall time is the transport's report.
    pub lag_alarm: Duration,
    /// The maximum number of records to batch per replication flush.
    pub batch_max: usize,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            follower: 0,
            lag_alarm: Duration::from_millis(100),
            batch_max: 1024,
        }
    }
}

/// The durable landing point on the follower for replicated records.
///
/// A [`ChainSink`] appends records to the follower's journal and reports the
/// highest *primary* (origin) LSN durably persisted. The sink is the *only*
/// thing the transport writes to on the follower side.
#[async_trait::async_trait]
pub trait ChainSink: Send + Sync {
    /// Durably persist `record` and return the follower's new watermark: the
    /// highest *origin* LSN now durable on the follower. The record carries
    /// its origin LSN (stamped by the origin journal before encoding), so the
    /// follower echoes the record's own LSN — not the locally assigned one,
    /// which lives in the follower's independent LSN space and would be
    /// meaningless as lag to the origin.
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError>;

    /// The follower's durable watermark (highest origin LSN persisted), if any.
    async fn watermark(&self) -> Option<Lsn>;
}

/// A transport that carries committed records from a primary to its follower.
///
/// The transport is responsible for delivery and for reporting the follower's
/// durable watermark (so the primary can measure lag). It is **not** in the ack
/// path.
#[async_trait::async_trait]
pub trait ChainTransport: Send + Sync {
    /// Push `record` to the follower and return the follower's durable
    /// watermark after this record (in the primary's LSN space).
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError>;

    /// Push one contiguous ordered batch and return the durable watermark after
    /// its final record.  Transports which have no wire-level batch primitive
    /// retain the old per-record behaviour; the gRPC transport overrides this
    /// so `ChainConfig::batch_max` is also an RPC batching bound.
    async fn append_batch(&self, records: Vec<JournalRecord>) -> Result<Lsn, JournalError> {
        let mut last = None;
        for record in records {
            last = Some(self.append(record).await?);
        }
        last.ok_or_else(|| JournalError::Store("cannot send empty chain batch".into()))
    }

    /// The follower's durable watermark (highest primary LSN persisted on the
    /// follower), or `None` if unknown.
    async fn follower_watermark(&self) -> Option<Lsn>;
}

/// The replicator's shutdown signal: an `AtomicBool` flag plus a `Notify`
/// early-wakeup. The flag is the source of truth, checked at every await
/// boundary; a bare `Notify` permit can be consumed by the wrong `select!`
/// (e.g. a stalled transport push races shutdown) and leave the outer loop
/// waiting forever.
#[derive(Debug, Default)]
struct ShutdownSignal {
    flag: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

impl ShutdownSignal {
    fn arm(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_waiters();
    }

    fn armed(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// A running chain-replication task: subscribes to the primary journal's
/// committed records and pushes them to the follower.
pub struct ChainReplicator {
    /// The follower's durable watermark, updated as the transport reports it.
    watermark: Arc<std::sync::Mutex<Option<Lsn>>>,
    /// Replication lag: `primary.committed()` minus the follower's reported
    /// durable watermark. LSN offsets are byte positions, so the raw gap is in
    /// bytes; [`ChainReplicator::lag_bytes`] exposes it and the alarm compares
    /// it against [`ChainConfig::lag_alarm`]'s byte budget (D11's "~100 ms of
    /// journal" mapped to bytes — see [`lag_alarm_bytes`]).
    lag_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Join handle; awaited on shutdown.
    join: tokio::task::JoinHandle<()>,
    /// Signals the task to stop.
    shutdown: Arc<ShutdownSignal>,
    /// The follower node id (for diagnostics).
    follower: u64,
}

impl ChainReplicator {
    /// The follower node id this replicator streams to.
    #[must_use]
    pub fn follower(&self) -> u64 {
        self.follower
    }

    /// The highest LSN durably persisted on the follower, if known.
    #[must_use]
    pub fn follower_watermark(&self) -> Option<Lsn> {
        *self.watermark.lock().expect("chain watermark lock")
    }

    /// The current replication lag in journal bytes: `primary.committed()`
    /// minus the highest primary LSN the follower has reported durable. Both
    /// are in the primary's LSN space (the follower echoes each record's own
    /// LSN), so the difference is a byte gap; the lag alarm fires (a
    /// `tracing::warn!`) while this exceeds the [`ChainConfig::lag_alarm`]
    /// byte budget.
    #[must_use]
    pub fn lag_bytes(&self) -> u64 {
        self.lag_bytes.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Stop the replicator task, awaiting its exit.
    pub async fn shutdown(self) {
        self.shutdown.arm();
        let _ = self.join.await;
    }
}

/// Compute the lag between the primary's committed LSN and the follower's
/// reported durable watermark, in journal bytes. `committed` is stamped by
/// this journal and `watermark` was echoed by the follower from the records'
/// own LSNs, so both live in the primary's LSN space. Returns `None` when the
/// watermark is in a different segment — a cross-segment gap is ill-defined
/// as a byte distance, and segment transitions are rare enough that the alarm
/// simply skips that sample.
fn lag_in_bytes(committed: Lsn, watermark: Option<Lsn>) -> Option<u64> {
    let w = watermark?;
    if w.segment != committed.segment {
        return None;
    }
    Some(committed.offset.saturating_sub(w.offset))
}

/// Convert the [`ChainConfig::lag_alarm`] duration into the journal byte
/// budget the lag gauge is compared against. The lag alarm is D11's "~100 ms
/// of journal"; LSNs carry no wall-clock time, so the replicator maps the
/// duration through the journal's sustained byte rate. At the D16 envelope a
/// node's P2 envelope is roughly 10k 128-byte records/s.  Keep the floor at
/// 1 MiB/s until the tonic transport (D12) reports the follower's wall-clock
/// watermark age.  The former `1 B/s` placeholder truncated the default
/// 100-ms window to zero bytes, causing one warning per replicated record and
/// putting log formatting on the gateway's latency-critical runtime.
const LAG_BYTES_PER_ALARM_SECOND: f64 = 1_048_576.0;

/// The byte budget for [`ChainConfig::lag_alarm`]: the alarm fires when the
/// follower's durable watermark trails the primary's committed LSN by more
/// journal bytes than the primary produces in `lag_alarm` wall time.
fn lag_alarm_bytes(lag_alarm: Duration) -> u64 {
    (lag_alarm.as_secs_f64() * LAG_BYTES_PER_ALARM_SECOND) as u64
}

#[cfg(test)]
mod lag_alarm_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use orrery_protocol::{CellId, Epoch, GridId, PersistId, RecordKind, Tick};

    use crate::journal::{AdaptiveCommitMode, GroupCommitConfig, JournalConfig};

    #[test]
    fn default_lag_window_never_collapses_to_a_per_record_alarm() {
        // The default is deliberately large enough to represent roughly
        // 100 ms of the P2 write envelope, rather than truncating to zero and
        // formatting a warning for every acknowledgement.
        assert_eq!(lag_alarm_bytes(Duration::from_millis(100)), 104_857);
    }

    struct BatchSpy {
        batches: Mutex<Vec<Vec<Lsn>>>,
    }

    #[async_trait::async_trait]
    impl ChainTransport for BatchSpy {
        async fn append(&self, _record: JournalRecord) -> Result<Lsn, JournalError> {
            unreachable!("the replicator must use append_batch")
        }

        async fn append_batch(&self, records: Vec<JournalRecord>) -> Result<Lsn, JournalError> {
            let last = records.last().expect("non-empty batch").lsn;
            self.batches
                .lock()
                .expect("batch spy lock")
                .push(records.into_iter().map(|record| record.lsn).collect());
            Ok(last)
        }

        async fn follower_watermark(&self) -> Option<Lsn> {
            None
        }
    }

    fn test_record(entity: u64) -> JournalRecord {
        let payload = entity.to_le_bytes();
        let author = iroh_base::SecretKey::from_bytes(&[7; 32]).public();
        JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(entity),
            tick: Tick::new(entity),
            epoch: Epoch::new(0),
            author,
            kind: RecordKind::Spawn,
            payload: bytes::Bytes::copy_from_slice(&payload),
            crc: crate::payload_crc(&payload),
        }
    }

    #[tokio::test]
    async fn replicator_sends_collected_records_as_one_transport_batch() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            Journal::open(&JournalConfig {
                dir: dir.path().to_path_buf(),
                commit: GroupCommitConfig {
                    mode: AdaptiveCommitMode::AlwaysBatch,
                    batch_window: Duration::from_millis(1),
                    batch_max_records: 128,
                    batch_max_bytes: 1 << 20,
                },
            })
            .unwrap(),
        );
        for entity in 1..=3 {
            journal.append(test_record(entity)).unwrap().committed().await.unwrap();
        }
        let transport = Arc::new(BatchSpy { batches: Mutex::new(Vec::new()) });
        let replicator = spawn_chain(
            Arc::clone(&journal),
            Arc::clone(&transport) as Arc<dyn ChainTransport>,
            &ChainConfig { follower: 9, batch_max: 3, ..ChainConfig::default() },
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if transport.batches.lock().expect("batch spy lock").len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replicator should flush the initial scan");
        let batches = transport.batches.lock().expect("batch spy lock").clone();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 3);
        assert!(batches[0].windows(2).all(|pair| pair[0] < pair[1]));
        replicator.shutdown().await;
        journal.close().await.unwrap();
    }
}

/// Spawn a chain-replication task streaming `journal`'s committed records to
/// `transport` (which writes into the follower's [`ChainSink`]).
///
/// The journal scan is the correctness path and the committed-record broadcast
/// is only its wake-up signal. The task probes the follower's durable watermark
/// on startup and after every transport error, then scans and replays everything
/// after that cursor. This also catches records committed before the task was
/// spawned and records committed while the follower was unavailable.
pub fn spawn_chain(
    journal: Arc<Journal>,
    transport: Arc<dyn ChainTransport>,
    config: &ChainConfig,
) -> ChainReplicator {
    spawn_chain_from(journal, transport, config, ChainSource::Originated)
}

/// Spawn an outbound chain from a promotion-adopted source prefix.
///
/// The records retain their original source LSNs and epochs. This path is
/// intentionally separate from ordinary mirroring, preventing a follower from
/// becoming an accidental relay without a fenced promotion decision.
#[cfg(feature = "chain-grpc")]
pub fn spawn_adopted_chain(
    journal: Arc<Journal>,
    history: AdoptedChainHistory,
    transport: Arc<dyn ChainTransport>,
    config: &ChainConfig,
) -> ChainReplicator {
    spawn_chain_from(journal, transport, config, ChainSource::Adopted(history))
}

fn spawn_chain_from(
    journal: Arc<Journal>,
    transport: Arc<dyn ChainTransport>,
    config: &ChainConfig,
    source: ChainSource,
) -> ChainReplicator {
    let watermark = Arc::new(std::sync::Mutex::new(None));
    let lag_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let shutdown = Arc::new(ShutdownSignal::default());
    let follower = config.follower;
    let batch_max = config.batch_max.max(1);
    let alarm_bytes = lag_alarm_bytes(config.lag_alarm);
    let mut rx = journal.subscribe();
    let wm = Arc::clone(&watermark);
    let lag = Arc::clone(&lag_bytes);
    let sd = Arc::clone(&shutdown);

    let join = tokio::spawn(async move {
        let mut cursor: Option<Lsn> = None;
        let mut needs_probe = true;
        loop {
            // The flag is checked at every await boundary: `push_batch`'s
            // per-append select races shutdown too, so a permit-style wakeup
            // could be consumed there and missed here.
            if sd.armed() {
                break;
            }
            if needs_probe {
                // A probe is deliberately cancellable: an unreachable network
                // follower must not make shutdown wait for connector timeout.
                let remote = tokio::select! {
                    value = transport.follower_watermark() => value,
                    _ = sd.wake.notified() => break,
                };
                if let Some(remote) = remote {
                    cursor = Some(remote);
                    update_progress(&journal, &wm, &lag, alarm_bytes, follower, remote);
                }
                needs_probe = false;
            }

            // `scan_from` is inclusive, while the durable watermark is the
            // record already held by the follower. Filtering it out is what
            // makes normal reconnects duplicate-free. Limit each collection so
            // a hot journal cannot make this non-Send iterator monopolize the
            // replication task indefinitely.
            let from = cursor.unwrap_or(Lsn::new(0, 0));
            let committed = journal.committed_watermark();
            let batch = committed
                .map(|committed| {
                    journal
                        .scan_source_from(&source, from)
                        .expect("adopted chain history was validated before replication")
                        .filter(|item| match item {
                            Ok(stored) => {
                                stored.lsn <= committed
                                    && cursor.is_none_or(|last| stored.record.lsn > last)
                            }
                            Err(_) => true,
                        })
                        .take(batch_max)
                        .map(|item| item.map(|stored| stored.record))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap_or_else(|| Ok(Vec::new()));
            let batch = match batch {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::error!(follower, ?error, "chain: primary journal scan failed");
                    break;
                }
            };

            if !batch.is_empty() {
                let pushed = push_batch(
                    &journal,
                    &*transport,
                    &batch,
                    &sd,
                    &wm,
                    &lag,
                    alarm_bytes,
                    follower,
                )
                .await;
                if let Some(last) = pushed.last() {
                    cursor = Some(last);
                }
                if !pushed.complete() {
                    // The failed append is intentionally not retained in
                    // volatile state. Re-probe the follower's durable cursor,
                    // then reconstruct the complete tail from the primary.
                    needs_probe = true;
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                        _ = sd.wake.notified() => break,
                    }
                }
                continue;
            }

            // We reached the journal tail represented by `cursor`. Broadcast
            // contents are never delivered directly; any message (or lag
            // notification) merely tells us to scan again from the durable
            // cursor, preserving primary order even across channel overflow.
            let recv = tokio::select! {
                result = rx.recv() => result,
                _ = sd.wake.notified() => break,
            };
            match recv {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    ChainReplicator {
        watermark,
        lag_bytes,
        join,
        shutdown,
        follower,
    }
}

/// The outcome of [`push_batch`]: the LSNs actually delivered, and whether
/// the whole batch went through.
struct PushOutcome {
    /// The last record durably pushed (if any), used to advance the rescan
    /// cursor.
    last: Option<Lsn>,
    /// False when a transport error or shutdown interrupted the batch.
    complete: bool,
}

impl PushOutcome {
    fn last(&self) -> Option<Lsn> {
        self.last
    }
    fn complete(&self) -> bool {
        self.complete
    }
}

fn update_progress(
    journal: &Journal,
    wm: &std::sync::Mutex<Option<Lsn>>,
    lag: &std::sync::atomic::AtomicU64,
    alarm_bytes: u64,
    follower: u64,
    durable: Lsn,
) {
    *wm.lock().expect("chain watermark lock") = Some(durable);
    if let Some(bytes) = lag_in_bytes(journal.committed(), Some(durable)) {
        lag.store(bytes, std::sync::atomic::Ordering::Release);
        if bytes > alarm_bytes {
            tracing::warn!(
                follower,
                lag_bytes = bytes,
                alarm_bytes,
                "chain: replication lag exceeds lag_alarm"
            );
        }
    }
}

/// Push one batch of records to the follower. `shutdown` races the batch RPC so
/// a stalled transport cannot wedge [`ChainReplicator::shutdown`] forever. The
/// result is the follower's durable watermark after the entire batch, avoiding
/// both a second probe and a per-record RPC on transports that support batching.
#[allow(clippy::too_many_arguments)]
async fn push_batch(
    journal: &Journal,
    transport: &dyn ChainTransport,
    records: &[JournalRecord],
    shutdown: &ShutdownSignal,
    wm: &std::sync::Mutex<Option<Lsn>>,
    lag: &std::sync::atomic::AtomicU64,
    alarm_bytes: u64,
    follower: u64,
) -> PushOutcome {
    if shutdown.armed() {
        return PushOutcome { last: None, complete: false };
    }
    let pushed = tokio::select! {
        r = transport.append_batch(records.to_vec()) => r,
        _ = shutdown.wake.notified() => {
            return PushOutcome { last: None, complete: false };
        }
    };
    match pushed {
        Ok(durable) if durable >= records.last().expect("non-empty batch").lsn => {
            update_progress(journal, wm, lag, alarm_bytes, follower, durable);
            PushOutcome { last: Some(durable), complete: true }
        }
        Ok(durable) => {
            tracing::warn!(
                follower,
                record_lsn = %records.last().expect("non-empty batch").lsn,
                durable_lsn = %durable,
                "chain: follower returned a regressed batch watermark"
            );
            PushOutcome { last: None, complete: false }
        }
        Err(error) => {
            tracing::warn!(follower, ?error, "chain: follower batch append failed");
            PushOutcome { last: None, complete: false }
        }
    }
}

/// An in-process chain transport: pushes committed records straight into a
/// follower's [`ChainSink`].
///
/// Used for tests and single-process cluster harnesses where the "follower" is
/// another journal in the same process. It reports the sink's watermark after
/// each append.
pub struct MemChainTransport {
    sink: Arc<dyn ChainSink>,
}

impl MemChainTransport {
    /// A transport writing into `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn ChainSink>) -> Self {
        Self { sink }
    }
}

#[async_trait::async_trait]
impl ChainTransport for MemChainTransport {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        self.sink.append(record).await
    }

    async fn follower_watermark(&self) -> Option<Lsn> {
        self.sink.watermark().await
    }
}

/// A [`ChainSink`] that appends replicated records to a [`Journal`].
///
/// This is the standard follower-side sink: it journals each replicated record
/// durably (so the follower can serve as the recovery source) and tracks the
/// highest *origin* LSN persisted. The journal write goes through
/// [`Journal::append_replicated`], which keeps the record out of the
/// follower's own replication broadcast — otherwise a ring of nodes echoes
/// every record around forever.
pub struct JournalChainSink {
    journal: Arc<Journal>,
    watermark: std::sync::Mutex<Option<Lsn>>,
}

impl JournalChainSink {
    /// A sink appending into `journal`.
    #[must_use]
    pub fn new(journal: Arc<Journal>) -> Self {
        Self {
            journal,
            watermark: std::sync::Mutex::new(None),
        }
    }

    /// The highest origin LSN durably persisted on the follower journal.
    #[must_use]
    pub fn watermark(&self) -> Option<Lsn> {
        *self.watermark.lock().expect("sink watermark lock")
    }
}

#[async_trait::async_trait]
impl ChainSink for JournalChainSink {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        // The record's own (origin) LSN becomes the watermark — this is what
        // makes the primary's lag computation meaningful. `append_replicated`
        // stamps the *local* LSN into the record's key but the record's
        // origin LSN is what the sink reports.
        let origin_lsn = record.lsn;
        let handle = self.journal.append_replicated(record)?;
        handle.committed().await?;
        *self.watermark.lock().expect("sink watermark lock") = Some(origin_lsn);
        Ok(origin_lsn)
    }

    async fn watermark(&self) -> Option<Lsn> {
        self.watermark()
    }
}
