# A4 — Deterministic ECS execution and the Bevy-gate replacement (#400)

**Status:** decision node for the #395 planning tree · **Date:** 2026-08-26 ·
**Tree:** `docs/400-a4` at `a7fc7f2d` · **Parents:**
[#400](https://github.com/baadc0de/orrery/issues/400) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md), [A2](a2-kernel-game-module-ownership.md),
[A3](a3-simulation-host-comparison.md) (+ its
[second opinion](a3-simulation-host-second-opinion.md)) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)
§Deterministic scheduling model

A3 left this node one load-bearing obligation: a dedicated `bevy_ecs::World`
may be admitted only once A4 delivers "a determinism gate at least as strong as
today's" (a3-simulation-host-comparison.md §7, precondition list item 1). This
document delivers that specification: the determinism envelope, the canonical
stage model, the enforcement mechanisms with their named checks, the threat
model they answer, the repeatability matrix — and the replacement for
`scripts/core-gates.sh`'s Bevy-in-dependency-graph clause, argued honestly
against what that clause actually enforces today.

Method, continuing A1/A2/A3:

- Every claim cites a file and line opened on this tree. Where this document
  asserts an enforcement property for an existing check, either the check's
  liveness was re-proven by mutation here, or A1–A3's mutation is relied on
  re-based (`git diff ce5e34a7..HEAD -- crates gates clients` is empty; only
  `scripts/check.sh`, `scripts/ci-changed-code.sh`, `scripts/gate-status.sh`
  changed after `ce5e34a7`, none of them in `core-gates.sh`). §9 records both.
- Proposed-but-unbuilt mechanisms were prototyped in scratch crates under
  `/tmp/opencode/gate-proto/` against the pinned dependency
  (`bevy_ecs = "=0.19.1"`, matching root `Cargo.toml:60` /
  `Cargo.lock:1224-1225`) and mutation-checked there; sources are summarized
  inline so the evidence is auditable without rebuilding anything.
- What **exists today**, what is **proposed**, and what belongs to another
  owner never share a sentence.

---

## 1. Ground truth inherited and verified on this tree

### 1.1 The gate as it exists

`scripts/core-gates.sh` runs per-commit (CI `gates` lane via
`scripts/check.sh`) and enforces five clauses over a hand-typed crate list:

| Clause | Enforces | Lines |
|---|---|---|
| 0 | `GATED_CRATES = (orrery_core orrery_games orrery_conformance)` | core-gates.sh:37 |
| 0b | `RULES_CRATES = (orrery_games orrery_conformance)` scopes the neighbour ban | :42 |
| 1 | no bevy anywhere in each gated crate's graph, dev-deps included | :71-75 |
| 2 | VC-4: no std `HashMap`/`HashSet` in gated library sources | :95-97 |
| 3 | VC-8: no ambient inputs (`Instant::now`, `SystemTime::now`, `thread_rng`, `from_entropy`, `rand::random`, `OsRng`, `from_os_rng`, `std::env::var`, `.elapsed()`) | :103-105 |
| 4 | VC-6: no std float transcendentals, path *and* method spelling; exact IEEE ops deliberately allowed (:112-116 comment) | :117-123 |
| 5 | no live neighbour reads in rules crates (`view.neighbor(`) | :137-139 |

Liveness was re-proven first-hand here: adding a `[dev-dependencies] bevy_ecs`
to `orrery_games` killed clause 1 — `core-gates: orrery_games has Bevy in its
dependency graph`, exit 1; revert passed, exit 0 (§9 M-G1). This also confirms
the scan covers dev-dependencies, not just normal ones.

### 1.2 The gate's coverage is its crate list — verified

The task asserts `GATED_CRATES` is hardcoded and that `orrery_witness` enables
bevy by default yet passes. Both hold on this tree:

- `readonly GATED_CRATES=(orrery_core orrery_games orrery_conformance)`
  (core-gates.sh:37) — a typed list, not derived from anything.
- `crates/orrery_witness/Cargo.toml:18`: `default = ["bevy"]`, pulling
  `bevy_app`, `bevy_ecs`, `bevy_time` (:24-27). Today:
  `cargo tree -p orrery_witness | grep -ci bevy` → **530**, and
  `./scripts/core-gates.sh` → exit 0. A first-party crate whose engine half
  re-executes `Ruleset::step`s carries half a thousand bevy references while
  the gate that supposedly keeps engines out of the verifiable core passes.
- Consequence (A3 G9, restated): any *new* crate hosting canonical execution
  would pass the unchanged gate too. Coverage is a decision someone makes per
  commit, not a property of the tree. A2 §3.3 finding 3 predicted exactly this
  shape of gap ("a future gate keyed on role rather than crate list is
  A4/A10 material"); this document builds it (§5).

### 1.3 The determinism machinery that already works

The executor defines the semantics any future schedule must reproduce:

- Fixed tick, absolute ticks: `TICK_HZ = 60`, "a constant, never a
  measurement" (executor.rs:25-28); VC-1 at docs/06-verifiable-core.md:246.
- Snapshot isolation per step: own state removed from the map before the step,
  so a step cannot read same-tick mutations or itself (executor.rs:116-118;
  mutation-proven by A3 F-2, test `a_step_never_sees_itself_as_a_neighbour`).
- RNG ownership: fresh `ChaCha8Rng` per entity per tick from
  `blake3::keyed_hash(seed ‖ persist_id ‖ tick)` (rng.rs:31-43), handed to
  `step` as `&mut TickRng`; the derivation itself golden-pinned
  (rng.rs:102-117).
- Quantize before hash: `own.quantize(); state_hash(&own)` (executor.rs:126-
  127); lattice 1 mm / 1 mm·s⁻¹ via `libm::round` half-away-from-zero
  (quantize.rs:22-24, :53-60).
- Input order is log order, never sorted (ruleset.rs:149-157; OrderedInputs
  has no sort path).
- Events are emission-ordered `Vec`s consumed next tick (ruleset.rs:193-202);
  cross-entity effects travel only as events — the neighbour-read gate bans
  the alternative until a `NeighborFrame` producer exists (core-gates.sh:126-
  139).
- Materializations install first-writer-wins in description order
  (executor.rs:144-157); identifiers derive from replayable inputs, never
  allocation order (ruleset.rs:272-278).
- Entity iteration order is `PersistId` order everywhere it can be observed
  (`BTreeMap` keyed storage, executor.rs:51,60-64; corpus stages iterate keys,
  corpus.rs:289-295).

And the harnesses that pin it:

- In-process double-run: `the_same_tick_run_twice_produces_the_same_state`
  (executor.rs:489).
- Committed cross-platform corpus: five cases, chain hash over every per-tick
  state hash, checked against `corpus/golden.json` on every target
  (conformance lib.rs:23-29; corpus.rs:58-103). One case axis runs every
  entity in its own single-entity executor and asserts the same chain as the
  shared run — replay isolation made mechanical (corpus.rs:38-49, :95-102).
- Four-platform matrix comparing digests, partial-matrix refusal
  (ci.yml:726+ matrix; verdict job requires all three non-baseline reports,
  ci.yml ~:843-850).
- Nightly ten-repeat soak in one process (nightly.yml:1187+).
- Game-level golden chains per scenario (games/src/golden.rs:44+), whose doc
  states the known blind spot honestly: state hashes only; a state-invisible
  outcome moves nothing (golden.rs:20-28).

---

## 2. The determinism threat model

Fourteen named ways nondeterminism could enter canonical execution. Each is
stated as the mechanism that would produce a divergence, not as a style rule;
§4 ties every enforcement mechanism to at least one of these, and §9 proves
the load-bearing ones live. T1–T5 exist today and are enforced; T6–T14 are
the classes an ECS-hosted future adds or sharpens.

| # | Threat | Mechanism of divergence |
|---|---|---|
| **T1** | **Unordered container iteration** | A `HashMap`/`HashSet`'s per-process randomized iteration order reaches observable behaviour — event emission order, a fold feeding state bytes, an input queue — so two runs of one binary disagree. The reason VC-4 exists (core-gates.sh:8-11). |
| **T2** | **Storage-order dependence (ECS class)** | Query iteration order in `bevy_ecs` follows archetype/table layout, hence insertion and structural-mutation history. Any projection that hashes or emits rows in query order produces world-history-dependent output from world-identical states. Demonstrated by both A3 prototypes and reproduced here (§9 E-3: naive forward `f6a3…` vs scrambled `d243…`). |
| **T3** | **Ambiguous system ordering** | Two systems with conflicting data access and no explicit ordering edge have *unspecified* relative order. Bevy's own build diagnostics treat this as ambiguity, not safety. The dangerous part is epistemic: ambiguous schedules can execute identically hundreds of runs in a row (A3 P3: 200/200 stable on its box), so observed stability is not evidence — only mechanical rejection is. |
| **T4** | **Ambient inputs** | Wall clock, monotonic clock, entropy sources, environment variables, filesystem reads, address/pointer hashing, allocation-order-derived identity — anything whose value differs across processes or machines entering a step. VC-8's list (core-gates.sh:103-105); identity clause separately pinned at ruleset.rs:272-278. |
| **T5** | **Floating-point divergence across targets/toolchains** | Non-associative float ops reassociated by differing codegen; std transcendentals with platform-specific implementations; fast-math-style contraction. Discrete outcomes must be integer-exact (VC-5) and continuous ones libm-routed then lattice-snapped (VC-6/VC-7), because three OSes × two architectures cannot promise bit-equality on raw floats (docs/06:251, :268). |
| **T6** | **RNG misuse** | A stream shared across entities (draw order becomes semantics), a stream seeded from anything but `(universe_seed, entity, tick)`, reseeding mid-tick, or draw-count dependence on data-dependent branches making replay draw differently. VC-3's ownership rules exist for all four. |
| **T7** | **Event and observer ordering** | Multiple producers emitting into one buffer without a total producer order; immediate observer/hook cascades whose recursion depth and interleaving are unspecified; dedup/idempotency applied in unspecified order. Emission order is determinism (ruleset.rs:196-201), so any delivery path that can reorder emissions breaks it. |
| **T8** | **Deferred structural-change application order** | Spawn/despawn/component-insert commands deferred to a sync point apply in whatever order they were queued — which is deterministic only if the queuing order is. Colliding inserts need a stated winner rule. Today's executor answers both: description order = emission order, first writer wins (executor.rs:144-157). An ECS host must pin the same two things. |
| **T9** | **Async work escaping the tick** | Tasks spawned inside canonical execution that complete during or after later ticks; I/O performed mid-schedule; futures holding `&mut World` across await points. Completion order of concurrent tasks is scheduling-dependent. Today structurally impossible (no async runtime in `orrery_core`; Cargo.toml:9-12 "no Bevy, no tokio"). |
| **T10** | **Cross-entity same-tick reads** | Reading another entity's current-tick state makes results depend on processing order. Banned today by snapshot isolation plus the neighbour gate; under ECS storage, join-style queries make the same hazard expressible silently. |
| **T11** | **Engine and dependency drift** | A dependency upgrade changes scheduler internals, entity allocation strategy, message-buffer semantics, or float library behaviour without any source change. D14 pins versions; the missing half is detecting *semantic* drift that survives identical source — answered by goldens + schedule digest (§3.10). |
| **T12** | **Build-profile divergence** | Identical source, different profiles, different outcomes. Concrete on this tree today: `i32::MAX + 1000` panics under dev (`overflow-checks = true`, cargo default) and wraps to `-2147482649` under release (§9 P-OV). Any canonical integer arithmetic that can overflow diverges by profile unless the policy is explicit. Float contraction differences add the same risk to continuous math. |
| **T13** | **Worker-count / executor interleaving** | Parallel execution of independent systems reordering shared-resource touches that ambiguity detection failed to flag, or completion-order-sensitive aggregation. Becomes reachable only when a multithreaded executor enters canonical execution. Probe evidence: ordered schedule produced one hash across 1-worker single-threaded and 2-/4-task multithreaded executors (§9 E-2). |
| **T14** | **Canonical/presentation leakage** | Cosmetic or presentation state entering claim bytes, persistence rows, or event payloads — the mixed-world hazard A3 used to reject V2, still present in weaker form if projections are written casually. The witness hash must see exactly what replication and persistence see (VC-7 rationale, quantize.rs:7-9). |

<!-- a4-section-3 -->

