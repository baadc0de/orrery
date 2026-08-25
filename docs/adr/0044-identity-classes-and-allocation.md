# ADR-0044: Three closed identity classes, granted-range derivation, and no provisional durable identity

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D44

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree, as
proposal R3 of [A11](../plans/a11-adrs-and-pr-plan.md) §2 ([#407]), with the
id-reuse question — which [A5] priced and left open — decided by the owner at
acceptance as clause (f) records.

**Supersedes:** nothing. It sits under [D42], the umbrella record of the #395
architecture programme, and supplies the identity vocabulary the sibling
proposals consume. It **extends** [docs/08 §6]'s block-grant vocabulary
(clause (c): a granted range as the base of *derived* identifiers, not only of
minted ones) and it ships one code-comment correction: `PersistId`'s doc
described the unbuilt peer-side journaled grant path in the same present tense
as the built cluster-side mint (`crates/orrery_protocol/src/persist.rs:41-45`
before this record; corrected in this record's tree to say which half is
built). It amends no accepted record's normative text.

Out of scope, decided by their own proposals or not at all: the per-component
capability policy P/R/W/N/A and its invalid combinations, including the
engine-handle-in-schema corridor's mechanical closure (R4 — the sibling record
that consumes these identity classes; A5 G-1 stays review-held until it
lands); the determinism envelope and gate bundle (D43, drafted in a sibling
lane); message-class semantics (R5); the rollback unit (R6); the canonical
witness projection format (R7); compatibility manifests and schema-id
governance (R8, which also owns `ComponentTypeId`/`SchemaVersion` allocation —
those are *schema* identity, inventoried by A5 §4 and not decided here);
account, item, and node identity (`AccountId`, `ItemUid`, `NodeId` — [D31],
[D33], [D3]); and all implementation scheduling — nothing here starts work in
the P4 digest trees (`crates/orrery_witness`, `crates/orrery_core`,
`crates/orrery_games`, `gates/p1-swarm` — `scripts/p4-ledger.sh:409-414`)
before P4 exit. [D42] is the umbrella; this record cites it and restates
nothing of it.

## Context

### 1. The identity model already operates in layers

The tree runs a layered identity model today; most of this record ratifies it.
Three classes name entities:

- **`PersistId(u64)`** — durable, universe-scoped, cluster-minted. "Never a
  Bevy `Entity`: this is the canonical id carried into every peer's world and
  the storage key for `world/` rows"
  (`crates/orrery_protocol/src/persist.rs:38-49`).
- **`EphemeralId { island, spawner: NodeId, seq: u32 }`** — island-scoped and
  spawner-partitioned (`crates/orrery_authority/src/ephemeral.rs:52-60`).
  Minting is a local increment with no allocator and no round trip
  (`EphemeralRegistry::spawn`, `ephemeral.rs:217-239`): collision-free by
  construction because the namespace is partitioned by spawner, and an island
  merge or epoch bump renumbers nothing, since uniqueness never depended on
  the `island` field (`ephemeral.rs:45-50`).
- **`bevy_ecs::Entity`** — a generational index into one world's allocator.
  World-local, allocation-order-dependent, and good at exactly one job:
  convicting a stale handle after despawn, because the generation dies with
  the row.

One description [A5] carried is corrected here rather than repeated: the
`EphemeralRegistry` holds no `Entity` at all — its storage is
`entries: BTreeMap<EphemeralId, EphemeralEntry>` where an entry is an
`IslandClaim` (`ephemeral.rs:149-166`). The tie to the ECS entity is the
`Ephemeral(EphemeralId)` *component* (`ephemeral.rs:342`), inserted by
`IslandClient::spawn` alongside `IslandAuthoritative`
(`ephemeral.rs:391-398`). The registry answers "who holds this id"; only the
world answers "which row carries it".

### 2. No engine handle reaches any durable format — and what enforces that

Every durable format is keyed by `PersistId` or a coarser id and has no field
that could carry an `Entity`: `world/` rows (key carries the entity's own
cell; value is a tagged bag or tombstone,
`crates/orrery_persistd/src/keyspace.rs:105-146`), journal records
(`JournalRecord.entity: PersistId`, "never a Bevy `Entity`",
`persist.rs:204-230`), the bulk uplink (`DiffUplink.entity: PersistId`,
`crates/orrery_protocol/src/gateway.rs:371-393`), and evidence
(`EntitySlice.entity`, `StateClaim.entity`, `EvidenceBundle.entity` — all
`PersistId`, `crates/orrery_protocol/src/verifiable.rs:146-231`). The
defining crate cannot name the type: `orrery_protocol` depends on no bevy
crate, and A5's mutation X5 showed the property is gate-held transitively —
adding `bevy_ecs` to `orrery_protocol`'s dependencies fails
`scripts/core-gates.sh` clause 1 through `orrery_core`'s tree.

The one seam where an `Entity` approaches the durable path translates it away
explicitly: `feed_uplink` maps the replicon diff's `Entity` to the entity's
replicated `PersistId` component — "the canonical Bevy `Entity` ↔
persistent-id mapping", written only by the entity's owner — and builds the
`DiffUplink` from it (`crates/orrery_persist_client/src/feed.rs:27-34,56-97`).

The known corridor stays named, not waved off: a game's replicated component
with an `Entity` field would ride into the journal as opaque payload bytes,
and no mechanical check watches component field types today (A5 gap G-1).
That closure is R4's, and until then the corridor is held by review.

### 3. Allocation today: three built paths, one designed, and a shared namespace

What exists on this tree, each half named honestly:

1. **Cluster mint inside intent transactions — built.** The FDB counter
   `pid/{grid}/next` is mutated only by atomic add; abandoned grants leave
   permanent, unreclaimed gaps (`keyspace.rs:1626-1640`). The intent executor
   amortizes it through a **process-local durable block grant** of 4 096 ids
   (`PERSIST_ID_BLOCK_GRANT`,
   `crates/orrery_persistd/src/intent/fdb.rs:86-92`) — "individual intent
   transactions never read that hot key" (`fdb.rs:18-20`) — and minted ids
   return in commit receipts (`persist.rs:653-660`, `:698-701`).
2. **Offline seeder mint — built.** The world seeder reserves blocks from the
   same counter (`BlockGrant`/`BlockGrantCursor`,
   `crates/orrery_seed/src/idmap.rs:16-45`; consumption at
   `crates/orrery_seed/src/apply.rs:367-381`), so designed and dynamic
   entities share one id space.
3. **Game-derived materialization ids — built.** `Ruleset::materialize`
   supplies fully described entities; "the ruleset supplies the identifier;
   the executor deliberately has no allocator", making identity "a pure
   function of the emitting entity's replayable inputs (for example
   `(parent, generation, slot)`)"
   (`crates/orrery_core/src/ruleset.rs:210-222`), never allocated from
   executor population or creation order (`ruleset.rs:270-278`). This is what
   lets an isolated single-entity replay reproduce the same descriptions.
4. **Peer-side journaled block grant — designed, not built.** [docs/08 §4]
   and [docs/08 §6] (`pid/next` row) describe contiguous ranges, default
   4 096, leased per session through the gateway, journaled, usable offline.
   No issuance, peer-side holding, or journal-record code for it exists in
   any crate (`rg -in 'BlockGrant|block.grant'` over first-party crates finds
   paths 1–2 above and the doc comments only; `RecordKind` has no grant
   variant, `persist.rs:138-152`). The `PersistId` doc comment described this
   path in the built paths' present tense; this record's tree corrects it.

**Gap G-2 (A5 §2.4): paths 1–2 and path 3 share the u64 namespace with no
partition.** Nothing prevents a step from deriving an id the cluster has
minted or will mint. Latent today — materialized entities live in executor
worlds and the games derive from small parent ids — but the moment a
materialized entity is persisted, a derived id colliding with a minted one
corrupts a `world/` row silently: journal records are `(entity, tick)`-keyed
last-writer-wins (`persist.rs:201-205`) and the cell actor replaces the bag
wholesale (`crates/orrery_persistd/src/actor.rs:1296-1300`).

### 4. The id-reuse edge, and what actually carries entity ids in evidence

`PersistId` reuse across a despawn is **legal today**, and the cell actor
codes for it explicitly: a `Spawn` or diff for a tombstoned id cancels the
marker — "A re-spawn (id reuse across a despawn) cancels the marker"
(`actor.rs:1320-1326`; `cancel_tombstone` at `actor.rs:104-115`, which also
tracks the superseded row when the entity comes back in a different cell).
The despawn lifecycle it rides on is built and discriminated:
`RecordKind::Despawn` (`persist.rs:145-148`), the durable
`TOMBSTONE_TAG ‖ postcard(Tombstone)` value with a GC deadline
(`keyspace.rs:105-146`; `Tombstone { cell, tick, gc_deadline_ms }`,
`actor.rs:39-48`). No named test pins the re-spawn cancellation itself; the
tag discrimination it depends on is pinned (§5 M3).

[A5] §2.6 motivated a reuse ban with "a strike row citing entity E should not
later mean a different E". **That motivation is wrong and is corrected here**:
strikes are keyed by *account*, not entity — `strike_key(account: AccountId)`
(`keyspace.rs:1943-1949`), and [D33] states "strikes attach to accounts, not
rotating NodeIds". What durable evidence actually carries entity identity is
the claim/frame layer: `StateClaim.entity` and `EntitySlice.entity`
(`verifiable.rs:148`, `:191`) — and each such artifact also carries a tick or
tick window (`StateClaim.tick`, `verifiable.rs:195`; frames' tick base;
`EvidenceBundle.window_start/window_end`, `verifiable.rs:216-221`;
`JournalRecord.tick`; `Tombstone.tick`). That fact is what clause (f) builds
its obligation on. The other exposed reader is historical: any full-history
query that follows one `PersistId` across a lifetime boundary — and that
reader is a P6 deliverable (the journal→archive tailer and inverse-op replay,
[docs/11 §P6]) which does not exist yet, so the obligation lands on its
design, not on shipped code.

### 5. Enforcement re-verified for this record

Three claims this record leans on as *enforced* were mutation-checked on this
tree (break the guarded stage, watch a named check die, revert, watch it
pass), rather than inherited:

- **M1 — an ephemeral entity can never acquire the persistence marker.** The
  yield path of the ephemeral claim system
  (`ephemeral.rs:446-447`) was mutated to insert `LocallyAuthoritative`
  instead of removing `IslandAuthoritative`. Named result:
  `ephemeral::tests::an_ephemeral_entity_never_gets_the_persistence_uplink_marker`
  FAILED (`36 passed; 1 failed`); reverted, `37 passed; 0 failed`. This is
  A5's X1 property, re-proven here.
- **M2 — only marked-authoritative entities are uplinked.** The
  `LocallyAuthoritative` guard in `feed_uplink` (`feed.rs:64-68`) was
  deleted. Named result:
  `feed::tests::local_granted_without_marker_is_not_uplinked` FAILED
  (`95 passed; 1 failed`); reverted, `96 passed; 0 failed`. This is the exact
  guard A5 recorded as its surviving mutation X2 — nothing pinned it then;
  [#427] pinned it, and the pin now demonstrably bites.
- **M3 — a tombstone is never mistaken for a live row.** `encode_tombstone_value`
  (`keyspace.rs:232-241`) was mutated to push `LIVE_TAG`. Named result: two
  tests FAILED, including
  `keyspace::tests::tombstone_value_roundtrips_and_is_distinct_from_live`
  (`358 passed; 2 failed`); reverted, `360 passed; 0 failed`.
