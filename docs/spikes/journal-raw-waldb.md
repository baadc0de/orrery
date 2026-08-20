# Spike brief: a purpose-built `journal-raw`, on `wal-db`

**Status: non-normative working document.** This is not an ADR and does not
decide anything. It is the brief for an investigation whose *output* may
justify an ADR — swapping the journal's backing store is a D14 dependency
decision and a D11 persistence decision, and neither is taken here. Accepted
ADRs in [`docs/adr/`](../adr/) remain normative over every word below.

**Date:** 2026-08-20. **Reads from:**
[08-persistence.md](../08-persistence.md) §4.3–§4.8.

---

## 1. The question

[§4.7](../08-persistence.md) established that P2's `journal_commit_ms` tail is
fjall 3.1.9's write backpressure — `Batch::commit` calls
`local_backpressure()`, which sleeps in 100 ms steps while four or more sealed
memtables are queued. [§4.8](../08-persistence.md) established that this is
**fjall's and not an LSM's**: under an identical write pattern on the same
box, RocksDB and a pure WAL both stall *zero* times on tmpfs where fjall stalls
59, and both hold p99 inside D16's 2 ms budget where fjall reads 72 ms.

§4.8 could not answer the next question, and said so. Its wal-db arm keeps **no
keyed index**, so it does strictly less work than the journal does — 89.5 MB on
disk against fjall's 158.6 MB for the same records. Those numbers are a *lower
bound on a WAL-shaped store*, not a result about a journal.

**This spike answers: what does a purpose-built Orrery journal on a WAL
substrate actually cost, measured against fjall through the same gate?**

The hypothesis worth falsifying is that it wins, and for a structural reason
rather than a tuning one: the journal is **append-only and never overwrites a
key**, so every LSM mechanism that exists to reconcile overwrites — memtable
rotation, sealed-memtable queueing, compaction, and the backpressure that
guards them — is machinery it pays for and cannot use.

**If the spike does not beat fjall through `p2-kill9-gate.sh`, it has
succeeded**: that is a real answer, and it is cheaper to learn here than after
an ADR.

---

## 2. Background, in the order it was learned

Each of these was measured, and each replaced the one before it. Read them as a
sequence of *retractions*, because that is what they are.

| § | Claim | Status |
|---|---|---|
| 4.3 | The tail is the device's `fdatasync` distribution; the fix is storage with a power-loss-protected write cache (p99 ≤ 1.5 ms at ≥ 400 barriers/s) | **Retracted by §4.4** |
| 4.4 | Measured on a PLP NVMe (barrier p99 **0.089 ms**, 40× better): the gated p99 **did not move**, 15 ms in 11 of 16 runs. Blamed the harness's own buffered writeback, labelled circumstantial | **Mechanism retracted by §4.6** |
| 4.5 | Added `P2_GATE_DATA_DIR`; the reference box cannot test it — its bare barrier already stalls at 78 ms, so signal and noise are the same size | Standing |
| 4.6 | Removed every co-tenant (evidence → tmpfs, then FoundationDB → tmpfs) on two filesystems: **34 of 36 runs still stalled**. XFS is 10× more writeback-resistant at the device and stalls anyway, which also removes jbd2 | Standing |
| 4.7 | The worst barrier carries an *ordinary* batch (0.94–1.53× median 1.13×), the stall reproduces on **tmpfs**, and the mechanism is fjall's 100 ms `sleep`. Varying `max_memtable_size` trades stall frequency for severity and never removes it | Standing |
| 4.8 | It is fjall's, not an LSM's. RocksDB and wal-db: 0 tmpfs stalls, p99 inside budget | Standing |

**The one number to carry into this spike:** 92.5–96.1 % of journal commits
already land at or below 0.5 ms (§4.6). The body of the distribution is
healthy. This is a tail problem caused by a handful of multi-hundred-millisecond
events per run, and a fix is anything that removes *those*.

---

## 3. Where to land it

**The seam already exists. Use it; do not build beside it.**

`crates/orrery_persistd/Cargo.toml` declares, with the comment *"Only one may be
set"*:

```toml
journal-fjall = ["dep:fjall"]   # default
journal-raw = []                # planned, empty
```

and `crates/orrery_persistd/src/journal/mod.rs` has one line that is the switch:

```rust
pub use fjall::Journal;
```

If the spike lands a `Journal` under `journal-raw` with the same surface, then
[`crates/orrery_persistd/tests/journal_arrival_rate.rs`](../../crates/orrery_persistd/tests/journal_arrival_rate.rs)
and [`scripts/p2-kill9-gate.sh`](../../scripts/p2-kill9-gate.sh) both work
**unmodified**, and the comparison is apples-to-apples by construction rather
than by argument. A new API instead produces another
[`p2-journal-bench`](../../p2-journal-bench/README.md) — useful, but not a
verdict on the journal.

`wal-db` stays *behind* that seam. If the crate proves unsound (see §7), the
index layer must survive and only the substrate be replaceable.

---

## 4. The contract

### 4.1 Public surface — 11 methods

`open`, `append`, `append_replicated`, `committed`, `flush_count`,
`commit_metrics`, `subscribe`, `scan_from`, `close`, `is_closed`,
`adopt_chain_history`.

### 4.2 Crate-visible surface — 8 more, and this is where correctness lives

`scan_originated_from`, `scan_source_from`, `chain_grpc_records`,
`chain_epoch_sibling`, `chain_state_epoch_sibling`, `chain_grpc_record`,
`chain_grpc_state`, `set_chain_grpc_state`.

### 4.3 What sits behind them

**8 keyspaces** — `records`, `originated_records`, `journal_meta`, `segments`,
`chain_records`, `chain_state`, `chain_adoptions`, `adopted_chain_records` —
and **26 point/range read sites**. `wal-db` supplies `iter_from` and nothing
else; every one of those is the spike's to build. That gap *is* the work, and
it is also why §4.8's numbers are a lower bound.

### 4.4 Invariants that are not in the signatures

1. **`AppendHandle` resolves only after the group fsync.** The ack *is* the
   durability contract (§2.1). A backend that resolves on `append` rather than
   on the barrier will look spectacular and be wrong, and no throughput
   benchmark will catch it.
2. **LSN recovery on reopen.** The next LSN comes from the last stored record,
   or a restarted journal collides and replay starts in the wrong place.
3. **`chain_state` is epoch fencing.** It is what the kill-9 gate's zombie and
   epoch-fork proofs exercise. This is the part most likely to be quietly
   broken while every latency number looks excellent.
4. **macOS needs `F_FULLFSYNC`**, not plain `fsync`, which does not reach
   stable storage there. The determinism matrix includes `aarch64-macos`.
   `wal-db` documents doing this correctly and it is one reason it is the
   better of the two candidate crates.
5. **`records` is written under `strict_authority`.** Nothing in the journal
   may admit an unfenced write; the gate's 10 000-lease phase depends on it.

---

## 5. Gotchas, each of which cost real time

These are not hypotheticals. Every one of them was hit while producing §4.4–§4.8.

**A short run is a false pass, and this is the big one.** At 90 s fjall's tmpfs
p99 reads **0.535 ms — inside D16's budget** — because 21 stalls in ~22 000
flushes do not reach the 1 % mark. At 300 s the *identical configuration* reads
**64.7 ms**. §4.7 fell into the same trap from the other side: a 256 MiB
memtable showed **zero** stalls at 60 s and 13 stalls with p99.9 661 ms at
180 s. **Benchmark at ≥ 300 s, or state why not.** At the default rate that is
~830 MB written; anything under ~250 MB may not rotate at all.

**Build the rig with the same feature set as the binaries under test.**
`cargo test --release -p orrery_persistd --test journal_arrival_rate` *without*
`--features fdb` un-unifies the release profile and silently rebuilds
`persistd` **without** FDB. The gate then dies at startup with `persistd was
compiled without the fdb feature`, which reads like a code defect and is not.

**`p2-kill9-gate.sh` consumes its cluster.** The primary asserts
`--chain-epoch 1` against a fence that only ever moves forward, so every run
needs `fdbcli --exec 'writemode on; clearrange "" \xff'` first. Its pre-flight
refuses a cluster that already carries an `actor/` row.

**`artifact.json` is written only on a pass.** Extract evidence regardless of
exit status — keeping only the runs that passed is the one selection bias this
work cannot afford. `scripts/p2-baseline-extract.py` reduces a ~1 GB gate
directory to a few hundred bytes so an n-run baseline fits on disk.

**The p99 is a knife-edge here, so prefer tail *mass*.** The shared bucket
lattice is coarse above 2 ms (15, 20, 30, 40, 50, 75, 100…), and in §4.6 the
mass above 15 ms was ~0.8 % in both filesystems — the 1 % point sits almost
exactly on a bucket boundary, so the gated statistic swings a whole bucket on
very little. Compare `pct_over_2ms` / `pct_over_15ms`, not just p99.

**Measure the worst barrier's *shape*, not only its cost.** §4.7 added
`sync_data_us_max_bytes` / `_records` and `slow_syncs` for exactly this: a slow
barrier carrying an ordinary batch is the store's own doing, one carrying
megabytes is volume. A `journal-raw` backend must populate the same
`JournalStageSnapshot` fields or the comparison loses its discriminator.

**tmpfs is the decisive control.** It removes storage entirely. If a backend
stalls there, no storage change will save it; if it does not, its stalls are
device-coupled. Run every latency claim on tmpfs *and* a real device.

**Do not trust a store that merely looks fast.** Two controls, both cheap:
`--no-sync` (the barrier must collapse without the fsync — measured 250.8 → 58.4 µs
for fjall, 162.3 → 23.1 µs for wal-db) and on-disk bytes (an arm that wrote
less cannot be compared on latency). wal-db's 89.5 MB against fjall's 158.6 MB
is the no-index gap, quantified.

**Do not re-chase the harness's writeback.** §4.4 blamed it, §4.6 disproved it
by putting the evidence on tmpfs and the stall staying. `P2_GATE_DATA_DIR`
exists and is worth keeping; it is not a fix.

**Do not benchmark on the reference box and expect an answer.** Its bare
barrier stalls at 78 ms unloaded (§4.5), so signal and noise are the same size.
That experiment is not underpowered, it is unrunnable. Use hardware whose
`fio` job `A` tops out under 1 ms.

**Operational trivia that wasted time:** `fdbserver` lives in `/usr/sbin`,
which is not on a normal user's `PATH`, so `nohup fdbserver …` fails silently.
`set -u` plus a multi-assignment `local a=$1 b="$a"` is an unbound-variable
error in bash.

---

## 6. Phased acceptance criteria

Each phase has a **gate**. Do not start the next phase until the current gate
holds. Phases 0–2 need no FoundationDB cluster and no cloud hardware.

### Phase 0 — the seam, with no behaviour change

Introduce the backend switch so `journal-raw` compiles as a stub while the
default path stays byte-identical.

**Gate.** `./scripts/check.sh` passes all four lanes with default features;
`cargo check -p orrery_persistd --no-default-features --features journal-raw,chain-grpc`
compiles; **no published number in `docs/08` changes meaning**, and the diff
touches no `docs/data/` file.

### Phase 1 — append and replay

`open`, `append`, `committed`, `scan_from`, `close`, `is_closed`, plus LSN
recovery on reopen.

**Gate.** The open-loop rig runs under `journal-raw` and prints a latency
report. Reopening a journal returns **every acked record, in LSN order**, with
CRC verified. A property test covers a torn tail: truncate the last record's
bytes, reopen, and the journal recovers to the last intact record rather than
erroring or silently returning a short prefix.

### Phase 2 — the durability contract

`AppendHandle` resolves only after the barrier; group commit honours
`GroupCommitConfig` (`batch_window`, `batch_max_records`, `batch_max_bytes`);
`commit_metrics` populates `JournalStageSnapshot` **including** §4.7's
`sync_data_us_max_bytes` / `_records` / `slow_syncs` fields.

**Gate.** A 300 s rig run on tmpfs *and* on an NVMe reports the barrier-shape
fields. `kill -9` mid-run loses **no acked record** on reopen. The `--no-sync`
control shows the barrier collapsing, and on-disk bytes are recorded. **This is
the first point at which a latency number means anything** — quote it with its
duration and its medium or not at all.

### Phase 3 — indexes and chain replication

`originated_records`, `chain_records`, `chain_state`, `chain_adoptions`;
`append_replicated`, `scan_originated_from`, `scan_source_from`, the four
`chain_grpc_*` methods and epoch fencing.

**Gate.** `scripts/p2-kill9-gate.sh` **reaches the latency verdict** under
`journal-raw` — which means every durability proof before it passed: recovery
verification true, durable acks in the 539 000–542 000 family, zero leases
lost, zero diff nacks, the zombie primary refused fenced admission, and a
bumped chain epoch refused rather than forked. The latency verdict itself may
be red; that is Phase 4's question, not this one.

### Phase 4 — the apples-to-apples comparison

Both backends through the same gate on the same box.

**Gate.** ≥ 5 interleaved pairs, arm order alternating per repeat, on hardware
whose `fio` job `A` maximum is under 1 ms, at the full gate duration. A
`scripts/p2-journal-raw-report.py` re-derives every quoted number from a
versioned `docs/data/*.jsonl`, with a `--self-test` wired into `check.sh`'s
`gates` lane, **every clause of which has been mutation-checked** — break the
guarded fact, confirm the self-test fails, restore. Report tail *mass* beside
p99. State on-disk bytes for both arms. **The verdict is whatever it is.**

### Phase 5 — the decision, if it is earned

Only if Phase 4 shows a material win: an ADR proposing the backing-store change,
naming D11 and D14, with Phase 4's data as its evidence and this document's §7
as its risk section. Until that ADR is accepted, `journal-fjall` stays the
default.

---

## 7. Risk, and what would stop this

**`wal-db` 1.0.0 is the right shape and an unproven dependency.** The shape
argument is strong — `append` (page cache) / `sync` (coalescing barrier) is
`journal::group_commit`'s contract exactly, plus `fdatasync`/`F_FULLFSYNC`,
CRC32C per record, torn-tail truncation, and a segmented log with
`truncate_before`, which is `journal-raw` as §4 already scoped it. The maturity
argument is equally strong in the other direction: **235 downloads, and 0.5.0
through 1.0.0 published inside about eight hours on 2026-06-05**, with the only
dependents being the same author's other crates. "On-disk format frozen for
1.x" is a claim made on day one, not a track record.

Its test posture (loom, fuzz-hardened recovery, property tests for torn writes)
is better than most crates its size. That is not the same as having survived
other people's crashes, and this is the **system of record's durability
substrate**.

**`lsm-db` is not the candidate**, twice over: it is an LSM — the class this
work exists to escape — and it is built *on* `wal-db`, so it adds the layer the
journal does not need over the one it does. 169 downloads across nine versions
in five days.

**Mitigations, in order of preference:** keep `wal-db` behind the `Journal`
seam so the index layer survives a substrate swap; consider vendoring and
auditing it (this repository already vendors three crates under `vendor/`);
treat "wal-db versus a hand-rolled segment file" as a *later measurement*
rather than a rewrite. Phase 4's harness makes that a one-arm addition.

**Stop conditions.** Abandon or re-scope if: Phase 1's torn-tail property test
cannot be made to pass against the crate; Phase 3 cannot hold epoch fencing
without reimplementing an index the crate actively fights; or Phase 4 shows no
material win, in which case the finding is that fjall's backpressure should be
fixed upstream and the journal left alone.

---

## 8. What already exists, so nothing is rebuilt

| Thing | Where | Use it for |
|---|---|---|
| Open-loop journal rig | `crates/orrery_persistd/tests/journal_arrival_rate.rs` | Phases 1–2; no cluster, no network |
| Full crash/recovery gate | `scripts/p2-kill9-gate.sh` | Phases 3–4 |
| Store-level bench, 3 arms | `p2-journal-bench/` | Substrate questions; `--no-sync` and on-disk controls |
| Evidence reducer | `scripts/p2-baseline-extract.py` | ~1 GB gate dir → a few hundred bytes |
| Report + self-test pattern | `scripts/p2-barrier-shape-report.py` | The shape Phase 4's report must follow |
| Journal placement knob | `P2_GATE_DATA_DIR` | Separating evidence from journal; **not** a fix |
| Memtable knob (fjall arm only) | `ORRERY_JOURNAL_MEMTABLE_BYTES` | Moving fjall's rotation cadence; unset = 64 MiB default |

---

## 9. Phase 4 measurement — 2026-08-20

Phase 4 ran on the same ephemeral GCP shape used for §4.4–§4.8:
`c4d-standard-32-lssd` in `us-central1-b`, with the journal on a write-through
local NVMe mounted ext4 `noatime`, gate evidence on tmpfs, and FoundationDB
7.3.77 on the NVMe. The exact two-job `fio` qualification sustained **940.0
barriers/s** in aggregate, with **0.185 ms p99** and **0.509 ms maximum**, so
the host cleared this brief's `< 1 ms` maximum requirement.

Five full-duration pairs then ran through `scripts/p2-kill9-gate.sh`, with arm
order alternating per pair. These are the headline medians; brackets are the
five-run range for p99 and the worst observed maximum for the sync column:

| backend | gate passes | `journal_commit_ms` p99 | commits > 2 ms | commits > 15 ms | sync maximum | median on-disk bytes |
|---|---:|---:|---:|---:|---:|---:|
| fjall | 0 / 5 | **40 ms** [15, 75] | **3.856 %** | **1.580 %** | 97.488 ms [191.969] | 896,138,100 |
| indexed `journal-raw` | **5 / 5** | **1 ms** [1, 1] | **0.009 %** | **0.000 %** | 2.290 ms [2.343] | 781,193,686 |

All ten runs passed recovery verification. Each produced 540,640–541,256
durable acknowledgements, with zero leases lost, zero diff nacks and zero
duplicate durable acknowledgements. The p99 values are bounded-histogram
bucket upper bounds; the tail percentages come from the same versioned
histograms rather than from interpolation.

Every number above is derived by
`scripts/p2-journal-raw-report.py` from
`docs/data/p2-journal-raw-2026-08-20.jsonl` and
`docs/data/p2-journal-raw-device-2026-08-20.json`. Its `--self-test` is wired
into `scripts/check.sh` and mutation-checks every guarded class of evidence.
This section records a measurement only: it makes no dependency or
default-backend decision.
