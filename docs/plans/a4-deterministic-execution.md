# A4 — Deterministic ECS execution and the Bevy-gate replacement (#400)

**Status:** decision node for the #395 planning tree · **Date:** 2026-08-25 ·
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
| **T1** | **Unordered container iteration** | A `HashMap`/`HashSet`'s per-process randomized iteration order reaches observable behaviour — event emission order, a fold feeding state bytes, an input queue — so two runs of one binary disagree. The reason VC-4 exists (core-gates.sh:9-11). |
| **T2** | **Storage-order dependence (ECS class)** | Query iteration order in `bevy_ecs` follows archetype/table layout, hence insertion and structural-mutation history. Any projection that hashes or emits rows in query order produces world-history-dependent output from world-identical states. Demonstrated by both A3 prototypes and reproduced here (§9 E-3: naive forward `f6a3…` vs scrambled `d243…`). |
| **T3** | **Ambiguous system ordering** | Two systems with conflicting data access and no explicit ordering edge have *unspecified* relative order. Bevy's own build diagnostics treat this as ambiguity, not safety. The dangerous part is epistemic: ambiguous schedules can execute identically hundreds of runs in a row (A3 P3: 200/200 stable on its box), so observed stability is not evidence — only mechanical rejection is. |
| **T4** | **Ambient inputs** | Wall clock, monotonic clock, entropy sources, environment variables, filesystem reads, address/pointer hashing, allocation-order-derived identity — anything whose value differs across processes or machines entering a step. VC-8's list (core-gates.sh:103-105); identity clause separately pinned at ruleset.rs:272-278. |
| **T5** | **Floating-point divergence across targets/toolchains** | Non-associative float ops reassociated by differing codegen; std transcendentals with platform-specific implementations; fast-math-style contraction. Discrete outcomes must be integer-exact (VC-5) and continuous ones libm-routed then lattice-snapped (VC-6/VC-7), because three OSes × two architectures cannot promise bit-equality on raw floats (docs/06:251, :268). |
| **T6** | **RNG misuse** | A stream shared across entities (draw order becomes semantics), a stream seeded from anything but `(universe_seed, entity, tick)`, reseeding mid-tick, or draw-count dependence on data-dependent branches making replay draw differently. VC-3's ownership rules exist for all four. |
| **T7** | **Event and observer ordering** | Multiple producers emitting into one buffer without a total producer order; immediate observer/hook cascades whose recursion depth and interleaving are unspecified; dedup/idempotency applied in unspecified order. Emission order is determinism (ruleset.rs:196-201), so any delivery path that can reorder emissions breaks it. |
| **T8** | **Deferred structural-change application order** | Spawn/despawn/component-insert commands deferred to a sync point apply in whatever order they were queued — which is deterministic only if the queuing order is. Colliding inserts need a stated winner rule. Today's executor answers both: description order = emission order, first writer wins (executor.rs:144-157). An ECS host must pin the same two things. |
| **T9** | **Async work escaping the tick** | Tasks spawned inside canonical execution that complete during or after later ticks; I/O performed mid-schedule; futures holding `&mut World` across await points. Completion order of concurrent tasks is scheduling-dependent. Today structurally impossible (no async runtime in `orrery_core`; orrery_core/Cargo.toml:8). |
| **T10** | **Cross-entity same-tick reads** | Reading another entity's current-tick state makes results depend on processing order. Banned today by snapshot isolation plus the neighbour gate; under ECS storage, join-style queries make the same hazard expressible silently. |
| **T11** | **Engine and dependency drift** | A dependency upgrade changes scheduler internals, entity allocation strategy, message-buffer semantics, or float library behaviour without any source change. D14 pins versions; the missing half is detecting *semantic* drift that survives identical source — answered by goldens + schedule digest (§3.10). |
| **T12** | **Build-profile divergence** | Identical source, different profiles, different outcomes. Concrete on this tree today: `i32::MAX + 1000` panics under dev (`overflow-checks = true`, cargo default) and wraps to `-2147482649` under release (§9 P-OV). Any canonical integer arithmetic that can overflow diverges by profile unless the policy is explicit. Float contraction differences add the same risk to continuous math. |
| **T13** | **Worker-count / executor interleaving** | Parallel execution of independent systems reordering shared-resource touches that ambiguity detection failed to flag, or completion-order-sensitive aggregation. Becomes reachable only when a multithreaded executor enters canonical execution. Probe evidence: ordered schedule produced one hash across 1-worker single-threaded and 2-/4-task multithreaded executors (§9 E-2). |
| **T14** | **Canonical/presentation leakage** | Cosmetic or presentation state entering claim bytes, persistence rows, or event payloads — the mixed-world hazard A3 used to reject V2, still present in weaker form if projections are written casually. The witness hash must see exactly what replication and persistence see (VC-7 rationale, quantize.rs:7-9). |

---

## 3. The required determinism envelope

### 3.1 Envelope (what "deterministic" promises, and to whom)

Three rings, matching D9's scoping and docs/06 §5:

1. **In-process (one binary, one machine): bit-exact.** Same inputs → same
   state bytes, same event sequence, same hashes — across runs, worker
   counts, executor kinds, insertion orders. No tolerance anywhere in this
   ring; it is what double-run tests and the soak assert.
2. **Across the supported platform matrix: discrete bit-exact, continuous
   within bands.** Four targets (x86_64 Linux/Windows, aarch64 Linux/macOS —
   x86_64-macos deliberately unsupported, ci.yml ~:843-848 comment), pinned
   toolchain and deps (D14). Discrete (VC-5) compares `==`; continuous
   (VC-6/VC-7) is libm-routed, lattice-snapped each tick, then compared under
   D16 bands (ε_pos 1 cm, ε_vel 1 cm/s) by the §5 comparator.
3. **Explicitly outside the envelope:** differing compiler versions outside
   the pin, modified or third-party rules builds claiming an honest id (the
   tamper model keeps the honest `RulesetId` on purpose — that is what
   witnessing adjudicates, not a determinism failure), fast-math/codegen-flag
   variance, and any platform outside the four.

A future ECS host inherits this envelope unchanged. It does not get to narrow
ring 1 ("stable enough per process") because ring 1 is what makes ring 2's
corpus comparable at all, and it does not get to widen ring 2 into "all
platforms" because that promise is exactly the one rapier scopes and avian
declines (docs/06:268).

### 3.2 Canonical stages

Derived from what `Executor::step_entity` does today (§1.3), not invented:
a canonical tick is a fixed pipeline over a frozen input set. In an
ECS-hosted future the same pipeline becomes explicit schedule structure:

```text
S0 SealInputs     freeze this tick's input log in VC-2 order; nothing may
                  append after S0 begins (late arrivals join t+1)
S1 Deliver        apply external commands + last tick's events as inputs,
                  per entity, in log order
S2 Step           per-entity pure step: own state + ordered inputs + TickRng;
                  entity processing order = PersistId ascending; steps are
                  independent (snapshot isolation), so S2 may parallelize
S3 Record         collect neighbour reads for the log (first-read order)
S4 Quantize       snap every continuous field (VC-7) — before any hashing
S5 Claim          compute per-entity state_hash; assemble claims
S6 Materialize    install structural changes: emission/description order,
                  first-writer-wins (executor.rs:144-157 semantics)
S7 Emit           enqueue emitted events as t+1 inputs (delivery strictly
                  next tick)
```

Two properties are non-negotiable because adjudication consumes them: **S4
precedes S5** (quantize-before-hash, executor.rs:126-127) and **S6 applies no
input visible to any S2 of the same tick** (materialized children cannot
change a step that already ran; corpus.rs:95-102 asserts shared-vs-isolated
chain equality, which pins this).

### 3.3 System ordering

Within a stage, systems form a total order established at composition time:

- **S2** parallelizes freely *across entities* (independent own-state writes)
  but never across systems with overlapping access without an explicit edge.
- Every pair of systems with conflicting data access carries an explicit
  `.before()`/`.after()` edge — never an `.ignore_ambiguity()`. Ambiguity is
  rejected, never ignored (mechanism E-M2, §4).
- Ordering edges are declared between *systems*, not left to registration
  luck; registration order is code order and stable only under pinned source,
  so the schedule digest (§3.10) exists to notice drift.

### 3.4 Deferred structural changes

All spawn/despawn/component-insert in canonical execution goes through
deferred commands flushed at **S6 only**. Flush application order = queue
order = (system order within stage) × (emission order within system); both
are deterministic by §3.3. Identifier collisions resolve first-writer-wins by
`PersistId` (today's rule verbatim). Direct `World::spawn`/`despawn` calls
inside S0–S5 are prohibited; the host exposes command queues, not world
mutation, to canonical systems. Enforcement honesty: the flush-point rule is
API-shaped (host design) plus review; its *symptom* is caught by the
isolated-replay corpus axis if it ever breaks ordering.

### 3.5 Event ordering

Emission order within a producer; producer total order across producers;
delivery at S1 of t+1 and never earlier; observers/hooks do not exist in the
canonical path (immediate cascades are T7). Message buffers are drained in
system order at the delivering stage. Deduplication, replay behaviour,
idempotency keys, and volume bounds are **A6's (#402) territory** — this
document fixes ordering and delivery timing only, and defers the rest rather
than deciding it in passing.

### 3.6 Query-order constraints

Query iteration order must never be observable in anything that leaves the
canonical context: claim bytes, persistence rows, event payloads, presentation
frames that peers might diff. Concretely:

- Any projection producing canonical output iterates rows **sorted by
  `PersistId`** through one host-projection API; raw query iteration is
  permitted for presentation-local work only (mirroring, HUD).
- This is enforced mechanically where possible (projection differential
  harness, §4 E-M3) and honestly *not* lintable beyond that: a grep cannot
  tell an observable iteration from an unobservable one. A3's second opinion
  said the same (its V3 note: "a lint cannot ban observable iteration order
  the way grep bans HashMap").

### 3.7 Random-stream ownership

Unchanged from VC-3, restated as ownership rules: the RNG is a per-entity,
per-tick value derived by `tick_rng` (rng.rs:31), passed `&mut` into the step;
draws are code order; there is deliberately no reseed path mid-tick. An ECS
host adds two prohibitions: no RNG resource in canonical stages (`Res<TickRng>`
or similar global streams are T6), and no draw whose count depends on
data-dependent branches across entities (each entity's stream must be
reproducible from `(seed, entity, tick)` alone).

### 3.8 Floating-point policy

VC-5/VC-6/VC-7 verbatim: integer math for discrete outcomes; libm routing for
continuous math with std transcendentals banned in both spellings
(core-gates.sh:117-123); exact IEEE ops (`round/floor/ceil/trunc/abs/
mul_add`) allowed and load-bearing for the lattice; quantize-before-hash; D16
bands for cross-platform comparison. Nothing here changes with storage; the
policy attaches to *code roles*, which is why §5's replacement gate scans
roles, not crates-that-exist-today.

### 3.9 Async prohibitions

Canonical execution is synchronous end-to-end within a tick: no async runtime
in the canonical graph (true today: orrery_core/Cargo.toml:8, kept as a
gate clause in §5); no task spawned during S0–S7 that outlives the schedule run;
no I/O inside canonical stages — the outside world enters as sealed inputs
(S0) and leaves as events/frames (S7+). Presentation worlds and network
pumps live entirely outside the host seam.

### 3.10 Schedule compatibility hashing

The brief asks how schedule topology enters the compatibility hash (brief
line ~592). Proposal: the composition root computes a **schedule digest** =
blake3 over a canonical serialization of: ordered stage list; per-stage
ordered system names; all declared ordering edges sorted lexicographically;
the ambiguity-detection setting; the executor policy. Uses:

- pinned into the game manifest (format owned by **A8**, #404);
- asserted equal between peers at session setup alongside `RulesetId`
  (wire placement owned by A11/owner — proposed, not decided here);
- asserted by a unit test against the current value, so an accidental system
  reorder fails CI the way a golden does.

This catches T11 for scheduler-level drift that source goldens cannot see
(goldens hash states, not graphs). Note honestly what it cannot do: it pins
*topology*, not the semantics bevy_ecs attaches to a topology — that half
stays with D14 pins + upgrade conformance runs.

---

## 4. Enforcement mechanisms, each tied to its threat

"Proposed" mechanisms are specced here and prototyped where marked; they are
**not implemented** in this tree, and their landing is implementation work
sequenced by A11 after the P4 window (digest constraint, A1 §7.3).

| # | Mechanism | Threats | Status | Named check that enforces it |
|---|---|---|---|---|
| E-M1 | **Role-keyed static gate** — §5's replacement: full clause battery (bevy-free for verifiable-tier crates, VC-4/6/8, neighbour ban) applied to a *discovered* crate set | T1 T2-spelling T4 T10 | exists (clauses) + proposed (discovery; prototype live) | `scripts/core-gates.sh` clauses 1–5 + new discovery cross-check |
| E-M2 | **Ambiguity rejection at Error + canary mutation test**: every canonical schedule builds with `ScheduleBuildSettings { ambiguity_detection: LogLevel::Error }`; CI carries a test that asserts the real schedule initializes Ok *and* that a deliberately un-ordered mutant initializes Err | T3 | proposed; prototype proven both directions (§9 E-1) | host crate's `canonical_schedule_rejects_ambiguity` test |
| E-M3 | **Projection differential harness**: per commit, run the canonical world twice from one state with permuted insertion orders and assert only what must hold: the sorted-by-`PersistId` projection hash agrees across both runs *and* matches the executor-computed chain. Whether naive query-order folds agree is deliberately not asserted — their agreement would be luck (P3's lesson), not a property | T2 T13 T14 | proposed; prototype pattern proven (§9 E-2/E-3) | conformance case `projection-order-permuted` (A10 lands it) |
| E-M4 | **Double-run symptom tests**: identical tick twice in one process, compare hashes | T1 T4 T6 symptoms | exists | `executor.rs:489 the_same_tick_run_twice_produces_the_same_state` (+ games/conformance equivalents) |
| E-M5 | **Committed golden corpus over every-tick chains**, checked per platform inside ordinary tests | T5 T11 T12-partially | exists | `orrery_conformance --test conformance :: this_platform_matches_the_committed_golden` (mutation-proven A1 M7) |
| E-M6 | **Cross-platform matrix + partial-refusal verdict** | T5 | exists | ci.yml determinism + verdict jobs (≥3 non-baseline reports or fail) |
| E-M7 | **Nightly soak** — ten corpus repeats in one process | per-process nondeterminism (T1/T4/T9 symptoms) | exists | nightly.yml `determinism-soak` (:1187+) |
| E-M8 | **Profile-parity leg + overflow policy**: release-profile corpus run compared against debug digest; canonical crates adopt an explicit overflow policy (`overflow-checks = true` in *all* profiles recommended, so wrap-vs-panic can never split two hosts) | T12 | proposed; hazard demonstrated on this tree (§9 P-OV) | new matrix leg `profile-release`; policy lands as `[profile]` override |
| E-M9 | **Worker-count leg**: corpus emitted under single-threaded and multithreaded executors, digests compared | T13 | proposed; prototype shows stability only under ordering (E-2), never trusted alone | conformance `--workers w1/w4` labels compared by verdict job |
| E-M10 | **Stage-pinned structural flush + FWW install**, isolated-replay equality axis | T8 | spec'd here (§3.4); isolation axis already mechanical today | corpus `combat-isolated == combat-island` chain-equality test |
| E-M11 | **Next-tick event delivery, observer-free canonical path** | T7 | spec'd here (§3.5); detailed command/event semantics owned by A6 | review + E-M4/E-M5 symptoms |
| E-M12 | **Dependency pins + upgrade conformance** (D14 pins, Cargo.lock, vendored forks) + **schedule digest** (§3.10) | T11 | pins exist; digest proposed | goldens (existing) + manifest assertion (A8 format) |
| E-M13 | **Witnessing itself** — re-execution against subject-signed logs, bands comparator, shadow-mode reports | all of the above, post-hoc, cross-machine | exists, shipped (D10 pipeline) | `witness detection.rs` suite (25 tests; mutation-proven A1 M8) |

The pattern worth stating: spelling gates (E-M1) catch classes cheaply at
commit time; symptom tests (E-M4/E-M5) catch what spellings miss; harnesses
(E-M6/E-M7/E-M9) separate platform from process from worker effects;
witnessing (E-M13) is the runtime backstop when everything upstream failed or
was lied about. No single layer is trusted alone — that is the existing
philosophy (core-gates.sh:22-25) carried forward.

---

## 5. The replacement for the Bevy-in-dependency-graph gate

### 5.1 What clause 1 actually enforces today

The property is real: *no engine in the graph of a crate whose code the
adjudicator re-executes*, because "the same build links into peers, field
hosts and persistd" (ruleset.rs:3-6) is what makes verdicts portable. The
mechanism is `cargo tree -p $crate | grep -qi bevy` over three typed names
(core-gates.sh:37, :71-75).

The mechanism's coverage, however, is exactly its list. §1.2 verified the
witness-shaped escape (530 bevy refs, gate green), and the same shape applies
to every future crate: an `orrery_sim_host` carrying bevy_ecs tomorrow passes
today's gate exactly as orrery_witness does today. **A weaker gate that
passes is worse than this one** (epic constraint) — but so is an equally
strong gate that watches less than it appears to.

### 5.2 The proposed replacement: two tiers, role-keyed membership

**Tier V — verifiable core.** Crates whose sources define or implement
canonical rules execution get the full existing battery unchanged: bevy-free
graph, VC-4, VC-6, VC-8, neighbour ban. What changes is membership:

- *Discovery scan:* walk workspace crates; strip whole `#[cfg(test)]`
  modules by brace counting; flag any crate containing `trait Ruleset` or an
  `impl … Ruleset for` site (qualified paths included). Discovered set ∪
  declared set = scanned set.
- *Cross-check both ways, two-source by construction:* an impl-bearing crate
  that was not scanned fails ("undiscovered ruleset crate — add it to the
  gate or justify"); a declared crate with no impl site fails as stale.
  Neither side can pass by agreeing with itself, the same property
  check.sh --self-test relies on for the workspace table.

Prototype evidence (§9 E-D1/E-D2): on this tree the discovery reproduces
exactly `{orrery_core, orrery_games, orrery_conformance}` — including
excluding persistd's test-only macro impl (src/adjudication.rs:771, inside
`#[cfg(test)] mod tests` at :748) and core's own cfg-test executor impls —
while catching a synthetic new `impl Ruleset` crate that the typed list
misses.

Known implementation wrinkles found while prototyping, stated rather than
hidden: (a) item-level `#[cfg(test)]` annotations (vs module-level) need the
stripper extended — fail-loud-and-fix-the-scanner is the right response to a
false positive, never narrowing the pattern; (b) qualified-path impls must be
matched (my first prototype missed them; §9 E-D1 records the fix); (c) the
scanner greps text, so a crate could evade by constructing the trait name
dynamically — accepted residual risk, identical in kind to every grep gate
here, backstopped by symptom tests.

**Tier H — host machinery (landed 2026-08-31 as `scripts/core-gates.sh`
section 6, #771, armed per declared host — D43 (e)(1); at plan time it
existed only if A3's trigger T3 ever fired, and the admitted host was
sanctioned directly instead).** A crate hosting canonical state in a
`bevy_ecs::World`:

- must appear on an explicit, review-required host allowlist (no discovery
  here: hosting ECS is always a decision);
- may depend on `bevy_ecs` only — `bevy_app`, `bevy_internal`, `bevy_time`,
  full `bevy` remain hard failures (keeps SubApp-style app coupling out per
  second-opinion E-9);
- inherits the full Tier V source battery over its canonical modules;
  additionally bans tokio/async-std (T9) and any RNG construction outside
  `tick_rng` (T6);
- must carry E-M2's canary test and E-M3's projection differential harness,
  wired into CI like the corpus;
- must expose single-entity step semantics to witnesses/adjudication — the
  per-entity replay contract (A3 E-8 / second-opinion E-8) is not renegotiable
  at this node; the rollback unit itself stays A7's.

Tier H is no longer vacant: it landed (#771) and arms per declared host
(`DECLARED_HOST_CRATES` — `orrery_sim_host` holds the row), with the host
admitted by owner sanction (#757) rather than a fired trigger. The
witness-adapter exception stands unchanged.

### 5.3 Strength accounting against today's gate

| Property | Today (clause 1 et al.) | Replacement | Verdict |
|---|---|---|---|
| No engine in the verifiable core's graph | enforced on 3 typed crates | enforced identically on discovered+declared set | **equal in kind, stronger in coverage** |
| New ruleset-hosting crate escapes silently | escapes (G9-shape) | caught by discovery (E-D2) | **strictly stronger** |
| Witness adapter carrying bevy | unwatched | still legal (engine calls core, never the reverse — dependency direction preserved), now *visible* as a named exception rather than an accident of the list | equal behaviour, honest accounting |
| Ambiguous schedules | nothing (class doesn't exist in today's architecture) | mechanically rejected + canary-tested (Tier H) | **new enforcement where today there is none** |
| Storage-order dependence | nothing | projection differential harness (Tier H) | **new enforcement** |
| Ambient inputs, unordered collections, transcendentals, live neighbour reads | enforced | unchanged | equal |
| Async runtime in canonical graph | structural only (no tokio dep) | dependency-scan clause | stronger |

**Verdict: the replacement is stronger than today's gate**, in exactly the
sense the epic demands — and one honest caveat belongs next to that word.
In *kind*, Tier H admits `bevy_ecs` somewhere clause 1 admitted zero bevy
crates; if the baseline were "the gate as documented," admitting anything is
weaker. But the operative baseline is the gate as *behaving*: §1.2 shows the
enforced property already excludes whatever isn't typed into the list, so the
escape hatch exists today and is simply unwatched. The replacement converts
that silent hole into (a) a closed hole for rules code (discovery) and (b) a
watched, constrained door for machinery (Tier H). Weaker nowhere that today's
gate actually bites; strictly stronger at the edges; new mechanical coverage
of two hazard classes today's architecture doesn't even contain. That is the
claim, with its evidence, and A3's precondition is met by it *conditionally*:
if the owner rejects role-discovery (e.g. prefers a manifest marker over
scanning), the fallback — declared-list-only with a stale-entry check — is
merely equal-in-kind-plus-new-Tier-H-checks, which still satisfies "at least
as strong" but should then be recorded as such, not as stronger.

### 5.4 Sequencing

`scripts/core-gates.sh` is outside the P4 digest (p4-ledger.sh hashes
`orrery_witness`, `orrery_core`, `orrery_games`, `gates/p1-swarm`; scripts/
is not hashed — p4-ledger.sh:33-35). The discovery clause can land whenever
review allows; Tier H was to land only with, and gated behind, an actual
ECS-host trigger — overtaken 2026-08-31: it landed (#771) with the host
admitted by owner sanction (#757), arming per declared host (D43 (e)(1)).
Neither touches a hashed crate.

---

## 6. Repeatability test matrix

The acceptance criterion asks for worker counts and build profiles
explicitly. Today's harnesses cover platform and repeat axes thoroughly and
the other two not at all — which is honest today (canonical execution is
single-threaded and profile-uniform by construction) and insufficient the day
an ECS host lands. The proposed matrix; implementation owned by **A10**
(#406), specified here:

| Axis | Values | Mechanism | Status |
|---|---|---|---|
| Platform | x86_64-linux, aarch64-linux, x86_64-windows, aarch64-macos | ci.yml determinism matrix + digest compare + partial refusal | **exists** |
| Process repeats | 10 in-process repeats | nightly soak (:1187+) | **exists** |
| In-run repeats | identical tick ×2 in tests; 200-world probes for schedule classes | double-run tests; probe-style CI test if Tier H lands | exists / conditional |
| **Workers** | single-threaded executor; multi-threaded at 2 and ≥4 tasks | corpus `emit --workers {s,w2,w4}` labels compared by the verdict job; requires a multithreaded-capable canonical host, i.e. only meaningful under Tier H | **proposed** (E-M9) |
| **Build profile** | dev; release (with overflow-checks policy applied to canonical crates) | release-profile corpus leg; digests must equal dev's bit-for-bit on discrete axes and within bands on continuous ones | **proposed** (E-M8); hazard demonstrated §9 P-OV |
| Insertion order | forward vs fixed permutation of spawn sequence | new corpus case `projection-order-permuted` asserting chain equality with its forward twin (E-M3 pattern at corpus scale) | **proposed** |

Design rule carried from A3 P3: every axis pair that *agrees* is recorded,
but no agreement is ever load-bearing by itself — the mechanical checks
(E-M1/E-M2/E-M3) carry the proof, the matrix catches what they cannot see.
A partial matrix must refuse to pass (existing verdict-job behaviour extended
to new axes).

---

## 7. Why witnessing remains canonical (and ECS ordering cannot replace it)

Even with every mechanism above landed, witnessing stays the arbiter. Five
reasons, each independent of the others:

1. **Determinism ≠ honesty.** Schedule guarantees constrain how *given*
   inputs execute; they say nothing about whether inputs were fabricated or
   state mutated off-log. A cheating authority runs a perfectly deterministic
   simulation of its lies. The strike pipeline exists precisely for that
   case: claims are subject-signed commitments (state_hash over quantized
   bytes), and a witness re-executes the signed log and compares
   (witness.rs stage pipeline; verify_bundle semantics, docs/06:499). No
   ordering property produces a verdict.
2. **Observed stability proves nothing** — this task's own inherited
   negative result, re-demonstrated here: ambiguous schedules ran 200/200
   identically on A3's box, and my E-3 probe initially "passed" twice due to
   two different probe bugs before showing the real divergence. Symptom-free
   operation is not evidence anywhere in this domain; adjudication is the
   layer that doesn't need to trust anyone's stability.
3. **Isolation is the contract, and no schedule reproduces it.** The
   adjudicator replays exactly one entity against an empty neighbour map
   (replay.rs:106-130); the corpus asserts shared-vs-isolated chain equality
   as a property of the rules, not of the scheduler. An ECS schedule's
   guarantees are about *its* world; the verdict must hold in a world of one.
   That is why Tier H must expose per-entity steps (§5.2) rather than offer
   "the schedule was deterministic" as a substitute.
4. **Continuous state needs bands, not orders.** Cross-platform, raw float
   trajectories cannot promise bit-equality (docs/06:268); the comparator's
   ε-bands + sustain windows are what keep honest players unstruck while
   catching drift farming. Ordering has no answer to "how different is too
   different."
5. **Witnesses run where trust doesn't.** A witness peer holds its own
   executors fed by signed frames — it never imports the authority's
   scheduler, world, or module set. ECS execution happens *inside* someone's
   process; witnessing works *across* processes that distrust each other.
   The second is strictly more than the first.

So: ECS-ordering machinery (E-M1..E-M12) makes re-execution *reproducible*;
witnessing (E-M13) makes it *meaningful*. The first is infrastructure for the
second, never a replacement.

---

## 8. What this document deliberately does not decide

- **Identity**: `PersistId` ↔ ECS entity mapping ownership, allocation
  classes, tombstone semantics — **A5 (#401)**.
- **Per-component capability/policy shape**, including whether `CoreClass`
  survives as the Tier-H routing hook — **A5 (#401)**.
- **Rollback unit** (world/island/cell/entity/component subset) and the
  canonical witness projection format — **A7 (#403)**. This document pinned
  only that S4-before-S5 and next-tick delivery are non-negotiable because
  adjudication consumes them.
- **Command/event detailed semantics** — replay, dedup, idempotency keys,
  volume bounds beyond ordering/delivery timing fixed in §3.5 — **A6 (#402)**.
- **Manifest format** carrying the schedule digest (§3.10) — **A8 (#404)**.
- **Conformance/matrix implementation** (E-M3, E-M8, E-M9 legs) — **A10
  (#406)**.
- **ADR acceptance**, including whether the §5 replacement amends the D9/D15
  record set — reserved to the owner.

---

## 9. Mutation and probe log

### 9.1 Re-based predecessor mutations

`git diff ce5e34a7..HEAD -- crates gates clients` is empty (post-A3 commits
touched only `scripts/check.sh`, `scripts/ci-changed-code.sh`,
`scripts/gate-status.sh`, and docs). A1 M1–M8, A2 M-A/M-B/M-A′ and A3 F-1/F-2
therefore carry over (A3's prototype entries E-1–E-6 are evidence records,
reproduced here only where load-bearing) at full strength, including their reverts.
`core-gates.sh` itself is byte-identical since `ce5e34a7`.

### 9.2 This document's own runs

| # | Claim demonstrated | Method | Result |
|---|---|---|---|
| **M-G1** | Clause 1 live on this tree, dev-deps included | `[dev-dependencies] bevy_ecs = { workspace = true }` appended to `crates/orrery_games/Cargo.toml`; ran `./scripts/core-gates.sh` | mutated: `core-gates: orrery_games has Bevy in its dependency graph`, exit 1 · reverted: exit 0 |
| **E-D1** | Role-discovery reproduces the gated set from the tree, cfg(test)-aware | prototype scanner `/tmp/opencode/gate-proto/ruleset-impl-crates.sh` over this workspace | exactly `orrery_conformance orrery_core orrery_games` (persistd's test-only impl at src/adjudication.rs:771 correctly excluded; core included via the trait definition at ruleset.rs:233). First version missed qualified-path impls (`impl foo::Ruleset for`) — pattern widened, recorded as a wrinkle in §5.2 |
| **E-D2** | Discovery catches a crate the typed list misses (the G9-shaped escape) — both directions | synthetic workspace with new `orrery_host` carrying `impl orrery_host_rules::Ruleset for Host` in `src/` | typed-list scan would see only `orrery_core`; discovery reports `orrery_core orrery_host`. Impl removed → discovery drops it; restored → returns. Both directions shown |
| **E-1** | `ambiguity_detection = Error` mechanically rejects an ambiguous schedule and accepts a totally-ordered one | scratch crate `/tmp/opencode/gate-proto/amb-probe` (`bevy_ecs = "=0.19.1"`, multi_threaded): two systems with conflicting access + order-sensitive effects, built once without edges, once chained | guard live: ambiguous → `initialize = Err (rejected)`, ordered → `Ok`. Mutation direction: rebuilt with `LogLevel::Ignore` → ambiguous builds Ok and the assertion `expected rejection after breaking the guard — this line is the mutation check` panicked, exit 101. Both directions recorded |
| **E-2** | Ordered canonical schedule: one hash across executor configs and insertion orders | same probe: 500 entities, forward vs fixed-permutation spawn order × {single-threaded, multi-threaded×2 configs} | `canonical=f6a3…` identical in all six cells. Recorded as repeatability evidence, *never* as proof of determinism by stability |
| **E-3** | Query iteration order reaches naive hashes; sorted projection neutralizes | same probe: blake3 fold of `(id,pos)` rows in raw query order across the two insertion sequences | forward `f6a3…` vs permuted `d243…` — differ. (Probe honesty note: my first two scramble implementations accidentally produced id-sorted or id-changed populations and E-3 "passed"/failed for wrong reasons; both were caught because the probe asserts its own expectations — recorded because it is §7's point made about my own tooling.) A3's P1/P2 found the same at larger scale |
| **P-OV** | Build-profile divergence is real on cargo defaults | `/tmp/opencode/gate-proto/ovf-probe`: `black_box(i32::MAX) + 1000` under dev vs release | dev: panic `attempt to add with overflow`; release: prints `healed=-2147482649`. Root workspace sets no `[profile]`, so both behaviours are what canonical integer math would do today if it could overflow |

No repository file was modified by any probe; all lived in
`/tmp/opencode/gate-proto/`. The single repo mutation (M-G1) lived for one
command run and was reverted with its passing result re-run.

---

## 10. Stale citations found while verifying

| Record | Citation / claim | Current truth |
|---|---|---|
| A3 primary doc | `ci.yml:673-735` for the determinism matrix | Job moved to `ci.yml:726+` by bf22ee3a (doc-only-CI commit added ~53 lines above it); claim unchanged, line drifted |
| Issue #400 text | "core-gates.sh declares GATED_CRATES=(orrery_core orrery_games orrery_conformance)" — hardcoded | Verified true verbatim (core-gates.sh:37), including that witness's bevy rides past it |
| Epic #395 constraint block | "bans `view.neighbor(` in the rules crates" | True (:137-139), scoped by RULES_CRATES :42 |
| Inherited-stale set (A1/A2/A3 records): ADR-0038 `ruleset.rs:211` drift; D21's `validate_intent` parenthetical; docs/06:210 present-tense classify_component consumers; docs/10-crates.md `orrery_field_host` rows; brief's `p{N}-*` paths; bot.rs producer line drift | — | Not re-litigated; nothing this document relies on touches them beyond what predecessors recorded |

No stale citation was found in AGENTS.md relevant to this node.

---

## 11. Unsure

Stated as unsure rather than smoothed over:

1. **The discovery scanner's cfg(test) handling is a prototype.** Brace-count
   stripping handles module-level annotations (the tree's only shape today);
   item-level `#[cfg(test)]` functions containing impls would false-positive.
   Fail-loud is acceptable, but the implementation must treat scanner
   false positives as scanner bugs, not add exclusions.
2. **E-2's stability is one box, one build, tiny systems** — exactly the
   class of evidence P3 warned about. It justifies the worker axis' design;
   it proves nothing beyond it.
3. **Schedule digest placement in the wire/manifest** is proposed, not
   decided: whether it joins `protocol_accepted` equality or rides the
   manifest alone is A8/owner territory, and I did not want §3.10 to quietly
   become a protocol change.
4. **Overflow policy choice.** I recommend `overflow-checks = true` in all
   profiles for canonical crates; a game needing wrapping semantics could
   instead pin `wrapping_*` explicitly. Either works; silence does not. The
   owner should pick.
5. **Tier H is no longer conditional-vacant.** At plan time, had no trigger
   ever fired, every Tier-H clause here would have been unused specification —
   that posture was deliberate (A3 V5's). Since then ECS was admitted: the
   host by owner sanction (#757), the battery by #771 — enforced and
   demonstrated mutation-style in full (D43 clause (e)'s amendments), so
   this document's *new* enforcement is no longer untested against
   production pressure.

Deliberately not done:

- **No implementation.** No clause of core-gates.sh changed; no corpus case,
  CI leg, or host API exists yet — those belong to A10/A11 after acceptance.
- **No decision owned elsewhere** (§8).
