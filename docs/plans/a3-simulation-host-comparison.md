# A3 — Compare simulation-host and world models (#399)

**Status:** decision node for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/399-a3` at `ce5e34a7` · **Parents:**
[#399](https://github.com/baadc0de/orrery/issues/399) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md),
[A2](a2-kernel-game-module-ownership.md) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)

A1 mapped what exists and took no position. A2 assigned ownership of every
responsibility and took no position. **This document takes positions**, states
what they beat, and marks every input that lacks evidence.

Method, continuing A1/A2:

- Every claim cites a file and line opened on this tree. Where this document
  asserts an enforcement property, the guard was demonstrated against its
  **guarded stage** — broken stage fails, fixed stage passes, both directions
  recorded (§9). A1/A2's eight-plus-three mutations were re-based rather than
  re-run: `git diff --stat 46c9301a ce5e34a7 -- crates gates clients scripts`
  is empty, so their logs carry over at full strength.
- What **exists**, what is **designed but unwired**, and what is **speculative**
  never share a sentence. Claims with no in-tree or prototype evidence are
  marked **unevidenced** and carry reduced weight in the matrix (§5) — they do
  not get laundered into scores by confident prose.
- Feasibility claims in dispute were settled by **prototype against the pinned
  dependency** (`bevy_ecs 0.19`, matching the workspace pin,
  root `Cargo.toml:58-65`), not by assertion. The probes live outside the repo
  (scratch crate, §4); their sources and outputs are reproduced here so the
  evidence is auditable without rebuilding anything.

---

## 1. Ground truth inherited from A1/A2, re-based on this tree

These findings bear directly on the comparison. Each was re-checked where this
document leans on it; none had drifted (the code tree is identical to the one
A1/A2 verified).

| # | Finding | Evidence |
|---|---|---|
| G1 | `R: Ruleset` appears in exactly four first-party crates — core (definition), witness (engine + adapter), games (`Game` supertrait), persistd (registration edge) — plus the facade's pass-through. It stops at `AdjudicationExecutor::register`, which boxes the build into `Worker = Box<dyn Fn …>` immediately | Re-ran `rg 'R: Ruleset'` today: hits only `executor.rs`, `replay.rs`, `witness.rs`, `plugin.rs`, `adjudication.rs`, facade `lib.rs`. Box at `adjudication.rs:278-279`, `register<R>` at `:350` |
| G2 | Only the `orrery_core ↔ orrery_games` edge is structurally locked (cargo refuses the cycle). Every other kernel→game arrow is convention plus review | A2 §9 M-A (cycle refused), M-A′ (dev-dep false start discarded); A2 §3.3 finding 1 |
| G3 | `classify_component`: definition + three impls, zero call sites | Re-ran today: `ruleset.rs:298`, `conformance/ruleset.rs:242`, `skirmish/mod.rs:186`, `regolith/mod.rs:129`, nothing else |
| G4 | `PublishFrame`/`PublishClaim` have no producer in any production crate; p1-swarm writes both harness-side. No `NeighborFrame` producer exists anywhere | `bot.rs:1105` (claim), `bot.rs:1133` (frame); `NeighborFrame` constructed only as test evidence (`adjudication.rs:526`), type at `protocol/verifiable.rs:116` |
| G5 | The witness hash is blake3 over the canonical encoding of one entity's **quantized** state; the executor holds states in a `BTreeMap` keyed by `PersistId`; a step snapshots neighbours structurally by removing own state first. Archetype order, insertion order and allocation order cannot reach today's hash **because no world iteration feeds it** | `state_hash` `ruleset.rs:324-326`; `BTreeMap` `executor.rs:51`; snapshot-by-removal `executor.rs:116-118`; quantize-before-hash `executor.rs:126-127` |
| G6 | Replay installs exactly one entity — the disputed one — after verifying snapshot bytes; verdicts compare subject-signed claims against computed hashes | `replay.rs:106-130` |
| G7 | Goldens are chains over per-tick state hashes **only**; the source itself documents that an outcome leaving no trace in the emitter's state is invisible to them and to adjudication | `golden.rs:20-28` ("State hashes, and only those") |
| G8 | `scripts/core-gates.sh` enforces, per commit: Bevy-free graphs for exactly `GATED_CRATES = (orrery_core orrery_games orrery_conformance)` via `cargo tree` grep; VC-4 unordered-collection ban; VC-8 ambient-input ban; VC-6 transcendental ban; and `view.neighbor(` banned in `RULES_CRATES = (orrery_games orrery_conformance)` | `core-gates.sh:37`, `:71-75`, `:95-97`, `:103-105`, `:117-123`, `:42`, `:137-139` |
| G9 | The Bevy gate's coverage is the crate list, nothing wider. `cargo tree -p orrery_witness` contains bevy (530 references, default-on `bevy` feature) and the gate passes — bevy already sits in a first-party rules-adjacent crate without failing anything | Run today: count 530, gate exit 0. Corollary: a *new* crate hosting an ECS simulation would pass the unchanged gate too. Coverage is a decision, not a property |
| G10 | `orrery_persistd` links zero bevy (takes the witness engine `default-features = false`) | `persistd/Cargo.toml:36`; `cargo tree -p orrery_persistd \| grep -ci bevy` = 0 today |
| G11 | D21 freezes persistd's exports, not the `Ruleset` trait; D38's honesty note pins that a *required* trait method "names D21 and pays its ADR"; additive (defaulted) methods are free | `adr/0021-ruleset-distribution.md:61-77` (frozen table), `adr/0038-at-rest-schema-versioning.md:155-169` |
| G12 | `dyn Ruleset` is unnameable (associated types); composition is static visitor + boxed factories | `game.rs:171-174`; G1 |
| G13 | Lightyear integration is configuration-layer and App-centric: `orrery_predict` is the only crate naming lightyear internals, and lightyear's replication is component-centric on the Bevy application world. Lightyear supplies prediction mechanics only — per-entity authority and any rollback signal are Orrery-side | `predict/lib.rs:3-8`; `predict/src/wiring.rs:36-51` quoting `lightyear_replication-0.29.0/src/lib.rs:67` |
| G14 | There is no field host. Three hosts advance ticks themselves (swarm bot, regolith local session, lightyear session clock); the harness stands in for the missing crate | A2 §2 row 1; A1 §5.5 |
| G15 | The P4 pipeline digest banks hours over `orrery_core`, `orrery_games`, `orrery_witness`, `gates/p1-swarm` — a temporal constraint tied to #329, not an architectural one | A1 §7.3 |

One correction to the briefing text of this very task: the harness producers
are at `bot.rs:1105` and `bot.rs:1133`, not `:1104`/`:1132` — one-line drift,
finding unchanged.

---
## 2. The variants, defined precisely

Five are scored. Definitions are operational — each says what changes, what
stays, and what would have to be built that does not exist today.

**V1 — Current `Ruleset`, improved (composition without storage change).**
The trait, the associated types, the per-entity `Executor` and its BTreeMap
storage stay exactly as they are. What improves is *authoring*: a composition
root (the brief's phase 2, `ruleset-ecs-migration-brief.md:660-678`) under
which Regolith becomes an assembly of delegated rule modules behind the
existing `Ruleset` contract; module manifests as data (A8's territory);
`classify_component`'s consumers wired when A5 decides their shape. All gates
keep their current crate lists. No wire change, no persistence change, no new
dependency anywhere.

**V2 — Shared Bevy application world.** Canonical components live in the same
`World` the Bevy app runs: presentation components beside them, separated by
component-policy registries and schedule conventions (brief §Bevy integration,
Variant B). The witness projection, persistence codec and rollback unit must
select canonical state out of a mixed world by policy instead of by storage.

**V3 — Dedicated canonical `bevy_ecs::World`.** The brief's preferred
hypothesis (brief §Proposed ECS boundary): a `SimulationHost` owns its own
world and explicit deterministic stages; the Bevy engine adapter submits
commands and consumes presentation frames; the same host runs headless, in the
client process, or in a sidecar. Canonical state never shares a world with
presentation state.

**V4 — Bespoke engine-neutral core, generalized.** Keep everything
engine-free, but grow today's minimal executor into a real simulation core:
structural queries over multiple entities, richer scheduling, change tracking
— what ECS provides, rebuilt on ordinary Rust data structures.

**H1 — Hybrid: two tiers by component class (the variant this tree suggests
and the brief does not list).** `CoreClass` already declares a three-way split
— Core / Bulk / Cosmetic (`ruleset.rs:63-75`) — whose consumers were never
wired (G3). H1 takes it seriously: **Core-class state stays in the per-entity
executor** with every structural guarantee it has today (G5/G6); Bulk and
Cosmetic state — the high-volume, non-adjudicated mass — lives in an ECS world
(shared or dedicated) replicated through the existing replicon/lightyear stack.
`classify_component` becomes the routing hook between the tiers, which is
precisely the consumer docs/06 promised for it
(`docs/06-verifiable-core.md:60`). The tiers meet only at declared seams:
events downward, projections upward.

V2 vs V3 is the brief's "single most consequential" question and is decided
head-on in §6. V4 exists in the comparison partly to make visible that **the
tree already contains a small bespoke engine-neutral core** — the executor — so
V4-as-replacement must justify rebuilding ECS features precisely because they
are wanted, not because anything is missing.

---

## 3. Per-variant analysis across the six required axes

Each variant against Lightyear/Replicon integration · headless field host ·
services · witness replay · Unreal sidecar/embedding · copying/mirroring cost.
Evidence tags: **[S]** = source citation above; **[P]** = prototype evidence
(§4); **[U]** = unevidenced.

### V1 — improved status quo

- **Lightyear/Replicon:** untouched. Prediction already couples to Orrery only
  through configuration and correction queues (G13); nothing about V1 moves
  state into or out of any world. [S]
- **Headless field host:** the gap is real and V1 does not close it by itself —
  but nothing about V1 blocks it either: p1-swarm's bot *is* the sketch
  (`Executor<Regolith>` inside a Bevy App plus harness-side frame assembly,
  G4), and the SimulationHost seam recommended unconditionally in §7 extracts
  that shape from harness code into a crate. [S]
- **Services:** persistd's registration edge already erases the type (G1);
  zero-bevy backend preserved by construction (G10). [S]
- **Witness replay:** unchanged and structural — isolation by storage (G5),
  single-entity replay (G6). This is V1's core asset. [S]
- **Unreal sidecar/embedding:** the engine-neutral core links anywhere today;
  the exterior bridge demonstrates a framed boundary (A1 §9 #8). What V1 lacks
  is an output contract designed for foreign renderers — presentation frames
  remain ad hoc. [S], partially [U] (no Unreal consumer exists to test
  against).
- **Copy/mirror cost:** none today; none added. [S]

Honest weaknesses of V1: modularity rests on discipline (delegation inside one
trait impl family) rather than structure; the god-trait pressure the brief
fears is not refuted by current evidence, only unobserved (A1 §4.4); and
cross-module interactions keep landing wherever the author put them unless the
composition root makes them visible — which is exactly the phase-2 work V1
commits to.

### V2 — shared application world

- **Lightyear/Replicon:** the best of all variants — canonical components are
  directly registrable, predictable, interpolatable; no mirror hop. [S via G13;
  no prototype needed, this is lightyear's native mode]
- **Headless field host:** available in principle — a Bevy app without render
  plugins — but the host now drags the full plugin ecosystem's conventions
  with it, and "canonical" means "a subset of a mixed world", so the headless
  host must reproduce the same mixed-world policies minus the renderer. [S on
  mechanics; [U] on cost]
- **Services:** the cluster itself stays clean (nothing forces bevy into
  persistd), but gravitational pressure rises: the shared world makes it
  tempting to share component *types* upward, and D15 layering forbids that.
  [S on the rule; [U] on whether pressure materializes]
- **Witness replay:** weakest of all variants, and this is decisive. Every
  guarantee that is structural today becomes policy: the hash projection must
  exclude presentation components by registry lookup, not by construction
  (contrast G5); rollback rewinds whatever shares the world, audio and UI
  included (the brief names this risk itself,
  `ruleset-ecs-migration-brief.md:466-468`); and P2 shows iteration order over
  a mixed world reaches naive hashes. Nothing here is impossible — all of it is
  convention, review, and new gates. Per this task's own standard, a weaker
  replacement for a structural property scores as weaker. [S + P]
- **Unreal:** worst option — canonical truth lives in Bevy-world semantics, so
  a sidecar cannot host "the same world"; it would re-host a different model.
  [S by elimination]
- **Copy/mirror cost:** lowest (no copies). Its savings are real but purchasable
  elsewhere (see V3's measured mirror cost). [P]

### V3 — dedicated canonical world

- **Lightyear/Replicon:** workable with one added hop: lightyear replicates
  components registered in the app world (G13), so canonical state must be
  mirrored into AOI-scoped presentation components for replication to see it —
  or lightyear internals rewritten, which D15's plan-B seam exists to avoid.
  The hop is the same extraction V3 needs anyway for presentation. [S + P]
- **Headless field host:** the natural winner — the host is already headless;
  the client adds adapters around it. P1 demonstrates a bare
  `bevy_ecs::World` + `Schedule` running a fixed-step loop with no `App`, no
  tokio, no OS services. [P]
- **Services:** persistd can keep linking only the engine-free engine (the
  witness engine half) if adjudication continues to consume bundles rather
  than worlds — true today (G1). But note the dependency gravity honestly: any
  crate hosting the canonical world links bevy_ecs, and G9 shows the existing
  gate would not notice a new crate doing so. Keeping the backend clean becomes
  a maintained decision instead of a structural fact. [S]
- **Witness replay:** the crux. Storage isolation from presentation is gained;
  single-entity replay isolation is *lost* unless rules keep composing through
  events, because an ECS makes multi-entity queries expressible where today's
  executor structurally refuses them (G5). The compensation exists and is
  mechanical — P5 proves bevy_ecs can reject ambiguous schedules at build time,
  P2 proves a sorted-by-stable-id projection is immune to insertion order —
  but the full gate bundle (ambiguity detection, projection differential test,
  neighbour-query lint equivalent) is undesigned work owned by A4, and until it
  exists the property is conventional. [P + S; delivery [U] until A4 lands]
- **Unreal sidecar/embedding:** strongest with V4 — the host is a library with
  a command-in/frames-out contract; bevy_ecs types need never cross the ABI
  (the brief's own boundary, `ruleset-ecs-migration-brief.md:495-505`). [S +
  P1's headless demonstration]
- **Copy/mirror cost:** the feared cost measures small at current scale — P4:
  10k entities extracted per frame in ~9 µs. Marked indicative throughout:
  synthetic workload, one box, no archetype churn, not capacity evidence. [P]

### V4 — bespoke generalized core

- **Lightyear/Replicon:** unchanged from V1 — frames out, corrections in. [S]
- **Headless field host:** fine — but so is V1's; generalization buys nothing
  here that the SimulationHost seam doesn't. [S]
- **Services:** cleanest dependency story possible. [S]
- **Witness replay:** keeps the per-entity model only if the generalized core
  declines to use its own multi-entity query power in rules — i.e., V4's
  distinguishing feature is precisely the one the adjudication model cannot
  admit (G5/G6). A bespoke core strong enough to matter is strong enough to
  break isolated replay. [S]
- **Unreal:** equal-best with V3. [S]
- **Copy/mirror:** none. Compile/tooling: DIY schedules, DIY change detection,
  DIY inspection tooling — all real costs with no in-tree evidence bounding
  them ([U]); A1 §11.2 already noted no build-cost benchmark exists for even
  the current generics.

The decisive framing: **V4 is the status quo plus features nobody has asked
for yet, where the headline feature conflicts with the product's verification
model.** If multi-entity structural queries ever become genuinely needed for
*canonical* logic, that need arrives as an adjudication problem first, and H1's
tiered answer (queries allowed outside the verified tier) handles it with the
guarantee boundary drawn explicitly.

### H1 — hybrid two-tier

- **Lightyear/Replicon:** bulk/cosmetic components replicate natively through
  the existing stack (they are the volume the replication layer was built
  for); core-class state keeps its existing claim/frame path (p1-swarm's
  assembly, G4, promoted to production). Two paths instead of one — a real
  complexity cost, paid in exchange for keeping guarantees structural. [S]
- **Headless field host:** the SimulationHost seam hosts both tiers; bulk-tier
  world optional per deployment (a pure-core host skips it entirely). [S]
- **Services:** adjudication consumes evidence bundles as today; the cluster
  never needs either tier's storage. [S]
- **Witness replay:** core tier keeps G5/G6 verbatim — the hash never touches
  the ECS world, because Core-class state is *in* the executor. Bulk/cosmetic
  never enters adjudication by definition (that is what the class means,
  `ruleset.rs:293-301`: unclassified defaults to Cosmetic and is "never
  persisted"). The tier boundary does the work archetype discipline would
  otherwise do. [S]
- **Unreal:** good — presentation consumes bulk-tier frames plus core-tier
  claims; embedding ships both tiers or core-only. [S]
- **Copy/mirror:** bulk tier mirrors like V3 (measured trivial at scale, P4);
  core tier unchanged. [P]

Honest weakness of H1: two mental models coexist, which the brief warns about
("merely two competing models", `ruleset-ecs-migration-brief.md:558-560`); the
class registry becomes load-bearing infrastructure before A5 has decided the
policy model that would own it; and until `classify_component` has wired
consumers, deciding architecture around it is building on an unwired hook
(G3). H1 is scored as the best *destination candidate*, conditional on the
pilot and on A5 — not as something to start building Monday.

## 4. Prototype evidence (P1–P5)

Scratch crate at `/tmp/opencode/ecs-probe`, `bevy_ecs = { version = "0.19",
features = ["multi_threaded"] }` + `blake3`; release profile; x86_64 Linux
(this workstation). Sources condensed below; full files were disposable and no
repo code was touched. Version note: bare `bevy_ecs` default features do **not**
include `multi_threaded`
(`bevy_ecs-0.19.1/Cargo.toml` `[features] default = ["std", "bevy_reflect",
"async_executor", "backtrace"]`) — a bare-ECS host is single-threaded unless it
opts in; full `bevy` turns the feature on via `default_platform`
(`bevy_internal-0.19.1/Cargo.toml:323-329`). Both configurations were probed.

**P1/P2 — dedicated world runs headless and deterministically; iteration order
reaches naive hashes; canonical projection neutralizes it.** A `World` plus two
`Schedule`s (integrate; hash), 2000 entities with `PersistId/Pos/Vel/Hull`,
600 fixed ticks, integer integration, fresh world per run. Two insertion
orders of the same semantic population (Fisher–Yates shuffled ids):

```
P1 order-A: naive=80c75b22.. canonical=d8eb8313..
P1 order-B: naive=8916fefa.. canonical=d8eb8313..
P1 verdict: canonical AGREES | naive DIFFERS
```

Findings: (a) no `App` is needed — `Schedule::run(&mut World)` suffices,
refuting any "ECS cannot run headless without the engine" claim; (b) hashing
rows in query-iteration order produces **different hashes for identical
worlds**, confirming the brief's projection worry concretely; (c) collecting,
sorting by stable id, then encoding agrees across orders — the mitigation is
one sort. This is exactly today's witness discipline (G5: per-entity codec
hash, BTreeMap order) restated for ECS storage.

**P3 — ambiguous schedules: empirical stability proves nothing here.** Two
systems with conflicting access (`ResMut<Counter>` + `ResMut<Log>`),
non-commuting ops, 200 fresh worlds per configuration:

```
P3 ambiguous/single_threaded_executor:      distinct outcomes over 200 runs = 1   (counter=1 log="ba")
P3 ambiguous/default(multi_threaded):       distinct outcomes over 200 runs = 1   (counter=1 log="ba")
P3 chained(total-order)/multi_threaded:     distinct outcomes over 200 runs = 1   (counter=3 log="ab")
```

The honest negative result: on this box, bevy_ecs 0.19.1 executed ambiguous
systems in the same order every run under both executors. The claim "ambiguous
schedules flip nondeterministically" is therefore **unevidenced here** — and
the finding cuts the other way, harder: if repeat testing cannot distinguish
an ordered schedule from an ambiguous one, *observed stability is not
evidence of determinism*, and mechanical ordering checks are the only defensible
basis. That is precisely this repository's existing philosophy (the VC gates
fail on spelling because symptom-testing misses silent classes of failure;
`core-gates.sh:4-25`). Bevy itself treats ambiguity as unspecified order, not
as safe-by-stability.

**P5 — bevy_ecs can mechanically reject ambiguous schedules at build time.**
Same systems, `ScheduleBuildSettings { ambiguity_detection:
LogLevel::Error, .. }` (source-verified to exist and default to Ignore,
`bevy_ecs-0.19.1/src/schedule/schedule.rs:1583-1595`; enforced during
initialize, `:1253-1261`):

```
P5 ambiguous schedule: initialize = Err(Elevated(Ambiguity(AmbiguousSystemConflictsWarning(
                           ConflictingSystems([(SystemKey(2v1), SystemKey(1v1), …)])))))
P5 chained schedule:   initialize = Ok (builds)
```

Both directions demonstrated: the guard fires on the bad stage and passes the
good one. This matters because "what replaces the Bevy gate" (task constraint)
has a candidate answer that is *mechanical, in-crate, and per-commit* rather
than aspirational. Whether the bundle — ambiguity detection + projection
differential test (P2's pattern) + a neighbour-query lint equivalent — equals
today's structural isolation is A4's (#400) judgement call; this document only
establishes that the raw material exists and works.

**P4 — extraction/mirror cost, indicative only.** Copying 10k entities'
`(PersistId, Pos)` rows into a plain Vec each frame, 2000 frames after warmup:

```
P4 extraction of 10k entities x 2000 frames: 18.32ms total, 9.16 us/frame, 10000 rows/frame
```

Labelled indicative everywhere it is used: synthetic workload (no archetype
churn, no fragmentation, no change-detection filtering), one workstation,
release build. It bounds the two-world overhead question from above at current
scale — copying is microseconds, not milliseconds — but makes no claim about
capacity-scale populations or real AOI churn. Per §5's weighting rule, the
performance dimension carries low weight *because* this is its best current
evidence.

---

## 5. Weights, justified before anything is scored

Unjustified weights are where predetermined conclusions hide. Each dimension's
weight is set by three questions: how close is the dimension to the verifiable-
core value proposition (D9/D10)? How irreversible is getting it wrong? And how
good is the available evidence? Weights sum to 100; scores are integers 0–5.
A dimension whose evidence base is weak carries low weight **by rule**, so an
unevidenced fear cannot vote a variant down and an unevidenced hope cannot
vote one up.

| # | Dimension | Weight | Why this weight |
|---|---|---|---|
| 1 | Witness & adjudication fit | **20** | The strike pipeline is the product differentiator (D9/D10): shipped, nightly-measured, and the reason peers can distrust authorities. Getting it wrong is the least reversible failure on the board — wrongful strikes end sessions; missed cheats end trust. It also has the strongest evidence base (G5/G6 + A1 M6/M8). Highest weight, highest confidence |
| 2 | Determinism enforceability | **16** | The four-platform matrix and golden corpus are permanent regression harnesses; the repo's stated philosophy prefers mechanical enforcement over empirical stability (§4 P3 is fresh proof of why). Second-highest because violations surface late and far from their cause |
| 3 | Migration risk & continuity | **14** | No-flag-day is a brief principle (`ruleset-ecs-migration-brief.md:614`); P0–P4 gates are permanent harnesses; G15 banks hours over four crates until #329 exits; and G7 limits what goldens can prove about parity, raising migration verification cost. High but below 1–2 because the temporal blocker expires by construction |
| 4 | Modularity & growth headroom | **12** | The brief's actual motivation. Weighted mid-high because the epic exists to prepare for growth — but capped, because the growth itself is speculative: one production game, a catalogue of two (G12 context; A1 §4.4). A score here leans on projection more than measurement |
| 5 | Client-stack integration (lightyear/replicon/aeronet) | **10** | Real shipped stack (G13); friction converts directly into PRs and risk. Mid weight: differences between variants are one mirror hop or none, and P4 suggests the hop is cheap at scale |
| 6 | Services & backend neutrality | **8** | persistd's zero-bevy property is valuable and currently free (G10). Low-ish weight because *every* variant can preserve it — variance between variants is small, so the decision barely moves on it |
| 7 | Unreal sidecar & embedding optionality | **6** | Irreversible-if-wrong argues for weight; absence of any consumer argues against: the requirement is imported and unverifiable in-tree (A1 §9 #10). Mid-low, held at 6 rather than lower purely because retrofitting an output contract later is the expensive direction |
| 8 | Performance & copy/mirror cost | **6** | Weighted by the evidence rule: the best available number is an indicative microbench (P4). An unevidenced slowness claim must not sink V2/V3/H1, and an unevidenced speed claim must not crown them |
| 9 | Compile-time & build complexity | **4** | bevy_ecs is already in the workspace graph; kache mitigates rebuild cost; A1 found zero timing evidence either way (§11.2). Low weight, low confidence |
| 10 | Testability & contributor ergonomics | **4** | Real but soft; folded-in residual dimension with modest expected variance |

