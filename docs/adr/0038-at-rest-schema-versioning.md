# ADR-0038: At-rest schema versioning is owned as three work items — the self-describing formats precede the machinery, and D21's freeze absorbs lazy migrations additively

**Status:** Accepted · **Date:** 2026-08-22 · **Decision:** D38

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. [ADR-0011] already decides the *scheme* (its
at-rest bullet, `:21`) and [docs/08 §16](../08-persistence.md) already expands
it. What no record settles — and what has kept [#223] unowned since it was
filed — is three narrower things this record decides: whether the machinery
reopens [ADR-0021]'s freeze, what must be fixed in the on-disk formats before
further bytes accrue, and how the four-piece deliverable decomposes across
phases and owners.

Out of scope, owned elsewhere: any edit to Rust source; the rewording of the
P5 deliverable line in docs/11-roadmap.md (clause (b) specifies it rather than
making it); filing or scoping the epic itself, which is the owner's call;
the archive tailer and its retention ([#107], P6); anything about *how* a
migration transforms bytes beyond the obligations of clause (e).

## Context

### 1. The deliverable, and what is and is not decided

The roadmap lists under P5 Deliverables ([11-roadmap.md:868](../11-roadmap.md)):

> At-rest schema versioning (D11): per-component schema versions in the
> component bag; `Ruleset`-registered migrations applied lazily on
> checkpoint-load/area-read plus an optional background sweep; journal/archive
> records carry their encoding version; migrations span ≥ 2 adjacent versions.

[#223] correctly observes that no epic claims any of it: [#105] is attestation,
[#106] identity and enforcement. The scheme itself, however, **is already
decided at ADR level** — D11's bullet (`0011-persistence.md:21`) carries all
four pieces, including the purpose clause ("so replay, catch-up, and griefing
rollback can decode history"), and docs/08 §16 (`:3582-3589`) expands each.
The issue's verification note that "an ADR is almost certainly required before
any code" is therefore right in effect but imprecise in letter: the missing
record was never the scheme; it is the ruling on seams, sequencing, and
ownership that follows.

### 2. What the tree holds today

Verified against the working tree:

- **`world/` values are written unversioned today.** The value envelope is
  `LIVE_TAG ‖ components` — one tag byte, then an opaque bag
  (`keyspace.rs:113`, `:119`); `EntityRecord.components` is documented as
  opaque bytes (`actor.rs:123`). No schema version exists anywhere in the
  checkpoint path.
- **Journal records are half-versioned.** The physical WAL envelope is
  versioned (`RawEnvelope::V1`, `journal/raw.rs:31-33`), but the logical
  `JournalRecord` carries an opaque postcard `payload` with no encoding
  version (`orrery_protocol/src/persist.rs:205`, `:223`). One payload type has
  already grown one ad hoc: `EntityRekey { version: u8 }` with
  `ENTITY_REKEY_VERSION = 1` (`persist.rs:229`) — server-owned, versioned;
  game-owned payloads unversioned.
- **Shape changes without migration exist once, and only for swept rows.**
  When `attest/` rows gained a field, the record of it noted plainly that
  positional postcard refuses trailing bytes, so old readers fail outright —
  affordable *only because* retention sweeps those rows within an hour of
  commit (`keyspace.rs:972-981`). That escape hatch does not exist for
  `world/`, `player/` or `ledger/` rows, which are permanent.
- **No migration machinery exists anywhere in the workspace**, and the
  `Ruleset` trait has no migration hook (`ruleset.rs:211`); its module doc
  scopes the trait to current consumers and treats additions as additive when
  consumers land (`ruleset.rs:8-13`).
- **The registration pattern this deliverable needs already exists inside the
  frozen surface**: `AdjudicationExecutor::register<R: Ruleset>(&mut self,
  factory: fn() -> R)` holds version-keyed builds bounded by
  `RETAINED_BUILDS = 3` (`adjudication.rs:324`, `:34`), composed at link time,
  outside any trait method.

### 3. Timing

[#223] was filed at 2026-08-21T18:06Z. The proposed extension beyond P6 merged
three hours later (#243, 2026-08-21T21:00Z) and claims [#223] twice — as an
orphaned deliverable followed to its conclusion ([11-roadmap.md:909]) and as a
P8 deliverable whose construction stays P5 but whose proof waits for a live,
populated, multi-version universe (`:1068-1071`, `:1103-1107`). That section
is explicitly **not accepted**, so it assigns nothing; but it is the owner's
own recorded intent, and clause (b) refines rather than contradicts it.

## Decision

### (a) Ownership: a standalone epic of three work items

> **At-rest schema versioning is one epic owning three independently landable
> work items. It rides no existing epic**: [#105]/[#106] are P5 enforcement
> bodies with their own criteria, and [#107] touches exactly one work item's
> tail end (below). Filing it is the owner's call.

- **W1 — Self-describing formats.** Version fields exist at rest: per-component
  schema versions inside the component bag per docs/08 §16; a persistd-visible
  staleness marker on `world/` values per clause (d)(2); encoding versions on
  journal logical records per clause (d)(5); versions on `player/` and
  `ledger/` rows at their next shape change at the latest.
- **W2 — Migration machinery.** The registration seam, lazy application on
  checkpoint-load and area-read, and the optional background sweep — built as
  additive composition per clause (c).
- **W3 — Proof.** The ≥ 2-adjacent-versions clause demonstrated on live data:
  a planted v₍n−1₎ row read *after* the sweep window still migrates lazily —
  the acceptance form the proposed P8 criterion already writes down
  ([11-roadmap.md:1103-1107]). W3 requires ≥ 2 shipped schemas over surviving
  data and structurally cannot precede P8; W2 must not wait for it.

W3's evidence leg coordinates with #107 only where the planted row lives in
the archive rather than FDB; everything else is independent of it.

### (b) Phase: the formats land before the machinery, and neither gates the P5 criterion as written

Stated plainly first: **the P5 demo criterion exercises none of the four
pieces.** It is the dupe gauntlet — replayed intent, double-spend race, forged
attestation, quarantine trade ([11-roadmap.md:870]) — and no leg reads a stale
row. The P5 listing survives on a different argument, reversibility, not the
criterion:

- **W1 belongs in P5**, and its real deadline is measured in commits, not
  phases: every week of development and playtest writes more unversioned
  long-lived rows, and rows written without versions are debt the migration
  mechanism cannot retroactively service except by an out-of-band rule
  ("absent field == v0") that should be written deliberately, in one place,
  while the keyspace is still cheap to reset. P5 is also when durable value
  begins to accumulate (accounts, ledgers, trades — the phase goal itself),
  which is exactly when disposable-data reasoning stops.
- **W2 constructs in P6–P7.** Its first real demand is the first schema bump
  on a universe worth keeping, which cannot occur before a universe survives
  across deployments — P7 territory. Constructing earlier buys nothing except
  unmaintained code; constructing later than the first bump is too late by
  definition.
- **W3 proves at P8**, matching the proposed extension's placement. This
  record recommends the roadmap line narrow accordingly — W1 at P5, W2/W3
  where stated above — an edit owned by whoever holds the docs/11 lane.

### (c) The crux: lazy migrations fit inside D21's freeze additively; nothing reopens

[ADR-0021] froze `orrery_persistd`'s public exports (:66-77) and defined the
escape hatch precisely: "Additive change — new methods, new types, new
default-carrying config fields — is not breaking and needs no record"
(:61-64).

Everything W2 needs enters through that door:

1. **A migration registry is a new type**, passed at composition time — the
   same shape as `AdjudicationExecutor::register`, which sits *inside* the
   frozen table and demonstrates that "`Ruleset`-registered" is satisfied by
   registration through composition, not by a trait method.
2. **Config gains default-carrying fields.** `CheckpointConfig` is a plain
   struct of defaulted fields (`checkpoint/scheduler.rs:30-51`); a registry
   field defaulting to empty is additive verbatim. `ColdCellReader::read_cold`
   and `CheckpointStore`'s signatures are untouched — rows flow as bytes, and
   migration composes around them inside persistd internals, which were never
   frozen.
3. **The `Ruleset` trait need not change.** Migrators are registered functions
   keyed by `(ComponentTypeId, from_version)`, supplied by the game at
   composition. Nothing about W2 requires adding a required method to
   `orrery_core::Ruleset` — which would break every implementation of the one
   trait games implement.

Two honesty notes, so the ruling is auditable. First, D21's frozen table names
only persistd surfaces; `orrery_core::Ruleset` appears nowhere in it — yet the
same record's Consequences call that trait "already … the only thing the
cluster calls" (:98-104), i.e., the seam in spirit. A required trait method
would arguably evade the freeze's letter while cutting against it; this record
declines that branch entirely by pinning composition-time registration as the
mechanism. Second, if a future design genuinely wants migrators on the trait,
that specific change names D21 and pays its ADR; it is priced under
Alternatives, not foreclosed.

**Ruling: the deliverable fits the freeze. The conditional in the proposed P8
crate notes — "must be designed against the freeze or argue to reopen it"
([11-roadmap.md:1088-1091]) — resolves to the first horn.**

### (d) What must be decided before further bytes are written

Ordered by irreversibility. (1), (2) and (3) gate W1; (4) and (5) gate W1's
journal half before P6's tailer makes archive history long-lived.

1. **Every long-lived at-rest family becomes self-describing.** `world/`
   values gain a persistd-visible version prefix; `player/`, `ledger/` and
   intent-family rows gain versions at their next shape change at the latest —
   the `enforced: bool` episode (`keyspace.rs:972`) is the documented cost of
   skipping this on a permanent-row family. The change that adds each field
   also states the bootstrap rule for rows predating it (**absent == v0**),
   in the record or code that adds it — never left implicit.
2. **Staleness must be visible without decoding game types.** docs/08 §16
   holds both that versions live per-component *inside* the bag (`:3586`) and
   that the sweep walks cold ranges (`:3588`) while the actor never decodes
   game types. Read together, something outside the bag must answer "is this
   row behind?": the envelope carries a bag-level marker (a floor or
   generation counter — implementer's choice within this constraint), written
   by persistd on write-back, so the sweep filters stale ranges without
   invoking game code. Per-component versions govern *what* migrates; the
   envelope marker governs *whether*, visible to code that never decodes the
   bag. Deciding this later would reshape envelope (1) under deployed data —
   hence it is a pre-bytes decision, not an implementation detail.
3. **Version-domain semantics are pinned now, because conflation poisons
   decode routing later.** Component-schema versions are per
   `ComponentTypeId`, allocated by the game, monotone, never reused or gapped
   within a type. They are **orthogonal to `RulesetId.version`**: a rules
   hotfix bumps no schema, a schema bump may ship without a rules change, and
   `RETAINED_BUILDS` bounds adjudication evidence, not schemas
   (`adjudication.rs:34`). They are likewise **orthogonal to
   `projection_version`**, the witness-projection axis D48 defines: a
   projection framing change (reordering slots, same payloads) alters
   commitment bytes without changing any schema or any rule, so without the
   third axis two hosts running identical rules over identical schemas could
   hash differently with nothing recording why — the same conflation failure
   this clause exists to prevent, one level up. A projection bump forces no
   component migration, and no schema or rules bump forces a projection
   bump. The bag's version fields, the build's digest, and the projection
   version answer different questions and none of the three may ever be
   derived from another.

   > *Amended by D48 (docs/adr/0048-canonical-witness-projection.md,
   > 2026-08-25, accepted by the owner through the #395 planning tree): this
   > clause originally pinned two axes — component-schema versions ⊥
   > `RulesetId.version` — and was widened in place to three when D48
   > defined `projection_version`. Every original obligation stands
   > unchanged; the amendment adds the third axis and extends the
   > derivation ban to it.*
4. **"≥ 2 adjacent versions" is pinned to its testable content:** the registry
   holds a v→v+1 step for every adjacent pair since the oldest readable era —
   a gapless chain, no skipped versions — and a retired step leaves only after
   the sweep has provably passed its range. docs/08 §16's sentence admits two
   readings (steps spanning pairs vs. readers retained n−2 deep); both are
   served by the gapless chain plus the planted-stale-row proof of clause
   (a)/W3, so the ambiguity is closed rather than argued.
5. **Journal logical records carry their encoding version with the record,**
   decided before the tailer first copies records into the archive. Mechanism
   is deferred — a header field on `JournalRecord` (a durable value-shape
   change in exactly the `keyspace.rs:972` sense, affordable only while
   journals stay retention-bounded per D20 and pre-P7 journals remain
   disposable) or a payload-prefix convention per `RecordKind`, generalizing
   `ENTITY_REKEY_VERSION`. What is not deferred: the version travels *with*
   the record, and the physical `RawEnvelope::V1` is the upgrade vehicle, not
   the answer.

Deferrable, and deliberately unpriced here: sweep rate and priority dials, the
registry's API signatures, migrator retirement schedules, whether the
background sweep exists at v1 (D11 marks it optional).

### (e) Migrators are rules code, under the verifiable core's discipline

A migration function is pure: a function of `(bytes, from_version)` alone —
no I/O, no clocks, no globals, no neighbour reads (the cross-entity rule the
core gates enforce for rules code extends to migrators wherever they live).
Steps apply in ascending chain order. The reason is not style: migrated state
feeds state that claims hash — `state_hash` commits to canonical encodings
(`ruleset.rs:273`) — so nondeterministic migration manufactures false
deviations, the exact failure witnessing exists to adjudicate. Unlike tick
rules, migrations run on the persistence path outside input logs, so nothing
replays them; their determinism is enforced by construction and review, which
is why the purity requirement is normative here rather than harness-checked.

### (f) Cost arithmetic

```
per-component version   postcard varint: 1 B/slot for values < 128
typical 16-slot bag     +16 B/entity-row
10^7 entity-rows        ≈ 160 MB at rest          (illustrative universe)
                        noise against FDB values ≤ 10 KB and bag payloads
per-bag alternative     1 B/row total — saves ~15 B/entity, couples unrelated
                        components' migrations, forces whole-bag rewrite on
                        single-component change; rejected by accepted §16
journal overhead        100k rec/s × 1 B = 0.1 MB/s against the modeled
                        26 MB/s ingest ([11-roadmap.md:1021-1022]) ≈ 0.4%
lazy migration          k chained steps × O(payload), paid once per row;
                        milliseconds against the < 50 ms area page-in budget
                        (D11), amortized to zero thereafter by write-back at
                        current version on the ordinary checkpoint cadence
background sweep        bounded rate R clears N stale rows in N/R: 50k rows/s
                        retires a 10^7-row backlog in ≈ 3½ min of dedicated
                        work, spread to hours under an FDB-load cap
```

The storage argument never motivates urgency — bytes are cheap. What motivates
clause (d) is that the *decision* embedded in those bytes is free today and
expensive after eras of production rows.

## Consequences

- **For the epic filer:** scope = W1 + W2 + W3 with clause (b)'s phase split;
  one coordination row with #107 for W3's archive-planted-row option.
- **For the W1 implementer:** the `world/` envelope change, protocol record
  versions, and the player/ledger row versions of clause (d), with tests
  asserting absent-field-decodes-as-v0 and the envelope-marker filtering.
- **For the W2 implementer:** new public types and default-carrying config
  fields only. If a frozen signature seems necessary, stop — that change names
  D21 and reopens this record instead.
- **Roadmap wording owed by the docs/11 lane holder:** the P5 deliverable line
  narrows to W1 (formats), with W2/W3 placed per clause (b).
- **AGENTS.md's decision-table row is owed by whoever holds that lane**, as
  [ADR-0036]'s Consequences already recorded for D36.
- **DECISIONS.md gains the D38 row.**

## Alternatives considered

- **Ride #106 or #107.** Rejected: wrong bodies — different phases, different
  criteria; #107 intersects only W3's archive leg. A deliverable owned by an
  epic that cannot prove it is how #223 sat unowned in the first place.
- **No record until P8 ("not yet").** Taken seriously and rejected on two
  grounds with deadlines attached: unversioned long-lived rows accrue *per
  commit* until clause (d) lands, and the freeze-fit ruling gates the epic's
  shape — the reason #223 has sat unowned since filing. The scheme being
  already-decided cuts for this record, not against: its marginal content is
  exactly the contested remainder, none of it manufactured.
- **Migrators as a required method on `orrery_core::Ruleset`.** Rejected as
  the default: semver-breaking for every implementation of the trait games
  implement; evades D21's letter (core is not in its table) while cutting
  against its stated seam. Remains available by naming D21 if a design ever
  argues the trade.
- **One per-bag version byte only.** Cheapest possible marker (~15 B/entity
  saved), but couples unrelated components' migrations, rewrites whole bags on
  single-component change, and contradicts accepted docs/08 §16 ("versioning
  is per *component*, not per snapshot", `:3586`). Priced, rejected.
- **Retention-bounded shape changes everywhere** — the `attest/`/
  `enforced` escape hatch as a general policy. Rejected for exactly the rows
  this deliverable concerns: it works only where rows are swept on hour-scale
  horizons (`keyspace.rs:972-981`), and `world/`, `player/` and `ledger/` rows
  are permanent.
- **Whole-universe reseed at the first schema bump.** Viable precisely while
  universes are disposable — until P7. Recorded because it bounds W2's true
  deadline: *before the first schema bump on a universe worth keeping*, not a
  calendar date.

## Open questions

1. **Sweep runner and dials** — persistd's maintenance path vs. a separate
   actor, and the rate/priority parameters as D16 rows: owner's call when W2
   constructs.
2. **Whether the background sweep ships at v1** or defers with W2 — D11 marks
   it optional; lazy-only operation is coherent until retirement pressure
   appears. Owner's call.
3. **Who files the epic, and whether W1 rides the current P5 persistence PR
   series** — owner.

[#223]: https://github.com/baadc0de/orrery/issues/223
[#105]: https://github.com/baadc0de/orrery/issues/105
[#106]: https://github.com/baadc0de/orrery/issues/106
[#107]: https://github.com/baadc0de/orrery/issues/107
[11-roadmap.md:868]: ../11-roadmap.md
[11-roadmap.md:870]: ../11-roadmap.md
[11-roadmap.md:909]: ../11-roadmap.md
[11-roadmap.md:1021-1022]: ../11-roadmap.md
[11-roadmap.md:1088-1091]: ../11-roadmap.md
[11-roadmap.md:1103-1107]: ../11-roadmap.md
[ADR-0011]: 0011-persistence.md
[ADR-0021]: 0021-ruleset-distribution.md
[ADR-0036]: 0036-binding-rate-window.md
