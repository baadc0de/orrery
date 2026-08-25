# ADR-0045: Per-component capability policy

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D45

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree (proposal
R4, [a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md) §2), with one
sub-choice — the enforcement mechanism for clause (e)'s row IV-7 —
deliberately left open and reserved to the owner (see Open questions).

**Supersedes:** nothing. It **amends no accepted record's normative text** —
within the #395 proposal set, R7 remains the only proposal that will. What it
does overwrite is documentation: docs/06 §2's present-tense claim that
`classify_component` has consumers ("`orrery_persist_client` uses it to route
bulk diffs vs. intents, `orrery_witness` uses it to decide what to watch",
`docs/06-verifiable-core.md:210`, echoed at `:60`) describes call sites that
have never existed — re-verified at acceptance: `rg classify_component` over
the tree finds the defaulted trait method
(`crates/orrery_core/src/ruleset.rs:298`) and three overriding
implementations (`crates/orrery_games/src/regolith/mod.rs:129`,
`crates/orrery_games/src/skirmish/mod.rs:186`,
`crates/orrery_conformance/src/ruleset.rs:242`) and **zero call sites**. That
section is rewritten alongside this record (A11 drift item DA-3). This record
sits under [D42]'s canonical simulation architecture (the umbrella) beside
[D43] (determinism envelope), and cites both rather than restating them. Its
substance is
[a5-identity-and-capabilities.md](../plans/a5-identity-and-capabilities.md)
§4–§6 carried into a record, with IV-7's enforcement analysis from
[a9-engine-boundaries.md](../plans/a9-engine-boundaries.md) §3; the evidence
both nodes recorded is incorporated by reference and re-verified below where
this record leans on it.

Out of scope, each with its owner: identity classes and allocation — the
three closed identity classes, `EphemeralId`, and the no-provisional-durable-
identity rule that clause (e) row IV-4 consumes are defined by D44 (drafted
concurrently in this proposal set; this record consumes them and defines none
of them); the determinism envelope and gate membership ([D43]); message
semantics — replay, dedup, idempotency, volume bounds (R5, A6/#402); the
rollback unit and mechanism (R6, A7/#403 — clause (c)'s R dimension records
membership only); the canonical witness projection format (R7, A7/#403);
manifests, schema-id namespace governance, and the capability registry's
storage and construct (R8, A8/#404 — clause (c) fixes the *shape* of a
declaration, not the registry that holds it). Nothing in this record
schedules work inside the P4 digest before P4 exit: the pipeline digest
covers `crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games`
and `gates/p1-swarm` (`scripts/p4-ledger.sh:409-414`, verified on this tree),
and this record's only code-touching consequence — clause (f)'s eventual
removal of a trait method from `orrery_core`/`orrery_games` — is explicitly
sequenced last, at the owner's pleasure, post-P4-digest.

## Context

### 1. One enum, five questions — the hook this record retires never answered any of them

`Ruleset::classify_component` returns one three-valued `CoreClass` —
Core / Bulk / Cosmetic (`crates/orrery_core/src/ruleset.rs:61-71`) — with a
deliberately conservative default: "an unclassified component is `Cosmetic`,
so a game that forgets to classify gets a component that is never persisted
rather than one silently admitted to adjudication" (`ruleset.rs:293-297`).
The default's instinct is right and clause (c) keeps it. The enum's shape is
wrong, and the tree proves it: the kernel asks at least five *independent*
questions about a component — is it persisted, is it rolled back, is it
witnessed, is it replicated, who may write it — and the in-tree answers do
not lie on one axis (Context §3). Meanwhile the one hook that was supposed
to answer them has zero call sites (re-verified at acceptance, and
independently by two prior nodes: A3's inventory row G3,
[a3-simulation-host-comparison.md](../plans/a3-simulation-host-comparison.md):45,
and A5 §6.1). Nothing routes on the enum today; the only cost of replacing
it is deleting three first-party overrides.

### 2. Reflection defines no encoding today — census, not assumption

Re-run at acceptance on this tree: `rg "Reflect"` over `crates/*/src`
matches **zero** first-party source lines. `bevy_reflect` appears as a
listed direct dependency of exactly three crates —
`crates/orrery_spatial/Cargo.toml:27`, `crates/orrery_net/Cargo.toml:24`,
`crates/orrery_persist_client/Cargo.toml:33` — with no use in their sources;
whether those entries are needed for feature unification with the vendored
replicon or are dead weight is recorded as an open question, not a finding
(A5 §5.1, unchanged). Every encoding that reaches a wire or a store is a
*declared* codec: hand-written `CoreCodec` for canonical state, where
"canonical is the whole requirement" because divergent encodings of equal
state produce false deviations (`crates/orrery_core/src/ruleset.rs:23-27`);
the framed component bag carrying `(component, schema_version, payload)` per
slot (`crates/orrery_persistd/src/schema.rs:48-59`); and replicon uplink
payloads produced by **registered per-component serialize functions, not
reflection** (`vendor/bevy_replicon/src/server/uplink.rs`). Clause (b)
ratifies this state; under it, nothing changes today.

### 3. The five dimensions' independence is in the tree, not in the argument

Four capability combinations that a single flag cannot express already ship:

- **Witnessed but not replicated.** Core state reaches witnesses as signed
  frames and claims over the witness link, run by every interested peer
  "regardless of witness-set membership"
  (`crates/orrery_witness/src/witness.rs:655-663`); a watched entity outside
  a peer's replication interest is still checked. W2 ∧ N0 is coherent.
- **Replicated but never persisted.** Projectiles replicate in-island under
  `IslandAuthoritative`, a marker kept distinct from `LocallyAuthoritative`
  on purpose so that "an ephemeral entity carrying this one can never be
  persisted no matter what game code does with it"
  (`crates/orrery_authority/src/ephemeral.rs:346-352`). N1 ∧ P0.
- **Persisted but not adjudicated.** `CoreClass::Bulk`'s own definition:
  "Persisted but not adjudicated: quantized replication, bulk writes,
  invariant validators only" (`crates/orrery_core/src/ruleset.rs:66-68`).
  P1 ∧ W1 ∧ ¬W2.
- **Persisted but not rolled back.** Ledger rows are transaction-final; the
  FDB intent transaction "remains the sole authority"
  (`crates/orrery_persistd/src/intent/mod.rs:152-154`), and a committed
  credit is never rewound — corrections are compensating transactions.
  P2 ∧ R0.

This is why clause (c) declares five dimensions rather than one flag: the
combinations above are facts of the shipped tree, and any single-axis policy
would either forbid one of them or misfile it.

### 4. The one row nothing enforces — G-1, confirmed live at acceptance

A9's mutation M3 showed that an engine handle appended to a replicated,
journal-bound payload passes everything. Re-run at acceptance on this tree
(post-#427): `entity.to_bits().to_le_bytes()` appended to the `DiffUplink`
payload in `crates/orrery_persist_client/src/feed.rs` — `cargo test -p
orrery_persist_client` fully green (`96 passed`, `2 passed`, `2 passed`,
`1 passed`, all `0 failed`; one suite `0 passed; 0 filtered out`, an empty
suite read and not counted as coverage), `./scripts/core-gates.sh` exit 0,
**no named check exists**. The mutation was reverted and the tree
re-verified clean. Gap G-1 is live: clause (e)'s row IV-7 states the rule,
and no mechanism enforces it today. The record says so plainly rather than
implying otherwise (Open questions, item 1).

## Decision

### (a) The schema id of record is `(ComponentTypeId, SchemaVersion)` — N-5

Every capability declaration, at-rest slot, and manifest entry names a
component by the pair `(ComponentTypeId, SchemaVersion)`, declared at
composition time. The pair is **independent of Rust type names, `TypeId`,
reflection registration, replicon `FnsId`, and archetype layout** — every
item on that list is build- or registration-order-dependent, and the pair is
the only component naming that survives a recompile. `SchemaVersion` is
game-allocated per component type, monotone, never reused or gapped, and
orthogonal to `RulesetId::version`: "a rules hotfix bumps no schema, a
schema bump ships without a rules change, and neither number is ever derived
from the other" (`crates/orrery_protocol/src/atrest.rs:23-27`; [D38]
clause (d)(3)).

This clause ratifies what the durable layer already does: the framed bag
stores the pair per slot (`crates/orrery_persistd/src/schema.rs:48-59`), and
the uplink feed already drops the registration-order-dependent `FnsId`
before anything durable is built
(`crates/orrery_persist_client/src/feed.rs:85-96` — `DiffUplink` carries no
`FnsId` field, `crates/orrery_protocol/src/gateway.rs:371-393`). What is new
is the extension: the pair keys *capability declarations* (clause (c)), not
only at-rest slots. Who allocates `ComponentTypeId` values across modules,
collision detection, and how the pair enters the compatibility manifest are
R8's (A8) — this clause fixes the namespace's shape so R8 has exactly one
namespace to govern.

### (b) Reflection never defines an encoding — N-6

Reflection may serve tooling — inspectors, debug dumps — but may never
*define* an encoding. Any reflect-assisted path that produces bytes for wire
or store must go through an explicit mapping to a declared
`(ComponentTypeId, SchemaVersion)` codec. Under Context §2's census this
clause changes nothing today — zero first-party reflection uses exist, and
replicon payloads are registered serde functions
(`vendor/bevy_replicon/src/server/uplink.rs:5-6`), not reflection. The
clause exists to bind the future: it makes "derive the persistence format
from `Reflect`" a rejected shortcut rather than an available one, whichever
engine adapter or tooling later grows a reflection habit.

### (c) Five independent capability dimensions; zeros fail closed — N-7

Every component type a module declares carries five independent capability
dimensions, declared **as data at composition time** — the registration
idiom [D38] clause (c) pins, like `MigrationRegistry::declare`
(`crates/orrery_persistd/src/migration.rs:53-55`) and
`AdjudicationExecutor::register` (`crates/orrery_persistd/src/adjudication.rs:350`)
— keyed by clause (a)'s pair. The dimension names are this record's; the
registry construct (one registry vs several, its storage) is R8's.

| Dim | Values | Meaning · today's consumer |
|---|---|---|
| **P** persistence | `P0` none · `P1` bulk · `P2` critical | `P1`: journal/checkpoint path, last-writer-wins per `(entity, tick)` under lease fencing (`gateway.rs:367-369`, `:388-389` in `orrery_protocol`). `P2`: mutated only inside attested intent transactions (`intent/mod.rs:152-154`). `P0`: never leaves the world |
| **R** rollback | `R0` excluded · `R1` included | Whether prediction resimulation and post-adjudication correction restore it. Unit and mechanism are R6's (A7); this dimension records membership only |
| **W** witness | `W0` unwatched · `W1` invariant-checked · `W2` replay-adjudicated | `W1`: stage-1 `Invariant` predicates on received samples (`crates/orrery_core/src/invariants.rs:114-118`) — "the only validation most bulk-class state ever gets" (`ruleset.rs:304-310`). `W2`: logged inputs, signed claims, isolated re-execution (`crates/orrery_core/src/replay.rs:106-116`) |
| **N** replication | `N0` none · `N1` interest-replicated | `N1`: replicon under AOI/interest, owner-written (single-writer). The witness frame/claim channel is **not** this dimension — evidence flows to witness peers regardless of interest membership, which is what makes W and N independent (Context §3) |
| **A** write authority | `A0` local · `A2` island-weak · `A1` lease-holder · `A3` cluster-transaction | Who may mutate: nobody but this process (`A0`); the in-island total order with no fence (`A2`, `crates/orrery_authority/src/ephemeral.rs:82-90`); the fenced lease holder (`A1`, [D7]); only an FDB intent transaction (`A3`) |

**Defaults are the zeros, and the zeros fail closed: no declaration, no
capability.** This generalizes two behaviours the tree already has and this
record keeps — the unclassified-defaults-to-Cosmetic rule
(`ruleset.rs:293-297`) and the migration registry's refusal to load a bag
slot whose component no build declares
(`crates/orrery_persistd/src/migration.rs:22-25`, `:80-85`; mutation-proven,
Verification appendix row MV-2).

Privacy/visibility filtering and maximum encoded size are real per-component
concerns but are *attributes of N and P respectively*, not independent axes;
migration is already a per-`(ComponentTypeId, from_version)` registration
and needs no new dimension. Recorded so R8 can revisit if an independent
consumer appears (A5 §5.2).

Consumers, per dimension (game-authored, kernel-consumed): **P** routes the
uplink and write classes (persist-client scheduler and gateway); **R** feeds
R6's rollback-unit membership; **W** feeds witness attention — which
components get executors and claims versus invariants only; **N** feeds
replication registration and interest; **A** feeds admission — which write
path will accept a mutation (lease-fenced diff, island claim, intent op).

### (d) Named valid profiles — the diagonal, plus the points the enum could not express

| Profile | P | R | W | N | A | In-tree example |
|---|---|---|---|---|---|---|
| **Core** (verifiable) | P1/P2 | per R6 | W2 | N1 | A1 | `RegolithState` via `components::STATE` (`crates/orrery_games/src/regolith/mod.rs:79-84`, classified Core at `:129-135`) |
| **Bulk** | P1 | R0 | W1 | N1 | A1 | docs/06 §2's bulk class; terrain deltas |
| **Cosmetic-local** | P0 | R0 | W0 | N0 | A0 | UI/selection state; anything undeclared (the default) |
| **Ephemeral-shared** | P0 | R0 | W0 | N1 | A2 | Projectiles/VFX under `EphemeralId` (D44's transient class) |
| **Critical/ledger** | P2 | R0 | W0* | N0 | A3 | Balances, item ownership; *audited by receipts and the single-ownership row, not by replay |

The last two rows are the demonstration that clause (f) is necessary rather
than tidy: `CoreClass` files an ephemeral projectile and a local UI
component under the same value, and gives a ledger row no value at all.

### (e) The eight invalid combinations, each with its mechanism

A combination is listed here only when a mechanism makes it *incoherent*,
not merely unusual. A prohibition without its mechanism is unreviewable;
each row carries its reason.

| # | Combination | Why it is invalid — the mechanism |
|---|---|---|
| IV-1 | `W2` without `A1` (single fenced writer) | Replay adjudication verifies a **subject-signed** claim chain (`crates/orrery_core/src/replay.rs:287`, `verify_claim`; `StateClaim` at `crates/orrery_protocol/src/verifiable.rs:189`). Island-weak (`A2`) entities have no fence and no chain — contested writes have no single subject to hold to; cluster-written (`A3`) rows have no step to re-execute. No signer, no verdict |
| IV-2 | `W2` without a deterministic canonical encoding (`CoreCodec` + quantization, VC-1..8) | The claim commits to a hash of canonical quantized bytes. A nondeterministic or unstable encoding makes every honest re-execution a false deviation — the witness convicts everyone, which is worse than watching no one (`ruleset.rs:23-27`) |
| IV-3 | `P2` with any writer but `A3` | The FDB transaction is "the sole authority" for critical rows (`intent/mod.rs:152-154`); the single-ownership row *is* the anti-dupe invariant (`crates/orrery_protocol/src/persist.rs:185-186`). A lease-holder journaling a balance bypasses read-check-write: **duplication by construction** |
| IV-4 | `P1`/`P2` on an `EphemeralId` entity | Transient identity (D44) has no durable row to write. Mechanically enforced today at both ends: the uplink keys off `LocallyAuthoritative`, ephemerals carry `IslandAuthoritative` — a marker kept distinct precisely so this cannot happen (`ephemeral.rs:346-352`), and the uplink-side guard is now pinned by a named test (Verification appendix row MV-1; the coverage gap A5 recorded as X2 was closed by #427, commit `9aae34f9`) |
| IV-5 | `R1` with `P2` | Rolling back transaction-final state re-plays committed durable effects — a rewound-then-recommitted credit is **a dupe machine**. Corrections to critical state are compensating transactions through the same envelope, never rewind. (The converse, `R1 ∧ P1`, is R6's to shape, not invalid) |
| IV-6 | `N1` with `A0` | Replicating state nobody holds authority to write breaks single-writer: receivers have no rule for whose value wins. Everything replicated today is owner-written (the uplink feed persists only `LocallyAuthoritative` entities, `feed.rs:64-67`; replicon uplink is owner-side by construction) |
| IV-7 | Any capability above the zeros for a schema embedding an engine handle (`Entity`, `ComponentId`, `FnsId`, archetype/row indices) | Engine handles are allocator-local and generation-dependent; their bytes mean nothing to another world, a restart, or a replay. **The rule is accepted; its enforcement mechanism is not chosen** (Open questions, item 1) and no mechanism exists today — Context §4's mutation passed everything. **Until a mechanism lands, IV-7 is enforced by review**, and this record must not be read as claiming otherwise |
| IV-8 | Any capability above the zeros without a declared `(ComponentTypeId, SchemaVersion)` | "No declaration, no capability" (clause (c)). Existing behaviour at both ends: default-Cosmetic (`ruleset.rs:293-297`), fail-closed migration (`migration.rs:80-85`, mutation-proven, appendix row MV-2) |

**Inert-but-legal**, named so nobody "fixes" them into the table above:
`W1` with an empty invariant slice (the trait's own default — "correct but
slower to notice", `ruleset.rs:302-310`); all-zeros on a *declared*
component (declaration without capability is a no-op, not an error);
`W2 ∧ N0` (Context §3 — the witness channel is not replication).

### (f) `classify_component` is replaced, not wired; removal sequenced last

The capability policy does not give `classify_component` a consumer. It
replaces it. The hook's *shape* — game-declared, kernel-consumed, keyed by
`ComponentTypeId` — is confirmed and kept; the hook's *form* — a method on
`Ruleset` returning one three-valued enum — is retired, for three reasons:

1. **One value cannot carry five independent dimensions** (clause (d)).
   Wiring consumers to the enum would encode the diagonal as law at the
   moment this record establishes the space is wider than the diagonal.
2. **Code where data belongs.** A capability declaration must reach the
   compatibility manifest (R8) and the at-rest reader (persistd), and most
   rows must be readable *without* linking a `Ruleset`. The registration
   idiom already in the tree carries declarations as data at composition
   time; a trait method requires calling into the build to learn a static
   fact.
3. **Retirement is cheap now and expensive later.** Zero call sites means
   no consumer migrates; three first-party overrides
   (`regolith/mod.rs:129`, `skirmish/mod.rs:186`,
   `conformance/ruleset.rs:242`) are deleted with it — a compile-visible,
   first-party-only change.

**Sequencing rider: the removal is last, not first.** The defaulted method
stays until the declaration registry exists and the three implementations'
facts are restated as declarations, so at no point does the tree hold less
classification information than it does today. Removing a *defaulted* trait
method is not [D38]'s "required method" branch, but it is still
trait-surface change on `orrery_core`/`orrery_games` — both P4 digest crates
— and it **lands at the owner's pleasure, post-P4-digest**, proposed as its
own change, never smuggled inside registry or manifest work.

### (g) `CoreClass` survives as derived vocabulary only

"Core", "Bulk" and "Cosmetic" remain the names of clause (d)'s load-bearing
macro-profiles, and the documentation set keeps speaking them ([D9], [D10],
[D11], docs/06 §2). But the enum ceases to be a source of truth: **nothing
authors, persists, hashes, or routes on `CoreClass`**. A validator may
*compute* a profile name from a declaration's five values — and may refuse
declarations matching no known profile unless explicitly marked novel, a
cheap tripwire for typo'd policies — and prose may say "core-class state".
The value is derived output, never input.

### (h) The tier predicate is `W2`, not `CoreClass::Core`

Where an accepted design keys behaviour to "core-class state" — most
concretely A3's H1 routing wall, scored there "on an unwired hook" with the
explicit caveat that a per-dimension policy would force re-derivation
(`docs/plans/a3-simulation-host-comparison.md:574-577`) — the predicate is
re-derived as: **state with `W2` (replay-adjudicated) lives in the
per-entity executor with every structural guarantee; everything else is
eligible for whatever storage the host seam chooses.** This is the
re-derivation A3 §11.3 anticipated, performed. It is also *stronger* than
the enum it replaces: `W2` is the one dimension that actually forces the
executor's structure (isolated single-entity replay), whereas a component
could have been marked `Core` for persistence-priority reasons without
needing replay semantics at all.

## Consequences

- **What this record actually adds is smaller than its title.** Clause (a)
  ratifies how the durable layer already names components; clause (b)
  forbids a practice with zero current instances; clause (c)'s fail-closed
  zeros generalize two shipped behaviours; four of clause (e)'s rows
  (IV-3, IV-4, IV-6, IV-8) describe mechanisms that already refuse the
  combination. The record's real new commitments are the five-dimension
  declaration shape itself, the IV table as acceptance criteria for R8's
  registry, clause (f)'s replacement-and-removal sequence, and clause (h)'s
  predicate change. A reader weighing its cost should weigh those four.
- **IV-7 is a stated rule with no mechanism.** Until the owner picks one
  (Open questions, item 1) and it lands, the only thing between an engine
  handle and the journal is review — demonstrated, not feared (Context §4).
  The record chooses to say this rather than let the IV table imply
  enforcement that does not exist.
- **docs/06 §2 is corrected, not preserved.** The consumer sentence at
  `docs/06-verifiable-core.md:210` (and the machine-checked claim at `:60`)
  described wiring that never existed; the rewrite replaces it with clause
  (c)'s consumer list and clause (f)'s disposition. This is a documentation
  overwrite, not an amendment to any accepted record's normative text.
- **R8 inherits a defined admission bar.** The registry's acceptance
  criteria — refuse undeclared capability (IV-8), refuse invalid
  combinations (IV-1..IV-6), refuse engine-handle schemas if the owner
  picks registry-time enforcement (IV-7 option 2) — are fixed here, so
  manifest formatting cannot silently weaken them.
- **Three `Ruleset` implementations eventually shrink** by one method each,
  and the kernel's `CoreClass` enum stops being API input. That is the whole
  first-party blast radius of clause (f), and it is deferred behind the
  registry existing and the P4 digest closing.

## Alternatives considered

- **Wire `classify_component` to the promised consumers** (make docs/06 §2
  true instead of correcting it). Rejected: the enum cannot express the
  shipped combinations (Context §3, clause (d)), so wiring it would encode
  the diagonal as law and every off-diagonal component would be misfiled at
  the moment of wiring.
- **One flag, or fewer dimensions.** Rejected by the tree, not by taste:
  four off-diagonal combinations ship today (Context §3). Folding privacy
  and size caps in as sixth and seventh axes was likewise rejected — no
  independent consumer exists; they are attributes of N and P until one
  does (clause (c)).
- **Byte-scanning payloads for engine handles** as IV-7's enforcement.
  Rejected as theater: entity bits are indistinguishable from any other
  `u64` in a payload, so a scanner would alarm on nothing or on everything.
  The guard must live where the type is still a type — which is exactly why
  the surviving options are a compile-time bound or registry-time schema
  refusal (Open questions, item 1; A9 §3).
- **Keep `CoreClass` as the authored datum and bolt dimensions beside it.**
  Rejected: two sources of truth for one fact, with the enum's three values
  guaranteed to drift from the five-dimension rows they summarize. Derived
  vocabulary (clause (g)) keeps the names and deletes the drift.

## Open questions reserved to the owner

1. **IV-7's enforcement mechanism.** The rule is accepted; the mechanism is
   deliberately not chosen. Two options, priced:
   - **Compile-time `EngineHandleFree` bound at replicon registration.**
     A sealed marker trait implemented for primitives, protocol types, and
     containers of same — and *not* for `Entity`, `ComponentId`, or any
     `bevy_ecs` type — required by the payload-registration path, so a
     component embedding a handle fails at the registration call site at
     compile time. A9 §3's reasoning is load-bearing: **byte scanning
     cannot distinguish entity bits from any other `u64`**, so compile time
     is the only workable enforcement point on the wire path. Whether
     replicon's registration API admits the bound without forking the
     vendored copy is **not prototyped — unevidenced either way**.
   - **Registry-time schema refusal.** The declaration granting P/N/W must
     declare a schema, and a schema containing engine-handle types is
     refused at declaration. Depends on R8's registry existing; reaches
     every capability grant, not only the replicated path.
   The options are not exclusive. **Until at least one lands, IV-7 is
   review-held** — and Context §4 shows the review is currently the only
   thing there.
2. **The three unused `bevy_reflect` Cargo entries** (Context §2): remove,
   or keep for vendored-replicon feature unification. Removal was not
   attempted on this docs-only branch; unevidenced either way.
3. **When clause (f)'s removal lands.** Post-P4-digest and post-registry by
   this record; the date is the owner's.
