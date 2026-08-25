# ADR-0043: The determinism envelope, canonical stages, and the role-discovery gate replacement

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D43

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree (proposal
R2, [a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md) §2), with the
overflow posture fixed as recorded in clause (f) and one sub-question of that
clause explicitly reserved to the owner in clause (f)(4).

**Supersedes:** nothing. It amends the **enforcement mechanism** of the
Bevy-free property that [D9] scopes and [D15] assigns to crates — the
membership rule of `scripts/core-gates.sh`, a script — and it amends no
accepted record's normative text. Within the #395 proposal set, R7 is the only
proposal that amends an accepted record's text; this record deliberately is
not the second. It sits under [D42]'s canonical simulation architecture (R1,
the umbrella: executor-hosted canonical state; composition-root/`SimulationHost`
seam; shared app world rejected; dedicated world trigger-gated T1–T3) and
cites it rather than restating it. Its substance is
[a4-deterministic-execution.md](../plans/a4-deterministic-execution.md) §3–§6,
carried into a record; the threat model (A4 §2, T1–T14) and the probe evidence
(A4 §9) are incorporated by reference and re-verified below where this record
leans on them.

Out of scope, each with its owner: identity and the `PersistId` ↔ ECS entity
mapping (R3, A5/#401); per-component capabilities and policy (R4, A5/#401);
command/event semantics beyond the ordering and delivery timing fixed in
clause (c) — replay, dedup, idempotency, volume bounds (R5, A6/#402); the
rollback unit (R6, A7/#403); the canonical witness projection **format** (R7,
A7/#403 — clause (f) *places a bit inside* whatever that format is, it does
not define the format); manifests and the schedule digest's **storage** (R8,
A8/#404 — clause (g) defines the digest's existence and content only, keeping
exactly the division A4 §3.10 and A8 drew); conformance and matrix
implementation (A10/#406). Nothing in this record schedules work inside the
P4 digest before P4 exit: the pipeline digest covers `crates/orrery_witness`,
`crates/orrery_core`, `crates/orrery_games` and `gates/p1-swarm`
(`scripts/p4-ledger.sh:409-414`, verified on this tree), and every enforcement
change this record orders lands in `scripts/` or in new conformance material,
neither of which is hashed.

## Context

### 1. The gate this record replaces is weaker than its name — verified, not implied

`scripts/core-gates.sh` enforces five clauses: no Bevy anywhere in a gated
crate's dependency graph (core-gates.sh:71-76), no std `HashMap`/`HashSet`
(VC-4, :95), no ambient inputs (VC-8, :103), no std float transcendentals in
either spelling (VC-6, :117-123), and no live neighbour reads in rules crates
(:137). The clauses are sound and stay. The defect is **membership**:

```text
readonly GATED_CRATES=(orrery_core orrery_games orrery_conformance)   # core-gates.sh:37
```

The gate's coverage *is* that hand-typed list. Re-verified on this tree at
acceptance time, both halves:

- `cargo tree -p orrery_witness | grep -ci bevy` → **530**.
  `crates/orrery_witness/Cargo.toml:18` sets `default = ["bevy"]`, pulling
  `bevy_app`, `bevy_ecs`, `bevy_time` (:24-26).
- `./scripts/core-gates.sh` → exit **0**, all five clauses green.

A first-party crate whose engine half re-executes `Ruleset` steps carries half
a thousand Bevy references past a green gate, because nobody typed its name
into line 37. The same shape applies forward: any *new* crate hosting
canonical execution — the exact crate [D42]'s trigger-gated world would
create — passes today's gate unchanged. Coverage is a per-commit decision
someone remembers to make, not a property of the tree. (A2 §3.3 predicted the
gap; A3 G9 restated it; A4 §1.2 verified it; this record closes it.)

The gate's clause liveness is separately real: at acceptance time a
`[dev-dependencies] bevy_ecs = { workspace = true }` appended to
`crates/orrery_games/Cargo.toml` killed clause 1 —
`core-gates: orrery_games has Bevy in its dependency graph`, exit 1 — and the
revert passed, exit 0. The scan covers dev-dependencies, not just normal ones.
The clauses are worth keeping; the list is what this record replaces.

### 2. The machinery the envelope codifies already exists

The stages in clause (b) are derived from what `Executor::step_entity` does
today, not invented: quantize-before-hash at `executor.rs:126-127`
(`own.quantize(); let hash = state_hash(&own);`), first-writer-wins
materialization in description order at `executor.rs:144-157`
(`Entry::Vacant` install), per-entity per-tick RNG from
`blake3::keyed_hash(seed, entity ‖ tick)` at `rng.rs:31-43`, input order = log
order, events consumed strictly next tick, entity iteration in `PersistId`
order (`BTreeMap` storage). The double-run test, the committed cross-platform
corpus with chain hashes, the four-target CI matrix with partial-refusal
verdict, and the nightly soak pin this behaviour (A4 §1.3 with per-line
citations, spot-re-verified here). This record turns that behaviour from
"what the code happens to do" into "what any host must do".

### 3. The profile-divergence hazard is demonstrated, not feared

Re-verified at acceptance time in a scratch crate:
`black_box(i32::MAX) + 1000` **panics** under the dev profile
(`attempt to add with overflow`) and **wraps to `-2147482649`** under release.
The workspace root `Cargo.toml` sets no `[profile]` section (verified: no
match for `profile` in the file), so both behaviours are cargo defaults —
`overflow-checks = true` in dev, off in release. Any canonical integer
arithmetic that can overflow therefore diverges *by build profile* today
(threat T12). The defect is the profile-dependence, regardless of which
posture replaces it; clause (f) is the owner's answer.
