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
