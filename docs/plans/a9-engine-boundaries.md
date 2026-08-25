# A9 — Bevy and Unreal integration boundaries (#405)

**Parent:** #395 · **Source brief:** `docs/plans/ruleset-ecs-migration-brief.md`
(Bevy integration and "Unreal integration implications" sections; boundary
sketch at `ruleset-ecs-migration-brief.md:481-505`) · **Status:** planning
document; proposes, does not decide. ADR acceptance is the owner's alone.

**What this node establishes:** whether `bevy_ecs` genuinely stays an internal
substrate, or leaks — stated as two boundaries, one per engine, each with the
mechanism (or absence of mechanism) that enforces it.

## 0. The asymmetry, stated before anything else

The two halves of this document are not the same kind of document, and reading
them as if they were would be the most misleading outcome this node could
produce.

- **The Bevy half describes a boundary that exists.** Canonical state already
  lives outside every Bevy application world (`crates/orrery_core/src/executor.rs:48-52`),
  a named gate already fails a gated crate with Bevy in its graph
  (`scripts/core-gates.sh` clause 1, mutation-checked in §6), and the backend
  already links zero Bevy (`cargo tree -p orrery_persistd | grep -ci bevy` = 0,
  re-run for this document). Claims in §2 are cited to code and gates.
- **The Unreal half describes a boundary that does not exist. There is zero
  Unreal code in this tree.** Verified for this document:
  `grep -ril unreal` over `crates/` and `gates/` matches nothing; the only
  matches anywhere are three documentation references to Epic's Replication
  Graph *as prior art for cell-based interest management*
  (`docs/01-spatial-model.md:9,139`, `docs/03-replication.md:106`,
  `docs/references.md:103`) — citations about Fortnite's AOI pattern, not
  integration code. Both A3 lanes flagged this; the second opinion weighted
  the entire Unreal axis down to 3/10 for exactly this reason
  (`docs/plans/a3-simulation-host-second-opinion.md:391`: "Owner-stated
  requirement with zero in-tree code … a high weight would let an unevidenced
  future dominate evidenced present costs").

Everything in §4–§5 is therefore **specification against an absent
implementation, unevidenced by construction**. Each subsection there carries
the marker **[SPEC — no implementation exists]** so that no sentence of it can
be quoted without its status. The asymmetry is not a defect of this document;
it is the finding.

---

## 1. Evidence base

Every handed-down claim this document relies on was re-verified against the
tree at `origin/main` (`a82c062e`) before use. Verification method in the
right column; discrepancies found during verification are in §7.

| # | Claim | Verification |
|---|---|---|
| E-1 | Canonical state lives outside any app world: `Executor<R>` holds `states: BTreeMap<PersistId, R::CoreState>`, deliberately a `BTreeMap` because iteration order is observable (VC-4) | `crates/orrery_core/src/executor.rs:48-52`, comment at `:58-61`. Re-read for this document |
| E-2 | The backend links zero Bevy; the witness crate links a great deal of it while the gate passes | `cargo tree -p orrery_persistd \| grep -ci bevy` = **0**; same for `orrery_witness` = **530**; `./scripts/core-gates.sh` exits **0** — because `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` (`scripts/core-gates.sh:38`) and the gate's coverage *is* that list. All three re-run for this document. A4's lesson holds: never assume a gate covers what its name suggests |
| E-3 | Lightyear per-entity authority does not run: "Authority is currently not working since replicon only supports server to client replication" | `lightyear_replication-0.29.0/src/lib.rs:67-68` — re-read in the actual crate source for this document (registry copy under `~/.cargo/registry`, **not** vendored; see §7 D-1). First-party restatement with consequences at `crates/orrery_predict/src/lib.rs` doc ("What lightyear 0.29 does not provide") |
| E-4 | Lightyear types are named by exactly one crate | `crates/orrery_predict/src/lib.rs:3-8`: "This crate is **the only one whose internals name lightyear types** (docs/10-crates.md layering rule 3) — the plan-B seam". Layering rule at `docs/10-crates.md` ("lightyear types appear only inside `orrery_predict`") |
| E-5 | The replication-side id is `PersistId`, never `Entity`: the uplink component's own doc says "never a Bevy `Entity`, which is not stable across peers or restarts"; the only `Entity`-keyed structure in that file is an in-memory resource never serialized | `crates/orrery_persist_client/src/feed.rs:27-49` (`PersistId` component, `UplinkSeq.next: HashMap<Entity, u64>`); `DiffUplink` addressed by `persist_id.0` at `feed.rs:88-97` |
| E-6 | The live client topology is executor-outside-world plus mirror writes: the p1-swarm bot steps `Executor<Regolith>` (`bot.rs:170`, `:724`) and then *mirrors* position into the app world | `gates/p1-swarm/src/bot.rs:738-744` (queries `GridPosition` and assigns — the mirror write A6 §3.5 classifies as a presentation event) |
| E-7 | The fixed simulation tick is 60 Hz, set through lightyear: `ClientPlugins { tick_duration }` sets `Time<Fixed>` and `TickDuration` | `crates/orrery_predict/src/wiring.rs:10`; 100 ms interpolation buffer at `wiring.rs:13` |
| E-8 | No first-party reflection: `grep -rn "Reflect" crates/*/src` matches nothing; `bevy_reflect` is a listed-but-unused dependency of three crates | Re-run for this document: zero source matches; entries at `crates/orrery_spatial/Cargo.toml:27`, `crates/orrery_net/Cargo.toml:24`, `crates/orrery_persist_client/Cargo.toml:33` (A5 §5.1's question stands, #421-adjacent; not resolved here) |
| E-9 | The witness projection is `blake3(CoreCodec(quantize(state)))` per entity, computed inside `orrery_core`; WP-5 forbids any engine artifact reaching it; the world digest sorts by `PersistId` ascending | `docs/plans/a7-persistence-rollback-witnessing.md` §5 (WP-1..WP-6); hash-site comment at `crates/orrery_core/src/executor.rs:41-44` ("blake3 over the canonical encoding of the quantized state") |
| E-10 | Presentation events have no canonical authority, overwrite semantics, and are regenerated (never rolled back) after correction | `docs/plans/a6-commands-events-transactions.md` §3.5 table; rules R5/R6/R7 at §"R5–R7" (presentation reads only post-S4 state; never writes canonical state) |
| E-11 | IV-7 forbids any capability for a schema embedding an engine handle (`Entity`, `ComponentId`, `FnsId`, archetype/row indices); G-1 records that **no mechanical guard exists** for engine handles inside replicated payloads — "held by review alone" | `docs/plans/a5-identity-and-capabilities.md` §5.4 row IV-7, §2.2 item 4. Re-demonstrated at this node's own boundary by mutation M3 (§6): entity bits ride a `DiffUplink` payload and every named check passes |
| E-12 | A3, twice independently: the dedicated-store topology already ships and the shared application world is rejected outright | `docs/plans/a3-simulation-host-comparison.md` (V2 rejected at 268/500; "uniquely hostile to the Unreal direction"); `docs/plans/a3-simulation-host-second-opinion.md` §2 ("The `BTreeMap` inside `Executor` *is* the dedicated canonical world, minus the word 'world'") and §6 ("dedicated, permanently; it is what already ships") |
| E-13 | Sorted-by-stable-id projection agrees across entity insertion orders; raw world iteration does not; observed stability proves nothing (ambiguous schedules ran 200/200 stable in one lane) | Found independently three times (A3 P1/P2, second opinion P-2, A4 E-3); stability caveat A3 P3, restated at A7 §5.2. Relied on as recorded — the three independent reproductions are the point; not re-run |
| E-14 | Wire and id types are engine-free and fixed-width: `Tick(pub u64)`, `PersistId(pub u64)`; `cargo tree -p orrery_protocol \| grep -ci bevy` = 0 | `crates/orrery_protocol/src/persist.rs:28,46`; tree count re-run for this document |
| E-15 | No field host exists; the `SimulationHost` seam is recommendation, not code | A3 G14; `docs/10-crates.md` documents `orrery_field_host` which does not exist — already filed as #414, confirmed still open and still absent from `crates/` today |

---

## 2. The Bevy boundary

### 2.1 The boundary statement

**B-1 (proposed as normative). Canonical truth never lives in a Bevy
application world.** The canonical store is the `Executor`'s
`BTreeMap<PersistId, R::CoreState>` (E-1), owned by whatever hosts the
simulation — a p1-swarm bot today (E-6), the witness engine's replay executor
(`crates/orrery_witness/src/witness.rs:397`), persistd's
`AdjudicationExecutor`, a future `SimulationHost`. The app world holds only
*mirrors*: presentation and replication components keyed by the `PersistId`
component (E-5), written by mirror writes that A6 classifies as presentation
events (E-10). This is not a proposal to build something; it is A3's finding
(E-12) restated as the boundary rule: **the dedicated-store topology is the
boundary**, and the shared-application-world variant — the one shape under
which `bevy_ecs` would leak by construction — is rejected, twice,
independently, on scored evidence.

Consequences of shared versus mirrored, in this document's terms:

| Axis | Shared world (rejected) | Mirrored store (ships) |
|---|---|---|
| Witness projection | must *exclude* presentation by registry lookup — policy | cannot *include* it — the hash is computed in `orrery_core` from `CoreCodec` bytes of a `CoreState` defined in Bevy-free crates (E-9); presentation components are unreachable from that call site by construction |
| Rollback scope | rewinds whatever shares the world, audio and UI included (the brief's own risk, `ruleset-ecs-migration-brief.md:466-468`) | rollback touches executor state; presentation is regenerated, never rewound (E-10) |
| Unreal direction | canonical truth acquires Bevy-world semantics; a sidecar "re-hosts a different model" (A3 V2 axis) | canonical truth is engine-neutral bytes; any engine is a mirror consumer |
| Mirror cost | zero | measured small: ~9 µs per 10k entities extracted (A3 P4, marked indicative); ~1.5 % of a tick (second opinion P-1) |

### 2.2 What enforces it — and exactly how far the enforcement reaches

Stated with A4's lesson applied (E-2): the coverage of each mechanism is
listed, not assumed.

| Mechanism | What it actually covers | What it does not |
|---|---|---|
| Dependency graph: `CoreState` types live in crates that cannot name `bevy_ecs` | a `CoreState` field of type `Entity` fails to compile (mutation M2, §6: E0433) | any type in a Bevy-side crate |
| `core-gates.sh` clause 1: gated crate with Bevy in its graph dies | exactly `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` (`scripts/core-gates.sh:38`); mutation M1 (§6) confirms the death is real | `orrery_witness` (530 bevy lines, gate passes — E-2); **any new crate hosting canonical state** (A3 G9); the list is the coverage |
| Uplink seam drops engine ids: journal records addressed by `PersistId` | the *addressing* of every durable record (E-5) | the *payload bytes* — mutation M3 (§6) rode `Entity` bits into a `DiffUplink` payload and nothing died. That is A5's G-1, alive at this exact boundary; §3 is the closure proposal |
| Witness projection WP-5: no engine artifact reaches the hash | structural at the hash site (E-9): `state_hash` is computed from canonical bytes before any extraction runs; A7's battery mutation-checked the projection itself | nothing beyond the hash — a presentation bug can still *display* nonsense; it just cannot witness it |
| Review-held | everything else, explicitly: IV-7 (E-11), flush-point rules (A4 §3.4) | — |

**Answer to the node's question:** `bevy_ecs` stays an internal substrate
today, but the property is enforced *asymmetrically*: structural where it
matters most (canonical state, witness hash, backend linkage), a hardcoded
list where it is mechanical (core-gates), and review-only at the replicated
payload corridor (G-1). The honest summary is: **it does not leak today, and
one of the three fences that keep it that way is made of review.**

### 2.3 Lightyear/Replicon ownership

The division that prevents accidentally rebuilding prediction and rollback,
grounded in E-3/E-4:

- **Lightyear owns prediction mechanics, on the app world, over mirrors
  only**: the input timeline, rollback execution, `VisualCorrection`
  residuals, interpolation buffering (E-7). These operate on replicated
  *presentation* components. Orrery must not reimplement them — the seam
  exists so that lightyear can be replaced, not duplicated
  (`crates/orrery_predict/src/lib.rs:3-8`, plan-B blast radius).
- **Orrery owns everything lightyear's own documentation disclaims**:
  per-entity authority and leases (`orrery_authority` — because lightyear's
  authority "is currently not working", E-3), the rollback *signal* and its
  per-entity attribution (lightyear fires no event; the residual arrives as
  `VisualCorrection<D>`, hooked by `track_reconciliation` — predict lib doc),
  reconciliation evidence, and every canonical outcome. Authority corrections
  reach canonical state through the correction inbox drained outside the
  schedule run (A4 §3.9 via A6 R5) — never through lightyear's rollback.
- **Replicon owns component transport** for the mirror plane
  (`orrery_spatial` visibility, `orrery_persist_client` change-detection
  uplink — `docs/10-crates.md` layering rule 4), with payloads produced by
  **registered serde fns, not reflection** (E-8; A5 N-6). Reflection must not
  become the wire schema by default; nothing needs to change today to satisfy
  that, and the three unused `bevy_reflect` entries should be resolved
  (question recorded by A5, still open — E-8).
- **The rule that keeps the division stable**: canonical execution never
  consumes a replicated component as an input (A6 R5/R6). Prediction operates
  on mirrors; canonical state is stepped from sealed input logs (A4 S0–S7).
  A predicted mirror diverging is a *visual* event; a canonical divergence is
  a witness event. The two must never share a code path.

### 2.4 Presentation extraction

The contract, assembled from A6 (E-10) and A4/A7 (E-9), all of it existing
doctrine restated at the boundary:

1. Extraction reads **post-S4 quantized state only** (A6 R6) — what a player
   sees is exactly what a claim commits to.
2. Extraction is **copy-out, AOI-scoped, `PersistId`-keyed** (E-5, E-6).
3. Mirror writes have **overwrite semantics** — assignment, not accumulation;
   duplicate frames converge (A6 §3.5).
4. On correction, presentation is **discarded and regenerated** from corrected
   canonical state; presentation performs no undo logic (A6 §3.5).
5. Extraction **cannot reach the witness projection**: `state_hash` is
   computed at S5 inside `orrery_core` from `CoreCodec` bytes (E-9);
   extraction runs after, on a copy, in crates the projection never calls.
   No world-iteration bytes exist to leak (WP-2/WP-5), which is what E-13's
   three-times-reproduced iteration-order finding demands.

### 2.5 Fixed simulation tick versus render tick

- The canonical tick is **fixed 60 Hz**, set as `Time<Fixed>` through
  lightyear's `ClientPlugins { tick_duration }` (E-7). Canonical stages S0–S7
  run inside it (A4 §3.2); time inside a rule is `Tick`, never wall clock
  (VC-8, gate-scanned).
- The render tick is unrelated and unconstrained; it consumes interpolated
  frames behind a 100 ms buffer (`wiring.rs:13`).
- Boundary rule: **nothing render-rate-dependent may write anything
  canonical** — this is A6 R6's cyclic-dependency clause, and it is
  structurally satisfied while canonical state lives outside the app world
  (A6 §3.5 table, last row).
