# A5 — Canonical identity and state capabilities (#401)

**Status:** decision proposal for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/401-a5` (based on `main` at `07d73d62`) · **Parents:**
[#401](https://github.com/baadc0de/orrery/issues/401) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md), [A2](a2-kernel-game-module-ownership.md),
[A3](a3-simulation-host-comparison.md) (+ the preserved
[second opinion](a3-simulation-host-second-opinion.md)) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)

`bevy_ecs::Entity` is a generational index into one world's allocator. It
cannot become durable identity. This document defines what does — and defines
the per-component capability policy that decides which state reaches
persistence, rollback, witnessing, replication and authority, as five
**independent** dimensions rather than one flag.

Two decisions reserved to this node by predecessors are settled here:
whether `CoreClass` survives (A2 §10.2), and whether `classify_component` —
implemented by three rulesets, called by nothing — gains a consumer or a
replacement (A2 §6). Accepting or amending anything below is the owner's call
(#395: propose, do not decide); ADR text belongs to A11 (#407).

Method, as in the predecessors:

- Every claim cites a file and line opened on this tree today. Where this
  document asserts a property is *enforced*, the **guarded stage** was broken
  (not the check line), the named check that died recorded with its real
  result line, the change reverted, and the pass re-confirmed (§9). One
  mutation **passed** and is recorded as the coverage gap it is rather than
  discarded quietly (§9 X2).
- What **exists**, what is **designed but unwired**, and what is **proposed
  here** never share a sentence.
- Where a decision belongs to another node — the rollback unit and canonical
  witness projection (A7, #403), the determinism envelope and gate bundle
  (A4, #400, in flight), schema-id governance and manifests (A8, #404) — it
  is named and not decided in passing (§8).

---

## 1. Ground truth inherited and re-verified

Each finding this document leans on was re-checked on this tree before use.

| # | Finding | Re-verification |
|---|---|---|
| I1 | Canonical rules state lives in `Executor`'s `BTreeMap<PersistId, R::CoreState>`, outside any app world; the BTreeMap choice is VC-4-motivated ("iteration order is observable through neighbour snapshots") | `crates/orrery_core/src/executor.rs:48-51`, comment at `:60-62` |
| I2 | `classify_component`: one defaulted definition, three impls, **zero call sites** | `rg classify_component crates gates clients` today: `orrery_core/src/ruleset.rs:298`, `orrery_conformance/src/ruleset.rs:242`, `orrery_games/src/regolith/mod.rs:129`, `orrery_games/src/skirmish/mod.rs:186`, nothing else |
| I3 | Query iteration order over an ECS world is allocation/archetype-dependent; a projection sorted by stable id agrees across orders — found independently by both A3 lanes | A3 §4 P1/P2; second opinion §1.3 P-2. Relied on as recorded; not re-run (prototype evidence, no repo delta since) |
| I4 | `scripts/core-gates.sh` gates a hardcoded crate list, not a graph property: `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` | `scripts/core-gates.sh:37`. Corollary re-verified fresh by mutation X5 (§9): the *transitive* closure of a gated crate is covered — `orrery_protocol` is not in the list, yet Bevy entering it fails clause 1 through `orrery_core`'s tree |
| I5 | D21 freezes `orrery_persistd`'s exports, not the `Ruleset` trait; D38(c) pins composition-time registration as the additive mechanism and says a *required* trait method "names D21 and pays its ADR" | `docs/adr/0021-ruleset-distribution.md:61-77`; `docs/adr/0038-at-rest-schema-versioning.md:161-169` |
| I6 | A3's recommendation (both lanes converge): canonical state stays in the engine-neutral per-entity executor; a dedicated `bevy_ecs::World` is admitted only behind the host seam on named triggers, one of which (T1/pilot precondition) is *this document's* policy decision | A3 §7; second opinion §3 V5 trigger T1, §8 item 3 |
| I7 | The witness hash is blake3 over one entity's canonical **quantized** encoding; nothing iterates a container into it | `orrery_core/src/ruleset.rs:324-330`; `executor.rs:126-128` |
| I8 | The at-rest schema machinery exists and is live: `SchemaVersion` per `ComponentTypeId` (`atrest.rs:76`), bootstrap rule absent==v0 (`atrest.rs:82` and module doc `:14-21`), framed `ComponentBag` with per-slot versions (`orrery_persistd/src/schema.rs:48-66`), envelope floor (`keyspace.rs:144`), fail-closed `MigrationRegistry` (`migration.rs:27-114`) | Opened and quoted throughout §7; liveness proven by mutations X3/X4 (§9) |

One briefing-text correction, recorded because the task's own rules demand it:
the brief for this node says A2 "asked what `classify_component` was for". A2
in fact went further — it assigned the hook's *role* (game-declared,
kernel-consumed; A2 §6) and reserved to A5 only whether the *signature and
enum* survive contact with a fuller policy model (A2 §6 open question 1,
§10.2). The question this document answers is therefore narrower and sharper
than the briefing implies.

---

## 2. The identity model

### 2.1 The identity classes that exist

The tree already operates a layered identity model. This section is
inventory; §2.2 onward is where decisions live.

| Class | Type | Scope and allocation | Where defined |
|---|---|---|---|
| Durable entity | `PersistId(u64)` | Universe-durable. "Never a Bevy `Entity`: this is the canonical id carried into every peer's world and the storage key for `world/` rows" | `orrery_protocol/src/persist.rs:38-54` |
| Transient entity | `EphemeralId { island, spawner: NodeId, seq: u32 }` | Island-scoped, spawner-partitioned, minted by local increment with no allocator and no round trip; collision-free by construction | `orrery_authority/src/ephemeral.rs:52-61` |
| Engine handle | `bevy_ecs::Entity` | World-local, generational, allocator-order-dependent. Appears in no protocol type (§2.2) | bevy_ecs |
| Rules build | `RulesetId { version, digest }` | Game-assigned; pinned into frames, claims, bundles, strike rows | `orrery_protocol/src/verifiable.rs:59-64` |
| Component type | `ComponentTypeId(u32)` | Game-assigned, no registry today (§4) | `orrery_core/src/ruleset.rs:76-77` |
| Component schema | `SchemaVersion(u32)` per `ComponentTypeId` | Game-allocated, monotone, never reused/gapped; orthogonal to `RulesetId.version` (D38(d)(3)) | `orrery_protocol/src/atrest.rs:76-80` |
| Account / item / node | `AccountId`, `ItemUid`, `NodeId` | Ledger and transport identity; out of A5's scope except as capability-dimension anchors (§5) | `persist.rs:157-198`; D3/D31 |

**Proposed as normative (N-1): these are the only identity classes, and every
entity in every runtime holds exactly one of the first three as its canonical
name.** A durable entity is a `PersistId` everywhere — wire, store, evidence,
replay. A transient entity is an `EphemeralId` everywhere it is shared. An
engine `Entity` names *only* rows of one world's local storage and may appear
in no encoded artifact that outlives that world. No fourth class (in
particular, no "provisional durable id" — §2.5) is admitted.

### 2.2 No engine entity in any durable format — shown, not asserted

The durable formats are enumerable. Each is keyed and addressed by
`PersistId` or a coarser id, and none has a field that could carry an
`Entity`:

| Durable format | Key / entity field | Evidence |
|---|---|---|
| `world/` rows | key = `(grid, cell, PersistId)` (`keyspace::world_key`); value = tag ‖ bag or tag ‖ tombstone | `orrery_persistd/src/keyspace.rs:109-146` |
| Journal records | `JournalRecord.entity: PersistId` ("never a Bevy `Entity`") | `orrery_protocol/src/persist.rs:205-227` |
| Bulk uplink | `DiffUplink.entity: PersistId` | `orrery_protocol/src/gateway.rs:371-393` |
| Evidence: frames, claims, bundles | `EntitySlice.entity`, `StateClaim.entity`, `EvidenceBundle.entity` — all `PersistId` | `orrery_protocol/src/verifiable.rs:146-231` |
| Ledger / receipts | `AccountId`/`ItemUid`/minted `Vec<PersistId>` in commit receipts | `persist.rs:650-698`; `keyspace.rs:1644+` |
| Goldens | chains over per-tick `state_hash` of `CoreState` — a `CoreCodec` encoding of types defined in Bevy-free crates | A3 G7 (`golden.rs:20-28`); I7 |

Four demonstrations carry this beyond enumeration:

1. **The defining crate cannot name the type.** Every struct above lives in
   `orrery_protocol`, which names no bevy crate (A1 §9 #1, re-opened:
   `Cargo.toml` deps are glam/iroh-base/serde/postcard/bytes/blake3/…).
   Mutation **X5** (§9) shows this is *gate-held, transitively*: adding
   `bevy_ecs` to `orrery_protocol`'s `[dependencies]` — without touching a
   line of Rust — fails `core-gates.sh` clause 1 with
   `core-gates: orrery_core has Bevy in its dependency graph`, because the
   gated core's `cargo tree` includes its protocol dependency. The durable
   vocabulary cannot acquire the ability to name `Entity` without a named
   gate dying.
2. **The one seam where an `Entity` approaches the durable path translates
   it away, explicitly.** The vendored replicon uplink emits
   `ComponentDiff { entity: Entity, fns_id, payload }`
   (`vendor/bevy_replicon/src/server/uplink.rs:60-67`). `feed_uplink` maps
   that `Entity` to the entity's replicated `PersistId` component and builds
   the `DiffUplink` from **`persist_id.0`**, dropping both the `Entity` and
   the `FnsId` (`orrery_persist_client/src/feed.rs:56-97`; the component and
   its doc — "never a Bevy `Entity`, which is not stable across peers or
   restarts" — at `feed.rs:27-41`). The only `Entity`-keyed structure in that
   file is `UplinkSeq.next: HashMap<Entity, u64>` (`feed.rs:45-49`), an
   in-memory resource that is never serialized.
3. **The canonical execution path never holds one.** `Executor` state is
   `BTreeMap<PersistId, R::CoreState>` (I1); `StateView::entity()` is a
   `PersistId` supplied by the executor (`ruleset.rs:85-118`);
   materialization identifiers are `PersistId`s derived by the emitting step
   (`ruleset.rs:210-229`). `CoreState` types are defined in crates whose
   dependency graphs the gate keeps Bevy-free (I4), so a `CoreState` field of
   type `Entity` cannot compile.
4. **The remaining corridor is named, not waved off (gap G-1).** A *game's
   replicated Bevy component* with an `Entity` field would serialize entity
   bits into `ComponentDiff.payload` and ride into the journal as opaque
   bytes. No mechanical check watches component field types today. This is
   precisely a capability-policy job: §5's declaration rule makes a persisted
   component's schema a declared, versioned codec — and §5.4 row IV-7 names
   the engine-handle-in-schema combination invalid. Until a declaration
   registry exists, G-1 is held by review alone. Stated as a gap, not
   laundered.

### 2.3 `PersistId ↔ Entity` mapping

**What exists.** The mapping is presentation/replication-side only, and there
are three independent shapes of it today:

- `orrery_persist_client::feed::PersistId` — a replicated Bevy component,
  written only by the entity's owner, described in its own doc as "the
  canonical Bevy `Entity` ↔ persistent-id mapping on every peer"
  (`feed.rs:27-41`). Lookup direction `Entity → PersistId` is a component
  read; the reverse is a query scan.
- The p1-swarm bots' harness mirror: the bot steps `Executor<Regolith>` and
  writes results onto its ECS mirror entity (`gates/p1-swarm/src/bot.rs:669-745`).
- `orrery_predict`'s `PredictedBy { authority: NodeId, persist_id: PersistId }`
  component tying a predicted entity to the canonical id its claims are
  reconciled against (`orrery_predict/src/wiring.rs:79-93`); the monitor keys
  tracks by `(NodeId, PersistId)` (`monitor.rs:45-52`).

The canonical side needs no map at all: the executor's own storage is keyed
by `PersistId` (I1). There is no global bidirectional index resource, and
under A3's recommendation none is needed for correctness today.

**Proposed as normative (N-2), for any present or future world that mirrors
canonical entities (including a triggered ECS host under A3-V5/T1):**

1. **One index per world, owned by the host seam.** The `SimulationHost` seam
   (A3 §7.1; brief phase 3 "stable-ID lookup") owns the single
   `PersistId ↔ Entity` index of the world it drives. Module code resolves
   through it; a second map is a bug, because two maps *will* disagree
   during despawn/rekey races.
2. **The index is a rebuildable projection, never an authority.** It is
   populated from replicated `PersistId` components (exactly `feed.rs`'s
   shape today), reconstructible from world contents alone; it is never
   encoded, never hashed, never persisted, and never consulted by canonical
   rules (which receive `PersistId` directly, I1).
3. **Iteration over the index is not canonical order.** Any projection that
   enumerates entities for hashing, witnessing or golden emission sorts by
   `PersistId` first — I3's finding made policy. Today's hash is immune
   because nothing iterates a container into it (I7); the rule exists so
   that property survives any storage change.
4. **Lookups of despawned ids miss.** A tombstoned `PersistId` (§2.6)
   resolves to no `Entity`; holding a stale `Entity` past despawn is a bug
   the generational index itself catches (the generation dies with the row),
   which is the one job `Entity` is good at and the reason it needs no help.

### 2.4 Allocation of `PersistId` — and the collision gap

Three allocation paths exist or are designed:

1. **Cluster mint inside intent transactions** — exists. The FDB counter
   `pid/{grid}/next` (`keyspace.rs:1616-1642`: mutated only via atomic add;
   abandoned grants leave permanent, unreclaimed gaps), minted ids returned
   in commit receipts (`persist.rs:650-657`, `:695-698`: "minted `PersistId`s
   … in op order"). Gateway-side grant replenishment is instrumented
   (`intent/stages.rs:110`, `alloc_refills`).
2. **Peer-side journaled block grants** — designed, unwired. The `PersistId`
   doc promises "peer-side from a journaled block grant (contiguous ranges,
   default 4096, usable offline)" (`persist.rs:41-44`). No grant issuance,
   holding or consumption code exists in any crate (`rg` for block-grant
   machinery finds the doc comment and the `keyspace.rs:1618-1625` counter
   comment only). Designed-but-unwired, exactly like `validate_intent`
   (A1 §1.5).
3. **Game-derived materialization ids** — exists. `Ruleset::materialize`
   supplies fully described entities whose identifiers "are derived by the
   emitting step from its own replayable inputs; they are never allocated
   from executor population or creation order" (`ruleset.rs:210-215`,
   `:272-278`) — e.g. `(parent, generation, slot)`. This is what makes
   isolated single-entity replay reproduce the same descriptions (A2 §4
   CC-4).

**Finding (gap G-2): paths 1 and 3 share the u64 namespace with no
partition.** Nothing prevents a step from deriving an id the cluster has
minted or will mint. Today this is latent — materialized entities live in
executor worlds and the games derive from small parent ids — but the moment a
materialized entity is *persisted*, a derived id colliding with a minted one
corrupts a `world/` row silently (the journal is keyed `(entity, tick)`
last-writer-wins, `persist.rs:200-205`; the actor replaces the bag wholesale,
`actor.rs:1295-1300`).

**Proposed (N-3): derivation composes with granting.** A derived identifier
must be a pure function of replayable inputs *whose range is a granted
block*: the emitting entity's state carries (or its spawn record carried) a
block-grant base, and `materialize` derives `base + f(replayable inputs)`
within the block. This keeps both properties at once — replay-stable
derivation (path 3's requirement) and cluster-coordinated uniqueness (path
1's requirement) — and it is the same mechanism path 2 already promises, so
it creates no new machinery class. Alternative considered: static partition
of the u64 space (high-bit split "minted vs derived"); rejected as proposal
because derived ids still collide with *each other* across emitters without
a per-emitter base, which the grant supplies anyway. **Owner decision**: this
touches docs/08 §6 vocabulary and the unbuilt grant path; flagged to A11, and
nothing in the current tree has to move for it.

### 2.5 Predicted and transient identity

**What exists.**

- **Transient entities have a complete, shipped identity story.**
  `EphemeralId` is minted allocator-free (spawner-partitioned local
  increment, `ephemeral.rs:52-61`, registry `spawn` at `:217-239`), attached
  to the ECS entity as the `Ephemeral` component (`:342`), and marked
  `IslandAuthoritative` — deliberately *not* `LocallyAuthoritative`, "so an
  ephemeral entity carrying this one can never be persisted no matter what
  game code does with it" (`:344-352`). That claim is enforced: mutation
  **X1** (§9) breaks the yield path to install the persistence marker and
  the named test
  `an_ephemeral_entity_never_gets_the_persistence_uplink_marker` dies.
  Island merge and epoch bump cost no renumbering because uniqueness never
  depended on the island field (`:198-201`).
- **Predicted views of existing canonical entities carry the canonical id.**
  A lightyear-predicted entity is tied to its subject by
  `PredictedBy { authority, persist_id }` (`wiring.rs:79-93`) — prediction
  never mints identity; it annotates a presentation-world `Entity` with the
  durable id it shadows. Corrections address the same `PersistId`
  (`correction.rs`, `monitor.rs:45-52`).
- **Predicted spawns of *durable* entities do not exist as a mechanism.** No
  pre-spawn/prespawned-entity flow is wired (`rg -i prespawn` over first-party
  crates: nothing); durable creation happens cluster-side in intent receipts
  or executor-side through materialization.

**Proposed as normative (N-4): predicted and transient spawns use
`EphemeralId`; durable identity is never provisional.** A client that
fire-and-predicts a projectile already has the right tool — free minting,
island-scoped, structurally unpersistable. A client that optimistically
renders the *outcome* of a durable creation (crafting output, loot) renders
it against the intent's pending receipt and adopts the receipt's minted
`PersistId` on commit; it does not invent a `PersistId` and renumber later.
What makes a provisional-then-renumbered durable id **invalid** rather than
merely inelegant: journal records, claims and evidence address entities by
`PersistId` under a single-writer, `(entity, tick)`-idempotent discipline
(`persist.rs:200-205`) — bytes emitted under a provisional id are bytes the
renumbered entity's history does not contain, so replay and adjudication of
the window spanning the rename would be unproducible. The identity classes
of §2.1 are closed precisely to keep this door shut.

### 2.6 Tombstones and stale references

**What exists — the despawn lifecycle is built and discriminated:**

- `RecordKind::Despawn` journal records ("An entity despawn (tombstone)",
  `persist.rs:135-149`).
- The durable marker: `world/` values are `LIVE_TAG ‖ bag`,
  `LIVE_VERSIONED_TAG ‖ floor ‖ bag`, or `TOMBSTONE_TAG ‖ postcard(Tombstone)`
  (`keyspace.rs:109-146`); `Tombstone { cell, tick, gc_deadline_ms }`
  (`actor.rs:39-48`) is GC'd by the checkpoint pass after its deadline, and
  a re-spawn of the same id cancels the marker (`actor.rs:1319-1325`,
  `cancel_tombstone` at `:100-115`). Rows a moving entity leaves behind are
  tracked as superseded and cleared (`actor.rs:50-59`, fold at
  `:1308-1318`).
- Discrimination is enforced, not assumed: mutation **X4** (§9) makes the
  tombstone wear the live tag and two named tests die, including
  `tombstone_value_roundtrips_and_is_distinct_from_live`. A tombstone has no
  schema and reads as "not a live row" everywhere
  (`world_value_schema_floor` → `None`, `keyspace.rs:181-202`).
- Sharp edge, stated: **`PersistId` reuse across a despawn is legal today**
  — the actor codes for it ("A re-spawn (id reuse across a despawn)",
  `actor.rs:1319`). Under N-3 (granted ranges) reuse remains possible only
  by the id's original minter, which is the tolerable half of the hazard;
  a *policy* forbidding reuse entirely is attractive for evidence hygiene
  (a strike row citing entity E should not later mean a different E) but is
  an owner call with retention-window implications — flagged, not decided.

**Stale references, by layer:**

- **Inside canonical state**, a reference is a `PersistId` as plain data. A
  step that names a despawned neighbour gets `None` from
  `StateView::neighbor` (`ruleset.rs:120-136`) — a missing neighbour and a
  despawned one are indistinguishable *by design*, since the adjudicator
  installs one entity and its neighbour map is empty (A1 §5.2). Events
  addressed to dead entities are dropped by the authority that owns input
  admission. Rules must therefore treat every cross-entity reference as
  possibly-dangling; nothing at the identity layer can promise otherwise
  without breaking isolated replay.
- **At the interest layer**, staleness is what D25's `Expire` fan-out
  bounds: expiries reach the holder *and* the cell's interest set with a
  defined non-holder semantics and an amplification bound
  (`docs/adr/0025-expire-fan-out.md`; implemented per #128/#126).
- **At the durable layer**, staleness is the tombstone/superseded machinery
  above plus lease fencing (`Epoch`, `persist.rs:56-76`) so zombie writers
  cannot resurrect rows.

### 2.7 Cross-cell and cross-island references

- **Cross-cell**: the `world/` key carries the entity's own cell, so a cell
  change writes a new key; the vacated key is tracked and cleared
  (`actor.rs:50-59`); cross-cell movement commits ride
  `RecordKind::Rekey` with a versioned `EntityRekey` payload whose v2
  refuses v1 rather than guessing (`persist.rs:277-308`). References *across*
  cells are just `PersistId`s — the id is cell-free; only the storage key is
  cell-scoped.
- **Cross-island**: `PersistId` is island-free and universe-scoped, so
  durable references cross islands trivially. `EphemeralId` carries its
  minting island but its uniqueness is spawner-scoped, so island merge and
  epoch bumps need no renumbering (`ephemeral.rs:195-201`); an ephemeral
  reference crossing an island boundary is meaningful only while both peers
  share replication interest — it is transient by class, and N-4 forbids
  laundering it into a durable one.
- **Cross-grid**: `PersistId` allocation is grid-scoped
  (`pid_next_key(grid)`, `keyspace.rs:1627-1642`) — nested grids allocate
  from independent counters. A cross-grid durable reference therefore needs
  the `(GridId, PersistId)` pair; every durable row already carries both
  (`JournalRecord.grid`, `persist.rs:211-213`). Proposed rider to N-1:
  in-state references that may cross grids store the pair, not the bare id.

---

## 3. Where identity meets the A3 recommendation

Under A3's adopted position the canonical store stays the per-entity
executor, so this document's identity model is mostly *already true* and its
job is to keep it true under change:

- The `PersistId ↔ Entity` index rules (N-2) bind today's presentation
  worlds and become preconditions for the triggered ECS pilot: A3 requires
  "A5's component-policy decision" before any canonical byte moves (A3 §7.4);
  this document supplies it (§5) and adds the identity-side preconditions —
  sorted-by-stable-id projection (N-2.3) and host-owned single index
  (N-2.1).
- I3 is the load-bearing fact: any future world-hosted projection that
  iterates must sort by `PersistId` before hashing or emitting. This is not
  hypothetical hygiene — both A3 lanes measured iteration order varying with
  insertion order on the pinned `bevy_ecs 0.19`.

---

## 4. Component schema identity

**What exists.** `ComponentTypeId(u32)` is game-assigned with no registry —
Regolith hardcodes `components::STATE = ComponentTypeId(1)`
(`orrery_games/src/regolith/mod.rs:79-84`); Skirmish and the conformance
ruleset classify against their own constants. `SchemaVersion` is per
component type, game-allocated, monotone, never reused or gapped, and
**orthogonal to `RulesetId.version`** — "a rules hotfix bumps no schema, a
schema bump ships without a rules change, and neither number is ever derived
from the other" (`atrest.rs:22-27`; D38(d)(3)). The framed bag carries
`(component, schema_version, payload)` per slot
(`schema.rs:48-66`), and the envelope floor summarizes the bag for readers
that cannot open it (`keyspace.rs:124-146`; `schema_floor` at
`schema.rs:84-91`).

**Proposed as normative (N-5): the schema id of record is the pair
`(ComponentTypeId, SchemaVersion)`, declared at composition time, and it is
independent of Rust type names, `TypeId`, reflection registration, replicon
`FnsId`, and archetype layout.** Everything in that list is
build-or-registration-order-dependent; the pair is the only component naming
that survives a recompile. The tree already behaves this way at the durable
layer (the bag stores the pair; the uplink seam drops `FnsId`,
`feed.rs:82-97`); N-5 extends it to every capability declaration in §5.

**Governance stays A8's.** Who allocates `ComponentTypeId` values across
modules, collision detection between modules, and how the pair enters the
compatibility manifest are module-manifest questions (A2 §6 flagged them to
A8; unchanged here). What A5 fixes is only the *shape*: capabilities and
schemas key off the pair, so A8's registry has one namespace to govern.

---

## 5. The capability model

### 5.1 Reflection is not the schema (acceptance item, shown)

- No first-party crate uses reflection: `rg "Reflect"` over `crates/*/src`
  matches nothing. `bevy_reflect` appears as a *listed direct dependency* of
  three crates (`orrery_spatial/Cargo.toml:27`, `orrery_net/Cargo.toml:24`,
  `orrery_persist_client/Cargo.toml:33`) with zero uses in their sources —
  whether those entries are needed for feature unification with the vendored
  replicon or are simply dead weight was not settled here; recorded as a
  question, not a finding. `bevy_ecs` 0.19's default features include
  `bevy_reflect` (A3 §4 version note), so the library rides the graph of
  every Bevy-side crate regardless.
- Every durable and wire encoding in the tree is a *declared* codec:
  hand-written `CoreCodec` for canonical state (`ruleset.rs:19-46` — field
  order fixed, map-iteration order banned), positional postcard over
  explicit wire structs for protocol types, the framed bag + version
  trailer for at-rest families (`atrest.rs:36-70`). Replicon's uplink
  payload is produced by *registered* per-component serialize functions,
  not reflection (`vendor/bevy_replicon/src/server/uplink.rs:5-15`,
  `:115-158`).
- **Proposed as normative (N-6):** reflection may serve tooling
  (inspectors, debug dumps) but may never *define* an encoding. Any
  reflect-assisted path that produces bytes for wire or store must go
  through an explicit mapping to a declared `(ComponentTypeId,
  SchemaVersion)` codec — "Reflection may assist tooling but should not
  silently define the persistence or wire format" (brief, adopted as
  written). Under A3's outcome nothing needs to change to satisfy N-6
  today; it exists to bind the triggered pilot.

### 5.2 The five dimensions

**Proposed (N-7): every component type a module declares carries five
independent capability dimensions**, declared as data at composition time
(the registration idiom D38(c) pins: like `MigrationRegistry::declare`,
`migration.rs:53-56`, and `AdjudicationExecutor::register`,
`adjudication.rs:350`), keyed by `ComponentTypeId` (N-5). Names below are
for this document; the construct (one registry vs several) is A8's, and the
brief's own warning against "a single overgeneralized policy object" stands.

| Dim | Values | Meaning · today's consumer (existing or named-unwired) |
|---|---|---|
| **P** persistence | `P0` none · `P1` bulk · `P2` critical | `P1`: journal/checkpoint path, last-writer-wins per `(entity, tick)` under lease fencing (`persist.rs:200-205`; `DiffUplink.lease_id`, `gateway.rs:386-392`). `P2`: mutated only inside attested intent transactions (`intent/mod.rs:152-155`); rows like ledger balances. `P0`: never leaves the world |
| **R** rollback | `R0` excluded · `R1` included | Whether prediction resimulation and post-adjudication correction restore it. Unit and mechanism are **A7's** (#403); this dimension only records membership, exactly as A2 row 7 assigned |
| **W** witness | `W0` unwatched · `W1` invariant-checked · `W2` replay-adjudicated | `W1`: stage-1a `Invariant<CoreState>` predicates on received samples (`witness.rs:663`; `invariants.rs:118`) — "the only validation most bulk-class state ever gets" (`ruleset.rs:302-312`). `W2`: logged inputs, signed claims, isolated re-execution (`replay.rs:106-130`) |
| **N** replication | `N0` none · `N1` interest-replicated | `N1`: replicon under AOI/interest with hysteresis (`orrery_spatial`), owner-written (single-writer). Note the witness **frame/claim channel is not this dimension** — evidence flows to witness-set peers regardless of interest membership, which is what makes W and N independent (§5.3) |
| **A** write authority | `A0` local · `A2` island-weak · `A1` lease-holder · `A3` cluster-transaction | Who may mutate: nobody but this process (`A0`); the in-island total order with no fence (`A2`, `ephemeral.rs:82-148`); the fenced lease holder (`A1`, D7); only an FDB intent transaction (`A3`, `intent/mod.rs:152-155`) |

Defaults are the zeros, and the zeros fail closed. This is already the
tree's instinct twice over: an unclassified component defaults to `Cosmetic`
"never persisted rather than one silently admitted" (`ruleset.rs:293-297`),
and a bag slot whose component is undeclared **refuses to load** rather than
passing through (`migration.rs:80-85`; proven live by mutation X3, §9 —
skipping undeclared slots silently kills
`missing_registration_refuses_stale_checkpoint` by name). N-7 keeps both
behaviours and generalizes them: *no declaration, no capability*.

The brief lists three further per-component concerns — privacy/visibility
filtering, maximum encoded size, and migration functions. Migration is
already a per-`(ComponentTypeId, from_version)` registration
(`ComponentMigrator`, `orrery_core/src/migration.rs:18-19`) and needs no new
dimension. Privacy and size caps are real but are *attributes of N and P
respectively* (a relevance filter; an admission bound like
`MAX_OPS_PER_INTENT`, `intent/mod.rs:189-202`) rather than independent
axes; folding them in as axes would manufacture combinations nothing
consumes. Recorded so A8 can revisit if a consumer appears.

### 5.3 Independence, demonstrated from the tree

The capability sets are not identical, and the tree already contains the
witnesses:

- **Witnessed but not replicated.** Core state reaches witnesses as signed
  frames and claims over the witness link (`WitnessMsg::Frame`; harness
  producers `bot.rs:1103-1137`), not through replicon interest. A watched
  entity outside a peer's AOI is still checked. (W2 ∧ N0 is coherent.)
- **Replicated but never persisted.** Projectiles: `N1` under island
  authority, structurally excluded from the uplink (`IslandAuthoritative`
  vs `LocallyAuthoritative`, `ephemeral.rs:344-352`; mutation X1). (N1 ∧ P0.)
- **Persisted but not adjudicated.** Bulk journal rows get stage-1
  invariants only — `CoreClass::Bulk`'s own definition, "Persisted but not
  adjudicated" (`ruleset.rs:63-71`). (P1 ∧ W1 ∧ ¬W2.)
- **Persisted but not rolled back.** Ledger rows are transaction-final;
  the correction path re-executes and *corrects* entity state
  (`AdjudicatedState`, `adjudication.rs:282-298`) but a committed credit is
  never rewound — reversal is a compensating transaction. (P2 ∧ R0.)

One flag cannot carry this. That is the argument the brief makes and the
tree confirms.

### 5.4 Valid profiles and invalid combinations

Named valid profiles (the diagonal points, plus the two the enum cannot
express — see §6):

| Profile | P | R | W | N | A | In-tree example |
|---|---|---|---|---|---|---|
| **Core** (verifiable) | P1/P2 | per A7 | W2 | N1 | A1 | `RegolithState` via `components::STATE` (`regolith/mod.rs:79-84`, classified Core at `:129-135`) |
| **Bulk** | P1 | R0 | W1 | N1 | A1 | docs/06 §2's bulk class; terrain deltas would be Bulk if durable terrain existed (`RecordKind::TerrainDelta` removed in v1 by [D51](../adr/0051-v1-terrain-is-not-durable-state.md)) |
| **Cosmetic-local** | P0 | R0 | W0 | N0 | A0 | UI/selection state; anything undeclared (the default) |
| **Ephemeral-shared** | P0 | R0 | W0 | N1 | A2 | Projectiles/VFX under `EphemeralId` (§2.5) |
| **Critical/ledger** | P2 | R0 | W0* | N0 | A3 | Balances, item ownership; *audited by receipts and the anti-dupe row, not by replay |

Invalid combinations, each with the mechanism that makes it invalid rather
than merely unusual:

| # | Combination | Why it is invalid |
|---|---|---|
| IV-1 | `W2` without `A1` (single fenced writer) | Replay adjudication verifies a **subject-signed** claim chain (`replay.rs:325-339`; `StateClaim.sig`, `verifiable.rs:189-209`). Island-weak (`A2`) entities have no fence and no chain — contested writes have no single subject to hold to; cluster-written (`A3`) rows have no step to re-execute. No signer, no verdict |
| IV-2 | `W2` without deterministic canonical encoding (`CoreCodec` + `Quantized`, VC-1..8) | The claim commits to blake3 over canonical quantized bytes (I7). Nondeterministic or unstable encoding makes every honest re-execution a false deviation — the witness convicts everyone, which is worse than watching no one ("false deviation", `ruleset.rs:22-27`) |
| IV-3 | `P2` with any writer but `A3` | The FDB transaction is "the sole authority" for critical rows (`intent/mod.rs:152-155`); the single-ownership row *is* the anti-dupe invariant (`persist.rs:190-198`). A lease-holder journaling a balance bypasses read-check-write: duplication by construction |
| IV-4 | `P1`/`P2` on an `EphemeralId` entity | Transient identity has no durable row to write. Mechanically enforced today: the uplink keys off `LocallyAuthoritative`, ephemerals carry `IslandAuthoritative` (mutation X1 kills the named test). N-4 keeps the classes closed so this cannot be reintroduced by a marker mix-up — though see §9 X2 for the uplink-side coverage gap |
| IV-5 | `R1` with `P2` | Rolling back transaction-final state re-plays committed durable effects — a rewound-then-recommitted credit is a dupe machine. Corrections to critical state are compensating transactions through the same envelope, never rewind. (The converse, `R1 ∧ P1`, is A7's to shape, not invalid) |
| IV-6 | `N1` with `A0` | Replicating state nobody holds authority to write breaks single-writer: receivers have no rule for whose value wins. Everything replicated today is owner-written (`feed.rs:27-33`; replicon uplink is owner-side by construction) |
| IV-7 | Any capability above the zeros for a schema embedding an engine handle (`Entity`, `ComponentId`, `FnsId`, archetype/row indices) | Engine handles are allocator-local and generation-dependent; their bytes mean nothing to another world, a restart, or a replay (§2.2). This is gap G-1's guard: the declaration that grants P/N/W must declare a schema, and a schema containing engine handles is refused at declaration time. Until the registry exists this row is review-held |
| IV-8 | Any capability above the zeros without a declared `(ComponentTypeId, SchemaVersion)` | "No declaration, no capability" (§5.2). Existing behaviour at both ends: default-Cosmetic (`ruleset.rs:293-297`), fail-closed migration (X3) |

Inert-but-legal, named so nobody "fixes" them into the invalid table: `W1`
with an empty invariant slice (the trait's own default — "correct but slower
to notice", `ruleset.rs:302-312`); `P0 R0 W0 N0 A0` on a *declared*
component (declaration without capability is a no-op, not an error);
`W2 ∧ N0` (§5.3 — the witness channel is not replication).

### 5.5 Who consumes each dimension

The policy is game-authored, kernel-consumed (A2 §1's policy-handover
category, rows 6/8/14):

- **P** routes the uplink and write classes (persist-client scheduler and
  gateway; today's consumer-in-waiting that docs/06 §2 promised —
  "`orrery_persist_client` uses it to route bulk diffs vs. intents" — still
  present-tense-stale, `docs/06-verifiable-core.md:210`, as A2 §9 recorded).
- **R** feeds A7's rollback unit membership; consumed by
  prediction/correction (`orrery_predict`).
- **W** feeds witness attention (which entities/components get executors and
  claims vs invariants only) — docs/06's second promised consumer.
- **N** feeds replication registration and relevance (spatial/interest).
- **A** feeds admission: which write path will accept a mutation
  (lease-fenced diff, island claim, intent op).

---

## 6. The verdicts: `CoreClass` and `classify_component`

### 6.1 `classify_component` is replaced, not wired

**Decision (proposed): the capability policy does not give
`classify_component` a consumer. It replaces it.** The hook's *shape* —
game-declared, kernel-consumed, keyed by `ComponentTypeId` — is confirmed
and kept (A2 §6 assigned exactly this role). The hook's *form* — a method on
`Ruleset` returning one three-valued enum — is wrong for the policy §5
defines, for three reasons grounded above:

1. **One value cannot carry five independent dimensions.** The in-tree
   witnesses of §5.3 are inexpressible: an ephemeral projectile
   (replicated, never persisted, island authority) and a local UI component
   are both `Cosmetic`; a ledger-backed row (critical persistence, no
   replay) has no class at all. Wiring consumers to the enum would encode
   the diagonal as law right at the moment this node establishes the space
   is wider than the diagonal.
2. **Code where data belongs.** A capability declaration must reach the
   compatibility manifest (A8) and the at-rest reader (persistd), neither of
   which links game code by default ("registering a build means linking a
   `Ruleset`", `bin/persistd.rs:1261-1263` — and most rows must be readable
   *without* that). The registry idiom already in the tree
   (`MigrationRegistry::declare` + `AdjudicationExecutor::register`,
   D38(c)) carries declarations as data at composition time; a trait method
   requires calling into the build to learn a static fact.
3. **Retirement is cheap now and expensive later.** Zero call sites (I2)
   means no consumer migrates. Three first-party impls override the
   defaulted method (`regolith/mod.rs:129`, `skirmish/mod.rs:186`,
   `conformance/ruleset.rs:242`) and would be deleted with it — a
   compile-visible, first-party-only change. `Ruleset` is not in D21's
   frozen table (I5), and removing a *defaulted* method is not the
   "required method" branch D38's honesty note prices; still, any trait
   surface change is proposed through A11 and lands at the owner's pleasure
   (and, per G15/A1 §7.3, after the P4 digest window, since `orrery_core`
   and `orrery_games` are digest crates).

Sequencing rider: the removal is *last*, not first — the method stays until
the declaration registry exists and the three impls' facts are restated as
declarations, so at no point does the tree hold less classification
information than it does today.

### 6.2 `CoreClass` survives as vocabulary, not as datum

**Decision (proposed): `CoreClass` survives — as the derived name of a
capability profile, never as an authored, stored, or routed-on value.**
"Core", "Bulk" and "Cosmetic" are the three load-bearing macro-profiles of
§5.4 and the documentation set speaks them fluently (docs/06 §2, D10, D11);
renaming the concepts away would cost real communication for no modelling
gain. But the enum ceases to be the source of truth: a validator may
*compute* a profile name from a declaration's five values (and refuse
declarations that match no known profile unless explicitly marked novel —a
cheap tripwire for typo'd policies), and prose may say "core-class state";
nothing persists, hashes, or branches on the enum.

**Consequence for A3's hybrid.** H1/V5's tier boundary was scored "on an
unwired hook" with the explicit caveat that if A5 replaces `CoreClass` with
per-dimension policies "H1's routing wall needs re-derivation" (A3 §11.3;
second opinion T1). The re-derivation is one sentence: **the tier predicate
is `W2` (replay-adjudicated), not `CoreClass::Core`.** State with `W2`
lives in the per-entity executor with every structural guarantee (I1, I7);
everything else is eligible for whatever storage the host seam chooses.
The boundary survives; it is now keyed to the one dimension that actually
forces the executor's structure (isolated single-entity replay), which is
*stronger* than the enum it replaces — a component could have been marked
`Core` for persistence-priority reasons without needing replay semantics.

---

## 7. Unknown-component and version handling

Mostly built; inventory first, gaps after.

**Exists and mutation-proven:**

- **Bootstrap rule**: bytes without a version field are v0 — "not unknown,
  not rejected, not guessed from its shape" (`atrest.rs:14-21`, `SCHEMA_V0`
  at `:82`); decidable by the trailer construction (`:36-70`). Old rows
  stay readable forever; the migration chain starts at 0.
- **Per-component versions, per-bag floor**: slots carry
  `(component, schema_version, payload)` (`schema.rs:48-58`); the envelope
  floor is *derived* from the bag ("a row whose floor disagrees with its
  bag is a bug the bag itself convicts", `keyspace.rs:124-143`), readable
  without decoding game types (`world_value_schema_floor`,
  `keyspace.rs:181-202`); tombstones have no schema (X4's second dead test:
  `the_three_world_value_tags_are_distinct_and_only_live_ones_carry_schema`).
- **Unknown component ⇒ refuse, not guess**: `migrate_bag` errors with
  `UnregisteredComponent`; "Components must be declared even when their
  current version is zero. That makes an accidentally empty registry fail
  closed instead of interpreting a stale payload as current"
  (`migration.rs:20-30`, `:80-85`). Mutation **X3**: skipping undeclared
  slots kills `missing_registration_refuses_stale_checkpoint` by name.
- **Future version ⇒ refuse** (`FutureVersion`, `migration.rs:87-93`);
  steps are adjacent, monotone, pure functions keyed
  `(component, from_version)` (`migration.rs:58-71`;
  `ComponentMigrator`, `orrery_core/src/migration.rs:18-19`); a missing
  step is a hard error (`MissingStep`, `:95-101`). Wire twins exist:
  `EntityRekey::VERSION = 2` refuses v1 (`persist.rs:277-283`);
  `protocol_accepted` is exact equality (#395 constraint — version skew
  refuses the session before bytes flow).

**Gaps (named, with owners):**

- **G-3: the bulk uplink makes no schema statement.** `DiffUplink.payload`
  is a replicon-serialized component value with no `ComponentTypeId` and no
  `SchemaVersion` (`feed.rs:82-97` drops `fns_id`; `gateway.rs:382-383`).
  The actor consequently resets a diff-overwritten bag's floor to v0 with a
  documented apology: "a peer that makes no schema statement … When a
  producer starts framing its bags … the declared floor arrives on the
  uplink and this becomes a read of it" (`actor.rs:1300-1308`). The framed
  `ComponentBag` has **no production writer** — its encoder is exercised by
  persistd's own migration/keyspace paths and tests, not by any client
  (`rg ComponentBag` finds persistd + `orrery_seed` only). §5's P dimension
  cannot route what the wire does not name; the framed-bag producer (W1's
  other half) is therefore a precondition for wiring P, and belongs to the
  same implementation package as the capability registry. Flagged to
  A8/A11.
- **G-1** (§2.2): no mechanical guard against engine handles inside
  replicated component payloads; IV-7 is the policy-level fix, review holds
  it until then.
- **Module removal with persisted data present** (brief's test list) is
  answered by the same fail-closed rule: rows whose components no build
  declares refuse to load rather than silently dropping slots. Whether an
  *operator* override exists (read-and-quarantine) is A8 manifest policy.

---

## 8. Reported rather than forced

1. **The rollback unit and the canonical witness projection are A7's.** The
   R dimension records membership only; IV-5 constrains one corner
   (`R1 ∧ P2`) because the *transaction envelope*, not the rollback design,
   makes it incoherent. If A7 lands a unit that wants component-subset
   granularity, the R dimension is already per-component and nothing here
   moves.
2. **Determinism enforcement is A4's** (#400, in flight in a sibling lane).
   IV-2 names the *dependency* — W2 requires the deterministic envelope —
   and deliberately does not specify how the envelope is enforced.
3. **Schema-id allocation and manifests are A8's** (§4); the capability
   registry construct (one vs several, trait vs data) likewise.
4. **Owner decisions surfaced**: N-3 (granted-range derivation) touches
   docs/08 vocabulary; §2.6's id-reuse-after-despawn policy; §6.1's
   eventual removal of a defaulted trait method (via A11, post-P4-digest);
   whether the three `bevy_reflect` Cargo entries are needed (§5.1).

---

## 9. Mutation log (break stage → named check dies → revert → passes)

Baselines were recorded before each mutation; failing runs produced real
result lines; no mutation landed on both sides of an equality; every revert
re-ran the check and passed. X2 is a mutation that **passed** and is
recorded as such.

| # | Guarded stage broken | Named check | Observed | Reverted |
|---|---|---|---|---|
| X1 | `process_island_claims` yield branch made to insert `crate::LocallyAuthoritative` on the ephemeral entity (marker reuse — the exact failure the D7 §6 split exists to prevent; `ephemeral.rs:446-452`) | `cargo test -p orrery_authority --lib` | `ephemeral::tests::an_ephemeral_entity_never_gets_the_persistence_uplink_marker` FAILED (panic at its `no ephemeral path may ever install the persistence uplink marker` assertion); `36 passed; 1 failed` | `37 passed; 0 failed` |
| X2 | `feed_uplink`'s `LocallyAuthoritative` guard removed (`feed.rs:66-69` replaced with a no-op) — the uplink-side half of IV-4 | `cargo test -p orrery_persist_client --lib` | **`95 passed; 0 failed` — the mutation survived.** Cause, from the test body: the `non_authoritative_entities_are_ignored` fixture entity (`feed.rs:152-170`) also lacks `Authority`/`AuthorityPhase`, so the *component-query* guard drops it before the marker guard is consulted; the marker clause is shadowed and unpinned. Recorded as proving nothing about that clause — the property currently rests on the conjunction of three checks, of which only the conjunction is tested. A test spawning `PersistId + Cell + Authority + AuthorityPhase::LocalGranted` *without* the marker would pin the clause; left as a finding (this branch is docs-only) | `95 passed` before and after; source restored byte-identical |
| X3 | `MigrationRegistry::migrate_bag` made to silently `continue` past undeclared components instead of refusing (`migration.rs:80-85`) | `cargo test -p orrery_persistd --lib migration` | `migration::tests::missing_registration_refuses_stale_checkpoint` FAILED; `5 passed; 1 failed` | `6 passed; 0 failed` |
| X4 | `encode_tombstone_value` made to wear `LIVE_TAG` (`keyspace.rs:232-239`) — a despawn marker masquerading as a live row | `cargo test -p orrery_persistd --lib keyspace` | 2 named failures: `tombstone_value_roundtrips_and_is_distinct_from_live`, `the_three_world_value_tags_are_distinct_and_only_live_ones_carry_schema`; `58 passed; 2 failed` | `60 passed; 0 failed` |
| X5 | `[dependencies.bevy_ecs]` appended to `crates/orrery_protocol/Cargo.toml` — the crate defining every durable format acquires the ability to name `Entity`, without touching a line of Rust | `./scripts/core-gates.sh` | `core-gates: orrery_core has Bevy in its dependency graph`, exit 1 — the ungated protocol crate is caught **transitively** through the gated core's `cargo tree`. Precision note: a durable-format crate that core did *not* depend on would not be caught (I4's corollary; same class as A3 G9) | All four clause notes print, `verifiable-core static gates pass`, exit 0 |

A1's M1–M8, A2's M-A/M-B/M-A′ and A3's F-1/F-2 are relied on as recorded
there; the code tree they ran against is byte-identical to this one for the
crates involved (this branch adds only this document).

---

## 10. Stale citations found while verifying

| Record | Citation / phrasing | Current truth |
|---|---|---|
| This node's briefing text | "A2 asked what `classify_component` was for" | Under-compressed the predecessor: A2 §6 *assigned* the hook's role and reserved only signature/enum survival to A5 (details §1) |
| A1 §9 assumption 6 | "`EphemeralRegistry`, which maps bevy `Entity ↔ EphemeralId` (`ephemeral.rs:342-430`)" | Imprecise: the registry (`ephemeral.rs:160-166`) holds `BTreeMap<EphemeralId, EphemeralEntry>` and never an `Entity`; the `Entity ↔ EphemeralId` association is the `Ephemeral` component on the entity itself (`:342`) plus queries (`:436-439`). The claim's substance (durable/ephemeral ids separate from Bevy entities) is unaffected |
| `orrery_protocol/src/persist.rs:41-44` | `PersistId` "minted … peer-side from a journaled block grant (contiguous ranges, default 4096, usable offline)" | Present-tense description of an unbuilt path: no grant issuance/holding/consumption code exists anywhere (§2.4). Same drift class as docs/06 §210's `classify_component` consumers (still stale exactly as A2 §9 recorded, re-checked) |
| `docs/06-verifiable-core.md:210` + `:60` | Present-tense `classify_component` consumers | Re-confirmed stale (zero call sites, I2); now also the section §6.1 proposes to overwrite — A11 should amend docs/06 §2 alongside the ADR |
| A1/A2/A3's previously recorded stale citations (ADR-0038 line drift, D21 `validate_intent` parenthetical, docs/10 `orrery_field_host` rows, brief path drift) | — | Not re-litigated; where this document touches the same ground (D38 clause text, D21 table) the quotes were re-opened at source and held |

---

## 11. Unsure

Stated as unsure rather than smoothed over:

1. **Whether five dimensions is the right count.** P/R/W/N/A cover every
   consumer the tree names or promises; privacy and size caps were folded
   into N and P as attributes (§5.2) on the grounds that no independent
   consumer exists — the same grounds could later justify unfolding them.
   The invalid-combination table constrains only the five; a sixth axis
   would need its own IV rows.
2. **IV-1's cluster corner.** `W2 ∧ A3` is ruled invalid ("no step to
   re-execute") on today's shapes: intents are validated and receipted, not
   stepped. A future design where cluster-applied ops *are* deterministic
   steps over entity state could dissolve that row; nothing in the tree
   points there now.
3. **The X2 coverage gap's blast radius.** The shadowed marker clause means
   the uplink currently trusts `AuthorityPhase::LocalGranted` as its
   effective gate. Whether a real state exists where phase says
   `LocalGranted` while the marker is absent (mid-yield races, correction
   windows) was not traced end-to-end; the missing test is cheap either
   way.
4. **`bevy_reflect`'s three Cargo entries** (§5.1): unused in first-party
   source; whether removal breaks vendored-replicon feature unification was
   not attempted on this docs-only branch.
5. **Id-reuse-after-despawn** (§2.6): the actor supports it deliberately;
   forbidding it for evidence hygiene has retention implications this
   document did not cost out. Owner's trade.

Deliberately not done:

- **No implementation.** Mutations lived for one command run each and were
  reverted with passing results re-confirmed (§9); the only file this
  branch adds is this document.
- **No ADR text, no trait edits, no registry code.** N-1…N-7, the IV table
  and the §6 verdicts are proposals for A11 to carry into ADR form and for
  the owner to accept, amend, or refuse.
