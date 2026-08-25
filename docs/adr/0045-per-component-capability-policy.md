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
