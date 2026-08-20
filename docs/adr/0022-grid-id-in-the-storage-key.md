# ADR-0022: `GridId` stays a key discriminator (P-7)

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D22

This decision is normative. See the [ADR index](../DECISIONS.md) for
precedence, scope, and the complete decision set.

**Supersedes:** nothing. It closes open question **P-7**
([docs/12](../12-world-seeding.md) §17, S6): *"`GridId` in the storage key: key
discriminator vs. per-grid Directory subspace"*, whose stated deadline is **P2
exit**, now reached.

## Context

Nested grids give each grid its own `CellId` space (D5), so the same cell id
under two grids must not be the same row. Two ways to arrange that in
FoundationDB:

- **A key discriminator.** Every key in a grid-scoped family carries the
  4-byte `GridId` immediately after the family byte. Uniform, no indirection,
  and a per-grid subtree stays one contiguous range.
- **A per-grid Directory subspace.** The FDB directory layer maps a path to a
  short allocated prefix. Cleaner keys, and the discriminator's bytes come back
  — at the cost of a directory lookup per grid and a second, mutable, durable
  mapping in the path of every read.

P-7's proposed resolution path was to prototype the subspace form against the
`kepler` showcase and measure its lookup cost against the < 50 ms
first-page-in budget.

**That prototype has not been built, and this record decides without it.** What
changed is which question is actually open. The key form is not a candidate any
more: it is implemented and carrying the gate, in `keyspace.rs`, across four
row families — `world/{grid}/{cell}/{entity}`, `ckpt/{grid}/{shard}`,
`actor/{grid}/{shard}` and their range bounds, with `decode_world_key` reading
it back. So the choice on the table is no longer "which to build" but "whether
to migrate", and the evidence that bears on it is different.

### What the budget says

The P2 kill-9 gate measures cold first-page-in as `area_first_page_ms` against
the D16 50 ms budget. Across the five passing raw-backend runs of 2026-08-20
(`docs/data/p2-journal-raw-2026-08-20.jsonl`, 125 samples each):

| | p50 | p99 | max |
|---|---:|---:|---:|
| `area_first_page_ms` | 1.25–2.5 ms | 2.5–3.5 ms | 2.5–3.0 ms |

**At most 7% of the budget is spent**, leaving ~46.5 ms of headroom. A cached
directory lookup is a fraction of one FDB read; an uncached one is one read.
The measurement P-7 asked for would be answering whether a millisecond fits in
forty-six.

### What the discriminator costs

4 bytes per key, in four families, forever. At the gate's scale — 10 000
entities across 10 000 cells — that is tens of kilobytes. At a billion `world/`
rows it is 4 GB of keys, against entity bags that are hundreds of bytes each.
It is a real cost and a second-order one.

## Decision

**1. `GridId` stays a key discriminator.** The Directory-subspace form is not
adopted. `keyspace.rs` remains the single definition of the layout.

**2. The reasons, in the order they carry weight.**

- **Migration asymmetry.** Durable data in the key form already exists — the
  seeder writes it, the gate writes it, every checkpoint and fence row carries
  it. Keeping it costs nothing; changing it is a rewrite of every row in four
  families plus a compatibility window in which both layouts are readable. The
  cheap moment to prefer the subspace was before the key form was built, and
  that moment has passed.
- **A subspace adds an availability dependency the key form does not have.**
  The directory layer is durable, mutable, cluster-side state that every cold
  read resolves through. A stale or lost prefix mapping does not slow a read
  down, it makes the data unaddressable. The discriminator is derivable from
  the `GridId` in hand, always, with no lookup that can be wrong.
- **The budget is not the binding constraint.** 7% used. The lookup this
  decision avoids was never going to be what missed 50 ms.

**3. The 4 bytes are accepted as a stated, permanent cost**, and named here so
that a future capacity model counts them rather than rediscovering them.

**4. What reopens it.** A keyspace measurement in which grid-scoped key bytes
are a leading term in storage or in FDB's index memory; or a nested-grid
deployment large enough that per-grid *isolation* (rather than distinctness)
becomes an operational requirement — the directory layer can move or drop a
whole grid's prefix in one operation, which the discriminator cannot. Neither
is true today.

## Consequences

- **P-7 is closed and the S6 delta in [docs/12](../12-world-seeding.md) §16 is
  satisfied by what is already built**: the `world/` key carries the
  discriminator, and no subspace specification is owed.
- **`decode_world_key` stays total.** A key is self-describing: `(grid, cell,
  entity)` is recoverable from the bytes alone, without a cluster read. The
  archive tailer (R7) and any offline tool over raw ranges depend on that, and
  the subspace form would have made every such tool a client of the directory
  layer.
- **The prototype P-7 asked for is not owed and will not be built.** If the
  question reopens on one of the triggers above, the measurement to take then
  is a keyspace-size one, not a latency one — which is a different rig than the
  one P-7 imagined.

## Alternatives considered

- **Per-grid Directory subspaces**, as P-7 framed them. Rejected above: the
  saving is second-order, the cost is an indirection on every cold read, and
  the migration is now the dominant term.
- **Build the prototype anyway before deciding.** Rejected: it would measure a
  latency question whose answer the gate already bounds at 7% of budget, and it
  would not measure the term that actually decides this — the cost of migrating
  durable data that exists.
- **A shorter discriminator (2 bytes).** Rejected: `GridId` is a `u32` in
  `orrery_protocol`, and narrowing the storage form of an id to save two bytes
  per row buys a truncation bug for a cost this record has already called
  second-order.
