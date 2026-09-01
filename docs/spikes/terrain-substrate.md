# Spike: price the terrain substrate decision

**Status: PROPOSE-ONLY, non-normative spike.** This document implements
nothing, decides nothing, and amends no ADR. It makes [#830] decidable. The
owner chooses between (A) making terrain durable v1 state and (B) deleting the
unimplemented surface, reclaiming `chunk/`, and changing the P6 demonstration
to something that exists. The recommendation below is advice the owner may
reject.

**Date:** 2026-09-01. **Reads from:** [D11](../adr/0011-persistence.md),
[D17](../adr/0017-risks-and-open-questions.md), [D19](../adr/0019-indexed-waldb-journal.md),
[D20](../adr/0020-journal-retention.md), [D32](../adr/0032-enforcement-ramp.md),
[D35](../adr/0035-lease-key-discriminator.md), [D47](../adr/0047-rollback-unit.md),
[docs/08](../08-persistence.md) §§3.4, 8, 10, 11, [docs/11](../11-roadmap.md)
P6, [#830](https://github.com/baadc0de/orrery/issues/830), and
[#822](https://github.com/baadc0de/orrery/pull/822).

Every code citation below was opened on this tree. Anchor on the quoted shape,
not a line number.

---

## 0. Decision, recommendation, and the non-choice

The owner is choosing this:

| Choice | v1 durable terrain | `chunk/` / `TerrainDelta` | P6 consequence |
|---|---|---|---|
| **A — build** | Yes: a terrain edit survives append, checkpoint, recovery replay, area load, archive, and forward restore. | Keep `k` and give the record a real fold. | The existing bulldozed-town criterion becomes implementable only after the new terrain gate passes. |
| **B — delete** | No: terrain is not durable state in v1. | Remove the variant and `k` family, including the wipe-only reader. | The owner replaces or defers the bulldozed-town criterion; options are in §4. |

**Recommendation: choose B for v1.** It removes a misleading public wire and
durable-key surface that currently performs no state transition, returns one
scarce prefix byte, and lets P6 demonstrate forward restoration on state that
actually exists. A is a real subsystem — not a fold-arm patch — and has no
defined payload, replay idempotence rule, checkpoint representation, client
read shape, or workload measurement. This recommendation does **not** choose a
replacement P6 demonstration; that is explicitly owner-reserved.

The status quo is not a third option. It advertises durable terrain while
discarding it, reserves a byte whose writer does not exist, and leaves the P6
criterion dependent on both.

## 1. What the tree proves today

| Claim | Evidence | Result |
|---|---|---|
| A terrain record has no fold. | `actor.rs` matches `TerrainDelta` with `CheckpointMark` in both state and per-entity-LSN arms; `runtime.rs` matches it with `Rekey` and `CheckpointMark` in its recovery fold. Every arm is `{}`. | A journaled terrain payload changes neither hot state nor recovered state. The actor still advances the general checkpoint watermark, so an ignored record can be released after a checkpoint. |
| No terrain state is checkpointed. | Actual `CellActorState`, `CheckpointSnapshot`, and `CheckpointData` carry entities, cell map, tombstones, superseded rows, and watermark — no terrain field. `FdbCheckpointStore` scans and writes `world/`; it has no `chunk/` load or write. | The documented `base + delta list` is not implemented. There is no base to load before replay. |
| No executable code constructs `RecordKind::TerrainDelta`. | A repository search of Rust sources finds the enum declaration; the three empty fold arms; and `archive/object.rs`'s pinned discriminant encode/decode plus its round-trip list. | The discriminant can cross the generic `DiffUplink.kind` wire field, but there is no in-tree producer or state consumer. The archive would faithfully archive a record that did nothing. |
| `chunk/` is allocated and unwritten. | `keyspace.rs` defines `chunk_key(grid, cell, section)` as `k + grid + cell + u16 section`, its subtree range bounds, and registers `k` as `chunk`. `orrery_seed/src/plan.rs` says: “v1 has no `chunk/` rows.” | The key shape is ready, but no checkpoint, seeder, or area reader writes or loads a chunk row. |
| The issue's total “no callers” claim has one correction. | `chunk_key` is used only by its own unit/registry tests. `chunk_range_start` and `_end` are also called by `orrery_seed/src/wipe.rs`, which clears the range after proving no live fence rows. | The sole non-test caller deletes possible stale `chunk/` rows; it does not read or write terrain. Deletion must remove or revise this wipe step too. |
| `k` consumes a scarce byte. | D35 §4 lists 18 registered one-byte families, including `k`, six exclusive range ends, and accepted `y`/`z` allocations: 26 total and zero clean bytes. D32's allocation rule says documented-but-unimplemented families still consume a byte. | Reclaiming `k`, if the owner also ends D11's terrain allocation, changes the arithmetic from 0 to **1** clean byte. |

There is a second, important correction to the request's context. At inspection,
[#822] is **closed, not merged** (`mergedAt: null`), and this tree has neither
`RecordKind::Restore` nor the stated restore contract in `docs/08`. Its PR body
is a useful proposed shape — bulk-only, `world/`, `grid/`, and `chunk/` once
terrain exists — but is not a landed contract or normative source on this
branch. The pricing below nevertheless treats that parenthetical as the exact
integration work a future forward-restore implementation would need.

## 2. Branch A — make terrain durable

### 2.1 `chunk/` is the right physical shape, conditional on building terrain

Yes. D11 explicitly assigns terrain to `chunk/{cell_id}/{n}`, and the landed
constructor is `k | grid:u32 BE | cell:u64 BE | section:u16 BE`. It gives one
cell's sections contiguous order and makes a shard/subtree range scan use the
same Morton cell bounds as `world/`. It also keeps each section under FDB's
100 KB value limit, as D11 and docs/08 §10 require. The archived record already
carries `(grid, cell, lsn)`, and #807's landed tailer sorts exactly that total
key; keeping `section` inside the payload preserves the archive's cell-centric
restore selector.

The correct grain is therefore **per-section mutation, per-cell ownership and
checkpointing**:

```text
Journal target:       (grid, cell, section)        // one operation
Actor ownership:      shard contains cell           // single writer
FDB base row:         chunk/(grid, cell, section)  // <= 100 KB
Archive clustering:   (grid, cell, lsn)             // #807 / landed tailer
```

Making the archive primary key per chunk would require a new schema/index and
would make the P6 operation, which restores a cell, less direct. It buys
nothing the payload's section identifier cannot provide.

### 2.2 Define `TerrainDelta` before admitting one

The variant's current comment — “a terrain delta for a chunk section” — is not
a meaning. For A, the following is the minimum definition the implementation
ADR/spec must freeze:

1. `JournalRecord.grid` and `.cell` name the owner cell; the payload names one
   `section: u16` and a versioned, deterministic operation. `record.entity`
   must be defined as the editing `PLAYER_BOUND` entity used for lease fencing,
   not silently treated as a terrain identity.
2. The operation transforms a named base representation into one exact next
   representation. It has a base generation/hash precondition, bounded decoded
   size, and Ruleset reach/rate/tool validation. A bad precondition is refused,
   never applied approximately.
3. Re-offering an unacknowledged edit is a no-op. The present generic
   `(entity, tick)` description is insufficient: one editor can legitimately
   edit two sections in one tick. Carry a stable operation id (at least editor
   sequence plus section) in the terrain payload and retain enough per-section
   dedupe state to make replay and retransmit idempotent.
4. The fold applies the operation to the in-memory base plus pending operations,
   marks that section dirty, and exposes the result to live/area readers. It
   must not increment the checkpoint watermark merely because it ignored the
   record, as today.

This is larger than adding an enum match. `DiffUplink` and the gateway currently
mean “one change-detection diff for an entity”; its fenced path requires
`by_cell[entity] == record.cell`, and its acknowledgement is keyed by entity
and tick. A terrain path can share the journal and actor serialization, but it
needs either an explicitly extended bulk envelope/ack or a carefully specified
editor-entity interpretation. Leaving that ambiguous recreates the trap under
a non-empty match arm.

### 2.3 Checkpoint and recovery: the load-bearing price

The current zero-loss proof is checkpoint base + records with local LSN greater
than the checkpoint watermark. `CheckpointData` contains only entity state;
`FdbCheckpointStore::checkpoint` writes row batches and commits `ckpt/` last;
`load` rebuilds the `world/` bag and recovery folds the tail. Terrain must join
that same atomic *meaning*, not merely gain rows in FDB.

The new checkpoint snapshot must contain, per dirty `(cell, section)`, the
compact base image, its generation, and the dedupe/applied-operation state
needed by §2.2. The FDB store must:

1. write those `chunk/` base rows and clear sparsely-elided rows;
2. load the relevant chunk subtree with the `world/` subtree into the actor;
3. publish a single watermark only after the world and chunk snapshot it
   describes is durable; and
4. clear actor dirtiness only after that publication succeeds.

There is a non-negotiable crash rule. Chunk rows may need multiple FDB
transactions because the checkpoint budget is 10 MB while a section may be up
to 100 KB. If rows for watermark `W` commit but the final `ckpt/` row does not,
recovery cannot load those rows and then reapply the same non-idempotent deltas
from an older watermark. It must either identify and ignore incomplete chunk
generations, or make every delta idempotent against its retained operation id.
This is the terrain-specific part of the checkpoint design that has not been
paid for by the current entity checkpoint code.

The actor snapshot, restore, split/handover partitioning, cold area page, and
checkpoint FDB adapter all need a terrain member. A terrain-less `SnapshotPage`
and `ColdCellReader` are another concrete implementation seam: neither can
serve the compacted base a late joiner needs today.

### 2.4 Archive and forward restoration

The archive tailer is already the right ordering substrate: it consumes sealed
128 MiB logical segments, sorts by `(grid, cell, lsn)`, writes/verifies an
object, then advances its watermark. With archive retention enabled, release is
clamped behind that verified watermark; an archive that cannot keep up makes
the journal retain the growing suffix.

Terrain makes the parenthetical “`chunk/` once terrain exists” real work:

- A historical restore needs an exact pre-image of every touched section, not
  merely a stream of opaque operations whose base was overwritten. The archive
  must be able to reconstruct it or refuse it by name, just as the proposed
  #822 contract requires for entity images.
- A forward restoration record must name a full terrain-section target image
  (or an equally idempotent, versioned inverse operation), fold it into the
  same actor map, and checkpoint to `chunk/`. An entity-only full-image
  `Restore` cannot restore a terrain section.
- Restore stays bulk-only: it writes `chunk/` together with the existing bulk
  families, never ledger or intent rows. Its archive entry must remain an
  ordinary later LSN, not a rewind.

Thus A also pulls terrain into the archive-state-at-time work. It does not get
the P6 restore for free merely because archive serialization already preserves
the `TerrainDelta` discriminant.

### 2.5 Capacity price: bounded arithmetic, not a benchmark claim

D20 measured the P2 load phase at roughly 18,000 records/s and 26 MB/s. The
raw journal's logical accounted span is `payload.len() + 64`; a logical segment
is 128 MiB. At the measured aggregate stream, a segment turns over every
about **5.16 s**. The tailer has to buffer, sort, encode, verify, and publish
each such segment; it cannot release it until publication verifies.

No terrain payload encoding exists, so no honest p99, compression ratio, or
terrain arrival rate exists to quote. The price is instead explicit. For `r`
terrain records/s whose mean encoded payload is `p` bytes:

```text
additional journal ingress = r × (p + 64) bytes/s
additional archive payload ≈ r × (p + 30) bytes/s
```

The second line is the tailer's documented uncompressed payload-plus-per-row
approximation; fixed archive columns are already part of its object bound.

If terrain alone arrives at D20's 18,000 records/s, the consequences are:

| Mean delta payload | Extra journal ingress | Extra journal/day | Segment turnover if this replaces the whole stream | Extra archive payload/day |
|---:|---:|---:|---:|---:|
| 256 B | 5.76 MB/s | 498 GB | 23.3 s | 443 GB |
| 1 KiB | 19.58 MB/s | 1.69 TB | 6.85 s | 1.64 TB |
| 4 KiB | 74.88 MB/s | 6.47 TB | 1.79 s | 6.42 TB |

If 1 KiB terrain deltas are **additional** to the measured P2 mix at that
rate, total journal ingress rises from 26 to about **45.6 MB/s** and 128 MiB
segments seal every **2.94 s**, rather than 5.16 s. If they are only 10% of
that 18,000/s stream, the additional journal rate is 1.96 MB/s and the added
archive payload is about 164 GB/day. These are throughput/retention prices,
not a prediction that gameplay will generate either workload.

The implementation gate for A must therefore run representative edit shapes,
not generic 1,400-byte component payloads, and report terrain share, mean and
p99 encoded bytes, segment seals/s, checkpoint bytes/transactions, archive
lag, archive verification rate, `journal_commit_ms`, and release floor. A
tailer failure under this load is no longer only slow history: the archive
clamp holds journal release and starts the disk-full countdown.

### 2.6 Build scope and the acceptance mutation

The priced work is at least: versioned terrain payload and client/gateway
admission; actor fold, dedupe, snapshot, split and recovery; chunk checkpoint
read/write/GC and area-load response; live replication; archive historical
reconstruction and forward terrain restore; plus a measured P2/archive arm.
This crosses `orrery_protocol`, `orrery_persistd` actor/runtime/checkpoint/
keyspace/archive/gateway paths, client replication, seeding, documentation,
and the P6 harness. It is a subsystem, not a one-file feature.

Its mutation check must be named and non-vacuous:

> Seed a procedural cell; append two distinct section operations, checkpoint
> after the first, append the second, kill/reopen from the checkpoint and tail,
> and assert the exact section hash and operation-dedupe state match the live
> actor. Then change the terrain fold to the present empty behaviour and show
> that test fail by name.

A second arm must crash after chunk-row batches but before the checkpoint
watermark, proving no operation is doubled or lost. “No terrain was written”
or a filtered test proves neither property.

## 3. Branch B — delete the false surface and reclaim `k`

### 3.1 Actual deletion surface

The code deletion is small but not zero. Today it touches these production
modules:

| Surface | What B removes or changes |
|---|---|
| `orrery_protocol::RecordKind` and `DiffUplink` docs | Remove `TerrainDelta` and the word “terrain” from the generic entity-diff kind list. |
| actor and runtime folds | Remove the empty terrain arms. |
| archive object codec | Remove the pinned kind-1 mapping and its round-trip list. A compatibility policy must refuse or explicitly migrate an archived kind 1; it must not reinterpret it as a future kind. |
| keyspace | Remove `chunk_key`, chunk ranges, their tests, and the `k` family registry entry. |
| seeder wipe | Stop clearing the deleted range. |
| docs and tests | Remove v1 terrain promises from D11-following expansions and replace them with the owner decision; delete terrain-only tests. |

There is no producer, fold, checkpoint row writer, cold reader, or replication
consumer to unwind. That is why the source diff is modest. It is not correct to
say “nothing references it”: the archive codec deliberately reserves a durable
discriminant, and the seeder deliberately clears the key family.

### 3.2 The byte dividend is one clean prefix byte

D35's verified ledger is `26 = 18 registered + 6 range ends + 2 accepted
allocations`; `k` is one of the 18. If B removes the terrain allocation from
the accepted design as well as the code, the same arithmetic becomes:

```text
26 lowercase bytes - 17 registered families - 6 range ends - 2 y/z allocations = 1
```

So B restores exactly **one** clean byte. That matters because the current
number is zero: D32's rule otherwise forces every new key kind to share a
sub-discriminator or justify a multi-byte-family ADR. One byte does not solve
all future keyspace pressure, but it changes the next family decision from
“closed by arithmetic” to a normal, still-ADR-governed allocation decision.

This is not a silent code cleanup. D11 normatively names terrain deltas and
`chunk/`; D32 says documented-but-unimplemented families count; D35 records
the current zero-budget proof. The deletion decision must therefore be written
as a new ADR that supersedes/amends D11's terrain clause and explicitly updates
the D32/D35 arithmetic. An expansion-doc edit alone would conflict with
accepted D11.

### 3.3 Is deletion a one-way door?

No in-tree data migration is implied: no in-tree writer can have produced a
terrain row or record. Before deletion, an operator must still inventory any
external development archive or FDB fixture and either prove kind 1 / `k` absent
or retain a reader that refuses it explicitly. B is deliberately a **design**
reversal, not a promise that a later re-add is a two-line enum restoration. A
later owner must allocate a prefix again (or choose another key shape), add a
new durable archive kind rather than casually reusing kind 1, and pay every A
work package above.

That is a cheap re-add only in the limited sense that v1 has no committed
terrain format or rows to migrate. It remains a substantial future build. The
owner should choose B only if “terrain is not durable state in v1” is intended,
not as a temporary way to hide an imminent terrain feature.

### 3.4 The deletion mutation check

The implementation PR should add a static gate with a clear failure line, for
example `terrain_substrate_is_absent`, which scans production Rust sources for
the removed variant and `chunk/` constructor/registry. The mutation is to
reintroduce a `TerrainDelta` variant or register `k`; the named gate must fail.
It must run in `scripts/check.sh` and be covered by the gate script's own
self-test. Compile success after deleting an unused variant is useful, but it
does not prove the trap cannot return.

## 4. P6 demonstration: owner-reserved options under B

The present P6 wording is “a griefer bulldozes a player town; an operator
restores it to a timestamp”. Under A, retain it only after the terrain mutation
gate proves the bulldozed state survives checkpoint-plus-tail replay.

Under B, the owner, rather than this spike, may choose one of these options:

1. **Entity town:** make the town a set of already-durable `world/` entities;
   demonstrate a griefer's bulk entity-state damage and a forward archive
   restoration, ledger untouched.
2. **Different durable incident:** replace the town with another real bulk
   `world/` mutation whose pre-image and forward restore contract are proven.
3. **Defer this P6 acceptance item:** retain terrain as the intended object but
   make A a prerequisite before the demo can be claimed.

Options 1 and 2 change the criterion's object; option 3 changes schedule. None
is selected here. What cannot remain is wording that claims an operator restores
terrain when the durable tier has no terrain state.

## 5. Owner checklist

Choose **A** only if the product needs mutable terrain as v1 durable state and
is willing to fund its semantic, checkpoint, archive, client, and measured
bandwidth work before P6. Require the two crash/replay mutations in §2.6 and a
terrain-shaped P2/archive measurement before accepting it.

Choose **B** if v1 can honestly say terrain is not durable state. File the
superseding ADR; remove the false wire/key/archive surface and its wipe-only
caller; make the static non-reintroduction gate; and choose one §4 demo
consequence. The recovered `k` byte is real headroom, not a reason to spend it
without the normal allocation decision.
