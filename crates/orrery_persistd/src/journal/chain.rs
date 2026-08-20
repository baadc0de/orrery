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

/// A condition that stops chain replication and cannot be retried away.
///
/// Chain faults exist because the chain's only alarm is a lag gauge, and both
/// of the ways this loop can wedge report a healthy zero-byte lag: the gauge
/// advances from [`update_progress`], which runs only on a *successful* probe
/// or push. A wedged replicator therefore has to say so itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainFault {
    /// The follower reported a durable watermark ahead of everything this
    /// primary has committed.
    ///
    /// The batch filter drops every record at or below the probed cursor, so
    /// an over-large cursor empties every batch and the loop parks on the
    /// commit broadcast forever — at zero reported lag. The watermark can only
    /// be ahead if the follower holds history this primary never wrote, so
    /// resuming would mirror onto a chain that already diverged.
    FollowerAhead {
        /// The watermark the follower reported.
        remote: Lsn,
        /// This primary's committed LSN, or `None` for an empty journal.
        committed: Option<Lsn>,
    },
    /// The primary's own journal scan failed, so the loop can no longer read
    /// the records it exists to mirror.
    ///
    /// The underlying [`JournalError`] is deliberately *not* carried here.
    /// This enum is `Copy` and compared by value by every consumer; a variant
    /// holding an error message would be neither. The error is logged at the
    /// break instead, and the fault is the part an operator polls for.
    PrimaryScanFailed,
}

impl core::fmt::Display for ChainFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FollowerAhead { remote, committed } => match committed {
                Some(committed) => write!(
                    f,
                    "follower watermark {remote} is ahead of the primary's committed LSN \
                     {committed}"
                ),
                None => write!(
                    f,
                    "follower watermark {remote} is ahead of an empty primary journal"
                ),
            },
            Self::PrimaryScanFailed => {
                write!(f, "the primary journal scan failed; see the chain log")
            }
        }
    }
}

/// A point-in-time reading of one chain's health, for a delta reporter or an
/// operator endpoint to publish under the docs/13 §6 chain series.
///
/// The first four fields used to be the whole reading, and all four report a
/// healthy chain while the replicator is wedged: `lag_bytes` and `watermark`
/// are written only by [`update_progress`], which runs only on a *successful*
/// probe or push, so both **freeze** at their last value — zero, if the
/// follower died while the chain was caught up. The remaining fields are the
/// ones that move when nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainSnapshot {
    /// The follower node id this chain streams to.
    pub follower: u64,
    /// The highest primary LSN the follower has reported durable.
    pub watermark: Option<Lsn>,
    /// Replication lag in journal bytes.
    pub lag_bytes: u64,
    /// Set once the chain has stopped for a reason retrying cannot fix.
    pub fault: Option<ChainFault>,
    /// Whether the replication task is still alive. A task that panicked sets
    /// no fault, so this is the only field that reports one.
    pub running: bool,
    /// Milliseconds since the last successful probe or push — the age of
    /// `watermark` and `lag_bytes`, and the D16 `chain_lag_age_ms` reading.
    pub progress_age_ms: u64,
    /// Pushes that have failed since the last successful one; zero on a
    /// healthy chain, and climbing at the retry rate on a wedged one.
    pub failed_pushes: u64,
    /// Whether the primary has committed past `watermark` *right now*, read
    /// live from the journal rather than from the frozen gauge.
    ///
    /// Always `false` for a promotion-adopted chain, whose watermark lives in
    /// the source's LSN space and is not comparable with this journal's
    /// committed cursor; `failed_pushes` is the stall primitive there.
    pub behind: bool,
}

impl ChainSnapshot {
    /// Whether this chain is wedged: stopped outright, or owing work it has
    /// made no progress on for longer than `grace`.
    ///
    /// This is deliberately *not* gated on `lag_bytes > 0`, which cannot fire
    /// for the headline case: a follower killed while the chain is caught up
    /// leaves the gauge at 0, and with no further appends the loop parks on
    /// the commit broadcast without making a transport call at all. Live
    /// state (`behind`) and failed pushes are what still move.
    #[must_use]
    pub fn stalled_for(&self, grace: Duration) -> bool {
        if self.fault.is_some() || !self.running {
            return true;
        }
        (self.behind || self.failed_pushes > 0)
            && u128::from(self.progress_age_ms) >= grace.as_millis()
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

/// Everything the replication task publishes and [`ChainReplicator`] reads.
///
/// Bundled rather than threaded through as five separate `Arc`s because one
/// progress update writes four of them at once, and the added fields are only
/// meaningful next to the frozen ones.
#[derive(Debug)]
struct ChainState {
    /// The follower's durable watermark, updated as the transport reports it.
    watermark: std::sync::Mutex<Option<Lsn>>,
    /// Replication lag: `primary.committed()` minus the follower's reported
    /// durable watermark. LSN offsets are byte positions, so the raw gap is in
    /// bytes; [`ChainReplicator::lag_bytes`] exposes it and the alarm compares
    /// it against [`ChainConfig::lag_alarm`]'s byte budget (D11's "~100 ms of
    /// journal" mapped to bytes — see [`lag_alarm_bytes`]).
    lag_bytes: std::sync::atomic::AtomicU64,
    /// Set when the loop stopped for a reason retrying cannot fix.
    fault: std::sync::Mutex<Option<ChainFault>>,
    /// The spawn instant `last_progress_ms` is measured from. An `Instant`
    /// does not fit in an atomic, and the age is all any consumer wants.
    started: std::time::Instant,
    /// Milliseconds after `started` of the last successful probe or push.
    last_progress_ms: std::sync::atomic::AtomicU64,
    /// Pushes failed since the last successful one.
    failed_pushes: std::sync::atomic::AtomicU64,
}

impl ChainState {
    fn new() -> Self {
        Self {
            watermark: std::sync::Mutex::new(None),
            lag_bytes: std::sync::atomic::AtomicU64::new(0),
            fault: std::sync::Mutex::new(None),
            started: std::time::Instant::now(),
            last_progress_ms: std::sync::atomic::AtomicU64::new(0),
            failed_pushes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Stamp a successful probe or push. Progress clears the failure run: the
    /// counter answers "is this chain moving", not "has it ever failed".
    fn note_progress(&self) {
        self.last_progress_ms
            .store(self.elapsed_ms(), std::sync::atomic::Ordering::Release);
        self.failed_pushes
            .store(0, std::sync::atomic::Ordering::Release);
    }

    fn note_failed_push(&self) {
        self.failed_pushes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn progress_age_ms(&self) -> u64 {
        self.elapsed_ms().saturating_sub(
            self.last_progress_ms
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    fn set_fault(&self, fault: ChainFault) {
        *self.fault.lock().expect("chain fault lock") = Some(fault);
    }
}

/// A running chain-replication task: subscribes to the primary journal's
/// committed records and pushes them to the follower.
pub struct ChainReplicator {
    /// The primary journal, kept so a snapshot can compare *live* committed
    /// state against the follower watermark. The lag gauge cannot: it is
    /// written only on success, so it holds its last value when the chain
    /// wedges instead of growing.
    journal: Arc<Journal>,
    /// State published by the task.
    state: Arc<ChainState>,
    /// Join handle; awaited on shutdown.
    join: tokio::task::JoinHandle<()>,
    /// Signals the task to stop.
    shutdown: Arc<ShutdownSignal>,
    /// The follower node id (for diagnostics).
    follower: u64,
    /// Whether the watermark and `journal.committed()` share an LSN space.
    /// False for a promotion-adopted chain, which re-exports another node's.
    comparable_watermark: bool,
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
        *self.state.watermark.lock().expect("chain watermark lock")
    }

    /// The current replication lag in journal bytes: `primary.committed()`
    /// minus the highest primary LSN the follower has reported durable. Both
    /// are in the primary's LSN space (the follower echoes each record's own
    /// LSN), so the difference is a byte gap; the lag alarm fires (a
    /// `tracing::warn!`) while this exceeds the [`ChainConfig::lag_alarm`]
    /// byte budget.
    ///
    /// Read it together with [`ChainReplicator::progress_age`]: this gauge
    /// advances only on a successful probe or push, so a wedged chain holds
    /// whatever it last read rather than growing.
    #[must_use]
    pub fn lag_bytes(&self) -> u64 {
        self.state
            .lag_bytes
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The fault that stopped this chain, if one did.
    ///
    /// `None` covers both a healthy chain and a merely degraded one: a
    /// transport error is retried and is not a fault.
    #[must_use]
    pub fn fault(&self) -> Option<ChainFault> {
        *self.state.fault.lock().expect("chain fault lock")
    }

    /// Whether the replication task is still alive.
    ///
    /// The task exits on shutdown, on a fault, and when the commit broadcast
    /// closes — but it can also **panic**, at the bare `expect` guarding the
    /// adopted-history scan, and a panicked task publishes no fault at all.
    /// This accessor is what makes that case observable.
    #[must_use]
    pub fn running(&self) -> bool {
        !self.join.is_finished()
    }

    /// Time since the last successful probe or push: the age of both
    /// [`ChainReplicator::follower_watermark`] and
    /// [`ChainReplicator::lag_bytes`], since nothing else writes them.
    #[must_use]
    pub fn progress_age(&self) -> Duration {
        Duration::from_millis(self.state.progress_age_ms())
    }

    /// Pushes that have failed since the last successful one.
    #[must_use]
    pub fn failed_pushes(&self) -> u64 {
        self.state
            .failed_pushes
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether the primary has committed past the follower's reported
    /// watermark right now — the live read the frozen gauge cannot give.
    ///
    /// Always `false` for a promotion-adopted chain: its watermark is in the
    /// source's LSN space, so the comparison would be meaningless rather than
    /// merely stale.
    #[must_use]
    pub fn behind(&self) -> bool {
        if !self.comparable_watermark {
            return false;
        }
        match (
            self.journal.committed_watermark(),
            self.follower_watermark(),
        ) {
            (Some(committed), Some(durable)) => committed > durable,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Everything a reporter needs about this chain in one consistent read.
    #[must_use]
    pub fn snapshot(&self) -> ChainSnapshot {
        ChainSnapshot {
            follower: self.follower,
            watermark: self.follower_watermark(),
            lag_bytes: self.lag_bytes(),
            fault: self.fault(),
            running: self.running(),
            progress_age_ms: self.state.progress_age_ms(),
            failed_pushes: self.failed_pushes(),
            behind: self.behind(),
        }
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
/// own LSNs, so both live in the primary's LSN space.
///
/// A watermark in an earlier segment is measured through `segment_size`, the
/// stride at which the cursor rolls. Skipping that sample instead — which is
/// what this used to do — froze the gauge at its last value on **every**
/// ordinary segment roll, not only when the chain was in trouble, and the lag
/// gauge is the chain's only alarm. The result is an upper bound: a segment
/// closes at the first record that would cross the stride, so it can hold
/// slightly fewer bytes than the stride itself.
///
/// `None` means the watermark is *ahead* of the primary, which is not a
/// distance at all — it is [`ChainFault::FollowerAhead`], and the caller
/// reports it as one.
fn lag_in_bytes(committed: Lsn, watermark: Lsn, segment_size: u64) -> Option<u64> {
    if watermark > committed {
        return None;
    }
    let segments = committed.segment - watermark.segment;
    Some(
        segments
            .saturating_mul(segment_size)
            .saturating_add(committed.offset)
            .saturating_sub(watermark.offset),
    )
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
    let state = Arc::new(ChainState::new());
    let shutdown = Arc::new(ShutdownSignal::default());
    // An adopted chain re-exports records that keep their *source* LSNs, so
    // the follower echoes a watermark from the source's LSN space while
    // `committed()` reports this journal's own. Only an originated chain has
    // both in one space, so only an originated chain can bound the probe.
    let bound_watermark = matches!(source, ChainSource::Originated);
    let follower = config.follower;
    let batch_max = config.batch_max.max(1);
    let alarm_bytes = lag_alarm_bytes(config.lag_alarm);
    let mut rx = journal.subscribe();
    let handle = Arc::clone(&journal);
    let st = Arc::clone(&state);
    let sd = Arc::clone(&shutdown);

    // Retention answers to this chain from the moment it is spawned, not from
    // its first successful probe: between the two, the follower's watermark is
    // unknown and every record is potentially one it still has to be sent.
    journal.register_chain(bound_watermark);

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
                    // A watermark past this primary's committed LSN is not a
                    // resume point: the batch filter drops every record at or
                    // below the cursor, so accepting it empties the batch and
                    // parks the loop on the commit broadcast at zero lag. The
                    // follower holds history this primary never wrote, and no
                    // amount of retrying makes that a resumable chain.
                    let committed = journal.committed_watermark();
                    if bound_watermark && committed.is_none_or(|local| remote > local) {
                        let reason = ChainFault::FollowerAhead { remote, committed };
                        tracing::error!(follower, %reason, "chain: replication stopped");
                        st.set_fault(reason);
                        break;
                    }
                    cursor = Some(remote);
                    update_progress(&journal, &st, alarm_bytes, follower, remote);
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
                    // The loop cannot read what it exists to mirror, and the
                    // gauges it leaves behind still say zero. Publish the
                    // fault before exiting so the chain is not silently down.
                    tracing::error!(follower, ?error, "chain: primary journal scan failed");
                    st.set_fault(ChainFault::PrimaryScanFailed);
                    break;
                }
            };

            if !batch.is_empty() {
                let pushed = push_batch(
                    &journal,
                    &*transport,
                    &batch,
                    &sd,
                    &st,
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
        journal: handle,
        state,
        join,
        shutdown,
        follower,
        comparable_watermark: bound_watermark,
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

/// Publish one successful probe or push.
///
/// This is the *only* writer of the watermark and the lag gauge, which is why
/// it also stamps the progress clock: a chain that stops calling this holds
/// both readings unchanged, and the age is the only thing left that moves.
fn update_progress(
    journal: &Journal,
    state: &ChainState,
    alarm_bytes: u64,
    follower: u64,
    durable: Lsn,
) {
    *state.watermark.lock().expect("chain watermark lock") = Some(durable);
    // Retention may not release records this follower would have to rescan
    // (D20). The journal drops the update if this chain registered as
    // non-bounding, so an adopted chain's source-space watermark never becomes
    // a floor in this journal's space.
    journal.note_chain_watermark(durable);
    state.note_progress();
    if let Some(bytes) = lag_in_bytes(journal.committed(), durable, journal.segment_size()) {
        state
            .lag_bytes
            .store(bytes, std::sync::atomic::Ordering::Release);
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
async fn push_batch(
    journal: &Journal,
    transport: &dyn ChainTransport,
    records: &[JournalRecord],
    shutdown: &ShutdownSignal,
    state: &ChainState,
    alarm_bytes: u64,
    follower: u64,
) -> PushOutcome {
    if shutdown.armed() {
        return PushOutcome {
            last: None,
            complete: false,
        };
    }
    let pushed = tokio::select! {
        r = transport.append_batch(records.to_vec()) => r,
        _ = shutdown.wake.notified() => {
            return PushOutcome { last: None, complete: false };
        }
    };
    match pushed {
        Ok(durable) if durable >= records.last().expect("non-empty batch").lsn => {
            update_progress(journal, state, alarm_bytes, follower, durable);
            PushOutcome {
                last: Some(durable),
                complete: true,
            }
        }
        Ok(durable) => {
            tracing::warn!(
                follower,
                record_lsn = %records.last().expect("non-empty batch").lsn,
                durable_lsn = %durable,
                "chain: follower returned a regressed batch watermark"
            );
            state.note_failed_push();
            PushOutcome {
                last: None,
                complete: false,
            }
        }
        Err(error) => {
            tracing::warn!(follower, ?error, "chain: follower batch append failed");
            // A retried transport error is not a fault, but a *run* of them is
            // the only thing that moves while the lag gauge stays frozen.
            state.note_failed_push();
            PushOutcome {
                last: None,
                complete: false,
            }
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

#[cfg(test)]
mod lag_alarm_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use orrery_protocol::{CellId, Epoch, GridId, PersistId, RecordKind, Tick};

    use crate::journal::{AdaptiveCommitMode, GroupCommitConfig, JournalConfig};

    #[test]
    fn lag_is_measured_across_a_segment_roll() {
        const SEGMENT: u64 = 128 * 1024 * 1024;
        // The gauge used to return `None` here and hold its last value, so an
        // ordinary segment roll looked exactly like a healthy chain.
        assert_eq!(
            lag_in_bytes(Lsn::new(1, 40), Lsn::new(0, SEGMENT - 60), SEGMENT),
            Some(100)
        );
        assert_eq!(
            lag_in_bytes(Lsn::new(3, 10), Lsn::new(1, 10), SEGMENT),
            Some(2 * SEGMENT)
        );
        assert_eq!(
            lag_in_bytes(Lsn::new(0, 90), Lsn::new(0, 30), SEGMENT),
            Some(60)
        );
        // A watermark past the primary is a fault, not a negative distance.
        assert_eq!(lag_in_bytes(Lsn::new(0, 30), Lsn::new(1, 0), SEGMENT), None);
    }

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
            journal
                .append(test_record(entity))
                .unwrap()
                .committed()
                .await
                .unwrap();
        }
        let transport = Arc::new(BatchSpy {
            batches: Mutex::new(Vec::new()),
        });
        let replicator = spawn_chain(
            Arc::clone(&journal),
            Arc::clone(&transport) as Arc<dyn ChainTransport>,
            &ChainConfig {
                follower: 9,
                batch_max: 3,
                ..ChainConfig::default()
            },
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

    #[tokio::test]
    async fn a_primary_scan_failure_faults_and_stops_the_task() {
        // The unrecoverable read is a *per-item* error inside the outbound
        // scan, and `spawn_chain` takes a concrete journal, so this test lives
        // in-crate: `inject_scan_fault` is the only seam that reaches it. The
        // outer-scan `expect` is a different branch — it panics the task, and
        // `running()` is what observes that one.
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
        journal
            .append(test_record(1))
            .unwrap()
            .committed()
            .await
            .unwrap();
        journal.inject_scan_fault();

        let transport = Arc::new(BatchSpy {
            batches: Mutex::new(Vec::new()),
        });
        let replicator = spawn_chain(
            Arc::clone(&journal),
            Arc::clone(&transport) as Arc<dyn ChainTransport>,
            &ChainConfig {
                follower: 11,
                ..ChainConfig::default()
            },
        );

        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = replicator.snapshot();
                if snapshot.fault.is_some() && !snapshot.running {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a scan failure must fault, not exit quietly");

        assert_eq!(snapshot.fault, Some(ChainFault::PrimaryScanFailed));
        assert!(!snapshot.running);
        // The reason the fault has to exist: the task broke out of the loop
        // before ever pushing, so the two original gauges still read healthy.
        assert_eq!(snapshot.lag_bytes, 0);
        assert_eq!(snapshot.watermark, None);
        assert!(snapshot.stalled_for(Duration::ZERO));
        assert!(transport.batches.lock().expect("batch spy lock").is_empty());

        replicator.shutdown().await;
        journal.close().await.unwrap();
    }
}
