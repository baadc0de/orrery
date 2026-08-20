# ADR-0019: Default to the indexed wal-db journal

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D19

This decision is normative. See the [ADR index](../DECISIONS.md) for
precedence, scope, and the complete decision set.

**Supersedes:** the local journal-engine choice in
[D11](0011-persistence.md) and the journal-engine entries in
[D14](0014-pinned-versions.md). The remainder of both decisions stays
accepted.

## Context

D11 deliberately left the per-node segmented journal open between Fjall 3.x
and raw segments. The implementation initially defaulted to Fjall 3.1.9. Full
P2 measurements then isolated a long commit tail in Fjall's write
backpressure: its commit path sleeps in 100 ms steps while sealed memtables
queue. The same stalls reproduced on tmpfs, while RocksDB and wal-db did not,
so the tail was neither the block device nor a general property of LSMs.

The store-level wal-db result was only a lower bound because it had no Orrery
index. Phase 4 therefore implemented the complete `Journal` contract on
wal-db: versioned record envelopes, logical-LSN and originated-record indexes,
chain state and provenance, adoption markers, recovery-time index rebuild,
torn-tail recovery, and the existing adaptive group-commit boundary. The
indexed implementation then ran through the full P2 kill-9 gate in five
alternating pairs on a qualified `c4d-standard-32-lssd` local NVMe:

| backend | gate passes | `journal_commit_ms` p99 | commits > 2 ms | commits > 15 ms |
|---|---:|---:|---:|---:|
| Fjall 3.1.9 | 0/5 | 40 ms median [15, 75] | 3.856% median | 1.580% median |
| indexed wal-db | **5/5** | **1 ms in every run** | **0.009% median** | **0.000%** |

All ten runs passed recovery verification with 540,640–541,256 durable
acknowledgements per run, zero leases lost, zero diff nacks, and zero duplicate
durable acknowledgements. The evidence and mutation-checked reducer are in
[`docs/spikes/journal-raw-waldb.md`](../spikes/journal-raw-waldb.md) §9.

These are promising performance and correctness results, not proof of a
mature dependency. wal-db 1.0.0 was newly published and has little field
history. The project owner accepts that maturity risk because the indexed
implementation clears the actual gate while the incumbent repeatedly does
not, and because the risk can be bounded without keeping the failing backend
as the default.

## Decision

- `orrery_persistd` defaults to `journal-raw` plus `chain-grpc`.
  `journal-raw` uses wal-db's segmented, CRC32C-framed durable log and rebuilds
  Orrery's ordered in-memory indexes from versioned envelopes at open.
- Pin `wal-db` exactly to **1.0.0**. A version change requires re-running the
  recovery suite and the full P2 gate; an upstream semver claim alone is not a
  durability qualification.
- Retain `journal-fjall` as an explicit, mutually exclusive fallback feature.
  It is not the shipping default and does not satisfy the measured P2 latency
  criterion on the qualified host.
- Do not reinterpret an existing Fjall directory as an empty raw journal. A
  raw open requires either its `raw-wal/` backend marker or an empty journal
  directory. Existing Fjall data must be drained and checkpointed, or migrated
  explicitly, before changing features.
- Keep the indexed Phase 4 evidence and its mutation-checked report in the
  per-commit gates. Performance regression is judged by the full kill-9 gate,
  not by a microbenchmark alone.

## Consequences

- The default build now meets all four D16 P2 latency budgets in the measured
  five-run gate set while preserving the same crash/recovery and chain proofs.
- Opening the journal rebuilds indexes in one forward WAL scan. Startup work
  and index memory are therefore linear in retained journal metadata and
  records. Segment retention and future persisted index footers must be
  measured before treating arbitrarily old journals as free to open.
- The on-disk format has two versioned layers: wal-db's pinned 1.0 format and
  Orrery's `RawEnvelope::V1`. Either layer changing requires an explicit
  compatibility review and recovery fixture.
- Fjall remains available for rollback and comparison, so CI must compile and
  test both mutually exclusive backend feature sets even though only raw is
  the default.
- The new default removes Fjall's memtable/compaction machinery and its known
  shutdown workaround from the shipping path. It adds dependence on a young
  crate whose audit and field history remain a tracked risk.

## Alternatives considered

- **Keep Fjall as default and tune memtables:** rejected. The sweep trades
  stall frequency for severity and the 100 ms-step backpressure returns in
  longer runs.
- **Use RocksDB:** it cleared p99 in the store comparison, but adds a large C++
  dependency and still recorded NVMe stalls. It remains the conservative
  fallback if wal-db's maturity risk materializes.
- **Hand-roll framing, recovery, and barriers:** rejected for now. wal-db
  already supplies the narrow WAL primitive, CRC framing, segmented recovery,
  and platform-specific durability call the journal needs; Orrery owns only
  its domain indexes and envelope.
- **Remove Fjall immediately:** rejected. Keeping an explicit fallback is cheap
  insurance while wal-db accumulates audit and production history.
