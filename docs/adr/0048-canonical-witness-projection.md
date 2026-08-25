# ADR-0048: The canonical witness projection — entity-tick commitment, sorted stable-id ordering, persistence-identical framing, and a third version axis

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D48

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree (proposal
R7, [a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md) §2). Its
substance is
[a7-persistence-rollback-witnessing.md](../plans/a7-persistence-rollback-witnessing.md)
§5 and §7.1 carried into a record; the evidence that node logged is
incorporated by reference and re-verified below where this record leans on it.

**Supersedes:** nothing. **Amends [D38]:** this is the only record in the
#395 set that amends an Accepted record's normative text. Clause (g) below
widens D38 clause (d)(3)'s version-domain law from two orthogonal axes to
three, and the owner chose the mechanism deliberately: a **direct edit to the
clause with a provenance note**, not an erratum blockquote. D28 and D31 carry
errata because facts under them went stale while the law stood; here the law
itself is widened by an accepted decision, so D38's text should read correctly
to a first-time reader and the note records where the third axis came from.
Every pre-existing obligation of (d)(3) survives the edit verbatim in
substance; the amendment adds, it does not rewrite.

This record sits under [D42]'s canonical simulation architecture (the
umbrella: executor-hosted canonical state; composition-root/`SimulationHost`
seam; shared application world rejected; dedicated world trigger-gated T1–T3)
beside [D43] (determinism envelope) and [D45] (per-component capability
policy), and cites all three rather than restating them.

Out of scope, each with its owner: the rollback unit, grain, window and
budget (D47); capability classes and their fail-closed defaults ([D45]);
identity classes and allocation (D44); message semantics — delivery, dedup,
idempotency, replay of events ([D46]); the determinism envelope and gate
membership ([D43]); manifests, and **where `projection_version` is stored**
— this record defines the axis, R8 (A8/#404) stores it in the manifest beside
`RulesetId` and the schedule digest. Nothing here schedules work inside the
P4 digest before P4 exit: the pipeline digest covers
`crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games` and
`gates/p1-swarm` (`scripts/p4-ledger.sh:409-414`, verified on this tree).

## Context

### 1. The projection as it ships

The projection of one entity `e` at tick `t`, already true of the tree:

```text
bytes(e, t)  = CoreCodec::encode( quantize( state(e, t) ) )   # declared codec, fixed field order
hash(e, t)   = blake3( bytes(e, t) )                          # the value a StateClaim commits to
```

Multi-entity aggregates, wherever one is ever needed (corpus chains, a
checkpoint digest, any future world digest):

```text
world_digest(t) = blake3( concat( for id in sort_ascending(ids): id ‖ bytes(id, t) ) )
```

Verified at source on this tree: canonical state lives in the engine-neutral
per-entity executor's `BTreeMap<PersistId, R::CoreState>`
(`crates/orrery_core/src/executor.rs:51`); `entities()` documents "Every
entity, in `PersistId` order" (`executor.rs:97-99`); `step_entity` snaps then
hashes — "VC-7: snap before anything hashes or replicates it" —
`own.quantize(); let hash = state_hash(&own);` (`executor.rs:125-127`); and
`state_hash` is blake3 over the state's `CoreCodec` encoding
(`crates/orrery_core/src/ruleset.rs:324`; A7 and D38 cite this function at
`:273` — it has drifted to `:324`, the claim is unchanged).

### 2. Order-immunity is shown, not assumed

The acceptance bar A7 inherited was: no reliance on raw ECS world
serialization without evidence that archetype order, insertion order,
allocation order and hash-map iteration cannot reach the hash.

- **Current store — immunity is structural**, re-verified first-hand for this
  record: nothing iterates a container into `hash(e,t)`. The hash input in
  `step_entity` is one entity's own post-step state (`executor.rs:126-127`);
  `state_hash` encodes one value through the declared codec
  (`ruleset.rs:324`); the one map on the path is the keyed `BTreeMap`, whose
  iteration order is `PersistId` by type and which is not iterated into any
  hash anyway.
- **The hazard is real, three times over.** Three independent probes
  reproduced insertion-order-dependent query iteration on the pinned
  `bevy_ecs 0.19`, and each showed a sorted-by-stable-id projection agreeing
  across permuted orders while the naive fold differed (A7 §5.2, citing A3
  P1/P2 "canonical AGREES | naive DIFFERS" and A4 E-2's one hash across six
  executor/order cells).
- **The stability trap is refused.** Observed run-to-run agreement of a naive
  projection proves nothing (A3 P3: 200/200 stable and still unspecified).
  WP-2 is the rule because sortedness is provable; stability is not.

### 3. Quantize-before-hash went from unpinned to pinned

A7's mutation X-C swapped the quantize/hash pair in `step_entity` and **every
suite passed** — every in-tree `CoreState` stored lattice integers, so the
snap was a no-op on every fixture. A7 reported that against interest as a
coverage gap and named the cheap closing test. The gap is now closed: #426
(commit `bfefc5a6`, issue #425) landed
`crates/orrery_conformance/tests/quantize_pin.rs`, an off-lattice ruleset
whose step lands 567 µm off the millimetre lattice on every tick, with a
vacuity self-check so a re-latticed fixture fails loudly instead of pinning
nothing. Clause (d) cites the re-run performed for this record's acceptance
(Verification appendix): the X-C mutation now kills
`the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` by name.

### 4. The framing the projection shares with persistence

The at-rest bag frames each component slot as
`(ComponentTypeId, SchemaVersion, payload)` — `ComponentSlot { component,
schema_version, payload }` (`crates/orrery_persistd/src/schema.rs:47-58`).
A7 §12 made WP-3 explicitly conditional on A8 keeping that shape, and **A8
committed to keeping it** (a8-compatibility-manifests.md §9.2: "the framed
slot stays `(ComponentTypeId, SchemaVersion, payload)`"), which is why WP-3
can bind to the tuple rather than only to the slogan behind it.

## Decision

### (a) WP-1 — the unit of witness commitment is one entity-tick

Claims, frames, bundles and replay all address `(PersistId, Tick)`; nothing
commits to a multi-entity hash on the wire. This is today's shape kept
deliberately: it is what makes single-entity adjudication, the witness's
one-executor-per-watched-entity model, and D47's per-entity restore
consistent with one another. Any future multi-entity aggregate is a fold
over entity-tick commitments under clause (b)'s order, never a new
commitment primitive.

### (b) WP-2 — entity order is `PersistId` ascending; cross-grid, `(GridId, PersistId)` ascending

Any enumeration of entities that feeds bytes into a hash, a golden, a
fixture or an emitted artifact sorts first, by `PersistId` ascending within
a grid and `(GridId, PersistId)` ascending across grids. Today's executor
satisfies this structurally (`BTreeMap` keys; `entities()` documents the
order, `executor.rs:97-99`); the clause exists so that a future host must
*prove* it rather than inherit it — sortedness is the property all three
order probes validated, and observed stability of an unsorted fold is
explicitly not an acceptable substitute (Context §2).

### (c) WP-3 — component order is `ComponentTypeId` ascending, slots framed as at rest

Whenever state is per-component, components enumerate in `ComponentTypeId`
ascending order and each slot is framed `(ComponentTypeId, SchemaVersion,
payload)` — byte-identical framing to the at-rest bag
(`schema.rs:47-58`), which A8 has committed to keeping (Context §4). Today
a `CoreState` is one declared codec and this clause is vacuous for it; it
binds the day state becomes per-component (D42's T1 trigger is precisely
that day). The rule's substance is **witness framing ≡ persistence
framing**: a claim commits to exactly what replication and persistence saw,
as one sentence with one meaning. If the at-rest slot shape ever changes
under D38's machinery, WP-3 follows the bag — and that is a
`projection_version` bump under clause (f).

### (d) WP-4 — quantize before hash, always

`state_hash` is computed over the quantized state, never the raw post-step
state (VC-7; A4's stage rule S4 ≺ S5). This ordering is now **pinned**:
`crates/orrery_conformance/tests/quantize_pin.rs` holds an off-lattice
ruleset for which raw and quantized bytes differ on every tick, plus a
vacuity self-check. Re-run at acceptance for this record: the X-C mutation
(swap `own.quantize()` / `let hash = state_hash(&own)` at
`executor.rs:126-127`) kills
`the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` by name
(`1 passed; 1 failed`); reverted, `2 passed; 0 failed`, tree clean
(Verification appendix). A7 recorded X-C as a surviving mutation; that
resolution — #425/#426, commit `bfefc5a6` — is the reason this clause can
be stated as enforced rather than aspirational.

### (e) WP-5 — no engine artifact may reach the projection

No `Entity` bits, `ComponentId`s, `FnsId`s, archetype or row indices,
reflection-derived names, and no bytes produced by iterating any engine
world container may appear in witness bytes. This is [D45]'s invariant row
IV-7 seen from the projection side, and this clause is honest about its
enforcement status: **WP-5 is normative but not mechanically enforced at
the projection boundary today.** IV-7's enforcement mechanism is
deliberately open ([D45] Open questions item 1 — schema-shape lint,
`EngineHandleFree` marker trait, or registry-side refusal; until one lands,
IV-7 is review-held), and the live evidence is a survived mutation, twice:
A9's M3 rode `entity.to_bits()` into a `DiffUplink` payload and every named
check passed, and [D45]'s MV-3 re-ran it with the same result. For the
executor store the property is structural (the defining crates are
gate-held Bevy-free under [D43]); for any world-hosted store it is exactly
what D42's Tier-H differential harness must prove per commit before the
host is admitted. This record does not imply a guard that does not exist.

### (f) WP-6 — the projection is versioned: `projection_version`, a third orthogonal axis

An integer `projection_version`, bumped on any change to WP-2/WP-3 framing
— entity ordering, component ordering, slot shape — and never for a payload
schema change or a rules change. Its value today is **1** and describes the
shape in Context §1. It is stored in the manifest beside `RulesetId` and
the schedule digest — the manifest construct, storage and governance are
R8's (A8/#404); this clause defines the axis and its bump rule only, and
the integer is carried nowhere in the tree today. Without this axis, a
projection change would present as mass deviation — the false-conviction
failure witnessing exists to prevent.

### (g) The amendment to D38 clause (d)(3): two axes become three

[D38] clause (d)(3) pinned version-domain semantics as a two-axis law:
component-schema versions orthogonal to `RulesetId.version`, neither
derived from the other. This record widens it, by direct edit with a
provenance note, to three:

> component-schema version ⊥ `RulesetId.version` ⊥ `projection_version`,
> and none of the three is ever derived from another.

The justification is D38's own, one level up: a projection framing change
(reordering slots, same payloads) alters commitment bytes without changing
any schema or any rule, so without a third axis two hosts running identical
rules over identical schemas can hash differently with nothing recording
why — the same conflation failure clause (d)(3) already exists to prevent.
The co-movement discipline carries unchanged: a rules hotfix bumps no
schema and no projection; a schema bump may ship without either other bump;
a projection bump forces no component migration (A7 M-4);
`RETAINED_BUILDS` still bounds adjudication evidence, not schemas and not
projections. Every pre-existing obligation of (d)(3) — per-type monotone
gapless allocation, the hotfix/schema-bump examples, the derivation ban
between bag fields and build digest — survives in the amended text.

## Consequences

- **What this record actually adds is smaller than its title.** Clauses (a),
  (b) and (d) ratify what already ships — the entity-tick unit, the
  `BTreeMap`-ordered executor, and the now-pinned quantize-before-hash
  ordering cost nothing today; clause (c) is vacuous until state becomes
  per-component. The record's real new commitments are clause (f)'s version
  axis, clause (g)'s D38 amendment, and the discipline that a future host
  must *prove* (b) and (e) rather than inherit them.
- **What the commitment does not cover, stated so no one reads more into a
  green golden than it holds.** `hash(e,t)` commits to quantized canonical
  *state* only. A7's X-A mutation injected an event-only outcome — no state
  trace — and the entire golden battery passed, because goldens cover state
  hashes alone; only per-game unit tests caught it. Events, materialization
  descriptions (`ruleset.rs:280` — not part of `state_hash` by doc and by
  design), and delivery mappings are outside the commitment. The proposed
  closure is A7's G-1..G-3 event/outcome fixture chains — test
  infrastructure accepted as a programme item in the #395 tree, not part of
  this record — and games whose outcomes must be *adjudicable* still write
  own-state traces. Whether event commitments ever enter `StateClaim` is a
  protocol change reserved to the owner (OD-27), not proposed here.
- **Excluded from witness bytes** (each with its reason, per A7 §5.3):
  presentation and cosmetic state ([D45]'s zeros fail closed);
  `EphemeralId` entities (structurally unpersistable); materialization
  descriptions; lightyear ring contents and `VisualCorrection` residuals
  (presentation tier); `UplinkSeq` counters and every rebuildable in-memory
  index.
- **For D38's readers:** clause (d)(3) now states the three-axis law
  directly; the note under it records that D48 widened it. No other D38
  clause moved.
- **For R8 (manifest):** the manifest carries `projection_version`; its
  storage, construct and governance land there. Expected-difference
  classification in the differential harness keys off `projection_version`
  and `RulesetId.version`: a difference under equal versions is a failure,
  under bumped versions a migration fixture.
- **DECISIONS.md gains the D48 row.**

## Alternatives considered

- **Raw engine-world serialization as the projection.** Rejected on
  demonstrated hazard: insertion-order-dependent iteration reproduced three
  times on the pinned `bevy_ecs 0.19`; the sorted projection is the
  validated mitigation (Context §2).
- **"It has always hashed the same" as the ordering guarantee.** Rejected —
  the stability trap: a naive fold observed stable 200/200 is still
  unspecified. Sortedness is provable per commit; stability is an accident
  of the current allocator.
- **Folding the projection axis into `RulesetId.version` or into
  `SchemaVersion`.** Rejected for exactly D38 (d)(3)'s reason: conflated
  axes poison decode routing and turn an innocent framing change into
  either a phantom rules change (invalidating retained adjudication builds
  for nothing) or a phantom schema change (forcing migrations of unchanged
  payloads). Three questions, three numbers.
- **A per-projection erratum on D38 instead of amending the clause.**
  Rejected by the owner's explicit choice: errata mark facts that went
  stale under a standing law (D28, D31); here the law itself is widened by
  an accepted decision, and first-time readers should read the current law,
  not reconstruct it from a correction.
- **Making WP-5 mechanically enforced as part of this record.** Declined,
  not rejected: the mechanism menu is [D45]'s open question, owned there;
  this record widening its own scope into enforcement machinery would
  duplicate an open owner decision.

## Open questions

1. **IV-7 / WP-5 enforcement mechanism** — schema-shape lint,
   `EngineHandleFree` marker, registry-side refusal, or a combination:
   [D45] Open questions item 1, owner's call there. Until one lands, WP-5
   is review-held at the projection boundary.
2. **Whether event commitments enter `StateClaim`** (OD-27) — a protocol
   change with claim-size and [D46] interplay, the owner's door.
3. **Where `projection_version` lives on the wire and at rest** — R8's
   manifest work; this record deliberately fixes only the axis and the
   bump rule.

## Verification appendix — what was re-run at acceptance

All on this tree (branch `docs/adr-0048-r7`, based on `21c25e17`),
2026-08-25. The mutation lived for one run; the revert re-ran the check and
passed; the working tree was verified clean after.

| Check | Result |
|---|---|
| Executor store and ordering | `BTreeMap<PersistId, R::CoreState>` at `crates/orrery_core/src/executor.rs:51`; `entities()` "Every entity, in `PersistId` order" at `:97-99` |
| Quantize-before-hash at source | `executor.rs:125-127`: "VC-7: snap before anything hashes or replicates it" — `own.quantize(); let hash = state_hash(&own);` |
| No container iterated into the hash | `step_entity` hashes one entity's own post-step state only; `state_hash` (`ruleset.rs:324`) encodes one value through its declared codec. A7 and D38 cite `state_hash` at `ruleset.rs:273`; drifted to `:324`, claim unchanged |
| X-C mutation re-run (WP-4) | Swapped the pair at `executor.rs:126-127`. Named check died: `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` FAILED, `1 passed; 1 failed` (`cargo test -p orrery_conformance --test quantize_pin`). Revert: `2 passed; 0 failed`; `git status` clean |
| Pin provenance | `crates/orrery_conformance/tests/quantize_pin.rs` landed in #426, commit `bfefc5a6` (issue #425), with the vacuity self-check `the_fixture_lattice_rounds_half_away_from_zero` |
| At-rest slot shape | `ComponentSlot { component, schema_version, payload }` at `crates/orrery_persistd/src/schema.rs:47-58`; its doc already cites D38 clause (d)(3) for schema-version semantics, and the amended clause keeps those semantics verbatim |
| A8's slot-shape commitment | a8-compatibility-manifests.md §9.2: "the framed slot stays `(ComponentTypeId, SchemaVersion, payload)`" |
| WP-5 not mechanically enforced | [D45] Open questions item 1 (IV-7 review-held); A9 M3 and D45 MV-3 both survived riding `Entity` bits in a `DiffUplink` payload — relied on as recorded, not re-run here |
| `projection_version` absent from code | `rg projection_version` over `crates`, `gates`, `scripts`: zero matches — the axis is defined here, stored by R8, carried nowhere today |
| D38 clause (d)(3) location | `docs/adr/0038-at-rest-schema-versioning.md:198-205` before the edit, exactly as R7's row cited |
| P4 pipeline trees | `scripts/p4-ledger.sh:409-414`: `crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games`, `gates/p1-swarm`. The one mutation above touched `orrery_core` for one test run and was reverted byte-identical |

[D38]: 0038-at-rest-schema-versioning.md
[D42]: 0042-canonical-simulation-architecture.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[D45]: 0045-per-component-capability-policy.md
[D46]: 0046-message-class-semantics.md
