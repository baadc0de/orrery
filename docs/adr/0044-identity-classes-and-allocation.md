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
`RecordKind::Despawn` (`persist.rs:147-148`), the durable
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
`EvidenceBundle.window_start/window_end`, `verifiable.rs:218-220`;
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

## Decision

### (a) Three identity classes, and the set is closed

Every entity in every runtime holds exactly one of three canonical names
(A5 N-1):

1. **A durable entity is a `PersistId` everywhere** — wire, store, evidence,
   replay.
2. **A transient entity is an `EphemeralId` everywhere it is shared** —
   island-scoped, spawner-partitioned, minted allocator-free by local
   increment.
3. **An engine `bevy_ecs::Entity` names only rows of one world's local
   storage and may appear in no encoded artifact that outlives that world.**
   Serialized wire messages, durable rows, evidence, goldens, and hashes are
   all "encoded artifacts" here; an in-memory, never-serialized resource
   (such as `UplinkSeq.next: HashMap<Entity, u64>`, `feed.rs:44-48`) is not.

No fourth class is admitted — in particular, no provisional durable id
(clause (d)). This clause **ratifies what ships**: Context §2 is the
demonstration, and the closure's value is that it converts today's incidental
absence of a fourth class into a rule a review can point at. The one open
corridor (opaque payload bytes, G-1) is R4's to close mechanically and is not
re-litigated here.

### (b) The `PersistId ↔ Entity` index is host-owned, rebuildable, and never canonical

For any present or future world that mirrors canonical entities — including a
triggered dedicated ECS world under [D42] clause (d) (A5 N-2):

1. **One index per world, owned by the host seam.** The `SimulationHost` seam
   ([D42] clause (b)) owns the single `PersistId ↔ Entity` index of the world
   it drives; module code resolves through it. A second map is a bug, because
   two maps *will* disagree during despawn/rekey races.
2. **The index is a rebuildable projection, never an authority.** It is
   populated from the replicated owner-written `PersistId` component
   (exactly `feed.rs:27-34`'s shape today), reconstructible from world
   contents alone; it is never encoded, never hashed, never persisted, and
   never consulted by canonical rules — which receive their `PersistId` from
   the executor directly (`StateView::entity`,
   `crates/orrery_core/src/ruleset.rs:112-114`; store keyed by `PersistId`,
   `executor.rs:48-51`).
3. **Index iteration is not canonical order.** Any enumeration of entities
   for hashing, witnessing, or golden emission sorts by `PersistId` first.
   Today's witness hash is immune because nothing iterates a container into
   it; the rule exists so that property survives any storage change (A5 I3/I7
   made policy; the projection format itself is R7's).
4. **Lookups of despawned ids miss.** A tombstoned `PersistId` resolves to no
   `Entity`; a stale `Entity` held past despawn is convicted by its own
   generation, which is the one job the engine handle is for.

This too mostly ratifies: today's mappings (the `feed.rs` component, the
prediction-side `PredictedBy { authority, persist_id }`,
`crates/orrery_predict/src/wiring.rs:79-93`) already follow rules 2 and 4,
and the canonical side needs no map at all. Rule 1 binds at the moment the
host seam lands.

### (c) Derived identifiers draw from granted ranges — G-2 is closed by composition

A derived durable identifier must be a pure function of replayable inputs
**whose range is a granted block**: the emitting entity's state carries (or
its spawn record carried) a block-grant base, and `materialize` derives
`base + f(replayable inputs)` within the block (A5 N-3).

This keeps both properties at once — replay-stable derivation, which path 3
requires (`ruleset.rs:210-222`: identity stays a pure function of the
emitter's replayable inputs, never of population or creation order), and
cluster-coordinated uniqueness, which paths 1–2 already have (every range is
carved from `pid/{grid}/next` by atomic add). And it creates **no new
machinery class**: a granted contiguous range is exactly what the intent
executor and the seeder already hold process-locally (Context §3.1–.2) and
exactly what the designed peer-side path promises. The docs/08 §6 block-grant
vocabulary is hereby extended to cover grants held as *derivation bases* in
entity state, not only as mint cursors.

What this clause does **not** do: build anything, or schedule anything. The
grant-carrying field, grant sizing for emitters, and the journaling shape of
peer-side grants are implementation decisions for the PRs that first persist
a materialized entity — which is the moment G-2 stops being latent. Until
then nothing in the tree has to move, and the games' current small-parent-id
derivations remain legal because their outputs never reach a `world/` row.

### (d) No provisional durable identity

Predicted and transient spawns use `EphemeralId`; durable identity is never
provisional (A5 N-4).

- A client that fire-and-predicts a projectile has the right tool already:
  free minting, island-scoped, and structurally unpersistable — the ephemeral
  path installs `IslandAuthoritative`, deliberately distinct from
  `LocallyAuthoritative`, "so an ephemeral entity carrying this one can never
  be persisted no matter what game code does with it"
  (`ephemeral.rs:344-352`; enforcement re-proven by M1 and M2, Context §5).
- A client that optimistically renders the outcome of a durable creation
  (crafting output, loot) renders it against the intent's pending receipt and
  **adopts the receipt's minted `PersistId` on commit**
  (`persist.rs:653-660`, `:698-701`); it does not invent a `PersistId` and
  renumber later. No prespawn flow exists to grandfather
  (A5 §2.5, re-checked: `rg -i prespawn` over first-party crates finds
  nothing).

Why a provisional-then-renumbered durable id is *invalid* rather than merely
inelegant — the argument this record exists to preserve: journal records,
claims, and evidence address entities by `PersistId` under a single-writer,
`(entity, tick)`-idempotent discipline (`persist.rs:201-205`). Bytes emitted
under a provisional id are bytes the renumbered entity's history does not
contain, so replay and adjudication of any window spanning the rename would
be unproducible. The class set of clause (a) is closed precisely to keep this
door shut.

### (e) Cross-grid references carry `(GridId, PersistId)`

`PersistId` allocation is grid-scoped — `pid/{grid}/next`
(`keyspace.rs:1626-1640`) — so nested grids allocate from independent
counters and the bare u64 is unique only within its grid. Any in-state or
durable reference that may cross grids stores the pair `(GridId, PersistId)`,
not the bare id. Every durable row already carries both
(`JournalRecord.grid` and `.entity`, `persist.rs:214-216`); this rider makes
the same rule bind game-state references. Within one grid the bare id remains
sufficient, and cross-cell and cross-island references need nothing extra:
the id is cell-free (only the storage key is cell-scoped) and island-free.

### (f) `PersistId` reuse across a despawn stays legal — and durable readers must be lifetime-aware

**The owner decides: reuse stays legal.** The actor's re-spawn handling
(Context §4) is ratified, not deprecated. The owner's rationale, recorded
verbatim: *"the same `PersistId` over time is a power not a liability when it
identifies the same durable entity."*

One tension in that rationale is named here rather than smoothed over: the
rationale is about **identity continuity** — one durable entity keeping one
name across despawn-shaped lifecycle events (unload, demotion, a town
restored by rollback) — while reuse across a despawn is precisely the case
where the id does *not* identify the same entity. Both halves hold together:
continuity is the power being kept, reuse-for-a-different-entity is the
narrow hazard that rides along with it, and the obligation below is what
contains that hazard. Clause (c) already narrows it structurally — under
granted ranges, an id can be re-derived or re-minted only by the holder of
its original block — but narrowing is not removal, and this clause does not
pretend otherwise.

**The obligation: a bare `PersistId` is not a sufficient reference in
anything that outlives a tick.** Concretely:

1. **Schema rule (checkable).** Every durable or evidence schema that names a
   `PersistId` must carry a tick or tick window in the same artifact, so the
   reference is to "*the entity live at that tick*", never to the naked id.
   Every such schema today already complies — `JournalRecord.tick`,
   `StateClaim.tick`, `EvidenceBundle.window_start/window_end`,
   `Tombstone.tick`, frames with a tick base (Context §4) — so this rule
   ratifies the shipped shapes and binds new ones. No mechanical check exists
   today for a *new* schema violating it; the check belongs with R8's
   manifest declarations, where schemas become declared artifacts a gate can
   inspect, and that assignment is named here so the rule is not left as an
   unowned "should".
2. **Reader rule.** A reader that resolves a `PersistId` from a durable
   artifact must scope any attribution to the lifetime containing that
   artifact's tick, and must treat a `Despawn` record or tombstone between
   two references as an identity boundary it may not silently cross. The
   adjudication path complies by construction — windows are per-entity,
   bounded, end at a claim tick, and never span a `chain_epoch`
   (`verifiable.rs:149-151`, `:218-220`). The reader this rule is really
   for does not exist yet: the P6 journal→archive tailer and its
   inverse-op-replay/history queries ([docs/11 §P6]). **Lifetime-awareness is
   a design requirement on that deliverable**, stated now so it is priced in
   before the reader is built rather than retrofitted after the first
   misattribution.
3. **What no rule requires:** in-memory presentation state may hold bare ids
   freely — clause (b)'s index rules already make stale lookups miss — and
   canonical rules may store bare `PersistId`s as data, because a step that
   names a despawned neighbour observes `None` from `StateView::neighbor`
   (`ruleset.rs:131-138`) and must treat every cross-entity reference as
   possibly-dangling anyway.

## Consequences

- **What this record actually adds is smaller than its title**, in the manner
  [D42]'s consequences open with. Clauses (a), (b), and most of (f) ratify
  what ships — the class inventory, the mapping shapes, reuse legality, and
  evidence schemas that all carry ticks already. The record's real new
  commitments are clause (c)'s granted-range rule for derived ids, clause
  (d)'s closure of the provisional-id door, clause (e)'s cross-grid pair, and
  clause (f)'s lifetime-awareness obligation on the not-yet-built history
  reader.
- **Clause (c) makes the unbuilt grant path load-bearing for a second
  consumer.** The peer-side journaled block grant was designed for offline
  minting; it is now also the uniqueness mechanism for persisted derived
  identifiers. Whoever builds either consumer builds one mechanism. Until a
  materialized entity is first persisted, nothing must move; when one is,
  G-2 stops being latent and clause (c) is the rule the PR is held to.
- **Accepting reuse-legality spends nothing today but binds the P6 reader's
  design.** The cheap half of the alternative's benefit — evidence rows never
  ambiguous about which lifetime they mean — is bought instead by clause
  (f)'s schema rule, which every shipped schema already satisfies. The
  expensive half — bare ids being globally unambiguous forever — is
  deliberately not bought, and any future tool that assumes it is wrong by
  this record.
- **Two review-held seams are inherited, not closed**: G-1 (an `Entity`
  inside opaque replicated payload bytes) waits for R4, and clause (f)'s
  schema rule waits for R8's manifests to become mechanically checkable.
  Naming them here keeps this record honest about what is structural versus
  held by review — the distinction A9's enforcement table made the house
  standard.
- The identity vocabulary the sibling records assume is now fixed: R4's
  capability dimensions attach to state named by these classes; R6's rollback
  unit and R7's projection enumerate by `PersistId`; R8's manifests govern
  schema identity (`ComponentTypeId`, `SchemaVersion`) which this record
  deliberately leaves untouched.

## Alternatives considered

- **Static high-bit partition of the u64 space ("minted vs derived").**
  Rejected for G-2: a partition keeps derived ids from colliding with minted
  ones but not with *each other* — two emitters deriving from similar
  replayable inputs still need a per-emitter base, and the granted block
  supplies exactly that anyway. The partition would add a second namespace
  convention while still needing the grant; the grant alone suffices (A5
  §2.4).
- **Central allocation for materialized entities.** Rejected: it would make
  derived identity a function of allocation order — precisely what
  `Ruleset::materialize` forbids so that isolated single-entity replay
  reproduces the same descriptions (`ruleset.rs:210-222`) — and it would put
  a round trip inside a step.
- **A provisional durable id class with later renumbering.** Rejected by
  clause (d): under `(entity, tick)`-idempotent journal and claim addressing,
  a window spanning the rename is unadjudicable. This is the strongest
  argument in the record and the reason the class set is closed rather than
  merely enumerated.
- **Forbid `PersistId` reuse after despawn outright.** Rejected by the owner
  in clause (f). Priced honestly, since [A5] left this open as a genuine
  trade: a ban buys evidence hygiene — a bare id in any artifact would mean
  one entity forever — at the cost of a retention-window discipline
  (tombstones or minter bookkeeping would have to pin every id ever used for
  as long as any artifact naming it can resurface, where today
  `Tombstone.gc_deadline_ms` lets the marker go). The owner keeps continuity
  as the power instead, and clause (f)'s lifetime-awareness buys the same
  hygiene per-artifact without the perpetual bookkeeping. Note the ban's
  classic motivation in [A5] §2.6 — strike rows — was found to be
  misaimed during this record's verification (strikes attach to accounts,
  Context §4), which weakens the case for the ban beyond what A5 stated.

[A5]: ../plans/a5-identity-and-capabilities.md
[A11]: ../plans/a11-adrs-and-pr-plan.md
[D3]: 0003-transport.md
[D31]: 0031-id-account-subspace.md
[D33]: 0033-strike-ledger-standing.md
[D42]: 0042-canonical-simulation-architecture.md
[docs/08 §4]: ../08-persistence.md
[docs/08 §6]: ../08-persistence.md
[docs/11 §P6]: ../11-roadmap.md
[#407]: https://github.com/baadc0de/orrery/issues/407
[#427]: https://github.com/baadc0de/orrery/pull/427
