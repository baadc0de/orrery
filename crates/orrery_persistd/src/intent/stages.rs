//! Stage decomposition for the intent path (`intent_commit_ms`, D16 p99 < 10 ms).
//!
//! # Why this exists
//!
//! `gateway_intent_server_ms` reported one number for everything between the
//! gateway's receipt stamp and its reply, and `intent_commit_ms` reported one
//! number for the client round trip around it. At ~200 intents/s the second
//! showed p50 6-8 ms with a p99 in the 100-150 ms bucket — a 20x ratio at a
//! load where nothing in the system is saturated. One number cannot say which
//! of the eight waits on that path produced it, and the routing path was in
//! exactly this position until [`crate::cluster::RouteStageMetrics`] split
//! `router_apply` into gate-wait / locate / mailbox. This is the same split for
//! intents, and it is modelled on that struct deliberately.
//!
//! # Denominators — read this before dividing anything
//!
//! There are **two** denominators here and they are not interchangeable:
//!
//! * [`IntentStageTotals::intents`] — one per *definitive reply*, incremented
//!   in `send_intent_reply`. Every gateway-side stage (`ingress`, `admit`,
//!   `spawn_wait`, `exec`, `reply`, `server`) is summed once per reply, so
//!   `<stage>_us_sum / intents` is a mean over intents.
//! * [`IntentStageTotals::executed`] — one per intent that actually reached
//!   [`crate::intent::IntentExecutor::execute`]. An intent refused at admission
//!   or by lane saturation has `exec_us == 0` and never touches the FDB
//!   stages, so dividing an FDB stage by `intents` understates it. Divide FDB
//!   stages (`alloc_wait`, `alloc_refill`, `grv`, `idem_read`, `fence`,
//!   `commit`, `backoff`) by `executed`.
//!
//! Neither is per-flush and neither is per-op. The failure mode this warning
//! exists for is real: `JournalStageSnapshot` samples once per *flush* and
//! dividing its sums by records understated every stage ~30x. Nothing here is
//! sampled per flush — every counter is incremented exactly once per intent,
//! at one call site, from a trace the intent carried with it.
//!
//! # The gap is a stage too
//!
//! Two derived quantities are the point of the whole instrument:
//!
//! ```text
//! server_gap = server_us - (admit_us + spawn_wait_us + exec_us)
//! fdb_gap    = exec_us   - (alloc_wait + alloc_refill + grv + idem_read
//!                           + fence + commit + backoff)
//! ```
//!
//! `server_gap` is time inside the measured server span that no stage claims.
//! `fdb_gap` is time inside `IntentExecutor::execute` that no FDB phase claims
//! — i.e. the tokio scheduler delay between libfdb_c's network thread waking
//! the intent future and a worker polling it, summed over every await hop in
//! the transaction. Neither is a residual anyone should have to compute from a
//! doc: both are emitted, because an unattributed gap is a finding and this
//! project has been bitten by one before (a location audit whose cost was
//! excluded from every stage timer and reappeared as the next diff's gate
//! wait, docs/08 §2.1.3).
//!
//! # Sums and maxima are not enough for a tail, so there is a third view
//!
//! A p99 cannot be read out of a sum and a max. [`IntentStageMetrics`]
//! therefore keeps the same field set **twice**: once over every intent
//! (`all`), and once over only those whose server span exceeded
//! [`slow_threshold_us`] (`slow`). `slow.<stage>_us_sum / slow.intents` is the
//! mean decomposition *of the tail itself*, which is the number the question
//! actually asks for. A per-interval exemplar — the single slowest intent's
//! whole trace — is emitted alongside, so one concrete 150 ms sample can be
//! read stage by stage rather than inferred from means.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Default server-span threshold, in microseconds, above which an intent is
/// folded into the `slow` accumulator as well as `all`.
///
/// 20 ms: twice the D16 budget, so the `slow` population is unambiguously
/// "intents that missed the target", not "intents near it". Overridable with
/// `ORRERY_INTENT_SLOW_US` for a study that wants a different cut.
const DEFAULT_SLOW_THRESHOLD_US: u64 = 20_000;

/// The per-intent stage timings, accumulated as the intent moves along the
/// path and folded into [`IntentStageMetrics`] exactly once, at the reply.
///
/// Every field is microseconds unless its name says otherwise. A stage that
/// did not run stays zero — an admission refusal has no `exec_us`, an
/// unfenced executor no `fence_us` — which is why `executed` exists as a
/// separate denominator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntentTrace {
    /// Transport dequeue through the connection receive loop picking this
    /// message up. Upstream of the server span entirely, and *inside*
    /// `intent_commit_ms`: an intent queued behind a burst of diffs waited
    /// here and no intent series has ever counted it.
    pub ingress_us: u64,
    /// Receipt stamp through the admission verdict: one ed25519 issuer verify
    /// plus the validator, synchronous, on the receive loop.
    pub admit_us: u64,
    /// `tokio::spawn` call through the spawned task's first statement.
    ///
    /// The runtime-queue detector. It is measured because nothing else on
    /// this path can distinguish "FDB was slow" from "a worker did not pick
    /// the task up for 40 ms while 18 500 diff routes/s occupied every local
    /// queue".
    pub spawn_wait_us: u64,
    /// Wall time inside `IntentExecutor::execute`.
    pub exec_us: u64,
    /// Wait on the process-wide `PersistId` allocator mutex.
    pub alloc_wait_us: u64,
    /// Time inside a `pid/next` block refill — a whole separate FDB
    /// transaction, run with the allocator mutex **held**. Zero on all but
    /// roughly every 4096th intent; see [`IntentStageTotals::alloc_refills`].
    pub alloc_refill_us: u64,
    /// Get-read-version, taken explicitly as the transaction's first act so it
    /// is a stage rather than an invisible prefix of the idempotency read.
    pub grv_us: u64,
    /// The `intent/{intent_id}` idempotency read.
    pub idem_read_us: u64,
    /// Wall time of the ownership fence's concurrent read fan-out.
    pub fence_us: u64,
    /// The **slowest single** fence read in that fan-out.
    ///
    /// The discriminator between "FDB is slow" and "the scheduler is slow":
    /// the fence waits on the max of N concurrent reads, so if `fence_us` is
    /// large and this is large too, the cluster served one read slowly; if
    /// `fence_us` is large and this is small, the time went between the reads
    /// completing and the task being polled.
    pub fence_read_max_us: u64,
    /// Reads the fence issued (the node's activated shard count, per attempt).
    pub fence_reads: u64,
    /// Commit: the closure's last statement through the binding's
    /// `on_commit_success`.
    pub commit_us: u64,
    /// Summed `on_error` backoff between attempts. Nonzero only if `db.run`
    /// retried, which nothing in this repo could previously observe.
    pub backoff_us: u64,
    /// Closure invocations. `1` is the no-retry case; `attempts - 1` retries.
    pub attempts: u64,
    /// The most recent retryable FDB error code seen on this intent, or 0.
    pub last_err_code: u64,
    /// Receipt stamp through `record_reply` — the server span, duplicated here
    /// so the stage sum can be checked against it within one trace.
    pub server_us: u64,
    /// `record_reply` through the send closure returning.
    pub reply_us: u64,
}

impl IntentTrace {
    /// Stage time inside the server span that is claimed by a named stage.
    #[must_use]
    pub fn server_claimed_us(&self) -> u64 {
        self.admit_us + self.spawn_wait_us + self.exec_us
    }

    /// Server-span time no stage claims. See the module docs.
    #[must_use]
    pub fn server_gap_us(&self) -> u64 {
        self.server_us.saturating_sub(self.server_claimed_us())
    }

    /// Execution time claimed by a named FDB phase.
    #[must_use]
    pub fn fdb_claimed_us(&self) -> u64 {
        self.alloc_wait_us
            + self.alloc_refill_us
            + self.grv_us
            + self.idem_read_us
            + self.fence_us
            + self.commit_us
            + self.backoff_us
    }

    /// Execution time no FDB phase claims — the scheduler-hop term.
    #[must_use]
    pub fn fdb_gap_us(&self) -> u64 {
        self.exec_us.saturating_sub(self.fdb_claimed_us())
    }
}

/// One accumulator's worth of [`IntentTrace`] fields.
///
/// Held twice by [`IntentStageMetrics`] — over all intents and over slow ones
/// — so the tail has its own decomposition rather than being averaged away.
#[derive(Debug, Default)]
pub struct IntentStageTotals {
    intents: AtomicU64,
    executed: AtomicU64,
    alloc_refills: AtomicU64,
    fence_reads: AtomicU64,
    attempts: AtomicU64,
    ingress_us_sum: AtomicU64,
    ingress_us_max: AtomicU64,
    admit_us_sum: AtomicU64,
    admit_us_max: AtomicU64,
    spawn_wait_us_sum: AtomicU64,
    spawn_wait_us_max: AtomicU64,
    exec_us_sum: AtomicU64,
    exec_us_max: AtomicU64,
    alloc_wait_us_sum: AtomicU64,
    alloc_wait_us_max: AtomicU64,
    alloc_refill_us_sum: AtomicU64,
    alloc_refill_us_max: AtomicU64,
    grv_us_sum: AtomicU64,
    grv_us_max: AtomicU64,
    idem_read_us_sum: AtomicU64,
    idem_read_us_max: AtomicU64,
    fence_us_sum: AtomicU64,
    fence_us_max: AtomicU64,
    fence_read_max_us: AtomicU64,
    commit_us_sum: AtomicU64,
    commit_us_max: AtomicU64,
    backoff_us_sum: AtomicU64,
    backoff_us_max: AtomicU64,
    server_us_sum: AtomicU64,
    server_us_max: AtomicU64,
    reply_us_sum: AtomicU64,
    reply_us_max: AtomicU64,
    server_gap_us_sum: AtomicU64,
    server_gap_us_max: AtomicU64,
    fdb_gap_us_sum: AtomicU64,
    fdb_gap_us_max: AtomicU64,
}

/// A point-in-time read of an [`IntentStageTotals`], usable as a drain cursor.
///
/// Field-for-field with the accumulator. `delta` subtracts sums and counters
/// and takes the *later* maxima verbatim — a max is not differenceable, so an
/// interval's reported max is the running max, which for a per-second report
/// is dominated by that second's worst sample and is exactly what a tail hunt
/// wants. Documented rather than fixed, because the alternative (resetting the
/// max) loses the run-wide worst case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct IntentStageSnapshot {
    /// Definitive replies. Denominator for every gateway-side stage.
    pub intents: u64,
    /// Intents that reached the executor. Denominator for every FDB stage.
    pub executed: u64,
    /// `pid/next` block refills, each an FDB transaction under the allocator
    /// mutex.
    pub alloc_refills: u64,
    /// Fence reads issued, summed. `/ executed` is the fan-out width.
    pub fence_reads: u64,
    /// Closure invocations. `attempts - executed` is the retry count, which
    /// nothing in this repo could previously observe.
    pub attempts: u64,
    pub ingress_us_sum: u64,
    pub ingress_us_max: u64,
    pub admit_us_sum: u64,
    pub admit_us_max: u64,
    pub spawn_wait_us_sum: u64,
    pub spawn_wait_us_max: u64,
    pub exec_us_sum: u64,
    pub exec_us_max: u64,
    pub alloc_wait_us_sum: u64,
    pub alloc_wait_us_max: u64,
    pub alloc_refill_us_sum: u64,
    pub alloc_refill_us_max: u64,
    pub grv_us_sum: u64,
    pub grv_us_max: u64,
    pub idem_read_us_sum: u64,
    pub idem_read_us_max: u64,
    pub fence_us_sum: u64,
    pub fence_us_max: u64,
    pub fence_read_max_us: u64,
    pub commit_us_sum: u64,
    pub commit_us_max: u64,
    pub backoff_us_sum: u64,
    pub backoff_us_max: u64,
    pub server_us_sum: u64,
    pub server_us_max: u64,
    pub reply_us_sum: u64,
    pub reply_us_max: u64,
    pub server_gap_us_sum: u64,
    pub server_gap_us_max: u64,
    pub fdb_gap_us_sum: u64,
    pub fdb_gap_us_max: u64,
}

macro_rules! stage_fields {
    ($mac:ident) => {
        $mac! {
            counters: [intents, executed, alloc_refills, fence_reads, attempts],
            sums: [
                ingress_us_sum, admit_us_sum, spawn_wait_us_sum, exec_us_sum,
                alloc_wait_us_sum, alloc_refill_us_sum, grv_us_sum, idem_read_us_sum,
                fence_us_sum, commit_us_sum, backoff_us_sum, server_us_sum,
                reply_us_sum, server_gap_us_sum, fdb_gap_us_sum
            ],
            maxima: [
                ingress_us_max, admit_us_max, spawn_wait_us_max, exec_us_max,
                alloc_wait_us_max, alloc_refill_us_max, grv_us_max, idem_read_us_max,
                fence_us_max, fence_read_max_us, commit_us_max, backoff_us_max,
                server_us_max, reply_us_max, server_gap_us_max, fdb_gap_us_max
            ],
        }
    };
}

macro_rules! impl_snapshot {
    (counters: [$($c:ident),* $(,)?], sums: [$($s:ident),* $(,)?], maxima: [$($m:ident),* $(,)?],) => {
        impl IntentStageTotals {
            /// Read every counter.
            #[must_use]
            pub fn snapshot(&self) -> IntentStageSnapshot {
                let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
                IntentStageSnapshot {
                    $($c: load(&self.$c),)*
                    $($s: load(&self.$s),)*
                    $($m: load(&self.$m),)*
                }
            }
        }

        impl IntentStageSnapshot {
            /// Counters and sums since `previous`; maxima verbatim.
            #[must_use]
            pub fn delta(self, previous: Self) -> Self {
                Self {
                    $($c: self.$c.saturating_sub(previous.$c),)*
                    $($s: self.$s.saturating_sub(previous.$s),)*
                    $($m: self.$m,)*
                }
            }
        }
    };
}

stage_fields!(impl_snapshot);

impl IntentStageTotals {
    fn record(&self, trace: &IntentTrace, executed: bool) {
        self.intents.fetch_add(1, Ordering::Relaxed);
        if executed {
            self.executed.fetch_add(1, Ordering::Relaxed);
        }
        if trace.alloc_refill_us > 0 {
            self.alloc_refills.fetch_add(1, Ordering::Relaxed);
        }
        self.fence_reads
            .fetch_add(trace.fence_reads, Ordering::Relaxed);
        self.attempts.fetch_add(trace.attempts, Ordering::Relaxed);
        for (sum, max, value) in [
            (&self.ingress_us_sum, &self.ingress_us_max, trace.ingress_us),
            (&self.admit_us_sum, &self.admit_us_max, trace.admit_us),
            (
                &self.spawn_wait_us_sum,
                &self.spawn_wait_us_max,
                trace.spawn_wait_us,
            ),
            (&self.exec_us_sum, &self.exec_us_max, trace.exec_us),
            (
                &self.alloc_wait_us_sum,
                &self.alloc_wait_us_max,
                trace.alloc_wait_us,
            ),
            (
                &self.alloc_refill_us_sum,
                &self.alloc_refill_us_max,
                trace.alloc_refill_us,
            ),
            (&self.grv_us_sum, &self.grv_us_max, trace.grv_us),
            (
                &self.idem_read_us_sum,
                &self.idem_read_us_max,
                trace.idem_read_us,
            ),
            (&self.fence_us_sum, &self.fence_us_max, trace.fence_us),
            (&self.commit_us_sum, &self.commit_us_max, trace.commit_us),
            (&self.backoff_us_sum, &self.backoff_us_max, trace.backoff_us),
            (&self.server_us_sum, &self.server_us_max, trace.server_us),
            (&self.reply_us_sum, &self.reply_us_max, trace.reply_us),
            (
                &self.server_gap_us_sum,
                &self.server_gap_us_max,
                trace.server_gap_us(),
            ),
            (
                &self.fdb_gap_us_sum,
                &self.fdb_gap_us_max,
                trace.fdb_gap_us(),
            ),
        ] {
            sum.fetch_add(value, Ordering::Relaxed);
            max.fetch_max(value, Ordering::Relaxed);
        }
        self.fence_read_max_us
            .fetch_max(trace.fence_read_max_us, Ordering::Relaxed);
    }
}

/// Process-global intent stage decomposition.
///
/// Global for the same reason [`crate::cluster::RouteStageMetrics`] is: a node
/// runs one gateway over one executor, so the process aggregate *is* the
/// intent path's. It is also what lets the FDB executor — which the gateway
/// only sees behind `dyn IntentExecutor` — write into the same trace without
/// widening that trait.
#[derive(Debug, Default)]
pub struct IntentStageMetrics {
    /// Every intent that got a definitive reply.
    pub all: IntentStageTotals,
    /// Only intents whose server span exceeded [`slow_threshold_us`].
    pub slow: IntentStageTotals,
    exemplar: Mutex<Option<IntentTrace>>,
}

impl IntentStageMetrics {
    /// Fold one finished intent's trace into both accumulators and offer it as
    /// this interval's exemplar.
    pub fn record(&self, trace: &IntentTrace, executed: bool) {
        self.all.record(trace, executed);
        if trace.server_us >= slow_threshold_us() {
            self.slow.record(trace, executed);
        }
        if let Ok(mut slot) = self.exemplar.lock() {
            let better = slot.is_none_or(|current| trace.server_us > current.server_us);
            if better {
                *slot = Some(*trace);
            }
        }
    }

    /// Take the slowest trace seen since the last call, clearing it.
    ///
    /// Cleared on read so each report interval's exemplar is that interval's
    /// worst intent, not the run's. The run-wide worst is already kept, as
    /// `all.server_us_max`.
    pub fn take_exemplar(&self) -> Option<IntentTrace> {
        self.exemplar.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// The server-span threshold above which an intent counts as slow.
///
/// Read once from `ORRERY_INTENT_SLOW_US`, defaulting to
/// [`DEFAULT_SLOW_THRESHOLD_US`].
#[must_use]
pub fn slow_threshold_us() -> u64 {
    static THRESHOLD: OnceLock<u64> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("ORRERY_INTENT_SLOW_US")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(DEFAULT_SLOW_THRESHOLD_US)
    })
}

/// The process-global stage metrics.
#[must_use]
pub fn intent_stage_metrics() -> Arc<IntentStageMetrics> {
    static METRICS: OnceLock<Arc<IntentStageMetrics>> = OnceLock::new();
    Arc::clone(METRICS.get_or_init(|| Arc::new(IntentStageMetrics::default())))
}

tokio::task_local! {
    /// The in-flight intent's trace, scoped by the gateway around the spawned
    /// execution task.
    ///
    /// A task-local rather than an argument because the executor is reached
    /// through `dyn IntentExecutor`, whose one method takes only the intent —
    /// and widening that trait to carry a metrics handle would put an
    /// observability type into the authority seam every future executor has to
    /// implement. Everything that writes to it runs inside the gateway's
    /// scope, on the same task; anything outside one (a unit test calling the
    /// executor directly) silently records nothing.
    static TRACE: RefCell<IntentTrace>;
}

/// Run `fut` with a fresh trace in scope, returning the trace alongside its
/// output.
pub async fn with_trace<F: std::future::Future>(fut: F) -> (F::Output, IntentTrace) {
    TRACE
        .scope(RefCell::new(IntentTrace::default()), async move {
            let out = fut.await;
            let trace = TRACE.with(|cell| *cell.borrow());
            (out, trace)
        })
        .await
}

/// Mutate the in-flight trace, if one is in scope.
///
/// A no-op outside [`with_trace`], which is what keeps the executor's own unit
/// tests and the in-memory executor free of any metrics wiring.
pub fn trace(f: impl FnOnce(&mut IntentTrace)) {
    let _ = TRACE.try_with(|cell| f(&mut cell.borrow_mut()));
}

/// Time `fut`, adding its microseconds to the trace field `f` selects.
pub async fn timed<T>(
    field: fn(&mut IntentTrace) -> &mut u64,
    fut: impl std::future::Future<Output = T>,
) -> T {
    let started = Instant::now();
    let out = fut.await;
    let elapsed = started.elapsed().as_micros() as u64;
    trace(|t| *field(t) += elapsed);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_are_the_unclaimed_remainder() {
        let trace = IntentTrace {
            admit_us: 100,
            spawn_wait_us: 200,
            exec_us: 10_000,
            grv_us: 1_000,
            idem_read_us: 1_000,
            fence_us: 2_000,
            commit_us: 3_000,
            server_us: 12_000,
            ..IntentTrace::default()
        };
        // 12_000 - (100 + 200 + 10_000)
        assert_eq!(trace.server_gap_us(), 1_700);
        // 10_000 - (1_000 + 1_000 + 2_000 + 3_000)
        assert_eq!(trace.fdb_gap_us(), 3_000);
    }

    #[test]
    fn denominators_separate_executed_from_replied() {
        let metrics = IntentStageMetrics::default();
        metrics.record(
            &IntentTrace {
                server_us: 500,
                admit_us: 500,
                ..IntentTrace::default()
            },
            false,
        );
        metrics.record(
            &IntentTrace {
                server_us: 1_000,
                exec_us: 900,
                grv_us: 400,
                ..IntentTrace::default()
            },
            true,
        );
        let all = metrics.all.snapshot();
        assert_eq!(all.intents, 2, "both replies counted");
        assert_eq!(all.executed, 1, "only one reached the executor");
        assert_eq!(all.grv_us_sum, 400);
        // Dividing an FDB stage by `intents` would halve it; by `executed` it
        // is the mean over the intents that actually paid it.
        assert_eq!(all.grv_us_sum / all.executed, 400);
    }

    #[test]
    fn slow_accumulator_holds_only_the_tail() {
        let metrics = IntentStageMetrics::default();
        let fast = IntentTrace {
            server_us: 1_000,
            exec_us: 900,
            ..IntentTrace::default()
        };
        let slow = IntentTrace {
            server_us: slow_threshold_us() + 5_000,
            exec_us: slow_threshold_us(),
            ..IntentTrace::default()
        };
        metrics.record(&fast, true);
        metrics.record(&slow, true);
        let all = metrics.all.snapshot();
        let tail = metrics.slow.snapshot();
        assert_eq!(all.intents, 2);
        assert_eq!(tail.intents, 1, "only the slow one is in the tail view");
        assert_eq!(tail.server_us_sum, slow.server_us);
        assert_eq!(
            metrics.take_exemplar().map(|t| t.server_us),
            Some(slow.server_us),
            "the exemplar is the slowest trace"
        );
        assert_eq!(metrics.take_exemplar(), None, "cleared on read");
    }

    #[test]
    fn delta_subtracts_sums_and_keeps_maxima() {
        let metrics = IntentStageMetrics::default();
        metrics.record(
            &IntentTrace {
                server_us: 9_000,
                exec_us: 8_000,
                ..IntentTrace::default()
            },
            true,
        );
        let cursor = metrics.all.snapshot();
        metrics.record(
            &IntentTrace {
                server_us: 3_000,
                exec_us: 2_000,
                ..IntentTrace::default()
            },
            true,
        );
        let delta = metrics.all.snapshot().delta(cursor);
        assert_eq!(delta.intents, 1);
        assert_eq!(delta.server_us_sum, 3_000);
        assert_eq!(
            delta.server_us_max, 9_000,
            "a max is not differenceable; the running max is reported"
        );
    }

    #[tokio::test]
    async fn trace_is_scoped_to_the_task_and_no_op_outside_it() {
        // Outside a scope this must not panic and must not record.
        trace(|t| t.grv_us += 7);

        let (out, captured) = with_trace(async {
            trace(|t| t.admit_us = 11);
            timed(|t| &mut t.grv_us, async { 42 }).await
        })
        .await;
        assert_eq!(out, 42);
        assert_eq!(captured.admit_us, 11);
        // `timed` measures a real elapsed span; the assertion is that it wrote
        // to the field the selector names, not how long an empty future takes.
        assert_eq!(captured.idem_read_us, 0);
    }
}
