> **Second opinion, produced independently.** This document and
> `a3-simulation-host-comparison.md` were written by two agents working the same
> brief at the same time, in separate worktrees, neither able to see the other.
> The primary document carries the recommendation A4–A11 build on; this one is
> preserved because where the two converge independently, the agreement is much
> stronger evidence than either document alone — and where they differ, the
> difference is itself worth knowing.
>
> **They converge on the action**: keep canonical state in the engine-neutral
> per-entity executor, land the composition root and simulation-host seam now,
> reject the shared Bevy world, and admit a dedicated `bevy_ecs::World` only
> behind pre-registered triggers. They differ on ordering — this document scores
> a hybrid marginally over improved-status-quo and says so is inside its own
> method's noise, naming improved-status-quo as the fallback; the primary scores
> improved-status-quo clearly first and shows it survives a hostile sensitivity
> pass.
>
> This document's distinctive contributions are its topology reframing (the
> dedicated-store shape already ships) and its verified source quote from
> `lightyear_replication-0.29.0`.

# A3 — Simulation-host and world-model comparison (#399)

**Status:** decision proposal for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/399-a3-fable` at `ce5e34a7` · **Parents:**
[#399](https://github.com/baadc0de/orrery/issues/399) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md), [A2](a2-kernel-game-module-ownership.md) ·
**Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)

A1 mapped what exists and A2 assigned ownership; both were forbidden a
position. This node takes one. It compares five simulation-host variants,
weights the comparison dimensions **before** scoring, and recommends one
variant while naming what it beat and why. Accepting or amending anything
here is the owner's call (#395: propose, do not decide).

Method, as in the predecessors:

- Every claim cites a file and line opened on this tree today. Load-bearing
  A1/A2 findings were re-verified here before use (§1.1).
- Where this document claims a gate or property is enforced, the **guarded
  stage** was broken, the named check that died recorded with its real output,
  the change reverted, and the pass re-confirmed (§9).
- Disputed feasibility claims carry prototype or source evidence, or are
  marked **unevidenced** and excluded from load-bearing use in the matrix
  (§7). Two runnable prototypes were built for this document (§1.3).

---

## 1. Evidence base

### 1.1 Predecessor findings re-verified on this tree

| Finding | Re-verification |
|---|---|
| `R: Ruleset` reaches four first-party crates and stops at `AdjudicationExecutor::register`'s boxed closure (A1 §3–4) | `rg 'R: Ruleset'` on this tree hits only `orrery_core`, `orrery_witness`, `orrery_games`, `orrery_persistd` (+ facade pass-through `crates/orrery/src/lib.rs:467`); erasure at `orrery_persistd/src/adjudication.rs:350` |
| Only the `core ↔ games` edge is structurally locked; every other kernel→game arrow is convention (A2 §3.2, M-A/M-A′) | Relied on as A2 proved it; not re-mutated. A2's own caveat stands: dev-dep cycles are legal, so the lock covers ordinary dependencies only |
| `classify_component`: zero call sites; `PublishFrame`/`PublishClaim`: produced harness-side only | `rg classify_component`: definition + three impls, nothing else. `PublishClaim` written at `gates/p1-swarm/src/bot.rs:1103-1108`, `PublishFrame` at `:1129-1137` (the task text's `:1104`/`:1132` are within those spans) |
| D21 freezes `orrery_persistd`'s exports, not the trait; D38 (c) pins that a *required* `Ruleset` method names D21 | `docs/adr/0021-ruleset-distribution.md:61-77` (frozen table); `docs/adr/0038-at-rest-schema-versioning.md:161-169` |
| `scripts/core-gates.sh` fails any gated crate with Bevy in its `cargo tree`, and bans `view.neighbor(` in rules crates | Read in full; clause 1 at `:67-78`, neighbour clause at `:126-139`. Liveness re-proven first-hand, mutation F-1 (§9) |
| The rollback *unit* is reserved to A7; the integration-module construct to A3/A8 | A2 §7.1, §5.3. This document honours both: §8 |

### 1.2 New facts this document adds (each verified here)

- **E-1 — Every running host already keeps canonical state in a dedicated
  store outside the app world, and mirrors it.** `Executor<R>` holds state in
  a `BTreeMap<PersistId, R::CoreState>` (`crates/orrery_core/src/executor.rs:51`,
  with the VC-4 comment at `:60`). The regolith client wraps an
  `Executor<Regolith>` in a `LocalSession`/`CampaignRuntime` resource
  (`clients/regolith/src/lib.rs:71-73`, `campaign.rs:260`); each p1-swarm bot
  "advance[s] the core by one tick and mirror[s] the result into the ECS"
  (`gates/p1-swarm/src/bot.rs:669`, mirror write at `:738-741`); the witness
  holds one `Executor` per watched entity inside a `Witness<R>` stored as a
  Bevy `Resource` (`orrery_witness/src/plugin.rs:384`, `witness.rs:397`). No
  canonical rules state lives in ECS components anywhere in the tree.
- **E-2 — Canonical state already crosses processes as engine-neutral
  canonical bytes.** The campaign client decodes replicated
  `RegolithState` from wire bytes and inserts it into its local executor
  (`clients/regolith/src/campaign.rs:696-700`); the harness snapshots claims
  as `CoreCodec::to_canonical` bytes (`bot.rs:1107`). The wire never carries
  an engine type (A1 §9 row 9).
- **E-3 — lightyear 0.29's per-entity authority does not function, by its own
  documentation.** "Authority is currently not working since replicon only
  supports server to client replication" —
  `lightyear_replication-0.29.0/src/lib.rs:67-68`, read in the pinned
  registry source. `orrery_predict/src/wiring.rs:36-46` records the same and
  drew the plan-B seam accordingly: lightyear supplies prediction *mechanics*
  only; authority is `orrery_authority`'s in full.
- **E-4 — lightyear and replicon are configured as plugins over the
  application world** (`wiring.rs:8-14`: `ClientPlugins { tick_duration }`
  sets `Time<Fixed>`; replicon is likewise a plugin stack, vendored at
  `vendor/bevy_replicon`) — **and neither ever touches canonical rules
  state**: they replicate mirror components and wire bytes (E-1, E-2). What
  they predict and interpolate is the presentation/replication surface.
- **E-5 — the backend links zero Bevy.** `orrery_persistd` takes the witness
  engine with `default-features = false` (`crates/orrery_persistd/Cargo.toml:36`);
  cell actors store components as opaque postcard bytes
  (`orrery_persistd/src/actor.rs:117-121` per A1 §5.4, re-opened).
- **E-6 — canonical hashing never iterates a container.** `state_hash` is
  blake3 over one entity's canonical encoding (`ruleset.rs:324-330`), computed
  per entity inside `step_entity` after quantization
  (`executor.rs:126-128`). Archetype order, insertion order and allocation
  order cannot reach it because no multi-entity collection is hashed.
- **E-7 — no canonical rule performs a multi-component or multi-entity
  query.** `CoreState` is one enum per entity
  (`orrery_games/src/regolith/state.rs:217-225`); `step` sees own state,
  ordered inputs and RNG (`ruleset.rs:257-262`); live neighbour reads are
  gate-banned in rules crates (`core-gates.sh:126-139`). The ECS features a
  canonical world would buy — archetype queries, parallel system scheduling,
  structural flexibility — currently have **zero consumers** in canonical
  rules.
- **E-8 — the adjudicator installs exactly one entity.**
  `ReplayHarness::load_claimed_snapshot` (`orrery_core/src/replay.rs:106-130`,
  per A1 §5.2 re-opened); the witness holds one executor per watched entity
  for the same reason (`witness.rs:406-421`). Per-entity isolated replay is
  the fixed point every variant must reproduce.
- **E-9 — `Time<Fixed>`-anchored SubApps exist but live in `bevy_app`, not
  `bevy_ecs`.** `SubApp` with an extract function:
  `bevy_app-0.19.1/src/sub_app.rs:65` (`extract` at `:80`, `set_extract`
  example at `:48-55`). Choosing SubApp for a dedicated world therefore drags
  `bevy_app` into the canonical graph; a manually owned `World` needs only
  `bevy_ecs`.
- **E-10 — the P4 digest covers `orrery_witness`, `orrery_core`,
  `orrery_games`, `gates/p1-swarm`** (`scripts/p4-ledger.sh:32-35`) — a
  temporal constraint on when implementation lands, not on what is chosen.

### 1.3 Prototype evidence (built for this document)

Prototype `ecs-proto`, `bevy_ecs = "=0.19.1"` (the workspace's locked
version, `Cargo.lock`), release profile, run on this machine. Source is
reproduced in §7 (C-4, C-5) so the result is auditable without the
scratch tree.

**P-1 — mirror-copy cost between a dedicated canonical `World` and a
presentation `World`.** N entities with `(PersistId, QPos([i64;3]),
QVel([i32;3]))`; per tick, iterate the canonical world and write every `QPos`
into the mapped presentation entity through a `BTreeMap<u64, Entity>`:

| N (full set, not AOI-limited) | copy cost / tick | share of the 16.67 ms tick |
|---|---|---|
| 1 000 | 10.6 µs | 0.06 % |
| 10 000 | 250 µs | 1.5 % |
| 100 000 | 2.88 ms | 17 % |

Plain math: the interest set is deliberately bounded
(`orrery_spatial/src/interest.rs:1`, `:32` — "the bounded high-rate interest
set"), so a mirror is O(|AOI|), not O(|world|); even the *unbounded* 10 k
figure costs 1.5 % of a tick. "Two-world overhead" is a measured
non-blocker at any plausible near-term scale, and only approaches
significance at 100 k mirrored entities — a scale at which no in-tree
evidence exists for any part of the system.

**P-2 — `bevy_ecs` query iteration order is allocation- and
archetype-dependent.** Spawning ids 0..8 (evens into a two-component
archetype, odds into another) yields iteration `[0,2,4,6,1,3,5,7]`; spawning
the same set in reverse yields `[7,5,3,1,6,4,2,0]`. Same id set, different
observable order. Consequence: any canonical projection over an ECS world
**must sort by stable id before hashing or emitting**; the current design is
immune because nothing hashes a container (E-6).

---

## 2. The finding that reframes the comparison

The brief frames "dedicated canonical world versus shared application world"
as a future choice. The tree says otherwise: **the dedicated-store topology
already ships**. Every host today — regolith client, campaign runtime,
p1-swarm bots, the witness — keeps canonical state in an `Executor` outside
the app world and mirrors what presentation needs (E-1); canonical state
crosses the wire as engine-neutral bytes (E-2); the backend runs the same
store with no engine at all (E-5). The `BTreeMap` inside `Executor` *is* the
dedicated canonical world, minus the word "world".

Three consequences flow from this and shape everything below:

1. **The shared-world variant is the only one that changes the topology, and
   it changes it in the direction every existing invariant points away
   from.** It is not the cheap default; it is the expensive rewrite.
2. **The dedicated-`bevy_ecs` variant is a substrate swap, not a topology
   change**: replace the `BTreeMap` with a `World` holding the same
   per-entity state. Its costs and benefits must be judged as a storage
   decision under an unchanged per-entity execution contract (E-7, E-8) —
   and under that contract a canonical ECS world is structurally a fancier
   `BTreeMap`: the adjudicator and witness still need single-entity pure
   steps, so world-level machinery (archetype queries, parallel schedules)
   stays unused or becomes a hazard.
3. **The brief's central fear — generic infection — is measured absent**
   (A1 §4.4, re-verified §1.1), so Variant 1 starts from a stronger position
   than the brief assumes, and the matrix reflects that.

---

## 3. The variants

Each variant answers the six required axes: Lightyear/Replicon ·
headless field host · services · witness replay · Unreal sidecar and
embedding · copying and mirroring cost.

### V1 — Current `Ruleset`, improved

Keep the trait, the executor, and the dedicated-store topology. Address the
monolith fear with composition *behind* the existing contract (brief phase
2): a composition root (`GameDefinition`-shaped, exact construct per A8)
assembles per-domain rule modules that each own a section of `CoreState` and
a slice of the input/event vocabulary; the assembled game still presents one
`Ruleset` to the executor, the witness, and persistd. D38 (c) already
demonstrates the registration idiom (boxed factories at composition time,
`adjudication.rs:350-360`).

- **Lightyear/Replicon:** unchanged — they never touch canonical state
  (E-4); prediction stays behind the plan-B seam (`predict/lib.rs:3-8`).
- **Headless field host:** unchanged; the future field host links
  `orrery_core` exactly as p1-swarm bots do today (A1 §5.5).
- **Services:** unchanged; persistd keeps zero Bevy (E-5); coordinator/
  identity/seed never touch rules at all (A1 §3.7).
- **Witness replay:** unchanged, byte-for-byte — the strongest possible
  score, since the replay path is the system's crown jewels (E-8).
- **Unreal sidecar/embedding:** possible but ad hoc: canonical bytes are
  already engine-neutral (E-2), but with no host seam a sidecar re-embeds
  `Executor` plus a hand-rolled driver, the way each of today's three hosts
  hand-rolls its own tick loop (A2 §7.5).
- **Copy/mirror cost:** status quo — already paid, already measured by the
  P1/P4 gates; P-1 bounds it.
- **Weakness:** composition inside one trait impl is convention; module
  boundaries are not manifest-visible or compatibility-hashed (A8's gap),
  and host-driver divergence (three tick loops today) persists.

### V2 — Shared Bevy application world

Canonical components live in the same `World` as presentation and
replication state; schedules and a policy registry (unbuilt) define the
boundary.

- **Lightyear/Replicon:** nominally the best fit — canonical components
  would sit where the plugins already operate (E-4). The advantage is
  smaller than it looks: the thing Orrery most needs from lightyear at the
  canonical level — per-entity authority — **does not function** in 0.29
  (E-3), so a shared world buys integration with machinery that cannot
  carry the authority model anyway.
- **Headless field host:** every host must link `bevy_app`+scheduling to run
  rules at all. The Bevy-free property of the core is definitionally gone.
- **Services:** persistd must either link Bevy to re-execute (contradicting
  E-5 and the D21-frozen composition surface's current shape) or a second,
  non-ECS execution path must be maintained for adjudication — at which
  point the shared world has *two* rule hosts and the variant defeats
  itself: the adjudicating path, not the app world, is canonical.
- **Witness replay:** the witness re-executes one entity per watched log
  (E-8). Under a shared world it needs a per-entity headless world spun up
  per watched entity, inside a client that is itself running a full app
  world — plus proof that no presentation component leaked into the hash.
  P-2's ordering hazard applies to any projection that iterates.
- **Unreal sidecar/embedding:** worst case: canonical truth lives inside a
  Bevy application; an Unreal embedding either hosts a Bevy app or the
  variant is abandoned at the boundary.
- **Copy/mirror cost:** the one axis it wins — zero mirroring (scored
  honestly in the matrix).
- **Gate:** `core-gates.sh` clause 1 (proven live, F-1) cannot be preserved
  or equivalently replaced: the property it enforces — no Bevy in the gated
  dependency graph — is the property this variant deletes. Any replacement
  (API-subset lints, schedule discipline) is weaker **in kind**, and the
  epic's standing constraint says a weaker replacement that passes is worse
  than the current gate.

### V3 — Dedicated `bevy_ecs::World`

Keep the dedicated topology; swap the store: canonical state becomes
components in an Orrery-owned `World` behind the host, keyed by `PersistId`,
stepped by a constrained schedule. Mechanism: manually owned `World`, not
`SubApp` — SubApp drags `bevy_app` into the canonical graph and couples the
canonical tick to the app runner (E-9).

- **Lightyear/Replicon:** unchanged from today (E-4): they keep replicating
  mirror components; the canonical world stays invisible to them. The
  brief's worry "integration complications if they expect direct access to
  presentation entities" is moot — they *have* direct access to
  presentation entities, which is all they ever had.
- **Headless field host:** works; `bevy_ecs` runs standalone (P-1 built
  exactly that, twice per run). But the gated crates' graph now contains
  `bevy_ecs`, so clause 1 must become an allow-list ("`bevy_ecs` only, no
  `bevy_app`/`bevy_time`/reflect-by-default") — weaker in kind than the
  current total ban, and the new hazard classes P-2 demonstrates
  (iteration order; plus deferred commands, archetype moves) need gates
  that do not exist. Designing them is A4's node; scoring here assumes
  A4 succeeds *partially*, because a lint cannot ban "observable iteration
  order" the way `grep` bans `HashMap`.
- **Services:** persistd links `bevy_ecs` transitively through core, ending
  E-5's zero-Bevy property for the backend, or core is split into
  bevy-free contract + ECS host crates (extra seam, extra migration).
- **Witness replay:** the contract (E-8) forces per-entity re-execution
  regardless of store. Either the witness spins one micro-`World` per
  watched entity (heavier than today's one `Executor` each), or the ECS
  host exposes a single-entity step API — which is the current `Executor`
  contract re-implemented on ECS storage. This is the "fancier `BTreeMap`"
  point of §2 made concrete: the store changes; the shape cannot.
- **Unreal sidecar/embedding:** equivalent to V1/V5 — the sidecar links the
  host; `bevy_ecs` stays behind it; no ECS type crosses the boundary
  (brief's own requirement).
- **Copy/mirror cost:** measured (P-1): bounded, cheap at plausible scale.
  Not a rejection ground, and this document explicitly declines to use
  "two-world copying is slow" as an argument — the evidence says it is not.
- **Weakness:** pays real, immediate costs — gate weakening, Bevy version
  coupling inside the most-frozen part of the system (D14 pins; the tree
  already carries a forked replicon for one upstream gap,
  root `Cargo.toml:84-88`), P4-digest churn (E-10), new determinism
  surface — to buy capabilities with zero current consumers (E-7).

### V4 — Bespoke engine-neutral core

Grow the existing engine-neutral core into the full module host: the
`Executor`/`Ruleset` contract *is* the bespoke core, so this variant means
building module registries, a scheduler, and policy storage on normal Rust
structures, never adopting `bevy_ecs`.

- **Lightyear/Replicon / field host / services / witness replay / Unreal /
  copy cost:** identical to V1 on all six axes — the topology and contract
  are unchanged; only the amount of bespoke host machinery grows.
- The brief's costed fear — "rebuilding ECS scheduling, queries, storage,
  and tooling" — largely fails to materialize because canonical rules
  consume none of those (E-7). What *would* be rebuilt if module count
  grows: deterministic stage ordering, per-component policy storage,
  registries — genuine work, but work every variant needs above the ECS
  anyway (the brief itself: ECS does not deliver determinism, persistence,
  rollback, versioning, authority).
- **Weakness:** as a *terminal commitment* ("never ECS") it forecloses the
  substrate question permanently on today's single-game evidence, which is
  as unjustified in that direction as V3's immediate adoption is in the
  other.

### V5 — Hybrid: composition root + `SimulationHost` seam over the existing core; ECS admitted only behind the seam, on named triggers

The recommended shape. Three commitments:

1. **V1's composition programme**: composition root and per-domain modules
   behind the existing `Ruleset` contract; no storage change; manifests and
   the module construct per A8.
2. **A `SimulationHost` seam** (brief phase 3): one kernel-owned driver
   owning tick advance, stable-id lookup, command-in/event-out, and output
   collection — the thing all three of today's hosts hand-roll (A2 §7.5:
   client `lib.rs:71-91`, bots `bot.rs:669+`, lightyear's bridged clock
   `predict/tick.rs`). The Bevy client, the future field host, and an
   Unreal sidecar drive the same host API. The host's storage is an
   implementation detail behind the seam.
3. **ECS adoption is neither performed nor foreclosed.** The dedicated
   `bevy_ecs::World` (V3) becomes a legal *future* host implementation, to
   be adopted only if a named trigger fires and only behind the seam.
   Trigger candidates (owner may amend): (T1) a shipped module genuinely
   needs per-component canonical storage with independent A5 policies —
   i.e. `CoreState`-as-one-enum measurably stops scaling; (T2) measured
   tick cost in a real host shows the `BTreeMap` store dominating; (T3)
   A4 lands an iteration-order/structural-change gate at least as strong
   as today's clause 1 replacement requires. Until a trigger fires, the
   gated crates stay Bevy-free and clause 1 stays exactly as it is —
   **nothing replaces the gate because nothing weakens it**.

Differences from the brief's own hybrid (Variant 4 there: deterministic
transaction core outside ECS + world state in `bevy_ecs` now): the brief's
version *starts* with two competing state models and pays V3's gate and
churn costs on day one. This hybrid keeps one state model until evidence
demands two. That is the "clean separation vs merely two competing models"
test the brief asks applied: two models without a consumer for the second is
the failure case.

Axes: identical to V1 today on all six, with two upgrades — Unreal
sidecar/embedding gains a real attach point (the host seam is the C-ABI/IPC
surface; canonical bytes already cross processes, E-2), and host-driver
divergence ends (one driver instead of three).

Cost stated honestly: the seam itself is churn — it touches `orrery_core`
adjacency and hosts, which intersects the P4 digest (E-10), so the
implementation lands after P4 exits like everything else in this tree; and a
seam with one implementation risks being speculative structure. The
mitigation is that the seam is *also* the phase-3 step of every other
forward path, so it is not stranded investment under any later decision.

---

## 4. The weights, justified before any scoring

Stated before scores were assigned, per the acceptance rule: unjustified
weights are where a predetermined conclusion hides. Weight scale 1–5.

| # | Criterion | W | Why this weight, from evidence |
|---|---|---|---|
| K1 | Determinism & adjudication fit | **5** | Verifiability is the product, not a feature: D9/VC-1..8, the four-platform corpus (`ci.yml:673-735` per A1 §7.3), and the entire strike economy assume "re-run it somewhere else and get the same answer" (`ruleset.rs:3-6`). A variant that degrades this degrades everything downstream at once. |
| K2 | Witness replay & canonical projection fit | **5** | The replay unit is structural: one entity, isolated (E-8). A variant that cannot reproduce per-entity pure replay makes every verdict unproducible — not slower, impossible. Weighted equal to K1 because it is K1's enforcement path. |
| K3 | Headless field host & services fit | **4** | The backend's zero-Bevy build is a working, shipped property (E-5), D21-frozen at its composition surface. Slightly below K1/K2 only because a split-crate workaround exists in principle (V3 note); losing it is expensive, not impossible. |
| K4 | Migration risk & gate preservation | **4** | Goldens cover state hashes only (epic constraint; `ruleset.rs:280-284` states the trap), so behaviour moved without fixtures is invisibly droppable; the P4 digest (E-10) taxes churn in exactly the crates every variant touches; and the standing rule that a weaker gate replacement is worse than the current gate binds here. Proven live: F-1, F-2 (§9). |
| K5 | Modularity / composition-root fit | **3** | The brief's motivating pain — but the tree's evidence for it is thin: one production game, generic spread measured absent (§1.1), no in-tree incident of cross-module failure (A2 §5.2). Real at target scale, speculative today; weighted mid, not high, because weights follow evidence, not fear. |
| K6 | Lightyear/Replicon integration | **3** | Coupling is measured narrow (plan-B seam, `predict/lib.rs:3-8`) and the stack's own authority machinery is non-functional (E-3), so no variant can gain much here; canonical state touches neither stack today (E-4). Weight reflects bounded upside and bounded downside. |
| K7 | Unreal sidecar & embedding | **3** | Owner-stated requirement with zero in-tree code (A1 §9 row 10: unverifiable as current state). It must be preserved as a possibility (both sidecar and embedded per the brief), but a high weight would let an unevidenced future dominate evidenced present costs. |
| K8 | Copy/mirror & runtime performance | **2** | Measured cheap (P-1: 1.5 % of tick at 10 k full-copy; AOI-bounded in practice) and no recorded in-tree performance pain. Low weight is itself an evidence-driven choice: this axis is where assertion most outruns measurement in ECS debates. |
| K9 | Bevy upgrade churn / maintenance | **2** | Real — D14 pins, one vendored fork already exists (`Cargo.toml:84-88`) — but bounded by seams, and historical evidence of harm is one fork, not a pattern. |
| K10 | Testability & contributor ergonomics | **2** | Behaviour is thoroughly pinned (73 core lib tests, goldens, conformance corpus, 25 witness detection tests — A1 §7), which compresses the differences between variants: all are testable; some need more new fixtures than others. |

Total weight 33; maximum score 165.

## 5. The decision matrix

Scores 1–5 per criterion; rationale for every non-obvious cell is in §3's
axis text. Unevidenced claims (§7) were **not** allowed to move scores: V3
is not penalized for "copying is slow" (measured false, P-1) and V2 is
credited for zero mirroring.

| Criterion (weight) | V1 Ruleset improved | V2 Shared world | V3 Dedicated ECS world | V4 Bespoke terminal | V5 Hybrid |
|---|---|---|---|---|---|
| K1 Determinism & adjudication (5) | 5 | 1 | 3 | 4 | 5 |
| K2 Witness replay & projection (5) | 5 | 1 | 3 | 4 | 5 |
| K3 Field host & services (4) | 5 | 1 | 3 | 5 | 5 |
| K4 Migration risk & gates (4) | 5 | 1 | 2 | 3 | 4 |
| K5 Modularity (3) | 3 | 4 | 4 | 4 | 4 |
| K6 Lightyear/Replicon (3) | 5 | 3 | 4 | 4 | 5 |
| K7 Unreal (3) | 3 | 1 | 4 | 4 | 4 |
| K8 Copy/perf (2) | 4 | 5 | 4 | 4 | 4 |
| K9 Churn (2) | 4 | 2 | 2 | 3 | 4 |
| K10 Testability (2) | 4 | 3 | 3 | 4 | 4 |
| **Weighted total (max 165)** | **146** | **62** | **104** | **131** | **150** |

Cell notes the table cannot carry: V2/K1–K3 score 1, not 0, because a
determined design could partially fence contamination with schedules and
registries — but every fence is convention where today's is a gate. V3/K1–K2
score 3, not 1, because per-entity purity *can* be preserved on ECS storage
(sorted projection, single-entity step API) — the deductions are the unbuilt
gates (P-2's hazard class) and the witness's per-watched-entity machinery.
V4/K1–K2 score 4: same contract as V1 but more newly written host machinery
in witnessed paths. V1/K5 scores 3: composition is achievable but stays
convention without manifests. V5/K4 scores 4, not 5: the seam is real churn
in P4-digest territory.

**Arithmetic honesty:** V5 (150) beats V1 (146) by less than the resolution
of this method. The matrix's robust findings are the *ordering of the rest*:
V2 is rejected by an unbridgeable margin; V3 sits ~45 points back; V4
forecloses for no gain over V5. The V5-over-V1 call is therefore argued, not
computed: the host seam is the piece V1 lacks, it is required by the Unreal
requirement and the three-driver divergence regardless of storage decisions,
and it is the phase-3 step of every path the brief contemplates — so its
cost is not stranded under any future. If the owner rejects the seam, V1 is
the correct fallback and nothing else in this document changes.

**Sensitivity.** Doubling both speculative-future weights (K5→6, K7→6) —
the reweighting most favourable to ECS adoption — gives V1 162, V3 128,
V5 174, V2 77: V5 still first, V2 still last. For V3 to overtake V5, K1+K2
would need to fall from 10 to below ~3 combined — i.e. the owner would have
to decide verifiability is not the product. The recommendation is robust to
any reweighting short of that.

---

## 6. Recommendation

**Adopt V5: keep the engine-neutral `Ruleset`/`Executor` contract and the
dedicated-store topology; add a composition root with per-domain rule
modules behind the existing trait; add a kernel-owned `SimulationHost` seam
that all hosts (Bevy client, field host, future Unreal sidecar) drive; admit
a dedicated `bevy_ecs::World` only behind that seam and only on the named
triggers T1–T3 (§3 V5). Answer the brief's central question — dedicated
versus shared — as: dedicated, permanently; it is what already ships.**

What was rejected, and why:

- **V2 (shared Bevy application world) — rejected outright, not deferred.**
  It is the only variant that changes the shipped topology, and it spends
  the system's three load-bearing properties — gate-enforced Bevy-free
  rules (F-1), per-entity isolated replay (E-8), a zero-Bevy backend (E-5)
  — to buy integration with an authority mechanism that does not work
  (E-3) and to save a mirroring cost that measures at 1.5 % of a tick
  (P-1). Its gate replacement is weaker in kind, which the epic's standing
  constraint scores as worse than the gate it replaces. 62/165; last on
  every reweighting tried.
- **V3 (dedicated `bevy_ecs::World`, adopted now) — rejected as a
  present-tense decision; preserved as V5's triggered future.** Every
  immediate cost is real — allow-list gate weaker than clause 1, new
  determinism hazard classes (P-2), Bevy version coupling in the frozen
  zone, P4 churn — and every immediate benefit lacks a consumer (E-7: no
  canonical rule queries components; E-8: the replay contract reduces the
  world to a per-entity store anyway). Adopting it today is paying for
  capabilities nothing uses with properties everything uses.
- **V4 (bespoke, terminal) — rejected for its foreclosure, not its
  architecture.** Its architecture *is* today's, and today's is good; but
  committing never to adopt ECS, on one production game's evidence, is the
  same over-decision as V3 in the opposite direction. V5 keeps V4's
  substance and discards its finality.
- **V1 (improve in place) — beaten narrowly, kept as the named fallback.**
  Everything in V5 except the host seam. If the seam is judged speculative
  structure, V1 is correct and loses only the Unreal attach point and
  driver convergence.

What this recommendation does **not** decide (owned elsewhere): the rollback
unit (A7); the per-component policy shape and `classify_component`'s
successor (A5); module manifest and construct (A8); the deterministic
schedule and any future gate design for an ECS host (A4); Bevy/Unreal
boundary details (A9); all ADR acceptance (owner, per #395).

---

## 7. Disputed-claims register

Every feasibility claim this comparison leaned on, with its evidence status.
Claims marked **unevidenced** were kept out of the matrix arithmetic.

| # | Claim | Status | Evidence |
|---|---|---|---|
| C-1 | "A generic `R: Ruleset` propagates through the crate graph" (brief's fear) | **Refuted for the current tree** | A1 §3 re-verified §1.1: four crates + facade; erased at one boxed closure |
| C-2 | "Lightyear/Replicon integration complicates a dedicated canonical world" | **Evidenced moot** | They are app-world plugin stacks (E-4) that never touch canonical state; canonical bytes bypass them today (E-2) |
| C-3 | "Lightyear provides per-entity authority a shared world could use" | **Refuted from source** | `lightyear_replication-0.29.0/src/lib.rs:67-68`; mirrored by `wiring.rs:36-46` |
| C-4 | "Two-world state copying is prohibitively slow" | **Refuted by prototype** | P-1: 10.6 µs / 250 µs / 2.88 ms per tick at 1 k/10 k/100 k full-set mirror, `bevy_ecs 0.19.1` release build; AOI-bounded in practice |
| C-5 | "`bevy_ecs` iteration order is safe to hash" | **Refuted by prototype** | P-2: same id set, spawn-order-dependent iteration. Any ECS projection must sort by stable id; current hashing is immune (E-6) |
| C-6 | "Raw `World` cloning/serialization is unacceptable for rollback" (brief) | **Unevidenced — and moot here** | No world-clone rollback exists (rollback is per-entity log replay + correction, A2 row 7); unit reserved to A7. Not scored |
| C-7 | "Monomorphization / compile-time blowup from `R:` generics" | **Unevidenced** | No in-tree benchmark; one production game (A1 §11.2). Not scored |
| C-8 | "`SubApp` is the right dedicated-world mechanism" | **Evidenced against** | E-9: SubApp lives in `bevy_app` with an app-driven extract; a manual `World` keeps the graph `bevy_ecs`-only. V3/V5-trigger text uses manual `World` |
| C-9 | "The witness projection survives archetype/insertion/allocation order" | **Evidenced structurally** for the current design | E-6: no container is hashed. For any ECS host this becomes a proof obligation (A4/A7), per P-2 |
| C-10 | "The Bevy-free gate is live and strong" | **Evidenced by mutation** | F-1 (§9): dev-dep alone kills clause 1 by name |
| C-11 | "Per-entity replay semantics are pinned by tests, so re-hosting is mechanically checkable" | **Evidenced by mutation** | F-2 (§9): breaking the neighbour-snapshot stage kills `a_step_never_sees_itself_as_a_neighbour` by name |
| C-12 | "An Unreal sidecar/embedding can consume the current outputs" | **Partially evidenced** | Canonical bytes already cross processes engine-neutrally (E-2) and the backend proves engine-free linking (E-5); but no Unreal/C-ABI code exists (A1 §9 row 10), so the axis is scored on preserved possibility, never on demonstrated integration |

Prototype source (both parts), for auditability — `bevy_ecs = "=0.19.1"`,
`default-features = false`, release profile:

```rust
// P-1: two Worlds; canonical (PersistId, QPos, QVel); presentation mirror
// keyed via BTreeMap<u64, Entity>. Per tick: iterate canonical, write QPos
// into the mapped presentation entity; 100 reps timed after 3 warm-ups.
// P-2: spawn ids 0..8 alternating between two archetypes, forward vs
// reversed order; collect query iteration order of &PersistId; compare.
```

(Full listing lived in the session scratchpad; the numbers in §1.3 are the
program's verbatim output on this machine. Re-running requires only the two
structs above and the loop described.)

---

## 8. Deferred to their owners

Stated so this document does not decide in passing what other nodes own:

1. **Rollback unit** — A7 (#403). This comparison used only the *current*
   mechanism (per-entity log replay + correction inbox) as fact; where a
   variant's rollback story depends on the unit (e.g. world-clone under V3),
   the dependency is named, not resolved (C-6).
2. **Gate replacement design for any ECS host** — A4 (#400). §3 V3 records
   what the replacement must cover (allow-list + iteration-order/structural
   hazards) and the standing rule it must satisfy; T3 makes A4's success a
   precondition of ECS adoption rather than an afterthought.
3. **Per-component policy shape** (and whether `CoreClass` survives) — A5
   (#401). V5's trigger T1 references it without presuming its answer.
4. **Module construct and manifests** — A8 (#404). The composition root's
   exact form (trait / builder / macro) is A8's, per the brief's own
   instruction not to copy its illustrative API.
5. **Bevy and Unreal boundary details** — A9 (#405), building on the host
   seam if accepted.

---

## 9. Mutation log (break stage → named check dies → revert → passes)

Both enforcement claims this document itself leans on were proven live on
this tree, first-hand. Baselines were recorded before each mutation;
failing runs produced real result lines; no mutation landed on both sides
of an equality; both reverts re-ran the check and passed.

| # | Guarded stage broken | Named check that died | Observed failure | Reverted result |
|---|---|---|---|---|
| F-1 | Appended `[dev-dependencies.bevy_ecs] version = "0.19"` to `crates/orrery_core/Cargo.toml` | `scripts/core-gates.sh` clause 1 | `core-gates: orrery_core has Bevy in its dependency graph`, exit 1 | Baseline and revert: all four clause notes print, `verifiable-core static gates pass`, exit 0 |
| F-2 | In `Executor::step_entity`, replaced `self.states.remove(&entity)?` with `self.states.get(&entity)?.clone()` — the stepping entity remains visible in its own neighbour map (the snapshot-isolation stage itself, not any check line) | `cargo test -p orrery_core --lib` | `executor::tests::a_step_never_sees_itself_as_a_neighbour` panicked `reached itself` at `executor.rs:596`; `72 passed; 1 failed` | `73 passed; 0 failed` |

A1's M1–M8 and A2's M-A/M-B are relied on as recorded there (their targets
were re-read, not re-run, except where F-1 independently reproduces A1 M1's
result). A2's discarded false start (M-A′, dev-cycle legality) is honoured:
F-1 deliberately used a dev-dependency to show clause 1 catches even the
weakest insertion point.

---

## 10. Stale citations found while verifying

| Record | Citation | Current truth |
|---|---|---|
| Task/epic text for this node | `PublishFrame`/`PublishClaim` producers at `gates/p1-swarm/src/bot.rs:1104`, `:1132` | Drifted by a few lines: `PublishClaim` write spans `:1103-1108`, `PublishFrame` `:1129-1137`. Claims themselves correct |
| `docs/10-crates.md:94` (layering rule 2) and `:120` | Both name `orrery_field_host` as a crate | No such crate exists (A1 §5.5; already filed as #414 per A2 §9 for `:120` — rule 2's mention at `:94` is the same drift one section earlier) |
| Brief §Reference material | cites `docs/adr/0004-bevy-netcode-stack.md` | File exists; not re-litigated here. The brief's `p{N}-*` path drift was already recorded by A1 §10 |
| A1/A2's previously recorded stale citations (ADR-0038 line drift, D21 `validate_intent` parenthetical, docs/06 present-tense `classify_component` consumers) | — | Re-confirmed still stale, still claim-preserving, exactly as recorded; not repeated here |

No new stale citation was found that changes any predecessor's conclusion.

---

## 11. Unsure

Stated as unsure rather than smoothed over:

1. **Whether the composition root alone relieves the monolith pressure at
   real scale.** Phase 2's own purpose is to test exactly this before any
   storage decision; V5 adopts that test rather than predicting its result.
   If composition behind one trait proves insufficient, trigger T1 is the
   honest exit, and the matrix's K5 scores would deserve re-weighing then.
2. **The witness's cost under an eventual ECS host.** §3 V3's
   "micro-`World` per watched entity or single-entity step API" analysis is
   reasoning from the contract (E-8), not from a prototype; if ECS adoption
   is ever triggered, this deserves a measured spike before commitment.
3. **How much of the host seam can be extracted from today's three drivers
   without touching P4-digest crates.** The client's driver lives outside
   the digest, but the bots' (`gates/p1-swarm`) and anything touching
   `orrery_core` are inside it (E-10); sequencing is A11's, and the seam may
   prove more invasive than §3 V5 hopes.
4. **The Unreal axis rests on zero in-tree code** (C-12). Both K7's weight
   and every variant's Unreal score would change if the owner supplied a
   concrete Unreal requirement (embedded vs sidecar first, latency budget,
   Blueprint surface); this document deliberately kept the axis's weight at
   the level its evidence supports.
5. **Whether "reject V2 permanently" overreaches.** The rejection rests on
   three currently load-bearing properties (F-1, E-5, E-8). A future in
   which the owner deliberately retires one of them — e.g. abandons
   third-party adjudication — would reopen V2's file; nothing in this tree
   suggests that future.

Deliberately not done:

- **No implementation.** The prototype lived in the session scratchpad; the
  two mutations lived for one command run each and were reverted with
  passing results re-confirmed (§9). The only file this branch adds is this
  document.
- **No ADR text.** Proposals for ADR changes belong to A11 (#407); this
  document supplies the decision and its evidence, not the record.
