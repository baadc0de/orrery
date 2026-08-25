# gates/p2-journal-bench

A **store-level** comparison for the P2 journal's durability barrier
([docs/08-persistence.md](../../docs/08-persistence.md) §4.8).

[§4.7](../../docs/08-persistence.md) traced P2's `journal_commit_ms` tail to
fjall 3.1.9's write backpressure — `Batch::commit` calls
`local_backpressure()`, which sleeps in 100 ms steps while four or more sealed
memtables are queued — and showed the stall survives every storage change,
including running the journal on tmpfs. That leaves one question open, and it
is a question about a *second* store: is this pathology **fjall's**, or **an
LSM's**?

This tool answers it by driving two stores through the same write pattern the
journal produces, and reporting the same statistics for both.

## What it is not

It is **not** a second `Journal`. `orrery_persistd::journal::Journal` is
concrete on fjall, and reimplementing it against RocksDB would put a thousand
lines of untested code between the question and the answer. What the journal
actually asks of a store is narrow:

> batch N keyed records into two column families, commit the batch with one
> WAL fsync, and let the caller time that call.

That is the whole `Store` trait here, and both arms implement exactly it.

It is also **not an adoption proposal.** `rocksdb` is behind a non-default
feature in a standalone workspace precisely so it stays out of
`orrery_persistd`'s dependency graph. Swapping the journal's backing store is a
D14/ADR decision; this crate exists to put numbers in front of that decision,
not to pre-empt it.

## What is held equal

Both arms see the same arrival process (the P2 gate's bulk shape: ~250 bursts/s
of ~71 records, ~17.7 k records/s), the same coalescing window and caps
(persistd's 200 µs, 8192 records, 1 MiB), the same **monotonic big-endian keys**
— what the journal's LSN ordering produces, and the ordering an LSM's
compaction is most sensitive to — the same value sizes, and the same two column
families. Both are asked for a WAL write plus fsync per batch: fjall's
`PersistMode::SyncData` and RocksDB's `WriteOptions::set_sync(true)`.

**Neither arm is tuned**, and the tool says so in its own output. This measures
the stall behaviour of stock configurations under the journal's write pattern,
not the best either store can be made to do.

## Running it

```sh
cargo build --release                              # fjall arm only
cargo build --release --features rocksdb-store     # adds RocksDB (compiles C++; minutes)

./target/release/p2-journal-bench --store fjall   --dir /mnt/nvme/bench-f --seconds 90
./target/release/p2-journal-bench --store rocksdb --dir /mnt/nvme/bench-r --seconds 90
```

`--seconds` matters more than it looks. At the default rate the run writes
~2.7 MB/s, and fjall's default memtable is 64 MiB, so a 30 s run may never
rotate and a 90 s run rotates two or three times. **A short run can report zero
stalls from a store that stalls**, which is the same trap
[§4.7](../../docs/08-persistence.md)'s 256 MiB sweep point fell into.

| Flag | Default | Meaning |
|---|---|---|
| `--store` | `fjall` | `fjall` or `rocksdb` (needs the matching feature) |
| `--dir` | `bench-data` | Where the store lives — put it on the device under test |
| `--seconds` | 60 | Run duration |
| `--bursts` | 250 | Bursts per second |
| `--burst-size` | 71 | Records per burst |
| `--value-bytes` | 152 | Value size; the gate and rig both measure ~140–170 B/record |
| `--batch-window-us` | 200 | Group-commit window — persistd's production value |
| `--json` | off | One JSON object instead of the human report |
| `--no-sync` | off | **Control only.** Drops the fsync |

## `--no-sync` is the control that makes the rest trustworthy

A comparison of two "durable" stores is worthless if one of them was quietly
not syncing. Run each arm both ways on a real device: the barrier must get
dramatically cheaper without the fsync. Measured on a consumer QLC NVMe, 30 s
each:

| arm | mean barrier | on disk |
|---|---|---|
| fjall, fsync per batch | 1591.5 µs | 158.6 MB |
| fjall, buffered | 84.9 µs | 158.6 MB |
| rocksdb, fsync per batch | 1630.4 µs | 152.4 MB |
| rocksdb, buffered | 54.0 µs | 152.4 MB |

Both stores really are syncing, at the same cost, having written comparable
bytes. The tool prints on-disk size for the same reason — an arm that wrote
less cannot quietly look faster for it.

## Reading the output

```
commit_ms p50=… p90=… p99=… p99.9=… p99.99=… max=…
barrier: mean … us/flush | slow (>= 20 ms) N of M | worst … ms carrying … KB / … records
         against … KB for an ordinary flush
```

The last clause is the discriminator §4.7 introduced, and it is the one to read
first: **a slow barrier carrying an ordinary batch is the store's own doing**,
not the volume being persisted. The 20 ms slow-barrier threshold matches
`orrery_persistd::journal::SLOW_SYNC_THRESHOLD_US`, so a count here means the
same thing as a count there.
