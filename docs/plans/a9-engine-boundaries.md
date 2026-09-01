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
| E-2 | The backend links zero Bevy; the witness crate links a great deal of it while the gate passes | `cargo tree -p orrery_persistd \| grep -ci bevy` = **0**; same for `orrery_witness` = **530**; `./scripts/core-gates.sh` exits **0** — because `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` (`scripts/core-gates.sh:37`) and the gate's coverage *is* that list. All three re-run for this document. A4's lesson holds: never assume a gate covers what its name suggests |
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
| E-15 | No field host exists; the `SimulationHost` seam is recommendation, not code (as written. The seam half is history: it landed 2026-08-30 as `crates/orrery_sim_host` (#738) — `SimulationHost` at `src/lib.rs:248`, the existing `Ruleset` hosted through an adapter — with the `EcsBackend` substrate admitted behind it on 2026-08-31 (#757; `src/ecs.rs:653`). The field-host half stands: no field host is built on the seam yet — its one dependent crate is `gates/migration-bench`) | A3 G14; `docs/10-crates.md` documents `orrery_field_host` which does not exist — already filed as #414, confirmed still open and still absent from `crates/` today (still absent — the crate that landed is `orrery_sim_host`, not the census's `orrery_field_host`, annotated "planned P6, not built" at `docs/10-crates.md:83`; #414 itself has since closed) |

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
| `core-gates.sh` clause 1: gated crate with Bevy in its graph dies | exactly `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` (`scripts/core-gates.sh:37`); mutation M1 (§6) confirms the death is real | `orrery_witness` (530 bevy lines, gate passes — E-2); **any new crate hosting canonical state** (A3 G9); the list is the coverage |
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

---

## 3. Closing G-1: what would enforce IV-7 at this boundary

A5 named the invariant (IV-7: no capability for a schema embedding an engine
handle) and named the gap (G-1: no mechanical guard watches replicated payload
field types — E-11). Mutation M3 (§6) proves the gap is real at exactly the
seam this document draws: `Entity::to_bits()` rode into a `DiffUplink` payload
and 100 tests plus every gate passed. Proposals, in order of arrival:

1. **Type-level guard at registration (near-term, mechanical).** Replicon
   payloads are produced by registered per-component serde fns (E-8). Require
   the registration path to bound the component type with a sealed marker
   trait — call it `EngineHandleFree` — implemented for primitives, protocol
   types, and std containers of same, derivable structurally, and **not**
   implemented for `Entity`, `ComponentId`, or any `bevy_ecs` type. A
   component embedding a handle then fails at the registration call site, at
   compile time. This is the only guard that can work: **byte-level scanning
   cannot** — entity bits are indistinguishable from any other `u64`, so a
   payload scanner would be theater. The guard must live where the type is
   still a type.
2. **Schema-declaration refusal (the durable fix, A5 §5).** When the
   capability/declaration registry exists, a declared `(ComponentTypeId,
   SchemaVersion)` codec whose schema contains an engine-handle type is
   refused at declaration time — IV-7 as written. The registry's namespace
   governance is **A8's (#404)**, being defined now; this document proposes
   only that IV-7 refusal be a registry acceptance criterion, and does not
   specify manifest content.
3. **Gate-list honesty (cheap, immediate).** Whatever crate ends up hosting
   the registration seam should be added to a *named* scan — not assumed
   covered. E-2 is the standing lesson: `GATED_CRATES` is the coverage.

Decision ownership: adopting 1 touches `orrery_persist_client`/`orrery_spatial`
API; adopting 2 is A8-adjacent registry design; both are proposals for the
owner, not decisions made here.

---

## 4. The Unreal boundary — **[SPEC — no implementation exists]**

**Standing marker for this entire section:** there is zero Unreal code in the
tree (§0). Every sentence below is a specification against an absent
implementation and is unevidenced by construction. Where a statement borrows
evidence, the evidence is about the *Rust side* of the boundary, and is cited;
nothing here says anything evidenced about Unreal itself. The brief's sketch
(`ruleset-ecs-migration-brief.md:481-505`) is the source; this section grooms
it against what actually ships on the Rust side.

### 4.1 Commands into the runtime **[SPEC]**

- Input/interaction commands, view/interest hints, and session requests enter
  as `orrery_protocol` types (postcard-encoded, engine-free — E-14), exactly
  the classes A6 §2 fixed. Commands join the input log at S0 seal order like
  any other host's commands (A4 S0/S1); an Unreal-originated command is
  indistinguishable from a Bevy-originated one past the seal. **This is the
  demonstrable half of "same canonical rules": the rules cannot tell engines
  apart because no engine identity survives S0.**
- No Unreal type crosses inward. UObject identity, Actor references, and
  Blueprint state never appear in a command; the FFI carries fixed-width
  integers and byte buffers only (§4.4).

### 4.2 Presentation frames and events out **[SPEC]**

The outbound contract is §2.4's extraction contract verbatim — the sidecar
emits what the Bevy mirror consumes today, keyed the same way:

- spawn/despawn batches keyed by `PersistId` (u64 — E-14);
- interpolated/predicted transforms from post-S4 quantized state;
- domain events, presentation events, authority changes, persistence
  outcomes, corrections and rollback notices (the brief's list, adopted);
- overwrite semantics throughout; on correction, frames are regenerated and
  the Unreal side performs no undo logic (A6 §3.5 applies unchanged — the
  contract is engine-generic because nothing in it names an engine).

### 4.3 Sidecar and embedded variants **[SPEC]**

- **Sidecar (first): a headless Rust process linking the same crates** the
  Bevy client's canonical path links, plus an IPC adapter. The transport
  precedent exists in-tree: persistd and the coordinator already speak
  "an admission uni-stream, then tagged datagrams" over iroh, and persistd
  serves gRPC (`crates/orrery_persistd/src/journal/chain_grpc.rs`) — the
  sidecar adds no novel transport class. Note honestly: the host the sidecar
  would wrap — the `SimulationHost` seam — **does not exist yet** (E-15;
  as written — overtaken 2026-08-30, when the seam landed as
  `crates/orrery_sim_host` (#738), with the `EcsBackend` substrate admitted
  behind it 2026-08-31, #757). What was two absences deep — no host seam,
  no Unreal consumer — is one: the host seam exists, the Unreal consumer
  does not. Both A3 lanes recommend building the seam first for reasons
  independent of Unreal.
- **Embedded (second): a static/dynamic library owning the same canonical
  store behind a C ABI.** Same crates, same store, no process boundary.
  Deferred behind the sidecar because the sidecar exercises every contract
  the embedding needs while keeping crash and version isolation.
- In both variants the canonical store is the executor's (E-1) — never an
  Unreal-side structure. The sidecar/embedding is an *authority host or an
  observer*; Unreal is a mirror consumer in exactly the sense the Bevy app
  world is one today (§2.1).

### 4.4 Stable FFI types **[SPEC]**

Adopting the brief's list (`ruleset-ecs-migration-brief.md:497-503`), made
concrete against what exists:

- ids: `PersistId`/`Tick` are already `u64` newtypes (E-14) — they cross as
  `uint64_t`;
- opaque runtime handles for sessions/subscriptions; fixed-width integers
  everywhere; explicit byte buffers with declared ownership and length;
  batched commands and frames;
- payload encoding: the declared codecs that already exist (postcard over
  explicit wire structs; `CoreCodec` for canonical bytes — E-8/E-9). No
  reflection-derived schema (A5 N-6).
- versioning: a **runtime ABI version** separate from the **rules/manifest
  version**. The manifest axes and their content are **A8's (#404)** — this
  document cites A8 as owner and specifies nothing about manifest content
  beyond: the FFI must carry both versions and refuse a mismatch it cannot
  prove compatible.
- **No `bevy_ecs` type crosses the C ABI or IPC boundary** — the brief's rule
  (`ruleset-ecs-migration-brief.md:495`), adopted as the Unreal twin of B-1.
  Proposed enforcement (the honest kind, learned from E-2): the adapter
  crate's dependency graph is the guard — it should depend on
  `orrery_protocol` and the host API only, and be added *by name* to the
  Bevy-free scan list, because a gate not naming it does not cover it.

### 4.5 Explicit exclusions **[SPEC]**

**Actor replication, Iris, Mass, Chaos, UObject identity, and Blueprint state
are not canonical truth and may never become it.** They may mirror or consume
canonical state (the brief's rule, adopted verbatim). The reason is the same
one that rejected the shared Bevy world (E-12), applied symmetrically: the
moment canonical truth acquires engine-native replication semantics, every
structural guarantee — witness projection, rollback scope, single-writer
authority — degrades to convention inside a machine Orrery does not control.
Chaos physics results, in particular, are presentation: a Chaos-simulated
transform is a *visual*, and anything canonical about motion comes from the
rules crates or it does not exist.

### 4.6 "Same canonical rules" — what is demonstrated versus asserted

The acceptance item asks for demonstration. What can honestly be demonstrated
today, and what cannot:

- **Demonstrated (Rust side): the same rules crates already execute under
  three host shapes with hash equality** — a Bevy `App` (p1-swarm bot, E-6),
  headless test harnesses, and Bevy-free persistd adjudication — and the
  conformance corpus pins cross-platform chain equality against committed
  goldens. The rules are host-blind because no host identity survives S0
  (§4.1) and no engine type can compile into them (M2, §6).
- **Not demonstrated (Unreal side): that any Unreal build will drive them.**
  "Bevy and Unreal execute the same canonical rules" is reducible to "the
  sidecar links the same crates and replays the committed goldens" — which is
  a *checkable acceptance criterion of the observer proof* (§5), not a
  current fact. Asserting it today would be asserting a property of code
  that does not exist. This document declines to.

---

## 5. The minimal Unreal observer proof — **[SPEC — specified, not implemented]**

The smallest artifact that would turn §4 from specification into evidence.
Scope: an *observer* — it renders canonical state and submits commands; it
holds no authority, predicts nothing, persists nothing. (Brief phase 8,
`ruleset-ecs-migration-brief.md:762-771`, narrowed.)

**Components (three, no more):**

1. **Sidecar binary**: the `SimulationHost` seam (must exist first — E-15;
   it does — landed 2026-08-30, #738)
   hosting `Regolith` via the existing `Ruleset` adapter, plus an IPC adapter
   speaking length-prefixed postcard frames over a local transport (the
   persistd/coordinator session pattern — §4.3). Links no render, no winit,
   no Unreal anything.
2. **IPC schema crate**: depends on `orrery_protocol` only; defines the
   command-in/frame-out messages of §4.1/§4.2 and both version numbers of
   §4.4. Added by name to the Bevy-free scan.
3. **Unreal plugin (C++)**: connects, subscribes an interest volume, maps
   `PersistId → AActor*` in a `TMap`, applies transform frames, spawns and
   despawns on batch messages, submits movement commands. No Actor
   replication, no Iris, no savegame — the map is the entire state.

**Acceptance checks (each falsifiable, each named):**

- **P-1 (same rules)**: the sidecar replays a committed conformance golden
  chain end-to-end while the observer is attached; the replayed chain hash
  equals the committed golden. This is the "demonstrated, not asserted"
  criterion made mechanical: the bytes that reach Unreal are derived from
  the same hashes the golden battery pins. (Caveat inherited from A7 X-A:
  goldens cover state hashes only — an event-only outcome is invisible to
  them, so P-1 proves state equivalence, not event-surface equivalence.)
- **P-2 (no engine type crosses)**: `cargo tree` on the IPC schema crate
  shows zero bevy; a grep for `bevy_ecs` in the plugin's generated bindings
  matches nothing. Named in CI, not assumed.
- **P-3 (correction discipline)**: inject an authority correction mid-replay;
  the observer receives regenerated frames and converges by overwrite —
  verified by asserting the plugin holds no history buffer to un-wind
  (code-shape check) and the final displayed transform equals the corrected
  canonical one.
- **P-4 (observer inertness)**: kill the observer mid-run; the sidecar's
  chain hash is unchanged versus an unobserved run. Presentation has no
  canonical authority (E-10) — now demonstrated across an engine boundary.

**Not in scope**: prediction on the Unreal side, embedded variant, authority
hosting, persistence, any manifest content (A8's).

---

## 6. Mutation log (break the guarded stage → named check dies → revert → passes)

Constraint honored: the P4 pipeline digest hashes `orrery_witness`,
`orrery_core`, `orrery_games` and `p1-swarm`; no mutation touched any of the
four. `orrery_conformance` is gated by core-gates but not digest-hashed, and
`orrery_persist_client` is neither.

| # | Mutation (guarded stage) | Result | Named check | Reverted |
|---|---|---|---|---|
| M1 | Added `bevy_ecs = { workspace = true }` to `crates/orrery_conformance/Cargo.toml` — breaking the Bevy-free property of a gated crate | `./scripts/core-gates.sh` → `core-gates: orrery_conformance has Bevy in its dependency graph`, exit 1 | `core-gates.sh` clause 1 ("Bevy-free") | yes; gate exits 0 again |
| M2 | Added `pub fn leak_handle() -> bevy_ecs::entity::Entity` to `crates/orrery_conformance/src/lib.rs` — an engine handle named inside a rules crate | `cargo check -p orrery_conformance` → **E0433** (`bevy_ecs` unresolvable): the guard is the dependency graph itself; no gate ever runs because the code cannot exist | rustc E0433 (structural), backed by M1's gate against re-adding the dep | yes; clean check |
| M3 | In `crates/orrery_persist_client/src/feed.rs`, appended `entity.to_bits().to_le_bytes()` to the `DiffUplink` payload — an engine handle inside a replicated, journal-bound payload (the exact G-1 corridor) | **Survived by design**: `cargo check` clean; `./scripts/core-gates.sh` exit 0; `cargo test -p orrery_persist_client` → `95 passed`, `2 passed`, `2 passed`, `1 passed`, all `0 failed` (one suite `0 passed; 0 filtered out` — an empty suite, read and noted, not counted as coverage) | **none** — that is the finding. A5's G-1 confirmed live at this boundary; §3 proposes the closure | yes; clean check, tree clean |

M3 is reported against interest deliberately: this document's Bevy-boundary
claim would read stronger without it. It stays because a surviving mutation
is a finding, and because §3's proposal is only justified by it.

---

## 7. Stale citations and discrepancies found while verifying

| # | What was claimed | What the tree says |
|---|---|---|
| D-1 | This task's own brief called the lightyear quote "vendored `lightyear_replication-0.29.0`" | The quote is real and at the cited lines (`src/lib.rs:67-68`), but the crate is **not vendored** — it resolves from the cargo registry. `vendor/` contains `aeronet_iroh`, `aeronet_tokio_runtime`, `bevy_replicon` only. The A3 documents themselves cite it without the "vendored" label; the drift is in the hand-off wording, recorded so it stops propagating |
| D-2 | `docs/10-crates.md` workspace layout lists `orrery_aeronet_iroh` under `crates/` | No such workspace crate exists; the aeronet-iroh code lives at `vendor/aeronet_iroh`. Same document, same class of drift as #414 (`orrery_field_host`, confirmed still open and still absent); #414's text covers the field host only, so this is an additional instance, not a duplicate |
| D-3 | `docs/10-crates.md` says "all thirteen `orrery_*` crates" | `crates/` holds fifteen `orrery_*` members today, and the document's reference table omits `orrery_conformance` entirely while listing two crates that do not exist (D-2, #414). The doc is sketch-grade by its own header, but its crate census is now wrong in both directions |

None of the substantive handed-down claims failed verification: E-1 through
E-15 all held as cited.

---

## 8. Deferred to owners

- **A8 (#404)**: manifest content, version axes, `(ComponentTypeId,
  SchemaVersion)` namespace governance, and the registry that would make §3
  proposal 2 real. Cited as owner throughout; nothing about manifests is
  specified here.
- **Owner**: acceptance of B-1 and the §4.4 no-engine-type-crosses rule as
  ADR content; adoption of the `EngineHandleFree` registration bound (§3.1);
  resolution of the three unused `bevy_reflect` entries (E-8, A5's open
  question); whether the observer proof (§5) becomes an implementation issue
  after the #395 ADRs land.
- **A4's owner artifacts**: the Tier-H gate bundle was conditional on A3's
  triggers when this document was written, and nothing here armed it. Since
  then it landed (#771) — battery at `scripts/core-gates.sh` section 6,
  enforced mutation-style, D43 (e)(1) — and arms per declared host, with the
  host admitted by owner sanction (#757) rather than a fired trigger.

## 9. Unsure

- Whether lightyear's `ClientPlugins` remaining the *setter* of `Time<Fixed>`
  (E-7) is acceptable long-term for a sidecar that must run the same 60 Hz
  tick without lightyear present. The sidecar spec assumes the host seam owns
  tick cadence; today the client's cadence is lightyear-owned. This is a real
  asymmetry between §2.5 and §4.3 and I could not resolve it from the tree.
- Whether the empty test suite observed under M3 (`0 passed; 0 filtered out`)
  is a feature-gated integration file or genuinely empty; it was not counted
  as evidence either way.
- The `EngineHandleFree` sealed-trait design (§3.1) is sketched from replicon's
  registration shape as it exists in `vendor/bevy_replicon`; whether replicon's
  registration API actually admits the extra bound without forking the vendored
  copy was not prototyped.
