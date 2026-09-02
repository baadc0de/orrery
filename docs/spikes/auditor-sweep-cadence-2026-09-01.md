# #224 — auditor sweep cadence and time-to-detection calibration

**Status:** proposal and measurement interpretation only. This does not amend an
ADR or change either sweep configuration.

## Verdict

Keep the recorded start cadence: **hourly hot-ledger incremental sweep and daily
full archive conservation sweep**. Do not shorten the full sweep until a
production-shaped archive run supplies its duration, resource use, and serving
path impact. Daily is not a one-day delta: the landed full sweep rereads the
complete receipt archive visible at its start.

This is a conditional recommendation, not a production calibration. Orrery is
still described as an in-development workspace (README.md:7), its README says
no enforcement control is promoted to live (README.md:153-160), and this tree
contains no production auditor measurements. The numbers below are either
derived from the checked-in Shape-C layout fixture or labelled assumptions.

Revisit this decision at the earlier of C3 promotion review entering scheduling
range, or the first production-shaped seven-day window which shows a full-pass
p95 above two hours, a missed full-sweep tick, or a material serving-path
regression while the scanner/auditor runs. The two-hour value is not measured:
it is the duration allowance derived from the roadmap proposed daily/full
confirmation within 26 h (24 h maximum wait + 2 h pass).

## What is actually swept today

ADR-0032 defines liveness as both a daily full conservation sweep and an hourly
incremental hot-ledger sweep, emitting findings to the audit pipeline; it gates
C3 only (docs/adr/0032-enforcement-ramp.md:570-605). The implementations have
different scopes and cost shapes.

| Sweep | Default / enablement | What one pass reads and checks | Known cost |
|---|---|---|---|
| Hot-ledger incremental | 3,600,000 ms; starts with an FDB context unless zeroed (crates/orrery_persistd/src/bin/persistd.rs:325-335, :1330-1334) | All current ledger/bal and ledger/item rows; receipt rows strictly after the durable versionstamp cursor; every gap in the ledger family (crates/orrery_persistd/src/audit.rs:603-667, :689-873). It catches duplicate ownership, negative balance, unbacked receipt party, and malformed/stray row findings; it does **not** prove global sanctioned-versus-observed conservation (audit.rs:15-60, :102-132). | No production byte/CPU/I/O cost. The only timing artifact is a 12-sample harness: 73 balance rows, 186 item claims, 150 ms interval, pass max 2 ms and sum 15 ms (docs/data/hot-ledger-sweep-ttd-2026-08-23.json). The daemon logs counts and elapsed time (persistd.rs:2569-2613). |
| Full conservation | 86,400,000 ms, only in explicit --receipt-archive role and unless zeroed (crates/orrery_persistd/src/audit/conservation.rs; persistd.rs:301-312, :1576-1592) | Captures every visible rarchive/*.parquet object, decodes every row, folds account/asset balance effects against sanctioned source/sink ops, then external-merges page-bounded ownership runs by (item, receipt key, ordinal). It catches the same global conservation, continuity, and effect-shape findings (audit.rs:121-132). | Shape-C layout fixture projects a 24-hour all-transfer workload: 45,792,000 receipts and 3.35577 GiB read; the ownership external-sort **heap** working set is 230,272 bytes (224.875 KiB) per full pass, independent of retained transitions, and its **spill** is 42 bytes per transition — 1,923,264,000 bytes (1.791 GiB) on the work directory for that window, held until the pass ends (docs/data/full-conservation-sweep-layout-2026-09-01.json). The work directory defaults to the OS temporary directory, tmpfs on systemd hosts; `--audit-work-dir` / `ORRERY_AUDIT_WORK_DIR` moves it to disk (#912). This is a byte/layout measurement, not a duration benchmark. |

The full-sweep input is real Shape C: ReceiptRow records account-scoped signed
balance deltas and item before/after ownership transitions
(crates/orrery_persistd/src/keyspace.rs:1737-1788), preserved in Parquet columns
(crates/orrery_persistd/src/archive/receipt_object.rs:1-7, :45-53, :173-201).
It needs no inferred identity join.

Two implementation details matter for calibration:

* The hot sweep re-walks balances and items every pass; only receipts are
  incremental. Its cursor advances in the same transaction as the reads, and a
  failed pass advances nothing (crates/orrery_persistd/src/audit.rs:574-686).
* The full sweep has no cursor: it captures an object list, so objects published
  after capture wait for the next run (conservation.rs:151-168, :313-345;
  persistd.rs:2616-2641). Cost therefore follows retained archive history.
  Both worker loops are non-overlapping and use MissedTickBehavior::Skip
  (persistd.rs:2582-2586, :2625-2629).

## Time to detection

Let C be the period, T the time a violation becomes readable, S(T) the first
following pass start whose snapshot includes it, and D(S) that pass start-to-
finding duration:

~~~
TTD(T) = S(T) - T + D(S(T))
~~~

For a non-overrunning periodic worker and an arrival phase U uniform on [0, C),
the model is:

~~~
TTD = C - U + D
E[TTD] = C/2 + E[D]
p95(TTD) ~= 0.95C + p95(D)       (independence approximation)
worst case <= C + max(D)
~~~

Uniform arrivals, independent duration, and no overrun are assumptions—not
claims about production. A violation arriving after a full pass object-list
capture waits for the next pass and has the same formula. Once duration exceeds
a period, Skip means these bounds no longer hold; measure actual starts and
finishes instead.

| Period C | Expected scheduling wait | p95 scheduling wait | Maximum scheduling wait | Additive pass duration |
|---:|---:|---:|---:|---|
| 5 min | 2 min 30 s | 4 min 45 s | < 5 min | + D |
| 15 min | 7 min 30 s | 14 min 15 s | < 15 min | + D |
| 1 h (hot default) | 30 min | 57 min | < 1 h | Harness observed D <= 2 ms; production unknown |
| 24 h (full default) | 12 h | 22 h 48 min | < 24 h | Full D unknown; a 26 h confirmation implies D <= 2 h |

The hot harness sanity-checks, but does not capacity-prove, the model: at
C = 150 ms, 12 planted cases had min/median/p95/max TTD 7/83/156/156 ms with
no miss (docs/data/hot-ledger-sweep-ttd-2026-08-23.json).

## Cost curve and where it bends

For a full pass over archive size A and ownership re-sort bound M, one pass
costs roughly (A, M). With period C, read volume and allocation churn per unit
time are (A/C, M/C), while expected scheduling wait is C/2: shortening the
period makes cost reciprocal but improves expected wait linearly. There is no
measured physical knee yet; production duration/resource distributions must
establish it.

### #890 implementation decision: external merge sort

The full sweep keeps its no-cursor, complete-visible-window meaning. #890
therefore uses a bounded external merge sort over the existing receipt objects:
one 4,096-event run is sorted and spilled at a time, and merge passes retain at
most 16 run heads. This makes ownership-sort *heap* O(page), not O(transitions
in retention), while preserving the same item history and findings in one pass.
The transitions do not disappear: they are spilled at 42 bytes each to the
sweep's work directory and stay there until the pass ends, and a consolidation
stage transiently holds one merged output batch beside its unconsumed inputs,
so the work directory needs headroom of under twice the spill.

#### #912 correction: where the spill lives, and what a spill failure reports

The first version of this section presented 224.875 KiB as the per-pass memory.
That is the heap alone. The spill went to `std::env::temp_dir()`, which on a
systemd host is RAM-backed tmpfs — so the 24 h window's 1.791 GiB and the
30-day arm's 53.735 GiB were resident memory the artifact did not mention, on
a filesystem that is often smaller than the spill. #912 adds
`--audit-work-dir` (`ORRERY_AUDIT_WORK_DIR`), reports `spill_bytes` and
`spill_record_bytes` beside `memory_bytes_bound` in every full-sweep report
and in the daemon's pass-complete line, and restates the tables below with
both numbers. Set the flag on any production-shaped host.

The same review found that a spill failure discarded the verdict: the balance
fold had already computed a `global_conservation_break`, and an ENOSPC in the
ownership merge aborted the pass before the emit loop, every day, until the
disk was fixed. The whole-window balance, effect-shape, and conservation
findings are now emitted the moment the fold completes — before the merge,
which is the only fallible host I/O left in the pass — and a merge failure
returns them again inside `FullSweepFailure`, so the daemon's failure line
counts `conservation_breaks_emitted` beside the error. Ownership-continuity
findings are emitted only when the merge completes; a failure states that
continuity is unverified for the window rather than implying it was clean.

The other offered shapes lose here. A tailer-written item-clustered secondary
Parquet object would add write-path work, a new `z` family sub-kind and an
additional publication/consistency surface; the registered `z` sub-kinds are
already `za` and `zr`, and `yb` is allocated. A carried per-item cursor would
make a pass incremental rather than a full-window proof, weakening D32 clause
(g)'s conclusion. Neither trade is necessary to bound this read-only sweep.

Using the fixture one-day archive projection as a per-pass assumption:

| Full period | Passes/day | Read volume/day | Ownership re-sort heap/pass | Ownership spill on work dir/pass | Expected scheduling wait |
|---:|---:|---:|---:|---:|---:|
| 24 h | 1 | 3.356 GiB | 224.875 KiB | 1.791 GiB | 12 h |
| 1 h | 24 | 80.538 GiB | 224.875 KiB | 1.791 GiB | 30 min |
| 15 min | 96 | 322.154 GiB | 224.875 KiB | 1.791 GiB | 7 min 30 s |
| 5 min | 288 | 966.462 GiB | 224.875 KiB | 1.791 GiB | 5 min |

The spill column is per pass and does not multiply with cadence, because each
pass removes its private subdirectory when it ends; it is the window size,
not the period, that sets it. The work directory must hold it twice over
during consolidation. On the default tmpfs location it is resident memory.

The operational bend remains **daily, not faster**. Hourly to 15-minute full
scans multiply I/O by four to remove 22 min 30 s expected wait; 15 to five
minutes multiplies it by three to remove five minutes. #890 removes the
history-sized ownership allocation, not the retained-history read volume that
keeps the daily cadence appropriate.

Because the code reads retained history, 30-day full-fidelity retention still
projects 100.673 GiB read **per pass**
(docs/data/full-conservation-sweep-layout-2026-09-01.json). Its ownership-sort
heap stays 224.875 KiB; its spill grows with the window to 57,697,920,000
bytes (53.735 GiB) on the work directory per pass, needing under 107.47 GiB
of headroom there. At daily cadence that read volume is the daily cost, not a
one-time bootstrap; shortening cadence would multiply it.

The hot sweep has a different steady-state model. If H is current
balances/items/gaps read per pass, r receipt rate, and q receipt processing
cost, work is approximately H/C + q*r; only H/C grows with frequency because
the receipt tail is cursor-bounded. H, q, and production latency are unmeasured,
so this spike does not invent its knee.

## Evidence versus required production calibration

### What exists

* **Derived layout evidence, not production:** the test writes a real
  uncompressed 4,096-row Shape-C object, applies the prior 530 intents/s spike
  rate for 86,400 s, and uses compiled OwnershipEvent size plus the fixed
  4,096-event run and 16-way merge fan-in for the ownership heap bound, and
  the fixed 42-byte run record for the spill. The 45,792,000 / 3.35577 GiB /
  224.875 KiB heap / 1.791 GiB spill figures are recorded in the JSON; a
  separate 30-page synthetic pass exercises the multi-stage merge below its
  stated 256 KiB heap ceiling.
* **Harness TTD only:** 12 planted duplicate-owner cases at 150 ms over the
  small ledger above; neither production traffic nor a full-pass duration.
* **Instrumentation but no series:** hot passes log elapsed and row counts;
  full passes log elapsed, archive objects/bytes, receipts/effects, re-sort
  bound and findings (persistd.rs:2591-2609, :2643-2667). Reports retain cursor
  and scan denominators (audit.rs:664-676, conservation.rs:79-98). Nothing
  checked in shows these from production.

### What must be collected

Collect raw per-pass samples, tied to build, region/role and period:

1. scheduled/actual start, snapshot cutoff, finish/finding emission,
   failures/retries and missed ticks; plus commit-readable and first-finding
   timestamps for controlled injected findings. This supplies C, D, and TTD.
2. hot rows, receipts since cursor, receipt rate, archive objects/bytes/rows/
   effects, retention age and ownership transitions. This supplies H, r, A,
   and M at peak as well as average.
3. RSS/high-water memory, CPU, allocation, object-store latency/throughput, FDB
   range-read bytes/retries/conflicts, tailer lag, and simultaneous intent
   commit p50/p95/p99. D11 critical-operation target is FDB commit p99 below
   10 ms (docs/adr/0011-persistence.md:10-15).
4. known-magnitude controlled leaks in an isolated shadow/test arm,
   first-finding/magnitude error, false positives with scanned denominator, and
   a clean control. The roadmap proposes a seven-day clean control and calls
   detection targets proposed defaults (docs/11-roadmap.md:1145-1160).

There is **no production data** to populate these fields today: no full-pass
duration, live RSS, archive/object-store latency, FDB contention, production TTD
distribution, or false-positive rate. Do not substitute layout or harness
figures for any of them.

## Recommended cadence and decision rule

For the first production-shaped shadow window, run the current defaults:
**1 h hot-ledger incremental and 24 h full conservation**. This assumes:

* current-state contradictions can tolerate about 30 minutes expected detection,
  while global confirmation can tolerate about 12 hours;
* full duration fits the derived two-hour allowance;
* the worker has a reservation/isolation appropriate to the measured ownership
  re-sort heap bound, and `--audit-work-dir` points at a disk with room for
  twice the reported spill rather than the default tmpfs; and
* serving-path latency remains inside D11 budget while both read paths run.

If any assumption fails, revisit before C3 promotion review. A shorter full
period is not the automatic remedy: first identify whether pass duration,
archive growth, ownership-sort memory, object-store throughput, FDB contention,
or a stricter detection target is responsible. The answer may be archive layout
or worker isolation rather than a timer change. This is why the recommendation is
proposal-only and does not amend an ADR.

## Provenance

Verified sequence: #615; Shape C (#832); enriched receipt/archive landing #841;
daily full sweep #842; external merge sort #890; spill accounting and
emit-before-spill #912. Governing records are D11/D20 for history
(docs/adr/0011-persistence.md:10-21, docs/adr/0020-journal-retention.md:221-235)
and D32 clause (g) for liveness (docs/adr/0032-enforcement-ramp.md:570-605).
