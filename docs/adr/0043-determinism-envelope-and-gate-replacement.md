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

## Decision

### (a) The determinism envelope — three rings, and an explicit outside

What "deterministic" promises, and to whom (A4 §3.1, matching [D9]'s scoping
and docs/06-verifiable-core.md §5):

1. **Ring 1 — in-process (one binary, one machine): bit-exact.** Same inputs
   produce the same state bytes, the same event sequence, the same hashes —
   across runs, worker counts, executor kinds, and insertion orders. No
   tolerance anywhere in this ring. This is what the double-run tests and the
   nightly soak assert, and it is what makes ring 2's corpus comparable at
   all.
2. **Ring 2 — across the supported platform matrix: discrete bit-exact,
   continuous within bands.** Four targets (x86_64 Linux/Windows, aarch64
   Linux/macOS; x86_64-macOS deliberately unsupported), pinned toolchain and
   dependencies ([D14]). Discrete state (VC-5) compares `==`; continuous
   state (VC-6/VC-7) is libm-routed, lattice-snapped each tick, then compared
   under [D16]'s bands (ε_pos 1 cm, ε_vel 1 cm/s).
3. **Ring 3 — explicitly outside the envelope:** compiler versions outside
   the pin; modified or third-party rules builds claiming an honest
   `RulesetId` (the tamper model keeps the honest id on purpose — that is
   what witnessing adjudicates, not a determinism failure); fast-math and
   codegen-flag variance; any platform outside the four.

A future ECS host inherits this envelope unchanged. It may not narrow ring 1
to "stable enough per process", and it may not widen ring 2 to "all
platforms" — that is the promise rapier scopes and avian declines.

### (b) Canonical stages S0–S7

A canonical tick is a fixed pipeline over a frozen input set. Under the
current executor the pipeline is implicit in `step_entity`; under any future
host it becomes explicit schedule structure. Either way it is:

```text
S0 SealInputs     freeze this tick's input log in VC-2 order; nothing appends
                  after S0 begins (late arrivals join t+1)
S1 Deliver        apply external commands + last tick's events as inputs, per
                  entity, in log order
S2 Step           per-entity pure step: own state + ordered inputs + TickRng;
                  entity processing order = PersistId ascending; steps are
                  independent (snapshot isolation), so S2 may parallelize
                  across entities
S3 Record         collect neighbour reads for the log (first-read order)
S4 Quantize       snap every continuous field (VC-7) — before any hashing
S5 Claim          compute per-entity state_hash; assemble claims
S6 Materialize    install structural changes: emission/description order,
                  first-writer-wins (executor.rs:144-157 semantics)
S7 Emit           enqueue emitted events as t+1 inputs (delivery strictly
                  next tick)
```

Two properties are **non-negotiable**, because adjudication consumes them:

- **S4 ≺ S5.** Quantize before hash, always (`executor.rs:126-127`; VC-7;
  A7's projection rule WP-4 states the same clause from the projection side).
- **S6 applies no input visible to any S2 of the same tick.** Materialized
  children cannot change a step that already ran; the corpus's
  shared-vs-isolated chain-equality axis pins this mechanically.

### (c) Ordering and prohibition rules attached to the stages

Normative, condensed from A4 §3.3–§3.9; where a rule is another record's to
detail, the boundary is stated.

1. **System ordering.** Within a stage, systems form a total order fixed at
   composition time. Every pair of systems with conflicting data access
   carries an explicit ordering edge; ambiguity is *rejected at composition*
   (error, not log), never ignored — and the rejector itself must be proven
   awake by a canary mutant (A4 E-M2: the real schedule initializes Ok, a
   deliberately un-ordered mutant initializes Err; both directions in CI).
   Observed run-to-run stability of an ambiguous schedule is not evidence of
   anything: A3's probe ran an ambiguous schedule 200/200 identical.
2. **Deferred structural changes.** All spawn/despawn/insert in canonical
   execution goes through deferred commands flushed at **S6 only**; flush
   order = queue order = system order × emission order; identifier collisions
   resolve first-writer-wins by `PersistId` — today's rule verbatim. Direct
   world mutation inside S0–S5 is prohibited; a host exposes command queues
   to canonical systems, not `&mut World`.
3. **Events.** Emission order within a producer; producer total order across
   producers; delivery at S1 of t+1, never earlier; no observers, hooks, or
   immediate cascades in the canonical path. Dedup, replay, idempotency and
   volume bounds are R5's (A6) — this clause fixes ordering and timing only.
4. **Query order.** Query iteration order must never be observable in
   anything leaving the canonical context — claim bytes, persistence rows,
   event payloads. Any projection producing canonical output iterates sorted
   by `PersistId` (cross-grid, `(GridId, PersistId)` — A7 WP-2). Stated
   honestly: this is enforced mechanically where possible (the projection
   differential harness, Tier H clause (e)(4)) and by review beyond that; a
   grep cannot tell an observable iteration from an unobservable one.
5. **RNG ownership.** Per-entity, per-tick stream derived by `tick_rng`
   (rng.rs:31-43) and passed `&mut` into the step; draws are code order; no
   mid-tick reseed; no global RNG resource in canonical stages; no draw count
   depending on cross-entity data-dependent branches.
6. **Floats.** VC-5/VC-6/VC-7 verbatim: integers for discrete outcomes; libm
   routing with std transcendentals banned in both spellings; exact IEEE ops
   (`round`/`floor`/`ceil`/`trunc`/`abs`/`mul_add`) allowed and load-bearing
   for the lattice; quantize-before-hash; [D16] bands for ring-2 comparison.
7. **Async.** Canonical execution is synchronous end-to-end within a tick: no
   async runtime in the canonical graph, no task spawned during S0–S7 that
   outlives the schedule run, no I/O inside canonical stages. The outside
   world enters as sealed inputs at S0 and leaves as events and frames after
   S7.

### (d) Tier V — role-discovered membership replaces the typed `GATED_CRATES` list

This is the record's load-bearing clause. **The epic's standing rule is its
acceptance bar: a weaker gate that passes is worse than the current one.**

The full existing clause battery — Bevy-free graph, VC-4, VC-6, VC-8,
neighbour ban — is kept **unchanged**. What changes is who it applies to:

1. **Discovery scan.** Walk workspace crates; strip `#[cfg(test)]` modules;
   flag any crate whose library sources define `trait Ruleset` or contain an
   `impl … Ruleset for` site, qualified paths included. Crates so flagged are
   Tier V.
2. **Scanned set = discovered ∪ declared.** The declared list survives as a
   floor, not as the coverage. On this tree discovery reproduces exactly
   `{orrery_core, orrery_games, orrery_conformance}` — including correctly
   excluding `orrery_persistd`'s test-only macro impl inside a
   `#[cfg(test)] mod tests` (A4 E-D1).
3. **Two-way cross-check, two-source by construction.** An impl-bearing crate
   absent from the scanned set fails ("undiscovered ruleset crate — add it to
   the gate or justify"); a declared crate with no impl site fails as stale.
   Neither side can pass by agreeing with itself — the same property
   `check.sh --self-test` relies on for its lane table. A4 E-D2 proved both
   directions on a synthetic `impl Ruleset` crate the typed list misses:
   discovery catches it, removing the impl releases it, restoring returns it.
4. **Async clause added.** Tier V crates must have no async runtime
   (tokio/async-std) in their dependency graph — structural today
   (`orrery_core` has none), a scan clause tomorrow (T9).

**Strength accounting, carried from A4 §5.3 with its caveat intact.** The
verdict there is that the replacement is *"equal in kind, stronger in
coverage"* on the Bevy-free property and *"strictly stronger"* at the edges —
and one honest caveat belongs next to that word. In *kind*, Tier H (clause
(e)) admits `bevy_ecs` somewhere clause 1 admitted zero Bevy crates; if the
baseline were the gate *as documented*, admitting anything is weaker. But the
operative baseline is the gate *as behaving*: Context §1 shows the enforced
property already excludes only what is typed into the list — the escape hatch
exists today (530 Bevy references riding past a green gate) and is simply
unwatched. The replacement converts that silent hole into (i) a closed hole
for rules code — discovery — and (ii) a watched, constrained door for
machinery — Tier H. Weaker nowhere today's gate actually bites; strictly
stronger at the edges; new mechanical coverage of two hazard classes
(ambiguity, storage-order dependence) that today's architecture does not even
contain. The witness adapter's Bevy remains legal — engine calls core, never
the reverse — but becomes a *named exception* rather than an accident of the
list.

Known residual risks, stated rather than hidden (A4 §5.2): the scanner is
textual, so a crate could evade by constructing the trait name dynamically —
accepted, identical in kind to every grep gate here, backstopped by symptom
tests; item-level `#[cfg(test)]` attributes (vs module-level) would
false-positive the current prototype stripper, and the required response is
fail-loud-and-fix-the-scanner, never narrowing the pattern or adding
exclusions.

Sequencing: `scripts/core-gates.sh` is outside the P4 pipeline digest
(Context, scope paragraph), so the discovery clause lands when review allows
(a11 PR-6, gated on this record's acceptance); no hashed tree moves.

### (e) Tier H — conditional host battery, armed only by a D42 trigger

Tier H exists only if [D42]'s dedicated-world trigger (T1–T3) ever fires. A
crate hosting canonical state in a `bevy_ecs::World`:

1. appears on an explicit, review-required **host allowlist** — no discovery
   here, because hosting ECS is always a decision, never an accident;
2. may depend on `bevy_ecs` **only** — `bevy_app`, `bevy_internal`,
   `bevy_time`, full `bevy` remain hard failures (keeps SubApp-style app
   coupling out);
3. inherits the full Tier V source battery over its canonical modules, plus
   the async ban and a ban on RNG construction outside `tick_rng`;
4. carries the ambiguity canary test (clause (c)(1)) and the projection
   differential harness — permuted insertion orders must yield equal
   sorted-by-`PersistId` projection hashes matching the executor-computed
   chain, while agreement of naive query-order folds is deliberately *not*
   asserted (their agreement would be luck, not a property) — wired into CI
   as preconditions of admitting the host, not follow-ups;
5. exposes single-entity step semantics to witnesses and adjudication: the
   verdict must hold in a world of one, and "the schedule was deterministic"
   is never a substitute for per-entity replay. The rollback unit itself
   stays R6's (A7).

**Honest accounting this record owes the reader (A4 §11.5, not dropped):**
Tier H is *entirely conditional*. Until a trigger fires, Tier H is empty, the
tree is exactly Tier V, and every Tier-H clause above is unused
specification — which means most of this record's *new* enforcement is
untested against production pressure unless and until an ECS host is
admitted. That posture is deliberate (specify the door before anyone needs to
walk through it), but it is a fact about how much of this record is currently
exercised, and it belongs in the record rather than in a plan's appendix.
