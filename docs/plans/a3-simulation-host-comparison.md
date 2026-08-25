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
