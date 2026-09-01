# ADR-0051: v1 terrain is not durable state

**Status:** Proposed · **Date:** 2026-09-01 · **Decision:** D51

This record is non-normative until accepted. See the [ADR
index](../DECISIONS.md) for precedence, scope, and the complete accepted
decision set. Acceptance is reserved to the owner.

**Acceptance provenance.** The owner's authoritative **"OWNER DECISION,
2026-09-01"** comment on [#830] chose deletion after [#834]'s pricing: terrain
is not durable state in v1. This draft records that choice without accepting
it; flipping this record to Accepted is the owner's act alone.

**Supersedes on acceptance:** the terrain allocation in [D11] — its bulk-state
and checkpoint bullets' `TerrainDelta` / `chunk/` promises, including seeded
`chunk/` rows — and the resulting `k` family allocation. It amends [D32]
clause (c)'s byte ledger and [D35] §4's zero-clean-byte proof: removing `k`
changes `26 - 17 registered families - 6 exclusive ends - 2 accepted
allocations` from zero to **one clean prefix byte**. While Proposed, this draft
does not silently alter any accepted record.

Out of scope, owner-reserved: replacing or deferring the P6 bulldozed-town
criterion in [docs/11](../11-roadmap.md). That demonstration cannot claim a
terrain restoration once this decision is accepted; this record deliberately
does not select its replacement. Also out of scope: [#822]'s in-flight rollback
contract. Its "`chunk/` once terrain exists" parenthetical should be adjusted
if that work resumes, but this branch does not edit #822.

## Context

The surface advertised durable terrain while doing no state transition:
`RecordKind::TerrainDelta` folded into empty arms in actor and recovery,
nothing constructed it, no terrain state joined a checkpoint, and no in-tree
writer populated `chunk/`. The seeder's one real reader was its wipe path,
which cleared the otherwise unwritten range.

The price to make that surface true is not an enum arm. At D20's measured
18,000 records/s, 1 KiB terrain deltas add **19.58 MB/s / 1.69 TB/day** of
journal ingress. Added to the measured mix, 128 MiB segments seal every
**2.94 s**; the archive tailer then clamps release behind archival publication.

Conversely, D35's current keyspace proof closes only because `z` is allocated
to `jarchive/`; [D32] says documented-but-unimplemented families still consume
a byte. Removing the unimplemented `k` family therefore recovers exactly one
clean byte where none existed.

## Decision

### (a) Delete the false v1 durable terrain surface

v1 has no durable terrain state. It has no `RecordKind::TerrainDelta`, no
`chunk/` key family, no terrain section encoder, and no terrain checkpoint,
recovery, area-load, archive, replication, or seed-write contract. A future
terrain proposal must not restore these names as placeholders: it needs a new
owner decision that defines the payload, replay/idempotence, checkpoint
atomicity, admission, archive/restore semantics, workload measurement, and a
new key allocation.

Archive discriminant 1, formerly reserved for `TerrainDelta`, is retired and
must remain unassigned. The reader refuses it rather than interpreting an old
archive row as a future record kind.

### (b) The seeder wipe changes by name, not silently

`wipe` clears every seeded v1 durable family it can write: the scenario's
`world/` rows, its grid-scoped seed metadata, and `content/version`, after its
existing fence and operator-confirmation checks. It no longer clears a `k`
range, because `k` is not a supported durable family and the v1 seeder has no
writer for it. This is a deliberate removal of stale-development-terrain
cleanup, not a silent narrowing of any v1 durable-state wipe and not an
attempt to clear a prefix that no longer exists.

### (c) One byte is recovered, but not pre-spent

The recovered `k` is exactly one clean prefix byte. It may later be spent only
by the normal allocation decision required by D32 clause (c); this decision
does not reserve it for terrain or any other family. The byte changes the next
allocation from an arithmetic impossibility to a separately governed choice.

### (d) Reintroduction fails loudly

`scripts/terrain-substrate-gate.sh`, run by `./scripts/check.sh gates`, scans
the production `RecordKind`, seeder, and keyspace seams. It refuses
`TerrainDelta`, the removed section-encoder / `chunk/` surface, and every
recognized production form of a `k` constructor or registry entry. Its own
non-vacuous fixtures mutate a `TerrainDelta` variant and a `k` key constructor;
each must fail by name. Rust exhaustiveness provides a second line of defence:
constructing the absent enum variant is a compile-time error, and adding it
back forces every exhaustive record-kind fold to acknowledge it.

## Consequences

- The P6 terrain-restoration wording is now knowingly blocked on an
  owner-selected replacement or deferral; it is not changed by this draft.
- A later terrain implementation starts with a fresh design and allocation
  decision rather than an apparently working variant that discards records.
- The archive codec retains all remaining discriminants and permanently leaves
  1 retired, so deleting terrain does not renumber durable archive kinds.

[#830]: https://github.com/baadc0de/orrery/issues/830
[#834]: https://github.com/baadc0de/orrery/pull/834
[#822]: https://github.com/baadc0de/orrery/pull/822
[D11]: 0011-persistence.md
[D32]: 0032-enforcement-ramp.md
[D35]: 0035-lease-key-discriminator.md
