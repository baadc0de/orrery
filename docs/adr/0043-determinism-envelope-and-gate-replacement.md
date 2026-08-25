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

> *Amended by the owner on 2026-08-25 (through #444/#457, executing #390's
> approved design): the neighbour ban is narrowed from a categorical refusal
> of `view.neighbor(` in every Tier V crate to a refusal of every **unrecorded
> or unadjudicable** neighbour read. The ban's stated rationale was that a
> neighbour read could not be adjudicated — no `NeighborFrame` producer
> existed and `ReplayHarness::load_claimed_snapshot` installs exactly one
> entity, so a rule that branched on a neighbour resolved differently under
> replay than under play, convicting an honest peer. That premise no longer
> holds: `Executor` emits the neighbour tick actually observed, the replay
> harness serves those frames without installing a live world, and
> `cross_check_neighbor_record` verifies each frame against the claim signed
> by the neighbour's own authority for the tick the reader declares. Staleness
> is tested before state hashes, so ordinary replication lag is refused as
> uncheckable rather than turned into a deviation verdict.*
>
> *The acceptance bar above is unchanged and binds the narrowed form: the gate
> stays two-sided. It scans every Tier V crate, refuses every read outside the
> audited site, and **pins the permitted count at exactly one**, so a second
> site fails whether it is added at the audited path or anywhere else. A path
> allowlist that merely tolerated additional hits would be the weaker gate this
> bar forbids; this one is not that. Admitting a further site is an amendment
> to this clause, not a configuration change.*

> *Further amended by the owner on 2026-08-25 (through #441/#468): **the
> exactly-one count is withdrawn** and replaced by a declared list of audited
> **predicates**, each named as `path::function` in
> `AUDITED_NEIGHBOR_PREDICATES` in `scripts/core-gates.sh`. A read outside a
> declared predicate fails, naming the offending function; a declared predicate
> that no longer exists fails as stale.*
>
> *The count is withdrawn because it measured text rather than behaviour, and
> the amendment above overstated what it bought. The quantities that matter are
> enforced at the replay layer and always were: `max_neighbor_reads` caps how
> many frames a tick may pull in, `max_neighbor_staleness_ticks` bounds how old
> one may be, and `cross_check_neighbor_record` verifies each frame against the
> neighbour authority's signed claim. A site count bounds none of those — one
> expression can read a hundred neighbours, and a hundred expressions reading
> one each are identically safe. #441 demonstrated the gap directly: folding
> three lookups into one expression widened the audited predicate to a third
> entity while "exactly one site" still passed. A check satisfiable by
> reformatting is not an invariant.*
>
> *What survives is the property the count was standing in for: **no code reads
> a neighbour without a human seeing it**. Adding a predicate is a one-line diff
> to a declared list, which is a stronger review trigger than a number, and it
> avoids forcing every future neighbour-reading feature into one god-predicate —
> several small named predicates review better than one that does everything.
> The acceptance bar still binds: this is not a weaker gate that passes, it is
> the same tripwire stated in terms of what it actually checks. Adding a
> predicate to the list remains an ordinary reviewed change; removing the
> declaration requirement would be a further amendment.*

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

### (f) Integer overflow — a flag in witnessed state, one behaviour everywhere

A4 §11.4 offered two postures — `overflow-checks = true` in all profiles
(panic) or explicit `wrapping_*` — and reserved the choice. **The owner has
chosen: a flag, not a loud failure.** The clause, and the consequence that
makes it more than a preference:

1. **No panic.** Canonical crates must not resolve overflow by aborting the
   tick. An overflowing operation produces its defined result (sub-clause
   (f)(4)) and execution continues.
2. **One behaviour across all profiles.** The demonstrated hazard (Context
   §3, re-verified: dev panics, release wraps to `-2147482649`) is the
   *profile-dependence*, regardless of posture. Therefore: canonical integer
   arithmetic that can overflow uses the explicit-semantics operations that
   carry the chosen posture and set the flag; and the canonical crates' build
   pins `overflow-checks = false` uniformly across profiles, so that any
   stray plain operation the review missed behaves *identically* on every
   host and profile (it wraps) instead of splitting dev from release.
   `overflow-checks = true` is unavailable under this clause — it means
   panic, which (f)(1) bars. A stray plain operation is still a bug (it
   bypasses the flag and, if saturating is chosen, the posture); the pin
   turns it from a divergence into an ordinary defect.
3. **Occurrence reaches witnessed state, or the flag is theater.** This is
   the part that needs care. A flag is only evidence if it is part of the
   hashed projection: if overflow set a bit that hashing never sees, two
   hosts could diverge — one flagged, one not — while `hash(e, t)` still
   matches, and the flag would prove nothing precisely when it matters.
   Therefore the flag is a **per-entity discrete field of canonical state**:
   a saturating overflow counter (or bitset — implementation's choice of
   width, not of location), set during S2 at the point of occurrence, carried
   in the entity's state exactly like any other discrete field. Under R7's
   projection rules (a7-persistence-rollback-witnessing.md §5) it is then
   inside `bytes(e,t) = CoreCodec::encode(quantize(state(e,t)))` and hence
   inside `hash(e,t) = blake3(bytes(e,t))` — the value a `StateClaim` commits
   to (WP-1). It is an integer, so S4 quantization is the identity on it and
   ring-2 comparison is `==` (VC-5, discrete axis — no band ever applies to
   it). Persistence and replication see the same bytes by construction
   (WP-3's one-sentence property), so a flagged entity is flagged in the
   at-rest row, the replicated state, and the witness's re-execution alike;
   a host that disagrees about occurrence produces a differing hash and the
   ordinary deviation pipeline adjudicates it. Setting the flag is itself
   deterministic — occurrence is a function of (state, inputs, TickRng), all
   sealed — so honest hosts agree on the flag the way they agree on any
   state bit.
4. **Wrapping vs saturating — reserved to the owner, undecided.** The owner
   has fixed the posture (flag, no panic, profile-uniform) but has not chosen
   the defined result of an overflowing operation, and this record does not
   choose it. The two candidates differ in what a game's arithmetic *means*
   at the boundary, not merely in mechanism — see Alternatives. Until the
   owner records the choice, no canonical arithmetic may be written that
   depends on which one wins; implementation of this clause blocks on that
   answer.

The flag's *placement* here is spec-level: "a discrete field of canonical
state, inside the claimed bytes". Which struct field, its width, and the
`SchemaVersion` bump it implies are implementation work after acceptance, and
the projection format it rides inside remains R7's.

### (g) The schedule digest — existence and content; storage is R8's

The composition root computes a **schedule digest**: blake3 over a canonical
serialization of the ordered stage list; the per-stage ordered system names;
all declared ordering edges, sorted lexicographically; the
ambiguity-detection setting; and the executor policy. It exists to catch
scheduler-topology drift that state goldens cannot see — goldens hash states,
not graphs (T11).

Uses, with ownership kept exactly as A4 §3.10 and A8 drew it: the digest is
pinned into the game manifest, whose **format and storage are R8's** (A8);
it is asserted by a unit test against the current value, so an accidental
system reorder fails CI the way a golden does. Whether it also joins the
session-setup equality check on the wire is a protocol question this record
does not decide (see Open questions). Stated honestly: the digest pins
*topology*, not the semantics the ECS library attaches to a topology — that
half stays with [D14]'s pins and upgrade conformance runs.

## Consequences

- The Bevy-free property of the verifiable core stops being a per-commit
  decision (remember to type the crate name) and becomes a property of the
  tree (an impl-bearing crate cannot exist unscanned). The witness adapter's
  530-reference ride past the gate ends as an accident and continues as a
  named, watched exception.
- **The gate gains a scanner, and the scanner is now load-bearing.** A
  scanner bug is a gate hole; the cross-check's two-source construction and
  the fail-loud rule are the mitigations, but maintaining a textual Rust
  scanner in bash is a real ongoing cost the typed list never had.
- **Most of the new enforcement is dormant until an ECS trigger fires**
  (clause (e), stated there). If no trigger ever fires, this record's lasting
  deltas are discovery, the async clause, the overflow clause, and the
  digest — the Tier H battery stays paper.
- Every canonical integer operation that can overflow must be written with
  explicit semantics and flag-setting; that is a coding-discipline cost paid
  on every rules crate forever, and it **blocks on the owner's (f)(4)
  answer** before any of it can be implemented.
- The overflow flag widens canonical state: a schema change
  (`SchemaVersion` bump under [D38]'s versioning), a projection change
  (`projection_version` bump under R7's WP-6 if the framing moves), and one
  more discrete field every claim commits to — the cost of making the flag
  mean something.
- A flagged entity is *not* a determinism failure: two honest hosts both
  flag it and both hash identically. The flag is telemetry-with-teeth — it
  makes overflow *visible and attributable* in witnessed state; deciding what
  standing or gameplay consequence follows is rules-design work outside this
  record.
- The schedule digest turns an innocent system reorder into a CI failure and
  a manifest change. That is its purpose; it is also a new way for a
  refactor to be louder than its author expected.
- Acceptance arms a11's PR-6 (the discovery clause in `core-gates.sh`),
  which was explicitly gated on this record because it edits a live gate.

## Alternatives considered

- **Keep the typed list, add a stale-entry check.** The recorded fallback if
  role-discovery were rejected: equal in kind plus the new Tier-H checks,
  which still meets "at least as strong" — but it leaves the G9-shaped
  escape open (a new impl-bearing crate passes silently) and would have to
  be recorded as *equal*, not stronger. Rejected in favour of discovery.
- **A manifest marker instead of scanning** (crates self-declare "I am
  rules"). Rejected: self-declaration is the typed list with extra steps —
  the crate that forgets to declare is exactly the crate the gate exists to
  catch. Discovery keys on what the code *is*, not on what it says it is.
- **`overflow-checks = true` in all profiles (panic posture).** A4's own
  recommendation; honest, loud, profile-uniform — and **rejected by the
  owner**: canonical crates must not resolve overflow by aborting the tick.
  A panic in S2 takes the entity (and under a shared schedule, the tick)
  down with it; the flag posture keeps the simulation running and the
  occurrence adjudicable.
- **A flag outside the hashed projection** (log line, metric, side table).
  Rejected for the reason worked through in clause (f)(3): divergence with
  matching hashes is invisible, so the flag would prove nothing exactly when
  it mattered.
- **Wrapping semantics for the defined result** — presented, not decided
  ((f)(4)). `wrapping_*`: the result is the two's-complement wrap.
  *For:* it is what the hardware does, what `overflow-checks = false` plain
  ops already do (so a stray op matches the posture), zero-cost, and
  bit-exact trivially. *Against:* the wrapped value is semantically garbage
  for almost every game quantity — a resource count of `i32::MIN + 999` is
  not a boundary value, it is nonsense with a flag next to it, and every
  consumer downstream must treat flagged state as suspect.
- **Saturating semantics for the defined result** — presented, not decided
  ((f)(4)). `saturating_*`: the result clamps to the type's bound.
  *For:* the value stays meaningful ("as much as fits"), which is usually
  what game arithmetic wants at a boundary; flagged state remains usable.
  *Against:* saturation destroys information (x + 1 − 1 ≠ x at the bound)
  and silently changes arithmetic identities, a stray plain op now *differs*
  from the posture (it wraps), and clamp-then-continue can mask a rules bug
  that wrap-plus-flag would have made grotesque and therefore noticed.
  These differ in what arithmetic *means* at the boundary; the owner picks.
- **Widen ring 2 to bit-exact floats everywhere.** Rejected: three OSes ×
  two architectures cannot promise raw-float bit-equality; that is the
  promise the physics ecosystem itself scopes away, and [D16]'s bands exist
  because of it.
- **Let ECS ordering guarantees stand in for witnessing.** Rejected (A4 §7,
  incorporated): determinism is not honesty; a cheating authority runs a
  perfectly deterministic simulation of its lies. Everything in this record
  makes re-execution *reproducible*; witnessing makes it *meaningful*.

## Open questions reserved to the owner

1. **(f)(4): wrapping or saturating** as the defined result of an
   overflowing canonical operation. Undecided; implementation of clause (f)
   blocks on it.
2. **Schedule-digest wire placement**: whether the digest joins the
   session-setup equality assertion alongside `RulesetId`, or rides the
   manifest alone. A protocol question, flagged in A4 §11.3 and left with
   A8/the owner.

## Verification appendix — what was re-run at acceptance

All on this tree (branch `docs/adr-0043-r2` at `2c31b4aa`), 2026-08-25:

| Check | Result |
|---|---|
| `cargo tree -p orrery_witness \| grep -ci bevy` | **530** |
| `./scripts/core-gates.sh` (unmodified tree) | exit **0**, all five clauses green |
| Mutation: `[dev-dependencies] bevy_ecs` appended to `crates/orrery_games/Cargo.toml` | named check died: `core-gates: orrery_games has Bevy in its dependency graph`, exit **1** |
| Revert of the mutation | exit **0**; working tree clean |
| Overflow probe (scratch crate, cargo defaults): `black_box(i32::MAX) + 1000` | dev: panic `attempt to add with overflow` · release: `healed=-2147482649` |
| Workspace `[profile]` override | none exists in root `Cargo.toml` |
| `GATED_CRATES` / `RULES_CRATES` | `core-gates.sh:37` / `:42`, verbatim as quoted |
| Quantize-before-hash | `crates/orrery_core/src/executor.rs:126-127` |
| FWW materialization | `crates/orrery_core/src/executor.rs:144-157` (`Entry::Vacant` install) |
| `tick_rng` derivation | `crates/orrery_core/src/rng.rs:31-43` |
| P4 pipeline trees | `scripts/p4-ledger.sh:409-414`: `crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games`, `gates/p1-swarm`; `scripts/` not hashed |

One citation drift found while verifying: A4's method note says the prototype
pinned `bevy_ecs = "=0.19.1"` "matching root `Cargo.toml:60`"; the manifest
line actually reads `bevy_ecs = { version = "0.19", default-features =
false }` — the exact `0.19.1` lives in `Cargo.lock:1224-1225`, which A4 also
cites correctly. Claim intact, manifest half imprecise; recorded here rather
than silently repeated.

[D9]: 0009-verifiable-core.md
[D14]: 0014-pinned-versions.md
[D15]: 0015-crate-set.md
[D16]: 0016-parameter-reference.md
[D38]: 0038-at-rest-schema-versioning.md
[D42]: 0042-canonical-simulation-architecture.md
