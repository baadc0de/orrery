# Spike: pricing a durable economic-effect record for the archive (#832)

**Status:** owner decision spike · **Scope:** price three shapes; recommend one
that the owner may reject · **Blocked:** #615 daily conservation sweep

This spike prices the decision in #832: how a durable economic-effect record
reaches a path the archive can see. It does not implement anything. It reads the
code the issue cites, verifies every structural claim, and attaches measured
numbers where they can be produced safely.

## What was verified

- `docs/adr/0011-persistence.md:10` splits write classes: bulk diffs go to the
  journal, economic intents go **directly to FoundationDB**.
- `crates/orrery_persistd/src/gateway.rs:8842` is the only production
  `JournalRecord` producer; it consumes `DiffUplink` component payloads.
- `crates/orrery_persistd/src/archive/object.rs:61-62` stores only journal
  records (`ARCHIVE_SCHEMA_NAME = "orrery_journal_record"`).
- `crates/orrery_persistd/src/intent/fdb.rs:1363-1407` writes balance and
  ownership effects inside the FDB transaction; the journal is not touched.
- `crates/orrery_persistd/src/intent/mod.rs:2202-2211` shows `receipt()` returns
  `None` for a pure credit; receipts exist only for item transfers today.
- `crates/orrery_persistd/src/keyspace.rs:1816` defines `ReceiptRow` as
  `{ intent_id, parties, ops }` — op ids, not deltas, item ids, or ownership
  transitions.

Therefore the effects #615 must reconcile are not in the journal and not in the
archive. `docs/08-persistence.md` §11.5 already records this gap (post-#833).

## The atomicity question that decides everything

**Is the record written inside the same FDB transaction as the effect?**

Anything less leaves a window in which the ledger moved and the record did not.
#615's sweep exists to detect exactly that discrepancy; a sweep that can be
defeated by a crash in that window is not a sweep. For each shape below, the
atomicity verdict is stated first and treated as decisive.

## Shape A: through the journal

Make the effect a `RecordKind` (e.g. `EconomicEffect`) and write a
`JournalRecord` to the local journal inside the intent path.

### What it costs the intent path

The intent path is FDB-only today. Adding a journal append means:

1. A synchronous append to the local journal and a wait for the group fsync.
2. A new `JournalRecord` per economic mutation, competing with bulk diffs for
   the single fsync stream.
3. The journal's LSN space now carries intent-derived ordering.

Measured journal frame sizes:

| record | frame size |
|---|---:|
| 128 B component diff (typical bulk record) | 181 B |
| economic-effect record carrying one transfer + two balance deltas | 141 B |

The effect record is slightly smaller than a typical diff because its payload is
postcard-encoded structured data rather than a component bag. A pure credit
carrying one balance delta would be smaller still (~110 B frame).

At the P2 gate's arrival rate of **~18,000 records/s** and **~26 MB/s**
(`docs/adr/0020-journal-retention.md:71`), adding economic records is a
percentage addition equal to the ratio of economic mutations to bulk diffs. The
P2 mix (`docs/data/p2-barrier-shape-2026-08-19.jsonl`) commits ~16,000 intents
in 30 s against ~540,000 diffs — about **3 %** of the record stream. The added
journal bandwidth is therefore roughly **0.8 MB/s** at P2 load if every intent
produces one effect record.

The latency cost is the journal fsync path. The P2 gate's `journal_commit_ms`
p99 is 8–50 ms depending on regime; the mean fsync cost per flush is ~180–310 µs
in the fast regime. An economic mutation would pay that same fsync tax. D11's
premise — "don't make the game wait for a database" — is preserved for the FDB
commit (still RPO 0), but the intent path now also waits for the local journal.

### What it guarantees

- The archive sees the effect stream for free because the tailer already
  consumes the journal.
- Total order between economic effects and bulk state is explicit: both share
  the journal LSN.
- Crash recovery is unified with the existing journal replay path.

### Atomicity

**Not atomic with the FDB transaction.** The journal append and the FDB commit
are independent durability barriers. Four failure modes:

1. Journal append succeeds, FDB commit succeeds: normal case, both records exist.
2. Journal append succeeds, FDB commit fails (e.g. conflict): the archive later
   sees an effect for a mutation that never happened. #615 reports a phantom
   delta.
3. Journal append fails, FDB commit succeeds: the ledger moved but the archive
   has no record of it. #615's sweep is defeated by a crash in this window.
4. Both fail: no effect, no record — consistent but the client saw a rejection.

Mode 3 is the decisive one. Because the journal append is not inside the FDB
transaction, a crash between the FDB commit and the journal fsync leaves the
archive blind to a committed economic effect. The sweep cannot reconcile what it
cannot see. This shape **cannot offer atomicity** without making the journal
append itself part of the FDB transaction, which defeats D11's split.

### Retention semantics

Shape A couples ledger durability to the journal's retention semantics, including
the clamp. `crates/orrery_persistd/src/journal/raw.rs:542-623`: a registered
archive claim clamps the retention floor to the tailer's verified watermark. If
the archive is unreachable, the journal grows at the arrival rate (~26 MB/s at
P2) until the tailer recovers.

Adding economic records to the journal makes that clamp apply to ledger history
too. An operator who expected FDB to hold the ledger indefinitely while the
journal released bulk state would discover that economic records cannot release
until the archive confirms them. **That coupling is real and should be accepted
only deliberately.** If the owner wants ledger history to survive independent of
object-storage availability, this shape is wrong.

## Shape B: a second archive-fed stream

Keep the journal bulk-only. Add a separate durable log (e.g. an FDB-backed
ledger-effect stream or a second local WAL) that the tailer also consumes, and
archive both sources into the same object store or into separate object families.

### What it costs the intent path

The record is written inside the FDB transaction (see atomicity below). The
incremental cost is therefore one extra `trx.set` per economic mutation.

Measured FDB write-set sizes for one item transfer today:

| write | key bytes | value bytes |
|---|---:|---:|
| `ledger/item/{uid}` | 10 | 17 |
| two `ledger/bal/{account}/{asset}` atomic adds | 2 × 18 | 2 × 16 |
| `ledger/receipt/{versionstamp}` | 16 | 39 |
| **total today** | — | **150** |

A shape-B effect record adds roughly **91 bytes** to that transaction (16-byte
hypothetical key prefix + 89-byte payload for one transfer + two balance deltas).
A pure credit adds less (~75 bytes). This is a **~60 %** increase in written
value bytes per trade, but the absolute size remains tiny next to FDB's 10 MB
transaction limit.

Current intent-path FDB timing from `docs/data/p2-intent-fence-2026-08-19.jsonl`
(post-fence optimization, fast regime, n≈15,800):

| stage | mean |
|---|---:|
| `exec_us` | ~2.47 ms |
| `commit_us` | ~1.99 ms |
| `grv_us` | ~0.15 ms |
| `idem_read_us` | ~0.08 ms |
| `fence_us` | ~0.24 ms |

The added 91-byte write is on the commit path, not the read path, so it does not
affect GRV, idempotency read, or fence. Its latency impact is bounded by the
marginal tlog/fsync cost of ~90 extra bytes per commit. That is below the
measurement noise floor of the existing instruments and cannot be isolated
without modifying the transaction. It is safe to say it is small relative to the
2 ms mean commit, but not zero.

### What the tailer needs to consume two sources

The current tailer (`crates/orrery_persistd/src/archive/tailer.rs`):

- Derives sealed segments from `Lsn::segment`.
- Keys objects by `(node_id, segment_seq)`.
- Buffers one segment, sorts by `(grid, cell, lsn)`, and writes one object.
- Advances its watermark only after the `jarchive/{node_id}/{segment_seq}` row
  commits.

A second stream breaks every one of those assumptions:

1. **Segmentation.** The effect stream is not LSN-based. It needs its own
   sequence space or batching key.
2. **Object keys.** `object_key(node_id, segment_seq)` no longer uniquely names
   the source. The key must include a stream discriminator (e.g.
   `jarchive/{node_id}/{stream}/{seq}`).
3. **Sort order.** Effect records have no natural `(grid, cell)`. A trade
   involves accounts, not spatial cells. If archived in the same Parquet schema,
   the `(grid, cell, lsn)` sort order would put effect records under a synthetic
   or null cell, scattering them. A separate object family with its own schema is
   cleaner.
4. **Watermark.** The journal watermark and the effect-stream watermark advance
   independently.

### Shared vs separate watermark in `retention_floor`

Two choices:

- **One minimum watermark.** The journal release is clamped by
  `min(journal_watermark, effect_watermark)`. A lagging effect stream blocks bulk
  journal release. This is simple but couples bulk retention to the effect
  tailer's health.
- **Independent watermarks.** Each stream has its own `jarchive/` family and the
  journal clamp uses only the journal watermark. The effect stream's retention is
  managed separately. This matches D11's split but doubles the archive metadata
  surface and the operator's alarms.

### Atomicity

**Atomic if and only if the effect record is written in the same FDB transaction
as the ledger mutation.** Shape B is the natural place to do that: the record is
not a journal append; it is a durable row beside the intent outcome. The
anti-dupe invariant, the balance deltas, the item ownership row, and the effect
record all commit or abort together under FDB's serializable transaction.

If the second stream is instead produced asynchronously (e.g. by reading the
`ledger/receipt/` family after commit), it reintroduces the same window as Shape
A and is not atomic. The decisive requirement is the in-txn write.

## Shape C: enrich `ReceiptRow` and archive the receipts

Make `ReceiptRow` carry deltas, item ids, and ownership transitions; write a
receipt for every economic mutation, including pure credits; and archive the
`ledger/receipt/` family.

### What it costs the intent path

Measured receipt sizes:

| shape | bytes |
|---|---:|
| current receipt for one item transfer | 39 B |
| enriched receipt (transfer + two balance deltas) | 110 B |
| current receipt for one pure credit (does not exist today) | 30 B |
| enriched receipt for one pure credit | 52 B |

For a trade, the enriched receipt replaces the 39-byte value with a 110-byte
value: **+71 bytes per committed trade**. For a pure credit, the intent path
gains a new receipt write of **52 bytes**.

The receipt is already written inside the FDB transaction
(`crates/orrery_persistd/src/intent/fdb.rs:1397-1406`). Enlarging it and writing
it for credits keeps atomicity trivially: the receipt commits exactly when the
ledger mutation commits.

Total FDB value bytes per trade today: **150 B**. With shape C: **221 B**
(+47 %). The absolute increase is smaller than shape B because the existing
receipt already pays the key cost; only the value grows. A pure credit adds one
new write of 52 B.

### What archiving the receipts requires

Today `ledger/receipt/` is a single FDB family ordered by commit versionstamp. To
archive it:

1. A new tailer (or a new pass in the existing tailer) must scan the `lr` key
   range, segment the scan, encode objects, and record a watermark.
2. The archive schema must include the enriched `ReceiptRow` fields, plus the
   commit versionstamp (the key), plus an encoding-version trailer.
3. The retention clamp must be extended to the receipt family, or receipts must
   be retained independently.

This is a second stream in practice, but it reuses the existing intent-path
write rather than adding a new one.

### Atomicity

**Atomic by construction.** The receipt is already inside the FDB transaction.
The only change is making receipts mandatory for every mutation and enlarging
their payload. The ledger mutation and the audit record share one commit
barrier.

### Failure modes

- Receipt value grows by ~71 bytes per trade; at 52 bytes per pure credit the
  receipt family grows faster than today.
- The `ledger/receipt/` range scan for the new tailer is hot: every intent appends
  to the end of the same key prefix. A range scan during write load adds read
  contention on the family.
- Archiving receipts still does not give #615 per-item ownership continuity for
  free: a receipt records "item X moved from A to B at tick T", which is exactly
  what the sweep needs, but reconstructing the full ownership interval graph
  requires a sort by `ItemUid` over the archive window.

## Measured transaction-size headroom

FDB limits: 10 MB transaction, 100 KB value, 10 KB key
(`docs/adr/0011-persistence.md:15`).

| case | written value bytes per intent |
|---|---:|
| one trade today | 150 B |
| shape A (journal only) | unchanged FDB size; +141 B journal frame |
| shape B (+ effect row in FDB txn) | +91 B → ~241 B |
| shape C (enlarged receipt) | +71 B → ~221 B |
| worst-case 64-op intent with enriched receipts | < 20 KB |

All shapes remain orders of magnitude below the 10 MB transaction limit. The
value-size limit (100 KB) is not threatened by any of the proposed records.

## Volume impact on journal / archive throughput

At P2 load:

- Bulk diffs: ~18,000 records/s, ~26 MB/s.
- Intents: ~530 intents/s (≈16,000 intents / 30 s).

Shape A adds ~530 effect records/s to the journal. At 141 B/frame that is
**~75 KB/s**, or **0.3 %** of the 26 MB/s bulk stream. The archive tailer pays
roughly the same proportional increase in object count and bandwidth.

Shapes B and C leave the journal unchanged. The archive grows by the effect
record bytes: ~530 records/s × ~90 B = **~48 KB/s** of additional archive
payload, plus object metadata overhead. This is negligible against the 26 MB/s
bulk stream but is a new archive family the operator must monitor.

## Identity: account or node?

`JournalRecord::author` is a `NodeId` (transport identity).
`ReceiptRow::parties` is `Vec<AccountId>` (economic identity). #807 recorded that
a sweep reasoning about accounts must join against `id/` bindings (#106) or
restrict itself to node-level attribution.

The effect record should carry **account identity**, not node identity, for two
reasons:

1. The sweep reconciles ledger state, which is keyed by `AccountId`. A node id
   cannot be summed into a conservation equation without a join.
2. The intent path already knows the accounts: `ReceiptRow::parties` and the
   balance/item keys are all account-scoped. Adding node identity would be a
   second-class field that does not help #615 and would invite incorrect
   attribution.

If shape A (journal) is chosen, the `JournalRecord::author` column remains a
`NodeId` for bulk records, but the new `EconomicEffect` payload should include
`AccountId`s. The archive schema should not pretend `author` is an account for
one kind and a node for another.

## Summary comparison

| criterion | Shape A: journal | Shape B: second stream | Shape C: enriched receipts |
|---|---|---|---|
| **Atomic with FDB effect** | **No** | **Yes** (if in-txn) | **Yes** |
| Couples ledger history to journal/archive availability | Yes | No (if independent watermarks) | No |
| Adds intent-path latency | journal fsync path | one small FDB write | one slightly larger FDB write |
| Tailer change | minimal | large: new segmentation, keys, schema | moderate: new pass over `lr` |
| Archive schema change | add `RecordKind` | new object family / schema | new object family / schema |
| Per-trade byte cost | +141 B journal frame | +91 B FDB write | +71 B receipt value |
| Pure-credit coverage | yes | yes | yes, requires new receipt |
| Matches D11 split | no | yes | yes |

## Recommendation

**Shape C, with shape B as the fallback if the enriched receipt proves too
fragile.**

Shape C wins on three decisive grounds:

1. **Atomicity is free.** The receipt already lives inside the FDB transaction.
   Making it mandatory and honest requires no new transaction boundary.
2. **It is the smallest change to the intent path.** +71 bytes per trade, +52
   bytes per pure credit, versus a new journal append or a new FDB row family.
3. **It preserves D11's split.** Bulk remains journal-only; economic intent
   history remains FDB-backed.

Shape B is the right fallback if the owner decides that receipts must stay
lightweight and that a separate effect row is a cleaner separation of concerns.
It also costs atomicity correctly but pays more in tailer complexity.

**Shape A should be rejected** unless the owner is willing to accept a
non-atomic window between the FDB commit and the journal fsync. That window
defeats #615's purpose. The coupling to the journal clamp is a secondary concern,
but the atomicity gap is decisive.

The one shape-specific risk in C is the hot `ledger/receipt/` range scan for the
new archival tailer. That is a read-contention question that a prototype should
measure before dispatch.

## Open measurement debts

- **Live FDB latency delta.** The available development cluster
  (`/tmp/orrery-fdb-669/fdb.cluster`) was not reachable, so the exact microseconds
  added by a 91-byte FDB write or a 71-byte receipt enlargement were not measured
  live. The existing instruments place mean `commit_us` at ~2 ms; the marginal
  cost is expected to be sub-100 µs but should be confirmed on a private cluster.
- **Archive tailer read contention.** No prototype measured scanning
  `ledger/receipt/` while intents append to it.
- **Object-store cost.** No object-storage dependency exists in the workspace yet
  (#807), so archive-bandwidth numbers are derived from frame sizes and D20 rates
  rather than measured uploads.

## References

- #832 — archive cannot see economic effects
- #615 — daily full conservation sweep
- #807 — archive record schema / `author` vs account
- `docs/adr/0011-persistence.md` — D11 write-class split
- `docs/08-persistence.md` §11.5 — corrected consumer description
- `docs/adr/0020-journal-retention.md` — D20 journal bounds and clamp
- Measured data cited from `docs/data/p2-barrier-shape-2026-08-19.jsonl` and
  `docs/data/p2-intent-fence-2026-08-19.jsonl`.
